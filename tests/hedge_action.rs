//! Hand-checked fixtures for `hedge_action::hedge_plan_from_action`.

use arbitrage_rs::hedge_action::{
    ActionMarket, ActionOutcome, ActionQuantity, Adjustment, ExcessPolicy, HedgeAction,
    HedgeLimits, NoPlanReason, hedge_plan_from_action,
};
use arbitrage_rs::{
    ArbitrageError, Cost, Currency, Decimal, FeeRate, Instrument, InstrumentId, Level, Money,
    Price, Quantity, Side, Timestamp, TopOfBook,
};
use rust_decimal_macros::dec;

fn usd() -> Currency {
    Currency::new("USD").unwrap()
}
fn q(x: Decimal) -> Quantity {
    Quantity::new(x).unwrap()
}
fn lvl(px: Decimal, s: Decimal) -> Option<Level> {
    Some(Level::new(Price::new(px).unwrap(), q(s)))
}

/// A dated future quoted 99.5 / 100.5 with 100 a side, 5 bps fee, current
/// exposure +12.5.
fn market() -> ActionMarket {
    ActionMarket {
        quote_currency: usd(),
        book: TopOfBook {
            bid: lvl(dec!(99.5), dec!(100)),
            ask: lvl(dec!(100.5), dec!(100)),
        },
        fee: FeeRate::from_bps(dec!(5)).unwrap(),
        exposure: dec!(12.5),
    }
}

fn limits(on_excess: ExcessPolicy) -> HedgeLimits {
    HedgeLimits {
        max_quantity: q(dec!(20)),
        lot_size: Some(q(dec!(0.1))),
        float_dp: 8,
        on_excess,
    }
}

fn action(side: Side, quantity: ActionQuantity) -> HedgeAction {
    HedgeAction {
        as_of: Timestamp::from_millis(1_000),
        instrument: Instrument::dated(
            InstrumentId::new("BTC-F-DEC").unwrap(),
            Timestamp::from_millis(10_000),
        ),
        side,
        quantity,
    }
}

fn plan(
    a: &HedgeAction,
    l: &HedgeLimits,
    m: &ActionMarket,
) -> arbitrage_rs::hedge_action::ActionPlan {
    hedge_plan_from_action(a, l, m)
        .unwrap()
        .plan()
        .cloned()
        .expect("a plan")
}

#[test]
fn an_exact_action_becomes_the_same_plan_as_the_band_hedger() {
    // Sell 12.5 at 99.5: fee 1243.75 * 0.0005 = 0.621875, spread vs mid
    // 0.5 * 12.5 = 6.25, cost 6.871875; exposure 12.5 -> 0.
    let a = action(Side::Sell, ActionQuantity::Exact(dec!(12.5)));
    let p = plan(&a, &limits(ExcessPolicy::Clip), &market());
    assert_eq!(p.requested, dec!(12.5));
    assert_eq!(p.feasible, q(dec!(12.5)));
    assert!(p.adjustments.is_empty());
    assert_eq!(p.plan.leg.side, Side::Sell);
    assert_eq!(p.plan.leg.price.value(), dec!(99.5));
    assert_eq!(
        p.plan.leg.instrument.expiry,
        Some(Timestamp::from_millis(10_000))
    );
    assert_eq!(p.plan.exposure_before, dec!(12.5));
    assert_eq!(p.plan.expected_exposure_after, dec!(0));
    assert_eq!(
        p.plan.expected_fee,
        Cost::new(Money::new(dec!(0.621875), usd()))
    );
    assert_eq!(p.plan.expected_cost.money().amount(), dec!(6.871875));
}

#[test]
fn out_of_range_actions_cannot_create_unbounded_plans() {
    // 1e6 asked: clipped to 20 and recorded.
    let a = action(Side::Buy, ActionQuantity::Exact(dec!(1000000)));
    let p = plan(&a, &limits(ExcessPolicy::Clip), &market());
    assert_eq!(p.requested, dec!(1000000));
    assert_eq!(p.feasible, q(dec!(20)));
    assert_eq!(p.adjustments, vec![Adjustment::MaxQuantity]);
    assert_eq!(p.plan.expected_exposure_after, dec!(32.5));
    // Under Reject the same action is an error.
    assert!(matches!(
        hedge_plan_from_action(&a, &limits(ExcessPolicy::Reject), &market()),
        Err(ArbitrageError::InvalidValue {
            field: "action quantity",
            ..
        })
    ));
    // The largest finite float within Decimal's range is still clipped.
    let huge = action(Side::Buy, ActionQuantity::Float(7.9e28));
    let p = plan(&huge, &limits(ExcessPolicy::Clip), &market());
    assert_eq!(p.feasible, q(dec!(20)));
}

#[test]
fn unsupported_float_values_are_errors() {
    for x in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e40, -1e40] {
        assert!(
            matches!(
                hedge_plan_from_action(
                    &action(Side::Buy, ActionQuantity::Float(x)),
                    &limits(ExcessPolicy::Clip),
                    &market()
                ),
                Err(ArbitrageError::InvalidInput { .. })
            ),
            "{x}"
        );
    }
    let mut bad = limits(ExcessPolicy::Clip);
    bad.float_dp = 29;
    assert!(matches!(
        hedge_plan_from_action(
            &action(Side::Buy, ActionQuantity::Float(1.0)),
            &bad,
            &market()
        ),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    assert!(matches!(
        hedge_plan_from_action(
            &action(Side::Buy, ActionQuantity::Exact(dec!(-1))),
            &limits(ExcessPolicy::Clip),
            &market()
        ),
        Err(ArbitrageError::InvalidValue { .. })
    ));
}

#[test]
fn float_actions_are_rounded_once_and_recorded() {
    // 12.34 as f64 is 12.3399999999999998578...; half-even at 8 places is
    // 12.34, then lots of 0.1 give 12.3.
    let a = action(Side::Sell, ActionQuantity::Float(12.34));
    let p = plan(&a, &limits(ExcessPolicy::Clip), &market());
    assert_eq!(p.requested, dec!(12.34));
    assert_eq!(p.feasible, q(dec!(12.3)));
    assert_eq!(
        p.adjustments,
        vec![Adjustment::FloatRounded, Adjustment::Lot]
    );
    // A dyadic float is exact: no rounding recorded.
    let p = plan(
        &action(Side::Sell, ActionQuantity::Float(12.5)),
        &limits(ExcessPolicy::Clip),
        &market(),
    );
    assert!(p.adjustments.is_empty());
    assert_eq!(p.feasible, q(dec!(12.5)));
}

#[test]
fn quoted_size_and_lot_bound_the_plan() {
    let mut m = market();
    m.book.bid = lvl(dec!(99.5), dec!(4.05));
    let p = plan(
        &action(Side::Sell, ActionQuantity::Exact(dec!(12.5))),
        &limits(ExcessPolicy::Clip),
        &m,
    );
    assert_eq!(p.feasible, q(dec!(4)));
    assert_eq!(p.adjustments, vec![Adjustment::QuotedSize, Adjustment::Lot]);
    assert_eq!(p.plan.required_quantity, q(dec!(12.5)));
    assert_eq!(p.plan.expected_exposure_after, dec!(8.5));
}

#[test]
fn no_plan_cases_keep_the_exposure() {
    let l = limits(ExcessPolicy::Clip);
    assert_eq!(
        hedge_plan_from_action(
            &action(Side::Sell, ActionQuantity::Exact(dec!(0))),
            &l,
            &market()
        )
        .unwrap(),
        ActionOutcome::NoPlan {
            requested: dec!(0),
            reason: NoPlanReason::ZeroQuantity
        }
    );
    // A float below the representable precision is zero too.
    assert!(matches!(
        hedge_plan_from_action(
            &action(Side::Sell, ActionQuantity::Float(1e-30)),
            &l,
            &market()
        )
        .unwrap(),
        ActionOutcome::NoPlan {
            reason: NoPlanReason::ZeroQuantity,
            ..
        }
    ));
    let mut m = market();
    m.book.bid = None;
    assert_eq!(
        hedge_plan_from_action(&action(Side::Sell, ActionQuantity::Exact(dec!(1))), &l, &m)
            .unwrap(),
        ActionOutcome::NoPlan {
            requested: dec!(1),
            reason: NoPlanReason::NoLiquidity
        }
    );
    assert_eq!(
        hedge_plan_from_action(
            &action(Side::Sell, ActionQuantity::Exact(dec!(0.05))),
            &l,
            &market()
        )
        .unwrap(),
        ActionOutcome::NoPlan {
            requested: dec!(0.05),
            reason: NoPlanReason::BelowLotSize
        }
    );
}

#[test]
fn the_conversion_posts_nothing_and_claims_no_execution() {
    // Inputs are borrowed immutably and unchanged; the plan only carries
    // expected amounts and an intended leg.
    let a = action(Side::Buy, ActionQuantity::Exact(dec!(2)));
    let l = limits(ExcessPolicy::Clip);
    let m = market();
    let (a0, m0) = (a.clone(), m.clone());
    let p = plan(&a, &l, &m);
    assert_eq!(a, a0);
    assert_eq!(m, m0);
    assert_eq!(m.exposure, dec!(12.5));
    assert_eq!(p.plan.exposure_before, dec!(12.5));
    assert_eq!(p.plan.expected_exposure_after, dec!(14.5));
}

#[test]
fn an_expired_instrument_is_an_error() {
    let mut a = action(Side::Sell, ActionQuantity::Exact(dec!(1)));
    a.as_of = Timestamp::from_millis(10_000);
    assert!(matches!(
        hedge_plan_from_action(&a, &limits(ExcessPolicy::Clip), &market()),
        Err(ArbitrageError::Expired { .. })
    ));
}
