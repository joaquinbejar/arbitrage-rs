//! Multi-leg package plan (#19).
//!
//! Fixture P: a reversal of one package of 1 XYZ at K = 100 from the
//! parity fixture E of #18 (buy 10 calls, sell 10 puts,
//! sell 100 futures; options of 0.1 XYZ, futures of 0.01 XYZ, lots of one
//! contract, so the grid is 0.1 XYZ).

use arbitrage_rs::legs::{ExecId, FailurePolicy, Fallback, LegSpec, PlanState};
use arbitrage_rs::package::{
    CALL_LEG, FUTURE_LEG, PUT_LEG, PackageAction, PackageEvent, PackageLegSpec, PackagePlan,
    PackageReport,
};
use arbitrage_rs::parity::{OptionFee, ParityFuture, ParityInput, ParityOption, evaluate_parity};
use arbitrage_rs::{
    ArbitrageError, Cost, Currency, Decimal, FeeRate, Instrument, InstrumentId, Leg, Level, Money,
    Price, Quantity, Rounding, Side, Timestamp, TopOfBook,
};
use rust_decimal_macros::dec;

const EXPIRY: i64 = 1_000_000;
const DEADLINE: i64 = 5_000;

fn usdc() -> Currency {
    Currency::new("USDC").unwrap()
}

fn q(x: Decimal) -> Quantity {
    Quantity::new(x).unwrap()
}

fn px(x: Decimal) -> Price {
    Price::new(x).unwrap()
}

fn ts(ms: i64) -> Timestamp {
    Timestamp::from_millis(ms)
}

fn book(bid: Decimal, ask: Decimal, size: Decimal) -> TopOfBook {
    TopOfBook {
        bid: Some(Level::new(px(bid), q(size))),
        ask: Some(Level::new(px(ask), q(size))),
    }
}

fn dated(id: &str) -> Instrument {
    Instrument::dated(InstrumentId::new(id).unwrap(), ts(EXPIRY))
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

fn parity_input() -> ParityInput {
    ParityInput {
        as_of: ts(0),
        quote_currency: usdc(),
        strike: px(dec!(100)),
        discount: dec!(0.99),
        index: px(dec!(101.4037)),
        call: option("XYZ-100-C", dec!(6.00), dec!(6.10)),
        put: option("XYZ-100-P", dec!(4.90), dec!(5.00)),
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
        edge_dp: 6,
        edge_rounding: Rounding::HalfEven,
    }
}

/// The reversal of fixture E at K = 100 as a plan.
fn reversal(policy: FailurePolicy) -> PackagePlan {
    let input = parity_input();
    let ev = evaluate_parity(&input).unwrap();
    let edge = ev.reversal.edge().unwrap();
    PackagePlan::from_parity(&input, edge, Some(ts(DEADLINE)), policy).unwrap()
}

fn fill(leg: usize, attempt: u32, id: &str, qty: Decimal, cum: Decimal, at: i64) -> PackageEvent {
    let price = match leg {
        CALL_LEG => dec!(6.10),
        PUT_LEG => dec!(4.90),
        _ => dec!(101.50),
    };
    PackageEvent::Fill {
        leg,
        attempt,
        exec_id: ExecId(id.to_owned()),
        quantity: q(qty),
        cum_qty: q(cum),
        price: px(price),
        fee: Cost::new(Money::zero(usdc())),
        at: ts(at),
    }
}

fn ack(leg: usize, attempt: u32, cum: Decimal, at: i64) -> PackageEvent {
    PackageEvent::CancelAck {
        leg,
        attempt,
        cum_qty: cum,
        at: ts(at),
    }
}

fn unwind(leg: usize, side: Side, qty: Decimal) -> PackageAction {
    let id = match leg {
        CALL_LEG => "XYZ-100-C",
        PUT_LEG => "XYZ-100-P",
        _ => "XYZ-F",
    };
    PackageAction::Unwind {
        leg,
        instrument: dated(id),
        side,
        quantity: q(qty),
    }
}

/// Call 10 and future 100 filled, then the put fills 6 and its rest is
/// cancelled: the report of the cancel acknowledgement.
fn put_short_by_four(plan: &mut PackagePlan) -> PackageReport {
    plan.apply(&fill(CALL_LEG, 0, "c1", dec!(10), dec!(10), 10))
        .unwrap();
    plan.apply(&fill(FUTURE_LEG, 0, "f1", dec!(100), dec!(100), 11))
        .unwrap();
    plan.apply(&fill(PUT_LEG, 0, "p1", dec!(6), dec!(6), 12))
        .unwrap();
    plan.apply(&ack(PUT_LEG, 0, dec!(6), 13)).unwrap()
}

#[test]
fn grid_and_legs_from_the_parity_edge() {
    let plan = reversal(FailurePolicy::Abort);
    assert_eq!(plan.grid().value(), dec!(0.1));
    assert_eq!(plan.package().value(), dec!(1));
    let legs = plan.legs();
    assert_eq!(legs[CALL_LEG].spec.spec.leg.side, Side::Buy);
    assert_eq!(legs[PUT_LEG].spec.spec.leg.side, Side::Sell);
    assert_eq!(legs[FUTURE_LEG].spec.spec.leg.side, Side::Sell);
    assert_eq!(legs[FUTURE_LEG].spec.spec.leg.quantity.value(), dec!(100));
}

#[test]
fn p1_complete() {
    let mut plan = reversal(FailurePolicy::Unwind);
    plan.apply(&fill(CALL_LEG, 0, "c1", dec!(10), dec!(10), 10))
        .unwrap();
    plan.apply(&fill(PUT_LEG, 0, "p1", dec!(10), dec!(10), 11))
        .unwrap();
    let r = plan
        .apply(&fill(FUTURE_LEG, 0, "f1", dec!(100), dec!(100), 12))
        .unwrap();
    assert_eq!(r.state, PlanState::Completed);
    assert_eq!(r.matched, dec!(1));
    assert_eq!(r.excess, vec![dec!(0), dec!(0), dec!(0)]);
    assert!(r.actions.is_empty());
    // Cash with the multipliers: buy 10 * 0.1 * 6.10, sell 10 * 0.1 * 4.90
    // and 100 * 0.01 * 101.50.
    assert_eq!(plan.net_cash().unwrap().amount(), dec!(100.30));
}

#[test]
fn p2_unwind() {
    let mut plan = reversal(FailurePolicy::Unwind);
    let r = put_short_by_four(&mut plan);
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert_eq!(r.matched, dec!(0.6));
    assert_eq!(r.excess, vec![dec!(4), dec!(0), dec!(40)]);
    assert_eq!(
        r.actions,
        vec![
            unwind(CALL_LEG, Side::Sell, dec!(4)),
            unwind(FUTURE_LEG, Side::Buy, dec!(40)),
        ]
    );
}

#[test]
fn p2_complete_resubmits_until_the_deadline_then_falls_back() {
    let mut plan = reversal(FailurePolicy::Complete {
        max_resubmits: 3,
        then: Fallback::Unwind,
    });
    let r = put_short_by_four(&mut plan);
    assert_eq!(r.state, PlanState::Working);
    assert_eq!(
        r.actions,
        vec![PackageAction::Resubmit {
            leg: PUT_LEG,
            attempt: 1,
            quantity: q(dec!(4)),
            price: px(dec!(4.90)),
        }]
    );

    let r = plan
        .apply(&PackageEvent::Tick { at: ts(DEADLINE) })
        .unwrap();
    assert_eq!(
        r.actions,
        vec![PackageAction::Cancel {
            leg: PUT_LEG,
            attempt: 1
        }]
    );
    // Acknowledged at the deadline: no further resubmission, the fallback
    // acts on the excess.
    let r = plan.apply(&ack(PUT_LEG, 1, dec!(0), DEADLINE)).unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert_eq!(
        r.actions,
        vec![
            unwind(CALL_LEG, Side::Sell, dec!(4)),
            unwind(FUTURE_LEG, Side::Buy, dec!(40)),
        ]
    );
}

#[test]
fn p2_complete_fills_the_rest() {
    let mut plan = reversal(FailurePolicy::Complete {
        max_resubmits: 1,
        then: Fallback::Unwind,
    });
    put_short_by_four(&mut plan);
    let r = plan
        .apply(&fill(PUT_LEG, 1, "p2", dec!(4), dec!(4), 20))
        .unwrap();
    assert_eq!(r.state, PlanState::Completed);
    assert_eq!(r.matched, dec!(1));
}

#[test]
fn p2_abort_keeps_the_residual() {
    let mut plan = reversal(FailurePolicy::Abort);
    let r = put_short_by_four(&mut plan);
    assert_eq!(r.state, PlanState::Aborted);
    assert!(r.actions.is_empty());
    assert_eq!(r.matched, dec!(0.6));
    assert_eq!(r.excess, vec![dec!(4), dec!(0), dec!(40)]);
}

#[test]
fn p3_overfill_is_recorded_and_unwound() {
    let mut plan = reversal(FailurePolicy::Unwind);
    plan.apply(&fill(CALL_LEG, 0, "c1", dec!(10), dec!(10), 10))
        .unwrap();
    plan.apply(&fill(FUTURE_LEG, 0, "f1", dec!(101), dec!(101), 11))
        .unwrap();
    assert_eq!(plan.legs()[FUTURE_LEG].overfill().unwrap(), dec!(1));
    plan.apply(&fill(PUT_LEG, 0, "p1", dec!(6), dec!(6), 12))
        .unwrap();
    let r = plan.apply(&ack(PUT_LEG, 0, dec!(6), 13)).unwrap();
    assert_eq!(r.matched, dec!(0.6));
    assert_eq!(r.excess, vec![dec!(4), dec!(0), dec!(41)]);
    assert_eq!(
        r.actions,
        vec![
            unwind(CALL_LEG, Side::Sell, dec!(4)),
            unwind(FUTURE_LEG, Side::Buy, dec!(41)),
        ]
    );
}

#[test]
fn overfill_with_every_leg_filled_unwinds_the_excess_only() {
    let mut plan = reversal(FailurePolicy::Unwind);
    plan.apply(&fill(CALL_LEG, 0, "c1", dec!(10), dec!(10), 10))
        .unwrap();
    plan.apply(&fill(PUT_LEG, 0, "p1", dec!(10), dec!(10), 11))
        .unwrap();
    let r = plan
        .apply(&fill(FUTURE_LEG, 0, "f1", dec!(101), dec!(101), 12))
        .unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert_eq!(r.matched, dec!(1));
    assert_eq!(r.actions, vec![unwind(FUTURE_LEG, Side::Buy, dec!(1))]);
}

#[test]
fn p4_nothing_filled_by_the_deadline() {
    let mut plan = reversal(FailurePolicy::Unwind);
    let r = plan
        .apply(&PackageEvent::Tick { at: ts(DEADLINE) })
        .unwrap();
    assert_eq!(
        r.actions,
        vec![
            PackageAction::Cancel { leg: 0, attempt: 0 },
            PackageAction::Cancel { leg: 1, attempt: 0 },
            PackageAction::Cancel { leg: 2, attempt: 0 },
        ]
    );
    plan.apply(&ack(CALL_LEG, 0, dec!(0), DEADLINE)).unwrap();
    plan.apply(&ack(PUT_LEG, 0, dec!(0), DEADLINE)).unwrap();
    let r = plan.apply(&ack(FUTURE_LEG, 0, dec!(0), DEADLINE)).unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert!(r.actions.is_empty());
    assert_eq!(r.matched, dec!(0));
    assert_eq!(r.excess, vec![dec!(0), dec!(0), dec!(0)]);
}

#[test]
fn p5_matched_is_rounded_down_to_the_grid() {
    let mut plan = reversal(FailurePolicy::Unwind);
    plan.apply(&fill(CALL_LEG, 0, "c1", dec!(10), dec!(10), 10))
        .unwrap();
    plan.apply(&fill(PUT_LEG, 0, "p1", dec!(10), dec!(10), 11))
        .unwrap();
    plan.apply(&fill(FUTURE_LEG, 0, "f1", dec!(55), dec!(55), 12))
        .unwrap();
    let r = plan.apply(&ack(FUTURE_LEG, 0, dec!(55), 13)).unwrap();
    // 0.55 XYZ rounds down to 0.5: never 5.5 option contracts.
    assert_eq!(r.matched, dec!(0.5));
    assert_eq!(r.excess, vec![dec!(5), dec!(5), dec!(5)]);
    assert_eq!(
        r.actions,
        vec![
            unwind(CALL_LEG, Side::Sell, dec!(5)),
            unwind(PUT_LEG, Side::Buy, dec!(5)),
            unwind(FUTURE_LEG, Side::Buy, dec!(5)),
        ]
    );
}

#[test]
fn late_fill_after_the_unwind_requests_the_difference() {
    let mut plan = reversal(FailurePolicy::Unwind);
    put_short_by_four(&mut plan);
    // Two more puts executed before the cancel but reported after it.
    let r = plan
        .apply(&fill(PUT_LEG, 0, "p2", dec!(2), dec!(8), 12))
        .unwrap();
    assert_eq!(r.matched, dec!(0.8));
    assert_eq!(r.excess, vec![dec!(2), dec!(0), dec!(20)]);
    assert_eq!(
        r.actions,
        vec![
            unwind(CALL_LEG, Side::Buy, dec!(2)),
            unwind(FUTURE_LEG, Side::Sell, dec!(20)),
        ]
    );
}

#[test]
fn events_are_idempotent_and_validated() {
    let mut plan = reversal(FailurePolicy::Abort);
    let e = fill(CALL_LEG, 0, "c1", dec!(4), dec!(4), 10);
    plan.apply(&e).unwrap();
    assert!(plan.apply(&e).unwrap().duplicate);
    assert_eq!(
        plan.apply(&fill(CALL_LEG, 0, "c1", dec!(5), dec!(5), 10)),
        Err(ArbitrageError::InvalidInput {
            reason: "execution id reused with different content"
        })
    );
    assert_eq!(
        plan.apply(&fill(CALL_LEG, 0, "c2", dec!(1), dec!(6), 10)),
        Err(ArbitrageError::InvalidInput {
            reason: "cum_qty is not the attempt's cumulative plus the fill"
        })
    );
    assert_eq!(
        plan.apply(&fill(CALL_LEG, 0, "c3", dec!(0.5), dec!(4.5), 10)),
        Err(ArbitrageError::InvalidInput {
            reason: "a fill is not a whole number of the leg's lots"
        })
    );
    assert_eq!(
        plan.apply(&fill(3, 0, "x", dec!(1), dec!(1), 10)),
        Err(ArbitrageError::InvalidInput {
            reason: "event for a leg that is not in the plan"
        })
    );
    assert_eq!(
        plan.apply(&fill(CALL_LEG, 1, "c4", dec!(1), dec!(1), 10)),
        Err(ArbitrageError::InvalidInput {
            reason: "event for an attempt that was never sent"
        })
    );
    // Errors leave the plan unchanged.
    assert_eq!(plan.legs()[CALL_LEG].executed().unwrap(), dec!(4));
}

fn manual_leg(id: &str, side: Side, qty: Decimal, size: Decimal) -> PackageLegSpec {
    PackageLegSpec {
        spec: LegSpec {
            leg: Leg {
                instrument: dated(id),
                side,
                quantity: q(qty),
                price: px(dec!(1)),
            },
            deadline: None,
            venue: None,
        },
        contract_size: q(size),
        lot: q(dec!(1)),
    }
}

#[test]
fn construction_is_validated() {
    let new =
        |legs, package| PackagePlan::new(legs, q(package), FailurePolicy::Abort, usdc(), ts(0));
    assert_eq!(
        new(
            vec![
                manual_leg("A", Side::Buy, dec!(1.5), dec!(0.1)),
                manual_leg("B", Side::Sell, dec!(15), dec!(0.01)),
            ],
            dec!(0.15)
        ),
        Err(ArbitrageError::InvalidInput {
            reason: "the package is not a multiple of the grid"
        })
    );
    assert_eq!(
        new(
            vec![
                manual_leg("A", Side::Buy, dec!(10), dec!(0.1)),
                manual_leg("B", Side::Sell, dec!(90), dec!(0.01)),
            ],
            dec!(1)
        ),
        Err(ArbitrageError::InvalidInput {
            reason: "a leg does not hold the package of underlying"
        })
    );
    assert_eq!(
        new(
            vec![manual_leg("A", Side::Buy, dec!(10), dec!(0.1))],
            dec!(1)
        ),
        Err(ArbitrageError::InvalidInput {
            reason: "a package needs at least two legs"
        })
    );
    assert_eq!(
        new(
            vec![
                manual_leg("A", Side::Buy, dec!(10), dec!(0.1)),
                manual_leg("A", Side::Sell, dec!(10), dec!(0.1)),
            ],
            dec!(1)
        ),
        Err(ArbitrageError::InvalidInput {
            reason: "two package legs are the same instrument"
        })
    );
    // A coarser grid: contracts of 0.3 and 0.02 give 0.3.
    let plan = new(
        vec![
            manual_leg("A", Side::Buy, dec!(2), dec!(0.3)),
            manual_leg("B", Side::Sell, dec!(30), dec!(0.02)),
        ],
        dec!(0.6),
    )
    .unwrap();
    assert_eq!(plan.grid().value(), dec!(0.3));
}

#[test]
fn net_cash_is_one_exact_sum() {
    // A running sum would pass -MAX - 1; the total, -1, fits.
    let max = Decimal::MAX;
    let mut plan = PackagePlan::new(
        vec![
            manual_leg("A", Side::Buy, dec!(1), dec!(1)),
            manual_leg("B", Side::Buy, dec!(1), dec!(1)),
            manual_leg("C", Side::Sell, dec!(1), dec!(1)),
        ],
        q(dec!(1)),
        FailurePolicy::Abort,
        usdc(),
        ts(0),
    )
    .unwrap();
    for (leg, price) in [(0, max), (1, dec!(1)), (2, max)] {
        plan.apply(&PackageEvent::Fill {
            leg,
            attempt: 0,
            exec_id: ExecId(format!("e{leg}")),
            quantity: q(dec!(1)),
            cum_qty: q(dec!(1)),
            price: px(price),
            fee: Cost::new(Money::zero(usdc())),
            at: ts(1),
        })
        .unwrap();
    }
    assert_eq!(plan.state(), PlanState::Completed);
    assert_eq!(plan.net_cash().unwrap().amount(), dec!(-1));
}

#[test]
fn overfill_on_every_leg_alike_is_excess() {
    // 20, 20, 200 on a package of 1 XYZ: the matched package is capped at 1
    // and the rest is excess, as in P3.
    for (policy, state) in [
        (FailurePolicy::Unwind, PlanState::UnwindRequested),
        (FailurePolicy::Abort, PlanState::Aborted),
    ] {
        let mut plan = reversal(policy);
        plan.apply(&fill(CALL_LEG, 0, "c1", dec!(20), dec!(20), 10))
            .unwrap();
        plan.apply(&fill(PUT_LEG, 0, "p1", dec!(20), dec!(20), 11))
            .unwrap();
        let r = plan
            .apply(&fill(FUTURE_LEG, 0, "f1", dec!(200), dec!(200), 12))
            .unwrap();
        assert_eq!(r.state, state);
        assert_eq!(r.matched, dec!(1));
        assert_eq!(r.excess, vec![dec!(10), dec!(10), dec!(100)]);
        if policy == FailurePolicy::Unwind {
            assert_eq!(
                r.actions,
                vec![
                    unwind(CALL_LEG, Side::Sell, dec!(10)),
                    unwind(PUT_LEG, Side::Buy, dec!(10)),
                    unwind(FUTURE_LEG, Side::Buy, dec!(100)),
                ]
            );
        } else {
            assert!(r.actions.is_empty());
        }
    }
}

fn lot_leg(id: &str, side: Side, qty: Decimal, size: Decimal, lot: Decimal) -> PackageLegSpec {
    let mut l = manual_leg(id, side, qty, size);
    l.lot = q(lot);
    l
}

#[test]
fn decimal_lots_are_supported() {
    // Spot-like legs of 1 unit per contract with a lot of 0.1.
    let mut plan = PackagePlan::new(
        vec![
            lot_leg("A", Side::Buy, dec!(0.1), dec!(1), dec!(0.1)),
            lot_leg("B", Side::Sell, dec!(0.1), dec!(1), dec!(0.1)),
        ],
        q(dec!(0.1)),
        FailurePolicy::Abort,
        usdc(),
        ts(0),
    )
    .unwrap();
    assert_eq!(plan.grid().value(), dec!(0.1));
    plan.apply(&PackageEvent::Fill {
        leg: 0,
        attempt: 0,
        exec_id: ExecId("a".to_owned()),
        quantity: q(dec!(0.1)),
        cum_qty: q(dec!(0.1)),
        price: px(dec!(1)),
        fee: Cost::new(Money::zero(usdc())),
        at: ts(1),
    })
    .unwrap();
    assert_eq!(plan.matched().unwrap(), dec!(0));
    assert_eq!(plan.excess().unwrap(), vec![dec!(0.1), dec!(0)]);

    // Mixed lots: A has 0.25 units per contract and a lot of 2 contracts
    // (0.5 units), B has 1 unit per contract and a lot of 0.2 (0.2 units).
    // The grid is 1 unit: 4 contracts of A (2 lots), 1 contract of B
    // (5 lots).
    let mut plan = PackagePlan::new(
        vec![
            lot_leg("A", Side::Buy, dec!(8), dec!(0.25), dec!(2)),
            lot_leg("B", Side::Sell, dec!(2), dec!(1), dec!(0.2)),
        ],
        q(dec!(2)),
        FailurePolicy::Unwind,
        usdc(),
        ts(0),
    )
    .unwrap();
    assert_eq!(plan.grid().value(), dec!(1));
    plan.apply(&PackageEvent::Fill {
        leg: 0,
        attempt: 0,
        exec_id: ExecId("a".to_owned()),
        quantity: q(dec!(8)),
        cum_qty: q(dec!(8)),
        price: px(dec!(1)),
        fee: Cost::new(Money::zero(usdc())),
        at: ts(1),
    })
    .unwrap();
    plan.apply(&PackageEvent::Fill {
        leg: 1,
        attempt: 0,
        exec_id: ExecId("b".to_owned()),
        quantity: q(dec!(1.4)),
        cum_qty: q(dec!(1.4)),
        price: px(dec!(1)),
        fee: Cost::new(Money::zero(usdc())),
        at: ts(2),
    })
    .unwrap();
    let r = plan.apply(&ack(1, 0, dec!(1.4), 3)).unwrap();
    // B holds 1.4 units: one grid matched; excess 4 contracts of A (1 unit)
    // and 0.4 of B.
    assert_eq!(r.matched, dec!(1));
    assert_eq!(r.excess, vec![dec!(4), dec!(0.4)]);
    assert_eq!(
        r.actions,
        vec![
            PackageAction::Unwind {
                leg: 0,
                instrument: dated("A"),
                side: Side::Sell,
                quantity: q(dec!(4)),
            },
            PackageAction::Unwind {
                leg: 1,
                instrument: dated("B"),
                side: Side::Buy,
                quantity: q(dec!(0.4)),
            },
        ]
    );
}
