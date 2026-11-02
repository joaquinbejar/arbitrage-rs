//! Hand-checked fixtures for `opportunity::evaluate`.

use arbitrage_rs::opportunity::{
    Carry, CarryKind, Evaluation, LegQuote, OpportunityInput, RejectReason, evaluate,
};
use arbitrage_rs::{
    ArbitrageError, Currency, Decimal, FeeRate, Instrument, InstrumentId, Level, Money, Price,
    Quantity, Side, Timestamp, TopOfBook,
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
fn lvl(p: Decimal, s: Decimal) -> Option<Level> {
    Some(Level::new(Price::new(p).unwrap(), q(s)))
}
fn inst(id: &str) -> Instrument {
    Instrument::undated(InstrumentId::new(id).unwrap())
}
fn bps(x: Decimal) -> FeeRate {
    FeeRate::from_bps(x).unwrap()
}

/// Buy A at 100.00 (5 available), sell B at 100.30 (3 available), 5 bps
/// taker fees, target 4, minimum 1, no carry, zero threshold.
fn base() -> OpportunityInput {
    OpportunityInput {
        as_of: Timestamp::from_millis(1_000),
        quote_currency: usd(),
        buy: LegQuote {
            instrument: inst("A"),
            book: TopOfBook {
                bid: lvl(dec!(99.90), dec!(5)),
                ask: lvl(dec!(100.00), dec!(5)),
            },
            fee: bps(dec!(5)),
        },
        sell: LegQuote {
            instrument: inst("B"),
            book: TopOfBook {
                bid: lvl(dec!(100.30), dec!(3)),
                ask: lvl(dec!(100.40), dec!(3)),
            },
            fee: bps(dec!(5)),
        },
        target_quantity: q(dec!(4)),
        min_quantity: q(dec!(1)),
        carry: vec![],
        min_net_edge: m(dec!(0)),
    }
}

#[test]
fn accepted_at_top_of_book_size() {
    // q = min(4, 5, 3) = 3. Buy 300, sell 300.9, gross 0.9.
    // Fees 300 * 0.0005 = 0.15 and 300.9 * 0.0005 = 0.15045; net 0.59955.
    let eval = evaluate(&base()).unwrap();
    let o = eval.accepted().expect("accepted");
    assert_eq!(o.buy.side, Side::Buy);
    assert_eq!(o.buy.price.value(), dec!(100));
    assert_eq!(o.sell.price.value(), dec!(100.3));
    assert_eq!(o.buy.quantity, q(dec!(3)));
    assert_eq!(o.sell.quantity, q(dec!(3)));
    assert_eq!(o.expected_gross, m(dec!(0.9)));
    assert_eq!(o.expected_costs.buy_fee.money(), m(dec!(0.15)));
    assert_eq!(o.expected_costs.sell_fee.money(), m(dec!(0.15045)));
    assert_eq!(o.expected_costs.total.money(), m(dec!(0.30045)));
    assert_eq!(o.expected_net, m(dec!(0.59955)));
    assert_eq!(o.as_of, Timestamp::from_millis(1_000));
}

#[test]
fn positive_gross_negative_net_is_rejected() {
    // q = 10, gross (100.05 - 100) * 10 = 0.5; fees 0.5 + 0.50025.
    let mut input = base();
    input.buy.book.ask = lvl(dec!(100), dec!(10));
    input.sell.book.bid = lvl(dec!(100.05), dec!(10));
    input.target_quantity = q(dec!(10));
    let eval = evaluate(&input).unwrap();
    assert!(eval.accepted().is_none());
    match eval {
        Evaluation::Rejected(RejectReason::NetEdgeBelowThreshold {
            gross, costs, net, ..
        }) => {
            assert_eq!(gross, m(dec!(0.5)));
            assert_eq!(costs.total.money(), m(dec!(1.00025)));
            assert_eq!(net, m(dec!(-0.50025)));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn zero_net_is_rejected_even_with_zero_threshold() {
    // gross 0.1 on q = 1; fees 5 bps on 100 and 0 bps on 100.1 = 0.05;
    // carry 5 bps on the buy leg = 0.05; net exactly 0.
    let mut input = base();
    input.target_quantity = q(dec!(1));
    input.sell.book.bid = lvl(dec!(100.1), dec!(3));
    input.sell.fee = FeeRate::zero();
    input.carry = vec![Carry {
        kind: CarryKind::Financing,
        leg: Side::Buy,
        rate: bps(dec!(5)),
    }];
    match evaluate(&input).unwrap() {
        Evaluation::Rejected(RejectReason::NetEdgeBelowThreshold { net, .. }) => {
            assert!(net.is_zero());
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn carry_is_itemized_and_subtracted() {
    // Base plus borrow 10 bps on the sell notional (300.9 -> 0.3009) and
    // financing 2 bps on the buy notional (300 -> 0.06).
    // Total 0.30045 + 0.3009 + 0.06 = 0.66135; net 0.9 - 0.66135 = 0.23865.
    let mut input = base();
    input.carry = vec![
        Carry {
            kind: CarryKind::Borrow,
            leg: Side::Sell,
            rate: bps(dec!(10)),
        },
        Carry {
            kind: CarryKind::Financing,
            leg: Side::Buy,
            rate: bps(dec!(2)),
        },
    ];
    let eval = evaluate(&input).unwrap();
    let o = eval.accepted().expect("accepted");
    assert_eq!(o.expected_costs.carry.len(), 2);
    assert_eq!(o.expected_costs.carry[0].kind, CarryKind::Borrow);
    assert_eq!(o.expected_costs.carry[0].cost.money(), m(dec!(0.3009)));
    assert_eq!(o.expected_costs.carry[1].leg, Side::Buy);
    assert_eq!(o.expected_costs.carry[1].cost.money(), m(dec!(0.06)));
    assert_eq!(o.expected_costs.total.money(), m(dec!(0.66135)));
    assert_eq!(o.expected_net, m(dec!(0.23865)));

    // The same opportunity under a 0.25 threshold is rejected.
    input.min_net_edge = m(dec!(0.25));
    assert!(matches!(
        evaluate(&input).unwrap(),
        Evaluation::Rejected(RejectReason::NetEdgeBelowThreshold { .. })
    ));
}

#[test]
fn rebates_raise_the_net_edge() {
    // q = 10, gross 0.5; fees -1 bps: -0.1 and -0.10005; net 0.70005.
    let mut input = base();
    input.buy.book.ask = lvl(dec!(100), dec!(10));
    input.sell.book.bid = lvl(dec!(100.05), dec!(10));
    input.target_quantity = q(dec!(10));
    input.buy.fee = bps(dec!(-1));
    input.sell.fee = bps(dec!(-1));
    let eval = evaluate(&input).unwrap();
    let o = eval.accepted().expect("accepted");
    assert_eq!(o.expected_costs.total.money(), m(dec!(-0.20005)));
    assert_eq!(o.expected_net, m(dec!(0.70005)));
}

#[test]
fn missing_quotes_are_explicit() {
    let mut input = base();
    input.buy.book.ask = None;
    assert_eq!(
        evaluate(&input).unwrap(),
        Evaluation::Rejected(RejectReason::MissingQuote {
            instrument: inst("A"),
            side: Side::Buy
        })
    );
    let mut input = base();
    input.sell.book.bid = None;
    assert_eq!(
        evaluate(&input).unwrap(),
        Evaluation::Rejected(RejectReason::MissingQuote {
            instrument: inst("B"),
            side: Side::Sell
        })
    );
}

#[test]
fn insufficient_size_is_explicit() {
    let mut input = base();
    input.buy.book.ask = lvl(dec!(100), dec!(0.5));
    assert_eq!(
        evaluate(&input).unwrap(),
        Evaluation::Rejected(RejectReason::InsufficientLiquidity {
            available: q(dec!(0.5)),
            required: q(dec!(1)),
        })
    );
}

#[test]
fn no_gross_edge() {
    let mut input = base();
    input.sell.book.bid = lvl(dec!(99.9), dec!(3));
    assert_eq!(
        evaluate(&input).unwrap(),
        Evaluation::Rejected(RejectReason::NoGrossEdge {
            gross: m(dec!(-0.3))
        })
    );
    input.sell.book.bid = lvl(dec!(100), dec!(3));
    assert_eq!(
        evaluate(&input).unwrap(),
        Evaluation::Rejected(RejectReason::NoGrossEdge { gross: m(dec!(0)) })
    );
}

#[test]
fn invalid_inputs_are_errors() {
    let mut input = base();
    input.sell.instrument = inst("A");
    assert!(matches!(
        evaluate(&input),
        Err(ArbitrageError::InvalidInput { .. })
    ));

    let mut input = base();
    input.min_quantity = q(dec!(5));
    assert!(matches!(
        evaluate(&input),
        Err(ArbitrageError::InvalidInput { .. })
    ));

    let mut input = base();
    input.min_net_edge = m(dec!(-0.01));
    assert!(matches!(
        evaluate(&input),
        Err(ArbitrageError::InvalidValue {
            field: "min_net_edge",
            ..
        })
    ));

    let mut input = base();
    input.min_net_edge = Money::new(dec!(0), Currency::new("EUR").unwrap());
    assert!(matches!(
        evaluate(&input),
        Err(ArbitrageError::CurrencyMismatch { .. })
    ));
}

#[test]
fn expired_leg_is_an_error() {
    let mut input = base();
    input.sell.instrument = Instrument::dated(
        InstrumentId::new("B-F").unwrap(),
        Timestamp::from_millis(1_000),
    );
    assert!(matches!(
        evaluate(&input),
        Err(ArbitrageError::Expired { .. })
    ));
    input.as_of = Timestamp::from_millis(999);
    let eval = evaluate(&input).unwrap();
    let o = eval.accepted().expect("accepted");
    assert_eq!(
        o.sell.instrument.expiry,
        Some(Timestamp::from_millis(1_000))
    );
}

/// 0.6 * Decimal::MAX: twice it passes the range of a Decimal.
const BIG: Decimal = dec!(47536897508558602556126370201);

#[test]
fn gross_and_fees_are_exact_when_notionals_pass_max() {
    // Notionals near 2 * 0.6 * MAX pass Decimal::MAX; the gross and the
    // fees (1 bps of each notional) fit.
    let ask = dec!(47536897508558602556126370000);
    let mut i = base();
    i.buy.book.ask = lvl(ask, dec!(5));
    i.sell.book.bid = lvl(ask + dec!(10000), dec!(5));
    i.buy.fee = bps(dec!(1));
    i.sell.fee = bps(dec!(1));
    i.target_quantity = q(dec!(2));
    match evaluate(&i).unwrap() {
        Evaluation::Rejected(RejectReason::NetEdgeBelowThreshold { gross, costs, .. }) => {
            assert_eq!(gross.amount(), dec!(20000));
            assert_eq!(
                costs.buy_fee.money().amount(),
                dec!(9507379501711720511225274)
            );
            assert_eq!(
                costs.sell_fee.money().amount(),
                dec!(9507379501711720511225276)
            );
            assert_eq!(
                costs.total.money().amount(),
                dec!(19014759003423441022450550)
            );
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn cost_total_is_one_exact_sum() {
    // Fees of BIG and BIG + 1 (100 % of notional) pass Decimal::MAX as a
    // running sum; a carry benefit of BIG brings the total back to BIG + 1.
    let mut i = base();
    i.buy.book.ask = lvl(BIG, dec!(5));
    i.sell.book.bid = lvl(BIG + dec!(1), dec!(5));
    i.buy.fee = bps(dec!(10000));
    i.sell.fee = bps(dec!(10000));
    i.target_quantity = q(dec!(1));
    i.carry = vec![Carry {
        kind: CarryKind::Other,
        leg: Side::Buy,
        rate: bps(dec!(-10000)),
    }];
    match evaluate(&i).unwrap() {
        Evaluation::Rejected(RejectReason::NetEdgeBelowThreshold { costs, net, .. }) => {
            assert_eq!(costs.total.money().amount(), BIG + dec!(1));
            assert_eq!(net.amount(), -BIG);
        }
        other => panic!("unexpected {other:?}"),
    }
}
