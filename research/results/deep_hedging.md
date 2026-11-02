# M5-06: deep hedging against delta and no-trade-band controls

**Decision: adopt for further validation, synthetic setting only.** The
predeclared criterion is met. In model, at a 5 bp half spread, the learned
hedge has a lower CVaR95 than the best band tested (the crate's
constant-width band that re-hedges to its centre, at the best width of a
fixed grid), and the upper bound of the 95 %
paired bootstrap interval is below zero for each of the five training seeds.
Out of model (Heston) no seed is significantly worse; every interval is
below zero there too. This is evidence about one synthetic setting. It is
not a shipped capability, and it says nothing yet about real markets
(M5-06c, which waits on market-maker-rs#77) or against the library's own
pricing and volatility surface (M5-06b, which waits on market-maker-rs#95
and #96). Before any adoption beyond research, the learned hedge must
also be compared with a state-dependent band that re-hedges to its nearest
edge (see Limitations).

## Hypothesis

With proportional costs and limited execution (lots, quoted size,
rejections), a hedge trained to minimise CVaR95 of the hedging loss has a
lower CVaR95 than the Black-Scholes delta and than the best no-trade band
around it, evaluated with the same executable costs on the same paths.

## Setting

This is an adaptation inspired by Buehler et al. (2018), "Deep Hedging",
section 5 ([arXiv:1802.03042](https://arxiv.org/abs/1802.03042)), not a
reproduction of it: the paper hedges under Heston with the underlying and a
variance swap; here there is one hedge instrument, training is under
geometric Brownian motion, and Heston is only the out-of-model test.

- A short at-the-money European call, S0 = K = 100, 30 days, daily
  rebalancing (30 decisions), r = 0. The Black-Scholes premium at
  σ = 0.2 is credited to every control alike; it is a comparison convention.
- In model: GBM with σ = 0.2. Out of model: Heston with v0 = θ = 0.04,
  κ = 1, ξ = 0.5, ρ = −0.7, full truncation Euler with 10 substeps a day
  (v⁺ in the drift and the diffusion), no retraining.
- Train, validation and test sets of 2^15, 2^13 and 2^15 independent paths;
  the Heston test set has 2^15 paths. Test is used once.

### Execution, identical for the three controls

Paths, quoted sizes and rejection draws are generated once per (path, step)
and shared. Each step, a control states a target holding (bounded to
[−2, 2]) and one function executes it:

- the order is capped by the quoted size (lot multiples of 0.3 × Exp(1),
  at least one lot), then floored to the 1e-4 lot; an order of zero lots is
  not sent;
- a sent order is rejected with probability 2 %;
- a buy fills at `S · (1 + ε)` and a sell at `S · (1 − ε)` (ε is the half
  spread: 0, 5 bp or 50 bp), with a 2 bp fee on the notional.

The account is self financing: cash moves by the signed fill times its price
plus the fee, and the inventory by the actual fill only. At expiry the
inventory is closed in full at the same prices and fee (unlimited
liquidity, no rejection), and the call pays off. PnL = premium + cash −
payoff; the loss is −PnL.

### Controls

- **delta**: the Black-Scholes delta at σ = 0.2, every step.
- **band**: `hedge::plan_band_hedge` semantics: exposure = holding − delta,
  band [−w, w] edges included, target 0 (the delta), capped by the quoted
  size and floored to the lot. w is chosen on validation from
  {0.005, 0.01, 0.02, 0.05, 0.1, 0.15, 0.2, 0.3} with the same execution.
- **learned**: a network (3 inputs: log(S/K), time to expiry, current
  holding; two hidden layers of 32; output 2 · tanh) giving the target
  holding. It is trained, for each ε, to minimise CVaR95 of the loss by
  Rockafellar-Uryasev (a learnt threshold), with Adam (learning rate 1e-3,
  batch 4096, 1500 iterations), on a **differentiable proxy**: proportional
  cost (ε + fee) · S · |Δholding|, with no lot, size cap or rejection. It is
  evaluated with the real execution above. Five fixed training seeds; all
  five are reported.

### Tail convention and criterion (declared before the run)

CVaR_α is the mean of the ⌈(1 − α) N⌉ largest losses, the count computed
in exact fractions (an empirical approximation, not exact fractional
weighting): CVaR95 averages the worst
5 %, CVaR50 the worst 50 %. Intervals are 95 % percentile intervals of a
paired bootstrap over whole paths (1000 resamples, seeded) of
ΔCVaR95 = CVaR95(learned) − CVaR95(band).

Adopt if and only if, at ε = 5 bp in model, the upper bound of the interval
is below zero **for every seed**, and no seed is significantly worse out of
model (lower bound above zero). The best seed is never chosen.

## Results

Learned rows give the range over the five seeds. "Rebalances" counts the
filled orders before expiry; the terminal close is not counted there but its
spread and fee are in "spread + fee". "CVaR95 − ideal execution"
is the same control on the same paths with no rejection and unlimited size,
a counterfactual reported separately, not added to the costs.

### In model (GBM, σ = 0.2)

| half spread | control | mean PnL | std | CVaR95 | CVaR50 | spread + fee | rebalances | rejected | CVaR95 − ideal execution |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | delta | -0.048 | 0.609 | 1.577 | 0.501 | 0.049 | 28.6 | 0.58 | 0.676 |
| 0 | band (w = 0.005) | -0.048 | 0.609 | 1.577 | 0.501 | 0.049 | 25.8 | 0.52 | 0.676 |
| 0 | learned (5 seeds) | -0.046 to -0.045 | 0.656 to 0.674 | 1.545 to 1.571 | 0.545 to 0.562 | 0.045 to 0.046 | 29.4 to 29.4 | 0.60 to 0.60 | 0.672 to 0.700 |
| 5 bp | delta | -0.170 | 0.618 | 1.718 | 0.631 | 0.170 | 28.6 | 0.58 | 0.639 |
| 5 bp | band (w = 0.01) | -0.169 | 0.618 | 1.717 | 0.630 | 0.169 | 24.1 | 0.49 | 0.639 |
| 5 bp | learned (5 seeds) | -0.157 to -0.153 | 0.671 to 0.682 | 1.678 to 1.696 | 0.667 to 0.675 | 0.153 to 0.156 | 29.3 to 29.4 | 0.60 to 0.60 | 0.669 to 0.688 |
| 50 bp | delta | -1.265 | 0.769 | 3.070 | 1.861 | 1.266 | 28.6 | 0.58 | 0.263 |
| 50 bp | band (w = 0.15) | -0.904 | 0.878 | 2.911 | 1.592 | 0.900 | 7.1 | 0.15 | 0.333 |
| 50 bp | learned (5 seeds) | -0.921 to -0.898 | 0.788 to 0.812 | 2.736 to 2.758 | 1.515 to 1.525 | 0.896 to 0.919 | 28.6 to 29.0 | 0.58 to 0.59 | 0.536 to 0.576 |

### Out of model (Heston, no retraining)

| half spread | control | mean PnL | std | CVaR95 | CVaR50 | spread + fee | rebalances | rejected | CVaR95 − ideal execution |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | delta | 0.004 | 0.686 | 1.710 | 0.510 | 0.049 | 28.6 | 0.59 | 0.387 |
| 0 | band (w = 0.005) | 0.004 | 0.686 | 1.710 | 0.510 | 0.049 | 25.8 | 0.53 | 0.387 |
| 0 | learned (5 seeds) | 0.008 to 0.009 | 0.726 to 0.747 | 1.619 to 1.665 | 0.552 to 0.571 | 0.045 to 0.046 | 29.3 to 29.4 | 0.60 to 0.60 | 0.345 to 0.381 |
| 5 bp | delta | -0.118 | 0.692 | 1.856 | 0.636 | 0.170 | 28.6 | 0.59 | 0.349 |
| 5 bp | band (w = 0.01) | -0.117 | 0.692 | 1.855 | 0.635 | 0.169 | 24.1 | 0.49 | 0.349 |
| 5 bp | learned (5 seeds) | -0.102 to -0.099 | 0.738 to 0.755 | 1.744 to 1.778 | 0.671 to 0.686 | 0.153 to 0.156 | 29.3 to 29.4 | 0.60 to 0.60 | 0.333 to 0.366 |
| 50 bp | delta | -1.214 | 0.812 | 3.252 | 1.826 | 1.266 | 28.6 | 0.59 | -0.010 |
| 50 bp | band (w = 0.15) | -0.852 | 0.904 | 2.988 | 1.549 | 0.906 | 7.0 | 0.14 | 0.092 |
| 50 bp | learned (5 seeds) | -0.869 to -0.847 | 0.843 to 0.863 | 2.780 to 2.823 | 1.507 to 1.525 | 0.902 to 0.924 | 28.6 to 29.1 | 0.59 to 0.60 | 0.256 to 0.275 |

### Paired ΔCVaR95 = learned − band, 95 % bootstrap CI

| half spread | market | seed | executable | ideal execution (secondary) |
|---|---|---:|---|---|
| 0 | GBM | 11 | -0.032 [-0.045, -0.019] | -0.056 [-0.069, -0.041] |
| 0 | GBM | 12 | -0.006 [-0.021, 0.010] | -0.002 [-0.018, 0.012] |
| 0 | GBM | 13 | -0.027 [-0.041, -0.013] | -0.047 [-0.061, -0.034] |
| 0 | GBM | 14 | -0.024 [-0.038, -0.011] | -0.041 [-0.055, -0.028] |
| 0 | GBM | 15 | -0.017 [-0.032, -0.003] | -0.030 [-0.043, -0.015] |
| 0 | Heston | 11 | -0.089 [-0.106, -0.072] | -0.077 [-0.096, -0.059] |
| 0 | Heston | 12 | -0.046 [-0.065, -0.026] | -0.004 [-0.027, 0.018] |
| 0 | Heston | 13 | -0.091 [-0.107, -0.074] | -0.085 [-0.105, -0.067] |
| 0 | Heston | 14 | -0.060 [-0.077, -0.042] | -0.029 [-0.051, -0.008] |
| 0 | Heston | 15 | -0.068 [-0.087, -0.048] | -0.034 [-0.053, -0.013] |
| 5 bp | GBM | 11 | -0.021 [-0.036, -0.006] | -0.070 [-0.085, -0.054] |
| 5 bp | GBM | 12 | -0.029 [-0.044, -0.014] | -0.071 [-0.087, -0.056] |
| 5 bp | GBM | 13 | -0.039 [-0.054, -0.025] | -0.088 [-0.104, -0.074] |
| 5 bp | GBM | 14 | -0.030 [-0.045, -0.016] | -0.078 [-0.093, -0.064] |
| 5 bp | GBM | 15 | -0.029 [-0.045, -0.014] | -0.059 [-0.073, -0.043] |
| 5 bp | Heston | 11 | -0.095 [-0.112, -0.075] | -0.108 [-0.130, -0.089] |
| 5 bp | Heston | 12 | -0.087 [-0.107, -0.069] | -0.087 [-0.110, -0.066] |
| 5 bp | Heston | 13 | -0.112 [-0.130, -0.093] | -0.129 [-0.150, -0.109] |
| 5 bp | Heston | 14 | -0.077 [-0.096, -0.058] | -0.067 [-0.090, -0.046] |
| 5 bp | Heston | 15 | -0.078 [-0.100, -0.057] | -0.062 [-0.084, -0.040] |
| 50 bp | GBM | 11 | -0.166 [-0.192, -0.138] | -0.379 [-0.408, -0.347] |
| 50 bp | GBM | 12 | -0.168 [-0.195, -0.140] | -0.393 [-0.421, -0.363] |
| 50 bp | GBM | 13 | -0.175 [-0.204, -0.149] | -0.418 [-0.448, -0.391] |
| 50 bp | GBM | 14 | -0.153 [-0.180, -0.126] | -0.356 [-0.388, -0.325] |
| 50 bp | GBM | 15 | -0.156 [-0.183, -0.130] | -0.369 [-0.401, -0.337] |
| 50 bp | Heston | 11 | -0.166 [-0.199, -0.133] | -0.330 [-0.369, -0.292] |
| 50 bp | Heston | 12 | -0.169 [-0.201, -0.136] | -0.349 [-0.389, -0.312] |
| 50 bp | Heston | 13 | -0.208 [-0.241, -0.174] | -0.391 [-0.431, -0.352] |
| 50 bp | Heston | 14 | -0.175 [-0.209, -0.143] | -0.346 [-0.386, -0.303] |
| 50 bp | Heston | 15 | -0.180 [-0.213, -0.144] | -0.347 [-0.385, -0.306] |

The interval under ideal execution is a secondary analysis added after the
first run, before anything was changed, and does not enter the decision.

## Reading

- **The criterion is met at 5 bp.** ΔCVaR95 is between −0.039 and −0.021
  in model, and the upper bounds are between −0.025 and −0.006: all below
  zero. Out of model it is between −0.112 and −0.077, all intervals below
  zero, so there is no veto.
- **The gain grows with costs.** At 50 bp the learned hedge cuts CVaR95 by
  0.15 to 0.18 against the best band (w = 0.15), trading at every step with
  small adjustments where the band trades about 7 times.
- **Without spread the learned hedge is close to delta.** ε = 0 removes the
  spread but keeps the 2 bp fee and every execution limit, so it is not a
  frictionless case. There the band collapses to the smallest grid width
  and matches delta; the learned hedge is 0.006 to 0.032 lower in CVaR95,
  and for one seed the interval crosses zero, which is inconclusive. This
  does not test the paper's frictionless benchmark.
- **The gain is not only adaptation to execution limits.** Under ideal
  execution the difference at 5 bp and 50 bp is larger, not smaller.
- **The learned hedge pays for its tail with the body.** Its CVaR50 is
  higher than the band's at 0 and 5 bp (0.67 against 0.63 at 5 bp, in
  model), as expected from a CVaR95 objective.
- **Execution limits dominate the tail here.** With quoted sizes of mean
  0.3 and an initial delta of about 0.5, the first hedge is usually partial,
  and "CVaR95 − ideal execution" is about 0.6 to 0.7 at low costs. This is
  a property of the chosen book, the same for every control.
- **Capital.** The mean of the maximum absolute holding per path is 0.73
  to 0.79 for every control (`max_holding_mean` and `max_holding_p99` in
  `deep_hedging.json`); CVaR95 itself is the risk capital measure used
  above.

## Limitations

- Synthetic only: one underlying, one option, one hedge instrument, a flat
  volatility, a simple book. No captured data; M5-06c covers that once
  market-maker-rs#77 provides queue evidence.
- Black-Scholes pricing and delta, not the library counterpart; M5-06b
  validates against market-maker-rs#95 and #96.
- The learned hedge is trained on a proxy without lots, size caps or
  rejections, while the band's width is tuned with the real execution: an
  asymmetry in the band's favour that the learned hedge still overcomes.
- The band control is the crate's `plan_band_hedge`: a constant width w,
  re-hedging to the centre (the delta). It is not the state-dependent band
  of the transaction-cost literature (Whalley and Wilmott; Davis, Panas and
  Zariphopoulou), whose width depends on gamma and time and which re-hedges
  to the nearest edge; that band was not tested, and nothing here shows the
  learned hedge beats it, nor that it would be optimal in this simulator
  (lots, rejections, a CVaR objective). A comparison with an edge-hedging
  band of width w(Γ, τ) is a condition for any adoption beyond research.
- The band grid is coarse and its optimum at ε = 0 is the grid's lower end.
- The Heston test uses one parameter set. Out-of-model intervals below zero
  show no deterioration for this set only.
- Training and evaluation use float64; the band control's semantics are
  checked exactly against the crate (below), the float simulation of the
  same rule is not.

## Reproducibility

- Code: `experiments/deep_hedging.py`; how to run it:
  `experiments/README.md`. Python 3.12, numpy 2.5.4, torch 2.15.0 (CPU),
  pinned with hashes in `experiments/requirements.txt`.
- Seeds: data 20261102 (one `SeedSequence` spawned per set), training
  11 to 15, bootstrap 7. torch runs with one thread and deterministic
  algorithms; two runs on the same machine give identical output.
- Run time: about six minutes on one CPU thread (arm64).
- All numbers: `research/results/deep_hedging.json`, including the
  validation scores of every band width; its `run.git_commit` is the
  commit the run was made from (the experiment itself was not yet
  committed).
- Cross-check: the band control is also run in Python `decimal` on 20 test
  paths, on exported values rounded to 1e-6 (prices, deltas) and to the lot
  (holdings, sizes), plus hand-made cases at the band edges, one quantum
  outside on both sides, without liquidity, below the lot and capped by
  size. `tests/hedge.rs` (`deep_hedging_band`) recomputes all 608 rows with
  `plan_band_hedge`: decisions, sides, quantities, fees and spread costs
  match exactly.

Affiliation of the paper's authors is not evidence of production adoption,
and none is claimed.
