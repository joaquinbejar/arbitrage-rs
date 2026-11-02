//! Net spot-perpetual basis opportunities.
//!
//! Two directions:
//!
//! - [`BasisDirection::LongSpotShortPerp`]: buy spot at its ask and sell the
//!   perpetual at its bid. The entry basis is `perp bid - spot ask`; the
//!   position earns `entry basis - exit basis` per unit and receives funding
//!   while the rate is positive.
//! - [`BasisDirection::ShortSpotLongPerp`]: sell spot at its bid (borrowing
//!   it) and buy the perpetual at its ask. The entry basis is `perp ask - spot
//!   bid`; the position earns `exit basis - entry basis` per unit and
//!   receives funding while the rate is negative.
//!
//! A perpetual does not converge to spot, so the exit basis is an explicit
//! assumption of the caller, as are the funding rates and marks of future
//! funding instants. Costs subtracted from the gross (convergence) edge:
//! entry and exit fees on both legs, carry items (borrow, financing) and
//! funding over the holding window `(as_of, exit_time]`.
//!
//! The decision uses the net edge only. A zero or negative convergence edge
//! is not a veto: a flat basis held while funding is received is the main
//! spot-perpetual trade, and it is accepted when the net edge clears the
//! threshold.
//!
//! Exit fees use the implied exit notionals: spot at its entry price (only
//! the basis is assumed to move) and the perpetual at `spot entry price +
//! expected_exit_basis`, at the same fee rates.
//!
//! As in [`crate::opportunity`], an accepted opportunity is an intent with
//! expected numbers, not an executed or guaranteed profit.

use crate::cost::Cost;
use crate::error::{ArbitrageError, Result};
use crate::exact;
use crate::funding::{
    FundingEvent, FundingSchedule, PerpConvention, RateKind, SettlementRounding, funding_cashflow,
    funding_cashflow_rounded,
};
use crate::instrument::{Action, Dir, Instrument, Leg, Timestamp};
use crate::money::{Currency, Money};
use crate::opportunity::{Carry, CarryCost, LegQuote};
use crate::units::{Price, Quantity};
use crate::wide;
use rust_decimal::Decimal;

/// Which way the basis is traded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BasisDirection {
    /// Buy spot, sell the perpetual (cash and carry).
    LongSpotShortPerp,
    /// Sell spot, buy the perpetual (reverse cash and carry).
    ShortSpotLongPerp,
}

impl BasisDirection {
    /// The side the spot leg trades at entry.
    #[must_use]
    pub fn spot_side(self) -> Action {
        match self {
            Self::LongSpotShortPerp => Action::Buy,
            Self::ShortSpotLongPerp => Action::Sell,
        }
    }

    /// The side the perpetual leg trades at entry.
    #[must_use]
    pub fn perp_side(self) -> Action {
        self.spot_side().opposite()
    }
}

/// Input to [`evaluate_spot_perp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasisInput {
    /// Decision time: quotes, mark and index are as of this instant.
    pub as_of: Timestamp,
    /// Currency of prices, fees and funding.
    pub quote_currency: Currency,
    /// Direction of the trade.
    pub direction: BasisDirection,
    /// Spot instrument, quote and taker fee.
    pub spot: LegQuote,
    /// Perpetual instrument, quote and taker fee.
    pub perp: LegQuote,
    /// Funding convention of the perpetual; only
    /// [`PerpConvention::Linear`] is supported.
    pub convention: PerpConvention,
    /// Current mark price of the perpetual (recorded; funding uses each
    /// event's own mark).
    pub mark: Price,
    /// Current index price (recorded; the premium is `mark - index`).
    pub index: Price,
    /// Size wanted on each leg, in base units, on both lot grids.
    pub target_quantity: Quantity,
    /// Smallest size worth trading.
    pub min_quantity: Quantity,
    /// The perpetual's funding grid.
    pub schedule: FundingSchedule,
    /// End of the holding window. The position is held through it: a
    /// funding instant exactly at `exit_time` is paid or received, and the
    /// position is closed just after.
    pub exit_time: Timestamp,
    /// One event per funding instant in `(as_of, exit_time]`, in order.
    pub funding: Vec<FundingEvent>,
    /// How the venue settles each funding amount. `None` keeps each amount
    /// exact; `Some` rounds each event's exact amount once, as the venue
    /// would.
    pub funding_rounding: Option<SettlementRounding>,
    /// Assumed executable basis (`perp - spot`, per unit) when the position
    /// is closed.
    pub expected_exit_basis: Decimal,
    /// Carry items; `Action::Buy` is the leg bought at entry and `Action::Sell`
    /// the leg sold.
    pub carry: Vec<Carry>,
    /// Smallest net edge to accept, in `quote_currency`; zero or positive.
    pub min_net_edge: Money,
}

/// Funding over one instant of the holding window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FundingCost {
    /// The funding instant.
    pub time: Timestamp,
    /// Where the rate came from.
    pub kind: RateKind,
    /// Paid (positive) or received (negative).
    pub cost: Cost,
}

/// Every cost subtracted from the gross basis edge, itemized.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BasisCosts {
    /// Entry fee on the spot leg.
    pub spot_entry_fee: Cost,
    /// Entry fee on the perpetual leg.
    pub perp_entry_fee: Cost,
    /// Exit fee on the spot leg (spot at its entry price, same rate).
    pub spot_exit_fee: Cost,
    /// Exit fee on the perpetual leg (at `spot entry price +
    /// expected_exit_basis`, same rate).
    pub perp_exit_fee: Cost,
    /// Carry items, in input order.
    pub carry: Vec<CarryCost>,
    /// Funding at each instant of the window, in order.
    pub funding: Vec<FundingCost>,
    /// Sum of funding.
    pub funding_total: Cost,
    /// Sum of all of the above.
    pub total: Cost,
}

/// An accepted spot-perpetual basis trade with its expected economics.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SpotPerpOpportunity {
    /// Decision time.
    pub as_of: Timestamp,
    /// Direction.
    pub direction: BasisDirection,
    /// Spot leg at its executable price.
    pub spot: Leg,
    /// Perpetual leg at its executable price.
    pub perp: Leg,
    /// Mark at `as_of`.
    pub mark: Price,
    /// Index at `as_of`.
    pub index: Price,
    /// Executable entry basis, `perp price - spot price`, per unit.
    pub entry_basis: Decimal,
    /// The assumed exit basis.
    pub expected_exit_basis: Decimal,
    /// Convergence edge, of any sign: `(entry - exit) * q` long spot,
    /// `(exit - entry) * q` short spot.
    pub expected_gross: Money,
    /// Costs subtracted from it.
    pub expected_costs: BasisCosts,
    /// `expected_gross - expected_costs.total`.
    pub expected_net: Money,
}

/// Why [`evaluate_spot_perp`] did not accept the trade.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BasisRejectReason {
    /// The side a leg needs is not quoted.
    MissingQuote {
        /// The instrument without the needed side.
        instrument: Instrument,
        /// The side the leg trades.
        side: Action,
    },
    /// Less size is executable on both legs than `min_quantity`.
    InsufficientLiquidity {
        /// Size executable on both legs, capped at the target.
        available: Quantity,
        /// The minimum asked for.
        required: Quantity,
    },
    /// The net edge is not above zero or is below the threshold.
    NetEdgeBelowThreshold {
        /// The convergence edge, of any sign.
        gross: Money,
        /// Costs subtracted from it (boxed: the itemized costs are large).
        costs: Box<BasisCosts>,
        /// The net edge.
        net: Money,
        /// The threshold asked for.
        min_net_edge: Money,
    },
}

/// Result of [`evaluate_spot_perp`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BasisEvaluation {
    /// The net edge clears the threshold (boxed: the opportunity is large).
    Accepted(Box<SpotPerpOpportunity>),
    /// No opportunity, with the reason.
    Rejected(BasisRejectReason),
}

impl BasisEvaluation {
    /// The opportunity if accepted.
    #[must_use]
    pub fn accepted(&self) -> Option<&SpotPerpOpportunity> {
        match self {
            Self::Accepted(o) => Some(o.as_ref()),
            Self::Rejected(_) => None,
        }
    }
}

/// `q * (perp - spot - exit_basis)` for long spot, the negative otherwise:
/// one exact sum of products.
fn gross_edge(
    input: &BasisInput,
    qv: Decimal,
    spot_px: Decimal,
    perp_px: Decimal,
) -> Result<Money> {
    let sign = match input.direction {
        BasisDirection::LongSpotShortPerp => Decimal::ONE,
        BasisDirection::ShortSpotLongPerp => Decimal::NEGATIVE_ONE,
    };
    let neg = exact::neg(sign);
    let gross = wide::sum_products_exact(
        &[
            &[sign, qv, perp_px],
            &[neg, qv, spot_px],
            &[neg, qv, input.expected_exit_basis],
        ],
        "basis gross",
    )?;
    Ok(Money::new(gross, input.quote_currency))
}

/// Entry and exit fees of both legs: spot entry, perpetual entry, spot exit
/// (at the entry price) and perpetual exit (at spot + exit basis).
fn trade_fees(
    input: &BasisInput,
    spot_px: Decimal,
    perp_px: Decimal,
    qv: Decimal,
) -> Result<[Cost; 4]> {
    let ccy = input.quote_currency;
    let spot_entry_fee = input.spot.fee.fee_on_factors(&[spot_px, qv], ccy)?;
    let perp_entry_fee = input.perp.fee.fee_on_factors(&[perp_px, qv], ccy)?;
    // Implied exit: spot unchanged, perpetual at spot + exit basis.
    let perp_exit_price =
        Price::new(exact::add(spot_px, input.expected_exit_basis)?).map_err(|_| {
            ArbitrageError::InvalidInput {
                reason: "spot entry price + expected_exit_basis must be positive",
            }
        })?;
    let perp_exit_fee = input
        .perp
        .fee
        .fee_on_factors(&[perp_exit_price.value(), qv], ccy)?;
    Ok([
        spot_entry_fee,
        perp_entry_fee,
        spot_entry_fee,
        perp_exit_fee,
    ])
}

/// The carry costs, each on its leg's entry notional.
fn carry_costs(
    input: &BasisInput,
    spot_px: Decimal,
    perp_px: Decimal,
    qv: Decimal,
) -> Result<Vec<CarryCost>> {
    let spot_side = input.direction.spot_side();
    input
        .carry
        .iter()
        .map(|item| {
            let price = if item.leg == spot_side {
                spot_px
            } else {
                perp_px
            };
            Ok(CarryCost {
                kind: item.kind,
                leg: item.leg,
                cost: item
                    .rate
                    .fee_on_factors(&[price, qv], input.quote_currency)?,
            })
        })
        .collect()
}

/// The funding of the perpetual position over the events.
fn funding_costs(input: &BasisInput, q: Quantity) -> Result<Vec<FundingCost>> {
    let ccy = input.quote_currency;
    // The perpetual position: short q when long spot, long q otherwise.
    let perp_position = match input.direction {
        BasisDirection::LongSpotShortPerp => exact::neg(q.value()),
        BasisDirection::ShortSpotLongPerp => q.value(),
    };
    input
        .funding
        .iter()
        .map(|event| {
            let cost = match input.funding_rounding {
                None => funding_cashflow(input.convention, perp_position, event, ccy)?,
                Some(r) => {
                    funding_cashflow_rounded(input.convention, perp_position, event, ccy, r)?
                }
            };
            Ok(FundingCost {
                time: event.time,
                kind: event.rate.kind(),
                cost,
            })
        })
        .collect()
}

/// Evaluates a spot-perpetual basis trade net of fees, carry and funding.
///
/// # Errors
///
/// - [`ArbitrageError::UnsupportedAction`] if a carry item's leg is not buy or
///   sell (checked first).
/// - [`ArbitrageError::InvalidInput`] if the perpetual is not linear (checked
///   next, before anything is computed: an inverse perpetual's `PnL` is not
///   linear in price, and its funding is paid in the base currency), if the
///   legs are the same instrument,
///   `min_quantity` is above `target_quantity`, `exit_time` is not after
///   `as_of`, the funding events are not exactly the schedule's instants in
///   `(as_of, exit_time]` in order, or the implied perpetual exit price
///   (`spot entry price + expected_exit_basis`) is not positive.
/// - [`ArbitrageError::LookAhead`] if an event uses a rate published after
///   `as_of`.
/// - [`ArbitrageError::InvalidValue`] or
///   [`ArbitrageError::CurrencyMismatch`] for a bad `min_net_edge`, and
///   [`ArbitrageError::Expired`] for an expired leg.
/// - Arithmetic errors. Nothing is rounded, except each funding amount when
///   `funding_rounding` is `Some` (once per event, from its exact value).
pub fn evaluate_spot_perp(input: &BasisInput) -> Result<BasisEvaluation> {
    validate(input)?;
    let ccy = input.quote_currency;
    let dir = input.direction;

    let Some(spot_level) = input.spot.book.executable(dir.spot_side())? else {
        return Ok(BasisEvaluation::Rejected(BasisRejectReason::MissingQuote {
            instrument: input.spot.instrument.clone(),
            side: dir.spot_side(),
        }));
    };
    let Some(perp_level) = input.perp.book.executable(dir.perp_side())? else {
        return Ok(BasisEvaluation::Rejected(BasisRejectReason::MissingQuote {
            instrument: input.perp.instrument.clone(),
            side: dir.perp_side(),
        }));
    };

    let q = input
        .target_quantity
        .min(spot_level.size)
        .min(perp_level.size);
    if q < input.min_quantity {
        return Ok(BasisEvaluation::Rejected(
            BasisRejectReason::InsufficientLiquidity {
                available: q,
                required: input.min_quantity,
            },
        ));
    }

    let (qv, spot_px, perp_px) = (
        q.value(),
        spot_level.price.value(),
        perp_level.price.value(),
    );
    let entry_basis = exact::sub(perp_px, spot_px)?;
    let gross = gross_edge(input, qv, spot_px, perp_px)?;

    let [spot_entry_fee, perp_entry_fee, spot_exit_fee, perp_exit_fee] =
        trade_fees(input, spot_px, perp_px, qv)?;
    let mut items = vec![spot_entry_fee, perp_entry_fee, spot_exit_fee, perp_exit_fee];
    let carry = carry_costs(input, spot_px, perp_px, qv)?;
    items.extend(carry.iter().map(|c| c.cost));
    let funding = funding_costs(input, q)?;
    let funding_items: Vec<Cost> = funding.iter().map(|f| f.cost).collect();
    let funding_total = Cost::total(&funding_items, ccy)?;
    items.extend(funding_items);
    let total = Cost::total(&items, ccy)?;

    let costs = BasisCosts {
        spot_entry_fee,
        perp_entry_fee,
        spot_exit_fee,
        perp_exit_fee,
        carry,
        funding,
        funding_total,
        total,
    };
    let net = total.net_of(gross)?;
    if !net.is_positive() || net.amount() < input.min_net_edge.amount() {
        return Ok(BasisEvaluation::Rejected(
            BasisRejectReason::NetEdgeBelowThreshold {
                gross,
                costs: Box::new(costs),
                net,
                min_net_edge: input.min_net_edge,
            },
        ));
    }

    Ok(BasisEvaluation::Accepted(Box::new(SpotPerpOpportunity {
        as_of: input.as_of,
        direction: dir,
        spot: Leg {
            instrument: input.spot.instrument.clone(),
            side: dir.spot_side(),
            quantity: q,
            price: spot_level.price,
        },
        perp: Leg {
            instrument: input.perp.instrument.clone(),
            side: dir.perp_side(),
            quantity: q,
            price: perp_level.price,
        },
        mark: input.mark,
        index: input.index,
        entry_basis,
        expected_exit_basis: input.expected_exit_basis.normalize(),
        expected_gross: gross,
        expected_costs: costs,
        expected_net: net,
    })))
}

fn validate(input: &BasisInput) -> Result<()> {
    // Directions first: an unsupported action is never reported as a
    // rejection for another reason.
    for item in &input.carry {
        Dir::try_from(item.leg)?;
    }
    // The basis kernel takes the perpetual position in base units and its
    // funding in the quote currency, with PnL linear in price. An inverse
    // perpetual has a position in contracts, funding in base and PnL
    // convex in price, so it is refused before anything is computed.
    if input.convention != PerpConvention::Linear {
        return Err(ArbitrageError::InvalidInput {
            reason: "evaluate_spot_perp supports linear perpetuals only",
        });
    }
    if input.spot.instrument.id == input.perp.instrument.id {
        return Err(ArbitrageError::InvalidInput {
            reason: "spot and perpetual legs are the same instrument",
        });
    }
    if input.min_quantity > input.target_quantity {
        return Err(ArbitrageError::InvalidInput {
            reason: "min_quantity is above target_quantity",
        });
    }
    if input.min_net_edge.currency() != input.quote_currency {
        return Err(ArbitrageError::CurrencyMismatch {
            left: input.quote_currency,
            right: input.min_net_edge.currency(),
        });
    }
    if input.min_net_edge.is_negative() {
        return Err(ArbitrageError::InvalidValue {
            field: "min_net_edge",
            value: input.min_net_edge.amount(),
            reason: "must not be negative",
        });
    }
    if input.exit_time <= input.as_of {
        return Err(ArbitrageError::InvalidInput {
            reason: "exit_time must be after as_of",
        });
    }
    input.spot.instrument.ensure_live(input.as_of)?;
    input.perp.instrument.ensure_live(input.as_of)?;

    let instants = input.schedule.instants(input.as_of, input.exit_time)?;
    let times: Vec<Timestamp> = input.funding.iter().map(|e| e.time).collect();
    if times != instants {
        return Err(ArbitrageError::InvalidInput {
            reason: "funding events must be exactly the schedule instants in (as_of, exit_time], in order",
        });
    }
    for event in &input.funding {
        event.rate.check_known_at(input.as_of)?;
    }
    Ok(())
}
