//! Linear relations: cash and carry, perpetual basis versus funding and
//! calendar spreads.
//!
//! Shared fixture: spot trades 1 XYZ per contract in lots of 0.01; futures
//! and the perpetual trade 0.01 XYZ per contract in lots of 1. Every leg
//! pays 5 bps of its notional. One package is 1 XYZ, so 1 spot contract and
//! 100 future or perpetual contracts. Fees are truncated to 1e-6 USDC and
//! the gross edge is rounded half even to 1e-6.

// Tests fail loudly: the library's no-panic lints do not apply here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use arbitrage_rs::linear::{
    CalendarInput, CashAndCarryInput, LinearDirection, LinearDirectionEvaluation, LinearLeg,
    LinearTerms, LinearUnavailable, PerpBasisInput, PerpFunding, evaluate_calendar,
    evaluate_cash_and_carry, evaluate_perp_basis,
};
use arbitrage_rs::{
    Action, ArbitrageError, Currency, Decimal, FeeRate, Instrument, InstrumentId, Level, Price,
    Quantity, Rounding, Timestamp, TopOfBook,
};
use rust_decimal_macros::dec;

const NEAR_EXPIRY: i64 = 1_000_000;
const FAR_EXPIRY: i64 = 2_000_000;

fn q(x: Decimal) -> Quantity {
    Quantity::new(x).unwrap()
}

fn px(x: Decimal) -> Price {
    Price::new(x).unwrap()
}

fn book(bid: Decimal, ask: Decimal, size: Decimal) -> TopOfBook {
    TopOfBook {
        bid: Some(Level::new(px(bid), q(size))),
        ask: Some(Level::new(px(ask), q(size))),
    }
}

fn id(s: &str) -> InstrumentId {
    InstrumentId::new(s).unwrap()
}

fn terms() -> LinearTerms {
    LinearTerms {
        as_of: Timestamp::from_millis(0),
        quote_currency: Currency::new("USDC").unwrap(),
        package: q(dec!(1)),
        fee_dp: 6,
        fee_rounding: Rounding::TowardZero,
        edge_dp: 6,
        edge_rounding: Rounding::HalfEven,
    }
}

fn spot(bid: Decimal, ask: Decimal) -> LinearLeg {
    LinearLeg {
        instrument: Instrument::undated(id("XYZ-SPOT")),
        book: book(bid, ask, dec!(10)),
        contract_size: q(dec!(1)),
        lot: q(dec!(0.01)),
        fee: FeeRate::from_bps(dec!(5)).unwrap(),
    }
}

fn derivative(instrument: Instrument, bid: Decimal, ask: Decimal) -> LinearLeg {
    LinearLeg {
        instrument,
        book: book(bid, ask, dec!(500)),
        contract_size: q(dec!(0.01)),
        lot: q(dec!(1)),
        fee: FeeRate::from_bps(dec!(5)).unwrap(),
    }
}

fn future(name: &str, expiry: i64, bid: Decimal, ask: Decimal) -> LinearLeg {
    derivative(
        Instrument::dated(id(name), Timestamp::from_millis(expiry)),
        bid,
        ask,
    )
}

fn perp(bid: Decimal, ask: Decimal) -> LinearLeg {
    derivative(Instrument::undated(id("XYZ-PERP")), bid, ask)
}

/// Spot 100.00 / 100.10, future at `fut`, carry 5 % a year over a quarter.
fn cash_and_carry(fut: (Decimal, Decimal)) -> CashAndCarryInput {
    CashAndCarryInput {
        terms: terms(),
        spot: spot(dec!(100.00), dec!(100.10)),
        future: future("XYZ-F1", NEAR_EXPIRY, fut.0, fut.1),
        carry_rate: dec!(0.05),
        time_to_expiry: dec!(0.25),
    }
}

/// Spot 100.00 / 100.10, perpetual at `p`, funding `rate` for 3 intervals
/// at mark `mark`.
fn perp_basis(p: (Decimal, Decimal), rate: Decimal, mark: Decimal) -> PerpBasisInput {
    PerpBasisInput {
        terms: terms(),
        spot: spot(dec!(100.00), dec!(100.10)),
        perp: perp(p.0, p.1),
        funding: PerpFunding {
            rate,
            mark: px(mark),
            intervals: 3,
        },
    }
}

/// Near future 101.50 / 101.60, far future 103.00 / 103.10, carry 5 % a
/// year over a quarter.
fn calendar() -> CalendarInput {
    CalendarInput {
        terms: terms(),
        near: future("XYZ-F1", NEAR_EXPIRY, dec!(101.50), dec!(101.60)),
        far: future("XYZ-F2", FAR_EXPIRY, dec!(103.00), dec!(103.10)),
        carry_rate: dec!(0.05),
        time_between: dec!(0.25),
        funding: None,
    }
}

/// Perpetual 100.30 / 100.40 near, future 101.50 / 101.60 far, no carry,
/// funding 1 bp for 270 intervals at mark 100.35.
fn perp_calendar() -> CalendarInput {
    CalendarInput {
        terms: terms(),
        near: perp(dec!(100.30), dec!(100.40)),
        far: future("XYZ-F1", NEAR_EXPIRY, dec!(101.50), dec!(101.60)),
        carry_rate: Decimal::ZERO,
        time_between: dec!(0.25),
        funding: Some(PerpFunding {
            rate: dec!(0.0001),
            mark: px(dec!(100.35)),
            intervals: 270,
        }),
    }
}

/// `(gross, [near, far, total] fees, net)` of a computed direction.
fn numbers(d: &LinearDirectionEvaluation) -> (Decimal, [Decimal; 3], Decimal) {
    let e = d.edge().expect("edge computed");
    (
        e.expected_gross.amount(),
        [
            e.expected_fees.near.money().amount(),
            e.expected_fees.far.money().amount(),
            e.expected_fees.total.money().amount(),
        ],
        e.expected_net.amount(),
    )
}

#[test]
fn cash_and_carry_fixture() {
    // Future 101.50 / 101.60.
    let ev = evaluate_cash_and_carry(&cash_and_carry((dec!(101.50), dec!(101.60)))).unwrap();
    // Buy spot at 100.10, sell the future at 101.50:
    //   gross = 101.50 - 100.10 - 100.10 * 0.05 * 0.25
    //         = 1.40 - 1.25125 = 0.14875
    //   fees  = 100.10 * 5e-4 = 0.05005 (spot)
    //         + 101.50 * 5e-4 = 0.05075 (future) = 0.1008
    //   net   = 0.14875 - 0.1008 = 0.04795
    assert_eq!(
        numbers(&ev.long_near),
        (
            dec!(0.14875),
            [dec!(0.05005), dec!(0.05075), dec!(0.1008)],
            dec!(0.04795)
        )
    );
    // Sell spot at 100.00, buy the future at 101.60:
    //   gross = 100.00 + 100.00 * 0.05 * 0.25 - 101.60 = 101.25 - 101.60
    //         = -0.35
    //   fees  = 0.05 + 0.0508 = 0.1008; net = -0.4508
    assert_eq!(
        numbers(&ev.short_near),
        (
            dec!(-0.35),
            [dec!(0.05), dec!(0.0508), dec!(0.1008)],
            dec!(-0.4508)
        )
    );
    let best = ev.executable().unwrap();
    assert_eq!(best.direction, LinearDirection::LongNearShortFar);

    // Legs: the spot multiplier is 1 (1 contract), the future's 0.01 (100
    // contracts), at the executable prices.
    let legs = &best.legs;
    assert_eq!(
        (legs.near.side, legs.near.quantity, legs.near.price),
        (Action::Buy, q(dec!(1)), px(dec!(100.10)))
    );
    assert_eq!(
        (legs.far.side, legs.far.quantity, legs.far.price),
        (Action::Sell, q(dec!(100)), px(dec!(101.50)))
    );
    assert_eq!(legs.far.instrument.id.as_str(), "XYZ-F1");
}

#[test]
fn reverse_cash_and_carry_fixture() {
    // Future 100.50 / 100.60, below spot plus carry.
    let ev = evaluate_cash_and_carry(&cash_and_carry((dec!(100.50), dec!(100.60)))).unwrap();
    // Sell spot at 100.00, buy the future at 100.60:
    //   gross = 100.00 + 1.25 - 100.60 = 0.65
    //   fees  = 0.05 + 100.60 * 5e-4 = 0.05 + 0.0503 = 0.1003
    //   net   = 0.5497
    assert_eq!(
        numbers(&ev.short_near),
        (
            dec!(0.65),
            [dec!(0.05), dec!(0.0503), dec!(0.1003)],
            dec!(0.5497)
        )
    );
    // Buy spot: 100.50 - 100.10 - 1.25125 = -0.85125;
    // fees 0.05005 + 0.05025 = 0.1003; net -0.95155.
    assert_eq!(
        numbers(&ev.long_near),
        (
            dec!(-0.85125),
            [dec!(0.05005), dec!(0.05025), dec!(0.1003)],
            dec!(-0.95155)
        )
    );
    let best = ev.executable().unwrap();
    assert_eq!(best.direction, LinearDirection::ShortNearLongFar);
    assert_eq!(best.legs.near.side, Action::Sell);
    assert_eq!(best.legs.far.side, Action::Buy);
}

#[test]
fn cash_and_carry_no_edge() {
    // Future 101.30 / 101.40, inside the no-arbitrage band.
    let ev = evaluate_cash_and_carry(&cash_and_carry((dec!(101.30), dec!(101.40)))).unwrap();
    // Buy spot: 101.30 - 100.10 - 1.25125 = -0.05125;
    // fees 0.05005 + 0.05065 = 0.1007; net -0.15195.
    assert_eq!(
        ev.long_near.edge().unwrap().expected_net.amount(),
        dec!(-0.15195)
    );
    // Sell spot: 101.25 - 101.40 = -0.15; fees 0.05 + 0.0507; net -0.2507.
    assert_eq!(
        ev.short_near.edge().unwrap().expected_net.amount(),
        dec!(-0.2507)
    );
    // The best direction is still reported, with its negative edge.
    assert_eq!(
        ev.executable().unwrap().expected_net.amount(),
        dec!(-0.15195)
    );
}

#[test]
fn perp_basis_fixture_positive_funding() {
    // Perpetual 100.30 / 100.40, funding +1 bp for 3 intervals at 100.35.
    let ev = evaluate_perp_basis(&perp_basis(
        (dec!(100.30), dec!(100.40)),
        dec!(0.0001),
        dec!(100.35),
    ))
    .unwrap();
    // Buy spot at 100.10, sell the perpetual at 100.30; the short receives
    // funding 100.35 * 0.0001 * 3 = 0.030105:
    //   gross = 100.30 - 100.10 + 0.030105 = 0.230105
    //   fees  = 0.05005 + 100.30 * 5e-4 = 0.05005 + 0.05015 = 0.1002
    //   net   = 0.129905
    assert_eq!(
        numbers(&ev.long_near),
        (
            dec!(0.230105),
            [dec!(0.05005), dec!(0.05015), dec!(0.1002)],
            dec!(0.129905)
        )
    );
    // Sell spot at 100.00, buy the perpetual at 100.40; the long pays:
    //   gross = 100.00 - 100.40 - 0.030105 = -0.430105
    //   fees  = 0.05 + 0.0502 = 0.1002; net = -0.530305
    assert_eq!(
        numbers(&ev.short_near),
        (
            dec!(-0.430105),
            [dec!(0.05), dec!(0.0502), dec!(0.1002)],
            dec!(-0.530305)
        )
    );
    assert_eq!(
        ev.executable().unwrap().direction,
        LinearDirection::LongNearShortFar
    );
}

#[test]
fn perp_basis_negative_funding_favours_the_long_perpetual() {
    // Perpetual 99.90 / 100.00, funding -5 bp for 3 intervals at 99.95:
    // shorts pay longs 99.95 * 0.0005 * 3 = 0.149925.
    let ev = evaluate_perp_basis(&perp_basis(
        (dec!(99.90), dec!(100.00)),
        dec!(-0.0005),
        dec!(99.95),
    ))
    .unwrap();
    // Sell spot at 100.00, buy the perpetual at 100.00:
    //   gross = 0 + 0.149925; fees 0.05 + 0.05 = 0.1; net 0.049925.
    assert_eq!(
        numbers(&ev.short_near),
        (
            dec!(0.149925),
            [dec!(0.05), dec!(0.05), dec!(0.1)],
            dec!(0.049925)
        )
    );
    // Buy spot at 100.10, sell the perpetual at 99.90, paying funding:
    //   gross = -0.20 - 0.149925 = -0.349925; fees 0.05005 + 0.04995 = 0.1;
    //   net = -0.449925.
    assert_eq!(
        numbers(&ev.long_near),
        (
            dec!(-0.349925),
            [dec!(0.05005), dec!(0.04995), dec!(0.1)],
            dec!(-0.449925)
        )
    );
    assert_eq!(
        ev.executable().unwrap().direction,
        LinearDirection::ShortNearLongFar
    );
}

#[test]
fn perp_basis_no_edge_without_funding() {
    // Zero funding and a basis tighter than the fees. Perpetual 100.15 /
    // 100.25:
    //   buy spot: 100.15 - 100.10 = 0.05; fees 0.05005 + 100.15 * 5e-4
    //             = 0.05005 + 0.050075 = 0.100125; net -0.050125
    //   sell spot: 100.00 - 100.25 = -0.25; fees 0.05 + 0.050125; net -0.350125
    let ev = evaluate_perp_basis(&perp_basis(
        (dec!(100.15), dec!(100.25)),
        Decimal::ZERO,
        dec!(100.20),
    ))
    .unwrap();
    assert_eq!(
        numbers(&ev.long_near),
        (
            dec!(0.05),
            [dec!(0.05005), dec!(0.050075), dec!(0.100125)],
            dec!(-0.050125)
        )
    );
    assert_eq!(
        ev.short_near.edge().unwrap().expected_net.amount(),
        dec!(-0.350125)
    );
}

#[test]
fn calendar_fixture_dated_legs() {
    let ev = evaluate_calendar(&calendar()).unwrap();
    // Buy the near future at 101.60, sell the far at 103.00:
    //   gross = 103.00 - 101.60 - 101.60 * 0.05 * 0.25 = 1.40 - 1.27 = 0.13
    //   fees  = 101.60 * 5e-4 = 0.0508 + 103.00 * 5e-4 = 0.0515 = 0.1023
    //   net   = 0.0277
    assert_eq!(
        numbers(&ev.long_near),
        (
            dec!(0.13),
            [dec!(0.0508), dec!(0.0515), dec!(0.1023)],
            dec!(0.0277)
        )
    );
    // Sell the near at 101.50, buy the far at 103.10:
    //   gross = 101.50 + 101.50 * 0.0125 - 103.10 = 101.50 + 1.26875 - 103.10
    //         = -0.33125
    //   fees  = 0.05075 + 0.05155 = 0.1023; net = -0.43355
    assert_eq!(
        numbers(&ev.short_near),
        (
            dec!(-0.33125),
            [dec!(0.05075), dec!(0.05155), dec!(0.1023)],
            dec!(-0.43355)
        )
    );
    let best = ev.executable().unwrap();
    assert_eq!(best.direction, LinearDirection::LongNearShortFar);
    assert_eq!(
        (best.legs.near.quantity, best.legs.far.quantity),
        (q(dec!(100)), q(dec!(100)))
    );
}

#[test]
fn calendar_fixture_perpetual_near_leg() {
    let ev = evaluate_calendar(&perp_calendar()).unwrap();
    // Funding over the window: 100.35 * 0.0001 * 270 = 2.70945.
    // Sell the perpetual at 100.30 (receives funding), buy the future at
    // 101.60:
    //   gross = 100.30 - 101.60 + 2.70945 = 1.40945
    //   fees  = 0.05015 + 0.0508 = 0.10095; net = 1.3085
    assert_eq!(
        numbers(&ev.short_near),
        (
            dec!(1.40945),
            [dec!(0.05015), dec!(0.0508), dec!(0.10095)],
            dec!(1.3085)
        )
    );
    // Buy the perpetual at 100.40 (pays funding), sell the future at 101.50:
    //   gross = 101.50 - 100.40 - 2.70945 = -1.60945
    //   fees  = 0.0502 + 0.05075 = 0.10095; net = -1.7104
    assert_eq!(
        numbers(&ev.long_near),
        (
            dec!(-1.60945),
            [dec!(0.0502), dec!(0.05075), dec!(0.10095)],
            dec!(-1.7104)
        )
    );
    assert_eq!(
        ev.executable().unwrap().direction,
        LinearDirection::ShortNearLongFar
    );
}

#[test]
fn fees_are_rounded_once_with_the_callers_mode() {
    // The spot fee 0.05005 at 4 places: 0.0500 toward zero, 0.0501 up.
    let mut i = cash_and_carry((dec!(101.50), dec!(101.60)));
    i.terms.fee_dp = 4;
    let ev = evaluate_cash_and_carry(&i).unwrap();
    assert_eq!(numbers(&ev.long_near).1[0], dec!(0.05));
    i.terms.fee_rounding = Rounding::Up;
    let ev = evaluate_cash_and_carry(&i).unwrap();
    assert_eq!(numbers(&ev.long_near).1[0], dec!(0.0501));
}

#[test]
fn gross_is_rounded_once_with_the_callers_mode() {
    // 0.14875 at 4 places half even is 0.1488 (the 5 rounds to even 8);
    // 2 places toward zero gives 0.14.
    let mut i = cash_and_carry((dec!(101.50), dec!(101.60)));
    i.terms.edge_dp = 4;
    let ev = evaluate_cash_and_carry(&i).unwrap();
    assert_eq!(numbers(&ev.long_near).0, dec!(0.1488));
    i.terms.edge_dp = 2;
    i.terms.edge_rounding = Rounding::TowardZero;
    let ev = evaluate_cash_and_carry(&i).unwrap();
    assert_eq!(numbers(&ev.long_near).0, dec!(0.14));
}

#[test]
fn package_scales_every_amount() {
    // 2 XYZ: 2 spot contracts, 200 future contracts, every amount doubled.
    let mut i = cash_and_carry((dec!(101.50), dec!(101.60)));
    i.terms.package = q(dec!(2));
    let ev = evaluate_cash_and_carry(&i).unwrap();
    assert_eq!(
        numbers(&ev.long_near),
        (
            dec!(0.2975),
            [dec!(0.1001), dec!(0.1015), dec!(0.2016)],
            dec!(0.0959)
        )
    );
    let legs = &ev.long_near.edge().unwrap().legs;
    assert_eq!(
        (legs.near.quantity, legs.far.quantity),
        (q(dec!(2)), q(dec!(200)))
    );
}

#[test]
fn rebate_is_a_negative_fee() {
    // Future rate -1 bp: 101.50 * -1e-4 = -0.01015.
    let mut i = cash_and_carry((dec!(101.50), dec!(101.60)));
    i.future.fee = FeeRate::from_bps(dec!(-1)).unwrap();
    let ev = evaluate_cash_and_carry(&i).unwrap();
    // net = 0.14875 - (0.05005 - 0.01015) = 0.10885
    assert_eq!(
        numbers(&ev.long_near),
        (
            dec!(0.14875),
            [dec!(0.05005), dec!(-0.01015), dec!(0.0399)],
            dec!(0.10885)
        )
    );
}

#[test]
fn missing_side_makes_only_that_direction_unavailable() {
    let mut i = cash_and_carry((dec!(101.50), dec!(101.60)));
    i.future.book.bid = None;
    let ev = evaluate_cash_and_carry(&i).unwrap();
    assert_eq!(
        ev.long_near,
        LinearDirectionEvaluation::Unavailable(LinearUnavailable::MissingQuote {
            instrument: i.future.instrument.clone(),
            side: Action::Sell,
        })
    );
    assert!(ev.short_near.edge().is_some());
    assert_eq!(
        ev.executable().unwrap().direction,
        LinearDirection::ShortNearLongFar
    );
}

#[test]
fn top_level_must_hold_one_package() {
    // 99 future contracts at the bid, 100 needed.
    let mut i = cash_and_carry((dec!(101.50), dec!(101.60)));
    i.future.book.bid = Some(Level::new(px(dec!(101.50)), q(dec!(99))));
    let ev = evaluate_cash_and_carry(&i).unwrap();
    assert_eq!(
        ev.long_near,
        LinearDirectionEvaluation::Unavailable(LinearUnavailable::InsufficientLiquidity {
            instrument: i.future.instrument.clone(),
            available: q(dec!(99)),
            required: q(dec!(100)),
        })
    );
}

fn invalid_input(r: &Result<arbitrage_rs::linear::LinearEvaluation, ArbitrageError>) {
    assert!(
        matches!(r, Err(ArbitrageError::InvalidInput { .. })),
        "{r:?}"
    );
}

#[test]
fn cash_and_carry_input_validation() {
    let base = || cash_and_carry((dec!(101.50), dec!(101.60)));

    let mut i = base();
    i.future.instrument = Instrument::dated(id("XYZ-SPOT"), Timestamp::from_millis(NEAR_EXPIRY));
    invalid_input(&evaluate_cash_and_carry(&i));

    let mut i = base();
    i.spot.instrument = Instrument::dated(id("XYZ-S"), Timestamp::from_millis(NEAR_EXPIRY));
    invalid_input(&evaluate_cash_and_carry(&i));

    let mut i = base();
    i.future.instrument = Instrument::undated(id("XYZ-F1"));
    invalid_input(&evaluate_cash_and_carry(&i));

    // 0.005 XYZ is not a whole spot lot of 0.01.
    let mut i = base();
    i.terms.package = q(dec!(0.005));
    invalid_input(&evaluate_cash_and_carry(&i));

    let mut i = base();
    i.time_to_expiry = Decimal::ZERO;
    assert_eq!(
        evaluate_cash_and_carry(&i),
        Err(ArbitrageError::InvalidValue {
            field: "time to expiry",
            value: Decimal::ZERO,
            reason: "must be positive",
        })
    );

    let mut i = base();
    i.terms.as_of = Timestamp::from_millis(NEAR_EXPIRY);
    assert!(matches!(
        evaluate_cash_and_carry(&i),
        Err(ArbitrageError::Expired { .. })
    ));
}

#[test]
fn perp_basis_input_validation() {
    let base = || perp_basis((dec!(100.30), dec!(100.40)), dec!(0.0001), dec!(100.35));

    let mut i = base();
    i.perp.instrument = Instrument::undated(id("XYZ-SPOT"));
    invalid_input(&evaluate_perp_basis(&i));

    let mut i = base();
    i.perp.instrument = Instrument::dated(id("XYZ-PERP"), Timestamp::from_millis(NEAR_EXPIRY));
    invalid_input(&evaluate_perp_basis(&i));

    let mut i = base();
    i.spot.instrument = Instrument::dated(id("XYZ-SPOT"), Timestamp::from_millis(NEAR_EXPIRY));
    invalid_input(&evaluate_perp_basis(&i));

    // 1 XYZ is 100 perpetual contracts, not a whole multiple of a lot of 3.
    let mut i = base();
    i.perp.lot = q(dec!(3));
    invalid_input(&evaluate_perp_basis(&i));
}

#[test]
fn calendar_input_validation() {
    let mut i = calendar();
    i.far.instrument = i.near.instrument.clone();
    invalid_input(&evaluate_calendar(&i));

    // The near leg must expire first.
    let mut i = calendar();
    i.near.instrument = Instrument::dated(id("XYZ-F1"), Timestamp::from_millis(FAR_EXPIRY));
    invalid_input(&evaluate_calendar(&i));

    // An undated far leg.
    let mut i = calendar();
    i.far.instrument = Instrument::undated(id("XYZ-F2"));
    invalid_input(&evaluate_calendar(&i));

    // Funding with a dated near leg.
    let mut i = calendar();
    i.funding = perp_calendar().funding;
    invalid_input(&evaluate_calendar(&i));

    // A perpetual near leg without funding.
    let mut i = perp_calendar();
    i.funding = None;
    invalid_input(&evaluate_calendar(&i));

    let mut i = calendar();
    i.time_between = dec!(-0.25);
    assert_eq!(
        evaluate_calendar(&i),
        Err(ArbitrageError::InvalidValue {
            field: "time between expiries",
            value: dec!(-0.25),
            reason: "must be positive",
        })
    );

    // The near leg has expired.
    let mut i = calendar();
    i.terms.as_of = Timestamp::from_millis(NEAR_EXPIRY);
    assert!(matches!(
        evaluate_calendar(&i),
        Err(ArbitrageError::Expired { .. })
    ));
}

#[test]
fn funding_scales_with_the_package() {
    // The positive funding fixture with 2 XYZ (2 spot, 200 perpetual
    // contracts). Buy spot at 100.10, sell the perpetual at 100.30:
    //   gross = 2 * (100.30 - 100.10) + 2 * 100.35 * 0.0001 * 3
    //         = 0.40 + 0.06021 = 0.46021
    //   fees  = 2 * 100.10 * 5e-4 = 0.1001 + 2 * 100.30 * 5e-4 = 0.1003
    //         = 0.2004
    //   net   = 0.46021 - 0.2004 = 0.25981
    let mut i = perp_basis((dec!(100.30), dec!(100.40)), dec!(0.0001), dec!(100.35));
    i.terms.package = q(dec!(2));
    let ev = evaluate_perp_basis(&i).unwrap();
    assert_eq!(
        numbers(&ev.long_near),
        (
            dec!(0.46021),
            [dec!(0.1001), dec!(0.1003), dec!(0.2004)],
            dec!(0.25981)
        )
    );
    let e = ev.long_near.edge().unwrap();
    assert_eq!(e.expected_net.currency(), Currency::new("USDC").unwrap());
    assert_eq!(e.as_of, Timestamp::from_millis(0));
}

#[test]
fn near_leg_book_is_checked() {
    // Half a spot contract at the ask, one needed: buying spot is unavailable.
    let mut i = cash_and_carry((dec!(101.50), dec!(101.60)));
    i.spot.book.ask = Some(Level::new(px(dec!(100.10)), q(dec!(0.5))));
    let ev = evaluate_cash_and_carry(&i).unwrap();
    assert_eq!(
        ev.long_near,
        LinearDirectionEvaluation::Unavailable(LinearUnavailable::InsufficientLiquidity {
            instrument: i.spot.instrument.clone(),
            available: q(dec!(0.5)),
            required: q(dec!(1)),
        })
    );
    // No spot bid: selling spot is unavailable.
    let mut i = cash_and_carry((dec!(101.50), dec!(101.60)));
    i.spot.book.bid = None;
    let ev = evaluate_cash_and_carry(&i).unwrap();
    assert_eq!(
        ev.short_near,
        LinearDirectionEvaluation::Unavailable(LinearUnavailable::MissingQuote {
            instrument: i.spot.instrument.clone(),
            side: Action::Sell,
        })
    );
}

#[test]
fn calendar_far_leg_and_expiry_order() {
    // Perpetual near leg, far future expiring at NEAR_EXPIRY: expired.
    let mut i = perp_calendar();
    i.terms.as_of = Timestamp::from_millis(NEAR_EXPIRY);
    assert!(matches!(
        evaluate_calendar(&i),
        Err(ArbitrageError::Expired { .. })
    ));
    // A near leg dated after the far leg.
    let mut i = calendar();
    i.near.instrument = Instrument::dated(id("XYZ-F1"), Timestamp::from_millis(FAR_EXPIRY + 1));
    invalid_input(&evaluate_calendar(&i));
}

#[test]
fn tie_returns_long_near() {
    // Spot and perpetual both 100 / 100, zero funding: each direction has
    // gross 0 and fees 0.05 + 0.05, so both nets are -0.1.
    let mut i = perp_basis((dec!(100), dec!(100)), Decimal::ZERO, dec!(100));
    i.spot.book = book(dec!(100), dec!(100), dec!(10));
    let ev = evaluate_perp_basis(&i).unwrap();
    assert_eq!(
        ev.long_near.edge().unwrap().expected_net.amount(),
        dec!(-0.1)
    );
    assert_eq!(
        ev.short_near.edge().unwrap().expected_net.amount(),
        dec!(-0.1)
    );
    assert_eq!(
        ev.executable().unwrap().direction,
        LinearDirection::LongNearShortFar
    );
}

#[test]
fn linear_carry_with_28_places() {
    // r = 0.05 + 1e-28 and tau = 0.25 + 1e-28: r * tau = 0.0125 + 3e-29
    // + 1e-56, whose product with 100.10 has 58 places.
    let mut i = cash_and_carry((dec!(101.50), dec!(101.60)));
    i.carry_rate = dec!(0.0500000000000000000000000001);
    i.time_to_expiry = dec!(0.2500000000000000000000000001);
    i.terms.edge_dp = 28;
    let ev = evaluate_cash_and_carry(&i).unwrap();
    // Buy spot: 1.40 - 100.10 * r * tau
    //   = 0.14875 - 3.003e-27 - 1.001e-54
    //   = 0.14874999999999999999999999699699...9
    // rounded once, half even, to 28 places: ...9970.
    assert_eq!(
        numbers(&ev.long_near).0,
        dec!(0.1487499999999999999999999970)
    );
    // Sell spot: 100.00 * (1 + r * tau) - 101.60
    //   = -0.35 + 3e-27 + 1e-54, to 28 places: -0.3499999999999999999999999970.
    assert_eq!(
        numbers(&ev.short_near).0,
        dec!(-0.3499999999999999999999999970)
    );
}

#[test]
fn linear_gross_is_exact_when_only_intermediates_overflow() {
    // Spot and future at MAX, carry 2 over 0.5: package * spot * rate is
    // 2 * MAX, not a Decimal, but the carry term MAX and the gross fit.
    let max = Decimal::MAX;
    let mut i = cash_and_carry((max, max));
    i.spot.book = book(max, max, dec!(10));
    i.spot.fee = FeeRate::zero();
    i.future.fee = FeeRate::zero();
    i.carry_rate = dec!(2);
    i.time_to_expiry = dec!(0.5);
    let ev = evaluate_cash_and_carry(&i).unwrap();
    // Buy spot: MAX - MAX - MAX * 2 * 0.5 = -MAX.
    assert_eq!(ev.long_near.edge().unwrap().expected_net.amount(), -max);
    // Sell spot: MAX + MAX * 1 - MAX = MAX.
    assert_eq!(ev.short_near.edge().unwrap().expected_net.amount(), max);
}
