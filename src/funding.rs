//! Perpetual funding: conventions, rates, schedules and cashflows.
//!
//! ## Conventions declared here
//!
//! - **Contract:** [`PerpConvention::Linear`]. The position is in base units
//!   (contracts times multiplier, converted by the caller), prices are in the
//!   quote currency, and funding is paid in the quote currency.
//! - **Payment base:** the perpetual's **mark** price at the funding instant,
//!   not the index and not an executable price. All three are separate
//!   inputs in this crate.
//! - **Amount and sign:** `position * mark * rate`, as a [`Cost`] (positive is
//!   paid). Longs pay shorts when the rate is positive; shorts pay longs when
//!   it is negative.
//! - **Interval:** a rate is per funding interval, not annualized. Funding is
//!   a snapshot: a position held at a funding instant pays or receives the
//!   whole interval, with no proration by holding time.
//! - **Rounding:** [`funding_cashflow`] returns the exact amount. The product
//!   `position * mark * rate` is formed in 768-bit integers, so nothing is
//!   rounded between the two multiplications. A venue's settlement quantum is
//!   applied with [`funding_cashflow_rounded`]: **one** rounding of that
//!   exact product, to [`SettlementRounding::dp`] decimal places in the quote
//!   currency, with an explicit [`Rounding`] mode (for example half-even, as
//!   a venue may specify). Rounding the two-step product, or rounding an
//!   already-rounded amount again, is never done.
//!
//! ## Estimated and published rates
//!
//! A [`RateKind::Published`] rate was fixed by the venue and is known from
//! its publication time; an [`RateKind::Estimated`] rate is the caller's
//! forecast. A decision at time `t` may use a published rate only if it was
//! published at or before `t`; anything else is look-ahead and is rejected
//! (see [`FundingRate::check_known_at`]).

use crate::cost::Cost;
use crate::error::{ArbitrageError, Result};
use crate::exact;
use crate::instrument::Timestamp;
use crate::money::Rounding;
use crate::money::{Currency, Money};
use crate::units::Price;
use crate::wide;
use rust_decimal::Decimal;

/// Most funding instants [`FundingSchedule::instants`] returns at once.
pub const MAX_FUNDING_INSTANTS: usize = 100_000;

/// How a perpetual's funding is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PerpConvention {
    /// Linear (quote-margined) perpetual: `position (base units) * mark *
    /// rate`, paid in the quote currency.
    Linear,
}

/// Where a funding rate comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateKind {
    /// The caller's forecast for a future funding instant.
    Estimated,
    /// Fixed by the venue and known from `at`.
    Published {
        /// When the venue published the rate.
        at: Timestamp,
    },
}

/// A funding rate for one interval, as a fraction (`0.0001` is 1 bp).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FundingRate {
    rate: Decimal,
    kind: RateKind,
}

impl FundingRate {
    /// A rate of `rate` per interval.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidValue`] if `|rate| > 1` (100% per interval).
    pub fn new(rate: Decimal, kind: RateKind) -> Result<Self> {
        if rate.abs() > Decimal::ONE {
            return Err(ArbitrageError::InvalidValue {
                field: "funding rate",
                value: rate,
                reason: "must be within [-1, 1] per interval",
            });
        }
        Ok(Self {
            rate: rate.normalize(),
            kind,
        })
    }

    /// The rate per interval.
    #[must_use]
    pub fn rate(&self) -> Decimal {
        self.rate
    }

    /// Where it comes from.
    #[must_use]
    pub fn kind(&self) -> RateKind {
        self.kind
    }

    /// Checks that a decision at `as_of` may use this rate: an estimate
    /// always, a published rate only once it is published.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::LookAhead`] if the rate is published after `as_of`.
    pub fn check_known_at(&self, as_of: Timestamp) -> Result<()> {
        match self.kind {
            RateKind::Published { at } if at > as_of => Err(ArbitrageError::LookAhead {
                known_at: at,
                as_of,
            }),
            _ => Ok(()),
        }
    }
}

/// One funding instant with the rate and mark assumed for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FundingEvent {
    /// The funding instant.
    pub time: Timestamp,
    /// The rate for the interval ending at `time`.
    pub rate: FundingRate,
    /// The mark price the payment is computed on. For a future instant this
    /// is an assumption, typically the current mark.
    pub mark: Price,
}

/// Exact funding paid (positive) or received (negative) by a position at one
/// funding instant: `position * mark * rate`, with no rounding.
///
/// ```
/// use arbitrage_rs::funding::{funding_cashflow, FundingEvent, FundingRate, PerpConvention, RateKind};
/// use arbitrage_rs::{Currency, Price, Timestamp};
/// use rust_decimal_macros::dec;
///
/// let usd = Currency::new("USD").unwrap();
/// let event = FundingEvent {
///     time: Timestamp::from_millis(28_800_000),
///     rate: FundingRate::new(dec!(0.0001), RateKind::Estimated).unwrap(),
///     mark: Price::new(dec!(30000)).unwrap(),
/// };
/// // A long of 2 pays 2 * 30000 * 0.0001 = 6.
/// let paid = funding_cashflow(PerpConvention::Linear, dec!(2), &event, usd).unwrap();
/// assert_eq!(paid.money().amount(), dec!(6));
/// ```
///
/// # Errors
///
/// [`ArbitrageError::Overflow`] if the amount is out of range and
/// [`ArbitrageError::Inexact`] if no `Decimal` equals it (more than 28
/// decimal places); [`funding_cashflow_rounded`] settles such amounts.
pub fn funding_cashflow(
    convention: PerpConvention,
    position: Decimal,
    event: &FundingEvent,
    settlement: Currency,
) -> Result<Cost> {
    let (m, scale, negative) = exact_product(convention, position, event)?;
    let amount = wide::to_decimal(m, scale, negative, "funding")?;
    Ok(Cost::new(Money::new(amount, settlement)))
}

/// How a venue settles funding amounts: decimal places of the quote
/// currency and the rounding mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SettlementRounding {
    dp: u32,
    mode: Rounding,
}

impl SettlementRounding {
    /// Settle to `dp` decimal places with `mode`.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidInput`] if `dp` is above 28.
    pub fn new(dp: u32, mode: Rounding) -> Result<Self> {
        if dp > exact::MAX_SCALE {
            return Err(ArbitrageError::InvalidInput {
                reason: "settlement decimal places must be at most 28",
            });
        }
        Ok(Self { dp, mode })
    }

    /// Decimal places of the settled amount.
    #[must_use]
    pub fn dp(&self) -> u32 {
        self.dp
    }

    /// Rounding mode.
    #[must_use]
    pub fn mode(&self) -> Rounding {
        self.mode
    }
}

/// Funding as a venue settles it: the exact `position * mark * rate` rounded
/// **once** to `rounding.dp()` decimal places with `rounding.mode()`.
///
/// ```
/// use arbitrage_rs::funding::{
///     funding_cashflow_rounded, FundingEvent, FundingRate, PerpConvention, RateKind,
///     SettlementRounding,
/// };
/// use arbitrage_rs::{Currency, Price, Rounding, Timestamp};
/// use rust_decimal_macros::dec;
///
/// let usd = Currency::new("USD").unwrap();
/// let event = FundingEvent {
///     time: Timestamp::from_millis(28_800_000),
///     rate: FundingRate::new(dec!(0.000125), RateKind::Estimated).unwrap(),
///     mark: Price::new(dec!(12345.67)).unwrap(),
/// };
/// // Exact: 0.001 * 12345.67 * 0.000125 = 0.00154320875.
/// let cents = SettlementRounding::new(2, Rounding::HalfEven).unwrap();
/// let paid = funding_cashflow_rounded(PerpConvention::Linear, dec!(0.001), &event, usd, cents)
///     .unwrap();
/// assert_eq!(paid.money().amount(), dec!(0));
/// let up = SettlementRounding::new(2, Rounding::Up).unwrap();
/// let paid = funding_cashflow_rounded(PerpConvention::Linear, dec!(0.001), &event, usd, up)
///     .unwrap();
/// assert_eq!(paid.money().amount(), dec!(0.01));
/// ```
///
/// # Errors
///
/// [`ArbitrageError::Overflow`] if the settled amount is out of range and
/// [`ArbitrageError::Inexact`] if it needs more than 96 bits of mantissa at
/// `rounding.dp()` places (no `Decimal` holds it).
pub fn funding_cashflow_rounded(
    convention: PerpConvention,
    position: Decimal,
    event: &FundingEvent,
    settlement: Currency,
    rounding: SettlementRounding,
) -> Result<Cost> {
    let (m, scale, negative) = exact_product(convention, position, event)?;
    let (m, scale) = wide::round_to_dp(m, scale, negative, rounding.dp, rounding.mode)?;
    let amount = wide::to_decimal(m, scale, negative, "funding")?;
    Ok(Cost::new(Money::new(amount, settlement)))
}

fn exact_product(
    convention: PerpConvention,
    position: Decimal,
    event: &FundingEvent,
) -> Result<(wide::U768, u32, bool)> {
    match convention {
        PerpConvention::Linear => wide::product(
            &[position, event.mark.value(), event.rate.rate()],
            "funding",
        ),
    }
}

/// Funding instants `anchor + k * interval` for every integer `k`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FundingSchedule {
    anchor: Timestamp,
    interval_ms: i64,
}

impl FundingSchedule {
    /// A grid through `anchor` with `interval_ms` between instants, for
    /// example `00:00 UTC` and 8 hours.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidInput`] if `interval_ms` is not positive.
    pub fn new(anchor: Timestamp, interval_ms: i64) -> Result<Self> {
        if interval_ms <= 0 {
            return Err(ArbitrageError::InvalidInput {
                reason: "funding interval must be positive",
            });
        }
        Ok(Self {
            anchor,
            interval_ms,
        })
    }

    /// Interval between funding instants, in milliseconds.
    #[must_use]
    pub fn interval_ms(&self) -> i64 {
        self.interval_ms
    }

    /// The funding instants in `(after, until]`, in order.
    ///
    /// # Errors
    ///
    /// - [`ArbitrageError::InvalidInput`] if `until < after` or there are
    ///   more than [`MAX_FUNDING_INSTANTS`].
    /// - [`ArbitrageError::Overflow`] if an instant is outside `i64`
    ///   milliseconds.
    pub fn instants(&self, after: Timestamp, until: Timestamp) -> Result<Vec<Timestamp>> {
        if until < after {
            return Err(ArbitrageError::InvalidInput {
                reason: "funding window ends before it starts",
            });
        }
        let overflow = || ArbitrageError::Overflow { op: "time" };
        let offset = i128::from(after.millis()) - i128::from(self.anchor.millis());
        let interval = i128::from(self.interval_ms);
        // First k with anchor + k * interval > after.
        let mut k = offset.div_euclid(interval) + 1;
        let mut out = Vec::new();
        loop {
            let t = i128::from(self.anchor.millis()) + k * interval;
            if t > i128::from(until.millis()) {
                break;
            }
            if out.len() == MAX_FUNDING_INSTANTS {
                return Err(ArbitrageError::InvalidInput {
                    reason: "too many funding instants in the window",
                });
            }
            out.push(Timestamp::from_millis(
                i64::try_from(t).map_err(|_| overflow())?,
            ));
            k += 1;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    const H8: i64 = 8 * 3_600_000;

    fn usd() -> Currency {
        Currency::new("USD").unwrap()
    }

    fn event(rate: Decimal, mark: Decimal) -> FundingEvent {
        FundingEvent {
            time: Timestamp::from_millis(H8),
            rate: FundingRate::new(rate, RateKind::Estimated).unwrap(),
            mark: Price::new(mark).unwrap(),
        }
    }

    fn paid(position: Decimal, rate: Decimal, mark: Decimal) -> Decimal {
        funding_cashflow(PerpConvention::Linear, position, &event(rate, mark), usd())
            .unwrap()
            .money()
            .amount()
    }

    #[test]
    fn sign_table() {
        // 1.5 * 30000.5 = 45000.75; * 0.0001 = 4.500075.
        assert_eq!(paid(dec!(1.5), dec!(0.0001), dec!(30000.5)), dec!(4.500075));
        assert_eq!(
            paid(dec!(-1.5), dec!(0.0001), dec!(30000.5)),
            dec!(-4.500075)
        );
        assert_eq!(
            paid(dec!(1.5), dec!(-0.0001), dec!(30000.5)),
            dec!(-4.500075)
        );
        assert_eq!(
            paid(dec!(-1.5), dec!(-0.0001), dec!(30000.5)),
            dec!(4.500075)
        );
        assert_eq!(paid(dec!(0), dec!(0.0001), dec!(30000.5)), dec!(0));
        assert_eq!(paid(dec!(1.5), dec!(0), dec!(30000.5)), dec!(0));
    }

    #[test]
    fn cashflow_is_exact_and_tagged() {
        // 0.001 * 12345.67 * 0.000125 = 0.00154320875.
        let c = funding_cashflow(
            PerpConvention::Linear,
            dec!(0.001),
            &event(dec!(0.000125), dec!(12345.67)),
            usd(),
        )
        .unwrap();
        assert_eq!(c.money(), Money::new(dec!(0.00154320875), usd()));
    }

    #[test]
    fn exact_product_needs_no_intermediate_rounding() {
        // 0.00000001 * 0.12345678901234 * 0.0000012345 has scale 8 + 14 + 10
        // = 32 before stripping: 1.524074059...e-15, with 30 significant
        // places, so the exact amount is not a Decimal, while the settled
        // one is.
        let e = FundingEvent {
            time: Timestamp::from_millis(H8),
            rate: FundingRate::new(dec!(0.0000012345), RateKind::Estimated).unwrap(),
            mark: Price::new(dec!(0.12345678901234)).unwrap(),
        };
        assert_eq!(
            funding_cashflow(PerpConvention::Linear, dec!(0.00000001), &e, usd()),
            Err(ArbitrageError::Inexact { op: "funding" })
        );
        let r = SettlementRounding::new(18, Rounding::HalfEven).unwrap();
        let c = funding_cashflow_rounded(PerpConvention::Linear, dec!(0.00000001), &e, usd(), r)
            .unwrap();
        // 1.5240740603673373e-15 exact digits: 0.000000000000001524074060367...
        assert_eq!(c.money().amount(), dec!(0.000000000000001524));
    }

    #[test]
    fn settlement_rounding_modes_and_signs() {
        // Exact 4.500075 paid by a long, -4.500075 received by a short.
        let e = event(dec!(0.0001), dec!(30000.5));
        let settle = |pos, dp, mode| {
            funding_cashflow_rounded(
                PerpConvention::Linear,
                pos,
                &e,
                usd(),
                SettlementRounding::new(dp, mode).unwrap(),
            )
            .unwrap()
            .money()
            .amount()
        };
        assert_eq!(settle(dec!(1.5), 2, Rounding::HalfEven), dec!(4.5));
        assert_eq!(settle(dec!(1.5), 2, Rounding::Up), dec!(4.51));
        assert_eq!(settle(dec!(1.5), 2, Rounding::Down), dec!(4.5));
        assert_eq!(settle(dec!(-1.5), 2, Rounding::Up), dec!(-4.5));
        assert_eq!(settle(dec!(-1.5), 2, Rounding::Down), dec!(-4.51));
        assert_eq!(settle(dec!(-1.5), 2, Rounding::TowardZero), dec!(-4.5));
        // Exactly half: 0.125 at 2 places, half-even gives 0.12.
        let half = event(dec!(0.000125), dec!(1000));
        let c = funding_cashflow_rounded(
            PerpConvention::Linear,
            dec!(1),
            &half,
            usd(),
            SettlementRounding::new(2, Rounding::HalfEven).unwrap(),
        )
        .unwrap();
        assert_eq!(c.money().amount(), dec!(0.12));
        assert!(SettlementRounding::new(29, Rounding::Up).is_err());
    }

    #[test]
    fn rate_bounds_and_look_ahead() {
        assert!(FundingRate::new(dec!(1), RateKind::Estimated).is_ok());
        assert!(FundingRate::new(dec!(-1.0001), RateKind::Estimated).is_err());
        let t = Timestamp::from_millis(1_000);
        let published = FundingRate::new(
            dec!(0.0001),
            RateKind::Published {
                at: Timestamp::from_millis(1_000),
            },
        )
        .unwrap();
        assert!(published.check_known_at(t).is_ok());
        assert_eq!(
            published.check_known_at(Timestamp::from_millis(999)),
            Err(ArbitrageError::LookAhead {
                known_at: t,
                as_of: Timestamp::from_millis(999)
            })
        );
        let estimate = FundingRate::new(dec!(0.0001), RateKind::Estimated).unwrap();
        assert!(estimate.check_known_at(Timestamp::from_millis(0)).is_ok());
    }

    #[test]
    fn schedule_instants_are_left_open_right_closed() {
        let s = FundingSchedule::new(Timestamp::from_millis(0), H8).unwrap();
        let ts = |v: &[i64]| {
            v.iter()
                .map(|m| Timestamp::from_millis(*m))
                .collect::<Vec<_>>()
        };
        // (0, 24h]: 8h, 16h, 24h; the instant at 0 is excluded.
        assert_eq!(
            s.instants(Timestamp::from_millis(0), Timestamp::from_millis(3 * H8))
                .unwrap(),
            ts(&[H8, 2 * H8, 3 * H8])
        );
        // (1ms, 8h - 1ms]: none.
        assert!(
            s.instants(Timestamp::from_millis(1), Timestamp::from_millis(H8 - 1))
                .unwrap()
                .is_empty()
        );
        // Before the anchor.
        assert_eq!(
            s.instants(Timestamp::from_millis(-H8 - 1), Timestamp::from_millis(0))
                .unwrap(),
            ts(&[-H8, 0])
        );
        // Off-grid anchor.
        let s = FundingSchedule::new(Timestamp::from_millis(5), 10).unwrap();
        assert_eq!(
            s.instants(Timestamp::from_millis(5), Timestamp::from_millis(25))
                .unwrap(),
            ts(&[15, 25])
        );
    }

    #[test]
    fn schedule_errors() {
        assert!(FundingSchedule::new(Timestamp::from_millis(0), 0).is_err());
        let s = FundingSchedule::new(Timestamp::from_millis(0), 1).unwrap();
        assert!(matches!(
            s.instants(Timestamp::from_millis(10), Timestamp::from_millis(9)),
            Err(ArbitrageError::InvalidInput { .. })
        ));
        assert!(matches!(
            s.instants(Timestamp::from_millis(0), Timestamp::from_millis(200_000)),
            Err(ArbitrageError::InvalidInput { .. })
        ));
        let s = FundingSchedule::new(Timestamp::from_millis(i64::MAX - 1), i64::MAX).unwrap();
        assert_eq!(
            s.instants(
                Timestamp::from_millis(i64::MAX - 1),
                Timestamp::from_millis(i64::MAX)
            )
            .unwrap(),
            vec![]
        );
    }
}
