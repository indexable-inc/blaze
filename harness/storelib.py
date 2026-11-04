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

`scratch` and `run` are what other harnesses take from here too.
"""
import concurrent.futures, dataclasses, hashlib, json, os, re, shlex, shutil, signal, subprocess, sys, tempfile, time

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


def scratch(prefix):
    # cargo names a checkout by its real path, and the temporary directory of macOS is a link.
    # That directory is also too long for the path of a daemon's socket in it (104 bytes).
    return os.path.realpath(tempfile.mkdtemp(prefix=prefix, dir="/tmp" if sys.platform == "darwin" else None))


def run(path):
    try:
        ran = subprocess.run([*RUNNER, path], capture_output=True, text=True, timeout=60)
    except Exception as error:
        return f"did not run: {error}"
    return ran.stdout + (f"[exit {ran.returncode}]" if ran.returncode else "")


def printed(executable):
    """What a program prints, with its exit status when that is not 0."""
    try:
        ran = subprocess.run([executable], capture_output=True, text=True, timeout=RUN_SECS)
    except (OSError, subprocess.TimeoutExpired) as error:
        return f"did not run: {error}"
    return ran.stdout + (f"[exit {ran.returncode}]" if ran.returncode else "")


def state_of(home):
    return os.path.join(home, "v2")


def store_of(home):
    return os.path.join(home, "v2", "store")


def kind(name):
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


def files(store, *kinds):
    """The paths of the store's files of those kinds, by name; a store not there has none."""
    if not os.path.isdir(store):
        return []
    return sorted(os.path.join(store, name) for name in os.listdir(store) if kind(name) in kinds)


def listing(store):
    """Each file of the store with a hash of its bytes: two equal listings are the same store."""
    if not os.path.isdir(store):
        return {}
    out = {}
    for name in sorted(os.listdir(store)):
        path = os.path.join(store, name)
        if os.path.isfile(path):
            with open(path, "rb") as file:
                out[name] = hashlib.blake2b(file.read(), digest_size=16).hexdigest()
    return out


def stored_bytes(store):
    return sum(os.path.getsize(path) for path in files(store, "head", "index", "pack", "file"))


def forget(home):
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


def named(text, path):
    """Whether what blaze said names the file at `path`: a store's files are named by the
    driver as paths under its root `/` without the leading slash."""
    return path.lstrip("/") in text


def environment(home):
    env = {name: value for name, value in os.environ.items() if name not in SCRUBBED}
    env["BLAZE_HOME"] = home
    return env


class Built:
    """One `blaze build`: its exit status, what it said, and the program it built (None when
    it built none)."""

    def __init__(self, status, said, executable):
        self.status, self.said, self.executable = status, said, executable

    def program(self):
        return printed(self.executable) if self.executable else None


def artifact(stdout, program):
    """The executable of `program` in what `blaze build --message-format=json` printed."""
    found = None
    for line in stdout.splitlines():
        if not line.startswith("{"):
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") == "compiler-artifact" and message.get("executable") \
                and message.get("target", {}).get("name") == program:
            found = message["executable"]
    return found


class Daemon:
    """`blaze serve` with `home` as its home, its output appended to `<home>/<name>.log`; `env`
    is set for it and for every build asked of it, over what `environment` leaves, a value of
    None unsetting the variable."""

    def __init__(self, blaze, home, name="serve", wait=True, env=None):
        self.blaze, self.home = blaze, home
        os.makedirs(home, exist_ok=True)
        self.env = {name: value for name, value in {**environment(home), **(env or {})}.items() if value is not None}
        self.log_path = os.path.join(home, f"{name}.log")
        self.seen = os.path.getsize(self.log_path) if os.path.exists(self.log_path) else 0
        with open(self.log_path, "a") as out:
            self.process = subprocess.Popen([blaze, "serve"], stdin=subprocess.DEVNULL, stdout=out, stderr=out,
                                            env=self.env, start_new_session=True)
        self.listening = False
        if wait:
            self.wait()

    @property
    def store(self):
        return store_of(self.home)

    def log(self):
        """What this daemon printed, without what those before it in the same file did."""
        with open(self.log_path, errors="replace") as log:
            log.seek(self.seen)
            return log.read()

    def alive(self):
        return self.process.poll() is None

    def listens(self):
        """Whether it has said it listens, without waiting."""
        return LISTENING in self.log()

    def wait(self, timeout=START_SECS):
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

    def argv(self, *args):
        return [self.blaze, "build", "--message-format=json", *args]

    def asking(self, ws, *args):
        """A `blaze build` of `ws` started and not waited for."""
        if not self.listening or not self.alive():
            raise Failed(f"a build asked of a daemon that does not listen: {self.log_path}")
        return subprocess.Popen(self.argv(*args), cwd=ws, env=self.env, stdin=subprocess.DEVNULL,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def answer(self, client, program=PROGRAM, timeout=BUILD_SECS):
        try:
            out, err = client.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            client.kill()
            out, err = client.communicate()
            raise Failed(f"`blaze build` took more than {timeout} s:\n{(out + err)[-3000:]}\nlog: {self.log_path}")
        return Built(client.returncode, out + err, artifact(out, program) if client.returncode == 0 else None)

    def build(self, ws, *args, program=PROGRAM, timeout=BUILD_SECS):
        return self.answer(self.asking(ws, *args), program, timeout)

    def stop(self, timeout=STOP_SECS):
        """`blaze stop`, which writes the daemon's image and ends it, waited for."""
        if self.alive():
            subprocess.run([self.blaze, "stop"], env=self.env, capture_output=True, timeout=timeout)
            try:
                self.process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                self.kill()
                raise Failed(f"`blaze stop` did not end the daemon in {self.home} within {timeout} s: {self.log_path}")
        self.stray()

    def kill(self):
        if self.alive():
            os.kill(self.process.pid, signal.SIGKILL)
        self.process.wait()

    def stray(self):
        """Whether a `blaze build` found no daemon and started one of its own; that one is
        stopped and its log moved aside, so no other daemon is on the store."""
        log = os.path.join(state_of(self.home), "serve.log")
        if not os.path.exists(log):
            return False
        subprocess.run([self.blaze, "stop"], env=self.env, capture_output=True, timeout=STOP_SECS)
        os.replace(log, f"{log}.{time.time_ns()}")
        return True


def checkout(at, value=0):
    """A copy of the stress workspace at `at`, printing what it prints with `value` added to a
    tile's area (0 is the workspace as it came)."""
    shutil.copytree(STRESS, at, ignore=shutil.ignore_patterns("target*"))
    set_value(at, 0, value)
    return at


def set_value(ws, old, new):
    path = os.path.join(ws, MID)
    text = open(path).read()
    was = AREA + (f" + {old}" if old else "")
    if was not in text:
        raise Failed(f"{path} has no `{was}`")
    open(path, "w").write(text.replace(was, AREA + (f" + {new}" if new else ""), 1))


class Checkout:
    """A copy of the stress workspace that a test edits, at `<work>/<name>`."""

    def __init__(self, work, name="checkout"):
        self.ws = checkout(os.path.join(work, name))
        self.value = 0

    def set(self, value):
        set_value(self.ws, self.value, value)
        self.value = value


def from_nothing(blaze, at, value, keep):
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
        raise Failed(f"the build from nothing of {value} failed ({built.status}):\n{built.said[-3000:]}\nlog: {daemon.log_path}")
    out = built.program()
    if not keep:
        shutil.rmtree(at, ignore_errors=True)
    return out


def references(blaze, work, values, jobs=4):
    """{value: what the stress program prints when built from nothing}, built `jobs` at a time."""
    values = sorted(set(values))
    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as pool:
        made = {value: pool.submit(from_nothing, blaze, os.path.join(work, f"ref-{value}"), value, False)
                for value in values}
        return {value: future.result() for value, future in made.items()}


def blaze_of(path):
    """The `blaze` command line at `path`, refused when it is not there."""
    path = os.path.abspath(path)
    if not os.path.isfile(path) or not os.access(path, os.X_OK):
        raise Failed(f"no blaze at {path}")
    return path


# What blaze says of itself in its machine formats, read into frozen dataclasses: the
# `build-finished` line every build, check, run and test ends its build with under
# `--message-format=json` (or `json-render-diagnostics`), `blaze status --json` and
# `blaze toolchain --json` (docs/cargo.md names every field). Each reader refuses a value of
# another shape by raising `Failed` naming the field, never guesses: a harness that reads one
# wrong is a harness bug to see, not a build to count. Self-contained (the standard library
# only) and typed for pyright's strict mode, so any harness takes them as they are.

type Json = None | bool | int | float | str | list["Json"] | dict[str, "Json"]


def _object(where: str, value: Json) -> dict[str, Json]:
    if not isinstance(value, dict):
        raise Failed(f"{where} is not a JSON object: {value!r:.200}")
    return value


def _field[T](where: str, value: dict[str, Json], name: str, kind: type[T]) -> T:
    if name not in value:
        raise Failed(f"{where} has no `{name}`: {value!r:.300}")
    it = value[name]
    # A JSON true is a Python int too: a bool is one only where one is asked for.
    if not isinstance(it, kind) or (kind is int and isinstance(it, bool)):
        raise Failed(f"{where}'s `{name}` is not {kind.__name__}: {it!r:.200}")
    return it


def _maybe[T](where: str, value: dict[str, Json], name: str, kind: type[T]) -> T | None:
    if value.get(name, ...) is None:
        return None
    return _field(where, value, name, kind)


def _only(where: str, value: dict[str, Json], names: set[str]) -> None:
    unknown = sorted(set(value) - names)
    if unknown:
        raise Failed(f"{where} has fields this harness does not know: {', '.join(unknown)}")


@dataclasses.dataclass(frozen=True)
class FinishedUnits:
    """The build's units: made again, and replayed from the store as they were."""
    built: int
    replayed: int


@dataclasses.dataclass(frozen=True)
class FinishedLinks:
    """Each image the build linked, by how: its first link, spliced into the last, unchanged,
    or laid out whole again (`full`)."""
    first: int
    spliced: int
    unchanged: int
    full: int


@dataclasses.dataclass(frozen=True)
class BuildFinished:
    """`{"reason":"build-finished", ...}`: how a build ended."""
    success: bool
    # The profile's name (`dev`, `release`), None when the build placed nothing.
    profile: str | None
    # "daemon", or "in-process" for the seed's one-shot engine.
    served: str
    took_ms: int
    # The history's `build_id` (`blaze slow --build N`); None with no work report.
    build_id: int | None
    units: FinishedUnits | None
    links: FinishedLinks | None

    @property
    def daemon(self) -> bool:
        """Whether blaze's daemon built it and the build succeeded."""
        return self.success and self.served == "daemon"


FINISHED_FIELDS = {"reason", "success", "profile", "served", "took_ms", "build_id", "units", "links"}


def build_finished_of(line: str) -> BuildFinished:
    """One `build-finished` line read, refused whole when it is not one."""
    where = "the `build-finished` line"
    try:
        value = _object(where, json.loads(line))
    except json.JSONDecodeError as error:
        raise Failed(f"{where} is not JSON ({error}): {line!r:.300}") from error
    if value.get("reason") != "build-finished":
        raise Failed(f"{where} has reason {value.get('reason')!r}: {line!r:.300}")
    _only(where, value, FINISHED_FIELDS)
    units = links = None
    if (raw := value.get("units")) is not None:
        it = _object(f"{where}'s `units`", raw)
        _only(f"{where}'s `units`", it, {"built", "replayed"})
        units = FinishedUnits(built=_field(where, it, "built", int), replayed=_field(where, it, "replayed", int))
    if (raw := value.get("links")) is not None:
        it = _object(f"{where}'s `links`", raw)
        _only(f"{where}'s `links`", it, {"first", "spliced", "unchanged", "full"})
        links = FinishedLinks(first=_field(where, it, "first", int), spliced=_field(where, it, "spliced", int),
                              unchanged=_field(where, it, "unchanged", int), full=_field(where, it, "full", int))
    return BuildFinished(
        success=_field(where, value, "success", bool),
        profile=_maybe(where, value, "profile", str),
        served=_field(where, value, "served", str),
        took_ms=_field(where, value, "took_ms", int),
        build_id=_maybe(where, value, "build_id", int),
        units=units,
        links=links,
    )


FINISHED_PREFIX = '{"reason":"build-finished"'


def split_build_output(stdout: str) -> tuple[BuildFinished | None, str]:
    """A command's stdout under `--message-format=json(-render-diagnostics)` cut at its
    `build-finished` line: what that line says, and what came after it, which is the program's
    (`blaze run`) or the tests' (`blaze test`) own output. Every line before it is one of blaze's
    JSON messages. (None, stdout) when no build finished: a command refused before it built.
    More than one `build-finished` is refused: one command is one build."""
    lines = stdout.splitlines(keepends=True)
    at = [n for n, line in enumerate(lines) if line.startswith(FINISHED_PREFIX)]
    if not at:
        return None, stdout
    if len(at) > 1:
        raise Failed(f"one command's stdout has {len(at)} `build-finished` lines: {stdout[-1500:]!r}")
    return build_finished_of(lines[at[0]]), "".join(lines[at[0] + 1:])


def build_finished(stdout: str) -> BuildFinished | None:
    """The `build-finished` line of a command's stdout, read; None when it has none."""
    return split_build_output(stdout)[0]


@dataclasses.dataclass(frozen=True)
class DaemonStatus:
    """The daemon's half of `blaze status --json`."""
    pid: int
    # The hash of its executable as it started, in hex.
    build: str
    exe: str
    uptime_ms: int
    memory_bytes: int
    budget_bytes: int
    # Bugs it went on past, every site's summed.
    bugs: int
    requests: tuple[int, ...]
    views: int
    history: str | None
    settings: tuple[tuple[str, str | None], ...]


@dataclasses.dataclass(frozen=True)
class BlazeStatus:
    """`blaze status --json`: this blaze, where it keeps its state, and its daemon (None when
    none runs)."""
    version: str
    api_schema: int
    home: str
    state: str
    socket: str
    daemon: DaemonStatus | None


STATUS_FORMAT = 1


def blaze_status_of(text: str) -> BlazeStatus:
    """What `blaze status --json` printed, read."""
    where = "`blaze status --json`"
    try:
        value = _object(where, json.loads(text))
    except json.JSONDecodeError as error:
        raise Failed(f"{where} is not JSON ({error}): {text!r:.300}") from error
    _only(where, value, {"format", "version", "api_schema", "home", "state", "socket", "daemon"})
    if (form := _field(where, value, "format", int)) != STATUS_FORMAT:
        raise Failed(f"{where} is of format {form}, this harness reads {STATUS_FORMAT}")
    daemon = None
    if (raw := value.get("daemon")) is not None:
        inner = f"{where}'s `daemon`"
        it = _object(inner, raw)
        _only(inner, it, {"pid", "build", "exe", "uptime_ms", "memory_bytes", "budget_bytes", "bugs",
                          "requests", "views", "history", "settings"})
        requests_raw, settings_raw = it.get("requests"), it.get("settings")
        if not isinstance(requests_raw, list) or not isinstance(settings_raw, dict):
            raise Failed(f"{inner}'s `requests` is no list or its `settings` no object: {raw!r:.300}")
        requests = tuple(r for r in requests_raw if isinstance(r, int) and not isinstance(r, bool))
        settings = tuple((k, v) for k, v in sorted(settings_raw.items()) if v is None or isinstance(v, str))
        if len(requests) != len(requests_raw) or len(settings) != len(settings_raw):
            raise Failed(f"{inner} has a request that is no number or a setting that is no string: {raw!r:.300}")
        daemon = DaemonStatus(
            pid=_field(inner, it, "pid", int),
            build=_field(inner, it, "build", str),
            exe=_field(inner, it, "exe", str),
            uptime_ms=_field(inner, it, "uptime_ms", int),
            memory_bytes=_field(inner, it, "memory_bytes", int),
            budget_bytes=_field(inner, it, "budget_bytes", int),
            bugs=_field(inner, it, "bugs", int),
            requests=requests,
            views=_field(inner, it, "views", int),
            history=_maybe(inner, it, "history", str),
            settings=settings,
        )
    return BlazeStatus(
        version=_field(where, value, "version", str),
        api_schema=_field(where, value, "api_schema", int),
        home=_field(where, value, "home", str),
        state=_field(where, value, "state", str),
        socket=_field(where, value, "socket", str),
        daemon=daemon,
    )


def blaze_status(blaze: str, env: dict[str, str], cwd: str | None = None, timeout: float = 60) -> BlazeStatus:
    """`blaze status --json` asked of the daemon `env`'s home names (`BLAZE_HOME`); it starts
    none. A failed answer is `Failed`, naming its exit code and stderr."""
    ran = subprocess.run([blaze, "status", "--json"], cwd=cwd, env=env, capture_output=True, text=True,
                         timeout=timeout, stdin=subprocess.DEVNULL)
    if ran.returncode != 0:
        raise Failed(f"`{blaze} status --json` exited {ran.returncode}: {ran.stderr[-1500:]}")
    return blaze_status_of(ran.stdout)


def daemon_pid(blaze: str, env: dict[str, str], cwd: str | None = None) -> int | None:
    """The pid of the daemon of `env`'s home, as the daemon itself answers it; None when none
    answers. Where no daemon may be up to answer (a daemon that died, one still writing its image
    as it stops) only its `serve.lock` can say which process held the state; that read stays with
    the callers that need exactly that, as no status can come from a daemon that is not there."""
    status = blaze_status(blaze, env, cwd)
    return status.daemon.pid if status.daemon else None


@dataclasses.dataclass(frozen=True)
class Toolchain:
    """`blaze toolchain --json`: the Rust blaze builds with, as blaze and rustup name it."""
    name: str
    nightly: str
    rustc: str
    release: str
    commit: str
    host: str
    sysroot: str


def blaze_toolchain_of(text: str) -> Toolchain:
    """What `blaze toolchain --json` printed, read. A host blaze ships no toolchain for
    (`"host": null`) is refused: nothing it would build is for a tuple anyone can name."""
    where = "`blaze toolchain --json`"
    try:
        value = _object(where, json.loads(text))
    except json.JSONDecodeError as error:
        raise Failed(f"{where} is not JSON ({error}): {text!r:.300}") from error
    names = {"name", "nightly", "rustc", "release", "commit", "host", "sysroot"}
    _only(where, value, names)
    host = _maybe(where, value, "host", str)
    if host is None:
        raise Failed(f"{where}: blaze ships no toolchain for this host")
    return Toolchain(
        name=_field(where, value, "name", str),
        nightly=_field(where, value, "nightly", str),
        rustc=_field(where, value, "rustc", str),
        release=_field(where, value, "release", str),
        commit=_field(where, value, "commit", str),
        host=host,
        sysroot=_field(where, value, "sysroot", str),
    )


def blaze_toolchain(blaze: str, env: dict[str, str], timeout: float = 900) -> Toolchain:
    """`blaze toolchain --json`: downloads blaze's Rust once when it is not there yet."""
    ran = subprocess.run([blaze, "toolchain", "--json"], env=env, capture_output=True, text=True,
                         timeout=timeout, stdin=subprocess.DEVNULL)
    if ran.returncode != 0:
        raise Failed(f"`{blaze} toolchain --json` exited {ran.returncode}: {ran.stderr[-1500:]}")
    return blaze_toolchain_of(ran.stdout)
