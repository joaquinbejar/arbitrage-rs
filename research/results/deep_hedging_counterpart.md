# M5-06b: deep hedging against a pricing counterpart

> **Pending counterpart validation.** The counterpart here is the
> reference implementation in `experiments/counterpart.py`, not
> market-maker-rs: its pricing and volatility surface (market-maker-rs#95
> and #96) do not exist yet. The json records the same status
> (`counterpart_validation: "pending"`). This issue stays open until the
> run is repeated against that counterpart.

**Outcome against the reference: ADOPT for further validation, synthetic
setting only.** The pre-registered criterion holds. In model, the result
is the M5-06 result (same surface, same functions, same rows). Out of
model (Heston), with delta and band taken from the reference counterpart
on a calibrated surface, no seed is significantly worse under either
dynamics; every interval is below zero under both. Relative to
[`deep_hedging.md`](deep_hedging.md) the conclusion does not change; the
learned hedge's advantage over the band out of model is larger with the
sticky-moneyness delta and about the same with the sticky-strike delta.
Everything in the M5-06 limitations still applies, including the open
comparison with a state-dependent band.

## What changes against M5-06

This is a different experiment from M5-06: the out-of-model baselines use
a Heston-informed reference surface. Only the following changed (declared
before the run):

- **In model (GBM):** the surface is `FlatSurface(0.2)`. The counterpart's
  delta is the M5-06 `bs_delta` bit for bit (checked on every test path and
  step), and the in-model rows equal `deep_hedging.json`
  (`in_model_matches_m5_06: true`).
- **Out of model (Heston):** delta and band use the counterpart's delta on
  the reference surface, under each of two dynamics, and the premium
  credited to every control is the counterpart's price at t = 0 (2.2394
  against 2.2873 at a flat 0.2). The band width is the one chosen on the
  GBM validation set, as in M5-06.
- **Learned controls:** trained exactly as in M5-06 (same data, seeds and
  proxy) and now frozen in `research/models/` with their sha256 in the
  json. Their inputs (log(S/K), time to expiry, holding) do not see the
  counterpart, and the premium is common to every control, so any change
  in the conclusion comes only from the delta and band baselines.

Costs, execution, paths, seeds, the tail convention (CVaR_α is the mean of
the ⌈(1 − α) N⌉ largest losses) and the paired bootstrap are those of
M5-06.

### The reference counterpart

- Black-Scholes-Merton (r = carry = 0 here) on a surface of total implied
  variance w(k, τ), k = ln(K / F). Price, delta, gamma, vega and theta all
  come from one price function; delta and gamma are total derivatives
  under the declared dynamics.
- **Dynamics:**
  - sticky-strike: the volatility of a strike does not move with the spot,
    so the delta is the Black-Scholes delta at the smile volatility (here
    the ATM volatility of each slice);
  - sticky-moneyness: the smile moves with the forward,
    delta = BS delta − vega · σ'(k) / S.
  With the negative skew of ρ = −0.7 (σ'(k) < 0), sticky-moneyness raises
  the delta of this call above the Black-Scholes delta. Neither is
  Heston's minimum-variance delta, which for ρ < 0 lies below the
  Black-Scholes delta.
- **Surface:** one raw SVI slice per day from 1 to 30 days (every decision
  is on a node), linear in w between nodes, no extrapolation in τ. It is
  fitted once at t = 0 with v0 to semi-analytic Heston prices with the
  out-of-model parameters (v0 = θ = 0.04, κ = 1, ξ = 0.5, ρ = −0.7), never
  recalibrated along a path.
  - Heston prices: Lewis's single integral with the characteristic function
    in the "little trap" form, composite Gauss-Legendre; doubling the nodes
    and the cutoff moves no price by more than 5.7e-14 (tolerance 1e-10).
  - Quotes: 33 strikes per slice at k from −4 to 4 ATM standard deviations,
    out-of-the-money prices inverted by bisection; no quote was dropped.
  - Fit: the quasi-explicit method (grid over (m, σ), exact constrained
    least squares for the linear parameters), weights 1 / σ_iv², to first
    order uniform in implied volatility.
- **Static diagnostics,** on a grid covering the calibration range and the
  realised k of every Heston test path at every step: w > 0 (minimum
  7.8e-5), Lee's wing bound b(1 + |ρ|) ≤ 2 (maximum 0.014), Gatheral and
  Jacquier's g(k) ≥ 0 (minimum 0.549) and calendar monotonicity (minimum
  gap 6.1e-5). All hold. A failure would have stopped the run. These
  describe that grid only; they are not a proof of absence of static
  arbitrage.

Implied volatility residuals of the fit, SVI minus Heston, in basis points
of volatility, by expiry (days) and strike (ATM standard deviations):

| day | −4 | −3 | −2 | −1 | 0 | 1 | 2 | 3 | 4 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 0.2 | −0.0 | −0.1 | −0.1 | 0.0 | 0.1 | 0.0 | −0.1 | 0.1 |
| 2 | 0.6 | −0.1 | −0.3 | −0.3 | 0.0 | 0.3 | 0.2 | −0.3 | 0.4 |
| 5 | 2.5 | 0.0 | −1.3 | −1.3 | −0.2 | 1.2 | 1.0 | −1.2 | 1.7 |
| 10 | 5.1 | 0.2 | −2.6 | −2.8 | −0.3 | 2.9 | 0.7 | −2.1 | 1.9 |
| 15 | 6.6 | 0.2 | −3.5 | −3.6 | 0.4 | 4.4 | −2.5 | 1.1 | −1.9 |
| 20 | 8.2 | 0.3 | −4.4 | −4.5 | 0.9 | 5.7 | −5.0 | 3.7 | −5.0 |
| 25 | 9.8 | 0.4 | −5.4 | −5.6 | 1.3 | 6.9 | −6.1 | 4.7 | −5.9 |
| 30 | 11.3 | 0.6 | −6.2 | −6.7 | 1.8 | 7.7 | −6.2 | 5.4 | −6.7 |

The ATM volatility falls from 0.1999 (1 day) to 0.1958 (30 days). The fit
sits on or near the SVI boundary ρ = −1 at short expiries (exactly −1 up to
16 days, −0.999 to −0.901 from 17 to 22 days): raw SVI cannot follow the
short-dated Heston skew exactly, and the residual grows with the expiry and
in the left wing, at most 11 bp of volatility. Every slice is in the json
with its parameters, residuals and active constraints.

## Decision rule (pre-registered)

At the primary half spread (5 bp):

- **in_model:** the upper bound of the paired 95 % interval of
  ΔCVaR95 = CVaR95(learned) − CVaR95(band) is below 0 for every seed, on
  GBM with the flat surface (the M5-06 rule);
- **veto(D):** under Heston with the reference surface and dynamics D,
  some seed has its lower bound above 0;
- **ADOPT** if in_model holds and there is no veto under either dynamics;
  **REJECT** if in_model fails or there is a veto under both; **REVISE**
  if in_model holds and there is a veto under exactly one.

Result: in_model holds (the M5-06 rows), veto(sticky-strike) = false,
veto(sticky-moneyness) = false: **ADOPT**, pending counterpart
validation.

## Results out of model (Heston, no retraining)

The learned rows are the same under both dynamics (their inputs and
premium do not depend on it). "Rebalances" counts the filled orders before
expiry; "CVaR95 − ideal execution" is the same control with no rejection
and unlimited size, a counterfactual reported separately.

### Sticky-strike delta

| half spread | control | mean PnL | std | CVaR95 | CVaR50 | spread + fee | rebalances | rejected | CVaR95 − ideal execution |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | delta | −0.044 | 0.686 | 1.765 | 0.558 | 0.049 | 28.6 | 0.59 | 0.384 |
| 0 | band (w = 0.005) | −0.044 | 0.686 | 1.765 | 0.558 | 0.049 | 25.7 | 0.53 | 0.384 |
| 0 | learned (5 seeds) | −0.040 to −0.039 | 0.726 to 0.747 | 1.667 to 1.713 | 0.600 to 0.618 | 0.045 to 0.046 | 29.3 to 29.4 | 0.60 | 0.345 to 0.381 |
| 5 bp | delta | −0.167 | 0.693 | 1.912 | 0.684 | 0.171 | 28.6 | 0.59 | 0.345 |
| 5 bp | band (w = 0.01) | −0.165 | 0.693 | 1.911 | 0.683 | 0.170 | 24.0 | 0.49 | 0.346 |
| 5 bp | learned (5 seeds) | −0.150 to −0.147 | 0.738 to 0.755 | 1.791 to 1.826 | 0.719 to 0.734 | 0.153 to 0.156 | 29.3 to 29.4 | 0.60 | 0.333 to 0.366 |
| 50 bp | delta | −1.265 | 0.816 | 3.315 | 1.881 | 1.270 | 28.6 | 0.59 | −0.016 |
| 50 bp | band (w = 0.15) | −0.903 | 0.904 | 3.048 | 1.599 | 0.909 | 7.0 | 0.14 | 0.092 |
| 50 bp | learned (5 seeds) | −0.917 to −0.895 | 0.843 to 0.863 | 2.828 to 2.870 | 1.555 to 1.573 | 0.902 to 0.924 | 28.6 to 29.1 | 0.59 to 0.60 | 0.256 to 0.275 |

### Sticky-moneyness delta

| half spread | control | mean PnL | std | CVaR95 | CVaR50 | spread + fee | rebalances | rejected | CVaR95 − ideal execution |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | delta | −0.046 | 0.725 | 1.854 | 0.596 | 0.050 | 28.5 | 0.59 | 0.358 |
| 0 | band (w = 0.005) | −0.045 | 0.725 | 1.854 | 0.596 | 0.049 | 25.7 | 0.53 | 0.358 |
| 0 | learned (5 seeds) | −0.040 to −0.039 | 0.726 to 0.747 | 1.667 to 1.713 | 0.600 to 0.618 | 0.045 to 0.046 | 29.3 to 29.4 | 0.60 | 0.345 to 0.381 |
| 5 bp | delta | −0.170 | 0.730 | 1.999 | 0.721 | 0.173 | 28.5 | 0.59 | 0.317 |
| 5 bp | band (w = 0.01) | −0.168 | 0.730 | 1.998 | 0.720 | 0.172 | 23.9 | 0.49 | 0.317 |
| 5 bp | learned (5 seeds) | −0.150 to −0.147 | 0.738 to 0.755 | 1.791 to 1.826 | 0.719 to 0.734 | 0.153 to 0.156 | 29.3 to 29.4 | 0.60 | 0.333 to 0.366 |
| 50 bp | delta | −1.285 | 0.827 | 3.398 | 1.906 | 1.289 | 28.5 | 0.59 | −0.068 |
| 50 bp | band (w = 0.15) | −0.929 | 0.910 | 3.138 | 1.625 | 0.933 | 7.1 | 0.14 | 0.048 |
| 50 bp | learned (5 seeds) | −0.917 to −0.895 | 0.843 to 0.863 | 2.828 to 2.870 | 1.555 to 1.573 | 0.902 to 0.924 | 28.6 to 29.1 | 0.59 to 0.60 | 0.256 to 0.275 |

### Paired ΔCVaR95 = learned − band, 95 % bootstrap CI

The flat column is M5-06 (`deep_hedging.json`), for comparison.

| half spread | seed | flat (M5-06) | sticky-strike | sticky-moneyness |
|---|---:|---|---|---|
| 0 | 11 | −0.089 [−0.106, −0.072] | −0.096 [−0.113, −0.078] | −0.185 [−0.202, −0.167] |
| 0 | 12 | −0.046 [−0.065, −0.026] | −0.053 [−0.072, −0.033] | −0.141 [−0.162, −0.121] |
| 0 | 13 | −0.091 [−0.107, −0.074] | −0.098 [−0.115, −0.080] | −0.187 [−0.205, −0.168] |
| 0 | 14 | −0.060 [−0.077, −0.042] | −0.067 [−0.084, −0.049] | −0.155 [−0.173, −0.136] |
| 0 | 15 | −0.068 [−0.087, −0.048] | −0.075 [−0.094, −0.054] | −0.163 [−0.184, −0.143] |
| 5 bp | 11 | −0.095 [−0.112, −0.075] | −0.103 [−0.121, −0.083] | −0.190 [−0.209, −0.171] |
| 5 bp | 12 | −0.087 [−0.107, −0.069] | −0.094 [−0.115, −0.076] | −0.182 [−0.203, −0.162] |
| 5 bp | 13 | −0.112 [−0.130, −0.093] | −0.119 [−0.138, −0.101] | −0.207 [−0.227, −0.187] |
| 5 bp | 14 | −0.077 [−0.096, −0.058] | −0.085 [−0.104, −0.066] | −0.173 [−0.193, −0.153] |
| 5 bp | 15 | −0.078 [−0.100, −0.057] | −0.086 [−0.108, −0.065] | −0.174 [−0.196, −0.152] |
| 50 bp | 11 | −0.166 [−0.199, −0.133] | −0.178 [−0.211, −0.145] | −0.268 [−0.304, −0.233] |
| 50 bp | 12 | −0.169 [−0.201, −0.136] | −0.182 [−0.214, −0.149] | −0.271 [−0.307, −0.237] |
| 50 bp | 13 | −0.208 [−0.241, −0.174] | −0.220 [−0.253, −0.187] | −0.310 [−0.345, −0.275] |
| 50 bp | 14 | −0.175 [−0.209, −0.143] | −0.187 [−0.221, −0.155] | −0.277 [−0.313, −0.245] |
| 50 bp | 15 | −0.180 [−0.213, −0.144] | −0.192 [−0.226, −0.157] | −0.281 [−0.315, −0.245] |

Under ideal execution (secondary, not a decision input; in the json) every
interval is below zero except at a zero half spread for seed 12, where
both the flat (−0.004 [−0.027, 0.018]) and the sticky-strike interval
(−0.013 [−0.037, 0.009]) cross zero, as in M5-06.

## Reading

- **The conclusion does not change.** Both dynamics keep every
  out-of-model interval below zero at every half spread, so there is no
  veto, and the in-model part is M5-06 unchanged.
- **The baselines move, the learned hedge does not.** With the smile, the
  delta and band controls get worse out of model under both dynamics
  (CVaR95 at 5 bp: 1.856 flat, 1.912 sticky-strike, 1.999
  sticky-moneyness for delta); the learned rows change only by the common
  premium (2.2394 here against 2.2873), which shifts every control's PnL
  alike. The ΔCVaR95 against the band therefore widens slightly under
  sticky-strike and clearly under sticky-moneyness.
- **Sticky-moneyness moves away from the minimum-variance delta here.** It
  raises the delta of the call with the negative skew, while under Heston
  with ρ < 0 the minimum-variance delta is below the Black-Scholes delta. Its larger ΔCVaR95 says more about that
  baseline than about the learned hedge; sticky-strike, close to the flat
  ATM delta, is the more conservative comparison and still gives no veto.
- **The surface is a reference, not a market.** It is Heston-informed by
  construction (the same parameters as the out-of-model paths), static,
  and fitted with residuals of up to 11 bp of volatility in the left wing
  at 30 days.

## Limitations

- **Pending counterpart validation:** the counterpart is the reference
  implementation. The decision becomes final only when the run is
  repeated against market-maker-rs (#95 pricing and Greeks, #96 surface)
  with its version pinned; the adapter is in place
  (`MarketMakerRsCounterpart`) and empty.
- **The surface is synthetic and Heston-informed:** it shares the
  out-of-model parameters, is calibrated once at t = 0 and never
  recalibrated, and the two dynamics are modelling assumptions, not
  observed surface behaviour.
- The static diagnostics hold on a finite grid only.
- All M5-06 limitations remain: one synthetic setting, one option, one
  hedge instrument, a training proxy without execution limits, a band
  that re-hedges to its centre, one Heston parameter set, float64.

## Reproducibility

- Code: `experiments/deep_hedging.py --counterpart reference` (see
  `experiments/README.md`), with `counterpart.py`, `calibration.py`,
  `svi.py` and `heston.py`. Python 3.12, numpy 2.5.4, torch 2.15.0 (CPU).
- Identities in the json: the sha256 of the counterpart implementation,
  of its configuration, of the surface parameters, of the experiment and
  counterpart configurations, and of every frozen weight file in
  `research/models/`.
- Conformance suite: `experiments/.venv/bin/python -m unittest discover
  -s experiments -p 'test_*.py' -v`.
- Run time: about eight and a half minutes on one CPU thread (arm64),
  including the calibration (about 100 s).
- The run without `--counterpart` still reproduces `deep_hedging.json`
  (except `run.seconds` and `run.git_commit`) and
  `tests/fixtures/deep_hedging_band.csv` byte for byte.
