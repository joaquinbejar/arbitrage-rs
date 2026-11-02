//! Hand-checked fixtures for `legs::TwoLegPlan`.

// Tests fail loudly: the library's no-panic lints do not apply here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use arbitrage_rs::legs::{
    ExecId, FailurePolicy, Fallback, LegExecution, LegIndex, LegSpec, LegStatus, PlanAction,
    PlanState, TwoLegPlan,
};
use arbitrage_rs::opportunity::{Evaluation, LegQuote, OpportunityInput, evaluate};
use arbitrage_rs::{
    Action, ArbitrageError, Cost, Currency, Decimal, FeeRate, Instrument, InstrumentId, Leg, Level,
    Money, Price, Quantity, Timestamp, TopOfBook,
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
    side: Action,
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
        venue: VenueId::new("VB").unwrap(),
    }
}

/// Buy 10 X at 100 (first) and sell 10 Y at 100.5 (second), created at 0.
fn plan(policy: FailurePolicy, deadline: Option<Timestamp>) -> TwoLegPlan {
    TwoLegPlan::new(
        spec("X", Action::Buy, dec!(10), dec!(100), deadline),
        spec("Y", Action::Sell, dec!(10), dec!(100.5), deadline),
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

fn unwind(id: &str, side: Action, qty: Decimal) -> PlanAction {
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
    assert_eq!(r.actions.len(), 0);
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
    assert_eq!(r.actions.len(), 0);
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
    assert_eq!(r.actions, vec![unwind("X", Action::Sell, dec!(10))]);
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
    assert_eq!(r.actions, vec![unwind("X", Action::Sell, dec!(5))]);
    assert_eq!(plan.leg(B).resubmits(), 1);
    assert_eq!(plan.leg(B).executed().unwrap(), dec!(5));
}

#[test]
fn ack_reports_fills_in_flight_so_no_resubmit_overlaps_them() {
    // A fills 10; B0 fills 3; B0 is cancelled after 10 executed
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
    assert_eq!(r.actions.len(), 0);
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
    assert_eq!(r.actions.len(), 0);
    assert_eq!(r.state, PlanState::Completed);
    assert_eq!(plan.leg(B).fees, fee(dec!(0.5)));
    assert_eq!(plan.leg(B).attempts.len(), 1);
    // A real fill beyond the acknowledged quantity is recorded, not
    // rejected: B is oversold by 1 and the fallback (abort) keeps it
    // visible.
    let r = plan
        .apply(&fill(
            B,
            0,
            "b3",
            dec!(1),
            dec!(11),
            dec!(100.5),
            dec!(0.05),
            4,
        ))
        .unwrap();
    assert_eq!(r.state, PlanState::Aborted);
    assert_eq!(r.residual, dec!(-1));
    assert_eq!(plan.leg(B).overfill().unwrap(), dec!(1));
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
    assert_eq!(r.actions.len(), 0);
    assert_eq!(r.residual, dec!(5));
    // Attempt 1 fills its 5 and the plan completes.
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
    // A further real fill on attempt 1 exceeds it: recorded as exposure,
    // and the fallback (abort) keeps the residual visible.
    let r = plan
        .apply(&fill(
            B,
            1,
            "b4",
            dec!(1),
            dec!(6),
            dec!(100.5),
            dec!(0.05),
            5,
        ))
        .unwrap();
    assert_eq!(r.state, PlanState::Aborted);
    assert_eq!(r.residual, dec!(-1));
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
    assert_eq!(r.actions.len(), 0);
    assert_eq!(r.residual, dec!(8));
    // The ack reports 9 executed: unwind 9 at once.
    let r = plan.apply(&ack(A, 0, dec!(9), 4)).unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert_eq!(r.actions, vec![unwind("X", Action::Sell, dec!(9))]);
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
    assert_eq!(r.actions.len(), 0);
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
    assert_eq!(r.actions.len(), 0);
    let r = plan.apply(&LegExecution::Tick { at: t(1_000) }).unwrap();
    assert_eq!(r.actions, vec![PlanAction::Cancel { leg: B, attempt: 0 }]);
    // Asked once only.
    let r = plan.apply(&LegExecution::Tick { at: t(1_050) }).unwrap();
    assert_eq!(r.actions.len(), 0);
    assert_eq!(r.state, PlanState::Working);
    // The ack makes B a failure; past its deadline it is not resubmitted.
    let r = plan.apply(&ack(B, 0, dec!(0), 1_100)).unwrap();
    assert_eq!(r.actions.len(), 0);
    assert_eq!(r.state, PlanState::Aborted);
    assert_eq!(r.residual, dec!(10));
}

#[test]
fn a_dated_leg_expiring_while_working_is_cancelled_and_not_resubmitted() {
    // The sell leg is a future expiring at 2_000; the plan deadline is
    // later (5_000), so the expiry is the effective deadline.
    let first = spec("X", Action::Buy, dec!(10), dec!(100), Some(t(5_000)));
    let mut second = spec("Y-FUT", Action::Sell, dec!(10), dec!(100.5), Some(t(5_000)));
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
    assert_eq!(r.actions, vec![unwind("X", Action::Sell, dec!(6))]);
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
    assert_eq!(r.actions.len(), 0);
    assert_eq!(plan.leg(A).executed().unwrap(), dec!(4));
    assert_eq!(plan.leg(A).fees, fee(dec!(0.2)));

    let rej = rejected(B, 0, 2);
    let first = plan.apply(&rej).unwrap();
    assert_eq!(
        first.actions,
        vec![PlanAction::Cancel { leg: A, attempt: 0 }]
    );
    let again = plan.apply(&rej).unwrap();
    assert!(again.duplicate);
    assert_eq!(again.actions.len(), 0);

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
        // cum_qty not previous + fill.
        fill(A, 0, "a3", dec!(1), dec!(4), dec!(100), dec!(0.2), 11),
        // Attempt never sent.
        fill(B, 1, "b1", dec!(1), dec!(1), dec!(100.5), dec!(0.2), 11),
        // Execution id reused with different content.
        fill(A, 0, "a1", dec!(5), dec!(5), dec!(100), dec!(0.2), 11),
        // Rejection of an attempt with fills.
        rejected(A, 0, 11),
        // Ack below the fills already applied, or negative.
        ack(A, 0, dec!(3), 11),
        ack(A, 0, dec!(-1), 11),
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
    // The plan must not commit before the whole step and its
    // report succeed. Legs of MAX: A executes MAX, then B executes 0.1, so
    // B's remaining and the residual MAX - 0.1 need 29 digits and are not
    // representable. The fill is valid on its own; the step fails after it.
    let big = Decimal::MAX;
    let mut plan = TwoLegPlan::new(
        spec("X", Action::Buy, big, dec!(1), None),
        spec("Y", Action::Sell, big, dec!(1), None),
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
    assert!(
        new(
            leg("X", Action::Buy, dec!(1)),
            leg("X", Action::Sell, dec!(1))
        )
        .is_err()
    );
    assert!(
        new(
            leg("X", Action::Buy, dec!(1)),
            leg("Y", Action::Buy, dec!(1))
        )
        .is_err()
    );
    // Buy 10 / sell 5 cannot offset one to one.
    assert!(matches!(
        new(
            leg("X", Action::Buy, dec!(10)),
            leg("Y", Action::Sell, dec!(5))
        ),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    let mut late = leg("Y", Action::Sell, dec!(1));
    late.deadline = Some(t(0));
    assert!(new(leg("X", Action::Buy, dec!(1)), late).is_err());
    assert!(
        new(
            leg("X", Action::Buy, dec!(1)),
            leg("Y", Action::Sell, dec!(1))
        )
        .is_ok()
    );
}

#[test]
fn both_legs_failing_flat_needs_no_unwind() {
    let mut plan = plan(FailurePolicy::Unwind, None);
    let r = plan.apply(&rejected(A, 0, 1)).unwrap();
    assert_eq!(r.actions, vec![PlanAction::Cancel { leg: B, attempt: 0 }]);
    let r = plan.apply(&ack(B, 0, dec!(0), 2)).unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert_eq!(r.actions.len(), 0);
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
    let plan = TwoLegPlan::from_opportunity(
        &o,
        VenueId::new("VA").unwrap(),
        VenueId::new("VB").unwrap(),
        Some(t(200)),
        FailurePolicy::Unwind,
        usd(),
    )
    .unwrap();
    assert_eq!(plan.leg(A).spec.venue.as_str(), "VA");
    assert_eq!(plan.leg(B).spec.venue.as_str(), "VB");
    assert_eq!(plan.leg(A).spec.leg.side, Action::Buy);
    assert_eq!(plan.leg(B).spec.leg.side, Action::Sell);
    assert_eq!(plan.leg(A).spec.leg.quantity, q(dec!(3)));
    assert_eq!(plan.leg(B).spec.deadline, Some(t(200)));
    assert_eq!(plan.leg(A).remaining().unwrap(), dec!(3));
    assert_eq!(plan.state(), PlanState::Working);
}

#[test]
fn a_late_fill_beyond_the_ack_cancels_the_surplus_retry() {
    // The ack under-reports (3) and the plan resubmits 7; then the late fill
    // of 7 on attempt 0 shows B was already complete. The live retry is
    // surplus: cancel it, and complete once it is acknowledged empty.
    let policy = FailurePolicy::Complete {
        max_resubmits: 3,
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
    let r = plan.apply(&ack(B, 0, dec!(3), 3)).unwrap();
    assert_eq!(
        r.actions,
        vec![PlanAction::Resubmit {
            leg: B,
            attempt: 1,
            quantity: q(dec!(7)),
            price: p(dec!(100.5)),
        }]
    );
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
    assert_eq!(r.actions, vec![PlanAction::Cancel { leg: B, attempt: 1 }]);
    assert_eq!(r.state, PlanState::Working);
    assert_eq!(r.residual, dec!(0));
    let r = plan.apply(&ack(B, 1, dec!(0), 4)).unwrap();
    assert_eq!(r.state, PlanState::Completed);
    assert_eq!(r.residual, dec!(0));
}

#[test]
fn a_retry_that_fills_before_its_cancel_is_an_overfill_offset_by_unwind() {
    // Same race, but the surplus retry fills 2 before its cancel: B is
    // oversold by 2. Under the unwind fallback the plan buys 2 of Y back.
    let policy = FailurePolicy::Complete {
        max_resubmits: 3,
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
    plan.apply(&ack(B, 0, dec!(3), 3)).unwrap();
    plan.apply(&fill(
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
    let r = plan
        .apply(&fill(
            B,
            1,
            "b3",
            dec!(2),
            dec!(2),
            dec!(100.5),
            dec!(0.1),
            4,
        ))
        .unwrap();
    assert_eq!(r.state, PlanState::Working);
    assert_eq!(r.residual, dec!(-2));
    let r = plan.apply(&ack(B, 1, dec!(2), 5)).unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert_eq!(r.actions, vec![unwind("Y", Action::Buy, dec!(2))]);
    assert_eq!(plan.leg(B).overfill().unwrap(), dec!(2));
}

#[test]
fn an_overfill_after_completion_is_recorded_and_unwound() {
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
    let r = plan
        .apply(&fill(
            B,
            0,
            "b1",
            dec!(10),
            dec!(10),
            dec!(100.5),
            dec!(0.5),
            2,
        ))
        .unwrap();
    assert_eq!(r.state, PlanState::Completed);
    let r = plan
        .apply(&fill(
            B,
            0,
            "b2",
            dec!(1),
            dec!(11),
            dec!(100.5),
            dec!(0.05),
            3,
        ))
        .unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert_eq!(r.actions, vec![unwind("Y", Action::Buy, dec!(1))]);
    assert_eq!(r.residual, dec!(-1));
}

#[test]
fn an_ack_reporting_more_than_the_attempt_is_recorded() {
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
    let r = plan.apply(&ack(B, 0, dec!(12), 2)).unwrap();
    assert_eq!(r.state, PlanState::Aborted);
    assert_eq!(r.residual, dec!(-2));
    assert_eq!(plan.leg(B).overfill().unwrap(), dec!(2));
}

#[test]
fn no_resubmit_at_or_after_the_deadline_without_a_tick() {
    let policy = FailurePolicy::Complete {
        max_resubmits: 2,
        then: Fallback::Abort,
    };
    // A CancelAck exactly at the deadline, no tick before it.
    let mut plan_a = plan(policy, Some(t(10)));
    let r = plan_a.apply(&ack(B, 0, dec!(0), 10)).unwrap();
    assert_eq!(r.actions, vec![PlanAction::Cancel { leg: A, attempt: 0 }]);
    assert_eq!(r.state, PlanState::Aborting);
    assert_eq!(plan_a.leg(B).resubmits(), 0);
    // A rejection after the deadline, no tick before it.
    let mut plan_b = plan(policy, Some(t(10)));
    let r = plan_b.apply(&rejected(B, 0, 11)).unwrap();
    assert!(
        !r.actions
            .iter()
            .any(|a| matches!(a, PlanAction::Resubmit { .. }))
    );
    assert_eq!(r.state, PlanState::Aborting);
    // Before the deadline the same rejection is resubmitted.
    let mut plan_c = plan(policy, Some(t(10)));
    let r = plan_c.apply(&rejected(B, 0, 9)).unwrap();
    assert_eq!(
        r.actions,
        vec![PlanAction::Resubmit {
            leg: B,
            attempt: 1,
            quantity: q(dec!(10)),
            price: p(dec!(100.5)),
        }]
    );
}

#[test]
fn no_resubmit_of_an_expired_instrument_without_a_tick() {
    let policy = FailurePolicy::Complete {
        max_resubmits: 2,
        then: Fallback::Unwind,
    };
    let first = spec("X", Action::Buy, dec!(10), dec!(100), None);
    let mut second = spec("Y-FUT", Action::Sell, dec!(10), dec!(100.5), None);
    second.leg.instrument = Instrument::dated(InstrumentId::new("Y-FUT").unwrap(), t(50));
    // Rejected after the expiry and acknowledged at the expiry: no resubmit.
    for ev in [rejected(B, 0, 60), ack(B, 0, dec!(0), 50)] {
        let mut plan = TwoLegPlan::new(first.clone(), second.clone(), policy, usd(), t(0)).unwrap();
        let r = plan.apply(&ev).unwrap();
        assert_eq!(
            r.actions,
            vec![PlanAction::Cancel { leg: A, attempt: 0 }],
            "{ev:?}"
        );
        assert_eq!(r.state, PlanState::Unwinding, "{ev:?}");
        assert_eq!(plan.leg(B).resubmits(), 0, "{ev:?}");
    }
}

#[test]
fn an_overfill_waits_for_the_other_leg_and_unwinds_only_the_excess() {
    // A is overfilled by 2 (12 of 10) while B is still working: B is not
    // cancelled and keeps filling to its 10; then only the excess of 2 is
    // unwound.
    let mut plan = plan(FailurePolicy::Unwind, None);
    let r = plan
        .apply(&fill(
            A,
            0,
            "a1",
            dec!(12),
            dec!(12),
            dec!(100),
            dec!(0.6),
            1,
        ))
        .unwrap();
    assert_eq!(r.actions.len(), 0);
    assert_eq!(r.state, PlanState::Working);
    assert_eq!(
        plan.leg(B).status().unwrap(),
        LegStatus::Working { attempt: 0 }
    );
    let r = plan
        .apply(&fill(
            B,
            0,
            "b1",
            dec!(4),
            dec!(4),
            dec!(100.5),
            dec!(0.2),
            2,
        ))
        .unwrap();
    assert_eq!(r.actions.len(), 0);
    assert_eq!(r.residual, dec!(8));
    let r = plan
        .apply(&fill(
            B,
            0,
            "b2",
            dec!(6),
            dec!(10),
            dec!(100.5),
            dec!(0.3),
            3,
        ))
        .unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert_eq!(r.actions, vec![unwind("X", Action::Sell, dec!(2))]);
    assert_eq!(r.residual, dec!(2));

    // Under abort the same excess is kept and visible.
    let mut plan = super_abort();
    plan.apply(&fill(
        A,
        0,
        "a1",
        dec!(12),
        dec!(12),
        dec!(100),
        dec!(0.6),
        1,
    ))
    .unwrap();
    let r = plan
        .apply(&fill(
            B,
            0,
            "b1",
            dec!(10),
            dec!(10),
            dec!(100.5),
            dec!(0.5),
            2,
        ))
        .unwrap();
    assert_eq!(r.state, PlanState::Aborted);
    assert_eq!(r.actions.len(), 0);
    assert_eq!(r.residual, dec!(2));
}

fn super_abort() -> TwoLegPlan {
    plan(FailurePolicy::Abort, None)
}

// ---- M3-11B: per-venue clocks, connectivity, gated recovery, net cash ----

use arbitrage_rs::venues::{VenueId, VenueView};

fn view_of(sym: &str, at: i64, bid: Option<(Decimal, Decimal)>) -> VenueView {
    VenueView {
        venue: VenueId::new("VB").unwrap(),
        instrument: inst(sym),
        observed_at: t(at),
        book: TopOfBook {
            bid: bid.map(|(px, s)| Level::new(p(px), q(s))),
            ask: None,
        },
    }
}

fn complete(then: Fallback) -> FailurePolicy {
    FailurePolicy::Complete {
        max_resubmits: 3,
        then,
    }
}

#[test]
fn each_venue_keeps_its_own_event_order() {
    // Venue B is delayed: its rejection stamped 50 arrives after A's cancel
    // acknowledgement stamped 100, and is still applied.
    let mut plan = plan(FailurePolicy::Abort, None);
    plan.apply(&fill(
        A,
        0,
        "a1",
        dec!(2),
        dec!(2),
        dec!(100),
        dec!(0.1),
        90,
    ))
    .unwrap();
    plan.apply(&ack(A, 0, dec!(2), 100)).unwrap();
    let r = plan.apply(&rejected(B, 0, 50)).unwrap();
    assert_eq!(r.state, PlanState::Aborted);
    assert_eq!(r.residual, dec!(2));
    // Within one leg, an older non-fill event is still rejected.
    let mut other = super_abort();
    other
        .apply(&LegExecution::Disconnected { leg: B, at: t(60) })
        .unwrap();
    assert!(matches!(
        other.apply(&rejected(B, 0, 59)),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    // A tick older than the plan clock is rejected.
    assert!(matches!(
        other.apply(&LegExecution::Tick { at: t(59) }),
        Err(ArbitrageError::InvalidInput { .. })
    ));
}

#[test]
fn a_disconnected_venue_holds_the_resubmission_until_it_reconnects() {
    let mut plan = plan(complete(Fallback::Abort), None);
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
    plan.apply(&LegExecution::Disconnected { leg: B, at: t(3) })
        .unwrap();
    let r = plan.apply(&ack(B, 0, dec!(3), 4)).unwrap();
    assert_eq!(r.actions.len(), 0);
    assert_eq!(r.state, PlanState::Working);
    assert!(plan.leg(B).pending_resubmit);
    // The residual and the cash flow of the asymmetric fills stay visible:
    // 3 * 100.5 - 10 * 100 - (0.5 + 0.15) = 301.5 - 1000 - 0.65.
    assert_eq!(r.residual, dec!(7));
    assert_eq!(plan.net_cash().unwrap(), Money::new(dec!(-699.15), usd()));
    let r = plan
        .apply(&LegExecution::Reconnected { leg: B, at: t(5) })
        .unwrap();
    assert_eq!(
        r.actions,
        vec![PlanAction::Resubmit {
            leg: B,
            attempt: 1,
            quantity: q(dec!(7)),
            price: p(dec!(100.5)),
        }]
    );
    assert!(!plan.leg(B).pending_resubmit);
}

#[test]
fn gated_resubmission_waits_for_fresh_liquidity() {
    // With gating, the resubmission of B (sell 10 at a limit of 100.5)
    // needs a view of Y at most 50 ms old with a bid >= 100.5 and size 10.
    let mut plan = plan(complete(Fallback::Unwind), Some(t(1_000))).with_venue_gating(50);
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
    // No view yet: held.
    let r = plan.apply(&rejected(B, 0, 10)).unwrap();
    assert_eq!(r.actions.len(), 0);
    assert!(plan.leg(B).pending_resubmit);
    // Bid below the limit: held.
    let r = plan
        .apply(&LegExecution::VenueUpdate {
            leg: B,
            view: view_of("Y", 20, Some((dec!(100.4), dec!(50)))),
        })
        .unwrap();
    assert_eq!(r.actions.len(), 0);
    // Bid at the limit but only 5: held.
    let r = plan
        .apply(&LegExecution::VenueUpdate {
            leg: B,
            view: view_of("Y", 30, Some((dec!(100.5), dec!(5)))),
        })
        .unwrap();
    assert_eq!(r.actions.len(), 0);
    // Enough size, but the plan clock (a tick at 100) makes it stale.
    plan.apply(&LegExecution::VenueUpdate {
        leg: B,
        view: view_of("Y", 40, None),
    })
    .unwrap();
    let r = plan.apply(&LegExecution::Tick { at: t(100) }).unwrap();
    assert_eq!(r.actions.len(), 0);
    // A fresh, deep view admits it: liquidity recovered.
    let r = plan
        .apply(&LegExecution::VenueUpdate {
            leg: B,
            view: view_of("Y", 120, Some((dec!(100.6), dec!(10)))),
        })
        .unwrap();
    assert_eq!(
        r.actions,
        vec![PlanAction::Resubmit {
            leg: B,
            attempt: 1,
            quantity: q(dec!(10)),
            price: p(dec!(100.5)),
        }]
    );
    // A view of another instrument is an error.
    assert!(matches!(
        plan.apply(&LegExecution::VenueUpdate {
            leg: B,
            view: view_of("X", 130, None),
        }),
        Err(ArbitrageError::InvalidInput { .. })
    ));
}

#[test]
fn a_held_resubmission_past_the_deadline_falls_back() {
    let mut plan = plan(complete(Fallback::Unwind), Some(t(1_000))).with_venue_gating(50);
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
        dec!(4),
        dec!(4),
        dec!(100.5),
        dec!(0.2),
        2,
    ))
    .unwrap();
    let r = plan.apply(&ack(B, 0, dec!(4), 10)).unwrap();
    assert_eq!(r.actions.len(), 0);
    assert!(plan.leg(B).pending_resubmit);
    // Liquidity never comes back before the deadline: unwind the residual.
    let r = plan.apply(&LegExecution::Tick { at: t(1_000) }).unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert_eq!(r.actions, vec![unwind("X", Action::Sell, dec!(6))]);
    assert!(!plan.leg(B).pending_resubmit);
    // Net cash: 4 * 100.5 - 10 * 100 - 0.7 = -598.7, with residual 6.
    assert_eq!(plan.net_cash().unwrap(), Money::new(dec!(-598.7), usd()));
    assert_eq!(plan.residual().unwrap(), dec!(6));
}

#[test]
fn net_cash_of_a_completed_plan() {
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
    plan.apply(&fill(
        B,
        0,
        "b1",
        dec!(10),
        dec!(10),
        dec!(100.5),
        dec!(0.5),
        2,
    ))
    .unwrap();
    // 1005 - 1000 - 1 = 4.
    assert_eq!(plan.net_cash().unwrap(), Money::new(dec!(4), usd()));
    assert_eq!(plan.state(), PlanState::Completed);
}

#[test]
fn a_venue_update_from_another_venue_is_rejected() {
    // The legs are on venue VB; a view from VA is not theirs.
    let mut plan = plan(complete(Fallback::Abort), None);
    let mut foreign = view_of("Y", 10, Some((dec!(101), dec!(10))));
    foreign.venue = VenueId::new("VA").unwrap();
    let before = plan.clone();
    assert!(matches!(
        plan.apply(&LegExecution::VenueUpdate {
            leg: B,
            view: foreign
        }),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    assert_eq!(plan, before);
}

#[test]
fn net_cash_is_one_exact_sum() {
    // Buy notional BIG and a fee of BIG pass Decimal::MAX as a running sum;
    // the sell notional BIG brings the net back to -BIG.
    let big = dec!(47536897508558602556126370201);
    let px = dec!(4753689750855860255612637020.1);
    let mut plan = plan(FailurePolicy::Abort, None);
    plan.apply(&fill(A, 0, "a", dec!(10), dec!(10), px, big, 1))
        .unwrap();
    plan.apply(&fill(B, 0, "b", dec!(10), dec!(10), px, dec!(0), 2))
        .unwrap();
    assert_eq!(plan.net_cash().unwrap().amount(), -big);
}

#[test]
fn an_unsupported_action_cannot_build_a_plan() {
    for (first, second) in [(Action::Other, Action::Sell), (Action::Buy, Action::Other)] {
        assert_eq!(
            TwoLegPlan::new(
                spec("X", first, dec!(10), dec!(100), None),
                // The same instrument would be another error: the action is
                // checked first.
                spec("X", second, dec!(10), dec!(100.5), None),
                FailurePolicy::Abort,
                usd(),
                t(0),
            ),
            Err(ArbitrageError::UnsupportedAction {
                action: Action::Other
            })
        );
    }
}
