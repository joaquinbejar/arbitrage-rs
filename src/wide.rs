//! Unsigned 768-bit integers for exact comparisons, products and sums of
//! products of [`Decimal`] mantissas.
//!
//! A `Decimal` is `m / 10^s` with `m < 2^96` and `s <= 28`. A product of
//! three factors is below `2^288` at a scale of at most 84, and aligning it
//! to a common scale multiplies it by at most `10^84 < 2^280`, so a sum of
//! such products stays far below 768 bits. A product of four factors where
//! one is a power of ten (a basis-point divisor) fits as well. The
//! operations still check and report overflow instead of wrapping.

use crate::error::{ArbitrageError, Result};
use crate::exact::MAX_SCALE;
use crate::money::Rounding;
use core::cmp::Ordering;
use rust_decimal::Decimal;

const LIMBS: usize = 12;

/// The low 64 bits of `x`.
fn lo64(x: u128) -> u64 {
    // The mask makes the value fit, so the fallback is never taken.
    u64::try_from(x & u128::from(u64::MAX)).unwrap_or_default()
}

/// The high 64 bits of `x`.
fn hi64(x: u128) -> u64 {
    lo64(x >> 64)
}

/// `lhs * rhs + addend + carry` as `(low, high)` limbs. The value is at
/// most `(2^64 - 1)^2 + 2 (2^64 - 1) = 2^128 - 1`, so it never wraps.
fn mul_add(lhs: u64, rhs: u64, addend: u64, carry: u64) -> (u64, u64) {
    let value = u128::from(lhs)
        .wrapping_mul(u128::from(rhs))
        .wrapping_add(u128::from(addend))
        .wrapping_add(u128::from(carry));
    (lo64(value), hi64(value))
}

/// Little-endian 64-bit limbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct U768([u64; LIMBS]);

impl U768 {
    pub(crate) const ZERO: Self = Self([0; LIMBS]);

    pub(crate) fn from_u128(x: u128) -> Self {
        let mut limbs = [0; LIMBS];
        let [l0, l1, ..] = &mut limbs;
        *l0 = lo64(x);
        *l1 = hi64(x);
        Self(limbs)
    }

    /// The magnitude of a `Decimal`'s mantissa.
    pub(crate) fn from_mantissa(d: Decimal) -> Self {
        Self::from_u128(d.mantissa().unsigned_abs())
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.0.iter().all(|l| *l == 0)
    }

    fn is_odd(&self) -> bool {
        self.0.first().is_some_and(|l| l & 1 == 1)
    }

    fn to_u128(self) -> Option<u128> {
        let [l0, l1, rest @ ..] = self.0;
        if rest.iter().any(|l| *l != 0) {
            return None;
        }
        Some(u128::from(l0) | (u128::from(l1) << 64))
    }

    /// The limb at `i`, zero outside the number.
    fn limb(&self, i: Option<usize>) -> u64 {
        i.and_then(|i| self.0.get(i)).copied().unwrap_or(0)
    }

    /// `self * other`, or `None` on overflow.
    pub(crate) fn checked_mul(&self, other: &Self) -> Option<Self> {
        let mut out = [0u64; LIMBS * 2];
        for (i, a) in self.0.iter().enumerate() {
            let mut carry = 0u64;
            let mut slots = out.iter_mut().skip(i);
            // `other` first, so the zip stops without consuming a slot.
            for (b, slot) in other.0.iter().zip(slots.by_ref()) {
                let (lo, hi) = mul_add(*a, *b, *slot, carry);
                *slot = lo;
                carry = hi;
            }
            if let Some(slot) = slots.next() {
                *slot = carry;
            }
        }
        if out.iter().skip(LIMBS).any(|l| *l != 0) {
            return None;
        }
        let mut limbs = [0; LIMBS];
        for (l, o) in limbs.iter_mut().zip(out.iter()) {
            *l = *o;
        }
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

    /// Whole limbs and remaining bits of a shift, `None` past the width.
    fn split_shift(bits: u32) -> Option<(usize, u32)> {
        let words = usize::try_from(bits / 64).ok()?;
        (words < LIMBS).then_some((words, bits % 64))
    }

    /// `self << bits`, or `None` if bits are lost.
    fn checked_shl(&self, bits: u32) -> Option<Self> {
        let Some((words, rest)) = Self::split_shift(bits) else {
            return self.is_zero().then_some(*self);
        };
        let mut out = [0u64; LIMBS];
        for (i, slot) in out.iter_mut().enumerate().skip(words) {
            let src = i.checked_sub(words);
            // The limb pair (src, src - 1) shifted left; its high half.
            let pair = (u128::from(self.limb(src)) << 64)
                | u128::from(self.limb(src.and_then(|s| s.checked_sub(1))));
            *slot = hi64(pair << rest);
        }
        let back = Self(out).shr(bits);
        if back == *self { Some(Self(out)) } else { None }
    }

    /// `self >> bits`.
    fn shr(&self, bits: u32) -> Self {
        let Some((words, rest)) = Self::split_shift(bits) else {
            return Self::ZERO;
        };
        let mut out = [0u64; LIMBS];
        for (i, slot) in out.iter_mut().enumerate() {
            let src = i.checked_add(words);
            // The limb pair (src + 1, src) shifted right; its low half.
            let pair = (u128::from(self.limb(src.and_then(|s| s.checked_add(1)))) << 64)
                | u128::from(self.limb(src));
            *slot = lo64(pair >> rest);
        }
        Self(out)
    }

    /// `self - other`, for `self >= other`.
    fn wrapping_sub(&self, other: &Self) -> Self {
        let mut out = [0u64; LIMBS];
        let mut borrow = 0u64;
        for ((slot, a), b) in out.iter_mut().zip(self.0.iter()).zip(other.0.iter()) {
            let (v, b1) = a.overflowing_sub(*b);
            let (v, b2) = v.overflowing_sub(borrow);
            *slot = v;
            borrow = u64::from(b1 || b2);
        }
        Self(out)
    }

    /// `self + other`, or `None` on overflow.
    fn checked_add(&self, other: &Self) -> Option<Self> {
        let mut out = [0u64; LIMBS];
        let mut carry = false;
        for ((slot, a), b) in out.iter_mut().zip(self.0.iter()).zip(other.0.iter()) {
            let (v, c1) = a.overflowing_add(*b);
            let (v, c2) = v.overflowing_add(u64::from(carry));
            *slot = v;
            carry = c1 || c2;
        }
        if carry { None } else { Some(Self(out)) }
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
        for (slot, limb) in q.iter_mut().zip(self.0.iter()).rev() {
            // rem < 10, so the shift loses nothing.
            let cur = (rem << 64) | u128::from(*limb);
            *slot = lo64(cur / 10);
            rem = cur % 10;
        }
        (Self(q), lo64(rem))
    }
}

/// The exact product of `values` as `(magnitude, scale, negative)`.
///
/// # Errors
///
/// [`ArbitrageError::Overflow`] if the magnitude exceeds 768 bits (at most
/// eight `Decimal` factors always fit: `8 * 96 = 768` bits).
pub(crate) fn product(values: &[Decimal], op: &'static str) -> Result<(U768, u32, bool)> {
    let mut m = U768::from_u128(1);
    let mut scale = 0u32;
    let mut negative = false;
    for v in values {
        m = m
            .checked_mul(&U768::from_mantissa(*v))
            .ok_or(ArbitrageError::Overflow { op })?;
        scale = scale
            .checked_add(v.scale())
            .ok_or(ArbitrageError::Overflow { op })?;
        negative ^= v.is_sign_negative() && !v.is_zero();
    }
    if m.is_zero() {
        negative = false;
    }
    Ok((m, scale, negative))
}

/// Rounds `±m / 10^scale` once to `dp` decimal places with `mode`, returning
/// the rounded magnitude at scale `min(scale, dp)`.
/// The exact sum of products `sum_i prod_j terms[i][j]` as
/// `(magnitude, scale, negative)`, at the largest scale of the products.
/// A term is negated by including `-1` among its factors. Nothing is
/// rounded; convert once with [`to_decimal`] or round once with
/// [`round_to_dp`].
///
/// # Errors
///
/// [`ArbitrageError::Overflow`] if a product, its alignment or the sum
/// exceeds 768 bits.
pub(crate) fn sum_products(terms: &[&[Decimal]], op: &'static str) -> Result<(U768, u32, bool)> {
    let overflow = ArbitrageError::Overflow { op };
    let products = terms
        .iter()
        .map(|t| product(t, op))
        .collect::<Result<Vec<_>>>()?;
    let scale = products.iter().map(|p| p.1).max().unwrap_or(0);
    let mut acc = U768::ZERO;
    let mut acc_negative = false;
    for (m, s, negative) in products {
        // s <= scale, the largest of the scales.
        let m = scale
            .checked_sub(s)
            .and_then(|k| m.checked_mul_pow10(k))
            .ok_or_else(|| overflow.clone())?;
        if acc_negative == negative || acc.is_zero() {
            acc = acc.checked_add(&m).ok_or_else(|| overflow.clone())?;
            if !m.is_zero() {
                acc_negative = negative;
            }
        } else if acc >= m {
            acc = acc.wrapping_sub(&m);
        } else {
            acc = m.wrapping_sub(&acc);
            acc_negative = negative;
        }
    }
    if acc.is_zero() {
        acc_negative = false;
    }
    Ok((acc, scale, acc_negative))
}

/// The exact sum of products (see [`sum_products`]) as a `Decimal`.
///
/// # Errors
///
/// [`ArbitrageError::Overflow`] or [`ArbitrageError::Inexact`] only if the
/// exact result does not fit in a `Decimal`.
pub(crate) fn sum_products_exact(terms: &[&[Decimal]], op: &'static str) -> Result<Decimal> {
    let (m, scale, negative) = sum_products(terms, op)?;
    to_decimal(m, scale, negative, op)
}

/// The exact sum of `values`, converted once.
///
/// # Errors
///
/// As [`sum_products_exact`].
pub(crate) fn sum(values: &[Decimal], op: &'static str) -> Result<Decimal> {
    let terms = values.iter().map(core::slice::from_ref).collect::<Vec<_>>();
    sum_products_exact(&terms, op)
}

/// The sum of products (see [`sum_products`]) rounded once to `dp` decimal
/// places with `mode`.
///
/// # Errors
///
/// [`ArbitrageError::Inexact`] if `dp` is above 28, and
/// [`ArbitrageError::Overflow`] if the rounded result does not fit.
pub(crate) fn sum_products_round(
    terms: &[&[Decimal]],
    dp: u32,
    mode: Rounding,
    op: &'static str,
) -> Result<Decimal> {
    if dp > MAX_SCALE {
        return Err(ArbitrageError::Inexact { op });
    }
    let (m, scale, negative) = sum_products(terms, op)?;
    let (m, scale) = round_to_dp(m, scale, negative, dp, mode)?;
    to_decimal(m, scale, negative, op)
}

pub(crate) fn round_to_dp(
    m: U768,
    scale: u32,
    negative: bool,
    dp: u32,
    mode: Rounding,
) -> Result<(U768, u32)> {
    if scale <= dp {
        return Ok((m, scale));
    }
    // Drop scale - dp digits, remembering the most significant dropped digit
    // and whether any digit below it was non-zero.
    let mut q = m;
    let mut top = 0u64;
    let mut below = false;
    // scale > dp here.
    for _ in 0..scale.saturating_sub(dp) {
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
            above_half || (at_half && q.is_odd())
        }
    };
    if away {
        q = q
            .checked_add_one()
            .ok_or(ArbitrageError::Overflow { op: "round" })?;
    }
    Ok((q, dp))
}

impl PartialOrd for U768 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for U768 {
    fn cmp(&self, other: &Self) -> Ordering {
        for (a, b) in self.0.iter().zip(other.0.iter()).rev() {
            match a.cmp(b) {
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
pub(crate) fn to_decimal(m: U768, scale: u32, negative: bool, op: &'static str) -> Result<Decimal> {
    let mut m = m;
    let mut scale = scale;
    while scale > 0 && !m.is_zero() {
        let (q, r) = m.div_rem_10();
        if r != 0 {
            break;
        }
        m = q;
        // scale > 0 in the loop.
        scale = scale.saturating_sub(1);
    }
    if m.is_zero() {
        return Ok(Decimal::ZERO);
    }
    let fits = m
        .to_u128()
        .filter(|v| *v < (1u128 << 96))
        .filter(|_| scale <= MAX_SCALE);
    if let Some(v) = fits {
        // v < 2^96 and scale <= 28, so this cannot fail.
        let magnitude = i128::try_from(v).map_err(|_| ArbitrageError::Overflow { op })?;
        let signed = if negative {
            magnitude
                .checked_neg()
                .ok_or(ArbitrageError::Overflow { op })?
        } else {
            magnitude
        };
        Decimal::try_from_i128_with_scale(signed, scale)
            .map_err(|_| ArbitrageError::Overflow { op })
            .map(|d| d.normalize())
    } else {
        // Out of range if the integer part alone needs more than 96 bits.
        let ten_s = U768::from_u128(1)
            .checked_mul_pow10(scale)
            .ok_or(ArbitrageError::Overflow { op })?;
        let max_plus_one = U768::from_u128(1u128 << 96);
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

/// The exact value of a finite, non-negative `f64`, rounded **once**, half
/// to even, to `dp` decimal places. Returns the rounded `Decimal` and
/// whether rounding changed the value.
///
/// The float is decomposed into its integer mantissa `m` and binary exponent
/// `e` (`x = m * 2^e`), so no intermediate decimal approximation is made:
/// `round(m * 10^dp / 2^-e)` is computed in 768-bit integers.
///
/// # Errors
///
/// [`ArbitrageError::InvalidInput`] if `x` is not finite, is negative, `dp`
/// is above 28, or the value is outside `Decimal`'s range.
pub(crate) fn f64_to_decimal_half_even(x: f64, dp: u32) -> Result<(Decimal, bool)> {
    let unsupported = |reason| ArbitrageError::InvalidInput { reason };
    if !x.is_finite() {
        return Err(unsupported("action quantity is not finite"));
    }
    if x.is_sign_negative() {
        return Err(unsupported("float is negative"));
    }
    if dp > MAX_SCALE {
        return Err(unsupported("float_dp must be at most 28"));
    }
    let bits = x.to_bits();
    let exp_bits = i64::try_from((bits >> 52) & 0x7ff).unwrap_or_default();
    let frac = bits & ((1u64 << 52) - 1);
    let out_of_range = || unsupported("action quantity is outside the supported Decimal range");
    let (mantissa, exponent) = if exp_bits == 0 {
        (frac, -1074i64)
    } else {
        // exp_bits is at most 0x7ff.
        (
            frac | (1u64 << 52),
            exp_bits.checked_sub(1075).ok_or_else(out_of_range)?,
        )
    };
    if mantissa == 0 {
        return Ok((Decimal::ZERO, false));
    }
    let mantissa = U768::from_u128(u128::from(mantissa));
    if exponent >= 0 {
        // An integer: exact at scale 0 if it fits.
        let shift = u32::try_from(exponent).map_err(|_| out_of_range())?;
        let value = mantissa.checked_shl(shift).ok_or_else(out_of_range)?;
        let decimal = to_decimal(value, 0, false, "float").map_err(|_| out_of_range())?;
        return Ok((decimal, false));
    }
    let shift = exponent
        .checked_neg()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(out_of_range)?;
    // numerator = mantissa * 10^dp < 2^53 * 2^94; a quotient by 2^shift
    // with shift above 300 is below one half, so it rounds to zero.
    let numerator = mantissa.checked_mul_pow10(dp).ok_or_else(out_of_range)?;
    if shift > 300 {
        return Ok((Decimal::ZERO, true));
    }
    let quotient = numerator.shr(shift);
    let back = quotient.checked_shl(shift).ok_or_else(out_of_range)?;
    let remainder = numerator.wrapping_sub(&back);
    // shift >= 1 here (the exponent is negative).
    let half = shift
        .checked_sub(1)
        .and_then(|h| U768::from_u128(1).checked_shl(h))
        .ok_or_else(out_of_range)?;
    let inexact = !remainder.is_zero();
    let up = match remainder.cmp(&half) {
        Ordering::Greater => true,
        Ordering::Equal => quotient.is_odd(),
        Ordering::Less => false,
    };
    let rounded = if up {
        quotient.checked_add_one().ok_or_else(out_of_range)?
    } else {
        quotient
    };
    let decimal = to_decimal(rounded, dp, false, "float").map_err(|_| out_of_range())?;
    Ok((decimal, inexact))
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

    /// The limb loops of U768 against native `u128` arithmetic on values
    /// that fit, over a fixed xorshift sequence.
    #[test]
    fn limb_arithmetic_matches_u128() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let a = (u128::from(next()) << 64) | u128::from(next());
            let b = u128::from(next()) >> (next() % 64);
            let shift = u32::try_from(next() % 140).unwrap();
            let (ua, ub) = (U768::from_u128(a), U768::from_u128(b));
            assert_eq!(ua.cmp(&ub), a.cmp(&b));
            let product = ua.checked_mul(&ub).unwrap();
            assert_eq!(product.to_u128(), a.checked_mul(b));
            assert_eq!(ua.checked_add(&ub).unwrap().to_u128(), a.checked_add(b));
            if a >= b {
                assert_eq!(ua.wrapping_sub(&ub).to_u128(), Some(a.wrapping_sub(b)));
            }
            let (q, r) = ua.div_rem_10();
            assert_eq!(
                (q.to_u128(), r),
                (Some(a / 10), u64::try_from(a % 10).unwrap())
            );
            assert_eq!(
                ua.shr(shift).to_u128(),
                Some(a.checked_shr(shift).unwrap_or(0))
            );
            let shifted = ua.checked_shl(shift).unwrap();
            assert_eq!(shifted.shr(shift), ua);
            if shift < 128 && (a << shift) >> shift == a {
                assert_eq!(shifted.to_u128(), Some(a << shift));
            }
            assert_eq!(ua.is_odd(), a % 2 == 1);
        }
        // Past the width: only zero shifts losslessly.
        assert_eq!(U768::ZERO.checked_shl(800), Some(U768::ZERO));
        assert_eq!(
            U768::from_u128(1).checked_shl(767).map(|v| v.shr(767)),
            Some(U768::from_u128(1))
        );
        assert_eq!(U768::from_u128(2).checked_shl(767), None);
        assert_eq!(U768::from_u128(1).shr(768), U768::ZERO);
        // The top limb carries out of a product.
        let top = U768::from_u128(1).checked_shl(700).unwrap();
        assert_eq!(top.checked_mul(&top), None);
    }

    #[test]
    fn mul_and_compare() {
        let a = U768::from_u128(u128::MAX);
        let sq = a.checked_mul(&a).unwrap();
        assert!(sq > a);
        let b = a.checked_mul_pow10(28).unwrap();
        assert!(b > a);
        let big = U768::from_u128(1 << 127).checked_mul_pow10(78).unwrap();
        assert!(big.checked_mul(&big).is_none());
        assert_eq!(U768::ZERO.checked_mul(&a).unwrap(), U768::ZERO);
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
        assert_eq!((m, s, neg), (U768::from_u128(1), 32, false));
        // Rounded once to 30 places it is still 1e-32 > 0, so Up gives 1e-30.
        let (r, rs) = round_to_dp(m, s, neg, 30, Rounding::Up).unwrap();
        assert_eq!((r, rs), (U768::from_u128(1), 30));
        let (_, _, neg) = product(&[dec!(-2), dec!(3), dec!(-4)], "t").unwrap();
        assert!(!neg);
        let (m, _, neg) = product(&[dec!(-2), dec!(0)], "t").unwrap();
        assert!(m.is_zero() && !neg);
    }

    #[test]
    fn to_decimal_strips_zeros_and_classifies() {
        // 9.9999999999999999999999999990 at scale 28 does not fit, but
        // 9.999999999999999999999999999 at scale 27 does.
        let m = U768::from_u128(99_999_999_999_999_999_999_999_999_990);
        assert_eq!(
            to_decimal(m, 28, false, "t").unwrap(),
            dec!(9.999999999999999999999999999)
        );
        assert_eq!(
            to_decimal(U768::from_u128(1500), 3, true, "t").unwrap(),
            dec!(-1.5)
        );
        assert_eq!(
            to_decimal(U768::ZERO, 40, false, "t").unwrap(),
            Decimal::ZERO
        );
        // 2^96 at scale 0 is out of range; at scale 1 it is in range but
        // needs 97 bits.
        let two96 = U768::from_u128(1 << 96);
        assert_eq!(
            to_decimal(two96, 0, false, "t"),
            Err(ArbitrageError::Overflow { op: "t" })
        );
        assert_eq!(
            to_decimal(
                two96.checked_mul(&U768::from_u128(3)).unwrap(),
                1,
                false,
                "t"
            ),
            Err(ArbitrageError::Inexact { op: "t" })
        );
        // 1e-29 needs 29 places.
        assert_eq!(
            to_decimal(U768::from_u128(1), 29, false, "t"),
            Err(ArbitrageError::Inexact { op: "t" })
        );
        assert_eq!(
            to_decimal(U768::from_u128((1 << 96) - 1), 0, false, "t").unwrap(),
            Decimal::MAX
        );
    }

    #[test]
    fn sums_of_products_are_exact_and_signed() {
        let t = |terms: &[&[Decimal]]| sum_products_exact(terms, "t").unwrap();
        assert_eq!(t(&[&[dec!(1.5)], &[dec!(-2.25)], &[dec!(0.75)]]), dec!(0));
        assert_eq!(
            t(&[&[dec!(0.1), dec!(0.2)], &[dec!(-1), dec!(0.03)]]),
            dec!(-0.01)
        );
        assert_eq!(t(&[&[dec!(-3)], &[dec!(1)]]), dec!(-2));
        assert_eq!(t(&[&[dec!(-3)], &[dec!(5)]]), dec!(2));
        assert_eq!(t(&[]), dec!(0));
        // Intermediates beyond Decimal::MAX, result representable.
        let max = Decimal::MAX;
        assert_eq!(
            t(&[&[dec!(2), max], &[dec!(-1), max], &[dec!(-1)]]),
            max - dec!(1)
        );
        // An exact result that is not a Decimal is reported, not rounded.
        assert_eq!(
            sum_products_exact(&[&[dec!(0.0000000000000000000000000001), dec!(0.1)]], "t"),
            Err(ArbitrageError::Inexact { op: "t" })
        );
    }

    #[test]
    fn sums_of_products_round_once() {
        let r = |x: Decimal, mode| sum_products_round(&[&[x]], 0, mode, "t").unwrap();
        assert_eq!(r(dec!(-1.5), Rounding::HalfEven), dec!(-2));
        assert_eq!(r(dec!(-0.5), Rounding::HalfEven), dec!(0));
        assert_eq!(r(dec!(-1.25), Rounding::Down), dec!(-2));
        assert_eq!(r(dec!(-1.25), Rounding::Up), dec!(-1));
        assert_eq!(r(dec!(-1.25), Rounding::TowardZero), dec!(-1));
        assert_eq!(r(dec!(1.25), Rounding::Up), dec!(2));
        // 1e-32 rounds to zero at 6 places.
        assert_eq!(
            sum_products_round(
                &[&[dec!(0.0000000000000000000000000001), dec!(0.0001)]],
                6,
                Rounding::Up,
                "t"
            )
            .unwrap(),
            dec!(0.000001)
        );
        assert_eq!(
            sum_products_round(&[&[dec!(1)]], 29, Rounding::Up, "t"),
            Err(ArbitrageError::Inexact { op: "t" })
        );
    }

    #[test]
    fn sums_of_products_match_decimal_on_small_values() {
        // A fixed linear congruential sequence: no randomness dependency.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = |modulus: i64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            i64::try_from(state >> 33).unwrap() % modulus - modulus / 2
        };
        for _ in 0..5_000 {
            let mut terms = Vec::new();
            for _ in 0..4 {
                let a = Decimal::new(next(2_000_001), u32::try_from(next(9) + 4).unwrap());
                let b = Decimal::new(next(2_000_001), u32::try_from(next(9) + 4).unwrap());
                terms.push([a, b]);
            }
            let expected = terms
                .iter()
                .map(|[a, b]| a * b)
                .fold(Decimal::ZERO, |acc, x| acc + x);
            let refs: Vec<&[Decimal]> = terms.iter().map(<[Decimal; 2]>::as_slice).collect();
            assert_eq!(sum_products_exact(&refs, "t").unwrap(), expected);
        }
    }
}
