"""What the functions' records cost a build of anim, before and after (lane web).

    bench.py INPUT.json

INPUT is a `Bench` as JSON. Each arm is a blaze binary building its own copy of `project` with a
`BLAZE_HOME` and a store of its own. The arms take turns throughout: `colds` cold builds each
(home, store and target directory removed before each), then, with the last cold's daemon, 3
warm-up no-op builds, `noops` no-op builds and `edits` body edits of `edited` (a statement added
after the line `opening`). Each is a fresh process timed whole from Python, output piped.
`out/report.json` has every run.
"""
import dataclasses
import json
import os
import shutil
import statistics
import subprocess
import sys
import time


@dataclasses.dataclass(frozen=True)
class Bench:
    arms: dict
    project: str
    out: str
    edited: str
    opening: str
    colds: int = 2
    noops: int = 30
    edits: int = 8
    env: dict = dataclasses.field(default_factory=dict)


def load(path):
    raw = json.load(open(path))
    known = {f.name for f in dataclasses.fields(Bench)}
    unknown = set(raw) - known
    if unknown:
        sys.exit(f"{path}: unknown fields {sorted(unknown)}")
    return Bench(**raw)


bench = load(sys.argv[1])
os.makedirs(bench.out, exist_ok=True)
base_env = {k: v for k, v in os.environ.items() if not k.startswith(("RUSTUP_", "CARGO", "RUSTC", "BLAZE_"))}
base_env.update(bench.env)


def paths(arm):
    return {k: os.path.join(bench.out, f"{k}-{arm}") for k in ("home", "store", "ws")}


def env(arm):
    p = paths(arm)
    return dict(base_env, BLAZE_HOME=p["home"], BLAZE_CACHE=p["store"])


def run(arm, *args):
    t = time.perf_counter()
    p = subprocess.run([bench.arms[arm], *args], cwd=paths(arm)["ws"], env=env(arm), capture_output=True, text=True)
    ms = (time.perf_counter() - t) * 1000
    if p.returncode != 0:
        sys.exit(f"{arm} {args} failed {p.returncode}: {p.stderr[-3000:]}")
    return ms


def fresh(arm):
    subprocess.run([bench.arms[arm], "stop"], env=env(arm), capture_output=True)
    for path in paths(arm).values():
        shutil.rmtree(path, ignore_errors=True)
    shutil.copytree(bench.project, paths(arm)["ws"], ignore=shutil.ignore_patterns("target"))


def edit(arm, n):
    file = os.path.join(paths(arm)["ws"], bench.edited)
    text = open(file).read()
    lines = [line for line in text.split("\n") if "_web_edit" not in line]
    at = lines.index(bench.opening)
    lines.insert(at + 1, f"    let _web_edit = {n}u32;")
    open(file, "w").write("\n".join(lines))


def summary(runs):
    t = sorted(runs)
    return dict(median=round(statistics.median(t), 2), min=round(t[0], 2), p90=round(t[int(len(t) * 0.9)], 2),
                runs=[round(x, 2) for x in runs])


times = {arm: {"cold": [], "noop": [], "body": []} for arm in bench.arms}
for _ in range(bench.colds):
    for arm in bench.arms:
        fresh(arm)
        times[arm]["cold"].append(run(arm, "build"))
        print(arm, "cold", round(times[arm]["cold"][-1]), "ms", flush=True)
for _ in range(3):
    for arm in bench.arms:
        run(arm, "build")
for _ in range(bench.noops):
    for arm in bench.arms:
        times[arm]["noop"].append(run(arm, "build"))
for n in range(1, bench.edits + 1):
    for arm in bench.arms:
        edit(arm, n)
        times[arm]["body"].append(run(arm, "build"))
report = {"arms": {arm: {phase: summary(runs) for phase, runs in phases.items()} for arm, phases in times.items()}}
for arm in bench.arms:
    subprocess.run([bench.arms[arm], "stop"], env=env(arm), capture_output=True)
json.dump(report, open(os.path.join(bench.out, "report.json"), "w"), indent=1)
print(json.dumps({a: {p: {k: x for k, x in v.items() if k != "runs"} for p, v in r.items()}
                  for a, r in report["arms"].items()}, indent=1))
