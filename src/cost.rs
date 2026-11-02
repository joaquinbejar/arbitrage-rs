//! Trading costs and fee rates.

use crate::error::{ArbitrageError, Result};
use crate::money::{Currency, Money};
use crate::wide;
use core::fmt;
use rust_decimal::Decimal;

/// Basis points in one unit (`1 = 10_000 bps`).
pub const BPS_PER_UNIT: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

/// `1 / 10_000`, as an exact factor.
const BPS_FRACTION: Decimal = Decimal::from_parts(1, 0, 0, false, 4);

/// A cost in one currency, signed from the payer's side: positive is paid,
/// negative is received (for example a maker rebate).
///
/// A net result is `gross - cost`, so a rebate raises it.
///
/// ```
/// use arbitrage_rs::{Cost, Currency, Money};
/// use rust_decimal_macros::dec;
///
/// let usd = Currency::new("USD").unwrap();
/// let gross = Money::new(dec!(0.50), usd);
/// let cost = Cost::new(Money::new(dec!(0.75), usd));
/// assert!(cost.net_of(gross).unwrap().is_negative());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cost(Money);

impl Cost {
    /// A cost of `amount` (positive is paid).
    #[must_use]
    pub fn new(amount: Money) -> Self {
        Self(amount)
    }

    /// The cost as signed money (positive is paid).
    #[must_use]
    pub fn money(&self) -> Money {
        self.0
    }

    /// True if this is a payment rather than a receipt or zero.
    #[must_use]
    pub fn is_paid(&self) -> bool {
        self.0.is_positive()
    }

    /// Exact sum of two costs.
    ///
    /// # Errors
    ///
    /// The errors of [`Money::checked_add`].
    pub fn checked_add(&self, other: Self) -> Result<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    /// Exact sum of `costs` in `ccy`, converted once: a running sum may pass
    /// the range of a `Decimal` when the total does not.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::CurrencyMismatch`] for a cost in another currency,
    /// and [`ArbitrageError::Overflow`] if the total does not fit.
    pub(crate) fn total(costs: &[Cost], ccy: Currency) -> Result<Self> {
        let mut amounts = Vec::with_capacity(costs.len());
        for c in costs {
            if c.0.currency() != ccy {
                return Err(ArbitrageError::CurrencyMismatch {
                    left: ccy,
                    right: c.0.currency(),
                });
            }
            amounts.push(c.0.amount());
        }
        Ok(Self(Money::new(wide::sum(&amounts, "cost total")?, ccy)))
    }

    /// Exact `gross - self`, the result net of this cost.
    ///
    /// # Errors
    ///
    /// The errors of [`Money::checked_sub`].
    pub fn net_of(&self, gross: Money) -> Result<Money> {
        gross.checked_sub(self.0)
    }
}

impl fmt::Display for Cost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// A proportional fee in basis points of traded notional.
///
/// Negative rates are rebates. The rate is bounded to `[-10_000, 10_000]` bps,
/// that is at most 100% of notional either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FeeRate {
    bps: Decimal,
}

impl FeeRate {
    /// A rate of `bps` basis points.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidValue`] if `bps` is outside
    /// `[-10_000, 10_000]`.
    pub fn from_bps(bps: Decimal) -> Result<Self> {
        if bps.abs() > BPS_PER_UNIT {
            return Err(ArbitrageError::InvalidValue {
                field: "fee rate (bps)",
                value: bps,
                reason: "must be within [-10000, 10000]",
            });
        }
        Ok(Self {
            bps: bps.normalize(),
        })
    }

    /// A zero fee.
    #[must_use]
    pub fn zero() -> Self {
        Self { bps: Decimal::ZERO }
    }

    /// The rate in basis points.
    #[must_use]
    pub fn bps(&self) -> Decimal {
        self.bps
    }

    /// Exact fee on a traded notional: `|notional| * bps / 10_000`, in the
    /// notional's currency. The side of the trade does not change the fee.
    ///
    /// No rounding is applied; round the result with [`Money::round_dp`] to
    /// a venue's fee quantum if needed.
    ///
    /// ```
    /// use arbitrage_rs::{Currency, FeeRate, Money};
    /// use rust_decimal_macros::dec;
    ///
    /// let usd = Currency::new("USD").unwrap();
    /// let fee = FeeRate::from_bps(dec!(2.5)).unwrap()
    ///     .fee_on(Money::new(dec!(-405), usd))
    ///     .unwrap();
    /// assert_eq!(fee.money().amount(), dec!(0.10125));
    /// ```
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::Overflow`] or [`ArbitrageError::Inexact`] only if the
    /// exact fee does not fit in a [`Decimal`].
    pub fn fee_on(&self, notional: Money) -> Result<Cost> {
        self.fee_on_factors(&[notional.amount()], notional.currency())
    }

    /// Exact fee on the notional `|product of factors|`, for example
    /// `[price, quantity]`: one product with the rate, converted once, so the
    /// notional itself may exceed a `Decimal` when the fee does not.
    pub(crate) fn fee_on_factors(&self, factors: &[Decimal], ccy: Currency) -> Result<Cost> {
        let mut all = Vec::with_capacity(factors.len() + 2);
        all.extend(factors.iter().map(|f| f.abs()));
        all.push(self.bps);
        all.push(BPS_FRACTION);
        let fee = wide::sum_products_exact(&[&all], "fee")?;
        Ok(Cost::new(Money::new(fee, ccy)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Currency;
    use rust_decimal_macros::dec;

    fn usd(x: Decimal) -> Money {
        Money::new(x, Currency::new("USD").unwrap())
    }

    #[test]
    fn bps_constant() {
        assert_eq!(BPS_PER_UNIT, dec!(10000));
    }

    #[test]
    fn fee_rate_bounds() {
        assert!(FeeRate::from_bps(dec!(10000)).is_ok());
        assert!(FeeRate::from_bps(dec!(-10000)).is_ok());
        assert_eq!(
            FeeRate::from_bps(dec!(10000.01)),
            Err(ArbitrageError::InvalidValue {
                field: "fee rate (bps)",
                value: dec!(10000.01),
                reason: "must be within [-10000, 10000]"
            })
        );
        assert!(FeeRate::from_bps(dec!(-10001)).is_err());
        assert_eq!(FeeRate::from_bps(dec!(1.50)).unwrap().bps().scale(), 1);
    }

    #[test]
    fn fee_is_exact_and_side_independent() {
        let r = FeeRate::from_bps(dec!(2.5)).unwrap();
        let buy = r.fee_on(usd(dec!(405))).unwrap();
        let sell = r.fee_on(usd(dec!(-405))).unwrap();
        assert_eq!(buy, sell);
        assert_eq!(buy.money().amount(), dec!(0.10125));
        assert!(buy.is_paid());
        assert_eq!(
            FeeRate::zero().fee_on(usd(dec!(405))).unwrap(),
            Cost::new(usd(dec!(0)))
        );
        // Zero rate on a fractional notional: an exact zero, not Inexact.
        assert_eq!(
            FeeRate::zero().fee_on(usd(dec!(100.1))).unwrap(),
            Cost::new(usd(dec!(0)))
        );
    }

    #[test]
    fn rebate_is_a_negative_cost() {
        let rebate = FeeRate::from_bps(dec!(-1))
            .unwrap()
            .fee_on(usd(dec!(1000)))
            .unwrap();
        assert_eq!(rebate.money().amount(), dec!(-0.1));
        assert!(!rebate.is_paid());
        assert_eq!(rebate.net_of(usd(dec!(1))).unwrap().amount(), dec!(1.1));
    }

    #[test]
    fn fee_that_needs_more_than_28_places_is_rejected() {
        let r = FeeRate::from_bps(dec!(0.0001)).unwrap();
        let n = usd(dec!(0.000000000000000000001));
        assert_eq!(r.fee_on(n), Err(ArbitrageError::Inexact { op: "fee" }));
    }

    #[test]
    fn fee_on_a_notional_near_max_is_exact() {
        // |n| * bps passes Decimal::MAX; |n| * bps / 10_000 fits.
        let r = FeeRate::from_bps(dec!(4)).unwrap();
        let fee = r.fee_on(usd(Decimal::MAX)).unwrap();
        assert_eq!(fee.money().amount(), dec!(31691265005705735037417580.134));
        // The same fee from the factors of a notional beyond Decimal::MAX.
        let fee = r
            .fee_on_factors(&[Decimal::MAX, dec!(2)], Currency::new("USD").unwrap())
            .unwrap();
        assert_eq!(fee.money().amount(), dec!(63382530011411470074835160.268));
    }

    #[test]
    fn cost_total_is_one_exact_sum() {
        let usd_ccy = Currency::new("USD").unwrap();
        let big = Cost::new(usd(dec!(47536897508558602556126370201)));
        let rebate = Cost::new(usd(dec!(-47536897508558602556126370201)));
        // A running sum would pass Decimal::MAX after the first two.
        let total = Cost::total(&[big, big, rebate], usd_ccy).unwrap();
        assert_eq!(total, big);
        let eur = Cost::new(Money::new(dec!(1), Currency::new("EUR").unwrap()));
        assert!(matches!(
            Cost::total(&[big, eur], usd_ccy),
            Err(ArbitrageError::CurrencyMismatch { .. })
        ));
    }

    #[test]
    fn gross_positive_net_negative() {
        let gross = usd(dec!(0.50));
        let fees = FeeRate::from_bps(dec!(5))
            .unwrap()
            .fee_on(usd(dec!(1000)))
            .unwrap()
            .checked_add(
                FeeRate::from_bps(dec!(5))
                    .unwrap()
                    .fee_on(usd(dec!(1000.5)))
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(fees.money().amount(), dec!(1.00025));
        let net = fees.net_of(gross).unwrap();
        assert_eq!(net.amount(), dec!(-0.50025));
    }

    #[test]
    fn cost_currency_mismatch() {
        let eur = Money::new(dec!(1), Currency::new("EUR").unwrap());
        assert!(matches!(
            Cost::new(usd(dec!(1))).checked_add(Cost::new(eur)),
            Err(ArbitrageError::CurrencyMismatch { .. })
        ));
    }
}
