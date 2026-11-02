//! Prices and quantities.

use crate::error::{ArbitrageError, Result};
use crate::exact;
use crate::money::{Currency, Money};
use crate::wide::{U384, to_decimal};
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

    /// The largest whole multiple of `lot` that is not above `self`, or
    /// `None` if `self` is below one lot. Exact: the result never exceeds
    /// `self`.
    ///
    /// ```
    /// use arbitrage_rs::Quantity;
    /// use rust_decimal_macros::dec;
    ///
    /// let q = Quantity::new(dec!(2.37)).unwrap();
    /// let lot = Quantity::new(dec!(0.1)).unwrap();
    /// assert_eq!(q.floor_to_lot(lot).unwrap().unwrap().value(), dec!(2.3));
    /// assert_eq!(lot.floor_to_lot(q).unwrap(), None);
    /// ```
    ///
    /// # Errors
    ///
    /// - [`ArbitrageError::Overflow`] if the number of lots, `self / lot`,
    ///   is above [`Decimal::MAX`].
    /// - [`ArbitrageError::Inexact`] if the result is not representable as a
    ///   `Decimal` (more than 28 decimal places or 96 bits of mantissa at its
    ///   shortest scale).
    pub fn floor_to_lot(&self, lot: Quantity) -> Result<Option<Quantity>> {
        let (q, l) = (self.0.normalize(), lot.0.normalize());
        // Both values as integers at a common scale: Q = q * 10^s and
        // L = lot * 10^s, compared exactly in 384 bits.
        let s = q.scale().max(l.scale());
        let overflow = || ArbitrageError::Overflow { op: "floor_to_lot" };
        let big_q = U384::from_mantissa(q)
            .checked_mul_pow10(s - q.scale())
            .ok_or_else(overflow)?;
        let big_l = U384::from_mantissa(l)
            .checked_mul_pow10(s - l.scale())
            .ok_or_else(overflow)?;
        let lots_le_q = |n: u128| -> Result<bool> {
            let product = U384::from_u128(n)
                .checked_mul(&big_l)
                .ok_or_else(overflow)?;
            Ok(product <= big_q)
        };

        // The rounded Decimal quotient is within one of the floor; the exact
        // comparisons correct it until n * L <= Q < (n + 1) * L.
        let estimate = q
            .checked_div(l)
            .ok_or(ArbitrageError::Overflow { op: "div" })?
            .floor()
            .normalize();
        let mut n = estimate.mantissa().unsigned_abs();
        while n > 0 && !lots_le_q(n)? {
            n -= 1;
        }
        // n < 2^96, so n + 1 cannot overflow u128.
        while lots_le_q(n + 1)? {
            n += 1;
        }
        if n == 0 {
            return Ok(None);
        }
        let product = U384::from_u128(n)
            .checked_mul(&U384::from_mantissa(l))
            .ok_or_else(overflow)?;
        let value = to_decimal(product, l.scale(), false, "floor_to_lot")?;
        Ok(Some(Self(value)))
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
    fn floor_to_lot_never_exceeds_the_quantity() {
        let q = |x| Quantity::new(x).unwrap();
        let floor = |a, lot| q(a).floor_to_lot(q(lot)).unwrap().map(|r| r.value());
        assert_eq!(floor(dec!(1), dec!(0.3)), Some(dec!(0.9)));
        assert_eq!(floor(dec!(0.9), dec!(0.3)), Some(dec!(0.9)));
        assert_eq!(floor(dec!(12.34), dec!(0.1)), Some(dec!(12.3)));
        assert_eq!(floor(dec!(5), dec!(5)), Some(dec!(5)));
        assert_eq!(floor(dec!(4.999), dec!(5)), None);
        // 1/3 rounds up in its last place; the correction keeps 0.9999...9.
        let third = dec!(0.3333333333333333333333333333);
        let r = floor(dec!(1), third).unwrap();
        assert_eq!(r, dec!(0.9999999999999999999999999999));
        assert!(r <= dec!(1));
        assert_eq!(
            q(Decimal::MAX).floor_to_lot(q(dec!(0.0000000000000000000000000001))),
            Err(ArbitrageError::Overflow { op: "div" })
        );
    }

    #[test]
    fn floor_to_lot_at_the_top_of_the_range() {
        let q = |x| Quantity::new(x).unwrap();
        let floor = |a, lot| q(a).floor_to_lot(q(lot)).unwrap().map(|r| r.value());
        // Exact at MAX: the next multiple is out of range, not an error.
        assert_eq!(floor(Decimal::MAX, Decimal::MAX), Some(Decimal::MAX));
        assert_eq!(floor(Decimal::MAX, dec!(1)), Some(Decimal::MAX));
        // Not exact: MAX = 2 * 39614081257132168796771975167 + 1, and the
        // next multiple, MAX + 1, is out of range.
        assert_eq!(floor(Decimal::MAX, dec!(2)), Some(Decimal::MAX - dec!(1)));
        let max_minus_one = Decimal::MAX - dec!(1);
        assert_eq!(floor(max_minus_one, dec!(2)), Some(max_minus_one));
        assert_eq!(floor(Decimal::MAX, dec!(3)), Some(Decimal::MAX));
        // Half of MAX (rounded down) as the lot: one lot fits, two do not.
        let half = dec!(39614081257132168796771975167);
        assert_eq!(floor(Decimal::MAX, half), Some(Decimal::MAX - dec!(1)));
        // Next multiple wider than 96 bits at the shared scale: lot 0.2 on
        // ...033.5 floors to ...033.4; ...033.6 does not fit.
        assert_eq!(
            floor(dec!(7922816251426433759354395033.5), dec!(0.2)),
            Some(dec!(7922816251426433759354395033.4))
        );
        // Not a whole number of lots near the top: ...033 / 0.3 floors to
        // 26409387504754779197847983443 lots, ...032.9.
        assert_eq!(
            floor(dec!(7922816251426433759354395033), dec!(0.3)),
            Some(dec!(7922816251426433759354395032.9))
        );
        // MAX / 1.5 = 52818775009509558395695966890 lots exactly; the product
        // is MAX at scale 1 (97 bits) and MAX after stripping the zero.
        assert_eq!(floor(Decimal::MAX, dec!(1.5)), Some(Decimal::MAX));
    }

    #[test]
    fn floor_to_lot_when_a_product_needs_more_than_96_bits() {
        let q = |x| Quantity::new(x).unwrap();
        let third = dec!(0.3333333333333333333333333333);
        // 10 / third = 30.000...03, floor 30. 30 * third =
        // 9.9999999999999999999999999990 does not fit at scale 28 but is
        // 9.999999999999999999999999999 at scale 27.
        let r = q(dec!(10)).floor_to_lot(q(third)).unwrap().unwrap();
        assert_eq!(r.value(), dec!(9.999999999999999999999999999));
        assert_eq!(r.value().scale(), 27);
        // 7 * third = 2.3333333333333333333333333331 fits at scale 28.
        let r = q(dec!(2.5)).floor_to_lot(q(third)).unwrap().unwrap();
        assert_eq!(r.value(), dec!(2.3333333333333333333333333331));
        // A floor that no Decimal equals is reported. The lot has the
        // largest mantissa at scale 28; 2 lots end in 0 and fit at scale 27,
        // 3 lots (23.7684487542793012780631851005) need 97 bits at scale 28.
        let lot = dec!(7.9228162514264337593543950335);
        assert_eq!(
            q(dec!(16)).floor_to_lot(q(lot)).unwrap().unwrap().value(),
            dec!(15.845632502852867518708790067)
        );
        assert_eq!(
            q(dec!(24)).floor_to_lot(q(lot)),
            Err(ArbitrageError::Inexact { op: "floor_to_lot" })
        );
        // Lots above Decimal::MAX are still the documented division overflow.
        assert_eq!(
            q(Decimal::MAX).floor_to_lot(q(dec!(0.5))),
            Err(ArbitrageError::Overflow { op: "div" })
        );
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
