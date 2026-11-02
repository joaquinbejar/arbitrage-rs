//! One-leg band hedging.
//!
//! A position's exposure is kept inside a band `[lower, upper]`. While it is
//! inside (edges included), nothing is done. When it leaves the band, a single
//! hedge leg brings it back to a target inside the band, at the hedge
//! instrument's executable price.
//!
//! A [`HedgePlan`] is an intent with expected numbers: it reduces exposure if
//! it fills, it is not a profit, and it is not assumed to fill. When it cannot
//! be planned (no quote on the needed side, or less than one lot), the
//! decision says why and the exposure is left as it was.

use crate::cost::{Cost, FeeRate};
use crate::error::{ArbitrageError, Result};
use crate::exact;
use crate::instrument::{Instrument, Leg, Side, Timestamp, TopOfBook};
use crate::money::{Currency, Money};
use crate::units::Quantity;
use rust_decimal::Decimal;

/// Exposure band with a re-hedge target, in hedge instrument units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Band {
    lower: Decimal,
    target: Decimal,
    upper: Decimal,
}

impl Band {
    /// A band `[lower, upper]` that hedges back to `target`.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidValue`] unless `lower <= target <= upper`.
    pub fn new(lower: Decimal, target: Decimal, upper: Decimal) -> Result<Self> {
        if lower > upper {
            return Err(ArbitrageError::InvalidValue {
                field: "band lower",
                value: lower,
                reason: "must not be above upper",
            });
        }
        if target < lower || target > upper {
            return Err(ArbitrageError::InvalidValue {
                field: "band target",
                value: target,
                reason: "must be within [lower, upper]",
            });
        }
        Ok(Self {
            lower: lower.normalize(),
            target: target.normalize(),
            upper: upper.normalize(),
        })
    }

    /// A band `[-half_width, half_width]` around zero that hedges back to
    /// zero.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidValue`] if `half_width` is negative.
    pub fn symmetric(half_width: Decimal) -> Result<Self> {
        if half_width < Decimal::ZERO {
            return Err(ArbitrageError::InvalidValue {
                field: "band half width",
                value: half_width,
                reason: "must not be negative",
            });
        }
        Self::new(exact::neg(half_width), Decimal::ZERO, half_width)
    }

    /// Lower edge.
    #[must_use]
    pub fn lower(&self) -> Decimal {
        self.lower
    }

    /// Re-hedge target.
    #[must_use]
    pub fn target(&self) -> Decimal {
        self.target
    }

    /// Upper edge.
    #[must_use]
    pub fn upper(&self) -> Decimal {
        self.upper
    }

    /// True if `exposure` is inside the band, edges included.
    #[must_use]
    pub fn contains(&self, exposure: Decimal) -> bool {
        self.lower <= exposure && exposure <= self.upper
    }
}

/// Input to [`plan_band_hedge`]: a position's exposure and the hedge
/// instrument's market.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionView {
    /// Time of the exposure and quotes.
    pub as_of: Timestamp,
    /// Currency of prices and fees.
    pub quote_currency: Currency,
    /// Signed exposure in units of the hedge instrument's quantity (positive
    /// is long). The caller converts deltas or other instruments into these
    /// units.
    pub exposure: Decimal,
    /// The band.
    pub band: Band,
    /// Instrument the hedge trades.
    pub instrument: Instrument,
    /// Its top of book.
    pub book: TopOfBook,
    /// Taker fee on it.
    pub fee: FeeRate,
    /// Order size step; the hedge is rounded down to a whole number of lots.
    pub lot_size: Option<Quantity>,
    /// Largest hedge order allowed.
    pub max_quantity: Option<Quantity>,
}

/// A planned hedge leg with its expected effect and cost.
///
/// Every amount is an estimate at the quoted price; nothing here has been
/// executed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HedgePlan {
    /// Time of the inputs.
    pub as_of: Timestamp,
    /// The leg to send: instrument (with expiry), side, quantity and the
    /// executable price used for the estimate.
    pub leg: Leg,
    /// Exposure before the hedge.
    pub exposure_before: Decimal,
    /// Size needed to reach the band target.
    pub required_quantity: Quantity,
    /// Exposure if the leg fills in full at the quoted price. It differs from
    /// the target when the size was capped by the quoted size, the maximum
    /// order or the lot size.
    pub expected_exposure_after: Decimal,
    /// Fee on the leg's notional.
    pub expected_fee: Cost,
    /// Cost of crossing from mid to the executable price,
    /// `|price - mid| * quantity`; `None` when only one side is quoted.
    pub expected_spread_cost: Option<Cost>,
    /// `expected_fee` plus `expected_spread_cost` when known.
    pub expected_cost: Cost,
}

/// Why [`plan_band_hedge`] planned nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NoHedgeReason {
    /// The exposure is inside the band.
    InsideBand,
    /// The side the hedge needs is not quoted.
    NoLiquidity {
        /// The side the hedge would trade.
        side: Side,
    },
    /// The executable size is below one lot.
    BelowLotSize {
        /// Size that could have been traded before lot rounding.
        executable: Quantity,
        /// The lot size.
        lot_size: Quantity,
    },
}

/// Result of [`plan_band_hedge`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HedgeDecision {
    /// A hedge leg to send.
    Hedge(HedgePlan),
    /// Nothing to send. The exposure stays as it was.
    NoHedge {
        /// Why.
        reason: NoHedgeReason,
        /// The unchanged exposure.
        exposure: Decimal,
    },
}

impl HedgeDecision {
    /// The plan, if any.
    #[must_use]
    pub fn plan(&self) -> Option<&HedgePlan> {
        match self {
            Self::Hedge(p) => Some(p),
            Self::NoHedge { .. } => None,
        }
    }
}

/// Plans a single hedge leg that brings `view.exposure` back to the band
/// target when it is outside the band.
///
/// Above the band the hedge sells at the bid; below it buys at the ask. The
/// size is the distance to the target, capped by the quoted size and
/// `max_quantity`, then rounded down to whole lots, so the hedge never
/// overshoots the target.
///
/// # Errors
///
/// - [`ArbitrageError::Expired`] if the hedge instrument has expired at
///   `as_of`.
/// - Arithmetic errors ([`ArbitrageError::Overflow`],
///   [`ArbitrageError::Inexact`]); nothing is rounded except the explicit
///   lot rounding.
pub fn plan_band_hedge(view: &PositionView) -> Result<HedgeDecision> {
    view.instrument.ensure_live(view.as_of)?;
    let exposure = view.exposure.normalize();
    let no_hedge = |reason| Ok(HedgeDecision::NoHedge { reason, exposure });

    if view.band.contains(exposure) {
        return no_hedge(NoHedgeReason::InsideBand);
    }
    let side = if exposure > view.band.upper() {
        Side::Sell
    } else {
        Side::Buy
    };
    // Outside the band and target inside it, so the distance is positive.
    let required = Quantity::new(exact::sub(exposure, view.band.target())?.abs())?;

    let Some(level) = view.book.executable(side) else {
        return no_hedge(NoHedgeReason::NoLiquidity { side });
    };
    let mut executable = required.min(level.size);
    if let Some(max) = view.max_quantity {
        executable = executable.min(max);
    }
    let quantity = match view.lot_size {
        None => executable,
        Some(lot) => match executable.floor_to_lot(lot)? {
            Some(q) => q,
            None => {
                return no_hedge(NoHedgeReason::BelowLotSize {
                    executable,
                    lot_size: lot,
                });
            }
        },
    };

    let ccy = view.quote_currency;
    let notional = level.price.notional(quantity, ccy)?;
    let expected_fee = view.fee.fee_on(notional)?;
    let expected_spread_cost = spread_cost(&view.book, side, quantity, ccy)?;
    let expected_cost = match expected_spread_cost {
        Some(s) => expected_fee.checked_add(s)?,
        None => expected_fee,
    };
    let expected_exposure_after = match side {
        Side::Sell => exact::sub(exposure, quantity.value())?,
        Side::Buy => exact::add(exposure, quantity.value())?,
    };

    Ok(HedgeDecision::Hedge(HedgePlan {
        as_of: view.as_of,
        leg: Leg {
            instrument: view.instrument.clone(),
            side,
            quantity,
            price: level.price,
        },
        exposure_before: exposure,
        required_quantity: required,
        expected_exposure_after,
        expected_fee,
        expected_spread_cost,
        expected_cost,
    }))
}

/// `(ask - mid) * q` for a buy, `(mid - bid) * q` for a sell, when both sides
/// are quoted. Signed: a crossed book gives a negative cost.
fn spread_cost(book: &TopOfBook, side: Side, q: Quantity, ccy: Currency) -> Result<Option<Cost>> {
    let (Some(bid), Some(ask)) = (book.bid, book.ask) else {
        return Ok(None);
    };
    let half = Decimal::new(5, 1);
    let mid = exact::mul(exact::add(bid.price.value(), ask.price.value())?, half)?;
    let per_unit = match side {
        Side::Buy => exact::sub(ask.price.value(), mid)?,
        Side::Sell => exact::sub(mid, bid.price.value())?,
    };
    let amount = exact::mul(per_unit, q.value())?;
    Ok(Some(Cost::new(Money::new(amount, ccy))))
}
