//! Hand-checked fixtures for budget gating: every resubmission of a
//! two-leg plan is readmitted against its venue's current budget.
//!
//! Fixture: buy 2 BTC-A on venue A at a limit of 100 (first leg) and sell 2
//! BTC-B on venue B at a limit of 101 (second leg), fee 10 bps, the sell on
//! 20 % margin. Admitted at 1000 on views at 990 (bid 101, ask 100):
//!
//! - buy: `2 * 100 + 0.2 fee = 200.2` on A;
//! - sell: `0.2 * 101 * 2 + 0.202 fee = 40.602` on B.
//!
//! A sell readmitted at a bid of 110 needs `44 + 0.22 = 44.22`; at a fill
//! price of 112, `44.8 + 0.224 = 45.024`.

// Tests fail loudly: the library's no-panic lints do not apply here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use arbitrage_rs::legs::{
    ExecId, FailurePolicy, Fallback, LegExecution, LegIndex, LegSpec, PlanAction, PlanState,
    TwoLegPlan,
};
use arbitrage_rs::venues::{
    Admission, AdmissionReject, AdmittedLeg, LegFunding, LegRequest, Reservation, SellFunding,
    VenueBudget, VenueId, VenueView, admit_two_legs,
};
use arbitrage_rs::{
    Action, ArbitrageError, Cost, Currency, Decimal, FeeRate, Instrument, InstrumentId, Leg, Level,
    Money, Price, Quantity, Timestamp, TopOfBook,
};
use rust_decimal_macros::dec;
use std::collections::BTreeMap;

use LegIndex::{First as A, Second as B};

type Budgets = BTreeMap<VenueId, VenueBudget>;

fn usd() -> Currency {
    Currency::new("USD").unwrap()
}
fn m(x: Decimal) -> Money {
    Money::new(x, usd())
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
fn venue(id: &str) -> VenueId {
    VenueId::new(id).unwrap()
}

const MARGIN_20: SellFunding = SellFunding::Margin { rate: dec!(0.2) };

fn funding(bps: Decimal, sell_funding: SellFunding) -> LegFunding {
    LegFunding {
        fee: FeeRate::from_bps(bps).unwrap(),
        sell_funding,
    }
}

fn leg_on(v: &str, sym: &str, side: Action, qty: Decimal, price: Decimal) -> LegSpec {
    LegSpec {
        leg: Leg {
            instrument: Instrument::undated(InstrumentId::new(sym).unwrap()),
            side,
            quantity: q(qty),
            price: p(price),
        },
        deadline: Some(t(5_000)),
        venue: venue(v),
    }
}

fn view(v: &str, sym: &str, at: i64, bid: Decimal) -> VenueView {
    VenueView {
        venue: venue(v),
        instrument: Instrument::undated(InstrumentId::new(sym).unwrap()),
        observed_at: t(at),
        book: TopOfBook {
            bid: Some(Level::new(p(bid), q(dec!(5)))),
            ask: Some(Level::new(p(dec!(100)), q(dec!(5)))),
        },
    }
}

fn budgets(a_cash: Decimal, b_cash: Decimal) -> Budgets {
    let mut map = BTreeMap::new();
    map.insert(venue("A"), VenueBudget::new(venue("A"), m(a_cash)).unwrap());
    map.insert(venue("B"), VenueBudget::new(venue("B"), m(b_cash)).unwrap());
    map
}

/// Admits `buy` and `sell` on their views and returns the admitted legs.
fn admit(
    buy: &LegSpec,
    sell: &LegSpec,
    views: (&VenueView, &VenueView),
    funds: (LegFunding, LegFunding),
    b: &mut Budgets,
) -> [AdmittedLeg; 2] {
    let request = |spec, view, f: LegFunding| LegRequest {
        spec,
        view,
        fee: f.fee,
        sell_funding: f.sell_funding,
    };
    let Admission::Admitted { first, second } = admit_two_legs(
        request(buy, views.0, funds.0),
        request(sell, views.1, funds.1),
        b,
        t(1_000),
        50,
    )
    .unwrap() else {
        panic!("expected both legs admitted");
    };
    [
        AdmittedLeg {
            funding: funds.0,
            view: views.0.clone(),
            reservation: first,
        },
        AdmittedLeg {
            funding: funds.1,
            view: views.1.clone(),
            reservation: second,
        },
    ]
}

fn specs() -> (LegSpec, LegSpec) {
    (
        leg_on("A", "BTC-A", Action::Buy, dec!(2), dec!(100)),
        leg_on("B", "BTC-B", Action::Sell, dec!(2), dec!(101)),
    )
}

/// The fixture plan, gated on `b`, with up to five resubmissions.
fn gated(b: &mut Budgets) -> TwoLegPlan {
    gated_with(b, funding(dec!(10), MARGIN_20))
}

fn gated_with(b: &mut Budgets, sell_funding: LegFunding) -> TwoLegPlan {
    let (buy, sell) = specs();
    let (va, vb) = (
        view("A", "BTC-A", 990, dec!(99)),
        view("B", "BTC-B", 990, dec!(101)),
    );
    let admitted = admit(
        &buy,
        &sell,
        (&va, &vb),
        (funding(dec!(10), MARGIN_20), sell_funding),
        b,
    );
    TwoLegPlan::new(buy, sell, policy(), usd(), t(1_000))
        .unwrap()
        .with_budget_gating(50, admitted)
        .unwrap()
}

fn policy() -> FailurePolicy {
    FailurePolicy::Complete {
        max_resubmits: 5,
        then: Fallback::Unwind,
    }
}

fn update(leg: LegIndex, v: VenueView) -> LegExecution {
    LegExecution::VenueUpdate { leg, view: v }
}
fn rejected(leg: LegIndex, attempt: u32, at: i64) -> LegExecution {
    LegExecution::Rejected {
        leg,
        attempt,
        at: t(at),
    }
}
fn tick(at: i64) -> LegExecution {
    LegExecution::Tick { at: t(at) }
}
fn fill(
    leg: LegIndex,
    attempt: u32,
    id: &str,
    qty: Decimal,
    cum: Decimal,
    px: Decimal,
) -> LegExecution {
    LegExecution::Fill {
        leg,
        attempt,
        exec_id: ExecId(id.to_owned()),
        quantity: q(qty),
        cum_qty: q(cum),
        price: p(px),
        fee: Cost::new(m(dec!(0))),
        at: t(1_050),
    }
}
fn reservation(v: &str, cash: Decimal) -> Reservation {
    Reservation {
        venue: venue(v),
        cash: m(cash),
        inventory: None,
    }
}
fn resubmits(actions: &[PlanAction]) -> usize {
    actions
        .iter()
        .filter(|a| matches!(a, PlanAction::Resubmit { .. }))
        .count()
}

#[test]
fn a_sell_resubmitted_after_the_bid_rose_reserves_the_higher_requirement() {
    let mut b = budgets(dec!(1_000), dec!(100));
    let mut plan = gated(&mut b);
    assert_eq!(b[&venue("B")].reserved_cash(), m(dec!(40.602)));

    plan.apply_with_budgets(&update(B, view("B", "BTC-B", 1_090, dec!(110))), &mut b)
        .unwrap();
    let report = plan
        .apply_with_budgets(&rejected(B, 0, 1_100), &mut b)
        .unwrap();

    assert_eq!(
        report.actions,
        vec![PlanAction::Resubmit {
            leg: B,
            attempt: 1,
            quantity: q(dec!(2)),
            price: p(dec!(101)),
        }]
    );
    let leg = plan.leg(B);
    assert_eq!(leg.reservation, Some(reservation("B", dec!(44.22))));
    assert_eq!(leg.covered_bound, Some(p(dec!(110))));
    assert_eq!(leg.hold, None);
    // Only the delta was added: 40.602 released, 44.22 reserved.
    assert_eq!(b[&venue("B")].reserved_cash(), m(dec!(44.22)));
    assert_eq!(b[&venue("A")].reserved_cash(), m(dec!(200.2)));
}

#[test]
fn a_resubmission_that_does_not_fit_is_held_and_sent_after_a_tick() {
    // Another plan holds 17.25585 on B (sell 0.85 at 101 on 20 % margin),
    // so after releasing the leg's 40.602 only 42.74415 of B's 60 is free:
    // less than the 44.22 needed at a bid of 110.
    let mut b = budgets(dec!(1_000), dec!(60));
    let mut plan = gated(&mut b);
    let (other_buy, other_sell) = (
        leg_on("A", "ETH-A", Action::Buy, dec!(0.85), dec!(100)),
        leg_on("B", "ETH-B", Action::Sell, dec!(0.85), dec!(101)),
    );
    let other = admit(
        &other_buy,
        &other_sell,
        (
            &view("A", "ETH-A", 990, dec!(99)),
            &view("B", "ETH-B", 990, dec!(101)),
        ),
        (funding(dec!(10), MARGIN_20), funding(dec!(10), MARGIN_20)),
        &mut b,
    );
    assert_eq!(other[1].reservation, reservation("B", dec!(17.25585)));
    let before = b.clone();

    plan.apply_with_budgets(&update(B, view("B", "BTC-B", 1_090, dec!(110))), &mut b)
        .unwrap();
    let report = plan
        .apply_with_budgets(&rejected(B, 0, 1_100), &mut b)
        .unwrap();
    assert_eq!(resubmits(&report.actions), 0);
    let leg = plan.leg(B);
    assert!(leg.pending_resubmit);
    assert_eq!(
        leg.hold,
        Some(AdmissionReject::InsufficientCash {
            required: m(dec!(44.22)),
            available: m(dec!(42.74415)),
        })
    );
    // Nothing moved: the leg keeps its admission reservation.
    assert_eq!(leg.reservation, Some(reservation("B", dec!(40.602))));
    assert_eq!(b, before);

    // The other plan frees its capital outside this plan; a tick retries.
    b.get_mut(&venue("B"))
        .unwrap()
        .release(&other[1].reservation)
        .unwrap();
    let report = plan.apply_with_budgets(&tick(1_110), &mut b).unwrap();
    assert_eq!(resubmits(&report.actions), 1);
    assert_eq!(plan.leg(B).hold, None);
    assert!(!plan.leg(B).pending_resubmit);
    assert_eq!(b[&venue("B")].reserved_cash(), m(dec!(44.22)));
}

#[test]
fn a_resubmitted_buy_keeps_its_reservation() {
    let mut b = budgets(dec!(1_000), dec!(100));
    let mut plan = gated(&mut b);
    plan.apply_with_budgets(&update(A, view("A", "BTC-A", 1_090, dec!(99))), &mut b)
        .unwrap();
    let report = plan
        .apply_with_budgets(&rejected(A, 0, 1_100), &mut b)
        .unwrap();
    assert_eq!(resubmits(&report.actions), 1);
    assert_eq!(plan.leg(A).reservation, Some(reservation("A", dec!(200.2))));
    assert_eq!(b[&venue("A")].reserved_cash(), m(dec!(200.2)));
}

#[test]
fn a_fill_above_the_bound_is_recorded_and_leaves_a_deficit() {
    // B has only 41 cash: the fill at 112 cannot be covered, but it is
    // still recorded; the resubmission is held.
    let mut b = budgets(dec!(1_000), dec!(41));
    let mut plan = gated(&mut b);
    let report = plan
        .apply_with_budgets(&fill(B, 0, "b-1", dec!(1), dec!(1), dec!(112)), &mut b)
        .unwrap();
    assert!(!report.duplicate);
    assert_eq!(plan.leg(B).executed().unwrap(), dec!(1));
    assert_eq!(plan.leg(B).deficit, Some(p(dec!(112))));
    assert_eq!(
        plan.leg(B).reservation,
        Some(reservation("B", dec!(40.602)))
    );

    // The rest is cancelled; the readmission at 112 needs 45.024.
    plan.apply_with_budgets(&update(B, view("B", "BTC-B", 1_090, dec!(101))), &mut b)
        .unwrap();
    let report = plan
        .apply_with_budgets(
            &LegExecution::CancelAck {
                leg: B,
                attempt: 0,
                cum_qty: dec!(1),
                at: t(1_100),
            },
            &mut b,
        )
        .unwrap();
    assert_eq!(resubmits(&report.actions), 0);
    assert_eq!(
        plan.leg(B).hold,
        Some(AdmissionReject::InsufficientCash {
            required: m(dec!(45.024)),
            available: m(dec!(41)),
        })
    );
    assert_eq!(plan.leg(B).deficit, Some(p(dec!(112))));
}

#[test]
fn a_covered_deficit_is_cleared_by_the_readmission() {
    let mut b = budgets(dec!(1_000), dec!(100));
    let mut plan = gated(&mut b);
    plan.apply_with_budgets(&fill(B, 0, "b-1", dec!(1), dec!(1), dec!(112)), &mut b)
        .unwrap();
    plan.apply_with_budgets(&update(B, view("B", "BTC-B", 1_090, dec!(101))), &mut b)
        .unwrap();
    let report = plan
        .apply_with_budgets(
            &LegExecution::CancelAck {
                leg: B,
                attempt: 0,
                cum_qty: dec!(1),
                at: t(1_100),
            },
            &mut b,
        )
        .unwrap();
    assert_eq!(
        report.actions,
        vec![PlanAction::Resubmit {
            leg: B,
            attempt: 1,
            quantity: q(dec!(1)),
            price: p(dec!(101)),
        }]
    );
    let leg = plan.leg(B);
    assert_eq!(leg.deficit, None);
    assert_eq!(leg.covered_bound, Some(p(dec!(112))));
    assert_eq!(leg.reservation, Some(reservation("B", dec!(45.024))));
    assert_eq!(b[&venue("B")].reserved_cash(), m(dec!(45.024)));
}

#[test]
fn a_fill_event_never_readmits() {
    // The sell is held for lack of budget; capital is then freed, but the
    // next event is a fill on the other leg: nothing is resubmitted until a
    // non-fill event.
    let mut b = budgets(dec!(1_000), dec!(42));
    let mut plan = gated(&mut b);
    plan.apply_with_budgets(&update(B, view("B", "BTC-B", 1_090, dec!(110))), &mut b)
        .unwrap();
    plan.apply_with_budgets(&rejected(B, 0, 1_100), &mut b)
        .unwrap();
    assert!(plan.leg(B).pending_resubmit);

    // Free capital: B now has 100 cash, still holding the leg's 40.602
    // (an admission of the same legs on fresh budgets reserves exactly it).
    let mut fresh = budgets(dec!(1_000), dec!(100));
    let (buy, sell) = specs();
    let _ = admit(
        &buy,
        &sell,
        (
            &view("A", "BTC-A", 990, dec!(99)),
            &view("B", "BTC-B", 990, dec!(101)),
        ),
        (funding(dec!(10), MARGIN_20), funding(dec!(10), MARGIN_20)),
        &mut fresh,
    );
    b.insert(venue("B"), fresh[&venue("B")].clone());

    let report = plan
        .apply_with_budgets(&fill(A, 0, "a-1", dec!(1), dec!(1), dec!(100)), &mut b)
        .unwrap();
    assert_eq!(resubmits(&report.actions), 0);
    assert!(plan.leg(B).pending_resubmit);
    let report = plan.apply_with_budgets(&tick(1_110), &mut b).unwrap();
    assert_eq!(resubmits(&report.actions), 1);
}

#[test]
fn releasing_returns_the_budgets_to_their_initial_state_once() {
    let initial = budgets(dec!(1_000), dec!(100));
    let mut b = initial.clone();
    let mut plan = gated(&mut b);
    plan.apply_with_budgets(&update(B, view("B", "BTC-B", 1_090, dec!(110))), &mut b)
        .unwrap();
    plan.apply_with_budgets(&rejected(B, 0, 1_100), &mut b)
        .unwrap();
    assert_eq!(b[&venue("B")].reserved_cash(), m(dec!(44.22)));

    plan.release_reservations(&mut b).unwrap();
    assert_eq!(b, initial);
    assert_eq!(plan.leg(A).reservation, None);
    assert_eq!(plan.leg(B).reservation, None);
    // Idempotent.
    plan.release_reservations(&mut b).unwrap();
    assert_eq!(b, initial);

    // A resubmission due after the release is held, never sent.
    plan.apply_with_budgets(&update(B, view("B", "BTC-B", 1_190, dec!(110))), &mut b)
        .unwrap();
    let report = plan
        .apply_with_budgets(&rejected(B, 1, 1_200), &mut b)
        .unwrap();
    assert_eq!(resubmits(&report.actions), 0);
    assert_eq!(plan.leg(B).hold, Some(AdmissionReject::Released));
    assert_eq!(b, initial);
}

#[test]
fn a_readmission_error_leaves_the_plan_and_the_budgets_unchanged() {
    let mut b = budgets(dec!(1_000), dec!(100));
    let mut plan = gated(&mut b);
    plan.apply_with_budgets(&update(B, view("B", "BTC-B", 1_090, dec!(110))), &mut b)
        .unwrap();
    // B's budget no longer holds the leg's reservation: releasing it fails.
    b.insert(
        venue("B"),
        VenueBudget::new(venue("B"), m(dec!(100))).unwrap(),
    );
    let (plan_before, b_before) = (plan.clone(), b.clone());
    assert!(matches!(
        plan.apply_with_budgets(&rejected(B, 0, 1_100), &mut b),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    assert_eq!(plan, plan_before);
    assert_eq!(b, b_before);
}

#[test]
fn capital_is_never_borrowed_from_another_venue() {
    let mut b = budgets(dec!(1_000_000), dec!(100));
    let mut plan = gated(&mut b);
    b.remove(&venue("B"));
    plan.apply_with_budgets(&update(B, view("B", "BTC-B", 1_090, dec!(110))), &mut b)
        .unwrap();
    let report = plan
        .apply_with_budgets(&rejected(B, 0, 1_100), &mut b)
        .unwrap();
    assert_eq!(resubmits(&report.actions), 0);
    assert_eq!(plan.leg(B).hold, Some(AdmissionReject::NoBudget));
    assert_eq!(b[&venue("A")].reserved_cash(), m(dec!(200.2)));
}

#[test]
fn the_budget_contract_cannot_be_bypassed() {
    let mut b = budgets(dec!(1_000), dec!(100));
    let mut gated_plan = gated(&mut b);
    // A gated plan is driven with apply_with_budgets only.
    assert!(matches!(
        gated_plan.apply(&tick(1_010)),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    // A plan without gating has no budgets to apply or release.
    let (buy, sell) = specs();
    let mut ungated = TwoLegPlan::new(buy, sell, policy(), usd(), t(1_000)).unwrap();
    let before = b.clone();
    assert!(matches!(
        ungated.apply_with_budgets(&tick(1_010), &mut b),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    assert!(matches!(
        ungated.release_reservations(&mut b),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    assert_eq!(b, before);
}

#[test]
fn budget_gating_checks_what_it_is_handed() {
    let mut b = budgets(dec!(1_000), dec!(100));
    let (buy, sell) = specs();
    let (va, vb) = (
        view("A", "BTC-A", 990, dec!(99)),
        view("B", "BTC-B", 990, dec!(101)),
    );
    let fund = (funding(dec!(10), MARGIN_20), funding(dec!(10), MARGIN_20));
    let admitted = admit(&buy, &sell, (&va, &vb), fund, &mut b);
    let fresh = || TwoLegPlan::new(buy.clone(), sell.clone(), policy(), usd(), t(1_000)).unwrap();

    // A reservation that is not the leg's requirement.
    let mut wrong = admitted.clone();
    wrong[1].reservation.cash = m(dec!(40));
    assert!(fresh().with_budget_gating(50, wrong).is_err());
    // A view whose bid differs from the one the reservation was made on.
    let mut moved = admitted.clone();
    moved[1].view = view("B", "BTC-B", 990, dec!(110));
    assert!(fresh().with_budget_gating(50, moved).is_err());
    // A reservation of another venue.
    let mut elsewhere = admitted.clone();
    elsewhere[1].reservation.venue = venue("A");
    assert!(fresh().with_budget_gating(50, elsewhere).is_err());
    // Other funding than the admission's.
    let mut funded = admitted.clone();
    funded[1].funding = funding(dec!(20), MARGIN_20);
    assert!(fresh().with_budget_gating(50, funded).is_err());
    // After the plan has seen an event.
    let mut started = fresh();
    started.apply(&rejected(B, 0, 1_100)).unwrap();
    assert!(matches!(
        started.with_budget_gating(50, admitted.clone()),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    // The matching legs are accepted.
    assert!(fresh().with_budget_gating(50, admitted).is_ok());
}

#[test]
fn a_held_resubmission_past_the_deadline_falls_back_once() {
    let mut b = budgets(dec!(1_000), dec!(42));
    let mut plan = gated(&mut b);
    plan.apply_with_budgets(&update(B, view("B", "BTC-B", 1_090, dec!(110))), &mut b)
        .unwrap();
    plan.apply_with_budgets(&rejected(B, 0, 1_100), &mut b)
        .unwrap();
    assert!(plan.leg(B).pending_resubmit);

    // At the deadline (5000) the fallback applies: cancel the buy leg.
    let report = plan.apply_with_budgets(&tick(5_000), &mut b).unwrap();
    assert_eq!(resubmits(&report.actions), 0);
    assert!(!plan.leg(B).pending_resubmit);
    assert!(
        report
            .actions
            .iter()
            .any(|a| matches!(a, PlanAction::Cancel { leg: A, attempt: 0 }))
    );
    assert_eq!(plan.state(), PlanState::Unwinding);
    // Later events neither resubmit nor fall back again.
    let report = plan.apply_with_budgets(&tick(5_100), &mut b).unwrap();
    assert_eq!(report.actions, vec![]);
}

#[test]
fn repeated_resubmissions_hold_a_single_reservation() {
    let mut b = budgets(dec!(1_000), dec!(100));
    let mut plan = gated(&mut b);
    plan.apply_with_budgets(&update(B, view("B", "BTC-B", 1_090, dec!(110))), &mut b)
        .unwrap();
    plan.apply_with_budgets(&rejected(B, 0, 1_100), &mut b)
        .unwrap();
    // The bid falls back: the bound does not go down.
    plan.apply_with_budgets(&update(B, view("B", "BTC-B", 1_190, dec!(101))), &mut b)
        .unwrap();
    let report = plan
        .apply_with_budgets(&rejected(B, 1, 1_200), &mut b)
        .unwrap();
    assert_eq!(resubmits(&report.actions), 1);
    assert_eq!(plan.leg(B).covered_bound, Some(p(dec!(110))));
    assert_eq!(plan.leg(B).reservation, Some(reservation("B", dec!(44.22))));
    assert_eq!(b[&venue("B")].reserved_cash(), m(dec!(44.22)));
}

#[test]
fn legs_on_the_same_venue_only_touch_their_own_reservation() {
    // Both legs on venue A, sharing its budget: 200.2 + 40.602.
    let mut b = budgets(dec!(1_000), dec!(0));
    let buy = leg_on("A", "BTC-A", Action::Buy, dec!(2), dec!(100));
    let sell = leg_on("A", "BTC-PERP", Action::Sell, dec!(2), dec!(101));
    let (va, vs) = (
        view("A", "BTC-A", 990, dec!(99)),
        view("A", "BTC-PERP", 990, dec!(101)),
    );
    let fund = (funding(dec!(10), MARGIN_20), funding(dec!(10), MARGIN_20));
    let admitted = admit(&buy, &sell, (&va, &vs), fund, &mut b);
    let mut plan = TwoLegPlan::new(buy, sell, policy(), usd(), t(1_000))
        .unwrap()
        .with_budget_gating(50, admitted)
        .unwrap();
    assert_eq!(b[&venue("A")].reserved_cash(), m(dec!(240.802)));

    plan.apply_with_budgets(&update(B, view("A", "BTC-PERP", 1_090, dec!(110))), &mut b)
        .unwrap();
    plan.apply_with_budgets(&rejected(B, 0, 1_100), &mut b)
        .unwrap();
    // 200.2 for the buy, untouched, plus the sell's new 44.22.
    assert_eq!(b[&venue("A")].reserved_cash(), m(dec!(244.42)));
    assert_eq!(plan.leg(A).reservation, Some(reservation("A", dec!(200.2))));
}

#[test]
fn a_rebate_never_lowers_the_requirement() {
    // -5 bps: the fee is floored at zero, so only the margin grows:
    // 0.2 * 101 * 2 = 40.4 at admission, 0.2 * 110 * 2 = 44 readmitted.
    let mut b = budgets(dec!(1_000), dec!(100));
    let mut plan = gated_with(&mut b, funding(dec!(-5), MARGIN_20));
    assert_eq!(plan.leg(B).reservation, Some(reservation("B", dec!(40.4))));
    plan.apply_with_budgets(&update(B, view("B", "BTC-B", 1_090, dec!(110))), &mut b)
        .unwrap();
    plan.apply_with_budgets(&rejected(B, 0, 1_100), &mut b)
        .unwrap();
    assert_eq!(plan.leg(B).reservation, Some(reservation("B", dec!(44))));
    assert_eq!(b[&venue("B")].reserved_cash(), m(dec!(44)));
}

#[test]
fn an_inventory_sell_keeps_its_quantity_and_raises_its_fee() {
    // Selling 2 from inventory: the quantity stays reserved; the fee grows
    // from 0.202 at 101 to 0.22 at 110.
    let mut b = budgets(dec!(1_000), dec!(100));
    let inventory = b
        .remove(&venue("B"))
        .unwrap()
        .with_inventory(InstrumentId::new("BTC-B").unwrap(), q(dec!(2)))
        .unwrap();
    b.insert(venue("B"), inventory);
    let mut plan = gated_with(&mut b, funding(dec!(10), SellFunding::Inventory));
    let held = Some((InstrumentId::new("BTC-B").unwrap(), q(dec!(2))));
    assert_eq!(
        plan.leg(B).reservation,
        Some(Reservation {
            venue: venue("B"),
            cash: m(dec!(0.202)),
            inventory: held.clone(),
        })
    );
    plan.apply_with_budgets(&update(B, view("B", "BTC-B", 1_090, dec!(110))), &mut b)
        .unwrap();
    plan.apply_with_budgets(&rejected(B, 0, 1_100), &mut b)
        .unwrap();
    assert_eq!(
        plan.leg(B).reservation,
        Some(Reservation {
            venue: venue("B"),
            cash: m(dec!(0.22)),
            inventory: held,
        })
    );
    assert_eq!(
        b[&venue("B")]
            .available_inventory(&InstrumentId::new("BTC-B").unwrap())
            .unwrap(),
        dec!(0)
    );
}

#[test]
fn a_buy_filled_above_its_limit_is_covered_at_the_fill_price() {
    // A venue fill above the buy limit (an anomaly) is recorded; the next
    // readmission covers it: 2 * 103 + 0.206 = 206.206.
    let mut b = budgets(dec!(1_000), dec!(100));
    let mut plan = gated(&mut b);
    plan.apply_with_budgets(&fill(A, 0, "a-1", dec!(1), dec!(1), dec!(103)), &mut b)
        .unwrap();
    assert_eq!(plan.leg(A).deficit, Some(p(dec!(103))));
    plan.apply_with_budgets(&update(A, view("A", "BTC-A", 1_090, dec!(99))), &mut b)
        .unwrap();
    plan.apply_with_budgets(
        &LegExecution::CancelAck {
            leg: A,
            attempt: 0,
            cum_qty: dec!(1),
            at: t(1_100),
        },
        &mut b,
    )
    .unwrap();
    assert_eq!(
        plan.leg(A).reservation,
        Some(reservation("A", dec!(206.206)))
    );
    assert_eq!(plan.leg(A).deficit, None);
}
