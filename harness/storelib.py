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
import concurrent.futures, hashlib, json, os, re, shlex, shutil, signal, subprocess, sys, tempfile, time

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
