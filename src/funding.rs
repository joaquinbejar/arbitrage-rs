//! Perpetual funding: conventions, rates, schedules and cashflows.
//!
//! ## Conventions declared here
//!
//! - **Contract:** [`PerpConvention::Linear`]. The position is in base units
//!   (contracts times multiplier, converted by the caller), prices are in the
//!   quote currency, and funding is paid in the quote currency.
//!   [`PerpConvention::Inverse`]: the position is in contracts, each worth a
//!   fixed [`ContractValue`] in the quote currency ("face"), prices are in
//!   the quote currency per unit of base, and funding is paid in the **base**
//!   currency: `contracts * contract_value / mark * rate`.
//! - **Payment base:** the perpetual's **mark** price at the funding instant,
//!   not the index and not an executable price. All three are separate
//!   inputs in this crate.
//! - **Amount and sign:** `position * mark * rate` (linear) or
//!   `contracts * contract_value / mark * rate` (inverse), as a [`Cost`]
//!   (positive is paid). Longs pay shorts when the rate is positive; shorts
//!   pay longs when it is negative. A ledger that books cash received as
//!   positive posts the negated amount.
//! - **Interval:** a rate is per funding interval, not annualized. Funding is
//!   a snapshot: a position held at a funding instant pays or receives the
//!   whole interval, with no proration by holding time.
//! - **Rounding:** [`funding_cashflow`] returns the exact amount. The product
//!   `position * mark * rate` is formed in 768-bit integers, so nothing is
//!   rounded between the two multiplications; the inverse amount is one exact
//!   quotient of `contracts * contract_value * rate` by `mark` in the same
//!   integers, so it usually has no exact `Decimal` and must be settled. A venue's settlement quantum is
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

/// The value of one inverse contract in the quote currency ("face"), for
/// example 100 USD: strictly positive. It is an amount per contract, not a
/// price per unit of base, so it has its own type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContractValue(Decimal);

impl ContractValue {
    /// A contract value of `value` quote units per contract.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidValue`] unless `value` is positive.
    pub fn new(value: Decimal) -> Result<Self> {
        if value > Decimal::ZERO {
            Ok(Self(value.normalize()))
        } else {
            Err(ArbitrageError::InvalidValue {
                field: "contract value",
                value,
                reason: "must be positive",
            })
        }
    }

    /// Quote units per contract.
    #[must_use]
    #[inline]
    pub fn value(&self) -> Decimal {
        self.0
    }
}

/// How a perpetual's funding is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PerpConvention {
    /// Linear (quote-margined) perpetual: `position (base units) * mark *
    /// rate`, paid in the quote currency.
    Linear,
    /// Inverse (base-margined) perpetual, paid in the `base` currency:
    /// `contracts * contract_value / mark * rate`. The position is a signed
    /// number of contracts (long positive) and `mark` is in the quote
    /// currency per unit of base.
    ///
    /// ```
    /// use arbitrage_rs::funding::{
    ///     funding_cashflow_rounded, ContractValue, FundingEvent, FundingRate, PerpConvention,
    ///     RateKind, SettlementRounding,
    /// };
    /// use arbitrage_rs::{Currency, Price, Rounding, Timestamp};
    /// use rust_decimal_macros::dec;
    ///
    /// let btc = Currency::new("BTC").unwrap();
    /// let inverse = PerpConvention::Inverse {
    ///     contract_value: ContractValue::new(dec!(100)).unwrap(),
    ///     base: btc,
    /// };
    /// let event = FundingEvent {
    ///     time: Timestamp::from_millis(28_800_000),
    ///     rate: FundingRate::new(dec!(0.0001), RateKind::Estimated).unwrap(),
    ///     mark: Price::new(dec!(30000)).unwrap(),
    /// };
    /// // A long of 1 contract pays 100 / 30000 * 0.0001 = 0.000000333... BTC,
    /// // settled once to the satoshi.
    /// let sats = SettlementRounding::new(8, Rounding::HalfEven).unwrap();
    /// let paid = funding_cashflow_rounded(inverse, dec!(1), &event, btc, sats).unwrap();
    /// assert_eq!(paid.money().amount(), dec!(0.00000033));
    /// ```
    Inverse {
        /// Quote units per contract.
        contract_value: ContractValue,
        /// The base currency, in which funding is paid; a funding call with
        /// another settlement currency is rejected.
        base: Currency,
    },
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
/// funding instant: `position * mark * rate` (linear) or `contracts *
/// contract_value / mark * rate` (inverse, in the base currency), with no
/// rounding.
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
/// - [`ArbitrageError::Overflow`] if the amount is out of range.
/// - [`ArbitrageError::Inexact`] if no `Decimal` equals it (more than 28
///   decimal places, the usual case for an inverse quotient);
///   [`funding_cashflow_rounded`] settles such amounts.
/// - [`ArbitrageError::CurrencyMismatch`] if an inverse convention's base
///   currency is not `settlement`.
pub fn funding_cashflow(
    convention: PerpConvention,
    position: Decimal,
    event: &FundingEvent,
    settlement: Currency,
) -> Result<Cost> {
    let amount = match convention {
        PerpConvention::Linear => {
            let (m, scale, negative) = linear_product(position, event)?;
            wide::to_decimal(m, scale, negative, "funding")?
        }
        PerpConvention::Inverse {
            contract_value,
            base,
        } => {
            check_settlement(base, settlement)?;
            wide::quotient_exact(
                &[position, contract_value.value(), event.rate.rate()],
                event.mark.value(),
                "funding",
            )?
        }
    };
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

/// Funding as a venue settles it: the exact `position * mark * rate` (or, for
/// an inverse perpetual, the exact quotient `contracts * contract_value * rate
/// / mark`) rounded **once** to `rounding.dp()` decimal places with
/// `rounding.mode()`.
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
/// - [`ArbitrageError::Overflow`] if the settled amount is out of range.
/// - [`ArbitrageError::Inexact`] if it needs more than 96 bits of mantissa at
///   `rounding.dp()` places (no `Decimal` holds it).
/// - [`ArbitrageError::CurrencyMismatch`] if an inverse convention's base
///   currency is not `settlement`.
pub fn funding_cashflow_rounded(
    convention: PerpConvention,
    position: Decimal,
    event: &FundingEvent,
    settlement: Currency,
    rounding: SettlementRounding,
) -> Result<Cost> {
    let amount = match convention {
        PerpConvention::Linear => {
            let (m, scale, negative) = linear_product(position, event)?;
            let (m, scale) = wide::round_to_dp(m, scale, negative, rounding.dp, rounding.mode)?;
            wide::to_decimal(m, scale, negative, "funding")?
        }
        PerpConvention::Inverse {
            contract_value,
            base,
        } => {
            check_settlement(base, settlement)?;
            wide::quotient_round(
                &[position, contract_value.value(), event.rate.rate()],
                event.mark.value(),
                rounding.dp,
                rounding.mode,
                "funding",
            )?
        }
    };
    Ok(Cost::new(Money::new(amount, settlement)))
}

/// The exact linear product `position * mark * rate`.
fn linear_product(position: Decimal, event: &FundingEvent) -> Result<(wide::U768, u32, bool)> {
    wide::product(
        &[position, event.mark.value(), event.rate.rate()],
        "funding",
    )
}

/// Inverse funding is paid in the base currency.
fn check_settlement(base: Currency, settlement: Currency) -> Result<()> {
    if settlement == base {
        Ok(())
    } else {
        Err(settlement_mismatch(settlement, base))
    }
}

#[cold]
#[inline(never)]
fn settlement_mismatch(settlement: Currency, base: Currency) -> ArbitrageError {
    ArbitrageError::CurrencyMismatch {
        left: settlement,
        right: base,
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
        let anchor = i128::from(self.anchor.millis());
        let offset = i128::from(after.millis())
            .checked_sub(anchor)
            .ok_or_else(overflow)?;
        let interval = i128::from(self.interval_ms);
        // First k with anchor + k * interval > after (interval > 0).
        let mut k = offset
            .checked_div_euclid(interval)
            .and_then(|q| q.checked_add(1))
            .ok_or_else(overflow)?;
        let mut out = Vec::new();
        loop {
            let t = k
                .checked_mul(interval)
                .and_then(|x| x.checked_add(anchor))
                .ok_or_else(overflow)?;
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
            k = k.checked_add(1).ok_or_else(overflow)?;
        }
        Ok(out)
    }
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
        assert_eq!(
            s.instants(Timestamp::from_millis(1), Timestamp::from_millis(H8 - 1))
                .unwrap()
                .len(),
            0
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

    // ---- Inverse perpetuals (M3-06b) ----

    fn btc() -> Currency {
        Currency::new("BTC").unwrap()
    }

    fn inverse(contract_value: Decimal) -> PerpConvention {
        PerpConvention::Inverse {
            contract_value: ContractValue::new(contract_value).unwrap(),
            base: btc(),
        }
    }

    fn inverse_paid(contracts: Decimal, rate: Decimal, mark: Decimal) -> Result<Decimal> {
        funding_cashflow(inverse(dec!(100)), contracts, &event(rate, mark), btc())
            .map(|c| c.money().amount())
    }

    fn inverse_settled(
        contracts: Decimal,
        rate: Decimal,
        mark: Decimal,
        dp: u32,
        mode: Rounding,
    ) -> Decimal {
        let rounding = SettlementRounding::new(dp, mode).unwrap();
        funding_cashflow_rounded(
            inverse(dec!(100)),
            contracts,
            &event(rate, mark),
            btc(),
            rounding,
        )
        .unwrap()
        .money()
        .amount()
    }

    #[test]
    fn inverse_sign_table_by_hand() {
        // contracts * 100 / mark * rate, in BTC; positive is paid.
        // Long, rate > 0: pays 2 * 100 / 40000 * 0.0001 = 0.0000005.
        assert_eq!(
            inverse_paid(dec!(2), dec!(0.0001), dec!(40000)).unwrap(),
            dec!(0.0000005)
        );
        // Short, rate > 0: receives 3 * 100 / 30000 * 0.0001 = 0.000001.
        assert_eq!(
            inverse_paid(dec!(-3), dec!(0.0001), dec!(30000)).unwrap(),
            dec!(-0.000001)
        );
        // Long, rate < 0: receives 2 * 100 / 50000 * 0.0002 = 0.0000008.
        assert_eq!(
            inverse_paid(dec!(2), dec!(-0.0002), dec!(50000)).unwrap(),
            dec!(-0.0000008)
        );
        // Short, rate < 0: pays.
        assert_eq!(
            inverse_paid(dec!(-2), dec!(-0.0002), dec!(50000)).unwrap(),
            dec!(0.0000008)
        );
        // No position or no rate: nothing, of either sign.
        assert_eq!(
            inverse_paid(dec!(0), dec!(0.0001), dec!(30000)).unwrap(),
            dec!(0)
        );
        assert_eq!(
            inverse_paid(dec!(5), dec!(0), dec!(30000)).unwrap(),
            dec!(0)
        );
        let tagged = funding_cashflow(
            inverse(dec!(100)),
            dec!(2),
            &event(dec!(0.0001), dec!(40000)),
            btc(),
        )
        .unwrap();
        assert_eq!(tagged.money().currency(), btc());
    }

    #[test]
    fn inverse_exact_amount_is_inexact_when_the_quotient_does_not_terminate() {
        // 1 * 100 / 30000 * 0.0001 = 0.000000333...
        assert_eq!(
            inverse_paid(dec!(1), dec!(0.0001), dec!(30000)).unwrap_err(),
            ArbitrageError::Inexact { op: "funding" }
        );
    }

    #[test]
    fn inverse_settlement_rounds_once_with_every_mode() {
        // 1 * 100 / 30000 * 0.0001 = 0.000000333... at 8 places.
        let third = |c, mode| inverse_settled(c, dec!(0.0001), dec!(30000), 8, mode);
        assert_eq!(third(dec!(1), Rounding::HalfEven), dec!(0.00000033));
        assert_eq!(third(dec!(1), Rounding::Up), dec!(0.00000034));
        assert_eq!(third(dec!(1), Rounding::Down), dec!(0.00000033));
        assert_eq!(third(dec!(1), Rounding::TowardZero), dec!(0.00000033));
        assert_eq!(third(dec!(-1), Rounding::HalfEven), dec!(-0.00000033));
        assert_eq!(third(dec!(-1), Rounding::Up), dec!(-0.00000033));
        assert_eq!(third(dec!(-1), Rounding::Down), dec!(-0.00000034));
        assert_eq!(third(dec!(-1), Rounding::TowardZero), dec!(-0.00000033));
        // Exactly half: 1 * 100 / 40000 * 0.0001 = 0.00000025 to 7 places
        // goes to the even 0.0000002; 0.00000075 goes to the even 0.0000008.
        let half = |c, rate, mode| inverse_settled(c, rate, dec!(40000), 7, mode);
        assert_eq!(
            half(dec!(1), dec!(0.0001), Rounding::HalfEven),
            dec!(0.0000002)
        );
        assert_eq!(
            half(dec!(-1), dec!(0.0001), Rounding::HalfEven),
            dec!(-0.0000002)
        );
        assert_eq!(
            half(dec!(1), dec!(0.0003), Rounding::HalfEven),
            dec!(0.0000008)
        );
        assert_eq!(
            half(dec!(-1), dec!(0.0003), Rounding::HalfEven),
            dec!(-0.0000008)
        );
        // At 8 places the same amount is exact and unchanged.
        assert_eq!(
            inverse_settled(dec!(1), dec!(0.0001), dec!(40000), 8, Rounding::Up),
            dec!(0.00000025)
        );
        // A tiny amount settles to zero or, rounding up, to one quantum.
        assert_eq!(
            inverse_settled(dec!(1), dec!(0.0001), dec!(30000), 0, Rounding::HalfEven),
            dec!(0)
        );
        assert_eq!(
            inverse_settled(dec!(1), dec!(0.0001), dec!(30000), 0, Rounding::Up),
            dec!(1)
        );
        assert_eq!(
            inverse_settled(dec!(-1), dec!(0.0001), dec!(30000), 0, Rounding::Down),
            dec!(-1)
        );
        // 28 places: 0.0000003333333333333333333333.
        assert_eq!(
            inverse_settled(dec!(1), dec!(0.0001), dec!(30000), 28, Rounding::HalfEven),
            dec!(0.0000003333333333333333333333)
        );
        assert_eq!(
            inverse_settled(dec!(0), dec!(0.0001), dec!(30000), 2, Rounding::Up),
            dec!(0)
        );
    }

    #[test]
    fn funding_inverse_near_max_is_exact() {
        // MAX contracts of 100 at a rate of 0.01 and a mark of 1: the exact
        // amount is MAX, though contracts * contract_value alone is not a
        // Decimal. No intermediate is formed.
        let max = funding_cashflow(
            inverse(dec!(100)),
            Decimal::MAX,
            &event(dec!(0.01), dec!(1)),
            btc(),
        )
        .unwrap();
        assert_eq!(max.money().amount(), Decimal::MAX);
        // The smallest mark: a large quotient, 1 * 100 * 0.0001 / 1e-28 = 1e26.
        let tiny_mark = Decimal::new(1, 28);
        let large = funding_cashflow(
            inverse(dec!(100)),
            dec!(1),
            &event(dec!(0.0001), tiny_mark),
            btc(),
        )
        .unwrap();
        assert_eq!(
            large.money().amount(),
            Decimal::from_i128_with_scale(10_i128.pow(26), 0)
        );
        // Past MAX the amount overflows instead of wrapping or rounding.
        assert_eq!(
            funding_cashflow(
                inverse(dec!(100)),
                Decimal::MAX,
                &event(dec!(0.02), dec!(1)),
                btc()
            )
            .unwrap_err(),
            ArbitrageError::Overflow { op: "funding" }
        );
    }

    #[test]
    fn inverse_funding_is_paid_in_its_base_currency() {
        let err = funding_cashflow(
            inverse(dec!(100)),
            dec!(1),
            &event(dec!(0.0001), dec!(40000)),
            usd(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            ArbitrageError::CurrencyMismatch {
                left: usd(),
                right: btc()
            }
        );
        let rounding = SettlementRounding::new(8, Rounding::HalfEven).unwrap();
        assert!(matches!(
            funding_cashflow_rounded(
                inverse(dec!(100)),
                dec!(1),
                &event(dec!(0.0001), dec!(40000)),
                usd(),
                rounding
            ),
            Err(ArbitrageError::CurrencyMismatch { .. })
        ));
        assert!(matches!(
            ContractValue::new(dec!(0)),
            Err(ArbitrageError::InvalidValue {
                field: "contract value",
                ..
            })
        ));
        assert!(ContractValue::new(dec!(-100)).is_err());
        assert_eq!(ContractValue::new(dec!(100.00)).unwrap().value(), dec!(100));
    }

    /// The settled amount against an independent oracle in `i128`: the
    /// numerator and denominator as integers, the quotient and remainder by
    /// `div_euclid`, and each rounding rule written out again here.
    fn oracle(
        contracts: Decimal,
        cv: Decimal,
        rate: Decimal,
        mark: Decimal,
        dp: u32,
        mode: Rounding,
    ) -> Decimal {
        let n = contracts.mantissa() * cv.mantissa() * rate.mantissa();
        let s_n = contracts.scale() + cv.scale() + rate.scale();
        let num = n.abs() * 10_i128.pow(mark.scale() + dp);
        let den = mark.mantissa() * 10_i128.pow(s_n);
        let (q, r) = (num.div_euclid(den), num.rem_euclid(den));
        let negative = n < 0;
        let away = match mode {
            Rounding::Up => r != 0 && !negative,
            Rounding::Down => r != 0 && negative,
            Rounding::TowardZero => false,
            Rounding::HalfEven => 2 * r > den || (2 * r == den && q % 2 == 1),
        };
        let q = if away { q + 1 } else { q };
        Decimal::from_i128_with_scale(if negative { -q } else { q }, dp).normalize()
    }

    /// Whether `contracts * cv * rate / mark` terminates within 28 places,
    /// and its value if so: the reduced denominator must be `2^a 5^b` with
    /// `max(a, b)` places at most 28.
    fn oracle_exact(
        contracts: Decimal,
        cv: Decimal,
        rate: Decimal,
        mark: Decimal,
    ) -> Option<Decimal> {
        fn gcd(a: i128, b: i128) -> i128 {
            if b == 0 { a } else { gcd(b, a % b) }
        }
        let n = contracts.mantissa() * cv.mantissa() * rate.mantissa();
        if n == 0 {
            return Some(Decimal::ZERO);
        }
        let s_n = contracts.scale() + cv.scale() + rate.scale();
        let num = n.abs() * 10_i128.pow(mark.scale());
        let den = mark.mantissa() * 10_i128.pow(s_n);
        let mut reduced = den / gcd(num, den);
        let (mut twos, mut fives) = (0_u32, 0_u32);
        while reduced % 2 == 0 {
            reduced /= 2;
            twos += 1;
        }
        while reduced % 5 == 0 {
            reduced /= 5;
            fives += 1;
        }
        let places = twos.max(fives);
        (reduced == 1 && places <= 28)
            .then(|| oracle(contracts, cv, rate, mark, places, Rounding::TowardZero))
    }

    #[test]
    fn inverse_settlement_matches_an_exact_oracle() {
        let seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut state = seed;
        let mut next = move |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        let modes = [
            Rounding::HalfEven,
            Rounding::Up,
            Rounding::Down,
            Rounding::TowardZero,
        ];
        for case in 0..20_000 {
            let sign = |positive: bool| if positive { 1 } else { -1 };
            let contracts = Decimal::new(
                sign(next(2) == 0) * i64::try_from(next(1_000_000)).unwrap(),
                u32::try_from(next(4)).unwrap(),
            );
            let cv = Decimal::new(
                1 + i64::try_from(next(100_000)).unwrap(),
                u32::try_from(next(3)).unwrap(),
            );
            let rate = Decimal::new(
                sign(next(2) == 0) * i64::try_from(next(10_000)).unwrap(),
                4 + u32::try_from(next(3)).unwrap(),
            );
            let mark = Decimal::new(
                1 + i64::try_from(next(10_000_000)).unwrap(),
                u32::try_from(next(3)).unwrap(),
            );
            let dp = u32::try_from(next(11)).unwrap();
            let mode = modes[usize::try_from(next(4)).unwrap()];
            let convention = PerpConvention::Inverse {
                contract_value: ContractValue::new(cv).unwrap(),
                base: btc(),
            };
            let ev = event(rate, mark);
            let rounding = SettlementRounding::new(dp, mode).unwrap();
            let settled = funding_cashflow_rounded(convention, contracts, &ev, btc(), rounding)
                .unwrap()
                .money()
                .amount();
            assert_eq!(
                settled,
                oracle(contracts, cv, rate, mark, dp, mode),
                "seed {seed:#x}, case {case}: {contracts} * {cv} * {rate} / {mark}, {dp} dp, {mode:?}"
            );
            let exact =
                funding_cashflow(convention, contracts, &ev, btc()).map(|c| c.money().amount());
            match oracle_exact(contracts, cv, rate, mark) {
                Some(value) => assert_eq!(exact, Ok(value), "seed {seed:#x}, case {case}"),
                None => assert_eq!(
                    exact,
                    Err(ArbitrageError::Inexact { op: "funding" }),
                    "seed {seed:#x}, case {case}"
                ),
            }
        }
    }

    /// Exact ties, which random inputs almost never hit: an odd numerator
    /// over a mark of 2, settled at the numerator's scale, is exactly half a
    /// quantum. Every mode is checked against the oracle, and half even must
    /// round both ways depending on the parity of the quotient.
    #[test]
    fn inverse_settlement_ties_match_the_oracle() {
        let seed = 0xd1b5_4a32_d192_ed03_u64;
        let mut state = seed;
        let mut next = move |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        let modes = [
            Rounding::HalfEven,
            Rounding::Up,
            Rounding::Down,
            Rounding::TowardZero,
        ];
        let (mut up, mut down) = (0, 0);
        for case in 0..4_000 {
            let odd = |n: u64| i64::try_from(2 * n + 1).unwrap();
            let sign = if next(2) == 0 { 1 } else { -1 };
            let contracts =
                Decimal::new(sign * odd(next(500_000)), u32::try_from(next(4)).unwrap());
            let rate = Decimal::new(odd(next(5_000)), 4 + u32::try_from(next(3)).unwrap());
            let cv = dec!(1);
            let mark = dec!(2);
            let dp = contracts.scale() + rate.scale();
            let convention = PerpConvention::Inverse {
                contract_value: ContractValue::new(cv).unwrap(),
                base: btc(),
            };
            for mode in modes {
                let rounding = SettlementRounding::new(dp, mode).unwrap();
                let settled = funding_cashflow_rounded(
                    convention,
                    contracts,
                    &event(rate, mark),
                    btc(),
                    rounding,
                )
                .unwrap()
                .money()
                .amount();
                let expected = oracle(contracts, cv, rate, mark, dp, mode);
                assert_eq!(settled, expected, "seed {seed:#x}, case {case}, {mode:?}");
                if mode == Rounding::HalfEven {
                    if settled.abs() > (contracts * rate / dec!(2)).abs() {
                        up += 1;
                    } else {
                        down += 1;
                    }
                }
            }
        }
        assert!(
            up > 0 && down > 0,
            "ties went both ways: {up} up, {down} down"
        );
    }
}
