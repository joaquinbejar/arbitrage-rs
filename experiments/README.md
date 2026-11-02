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
