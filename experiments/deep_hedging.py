"""M5-06: deep hedging against delta and no-trade-band controls (synthetic).

An adaptation inspired by Buehler et al. (2018), "Deep Hedging", section 5
(https://arxiv.org/abs/1802.03042): a short at-the-money European call on
one underlying, hedged daily with the underlying only. The paper hedges
under Heston with the underlying and a variance swap; this experiment uses
one hedge instrument, trains under geometric Brownian motion and tests both
in model (GBM) and out of model (Heston, no retraining).

Three controls are evaluated on the same paths, books and rejection draws,
with one execution function (`execute`):

- delta: the Black-Scholes delta, every step;
- band: the semantics of `arbitrage_rs::hedge::plan_band_hedge`: exposure
  `holding - delta`, band `[-w, w]` (edges included), target 0;
- learned: a small network that outputs the target holding, trained to
  minimise CVaR_95 of the loss (Rockafellar-Uryasev) on a differentiable
  proxy of the costs.

Run `experiments/.venv/bin/python experiments/deep_hedging.py` from the
repository root (see experiments/README.md). It writes
`research/results/deep_hedging.json` and the cross-check fixture
`tests/fixtures/deep_hedging_band.csv`. Everything is seeded; torch runs on
one CPU thread with deterministic algorithms.
"""

from __future__ import annotations

import json
import math
import platform
import subprocess
import sys
import time
from decimal import ROUND_FLOOR, ROUND_HALF_EVEN, Decimal, getcontext
from pathlib import Path

import numpy as np
import torch

ROOT = Path(__file__).resolve().parent.parent

CONFIG = {
    "s0": 100.0,
    "strike": 100.0,
    "sigma": 0.2,
    "steps": 30,  # daily rebalancing over 30 days
    "days_per_year": 365.0,
    # Half spread: bid = S * (1 - eps), ask = S * (1 + eps).
    "eps_list": [0.0, 0.0005, 0.005],
    "primary_eps": 0.0005,
    "fee_bps": 2.0,
    "lot": 1e-4,
    "p_reject": 0.02,
    # Quoted size per step: lot multiples of size_mean * Exp(1).
    "size_mean": 0.3,
    "holding_bound": 2.0,
    "n_train": 2**15,
    "n_val": 2**13,
    "n_test": 2**15,
    "alpha": 0.95,
    "band_grid": [0.005, 0.01, 0.02, 0.05, 0.1, 0.15, 0.2, 0.3],
    "train_seeds": [11, 12, 13, 14, 15],
    "data_seed": 20261102,
    "bootstrap": 1000,
    "bootstrap_seed": 7,
    "batch": 4096,
    "iterations": 1500,
    "learning_rate": 1e-3,
    "hidden": 32,
    "heston": {
        "v0": 0.04,
        "kappa": 1.0,
        "theta": 0.04,
        "xi": 0.5,
        "rho": -0.7,
        "substeps": 10,
    },
    "fixture_paths": 20,
}


# ---------------------------------------------------------------- pricing


def norm_cdf(x: np.ndarray) -> np.ndarray:
    return torch.special.ndtr(torch.from_numpy(np.asarray(x, dtype=np.float64))).numpy()


def bs_delta(s: np.ndarray, tau: float, cfg: dict) -> np.ndarray:
    """Black-Scholes call delta at r = 0; at tau = 0, the payoff's slope."""
    if tau <= 0.0:
        return (s > cfg["strike"]).astype(float)
    vol = cfg["sigma"] * math.sqrt(tau)
    d1 = (np.log(s / cfg["strike"]) + 0.5 * vol * vol) / vol
    return norm_cdf(d1)


def bs_call(s: float, tau: float, cfg: dict) -> float:
    vol = cfg["sigma"] * math.sqrt(tau)
    d1 = (math.log(s / cfg["strike"]) + 0.5 * vol * vol) / vol
    d2 = d1 - vol
    n = lambda x: 0.5 * (1.0 + math.erf(x / math.sqrt(2.0)))  # noqa: E731
    return s * n(d1) - cfg["strike"] * n(d2)


# ---------------------------------------------------------------- markets


def gbm_paths(n: int, rng: np.random.Generator, cfg: dict) -> np.ndarray:
    dt = 1.0 / cfg["days_per_year"]
    z = rng.standard_normal((n, cfg["steps"]))
    sig = cfg["sigma"]
    log_steps = -0.5 * sig * sig * dt + sig * math.sqrt(dt) * z
    log_s = np.concatenate([np.zeros((n, 1)), np.cumsum(log_steps, axis=1)], axis=1)
    return cfg["s0"] * np.exp(log_s)


def heston_paths(n: int, rng: np.random.Generator, cfg: dict) -> np.ndarray:
    """Full truncation Euler (v+ in drift and diffusion), r = 0."""
    h = cfg["heston"]
    m = h["substeps"]
    dt = 1.0 / cfg["days_per_year"] / m
    log_s = np.zeros(n)
    v = np.full(n, h["v0"])
    out = np.empty((n, cfg["steps"] + 1))
    out[:, 0] = cfg["s0"]
    for t in range(cfg["steps"]):
        for _ in range(m):
            z1 = rng.standard_normal(n)
            z2 = h["rho"] * z1 + math.sqrt(1.0 - h["rho"] ** 2) * rng.standard_normal(n)
            vp = np.maximum(v, 0.0)
            log_s += -0.5 * vp * dt + np.sqrt(vp * dt) * z1
            v = v + h["kappa"] * (h["theta"] - vp) * dt + h["xi"] * np.sqrt(vp * dt) * z2
        out[:, t + 1] = cfg["s0"] * np.exp(log_s)
    return out


def market(paths: np.ndarray, rng: np.random.Generator, cfg: dict) -> dict:
    """Quoted sizes (in lots, at least 1) and rejection draws per (path, t),
    drawn once and shared by every control."""
    n = paths.shape[0]
    steps = cfg["steps"]
    size = np.maximum(
        np.floor(cfg["size_mean"] * rng.exponential(1.0, (n, steps)) / cfg["lot"]), 1
    ).astype(np.int64)
    u_reject = rng.random((n, steps))
    return {"s": paths, "size_lots": size, "u_reject": u_reject}


# ---------------------------------------------------------------- execution


def execute(target, holding_lots, s, eps, size_lots, u_reject, cfg, p_reject):
    """One order towards `target` (in units), at the executable price.

    The size is capped by the quoted size, then floored to whole lots; an
    order of zero lots is not sent. A sent order is rejected when
    `u_reject < p_reject`. Returns the new holding in lots and the cash,
    spread and fee flows of the fill.
    """
    lot = cfg["lot"]
    order = target - holding_lots * lot
    # Lots of the order, capped by the quoted size, floored to whole lots.
    # The small tolerance absorbs float noise in `target - holding`.
    want = np.floor(np.abs(order) / lot + 1e-9).astype(np.int64)
    lots = np.minimum(want, size_lots)
    sent = lots > 0
    rejected = sent & (u_reject < p_reject)
    filled = np.where(rejected, 0, lots)
    sign = np.sign(order)
    q = filled * lot
    price = s * (1.0 + sign * eps)
    cash = -sign * q * price - np.abs(q * price) * cfg["fee_bps"] / 1e4
    spread = q * s * eps
    fee = np.abs(q * price) * cfg["fee_bps"] / 1e4
    new_holding = holding_lots + (sign * filled).astype(np.int64)
    return new_holding, cash, spread, fee, filled, sent, rejected


def simulate(policy, mkt, eps, cfg, p_reject=None, unlimited=False):
    """Self-financing account: cash -= signed fill * price + fee; the
    inventory moves by the actual fill only. At T the inventory is closed in
    full at the executable price with the fee (unlimited liquidity, no
    rejection) and the call payoff is paid. PnL = premium + cash - payoff."""
    p_reject = cfg["p_reject"] if p_reject is None else p_reject
    s = mkt["s"]
    n, steps = s.shape[0], cfg["steps"]
    year = cfg["days_per_year"]
    holding = np.zeros(n, dtype=np.int64)
    cash = np.zeros(n)
    stats = {k: np.zeros(n) for k in ("spread", "fee", "turnover", "trades", "rejected", "residual")}
    max_holding = np.zeros(n)
    for t in range(steps):
        tau = (steps - t) / year
        target = np.clip(policy(t, s[:, t], tau, holding * cfg["lot"]), -cfg["holding_bound"], cfg["holding_bound"])
        size = np.full(n, 10**12, dtype=np.int64) if unlimited else mkt["size_lots"][:, t]
        holding, c, sp, fe, filled, sent, rej = execute(
            target, holding, s[:, t], eps, size, mkt["u_reject"][:, t], cfg, p_reject
        )
        cash += c
        stats["spread"] += sp
        stats["fee"] += fe
        stats["turnover"] += filled * cfg["lot"]
        stats["trades"] += (filled > 0)
        stats["rejected"] += rej
        stats["residual"] += np.abs(target - holding * cfg["lot"])
        max_holding = np.maximum(max_holding, np.abs(holding * cfg["lot"]))
    # Terminal close, in full.
    s_t = s[:, steps]
    q = holding * cfg["lot"]
    sign = -np.sign(q)
    price = s_t * (1.0 + sign * eps)
    close_fee = np.abs(q * price) * cfg["fee_bps"] / 1e4
    cash += q * price - close_fee
    stats["spread"] += np.abs(q) * s_t * eps
    stats["fee"] += close_fee
    premium = bs_call(cfg["s0"], steps / year, cfg)
    payoff = np.maximum(s_t - cfg["strike"], 0.0)
    pnl = premium + cash - payoff
    stats["max_holding"] = max_holding
    return pnl, stats


def delta_policy(cfg):
    return lambda t, s, tau, h: bs_delta(s, tau, cfg)


def band_policy(w, cfg):
    """plan_band_hedge: exposure h - delta; inside [-w, w] (edges included)
    nothing is sent; outside, the target is the delta (exposure 0)."""

    def policy(t, s, tau, h):
        d = bs_delta(s, tau, cfg)
        e = h - d
        return np.where(np.abs(e) <= w, h, d)

    return policy


# ---------------------------------------------------------------- learned


class Net(torch.nn.Module):
    def __init__(self, hidden: int, bound: float):
        super().__init__()
        self.body = torch.nn.Sequential(
            torch.nn.Linear(3, hidden),
            torch.nn.ReLU(),
            torch.nn.Linear(hidden, hidden),
            torch.nn.ReLU(),
            torch.nn.Linear(hidden, 1),
        )
        self.bound = bound

    def forward(self, x):
        return self.bound * torch.tanh(self.body(x)).squeeze(-1)


def features(s, tau, h, cfg):
    t = np.log(s / cfg["strike"]), tau * cfg["days_per_year"] / cfg["steps"], h
    return np.stack(t, axis=-1)


def train(paths: np.ndarray, eps: float, seed: int, cfg: dict) -> Net:
    """Training proxy (declared): proportional cost (eps + fee) * S * |dh|
    with no lot, size cap or rejection; terminal close at the same cost;
    loss = CVaR_alpha(-PnL) by Rockafellar-Uryasev with a learnt threshold."""
    torch.manual_seed(seed)
    gen = np.random.default_rng(seed)
    net = Net(cfg["hidden"], cfg["holding_bound"]).double()
    threshold = torch.zeros(1, dtype=torch.float64, requires_grad=True)
    opt = torch.optim.Adam(list(net.parameters()) + [threshold], lr=cfg["learning_rate"])
    steps, year, alpha = cfg["steps"], cfg["days_per_year"], cfg["alpha"]
    rate = eps + cfg["fee_bps"] / 1e4
    premium = bs_call(cfg["s0"], steps / year, cfg)
    s_all = torch.from_numpy(paths)
    for _ in range(cfg["iterations"]):
        idx = torch.from_numpy(gen.integers(0, paths.shape[0], cfg["batch"]))
        s = s_all[idx]
        h = torch.zeros(cfg["batch"], dtype=torch.float64)
        pnl = torch.full((cfg["batch"],), premium, dtype=torch.float64)
        for t in range(steps):
            tau = (steps - t) / year
            x = torch.stack(
                [torch.log(s[:, t] / cfg["strike"]), torch.full_like(h, tau * year / steps), h], dim=-1
            )
            h_new = net(x)
            pnl = pnl - rate * s[:, t] * torch.abs(h_new - h) + h_new * (s[:, t + 1] - s[:, t])
            h = h_new
        pnl = pnl - rate * s[:, steps] * torch.abs(h) - torch.clamp(s[:, steps] - cfg["strike"], min=0.0)
        loss = -pnl
        objective = threshold + torch.mean(torch.relu(loss - threshold)) / (1.0 - alpha)
        opt.zero_grad()
        objective.backward()
        opt.step()
    return net


def learned_policy(net: Net, cfg):
    def policy(t, s, tau, h):
        with torch.no_grad():
            x = torch.from_numpy(features(s, np.full_like(s, tau), h, cfg))
            return net(x).numpy()

    return policy


# ---------------------------------------------------------------- metrics


def cvar(loss: np.ndarray, alpha: float) -> float:
    """Empirical CVaR: the mean of the ceil((1 - alpha) N) largest losses
    (an empirical approximation, not exact fractional weighting)."""
    k = math.ceil((1.0 - alpha) * loss.size)
    return float(np.mean(np.partition(loss, loss.size - k)[loss.size - k :]))


def paired_bootstrap(loss_a, loss_b, alpha, cfg, seed_offset=0):
    """95 % percentile CI of CVaR(a) - CVaR(b), resampling whole paths."""
    rng = np.random.default_rng(cfg["bootstrap_seed"] + seed_offset)
    n = loss_a.size
    diffs = np.empty(cfg["bootstrap"])
    for b in range(cfg["bootstrap"]):
        idx = rng.integers(0, n, n)
        diffs[b] = cvar(loss_a[idx], alpha) - cvar(loss_b[idx], alpha)
    return {
        "estimate": cvar(loss_a, alpha) - cvar(loss_b, alpha),
        "ci_low": float(np.percentile(diffs, 2.5)),
        "ci_high": float(np.percentile(diffs, 97.5)),
    }


def summary(pnl, stats, cfg):
    loss = -pnl
    return {
        "mean_pnl": float(np.mean(pnl)),
        "std_pnl": float(np.std(pnl)),
        "cvar95": cvar(loss, 0.95),
        "cvar50": cvar(loss, 0.50),
        "spread_cost": float(np.mean(stats["spread"])),
        "fee_cost": float(np.mean(stats["fee"])),
        "turnover": float(np.mean(stats["turnover"])),
        "trades": float(np.mean(stats["trades"])),
        "rejected_orders": float(np.mean(stats["rejected"])),
        "mean_residual": float(np.mean(stats["residual"]) / cfg["steps"]),
        "max_holding_mean": float(np.mean(stats["max_holding"])),
        "max_holding_p99": float(np.percentile(stats["max_holding"], 99)),
    }


def evaluate(policy, mkt, eps, cfg):
    pnl, stats = simulate(policy, mkt, eps, cfg)
    ideal, _ = simulate(policy, mkt, eps, cfg, p_reject=0.0, unlimited=True)
    out = summary(pnl, stats, cfg)
    # Counterfactual: the same policy with no rejection and unlimited size,
    # on the same paths. Not added to the costs.
    out["cvar95_vs_ideal_execution"] = cvar(-pnl, 0.95) - cvar(-ideal, 0.95)
    out["mean_pnl_vs_ideal_execution"] = float(np.mean(pnl - ideal))
    return out, (-pnl, -ideal)


# ---------------------------------------------------------------- fixture


def export_fixture(mkt, eps, w, cfg, path: Path):
    """The band control run in `decimal` on the exported, rounded values:
    S and delta to 1e-6, holdings and sizes on the lot grid. Each row is one
    plan_band_hedge decision; the Rust test recomputes it exactly."""
    getcontext().prec = 60
    q6 = Decimal("0.000001")
    lot = Decimal(str(cfg["lot"]))
    eps_d = Decimal(str(eps))
    fee_bps = Decimal(str(cfg["fee_bps"]))
    w_d = Decimal(str(w))
    steps, year = cfg["steps"], cfg["days_per_year"]
    rows = []

    def decide(case, holding, delta, bid, ask):
        exposure = holding - delta
        row = {
            "case": case, "holding": holding, "delta": delta, "w": w_d,
            "bid": bid[0] if bid else "", "bid_size": bid[1] if bid else "",
            "ask": ask[0] if ask else "", "ask_size": ask[1] if ask else "",
            "fee_bps": fee_bps, "lot": lot,
            "decision": "", "side": "", "quantity": "", "fee": "", "spread_cost": "",
        }
        if abs(exposure) <= w_d:
            row["decision"] = "inside"
            return row, None
        side = "sell" if exposure > w_d else "buy"
        row["side"] = side
        level = bid if side == "sell" else ask
        if level is None:
            row["decision"] = "no_liquidity"
            return row, None
        executable = min(abs(exposure), level[1])
        lots = (executable / lot).to_integral_value(rounding=ROUND_FLOOR)
        if lots == 0:
            row["decision"] = "below_lot"
            row["quantity"] = executable
            return row, None
        quantity = lots * lot
        row["decision"] = "hedge"
        row["quantity"] = quantity
        row["fee"] = abs(level[0] * quantity) * fee_bps / Decimal(10000)
        row["spread_cost"] = Decimal("0.5") * ask[0] * quantity - Decimal("0.5") * bid[0] * quantity if bid and ask else ""
        return row, (side, quantity)

    for p in range(cfg["fixture_paths"]):
        holding = Decimal(0)
        for t in range(steps):
            tau = (steps - t) / year
            s = Decimal(repr(float(mkt["s"][p, t]))).quantize(q6, rounding=ROUND_HALF_EVEN)
            delta = Decimal(repr(float(bs_delta(np.array([float(s)]), tau, cfg)[0]))).quantize(
                q6, rounding=ROUND_HALF_EVEN
            )
            size = int(mkt["size_lots"][p, t]) * lot
            bid, ask = (s * (1 - eps_d), size), (s * (1 + eps_d), size)
            row, fill = decide(f"path{p}-t{t}", holding, delta, bid, ask)
            rows.append(row)
            if fill and mkt["u_reject"][p, t] >= cfg["p_reject"]:
                holding += fill[1] if fill[0] == "buy" else -fill[1]

    # Hand-made boundary cases around one state.
    s = Decimal("100.000000")
    delta = Decimal("0.500000")
    big = Decimal("10")
    book = lambda size=big: ((s * (1 - eps_d), size), (s * (1 + eps_d), size))  # noqa: E731
    cases = [
        ("edge_upper", delta + w_d, book()),
        ("edge_lower", delta - w_d, book()),
        ("one_quantum_above", delta + w_d + q6, book()),
        ("one_quantum_below", delta - w_d - q6, book()),
        ("no_bid", delta + w_d + q6, (None, book()[1])),
        ("no_ask", delta - w_d - q6, (book()[0], None)),
        ("below_lot", delta + w_d + q6, book(Decimal("0.00005"))),
        ("capped_by_size", delta + w_d + Decimal("0.5"), book(Decimal("0.0123"))),
    ]
    for case, holding, (bid, ask) in cases:
        rows.append(decide(case, holding, delta, bid, ask)[0])

    cols = list(rows[0].keys())
    with path.open("w") as f:
        f.write(",".join(cols) + "\n")
        for row in rows:
            f.write(",".join(str(row[c]) for c in cols) + "\n")
    return len(rows)


# ---------------------------------------------------------------- main


def main() -> None:
    cfg = CONFIG
    torch.set_num_threads(1)
    torch.use_deterministic_algorithms(True)
    started = time.time()
    seq = np.random.SeedSequence(cfg["data_seed"])
    gens = [np.random.default_rng(s) for s in seq.spawn(7)]
    train_paths = gbm_paths(cfg["n_train"], gens[0], cfg)
    val = market(gbm_paths(cfg["n_val"], gens[1], cfg), gens[2], cfg)
    test = market(gbm_paths(cfg["n_test"], gens[3], cfg), gens[4], cfg)
    heston = market(heston_paths(cfg["n_test"], gens[5], cfg), gens[6], cfg)

    results = {"config": cfg, "by_eps": {}}
    for eps in cfg["eps_list"]:
        key = f"{eps:g}"
        # Band width chosen on validation only.
        val_cvar = {
            w: cvar(-simulate(band_policy(w, cfg), val, eps, cfg)[0], cfg["alpha"]) for w in cfg["band_grid"]
        }
        best_w = min(val_cvar, key=lambda w: (val_cvar[w], w))
        entry = {"band_validation_cvar95": {f"{w:g}": v for w, v in val_cvar.items()}, "band_w": best_w}
        for name, mkt in (("in_model", test), ("heston", heston)):
            d, d_loss = evaluate(delta_policy(cfg), mkt, eps, cfg)
            b, b_loss = evaluate(band_policy(best_w, cfg), mkt, eps, cfg)
            entry[name] = {"delta": d, "band": b, "learned": []}
            entry[name]["_losses"] = (d_loss, b_loss)
        entry["learned_seeds"] = []
        for i, seed in enumerate(cfg["train_seeds"]):
            net = train(train_paths, eps, seed, cfg)
            for name, mkt in (("in_model", test), ("heston", heston)):
                lr, (l_loss, l_ideal) = evaluate(learned_policy(net, cfg), mkt, eps, cfg)
                b_loss, b_ideal = entry[name]["_losses"][1]
                lr["seed"] = seed
                lr["delta_cvar95_vs_band"] = paired_bootstrap(l_loss, b_loss, 0.95, cfg, seed_offset=i)
                # Secondary (declared after the first run, not a decision
                # input): the same comparison with no rejection and unlimited
                # size, which separates cost handling from execution limits.
                lr["delta_cvar95_vs_band_ideal_execution"] = paired_bootstrap(
                    l_ideal, b_ideal, 0.95, cfg, seed_offset=100 + i
                )
                entry[name]["learned"].append(lr)
        for name in ("in_model", "heston"):
            del entry[name]["_losses"]
        results["by_eps"][key] = entry
        print(f"eps {key}: band w {best_w:g}, {time.time() - started:.0f} s", flush=True)

    primary = results["by_eps"][f"{cfg['primary_eps']:g}"]
    adopt_in = all(l["delta_cvar95_vs_band"]["ci_high"] < 0 for l in primary["in_model"]["learned"])
    veto_out = any(l["delta_cvar95_vs_band"]["ci_low"] > 0 for l in primary["heston"]["learned"])
    results["decision"] = {
        "criterion": "adopt iff, at the primary eps in model, the upper bound of the 95 % paired bootstrap CI of "
        "CVaR95(learned) - CVaR95(best band) is below 0 for every seed, and no seed is significantly worse "
        "out of model (lower bound above 0)",
        "in_model_all_seeds_better": adopt_in,
        "out_of_model_veto": veto_out,
        "adopt": adopt_in and not veto_out,
    }
    fixture = ROOT / "tests" / "fixtures" / "deep_hedging_band.csv"
    rows = export_fixture(test, cfg["primary_eps"], primary["band_w"], cfg, fixture)
    results["run"] = {
        "seconds": round(time.time() - started, 1),
        "python": platform.python_version(),
        "numpy": np.__version__,
        "torch": torch.__version__,
        "machine": platform.machine(),
        "git_commit": subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True, check=False
        ).stdout.strip(),
        "fixture_rows": rows,
    }
    out = ROOT / "research" / "results" / "deep_hedging.json"
    out.write_text(json.dumps(results, indent=2, sort_keys=True) + "\n")
    print(json.dumps(results["decision"], indent=2))
    print(f"done in {results['run']['seconds']} s", file=sys.stderr)


if __name__ == "__main__":
    main()
