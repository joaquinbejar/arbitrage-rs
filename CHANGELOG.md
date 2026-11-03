# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> **Pre-1.0 note:** while the crate is below `1.0.0`, a minor version bump
> (`0.x.0`) may carry breaking changes; a patch release does not.

## [Unreleased]

### Fixed

- Research, outside the crate (#36, #37): the reference pricing
  counterpart rejects boolean or non-numeric spots and `as_of_days`
  (the check ran after the cast to float and never fired); the M5-06b
  report states where the SVI fit is on or near rho = -1; the dataset
  validator prints the tick as declared in the manifest.

## [0.3.1] - 2026-11-03

No change to the library API or behaviour; this release packages the
research below and the fixture used by `tests/hedge.rs`.

### Added

- Research, outside the crate (M5-06, #6): `experiments/deep_hedging.py`
  compares a learned hedge with the delta and the `plan_band_hedge` band on
  one synthetic setting; the report is `research/results/deep_hedging.md`.
  The band's decisions are cross-checked exactly in `tests/hedge.rs`, whose
  fixture `tests/fixtures/deep_hedging_band.csv` is now packaged.
- Research, outside the crate (M5-06b, #36): a pricing counterpart
  interface for the deep hedging experiment (`experiments/counterpart.py`),
  a reference implementation (Black-Scholes-Merton on raw SVI slices
  fitted to semi-analytic Heston prices), an empty market-maker-rs adapter
  and a conformance suite. `deep_hedging.py --counterpart reference` reruns
  the comparison against it; the report,
  `research/results/deep_hedging_counterpart.md`, is pending counterpart
  validation.
- Research, outside the crate (M5-06c, #37): a dataset interface and
  schema validator (`experiments/dataset.py`) and an empirical runner
  (`experiments/empirical.py`) that tests the frozen deep hedging controls
  on a dataset's book and queue-model capacities without retraining. It
  is checked on a synthetic dataset (`experiments/datasets/synthetic-v1`),
  a pipeline check that supports no empirical claim.

## [0.3.0] - 2026-11-02

### Breaking

- `LegSpec::venue` is required: `VenueId` instead of `Option<VenueId>`, so
  a leg without a venue cannot be expressed. The "leg has no venue" errors
  of `check_view` and `TwoLegPlan::apply` are gone (#16, #31).
- `TwoLegPlan::from_opportunity` takes the venue of each leg
  (`buy_venue`, `sell_venue`), and `PackagePlan::from_parity` the venue of
  the package (#16, #31).
- `AdmissionReject` has new variants: `Released` and `CoverageUnknown`
  (#31), `Disconnected` and `UnwindReversal` (#34). The enum is
  `#[non_exhaustive]`.
- `PlanAction::Unwind` carries the unwind's index (`unwind`), its `venue`
  and, on a plan with budget gating, its admitted limit (`price`; `None`
  without gating). `AdmittedLeg` has a new field, `unwind_funding`, the
  funding of a sell that unwinds that leg. `LegExecution` has a new
  variant, `UnwindDone` (#30, #34).

### Added

- Inverse perpetual funding (M3-06b, #13, #32): `PerpConvention::Inverse`
  with a `ContractValue` (quote units per contract, "face") and the base
  currency in which funding is paid. `funding_cashflow` returns the exact
  amount `contracts * contract_value / mark * rate` (positive is paid; longs
  pay when the rate is positive), formed as one exact quotient in the wide
  integers, or `Inexact` when it does not terminate within 28 places, which
  is the usual case. `funding_cashflow_rounded` settles it with one rounding
  to the settlement quantum in the explicit mode. A settlement currency other
  than the convention's base is `CurrencyMismatch`.

- Budget gating for two-leg plans (#16, #31):
  `TwoLegPlan::with_budget_gating` takes over the admission's
  reservations (`AdmittedLeg`, `LegFunding`) after checking that each is
  exactly its leg's requirement, and `TwoLegPlan::apply_with_budgets`
  readmits every resubmission against its venue's current budget before
  sending it: the leg's whole quantity at a price bound that never goes
  down (the admission's bound, fill prices above it and, for a sell, the
  bid at each readmission), released and reserved again all or nothing on
  the leg's own venue. A resubmission that does not fit is held
  (`LegState::hold`) and retried on later non-fill events (send a `Tick`
  after changing a budget); past the deadline the fallback applies. Fills
  are always recorded: what they use beyond the reservation (a price above
  the covered bound, cumulative fees above the fee modelled at it, an
  overfill, or an unknown coverage when it cannot be computed exactly) is
  reported in `LegState::deficit` (`CoverageDeficit`), and the next
  readmission covers the price and the fees, or holds
  (`AdmissionReject::CoverageUnknown`).
  `TwoLegPlan::release_reservations` releases the current reservations
  once, idempotently. Gating is configured once, before any event.
  `TwoLegPlan::apply` refuses a gated plan, and
  `apply_with_budgets` and `release_reservations` refuse a plan without
  gating. `AdmissionReject::CoverageUnknown` is permanent for the rest of
  the plan.

- Unwind admission (#30, #34): on a gated plan every unwind is admitted
  against its venue's budget before it is sent, at the executable price of a
  fresh view (the bid for a sell, the ask for a buy), with the fee and
  funding of that view, reserved all or nothing and held until
  `release_reservations`. An unwind that cannot be admitted is held
  (`TwoLegPlan::unwind_hold`) with the new rejects `Disconnected` and
  `UnwindReversal`, or the existing ones, and retried on later non-fill
  events; a fill never admits one. The caller reports each unwind's end with
  `LegExecution::UnwindDone` (exact duplicates are no-ops, conflicts are
  `InvalidInput`), credits the venue budget with what the plan's legs
  filled, and sends a `Tick`; the plan then requests what is still open. The
  open residual is the residual less the coverage of the unwinds
  (`TwoLegPlan::unwind_coverage`, live unwinds at their quantity, ended ones
  at their fill), and the plan never sends the opposite side while an unwind
  is live. `TwoLegPlan::with_unwind_deadline` stops new unwind admissions at
  a time (`PastDeadline`), `TwoLegPlan::unwinds` lists them (`UnwindState`),
  and `release_reservations` is refused while an unwind is live.

### Changed

- `basis::evaluate_spot_perp` refuses an inverse perpetual with
  `InvalidInput` before computing anything: its kernel takes the position in
  base units, funding in the quote currency and a PnL linear in price (#32).

## [0.2.0] - 2026-11-02

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
  rejection or plan mutation, and every entry point that can return it
  names it in its `# Errors` (#28, #29).
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

[Unreleased]: https://github.com/joaquinbejar/arbitrage-rs/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/joaquinbejar/arbitrage-rs/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/joaquinbejar/arbitrage-rs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/joaquinbejar/arbitrage-rs/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/joaquinbejar/arbitrage-rs/releases/tag/v0.1.0
