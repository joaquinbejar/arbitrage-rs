"""Write `experiments/datasets/synthetic-v1/`: a small SYNTHETIC dataset in
the format of `dataset.py`, to check the empirical pipeline end to end.

It is not captured data and supports no empirical claim. Everything is
drawn from one seed (recorded in the manifest):

- 64 paths, 31 daily book rows each (30 decisions and the expiry), a mid
  following geometric Brownian motion from 2500 at a volatility of 0.25, so
  the runner's rebasing to s0 = 100 is exercised;
- a half spread of 2 to 8 bp, drawn per row and rounded to the tick, and
  quoted sizes in lots of 0.3 x Exp(1) / lot (at least 1 lot);
- the capacity of the queue model per side, the quoted size times a uniform
  fraction in [0.5, 1], floored to whole lots; a rejection with
  probability 2 %.

Run `experiments/.venv/bin/python experiments/make_synthetic_dataset.py`
from the repository root; it overwrites the directory.
"""

from __future__ import annotations

import hashlib
import json
import math
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "experiments" / "datasets" / "synthetic-v1"

SPEC = {
    "seed": 20261103,
    "paths": 64,
    "steps": 30,
    "s0": 2500.0,
    "sigma": 0.25,
    "days_per_year": 365.0,
    "tick": 0.01,
    "lot": 1e-4,
    "half_spread_bp": [2.0, 8.0],
    "size_mean": 0.3,
    "capacity_fraction": [0.5, 1.0],
    "p_reject": 0.02,
    "start_timestamp": 1767225600,  # 2026-01-01T00:00:00Z, seconds
}


def main() -> None:
    sp = SPEC
    rng = np.random.default_rng(sp["seed"])
    n, steps = sp["paths"], sp["steps"]
    dt = 1.0 / sp["days_per_year"]
    z = rng.standard_normal((n, steps))
    log_s = np.concatenate(
        [np.zeros((n, 1)), np.cumsum(-0.5 * sp["sigma"] ** 2 * dt + sp["sigma"] * math.sqrt(dt) * z, axis=1)], axis=1
    )
    mid = sp["s0"] * np.exp(log_s)
    half = rng.uniform(*sp["half_spread_bp"], (n, steps + 1)) / 1e4 * mid
    tick = sp["tick"]
    bid = np.floor((mid - half) / tick) * tick
    ask = np.ceil((mid + half) / tick) * tick
    size = lambda: np.maximum(np.floor(sp["size_mean"] * rng.exponential(1.0, (n, steps + 1)) / sp["lot"]), 1)  # noqa: E731
    bid_size, ask_size = size().astype(np.int64), size().astype(np.int64)
    frac_b = rng.uniform(*sp["capacity_fraction"], (n, steps))
    frac_s = rng.uniform(*sp["capacity_fraction"], (n, steps))
    fill_buy = np.floor(ask_size[:, :steps] * frac_b).astype(np.int64)
    fill_sell = np.floor(bid_size[:, :steps] * frac_s).astype(np.int64)
    reject = (rng.random((n, steps)) < sp["p_reject"]).astype(np.int64)

    OUT.mkdir(parents=True, exist_ok=True)
    with (OUT / "paths.csv").open("w") as f:
        f.write("path_id,step,timestamp,bid,ask,bid_size,ask_size\n")
        for p in range(n):
            for s in range(steps + 1):
                ts = sp["start_timestamp"] + 86400 * s
                f.write(f"{p},{s},{ts},{bid[p, s]:.2f},{ask[p, s]:.2f},{bid_size[p, s]},{ask_size[p, s]}\n")
    with (OUT / "execution.csv").open("w") as f:
        f.write("path_id,step,fillable_buy,fillable_sell,reject\n")
        for p in range(n):
            for s in range(steps):
                f.write(f"{p},{s},{fill_buy[p, s]},{fill_sell[p, s]},{reject[p, s]}\n")
    files = {name: hashlib.sha256((OUT / name).read_bytes()).hexdigest() for name in ("paths.csv", "execution.csv")}
    manifest = {
        "name": "synthetic-v1",
        "version": "1",
        "source": "SYNTHETIC: experiments/make_synthetic_dataset.py, not captured data",
        "synthetic": True,
        "seed": sp["seed"],
        "files": files,
        "currency": "USD",
        "tick": sp["tick"],
        "lot": sp["lot"],
        "fee_bps": 2.0,
        "fee_asset": "quote",
        "rounding": "floor-lots",
        "contract": {"strike": "atm-at-step-0", "expiry_days": float(steps), "kind": "call", "style": "european"},
        "steps": steps,
        "step_days": 1.0,
        "timestamp_unit": "s",
        "queue_model": {
            "id": "synthetic-exogenous-capacity",
            "version": "1",
            "limits": [
                "capacity per side is exogenous: a fraction of the quoted size, independent of the order",
                "no queue position, latency or market impact is modelled",
                "rejections are independent draws, not venue behaviour",
            ],
        },
    }
    (OUT / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(f"wrote {OUT} ({n} paths, {steps} decisions)")


if __name__ == "__main__":
    main()
