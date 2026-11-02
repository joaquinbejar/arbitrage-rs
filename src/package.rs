//! Multi-leg package plan with partial fills, a residual per leg and
//! recovery.
//!
//! A [`PackagePlan`] executes a package: several legs that together hold
//! the same quantity of underlying, such as the call, the put and the
//! future of a put-call parity trade ([`crate::parity`]). Each leg trades in
//! contracts of its own size. It extends the semantics of
//! [`TwoLegPlan`](crate::legs::TwoLegPlan) to any number of legs whose
//! contract sizes differ:
//!
//! - every fill, rejection and cancel acknowledgement is applied as
//!   observed, with per-attempt cumulative quantities (`cum_qty`), and a
//!   repeated event changes nothing;
//! - a real execution is never rejected for exceeding a quantity: an
//!   overfill is recorded, a live attempt made surplus by it is cancelled,
//!   and once every leg is done the policy's fallback acts on the excess
//!   alone;
//! - a failed leg (rejected, or cancelled with quantity left) is resubmitted
//!   or not by the [`FailurePolicy`]; a leg's effective deadline is the
//!   earlier of its deadline and its instrument's expiry, and no
//!   resubmission is sent at or after it.
//!
//! The plan only emits intents ([`PackageAction`]); sending orders is the
//! caller's job. Venue views and connection state are not tracked: the legs
//! are assumed to trade on one reachable venue.
//!
//! ## Matched package and excess
//!
//! - The **grid** `g` is the least common multiple of the legs' lot
//!   exposures `lot * contract_size`, in underlying. With options of
//!   0.1 XYZ and futures of 0.01 XYZ, both with a lot of one contract,
//!   `g = 0.1` XYZ. The package must be a multiple of `g`.
//! - The **matched** package is the smallest executed exposure
//!   (`executed * contract_size`) over the legs, rounded down to a multiple
//!   of `g`, and at most the package: anything executed beyond the package
//!   is excess, even when every leg is overfilled alike. It is always a
//!   whole number of lots on every leg.
//! - The **excess** of a leg is its executed contracts minus the matched
//!   package in its contracts. It is what the fallback acts on: abort keeps
//!   it visible, unwind offsets it with a [`PackageAction::Unwind`] on that
//!   leg's instrument, in whole lots.
//!
//! Lots may be decimal (for example 0.1 of a spot unit). Fills must be whole
//! multiples of their leg's lot, so the excess is too.

/// Position of the call in a plan built by [`PackagePlan::from_parity`].
pub const CALL_LEG: usize = 0;
/// Position of the put in a plan built by [`PackagePlan::from_parity`].
pub const PUT_LEG: usize = 1;
/// Position of the future in a plan built by [`PackagePlan::from_parity`].
pub const FUTURE_LEG: usize = 2;

use crate::cost::Cost;
use crate::error::{ArbitrageError, Result};
use crate::exact;
use crate::instrument::{Action, Dir, Instrument, Leg, Timestamp};
use crate::legs::{
    AttemptOutcome, AttemptState, ExecId, FailurePolicy, Fallback, LegSpec, LegStatus, PlanState,
};
use crate::money::{Currency, Money};
use crate::parity::{ParityEdge, ParityInput};
use crate::units::{Price, Quantity};
use crate::venues::VenueId;
use crate::wide;
use rust_decimal::Decimal;
use std::collections::BTreeMap;

/// One leg of a package: what to trade, in contracts, and the size of its
/// contracts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PackageLegSpec {
    /// Instrument, side, quantity in contracts, limit price, deadline and
    /// venue.
    pub spec: LegSpec,
    /// Units of underlying per contract.
    pub contract_size: Quantity,
    /// Smallest tradable number of contracts.
    pub lot: Quantity,
}

/// An observed execution event for one leg, or the passage of time.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PackageEvent {
    /// A (partial) fill of an attempt.
    Fill {
        /// The leg, by its position in the plan.
        leg: usize,
        /// The order attempt it belongs to (0 for the first submission).
        attempt: u32,
        /// Venue execution id; a repeat with the same content is ignored.
        exec_id: ExecId,
        /// Filled contracts, a whole multiple of the leg's lot.
        quantity: Quantity,
        /// The attempt's cumulative executed contracts including this fill.
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
        leg: usize,
        /// The rejected attempt.
        attempt: u32,
        /// When it was observed.
        at: Timestamp,
    },
    /// The attempt's open quantity is cancelled (requested or not).
    CancelAck {
        /// The leg.
        leg: usize,
        /// The cancelled attempt.
        attempt: u32,
        /// The attempt's final cumulative executed contracts (zero or more,
        /// a whole multiple of the leg's lot).
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

impl PackageEvent {
    fn leg(&self) -> Option<usize> {
        match self {
            Self::Fill { leg, .. } | Self::Rejected { leg, .. } | Self::CancelAck { leg, .. } => {
                Some(*leg)
            }
            Self::Tick { .. } => None,
        }
    }

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
pub enum PackageAction {
    /// Cancel the open quantity of a leg's attempt.
    Cancel {
        /// The leg.
        leg: usize,
        /// The attempt to cancel.
        attempt: u32,
    },
    /// Send the leg again for its remaining contracts as a new attempt.
    Resubmit {
        /// The leg.
        leg: usize,
        /// The new attempt number.
        attempt: u32,
        /// Contracts still to execute.
        quantity: Quantity,
        /// Limit price (the plan's price for the leg).
        price: Price,
    },
    /// Send an order that offsets a leg's excess. It is an intent: the excess
    /// stays visible until fills are observed elsewhere.
    Unwind {
        /// The leg whose excess it offsets.
        leg: usize,
        /// Instrument to trade.
        instrument: Instrument,
        /// The action that reduces the excess.
        side: Action,
        /// Contracts to offset.
        quantity: Quantity,
    },
}

/// Running totals of one leg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageLegState {
    /// The leg as planned.
    pub spec: PackageLegSpec,
    /// Its attempts; the index is the attempt number.
    pub attempts: Vec<AttemptState>,
    /// Sum of `price * quantity * contract_size` over applied fills.
    pub filled_notional: Money,
    /// Sum of fees over applied fills.
    pub fees: Cost,
    grid_contracts: Decimal,
    deadline_cancelled: bool,
    last_at: Timestamp,
}

/// A leg without attempts: never built by this crate.
const NO_ATTEMPT: ArbitrageError = ArbitrageError::InvalidInput {
    reason: "a leg has no attempt",
};

/// A leg index outside the plan (events are checked on entry).
const UNKNOWN_LEG: ArbitrageError = ArbitrageError::InvalidInput {
    reason: "event for a leg that is not in the plan",
};

impl PackageLegState {
    /// Executed contracts over all attempts (see [`AttemptState::executed`]).
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn executed(&self) -> Result<Decimal> {
        let terms = self
            .attempts
            .iter()
            .map(|a| [a.executed()])
            .collect::<Vec<_>>();
        sum(&terms, "executed")
    }

    /// Contracts left to execute; negative after an overfill.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn remaining(&self) -> Result<Decimal> {
        let mut terms = vec![[Decimal::ONE, self.spec.spec.leg.quantity.value()]];
        terms.extend(
            self.attempts
                .iter()
                .map(|a| [Decimal::NEGATIVE_ONE, a.executed()]),
        );
        sum(&terms, "remaining")
    }

    /// Contracts executed beyond the leg's quantity (zero if none).
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn overfill(&self) -> Result<Decimal> {
        Ok(exact::neg(self.remaining()?).max(Decimal::ZERO))
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
            _ => LegStatus::Cancelled,
        })
    }

    fn past_deadline(&self, at: Timestamp) -> bool {
        self.deadline_cancelled || self.spec.spec.effective_deadline().is_some_and(|d| d <= at)
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

/// What [`PackagePlan::apply`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageReport {
    /// Intents to carry out, in order.
    pub actions: Vec<PackageAction>,
    /// The plan state after the event.
    pub state: PlanState,
    /// The matched package in underlying (a multiple of the grid).
    pub matched: Decimal,
    /// The excess of each leg in contracts, in leg order.
    pub excess: Vec<Decimal>,
    /// True if the event was a repeat and changed nothing.
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FillRecord {
    leg: usize,
    attempt: u32,
    quantity: Quantity,
    cum_qty: Quantity,
    price: Price,
    fee: Cost,
}

/// A package of legs driven by observed execution events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackagePlan {
    currency: Currency,
    policy: FailurePolicy,
    package: Quantity,
    grid: Quantity,
    package_grids: Decimal,
    legs: Vec<PackageLegState>,
    state: PlanState,
    clock: Timestamp,
    fills: BTreeMap<ExecId, FillRecord>,
    unwind_requested: Vec<Decimal>,
}

impl PackagePlan {
    /// A plan for `package` units of underlying over `legs`, created at
    /// `created_at`. Every leg starts working as attempt 0.
    ///
    /// # Errors
    ///
    /// - [`ArbitrageError::UnsupportedAction`] if a leg's side is not buy or
    ///   sell (checked first).
    /// - [`ArbitrageError::InvalidInput`] if there are fewer than two legs,
    ///   two legs are the same instrument, a leg's quantity is not a whole
    ///   number of lots or does not hold `package` of underlying, the package
    ///   is not a multiple of the grid, or a deadline is at or before
    ///   `created_at`.
    /// - [`ArbitrageError::Expired`] if a leg's instrument has expired at
    ///   `created_at`.
    /// - Arithmetic errors, including [`ArbitrageError::Overflow`] if the
    ///   grid does not fit.
    pub fn new(
        legs: Vec<PackageLegSpec>,
        package: Quantity,
        policy: FailurePolicy,
        currency: Currency,
        created_at: Timestamp,
    ) -> Result<Self> {
        for leg in &legs {
            Dir::try_from(leg.spec.leg.side)?;
        }
        let invalid = |reason| Err(ArbitrageError::InvalidInput { reason });
        if legs.len() < 2 {
            return invalid("a package needs at least two legs");
        }
        for (i, a) in legs.iter().enumerate() {
            if legs
                .iter()
                .take(i)
                .any(|b| b.spec.leg.instrument.id == a.spec.leg.instrument.id)
            {
                return invalid("two package legs are the same instrument");
            }
        }
        let exposures = legs
            .iter()
            .map(|l| exact::mul(l.lot.value(), l.contract_size.value()))
            .collect::<Result<Vec<_>>>()?;
        let grid = Quantity::new(decimal_lcm(&exposures)?)?;
        if !is_multiple(package, grid)? {
            return invalid("the package is not a multiple of the grid");
        }
        let mut states = Vec::with_capacity(legs.len());
        for l in legs {
            l.spec.leg.instrument.ensure_live(created_at)?;
            if l.spec.deadline.is_some_and(|d| d <= created_at) {
                return invalid("a leg deadline is not after the plan creation");
            }
            if !is_multiple(l.spec.leg.quantity, l.lot)? {
                return invalid("a leg quantity is not a whole number of lots");
            }
            if exact::mul(l.spec.leg.quantity.value(), l.contract_size.value())? != package.value()
            {
                return invalid("a leg does not hold the package of underlying");
            }
            // The leg's contracts per grid: a whole number of its lots,
            // decimal if the lot is.
            let grid_contracts = exact_quotient(grid.value(), l.contract_size.value())?;
            states.push(PackageLegState {
                attempts: vec![AttemptState::new(l.spec.leg.quantity)],
                spec: l,
                filled_notional: Money::zero(currency),
                fees: Cost::new(Money::zero(currency)),
                grid_contracts,
                deadline_cancelled: false,
                last_at: created_at,
            });
        }
        let n = states.len();
        Ok(Self {
            currency,
            policy,
            package,
            grid,
            package_grids: whole_quotient(package.value(), grid.value())?,
            legs: states,
            state: PlanState::Working,
            clock: created_at,
            fills: BTreeMap::new(),
            unwind_requested: vec![Decimal::ZERO; n],
        })
    }

    /// A plan for one package of a parity edge from
    /// [`evaluate_parity`](crate::parity::evaluate_parity):
    /// legs in the order [`CALL_LEG`], [`PUT_LEG`], [`FUTURE_LEG`], sized and
    /// priced as the edge, with the contract sizes and lots of `input`, all
    /// on `venue` and with `deadline`, created at the edge's `as_of`. A
    /// parity package trades on one venue: neither `input` nor `edge` names
    /// a venue, so the caller gives the one all three legs use.
    ///
    /// # Errors
    ///
    /// As [`PackagePlan::new`].
    pub fn from_parity(
        input: &ParityInput,
        edge: &ParityEdge,
        venue: &VenueId,
        deadline: Option<Timestamp>,
        policy: FailurePolicy,
    ) -> Result<Self> {
        let leg = |leg: &Leg, contract_size, lot| PackageLegSpec {
            spec: LegSpec {
                leg: leg.clone(),
                deadline,
                venue: venue.clone(),
            },
            contract_size,
            lot,
        };
        Self::new(
            vec![
                leg(&edge.legs.call, input.call.contract_size, input.call.lot),
                leg(&edge.legs.put, input.put.contract_size, input.put.lot),
                leg(
                    &edge.legs.future,
                    input.future.contract_size,
                    input.future.lot,
                ),
            ],
            input.package,
            policy,
            input.quote_currency,
            edge.as_of,
        )
    }

    /// The plan state.
    #[must_use]
    pub fn state(&self) -> PlanState {
        self.state
    }

    /// The package in underlying.
    #[must_use]
    pub fn package(&self) -> Quantity {
        self.package
    }

    /// The grid in underlying: the least common multiple of the legs' lot
    /// exposures.
    #[must_use]
    pub fn grid(&self) -> Quantity {
        self.grid
    }

    /// The legs, in plan order.
    #[must_use]
    pub fn legs(&self) -> &[PackageLegState] {
        &self.legs
    }

    /// The matched package in underlying: the smallest executed exposure
    /// over the legs, rounded down to a multiple of the grid.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn matched(&self) -> Result<Decimal> {
        exact::mul(self.matched_grids()?, self.grid.value())
    }

    /// Each leg's excess in contracts, in leg order: executed contracts
    /// minus the matched package in that leg's contracts.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn excess(&self) -> Result<Vec<Decimal>> {
        let n = self.matched_grids()?;
        self.legs
            .iter()
            .map(|l| {
                let mut terms = vec![vec![Decimal::NEGATIVE_ONE, n, l.grid_contracts]];
                terms.extend(l.attempts.iter().map(|a| vec![a.executed()]));
                sum(&terms, "excess")
            })
            .collect()
    }

    /// Fees of every leg.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn fees(&self) -> Result<Cost> {
        let terms = self
            .legs
            .iter()
            .map(|l| [l.fees.money().amount()])
            .collect::<Vec<_>>();
        Ok(Cost::new(Money::new(sum(&terms, "fees")?, self.currency)))
    }

    /// Cash flow of the fills so far: sell notional minus buy notional minus
    /// fees.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn net_cash(&self) -> Result<Money> {
        let mut terms = Vec::with_capacity(self.legs.len().saturating_mul(2));
        for leg in &self.legs {
            let sign = match Dir::try_from(leg.spec.spec.leg.side)? {
                Dir::Sell => Decimal::ONE,
                Dir::Buy => Decimal::NEGATIVE_ONE,
            };
            terms.push([sign, leg.filled_notional.amount()]);
            terms.push([Decimal::NEGATIVE_ONE, leg.fees.money().amount()]);
        }
        Ok(Money::new(sum(&terms, "net cash")?, self.currency))
    }

    /// Applies one observed event and returns the resulting intents.
    ///
    /// The plan is unchanged when an error is returned.
    ///
    /// # Errors
    ///
    /// - [`ArbitrageError::InvalidInput`] for a leg or attempt that does not
    ///   exist, a quantity that is not a whole number of the leg's lots, an
    ///   inconsistent `cum_qty`, a reused execution id with different
    ///   content, a rejection of an attempt with fills, a fill on a rejected
    ///   attempt, an attempt closed twice differently, a non-fill event older
    ///   than the last non-fill event of its leg, or a tick older than the
    ///   plan clock. Executions beyond a quantity are not errors.
    /// - [`ArbitrageError::CurrencyMismatch`] for a fee in another currency.
    /// - Arithmetic errors.
    pub fn apply(&mut self, event: &PackageEvent) -> Result<PackageReport> {
        if let Some(leg) = event.leg()
            && leg >= self.legs.len()
        {
            return Err(ArbitrageError::InvalidInput {
                reason: "event for a leg that is not in the plan",
            });
        }
        if self.is_duplicate(event)? {
            return self.report(Vec::new(), true);
        }
        let at = event.at();
        match (event, event.leg()) {
            (PackageEvent::Fill { .. }, _) => {}
            (_, Some(leg)) => {
                if at < self.legs.get(leg).ok_or(UNKNOWN_LEG)?.last_at {
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
        match event {
            PackageEvent::Fill {
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
            PackageEvent::Rejected { leg, attempt, .. } => {
                next.legs.get_mut(*leg).ok_or(UNKNOWN_LEG)?.last_at = at;
                next.close_attempt(*leg, *attempt, AttemptOutcome::Rejected, Decimal::ZERO)?;
            }
            PackageEvent::CancelAck {
                leg,
                attempt,
                cum_qty,
                ..
            } => {
                next.legs.get_mut(*leg).ok_or(UNKNOWN_LEG)?.last_at = at;
                next.close_attempt(*leg, *attempt, AttemptOutcome::Cancelled, *cum_qty)?;
            }
            PackageEvent::Tick { .. } => {}
        }
        next.step(now, &mut actions)?;
        let report = next.report(actions, false)?;
        *self = next;
        Ok(report)
    }

    /// Whole grids matched: the smallest, over the legs, of the executed
    /// contracts divided by the leg's contracts per grid, rounded down, and
    /// at most the package's grids.
    fn matched_grids(&self) -> Result<Decimal> {
        let mut n = self.package_grids;
        for l in &self.legs {
            let executed = l.executed()?;
            let grids = if executed <= Decimal::ZERO {
                Decimal::ZERO
            } else {
                let lot = Quantity::new(l.grid_contracts)?;
                match Quantity::new(executed)?.floor_to_lot(lot)? {
                    Some(floored) => whole_quotient(floored.value(), l.grid_contracts)?,
                    None => Decimal::ZERO,
                }
            };
            n = n.min(grids);
        }
        Ok(n)
    }

    fn report(&self, actions: Vec<PackageAction>, duplicate: bool) -> Result<PackageReport> {
        Ok(PackageReport {
            actions,
            state: self.state,
            matched: self.matched()?,
            excess: self.excess()?,
            duplicate,
        })
    }

    fn closed_as(&self, leg: usize, attempt: u32) -> Option<(AttemptOutcome, Option<Decimal>)> {
        self.legs
            .get(leg)?
            .attempts
            .get(attempt as usize)
            .and_then(|a| a.outcome.map(|o| (o, a.final_cum)))
    }

    fn is_duplicate(&self, event: &PackageEvent) -> Result<bool> {
        match event {
            PackageEvent::Fill {
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
            PackageEvent::Rejected { leg, attempt, .. } => Ok(self
                .closed_as(*leg, *attempt)
                .is_some_and(|(o, _)| o == AttemptOutcome::Rejected)),
            PackageEvent::CancelAck {
                leg,
                attempt,
                cum_qty,
                ..
            } => Ok(self.closed_as(*leg, *attempt).is_some_and(|(o, f)| {
                o == AttemptOutcome::Cancelled && f == Some(cum_qty.normalize())
            })),
            PackageEvent::Tick { .. } => Ok(false),
        }
    }

    fn attempt_mut(&mut self, leg: usize, attempt: u32) -> Result<&mut AttemptState> {
        self.legs
            .get_mut(leg)
            .ok_or(UNKNOWN_LEG)?
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
        let lot = self.legs.get(fill.leg).ok_or(UNKNOWN_LEG)?.spec.lot;
        if !is_multiple(fill.quantity, lot)? || !is_multiple(fill.cum_qty, lot)? {
            return invalid("a fill is not a whole number of the leg's lots");
        }
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
        let leg_state = self.legs.get_mut(fill.leg).ok_or(UNKNOWN_LEG)?;
        // Prices are per unit of underlying and quantities in contracts: the
        // notional is price * contracts * contract_size, added in one exact
        // sum.
        let notional = wide::sum_products_exact(
            &[
                &[leg_state.filled_notional.amount()],
                &[
                    fill.price.value(),
                    fill.quantity.value(),
                    leg_state.spec.contract_size.value(),
                ],
            ],
            "filled notional",
        )?;
        leg_state.filled_notional = Money::new(notional, self.currency);
        leg_state.fees = leg_state.fees.checked_add(fill.fee)?;
        self.fills.insert(exec_id.clone(), fill);
        Ok(())
    }

    fn close_attempt(
        &mut self,
        leg: usize,
        attempt: u32,
        outcome: AttemptOutcome,
        final_cum: Decimal,
    ) -> Result<()> {
        let invalid = |reason| Err(ArbitrageError::InvalidInput { reason });
        if final_cum < Decimal::ZERO {
            return invalid("acknowledged cum_qty is negative");
        }
        if final_cum > Decimal::ZERO
            && !is_multiple(
                Quantity::new(final_cum)?,
                self.legs.get(leg).ok_or(UNKNOWN_LEG)?.spec.lot,
            )?
        {
            return invalid("acknowledged cum_qty is not a whole number of the leg's lots");
        }
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
        state.outcome = Some(outcome);
        state.final_cum = Some(final_cum.normalize());
        Ok(())
    }

    /// Re-evaluates deadlines, surplus attempts, overfills, completion and
    /// the failure policy at `at`.
    fn step(&mut self, at: Timestamp, actions: &mut Vec<PackageAction>) -> Result<()> {
        for i in 0..self.legs.len() {
            let state = self.legs.get_mut(i).ok_or(UNKNOWN_LEG)?;
            let remaining = state.remaining()?;
            let attempt = state.resubmits();
            if !state.current()?.is_live() || state.current()?.cancel_requested {
                continue;
            }
            // A live attempt made surplus by executions elsewhere (a late
            // fill beyond an acknowledgement) is cancelled.
            let surplus = remaining <= Decimal::ZERO;
            let expired = !state.deadline_cancelled
                && state
                    .spec
                    .spec
                    .effective_deadline()
                    .is_some_and(|d| d <= at);
            if expired {
                state.deadline_cancelled = true;
            }
            if surplus || expired {
                state.current_mut()?.cancel_requested = true;
                actions.push(PackageAction::Cancel { leg: i, attempt });
            }
        }

        // An overfill is recorded as exposure. Once every leg is done, the
        // policy's fallback applies to the excess alone.
        let statuses = self
            .legs
            .iter()
            .map(PackageLegState::status)
            .collect::<Result<Vec<_>>>()?;
        let all_filled = statuses.iter().all(|s| *s == LegStatus::Filled);
        if all_filled && matches!(self.state, PlanState::Working | PlanState::Completed) {
            if self.excess()?.iter().all(Decimal::is_zero) {
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

        for (i, status) in statuses.iter().enumerate() {
            if self.state != PlanState::Working {
                break;
            }
            if matches!(status, LegStatus::Rejected | LegStatus::Cancelled) {
                self.on_failure(i, at, actions)?;
            }
        }

        let mut all_done = true;
        for l in &self.legs {
            all_done &= l.status()?.is_terminal();
        }
        match self.state {
            PlanState::Aborting if all_done => self.state = PlanState::Aborted,
            PlanState::Unwinding if all_done => {
                self.state = PlanState::UnwindRequested;
                self.request_unwind(actions)?;
            }
            // Executions after the unwind was requested change the excess;
            // request the difference.
            PlanState::UnwindRequested => self.request_unwind(actions)?,
            _ => {}
        }
        Ok(())
    }

    fn fallback(&self) -> Fallback {
        match self.policy {
            FailurePolicy::Abort => Fallback::Abort,
            FailurePolicy::Unwind => Fallback::Unwind,
            FailurePolicy::Complete { then, .. } => then,
        }
    }

    /// Enters the fallback and cancels every working attempt.
    fn enter_fallback(&mut self, actions: &mut Vec<PackageAction>) -> Result<()> {
        self.state = match self.fallback() {
            Fallback::Abort => PlanState::Aborting,
            Fallback::Unwind => PlanState::Unwinding,
        };
        for (i, state) in self.legs.iter_mut().enumerate() {
            if let LegStatus::Working { attempt } = state.status()? {
                state.current_mut()?.cancel_requested = true;
                actions.push(PackageAction::Cancel { leg: i, attempt });
            }
        }
        Ok(())
    }

    fn on_failure(
        &mut self,
        leg: usize,
        at: Timestamp,
        actions: &mut Vec<PackageAction>,
    ) -> Result<()> {
        if let FailurePolicy::Complete { max_resubmits, .. } = self.policy {
            let total: u32 = self.legs.iter().map(PackageLegState::resubmits).sum();
            // Checked against the event time itself: a rejection or an
            // acknowledgement at or after the effective deadline never
            // resubmits, with or without an earlier tick.
            let state = self.legs.get_mut(leg).ok_or(UNKNOWN_LEG)?;
            if total < max_resubmits && !state.past_deadline(at) {
                let quantity = Quantity::new(state.remaining()?)?;
                state.attempts.push(AttemptState::new(quantity));
                actions.push(PackageAction::Resubmit {
                    leg,
                    attempt: state.resubmits(),
                    quantity,
                    price: state.spec.spec.leg.price,
                });
                return Ok(());
            }
        }
        self.enter_fallback(actions)
    }

    fn request_unwind(&mut self, actions: &mut Vec<PackageAction>) -> Result<()> {
        let excess = self.excess()?;
        let pending = excess.iter().zip(self.unwind_requested.iter_mut());
        for ((i, leg), (excess, requested)) in self.legs.iter().enumerate().zip(pending) {
            let signed = match Dir::try_from(leg.spec.spec.leg.side)? {
                Dir::Buy => *excess,
                Dir::Sell => exact::neg(*excess),
            };
            let open = exact::sub(signed, *requested)?;
            if open.is_zero() {
                continue;
            }
            actions.push(PackageAction::Unwind {
                leg: i,
                instrument: leg.spec.spec.leg.instrument.clone(),
                side: if open.is_sign_positive() {
                    Action::Sell
                } else {
                    Action::Buy
                },
                quantity: Quantity::new(open.abs())?,
            });
            *requested = signed;
        }
        Ok(())
    }
}

/// The exact sum of products `terms`, converted once.
fn sum<T: AsRef<[Decimal]>>(terms: &[T], op: &'static str) -> Result<Decimal> {
    let refs = terms.iter().map(AsRef::as_ref).collect::<Vec<_>>();
    wide::sum_products_exact(&refs, op)
}

/// True if `q` is a whole multiple of `unit`.
fn is_multiple(q: Quantity, unit: Quantity) -> Result<bool> {
    Ok(q.floor_to_lot(unit)? == Some(q))
}

/// `a / b` when the quotient is exact (a `Decimal` whose product with `b`
/// is `a`), or `InvalidInput`.
fn exact_quotient(a: Decimal, b: Decimal) -> Result<Decimal> {
    let q = a
        .checked_div(b)
        .ok_or(ArbitrageError::Overflow { op: "div" })?
        .normalize();
    match exact::mul(q, b) {
        Ok(back) if back == a => Ok(q),
        _ => Err(ArbitrageError::InvalidInput {
            reason: "a quantity is not an exact multiple of its unit",
        }),
    }
}

/// `a / b` when it is a whole number, or `InvalidInput`.
fn whole_quotient(a: Decimal, b: Decimal) -> Result<Decimal> {
    let not_whole = ArbitrageError::InvalidInput {
        reason: "a quantity is not a whole multiple of its unit",
    };
    let q = a
        .checked_div(b)
        .ok_or(ArbitrageError::Overflow { op: "div" })?
        .normalize();
    // A fractional quotient is rejected before the product, which could
    // otherwise not fit at the quotient's scale.
    if !q.fract().is_zero() || exact::mul(q, b)? != a {
        return Err(not_whole);
    }
    Ok(q)
}

/// Least common multiple of positive decimals: the values are scaled to
/// integers at their largest scale, and the integer LCM is scaled back.
fn decimal_lcm(values: &[Decimal]) -> Result<Decimal> {
    let overflow = ArbitrageError::Overflow { op: "lcm" };
    let scale = values
        .iter()
        .map(|v| v.normalize().scale())
        .max()
        .unwrap_or(0);
    let mut lcm: u128 = 1;
    for v in values {
        let v = v.normalize();
        let m = v.mantissa().unsigned_abs();
        let n = scale
            .checked_sub(v.scale())
            .and_then(|k| 10u128.checked_pow(k))
            .and_then(|p| m.checked_mul(p))
            .ok_or(overflow.clone())?;
        // A zero value has no positive multiple: the division fails.
        lcm = lcm
            .checked_div(gcd(lcm, n))
            .and_then(|l| l.checked_mul(n))
            .ok_or(overflow.clone())?;
    }
    let lcm = i128::try_from(lcm).map_err(|_| overflow.clone())?;
    Decimal::try_from_i128_with_scale(lcm, scale)
        .map(|d| d.normalize())
        .map_err(|_| overflow)
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        // b != 0 in the loop.
        (a, b) = (b, a.checked_rem(b).unwrap_or(0));
    }
    a
}

#[cfg(test)]
// Tests fail loudly: the library's no-panic lints do not apply here.
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn lcm_of_lot_exposures() {
        assert_eq!(
            decimal_lcm(&[dec!(0.1), dec!(0.1), dec!(0.01)]).unwrap(),
            dec!(0.1)
        );
        assert_eq!(decimal_lcm(&[dec!(0.3), dec!(0.02)]).unwrap(), dec!(0.3));
        assert_eq!(decimal_lcm(&[dec!(0.4), dec!(0.06)]).unwrap(), dec!(1.2));
        assert_eq!(decimal_lcm(&[dec!(1.5), dec!(2)]).unwrap(), dec!(6));
        assert_eq!(decimal_lcm(&[dec!(0.001)]).unwrap(), dec!(0.001));
    }

    #[test]
    fn whole_quotients() {
        assert_eq!(whole_quotient(dec!(0.1), dec!(0.01)).unwrap(), dec!(10));
        assert!(whole_quotient(dec!(1), dec!(0.3)).is_err());
    }
}
