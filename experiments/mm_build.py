"""Build the market-maker-rs counterpart binary and write its manifest.

    experiments/.venv/bin/python experiments/mm_build.py --checkout <market-maker-rs checkout>

The checkout must be clean (no change, tracked or untracked, outside
`target/`). market-maker-rs does not commit a Cargo.lock, so the resolved
dependencies are pinned here: `experiments/mm_counterpart.Cargo.lock` is
copied into the checkout (where it is ignored) and the build is `--locked`
to it; with `--generate-lock`, cargo resolves a new one and it is written
there for review. The binary is built with
`cargo build --release --locked --features options --example counterpart`,
with COUNTERPART_GIT_SHA set to the checkout's HEAD and COUNTERPART_GIT_DIRTY
to false, and `experiments/mm_counterpart_manifest.json` records the commit,
the build command, the toolchain, the host, the crate version and the
binary's sha256. `MarketMakerRsCounterpart` refuses any binary whose sha256,
commit, version or clean flag differ from the manifest.

The sha256 pins this build on this host: a binary embeds paths and the
toolchain, so a build elsewhere has another hash and needs its own manifest
(rebuilt with this script and reviewed). Print the binary's path for
MM_COUNTERPART_BIN.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
from pathlib import Path

MANIFEST = Path(__file__).resolve().parent / "mm_counterpart_manifest.json"
LOCK = Path(__file__).resolve().parent / "mm_counterpart.Cargo.lock"
BUILD = ["cargo", "build", "--release", "--locked", "--features", "options", "--example", "counterpart"]


def run(cmd: list[str], cwd: Path, env: dict | None = None) -> str:
    out = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True, check=False)
    if out.returncode != 0:
        raise SystemExit(f"{' '.join(cmd)} failed:\n{out.stderr}")
    return out.stdout.strip()


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--checkout", required=True, type=Path, help="a clean market-maker-rs checkout")
    ap.add_argument("--manifest", type=Path, default=MANIFEST)
    ap.add_argument("--generate-lock", action="store_true", help="resolve a new Cargo.lock and write it next to the manifest")
    args = ap.parse_args()
    repo = args.checkout.resolve()
    status = run(["git", "status", "--porcelain", "--untracked-files=all"], repo)
    dirty = [line for line in status.splitlines() if not line[3:].startswith("target/")]
    if dirty:
        raise SystemExit("the checkout is not clean:\n" + "\n".join(dirty))
    commit = run(["git", "rev-parse", "HEAD"], repo)
    if args.generate_lock:
        (repo / "Cargo.lock").unlink(missing_ok=True)
        run(["cargo", "generate-lockfile"], repo)
        LOCK.write_bytes((repo / "Cargo.lock").read_bytes())
    elif not LOCK.is_file():
        raise SystemExit(f"{LOCK} is missing: run with --generate-lock once and review it")
    (repo / "Cargo.lock").write_bytes(LOCK.read_bytes())
    env = os.environ | {"COUNTERPART_GIT_SHA": commit, "COUNTERPART_GIT_DIRTY": "false"}
    run(BUILD, repo, env)
    binary = repo / "target" / "release" / "examples" / "counterpart"
    ident = json.loads(run([str(binary), "identity"], repo))
    if ident.get("git_sha") != commit or ident.get("git_dirty") != "false":
        raise SystemExit(f"the binary reports {ident}, not commit {commit} clean")
    rustc = run(["rustc", "-vV"], repo)
    manifest = {
        "protocol": ident["protocol"],
        "crate_version": ident["version"],
        "market_maker_rs_commit": commit,
        "build_command": f"COUNTERPART_GIT_SHA={commit} COUNTERPART_GIT_DIRTY=false {' '.join(BUILD)}",
        "toolchain": run(["rustc", "-V"], repo),
        "cargo": run(["cargo", "-V"], repo),
        "host": next(line.split(": ", 1)[1] for line in rustc.splitlines() if line.startswith("host: ")),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "cargo_lock_sha256": hashlib.sha256(LOCK.read_bytes()).hexdigest(),
    }
    args.manifest.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(binary)


if __name__ == "__main__":
    main()
