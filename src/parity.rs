//! Executable edge of put-call parity against a same-expiry future.
//!
//! For a strike `K`, a call, a put and a future that all expire at `T`, and
//! the discount factor `D = D(t, T)` published for that expiry, the relation
//! `C - P = D * (F - K)` holds per unit of underlying. Two packages trade it:
//!
//! - **reversal:** buy the call at its ask, sell the put at its bid and sell
//!   the future at its bid. Per unit of underlying the gross edge is
//!   `D * (bid_F - K) - (ask_C - bid_P)`;
//! - **conversion:** sell the call at its bid, buy the put at its ask and
//!   buy the future at its ask. The gross edge is
//!   `(bid_C - ask_P) - D * (ask_F - K)`.
//!
//! A package is a quantity of underlying (for example 1 XYZ), the same on
//! all three legs. Each leg trades `package / contract_size` contracts, which
//! must be a whole multiple of its lot. The fee of each leg is computed on
//! the package and rounded once to the fee quantum with the caller's mode,
//! and the net edge is the gross edge minus the three fees.
//!
//! Option fees follow one of two bases ([`OptionFee`]): basis points of the
//! underlying notional at the index, capped at a fraction of the premium
//! (the usual exchange rule), or basis points of the premium. The future
//! pays basis points of its notional.
//!
//! Everything is an estimate at the top of book. An edge is an intent to
//! trade, not an executed or guaranteed profit: the three legs are not
//! executed atomically and can fill partially.

use crate::cost::{Cost, FeeRate};
use crate::error::{ArbitrageError, Result};
use crate::exact;
use crate::instrument::{Instrument, Leg, Side, Timestamp, TopOfBook};
use crate::money::{Currency, Money, Rounding};
use crate::units::{Price, Quantity};
use rust_decimal::Decimal;

/// How an option leg's fee is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OptionFee {
    /// `min(rate * index * package, cap * premium)`, with
    /// `premium = price * package`. `rate` is not negative and `cap` is a
    /// fraction of the premium in `[0, 1]`.
    UnderlyingCapped {
        /// Rate on the underlying notional at the index.
        rate: FeeRate,
        /// Largest fee as a fraction of the premium.
        cap: Decimal,
    },
    /// `rate * premium`, with `premium = price * package`. `rate` is not
    /// negative.
    Premium {
        /// Rate on the premium.
        rate: FeeRate,
    },
}

/// One option of the parity package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityOption {
    /// The option, dated at the common expiry.
    pub instrument: Instrument,
    /// Its top of book. Prices are per unit of underlying and sizes are in
    /// contracts.
    pub book: TopOfBook,
    /// Units of underlying per contract (the multiplier).
    pub contract_size: Quantity,
    /// Smallest tradable number of contracts.
    pub lot: Quantity,
    /// Its fee (taker: the package crosses the spread).
    pub fee: OptionFee,
}

/// The future of the parity package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityFuture {
    /// The future, dated at the common expiry.
    pub instrument: Instrument,
    /// Its top of book. Prices are per unit of underlying and sizes are in
    /// contracts.
    pub book: TopOfBook,
    /// Units of underlying per contract (the multiplier).
    pub contract_size: Quantity,
    /// Smallest tradable number of contracts.
    pub lot: Quantity,
    /// Its taker fee, on the notional.
    pub fee: FeeRate,
}

/// Input to [`evaluate_parity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityInput {
    /// Time of the quotes.
    pub as_of: Timestamp,
    /// Currency of every price and fee.
    pub quote_currency: Currency,
    /// The common strike of the call and the put.
    pub strike: Price,
    /// The published discount factor `D(t, T)` to the common expiry;
    /// strictly positive.
    pub discount: Decimal,
    /// The index the underlying-capped option fee is computed on.
    pub index: Price,
    /// The call.
    pub call: ParityOption,
    /// The put.
    pub put: ParityOption,
    /// The future.
    pub future: ParityFuture,
    /// Underlying per package, the same on every leg.
    pub package: Quantity,
    /// Decimal places of the fee quantum (6 for `1e-6`).
    pub fee_dp: u32,
    /// How each leg's fee is rounded to the quantum, once.
    pub fee_rounding: Rounding,
}

/// Which way the package trades.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParityDirection {
    /// Buy the call, sell the put, sell the future.
    Reversal,
    /// Sell the call, buy the put, buy the future.
    Conversion,
}

impl ParityDirection {
    /// The side of the call leg; the put and the future take the opposite.
    #[must_use]
    pub fn call_side(self) -> Side {
        match self {
            Self::Reversal => Side::Buy,
            Self::Conversion => Side::Sell,
        }
    }
}

/// The three legs of a package, sized in contracts at executable prices.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParityLegs {
    /// The call leg.
    pub call: Leg,
    /// The put leg.
    pub put: Leg,
    /// The future leg.
    pub future: Leg,
}

/// The fee of each leg, each rounded once, and their sum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParityFees {
    /// Fee of the call leg.
    pub call: Cost,
    /// Fee of the put leg.
    pub put: Cost,
    /// Fee of the future leg.
    pub future: Cost,
    /// `call + put + future`.
    pub total: Cost,
}

/// The executable edge of one direction for one package.
///
/// Every amount is an estimate at the quoted prices, in the quote currency
/// per package.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParityEdge {
    /// The direction.
    pub direction: ParityDirection,
    /// Time of the quotes.
    pub as_of: Timestamp,
    /// The legs to send.
    pub legs: ParityLegs,
    /// The gross edge, before fees.
    pub expected_gross: Money,
    /// The fees of the three legs.
    pub expected_fees: ParityFees,
    /// `expected_gross - expected_fees.total`.
    pub expected_net: Money,
}

/// Why one direction has no executable edge.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ParityUnavailable {
    /// A side the direction needs is not quoted.
    MissingQuote {
        /// The instrument without the needed side.
        instrument: Instrument,
        /// The side the leg trades.
        side: Side,
    },
    /// The top of book holds fewer contracts than one package needs.
    InsufficientLiquidity {
        /// The instrument.
        instrument: Instrument,
        /// The contracts at the top level.
        available: Quantity,
        /// The contracts one package needs.
        required: Quantity,
    },
}

/// The outcome of one direction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DirectionEvaluation {
    /// The edge, which may be negative.
    Edge(Box<ParityEdge>),
    /// No edge can be computed.
    Unavailable(ParityUnavailable),
}

impl DirectionEvaluation {
    /// The edge if one was computed.
    #[must_use]
    pub fn edge(&self) -> Option<&ParityEdge> {
        match self {
            Self::Edge(e) => Some(e.as_ref()),
            Self::Unavailable(_) => None,
        }
    }
}

/// Result of [`evaluate_parity`]: both directions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParityEvaluation {
    /// The reversal.
    pub reversal: DirectionEvaluation,
    /// The conversion.
    pub conversion: DirectionEvaluation,
}

impl ParityEvaluation {
    /// The executable edge: the direction with the larger net edge, of those
    /// that could be computed. On a tie the reversal is returned. The edge
    /// may be negative; comparing it with a tolerance is the caller's call.
    #[must_use]
    pub fn executable(&self) -> Option<&ParityEdge> {
        match (self.reversal.edge(), self.conversion.edge()) {
            (Some(r), Some(c)) => {
                if c.expected_net.amount() > r.expected_net.amount() {
                    Some(c)
                } else {
                    Some(r)
                }
            }
            (Some(r), None) => Some(r),
            (None, c) => c,
        }
    }
}

/// Evaluates both directions of put-call parity for one package.
///
/// Only the top level of each book is used. A direction is unavailable if a
/// side it needs is missing or holds fewer contracts than one package.
///
/// # Errors
///
/// - [`ArbitrageError::InvalidInput`] if two legs are the same instrument,
///   a leg is undated, the expiries differ, or the package is not a whole
///   multiple of a leg's lot in contracts.
/// - [`ArbitrageError::InvalidValue`] if the discount factor is not
///   positive, an option fee rate is negative, or a cap is outside `[0, 1]`.
/// - [`ArbitrageError::Expired`] if the legs have expired at `as_of`.
/// - Arithmetic errors ([`ArbitrageError::Overflow`],
///   [`ArbitrageError::Inexact`]); nothing is rounded except each fee, once.
pub fn evaluate_parity(input: &ParityInput) -> Result<ParityEvaluation> {
    validate(input)?;
    let contracts = Contracts {
        call: contracts(input.package, input.call.contract_size, input.call.lot)?,
        put: contracts(input.package, input.put.contract_size, input.put.lot)?,
        future: contracts(input.package, input.future.contract_size, input.future.lot)?,
    };
    Ok(ParityEvaluation {
        reversal: direction(input, &contracts, ParityDirection::Reversal)?,
        conversion: direction(input, &contracts, ParityDirection::Conversion)?,
    })
}

struct Contracts {
    call: Quantity,
    put: Quantity,
    future: Quantity,
}

fn direction(
    input: &ParityInput,
    contracts: &Contracts,
    dir: ParityDirection,
) -> Result<DirectionEvaluation> {
    let call_side = dir.call_side();
    let other_side = call_side.opposite();
    let call = match level(
        &input.call.instrument,
        &input.call.book,
        call_side,
        contracts.call,
    ) {
        Ok(p) => p,
        Err(u) => return Ok(DirectionEvaluation::Unavailable(u)),
    };
    let put = match level(
        &input.put.instrument,
        &input.put.book,
        other_side,
        contracts.put,
    ) {
        Ok(p) => p,
        Err(u) => return Ok(DirectionEvaluation::Unavailable(u)),
    };
    let future = match level(
        &input.future.instrument,
        &input.future.book,
        other_side,
        contracts.future,
    ) {
        Ok(p) => p,
        Err(u) => return Ok(DirectionEvaluation::Unavailable(u)),
    };

    // Per unit of underlying: D * (F - K) - (C - P) for a reversal, the
    // negative for a conversion.
    let carry = exact::mul(
        input.discount,
        exact::sub(future.value(), input.strike.value())?,
    )?;
    let options = exact::sub(call.value(), put.value())?;
    let per_unit = match dir {
        ParityDirection::Reversal => exact::sub(carry, options)?,
        ParityDirection::Conversion => exact::sub(options, carry)?,
    };
    let ccy = input.quote_currency;
    let gross = Money::new(exact::mul(per_unit, input.package.value())?, ccy);

    let call_fee = option_fee(input, &input.call.fee, call)?;
    let put_fee = option_fee(input, &input.put.fee, put)?;
    let future_fee = round(
        input,
        input
            .future
            .fee
            .fee_on(future.notional(input.package, ccy)?)?,
    )?;
    let total = call_fee.checked_add(put_fee)?.checked_add(future_fee)?;
    let net = total.net_of(gross)?;

    let leg = |instrument: &Instrument, side, quantity, price| Leg {
        instrument: instrument.clone(),
        side,
        quantity,
        price,
    };
    Ok(DirectionEvaluation::Edge(Box::new(ParityEdge {
        direction: dir,
        as_of: input.as_of,
        legs: ParityLegs {
            call: leg(&input.call.instrument, call_side, contracts.call, call),
            put: leg(&input.put.instrument, other_side, contracts.put, put),
            future: leg(
                &input.future.instrument,
                other_side,
                contracts.future,
                future,
            ),
        },
        expected_gross: gross,
        expected_fees: ParityFees {
            call: call_fee,
            put: put_fee,
            future: future_fee,
            total,
        },
        expected_net: net,
    })))
}

/// The executable price of `side`, if quoted with at least `required`
/// contracts.
fn level(
    instrument: &Instrument,
    book: &TopOfBook,
    side: Side,
    required: Quantity,
) -> core::result::Result<Price, ParityUnavailable> {
    let Some(l) = book.executable(side) else {
        return Err(ParityUnavailable::MissingQuote {
            instrument: instrument.clone(),
            side,
        });
    };
    if l.size < required {
        return Err(ParityUnavailable::InsufficientLiquidity {
            instrument: instrument.clone(),
            available: l.size,
            required,
        });
    }
    Ok(l.price)
}

fn option_fee(input: &ParityInput, fee: &OptionFee, price: Price) -> Result<Cost> {
    let ccy = input.quote_currency;
    let premium = price.notional(input.package, ccy)?;
    let exact_fee = match *fee {
        OptionFee::UnderlyingCapped { rate, cap } => {
            let on_underlying = rate.fee_on(input.index.notional(input.package, ccy)?)?;
            let capped = premium.checked_scale(cap)?;
            if capped.amount() < on_underlying.money().amount() {
                Cost::new(capped)
            } else {
                on_underlying
            }
        }
        OptionFee::Premium { rate } => rate.fee_on(premium)?,
    };
    round(input, exact_fee)
}

fn round(input: &ParityInput, fee: Cost) -> Result<Cost> {
    Ok(Cost::new(
        fee.money().round_dp(input.fee_dp, input.fee_rounding)?,
    ))
}

/// `package / contract_size` contracts, which must be a whole multiple of
/// `lot`.
fn contracts(package: Quantity, contract_size: Quantity, lot: Quantity) -> Result<Quantity> {
    let not_whole = ArbitrageError::InvalidInput {
        reason: "the package is not a whole multiple of a leg's lot in contracts",
    };
    // An exact quotient is recovered by checked_div; the product check
    // rejects a rounded one.
    let n = package
        .value()
        .checked_div(contract_size.value())
        .ok_or(ArbitrageError::Overflow { op: "div" })?
        .normalize();
    // A fractional quotient is rejected before the product, which could
    // otherwise not fit at the quotient's scale.
    if !n.fract().is_zero() || exact::mul(n, contract_size.value())? != package.value() {
        return Err(not_whole);
    }
    let n = Quantity::new(n)?;
    match n.floor_to_lot(lot)? {
        Some(floored) if floored == n => Ok(n),
        _ => Err(not_whole),
    }
}

fn validate(input: &ParityInput) -> Result<()> {
    let (c, p, f) = (
        &input.call.instrument,
        &input.put.instrument,
        &input.future.instrument,
    );
    if c.id == p.id || c.id == f.id || p.id == f.id {
        return Err(ArbitrageError::InvalidInput {
            reason: "two parity legs are the same instrument",
        });
    }
    let Some(expiry) = f.expiry else {
        return Err(ArbitrageError::InvalidInput {
            reason: "parity legs must be dated",
        });
    };
    if c.expiry != Some(expiry) || p.expiry != Some(expiry) {
        return Err(ArbitrageError::InvalidInput {
            reason: "parity legs must share one expiry",
        });
    }
    if input.discount <= Decimal::ZERO {
        return Err(ArbitrageError::InvalidValue {
            field: "discount factor",
            value: input.discount,
            reason: "must be positive",
        });
    }
    for fee in [&input.call.fee, &input.put.fee] {
        let (rate, cap) = match *fee {
            OptionFee::UnderlyingCapped { rate, cap } => (rate, Some(cap)),
            OptionFee::Premium { rate } => (rate, None),
        };
        if rate.bps() < Decimal::ZERO {
            return Err(ArbitrageError::InvalidValue {
                field: "option fee rate (bps)",
                value: rate.bps(),
                reason: "must not be negative",
            });
        }
        if let Some(cap) = cap {
            if cap < Decimal::ZERO || cap > Decimal::ONE {
                return Err(ArbitrageError::InvalidValue {
                    field: "option fee cap",
                    value: cap,
                    reason: "must be within [0, 1]",
                });
            }
        }
    }
    f.ensure_live(input.as_of)
}
