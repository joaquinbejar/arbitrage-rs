"""Dataset interface for the empirical deep hedging test (M5-06c, #37).

A dataset is a directory with three files:

- `manifest.json`: what the data is and how to read it (`MANIFEST_FIELDS`);
- `paths.csv`: the book of the hedge instrument, one row per (path, step)
  for steps 0 to `steps` (the last row is the expiry, used for the terminal
  close and the payoff): `path_id, step, timestamp, bid, ask, bid_size,
  ask_size`, sizes in whole lots;
- `execution.csv`: what the queue model says an order sent at a decision
  step could get, one row per (path, step) for steps 0 to `steps - 1`:
  `path_id, step, fillable_buy, fillable_sell, reject`, in whole lots and
  0 | 1.

The execution row of step t applies only to the order sent after the
decision at t. It is never an input of a control. A fixed `fillable` is an
exogenous capacity per side for orders compatible with the declared queue
model; by itself it does not model size-dependent queues, latency or
impact, and the manifest must declare those limits.

`validate` lists every violation without changing anything; `load` refuses
an invalid dataset. Parsing is strict: integers are plain decimal digits
(no `1.0`, no `True`), numbers are finite decimals (no `nan`, no `inf`).
The manifest's `rounding` must be `"floor-lots"` (orders floored to whole
lots, prices on the tick), the only contract the runner implements.
Timestamps are integers in `timestamp_unit` (only `"s"`, seconds, is
supported), and consecutive steps of a path are exactly `step_days` days
apart. Prices are exact multiples of `tick`, checked in exact rational arithmetic
on the CSV text (`Fraction`), not in binary floating point and with no
precision limit.
"""

from __future__ import annotations

import csv
import hashlib
import json
import math
import re
from dataclasses import dataclass
from fractions import Fraction
from pathlib import Path

import numpy as np

PATHS_COLUMNS = ["path_id", "step", "timestamp", "bid", "ask", "bid_size", "ask_size"]
EXECUTION_COLUMNS = ["path_id", "step", "fillable_buy", "fillable_sell", "reject"]

# field: (type, required). "number" is an int or a float, never a bool.
MANIFEST_FIELDS = {
    "name": ("str", True),
    "version": ("str", True),
    "source": ("str", True),
    "synthetic": ("bool", True),
    "seed": ("int_or_null", True),
    "files": ("dict", True),
    "currency": ("str", True),
    "tick": ("positive_number", True),
    "lot": ("positive_number", True),
    "fee_bps": ("non_negative_number", True),
    "fee_asset": ("str", True),
    "rounding": ("str", True),
    "contract": ("dict", True),
    "steps": ("positive_int", True),
    "step_days": ("positive_number", True),
    "timestamp_unit": ("str", True),
    "queue_model": ("dict", True),
}
FILES = ("paths.csv", "execution.csv")
# The only rounding contract the runner implements: orders floored to whole
# lots, prices on the tick.
ROUNDING = "floor-lots"
# Lot counts are stored as int64; decimal exponents beyond this bound are
# refused before any exact arithmetic, so the Fractions stay small.
MAX_LOTS = 2**63 - 1
MAX_EXPONENT = 400
MAX_STEPS = 100_000

_INT = re.compile(r"^(0|[1-9][0-9]*)$")
_NUMBER = re.compile(r"^-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][-+]?[0-9]+)?$")


class DatasetInvalid(Exception):
    def __init__(self, violations: list[str]):
        super().__init__(f"{len(violations)} violation(s): " + "; ".join(violations[:10]))
        self.violations = violations


@dataclass(frozen=True)
class Dataset:
    """A validated dataset as arrays of shape (paths, steps + 1) for the book
    and (paths, steps) for the execution capacities."""

    manifest: dict
    bid: np.ndarray
    ask: np.ndarray
    bid_size: np.ndarray
    ask_size: np.ndarray
    fillable_buy: np.ndarray
    fillable_sell: np.ndarray
    reject: np.ndarray
    sha256: dict

    @property
    def mid(self) -> np.ndarray:
        """bid + (ask - bid) / 2: finite whenever bid and ask are, unlike
        (bid + ask) / 2, which overflows near the float maximum."""
        return self.bid + 0.5 * (self.ask - self.bid)


def sha256_file(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _show(value, limit: int = 40) -> str:
    """A violation echoes at most `limit` characters of the offending value."""
    text = value if isinstance(value, str) else repr(value)
    return repr(text) if len(text) <= limit else repr(text[:limit]) + f"... ({len(text)} characters)"


def _finite(value) -> bool:
    """A JSON number usable as a float: an integer beyond the float range
    (10**400) is not, and must be a violation rather than an OverflowError."""
    try:
        return math.isfinite(float(value))
    except OverflowError:
        return False


def _check_type(value, kind: str) -> bool:
    is_num = isinstance(value, (int, float)) and not isinstance(value, bool)
    if kind == "str":
        return isinstance(value, str) and value != ""
    if kind == "bool":
        return isinstance(value, bool)
    if kind == "dict":
        return isinstance(value, dict)
    if kind == "int_or_null":
        return value is None or (isinstance(value, int) and not isinstance(value, bool))
    if kind == "positive_int":
        return isinstance(value, int) and not isinstance(value, bool) and value > 0
    if kind == "positive_number":
        return is_num and _finite(value) and value > 0
    if kind == "non_negative_number":
        return is_num and _finite(value) and value >= 0
    raise ValueError(kind)


def _manifest_violations(m) -> list[str]:
    out = []
    if not isinstance(m, dict):
        return ["manifest: not a JSON object"]
    for field, (kind, required) in MANIFEST_FIELDS.items():
        if field not in m:
            if required:
                out.append(f"manifest: missing {field}")
            continue
        if not _check_type(m[field], kind):
            out.append(f"manifest: {field} must be {kind}, got {_show(m[field])}")
    for field in m:
        if field not in MANIFEST_FIELDS:
            out.append(f"manifest: unknown field {field}")
    if m.get("synthetic") is True and not (isinstance(m.get("seed"), int) and not isinstance(m.get("seed"), bool)):
        out.append("manifest: a synthetic dataset must declare its integer seed")
    files = m.get("files")
    if isinstance(files, dict):
        for f in FILES:
            if not (isinstance(files.get(f), str) and re.fullmatch(r"[0-9a-f]{64}", files[f])):
                out.append(f"manifest: files.{f} must be a sha256 hex digest")
        for f in files:
            if f not in FILES:
                out.append(f"manifest: unknown file {f}")
    c = m.get("contract")
    if isinstance(c, dict):
        if c.get("kind") not in ("call", "put"):
            out.append("manifest: contract.kind must be call or put")
        if c.get("style") != "european":
            out.append("manifest: contract.style must be european")
        if not _check_type(c.get("expiry_days"), "positive_number"):
            out.append("manifest: contract.expiry_days must be a positive number")
        if c.get("strike") != "atm-at-step-0":
            out.append("manifest: contract.strike must be atm-at-step-0 (the only mapping supported)")
    q = m.get("queue_model")
    if isinstance(q, dict):
        for f in ("id", "version"):
            if not _check_type(q.get(f), "str"):
                out.append(f"manifest: queue_model.{f} must be a non-empty string")
        lim = q.get("limits")
        if not (isinstance(lim, list) and lim and all(_check_type(x, "str") for x in lim)):
            out.append("manifest: queue_model.limits must be a non-empty list of strings")
    if m.get("rounding") not in (None, ROUNDING):
        out.append(f"manifest: rounding must be {ROUNDING} (orders floored to whole lots, prices on the tick)")
    if m.get("timestamp_unit") not in (None, "s"):
        out.append("manifest: timestamp_unit must be s (seconds)")
    if m.get("fee_asset") not in (None, "quote"):
        out.append("manifest: fee_asset must be quote")
    return out


def _read_csv(path: Path, columns: list[str], out: list[str]):
    rows = []
    with path.open(newline="") as f:
        reader = csv.reader(f)
        header = next(reader, None)
        if header != columns:
            out.append(f"{path.name}: header must be {','.join(columns)}, got {header}")
            return None
        for n, row in enumerate(reader, start=2):
            if len(row) != len(columns):
                out.append(f"{path.name}:{n}: expected {len(columns)} fields, got {len(row)}")
                continue
            rows.append((n, dict(zip(columns, row))))
    return rows


def _int(value: str, where: str, out: list[str], bound: int | None = None):
    if not _INT.match(value):
        out.append(f"{where}: {_show(value)} is not a non-negative integer")
        return None
    x = int(value)
    if bound is not None and x > bound:
        out.append(f"{where}: {value} is above the int64 range of lot counts")
        return None
    return x


def _num(value: str, where: str, out: list[str]):
    m = _NUMBER.match(value)
    if not m:
        out.append(f"{where}: {_show(value)} is not a finite decimal number")
        return None
    if m.group(3) and abs(int(m.group(3)[1:])) > MAX_EXPONENT:
        out.append(f"{where}: {_show(value)} has an exponent beyond {MAX_EXPONENT}")
        return None
    x = float(value)
    if not math.isfinite(x):
        out.append(f"{where}: {_show(value)} is not finite")
        return None
    return x


def _parse(directory: Path):
    """Parse and check everything; returns (manifest, arrays or None, violations, hashes)."""
    out: list[str] = []
    mpath = directory / "manifest.json"
    if not mpath.is_file():
        return None, None, [f"{directory}: no manifest.json"], {}
    try:
        manifest = json.loads(mpath.read_text())
    except json.JSONDecodeError as e:
        return None, None, [f"manifest: not JSON ({e})"], {}
    out += _manifest_violations(manifest)
    if not isinstance(manifest, dict):
        return None, None, out, {}
    hashes = {}
    for f in FILES:
        p = directory / f
        if not p.is_file():
            out.append(f"{f}: missing")
            continue
        hashes[f] = sha256_file(p)
        want = manifest.get("files", {}).get(f) if isinstance(manifest.get("files"), dict) else None
        if isinstance(want, str) and hashes[f] != want:
            out.append(f"{f}: sha256 {hashes[f]} does not match the manifest")
    if len(hashes) < len(FILES) or not _check_type(manifest.get("steps"), "positive_int"):
        return manifest, None, out, hashes
    steps = manifest["steps"]
    if steps > MAX_STEPS:
        out.append(f"manifest: steps {steps} above {MAX_STEPS}")
        return manifest, None, out, hashes
    tick = None
    if _check_type(manifest.get("tick"), "positive_number"):
        tick = Fraction(repr(manifest["tick"]))
    step_seconds = None
    if _check_type(manifest.get("step_days"), "positive_number") and manifest.get("timestamp_unit") == "s":
        x = Fraction(repr(manifest["step_days"])) * 86400
        if x.denominator == 1:
            step_seconds = x.numerator
        else:
            out.append(f"manifest: step_days {manifest['step_days']} is not a whole number of seconds")

    book = {}
    rows = _read_csv(directory / "paths.csv", PATHS_COLUMNS, out) or []
    for n, r in rows:
        where = f"paths.csv:{n}"
        pid, step, ts = _int(r["path_id"], where, out), _int(r["step"], where, out), _int(r["timestamp"], where, out)
        bid, ask = _num(r["bid"], where, out), _num(r["ask"], where, out)
        bs, as_ = _int(r["bid_size"], where, out, MAX_LOTS), _int(r["ask_size"], where, out, MAX_LOTS)
        if None in (pid, step, ts, bid, ask, bs, as_):
            continue
        if (pid, step) in book:
            out.append(f"{where}: duplicate key (path {pid}, step {step})")
            continue
        # Exact comparison on the CSV text: floats could hide a crossed book.
        exact = {col: Fraction(r[col]) for col in ("bid", "ask")}
        if not 0 < exact["bid"] <= exact["ask"]:
            out.append(f"{where}: needs 0 < bid <= ask, got {_show(r['bid'])}, {_show(r['ask'])}")
        elif not bid > 0.0:
            out.append(f"{where}: bid {_show(r['bid'])} underflows to zero as a float")
        elif tick is not None:
            for col in ("bid", "ask"):
                if exact[col] % tick != 0:
                    out.append(f"{where}: {col} {_show(r[col])} is not a multiple of the tick {manifest['tick']!r}")
        if step > steps:
            out.append(f"{where}: step {step} beyond steps {steps}")
            continue
        book[(pid, step)] = (ts, bid, ask, bs, as_)

    execution = {}
    rows = _read_csv(directory / "execution.csv", EXECUTION_COLUMNS, out) or []
    for n, r in rows:
        where = f"execution.csv:{n}"
        vals = [_int(r[c], where, out, MAX_LOTS) for c in EXECUTION_COLUMNS]
        if None in vals:
            continue
        pid, step, fb, fs, rej = vals
        if (pid, step) in execution:
            out.append(f"{where}: duplicate key (path {pid}, step {step})")
            continue
        if rej not in (0, 1):
            out.append(f"{where}: reject must be 0 or 1, got {rej}")
        if step >= steps:
            out.append(f"{where}: step {step} is not a decision step (0 to {steps - 1})")
            continue
        execution[(pid, step)] = (fb, fs, rej)

    paths = sorted({p for p, _ in book} | {p for p, _ in execution})
    if not paths:
        out.append("dataset: no rows")
        return manifest, None, out, hashes
    if paths != list(range(len(paths))):
        out.append("dataset: path_id must run 0 to N - 1")
    for p in paths:
        for s in range(steps + 1):
            if (p, s) not in book:
                out.append(f"paths.csv: missing (path {p}, step {s})")
        for s in range(steps):
            if (p, s) not in execution:
                out.append(f"execution.csv: missing (path {p}, step {s}) (no 1:1 join with paths.csv)")
        ts = [book[(p, s)][0] for s in range(steps + 1) if (p, s) in book]
        if any(b <= a for a, b in zip(ts, ts[1:])):
            out.append(f"paths.csv: timestamps of path {p} are not strictly increasing")
        if step_seconds is not None and len(ts) == steps + 1:
            for s, (a, b) in enumerate(zip(ts, ts[1:])):
                if b - a != step_seconds:
                    out.append(
                        f"paths.csv: path {p} steps {s}-{s + 1} are {b - a} s apart, not step_days ({step_seconds} s)"
                    )
    for (p, s), (fb, fs, _) in execution.items():
        if (p, s) in book:
            _, _, _, bs, as_ = book[(p, s)]
            if fb > as_:
                out.append(f"execution.csv: (path {p}, step {s}) fillable_buy {fb} above ask_size {as_}")
            if fs > bs:
                out.append(f"execution.csv: (path {p}, step {s}) fillable_sell {fs} above bid_size {bs}")
    if out:
        return manifest, None, out, hashes
    n = len(paths)
    arr = {k: np.empty((n, steps + 1)) for k in ("bid", "ask")}
    arr |= {k: np.empty((n, steps + 1), dtype=np.int64) for k in ("bid_size", "ask_size")}
    arr |= {k: np.empty((n, steps), dtype=np.int64) for k in ("fillable_buy", "fillable_sell", "reject")}
    for (p, s), (_, bid, ask, bs, as_) in book.items():
        arr["bid"][p, s], arr["ask"][p, s], arr["bid_size"][p, s], arr["ask_size"][p, s] = bid, ask, bs, as_
    for (p, s), (fb, fs, rej) in execution.items():
        arr["fillable_buy"][p, s], arr["fillable_sell"][p, s], arr["reject"][p, s] = fb, fs, rej
    return manifest, arr, out, hashes


def validate(directory: Path) -> list[str]:
    """Every violation of the schema, the hashes and the invariants."""
    return _parse(Path(directory))[2]


def load(directory: Path) -> Dataset:
    manifest, arr, out, hashes = _parse(Path(directory))
    if out or arr is None:
        raise DatasetInvalid(out or ["dataset: could not be read"])
    return Dataset(manifest=manifest, sha256=hashes, **arr)
