//! Executable edge of put-call parity against a same-expiry future, and of
//! the options' synthetic future against a perpetual
//! ([`evaluate_synthetic`]).
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
//! must be a whole multiple of its lot.
//!
//! In [`evaluate_synthetic`] the hedge ([`ParityFuture`], the `future` leg
//! of the edge) may instead be a perpetual, with a funding forecast to the
//! options' expiry; see that function for the formula and its assumptions.
//!
//! Option fees follow one of two bases ([`OptionFee`]): basis points of the
//! underlying notional at the index, capped at a fraction of the premium
//! (the usual exchange rule), or basis points of the premium. The future
//! (or perpetual) pays basis points of its notional; a negative future rate is a rebate
//! and gives a negative fee.
//!
//! ## Rounding
//!
//! Every figure is formed exactly in wide integers and converted once, so
//! an intermediate never fails when the result is representable:
//!
//! - the gross edge `package * (D * F - D * K - C + P)` (negated for a
//!   conversion; with a perpetual, plus the discounted funding term) is one exact sum of products, rounded **once** to
//!   [`ParityInput::edge_dp`] with [`ParityInput::edge_rounding`];
//! - each leg's fee is an exact product (the two candidates of the capped
//!   basis are compared exactly), rounded **once** to
//!   [`ParityInput::fee_dp`] with [`ParityInput::fee_rounding`];
//! - the fee total and the net edge (`gross - fees`, from the rounded
//!   figures) are exact sums, converted once without rounding.
//!
//! Everything is an estimate at the top of book. An edge is an intent to
//! trade, not an executed or guaranteed profit: the three legs are not
//! executed atomically and can fill partially.

use crate::cost::{Cost, FeeRate};
use crate::error::{ArbitrageError, Result};
use crate::exact;
use crate::instrument::{Action, Instrument, Leg, Timestamp, TopOfBook};
use crate::linear::PerpFunding;
use crate::money::{Currency, Money, Rounding};
use crate::units::{Price, Quantity};
use crate::wide;
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

/// The hedge of the parity package: a dated future, or a perpetual in
/// [`evaluate_synthetic`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityFuture {
    /// The future, dated at the common expiry; or, in
    /// [`evaluate_synthetic`], an undated perpetual.
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
    /// The hedge: a dated future (a perpetual in [`evaluate_synthetic`]).
    pub future: ParityFuture,
    /// Underlying per package, the same on every leg.
    pub package: Quantity,
    /// Decimal places of the fee quantum (6 for `1e-6`).
    pub fee_dp: u32,
    /// How each leg's fee is rounded to the quantum, once.
    pub fee_rounding: Rounding,
    /// Decimal places the gross edge is rounded to, once, from its exact
    /// value (6 for USDC).
    pub edge_dp: u32,
    /// How the gross edge is rounded.
    pub edge_rounding: Rounding,
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
    pub fn call_side(self) -> Action {
        match self {
            Self::Reversal => Action::Buy,
            Self::Conversion => Action::Sell,
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
        side: Action,
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
///   [`ArbitrageError::Inexact`]) only when a figure itself does not fit in
///   a [`Decimal`] (or `edge_dp` or `fee_dp` is above 28); see the module
///   rounding rules.
pub fn evaluate_parity(input: &ParityInput) -> Result<ParityEvaluation> {
    validate(input, None)?;
    let contracts = Contracts {
        call: contracts(input.package, input.call.contract_size, input.call.lot)?,
        put: contracts(input.package, input.put.contract_size, input.put.lot)?,
        future: contracts(input.package, input.future.contract_size, input.future.lot)?,
    };
    Ok(ParityEvaluation {
        reversal: direction(input, &contracts, ParityDirection::Reversal, None)?,
        conversion: direction(input, &contracts, ParityDirection::Conversion, None)?,
    })
}
/// Input to [`evaluate_synthetic`]: a parity package whose hedge is a dated
/// future (no funding) or a perpetual (with its funding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticInput {
    /// The package. With a perpetual hedge, `parity.future` is the
    /// perpetual (undated) and the options' expiry is the horizon.
    pub parity: ParityInput,
    /// The caller's forecast of the perpetual's funding from `as_of` to
    /// the options' expiry: one rate and one mark for `intervals` future
    /// funding intervals (the venue publishes each rate only as it comes).
    /// Required if and only if the hedge is undated.
    pub funding: Option<PerpFunding>,
}

/// Evaluates both directions of the options' synthetic future (call minus
/// put at one strike) against a dated future or a perpetual.
///
/// With a dated future and no funding it is [`evaluate_parity`]. With a
/// perpetual, the perpetual is held to the options' expiry and assumed to
/// close at the settlement index, and its funding `M * rate * n` is added
/// to the hedge price and discounted with the strike:
///
/// - reversal: `Q * (D * (bid_H - K + M * rate * n) - ask_C + bid_P)`;
/// - conversion: `Q * (bid_C - ask_P - D * (ask_H - K + M * rate * n))`.
///
/// Longs pay shorts when the rate is positive, so a positive rate helps the
/// reversal (short perpetual) and costs the conversion. The funding is
/// treated as paid at expiry: discounting each payment at its own date is
/// the caller's refinement.
///
/// Against a perpetual the edge is not locked in as it is against a dated
/// future:
///
/// - closing the perpetual at expiry is a trade whose exit fee and spread
///   are not counted here;
/// - the funding is the caller's forecast (one rate and one mark for `n`
///   future intervals), not a known cashflow;
/// - the perpetual may trade at a basis to the settlement index at expiry,
///   which this formula takes as zero.
///
/// # Errors
///
/// As [`evaluate_parity`], plus [`ArbitrageError::InvalidInput`] if funding
/// is given with a dated hedge, missing with an undated one, or the options
/// are undated.
pub fn evaluate_synthetic(input: &SyntheticInput) -> Result<ParityEvaluation> {
    let p = &input.parity;
    validate(p, input.funding)?;
    let contracts = Contracts {
        call: contracts(p.package, p.call.contract_size, p.call.lot)?,
        put: contracts(p.package, p.put.contract_size, p.put.lot)?,
        future: contracts(p.package, p.future.contract_size, p.future.lot)?,
    };
    Ok(ParityEvaluation {
        reversal: direction(p, &contracts, ParityDirection::Reversal, input.funding)?,
        conversion: direction(p, &contracts, ParityDirection::Conversion, input.funding)?,
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
    funding: Option<PerpFunding>,
) -> Result<DirectionEvaluation> {
    let call_side = dir.call_side();
    let other_side = call_side.opposite();
    let call = match level(
        &input.call.instrument,
        &input.call.book,
        call_side,
        contracts.call,
    )? {
        Ok(p) => p,
        Err(u) => return Ok(DirectionEvaluation::Unavailable(u)),
    };
    let put = match level(
        &input.put.instrument,
        &input.put.book,
        other_side,
        contracts.put,
    )? {
        Ok(p) => p,
        Err(u) => return Ok(DirectionEvaluation::Unavailable(u)),
    };
    let future = match level(
        &input.future.instrument,
        &input.future.book,
        other_side,
        contracts.future,
    )? {
        Ok(p) => p,
        Err(u) => return Ok(DirectionEvaluation::Unavailable(u)),
    };

    // Q * (D * F - D * K - C + P) for a reversal, the negative for a
    // conversion: one exact sum of products, rounded once.
    let (qty, disc, strike) = (input.package.value(), input.discount, input.strike.value());
    let (fut, call_px, put_px) = (future.value(), call.value(), put.value());
    let sign = match dir {
        ParityDirection::Reversal => Decimal::ONE,
        ParityDirection::Conversion => Decimal::NEGATIVE_ONE,
    };
    let neg = exact::neg(sign);
    // Funding of a perpetual hedge, discounted like the strike: the short
    // perpetual of a reversal receives `M * rate * n` when the rate is
    // positive, the long one of a conversion pays it.
    let (f_mark, f_rate, f_n) = funding
        .map_or((Decimal::ZERO, Decimal::ZERO, Decimal::ZERO), |f| {
            (f.mark.value(), f.rate, Decimal::from(f.intervals))
        });
    let gross = wide::sum_products_round(
        &[
            &[sign, qty, disc, fut],
            &[neg, qty, disc, strike],
            &[neg, qty, call_px],
            &[sign, qty, put_px],
            &[sign, qty, disc, f_mark, f_rate, f_n],
        ],
        input.edge_dp,
        input.edge_rounding,
        "parity gross",
    )?;

    let ft = FeeTerms::of(input);
    let call_fee = option_fee(ft, &input.call.fee, call)?;
    let put_fee = option_fee(ft, &input.put.fee, put)?;
    let future_fee = rounded_fee(ft, &[input.future.fee.bps(), BPS, fut, qty])?;
    let total = wide::sum_products_exact(&[&[call_fee], &[put_fee], &[future_fee]], "parity fees")?;
    let net = wide::sum_products_exact(
        &[
            &[gross],
            &[Decimal::NEGATIVE_ONE, call_fee],
            &[Decimal::NEGATIVE_ONE, put_fee],
            &[Decimal::NEGATIVE_ONE, future_fee],
        ],
        "parity net",
    )?;
    let ccy = input.quote_currency;
    let cost = |x| Cost::new(Money::new(x, ccy));

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
        expected_gross: Money::new(gross, ccy),
        expected_fees: ParityFees {
            call: cost(call_fee),
            put: cost(put_fee),
            future: cost(future_fee),
            total: cost(total),
        },
        expected_net: Money::new(net, ccy),
    })))
}

/// The executable price of `side`, if quoted with at least `required`
/// contracts; the inner error says why it is unavailable.
pub(crate) fn level(
    instrument: &Instrument,
    book: &TopOfBook,
    side: Action,
    required: Quantity,
) -> Result<core::result::Result<Price, ParityUnavailable>> {
    let Some(l) = book.executable(side)? else {
        return Ok(Err(ParityUnavailable::MissingQuote {
            instrument: instrument.clone(),
            side,
        }));
    };
    if l.size < required {
        return Ok(Err(ParityUnavailable::InsufficientLiquidity {
            instrument: instrument.clone(),
            available: l.size,
            required,
        }));
    }
    Ok(Ok(l.price))
}

/// `1 / 10_000`: basis points to a fraction, as an exact factor.
pub(crate) const BPS: Decimal = Decimal::from_parts(1, 0, 0, false, 4);

/// What an option or future fee needs besides its rate and price.
#[derive(Clone, Copy)]
pub(crate) struct FeeTerms {
    pub(crate) index: Price,
    pub(crate) package: Quantity,
    pub(crate) fee_dp: u32,
    pub(crate) fee_rounding: Rounding,
}

impl FeeTerms {
    fn of(input: &ParityInput) -> Self {
        Self {
            index: input.index,
            package: input.package,
            fee_dp: input.fee_dp,
            fee_rounding: input.fee_rounding,
        }
    }
}

/// An option leg's fee: the candidates are exact products, compared exactly,
/// and the chosen one is rounded once.
pub(crate) fn option_fee(input: FeeTerms, fee: &OptionFee, price: Price) -> Result<Decimal> {
    let (q, premium_price) = (input.package.value(), price.value());
    match *fee {
        OptionFee::UnderlyingCapped { rate, cap } => {
            let on_underlying = [rate.bps(), BPS, input.index.value(), q];
            let capped = [cap, premium_price, q];
            // on_underlying - capped, exactly: the cap applies when it is
            // smaller.
            let (_, _, cap_is_smaller) = wide::sum_products(
                &[
                    &capped,
                    &[
                        Decimal::NEGATIVE_ONE,
                        rate.bps(),
                        BPS,
                        input.index.value(),
                        q,
                    ],
                ],
                "option fee",
            )?;
            if cap_is_smaller {
                rounded_fee(input, &capped)
            } else {
                rounded_fee(input, &on_underlying)
            }
        }
        OptionFee::Premium { rate } => rounded_fee(input, &[rate.bps(), BPS, premium_price, q]),
    }
}

/// The exact product `factors`, rounded once to the fee quantum.
pub(crate) fn rounded_fee(input: FeeTerms, factors: &[Decimal]) -> Result<Decimal> {
    wide::sum_products_round(&[factors], input.fee_dp, input.fee_rounding, "fee")
}

/// `package / contract_size` contracts, which must be a whole multiple of
/// `lot`.
pub(crate) fn contracts(
    package: Quantity,
    contract_size: Quantity,
    lot: Quantity,
) -> Result<Quantity> {
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

fn validate(input: &ParityInput, funding: Option<PerpFunding>) -> Result<()> {
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
    let expiry = match (f.expiry, funding) {
        (Some(e), None) => e,
        (None, Some(_)) => c.expiry.ok_or(ArbitrageError::InvalidInput {
            reason: "parity options must be dated",
        })?,
        (Some(_), Some(_)) => {
            return Err(ArbitrageError::InvalidInput {
                reason: "funding applies only to a perpetual hedge",
            });
        }
        (None, None) => {
            return Err(ArbitrageError::InvalidInput {
                reason: "parity legs must be dated",
            });
        }
    };
    if c.expiry != Some(expiry) || p.expiry != Some(expiry) {
        return Err(ArbitrageError::InvalidInput {
            reason: "parity legs must share one expiry",
        });
    }
    positive_discount(input.discount)?;
    validate_option_fee(&input.call.fee)?;
    validate_option_fee(&input.put.fee)?;
    // The dated hedge expires with the options; a perpetual never does.
    if f.expiry.is_some() {
        f.ensure_live(input.as_of)
    } else {
        c.ensure_live(input.as_of)
    }
}

pub(crate) fn positive_discount(discount: Decimal) -> Result<()> {
    if discount <= Decimal::ZERO {
        return Err(ArbitrageError::InvalidValue {
            field: "discount factor",
            value: discount,
            reason: "must be positive",
        });
    }
    Ok(())
}

pub(crate) fn validate_option_fee(fee: &OptionFee) -> Result<()> {
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
    if let Some(cap) = cap
        && (cap < Decimal::ZERO || cap > Decimal::ONE)
    {
        return Err(ArbitrageError::InvalidValue {
            field: "option fee cap",
            value: cap,
            reason: "must be within [0, 1]",
        });
    }
    Ok(())
}
