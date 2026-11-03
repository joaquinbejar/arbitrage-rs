# Experiments

Research code that is not part of the crate. Nothing here is a dependency
of `arbitrage-rs`, and none of it is packaged (`Cargo.toml` lists what is).

## Deep hedging against delta and band controls (M5-06)

`deep_hedging.py` compares a learned hedge with the Black-Scholes delta and
the no-trade band of `hedge::plan_band_hedge` on one synthetic setting. The
results and the decision are in
[`research/results/deep_hedging.md`](../research/results/deep_hedging.md).

Setup, with Python 3.12 (torch has no wheels for newer versions yet) and
[uv](https://docs.astral.sh/uv/):

```sh
uv venv --python 3.12 experiments/.venv
VIRTUAL_ENV=experiments/.venv uv pip install --require-hashes -r experiments/requirements.txt
experiments/.venv/bin/python experiments/deep_hedging.py
```

`requirements.in` pins the two direct dependencies (numpy, torch, CPU
builds); `requirements.txt` is the full lock with hashes, generated with
`uv pip compile experiments/requirements.in --python-version 3.12
--generate-hashes -o experiments/requirements.txt`.

The run takes about six minutes on one CPU thread and writes:

- `research/results/deep_hedging.json`: the configuration, every metric
  and the decision;
- `tests/fixtures/deep_hedging_band.csv`: the band control's decisions,
  recomputed exactly by `tests/hedge.rs` (`deep_hedging_band`).

Every random draw is seeded (`CONFIG["data_seed"]`, `train_seeds`,
`bootstrap_seed`); torch runs with one thread and deterministic algorithms,
so a rerun on the same platform gives the same output.

## Against a pricing counterpart (M5-06b)

`deep_hedging.py --counterpart reference` reruns the comparison with the
delta, the band and the premium taken from a pricing counterpart
(`counterpart.py`) instead of Black-Scholes at a flat volatility. Without
the flag the script is the M5-06 run, unchanged. The results are in
[`research/results/deep_hedging_counterpart.md`](../research/results/deep_hedging_counterpart.md);
they stay **pending counterpart validation** until market-maker-rs
replaces the reference.

```sh
experiments/.venv/bin/python experiments/deep_hedging.py --counterpart reference
```

It writes `research/results/deep_hedging_counterpart.json` and the frozen
weights of the learned controls, `research/models/*.npz` (float64, loaded
with `allow_pickle=False`; their sha256 is in the json).

- `counterpart.py`: the interface (`Contract`, `ReferenceCounterpart`,
  `FlatSurface`, `SviSurface`), its conventions (units, time, dynamics,
  Greeks, expiry) in the module docstring, and the empty
  `MarketMakerRsCounterpart`.
- `calibration.py`, `svi.py`, `heston.py`: the reference surface, raw SVI
  slices fitted to semi-analytic Heston prices, and its static
  diagnostics.

### Conformance suite

Every counterpart runs the same checks (payoff at expiry, put-call parity,
Greeks against finite differences of the same price function, explicit
errors), plus the SVI and Heston checks. It is not part of CI or
`make pre-push`; run it with:

```sh
experiments/.venv/bin/python -m unittest discover -s experiments -p 'test_*.py' -v
```

The market-maker-rs checks are skipped until that counterpart exists.

### Proposed market-maker-rs adapter contract

A proposal only: market-maker-rs fixes it when it plans its pricing and
surface work (market-maker-rs#95 and #96), and the exact columns are set
there.

- A market-maker-rs binary at a pinned version, run as a batch subprocess:
  `<binary> --requests requests.csv --responses responses.csv`.
- Request rows: `id, strike, expiry_days, kind, style, spot, as_of_days,
  rate, carry`.
- Response rows, one per request: `id, status, price, delta, gamma, vega,
  theta, total_variance, dynamics, error`, with `status` `ok` or `error`.
  An unsupported style or an invalid input is an `error` row with a
  reason, never a number.
- The binary's version and the configuration it ran with are reported, so
  the run can pin them.
