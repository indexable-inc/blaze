#!/usr/bin/env python3
"""versus.py: blaze against makepad's Relax, head to head on one machine (decision D21).

The bar of D21 is Relax measured on the same machine in the same hour and kept in the gate; this
is that measurement. Each row is measured for both tools on their own copy of the same project
with their own fresh state, every run's exit status and printed line checked, and a row is won
only when blaze beats Relax by the stated factor (harness/v2/versus/README.md says how each row
is judged). Run with `python3 -I`:

    python3 -I harness/v2/versus.py '{"blaze": "/path/to/inst/blaze", "relax": "/tmp/relax-src/relax/target/release/relax", "root": "/private/tmp/versus"}'

The input is one JSON value (inline as the one argument, a path to a file holding it, or `-` for
stdin) parsed into `Input`, unknown fields refused:

    blaze     the blaze binary (an installed one: the thin `blaze` beside its `blaze-engine`)
    relax     the relax binary
    root      a directory this script owns: both tools' copies and states are made under it, and
              `versus.json` is written into it. Refused when it is under ~/.cache/blaze or ~/.blaze,
              or holds anything and `reuse` is not true.
    projects  which fixtures, of "hello", "six" and "bstd-hello" (all by default; harness/v2/versus/).
              `bstd-hello` is hello on bstd, which only blaze builds: its rows are judged against
              Relax's `hello` rows, so `hello` comes before it
    n         samples a row (20); `cold` takes 3 whatever n is
    factor    how many times faster blaze must be on a timed row to win it (3.0)
    extra     real checkouts: [{"name", "dir", "package", "edit": {"file", "find", "replace"},
              "expect": a line the program prints, or null to check the exit status alone}]
    reuse     run in a root that is not empty (what this script made there is replaced)
    date      the date written into versus.json ("" reads today's date once)
    tree      the blaze checkout whose crates/blaze-std `bstd-hello` depends on (needed with it)

Rows, per project and tool, each after two warm-ups of the same step: cold (state deleted before
each of 3 runs); no-op run; comment edit then run; body edit then run; restart then no-op run
(blaze: `blaze stop`, the daemon seen gone, then the run that starts it again; Relax keeps no
process, so its row is its no-op row); held between runs (blaze: the daemon's resident and
footprint bytes a second after each body-edit run, and beside them blaze's own peak of one no-op
run under /usr/bin/time with no daemon before it; Relax holds nothing, and beside its 0 stands
the peak of one no-op run under /usr/bin/time); idle 45 s (blaze's daemon 45 s after the fifth of
five body edits, judged as held); body edit after 60 s idle (judged as body edit); these two
are 3 samples each, not n; on disk (blocks under the tool's state, the
growth of its copy and, for blaze, the growth of the shared ~/.blaze). blaze's no-op and restart
rows, and its one-run peak, say whether a daemon held its state after the runs (`daemon_after`:
`serve.lock` in `<BLAZE_HOME>/v2` has a holder, a shared try-lock fails), and after how many.

A failed run of a fixture stops the script with exit 2, naming the tool, the row and what the run
printed (rule no-fallback). A failed run of an `extra` checkout is recorded as that tool's fact
for the project (its exit status and its last 20 lines) and the other tool's numbers stand.
Exit 0 when blaze wins every row, 1 when it loses any, 2 for a failed fixture run. No retries,
no outlier dropped: the table shows each row's range, versus.json every sample.
"""

from __future__ import annotations

import dataclasses
import datetime
import fcntl
import json
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import time
import types
import typing
from dataclasses import dataclass
from pathlib import Path

HERE = Path(os.path.dirname(os.path.abspath(__file__)))
FIXTURES = HERE / "versus"
FIXTURE_NAMES = ("hello", "six", "bstd-hello")
# Only blaze builds bstd: its fixture's rows are judged against Relax's rows of this one, the
# same program on std.
ONLY_BLAZE = {"bstd-hello": "hello"}
# How the bstd fixture names crates/blaze-std (as harness/v2/smoke/edition does); the copy
# names `<tree>/crates/blaze-std` instead.
BSTD = "../../../../crates/blaze-std"
TOOLS = ("relax", "blaze")
ROWS = ("cold", "noop", "comment", "body", "restart", "held", "idle", "body_idle", "disk")
MEMORY_ROWS = ("held", "idle")
ROW_TITLES = {
    "cold": "cold",
    "noop": "no-op run",
    "comment": "comment edit + run",
    "body": "body edit + run",
    "restart": "restart + no-op run",
    "held": "held between runs",
    "idle": "idle 45 s",
    "body_idle": "body edit after 60 s idle",
    "disk": "on disk",
}
COLD_RUNS = 3
WARM = 2
# The memory plan's two slow rows, 3 samples each whatever n is: the daemon's memory IDLE_S
# after the IDLE_EDITS-th body edit, and a body edit then run BODY_IDLE_S after the one before.
SLOW_RUNS = 3
IDLE_EDITS = 5
IDLE_S = 45
BODY_IDLE_S = 60
# Between two edit samples, untimed and the same for both tools (the earlier one-off,
# /tmp/launch3-relax-measure.py, waited 1 s): what a tool does after it answered is not the
# next sample's, and blaze's daemon is read at the end of this pause.
SETTLE_S = 1.0
RUN_TIMEOUT_S = 3600
STOP_TIMEOUT_S = 120
# blaze serve's line when it listens (crates/blaze-cargo/src/serve.rs, LISTENING).
LISTENING = re.compile(r"^blaze serve: listening on .* \(pid (\d+)\)\s*$", re.MULTILINE)
NAME = re.compile(r"^[a-z0-9][a-z0-9_-]{0,39}$")
# A unix socket path is at most 104 bytes on macOS; blaze's is <BLAZE_HOME>/v2.sock.
SOCKET_MAX = 100


# --- input --------------------------------------------------------------------------------


@dataclass(frozen=True)
class Edit:
    file: str
    find: str
    replace: str


@dataclass(frozen=True)
class Extra:
    name: str
    dir: str
    package: str
    edit: Edit
    expect: str | None = None


@dataclass(frozen=True)
class Input:
    blaze: str
    relax: str
    root: str
    projects: tuple[str, ...] = FIXTURE_NAMES
    n: int = 20
    factor: float = 3.0
    extra: tuple[Extra, ...] = ()
    reuse: bool = False
    date: str = ""
    tree: str = ""  # the blaze checkout whose crates/blaze-std `bstd-hello` builds on


def parsed[T](cls: type[T], value: object, where: str) -> T:
    """`value` (decoded JSON) as the frozen dataclass `cls`; an unknown or missing field or a
    value of another type is refused, naming the field (rule scripts-take-typed-input)."""
    if not isinstance(value, dict):
        raise ValueError(f"{where} is a JSON object for {cls.__name__}, not {json.dumps(value)[:80]}")
    given = typing.cast(dict[str, object], value)
    hints = typing.get_type_hints(cls)
    known = {f.name: f for f in dataclasses.fields(typing.cast(typing.Any, cls))}
    for key in given:
        if key not in known:
            raise ValueError(f"{where}: `{key}` is not a field of {cls.__name__} ({', '.join(known)})")
    made: dict[str, object] = {}
    for name, f in known.items():
        if name not in given:
            if f.default is dataclasses.MISSING and f.default_factory is dataclasses.MISSING:
                raise ValueError(f"{where}: {cls.__name__} needs `{name}`")
            continue
        made[name] = fitted(hints[name], given[name], f"{where}.{name}")
    return cls(**made)


def fitted(hint: object, value: object, where: str) -> object:
    shown = json.dumps(value)[:80]
    origin = typing.get_origin(hint)
    if origin in (typing.Union, types.UnionType):
        options = typing.get_args(hint)
        if value is None and type(None) in options:
            return None
        inner = [o for o in options if o is not type(None)]
        if len(inner) != 1:
            raise ValueError(f"{where}: versus.py reads no field of type {hint}")
        return fitted(inner[0], value, where)
    if isinstance(hint, type) and dataclasses.is_dataclass(hint):
        return parsed(hint, value, where)
    if origin is tuple:
        if not isinstance(value, list):
            raise ValueError(f"{where} is a list, not {shown}")
        of = typing.get_args(hint)[0]
        return tuple(fitted(of, it, f"{where}[{at}]") for at, it in enumerate(typing.cast(list[object], value)))
    if hint is float:
        if type(value) not in (int, float):
            raise ValueError(f"{where} is a number, not {shown}")
        return float(typing.cast(float, value))
    if hint in (int, str, bool):
        if type(value) is not hint:
            raise ValueError(f"{where} is of type {typing.cast(type, hint).__name__}, not {shown}")
        return value
    raise ValueError(f"{where}: versus.py reads no field of type {hint}")


def read_input(argv: list[str]) -> Input:
    if len(argv) != 2:
        sys.exit("versus.py takes one JSON value: inline, a path to a file holding it, or `-` (see its module docs)")
    arg = argv[1]
    if arg == "-":
        text = sys.stdin.read()
    elif not arg.lstrip().startswith("{") and Path(arg).is_file():
        text = Path(arg).read_text()
    else:
        text = arg
    try:
        asked = parsed(Input, json.loads(text), "the input")
    except (ValueError, json.JSONDecodeError) as error:
        sys.exit(f"versus.py: {error}")
    return asked


def refuse(why: str) -> typing.NoReturn:
    sys.exit(f"versus.py: {why}")


def under(path: Path, top: Path) -> bool:
    p, t = os.path.realpath(path), os.path.realpath(top)
    return p == t or p.startswith(t.rstrip("/") + "/")


def checked(asked: Input) -> None:
    for tool, path in (("blaze", asked.blaze), ("relax", asked.relax)):
        if not os.path.isfile(path) or not os.access(path, os.X_OK):
            refuse(f"`{tool}` {path} is not an executable file")
    engine = Path(asked.blaze).parent / "blaze-engine"
    if not engine.is_file():
        refuse(f"no blaze-engine beside {asked.blaze}: give an installed blaze (the thin client and its engine)")
    if asked.n < 1:
        refuse(f"n is at least 1, not {asked.n}")
    if not asked.factor > 0:
        refuse(f"factor is above 0, not {asked.factor}")
    if len(set(asked.projects)) != len(asked.projects) or any(p not in FIXTURE_NAMES for p in asked.projects):
        refuse(f"projects is some of {FIXTURE_NAMES}, each once, not {list(asked.projects)}")
    for only, against in ONLY_BLAZE.items():
        if only not in asked.projects:
            continue
        if against not in asked.projects or asked.projects.index(against) > asked.projects.index(only):
            refuse(f"`{only}` is judged against Relax's `{against}` rows: list `{against}` before it in projects")
        if not asked.tree or not (Path(asked.tree) / "crates/blaze-std/Cargo.toml").is_file():
            refuse(f"`{only}` builds on bstd: give \"tree\", a blaze checkout holding crates/blaze-std "
                   f"(not {asked.tree!r})")
    names = list(asked.projects)
    for extra in asked.extra:
        if not NAME.match(extra.name) or extra.name in names or extra.name in FIXTURE_NAMES:
            refuse(f"extra `{extra.name}`: a name is lowercase letters, digits, - and _, unique and not a fixture's")
        names.append(extra.name)
        top = Path(extra.dir)
        if not (top / "Cargo.toml").is_file():
            refuse(f"extra `{extra.name}`: {top} holds no Cargo.toml")
        source = top / extra.edit.file
        if not source.is_file() or not under(source, top):
            refuse(f"extra `{extra.name}`: edit.file {extra.edit.file} is not a file inside {top}")
        text = source.read_text()
        if text.count(extra.edit.find) != 1:
            refuse(f"extra `{extra.name}`: edit.find occurs {text.count(extra.edit.find)} times in {source}, not once")
        if extra.edit.find == extra.edit.replace:
            refuse(f"extra `{extra.name}`: edit.replace is edit.find: the body edit would change nothing")
    root = Path(asked.root)
    if not root.is_absolute():
        refuse(f"root {asked.root} is not an absolute path")
    for mine in (Path.home() / ".cache/blaze", Path.home() / ".blaze"):
        if under(root, mine):
            refuse(f"root {root} is under {mine}: the user's own blaze state is never measured or wiped")
    if under(HERE, root):
        refuse(f"root {root} holds this script's own tree")
    if root.exists() and any(root.iterdir()) and not asked.reuse:
        refuse(f"root {root} is not empty: give an empty or new directory, or \"reuse\": true")
    socket = root / "blaze-home" / "v2.sock"
    if len(str(socket).encode()) > SOCKET_MAX:
        refuse(f"root {root} is too long: blaze's socket {socket} would pass {SOCKET_MAX} bytes")


# --- runs ---------------------------------------------------------------------------------


class RunFailed(Exception):
    def __init__(self, tool: str, project: str, row: str, exit: int | None, why: str, tail: list[str]) -> None:
        super().__init__(f"{tool} failed on {project}, row {row}: {why}")
        self.tool, self.project, self.row, self.exit, self.why, self.tail = tool, project, row, exit, why, tail

    def record(self) -> dict[str, object]:
        return {"tool": self.tool, "project": self.project, "row": self.row, "exit": self.exit,
                "why": self.why, "last_lines": self.tail}


def last_lines(stdout: str, stderr: str, most: int = 20) -> list[str]:
    return (stdout.rstrip("\n").splitlines() + stderr.rstrip("\n").splitlines())[-most:]


@dataclass
class Ran:
    ms: float
    stdout: str
    stderr: str


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


@dataclass
class Tool:
    """One tool's side of one project: its binary, its copy, its state and its environment."""

    name: str
    binary: str
    project: str
    copy: Path
    package: str
    workspace: bool  # an extra checkout: blaze runs `-p <package>`
    env: dict[str, str]
    state: tuple[Path, ...]  # deleted for a cold run, counted for the disk row

    def command(self, timed: bool = False) -> list[str]:
        if self.name == "relax":
            args = [self.binary, "run", "--quiet", self.package]
        elif self.workspace:
            args = [self.binary, "run", "-q", "-p", self.package]
        else:
            args = [self.binary, "run", "-q"]
        if timed:
            return ["/usr/bin/time", "-l" if sys.platform == "darwin" else "-v", *args]
        return args

    def run(self, row: str, want: str | None, timed: bool = False) -> Ran:
        began = time.perf_counter()
        try:
            done = subprocess.run(self.command(timed), cwd=self.copy, env=self.env, capture_output=True,
                                  text=True, timeout=RUN_TIMEOUT_S, check=False)
        except subprocess.TimeoutExpired as out:
            stdout = out.stdout.decode(errors="replace") if isinstance(out.stdout, bytes) else (out.stdout or "")
            stderr = out.stderr.decode(errors="replace") if isinstance(out.stderr, bytes) else (out.stderr or "")
            raise RunFailed(self.name, self.project, row, None, f"no exit in {RUN_TIMEOUT_S} s",
                            last_lines(stdout, stderr)) from None
        ms = (time.perf_counter() - began) * 1000
        if done.returncode != 0:
            raise RunFailed(self.name, self.project, row, done.returncode, f"exit {done.returncode}",
                            last_lines(done.stdout, done.stderr))
        if want is not None and want not in [line.rstrip("\r") for line in done.stdout.splitlines()]:
            raise RunFailed(self.name, self.project, row, 0, f"exit 0 but no line `{want}` on stdout",
                            last_lines(done.stdout, done.stderr))
        return Ran(ms, done.stdout, done.stderr)

    # blaze's daemon --------------------------------------------------------------------

    def daemon(self) -> int | None:
        """The live daemon on this tool's state: the pid of the last `listening` line of its
        log, while that process lives."""
        if self.name != "blaze":
            return None
        log = Path(self.env["BLAZE_HOME"]) / "v2" / "serve.log"
        if not log.is_file():
            return None
        found = LISTENING.findall(log.read_text(errors="replace"))
        if not found:
            return None
        pid = int(found[-1])
        return pid if alive(pid) else None

    def daemon_after(self, row: str) -> bool:
        """Whether a daemon holds blaze's state now: `serve.lock` in its state directory
        (`<BLAZE_HOME>/v2`) taken shared without waiting fails (the daemon holds it exclusive,
        crates/blaze-cargo/src/serve.rs; the thin `blaze` asks it so, blaze-thin solo.rs). No
        file: no daemon ever ran on this state."""
        if self.name != "blaze":
            return False
        lock = Path(self.env["BLAZE_HOME"]) / "v2" / "serve.lock"
        try:
            fd = os.open(lock, os.O_RDONLY)
        except FileNotFoundError:
            return False
        except OSError as error:
            raise RunFailed(self.name, self.project, row, None, f"opening {lock}: {error}", []) from None
        try:
            fcntl.flock(fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        except OSError as error:
            raise RunFailed(self.name, self.project, row, None, f"asking who holds {lock}: {error}", []) from None
        finally:
            os.close(fd)  # lets go of a lock taken, with the descriptor
        return False

    def running_daemon(self, row: str) -> int:
        pid = self.daemon()
        if pid is None:
            raise RunFailed(self.name, self.project, row, None,
                            f"no live daemon named by {self.env['BLAZE_HOME']}/v2/serve.log after a run", [])
        return pid

    def stop(self, row: str) -> None:
        """Stop blaze's daemon by `blaze stop` and see it gone (bounded)."""
        pid = self.daemon()
        if pid is None:
            return
        done = subprocess.run([self.binary, "stop"], cwd=self.copy, env=self.env, capture_output=True, text=True,
                              timeout=STOP_TIMEOUT_S, check=False)
        if done.returncode != 0:
            raise RunFailed(self.name, self.project, row, done.returncode,
                            f"`blaze stop` of daemon {pid} exited {done.returncode}", last_lines(done.stdout, done.stderr))
        began = time.monotonic()
        while alive(pid):
            if time.monotonic() - began > STOP_TIMEOUT_S:
                raise RunFailed(self.name, self.project, row, 0,
                                f"daemon {pid} outlived `blaze stop` by {STOP_TIMEOUT_S} s", [])
            time.sleep(0.05)

    def wipe(self, row: str) -> None:
        self.stop(row)
        for path in self.state:
            if path.exists():
                shutil.rmtree(path)
            path.mkdir(parents=True)
        target = self.copy / "target"
        if target.exists():
            shutil.rmtree(target)
        tmp = Path(self.env["TMPDIR"])
        if tmp.exists():
            shutil.rmtree(tmp)
        tmp.mkdir(parents=True)


# --- sources: what each step writes ---------------------------------------------------------

HELLO = """// versus: comment {comment}
fn value() -> u32 {{
    (1..=10).sum::<u32>() + {body}
}}

fn main() {{
    println!("hello: {{}}", value());
}}
"""

SIX = """// versus: comment {comment}
bitflags::bitflags! {{
    struct Flags: u8 {{
        const A = 1;
        const B = 2;
    }}
}}

fn value() -> u32 {{
    (1..=10).sum::<u32>() + {body}
}}

// One call into each dependency, so each is linked: 1 + 1 + 2 + 1 + 3.
fn others() -> usize {{
    let digits = regex::Regex::new(r"^[0-9]+$").unwrap();
    let mut map: hashbrown::HashMap<u32, u32> = hashbrown::HashMap::new();
    map.insert(1, 2);
    let mut buffer = itoa::Buffer::new();
    let text = buffer.format(42u32);
    // No `smallvec!`: Relax substitutes its own smallvec, which has no such macro.
    let mut small: smallvec::SmallVec<[u8; 4]> = smallvec::SmallVec::new();
    small.push(1);
    small.push(2);
    let at = memchr::memchr(b'2', text.as_bytes()).unwrap_or(9);
    usize::from(digits.is_match(text)) + map.len() + small.len() + at + usize::from((Flags::A | Flags::B).bits())
}}

fn main() {{
    println!("six: {{}} {{}}", value(), others());
}}
"""
SIX_OTHERS = 8

# hello on bstd, shaped as harness/v2/smoke/edition/src/main.rs: `main` takes the task context
# and the Io, and prints through the Io's output.
BSTD_HELLO = """// versus: comment {comment}
fn value() -> u32 {{
    (1..=10).sum::<u32>() + {body}
}}

fn main(cx: &Cx, io: Io) -> Result<(), Error> {{
    io.need_out()?.line(cx, format_args!("hello: {{}}", value()))?;
    Ok(())
}}
"""
BASE = 100


@dataclass
class Source:
    """The one file a project's edits change, and what its program prints for each state."""

    path: Path
    text: typing.Callable[[int, int], str]
    expect: typing.Callable[[int, int], str | None]

    def write(self, body: int, comment: int) -> None:
        # In place, as an editor that truncates and writes saves.
        self.path.write_text(self.text(body, comment))


def fixture_source(project: str, copy: Path) -> Source:
    template = {"hello": HELLO, "six": SIX, "bstd-hello": BSTD_HELLO}[project]
    if project in ("hello", "bstd-hello"):
        expect: typing.Callable[[int, int], str | None] = lambda body, _c: f"hello: {55 + body}"
    else:
        expect = lambda body, _c: f"six: {55 + body} {SIX_OTHERS}"
    return Source(copy / "src" / "main.rs", lambda body, comment: template.format(body=body, comment=comment), expect)


def extra_source(extra: Extra, copy: Path) -> Source:
    path = copy / extra.edit.file
    original = path.read_text()
    if not original.endswith("\n"):
        original += "\n"
    edited = original.replace(extra.edit.find, extra.edit.replace, 1)

    def text(body: int, comment: int) -> str:
        # The body edit toggles find and replace; the comment edit is the last line's text.
        return (edited if (body - BASE) % 2 else original) + (f"// versus: comment {comment}\n" if comment else "")

    return Source(path, text, lambda _b, _c: extra.expect)


# --- measurements -------------------------------------------------------------------------


def summary(samples: list[float]) -> dict[str, object]:
    ordered = sorted(samples)
    return {"samples": [round(s, 3) for s in samples], "median": round(statistics.median(ordered), 3),
            "min": round(ordered[0], 3), "max": round(ordered[-1], 3), "n": len(samples)}


def blocks(path: Path, seen: set[tuple[int, int]] | None = None) -> int:
    """Bytes of the blocks under `path` (st_blocks, each inode once), not their lengths."""
    seen = set() if seen is None else seen
    if not os.path.lexists(path):
        return 0
    total = 0
    stack = [str(path)]
    while stack:
        at = stack.pop()
        st = os.lstat(at)
        key = (st.st_dev, st.st_ino)
        if key not in seen:
            seen.add(key)
            total += st.st_blocks * 512
        if os.path.isdir(at) and not os.path.islink(at):
            with os.scandir(at) as entries:
                stack.extend(e.path for e in entries)
    return total


def unit_bytes(number: str, unit: str) -> int:
    scale = {"B": 1, "KB": 1 << 10, "MB": 1 << 20, "GB": 1 << 30, "K": 1 << 10, "M": 1 << 20, "G": 1 << 30}
    return int(float(number) * scale[unit])


def held(tool: Tool, row: str) -> dict[str, int]:
    """blaze's daemon's resident and footprint bytes now."""
    pid = tool.running_daemon(row)
    if sys.platform == "linux":
        status = dict(line.split(":", 1) for line in Path(f"/proc/{pid}/status").read_text().splitlines() if ":" in line)
        rollup = dict(line.split(":", 1) for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines()[1:]
                      if ":" in line)
        kb = lambda table, key: int(table[key].split()[0]) * 1024
        # Footprint as macOS counts it: pages that are dirty (private or shared) plus swapped.
        return {"resident": kb(status, "VmRSS"),
                "footprint": kb(rollup, "Private_Dirty") + kb(rollup, "Shared_Dirty") + kb(rollup, "Swap")}
    rss = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True, check=False)
    foot = subprocess.run(["footprint", "-p", str(pid)], capture_output=True, text=True, check=False)
    found = re.search(r"Footprint:\s+([0-9.]+)\s*(B|KB|MB|GB)\b", foot.stdout)
    if rss.returncode != 0 or not rss.stdout.strip().isdigit() or foot.returncode != 0 or not found:
        raise RunFailed(tool.name, tool.project, row, None,
                        f"daemon {pid}'s memory unread: ps exit {rss.returncode}, footprint exit {foot.returncode}",
                        last_lines(rss.stdout + foot.stdout, rss.stderr + foot.stderr))
    return {"resident": int(rss.stdout.strip()) * 1024, "footprint": unit_bytes(found.group(1), found.group(2))}


def peak(tool: Tool, row: str, ran: Ran) -> dict[str, int]:
    """One run's peak, from /usr/bin/time's report at the end of its stderr."""
    text = ran.stderr
    if sys.platform == "darwin":
        resident = re.search(r"^\s*(\d+)\s+maximum resident set size\s*$", text, re.MULTILINE)
        footprint = re.search(r"^\s*(\d+)\s+peak memory footprint\s*$", text, re.MULTILINE)
        if not resident or not footprint:
            raise RunFailed(tool.name, tool.project, row, 0, "/usr/bin/time -l printed no peak", last_lines("", text))
        return {"resident": int(resident.group(1)), "footprint": int(footprint.group(1))}
    resident = re.search(r"Maximum resident set size \(kbytes\):\s*(\d+)", text)
    if not resident:
        raise RunFailed(tool.name, tool.project, row, 0, "/usr/bin/time -v printed no peak", last_lines("", text))
    return {"resident": int(resident.group(1)) * 1024}


def with_daemon_after(row: dict[str, object], after: list[bool]) -> dict[str, object]:
    """`row` with whether a daemon held blaze's state after its runs: `daemon_after` true when
    one did after any sample, and after how many of them."""
    return dict(row, daemon_after=any(after), daemon_after_n=sum(after))


def measure(tool: Tool, source: Source, n: int, shared_home: Path) -> dict[str, object]:
    """Every row for one tool on one project, in the order of ROWS."""
    rows: dict[str, object] = {}
    body, comment = BASE, 0
    copy_before = blocks(tool.copy)
    shared_before = blocks(shared_home) if tool.name == "blaze" else 0
    source.write(body, comment)

    cold = []
    for _ in range(COLD_RUNS):
        tool.wipe("cold")
        cold.append(tool.run("cold", source.expect(body, comment)).ms)
    rows["cold"] = summary(cold)

    for _ in range(WARM):
        tool.run("noop", source.expect(body, comment))
    samples, after = [], []
    for _ in range(n):
        samples.append(tool.run("noop", source.expect(body, comment)).ms)
        after.append(tool.daemon_after("noop"))
    rows["noop"] = summary(samples)
    if tool.name == "blaze":
        rows["noop"] = with_daemon_after(typing.cast(dict[str, object], rows["noop"]), after)

    samples = []
    for i in range(WARM + n):
        comment += 1
        began = time.perf_counter()
        source.write(body, comment)
        tool.run("comment", source.expect(body, comment))
        if i >= WARM:
            samples.append((time.perf_counter() - began) * 1000)
        time.sleep(SETTLE_S)
    rows["comment"] = summary(samples)

    samples, resident, footprint = [], [], []
    for i in range(WARM + n):
        body += 1
        began = time.perf_counter()
        source.write(body, comment)
        tool.run("body", source.expect(body, comment))
        if i >= WARM:
            samples.append((time.perf_counter() - began) * 1000)
        time.sleep(SETTLE_S)
        if i >= WARM and tool.name == "blaze":
            now = held(tool, "held")
            resident.append(float(now["resident"]))
            footprint.append(float(now["footprint"]))
    rows["body"] = summary(samples)

    if tool.name == "blaze":
        samples, after = [], []
        for i in range(WARM + n):
            tool.stop("restart")
            ran = tool.run("restart", source.expect(body, comment))
            if i >= WARM:
                samples.append(ran.ms)
                after.append(tool.daemon_after("restart"))
        rows["restart"] = with_daemon_after(summary(samples), after)
        # One run's peak, as Relax's is read: each run under /usr/bin/time with no daemon on
        # blaze's state before it (stopped, and its lock seen free), so the peak is a run that
        # waits for no daemon; whether the run left one is said beside it, as that daemon's
        # memory is not in this process's peak.
        peaks, after = [], []
        for i in range(WARM + n):
            tool.stop("held")
            if tool.daemon_after("held"):
                raise RunFailed(tool.name, tool.project, "held", None,
                                "serve.lock still has a holder after `blaze stop`: no run with no daemon to measure", [])
            ran = tool.run("held", source.expect(body, comment), timed=True)
            if i >= WARM:
                peaks.append(peak(tool, "held", ran))
                after.append(tool.daemon_after("held"))
        rows["held"] = {"resident": summary(resident), "footprint": summary(footprint),
                        "run_peak_resident": summary([float(p["resident"]) for p in peaks]),
                        "run_peak_daemon_after": any(after), "run_peak_daemon_after_n": sum(after)}
        if sys.platform == "darwin":
            typing.cast(dict[str, object], rows["held"])["run_peak_footprint"] = summary(
                [float(p["footprint"]) for p in peaks])
    else:
        rows["restart"] = dict(typing.cast(dict[str, object], rows["noop"]), same_as="noop: Relax keeps no process")
        peaks: list[dict[str, int]] = []
        for i in range(WARM + n):
            ran = tool.run("held", source.expect(body, comment), timed=True)
            if i >= WARM:
                peaks.append(peak(tool, "held", ran))
        rows["held"] = {"resident": summary([0.0] * n), "footprint": summary([0.0] * n),
                        "run_peak_resident": summary([float(p["resident"]) for p in peaks])}
        if sys.platform == "darwin":
            typing.cast(dict[str, object], rows["held"])["run_peak_footprint"] = summary(
                [float(p["footprint"]) for p in peaks])

    samples, resident, footprint = [], [], []
    for _ in range(SLOW_RUNS):
        if tool.name != "blaze":
            break
        for _ in range(IDLE_EDITS):
            body += 1
            source.write(body, comment)
            tool.run("idle", source.expect(body, comment))
            time.sleep(SETTLE_S)
        time.sleep(IDLE_S - SETTLE_S)
        now = held(tool, "idle")
        resident.append(float(now["resident"]))
        footprint.append(float(now["footprint"]))
    if tool.name == "blaze":
        rows["idle"] = {"resident": summary(resident), "footprint": summary(footprint)}
    else:
        rows["idle"] = {"resident": summary([0.0] * SLOW_RUNS), "footprint": summary([0.0] * SLOW_RUNS),
                        "same_as": "held: Relax keeps no process"}

    samples = []
    for _ in range(SLOW_RUNS):
        time.sleep(BODY_IDLE_S)
        body += 1
        began = time.perf_counter()
        source.write(body, comment)
        tool.run("body_idle", source.expect(body, comment))
        samples.append((time.perf_counter() - began) * 1000)
    rows["body_idle"] = summary(samples)

    tool.stop("disk")
    state = sum(blocks(path) for path in tool.state)
    grown = blocks(tool.copy) - copy_before
    shared = blocks(shared_home) - shared_before if tool.name == "blaze" else 0
    rows["disk"] = {"bytes": state + max(grown, 0) + max(shared, 0), "state": state, "copy_grew": grown,
                    "shared_home_grew": shared}
    return rows


# --- one project, both tools ----------------------------------------------------------------


def base_env(tmp: Path) -> dict[str, str]:
    path = "/usr/bin:/bin:/usr/sbin:/sbin"
    if os.path.isdir("/run/current-system/sw/bin"):
        path = "/run/current-system/sw/bin:" + path
    return {"HOME": str(Path.home()), "PATH": path, "TMPDIR": str(tmp)}


def tools_for(asked: Input, root: Path, project: str, extra: Extra | None) -> list[tuple[Tool, Source]]:
    made = []
    for name in TOOLS:
        if name != "blaze" and project in ONLY_BLAZE:
            continue
        copy = root / name / project
        if copy.exists():
            shutil.rmtree(copy)
        if extra is None:
            shutil.copytree(FIXTURES / project, copy)
            (copy / "src").mkdir()
            if project in ONLY_BLAZE:
                manifest = copy / "Cargo.toml"
                text = manifest.read_text()
                if text.count(BSTD) != 1:
                    refuse(f"{FIXTURES / project}/Cargo.toml does not name bstd as {BSTD} once: versus.py rewrites it")
                manifest.write_text(text.replace(BSTD, str(Path(asked.tree).resolve() / "crates/blaze-std")))
        else:
            shutil.copytree(extra.dir, copy, symlinks=True,
                            ignore=lambda at, names: ["target"] if os.path.samefile(at, extra.dir) else [])
        tmp = root / f"{name}-tmp"
        env = base_env(tmp)
        if name == "blaze":
            home = root / "blaze-home"
            env["BLAZE_HOME"] = str(home)
            binary = asked.blaze
        else:
            # Relax's run home (gitstore::relax_home); its target directory is the copy's `target`.
            home = root / "relax-home"
            env["MAKEPAD_RUN_HOME"] = str(home)
            binary = asked.relax
        source = fixture_source(project, copy) if extra is None else extra_source(extra, copy)
        package = project if extra is None else extra.package
        made.append((Tool(name, binary, project, copy, package, extra is not None, env, (home,)), source))
    return made


def one_project(asked: Input, root: Path, project: str, extra: Extra | None) -> dict[str, object]:
    print(f"== {project}", flush=True)
    found: dict[str, object] = {}
    for tool, source in tools_for(asked, root, project, extra):
        print(f"   {tool.name}: measuring", flush=True)
        try:
            found[tool.name] = measure(tool, source, asked.n, Path.home() / ".blaze")
        except RunFailed as failed:
            if extra is None:
                raise
            found[tool.name] = {"failed": failed.record()}
            print(f"   {tool.name}: cannot build {project}: {failed.why}", flush=True)
        finally:
            # No daemon outlives its project, whatever ended the measurement.
            stop_quietly(tool)
    return found


def stop_quietly(tool: Tool) -> None:
    """After a failure, leave no daemon behind; a stop that itself fails is printed, and the
    failure already being reported stands."""
    try:
        tool.stop("cleanup")
    except RunFailed as failed:
        print(f"   {tool.name}: {failed}", file=sys.stderr, flush=True)


# --- judging --------------------------------------------------------------------------------


def judged_memory(rows: dict[str, object]) -> tuple[str, float, dict[str, object]]:
    held_rows = typing.cast(dict[str, dict[str, object]], rows["held"])
    if "run_peak_footprint" in held_rows:
        return "footprint", typing.cast(float, held_rows["run_peak_footprint"]["median"]), held_rows["run_peak_footprint"]
    return "resident", typing.cast(float, held_rows["run_peak_resident"]["median"]), held_rows["run_peak_resident"]


def cell(row: str, value: dict[str, object]) -> str:
    if row in ("held", "disk"):
        mb = lambda b: f"{typing.cast(float, b) / (1 << 20):.1f}"
        if row == "disk":
            return f"{mb(value['bytes'])} MB"
        return f"{mb(value['median'])} MB ({mb(value['min'])}-{mb(value['max'])}, n {value['n']})"
    return f"{value['median']:.1f} ms ({value['min']:.1f}-{value['max']:.1f}, n {value['n']})"


def yes(value: object) -> str:
    return "yes" if value else "no"


def blaze_notes(row: str, rows: dict[str, object], kind: str) -> str:
    """What blaze's cell says beside its number: on the no-op and restart rows whether a daemon
    held its state after the runs, and on held its own one-run peak of a run with no daemon
    before it."""
    value = typing.cast(dict[str, object], rows[row])
    if "daemon_after" in value:
        return f"; daemon after: {yes(value['daemon_after'])} ({value['daemon_after_n']} of {value['n']})"
    peak_key = f"run_peak_{kind}"
    if row == "held" and peak_key in value:
        return (f"; one run's peak {kind}, no daemon before it, "
                f"{cell('held', typing.cast(dict[str, object], value[peak_key]))}, "
                f"daemon after: {yes(value['run_peak_daemon_after'])} "
                f"({value['run_peak_daemon_after_n']} of {typing.cast(dict[str, object], value[peak_key])['n']})")
    return ""


def plain(tool: str, row: str, rows: dict[str, object]) -> str:
    """One tool's cell for a row, alone (the other tool could not build the project)."""
    if row in MEMORY_ROWS:
        if tool == "blaze":
            memory = typing.cast(dict[str, dict[str, object]], rows[row])
            kind = "footprint" if "run_peak_footprint" in memory else "resident"
            return f"footprint {cell('held', memory['footprint'])}{blaze_notes(row, rows, kind)}"
        kind, _, summary_of = judged_memory(rows)
        return f"0 held; one run's peak {kind} {cell('held', summary_of)}"
    notes = blaze_notes(row, rows, "") if tool == "blaze" else ""
    return cell(row, typing.cast(dict[str, object], rows[row])) + notes


def verdicts(project: str, found: dict[str, object], factor: float) -> list[dict[str, object]]:
    """Each row of `project` judged; `found` holds both tools' rows (for a blaze-only fixture,
    Relax's are those of the project it is judged against, and `project` says so)."""
    out = []
    relax, blaze = found["relax"], found["blaze"]
    relax_failed = isinstance(relax, dict) and "failed" in relax
    blaze_failed = isinstance(blaze, dict) and "failed" in blaze
    for row in ROWS:
        line: dict[str, object] = {"project": project, "row": row}
        if blaze_failed or relax_failed:
            for name, failed in (("relax", relax_failed), ("blaze", blaze_failed)):
                rows = typing.cast(dict[str, object], found[name])
                line[name] = f"failed (exit {typing.cast(dict[str, object], rows['failed'])['exit']})" if failed \
                    else plain(name, row, rows)
            line["ratio"] = None
            line["judged"] = "a tool that cannot build the project loses each of its rows"
            line["won"] = not blaze_failed
            out.append(line)
            continue
        r_rows, b_rows = typing.cast(dict[str, object], relax), typing.cast(dict[str, object], blaze)
        if row in MEMORY_ROWS:
            kind, relax_value, relax_summary = judged_memory(r_rows)
            blaze_summary = typing.cast(dict[str, dict[str, object]], b_rows[row])[kind]
            blaze_value = typing.cast(float, blaze_summary["median"])
            line["relax"] = f"0 held; one run's peak {kind} {cell('held', relax_summary)}"
            line["blaze"] = f"{kind} {cell('held', blaze_summary)}{blaze_notes(row, b_rows, kind)}"
            line["judged"] = f"blaze's idle {kind} under Relax's peak {kind} of one no-op run"
            line["won"] = blaze_value < relax_value
        elif row == "disk":
            relax_value = float(typing.cast(dict[str, int], r_rows["disk"])["bytes"])
            blaze_value = float(typing.cast(dict[str, int], b_rows["disk"])["bytes"])
            line["relax"] = cell("disk", typing.cast(dict[str, object], r_rows["disk"]))
            line["blaze"] = cell("disk", typing.cast(dict[str, object], b_rows["disk"]))
            line["judged"] = "blaze's bytes under Relax's"
            line["won"] = blaze_value < relax_value
        else:
            r, b = typing.cast(dict[str, object], r_rows[row]), typing.cast(dict[str, object], b_rows[row])
            relax_value, blaze_value = typing.cast(float, r["median"]), typing.cast(float, b["median"])
            line["relax"], line["blaze"] = cell(row, r), cell(row, b) + blaze_notes(row, b_rows, "")
            line["judged"] = f"Relax's median at least {factor} times blaze's"
            line["won"] = blaze_value > 0 and relax_value / blaze_value >= factor
        line["ratio"] = round(relax_value / blaze_value, 3) if blaze_value > 0 else None
        out.append(line)
    return out


def table(lines: list[dict[str, object]]) -> str:
    head = ("row", "Relax", "blaze", "Relax/blaze", "")
    body = [(f"{l['project']} {ROW_TITLES[typing.cast(str, l['row'])]}", str(l["relax"]), str(l["blaze"]),
             "-" if l["ratio"] is None else f"{l['ratio']:.2f}x", "won" if l["won"] else "lost") for l in lines]
    widths = [max(len(r[i]) for r in [head, *body]) for i in range(len(head))]
    fmt = lambda r: "  ".join(c.ljust(widths[i]) for i, c in enumerate(r)).rstrip()
    return "\n".join([fmt(head), fmt(tuple("-" * w for w in widths)), *(fmt(r) for r in body)])


# --- the machine ----------------------------------------------------------------------------


def version(tool: str, binary: str, env: dict[str, str], cwd: Path) -> str:
    done = subprocess.run([binary, "--version"], cwd=cwd, env=env, capture_output=True, text=True, timeout=120, check=False)
    printed = done.stdout.strip()
    if done.returncode != 0 or not printed:
        raise RunFailed(tool, "-", "version", done.returncode, f"`{binary} --version` exited {done.returncode}",
                        last_lines(done.stdout, done.stderr))
    return printed


def machine() -> dict[str, object]:
    if sys.platform == "darwin":
        done = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True, check=False)
        cpu = done.stdout.strip() if done.returncode == 0 else f"unread: sysctl exited {done.returncode}"
    else:
        names = re.findall(r"^model name\s*:\s*(.+)$", Path("/proc/cpuinfo").read_text(), re.MULTILINE)
        cpu = names[0].strip() if names else f"{platform.machine()}: /proc/cpuinfo names no model"
    return {"os": platform.platform(), "cpu": cpu, "cpus": os.cpu_count()}


# --- main -----------------------------------------------------------------------------------


def write_result(root: Path, result: dict[str, object]) -> None:
    (root / "versus.json").write_text(json.dumps(result, indent=1) + "\n")


def main(argv: list[str]) -> None:
    asked = read_input(argv)
    checked(asked)
    if sys.platform != "darwin" and not os.access("/usr/bin/time", os.X_OK):
        refuse("no /usr/bin/time: Relax's one-run peak is read from `/usr/bin/time -v`")
    root = Path(asked.root)
    root.mkdir(parents=True, exist_ok=True)
    for made in ("blaze-home", "relax-home", "blaze-tmp", "relax-tmp"):
        (root / made).mkdir(exist_ok=True)
    result: dict[str, object] = {
        "date": asked.date or datetime.date.today().isoformat(),
        "machine": machine(),
        "input": dataclasses.asdict(asked),
        "factor": asked.factor,
        "projects": [],
        "verdicts": [],
        "failed": None,
    }
    blaze_env = dict(base_env(root / "blaze-tmp"), BLAZE_HOME=str(root / "blaze-home"))
    relax_env = dict(base_env(root / "relax-tmp"), MAKEPAD_RUN_HOME=str(root / "relax-home"))
    try:
        result["versions"] = {"relax": version("relax", asked.relax, relax_env, root),
                              "blaze": version("blaze", asked.blaze, blaze_env, root)}
        result["install"] = {"relax_binary_bytes": blocks(Path(asked.relax)),
                             "blaze_shared_home_bytes": blocks(Path.home() / ".blaze")}
        lines: list[dict[str, object]] = []
        measured: dict[str, dict[str, object]] = {}
        work: list[tuple[str, Extra | None]] = [(p, None) for p in asked.projects]
        work += [(e.name, e) for e in asked.extra]
        for project, extra in work:
            found = one_project(asked, root, project, extra)
            typing.cast(list[object], result["projects"]).append({"name": project, "tools": found})
            measured[project] = found
            if project in ONLY_BLAZE:
                against = ONLY_BLAZE[project]
                found = {"relax": measured[against]["relax"], "blaze": found["blaze"]}
                project = f"{project} (vs Relax {against})"
            lines += verdicts(project, found, asked.factor)
            result["verdicts"] = lines
            write_result(root, result)
    except RunFailed as failed:
        result["failed"] = failed.record()
        write_result(root, result)
        print(f"versus.py: {failed}", file=sys.stderr)
        for line in failed.tail:
            print(f"  | {line}", file=sys.stderr)
        sys.exit(2)
    print(table(lines))
    won = all(typing.cast(bool, l["won"]) for l in lines)
    result["won"] = won
    write_result(root, result)
    print(f"\n{sum(1 for l in lines if l['won'])} of {len(lines)} rows won at factor {asked.factor}; {root / 'versus.json'}")
    sys.exit(0 if won else 1)


if __name__ == "__main__":
    main(sys.argv)
