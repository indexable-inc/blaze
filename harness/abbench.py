#!/usr/bin/env python3
"""abbench.py: two or more builds of blaze against each other on bench.py's edits, interleaved.

    abbench.py --bin NAME=DIR [--bin NAME=DIR ...] [--env NAME=KEY=VALUE ...] [PROJECT...]
               [--rounds R] [--runs N] [--kinds body,leaf,signature,no-op] [--out FILE]
               [--work DIR] [--pause SECS] [--train]

Each DIR holds a `blaze` and the program it runs from next to itself (`blaze-engine`). A round
goes through the builds in turn, in an order that rotates from round to round; each build gets a copy of the project and a home and store of its own that live for the
whole run, and in its turn it stops any daemon of its own, starts one from nothing with its
first build (the cold start), and makes every edit `--runs` times and takes it back. So a machine
that is busier for a while is busier for all of them alike, and every number is a median over
`rounds x runs` samples.

An edit is timed by the wall time of `blaze build`, the steps the daemon timed itself (the
`steps_us` of the build's `blaze-work` line under `--message-format=json`), and the CPU cycles
and instructions the daemon spent on it (counters.py: Linux's counters in user mode, macOS's
totals), which a loaded machine moves much less than the wall time; the cold start's are macOS's
alone (Linux keeps no lifetime count of a process). v1's `edit-to-binary`, a line of its
workers' logs, has no counterpart in v2. Memory is the most the daemon has
held (VmHWM on Linux, the lifetime peak physical footprint on macOS), read at the end
of each turn. Plain cargo never builds here: bench.py compares against cargo, this compares
builds of blaze. `--env NAME=KEY=VALUE` sets a variable for one build only (an allocator
option, for example); the same DIR may be given under two names with different variables.
`--train` ends each turn so that an instrumented build writes its profile
(.github/scripts/build_release.py trains the release build this way).
"""
import json, os, platform, re, shutil, statistics, sys, tempfile, time

import blaze_cli, counters
from bench import JSON, PROJECTS, Daemon, cold_steps, daemon_steps, options, write_edit
from blaze_cli import Side, base_env, fetch, how_built

DARWIN = platform.system() == "Darwin"


class Usage:
    """What a process has used so far: its CPU seconds and its peak memory. Its instructions and
    cycles are counters.py's (a Meter around each build; `counters.lifetime` for the cold start)."""

    @classmethod
    def of(cls, pid):
        if DARWIN:
            got = counters.rusage(pid)
            if got is None:
                return None
            return dict(cpu_s=got["cpu_ns"] / 1e9, peak_mb=got["lifetime_max_footprint"] / 2**20)
        try:
            stat = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
            status = open(f"/proc/{pid}/status").read()
        except OSError:
            return None
        ticks = os.sysconf("SC_CLK_TCK")
        return dict(cpu_s=(int(stat[11]) + int(stat[12])) / ticks,
                    peak_mb=int(re.search(r"VmHWM:\s+(\d+) kB", status)[1]) / 1024)

    @classmethod
    def total(cls, pids):
        found = [u for u in map(cls.of, pids) if u]
        return {k: sum(u[k] for u in found) for k in ("cpu_s", "peak_mb")}


class Build:
    """One build of blaze under test: its copy of the project, its home and its store."""

    def __init__(self, name, bindir, extra, project, spec, work):
        self.name, self.bindir, self.spec = name, os.path.realpath(bindir), spec
        self.ws = os.path.realpath(os.path.join(work, f"{project}-{name}"))
        if not os.path.isdir(self.ws):
            shutil.copytree(fetch(project, spec), self.ws, symlinks=True, ignore=shutil.ignore_patterns("target", "target-ref", ".git"))
        self.home = os.path.join(work, f"home-{project}-{name}")
        self.env = base_env(self.home)
        self.env["PATH"] = self.bindir + os.pathsep + self.env["PATH"]
        self.env.update(extra, BLAZE_STATS="1")
        self.env.pop("RUSTC_WRAPPER", None)
        self.side = Side(self.ws, self.env, True)
        self.samples, self.train = {}, False

    def add(self, kind, **sample):
        self.samples.setdefault(kind, []).append(sample)

    def turn(self, kinds, runs, pause):
        """Starts a daemon from nothing with the first build, then makes every edit."""
        side, build = self.side, self.spec.get("build", [])
        side.cargo("stop")
        # A home, and with it its store, from nothing: the cold start is a first start.
        shutil.rmtree(self.home, ignore_errors=True)
        daemon = Daemon(side)
        took, ran = side.cargo("build", *build, JSON)
        if ran.returncode != 0:
            raise SystemExit(f"{self.name}: the first build failed:\n{ran.stderr[-3000:]}")
        # v2's first build is the daemon's, with nothing left to load after it; the daemon
        # does the work itself, with no workers.
        warm, pid = 0.0, daemon.pid()
        if pid is None:
            raise SystemExit(f"{self.name}: no daemon runs after the first build (no {self.home}/v2/serve.lock)")
        pids = [pid]
        cold = counters.lifetime(pids)
        self.add("cold", wall_s=round(took + warm, 3), first_build_s=round(took, 3), warm_s=round(warm, 3),
                 steps_s=cold_steps(ran.stdout), cycles=cold.cycles, instructions=cold.instructions, **Usage.total(pids))
        # The daemon, and what it starts, counted from here on.
        meter = counters.Meter()
        for pid in pids:
            meter.follow(pid)

        def timed_build(kind):
            time.sleep(pause)
            before, counted = Usage.total(pids), meter.snapshot()
            took, ran = side.cargo("build", *build, JSON)
            after, counted = Usage.total(pids), meter.since(counted)
            built = how_built(ran)
            if ran.returncode != 0 or built != "daemon":
                raise SystemExit(f"{self.name} {kind}: built by {built} (exit {ran.returncode}):\n{ran.stderr[-2000:]}")
            self.add(kind, wall_s=took, steps_ms=daemon_steps(ran.stdout), cycles=counted.cycles, instructions=counted.instructions,
                     cpu_s=after["cpu_s"] - before["cpu_s"])

        for kind in kinds:
            for _ in range(runs):
                if kind == "no-op":
                    timed_build(kind)
                    continue
                changes = self.spec["edits"][kind]
                write_edit(self.ws, changes, False)
                timed_build(kind)
                write_edit(self.ws, changes, True)
                timed_build(kind + " back")
        meter.close()
        self.add("memory", peak_mb=Usage.total(pids)["peak_mb"])
        if self.train:
            end_for_profile(side, pids)
        else:
            side.cargo("stop")


def end_for_profile(side, pids):
    """Ends the daemon so that it exits by itself: a killed process never writes the counters
    of an instrumented (PGO) build, and v2's `blaze stop` has the daemon write its image and
    exit, which writes them. Waits, at most 120 s, until it is gone."""
    side.cargo("stop")
    began = time.time()
    while any(blaze_cli.alive(pid) for pid in pids) and time.time() - began < 120:
        time.sleep(0.2)


def summary(builds):
    """The medians of every build, per kind of sample."""
    out = {}
    for b in builds:
        rows = {}
        for kind, samples in b.samples.items():
            keys = [k for k, v in samples[0].items() if isinstance(v, (int, float)) and v is not None]
            rows[kind] = {k: round(statistics.median(s[k] for s in samples if s.get(k) is not None), 4) for k in keys}
            rows[kind]["n"] = len(samples)
        out[b.name] = rows
    return out


def main():
    argv = sys.argv[1:]
    bins = [b.split("=", 1) for b in options(argv, "--bin")]
    extra = {}
    for item in options(argv, "--env"):
        name, key, value = item.split("=", 2)
        extra.setdefault(name, {})[key] = value
    rounds, runs = int(options(argv, "--rounds", "3")[0]), int(options(argv, "--runs", "3")[0])
    kinds = options(argv, "--kinds", "body,leaf,signature,no-op")[0].split(",")
    pause = float(options(argv, "--pause", "0.3")[0])
    train = "--train" in argv
    out = options(argv, "--out", "abbench.json")[0]
    work = (options(argv, "--work") or [None])[0] or tempfile.mkdtemp(prefix="blaze-ab-")
    projects = [a for a in argv if not a.startswith("--")] or ["ripgrep"]
    os.makedirs(work, exist_ok=True)
    result = dict(date=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), machine=platform.machine(), os=platform.system(),
                  cores=os.cpu_count(), rounds=rounds, runs=runs, bins=dict(bins), env=extra, results={}, samples={})
    for project in projects:
        spec = PROJECTS[project]
        builds = [Build(name, d, extra.get(name, {}), project, spec, work) for name, d in bins]
        for b in builds:
            b.train = train
        for r in range(rounds):
            order = builds[r % len(builds):] + builds[:r % len(builds)]
            for b in order:
                print(f"{project} round {r + 1}/{rounds}: {b.name}", flush=True)
                b.turn([k for k in kinds if k == "no-op" or k in spec["edits"]], runs, pause)
            result["results"][project] = summary(builds)
            result["samples"][project] = {b.name: b.samples for b in builds}
            json.dump(result, open(out, "w"), indent=1)
        for name, rows in result["results"][project].items():
            print(f"  {name:12} " + "  ".join(f"{k} {v.get('wall_s', v.get('peak_mb'))}" for k, v in rows.items()), flush=True)
    print(f"wrote {out}")


if __name__ == "__main__":
    main()
