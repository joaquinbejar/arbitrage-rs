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

/// Buy 10 X at 100 (first) and sell 10 Y at 100.5 (second), created at 0.
fn plan(policy: FailurePolicy, deadline: Option<Timestamp>) -> TwoLegPlan {
    let spec = |id: &str, side, price| LegSpec {
        leg: Leg {
            instrument: inst(id),
            side,
            quantity: q(dec!(10)),
            price: p(price),
        },
        deadline,
    };
    TwoLegPlan::new(
        spec("X", Side::Buy, dec!(100)),
        spec("Y", Side::Sell, dec!(100.5)),
        policy,
        usd(),
        t(0),
    )
    .unwrap()
}

fn fill(
    leg: LegIndex,
    attempt: u32,
    id: &str,
    qty: Decimal,
    price: Decimal,
    f: Decimal,
    at: i64,
) -> LegExecution {
    LegExecution::Fill {
        leg,
        attempt,
        exec_id: ExecId(id.to_owned()),
        quantity: q(qty),
        price: p(price),
        fee: fee(f),
        at: t(at),
    }
}

#[test]
fn both_legs_fill_and_the_plan_completes() {
    let mut plan = plan(FailurePolicy::Abort, None);
    let r = plan
        .apply(&fill(A, 0, "a1", dec!(10), dec!(100), dec!(0.5), 1))
        .unwrap();
    // One leg filled, the other not: the residual is visible.
    assert_eq!(r.residual, dec!(10));
    assert_eq!(r.state, PlanState::Working);
    plan.apply(&fill(B, 0, "b1", dec!(4), dec!(100.5), dec!(0.201), 2))
        .unwrap();
    let r = plan
        .apply(&fill(B, 0, "b2", dec!(6), dec!(100.6), dec!(0.3018), 3))
        .unwrap();
    assert_eq!(r.state, PlanState::Completed);
    assert_eq!(r.residual, dec!(0));
    assert!(r.actions.is_empty());
    // 4 * 100.5 + 6 * 100.6 = 402 + 603.6 = 1005.6.
    assert_eq!(plan.leg(B).filled_notional, Money::new(dec!(1005.6), usd()));
    assert_eq!(plan.leg(B).fees, fee(dec!(0.5028)));
    assert_eq!(plan.fees().unwrap(), fee(dec!(1.0028)));
    assert_eq!(plan.leg(A).status, LegStatus::Filled);
}

#[test]
fn filled_and_rejected_under_abort_keeps_the_residual_visible() {
    let mut plan = plan(FailurePolicy::Abort, None);
    plan.apply(&fill(A, 0, "a1", dec!(10), dec!(100), dec!(0.5), 1))
        .unwrap();
    let r = plan
        .apply(&LegExecution::Rejected {
            leg: B,
            attempt: 0,
            at: t(2),
        })
        .unwrap();
    assert_eq!(r.state, PlanState::Aborted);
    assert!(r.actions.is_empty());
    assert_eq!(r.residual, dec!(10));
    assert_eq!(plan.leg(B).status, LegStatus::Rejected);
}

#[test]
fn filled_and_rejected_under_unwind_requests_the_offset() {
    let mut plan = plan(FailurePolicy::Unwind, None);
    plan.apply(&fill(A, 0, "a1", dec!(10), dec!(100), dec!(0.5), 1))
        .unwrap();
    let r = plan
        .apply(&LegExecution::Rejected {
            leg: B,
            attempt: 0,
            at: t(2),
        })
        .unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert_eq!(
        r.actions,
        vec![PlanAction::Unwind {
            instrument: inst("X"),
            side: Side::Sell,
            quantity: q(dec!(10)),
        }]
    );
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
    plan.apply(&fill(A, 0, "a1", dec!(10), dec!(100), dec!(0.5), 1))
        .unwrap();
    plan.apply(&fill(B, 0, "b1", dec!(3), dec!(100.5), dec!(0.15), 2))
        .unwrap();
    // B's attempt 0 is cancelled by the venue with 7 left: resubmit 7.
    let r = plan
        .apply(&LegExecution::CancelAck {
            leg: B,
            attempt: 0,
            at: t(3),
        })
        .unwrap();
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
    // Attempt 1 fills 2 and is rejected... a rejection with fills is
    // invalid, so it is cancelled instead: 5 left and no resubmit left.
    plan.apply(&fill(B, 1, "b2", dec!(2), dec!(100.5), dec!(0.1), 4))
        .unwrap();
    let r = plan
        .apply(&LegExecution::CancelAck {
            leg: B,
            attempt: 1,
            at: t(5),
        })
        .unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert_eq!(
        r.actions,
        vec![PlanAction::Unwind {
            instrument: inst("X"),
            side: Side::Sell,
            quantity: q(dec!(5)),
        }]
    );
    assert_eq!(plan.leg(B).resubmits, 1);
    assert_eq!(plan.leg(B).filled, dec!(5));
}

#[test]
fn unwind_waits_for_the_other_cancel_and_follows_late_fills() {
    let mut plan = plan(FailurePolicy::Unwind, None);
    plan.apply(&fill(A, 0, "a1", dec!(6), dec!(100), dec!(0.3), 1))
        .unwrap();
    // B fails while A is still working: cancel A first.
    let r = plan
        .apply(&LegExecution::Rejected {
            leg: B,
            attempt: 0,
            at: t(2),
        })
        .unwrap();
    assert_eq!(r.state, PlanState::Unwinding);
    assert_eq!(r.actions, vec![PlanAction::Cancel { leg: A, attempt: 0 }]);
    assert_eq!(
        plan.leg(A).status,
        LegStatus::CancelRequested { attempt: 0 }
    );
    // A fill that raced the cancel still counts.
    let r = plan
        .apply(&fill(A, 0, "a2", dec!(2), dec!(100), dec!(0.1), 3))
        .unwrap();
    assert!(r.actions.is_empty());
    assert_eq!(r.residual, dec!(8));
    let r = plan
        .apply(&LegExecution::CancelAck {
            leg: A,
            attempt: 0,
            at: t(4),
        })
        .unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert_eq!(
        r.actions,
        vec![PlanAction::Unwind {
            instrument: inst("X"),
            side: Side::Sell,
            quantity: q(dec!(8)),
        }]
    );
    // A late fill after the acknowledgement adds 1 more to unwind.
    let r = plan
        .apply(&fill(A, 0, "a3", dec!(1), dec!(100), dec!(0.05), 5))
        .unwrap();
    assert_eq!(
        r.actions,
        vec![PlanAction::Unwind {
            instrument: inst("X"),
            side: Side::Sell,
            quantity: q(dec!(1)),
        }]
    );
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
    plan.apply(&fill(A, 0, "a1", dec!(10), dec!(100), dec!(0.5), 500))
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
    let r = plan
        .apply(&LegExecution::CancelAck {
            leg: B,
            attempt: 0,
            at: t(1_100),
        })
        .unwrap();
    assert!(r.actions.is_empty());
    assert_eq!(r.state, PlanState::Aborted);
    assert_eq!(r.residual, dec!(10));
}

#[test]
fn repeated_events_do_not_double_apply() {
    let mut plan = plan(FailurePolicy::Abort, None);
    let f = fill(A, 0, "a1", dec!(4), dec!(100), dec!(0.2), 1);
    plan.apply(&f).unwrap();
    let r = plan.apply(&f).unwrap();
    assert!(r.duplicate);
    assert!(r.actions.is_empty());
    assert_eq!(plan.leg(A).filled, dec!(4));
    assert_eq!(plan.leg(A).fees, fee(dec!(0.2)));
    // A repeat delivered later (older timestamp is fine for a duplicate).
    let r = plan.apply(&f).unwrap();
    assert!(r.duplicate);

    let rej = LegExecution::Rejected {
        leg: B,
        attempt: 0,
        at: t(2),
    };
    let first = plan.apply(&rej).unwrap();
    assert_eq!(
        first.actions,
        vec![PlanAction::Cancel { leg: A, attempt: 0 }]
    );
    let again = plan.apply(&rej).unwrap();
    assert!(again.duplicate && again.actions.is_empty());

    let ack = LegExecution::CancelAck {
        leg: A,
        attempt: 0,
        at: t(3),
    };
    assert_eq!(plan.apply(&ack).unwrap().state, PlanState::Aborted);
    assert!(plan.apply(&ack).unwrap().duplicate);
    assert_eq!(plan.residual().unwrap(), dec!(4));
}

#[test]
fn invalid_events_leave_the_plan_unchanged() {
    let mut plan = plan(FailurePolicy::Abort, None);
    plan.apply(&fill(A, 0, "a1", dec!(4), dec!(100), dec!(0.2), 10))
        .unwrap();
    let before = plan.clone();

    let cases = [
        // Overfill: 4 + 7 > 10.
        fill(A, 0, "a2", dec!(7), dec!(100), dec!(0.2), 11),
        // Older than the last applied event.
        fill(A, 0, "a3", dec!(1), dec!(100), dec!(0.2), 9),
        // Attempt never sent.
        fill(B, 1, "b1", dec!(1), dec!(100.5), dec!(0.2), 11),
        // Execution id reused with different content.
        fill(A, 0, "a1", dec!(5), dec!(100), dec!(0.2), 11),
        // Rejection of an attempt with fills.
        LegExecution::Rejected {
            leg: A,
            attempt: 0,
            at: t(11),
        },
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
    plan.apply(&LegExecution::Rejected {
        leg: B,
        attempt: 0,
        at: t(12),
    })
    .unwrap();
    assert!(matches!(
        plan.apply(&fill(B, 0, "b3", dec!(1), dec!(100.5), dec!(0.1), 13)),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    assert!(matches!(
        plan.apply(&LegExecution::CancelAck {
            leg: B,
            attempt: 0,
            at: t(13)
        }),
        Err(ArbitrageError::InvalidInput { .. })
    ));
}

#[test]
fn construction_is_validated() {
    let leg = |id: &str, side| LegSpec {
        leg: Leg {
            instrument: inst(id),
            side,
            quantity: q(dec!(1)),
            price: p(dec!(1)),
        },
        deadline: None,
    };
    let new = |a, b| TwoLegPlan::new(a, b, FailurePolicy::Abort, usd(), t(0));
    assert!(new(leg("X", Side::Buy), leg("X", Side::Sell)).is_err());
    assert!(new(leg("X", Side::Buy), leg("Y", Side::Buy)).is_err());
    let mut late = leg("Y", Side::Sell);
    late.deadline = Some(t(0));
    assert!(new(leg("X", Side::Buy), late).is_err());
    assert!(new(leg("X", Side::Buy), leg("Y", Side::Sell)).is_ok());
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

#[test]
fn both_legs_failing_flat_needs_no_unwind() {
    let mut plan = plan(FailurePolicy::Unwind, None);
    let r = plan
        .apply(&LegExecution::Rejected {
            leg: A,
            attempt: 0,
            at: t(1),
        })
        .unwrap();
    assert_eq!(r.actions, vec![PlanAction::Cancel { leg: B, attempt: 0 }]);
    let r = plan
        .apply(&LegExecution::CancelAck {
            leg: B,
            attempt: 0,
            at: t(2),
        })
        .unwrap();
    assert_eq!(r.state, PlanState::UnwindRequested);
    assert!(r.actions.is_empty());
    assert_eq!(r.residual, dec!(0));
}
