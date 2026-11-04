[![License](https://img.shields.io/badge/license-MIT-blue)](./LICENSE)
[![Crates.io](https://img.shields.io/crates/v/arbitrage-rs.svg)](https://crates.io/crates/arbitrage-rs)
[![Downloads](https://img.shields.io/crates/d/arbitrage-rs.svg)](https://crates.io/crates/arbitrage-rs)
[![Stars](https://img.shields.io/github/stars/joaquinbejar/arbitrage-rs.svg)](https://github.com/joaquinbejar/arbitrage-rs/stargazers)
[![Issues](https://img.shields.io/github/issues/joaquinbejar/arbitrage-rs.svg)](https://github.com/joaquinbejar/arbitrage-rs/issues)
[![PRs](https://img.shields.io/github/issues-pr/joaquinbejar/arbitrage-rs.svg)](https://github.com/joaquinbejar/arbitrage-rs/pulls)

[![Build Status](https://img.shields.io/github/actions/workflow/status/joaquinbejar/arbitrage-rs/ci.yml?branch=main)](https://github.com/joaquinbejar/arbitrage-rs/actions)
[![Documentation](https://docs.rs/arbitrage-rs/badge.svg)](https://docs.rs/arbitrage-rs)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://github.com/joaquinbejar/arbitrage-rs/blob/main/Cargo.toml)

## Arbitrage Library

Arbitrage models for spot, futures, perpetuals and options: pricing
relations, opportunity detection, hedging and multi-leg execution, in exact
decimal arithmetic.

### Overview

arbitrage-rs is a set of pure decision kernels. It takes quotes, fees,
funding and fills as values and returns decisions: an opportunity with its
expected economics, a hedge plan, an admission against per-venue budgets, or
the next intents of an execution plan. It has no connector, matching engine,
clock or I/O, so the same code runs in a backtest, a simulator or next to a
live venue adapter.

Two principles hold everywhere:

- **Intents, not executions.** Opportunities and plans carry *expected*
  numbers. Nothing is reported as executed or as guaranteed profit, and
  execution is never assumed to be atomic: legs can fill partially, late, or
  not at all, and the execution plans react to what is observed.
- **Exact arithmetic.** Every amount is a `Decimal`. An operation is exact or
  it fails with a typed error; rounding happens only where the caller asks
  for it, once, with an explicit mode.

### Features

#### Opportunities

- **Net opportunity** between two instruments at executable prices: buy at
  the ask, sell at the bid, after the fees of both legs and itemized carry
  (financing, borrow), against a minimum net edge.
- **Spot-perpetual basis** with funding: entry and implied exit fees, carry,
  and funding cashflows over the holding period, estimated or published, with
  a look-ahead check.
- **Put-call parity** against a same-expiry future: the executable edge of
  the reversal and the conversion, with the discount factor, contract
  multipliers and the fees of all three legs (including the usual premium
  cap on option fees).

#### Hedging

- **Band hedging**: a one-leg hedge that brings an exposure back inside a
  band, bounded by the quoted size, a maximum quantity and a lot size, with
  its expected fee and half-spread cost.
- **Hedge actions from external policies** (for example a learned policy):
  exact or floating-point quantities converted once, bounded by limits,
  reduce-only and exposure caps, with every adjustment recorded.

#### Funding

- Linear and inverse perpetual funding cashflows, funding schedules, and
  estimated versus published rates, with a single rounding to a settlement
  quantum. Inverse funding is paid in the base currency, `contracts *
  contract_value / mark * rate`, as one exact quotient. The spot-perpetual
  basis evaluator supports linear perpetuals only.

#### Execution

- **Two-leg plans**: a state machine driven by observed fills, rejections,
  cancel acknowledgements, connection changes and venue views. Per-attempt
  cumulative quantities, idempotent events, recorded overfills, deadlines
  and expiries, and the failure policies abort, unwind and complete.
- **Multi-leg packages**: the same execution semantics for any number of
  legs with different contract sizes and lots (for example the call, put
  and future of a parity trade). The matched package is kept on a common
  lot grid, and recovery acts on each leg's excess in whole lots.
- **Per-venue budgets**: all-or-nothing admission of both legs without
  borrowing capital across venues, with inventory or margin funding for
  sells. Every leg names its venue. With budget gating, every resubmission
  is readmitted against its venue's current budget at a price bound that
  never goes down, and is held when it does not fit; fills are always
  recorded, and the plan releases exactly the reservations it holds.

### Modules

| Module | Content |
|---|---|
| `exact` | exact `Decimal` add, subtract, multiply and power-of-ten division |
| `opportunity` | `evaluate`: the net edge of a two-instrument opportunity |
| `hedge` | `plan_band_hedge`: band hedging of one exposure |
| `hedge_action` | `hedge_plan_from_action`: bounded conversion of an external action |
| `funding` | funding rates, schedules and cashflows |
| `basis` | `evaluate_spot_perp`: the net edge of a spot-perpetual basis trade |
| `legs` | `TwoLegPlan`: two-leg execution state machine |
| `venues` | venue views, budgets and two-leg admission |
| `parity` | `evaluate_parity`: put-call parity edge against a future |
| `package` | `PackagePlan`: multi-leg package execution on a common lot grid |
| `error` | `ArbitrageError`, the crate's typed error |

The monetary types are at the crate root: `Currency`, `Money`, `Cost`,
`FeeRate`, `Price`, `Quantity`, `Rounding`, and the instrument types
`Instrument`, `InstrumentId`, `Action`, `Level`, `TopOfBook`, `Leg` and
`Timestamp`. `Action` is the trade direction shared with other crates
([`financial_types::Action`](https://docs.rs/financial_types)); only
`Action::Buy` and `Action::Sell` are supported, and any other action is
`ArbitrageError::UnsupportedAction`.

### Exact arithmetic

- Every amount, price, size and rate is a `rust_decimal::Decimal` (96-bit
  mantissa, up to 28 decimal places). There is no floating point in the
  arithmetic; `clippy::float_arithmetic` is denied. The only float input is a
  hedge action's quantity, converted once from its exact binary value.
- `rust_decimal`'s `checked_*` operations round silently when a result needs
  more precision than a `Decimal` holds. The crate detects that and returns
  `ArbitrageError::Inexact`; out-of-range results return
  `ArbitrageError::Overflow`.
- A figure built from more than two terms (a sum of products, or a product
  of more than two factors) is formed exactly in 768-bit integers and
  converted or rounded **once**, so an intermediate never fails when the
  result is representable. The rule and its audit are in
  [`docs/exact-arithmetic.md`](docs/exact-arithmetic.md).
- Rounding happens only where a caller asks for it, with an explicit
  `Rounding` mode: `Up`, `Down`, `TowardZero` or `HalfEven`.

### Units and signs

- `Money` is a signed amount tagged with its `Currency`. Combining different
  currencies is an error; the crate never converts.
- `Price` is quote currency per unit of `Quantity`; both are strictly
  positive. A quantity has no direction: legs and positions carry it.
- `Cost` is signed from the payer's side: positive is paid, negative is
  received (a rebate). A net result is `gross - cost`.
- `FeeRate` is in basis points of traded notional, within
  `[-10_000, 10_000]`; negative rates are rebates.

### Quick Start

A spread between two venues, accepted after fees:

```rust
use arbitrage_rs::opportunity::{evaluate, Evaluation, LegQuote, OpportunityInput};
use arbitrage_rs::{
    Currency, FeeRate, Instrument, InstrumentId, Level, Money, Price, Quantity, Timestamp,
    TopOfBook,
};
use rust_decimal_macros::dec;

# fn main() -> Result<(), arbitrage_rs::ArbitrageError> {
let usd = Currency::new("USD")?;
let level = |p, s| Some(Level::new(Price::new(p).unwrap(), Quantity::new(s).unwrap()));
let quote = |id: &str, bid, ask, size| -> Result<LegQuote, arbitrage_rs::ArbitrageError> {
    Ok(LegQuote {
        instrument: Instrument::undated(InstrumentId::new(id)?),
        book: TopOfBook { bid: level(bid, size), ask: level(ask, size) },
        fee: FeeRate::from_bps(dec!(5))?,
    })
};

let input = OpportunityInput {
    as_of: Timestamp::from_millis(1_000),
    quote_currency: usd,
    buy: quote("BTC-A", dec!(99.90), dec!(100.00), dec!(5))?,
    sell: quote("BTC-B", dec!(100.30), dec!(100.40), dec!(3))?,
    target_quantity: Quantity::new(dec!(4))?,
    min_quantity: Quantity::new(dec!(1))?,
    carry: vec![],
    min_net_edge: Money::new(dec!(0), usd),
};

// Size 3 (the sell bid's size): gross 0.90, fees 0.15 + 0.15045.
let Evaluation::Accepted(opp) = evaluate(&input)? else { panic!("rejected") };
assert_eq!(opp.expected_gross.amount(), dec!(0.90));
assert_eq!(opp.expected_costs.total.money().amount(), dec!(0.30045));
assert_eq!(opp.expected_net.amount(), dec!(0.59955));
# Ok(())
# }
```

Executing it as a two-leg plan, when the second leg is rejected:

```rust
# use arbitrage_rs::opportunity::{evaluate, Evaluation, LegQuote, OpportunityInput};
# use arbitrage_rs::{
#     Currency, FeeRate, Instrument, InstrumentId, Level, Money, Price, Quantity, Timestamp,
#     TopOfBook,
# };
use arbitrage_rs::legs::{ExecId, FailurePolicy, LegExecution, LegIndex, PlanAction, PlanState, TwoLegPlan};
use arbitrage_rs::venues::VenueId;
use arbitrage_rs::{Action, Cost};
use rust_decimal_macros::dec;

# fn main() -> Result<(), arbitrage_rs::ArbitrageError> {
# let usd = Currency::new("USD")?;
# let level = |p, s| Some(Level::new(Price::new(p).unwrap(), Quantity::new(s).unwrap()));
# let quote = |id: &str, bid, ask, size| -> Result<LegQuote, arbitrage_rs::ArbitrageError> {
#     Ok(LegQuote {
#         instrument: Instrument::undated(InstrumentId::new(id)?),
#         book: TopOfBook { bid: level(bid, size), ask: level(ask, size) },
#         fee: FeeRate::from_bps(dec!(5))?,
#     })
# };
# let input = OpportunityInput {
#     as_of: Timestamp::from_millis(1_000),
#     quote_currency: usd,
#     buy: quote("BTC-A", dec!(99.90), dec!(100.00), dec!(5))?,
#     sell: quote("BTC-B", dec!(100.30), dec!(100.40), dec!(3))?,
#     target_quantity: Quantity::new(dec!(4))?,
#     min_quantity: Quantity::new(dec!(1))?,
#     carry: vec![],
#     min_net_edge: Money::new(dec!(0), usd),
# };
# let Evaluation::Accepted(opp) = evaluate(&input)? else { panic!("rejected") };
let mut plan = TwoLegPlan::from_opportunity(
    &opp,
    VenueId::new("VENUE-A")?,
    VenueId::new("VENUE-B")?,
    Some(Timestamp::from_millis(5_000)),
    FailurePolicy::Unwind,
    usd,
)?;
// The buy leg fills in full.
plan.apply(&LegExecution::Fill {
    leg: LegIndex::First,
    attempt: 0,
    exec_id: ExecId("a-1".to_owned()),
    quantity: Quantity::new(dec!(3))?,
    cum_qty: Quantity::new(dec!(3))?,
    price: Price::new(dec!(100.00))?,
    fee: Cost::new(Money::new(dec!(0.15), usd)),
    at: Timestamp::from_millis(1_100),
})?;
// The sell leg is rejected: the plan asks to offset the residual.
let report = plan.apply(&LegExecution::Rejected {
    leg: LegIndex::Second,
    attempt: 0,
    at: Timestamp::from_millis(1_200),
})?;
assert_eq!(report.state, PlanState::UnwindRequested);
assert_eq!(report.residual, dec!(3));
assert!(matches!(
    &report.actions[..],
    [PlanAction::Unwind { side: Action::Sell, .. }]
));
# Ok(())
# }
```

The executable edge of put-call parity against a same-expiry future, with
options of 0.1 and futures of 0.01 underlying per contract:

```rust
use arbitrage_rs::parity::{
    evaluate_parity, OptionFee, ParityDirection, ParityFuture, ParityInput, ParityOption,
};
use arbitrage_rs::{
    Currency, FeeRate, Instrument, InstrumentId, Level, Price, Quantity, Rounding, Timestamp,
    TopOfBook,
};
use rust_decimal_macros::dec;

# fn main() -> Result<(), arbitrage_rs::ArbitrageError> {
let q = |x| Quantity::new(x).unwrap();
let book = |bid, ask, size| TopOfBook {
    bid: Some(Level::new(Price::new(bid).unwrap(), q(size))),
    ask: Some(Level::new(Price::new(ask).unwrap(), q(size))),
};
let expiry = Timestamp::from_millis(1_000_000);
let dated = |id: &str| Instrument::dated(InstrumentId::new(id).unwrap(), expiry);
let option = |id: &str, bid, ask| ParityOption {
    instrument: dated(id),
    book: book(bid, ask, dec!(50)),
    contract_size: q(dec!(0.1)),
    lot: q(dec!(1)),
    // 3 bps of the underlying notional, at most 12.5 % of the premium.
    fee: OptionFee::UnderlyingCapped { rate: FeeRate::from_bps(dec!(3)).unwrap(), cap: dec!(0.125) },
};

let input = ParityInput {
    as_of: Timestamp::from_millis(0),
    quote_currency: Currency::new("USDC")?,
    strike: Price::new(dec!(100))?,
    discount: dec!(0.99),
    index: Price::new(dec!(101.4037))?,
    call: option("XYZ-100-C", dec!(6.00), dec!(6.10)),
    put: option("XYZ-100-P", dec!(4.90), dec!(5.00)),
    future: ParityFuture {
        instrument: dated("XYZ-F"),
        book: book(dec!(101.50), dec!(101.60), dec!(500)),
        contract_size: q(dec!(0.01)),
        lot: q(dec!(1)),
        fee: FeeRate::from_bps(dec!(5))?,
    },
    package: q(dec!(1)),
    fee_dp: 6,
    fee_rounding: Rounding::TowardZero,
    edge_dp: 6,
    edge_rounding: Rounding::HalfEven,
};

// Reversal: 0.99 * (101.50 - 100) - (6.10 - 4.90) = 0.285 gross, minus
// fees of 0.030421 + 0.030421 + 0.05075.
let edge = evaluate_parity(&input)?;
let best = edge.executable().expect("an edge");
assert_eq!(best.direction, ParityDirection::Reversal);
assert_eq!(best.expected_gross.amount(), dec!(0.285));
assert_eq!(best.expected_net.amount(), dec!(0.173408));
assert_eq!(best.legs.future.quantity.value(), dec!(100));
# Ok(())
# }
```

More examples are in the crate documentation and in `tests/`.

### Feature flags

| Feature | Default | Effect |
|---|---|---|
| `instruments` | off | The `spec` module: an exact conversion of a `financial_types` instrument specification into `Instrument`, `Currency`, `PerpConvention` and contract terms, with typed refusals. Enables `financial_types/instruments`. |

`instruments` needs `financial_types` 0.3.1 or later: Cargo does not upgrade a
lockfile at 0.3.0 by itself, so run `cargo update -p financial_types`.

### Dependency direction

arbitrage-rs depends only on `rust_decimal`, `thiserror` and
`financial_types` (shared enums; it brings `serde` with its derive macros,
which arbitrage-rs does not use). It does not depend on any simulator, backtest engine or connector. Integration goes the
other way: the consumer, or an adapter next to it, depends on arbitrage-rs
and translates its prices, inventory and ledgers into these types.

### Status

`0.3.1` is the current release. It changes no library code; `0.3.0`
added budget gating for two-leg plans,
unwind admission and inverse perpetual funding, with breaking changes to
`LegSpec`, `PlanAction::Unwind`, `AdmittedLeg` and `LegExecution` (see the
changelog). The crate follows semantic versioning: below
`1.0.0`, a minor version bump (`0.x.0`) may carry breaking changes, and a
patch release does not. Changes are listed in
[`CHANGELOG.md`](CHANGELOG.md). `Decimal` is re-exported and part of the
public API, so `rust_decimal` 1.x is too.

MSRV: Rust 1.88 (edition 2024), checked in CI.

## 🛠 Makefile Commands

```sh
make build         # Compile the project
make test          # Run all tests and doctests
make fmt           # Format code
make fmt-check     # Check formatting without applying
make lint          # Run clippy with warnings as errors
make doc           # Build the docs with warnings as errors
make msrv          # Check the build on the minimum supported Rust version
make pre-push      # fmt-check, lint, test, doc and msrv: run before every push
make clean         # Remove build artifacts
```

## Contribution and Contact

We welcome contributions to this project! If you would like to contribute, please follow these steps:

1. Fork the repository.
2. Create a new branch for your feature or bug fix.
3. Make your changes and ensure that the project still builds and all tests pass.
4. Commit your changes and push your branch to your forked repository.
5. Submit a pull request to the main repository.

If you have any questions, issues, or would like to provide feedback, please feel free to contact the project
maintainer:

### **Contact Information**
- **Author**: Joaquín Béjar García
- **Email**: jb@taunais.com
- **Telegram**: [@joaquin_bejar](https://t.me/joaquin_bejar)
- **Repository**: <https://github.com/joaquinbejar/arbitrage-rs>
- **Documentation**: <https://docs.rs/arbitrage-rs>


We appreciate your interest and look forward to your contributions!

**License**: MIT

<!-- related-projects:start -->
## Related projects

Repositories by the same author that this project depends on, and repositories that depend on it.

### Depends on

[financial_types](https://github.com/joaquinbejar/financial_types): the shared `Action` enum and, with the `instruments` feature, its instrument specifications.

### Used by

| Repository | Description |
|------------|-------------|
| [market-maker-rs](https://github.com/joaquinbejar/market-maker-rs) | Quantitative market making strategies, starting with the Avellaneda-Stoikov model. Planned integration: it does not depend on arbitrage-rs yet. |

<!-- related-projects:end -->
