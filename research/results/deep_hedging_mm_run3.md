# Deep hedging against the market-maker-rs counterpart, run 3 (#43)

Outcome: **ADOPT** (criterion adopt, all 7 gates hold), the preregistered result. Its scope is further validation, in this synthetic setting only.

Run 3 is a new held-out evaluation after run 2, with no prior access to its paths. The learned hedge keeps its in-model result, and on a new Heston set no seed is significantly worse than the band under either dynamics. market-maker-rs and the reference agree on every row within the tolerances. This is the counterpart result that the reference run was pending. It says nothing about live markets (see "Limitations"), and it rests on the amended `k` rule (see "Not in the decision").

## Protocol

- Preregistration: `research/preregistration/deep_hedging_mm_v2.md`.
- Code commit C `b7780a802e7281356275773e2c8ad8c40ff8a671`; preregistration sha256 `0632e0689f583f4814693c7645c6d8aa4ea212777d969da0e3324acf78f2e080`; template sha256 `04af7765d5873f5d21ded0d116589c58a93f1e974ed4bfcbaa3bef684d94da2b`; final manifest sha256 `baaa9b767a04bd86f9e5809cdee0affc39102420018f4456ad9cb446df10fee6`.
- Launch commit `f1a387074aa8b0bc7a03782f3c230186b7f58ef8`, a signed child of C that adds only the final manifest. Two independent approvers confirmed it after the run.
- Seeds, drawn by the maintainer after both approvals with `secrets.randbelow(2**64 - 299)` in one draw: `eval_seed` 7161644997987983501, `bootstrap_seed` 16423988309481355619.
- Authorized at 2026-11-03T18:34:28Z; attempt 1 started at 2026-11-03T18:37:09Z, inside the one hour window, and finished in 197.5 s. One attempt, no restart, no exception record, no preflight failure.
- Results sha256 `a2590b2841128e0f15855bf033d5c12c175799c3b3d47fa58b6f9d8119b33ac1`, committed byte for byte. The file stayed unopened until both approvers had confirmed the launch commit and the maintainer gave the go; that is a procedural commitment, which the records cannot prove. The stage log carries no outcome.
- market-maker-rs 0.6.1 at `010e8183734b4cae489c230c071a94cc20a1f7c6`, binary sha256 `696948412d7b526268504569e48de3c254f44c639b1133ae8108b5662a5a3abe`.

## Earlier runs and the dry run

- Run 1 aborted before writing any result (`research/results/deep_hedging_mm_failed_run_1.json`). It keeps that outcome.
- Run 2 gave **REVISE** (`research/results/deep_hedging_mm.json`). It is not reclassified: gate 6 failed there as preregistered, and this run does not change that.
- A dry run of the whole pipeline ran once before C was fixed, on commit `05e2c9e`, with dummy seeds now forbidden (`eval_seed` 4659386883093802759, `bootstrap_seed` 2043307074730982498). Only its exit code (0), duration (191.8 s), peak memory and gate 6 equality were read; everything else was deleted unread. It checked the code, and with other seeds it gave no access to this run's paths. The preregistration lists how that commit differs from C.

## Decision

| preregistered requirement | result |
|---|---|
| in_model: every seed's upper bound below 0 on the GBM test set | holds |
| no veto under sticky-moneyness: no seed's lower bound above 0 | no veto |
| no veto under sticky-strike: no seed's lower bound above 0 | no veto |
| gate `1_conformance_passed` | holds |
| gate `2_surface_b_not_failed_and_frozen` | holds |
| gate `3_out_of_domain_within_bound` | holds |
| gate `4_every_required_row_priced` | holds |
| gate `5_all_panels_agree` | holds |
| gate `6_in_model_matches_reference` | holds |
| gate `7_in_range_subset_no_veto` | holds |

Gate 6 compared 15 in-model rows and 255 shared fields with the reference run, bit for bit: 0 differing, 0 missing, 0 extra, 0 non-finite, 0 identity errors; the 15 `weights_sha256` fields match the weight record. It depends only on `data_seed`.

## Hedging at the primary half spread (0.0005)

CVaR95(learned) − CVaR95(band), estimate and paired 95 % interval. Negative favours the learned hedge.

| seed | in model (GBM) | Heston, sticky moneyness | Heston, sticky strike |
|---|---|---|---|
| 11 | -0.0212 [-0.0365, -0.0057] | -0.1843 [-0.2032, -0.1651] | -0.0969 [-0.1152, -0.0783] |
| 12 | -0.0293 [-0.0440, -0.0138] | -0.1793 [-0.2001, -0.1598] | -0.0919 [-0.1119, -0.0729] |
| 13 | -0.0391 [-0.0541, -0.0246] | -0.2018 [-0.2203, -0.1825] | -0.1144 [-0.1332, -0.0956] |
| 14 | -0.0302 [-0.0449, -0.0164] | -0.1704 [-0.1884, -0.1499] | -0.0830 [-0.1005, -0.0633] |
| 15 | -0.0288 [-0.0445, -0.0139] | -0.1674 [-0.1885, -0.1459] | -0.0800 [-0.1001, -0.0608] |

| control | in model (GBM) | Heston, sticky moneyness | Heston, sticky strike |
|---|---|---|---|
| CVaR95 delta | 1.7183 | 2.0239 | 1.9362 |
| CVaR95 band | 1.7175 | 2.0223 | 1.9350 |

The criterion asks only that there is no veto out of model; here every Heston interval is also below 0. In model the margin is small: estimates -0.0391 to -0.0212 against a band CVaR95 of 1.7175, with the tightest upper bound at -0.0057.

## Gate 7: the paths that stay in range on every day

| seed | sticky-moneyness (16849 of 32768 paths) | sticky-strike (32768 of 32768 paths) |
|---|---|---|
| 11 | -0.4276 [-0.4540, -0.4005] | -0.0969 [-0.1142, -0.0788] |
| 12 | -0.4464 [-0.4737, -0.4178] | -0.0919 [-0.1120, -0.0721] |
| 13 | -0.4391 [-0.4645, -0.4134] | -0.1144 [-0.1334, -0.0946] |
| 14 | -0.4063 [-0.4344, -0.3784] | -0.0830 [-0.1034, -0.0646] |
| 15 | -0.4509 [-0.4806, -0.4190] | -0.0800 [-0.1017, -0.0584] |

No veto under either dynamics. Under sticky moneyness the subset is 51.42 % of the paths and is selected on the whole path, future days included. It is a robustness gate, not a tradable condition, and its estimates are not effect sizes for the full population.

## Coverage and agreement

Extrapolated is a label, never an exclusion: every row is priced and compared.

| dynamics | rows | out of domain | extrapolated | max vega extrapolated | max price extrapolated |
|---|---|---|---|---|---|
| sticky-moneyness | 983040 | 0.00 % | 5.31 % | 1.020e-01 | 1.990e+01 |
| sticky-strike | 983040 | 0.00 % | 0.00 % | 0.000e+00 | 0.000e+00 |

Extrapolated share by days to expiry under sticky moneyness (below 0.005 % from day 20 on):

| days | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 14 | 15 | 16 | 17 | 18 | 19 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| % | 44.57 | 43.80 | 26.07 | 15.79 | 9.96 | 6.54 | 4.39 | 2.93 | 1.95 | 1.26 | 0.77 | 0.51 | 0.34 | 0.23 | 0.15 | 0.09 | 0.05 | 0.02 | 0.01 |

market-maker-rs against the reference on surface B, sticky-moneyness (violations must be 0):

| panel | output | rows | violations | max abs | max ratio | at (days, path) |
|---|---|---|---|---|---|---|
| all | price | 983040 | 0 | 3.730e-14 | 3.730e-07 | 6, 16567 |
| all | delta | 983040 | 0 | 4.774e-15 | 4.774e-07 | 1, 4039 |
| all | gamma | 983040 | 0 | 2.887e-15 | 4.789e-08 | 1, 2370 |
| all | vega | 983040 | 0 | 2.487e-14 | 2.487e-07 | 23, 4421 |
| all | theta | 983040 | 0 | 5.826e-13 | 4.697e-08 | 1, 22873 |
| in_range | price | 930803 | 0 | 3.730e-14 | 3.730e-07 | 6, 16567 |
| in_range | delta | 930803 | 0 | 4.774e-15 | 4.774e-07 | 1, 4039 |
| in_range | gamma | 930803 | 0 | 2.887e-15 | 4.789e-08 | 1, 2370 |
| in_range | vega | 930803 | 0 | 2.487e-14 | 2.487e-07 | 23, 4421 |
| in_range | theta | 930803 | 0 | 5.826e-13 | 4.697e-08 | 1, 22873 |
| extrapolated | price | 52237 | 0 | 3.553e-14 | 3.553e-07 | 2, 5455 |
| extrapolated | delta | 52237 | 0 | 6.661e-16 | 6.661e-08 | 1, 326 |
| extrapolated | gamma | 52237 | 0 | 1.258e-16 | 4.191e-08 | 1, 26678 |
| extrapolated | vega | 52237 | 0 | 1.901e-15 | 1.901e-08 | 2, 25921 |
| extrapolated | theta | 52237 | 0 | 3.597e-14 | 3.464e-08 | 2, 18695 |

market-maker-rs against the reference on surface B, sticky-strike (violations must be 0):

| panel | output | rows | violations | max abs | max ratio | at (days, path) |
|---|---|---|---|---|---|---|
| all | price | 983040 | 0 | 3.730e-14 | 3.730e-07 | 21, 22047 |
| all | delta | 983040 | 0 | 4.996e-15 | 4.996e-07 | 1, 24039 |
| all | gamma | 983040 | 0 | 2.998e-15 | 5.128e-08 | 1, 25881 |
| all | vega | 983040 | 0 | 2.309e-14 | 2.309e-07 | 20, 7219 |
| all | theta | 983040 | 0 | 6.466e-13 | 4.516e-08 | 1, 9514 |
| in_range | price | 983040 | 0 | 3.730e-14 | 3.730e-07 | 21, 22047 |
| in_range | delta | 983040 | 0 | 4.996e-15 | 4.996e-07 | 1, 24039 |
| in_range | gamma | 983040 | 0 | 2.998e-15 | 5.128e-08 | 1, 25881 |
| in_range | vega | 983040 | 0 | 2.309e-14 | 2.309e-07 | 20, 7219 |
| in_range | theta | 983040 | 0 | 6.466e-13 | 4.516e-08 | 1, 9514 |
| extrapolated | price | 0 | 0 | 0.000e+00 | 0.000e+00 |  |
| extrapolated | delta | 0 | 0 | 0.000e+00 | 0.000e+00 |  |
| extrapolated | gamma | 0 | 0 | 0.000e+00 | 0.000e+00 |  |
| extrapolated | vega | 0 | 0 | 0.000e+00 | 0.000e+00 |  |
| extrapolated | theta | 0 | 0 | 0.000e+00 | 0.000e+00 |  |

## Diagnostic surface A (never in the decision)

Surface A sha256 `a645f12784a3470517fa27e52d3ea6e68631ef2b4fa7df47ec6386c394ca07c3`. Rho is exactly -1.0 on days 1 to 16. Against run 2's surface A, 13 slices differ, on days 1, 2, 3, 4, 7, 8, 9, 10, 11, 12, 13, 14, 15, in rho only; days 5, 6 and 16 were already exactly -1.0. Every other parameter is bit for bit the same.

Every slice is now representable in market-maker-rs. Agreement with the reference: 0 violations, largest ratio 5.218e-07.

## Not in the decision

- **The original `k` rule gives REVISE.** It excluded `k` outside `k_range` and gave REVISE above 1 %; the extrapolated share is 5.31 % under sticky moneyness and 0.00 % under sticky strike. The first preregistration replaced that rule with the extrapolation label and gate 7. It did so before any run, but after measuring coverage on the earlier Heston set and seeing that the original rule would give REVISE; no price had been compared. Run 3 was preregistered with the amended rule. **The outcome above rests on that amendment: under the original rule it would be REVISE.**

Everything below is **exploratory**: reported for context, outside the preregistered decision, and not a test.

- **The other half spreads.** The criterion reads only the primary one.

| half spread | market | band w | largest upper bound of the interval | mean PnL learned | mean PnL band |
|---|---|---|---|---|---|
| 0 | in_model | 0.005 | +0.0100 | -0.0465 to -0.0454 | -0.0481 |
| 0 | heston_sticky-moneyness | 0.005 | -0.1266 | -0.0488 to -0.0476 | -0.0510 |
| 0 | heston_sticky-strike | 0.005 | -0.0378 | -0.0488 to -0.0476 | -0.0488 |
| 0.0005 | in_model | 0.01 | -0.0057 | -0.1567 to -0.1531 | -0.1689 |
| 0.0005 | heston_sticky-moneyness | 0.01 | -0.1459 | -0.1583 to -0.1557 | -0.1734 |
| 0.0005 | heston_sticky-strike | 0.01 | -0.0608 | -0.1583 to -0.1557 | -0.1693 |
| 0.005 | in_model | 0.15 | -0.1264 | -0.9205 to -0.8984 | -0.9042 |
| 0.005 | heston_sticky-moneyness | 0.15 | -0.1901 | -0.9262 to -0.9045 | -0.9312 |
| 0.005 | heston_sticky-strike | 0.15 | -0.1210 | -0.9262 to -0.9045 | -0.9088 |

  An interval reaches 0 only at half spread 0, in model, on one seed.
  Mean PnL is not part of the criterion, which is about tail loss. At half spread 0.005, in model and under sticky strike, some seeds have a lower mean PnL than the band while their CVaR95 is better.
- **The two dynamics.** The learned rows are the same under both; only the baselines change (band CVaR95 2.0223 under sticky moneyness, 1.9350 under sticky strike). The sticky strike column is therefore the more conservative comparison.
- **Against run 2's Heston set**, the estimates at the primary half spread are close: sticky-moneyness -0.2018 to -0.1674 (run 2: -0.2066 to -0.1722); sticky-strike -0.1144 to -0.0800 (run 2: -0.1200 to -0.0856). These are two draws, not a replication test.

## Limitations

- **Synthetic only.** The paths and surface B come from one Heston parameter set, and the surface shares those parameters. No market data enters this result, and ADOPT is not a statement about live trading.
- **The surface is static.** It is calibrated once at t = 0, and the two dynamics are modelling assumptions.
- **Extrapolation.** Near expiry under sticky moneyness a large share of rows is outside the calibrated `k` range (the table above). There the prices are analytic SVI wings with no quote behind them. Agreement between the two implementations there is numerical consistency, not economic validation.
- **The learned controls are frozen and were trained on GBM.** The evaluation covers one option, one hedge instrument and five seeds, and the band re-hedges to its centre.
- **The no-look commitments are procedural.** The records show the binding, the single attempt and the hashes; they cannot show that nobody looked.

## Reproducibility

python 3.12.16, numpy 2.5.4, torch 2.15.0, arm64, one thread everywhere, deterministic algorithms. The attempt's execution manifest, stage log (with the sha256 of each data array) and captured stdout are in `research/results/deep_hedging_mm_run3/attempt-1/`; its stderr was empty. A rerun with these seeds would reproduce the same paths and would not be a new evaluation.
