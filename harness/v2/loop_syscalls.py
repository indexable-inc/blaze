"""loop_syscalls.py: the daemon's loop thread makes no slow system call during an edit's build.

    uv run --frozen python harness/v2/loop_syscalls.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/loop-syscalls.json"}'
    uv run --frozen python harness/v2/loop_syscalls.py '{"engine": {...}, "out": "...", "max_ms": 20.0, "work": "/mnt/xfs/loop"}'
    uv run --frozen python harness/v2/loop_syscalls.py selftest

The input is one JSON object (inline, a file's path, or `-`):

    command     `run` (the default) or `selftest` (which needs nothing else, as the bare word does)
    engine      the blaze measured (engine.EngineSpec); `run` needs one
    out         the result's path (report.py's JSON); none written when empty
    work        where the checkout and its daemon home go: a directory on a volume that clones
                (engine.work_dir); a new one when empty
    max_ms      the longest one system call of the loop thread may take, other than its own
                waits (20.0)
    timeout_s   each build's bound (600.0)
    keep        keep the work directory, with the trace in it (false)

One check, `the_loop_thread_makes_no_slow_syscall`. The finding it is for (2026-11-10, Linux,
XFS): after a one-line body edit of a hello world the daemon's loop thread sat in
`ioctl(FICLONE)` for 0.9 and 1.4 s, and the build's answer came 3.2 s late. budgets.py holds
the same from inside, by the profile's `loop stalled` spans; this holds it from outside, so a
stall the daemon's watchdog does not see (or a watchdog that is gone) still fails.

`run` writes the workspace budgets.py's smoke builds (fixture.py's, `budgets.smoke_source`),
builds it cold and once more warm in a daemon home of its own, attaches `strace -f -T -tt -y -o
FILE -p PID` to the daemon (its pid from its lock, `budgets.daemon_pid`), applies the smoke's
body edit, runs `blaze build`, detaches strace and reads the trace. The loop thread is the
daemon's main thread (tid == pid). The check fails when a system call of that thread took more
than `max_ms`, unless it is one of the loop's own waits (`WAITS`), and prints each such call:
the system call, its arguments, its seconds. A call that had not returned when the trace ended
is held to the time from its start to the trace's last line. A trace with no call of the loop
thread, or with a line of it that cannot be read, is an error: nothing is passed unseen.

Linux only: strace. On any other system `run` stops with "loop_syscalls: Linux only (strace)".
With no strace on PATH, or where strace may not attach (ptrace refused: the daemon is no child
of it, so `kernel.yama.ptrace_scope` has to be 0 or the run root's), it stops saying so; there is
no other way to see the thread's system calls, and none is tried.

`selftest` needs neither blaze nor strace: it reads literal lines of a trace.
"""

import re
import shutil
import signal
import subprocess
import sys
import time
from collections.abc import Iterable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse
import budgets
import engine
import report
from engine import EngineSpec

SCRIPT = "loop_syscalls.py"
CHECK = "the_loop_thread_makes_no_slow_syscall"
# The system calls the loop waits in: a turn that has nothing to do blocks here, for as long as
# nothing happens. `restart_syscall` is one of them taken up again after strace's attach
# interrupted it.
WAITS = frozenset({"epoll_wait", "epoll_pwait", "epoll_pwait2", "io_uring_enter", "futex", "ppoll", "poll", "kevent",
                   "restart_syscall"})
ATTACH_SECS = 30.0
DETACH_SECS = 30.0
# The platform as a plain string, so a type check on any one platform reads every branch.
PLATFORM: str = sys.platform


@dataclass(frozen=True)
class Input:
    engine: EngineSpec | None = None
    out: str = ""
    command: Literal["run", "selftest"] = "run"
    work: str = ""
    max_ms: float = 20.0
    timeout_s: float = 600.0
    keep: bool = False


@dataclass(frozen=True)
class Call:
    """One system call of a trace: the thread that made it, its name, the call as strace wrote it
    (an unfinished call joined with its resumption), and how long it took. `returned` is false
    for one the trace ended inside, whose seconds run to the trace's last line."""

    tid: int
    syscall: str
    text: str
    seconds: float
    returned: bool = True

    def shown(self) -> str:
        return f"{self.syscall}: {self.text}: {self.seconds:.6f} s" + ("" if self.returned else " and had not returned")


@dataclass(frozen=True)
class Trace:
    """What a trace's lines say: its calls in the order they ended, and the lines that are no
    call, signal or exit, each with its thread."""

    calls: tuple[Call, ...]
    unread: tuple[tuple[int, str], ...]


# A line of `strace -f -tt -o FILE`: the thread, the time of day, the rest.
LINE = re.compile(r"^(\d+)\s+(?:(\d\d):(\d\d):(\d\d\.\d+)\s+)?(.*)$")
NAME = re.compile(r"^(\w+)\(")
RESUMED = re.compile(r"^<\.\.\. (\w+) resumed>\s?(.*)$")
TOOK = re.compile(r"<(\d+\.\d+)>$")
CUT = re.compile(r"\s*<(?:unfinished|detached) \.\.\.>$")


def read_trace(lines: Iterable[str]) -> Trace:
    """The calls of a trace. An `<unfinished ...>` call is joined, by its thread, with the `<...
    NAME resumed>` line that ends it, which carries the time `-T` measured. One never resumed
    (the trace ended, or strace detached, inside it) took from its start to the last time of day
    in the trace. Signals (`---`), exits (`+++`) and calls that never return (`= ?`, no time)
    are no calls."""
    calls: list[Call] = []
    unread: list[tuple[int, str]] = []
    open_calls: dict[int, tuple[str, str, float | None]] = {}
    last: float | None = None
    for raw in lines:
        line = raw.rstrip("\n")
        if not line.strip():
            continue
        found = LINE.match(line)
        if found is None:
            unread.append((0, line))
            continue
        tid, rest = int(found[1]), found[5]
        at = None if found[2] is None else int(found[2]) * 3600 + int(found[3]) * 60 + float(found[4])
        if at is not None:
            # The time of day, past midnight: the trace is in order.
            while last is not None and at < last - 43200:
                at += 86400
            last = at
        if rest.startswith(("---", "+++")):
            continue
        took = TOOK.search(rest)
        if resumed := RESUMED.match(rest):
            begun = open_calls.pop(tid, None)
            if begun is None or begun[0] != resumed[1] or took is None:
                unread.append((tid, line))
                continue
            calls.append(Call(tid, resumed[1], begun[1] + resumed[2], float(took[1])))
        elif (name := NAME.match(rest)) is None:
            unread.append((tid, line))
        elif CUT.search(rest):
            open_calls[tid] = (name[1], CUT.sub("", rest), at)
        elif took is not None:
            calls.append(Call(tid, name[1], rest, float(took[1])))
        elif not rest.endswith("= ?"):
            unread.append((tid, line))
    for tid, (name, text, began) in open_calls.items():
        seconds = last - began if last is not None and began is not None else 0.0
        calls.append(Call(tid, name, text + " <unfinished ...>", seconds, returned=False))
    return Trace(tuple(calls), tuple(unread))


def slow(calls: Sequence[Call], loop_tid: int, max_ms: float) -> list[Call]:
    """The calls of the thread `loop_tid` that took more than `max_ms` and are not its waits."""
    return [c for c in calls if c.tid == loop_tid and c.syscall not in WAITS and c.seconds * 1000 > max_ms]


def attach(strace: str, pid: int, trace: Path, said: Path) -> subprocess.Popen[bytes]:
    """strace attached to every thread of `pid`, writing `trace`; what it says goes to `said`.
    It is attached once it says so; one that ends first, or says nothing in `ATTACH_SECS`, is an
    engine.Failed with what it said."""
    with open(said, "wb") as err:
        proc = subprocess.Popen([strace, "-f", "-T", "-tt", "-y", "-o", str(trace), "-p", str(pid)],
                                stdin=subprocess.DEVNULL, stdout=err, stderr=err)
    began = time.monotonic()
    while time.monotonic() - began < ATTACH_SECS:
        text = said.read_text(errors="replace")
        if proc.poll() is not None:
            raise engine.Failed(f"strace could not attach to the daemon {pid} (exit {proc.returncode}): {text.strip()}\n"
                                "the daemon is no child of strace: attaching needs kernel.yama.ptrace_scope = 0 "
                                "(/proc/sys/kernel/yama/ptrace_scope) or root")
        if "attached" in text:
            return proc
        time.sleep(0.01)
    proc.kill()
    proc.wait()
    raise engine.Failed(f"strace did not say it attached to the daemon {pid} in {ATTACH_SECS:.0f} s: "
                        f"{said.read_text(errors='replace').strip()}")


def detach(proc: subprocess.Popen[bytes]) -> None:
    """strace told to detach (SIGINT), and waited for: its trace is whole once it has ended."""
    if proc.poll() is None:
        proc.send_signal(signal.SIGINT)
    try:
        proc.wait(DETACH_SECS)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        raise engine.Failed(f"strace did not detach in {DETACH_SECS:.0f} s of its SIGINT") from None


def check(eng: engine.Engine, rep: report.Report, args: Input, work: Path, strace: str) -> None:
    top = work / "fixture"
    shutil.rmtree(top, ignore_errors=True)
    top.mkdir(parents=True)
    spec, ws = budgets.smoke_source(top / "src")
    home = top / "home"
    env = eng.env(home)
    build_args: list[str] = list(spec["build"])
    changes: list[budgets.Change] = spec["edits"]["body"]
    trace, said = top / "loop.strace", top / "strace.stderr"

    def build(step: str) -> engine.Ran:
        return eng.build(step, ws, env, build_args, timeout=args.timeout_s).checked(step)

    try:
        build("the cold build")
        eng.wait_ready(ws, env)
        build("the build that warms up")
        pid = budgets.daemon_pid(home)
        proc = attach(strace, pid, trace, said)
        try:
            budgets.write_edit(str(ws), changes, False)
            try:
                ran = build("the body edit's build, under strace")
            finally:
                budgets.write_edit(str(ws), changes, True)
        finally:
            detach(proc)
    finally:
        eng.stop(ws, env)
    with open(trace, errors="replace") as f:
        read = read_trace(f)
    mine = [c for c in read.calls if c.tid == pid]
    unread = [line for tid, line in read.unread if tid in (pid, 0)]
    if not mine:
        raise engine.Failed(f"the trace {trace} has no system call of the daemon's main thread {pid} "
                            f"({len(read.calls)} calls of other threads): the loop thread was not seen")
    if unread:
        raise engine.Failed(f"the trace {trace} has {len(unread)} lines of the daemon's main thread {pid} that are no "
                            "call this script reads:\n" + "\n".join(unread[:10]))
    found = slow(read.calls, pid, args.max_ms)
    worst = max((c for c in mine if c.syscall not in WAITS), key=lambda c: c.seconds, default=None)
    numbers: dict[str, report.Number] = dict(
        loop_calls=len(mine), other_calls=len(read.calls) - len(mine), slow=len(found), max_ms=args.max_ms,
        slowest_ms=None if worst is None else round(worst.seconds * 1000, 3),
        slowest=None if worst is None else worst.syscall, build_ms=round(ran.seconds * 1000, 3))
    if found:
        rep.add(CHECK, "fail", f"{len(found)} system calls of the daemon's loop thread {pid} took more than "
                f"{args.max_ms:.4g} ms over the body edit's build:\n" + "\n".join(c.shown() for c in found), **numbers)
        for c in found:
            print(f"{SCRIPT}: {c.shown()}", flush=True)
    else:
        rep.add(CHECK, "pass", f"{len(mine)} system calls of the daemon's loop thread {pid} over the body edit's build, "
                f"none but its waits over {args.max_ms:.4g} ms" + ("" if worst is None else f"; the slowest: {worst.shown()}"),
                **numbers)


def main(args: Input) -> None:
    if args.command == "selftest":
        selftest()
        return
    if PLATFORM != "linux":
        raise SystemExit("loop_syscalls: Linux only (strace)")
    if args.engine is None:
        fail(SCRIPT, "engine: command `run` measures a blaze, and was given none")
    if not args.max_ms > 0 or not args.timeout_s > 0:
        fail(SCRIPT, f"max_ms and timeout_s: more than 0, not {args.max_ms} and {args.timeout_s}")
    strace = shutil.which("strace")
    if strace is None:
        fail(SCRIPT, "no strace on PATH: the loop thread's system calls are read with it (install strace on the VM)")
    try:
        eng = engine.Engine.of(args.engine)
        work = engine.work_dir(args.work, "bz-loop-syscalls-")
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    rep = report.Report(SCRIPT, args.engine, args.out)
    print(f"work: {work}", flush=True)
    try:
        rep.guarded(CHECK, lambda: check(eng, rep, args, work, strace))
    except budgets.HARNESS_ERRORS as e:
        rep.add(CHECK, "error", f"{type(e).__name__}: {e}")
    rep.extra.update(work=str(work), blaze=getattr(eng, "version", ""))
    if not args.keep:
        shutil.rmtree(work / "fixture", ignore_errors=True)
        if not args.work:
            shutil.rmtree(work, ignore_errors=True)
    rep.finish()


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


# The finding of 2026-11-10 as strace wrote it: the daemon 4242, whose main thread is 4242.
CLONE = "ioctl(12</x/staged>, FICLONE, 13</home/v2/store/placed/abcd>) = 0 <0.889012>"
TRACED = f"""4242 10:00:00.000100 epoll_pwait(4<anon_inode:[eventpoll]>, [], 64, -1, NULL, 8) = 1 <0.250000>
4242 10:00:00.250200 read(9<socket:[77]>, "\\x01\\x02", 4096) = 2 <0.000011>
4242 10:00:00.250300 {CLONE}
4250 10:00:00.250400 {CLONE}
4242 10:00:01.139500 io_uring_enter(5<anon_inode:[io_uring]>, 1, 1, IORING_ENTER_GETEVENTS, NULL, 8) = 1 <1.400000>
4251 10:00:01.139600 futex(0x7f00, FUTEX_WAIT_PRIVATE, 2, NULL <unfinished ...>
4242 10:00:02.539700 fsync(14</home/v2/store/packs/0.pack> <unfinished ...>
4250 10:00:02.539800 write(15</x/out>, "a) = 1 <9.999999>", 17) = 17 <0.000020>
4251 10:00:02.600000 <... futex resumed>) = 0 <1.460400>
4250 10:00:02.700000 --- SIGCHLD {{si_signo=SIGCHLD, si_code=CLD_EXITED, si_pid=4300, si_uid=1000, si_status=0}} ---
4242 10:00:02.839900 <... fsync resumed>) = 0 <0.300200>
4242 10:00:02.840000 futex(0x7f08, FUTEX_WAKE_PRIVATE, 1) = 1 <0.000004>
4260 10:00:02.840100 exit(0) = ?
4260 10:00:02.840200 +++ exited with 0 +++
"""


def selftest() -> None:
    lines = TRACED.splitlines()
    read = read_trace(lines)
    expect(not read.unread and len(read.calls) == 9, f"every line is a call, a signal or an exit: {read}")
    found = slow(read.calls, 4242, 20.0)
    expect([c.syscall for c in found] == ["ioctl", "fsync"], f"the clone and the sync on the main thread fail: {found}")
    clone, sync = found
    expect(clone.seconds == 0.889012 and clone.text == CLONE and clone.shown() == f"ioctl: {CLONE}: 0.889012 s",
           f"the clone is printed with its arguments and its seconds: {clone.shown()}")
    expect(sync.seconds == 0.3002 and sync.text == "fsync(14</home/v2/store/packs/0.pack>) = 0 <0.300200>" and sync.returned,
           f"an unfinished call is joined with its resumption, whose time it is: {sync}")
    expect(not slow(read.calls, 4250, 1e6) and [c.syscall for c in slow(read.calls, 4250, 20.0)] == ["ioctl"]
           and all(c.tid == 4242 for c in found), "the same clone on another thread is that thread's, not the loop's")
    waited = [c for c in read.calls if c.tid == 4242 and c.syscall in WAITS]
    expect([c.syscall for c in waited] == ["epoll_pwait", "io_uring_enter", "futex"] and max(c.seconds for c in waited) == 1.4,
           f"the loop's own waits pass, however long: {waited}")
    expect([(c.tid, c.seconds) for c in read.calls if c.syscall == "futex" and c.tid == 4251] == [(4251, 1.4604)],
           "an unfinished call is joined by its thread")
    expect(any(c.syscall == "write" and c.seconds == 0.00002 for c in read.calls),
           "a time inside a string is not the call's: the last one on the line is")
    expect(not slow(read.calls, 4242, 900.0) and len(slow(read.calls, 4242, 300.0)) == 2 and len(slow(read.calls, 4242, 301.0)) == 1,
           "the bound is `max_ms`")
    # A pair over the bound alone, and a trace that ends inside a call.
    pair = ["4242 10:00:00.000000 ioctl(12</x/staged>, FICLONE <unfinished ...>", "4250 10:00:00.500000 getpid() = 4242 <0.000001>",
            "4242 10:00:01.400000 <... ioctl resumed>, 13</y>) = 0 <1.400000>"]
    joined = slow(read_trace(pair).calls, 4242, 20.0)
    expect(len(joined) == 1 and joined[0].seconds == 1.4 and joined[0].text == "ioctl(12</x/staged>, FICLONE, 13</y>) = 0 <1.400000>",
           f"an unfinished and resumed pair over the bound on the main thread fails: {joined}")
    cut = read_trace(pair[:2] + ["4250 10:00:03.000000 getpid() = 4242 <0.000001>"])
    inside = slow(cut.calls, 4242, 20.0)
    expect(len(inside) == 1 and not inside[0].returned and inside[0].seconds == 3.0 and "had not returned" in inside[0].shown(),
           f"a call the trace ends inside took until the trace's last line: {inside}")
    detached = read_trace(["4242 23:59:59.900000 epoll_wait(4, <detached ...>", "4250 00:00:00.100000 getpid() = 1 <0.000001>"])
    expect(not slow(detached.calls, 4242, 20.0) and abs(detached.calls[-1].seconds - 0.2) < 1e-6,
           f"a wait strace detached inside passes, and midnight is no day back: {detached}")
    odd = read_trace(["4242 10:00:00.000000 <... read resumed>) = 1 <0.5>", "4242 10:00:00.100000 what is this", "no thread"])
    expect([tid for tid, _ in odd.unread] == [4242, 4242, 0] and not odd.calls, f"a line that is no call is kept as unread: {odd}")
    bare = read_trace([f"4242 {CLONE}"])
    expect(len(slow(bare.calls, 4242, 20.0)) == 1, "a line with no time of day (no `-tt`) is read the same")
    given = parse(Input, '{"engine": {"kind": "v2", "blaze": "blaze"}, "out": "o.json"}', SCRIPT)
    expect(given.command == "run" and given.max_ms == 20.0 and not given.keep, f"the input's defaults: {given}")
    expect(parse(Input, '{"command": "selftest"}', SCRIPT).engine is None, "`selftest` needs no engine")
    for bad in ('{"engine": {"kind": "v2", "blaze": "blaze"}, "max_ms": 20}', '{"command": "trace"}', '{"pid": 1}'):
        try:
            parse(Input, bad, SCRIPT)
            expect(False, f"the input {bad} is refused")
        except SystemExit:
            pass
    if PLATFORM != "linux":
        try:
            main(given)
            expect(False, "`run` is refused off Linux")
        except SystemExit as e:
            expect(str(e) == "loop_syscalls: Linux only (strace)", f"what `run` says off Linux: {e}")
    ledger = (engine.ROOT / "docs/v2/plan/objectives.json").read_text()
    expect(f'"script": "harness/v2/{SCRIPT}"' in ledger, "a row of the objectives ledger runs this script")
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
