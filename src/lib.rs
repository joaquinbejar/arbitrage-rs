//! # arbitrage-rs
//!
//! Arbitrage models for spot, futures, perpetuals and options: pricing
//! relations, opportunity detection and hedge plans.
//!
//! The crate is a set of pure decision kernels: no connector, matching
//! engine or live trading. It contains:
//!
//! - exact monetary types (ARB-00): [`Money`], [`Cost`], [`FeeRate`],
//!   [`Price`], [`Quantity`] and the [`exact`] arithmetic;
//! - [`opportunity::evaluate`] (M1-06): the net edge of buying one instrument
//!   at its ask and selling another at its bid, after fees and carry;
//! - [`hedge::plan_band_hedge`] (M1-06): a one-leg hedge that brings an
//!   exposure back inside a band;
//! - [`funding`] (M3-06): linear perpetual funding cashflows, schedules and
//!   estimated versus published rates;
//! - [`basis::evaluate_spot_perp`] (M3-06): the net edge of a spot-perpetual
//!   basis trade after fees, carry and funding;
//! - [`legs::TwoLegPlan`] (M3-11A): a two-leg execution state machine that
//!   applies observed fills, rejections and cancels without assuming atomic
//!   execution, and emits recovery intents under an explicit policy;
//! - [`venues`] (M3-11B): per-venue views and budgets, and all-or-nothing
//!   admission of both legs without borrowing capital across venues;
//! - [`hedge_action::hedge_plan_from_action`] (M4-04): a bounded, audited
//!   conversion of an external policy's hedge action into a hedge plan;
//! - [`parity::evaluate_parity`] (#18): the executable edge of
//!   put-call parity against a same-expiry future, in both directions, after
//!   the fees of all three legs;
//! - [`package::PackagePlan`] (#19): a multi-leg package execution
//!   state machine with legs of different contract sizes, a matched package
//!   on a common lot grid, a residual per leg and recovery.
//!
//! Opportunities and hedge plans are intents with expected numbers. They are
//! never reported as executed or guaranteed profit, and execution is not
//! assumed to be atomic.
//!
//! ## Arithmetic
//!
//! - Every amount, price, size and rate is a [`Decimal`] (96-bit mantissa,
//!   up to 28 decimal places). There is no floating point in the crate;
//!   `clippy::float_arithmetic` is denied.
//! - Arithmetic is exact or fails. `rust_decimal`'s `checked_*` operations
//!   round silently when a result needs more precision than a `Decimal`
//!   holds; the [`exact`] module detects that and returns
//!   [`ArbitrageError::Inexact`]. Out-of-range results return
//!   [`ArbitrageError::Overflow`].
//! - A figure built from more than two terms (a sum of products, or a
//!   product of more than two factors) is formed exactly in wide integers
//!   and converted or rounded **once**, so an intermediate never fails when
//!   the result is representable ([`parity`] follows this rule).
//! - Rounding happens only where a caller asks for it, with an explicit
//!   [`Rounding`] mode (for example a fee rounded up to a venue's quantum).
//! - Values are stored normalized, so equal amounts compare, hash and print
//!   the same.
//!
//! ## Units and signs
//!
//! - [`Money`] is a signed amount tagged with its [`Currency`]. Adding or
//!   subtracting different currencies is an error; the crate never converts.
//! - [`Price`] is quote currency per unit of [`Quantity`]; both are strictly
//!   positive. A quantity has no direction: legs and positions carry it.
//!   Contract multipliers are applied by the caller before building a
//!   quantity.
//! - [`Cost`] is signed from the payer's side: positive is paid, negative is
//!   received (a rebate). A net result is `gross - cost`.
//! - [`FeeRate`] is in basis points of traded notional, within
//!   `[-10_000, 10_000]`; negative rates are rebates. The fee does not depend
//!   on the trade side.
//!
//! ## Dependency direction
//!
//! arbitrage-rs depends only on `rust_decimal` and `thiserror`. It does not
//! depend on market-maker-rs, margin-rs, tessera-rl or any simulator or
//! backtest engine. Integration goes the other way: those crates, or adapters
//! next to them, depend on arbitrage-rs and translate their prices,
//! inventory and ledgers into its types.
//!
//! ## Versioning
//!
//! The crate is pre-release (`0.1.0`, `publish = false`); the public API may
//! change in any commit until a release is scoped separately. [`Decimal`] is
//! re-exported and appears in the public API, so `rust_decimal` 1.x is part
//! of it. The minimum
//! supported Rust version is 1.85 (edition 2024) and is checked in CI.
//!
//! ## Example
//!
//! A spread that is positive before fees and negative after them:
//!
//! ```
//! use arbitrage_rs::{Currency, FeeRate, Price, Quantity};
//! use rust_decimal_macros::dec;
//!
//! # fn main() -> Result<(), arbitrage_rs::ArbitrageError> {
//! let usd = Currency::new("USD")?;
//! let size = Quantity::new(dec!(10))?;
//! let buy = Price::new(dec!(100.00))?.notional(size, usd)?;
//! let sell = Price::new(dec!(100.05))?.notional(size, usd)?;
//! let gross = sell.checked_sub(buy)?;
//!
//! let fee = FeeRate::from_bps(dec!(5))?;
//! let cost = fee.fee_on(buy)?.checked_add(fee.fee_on(sell)?)?;
//! let net = cost.net_of(gross)?;
//!
//! assert_eq!(gross.amount(), dec!(0.5));
//! assert_eq!(cost.money().amount(), dec!(1.00025));
//! assert_eq!(net.amount(), dec!(-0.50025));
//! # Ok(())
//! # }
//! ```

pub mod basis;
pub mod error;
pub mod exact;
pub mod funding;
pub mod hedge;
pub mod hedge_action;
pub mod legs;
pub mod opportunity;
pub mod package;
pub mod parity;

mod cost;
mod instrument;
mod money;
mod units;
pub mod venues;
mod wide;

pub use cost::{BPS_PER_UNIT, Cost, FeeRate};
pub use error::{ArbitrageError, Result};
pub use instrument::{
    Instrument, InstrumentId, Leg, Level, MAX_INSTRUMENT_ID_LEN, Side, Timestamp, TopOfBook,
};
pub use money::{Currency, MAX_CURRENCY_LEN, Money, Rounding};
pub use rust_decimal::Decimal;
pub use units::{Price, Quantity};

/// The README examples, compiled and run as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
