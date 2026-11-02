//! Two-leg plan execution state machine.
//!
//! A [`TwoLegPlan`] tracks two legs meant to offset each other one to one
//! (for example the buy and sell legs of an
//! [`Opportunity`]) as execution events are
//! observed. It never assumes that both legs fill together:
//!
//! - every fill, rejection and cancel acknowledgement is applied as observed,
//!   and the residual exposure (the signed sum of both legs' filled
//!   positions) is always visible;
//! - when a leg fails (rejected, or cancelled with quantity left), the
//!   configured [`FailurePolicy`] decides the recovery intent: resubmit the
//!   failed leg, abort (cancel the other leg and keep the residual), or
//!   unwind (cancel the other leg, then offset the residual);
//! - a leg deadline asks for a cancel; the leg stays working until the cancel
//!   is acknowledged, and fills that arrive before or after the
//!   acknowledgement are still applied;
//! - events are idempotent: a fill is identified by its execution id, and a
//!   rejection or acknowledgement by its leg and attempt, so a repeated event
//!   changes nothing.
//!
//! The plan only emits intents ([`PlanAction`]); sending orders is the
//! caller's job. Observations are assumed to arrive in time order;
//! per-venue clocks and delayed views are a later extension.

use crate::cost::Cost;
use crate::error::{ArbitrageError, Result};
use crate::exact;
use crate::instrument::{Instrument, Leg, Side, Timestamp};
use crate::money::{Currency, Money};
use crate::opportunity::Opportunity;
use crate::units::{Price, Quantity};
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

    fn slot(self) -> usize {
        match self {
            Self::First => 0,
            Self::Second => 1,
        }
    }
}

/// A leg to execute and its optional deadline.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LegSpec {
    /// Instrument, side, quantity and limit price.
    pub leg: Leg,
    /// When to stop working the leg; reaching it requests a cancel.
    pub deadline: Option<Timestamp>,
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
    /// times in total, then apply `then`.
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
    /// A (partial) fill.
    Fill {
        /// The leg.
        leg: LegIndex,
        /// The order attempt it belongs to (0 for the first submission).
        attempt: u32,
        /// Venue execution id; a repeat with the same content is ignored.
        exec_id: ExecId,
        /// Filled quantity.
        quantity: Quantity,
        /// Fill price.
        price: Price,
        /// Fee charged (negative for a rebate), in the plan currency.
        fee: Cost,
        /// When it was observed.
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
        /// When it was observed.
        at: Timestamp,
    },
    /// Time has advanced to `at` with no execution event; checks deadlines.
    Tick {
        /// The current time.
        at: Timestamp,
    },
}

impl LegExecution {
    fn at(&self) -> Timestamp {
        match self {
            Self::Fill { at, .. }
            | Self::Rejected { at, .. }
            | Self::CancelAck { at, .. }
            | Self::Tick { at } => *at,
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
        /// Quantity still to fill.
        quantity: Quantity,
        /// Limit price (the plan's price for the leg).
        price: Price,
    },
    /// Send an order that offsets residual exposure. It is an intent: the
    /// residual stays visible until fills are observed elsewhere.
    Unwind {
        /// Instrument to trade (the leg that filled more).
        instrument: Instrument,
        /// Side that reduces the residual.
        side: Side,
        /// Quantity to offset.
        quantity: Quantity,
    },
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
    /// The whole quantity is filled.
    Filled,
    /// The last attempt was rejected with quantity left.
    Rejected,
    /// The last attempt was cancelled with quantity left.
    Cancelled,
}

impl LegStatus {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Filled | Self::Rejected | Self::Cancelled)
    }
}

/// Where the plan stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanState {
    /// Both legs are being worked (possibly after resubmissions).
    Working,
    /// Both legs are filled.
    Completed,
    /// A leg failed; the other leg is being cancelled before aborting.
    Aborting,
    /// A leg failed; the other leg is being cancelled before unwinding.
    Unwinding,
    /// Both legs are done and the residual is kept.
    Aborted,
    /// Both legs are done under the unwind policy: every residual, including
    /// one created by a late fill, is offset by an [`PlanAction::Unwind`]
    /// intent (none when the legs ended flat).
    UnwindRequested,
}

/// Running totals of one leg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegState {
    /// The leg as planned.
    pub spec: LegSpec,
    /// Its status.
    pub status: LegStatus,
    /// Total filled quantity over all attempts.
    pub filled: Decimal,
    /// Sum of `price * quantity` over fills.
    pub filled_notional: Money,
    /// Sum of fees over fills.
    pub fees: Cost,
    /// Number of resubmissions so far.
    pub resubmits: u32,
    deadline_cancelled: bool,
}

impl LegState {
    /// Quantity left to fill.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn remaining(&self) -> Result<Decimal> {
        exact::sub(self.spec.leg.quantity.value(), self.filled)
    }

    /// Signed filled position: positive for a buy leg, negative for a sell.
    #[must_use]
    pub fn position(&self) -> Decimal {
        match self.spec.leg.side {
            Side::Buy => self.filled,
            Side::Sell => exact::neg(self.filled),
        }
    }

    fn attempt(&self) -> Option<u32> {
        match self.status {
            LegStatus::Working { attempt } | LegStatus::CancelRequested { attempt } => {
                Some(attempt)
            }
            _ => None,
        }
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
    /// filled positions (zero when the legs offset each other).
    pub residual: Decimal,
    /// True if the event was a repeat and changed nothing.
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FillRecord {
    leg: LegIndex,
    attempt: u32,
    quantity: Quantity,
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
    last_at: Timestamp,
    fills: BTreeMap<ExecId, FillRecord>,
    closed_attempts: BTreeMap<(LegIndex, u32), LegStatus>,
    unwind_requested: Decimal,
}

impl TwoLegPlan {
    /// A plan for two offsetting legs, created at `created_at`. Both legs
    /// start working as attempt 0.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidInput`] if the legs are on the same
    /// instrument, on the same side, or have a deadline at or before
    /// `created_at`.
    pub fn new(
        first: LegSpec,
        second: LegSpec,
        policy: FailurePolicy,
        currency: Currency,
        created_at: Timestamp,
    ) -> Result<Self> {
        if first.leg.instrument.id == second.leg.instrument.id {
            return Err(ArbitrageError::InvalidInput {
                reason: "the two legs are the same instrument",
            });
        }
        if first.leg.side == second.leg.side {
            return Err(ArbitrageError::InvalidInput {
                reason: "the two legs must be on opposite sides to offset",
            });
        }
        for spec in [&first, &second] {
            if spec.deadline.is_some_and(|d| d <= created_at) {
                return Err(ArbitrageError::InvalidInput {
                    reason: "a leg deadline is not after the plan creation",
                });
            }
        }
        let leg_state = |spec: LegSpec| LegState {
            spec,
            status: LegStatus::Working { attempt: 0 },
            filled: Decimal::ZERO,
            filled_notional: Money::zero(currency),
            fees: Cost::new(Money::zero(currency)),
            resubmits: 0,
            deadline_cancelled: false,
        };
        Ok(Self {
            currency,
            policy,
            legs: [leg_state(first), leg_state(second)],
            state: PlanState::Working,
            last_at: created_at,
            fills: BTreeMap::new(),
            closed_attempts: BTreeMap::new(),
            unwind_requested: Decimal::ZERO,
        })
    }

    /// A plan for an accepted [`Opportunity`]: its buy leg first and its sell
    /// leg second, both with `deadline`.
    ///
    /// # Errors
    ///
    /// As [`TwoLegPlan::new`].
    pub fn from_opportunity(
        opportunity: &Opportunity,
        deadline: Option<Timestamp>,
        policy: FailurePolicy,
        currency: Currency,
    ) -> Result<Self> {
        Self::new(
            LegSpec {
                leg: opportunity.buy.clone(),
                deadline,
            },
            LegSpec {
                leg: opportunity.sell.clone(),
                deadline,
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
        &self.legs[leg.slot()]
    }

    /// Residual exposure: the sum of both legs' signed filled positions.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn residual(&self) -> Result<Decimal> {
        exact::add(self.legs[0].position(), self.legs[1].position())
    }

    /// Fees of both legs.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn fees(&self) -> Result<Cost> {
        self.legs[0].fees.checked_add(self.legs[1].fees)
    }

    /// Applies one observed event and returns the resulting intents.
    ///
    /// # Errors
    ///
    /// - [`ArbitrageError::InvalidInput`] for an event older than the last
    ///   applied one, a fill that would exceed the leg quantity, an event for
    ///   an attempt that was never sent, a reused execution id with
    ///   different content, or a rejection after fills of the same attempt.
    /// - [`ArbitrageError::CurrencyMismatch`] for a fee in another currency.
    /// - Arithmetic errors.
    pub fn apply(&mut self, event: &LegExecution) -> Result<PlanActions> {
        if self.is_duplicate(event)? {
            return self.report(Vec::new(), true);
        }
        let at = event.at();
        if at < self.last_at {
            return Err(ArbitrageError::InvalidInput {
                reason: "event is older than the last applied event",
            });
        }
        // Work on a copy so that an error leaves the plan unchanged.
        let mut next = self.clone();
        let mut actions = Vec::new();
        next.last_at = at;
        match event {
            LegExecution::Fill {
                leg,
                attempt,
                exec_id,
                quantity,
                price,
                fee,
                ..
            } => next.apply_fill(*leg, *attempt, exec_id, *quantity, *price, *fee)?,
            LegExecution::Rejected { leg, attempt, .. } => {
                next.close_attempt(*leg, *attempt, LegStatus::Rejected)?;
            }
            LegExecution::CancelAck { leg, attempt, .. } => {
                next.close_attempt(*leg, *attempt, LegStatus::Cancelled)?;
            }
            LegExecution::Tick { .. } => {}
        }
        next.step(at, &mut actions)?;
        *self = next;
        self.report(actions, false)
    }

    fn report(&self, actions: Vec<PlanAction>, duplicate: bool) -> Result<PlanActions> {
        Ok(PlanActions {
            actions,
            state: self.state,
            residual: self.residual()?,
            duplicate,
        })
    }

    fn is_duplicate(&self, event: &LegExecution) -> Result<bool> {
        match event {
            LegExecution::Fill {
                leg,
                attempt,
                exec_id,
                quantity,
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
                .closed_attempts
                .get(&(*leg, *attempt))
                .is_some_and(|s| *s == LegStatus::Rejected)),
            LegExecution::CancelAck { leg, attempt, .. } => Ok(self
                .closed_attempts
                .get(&(*leg, *attempt))
                .is_some_and(|s| *s == LegStatus::Cancelled)),
            LegExecution::Tick { .. } => Ok(false),
        }
    }

    fn check_attempt(&self, leg: LegIndex, attempt: u32) -> Result<()> {
        if attempt > self.legs[leg.slot()].resubmits {
            return Err(ArbitrageError::InvalidInput {
                reason: "event for an attempt that was never sent",
            });
        }
        Ok(())
    }

    fn apply_fill(
        &mut self,
        leg: LegIndex,
        attempt: u32,
        exec_id: &ExecId,
        quantity: Quantity,
        price: Price,
        fee: Cost,
    ) -> Result<()> {
        self.check_attempt(leg, attempt)?;
        if self.closed_attempts.get(&(leg, attempt)) == Some(&LegStatus::Rejected) {
            return Err(ArbitrageError::InvalidInput {
                reason: "fill for a rejected attempt",
            });
        }
        if fee.money().currency() != self.currency {
            return Err(ArbitrageError::CurrencyMismatch {
                left: self.currency,
                right: fee.money().currency(),
            });
        }
        let state = &mut self.legs[leg.slot()];
        let filled = exact::add(state.filled, quantity.value())?;
        if filled > state.spec.leg.quantity.value() {
            return Err(ArbitrageError::InvalidInput {
                reason: "fill exceeds the leg quantity",
            });
        }
        state.filled = filled;
        state.filled_notional = state
            .filled_notional
            .checked_add(price.notional(quantity, self.currency)?)?;
        state.fees = state.fees.checked_add(fee)?;
        if state.remaining()?.is_zero() {
            state.status = LegStatus::Filled;
        }
        self.fills.insert(
            exec_id.clone(),
            FillRecord {
                leg,
                attempt,
                quantity,
                price,
                fee,
            },
        );
        Ok(())
    }

    fn close_attempt(&mut self, leg: LegIndex, attempt: u32, outcome: LegStatus) -> Result<()> {
        self.check_attempt(leg, attempt)?;
        if outcome == LegStatus::Rejected
            && self
                .fills
                .values()
                .any(|f| f.leg == leg && f.attempt == attempt)
        {
            return Err(ArbitrageError::InvalidInput {
                reason: "rejection of an attempt that has fills",
            });
        }
        if self.closed_attempts.contains_key(&(leg, attempt)) {
            return Err(ArbitrageError::InvalidInput {
                reason: "attempt already closed with a different outcome",
            });
        }
        self.closed_attempts.insert((leg, attempt), outcome);
        let state = &mut self.legs[leg.slot()];
        // Only the current attempt moves the leg; a late acknowledgement of
        // an older attempt is recorded and changes nothing else.
        if state.attempt() == Some(attempt) {
            state.status = if state.remaining()?.is_zero() {
                LegStatus::Filled
            } else {
                outcome
            };
        }
        Ok(())
    }

    /// Re-evaluates deadlines, completion and the failure policy at `at`.
    fn step(&mut self, at: Timestamp, actions: &mut Vec<PlanAction>) -> Result<()> {
        // Deadlines request a cancel once per leg.
        for i in [LegIndex::First, LegIndex::Second] {
            let state = &mut self.legs[i.slot()];
            if let LegStatus::Working { attempt } = state.status {
                if !state.deadline_cancelled && state.spec.deadline.is_some_and(|d| d <= at) {
                    state.deadline_cancelled = true;
                    state.status = LegStatus::CancelRequested { attempt };
                    actions.push(PlanAction::Cancel { leg: i, attempt });
                }
            }
        }

        // Completion only from Working: once a recovery started, late fills
        // that complete both legs only change the residual.
        if self.state == PlanState::Working
            && self.legs.iter().all(|l| l.status == LegStatus::Filled)
        {
            self.state = PlanState::Completed;
            return Ok(());
        }

        if self.state == PlanState::Working {
            let failed = [LegIndex::First, LegIndex::Second].into_iter().find(|i| {
                matches!(
                    self.legs[i.slot()].status,
                    LegStatus::Rejected | LegStatus::Cancelled
                )
            });
            if let Some(leg) = failed {
                self.on_failure(leg, actions)?;
            }
        }

        let all_done = self.legs.iter().all(|l| l.status.is_terminal());
        match self.state {
            PlanState::Aborting if all_done => self.state = PlanState::Aborted,
            PlanState::Unwinding if all_done => {
                self.state = PlanState::UnwindRequested;
                self.request_unwind(actions)?;
            }
            // A late fill after the unwind was requested changes the
            // residual; request the difference.
            PlanState::UnwindRequested => self.request_unwind(actions)?,
            _ => {}
        }
        Ok(())
    }

    fn on_failure(&mut self, leg: LegIndex, actions: &mut Vec<PlanAction>) -> Result<()> {
        let fallback = match self.policy {
            FailurePolicy::Abort => Fallback::Abort,
            FailurePolicy::Unwind => Fallback::Unwind,
            FailurePolicy::Complete {
                max_resubmits,
                then,
            } => {
                let total: u32 = self.legs.iter().map(|l| l.resubmits).sum();
                let state = &self.legs[leg.slot()];
                let deadline_passed = state.deadline_cancelled;
                if total < max_resubmits && !deadline_passed {
                    let state = &mut self.legs[leg.slot()];
                    state.resubmits += 1;
                    let attempt = state.resubmits;
                    state.status = LegStatus::Working { attempt };
                    actions.push(PlanAction::Resubmit {
                        leg,
                        attempt,
                        quantity: Quantity::new(state.remaining()?)?,
                        price: state.spec.leg.price,
                    });
                    return Ok(());
                }
                then
            }
        };
        self.state = match fallback {
            Fallback::Abort => PlanState::Aborting,
            Fallback::Unwind => PlanState::Unwinding,
        };
        let other = leg.other();
        let state = &mut self.legs[other.slot()];
        if let LegStatus::Working { attempt } = state.status {
            state.status = LegStatus::CancelRequested { attempt };
            actions.push(PlanAction::Cancel {
                leg: other,
                attempt,
            });
        }
        Ok(())
    }

    fn request_unwind(&mut self, actions: &mut Vec<PlanAction>) -> Result<()> {
        let residual = self.residual()?;
        let open = exact::sub(residual, self.unwind_requested)?;
        if open.is_zero() {
            return Ok(());
        }
        // Offset on the leg whose position has the sign of the open residual.
        let source = if self.legs[0].position().is_sign_negative() == open.is_sign_negative()
            && !self.legs[0].position().is_zero()
        {
            &self.legs[0]
        } else {
            &self.legs[1]
        };
        let side = if open.is_sign_positive() {
            Side::Sell
        } else {
            Side::Buy
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
