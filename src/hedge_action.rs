//! Bounded conversion of external hedge actions into hedge plans.
//!
//! A hedging policy outside this crate (a learned policy, a rule engine or
//! an operator) proposes a [`HedgeAction`]: an instrument, a side and a
//! quantity. [`hedge_plan_from_action`] turns it into the same
//! [`HedgePlan`] the band hedger produces, after validating and bounding it:
//!
//! - a quantity given as a float must be finite and within `Decimal`'s
//!   range. Its exact binary value is rounded once, half to even, to
//!   [`HedgeLimits::float_dp`] decimal places (so `12.34` stays `12.34`);
//!   the conversion is deterministic and moves the value by at most half a
//!   unit in that place, before any bound below;
//! - the quantity must not be negative, and zero plans nothing;
//! - it is bounded by [`HedgeLimits::max_quantity`] (clipped or rejected, as
//!   configured), by the size quoted on the side the hedge trades and by the
//!   lot size, so no action can produce an unbounded plan;
//! - the requested and the feasible quantity, and every adjustment between
//!   them, are kept for audit ([`ActionPlan`]).
//!
//! The conversion is pure: it posts no cashflow and does not claim that
//! anything executed. The resulting plan is an intent that still needs
//! capital admission and execution checks.

use crate::cost::FeeRate;
use crate::error::{ArbitrageError, Result};
use crate::exact;
use crate::hedge::{HedgePlan, spread_cost};
use crate::instrument::{Instrument, Leg, Side, Timestamp, TopOfBook};
use crate::money::Currency;
use crate::units::Quantity;
use rust_decimal::{Decimal, RoundingStrategy};

/// The quantity a policy proposes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ActionQuantity {
    /// An exact decimal quantity.
    Exact(Decimal),
    /// A floating-point output, for example from a learned policy. It must
    /// be finite and within `Decimal`'s range.
    Float(f64),
}

/// A hedge proposed by an external policy.
#[derive(Debug, Clone, PartialEq)]
pub struct HedgeAction {
    /// When the action was proposed.
    pub as_of: Timestamp,
    /// Instrument to trade, with its expiry if dated.
    pub instrument: Instrument,
    /// Direction.
    pub side: Side,
    /// Proposed quantity, in the instrument's units.
    pub quantity: ActionQuantity,
}

/// What to do with a quantity above the maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExcessPolicy {
    /// Clip it to the maximum and record the adjustment.
    Clip,
    /// Reject the action with an error.
    Reject,
}

/// Bounds every action must respect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HedgeLimits {
    /// Largest quantity a plan may have.
    pub max_quantity: Quantity,
    /// Order size step; the plan is rounded down to whole lots.
    pub lot_size: Option<Quantity>,
    /// Decimal places a float quantity is rounded to, half to even (at most
    /// 28).
    pub float_dp: u32,
    /// What to do with a quantity above `max_quantity`.
    pub on_excess: ExcessPolicy,
}

/// The market and position the action is priced against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionMarket {
    /// Currency of prices and fees.
    pub quote_currency: Currency,
    /// The instrument's top of book.
    pub book: TopOfBook,
    /// Taker fee on the instrument.
    pub fee: FeeRate,
    /// Current signed exposure in the instrument's units (positive is
    /// long); recorded before and after the plan.
    pub exposure: Decimal,
}

/// A step between the requested and the feasible quantity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Adjustment {
    /// A float quantity rounded to `float_dp` places from its exact binary
    /// value.
    FloatRounded,
    /// Clipped to `max_quantity`.
    MaxQuantity,
    /// Capped at the size quoted on the side the hedge trades.
    QuotedSize,
    /// Rounded down to whole lots.
    Lot,
}

/// A plan built from an action, with its audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActionPlan {
    /// The quantity the policy asked for, after float conversion.
    pub requested: Decimal,
    /// The quantity of the plan.
    pub feasible: Quantity,
    /// The adjustments between them, in the order applied.
    pub adjustments: Vec<Adjustment>,
    /// The plan: an intent, not an execution.
    pub plan: HedgePlan,
}

/// Why no plan was built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NoPlanReason {
    /// The action asks for zero.
    ZeroQuantity,
    /// The side the hedge trades is not quoted.
    NoLiquidity,
    /// The bounded quantity is below one lot.
    BelowLotSize,
}

/// Result of [`hedge_plan_from_action`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ActionOutcome {
    /// A bounded plan (boxed: the plan is large).
    Plan(Box<ActionPlan>),
    /// No plan; the exposure is unchanged.
    NoPlan {
        /// The quantity the policy asked for.
        requested: Decimal,
        /// Why.
        reason: NoPlanReason,
    },
}

impl ActionOutcome {
    /// The plan, if any.
    #[must_use]
    pub fn plan(&self) -> Option<&ActionPlan> {
        match self {
            Self::Plan(p) => Some(p.as_ref()),
            Self::NoPlan { .. } => None,
        }
    }
}

/// Validates and bounds `action` and builds the hedge plan for it.
///
/// # Errors
///
/// - [`ArbitrageError::InvalidInput`] for a float quantity that is not
///   finite or not representable as a `Decimal`, or `float_dp` above 28.
/// - [`ArbitrageError::InvalidValue`] for a negative quantity, or one above
///   `max_quantity` under [`ExcessPolicy::Reject`].
/// - [`ArbitrageError::Expired`] if the instrument has expired at `as_of`.
/// - Arithmetic errors; nothing is rounded except the explicit float
///   rounding and lot rounding.
pub fn hedge_plan_from_action(
    action: &HedgeAction,
    limits: &HedgeLimits,
    market: &ActionMarket,
) -> Result<ActionOutcome> {
    action.instrument.ensure_live(action.as_of)?;
    let mut adjustments = Vec::new();
    let requested = requested_quantity(action.quantity, limits.float_dp, &mut adjustments)?;
    if requested < Decimal::ZERO {
        return Err(ArbitrageError::InvalidValue {
            field: "action quantity",
            value: requested,
            reason: "must not be negative; the side gives the direction",
        });
    }
    if requested.is_zero() {
        return Ok(ActionOutcome::NoPlan {
            requested,
            reason: NoPlanReason::ZeroQuantity,
        });
    }

    let mut quantity = Quantity::new(requested)?;
    if quantity > limits.max_quantity {
        match limits.on_excess {
            ExcessPolicy::Clip => {
                quantity = limits.max_quantity;
                adjustments.push(Adjustment::MaxQuantity);
            }
            ExcessPolicy::Reject => {
                return Err(ArbitrageError::InvalidValue {
                    field: "action quantity",
                    value: requested,
                    reason: "above max_quantity",
                });
            }
        }
    }

    let Some(level) = market.book.executable(action.side) else {
        return Ok(ActionOutcome::NoPlan {
            requested,
            reason: NoPlanReason::NoLiquidity,
        });
    };
    if level.size < quantity {
        quantity = level.size;
        adjustments.push(Adjustment::QuotedSize);
    }
    if let Some(lot) = limits.lot_size {
        match quantity.floor_to_lot(lot)? {
            None => {
                return Ok(ActionOutcome::NoPlan {
                    requested,
                    reason: NoPlanReason::BelowLotSize,
                });
            }
            Some(q) => {
                if q != quantity {
                    adjustments.push(Adjustment::Lot);
                }
                quantity = q;
            }
        }
    }

    let ccy = market.quote_currency;
    let notional = level.price.notional(quantity, ccy)?;
    let expected_fee = market.fee.fee_on(notional)?;
    let expected_spread_cost = spread_cost(&market.book, action.side, quantity, ccy)?;
    let expected_cost = match expected_spread_cost {
        Some(s) => expected_fee.checked_add(s)?,
        None => expected_fee,
    };
    let exposure = market.exposure.normalize();
    let expected_exposure_after = match action.side {
        Side::Buy => exact::add(exposure, quantity.value())?,
        Side::Sell => exact::sub(exposure, quantity.value())?,
    };
    Ok(ActionOutcome::Plan(Box::new(ActionPlan {
        requested,
        feasible: quantity,
        adjustments,
        plan: HedgePlan {
            as_of: action.as_of,
            leg: Leg {
                instrument: action.instrument.clone(),
                side: action.side,
                quantity,
                price: level.price,
            },
            exposure_before: exposure,
            required_quantity: Quantity::new(requested)?,
            expected_exposure_after,
            expected_fee,
            expected_spread_cost,
            expected_cost,
        },
    })))
}

fn requested_quantity(
    quantity: ActionQuantity,
    float_dp: u32,
    adjustments: &mut Vec<Adjustment>,
) -> Result<Decimal> {
    match quantity {
        ActionQuantity::Exact(d) => Ok(d.normalize()),
        ActionQuantity::Float(x) => {
            if !x.is_finite() {
                return Err(ArbitrageError::InvalidInput {
                    reason: "action quantity is not finite",
                });
            }
            if float_dp > exact::MAX_SCALE {
                return Err(ArbitrageError::InvalidInput {
                    reason: "float_dp must be at most 28",
                });
            }
            // The float's exact binary value (to Decimal's 28 digits), then
            // one explicit rounding to float_dp places.
            let d = Decimal::from_f64_retain(x).ok_or(ArbitrageError::InvalidInput {
                reason: "action quantity is outside the supported Decimal range",
            })?;
            let rounded = d
                .round_dp_with_strategy(float_dp, RoundingStrategy::MidpointNearestEven)
                .normalize();
            if rounded != d.normalize() {
                adjustments.push(Adjustment::FloatRounded);
            }
            Ok(rounded)
        }
    }
}
