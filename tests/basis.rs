//! Hand-checked fixtures for `basis::evaluate_spot_perp`.

// Tests fail loudly: the library's no-panic lints do not apply here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use arbitrage_rs::basis::{
    BasisDirection, BasisEvaluation, BasisInput, BasisRejectReason, evaluate_spot_perp,
};
use arbitrage_rs::funding::{
    FundingEvent, FundingRate, FundingSchedule, PerpConvention, RateKind, SettlementRounding,
};
use arbitrage_rs::opportunity::{Carry, CarryKind, LegQuote};
use arbitrage_rs::{
    ArbitrageError, Currency, Decimal, FeeRate, Instrument, InstrumentId, Level, Money, Price,
    Quantity, Rounding, Side, Timestamp, TopOfBook,
};
use rust_decimal_macros::dec;

const H8: i64 = 8 * 3_600_000;

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
fn lvl(px: Decimal, s: Decimal) -> Level {
    Level::new(p(px), q(s))
}
fn bps(x: Decimal) -> FeeRate {
    FeeRate::from_bps(x).unwrap()
}
fn t(ms: i64) -> Timestamp {
    Timestamp::from_millis(ms)
}
fn events(rate: Decimal, kind: RateKind) -> Vec<FundingEvent> {
    [H8, 2 * H8, 3 * H8]
        .iter()
        .map(|ms| FundingEvent {
            time: t(*ms),
            rate: FundingRate::new(rate, kind).unwrap(),
            mark: p(dec!(30050)),
        })
        .collect()
}

/// Spot 29990 / 30000, perp 30060 / 30070, 2 on every side. Fees 10 bps
/// spot and 5 bps perp. Long spot, short perp, q = 1, exit basis 0.
/// Funding every 8 h from 0; window (0, 24h] holds 3 instants at an
/// estimated 1 bp on a mark of 30050.
fn base() -> BasisInput {
    BasisInput {
        as_of: t(0),
        quote_currency: usd(),
        direction: BasisDirection::LongSpotShortPerp,
        spot: LegQuote {
            instrument: Instrument::undated(InstrumentId::new("BTC-USD").unwrap()),
            book: TopOfBook {
                bid: Some(lvl(dec!(29990), dec!(2))),
                ask: Some(lvl(dec!(30000), dec!(2))),
            },
            fee: bps(dec!(10)),
        },
        perp: LegQuote {
            instrument: Instrument::undated(InstrumentId::new("BTC-USD-PERP").unwrap()),
            book: TopOfBook {
                bid: Some(lvl(dec!(30060), dec!(2))),
                ask: Some(lvl(dec!(30070), dec!(2))),
            },
            fee: bps(dec!(5)),
        },
        convention: PerpConvention::Linear,
        mark: p(dec!(30050)),
        index: p(dec!(30000)),
        target_quantity: q(dec!(1)),
        min_quantity: q(dec!(0.1)),
        schedule: FundingSchedule::new(t(0), H8).unwrap(),
        exit_time: t(3 * H8),
        funding: events(dec!(0.0001), RateKind::Estimated),
        funding_rounding: None,
        expected_exit_basis: dec!(0),
        carry: vec![],
        min_net_edge: m(dec!(0)),
    }
}

#[test]
fn gross_positive_net_negative_is_rejected() {
    // Gross (60 - 0) * 1 = 60. Fees: spot 30 + 30; perp 15.03 at entry and
    // 15 at the implied exit price 30000 + 0: 90.03. Funding: short 1
    // receives 30050 * 0.0001 = 3.005 three times, -9.015. Total 81.015;
    // net -21.015.
    match evaluate_spot_perp(&base()).unwrap() {
        BasisEvaluation::Rejected(BasisRejectReason::NetEdgeBelowThreshold {
            gross,
            costs,
            net,
            ..
        }) => {
            assert_eq!(gross, m(dec!(60)));
            assert_eq!(costs.spot_entry_fee.money(), m(dec!(30)));
            assert_eq!(costs.perp_exit_fee.money(), m(dec!(15)));
            assert_eq!(costs.funding.len(), 3);
            assert_eq!(costs.funding[0].cost.money(), m(dec!(-3.005)));
            assert_eq!(costs.funding_total.money(), m(dec!(-9.015)));
            assert_eq!(costs.total.money(), m(dec!(81.015)));
            assert_eq!(net, m(dec!(-21.015)));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn cash_and_carry_accepted_with_funding_received() {
    // Fees 2 bps spot (6 + 6) and 1 bp perp (3.006 at entry, 3 at the
    // implied exit 30000) = 18.006; funding -9.015; financing 3 bps on the
    // spot notional = 9. Total 17.991; net 60 - 17.991 = 42.009.
    let mut input = base();
    input.spot.fee = bps(dec!(2));
    input.perp.fee = bps(dec!(1));
    input.carry = vec![Carry {
        kind: CarryKind::Financing,
        leg: Side::Buy,
        rate: bps(dec!(3)),
    }];
    let eval = evaluate_spot_perp(&input).unwrap();
    let o = eval.accepted().expect("accepted");
    assert_eq!(o.spot.side, Side::Buy);
    assert_eq!(o.spot.price.value(), dec!(30000));
    assert_eq!(o.perp.side, Side::Sell);
    assert_eq!(o.perp.price.value(), dec!(30060));
    assert_eq!(o.entry_basis, dec!(60));
    assert_eq!(o.mark.value(), dec!(30050));
    assert_eq!(o.index.value(), dec!(30000));
    assert_eq!(o.expected_costs.carry[0].cost.money(), m(dec!(9)));
    assert_eq!(o.expected_costs.funding_total.money(), m(dec!(-9.015)));
    assert_eq!(o.expected_costs.total.money(), m(dec!(17.991)));
    assert_eq!(o.expected_net, m(dec!(42.009)));
}

#[test]
fn reverse_trade_receives_negative_funding() {
    // Sell spot at 29990, buy perp at 30070: entry basis 80. Exit basis
    // assumed 100, so (100 - 80) * 1 = 20. No fees. Long 1 perp at -2 bp:
    // 30050 * -0.0002 = -6.01 per instant, -18.03 received. Borrow 5 bps
    // on the spot notional 29990 = 14.995. Net 20 + 18.03 - 14.995 = 23.035.
    let mut input = base();
    input.direction = BasisDirection::ShortSpotLongPerp;
    input.spot.fee = FeeRate::zero();
    input.perp.fee = FeeRate::zero();
    input.expected_exit_basis = dec!(100);
    input.funding = events(dec!(-0.0002), RateKind::Estimated);
    input.carry = vec![Carry {
        kind: CarryKind::Borrow,
        leg: Side::Sell,
        rate: bps(dec!(5)),
    }];
    let eval = evaluate_spot_perp(&input).unwrap();
    let o = eval.accepted().expect("accepted");
    assert_eq!(o.spot.side, Side::Sell);
    assert_eq!(o.spot.price.value(), dec!(29990));
    assert_eq!(o.perp.side, Side::Buy);
    assert_eq!(o.entry_basis, dec!(80));
    assert_eq!(o.expected_gross, m(dec!(20)));
    assert_eq!(o.expected_costs.funding_total.money(), m(dec!(-18.03)));
    assert_eq!(o.expected_costs.carry[0].cost.money(), m(dec!(14.995)));
    assert_eq!(o.expected_net, m(dec!(23.035)));
}

#[test]
fn reverse_trade_pays_positive_funding() {
    // As above without borrow and at +2 bp: long pays 6.01 three times.
    // Net 20 - 18.03 = 1.97.
    let mut input = base();
    input.direction = BasisDirection::ShortSpotLongPerp;
    input.spot.fee = FeeRate::zero();
    input.perp.fee = FeeRate::zero();
    input.expected_exit_basis = dec!(100);
    input.funding = events(dec!(0.0002), RateKind::Estimated);
    let eval = evaluate_spot_perp(&input).unwrap();
    let o = eval.accepted().expect("accepted");
    assert_eq!(o.expected_costs.funding_total.money(), m(dec!(18.03)));
    assert_eq!(o.expected_net, m(dec!(1.97)));
}

/// Fee-free input with an explicit exit basis, direction and funding rate.
fn fee_free(dir: BasisDirection, exit_basis: Decimal, rate: Decimal) -> BasisInput {
    let mut input = base();
    input.direction = dir;
    input.spot.fee = FeeRate::zero();
    input.perp.fee = FeeRate::zero();
    input.expected_exit_basis = exit_basis;
    input.funding = events(rate, RateKind::Estimated);
    input
}

fn net_of(input: &BasisInput) -> (Money, Money, bool) {
    match evaluate_spot_perp(input).unwrap() {
        BasisEvaluation::Accepted(o) => (o.expected_gross, o.expected_net, true),
        BasisEvaluation::Rejected(BasisRejectReason::NetEdgeBelowThreshold {
            gross, net, ..
        }) => (gross, net, false),
        other @ BasisEvaluation::Rejected(_) => panic!("unexpected {other:?}"),
    }
}

#[test]
fn flat_basis_with_funding_received_is_accepted() {
    // Long spot / short perp, entry basis 60, exit 60: gross 0. The short
    // receives 30050 * 0.0001 = 3.005 three times: net 9.015.
    let long = fee_free(BasisDirection::LongSpotShortPerp, dec!(60), dec!(0.0001));
    assert_eq!(net_of(&long), (m(dec!(0)), m(dec!(9.015)), true));
    // Short spot / long perp, entry basis 80, exit 80: gross 0. The long
    // receives 30050 * 0.0002 = 6.01 three times at -2 bp: net 18.03.
    let short = fee_free(BasisDirection::ShortSpotLongPerp, dec!(80), dec!(-0.0002));
    assert_eq!(net_of(&short), (m(dec!(0)), m(dec!(18.03)), true));
}

#[test]
fn negative_convergence_offset_by_funding_is_accepted() {
    // Exit basis 61 long spot: gross -1, net -1 + 9.015 = 8.015.
    let long = fee_free(BasisDirection::LongSpotShortPerp, dec!(61), dec!(0.0001));
    assert_eq!(net_of(&long), (m(dec!(-1)), m(dec!(8.015)), true));
    // Exit basis 79 short spot: gross -1, net -1 + 18.03 = 17.03.
    let short = fee_free(BasisDirection::ShortSpotLongPerp, dec!(79), dec!(-0.0002));
    assert_eq!(net_of(&short), (m(dec!(-1)), m(dec!(17.03)), true));
}

#[test]
fn funding_paid_that_pushes_net_to_zero_or_below_is_rejected() {
    // Flat basis, long spot / short perp at -1 bp: the short pays 9.015.
    let long = fee_free(BasisDirection::LongSpotShortPerp, dec!(60), dec!(-0.0001));
    assert_eq!(net_of(&long), (m(dec!(0)), m(dec!(-9.015)), false));
    // Flat basis, short spot / long perp at +2 bp: the long pays 18.03.
    let short = fee_free(BasisDirection::ShortSpotLongPerp, dec!(80), dec!(0.0002));
    assert_eq!(net_of(&short), (m(dec!(0)), m(dec!(-18.03)), false));
    // Convergence 18.03 exactly offset by funding paid: net 0, rejected.
    let zero = fee_free(BasisDirection::ShortSpotLongPerp, dec!(98.03), dec!(0.0002));
    assert_eq!(net_of(&zero), (m(dec!(18.03)), m(dec!(0)), false));
}

#[test]
fn exit_fee_uses_the_implied_exit_notional() {
    // Long spot / short perp, exit basis 10: the perpetual is assumed to
    // exit at 30000 + 10 = 30010, so its exit fee at 5 bps is 15.005
    // instead of 15.03 on the entry notional.
    let mut input = base();
    input.expected_exit_basis = dec!(10);
    match evaluate_spot_perp(&input).unwrap() {
        BasisEvaluation::Rejected(BasisRejectReason::NetEdgeBelowThreshold { costs, .. }) => {
            assert_eq!(costs.perp_entry_fee.money(), m(dec!(15.03)));
            assert_eq!(costs.perp_exit_fee.money(), m(dec!(15.005)));
            assert_eq!(costs.spot_exit_fee.money(), m(dec!(30)));
        }
        other => panic!("unexpected {other:?}"),
    }
    // An exit basis that implies a non-positive perpetual price is invalid.
    input.expected_exit_basis = dec!(-30000);
    assert!(matches!(
        evaluate_spot_perp(&input),
        Err(ArbitrageError::InvalidInput { .. })
    ));
}

#[test]
fn a_funding_instant_at_exit_time_is_counted() {
    // exit_time is exactly the third instant: three events, all counted.
    let input = fee_free(BasisDirection::LongSpotShortPerp, dec!(60), dec!(0.0001));
    assert_eq!(input.exit_time, t(3 * H8));
    let eval = evaluate_spot_perp(&input).unwrap();
    let o = eval.accepted().unwrap();
    assert_eq!(o.expected_costs.funding.len(), 3);
    assert_eq!(o.expected_costs.funding[2].time, t(3 * H8));
}

#[test]
fn missing_quote_and_insufficient_size() {
    let mut input = base();
    input.perp.book.bid = None;
    assert!(matches!(
        evaluate_spot_perp(&input).unwrap(),
        BasisEvaluation::Rejected(BasisRejectReason::MissingQuote {
            side: Side::Sell,
            ..
        })
    ));
    let mut input = base();
    input.spot.book.ask = Some(lvl(dec!(30000), dec!(0.05)));
    assert_eq!(
        evaluate_spot_perp(&input).unwrap(),
        BasisEvaluation::Rejected(BasisRejectReason::InsufficientLiquidity {
            available: q(dec!(0.05)),
            required: q(dec!(0.1)),
        })
    );
}

#[test]
fn published_rates_are_usable_once_published() {
    let mut input = base();
    input.spot.fee = FeeRate::zero();
    input.perp.fee = FeeRate::zero();
    // The next rate was fixed by the venue at as_of; the later ones are
    // estimates.
    input.funding[0].rate =
        FundingRate::new(dec!(0.0001), RateKind::Published { at: t(0) }).unwrap();
    let eval = evaluate_spot_perp(&input).unwrap();
    let o = eval.accepted().expect("accepted");
    assert_eq!(
        o.expected_costs.funding[0].kind,
        RateKind::Published { at: t(0) }
    );
    assert_eq!(o.expected_costs.funding[1].kind, RateKind::Estimated);
}

#[test]
fn a_rate_published_after_the_decision_is_look_ahead() {
    let mut input = base();
    input.funding[2].rate =
        FundingRate::new(dec!(0.0001), RateKind::Published { at: t(3 * H8) }).unwrap();
    assert_eq!(
        evaluate_spot_perp(&input),
        Err(ArbitrageError::LookAhead {
            known_at: t(3 * H8),
            as_of: t(0)
        })
    );
}

#[test]
fn funding_events_must_match_the_schedule() {
    let bad = |f: &dyn Fn(&mut BasisInput)| {
        let mut input = base();
        f(&mut input);
        assert!(
            matches!(
                evaluate_spot_perp(&input),
                Err(ArbitrageError::InvalidInput { .. })
            ),
            "{:?}",
            input.funding.iter().map(|e| e.time).collect::<Vec<_>>()
        );
    };
    bad(&|i| {
        i.funding.pop();
    });
    bad(&|i| i.funding.swap(0, 1));
    bad(&|i| {
        let e = i.funding[0];
        i.funding.insert(0, e);
    });
    bad(&|i| i.funding[1].time = t(2 * H8 + 1));
    // An instant at as_of is outside (as_of, exit_time].
    bad(&|i| {
        let mut e = i.funding[0];
        e.time = t(0);
        i.funding.insert(0, e);
    });
    // Exit before the third instant but three events.
    bad(&|i| i.exit_time = t(3 * H8 - 1));
}

#[test]
fn other_invalid_inputs() {
    let mut input = base();
    input.exit_time = t(0);
    assert!(matches!(
        evaluate_spot_perp(&input),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    let mut input = base();
    input.perp.instrument = input.spot.instrument.clone();
    assert!(matches!(
        evaluate_spot_perp(&input),
        Err(ArbitrageError::InvalidInput { .. })
    ));
    let mut input = base();
    input.min_net_edge = m(dec!(-1));
    assert!(matches!(
        evaluate_spot_perp(&input),
        Err(ArbitrageError::InvalidValue { .. })
    ));
}

#[test]
fn no_funding_instant_in_a_short_window() {
    // Hold 1 hour: no funding instant, so no funding cost.
    let mut input = base();
    input.spot.fee = FeeRate::zero();
    input.perp.fee = FeeRate::zero();
    input.exit_time = t(3_600_000);
    input.funding = vec![];
    let eval = evaluate_spot_perp(&input).unwrap();
    let o = eval.accepted().expect("accepted");
    assert_eq!(o.expected_costs.funding.len(), 0);
    assert!(o.expected_costs.funding_total.money().is_zero());
    assert_eq!(o.expected_net, m(dec!(60)));
}

#[test]
fn funding_settled_per_event_as_the_venue_rounds() {
    // Mark 30050.5, q = 1, 1 bp: each event is -3.00505 exact for the
    // short. Settled to cents half-even: -3.01 (0.00505 is above half a
    // cent), three times -9.03. Exact total would be -9.01515.
    let mut input = base();
    input.spot.fee = FeeRate::zero();
    input.perp.fee = FeeRate::zero();
    for e in &mut input.funding {
        e.mark = p(dec!(30050.5));
    }
    let exact = evaluate_spot_perp(&input).unwrap();
    assert_eq!(
        exact
            .accepted()
            .unwrap()
            .expected_costs
            .funding_total
            .money(),
        m(dec!(-9.01515))
    );
    input.funding_rounding = Some(SettlementRounding::new(2, Rounding::HalfEven).unwrap());
    let settled = evaluate_spot_perp(&input).unwrap();
    let o = settled.accepted().unwrap();
    assert_eq!(o.expected_costs.funding[0].cost.money(), m(dec!(-3.01)));
    assert_eq!(o.expected_costs.funding_total.money(), m(dec!(-9.03)));
    assert_eq!(o.expected_net, m(dec!(69.03)));
}

/// 0.6 * `Decimal::MAX`: twice it passes the range of a Decimal.
const BIG: Decimal = dec!(47536897508558602556126370201);

#[test]
fn gross_is_exact_when_notionals_pass_max() {
    // Long spot at BIG, short perp at BIG + 1, q = 2: the notionals pass
    // Decimal::MAX, the gross 2 * (1 - 0) fits. Zero fees and funding.
    let mut i = base();
    i.spot.book.ask = Some(lvl(BIG, dec!(2)));
    i.perp.book.bid = Some(lvl(BIG + dec!(1), dec!(2)));
    i.spot.fee = FeeRate::zero();
    i.perp.fee = FeeRate::zero();
    i.target_quantity = q(dec!(2));
    i.funding = events(dec!(0), RateKind::Estimated);
    match evaluate_spot_perp(&i).unwrap() {
        BasisEvaluation::Accepted(o) => {
            assert_eq!(o.expected_gross.amount(), dec!(2));
            assert_eq!(o.expected_costs.total.money().amount(), dec!(0));
            assert_eq!(o.expected_net.amount(), dec!(2));
        }
        other @ BasisEvaluation::Rejected(_) => panic!("unexpected {other:?}"),
    }
}

#[test]
fn cost_total_is_one_exact_sum() {
    // Entry fees of BIG and BIG + 1 (100 % of notional) pass Decimal::MAX
    // as a running sum; carry benefits bring the total back.
    let mut i = base();
    i.spot.book.ask = Some(lvl(BIG, dec!(2)));
    i.perp.book.bid = Some(lvl(BIG + dec!(1), dec!(2)));
    i.spot.fee = bps(dec!(10000));
    i.perp.fee = FeeRate::zero();
    i.target_quantity = q(dec!(1));
    i.funding = events(dec!(0), RateKind::Estimated);
    i.carry = vec![Carry {
        kind: CarryKind::Other,
        leg: Side::Buy,
        rate: bps(dec!(-10000)),
    }];
    // Spot entry BIG and exit BIG (the implied exit keeps spot), carry -BIG:
    // total BIG.
    match evaluate_spot_perp(&i).unwrap() {
        BasisEvaluation::Rejected(BasisRejectReason::NetEdgeBelowThreshold { costs, .. }) => {
            assert_eq!(costs.total.money().amount(), BIG);
        }
        other => panic!("unexpected {other:?}"),
    }
}
