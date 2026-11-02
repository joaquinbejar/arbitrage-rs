//! Hand-checked fixtures for `venues`: per-venue views, budgets and
//! all-or-nothing admission.

// Tests fail loudly: the library's no-panic lints do not apply here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use arbitrage_rs::legs::{LegIndex, LegSpec};
use arbitrage_rs::venues::{
    Admission, AdmissionReject, LegRequest, Reservation, SellFunding, VenueBudget, VenueId,
    VenueView, admit_two_legs, check_view, requirement,
};
use arbitrage_rs::{
    Action, ArbitrageError, Currency, Decimal, FeeRate, Instrument, InstrumentId, Leg, Level,
    Money, Price, Quantity, Timestamp, TopOfBook,
};
use rust_decimal_macros::dec;
use std::collections::BTreeMap;

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
fn id(s: &str) -> InstrumentId {
    InstrumentId::new(s).unwrap()
}

/// Buy 2 BTC on venue A at a limit of 100; sell 2 BTC on venue B at a limit
/// of 101. Views at 990 quote 100 / 101 with size 5.
fn specs() -> (LegSpec, LegSpec) {
    (
        leg_on("A", "BTC-A", Action::Buy, dec!(100)),
        leg_on("B", "BTC-B", Action::Sell, dec!(101)),
    )
}

fn leg_on(v: &str, sym: &str, side: Action, price: Decimal) -> LegSpec {
    LegSpec {
        leg: Leg {
            instrument: Instrument::undated(id(sym)),
            side,
            quantity: q(dec!(2)),
            price: p(price),
        },
        deadline: None,
        venue: venue(v),
    }
}

fn view(v: &str, sym: &str, at: i64) -> VenueView {
    VenueView {
        venue: venue(v),
        instrument: Instrument::undated(id(sym)),
        observed_at: t(at),
        book: TopOfBook {
            bid: Some(Level::new(p(dec!(101)), q(dec!(5)))),
            ask: Some(Level::new(p(dec!(100)), q(dec!(5)))),
        },
    }
}

fn budgets(a_cash: Decimal, b_cash: Decimal) -> BTreeMap<VenueId, VenueBudget> {
    let mut map = BTreeMap::new();
    map.insert(venue("A"), VenueBudget::new(venue("A"), m(a_cash)).unwrap());
    map.insert(venue("B"), VenueBudget::new(venue("B"), m(b_cash)).unwrap());
    map
}

fn req<'a>(spec: &'a LegSpec, view: &'a VenueView, funding: SellFunding) -> LegRequest<'a> {
    LegRequest {
        spec,
        view,
        fee: FeeRate::from_bps(dec!(10)).unwrap(),
        sell_funding: funding,
    }
}

const MARGIN_20: SellFunding = SellFunding::Margin { rate: dec!(0.2) };

#[test]
fn both_legs_reserve_on_their_own_venue() {
    // Buy: 200 + 0.2 fee = 200.2 on A. Sell on margin: 202 * 0.2 = 40.4 +
    // 0.202 fee = 40.602 on B.
    let (buy, sell) = specs();
    let (va, vb) = (view("A", "BTC-A", 990), view("B", "BTC-B", 990));
    let mut b = budgets(dec!(500), dec!(100));
    let r = admit_two_legs(
        req(&buy, &va, MARGIN_20),
        req(&sell, &vb, MARGIN_20),
        &mut b,
        t(1_000),
        50,
    )
    .unwrap();
    assert_eq!(
        r,
        Admission::Admitted {
            first: Reservation {
                venue: venue("A"),
                cash: m(dec!(200.2)),
                inventory: None
            },
            second: Reservation {
                venue: venue("B"),
                cash: m(dec!(40.602)),
                inventory: None
            },
        }
    );
    assert_eq!(b[&venue("A")].available_cash().unwrap(), m(dec!(299.8)));
    assert_eq!(b[&venue("B")].reserved_cash(), m(dec!(40.602)));
    assert_eq!(b[&venue("B")].available_cash().unwrap(), m(dec!(59.398)));
}

#[test]
fn capital_is_not_borrowed_across_venues() {
    // A has 150 (needs 200.2) while B has plenty: the buy is rejected and
    // nothing is reserved anywhere.
    let (buy, sell) = specs();
    let (va, vb) = (view("A", "BTC-A", 990), view("B", "BTC-B", 990));
    let mut b = budgets(dec!(150), dec!(10_000));
    let before = b.clone();
    let r = admit_two_legs(
        req(&buy, &va, MARGIN_20),
        req(&sell, &vb, MARGIN_20),
        &mut b,
        t(1_000),
        50,
    )
    .unwrap();
    assert_eq!(
        r,
        Admission::Rejected {
            leg: LegIndex::First,
            reason: AdmissionReject::InsufficientCash {
                required: m(dec!(200.2)),
                available: m(dec!(150))
            },
        }
    );
    assert_eq!(b, before);
    // A venue with no budget admits nothing, even with cash elsewhere.
    let mut only_b = BTreeMap::new();
    only_b.insert(
        venue("B"),
        VenueBudget::new(venue("B"), m(dec!(10_000))).unwrap(),
    );
    let r = admit_two_legs(
        req(&buy, &va, MARGIN_20),
        req(&sell, &vb, MARGIN_20),
        &mut only_b,
        t(1_000),
        50,
    )
    .unwrap();
    assert_eq!(
        r,
        Admission::Rejected {
            leg: LegIndex::First,
            reason: AdmissionReject::NoBudget
        }
    );
}

#[test]
fn a_second_leg_rejection_releases_nothing_because_nothing_was_kept() {
    // The buy fits on A, the margin sell does not fit on B (needs 40.602,
    // has 40): all or nothing, A keeps all its cash.
    let (buy, sell) = specs();
    let (va, vb) = (view("A", "BTC-A", 990), view("B", "BTC-B", 990));
    let mut b = budgets(dec!(500), dec!(40));
    let r = admit_two_legs(
        req(&buy, &va, MARGIN_20),
        req(&sell, &vb, MARGIN_20),
        &mut b,
        t(1_000),
        50,
    )
    .unwrap();
    assert!(matches!(
        r,
        Admission::Rejected {
            leg: LegIndex::Second,
            reason: AdmissionReject::InsufficientCash { .. }
        }
    ));
    assert_eq!(b[&venue("A")].available_cash().unwrap(), m(dec!(500)));
}

#[test]
fn two_legs_on_one_venue_share_its_budget() {
    // Both legs on A: 200.2 + 40.602 = 240.802 needed. 240 is not enough.
    let buy = leg_on("A", "BTC-A", Action::Buy, dec!(100));
    let sell = leg_on("A", "BTC-B", Action::Sell, dec!(101));
    let va = view("A", "BTC-A", 990);
    let vb_on_a = view("A", "BTC-B", 990);
    let mut b = budgets(dec!(240), dec!(0));
    let r = admit_two_legs(
        req(&buy, &va, MARGIN_20),
        req(&sell, &vb_on_a, MARGIN_20),
        &mut b,
        t(1_000),
        50,
    )
    .unwrap();
    assert_eq!(
        r,
        Admission::Rejected {
            leg: LegIndex::Second,
            reason: AdmissionReject::InsufficientCash {
                required: m(dec!(40.602)),
                available: m(dec!(39.8))
            },
        }
    );
    let mut b = budgets(dec!(240.802), dec!(0));
    assert!(matches!(
        admit_two_legs(
            req(&buy, &va, MARGIN_20),
            req(&sell, &vb_on_a, MARGIN_20),
            &mut b,
            t(1_000),
            50
        )
        .unwrap(),
        Admission::Admitted { .. }
    ));
    assert!(b[&venue("A")].available_cash().unwrap().is_zero());
}

#[test]
fn selling_from_inventory_reserves_the_quantity() {
    let (buy, sell) = specs();
    let (va, vb) = (view("A", "BTC-A", 990), view("B", "BTC-B", 990));
    let mut b = budgets(dec!(500), dec!(1));
    let with_inv = b[&venue("B")]
        .clone()
        .with_inventory(id("BTC-B"), q(dec!(3)))
        .unwrap();
    b.insert(venue("B"), with_inv);
    let r = admit_two_legs(
        req(&buy, &va, SellFunding::Inventory),
        req(&sell, &vb, SellFunding::Inventory),
        &mut b,
        t(1_000),
        50,
    )
    .unwrap();
    let Admission::Admitted { second, .. } = r else {
        panic!("{r:?}")
    };
    // Only the fee in cash, and 2 BTC-B of inventory.
    assert_eq!(second.cash, m(dec!(0.202)));
    assert_eq!(second.inventory, Some((id("BTC-B"), q(dec!(2)))));
    assert_eq!(
        b[&venue("B")].available_inventory(&id("BTC-B")).unwrap(),
        dec!(1)
    );
    // A second plan cannot reuse the reserved inventory.
    let r = admit_two_legs(
        req(&buy, &va, SellFunding::Inventory),
        req(&sell, &vb, SellFunding::Inventory),
        &mut b,
        t(1_000),
        50,
    )
    .unwrap();
    assert_eq!(
        r,
        Admission::Rejected {
            leg: LegIndex::Second,
            reason: AdmissionReject::InsufficientInventory {
                required: q(dec!(2)),
                available: dec!(1)
            },
        }
    );
    // Releasing the reservation frees it again.
    b.get_mut(&venue("B")).unwrap().release(&second).unwrap();
    assert_eq!(
        b[&venue("B")].available_inventory(&id("BTC-B")).unwrap(),
        dec!(3)
    );
    assert!(b[&venue("B")].reserved_cash().is_zero());
}

#[test]
fn stale_future_unquoted_and_thin_views_are_rejected() {
    let (buy, sell) = specs();
    let vb = view("B", "BTC-B", 990);
    let reject = |va: VenueView| {
        let mut b = budgets(dec!(500), dec!(100));
        admit_two_legs(
            req(&buy, &va, MARGIN_20),
            req(&sell, &vb, MARGIN_20),
            &mut b,
            t(1_000),
            50,
        )
    };
    // 1000 - 949 = 51 ms > 50.
    assert_eq!(
        reject(view("A", "BTC-A", 949)).unwrap(),
        Admission::Rejected {
            leg: LegIndex::First,
            reason: AdmissionReject::Stale {
                age_ms: 51,
                max_staleness_ms: 50
            }
        }
    );
    assert!(matches!(
        reject(view("A", "BTC-A", 950)).unwrap(),
        Admission::Admitted { .. }
    ));
    assert!(matches!(
        reject(view("A", "BTC-A", 1_001)),
        Err(ArbitrageError::LookAhead { .. })
    ));
    let mut no_ask = view("A", "BTC-A", 990);
    no_ask.book.ask = None;
    assert_eq!(
        reject(no_ask).unwrap(),
        Admission::Rejected {
            leg: LegIndex::First,
            reason: AdmissionReject::NoQuote
        }
    );
    let mut dear = view("A", "BTC-A", 990);
    dear.book.ask = Some(Level::new(p(dec!(100.5)), q(dec!(5))));
    assert_eq!(
        reject(dear).unwrap(),
        Admission::Rejected {
            leg: LegIndex::First,
            reason: AdmissionReject::PriceBeyondLimit
        }
    );
    let mut thin = view("A", "BTC-A", 990);
    thin.book.ask = Some(Level::new(p(dec!(100)), q(dec!(1))));
    assert_eq!(
        reject(thin).unwrap(),
        Admission::Rejected {
            leg: LegIndex::First,
            reason: AdmissionReject::InsufficientSize {
                available: q(dec!(1))
            }
        }
    );
    assert!(matches!(
        reject(view("A", "OTHER", 990)),
        Err(ArbitrageError::InvalidInput { .. })
    ));
}

#[test]
fn margin_rate_and_budget_are_validated() {
    let (buy, sell) = specs();
    let (va, vb) = (view("A", "BTC-A", 990), view("B", "BTC-B", 990));
    let mut b = budgets(dec!(500), dec!(100));
    for rate in [dec!(0), dec!(1.01)] {
        assert!(matches!(
            admit_two_legs(
                req(&buy, &va, MARGIN_20),
                req(&sell, &vb, SellFunding::Margin { rate }),
                &mut b,
                t(1_000),
                50
            ),
            Err(ArbitrageError::InvalidValue { .. })
        ));
    }
    assert!(VenueBudget::new(venue("A"), m(dec!(-1))).is_err());
    assert!(VenueId::new("").is_err());
    assert!(VenueId::new("BIN ANCE").is_err());
}

#[test]
fn a_view_from_another_venue_never_funds_the_leg() {
    // The sell leg is on B; a view from A (where cash
    // is plentiful) must not admit it or reserve A's capital.
    let (buy, sell) = specs();
    let va = view("A", "BTC-A", 990);
    let vb_from_a = view("A", "BTC-B", 990);
    let mut b = budgets(dec!(10_000), dec!(0));
    let before = b.clone();
    assert!(matches!(
        admit_two_legs(
            req(&buy, &va, MARGIN_20),
            req(&sell, &vb_from_a, MARGIN_20),
            &mut b,
            t(1_000),
            50
        ),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    assert_eq!(b, before);
    // A leg without a venue cannot be expressed: `LegSpec::venue` is
    // required.
}

#[test]
fn expired_or_past_deadline_legs_reserve_nothing() {
    // The buy is a future expiring at 900; at 1000 it is
    // expired and nothing is reserved. A deadline at 1000 is past too.
    let (mut buy, sell) = specs();
    let (va, vb) = (view("A", "BTC-A", 990), view("B", "BTC-B", 990));
    buy.leg.instrument = Instrument::dated(id("BTC-A"), t(900));
    let mut dated_view = va.clone();
    dated_view.instrument = buy.leg.instrument.clone();
    let mut b = budgets(dec!(500), dec!(100));
    let before = b.clone();
    assert!(matches!(
        admit_two_legs(
            req(&buy, &dated_view, MARGIN_20),
            req(&sell, &vb, MARGIN_20),
            &mut b,
            t(1_000),
            50
        ),
        Err(ArbitrageError::Expired { .. })
    ));
    assert_eq!(b, before);
    let (mut buy, sell) = specs();
    buy.deadline = Some(t(1_000));
    assert_eq!(
        admit_two_legs(
            req(&buy, &va, MARGIN_20),
            req(&sell, &vb, MARGIN_20),
            &mut b,
            t(1_000),
            50
        )
        .unwrap(),
        Admission::Rejected {
            leg: LegIndex::First,
            reason: AdmissionReject::PastDeadline
        }
    );
    assert_eq!(b, before);
}

#[test]
fn a_sell_is_reserved_at_the_higher_of_its_limit_and_the_bid() {
    // Sell 2 with a limit of 101 while the bid is 1000.
    // Margin 20%: 2000 * 0.2 = 400 + fee 2 = 402, not 40.4 + 0.202.
    let (buy, sell) = specs();
    let va = view("A", "BTC-A", 990);
    let mut vb = view("B", "BTC-B", 990);
    vb.book.bid = Some(Level::new(p(dec!(1000)), q(dec!(5))));
    let mut b = budgets(dec!(500), dec!(41));
    assert_eq!(
        admit_two_legs(
            req(&buy, &va, MARGIN_20),
            req(&sell, &vb, MARGIN_20),
            &mut b,
            t(1_000),
            50
        )
        .unwrap(),
        Admission::Rejected {
            leg: LegIndex::Second,
            reason: AdmissionReject::InsufficientCash {
                required: m(dec!(402)),
                available: m(dec!(41))
            },
        }
    );
    // From inventory the fee is also on the 2000 notional: 2.
    let inv = b[&venue("B")]
        .clone()
        .with_inventory(id("BTC-B"), q(dec!(2)))
        .unwrap();
    b.insert(venue("B"), inv);
    let r = admit_two_legs(
        req(&buy, &va, MARGIN_20),
        req(&sell, &vb, SellFunding::Inventory),
        &mut b,
        t(1_000),
        50,
    )
    .unwrap();
    let Admission::Admitted { second, .. } = r else {
        panic!("{r:?}")
    };
    assert_eq!(second.cash, m(dec!(2)));
    // Rollback and release still hold.
    b.get_mut(&venue("B")).unwrap().release(&second).unwrap();
    assert!(b[&venue("B")].reserved_cash().is_zero());
    assert_eq!(
        b[&venue("B")].available_inventory(&id("BTC-B")).unwrap(),
        dec!(2)
    );
}

#[test]
fn margin_requirement_is_exact_when_the_notional_passes_max() {
    // A sell of 2 at 0.6 * Decimal::MAX: the notional passes Decimal::MAX,
    // the 50 % margin (0.6 * MAX) fits.
    let big = dec!(47536897508558602556126370201);
    let spec = leg_on("B", "BTC-B", Action::Sell, big);
    let v = view("B", "BTC-B", 990);
    let request = LegRequest {
        spec: &spec,
        view: &v,
        fee: FeeRate::zero(),
        sell_funding: SellFunding::Margin { rate: dec!(0.5) },
    };
    let need = arbitrage_rs::venues::requirement(&request, usd()).unwrap();
    assert_eq!(need.cash.amount(), big);
}

#[test]
fn an_unsupported_action_is_never_admitted_and_reserves_nothing() {
    let (buy, _) = specs();
    let other = leg_on("B", "BTC-B", Action::Other, dec!(101));
    let (va, vb) = (view("A", "BTC-A", 990), view("B", "BTC-B", 990));
    let unsupported = ArbitrageError::UnsupportedAction {
        action: Action::Other,
    };
    assert_eq!(
        check_view(&other, &vb, t(1_000), 50).unwrap_err(),
        unsupported
    );
    assert_eq!(
        requirement(&req(&other, &vb, MARGIN_20), usd()).unwrap_err(),
        unsupported
    );
    // The first leg would be admitted; the budgets stay untouched.
    let mut b = budgets(dec!(500), dec!(100));
    let before = b.clone();
    let result = admit_two_legs(
        req(&buy, &va, MARGIN_20),
        req(&other, &vb, MARGIN_20),
        &mut b,
        t(1_000),
        50,
    );
    assert_eq!(result.unwrap_err(), unsupported);
    assert_eq!(b, before);
}
