#!/usr/bin/env python3
"""unittests.py: the crates' own tests, with the ones known to fail named.

usage: unittests.py [CONFIG]

CONFIG is one JSON object, inline, in a file, or `-` for standard input, `{}` when left out:

    {"cargo": false, "build": false, "args": ["<test argument>", ...], "max_mb": 0}

`max_mb`, when not 0, fails the run when the machine had more than that many megabytes of memory
and swap in use at once while it ran (docs/memory-budget.md, the check in CI).

Runs `blaze test --release --no-fail-fast` of CRATES with their default features, blaze-engine's
`native` (LLVM among them) included (blaze tests blaze, AGENTS.md rule blaze-builds-blaze;
`"cargo": true`: plain `cargo test`, on Windows, which has no release of blaze to test with yet), one run in the root workspace, with `args` after its own.
`"build": true` is CI's build job: `blaze build` of the same packages with the same profile and
features, so that each library it compiles has the key the test build asks for (a crate of the
workspace is keyed by its features, by whether it is of a package named, `CARGO_PRIMARY_PACKAGE`,
and by the keys of the crates it is built on), and the tests job takes those from the store the build job hands it
(.github/workflows/blaze-platform.yaml). Only the crates built with `cfg(test)` are then compiled
again. Each line of
the run is passed on as it comes, so a CI log shows a slow build while it is slow. The run's
output is kept in `target/known-failures/unittests.log` and held to harness/known-failures.toml
by harness/failures.py: a failing test that is not listed there fails the run, and so does a
listed test that passes or no longer exists (take it out), and a test binary that does not
finish; so does the cargo run failing before any of its tests failed. Each listed failure has an
owner and a cause. `blaze-diagnostics` is the daemon's errors (its diagnostics are the verdict).
"""
import dataclasses, json, os, re, shutil, signal, subprocess, sys, threading, time, typing

import failures
import memory

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CRATES = [
    "hir-ty-codegen", "blaze-cas", "blaze-registry", "blaze-resolve", "blaze-build-script", "blaze-manifest", "blaze-cargo", "blaze-engine",
    "hir-ty", "hir-ty-mir", "hir-ty-borrowck", "hir-ty-wfcheck", "hir-def", "parser", "syntax", "const-ast", "blaze-diagnostics",
]
CRATES += ["proc-macro-api"]
# MIR to Charon's LLBC (vendor/charon): needs no solver or tool outside the tree, unlike hir-ty-verus.
CRATES += ["hir-ty-charon"]

# The failures known, each with its owner and cause: harness/known-failures.toml (failures.py).

COLOR = re.compile(r"\x1b\[[0-9;]*m")
# A test binary is `deps/<name>-<hash>` in cargo's first layout of the build directory and
# `build/<package>/<hash>/out/<name>-<hash>` in its second (nightly cargo's default); blaze names
# it `deps/<name>-<hash>` too, a bin's name with its dashes.
RUNNING = re.compile(r"^\s+Running .*[/\\](?:deps|out)[/\\]([\w-]+?)-[0-9a-f]{16}\b")
DOCTESTS = re.compile(r"^\s+Doc-tests (\w+)")
RESULT = re.compile(r"^test (\S+)(?: - .*)? \.\.\. (ok|FAILED|ignored)")


class Watch:
    """What the machine and the run's processes hold, said every `EVERY` seconds while the run
    runs, and the most of it: a hosted runner that runs out of memory and swap is shut down
    with nothing in the log ("The runner has received a shutdown signal"), and these lines
    are the last it has."""

    EVERY = 30

    def __init__(self, proc, dirs):
        self.proc = proc
        pid = proc.pid
        self.sampler = memory.Sampler(interval=1.0).add(pid)
        self.dirs, self.peak, self.at_peak, self._stop = dirs, 0, None, threading.Event()
        self.began = time.monotonic()
        self.cpu, self.cpu_shown, self.idle_since, self.dumped = {}, {}, None, False
        self.read_to, self.tailed = {}, False

    # Swap in use, in MB, past which the end of each blaze log is said once: what the daemon and
    # its workers were doing when the machine ran out of memory.
    SWAPPING = 2048

    def tails(self, count=80):
        home = self.blaze_home()
        for at, _, files in os.walk(home):
            for name in files:
                if name not in ("log", "daemon.log"):
                    continue
                log = os.path.join(at, name)
                try:
                    with open(log, "rb") as f:
                        lines = f.read().decode(errors="replace").splitlines()[-count:]
                except OSError:
                    continue
                print(f"::group::the end of {os.path.relpath(log, home)}, the machine swapping", flush=True)
                print("\n".join(lines), flush=True)
                print("::endgroup::", flush=True)

    # Lines of the daemon's and its workers' logs that are said in the run's: they are files the
    # CI log does not have, and a runner that is lost takes them with it.
    SAID = re.compile(r"memory|over budget|not compiling the program")

    @staticmethod
    def blaze_home():
        """Where the daemon keeps its log and its workers', as `daemon::home` says."""
        if os.environ.get("BLAZE_HOME"):
            return os.environ["BLAZE_HOME"]
        if os.environ.get("XDG_RUNTIME_DIR"):
            return os.path.join(os.environ["XDG_RUNTIME_DIR"], "blaze")
        return os.path.expanduser("~/.cache/blaze")

    def logged(self):
        """The lines of the blaze logs, new since the last call, that `SAID` matches."""
        home = self.blaze_home()
        logs = [os.path.join(home, "daemon.log")]
        for at, _, files in os.walk(home):
            logs += [os.path.join(at, it) for it in files if it == "log"]
        said = []
        for log in logs:
            try:
                with open(log, "rb") as f:
                    f.seek(self.read_to.get(log, 0))
                    text = f.read()
                    self.read_to[log] = f.tell()
            except OSError:
                continue
            name = os.path.relpath(log, home)
            said += [f"{name}: {line}" for line in text.decode(errors="replace").splitlines() if self.SAID.search(line)]
        return said

    # A run whose blaze-engines use less than this share of one core for `STILL` seconds is stuck:
    # the threads of its blaze-engines are dumped and the run is stopped.
    IDLE, STILL = 0.05, 300

    @staticmethod
    def busy(table, pid, now):
        """The share of one core `pid` used since `table` last had it, or None the first time."""
        cpu = memory.cpu_seconds(pid)
        last, table[pid] = table.get(pid), (now, cpu)
        if cpu is None or last is None or last[1] is None or now <= last[0]:
            return None
        return (cpu - last[1]) / (now - last[0])

    def engines(self):
        """The blaze-engines of the run that still run: the daemon and its workers, whose CPU says
        whether a build moves (the client's ticker uses some however stuck the build is)."""
        named = lambda pid: open(f"/proc/{pid}/comm").read().strip() if os.path.exists(f"/proc/{pid}/comm") else ""
        return [pid for pid in list(self.sampler.most) if named(pid) == "blaze-engine"]

    def stuck(self, now):
        """Whether the run's blaze-engines have been as good as idle for `STILL` seconds."""
        shares = [self.busy(self.cpu, pid, now) for pid in self.engines()]
        shares = [it for it in shares if it is not None]
        if not shares or sum(shares) >= self.IDLE:
            self.idle_since = None
            return False
        self.idle_since = self.idle_since or now
        return now - self.idle_since >= self.STILL

    def dump(self):
        """Every thread's stack of each blaze-engine of the run, by gdb."""
        engines = self.engines()
        if not shutil.which("gdb"):
            print(f"::warning::the run is stuck (blaze-engine {engines}) and there is no gdb to say where", flush=True)
            return
        for pid in engines:
            print(f"::group::the threads of blaze-engine[{pid}], stuck {self.STILL // 60} min", flush=True)
            subprocess.run(["sudo", "-n", "gdb", "-p", str(pid), "-batch", "-ex", "thread apply all bt 40"], stdin=subprocess.DEVNULL, timeout=600)
            print("::endgroup::", flush=True)

    def line(self):
        machine = memory.system_mb()
        if machine:
            held = machine["used"] + machine["swap_used"]
            if held > self.peak:
                self.peak, self.at_peak = held, machine
        gb = lambda mb: f"{mb / 1024:.1f}"
        said = [f"memory at {(time.monotonic() - self.began) / 60:.0f} min:"]
        if machine:
            said.append(f"machine {gb(machine['used'])}/{gb(machine['total'])} GB, swap {gb(machine['swap_used'])}/{gb(machine['swap_total'])} GB;")
        said.append(f"the run's processes {gb(self.sampler.peak_mb)} GB at most;")
        said.append("disk free " + ", ".join(f"{d} {shutil.disk_usage(d).free / 2**30:.0f} GB" for d in self.dirs if os.path.isdir(d)) + ";")
        now = time.monotonic()
        cpu = lambda pid: "" if (share := self.busy(self.cpu_shown, pid, now)) is None else f" {share * 100:.0f}% cpu"
        said.append("largest " + ", ".join(f"{name}[{pid}] {gb(rss)}+{gb(swap)} GB{cpu(pid)}" for rss, swap, pid, name in memory.largest_processes(4)))
        return " ".join(said)

    def _loop(self):
        while not self._stop.wait(self.EVERY):
            print(self.line(), flush=True)
            for line in self.logged():
                print(f"blaze log: {line}", flush=True)
            machine = memory.system_mb()
            if not self.tailed and machine and machine["swap_used"] > self.SWAPPING:
                self.tailed = True
                self.tails()
            if not self.dumped and self.stuck(time.monotonic()):
                self.dumped = True
                self.dump()
                print(f"::error::the run used under {self.IDLE * 100:.0f}% of a core for {self.STILL // 60} min: stuck, stopped (the threads are above)", flush=True)
                # The daemon keeps the output open, which the run's end waits for.
                for pid in [self.proc.pid, *self.engines()]:
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except OSError:
                        pass

    def __enter__(self):
        self.sampler.start()
        threading.Thread(target=self._loop, daemon=True).start()
        return self

    def __exit__(self, *_):
        self._stop.set()
        self.sampler.stop()
        print(self.line(), flush=True)


def run(command, cwd, env, seen, failed, watch_dirs, log):
    """Runs one `cargo test`, adding its tests to seen and failed and its output to `log`;
    returns its exit status, whether any of its tests failed and its `Watch`."""
    print(" ".join(command), flush=True)
    proc = subprocess.Popen(command, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace")
    with Watch(proc, watch_dirs) as watch, open(log, "w", encoding="utf-8") as kept:
        return *read(proc, seen, failed, kept), watch


def read(proc, seen, failed, kept):
    binary, any_failed = "?", False
    for line in proc.stdout:
        # Flushed: a pipe's buffer (128 KiB from Python 3.14) held a whole build's lines until
        # the tests' output filled it, so CI's log showed nothing for half an hour.
        sys.stdout.write(line)
        sys.stdout.flush()
        kept.write(line)
        line = COLOR.sub("", line)
        if m := RUNNING.match(line) or DOCTESTS.match(line):
            binary = m.group(1).replace("-", "_")
        elif m := RESULT.match(line):
            name = f"{binary}::{m.group(1)}"
            seen.add(name)
            if m.group(2) == "FAILED":
                failed.add(name)
                any_failed = True
    return proc.wait(), any_failed


@dataclasses.dataclass(frozen=True)
class Config:
    # Plain `cargo test`, where there is no release of blaze to test with (Windows).
    cargo: bool = False
    # `blaze build` of what the tests are built on, and no tests: CI's build job.
    build: bool = False
    # Passed after the command's own arguments.
    args: list = dataclasses.field(default_factory=list)
    # The most memory and swap the machine may have in use at once, in MB; 0 is no bound.
    max_mb: int = 0


def config(argv: list[str]) -> Config:
    """The one JSON value of the command line, as a `Config`; `{}` when there is none."""
    if len(argv) > 1:
        sys.exit("usage: unittests.py [CONFIG], one JSON object (see the top of the file)")
    argument = argv[0] if argv else "{}"
    text = sys.stdin.read() if argument == "-" else open(argument).read() if os.path.isfile(argument) else argument
    try:
        value = json.loads(text)
    except json.JSONDecodeError as e:
        sys.exit(f"unittests.py: the configuration is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("unittests.py: the configuration is a JSON object")
    fields = {f.name for f in dataclasses.fields(Config)}
    hints = typing.get_type_hints(Config)
    for key, item in value.items():
        if key not in fields:
            sys.exit(f"unittests.py: `{key}` is not a field of the configuration ({', '.join(sorted(fields))})")
        if type(item) is not hints[key]:
            sys.exit(f"unittests.py: `{key}` is a {hints[key].__name__}, not {json.dumps(item)}")
    if not all(isinstance(it, str) for it in value.get("args", [])):
        sys.exit("unittests.py: `args` is a list of strings")
    return Config(**value)


def main():
    cfg = config(sys.argv[1:])
    plain, args = cfg.cargo, cfg.args
    if cfg.build and plain:
        sys.exit("unittests.py: `build` is blaze's build, not cargo's")
    packages = [f"-p{c}" for c in CRATES]
    if plain:
        command = ["cargo", "test", "--release", "--no-fail-fast", *packages, *args]
    else:
        verb = ["build"] if cfg.build else ["test", "--no-fail-fast"]
        command = ["blaze", *verb, "--release", "--locked", *packages, *args]
    # Under CI rust-analyzer's slow tests run, and mark that they did in the target directory.
    env = {"CARGO_TARGET_DIR": os.path.join(ROOT, "target"), **os.environ}
    # blaze's frames are larger than rustc's, and blaze-diagnostics' 5204-arm test overflows the
    # 2 MiB a test thread has, which aborts its whole program
    # (https://github.com/indexable-inc/blaze/issues/894). Not a `--skip`: an argument to the
    # tests has blaze run every test, not those an edit reached.
    if not plain:
        env.setdefault("RUST_MIN_STACK", str(16 << 20))
    seen, failed = set(), set()
    watch_dirs = [ROOT, *(d for d in [os.environ.get("BLAZE_CACHE"), "/mnt"] if d)]
    log = os.path.join(env["CARGO_TARGET_DIR"], "known-failures", "unittests.log")
    os.makedirs(os.path.dirname(log), exist_ok=True)
    status, any_failed, watch = run(command, ROOT, env, seen, failed, watch_dirs, log)
    sys.stdout.flush()
    took = (time.monotonic() - watch.began) / 60
    peak = f"{watch.peak / 1024:.1f} GB of memory and swap in use on the machine at most" if watch.at_peak else "no memory reading on this platform"
    said = f"`{' '.join(command[:2])}`: {took:.0f} min, {peak}, {watch.sampler.peak_mb / 1024:.1f} GB resident in the run's processes at most"
    print(said, flush=True)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
            f.write(f"### Unit tests\n\n{said}\n\n")
    over = cfg.max_mb and watch.at_peak and watch.peak > cfg.max_mb
    if over:
        print(f"::error::the machine had {watch.peak} MB of memory and swap in use at once, over the {cfg.max_mb} MB bound (max_mb; docs/memory-budget.md)")
    if cfg.build:
        sys.exit(status or (1 if over else 0))
    print(f"\n{len(seen)} tests, {len(failed)} failed; held to harness/known-failures.toml:", flush=True)
    context = failures.Context("cargo" if plain else "blaze", "release", failures.this_os())
    try:
        held = failures.check([failures.Path(log)], context, report=failures.Path(log).with_suffix(".json"))
    except failures.Refused as e:
        print(f"::error::harness/failures.py: {e}")
        sys.exit(1)
    if status and not any_failed:
        print(f"::error::{' '.join(command)} failed (exit status {status}) before any test failed: the build, most likely")
        sys.exit(1)
    sys.exit(0 if held and not over else 1)


if __name__ == "__main__":
    main()
