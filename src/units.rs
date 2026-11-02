//! Prices and quantities.

use crate::error::{ArbitrageError, Result};
use crate::exact;
use crate::money::{Currency, Money};
use core::fmt;
use rust_decimal::Decimal;

/// A strictly positive price: quote currency per unit of quantity.
///
/// A price does not carry its quote currency; [`Price::notional`] takes it
/// explicitly, so the caller states the unit at the point where money is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(Decimal);

impl Price {
    /// Builds a price.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidValue`] if `value` is zero or negative.
    pub fn new(value: Decimal) -> Result<Self> {
        positive("price", value).map(Self)
    }

    /// The price as a [`Decimal`].
    #[must_use]
    pub fn value(&self) -> Decimal {
        self.0
    }

    /// Exact `price * quantity` in `quote`.
    ///
    /// ```
    /// use arbitrage_rs::{Currency, Price, Quantity};
    /// use rust_decimal_macros::dec;
    ///
    /// let usd = Currency::new("USD").unwrap();
    /// let n = Price::new(dec!(101.25)).unwrap()
    ///     .notional(Quantity::new(dec!(4)).unwrap(), usd)
    ///     .unwrap();
    /// assert_eq!(n.amount(), dec!(405));
    /// ```
    ///
    /// # Errors
    ///
    /// The errors of [`exact::mul`].
    pub fn notional(&self, quantity: Quantity, quote: Currency) -> Result<Money> {
        Ok(Money::new(exact::mul(self.0, quantity.0)?, quote))
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// A strictly positive size, in the units the matching [`Price`] is quoted
/// per (base units, or contracts times multiplier, converted by the caller).
///
/// Direction is not part of a quantity; legs and positions carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Quantity(Decimal);

impl Quantity {
    /// Builds a quantity.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidValue`] if `value` is zero or negative.
    pub fn new(value: Decimal) -> Result<Self> {
        positive("quantity", value).map(Self)
    }

    /// The quantity as a [`Decimal`].
    #[must_use]
    pub fn value(&self) -> Decimal {
        self.0
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

fn positive(field: &'static str, value: Decimal) -> Result<Decimal> {
    if value > Decimal::ZERO {
        Ok(value.normalize())
    } else {
        Err(ArbitrageError::InvalidValue {
            field,
            value,
            reason: "must be positive",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn price_and_quantity_must_be_positive() {
        assert_eq!(Price::new(dec!(1.50)).unwrap().value().scale(), 1);
        assert_eq!(Quantity::new(dec!(0.001)).unwrap().to_string(), "0.001");
        for v in [dec!(0), dec!(-1)] {
            assert_eq!(
                Price::new(v),
                Err(ArbitrageError::InvalidValue {
                    field: "price",
                    value: v,
                    reason: "must be positive"
                })
            );
            assert_eq!(
                Quantity::new(v),
                Err(ArbitrageError::InvalidValue {
                    field: "quantity",
                    value: v,
                    reason: "must be positive"
                })
            );
        }
    }

    #[test]
    fn notional_is_exact() {
        let usd = Currency::new("USD").unwrap();
        let p = Price::new(dec!(0.1)).unwrap();
        let q = Quantity::new(dec!(3)).unwrap();
        assert_eq!(p.notional(q, usd).unwrap(), Money::new(dec!(0.3), usd));
        let huge = Price::new(Decimal::MAX).unwrap();
        assert_eq!(
            huge.notional(Quantity::new(dec!(2)).unwrap(), usd),
            Err(ArbitrageError::Overflow { op: "mul" })
        );
    }
}
