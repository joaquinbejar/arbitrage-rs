//! Executable edge of a box spread: two strikes, one expiry.
//!
//! For strikes `K_low < K_high`, calls and puts at both strikes that expire
//! at `T`, and the discount factor `D = D(t, T)` published for that expiry,
//! the box pays `K_high - K_low` at `T` whatever the underlying does, so
//! `C_low - P_low - C_high + P_high = D * (K_high - K_low)` per unit of
//! underlying. Two packages trade it:
//!
//! - **long box:** buy the low call and the high put at their asks, sell the
//!   low put and the high call at their bids. Per unit of underlying the
//!   gross edge is
//!   `D * (K_high - K_low) - (ask_Cl - bid_Pl - bid_Ch + ask_Ph)`;
//! - **short box:** the opposite sides. The gross edge is
//!   `(bid_Cl - ask_Pl - ask_Ch + bid_Ph) - D * (K_high - K_low)`.
//!
//! A package is a quantity of underlying, the same on all four legs. Each
//! leg trades `package / contract_size` contracts, which must be a whole
//! multiple of its lot. Each leg pays its [`crate::parity::OptionFee`], as in
//! [`crate::parity`].
//!
//! ## Rounding
//!
//! As in [`crate::parity`]: the gross edge is one exact sum of products,
//! rounded once to [`BoxInput::edge_dp`] with [`BoxInput::edge_rounding`];
//! each leg's fee is rounded once to [`BoxInput::fee_dp`] with
//! [`BoxInput::fee_rounding`]; the fee total and the net edge are exact
//! sums of the rounded figures.
//!
//! Everything is an estimate at the top of book. An edge is an intent to
//! trade, not an executed or guaranteed profit: the four legs are not
//! executed atomically and can fill partially.

use crate::cost::Cost;
use crate::error::{ArbitrageError, Result};
use crate::exact;
use crate::instrument::{Action, Instrument, Leg, Timestamp};
use crate::money::{Currency, Money, Rounding};
use crate::parity::{
    FeeTerms, ParityOption, ParityUnavailable, contracts, level, option_fee, positive_discount,
};
use crate::units::{Price, Quantity};
use crate::wide;
use rust_decimal::Decimal;

/// Input to [`evaluate_box`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxInput {
    /// Time of the quotes.
    pub as_of: Timestamp,
    /// Currency of every price and fee.
    pub quote_currency: Currency,
    /// The lower strike.
    pub low_strike: Price,
    /// The higher strike; strictly above `low_strike`.
    pub high_strike: Price,
    /// The published discount factor `D(t, T)` to the common expiry;
    /// strictly positive.
    pub discount: Decimal,
    /// The index the underlying-capped option fee is computed on.
    pub index: Price,
    /// The call at the lower strike.
    pub low_call: ParityOption,
    /// The put at the lower strike.
    pub low_put: ParityOption,
    /// The call at the higher strike.
    pub high_call: ParityOption,
    /// The put at the higher strike.
    pub high_put: ParityOption,
    /// Underlying per package, the same on every leg.
    pub package: Quantity,
    /// Decimal places of the fee quantum (6 for `1e-6`).
    pub fee_dp: u32,
    /// How each leg's fee is rounded to the quantum, once.
    pub fee_rounding: Rounding,
    /// Decimal places the gross edge is rounded to, once.
    pub edge_dp: u32,
    /// How the gross edge is rounded.
    pub edge_rounding: Rounding,
}

/// Which way the box trades.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BoxDirection {
    /// Buy the low call and the high put, sell the low put and the high
    /// call: pay now, receive `K_high - K_low` at expiry.
    Long,
    /// The opposite sides: receive now, pay `K_high - K_low` at expiry.
    Short,
}

impl BoxDirection {
    /// The side of the low call and the high put; the low put and the high
    /// call take the opposite.
    #[must_use]
    pub fn low_call_side(self) -> Action {
        match self {
            Self::Long => Action::Buy,
            Self::Short => Action::Sell,
        }
    }
}

/// The four legs of a box, sized in contracts at executable prices.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BoxLegs {
    /// The call at the lower strike.
    pub low_call: Leg,
    /// The put at the lower strike.
    pub low_put: Leg,
    /// The call at the higher strike.
    pub high_call: Leg,
    /// The put at the higher strike.
    pub high_put: Leg,
}

/// The fee of each leg, each rounded once, and their sum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BoxFees {
    /// Fee of the low call.
    pub low_call: Cost,
    /// Fee of the low put.
    pub low_put: Cost,
    /// Fee of the high call.
    pub high_call: Cost,
    /// Fee of the high put.
    pub high_put: Cost,
    /// The sum of the four.
    pub total: Cost,
}

/// The executable edge of one direction for one package, in the quote
/// currency.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BoxEdge {
    /// The direction.
    pub direction: BoxDirection,
    /// Time of the quotes.
    pub as_of: Timestamp,
    /// The legs to send.
    pub legs: BoxLegs,
    /// The gross edge, before fees.
    pub expected_gross: Money,
    /// The fees of the four legs.
    pub expected_fees: BoxFees,
    /// `expected_gross - expected_fees.total`.
    pub expected_net: Money,
}

/// The outcome of one direction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BoxDirectionEvaluation {
    /// The edge, which may be negative.
    Edge(Box<BoxEdge>),
    /// No edge can be computed.
    Unavailable(ParityUnavailable),
}

impl BoxDirectionEvaluation {
    /// The edge if one was computed.
    #[must_use]
    pub fn edge(&self) -> Option<&BoxEdge> {
        match self {
            Self::Edge(e) => Some(e.as_ref()),
            Self::Unavailable(_) => None,
        }
    }
}

/// Result of [`evaluate_box`]: both directions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BoxEvaluation {
    /// The long box.
    pub long: BoxDirectionEvaluation,
    /// The short box.
    pub short: BoxDirectionEvaluation,
}

impl BoxEvaluation {
    /// The direction with the larger net edge, of those that could be
    /// computed. On a tie the long box is returned. The edge may be
    /// negative; comparing it with a tolerance is the caller's call.
    #[must_use]
    pub fn executable(&self) -> Option<&BoxEdge> {
        match (self.long.edge(), self.short.edge()) {
            (Some(l), Some(s)) => {
                if s.expected_net.amount() > l.expected_net.amount() {
                    Some(s)
                } else {
                    Some(l)
                }
            }
            (Some(l), None) => Some(l),
            (None, s) => s,
        }
    }
}

/// Evaluates both directions of a box for one package.
///
/// Only the top level of each book is used. A direction is unavailable if a
/// side it needs is missing or holds fewer contracts than one package.
///
/// # Errors
///
/// - [`ArbitrageError::InvalidInput`] if two legs are the same instrument,
///   a leg is undated, the expiries differ, the high strike is not above
///   the low one, or the package is not a whole multiple of a leg's lot in
///   contracts.
/// - [`ArbitrageError::InvalidValue`] if the discount factor is not
///   positive, an option fee rate is negative, or a cap is outside `[0, 1]`.
/// - [`ArbitrageError::Expired`] if the legs have expired at `as_of`.
/// - Arithmetic errors only when a figure itself does not fit in a
///   [`Decimal`] (or `edge_dp` or `fee_dp` is above 28).
pub fn evaluate_box(input: &BoxInput) -> Result<BoxEvaluation> {
    validate(input)?;
    let n = [
        contracts(
            input.package,
            input.low_call.contract_size,
            input.low_call.lot,
        )?,
        contracts(
            input.package,
            input.low_put.contract_size,
            input.low_put.lot,
        )?,
        contracts(
            input.package,
            input.high_call.contract_size,
            input.high_call.lot,
        )?,
        contracts(
            input.package,
            input.high_put.contract_size,
            input.high_put.lot,
        )?,
    ];
    Ok(BoxEvaluation {
        long: direction(input, n, BoxDirection::Long)?,
        short: direction(input, n, BoxDirection::Short)?,
    })
}

fn direction(
    input: &BoxInput,
    n: [Quantity; 4],
    dir: BoxDirection,
) -> Result<BoxDirectionEvaluation> {
    let side = dir.low_call_side();
    let other = side.opposite();
    let [qty_low_call, qty_low_put, qty_high_call, qty_high_put] = n;
    let mut px = [Price::new(Decimal::ONE)?; 4];
    let legs = [
        (&input.low_call, side, qty_low_call),
        (&input.low_put, other, qty_low_put),
        (&input.high_call, other, qty_high_call),
        (&input.high_put, side, qty_high_put),
    ];
    for (slot, (opt, s, req)) in px.iter_mut().zip(legs) {
        match level(&opt.instrument, &opt.book, s, req)? {
            Ok(p) => *slot = p,
            Err(u) => return Ok(BoxDirectionEvaluation::Unavailable(u)),
        }
    }
    let [lc, lp, hc, hp] = px;

    // s * Q * (D * K_high - D * K_low - C_low + P_low + C_high - P_high):
    // one exact sum of products, rounded once.
    let sign = match dir {
        BoxDirection::Long => Decimal::ONE,
        BoxDirection::Short => Decimal::NEGATIVE_ONE,
    };
    let neg = exact::neg(sign);
    let (qty, disc) = (input.package.value(), input.discount);
    let gross = wide::sum_products_round(
        &[
            &[sign, qty, disc, input.high_strike.value()],
            &[neg, qty, disc, input.low_strike.value()],
            &[neg, qty, lc.value()],
            &[sign, qty, lp.value()],
            &[sign, qty, hc.value()],
            &[neg, qty, hp.value()],
        ],
        input.edge_dp,
        input.edge_rounding,
        "box gross",
    )?;

    let ft = FeeTerms {
        index: input.index,
        package: input.package,
        fee_dp: input.fee_dp,
        fee_rounding: input.fee_rounding,
    };
    let fees = [
        option_fee(ft, &input.low_call.fee, lc)?,
        option_fee(ft, &input.low_put.fee, lp)?,
        option_fee(ft, &input.high_call.fee, hc)?,
        option_fee(ft, &input.high_put.fee, hp)?,
    ];
    let total = wide::sum(&fees, "box fees")?;
    let [fee_low_call, fee_low_put, fee_high_call, fee_high_put] = fees;
    let net = wide::sum_products_exact(
        &[
            &[gross],
            &[Decimal::NEGATIVE_ONE, fee_low_call],
            &[Decimal::NEGATIVE_ONE, fee_low_put],
            &[Decimal::NEGATIVE_ONE, fee_high_call],
            &[Decimal::NEGATIVE_ONE, fee_high_put],
        ],
        "box net",
    )?;
    let ccy = input.quote_currency;
    let cost = |x| Cost::new(Money::new(x, ccy));
    let leg = |o: &ParityOption, side, quantity, price| Leg {
        instrument: o.instrument.clone(),
        side,
        quantity,
        price,
    };
    Ok(BoxDirectionEvaluation::Edge(Box::new(BoxEdge {
        direction: dir,
        as_of: input.as_of,
        legs: BoxLegs {
            low_call: leg(&input.low_call, side, qty_low_call, lc),
            low_put: leg(&input.low_put, other, qty_low_put, lp),
            high_call: leg(&input.high_call, other, qty_high_call, hc),
            high_put: leg(&input.high_put, side, qty_high_put, hp),
        },
        expected_gross: Money::new(gross, ccy),
        expected_fees: BoxFees {
            low_call: cost(fee_low_call),
            low_put: cost(fee_low_put),
            high_call: cost(fee_high_call),
            high_put: cost(fee_high_put),
            total: cost(total),
        },
        expected_net: Money::new(net, ccy),
    })))
}

fn validate(input: &BoxInput) -> Result<()> {
    let opts: [&Instrument; 4] = [
        &input.low_call.instrument,
        &input.low_put.instrument,
        &input.high_call.instrument,
        &input.high_put.instrument,
    ];
    for (i, a) in opts.iter().enumerate() {
        if opts.iter().take(i).any(|b| b.id == a.id) {
            return Err(ArbitrageError::InvalidInput {
                reason: "two box legs are the same instrument",
            });
        }
    }
    let Some(expiry) = input.low_call.instrument.expiry else {
        return Err(ArbitrageError::InvalidInput {
            reason: "box legs must be dated",
        });
    };
    if opts.iter().any(|o| o.expiry != Some(expiry)) {
        return Err(ArbitrageError::InvalidInput {
            reason: "box legs must share one expiry",
        });
    }
    if input.high_strike <= input.low_strike {
        return Err(ArbitrageError::InvalidInput {
            reason: "the high strike must be above the low strike",
        });
    }
    positive_discount(input.discount)?;
    for o in [
        &input.low_call,
        &input.low_put,
        &input.high_call,
        &input.high_put,
    ] {
        crate::parity::validate_option_fee(&o.fee)?;
    }
    input.low_call.instrument.ensure_live(input.as_of)
}
