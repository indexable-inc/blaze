"""counters.py: the instructions and cycles blaze's processes retire, the same way on Linux and macOS.

The one helper the benchmarks count instructions with (bench.py, abbench.py, v2/budgets.py); a
harness that counts them imports this rather than reading counters itself, as memory.py is the
one way memory is measured. What it counts, and how far off it can be, is docs/benchmarks.md,
"Instruction counts". In short:

- Retired instructions move far less than wall time on a busy or noisy machine (rustc-perf's
  primary metric for the same reason), so a 1% change of a step is visible where its time is
  noise. Cycles are kept beside them.
- Linux: hardware counters from `perf_event_open(2)` (ctypes, no `perf` binary), user mode only
  (`exclude_kernel`, `exclude_hv`). `Meter.popen` starts a command stopped (`sh -c 'kill -STOP
  $$; exec ...'`), opens a counter on it with `inherit` and `enable_on_exec` and lets it go, so
  the command and every process and thread it starts from then on are counted, a daemon it
  starts included, for as long as the meter is open. `Meter.follow` opens counters on every
  thread of a process that is already running and of its descendants, with `inherit`, from now
  on. A counter reads what its task and all its inheriting children retired, those that exited
  too. It needs `kernel.perf_event_paranoid` at most 2 and a PMU (a VM without a virtual PMU,
  such as GitHub's hosted runners, has none).
- macOS: `proc_pid_rusage`'s `ri_instructions` and `ri_cycles` (user and kernel mode: macOS does
  not split them there), each a process's lifetime total. `Meter` reads the processes it was
  given and every descendant of them alive when it reads; a command `Meter.popen` started is
  read once more when it exited, before it is reaped (`Meter.exited`). A short-lived process
  below the daemon that comes and goes between two reads is not counted (`partial` says so
  when one the meter had seen is gone).
- Where neither can count (another OS, a refused or missing counter), a count is None with a
  `gap` naming why: never a zero, never a guess (AGENTS.md, no-fallback).

`Meter.settle` waits, at most `SETTLE_SECS`, until what is counted retires fewer than
`QUIET_INSTRUCTIONS` in `QUIET_SECS` (a daemon finishing work after it answered), so a step's
count is from quiet to quiet and holds the work it caused after its command returned. Python 3
standard library only.
"""

import ctypes
import errno
import os
import platform
import resource
import signal
import statistics
import struct
import subprocess
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from typing import Any, Self, cast

SYSTEM = platform.system()
DARWIN = SYSTEM == "Darwin"
LINUX = SYSTEM == "Linux"
# What the counts take in: Linux's counters are opened for user mode alone; macOS's totals are both.
MODE = "user" if LINUX else "user+kernel" if DARWIN else None
QUIET_SECS = 0.2
QUIET_INSTRUCTIONS = 2_000_000
SETTLE_SECS = 10.0


@dataclass(frozen=True)
class Counts:
    """What the counted processes retired over a step: None with `gap` when it could not be
    counted, and `partial` when it is a count that misses something it names."""

    instructions: int | None
    cycles: int | None
    gap: str | None = None
    partial: str | None = None


def unmeasured(gap: str) -> Counts:
    return Counts(None, None, gap)


def summary(samples: Sequence[Counts]) -> dict[str, Any]:
    """A phase's counts from its samples: the median of each, every sample of the instructions,
    and why there are none (`counts_gap`) or what they miss (`counts_partial`)."""
    out: dict[str, Any] = {"instructions": None, "cycles": None, "counts_mode": MODE}
    if not samples:
        out["counts_gap"] = "nothing was counted"
        return out
    gaps = [s.gap for s in samples if s.instructions is None]
    if gaps:
        some = f"{len(gaps)} of {len(samples)} samples: {gaps[0]}"
        out["counts_gap"] = gaps[0] if len(gaps) == len(samples) else some
        return out
    counted = [s.instructions for s in samples if s.instructions is not None]
    out["instructions"] = round(statistics.median(counted))
    cycles = [s.cycles for s in samples if s.cycles is not None]
    out["cycles"] = round(statistics.median(cycles)) if cycles else None
    out["instructions_samples"] = [s.instructions for s in samples]
    partial = next((s.partial for s in samples if s.partial), None)
    if partial:
        out["counts_partial"] = partial
    return out


@dataclass(frozen=True)
class Snapshot:
    """What every counter of a meter read at one moment: (instructions, cycles) by counter."""

    values: Mapping[str, tuple[int, int]] = field(default_factory=dict[str, tuple[int, int]])
    # Counters that could not be read this time, and why.
    unread: tuple[str, ...] = ()
    # False when `Meter.settle` gave up waiting for quiet.
    settled: bool = True


# macOS: struct rusage_info_v4 (<sys/resource.h>), a UUID, then 35 counters of 64 bits.
RUSAGE_INFO_V4 = 4
RUSAGE = {
    "user": 0,
    "system": 1,
    "footprint": 7,
    "lifetime_max_footprint": 28,
    "instructions": 29,
    "cycles": 30,
}


class _RusageInfo(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_uint8 * 16), ("counters", ctypes.c_uint64 * 35)]


_libproc: Any = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True) if DARWIN else None
_timebase: list[float] = []


def rusage(pid: int) -> dict[str, int] | None:
    """macOS: the counters of `pid`'s rusage_info_v4 by name (`RUSAGE`), its CPU time in
    nanoseconds as `cpu_ns`; None when it is gone (a zombie, not yet reaped, still answers, with
    the totals it exited with) or this is not macOS. The one reader of it: memory.py and
    abbench.py read it here."""
    if _libproc is None:
        return None
    info = _RusageInfo()
    if _libproc.proc_pid_rusage(pid, RUSAGE_INFO_V4, ctypes.byref(info)) != 0:
        return None
    got = {name: int(info.counters[i]) for name, i in RUSAGE.items()}
    if not _timebase:

        class Timebase(ctypes.Structure):
            _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]

        tb = Timebase()
        ctypes.CDLL("/usr/lib/libSystem.dylib").mach_timebase_info(ctypes.byref(tb))
        _timebase.append(tb.numer / tb.denom)
    got["cpu_ns"] = round((got["user"] + got["system"]) * _timebase[0])
    return got


def lifetime(pids: Sequence[int]) -> Counts:
    """What `pids` retired since each started, summed: macOS keeps it; Linux keeps no such total
    of a process, only what a counter opened before saw (`Meter`)."""
    if not DARWIN:
        return unmeasured(
            f"{SYSTEM} keeps no lifetime count of a process: only a counter opened before it ran sees it"
        )
    found = [u for u in map(rusage, pids) if u]
    if not found:
        return unmeasured(f"proc_pid_rusage answered for none of {list(pids)}")
    gone = len(pids) - len(found)
    partial = f"{gone} of {len(pids)} processes were gone" if gone else None
    return Counts(sum(u["instructions"] for u in found), sum(u["cycles"] for u in found), partial=partial)


# Linux: perf_event_open(2).
_PERF_SYSCALL = {"x86_64": 298, "aarch64": 241, "arm64": 241, "riscv64": 241}
PERF_TYPE_HARDWARE = 0
PERF_COUNT_HW_CPU_CYCLES, PERF_COUNT_HW_INSTRUCTIONS = 0, 1
PERF_FORMAT_TOTAL_TIME_ENABLED, PERF_FORMAT_TOTAL_TIME_RUNNING = 1, 2
PERF_FLAG_FD_CLOEXEC = 8
# The bits of perf_event_attr's flags word.
DISABLED, INHERIT, EXCLUDE_KERNEL, EXCLUDE_HV, ENABLE_ON_EXEC = 1 << 0, 1 << 1, 1 << 5, 1 << 6, 1 << 12


class _PerfEventAttr(ctypes.Structure):
    """struct perf_event_attr up to PERF_ATTR_SIZE_VER5 (112 bytes)."""

    _fields_ = [
        ("type", ctypes.c_uint32),
        ("size", ctypes.c_uint32),
        ("config", ctypes.c_uint64),
        ("sample_period", ctypes.c_uint64),
        ("sample_type", ctypes.c_uint64),
        ("read_format", ctypes.c_uint64),
        ("flags", ctypes.c_uint64),
        ("wakeup_events", ctypes.c_uint32),
        ("bp_type", ctypes.c_uint32),
        ("config1", ctypes.c_uint64),
        ("config2", ctypes.c_uint64),
        ("branch_sample_type", ctypes.c_uint64),
        ("sample_regs_user", ctypes.c_uint64),
        ("sample_stack_user", ctypes.c_uint32),
        ("clockid", ctypes.c_int32),
        ("sample_regs_intr", ctypes.c_uint64),
        ("aux_watermark", ctypes.c_uint32),
        ("sample_max_stack", ctypes.c_uint16),
        ("reserved_2", ctypes.c_uint16),
    ]


_libc: Any = ctypes.CDLL(None, use_errno=True) if LINUX else None


def paranoid() -> str:
    try:
        with open("/proc/sys/kernel/perf_event_paranoid") as f:
            return f.read().strip()
    except OSError:
        return "unreadable"


def refusal(code: int, what: str) -> str:
    """Why perf_event_open refused a counter, as a gap."""
    name = errno.errorcode.get(code, str(code))
    if code in (errno.EACCES, errno.EPERM):
        return (
            f"perf_event_open refused {what} ({name}): kernel.perf_event_paranoid is {paranoid()}, and "
            "counting the user mode of one's own processes needs at most 2 "
            "(`sudo sysctl kernel.perf_event_paranoid=2`)"
        )
    if code in (errno.ENOENT, errno.EOPNOTSUPP, errno.ENODEV, errno.EINVAL):
        return (
            f"perf_event_open has no hardware counter for {what} here ({name}): the CPU or the VM "
            "exposes no PMU (GitHub's hosted runners do not)"
        )
    return f"perf_event_open failed for {what}: {name}"


def perf_open(pid: int, config: int, flags: int) -> int:
    """A counter of `config` on `pid`, its file descriptor; OSError with the errno when refused."""
    number = _PERF_SYSCALL.get(platform.machine())
    if _libc is None or number is None:
        raise OSError(errno.ENOSYS, f"no perf_event_open on {SYSTEM} {platform.machine()}")
    attr = _PerfEventAttr()
    attr.type, attr.size, attr.config = PERF_TYPE_HARDWARE, ctypes.sizeof(_PerfEventAttr), config
    attr.read_format = PERF_FORMAT_TOTAL_TIME_ENABLED | PERF_FORMAT_TOTAL_TIME_RUNNING
    attr.flags = flags | EXCLUDE_KERNEL | EXCLUDE_HV
    fd = int(
        _libc.syscall(
            ctypes.c_long(number),
            ctypes.byref(attr),
            ctypes.c_int(pid),
            ctypes.c_int(-1),
            ctypes.c_int(-1),
            ctypes.c_ulong(PERF_FLAG_FD_CLOEXEC),
        )
    )
    if fd < 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code))
    return fd


def perf_read(fd: int) -> tuple[int, str | None]:
    """A counter's value, scaled when the kernel multiplexed it, with what is wrong with it."""
    value, enabled, running = struct.unpack("QQQ", os.read(fd, 24))
    if enabled == 0:
        # Not enabled yet (a command not yet past its exec): nothing to count.
        return 0, None
    if running == 0:
        return 0, "the counter was never on the PMU (other users of perf took it)"
    if running < enabled:
        return round(value * enabled / running), None
    return value, None


def start_ticks(pid: int) -> int | None:
    """Linux: when `pid` started, in clock ticks since boot (/proc/PID/stat's 22nd field)."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            return int(f.read().rsplit(")", 1)[1].split()[19])
    except (OSError, ValueError, IndexError):
        return None


def descendants(pid: int) -> list[int]:
    """`pid` and every process below it that still runs."""
    children: Callable[[int], list[int]]
    if LINUX:
        parent: dict[int, list[int]] = {}
        for entry in os.listdir("/proc"):
            if not entry.isdigit():
                continue
            try:
                with open(f"/proc/{entry}/stat") as f:
                    ppid = int(f.read().rsplit(")", 1)[1].split()[1])
            except (OSError, ValueError, IndexError):
                continue
            parent.setdefault(ppid, []).append(int(entry))
        children = lambda p: parent.get(p, [])
    else:
        # memory.py lists children (its Sampler follows the same trees); imported here, as
        # memory.py imports this module for rusage.
        import memory

        # memory.py is untyped: through getattr, so its type is the one cast here.
        children = cast(Callable[[int], list[int]], getattr(memory, "children"))  # noqa: B009
    found: list[int] = []
    todo = [pid]
    while todo:
        p = todo.pop()
        found.append(p)
        todo.extend(children(p))
    return found


class Meter:
    """Counts the processes it is given, and what they start, from when it is given them until
    it is closed. `snapshot` reads every counter; `since` is what they retired between two."""

    def __init__(self) -> None:
        self.gap: str | None = None if MODE else f"no instruction counters on {SYSTEM}"
        if LINUX and platform.machine() not in _PERF_SYSCALL:
            self.gap = f"no perf_event_open syscall number known for {platform.machine()}"
        # Linux: the two counters (instructions, cycles) by key; macOS: the processes followed.
        self._fds: dict[str, tuple[int, int]] = {}
        self._first_root: int | None = None
        self._pids: set[int] = set()
        self._final: dict[int, tuple[int, int]] = {}
        if LINUX:
            # Two descriptors for every command counted: let a long run have them.
            soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
            want = max(soft, 65536) if hard == resource.RLIM_INFINITY else hard
            if want > soft:
                resource.setrlimit(resource.RLIMIT_NOFILE, (want, hard))

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    def close(self) -> None:
        for fds in self._fds.values():
            for fd in fds:
                os.close(fd)
        self._fds.clear()

    def _open(self, key: str, pid: int, flags: int) -> bool:
        if self.gap is not None:
            return False
        opened: list[int] = []
        try:
            for config in (PERF_COUNT_HW_INSTRUCTIONS, PERF_COUNT_HW_CPU_CYCLES):
                opened.append(perf_open(pid, config, flags))
        except OSError as e:
            for fd in opened:
                os.close(fd)
            if e.errno == errno.ESRCH:
                return False
            self.gap = refusal(e.errno or 0, f"pid {pid}")
            return False
        self._fds[key] = (opened[0], opened[1])
        return True

    def popen(self, argv: Sequence[str], **kwargs: Any) -> "subprocess.Popen[Any]":
        """`subprocess.Popen(argv, **kwargs)`, counted with everything it starts. On Linux the
        command runs under `sh`, stopped until its counter is open; its pid is the command's."""
        if not LINUX or self.gap is not None:
            child: subprocess.Popen[Any] = subprocess.Popen(list(argv), **kwargs)
            if DARWIN:
                self._pids.add(child.pid)
            return child
        gated = ["/bin/sh", "-c", 'kill -STOP $$ && exec "$@"', "counted", *argv]
        child = subprocess.Popen(gated, **kwargs)
        try:
            # Until it has stopped itself (or died before it could, which `wait` then reports).
            info = os.waitid(os.P_PID, child.pid, os.WSTOPPED | os.WEXITED | os.WNOWAIT)
            stopped = info is not None and info.si_code in (os.CLD_STOPPED, os.CLD_TRAPPED)
            opened = stopped and self._open(
                f"root:{child.pid}", child.pid, DISABLED | INHERIT | ENABLE_ON_EXEC
            )
            if opened and self._first_root is None:
                self._first_root = start_ticks(child.pid)
        finally:
            os.kill(child.pid, signal.SIGCONT)
        return child

    def exited(self, pid: int) -> None:
        """`pid`, a command `popen` started, has exited and is not reaped yet: macOS reads its
        totals now, the last time it can. Linux's counter keeps them by itself."""
        if DARWIN and (got := rusage(pid)):
            self._final[pid] = (got["instructions"], got["cycles"])

    def adopt(self, pid: int) -> None:
        """A process a counted command started that may have left its tree since (a daemon,
        reparented): Linux's counters already follow it when it started after the first counted
        command did (inherit); one that is older is followed from now; macOS reads it from now."""
        if LINUX and self._first_root is not None:
            started = start_ticks(pid)
            if started is not None and started >= self._first_root:
                return
        self.follow(pid)

    def follow(self, pid: int) -> None:
        """A process no counted command started (a daemon already running), and every process
        below it, counted from now: on Linux every thread of each, with inherit."""
        if DARWIN:
            self._pids.add(pid)
            return
        if not LINUX:
            return
        for p in descendants(pid):
            try:
                tids = [int(t) for t in os.listdir(f"/proc/{p}/task")]
            except OSError:
                continue
            for tid in tids:
                if f"task:{tid}" not in self._fds:
                    self._open(f"task:{tid}", tid, INHERIT)

    def snapshot(self) -> Snapshot:
        values: dict[str, tuple[int, int]] = {}
        unread: list[str] = []
        if LINUX:
            for key, (instructions, cycles) in self._fds.items():
                (i, why_i), (c, why_c) = perf_read(instructions), perf_read(cycles)
                if why_i or why_c:
                    unread.append(f"{key}: {why_i or why_c}")
                values[key] = (i, c)
        elif DARWIN:
            for root in list(self._pids):
                self._pids.update(descendants(root))
            for pid in sorted(self._pids):
                if (got := rusage(pid)) is not None:
                    values[f"pid:{pid}"] = (got["instructions"], got["cycles"])
                elif pid in self._final:
                    values[f"pid:{pid}"] = self._final[pid]
            # A process gone and reaped without `exited` is read no more.
            self._pids = {p for p in self._pids if f"pid:{p}" in values}
        return Snapshot(values, tuple(unread))

    def since(self, before: Snapshot, now: Snapshot | None = None) -> Counts:
        """What was retired from `before` to `now` (a fresh snapshot when not given)."""
        if self.gap is not None and not self._fds and not self._pids:
            return unmeasured(self.gap)
        if not MODE:
            return unmeasured(f"no instruction counters on {SYSTEM}")
        now = now if now is not None else self.snapshot()
        if now.unread:
            return unmeasured("; ".join(now.unread[:3]))
        if not now.values:
            return unmeasured(self.gap or "the meter was given no process to count")
        instructions = sum(v[0] - before.values.get(k, (0, 0))[0] for k, v in now.values.items())
        cycles = sum(v[1] - before.values.get(k, (0, 0))[1] for k, v in now.values.items())
        notes: list[str] = []
        if gone := [k for k in before.values if k not in now.values]:
            notes.append(f"{len(gone)} counted processes exited unread between the two readings")
        if not (before.settled and now.settled):
            notes.append(f"the processes were still busy after {SETTLE_SECS:.0f} s of waiting for quiet")
        if self.gap is not None:
            notes.append(self.gap)
        return Counts(instructions, cycles, partial="; ".join(notes) or None)

    def settle(self) -> Snapshot:
        """A snapshot once the counted processes have gone quiet: fewer than
        `QUIET_INSTRUCTIONS` retired in `QUIET_SECS`, waited for at most `SETTLE_SECS` (then
        `settled` is False, which `since` names)."""
        last = self.snapshot()
        if self.gap is not None and not last.values:
            return last
        began = time.monotonic()
        while time.monotonic() - began < SETTLE_SECS:
            time.sleep(QUIET_SECS)
            now = self.snapshot()
            moved = self.since(last, now).instructions
            if moved is None or moved < QUIET_INSTRUCTIONS:
                return now
            last = now
        return Snapshot(last.values, last.unread, settled=False)


def selftest() -> None:
    """What needs no counter: the summary of samples, and the struct's size."""
    assert ctypes.sizeof(_PerfEventAttr) == 112, ctypes.sizeof(_PerfEventAttr)
    s = summary([Counts(100, 200), Counts(300, 400), Counts(200, 300, partial="p")])
    assert s["instructions"] == 200 and s["cycles"] == 300 and s["instructions_samples"] == [100, 300, 200], s
    assert s["counts_partial"] == "p", s
    s = summary([Counts(100, 200), unmeasured("no PMU")])
    assert s["instructions"] is None and s["counts_gap"] == "1 of 2 samples: no PMU", s
    meter = Meter()
    before = Snapshot({"a": (10, 20), "b": (5, 5)})
    got = meter.since(before, Snapshot({"a": (15, 30), "c": (1, 1)}))
    if MODE and meter.gap is None:
        assert got.instructions == 6 and got.cycles == 11 and got.partial and "1 counted" in got.partial, got
    print("counters.py selftest: ok")


if __name__ == "__main__":
    selftest()
