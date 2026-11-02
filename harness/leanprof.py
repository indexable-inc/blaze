#!/usr/bin/env python3
"""leanprof.py PROJECT [--work DIR] [--edits KIND,...] [--rounds N] [--env NAME=VALUE]...:
the phases of a project's first hour, each with the monotonic time it began and ended, for a
profile of the whole run to be cut by ([research/lean-frontend.md](../docs/research/lean-frontend.md)).

    perf record -k CLOCK_MONOTONIC -g -F 199 -o run.data -- python3 leanprof.py nushell --work W
    perf script -i run.data > run.txt
    python3 cpufold.py run.txt --phases W/phases.json

The standard library's crates are units of the cold build like any other, as in bench.py. The
phases: `cold` (`blaze build` from nothing until the daemon is ready), `base` (the
build of its own base it makes once idle), each edit of `--edits` (default `body,signature`,
from bench.py's `PROJECTS`) made and taken back `--rounds` times (default 3), and `warm` (the
daemon stopped and started again on the same store and target directory, until ready) and
`warm base` (left out with `--no-warm`). The first daemon's home, logs and all, is kept in
WORK/logs-cold. `--env` is given to the daemon; `BLAZE_MEMORY=DIR` is given as DIR/cold for the
first daemon and DIR/warm for the second, so the reports of the two do not overwrite each
other. Writes the phases as JSON to WORK/phases.json and on stdout's last line. Projects are
bench.py's, fetched as it fetches them. It measures and checks nothing.
"""
import json, os, shutil, subprocess, sys, time

import bench, blaze_cli, memory
from blaze_cli import Side, fetch, wait_warm
from memsplit import wait_own_base


def main():
    argv = sys.argv[1:]
    cold_only = "--no-warm" in argv
    argv = [it for it in argv if it != "--no-warm"]
    work = os.path.realpath(bench.options(argv, "--work", "/tmp/leanprof")[0])
    kinds = bench.options(argv, "--edits", "body,signature")[0].split(",")
    rounds = int(bench.options(argv, "--rounds", "3")[0])
    extra_env = dict(it.split("=", 1) for it in bench.options(argv, "--env"))
    name, = argv
    spec = bench.PROJECTS[name]
    source = fetch(name, spec)
    ws = os.path.join(work, name)
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    shutil.copytree(source, ws, symlinks=True, ignore=lambda at, names: [n for n in names if n in ("target", ".git")])
    home = os.path.join(work, "home")
    env = dict(blaze_cli.base_env(home), **spec.get("env", {}), **extra_env)
    env["PATH"] = os.path.dirname(bench.blaze_of("cranelift")) + os.pathsep + env["PATH"]
    env.update(BLAZE_STATS="1")
    env.pop("RUSTC_WRAPPER", None)
    reports = env.get("BLAZE_MEMORY")
    build = spec.get("build", [])
    subprocess.run(["cargo", "fetch"], cwd=ws, env=env, capture_output=True)
    phases = []

    def phase(label, began, **extra):
        phases.append(dict(phase=label, began=began, ended=time.monotonic(), **extra))
        print(f"{label:16} {phases[-1]['ended'] - began:8.1f} s  " + "  ".join(f"{k} {v}" for k, v in extra.items()), flush=True)

    def side(step):
        if reports:
            return Side(ws, dict(env, BLAZE_MEMORY=os.path.join(reports, step)), True)
        return Side(ws, env, True)

    ours = side("cold")
    daemon = bench.Daemon(ours)
    try:
        began = time.monotonic()
        took, ran = ours.cargo("build", *build)
        assert ran.returncode == 0, ran.stderr[-3000:]
        warm, states = wait_warm(ours, ws)
        phase("cold", began, build_s=round(took, 1), states=states, resident_mb=daemon.resident_mb())
        began = time.monotonic()
        with memory.Sampler() as sampler:
            sampler.add(daemon.pid())
            _, how = wait_own_base(daemon, sampler)
        phase("base", began, how=how, peak_mb=sampler.peak_mb, resident_mb=daemon.resident_mb())
        for kind in kinds:
            began = time.monotonic()
            walls = []
            with memory.Sampler() as sampler:
                sampler.add(daemon.pid())
                for _ in range(rounds):
                    for back in (False, True):
                        bench.write_edit(ws, spec["edits"][kind], back)
                        took, ran = ours.cargo("build", *build)
                        walls.append((round(took, 3), bench.how_built(ran.stderr)[:40]))
            phase(kind, began, walls=walls, peak_mb=sampler.peak_mb, resident_mb=daemon.resident_mb())
        ours.cargo("stop")
        # The second daemon writes its logs where the first did.
        shutil.copytree(home, os.path.join(work, "logs-cold"), ignore=lambda at, names: [n for n in names if n in ("cargo", "rustup", "target") or n.endswith(".sock")])
        if cold_only:
            return
        ours = side("warm")
        daemon = bench.Daemon(ours)
        began = time.monotonic()
        took, ran = ours.cargo("build", *build)
        assert ran.returncode == 0, ran.stderr[-3000:]
        warm, states = wait_warm(ours, ws)
        phase("warm", began, build_s=round(took, 1), states=states, resident_mb=daemon.resident_mb())
        began = time.monotonic()
        with memory.Sampler() as sampler:
            sampler.add(daemon.pid())
            _, how = wait_own_base(daemon, sampler)
        phase("warm base", began, how=how, peak_mb=sampler.peak_mb, resident_mb=daemon.resident_mb())
    finally:
        ours.cargo("stop")
        result = dict(project=name, env=extra_env, phases=phases)
        open(os.path.join(work, "phases.json"), "w").write(json.dumps(result))
        print(json.dumps(result))


if __name__ == "__main__":
    main()
