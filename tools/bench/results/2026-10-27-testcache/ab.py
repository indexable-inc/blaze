#!/usr/bin/env python3
"""Lane testcache: the tests' walk by stable names, before against after. Per app and binary: main's run on
a store of the app's own (cold), kept as a snapshot; then a one-function PR, each run on a fresh target,
daemon, BLAZE_HOME and a copy of that snapshot: `all` and `affected`, twice, ABAB. Each run's
`tests-reach-us` is read from its daemon's log.
Runs in the lane's VM: nix develop /root/tc/src -c python3 /root/tc/ab.py [names]"""
import glob, json, os, re, shutil, subprocess, sys, time

sys.path.insert(0, "/root/tc/src/harness")
import bench  # noqa: E402
import blaze_cli  # noqa: E402

APPS = "/root/apps"
OUT = f"{APPS}/ab.json"
BINS = {"before": "/root/tc/bin-before", "after": "/root/tc/bin-after"}
RAN = re.compile(r"^test (\S+) \.\.\. (ok|ignored|FAILED)", re.M)
CACHED = re.compile(r"Cached (\d+) tests? from")
REACH = re.compile(r"tests-reach-us: (\d+)")


def run(ws, env, args, log):
    start = time.monotonic()
    out = subprocess.run(["blaze", "test", *args, "--no-fail-fast"], cwd=ws, env=env,
                         capture_output=True, text=True, timeout=3600)
    took = time.monotonic() - start
    subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    said = out.stdout + out.stderr
    open(log, "w").write(said)
    logs = glob.glob(os.path.join(env["BLAZE_HOME"], "workers", "*", "log"))
    reach = [int(n) for path in logs for n in REACH.findall(open(path, errors="replace").read())]
    ran = RAN.findall(said)
    return dict(code=out.returncode, secs=round(took, 1), ran=len(ran),
                failed=sum(1 for _, e in ran if e == "FAILED"),
                cached=sum(int(n) for n in CACHED.findall(said)), reach_us=reach)


def main():
    names = sys.argv[1:]
    results = json.load(open(OUT)) if os.path.exists(OUT) else {}
    for name in names:
        spec = blaze_cli.PROJECTS[name]
        ws = os.path.join(APPS, name)
        args = spec.get("test", [])
        (path, old, new), = bench.PROJECTS[name]["edits"]["leaf"]
        new = new.replace("let _bench_edit = 1u32;", "std::hint::black_box(1u32);")
        for bin_name, bin_dir in BINS.items():
            label = f"{name} {bin_name}"
            if label in results:
                continue
            store, snap = f"{APPS}/ab-store-{name}-{bin_name}", f"{APPS}/ab-snap-{name}-{bin_name}"
            for it in (store, snap):
                shutil.rmtree(it, ignore_errors=True)
            env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}", BLAZE_CACHE=store,
                       RUST_BACKTRACE="0", HOME="/root", BLAZE_STATS="1", BLAZE_MANIFEST="off")
            env.pop("CARGO_TARGET_DIR", None)
            count = 0

            def fresh(mode, tag):
                nonlocal count
                count += 1
                home = f"/tmp/ab-{name}-{bin_name}-{count}"
                shutil.rmtree(home, ignore_errors=True)
                shutil.rmtree(os.path.join(ws, "target"), ignore_errors=True)
                e = dict(env, BLAZE_HOME=home, BLAZE_TESTS=mode, BLAZE_TESTS_FROM=tag)
                return e

            subprocess.run(["git", "checkout", "-q", "--", "."], cwd=ws)
            rows = {"main": run(ws, fresh("affected", f"main of {name}"), args, f"{APPS}/{name}.ab-{bin_name}-main.log")}
            print(label, "main", json.dumps(rows["main"]), flush=True)
            shutil.copytree(store, snap)
            text = open(os.path.join(ws, path)).read()
            open(os.path.join(ws, path), "w").write(text.replace(old, new, 1))
            for round_ in range(2):
                for mode in ("all", "affected"):
                    shutil.rmtree(store)
                    shutil.copytree(snap, store)
                    row = run(ws, fresh(mode, f"PR of {name}"), args, f"{APPS}/{name}.ab-{bin_name}-{mode}-{round_}.log")
                    rows[f"{mode} {round_}"] = row
                    print(label, mode, round_, json.dumps(row), flush=True)
            subprocess.run(["git", "checkout", "-q", "--", "."], cwd=ws)
            for it in (store, snap):
                shutil.rmtree(it, ignore_errors=True)
            results[label] = rows
            json.dump(results, open(OUT, "w"), indent=1)
    print("DONE")


if __name__ == "__main__":
    main()
