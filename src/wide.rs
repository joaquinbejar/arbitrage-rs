//! Unsigned 384-bit integers for exact comparisons and products of
//! [`Decimal`] mantissas.
//!
//! A `Decimal` is `m / 10^s` with `m < 2^96` and `s <= 28`. Aligning two of
//! them to a common scale multiplies a mantissa by at most `10^28 < 2^94`,
//! and a product of two aligned values stays below `2^96 * 2^96 * 2^94 =
//! 2^286`, so 384 bits never overflow for the uses in this crate. The
//! operations still check and report overflow instead of wrapping.

use crate::error::{ArbitrageError, Result};
use crate::exact::MAX_SCALE;
use crate::money::Rounding;
use core::cmp::Ordering;
use rust_decimal::Decimal;

const LIMBS: usize = 6;

/// Little-endian 64-bit limbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct U384([u64; LIMBS]);

impl U384 {
    #[cfg(test)]
    pub(crate) const ZERO: Self = Self([0; LIMBS]);

    pub(crate) fn from_u128(x: u128) -> Self {
        let mut limbs = [0; LIMBS];
        limbs[0] = x as u64;
        limbs[1] = (x >> 64) as u64;
        Self(limbs)
    }

    /// The magnitude of a `Decimal`'s mantissa.
    pub(crate) fn from_mantissa(d: Decimal) -> Self {
        Self::from_u128(d.mantissa().unsigned_abs())
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.0.iter().all(|l| *l == 0)
    }

    fn to_u128(self) -> Option<u128> {
        if self.0[2..].iter().any(|l| *l != 0) {
            return None;
        }
        Some(u128::from(self.0[0]) | (u128::from(self.0[1]) << 64))
    }

    /// `self * other`, or `None` on overflow.
    pub(crate) fn checked_mul(&self, other: &Self) -> Option<Self> {
        let mut out = [0u64; LIMBS * 2];
        for (i, a) in self.0.iter().enumerate() {
            let mut carry = 0u128;
            for (j, b) in other.0.iter().enumerate() {
                let cur = u128::from(out[i + j]) + u128::from(*a) * u128::from(*b) + carry;
                out[i + j] = cur as u64;
                carry = cur >> 64;
            }
            out[i + LIMBS] = carry as u64;
        }
        if out[LIMBS..].iter().any(|l| *l != 0) {
            return None;
        }
        let mut limbs = [0; LIMBS];
        limbs.copy_from_slice(&out[..LIMBS]);
        Some(Self(limbs))
    }

    /// `self * 10^k`, or `None` on overflow.
    pub(crate) fn checked_mul_pow10(&self, k: u32) -> Option<Self> {
        let ten = Self::from_u128(10);
        let mut r = *self;
        for _ in 0..k {
            r = r.checked_mul(&ten)?;
        }
        Some(r)
    }

    /// `self + 1`, or `None` on overflow.
    fn checked_add_one(&self) -> Option<Self> {
        let mut limbs = self.0;
        for l in &mut limbs {
            let (v, carry) = l.overflowing_add(1);
            *l = v;
            if !carry {
                return Some(Self(limbs));
            }
        }
        None
    }

    /// `(self / 10, self % 10)`.
    fn div_rem_10(&self) -> (Self, u64) {
        let mut q = [0u64; LIMBS];
        let mut rem = 0u128;
        for i in (0..LIMBS).rev() {
            let cur = (rem << 64) | u128::from(self.0[i]);
            q[i] = (cur / 10) as u64;
            rem = cur % 10;
        }
        (Self(q), rem as u64)
    }
}

/// The exact product of `values` as `(magnitude, scale, negative)`.
///
/// # Errors
///
/// [`ArbitrageError::Overflow`] if the magnitude exceeds 384 bits (at most
/// three `Decimal` factors always fit: `3 * 96 = 288` bits).
pub(crate) fn product(values: &[Decimal], op: &'static str) -> Result<(U384, u32, bool)> {
    let mut m = U384::from_u128(1);
    let mut scale = 0u32;
    let mut negative = false;
    for v in values {
        m = m
            .checked_mul(&U384::from_mantissa(*v))
            .ok_or(ArbitrageError::Overflow { op })?;
        scale += v.scale();
        negative ^= v.is_sign_negative() && !v.is_zero();
    }
    if m.is_zero() {
        negative = false;
    }
    Ok((m, scale, negative))
}

/// Rounds `±m / 10^scale` once to `dp` decimal places with `mode`, returning
/// the rounded magnitude at scale `min(scale, dp)`.
pub(crate) fn round_to_dp(
    m: U384,
    scale: u32,
    negative: bool,
    dp: u32,
    mode: Rounding,
) -> Result<(U384, u32)> {
    if scale <= dp {
        return Ok((m, scale));
    }
    // Drop scale - dp digits, remembering the most significant dropped digit
    // and whether any digit below it was non-zero.
    let mut q = m;
    let mut top = 0u64;
    let mut below = false;
    for _ in 0..(scale - dp) {
        below |= top != 0;
        let (next, digit) = q.div_rem_10();
        top = digit;
        q = next;
    }
    let inexact = top != 0 || below;
    let away = match mode {
        Rounding::Up => inexact && !negative,
        Rounding::Down => inexact && negative,
        Rounding::TowardZero => false,
        Rounding::HalfEven => {
            let above_half = top > 5 || (top == 5 && below);
            let at_half = top == 5 && !below;
            above_half || (at_half && q.0[0] & 1 == 1)
        }
    };
    if away {
        q = q
            .checked_add_one()
            .ok_or(ArbitrageError::Overflow { op: "round" })?;
    }
    Ok((q, dp))
}

impl PartialOrd for U384 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for U384 {
    fn cmp(&self, other: &Self) -> Ordering {
        for i in (0..LIMBS).rev() {
            match self.0[i].cmp(&other.0[i]) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        Ordering::Equal
    }
}

/// The `Decimal` equal to `±m / 10^scale`, normalized.
///
/// Trailing zeros are stripped first, so a value is rejected only when no
/// `Decimal` equals it.
///
/// # Errors
///
/// - [`ArbitrageError::Overflow`] if the value is out of `Decimal`'s range.
/// - [`ArbitrageError::Inexact`] if it is in range but needs more than 28
///   decimal places or 96 bits of mantissa at its shortest scale.
pub(crate) fn to_decimal(m: U384, scale: u32, negative: bool, op: &'static str) -> Result<Decimal> {
    let mut m = m;
    let mut scale = scale;
    while scale > 0 && !m.is_zero() {
        let (q, r) = m.div_rem_10();
        if r != 0 {
            break;
        }
        m = q;
        scale -= 1;
    }
    if m.is_zero() {
        return Ok(Decimal::ZERO);
    }
    let fits = m
        .to_u128()
        .filter(|v| *v < (1u128 << 96))
        .filter(|_| scale <= MAX_SCALE);
    match fits {
        Some(v) => {
            // v < 2^96 and scale <= 28, so this cannot fail.
            let magnitude = i128::try_from(v).map_err(|_| ArbitrageError::Overflow { op })?;
            let signed = if negative { -magnitude } else { magnitude };
            Decimal::try_from_i128_with_scale(signed, scale)
                .map_err(|_| ArbitrageError::Overflow { op })
                .map(|d| d.normalize())
        }
        None => {
            // Out of range if the integer part alone needs more than 96 bits.
            let ten_s = U384::from_u128(1)
                .checked_mul_pow10(scale)
                .ok_or(ArbitrageError::Overflow { op })?;
            let max_plus_one = U384::from_u128(1u128 << 96);
            let limit = max_plus_one
                .checked_mul(&ten_s)
                .ok_or(ArbitrageError::Overflow { op })?;
            if m >= limit {
                Err(ArbitrageError::Overflow { op })
            } else {
                Err(ArbitrageError::Inexact { op })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn mul_and_compare() {
        let a = U384::from_u128(u128::MAX);
        let sq = a.checked_mul(&a).unwrap();
        assert!(sq > a);
        let b = a.checked_mul_pow10(28).unwrap();
        assert!(b > a);
        let big = U384::from_u128(1 << 127).checked_mul_pow10(76).unwrap();
        assert!(big.checked_mul(&big).is_none());
        assert_eq!(U384::ZERO.checked_mul(&a).unwrap(), U384::ZERO);
    }

    fn round(x: Decimal, dp: u32, mode: Rounding) -> Decimal {
        let (m, s, neg) = product(&[x], "t").unwrap();
        let (m, s) = round_to_dp(m, s, neg, dp, mode).unwrap();
        to_decimal(m, s, neg, "t").unwrap()
    }

    #[test]
    fn single_rounding_matches_rust_decimal_on_representable_values() {
        use rust_decimal::RoundingStrategy as R;
        let cases = [
            dec!(1.005),
            dec!(-1.005),
            dec!(1.015),
            dec!(-1.015),
            dec!(2.5),
            dec!(-2.5),
            dec!(3.5),
            dec!(0.0049999),
            dec!(-0.0050001),
            dec!(123.456789),
            dec!(0),
        ];
        for x in cases {
            for (mode, strategy) in [
                (Rounding::Up, R::ToPositiveInfinity),
                (Rounding::Down, R::ToNegativeInfinity),
                (Rounding::TowardZero, R::ToZero),
                (Rounding::HalfEven, R::MidpointNearestEven),
            ] {
                for dp in [0, 1, 2, 4] {
                    assert_eq!(
                        round(x, dp, mode),
                        x.round_dp_with_strategy(dp, strategy),
                        "{x} dp {dp} {mode:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn product_of_three_is_exact_beyond_28_places() {
        // 0.00000001 * 0.0000000001 * 0.00000000000001 = 1e-32, scale 32.
        let (m, s, neg) = product(
            &[dec!(0.00000001), dec!(0.0000000001), dec!(0.00000000000001)],
            "t",
        )
        .unwrap();
        assert_eq!((m, s, neg), (U384::from_u128(1), 32, false));
        // Rounded once to 30 places it is still 1e-32 > 0, so Up gives 1e-30.
        let (r, rs) = round_to_dp(m, s, neg, 30, Rounding::Up).unwrap();
        assert_eq!((r, rs), (U384::from_u128(1), 30));
        let (_, _, neg) = product(&[dec!(-2), dec!(3), dec!(-4)], "t").unwrap();
        assert!(!neg);
        let (m, _, neg) = product(&[dec!(-2), dec!(0)], "t").unwrap();
        assert!(m.is_zero() && !neg);
    }

    #[test]
    fn to_decimal_strips_zeros_and_classifies() {
        // 9.9999999999999999999999999990 at scale 28 does not fit, but
        // 9.999999999999999999999999999 at scale 27 does.
        let m = U384::from_u128(99_999_999_999_999_999_999_999_999_990);
        assert_eq!(
            to_decimal(m, 28, false, "t").unwrap(),
            dec!(9.999999999999999999999999999)
        );
        assert_eq!(
            to_decimal(U384::from_u128(1500), 3, true, "t").unwrap(),
            dec!(-1.5)
        );
        assert_eq!(
            to_decimal(U384::ZERO, 40, false, "t").unwrap(),
            Decimal::ZERO
        );
        // 2^96 at scale 0 is out of range; at scale 1 it is in range but
        // needs 97 bits.
        let two96 = U384::from_u128(1 << 96);
        assert_eq!(
            to_decimal(two96, 0, false, "t"),
            Err(ArbitrageError::Overflow { op: "t" })
        );
        assert_eq!(
            to_decimal(
                two96.checked_mul(&U384::from_u128(3)).unwrap(),
                1,
                false,
                "t"
            ),
            Err(ArbitrageError::Inexact { op: "t" })
        );
        // 1e-29 needs 29 places.
        assert_eq!(
            to_decimal(U384::from_u128(1), 29, false, "t"),
            Err(ArbitrageError::Inexact { op: "t" })
        );
        assert_eq!(
            to_decimal(U384::from_u128((1 << 96) - 1), 0, false, "t").unwrap(),
            Decimal::MAX
        );
    }
}
