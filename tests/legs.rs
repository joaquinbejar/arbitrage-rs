//! Hand-checked fixtures for `legs::TwoLegPlan`.

use arbitrage_rs::legs::{
    ExecId, FailurePolicy, Fallback, LegExecution, LegIndex, LegSpec, LegStatus, PlanAction,
    PlanState, TwoLegPlan,
};
use arbitrage_rs::opportunity::{Evaluation, LegQuote, OpportunityInput, evaluate};
use arbitrage_rs::{
    ArbitrageError, Cost, Currency, Decimal, FeeRate, Instrument, InstrumentId, Leg, Level, Money,
    Price, Quantity, Side, Timestamp, TopOfBook,
};
use rust_decimal_macros::dec;

use LegIndex::{First as A, Second as B};

fn usd() -> Currency {
    Currency::new("USD").unwrap()
}
fn q(x: Decimal) -> Quantity {
    Quantity::new(x).unwrap()
}
fn p(x: Decimal) -> Price {
    Price::new(x).unwrap()
}
fn t(ms: i64) -> Timestamp {
    Timestamp::from_millis(ms)
}
fn fee(x: Decimal) -> Cost {
    Cost::new(Money::new(x, usd()))
}
fn inst(id: &str) -> Instrument {
    Instrument::undated(InstrumentId::new(id).unwrap())
}

fn spec(
    id: &str,
    side: Side,
    qty: Decimal,
    price: Decimal,
    deadline: Option<Timestamp>,
) -> LegSpec {
    LegSpec {
        leg: Leg {
            instrument: inst(id),
            side,
            quantity: q(qty),
            price: p(price),
        },
        deadline,
    }
}

/// Buy 10 X at 100 (first) and sell 10 Y at 100.5 (second), created at 0.
fn plan(policy: FailurePolicy, deadline: Option<Timestamp>) -> TwoLegPlan {
    TwoLegPlan::new(
        spec("X", Side::Buy, dec!(10), dec!(100), deadline),
        spec("Y", Side::Sell, dec!(10), dec!(100.5), deadline),
        policy,
        usd(),
        t(0),
    )
    .unwrap()
}

/// A fill of `qty` that brings the attempt's cumulative to `cum`.
#[allow(clippy::too_many_arguments)]
fn fill(
    leg: LegIndex,
    attempt: u32,
    id: &str,
    qty: Decimal,
    cum: Decimal,
    price: Decimal,
    f: Decimal,
    at: i64,
) -> LegExecution {
    LegExecution::Fill {
        leg,
        attempt,
        exec_id: ExecId(id.to_owned()),
        quantity: q(qty),
        cum_qty: q(cum),
        price: p(price),
        fee: fee(f),
        at: t(at),
    }
}

fn ack(leg: LegIndex, attempt: u32, cum: Decimal, at: i64) -> LegExecution {
    LegExecution::CancelAck {
        leg,
        attempt,
        cum_qty: cum,
        at: t(at),
    }
}

fn rejected(leg: LegIndex, attempt: u32, at: i64) -> LegExecution {
    LegExecution::Rejected {
        leg,
        attempt,
        at: t(at),
    }
}

fn unwind(id: &str, side: Side, qty: Decimal) -> PlanAction {
    PlanAction::Unwind {
        instrument: inst(id),
        side,
        quantity: q(qty),
    }
}

#[test]
fn both_legs_fill_and_the_plan_completes() {
    let mut plan = plan(FailurePolicy::Abort, None);
    let r = plan
        .apply(&fill(
            A,
            0,
            "a1",
            dec!(10),
            dec!(10),
            dec!(100),
            dec!(0.5),
            1,
        ))
        .unwrap();
    // One leg filled, the other not: the residual is visible.
    assert_eq!(r.residual, dec!(10));
    assert_eq!(r.state, PlanState::Working);
    plan.apply(&fill(
        B,
        0,
        "b1",
        dec!(4),
        dec!(4),
        dec!(100.5),
        dec!(0.201),
        2,
    ))
    .unwrap();
    let r = plan
        .apply(&fill(
            B,
            0,
            "b2",
            dec!(6),
            dec!(10),
            dec!(100.6),
            dec!(0.3018),
            3,
        ))
        .unwrap();
    assert_eq!(r.state, PlanState::Completed);
    assert_eq!(r.residual, dec!(0));
    assert!(r.actions.is_empty());
    // 4 * 100.5 + 6 * 100.6 = 402 + 603.6 = 1005.6.
    assert_eq!(plan.leg(B).filled_notional, Money::new(dec!(1005.6), usd()));
    assert_eq!(plan.leg(B).fees, fee(dec!(0.5028)));
    assert_eq!(plan.fees().unwrap(), fee(dec!(1.0028)));
    assert_eq!(plan.leg(A).status().unwrap(), LegStatus::Filled);
}

#[test]
fn filled_and_rejected_under_abort_keeps_the_residual_visible() {
    let mut plan = plan(FailurePolicy::Abort, None);
    plan.apply(&fill(
        A,
        0,
        "a1",
        dec!(10),
        dec!(10),
        dec!(100),
        dec!(0.5),
        1,
    ))
    .unwrap();
    let r = plan.apply(&rejected(B, 0, 2)).unwrap();
    assert_eq!(r.state, PlanState::Aborted);
    assert!(r.actions.is_empty());
    assert_eq!(r.residual, dec!(10));
    assert_eq!(plan.leg(B).status().unwrap(), LegStatus::Rejected);
}

#[test]
fn filled_and_rejected_under_unwind_requests_the_offset() {
    let mut plan = plan(FailurePolicy::Unwind, None);
    plan.apply(&fill(
        A,
        0,
        "a1",
        dec!(10),
        dec!(10),
        dec!(100),
        dec!(0.5),
        1,
    ))
    .unwrap();
    let r = plan.apply(&rejected(B, 0, 2)).unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert_eq!(r.actions, vec![unwind("X", Side::Sell, dec!(10))]);
    // The unwind is an intent: the residual stays visible.
    assert_eq!(r.residual, dec!(10));
}

#[test]
fn complete_policy_resubmits_then_falls_back() {
    let policy = FailurePolicy::Complete {
        max_resubmits: 1,
        then: Fallback::Unwind,
    };
    let mut plan = plan(policy, None);
    plan.apply(&fill(
        A,
        0,
        "a1",
        dec!(10),
        dec!(10),
        dec!(100),
        dec!(0.5),
        1,
    ))
    .unwrap();
    plan.apply(&fill(
        B,
        0,
        "b1",
        dec!(3),
        dec!(3),
        dec!(100.5),
        dec!(0.15),
        2,
    ))
    .unwrap();
    // B's attempt 0 is cancelled by the venue at a final 3: resubmit 7.
    let r = plan.apply(&ack(B, 0, dec!(3), 3)).unwrap();
    assert_eq!(r.state, PlanState::Working);
    assert_eq!(
        r.actions,
        vec![PlanAction::Resubmit {
            leg: B,
            attempt: 1,
            quantity: q(dec!(7)),
            price: p(dec!(100.5)),
        }]
    );
    assert_eq!(r.residual, dec!(7));
    // Attempt 1 fills 2 and is cancelled: 5 left and no resubmit left.
    plan.apply(&fill(
        B,
        1,
        "b2",
        dec!(2),
        dec!(2),
        dec!(100.5),
        dec!(0.1),
        4,
    ))
    .unwrap();
    let r = plan.apply(&ack(B, 1, dec!(2), 5)).unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert_eq!(r.actions, vec![unwind("X", Side::Sell, dec!(5))]);
    assert_eq!(plan.leg(B).resubmits(), 1);
    assert_eq!(plan.leg(B).executed().unwrap(), dec!(5));
}

#[test]
fn ack_reports_fills_in_flight_so_no_resubmit_overlaps_them() {
    // Codex probe: A fills 10; B0 fills 3; B0 is cancelled after 10 executed
    // at the venue, of which only 3 were observed. The ack's cum_qty of 10
    // completes B: no resubmit, and the late fill of 7 is consistent.
    let policy = FailurePolicy::Complete {
        max_resubmits: 3,
        then: Fallback::Abort,
    };
    let mut plan = plan(policy, None);
    plan.apply(&fill(
        A,
        0,
        "a1",
        dec!(10),
        dec!(10),
        dec!(100),
        dec!(0.5),
        1,
    ))
    .unwrap();
    plan.apply(&fill(
        B,
        0,
        "b1",
        dec!(3),
        dec!(3),
        dec!(100.5),
        dec!(0.15),
        2,
    ))
    .unwrap();
    let r = plan.apply(&ack(B, 0, dec!(10), 3)).unwrap();
    assert!(r.actions.is_empty());
    assert_eq!(r.state, PlanState::Completed);
    assert_eq!(r.residual, dec!(0));
    // Fees so far come from observed fills only.
    assert_eq!(plan.leg(B).fees, fee(dec!(0.15)));
    // The late fill, executed before the ack, is applied with its own time.
    let r = plan
        .apply(&fill(
            B,
            0,
            "b2",
            dec!(7),
            dec!(10),
            dec!(100.5),
            dec!(0.35),
            2,
        ))
        .unwrap();
    assert!(r.actions.is_empty());
    assert_eq!(r.state, PlanState::Completed);
    assert_eq!(plan.leg(B).fees, fee(dec!(0.5)));
    assert_eq!(plan.leg(B).attempts.len(), 1);
    // A fill beyond the acknowledged final quantity is impossible.
    let before = plan.clone();
    assert!(matches!(
        plan.apply(&fill(
            B,
            0,
            "b3",
            dec!(1),
            dec!(11),
            dec!(100.5),
            dec!(0.05),
            4
        )),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    assert_eq!(plan, before);
}

#[test]
fn partial_fill_in_flight_shrinks_the_resubmission() {
    // Ack reports 5 executed while only 3 were observed: resubmit 5, not 7.
    // The in-flight fill of 2 then arrives and changes nothing but fees.
    let policy = FailurePolicy::Complete {
        max_resubmits: 3,
        then: Fallback::Abort,
    };
    let mut plan = plan(policy, None);
    plan.apply(&fill(
        A,
        0,
        "a1",
        dec!(10),
        dec!(10),
        dec!(100),
        dec!(0.5),
        1,
    ))
    .unwrap();
    plan.apply(&fill(
        B,
        0,
        "b1",
        dec!(3),
        dec!(3),
        dec!(100.5),
        dec!(0.15),
        2,
    ))
    .unwrap();
    let r = plan.apply(&ack(B, 0, dec!(5), 3)).unwrap();
    assert_eq!(
        r.actions,
        vec![PlanAction::Resubmit {
            leg: B,
            attempt: 1,
            quantity: q(dec!(5)),
            price: p(dec!(100.5)),
        }]
    );
    assert_eq!(r.residual, dec!(5));
    let r = plan
        .apply(&fill(
            B,
            0,
            "b2",
            dec!(2),
            dec!(5),
            dec!(100.5),
            dec!(0.1),
            2,
        ))
        .unwrap();
    assert!(r.actions.is_empty());
    assert_eq!(r.residual, dec!(5));
    // Attempt 1 fills its 5 and the plan completes; a further real fill on
    // attempt 1 would exceed its quantity and is rejected, not hidden.
    let r = plan
        .apply(&fill(
            B,
            1,
            "b3",
            dec!(5),
            dec!(5),
            dec!(100.5),
            dec!(0.25),
            4,
        ))
        .unwrap();
    assert_eq!(r.state, PlanState::Completed);
    assert_eq!(r.residual, dec!(0));
    assert!(matches!(
        plan.apply(&fill(
            B,
            1,
            "b4",
            dec!(1),
            dec!(6),
            dec!(100.5),
            dec!(0.05),
            5
        )),
        Err(ArbitrageError::InvalidInput { .. })
    ));
}

#[test]
fn unwind_waits_for_the_other_cancel_and_uses_the_ack_quantity() {
    let mut plan = plan(FailurePolicy::Unwind, None);
    plan.apply(&fill(A, 0, "a1", dec!(6), dec!(6), dec!(100), dec!(0.3), 1))
        .unwrap();
    // B fails while A is still working: cancel A first.
    let r = plan.apply(&rejected(B, 0, 2)).unwrap();
    assert_eq!(r.state, PlanState::Unwinding);
    assert_eq!(r.actions, vec![PlanAction::Cancel { leg: A, attempt: 0 }]);
    assert_eq!(
        plan.leg(A).status().unwrap(),
        LegStatus::CancelRequested { attempt: 0 }
    );
    // A fill that raced the cancel still counts.
    let r = plan
        .apply(&fill(A, 0, "a2", dec!(2), dec!(8), dec!(100), dec!(0.1), 3))
        .unwrap();
    assert!(r.actions.is_empty());
    assert_eq!(r.residual, dec!(8));
    // The ack reports 9 executed: unwind 9 at once.
    let r = plan.apply(&ack(A, 0, dec!(9), 4)).unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert_eq!(r.actions, vec![unwind("X", Side::Sell, dec!(9))]);
    assert_eq!(r.residual, dec!(9));
    // The in-flight fill of 1 arrives after the ack: no new unwind.
    let r = plan
        .apply(&fill(
            A,
            0,
            "a3",
            dec!(1),
            dec!(9),
            dec!(100),
            dec!(0.05),
            3,
        ))
        .unwrap();
    assert!(r.actions.is_empty());
    assert_eq!(r.residual, dec!(9));
    assert_eq!(plan.fees().unwrap(), fee(dec!(0.45)));
}

#[test]
fn deadline_requests_a_cancel_and_waits_for_the_ack() {
    let policy = FailurePolicy::Complete {
        max_resubmits: 3,
        then: Fallback::Abort,
    };
    let mut plan = plan(policy, Some(t(1_000)));
    plan.apply(&fill(
        A,
        0,
        "a1",
        dec!(10),
        dec!(10),
        dec!(100),
        dec!(0.5),
        500,
    ))
    .unwrap();
    let r = plan.apply(&LegExecution::Tick { at: t(999) }).unwrap();
    assert!(r.actions.is_empty());
    let r = plan.apply(&LegExecution::Tick { at: t(1_000) }).unwrap();
    assert_eq!(r.actions, vec![PlanAction::Cancel { leg: B, attempt: 0 }]);
    // Asked once only.
    let r = plan.apply(&LegExecution::Tick { at: t(1_050) }).unwrap();
    assert!(r.actions.is_empty());
    assert_eq!(r.state, PlanState::Working);
    // The ack makes B a failure; past its deadline it is not resubmitted.
    let r = plan.apply(&ack(B, 0, dec!(0), 1_100)).unwrap();
    assert!(r.actions.is_empty());
    assert_eq!(r.state, PlanState::Aborted);
    assert_eq!(r.residual, dec!(10));
}

#[test]
fn a_dated_leg_expiring_while_working_is_cancelled_and_not_resubmitted() {
    // The sell leg is a future expiring at 2_000; the plan deadline is
    // later (5_000), so the expiry is the effective deadline.
    let first = spec("X", Side::Buy, dec!(10), dec!(100), Some(t(5_000)));
    let mut second = spec("Y-FUT", Side::Sell, dec!(10), dec!(100.5), Some(t(5_000)));
    second.leg.instrument = Instrument::dated(InstrumentId::new("Y-FUT").unwrap(), t(2_000));
    assert_eq!(second.effective_deadline(), Some(t(2_000)));
    let policy = FailurePolicy::Complete {
        max_resubmits: 2,
        then: Fallback::Unwind,
    };
    let mut plan = TwoLegPlan::new(first.clone(), second.clone(), policy, usd(), t(0)).unwrap();
    plan.apply(&fill(
        A,
        0,
        "a1",
        dec!(10),
        dec!(10),
        dec!(100),
        dec!(0.5),
        100,
    ))
    .unwrap();
    plan.apply(&fill(
        B,
        0,
        "b1",
        dec!(4),
        dec!(4),
        dec!(100.5),
        dec!(0.2),
        200,
    ))
    .unwrap();
    let r = plan.apply(&LegExecution::Tick { at: t(2_000) }).unwrap();
    assert_eq!(r.actions, vec![PlanAction::Cancel { leg: B, attempt: 0 }]);
    // After the ack the expired leg is not resubmitted: fall back to unwind.
    let r = plan.apply(&ack(B, 0, dec!(4), 2_100)).unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert_eq!(r.actions, vec![unwind("X", Side::Sell, dec!(6))]);
    assert_eq!(plan.leg(B).resubmits(), 0);

    // A plan created at or after the expiry is rejected.
    assert!(matches!(
        TwoLegPlan::new(first, second, policy, usd(), t(2_000)),
        Err(ArbitrageError::Expired { .. })
    ));
}

#[test]
fn repeated_events_do_not_double_apply() {
    let mut plan = plan(FailurePolicy::Abort, None);
    let f = fill(A, 0, "a1", dec!(4), dec!(4), dec!(100), dec!(0.2), 1);
    plan.apply(&f).unwrap();
    let r = plan.apply(&f).unwrap();
    assert!(r.duplicate);
    assert!(r.actions.is_empty());
    assert_eq!(plan.leg(A).executed().unwrap(), dec!(4));
    assert_eq!(plan.leg(A).fees, fee(dec!(0.2)));

    let rej = rejected(B, 0, 2);
    let first = plan.apply(&rej).unwrap();
    assert_eq!(
        first.actions,
        vec![PlanAction::Cancel { leg: A, attempt: 0 }]
    );
    let again = plan.apply(&rej).unwrap();
    assert!(again.duplicate && again.actions.is_empty());

    let a = ack(A, 0, dec!(4), 3);
    assert_eq!(plan.apply(&a).unwrap().state, PlanState::Aborted);
    assert!(plan.apply(&a).unwrap().duplicate);
    // The same ack with another cum_qty is not a duplicate but an error.
    assert!(matches!(
        plan.apply(&ack(A, 0, dec!(5), 4)),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    assert_eq!(plan.residual().unwrap(), dec!(4));
}

#[test]
fn invalid_events_leave_the_plan_unchanged() {
    let mut plan = plan(FailurePolicy::Abort, None);
    plan.apply(&fill(
        A,
        0,
        "a1",
        dec!(4),
        dec!(4),
        dec!(100),
        dec!(0.2),
        10,
    ))
    .unwrap();
    let before = plan.clone();

    let cases = [
        // Overfill: cum 11 > 10.
        fill(A, 0, "a2", dec!(7), dec!(11), dec!(100), dec!(0.2), 11),
        // cum_qty not previous + fill.
        fill(A, 0, "a3", dec!(1), dec!(4), dec!(100), dec!(0.2), 11),
        // Attempt never sent.
        fill(B, 1, "b1", dec!(1), dec!(1), dec!(100.5), dec!(0.2), 11),
        // Execution id reused with different content.
        fill(A, 0, "a1", dec!(5), dec!(5), dec!(100), dec!(0.2), 11),
        // Rejection of an attempt with fills.
        rejected(A, 0, 11),
        // Ack below the fills already applied, or above the attempt.
        ack(A, 0, dec!(3), 11),
        ack(A, 0, dec!(11), 11),
        // A non-fill event older than the last applied one.
        LegExecution::Tick { at: t(9) },
    ];
    for ev in &cases {
        assert!(
            matches!(plan.apply(ev), Err(ArbitrageError::InvalidInput { .. })),
            "{ev:?}"
        );
        assert_eq!(plan, before, "{ev:?}");
    }
    let eur_fee = LegExecution::Fill {
        leg: B,
        attempt: 0,
        exec_id: ExecId("b2".to_owned()),
        quantity: q(dec!(1)),
        cum_qty: q(dec!(1)),
        price: p(dec!(100.5)),
        fee: Cost::new(Money::new(dec!(0.1), Currency::new("EUR").unwrap())),
        at: t(11),
    };
    assert!(matches!(
        plan.apply(&eur_fee),
        Err(ArbitrageError::CurrencyMismatch { .. })
    ));
    assert_eq!(plan, before);

    // A fill for a rejected attempt, and a second close of an attempt.
    plan.apply(&rejected(B, 0, 12)).unwrap();
    assert!(matches!(
        plan.apply(&fill(
            B,
            0,
            "b3",
            dec!(1),
            dec!(1),
            dec!(100.5),
            dec!(0.1),
            13
        )),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    assert!(matches!(
        plan.apply(&ack(B, 0, dec!(0), 13)),
        Err(ArbitrageError::InvalidInput { .. })
    ));
}

#[test]
fn an_error_after_the_state_change_leaves_the_plan_unchanged() {
    // Codex M311A-3: the plan must not commit before the whole step and its
    // report succeed. Legs of MAX: A executes MAX, then B executes 0.1, so
    // B's remaining and the residual MAX - 0.1 need 29 digits and are not
    // representable. The fill is valid on its own; the step fails after it.
    let big = Decimal::MAX;
    let mut plan = TwoLegPlan::new(
        spec("X", Side::Buy, big, dec!(1), None),
        spec("Y", Side::Sell, big, dec!(1), None),
        FailurePolicy::Abort,
        usd(),
        t(0),
    )
    .unwrap();
    plan.apply(&fill(A, 0, "a1", big, big, dec!(1), dec!(0), 1))
        .unwrap();
    let before = plan.clone();
    assert!(matches!(
        plan.apply(&fill(B, 0, "b1", dec!(0.1), dec!(0.1), dec!(1), dec!(0), 2)),
        Err(ArbitrageError::Inexact { .. })
    ));
    assert_eq!(plan, before);
    assert_eq!(plan.leg(B).executed().unwrap(), dec!(0));
}

#[test]
fn construction_is_validated() {
    let new = |a, b| TwoLegPlan::new(a, b, FailurePolicy::Abort, usd(), t(0));
    let leg = |id: &str, side, qty| spec(id, side, qty, dec!(1), None);
    assert!(new(leg("X", Side::Buy, dec!(1)), leg("X", Side::Sell, dec!(1))).is_err());
    assert!(new(leg("X", Side::Buy, dec!(1)), leg("Y", Side::Buy, dec!(1))).is_err());
    // Codex probe: buy 10 / sell 5 cannot offset one to one.
    assert!(matches!(
        new(leg("X", Side::Buy, dec!(10)), leg("Y", Side::Sell, dec!(5))),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    let mut late = leg("Y", Side::Sell, dec!(1));
    late.deadline = Some(t(0));
    assert!(new(leg("X", Side::Buy, dec!(1)), late).is_err());
    assert!(new(leg("X", Side::Buy, dec!(1)), leg("Y", Side::Sell, dec!(1))).is_ok());
}

#[test]
fn both_legs_failing_flat_needs_no_unwind() {
    let mut plan = plan(FailurePolicy::Unwind, None);
    let r = plan.apply(&rejected(A, 0, 1)).unwrap();
    assert_eq!(r.actions, vec![PlanAction::Cancel { leg: B, attempt: 0 }]);
    let r = plan.apply(&ack(B, 0, dec!(0), 2)).unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert!(r.actions.is_empty());
    assert_eq!(r.residual, dec!(0));
}

#[test]
fn plan_from_an_accepted_opportunity() {
    let lvl = |px, s| Some(Level::new(p(px), q(s)));
    let input = OpportunityInput {
        as_of: t(100),
        quote_currency: usd(),
        buy: LegQuote {
            instrument: inst("X"),
            book: TopOfBook {
                bid: None,
                ask: lvl(dec!(100), dec!(5)),
            },
            fee: FeeRate::zero(),
        },
        sell: LegQuote {
            instrument: inst("Y"),
            book: TopOfBook {
                bid: lvl(dec!(101), dec!(3)),
                ask: None,
            },
            fee: FeeRate::zero(),
        },
        target_quantity: q(dec!(4)),
        min_quantity: q(dec!(1)),
        carry: vec![],
        min_net_edge: Money::zero(usd()),
    };
    let Evaluation::Accepted(o) = evaluate(&input).unwrap() else {
        panic!("expected an accepted opportunity");
    };
    let plan =
        TwoLegPlan::from_opportunity(&o, Some(t(200)), FailurePolicy::Unwind, usd()).unwrap();
    assert_eq!(plan.leg(A).spec.leg.side, Side::Buy);
    assert_eq!(plan.leg(B).spec.leg.side, Side::Sell);
    assert_eq!(plan.leg(A).spec.leg.quantity, q(dec!(3)));
    assert_eq!(plan.leg(B).spec.deadline, Some(t(200)));
    assert_eq!(plan.leg(A).remaining().unwrap(), dec!(3));
    assert_eq!(plan.state(), PlanState::Working);
}
