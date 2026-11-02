#!/usr/bin/env python3
"""warmstart.py PROJECT [--work DIR] [--env NAME=VALUE]...: what a start costs when the store
already has every function, with nothing of rustc's to start from (#41).

The first build of a daemon is its own, with nothing of rustc's (docs/no-rustc.md, step K):
  first    a checkout and an empty store: `blaze build` and the wait until ready
  warm     a second checkout of the same commit with a home of its own and the store the first
           filled
  restart  the second checkout's daemon stopped and started again with nothing changed: its
           build is the daemon's, every crate from source, and what it finds is what a build
           told ahead of cargo's stored, keyed by the same toolchain
  again    the same once more, with the store holding all of that: what the issue is for
After each, a body edit and its undoing, which the daemon is to patch, and the program's output,
which has to be what blaze from nothing (`blaze_cli.Scratch`) prints. For each step: the command's seconds, the wait until ready, the most
memory held, what the worker compiled and timed, and what it took from the store: functions
found and added, frozen crates served and written (all it said is in `WORK/said-STEP.txt`, and
for the edit and its undoing in `WORK/said-STEP-edit.txt` and `-back.txt`). It fails when a
step's output is not the one from nothing, or when the restart took nothing from the store the warm step
filled.
Writes the steps as JSON on stdout's last line. The projects are bench.py's, fetched as it fetches them.
"""
import json, os, re, shutil, sys

import bench, blaze_cli, memory
from blaze_cli import Scratch, Side, fetch, program_output, wait_warm


def from_store(said):
    """What a start took from the store and added to it: the functions of its builds, and the
    crates whose def maps came frozen."""
    functions = re.findall(r"^store: (\d+) functions found, (\d+) compiled and added$", said, re.M)
    frozen = re.search(r"^frozen, of \d+ crates: def maps of (\d+).* from the store, (\d+) written", said, re.M)
    return dict(found=sum(int(f) for f, _ in functions), added=sum(int(a) for _, a in functions),
                frozen=int(frozen[1]) if frozen else 0, frozen_written=int(frozen[2]) if frozen else 0)


def main():
    argv = sys.argv[1:]
    work = os.path.realpath(bench.options(argv, "--work", "/tmp/warmstart")[0])
    extra_env = dict(it.split("=", 1) for it in bench.options(argv, "--env"))
    name, = argv
    spec = bench.PROJECTS[name]
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    source = fetch(name, spec)
    build, run = spec.get("build", []), spec.get("run")
    edit = spec["edits"]["body"]

    def checkout(label, seed=None):
        """A copy of the project with a home of its own, whose store is a copy of `seed`'s."""
        ws = os.path.join(work, label)
        shutil.copytree(source, ws, symlinks=True, ignore=lambda at, names: [n for n in names if n in ("target", ".git")])
        home = os.path.join(work, f"home-{label}")
        if seed is not None:
            shutil.copytree(os.path.join(seed.env["BLAZE_HOME"], "v2", "store"), os.path.join(home, "v2", "store"), symlinks=True)
        env = dict(blaze_cli.base_env(home), **spec.get("env", {}), **extra_env)
        env.update(BLAZE_STATS="1")
        env.pop("RUSTC_WRAPPER", None)
        return Side(ws, env, True)

    expected = None
    if run is not None:
        scratch = checkout("scratch")
        expected = program_output(Scratch(scratch.ws, scratch.env).cargo("run", *build, "--", *run)[1])

    steps = []

    def step(label, side):
        daemon = bench.Daemon(side)
        with memory.Sampler() as sampler:
            took, ran, used = bench.metered(side, build, sampler=sampler)
            assert ran.returncode == 0, f"{label}: {ran.stderr[-3000:]}"
            ready, states = wait_warm(side, side.ws)
        said = daemon.said()
        with open(os.path.join(work, f"said-{label}.txt"), "w") as kept:
            kept.write(said)
        row = dict(step=label, build_s=round(took, 3), ready_s=round(ready, 3), states=states,
                   built=bench.how_built(ran.stderr), peak_mb=max(sampler.peak_mb, used["largest_mb"]),
                   resident_mb=daemon.resident_mb(), compiled=bench.compiled(said), steps_s=bench.cold_steps(said),
                   store=from_store(said))
        edits = []
        for back in (False, True):
            bench.write_edit(side.ws, edit, back)
            took, ran, used = bench.metered(side, build, daemon)
            assert ran.returncode == 0, f"{label}: the edit: {ran.stderr[-3000:]}"
            built = bench.how_built(ran.stderr)
            said = daemon.said(1 if built == "daemon" else 0)
            with open(os.path.join(work, f"said-{label}-{'back' if back else 'edit'}.txt"), "w") as kept:
                kept.write(said)
            edits.append(dict(wall_s=round(took, 3), built=built, link=bench.link_kind(said),
                              peak_mb=used["peak_mb"]))
        row["edit"] = edits
        if run is not None:
            row["right"] = program_output(side.cargo("run", *build, "--", *run)[1]) == expected
        steps.append(row)
        print(f"{label:8} build {row['build_s']:8.2f}s  ready {row['ready_s']:7.2f}s  peak {row['peak_mb']:6} MB  "
              f"daemon {row['resident_mb']:6} MB  {row['built'][:60]}  compiled {row['compiled']}  "
              f"store {row['store']['found']} found {row['store']['added']} added, "
              f"frozen {row['store']['frozen']} found {row['store']['frozen_written']} written  "
              f"edit {edits[0]['wall_s']:.2f}s {edits[0]['link'] or edits[0]['built']}  back {edits[1]['wall_s']:.2f}s"
              + ("" if row.get("right", True) else "  WRONG OUTPUT"), flush=True)

    first = checkout("first")
    step("first", first)
    first.cargo("stop")
    second = checkout("second", seed=first)
    step("warm", second)
    second.cargo("stop")
    step("restart", second)
    second.cargo("stop")
    step("again", second)
    second.cargo("stop")
    print(json.dumps(dict(project=name, steps=steps)))
    restart = next(it["store"] for it in steps if it["step"] == "restart")
    took = restart["found"] > restart["added"] and restart["frozen"] > restart["frozen_written"]
    if not took:
        print(f"NOT the restart takes from the store what the warm step filled: {restart}", flush=True)
    return 0 if took and all(it.get("right", True) for it in steps) else 1


if __name__ == "__main__":
    sys.exit(main())
