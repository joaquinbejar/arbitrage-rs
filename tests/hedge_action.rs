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
        reduce_only: false,
        exposure_cap: None,
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
    for x in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e40] {
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

fn requested_of(x: f64, dp: u32) -> Result<Decimal, ArbitrageError> {
    let l = HedgeLimits {
        max_quantity: q(Decimal::MAX),
        lot_size: None,
        float_dp: dp,
        on_excess: ExcessPolicy::Clip,
        reduce_only: false,
        exposure_cap: None,
    };
    let mut m = market();
    m.book.ask = lvl(dec!(1), Decimal::MAX);
    m.book.bid = None;
    m.fee = FeeRate::zero();
    // Zero exposure so that tiny quantities stay representable after it.
    m.exposure = Decimal::ZERO;
    match hedge_plan_from_action(&action(Side::Buy, ActionQuantity::Float(x)), &l, &m)? {
        ActionOutcome::Plan(p) => Ok(p.requested),
        ActionOutcome::NoPlan { requested, .. } => Ok(requested),
    }
}

#[test]
fn negative_floats_are_rejected_even_when_tiny() {
    // Codex M404-C1: checked on the float, before any rounding.
    for x in [-1e-9, -1e-100, -5e-324, -1.0, -1e40] {
        assert!(
            matches!(requested_of(x, 8), Err(ArbitrageError::InvalidValue { .. })),
            "{x}"
        );
    }
    // -0.0 is zero.
    assert_eq!(requested_of(-0.0, 8).unwrap(), dec!(0));
}

#[test]
fn floats_are_rounded_once_from_their_exact_binary_value() {
    // Codex M404-C2: 1.4999999999999999e-27 is 1.4999999999999998780e-27 in
    // binary, below the tie, so 27 places give 1e-27 (not 2e-27 as a double
    // rounding through 28 digits would).
    assert_eq!(
        requested_of(1.4999999999999999e-27, 27).unwrap(),
        dec!(0.000000000000000000000000001)
    );
    // Exact ties go to even; their neighbours go to the nearer side.
    assert_eq!(requested_of(0.125, 2).unwrap(), dec!(0.12));
    assert_eq!(
        requested_of(f64::from_bits(0.125f64.to_bits() + 1), 2).unwrap(),
        dec!(0.13)
    );
    assert_eq!(requested_of(0.375, 2).unwrap(), dec!(0.38));
    assert_eq!(
        requested_of(f64::from_bits(0.375f64.to_bits() - 1), 2).unwrap(),
        dec!(0.37)
    );
    assert_eq!(requested_of(2.5, 0).unwrap(), dec!(2));
    assert_eq!(requested_of(3.5, 0).unwrap(), dec!(4));
    // 28 places, and a subnormal that rounds to zero.
    assert_eq!(
        requested_of(1e-28, 28).unwrap(),
        dec!(0.0000000000000000000000000001)
    );
    assert_eq!(requested_of(5e-324, 28).unwrap(), dec!(0));
    // Large integers are exact; beyond Decimal's range is an error.
    assert_eq!(
        requested_of(2f64.powi(80), 8).unwrap(),
        dec!(1208925819614629174706176)
    );
    assert!(matches!(
        requested_of(2f64.powi(96), 0),
        Err(ArbitrageError::InvalidInput { .. })
    ));
}

#[test]
fn reduce_only_never_increases_or_flips_the_exposure() {
    // Exposure +12.5. A buy would increase it: no plan.
    let mut l = limits(ExcessPolicy::Clip);
    l.reduce_only = true;
    assert_eq!(
        hedge_plan_from_action(
            &action(Side::Buy, ActionQuantity::Exact(dec!(1))),
            &l,
            &market()
        )
        .unwrap(),
        ActionOutcome::NoPlan {
            requested: dec!(1),
            reason: NoPlanReason::NotReducing
        }
    );
    // A sell of 18 is clipped to 12.5 so the position does not flip.
    let p = plan(
        &action(Side::Sell, ActionQuantity::Exact(dec!(18))),
        &l,
        &market(),
    );
    assert_eq!(p.feasible, q(dec!(12.5)));
    assert_eq!(p.adjustments, vec![Adjustment::ReduceOnly]);
    assert_eq!(p.plan.expected_exposure_after, dec!(0));
    // Flat exposure: nothing reduces it.
    let mut flat = market();
    flat.exposure = dec!(0);
    assert!(matches!(
        hedge_plan_from_action(
            &action(Side::Sell, ActionQuantity::Exact(dec!(1))),
            &l,
            &flat
        )
        .unwrap(),
        ActionOutcome::NoPlan {
            reason: NoPlanReason::NotReducing,
            ..
        }
    ));
}

#[test]
fn the_exposure_limit_clips_the_quantity() {
    // |exposure after| <= 15. From +12.5 a buy has room 2.5 and a sell 27.5
    // (clipped by max_quantity 20 first).
    let mut l = limits(ExcessPolicy::Clip);
    l.exposure_cap = Some(q(dec!(15)));
    let p = plan(
        &action(Side::Buy, ActionQuantity::Exact(dec!(10))),
        &l,
        &market(),
    );
    assert_eq!(p.feasible, q(dec!(2.5)));
    assert_eq!(p.adjustments, vec![Adjustment::ExposureLimit]);
    assert_eq!(p.plan.expected_exposure_after, dec!(15));
    let p = plan(
        &action(Side::Sell, ActionQuantity::Exact(dec!(30))),
        &l,
        &market(),
    );
    assert_eq!(p.feasible, q(dec!(20)));
    assert_eq!(p.adjustments, vec![Adjustment::MaxQuantity]);
    assert_eq!(p.plan.expected_exposure_after, dec!(-7.5));
    // Already beyond the limit (+16): buying has no room; selling reduces.
    let mut over = market();
    over.exposure = dec!(16);
    assert_eq!(
        hedge_plan_from_action(
            &action(Side::Buy, ActionQuantity::Exact(dec!(1))),
            &l,
            &over
        )
        .unwrap(),
        ActionOutcome::NoPlan {
            requested: dec!(1),
            reason: NoPlanReason::ExposureLimit
        }
    );
    let p = plan(
        &action(Side::Sell, ActionQuantity::Exact(dec!(3))),
        &l,
        &over,
    );
    assert_eq!(p.feasible, q(dec!(3)));
    assert_eq!(p.plan.expected_exposure_after, dec!(13));
}

#[test]
fn above_the_cap_only_reductions_are_accepted() {
    // Codex M404-C3 with the coordinator's contract: cap 10.
    let mut l = limits(ExcessPolicy::Clip);
    l.exposure_cap = Some(q(dec!(10)));
    let at = |exposure: Decimal| {
        let mut m = market();
        m.exposure = exposure;
        m
    };
    // Long 100: selling 1 is a partial reduction above the cap, tagged.
    let p = plan(
        &action(Side::Sell, ActionQuantity::Exact(dec!(1))),
        &l,
        &at(dec!(100)),
    );
    assert_eq!(p.plan.expected_exposure_after, dec!(99));
    assert_eq!(p.adjustments, vec![Adjustment::ExposureAboveCap]);
    // Short 100: buying 1 likewise.
    let p = plan(
        &action(Side::Buy, ActionQuantity::Exact(dec!(1))),
        &l,
        &at(dec!(-100)),
    );
    assert_eq!(p.plan.expected_exposure_after, dec!(-99));
    assert_eq!(p.adjustments, vec![Adjustment::ExposureAboveCap]);
    // Increasing |exposure| above the cap: no plan, both signs.
    for (side, exposure) in [(Side::Buy, dec!(100)), (Side::Sell, dec!(-100))] {
        assert!(matches!(
            hedge_plan_from_action(
                &action(side, ActionQuantity::Exact(dec!(1))),
                &l,
                &at(exposure)
            )
            .unwrap(),
            ActionOutcome::NoPlan {
                reason: NoPlanReason::ExposureLimit,
                ..
            }
        ));
    }
    // Crossing zero from +15 never goes beyond -10. With max 20 a sell of
    // 30 is clipped to 20 (to -5); with max 100 it is clipped to 25 (-10).
    let p = plan(
        &action(Side::Sell, ActionQuantity::Exact(dec!(30))),
        &l,
        &at(dec!(15)),
    );
    assert_eq!(p.feasible, q(dec!(20)));
    assert_eq!(p.plan.expected_exposure_after, dec!(-5));
    let mut wide_max = l;
    wide_max.max_quantity = q(dec!(100));
    let p = plan(
        &action(Side::Sell, ActionQuantity::Exact(dec!(30))),
        &wide_max,
        &at(dec!(15)),
    );
    assert_eq!(p.feasible, q(dec!(25)));
    assert_eq!(p.adjustments, vec![Adjustment::ExposureLimit]);
    assert_eq!(p.plan.expected_exposure_after, dec!(-10));
    // From -100 a buy of 150 is clipped to 100 by max_quantity, which is
    // within the cap's room of 110 (up to +10): it ends flat.
    let p = plan(
        &action(Side::Buy, ActionQuantity::Exact(dec!(150))),
        &wide_max,
        &at(dec!(-100)),
    );
    assert_eq!(p.feasible, q(dec!(100)));
    assert_eq!(p.adjustments, vec![Adjustment::MaxQuantity]);
    assert_eq!(p.plan.expected_exposure_after, dec!(0));
    // Thin liquidity above the cap: from +100 a sell of 1 limited to 0.5 by
    // the book is still an accepted partial reduction.
    let mut thin = at(dec!(100));
    thin.book.bid = lvl(dec!(99.5), dec!(0.5));
    let p = plan(
        &action(Side::Sell, ActionQuantity::Exact(dec!(1))),
        &l,
        &thin,
    );
    assert_eq!(p.plan.expected_exposure_after, dec!(99.5));
    assert_eq!(
        p.adjustments,
        vec![Adjustment::QuotedSize, Adjustment::ExposureAboveCap]
    );
}
