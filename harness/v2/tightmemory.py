"""tightmemory.py: the check of ledger row T9 ("No build is killed for memory; peak is at most
cargo's"): the same projects built by cargo and by blaze on one machine, and blaze's memory held
to cargo's peak.

    uv run --frozen python harness/v2/tightmemory.py '{"blaze": "<blaze>", "work": "<empty dir>"}'
    uv run --frozen python harness/v2/tightmemory.py selftest

The input is one JSON object (inline, a path, or `-`):

    blaze     the blaze binary under test, required
    work      a directory the run may create and delete: absent or empty (a new temporary one)
    out       where the result goes (`<work>/tightmemory.json`); the ledger hands it
    cargo     the cargo command, as argv (["cargo"])
    projects  fixtures of harness/v2/versus, of "hello" and "six" (["hello", "six"])
    extra     real checkouts, each {"name", "dir", "edit": {"file", "find", "replace"}, "needs_gb"}:
              built from a copy of `dir`, the edit's `find` replaced once in `file`. One whose
              machine has less memory than `needs_gb` (32) is skipped, and said
    runs      samples per tool and project (3)
    cap_gb    the most blaze's processes may hold at once before this script stops them (12):
              the machine's guard, whatever blaze's own ceiling does
    root      the checkout (".")

Per project and run, each in a fresh copy (a fixture's manifest and lock files, with the program
smoke.rs writes, read from it) and, for blaze, a fresh HOME and BLAZE_HOME (its store follows HOME)
holding only a link to the installed toolchain shelf, each command under a deadline with only
HOME, PATH, TERM=dumb and LANG=C.UTF-8 handed on (and BLAZE_HOME):

    cargo     `cargo build`, then the body edit and `cargo build` again: cargo's peak is the most
              cargo, rustc, build scripts and the linker held at once in either
    cold      `blaze run` from nothing: the most blaze and everything it started held at once
    edit      the body edit and `blaze run` again
    held      what those processes still hold 1 s after the edit's run returned (a daemon, or
              nothing); they are then stopped
    ceiling   once per project: `blaze run` from nothing with BLAZE_DAEMON_MEMORY_GB and this
              script's own stop both at cargo's peak rounded up to whole GiB

Memory is harness/memory.py's (resident on Linux, footprint on macOS; docs/benchmarks.md,
"Memory and disk"), sampled every 10 ms. A row is one median per tool (no sample dropped, no
retry, no slack): cold, edit and held are met when blaze's median is at most cargo's peak's
median; ceiling is met when that run exits 0 (it is lost when blaze exits 77, its own ceiling,
or this script stops it there). A run this script stops at `cap_gb` loses its row with the
most it held then.

Output: `out` (every sample, the machine, blaze's version, the date, and one outcome a row in
report.py's shape, which the ledger reads to say why a run failed) and the table. Exit 0 when every row is met, 1 when any is lost, 2 when a run failed for another reason
(its exit status and last lines are the record), 3 when none is lost but a project was skipped or
no `extra` checkout was measured (the fixtures alone are too small to decide T9).
"""

import json
import os
import platform
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, Self, cast

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
import memory
from typed_input import parse

SCRIPT = "tightmemory.py"
# harness/memory.py has no annotations: its calls are typed where this script uses them.
MEMORY: Any = memory
FIXTURES = "harness/v2/versus"
FIXTURE_FILES = ("Cargo.toml", "Cargo.lock", "blaze.lock")
# blaze-core's exit.rs: "its footprint passed its memory ceiling".
MEMORY_CEILING = 77
COLD_S = 1800
WARM_S = 600
HELD_AFTER_S = 1.0
INTERVAL_S = 0.01
MIB = 1024

# The fixtures' programs are smoke.rs's HELLO and SIX, read from it so the two never differ.
SMOKE = "scripts/src/bin/smoke.rs"


def programs(root: Path) -> dict[str, str]:
    """smoke.rs's HELLO and SIX, with the body's number at @BODY@."""
    text = (root / SMOKE).read_text()
    found = {
        name.lower(): body
        for name, body in re.findall(r'const (HELLO|SIX): &str = r#"(.*?)"#;', text, re.DOTALL)
    }
    if set(found) != {"hello", "six"}:
        raise SystemExit(f"{SCRIPT}: {SMOKE} has no HELLO and SIX programs")
    return found


@dataclass(frozen=True)
class Edit:
    file: str
    find: str
    replace: str


@dataclass(frozen=True)
class Extra:
    name: str
    dir: str
    edit: Edit
    needs_gb: int = 32


@dataclass(frozen=True)
class Input:
    blaze: str
    work: str = ""
    out: str = ""
    cargo: tuple[str, ...] = ("cargo",)
    projects: tuple[Literal["hello", "six"], ...] = ("hello", "six")
    extra: tuple[Extra, ...] = ()
    runs: int = 3
    cap_gb: int = 12
    root: str = "."


@dataclass
class Ran:
    """One command: how it ended, the most its processes held at once, and why it was stopped."""

    status: int
    peak_mb: int
    stopped_at_mb: int | None = None
    tail: str = ""


@dataclass
class Sample:
    cargo_mb: int
    cold_mb: int
    edit_mb: int
    held_mb: int
    stopped: list[str] = field(default_factory=lambda: list[str]())


@dataclass(frozen=True)
class Row:
    project: str
    what: str
    blaze_mb: int | None
    cargo_mb: int
    met: bool
    note: str = ""


class Failed(Exception):
    """A run that ended for a reason other than memory: the check cannot judge the project."""


class Watched:
    """memory.Sampler with a stop: once the processes it watches hold more than `limit_mb` at
    once, every one of them is killed, and the most they held then is kept."""

    def __init__(self, limit_mb: int, places: tuple[Path, ...] = ()):
        self.limit_kb = limit_mb * 1024
        self.sampler = MEMORY.Sampler(INTERVAL_S)
        self.stopped_kb: int | None = None
        # The run's BLAZE_HOME and project: a process with a file open there is the run's, even
        # one that left the tree before a sample saw it (a daemon that detached at once).
        self.places = places
        self._done = threading.Event()
        self._thread: threading.Thread | None = None

    def add(self, pid: int) -> None:
        self.sampler.add(pid)

    def adopt(self) -> None:
        """Watches every process with a file open (its working directory included) under `places`."""
        if not self.places:
            return
        try:
            found = subprocess.run(
                ["lsof", "-t", *(arg for p in self.places for arg in ("+D", str(p)))],
                check=False,
                capture_output=True,
                text=True,
                timeout=60,
            ).stdout
        except (OSError, subprocess.TimeoutExpired) as e:
            # A detached helper (journal-helper, the store's GC) would go uncounted, and the
            # run could pass on an undercount: the run cannot be judged.
            raise Failed(f"could not look for the run's detached processes with lsof: {e}") from e
        for pid in {int(word) for word in found.split() if word.isdigit()} - {os.getpid()}:
            self.add(pid)

    def held_kb(self) -> int:
        return int(self.sampler.sample())

    def peak_mb(self) -> int:
        return round(int(self.sampler.peak_kb) / 1024)

    def reset_peak(self) -> None:
        self.sampler.peak_kb = 0

    def pids(self) -> list[int]:
        tree = cast(Callable[[int], list[int]], MEMORY.tree)
        return sorted({pid for root in list(self.sampler.roots) for pid in tree(root)})

    def kill(self) -> None:
        for pid in self.pids():
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass

    def _loop(self) -> None:
        while not self._done.is_set():
            held = self.held_kb()
            if held > self.limit_kb and self.stopped_kb is None:
                self.stopped_kb = held
                self.kill()
            self._done.wait(INTERVAL_S)

    def __enter__(self) -> Self:
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()
        return self

    def __exit__(self, *_: object) -> None:
        self._done.set()
        if self._thread:
            self._thread.join()
        try:
            self.adopt()
        finally:
            self.kill()


def source(root: Path, project: str, body: int) -> str:
    return programs(root)[project].replace("@BODY@", str(body))


def environment(home: str | None) -> dict[str, str]:
    """A command's whole environment. blaze's store follows HOME (blaze-core home.rs:
    `~/.blaze/v2/store`), so a blaze run gets `home` as HOME too: a cold run is cold."""
    env = {"HOME": home or os.environ["HOME"], "PATH": os.environ["PATH"], "TERM": "dumb", "LANG": "C.UTF-8"}
    return env | ({"BLAZE_HOME": str(Path(home) / ".blaze")} if home else {})


def fresh_home(home: Path) -> None:
    """An empty home for one blaze run with only the toolchain shelf the install cut
    (`~/.blaze/v2/toolchain`, which no build writes) linked in from the real one."""
    shelf = Path(os.environ["HOME"]) / ".blaze" / "v2" / "toolchain"
    if not shelf.is_dir():
        raise SystemExit(
            f"{SCRIPT}: {shelf} has no toolchain: cut one with scripts/src/bin/toolchain_cut.rs first"
        )
    (home / ".blaze" / "v2").mkdir(parents=True)
    (home / ".blaze" / "v2" / "toolchain").symlink_to(shelf)


def machine_gb() -> float:
    return os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES") / 2**30


def run(argv: list[str], cwd: Path, env: dict[str, str], watch: Watched, deadline_s: int) -> Ran:
    """`argv` under `watch`, to its end or `deadline_s`; its peak is the most the watched
    processes held at once from its start (a daemon from an earlier step included)."""
    watch.reset_peak()
    with open(cwd / ".tightmemory.log", "w+") as out:
        child = subprocess.Popen(
            argv,
            cwd=cwd,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=out,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        watch.add(child.pid)
        late = threading.Event()
        timer = threading.Timer(deadline_s, lambda: (late.set(), watch.kill()))
        timer.start()
        try:
            # wait4 also gives the kernel's exact peak of the largest process waited for, which a
            # sample can miss when the peak is shorter than the interval (memory.run does the same).
            largest_mb = int(MEMORY.waited(child))
        finally:
            timer.cancel()
        if late.is_set():
            raise Failed(f"`{' '.join(argv[1:])}` passed its {deadline_s} s deadline")
        out.seek(0)
        tail = "\n".join(out.read().splitlines()[-20:])
    stopped = None if watch.stopped_kb is None else round(watch.stopped_kb / 1024)
    return Ran(int(child.returncode), max(watch.peak_mb(), largest_mb), stopped, tail)


def ended(ran: Ran, what: str) -> None:
    """Raises Failed for a run that did not succeed for a reason other than memory."""
    if ran.stopped_at_mb is None and ran.status not in (0, MEMORY_CEILING):
        raise Failed(f"{what} exited {ran.status}:\n{ran.tail}")


def copy_project(it: Input, project: str, extra: Extra | None, dest: Path) -> None:
    if extra is not None:
        shutil.copytree(extra.dir, dest, ignore=shutil.ignore_patterns("target", ".git"))
        return
    fixture = Path(it.root) / FIXTURES / project
    (dest / "src").mkdir(parents=True)
    for name in FIXTURE_FILES:
        if (fixture / name).is_file():
            shutil.copy(fixture / name, dest / name)
    (dest / "src" / "main.rs").write_text(source(Path(it.root), project, 1))


def body_edit(it: Input, project: str, extra: Extra | None, dest: Path) -> None:
    if extra is None:
        (dest / "src" / "main.rs").write_text(source(Path(it.root), project, 2))
        return
    path = dest / extra.edit.file
    text = path.read_text()
    if extra.edit.find not in text:
        raise Failed(f"{extra.edit.file} has no `{extra.edit.find}` to edit")
    path.write_text(text.replace(extra.edit.find, extra.edit.replace, 1))


def cargo_sample(it: Input, project: str, extra: Extra | None, dest: Path) -> int:
    copy_project(it, project, extra, dest)
    env = environment(None) | {"CARGO_TARGET_DIR": str(dest / "target")}
    with Watched(10**9, (dest,)) as watch:
        cold = run([*it.cargo, "build"], dest, env, watch, COLD_S)
        ended(cold, "cargo build")
        body_edit(it, project, extra, dest)
        edit = run([*it.cargo, "build"], dest, env, watch, WARM_S)
        ended(edit, "cargo build after the edit")
    return max(cold.peak_mb, edit.peak_mb)


def blaze_sample(
    it: Input, project: str, extra: Extra | None, dest: Path, home: Path
) -> tuple[int, int, int, list[str]]:
    copy_project(it, project, extra, dest)
    fresh_home(home)
    env = environment(str(home))
    stopped: list[str] = []
    with Watched(it.cap_gb * 1024, (home, dest)) as watch:
        cold = run([it.blaze, "run"], dest, env, watch, COLD_S)
        ended(cold, "blaze run")
        if cold.stopped_at_mb is not None:
            stopped.append(f"cold stopped at {cold.stopped_at_mb} MB (cap {it.cap_gb} GB)")
            return max(cold.peak_mb, cold.stopped_at_mb), 0, 0, stopped
        body_edit(it, project, extra, dest)
        edit = run([it.blaze, "run"], dest, env, watch, WARM_S)
        ended(edit, "blaze run after the edit")
        if edit.stopped_at_mb is not None:
            stopped.append(f"edit stopped at {edit.stopped_at_mb} MB (cap {it.cap_gb} GB)")
        time.sleep(HELD_AFTER_S)
        watch.adopt()
        held = round(watch.held_kb() / 1024)
    return cold.peak_mb, max(edit.peak_mb, edit.stopped_at_mb or 0), held, stopped


def ceiling_gb(cargo_mb: int) -> int:
    """cargo's peak as BLAZE_DAEMON_MEMORY_GB takes it: whole GiB, rounded up, at least 1."""
    return max(1, -(-cargo_mb // 1024))


def ceiling_run(
    it: Input, project: str, extra: Extra | None, dest: Path, home: Path, gb: int
) -> tuple[bool, str]:
    copy_project(it, project, extra, dest)
    fresh_home(home)
    env = environment(str(home)) | {"BLAZE_DAEMON_MEMORY_GB": str(gb)}
    with Watched(gb * 1024, (home, dest)) as watch:
        ran = run([it.blaze, "run"], dest, env, watch, COLD_S)
        ended(ran, "blaze run under the ceiling")
    if ran.stopped_at_mb is not None:
        return False, f"stopped by this script at {ran.stopped_at_mb} MB, past the {gb} GiB ceiling"
    if ran.status == MEMORY_CEILING:
        return False, f"blaze ended itself at its {gb} GiB ceiling (exit {MEMORY_CEILING})"
    return True, f"built under a {gb} GiB ceiling"


def judge(project: str, samples: list[Sample], ceiling: tuple[bool, str] | None) -> list[Row]:
    """The rows of one project from its samples: each blaze median against cargo's peak's."""
    cargo = round(statistics.median(s.cargo_mb for s in samples))
    rows: list[Row] = []
    for what in ("cold", "edit", "held"):
        blaze = round(statistics.median(getattr(s, f"{what}_mb") for s in samples))
        stops = [line for s in samples for line in s.stopped if line.startswith(what)]
        rows.append(Row(project, what, blaze, cargo, blaze <= cargo and not stops, "; ".join(stops)))
    if ceiling is not None:
        rows.append(Row(project, "ceiling", None, cargo, ceiling[0], ceiling[1]))
    return rows


def table(rows: list[Row], skipped: list[str]) -> str:
    lines = [f"{'project':<10} {'row':<8} {'blaze MB':>9} {'cargo MB':>9}  verdict"]
    for r in rows:
        blaze = "-" if r.blaze_mb is None else str(r.blaze_mb)
        verdict = "met" if r.met else "LOST"
        lines.append(
            f"{r.project:<10} {r.what:<8} {blaze:>9} {r.cargo_mb:>9}  {verdict}"
            + (f"  ({r.note})" if r.note else "")
        )
    lines += [f"{name:<10} skipped   {why}" for name, why in (s.split(": ", 1) for s in skipped)]
    return "\n".join(lines)


def outcomes(rows: list[Row], skipped: list[str], failed: list[str]) -> list[dict[str, str]]:
    """One outcome a row, skipped project and failed run, in report.py's shape (the ledger's `said`)."""
    found = [
        {
            "name": f"{r.project} {r.what}",
            "verdict": "pass" if r.met else "fail",
            "detail": "; ".join(
                x
                for x in (
                    f"blaze {r.blaze_mb} MB" if r.blaze_mb is not None else "",
                    f"cargo {r.cargo_mb} MB",
                    r.note,
                )
                if x
            ),
        }
        for r in rows
    ]
    found += [
        {"name": s.split(": ", 1)[0], "verdict": "skip", "detail": s.split(": ", 1)[1]} for s in skipped
    ]
    return found + [
        {"name": f.split(": ", 1)[0], "verdict": "error", "detail": f.split(": ", 1)[1][:500]} for f in failed
    ]


def verdict(rows: list[Row], skipped: list[str], failed: list[str], large: bool = True) -> int:
    """2 a run failed, 1 a row lost, 3 nothing lost but not decided (a project skipped, or no
    `extra` checkout measured: the fixtures alone are too small to say T9 holds), 0 met."""
    if failed:
        return 2
    if any(not r.met for r in rows):
        return 1
    return 3 if skipped or not large else 0


def version(blaze: str) -> str:
    try:
        return subprocess.run(
            [blaze, "--version"], check=False, capture_output=True, text=True, timeout=30
        ).stdout.strip()
    except (OSError, subprocess.TimeoutExpired) as e:
        return f"unknown ({e})"


def main(it: Input) -> None:
    work = Path(it.work).resolve() if it.work else Path(tempfile.mkdtemp(prefix="tightmemory-"))
    if work.exists() and any(work.iterdir()):
        sys.exit(f"{SCRIPT}: {work} is not empty")
    work.mkdir(parents=True, exist_ok=True)
    gb = machine_gb()
    todo: list[tuple[str, Extra | None]] = [
        *((p, None) for p in it.projects),
        *((e.name, e) for e in it.extra),
    ]
    rows: list[Row] = []
    skipped: list[str] = []
    failed: list[str] = []
    record: dict[str, Any] = {
        "script": SCRIPT,
        "blaze": version(it.blaze),
        "date": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "machine": {"system": platform.platform(), "memory_gb": round(gb, 1)},
        "runs": it.runs,
        "cap_gb": it.cap_gb,
        "projects": {},
    }
    for name, extra in todo:
        if extra is not None and gb < extra.needs_gb:
            skipped.append(f"{name}: this machine has {gb:.0f} GB, it needs {extra.needs_gb}")
            continue
        samples: list[Sample] = []
        try:
            for i in range(it.runs):
                cargo_mb = cargo_sample(it, name, extra, work / name / f"cargo-{i}")
                cold, edit, held, stopped = blaze_sample(
                    it, name, extra, work / name / f"blaze-{i}", work / name / f"home-{i}"
                )
                samples.append(Sample(cargo_mb, cold, edit, held, stopped))
                print(
                    f"{SCRIPT}: {name} run {i + 1}: cargo {cargo_mb} MB; blaze cold {cold}, edit {edit}, held {held} MB"
                    + (f" ({'; '.join(stopped)})" if stopped else ""),
                    flush=True,
                )
            cargo = round(statistics.median(s.cargo_mb for s in samples))
            ceiling = ceiling_run(
                it, name, extra, work / name / "ceiling", work / name / "home-ceiling", ceiling_gb(cargo)
            )
        except Failed as e:
            failed.append(f"{name}: {e}")
            print(f"{SCRIPT}: {name}: {e}", flush=True)
            continue
        rows += judge(name, samples, ceiling)
        record["projects"][name] = {
            "samples": [s.__dict__ for s in samples],
            "ceiling": {"met": ceiling[0], "note": ceiling[1]},
        }
    record |= {
        "rows": [r.__dict__ for r in rows],
        "skipped": skipped,
        "failed": failed,
        "outcomes": outcomes(rows, skipped, failed),
    }
    out = Path(it.out) if it.out else work / "tightmemory.json"
    out.write_text(json.dumps(record, indent=1) + "\n")
    print(f"{SCRIPT}: the result is {out}")
    print(table(rows, skipped))
    for line in failed:
        print(f"{SCRIPT}: failed: {line}")
    large = any(e.name in record["projects"] for e in it.extra)
    if not large and not any(not r.met for r in rows):
        print(f"{SCRIPT}: no `extra` checkout was measured; hello and six alone do not decide T9")
    sys.exit(verdict(rows, skipped, failed, large))


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def selftest() -> None:
    root = HERE.parent.parent
    expect(
        "+ 7" in source(root, "hello", 7) and "hello: {}" in source(root, "hello", 7),
        "hello's body number, from smoke.rs",
    )
    expect(
        "regex::Regex" in source(root, "six", 1) and "@BODY@" not in source(root, "six", 1),
        "six's program, from smoke.rs",
    )
    expect(
        [ceiling_gb(mb) for mb in (1, 400, 1024, 1025, 3000)] == [1, 1, 1, 2, 3],
        "the ceiling in whole GiB, rounded up",
    )
    under = [Sample(500, 400, 300, 0), Sample(520, 450, 310, 0), Sample(480, 420, 290, 0)]
    rows = judge("hello", under, (True, "built under a 1 GiB ceiling"))
    expect(
        [(r.what, r.blaze_mb, r.cargo_mb, r.met) for r in rows]
        == [
            ("cold", 420, 500, True),
            ("edit", 300, 500, True),
            ("held", 0, 500, True),
            ("ceiling", None, 500, True),
        ],
        f"medians against cargo's peak: {rows}",
    )
    expect(verdict(rows, [], []) == 0, "every row met exits 0")
    equal = judge("hello", [Sample(500, 500, 500, 500)], None)
    expect(all(r.met for r in equal), "at cargo's peak is met: no slack either way")
    over = judge(
        "six",
        [Sample(600, 5800, 700, 5200), Sample(610, 5700, 650, 5100), Sample(590, 5900, 640, 5300)],
        (False, "blaze ended itself at its 1 GiB ceiling (exit 77)"),
    )
    expect(
        [r.met for r in over] == [False, False, False, False],
        f"a daemon holding 5 GB loses every row: {over}",
    )
    expect(verdict(over, [], []) == 1, "a lost row exits 1")
    stopped = judge("six", [Sample(600, 12300, 0, 0, ["cold stopped at 12300 MB (cap 12 GB)"])], None)
    expect(
        not stopped[0].met and "12300" in stopped[0].note,
        f"a run stopped at the cap loses its row, saying so: {stopped}",
    )
    expect(
        verdict(rows, ["ripgrep: this machine has 24 GB, it needs 32"], []) == 3, "met but skipped exits 3"
    )
    expect(verdict(over, [], ["hello: blaze run exited 125"]) == 2, "a failed run exits 2 before any verdict")
    expect(verdict(rows, [], [], large=False) == 3, "met on the fixtures alone is not decided: exits 3")
    expect(verdict(over, [], [], large=False) == 1, "a lost fixture row is lost whatever else ran")
    said = outcomes(over[:1] + rows[:1], ["ripgrep: needs 32"], ["six: blaze run exited 125"])
    expect(
        said
        == [
            {"name": "six cold", "verdict": "fail", "detail": "blaze 5800 MB; cargo 600 MB"},
            {"name": "hello cold", "verdict": "pass", "detail": "blaze 420 MB; cargo 500 MB"},
            {"name": "ripgrep", "verdict": "skip", "detail": "needs 32"},
            {"name": "six", "verdict": "error", "detail": "blaze run exited 125"},
        ],
        f"outcomes for the ledger: {said}",
    )
    text = table(over + rows, ["ripgrep: this machine has 24 GB, it needs 32"])
    expect(
        "LOST" in text and "met" in text and "ripgrep    skipped   this machine has 24 GB" in text,
        f"the table:\n{text}",
    )
    try:
        ended(Ran(125, 3, None, "error: the pinned toolchain"), "blaze run")
        expect(False, "a non-memory exit is a failure")
    except Failed as e:
        expect(
            "exited 125" in str(e) and "pinned toolchain" in str(e), f"a failure names its exit and tail: {e}"
        )
    ended(Ran(MEMORY_CEILING, 900), "blaze run")
    ended(Ran(-9, 12300, 12300), "blaze run")
    it = parse(
        Input,
        '{"blaze": "b", "work": "w", "extra": [{"name": "ripgrep", "dir": "/rg", '
        '"edit": {"file": "x.rs", "find": "a", "replace": "b"}}]}',
        SCRIPT,
    )
    expect(
        it.projects == ("hello", "six") and it.runs == 3 and it.cap_gb == 12 and it.extra[0].needs_gb == 32,
        f"the defaults: {it}",
    )
    # The watch stops a process tree past its limit and keeps what it held then.
    with Watched(1) as watch:
        child = subprocess.Popen(
            [sys.executable, "-c", "b = bytearray(64 << 20); import time; time.sleep(30)"],
            start_new_session=True,
        )
        watch.add(child.pid)
        status = child.wait(timeout=20)
    expect(
        status != 0 and watch.stopped_kb is not None and watch.stopped_kb > 1024,
        f"a 64 MB process under a 1 MB limit is stopped: status {status}, held {watch.stopped_kb}",
    )
    gone = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"], start_new_session=True)
    path = os.environ["PATH"]
    os.environ["PATH"] = "/nonexistent"
    try:
        with Watched(10**6, (Path("/tmp"),)) as watch:
            watch.add(gone.pid)
        expect(False, "a watch whose lsof cannot run must fail the run")
    except Failed as e:
        expect("lsof" in str(e), f"the failure names lsof: {e}")
    finally:
        os.environ["PATH"] = path
    expect(gone.wait(timeout=10) != 0, "the watched process is stopped even when discovery failed")
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
