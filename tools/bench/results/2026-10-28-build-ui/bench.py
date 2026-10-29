"""Build events' cost on a small project, before and after (lane tui).

    bench.py BEFORE AFTER OUT

BEFORE and AFTER are blaze binaries. Each arm builds a project of its own (anim: one binary, no
dependencies) with a `BLAZE_HOME` of its own and nothing of rustup's, cargo's or blaze's in
the environment: the first build (cold, the standard library made once), 3 warm-up no-op
builds, then 30 rounds of a no-op `blaze build` alternated across the arms, then 10 rounds of
a body edited, each a fresh process timed whole from Python. Output is piped, as in CI: the
renderer is the plain one. OUT gets report.json with every run.
"""
import collections
import json
import os
import statistics
import subprocess
import sys
import time

arms = {"before": sys.argv[1], "after": sys.argv[2]}
out = sys.argv[3]
os.makedirs(out, exist_ok=True)
base_env = {k: v for k, v in os.environ.items() if not k.startswith(("RUSTUP_", "CARGO", "RUSTC", "BLAZE_"))}
MAIN = 'fn main() {{\n    let _edit = {n}u32;\n    println!("anim {{}}", 6 * 7);\n}}\n'


def project(arm):
    d = os.path.join(out, f"anim-{arm}")
    if not os.path.exists(d):
        os.makedirs(d + "/src")
        open(d + "/Cargo.toml", "w").write('[package]\nname = "anim"\nversion = "0.1.0"\nedition = "2021"\n')
        open(d + "/src/main.rs", "w").write(MAIN.format(n=0))
    return d


def run(arm, *args):
    env = dict(base_env, BLAZE_HOME=os.path.join(out, f"home-{arm}"))
    t = time.perf_counter()
    p = subprocess.run([arms[arm], "build", *args], cwd=project(arm), env=env, capture_output=True, text=True)
    ms = (time.perf_counter() - t) * 1000
    if p.returncode != 0:
        print(arm, "FAILED", p.returncode, p.stderr[-2000:], flush=True)
    return ms


def summary(runs):
    t = sorted(runs)
    return dict(median=round(statistics.median(t), 2), min=round(t[0], 2), p90=round(t[int(len(t) * 0.9)], 2),
                runs=[round(x, 2) for x in runs])


report = {"arms": {arm: {"cold_ms": round(run(arm), 1)} for arm in arms}}
for _ in range(3):
    for arm in arms:
        run(arm)
times = collections.defaultdict(lambda: collections.defaultdict(list))
for _ in range(30):
    for arm in arms:
        times[arm]["noop"].append(run(arm))
for n in range(1, 11):
    for arm in arms:
        open(project(arm) + "/src/main.rs", "w").write(MAIN.format(n=n))
        times[arm]["body"].append(run(arm))
for arm in arms:
    for phase, runs in times[arm].items():
        report["arms"][arm][phase] = summary(runs)
    subprocess.run([arms[arm], "stop"], env=dict(base_env, BLAZE_HOME=os.path.join(out, f"home-{arm}")), capture_output=True)
json.dump(report, open(os.path.join(out, "report.json"), "w"), indent=1)
print(json.dumps({a: {p: (v if not isinstance(v, dict) else {k: x for k, x in v.items() if k != "runs"}) for p, v in r.items()}
                  for a, r in report["arms"].items()}, indent=1))
