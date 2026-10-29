#!/usr/bin/env python3
"""blaze_cli.py: `blaze` against plain cargo on real projects, unmodified.

    blaze_cli.py [PROJECT...] [--small] [--only-projects] [--only-checks] [--only-upgraded] [--keep]

Four parts, all of which have to pass.

Projects. Each one below is cloned at its tag, which has to be the commit it is pinned to, into `BLAZE_PROJECTS` (~/blaze-projects) when
it is not there, and copied; the clone is never written to. In the copy, with plain cargo
building into a target directory of its own as the reference:
  cold      `blaze build` from nothing, with no daemon; the program is run
  warm      once the daemon has loaded the workspace, `blaze build` again; the program,
            which is now the one the daemon linked, is run
  test      `blaze test`, cold and again warm
  edit      the source is edited; `blaze build`, the program is run, `blaze test`
  broken    a type error is added; both have to fail with the same status
After every step the program's output and exit status and the result of every test have to be
what plain cargo gives for the same source. A build cargo did in place of the daemon passes
(that is the contract) and is in the table with the reason.

Then a second checkout of the repository is built, edited and built again, and the
functions the daemon compiled itself are counted.

Flags. A small workspace made here is built and run with each flag `blaze` passes
through, cold and after an edit, and compared with cargo the same way.

Daemon. Started on demand, reused, shared by a git worktree, alive after its client is
killed, replaced when the client is another build of blaze, gone when idle, and its workers
gone with it. A project whose rust-toolchain file names an older stable or a nightly is built
and edited by the daemon, under the toolchain blaze compiles; a `#![feature]` of the nightly
builds, and an error in it fails, with nothing said of the project's toolchain but one line
under `-v`. With no rustup and no Rust on
the path, `blaze run` builds with blaze's own copy of its release and the edit is the daemon's.

Upgraded. A project whose rust-toolchain file names another release: that is not said, and the
build is not cargo's. `--only-upgraded` runs
this part alone.

Writes BLAZE-CLI.md and blaze_cli.json next to this file, or BLAZE-CLI-LLVM.md and
blaze_cli_llvm.json under `BLAZE_BACKEND=llvm`. A run of some projects replaces their rows
and keeps the others. `--small` is the three projects of the CI job.
"""
import collections, glob, json, os, re, shutil, signal, stat, subprocess, sys, tempfile, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
# `difftest` turns incremental compilation off for the harnesses that build with rustc
# alone. Here it stays as the caller has it: without it the tests of every project are
# cargo's, and that case is one of the flags.
INCREMENTAL = os.environ.get("CARGO_INCREMENTAL")
from difftest import host
if INCREMENTAL is None:
    os.environ.pop("CARGO_INCREMENTAL", None)
else:
    os.environ["CARGO_INCREMENTAL"] = INCREMENTAL
ROOT = os.path.dirname(HERE)
WINDOWS = os.name == "nt"
EXE = ".exe" if WINDOWS else ""
BLAZE = os.environ.get("BLAZE_BIN") or os.path.join(ROOT, "target/release/blaze-engine" + EXE)
TOOLCHAIN = open(os.path.join(HERE, "toolchain")).read().strip()
SOURCES = os.environ.get("BLAZE_PROJECTS") or os.path.expanduser("~/blaze-projects")
WARMUP_SECS = 1800

PROJECTS = {
    "ripgrep": dict(
        repo="BurntSushi/ripgrep", tag="15.1.0", rev="af60c2de9d85e7f3d81c78601669468cf02dabab",
        run=["-n", "--no-heading", "--sort", "path", "fn main", "crates/core/main.rs", "build.rs"],
        edits=[("crates/core/main.rs", "fn main() -> ExitCode {", 'fn main() -> ExitCode {\n    eprintln!("edited main");'),
               ("crates/printer/src/standard.rs", "pub fn new() -> StandardBuilder {", 'pub fn new() -> StandardBuilder {\n        eprintln!("edited printer");')],
    ),
    "fd": dict(
        repo="sharkdp/fd", tag="v10.3.0", rev="d38148f0aabdd073b4080cde770f679f3197b920",
        run=["--extension", "rs", "main", "src"],
        edits=[("src/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    "bat": dict(
        repo="sharkdp/bat", tag="v0.26.0", rev="c734087e1d15fb730d7653389c841fd9e3016556",
        run=["--plain", "--color=never", "rustfmt.toml"],
        edits=[("src/bin/bat/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    "hyperfine": dict(
        repo="sharkdp/hyperfine", tag="v1.19.0", rev="12fec42098642a19855ead34c8cb1e0be28c8ead",
        run=["--help"],
        edits=[("src/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    "just": dict(
        repo="casey/just", tag="1.43.0", rev="b7e7c46495b34e9407982ffbbb25eb2808ec40b7",
        run=["--list", "--unsorted"],
        edits=[("src/main.rs", "fn main() {", 'fn main() {\n  eprintln!("edited main");')],
    ),
    "axum": dict(
        repo="tokio-rs/axum", tag="axum-v0.8.6", rev="c1bb9c3e90625fb8de4f9c9750f669cb4edac3df",
        build=["-p", "example-hello-world"], serves="http://127.0.0.1:3000/",
        test=["-p", "example-testing"],
        edits=[("examples/hello-world/src/main.rs", "<h1>Hello, World!</h1>", "<h1>Hello, edited world!</h1>"),
               ("examples/testing/src/main.rs", "fn app() -> Router {", 'fn app() -> Router {\n    eprintln!("edited app");')],
        broken="examples/hello-world/src/main.rs",
    ),
    "nushell": dict(
        repo="nushell/nushell", tag="0.108.0", rev="da141be11b3acaa55b39321e5f9651d02aa2ed6f",
        run=["-n", "-c", "[3 1 2] | sort | str join ','"],
        test=["-p", "nu-path"],
        edits=[("src/main.rs", "fn main() -> Result<()> {", 'fn main() -> Result<()> {\n    eprintln!("edited main");')],
    ),
    "cargo": dict(
        repo="rust-lang/cargo", tag="0.92.0", rev="ea2d97820c16195b0ca3fadb4319fe512c199a43",
        run=["metadata", "--format-version", "1", "--no-deps", "--offline", "--manifest-path", "crates/cargo-platform/Cargo.toml"],
        test=["-p", "cargo-platform"],
        edits=[("src/bin/cargo/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    # The unit tests of cargo's own library, which is most of cargo.
    "cargo-lib": dict(
        repo="rust-lang/cargo", tag="0.99.0", rev="797e8a9bca276c1c9f9f738d2a20f484fa4eea9d",
        run=["--list"], test=["--lib"],
        edits=[("src/bin/cargo/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    "rust-analyzer": dict(
        repo="rust-lang/rust-analyzer", tag="2026-10-12", rev="2ad81e72b7e978cb800dffe1bcce5c9f98032acf",
        build=["-p", "rust-analyzer"], run=["--version"], test=["-p", "stdx"],
        edits=[("crates/rust-analyzer/src/bin/main.rs", "fn main() -> anyhow::Result<ExitCode> {", 'fn main() -> anyhow::Result<ExitCode> {\n    eprintln!("edited main");')],
    ),
    "tokei": dict(
        repo="XAMPPRocky/tokei", tag="v15.0.0", rev="c14f744716272fadeb27a74443cdffa0af35f82f",
        run=["src", "--sort", "code"],
        edits=[("src/main.rs", "fn main() -> Result<(), Box<dyn Error>> {", 'fn main() -> Result<(), Box<dyn Error>> {\n    eprintln!("edited main");')],
    ),
    "eza": dict(
        repo="eza-community/eza", tag="v0.23.5", rev="98442ab17c2c3738701b62a7e060b1431ae2d6ea",
        run=["--oneline", "--color=never", "src"],
        edits=[("src/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    "starship": dict(
        repo="starship/starship", tag="v1.27.0", rev="f8e4e2155542cc84f49b342122d371f74e0ad253",
        run=["module", "character"], test=["--lib", "modules::character"],
        edits=[("src/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    "delta": dict(
        repo="dandavison/delta", tag="0.20.1", rev="b88d2290468449d75950bfa150ea7c1291fe2977",
        run=["--list-languages"],
        edits=[("src/main.rs", "fn main() -> std::io::Result<()> {", 'fn main() -> std::io::Result<()> {\n    eprintln!("edited main");')],
    ),
    "helix": dict(
        repo="helix-editor/helix", tag="25.07.1", rev="a05c151bb6e8e9c65ec390b0ae2afe7a5efd619b",
        env=dict(HELIX_DISABLE_AUTO_GRAMMAR_BUILD="1"),
        run=["--version"], test=["-p", "helix-stdx"],
        edits=[("helix-term/src/main.rs", "fn main() -> Result<()> {", 'fn main() -> Result<()> {\n    eprintln!("edited main");')],
    ),
    "tonic": dict(
        repo="hyperium/tonic", tag="v0.14.5", rev="3f7caf3171393734ef19e12d010bd9c945c9e242",
        build=["-p", "examples", "--bin", "helloworld-server"], client="helloworld-client",
        test=["-p", "tonic-types"],
        edits=[("examples/src/helloworld/server.rs", '"Hello {}!"', '"Hello edited {}!"')],
    ),
    "wasmtime": dict(
        repo="bytecodealliance/wasmtime", tag="v49.0.3", rev="f96eabaa11a06086e4fb779ed1c1688481067ab9",
        run=["wast", "tests/misc_testsuite/add.wast"], test=["-p", "wasmtime-environ"],
        # With debug info one target directory of it is 8.5 GB and the wrapper's copies as
        # much again; the two builds side by side are more than the 20 GB this was run on.
        env={"CARGO_PROFILE_DEV_DEBUG": "0"},
        edits=[("src/bin/wasmtime.rs", "fn main() -> Result<()> {", 'fn main() -> Result<()> {\n    eprintln!("edited main");')],
    ),
}

# The projects of the CI job: small, and a build script, a workspace and a test suite among them.
SMALL = ["ripgrep", "fd", "hyperfine"]
# How a project is built: the name of the mode, and how the report names it.
MODES = {"blaze": "`blaze`"}

# What cargo and `blaze` print about the build, which is not the program's output.
# cargo's status words, then blaze's own steps' (crates/blaze-ui), right-aligned to 12 columns.
STATUS = re.compile(r"^\s*((Compiling|Checking|Finished|Running|Downloading|Downloaded|Blocking|Waiting|Locking|Updating|Adding|Fresh|Dirty|Doc-tests)\b|blaze:|warning:|`blaze why` says more$)"
                    r"|^(?=[ \w-]{12} )\s+(Loading|Loaded|Fetching|Fetched|Resolving|Resolved|Compiled|Linking|Linked|Checked|Testing|Tested|Ran|Rebuilt|Finishing|Took|Slowest|Cache|Chain|Building|Built|Storing|Stored|Optimizing|Optimized) ")


def base_env(home):
    env = dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN, BLAZE_HOME=home, CARGO_TERM_COLOR="never")
    # Every side's cargo lays out a build as stable cargo does, as a user's does and as blaze
    # places a test: in nightly's second layout a test is in its unit's directory, from where
    # fd's, ripgrep's and just's tests find no program beside them.
    env["CARGO_UNSTABLE_BUILD_DIR_NEW_LAYOUT"] = "false"
    # The store is on by default, in the user's cache directory: a run has one of its own.
    env["BLAZE_CACHE"] = os.path.join(home, "store")
    env["PATH"] = os.path.dirname(BLAZE) + os.pathsep + env["PATH"]
    # Set, not unset: cargo's `[env]` (as in ~/.cargo/config.toml) fills in only what is not already
    # set, so an unset RUST_BACKTRACE would take its value and panics would print differently.
    env["RUST_BACKTRACE"] = "0"
    for name in ("RUSTFLAGS", "CARGO_TARGET_DIR", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_TARGET"):
        env.pop(name, None)
    return env


# A process started with this is the head of a group or tree that `kill_all` ends whole.
OWN_GROUP = dict(creationflags=subprocess.CREATE_NEW_PROCESS_GROUP) if WINDOWS else dict(start_new_session=True)


def kill_all(process):
    """Ends `process` and what it started, with no chance to clean up."""
    if WINDOWS:
        subprocess.run(["taskkill", "/F", "/T", "/PID", str(process.pid)], capture_output=True)
    else:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            # It has not made its group yet, or the group has only the dead left in it, which
            # macOS says is not permitted: the process alone is what there is to end.
            try:
                process.kill()
            except ProcessLookupError:
                pass


def kill(pid):
    """Ends the one process `pid`. Windows has no signals; `SIGTERM` there ends it at once."""
    os.kill(pid, signal.SIGTERM if WINDOWS else signal.SIGKILL)


def alive(pid):
    """Whether `pid` is a process that still runs."""
    if os.path.isdir("/proc/self"):
        return os.path.exists(f"/proc/{pid}") and "Z" not in open(f"/proc/{pid}/stat").read().split(")")[-1].split()[0]
    if not WINDOWS:
        # No /proc on macOS: `ps` says the state, and nothing of a process that is gone.
        state = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
        return bool(state) and "Z" not in state
    listed = subprocess.run(["tasklist", "/FI", f"PID eq {pid}", "/FO", "CSV", "/NH"], capture_output=True, text=True)
    return f'"{pid}"' in listed.stdout


def another_build(other):
    """Another build of blaze in `other`: the same files in another place have another
    version. It has what a release ships beside blaze, as `BLAZE`'s directory does (the Pack
    step of blaze-platform.yaml): a load of a workspace with a proc macro fails without the
    proc-macro server (`own_macro::find`)."""
    os.makedirs(other, exist_ok=True)
    shipped = [it + EXE for it in ("blaze-engine", "blaze", "blaze-rustc", "blaze-proc-macro-srv")]
    for name in shipped:
        if os.path.exists(os.path.join(os.path.dirname(BLAZE), name)):
            shutil.copy(os.path.join(os.path.dirname(BLAZE), name), os.path.join(other, name))
    return other


def timed(argv, cwd, env, timeout=7200):
    began = time.time()
    ran = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=timeout, stdin=subprocess.DEVNULL)
    return time.time() - began, ran


class Side:
    """One of the builds of a checkout: `blaze` (`True`), or cargo alone into `target-ref`
    (`False`)."""

    def __init__(self, ws, env, blaze):
        self.ws, self.blaze = ws, bool(blaze)
        self.env = env if blaze else dict(env, CARGO_TARGET_DIR=os.path.join(ws, "target-ref"))

    def argv(self, command, *args):
        return [*(["blaze"] if self.blaze else ["cargo"]), command, *args]

    def cargo(self, command, *args, env=None, timeout=7200):
        """Runs it. Of `blaze test`, what ran also has `kept`: the tests blaze did not run for
        having passed as they are now, from its `tests.ndjson`."""
        env = dict(self.env, **(env or {}))
        report = self.blaze and command == "test" and os.path.join(env.get("CARGO_TARGET_DIR") or os.path.join(self.ws, "target"), "blaze", "tests.ndjson")
        if report and os.path.exists(report):
            os.remove(report)
        took, ran = timed(self.argv(command, *args), self.ws, env, timeout)
        if report:
            ran.kept = kept_passes(report)
        return took, ran

    def how_built(self, stderr):
        """Whether the daemon built what the last command asked for, or why cargo did."""
        return how_built(stderr)

    def share(self):
        """The functions of the largest load the daemon compiled itself, of all it has."""
        loads = []
        workers = os.path.join(self.env["BLAZE_HOME"], "workers")
        for worker in os.listdir(workers) if os.path.isdir(workers) else []:
            path = os.path.join(workers, worker, "log")
            # A worker that began as a spare keeps its log where it began.
            log = open(path, errors="replace").read() if os.path.exists(path) else ""
            loads += [(int(of), int(done)) for done, of in re.findall(r"^cold: compiled (\d+) of (\d+) fns", log, re.M)]
        return max(loads, default=(0, 0))[::-1]


def by_rustc(stderr):
    """Whether cargo built any of it: blaze said it fell back, or cargo finished a build.
    `blaze build`, `run` and `test` say `Compiling` of each package themselves (the
    whole build, docs/no-rustc.md step K), so that line is no sign of cargo; cargo's own
    `Finished` line names its profile's settings, where blaze's says `[blaze]`."""
    return bool(re.search(r"; (building with cargo|this build is cargo's)(, which .*)?$|^\s*Finished `\w+` profile \[(?!blaze|blaze)", stderr, re.M))


def how_built(stderr):
    """Whether the daemon built it, or why cargo did."""
    reason = re.search(r"^blaze: (?:bug: )?(.*?); (building with cargo|this build is cargo's|this is `cargo \w+`)(?:, which .*)?$", stderr, re.M)
    if reason:
        return "cargo: " + reason[1]
    kept = re.search(r"^blaze: (.*); nothing has changed since cargo built$", stderr, re.M)
    if kept:
        return "unchanged: " + kept[1]
    return "daemon" if "[blaze]" in stderr else "cargo"


def program_output(ran):
    """What the program said, without what cargo said, and with the two target directories
    named alike."""
    def clean(text):
        # What rustc warned of, which cargo shows again with every build, comes before
        # cargo says what it runs.
        lines = text.splitlines()
        running = [at for at, l in enumerate(lines) if re.match(r"^\s*Running `", l)]
        lines = [l for l in lines[running[-1] + 1 if running else 0:] if not STATUS.match(l)]
        text = "\n".join(lines).replace("target-ref", "target")
        # A panic message has the id of the thread, and a logger the time.
        text = re.sub(r"' \(\d+\) panicked", "' panicked", text)
        return re.sub(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z?", "<time>", text)
    return ran.returncode, clean(ran.stdout), clean(ran.stderr)


def kept_passes(report):
    """The tests a `blaze test` kept the pass of and did not run, by libtest's name, from the
    `tests.ndjson` it wrote at `report` (docs/cargo.md)."""
    lines = open(report, encoding="utf-8").read().splitlines() if os.path.exists(report) else []
    return [test["name"] for test in map(json.loads, filter(None, lines)) if test.get("result") == "cached"]


def test_results(ran):
    """The outcome of each test, the totals of the test executables, and the exit status.
    `blaze test` does not run a test that passed as it is now, and lists it on stderr only
    (#836): a test it kept (`ran.kept`, from `Side.cargo`) is one cargo ran and passed. So the
    totals are summed over the executables, the kept passes with those run, and leave out how
    many tests a filter took out, which is what blaze runs of each executable the rest of."""
    # A doc test in a generated file is named by its path, in a directory cargo names
    # with a hash of how it was built (`build/<package>-<hash>/out` in cargo's first layout,
    # `build/<package>/<hash>/out` in its second).
    names = re.sub(r"([-/])[0-9a-f]{16}/", r"\1HASH/", ran.stdout.replace("target-ref", "target"))
    # A test's result is not always on the line of its name: what a program started by
    # another test prints lands between the two. So the names are read from the lines and
    # the failures from the list at the end, and the totals say the rest.
    failed = [name for listed in re.findall(r"^failures:\n((?:    \S.*\n)+)", names, re.M) for name in listed.split("\n")]
    kept = getattr(ran, "kept", [])
    tests = collections.Counter([(name, "ran") for name in re.findall(r"^test (.+?) \.\.\. ", names, re.M) + kept] + [(name.strip(), "FAILED") for name in failed if name])
    ended = re.findall(r"^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; \d+ filtered out", ran.stdout, re.M)
    passed, failing, ignored, measured = (sum(int(it[at]) for it in ended) for at in range(1, 5))
    totals = (sum(it[0] != "ok" for it in ended), passed + len(kept), failing, ignored, measured)
    return ran.returncode, tests, totals


def difference(ours, theirs):
    """`None` when the two are the same, or the first thing that is not."""
    if ours == theirs:
        return None
    if not isinstance(ours, tuple):
        return f"{ours!r} for {theirs!r}"
    for mine, other in zip(ours, theirs):
        if mine != other:
            if isinstance(mine, collections.Counter):
                only = list((mine - other).items())[:3], list((other - mine).items())[:3]
                return f"tests differ: blaze only {only[0]}, cargo only {only[1]}"
            if isinstance(mine, str):
                pairs = [(a, b) for a, b in zip(mine.splitlines(), other.splitlines()) if a != b][:2]
                return f"output differs: {pairs or (mine[-200:], other[-200:])}"
            return f"{mine!r} for {other!r}"
    return "differ"


def asked_by(client):
    """What the server says to the client program `client`, which cargo built: the message
    of its answer."""
    def ask(timeout):
        ran = subprocess.run([client], capture_output=True, text=True, timeout=timeout + 5)
        if ran.returncode != 0:
            raise OSError(ran.stderr[-200:])
        return "; ".join(re.findall(r'message: "[^"]*"', ran.stdout))
    return ask


def fetched(url):
    return lambda timeout: urllib.request.urlopen(url, timeout=timeout).read().decode()


def served(side, command, ask):
    """What the server `cargo run` starts on `side` answers when `ask` asks it."""
    server = subprocess.Popen(side.argv("run", *command), cwd=side.ws, env=side.env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, **OWN_GROUP)
    try:
        for _ in range(3000):
            if server.poll() is not None:
                return f"exited {server.returncode}: {server.stderr.read()[-300:]}"
            try:
                return ask(5)
            except OSError:
                time.sleep(0.1)
        return "no answer"
    finally:
        kill_all(server)
        server.wait()
        # The port is free again before the other side's server asks for it.
        for _ in range(100):
            try:
                ask(1)
                time.sleep(0.05)
            except OSError:
                break


def checking(root):
    """Whether a `cargo check` runs below `root`: the one `blaze` starts after cargo
    built, for the dependencies to be checked by the time the daemon builds."""
    if not os.path.isdir("/proc/self"):
        # No /proc on macOS: `ps` has the commands and `lsof` where each one runs.
        listed = subprocess.run(["ps", "-axo", "pid=,command="], capture_output=True, text=True).stdout
        checks = [l.split()[0] for l in listed.splitlines() if l.split()[2:3] == ["check"]]
        cwd = lambda pid: subprocess.run(["lsof", "-a", "-d", "cwd", "-p", pid, "-Fn"], capture_output=True, text=True).stdout
        return any(os.path.realpath(l[1:]).startswith(root) for pid in checks for l in cwd(pid).splitlines() if l.startswith("n"))
    for pid in filter(str.isdigit, os.listdir("/proc")):
        try:
            words = open(f"/proc/{pid}/cmdline", "rb").read().split(b"\0")
            if b"check" in words[1:2] and os.readlink(f"/proc/{pid}/cwd").startswith(root):
                return True
        except OSError:
            pass
    return False


def linked_for_loads(ws):
    """The build scripts and proc macros linked into blaze's own target directory
    (`target/rust-analyzer`) of the checkout `ws`, with when each was written: a load of
    `blaze` links them there, and reads them there again after."""
    top = os.path.join(ws, "target", "rust-analyzer")
    linked = {}
    for at, _, files in os.walk(top):
        for name in files:
            path = os.path.join(at, name)
            try:
                info = os.stat(path, follow_symlinks=False)
            except OSError:
                continue
            library = name.endswith((".so", ".dylib", ".dll"))
            if library or (stat.S_ISREG(info.st_mode) and info.st_mode & 0o111):
                linked[os.path.relpath(path, top)] = info.st_mtime_ns
    return linked


def wait_warm(side, root):
    """Waits until the daemon has every build of `root` loaded or has given up on one. The
    result is the seconds it took and how each ended."""
    # The daemon names a checkout by its real path: on macOS /tmp and /var are links.
    root = os.path.realpath(root)
    began = time.time()
    while time.time() - began < WARMUP_SECS:
        _, status = side.cargo("status")
        states = re.findall(rf"^    {re.escape(root)}: (.*)$", status.stdout, re.M)
        if states and not any(s.startswith("loading") for s in states) and not checking(root):
            return time.time() - began, states
        # No daemon loads what every build so far was left to cargo for.
        left = re.search(r"^daemon: not running$(?s:.*)^  every build is cargo's: (.*)$", status.stdout, re.M)
        if left:
            return time.time() - began, ["left to cargo: " + left[1]]
        time.sleep(0.25)
    return time.time() - began, ["not loaded in time"]


def rewrite(path, old, new):
    """Replaces the first `old` in the file by `new`. The result is the text it had."""
    text = open(path).read()
    assert old in text, f"{old!r} is not in {path}"
    open(path, "w").write(text.replace(old, new, 1))
    return text


def fetch(name, spec):
    """The checkout of the project, cloned at its tag when it is not there. The tag has to
    be the commit the project is pinned to."""
    source = os.path.join(SOURCES, name)
    if not os.path.isdir(source):
        os.makedirs(SOURCES, exist_ok=True)
        subprocess.run(["git", "clone", "-q", "--depth", "1", "--branch", spec["tag"], f"https://github.com/{spec['repo']}", source], check=True)
    head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=source, capture_output=True, text=True).stdout.strip()
    assert head == spec["rev"], f"{name} is at {head}, not at the {spec['rev']} it is pinned to"
    return source


def project(name, spec, work, mode):
    """Runs the steps of one project with `blaze` (`mode` "blaze"). The result is the row of
    the table: times, who built each step, and every difference from cargo."""
    source = fetch(name, spec)
    ws = os.path.realpath(os.path.join(work, name))
    shutil.copytree(source, ws, symlinks=True, ignore=shutil.ignore_patterns("target", "target-ref"))
    env = dict(base_env(os.path.join(work, "home-" + mode)), **spec.get("env", {}))
    ours, theirs = Side(ws, env, True), Side(ws, env, False)
    build, run, test = spec.get("build", []), spec.get("run", []), spec.get("test", [])
    row = dict(project=name, mode=mode, tag=spec["tag"], wrong=[], times={}, built={})
    subprocess.run(["cargo", "fetch"], cwd=ws, env=env, capture_output=True)
    ours.cargo("stop")

    def step(label, command, args, compare):
        """Runs `command` on both sides and compares what `compare` reads from each."""
        (took, mine), (reference, other) = ours.cargo(command, *args), theirs.cargo(command, *args)
        row["times"][label] = (round(took, 2), round(reference, 2))
        row["built"][label] = ours.how_built(mine.stderr)
        wrong = difference(compare(mine), compare(other))
        if wrong:
            row["wrong"].append(f"{label}: {wrong}")
            open(os.path.join(work, f"{name}-{label}.blaze.txt"), "w", encoding="utf-8").write(mine.stdout + mine.stderr)
            open(os.path.join(work, f"{name}-{label}.cargo.txt"), "w", encoding="utf-8").write(other.stdout + other.stderr)
        print(f"  {label:12} blaze {took:7.2f}s  cargo {reference:7.2f}s  {row['built'][label]:8.80}  {wrong or 'same'}", flush=True)
        # What the tests that failed here alone said.
        said = wrong and re.search(r"^failures:\n(.*?)^failures:", mine.stdout, re.M | re.S)
        if said:
            print("".join(f"      {line}\n" for line in said[1].splitlines()[:60]), end="", flush=True)
        # What blaze said when it failed and cargo did not: the files are gone with the work
        # directory, and CI keeps only this.
        elif wrong and mine.returncode != 0 and other.returncode == 0:
            print("".join(f"      {line}\n" for line in mine.stderr.strip().splitlines()[-30:]), end="", flush=True)

    def runs(label):
        if ask:
            mine, other = served(ours, build, ask), served(theirs, build, ask)
            if mine != other:
                row["wrong"].append(f"{label}: served {mine[:80]!r} for {other[:80]!r}")
            print(f"  {label:12} served {mine[:50]!r}  {'same' if mine == other else 'WRONG'}", flush=True)
        else:
            step(label, "run", [*build, "--", *run], program_output)

    def warm(label):
        took, states = wait_warm(ours, ws)
        row["times"][label] = (round(took, 2), None)
        row["built"][label] = "; ".join(states)
        print(f"  {label:12} {took:7.2f}s  {'; '.join(states)[:150]}", flush=True)

    status = lambda ran: ran.returncode
    step("cold build", "build", build, status)
    ask = fetched(spec["serves"]) if "serves" in spec else None
    if "client" in spec:
        theirs.cargo("build", "-p", build[1], "--bin", spec["client"])
        ask = asked_by(os.path.join(ws, "target-ref/debug", spec["client"]))
    runs("cold run")
    linked = linked_for_loads(ws)
    warm("warm-up")
    # The daemon loads the workspace with the build scripts and proc macros the cold build
    # had: cargo's, or those its load linked into blaze's own target directory. It links
    # none of them again.
    again = sorted(path for path, written in linked_for_loads(ws).items() if linked.get(path) != written)
    if again:
        row["wrong"].append(f"warm-up: the daemon built the build scripts and proc macros again: {', '.join(again[:3])}")
        print(f"  warm-up      the daemon built the build scripts and proc macros again: {', '.join(again[:3])}", flush=True)
    step("warm build", "build", build, status)
    runs("warm run")
    step("cold test", "test", test, test_results)
    warm("test warm-up")
    step("warm test", "test", test, test_results)
    for file, old, new in spec["edits"]:
        rewrite(os.path.join(ws, file), old, new)
    step("edit build", "build", build, status)
    runs("edit run")
    step("edit test", "test", test, test_results)
    path = os.path.join(ws, spec.get("broken") or spec["edits"][0][0])
    edited = open(path).read()
    open(path, "w").write(edited + '\nfn blaze_type_error() -> u32 {\n    "not a number"\n}\n')
    # blaze's error is its own, of what rust-analyzer finds: only the status is rustc's.
    step("broken", "build", build, lambda ran: ran.returncode)
    open(path, "w").write(edited)
    step("mended", "build", build, status)
    runs("mended run")
    worktree(ours, row, build, [*build, "--", *run] if not ask else None, spec["edits"], work)
    row["share"] = ours.share()
    print(f"  functions    {row['share'][0]} of {row['share'][1]} compiled by blaze", flush=True)
    _, counted = ours.cargo("status")
    row["status"] = counted.stdout
    ours.cargo("stop")
    return row


FLAG_WORKSPACE = {
    "Cargo.toml": '[workspace]\nmembers = ["app", "util", "shout"]\nresolver = "2"\n\n[profile.quick]\ninherits = "dev"\n\n[profile.release]\npanic = "abort"\n\n[profile.quick.package.util]\nopt-level = 3\noverflow-checks = false\n',
    "util/Cargo.toml": '[package]\nname = "util"\nversion = "0.1.0"\nedition = "2021"\n\n[features]\nloud = []\n',
    "util/src/lib.rs": '/// ```\n/// assert_eq!(util::greeting("it").len(), 8);\n/// ```\n///\n/// ```should_panic\n/// assert_eq!(util::greeting("it"), "");\n/// ```\npub fn greeting(name: &str) -> String {\n    let text = format!("hello {name}");\n    if cfg!(feature = "loud") { text.to_uppercase() } else { text }\n}\n\n/// ```\n/// assert_eq!(util::WORDS.len(), 1);\n/// ```\npub const WORDS: &[&str] = &["hello"];\n\nthread_local! {\n    static SAID: std::cell::RefCell<Option<String>> = None.into();\n}\n\npub fn said() -> String {\n    SAID.with_borrow(|it| it.clone().unwrap_or_default())\n}\n\n/// What a word count is laid out with: `loud` puts a volume before it.\n#[repr(C)]\npub struct Words {\n    #[cfg(feature = "loud")]\n    pub volume: u64,\n    pub count: u64,\n}\n\npub fn words(count: u64) -> Words {\n    Words {\n        #[cfg(feature = "loud")]\n        volume: 11,\n        count,\n    }\n}\n\n#[cfg(test)]\nmod tests {\n    // A test and the library see one thread local, whoever compiled either.\n    #[test]\n    fn says_what_a_test_set() {\n        super::SAID.with_borrow_mut(|it| *it = Some("set".to_string()));\n        assert_eq!(super::said(), "set");\n    }\n\n    #[test]\n    fn greets() {\n        assert!(super::greeting("a").to_lowercase().starts_with("hello"));\n    }\n}\n',
    "app/Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\ndefault-run = "app"\n\n[features]\nloud = ["util/loud"]\n\n[dependencies]\nutil = { path = "../util" }\n\n[dev-dependencies]\nshout = { path = "../shout" }\n',
    "shout/Cargo.toml": '[package]\nname = "shout"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nutil = { path = "../util" }\n',
    # Its examples link against util as cargo built it, with the features of every package the
    # build selects: `Words` has a volume when app's `loud` is on, whichever package is tested.
    "shout/src/lib.rs": 'pub fn shout(text: &str) -> String {\n    text.to_uppercase()\n}\n\n/// ```\n/// assert_eq!(shout::counted(3), 3);\n/// ```\npub fn counted(count: u64) -> u64 {\n    util::words(count).count\n}\n',
    "app/src/main.rs": '#![allow(unexpected_cfgs)]\n\nfn main() {\n    let name = std::env::args().nth(1).unwrap_or("world".to_string());\n    println!("{}", util::greeting(&name));\n    println!("{} {} {}", cfg!(debug_assertions), cfg!(extra), cfg!(target_feature = "avx2"));\n    let sum = std::panic::catch_unwind(|| name.len() as u8 + 251);\n    println!("{:?}", sum.ok());\n    if name == "fail" {\n        std::process::exit(3);\n    }\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn in_the_program() {\n        assert_eq!(util::greeting("x").len(), 7);\n    }\n}\n',
    "app/src/bin/other.rs": 'fn main() {\n    println!("other: {}", util::greeting("other"));\n}\n',
    "app/examples/demo.rs": 'fn main() {\n    println!("demo: {} {}", util::greeting("demo"), shout::shout("demo"));\n}\n',
    "app/tests/it.rs": '#[test]\nfn greets_it() {\n    assert_eq!(util::greeting("it").to_lowercase(), "hello it");\n}\n\n#[test]\nfn fails_when_asked() {\n    assert!(std::env::var("FLAGS_FAIL").is_err());\n}\n\n// A cargo inside writes the programs again, and on macOS it removes each and copies it back: a\n// test starting one meanwhile finds none.\nstatic BUILDING: std::sync::Mutex<()> = std::sync::Mutex::new(());\n\n// A cargo run by a test builds, and leaves a program that runs: after a build by cargo it has\n// nothing to do, after a whole build, which leaves cargo no units, it builds the program again in\n// its place (as the check of the daemon of a program that runs cargo has it).\n#[test]\nfn a_cargo_inside_leaves_the_program() {\n    let _building = BUILDING.lock().unwrap_or_else(|it| it.into_inner());\n    let cargo = std::env::var("CARGO").unwrap_or("cargo".to_string());\n    let built = std::process::Command::new(cargo).args(["build", "-q", "-p", "app"]).status().unwrap();\n    assert!(built.success());\n    let ran = std::process::Command::new(env!("CARGO_BIN_EXE_app")).arg("it").output().unwrap();\n    assert!(String::from_utf8_lossy(&ran.stdout).starts_with("hello it"));\n}\n\n// What cargo tells a test of the places of the build is what the daemon tells it.\n#[test]\nfn the_places_of_the_build_are_cargos() {\n    assert!(std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).is_dir());\n    let _building = BUILDING.lock().unwrap_or_else(|it| it.into_inner());\n    let ran = std::process::Command::new(env!("CARGO_BIN_EXE_other")).output().unwrap();\n    assert_eq!(String::from_utf8_lossy(&ran.stdout).lines().count(), 1);\n}\n',
}

# What the edit round adds to the program: library code that rustc's build before the edit has
# no copy of, so the daemon compiles it. On AArch64 `spin_loop` is the `isb` intrinsic, the
# read guard's drop is the queue-based lock's `ptr_mask` on macOS, and `to_int_unchecked` is
# `float_to_int_unchecked`.
EDITED_IN_CALL = '    println!("{:?}", edited_in(name.len()));\n'
EDITED_IN = """fn edited_in(n: usize) -> (u64, (i8, u64), usize) {
    let lock = std::sync::RwLock::new(n as u64);
    let read = *lock.read().unwrap();
    let mut turns = 0;
    while turns < n {
        std::hint::spin_loop();
        turns += 1;
    }
    let half = n as f64 / 2.0 + 0.5;
    // SAFETY: half the length of a name is in range of both.
    let cast = unsafe { (half.to_int_unchecked(), half.to_int_unchecked()) };
    (read, cast, turns)
}

"""

# Each: the subcommand, its arguments, the variables it runs with.
FLAG_CASES = [
    ("run", [], {}),
    ("run", ["--", "fail"], {}),
    ("run", ["--release"], {}),
    ("run", ["--release", "--", "fail"], {}),
    ("run", ["--", "world"], {"RUSTFLAGS": "-C overflow-checks=off"}),
    # A processor of x86-64 is one only there.
    *([("run", ["--release"], {"RUSTFLAGS": "-C target-cpu=x86-64-v3"})] if host().startswith("x86_64") else []),
    ("run", ["--no-default-features", "-p", "app"], {}),
    # The host named: what is built for is the same, where it is put is not.
    ("run", ["--target", host()], {}),
    ("run", ["-p", "app"], {}),
    ("run", ["--features", "loud", "-p", "app"], {}),
    ("run", ["--bin", "other"], {}),
    ("run", ["--example", "demo"], {}),
    ("run", ["--manifest-path", "app/Cargo.toml"], {}),
    ("run", ["--profile", "quick"], {}),
    ("run", ["-q", "name"], {}),
    ("run", [], {"RUSTFLAGS": "--cfg extra"}),
    ("build", ["--workspace"], {}),
    ("build", ["--all-features"], {}),
    ("check", [], {}),
    ("test", [], {}),
    ("test", ["--test", "it"], {}),
    ("test", ["-p", "util", "--lib"], {}),
    ("test", ["greets"], {}),
    ("test", ["--no-fail-fast"], {"FLAGS_FAIL": "1"}),
    ("test", [], {"FLAGS_FAIL": "1"}),
    ("test", ["--no-run"], {}),
    ("test", ["--doc"], {}),
    ("test", ["--doc", "--features", "loud", "-p", "util"], {}),
    ("test", ["--doc", "--workspace", "--features", "app/loud"], {}),
    # Flags of how the results are shown are the harness's: blaze runs the doctests all the
    # same, and shows them as libtest does.
    ("test", ["--doc", "-q"], {}),
    ("test", ["--doc", "--", "--format=terse", "--color", "never"], {}),
    ("test", [], {"CARGO_INCREMENTAL": "0"}),
]


# What blaze fails and cargo builds, known and not mended: an edit to a crate whose profile
# differs from the program's in `overflow-checks` (`[profile.quick.package.util]`).
FLAGS_OPEN = {("edit", "blaze run --profile quick")}


def flags(work):
    """Every flag case on both sides, cold, warm and after an edit. The result is the rows
    and what differed."""
    ws = os.path.realpath(os.path.join(work, "flags"))
    for file, text in FLAG_WORKSPACE.items():
        os.makedirs(os.path.dirname(os.path.join(ws, file)), exist_ok=True)
        open(os.path.join(ws, file), "w").write(text)
    env = base_env(os.path.join(work, "home-flags"))
    ours, theirs = Side(ws, env, True), Side(ws, env, False)
    rows, wrong = [], []

    def round_of(label):
        for command, args, extra in FLAG_CASES:
            (_, mine), (_, other) = ours.cargo(command, *args, env=extra), theirs.cargo(command, *args, env=extra)
            read = test_results if command == "test" else program_output
            differs = difference(read(mine), read(other))
            if "--doc" in args:
                left = re.search(r"blaze: the doctests are left to rustdoc: (.*)", mine.stderr)
                # A panic message has the id of the thread, and where the terse progress
                # (` 1/2`) and blank lines fall depends on which test finished first.
                text = lambda ran: re.sub(r"finished in [0-9.]+s", "", re.sub(r"' \(\d+\) panicked", "' panicked", ran.stdout))
                said = lambda ran: sorted(line for line in re.sub(r" \d+/\d+$", "", text(ran), flags=re.M).splitlines() if line)
                differs = differs or (left and f"left to rustdoc: {left[1]}") or difference(said(mine), said(other))
            if differs and mine.returncode != other.returncode:
                differs += f"; {mine.stderr.strip()[-600:]!r}"
            shown = " ".join([*(f"{k}={v}" for k, v in extra.items()), "blaze", command, *args])
            rows.append((label, shown, how_built(mine.stderr), mine.returncode, differs or "same"))
            if differs and (label, shown) in FLAGS_OPEN:
                differs = "open: " + differs
            elif differs:
                wrong.append(f"{label}: {shown}: {differs}")
            print(f"  {label:6} {shown:50} exit {mine.returncode:3}  {how_built(mine.stderr):8.60}  {differs or 'same'}", flush=True)

    round_of("cold")
    wait_warm(ours, ws)
    round_of("warm")
    rewrite(os.path.join(ws, "util/src/lib.rs"), 'format!("hello {name}")', 'format!("hello {name}!")')
    # rustc's program of an example has the constant in it: the one of the warm round is stale.
    rewrite(os.path.join(ws, "util/src/lib.rs"), '&["hello"]', '&["hello", "there"]')
    rewrite(os.path.join(ws, "app/src/main.rs"), ".len(), 7)", ".len(), 8)")
    rewrite(os.path.join(ws, "app/src/main.rs"), "    if name ==", EDITED_IN_CALL + "    if name ==")
    rewrite(os.path.join(ws, "app/src/main.rs"), "#[cfg(test)]", EDITED_IN + "#[cfg(test)]")
    rewrite(os.path.join(ws, "app/tests/it.rs"), '"hello it"', '"hello it!"')
    round_of("edit")
    _, counted = ours.cargo("status")
    ours.cargo("stop")
    return rows, wrong, counted.stdout


def daemon(work):
    """The life of the daemon. The result is each check with whether it held."""
    ws = os.path.realpath(os.path.join(work, "life"))
    for file, text in FLAG_WORKSPACE.items():
        os.makedirs(os.path.dirname(os.path.join(ws, file)), exist_ok=True)
        open(os.path.join(ws, file), "w").write(text)
    # A file that is no text, beside the sources: the daemon loads the workspace all the same.
    open(os.path.join(ws, "util/src/not-text.bin"), "wb").write(b"\xff\xfe\x00\x80")
    with open(os.path.join(ws, "app/src/bin/nested.rs"), "w") as nested:
        nested.write('fn main() {\n    let cargo = std::process::Command::new("cargo").arg("build").status();\n    std::process::exit(cargo.ok().and_then(|it| it.code()).unwrap_or(1));\n}\n')
    git = lambda *args, cwd=ws: subprocess.run(["git", "-c", "user.name=blaze", "-c", "user.email=blaze@localhost", *args], cwd=cwd, check=True, capture_output=True)
    open(os.path.join(ws, ".gitignore"), "w").write("target\ntarget-ref\n")
    git("init", "-q"), git("add", "."), git("commit", "-qm", "first")
    tree = os.path.realpath(os.path.join(work, "life-worktree"))
    git("worktree", "add", "-q", tree)
    home = os.path.join(work, "home-life")
    env = base_env(home)
    here, there = Side(ws, env, True), Side(tree, env, True)
    checks = []

    def check(name, held, detail=""):
        checks.append((name, bool(held), detail))
        print(f"  {'ok ' if held else 'NOT'} {name}  {detail}", flush=True)

    def status(side=None):
        text = (side or here).cargo("status")[1].stdout
        pid = re.search(r"^daemon: pid (\d+)", text, re.M)
        return (int(pid[1]) if pid else None), [int(p) for p in re.findall(r"worker: pid (\d+)", text)], text

    edit = lambda root, text: open(os.path.join(root, "app/src/main.rs"), "a").write(f"\n// {text}\n")

    check("no daemon before the first build", status()[0] is None)
    _, first = here.cargo("build")
    pid, workers, _ = status()
    check("the first build starts one", first.returncode == 0 and pid is not None, f"pid {pid}")
    # No cargo builds first: the build is the daemon's from the start, with nothing to wait
    # for after cargo.
    check("and the first build's program is the daemon's, with no cargo", how_built(first.stderr) == "daemon" and not by_rustc(first.stderr), how_built(first.stderr))
    wait_warm(here, ws)
    # A program that runs plain `cargo build` on its own checkout, as a test suite may, after
    # the daemon built it: cargo's own artifacts are behind the sources, so cargo builds
    # them again and its executable takes the place of the daemon's. Both are right.
    edit(ws, "nested")
    _, installed = here.cargo("build")
    _, nested = here.cargo("run", "--bin", "nested")
    _, after = here.cargo("run", "--bin", "app")
    _, reference = timed(["cargo", "run", "--bin", "app"], ws, dict(env, CARGO_TARGET_DIR=os.path.join(ws, "target-ref")))
    check("a program that runs cargo on its checkout builds, and the next run is right", how_built(installed.stderr) == "daemon" and nested.returncode == 0 and after.returncode == reference.returncode and after.stdout == reference.stdout, f"{how_built(installed.stderr)}, nested {nested.returncode}, then {how_built(after.stderr)}")
    edit(ws, "one")
    _, second = here.cargo("build")
    check("a later build is the daemon's, by the same daemon", how_built(second.stderr) == "daemon" and status()[0] == pid, how_built(second.stderr))

    there.cargo("build")
    wait_warm(there, tree)
    _, workers_now, text = status()
    shared = re.search(rf"worker: pid \d+,[^\n]*\n(    [^\n]*\n)*    {re.escape(tree)}: ready", text)
    check("a worktree is loaded by the same daemon and the same worker", status()[0] == pid and len(workers_now) == 1 and shared, f"workers {workers_now}")
    edit(tree, "two")
    _, in_tree = there.cargo("run")
    check("a build in the worktree is the daemon's", how_built(in_tree.stderr) == "daemon" and in_tree.returncode == 0, how_built(in_tree.stderr))

    edit(ws, "three")
    client = subprocess.Popen(["blaze", "build"], cwd=ws, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, **OWN_GROUP)
    time.sleep(0.01)
    kill_all(client)
    client.wait()
    edit(ws, "four")
    _, after_kill = here.cargo("run")
    check("the daemon outlives a client that is killed", status()[0] == pid and after_kill.returncode == 0 and "hello world" in after_kill.stdout, how_built(after_kill.stderr))

    other = another_build(os.path.join(work, "other-build"))
    newer = Side(ws, dict(env, PATH=other + os.pathsep + env["PATH"]), True)
    edit(ws, "five")
    _, by_newer = newer.cargo("run")
    replaced = status(newer)[0]
    ran = by_newer.returncode == 0 and "hello world" in by_newer.stdout
    check("a client of another build replaces the daemon", ran and replaced not in (None, pid) and not alive(pid), f"pid {pid} then {replaced}")
    check("the workers of the daemon it replaced are gone", not any(alive(p) for p in workers_now), f"{workers_now}")
    newer.cargo("stop")

    quick = Side(ws, dict(env, BLAZE_IDLE_SECS="3"), True)
    quick.cargo("build")
    pid, _, _ = status()
    wait_warm(quick, ws)
    _, workers, _ = status()
    time.sleep(6)
    gone = not alive(pid) and not any(alive(p) for p in workers)
    check("idle for BLAZE_IDLE_SECS, the daemon and its workers exit", pid is not None and workers and gone, f"daemon {pid}, workers {workers}")

    here.cargo("build")
    wait_warm(here, ws)
    pid, workers, _ = status()
    kill(pid)
    time.sleep(1)
    check("workers exit when the daemon is killed", workers and not any(alive(p) for p in workers), f"workers {workers}")
    edit(ws, "six")
    _, again = here.cargo("run")
    check("the build after that starts a daemon again and succeeds", again.returncode == 0 and "hello world" in again.stdout and status()[0] not in (None, pid))

    # An example has the dev-dependencies, as cargo builds it: its edit is one the daemon
    # builds, not one rust-analyzer finds names missing from.
    demo = os.path.join(ws, "app/examples/demo.rs")
    here.cargo("run", "--example", "demo")
    wait_warm(here, ws)
    open(demo, "a").write("\n// edited\n")
    _, example = here.cargo("run", "--example", "demo")
    check("an edited example with a dev-dependency is the daemon's", how_built(example.stderr) == "daemon" and example.returncode == 0 and " DEMO" in example.stdout, f"{how_built(example.stderr)}: {example.stdout.strip()[-80:]}")
    # An error is blaze's, written as rustc and cargo write it, and no rustc runs to say it.
    main = os.path.join(ws, "app/src/main.rs")
    kept = open(main).read()
    open(main, "a").write("\nfn broken() -> u32 { \"text\" }\n")
    _, broken = here.cargo("build")
    said = re.search(r'^error\[E0308\]: .*\n +--> app/src/main\.rs:\d+:22\n', broken.stderr, re.M)
    ended = 'error: could not compile `app` (bin "app") due to 1 previous error' in broken.stderr
    check("an edit with an error fails the build with blaze's error, as rustc writes it, and no cargo", broken.returncode == 101 and not by_rustc(broken.stderr) and said and ended, f"exit {broken.returncode}, {broken.stderr.strip()[-300:]}")
    open(main, "w").write(kept)
    # So does one written while no daemon ran, which the worker finds as it loads.
    here.cargo("stop")
    open(main, "a").write("\nfn broken() -> u32 { \"text\" }\n")
    _, broken = here.cargo("build")
    said = re.search(r'^error\[E0308\]: .*\n +--> app/src/main\.rs:\d+:22\n', broken.stderr, re.M)
    ended = 'error: could not compile `app` (bin "app") due to 1 previous error' in broken.stderr
    check("an error written before the daemon loaded fails the build with blaze's error, and no cargo", broken.returncode == 101 and not by_rustc(broken.stderr) and said and ended, f"exit {broken.returncode}, {broken.stderr.strip()[-300:]}")
    open(main, "w").write(kept)
    _, mended = here.cargo("run")
    check("the build after it is mended is the daemon's", mended.returncode == 0 and not by_rustc(mended.stderr) and "hello world" in mended.stdout, f"exit {mended.returncode}, {mended.stderr.strip()[-300:]}")
    # The worker a first build starts loads the checkout with the places cargo set, which an
    # integration test names its program by: `env!("CARGO_BIN_EXE_app")` is set as it loads.
    here.cargo("stop")
    _, cold = here.cargo("test", "--test", "it")
    unset = "names a variable that is not set" in cold.stderr
    check("a first build that is an integration test naming a program builds it", not unset and re.search(r"^test greets_it \.\.\. ok$", cold.stdout, re.M), f"exit {cold.returncode}, {cold.stderr.strip()[-300:]!r}")
    # A lint the source denies fails the build as rustc's would, unless the flags cap lints.
    wait_warm(here, ws)
    open(main, "a").write("\n#[deny(non_snake_case)]\npub fn Denied() {}\n")
    _, denied = here.cargo("build")
    said = re.search(r'^error: .*`Denied`.*\n +--> app/src/main\.rs:\d+:8\n', denied.stderr, re.M)
    check("an edit with a lint its source denies fails the build with blaze's error, and no cargo", denied.returncode == 101 and not by_rustc(denied.stderr) and said, f"exit {denied.returncode}, {denied.stderr.strip()[-300:]}")
    open(main, "w").write(kept)
    wait_warm(here, ws)
    open(main, "a").write("\n#[deny(elided_lifetimes_in_paths)]\npub fn hidden(_: std::fmt::Arguments) {}\n")
    _, denied = here.cargo("build")
    said = re.search(r'^error: hidden lifetime parameters in types are deprecated\n +--> app/src/main\.rs:\d+:28\n', denied.stderr, re.M)
    check("and so does one with a lint of a type's hidden lifetime it denies", denied.returncode == 101 and not by_rustc(denied.stderr) and said, f"exit {denied.returncode}, {denied.stderr.strip()[-300:]}")
    capped = {"RUSTFLAGS": "--cap-lints warn"}
    here.cargo("build", env=capped)
    wait_warm(here, ws)
    open(main, "a").write("\n// edited\n")
    _, allowed = here.cargo("run", env=capped)
    check("with `--cap-lints` among the flags the build of that edit is the daemon's", allowed.returncode == 0 and how_built(allowed.stderr) == "daemon" and "hello world" in allowed.stdout, f"exit {allowed.returncode}, {how_built(allowed.stderr)}: {allowed.stderr.strip()[-300:]}")
    open(main, "w").write(kept)
    # `blaze check` is what the daemon's build says, with no rustc.
    _, checked = here.cargo("check")
    check("`blaze check` of a loaded build is the daemon's, and no rustc", checked.returncode == 0 and not by_rustc(checked.stderr) and "[blaze]" in checked.stderr, f"exit {checked.returncode}, {checked.stderr.strip()[-300:]}")
    open(main, "a").write("\nfn broken() -> u32 { \"text\" }\n")
    _, failed = here.cargo("check")
    said = re.search(r'^error\[E0308\]: .*\n +--> app/src/main\.rs:\d+:22\n', failed.stderr, re.M)
    check("`blaze check` of an edit with an error fails with blaze's error, and no rustc", failed.returncode == 101 and not by_rustc(failed.stderr) and said, f"exit {failed.returncode}, {failed.stderr.strip()[-300:]}")
    open(main, "w").write(kept)
    # With no build of its flags loaded, the check is the daemon's checker's: no `cargo check`.
    # cargo's line names a package and its version (`Checking app v0.1.0 (...)`); blaze's own
    # step is `Checking 7 crates`.
    cargo_checked = lambda stderr: by_rustc(stderr) or bool(re.search(r"^\s*Checking \S+ v\d|this is `cargo check`", stderr, re.M))
    here.cargo("stop")
    _, unloaded = here.cargo("check")
    check("`blaze check` with no build loaded is the daemon's, and no rustc", unloaded.returncode == 0 and not cargo_checked(unloaded.stderr) and "[blaze]" in unloaded.stderr, f"exit {unloaded.returncode}, {unloaded.stderr.strip()[-300:]}")
    open(main, "a").write("\nfn broken() -> u32 { \"text\" }\n")
    _, failed = here.cargo("check")
    said = re.search(r'^error\[E0308\]: .*\n +--> app/src/main\.rs:\d+:22\n', failed.stderr, re.M)
    check("and of an edit with an error, blaze's error", failed.returncode == 101 and not cargo_checked(failed.stderr) and said, f"exit {failed.returncode}, {failed.stderr.strip()[-300:]}")
    open(main, "w").write(kept)
    _, mended = here.cargo("check")
    check("and once it is mended, no error", mended.returncode == 0 and not cargo_checked(mended.stderr), f"exit {mended.returncode}, {mended.stderr.strip()[-300:]}")
    # A module in a file the checker did not load is loaded with the workspace again.
    added = os.path.join(ws, "app/src/added.rs")
    open(added, "w").write("pub fn added() -> u32 { \"no\" }\n")
    open(main, "a").write("\nmod added;\nfn more() -> u32 { added::added() }\n")
    _, moduled = here.cargo("check")
    said = re.search(r'^error\[E0308\]: .*\n +--> app/src/added\.rs:1:25\n', moduled.stderr, re.M)
    check("and of a new module with an error, its error", moduled.returncode == 101 and not cargo_checked(moduled.stderr) and said, f"exit {moduled.returncode}, {moduled.stderr.strip()[-300:]}")
    open(added, "w").write("pub fn added() -> u32 { 1 }\n")
    _, moduled = here.cargo("check")
    check("and once that is mended, no error", moduled.returncode == 0 and not cargo_checked(moduled.stderr), f"exit {moduled.returncode}, {moduled.stderr.strip()[-300:]}")
    open(main, "w").write(kept)
    os.remove(added)
    _, other = here.cargo("check", "--release")
    check("`blaze check` of flags no build had is the daemon's, and no rustc", other.returncode == 0 and not cargo_checked(other.stderr) and "`release` profile [blaze]" in other.stderr, f"exit {other.returncode}, {other.stderr.strip()[-300:]}")
    here.cargo("stop")
    manners(work, ws, env, here, check, edit)
    abuse(work, ws, tree, env, check, status, alive)
    in_place(work, check)
    cleaned(work, check)
    through_link(work, check)
    script_links_proc_macro(work, check)
    pinned(work, check)
    own_toolchain(work, check)
    return checks


def fake_toolchain(dir, version):
    """A toolchain a project's rust-toolchain file names by its path, whose rustc says it is
    `version` and compiles nothing, so that a build with it fails. Its cargo is the real one,
    copied: cargo names itself to what it runs by where it is."""
    os.makedirs(os.path.join(dir, "bin"))
    cargo = subprocess.run(["rustup", "which", "--toolchain", TOOLCHAIN, "cargo"], capture_output=True, text=True, check=True).stdout.strip()
    shutil.copy(cargo, os.path.join(dir, "bin", "cargo"))
    rustc = os.path.join(dir, "bin", "rustc")
    with open(rustc, "w") as script:
        script.write(f'#!/bin/sh\ncase "$1" in --version|-V) echo "rustc {version} (0000000 2026-01-01)"; exit 0;; esac\n'
                     'echo "error: the toolchain the project pins was asked to compile: $*" >&2\nexit 1\n')
    os.chmod(rustc, 0o755)


def pinned(work, check):
    """Projects whose rust-toolchain file names another toolchain than blaze's release (#269):
    an older stable and a nightly. `blaze` builds them with its own release, and their
    edits are the daemon's; a nightly's `#![feature]` builds, an error in it fails, the
    project's toolchain is said only under `-v`, and no build is cargo's."""
    if WINDOWS or not shutil.which("rustup"):
        return
    env = base_env(os.path.join(work, "home-pinned"))
    # rustup takes the variable over the project's file.
    env.pop("RUSTUP_TOOLCHAIN")

    def project(name, version, main):
        ws = os.path.join(work, f"pinned-{name}")
        fake_toolchain(os.path.join(ws, "toolchain"), version)
        os.makedirs(os.path.join(ws, "src"))
        open(os.path.join(ws, "Cargo.toml"), "w").write(f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n')
        open(os.path.join(ws, "rust-toolchain.toml"), "w").write(f'[toolchain]\npath = "{os.path.join(ws, "toolchain")}"\n')
        open(os.path.join(ws, "src/main.rs"), "w").write(main)
        return Side(ws, env, True)

    plain = 'fn main() {\n    println!("pinned {}", 6 * 7);\n}\n'
    for label, name, version in (("an older stable", "older", "1.98.1"), ("a nightly", "nightly", "1.101.0-nightly")):
        side = project(name, version, plain)
        _, cold = side.cargo("run")
        check(f"a project pinned to {label} ({version}) builds with blaze's release", cold.returncode == 0 and "pinned 42" in cold.stdout, f"exit {cold.returncode}, {cold.stderr.strip()[-300:]!r}")
        wait_warm(side, side.ws)
        rewrite(os.path.join(side.ws, "src/main.rs"), "6 * 7", "6 * 8")
        _, edited = side.cargo("run")
        how = how_built(edited.stderr)
        check("and its edit is the daemon's", edited.returncode == 0 and how == "daemon" and "pinned 48" in edited.stdout, f"{how[:90]}; {edited.stderr.strip()[-300:]!r}")
    # blaze's toolchain is a nightly (docs/toolchain.md): a nightly's `#![feature]` builds.
    # The project's rust-toolchain file is not read, and said only under `-v` (#837).
    side = project("featured", "1.101.0-nightly", "#![feature(never_type)]\nfn main() {\n    let _: Option<!> = None;\n}\n")
    named = lambda stderr: "0000000 2026-01-01" in stderr or "rust-toolchain" in stderr
    _, featured = side.cargo("build")
    check("a nightly's `#![feature]` builds with blaze's own, nothing said of the project's toolchain, and no cargo", featured.returncode == 0 and blazes(featured.stderr) and not named(featured.stderr) and not by_rustc(featured.stderr), f"exit {featured.returncode}, {featured.stderr.strip()[-400:]!r}")
    _, verbose = side.cargo("build", "-v")
    ignored = verbose.stderr.count("rust-toolchain.toml is ignored: blaze builds with its own Rust")
    check("and under `-v` one line says its rust-toolchain file is ignored", verbose.returncode == 0 and ignored == 1 and not by_rustc(verbose.stderr), f"exit {verbose.returncode}, {ignored} lines, {verbose.stderr.strip()[-400:]!r}")
    open(os.path.join(side.ws, "src/main.rs"), "a").write("\nfn broken() -> u32 { \"text\" }\n")
    _, failed = side.cargo("build")
    check("and an error in it fails with the error, nothing said of the project's toolchain, and no cargo", failed.returncode == 101 and "error[E0308]" in failed.stderr and not named(failed.stderr) and not by_rustc(failed.stderr), f"exit {failed.returncode}, {failed.stderr.strip()[-400:]!r}")
    side.cargo("stop")


class Bare(Side):
    """`blaze` itself, where there is no cargo to run `blaze`: the same commands."""

    def argv(self, command, *args):
        return [os.path.join(os.path.dirname(BLAZE), "blaze" + EXE), command, *args]


def own_toolchain(work, check):
    """A machine with no rustup and no Rust on the path (docs/self-contained.md): blaze builds
    with its own copy of its release, laid out in `BLAZE_HOME` as rustup lays one out (here a
    copy of rustup's, so nothing is downloaded), runs cargo and rustc from it, and the edit
    after the first build is the daemon's."""
    sysroot = subprocess.run(["rustup", "run", TOOLCHAIN, "rustc", "--print", "sysroot"], capture_output=True, text=True).stdout.strip() if shutil.which("rustup") else ""
    if WINDOWS or not os.path.exists(os.path.join(sysroot, "lib/rustlib/multirust-channel-manifest.toml")):
        return
    # Blaze's own copy has every component it pins (harness/toolchain_pins.py), wasm32-wasip1's
    # standard library among them; a copy without one is downloaded again.
    if not os.path.isdir(os.path.join(sysroot, "lib/rustlib/wasm32-wasip1")):
        subprocess.run(["rustup", "target", "add", "--toolchain", TOOLCHAIN, "wasm32-wasip1"], capture_output=True)
    home = os.path.join(work, "home-own")
    os.makedirs(home, mode=0o700)
    host = subprocess.run([os.path.join(sysroot, "bin", "rustc"), "-vV"], capture_output=True, text=True).stdout.split("host: ")[1].split()[0]
    own = os.path.join(home, "toolchains", f"{TOOLCHAIN}-{host}")

    def link(source, target):
        try:
            os.link(source, target)
        except OSError:
            shutil.copy2(source, target)
    shutil.copytree(sysroot, own, symlinks=True, copy_function=link)
    env = base_env(home)
    for name in ("RUSTUP_TOOLCHAIN", "RUSTC", "RUSTDOC", "CARGO"):
        env.pop(name, None)
    # BLAZE_HOME is the daemon's directory too.
    env["BLAZE_HOME"] = home
    env["RUSTUP_HOME"] = os.path.join(work, "no-rustup")
    # A directory with Rust's programs in it is replaced by one with the rest of them: the C
    # compiler can be a script of the shell beside them (Nix).
    rusts = re.compile(r"^(cargo|rustc|rustdoc|rustup|rustfmt|rust-\w+|clippy-driver|cargo-.*)$")
    rid = os.path.join(work, "path-without-rust")
    os.makedirs(rid)
    dirs = []
    for d in env["PATH"].split(os.pathsep):
        if not d or not os.path.isdir(d):
            continue
        if not any(os.path.exists(os.path.join(d, it + EXE)) for it in ("cargo", "rustc", "rustup")):
            dirs.append(d)
            continue
        for name in os.listdir(d):
            if not rusts.match(name) and not os.path.lexists(os.path.join(rid, name)):
                os.symlink(os.path.join(d, name), os.path.join(rid, name))
    env["PATH"] = os.pathsep.join([os.path.dirname(BLAZE), *dirs, rid])

    ws = os.path.join(work, "own-toolchain")
    os.makedirs(os.path.join(ws, "src"))
    open(os.path.join(ws, "Cargo.toml"), "w").write('[package]\nname = "own"\nversion = "0.1.0"\nedition = "2021"\n')
    open(os.path.join(ws, "src/main.rs"), "w").write('fn main() {\n    println!("own {}", 6 * 7);\n}\n')
    side = Bare(ws, env, True)
    _, said = side.cargo("toolchain")
    check("with no rustup and no Rust on the path, blaze builds with its own copy of its release",
          said.returncode == 0 and said.stdout.strip().endswith(f", at {own}") and "rustup's copy" not in said.stdout and "Downloading" not in said.stderr,
          f"exit {said.returncode}, {said.stdout.strip()!r} {said.stderr.strip()[-300:]!r}")
    _, cold = side.cargo("run")
    check("and `blaze run` builds and runs the program with it", cold.returncode == 0 and "own 42" in cold.stdout,
          f"exit {cold.returncode}, {cold.stderr.strip()[-300:]!r}")
    if cold.returncode:
        side.cargo("stop")
        return
    wait_warm(side, ws)
    rewrite(os.path.join(ws, "src/main.rs"), "6 * 7", "6 * 8")
    _, edited = side.cargo("run")
    how = how_built(edited.stderr)
    check("and its edit is the daemon's", edited.returncode == 0 and how == "daemon" and "own 48" in edited.stdout,
          f"{how[:90]}; {edited.stderr.strip()[-300:]!r}")
    side.cargo("stop")


THROUGH_LINK = {
    "dep/Cargo.toml": '[package]\nname = "dep"\nversion = "0.1.0"\nedition = "2021"\n',
    "dep/src/lib.rs": "use std::sync::atomic::{AtomicUsize, Ordering};\n\npub fn bump() -> usize {\n    static N: AtomicUsize = AtomicUsize::new(0);\n    N.fetch_add(1, Ordering::Relaxed)\n}\n\npub fn twice() -> (usize, usize) {\n    (bump(), bump())\n}\n",
    "ws/Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\ndep = { path = "../dep" }\n',
    "ws/src/main.rs": 'fn main() {\n    println!("{:?}", (dep::bump(), dep::twice(), dep::bump()));\n}\n',
}


def through_link(work, check):
    """A workspace that `--manifest-path` names through a symbolic link, as macOS's /tmp is
    one to /private/tmp, with a path dependency outside it: cargo names the dependency's
    files through the link, the daemon its checkout by where it is. An edit is the daemon's,
    against what rustc built of the dependency (its static is counted by both)."""
    real = os.path.join(work, "through-link")
    for file, text in THROUGH_LINK.items():
        os.makedirs(os.path.dirname(os.path.join(real, file)), exist_ok=True)
        open(os.path.join(real, file), "w").write(text)
    link = real + "-link"
    os.symlink(real, link)
    manifest = ["--manifest-path", os.path.join(link, "ws", "Cargo.toml")]
    env = base_env(os.path.join(work, "home-through-link"))
    ours, theirs = Side(work, env, True), Side(work, env, False)
    ours.cargo("build", *manifest)
    began = time.time()
    while ": ready" not in ours.cargo("status", *manifest)[1].stdout and time.time() - began < WARMUP_SECS:
        time.sleep(0.25)
    rewrite(os.path.join(real, "ws/src/main.rs"), 'println!("{:?}"', 'println!("edited {:?}"')
    (_, mine), (_, other) = ours.cargo("run", *manifest), theirs.cargo("run", *manifest)
    wrong = difference(program_output(mine), program_output(other))
    how = how_built(mine.stderr)
    check("an edit of a workspace named through a link, with a path dependency outside it, is the daemon's and right",
          mine.returncode == 0 and how == "daemon" and not wrong, f"{how[:90]}; {wrong or 'same as cargo'}; {mine.stderr.strip()[-300:]!r}")
    ours.cargo("stop", *manifest)


# A build script whose build dependency is a library that links the compiler's `proc_macro`,
# as proc-macro2 is in bat's: the script links `proc_macro` too, whose functions then say no
# macro is running.
SCRIPT_PROC_MACRO = {
    "Cargo.toml": '[workspace]\nmembers = ["app", "tokens"]\nresolver = "2"\n',
    "app/Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n[build-dependencies]\ntokens = { path = "../tokens" }\n',
    "app/build.rs": 'fn main() {\n    println!("cargo:rustc-env=TOKENS={}", tokens::described());\n}\n',
    "app/src/main.rs": 'fn main() {\n    println!("{}", env!("TOKENS"));\n}\n',
    "tokens/Cargo.toml": '[package]\nname = "tokens"\nversion = "0.1.0"\nedition = "2021"\n',
    "tokens/src/lib.rs": 'extern crate proc_macro;\n\npub fn described() -> String {\n    format!("proc_macro available: {}", proc_macro::is_available())\n}\n',
}


def script_links_proc_macro(work, check):
    """A build script that links `proc_macro` through a library ([`SCRIPT_PROC_MACRO`]) is
    built by blaze, and the program runs as cargo's."""
    ws = os.path.realpath(os.path.join(work, "script-proc-macro"))
    for file, text in SCRIPT_PROC_MACRO.items():
        os.makedirs(os.path.dirname(os.path.join(ws, file)), exist_ok=True)
        open(os.path.join(ws, file), "w").write(text)
    env = base_env(os.path.join(work, "home-script-proc-macro"))
    ours, theirs = Side(ws, env, True), Side(ws, env, False)
    (_, mine), (_, other) = ours.cargo("run"), theirs.cargo("run")
    wrong = difference(program_output(mine), program_output(other))
    how = how_built(mine.stderr)
    check("a build script whose build dependency links `proc_macro` is the daemon's, and the program is right",
          mine.returncode == 0 and how == "daemon" and not wrong, f"{how[:90]}; {wrong or 'same as cargo'}; {mine.stderr.strip()[-300:]!r}")
    ours.cargo("stop")


def in_place(work, check):
    """On Linux the daemon writes a program it linked in full in place after it answered: the
    debug info, to where the splices write it. A program started while its file is open for
    writing is refused ("text file busy"), so whatever is given the program waits for the
    write. The daemon holds each write here for BLAZE_LAYOUT_PAUSE_MS (and says "laid out:
    paused", then "written") while a client is given the program each way a user is."""
    if not sys.platform.startswith("linux"):
        return
    env = dict(base_env(os.path.join(work, "home-in-place")), BLAZE_LAYOUT_PAUSE_MS="3000")

    def writes():
        """How many writes in place began and how many are over, in every worker's log."""
        logs = glob.glob(os.path.join(env["BLAZE_HOME"], "*", "*", "log"))
        said = "".join(open(log, errors="replace").read() for log in logs)
        return said.count("laid out: paused"), said.count("laid out: written")

    def still():
        """Waits until no write is under way: what the daemon does after a client has the program."""
        began = time.time()
        while writes()[0] != writes()[1] and time.time() - began < 60:
            time.sleep(0.1)

    def started(name, side, ran):
        """Has the daemon link the program of a checkout it has loaded in full, for an edit
        that adds a thread local, which a splice leaves to the linker, and `ran` be given it.
        The result is how the program ran, who built it, and the writes under way when the
        client had it."""
        for file, text in FLAG_WORKSPACE.items():
            os.makedirs(os.path.dirname(os.path.join(side.ws, file)), exist_ok=True)
            open(os.path.join(side.ws, file), "w").write(text)
        side.cargo("build")
        wait_warm(side, side.ws)
        still()
        side.how_built("")
        before = writes()[0]
        main = os.path.join(side.ws, "app/src/main.rs")
        rewrite(main, 'println!("{:?}", sum.ok());', 'println!("{:?} {}", sum.ok(), IN_PLACE.with(|it| it.get()));')
        open(main, "a").write("\nthread_local! {\n    static IN_PLACE: std::cell::Cell<u32> = const { std::cell::Cell::new(1) };\n}\n")
        out, how, under_way = ran(side)
        written = writes()[0] - before
        still()
        held = out.returncode == 0 and how == "daemon" and written > 0 and not under_way
        check(f"{name}, while the daemon writes it in place, starts", held,
              f"{how[:90]}; {written} written in place, {under_way} under way when the client had it; exit {out.returncode}; {out.stderr.strip()[-300:]!r}")

    def by_path(side):
        _, built = side.cargo("build")
        paused, written = writes()
        try:
            ran = subprocess.run([os.path.join(side.ws, "target/debug/app" + EXE), "in-place"], capture_output=True, text=True, timeout=60)
        except OSError as refused:
            ran = subprocess.CompletedProcess([], 126, "", f"could not start it: {refused}")
        return (ran if built.returncode == 0 else built), side.how_built(built.stderr), paused - written

    def by_run(side):
        _, ran = side.cargo("run", "in-place")
        return ran, side.how_built(ran.stderr), 0

    started("the program by its path after `blaze build`", Side(os.path.join(work, "in-place-build"), env, True), by_path)
    started("`blaze run`", Side(os.path.join(work, "in-place-run"), env, True), by_run)
    subprocess.run([BLAZE, "stop"], env=env, capture_output=True)


# A program with a build script whose output it includes: a clean takes that away too.
CLEANED = {
    "Cargo.toml": '[package]\nname = "cleaned"\nversion = "0.1.0"\nedition = "2021"\n',
    "build.rs": 'fn main() {\n    let out = std::env::var("OUT_DIR").unwrap();\n    std::fs::write(format!("{out}/said.rs"), "const SAID: &str = \\"from the script\\";").unwrap();\n}\n',
    "src/main.rs": 'include!(concat!(env!("OUT_DIR"), "/said.rs"));\n\nfn main() {\n    println!("hello {SAID}");\n}\n',
}


def cleaned(work, check):
    """A clean while the daemon runs (#721): `blaze clean`, `cargo clean` and `rm -rf
    target` take away what its worker wrote under the target directory, the standard
    library's unit among them, and tell it nothing. The next build is the same worker's,
    which writes again what is gone, and so is an edit after it."""
    ws = os.path.realpath(os.path.join(work, "cleaned"))
    for file, text in CLEANED.items():
        os.makedirs(os.path.dirname(os.path.join(ws, file)), exist_ok=True)
        open(os.path.join(ws, file), "w").write(text)
    env = base_env(os.path.join(work, "home-cleaned"))
    here = Side(ws, env, True)
    workers = lambda: re.findall(r"worker: pid (\d+)", here.cargo("status")[1].stdout)
    _, first = here.cargo("run")
    wait_warm(here, ws)
    before = workers()
    check("a program with a build script's output is the daemon's", first.returncode == 0 and how_built(first.stderr) == "daemon" and "hello from the script" in first.stdout, f"exit {first.returncode}, {first.stderr.strip()[-300:]!r}")
    cleans = [
        ("`blaze clean`", lambda: here.cargo("clean")),
        ("`cargo clean`", lambda: timed(["cargo", "clean"], ws, env)),
        ("`rm -rf target`", lambda: shutil.rmtree(os.path.join(ws, "target"))),
    ]
    for at, (name, clean) in enumerate(cleans):
        clean()
        _, again = here.cargo("run")
        held = again.returncode == 0 and how_built(again.stderr) == "daemon" and "hello from the script" in again.stdout and workers() == before
        check(f"a build after {name} while the daemon runs is the same worker's, and runs", held, f"exit {again.returncode}, workers {before} then {workers()}, {again.stderr.strip()[-300:]!r}")
        rewrite(os.path.join(ws, "src/main.rs"), "hello", f"edit {at}")
        _, edited = here.cargo("run")
        held = edited.returncode == 0 and how_built(edited.stderr) == "daemon" and f"edit {at} from the script" in edited.stdout
        check("and an edit after it", held, f"exit {edited.returncode}, {edited.stderr.strip()[-300:]!r}")
        rewrite(os.path.join(ws, "src/main.rs"), f"edit {at}", "hello")
    here.cargo("stop")


USE_AFTER_MOVE = """
fn taken(text: String) -> usize { text.len() }
pub fn moved() -> usize { let text = String::new(); taken(text) + text.len() }
"""


def manners(work, ws, env, here, check, edit):
    """What the commands say for themselves, a daemon that is going away, the store that is
    on without being asked for, and an error only rustc's borrow checker finds."""
    for argv in (["blaze-engine", "--help"], ["blaze-engine", "-h"], ["blaze", "--help"], ["blaze", "-h"]):
        ran = subprocess.run([BLAZE, *argv[1:]] if argv[0] == "blaze-engine" else argv, cwd=ws, env=env, capture_output=True, text=True)
        listed = all(word in ran.stdout for word in ("status", "stop"))
        check(f"`{' '.join(argv)}` lists the commands and exits 0", ran.returncode == 0 and listed, f"exit {ran.returncode}")

    here.cargo("build")
    wait_warm(here, ws)
    for argv in ([BLAZE, "status"], [BLAZE, "cache", "stats"], [BLAZE, "--help"], ["blaze", "status"], ["blaze", "--help"]):
        shown = subprocess.Popen(argv, cwd=ws, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        shown.stdout.close()
        said = shown.stderr.read()
        code = shown.wait()
        name = " ".join(["blaze" if argv[0] == BLAZE else argv[0], *argv[1:]])
        # Cargo's subcommand ends by the signal; `blaze` has no status to pass on and says 1.
        check(f"`{name}` whose reader went away ends without a word", not said.strip() and code in (0, 1, -signal.SIGPIPE), f"exit {code} {said.strip()[:120]}")

    left = []
    for turn in range(5):
        here.cargo("stop")
        edit(ws, f"stopped {turn}")
        _, after = here.cargo("run")
        left.append(after.returncode == 0 and "hello world" in after.stdout and "answer" not in after.stderr and "EOF" not in after.stderr)
        wait_warm(here, ws)
    check("a build straight after `stop` starts a daemon and is not a fallback over the one leaving", all(left), f"{sum(left)} of 5")
    here.cargo("stop")

    lib = os.path.join(ws, "util/src/lib.rs")
    main = os.path.join(ws, "app/src/main.rs")
    for file, name in ((main, "the program's crate"), (lib, "a library of the workspace")):
        kept = open(file).read()
        here.cargo("build")
        wait_warm(here, ws)
        open(file, "a").write(USE_AFTER_MOVE)
        _, wrong = here.cargo("run")
        # The daemon refuses an edit its borrow checker finds wrong, as one with a type error,
        # and says what is.
        check(f"a use after a move in {name} is the daemon's error on a warm `blaze run`", wrong.returncode == 101 and not by_rustc(wrong.stderr) and "error[E0382]" in wrong.stderr, f"exit {wrong.returncode}, {how_built(wrong.stderr)}")
        _, checked = here.cargo("check")
        check(f"and `blaze check` of it", checked.returncode == 101 and "E0382" in checked.stderr, f"exit {checked.returncode}")
        open(file, "w").write(kept)
        _, mended = here.cargo("run")
        check("and the build once it is mended is right", mended.returncode == 0 and "hello world" in mended.stdout, how_built(mended.stderr))
    here.cargo("stop")

    for value, kept in ((None, True), ("off", False), ("0", False), ("", False)):
        cache = os.path.join(work, f"cache-{value}")
        home = os.path.join(work, f"home-store-{value}")
        own = base_env(home)
        own.pop("BLAZE_CACHE")
        own["XDG_CACHE_HOME"] = cache
        if value is not None:
            own["BLAZE_CACHE"] = value
        side = Side(ws, own, True)
        edit(ws, f"store {value}")
        side.cargo("build")
        wait_warm(side, ws)
        store = os.path.join(cache, "blaze", "store")
        filled = os.path.isdir(store) and any(files for _, _, files in os.walk(store))
        stats = subprocess.run([BLAZE, "cache", "stats"], cwd=ws, env=own, capture_output=True, text=True)
        side.cargo("stop")
        asked = "unset" if value is None else repr(value)
        check(f"BLAZE_CACHE {asked}: the store is {'on, in the user cache directory' if kept else 'off'}", filled == kept and (stats.returncode == 0) == kept, f"`blaze-engine cache stats` exit {stats.returncode}")


def greets(out, greeting):
    """Whether the program of the workspace said `greeting` before what it says of its build."""
    return out.splitlines()[:1] == [greeting]


def finished(client, timeout):
    """What `client` said and its exit code, or, if it has not ended in `timeout` seconds,
    what it and the processes under it were doing, as said by `ps`, and exit code `None`."""
    try:
        return client.communicate(timeout=timeout), client.returncode
    except subprocess.TimeoutExpired:
        listed = subprocess.run(["ps", "-axo", "pgid=,pid=,ppid=,stat=,etime=,command="], capture_output=True, text=True).stdout
        doing = "\n".join(line for line in listed.splitlines() if line.split()[:1] == [str(client.pid)])
        kill_all(client)
        out, err = client.communicate()
        return (out, f"{err}\n[still running after {timeout}s]\n{doing}"), None


def abuse(work, ws, tree, env, check, status, alive):
    """The daemon under what it is not asked for politely: many clients at once, a client
    and a daemon killed while a build is asked for, and a client that is another user."""
    here = Side(ws, env, True)
    git = lambda *args: subprocess.run(["git", *args], cwd=ws, check=True, capture_output=True)
    trees = [ws, tree]
    for n in (2, 3):
        trees.append(os.path.realpath(os.path.join(work, f"life-worktree{n}")))
        git("worktree", "add", "-q", trees[-1])
    sides = [Side(root, env, True) for root in trees]
    for side in sides:
        side.cargo("build")
    for side in sides:
        wait_warm(side, side.ws)
    pid, workers, _ = status()
    spawn = lambda root, *args: subprocess.Popen(["blaze", *args], cwd=root, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, stdin=subprocess.DEVNULL, start_new_session=True)
    # Round after round, as a client's program is started while the others put theirs in
    # place, which on macOS lost one program in a few hundred. `app` is its package's
    # `default-run`: the program is blaze's to start, not cargo's.
    rounds = 8
    greeting = lambda n, r: "hello" if r < 0 else f"tree{n}" if r == rounds - 1 else f"tree{n}-{r}"
    wrong, served = [], 0
    for r in range(rounds):
        for n, root in enumerate(trees):
            rewrite(os.path.join(root, "util/src/lib.rs"), f'format!("{greeting(n, r - 1)} {{name}}")', f'format!("{greeting(n, r)} {{name}}")')
        clients = [(n, spawn(root, "run", f"c{i}")) for i in range(4) for n, root in enumerate(trees)]
        ended = [(n, i // 4, *finished(client, 300)) for i, (n, client) in enumerate(clients)]
        cargos = lambda err: re.search(r"^\s*Finished `\w+` profile \[(?!blaze|blaze)", err, re.M)
        served += sum(how_built(err) == "daemon" and not cargos(err) for _, _, (_, err), _ in ended)
        wrong += [f"; round {r} tree{n} c{i}: exit {code}, {out[-80:]!r} {err[-200 if code is not None else -2000:]!r}" for n, i, (out, err), code in ended if code != 0 or not greets(out, f"{greeting(n, r)} c{i}")]
    check(f"16 clients at once on 4 worktrees each run their own tree's program, {rounds} times over", not wrong and served == 16 * rounds and status()[0] == pid and status()[1] == workers, f"{16 * rounds - len(wrong)} of {16 * rounds} right, {served} by the daemon alone, {len(workers)} worker" + "".join(wrong))

    # A worker that is stopped holds the build a client asks for until it is continued.
    edit = lambda text: open(os.path.join(ws, "app/src/main.rs"), "a").write(f"\n// {text}\n")
    held = lambda: [os.kill(p, signal.SIGSTOP) for p in workers]
    freed = lambda: [os.kill(p, signal.SIGCONT) for p in workers if alive(p)]
    edit("held one")
    held()
    client = spawn(ws, "build")
    time.sleep(1)
    waits = client.poll() is None
    kill_all(client)
    client.wait()
    freed()
    _, after = here.cargo("run", "again")
    check("a client killed while its build is held leaves the daemon serving", waits and after.returncode == 0 and greets(after.stdout, "tree0 again") and status()[0] == pid and how_built(after.stderr) == "daemon", how_built(after.stderr))

    edit("held two")
    held()
    client = spawn(ws, "run", "killed")
    time.sleep(1)
    waits = client.poll() is None
    os.kill(pid, signal.SIGKILL)
    freed()
    out, err = client.communicate(timeout=600)
    said = how_built(err)
    check("the build whose daemon is killed is right, by a daemon started in its place", waits and client.returncode == 0 and greets(out, "tree0 killed") and said == "daemon" and status()[0] not in (None, pid), f"exit {client.returncode}, {said}, {err.strip()[-200:]}")
    time.sleep(1)
    check("the workers of the killed daemon are gone", not any(alive(p) for p in workers), f"{workers}")
    wait_warm(here, ws)
    edit("after")
    _, after = here.cargo("run", "after")
    check("the build after that is a new daemon's", after.returncode == 0 and greets(after.stdout, "tree0 after") and status()[0] not in (None, pid) and how_built(after.stderr) == "daemon", how_built(after.stderr))

    other = os.path.join(work, "other-build")
    older = Side(ws, dict(env, PATH=other + os.pathsep + env["PATH"]), True)
    pid = status()[0]
    seen = next((line for line in older.cargo("status")[1].stdout.splitlines() if line.startswith("daemon:")), "")
    check("status by another build says the daemon is another build, and leaves it", "another build of blaze" in seen and status()[0] == pid, seen)
    # With no cargo to build in its place (docs/no-rustc.md), the build replaces the daemon
    # and is the new one's.
    edit("skew")
    _, skewed = older.cargo("run", "skew")
    said, newer = how_built(skewed.stderr), status(older)[0]
    check("a build by another build of blaze replaces the daemon and is the new one's", skewed.returncode == 0 and greets(skewed.stdout, "tree0 skew") and said == "daemon" and not by_rustc(skewed.stderr) and newer not in (None, pid) and not alive(pid), f"{said}, pid {pid} then {newer}")
    wait_warm(older, ws)
    edit("back")
    _, back = here.cargo("run", "back")
    said, again = how_built(back.stderr), status()[0]
    check("and the first build of blaze replaces that one the same way", back.returncode == 0 and greets(back.stdout, "tree0 back") and said == "daemon" and not by_rustc(back.stderr) and again not in (None, newer) and not alive(newer), f"{said}, pid {newer} then {again}")
    wait_warm(here, ws)

    stranger(env["BLAZE_HOME"], check)
    here.cargo("stop")


# A daemon that turns the peer away closes the connection, which can come before the request
# is sent (a broken pipe) as well as after it.
CONNECT = "import socket, sys\ns = socket.socket(socket.AF_UNIX)\ns.settimeout(5)\ntry:\n    s.connect(sys.argv[1])\nexcept OSError as e:\n    print('connect:', e.strerror)\n    sys.exit()\ntry:\n    s.sendall(bytes.fromhex(sys.argv[2]) + b'\\n')\n    print('answer:', s.recv(65536).decode(errors='replace'))\nexcept OSError as e:\n    print('answer:', e)\n"


def cbor(value):
    """RFC 8949 section 4.2.1's deterministic CBOR of what a request to the daemon has: maps
    with their keys in the order of their bytes, text, integers and booleans."""
    def head(major, n):
        if n < 24:
            return bytes([major << 5 | n])
        for extra, size in ((24, 1), (25, 2), (26, 4), (27, 8)):
            if n < 1 << (8 * size):
                return bytes([major << 5 | extra]) + n.to_bytes(size, "big")
    if isinstance(value, bool):
        return b"\xf5" if value else b"\xf4"
    if isinstance(value, int):
        return head(0, value)
    if isinstance(value, str):
        return head(3, len(value.encode())) + value.encode()
    if isinstance(value, dict):
        items = sorted((cbor(key), cbor(it)) for key, it in value.items())
        return head(5, len(items)) + b"".join(key + it for key, it in items)
    raise TypeError(value)


# The daemon answers a message of another schema as one of another version (`Restart`).
SCHEMA = int(re.search(r"^pub const SCHEMA: u32 = (\d+);", open(os.path.join(ROOT, "crates/blaze-protocol/src/lib.rs")).read(), re.M)[1])


def framed(body):
    """A request as the daemon reads one: a frame of a `blaze_protocol::Message`."""
    item = cbor({"schema": SCHEMA, "id": 1, "body": body})
    return (len(item).to_bytes(4, "big") + item).hex()

LISTEN = "import os, socket, sys\ns = socket.socket(socket.AF_UNIX)\ns.bind(sys.argv[1])\nos.chmod(sys.argv[1], 0o666)\ns.listen()\nprint('listening', flush=True)\ns.settimeout(20)\ntry:\n    c, _ = s.accept()\n    c.settimeout(5)\n    print('heard:', len(c.recv(65536)))\nexcept OSError as e:\n    print('heard: nothing,', e)\n"


def stranger(home, check):
    """A second user at the daemon's socket: kept out by the directory and the socket's
    modes, and turned away by the daemon when those are opened to it."""
    name = "blaze-stranger"
    sudo = lambda *argv: subprocess.run(["sudo", "-n", *argv], capture_output=True, text=True)
    if sudo("true").returncode != 0:
        check("a second user is turned away (needs sudo, which is not here)", os.environ.get("BLAZE_NO_SUDO") == "1", "not run")
        return
    # macOS has no `useradd`, and has a user with nothing of its own.
    DARWIN = sys.platform == "darwin"
    if DARWIN:
        name, made = "nobody", sudo("-u", "nobody", "true")
    else:
        sudo("userdel", name)
        made = sudo("useradd", "--system", "--no-create-home", "--shell", "/usr/sbin/nologin", name)
    sock = os.path.join(home, "daemon.sock")
    version = re.search(r"version (\S+),", subprocess.run(["blaze", "status"], env=dict(os.environ, BLAZE_HOME=home, PATH=os.path.dirname(BLAZE) + os.pathsep + os.environ["PATH"]), capture_output=True, text=True).stdout)[1]
    request = framed({"version": version, "replaces": False, "request": "Status"})
    def knock(user):
        run = subprocess.run([*(["sudo", "-n", "-u", user] if user else []), sys.executable, "-c", CONNECT, sock, request], capture_output=True, text=True, cwd="/")
        # What went wrong before the request was made, where nothing was printed.
        return run.stdout.strip() or run.stderr.strip()[-300:]
    try:
        mine = knock(None)
        check("the user's own raw request is answered", made.returncode == 0 and "Status" in mine, mine[:60])
        modes = (os.stat(home).st_mode & 0o777, os.stat(sock).st_mode & 0o777)
        closed = knock(name)
        check("a second user can't reach the socket", modes == (0o700, 0o600) and closed == "connect: Permission denied", f"modes {modes[0]:o} {modes[1]:o}, {closed}")
        # Every directory above has to let the second user through for the daemon to be the one that refuses.
        opened, at = [], home
        while at != "/" and os.stat(at).st_uid == os.getuid():
            opened.append((at, os.stat(at).st_mode & 0o7777))
            os.chmod(at, opened[-1][1] | 0o011)
            at = os.path.dirname(at)
        os.chmod(sock, 0o666)
        try:
            refused = knock(name)
        finally:
            os.chmod(sock, 0o600)
            for at, mode in opened:
                os.chmod(at, mode)
        log = open(os.path.join(home, "daemon.log")).read()
        check("with the modes opened, the daemon turns the second user away by its uid", refused.startswith("answer:") and "Status" not in refused and "the peer is user" in log, f"{refused!r}")
        mine = knock(None)
        check("and still answers its own user", "Status" in mine, mine[:60])
        # The other way round: the second user listens where a command line points the
        # user's client, which sends its request and its variables to whoever serves it.
        trap = os.path.join(tempfile.mkdtemp(prefix="blaze-trap-", dir="/tmp"), "sock")
        os.chmod(os.path.dirname(trap), 0o777)
        listener = subprocess.Popen(["sudo", "-n", "-u", name, sys.executable, "-c", LISTEN, trap], stdout=subprocess.PIPE, text=True, cwd="/")
        try:
            listener.stdout.readline()
            asked = subprocess.run([BLAZE, "sync", trap], capture_output=True, text=True, timeout=60)
            heard = listener.communicate(timeout=30)[0].strip()
        finally:
            listener.kill()
            sudo("rm", "-rf", os.path.dirname(trap))
        check("the client says nothing to a socket a second user serves", asked.returncode != 0 and "the peer is user" in asked.stderr and heard == "heard: 0", f"status {asked.returncode}, {heard}")
    finally:
        if not DARWIN:
            sudo("userdel", name)


# Each call is logged with the build that made it, so the calls of one build can be told from
# those of a daemon's or a settle's cargo still running in the background.
WRAPPER = '#!/bin/sh\nprintf "%s\\t%s\\n" "$1" "${{BLAZE_HARNESS_BUILD:-}}" >> "{log}"\nexec "$@"\n'
DOCTEST = '/// ```\n/// assert_eq!(util::doubled(2), 4);\n/// ```\npub fn doubled(n: u32) -> u32 {\n    n * 2\n}\n'

# What a build said when it handed itself, or a program of it, to cargo or rustc, or warned that
# the project's toolchain was not blaze's (the 10-20 nightly's lines, by either name).
FELL_BACK = re.compile(r"^(blaze|blaze): .*|; building with cargo|another build of (blaze|blaze)|this (blaze|blaze) compiles for|^warning: ", re.M)


def blazes(stderr):
    """Whether blaze built it whole: its own `Finished` line, which a build cargo ran has not.
    Its `Compiling` lines are its own too, so `by_rustc` can't tell."""
    return bool(re.search(r"^\s*Finished .*\[(blaze|blaze)\]", stderr, re.M)) and "; building with cargo" not in stderr


def upgraded(work):
    """One of Andrew's two lines of the 10-20 nightly (#270, the fallback lane): a project whose
    rust-toolchain file names another release. It is not said, and the build is not cargo's:
    blaze builds with its own release whatever the project names. The other, a build whose
    states an older blaze wrote, went with plain cargo, which alone wrote them."""
    checks = []

    def check(name, held, detail=""):
        checks.append((name, bool(held), detail))
        print(f"  {'ok ' if held else 'NOT'} {name}  {detail}", flush=True)

    said = lambda ran: "; ".join(m[0] for m in FELL_BACK.finditer(ran.stderr))[:300]
    if WINDOWS or not shutil.which("rustup"):
        return checks
    pinned_env = base_env(os.path.join(work, "home-upgraded-pinned"))
    pinned_env.pop("RUSTUP_TOOLCHAIN")
    ws = os.path.join(work, "upgraded-pinned")
    fake_toolchain(os.path.join(ws, "toolchain"), "1.98.1")
    os.makedirs(os.path.join(ws, "src"))
    open(os.path.join(ws, "Cargo.toml"), "w").write('[package]\nname = "anim"\nversion = "0.1.0"\nedition = "2021"\n')
    open(os.path.join(ws, "rust-toolchain.toml"), "w").write(f'[toolchain]\npath = "{os.path.join(ws, "toolchain")}"\n')
    open(os.path.join(ws, "src/main.rs"), "w").write('fn main() {\n    println!("anim {}", 6 * 7);\n}\n')
    side = Side(ws, pinned_env, True)
    for round in ("first", "second"):
        _, ran = side.cargo("run", "--release")
        # The pin is not read, and not said but under `-v` (#837).
        notes = len(re.findall(r"^.*(own toolchain|rust-toolchain|0000000 2026-01-01).*$", ran.stderr, re.M))
        check(f"a project pinned to rustc 1.98.1, {round} `blaze run --release`: blaze's release, no cargo, no warning", ran.returncode == 0 and "anim 42" in ran.stdout and not said(ran) and blazes(ran.stderr) and notes == 0, f"exit {ran.returncode}, {notes} notes, {said(ran) or ran.stderr.strip()[-200:]!r}")
    side.cargo("stop")
    return checks


def worktree(ours, row, build, run, edits, work):
    """A second checkout of the project's repository, at the commit the first started from:
    built from nothing, which is cargo's, then edited as the first was and built again. What
    its program says after the edit has to be what the first one's says."""
    git = lambda *args: subprocess.run(["git", "-c", "user.name=blaze", "-c", "user.email=blaze@localhost", *args], cwd=ours.ws, capture_output=True, text=True)
    # One place for each way of building: a daemon still loading the first writes there
    # while it is removed, and what that leaves is not the second's.
    tree = os.path.realpath(os.path.join(work, f"{row['project']}-worktree-blaze"))
    added = git("worktree", "add", "-q", "--detach", tree)
    if added.returncode != 0:
        row["wrong"].append(f"worktree: {added.stderr[-200:]}")
        return
    there = Side(tree, ours.env, True)

    def step(label, command, args):
        took, ran = there.cargo(command, *args)
        row["times"][label] = (round(took, 2), None)
        row["built"][label] = there.how_built(ran.stderr)
        print(f"  {label:12} blaze {took:7.2f}s  {row['built'][label]:8.80}  exit {ran.returncode}", flush=True)
        return ran

    if step("tree build", "build", build).returncode != 0:
        row["wrong"].append("tree build: failed")
    took, states = wait_warm(there, tree)
    row["times"]["tree warm-up"], row["built"]["tree warm-up"] = (round(took, 2), None), "; ".join(states)
    print(f"  tree warm-up {took:7.2f}s  {'; '.join(states)[:150]}", flush=True)
    for file, old, new in edits:
        rewrite(os.path.join(tree, file), old, new)
    if step("tree edit", "build", build).returncode != 0:
        row["wrong"].append("tree edit: failed")
    if run:
        _, first = ours.cargo("run", *run)
        ours.how_built(first.stderr)
        second = step("tree run", "run", run)
        wrong = difference(program_output(second), program_output(first))
        if wrong:
            row["wrong"].append(f"tree run: {wrong}")
    git("worktree", "remove", "--force", tree)
    shutil.rmtree(tree, ignore_errors=True)


def table(rows):
    """The table of the report: for every step the seconds of `blaze` and of cargo."""
    steps = ["cold build", "warm-up", "warm build", "cold test", "test warm-up", "warm test", "edit build", "edit test", "broken", "mended", "tree build", "tree warm-up", "tree edit"]
    out = ["| Project | " + " | ".join(steps) + " | Functions by blaze | Differences |", "|---|" + "---:|" * (len(steps) + 1) + "---|"]
    for row in rows:
        cells = []
        for step in steps:
            ours, theirs = row["times"].get(step, (None, None))
            who = row["built"].get(step, "")
            mark = "" if who in ("daemon", "cargo") or "warm-up" in step else " (c)" if who.startswith("cargo") else ""
            if "warm-up" in step:
                mark = "" if who and all(s == "ready" for s in who.split("; ")) else " (f)"
            cells.append("-" if ours is None else f"{ours:.2f}{mark}" if theirs is None else f"{'**' if who == 'daemon' else ''}{ours:.2f}{'**' if who == 'daemon' else ''}{mark} / {theirs:.2f}")
        done, of = row.get("share", (0, 0))
        share = f"{done} of {of} ({100 * done // of}%)" if of else "-"
        out.append(f"| {row['project']} {row['tag']} | " + " | ".join(cells) + f" | {share} | {len(row['wrong'])} |")
    return "\n".join(out)


def main():
    argv = sys.argv[1:]
    names = [a for a in argv if not a.startswith("--")] or (SMALL if "--small" in argv else list(PROJECTS))
    modes = [m for m in MODES if f"--{m}" in argv] or list(MODES)
    # As the daemon names it: with no link and no short name in it.
    # The path of a socket is at most 104 bytes on macOS, and its temporary directory is long.
    work = os.path.realpath(tempfile.mkdtemp(prefix="blaze-cargo-", dir="/tmp" if sys.platform == "darwin" else None))
    print(f"work {work}; blaze {BLAZE}", flush=True)
    result = dict(blaze=BLAZE, toolchain=TOOLCHAIN, projects=[], flags=[], daemon=[], wrong=[])
    alone = "--only-upgraded" in argv
    if "--only-checks" not in argv and not alone:
        for name in names:
            for mode in modes:
                print(f"{name}, {MODES[mode]}:", flush=True)
                row = project(name, PROJECTS[name], work, mode)
                result["projects"].append(row)
                result["wrong"] += [f"{name}, {mode}: {w}" for w in row["wrong"]]
                if "--keep" not in argv:
                    shutil.rmtree(os.path.join(work, name), ignore_errors=True)
    if "--only-projects" not in argv and not alone:
        print("flags:", flush=True)
        rows, wrong, counted = flags(work)
        result["flags"], result["flags_status"] = rows, counted
        result["wrong"] += wrong
        print("daemon:", flush=True)
        result["daemon"] = daemon(work)
        result["wrong"] += [f"daemon: {name} {detail}" for name, held, detail in result["daemon"] if not held]
    if "--only-projects" not in argv:
        print("upgraded:", flush=True)
        result["upgraded"] = upgraded(work)
        result["wrong"] += [f"upgraded: {name} {detail}" for name, held, detail in result["upgraded"] if not held]
    # A run of some of it leaves the rows of the rest as the run before wrote them.
    llvm = os.environ.get("BLAZE_BACKEND") == "llvm"
    stored = os.path.join(HERE, "blaze_cli_llvm.json" if llvm else "blaze_cli.json")
    before = json.load(open(stored)) if os.path.exists(stored) else {}
    rows = {(row["project"], row.get("mode", "blaze")): row for row in before.get("projects", []) + result["projects"]}
    result["projects"] = [rows[name, mode] for name in PROJECTS for mode in MODES if (name, mode) in rows]
    if "--only-projects" in argv or alone:
        kept = ("flags", "flags_status", "daemon") + (() if alone else ("upgraded",))
        result.update({part: before.get(part, []) for part in kept})
    json.dump(result, open(stored, "w"), indent=1)
    with open(os.path.join(HERE, "BLAZE-CLI-LLVM.md" if llvm else "BLAZE-CLI.md"), "w") as md:
        md.write(f"# blaze against cargo, {'LLVM at -O2' if llvm else 'Cranelift'}\n\nGenerated by `harness/blaze_cli.py`. Seconds, blaze / cargo alone for the same step; the steps in the second checkout (`tree`) are blaze's only. "
                 "Bold is a build the daemon did; `(c)` one cargo did in its place, with the reason below; `(f)` a warm-up that failed. "
                 "Differences counts the steps whose program output, exit status or test results were not cargo's.\n\n")
        for mode, title in MODES.items():
            md.write(f"## {title}\n\n" + table([r for r in result["projects"] if r.get("mode", "blaze") == mode]) + "\n\n")
        for row in result["projects"]:
            reasons = [f"{step}: {who}" for step, who in row["built"].items() if who.startswith(("cargo: ", "unchanged: ")) or "failed" in who]
            if reasons or row["wrong"]:
                md.write(f"**{row['project']}, {MODES[row.get('mode', 'blaze')]}**\n\n" + "".join(f"- {r}\n" for r in reasons + row["wrong"]) + "\n")
        if result["flags"]:
            md.write("## Flags\n\n| Round | Command | Built by | Exit | Against cargo |\n|---|---|---|---:|---|\n")
            md.writelines(f"| {label} | `{shown}` | {who} | {code} | {same} |\n" for label, shown, who, code, same in result["flags"])
        if result["daemon"]:
            md.write("\n## Daemon\n\n" + "".join(f"- {'ok' if held else 'NOT'}: {name} {detail}\n" for name, held, detail in result["daemon"]))
        if result.get("upgraded"):
            md.write("\n## After an upgrade, and a project pinned to another release\n\n" + "".join(f"- {'ok' if held else 'NOT'}: {name} {detail}\n" for name, held, detail in result["upgraded"]))
    # A failed run keeps its workers' logs for CI's upload (blaze-platform.yaml's blaze-cli-logs).
    if "--keep" not in argv and not result["wrong"]:
        shutil.rmtree(work, ignore_errors=True)
    print("PASS" if not result["wrong"] else "FAIL:\n" + "\n".join(result["wrong"]))
    sys.exit(1 if result["wrong"] else 0)


if __name__ == "__main__":
    main()
