"""The README's tables from the raw results: python3 summary.py '{"raw": "raw"}'

Each cell is the median of the runs, with every run's value after it; `raw/<condition>/` holds
one `<arm>-<run>.json` per run (run.py).
"""

from __future__ import annotations

import dataclasses
import json
import statistics
import sys
from pathlib import Path


@dataclasses.dataclass(frozen=True)
class Input:
    raw: str


def parse_input(raw: str) -> Input:
    v = json.loads(raw)
    if not isinstance(v, dict) or set(v) != {"raw"} or not isinstance(v["raw"], str):
        raise SystemExit('summary.py: the input is {"raw": "<dir>"}')
    return Input(**v)


def get(d: dict, path: str):
    for k in path.split("."):
        if d is None:
            return None
        d = d.get(k)
    return d


# (row, path into a run's JSON, scale, digits)
ROWS = [
    ("first frame (ms)", "setup.firstFrameMs", 1, 0),
    ("layout settled (s)¹", "settle.stepSettledMs", 1e-3, 0),
    ("layout steps to settle", None, 1, 0),
    ("ms per layout step, alone²", None, 1, 0),
    ("pan/zoom fps", "panzoom.fps", 1, 2),
    ("pan/zoom frame p50 (ms)", "panzoom.p50ms", 1, 0),
    ("renderer main thread per frame, pan/zoom (ms)³", "panzoom.rendererRafMsPerCall", 1, 2),
    ("live: fps", "live.fps", 1, 2),
    ("live: ms per update call⁴", "live.updateMeanMs", 1, 1),
    ("live: updates done per 100 asked for", "live.updates", 1, 0),
    ("live: renderer main thread per frame (ms)³", "live.rendererRafMsPerCall", 1, 1),
    ("live while moving: fps", "live_moving.fps", 1, 2),
    ("JS heap after layout (MB)", "memory_after_settle.usedJSHeapMB", 1, 0),
    ("browser RSS after layout (MB)⁵", "rss_after_settle_mb", 1, 0),
    ("browser RSS at end (MB)⁵", "rss_end_mb", 1, 0),
]


def value(run: dict, row: str, path: str | None):
    if row == "layout steps to settle":
        return get(run, "settle.stepSettledTicks") if run["arm"] == "cosmos" else get(run, "settle.stepSettledIterations")
    if row.startswith("ms per layout step"):
        return get(run, "sim_only.msPerStep") if run["arm"] == "cosmos" else get(run, "settle.iterationMedianMs")
    if row.startswith("live: updates done"):
        v = get(run, path)
        return None if v is None else v * 100 / (get(run, "live.seconds") * 10)
    return get(run, path)


def cell(vals: list, scale: float, digits: int) -> str:
    if any(v is None for v in vals):
        shown = ["—" if v is None else f"{v * scale:.{digits}f}" for v in vals]
        return "not reached (" + ", ".join(shown) + ")"
    xs = [v * scale for v in vals]
    med = statistics.median(xs)
    return f"**{med:.{digits}f}** ({', '.join(f'{x:.{digits}f}' for x in xs)})"


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    raw = Path(parse_input(sys.argv[1]).raw)
    for cond in sorted(p for p in raw.iterdir() if p.is_dir()):
        if cond.name.startswith("probe-"):
            # vm-probe.sh's runs shorten every other phase; only their probe counts.
            for f in sorted(cond.glob("cosmos-*.json")):
                run = json.loads(f.read_text())
                probe = run["probe_live"]
                print(f"### {cond.name}: a cosmos.gl live update after gl.finish(), {run['setup']['prep']['nodes']:,} functions\n")
                for k in ("finishMs", "colorsMs", "sizesMs", "renderMs"):
                    xs = [p[k] for p in probe]
                    print(f"- {k}: median {statistics.median(xs):.1f}, {min(xs):.1f} to {max(xs):.1f} over {len(xs)}")
                print()
            continue
        runs = {arm: [json.loads(f.read_text()) for f in sorted(cond.glob(f"{arm}-*.json"))] for arm in ("cosmos", "sigma")}
        if not runs["cosmos"] and not runs["sigma"]:
            continue
        print(f"### {cond.name}\n")
        prep = (runs["cosmos"] or runs["sigma"])[0]["setup"]["prep"]
        print(f"{prep['nodes']:,} functions, {prep['edges']:,} calls, {prep['modules']:,} modules; "
              f"{len(runs['cosmos'])} cosmos.gl runs, {len(runs['sigma'])} Sigma.js runs.\n")
        print("| | cosmos.gl 3.5.1 | Sigma.js 4.0.4 + FA2 worker |")
        print("|---|---|---|")
        for row, path, scale, digits in ROWS:
            cells = []
            for arm in ("cosmos", "sigma"):
                rs = runs[arm]
                if not rs:
                    cells.append("")
                    continue
                vals = [value(r, row, path) for r in rs]
                if row.startswith("layout settled"):
                    capped = [r["settle"].get("capped") for r in rs]
                    c = cell(vals, scale, digits)
                    cells.append(c + (" capped" if all(capped) and any(v is None for v in vals) else ""))
                else:
                    cells.append(cell(vals, scale, digits))
            print(f"| {row} | {cells[0]} | {cells[1]} |")
        problems = {arm: sorted({e for r in rs for e in r["problems"]["errors"] + r["problems"]["violations"]}) for arm, rs in runs.items()}
        for arm, ps in problems.items():
            if ps:
                print(f"\n{arm} page errors: " + "; ".join(p[:120] for p in ps))
        print()


if __name__ == "__main__":
    main()
