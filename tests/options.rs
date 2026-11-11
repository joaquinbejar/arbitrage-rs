//! Option relations: the synthetic future against a dated future or a
//! perpetual, and the box spread.
//!
//! Shared fixture: options trade 0.1 XYZ per contract in lots of 1 and pay
//! 3 bps of the underlying notional at an index of 100 (0.03 per XYZ),
//! capped at 12.5 % of the premium; the perpetual trades 0.01 XYZ per
//! contract in lots of 1 and pays 5 bps of its notional. One package is
//! 1 XYZ: 10 contracts per option, 100 perpetual contracts. Fees are
//! truncated to 1e-6 and the gross edge is rounded half even to 1e-6.

// Tests fail loudly: the library's no-panic lints do not apply here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::type_complexity,
    clippy::needless_range_loop
)]

use arbitrage_rs::box_spread::{
    BoxDirection, BoxDirectionEvaluation, BoxEvaluation, BoxInput, evaluate_box,
};
use arbitrage_rs::legs::FailurePolicy;
use arbitrage_rs::linear::PerpFunding;
use arbitrage_rs::package::{
    BOX_HIGH_CALL_LEG, BOX_HIGH_PUT_LEG, BOX_LOW_CALL_LEG, BOX_LOW_PUT_LEG, FUTURE_LEG, PackagePlan,
};
use arbitrage_rs::parity::{
    DirectionEvaluation, OptionFee, ParityDirection, ParityFuture, ParityInput, ParityOption,
    ParityUnavailable, SyntheticInput, evaluate_parity, evaluate_synthetic,
};
use arbitrage_rs::venues::VenueId;
use arbitrage_rs::{
    Action, ArbitrageError, Currency, Decimal, FeeRate, Instrument, InstrumentId, Level, Price,
    Quantity, Result, Rounding, Timestamp, TopOfBook,
};
use rust_decimal_macros::dec;

const EXPIRY: i64 = 1_000_000;
const D28: Decimal = dec!(0.9900498337491680535739059772);

fn q(x: Decimal) -> Quantity {
    Quantity::new(x).unwrap()
}

fn px(x: Decimal) -> Price {
    Price::new(x).unwrap()
}

fn usdc() -> Currency {
    Currency::new("USDC").unwrap()
}

fn book(bid: Decimal, ask: Decimal, size: Decimal) -> TopOfBook {
    TopOfBook {
        bid: Some(Level::new(px(bid), q(size))),
        ask: Some(Level::new(px(ask), q(size))),
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

fn funding(rate: Decimal) -> PerpFunding {
    PerpFunding {
        rate,
        mark: px(dec!(100.95)),
        intervals: 90,
    }
}

/// K = 100, D = 0.99, call 6.00 / 6.10, put 4.90 / 5.00, perpetual `perp`,
/// funding `rate` for 90 intervals at a mark of 100.95.
fn synthetic(perp: (Decimal, Decimal), rate: Decimal) -> SyntheticInput {
    SyntheticInput {
        parity: ParityInput {
            as_of: Timestamp::from_millis(0),
            quote_currency: usdc(),
            strike: px(dec!(100)),
            discount: dec!(0.99),
            index: px(dec!(100)),
            call: option("XYZ-100-C", dec!(6.00), dec!(6.10)),
            put: option("XYZ-100-P", dec!(4.90), dec!(5.00)),
            future: ParityFuture {
                instrument: Instrument::undated(InstrumentId::new("XYZ-PERP").unwrap()),
                book: book(perp.0, perp.1, dec!(500)),
                contract_size: q(dec!(0.01)),
                lot: q(dec!(1)),
                fee: FeeRate::from_bps(dec!(5)).unwrap(),
            },
            package: q(dec!(1)),
            fee_dp: 6,
            fee_rounding: Rounding::TowardZero,
            edge_dp: 6,
            edge_rounding: Rounding::HalfEven,
        },
        funding: Some(funding(rate)),
    }
}

fn synth_fixture() -> SyntheticInput {
    synthetic((dec!(100.90), dec!(101.00)), dec!(0.0001))
}

/// Strikes 90 and 110, D = 0.99, the four options at the given books.
fn boxed(
    lc: (Decimal, Decimal),
    lp: (Decimal, Decimal),
    hc: (Decimal, Decimal),
    hp: (Decimal, Decimal),
) -> BoxInput {
    BoxInput {
        as_of: Timestamp::from_millis(0),
        quote_currency: usdc(),
        low_strike: px(dec!(90)),
        high_strike: px(dec!(110)),
        discount: dec!(0.99),
        index: px(dec!(100)),
        low_call: option("XYZ-90-C", lc.0, lc.1),
        low_put: option("XYZ-90-P", lp.0, lp.1),
        high_call: option("XYZ-110-C", hc.0, hc.1),
        high_put: option("XYZ-110-P", hp.0, hp.1),
        package: q(dec!(1)),
        fee_dp: 6,
        fee_rounding: Rounding::TowardZero,
        edge_dp: 6,
        edge_rounding: Rounding::HalfEven,
    }
}

/// A box cheap enough to buy.
fn long_box() -> BoxInput {
    boxed(
        (dec!(11.40), dec!(11.50)),
        (dec!(1.50), dec!(1.60)),
        (dec!(1.80), dec!(1.90)),
        (dec!(11.20), dec!(11.30)),
    )
}

/// A box rich enough to sell.
fn short_box() -> BoxInput {
    boxed(
        (dec!(12.00), dec!(12.20)),
        (dec!(1.50), dec!(1.60)),
        (dec!(1.80), dec!(1.90)),
        (dec!(11.50), dec!(11.60)),
    )
}

/// `(gross, [call, put, hedge] fees, net)`.
fn synth_numbers(d: &DirectionEvaluation) -> (Decimal, [Decimal; 3], Decimal) {
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

/// `(gross, total fees, net)`.
fn box_numbers(d: &BoxDirectionEvaluation) -> (Decimal, Decimal, Decimal) {
    let e = d.edge().expect("edge computed");
    (
        e.expected_gross.amount(),
        e.expected_fees.total.money().amount(),
        e.expected_net.amount(),
    )
}

fn invalid_input<T: core::fmt::Debug>(r: &Result<T>) {
    assert!(
        matches!(r, Err(ArbitrageError::InvalidInput { .. })),
        "{r:?}"
    );
}

// ---------------------------------------------------------------- synthetic

#[test]
fn synthetic_against_perpetual_positive_funding() {
    let ev = evaluate_synthetic(&synth_fixture()).unwrap();
    // Funding to expiry: 100.95 * 0.0001 * 90 = 0.90855.
    // Reversal: buy the call at 6.10, sell the put at 4.90, sell the
    // perpetual at 100.90:
    //   gross = 0.99 * (100.90 - 100 + 0.90855) - 6.10 + 4.90
    //         = 0.99 * 1.80855 - 1.20 = 1.7904645 - 1.20 = 0.5904645
    //         -> 0.590464 (half even, the 4 is even)
    //   fees  = 0.03 (call) + 0.03 (put) + 100.90 * 5e-4 = 0.05045
    //   net   = 0.590464 - 0.11045 = 0.480014
    assert_eq!(
        synth_numbers(&ev.reversal),
        (
            dec!(0.590464),
            [dec!(0.03), dec!(0.03), dec!(0.05045)],
            dec!(0.480014)
        )
    );
    // Conversion: sell the call at 6.00, buy the put at 5.00, buy the
    // perpetual at 101.00, paying funding:
    //   gross = 6.00 - 5.00 - 0.99 * (1.00 + 0.90855) = 1 - 1.8894645
    //         = -0.8894645 -> -0.889464
    //   fees  = 0.03 + 0.03 + 0.0505; net = -0.999964
    assert_eq!(
        synth_numbers(&ev.conversion),
        (
            dec!(-0.889464),
            [dec!(0.03), dec!(0.03), dec!(0.0505)],
            dec!(-0.999964)
        )
    );
    let best = ev.executable().unwrap();
    assert_eq!(best.direction, ParityDirection::Reversal);
    assert_eq!(best.as_of, Timestamp::from_millis(0));
    assert_eq!(best.expected_net.currency(), usdc());
    // Legs: 10 option contracts, 100 perpetual contracts.
    assert_eq!(
        (
            best.legs.call.side,
            best.legs.call.quantity,
            best.legs.call.price
        ),
        (Action::Buy, q(dec!(10)), px(dec!(6.10)))
    );
    assert_eq!(
        (
            best.legs.put.side,
            best.legs.put.quantity,
            best.legs.put.price
        ),
        (Action::Sell, q(dec!(10)), px(dec!(4.90)))
    );
    assert_eq!(
        (
            best.legs.future.side,
            best.legs.future.quantity,
            best.legs.future.price
        ),
        (Action::Sell, q(dec!(100)), px(dec!(100.90)))
    );
}

#[test]
fn synthetic_negative_funding_favours_the_conversion() {
    // Funding 100.95 * -0.0002 * 90 = -1.8171: shorts pay longs.
    let ev = evaluate_synthetic(&synthetic((dec!(100.90), dec!(101.00)), dec!(-0.0002))).unwrap();
    // Conversion: 1.00 - 0.99 * (1.00 - 1.8171) = 1 + 0.808929 = 1.808929;
    // fees 0.1105; net 1.698429.
    assert_eq!(
        synth_numbers(&ev.conversion),
        (
            dec!(1.808929),
            [dec!(0.03), dec!(0.03), dec!(0.0505)],
            dec!(1.698429)
        )
    );
    // Reversal: 0.99 * (0.90 - 1.8171) - 1.20 = -0.907929 - 1.20
    // = -2.107929; fees 0.11045; net -2.218379.
    assert_eq!(synth_numbers(&ev.reversal).2, dec!(-2.218379));
    assert_eq!(
        ev.executable().unwrap().direction,
        ParityDirection::Conversion
    );
}

#[test]
fn synthetic_no_edge() {
    // Perpetual 101.00 / 101.10, no funding.
    // Reversal: 0.99 * 1.00 - 1.20 = -0.21; fees 0.03 + 0.03 + 0.0505;
    // net -0.3205. Conversion: 1 - 0.99 * 1.10 = -0.089; fees
    // 0.03 + 0.03 + 0.05055; net -0.19955.
    let ev = evaluate_synthetic(&synthetic((dec!(101.00), dec!(101.10)), Decimal::ZERO)).unwrap();
    assert_eq!(synth_numbers(&ev.reversal).2, dec!(-0.3205));
    assert_eq!(synth_numbers(&ev.conversion).2, dec!(-0.19955));
    assert_eq!(
        ev.executable().unwrap().expected_net.amount(),
        dec!(-0.19955)
    );
}

#[test]
fn synthetic_package_scales_every_term() {
    // 2 XYZ: 20 option and 200 perpetual contracts, funding included.
    //   reversal gross = 2 * 0.5904645 = 1.180929 (exact)
    //   fees = 0.06 + 0.06 + 2 * 100.90 * 5e-4 = 0.1009; total 0.2209
    //   net = 0.960029
    let mut i = synth_fixture();
    i.parity.package = q(dec!(2));
    let ev = evaluate_synthetic(&i).unwrap();
    assert_eq!(
        synth_numbers(&ev.reversal),
        (
            dec!(1.180929),
            [dec!(0.06), dec!(0.06), dec!(0.1009)],
            dec!(0.960029)
        )
    );
    let legs = &ev.reversal.edge().unwrap().legs;
    assert_eq!(
        (legs.call.quantity, legs.put.quantity, legs.future.quantity),
        (q(dec!(20)), q(dec!(20)), q(dec!(200)))
    );
}

#[test]
fn synthetic_with_a_dated_future_is_parity() {
    let mut i = synth_fixture();
    i.parity.future.instrument = dated("XYZ-F");
    i.funding = None;
    assert_eq!(
        evaluate_synthetic(&i).unwrap(),
        evaluate_parity(&i.parity).unwrap()
    );
}

#[test]
fn synthetic_tie_returns_the_reversal() {
    // C 1 / 3, P 1 / 3, perpetual 99 / 101, K 100, D 1, zero fees and
    // funding: reversal
    // 1 * (99 - 100) - 3 + 1 = -3, conversion 1 - 3 - (101 - 100) = -3.
    let mut i = synthetic((dec!(99), dec!(101)), Decimal::ZERO);
    i.parity.discount = dec!(1);
    i.parity.call.book = book(dec!(1), dec!(3), dec!(50));
    i.parity.put.book = book(dec!(1), dec!(3), dec!(50));
    i.parity.future.fee = FeeRate::zero();
    for o in [&mut i.parity.call, &mut i.parity.put] {
        o.fee = OptionFee::Premium {
            rate: FeeRate::zero(),
        };
    }
    let ev = evaluate_synthetic(&i).unwrap();
    assert_eq!(synth_numbers(&ev.reversal).2, dec!(-3));
    assert_eq!(synth_numbers(&ev.conversion).2, dec!(-3));
    assert_eq!(
        ev.executable().unwrap().direction,
        ParityDirection::Reversal
    );
}

#[test]
fn synthetic_discount_with_28_places() {
    // Package 0.1 (1 option and 10 perpetual contracts): 0.1 * D alone has
    // 29 places.
    // gross = 0.1 * (D * 1.80855 - 1.20)
    //       = 0.0590554626827057883291087655065060 (34 places), rounded
    // once, half even, to 28: 0.0590554626827057883291087655.
    // Conversion: 0.1 * (1 - D * 1.90855)
    //       = -0.0889559610201974688648478252785060
    //       -> -0.0889559610201974688648478253.
    let mut i = synth_fixture();
    i.parity.package = q(dec!(0.1));
    i.parity.discount = D28;
    i.parity.edge_dp = 28;
    let ev = evaluate_synthetic(&i).unwrap();
    assert_eq!(
        synth_numbers(&ev.reversal).0,
        dec!(0.0590554626827057883291087655)
    );
    assert_eq!(
        synth_numbers(&ev.conversion).0,
        dec!(-0.0889559610201974688648478253)
    );
}

#[test]
fn synthetic_gross_is_exact_when_only_intermediates_overflow() {
    // K and the perpetual at MAX, D = 2: D * H alone is 2 * MAX, but
    // D * (H - K + f) with f = 10 * 0.05 * 1 = 0.5 is 1.
    //   reversal = 2 * 0.5 - 1 + 1 = 1; fees 0.03 + 0.03; net 0.94
    //   conversion = 1 - 1 - 2 * 0.5 = -1; net -1.06
    let max = Decimal::MAX;
    let mut i = synthetic((max, max), dec!(0.05));
    i.funding = Some(PerpFunding {
        rate: dec!(0.05),
        mark: px(dec!(10)),
        intervals: 1,
    });
    i.parity.strike = px(max);
    i.parity.discount = dec!(2);
    i.parity.call.book = book(dec!(1), dec!(1), dec!(50));
    i.parity.put.book = book(dec!(1), dec!(1), dec!(50));
    i.parity.future.fee = FeeRate::zero();
    let ev = evaluate_synthetic(&i).unwrap();
    assert_eq!(
        synth_numbers(&ev.reversal),
        (dec!(1), [dec!(0.03), dec!(0.03), dec!(0)], dec!(0.94))
    );
    assert_eq!(synth_numbers(&ev.conversion).2, dec!(-1.06));
}

#[test]
fn synthetic_book_checks_on_every_leg() {
    let base = synth_fixture;
    // Missing side, per leg and direction.
    let cases: [(fn(&mut SyntheticInput), bool, &str, Action); 6] = [
        (
            |i| i.parity.call.book.ask = None,
            true,
            "XYZ-100-C",
            Action::Buy,
        ),
        (
            |i| i.parity.call.book.bid = None,
            false,
            "XYZ-100-C",
            Action::Sell,
        ),
        (
            |i| i.parity.put.book.bid = None,
            true,
            "XYZ-100-P",
            Action::Sell,
        ),
        (
            |i| i.parity.put.book.ask = None,
            false,
            "XYZ-100-P",
            Action::Buy,
        ),
        (
            |i| i.parity.future.book.bid = None,
            true,
            "XYZ-PERP",
            Action::Sell,
        ),
        (
            |i| i.parity.future.book.ask = None,
            false,
            "XYZ-PERP",
            Action::Buy,
        ),
    ];
    for (edit, reversal, id, side) in cases {
        let mut i = base();
        edit(&mut i);
        let ev = evaluate_synthetic(&i).unwrap();
        let (hit, other) = if reversal {
            (&ev.reversal, &ev.conversion)
        } else {
            (&ev.conversion, &ev.reversal)
        };
        match hit {
            DirectionEvaluation::Unavailable(ParityUnavailable::MissingQuote {
                instrument,
                side: s,
            }) => assert_eq!((instrument.id.as_str(), *s), (id, side)),
            other => panic!("{id} {side:?}: {other:?}"),
        }
        assert!(other.edge().is_some());
    }
    // Nine option contracts at the top, ten needed, per leg and side.
    type Edit = fn(&mut SyntheticInput);
    let thin: [(Edit, bool, &str); 4] = [
        (
            |i| i.parity.call.book.ask = Some(Level::new(px(dec!(6.10)), q(dec!(9)))),
            true,
            "XYZ-100-C",
        ),
        (
            |i| i.parity.call.book.bid = Some(Level::new(px(dec!(6.00)), q(dec!(9)))),
            false,
            "XYZ-100-C",
        ),
        (
            |i| i.parity.put.book.bid = Some(Level::new(px(dec!(4.90)), q(dec!(9)))),
            true,
            "XYZ-100-P",
        ),
        (
            |i| i.parity.put.book.ask = Some(Level::new(px(dec!(5.00)), q(dec!(9)))),
            false,
            "XYZ-100-P",
        ),
    ];
    for (edit, reversal, id) in thin {
        let mut i = base();
        edit(&mut i);
        let ev = evaluate_synthetic(&i).unwrap();
        let hit = if reversal {
            &ev.reversal
        } else {
            &ev.conversion
        };
        match hit {
            DirectionEvaluation::Unavailable(ParityUnavailable::InsufficientLiquidity {
                instrument,
                available,
                required,
            }) => assert_eq!(
                (instrument.id.as_str(), *available, *required),
                (id, q(dec!(9)), q(dec!(10)))
            ),
            x => panic!("{id}: {x:?}"),
        }
    }
    // Perpetual ask with 99 contracts: the conversion is unavailable.
    let mut i = base();
    i.parity.future.book.ask = Some(Level::new(px(dec!(101.00)), q(dec!(99))));
    assert!(matches!(
        evaluate_synthetic(&i).unwrap().conversion,
        DirectionEvaluation::Unavailable(ParityUnavailable::InsufficientLiquidity { .. })
    ));
    // Too few perpetual contracts at the bid: 99 of 100.
    let mut i = base();
    i.parity.future.book.bid = Some(Level::new(px(dec!(100.90)), q(dec!(99))));
    let ev = evaluate_synthetic(&i).unwrap();
    assert_eq!(
        ev.reversal,
        DirectionEvaluation::Unavailable(ParityUnavailable::InsufficientLiquidity {
            instrument: i.parity.future.instrument.clone(),
            available: q(dec!(99)),
            required: q(dec!(100)),
        })
    );
}

#[test]
fn synthetic_input_validation() {
    // Funding with a dated hedge.
    let mut i = synth_fixture();
    i.parity.future.instrument = dated("XYZ-F");
    invalid_input(&evaluate_synthetic(&i));
    // A perpetual without funding.
    let mut i = synth_fixture();
    i.funding = None;
    invalid_input(&evaluate_synthetic(&i));
    // Undated options.
    let mut i = synth_fixture();
    i.parity.call.instrument = Instrument::undated(InstrumentId::new("XYZ-100-C").unwrap());
    invalid_input(&evaluate_synthetic(&i));
    // Options of different expiries.
    let mut i = synth_fixture();
    i.parity.put.instrument = Instrument::dated(
        InstrumentId::new("XYZ-100-P").unwrap(),
        Timestamp::from_millis(EXPIRY + 1),
    );
    invalid_input(&evaluate_synthetic(&i));
    // The perpetual is the call.
    let mut i = synth_fixture();
    i.parity.future.instrument = Instrument::undated(InstrumentId::new("XYZ-100-C").unwrap());
    invalid_input(&evaluate_synthetic(&i));
    // 0.05 XYZ is half an option contract.
    let mut i = synth_fixture();
    i.parity.package = q(dec!(0.05));
    invalid_input(&evaluate_synthetic(&i));
    // Non-positive discount.
    let mut i = synth_fixture();
    i.parity.discount = Decimal::ZERO;
    assert!(matches!(
        evaluate_synthetic(&i),
        Err(ArbitrageError::InvalidValue {
            field: "discount factor",
            ..
        })
    ));
    // Expired options, with a perpetual hedge that never expires: the
    // error names the call.
    let mut i = synth_fixture();
    i.parity.as_of = Timestamp::from_millis(EXPIRY);
    match evaluate_synthetic(&i) {
        Err(ArbitrageError::Expired { instrument, .. }) => assert_eq!(instrument, "XYZ-100-C"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn synthetic_plan_has_the_three_legs() {
    // Quotes at 500: the plan is created at the edge's as_of, so a
    // deadline at 500 is refused and one at 501 accepted.
    let mut i = synth_fixture();
    set_as_of(&mut i, 500);
    let ev = evaluate_synthetic(&i).unwrap();
    let edge = ev.executable().unwrap();
    assert_eq!(edge.as_of, Timestamp::from_millis(500));
    invalid_input(&PackagePlan::from_synthetic(
        &i,
        edge,
        &VenueId::new("V1").unwrap(),
        Some(Timestamp::from_millis(500)),
        FailurePolicy::Abort,
    ));
    let plan = PackagePlan::from_synthetic(
        &i,
        edge,
        &VenueId::new("V1").unwrap(),
        Some(Timestamp::from_millis(501)),
        FailurePolicy::Abort,
    )
    .unwrap();
    let legs = plan.legs();
    assert_eq!(legs.len(), 3);
    assert_eq!(legs[FUTURE_LEG].spec.spec.leg, edge.legs.future);
    assert_eq!(legs[FUTURE_LEG].spec.contract_size, q(dec!(0.01)));
}

// ---------------------------------------------------------------------- box

#[test]
fn long_box_fixture() {
    let ev = evaluate_box(&long_box()).unwrap();
    // Long: buy the 90 call at 11.50 and the 110 put at 11.30, sell the
    // 90 put at 1.50 and the 110 call at 1.80:
    //   cost  = 11.50 - 1.50 - 1.80 + 11.30 = 19.50
    //   gross = 0.99 * (110 - 90) - 19.50 = 19.80 - 19.50 = 0.30
    //   fees  = 4 * 0.03 = 0.12 (3 bps of 100, below 12.5 % of each
    //           premium); net = 0.18
    assert_eq!(box_numbers(&ev.long), (dec!(0.3), dec!(0.12), dec!(0.18)));
    // Short: 11.40 - 1.60 - 1.90 + 11.20 = 19.10; gross 19.10 - 19.80
    // = -0.70; net -0.82.
    assert_eq!(
        box_numbers(&ev.short),
        (dec!(-0.7), dec!(0.12), dec!(-0.82))
    );
    let best = ev.executable().unwrap();
    assert_eq!(best.direction, BoxDirection::Long);
    assert_eq!(best.as_of, Timestamp::from_millis(0));
    assert_eq!(best.expected_net.currency(), usdc());
    let l = &best.legs;
    let sides = |leg: &arbitrage_rs::Leg| (leg.side, leg.quantity, leg.price);
    assert_eq!(
        sides(&l.low_call),
        (Action::Buy, q(dec!(10)), px(dec!(11.50)))
    );
    assert_eq!(
        sides(&l.low_put),
        (Action::Sell, q(dec!(10)), px(dec!(1.50)))
    );
    assert_eq!(
        sides(&l.high_call),
        (Action::Sell, q(dec!(10)), px(dec!(1.80)))
    );
    assert_eq!(
        sides(&l.high_put),
        (Action::Buy, q(dec!(10)), px(dec!(11.30)))
    );
    let f = &best.expected_fees;
    assert_eq!(
        [f.low_call, f.low_put, f.high_call, f.high_put].map(|c| c.money().amount()),
        [dec!(0.03); 4]
    );
}

#[test]
fn short_box_fixture() {
    let ev = evaluate_box(&short_box()).unwrap();
    // Short: sell the 90 call at 12.00 and the 110 put at 11.50, buy the
    // 90 put at 1.60 and the 110 call at 1.90:
    //   receipts = 12.00 - 1.60 - 1.90 + 11.50 = 20.00
    //   gross    = 20.00 - 19.80 = 0.20; fees 0.12; net 0.08
    assert_eq!(box_numbers(&ev.short), (dec!(0.2), dec!(0.12), dec!(0.08)));
    // Long: 19.80 - (12.20 - 1.50 - 1.80 + 11.60) = 19.80 - 20.50 = -0.70.
    assert_eq!(box_numbers(&ev.long), (dec!(-0.7), dec!(0.12), dec!(-0.82)));
    let best = ev.executable().unwrap();
    assert_eq!(best.direction, BoxDirection::Short);
    assert_eq!(best.legs.low_call.side, Action::Sell);
    assert_eq!(best.legs.low_put.side, Action::Buy);
    assert_eq!(best.legs.high_call.side, Action::Buy);
    assert_eq!(best.legs.high_put.side, Action::Sell);
}

#[test]
fn box_no_edge() {
    // The short fixture with the 110 put at 11.40 / 11.60:
    // short 12.00 - 1.60 - 1.90 + 11.40 - 19.80 = 0.10, net -0.02;
    // long -0.70, net -0.82.
    let mut i = short_box();
    i.high_put.book = book(dec!(11.40), dec!(11.60), dec!(50));
    let ev = evaluate_box(&i).unwrap();
    assert_eq!(box_numbers(&ev.short), (dec!(0.1), dec!(0.12), dec!(-0.02)));
    assert_eq!(box_numbers(&ev.long).2, dec!(-0.82));
}

#[test]
fn box_premium_cap_binds_on_a_cheap_leg() {
    // The 110 call at 0.10 / 0.20 in the long box: selling it at 0.10
    // costs min(0.03, 0.125 * 0.10) = 0.0125.
    //   long cost = 11.50 - 1.50 - 0.10 + 11.30 = 21.20; gross -1.40
    //   fees 0.03 + 0.03 + 0.0125 + 0.03 = 0.1025; net -1.5025
    let mut i = long_box();
    i.high_call.book = book(dec!(0.10), dec!(0.20), dec!(50));
    let ev = evaluate_box(&i).unwrap();
    assert_eq!(
        ev.long
            .edge()
            .unwrap()
            .expected_fees
            .high_call
            .money()
            .amount(),
        dec!(0.0125)
    );
    assert_eq!(
        box_numbers(&ev.long),
        (dec!(-1.4), dec!(0.1025), dec!(-1.5025))
    );
}

#[test]
fn box_package_scales_every_term() {
    // 2 XYZ, 20 contracts per leg: gross 0.60, fees 4 * 0.06 = 0.24,
    // net 0.36.
    let mut i = long_box();
    i.package = q(dec!(2));
    let ev = evaluate_box(&i).unwrap();
    assert_eq!(box_numbers(&ev.long), (dec!(0.6), dec!(0.24), dec!(0.36)));
    assert_eq!(ev.long.edge().unwrap().legs.high_put.quantity, q(dec!(20)));
}

#[test]
fn box_tie_returns_the_long_box() {
    // Low call and high put 5 / 6, the others 1 / 1, D = 0.45, zero fees:
    //   long  = 0.45 * 20 - (6 - 1 - 1 + 6) = 9 - 10 = -1
    //   short = (5 - 1 - 1 + 5) - 9 = 8 - 9 = -1
    let mut i = boxed(
        (dec!(5), dec!(6)),
        (dec!(1), dec!(1)),
        (dec!(1), dec!(1)),
        (dec!(5), dec!(6)),
    );
    i.discount = dec!(0.45);
    for o in [
        &mut i.low_call,
        &mut i.low_put,
        &mut i.high_call,
        &mut i.high_put,
    ] {
        o.fee = OptionFee::Premium {
            rate: FeeRate::zero(),
        };
    }
    let ev = evaluate_box(&i).unwrap();
    assert_eq!(box_numbers(&ev.long).2, dec!(-1));
    assert_eq!(box_numbers(&ev.short).2, dec!(-1));
    assert_eq!(ev.executable().unwrap().direction, BoxDirection::Long);
}

#[test]
fn box_discount_with_28_places() {
    // Package 0.1 (1 contract per leg): 0.1 * D alone has 29 places.
    //   gross = 0.1 * (D * 20 - 19.50) = 0.0300996674983361071478119544
    //   fees  = 4 * min(3e-4 * 100 * 0.1, ...) = 4 * 0.003 = 0.012
    //   net   = 0.0180996674983361071478119544
    let mut i = long_box();
    i.discount = D28;
    i.package = q(dec!(0.1));
    i.edge_dp = 28;
    let ev = evaluate_box(&i).unwrap();
    assert_eq!(
        box_numbers(&ev.long),
        (
            dec!(0.0300996674983361071478119544),
            dec!(0.012),
            dec!(0.0180996674983361071478119544)
        )
    );
}

#[test]
fn box_gross_is_exact_when_only_intermediates_overflow() {
    // Strikes MAX - 20 and MAX, D = 2: D * K alone is 2 * MAX, the box is
    // worth 2 * 20 = 40. Every option at 1: long gross 40 - 0 = 40, fees
    // 4 * 0.03; net 39.88. Short -40, net -40.12.
    let max = Decimal::MAX;
    let mut i = boxed(
        (dec!(1), dec!(1)),
        (dec!(1), dec!(1)),
        (dec!(1), dec!(1)),
        (dec!(1), dec!(1)),
    );
    i.low_strike = px(max - dec!(20));
    i.high_strike = px(max);
    i.discount = dec!(2);
    let ev = evaluate_box(&i).unwrap();
    assert_eq!(box_numbers(&ev.long), (dec!(40), dec!(0.12), dec!(39.88)));
    assert_eq!(box_numbers(&ev.short).2, dec!(-40.12));
}

#[test]
fn box_book_checks_on_every_leg() {
    type Edit = fn(&mut BoxInput);
    // (edit, long direction hit, instrument, side)
    let cases: [(Edit, bool, &str, Action); 8] = [
        (
            |i| i.low_call.book.ask = None,
            true,
            "XYZ-90-C",
            Action::Buy,
        ),
        (
            |i| i.low_call.book.bid = None,
            false,
            "XYZ-90-C",
            Action::Sell,
        ),
        (
            |i| i.low_put.book.bid = None,
            true,
            "XYZ-90-P",
            Action::Sell,
        ),
        (
            |i| i.low_put.book.ask = None,
            false,
            "XYZ-90-P",
            Action::Buy,
        ),
        (
            |i| i.high_call.book.bid = None,
            true,
            "XYZ-110-C",
            Action::Sell,
        ),
        (
            |i| i.high_call.book.ask = None,
            false,
            "XYZ-110-C",
            Action::Buy,
        ),
        (
            |i| i.high_put.book.ask = None,
            true,
            "XYZ-110-P",
            Action::Buy,
        ),
        (
            |i| i.high_put.book.bid = None,
            false,
            "XYZ-110-P",
            Action::Sell,
        ),
    ];
    let pick = |ev: &BoxEvaluation, long| {
        if long {
            (ev.long.clone(), ev.short.clone())
        } else {
            (ev.short.clone(), ev.long.clone())
        }
    };
    for (edit, long, id, side) in cases {
        let mut i = long_box();
        edit(&mut i);
        let (hit, other) = pick(&evaluate_box(&i).unwrap(), long);
        match hit {
            BoxDirectionEvaluation::Unavailable(ParityUnavailable::MissingQuote {
                instrument,
                side: s,
            }) => assert_eq!((instrument.id.as_str(), s), (id, side)),
            x => panic!("{id} {side:?}: {x:?}"),
        }
        assert!(other.edge().is_some());
    }
    // Nine contracts at the top of each leg's book, ten needed.
    let thin: [(Edit, bool, &str); 8] = [
        (
            |i| i.low_call.book.ask = Some(Level::new(px(dec!(11.50)), q(dec!(9)))),
            true,
            "XYZ-90-C",
        ),
        (
            |i| i.low_call.book.bid = Some(Level::new(px(dec!(11.40)), q(dec!(9)))),
            false,
            "XYZ-90-C",
        ),
        (
            |i| i.low_put.book.bid = Some(Level::new(px(dec!(1.50)), q(dec!(9)))),
            true,
            "XYZ-90-P",
        ),
        (
            |i| i.low_put.book.ask = Some(Level::new(px(dec!(1.60)), q(dec!(9)))),
            false,
            "XYZ-90-P",
        ),
        (
            |i| i.high_call.book.bid = Some(Level::new(px(dec!(1.80)), q(dec!(9)))),
            true,
            "XYZ-110-C",
        ),
        (
            |i| i.high_call.book.ask = Some(Level::new(px(dec!(1.90)), q(dec!(9)))),
            false,
            "XYZ-110-C",
        ),
        (
            |i| i.high_put.book.ask = Some(Level::new(px(dec!(11.30)), q(dec!(9)))),
            true,
            "XYZ-110-P",
        ),
        (
            |i| i.high_put.book.bid = Some(Level::new(px(dec!(11.20)), q(dec!(9)))),
            false,
            "XYZ-110-P",
        ),
    ];
    for (edit, long, id) in thin {
        let mut i = long_box();
        edit(&mut i);
        let (hit, _) = pick(&evaluate_box(&i).unwrap(), long);
        match hit {
            BoxDirectionEvaluation::Unavailable(ParityUnavailable::InsufficientLiquidity {
                instrument,
                available,
                required,
            }) => assert_eq!(
                (instrument.id.as_str(), available, required),
                (id, q(dec!(9)), q(dec!(10)))
            ),
            x => panic!("{id}: {x:?}"),
        }
    }
}

#[test]
fn box_input_validation() {
    let ids = ["XYZ-90-C", "XYZ-90-P", "XYZ-110-C", "XYZ-110-P"];
    // Each pair of legs on one instrument.
    for a in 1..4 {
        for b in 0..a {
            let mut i = long_box();
            let same = dated(ids[b]);
            match a {
                1 => i.low_put.instrument = same,
                2 => i.high_call.instrument = same,
                _ => i.high_put.instrument = same,
            }
            invalid_input(&evaluate_box(&i));
        }
    }
    // An undated leg, each.
    for k in 0..4 {
        let mut i = long_box();
        let undated = Instrument::undated(InstrumentId::new(ids[k]).unwrap());
        match k {
            0 => i.low_call.instrument = undated,
            1 => i.low_put.instrument = undated,
            2 => i.high_call.instrument = undated,
            _ => i.high_put.instrument = undated,
        }
        invalid_input(&evaluate_box(&i));
    }
    // Another expiry, each leg.
    for k in 0..4 {
        let mut i = long_box();
        let later = Instrument::dated(
            InstrumentId::new(ids[k]).unwrap(),
            Timestamp::from_millis(EXPIRY + 1),
        );
        match k {
            0 => i.low_call.instrument = later,
            1 => i.low_put.instrument = later,
            2 => i.high_call.instrument = later,
            _ => i.high_put.instrument = later,
        }
        invalid_input(&evaluate_box(&i));
    }
    // Strikes equal, and inverted.
    let mut i = long_box();
    i.high_strike = i.low_strike;
    invalid_input(&evaluate_box(&i));
    let mut i = long_box();
    i.high_strike = px(dec!(80));
    invalid_input(&evaluate_box(&i));
    // Not whole lots.
    let mut i = long_box();
    i.package = q(dec!(0.05));
    invalid_input(&evaluate_box(&i));
    // Discount.
    let mut i = long_box();
    i.discount = dec!(-1);
    assert_eq!(
        evaluate_box(&i),
        Err(ArbitrageError::InvalidValue {
            field: "discount factor",
            value: dec!(-1),
            reason: "must be positive",
        })
    );
    // A negative fee rate and a cap above 1, on the last leg checked.
    let mut i = long_box();
    i.high_put.fee = OptionFee::Premium {
        rate: FeeRate::from_bps(dec!(-1)).unwrap(),
    };
    assert!(matches!(
        evaluate_box(&i),
        Err(ArbitrageError::InvalidValue {
            field: "option fee rate (bps)",
            ..
        })
    ));
    let mut i = long_box();
    i.low_call.fee = OptionFee::UnderlyingCapped {
        rate: FeeRate::from_bps(dec!(3)).unwrap(),
        cap: dec!(1.5),
    };
    assert!(matches!(
        evaluate_box(&i),
        Err(ArbitrageError::InvalidValue {
            field: "option fee cap",
            ..
        })
    ));
    // Expired.
    let mut i = long_box();
    i.as_of = Timestamp::from_millis(EXPIRY);
    assert!(matches!(
        evaluate_box(&i),
        Err(ArbitrageError::Expired { .. })
    ));
}

#[test]
fn box_plan_has_four_legs_in_order() {
    // Quotes at 500: the plan is created at the edge's as_of, so a
    // deadline at 500 is refused and one at 501 accepted.
    let mut i = long_box();
    set_as_of(&mut i, 500);
    let ev = evaluate_box(&i).unwrap();
    let edge = ev.executable().unwrap();
    assert_eq!(edge.as_of, Timestamp::from_millis(500));
    invalid_input(&PackagePlan::from_box(
        &i,
        edge,
        &VenueId::new("V1").unwrap(),
        Some(Timestamp::from_millis(500)),
        FailurePolicy::Abort,
    ));
    let plan = PackagePlan::from_box(
        &i,
        edge,
        &VenueId::new("V1").unwrap(),
        Some(Timestamp::from_millis(501)),
        FailurePolicy::Abort,
    )
    .unwrap();
    let legs = plan.legs();
    assert_eq!(legs.len(), 4);
    assert_eq!(legs[BOX_LOW_CALL_LEG].spec.spec.leg, edge.legs.low_call);
    assert_eq!(legs[BOX_LOW_PUT_LEG].spec.spec.leg, edge.legs.low_put);
    assert_eq!(legs[BOX_HIGH_CALL_LEG].spec.spec.leg, edge.legs.high_call);
    assert_eq!(legs[BOX_HIGH_PUT_LEG].spec.spec.leg, edge.legs.high_put);
    assert_eq!(plan.package(), q(dec!(1)));
}

trait AsOf {
    fn as_of_mut(&mut self) -> &mut Timestamp;
}
impl AsOf for SyntheticInput {
    fn as_of_mut(&mut self) -> &mut Timestamp {
        &mut self.parity.as_of
    }
}
impl AsOf for BoxInput {
    fn as_of_mut(&mut self) -> &mut Timestamp {
        &mut self.as_of
    }
}
fn set_as_of(i: &mut impl AsOf, ms: i64) {
    *i.as_of_mut() = Timestamp::from_millis(ms);
}

/// The long box with a distinct contract size, lot and fee on every leg:
///
/// | leg | size | lot | contracts | fee | price | fee amount |
/// |---|---|---|---|---|---|---|
/// | low call | 0.1 | 1 | 10 | 3 bps of 100, cap 12.5 % | buy 11.50 | 0.03 |
/// | low put | 0.5 | 2 | 2 | 10 bps of premium | sell 1.50 | 0.0015 |
/// | high call | 0.25 | 1 | 4 | 3 bps of 100, cap 1 % (binds) | sell 1.80 | 0.018 |
/// | high put | 1 | 1 | 1 | 20 bps of premium | buy 11.30 | 0.0226 |
fn distinct_box() -> BoxInput {
    let mut i = long_box();
    i.low_put.contract_size = q(dec!(0.5));
    i.low_put.lot = q(dec!(2));
    i.low_put.fee = OptionFee::Premium {
        rate: FeeRate::from_bps(dec!(10)).unwrap(),
    };
    i.high_call.contract_size = q(dec!(0.25));
    i.high_call.fee = OptionFee::UnderlyingCapped {
        rate: FeeRate::from_bps(dec!(3)).unwrap(),
        cap: dec!(0.01),
    };
    i.high_put.contract_size = q(dec!(1));
    i.high_put.fee = OptionFee::Premium {
        rate: FeeRate::from_bps(dec!(20)).unwrap(),
    };
    i
}

#[test]
fn box_legs_keep_their_own_size_lot_and_fee() {
    let i = distinct_box();
    let ev = evaluate_box(&i).unwrap();
    let e = ev.long.edge().unwrap();
    // gross 0.30 as in the long fixture; fees
    //   low call  min(3e-4 * 100, 0.125 * 11.50) = 0.03
    //   low put   10e-4 * 1.50 = 0.0015
    //   high call min(0.03, 0.01 * 1.80 = 0.018) = 0.018
    //   high put  20e-4 * 11.30 = 0.0226
    //   total 0.0721; net 0.30 - 0.0721 = 0.2279
    let f = &e.expected_fees;
    assert_eq!(
        [f.low_call, f.low_put, f.high_call, f.high_put, f.total].map(|c| c.money().amount()),
        [
            dec!(0.03),
            dec!(0.0015),
            dec!(0.018),
            dec!(0.0226),
            dec!(0.0721)
        ]
    );
    assert_eq!(e.expected_net.amount(), dec!(0.2279));
    let l = &e.legs;
    assert_eq!(
        [
            l.low_call.quantity,
            l.low_put.quantity,
            l.high_call.quantity,
            l.high_put.quantity
        ],
        [q(dec!(10)), q(dec!(2)), q(dec!(4)), q(dec!(1))]
    );
    let plan = PackagePlan::from_box(
        &i,
        e,
        &VenueId::new("V1").unwrap(),
        None,
        FailurePolicy::Abort,
    )
    .unwrap();
    let terms = plan
        .legs()
        .iter()
        .map(|s| (s.spec.contract_size, s.spec.lot, s.spec.spec.leg.quantity))
        .collect::<Vec<_>>();
    assert_eq!(
        terms,
        vec![
            (q(dec!(0.1)), q(dec!(1)), q(dec!(10))),
            (q(dec!(0.5)), q(dec!(2)), q(dec!(2))),
            (q(dec!(0.25)), q(dec!(1)), q(dec!(4))),
            (q(dec!(1)), q(dec!(1)), q(dec!(1))),
        ]
    );
}

#[test]
fn box_fee_validation_covers_every_leg() {
    type Fee = fn(&mut BoxInput) -> &mut OptionFee;
    let legs: [Fee; 4] = [
        |i| &mut i.low_call.fee,
        |i| &mut i.low_put.fee,
        |i| &mut i.high_call.fee,
        |i| &mut i.high_put.fee,
    ];
    for fee in legs {
        let mut i = long_box();
        *fee(&mut i) = OptionFee::Premium {
            rate: FeeRate::from_bps(dec!(-1)).unwrap(),
        };
        assert!(matches!(
            evaluate_box(&i),
            Err(ArbitrageError::InvalidValue {
                field: "option fee rate (bps)",
                ..
            })
        ));
        let mut i = long_box();
        *fee(&mut i) = OptionFee::UnderlyingCapped {
            rate: FeeRate::from_bps(dec!(3)).unwrap(),
            cap: dec!(1.5),
        };
        assert!(matches!(
            evaluate_box(&i),
            Err(ArbitrageError::InvalidValue {
                field: "option fee cap",
                ..
            })
        ));
    }
}
