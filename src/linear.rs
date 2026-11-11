//! Executable edges of the linear relations: cash and carry, perpetual
//! basis versus funding, and calendar spreads.
//!
//! Each relation pairs a **near** leg with a **far** leg of the same
//! underlying and trades it in two directions:
//!
//! | Relation | Near leg | Far leg | Function |
//! |---|---|---|---|
//! | cash and carry | spot | dated future | [`evaluate_cash_and_carry`] |
//! | perpetual basis versus funding | spot | perpetual | [`evaluate_perp_basis`] |
//! | calendar | dated future or perpetual | later dated future | [`evaluate_calendar`] |
//!
//! - [`LinearDirection::LongNearShortFar`]: buy the near leg at its ask and
//!   sell the far leg at its bid;
//! - [`LinearDirection::ShortNearLongFar`]: sell the near leg at its bid
//!   and buy the far leg at its ask.
//!
//! With `s = +1` for `LongNearShortFar` and `-1` for `ShortNearLongFar`,
//! `Q` the package, `N` and `F` the executable near and far prices, the
//! gross edge per package is
//!
//! `Q * (s * (F - N) - s * N * r * tau - f * M * rate * n)`
//!
//! - `r * tau` is the carry of the near leg to the far expiry: `r` is a
//!   simple rate per unit of time and `tau` the time to that expiry in the
//!   same unit, both supplied by the caller (no day count or clock is used).
//!   A long near leg pays it; a short near leg earns it. It is zero for the
//!   perpetual basis;
//! - `M * rate * n` is the funding of the perpetual leg: the venue's
//!   published rate per interval `rate`, the mark `M` and a number of
//!   intervals `n`, all supplied by the caller. Longs pay shorts when the
//!   rate is positive, so `f = +1` when the perpetual leg is long and `-1`
//!   when it is short. It is zero when no leg is a perpetual.
//!
//! A package is a quantity of underlying, the same on both legs. Each leg
//! trades `package / contract_size` contracts (the contract multiplier),
//! which must be a whole multiple of its lot. Each leg pays its taker fee
//! rate in basis points of its notional `price * package`; a negative rate
//! is a rebate and gives a negative fee. Only entry fees are counted: the
//! dated legs settle at expiry, and an exit of the perpetual is the caller's
//! assumption.
//!
//! ## Rounding
//!
//! As in [`crate::parity`]: the gross edge is one exact sum of products,
//! rounded once to `edge_dp` with `edge_rounding`; each fee is an exact
//! product rounded once to `fee_dp` with `fee_rounding`; the fee total and
//! the net edge are exact sums of the rounded figures.
//!
//! Everything is an estimate at the top of book. An edge is an intent to
//! trade, not an executed or guaranteed profit: the legs are not executed
//! atomically and can fill partially.

use crate::cost::{Cost, FeeRate};
use crate::error::{ArbitrageError, Result};
use crate::exact;
use crate::instrument::{Action, Instrument, Leg, Timestamp, TopOfBook};
use crate::money::{Currency, Money, Rounding};
use crate::parity::{BPS, contracts};
use crate::units::{Price, Quantity};
use crate::wide;
use rust_decimal::Decimal;

/// One leg of a linear relation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearLeg {
    /// The instrument: undated for spot and perpetuals, dated for futures.
    pub instrument: Instrument,
    /// Its top of book. Prices are per unit of underlying and sizes are in
    /// contracts.
    pub book: TopOfBook,
    /// Units of underlying per contract (the multiplier); 1 for spot.
    pub contract_size: Quantity,
    /// Smallest tradable number of contracts.
    pub lot: Quantity,
    /// Its taker fee, on the notional.
    pub fee: FeeRate,
}

/// The funding of a perpetual leg over the holding window, as published by
/// the venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PerpFunding {
    /// The funding rate per interval, as a fraction (`0.0001` is 1 bp).
    /// Longs pay shorts when it is positive.
    pub rate: Decimal,
    /// The mark price the funding is computed on.
    pub mark: Price,
    /// The number of funding intervals the position is held over.
    pub intervals: u32,
}

/// Rounding and package shared by every relation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LinearTerms {
    /// Time of the quotes.
    pub as_of: Timestamp,
    /// Currency of every price and fee.
    pub quote_currency: Currency,
    /// Underlying per package, the same on both legs.
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

/// Input to [`evaluate_cash_and_carry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CashAndCarryInput {
    /// Package, time, currency and rounding.
    pub terms: LinearTerms,
    /// The spot leg (near), undated.
    pub spot: LinearLeg,
    /// The dated future (far).
    pub future: LinearLeg,
    /// Simple carry rate of the spot leg per unit of time, of any sign: the
    /// financing a long spot leg pays and a short spot leg earns.
    pub carry_rate: Decimal,
    /// Time to the future's expiry in the unit of `carry_rate`; strictly
    /// positive.
    pub time_to_expiry: Decimal,
}

/// Input to [`evaluate_perp_basis`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerpBasisInput {
    /// Package, time, currency and rounding.
    pub terms: LinearTerms,
    /// The spot leg (near), undated.
    pub spot: LinearLeg,
    /// The perpetual (far), undated.
    pub perp: LinearLeg,
    /// The perpetual's funding over the holding window.
    pub funding: PerpFunding,
}

/// Input to [`evaluate_calendar`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarInput {
    /// Package, time, currency and rounding.
    pub terms: LinearTerms,
    /// The near leg: a dated future, or a perpetual (undated).
    pub near: LinearLeg,
    /// The far leg: a future dated after the near leg.
    pub far: LinearLeg,
    /// Simple carry rate of the near leg per unit of time, of any sign.
    pub carry_rate: Decimal,
    /// Time from the near leg's expiry (or from `as_of` for a perpetual
    /// near leg) to the far expiry, in the unit of `carry_rate`; strictly
    /// positive.
    pub time_between: Decimal,
    /// The funding of a perpetual near leg; required if and only if the
    /// near leg is undated.
    pub funding: Option<PerpFunding>,
}

/// Which way the pair trades.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinearDirection {
    /// Buy the near leg, sell the far leg (cash and carry for spot and a
    /// future).
    LongNearShortFar,
    /// Sell the near leg, buy the far leg (reverse cash and carry).
    ShortNearLongFar,
}

impl LinearDirection {
    /// The side of the near leg; the far leg takes the opposite.
    #[must_use]
    pub fn near_side(self) -> Action {
        match self {
            Self::LongNearShortFar => Action::Buy,
            Self::ShortNearLongFar => Action::Sell,
        }
    }

    fn sign(self) -> Decimal {
        match self {
            Self::LongNearShortFar => Decimal::ONE,
            Self::ShortNearLongFar => Decimal::NEGATIVE_ONE,
        }
    }
}

/// The two legs, sized in contracts at executable prices.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LinearLegs {
    /// The near leg.
    pub near: Leg,
    /// The far leg.
    pub far: Leg,
}

/// The fee of each leg, each rounded once, and their sum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LinearFees {
    /// Fee of the near leg.
    pub near: Cost,
    /// Fee of the far leg.
    pub far: Cost,
    /// `near + far`.
    pub total: Cost,
}

/// The executable edge of one direction for one package, in the quote
/// currency.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LinearEdge {
    /// The direction.
    pub direction: LinearDirection,
    /// Time of the quotes.
    pub as_of: Timestamp,
    /// The legs to send.
    pub legs: LinearLegs,
    /// The gross edge, after carry and funding and before fees.
    pub expected_gross: Money,
    /// The fees of both legs.
    pub expected_fees: LinearFees,
    /// `expected_gross - expected_fees.total`.
    pub expected_net: Money,
}

/// Why one direction has no executable edge.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LinearUnavailable {
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
pub enum LinearDirectionEvaluation {
    /// The edge, which may be negative.
    Edge(Box<LinearEdge>),
    /// No edge can be computed.
    Unavailable(LinearUnavailable),
}

impl LinearDirectionEvaluation {
    /// The edge if one was computed.
    #[must_use]
    pub fn edge(&self) -> Option<&LinearEdge> {
        match self {
            Self::Edge(e) => Some(e.as_ref()),
            Self::Unavailable(_) => None,
        }
    }
}

/// Both directions of a relation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LinearEvaluation {
    /// Buy near, sell far.
    pub long_near: LinearDirectionEvaluation,
    /// Sell near, buy far.
    pub short_near: LinearDirectionEvaluation,
}

impl LinearEvaluation {
    /// The direction with the larger net edge, of those that could be
    /// computed. On a tie `long_near` is returned. The edge may be negative;
    /// comparing it with a tolerance is the caller's call.
    #[must_use]
    pub fn executable(&self) -> Option<&LinearEdge> {
        match (self.long_near.edge(), self.short_near.edge()) {
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

/// Evaluates both directions of cash and carry: spot against a dated
/// future, with the spot leg's carry to expiry.
///
/// # Errors
///
/// - [`ArbitrageError::InvalidInput`] if both legs are the same instrument,
///   the spot leg is dated, the future is undated, or the package is not a
///   whole multiple of a leg's lot in contracts.
/// - [`ArbitrageError::InvalidValue`] if `time_to_expiry` is not positive.
/// - [`ArbitrageError::Expired`] if the future has expired at `as_of`.
/// - Arithmetic errors only when a figure itself does not fit in a
///   [`Decimal`] (or `edge_dp` or `fee_dp` is above 28).
pub fn evaluate_cash_and_carry(input: &CashAndCarryInput) -> Result<LinearEvaluation> {
    distinct(&input.spot, &input.future)?;
    if input.spot.instrument.expiry.is_some() {
        return Err(ArbitrageError::InvalidInput {
            reason: "the spot leg must be undated",
        });
    }
    if input.future.instrument.expiry.is_none() {
        return Err(ArbitrageError::InvalidInput {
            reason: "the future leg must be dated",
        });
    }
    positive("time to expiry", input.time_to_expiry)?;
    input.future.instrument.ensure_live(input.terms.as_of)?;
    evaluate(
        &input.terms,
        &input.spot,
        &input.future,
        Carry {
            rate: input.carry_rate,
            time: input.time_to_expiry,
        },
        None,
    )
}

/// Evaluates both directions of the perpetual basis against the funding
/// the perpetual leg pays or receives over the holding window.
///
/// The basis is assumed to close to zero at exit; the funding is the
/// venue's rate, supplied by the caller. [`crate::basis`] covers the same
/// pair with an explicit exit basis, funding schedule and exit fees.
///
/// # Errors
///
/// - [`ArbitrageError::InvalidInput`] if both legs are the same instrument,
///   a leg is dated, or the package is not a whole multiple of a leg's lot
///   in contracts.
/// - Arithmetic errors as for [`evaluate_cash_and_carry`].
pub fn evaluate_perp_basis(input: &PerpBasisInput) -> Result<LinearEvaluation> {
    distinct(&input.spot, &input.perp)?;
    if input.spot.instrument.expiry.is_some() || input.perp.instrument.expiry.is_some() {
        return Err(ArbitrageError::InvalidInput {
            reason: "spot and perpetual legs must be undated",
        });
    }
    evaluate(
        &input.terms,
        &input.spot,
        &input.perp,
        Carry {
            rate: Decimal::ZERO,
            time: Decimal::ZERO,
        },
        Some((FundedLeg::Far, input.funding)),
    )
}

/// Evaluates both directions of a calendar spread: a near dated future (or
/// a perpetual) against a later dated future.
///
/// # Errors
///
/// - [`ArbitrageError::InvalidInput`] if both legs are the same instrument,
///   the far leg is undated, a dated near leg does not expire before the far
///   leg, funding is missing for a perpetual near leg or given for a dated
///   one, or the package is not a whole multiple of a leg's lot in
///   contracts.
/// - [`ArbitrageError::InvalidValue`] if `time_between` is not positive.
/// - [`ArbitrageError::Expired`] if a leg has expired at `as_of`.
/// - Arithmetic errors as for [`evaluate_cash_and_carry`].
pub fn evaluate_calendar(input: &CalendarInput) -> Result<LinearEvaluation> {
    distinct(&input.near, &input.far)?;
    let Some(far_expiry) = input.far.instrument.expiry else {
        return Err(ArbitrageError::InvalidInput {
            reason: "the far leg must be dated",
        });
    };
    match (input.near.instrument.expiry, input.funding) {
        (Some(near_expiry), None) => {
            if near_expiry >= far_expiry {
                return Err(ArbitrageError::InvalidInput {
                    reason: "the near leg must expire before the far leg",
                });
            }
        }
        (None, Some(_)) => {}
        (Some(_), Some(_)) => {
            return Err(ArbitrageError::InvalidInput {
                reason: "funding applies only to a perpetual near leg",
            });
        }
        (None, None) => {
            return Err(ArbitrageError::InvalidInput {
                reason: "a perpetual near leg needs its funding",
            });
        }
    }
    positive("time between expiries", input.time_between)?;
    input.near.instrument.ensure_live(input.terms.as_of)?;
    input.far.instrument.ensure_live(input.terms.as_of)?;
    evaluate(
        &input.terms,
        &input.near,
        &input.far,
        Carry {
            rate: input.carry_rate,
            time: input.time_between,
        },
        input.funding.map(|f| (FundedLeg::Near, f)),
    )
}

#[derive(Clone, Copy)]
struct Carry {
    rate: Decimal,
    time: Decimal,
}

#[derive(Clone, Copy)]
enum FundedLeg {
    Near,
    Far,
}

fn distinct(near: &LinearLeg, far: &LinearLeg) -> Result<()> {
    if near.instrument.id == far.instrument.id {
        return Err(ArbitrageError::InvalidInput {
            reason: "both legs are the same instrument",
        });
    }
    Ok(())
}

fn positive(field: &'static str, value: Decimal) -> Result<()> {
    if value <= Decimal::ZERO {
        return Err(ArbitrageError::InvalidValue {
            field,
            value,
            reason: "must be positive",
        });
    }
    Ok(())
}

fn evaluate(
    terms: &LinearTerms,
    near: &LinearLeg,
    far: &LinearLeg,
    carry: Carry,
    funding: Option<(FundedLeg, PerpFunding)>,
) -> Result<LinearEvaluation> {
    let near_n = contracts(terms.package, near.contract_size, near.lot)?;
    let far_n = contracts(terms.package, far.contract_size, far.lot)?;
    let pair = Pair {
        terms,
        near,
        far,
        near_n,
        far_n,
        carry,
        funding,
    };
    Ok(LinearEvaluation {
        long_near: pair.direction(LinearDirection::LongNearShortFar)?,
        short_near: pair.direction(LinearDirection::ShortNearLongFar)?,
    })
}

struct Pair<'a> {
    terms: &'a LinearTerms,
    near: &'a LinearLeg,
    far: &'a LinearLeg,
    near_n: Quantity,
    far_n: Quantity,
    carry: Carry,
    funding: Option<(FundedLeg, PerpFunding)>,
}

impl Pair<'_> {
    fn direction(&self, dir: LinearDirection) -> Result<LinearDirectionEvaluation> {
        let near_side = dir.near_side();
        let far_side = near_side.opposite();
        let near_px = match level(self.near, near_side, self.near_n)? {
            Ok(p) => p,
            Err(u) => return Ok(LinearDirectionEvaluation::Unavailable(u)),
        };
        let far_px = match level(self.far, far_side, self.far_n)? {
            Ok(p) => p,
            Err(u) => return Ok(LinearDirectionEvaluation::Unavailable(u)),
        };

        let terms = self.terms;
        let (qty, near_v, far_v) = (terms.package.value(), near_px.value(), far_px.value());
        let s = dir.sign();
        let neg_s = exact::neg(s);
        // Long funded leg pays (f = +1): the near leg's sign is s, the far
        // leg's is -s. The term enters the gross with -f.
        let (funding_sign, f_rate, f_mark, f_n) = match self.funding {
            Some((FundedLeg::Near, p)) => {
                (neg_s, p.rate, p.mark.value(), Decimal::from(p.intervals))
            }
            Some((FundedLeg::Far, p)) => (s, p.rate, p.mark.value(), Decimal::from(p.intervals)),
            None => (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO, Decimal::ZERO),
        };
        // Q * (s*F - s*N - s*N*r*tau - f*M*rate*n): one exact sum of
        // products, rounded once.
        let gross = wide::sum_products_round(
            &[
                &[s, qty, far_v],
                &[neg_s, qty, near_v],
                &[neg_s, qty, near_v, self.carry.rate, self.carry.time],
                &[funding_sign, qty, f_mark, f_rate, f_n],
            ],
            terms.edge_dp,
            terms.edge_rounding,
            "linear gross",
        )?;
        let near_fee = fee(terms, self.near.fee, near_v)?;
        let far_fee = fee(terms, self.far.fee, far_v)?;
        let total = wide::sum(&[near_fee, far_fee], "linear fees")?;
        let net = wide::sum_products_exact(
            &[
                &[gross],
                &[Decimal::NEGATIVE_ONE, near_fee],
                &[Decimal::NEGATIVE_ONE, far_fee],
            ],
            "linear net",
        )?;
        let ccy = terms.quote_currency;
        let cost = |x| Cost::new(Money::new(x, ccy));
        Ok(LinearDirectionEvaluation::Edge(Box::new(LinearEdge {
            direction: dir,
            as_of: terms.as_of,
            legs: LinearLegs {
                near: Leg {
                    instrument: self.near.instrument.clone(),
                    side: near_side,
                    quantity: self.near_n,
                    price: near_px,
                },
                far: Leg {
                    instrument: self.far.instrument.clone(),
                    side: far_side,
                    quantity: self.far_n,
                    price: far_px,
                },
            },
            expected_gross: Money::new(gross, ccy),
            expected_fees: LinearFees {
                near: cost(near_fee),
                far: cost(far_fee),
                total: cost(total),
            },
            expected_net: Money::new(net, ccy),
        })))
    }
}

/// `rate * price * package`, rounded once to the fee quantum.
fn fee(terms: &LinearTerms, rate: FeeRate, price: Decimal) -> Result<Decimal> {
    wide::sum_products_round(
        &[&[rate.bps(), BPS, price, terms.package.value()]],
        terms.fee_dp,
        terms.fee_rounding,
        "fee",
    )
}

/// The executable price of `side`, if quoted with at least `required`
/// contracts; the inner error says why it is unavailable.
fn level(
    leg: &LinearLeg,
    side: Action,
    required: Quantity,
) -> Result<core::result::Result<Price, LinearUnavailable>> {
    let Some(l) = leg.book.executable(side)? else {
        return Ok(Err(LinearUnavailable::MissingQuote {
            instrument: leg.instrument.clone(),
            side,
        }));
    };
    if l.size < required {
        return Ok(Err(LinearUnavailable::InsufficientLiquidity {
            instrument: leg.instrument.clone(),
            available: l.size,
            required,
        }));
    }
    Ok(Ok(l.price))
}
