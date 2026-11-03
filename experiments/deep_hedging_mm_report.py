"""Render `research/results/deep_hedging_mm.md` from the run's JSON.

    experiments/.venv/bin/python experiments/deep_hedging_mm_report.py

Formatting only: every number comes from `deep_hedging_mm.json`.
"""

from __future__ import annotations

import json

from deep_hedging import ROOT

RESULTS = ROOT / "research" / "results"
KEYS = ("price", "delta", "gamma", "vega", "theta")


def pct(x: float) -> str:
    return f"{100.0 * x:.2f} %"


def ci(c: dict) -> str:
    return f"{c['estimate']:+.4f} [{c['ci_low']:+.4f}, {c['ci_high']:+.4f}]"


def main() -> None:
    r = json.loads((RESULTS / "deep_hedging_mm.json").read_text())
    d = r["decision"]
    dyns = ("sticky-moneyness", "sticky-strike")
    out = ["# Deep hedging against the market-maker-rs counterpart (#43)", ""]
    out += [
        f"Outcome: **{d['outcome'].upper()}** (criterion {d['criterion_outcome']}).",
        "",
        "The counterpart's consistency (the comparison) and the hedging result are separate questions; "
        "the second rests on the first. Outside the calibrated `k` range the SVI wings are an analytic "
        "extrapolation, the same in both implementations, and no quote supports them.",
        "",
        f"- Preregistration sha256 `{r['preregistration_sha256']}`",
        f"- market-maker-rs {r['market_maker_rs']['version']} at `{r['market_maker_rs']['git_sha']}`, binary sha256 "
        f"`{r['market_maker_rs']['binary_sha256']}`",
        f"- Surface B: {r['surface_b']['status']}, file sha256 `{r['surface_b']['file_sha256']}`, quotes sha256 "
        f"`{r['surface_b']['quotes_sha256']}`",
        "",
        "## Earlier runs",
        "",
    ]
    for f in r["failed_runs"]:
        out += [
            f"- Run {f['run']} ({f['date']}, head `{f['head']}`, preregistration `{f['preregistration_sha256']}`): "
            f"{f['outcome']}. Error: {f['error']}. Log `{f['log']}`, sha256 `{f['log_sha256']}`. Results seen: "
            f"{f['results_seen']}",
            "  Slices outside the SVI domain: "
            + ", ".join(f"day {x['day']} rho {x['rho']} (bits {x['rho_bits']})" for x in f["slices_outside_the_svi_domain"]),
        ]
    a = r["diagnostic_a"]["comparison"]
    out += [
        "",
        "## Diagnostic surface A (the M5-06b Python fit)",
        "",
        "Representable in market-maker-rs: " + ("yes; compared below in the JSON." if a["representable"] else "**no**. Not compared (amendment 2)."),
    ]
    if not a["representable"]:
        out += ["", "| day | reason | rho | rho bits |", "|---|---|---|---|"]
        out += [f"| {x['day']} | {x['reason']} | {x['params']['rho']} | {x['rho_bits']} |" for x in a["not_representable"]]
    out += [
        "",
        "## Gates",
        "",
        "| gate | holds |",
        "|---|---|",
    ]
    out += [f"| {k} | {'yes' if v else '**no**'} |" for k, v in d["gates"].items()]
    out += [
        "",
        f"The original rule (exclude `k` outside `k_range`, REVISE above 1 %) gives "
        f"**{r['original_k_range_rule']['outcome']}**: extrapolated share "
        + ", ".join(f"{dy} {pct(v)}" for dy, v in r["original_k_range_rule"]["extrapolated_fraction"].items())
        + ".",
        "",
        "## Coverage and agreement",
        "",
        "| dynamics | rows | out of domain | extrapolated | max vega extrapolated | max price extrapolated |",
        "|---|---|---|---|---|---|",
    ]
    for dy in dyns:
        cov, cmp = r["coverage"][dy], r["comparison_b"][dy]
        out.append(
            f"| {dy} | {cov['rows']} | {pct(cov['out_of_domain_fraction'])} | {pct(cov['extrapolated_fraction'])} | "
            f"{cmp['max_vega_extrapolated']:.3e} | {cmp['max_price_extrapolated']:.3e} |"
        )
    out += ["", "Extrapolated share by days to expiry (sticky moneyness):", ""]
    by_day = r["coverage"]["sticky-moneyness"]["by_day"]
    n = r["config"]["n_test"]
    days = sorted(by_day, key=int)
    out += ["| days | " + " | ".join(days) + " |", "|---|" + "---|" * len(days)]
    out += ["| % | " + " | ".join(f"{100.0 * by_day[k]['extrapolated'] / n:.2f}" for k in days) + " |", ""]
    for dy in dyns:
        cmp = r["comparison_b"][dy]
        out += [f"Market-maker-rs against the reference on surface B, {dy} (violations must be 0):", ""]
        out += ["| panel | output | rows | violations | max abs | max ratio | at (days, path) |", "|---|---|---|---|---|---|---|"]
        for part in ("all", "in_range", "extrapolated"):
            for k in KEYS:
                c = cmp[part][k]
                at = c["max_ratio_at"]
                where = f"{at['days_to_expiry']}, {at['path']}" if at else ""
                out.append(f"| {part} | {k} | {c['rows']} | {c['violations']} | {c['max_abs']:.3e} | {c['max_ratio']:.3e} | {where} |")
        out.append("")
    out += ["## Hedging (primary half spread)", ""]
    primary = r["by_eps"][f"{r['config']['primary_eps']:g}"]
    out += ["| market | seed | CVaR95(learned) - CVaR95(band), 95 % CI |", "|---|---|---|"]
    for mname in ("in_model", *(f"heston_{dy}" for dy in dyns)):
        for lr in primary[mname]["learned"]:
            out.append(f"| {mname} | {lr['seed']} | {ci(lr['delta_cvar95_vs_band'])} |")
    out += ["", "On the paths that stay in range every day (gate 7):", ""]
    out += ["| dynamics | paths | seed | CI | veto |", "|---|---|---|---|---|"]
    for dy, sub in r["in_range_subset"].items():
        for row in sub["learned"]:
            out.append(f"| {dy} | {sub['paths']} of {sub['of']} | {row['seed']} | {ci(row['delta_cvar95_vs_band'])} | {sub['veto']} |")
    out += [
        "",
        f"Premium at t = 0 (sticky moneyness): market-maker-rs on B {r['premium']['mm_b']:.10f}, reference on B "
        f"{r['premium']['reference_b']:.10f}, reference surface of M5-06b {r['premium']['reference_m5_06b']:.10f}.",
        "",
        f"In-model rows equal M5-06b: {r['in_model_matches_m5_06b']}. Run: {r['run']['seconds']} s, "
        f"python {r['run']['python']}, numpy {r['run']['numpy']}, torch {r['run']['torch']}, {r['run']['machine']}, "
        f"commit `{r['run']['git_commit']}`.",
        "",
    ]
    out += post_run_gate6(r)
    (RESULTS / "deep_hedging_mm.md").write_text("\n".join(out))


def post_run_gate6(r: dict) -> list[str]:
    """A diagnostic written after the run, which changes no gate: gate 6
    compared this run's in-model rows with M5-06b's as whole dicts, and
    each learned row here carries an extra `weights_sha256` key. Compare
    the values with that key left out."""
    old = json.loads((RESULTS / "deep_hedging_counterpart.json").read_text())
    same, rows = True, 0
    for k, entry in r["by_eps"].items():
        o = old["by_eps"][k]
        same &= entry["band_w"] == o["band_w"]
        same &= entry["in_model"]["delta"] == o["in_model"]["delta"] and entry["in_model"]["band"] == o["in_model"]["band"]
        for a, b in zip(entry["in_model"]["learned"], o["in_model"]["learned"], strict=True):
            same &= {x: v for x, v in a.items() if x != "weights_sha256"} == b
            rows += 1
    return [
        "## After the run: gate 6",
        "",
        "Gate 6 failed, and this run's outcome is REVISE as preregistered. Its cause was found after the run and "
        "changes neither the gate nor the outcome: each learned row of this run carries an extra `weights_sha256` "
        "key, so the comparison of whole rows with M5-06b's fails.",
        "",
        f"With that key left out, every in-model value equals M5-06b's: band width, delta, band and {rows} learned "
        f"rows ({'yes' if same else '**no**'}).",
        "",
    ]


if __name__ == "__main__":
    main()
