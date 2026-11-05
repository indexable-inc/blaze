#!/usr/bin/env python3
"""Where `affected` costs time on a fresh runner: build (--no-run) and test run, timed apart, all vs affected,
ABAB, each on a fresh target and daemon with the warm store, after an edit no pass is kept for."""
import json, os, shutil, subprocess, sys, time
sys.path.insert(0, "/root/tc/src/harness")
import bench, blaze_cli
APPS, BIN = "/root/apps", "/root/tc/target/release"
rows = []
for name in sys.argv[1:]:
    ws = os.path.join(APPS, name)
    subprocess.run(["git", "checkout", "-q", "--", "."], cwd=ws)
    (path, old, new), = bench.PROJECTS[name]["edits"]["leaf"]
    text = open(os.path.join(ws, path)).read()
    env = dict(os.environ, PATH=f"{BIN}:{os.environ['PATH']}", BLAZE_HOME=f"/tmp/rjh3-{name}", BLAZE_CACHE=f"{APPS}/store2",
               RUST_BACKTRACE="0", HOME="/root", BLAZE_MANIFEST="off")
    env.pop("CARGO_TARGET_DIR", None)
    args = blaze_cli.PROJECTS[name].get("test", [])
    for round_ in range(2):
        for mode in ("all", "affected"):
            # A change of code no run has a pass for.
            mark = f"std::hint::black_box({100 + 2 * round_ + (mode == 'affected')}u32);"
            open(os.path.join(ws, path), "w").write(text.replace(old, new.replace("let _bench_edit = 1u32;", mark), 1))
            subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
            shutil.rmtree(os.path.join(ws, "target"), ignore_errors=True)
            e = dict(env, BLAZE_TESTS=mode)
            t0 = time.monotonic()
            b = subprocess.run(["blaze", "test", *args, "--no-run"], cwd=ws, env=e, capture_output=True, text=True, timeout=3600)
            t1 = time.monotonic()
            r = subprocess.run(["blaze", "test", *args, "--no-fail-fast"], cwd=ws, env=e, capture_output=True, text=True, timeout=3600)
            t2 = time.monotonic()
            open(f"{APPS}/{name}.split-{mode}-{round_}.log", "w").write(b.stdout + b.stderr + "\n=====\n" + r.stdout + r.stderr)
            row = dict(name=name, round=round_, mode=mode, build=round(t1 - t0, 1), run=round(t2 - t1, 1), build_rc=b.returncode, rc=r.returncode,
                       ran=(r.stdout + r.stderr).count(" ... ok") + (r.stdout + r.stderr).count(" ... FAILED"))
            rows.append(row); print(json.dumps(row), flush=True)
    subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    subprocess.run(["git", "checkout", "-q", "--", "."], cwd=ws)
json.dump(rows, open(f"{APPS}/split.json", "w"), indent=1)
print("DONE")
