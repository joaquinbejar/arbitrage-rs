"""#43, run 3: a new held-out evaluation against the market-maker-rs
counterpart, after run 2.

The protocol is `research/preregistration/deep_hedging_mm_v2.md`; this
docstring only summarises it. Run 2's script (`deep_hedging_mm.py`) is
unchanged and supplies every piece the two runs share; only the Heston
evaluation set, its bootstrap, the binding and the file policy are new.

Three commands, from the repository root:

    experiments/.venv/bin/python experiments/deep_hedging_mm_run3.py template
    experiments/.venv/bin/python experiments/deep_hedging_mm_run3.py fill
    OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 MKL_NUM_THREADS=1 \\
    VECLIB_MAXIMUM_THREADS=1 MM_COUNTERPART_BIN=<binary> \\
        experiments/.venv/bin/python experiments/deep_hedging_mm_run3.py run \\
        --code-commit C --preregistration-sha256 H_P \\
        --template-sha256 T --manifest-sha256 L [--restart]

- `template` writes the launch manifest template (`TEMPLATE`) from the
  tree: every pin, the forbidden seeds and two null seed slots. It is
  committed with the code in commit C. Run it with the same four thread
  variables set to 1, since the template pins them.
- `fill` is the maintainer's: it draws both seeds with `secrets` under the
  fill rule and writes the final manifest (`MANIFEST`), which is committed
  verbatim in a child commit of C that changes nothing else.
- `run` takes C, H_P, T and L from the approvals, never from the tree, and
  refuses to generate anything unless the tree matches all four.
"""

from __future__ import annotations

import argparse
import calendar
import errno
import hashlib
import json
import os
import platform
import secrets
import struct
import subprocess
import sys
import tempfile
import time
import traceback
from pathlib import Path

import numpy as np
import torch

import counterpart as cp
import deep_hedging_mm as mm2
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

PREREGISTRATION = ROOT / "research" / "preregistration" / "deep_hedging_mm_v2.md"
TEMPLATE = ROOT / "research" / "preregistration" / "deep_hedging_mm_v2_launch.template.json"
MANIFEST = ROOT / "research" / "preregistration" / "deep_hedging_mm_v2_launch.json"
RESULTS = mm2.RESULTS
OUT = RESULTS / "deep_hedging_mm_run3.json"
ATTEMPTS = RESULTS / "deep_hedging_mm_run3"
PRE_FLIGHT_FAILURE = RESULTS / "deep_hedging_mm_run3_preflight_failure.json"
RUN_2 = mm2.OUT
FIRST_PREREGISTRATION = mm2.PREREGISTRATION

PREREGISTRATION_REL = str(PREREGISTRATION.relative_to(ROOT))
TEMPLATE_REL = str(TEMPLATE.relative_to(ROOT))
MANIFEST_REL = str(MANIFEST.relative_to(ROOT))
ATTEMPTS_REL = str(ATTEMPTS.relative_to(ROOT))

SCHEMA = "arbitrage-rs/deep_hedging_mm_run3_launch/1"
SEED_SLOTS = ("eval_seed", "bootstrap_seed")
# Bootstrap draws use bootstrap_seed + offset, offsets below this bound:
# i, 100 + i and 200 + i for learned seed index i < 100.
BOOTSTRAP_SPAN = 300
MAX_SEED = 2**64 - BOOTSTRAP_SPAN
# Every integer seed the repository uses anywhere (experiments, tests and
# datasets), besides those of CONFIG. No seed of run 3 may be one of them.
LITERAL_SEEDS = (1, 2, 3, 5, 11, 12, 101, 202, 20261103, 2**64 - 1)
# The dummy seeds of the dry run done before the code commit
# (preregistration, "Dry run"): forbidden, and their bootstrap range is
# used, like the reference run's.
DRY_RUN_SEEDS = {"eval_seed": 4659386883093802759, "bootstrap_seed": 2043307074730982498}
# Infrastructure causes after which one restart is allowed (preregistration,
# "Failures"). Exceptions are classified from the attempt's own record;
# the two causes without an exception need a system record.
INFRA_ERRNOS = {errno.ENOSPC: "ENOSPC", errno.EIO: "EIO", errno.EDQUOT: "EDQUOT"}
INFRA_SYSTEM_CAUSES = frozenset({"oom-kill", "host-restart"})
# Thread counts of the numerical libraries under numpy and torch. They act
# only when set before the process starts, so the launch command sets them
# to 1, the template pins them, and the run refuses any other value.
THREAD_ENV = ("OMP_NUM_THREADS", "OPENBLAS_NUM_THREADS", "MKL_NUM_THREADS", "VECLIB_MAXIMUM_THREADS")
# The causes a failure record may give, each with its verbatim system
# record: the two without an exception, and those whose exception record
# the run could not write itself (a full disk), copied from its stderr.
FAILURE_RECORD_CAUSES = INFRA_SYSTEM_CAUSES | frozenset(INFRA_ERRNOS.values()) | {"MemoryError"}
# A restart must begin within this many seconds of the interrupted
# attempt's last record.
RESTART_WINDOW_S = 3600


class LaunchError(Exception):
    """The tree, the approvals or a pin disagree: nothing is generated."""


class AttemptError(Exception):
    """An attempt that cannot start or continue under the file policy."""


# ---------------------------------------------------------------- canonical JSON


def canonical_bytes(obj) -> bytes:
    """The one serialisation of a launch manifest: sorted keys, two space
    indent, ASCII, one final LF."""
    return (json.dumps(obj, indent=2, sort_keys=True, ensure_ascii=True, allow_nan=False) + "\n").encode("ascii")


def _no_duplicates(pairs):
    out = {}
    for k, v in pairs:
        if k in out:
            raise LaunchError(f"duplicate JSON key {k!r}")
        out[k] = v
    return out


def _no_constant(name):
    raise LaunchError(f"non-finite JSON number {name}")


def parse_canonical(raw: bytes, what: str) -> dict:
    """A launch manifest from its exact bytes: UTF-8 without BOM or CR, no
    duplicate key, no NaN or infinity, an object, and byte-equal to its
    canonical serialisation (so one content has one hash)."""
    if raw.startswith(b"\xef\xbb\xbf") or b"\r" in raw:
        raise LaunchError(f"{what}: byte order mark or CR")
    try:
        obj = json.loads(raw.decode("utf-8"), object_pairs_hook=_no_duplicates, parse_constant=_no_constant)
    except UnicodeDecodeError as e:
        raise LaunchError(f"{what}: not UTF-8 ({e})") from None
    except json.JSONDecodeError as e:
        raise LaunchError(f"{what}: not JSON ({e})") from None
    if not isinstance(obj, dict):
        raise LaunchError(f"{what}: not an object")
    if canonical_bytes(obj) != raw:
        raise LaunchError(f"{what}: not in canonical form")
    return obj


def sha256_bytes(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


# ---------------------------------------------------------------- seeds


def forbidden_seeds(cfg: dict) -> list[int]:
    """The seeds no run 3 seed may be: CONFIG's and every literal seed."""
    return sorted({cfg["data_seed"], cfg["bootstrap_seed"], *cfg["train_seeds"], *LITERAL_SEEDS, *DRY_RUN_SEEDS.values()})


def used_bootstrap_ranges(cfg: dict) -> list[tuple[int, int]]:
    """The bootstrap ranges already drawn from: the reference run's and the
    dry run's."""
    return [(b, b + BOOTSTRAP_SPAN - 1) for b in (cfg["bootstrap_seed"], DRY_RUN_SEEDS["bootstrap_seed"])]


def fill_rule(seeds: dict, cfg: dict, forbidden: list[int]) -> list[str]:
    """Every way `seeds` breaks the fill rule (preregistration, "Seeds"),
    empty when it holds."""
    bad = []
    if not isinstance(seeds, dict) or set(seeds) != set(SEED_SLOTS):
        return [f"seeds must be an object with exactly {list(SEED_SLOTS)}"]
    for k in SEED_SLOTS:
        v = seeds[k]
        if type(v) is not int:
            bad.append(f"{k} is {type(v).__name__}, not an int")
        elif not 0 <= v <= MAX_SEED:
            bad.append(f"{k} {v} is outside [0, 2^64 - {BOOTSTRAP_SPAN}]")
    if bad:
        return bad
    if len(cfg["train_seeds"]) >= 100:
        bad.append("100 or more train seeds would overlap the bootstrap offsets")
    e, b = seeds["eval_seed"], seeds["bootstrap_seed"]
    mine = (b, b + BOOTSTRAP_SPAN - 1)
    if e in forbidden:
        bad.append(f"eval_seed {e} is a forbidden seed")
    if mine[0] <= e <= mine[1]:
        bad.append(f"eval_seed {e} is in its own bootstrap range {list(mine)}")
    for used in used_bootstrap_ranges(cfg):
        if used[0] <= e <= used[1]:
            bad.append(f"eval_seed {e} is in the used bootstrap range {list(used)}")
        if mine[0] <= used[1] and used[0] <= mine[1]:
            bad.append(f"bootstrap range {list(mine)} meets the used bootstrap range {list(used)}")
    hit = [f for f in forbidden if mine[0] <= f <= mine[1]]
    if hit:
        bad.append(f"bootstrap range {list(mine)} holds forbidden seeds {hit}")
    return bad


def draw_seeds(cfg: dict, forbidden: list[int], randbelow=secrets.randbelow) -> tuple[dict, int]:
    """Both seeds drawn uniformly from [0, 2^64 - 300] with a CSPRNG, the
    pair redrawn only while it breaks the fill rule. Returns the seeds and
    the number of draws."""
    draws = 0
    while True:
        draws += 1
        seeds = {k: randbelow(MAX_SEED + 1) for k in SEED_SLOTS}
        if not fill_rule(seeds, cfg, forbidden):
            return seeds, draws


# ---------------------------------------------------------------- pins


def _git(root: Path, *args: str) -> str:
    out = subprocess.run(["git", *args], cwd=root, capture_output=True, text=True, check=False)
    if out.returncode != 0:
        raise LaunchError(f"git {' '.join(args)}: {out.stderr.strip()}")
    return out.stdout


def _file_sha(root: Path, rel: str) -> str:
    return sha256_bytes((root / rel).read_bytes())


def environment() -> dict:
    """Plain JSON types only: torch's version is a str subclass, which the
    strict comparison with the template would refuse."""
    return {
        "python": str(platform.python_version()),
        "numpy": str(np.__version__),
        "torch": str(torch.__version__),
        "machine": str(platform.machine()),
        "torch_threads": int(torch.get_num_threads()),
        "deterministic_algorithms": bool(torch.are_deterministic_algorithms_enabled()),
        "thread_env": {k: os.environ.get(k) for k in THREAD_ENV},
    }


def launch_facts(root: Path = ROOT) -> dict:
    """What the tree pins, in the template's shape without the seeds: every
    tracked file of experiments/, the preregistrations, the result inputs,
    the 15 weight files, the binary manifest, the configurations, the
    environment and the forbidden seeds."""
    files = sorted(p for p in _git(root, "ls-files", "-z", "experiments").split("\0") if p)
    reference = json.loads((root / "research" / "results" / "deep_hedging_counterpart.json").read_text())
    weights = {k: _file_sha(root, w["file"]) for k, w in sorted(reference["weights"].items())}
    binary_manifest = json.loads(cp.MM_MANIFEST.read_text())
    rel = lambda p: str(Path(p).resolve().relative_to(root.resolve()))  # noqa: E731
    return {
        "schema": SCHEMA,
        "preregistration": {"path": rel(PREREGISTRATION), "sha256": _file_sha(root, rel(PREREGISTRATION))},
        "first_preregistration_sha256": _file_sha(root, rel(FIRST_PREREGISTRATION)),
        "files": {p: _file_sha(root, p) for p in files},
        "results_inputs": {
            rel(p): _file_sha(root, rel(p))
            for p in (mm2.REFERENCE_RUN, RUN_2, mm2.SURFACE_B_QUOTES, mm2.SURFACE_B_FILE, *mm2.FAILED_RUNS)
        },
        "weights": weights,
        "binary_sha256": binary_manifest["binary_sha256"],
        "config": json.loads(json.dumps({"config": CONFIG, "counterpart_config": COUNTERPART_CONFIG, "mm_config": mm2.MM_CONFIG})),
        "environment": environment(),
        "forbidden_seeds": forbidden_seeds(CONFIG),
    }


def make_template(facts: dict) -> dict:
    return facts | {"seeds": {k: None for k in SEED_SLOTS}}


def _strict_diff(a, b, path: str) -> list[str]:
    out = {"differing": [], "missing": [], "extra": [], "nonfinite": []}
    mm2._strict_equal(a, b, path, out)
    return [f"{k}: {x}" for k, v in out.items() for x in v]


# ---------------------------------------------------------------- binding


def verify_launch(
    root: Path, code_commit: str, prereg_sha: str, template_sha: str, manifest_sha: str, restart: bool = False, facts_fn=launch_facts
) -> dict:
    """The preregistration's "Binding", step 4. The four approved values
    come from the caller; the tree must match each of them. On a restart,
    the interrupted attempt's own files are the only untracked files
    allowed. Returns the final manifest; raises LaunchError on the first
    disagreement."""
    for name, v, n in (("code commit", code_commit, 40), ("H_P", prereg_sha, 64), ("T", template_sha, 64), ("L", manifest_sha, 64)):
        if not (isinstance(v, str) and len(v) == n and all(c in "0123456789abcdef" for c in v)):
            raise LaunchError(f"{name} {v!r} is not {n} lowercase hex digits")
    status = _git(root, "status", "--porcelain=v1", "--untracked-files=all").splitlines()
    allowed = f"?? {ATTEMPTS_REL}/attempt-1/"
    dirty = [line for line in status if not (restart and line.startswith(allowed))]
    if dirty:
        raise LaunchError("the tree is not clean: " + "; ".join(dirty[:10]))
    parents = _git(root, "rev-list", "--parents", "-n", "1", "HEAD").split()[1:]
    if parents != [code_commit]:
        raise LaunchError(f"HEAD's parents {parents} are not exactly the approved commit {code_commit}")
    manifest_rel, template_rel, prereg_rel = MANIFEST_REL, TEMPLATE_REL, PREREGISTRATION_REL
    changes = _git(root, "diff", "--no-renames", "--name-status", code_commit, "HEAD").splitlines()
    if changes != [f"A\t{manifest_rel}"]:
        raise LaunchError(f"HEAD's change from the approved commit is {changes}, not only the added {manifest_rel}")
    prereg = (root / prereg_rel).read_bytes()
    if prereg.startswith(b"\xef\xbb\xbf") or b"\r" in prereg:
        raise LaunchError("the preregistration has a byte order mark or a CR")
    if sha256_bytes(prereg) != prereg_sha:
        raise LaunchError(f"the preregistration's sha256 {sha256_bytes(prereg)} is not the approved {prereg_sha}")
    template_raw = (root / template_rel).read_bytes()
    if sha256_bytes(template_raw) != template_sha:
        raise LaunchError(f"the template's sha256 {sha256_bytes(template_raw)} is not the approved {template_sha}")
    manifest_raw = (root / manifest_rel).read_bytes()
    if sha256_bytes(manifest_raw) != manifest_sha:
        raise LaunchError(f"the final manifest's sha256 {sha256_bytes(manifest_raw)} is not the authorized {manifest_sha}")
    template = parse_canonical(template_raw, "template")
    final = parse_canonical(manifest_raw, "final manifest")
    seeds = template.get("seeds")
    if not (isinstance(seeds, dict) and set(seeds) == set(SEED_SLOTS) and all(seeds[k] is None for k in SEED_SLOTS)):
        raise LaunchError("the template's seeds are not exactly two null slots")
    pins = {k: v for k, v in template.items() if k != "seeds"}
    diff = _strict_diff(facts_fn(root), pins, "pins")
    if diff:
        raise LaunchError(f"the tree does not match the template's pins: {diff[:10]}")
    env = pins.get("environment")
    if not (
        isinstance(env, dict)
        and env.get("torch_threads") == 1
        and env.get("deterministic_algorithms") is True
        and env.get("thread_env") == {k: "1" for k in THREAD_ENV}
    ):
        raise LaunchError(f"the template does not pin one thread everywhere and deterministic algorithms: {env}")
    if pins["preregistration"]["sha256"] != prereg_sha:
        raise LaunchError("the template does not pin the approved preregistration")
    if set(final) != set(template):
        raise LaunchError(f"the final manifest's fields {sorted(final)} are not the template's {sorted(template)}")
    diff = _strict_diff({k: v for k, v in final.items() if k != "seeds"}, pins, "manifest")
    if diff:
        raise LaunchError(f"the final manifest differs from the template outside the seeds: {diff[:10]}")
    bad = fill_rule(final["seeds"], CONFIG, pins["forbidden_seeds"])
    if bad:
        raise LaunchError(f"the seeds break the fill rule: {bad}")
    return final


# ---------------------------------------------------------------- attempts


def _scrub(text: str) -> str:
    """A record without local paths: the repository and the interpreter."""
    for path, name in ((str(ROOT), "<root>"), (sys.base_prefix, "<python>"), (str(Path.home()), "<home>")):
        text = text.replace(path, name)
    return text


def write_exclusive(path: Path, data: bytes) -> None:
    """Writes a new file; an existing one is never replaced."""
    with open(path, "xb") as f:
        f.write(data)
        f.flush()
        os.fsync(f.fileno())


def publish_exclusive(path: Path, data: bytes, tmp_dir: Path) -> None:
    """Writes to a temporary file in `tmp_dir` (the attempt's own
    directory, on the same filesystem) and links it to `path` in one step:
    the file exists complete or not at all, an existing one is never
    replaced, and an interruption leaves its temporary file only inside
    the attempt."""
    fd, tmp = tempfile.mkstemp(dir=tmp_dir, prefix=f".{path.name}.")
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(data)
            f.flush()
            os.fsync(f.fileno())
        os.link(tmp, path)
    finally:
        os.unlink(tmp)


def classify_exception(record: dict) -> str | None:
    """The infrastructure cause an attempt's exception record shows, or
    None."""
    if record.get("type") == "MemoryError":
        return "MemoryError"
    return INFRA_ERRNOS.get(record.get("errno"))


def utc_stamp(t: float | None = None) -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() if t is None else t))


def parse_utc(stamp) -> float:
    if not isinstance(stamp, str):
        raise AttemptError(f"a record time {stamp!r} is not a UTC stamp")
    try:
        return float(calendar.timegm(time.strptime(stamp, "%Y-%m-%dT%H:%M:%SZ")))
    except ValueError:
        raise AttemptError(f"a record time {stamp!r} is not a UTC stamp") from None


def last_record_time(manifest: dict, stages: list[dict], exception: dict | None) -> float:
    """The restart window's anchor: the latest time the interrupted run
    wrote inside its own records (the execution manifest's start, each
    stage line, the exception record). File times are not used, so
    touching or copying a file does not move it."""
    stamps = [manifest.get("started_at"), *(s.get("at") for s in stages)]
    if exception is not None:
        stamps.append(exception.get("at"))
    return max(parse_utc(x) for x in stamps)


def _comparable(manifest: dict) -> dict:
    return {k: v for k, v in manifest.items() if k not in ("attempt", "started_at")}


def open_attempt(attempts: Path, out: Path, execution: dict, restart: bool, now: float | None = None) -> tuple[Path, dict | None]:
    """The preregistration's file policy. The results must not exist. A
    first attempt needs no attempt on record; a restart needs exactly one,
    interrupted for an infrastructure cause, and the same execution
    manifest apart from the attempt number and the start time. Publishes
    this attempt's execution manifest (the attempt starts there) and
    returns its directory and the earlier attempt's logged stages."""
    now = time.time() if now is None else now
    if out.exists():
        raise AttemptError(f"{out.name} exists: run 3 has results")
    attempts.mkdir(exist_ok=True)
    existing = sorted(p.name for p in attempts.iterdir())
    previous = None
    if not restart:
        if existing:
            raise AttemptError(f"attempts already on record: {existing}")
        number = 1
    else:
        if existing != ["attempt-1"]:
            raise AttemptError(f"a restart needs exactly attempt-1 on record, found {existing}")
        first = attempts / "attempt-1"
        try:
            m1 = json.loads((first / "execution_manifest.json").read_text())
            # The stage log is absent when the interruption came before its
            # creation; then there is no logged array to reproduce.
            log1 = first / "stage.log"
            stages = [json.loads(line) for line in log1.read_text().splitlines() if line] if log1.exists() else []
        except (OSError, ValueError) as e:
            raise AttemptError(f"attempt-1 is incomplete: {e}") from None
        exc = first / "exception.json"
        if exc.exists():
            cause = classify_exception(json.loads(exc.read_text()))
            if cause is None:
                raise AttemptError("attempt-1 ended with an exception that is not an infrastructure cause")
        else:
            try:
                record = json.loads((first / "failure.json").read_text())
            except (OSError, ValueError):
                raise AttemptError("attempt-1 has neither an exception record nor a failure record") from None
            if record.get("cause") not in FAILURE_RECORD_CAUSES or not record.get("system_record"):
                raise AttemptError("attempt-1's failure record shows no infrastructure cause with its system record")
        if _comparable(m1) != _comparable(execution):
            raise AttemptError("the restart's execution manifest differs from attempt-1's")
        last = last_record_time(m1, stages, json.loads(exc.read_text()) if exc.exists() else None)
        if last > now:
            raise AttemptError(f"attempt-1's last record is in the future ({last - now:.0f} s ahead)")
        if now - last > RESTART_WINDOW_S:
            raise AttemptError(f"the restart is {now - last:.0f} s after attempt-1's last record, beyond {RESTART_WINDOW_S} s")
        previous = {s["stage"]: s.get("arrays", {}) for s in stages}
        number = 2
    here = attempts / f"attempt-{number}"
    here.mkdir()
    publish_exclusive(here / "execution_manifest.json", canonical_bytes(execution | {"attempt": number}), here)
    return here, previous


class StageLog:
    """Stage names, elapsed seconds and data array hashes, nothing else.
    On a restart, every array hash must equal the interrupted attempt's."""

    def __init__(self, path: Path, previous: dict | None, started: float, echo: bool = True):
        self.f = open(path, "x", buffering=1)
        self.echo = echo
        self.previous = previous
        self.started = started

    def stage(self, name: str, arrays: dict | None = None) -> None:
        hashes = {k: array_sha256(v) for k, v in (arrays or {}).items()}
        if self.previous is not None and name in self.previous and self.previous[name] != hashes:
            raise AttemptError(f"stage {name}: the restart does not reproduce attempt-1's data")
        line = json.dumps({"stage": name, "at": utc_stamp(), "elapsed_s": round(time.time() - self.started, 1), "arrays": hashes}, sort_keys=True)
        self.f.write(line + "\n")
        os.fsync(self.f.fileno())
        if self.echo:
            print(line, flush=True)


def array_sha256(a: np.ndarray) -> str:
    a = np.ascontiguousarray(a)
    return sha256_bytes(f"{a.dtype.str}{a.shape}".encode() + a.tobytes())


# ---------------------------------------------------------------- data


def heston_set(eval_seed: int, cfg: dict) -> dict:
    """The new Heston evaluation set (preregistration, "Seeds")."""
    c0, c1 = np.random.SeedSequence(eval_seed).spawn(2)
    return market(heston_paths(cfg["n_test"], np.random.default_rng(c0), cfg), np.random.default_rng(c1), cfg)


def with_bootstrap_seed(cfg: dict, bootstrap_seed: int) -> dict:
    """The configuration the Heston rows bootstrap with."""
    return cfg | {"bootstrap_seed": bootstrap_seed}


def surface_a_changes(cal: dict, run2_cal: dict) -> list[dict]:
    """Per day, the parameters of surface A that differ, bit for bit, from
    run 2's."""
    out = []
    old = {s["day"]: s["params"] for s in run2_cal["slices"]}
    for s in cal["slices"]:
        before = old.get(s["day"])
        if before is None:
            out.append({"day": s["day"], "new_day": True})
            continue
        changed = {
            k: {"run_2": repr(before.get(k)), "run_3": repr(v)}
            for k, v in s["params"].items()
            if not (isinstance(before.get(k), float) and struct.pack(">d", before[k]) == struct.pack(">d", float(v)))
        }
        if changed:
            out.append({"day": s["day"], "changed": changed})
    return out


# ---------------------------------------------------------------- run


def run(final: dict, binding: dict, log: StageLog, started: float, cfg: dict = CONFIG) -> dict:
    """Steps 2 to 8 of the preregistration's "Order of the run". `cfg` is
    CONFIG in the run; tests pass a smaller one."""
    ccfg, mcfg = COUNTERPART_CONFIG, mm2.MM_CONFIG
    seeds = final["seeds"]
    hcfg = with_bootstrap_seed(cfg, seeds["bootstrap_seed"])
    binary = Path(os.environ.get(cp.MM_BINARY_ENV, ""))
    identity = cp.mm_identity(binary)
    reference_run = json.loads(mm2.REFERENCE_RUN.read_text())
    run2 = json.loads(RUN_2.read_text())

    seq = np.random.SeedSequence(cfg["data_seed"])
    gens = [np.random.default_rng(s) for s in seq.spawn(7)]
    val = market(gbm_paths(cfg["n_val"], gens[1], cfg), gens[2], cfg)
    test = market(gbm_paths(cfg["n_test"], gens[3], cfg), gens[4], cfg)
    log.stage("data_gbm", {f"val.{k}": v for k, v in val.items()} | {f"test.{k}": v for k, v in test.items()})
    heston_mkt = heston_set(seeds["eval_seed"], cfg)
    log.stage("data_heston", {f"heston.{k}": v for k, v in heston_mkt.items()})

    conf = mm2.conformance(binary)
    b_text, b, quotes = mm2.calibrate_b(binary, ccfg, mcfg)
    committed = json.loads(mm2.SURFACE_B_FILE.read_text())
    frozen = mm2.SURFACE_B_QUOTES.read_text() == quotes and {k: v for k, v in committed.items() if k != "identity"} == {
        k: v for k, v in b.items() if k != "identity"
    }
    if not frozen:
        raise SystemExit("surface B or its quotes differ from the committed files")
    if not b["slices"]:
        raise SystemExit(f"surface B failed: {b['reasons']}")
    a_text, a_cal = mm2.surface_a(ccfg)
    log.stage("surfaces")

    b_slices, b_ranges = cp.slices_from_surface_json(b_text)
    a_slices, a_ranges = cp.slices_from_surface_json(a_text)
    contract = cp.Contract(strike=cfg["strike"], expiry_days=float(cfg["steps"]), kind="call")
    cov = {d: mm2.coverage(b_slices, b_ranges, heston_mkt["s"], cfg, d) for d in cp.DYNAMICS}
    mm = {d: cp.MarketMakerRsCounterpart(b_text, d, s_ref=cfg["s0"]) for d in cp.DYNAMICS}
    ref_b = {d: cp.ReferenceCounterpart(cp.SviSurface(b_slices), d, s_ref=cfg["s0"]) for d in cp.DYNAMICS}
    comparison = {d: mm2.compare(mm[d], ref_b[d], contract, heston_mkt["s"], cov[d], cfg) for d in cp.DYNAMICS}
    cov_a = {d: mm2.coverage(a_slices, a_ranges, heston_mkt["s"], cfg, d) for d in cp.DYNAMICS}
    not_representable = mm2.representability(binary, a_slices, a_ranges)
    if not_representable:
        diagnostic_a = {"representable": False, "not_representable": not_representable}
    else:
        mm_a = {d: cp.MarketMakerRsCounterpart(a_text, d, s_ref=cfg["s0"]) for d in cp.DYNAMICS}
        ref_a = {d: cp.ReferenceCounterpart(cp.SviSurface(a_slices), d, s_ref=cfg["s0"]) for d in cp.DYNAMICS}
        diagnostic_a = {"representable": True} | {
            d: mm2.compare(mm_a[d], ref_a[d], contract, heston_mkt["s"], cov_a[d], cfg) for d in cp.DYNAMICS
        }
    log.stage("coverage_and_agreement")

    premium = float(mm["sticky-moneyness"].price(contract, np.array([cfg["s0"]]), 0.0)[0])
    premium_ref_b = float(ref_b["sticky-moneyness"].price(contract, np.array([cfg["s0"]]), 0.0)[0])
    results = {
        "issue": "arbitrage-rs#43",
        "run": "3",
        "evaluation": "new held-out evaluation after run 2; no prior access to its paths",
        "launch": binding,
        "seeds": seeds,
        "previous_runs": {
            "run_1": {"record": str(mm2.FAILED_RUNS[0].relative_to(ROOT)), "sha256": cp.sha256_file(mm2.FAILED_RUNS[0])},
            "run_2": {"results": str(RUN_2.relative_to(ROOT)), "sha256": cp.sha256_file(RUN_2), "outcome": run2["decision"]["outcome"]},
        },
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
            "file_sha256": hashlib.sha256(b_text.encode()).hexdigest(),
            "quotes_sha256": hashlib.sha256(quotes.encode()).hexdigest(),
            "report": b["report"],
            "data_cutoff": b["data_cutoff"],
            "available_at": b["available_at"],
        },
        "surface_a": {
            "file_sha256": hashlib.sha256(a_text.encode()).hexdigest(),
            "calibration": a_cal,
            "changes_against_run_2": surface_a_changes(a_cal, run2["surface_a"]["calibration"]),
        },
        "coverage": cov,
        "comparison_b": comparison,
        "diagnostic_a": {"coverage": cov_a, "comparison": diagnostic_a},
        "counterparts": {f"heston_{d}": c.identity() for d, c in mm.items()},
        "premium": {"mm_b": premium, "reference_b": premium_ref_b, "reference_run": reference_run["premium"]["heston_svi"]},
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
            # The in-model rows keep the reference run's bootstrap (gate 6
            # reproduces them); the Heston rows use run 3's.
            markets = [("in_model", test, None, cfg)] + [(f"heston_{d}", heston_mkt, premium, hcfg) for d in mm]
            for mname, mkt, prem, bcfg in markets:
                lr, (l_loss, l_ideal) = evaluate(learned_policy(net, cfg), mkt, eps, cfg, premium=prem)
                b_loss, b_ideal = entry[mname]["_band_loss"]
                lr["seed"] = seed
                lr["weights_sha256"] = rec["sha256"]
                lr["delta_cvar95_vs_band"] = paired_bootstrap(l_loss, b_loss, 0.95, bcfg, seed_offset=i)
                lr["delta_cvar95_vs_band_ideal_execution"] = paired_bootstrap(l_ideal, b_ideal, 0.95, bcfg, seed_offset=100 + i)
                entry[mname]["learned"].append(lr)
        for mname in list(entry):
            if isinstance(entry[mname], dict):
                entry[mname] = strip_losses(entry[mname])
        results["by_eps"][key] = entry
        log.stage(f"eps_{key}")

    results["in_model_comparison"] = mm2.compare_in_model(
        json.loads(json.dumps(results["by_eps"])), reference_run["by_eps"], weights=reference_run["weights"]
    )
    results["in_model_matches_reference"] = results["in_model_comparison"]["equal"]
    log.stage("gate_6")

    eps = cfg["primary_eps"]
    in_range_subset = {}
    for dyn, c in mm.items():
        keep = mm2.in_range_paths(cov[dyn], heston_mkt["s"], cfg, dyn)
        _, (b_loss, _) = evaluate(cp_band_policy(results["by_eps"][f"{eps:g}"]["band_w"], c, contract), heston_mkt, eps, cfg, premium=premium)
        rows = []
        for i, seed in enumerate(cfg["train_seeds"]):
            rec = weights[f"{eps:g}/{seed}"]
            net = load_weights(ROOT / rec["file"], rec["sha256"], cfg)
            _, (l_loss, _) = evaluate(learned_policy(net, cfg), heston_mkt, eps, cfg, premium=premium)
            ci = paired_bootstrap(l_loss[keep], b_loss[keep], 0.95, hcfg, seed_offset=200 + i)
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
    log.stage("gate_7")

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
        "6_in_model_matches_reference": results["in_model_matches_reference"],
        "7_in_range_subset_no_veto": not any(v["veto"] for v in in_range_subset.values()),
    }
    outcome = criterion if criterion != "adopt" or all(gates.values()) else "revise"
    results["decision"] = {
        "criterion": "the reference run's, on surface B with the market-maker-rs counterpart, on the new Heston set; ADOPT also requires every gate",
        "in_model_all_seeds_better": in_model,
        "out_of_model_veto": veto,
        "criterion_outcome": criterion,
        "gates": gates,
        "outcome": outcome,
        "counterpart_validation": "market-maker-rs",
    }
    results["run_environment"] = {"seconds": round(time.time() - started, 1)} | environment()
    return results


def preflight_failure(args, error: Exception) -> None:
    record = {
        "error": _scrub(str(error)),
        "type": type(error).__name__,
        "launch": {"code_commit": args.code_commit, "preregistration_sha256": args.preregistration_sha256, "template_sha256": args.template_sha256, "manifest_sha256": args.manifest_sha256},
        "at": utc_stamp(),
    }
    try:
        write_exclusive(PRE_FLIGHT_FAILURE, canonical_bytes(record))
    except OSError:
        pass


def command_run(args) -> None:
    deterministic_settings()
    started = time.time()
    try:
        final = verify_launch(ROOT, args.code_commit, args.preregistration_sha256, args.template_sha256, args.manifest_sha256, restart=args.restart)
        binding = {
            "code_commit": args.code_commit,
            "preregistration_sha256": args.preregistration_sha256,
            "template_sha256": args.template_sha256,
            "manifest_sha256": args.manifest_sha256,
        }
        execution = {
            "launch": binding,
            "seeds": final["seeds"],
            "head": _git(ROOT, "rev-parse", "HEAD").strip(),
            "host": platform.node(),
            "environment": environment(),
            "binary_sha256": final["binary_sha256"],
            "started_at": utc_stamp(),
        }
        here, previous = open_attempt(ATTEMPTS, OUT, execution, args.restart)
    except (LaunchError, AttemptError, OSError) as e:
        preflight_failure(args, e)
        raise SystemExit(f"run 3 not started: {e}") from None
    results = execute_attempt(here, previous, OUT, started, lambda log: run(final, binding | {"attempt": int(here.name.split("-")[1])}, log, started))
    print(json.dumps(results["decision"], indent=2))


def execute_attempt(here: Path, previous: dict | None, out: Path, started: float, body) -> dict:
    """An attempt from the moment its execution manifest exists: everything
    after it, the stage log's creation included, runs under one handler
    that records any exception in the attempt (never replacing a file)
    before it propagates. Publishes the results."""
    try:
        log = StageLog(here / "stage.log", previous, started)
        log.stage("checks")
        results = body(log)
        publish_exclusive(out, (json.dumps(results, indent=2, sort_keys=True) + "\n").encode(), here)
        log.stage("results")
        return results
    except BaseException as e:
        record = {
            "type": type(e).__name__,
            "errno": getattr(e, "errno", None),
            "message": _scrub(str(e)),
            "traceback": _scrub(traceback.format_exc()),
            "at": utc_stamp(),
        }
        try:
            write_exclusive(here / "exception.json", canonical_bytes(record))
        except OSError:
            # A full disk: the traceback still reaches stderr, and is the
            # system record of the failure record (preregistration).
            pass
        raise


def deterministic_settings() -> None:
    """The settings the template pins and the run uses."""
    torch.set_num_threads(1)
    torch.use_deterministic_algorithms(True)


def command_template(args) -> None:
    deterministic_settings()
    raw = canonical_bytes(make_template(launch_facts(ROOT)))
    TEMPLATE.write_bytes(raw)
    print(f"{TEMPLATE.relative_to(ROOT)} sha256 {sha256_bytes(raw)}")


def command_fill(args) -> None:
    raw = TEMPLATE.read_bytes()
    template = parse_canonical(raw, "template")
    seeds, draws = draw_seeds(CONFIG, template["forbidden_seeds"])
    out = canonical_bytes(template | {"seeds": seeds})
    write_exclusive(MANIFEST, out)
    print(f"template sha256 {sha256_bytes(raw)}")
    print(f"{MANIFEST.relative_to(ROOT)} sha256 {sha256_bytes(out)} (secrets.randbelow(2**64 - {BOOTSTRAP_SPAN - 1}), {draws} draw(s))")


def main(argv=None) -> None:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = p.add_subparsers(dest="command", required=True)
    sub.add_parser("template")
    sub.add_parser("fill")
    r = sub.add_parser("run")
    r.add_argument("--code-commit", required=True)
    r.add_argument("--preregistration-sha256", required=True)
    r.add_argument("--template-sha256", required=True)
    r.add_argument("--manifest-sha256", required=True)
    r.add_argument("--restart", action="store_true")
    args = p.parse_args(argv)
    {"run": command_run, "template": command_template, "fill": command_fill}[args.command](args)


if __name__ == "__main__":
    main()
