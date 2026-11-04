"""Render `research/results/deep_hedging_mm_run3.md` from run 3's records.

    python3 research/deep_hedging_mm_run3_report.py

Formatting only: every number comes from the committed results and the
attempt's records, and the outcome is recomputed from the rows under the
preregistered rule and must equal the recorded one. It uses the standard
library alone and lives outside `experiments/`, whose tracked files the
launch manifest pins.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RESULTS = ROOT / "research" / "results"
RUN3 = RESULTS / "deep_hedging_mm_run3.json"
ATTEMPT = RESULTS / "deep_hedging_mm_run3" / "attempt-1"
OUT = RESULTS / "deep_hedging_mm_run3.md"
DYNAMICS = ("sticky-moneyness", "sticky-strike")
KEYS = ("price", "delta", "gamma", "vega", "theta")
PANELS = ("all", "in_range", "extrapolated")
# The time of the authorization, which the records of the run do not hold.
AUTHORIZED_AT = "2026-11-03T18:34:28Z"


def pct(x: float) -> str:
    return f"{100.0 * x:.2f} %"


def ci(c: dict) -> str:
    return f"{c['estimate']:+.4f} [{c['ci_low']:+.4f}, {c['ci_high']:+.4f}]"


def recompute(r: dict) -> tuple[str, str]:
    """The criterion and the outcome, from the rows at the primary half
    spread and the gates, under the preregistered rule."""
    primary = r["by_eps"][f"{r['config']['primary_eps']:g}"]
    in_model = all(x["delta_cvar95_vs_band"]["ci_high"] < 0 for x in primary["in_model"]["learned"])
    veto = [any(x["delta_cvar95_vs_band"]["ci_low"] > 0 for x in primary[f"heston_{d}"]["learned"]) for d in DYNAMICS]
    criterion = "reject" if not in_model or all(veto) else "revise" if any(veto) else "adopt"
    gates = r["decision"]["gates"]
    return criterion, criterion if criterion != "adopt" or all(gates.values()) else "revise"


def header(r: dict, raw: bytes, m: dict) -> list[str]:
    d = r["decision"]
    launch = m["launch"]
    return [
        "# Deep hedging against the market-maker-rs counterpart, run 3 (#43)",
        "",
        f"Outcome: **{d['outcome'].upper()}** (criterion {d['criterion_outcome']}, all {len(d['gates'])} gates hold), the "
        "preregistered result. Its scope is further validation, in this synthetic setting only.",
        "",
        "Run 3 is a new held-out evaluation after run 2, with no prior access to its paths. The learned hedge keeps its "
        "in-model result, and on a new Heston set no seed is significantly worse than the band under either dynamics. "
        "market-maker-rs and the reference agree on every row within the tolerances. This is the counterpart result that "
        "the reference run was pending. It says nothing about live markets (see \"Limitations\"), and it rests on the "
        "amended `k` rule (see \"Not in the decision\").",
        "",
        "## Protocol",
        "",
        "- Preregistration: `research/preregistration/deep_hedging_mm_v2.md`.",
        f"- Code commit C `{launch['code_commit']}`; preregistration sha256 `{launch['preregistration_sha256']}`; "
        f"template sha256 `{launch['template_sha256']}`; final manifest sha256 `{launch['manifest_sha256']}`.",
        f"- Launch commit `{m['head']}`, a signed child of C that adds only the final manifest. Two independent approvers "
        "confirmed it after the run.",
        f"- Seeds, drawn by the maintainer after both approvals with `secrets.randbelow(2**64 - 299)` in one draw: "
        f"`eval_seed` {r['seeds']['eval_seed']}, `bootstrap_seed` {r['seeds']['bootstrap_seed']}.",
        f"- Authorized at {AUTHORIZED_AT}; attempt 1 started at {m['started_at']}, inside the one hour window, and finished "
        f"in {r['run_environment']['seconds']} s. One attempt, no restart, no exception record, no preflight failure.",
        f"- Results sha256 `{hashlib.sha256(raw).hexdigest()}`, committed byte for byte. The file stayed unopened until both "
        "approvers had confirmed the launch commit and the maintainer gave the go; that is a procedural commitment, which "
        "the records cannot prove. The stage log carries no outcome.",
        f"- market-maker-rs {r['market_maker_rs']['version']} at `{r['market_maker_rs']['git_sha']}`, binary sha256 "
        f"`{r['market_maker_rs']['binary_sha256']}`.",
        "",
        "## Earlier runs and the dry run",
        "",
        f"- Run 1 aborted before writing any result (`{r['previous_runs']['run_1']['record']}`). It keeps that outcome.",
        f"- Run 2 gave **{r['previous_runs']['run_2']['outcome'].upper()}** (`{r['previous_runs']['run_2']['results']}`). "
        "It is not reclassified: gate 6 failed there as preregistered, and this run does not change that.",
        "- A dry run of the whole pipeline ran once before C was fixed, on commit `05e2c9e`, with dummy seeds now forbidden "
        "(`eval_seed` 4659386883093802759, `bootstrap_seed` 2043307074730982498). Only its exit code (0), duration (191.8 s), "
        "peak memory and gate 6 equality were read; everything else was deleted unread. It checked the code, and with other "
        "seeds it gave no access to this run's paths. The preregistration lists how that commit differs from C.",
        "",
    ]


def decision(r: dict) -> list[str]:
    d = r["decision"]
    c = r["in_model_comparison"]
    out = ["## Decision", "", "| preregistered requirement | result |", "|---|---|"]
    out.append(f"| in_model: every seed's upper bound below 0 on the GBM test set | {'holds' if d['in_model_all_seeds_better'] else '**fails**'} |")
    for dy in DYNAMICS:
        out.append(f"| no veto under {dy}: no seed's lower bound above 0 | {'**veto**' if d['out_of_model_veto'][dy] else 'no veto'} |")
    out += [f"| gate `{k}` | {'holds' if v else '**fails**'} |" for k, v in d["gates"].items()]
    out += [
        "",
        f"Gate 6 compared {c['rows']} in-model rows and {c['shared_fields']} shared fields with the reference run, bit for "
        f"bit: {c['differing_fields']} differing, {c['missing_fields']} missing, {c['extra_fields']} extra, "
        f"{c['nonfinite_fields']} non-finite, {len(c['identity_errors'])} identity errors; the {c['metadata_fields']['new']} "
        "`weights_sha256` fields match the weight record. It depends only on `data_seed`.",
        "",
    ]
    return out


def hedging(r: dict) -> list[str]:
    primary = r["by_eps"][f"{r['config']['primary_eps']:g}"]
    out = [
        f"## Hedging at the primary half spread ({r['config']['primary_eps']:g})",
        "",
        "CVaR95(learned) − CVaR95(band), estimate and paired 95 % interval. Negative favours the learned hedge.",
        "",
        "| seed | in model (GBM) | Heston, sticky moneyness | Heston, sticky strike |",
        "|---|---|---|---|",
    ]
    columns = [primary[m]["learned"] for m in ("in_model", *(f"heston_{d}" for d in DYNAMICS))]
    for rows in zip(*columns, strict=True):
        out.append(f"| {rows[0]['seed']} | " + " | ".join(ci(x["delta_cvar95_vs_band"]) for x in rows) + " |")
    out += ["", "| control | in model (GBM) | Heston, sticky moneyness | Heston, sticky strike |", "|---|---|---|---|"]
    for name in ("delta", "band"):
        out.append(f"| CVaR95 {name} | " + " | ".join(f"{primary[m][name]['cvar95']:.4f}" for m in ("in_model", *(f"heston_{d}" for d in DYNAMICS))) + " |")
    subset = r["in_range_subset"]
    out += [
        "",
        "The criterion asks only that there is no veto out of model; here every Heston interval is also below 0. In model "
        f"the margin is small: estimates {min(x['delta_cvar95_vs_band']['estimate'] for x in primary['in_model']['learned']):+.4f} to "
        f"{max(x['delta_cvar95_vs_band']['estimate'] for x in primary['in_model']['learned']):+.4f} against a band CVaR95 of "
        f"{primary['in_model']['band']['cvar95']:.4f}, with the tightest upper bound at "
        f"{max(x['delta_cvar95_vs_band']['ci_high'] for x in primary['in_model']['learned']):+.4f}.",
        "",
        "## Gate 7: the paths that stay in range on every day",
        "",
        "| seed | " + " | ".join(f"{d} ({subset[d]['paths']} of {subset[d]['of']} paths)" for d in DYNAMICS) + " |",
        "|---|---|---|",
    ]
    for a, b in zip(subset[DYNAMICS[0]]["learned"], subset[DYNAMICS[1]]["learned"], strict=True):
        out.append(f"| {a['seed']} | {ci(a['delta_cvar95_vs_band'])} | {ci(b['delta_cvar95_vs_band'])} |")
    out += [
        "",
        f"No veto under either dynamics. Under sticky moneyness the subset is {pct(subset[DYNAMICS[0]]['paths'] / subset[DYNAMICS[0]]['of'])} "
        "of the paths and is selected on the whole path, future days included. It is a robustness gate, not a tradable "
        "condition, and its estimates are not effect sizes for the full population.",
        "",
    ]
    return out


def agreement(r: dict) -> list[str]:
    out = [
        "## Coverage and agreement",
        "",
        "Extrapolated is a label, never an exclusion: every row is priced and compared.",
        "",
        "| dynamics | rows | out of domain | extrapolated | max vega extrapolated | max price extrapolated |",
        "|---|---|---|---|---|---|",
    ]
    for dy in DYNAMICS:
        cov, cmp = r["coverage"][dy], r["comparison_b"][dy]
        out.append(
            f"| {dy} | {cov['rows']} | {pct(cov['out_of_domain_fraction'])} | {pct(cov['extrapolated_fraction'])} | "
            f"{cmp['max_vega_extrapolated']:.3e} | {cmp['max_price_extrapolated']:.3e} |"
        )
    by_day = r["coverage"]["sticky-moneyness"]["by_day"]
    days = sorted(by_day, key=int)[:19]
    n = r["config"]["n_test"]
    out += [
        "",
        "Extrapolated share by days to expiry under sticky moneyness (below 0.005 % from day 20 on):",
        "",
        "| days | " + " | ".join(days) + " |",
        "|---|" + "---|" * len(days),
        "| % | " + " | ".join(f"{100.0 * by_day[k]['extrapolated'] / n:.2f}" for k in days) + " |",
        "",
    ]
    for dy in DYNAMICS:
        cmp = r["comparison_b"][dy]
        out += [f"market-maker-rs against the reference on surface B, {dy} (violations must be 0):", ""]
        out += ["| panel | output | rows | violations | max abs | max ratio | at (days, path) |", "|---|---|---|---|---|---|---|"]
        for part in PANELS:
            for k in KEYS:
                c = cmp[part][k]
                at = c["max_ratio_at"]
                where = f"{at['days_to_expiry']}, {at['path']}" if at else ""
                out.append(f"| {part} | {k} | {c['rows']} | {c['violations']} | {c['max_abs']:.3e} | {c['max_ratio']:.3e} | {where} |")
        out.append("")
    return out


def diagnostic(r: dict) -> list[str]:
    a = r["diagnostic_a"]["comparison"]
    changes = r["surface_a"]["changes_against_run_2"]
    params = sorted({k for c in changes for k in c.get("changed", {})})
    at_minus_one = [s["day"] for s in r["surface_a"]["calibration"]["slices"] if s["params"]["rho"] == -1.0]
    worst = max(a[d][p][k]["max_ratio"] for d in DYNAMICS for p in PANELS for k in KEYS) if a["representable"] else None
    bad = sum(a[d]["all"][k]["violations"] for d in DYNAMICS for k in KEYS) if a["representable"] else None
    out = [
        "## Diagnostic surface A (never in the decision)",
        "",
        f"Surface A sha256 `{r['surface_a']['file_sha256']}`. Rho is exactly -1.0 on days {at_minus_one[0]} to {at_minus_one[-1]}. "
        f"Against run 2's surface A, {len(changes)} slices differ, on days {', '.join(str(c['day']) for c in changes)}, in "
        f"{', '.join(params)} only; days 5, 6 and 16 were already exactly -1.0. Every other parameter is bit for bit the same.",
        "",
    ]
    if a["representable"]:
        out += [f"Every slice is now representable in market-maker-rs. Agreement with the reference: {bad} violations, largest ratio {worst:.3e}.", ""]
    else:
        out += [f"Not representable: {a['not_representable']}", ""]
    return out


def not_in_decision(r: dict, run2: dict) -> list[str]:
    rule = r["original_k_range_rule"]
    primary = r["by_eps"][f"{r['config']['primary_eps']:g}"]
    eps_rows = []
    for key, v in r["by_eps"].items():
        for m in ("in_model", *(f"heston_{d}" for d in DYNAMICS)):
            cs = [x["delta_cvar95_vs_band"] for x in v[m]["learned"]]
            means = [x["mean_pnl"] for x in v[m]["learned"]]
            eps_rows.append(
                f"| {key} | {m} | {v['band_w']:g} | {max(c['ci_high'] for c in cs):+.4f} | {min(means):.4f} to {max(means):.4f} | {v[m]['band']['mean_pnl']:.4f} |"
            )
    pos = sorted(k for k, v in r["by_eps"].items() if any(x["delta_cvar95_vs_band"]["ci_high"] >= 0 for m in ("in_model", *(f"heston_{d}" for d in DYNAMICS)) for x in v[m]["learned"]))
    run2_primary = run2["by_eps"][f"{r['config']['primary_eps']:g}"]
    span = []
    for d in DYNAMICS:
        now = [x["delta_cvar95_vs_band"]["estimate"] for x in primary[f"heston_{d}"]["learned"]]
        then = [x["delta_cvar95_vs_band"]["estimate"] for x in run2_primary[f"heston_{d}"]["learned"]]
        span.append(f"{d} {min(now):+.4f} to {max(now):+.4f} (run 2: {min(then):+.4f} to {max(then):+.4f})")
    return [
        "## Not in the decision",
        "",
        f"- **The original `k` rule gives {rule['outcome'].upper()}.** It excluded `k` outside `k_range` and gave REVISE above 1 %; "
        f"the extrapolated share is {pct(rule['extrapolated_fraction'][DYNAMICS[0]])} under sticky moneyness and "
        f"{pct(rule['extrapolated_fraction'][DYNAMICS[1]])} under sticky strike. The first preregistration replaced that rule "
        "with the extrapolation label and gate 7. It did so before any run, but after measuring coverage on the earlier "
        "Heston set and seeing that the original rule would give REVISE; no price had been compared. Run 3 was preregistered "
        "with the amended rule. **The outcome above rests on that amendment: under the original rule it would be REVISE.**",
        "",
        "Everything below is **exploratory**: reported for context, outside the preregistered decision, and not a test.",
        "",
        "- **The other half spreads.** The criterion reads only the primary one.",
        "",
        "| half spread | market | band w | largest upper bound of the interval | mean PnL learned | mean PnL band |",
        "|---|---|---|---|---|---|",
        *eps_rows,
        "",
        f"  An interval reaches 0 only at half spread {', '.join(pos)}, in model, on one seed." if pos else "  Every interval is below 0.",
        "  Mean PnL is not part of the criterion, which is about tail loss. At half spread 0.005, in model and under sticky "
        "strike, some seeds have a lower mean PnL than the band while their CVaR95 is better.",
        f"- **The two dynamics.** The learned rows are the same under both; only the baselines change (band CVaR95 "
        f"{primary[f'heston_{DYNAMICS[0]}']['band']['cvar95']:.4f} under sticky moneyness, "
        f"{primary[f'heston_{DYNAMICS[1]}']['band']['cvar95']:.4f} under sticky strike). The sticky strike column is therefore "
        "the more conservative comparison.",
        f"- **Against run 2's Heston set**, the estimates at the primary half spread are close: {'; '.join(span)}. "
        "These are two draws, not a replication test.",
        "",
    ]


def limitations(r: dict) -> list[str]:
    return [
        "## Limitations",
        "",
        "- **Synthetic only.** The paths and surface B come from one Heston parameter set, and the surface shares those "
        "parameters. No market data enters this result, and ADOPT is not a statement about live trading.",
        "- **The surface is static.** It is calibrated once at t = 0, and the two dynamics are modelling assumptions.",
        "- **Extrapolation.** Near expiry under sticky moneyness a large share of rows is outside the calibrated `k` range "
        "(the table above). There the prices are analytic SVI wings with no quote behind them. Agreement between the two "
        "implementations there is numerical consistency, not economic validation.",
        "- **The learned controls are frozen and were trained on GBM.** The evaluation covers one option, one hedge "
        "instrument and five seeds, and the band re-hedges to its centre.",
        "- **The no-look commitments are procedural.** The records show the binding, the single attempt and the hashes; they "
        "cannot show that nobody looked.",
        "",
        "## Reproducibility",
        "",
        f"python {r['run_environment']['python']}, numpy {r['run_environment']['numpy']}, torch {r['run_environment']['torch']}, "
        f"{r['run_environment']['machine']}, one thread everywhere, deterministic algorithms. The attempt's execution manifest, "
        "stage log (with the sha256 of each data array) and captured stdout are in "
        "`research/results/deep_hedging_mm_run3/attempt-1/`; its stderr was empty. A rerun with these seeds would reproduce "
        "the same paths and would not be a new evaluation.",
        "",
    ]


def main() -> None:
    raw = RUN3.read_bytes()
    r = json.loads(raw)
    criterion, outcome = recompute(r)
    if (criterion, outcome) != (r["decision"]["criterion_outcome"], r["decision"]["outcome"]):
        raise SystemExit(f"recomputed {criterion}/{outcome} is not the recorded {r['decision']}")
    m = json.loads((ATTEMPT / "execution_manifest.json").read_text())
    run2 = json.loads((RESULTS / "deep_hedging_mm.json").read_text())
    out = header(r, raw, m) + decision(r) + hedging(r) + agreement(r) + diagnostic(r) + not_in_decision(r, run2) + limitations(r)
    OUT.write_text("\n".join(out))


if __name__ == "__main__":
    main()
