//! The `spec` module: conversion of a `financial_types` instrument
//! specification (feature `instruments`, #50).
//!
//! Expected values are literals read from the specifications or computed by
//! hand (shown in the comments), never with the code under test.

#![cfg(feature = "instruments")]
// Tests fail loudly: the library's no-panic lints do not apply here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use arbitrage_rs::funding::{
    FundingEvent, FundingRate, PerpConvention, RateKind, SettlementRounding,
    funding_cashflow_rounded,
};
use arbitrage_rs::parity::{OptionFee, ParityFuture, ParityInput, ParityOption, evaluate_parity};
use arbitrage_rs::spec::{base_quantity, contract_terms, currencies, perp_convention};
use arbitrage_rs::{
    ArbitrageError, Currency, Decimal, FeeRate, Instrument, Level, Price, Quantity, Rounding,
    Timestamp, TopOfBook,
};
use financial_types::OptionStyle;
use financial_types::instrument::reference::{binance_usdm, deribit};
use financial_types::instrument::{
    AssetCode, ContractKind, ContractSize, ExerciseStyle, InstrumentDraft, InstrumentId,
    InstrumentSpec, Margining, OptionTerms, SettlementType, TickSchedule,
};
use rust_decimal_macros::dec;

/// 2026-11-27 08:00 UTC, in milliseconds.
const EXPIRY_MS: u64 = 1_795_766_400_000;

fn code(value: &str) -> AssetCode {
    AssetCode::new(value).unwrap()
}

fn id(value: &str) -> InstrumentId {
    InstrumentId::new(value).unwrap()
}

fn q(value: Decimal) -> Quantity {
    Quantity::new(value).unwrap()
}

/// A linear contract on `underlying` in `price`.
fn linear(
    kind: ContractKind,
    underlying: &str,
    price: &str,
    multiplier: Decimal,
    lot: Decimal,
    expiry_ms: Option<u64>,
    option: Option<OptionTerms>,
) -> InstrumentSpec {
    InstrumentSpec::new(InstrumentDraft {
        id: id("TEST"),
        kind,
        margining: Margining::Linear,
        underlying: code(underlying),
        price_asset: code(price),
        settlement_asset: code(price),
        size: ContractSize::base(multiplier, code(underlying)).unwrap(),
        price_ticks: TickSchedule::fixed(dec!(0.01)).unwrap(),
        lot,
        expiry_ms,
        option,
    })
    .unwrap()
}

fn linear_future(multiplier: Decimal, lot: Decimal, expiry_ms: u64) -> InstrumentSpec {
    linear(
        ContractKind::Future,
        "BTC",
        "USDT",
        multiplier,
        lot,
        Some(expiry_ms),
        None,
    )
}

fn linear_option(
    multiplier: Decimal,
    lot: Decimal,
    style: OptionStyle,
    exercise: ExerciseStyle,
) -> InstrumentSpec {
    linear(
        ContractKind::Option,
        "BTC",
        "USDC",
        multiplier,
        lot,
        Some(EXPIRY_MS),
        Some(
            OptionTerms::new(
                dec!(100),
                style,
                exercise,
                SettlementType::Cash,
                code("USDC"),
            )
            .unwrap(),
        ),
    )
}

fn deribit_option() -> InstrumentSpec {
    deribit::btc_option(
        id("BTC-27NOV26-100000-C"),
        EXPIRY_MS,
        dec!(100000),
        OptionStyle::Call,
    )
    .unwrap()
}

fn is_unsupported<T: core::fmt::Debug>(result: Result<T, ArbitrageError>, instrument: &str) {
    match result {
        Err(ArbitrageError::UnsupportedSpec {
            instrument: found, ..
        }) => assert_eq!(found, instrument),
        other => panic!("expected UnsupportedSpec for {instrument}, got {other:?}"),
    }
}

// ------------------------------------------------- identity and expiry

#[test]
fn instrument_from_undated_reference_specs_has_no_expiry() {
    for (spec, expected) in [
        (binance_usdm::btcusdt_perpetual().unwrap(), "BTCUSDT"),
        (deribit::btc_perpetual().unwrap(), "BTC-PERPETUAL"),
    ] {
        let instrument = Instrument::try_from(&spec).unwrap();
        assert_eq!(instrument.id.as_str(), expected);
        assert_eq!(instrument.expiry, None);
    }
}

#[test]
fn instrument_from_dated_specs_keeps_the_expiry() {
    let future = deribit::btc_future(id("BTC-27NOV26"), EXPIRY_MS).unwrap();
    for (spec, expected) in [
        (future, "BTC-27NOV26"),
        (deribit_option(), "BTC-27NOV26-100000-C"),
    ] {
        let instrument = Instrument::try_from(&spec).unwrap();
        assert_eq!(instrument.id.as_str(), expected);
        assert_eq!(
            instrument.expiry,
            Some(Timestamp::from_millis(1_795_766_400_000))
        );
    }
}

/// `i64::MAX` is 9223372036854775807.
#[test]
fn instrument_expiry_above_i64_max_is_refused() {
    let at_limit = linear_future(dec!(1), dec!(1), 9_223_372_036_854_775_807);
    assert_eq!(
        Instrument::try_from(&at_limit).unwrap().expiry,
        Some(Timestamp::from_millis(i64::MAX))
    );
    for expiry_ms in [9_223_372_036_854_775_808, u64::MAX] {
        let spec = linear_future(dec!(1), dec!(1), expiry_ms);
        assert_eq!(
            Instrument::try_from(&spec),
            Err(ArbitrageError::ExpiryOutOfRange {
                instrument: "TEST".to_owned(),
                expiry_ms,
            })
        );
    }
}

#[test]
fn converted_future_expires_at_its_expiry() {
    let spec = linear_future(dec!(1), dec!(1), 1_000);
    let instrument = Instrument::try_from(&spec).unwrap();
    assert!(instrument.ensure_live(Timestamp::from_millis(999)).is_ok());
    assert!(matches!(
        instrument.ensure_live(Timestamp::from_millis(1_000)),
        Err(ArbitrageError::Expired { .. })
    ));
}

// ------------------------------------------------------------ currencies

#[test]
fn currencies_of_reference_specs_map_exactly() {
    let usdt = Currency::new("USDT").unwrap();
    let (btc, usd) = (Currency::new("BTC").unwrap(), Currency::new("USD").unwrap());

    let linear = currencies(&binance_usdm::btcusdt_perpetual().unwrap()).unwrap();
    assert_eq!(
        (
            linear.underlying,
            linear.price,
            linear.settlement,
            linear.premium
        ),
        (btc, usdt, usdt, None)
    );

    let inverse = currencies(&deribit::btc_perpetual().unwrap()).unwrap();
    assert_eq!(
        (
            inverse.underlying,
            inverse.price,
            inverse.settlement,
            inverse.premium
        ),
        (btc, usd, btc, None)
    );

    // Priced in USD, with its premium in BTC.
    let option = currencies(&deribit_option()).unwrap();
    assert_eq!(
        (
            option.underlying,
            option.price,
            option.settlement,
            option.premium
        ),
        (btc, usd, btc, Some(btc))
    );
}

/// An asset code has up to 16 characters, a currency 12.
#[test]
fn currency_of_13_characters_is_refused() {
    let twelve = linear(
        ContractKind::Perpetual,
        "ABCDEFGHIJKL",
        "USDT",
        dec!(1),
        dec!(1),
        None,
        None,
    );
    assert_eq!(
        currencies(&twelve).unwrap().underlying.as_str(),
        "ABCDEFGHIJKL"
    );
    let thirteen = linear(
        ContractKind::Perpetual,
        "ABCDEFGHIJKLM",
        "USDT",
        dec!(1),
        dec!(1),
        None,
        None,
    );
    assert_eq!(
        currencies(&thirteen),
        Err(ArbitrageError::InvalidCurrency {
            code: "ABCDEFGHIJKLM".to_owned(),
        })
    );
}

// ------------------------------------------------------ funding convention

#[test]
fn perp_convention_of_reference_specs_maps_exactly() {
    assert_eq!(
        perp_convention(&binance_usdm::btcusdt_perpetual().unwrap()).unwrap(),
        PerpConvention::Linear
    );
    let PerpConvention::Inverse {
        contract_value,
        base,
    } = perp_convention(&deribit::btc_perpetual().unwrap()).unwrap()
    else {
        panic!("expected an inverse convention");
    };
    assert_eq!(contract_value.value(), dec!(10));
    assert_eq!(base.as_str(), "BTC");
}

#[test]
fn perp_convention_of_other_kinds_is_refused() {
    is_unsupported(
        perp_convention(&deribit::btc_future(id("BTC-27NOV26"), EXPIRY_MS).unwrap()),
        "BTC-27NOV26",
    );
    is_unsupported(perp_convention(&deribit_option()), "BTC-27NOV26-100000-C");
    is_unsupported(
        perp_convention(&linear_future(dec!(1), dec!(1), EXPIRY_MS)),
        "TEST",
    );
}

/// Deribit `BTC-PERPETUAL`, CV 10 USD. A long of 100 contracts at a mark of
/// 60000 and a rate of 0.0001 pays 100 x 10 / 60000 x 0.0001 =
/// 0.00000166... BTC, 0.00000167 settled once to the satoshi. The position
/// is in contracts.
#[test]
fn inverse_funding_through_the_converted_convention_matches_the_hand_value() {
    let spec = deribit::btc_perpetual().unwrap();
    let event = FundingEvent {
        time: Timestamp::from_millis(28_800_000),
        rate: FundingRate::new(dec!(0.0001), RateKind::Estimated).unwrap(),
        mark: Price::new(dec!(60000)).unwrap(),
    };
    let settlement = currencies(&spec).unwrap().settlement;
    let paid = funding_cashflow_rounded(
        perp_convention(&spec).unwrap(),
        dec!(100),
        &event,
        settlement,
        SettlementRounding::new(8, Rounding::HalfEven).unwrap(),
    )
    .unwrap();
    assert_eq!(paid.money().amount(), dec!(0.00000167));
    assert_eq!(paid.money().currency().as_str(), "BTC");
}

// ------------------------------------------- contract terms and base units

#[test]
fn contract_terms_of_a_linear_contract_map_exactly() {
    let terms = contract_terms(&binance_usdm::btcusdt_perpetual().unwrap()).unwrap();
    assert_eq!(terms.contract_size.value(), dec!(1));
    assert_eq!(terms.lot.value(), dec!(0.001));

    let terms = contract_terms(&linear_future(dec!(0.001), dec!(0.25), EXPIRY_MS)).unwrap();
    assert_eq!(terms.contract_size.value(), dec!(0.001));
    assert_eq!(terms.lot.value(), dec!(0.25));
}

/// Inverse future and perpetual, sized in value; inverse option, whose
/// premium is not in the price asset.
#[test]
fn contract_terms_and_base_quantity_refuse_inverse_contracts() {
    let future = deribit::btc_future(id("BTC-27NOV26"), EXPIRY_MS).unwrap();
    let perpetual = deribit::btc_perpetual().unwrap();
    let option = deribit_option();
    for (spec, instrument) in [
        (&future, "BTC-27NOV26"),
        (&perpetual, "BTC-PERPETUAL"),
        (&option, "BTC-27NOV26-100000-C"),
    ] {
        is_unsupported(contract_terms(spec), instrument);
        is_unsupported(base_quantity(spec, q(dec!(1))), instrument);
    }
}

#[test]
fn contract_terms_accept_european_and_refuse_american_options() {
    let european = linear_option(
        dec!(0.1),
        dec!(1),
        OptionStyle::Call,
        ExerciseStyle::European,
    );
    let terms = contract_terms(&european).unwrap();
    assert_eq!(terms.contract_size.value(), dec!(0.1));
    assert_eq!(terms.lot.value(), dec!(1));
    // 3 contracts of 0.1 are 0.3 of the underlying.
    assert_eq!(
        base_quantity(&european, q(dec!(3))).unwrap().value(),
        dec!(0.3)
    );

    let american = linear_option(
        dec!(0.1),
        dec!(1),
        OptionStyle::Call,
        ExerciseStyle::American,
    );
    is_unsupported(contract_terms(&american), "TEST");
    is_unsupported(base_quantity(&american, q(dec!(3))), "TEST");
    // It still converts as an identity: that certifies nothing else.
    assert!(Instrument::try_from(&american).is_ok());
}

/// 0.25 contracts x 0.001 BTC per contract = 0.00025 BTC.
#[test]
fn base_quantity_of_a_fractional_multiplier_is_exact() {
    let spec = linear_future(dec!(0.001), dec!(0.25), EXPIRY_MS);
    let lot = contract_terms(&spec).unwrap().lot;
    assert_eq!(base_quantity(&spec, lot).unwrap().value(), dec!(0.00025));
}

#[test]
fn base_quantity_fails_rather_than_rounds() {
    let doubled = linear_future(dec!(2), dec!(1), EXPIRY_MS);
    assert!(matches!(
        base_quantity(&doubled, q(Decimal::MAX)),
        Err(ArbitrageError::Overflow { .. })
    ));
    // 31 fractional digits do not fit a Decimal.
    let fine = linear_future(dec!(1.000000000000001), dec!(1), EXPIRY_MS);
    assert!(matches!(
        base_quantity(&fine, q(dec!(1.00000000000001))),
        Err(ArbitrageError::Inexact { .. })
    ));
}

#[test]
fn zero_and_negative_contracts_cannot_reach_base_quantity() {
    for contracts in [dec!(0), dec!(-1)] {
        assert!(matches!(
            Quantity::new(contracts),
            Err(ArbitrageError::InvalidValue {
                field: "quantity",
                ..
            })
        ));
    }
}

// ------------------------------------------------------------ end to end

fn book(bid: Decimal, ask: Decimal, size: Decimal) -> TopOfBook {
    TopOfBook {
        bid: Some(Level::new(Price::new(bid).unwrap(), q(size))),
        ask: Some(Level::new(Price::new(ask).unwrap(), q(size))),
    }
}

/// Fixture E of `tests/parity.rs`, with instruments and contract terms taken
/// from specifications: options of 0.1 and a future of 0.01, lot 1. The
/// reversal nets 0.173408 USDC, as computed by hand there.
#[test]
fn converted_linear_specs_drive_parity_to_the_hand_value() {
    let expiry = 1_000_000_u64;
    let named = |name: &str, spec: InstrumentSpec| {
        InstrumentSpec::new(InstrumentDraft {
            id: id(name),
            expiry_ms: Some(expiry),
            ..spec.draft().clone()
        })
        .unwrap()
    };
    let option_leg = |name, style, bid, ask| {
        let spec = named(
            name,
            linear_option(dec!(0.1), dec!(1), style, ExerciseStyle::European),
        );
        let terms = contract_terms(&spec).unwrap();
        ParityOption {
            instrument: Instrument::try_from(&spec).unwrap(),
            book: book(bid, ask, dec!(50)),
            contract_size: terms.contract_size,
            lot: terms.lot,
            fee: OptionFee::UnderlyingCapped {
                rate: FeeRate::from_bps(dec!(3)).unwrap(),
                cap: dec!(0.125),
            },
        }
    };
    let future = named(
        "XYZ-F",
        linear(
            ContractKind::Future,
            "BTC",
            "USDC",
            dec!(0.01),
            dec!(1),
            Some(expiry),
            None,
        ),
    );
    let future_terms = contract_terms(&future).unwrap();
    let input = ParityInput {
        as_of: Timestamp::from_millis(0),
        quote_currency: currencies(&future).unwrap().price,
        strike: Price::new(dec!(100)).unwrap(),
        discount: dec!(0.99),
        index: Price::new(dec!(101.4037)).unwrap(),
        call: option_leg("XYZ-100-C", OptionStyle::Call, dec!(6.00), dec!(6.10)),
        put: option_leg("XYZ-100-P", OptionStyle::Put, dec!(4.90), dec!(5.00)),
        future: ParityFuture {
            instrument: Instrument::try_from(&future).unwrap(),
            book: book(dec!(101.50), dec!(101.60), dec!(500)),
            contract_size: future_terms.contract_size,
            lot: future_terms.lot,
            fee: FeeRate::from_bps(dec!(5)).unwrap(),
        },
        package: q(dec!(1)),
        fee_dp: 6,
        fee_rounding: Rounding::TowardZero,
        edge_dp: 6,
        edge_rounding: Rounding::HalfEven,
    };
    let evaluation = evaluate_parity(&input).unwrap();
    let best = evaluation.executable().unwrap();
    assert_eq!(best.expected_gross.amount(), dec!(0.285));
    assert_eq!(best.expected_net.amount(), dec!(0.173408));
}
