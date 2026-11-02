//! Put-call parity edge against a same-expiry future (XM2-17a-1).
//!
//! Fixture E of the Xynthex X-M2 plan (section 3.6), with the ECON
//! defaults: options trade 3 bps of the underlying notional at the index,
//! capped at 12.5 % of the premium, with a 0.1 XYZ multiplier; the future
//! trades 5 bps of its notional with a 0.01 XYZ multiplier. One package is
//! 1 XYZ, so 10 option contracts and 100 future contracts. Fees are
//! truncated to 1e-6 USDC.

use arbitrage_rs::parity::{
    DirectionEvaluation, OptionFee, ParityDirection, ParityFuture, ParityInput, ParityOption,
    ParityUnavailable, evaluate_parity,
};
use arbitrage_rs::{
    ArbitrageError, Currency, Decimal, FeeRate, Instrument, InstrumentId, Level, Price, Quantity,
    Rounding, Side, Timestamp, TopOfBook,
};
use rust_decimal_macros::dec;

const EXPIRY: i64 = 1_000_000;

fn usdc() -> Currency {
    Currency::new("USDC").unwrap()
}

fn q(x: Decimal) -> Quantity {
    Quantity::new(x).unwrap()
}

fn book(bid: Decimal, ask: Decimal, size: Decimal) -> TopOfBook {
    TopOfBook {
        bid: Some(Level::new(Price::new(bid).unwrap(), q(size))),
        ask: Some(Level::new(Price::new(ask).unwrap(), q(size))),
    }
}

fn dated(id: &str) -> Instrument {
    Instrument::dated(
        InstrumentId::new(id).unwrap(),
        Timestamp::from_millis(EXPIRY),
    )
}

fn option(id: &str, bid: Decimal, ask: Decimal) -> ParityOption {
    ParityOption {
        instrument: dated(id),
        book: book(bid, ask, dec!(50)),
        contract_size: q(dec!(0.1)),
        lot: q(dec!(1)),
        fee: OptionFee::UnderlyingCapped {
            rate: FeeRate::from_bps(dec!(3)).unwrap(),
            cap: dec!(0.125),
        },
    }
}

fn input(strike: Decimal, call: (Decimal, Decimal), put: (Decimal, Decimal)) -> ParityInput {
    ParityInput {
        as_of: Timestamp::from_millis(0),
        quote_currency: usdc(),
        strike: Price::new(strike).unwrap(),
        discount: dec!(0.99),
        index: Price::new(dec!(101.4037)).unwrap(),
        call: option(&format!("XYZ-{strike}-C"), call.0, call.1),
        put: option(&format!("XYZ-{strike}-P"), put.0, put.1),
        future: ParityFuture {
            instrument: dated("XYZ-F"),
            book: book(dec!(101.50), dec!(101.60), dec!(500)),
            contract_size: q(dec!(0.01)),
            lot: q(dec!(1)),
            fee: FeeRate::from_bps(dec!(5)).unwrap(),
        },
        package: q(dec!(1)),
        fee_dp: 6,
        fee_rounding: Rounding::TowardZero,
    }
}

fn k100() -> ParityInput {
    input(
        dec!(100),
        (dec!(6.00), dec!(6.10)),
        (dec!(4.90), dec!(5.00)),
    )
}

fn k80() -> ParityInput {
    input(
        dec!(80),
        (dec!(21.40), dec!(21.60)),
        (dec!(0.05), dec!(0.10)),
    )
}

/// `(gross, [call, put, future] fees, net)` of a computed direction.
fn numbers(d: &DirectionEvaluation) -> (Decimal, [Decimal; 3], Decimal) {
    let e = d.edge().expect("edge computed");
    (
        e.expected_gross.amount(),
        [
            e.expected_fees.call.money().amount(),
            e.expected_fees.put.money().amount(),
            e.expected_fees.future.money().amount(),
        ],
        e.expected_net.amount(),
    )
}

#[test]
fn fixture_e_strike_100() {
    let ev = evaluate_parity(&k100()).unwrap();
    assert_eq!(
        numbers(&ev.reversal),
        (
            dec!(0.285),
            [dec!(0.030421), dec!(0.030421), dec!(0.05075)],
            dec!(0.173408)
        )
    );
    assert_eq!(
        numbers(&ev.conversion),
        (
            dec!(-0.584),
            [dec!(0.030421), dec!(0.030421), dec!(0.0508)],
            dec!(-0.695642)
        )
    );
    let best = ev.executable().unwrap();
    assert_eq!(best.direction, ParityDirection::Reversal);
    assert_eq!(best.expected_net.amount(), dec!(0.173408));
    assert_eq!(best.expected_fees.total.money().amount(), dec!(0.111592));
}

#[test]
fn fixture_e_strike_80_premium_cap_binds() {
    let ev = evaluate_parity(&k80()).unwrap();
    assert_eq!(
        numbers(&ev.reversal),
        (
            dec!(-0.265),
            [dec!(0.030421), dec!(0.00625), dec!(0.05075)],
            dec!(-0.352421)
        )
    );
    assert_eq!(
        numbers(&ev.conversion),
        (
            dec!(-0.084),
            [dec!(0.030421), dec!(0.0125), dec!(0.0508)],
            dec!(-0.177721)
        )
    );
    let best = ev.executable().unwrap();
    assert_eq!(best.direction, ParityDirection::Conversion);
    assert_eq!(best.expected_net.amount(), dec!(-0.177721));
}

#[test]
fn legs_are_sized_in_contracts_at_executable_prices() {
    let ev = evaluate_parity(&k100()).unwrap();
    let r = ev.reversal.edge().unwrap();
    assert_eq!(
        (r.legs.call.side, r.legs.put.side, r.legs.future.side),
        (Side::Buy, Side::Sell, Side::Sell)
    );
    assert_eq!(r.legs.call.quantity.value(), dec!(10));
    assert_eq!(r.legs.put.quantity.value(), dec!(10));
    assert_eq!(r.legs.future.quantity.value(), dec!(100));
    assert_eq!(r.legs.call.price.value(), dec!(6.10));
    assert_eq!(r.legs.put.price.value(), dec!(4.90));
    assert_eq!(r.legs.future.price.value(), dec!(101.50));

    let c = ev.conversion.edge().unwrap();
    assert_eq!(
        (c.legs.call.side, c.legs.put.side, c.legs.future.side),
        (Side::Sell, Side::Buy, Side::Buy)
    );
    assert_eq!(c.legs.call.price.value(), dec!(6.00));
    assert_eq!(c.legs.put.price.value(), dec!(5.00));
    assert_eq!(c.legs.future.price.value(), dec!(101.60));
}

#[test]
fn fees_are_rounded_once_with_the_callers_mode() {
    let mut i = k100();
    i.fee_rounding = Rounding::Up;
    let ev = evaluate_parity(&i).unwrap();
    // 3 * 101.4037 / 10_000 = 0.03042111 rounds up to 0.030422.
    assert_eq!(
        numbers(&ev.reversal).1,
        [dec!(0.030422), dec!(0.030422), dec!(0.05075)]
    );
}

#[test]
fn premium_basis() {
    let mut i = k100();
    let rate = FeeRate::from_bps(dec!(30)).unwrap();
    i.call.fee = OptionFee::Premium { rate };
    i.put.fee = OptionFee::Premium { rate };
    let ev = evaluate_parity(&i).unwrap();
    // Reversal: 30 bps of 6.10 and of 4.90.
    assert_eq!(
        numbers(&ev.reversal).1,
        [dec!(0.0183), dec!(0.0147), dec!(0.05075)]
    );
}

#[test]
fn package_scales_every_amount() {
    let mut i = k100();
    i.package = q(dec!(2));
    let ev = evaluate_parity(&i).unwrap();
    let r = ev.reversal.edge().unwrap();
    assert_eq!(r.expected_gross.amount(), dec!(0.57));
    assert_eq!(r.legs.call.quantity.value(), dec!(20));
    assert_eq!(r.legs.future.quantity.value(), dec!(200));
    // 3 * 101.4037 * 2 / 10_000 = 0.06084222 truncates to 0.060842.
    assert_eq!(r.expected_fees.call.money().amount(), dec!(0.060842));
}

#[test]
fn missing_side_makes_only_that_direction_unavailable() {
    let mut i = k100();
    i.put.book.bid = None;
    let ev = evaluate_parity(&i).unwrap();
    assert_eq!(
        ev.reversal,
        DirectionEvaluation::Unavailable(ParityUnavailable::MissingQuote {
            instrument: i.put.instrument.clone(),
            side: Side::Sell,
        })
    );
    assert_eq!(
        ev.executable().unwrap().direction,
        ParityDirection::Conversion
    );

    i.future.book.ask = None;
    let ev = evaluate_parity(&i).unwrap();
    assert!(ev.executable().is_none());
}

#[test]
fn top_level_must_hold_one_package() {
    let mut i = k100();
    i.future.book = book(dec!(101.50), dec!(101.60), dec!(99));
    let ev = evaluate_parity(&i).unwrap();
    assert_eq!(
        ev.reversal,
        DirectionEvaluation::Unavailable(ParityUnavailable::InsufficientLiquidity {
            instrument: i.future.instrument.clone(),
            available: q(dec!(99)),
            required: q(dec!(100)),
        })
    );
}

#[test]
fn package_must_be_whole_lots_on_every_leg() {
    let not_whole = Err(ArbitrageError::InvalidInput {
        reason: "the package is not a whole multiple of a leg's lot in contracts",
    });
    let mut i = k100();
    i.package = q(dec!(0.05));
    assert_eq!(evaluate_parity(&i), not_whole);
    let mut i = k100();
    i.future.lot = q(dec!(3));
    assert_eq!(evaluate_parity(&i), not_whole);
    let mut i = k100();
    i.call.contract_size = q(dec!(0.3));
    assert_eq!(evaluate_parity(&i), not_whole);
}

#[test]
fn input_validation() {
    let mut i = k100();
    i.put.instrument = i.call.instrument.clone();
    assert!(matches!(
        evaluate_parity(&i),
        Err(ArbitrageError::InvalidInput { .. })
    ));

    let mut i = k100();
    i.put.instrument = Instrument::dated(
        i.put.instrument.id.clone(),
        Timestamp::from_millis(EXPIRY + 1),
    );
    assert_eq!(
        evaluate_parity(&i),
        Err(ArbitrageError::InvalidInput {
            reason: "parity legs must share one expiry"
        })
    );

    let mut i = k100();
    i.future.instrument = Instrument::undated(i.future.instrument.id.clone());
    assert_eq!(
        evaluate_parity(&i),
        Err(ArbitrageError::InvalidInput {
            reason: "parity legs must be dated"
        })
    );

    let mut i = k100();
    i.discount = dec!(0);
    assert!(matches!(
        evaluate_parity(&i),
        Err(ArbitrageError::InvalidValue {
            field: "discount factor",
            ..
        })
    ));

    let mut i = k100();
    i.call.fee = OptionFee::Premium {
        rate: FeeRate::from_bps(dec!(-1)).unwrap(),
    };
    assert!(matches!(
        evaluate_parity(&i),
        Err(ArbitrageError::InvalidValue {
            field: "option fee rate (bps)",
            ..
        })
    ));

    let mut i = k100();
    i.put.fee = OptionFee::UnderlyingCapped {
        rate: FeeRate::from_bps(dec!(3)).unwrap(),
        cap: dec!(1.01),
    };
    assert!(matches!(
        evaluate_parity(&i),
        Err(ArbitrageError::InvalidValue {
            field: "option fee cap",
            ..
        })
    ));

    let mut i = k100();
    i.as_of = Timestamp::from_millis(EXPIRY);
    assert!(matches!(
        evaluate_parity(&i),
        Err(ArbitrageError::Expired { .. })
    ));
}

#[test]
fn tie_returns_the_reversal() {
    // Symmetric books around parity with zero fees: both nets are equal.
    let mut i = input(dec!(100), (dec!(1), dec!(3)), (dec!(1), dec!(3)));
    i.discount = dec!(1);
    i.future.book = book(dec!(99), dec!(101), dec!(500));
    i.future.fee = FeeRate::zero();
    for o in [&mut i.call, &mut i.put] {
        o.fee = OptionFee::Premium {
            rate: FeeRate::zero(),
        };
    }
    let ev = evaluate_parity(&i).unwrap();
    assert_eq!(
        ev.reversal.edge().unwrap().expected_net,
        ev.conversion.edge().unwrap().expected_net
    );
    assert_eq!(
        ev.executable().unwrap().direction,
        ParityDirection::Reversal
    );
}
