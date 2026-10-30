#!/usr/bin/env python3
"""freshwarm.py PROJECT [--source DIR] [--work DIR] [--edits none,body,signature] [--env NAME=VALUE]...
[--fresh-env NAME=VALUE]...:
what a build costs on a fresh machine whose only warm thing is the remote (#41).

A fresh CI machine has the checkout and the remote and nothing else: no home, no store, no
target directory, no daemon. So:
  seed     a checkout with an empty store and the remote: `blaze build`, the wait until
           ready, and `blaze stop`, which pushes what it compiled to the remote
  fresh-E  for each edit E (`none` first): the checkout copied again to the same path, with E
           written before anything runs, a home and a store of its own, empty, and the remote.
           Its `blaze build` is the time to a built program, the wait after it the time
           to ready.
For each step: the command's seconds, the wait until ready, the most memory held, what the
worker compiled and took from the store, its steps (load, resolve, compile, link) and the
threads' time per step (inference, MIR, monomorphization, translation). What each worker said
is in `WORK/said-STEP.txt`. With a program to run, its output is checked against the seed's.

PROJECT is one of bench.py's, fetched as it fetches them, or `blaze`: the tree at `--source`
(blaze's own checkout), built `-p blaze-engine --release`. Writes the steps as JSON on stdout's last
line.
"""
import json, os, re, shutil, sys, time

import bench, blaze_cli, memory
from blaze_cli import Side, fetch, program_output, wait_warm

BLAZE = dict(
    build=["-p", "blaze-engine", "--release"],
    run=["--version"],
    run_bin="blaze",
    edits=dict(
        # Low in the graph: hir-def is under hir-ty, hir, the code generator and the daemon.
        body=bench.body("crates/hir-def/src/lib.rs", "    pub fn is_block_module(self, db: &dyn SourceDatabase) -> bool {"),
        # The daemon's own crate, which nothing in the checkout depends on.
        top=bench.body("crates/blaze-engine/src/content.rs", "fn of(&self, krate: Crate) -> std::sync::Arc<OnceLock<Option<ContentId>>> {"),
        signature=[("crates/blaze-store/src/lib.rs", "pub fn parse_size(text: &str) -> Option<u64> {",
                    "pub fn parse_size(text: &str, _bench_edit: ()) -> Option<u64> {"),
                   ("crates/**/*.rs", "parse_size(&text)", "parse_size(&text, ())")],
    ),
)


def from_store(said):
    functions = re.findall(r"^store: (\d+) functions found, (\d+) compiled and added$", said, re.M)
    frozen = re.search(r"^frozen, of \d+ crates: def maps of (\d+).* from the store, (\d+) written", said, re.M)
    return dict(found=sum(int(f) for f, _ in functions), added=sum(int(a) for _, a in functions),
                frozen=int(frozen[1]) if frozen else 0, frozen_written=int(frozen[2]) if frozen else 0)


def main():
    argv = sys.argv[1:]
    work = os.path.realpath(bench.options(argv, "--work", "/tmp/freshwarm")[0])
    source = bench.options(argv, "--source", "")[0]
    edits = bench.options(argv, "--edits", "none,body,signature")[0].split(",")
    extra_env = dict(it.split("=", 1) for it in bench.options(argv, "--env"))
    # Set on the fresh machines and not the seed: a build configured otherwise, whose
    # manifest has to be another.
    fresh_env = dict(it.split("=", 1) for it in bench.options(argv, "--fresh-env"))
    name, = argv
    spec = BLAZE if name == "blaze" else bench.PROJECTS[name]
    source = os.path.realpath(source) if source else fetch(name, spec)
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    remote = os.path.join(work, "remote")
    os.makedirs(remote)
    build, run = spec.get("build", []), spec.get("run")

    def checkout(label, edit=None):
        # At one path, as a CI runner's checkout is: what is built names where its sources are.
        ws = os.path.join(work, "ws")
        shutil.rmtree(ws, ignore_errors=True)
        shutil.copytree(source, ws, symlinks=True, ignore=lambda at, names: [n for n in names if n in ("target", ".git")])
        if edit:
            assert bench.write_edit(ws, edit, False), f"{label}: the edit found nothing to change"
        # One home at one path too, emptied: the machine has nothing of the one before.
        home = os.path.join(work, "home")
        shutil.rmtree(home, ignore_errors=True)
        env = dict(blaze_cli.base_env(home), **spec.get("env", {}), **extra_env)
        if label != "seed":
            env.update(fresh_env)
        env.update(BLAZE_CACHE=os.path.join(home, "store"), BLAZE_STATS="1")
        env.pop("RUSTC_WRAPPER", None)
        return Side(ws, env, True)

    def cache(side, command):
        """`blaze-engine cache push|pull REMOTE`, as CI runs it around a build, timed."""
        # The seed pushes unsigned: what signing costs is the remote's (rcache), not this.
        env = dict(side.env, BLAZE_REMOTE_UNSIGNED="1")
        took, ran, _ = memory.run([blaze_cli.BLAZE, "cache", command, remote], side.ws, env, 3600)
        assert ran.returncode == 0, f"cache {command}: {ran.stderr[-3000:]}"
        return round(took, 3), ran.stderr.strip().splitlines()[-3:]

    def stopped(side, daemon):
        """Stops the daemon of `side` and waits until it and its workers are gone, with what
        they write to the store written."""
        pids = [daemon.pid(), *(pid for pid, _ in daemon.workers())]
        side.cargo("stop")
        for _ in range(1200):
            if not any(pid and os.path.exists(f"/proc/{pid}") for pid in pids):
                return
            time.sleep(0.1)
        raise AssertionError(f"the daemon did not end: {pids}")

    def output(side):
        if run is None:
            return None
        if "run_bin" in spec:
            profile = "release" if "--release" in build else "debug"
            ran = memory.run([os.path.join(side.ws, "target", profile, spec["run_bin"]), *run], side.ws, side.env, 600)[1]
            return program_output(ran)
        return program_output(side.cargo("run", *build, "--", *run)[1])

    steps = []

    def step(label, side):
        pulled = cache(side, "pull") if label != "seed" else None
        side.env["BLAZE_PROFILE"] = os.path.join(work, f"profile-{label}")
        daemon = bench.Daemon(side)
        with memory.Sampler() as sampler:
            took, ran, used = bench.metered(side, build, sampler=sampler)
            assert ran.returncode == 0, f"{label}: {ran.stderr[-3000:]}"
            ready, states = wait_warm(side, side.ws)
        said = daemon.said()
        with open(os.path.join(work, f"said-{label}.txt"), "w") as kept:
            kept.write(said)
        with open(os.path.join(work, f"client-{label}.txt"), "w") as kept:
            kept.write(ran.stderr)
        row = dict(step=label, build_s=round(took, 3), ready_s=round(ready, 3), states=states,
                   built=bench.how_built(ran.stderr), peak_mb=max(sampler.peak_mb, used["largest_mb"]),
                   resident_mb=daemon.resident_mb(), compiled=bench.compiled(said),
                   steps_s=bench.cold_steps(said), threads_s=bench.thread_steps(said, side.env["BLAZE_PROFILE"]),
                   phases=re.findall(r"^phases: (.*)$", said, re.M), store=from_store(said),
                   manifest=re.findall(r"^manifest: (.*)$", said, re.M), pull=pulled)
        row["output"] = output(side)
        whole = os.path.join(side.ws, "target", "blaze", "whole")
        if os.path.isdir(whole):
            shutil.copytree(whole, os.path.join(work, f"whole-{label}"), dirs_exist_ok=True)
        stopped(side, daemon)
        if label == "seed":
            row["push"] = cache(side, "push")
        row["store_mb"] = round(sum(os.path.getsize(os.path.join(d, f)) for d, _, fs in os.walk(side.env["BLAZE_CACHE"]) for f in fs) / 2**20, 1)
        steps.append(row)
        print(f"{label:14} pull {pulled[0] if pulled else 0:6.2f}s  build {row['build_s']:8.2f}s  ready {row['ready_s']:7.2f}s  peak {row['peak_mb']:6} MB  "
              f"compiled {row['compiled']}  store {row['store']['found']} found {row['store']['added']} added  "
              f"frozen {row['store']['frozen']}  store {row['store_mb']} MB  manifest {row['manifest']}  "
              f"steps {row['steps_s']}  threads {row['threads_s']}", flush=True)
        return row

    seed = step("seed", checkout("seed"))
    for edit in edits:
        # `none`, `none2`, ...: the same checkout again, on another fresh machine.
        unedited = edit.startswith("none")
        row = step(f"fresh-{edit}", checkout(f"fresh-{edit}", None if unedited else spec["edits"][edit]))
        if unedited and seed["output"] is not None:
            row["right"] = row["output"] == seed["output"]
    for row in steps:
        row.pop("output", None)
    print(json.dumps(dict(project=name, steps=steps)))
    return 0 if all(it.get("right", True) for it in steps) else 1


if __name__ == "__main__":
    sys.exit(main())
