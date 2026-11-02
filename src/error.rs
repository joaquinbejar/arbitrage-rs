//! Typed errors of the crate.

use crate::money::Currency;
use rust_decimal::Decimal;

/// Result alias used across the crate.
pub type Result<T> = core::result::Result<T, ArbitrageError>;

/// Every failure the crate reports.
///
/// Arithmetic never rounds silently: an operation whose exact result does not
/// fit in a [`Decimal`] returns [`ArbitrageError::Overflow`] or
/// [`ArbitrageError::Inexact`] instead of a rounded value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ArbitrageError {
    /// The result is outside the range of [`Decimal`].
    #[error("arithmetic overflow in {op}")]
    Overflow {
        /// Operation that overflowed, for example `"add"` or `"mul"`.
        op: &'static str,
    },

    /// The exact result needs more precision than [`Decimal`] holds at the
    /// operands' scale (at most 28 decimal places and a 96-bit mantissa).
    ///
    /// The check is conservative: in rare cases a result that could be stored
    /// exactly at a smaller scale is also reported, never the reverse.
    #[error("inexact result in {op}: the exact value does not fit in a Decimal")]
    Inexact {
        /// Operation that would have rounded.
        op: &'static str,
    },

    /// Two monetary amounts in different currencies were combined.
    #[error("currency mismatch: {left} vs {right}")]
    CurrencyMismatch {
        /// Currency of the left operand.
        left: Currency,
        /// Currency of the right operand.
        right: Currency,
    },

    /// A currency code is empty, too long or has characters outside `A-Z0-9`.
    #[error("invalid currency code {code:?}: expected 1 to 12 characters in A-Z or 0-9")]
    InvalidCurrency {
        /// The rejected code.
        code: String,
    },

    /// A numeric input breaks the invariant of its type.
    #[error("invalid {field} {value}: {reason}")]
    InvalidValue {
        /// Name of the field or type, for example `"price"`.
        field: &'static str,
        /// The rejected value.
        value: Decimal,
        /// The invariant that failed, for example `"must be positive"`.
        reason: &'static str,
    },
}
