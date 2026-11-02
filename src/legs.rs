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
//! reservation computed at an earlier, lower price.
//!
//! ## Unwinds
//!
//! An unwind offsets the open residual: `residual - coverage`, where the
//! coverage of the unwinds already requested is signed as the residual
//! they neutralise (a sell covers a long residual, a buy a short one) and
//! counts a live unwind's whole quantity and an ended one's executed
//! quantity. The plan never sees an unwind execute: it never assumes an
//! unwind executed when it emits it, and a zero open residual with live
//! unwinds means requested, not flat. The caller reports each unwind's end
//! (rejected, cancelled or filled, with its final executed quantity) with
//! [`LegExecution::UnwindDone`]; whatever it did not execute is open again
//! and requested anew.
//!
//! With budget gating every unwind is admitted like a resubmission before it
//! is sent: on its venue's latest view (connected, fresh, quoted on its side
//! with size), at a limit equal to that view's executable price on its side,
//! with its capital (valued at the same view, fee included) reserved on that
//! venue's budget, all or nothing. A sell that unwinds a buy leg is funded
//! as [`AdmittedLeg::unwind_funding`] says; a buy that unwinds a sell leg
//! needs cash. When it does not fit, the unwind is held
//! ([`TwoLegPlan::unwind_hold`]) and retried on each later non-fill event,
//! never on a fill. In particular:
//!
//! - the plan never changes budgets for fills. When a fill changes what the
//!   caller holds on a venue (for example the inventory a buy leg acquired,
//!   which a sell unwind of that leg needs), the caller credits its
//!   [`VenueBudget`] once and sends a [`LegExecution::Tick`]; until then the
//!   unwind stays held with the shortfall as its reason;
//! - the plan never requests the opposite side while an unwind is live: a
//!   residual that fell, reached zero or changed sign is held with
//!   [`AdmissionReject::UnwindReversal`] until the live unwinds' ends are
//!   reported;
//! - an unwind keeps its limit: the plan does not chase the price. A new
//!   unwind is admitted on the view current when it is requested;
//! - an unwind's reservation is held until
//!   [`TwoLegPlan::release_reservations`], also once it ended, and never
//!   covers another order; release is refused while an unwind is live;
//! - with [`TwoLegPlan::with_unwind_deadline`] no unwind is admitted at or
//!   after the deadline; after a release none is admitted at all.
//!
//! Without budget gating an unwind is emitted at once, with no price, and
//! the caller funds it.
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
    AdmissionReject, AdmittedLeg, LegFunding, LegRequest, Reservation, SellFunding, VenueBudget,
    VenueId, VenueView, check_view, price_bound, requirement, requirement_at,
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
    /// An unwind the plan requested ended on the leg's venue: rejected
    /// (`filled` zero), or cancelled or filled with `filled` its final
    /// cumulative executed quantity. Until then the unwind is live and its
    /// whole quantity counts as covering the residual; from then on only
    /// `filled` does, and the plan requests what is still open.
    UnwindDone {
        /// The leg whose residual the unwind offsets (its venue is the
        /// unwind's).
        leg: LegIndex,
        /// The unwind's number ([`PlanAction::Unwind::unwind`]).
        unwind: u32,
        /// Its final cumulative executed quantity (zero or more; above the
        /// unwind's quantity it is recorded, not rejected).
        filled: Decimal,
        /// When it was observed.
        at: Timestamp,
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
            | Self::VenueUpdate { leg, .. }
            | Self::UnwindDone { leg, .. } => Some(*leg),
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
            | Self::Reconnected { at, .. }
            | Self::UnwindDone { at, .. } => *at,
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
    /// plan's residual is that of its legs and stays visible; report the
    /// unwind's end with [`LegExecution::UnwindDone`].
    Unwind {
        /// The unwind's number in the plan, from 0.
        unwind: u32,
        /// Venue to send it to: the venue of the leg whose position it
        /// offsets.
        venue: VenueId,
        /// Instrument to trade.
        instrument: Instrument,
        /// The action that reduces the residual.
        side: Action,
        /// Quantity to offset.
        quantity: Quantity,
        /// Limit price. With budget gating it is the executable price of the
        /// venue's latest admitted view on the unwind's side (the ask for a
        /// buy, the bid for a sell), and the capital is reserved at it; the
        /// plan never chases the price. Without budget gating it is `None`:
        /// the caller prices the unwind.
        price: Option<Price>,
    },
}

/// One unwind the plan requested.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnwindState {
    /// The leg whose position it offsets; it trades on that leg's venue.
    pub leg: LegIndex,
    /// Its side.
    pub side: Action,
    /// Its quantity.
    pub quantity: Quantity,
    /// Its limit price ([`PlanAction::Unwind::price`]).
    pub price: Option<Price>,
    /// Its final cumulative executed quantity once
    /// [`LegExecution::UnwindDone`] reported it; `None` while it is live.
    pub filled: Option<Decimal>,
    /// The capital reserved for it on its venue with budget gating, held
    /// until [`TwoLegPlan::release_reservations`], also once it ended.
    pub reservation: Option<Reservation>,
}

impl UnwindState {
    /// True until its end is reported.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.filled.is_none()
    }

    /// The residual exposure it covers, signed as the residual it
    /// neutralises: a sell covers a long residual (positive), a buy a short
    /// one (negative). A live unwind covers its whole quantity, an ended one
    /// what it executed.
    fn coverage(&self) -> Result<Decimal> {
        let amount = self.filled.unwrap_or_else(|| self.quantity.value());
        Ok(match Dir::try_from(self.side)? {
            Dir::Sell => amount,
            Dir::Buy => exact::neg(amount),
        })
    }
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

/// What a leg's recorded fills use beyond its current reservation, with
/// budget gating ([`TwoLegPlan::with_budget_gating`]).
///
/// The reservation covers the leg's quantity with its notional valued at
/// [`LegState::covered_bound`] and the fee modelled at that bound. Fills are
/// always recorded; whatever they use beyond that is reported here, never as
/// an error. The next readmission covers the price and the fee (it raises
/// the bound and adds the excess fee) or holds the resubmission; an
/// overfill is never resubmitted, so it stays reported.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct CoverageDeficit {
    /// The highest fill price above the covered bound.
    pub price: Option<Price>,
    /// The fees charged above the fee modelled at the covered bound for the
    /// executed quantity, cumulatively over the leg's fills and not yet
    /// included in the reservation.
    pub fee: Option<Money>,
    /// The quantity executed beyond the leg's quantity.
    pub overfill: Option<Decimal>,
    /// True when what the fills use could not be computed exactly (a
    /// product that does not fit a `Decimal`): the fills stand, the other
    /// fields are not measured, and no resubmission is sent
    /// ([`AdmissionReject::CoverageUnknown`]).
    pub coverage_unknown: bool,
}

impl CoverageDeficit {
    fn is_empty(&self) -> bool {
        self.price.is_none()
            && self.fee.is_none()
            && self.overfill.is_none()
            && !self.coverage_unknown
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
    /// With budget gating: what the recorded fills use beyond the current
    /// reservation, if anything. The fills stand; the next resubmission is
    /// first readmitted to cover it, or held.
    pub deficit: Option<CoverageDeficit>,
    /// With budget gating: why the last readmission held the resubmission
    /// back, if it did.
    pub hold: Option<AdmissionReject>,
    /// The fees above the modelled fee that the current reservation already
    /// includes (from the last readmission).
    covered_excess_fee: Decimal,
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
    /// True once any event has been applied.
    started: bool,
    fills: BTreeMap<ExecId, FillRecord>,
    /// How a sell unwinding each leg is funded, with budget gating.
    unwind_funding: Option<[SellFunding; 2]>,
    /// Every unwind requested, in order; its index is its number.
    unwinds: Vec<UnwindState>,
    /// Why the open residual's unwind is held, with budget gating.
    unwind_hold: Option<AdmissionReject>,
    /// No unwind is admitted at or after this instant.
    unwind_deadline: Option<Timestamp>,
    /// True once the reservations were released.
    released: bool,
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
            covered_excess_fee: Decimal::ZERO,
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
            started: false,
            fills: BTreeMap::new(),
            unwind_funding: None,
            unwinds: Vec::new(),
            unwind_hold: None,
            unwind_deadline: None,
            released: false,
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
    /// - a fill is always recorded. What the fills use beyond the current
    ///   reservation (a price above the covered bound, cumulative fees above
    ///   the fee modelled at it for the executed quantity, an overfill) is
    ///   reported in [`LegState::deficit`], never as an error; when it cannot
    ///   be computed exactly the coverage is marked unknown. The next
    ///   readmission covers the price and the fees, or holds the
    ///   resubmission ([`AdmissionReject::CoverageUnknown`] when unknown).
    ///   Fill events never readmit, so no budget shortfall can reject or
    ///   lose an execution;
    /// - capital is never borrowed across venues: a leg only ever reserves
    ///   on its own venue's budget.
    ///
    /// The plan is then driven with [`TwoLegPlan::apply_with_budgets`]; when
    /// it ends (completed, aborted, unwound, or abandoned after an error),
    /// the caller releases the current reservations with
    /// [`TwoLegPlan::release_reservations`], never the ones the admission
    /// returned. Unwinds are admitted and reserved as well (see the module
    /// docs), with [`AdmittedLeg::unwind_funding`] for a sell that unwinds a
    /// buy leg.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidInput`] if the plan already has budget
    /// gating (even released) or has already applied any event, a view is
    /// of another venue or instrument than its leg, or a reservation is of
    /// another venue or is not exactly the leg's requirement on its view,
    /// fee and funding; [`ArbitrageError::CurrencyMismatch`] if a
    /// reservation is not in the plan currency; the errors of
    /// [`requirement`].
    pub fn with_budget_gating(
        mut self,
        max_staleness_ms: i64,
        legs: [AdmittedLeg; 2],
    ) -> Result<Self> {
        if self.funding.is_some() {
            return Err(ArbitrageError::InvalidInput {
                reason: "the plan already has budget gating",
            });
        }
        if self.started {
            return Err(ArbitrageError::InvalidInput {
                reason: "budget gating must be set before the plan sees an event",
            });
        }
        let [first, second] = legs;
        let funding = [first.funding, second.funding];
        let unwind_funding = [first.unwind_funding, second.unwind_funding];
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
            if admitted.reservation.cash.currency() != self.currency {
                return Err(ArbitrageError::CurrencyMismatch {
                    left: self.currency,
                    right: admitted.reservation.cash.currency(),
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
        self.unwind_funding = Some(unwind_funding);
        Ok(self)
    }

    /// Stops admitting unwinds at `deadline`, for a plan with budget gating:
    /// an unwind is admitted only at an event time strictly before it (the
    /// same boundary as a leg's deadline). Past it, the open residual's
    /// unwind is held with [`AdmissionReject::PastDeadline`] and never
    /// retried; unwinds already requested are not cancelled, their ends are
    /// still applied, and the residual stays visible. Without a deadline a
    /// held unwind is retried until it is admitted or the reservations are
    /// released.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidInput`] for a plan without budget gating or
    /// one that has already applied an event.
    pub fn with_unwind_deadline(mut self, deadline: Timestamp) -> Result<Self> {
        if self.funding.is_none() {
            return Err(ArbitrageError::InvalidInput {
                reason: "an unwind deadline needs a plan with budget gating",
            });
        }
        if self.started {
            return Err(ArbitrageError::InvalidInput {
                reason: "the unwind deadline must be set before the plan sees an event",
            });
        }
        self.unwind_deadline = Some(deadline);
        Ok(self)
    }

    /// Every unwind the plan requested, in order; an unwind's number is its
    /// index.
    #[must_use]
    pub fn unwinds(&self) -> &[UnwindState] {
        &self.unwinds
    }

    /// Why the unwind of the open residual is held, with budget gating.
    #[must_use]
    pub fn unwind_hold(&self) -> Option<&AdmissionReject> {
        self.unwind_hold.as_ref()
    }

    /// The residual exposure the requested unwinds cover, signed as the
    /// residual they neutralise: a live unwind its whole quantity, an ended
    /// one what it executed. `residual() - unwind_coverage()` is what is
    /// still open. A zero open residual with live unwinds means requested,
    /// not flat: the plan does not see the unwinds execute.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn unwind_coverage(&self) -> Result<Decimal> {
        let values = self
            .unwinds
            .iter()
            .map(UnwindState::coverage)
            .collect::<Result<Vec<_>>>()?;
        wide::sum(&values, "unwind coverage")
    }

    /// Releases the legs' current reservations and the unwinds'
    /// reservations from `budgets`, all or nothing, for a plan with budget
    /// gating. Call it once the plan has ended, or is abandoned, after every
    /// unwind's end is reported ([`LegExecution::UnwindDone`]): it neither
    /// cancels nor confirms orders, so it is refused while an unwind is
    /// live. It is idempotent: a released reservation is not released again,
    /// and the plan sends no resubmission or unwind afterwards (both are
    /// held with [`AdmissionReject::Released`]); later fills are still
    /// recorded.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidInput`] for a plan without budget gating,
    /// while an unwind is live, or when a reservation's venue has no budget;
    /// the errors of [`VenueBudget::release`]. Neither the plan nor
    /// `budgets` change on error.
    pub fn release_reservations(
        &mut self,
        budgets: &mut BTreeMap<VenueId, VenueBudget>,
    ) -> Result<()> {
        if self.funding.is_none() {
            return Err(ArbitrageError::InvalidInput {
                reason: "plan has no budget gating, so no reservation to release",
            });
        }
        // Releasing the capital of an order that may still be in the market
        // would let it be spent twice.
        if self.unwinds.iter().any(UnwindState::is_live) {
            return Err(ArbitrageError::InvalidInput {
                reason: "an unwind is still live: apply its UnwindDone first",
            });
        }
        let mut next = budgets.clone();
        let held = self
            .legs
            .iter()
            .filter_map(|state| state.reservation.as_ref())
            .chain(self.unwinds.iter().filter_map(|u| u.reservation.as_ref()));
        for reservation in held {
            next.get_mut(&reservation.venue)
                .ok_or(ArbitrageError::InvalidInput {
                    reason: "no budget for the venue of a reservation",
                })?
                .release(reservation)?;
        }
        for state in &mut self.legs {
            state.reservation = None;
        }
        for unwind in &mut self.unwinds {
            unwind.reservation = None;
        }
        self.released = true;
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
        next.started = true;
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
            LegExecution::UnwindDone {
                leg,
                unwind,
                filled,
                ..
            } => next.close_unwind(*leg, *unwind, *filled)?,
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
            LegExecution::UnwindDone {
                leg,
                unwind,
                filled,
                ..
            } => Ok(self
                .unwinds
                .get(*unwind as usize)
                .is_some_and(|u| u.leg == *leg && u.filled == Some(filled.normalize()))),
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
        let leg = fill.leg;
        self.fills.insert(exec_id.clone(), fill);
        // The fill stands; what it uses beyond the reservation is reported.
        if let Some(covered) = leg.of(&self.legs).covered_bound {
            let deficit = self.deficit(leg, covered);
            leg.of_mut(&mut self.legs).deficit = deficit;
        }
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

    /// Records an unwind's end. A quantity above the unwind's is a real
    /// execution and is recorded.
    fn close_unwind(&mut self, leg: LegIndex, unwind: u32, filled: Decimal) -> Result<()> {
        let invalid = |reason| Err(ArbitrageError::InvalidInput { reason });
        let Some(state) = self.unwinds.get_mut(unwind as usize) else {
            return invalid("end of an unwind that was never requested");
        };
        if state.leg != leg {
            return invalid("unwind end reported for another leg than the unwind's");
        }
        if filled < Decimal::ZERO {
            return invalid("unwind filled quantity is negative");
        }
        if state.filled.is_some() {
            return invalid("unwind closed twice differently");
        }
        state.filled = Some(filled.normalize());
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
                    self.request_unwind(at, actions, budgets, is_fill)?;
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
                    self.on_failure(leg, at, actions, budgets.as_deref_mut(), is_fill)?;
                }
            }
        }

        let all_done = self.legs[0].status()?.is_terminal() && self.legs[1].status()?.is_terminal();
        match self.state {
            PlanState::Aborting if all_done => self.state = PlanState::Aborted,
            PlanState::Unwinding if all_done => {
                self.state = PlanState::UnwindRequested;
                self.request_unwind(at, actions, budgets, is_fill)?;
            }
            // Executions after the unwind was requested, and unwinds that
            // ended short, change what is open; request it.
            PlanState::UnwindRequested => self.request_unwind(at, actions, budgets, is_fill)?,
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
    /// passed (on any event), and otherwise keeps holding. With budget
    /// gating, a fill event never readmits: the retry waits for the next
    /// non-fill event, so a readmission error can never reject a real
    /// execution.
    fn try_resubmit(
        &mut self,
        leg: LegIndex,
        at: Timestamp,
        actions: &mut Vec<PlanAction>,
        budgets: Option<&mut BTreeMap<VenueId, VenueBudget>>,
        is_fill: bool,
    ) -> Result<()> {
        let state = leg.of(&self.legs);
        let past_deadline =
            state.deadline_cancelled || state.spec.effective_deadline().is_some_and(|d| d <= at);
        if past_deadline {
            // The fallback touches no budget, so it applies on any event.
            leg.of_mut(&mut self.legs).pending_resubmit = false;
            return self.enter_fallback(self.fallback(), leg, actions);
        }
        if self.funding.is_some() && is_fill {
            return Ok(());
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
        let mut bound = covered.max(
            state
                .deficit
                .as_ref()
                .and_then(|d| d.price)
                .unwrap_or(covered),
        );
        // venue_gate admitted the view, so it exists.
        if let Some(view) = &state.view {
            bound = bound.max(price_bound(&state.spec, view)?);
        }
        if state.deficit.as_ref().is_some_and(|d| d.coverage_unknown) {
            return Ok(Some(AdmissionReject::CoverageUnknown));
        }
        // The whole leg (or what it executed, if more) at the bound, plus the
        // fees charged above the fee modelled at that bound: the fills'
        // notional and modelled fee are within the bound's, so nothing is
        // counted twice, and an excess a higher bound now models is dropped.
        let mut spec = state.spec.clone();
        let executed = state.executed()?;
        if executed > spec.leg.quantity.value() {
            spec.leg.quantity = Quantity::new(executed)?;
        }
        let computed = (|| -> Result<(Reservation, Money)> {
            let mut need = requirement_at(&spec, bound, funding, current.cash.currency())?;
            let excess = self.excess_fees(leg, bound, funding)?;
            need.cash = need.cash.checked_add(excess)?;
            Ok((need, excess))
        })();
        let (need, excess) = match computed {
            Ok(values) => values,
            Err(ArbitrageError::Overflow { .. } | ArbitrageError::Inexact { .. }) => {
                return Ok(Some(AdmissionReject::CoverageUnknown));
            }
            Err(error) => return Err(error),
        };
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
        state.covered_excess_fee = excess.amount();
        let deficit = self.deficit(leg, bound);
        leg.of_mut(&mut self.legs).deficit = deficit;
        Ok(None)
    }

    /// What the leg's recorded fills use beyond a reservation covering its
    /// quantity at `covered`. It never fails: a value that cannot be
    /// computed exactly marks the coverage as unknown instead, so a real
    /// fill is never rejected by this diagnostic.
    fn deficit(&self, leg: LegIndex, covered: Price) -> Option<CoverageDeficit> {
        let funding = *leg.of(&self.funding?);
        let state = leg.of(&self.legs);
        let price = self
            .fills
            .values()
            .filter(|f| f.leg == leg)
            .map(|f| f.price)
            .max()
            .filter(|p| *p > covered);
        let measured = (|| -> Result<(Decimal, Decimal)> {
            // Only the excess the reservation does not include yet.
            let fee = exact::sub(
                self.excess_fees(leg, covered, funding)?.amount(),
                state.covered_excess_fee,
            )?;
            Ok((fee, state.overfill()?))
        })();
        let deficit = match measured {
            Ok((fee, overfill)) => CoverageDeficit {
                price,
                fee: (fee > Decimal::ZERO).then(|| Money::new(fee, self.currency)),
                overfill: (overfill > Decimal::ZERO).then_some(overfill),
                coverage_unknown: false,
            },
            Err(_) => CoverageDeficit {
                price,
                fee: None,
                overfill: None,
                coverage_unknown: true,
            },
        };
        (!deficit.is_empty()).then_some(deficit)
    }

    /// The fees the leg's fills were charged above the fee modelled at
    /// `bound` for the quantity they executed, cumulatively (a later rebate
    /// offsets an earlier excess): `max(0, sum(fees) - max(0, rate * bound *
    /// sum(quantities)))`, the model floored at zero as in the requirement.
    fn excess_fees(&self, leg: LegIndex, bound: Price, funding: LegFunding) -> Result<Money> {
        let (mut fees, mut quantities) = (Vec::new(), Vec::new());
        for fill in self.fills.values().filter(|f| f.leg == leg) {
            fees.push(fill.fee.money().amount());
            quantities.push(fill.quantity.value());
        }
        let observed = wide::sum(&fees, "observed fees")?;
        let executed = wide::sum(&quantities, "filled quantity")?;
        let modelled = funding
            .fee
            .fee_on_factors(&[bound.value(), executed], self.currency)?
            .money()
            .amount()
            .max(Decimal::ZERO);
        let excess = exact::sub(observed, modelled)?.max(Decimal::ZERO);
        Ok(Money::new(excess, self.currency))
    }

    /// Requests an unwind of the open residual, `residual - coverage`.
    ///
    /// Without budget gating the unwind is emitted at once with no price.
    /// With it, the unwind is held unless it can be admitted now: never the
    /// opposite side of a live unwind ([`AdmissionReject::UnwindReversal`]),
    /// never after the reservations were released or the unwind deadline,
    /// and never on a fill event; then on its venue's latest view (connected,
    /// fresh, quoted on its side with size) and with its capital reserved on
    /// that venue's budget, all or nothing.
    fn request_unwind(
        &mut self,
        at: Timestamp,
        actions: &mut Vec<PlanAction>,
        budgets: Option<&mut BTreeMap<VenueId, VenueBudget>>,
        is_fill: bool,
    ) -> Result<()> {
        let residual = self.residual()?;
        let coverage = self.unwind_coverage()?;
        let open = exact::sub(residual, coverage)?;
        if open.is_zero() {
            self.unwind_hold = None;
            return Ok(());
        }
        // Offset on the leg whose position has the sign of the open residual.
        let first = self.legs[0].position()?;
        let leg = if !first.is_zero() && first.is_sign_negative() == open.is_sign_negative() {
            LegIndex::First
        } else {
            LegIndex::Second
        };
        let side = if open.is_sign_positive() {
            Action::Sell
        } else {
            Action::Buy
        };
        let quantity = Quantity::new(open.abs())?;
        let (Some(funding), Some(unwind_funding)) = (self.funding, self.unwind_funding) else {
            self.push_unwind(leg, side, quantity, None, None, actions)?;
            return Ok(());
        };
        let reversal = !coverage.is_zero()
            && coverage.is_sign_negative() != open.is_sign_negative()
            && self.unwinds.iter().any(UnwindState::is_live);
        if reversal {
            self.unwind_hold = Some(AdmissionReject::UnwindReversal { open });
            return Ok(());
        }
        if self.released {
            self.unwind_hold = Some(AdmissionReject::Released);
            return Ok(());
        }
        if self.unwind_deadline.is_some_and(|d| d <= at) {
            self.unwind_hold = Some(AdmissionReject::PastDeadline);
            return Ok(());
        }
        // A fill never admits: the retry waits for the next non-fill event.
        if is_fill {
            return Ok(());
        }
        let budgets = budgets.ok_or(ArbitrageError::InvalidInput {
            reason: "a plan with budget gating is driven with apply_with_budgets",
        })?;
        let state = leg.of(&self.legs);
        let funding = LegFunding {
            fee: leg.of(&funding).fee,
            sell_funding: *leg.of(&unwind_funding),
        };
        match self.admit_unwind(state, side, quantity, funding, at, budgets)? {
            Ok((price, reservation)) => {
                self.unwind_hold = None;
                self.push_unwind(leg, side, quantity, Some(price), Some(reservation), actions)
            }
            Err(reason) => {
                self.unwind_hold = Some(reason);
                Ok(())
            }
        }
    }

    /// Admits an unwind of `quantity` on `state`'s venue: its price, bound,
    /// fee and requirement all come from the venue's latest view, checked at
    /// `at`, and the requirement is reserved on a copy of the venue's budget,
    /// committed to `budgets` only if it fits. Returns the limit price and
    /// the reservation, or why it is held.
    fn admit_unwind(
        &self,
        state: &LegState,
        side: Action,
        quantity: Quantity,
        funding: LegFunding,
        at: Timestamp,
        budgets: &mut BTreeMap<VenueId, VenueBudget>,
    ) -> Result<core::result::Result<(Price, Reservation), AdmissionReject>> {
        if !state.connected {
            return Ok(Err(AdmissionReject::Disconnected));
        }
        let Some(view) = &state.view else {
            return Ok(Err(AdmissionReject::NoQuote));
        };
        let Some(level) = view.book.executable(side)? else {
            return Ok(Err(AdmissionReject::NoQuote));
        };
        let spec = LegSpec {
            leg: Leg {
                instrument: state.spec.leg.instrument.clone(),
                side,
                quantity,
                price: level.price,
            },
            deadline: None,
            venue: state.spec.venue.clone(),
        };
        let max_staleness_ms = self.max_staleness_ms.unwrap_or(0);
        if let Some(reason) = check_view(&spec, view, at, max_staleness_ms)? {
            return Ok(Err(reason));
        }
        let computed = price_bound(&spec, view)
            .and_then(|bound| requirement_at(&spec, bound, funding, self.currency));
        let need = match computed {
            Ok(need) => need,
            Err(ArbitrageError::Overflow { .. } | ArbitrageError::Inexact { .. }) => {
                return Ok(Err(AdmissionReject::CoverageUnknown));
            }
            Err(error) => return Err(error),
        };
        let Some(budget) = budgets.get(&spec.venue) else {
            return Ok(Err(AdmissionReject::NoBudget));
        };
        let mut trial = budget.clone();
        if let Some(reason) = trial.reserve(&need)? {
            return Ok(Err(reason));
        }
        budgets.insert(spec.venue.clone(), trial);
        Ok(Ok((level.price, need)))
    }

    /// Records an unwind and emits its intent.
    fn push_unwind(
        &mut self,
        leg: LegIndex,
        side: Action,
        quantity: Quantity,
        price: Option<Price>,
        reservation: Option<Reservation>,
        actions: &mut Vec<PlanAction>,
    ) -> Result<()> {
        let unwind = u32::try_from(self.unwinds.len()).map_err(|_| ArbitrageError::Overflow {
            op: "unwind number",
        })?;
        let spec = &leg.of(&self.legs).spec;
        actions.push(PlanAction::Unwind {
            unwind,
            venue: spec.venue.clone(),
            instrument: spec.leg.instrument.clone(),
            side,
            quantity,
            price,
        });
        self.unwinds.push(UnwindState {
            leg,
            side,
            quantity,
            price,
            filled: None,
            reservation,
        });
        Ok(())
    }
}
