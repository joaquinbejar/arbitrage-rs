//! Conversion of a `financial_types` instrument specification into the types
//! the kernels take (feature `instruments`).
//!
//! Everything here is a conversion: nothing is rounded, rescaled, converted
//! between currencies or reinterpreted between contracts and base units, and
//! no kernel changes. What cannot be represented, or what a kernel could not
//! price consistently, is a typed error.
//!
//! | Function | Gives | Accepts |
//! |---|---|---|
//! | [`Instrument::try_from`] | identity and expiry | every kind |
//! | [`currencies`] | underlying, price, settlement and premium currency | every kind |
//! | [`perp_convention`] | [`PerpConvention`] | linear and inverse perpetuals |
//! | [`contract_terms`] | contract size and lot, for parity and packages | see below |
//! | [`base_quantity`] | contracts times the multiplier | see below |
//!
//! # Which `financial_types`
//!
//! This module needs a `financial_types` with its `instruments` feature, 0.3.1
//! or later. The dependency stays `"0.3"`: with `instruments` on, Cargo refuses
//! a version without that feature when it resolves, naming the missing feature,
//! and without it 0.3.0 is still accepted. A lockfile already at 0.3.0 is not
//! upgraded by itself: adding `instruments` fails to resolve until
//! `cargo update -p financial_types` is run.
//!
//! # What an [`Instrument`] certifies
//!
//! Only its identifier and its expiry. A converted [`Instrument`] says
//! nothing about the tick or the lot, the strike, call or put, the exercise
//! style, or whether the contract can be priced or used in parity: an
//! option converts, and that promises no pricing or exercise support.
//!
//! # Refused by [`contract_terms`] and [`base_quantity`]
//!
//! Both accept only a linear contract sized in the underlying, and not an
//! American option:
//!
//! - an inverse future or perpetual is sized in value, so a size in base units
//!   would reinterpret it;
//! - an inverse option is sized in the underlying but its premium is in the
//!   underlying too, while [`ParityInput`](crate::parity::ParityInput) has
//!   every price and fee in one quote currency: its premiums would mix BTC
//!   with USD and still produce a number;
//! - the parity relation holds for European exercise only, and
//!   [`evaluate_parity`](crate::parity::evaluate_parity) does not check the
//!   style.
//!
//! # Where the types differ
//!
//! A valid specification can still fail here:
//!
//! - its expiry is a `u64`, a [`Timestamp`] an `i64`: above `i64::MAX` is
//!   [`ArbitrageError::ExpiryOutOfRange`], never a cast;
//! - its asset codes have up to 16 characters, a [`Currency`] up to
//!   [`MAX_CURRENCY_LEN`](crate::MAX_CURRENCY_LEN):
//!   a longer one is [`ArbitrageError::InvalidCurrency`].
//!
//! # Units
//!
//! The units differ per kernel, and confusing them is silent:
//!
//! | Figure | Unit |
//! |---|---|
//! | `position` of [`funding_cashflow`](crate::funding::funding_cashflow), [`PerpConvention::Linear`] | signed base units: a multiplier of 0.001 is off by 1000 if contracts are passed |
//! | `position` of [`funding_cashflow`](crate::funding::funding_cashflow), [`PerpConvention::Inverse`] | signed contracts |
//! | mark of a [`PerpConvention::Inverse`] | the price asset per unit of base; [`ContractValue`] does not carry that asset, so the caller keeps them consistent |
//! | [`Level`](crate::Level) size in parity and packages | contracts |
//! | `target_quantity` and [`Level`](crate::Level) size in `basis` and `opportunity` | base units, on base unit lot grids |
//! | exposure, band, `lot_size` and [`Level`](crate::Level) size in `hedge` | units of the hedge instrument's quantity, as the caller chooses: all in contracts or all in base units, never mixed |
//! | [`base_quantity`] | base units and unsigned: the caller applies the direction |
//! | [`ContractTerms::lot`], `InstrumentSpec::lot` | contracts, and a step: not a minimum order size |
//!
//! # The grid
//!
//! The kernels have no tick, and treat lots as a caller precondition. Check
//! orders with `InstrumentSpec::check_price` and `InstrumentSpec::check_lot`.

use financial_types::instrument::{
    ContractKind, ExerciseStyle, InstrumentSpec, Margining, SizeKind,
};

use crate::error::{ArbitrageError, Result};
use crate::exact;
use crate::funding::{ContractValue, PerpConvention};
use crate::instrument::{Instrument, InstrumentId, Timestamp};
use crate::money::Currency;
use crate::units::Quantity;

/// The currencies of a specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpecCurrencies {
    /// The underlying asset.
    pub underlying: Currency,
    /// The asset prices, marks, strikes and settlement prices are in. Not
    /// the premium currency of an inverse option: see
    /// [`SpecCurrencies::premium`].
    pub price: Currency,
    /// The asset cashflows are paid in.
    pub settlement: Currency,
    /// The asset an option's premium is in; `None` for anything else.
    pub premium: Option<Currency>,
}

/// The size terms of a contract, as parity and package legs take them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContractTerms {
    /// Units of the underlying per contract (the multiplier).
    pub contract_size: Quantity,
    /// Order size step, in contracts. A step, not a minimum order size.
    pub lot: Quantity,
}

#[cold]
#[inline(never)]
fn unsupported(spec: &InstrumentSpec, reason: &'static str) -> ArbitrageError {
    ArbitrageError::UnsupportedSpec {
        instrument: spec.id().as_str().to_owned(),
        kind: spec.kind(),
        margining: spec.margining(),
        reason,
    }
}

/// Converts the identity and the expiry of a specification, and nothing
/// else; see the [module documentation](self) for what this does not certify.
///
/// # Errors
///
/// - [`ArbitrageError::ExpiryOutOfRange`] if the expiry is above `i64::MAX`.
/// - [`ArbitrageError::InvalidInstrument`] if the identifier is refused,
///   which a validated specification's cannot be: both crates apply the same
///   rule.
///
/// # Examples
///
/// ```
/// use arbitrage_rs::{Instrument, Timestamp};
/// use financial_types::instrument::InstrumentId;
/// use financial_types::instrument::reference::deribit;
///
/// let id = InstrumentId::new("BTC-27NOV26")?;
/// let spec = deribit::btc_future(id, 1_795_766_400_000)?;
/// let instrument = Instrument::try_from(&spec)?;
/// assert_eq!(instrument.id.as_str(), "BTC-27NOV26");
/// assert_eq!(instrument.expiry, Some(Timestamp::from_millis(1_795_766_400_000)));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
impl TryFrom<&InstrumentSpec> for Instrument {
    type Error = ArbitrageError;

    fn try_from(spec: &InstrumentSpec) -> Result<Self> {
        let id = InstrumentId::new(spec.id().as_str())?;
        match spec.expiry_ms() {
            None => Ok(Self::undated(id)),
            Some(expiry_ms) => {
                let millis =
                    i64::try_from(expiry_ms).map_err(|_| ArbitrageError::ExpiryOutOfRange {
                        instrument: spec.id().as_str().to_owned(),
                        expiry_ms,
                    })?;
                Ok(Self::dated(id, Timestamp::from_millis(millis)))
            }
        }
    }
}

/// The currencies of a specification.
///
/// # Errors
///
/// [`ArbitrageError::InvalidCurrency`] if an asset code is longer than a
/// [`Currency`] holds.
///
/// # Examples
///
/// An inverse option is priced in USD, with its premium in BTC:
///
/// ```
/// use arbitrage_rs::spec::currencies;
/// use financial_types::OptionStyle;
/// use financial_types::instrument::InstrumentId;
/// use financial_types::instrument::reference::deribit;
/// use rust_decimal_macros::dec;
///
/// let id = InstrumentId::new("BTC-27NOV26-100000-C")?;
/// let spec = deribit::btc_option(id, 1_795_766_400_000, dec!(100000), OptionStyle::Call)?;
/// let currencies = currencies(&spec)?;
/// assert_eq!(currencies.price.as_str(), "USD");
/// assert_eq!(currencies.premium.map(|c| c.to_string()).as_deref(), Some("BTC"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn currencies(spec: &InstrumentSpec) -> Result<SpecCurrencies> {
    Ok(SpecCurrencies {
        underlying: Currency::new(spec.underlying().as_str())?,
        price: Currency::new(spec.price_asset().as_str())?,
        settlement: Currency::new(spec.settlement_asset().as_str())?,
        premium: spec
            .option()
            .map(|terms| Currency::new(terms.premium_asset().as_str()))
            .transpose()?,
    })
}

/// The funding convention of a perpetual.
///
/// A linear perpetual takes its position in base units, whatever its
/// multiplier: convert it with [`base_quantity`]. An inverse perpetual takes
/// it in contracts, and its mark in the price asset.
///
/// # Errors
///
/// - [`ArbitrageError::UnsupportedSpec`] for anything but a perpetual sized
///   as its margining requires.
/// - The errors of [`ContractValue::new`] and [`Currency::new`].
///
/// # Examples
///
/// ```
/// use arbitrage_rs::funding::PerpConvention;
/// use arbitrage_rs::spec::perp_convention;
/// use financial_types::instrument::reference::{binance_usdm, deribit};
/// use rust_decimal_macros::dec;
///
/// let linear = perp_convention(&binance_usdm::btcusdt_perpetual()?)?;
/// assert_eq!(linear, PerpConvention::Linear);
///
/// let inverse = perp_convention(&deribit::btc_perpetual()?)?;
/// let PerpConvention::Inverse { contract_value, base } = inverse else {
///     return Err("expected an inverse convention".into());
/// };
/// assert_eq!(contract_value.value(), dec!(10));
/// assert_eq!(base.as_str(), "BTC");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn perp_convention(spec: &InstrumentSpec) -> Result<PerpConvention> {
    match (spec.kind(), spec.margining(), spec.size().kind()) {
        (ContractKind::Perpetual, Margining::Linear, SizeKind::BaseQuantity) => {
            Ok(PerpConvention::Linear)
        }
        (ContractKind::Perpetual, Margining::Inverse, SizeKind::QuoteValue) => {
            Ok(PerpConvention::Inverse {
                contract_value: ContractValue::new(spec.size().amount())?,
                base: Currency::new(spec.settlement_asset().as_str())?,
            })
        }
        // These enums are `#[non_exhaustive]` in `financial_types`, so this
        // arm is required: a kind, a margining or a size added there has no
        // convention until it is mapped here.
        _ => Err(unsupported(
            spec,
            "it is not a perpetual with a funding convention",
        )),
    }
}

/// The multiplier of a contract [`contract_terms`] and [`base_quantity`]
/// accept; see the [module documentation](self).
fn multiplier(spec: &InstrumentSpec) -> Result<Quantity> {
    if spec
        .option()
        .is_some_and(|terms| terms.exercise() != ExerciseStyle::European)
    {
        return Err(unsupported(spec, "only european options have these terms"));
    }
    match (spec.kind(), spec.margining(), spec.size().kind()) {
        (
            ContractKind::Spot
            | ContractKind::Future
            | ContractKind::Perpetual
            | ContractKind::Option,
            Margining::Linear,
            SizeKind::BaseQuantity,
        ) => Quantity::new(spec.size().amount()),
        // As in `perp_convention`, required for the foreign enums: a kind, a
        // margining or a size added there has no terms until it is mapped
        // here.
        _ => Err(unsupported(
            spec,
            "only a linear contract sized in the underlying has these terms",
        )),
    }
}

/// The contract size and the lot of a contract, for the `contract_size` and
/// `lot` fields of [`ParityOption`](crate::parity::ParityOption),
/// [`ParityFuture`](crate::parity::ParityFuture) and
/// [`PackageLegSpec`](crate::package::PackageLegSpec).
///
/// # Errors
///
/// - [`ArbitrageError::UnsupportedSpec`] for a contract that is not linear
///   and sized in the underlying, or an American option.
/// - [`ArbitrageError::InvalidValue`] if a value is not positive, which a
///   validated specification's cannot be.
///
/// # Examples
///
/// ```
/// use arbitrage_rs::ArbitrageError;
/// use arbitrage_rs::spec::contract_terms;
/// use financial_types::instrument::reference::{binance_usdm, deribit};
/// use rust_decimal_macros::dec;
///
/// let terms = contract_terms(&binance_usdm::btcusdt_perpetual()?)?;
/// assert_eq!(terms.contract_size.value(), dec!(1));
/// assert_eq!(terms.lot.value(), dec!(0.001));
///
/// // Sized in value: refused, never reinterpreted.
/// assert!(matches!(
///     contract_terms(&deribit::btc_perpetual()?),
///     Err(ArbitrageError::UnsupportedSpec { .. })
/// ));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn contract_terms(spec: &InstrumentSpec) -> Result<ContractTerms> {
    Ok(ContractTerms {
        contract_size: multiplier(spec)?,
        lot: Quantity::new(spec.lot())?,
    })
}

/// `contracts` of a contract in base units: the number of contracts times
/// the multiplier, exactly. Unsigned, like every [`Quantity`]: the caller
/// applies the direction.
///
/// The lot in base units is `base_quantity(&spec, terms.lot)`.
///
/// # Errors
///
/// - [`ArbitrageError::UnsupportedSpec`] as in [`contract_terms`].
/// - [`ArbitrageError::Overflow`] or [`ArbitrageError::Inexact`] if the exact
///   product does not fit, never a rounded value.
///
/// # Examples
///
/// ```
/// use arbitrage_rs::Quantity;
/// use arbitrage_rs::spec::{base_quantity, contract_terms};
/// use financial_types::instrument::reference::binance_usdm;
/// use rust_decimal_macros::dec;
///
/// let spec = binance_usdm::btcusdt_perpetual()?;
/// let contracts = Quantity::new(dec!(0.5))?;
/// assert_eq!(base_quantity(&spec, contracts)?.value(), dec!(0.5));
///
/// let lot = contract_terms(&spec)?.lot;
/// assert_eq!(base_quantity(&spec, lot)?.value(), dec!(0.001));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn base_quantity(spec: &InstrumentSpec, contracts: Quantity) -> Result<Quantity> {
    Quantity::new(exact::mul(contracts.value(), multiplier(spec)?.value())?)
}
