#!/usr/bin/env python3
"""bench.py ARM SHAPE ROUND JOB: one measured run, a row appended to /root/rb/results.tsv.

ARM   cargo (warm target dir of main, tree /root/rb/c), cold (blaze, empty store), warm (blaze,
      a copy of the store main's cold run of the same JOB left), remote (blaze, empty store filled by
      `blaze-engine cache pull` from the gateway, where main's cold run pushed it; the pull is timed apart
      and counted in the total), fill (blaze, the store /root/rb/fill-JOB kept across runs: repeated
      until one succeeds, it becomes main's store and is pushed, as cold a 1 does).
/root/rb/bench.env (KEY=VALUE lines) is added to every blaze arm's environment.
SHAPE a (no edit), b (body.patch: Symbol::intern's body), c (sig.patch: Symbol::empty's signature).
JOB   build (cargo build -p blaze-engine --release --features llvm) or test (the unit tests, as
      harness/unittests.py runs them: one cargo test of its crates).

Blaze runs are each a fresh CI job: a fresh copy of the tree, an empty target dir, a new
BLAZE_HOME, and the store. Run inside `nix develop`."""
import os, shutil, subprocess, sys, time
from pathlib import Path

RB = Path("/root/rb")
SRC, BIN, CTREE = RB / "src", RB / "bin", RB / "c"
PATCH = {"a": None, "b": RB / "body.patch", "c": RB / "sig.patch"}
CRATES = ["hir-ty-codegen", "blaze-store", "blaze-registry", "blaze-resolve", "blaze-build-script", "blaze-manifest", "blaze-in-place", "blaze-splice", "blaze-cargo", "blaze-engine",
          "hir-ty", "hir-ty-mir", "hir-ty-borrowck", "hir-ty-wfcheck", "hir-def", "parser", "syntax", "hir-ide", "ide-diagnostics"]
# Stack overflows of blaze-built hir-ide (selfhost lane's), skipped in both arms.
SKIP = ["--", "--skip", "infer_std_crash_5", "--skip", "recursive_vars"]
BOUND = 4 * 3600
SPEC = "blaze+https://blaze-gateway.indexable.workers.dev/t/rcache-test/v3"


def remote_env(env):
    """The gateway's token, signing key and trust, from files never printed."""
    env = dict(env, BLAZE_REMOTE_REQUIRED="1", BLAZE_REMOTE_TOKEN=Path("/root/.rcache-token").read_text().strip(),
               BLAZE_REMOTE_TRUST=Path("/root/.rcache-pub").read_text().strip())
    if Path("/root/.rcache-sign").exists():
        env["BLAZE_REMOTE_SIGN"] = "/root/.rcache-sign"
    return env


def sh(argv, cwd, env, log):
    with open(log, "ab") as out:
        out.write(f"\n$ {' '.join(map(str, argv))}  (cwd {cwd})\n".encode())
        out.flush()
        start = time.monotonic()
        try:
            rc = subprocess.run(list(map(str, argv)), cwd=cwd, env=env, stdout=out, stderr=out, timeout=BOUND).returncode
        except subprocess.TimeoutExpired:
            rc = "timeout"
        secs = time.monotonic() - start
        out.write(f"RC={rc} secs={secs:.1f}\n".encode())
    return rc, secs


def commands(tree, tool):
    if JOB == "build":
        return [(tree, [*tool, "build", "-p", "blaze-engine", "--release", "--features", "llvm"] + (["--locked"] if tool == ["cargo"] else []))]
    test = [*tool, "test", "--release", "--no-fail-fast"]
    return [(tree, [*test, *(f"-p{c}" for c in CRATES), *SKIP])]


def du(p):
    return int(subprocess.run(["du", "-sb", str(p)], capture_output=True, text=True).stdout.split()[0]) if p.exists() else 0


def git(tree, *args):
    subprocess.run(["git", "-C", str(tree), *args], check=True)


def cargo_tree():
    """Moves /root/rb/c by one edit of SHAPE: applies its patch to main, or takes it off again."""
    state_file = RB / "c.state"
    state = state_file.read_text().strip() if state_file.exists() else "a"
    note = "no edit"
    if SHAPE != "a":
        if state == SHAPE:
            git(CTREE, "checkout", "--", ".")
            state, note = "a", f"reverted {SHAPE}"
        else:
            assert state == "a", f"c is at {state}: put it back to main (and rebuild) first"
            git(CTREE, "apply", str(PATCH[SHAPE]))
            state, note = SHAPE, f"applied {SHAPE}"
        state_file.write_text(state)
    return note


def main():
    global ARM, SHAPE, JOB
    ARM, SHAPE, rnd, JOB = sys.argv[1:5]
    run = RB / "runs" / f"{ARM}-{SHAPE}-{JOB}-r{rnd}"
    shutil.rmtree(run, ignore_errors=True)
    run.mkdir(parents=True)
    log = run / "log"
    env = dict(os.environ, RUST_BACKTRACE="0", CARGO_INCREMENTAL="0")
    for k in ("RUSTC_WRAPPER", "BLAZE_WHOLE_BUILD"):
        env.pop(k, None)
    store = run / "store"
    if ARM == "cargo":
        # One edit for both jobs: build moves the tree, test sees the same edit.
        note = cargo_tree() if JOB == "build" else "same tree as build"
        tree, tool = CTREE, ["cargo"]
        env["CARGO_TARGET_DIR"] = str(CTREE / "target")
    else:
        tree = RB / "b"
        shutil.rmtree(tree, ignore_errors=True)
        shutil.copytree(SRC, tree, symlinks=True, ignore=shutil.ignore_patterns("target"))
        if PATCH[SHAPE]:
            git(tree, "apply", str(PATCH[SHAPE]))
        if ARM == "warm":
            base = RB / f"store-{JOB}-main"
            assert base.exists(), base
            shutil.copytree(base, store, symlinks=True)
        elif ARM == "fill":
            # main's store made by runs that each go on from what the last one sealed before a
            # 16 GB machine ran out of memory: the same entries one larger run writes.
            store = RB / f"fill-{JOB}"
            store.mkdir(exist_ok=True)
        else:
            store.mkdir()
        home = run / "home"
        home.mkdir(mode=0o700)
        env.update(PATH=f"{BIN}:{env['PATH']}", BLAZE_HOME=str(home), BLAZE_CACHE=str(store),
                   BLAZE_CACHE_MAX="off", BLAZE_STATS="1", BLAZE_TRACE=str(run / "trace"),
                   CARGO_TARGET_DIR=str(tree / "target"))
        knobs = RB / "bench.env"
        if knobs.exists():
            env.update(line.split("=", 1) for line in knobs.read_text().split() if "=" in line)
        tool = ["blaze"]
    total, rcs = 0.0, []
    if ARM == "remote":
        rc, secs = sh(["blaze-engine", "cache", "pull", SPEC, f"blaze-bench-{JOB}"], tree, remote_env(env), log)
        total += secs
        rcs.append(f"pull{rc}:{secs:.1f}")
    if ARM != "cargo":
        note = f"store in {du(store)}"
    for cwd, argv in commands(tree, tool):
        rc, secs = sh(argv, cwd, env, log)
        total += secs
        rcs.append(f"{rc}:{secs:.1f}")
    if ARM != "cargo":
        sh(["blaze", "stop"], tree, env, log)
    size = du(store) if ARM != "cargo" else 0
    cgroup = open("/proc/self/cgroup").read().strip().split("::", 1)[1]
    peaks = [Path(f"/sys/fs/cgroup{cgroup}/memory.{it}") for it in ("peak", "swap.peak")]
    peak = "/".join(it.read_text().strip() if it.exists() else "?" for it in peaks)
    row = [time.strftime("%Y-%m-%dT%H:%M:%S"), ARM, SHAPE, JOB, rnd, f"{total:.1f}", ",".join(rcs), str(size), peak, note]
    with open(RB / "results.tsv", "a") as f:
        f.write("\t".join(row) + "\n")
    print("\t".join(row), flush=True)
    if (ARM == "fill" or ARM == "cold" and SHAPE == "a" and rnd == "1") and all(it.startswith("0:") for it in rcs):
        base = RB / f"store-{JOB}-main"
        shutil.rmtree(base, ignore_errors=True)
        shutil.copytree(store, base, symlinks=True)
        rc, secs = sh(["blaze-engine", "cache", "push", SPEC, f"blaze-bench-{JOB}"], tree, remote_env(env), log)
        row = [time.strftime("%Y-%m-%dT%H:%M:%S"), "push", SHAPE, JOB, rnd, f"{secs:.1f}", f"{rc}:{secs:.1f}", str(size), "", "push of main's store"]
        with open(RB / "results.tsv", "a") as f:
            f.write("\t".join(row) + "\n")


main()
