//! Net opportunity between two instruments at executable prices.
//!
//! The relation is the simplest one: buy one instrument at its ask and sell
//! another, economically equivalent one at its bid. The gross edge is
//! `(bid - ask) * quantity`. Fees on both legs and explicit carry (financing,
//! borrow) are subtracted to get the net edge. An opportunity is accepted only
//! if the net edge is positive and at least the caller's threshold, so a
//! positive gross edge that costs eat is rejected.
//!
//! Everything here is an estimate at quoted prices. An accepted
//! [`Opportunity`] is an intent to trade, not an executed or guaranteed
//! profit: execution is not atomic, the legs can fill partially or not at
//! all, and prices can move between them.

use crate::cost::{Cost, FeeRate};
use crate::error::{ArbitrageError, Result};
use crate::instrument::{Instrument, Leg, Side, Timestamp, TopOfBook};
use crate::money::{Currency, Money};
use crate::units::Quantity;
use crate::wide;
use rust_decimal::Decimal;

/// What a carry item pays for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CarryKind {
    /// Financing of a long position (cash borrowed or not earning).
    Financing,
    /// Borrow fee for the asset sold short.
    Borrow,
    /// Any other proportional holding cost or benefit.
    Other,
}

/// A proportional holding cost charged on one leg's notional over the
/// expected holding period.
///
/// The rate is for the whole period, not annualized: the caller converts an
/// annual rate and a horizon into period basis points. A negative rate is a
/// benefit (for example interest earned on the short proceeds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Carry {
    /// What the item pays for.
    pub kind: CarryKind,
    /// The leg whose notional it is charged on.
    pub leg: Side,
    /// Rate for the holding period, in basis points of that notional.
    pub rate: FeeRate,
}

/// One instrument's quote and fee as an input to [`evaluate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegQuote {
    /// The instrument.
    pub instrument: Instrument,
    /// Its top of book.
    pub book: TopOfBook,
    /// Taker fee on this instrument (the leg crosses the spread).
    pub fee: FeeRate,
}

/// Input to [`evaluate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpportunityInput {
    /// Time of the quotes.
    pub as_of: Timestamp,
    /// Currency of every price, fee and carry amount.
    pub quote_currency: Currency,
    /// Instrument to buy, at its ask.
    pub buy: LegQuote,
    /// Instrument to sell, at its bid.
    pub sell: LegQuote,
    /// Size wanted on each leg, on both legs' lot grids (not rounded here).
    pub target_quantity: Quantity,
    /// Smallest size worth trading; less executable size is a rejection.
    pub min_quantity: Quantity,
    /// Carry items, each charged on one leg's notional.
    pub carry: Vec<Carry>,
    /// Smallest net edge to accept, in `quote_currency`; zero or positive.
    pub min_net_edge: Money,
}

/// A cost charged on one leg for a carry item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CarryCost {
    /// What it pays for.
    pub kind: CarryKind,
    /// The leg it was charged on.
    pub leg: Side,
    /// The cost.
    pub cost: Cost,
}

/// Every cost subtracted from the gross edge, itemized.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CostBreakdown {
    /// Fee on the buy leg.
    pub buy_fee: Cost,
    /// Fee on the sell leg.
    pub sell_fee: Cost,
    /// Carry items, in input order.
    pub carry: Vec<CarryCost>,
    /// Sum of all of the above.
    pub total: Cost,
}

/// An accepted opportunity: two legs to send and the expected economics.
///
/// Every amount is an estimate at the quoted prices, prefixed `expected_`.
/// This is an intent, not an executed or guaranteed profit.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Opportunity {
    /// Time of the quotes it was computed from.
    pub as_of: Timestamp,
    /// Leg bought at the ask.
    pub buy: Leg,
    /// Leg sold at the bid.
    pub sell: Leg,
    /// `(sell price - buy price) * quantity`.
    pub expected_gross: Money,
    /// Costs subtracted from the gross edge.
    pub expected_costs: CostBreakdown,
    /// `expected_gross - expected_costs.total`.
    pub expected_net: Money,
}

/// Why [`evaluate`] did not accept an opportunity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RejectReason {
    /// The side a leg needs is not quoted.
    MissingQuote {
        /// The instrument without the needed side.
        instrument: Instrument,
        /// The side the leg trades (a buy needs an ask, a sell a bid).
        side: Side,
    },
    /// Less size is executable on both legs than `min_quantity`.
    InsufficientLiquidity {
        /// Size executable on both legs at the top of book, capped at the
        /// target.
        available: Quantity,
        /// The minimum asked for.
        required: Quantity,
    },
    /// The sell bid is not above the buy ask.
    NoGrossEdge {
        /// The gross edge, zero or negative.
        gross: Money,
    },
    /// The gross edge is positive but the net edge is not above zero or is
    /// below the threshold.
    NetEdgeBelowThreshold {
        /// The gross edge, positive.
        gross: Money,
        /// Costs subtracted from it.
        costs: CostBreakdown,
        /// The net edge.
        net: Money,
        /// The threshold asked for.
        min_net_edge: Money,
    },
}

/// Result of [`evaluate`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Evaluation {
    /// The net edge clears the threshold.
    Accepted(Opportunity),
    /// No opportunity, with the reason.
    Rejected(RejectReason),
}

impl Evaluation {
    /// The opportunity if accepted.
    #[must_use]
    pub fn accepted(&self) -> Option<&Opportunity> {
        match self {
            Self::Accepted(o) => Some(o),
            Self::Rejected(_) => None,
        }
    }

    /// The reason if rejected.
    #[must_use]
    pub fn rejected(&self) -> Option<&RejectReason> {
        match self {
            Self::Accepted(_) => None,
            Self::Rejected(r) => Some(r),
        }
    }
}

/// Evaluates buying `input.buy` at its ask and selling `input.sell` at its
/// bid.
///
/// The size is `min(target_quantity, ask size, bid size)`: only the top level
/// of each book is used, so the estimate never assumes depth that was not
/// quoted.
///
/// Lot sizes are a caller precondition: the kernel does not round the size
/// to either venue's lot. Pass a `target_quantity` that both legs can trade;
/// if a quoted size is below it and is not on both lot grids, the resulting
/// leg size may not be placeable as is.
///
/// # Errors
///
/// - [`ArbitrageError::InvalidInput`] if both legs are the same instrument or
///   `min_quantity` is above `target_quantity`.
/// - [`ArbitrageError::InvalidValue`] if `min_net_edge` is negative.
/// - [`ArbitrageError::CurrencyMismatch`] if `min_net_edge` is not in
///   `quote_currency`.
/// - [`ArbitrageError::Expired`] if a leg's instrument has expired at
///   `as_of`.
/// - Arithmetic errors ([`ArbitrageError::Overflow`],
///   [`ArbitrageError::Inexact`]); nothing is rounded.
pub fn evaluate(input: &OpportunityInput) -> Result<Evaluation> {
    validate(input)?;
    let ccy = input.quote_currency;

    let Some(ask) = input.buy.book.executable(Side::Buy) else {
        return Ok(Evaluation::Rejected(RejectReason::MissingQuote {
            instrument: input.buy.instrument.clone(),
            side: Side::Buy,
        }));
    };
    let Some(bid) = input.sell.book.executable(Side::Sell) else {
        return Ok(Evaluation::Rejected(RejectReason::MissingQuote {
            instrument: input.sell.instrument.clone(),
            side: Side::Sell,
        }));
    };

    let quantity = input.target_quantity.min(ask.size).min(bid.size);
    if quantity < input.min_quantity {
        return Ok(Evaluation::Rejected(RejectReason::InsufficientLiquidity {
            available: quantity,
            required: input.min_quantity,
        }));
    }

    // q * bid - q * ask as one exact sum: the notionals may pass the range
    // of a Decimal when the gross does not.
    let (q, ask_px, bid_px) = (quantity.value(), ask.price.value(), bid.price.value());
    let gross = Money::new(
        wide::sum_products_exact(
            &[&[bid_px, q], &[Decimal::NEGATIVE_ONE, ask_px, q]],
            "gross",
        )?,
        ccy,
    );
    if !gross.is_positive() {
        return Ok(Evaluation::Rejected(RejectReason::NoGrossEdge { gross }));
    }

    let buy_fee = input.buy.fee.fee_on_factors(&[ask_px, q], ccy)?;
    let sell_fee = input.sell.fee.fee_on_factors(&[bid_px, q], ccy)?;
    let mut items = vec![buy_fee, sell_fee];
    let mut carry = Vec::with_capacity(input.carry.len());
    for item in &input.carry {
        let price = match item.leg {
            Side::Buy => ask_px,
            Side::Sell => bid_px,
        };
        let cost = item.rate.fee_on_factors(&[price, q], ccy)?;
        items.push(cost);
        carry.push(CarryCost {
            kind: item.kind,
            leg: item.leg,
            cost,
        });
    }
    let total = Cost::total(&items, ccy)?;
    let costs = CostBreakdown {
        buy_fee,
        sell_fee,
        carry,
        total,
    };
    let net = total.net_of(gross)?;

    if !net.is_positive() || net.amount() < input.min_net_edge.amount() {
        return Ok(Evaluation::Rejected(RejectReason::NetEdgeBelowThreshold {
            gross,
            costs,
            net,
            min_net_edge: input.min_net_edge,
        }));
    }

    Ok(Evaluation::Accepted(Opportunity {
        as_of: input.as_of,
        buy: Leg {
            instrument: input.buy.instrument.clone(),
            side: Side::Buy,
            quantity,
            price: ask.price,
        },
        sell: Leg {
            instrument: input.sell.instrument.clone(),
            side: Side::Sell,
            quantity,
            price: bid.price,
        },
        expected_gross: gross,
        expected_costs: costs,
        expected_net: net,
    }))
}

fn validate(input: &OpportunityInput) -> Result<()> {
    if input.buy.instrument.id == input.sell.instrument.id {
        return Err(ArbitrageError::InvalidInput {
            reason: "buy and sell legs are the same instrument",
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
    input.buy.instrument.ensure_live(input.as_of)?;
    input.sell.instrument.ensure_live(input.as_of)?;
    Ok(())
}
