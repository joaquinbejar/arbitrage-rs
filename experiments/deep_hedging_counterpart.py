"""M5-06b (#36): the M5-06 comparison against a pricing counterpart.

Run `experiments/.venv/bin/python experiments/deep_hedging.py --counterpart
reference` from the repository root. It writes
`research/results/deep_hedging_counterpart.json` and the frozen weights of
the learned controls in `research/models/`.

What changes against M5-06, and only this (declared before the run):

- In model (GBM): the surface is FlatSurface(sigma), so delta, band and
  premium are the M5-06 functions; the in-model rows must equal
  `deep_hedging.json` and the run checks it.
- Out of model (Heston): delta and band use the counterpart's delta on the
  reference surface (raw SVI fitted to semi-analytic Heston prices with the
  same Heston parameters, static, calibrated once at t = 0), under each of
  the two dynamics, sticky-strike and sticky-moneyness; the premium is the
  counterpart's price at t = 0. The band width is the one chosen on the GBM
  validation set, as in M5-06.
- The learned controls are trained exactly as in M5-06 (same data, seeds
  and proxy); their inputs do not see the counterpart, so any change in
  the conclusion comes only from the delta and band baselines and the
  premium.

Decision (pre-registered): at the primary half spread,

- in_model: the upper bound of the paired 95 % interval of
  CVaR95(learned) - CVaR95(band) is below 0 for every seed (unchanged);
- veto(D): under Heston with dynamics D, some seed has its lower bound
  above 0;
- ADOPT if in_model holds and there is no veto under either dynamics;
  REJECT if in_model fails or there is a veto under both; REVISE if
  in_model holds and there is a veto under exactly one.

The result is marked `counterpart_validation: "pending"` until the
market-maker-rs counterpart replaces the reference.
"""

from __future__ import annotations

import hashlib
import io
import json
import platform
import subprocess
import sys
import time
import zipfile
from pathlib import Path

import numpy as np
import torch

import calibration
import counterpart as cp
from deep_hedging import (
    CONFIG,
    ROOT,
    Net,
    band_policy,
    bs_delta,
    cvar,
    delta_policy,
    evaluate,
    gbm_paths,
    heston_paths,
    learned_policy,
    market,
    paired_bootstrap,
    simulate,
    train,
)

COUNTERPART_CONFIG = {
    "s_ref": CONFIG["s0"],
    "days": CONFIG["steps"],
    "year_days": CONFIG["days_per_year"],
    "heston": {k: CONFIG["heston"][k] for k in ("v0", "kappa", "theta", "xi", "rho")},
    # Strikes at k = k_std * sqrt(v0 tau), 33 points from -4 to 4.
    "k_std": [float(x) for x in np.linspace(-4.0, 4.0, 33)],
    "quad": {"nodes": 32, "cutoff": 40.0},
    "quad_tol": 1e-10,
    "min_price": 1e-10,
    "iv_price_tol": 1e-11,
    "svi_fit": {"grid": 21, "refinements": 12, "sigma_lo": 0.005, "sigma_hi": 2.0, "max_recentres": 40},
    "diagnostic_points": 2001,
}

MODELS = ROOT / "research" / "models"
OUT = ROOT / "research" / "results" / "deep_hedging_counterpart.json"
LEGACY = ROOT / "research" / "results" / "deep_hedging.json"


def sha256_bytes(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()


def sha256_json(obj) -> str:
    return sha256_bytes(json.dumps(obj, sort_keys=True, separators=(",", ":")).encode())


# ---------------------------------------------------------------- weights


def weights_bytes(net: Net) -> bytes:
    """The parameters as an .npz with fixed member dates, so the bytes (and
    their hash) depend on the parameters only."""
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", compression=zipfile.ZIP_STORED) as z:
        for name, tensor in sorted(net.state_dict().items()):
            arr = io.BytesIO()
            np.lib.format.write_array(arr, np.ascontiguousarray(tensor.detach().numpy()), allow_pickle=False)
            z.writestr(zipfile.ZipInfo(name + ".npy", date_time=(1980, 1, 1, 0, 0, 0)), arr.getvalue())
    return buf.getvalue()


def load_weights(path: Path, sha256: str, cfg: dict) -> Net:
    """A frozen net from its .npz; a hash mismatch is an error."""
    data = path.read_bytes()
    got = sha256_bytes(data)
    if got != sha256:
        raise ValueError(f"{path}: sha256 {got} does not match the recorded {sha256}")
    net = Net(cfg["hidden"], cfg["holding_bound"]).double()
    with np.load(io.BytesIO(data), allow_pickle=False) as npz:
        state = {name: torch.from_numpy(npz[name]) for name in npz.files}
    net.load_state_dict(state, strict=True)
    return net


# ---------------------------------------------------------------- policies


def cp_delta_policy(c, contract):
    """The counterpart's delta; one decision per day, so as_of_days = t."""
    return lambda t, s, tau, h: c.greeks(contract, s, float(t))["delta"]


def cp_band_policy(w, c, contract):
    """plan_band_hedge semantics on the counterpart's delta."""

    def policy(t, s, tau, h):
        d = c.greeks(contract, s, float(t))["delta"]
        return np.where(np.abs(h - d) <= w, h, d)

    return policy


def realised_k_by_day(paths: np.ndarray, cfg: dict) -> dict:
    """Per remaining day, the range of k = ln(K / S_t) on the paths."""
    out = {}
    for t in range(cfg["steps"]):
        k = np.log(cfg["strike"] / paths[:, t])
        out[cfg["steps"] - t] = (float(np.min(k)), float(np.max(k)))
    return out


def strip_losses(entry):
    return {k: v for k, v in entry.items() if not k.startswith("_")}


# ---------------------------------------------------------------- main


def main() -> None:
    cfg = CONFIG
    ccfg = COUNTERPART_CONFIG
    torch.set_num_threads(1)
    torch.use_deterministic_algorithms(True)
    started = time.time()
    seq = np.random.SeedSequence(cfg["data_seed"])
    gens = [np.random.default_rng(s) for s in seq.spawn(7)]
    train_paths = gbm_paths(cfg["n_train"], gens[0], cfg)
    val = market(gbm_paths(cfg["n_val"], gens[1], cfg), gens[2], cfg)
    test = market(gbm_paths(cfg["n_test"], gens[3], cfg), gens[4], cfg)
    heston = market(heston_paths(cfg["n_test"], gens[5], cfg), gens[6], cfg)

    # Surface: calibrated once, before any path is evaluated.
    slices, cal = calibration.calibrate(ccfg)
    diag = calibration.diagnose(slices, realised_k_by_day(heston["s"], cfg), ccfg)
    print(f"surface calibrated, {time.time() - started:.0f} s", flush=True)
    if cal["failures"] or diag["failures"]:
        print(json.dumps({"calibration": cal["failures"], "diagnostics": diag["failures"]}, indent=2))
        raise SystemExit(2)
    surface = cp.SviSurface(slices)
    contract = cp.Contract(strike=cfg["strike"], expiry_days=float(cfg["steps"]), kind="call")
    cps = {d: cp.ReferenceCounterpart(surface, d, s_ref=cfg["s0"]) for d in cp.DYNAMICS}
    flat = cp.ReferenceCounterpart(cp.FlatSurface(cfg["sigma"]), "sticky-moneyness", s_ref=cfg["s0"])
    premium_svi = float(cps["sticky-moneyness"].price(contract, np.array([cfg["s0"]]), 0.0)[0])
    # The flat counterpart is the M5-06 delta, bit for bit, on the test paths.
    flat_same = all(
        np.array_equal(
            flat.greeks(contract, test["s"][:, t], float(t))["delta"],
            bs_delta(test["s"][:, t], (cfg["steps"] - t) / cfg["days_per_year"], cfg),
        )
        for t in range(cfg["steps"])
    )
    if not flat_same:
        raise SystemExit("the flat counterpart does not reproduce the M5-06 delta")

    MODELS.mkdir(parents=True, exist_ok=True)
    results = {
        "counterpart_validation": "pending",
        "pending_reason": "reference counterpart; market-maker-rs#95/#96 not available",
        "config": cfg,
        "config_sha256": sha256_json(cfg),
        "counterpart_config": ccfg,
        "counterpart_config_sha256": sha256_json(ccfg),
        "counterparts": {
            "in_model": flat.identity() | {"surface": "flat"},
            **{f"heston_{d}": c.identity() for d, c in cps.items()},
        },
        "market_maker_rs": cp.MarketMakerRsCounterpart().identity(),
        "calibration": cal,
        "diagnostics": diag,
        "premium": {"in_model": None, "heston_svi": premium_svi},
        "by_eps": {},
        "weights": {},
    }
    for eps in cfg["eps_list"]:
        key = f"{eps:g}"
        val_cvar = {
            w: cvar(-simulate(band_policy(w, cfg), val, eps, cfg)[0], cfg["alpha"]) for w in cfg["band_grid"]
        }
        best_w = min(val_cvar, key=lambda w: (val_cvar[w], w))
        entry = {"band_w": best_w}
        d, d_loss = evaluate(delta_policy(cfg), test, eps, cfg)
        b, b_loss = evaluate(band_policy(best_w, cfg), test, eps, cfg)
        entry["in_model"] = {"delta": d, "band": b, "learned": [], "_band_loss": b_loss}
        for dyn, c in cps.items():
            d, _ = evaluate(cp_delta_policy(c, contract), heston, eps, cfg, premium=premium_svi)
            b, b_loss = evaluate(cp_band_policy(best_w, c, contract), heston, eps, cfg, premium=premium_svi)
            entry[f"heston_{dyn}"] = {"delta": d, "band": b, "learned": [], "_band_loss": b_loss}
        for i, seed in enumerate(cfg["train_seeds"]):
            net = train(train_paths, eps, seed, cfg)
            blob = weights_bytes(net)
            name = f"deep_hedging_eps{key}_seed{seed}.npz"
            (MODELS / name).write_bytes(blob)
            results["weights"][f"{key}/{seed}"] = {"file": f"research/models/{name}", "sha256": sha256_bytes(blob)}
            markets = [("in_model", test, None)] + [(f"heston_{d}", heston, premium_svi) for d in cps]
            for mname, mkt, prem in markets:
                lr, (l_loss, l_ideal) = evaluate(learned_policy(net, cfg), mkt, eps, cfg, premium=prem)
                b_loss, b_ideal = entry[mname]["_band_loss"]
                lr["seed"] = seed
                lr["delta_cvar95_vs_band"] = paired_bootstrap(l_loss, b_loss, 0.95, cfg, seed_offset=i)
                lr["delta_cvar95_vs_band_ideal_execution"] = paired_bootstrap(
                    l_ideal, b_ideal, 0.95, cfg, seed_offset=100 + i
                )
                entry[mname]["learned"].append(lr)
        for mname in list(entry):
            if isinstance(entry[mname], dict):
                entry[mname] = strip_losses(entry[mname])
        results["by_eps"][key] = entry
        print(f"eps {key}: band w {best_w:g}, {time.time() - started:.0f} s", flush=True)

    # The in-model rows must be the M5-06 rows.
    legacy = json.loads(LEGACY.read_text())
    results["in_model_matches_m5_06"] = all(
        results["by_eps"][k]["in_model"] == legacy["by_eps"][k]["in_model"]
        and results["by_eps"][k]["band_w"] == legacy["by_eps"][k]["band_w"]
        for k in results["by_eps"]
    )
    primary = results["by_eps"][f"{cfg['primary_eps']:g}"]
    in_model = all(l["delta_cvar95_vs_band"]["ci_high"] < 0 for l in primary["in_model"]["learned"])
    veto = {
        d: any(l["delta_cvar95_vs_band"]["ci_low"] > 0 for l in primary[f"heston_{d}"]["learned"]) for d in cps
    }
    if not in_model or all(veto.values()):
        outcome = "reject"
    elif any(veto.values()):
        outcome = "revise"
    else:
        outcome = "adopt"
    results["decision"] = {
        "criterion": "at the primary eps: in_model holds iff the upper bound of the 95 % paired bootstrap CI of "
        "CVaR95(learned) - CVaR95(band) is below 0 for every seed (flat surface, as M5-06); veto(D) iff some seed "
        "has its lower bound above 0 under Heston with the reference surface and dynamics D. ADOPT iff in_model "
        "and no veto under either dynamics; REJECT iff not in_model or a veto under both; REVISE otherwise",
        "in_model_all_seeds_better": in_model,
        "out_of_model_veto": veto,
        "outcome": outcome,
        "counterpart_validation": "pending",
    }
    results["run"] = {
        "seconds": round(time.time() - started, 1),
        "python": platform.python_version(),
        "numpy": np.__version__,
        "torch": torch.__version__,
        "machine": platform.machine(),
        "git_commit": subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True, check=False
        ).stdout.strip(),
    }
    OUT.write_text(json.dumps(results, indent=2, sort_keys=True) + "\n")
    print(json.dumps(results["decision"], indent=2))
    print(f"done in {results['run']['seconds']} s", file=sys.stderr)
    if not results["in_model_matches_m5_06"]:
        raise SystemExit("the in-model rows differ from deep_hedging.json")
