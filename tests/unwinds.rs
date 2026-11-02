//! Hand-checked fixtures for unwinds: admission against venue budgets,
//! holding and retry, ends reported with `UnwindDone`, reversal, deadline
//! and release.
//!
//! Fixture: buy 2 BTC-A on venue A at a limit of 100 (first leg) and sell 2
//! BTC-B on venue B at a limit of 101 (second leg), fee 10 bps, the sell on
//! 20 % margin, policy unwind. Admitted at 1000 on views at 990:
//!
//! - buy: `2 * 100 + 0.2 fee = 200.2` reserved on A;
//! - sell: `0.2 * 101 * 2 + 0.202 fee = 40.602` reserved on B.
//!
//! When the buy fills and the sell is rejected, the residual is +2: a sell
//! of 2 BTC-A on A at A's bid of 99. From inventory it reserves the 2 BTC-A
//! and the fee `99 * 2 * 0.001 = 0.198`; on 20 % margin
//! `0.2 * 99 * 2 + 0.198 = 39.798`. When the sell fills and the buy is
//! rejected, the residual is -2: a buy of 2 BTC-B on B at B's ask of 100,
//! `2 * 100 + 0.2 = 200.2` in cash.

// Tests fail loudly: the library's no-panic lints do not apply here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use arbitrage_rs::legs::{
    ExecId, FailurePolicy, LegExecution, LegIndex, LegSpec, PlanAction, PlanState, TwoLegPlan,
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

const MARGIN_20: SellFunding = SellFunding::Margin { rate: dec!(0.2) };

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
fn inst(sym: &str) -> Instrument {
    Instrument::undated(InstrumentId::new(sym).unwrap())
}
fn fee10() -> FeeRate {
    FeeRate::from_bps(dec!(10)).unwrap()
}

fn leg_on(v: &str, sym: &str, side: Action, qty: Decimal, price: Decimal) -> LegSpec {
    LegSpec {
        leg: Leg {
            instrument: inst(sym),
            side,
            quantity: q(qty),
            price: p(price),
        },
        deadline: Some(t(5_000)),
        venue: venue(v),
    }
}

/// A view with a bid of `bid` and an ask of 100, 5 each side.
fn view(v: &str, sym: &str, at: i64, bid: Decimal) -> VenueView {
    VenueView {
        venue: venue(v),
        instrument: inst(sym),
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

/// Credits `qty` of `sym` to the budget of venue `v`, as the caller does
/// after a fill.
fn credit(b: &mut Budgets, v: &str, sym: &str, qty: Decimal) {
    let budget = b.remove(&venue(v)).unwrap();
    b.insert(
        venue(v),
        budget
            .with_inventory(InstrumentId::new(sym).unwrap(), q(qty))
            .unwrap(),
    );
}

/// The fixture plan, gated on `b`; a sell unwinding the buy leg is funded
/// with `unwind_funding`.
fn gated(b: &mut Budgets, unwind_funding: SellFunding) -> TwoLegPlan {
    let buy = leg_on("A", "BTC-A", Action::Buy, dec!(2), dec!(100));
    let sell = leg_on("B", "BTC-B", Action::Sell, dec!(2), dec!(101));
    let (va, vb) = (
        view("A", "BTC-A", 990, dec!(99)),
        view("B", "BTC-B", 990, dec!(101)),
    );
    let request = |spec, view, sell_funding| LegRequest {
        spec,
        view,
        fee: fee10(),
        sell_funding,
    };
    let Admission::Admitted { first, second } = admit_two_legs(
        request(&buy, &va, MARGIN_20),
        request(&sell, &vb, MARGIN_20),
        b,
        t(1_000),
        50,
    )
    .unwrap() else {
        panic!("expected both legs admitted");
    };
    let leg_funding = LegFunding {
        fee: fee10(),
        sell_funding: MARGIN_20,
    };
    let admitted = [
        AdmittedLeg {
            funding: leg_funding,
            view: va,
            reservation: first,
            unwind_funding,
        },
        AdmittedLeg {
            funding: leg_funding,
            view: vb,
            reservation: second,
            unwind_funding,
        },
    ];
    TwoLegPlan::new(buy, sell, FailurePolicy::Unwind, usd(), t(1_000))
        .unwrap()
        .with_budget_gating(50, admitted)
        .unwrap()
}

fn update(leg: LegIndex, v: VenueView) -> LegExecution {
    LegExecution::VenueUpdate { leg, view: v }
}
fn rejected(leg: LegIndex, at: i64) -> LegExecution {
    LegExecution::Rejected {
        leg,
        attempt: 0,
        at: t(at),
    }
}
fn tick(at: i64) -> LegExecution {
    LegExecution::Tick { at: t(at) }
}
fn fill(leg: LegIndex, id: &str, qty: Decimal, cum: Decimal, px: Decimal, at: i64) -> LegExecution {
    LegExecution::Fill {
        leg,
        attempt: 0,
        exec_id: ExecId(id.to_owned()),
        quantity: q(qty),
        cum_qty: q(cum),
        price: p(px),
        fee: Cost::new(m(dec!(0))),
        at: t(at),
    }
}
fn done(leg: LegIndex, unwind: u32, filled: Decimal, at: i64) -> LegExecution {
    LegExecution::UnwindDone {
        leg,
        unwind,
        filled,
        at: t(at),
    }
}
fn sell_unwind(n: u32, qty: Decimal) -> PlanAction {
    PlanAction::Unwind {
        unwind: n,
        venue: venue("A"),
        instrument: inst("BTC-A"),
        side: Action::Sell,
        quantity: q(qty),
        price: Some(p(dec!(99))),
    }
}
fn buy_unwind(n: u32, qty: Decimal) -> PlanAction {
    PlanAction::Unwind {
        unwind: n,
        venue: venue("B"),
        instrument: inst("BTC-B"),
        side: Action::Buy,
        quantity: q(qty),
        price: Some(p(dec!(100))),
    }
}
/// No actions, for `assert_eq!`.
fn no_actions() -> Vec<PlanAction> {
    Vec::new()
}
fn unwinds(actions: &[PlanAction]) -> Vec<PlanAction> {
    actions
        .iter()
        .filter(|a| matches!(a, PlanAction::Unwind { .. }))
        .cloned()
        .collect()
}

/// The buy fills 2 at 100 and the sell is rejected at 1006: residual +2.
fn long_residual(plan: &mut TwoLegPlan, b: &mut Budgets) -> Vec<PlanAction> {
    plan.apply_with_budgets(&update(A, view("A", "BTC-A", 1_004, dec!(99))), b)
        .unwrap();
    plan.apply_with_budgets(&fill(A, "a1", dec!(2), dec!(2), dec!(100), 1_005), b)
        .unwrap();
    plan.apply_with_budgets(&rejected(B, 1_006), b)
        .unwrap()
        .actions
}

#[test]
fn an_inventory_sell_unwind_waits_for_the_credited_inventory_and_a_tick() {
    let mut b = budgets(dec!(300), dec!(100));
    let mut plan = gated(&mut b, SellFunding::Inventory);
    // The plan never credits fills: the bought 2 BTC-A are not in A's budget.
    assert_eq!(unwinds(&long_residual(&mut plan, &mut b)), no_actions());
    assert_eq!(plan.state(), PlanState::UnwindRequested);
    assert_eq!(
        plan.unwind_hold(),
        Some(&AdmissionReject::InsufficientInventory {
            required: q(dec!(2)),
            available: dec!(0)
        })
    );
    // Ticks alone change nothing.
    let r = plan.apply_with_budgets(&tick(1_007), &mut b).unwrap();
    assert_eq!(unwinds(&r.actions), no_actions());
    assert!(plan.unwind_hold().is_some());
    // The caller credits what the leg bought, once, then ticks.
    credit(&mut b, "A", "BTC-A", dec!(2));
    let r = plan.apply_with_budgets(&tick(1_008), &mut b).unwrap();
    assert_eq!(unwinds(&r.actions), vec![sell_unwind(0, dec!(2))]);
    assert_eq!(plan.unwind_hold(), None);
    let reserved = plan.unwinds()[0].reservation.clone().unwrap();
    assert_eq!(
        reserved,
        Reservation {
            venue: venue("A"),
            cash: m(dec!(0.198)),
            inventory: Some((InstrumentId::new("BTC-A").unwrap(), q(dec!(2)))),
        }
    );
    let a = &b[&venue("A")];
    assert_eq!(a.reserved_cash(), m(dec!(200.398)));
    assert_eq!(
        a.available_inventory(&InstrumentId::new("BTC-A").unwrap())
            .unwrap(),
        dec!(0)
    );
    // Open residual: 2 - 2 covered = 0, which means requested, not flat.
    assert_eq!(plan.residual().unwrap(), dec!(2));
    assert_eq!(plan.unwind_coverage().unwrap(), dec!(2));
}

#[test]
fn a_margin_sell_unwind_and_a_cash_buy_unwind_reserve_on_their_own_venue() {
    // Long residual, sell on 20 % margin: 39.798 on A, admitted at once.
    let mut b = budgets(dec!(300), dec!(100));
    let mut plan = gated(&mut b, MARGIN_20);
    assert_eq!(
        unwinds(&long_residual(&mut plan, &mut b)),
        vec![sell_unwind(0, dec!(2))]
    );
    assert_eq!(
        plan.unwinds()[0].reservation.as_ref().unwrap().cash,
        m(dec!(39.798))
    );
    assert_eq!(b[&venue("A")].reserved_cash(), m(dec!(239.998)));
    assert_eq!(b[&venue("B")].reserved_cash(), m(dec!(40.602)));

    // Short residual: the sell fills and the buy is rejected; a buy of 2
    // BTC-B on B at its ask of 100 needs 200.2 in cash on B.
    for (b_cash, admitted) in [(dec!(240.802), true), (dec!(240.801), false)] {
        let mut b = budgets(dec!(300), b_cash);
        let mut plan = gated(&mut b, MARGIN_20);
        plan.apply_with_budgets(&update(B, view("B", "BTC-B", 1_004, dec!(101))), &mut b)
            .unwrap();
        plan.apply_with_budgets(&fill(B, "b1", dec!(2), dec!(2), dec!(101), 1_005), &mut b)
            .unwrap();
        let r = plan
            .apply_with_budgets(&rejected(A, 1_006), &mut b)
            .unwrap();
        if admitted {
            assert_eq!(unwinds(&r.actions), vec![buy_unwind(0, dec!(2))]);
            assert_eq!(plan.unwind_coverage().unwrap(), dec!(-2));
            assert_eq!(b[&venue("B")].available_cash().unwrap(), m(dec!(0)));
            // Capital is never borrowed from venue A.
            assert_eq!(b[&venue("A")].reserved_cash(), m(dec!(200.2)));
        } else {
            assert_eq!(unwinds(&r.actions), no_actions());
            assert!(matches!(
                plan.unwind_hold(),
                Some(AdmissionReject::InsufficientCash { .. })
            ));
        }
    }
}

#[test]
fn an_unwind_is_held_on_a_stale_view_a_missing_quote_and_a_disconnection() {
    let mut b = budgets(dec!(300), dec!(100));
    let mut plan = gated(&mut b, MARGIN_20);
    plan.apply_with_budgets(&update(A, view("A", "BTC-A", 1_004, dec!(99))), &mut b)
        .unwrap();
    plan.apply_with_budgets(&fill(A, "a1", dec!(2), dec!(2), dec!(100), 1_005), &mut b)
        .unwrap();
    plan.apply_with_budgets(&rejected(B, 1_060), &mut b)
        .unwrap();
    assert_eq!(
        plan.unwind_hold(),
        Some(&AdmissionReject::Stale {
            age_ms: 56,
            max_staleness_ms: 50
        })
    );
    let no_bid = VenueView {
        book: TopOfBook {
            bid: None,
            ask: Some(Level::new(p(dec!(100)), q(dec!(5)))),
        },
        ..view("A", "BTC-A", 1_061, dec!(99))
    };
    plan.apply_with_budgets(&update(A, no_bid), &mut b).unwrap();
    assert_eq!(plan.unwind_hold(), Some(&AdmissionReject::NoQuote));
    plan.apply_with_budgets(
        &LegExecution::Disconnected {
            leg: A,
            at: t(1_062),
        },
        &mut b,
    )
    .unwrap();
    assert_eq!(plan.unwind_hold(), Some(&AdmissionReject::Disconnected));
    plan.apply_with_budgets(
        &LegExecution::Reconnected {
            leg: A,
            at: t(1_063),
        },
        &mut b,
    )
    .unwrap();
    let r = plan
        .apply_with_budgets(&update(A, view("A", "BTC-A", 1_064, dec!(99))), &mut b)
        .unwrap();
    assert_eq!(unwinds(&r.actions), vec![sell_unwind(0, dec!(2))]);
}

#[test]
fn a_fill_event_never_admits_an_unwind() {
    let mut b = budgets(dec!(300), dec!(100));
    let mut plan = gated(&mut b, SellFunding::Inventory);
    long_residual(&mut plan, &mut b);
    credit(&mut b, "A", "BTC-A", dec!(3));
    // A late fill of the buy beyond its quantity: recorded, but no
    // admission on a fill.
    let r = plan
        .apply_with_budgets(&fill(A, "a2", dec!(1), dec!(3), dec!(100), 1_007), &mut b)
        .unwrap();
    assert_eq!(unwinds(&r.actions), no_actions());
    assert_eq!(plan.residual().unwrap(), dec!(3));
    // The next non-fill event admits the whole open residual.
    let r = plan.apply_with_budgets(&tick(1_008), &mut b).unwrap();
    assert_eq!(unwinds(&r.actions), vec![sell_unwind(0, dec!(3))]);
}

#[test]
fn a_rejected_or_partly_cancelled_unwind_is_requested_again() {
    let mut b = budgets(dec!(500), dec!(100));
    let mut plan = gated(&mut b, MARGIN_20);
    assert_eq!(
        unwinds(&long_residual(&mut plan, &mut b)),
        vec![sell_unwind(0, dec!(2))]
    );
    // Cancelled after executing 0.5: 1.5 is open again, with a new
    // reservation; the first one is held until release.
    let r = plan
        .apply_with_budgets(&done(A, 0, dec!(0.5), 1_007), &mut b)
        .unwrap();
    assert_eq!(unwinds(&r.actions), vec![sell_unwind(1, dec!(1.5))]);
    assert_eq!(plan.unwind_coverage().unwrap(), dec!(2));
    // 0.2 * 99 * 1.5 + 0.1485 = 29.8485 more on A.
    assert_eq!(b[&venue("A")].reserved_cash(), m(dec!(269.8465)));
    // Rejected: all 1.5 is requested again.
    let r = plan
        .apply_with_budgets(&done(A, 1, dec!(0), 1_008), &mut b)
        .unwrap();
    assert_eq!(unwinds(&r.actions), vec![sell_unwind(2, dec!(1.5))]);
    assert_eq!(plan.unwinds().len(), 3);
    assert!(plan.unwinds()[..2].iter().all(|u| !u.is_live()));
}

#[test]
fn a_terminal_unwind_keeps_its_reservation_so_a_retry_may_wait_for_reconciliation() {
    let mut b = budgets(dec!(300), dec!(100));
    let mut plan = gated(&mut b, SellFunding::Inventory);
    long_residual(&mut plan, &mut b);
    credit(&mut b, "A", "BTC-A", dec!(2));
    plan.apply_with_budgets(&tick(1_007), &mut b).unwrap();
    // The unwind was rejected: its 2 BTC-A stay reserved until release, so
    // the retry is held until the caller reconciles the budget.
    let r = plan
        .apply_with_budgets(&done(A, 0, dec!(0), 1_008), &mut b)
        .unwrap();
    assert_eq!(unwinds(&r.actions), no_actions());
    assert!(matches!(
        plan.unwind_hold(),
        Some(AdmissionReject::InsufficientInventory { .. })
    ));
}

#[test]
fn unwind_ends_are_idempotent_and_validated() {
    let mut b = budgets(dec!(500), dec!(100));
    let mut plan = gated(&mut b, MARGIN_20);
    long_residual(&mut plan, &mut b);
    plan.apply_with_budgets(&done(A, 0, dec!(2), 1_007), &mut b)
        .unwrap();
    let before = (plan.clone(), b.clone());
    // The same end again, even older, is a no-op.
    let r = plan
        .apply_with_budgets(&done(A, 0, dec!(2.00), 1_006), &mut b)
        .unwrap();
    assert!(r.duplicate);
    assert_eq!((plan.clone(), b.clone()), before);
    for bad in [
        done(A, 0, dec!(1), 1_008),
        done(A, 9, dec!(1), 1_008),
        done(B, 0, dec!(2), 1_008),
        done(A, 0, dec!(-1), 1_008),
    ] {
        assert!(matches!(
            plan.apply_with_budgets(&bad, &mut b),
            Err(ArbitrageError::InvalidInput { .. })
        ));
        assert_eq!((plan.clone(), b.clone()), before);
    }
}

#[test]
fn a_residual_that_falls_with_a_live_unwind_is_held_as_a_reversal() {
    let mut b = budgets(dec!(500), dec!(500));
    let mut plan = gated(&mut b, MARGIN_20);
    plan.apply_with_budgets(&update(A, view("A", "BTC-A", 1_004, dec!(99))), &mut b)
        .unwrap();
    plan.apply_with_budgets(&update(B, view("B", "BTC-B", 1_004, dec!(101))), &mut b)
        .unwrap();
    plan.apply_with_budgets(&fill(A, "a1", dec!(2), dec!(2), dec!(100), 1_005), &mut b)
        .unwrap();
    plan.apply_with_budgets(&fill(B, "b1", dec!(1), dec!(1), dec!(101), 1_005), &mut b)
        .unwrap();
    // The sell is cancelled at 1: residual +1, a sell unwind of 1.
    let r = plan
        .apply_with_budgets(
            &LegExecution::CancelAck {
                leg: B,
                attempt: 0,
                cum_qty: dec!(1),
                at: t(1_006),
            },
            &mut b,
        )
        .unwrap();
    assert_eq!(unwinds(&r.actions), vec![sell_unwind(0, dec!(1))]);
    // A late fill of the sell brings the residual to 0 (and then to -1):
    // the plan never sends the opposite side while the unwind is live.
    for (cum, at) in [(dec!(2), 1_007), (dec!(3), 1_008)] {
        plan.apply_with_budgets(
            &fill(B, &format!("b{cum}"), dec!(1), cum, dec!(101), at),
            &mut b,
        )
        .unwrap();
        let r = plan.apply_with_budgets(&tick(at + 1), &mut b).unwrap();
        assert_eq!(unwinds(&r.actions), no_actions());
        let open = plan.residual().unwrap() - dec!(1);
        assert_eq!(
            plan.unwind_hold(),
            Some(&AdmissionReject::UnwindReversal { open })
        );
    }
    // Ticks keep it held: the plan does not see an external cancel.
    plan.apply_with_budgets(&tick(1_020), &mut b).unwrap();
    assert!(matches!(
        plan.unwind_hold(),
        Some(AdmissionReject::UnwindReversal { .. })
    ));
    // The live unwind ended unfilled: open is -1, nothing is live, so a buy
    // of 1 BTC-B is admitted on B.
    let r = plan
        .apply_with_budgets(&done(A, 0, dec!(0), 1_021), &mut b)
        .unwrap();
    assert_eq!(unwinds(&r.actions), vec![buy_unwind(1, dec!(1))]);
    assert_eq!(plan.unwind_coverage().unwrap(), dec!(-1));
}

#[test]
fn release_is_refused_while_an_unwind_is_live_and_covers_the_unwinds() {
    let mut b = budgets(dec!(300), dec!(100));
    let initial = b.clone();
    let mut plan = gated(&mut b, MARGIN_20);
    long_residual(&mut plan, &mut b);
    let before = (plan.clone(), b.clone());
    assert!(matches!(
        plan.release_reservations(&mut b),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    assert_eq!((plan.clone(), b.clone()), before);
    plan.apply_with_budgets(&done(A, 0, dec!(2), 1_007), &mut b)
        .unwrap();
    plan.release_reservations(&mut b).unwrap();
    assert_eq!(b, initial);
    assert!(plan.unwinds()[0].reservation.is_none());
    // Idempotent.
    plan.release_reservations(&mut b).unwrap();
    assert_eq!(b, initial);
}

#[test]
fn after_release_no_unwind_is_sent_and_fills_are_still_recorded() {
    let mut b = budgets(dec!(300), dec!(100));
    let mut plan = gated(&mut b, SellFunding::Inventory);
    long_residual(&mut plan, &mut b);
    plan.release_reservations(&mut b).unwrap();
    credit(&mut b, "A", "BTC-A", dec!(5));
    let r = plan.apply_with_budgets(&tick(1_007), &mut b).unwrap();
    assert_eq!(unwinds(&r.actions), no_actions());
    assert_eq!(plan.unwind_hold(), Some(&AdmissionReject::Released));
    let r = plan
        .apply_with_budgets(&fill(A, "a2", dec!(1), dec!(3), dec!(100), 1_008), &mut b)
        .unwrap();
    assert_eq!(r.actions, no_actions());
    assert_eq!(plan.residual().unwrap(), dec!(3));
    plan.apply_with_budgets(&tick(1_009), &mut b).unwrap();
    assert_eq!(plan.unwind_hold(), Some(&AdmissionReject::Released));
}

#[test]
fn no_unwind_is_admitted_at_or_after_the_unwind_deadline() {
    let mut b = budgets(dec!(300), dec!(100));
    let mut plan = gated(&mut b, SellFunding::Inventory)
        .with_unwind_deadline(t(1_008))
        .unwrap();
    long_residual(&mut plan, &mut b);
    assert!(matches!(
        plan.unwind_hold(),
        Some(AdmissionReject::InsufficientInventory { .. })
    ));
    credit(&mut b, "A", "BTC-A", dec!(2));
    // At the deadline itself the unwind is refused, and it stays refused.
    for at in [1_008, 1_009] {
        let r = plan.apply_with_budgets(&tick(at), &mut b).unwrap();
        assert_eq!(unwinds(&r.actions), no_actions());
        assert_eq!(plan.unwind_hold(), Some(&AdmissionReject::PastDeadline));
    }
    assert_eq!(plan.residual().unwrap(), dec!(2));

    // Just before it, the same unwind is admitted.
    let mut b = budgets(dec!(300), dec!(100));
    credit(&mut b, "A", "BTC-A", dec!(2));
    let mut plan = gated(&mut b, SellFunding::Inventory)
        .with_unwind_deadline(t(1_007))
        .unwrap();
    assert_eq!(
        unwinds(&long_residual(&mut plan, &mut b)),
        vec![sell_unwind(0, dec!(2))]
    );
}

#[test]
fn an_unwind_deadline_needs_gating_and_a_fresh_plan() {
    let ungated = TwoLegPlan::new(
        leg_on("A", "BTC-A", Action::Buy, dec!(2), dec!(100)),
        leg_on("B", "BTC-B", Action::Sell, dec!(2), dec!(101)),
        FailurePolicy::Unwind,
        usd(),
        t(1_000),
    )
    .unwrap();
    assert!(matches!(
        ungated.with_unwind_deadline(t(2_000)),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    let mut b = budgets(dec!(300), dec!(100));
    let mut plan = gated(&mut b, MARGIN_20);
    plan.apply_with_budgets(&tick(1_001), &mut b).unwrap();
    assert!(matches!(
        plan.with_unwind_deadline(t(2_000)),
        Err(ArbitrageError::InvalidInput { .. })
    ));
}

#[test]
fn an_ungated_plan_requests_again_what_an_ended_unwind_left_open() {
    let mut plan = TwoLegPlan::new(
        leg_on("A", "BTC-A", Action::Buy, dec!(2), dec!(100)),
        leg_on("B", "BTC-B", Action::Sell, dec!(2), dec!(101)),
        FailurePolicy::Unwind,
        usd(),
        t(1_000),
    )
    .unwrap();
    plan.apply(&fill(A, "a1", dec!(2), dec!(2), dec!(100), 1_005))
        .unwrap();
    let r = plan.apply(&rejected(B, 1_006)).unwrap();
    let unpriced = |n, qty| PlanAction::Unwind {
        unwind: n,
        venue: venue("A"),
        instrument: inst("BTC-A"),
        side: Action::Sell,
        quantity: q(qty),
        price: None,
    };
    assert_eq!(r.actions, vec![unpriced(0, dec!(2))]);
    let r = plan.apply(&done(A, 0, dec!(1.25), 1_007)).unwrap();
    assert_eq!(r.actions, vec![unpriced(1, dec!(0.75))]);
}
