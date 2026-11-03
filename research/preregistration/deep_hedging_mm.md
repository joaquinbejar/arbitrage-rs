# Preregistration: deep hedging against the market-maker-rs counterpart (#43)

Fixed on 2026-11-03, before any run of `experiments/deep_hedging_mm.py`.
The run records the sha256 of this file and refuses to start if it differs
from `PREREGISTRATION_SHA256` in that script, so this text cannot change
after the run without the change showing.

## What is compared

- Data, splits, seeds and learned controls are those of M5-06b
  (`deep_hedging_counterpart.json`). The frozen weights are checked
  against their recorded sha256, and nothing is retrained.
- **Surface B (decisive).** market-maker-rs calibrates raw SVI slices
  (`counterpart calibrate`, market-maker-rs #96) on the reference grid of
  Heston prices: one expiry a day for 30 days, `k = k_std sqrt(v0 tau)`
  with `k_std` from -4 to 4 (33 points), the out-of-the-money side.
  - Prices below `min_price` (1e-10) are dropped.
  - Each price is quoted with a relative half band of 1e-4 around it. This
    is a declared observation model, not an observed spread: the cleaning
    refuses a quote with no band.
  - Calibrated once at t = 0 from those prices only.
  - The quotes and the surface file are committed
    (`research/results/deep_hedging_mm_surface_b.{csv,json}`). The run
    recomputes both and stops on any difference: the quotes byte for byte,
    and every field of the surface file except `identity`, which names the
    binary that wrote it.
- **Counterparts.** market-maker-rs on surface B, under each dynamics,
  gives the delta, the band baseline and the premium (sticky moneyness at
  t = 0). The reference prices on the same slices, exported bit for bit,
  for the comparison. Surface A (the slices Python fits, as in M5-06b) is
  a diagnostic only.
- **The binary** is the one `experiments/mm_counterpart_manifest.json`
  pins: sha256, market-maker-rs commit, clean tree, build command,
  toolchain and host.
- `vol_shift` is 0 throughout the run. It is recorded in every
  counterpart's `config_sha256`; the conformance suite alone uses nonzero
  shifts, for vega by finite differences.

## Wings: semantics

Both implementations evaluate the raw SVI formula of each slice at any
`k`, so outside the calibrated range a slice follows its own wings.

- market-maker-rs `SviSurface::total_variance` does not reject a `k`
  outside `k_range`; its `region()` labels the point `Extrapolated`
  (`src/options/surface/svi.rs`, "Interpolation and extrapolation").
- Python `SviSurface` uses the same formula for every `k`.
- Between expiries both are linear in `w`.
- No range is widened in the harness, and no rejection is relabelled.

This supports numerical consistency between the two counterparts. It
does not validate the wings economically: no quote supports them.

## Coverage, measured before the run (2026-11-03)

These figures come from the Heston test paths and the slices of surface B
alone; no price was compared. A (path, day) row reads the slices on both
sides of its `tau`, or, on a node, that node and the segment towards
shorter expiries (the one theta uses).

- A row is **out of domain** when its `tau` is outside the slices: 0 rows
  under both dynamics.
- A row is **extrapolated** when its `k` (`ln(K / S_t)` under sticky
  moneyness, `ln(K / s_ref)` under sticky strike) is outside the
  intersection of the `k_range` of the slices it reads.
  - Under sticky strike: 0 rows.
  - Under sticky moneyness: 5.19 % of 983 040 rows.

Share of extrapolated rows under sticky moneyness, by days to expiry:

| days | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 14 | 15 | 16 | 17 | 18 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| % | 43.9 | 43.2 | 25.4 | 15.5 | 9.6 | 6.1 | 4.1 | 2.8 | 1.9 | 1.2 | 0.8 | 0.5 | 0.3 | 0.2 | 0.1 | 0.07 | 0.03 | 0.02 |

From 19 to 30 days the share is 0. 17 056 of the 32 768 paths stay in
range on every day.

**The original rule would give REVISE.** That rule excluded `k` outside
`k_range` and gave REVISE above 1 %; on these data it yields REVISE. The
run records that outcome next to the amended one.

**Quote audit.** All 990 Heston quotes are positive and above
`min_price`, and all of them enter the fit:
- the smallest out-of-the-money mid is 6.6e-8;
- no quote was dropped, rejected or failed its implied variance.

The calibrated range is narrow at short expiries because the grid is set
in units of the at-the-money standard deviation (`k_std` up to 4). Zero
or underflowing quotes play no part. Widening the grid would add quotes
below `min_price` and below the 28-decimal resolution of the quotes
(about e^-180 at `k = 0.2`, one day), which the declared filter drops.

## The amended rule

- **Extrapolated is a label, never an exclusion or an exemption.** Every
  row stays in every total, and is priced and compared.
- **Any error fails the run.** That covers prices, Greeks and `w`, in the
  wings too, and every class (InvalidInput, OutOfDomain, Unsupported,
  Numerical, protocol and identity).
- **The 1 % bound applies to out-of-domain rows** (a `tau` outside the
  slices), counted with their own denominator per dynamics. On these data
  there are none.
- **Comparison.** On every (path, day) row and every output (price,
  delta, gamma, vega, theta), the check is
  `|mm - ref| <= max(abs_tol, rel_tol |ref|)`, with the tolerances of
  `counterpart.AGREEMENT_TOLERANCES`:

  | output | abs_tol | rel_tol |
  |---|---|---|
  | price | 1e-7 | 1e-9 |
  | delta | 1e-8 | 0 |
  | gamma | 1e-10 | 1e-6 |
  | vega | 1e-7 | 1e-8 |
  | theta | 1e-7 | 1e-6 |

  The tolerances are the same in three panels: the whole population,
  in-range rows and extrapolated rows. The report shows the extrapolated
  share by day and dynamics, and the largest vega there, in its headline
  table. Small vega never excuses a difference.

## Decision

The criterion is M5-06b's, on surface B with the market-maker-rs
counterpart, at the primary half spread (5 bp):

- in_model holds when, for every seed, the upper bound of the paired 95 %
  interval of CVaR95(learned) - CVaR95(band) is below 0;
- veto(D) holds when some seed has its lower bound above 0 under Heston
  with dynamics D;
- ADOPT iff in_model holds and there is no veto under either dynamics;
  REJECT iff in_model fails or there is a veto under both; REVISE
  otherwise.

**ADOPT also requires every gate below.** A failed gate turns ADOPT into
REVISE.

1. The conformance suite passes on the pinned binary.
2. Surface B is not failed, and its recomputed quotes and file equal the
   committed ones.
3. At most 1 % of the rows are out of domain, under each dynamics.
4. Every required row is priced. An error stops the run, so this gate
   holds by construction whenever the run produces a report.
5. All three panels agree within the tolerances, under both dynamics.
6. The in-model rows equal M5-06b's.
7. **The conclusion holds on the in-range subset alone.** On the paths
   that stay in range on every day, with the same paired bootstrap, no
   seed may have its lower bound above 0 under either dynamics.

The conclusions keep counterpart consistency (the comparison) apart from
the hedging result. They also keep calibrated support apart from analytic
extrapolation.
