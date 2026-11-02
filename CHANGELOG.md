# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> **Pre-1.0 note:** while the crate is below `1.0.0`, a minor version bump
> (`0.x.0`) may carry breaking changes; a patch release does not.

## [Unreleased]

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
