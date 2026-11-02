//! Two-leg plan execution state machine.
//!
//! A [`TwoLegPlan`] tracks two legs meant to offset each other one to one
//! (for example the buy and sell legs of an [`Opportunity`]) as execution
//! events are observed. It never assumes that both legs fill together:
//!
//! - every fill, rejection and cancel acknowledgement is applied as observed,
//!   and the residual exposure (the signed sum of both legs' executed
//!   positions) is reported after every event;
//! - when a leg fails (rejected, or cancelled with quantity left), the
//!   configured [`FailurePolicy`] decides the recovery intent: resubmit the
//!   failed leg, abort (cancel the other leg and keep the residual), or
//!   unwind (cancel the other leg, then offset the residual);
//! - a leg's effective deadline is the earlier of its own deadline and its
//!   instrument's expiry; reaching it asks for a cancel, and the leg keeps
//!   working until the cancel is acknowledged;
//! - events are idempotent: a fill is identified by its execution id, and a
//!   rejection or acknowledgement by its leg and attempt, so a repeated event
//!   changes nothing.
//!
//! The plan only emits intents ([`PlanAction`]); sending orders is the
//! caller's job.
//!
//! ## Cumulative quantities and late fills
//!
//! Every order attempt is tracked separately. As in FIX execution reports,
//! a fill carries the attempt's cumulative executed quantity (`cum_qty`) and
//! a cancel acknowledgement carries the attempt's final one:
//!
//! - a fill is accepted when its `cum_qty` equals the attempt's previous
//!   cumulative plus the fill, whatever its timestamp: a fill executed
//!   before a cancel but delivered after its acknowledgement is applied. A
//!   `cum_qty` that breaks that sequence is an inconsistent report and is
//!   rejected;
//! - the **executed** quantity of an attempt is the larger of its observed
//!   fills and the acknowledged final quantity, so fills still in flight
//!   count as soon as the acknowledgement reports them; the residual
//!   exposure and every resubmission use it;
//! - a real execution is never rejected for exceeding a quantity. A fill or
//!   acknowledgement beyond the attempt or the leg is recorded as exposure,
//!   and a live attempt made surplus by it is cancelled. The other leg keeps
//!   working to its own quantity; once both legs are done, the policy's
//!   fallback applies to the excess (the overfill) alone: abort keeps it
//!   visible, unwind offsets it;
//! - the plan completes only when both legs are executed in full and no
//!   attempt is still live;
//! - fees and notional come from fills only, so they grow as late fills
//!   arrive.
//!
//! Adapter contract: the fills of one attempt must be applied in the
//! venue's execution order (each `cum_qty` builds on the previous one);
//! fills of different attempts or legs may interleave in any order.
//!
//! ## Venues, clocks and recovery
//!
//! Each leg trades on its own venue, with its own clock and delay:
//!
//! - non-fill events of a leg (rejections, acknowledgements, connection
//!   changes and venue views) must not be older than that leg's previous
//!   non-fill event, but may be older than the other leg's events, so one
//!   venue's delay never blocks or rejects the other's; ticks follow the
//!   plan clock, the latest time seen;
//! - [`LegExecution::Disconnected`] and [`LegExecution::Reconnected`] track
//!   whether the leg's venue is reachable, and [`LegExecution::VenueUpdate`]
//!   its latest view;
//! - a resubmission is never sent to a disconnected venue and, with
//!   [`TwoLegPlan::with_venue_gating`], only when the venue's latest view is
//!   fresh, quoted within the limit and deep enough. Otherwise it is held
//!   ([`LegState::pending_resubmit`]) and sent when an update admits it; if
//!   the leg's deadline passes first, the policy's fallback applies;
//! - [`TwoLegPlan::residual`] and [`TwoLegPlan::net_cash`] keep what an
//!   asymmetric or interrupted execution leaves behind.
//!
//! Admission of both legs against per-venue budgets, before the plan
//! starts, is in [`crate::venues`]. With [`TwoLegPlan::with_budget_gating`]
//! the plan takes over the admission's reservations and readmits every
//! resubmission against its venue's current budget before sending it, at a
//! price bound that never goes down, so no resubmission goes out with a
//! reservation computed at an earlier, lower price. [`PlanAction::Unwind`]
//! intents are not admitted: funding an unwind is the caller's job.
//!
//! ## Units
//!
//! The two legs must have the same quantity and offset one to one in its
//! units (for example the same underlying on two venues). Other hedge ratios
//! must be converted by the caller.

use crate::cost::Cost;
use crate::error::{ArbitrageError, Result};
use crate::exact;
use crate::instrument::{Action, Dir, Instrument, Leg, Timestamp};
use crate::money::{Currency, Money};
use crate::opportunity::Opportunity;
use crate::units::{Price, Quantity};
use crate::venues::{
    AdmissionReject, AdmittedLeg, LegFunding, LegRequest, Reservation, VenueBudget, VenueId,
    VenueView, check_view, price_bound, requirement, requirement_at,
};
use crate::wide;
use rust_decimal::Decimal;
use std::collections::BTreeMap;

/// Which of the two legs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LegIndex {
    /// The first leg.
    First,
    /// The second leg.
    Second,
}

impl LegIndex {
    /// The other leg.
    #[must_use]
    pub fn other(self) -> Self {
        match self {
            Self::First => Self::Second,
            Self::Second => Self::First,
        }
    }

    /// This leg's element of a pair.
    pub(crate) fn of<T>(self, pair: &[T; 2]) -> &T {
        let [first, second] = pair;
        match self {
            Self::First => first,
            Self::Second => second,
        }
    }

    /// This leg's element of a pair, mutably.
    pub(crate) fn of_mut<T>(self, pair: &mut [T; 2]) -> &mut T {
        let [first, second] = pair;
        match self {
            Self::First => first,
            Self::Second => second,
        }
    }
}

/// A leg to execute and its optional deadline.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LegSpec {
    /// Instrument, side, quantity and limit price.
    pub leg: Leg,
    /// When to stop working the leg; reaching it requests a cancel. The
    /// instrument's expiry, if earlier, applies instead.
    pub deadline: Option<Timestamp>,
    /// The venue the leg trades on; only a view and a budget of this venue
    /// apply to the leg ([`crate::venues`]).
    pub venue: VenueId,
}

impl LegSpec {
    /// The earlier of `deadline` and the instrument's expiry.
    #[must_use]
    pub fn effective_deadline(&self) -> Option<Timestamp> {
        match (self.deadline, self.leg.instrument.expiry) {
            (Some(d), Some(e)) => Some(d.min(e)),
            (d, e) => d.or(e),
        }
    }
}

/// What to do after a recovery by resubmission is exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fallback {
    /// Cancel the other leg and keep the residual exposure.
    Abort,
    /// Cancel the other leg, then request an offsetting order for the
    /// residual exposure.
    Unwind,
}

/// How the plan reacts when a leg fails (rejected, or cancelled with
/// quantity left).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailurePolicy {
    /// Cancel the other leg and keep the residual exposure.
    Abort,
    /// Cancel the other leg, then request an offsetting order for the
    /// residual exposure.
    Unwind,
    /// Resubmit the failed leg's remaining quantity, at most `max_resubmits`
    /// times over the plan's life and never past the leg's effective
    /// deadline, then apply `then`.
    Complete {
        /// Resubmissions allowed over the plan's life.
        max_resubmits: u32,
        /// What to do once they are used up.
        then: Fallback,
    },
}

/// An execution id as reported by the venue.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExecId(pub String);

/// An observed execution event for one leg, or the passage of time.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LegExecution {
    /// A (partial) fill of an attempt.
    Fill {
        /// The leg.
        leg: LegIndex,
        /// The order attempt it belongs to (0 for the first submission).
        attempt: u32,
        /// Venue execution id; a repeat with the same content is ignored.
        exec_id: ExecId,
        /// Filled quantity.
        quantity: Quantity,
        /// The attempt's cumulative executed quantity including this fill.
        cum_qty: Quantity,
        /// Fill price.
        price: Price,
        /// Fee charged (negative for a rebate), in the plan currency.
        fee: Cost,
        /// When it was executed or observed; it may be older than the last
        /// event if `cum_qty` is consistent.
        at: Timestamp,
    },
    /// The attempt was rejected without filling.
    Rejected {
        /// The leg.
        leg: LegIndex,
        /// The rejected attempt.
        attempt: u32,
        /// When it was observed.
        at: Timestamp,
    },
    /// The attempt's open quantity is cancelled (requested or not).
    CancelAck {
        /// The leg.
        leg: LegIndex,
        /// The cancelled attempt.
        attempt: u32,
        /// The attempt's final cumulative executed quantity (zero or more).
        cum_qty: Decimal,
        /// When it was observed.
        at: Timestamp,
    },
    /// Time has advanced to `at` with no execution event; checks deadlines.
    Tick {
        /// The current time.
        at: Timestamp,
    },
    /// The leg's venue is unreachable: no order can be sent to it.
    Disconnected {
        /// The leg.
        leg: LegIndex,
        /// When it was observed.
        at: Timestamp,
    },
    /// The leg's venue is reachable again.
    Reconnected {
        /// The leg.
        leg: LegIndex,
        /// When it was observed.
        at: Timestamp,
    },
    /// A new view of the leg's instrument on its venue, stamped with its
    /// `observed_at`.
    VenueUpdate {
        /// The leg.
        leg: LegIndex,
        /// The view.
        view: VenueView,
    },
}

impl LegExecution {
    /// The leg the event belongs to, if any.
    fn leg(&self) -> Option<LegIndex> {
        match self {
            Self::Fill { leg, .. }
            | Self::Rejected { leg, .. }
            | Self::CancelAck { leg, .. }
            | Self::Disconnected { leg, .. }
            | Self::Reconnected { leg, .. }
            | Self::VenueUpdate { leg, .. } => Some(*leg),
            Self::Tick { .. } => None,
        }
    }

    fn at(&self) -> Timestamp {
        match self {
            Self::Fill { at, .. }
            | Self::Rejected { at, .. }
            | Self::CancelAck { at, .. }
            | Self::Tick { at }
            | Self::Disconnected { at, .. }
            | Self::Reconnected { at, .. } => *at,
            Self::VenueUpdate { view, .. } => view.observed_at,
        }
    }
}

/// An intent the plan asks the caller to carry out.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PlanAction {
    /// Cancel the open quantity of a leg's attempt.
    Cancel {
        /// The leg.
        leg: LegIndex,
        /// The attempt to cancel.
        attempt: u32,
    },
    /// Send the leg again for its remaining quantity as a new attempt.
    Resubmit {
        /// The leg.
        leg: LegIndex,
        /// The new attempt number.
        attempt: u32,
        /// Quantity still to execute.
        quantity: Quantity,
        /// Limit price (the plan's price for the leg).
        price: Price,
    },
    /// Send an order that offsets residual exposure. It is an intent: the
    /// residual stays visible until fills are observed elsewhere.
    Unwind {
        /// Instrument to trade.
        instrument: Instrument,
        /// The action that reduces the residual.
        side: Action,
        /// Quantity to offset.
        quantity: Quantity,
    },
}

/// How an attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttemptOutcome {
    /// Rejected without fills.
    Rejected,
    /// Cancelled with a final cumulative quantity.
    Cancelled,
}

/// One order attempt of a leg.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AttemptState {
    /// Quantity the attempt was sent for.
    pub quantity: Quantity,
    /// Cumulative quantity of the fills applied so far.
    pub observed: Decimal,
    /// Final cumulative quantity from the rejection (zero) or cancel
    /// acknowledgement.
    pub final_cum: Option<Decimal>,
    /// How it ended, if it has.
    pub outcome: Option<AttemptOutcome>,
    /// True once a cancel was requested for it.
    pub cancel_requested: bool,
}

impl AttemptState {
    pub(crate) fn new(quantity: Quantity) -> Self {
        Self {
            quantity,
            observed: Decimal::ZERO,
            final_cum: None,
            outcome: None,
            cancel_requested: false,
        }
    }

    /// Executed quantity: the larger of the observed fills and the
    /// acknowledged final cumulative.
    #[must_use]
    pub fn executed(&self) -> Decimal {
        self.final_cum
            .map_or(self.observed, |f| f.max(self.observed))
    }

    /// True while the attempt can still execute: not closed and not filled
    /// to its quantity.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.outcome.is_none() && self.observed < self.quantity.value()
    }
}

/// Where a leg stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LegStatus {
    /// An attempt is working.
    Working {
        /// Current attempt.
        attempt: u32,
    },
    /// A cancel was requested and not yet acknowledged.
    CancelRequested {
        /// Attempt being cancelled.
        attempt: u32,
    },
    /// The whole quantity (or more) is executed and no attempt is live.
    Filled,
    /// The last attempt was rejected with quantity left.
    Rejected,
    /// The last attempt was cancelled with quantity left.
    Cancelled,
}

impl LegStatus {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Filled | Self::Rejected | Self::Cancelled)
    }
}

/// Where the plan stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanState {
    /// Both legs are being worked (possibly after resubmissions).
    Working,
    /// Both legs are executed in full.
    Completed,
    /// A leg failed; the other leg is being cancelled before aborting.
    Aborting,
    /// A leg failed; the other leg is being cancelled before unwinding.
    Unwinding,
    /// Both legs are done and the residual is kept.
    Aborted,
    /// Both legs are done under the unwind policy: every residual is offset
    /// by a [`PlanAction::Unwind`] intent (none when the legs ended flat).
    UnwindRequested,
}

/// Running totals of one leg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegState {
    /// The leg as planned.
    pub spec: LegSpec,
    /// Its attempts; the index is the attempt number.
    pub attempts: Vec<AttemptState>,
    /// Sum of `price * quantity` over applied fills.
    pub filled_notional: Money,
    /// Sum of fees over applied fills.
    pub fees: Cost,
    /// False while the leg's venue is disconnected.
    pub connected: bool,
    /// The latest view of the leg's instrument on its venue.
    pub view: Option<VenueView>,
    /// True while a resubmission is due but held back by the venue (see
    /// [`TwoLegPlan::with_venue_gating`]) or by its budget (see
    /// [`TwoLegPlan::with_budget_gating`]).
    pub pending_resubmit: bool,
    /// With budget gating: the capital currently reserved for the whole leg
    /// on its venue, `None` once released
    /// ([`TwoLegPlan::release_reservations`]). It is the only reservation
    /// the caller may release for the leg.
    pub reservation: Option<Reservation>,
    /// With budget gating: the price the current reservation values the
    /// leg's notional at.
    pub covered_bound: Option<Price>,
    /// With budget gating: the highest fill price above
    /// [`LegState::covered_bound`], if any. The fill is recorded as executed;
    /// the reservation does not cover it yet, so the next resubmission must
    /// first be readmitted at this price or higher.
    pub deficit: Option<Price>,
    /// With budget gating: why the last readmission held the resubmission
    /// back, if it did.
    pub hold: Option<AdmissionReject>,
    deadline_cancelled: bool,
    last_at: Timestamp,
}

/// A leg without attempts: never built by this crate.
const NO_ATTEMPT: ArbitrageError = ArbitrageError::InvalidInput {
    reason: "a leg has no attempt",
};

impl LegState {
    /// Executed quantity over all attempts (see [`AttemptState::executed`]).
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn executed(&self) -> Result<Decimal> {
        let values = self
            .attempts
            .iter()
            .map(AttemptState::executed)
            .collect::<Vec<_>>();
        wide::sum(&values, "executed")
    }

    /// Quantity left to execute; negative after an overfill.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn remaining(&self) -> Result<Decimal> {
        let mut values = vec![self.spec.leg.quantity.value()];
        values.extend(self.attempts.iter().map(|a| exact::neg(a.executed())));
        wide::sum(&values, "remaining")
    }

    /// The signed executions of each attempt: positive for a buy leg.
    fn signed_attempts(&self) -> Result<impl Iterator<Item = Decimal> + '_> {
        let dir = Dir::try_from(self.spec.leg.side)?;
        Ok(self.attempts.iter().map(move |a| match dir {
            Dir::Buy => a.executed(),
            Dir::Sell => exact::neg(a.executed()),
        }))
    }

    /// Quantity executed beyond the leg's quantity (zero if none).
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn overfill(&self) -> Result<Decimal> {
        Ok(exact::neg(self.remaining()?).max(Decimal::ZERO))
    }

    /// Signed executed position: positive for a buy leg, negative for a
    /// sell.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn position(&self) -> Result<Decimal> {
        let executed = self.executed()?;
        Ok(match Dir::try_from(self.spec.leg.side)? {
            Dir::Buy => executed,
            Dir::Sell => exact::neg(executed),
        })
    }

    /// Resubmissions so far.
    #[must_use]
    pub fn resubmits(&self) -> u32 {
        u32::try_from(self.attempts.len().saturating_sub(1)).unwrap_or(u32::MAX)
    }

    /// The leg's status.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn status(&self) -> Result<LegStatus> {
        let attempt = self.resubmits();
        let current = self.current()?;
        if current.is_live() {
            return Ok(if current.cancel_requested {
                LegStatus::CancelRequested { attempt }
            } else {
                LegStatus::Working { attempt }
            });
        }
        if self.remaining()? <= Decimal::ZERO {
            return Ok(LegStatus::Filled);
        }
        Ok(match current.outcome {
            Some(AttemptOutcome::Rejected) => LegStatus::Rejected,
            // Cancelled, or filled to its own quantity while the leg still
            // needs more (cannot happen with resubmissions sized to the
            // remaining quantity, reported as cancelled to stay terminal).
            _ => LegStatus::Cancelled,
        })
    }

    /// The latest attempt. `attempts` is never empty (attempt 0 is created
    /// with the leg), so the error is unreachable.
    fn current(&self) -> Result<&AttemptState> {
        self.attempts.last().ok_or(NO_ATTEMPT)
    }

    fn current_mut(&mut self) -> Result<&mut AttemptState> {
        self.attempts.last_mut().ok_or(NO_ATTEMPT)
    }
}

/// What [`TwoLegPlan::apply`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanActions {
    /// Intents to carry out, in order.
    pub actions: Vec<PlanAction>,
    /// The plan state after the event.
    pub state: PlanState,
    /// Residual exposure after the event: the sum of both legs' signed
    /// executed positions (zero when the legs offset each other).
    pub residual: Decimal,
    /// True if the event was a repeat and changed nothing.
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FillRecord {
    leg: LegIndex,
    attempt: u32,
    quantity: Quantity,
    cum_qty: Quantity,
    price: Price,
    fee: Cost,
}

/// A two-leg plan driven by observed execution events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwoLegPlan {
    currency: Currency,
    policy: FailurePolicy,
    legs: [LegState; 2],
    state: PlanState,
    clock: Timestamp,
    max_staleness_ms: Option<i64>,
    funding: Option<[LegFunding; 2]>,
    fills: BTreeMap<ExecId, FillRecord>,
    unwind_requested: Decimal,
}

/// Whether a leg's venue can take a resubmission now.
enum VenueGate {
    Open,
    Disconnected,
    Rejected(AdmissionReject),
}

impl TwoLegPlan {
    /// A plan for two offsetting legs, created at `created_at`. Both legs
    /// start working as attempt 0.
    ///
    /// # Errors
    ///
    /// - [`ArbitrageError::UnsupportedAction`] if a leg's side is not buy or
    ///   sell (checked first).
    /// - [`ArbitrageError::InvalidInput`] if the legs are on the same
    ///   instrument, on the same side, have different quantities, or have a
    ///   deadline at or before `created_at`.
    /// - [`ArbitrageError::Expired`] if a leg's instrument has expired at
    ///   `created_at`.
    pub fn new(
        first: LegSpec,
        second: LegSpec,
        policy: FailurePolicy,
        currency: Currency,
        created_at: Timestamp,
    ) -> Result<Self> {
        let (first_dir, second_dir) = (
            Dir::try_from(first.leg.side)?,
            Dir::try_from(second.leg.side)?,
        );
        if first.leg.instrument.id == second.leg.instrument.id {
            return Err(ArbitrageError::InvalidInput {
                reason: "the two legs are the same instrument",
            });
        }
        if first_dir == second_dir {
            return Err(ArbitrageError::InvalidInput {
                reason: "the two legs must be on opposite sides to offset",
            });
        }
        if first.leg.quantity != second.leg.quantity {
            return Err(ArbitrageError::InvalidInput {
                reason: "the two legs must have the same quantity to offset one to one",
            });
        }
        for spec in [&first, &second] {
            spec.leg.instrument.ensure_live(created_at)?;
            if spec.deadline.is_some_and(|d| d <= created_at) {
                return Err(ArbitrageError::InvalidInput {
                    reason: "a leg deadline is not after the plan creation",
                });
            }
        }
        let leg_state = |spec: LegSpec| LegState {
            attempts: vec![AttemptState::new(spec.leg.quantity)],
            spec,
            filled_notional: Money::zero(currency),
            fees: Cost::new(Money::zero(currency)),
            connected: true,
            view: None,
            pending_resubmit: false,
            reservation: None,
            covered_bound: None,
            deficit: None,
            hold: None,
            deadline_cancelled: false,
            last_at: created_at,
        };
        Ok(Self {
            currency,
            policy,
            legs: [leg_state(first), leg_state(second)],
            state: PlanState::Working,
            clock: created_at,
            max_staleness_ms: None,
            funding: None,
            fills: BTreeMap::new(),
            unwind_requested: Decimal::ZERO,
        })
    }

    /// A plan for an accepted [`Opportunity`]: its buy leg first, on
    /// `buy_venue`, and its sell leg second, on `sell_venue`, both with
    /// `deadline`.
    ///
    /// # Errors
    ///
    /// As [`TwoLegPlan::new`].
    pub fn from_opportunity(
        opportunity: &Opportunity,
        buy_venue: VenueId,
        sell_venue: VenueId,
        deadline: Option<Timestamp>,
        policy: FailurePolicy,
        currency: Currency,
    ) -> Result<Self> {
        Self::new(
            LegSpec {
                leg: opportunity.buy.clone(),
                deadline,
                venue: buy_venue,
            },
            LegSpec {
                leg: opportunity.sell.clone(),
                deadline,
                venue: sell_venue,
            },
            policy,
            currency,
            opportunity.as_of,
        )
    }

    /// The plan state.
    #[must_use]
    pub fn state(&self) -> PlanState {
        self.state
    }

    /// One leg's running totals.
    #[must_use]
    pub fn leg(&self, leg: LegIndex) -> &LegState {
        leg.of(&self.legs)
    }

    /// Residual exposure: the sum of both legs' signed executed positions.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn residual(&self) -> Result<Decimal> {
        let values = self.legs[0]
            .signed_attempts()?
            .chain(self.legs[1].signed_attempts()?)
            .collect::<Vec<_>>();
        wide::sum(&values, "residual")
    }

    /// Fees of both legs.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn fees(&self) -> Result<Cost> {
        self.legs[0].fees.checked_add(self.legs[1].fees)
    }

    /// Holds resubmissions back until the leg's venue admits them: the venue
    /// is connected and its latest view is at most `max_staleness_ms` old,
    /// quotes the side the leg trades within its limit, and has size for the
    /// remaining quantity. A held resubmission is sent when a later
    /// [`LegExecution::VenueUpdate`] or [`LegExecution::Reconnected`] admits
    /// it, and is dropped for the policy's fallback once the leg's deadline
    /// passes. Without gating, only a disconnected venue holds it back.
    #[must_use]
    pub fn with_venue_gating(mut self, max_staleness_ms: i64) -> Self {
        self.max_staleness_ms = Some(max_staleness_ms);
        self
    }

    /// Readmits every resubmission against its venue's budget before it is
    /// sent, on top of venue gating with `max_staleness_ms`.
    ///
    /// `legs` are the plan's two legs as admitted by
    /// [`admit_two_legs`](crate::venues::admit_two_legs): their fee and
    /// funding, the view each was admitted on, and the reservation it made.
    /// Those reservations are already held in the budgets; the plan takes
    /// them over without reserving them again. From then on:
    ///
    /// - each leg holds one current reservation ([`LegState::reservation`])
    ///   covering its whole quantity, valued at a price bound
    ///   ([`LegState::covered_bound`]) that never goes down: the admission's
    ///   bound (the limit for a buy; the higher of the limit and the bid for
    ///   a sell), then the higher of that, any fill price above it and, for
    ///   a sell, the bid seen at each readmission;
    /// - before a resubmission is sent, the leg is readmitted on a copy of
    ///   its venue's budget: its current reservation is released and the
    ///   requirement at the new bound reserved, all or nothing. If it does
    ///   not fit, the resubmission is held ([`LegState::hold`]) and retried
    ///   on each later non-fill event: after changing a budget outside the
    ///   plan, send a [`LegExecution::Tick`]. Past the leg's deadline the
    ///   policy's fallback applies, once;
    /// - a fill is always recorded. A fill price above the covered bound
    ///   leaves a [`LegState::deficit`] that the next readmission must cover;
    ///   fill events never readmit, so no budget shortfall can reject or
    ///   lose an execution;
    /// - capital is never borrowed across venues: a leg only ever reserves
    ///   on its own venue's budget.
    ///
    /// The plan is then driven with [`TwoLegPlan::apply_with_budgets`]; when
    /// it ends (completed, aborted, unwound, or abandoned after an error),
    /// the caller releases the current reservations with
    /// [`TwoLegPlan::release_reservations`], never the ones the admission
    /// returned. [`PlanAction::Unwind`] intents are not admitted or reserved:
    /// funding an unwind is the caller's responsibility.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidInput`] if the plan has already seen an
    /// event, a view is of another venue or instrument than its leg, or a
    /// reservation is of another venue or is not exactly the leg's
    /// requirement on its view, fee and funding; the errors of
    /// [`requirement`].
    pub fn with_budget_gating(
        mut self,
        max_staleness_ms: i64,
        legs: [AdmittedLeg; 2],
    ) -> Result<Self> {
        let fresh = self.fills.is_empty()
            && self
                .legs
                .iter()
                .all(|l| l.attempts.len() == 1 && l.attempts.iter().all(|a| a.outcome.is_none()));
        if !fresh {
            return Err(ArbitrageError::InvalidInput {
                reason: "budget gating must be set before the plan sees an event",
            });
        }
        let [first, second] = legs;
        let funding = [first.funding, second.funding];
        for (state, admitted) in self.legs.iter_mut().zip([first, second]) {
            let spec = &state.spec;
            if admitted.view.venue != spec.venue || admitted.reservation.venue != spec.venue {
                return Err(ArbitrageError::InvalidInput {
                    reason: "admitted view or reservation is of another venue than the leg's",
                });
            }
            if admitted.view.instrument.id != spec.leg.instrument.id {
                return Err(ArbitrageError::InvalidInput {
                    reason: "admitted view is for another instrument than the leg",
                });
            }
            let request = LegRequest {
                spec,
                view: &admitted.view,
                fee: admitted.funding.fee,
                sell_funding: admitted.funding.sell_funding,
            };
            let expected = requirement(&request, admitted.reservation.cash.currency())?;
            if expected != admitted.reservation {
                return Err(ArbitrageError::InvalidInput {
                    reason: "reservation is not the leg's requirement on its admitted view",
                });
            }
            state.covered_bound = Some(price_bound(spec, &admitted.view)?);
            state.reservation = Some(admitted.reservation);
        }
        self.max_staleness_ms = Some(max_staleness_ms);
        self.funding = Some(funding);
        Ok(self)
    }

    /// Releases the legs' current reservations from `budgets`, all or
    /// nothing, for a plan with budget gating. Call it once the plan has
    /// ended, or is abandoned. It is idempotent: a released reservation is
    /// not released again, and the plan sends no resubmission afterwards
    /// (it is held with [`AdmissionReject::Released`]).
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidInput`] for a plan without budget gating or
    /// when a reservation's venue has no budget; the errors of
    /// [`VenueBudget::release`]. Neither the plan nor `budgets` change on
    /// error.
    pub fn release_reservations(
        &mut self,
        budgets: &mut BTreeMap<VenueId, VenueBudget>,
    ) -> Result<()> {
        if self.funding.is_none() {
            return Err(ArbitrageError::InvalidInput {
                reason: "plan has no budget gating, so no reservation to release",
            });
        }
        let mut next = budgets.clone();
        for state in &self.legs {
            if let Some(reservation) = &state.reservation {
                next.get_mut(&reservation.venue)
                    .ok_or(ArbitrageError::InvalidInput {
                        reason: "no budget for the venue of a reservation",
                    })?
                    .release(reservation)?;
            }
        }
        for state in &mut self.legs {
            state.reservation = None;
        }
        *budgets = next;
        Ok(())
    }

    /// Cash flow of the fills so far: sell notional minus buy notional minus
    /// fees. With the residual it is what an asymmetric or interrupted
    /// execution leaves behind.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn net_cash(&self) -> Result<Money> {
        let mut values = Vec::with_capacity(4);
        for leg in &self.legs {
            let notional = leg.filled_notional.amount();
            values.push(match Dir::try_from(leg.spec.leg.side)? {
                Dir::Sell => notional,
                Dir::Buy => exact::neg(notional),
            });
            values.push(exact::neg(leg.fees.money().amount()));
        }
        Ok(Money::new(wide::sum(&values, "net cash")?, self.currency))
    }

    /// Applies one observed event and returns the resulting intents.
    ///
    /// The plan is unchanged when an error is returned.
    ///
    /// # Errors
    ///
    /// - [`ArbitrageError::InvalidInput`] for an attempt that was never sent,
    ///   an inconsistent `cum_qty` (a fill's that is not the previous
    ///   cumulative plus the fill, or an acknowledgement's below the fills
    ///   already applied), a reused execution id with different content, a
    ///   rejection of an attempt with fills, a fill on a rejected attempt, an
    ///   attempt closed twice differently, a non-fill event older than the
    ///   last non-fill event of its leg, a tick older than the plan clock, or
    ///   a venue view of another instrument or of a venue that is not the
    ///   leg's. Executions beyond a quantity are not errors.
    /// - [`ArbitrageError::CurrencyMismatch`] for a fee in another currency.
    /// - Arithmetic errors.
    /// - [`ArbitrageError::InvalidInput`] for a plan with budget gating,
    ///   which is driven with [`TwoLegPlan::apply_with_budgets`].
    pub fn apply(&mut self, event: &LegExecution) -> Result<PlanActions> {
        if self.funding.is_some() {
            return Err(ArbitrageError::InvalidInput {
                reason: "a plan with budget gating is driven with apply_with_budgets",
            });
        }
        self.apply_inner(event, None)
    }

    /// Applies one observed event to a plan with budget gating
    /// ([`TwoLegPlan::with_budget_gating`]), readmitting any resubmission it
    /// leads to against `budgets`. The plan and `budgets` change together or
    /// not at all.
    ///
    /// # Errors
    ///
    /// The errors of [`TwoLegPlan::apply`], except that the plan must have
    /// budget gating ([`ArbitrageError::InvalidInput`] otherwise), and the
    /// errors of [`VenueBudget::release`] and of the requirement during a
    /// readmission. A budget that does not fit is not an error: the
    /// resubmission is held ([`LegState::hold`]).
    pub fn apply_with_budgets(
        &mut self,
        event: &LegExecution,
        budgets: &mut BTreeMap<VenueId, VenueBudget>,
    ) -> Result<PlanActions> {
        if self.funding.is_none() {
            return Err(ArbitrageError::InvalidInput {
                reason: "apply_with_budgets needs a plan with budget gating",
            });
        }
        let mut next = budgets.clone();
        let report = self.apply_inner(event, Some(&mut next))?;
        *budgets = next;
        Ok(report)
    }

    fn apply_inner(
        &mut self,
        event: &LegExecution,
        budgets: Option<&mut BTreeMap<VenueId, VenueBudget>>,
    ) -> Result<PlanActions> {
        if self.is_duplicate(event)? {
            return self.report(Vec::new(), true);
        }
        // Each venue keeps its own order: a non-fill event may not be older
        // than the last one of its leg, but may be older than the other
        // leg's events. Fills are ordered by cum_qty instead. Ticks follow
        // the plan clock, the latest time seen.
        let at = event.at();
        match (event, event.leg()) {
            (LegExecution::Fill { .. }, _) => {}
            (_, Some(leg)) => {
                if at < leg.of(&self.legs).last_at {
                    return Err(ArbitrageError::InvalidInput {
                        reason: "event is older than the last event of its leg",
                    });
                }
            }
            (_, None) => {
                if at < self.clock {
                    return Err(ArbitrageError::InvalidInput {
                        reason: "tick is older than the plan clock",
                    });
                }
            }
        }
        let now = at.max(self.clock);
        // Work on a copy and commit only if the whole step and its report
        // succeed, so that an error leaves the plan unchanged.
        let mut next = self.clone();
        let mut actions = Vec::new();
        next.clock = now;
        if let (false, Some(leg)) = (matches!(event, LegExecution::Fill { .. }), event.leg()) {
            leg.of_mut(&mut next.legs).last_at = at;
        }
        match event {
            LegExecution::Fill {
                leg,
                attempt,
                exec_id,
                quantity,
                cum_qty,
                price,
                fee,
                ..
            } => next.apply_fill(
                exec_id,
                FillRecord {
                    leg: *leg,
                    attempt: *attempt,
                    quantity: *quantity,
                    cum_qty: *cum_qty,
                    price: *price,
                    fee: *fee,
                },
            )?,
            LegExecution::Rejected { leg, attempt, .. } => {
                next.close_attempt(*leg, *attempt, AttemptOutcome::Rejected, Decimal::ZERO)?;
            }
            LegExecution::CancelAck {
                leg,
                attempt,
                cum_qty,
                ..
            } => {
                next.close_attempt(*leg, *attempt, AttemptOutcome::Cancelled, *cum_qty)?;
            }
            LegExecution::Tick { .. } => {}
            LegExecution::Disconnected { leg, .. } => leg.of_mut(&mut next.legs).connected = false,
            LegExecution::Reconnected { leg, .. } => leg.of_mut(&mut next.legs).connected = true,
            LegExecution::VenueUpdate { leg, view } => {
                let spec = &leg.of(&next.legs).spec;
                if view.instrument.id != spec.leg.instrument.id {
                    return Err(ArbitrageError::InvalidInput {
                        reason: "venue view is for another instrument than the leg",
                    });
                }
                if spec.venue != view.venue {
                    return Err(ArbitrageError::InvalidInput {
                        reason: "venue view is from another venue than the leg's",
                    });
                }
                leg.of_mut(&mut next.legs).view = Some(view.clone());
            }
        }
        let is_fill = matches!(event, LegExecution::Fill { .. });
        next.step(now, &mut actions, budgets, is_fill)?;
        let report = next.report(actions, false)?;
        *self = next;
        Ok(report)
    }

    fn report(&self, actions: Vec<PlanAction>, duplicate: bool) -> Result<PlanActions> {
        Ok(PlanActions {
            actions,
            state: self.state,
            residual: self.residual()?,
            duplicate,
        })
    }

    fn closed_as(&self, leg: LegIndex, attempt: u32) -> Option<(AttemptOutcome, Option<Decimal>)> {
        leg.of(&self.legs)
            .attempts
            .get(attempt as usize)
            .and_then(|a| a.outcome.map(|o| (o, a.final_cum)))
    }

    fn is_duplicate(&self, event: &LegExecution) -> Result<bool> {
        match event {
            LegExecution::Fill {
                leg,
                attempt,
                exec_id,
                quantity,
                cum_qty,
                price,
                fee,
                ..
            } => match self.fills.get(exec_id) {
                None => Ok(false),
                Some(seen) => {
                    let same = FillRecord {
                        leg: *leg,
                        attempt: *attempt,
                        quantity: *quantity,
                        cum_qty: *cum_qty,
                        price: *price,
                        fee: *fee,
                    };
                    if *seen == same {
                        Ok(true)
                    } else {
                        Err(ArbitrageError::InvalidInput {
                            reason: "execution id reused with different content",
                        })
                    }
                }
            },
            LegExecution::Rejected { leg, attempt, .. } => Ok(self
                .closed_as(*leg, *attempt)
                .is_some_and(|(o, _)| o == AttemptOutcome::Rejected)),
            LegExecution::CancelAck {
                leg,
                attempt,
                cum_qty,
                ..
            } => Ok(self.closed_as(*leg, *attempt).is_some_and(|(o, f)| {
                o == AttemptOutcome::Cancelled && f == Some(cum_qty.normalize())
            })),
            LegExecution::Tick { .. }
            | LegExecution::Disconnected { .. }
            | LegExecution::Reconnected { .. }
            | LegExecution::VenueUpdate { .. } => Ok(false),
        }
    }

    fn attempt_mut(&mut self, leg: LegIndex, attempt: u32) -> Result<&mut AttemptState> {
        leg.of_mut(&mut self.legs)
            .attempts
            .get_mut(attempt as usize)
            .ok_or(ArbitrageError::InvalidInput {
                reason: "event for an attempt that was never sent",
            })
    }

    fn apply_fill(&mut self, exec_id: &ExecId, fill: FillRecord) -> Result<()> {
        if fill.fee.money().currency() != self.currency {
            return Err(ArbitrageError::CurrencyMismatch {
                left: self.currency,
                right: fill.fee.money().currency(),
            });
        }
        let invalid = |reason| Err(ArbitrageError::InvalidInput { reason });
        let state = self.attempt_mut(fill.leg, fill.attempt)?;
        if state.outcome == Some(AttemptOutcome::Rejected) {
            return invalid("fill for a rejected attempt");
        }
        let cum = exact::add(state.observed, fill.quantity.value())?;
        if cum != fill.cum_qty.value() {
            return invalid("cum_qty is not the attempt's cumulative plus the fill");
        }
        // Beyond the attempt quantity or the acknowledged final quantity it
        // is still a real execution: it is recorded, never rejected.
        state.observed = cum;
        let leg_state = fill.leg.of_mut(&mut self.legs);
        // The stored notional plus price * quantity, as one exact sum.
        let notional = wide::sum_products_exact(
            &[
                &[leg_state.filled_notional.amount()],
                &[fill.price.value(), fill.quantity.value()],
            ],
            "filled notional",
        )?;
        leg_state.filled_notional = Money::new(notional, self.currency);
        leg_state.fees = leg_state.fees.checked_add(fill.fee)?;
        // A fill above the price the reservation covers is still recorded;
        // the next readmission must cover it.
        if let Some(covered) = leg_state.covered_bound {
            let highest = leg_state.deficit.unwrap_or(covered).max(fill.price);
            if highest > covered {
                leg_state.deficit = Some(highest);
            }
        }
        self.fills.insert(exec_id.clone(), fill);
        Ok(())
    }

    fn close_attempt(
        &mut self,
        leg: LegIndex,
        attempt: u32,
        outcome: AttemptOutcome,
        final_cum: Decimal,
    ) -> Result<()> {
        let invalid = |reason| Err(ArbitrageError::InvalidInput { reason });
        let state = self.attempt_mut(leg, attempt)?;
        if state.outcome.is_some() {
            return invalid("attempt already closed with a different outcome");
        }
        if outcome == AttemptOutcome::Rejected && !state.observed.is_zero() {
            return invalid("rejection of an attempt that has fills");
        }
        if final_cum < state.observed {
            return invalid("acknowledged cum_qty is below the fills already applied");
        }
        if final_cum < Decimal::ZERO {
            return invalid("acknowledged cum_qty is negative");
        }
        state.outcome = Some(outcome);
        state.final_cum = Some(final_cum.normalize());
        Ok(())
    }

    /// Re-evaluates deadlines, surplus attempts, overfills, completion and
    /// the failure policy at `at`.
    fn step(
        &mut self,
        at: Timestamp,
        actions: &mut Vec<PlanAction>,
        mut budgets: Option<&mut BTreeMap<VenueId, VenueBudget>>,
        is_fill: bool,
    ) -> Result<()> {
        for i in [LegIndex::First, LegIndex::Second] {
            let remaining = i.of(&self.legs).remaining()?;
            let state = i.of_mut(&mut self.legs);
            let attempt = state.resubmits();
            if !state.current()?.is_live() || state.current()?.cancel_requested {
                continue;
            }
            // A live attempt made surplus by executions elsewhere (a late
            // fill beyond an acknowledgement) is cancelled.
            let surplus = remaining <= Decimal::ZERO;
            // Effective deadlines (own deadline or instrument expiry).
            let expired = !state.deadline_cancelled
                && state.spec.effective_deadline().is_some_and(|d| d <= at);
            if expired {
                state.deadline_cancelled = true;
            }
            if surplus || expired {
                state.current_mut()?.cancel_requested = true;
                actions.push(PlanAction::Cancel { leg: i, attempt });
            }
        }

        // An overfill is recorded as exposure. Only the surplus attempt is
        // cancelled (above); the other leg keeps working to its quantity, and
        // once both legs are done the policy's fallback applies to the
        // excess alone: abort keeps it visible, unwind offsets it.
        let statuses = [self.legs[0].status()?, self.legs[1].status()?];
        let both_filled = statuses.iter().all(|s| *s == LegStatus::Filled);
        let overfilled =
            self.legs[0].overfill()? > Decimal::ZERO || self.legs[1].overfill()? > Decimal::ZERO;
        if both_filled && matches!(self.state, PlanState::Working | PlanState::Completed) {
            if !overfilled {
                self.state = PlanState::Completed;
                return Ok(());
            }
            match self.fallback() {
                Fallback::Abort => self.state = PlanState::Aborted,
                Fallback::Unwind => {
                    self.state = PlanState::UnwindRequested;
                    self.request_unwind(actions)?;
                }
            }
            return Ok(());
        }

        if self.state == PlanState::Working {
            let failed = [LegIndex::First, LegIndex::Second]
                .into_iter()
                .find(|i| matches!(*i.of(&statuses), LegStatus::Rejected | LegStatus::Cancelled));
            if let Some(leg) = failed {
                if leg.of(&self.legs).pending_resubmit {
                    self.try_resubmit(leg, at, actions, budgets.as_deref_mut(), is_fill)?;
                } else {
                    self.on_failure(leg, at, actions, budgets, is_fill)?;
                }
            }
        }

        let all_done = self.legs[0].status()?.is_terminal() && self.legs[1].status()?.is_terminal();
        match self.state {
            PlanState::Aborting if all_done => self.state = PlanState::Aborted,
            PlanState::Unwinding if all_done => {
                self.state = PlanState::UnwindRequested;
                self.request_unwind(actions)?;
            }
            // Executions after the unwind was requested change the residual;
            // request the difference.
            PlanState::UnwindRequested => self.request_unwind(actions)?,
            _ => {}
        }
        Ok(())
    }

    /// The fallback the policy applies once recovery by resubmission is not
    /// possible.
    fn fallback(&self) -> Fallback {
        match self.policy {
            FailurePolicy::Abort => Fallback::Abort,
            FailurePolicy::Unwind => Fallback::Unwind,
            FailurePolicy::Complete { then, .. } => then,
        }
    }

    /// Enters `fallback` because of `leg` and cancels every live attempt
    /// that is not already being cancelled.
    fn enter_fallback(
        &mut self,
        fallback: Fallback,
        leg: LegIndex,
        actions: &mut Vec<PlanAction>,
    ) -> Result<()> {
        self.state = match fallback {
            Fallback::Abort => PlanState::Aborting,
            Fallback::Unwind => PlanState::Unwinding,
        };
        for i in [leg.other(), leg] {
            if let LegStatus::Working { attempt } = i.of(&self.legs).status()? {
                i.of_mut(&mut self.legs).current_mut()?.cancel_requested = true;
                actions.push(PlanAction::Cancel { leg: i, attempt });
            }
        }
        Ok(())
    }

    fn on_failure(
        &mut self,
        leg: LegIndex,
        at: Timestamp,
        actions: &mut Vec<PlanAction>,
        budgets: Option<&mut BTreeMap<VenueId, VenueBudget>>,
        is_fill: bool,
    ) -> Result<()> {
        if let FailurePolicy::Complete { max_resubmits, .. } = self.policy {
            let total: u32 = self.legs.iter().map(LegState::resubmits).sum();
            let state = leg.of_mut(&mut self.legs);
            // Checked against the event time itself: a rejection or an
            // acknowledgement at or after the effective deadline (or
            // expiry) never resubmits, with or without an earlier tick.
            let past_deadline = state.deadline_cancelled
                || state.spec.effective_deadline().is_some_and(|d| d <= at);
            if total < max_resubmits && !past_deadline {
                state.pending_resubmit = true;
                return self.try_resubmit(leg, at, actions, budgets, is_fill);
            }
        }
        self.enter_fallback(self.fallback(), leg, actions)
    }

    /// Sends a held resubmission if the venue (and, with budget gating, its
    /// budget) admits it, drops it for the fallback once the deadline has
    /// passed, and otherwise keeps holding. With budget gating, a fill event
    /// never readmits: the retry waits for the next non-fill event, so a
    /// readmission error can never reject a real execution.
    fn try_resubmit(
        &mut self,
        leg: LegIndex,
        at: Timestamp,
        actions: &mut Vec<PlanAction>,
        budgets: Option<&mut BTreeMap<VenueId, VenueBudget>>,
        is_fill: bool,
    ) -> Result<()> {
        if self.funding.is_some() && is_fill {
            return Ok(());
        }
        let state = leg.of(&self.legs);
        let past_deadline =
            state.deadline_cancelled || state.spec.effective_deadline().is_some_and(|d| d <= at);
        if past_deadline {
            leg.of_mut(&mut self.legs).pending_resubmit = false;
            return self.enter_fallback(self.fallback(), leg, actions);
        }
        match self.venue_gate(leg, at)? {
            VenueGate::Open => {}
            VenueGate::Disconnected => return Ok(()),
            VenueGate::Rejected(reason) => {
                if self.funding.is_some() {
                    leg.of_mut(&mut self.legs).hold = Some(reason);
                }
                return Ok(());
            }
        }
        if let Some(funding) = self.funding {
            let budgets = budgets.ok_or(ArbitrageError::InvalidInput {
                reason: "a plan with budget gating is driven with apply_with_budgets",
            })?;
            if let Some(reason) = self.readmit(leg, *leg.of(&funding), budgets)? {
                leg.of_mut(&mut self.legs).hold = Some(reason);
                return Ok(());
            }
        }
        let state = leg.of_mut(&mut self.legs);
        state.pending_resubmit = false;
        state.hold = None;
        // The executed quantity includes fills that the acknowledgement
        // reported but that are still in flight, so the resubmission never
        // overlaps them.
        let quantity = Quantity::new(state.remaining()?)?;
        state.attempts.push(AttemptState::new(quantity));
        actions.push(PlanAction::Resubmit {
            leg,
            attempt: state.resubmits(),
            quantity,
            price: state.spec.leg.price,
        });
        Ok(())
    }

    /// Whether the leg's venue can take a resubmission at `at`.
    fn venue_gate(&self, leg: LegIndex, at: Timestamp) -> Result<VenueGate> {
        let state = leg.of(&self.legs);
        if !state.connected {
            return Ok(VenueGate::Disconnected);
        }
        let Some(max_staleness_ms) = self.max_staleness_ms else {
            return Ok(VenueGate::Open);
        };
        let Some(view) = &state.view else {
            return Ok(VenueGate::Rejected(AdmissionReject::NoQuote));
        };
        let mut spec = state.spec.clone();
        spec.leg.quantity = Quantity::new(state.remaining()?)?;
        Ok(match check_view(&spec, view, at, max_staleness_ms)? {
            None => VenueGate::Open,
            Some(reason) => VenueGate::Rejected(reason),
        })
    }

    /// Readmits a leg before its resubmission: on a copy of its venue's
    /// budget, releases its current reservation and reserves the whole
    /// leg's requirement at the new price bound, all or nothing. Returns
    /// why it does not fit, changing nothing, or `None` once `budgets` and
    /// the leg hold the new reservation.
    fn readmit(
        &mut self,
        leg: LegIndex,
        funding: LegFunding,
        budgets: &mut BTreeMap<VenueId, VenueBudget>,
    ) -> Result<Option<AdmissionReject>> {
        let state = leg.of(&self.legs);
        let Some(current) = &state.reservation else {
            return Ok(Some(AdmissionReject::Released));
        };
        let covered = state.covered_bound.ok_or(ArbitrageError::InvalidInput {
            reason: "a reserved leg has no covered price bound",
        })?;
        let mut bound = covered.max(state.deficit.unwrap_or(covered));
        // venue_gate admitted the view, so it exists.
        if let Some(view) = &state.view {
            bound = bound.max(price_bound(&state.spec, view)?);
        }
        let need = requirement_at(&state.spec, bound, funding, current.cash.currency())?;
        let Some(budget) = budgets.get(&state.spec.venue) else {
            return Ok(Some(AdmissionReject::NoBudget));
        };
        let mut trial = budget.clone();
        trial.release(current)?;
        if let Some(reason) = trial.reserve(&need)? {
            return Ok(Some(reason));
        }
        budgets.insert(state.spec.venue.clone(), trial);
        let state = leg.of_mut(&mut self.legs);
        state.reservation = Some(need);
        state.covered_bound = Some(bound);
        state.deficit = None;
        Ok(None)
    }

    fn request_unwind(&mut self, actions: &mut Vec<PlanAction>) -> Result<()> {
        let residual = self.residual()?;
        let open = exact::sub(residual, self.unwind_requested)?;
        if open.is_zero() {
            return Ok(());
        }
        // Offset on the leg whose position has the sign of the open residual.
        let first = self.legs[0].position()?;
        let source = if !first.is_zero() && first.is_sign_negative() == open.is_sign_negative() {
            &self.legs[0]
        } else {
            &self.legs[1]
        };
        let side = if open.is_sign_positive() {
            Action::Sell
        } else {
            Action::Buy
        };
        actions.push(PlanAction::Unwind {
            instrument: source.spec.leg.instrument.clone(),
            side,
            quantity: Quantity::new(open.abs())?,
        });
        self.unwind_requested = residual;
        Ok(())
    }
}
