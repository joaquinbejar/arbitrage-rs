//! The shared `financial_types` instrument fixtures run through the `spec`
//! module and arbitrage-rs's own formulas (feature `instruments`, #50).
//!
//! Every case has an explicit disposition, keyed by its name and operation;
//! an unknown or missing case fails the test, so a new upstream case must be
//! classified here. The shared checker (`fixtures::check`) is a canary of
//! `financial_types` itself; the independent regression is the set of hand
//! literals in `tests/instrument_spec.rs`.

#![cfg(feature = "instruments")]
// Tests fail loudly: the library's no-panic lints do not apply here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::{BTreeMap, BTreeSet};

use arbitrage_rs::funding::{
    ContractValue, FundingEvent, FundingRate, PerpConvention, RateKind, SettlementRounding,
    funding_cashflow_rounded,
};
use arbitrage_rs::spec::{base_quantity, contract_terms, perp_convention};
use arbitrage_rs::{
    ArbitrageError, Currency, Decimal, Instrument, Money, Price, Quantity, Rounding, Timestamp,
};
use financial_types::Side;
use financial_types::instrument::fixtures::{self, Case, ErrorKind, Expected, Operation};
use financial_types::instrument::{ContractKind, Margining, MoneyRatio, Ratio, RoundingRule};
use rust_decimal_macros::dec;

/// What arbitrage-rs does with a case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Disposition {
    /// `Price::notional`, and a sale minus a purchase: equal to the exact
    /// amount.
    Linear,
    /// `funding_cashflow_rounded` at a rate of 1, the only public inverse
    /// quotient: equal to the settled amount.
    InverseNotional,
    /// `Quantity::floor_to_lot` leaves the size unchanged exactly when the
    /// fixture says it is on the grid. The lot comes from `spec.lot()`,
    /// since `contract_terms` refuses the inverse option.
    Lot,
    /// `funding_cashflow_rounded`, the route inverse amounts settle through
    /// (position = numerator, mark = denominator, contract value and rate 1)
    /// and, for the ratios a `Decimal` holds, `Money::round_dp`. There is no
    /// public residual, so the fixture's residual is not compared.
    RoundsOnce,
    /// Refused by arbitrage-rs too, with the variant asserted.
    Refused,
    /// No inverse `PnL` kernel: `evaluate_spot_perp` refuses inverse
    /// perpetuals, and the adapter refuses them in base units.
    InversePnl,
    /// `Price::notional` could compute a premium, but the adapter refuses
    /// the inverse option, whose premium is not in the price asset.
    PremiumRefused,
    /// No option expiry formula; the option still converts as a dated
    /// identity, and only a linear one has contract terms.
    OptionExpiry,
    /// The kernels have no tick: a price is only checked to be positive.
    Tick,
    /// `Rounding` has no away from zero modes.
    NoRoundingMode,
    /// A perpetual has no expiry: it converts undated and never expires.
    PerpetualExpiry,
}

use Disposition::*;

/// Every case, by `name | operation`, and its disposition.
const TABLE: &[(&str, Disposition)] = &[
    (
        "linear notional: 0.5 BTC x 60000.1 USDT = 30000.05 USDT | Notional { contracts: 0.5, price: 60000.1 }",
        Linear,
    ),
    (
        "linear pnl long: 0.5 x (61000.5 - 60000) = +500.25 USDT | RealizedPnl { side: Side::Long, contracts: 0.5, entry: 60000, exit: 61000.5 }",
        Linear,
    ),
    (
        "linear pnl short: the same move is -500.25 USDT | RealizedPnl { side: Side::Short, contracts: 0.5, entry: 60000, exit: 61000.5 }",
        Linear,
    ),
    (
        "inverse notional: 1000 x 10 USD / 60000 = 1/6 BTC, 0.16666667 at satoshi | Notional { contracts: 1000, price: 60000 }",
        InverseNotional,
    ),
    (
        "inverse pnl long: 10000 x (1/60000 - 1/61000) = 1/366 BTC, 0.00273224 | RealizedPnl { side: Side::Long, contracts: 1000, entry: 60000, exit: 61000 }",
        InversePnl,
    ),
    (
        "inverse pnl short: -1/366 BTC, -0.00273224 | RealizedPnl { side: Side::Short, contracts: 1000, entry: 60000, exit: 61000 }",
        InversePnl,
    ),
    (
        "inverse future pnl long: 1000 USD from 60000 to 59000 = -1/3540 BTC, -0.00028249 | RealizedPnl { side: Side::Long, contracts: 100, entry: 60000, exit: 59000 }",
        InversePnl,
    ),
    (
        "option premium long: 1.5 x 1 BTC x 0.0125 = -0.01875 BTC paid | Premium { side: Side::Long, contracts: 1.5, premium: 0.0125 }",
        PremiumRefused,
    ),
    (
        "option premium short: +0.01875 BTC received | Premium { side: Side::Short, contracts: 1.5, premium: 0.0125 }",
        PremiumRefused,
    ),
    (
        "inverse call long, S 110000, K 100000: 2 x 10000 / 110000 = 2/11 BTC | Expiry { side: Side::Long, contracts: 2, settlement_price: 110000 }",
        OptionExpiry,
    ),
    (
        "inverse call short: -2/11 BTC | Expiry { side: Side::Short, contracts: 2, settlement_price: 110000 }",
        OptionExpiry,
    ),
    (
        "inverse call at the money: 0 | Expiry { side: Side::Long, contracts: 2, settlement_price: 100000 }",
        OptionExpiry,
    ),
    (
        "inverse call out of the money: 0 | Expiry { side: Side::Long, contracts: 2, settlement_price: 90000 }",
        OptionExpiry,
    ),
    (
        "inverse put long, S 90000: 2 x 10000 / 90000 = 2/9 BTC | Expiry { side: Side::Long, contracts: 2, settlement_price: 90000 }",
        OptionExpiry,
    ),
    (
        "inverse put out of the money: 0 | Expiry { side: Side::Short, contracts: 2, settlement_price: 110000 }",
        OptionExpiry,
    ),
    (
        "illustrative linear cash call long, S 120, K 100: 3 x 20 = +60 USDT | Expiry { side: Side::Long, contracts: 3, settlement_price: 120 }",
        OptionExpiry,
    ),
    (
        "illustrative physical call long, S 120: +3 BTC, -300 USDT | Expiry { side: Side::Long, contracts: 3, settlement_price: 120 }",
        OptionExpiry,
    ),
    (
        "illustrative physical call short, S 120: -3 BTC, +300 USDT | Expiry { side: Side::Short, contracts: 3, settlement_price: 120 }",
        OptionExpiry,
    ),
    (
        "illustrative physical put long, S 80: -3 BTC, +300 USDT | Expiry { side: Side::Long, contracts: 3, settlement_price: 80 }",
        OptionExpiry,
    ),
    (
        "illustrative physical put short, S 80: +3 BTC, -300 USDT | Expiry { side: Side::Short, contracts: 3, settlement_price: 80 }",
        OptionExpiry,
    ),
    (
        "illustrative physical call out of the money: abandoned | Expiry { side: Side::Long, contracts: 3, settlement_price: 90 }",
        OptionExpiry,
    ),
    (
        "illustrative physical call at the money: abandoned | Expiry { side: Side::Short, contracts: 3, settlement_price: 100 }",
        OptionExpiry,
    ),
    (
        "illustrative physical put out of the money: abandoned | Expiry { side: Side::Long, contracts: 3, settlement_price: 120 }",
        OptionExpiry,
    ),
    (
        "Deribit option premium 0.005: on the 0.0001 band (inclusive) | CheckPrice { price: 0.005 }",
        Tick,
    ),
    (
        "Deribit option premium 0.0049: on 0.0001 | CheckPrice { price: 0.0049 }",
        Tick,
    ),
    (
        "Deribit option premium 0.00495: off 0.0001 | CheckPrice { price: 0.00495 }",
        Tick,
    ),
    (
        "Deribit option premium 0.0051: above 0.005, off 0.0005 | CheckPrice { price: 0.0051 }",
        Tick,
    ),
    (
        "Deribit option premium 0.0055: on 0.0005 | CheckPrice { price: 0.0055 }",
        Tick,
    ),
    (
        "Deribit future price 60002.5: on 2.5 | CheckPrice { price: 60002.5 }",
        Tick,
    ),
    (
        "Deribit future price 60001: off 2.5 | CheckPrice { price: 60001 }",
        Tick,
    ),
    (
        "Binance price 60000.15: off 0.1 | CheckPrice { price: 60000.15 }",
        Tick,
    ),
    (
        "Binance size 0.0015 BTC: off 0.001 | CheckLot { contracts: 0.0015 }",
        Lot,
    ),
    (
        "Deribit option size 0.15: off 0.1 | CheckLot { contracts: 0.15 }",
        Lot,
    ),
    (
        "Deribit option size 0.3: on 0.1 | CheckLot { contracts: 0.3 }",
        Lot,
    ),
    (
        "tie +1/8 at 2 places | Settle { value: Ratio { num: 1, den: 8 }, scale: 2, rule: Floor }",
        RoundsOnce,
    ),
    (
        "tie -1/8 at 2 places | Settle { value: Ratio { num: -1, den: 8 }, scale: 2, rule: Floor }",
        RoundsOnce,
    ),
    (
        "tie +1/8 at 2 places | Settle { value: Ratio { num: 1, den: 8 }, scale: 2, rule: Ceil }",
        RoundsOnce,
    ),
    (
        "tie -1/8 at 2 places | Settle { value: Ratio { num: -1, den: 8 }, scale: 2, rule: Ceil }",
        RoundsOnce,
    ),
    (
        "tie +1/8 at 2 places | Settle { value: Ratio { num: 1, den: 8 }, scale: 2, rule: TowardZero }",
        RoundsOnce,
    ),
    (
        "tie -1/8 at 2 places | Settle { value: Ratio { num: -1, den: 8 }, scale: 2, rule: TowardZero }",
        RoundsOnce,
    ),
    (
        "tie +1/8 at 2 places | Settle { value: Ratio { num: 1, den: 8 }, scale: 2, rule: AwayFromZero }",
        NoRoundingMode,
    ),
    (
        "tie -1/8 at 2 places | Settle { value: Ratio { num: -1, den: 8 }, scale: 2, rule: AwayFromZero }",
        NoRoundingMode,
    ),
    (
        "tie +1/8 at 2 places | Settle { value: Ratio { num: 1, den: 8 }, scale: 2, rule: HalfEven }",
        RoundsOnce,
    ),
    (
        "tie -1/8 at 2 places | Settle { value: Ratio { num: -1, den: 8 }, scale: 2, rule: HalfEven }",
        RoundsOnce,
    ),
    (
        "tie +1/8 at 2 places | Settle { value: Ratio { num: 1, den: 8 }, scale: 2, rule: HalfAwayFromZero }",
        NoRoundingMode,
    ),
    (
        "tie -1/8 at 2 places | Settle { value: Ratio { num: -1, den: 8 }, scale: 2, rule: HalfAwayFromZero }",
        NoRoundingMode,
    ),
    (
        "tie 3/8 half even: the odd floor goes up, 0.38 | Settle { value: Ratio { num: 3, den: 8 }, scale: 2, rule: HalfEven }",
        RoundsOnce,
    ),
    (
        "2/3 at scale 28 | Settle { value: Ratio { num: 2, den: 3 }, scale: 28, rule: HalfEven }",
        RoundsOnce,
    ),
    (
        "scale 29 is refused | Settle { value: Ratio { num: 1, den: 3 }, scale: 29, rule: HalfEven }",
        Refused,
    ),
    (
        "Decimal::MAX contracts x price 2: overflow | Notional { contracts: 79228162514264337593543950335, price: 2 }",
        Refused,
    ),
    (
        "1.000000000000001 x 1.00000000000001: 31 digits, inexact | Notional { contracts: 1.000000000000001, price: 1.00000000000001 }",
        Refused,
    ),
    (
        "1e-28 x 1e-28: below scale 28, inexact | Notional { contracts: 0.0000000000000000000000000001, price: 0.0000000000000000000000000001 }",
        Refused,
    ),
    (
        "negative premium is refused | Premium { side: Side::Long, contracts: 1, premium: -0.0001 }",
        Refused,
    ),
    (
        "zero contracts are refused | Notional { contracts: 0, price: 60000 }",
        Refused,
    ),
    (
        "expiry of a perpetual is unsupported | Expiry { side: Side::Long, contracts: 1, settlement_price: 60000 }",
        PerpetualExpiry,
    ),
];

fn key(case: &Case) -> String {
    format!("{} | {:?}", case.name, case.operation)
}

fn price(value: Decimal) -> Price {
    Price::new(value).unwrap()
}

fn quantity(value: Decimal) -> Quantity {
    Quantity::new(value).unwrap()
}

fn currency(code: &str) -> Currency {
    Currency::new(code).unwrap()
}

fn exact(expected: &Expected) -> &MoneyRatio {
    match expected {
        Expected::Amount { exact, .. } => exact,
        other => panic!("expected an amount, got {other:?}"),
    }
}

fn is_unsupported<T: core::fmt::Debug>(result: Result<T, ArbitrageError>, name: &str) {
    match result {
        Err(ArbitrageError::UnsupportedSpec { .. }) => {}
        other => panic!("{name}: expected UnsupportedSpec, got {other:?}"),
    }
}

/// `Money` equals an exact amount: same currency, same value.
fn assert_money(money: Money, expected: &MoneyRatio, name: &str) {
    assert_eq!(money.currency().as_str(), expected.asset.as_str(), "{name}");
    assert!(
        Ratio::from_decimal(money.amount())
            .equals(&expected.ratio)
            .unwrap(),
        "{name}: {} against {:?}",
        money.amount(),
        expected.ratio
    );
}

fn run_linear(case: &Case) {
    let (name, spec) = (case.name, &case.spec);
    let quote = currency(spec.price_asset().as_str());
    let expected = exact(&case.expected);
    match &case.operation {
        Operation::Notional {
            contracts,
            price: p,
        } => {
            let size = base_quantity(spec, quantity(*contracts)).unwrap();
            assert_money(price(*p).notional(size, quote).unwrap(), expected, name);
        }
        Operation::RealizedPnl {
            side,
            contracts,
            entry,
            exit,
        } => {
            let size = base_quantity(spec, quantity(*contracts)).unwrap();
            let bought = price(*entry).notional(size, quote).unwrap();
            let sold = price(*exit).notional(size, quote).unwrap();
            let pnl = match side {
                Side::Long => sold.checked_sub(bought),
                Side::Short => bought.checked_sub(sold),
            };
            assert_money(pnl.unwrap(), expected, name);
        }
        other => panic!("{name}: unexpected operation {other:?}"),
    }
}

/// `position * contract_value / mark * rate`, settled once.
fn quotient(
    convention: PerpConvention,
    position: Decimal,
    mark: Decimal,
    settlement: Currency,
    dp: u32,
    mode: Rounding,
) -> Decimal {
    let event = FundingEvent {
        time: Timestamp::from_millis(1),
        rate: FundingRate::new(Decimal::ONE, RateKind::Estimated).unwrap(),
        mark: price(mark),
    };
    funding_cashflow_rounded(
        convention,
        position,
        &event,
        settlement,
        SettlementRounding::new(dp, mode).unwrap(),
    )
    .unwrap()
    .money()
    .amount()
}

fn run_inverse_notional(case: &Case) {
    let (name, spec) = (case.name, &case.spec);
    let Operation::Notional {
        contracts,
        price: mark,
    } = &case.operation
    else {
        panic!("{name}: expected a notional");
    };
    let Expected::Amount {
        settled: Some(settled),
        ..
    } = &case.expected
    else {
        panic!("{name}: expected a settled amount");
    };
    assert_eq!(
        (settled.scale, settled.rule),
        (8, RoundingRule::HalfEven),
        "{name}"
    );
    let amount = quotient(
        perp_convention(spec).unwrap(),
        *contracts,
        *mark,
        currency(spec.settlement_asset().as_str()),
        settled.scale,
        Rounding::HalfEven,
    );
    assert_eq!(amount, settled.amount, "{name}");
}

fn run_lot(case: &Case) {
    let name = case.name;
    let Operation::CheckLot { contracts } = &case.operation else {
        panic!("{name}: expected a lot check");
    };
    let size = quantity(*contracts);
    let floored = size.floor_to_lot(quantity(case.spec.lot())).unwrap();
    let on_grid = floored == Some(size);
    match &case.expected {
        Expected::Valid => assert!(on_grid, "{name}"),
        Expected::Error(ErrorKind::NotOnGrid) => assert!(!on_grid, "{name}"),
        other => panic!("{name}: unexpected expectation {other:?}"),
    }
}

fn mode(rule: RoundingRule) -> Rounding {
    match rule {
        RoundingRule::Floor => Rounding::Down,
        RoundingRule::Ceil => Rounding::Up,
        RoundingRule::TowardZero => Rounding::TowardZero,
        RoundingRule::HalfEven => Rounding::HalfEven,
        other => panic!("no rounding mode for {other:?}"),
    }
}

fn run_rounding(case: &Case) {
    let name = case.name;
    let Operation::Settle { value, scale, rule } = &case.operation else {
        panic!("{name}: expected a settlement");
    };
    let Expected::Settled(settled) = &case.expected else {
        panic!("{name}: expected a settled amount");
    };
    let btc = currency("BTC");
    let convention = PerpConvention::Inverse {
        contract_value: ContractValue::new(Decimal::ONE).unwrap(),
        base: btc,
    };
    let amount = quotient(
        convention,
        value.num(),
        value.den(),
        btc,
        *scale,
        mode(*rule),
    );
    assert_eq!(amount, settled.amount, "{name}: quotient route");
    // The eighths terminate, so `Money` holds them exactly. 2/3 does not.
    if value.den() == dec!(8) {
        let held = Money::new(value.num() * dec!(0.125), btc);
        assert_eq!(
            held.round_dp(*scale, mode(*rule)).unwrap().amount(),
            settled.amount,
            "{name}: round_dp route"
        );
    }
}

fn run_refused(case: &Case) {
    let (name, spec) = (case.name, &case.spec);
    match &case.operation {
        Operation::Notional {
            contracts,
            price: p,
        } if name.starts_with("Decimal::MAX") => {
            let result = price(*p).notional(quantity(*contracts), currency("USDT"));
            assert!(
                matches!(result, Err(ArbitrageError::Overflow { .. })),
                "{name}: {result:?}"
            );
        }
        Operation::Notional {
            contracts,
            price: p,
        } if name.starts_with("1.000") || name.starts_with("1e-28") => {
            let result = price(*p).notional(quantity(*contracts), currency("USDT"));
            assert!(
                matches!(result, Err(ArbitrageError::Inexact { .. })),
                "{name}: {result:?}"
            );
        }
        Operation::Notional { contracts, .. } => {
            assert!(
                matches!(
                    Quantity::new(*contracts),
                    Err(ArbitrageError::InvalidValue {
                        field: "quantity",
                        ..
                    })
                ),
                "{name}"
            );
        }
        Operation::Premium { premium, .. } => {
            assert!(
                matches!(
                    Price::new(*premium),
                    Err(ArbitrageError::InvalidValue { field: "price", .. })
                ),
                "{name}"
            );
            is_unsupported(contract_terms(spec), name);
        }
        Operation::Settle { scale, rule, .. } => {
            assert!(
                matches!(
                    SettlementRounding::new(*scale, mode(*rule)),
                    Err(ArbitrageError::InvalidInput { .. })
                ),
                "{name}"
            );
            assert!(
                matches!(
                    Money::new(dec!(0.5), currency("BTC")).round_dp(*scale, mode(*rule)),
                    Err(ArbitrageError::Inexact { .. })
                ),
                "{name}"
            );
        }
        other => panic!("{name}: no refusal defined for {other:?}"),
    }
}

fn run_inverse_pnl(case: &Case) {
    let (name, spec) = (case.name, &case.spec);
    assert_eq!(spec.margining(), Margining::Inverse, "{name}");
    is_unsupported(base_quantity(spec, quantity(Decimal::ONE)), name);
    match spec.kind() {
        ContractKind::Perpetual => assert!(
            matches!(perp_convention(spec), Ok(PerpConvention::Inverse { .. })),
            "{name}"
        ),
        _ => is_unsupported(perp_convention(spec), name),
    }
}

fn run_premium_refused(case: &Case) {
    let (name, spec) = (case.name, &case.spec);
    is_unsupported(contract_terms(spec), name);
    is_unsupported(base_quantity(spec, quantity(Decimal::ONE)), name);
    assert!(
        Instrument::try_from(spec).unwrap().expiry.is_some(),
        "{name}"
    );
}

fn run_option_expiry(case: &Case) {
    let (name, spec) = (case.name, &case.spec);
    assert_eq!(spec.kind(), ContractKind::Option, "{name}");
    assert!(
        Instrument::try_from(spec).unwrap().expiry.is_some(),
        "{name}"
    );
    match spec.margining() {
        Margining::Linear => assert!(contract_terms(spec).is_ok(), "{name}"),
        _ => is_unsupported(contract_terms(spec), name),
    }
}

fn run_tick(case: &Case) {
    let name = case.name;
    let Operation::CheckPrice { price: p } = &case.operation else {
        panic!("{name}: expected a price check");
    };
    assert!(Price::new(*p).is_ok(), "{name}");
}

fn run_no_rounding_mode(case: &Case) {
    let name = case.name;
    assert!(
        matches!(
            case.operation,
            Operation::Settle {
                rule: RoundingRule::AwayFromZero | RoundingRule::HalfAwayFromZero,
                ..
            }
        ),
        "{name}"
    );
}

fn run_perpetual_expiry(case: &Case) {
    let name = case.name;
    assert_eq!(case.spec.kind(), ContractKind::Perpetual, "{name}");
    let instrument = Instrument::try_from(&case.spec).unwrap();
    assert_eq!(instrument.expiry, None, "{name}");
    assert!(
        instrument
            .ensure_live(Timestamp::from_millis(i64::MAX))
            .is_ok(),
        "{name}"
    );
}

#[test]
fn shared_fixtures_every_case_has_a_checked_disposition() {
    let table: BTreeMap<&str, Disposition> = TABLE.iter().copied().collect();
    assert_eq!(table.len(), TABLE.len(), "duplicate key in the table");
    let cases = fixtures::cases().unwrap();
    let mut seen = BTreeSet::new();
    let mut counts: BTreeMap<Disposition, usize> = BTreeMap::new();
    for case in &cases {
        // Canary: the case still passes against financial_types.
        fixtures::check(case).unwrap();
        let key = key(case);
        let disposition = *table
            .get(key.as_str())
            .unwrap_or_else(|| panic!("unclassified fixture: {key}"));
        assert!(seen.insert(key.clone()), "duplicate case: {key}");
        *counts.entry(disposition).or_default() += 1;
        match disposition {
            Linear => run_linear(case),
            InverseNotional => run_inverse_notional(case),
            Lot => run_lot(case),
            RoundsOnce => run_rounding(case),
            Refused => run_refused(case),
            InversePnl => run_inverse_pnl(case),
            PremiumRefused => run_premium_refused(case),
            OptionExpiry => run_option_expiry(case),
            Tick => run_tick(case),
            NoRoundingMode => run_no_rounding_mode(case),
            PerpetualExpiry => run_perpetual_expiry(case),
        }
    }
    for (key, _) in TABLE {
        assert!(seen.contains(*key), "fixture no longer present: {key}");
    }
    let expected: BTreeMap<Disposition, usize> = [
        (Linear, 3),
        (InverseNotional, 1),
        (Lot, 3),
        (RoundsOnce, 10),
        (Refused, 6),
        (InversePnl, 3),
        (PremiumRefused, 2),
        (OptionExpiry, 14),
        (Tick, 8),
        (NoRoundingMode, 4),
        (PerpetualExpiry, 1),
    ]
    .into_iter()
    .collect();
    assert_eq!(counts, expected);
}
