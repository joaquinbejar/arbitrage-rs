# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> **Pre-1.0 note:** while the crate is below `1.0.0`, a minor version bump
> (`0.x.0`) may carry breaking changes; a patch release does not.

## [Unreleased]

### Breaking

- The trade direction is `financial_types::Action`, re-exported at the
  crate root; the local `Side` is removed, with no alias (`financial_types`
  also has a `Side`, the position side `Long`/`Short`). Migrate `Side::Buy`
  and `Side::Sell` to `Action::Buy` and `Action::Sell` (#28).
- `Action` is a foreign `#[non_exhaustive]` enum with an `Other` variant,
  so the issue's "no `_ =>` arm" cannot hold literally: the crate converts
  an action once, at its entry points, into a private two-variant
  direction, the only conversion with a wildcard arm; every other match is
  exhaustive. Any action other than buy or sell is the new error
  `ArbitrageError::UnsupportedAction { action }`, checked before any
  rejection or plan mutation (#28).
- `TopOfBook::executable` returns `Result<Option<Level>>`: `None` still
  means an empty side of the book, and an unsupported action is an error,
  never confused with missing liquidity (#28).
- `Display` of a direction is `Buy` / `Sell` (was `buy` / `sell`) (#28).
- The minimum supported Rust version is 1.88, required by `financial_types`
  (#28).

### Added

- Dependency on `financial_types` 0.3 (default features). It brings `serde`
  and its derive macros transitively; arbitrage-rs serializes nothing (#28).

### Changed

- The library is checked with `clippy::pedantic`, `unwrap_used`,
  `expect_used`, `panic`, `indexing_slicing` and `arithmetic_side_effects`
  denied. No public behavior changes: index accesses use checked lookups
  with typed errors for states the crate never builds, and integer
  arithmetic is checked or explicitly wrapping where it cannot overflow.
  `unreachable`, `todo` and `unimplemented` are denied too. Test modules
  and `tests/` opt out of these lints explicitly (#27).

## [0.1.0] - 2026-11-02

First release.

### Added

- Exact monetary types: `Currency`, `Money`, `Cost`, `FeeRate`, `Price`,
  `Quantity` and `Rounding`, and the `exact` module, whose arithmetic fails
  with `ArbitrageError::Inexact` or `ArbitrageError::Overflow` instead of
  rounding silently (#7).
- `opportunity::evaluate`: the net edge of buying one instrument at its ask
  and selling another at its bid, after fees and itemized carry, against a
  minimum net edge. `hedge::plan_band_hedge`: a one-leg hedge back inside an
  exposure band, bounded by quoted size, a maximum quantity and a lot size,
  with its expected fee and half-spread cost (#2).
- `funding`: linear perpetual funding cashflows, schedules, estimated and
  published rates, and a single rounding to a settlement quantum.
  `basis::evaluate_spot_perp`: the net edge of a spot-perpetual basis trade
  after entry and exit fees, carry and funding, with a look-ahead check (#3).
- `legs::TwoLegPlan`: a two-leg execution state machine with per-attempt
  cumulative quantities, idempotent events, recorded overfills, deadlines
  and expiries, and the failure policies abort, unwind and complete (#8).
- `venues`: per-venue views and budgets, all-or-nothing admission of both
  legs without borrowing capital across venues, inventory or margin funding
  for sells, and per-leg clocks, connection state and gated resubmissions in
  `TwoLegPlan` (#9).
- `hedge_action::hedge_plan_from_action`: a bounded, audited conversion of
  an external policy's hedge action (exact or floating-point) into a hedge
  plan, with reduce-only and exposure-cap limits (#5).
- `parity::evaluate_parity`: the executable edge of put-call parity against
  a same-expiry future in both directions, with the discount factor,
  contract multipliers and the fees of all three legs (#18).
- `package::PackagePlan`: multi-leg package execution with legs of
  different contract sizes and lots, a matched package on a common lot grid,
  and recovery on each leg's excess (#19).
- 768-bit wide-integer helpers: any figure built from more than two terms is
  formed exactly and converted or rounded once. The rule and its audit are
  in `docs/exact-arithmetic.md` (#21). With it, `FeeRate::fee_on` is one
  wide product, and an unrepresentable fee is reported as
  `ArbitrageError::Inexact { op: "fee" }` (before #21 the `op` was
  `"div_pow10"`).
- A complete README with examples that run as doctests (#23).

[Unreleased]: https://github.com/joaquinbejar/arbitrage-rs/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/joaquinbejar/arbitrage-rs/releases/tag/v0.1.0
