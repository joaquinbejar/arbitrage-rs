"""#43: the M5-06b comparison with market-maker-rs as the counterpart.

Run from the repository root, with MM_COUNTERPART_BIN set to the binary
`experiments/mm_build.py` built (its sha256 must be the manifest's):

    experiments/.venv/bin/python experiments/deep_hedging_mm.py

It writes `research/results/deep_hedging_mm.json`. The protocol is
`research/preregistration/deep_hedging_mm.md`, whose sha256 is fixed below:
the run stops if the file differs. `--freeze-surface` computes surface B
and writes its quotes and file (`research/results/deep_hedging_mm_surface_b.
{csv,json}`) for review before the run; the run recomputes them and stops on
any difference.

The text below summarises the preregistration; the file is authoritative.

Fixed before the run, and only this changes against M5-06b:

- Data, splits, seeds and the learned controls are those of M5-06b: the
  same SeedSequence, and the frozen weights of
  `research/models/` checked against the sha256 recorded in
  `deep_hedging_counterpart.json`. Nothing is retrained.
- Surface B, which decides: market-maker-rs calibrates its own raw SVI
  slices (`counterpart calibrate`, #96) on the same Heston prices as the
  reference grid (one expiry a day, k = k_std sqrt(v0 tau), out-of-the-money
  side, prices below `min_price` dropped), each quoted with a relative half
  band `quote_band` around the price (a declared observation model, not a
  spread: the cleaning refuses a quote with no band). Calibrated once at
  t = 0, from those prices only, then frozen for the run.
- Counterparts: market-maker-rs on surface B under each dynamics gives the
  delta, the band baseline and the premium (sticky moneyness at t = 0), as
  the reference did in M5-06b. The reference prices on the same B slices
  (exported bit for bit) for the comparison table. Surface A (the slices
  Python fits, as in M5-06b) is a diagnostic only.
- Coverage, from the data alone before any pricing: a (path, day) row whose
  tau is outside surface B's slices is out of domain; a row whose k (under
  the dynamics) is outside the calibrated k range of its slices is
  extrapolated (SVI wings, the same in both implementations), counted and
  reported, never excluded.
- Comparison: every output, on every (path, day) row of the Heston test
  paths, |mm - ref| <= max(abs_tol, rel_tol |ref|) with the tolerances of
  `counterpart.AGREEMENT_TOLERANCES`, reported for in-range and
  extrapolated rows apart.

Decision (pre-registered): the M5-06b criterion on surface B with the
market-maker-rs counterpart (in_model at the primary half spread for every
seed; veto(D) when some seed's lower bound is above 0 under Heston with
dynamics D; ADOPT iff in_model and no veto under either dynamics, REJECT iff
not in_model or a veto under both, REVISE otherwise), and ADOPT only when
every gate holds: the conformance suite passes on this binary, surface B is
not failed, at most `max_out_of_domain` of the rows are out of domain, every
required row is priced (an error stops the run), and the in-range rows
agree. A failed gate turns ADOPT into REVISE.
"""

from __future__ import annotations

import io
import json
import math
import os
import platform
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import numpy as np
import torch

import calibration
import counterpart as cp
import heston
import svi
from deep_hedging import (
    CONFIG,
    ROOT,
    band_policy,
    cvar,
    delta_policy,
    evaluate,
    gbm_paths,
    heston_paths,
    learned_policy,
    market,
    paired_bootstrap,
    simulate,
)
from deep_hedging_counterpart import (
    COUNTERPART_CONFIG,
    cp_band_policy,
    cp_delta_policy,
    load_weights,
    sha256_json,
    strip_losses,
)

MM_CONFIG = {
    "quote_band": 1e-4,
    "cutoff_ms": 1_700_000_000_000,
    "rate": "0",
    "carry": "0",
    "cleaning": {"max_age_ms": 0, "max_spot_lag_ms": 0, "max_relative_spread": "0.001", "parity_tolerance": "0"},
    "k_grid": [repr(float(x)) for x in np.linspace(-0.6, 0.6, 121)],
    "short_expiry": "reject",
    "max_out_of_domain": 0.01,
    "tolerances": cp.AGREEMENT_TOLERANCES,
}

DAY_MS = 86_400_000
RESULTS = ROOT / "research" / "results"
OUT = RESULTS / "deep_hedging_mm.json"
REFERENCE_RUN = RESULTS / "deep_hedging_counterpart.json"
SURFACE_B_QUOTES = RESULTS / "deep_hedging_mm_surface_b.csv"
SURFACE_B_FILE = RESULTS / "deep_hedging_mm_surface_b.json"
PREREGISTRATION = ROOT / "research" / "preregistration" / "deep_hedging_mm.md"
PREREGISTRATION_SHA256 = "64346ef1a2734f3fddfe5ae1d7e78287ede63439abfc0c5c8240581eaaa404f9"
FAILED_RUNS = [RESULTS / "deep_hedging_mm_failed_run_1.json"]


# ---------------------------------------------------------------- surfaces


def heston_quotes(ccfg: dict, mcfg: dict) -> tuple[str, list[int]]:
    """The Heston prices of the reference grid as a quotes CSV for
    `counterpart calibrate`, and the days quoted."""
    p, spot = ccfg["heston"], ccfg["s_ref"]
    cutoff = mcfg["cutoff_ms"]
    band = mcfg["quote_band"]
    lines = [",".join(cp.MM_QUOTE_COLUMNS)]
    days = []
    for day in range(1, ccfg["days"] + 1):
        tau = day / ccfg["year_days"]
        strikes = spot * np.exp(np.array(ccfg["k_std"]) * math.sqrt(p["v0"] * tau))
        calls, change = heston.price_converged(spot, strikes, tau, p, ccfg["quad"])
        if change > ccfg["quad_tol"]:
            raise SystemExit(f"day {day}: quadrature change {change:.3e} above {ccfg['quad_tol']:.1e}")
        for k, c in zip(strikes, calls):
            put_side = k < spot
            otm = c - (spot - k) if put_side else c
            if otm < ccfg["min_price"]:
                continue
            row = [
                cp.plain_decimal(k),
                str(cutoff + day * DAY_MS),
                "put" if put_side else "call",
                cp.plain_decimal(otm * (1.0 - band)),
                cp.plain_decimal(otm * (1.0 + band)),
                str(cutoff),
                str(cutoff),
                cp.plain_decimal(spot),
                str(cutoff),
                str(cutoff),
            ]
            lines.append(",".join(row))
        days.append(day)
    return "\n".join(lines) + "\n", days


def calibrate_b(binary: Path, ccfg: dict, mcfg: dict) -> tuple[str, dict, str]:
    """Surface B: the market-maker-rs calibration of the Heston quotes.
    Returns the surface file, its parsed content and the quotes CSV."""
    quotes, _ = heston_quotes(ccfg, mcfg)
    config = {
        "cutoff": mcfg["cutoff_ms"],
        "available_at": mcfg["cutoff_ms"],
        "rate": mcfg["rate"],
        "carry": mcfg["carry"],
        "cleaning": mcfg["cleaning"],
        "k_grid": mcfg["k_grid"],
        "short_expiry": mcfg["short_expiry"],
    }
    with tempfile.TemporaryDirectory() as tmp:
        d = Path(tmp)
        (d / "quotes.csv").write_text(quotes)
        (d / "config.json").write_text(json.dumps(config))
        cp.mm_run(binary, ["calibrate", "--quotes", str(d / "quotes.csv"), "--config", str(d / "config.json"), "--out", str(d / "surface.json")])
        text = (d / "surface.json").read_text()
    return text, json.loads(text), quotes


def surface_a(ccfg: dict) -> tuple[str, dict]:
    """Surface A: the slices Python fits (M5-06b), with each slice's quoted
    k range, in market-maker-rs's format."""
    slices, cal = calibration.calibrate(ccfg)
    ranges = []
    for sl, rep in zip(slices, cal["slices"]):
        kept = [float(x) for x in rep["residuals_by_k_std"]]
        std = math.sqrt(ccfg["heston"]["v0"] * sl.tau)
        ranges.append((min(kept) * std, max(kept) * std))
    return cp.svi_surface_json(slices, ranges), cal


# A valid slice at one year, after every slice tested: market-maker-rs needs
# two slices for a surface, so each slice of A is loaded with this one.
CONTROL_SLICE = svi.SviSlice(tau=1.0, a=0.04, b=0.1, rho=0.0, m=0.0, sigma=0.1)


def representability(binary: Path, slices: list[svi.SviSlice], ranges: list) -> list[dict]:
    """Amendment 2: each slice loaded into market-maker-rs on its own (with
    the control slice), one price request at its own expiry. A slice the
    binary refuses is listed with its parameters and the error; no slice is
    altered."""
    out = []
    for sl, rng in zip(slices, ranges):
        text = cp.svi_surface_json([sl, CONTROL_SLICE], [rng, (-1.0, 1.0)])
        req = ",".join(cp.MM_REQUEST_COLUMNS) + "\n0,100.0," + cp.plain_decimal(sl.tau * cp.YEAR_DAYS) + ",call,european,100.0,0.0,0,0\n"
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "s.json").write_text(text)
            (d / "r.csv").write_text(req)
            try:
                cp.mm_run(binary, ["price", "--surface", str(d / "s.json"), "--dynamics", "sticky-moneyness", "--requests", str(d / "r.csv"), "--responses", str(d / "o.csv")])
            except cp.CounterpartUnavailable as e:
                # Amendment 2 catches only the known failure: rho outside
                # [-1, 1], refused by the binary for that reason. Anything
                # else aborts the run.
                if not (abs(sl.rho) > 1.0 and "svi slice: |rho| must be at most 1" in str(e)):
                    raise
                out.append(
                    {
                        "day": round(sl.tau * cp.YEAR_DAYS),
                        "reason": "rho outside [-1, 1]",
                        "params": {k: repr(v) for k, v in sl.params().items()},
                        "rho_bits": struct.pack(">d", sl.rho).hex(),
                        "k_range": [repr(x) for x in rng],
                        "error": str(e),
                    }
                )
    return out


# ---------------------------------------------------------------- coverage


def coverage(slices: list[svi.SviSlice], ranges: list, s: np.ndarray, cfg: dict, dynamics: str) -> dict:
    """Per day, the rows out of domain (tau outside the slices) and
    extrapolated (k outside the k range of the slices tau reads), from the
    paths alone. r = q = 0, so k = ln(K / S) or ln(K / s_ref)."""
    taus = np.array([sl.tau for sl in slices])
    out = {"rows": 0, "out_of_domain": 0, "extrapolated": 0, "by_day": {}}
    for t in range(cfg["steps"]):
        days_left = cfg["steps"] - t
        tau = days_left / cfg["days_per_year"]
        n = s.shape[0]
        out["rows"] += n
        tol = 1e-12 * taus[-1]
        if tau < taus[0] - tol or tau > taus[-1] + tol:
            out["out_of_domain"] += n
            out["by_day"][days_left] = {"out_of_domain": n, "extrapolated": 0}
            continue
        j = int(np.searchsorted(taus, tau - tol))
        used = {min(j, len(taus) - 1)}
        if abs(taus[min(j, len(taus) - 1)] - tau) > tol:
            used.add(max(j - 1, 0))
        elif j > 0:
            used.add(j - 1)  # theta on a node reads the shorter segment
        lo = max(ranges[i][0] for i in used)
        hi = min(ranges[i][1] for i in used)
        anchor = s[:, t] if dynamics == "sticky-moneyness" else np.full(n, cfg["s0"])
        k = np.log(cfg["strike"] / anchor)
        ext = int(np.sum((k < lo) | (k > hi)))
        out["extrapolated"] += ext
        out["by_day"][days_left] = {"out_of_domain": 0, "extrapolated": ext, "k_range": [lo, hi]}
    out["out_of_domain_fraction"] = out["out_of_domain"] / out["rows"]
    out["extrapolated_fraction"] = out["extrapolated"] / out["rows"]
    return out


# ---------------------------------------------------------------- comparison


def checked_outputs(side: str, out: dict, n: int, days_left: int) -> dict:
    """The five outputs of one side, each an array of `n` finite floats.
    Anything else fails the run: a non-finite value is never agreement."""
    keys = ("price", "delta", "gamma", "vega", "theta")
    checked = {}
    for key in keys:
        if key not in out:
            raise cp.NumericalFailure(f"{side}: no {key} at {days_left} days")
        a = np.asarray(out[key], dtype=float)
        if a.shape != (n,):
            raise cp.NumericalFailure(f"{side}: {key} has shape {a.shape}, expected ({n},) at {days_left} days")
        if not np.all(np.isfinite(a)):
            raise cp.NumericalFailure(f"{side}: {key} is not finite at {days_left} days (first at path {int(np.flatnonzero(~np.isfinite(a))[0])})")
        checked[key] = a
    return checked


def compare(mm, ref, contract, s: np.ndarray, cov: dict, cfg: dict) -> dict:
    """Every output on every (path, day) row: the largest differences and
    the rows outside the tolerance, for all rows, in range and extrapolated.
    Both sides are checked finite and of the right shape first."""
    keys = ("price", "delta", "gamma", "vega", "theta")
    parts = ("all", "in_range", "extrapolated")
    acc = {
        part: {k: {"max_abs": 0.0, "max_rel": 0.0, "violations": 0, "rows": 0, "max_ratio": 0.0, "max_ratio_at": None} for k in keys}
        for part in parts
    }
    max_vega_extrapolated = 0.0
    max_price_extrapolated = 0.0
    for t in range(cfg["steps"]):
        days_left = cfg["steps"] - t
        st = s[:, t]
        got = checked_outputs("market-maker-rs", mm.greeks(contract, st, float(t)), st.size, days_left)
        want = checked_outputs("reference", ref.greeks(contract, st, float(t)), st.size, days_left)
        lo, hi = cov["by_day"][days_left].get("k_range", (-math.inf, math.inf))
        anchor = st if mm.dynamics == "sticky-moneyness" else np.full(st.shape, cfg["s0"])
        k = np.log(cfg["strike"] / anchor)
        ext = (k < lo) | (k > hi)
        if np.any(ext):
            max_vega_extrapolated = max(max_vega_extrapolated, float(np.max(np.abs(want["vega"][ext]))))
            max_price_extrapolated = max(max_price_extrapolated, float(np.max(np.abs(want["price"][ext]))))
        for part, mask in (("all", np.ones_like(ext)), ("in_range", ~ext), ("extrapolated", ext)):
            if not np.any(mask):
                continue
            for key in keys:
                abs_tol, rel_tol = cp.AGREEMENT_TOLERANCES[key]
                a, b = got[key][mask], want[key][mask]
                diff = np.abs(a - b)
                rel = diff / np.maximum(np.abs(b), 1e-300)
                r = acc[part][key]
                r["max_abs"] = max(r["max_abs"], float(np.max(diff)))
                r["max_rel"] = max(r["max_rel"], float(np.max(np.where(np.abs(b) > 0, rel, 0.0))))
                bound = np.maximum(abs_tol, rel_tol * np.abs(b))
                # A non-finite value on either side is a violation: NaN
                # compares false, so the test is "not within", not "beyond".
                bad = ~(np.isfinite(a) & np.isfinite(b) & (diff <= bound))
                ratio = np.where(bad & ~np.isfinite(diff), np.inf, diff / bound)
                r["violations"] += int(np.sum(bad))
                r["rows"] += int(np.sum(mask))
                j = int(np.argmax(ratio))
                if float(ratio[j]) >= r["max_ratio"]:
                    path = int(np.flatnonzero(mask)[j])
                    r["max_ratio"] = float(ratio[j])
                    r["max_ratio_at"] = {"days_to_expiry": days_left, "path": path, "mm": float(a[j]), "ref": float(b[j])}
    acc["max_vega_extrapolated"] = max_vega_extrapolated
    acc["max_price_extrapolated"] = max_price_extrapolated
    acc["agrees"] = {part: all(acc[part][k]["violations"] == 0 for k in keys) for part in parts}
    return acc


# Fields the harness adds to a learned row that are records, not results:
# compared by no gate. A closed list: any other extra field fails gate 6.
METADATA_KEYS = frozenset({"weights_sha256"})


def _strict_equal(a, b, path: str, out: dict) -> None:
    """Records into `out` every difference between two JSON values: types
    must match exactly (True is not 1, 1 is not 1.0), floats must be finite
    and bitwise equal (the replay is deterministic, so -0.0 is not 0.0),
    dicts must have the same keys and lists the same length."""
    if type(a) is not type(b):
        out["differing"].append(f"{path}: type {type(a).__name__} != {type(b).__name__}")
    elif isinstance(a, float):
        if not (math.isfinite(a) and math.isfinite(b)):
            out["nonfinite"].append(path)
        elif struct.pack(">d", a) != struct.pack(">d", b):
            out["differing"].append(f"{path}: {a!r} != {b!r}")
    elif isinstance(a, dict):
        for k in sorted(set(a) - set(b)):
            out["extra"].append(f"{path}.{k}")
        for k in sorted(set(b) - set(a)):
            out["missing"].append(f"{path}.{k}")
        for k in sorted(set(a) & set(b)):
            _strict_equal(a[k], b[k], f"{path}.{k}", out)
    elif isinstance(a, list):
        if len(a) != len(b):
            out["differing"].append(f"{path}: length {len(a)} != {len(b)}")
        else:
            for i, (x, y) in enumerate(zip(a, b, strict=True)):
                _strict_equal(x, y, f"{path}[{i}]", out)
    elif a != b:
        out["differing"].append(f"{path}: {a!r} != {b!r}")


IN_MODEL_REQUIRED = ("delta", "band", "learned")


def _is_sha256(v) -> bool:
    return isinstance(v, str) and len(v) == 64 and all(c in "0123456789abcdef" for c in v)


def _learned_by_seed(side: str, eps: str, e, out: dict) -> dict | None:
    """The shape of one side's half spread entry, checked before any value
    is read. Every required field it lacks (band_w, in_model and, inside
    in_model, delta, band and learned) is reported as missing on that side;
    a value of the wrong type, an empty learned list, a row that is not an
    object, a row without a seed, a seed that is not an int and a repeated
    seed are identity errors. Returns the learned rows by seed, or None
    when they cannot be matched."""
    where = f"{side}:{eps}"
    if not isinstance(e, dict):
        out["identity"].append(f"{where}: entry is {type(e).__name__}, not an object")
        return None
    for need in ("band_w", "in_model"):
        if need not in e:
            out["missing"].append(f"{where}.{need}")
    if "in_model" not in e:
        return None
    im = e["in_model"]
    if not isinstance(im, dict):
        out["identity"].append(f"{where}.in_model is {type(im).__name__}, not an object")
        return None
    for need in IN_MODEL_REQUIRED:
        if need not in im:
            out["missing"].append(f"{where}.in_model.{need}")
    if "learned" not in im:
        return None
    rows = im["learned"]
    if not isinstance(rows, list):
        out["identity"].append(f"{where}.in_model.learned is {type(rows).__name__}, not a list")
        return None
    if not rows:
        out["identity"].append(f"{where}: no learned row")
        return None
    by_seed: dict = {}
    ok = True
    for i, r in enumerate(rows):
        at = f"{where}.in_model.learned[{i}]"
        if not isinstance(r, dict):
            out["identity"].append(f"{at} is {type(r).__name__}, not an object")
            ok = False
        elif "seed" not in r:
            out["missing"].append(f"{at}.seed")
            ok = False
        elif type(r["seed"]) is not int:
            out["identity"].append(f"{at}.seed is {type(r['seed']).__name__}, not an int")
            ok = False
        elif r["seed"] in by_seed:
            out["identity"].append(f"{where}: seed {r['seed']} repeated")
            ok = False
        else:
            by_seed[r["seed"]] = r
    return by_seed if ok else None


def _check_weights_sha256(side: str, eps: str, seed: int, row: dict, weights: dict, out: dict) -> None:
    """With the weight record given, a row's weights_sha256 must be a
    lowercase hex sha256 equal to the record's for "eps/seed", which must
    itself exist and be one. Every failure is an identity error."""
    where = f"{side}:{eps}.in_model.learned[seed {seed}].weights_sha256"
    rec = weights.get(f"{eps}/{seed}")
    want = rec.get("sha256") if isinstance(rec, dict) else None
    if "weights_sha256" not in row:
        out["identity"].append(f"{where} absent although the weight record is given")
    elif not _is_sha256(want):
        out["identity"].append(f"{where}: no valid weight record for {eps}/{seed}")
    elif not _is_sha256(row["weights_sha256"]):
        out["identity"].append(f"{where} {row['weights_sha256']!r} is not a sha256 digest")
    elif row["weights_sha256"] != want:
        out["identity"].append(f"{where} {row['weights_sha256']} is not {want}")


def compare_in_model(new: dict, old: dict, weights: dict | None = None) -> dict:
    """Gate 6: the in-model rows of a run (`by_eps`) against the reference
    run's, value by value. Never raises on malformed input: every problem
    is reported and makes the comparison unequal.
    - The half spreads must be the same non-empty set.
    - On each side, every entry must have band_w and in_model, and
      in_model must have delta, band and a non-empty list of learned rows,
      each an object with an int seed, unique on that side
      (`_learned_by_seed`). A required field absent on both sides is still
      missing, never equal.
    - band_w and every field of in_model are strictly equal (`_strict_equal`);
      a field on one side only is extra or missing.
    - The learned rows are matched by seed (the same set on both sides), not
      by position, and compared field by field leaving out only
      METADATA_KEYS, counted per side.
    - When `weights` (the reference run's record, by "eps/seed") is given,
      every new row must carry weights_sha256 and it must match the record;
      a reference row that carries one is checked the same way
      (`_check_weights_sha256`).
    Equal iff there is at least one row and no difference, no missing or
    extra field, no non-finite value and no identity error.
    The other fields of a half spread's entry (the out-of-model markets,
    such as heston_<dynamics>) are results of the run, not a reproduction,
    and are outside gate 6 by design."""
    out = {"differing": [], "missing": [], "extra": [], "nonfinite": [], "identity": []}
    rows = shared = 0
    metadata = {"new": 0, "old": 0}
    for side, v in (("new", new), ("old", old)):
        if not isinstance(v, dict):
            out["identity"].append(f"{side}: by_eps is {type(v).__name__}, not an object")
    if weights is not None and not isinstance(weights, dict):
        out["identity"].append(f"weights is {type(weights).__name__}, not an object")
        weights = {}
    new_eps = set(new) if isinstance(new, dict) else set()
    old_eps = set(old) if isinstance(old, dict) else set()
    if not new_eps or not old_eps:
        out["identity"].append("nothing to compare: no half spread on one side")
    if new_eps != old_eps:
        out["identity"].append(f"half spreads {sorted(new_eps, key=str)} != {sorted(old_eps, key=str)}")
    for eps in sorted(new_eps & old_eps, key=str):
        a, b = new[eps], old[eps]
        la = _learned_by_seed("new", eps, a, out)
        lb = _learned_by_seed("old", eps, b, out)
        if not (isinstance(a, dict) and isinstance(b, dict)):
            continue
        if "band_w" in a and "band_w" in b:
            _strict_equal(a["band_w"], b["band_w"], f"{eps}.band_w", out)
        ia, ib = a.get("in_model"), b.get("in_model")
        if isinstance(ia, dict) and isinstance(ib, dict):
            # Every in_model field is compared; only the learned rows are
            # matched by seed. A field added on either side is extra or
            # missing.
            for k in sorted(set(ia) - set(ib), key=str):
                out["extra"].append(f"{eps}.in_model.{k}")
            for k in sorted(set(ib) - set(ia), key=str):
                out["missing"].append(f"{eps}.in_model.{k}")
            for k in sorted((set(ia) & set(ib)) - {"learned"}, key=str):
                _strict_equal(ia[k], ib[k], f"{eps}.in_model.{k}", out)
        if la is None or lb is None:
            continue
        if set(la) != set(lb):
            out["identity"].append(f"{eps}: learned seeds {sorted(la)} against {sorted(lb)}")
            continue
        for seed in sorted(la):
            r, o = la[seed], lb[seed]
            rows += 1
            metadata["new"] += len(METADATA_KEYS & set(r))
            metadata["old"] += len(METADATA_KEYS & set(o))
            if weights is not None:
                _check_weights_sha256("new", eps, seed, r, weights, out)
                if "weights_sha256" in o:
                    _check_weights_sha256("old", eps, seed, o, weights, out)
            ra = {k: v for k, v in r.items() if k not in METADATA_KEYS}
            rb = {k: v for k, v in o.items() if k not in METADATA_KEYS}
            shared += len(set(ra) & set(rb))
            _strict_equal(ra, rb, f"{eps}.in_model.learned[seed {seed}]", out)
    report = {
        "rows": rows,
        "shared_fields": shared,
        "metadata_fields": metadata,
        "differing_fields": len(out["differing"]),
        "missing_fields": len(out["missing"]),
        "extra_fields": len(out["extra"]),
        "nonfinite_fields": len(out["nonfinite"]),
        "identity_errors": out["identity"],
        "examples": {k: v[:20] for k, v in out.items() if k != "identity"},
    }
    report["equal"] = rows > 0 and not any(out.values())
    return report


def in_range_paths(cov: dict, s: np.ndarray, cfg: dict, dynamics: str) -> np.ndarray:
    """The paths whose every (path, day) row is in range under `dynamics`."""
    keep = np.ones(s.shape[0], dtype=bool)
    for t in range(cfg["steps"]):
        lo, hi = cov["by_day"][cfg["steps"] - t].get("k_range", (-math.inf, math.inf))
        anchor = s[:, t] if dynamics == "sticky-moneyness" else np.full(s.shape[0], cfg["s0"])
        k = np.log(cfg["strike"] / anchor)
        keep &= (k >= lo) & (k <= hi)
    return keep


def conformance(binary: Path) -> dict:
    """The conformance suite on this binary, as a subprocess."""
    env = os.environ | {cp.MM_BINARY_ENV: str(binary)}
    out = subprocess.run(
        [sys.executable, "-W", "error::RuntimeWarning", "-m", "unittest", "test_counterpart.MarketMakerRsConformance", "test_counterpart.MarketMakerRsAdapter", "-v"],
        cwd=Path(__file__).resolve().parent,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    tail = out.stderr.strip().splitlines()[-3:]
    return {"passed": out.returncode == 0 and "skipped" not in out.stderr.split("Ran")[-1], "summary": tail}


# ---------------------------------------------------------------- main


def main() -> None:
    cfg, ccfg, mcfg = CONFIG, COUNTERPART_CONFIG, MM_CONFIG
    torch.set_num_threads(1)
    torch.use_deterministic_algorithms(True)
    started = time.time()
    binary = Path(os.environ.get(cp.MM_BINARY_ENV, ""))
    identity = cp.mm_identity(binary)
    if "--freeze-surface" in sys.argv[1:]:
        text, b, quotes = calibrate_b(binary, ccfg, mcfg)
        SURFACE_B_QUOTES.write_text(quotes)
        SURFACE_B_FILE.write_text(text)
        print(f"surface B {b['status']}: {SURFACE_B_QUOTES.name}, {SURFACE_B_FILE.name}")
        return
    prereg_sha = cp.sha256_file(PREREGISTRATION)
    if prereg_sha != PREREGISTRATION_SHA256:
        raise SystemExit(f"{PREREGISTRATION} sha256 {prereg_sha} is not the fixed {PREREGISTRATION_SHA256}")
    reference_run = json.loads(REFERENCE_RUN.read_text())

    seq = np.random.SeedSequence(cfg["data_seed"])
    gens = [np.random.default_rng(s) for s in seq.spawn(7)]
    val = market(gbm_paths(cfg["n_val"], gens[1], cfg), gens[2], cfg)
    test = market(gbm_paths(cfg["n_test"], gens[3], cfg), gens[4], cfg)
    heston_mkt = market(heston_paths(cfg["n_test"], gens[5], cfg), gens[6], cfg)

    conf = conformance(binary)
    b_text, b, quotes = calibrate_b(binary, ccfg, mcfg)
    # The surface file names the binary that wrote it; everything else must
    # equal the committed file, and the quotes byte for byte.
    committed = json.loads(SURFACE_B_FILE.read_text())
    frozen = SURFACE_B_QUOTES.read_text() == quotes and {k: v for k, v in committed.items() if k != "identity"} == {
        k: v for k, v in b.items() if k != "identity"
    }
    if not frozen:
        raise SystemExit("surface B or its quotes differ from the committed files")
    a_text, a_cal = surface_a(ccfg)
    print(f"surfaces: B {b['status']}, conformance {'pass' if conf['passed'] else 'FAIL'}, {time.time() - started:.0f} s", flush=True)
    if not b["slices"]:
        raise SystemExit(f"surface B failed: {b['reasons']}")
    b_slices, b_ranges = cp.slices_from_surface_json(b_text)
    a_slices, a_ranges = cp.slices_from_surface_json(a_text)
    contract = cp.Contract(strike=cfg["strike"], expiry_days=float(cfg["steps"]), kind="call")

    cov = {d: coverage(b_slices, b_ranges, heston_mkt["s"], cfg, d) for d in cp.DYNAMICS}
    mm = {d: cp.MarketMakerRsCounterpart(b_text, d, s_ref=cfg["s0"]) for d in cp.DYNAMICS}
    ref_b = {d: cp.ReferenceCounterpart(cp.SviSurface(b_slices), d, s_ref=cfg["s0"]) for d in cp.DYNAMICS}
    comparison = {d: compare(mm[d], ref_b[d], contract, heston_mkt["s"], cov[d], cfg) for d in cp.DYNAMICS}
    cov_a = {d: coverage(a_slices, a_ranges, heston_mkt["s"], cfg, d) for d in cp.DYNAMICS}
    not_representable = representability(binary, a_slices, a_ranges)
    if not_representable:
        diagnostic_a = {"representable": False, "not_representable": not_representable}
    else:
        mm_a = {d: cp.MarketMakerRsCounterpart(a_text, d, s_ref=cfg["s0"]) for d in cp.DYNAMICS}
        ref_a = {d: cp.ReferenceCounterpart(cp.SviSurface(a_slices), d, s_ref=cfg["s0"]) for d in cp.DYNAMICS}
        diagnostic_a = {"representable": True} | {
            d: compare(mm_a[d], ref_a[d], contract, heston_mkt["s"], cov_a[d], cfg) for d in cp.DYNAMICS
        }
    print(f"comparison done, {time.time() - started:.0f} s", flush=True)

    premium = float(mm["sticky-moneyness"].price(contract, np.array([cfg["s0"]]), 0.0)[0])
    premium_ref_b = float(ref_b["sticky-moneyness"].price(contract, np.array([cfg["s0"]]), 0.0)[0])
    results = {
        "issue": "arbitrage-rs#43",
        "preregistration_sha256": prereg_sha,
        "failed_runs": [json.loads(p.read_text()) | {"record_sha256": cp.sha256_file(p)} for p in FAILED_RUNS],
        "config": cfg,
        "config_sha256": sha256_json(cfg),
        "counterpart_config": ccfg,
        "counterpart_config_sha256": sha256_json(ccfg),
        "mm_config": mcfg,
        "mm_config_sha256": sha256_json(mcfg),
        "market_maker_rs": identity,
        "manifest": json.loads(cp.MM_MANIFEST.read_text()),
        "conformance": conf,
        "surface_b": {
            "status": b["status"],
            "reasons": b["reasons"],
            "params": b["params"],
            "file_sha256": cp.hashlib.sha256(b_text.encode()).hexdigest(),
            "quotes_sha256": cp.hashlib.sha256(quotes.encode()).hexdigest(),
            "report": b["report"],
            "data_cutoff": b["data_cutoff"],
            "available_at": b["available_at"],
        },
        "surface_a": {"file_sha256": cp.hashlib.sha256(a_text.encode()).hexdigest(), "calibration": a_cal},
        "coverage": cov,
        "comparison_b": comparison,
        "diagnostic_a": {"coverage": cov_a, "comparison": diagnostic_a},
        "counterparts": {f"heston_{d}": c.identity() for d, c in mm.items()},
        "premium": {"mm_b": premium, "reference_b": premium_ref_b, "reference_m5_06b": reference_run["premium"]["heston_svi"]},
        "by_eps": {},
    }

    weights = reference_run["weights"]
    for eps in cfg["eps_list"]:
        key = f"{eps:g}"
        val_cvar = {w: cvar(-simulate(band_policy(w, cfg), val, eps, cfg)[0], cfg["alpha"]) for w in cfg["band_grid"]}
        best_w = min(val_cvar, key=lambda w: (val_cvar[w], w))
        entry = {"band_w": best_w}
        d_, _ = evaluate(delta_policy(cfg), test, eps, cfg)
        b_, b_loss = evaluate(band_policy(best_w, cfg), test, eps, cfg)
        entry["in_model"] = {"delta": d_, "band": b_, "learned": [], "_band_loss": b_loss}
        for dyn, c in mm.items():
            d_, _ = evaluate(cp_delta_policy(c, contract), heston_mkt, eps, cfg, premium=premium)
            b_, b_loss = evaluate(cp_band_policy(best_w, c, contract), heston_mkt, eps, cfg, premium=premium)
            entry[f"heston_{dyn}"] = {"delta": d_, "band": b_, "learned": [], "_band_loss": b_loss}
        for i, seed in enumerate(cfg["train_seeds"]):
            rec = weights[f"{key}/{seed}"]
            net = load_weights(ROOT / rec["file"], rec["sha256"], cfg)
            markets = [("in_model", test, None)] + [(f"heston_{d}", heston_mkt, premium) for d in mm]
            for mname, mkt, prem in markets:
                lr, (l_loss, l_ideal) = evaluate(learned_policy(net, cfg), mkt, eps, cfg, premium=prem)
                b_loss, b_ideal = entry[mname]["_band_loss"]
                lr["seed"] = seed
                lr["weights_sha256"] = rec["sha256"]
                lr["delta_cvar95_vs_band"] = paired_bootstrap(l_loss, b_loss, 0.95, cfg, seed_offset=i)
                lr["delta_cvar95_vs_band_ideal_execution"] = paired_bootstrap(l_ideal, b_ideal, 0.95, cfg, seed_offset=100 + i)
                entry[mname]["learned"].append(lr)
        for mname in list(entry):
            if isinstance(entry[mname], dict):
                entry[mname] = strip_losses(entry[mname])
        results["by_eps"][key] = entry
        print(f"eps {key}: band w {best_w:g}, {time.time() - started:.0f} s", flush=True)

    # Compared as written: the JSON round trip turns numpy scalars into the
    # JSON types the reference run was read with.
    results["in_model_comparison"] = compare_in_model(
        json.loads(json.dumps(results["by_eps"])), reference_run["by_eps"], weights=reference_run["weights"]
    )
    results["in_model_matches_m5_06b"] = results["in_model_comparison"]["equal"]
    # The conclusion on the in-range paths alone (gate 7), with the same
    # paired bootstrap at the primary half spread.
    eps = cfg["primary_eps"]
    in_range_subset = {}
    for dyn, c in mm.items():
        keep = in_range_paths(cov[dyn], heston_mkt["s"], cfg, dyn)
        _, (b_loss, _) = evaluate(cp_band_policy(results["by_eps"][f"{eps:g}"]["band_w"], c, contract), heston_mkt, eps, cfg, premium=premium)
        rows = []
        for i, seed in enumerate(cfg["train_seeds"]):
            rec = weights[f"{eps:g}/{seed}"]
            net = load_weights(ROOT / rec["file"], rec["sha256"], cfg)
            _, (l_loss, _) = evaluate(learned_policy(net, cfg), heston_mkt, eps, cfg, premium=premium)
            ci = paired_bootstrap(l_loss[keep], b_loss[keep], 0.95, cfg, seed_offset=200 + i)
            rows.append({"seed": seed, "delta_cvar95_vs_band": ci})
        in_range_subset[dyn] = {
            "paths": int(np.sum(keep)),
            "of": int(keep.size),
            "learned": rows,
            "veto": any(r["delta_cvar95_vs_band"]["ci_low"] > 0 for r in rows),
        }
    results["in_range_subset"] = in_range_subset
    results["original_k_range_rule"] = {
        "rule": "exclude k outside k_range, REVISE above 1 %",
        "extrapolated_fraction": {d: cov[d]["extrapolated_fraction"] for d in mm},
        "outcome": "revise" if any(cov[d]["extrapolated_fraction"] > 0.01 for d in mm) else "no exclusion above 1 %",
    }

    primary = results["by_eps"][f"{cfg['primary_eps']:g}"]
    in_model = all(l["delta_cvar95_vs_band"]["ci_high"] < 0 for l in primary["in_model"]["learned"])
    veto = {d: any(l["delta_cvar95_vs_band"]["ci_low"] > 0 for l in primary[f"heston_{d}"]["learned"]) for d in mm}
    if not in_model or all(veto.values()):
        criterion = "reject"
    elif any(veto.values()):
        criterion = "revise"
    else:
        criterion = "adopt"
    gates = {
        "1_conformance_passed": conf["passed"],
        "2_surface_b_not_failed_and_frozen": b["status"] != "failed" and frozen,
        "3_out_of_domain_within_bound": all(cov[d]["out_of_domain_fraction"] <= mcfg["max_out_of_domain"] for d in mm),
        "4_every_required_row_priced": True,
        "5_all_panels_agree": all(all(comparison[d]["agrees"].values()) for d in mm),
        "6_in_model_matches_m5_06b": results["in_model_matches_m5_06b"],
        "7_in_range_subset_no_veto": not any(v["veto"] for v in in_range_subset.values()),
    }
    outcome = criterion if criterion != "adopt" or all(gates.values()) else "revise"
    results["decision"] = {
        "criterion": "M5-06b's, on surface B with the market-maker-rs counterpart; ADOPT also requires every gate",
        "in_model_all_seeds_better": in_model,
        "out_of_model_veto": veto,
        "criterion_outcome": criterion,
        "gates": gates,
        "outcome": outcome,
        "counterpart_validation": "market-maker-rs",
    }
    results["run"] = {
        "seconds": round(time.time() - started, 1),
        "python": platform.python_version(),
        "numpy": np.__version__,
        "torch": torch.__version__,
        "machine": platform.machine(),
        "git_commit": subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True, check=False).stdout.strip(),
    }
    OUT.write_text(json.dumps(results, indent=2, sort_keys=True) + "\n")
    print(json.dumps(results["decision"], indent=2))
    print(f"done in {results['run']['seconds']} s", file=sys.stderr)


if __name__ == "__main__":
    main()
