"""M5-06c (#37): the trained deep hedging controls on a dataset, without
retraining.

Run from the repository root:

    experiments/.venv/bin/python experiments/empirical.py --dataset experiments/datasets/<name>

It writes `research/results/deep_hedging_dataset_<name>.{json,md}`.

Declared before any run:

- **Controls.** The learned controls are the frozen M5-06 nets of the
  primary half spread (`research/models/`, checked against the sha256 in
  `deep_hedging_counterpart.json`; a mismatch is an error). Nothing is
  trained or selected on the dataset. Delta and band are the M5-06
  Black-Scholes delta at sigma = 0.2; the band width is the M5-06 one at
  the primary half spread, chosen on the synthetic GBM validation set.
- **Mapping.** S is the mid. Each path is rebased so that its first mid is
  s0 = 100 (bid and ask scale with it), the strike is s0 (at the money at
  step 0), the expiry is `steps` days, one decision per day. The manifest
  must match the trained configuration (steps, step_days, lot, fee).
- **Information set.** A control sees the rebased mid at t, the time to
  expiry and its own holding (past fills only). The execution row of t is
  used only after the decision at t.
- **Execution.** One order per step towards the target: a buy at the ask, a
  sell at the bid, the size floored to whole lots and capped by the
  queue model's capacity on that side (`fillable_buy` or `fillable_sell`).
  A rejected order fills 0 at zero cost and leaves the holding unchanged.
  The fee is `fee_bps` of the notional, in the quote currency.
- **Expiry.** The holding is closed in full at the expiry row's bid or ask
  with the fee; the dataset gives no capacity for that close, which is a
  declared residual assumption. The call pays max(S_T - K, 0) on the
  rebased mid. The premium credited to every control is the M5-06
  Black-Scholes price.
- **Metrics.** The M5-06 tail convention and paired bootstrap; the "ideal
  execution" counterfactual removes rejections and capacity.
- **Sections kept apart.** The paper-model reproduction (M5-06 in model),
  the out-of-model results (M5-06 Heston and M5-06b) and the dataset are
  reported separately; the first two are copied from their result files.
- A dataset flagged synthetic is a pipeline check, not empirical evidence:
  no decision is issued. For a captured dataset the decision criterion
  must be pre-registered with the dataset before the run (#37).
"""

from __future__ import annotations

import argparse
import json
import platform
import subprocess
import sys
from pathlib import Path

import numpy as np
import torch

import dataset as ds
from deep_hedging import CONFIG, ROOT, band_policy, bs_call, cvar, delta_policy, learned_policy, paired_bootstrap, summary
from deep_hedging_counterpart import load_weights

RESULTS = ROOT / "research" / "results"
SYNTHETIC_LABEL = "pipeline check on a synthetic dataset, not empirical evidence"


def compatible(manifest: dict, cfg: dict) -> list[str]:
    """The dataset must match what the controls were trained for."""
    out = []
    if manifest["steps"] != cfg["steps"]:
        out.append(f"steps {manifest['steps']} != trained {cfg['steps']}")
    if manifest["step_days"] != 1.0:
        out.append(f"step_days {manifest['step_days']} != 1 (one decision per day)")
    if manifest["contract"]["expiry_days"] != float(cfg["steps"]):
        out.append(f"expiry_days {manifest['contract']['expiry_days']} != {cfg['steps']}")
    if manifest["contract"]["kind"] != "call":
        out.append("the trained controls hedge a call")
    if manifest["lot"] != cfg["lot"]:
        out.append(f"lot {manifest['lot']} != trained {cfg['lot']}")
    if manifest["fee_bps"] != cfg["fee_bps"]:
        out.append(f"fee_bps {manifest['fee_bps']} != trained {cfg['fee_bps']}")
    return out


def rebased(data: ds.Dataset, cfg: dict) -> dict:
    scale = cfg["s0"] / data.mid[:, :1]
    return {"bid": data.bid * scale, "ask": data.ask * scale, "mid": data.mid * scale}


def simulate(policy, book, data: ds.Dataset, cfg: dict, ideal: bool = False, trace: list | None = None):
    """The M5-06 account on the dataset's book and capacities."""
    lot, fee = cfg["lot"], cfg["fee_bps"] / 1e4
    n, steps = book["mid"].shape[0], cfg["steps"]
    holding = np.zeros(n, dtype=np.int64)
    cash = np.zeros(n)
    stats = {k: np.zeros(n) for k in ("spread", "fee", "rebalance_turnover", "rebalance_trades", "rejected", "residual")}
    max_holding = np.zeros(n)
    for t in range(steps):
        tau = (steps - t) / cfg["days_per_year"]
        s = book["mid"][:, t]
        target = np.clip(policy(t, s, tau, holding * lot), -cfg["holding_bound"], cfg["holding_bound"])
        if trace is not None:
            trace.append(target.copy())
        # Only now is the execution row of step t used.
        order = target - holding * lot
        want = np.floor(np.abs(order) / lot + 1e-9).astype(np.int64)
        buy = order > 0
        if ideal:
            lots = want
            rejected = np.zeros(n, dtype=bool)
        else:
            cap = np.where(buy, data.fillable_buy[:, t], data.fillable_sell[:, t])
            lots = np.minimum(want, cap)
            rejected = (lots > 0) & (data.reject[:, t] == 1)
        filled = np.where(rejected, 0, lots)
        price = np.where(buy, book["ask"][:, t], book["bid"][:, t])
        q = filled * lot
        sign = np.sign(order)
        cash += -sign * q * price - q * price * fee
        stats["spread"] += q * np.abs(price - s)
        stats["fee"] += q * price * fee
        stats["rebalance_turnover"] += q
        stats["rebalance_trades"] += filled > 0
        stats["rejected"] += rejected
        holding = holding + (sign * filled).astype(np.int64)
        stats["residual"] += np.abs(target - holding * lot)
        max_holding = np.maximum(max_holding, np.abs(holding * lot))
    q = holding * lot
    s_t = book["mid"][:, steps]
    close_price = np.where(q > 0, book["bid"][:, steps], book["ask"][:, steps])
    cash += q * close_price - np.abs(q) * close_price * fee
    stats["spread"] += np.abs(q) * np.abs(close_price - s_t)
    stats["fee"] += np.abs(q) * close_price * fee
    premium = bs_call(cfg["s0"], steps / cfg["days_per_year"], cfg)
    pnl = premium + cash - np.maximum(s_t - cfg["strike"], 0.0)
    stats["max_holding"] = max_holding
    return pnl, stats


def evaluate(policy, book, data, cfg):
    pnl, stats = simulate(policy, book, data, cfg)
    ideal, _ = simulate(policy, book, data, cfg, ideal=True)
    out = summary(pnl, stats, cfg)
    out["cvar95_vs_ideal_execution"] = cvar(-pnl, 0.95) - cvar(-ideal, 0.95)
    out["mean_pnl_vs_ideal_execution"] = float(np.mean(pnl - ideal))
    return out, (-pnl, -ideal)


def frozen_nets(cfg: dict) -> list:
    """The primary half spread's nets, checked against their recorded hashes."""
    record = json.loads((RESULTS / "deep_hedging_counterpart.json").read_text())["weights"]
    key = f"{cfg['primary_eps']:g}"
    nets = []
    for seed in cfg["train_seeds"]:
        w = record[f"{key}/{seed}"]
        nets.append((seed, w, load_weights(ROOT / w["file"], w["sha256"], cfg)))
    return nets


def run(directory: Path, cfg: dict = CONFIG) -> dict:
    data = ds.load(directory)
    problems = compatible(data.manifest, cfg)
    if problems:
        raise SystemExit("dataset does not match the trained controls: " + "; ".join(problems))
    torch.set_num_threads(1)
    torch.use_deterministic_algorithms(True)
    legacy = json.loads((RESULTS / "deep_hedging.json").read_text())
    counterpart = json.loads((RESULTS / "deep_hedging_counterpart.json").read_text())
    key = f"{cfg['primary_eps']:g}"
    band_w = legacy["by_eps"][key]["band_w"]
    book = rebased(data, cfg)
    d, _ = evaluate(delta_policy(cfg), book, data, cfg)
    b, (b_loss, b_ideal) = evaluate(band_policy(band_w, cfg), book, data, cfg)
    learned = []
    for i, (seed, w, net) in enumerate(frozen_nets(cfg)):
        lr, (l_loss, l_ideal) = evaluate(learned_policy(net, cfg), book, data, cfg)
        lr["seed"] = seed
        lr["weights_sha256"] = w["sha256"]
        lr["delta_cvar95_vs_band"] = paired_bootstrap(l_loss, b_loss, 0.95, cfg, seed_offset=i)
        lr["delta_cvar95_vs_band_ideal_execution"] = paired_bootstrap(l_ideal, b_ideal, 0.95, cfg, seed_offset=100 + i)
        learned.append(lr)
    m = data.manifest
    synthetic = m["synthetic"]
    return {
        "label": SYNTHETIC_LABEL if synthetic else "captured dataset",
        "evidence": "none: synthetic dataset" if synthetic else "empirical, under the declared queue model",
        "decision": None,
        "decision_note": "no decision on a synthetic dataset"
        if synthetic
        else "the decision criterion must be pre-registered with the dataset before the run",
        "dataset": {
            "name": m["name"],
            "version": m["version"],
            "source": m["source"],
            "synthetic": synthetic,
            "seed": m["seed"],
            "sha256": data.sha256,
            "manifest_sha256": ds.sha256_file(Path(directory) / "manifest.json"),
            "queue_model": m["queue_model"],
            "paths": int(book["mid"].shape[0]),
        },
        "mapping": {
            "spot": "mid, each path rebased to s0 at step 0",
            "s0": cfg["s0"],
            "strike": cfg["strike"],
            "pricer": "Black-Scholes at sigma = 0.2, as trained",
            "band_w": band_w,
            "nets": f"frozen, primary half spread {key}",
            "terminal_close": "in full at the expiry row's bid or ask, no capacity limit (declared residual assumption)",
        },
        "paper_model_reproduction": {
            "source": "research/results/deep_hedging.json",
            "in_model": legacy["by_eps"][key]["in_model"],
        },
        "out_of_model": {
            "source": "research/results/deep_hedging.json, research/results/deep_hedging_counterpart.json",
            "heston_flat": legacy["by_eps"][key]["heston"],
            **{k: v for k, v in counterpart["by_eps"][key].items() if k.startswith("heston_")},
        },
        "on_dataset": {"delta": d, "band": b, "learned": learned},
        "run": {
            "python": platform.python_version(),
            "numpy": np.__version__,
            "torch": torch.__version__,
            "machine": platform.machine(),
            "git_commit": subprocess.run(
                ["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True, check=False
            ).stdout.strip(),
        },
    }


def render(res: dict) -> str:
    dset = res["dataset"]
    lines = [
        f"# Deep hedging controls on dataset `{dset['name']}` (v{dset['version']})",
        "",
    ]
    if dset["synthetic"]:
        lines += [
            f"> **{SYNTHETIC_LABEL.capitalize()}.** This dataset is synthetic "
            f"(`{dset['source']}`, seed {dset['seed']}). The numbers below check that the pipeline runs; they say "
            "nothing about real markets, and no decision is issued. With 64 paths the intervals have little power.",
            "",
        ]
    lines += [
        f"Generated by `experiments/empirical.py`; every number is in `{Path(res['_json']).name}`. The mapping, the "
        "information set and the execution rules are in the docstring of `experiments/empirical.py`.",
        "",
        "## Dataset",
        "",
        f"- Paths: {dset['paths']}; source: {dset['source']}.",
        f"- sha256: manifest `{dset['manifest_sha256']}`, paths `{dset['sha256']['paths.csv']}`, "
        f"execution `{dset['sha256']['execution.csv']}`.",
        f"- Queue model: `{dset['queue_model']['id']}` v{dset['queue_model']['version']}. Declared limits:",
    ]
    lines += [f"  - {x}" for x in dset["queue_model"]["limits"]]
    lines += [
        "",
        "## On the dataset",
        "",
        "| control | mean PnL | std | CVaR95 | CVaR50 | spread + fee | rebalances | rejected | CVaR95 − ideal execution |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    on = res["on_dataset"]

    def row(label, x):
        return (
            f"| {label} | {x['mean_pnl']:.3f} | {x['std_pnl']:.3f} | {x['cvar95']:.3f} | {x['cvar50']:.3f} | "
            f"{x['spread_cost'] + x['fee_cost']:.3f} | {x['rebalance_trades']:.1f} | {x['rejected_orders']:.2f} | "
            f"{x['cvar95_vs_ideal_execution']:.3f} |"
        )

    lines.append(row("delta", on["delta"]))
    lines.append(row(f"band (w = {res['mapping']['band_w']:g})", on["band"]))
    for lr in on["learned"]:
        lines.append(row(f"learned, seed {lr['seed']}", lr))
    lines += [
        "",
        "Paired ΔCVaR95 = learned − band, 95 % bootstrap CI:",
        "",
        "| seed | executable | ideal execution |",
        "|---:|---|---|",
    ]
    f = lambda d: f"{d['estimate']:.3f} [{d['ci_low']:.3f}, {d['ci_high']:.3f}]"  # noqa: E731
    for lr in on["learned"]:
        lines.append(f"| {lr['seed']} | {f(lr['delta_cvar95_vs_band'])} | {f(lr['delta_cvar95_vs_band_ideal_execution'])} |")
    lines += [
        "",
        "## Kept apart",
        "",
        "- Paper-model reproduction (M5-06, in model): `research/results/deep_hedging.md`; copied in the json.",
        "- Out of model (M5-06 Heston and M5-06b): `research/results/deep_hedging.md` and "
        "`research/results/deep_hedging_counterpart.md`; copied in the json.",
        "",
        f"Decision: {res['decision_note']}.",
        "",
    ]
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser(description="M5-06c: frozen deep hedging controls on a dataset")
    parser.add_argument("--dataset", required=True, type=Path)
    parser.add_argument("--out-dir", type=Path, default=RESULTS)
    args = parser.parse_args()
    violations = ds.validate(args.dataset)
    if violations:
        print("\n".join(violations), file=sys.stderr)
        raise SystemExit(2)
    res = run(args.dataset)
    name = res["dataset"]["name"]
    out_json = args.out_dir / f"deep_hedging_dataset_{name}.json"
    res["_json"] = str(out_json)
    md = render(res)
    del res["_json"]
    out_json.write_text(json.dumps(res, indent=2, sort_keys=True) + "\n")
    (args.out_dir / f"deep_hedging_dataset_{name}.md").write_text(md)
    print(f"wrote {out_json}: {res['label']}")


if __name__ == "__main__":
    main()
