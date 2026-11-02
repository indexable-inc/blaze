#!/usr/bin/env python3
"""abbench.py: two or more builds of blaze against each other on bench.py's edits, interleaved.

    abbench.py --bin NAME=DIR [--bin NAME=DIR ...] [--env NAME=KEY=VALUE ...] [PROJECT...]
               [--rounds R] [--runs N] [--kinds body,leaf,signature,no-op] [--out FILE]
               [--work DIR] [--pause SECS] [--train]

Each DIR holds a `blaze` and the program it runs from next to itself (`blaze-engine`). A round
goes through the builds in turn, in an order that rotates from round to round; each build gets a copy of the project and a home and store of its own that live for the
whole run, and in its turn it stops any daemon of its own, starts one from nothing, waits until
it is warm (the cold start), and makes every edit `--runs` times and takes it back. So a machine
that is busier for a while is busier for all of them alike, and every number is a median over
`rounds x runs` samples.

An edit is timed three ways: the wall time of `blaze build`, the daemon's own
`edit-to-binary`, and (macOS) the CPU cycles and instructions the daemon's workers spent on it,
which a loaded machine moves much less than the wall time. Memory is the most the workers have
held at once (VmHWM on Linux, the lifetime peak physical footprint on macOS), read at the end
of each turn. Plain cargo never builds here: bench.py compares against cargo, this compares
builds of blaze. `--env NAME=KEY=VALUE` sets a variable for one build only (an allocator
option, for example); the same DIR may be given under two names with different variables.
`--train` ends each turn so that an instrumented build writes its profile
(.github/scripts/build_release.py trains the release build this way).
"""
import ctypes, json, os, platform, re, shutil, statistics, subprocess, sys, tempfile, time

import blaze_cli
from bench import PROJECTS, Daemon, cold_steps, daemon_steps, options, write_edit
from blaze_cli import Side, base_env, fetch, how_built, wait_warm

DARWIN = platform.system() == "Darwin"


class Usage:
    """What a process has used so far: cycles, instructions, CPU seconds and its peak memory."""

    if DARWIN:
        _lib = ctypes.CDLL("/usr/lib/libproc.dylib")
        # rusage_info_v4: a 16-byte uuid, then 64-bit counters.
        _fields = ["user", "system", "pkg_idle", "interrupt", "pageins", "wired", "resident", "footprint", "start",
                   "exit", "child_user", "child_system", "child_pkg_idle", "child_interrupt", "child_pageins",
                   "child_elapsed", "read", "written", "q0", "q1", "q2", "q3", "q4", "q5", "q6", "billed",
                   "serviced", "logical_writes", "max_footprint", "instructions", "cycles"]
        _timebase = None

    @classmethod
    def of(cls, pid):
        if DARWIN:
            buf = (ctypes.c_uint64 * 48)()
            if cls._lib.proc_pid_rusage(pid, 4, ctypes.byref(buf)) != 0:
                return None
            got = dict(zip(cls._fields, list(buf)[2:]))
            if cls._timebase is None:
                class Timebase(ctypes.Structure):
                    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]
                tb = Timebase()
                ctypes.CDLL("/usr/lib/libSystem.dylib").mach_timebase_info(ctypes.byref(tb))
                cls._timebase = tb.numer / tb.denom
            cpu = (got["user"] + got["system"]) * cls._timebase / 1e9
            return dict(cycles=got["cycles"], instructions=got["instructions"], cpu_s=cpu, peak_mb=got["max_footprint"] / 2**20)
        try:
            stat = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
            status = open(f"/proc/{pid}/status").read()
        except OSError:
            return None
        ticks = os.sysconf("SC_CLK_TCK")
        return dict(cycles=0, instructions=0, cpu_s=(int(stat[11]) + int(stat[12])) / ticks,
                    peak_mb=int(re.search(r"VmHWM:\s+(\d+) kB", status)[1]) / 1024)

    @classmethod
    def total(cls, pids):
        found = [u for u in map(cls.of, pids) if u]
        return {k: sum(u[k] for u in found) for k in ("cycles", "instructions", "cpu_s", "peak_mb")}


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
        """Starts a daemon from nothing, waits for it to be warm, then makes every edit."""
        side, build = self.side, self.spec.get("build", [])
        side.cargo("stop")
        # A home, and with it its store, from nothing: the cold start is a first start.
        shutil.rmtree(self.home, ignore_errors=True)
        daemon = Daemon(side)
        took, ran = side.cargo("build", *build)
        if ran.returncode != 0:
            raise SystemExit(f"{self.name}: the first build failed:\n{ran.stderr[-3000:]}")
        warm, states = wait_warm(side, self.ws)
        if states != ["ready"]:
            raise SystemExit(f"{self.name}: the daemon did not get ready: {states}")
        pids = [pid for pid, _ in daemon.workers()]
        said = daemon.said()
        self.add("cold", wall_s=round(took + warm, 3), first_build_s=round(took, 3), warm_s=round(warm, 3),
                 steps_s=cold_steps(said), **Usage.total(pids))

        def timed_build(kind, edit):
            time.sleep(pause)
            before = Usage.total(pids)
            took, ran = side.cargo("build", *build)
            after = Usage.total(pids)
            built = how_built(ran.stderr)
            if ran.returncode != 0 or built != "daemon":
                raise SystemExit(f"{self.name} {kind}: built by {built} (exit {ran.returncode}):\n{ran.stderr[-2000:]}")
            said = daemon.said(1 if edit else 0)
            e2b = re.findall(r"^edit-to-binary: (\S+)$", said, re.M)
            self.add(kind, wall_s=took, edit_to_binary_s=sum(map(seconds, e2b)) if e2b else None,
                     steps_ms=daemon_steps(said), cycles=after["cycles"] - before["cycles"],
                     instructions=after["instructions"] - before["instructions"], cpu_s=after["cpu_s"] - before["cpu_s"])

        for kind in kinds:
            for _ in range(runs):
                if kind == "no-op":
                    timed_build(kind, False)
                    continue
                changes = self.spec["edits"][kind]
                write_edit(self.ws, changes, False)
                timed_build(kind, True)
                write_edit(self.ws, changes, True)
                timed_build(kind + " back", True)
        self.add("memory", peak_mb=Usage.total(pids)["peak_mb"])
        if self.train:
            end_for_profile(side, pids)
        else:
            side.cargo("stop")


def end_for_profile(side, workers):
    """Ends the daemon so that its workers exit by themselves. `stop` kills the workers, and a
    killed process never writes the counters of an instrumented (PGO) build; a worker whose
    daemon is gone sees its input end and exits, which writes them."""
    _, status = side.cargo("status")
    daemon = re.search(r"^daemon: pid (\d+),", status.stdout, re.M)
    if daemon:
        os.kill(int(daemon[1]), 9)
    began = time.time()
    while any(blaze_cli.alive(pid) for pid in workers) and time.time() - began < 120:
        time.sleep(0.2)


def seconds(text):
    number, unit = re.match(r"([0-9.]+)(ns|µs|ms|s)", text).groups()
    return float(number) * {"ns": 1e-9, "µs": 1e-6, "ms": 1e-3, "s": 1.0}[unit]


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
