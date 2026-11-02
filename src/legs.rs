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
//!   cumulative plus the fill, does not exceed the attempt's quantity and,
//!   once the attempt is acknowledged, does not exceed the acknowledged
//!   final quantity. Its timestamp may be older than the last event: a fill
//!   that was executed before a cancel but delivered after its
//!   acknowledgement is consistent and is applied;
//! - the **executed** quantity of an acknowledged attempt is the final
//!   quantity from the acknowledgement, even while some of its fills are
//!   still in flight; the residual exposure and every resubmission use it,
//!   so a late fill never makes the plan resubmit or unwind too much;
//! - fees and notional come from fills only, so they grow as late fills
//!   arrive.
//!
//! Rejections, acknowledgements and ticks must arrive with non-decreasing
//! timestamps; an older one is rejected.
//!
//! ## Units
//!
//! The two legs must have the same quantity and offset one to one in its
//! units (for example the same underlying on two venues). Other hedge ratios
//! must be converted by the caller.

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
    /// When to stop working the leg; reaching it requests a cancel. The
    /// instrument's expiry, if earlier, applies instead.
    pub deadline: Option<Timestamp>,
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
        /// Side that reduces the residual.
        side: Side,
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
    fn new(quantity: Quantity) -> Self {
        Self {
            quantity,
            observed: Decimal::ZERO,
            final_cum: None,
            outcome: None,
            cancel_requested: false,
        }
    }

    /// Executed quantity: the acknowledged final cumulative if known, else
    /// the fills observed so far.
    #[must_use]
    pub fn executed(&self) -> Decimal {
        self.final_cum.unwrap_or(self.observed)
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
    /// The whole quantity is executed.
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
    deadline_cancelled: bool,
}

impl LegState {
    /// Executed quantity over all attempts (see [`AttemptState::executed`]).
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn executed(&self) -> Result<Decimal> {
        self.attempts
            .iter()
            .try_fold(Decimal::ZERO, |acc, a| exact::add(acc, a.executed()))
    }

    /// Quantity left to execute.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn remaining(&self) -> Result<Decimal> {
        exact::sub(self.spec.leg.quantity.value(), self.executed()?)
    }

    /// Signed executed position: positive for a buy leg, negative for a
    /// sell.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn position(&self) -> Result<Decimal> {
        let executed = self.executed()?;
        Ok(match self.spec.leg.side {
            Side::Buy => executed,
            Side::Sell => exact::neg(executed),
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
        if self.remaining()?.is_zero() {
            return Ok(LegStatus::Filled);
        }
        let attempt = self.resubmits();
        let current = self.current();
        Ok(match (current.outcome, current.cancel_requested) {
            (Some(AttemptOutcome::Rejected), _) => LegStatus::Rejected,
            (Some(AttemptOutcome::Cancelled), _) => LegStatus::Cancelled,
            (None, true) => LegStatus::CancelRequested { attempt },
            (None, false) => LegStatus::Working { attempt },
        })
    }

    fn current(&self) -> &AttemptState {
        // `attempts` is never empty: attempt 0 is created with the leg.
        &self.attempts[self.attempts.len() - 1]
    }

    fn current_mut(&mut self) -> &mut AttemptState {
        let last = self.attempts.len() - 1;
        &mut self.attempts[last]
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
    last_at: Timestamp,
    fills: BTreeMap<ExecId, FillRecord>,
    unwind_requested: Decimal,
}

impl TwoLegPlan {
    /// A plan for two offsetting legs, created at `created_at`. Both legs
    /// start working as attempt 0.
    ///
    /// # Errors
    ///
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
            deadline_cancelled: false,
        };
        Ok(Self {
            currency,
            policy,
            legs: [leg_state(first), leg_state(second)],
            state: PlanState::Working,
            last_at: created_at,
            fills: BTreeMap::new(),
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

    /// Residual exposure: the sum of both legs' signed executed positions.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn residual(&self) -> Result<Decimal> {
        exact::add(self.legs[0].position()?, self.legs[1].position()?)
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
    /// The plan is unchanged when an error is returned.
    ///
    /// # Errors
    ///
    /// - [`ArbitrageError::InvalidInput`] for an attempt that was never sent,
    ///   an inconsistent `cum_qty` (not the previous cumulative plus the
    ///   fill, above the attempt quantity, or above or below what an
    ///   acknowledgement or the applied fills say), a reused execution id
    ///   with different content, a rejection of an attempt with fills, a
    ///   fill on a rejected attempt, an attempt closed twice differently, or
    ///   a non-fill event older than the last applied one.
    /// - [`ArbitrageError::CurrencyMismatch`] for a fee in another currency.
    /// - Arithmetic errors.
    pub fn apply(&mut self, event: &LegExecution) -> Result<PlanActions> {
        if self.is_duplicate(event)? {
            return self.report(Vec::new(), true);
        }
        let at = match event {
            LegExecution::Fill { at, .. } => (*at).max(self.last_at),
            LegExecution::Rejected { at, .. }
            | LegExecution::CancelAck { at, .. }
            | LegExecution::Tick { at } => {
                if *at < self.last_at {
                    return Err(ArbitrageError::InvalidInput {
                        reason: "event is older than the last applied event",
                    });
                }
                *at
            }
        };
        // Work on a copy and commit only if the whole step and its report
        // succeed, so that an error leaves the plan unchanged.
        let mut next = self.clone();
        let mut actions = Vec::new();
        next.last_at = at;
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
        }
        next.step(at, &mut actions)?;
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
        self.legs[leg.slot()]
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
            LegExecution::Tick { .. } => Ok(false),
        }
    }

    fn attempt_mut(&mut self, leg: LegIndex, attempt: u32) -> Result<&mut AttemptState> {
        self.legs[leg.slot()]
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
        if cum > state.quantity.value() {
            return invalid("cum_qty exceeds the attempt quantity");
        }
        if state.final_cum.is_some_and(|f| cum > f) {
            return invalid("cum_qty exceeds the acknowledged final quantity");
        }
        state.observed = cum;
        let notional = fill.price.notional(fill.quantity, self.currency)?;
        let leg_state = &mut self.legs[fill.leg.slot()];
        leg_state.filled_notional = leg_state.filled_notional.checked_add(notional)?;
        leg_state.fees = leg_state.fees.checked_add(fill.fee)?;
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
        if final_cum > state.quantity.value() {
            return invalid("acknowledged cum_qty exceeds the attempt quantity");
        }
        state.outcome = Some(outcome);
        state.final_cum = Some(final_cum.normalize());
        Ok(())
    }

    /// Re-evaluates deadlines, completion and the failure policy at `at`.
    fn step(&mut self, at: Timestamp, actions: &mut Vec<PlanAction>) -> Result<()> {
        // Effective deadlines (own deadline or instrument expiry) request a
        // cancel once per leg.
        for i in [LegIndex::First, LegIndex::Second] {
            let status = self.legs[i.slot()].status()?;
            let state = &mut self.legs[i.slot()];
            if let LegStatus::Working { attempt } = status {
                if !state.deadline_cancelled
                    && state.spec.effective_deadline().is_some_and(|d| d <= at)
                {
                    state.deadline_cancelled = true;
                    state.current_mut().cancel_requested = true;
                    actions.push(PlanAction::Cancel { leg: i, attempt });
                }
            }
        }

        let statuses = [self.legs[0].status()?, self.legs[1].status()?];
        // Completion only from Working: once a recovery started, executions
        // that complete both legs only change the residual.
        if self.state == PlanState::Working && statuses.iter().all(|s| *s == LegStatus::Filled) {
            self.state = PlanState::Completed;
            return Ok(());
        }

        if self.state == PlanState::Working {
            let failed = [LegIndex::First, LegIndex::Second].into_iter().find(|i| {
                matches!(
                    statuses[i.slot()],
                    LegStatus::Rejected | LegStatus::Cancelled
                )
            });
            if let Some(leg) = failed {
                self.on_failure(leg, actions)?;
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

    fn on_failure(&mut self, leg: LegIndex, actions: &mut Vec<PlanAction>) -> Result<()> {
        let fallback = match self.policy {
            FailurePolicy::Abort => Fallback::Abort,
            FailurePolicy::Unwind => Fallback::Unwind,
            FailurePolicy::Complete {
                max_resubmits,
                then,
            } => {
                let total: u32 = self.legs.iter().map(LegState::resubmits).sum();
                let state = &mut self.legs[leg.slot()];
                if total < max_resubmits && !state.deadline_cancelled {
                    // The executed quantity includes fills that the
                    // acknowledgement reported but that are still in flight,
                    // so the resubmission never overlaps them.
                    let quantity = Quantity::new(state.remaining()?)?;
                    state.attempts.push(AttemptState::new(quantity));
                    actions.push(PlanAction::Resubmit {
                        leg,
                        attempt: state.resubmits(),
                        quantity,
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
        if let LegStatus::Working { attempt } = self.legs[other.slot()].status()? {
            self.legs[other.slot()].current_mut().cancel_requested = true;
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
        let first = self.legs[0].position()?;
        let source = if !first.is_zero() && first.is_sign_negative() == open.is_sign_negative() {
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
