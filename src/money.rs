//! Currencies and currency-tagged monetary amounts.

use crate::error::{ArbitrageError, Result};
use crate::exact;
use core::fmt;
use rust_decimal::{Decimal, RoundingStrategy};

/// Maximum length of a [`Currency`] code.
pub const MAX_CURRENCY_LEN: usize = 12;

/// A currency or asset code such as `USD`, `USDT` or `BTC`.
///
/// Codes are 1 to 12 characters in `A-Z` and `0-9`. The type is `Copy`, so a
/// [`Money`] can be passed by value.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Currency {
    bytes: [u8; MAX_CURRENCY_LEN],
    len: u8,
}

impl Currency {
    /// Builds a currency from its code.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidCurrency`] if the code is empty, longer than
    /// 12 characters or contains a character outside `A-Z` and `0-9`.
    ///
    /// ```
    /// use arbitrage_rs::Currency;
    /// let usd = Currency::new("USD").unwrap();
    /// assert_eq!(usd.as_str(), "USD");
    /// assert!(Currency::new("usd").is_err());
    /// ```
    pub fn new(code: &str) -> Result<Self> {
        let valid = !code.is_empty()
            && code.len() <= MAX_CURRENCY_LEN
            && code
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
        if !valid {
            return Err(ArbitrageError::InvalidCurrency {
                code: code.to_owned(),
            });
        }
        let mut bytes = [0u8; MAX_CURRENCY_LEN];
        bytes[..code.len()].copy_from_slice(code.as_bytes());
        Ok(Self {
            bytes,
            // Bounded by MAX_CURRENCY_LEN above.
            len: code.len() as u8,
        })
    }

    /// The code as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Only ASCII bytes are stored, so the slice is valid UTF-8.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or_default()
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Currency({})", self.as_str())
    }
}

/// Explicit rounding mode for [`Money::round_dp`].
///
/// Arithmetic in this crate never rounds on its own; rounding happens only
/// where a caller asks for it, for example to a venue's fee quantum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rounding {
    /// Toward positive infinity. Use it for amounts you pay (costs), so the
    /// estimate is never below the charge.
    Up,
    /// Toward negative infinity. Use it for amounts you receive.
    Down,
    /// Toward zero (truncation).
    TowardZero,
    /// To the nearest value, ties to even.
    HalfEven,
}

impl Rounding {
    fn strategy(self) -> RoundingStrategy {
        match self {
            Self::Up => RoundingStrategy::ToPositiveInfinity,
            Self::Down => RoundingStrategy::ToNegativeInfinity,
            Self::TowardZero => RoundingStrategy::ToZero,
            Self::HalfEven => RoundingStrategy::MidpointNearestEven,
        }
    }
}

/// A signed amount of one currency.
///
/// The amount is stored normalized, so equal values compare, hash and print
/// the same whatever scale they were built with.
///
/// ```
/// use arbitrage_rs::{Currency, Money};
/// use rust_decimal_macros::dec;
///
/// let usd = Currency::new("USD").unwrap();
/// let a = Money::new(dec!(10.50), usd);
/// let b = Money::new(dec!(0.25), usd);
/// assert_eq!(a.checked_sub(b).unwrap().amount(), dec!(10.25));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Money {
    amount: Decimal,
    currency: Currency,
}

impl Money {
    /// An amount of `currency`.
    #[must_use]
    pub fn new(amount: Decimal, currency: Currency) -> Self {
        Self {
            amount: amount.normalize(),
            currency,
        }
    }

    /// Zero of `currency`.
    #[must_use]
    pub fn zero(currency: Currency) -> Self {
        Self::new(Decimal::ZERO, currency)
    }

    /// The signed amount.
    #[must_use]
    pub fn amount(&self) -> Decimal {
        self.amount
    }

    /// The currency.
    #[must_use]
    pub fn currency(&self) -> Currency {
        self.currency
    }

    /// True if the amount is zero.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.amount.is_zero()
    }

    /// True if the amount is above zero.
    #[must_use]
    pub fn is_positive(&self) -> bool {
        self.amount > Decimal::ZERO
    }

    /// True if the amount is below zero.
    #[must_use]
    pub fn is_negative(&self) -> bool {
        self.amount < Decimal::ZERO
    }

    /// The absolute value.
    #[must_use]
    pub fn abs(&self) -> Self {
        Self::new(self.amount.abs(), self.currency)
    }

    /// The negated amount.
    #[must_use]
    pub fn neg(&self) -> Self {
        Self::new(exact::neg(self.amount), self.currency)
    }

    /// Exact `self + other`.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::CurrencyMismatch`] if the currencies differ, and the
    /// errors of [`exact::add`].
    pub fn checked_add(&self, other: Self) -> Result<Self> {
        self.same_currency(other)?;
        Ok(Self::new(
            exact::add(self.amount, other.amount)?,
            self.currency,
        ))
    }

    /// Exact `self - other`.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::CurrencyMismatch`] if the currencies differ, and the
    /// errors of [`exact::sub`].
    pub fn checked_sub(&self, other: Self) -> Result<Self> {
        self.same_currency(other)?;
        Ok(Self::new(
            exact::sub(self.amount, other.amount)?,
            self.currency,
        ))
    }

    /// Exact `self * factor`, for a dimensionless factor such as a hedge
    /// ratio. The currency is unchanged.
    ///
    /// # Errors
    ///
    /// The errors of [`exact::mul`].
    pub fn checked_scale(&self, factor: Decimal) -> Result<Self> {
        Ok(Self::new(exact::mul(self.amount, factor)?, self.currency))
    }

    /// Rounds to `dp` decimal places with an explicit mode.
    ///
    /// ```
    /// use arbitrage_rs::{Currency, Money, Rounding};
    /// use rust_decimal_macros::dec;
    ///
    /// let usd = Currency::new("USD").unwrap();
    /// let fee = Money::new(dec!(0.00125), usd);
    /// assert_eq!(fee.round_dp(2, Rounding::Up).unwrap().amount(), dec!(0.01));
    /// assert_eq!(fee.round_dp(2, Rounding::Down).unwrap().amount(), dec!(0));
    /// ```
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::Inexact`] if `dp` is above 28, the most a
    /// [`Decimal`] holds.
    pub fn round_dp(&self, dp: u32, mode: Rounding) -> Result<Self> {
        if dp > exact::MAX_SCALE {
            return Err(ArbitrageError::Inexact { op: "round_dp" });
        }
        Ok(Self::new(
            self.amount.round_dp_with_strategy(dp, mode.strategy()),
            self.currency,
        ))
    }

    fn same_currency(&self, other: Self) -> Result<()> {
        if self.currency == other.currency {
            Ok(())
        } else {
            Err(ArbitrageError::CurrencyMismatch {
                left: self.currency,
                right: other.currency,
            })
        }
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.amount, self.currency)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn usd() -> Currency {
        Currency::new("USD").unwrap()
    }

    #[test]
    fn currency_codes_are_validated() {
        assert_eq!(Currency::new("USDT").unwrap().as_str(), "USDT");
        assert_eq!(Currency::new("1000PEPE").unwrap().to_string(), "1000PEPE");
        assert_eq!(Currency::new("ABCDEFGHIJKL").unwrap().as_str().len(), 12);
        for bad in ["", "usd", "US D", "USD-", "ABCDEFGHIJKLM", "€"] {
            assert_eq!(
                Currency::new(bad),
                Err(ArbitrageError::InvalidCurrency {
                    code: bad.to_owned()
                }),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn currency_equality_ignores_nothing_but_the_code() {
        assert_eq!(Currency::new("BTC").unwrap(), Currency::new("BTC").unwrap());
        assert_ne!(
            Currency::new("BTC").unwrap(),
            Currency::new("BTC1").unwrap()
        );
        assert_eq!(format!("{:?}", usd()), "Currency(USD)");
    }

    #[test]
    fn money_is_normalized() {
        let a = Money::new(dec!(1.500), usd());
        assert_eq!(a.amount().scale(), 1);
        assert_eq!(a, Money::new(dec!(1.5), usd()));
        assert_eq!(a.to_string(), "1.5 USD");
        assert!(!Money::new(dec!(-0.00), usd()).amount().is_sign_negative());
    }

    #[test]
    fn money_arithmetic_checks_currency() {
        let eur = Currency::new("EUR").unwrap();
        let a = Money::new(dec!(1), usd());
        let b = Money::new(dec!(1), eur);
        let mismatch = ArbitrageError::CurrencyMismatch {
            left: usd(),
            right: eur,
        };
        assert_eq!(a.checked_add(b), Err(mismatch.clone()));
        assert_eq!(a.checked_sub(b), Err(mismatch));
    }

    #[test]
    fn money_arithmetic_is_exact() {
        let a = Money::new(dec!(0.1), usd());
        let b = Money::new(dec!(0.2), usd());
        assert_eq!(a.checked_add(b).unwrap().amount(), dec!(0.3));
        assert_eq!(a.checked_sub(b).unwrap().amount(), dec!(-0.1));
        assert_eq!(a.checked_scale(dec!(-2.5)).unwrap().amount(), dec!(-0.25));
        assert_eq!(a.neg().amount(), dec!(-0.1));
        assert_eq!(a.neg().abs(), a);
        let big = Money::new(dec!(7922816251426433759354395033.5), usd());
        assert_eq!(
            big.checked_add(Money::new(dec!(0.01), usd())),
            Err(ArbitrageError::Inexact { op: "add" })
        );
    }

    #[test]
    fn signs() {
        assert!(Money::zero(usd()).is_zero());
        assert!(Money::new(dec!(0.01), usd()).is_positive());
        assert!(Money::new(dec!(-0.01), usd()).is_negative());
    }

    #[test]
    fn rounding_modes() {
        let x = Money::new(dec!(-0.125), usd());
        assert_eq!(x.round_dp(2, Rounding::Up).unwrap().amount(), dec!(-0.12));
        assert_eq!(x.round_dp(2, Rounding::Down).unwrap().amount(), dec!(-0.13));
        assert_eq!(
            x.round_dp(2, Rounding::TowardZero).unwrap().amount(),
            dec!(-0.12)
        );
        assert_eq!(
            x.round_dp(2, Rounding::HalfEven).unwrap().amount(),
            dec!(-0.12)
        );
        assert_eq!(
            x.round_dp(29, Rounding::Up),
            Err(ArbitrageError::Inexact { op: "round_dp" })
        );
    }
}
