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
    /// - [`ArbitrageError::Overflow`] if `self / lot` is out of range.
    /// - [`ArbitrageError::Inexact`] if the result is not representable at
    ///   `lot`'s scale (see [`exact::mul`]), or, near the top of the range, a
    ///   multiple within one unit in the last place of `self` cannot be
    ///   compared with it exactly.
    pub fn floor_to_lot(&self, lot: Quantity) -> Result<Option<Quantity>> {
        // The quotient may be rounded in its last place, so the floor is
        // corrected with exact products: n * lot <= self < (n + 1) * lot.
        let mut n = self
            .0
            .checked_div(lot.0)
            .ok_or(ArbitrageError::Overflow { op: "div" })?
            .floor();
        while n > Decimal::ZERO && !multiple_le(n, lot.0, self.0)? {
            n = exact::sub(n, Decimal::ONE)?;
        }
        // n + 1 out of range means n is Decimal::MAX, and then the next
        // multiple is above any representable quantity.
        while let Ok(next) = exact::add(n, Decimal::ONE) {
            if !multiple_le(next, lot.0, self.0)? {
                break;
            }
            n = next;
        }
        if n.is_zero() {
            return Ok(None);
        }
        Ok(Some(Self(exact::mul(n, lot.0)?)))
    }
}

/// `n * lot <= q` for `n, lot > 0` and `q > 0`, decided exactly.
///
/// - A product out of range is above every representable `q`.
/// - A product that does not fit at `lot`'s scale (`n` is an integer, so
///   that is the product's exact scale) is above `q` when `q` has at least
///   `lot`'s scale, because `q` itself fits at that scale.
/// - Otherwise such a product is compared through
///   `rust_decimal`'s rounded product, which is within half a unit of its
///   last place of the exact one: if it is more than one unit away from `q`
///   the order is certain. Otherwise the comparison is undecided and
///   reported as [`ArbitrageError::Inexact`].
fn multiple_le(n: Decimal, lot: Decimal, q: Decimal) -> Result<bool> {
    match exact::mul(n, lot) {
        Ok(product) => Ok(product <= q),
        Err(ArbitrageError::Overflow { .. }) => Ok(false),
        Err(ArbitrageError::Inexact { .. }) if lot.normalize().scale() <= q.normalize().scale() => {
            Ok(false)
        }
        Err(ArbitrageError::Inexact { .. }) => {
            let undecided = ArbitrageError::Inexact { op: "mul" };
            let rounded = n
                .checked_mul(lot)
                .ok_or(ArbitrageError::Overflow { op: "mul" })?;
            let ulp = Decimal::new(1, rounded.scale());
            match exact::sub(rounded, q) {
                Ok(d) if d > ulp => Ok(false),
                Ok(d) if -d > ulp => Ok(true),
                _ => Err(undecided),
            }
        }
        Err(e) => Err(e),
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
        // Half of MAX as the lot: one lot fits, two do not.
        let half = dec!(39614081257132168796771975167);
        assert_eq!(floor(Decimal::MAX, half), Some(Decimal::MAX - dec!(1)));
        // Next multiple in range but wider than 96 bits at the shared scale:
        // q = 7922816251426433759354395033.5, lot 0.2, floor ...033.4 and
        // the next multiple ...033.6 does not fit, so it is above q.
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
        // Documented undecided case: the lot has more decimal places than
        // q and the multiples near MAX need more than 96 bits at scale 1.
        assert_eq!(
            q(Decimal::MAX).floor_to_lot(q(dec!(1.5))),
            Err(ArbitrageError::Inexact { op: "mul" })
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
