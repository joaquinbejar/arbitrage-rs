//! Hand-checked fixtures for `hedge::plan_band_hedge`.

// Tests fail loudly: the library's no-panic lints do not apply here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use arbitrage_rs::hedge::{Band, HedgeDecision, NoHedgeReason, PositionView, plan_band_hedge};
use arbitrage_rs::{
    Action, ArbitrageError, Cost, Currency, Decimal, FeeRate, Instrument, InstrumentId, Level,
    Money, Price, Quantity, Timestamp, TopOfBook,
};
use rust_decimal_macros::dec;

fn usd() -> Currency {
    Currency::new("USD").unwrap()
}
fn m(x: Decimal) -> Money {
    Money::new(x, usd())
}
fn q(x: Decimal) -> Quantity {
    Quantity::new(x).unwrap()
}
fn lvl(p: Decimal, s: Decimal) -> Level {
    Level::new(Price::new(p).unwrap(), q(s))
}

/// Band [-10, 10] to 0; hedge on a dated future quoted 99.5 / 100.5 with 100
/// on each side, 5 bps fee, no lot size, no cap.
fn view(exposure: Decimal) -> PositionView {
    PositionView {
        as_of: Timestamp::from_millis(1_000),
        quote_currency: usd(),
        exposure,
        band: Band::symmetric(dec!(10)).unwrap(),
        instrument: Instrument::dated(
            InstrumentId::new("BTC-F-DEC").unwrap(),
            Timestamp::from_millis(10_000),
        ),
        book: TopOfBook {
            bid: Some(lvl(dec!(99.5), dec!(100))),
            ask: Some(lvl(dec!(100.5), dec!(100))),
        },
        fee: FeeRate::from_bps(dec!(5)).unwrap(),
        lot_size: None,
        max_quantity: None,
    }
}

fn plan(v: &PositionView) -> arbitrage_rs::hedge::HedgePlan {
    plan_band_hedge(v).unwrap().plan().cloned().expect("a plan")
}

#[test]
fn no_hedge_inside_the_band_edges_included() {
    for x in [dec!(0), dec!(5), dec!(-7.5), dec!(10), dec!(-10)] {
        assert_eq!(
            plan_band_hedge(&view(x)).unwrap(),
            HedgeDecision::NoHedge {
                reason: NoHedgeReason::InsideBand,
                exposure: x
            },
            "{x}"
        );
    }
}

#[test]
fn above_the_band_sells_back_to_target() {
    // Sell 12.5 at 99.5: notional 1243.75, fee 0.621875.
    // Mid 100, spread cost 0.5 * 12.5 = 6.25; total 6.871875.
    let p = plan(&view(dec!(12.5)));
    assert_eq!(p.leg.side, Action::Sell);
    assert_eq!(p.leg.quantity, q(dec!(12.5)));
    assert_eq!(p.leg.price.value(), dec!(99.5));
    assert_eq!(
        p.leg.instrument.expiry,
        Some(Timestamp::from_millis(10_000))
    );
    assert_eq!(p.exposure_before, dec!(12.5));
    assert_eq!(p.required_quantity, q(dec!(12.5)));
    assert_eq!(p.expected_exposure_after, dec!(0));
    assert_eq!(p.expected_fee.money(), m(dec!(0.621875)));
    assert_eq!(p.expected_spread_cost, Some(Cost::new(m(dec!(6.25)))));
    assert_eq!(p.expected_cost.money(), m(dec!(6.871875)));
}

#[test]
fn below_the_band_buys_whole_lots_without_overshooting() {
    // Required 12.34, lot 0.1 -> 12.3. Buy at 100.5: notional 1236.15,
    // fee 0.618075, spread 0.5 * 12.3 = 6.15, total 6.768075; after -0.04.
    let mut v = view(dec!(-12.34));
    v.lot_size = Some(q(dec!(0.1)));
    let p = plan(&v);
    assert_eq!(p.leg.side, Action::Buy);
    assert_eq!(p.leg.price.value(), dec!(100.5));
    assert_eq!(p.required_quantity, q(dec!(12.34)));
    assert_eq!(p.leg.quantity, q(dec!(12.3)));
    assert_eq!(p.expected_exposure_after, dec!(-0.04));
    assert_eq!(p.expected_fee.money(), m(dec!(0.618075)));
    assert_eq!(p.expected_cost.money(), m(dec!(6.768075)));
}

#[test]
fn asymmetric_band_hedges_to_its_target() {
    let mut v = view(dec!(9));
    v.band = Band::new(dec!(-5), dec!(2), dec!(8)).unwrap();
    let p = plan(&v);
    assert_eq!(p.leg.side, Action::Sell);
    assert_eq!(p.leg.quantity, q(dec!(7)));
    assert_eq!(p.expected_exposure_after, dec!(2));
}

#[test]
fn quoted_size_and_max_quantity_cap_the_hedge() {
    let mut v = view(dec!(12.5));
    v.book.bid = Some(lvl(dec!(99.5), dec!(4)));
    let p = plan(&v);
    assert_eq!(p.leg.quantity, q(dec!(4)));
    assert_eq!(p.required_quantity, q(dec!(12.5)));
    assert_eq!(p.expected_exposure_after, dec!(8.5));

    v.max_quantity = Some(q(dec!(3)));
    let p = plan(&v);
    assert_eq!(p.leg.quantity, q(dec!(3)));
    assert_eq!(p.expected_exposure_after, dec!(9.5));
}

#[test]
fn no_liquidity_keeps_the_exposure() {
    let mut v = view(dec!(12.5));
    v.book.bid = None;
    assert_eq!(
        plan_band_hedge(&v).unwrap(),
        HedgeDecision::NoHedge {
            reason: NoHedgeReason::NoLiquidity { side: Action::Sell },
            exposure: dec!(12.5)
        }
    );
}

#[test]
fn below_one_lot_keeps_the_exposure() {
    let mut v = view(dec!(12.5));
    v.book.bid = Some(lvl(dec!(99.5), dec!(0.5)));
    v.lot_size = Some(q(dec!(1)));
    assert_eq!(
        plan_band_hedge(&v).unwrap(),
        HedgeDecision::NoHedge {
            reason: NoHedgeReason::BelowLotSize {
                executable: q(dec!(0.5)),
                lot_size: q(dec!(1)),
            },
            exposure: dec!(12.5)
        }
    );
}

#[test]
fn one_sided_book_has_no_spread_cost() {
    let mut v = view(dec!(12.5));
    v.book.ask = None;
    let p = plan(&v);
    assert_eq!(p.expected_spread_cost, None);
    assert_eq!(p.expected_cost, p.expected_fee);
}

#[test]
fn expired_hedge_instrument_is_an_error() {
    let mut v = view(dec!(12.5));
    v.as_of = Timestamp::from_millis(10_000);
    assert!(matches!(
        plan_band_hedge(&v),
        Err(ArbitrageError::Expired { .. })
    ));
}

#[test]
fn band_validation() {
    assert!(Band::new(dec!(1), dec!(1), dec!(0)).is_err());
    assert!(Band::new(dec!(-1), dec!(2), dec!(1)).is_err());
    assert!(Band::symmetric(dec!(-1)).is_err());
    let b = Band::symmetric(dec!(0)).unwrap();
    assert!(b.contains(dec!(0)));
    assert!(!b.contains(dec!(0.0000001)));
}

/// The same exposure path with and without band hedging. Each plan is
/// applied as if filled at its quoted price, which is the plan's own
/// expectation and not a fill model.
///
/// Increments: +4, +4, +4, -3, +6, +5, -20.
/// - No hedge: 4, 8, 12, 9, 15, 20, 0. Max |exposure| 20, cost 0.
/// - Hedged: 4, 8, 12 -> sell 12 -> 0, -3, 3, 8, -12 -> buy 12 -> 0.
///   Max |exposure| after hedging 8. Costs: sell 12 at 99.5 (fee 0.597,
///   spread 6) and buy 12 at 100.5 (fee 0.603, spread 6): 13.2 in total.
#[test]
fn band_hedge_against_no_hedge_baseline() {
    let increments = [
        dec!(4),
        dec!(4),
        dec!(4),
        dec!(-3),
        dec!(6),
        dec!(5),
        dec!(-20),
    ];

    let mut baseline = Decimal::ZERO;
    let mut baseline_max = Decimal::ZERO;
    let mut hedged = Decimal::ZERO;
    let mut hedged_max = Decimal::ZERO;
    let mut hedge_plans = Vec::new();
    let mut total_cost = Cost::new(m(dec!(0)));

    for dx in increments {
        baseline += dx;
        baseline_max = baseline_max.max(baseline.abs());

        hedged += dx;
        if let HedgeDecision::Hedge(p) = plan_band_hedge(&view(hedged)).unwrap() {
            total_cost = total_cost.checked_add(p.expected_cost).unwrap();
            hedged = p.expected_exposure_after;
            hedge_plans.push((p.leg.side, p.leg.quantity));
        }
        hedged_max = hedged_max.max(hedged.abs());
    }

    assert_eq!(baseline, dec!(0));
    assert_eq!(baseline_max, dec!(20));
    assert_eq!(hedged_max, dec!(8));
    assert_eq!(
        hedge_plans,
        vec![(Action::Sell, q(dec!(12))), (Action::Buy, q(dec!(12)))]
    );
    assert_eq!(total_cost.money(), m(dec!(13.2)));
}

#[test]
fn spread_cost_and_fee_are_exact_near_max() {
    // bid + ask passes Decimal::MAX and so does the notional of 10 at the
    // bid; the half spread (1 * 10) and the 1 bps fee fit.
    let big = dec!(47536897508558602556126370201);
    let mut v = view(dec!(20));
    v.book = TopOfBook {
        bid: Some(lvl(big, dec!(100))),
        ask: Some(lvl(big + dec!(2), dec!(100))),
    };
    v.fee = FeeRate::from_bps(dec!(1)).unwrap();
    v.max_quantity = Some(q(dec!(10)));
    let h = plan(&v);
    assert_eq!(h.leg.side, Action::Sell);
    assert_eq!(h.expected_spread_cost, Some(Cost::new(m(dec!(10)))));
    assert_eq!(
        h.expected_fee.money().amount(),
        dec!(47536897508558602556126370.201)
    );
    assert_eq!(
        h.expected_cost.money().amount(),
        dec!(47536897508558602556126380.201)
    );
}
