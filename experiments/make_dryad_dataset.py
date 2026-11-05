"""Build the `dryad-gdax-v1` dataset for #44 from the published Dryad archive.

Run from the repository root:

    experiments/.venv/bin/python experiments/make_dryad_dataset.py <orderbook_data.sqlite>

The archive is opened read only and its sha256 is checked. It is not in this
repository (27 GB, CC0, DOI 10.5061/dryad.q2bvq83rn).

Selection rule, fixed before any control ran on the data: market `gdax`
(chosen for its known BTC-USD listing and quality alone), the level 1 book at
00:00 UTC, days with exactly one bid and one ask, bid < ask and both sizes of
at least one lot. Paths are taken in date order from runs of consecutive days,
each using 31 observations; a path's last day may be the next path's first.
Nothing is filled across a gap and no market is mixed in.
"""

from __future__ import annotations

import hashlib
import json
import sqlite3
import sys
from decimal import ROUND_FLOOR, Decimal
from pathlib import Path

SHA256 = "81cffe292e1f894bd78c3d14a37469c630f85bcefca3b6dc1300fef1e893f195"
MARKET = "gdax"
DAY = 86400
STEPS = 30
TICK = Decimal("0.01")
LOT = Decimal("0.0001")
OUT = Path("experiments/datasets/dryad-gdax-v1")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as file:
        while chunk := file.read(1 << 24):
            digest.update(chunk)
    return digest.hexdigest()


def on_tick(value: float) -> str:
    """The stored REAL back to its shortest decimal, which must be on the tick."""
    decimal = Decimal(repr(value))
    if decimal % TICK != 0:
        raise ValueError(f"price {value} is not on the tick")
    return f"{decimal.quantize(TICK)}"


def lots(value: float) -> int:
    return int((Decimal(repr(value)) / LOT).to_integral_value(rounding=ROUND_FLOOR))


def main(source: Path) -> None:
    if sha256(source) != SHA256:
        raise SystemExit("archive sha256 does not match the published one")
    connection = sqlite3.connect(f"file:{source}?mode=ro&immutable=1", uri=True)
    rows: dict[int, list[tuple[str, float, float]]] = {}
    query = (
        "SELECT CAST(ts AS INTEGER), side, price, size FROM orderbook_data "
        "WHERE exchange = ? AND level = 1 AND CAST(ts AS INTEGER) % ? = 0"
    )
    for ts, side, price, size in connection.execute(query, (MARKET, DAY)):
        rows.setdefault(ts, []).append((side, price, size))

    days = {}
    for ts, entries in rows.items():
        if sorted(entry[0] for entry in entries) != ["ask", "bid"]:
            continue
        book = {entry[0]: entry for entry in entries}
        bid, ask = book["bid"], book["ask"]
        if bid[1] < ask[1] and lots(bid[2]) >= 1 and lots(ask[2]) >= 1:
            days[ts] = (on_tick(bid[1]), on_tick(ask[1]), lots(bid[2]), lots(ask[2]))

    paths = []
    start = min(days)
    while start <= max(days):
        window = [start + step * DAY for step in range(STEPS + 1)]
        if all(ts in days for ts in window):
            paths.append(window)
            start = window[-1]
        else:
            start = min((ts for ts in days if ts > start), default=max(days) + 1)

    OUT.mkdir(parents=True, exist_ok=True)
    book_lines = ["path_id,step,timestamp,bid,ask,bid_size,ask_size"]
    execution_lines = ["path_id,step,fillable_buy,fillable_sell,reject"]
    for path_id, window in enumerate(paths):
        for step, ts in enumerate(window):
            bid, ask, bid_size, ask_size = days[ts]
            book_lines.append(f"{path_id},{step},{ts},{bid},{ask},{bid_size},{ask_size}")
            if step < STEPS:
                execution_lines.append(f"{path_id},{step},{ask_size},{bid_size},0")
    (OUT / "paths.csv").write_text("\n".join(book_lines) + "\n")
    (OUT / "execution.csv").write_text("\n".join(execution_lines) + "\n")

    manifest = {
        "contract": {
            "expiry_days": float(STEPS),
            "kind": "call",
            "strike": "atm-at-step-0",
            "style": "european",
        },
        "currency": "USD",
        "fee_asset": "quote",
        "fee_bps": 2.0,
        "files": {name: sha256(OUT / name) for name in ("execution.csv", "paths.csv")},
        "lot": float(LOT),
        "name": "dryad-gdax-v1",
        "queue_model": {
            "id": "visible-top-of-book-size",
            "limits": [
                "capacity is the visible level 1 size on the side consumed, as an assumption: the archive has no trades, so no fill is observed",
                "no queue position, latency, hidden liquidity or market impact is modelled; an order takes the touch price whatever its size",
                "reject is always 0: the archive records no rejections, so this is no evidence that none occurred",
                "tick, lot and fee are the trained configuration, not venue history: the archive has neither, and the 2 bp fee is an assumption",
                "the market's BTC-USD listing and the capture timing are not stated in the archive; the minute label is not an exchange time",
            ],
            "version": "1",
        },
        "rounding": "floor-lots",
        "seed": None,
        "source": f"Dryad doi:10.5061/dryad.q2bvq83rn, licence CC0, file 2816244, sha256 {SHA256}, market {MARKET}, level 1 at 00:00 UTC",
        "step_days": 1.0,
        "steps": STEPS,
        "synthetic": False,
        "tick": float(TICK),
        "timestamp_unit": "s",
        "version": "1",
    }
    (OUT / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(f"{len(paths)} paths, {len(days)} valid days of {len(rows)}")


if __name__ == "__main__":
    main(Path(sys.argv[1]))
