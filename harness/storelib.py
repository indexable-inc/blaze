"""storelib.py: v2's daemon (`blaze serve`) in a home of its own, the stress workspace it builds,
and the files of its store as blaze-cas lays them out.

What storekills.py, storecorrupts.py, storefull.py, storeguards.py, storetracked.py and
fuzz/store.py share. A daemon keeps what outlives it in `<home>/v2` and its store in
`<home>/v2/store` (crates/blaze-cargo/src/serve.rs, `state`; the daemon's `Store::open` in
serve/daemon.rs), so the same store is the same home and a build from nothing is a build in an
empty one. The store's files (crates/blaze-cas/src/disk.rs, docs/v2-store.md "On disk"):
`head`, `index-0` and `index-1`, `pack-NNNNNN`, `file-<content>`, and while a whole file is
written, its temporary `.<name>.<pid>-<n>.tmp` beside it (crates/blaze-core/src/fs.rs).

The daemon is started here, not by the first `blaze build` (crates/blaze-cargo/src/cli/daemon.rs,
`connect`), so its process can be killed and what it prints is in a log of the harness's. A
`blaze build` that finds no daemon listening starts one of its own, logging to
`<home>/v2/serve.log`: `Daemon.stray` tells of that and stops it.

`scratch` and `run` are what other harnesses take from here too, and the readers of what blaze
says for machines, which every harness reads facts from rather than the text blaze prints for
people: `messages` (a build's `--message-format=json` lines), `worked` and `links` (its
`blaze-work` line: what the daemon did and how it linked each image), `daemon_pid` (the
daemon's lock), and `Daemon.command` with `test_results`: any blaze command asked of the daemon,
and the `blaze-test-result` lines of a `blaze test --reached --message-format=json`
(crates/blaze-cargo/src/cli/json.rs, `test_result`) read into `TestLine`s.
"""
from __future__ import annotations

import concurrent.futures
import hashlib
import json
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from collections.abc import Iterable, Mapping
from concurrent.futures import Future
from dataclasses import dataclass
from typing import Literal, TypeVar, cast

HERE = os.path.dirname(os.path.abspath(__file__))
STRESS = os.path.join(HERE, "stress")
PROGRAM = "sapp"
MID = "mid/src/lib.rs"
AREA = "sbase::square!(self.side)"
START_SECS, BUILD_SECS, RUN_SECS, STOP_SECS = 300, 1800, 60, 300
# What `blaze serve` prints once it listens (crates/blaze-cargo/src/serve.rs, `run`), and how a
# store that cannot open or go on is said (blaze_cas::Broken's Display).
LISTENING = "blaze serve: listening on "
BROKEN = "the store cannot go on: "
# Read by blaze or cargo from the caller's environment; a harness sets what it means to.
SCRUBBED = ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTDOCFLAGS", "RUSTC", "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET", "CARGO_INCREMENTAL",
            "BLAZE_HOME", "XDG_RUNTIME_DIR")
# For `run`, as its callers outside the store harnesses have it: what a program is run under.
RUNNER = shlex.split(os.environ.get("BLAZE_RUNNER", ""))


class Failed(Exception):
    """A step the harness could not do, said whole: an error of the run, not a finding."""


def scratch(prefix: str) -> str:
    # cargo names a checkout by its real path, and the temporary directory of macOS is a link.
    # That directory is also too long for the path of a daemon's socket in it (104 bytes).
    return os.path.realpath(tempfile.mkdtemp(prefix=prefix, dir="/tmp" if sys.platform == "darwin" else None))


def run(path: str) -> str:
    try:
        ran = subprocess.run([*RUNNER, path], capture_output=True, text=True, timeout=60)
    except Exception as error:
        return f"did not run: {error}"
    return ran.stdout + (f"[exit {ran.returncode}]" if ran.returncode else "")


def printed(executable: str) -> str:
    """What a program prints, with its exit status when that is not 0."""
    try:
        ran = subprocess.run([executable], capture_output=True, text=True, timeout=RUN_SECS)
    except (OSError, subprocess.TimeoutExpired) as error:
        return f"did not run: {error}"
    return ran.stdout + (f"[exit {ran.returncode}]" if ran.returncode else "")


def state_of(home: str) -> str:
    return os.path.join(home, "v2")


def store_of(home: str) -> str:
    return os.path.join(home, "v2", "store")


def kind(name: str) -> str:
    """What a file of a store is: head, index, pack, file, temporary, or other."""
    if name == "head":
        return "head"
    if re.fullmatch(r"index-[01]", name):
        return "index"
    if re.fullmatch(r"pack-\d{6}", name):
        return "pack"
    if re.fullmatch(r"file-[0-9a-f]+", name):
        return "file"
    if re.fullmatch(r"\..+\.\d+-\d+\.tmp", name):
        return "temporary"
    return "other"


def files(store: str, *kinds: str) -> list[str]:
    """The paths of the store's files of those kinds, by name; a store not there has none."""
    if not os.path.isdir(store):
        return []
    return sorted(os.path.join(store, name) for name in os.listdir(store) if kind(name) in kinds)


def listing(store: str) -> dict[str, str]:
    """Each file of the store with a hash of its bytes: two equal listings are the same store."""
    if not os.path.isdir(store):
        return {}
    out: dict[str, str] = {}
    for name in sorted(os.listdir(store)):
        path = os.path.join(store, name)
        if os.path.isfile(path):
            with open(path, "rb") as file:
                out[name] = hashlib.blake2b(file.read(), digest_size=16).hexdigest()
    return out


def stored_bytes(store: str) -> int:
    return sum(os.path.getsize(path) for path in files(store, "head", "index", "pack", "file"))


def forget(home: str) -> None:
    """Everything of the home's daemon but its store: what a daemon that starts on the same
    store has nothing else of (no VFS journal, no database image)."""
    state = state_of(home)
    if not os.path.isdir(state):
        return
    for name in os.listdir(state):
        path = os.path.join(state, name)
        if name == "store":
            continue
        if os.path.isdir(path) and not os.path.islink(path):
            shutil.rmtree(path)
        else:
            os.remove(path)


def named(text: str, path: str) -> bool:
    """Whether what blaze said names the file at `path`: a store's files are named by the
    driver as paths under its root `/` without the leading slash."""
    return path.lstrip("/") in text


def environment(home: str) -> dict[str, str]:
    env = {name: value for name, value in os.environ.items() if name not in SCRUBBED}
    env["BLAZE_HOME"] = home
    return env


def daemon_pid(home: str) -> int | None:
    """The pid of the daemon running on `home`'s state, or None when none is: what its lock
    `<home>/v2/serve.lock` says (crates/blaze-cargo/src/serve.rs, `hold` and `Before::of`), a
    daemon's pid as it starts and `<pid> stopped` once its stop wrote its image. A pid the lock
    names without `stopped` whose process is gone ended without its stop: None too.

    `blaze status` prints the same pid for people only (`pid: N`, `status_lines`) and has no
    machine format, so a harness reads the lock, not that text."""
    try:
        with open(os.path.join(state_of(home), "serve.lock")) as lock:
            words = lock.read().split()
    except FileNotFoundError:
        return None
    if len(words) != 1 or not words[0].isdigit():
        return None
    pid = int(words[0])
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return None
    except PermissionError:
        pass
    return pid


class Built:
    """One `blaze build`: its exit status, what it said, and the program it built (None when
    it built none)."""

    def __init__(self, status: int, said: str, executable: str | None):
        self.status, self.said, self.executable = status, said, executable

    def program(self) -> str | None:
        return printed(self.executable) if self.executable else None


Message = dict[str, object]


def messages(said: str, reason: str) -> list[Message]:
    """The `--message-format=json` lines of `reason` in what a build printed, in order. Only
    JSON objects are messages; the lines blaze prints for people (`link: ...`, `Compiling`,
    warnings) never begin with `{` and are skipped."""
    out: list[Message] = []
    for line in said.splitlines():
        if not line.startswith("{") or reason not in line:
            continue
        try:
            parsed: object = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(parsed, dict):
            message = cast(Message, parsed)
            if message.get("reason") == reason:
                out.append(message)
    return out


def text_of(message: Message, field: str) -> str | None:
    """The string field `field` of `message`, None when it has none or another type."""
    value = message.get(field)
    return value if isinstance(value, str) else None


T = TypeVar("T")


def field_of(message: Mapping[str, object], field: str, kind: type[T]) -> T:
    """The field `field` of `message`, of type `kind`; a message without one, or with one of
    another type, is refused, naming both."""
    value = message.get(field)
    # bool is an int to isinstance: a number field never takes one.
    if not isinstance(value, kind) or (kind is int and isinstance(value, bool)):
        reason = message.get("reason")
        what = f"the `{reason}` message" if isinstance(reason, str) else "a message's object"
        raise Failed(f"{what}'s `{field}` is {value!r}, not {kind.__name__}: {dict(message)}")
    return value


def objects_of(message: Message, field: str) -> list[Message]:
    """The JSON objects in the array field `field` of `message` (none when it has none)."""
    value = message.get(field)
    if not isinstance(value, list):
        return []
    return [cast(Message, it) for it in cast(list[object], value) if isinstance(it, dict)]


def strings_of(message: Message, field: str) -> tuple[str, ...]:
    """The strings in the array field `field` of `message` (none when it has none)."""
    value = message.get(field)
    if not isinstance(value, list):
        return ()
    return tuple(it for it in cast(list[object], value) if isinstance(it, str))


def artifact(stdout: str, program: str) -> str | None:
    """The executable of `program` in what `blaze build --message-format=json` printed."""
    found = None
    for message in messages(stdout, "compiler-artifact"):
        target = message.get("target")
        name = text_of(cast(Message, target), "name") if isinstance(target, dict) else None
        executable = text_of(message, "executable")
        if executable and name == program:
            found = executable
    return found


@dataclass(frozen=True)
class Link:
    """One image a build linked, as its `blaze-work` line says (blaze_api::Linked): `how` is
    `first`, `spliced`, `unchanged` or `full`, and `why` names what made a `full` one."""

    path: str
    how: str
    why: tuple[str, ...]

    def __str__(self) -> str:
        return f"{self.how}: {', '.join(self.why)}" if self.why else self.how


@dataclass(frozen=True)
class Work:
    """What the daemon did for one request, its `blaze-work` line (crates/blaze-ui/src/api.rs,
    `work_json`): units run again, kept (`green`), compiled and found, how the request's link
    went (`patched` or `placed`), and each image it linked."""

    reran: int
    green: int
    compiled: int
    found: int
    link: str
    links: tuple[Link, ...]


def worked(said: str) -> list[Work]:
    """The `blaze-work` lines of what `blaze build`, `test` or `run` with
    `--message-format=json` printed: one a request the daemon served."""
    out: list[Work] = []
    for message in messages(said, "blaze-work"):
        made = tuple(Link(text_of(it, "path") or "", text_of(it, "how") or "", strings_of(it, "why"))
                     for it in objects_of(message, "links"))
        out.append(Work(field_of(message, "reran", int), field_of(message, "green", int),
                        field_of(message, "compiled", int), field_of(message, "found", int),
                        text_of(message, "link") or "", made))
    return out


def links(said: str, program: str) -> list[Link]:
    """How each image of `program` the build linked was linked, in order, from its `blaze-work`
    lines. A link names the output before it is uplifted, cargo's `deps/<program>-<metadata>`,
    or the uplifted path itself."""
    own = re.compile(rf"{re.escape(program)}(?:-[0-9a-f]+)?(?:\.exe|\.dylib|\.so|\.dll)?")
    return [link for work in worked(said) for link in work.links
            if own.fullmatch(os.path.basename(link.path))]


class Daemon:
    """`blaze serve` with `home` as its home, its output appended to `<home>/<name>.log`; `env`
    is set for it and for every build asked of it, over what `environment` leaves, a value of
    None unsetting the variable."""

    def __init__(self, blaze: str, home: str, name: str = "serve", wait: bool = True,
                 env: Mapping[str, str | None] | None = None):
        self.blaze, self.home = blaze, home
        os.makedirs(home, exist_ok=True)
        given: dict[str, str | None] = {**environment(home), **(env or {})}
        self.env: dict[str, str] = {name: value for name, value in given.items() if value is not None}
        self.log_path = os.path.join(home, f"{name}.log")
        self.seen = os.path.getsize(self.log_path) if os.path.exists(self.log_path) else 0
        with open(self.log_path, "a") as out:
            self.process = subprocess.Popen([blaze, "serve"], stdin=subprocess.DEVNULL, stdout=out, stderr=out,
                                            env=self.env, start_new_session=True)
        self.listening = False
        if wait:
            self.wait()

    @property
    def store(self) -> str:
        return store_of(self.home)

    def log(self) -> str:
        """What this daemon printed, without what those before it in the same file did."""
        with open(self.log_path, errors="replace") as log:
            log.seek(self.seen)
            return log.read()

    def alive(self) -> bool:
        return self.process.poll() is None

    def listens(self) -> bool:
        """Whether it has said it listens, without waiting."""
        return LISTENING in self.log()

    def wait(self, timeout: float = START_SECS) -> bool:
        """True once it listens; False when it ended first (its log says why)."""
        deadline = time.monotonic() + timeout
        while LISTENING not in self.log():
            if not self.alive():
                return False
            if time.monotonic() > deadline:
                self.kill()
                raise Failed(f"`blaze serve` in {self.home} did not listen within {timeout} s: {self.log_path}")
            time.sleep(0.02)
        self.listening = True
        return True

    def argv(self, *args: str) -> list[str]:
        return [self.blaze, "build", "--message-format=json", *args]

    def asking(self, ws: str, *args: str) -> subprocess.Popen[str]:
        """A `blaze build` of `ws` started and not waited for."""
        if not self.listening or not self.alive():
            raise Failed(f"a build asked of a daemon that does not listen: {self.log_path}")
        return subprocess.Popen(self.argv(*args), cwd=ws, env=self.env, stdin=subprocess.DEVNULL,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def answer(self, client: subprocess.Popen[str], program: str = PROGRAM,
               timeout: float = BUILD_SECS) -> Built:
        try:
            out, err = client.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            client.kill()
            out, err = client.communicate()
            raise Failed(f"`blaze build` took more than {timeout} s:\n{(out + err)[-3000:]}\nlog: {self.log_path}")
        return Built(client.returncode, out + err, artifact(out, program) if client.returncode == 0 else None)

    def build(self, ws: str, *args: str, program: str = PROGRAM, timeout: float = BUILD_SECS) -> Built:
        return self.answer(self.asking(ws, *args), program, timeout)

    def command(self, ws: str, verb: str, *args: str, env: Mapping[str, str | None] | None = None,
                timeout: float = BUILD_SECS) -> Ran:
        """`blaze VERB ARGS` in `ws` asked of this daemon and waited for, with `env` over the
        daemon's (None unsets a variable): what a client's environment changes, the request
        carries. One that outlasts `timeout` is killed and fails the step, naming it."""
        if not self.listening or not self.alive():
            raise Failed(f"`blaze {verb}` asked of a daemon that does not listen: {self.log_path}")
        given = {name: value for name, value in {**self.env, **(env or {})}.items() if value is not None}
        argv = [self.blaze, verb, *args]
        try:
            ran = subprocess.run(argv, cwd=ws, env=given, stdin=subprocess.DEVNULL, capture_output=True,
                                 text=True, encoding="utf-8", errors="replace", timeout=timeout, check=False)
        except subprocess.TimeoutExpired as error:
            raise Failed(f"`{shlex.join(argv)}` in {ws} took more than {timeout} s\nlog: {self.log_path}") from error
        return Ran(argv, ran.returncode, ran.stdout, ran.stderr)

    def stop(self, timeout: float = STOP_SECS) -> None:
        """`blaze stop`, which writes the daemon's image and ends it, waited for."""
        if self.alive():
            subprocess.run([self.blaze, "stop"], env=self.env, capture_output=True, timeout=timeout)
            try:
                self.process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                self.kill()
                raise Failed(f"`blaze stop` did not end the daemon in {self.home} within {timeout} s: {self.log_path}")
        self.stray()

    def kill(self) -> None:
        if self.alive():
            os.kill(self.process.pid, signal.SIGKILL)
        self.process.wait()

    def stray(self) -> bool:
        """Whether a `blaze build` found no daemon and started one of its own; that one is
        stopped and its log moved aside, so no other daemon is on the store."""
        log = os.path.join(state_of(self.home), "serve.log")
        if not os.path.exists(log):
            return False
        subprocess.run([self.blaze, "stop"], env=self.env, capture_output=True, timeout=STOP_SECS)
        os.replace(log, f"{log}.{time.time_ns()}")
        return True


def checkout(at: str, value: int = 0) -> str:
    """A copy of the stress workspace at `at`, printing what it prints with `value` added to a
    tile's area (0 is the workspace as it came)."""
    shutil.copytree(STRESS, at, ignore=shutil.ignore_patterns("target*"))
    set_value(at, 0, value)
    return at


def set_value(ws: str, old: int, new: int) -> None:
    path = os.path.join(ws, MID)
    text = open(path).read()
    was = AREA + (f" + {old}" if old else "")
    if was not in text:
        raise Failed(f"{path} has no `{was}`")
    open(path, "w").write(text.replace(was, AREA + (f" + {new}" if new else ""), 1))


class Checkout:
    """A copy of the stress workspace that a test edits, at `<work>/<name>`."""

    def __init__(self, work: str, name: str = "checkout"):
        self.ws = checkout(os.path.join(work, name))
        self.value = 0

    def set(self, value: int) -> None:
        set_value(self.ws, self.value, value)
        self.value = value


def from_nothing(blaze: str, at: str, value: int, keep: bool) -> str | None:
    """What the stress program with `value` prints when blaze builds it in an empty home."""
    ws = checkout(os.path.join(at, "checkout"), value)
    home = os.path.join(at, "h")
    daemon = Daemon(blaze, home)
    if not daemon.listening:
        raise Failed(f"the daemon of the build from nothing of {value} did not start: {daemon.log_path}")
    try:
        built = daemon.build(ws)
    finally:
        daemon.stop()
    if built.status != 0 or not built.executable:
        raise Failed(f"the build from nothing of {value} failed ({built.status}):\n{built.said[-3000:]}\n"
                     f"log: {daemon.log_path}")
    out = built.program()
    if not keep:
        shutil.rmtree(at, ignore_errors=True)
    return out


def references(blaze: str, work: str, values: Iterable[int], jobs: int = 4) -> dict[int, str | None]:
    """{value: what the stress program prints when built from nothing}, built `jobs` at a time."""
    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as pool:
        made: dict[int, Future[str | None]] = {
            value: pool.submit(from_nothing, blaze, os.path.join(work, f"ref-{value}"), value, False)
            for value in sorted(set(values))}
        return {value: future.result() for value, future in made.items()}


def blaze_of(path: str) -> str:
    """The `blaze` command line at `path`, refused when it is not there."""
    path = os.path.abspath(path)
    if not os.path.isfile(path) or not os.access(path, os.X_OK):
        raise Failed(f"no blaze at {path}")
    return path


@dataclass(frozen=True)
class Ran:
    """A blaze command that ended: its command line, exit status and what it printed."""
    argv: list[str]
    status: int
    stdout: str
    stderr: str


Outcome = Literal["passed", "failed", "ignored", "unreached"]
OUTCOMES: tuple[Outcome, ...] = ("passed", "failed", "ignored", "unreached")


@dataclass(frozen=True)
class CachedRun:
    """The kept run that stands for a test not run (blaze_api::Cached): what it was kept by
    (`executed`: the functions it executed; `reached`: what it reaches, its record not whole,
    with why), how long it took and how it ended."""
    basis: Literal["executed", "reached"]
    why: str | None
    millis: int
    outcome: Outcome


@dataclass(frozen=True)
class TestLine:
    """One `blaze-test-result` line (blaze_api::TestResult): `cached` is None for a test that
    ran this time; `outside` says it ran outside the sandbox, so its pass is never kept."""
    binary: str
    executable: int
    name: str
    outcome: Outcome
    millis: int
    outside: bool
    cached: CachedRun | None
    output: str


def _outcome(line: Mapping[str, object]) -> Outcome:
    said = field_of(line, "outcome", str)
    for known in OUTCOMES:
        if said == known:
            return known
    raise Failed(f"a blaze-test-result line's outcome {said!r} is none of {OUTCOMES}: {dict(line)}")


def _cached(line: Mapping[str, object]) -> CachedRun | None:
    value = line.get("cached")
    if value is None:
        return None
    if not isinstance(value, dict):
        raise Failed(f"a blaze-test-result line's `cached` is {value!r}, not an object or null: {dict(line)}")
    cached = cast(dict[str, object], value)
    basis = field_of(cached, "basis", str)
    if basis != "executed" and basis != "reached":
        raise Failed(f"a kept run's basis {basis!r} is neither `executed` nor `reached`: {dict(line)}")
    why = cached.get("why")
    if why is not None and not isinstance(why, str):
        raise Failed(f"a kept run's `why` is {why!r}, not text or null: {dict(line)}")
    return CachedRun(basis, why, field_of(cached, "millis", int), _outcome(cached))


def test_results(stdout: str) -> list[TestLine]:
    """The `blaze-test-result` lines of what `blaze test --reached --message-format=json`
    printed, in order; a line of that reason missing a field, or with one of another type,
    fails naming it."""
    found: list[TestLine] = []
    for line in messages(stdout, "blaze-test-result"):
        found.append(TestLine(
            binary=field_of(line, "binary", str),
            executable=field_of(line, "executable", int),
            name=field_of(line, "name", str),
            outcome=_outcome(line),
            millis=field_of(line, "millis", int),
            outside=field_of(line, "outside", bool),
            cached=_cached(line),
            output=field_of(line, "output", str),
        ))
    return found
