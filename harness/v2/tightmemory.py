"""tightmemory.py: a cold build under a tight memory budget finishes, slower, or refuses saying what
it needed; the kernel never kills it (docs/v2/plan/lanes/core.md section 4, docs/v2/plan/lanes/proof.md, docs/memory-budget.md).
Linux, cgroup v2.

    uv run --frozen python harness/v2/tightmemory.py '{"engine": {"kind": "v1", "blaze": "target/release/blaze"}, "out": "/tmp/tight.json"}'
    uv run --frozen python harness/v2/tightmemory.py '{"engine": {...}, "out": "...", "factors": [1.0, 0.5, 0.25], "cgroup": "/sys/fs/cgroup/blaze"}'
    uv run --frozen python harness/v2/tightmemory.py selftest

The input is one JSON object (inline, a file's path, or `-`):

    engine     the blaze measured (engine.EngineSpec)
    out        the result's path (report.py's JSON)
    work       where checkouts, homes, stores and target directories go; a new directory under
               /tmp when empty
    projects   the projects of bench.PROJECTS built, at their pinned revisions (ripgrep)
    factors    the limits, each a factor of cargo's peak (1.0, 0.5); numbers with a point
    cgroup     a cgroup v2 directory this user may make groups in, with the memory controller
               on for its children; empty: each limited build is a transient scope of the
               user's systemd (`systemd-run --user --scope`)
    timeout_s  how long a build may take before it counts as hung (7200.0)
    keep       keep the work directory (false)

For each project: cargo's cold build, whose peak (memory.run: the most its processes held at
once) sets the limits; then a cold blaze build with no limit, and what its program prints; then
one under each factor. A blaze build is cold: an empty home, store and target directory. Each
runs in a child of this script (`child`, a JSON input of its own) that drives it through
`engine.Engine`, so the daemon it starts is the child's descendant and inside its limit. Under a
factor the child is in a cgroup with `memory.max` of factor × cargo's peak and `memory.swap.max`
0 (swap is not budget), and `BLAZE_MEMORY_BUDGET` says the same size. The child's own Python is
inside the limit too; the result says how much it held (`harness_mb`).

A limited build passes when it finishes and its program prints what the unlimited build's does
(blaze's own cold build of the same source), or, below the
factor 1.0 only, when it fails saying what it needed (docs/memory-budget.md section 6: "needs N GB
... may use M GB"). It fails when the OOM killer killed anything in its group (`memory.events`
`oom_kill`), when anything of it ended by SIGKILL, when it hung past `timeout_s`, when its
program prints something else, or when it failed without saying what it needed. Each outcome
has its peak (sampled, and the group's `memory.peak`), its time and its slowdown against the
unlimited build. `selftest` checks the parsing and the verdicts without blaze, cargo or the
network.
"""

import json
import os
import re
import resource
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable
from dataclasses import asdict, dataclass, replace
from pathlib import Path
from typing import Protocol

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse
import engine
import report
import memory

SCRIPT = "tightmemory.py"
CGROUPS = Path("/sys/fs/cgroup")
SIZE = r"\d+(?:\.\d+)? ?[KMGT]i?B\b"
REFUSAL = re.compile(rf"needs {SIZE}.*?may use {SIZE}", re.S)
RUN_SECS = 600.0
SETTLE_SECS = 30.0


class Sampling(Protocol):
    """memory.Sampler, as this script uses it."""

    @property
    def peak_mb(self) -> int: ...

    def add(self, *pids: int, itself: bool = True) -> "Sampling": ...

    def start(self) -> "Sampling": ...

    def stop(self) -> "Sampling": ...


Sampler: Callable[[], Sampling] = getattr(memory, "Sampler")


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    projects: tuple[str, ...] = ("ripgrep",)
    factors: tuple[float, ...] = (1.0, 0.5)
    cgroup: str = ""
    timeout_s: float = 7200.0
    keep: bool = False


@dataclass(frozen=True)
class Child:
    """The input of `child`: one cold blaze build of `ws`, its program run once it is built, the
    result written to `done`. `enter` is a `cgroup.procs` the child moves itself into first;
    `limit_bytes` the `memory.max` its cgroup has to have, with `memory.swap.max` 0 (0: no limit);
    `env` is NAME=VALUE; `budget` is BLAZE_MEMORY_BUDGET's value, or empty for none."""

    engine: engine.EngineSpec
    ws: str
    home: str
    store: str
    build: tuple[str, ...]
    run: tuple[str, ...] | None
    env: tuple[str, ...]
    budget: str
    enter: str
    limit_bytes: int
    timeout_s: float
    done: str


@dataclass(frozen=True)
class Done:
    """What a child wrote of its build."""

    cgroup: str | None
    seconds: float
    build_status: int | None
    said: str
    not_blazes: str | None
    hung: bool
    error: str | None
    peak_mb: int
    cgroup_peak_mb: int | None
    oom_kills: int | None
    harness_mb: int
    program_status: int | None = None
    program_stdout: str = ""
    program_stderr: str = ""


def events(text: str) -> dict[str, int]:
    """A cgroup's `memory.events`: each counter by its name."""
    found: dict[str, int] = {}
    for line in text.splitlines():
        words = line.split()
        if len(words) == 2 and words[1].isdigit():
            found[words[0]] = int(words[1])
    return found


def cgroup_of(text: str) -> str | None:
    """The cgroup v2 path of a process from its /proc/PID/cgroup, or None without one."""
    for line in text.splitlines():
        if line.startswith("0::"):
            return line[3:].strip() or None
    return None


def refusal(text: str) -> str | None:
    """What a build said of the memory it needed when it refused to start, or None."""
    found = REFUSAL.search(text)
    if found is None:
        return None
    start = text.rfind("\n", 0, found.start()) + 1
    return " ".join(text[start:found.end()].split())


def limit_of(factor: float, cargo_peak_mb: int) -> tuple[int, int]:
    """The limit of `factor` times cargo's peak, in MiB (at least 1) and in bytes."""
    mib = max(1, int(factor * cargo_peak_mb))
    return mib, mib << 20


def scope_argv(unit: str, limit_bytes: int, argv: list[str]) -> list[str]:
    """`argv` in a transient scope of the user's systemd with that much memory and no swap."""
    return ["systemd-run", "--user", "--scope", "--quiet", f"--unit={unit}", "-p", f"MemoryMax={limit_bytes}", "-p",
            "MemorySwapMax=0", "--", *argv]


@dataclass(frozen=True)
class Ending:
    """How one build ended, as the parent saw it: the limit's factor (None: no limit), the child's
    exit status (None: it outlived its deadline and was killed), OOM kills in its group, what it
    wrote, and whether its program prints what the unlimited build's does (None: not run, or
    this is the unlimited build)."""

    factor: float | None
    status: int | None
    oom_kills: int
    done: Done | None
    same: bool | None
    said: str = ""


def judge(e: Ending) -> tuple[report.Verdict, str]:
    if e.oom_kills:
        return "fail", f"the OOM killer killed {e.oom_kills} process{'es' if e.oom_kills > 1 else ''} of it"
    if e.status is None or (e.done is not None and e.done.hung):
        return "fail", "it hung past the timeout"
    if e.status == -signal.SIGKILL:
        return "fail", "the process driving it was killed by SIGKILL"
    d = e.done
    if d is None:
        return "error", f"the child exited {e.status} without a result:\n{e.said[-2000:]}"
    if d.build_status == -signal.SIGKILL:
        return "fail", "`blaze build` was killed by SIGKILL"
    if d.error is not None:
        return "error", d.error
    if d.build_status == 0:
        if d.not_blazes is not None:
            return "fail", f"built by {d.not_blazes}, not blaze"
        if e.same is False:
            return "fail", "the program says something else than the unlimited build's"
        return "pass", "finished"
    why = refusal(d.said)
    if why and e.factor is not None and e.factor < 1.0:
        return "pass", f"refused: {why}"
    if why:
        return "fail", f"refused where cargo's peak fits: {why}"
    return "fail", f"`blaze build` exited {d.build_status} without saying what it needed:\n{d.said[-2000:]}"


def write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=1) + "\n")


def own_cgroup() -> Path | None:
    try:
        found = cgroup_of(Path("/proc/self/cgroup").read_text())
    except OSError:
        return None
    return None if found is None else CGROUPS / found.lstrip("/")


def group_numbers(group: Path | None) -> tuple[int | None, int | None]:
    """The OOM kills in `group` and below, and the most it held (MiB), where the kernel says."""
    if group is None:
        return None, None
    try:
        kills = events((group / "memory.events").read_text()).get("oom_kill")
    except OSError:
        kills = None
    try:
        peak = int((group / "memory.peak").read_text()) >> 20
    except (OSError, ValueError):
        peak = None
    return kills, peak


def limits(group: Path | None) -> tuple[str, str] | None:
    """A cgroup's own `memory.max` and `memory.swap.max`, or None where it has none."""
    if group is None:
        return None
    try:
        return (group / "memory.max").read_text().strip(), (group / "memory.swap.max").read_text().strip()
    except OSError:
        return None


def child(args: Child) -> None:
    """One cold build, driven through the engine, from inside the limit the parent set."""
    if args.enter:
        Path(args.enter).write_text(str(os.getpid()))
    group = own_cgroup()
    harness_mb = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss >> 10
    if args.limit_bytes and (held := limits(group)) != (str(args.limit_bytes), "0"):
        error = (f"the limit is not in force: the cgroup {group} has memory.max and memory.swap.max {held}, not "
                 f"{args.limit_bytes} and 0 (is the memory controller delegated to it?)")
        done = Done(cgroup=None if group is None else str(group), seconds=0.0, build_status=None, said="",
                    not_blazes=None, hung=False, error=error, peak_mb=0, cgroup_peak_mb=None, oom_kills=None,
                    harness_mb=harness_mb)
        write_json(Path(args.done), asdict(done))
        return
    eng = engine.Engine.of(args.engine)
    extra: dict[str, str] = {}
    for item in args.env:
        name, value = item.split("=", 1)
        extra[name] = value
    if args.budget:
        extra["BLAZE_MEMORY_BUDGET"] = args.budget
    ws = Path(args.ws)
    env = eng.env(Path(args.home), Path(args.store), None, extra)
    status: int | None = None
    said, who, error, hung = "", None, None, False
    program: engine.Ran | None = None
    sampler = Sampler().add(os.getpid(), itself=False).start()
    began = time.monotonic()
    try:
        ran = eng.build("the cold build", ws, env, args.build, timeout=args.timeout_s)
        status, said = ran.returncode, ran.said(20000)
        if status == 0:
            who = eng.not_blazes(ran)
            if who is None:
                eng.wait_ready(ws, env, max(1.0, args.timeout_s - (time.monotonic() - began)))
    except engine.Failed as e:
        hung = time.monotonic() - began >= args.timeout_s - 1
        error = str(e)
    finally:
        sampler.stop()
    seconds = time.monotonic() - began
    if status == 0 and who is None and error is None and args.run is not None:
        try:
            program = eng.build("the program", ws, env, [*args.build, "--", *args.run], "run", RUN_SECS)
        except engine.Failed as e:
            error = str(e)
    try:
        eng.stop(ws, env)
    except engine.Failed as e:
        error = error or str(e)
    kills, peak = group_numbers(group)
    done = Done(cgroup=None if group is None else str(group), seconds=seconds, build_status=status, said=said,
                not_blazes=who, hung=hung, error=error, peak_mb=sampler.peak_mb, cgroup_peak_mb=peak, oom_kills=kills,
                harness_mb=harness_mb, program_status=None if program is None else program.returncode,
                program_stdout="" if program is None else program.stdout,
                program_stderr="" if program is None else program.stderr)
    write_json(Path(args.done), asdict(done))


@dataclass(frozen=True)
class Attempt:
    ending: Ending
    group_peak_mb: int | None


class Project:
    """One project of bench.PROJECTS checked out under `work`, cargo's cold peak measured as the
    limits' baseline."""

    def __init__(self, eng: engine.Engine, args: Input, work: Path, name: str):
        import budgets

        self.eng, self.args, self.name = eng, args, name
        self.top = work / name
        shutil.rmtree(self.top, ignore_errors=True)
        self.top.mkdir(parents=True)
        spec, self.ws = budgets.checkout(name, self.top / "src")
        self.extra = budgets.project_env(spec)
        self.build: tuple[str, ...] = tuple(spec.get("build") or ())
        self.run: tuple[str, ...] | None = None if spec.get("run") is None else tuple(spec["run"])
        cargo_env = engine.reference_env(self.top / "cargo-target", self.extra)
        fetched = engine.cargo(f"{name}: cargo fetch", ["fetch"], self.ws, cargo_env)
        if fetched.returncode != 0:
            raise engine.Failed(f"{name}: `cargo fetch` exited {fetched.returncode}:\n{fetched.said()}")
        self.cargo_s, ran, used = budgets.memory_run(["cargo", "build", *self.build], cwd=self.ws, env=cargo_env,
                                                     timeout=args.timeout_s)
        if ran.returncode != 0:
            raise engine.Failed(f"{name}: the cold cargo build failed:\n{(ran.stdout + ran.stderr)[-3000:]}")
        self.cargo_peak_mb = used["peak_mb"] or 0
        self.program_output = budgets.program_output
        # What the unlimited build's program printed, once it ran.
        self.expected: tuple[int, str, str] | None = None

    def attempt(self, label: str, factor: float | None) -> Attempt:
        """A cold blaze build in a child, under `factor` times cargo's peak or with no limit."""
        for path in (self.ws / "target", self.top / "home", self.top / "store"):
            shutil.rmtree(path, ignore_errors=True)
        done_path = self.top / f"done-{label}.json"
        done_path.unlink(missing_ok=True)
        group: Path | None = None
        unit = f"blaze-tight-{os.getpid()}-{label}"
        budget, enter = "", ""
        limit_bytes = 0
        if factor is not None:
            mib, limit_bytes = limit_of(factor, self.cargo_peak_mb)
            budget = f"{mib}M"
            if self.args.cgroup:
                group = Path(self.args.cgroup) / unit
                try:
                    group.mkdir()
                    (group / "memory.max").write_text(str(limit_bytes))
                    (group / "memory.swap.max").write_text("0")
                except OSError as e:
                    raise engine.Failed(f"{self.name}: making the cgroup {group} with memory.max {limit_bytes} and "
                                        f"memory.swap.max 0: {e}") from None
                enter = str(group / "cgroup.procs")
        spec = Child(engine=self.eng.spec, ws=str(self.ws), home=str(self.top / "home"), store=str(self.top / "store"),
                     build=self.build, run=self.run, env=tuple(f"{k}={v}" for k, v in self.extra.items()),
                     budget=budget, enter=enter, limit_bytes=limit_bytes, timeout_s=self.args.timeout_s,
                     done=str(done_path))
        argv = [sys.executable, str(Path(__file__).resolve()), "child", json.dumps(asdict(spec))]
        if factor is not None and not self.args.cgroup:
            argv = scope_argv(unit, limit_bytes, argv)
        status: int | None
        said = ""
        try:
            ran = subprocess.run(argv, capture_output=True, text=True, errors="replace", stdin=subprocess.DEVNULL,
                                 timeout=2 * self.args.timeout_s + RUN_SECS)
            status, said = ran.returncode, ran.stdout + ran.stderr
        except subprocess.TimeoutExpired:
            status = None
        if factor is not None and not self.args.cgroup:
            group = scope_group(unit)
        kills, group_peak = group_numbers(group)
        done = parse(Done, str(done_path), SCRIPT) if done_path.exists() else None
        if factor is not None:
            self.clean(unit, group)
        elif status is None:
            self.eng.stop(self.ws, self.eng.env(self.top / "home", self.top / "store", None, self.extra))
        oom = max(kills or 0, (done.oom_kills or 0) if done else 0)
        same: bool | None = None
        if self.expected is not None and done is not None and done.program_status is not None:
            ran_program = engine.Ran((), 0.0, done.program_status, done.program_stdout, done.program_stderr)
            same = self.program_output(ran_program) == self.expected
        peak = group_peak if group_peak is not None else None if done is None else done.cgroup_peak_mb
        return Attempt(Ending(factor, status, oom, done, same, said), peak)

    def clean(self, unit: str, group: Path | None) -> None:
        """Whatever of a limited build outlived its child, killed, and its group gone."""
        if not self.args.cgroup:
            subprocess.run(["systemctl", "--user", "stop", f"{unit}.scope"], capture_output=True)
            return
        if group is None:
            return
        try:
            kill = group / "cgroup.kill"
            if kill.exists():
                kill.write_text("1")
            else:
                for pid in (group / "cgroup.procs").read_text().split():
                    try:
                        os.kill(int(pid), signal.SIGKILL)
                    except ProcessLookupError:
                        pass
            began = time.monotonic()
            while (group / "cgroup.procs").read_text().strip() and time.monotonic() - began < SETTLE_SECS:
                time.sleep(0.1)
            group.rmdir()
        except OSError as e:
            raise engine.Failed(f"{self.name}: emptying and removing the cgroup {group}: {e}") from None


def scope_group(unit: str) -> Path | None:
    """The cgroup of a transient scope that still runs, or None."""
    shown = subprocess.run(["systemctl", "--user", "show", "--property=ControlGroup", "--value", f"{unit}.scope"],
                           capture_output=True, text=True)
    path = shown.stdout.strip()
    return CGROUPS / path.lstrip("/") if shown.returncode == 0 and path else None


def numbers_of(attempt: Attempt, unlimited: Done | None) -> dict[str, report.Number]:
    d = attempt.ending.done
    found: dict[str, report.Number] = dict(oom_kills=attempt.ending.oom_kills, cgroup_peak_mb=attempt.group_peak_mb)
    if d is not None:
        found.update(seconds=round(d.seconds, 2), peak_mb=d.peak_mb, harness_mb=d.harness_mb)
        if unlimited is not None and unlimited.seconds > 0:
            found.update(unlimited_s=round(unlimited.seconds, 2), slowdown=round(d.seconds / unlimited.seconds, 3))
    return found


def project(eng: engine.Engine, rep: report.Report, args: Input, work: Path, name: str) -> None:
    import budgets

    setup = rep.guarded(f"{name}: setting up", lambda: budgets.failing(name, lambda: Project(eng, args, work, name)))
    if setup is None:
        return
    row = f"{name} with no limit"
    base = rep.guarded(row, lambda: budgets.failing(row, lambda: setup.attempt("unlimited", None)))
    unlimited: Done | None = None
    if base is not None:
        verdict, said = judge(base.ending)
        rep.add(row, verdict, said, cargo_peak_mb=setup.cargo_peak_mb, cargo_s=round(setup.cargo_s, 2),
                **numbers_of(base, None))
        unlimited = base.ending.done if verdict == "pass" else None
        if unlimited is not None and unlimited.program_status is not None:
            setup.expected = setup.program_output(engine.Ran((), 0.0, unlimited.program_status, unlimited.program_stdout,
                                                             unlimited.program_stderr))
    for i, factor in enumerate(args.factors):
        mib, _ = limit_of(factor, setup.cargo_peak_mb)
        row = f"{name} under {factor:g} x cargo's peak ({mib} MiB)"
        attempt = rep.guarded(row, lambda: budgets.failing(row, lambda: setup.attempt(f"{i}", factor)))
        if attempt is None:
            continue
        verdict, said = judge(attempt.ending)
        rep.add(row, verdict, said, factor=factor, limit_mb=mib, cargo_peak_mb=setup.cargo_peak_mb,
                **numbers_of(attempt, unlimited))
    if not args.keep:
        shutil.rmtree(setup.top, ignore_errors=True)


def main(args: Input) -> None:
    if sys.platform != "linux":
        fail(SCRIPT, f"the hard limit is a cgroup v2 memory.max, which is Linux's; {sys.platform} has none")
    if not args.factors or any(f <= 0 for f in args.factors):
        fail(SCRIPT, f"factors: positive numbers, not {list(args.factors)}")
    if args.cgroup:
        root = Path(args.cgroup)
        try:
            controllers = (root / "cgroup.subtree_control").read_text().split()
        except OSError as e:
            fail(SCRIPT, f"cgroup: {root} is not a cgroup v2 directory this script can read: {e}")
        if "memory" not in controllers:
            fail(SCRIPT, f"cgroup: the memory controller is not on for {root}'s children (cgroup.subtree_control)")
    elif shutil.which("systemd-run") is None or shutil.which("systemctl") is None:
        fail(SCRIPT, "systemd-run and systemctl are not on PATH: name a cgroup v2 directory in `cgroup`")
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    rep = report.Report(SCRIPT, args.engine, args.out)
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-tight-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    print(f"work: {work}", flush=True)
    for name in dict.fromkeys(args.projects):
        project(eng, rep, args, work, name)
    rep.extra.update(work=str(work), factors=list(args.factors), cgroup=args.cgroup or "systemd-run --user --scope",
                     timeout_s=args.timeout_s)
    rep.finish()


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


DOC_REFUSAL = """   Compiling hir-ty v0.0.0
error: the test program of `hir-ty` (release) needs 9.4 GB, measured on 2026-10-29, and blaze
  may use 7.1 GB here (cgroup memory.max of /system.slice/runner.service, less 0.8 GB in use
  by other processes): give the machine more memory or set BLAZE_MEMORY_MAX
"""


def selftest() -> None:
    expect(events("low 0\nhigh 3\nmax 12\noom 1\noom_kill 2\noom_group_kill 0\n")
           == dict(low=0, high=3, max=12, oom=1, oom_kill=2, oom_group_kill=0), "memory.events is read")
    expect(cgroup_of("0::/user.slice/user-1000.slice/user@1000.service/app.slice/x.scope\n")
           == "/user.slice/user-1000.slice/user@1000.service/app.slice/x.scope", "a cgroup v2 path is read")
    expect(cgroup_of("12:memory:/x\n1:name=systemd:/y\n") is None, "cgroup v1 alone has no v2 path")
    why = refusal(DOC_REFUSAL)
    expect(why is not None and why.startswith("error: the test program of `hir-ty`") and why.endswith("may use 7.1 GB"),
           f"docs/memory-budget.md's refusal is read: {why!r}")
    expect(refusal("error: could not compile `x`\nmemory allocation of 1024 bytes failed\n") is None,
           "an allocation failure is no refusal")
    expect(limit_of(0.5, 561) == (280, 280 << 20) and limit_of(0.0001, 3) == (1, 1 << 20), "limits")
    argv = scope_argv("u", 1 << 30, ["python", "x"])
    expect(argv[:4] == ["systemd-run", "--user", "--scope", "--quiet"] and "MemorySwapMax=0" in argv
           and f"MemoryMax={1 << 30}" in argv and argv[-3:] == ["--", "python", "x"], f"the scope's command: {argv}")

    def done(status: int | None, said: str = "", **more: object) -> Done:
        return replace(Done(cgroup=None, seconds=10.0, build_status=status, said=said, not_blazes=None, hung=False,
                            error=None, peak_mb=100, cgroup_peak_mb=None, oom_kills=0, harness_mb=20), **more)

    cases: list[tuple[Ending, report.Verdict]] = [
        (Ending(0.5, 0, 0, done(0), True), "pass"),
        (Ending(None, 0, 0, done(0), True), "pass"),
        (Ending(0.5, 0, 0, done(0), False), "fail"),
        (Ending(0.5, 0, 0, done(0, not_blazes="cargo: loading"), True), "fail"),
        (Ending(0.5, 0, 1, done(101), None), "fail"),
        (Ending(0.5, 0, 0, done(1, DOC_REFUSAL), None), "pass"),
        (Ending(1.0, 0, 0, done(1, DOC_REFUSAL), None), "fail"),
        (Ending(None, 0, 0, done(1, DOC_REFUSAL), None), "fail"),
        (Ending(0.5, 0, 0, done(1, "error: could not compile"), None), "fail"),
        (Ending(0.5, 0, 0, done(-signal.SIGKILL), None), "fail"),
        (Ending(0.5, -signal.SIGKILL, 0, None, None), "fail"),
        (Ending(0.5, None, 0, None, None), "fail"),
        (Ending(0.5, 0, 0, done(None, hung=True, error="took more than"), None), "fail"),
        (Ending(0.5, 0, 0, done(None, error="the cold build: `blaze build` did not start"), None), "error"),
        (Ending(0.5, 1, 0, None, None, "Traceback"), "error"),
    ]
    for at, (ending, wanted) in enumerate(cases):
        got, said = judge(ending)
        expect(got == wanted, f"case {at}: {wanted} expected, {got}: {said}")
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "child":
        child(parse(Child, sys.argv[2], SCRIPT))
    elif len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest | child '<json>'\n\n{__doc__}")
    elif sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
