//! Exact [`Decimal`] arithmetic.
//!
//! `rust_decimal`'s `checked_*` operations return `None` only when the result
//! leaves the representable range. When the result is in range but needs more
//! than 28 decimal places or a mantissa wider than 96 bits, they round it and
//! still return `Some`. For example, `7922816251426433759354395033.5 + 0.01`
//! comes back as `7922816251426433759354395033.5`.
//!
//! The functions here detect that case: `rust_decimal` only rounds by lowering
//! the result's scale, so a result whose scale is below the exact scale of the
//! operation (the larger operand scale for addition, the sum of the scales for
//! multiplication) has been rounded and is reported as
//! [`ArbitrageError::Inexact`]. A product with a zero operand is returned as
//! an exact zero before that check, because `rust_decimal` gives it scale 0.
//! Every `Ok` value is the exact result, normalized (no trailing zeros, no
//! negative zero). The cases where an exact result is still reported as
//! `Inexact` are listed on [`ArbitrageError::Inexact`].

use crate::error::{ArbitrageError, Result};
use rust_decimal::Decimal;

/// Largest scale a [`Decimal`] can hold.
pub const MAX_SCALE: u32 = 28;

/// Exact `a + b`.
///
/// # Errors
///
/// [`ArbitrageError::Overflow`] if the sum is out of range and
/// [`ArbitrageError::Inexact`] if it would be rounded.
pub fn add(a: Decimal, b: Decimal) -> Result<Decimal> {
    let (a, b) = (a.normalize(), b.normalize());
    let exact_scale = a.scale().max(b.scale());
    let r = a
        .checked_add(b)
        .ok_or(ArbitrageError::Overflow { op: "add" })?;
    finish(r, exact_scale, "add")
}

/// Exact `a - b`.
///
/// # Errors
///
/// [`ArbitrageError::Overflow`] if the difference is out of range and
/// [`ArbitrageError::Inexact`] if it would be rounded.
pub fn sub(a: Decimal, b: Decimal) -> Result<Decimal> {
    let (a, b) = (a.normalize(), b.normalize());
    let exact_scale = a.scale().max(b.scale());
    let r = a
        .checked_sub(b)
        .ok_or(ArbitrageError::Overflow { op: "sub" })?;
    finish(r, exact_scale, "sub")
}

/// Exact `a * b`.
///
/// # Errors
///
/// [`ArbitrageError::Overflow`] if the product is out of range and
/// [`ArbitrageError::Inexact`] if it needs more than 28 decimal places or
/// would be rounded.
pub fn mul(a: Decimal, b: Decimal) -> Result<Decimal> {
    let (a, b) = (a.normalize(), b.normalize());
    // rust_decimal returns a zero product at scale 0, which the scale check
    // below would misread as rounding. A zero operand gives an exact zero.
    if a.is_zero() || b.is_zero() {
        return Ok(Decimal::ZERO);
    }
    let exact_scale = a.scale() + b.scale();
    if exact_scale > MAX_SCALE {
        return Err(ArbitrageError::Inexact { op: "mul" });
    }
    let r = a
        .checked_mul(b)
        .ok_or(ArbitrageError::Overflow { op: "mul" })?;
    finish(r, exact_scale, "mul")
}

/// Exact `a / 10^k`, used for basis points (`k = 4`) and percentages
/// (`k = 2`).
///
/// Dividing by a power of ten only moves the decimal point, so the result is
/// exact whenever the new scale is at most 28.
///
/// # Errors
///
/// [`ArbitrageError::Inexact`] if the result would need more than 28 decimal
/// places.
pub fn div_pow10(a: Decimal, k: u32) -> Result<Decimal> {
    let mut r = a.normalize();
    let scale = r
        .scale()
        .checked_add(k)
        .filter(|s| *s <= MAX_SCALE)
        .ok_or(ArbitrageError::Inexact { op: "div_pow10" })?;
    r.set_scale(scale)
        .map_err(|_| ArbitrageError::Inexact { op: "div_pow10" })?;
    Ok(r.normalize())
}

/// Exact `-a`. Negation never rounds and never overflows in [`Decimal`].
#[must_use]
pub fn neg(a: Decimal) -> Decimal {
    (-a).normalize()
}

fn finish(r: Decimal, exact_scale: u32, op: &'static str) -> Result<Decimal> {
    if r.scale() < exact_scale {
        return Err(ArbitrageError::Inexact { op });
    }
    Ok(r.normalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn add_and_sub_are_exact_and_normalized() {
        assert_eq!(add(dec!(1.10), dec!(2.2)).unwrap(), dec!(3.3));
        assert_eq!(add(dec!(1.10), dec!(2.2)).unwrap().scale(), 1);
        assert_eq!(sub(dec!(1.10), dec!(1.1)).unwrap().scale(), 0);
        assert_eq!(sub(dec!(0.1), dec!(0.3)).unwrap(), dec!(-0.2));
    }

    #[test]
    fn add_rejects_silent_rounding() {
        // rust_decimal returns 7922816251426433759354395033.5 here.
        let a = dec!(7922816251426433759354395033.5);
        assert_eq!(
            add(a, dec!(0.01)),
            Err(ArbitrageError::Inexact { op: "add" })
        );
        assert_eq!(
            add(Decimal::MAX, dec!(0.4)),
            Err(ArbitrageError::Inexact { op: "add" })
        );
        assert_eq!(
            sub(Decimal::MIN, dec!(0.4)),
            Err(ArbitrageError::Inexact { op: "sub" })
        );
    }

    #[test]
    fn add_reports_overflow() {
        assert_eq!(
            add(Decimal::MAX, Decimal::ONE),
            Err(ArbitrageError::Overflow { op: "add" })
        );
        assert_eq!(
            sub(Decimal::MIN, Decimal::ONE),
            Err(ArbitrageError::Overflow { op: "sub" })
        );
    }

    #[test]
    fn mul_is_exact_or_errs() {
        assert_eq!(mul(dec!(0.2), dec!(0.5)).unwrap(), dec!(0.1));
        assert_eq!(mul(dec!(-1.5), dec!(2)).unwrap(), dec!(-3));
        // rust_decimal rounds this to ...925.50; the exact value ends in .5025.
        assert_eq!(
            mul(dec!(79228162514264337593543950.335), dec!(1.5)),
            Err(ArbitrageError::Inexact { op: "mul" })
        );
        // Combined scale 30 > 28.
        assert_eq!(
            mul(dec!(0.00000000000001), dec!(0.0000000000000001)),
            Err(ArbitrageError::Inexact { op: "mul" })
        );
        assert_eq!(
            mul(Decimal::MAX, dec!(2)),
            Err(ArbitrageError::Overflow { op: "mul" })
        );
    }

    #[test]
    fn zero_operands_and_zero_results_are_exact() {
        // rust_decimal returns 100.1 * 0 at scale 0, below the operand scale.
        assert_eq!(mul(dec!(100.1), Decimal::ZERO).unwrap(), Decimal::ZERO);
        assert_eq!(mul(Decimal::ZERO, dec!(0.25)).unwrap(), Decimal::ZERO);
        assert_eq!(mul(dec!(0.000), dec!(-1.5)).unwrap(), Decimal::ZERO);
        assert_eq!(
            mul(Decimal::ZERO, dec!(0.0000000000000000000000000001)).unwrap(),
            Decimal::ZERO
        );
        assert_eq!(add(dec!(1.5), dec!(-1.5)).unwrap(), Decimal::ZERO);
        assert_eq!(add(Decimal::ZERO, dec!(0.25)).unwrap(), dec!(0.25));
        assert_eq!(sub(dec!(1.5), dec!(1.5)).unwrap(), Decimal::ZERO);
        assert_eq!(sub(Decimal::ZERO, dec!(0.25)).unwrap(), dec!(-0.25));
    }

    #[test]
    fn mul_normalizes_operands_before_the_scale_check() {
        // 0.10000000000000 has scale 14 but normalizes to scale 1.
        let a = dec!(0.10000000000000);
        let b = dec!(0.00000000000001);
        assert_eq!(mul(a, b).unwrap(), dec!(0.000000000000001));
    }

    #[test]
    fn div_pow10_moves_the_point() {
        assert_eq!(div_pow10(dec!(12.5), 4).unwrap(), dec!(0.00125));
        assert_eq!(div_pow10(dec!(-300), 2).unwrap(), dec!(-3));
        assert_eq!(div_pow10(Decimal::ZERO, 4).unwrap(), Decimal::ZERO);
        assert_eq!(
            div_pow10(dec!(0.0000000000000000000000001), 4),
            Err(ArbitrageError::Inexact { op: "div_pow10" })
        );
        assert_eq!(
            div_pow10(Decimal::ONE, u32::MAX),
            Err(ArbitrageError::Inexact { op: "div_pow10" })
        );
    }

    #[test]
    fn neg_normalizes_negative_zero() {
        let z = neg(Decimal::ZERO);
        assert_eq!(z, Decimal::ZERO);
        assert!(!z.is_sign_negative());
        assert_eq!(neg(dec!(1.50)), dec!(-1.5));
    }
}
