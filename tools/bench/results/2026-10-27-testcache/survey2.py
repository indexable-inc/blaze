#!/usr/bin/env python3
"""Lane testcache: a one-function PR's test job on a fresh runner, with test results cold vs warm
from main. Per app: main (cold store, `affected`, keeps the passes); then bench.py's `leaf` edit,
and three PR-shaped runs, each on a fresh target and daemon with main's store: `all` (results
cold), `affected` (results warm from main), `verify` (every test, cached passes checked).
Runs in the lane's VM: nix develop /root/tc/src -c python3 /root/tc/survey2.py [names]"""
import json, os, re, shutil, subprocess, sys, time

sys.path.insert(0, "/root/tc/src/harness")
import bench  # noqa: E402
import blaze_cli  # noqa: E402

BIN = "/root/tc/target/release"
APPS = "/root/apps"
OUT = f"{APPS}/survey2.json"
PICK = ["ripgrep", "fd", "just", "hyperfine", "axum", "cargo", "nushell"]
RAN = re.compile(r"^test (\S+) \.\.\. (ok|ignored|FAILED)", re.M)
CACHED = re.compile(r"Cached (\d+) tests? from ([^;\n]*)")
VERIFIED = re.compile(r"Verified the passes kept: (\d+) of (\d+) tests passed again")
DENIED = re.compile(r"fails: the sandbox denied it")


def run(ws, env, args, log):
    start = time.monotonic()
    out = subprocess.run(["blaze", "test", *args, "--no-fail-fast"], cwd=ws, env=env,
                         capture_output=True, text=True, timeout=3600)
    took = time.monotonic() - start
    said = out.stdout + out.stderr
    open(log, "w").write(said)
    ran = RAN.findall(said)
    cached = CACHED.findall(said)
    return dict(code=out.returncode, secs=round(took, 1), ran=len(ran),
                failed=sum(1 for _, e in ran if e == "FAILED"),
                cached=sum(int(n) for n, _ in cached), cached_from=sorted({f for _, f in cached}),
                verified=VERIFIED.findall(said), denied=len(DENIED.findall(said)),
                served="[blaze]" in said)


def fresh(ws, env):
    subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    shutil.rmtree(os.path.join(ws, "target"), ignore_errors=True)


def main():
    names = sys.argv[1:] or PICK
    results = json.load(open(OUT)) if os.path.exists(OUT) else {}
    for name in names:
        if name in results:
            continue
        spec = blaze_cli.PROJECTS[name]
        ws = os.path.join(APPS, name)
        if not os.path.isdir(ws):
            subprocess.run(["git", "clone", "-q", "--depth", "1", "--branch", spec["tag"],
                            f"https://github.com/{spec['repo']}", ws], check=True)
        subprocess.run(["git", "checkout", "-q", "--", "."], cwd=ws)
        (path, old, new), = bench.PROJECTS[name]["edits"]["leaf"]
        # bench.py's unused local compiles to the same code; this changes the function's code
        # and nothing it does.
        new = new.replace("let _bench_edit = 1u32;", "std::hint::black_box(1u32);")
        text = open(os.path.join(ws, path)).read()
        if old not in text:
            results[name] = {"error": f"leaf edit not found in {path}"}
            continue
        env = dict(os.environ, PATH=f"{BIN}:{os.environ['PATH']}", BLAZE_HOME=f"/tmp/rjh2-{name}",
                   BLAZE_CACHE=f"{APPS}/store2", RUST_BACKTRACE="0", HOME="/root",
                   # The build manifest (#550) parses `.md` inputs as Rust on a fresh target with a warm store.
                   BLAZE_MANIFEST="off", BLAZE_TESTS_FROM=f"main of {name}")
        env.pop("CARGO_TARGET_DIR", None)
        args = spec.get("test", [])
        rows = {}
        fresh(ws, env)
        rows["main"] = run(ws, dict(env, BLAZE_TESTS="affected"), args, f"{APPS}/{name}.main.log")
        print(name, "main", json.dumps(rows["main"]), flush=True)
        open(os.path.join(ws, path), "w").write(text.replace(old, new, 1))
        pr_env = dict(env, BLAZE_TESTS_FROM=f"PR of {name}")
        for mode in ("all", "affected", "verify"):
            fresh(ws, env)
            rows[f"pr {mode}"] = run(ws, dict(pr_env, BLAZE_TESTS=mode), args, f"{APPS}/{name}.pr-{mode}.log")
            print(name, "pr", mode, json.dumps(rows[f"pr {mode}"]), flush=True)
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
        subprocess.run(["git", "checkout", "-q", "--", "."], cwd=ws)
        results[name] = {"edit": f"{path}: {old}", **rows}
        json.dump(results, open(OUT, "w"), indent=1)
    print("DONE")


if __name__ == "__main__":
    main()
