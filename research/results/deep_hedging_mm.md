# Deep hedging against the market-maker-rs counterpart (#43)

Outcome: **REVISE** (criterion adopt).

The counterpart's consistency (the comparison) and the hedging result are separate questions; the second rests on the first. Outside the calibrated `k` range the SVI wings are an analytic extrapolation, the same in both implementations, and no quote supports them.

- Preregistration sha256 `64346ef1a2734f3fddfe5ae1d7e78287ede63439abfc0c5c8240581eaaa404f9`
- market-maker-rs 0.6.1 at `010e8183734b4cae489c230c071a94cc20a1f7c6`, binary sha256 `696948412d7b526268504569e48de3c254f44c639b1133ae8108b5662a5a3abe`
- Surface B: healthy, file sha256 `5556e015f7e86e51a9da81fc64c1e6652031125d55348bfc07296f77595db42b`, quotes sha256 `773d94e0749dac6175ac143c1d041df7da12d1a6d5fff7a3ac2ab018f0fdb2d1`

## Earlier runs

- Run 1 (2026-11-03, head `ea7569db27c69553654a82719c97f186bf81912b`, preregistration `600e8a5ee5d968ee56bbf915ba5e026ca443ebef80645d43c9f4db7ce78ad735`): aborted: an error stopped the run at 108 s (exit code 1), before any result was written; there is no result artifact. Error: CounterpartUnavailable: market-maker-rs counterpart failed (2): counterpart: surface: invalid configuration: svi slice: |rho| must be at most 1, got -1.0000000000000002. Log `research/results/deep_hedging_mm_failed_run_1.log`, sha256 `a9a64b522fb6e8589f4f12cda179d32fdbbb52bf310ad9a02462a1ec5afdc53a`. Results seen: none. The log is the whole output: the line above (surface B status and the conformance gate) and the traceback. No value of the comparison on B, of the hedging, of the in-range subset or of the decision was printed or written.
  Slices outside the SVI domain: day 1 rho -1.0000000000000002 (bits bff0000000000001), day 4 rho -1.0000000000000002 (bits bff0000000000001), day 9 rho -1.0000000000000002 (bits bff0000000000001), day 10 rho -1.0000000000000002 (bits bff0000000000001), day 13 rho -1.0000000000000002 (bits bff0000000000001)

## Diagnostic surface A (the M5-06b Python fit)

Representable in market-maker-rs: **no**. Not compared (amendment 2).

| day | reason | rho | rho bits |
|---|---|---|---|
| 1 | rho outside [-1, 1] | -1.0000000000000002 | bff0000000000001 |
| 4 | rho outside [-1, 1] | -1.0000000000000002 | bff0000000000001 |
| 9 | rho outside [-1, 1] | -1.0000000000000002 | bff0000000000001 |
| 10 | rho outside [-1, 1] | -1.0000000000000002 | bff0000000000001 |
| 13 | rho outside [-1, 1] | -1.0000000000000002 | bff0000000000001 |

## Gates

| gate | holds |
|---|---|
| 1_conformance_passed | yes |
| 2_surface_b_not_failed_and_frozen | yes |
| 3_out_of_domain_within_bound | yes |
| 4_every_required_row_priced | yes |
| 5_all_panels_agree | yes |
| 6_in_model_matches_m5_06b | **no** |
| 7_in_range_subset_no_veto | yes |

The original rule (exclude `k` outside `k_range`, REVISE above 1 %) gives **revise**: extrapolated share sticky-moneyness 5.19 %, sticky-strike 0.00 %.

## Coverage and agreement

| dynamics | rows | out of domain | extrapolated | max vega extrapolated | max price extrapolated |
|---|---|---|---|---|---|
| sticky-moneyness | 983040 | 0.00 % | 5.19 % | 1.020e-01 | 1.893e+01 |
| sticky-strike | 983040 | 0.00 % | 0.00 % | 0.000e+00 | 0.000e+00 |

Extrapolated share by days to expiry (sticky moneyness):

| days | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 14 | 15 | 16 | 17 | 18 | 19 | 20 | 21 | 22 | 23 | 24 | 25 | 26 | 27 | 28 | 29 | 30 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| % | 43.94 | 43.17 | 25.40 | 15.49 | 9.55 | 6.13 | 4.13 | 2.76 | 1.90 | 1.22 | 0.80 | 0.46 | 0.27 | 0.16 | 0.10 | 0.07 | 0.03 | 0.02 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 |

Market-maker-rs against the reference on surface B, sticky-moneyness (violations must be 0):

| panel | output | rows | violations | max abs | max ratio | at (days, path) |
|---|---|---|---|---|---|---|
| all | price | 983040 | 0 | 3.819e-14 | 3.819e-07 | 6, 12052 |
| all | delta | 983040 | 0 | 5.218e-15 | 5.218e-07 | 1, 21114 |
| all | gamma | 983040 | 0 | 2.776e-15 | 5.251e-08 | 1, 29466 |
| all | vega | 983040 | 0 | 2.398e-14 | 2.398e-07 | 16, 23611 |
| all | theta | 983040 | 0 | 5.826e-13 | 4.609e-08 | 1, 26245 |
| in_range | price | 932055 | 0 | 3.819e-14 | 3.819e-07 | 6, 12052 |
| in_range | delta | 932055 | 0 | 5.218e-15 | 5.218e-07 | 1, 21114 |
| in_range | gamma | 932055 | 0 | 2.776e-15 | 5.251e-08 | 1, 29466 |
| in_range | vega | 932055 | 0 | 2.398e-14 | 2.398e-07 | 16, 23611 |
| in_range | theta | 932055 | 0 | 5.826e-13 | 4.609e-08 | 1, 26245 |
| extrapolated | price | 50985 | 0 | 3.553e-14 | 3.553e-07 | 1, 21122 |
| extrapolated | delta | 50985 | 0 | 6.661e-16 | 6.661e-08 | 1, 678 |
| extrapolated | gamma | 50985 | 0 | 1.362e-16 | 4.513e-08 | 1, 28023 |
| extrapolated | vega | 50985 | 0 | 1.971e-15 | 1.971e-08 | 2, 1849 |
| extrapolated | theta | 50985 | 0 | 3.975e-14 | 3.222e-08 | 2, 2104 |

Market-maker-rs against the reference on surface B, sticky-strike (violations must be 0):

| panel | output | rows | violations | max abs | max ratio | at (days, path) |
|---|---|---|---|---|---|---|
| all | price | 983040 | 0 | 3.642e-14 | 3.642e-07 | 13, 28253 |
| all | delta | 983040 | 0 | 4.885e-15 | 4.885e-07 | 1, 29357 |
| all | gamma | 983040 | 0 | 2.942e-15 | 4.920e-08 | 1, 3697 |
| all | vega | 983040 | 0 | 2.220e-14 | 2.220e-07 | 16, 5515 |
| all | theta | 983040 | 0 | 5.969e-13 | 4.369e-08 | 1, 23200 |
| in_range | price | 983040 | 0 | 3.642e-14 | 3.642e-07 | 13, 28253 |
| in_range | delta | 983040 | 0 | 4.885e-15 | 4.885e-07 | 1, 29357 |
| in_range | gamma | 983040 | 0 | 2.942e-15 | 4.920e-08 | 1, 3697 |
| in_range | vega | 983040 | 0 | 2.220e-14 | 2.220e-07 | 16, 5515 |
| in_range | theta | 983040 | 0 | 5.969e-13 | 4.369e-08 | 1, 23200 |
| extrapolated | price | 0 | 0 | 0.000e+00 | 0.000e+00 |  |
| extrapolated | delta | 0 | 0 | 0.000e+00 | 0.000e+00 |  |
| extrapolated | gamma | 0 | 0 | 0.000e+00 | 0.000e+00 |  |
| extrapolated | vega | 0 | 0 | 0.000e+00 | 0.000e+00 |  |
| extrapolated | theta | 0 | 0 | 0.000e+00 | 0.000e+00 |  |

## Hedging (primary half spread)

| market | seed | CVaR95(learned) - CVaR95(band), 95 % CI |
|---|---|---|
| in_model | 11 | -0.0212 [-0.0365, -0.0057] |
| in_model | 12 | -0.0293 [-0.0440, -0.0138] |
| in_model | 13 | -0.0391 [-0.0541, -0.0246] |
| in_model | 14 | -0.0302 [-0.0449, -0.0164] |
| in_model | 15 | -0.0288 [-0.0445, -0.0139] |
| heston_sticky-moneyness | 11 | -0.1897 [-0.2083, -0.1705] |
| heston_sticky-moneyness | 12 | -0.1816 [-0.2027, -0.1620] |
| heston_sticky-moneyness | 13 | -0.2066 [-0.2268, -0.1867] |
| heston_sticky-moneyness | 14 | -0.1722 [-0.1922, -0.1524] |
| heston_sticky-moneyness | 15 | -0.1733 [-0.1953, -0.1516] |
| heston_sticky-strike | 11 | -0.1031 [-0.1212, -0.0838] |
| heston_sticky-strike | 12 | -0.0950 [-0.1151, -0.0766] |
| heston_sticky-strike | 13 | -0.1200 [-0.1387, -0.1014] |
| heston_sticky-strike | 14 | -0.0856 [-0.1043, -0.0661] |
| heston_sticky-strike | 15 | -0.0867 [-0.1082, -0.0657] |

On the paths that stay in range every day (gate 7):

| dynamics | paths | seed | CI | veto |
|---|---|---|---|---|
| sticky-moneyness | 17056 of 32768 | 11 | -0.4260 [-0.4524, -0.4014] | False |
| sticky-moneyness | 17056 of 32768 | 12 | -0.4395 [-0.4665, -0.4128] | False |
| sticky-moneyness | 17056 of 32768 | 13 | -0.4331 [-0.4599, -0.4055] | False |
| sticky-moneyness | 17056 of 32768 | 14 | -0.4020 [-0.4276, -0.3762] | False |
| sticky-moneyness | 17056 of 32768 | 15 | -0.4469 [-0.4770, -0.4193] | False |
| sticky-strike | 32768 of 32768 | 11 | -0.1031 [-0.1211, -0.0840] | False |
| sticky-strike | 32768 of 32768 | 12 | -0.0950 [-0.1137, -0.0755] | False |
| sticky-strike | 32768 of 32768 | 13 | -0.1200 [-0.1372, -0.1019] | False |
| sticky-strike | 32768 of 32768 | 14 | -0.0856 [-0.1063, -0.0671] | False |
| sticky-strike | 32768 of 32768 | 15 | -0.0867 [-0.1058, -0.0665] | False |

Premium at t = 0 (sticky moneyness): market-maker-rs on B 2.2378132055, reference on B 2.2378132055, reference surface of M5-06b 2.2393929359.

In-model rows equal M5-06b: False. Run: 175.7 s, python 3.12.16, numpy 2.5.4, torch 2.15.0, arm64, commit `079f698687487ee7d91b8b82232bee493d955cd1`.

## After the run: the cause of gate 6

This section was written after the run. It changes neither gate 6 nor the outcome, which stays **REVISE** as preregistered.

Gate 6 compares this run's in-model rows with M5-06b's as whole records. Compared key by key and value by value, the 15 in-model learned rows (3 half spreads × 5 seeds) give the following.

All 255 shared values are equal and no key is missing. The band width, delta and band rows of every half spread are equal. The only difference is the 15 extra keys `weights_sha256`, which this run adds to record the weights it loaded. Their values are exactly the set of 15 weight sha256 recorded in M5-06b's `weights`.

Gate 6 therefore fails on a field the harness added, not on a value. That the criterion gives adopt with every other gate holding is **not** a preregistered result. "ADOPT except for gate 6" is a post-run interpretation, not the preregistered outcome.

Any ADOPT claim requires a new preregistered experiment, authorized separately and declared before it runs, with gate 6 comparing values and leaving metadata keys out. A correction of the gate 6 check belongs in a separate change, and it does not regenerate or reclassify run 2.
