"""engine.py: the blaze a v2 proof script measures, behind one interface.

Every script under harness/v2 takes, in its typed input, an `engine`:

    {"kind": "v2", "blaze": "target/release/blaze"}

(`kind` is `"v2"`, the one engine there is. `backend` takes only the default, as blaze picks each
unit's backend from its profile and refuses every unit while BLAZE_BACKEND is set;
`"dev_backend": "stencil"` or `"cranelift"` forces the dev tier's backend for every function,
`"default"` leaving it to blaze), and does everything through `Engine`. Builds, runs and tests
go through the `blaze` command line (`blaze build`, `run`, `test`, `check`, `status`, `stop`).
What only an engine's insides can say (what an edit recompiled, the codegen inputs of a build,
the critical path of a cold build) is a method here:

- a method with no answer raises `Unmeasurable`, and the result says "unmeasured", which is a
  failure of the run;
- blaze answers each from the agent API (`crates/blaze-api`) through the command line's forms of
  it (`crates/blaze-ui/src/api.rs`): what a request did from the `blaze-work` line a build, test
  or run prints under `--message-format=json`; `blaze codegen-inputs --format=json`,
  `blaze trace --out PATH` and `blaze tables --format=json` for the rest. An answer it cannot
  read is `Failed`, naming the command. Nothing is guessed.

`Failed` is a step the harness could not do (a command that timed out, a daemon that did not
come up): an error of the run, never a finding about the compiler.

Where a daemon keeps what it keeps is `layout(home)` (harness/storelib.py's `Layout`, the one
place it is decided): its socket, state, store, lock, log, profile history and build
directories are all under its home, which `BLAZE_HOME` names, and no setting moves one out. So
`Engine.env` takes a home and no store, `store_of(home)` is where a script reads the store, and
`Engine.build_dir` is where a build's outputs are (the checkout has no `target`). `Engine.of`
ends with `preflight`: what this harness assumes of the binary and cannot tell from the source
it was written against (`blaze --version`, the commands it runs as `blaze --help` lists them,
`blaze status --json`'s home, state and socket against `layout`) is checked against the binary
before any build, and a miss is a `Failed` naming the assumption.

`reference_env` is plain cargo with the pinned nightly (`harness/toolchain`), no incremental
compilation and no wrapper: what a script fetches sources with (`cargo fetch`, `cargo vendor`,
`cargo metadata`). Nothing blaze builds is compared with what cargo builds.
"""

import concurrent.futures
import io
import json
import mmap
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from abc import ABC, abstractmethod
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, cast

import blake3

HERE = Path(__file__).resolve().parent
HARNESS = HERE.parent
if str(HARNESS) not in sys.path:
    sys.path.append(str(HARNESS))
import storelib  # noqa: E402

ROOT = HARNESS.parent
TOOLCHAIN = (HARNESS / "toolchain").read_text().strip()
EXE = ".exe" if os.name == "nt" else ""
# Read from the caller's environment by cargo, rustc or blaze, and set by the harness alone.
SCRUBBED = (
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET",
    "CARGO_BUILD_JOBS",
    "CARGO_INCREMENTAL",
    "BLAZE_HOME",
    "BLAZE_BIN",
    "BLAZE_BACKEND",
    "BLAZE_OPT",
    "BLAZE_EMIT",
    # A memory limit nothing reads any more (the daemon's is BLAZE_DAEMON_MEMORY_GB, which a
    # caller keeps): dropped so a stale export says nothing.
    "BLAZE_MEMORY_BUDGET",
    # Where the socket goes without BLAZE_HOME (blaze_api::client::home); every command here has one.
    "XDG_RUNTIME_DIR",
)
# The commands whose request ends in `Response::Work` (crates/blaze-api/src/build.rs), which the
# command line prints under `--message-format=json` as a `blaze-work` line
# (crates/blaze-ui/src/api.rs, `work_json`).
WORKING = ("build", "test", "run")
WORK_COUNTS = ("reran", "green", "compiled", "found", "critical_path_us")

type Kind = Literal["v2"]
type Backend = Literal["cranelift", "llvm"]
type DevBackend = Literal["default", "stencil", "cranelift"]
DEV_BACKENDS: tuple[DevBackend, ...] = ("default", "stencil", "cranelift")
# What the v2 daemon lacks for a forced dev backend; `Engine.__init__` refuses one until it has it.
DEV_BACKEND_MISSING = (
    "the v2 daemon has no setting for the dev tier's backend yet: cargo's `codegen-backend` profile key "
    "(`-Zcodegen-backend --config 'profile.dev.codegen-backend=\"stencil\"'`) is parsed by blaze-manifest and "
    "not read into `blaze_codegen::plan::Policy`, which compiles opt-level 0 and 1 with Cranelift whatever it says; "
    "requested in fed1eb428d:docs/v2/requests/requests-backend-axis.md (codegen: `Profile::codegen_backend` in plan.rs; cargo: "
    "`serve/unit/codegen.rs::policy` reading it; coordinator: blaze-manifest's `-Zcodegen-backend` gate)"
)


@dataclass(frozen=True)
class EngineSpec:
    """The `engine` field of every script's input."""

    kind: Kind
    # The `blaze` command line client.
    blaze: str
    backend: Backend = "cranelift"
    # The backend of every function the dev tier compiles (opt-level 0 and 1, and the first
    # tier of a tiered one): "default" leaves it to blaze, the others force it.
    dev_backend: DevBackend = "default"

    def __post_init__(self) -> None:
        if self.dev_backend not in DEV_BACKENDS:
            raise Failed(f"engine.dev_backend: one of {', '.join(DEV_BACKENDS)}, not {self.dev_backend!r}")


class Unmeasurable(Exception):
    """What was asked has no answer in this engine."""


class Failed(Exception):
    """A step the harness could not do, said whole: the command, its status, what it said."""


@dataclass(frozen=True)
class Ran:
    """A command that ran to its end. Shaped as `subprocess.CompletedProcess` where
    `blaze_cli.program_output` and `test_results` read it."""

    argv: tuple[str, ...]
    seconds: float
    returncode: int
    stdout: str
    stderr: str
    def said(self, tail: int = 3000) -> str:
        return (self.stdout + self.stderr)[-tail:]

    def checked(self, step: str) -> "Ran":
        if self.returncode != 0:
            raise Failed(f"{step}: `{' '.join(self.argv)}` exited {self.returncode}:\n{self.said()}")
        return self


@dataclass(frozen=True)
class Work:
    """What one or more builds did, as the engine says it: `compiled` functions compiled (code
    generated, not found in the store), `reran` queries executed rather than found green, and
    how the program was linked. None where the engine does not say."""

    compiled: int | None
    reran: int | None = None
    found: int | None = None
    link: str | None = None
    said: str = ""
    green: int | None = None
    # The reports of steps recomputed (`blaze-report` lines, one a root cause): none from a
    # build that recomputed nothing (docs/v2/plan/design/why.md, "What is printed").
    reports: int | None = None
    # Each of those reports' text.
    causes: tuple[str, ...] = ()


@dataclass(frozen=True)
class Table:
    """One of the daemon's long-lived tables (docs/v2/plan/design/data.md): how many elements
    it holds and the bytes of all its columns and side tables together."""

    elements: int
    bytes: int


@dataclass(frozen=True)
class Mark:
    """Where an engine's logs were at a moment: `work_since` reads what came after it."""

    sizes: dict[str, int] = field(default_factory=dict[str, int])


def text(data: bytes) -> str:
    """What `subprocess.run(text=True, encoding="utf-8", errors="replace")` makes of a pipe's
    bytes (its `_translate_newlines`), done after the clock stops."""
    return data.decode("utf-8", errors="replace").replace("\r\n", "\n").replace("\r", "\n")


def run(step: str, argv: list[str], cwd: Path, env: dict[str, str], timeout: float = 3600, check: bool = False) -> Ran:
    """Runs `argv` to its end. A command that outlives `timeout`, or with `check` exits other
    than 0, is a `Failed` naming the step. `Ran.seconds` is the spawn to the reaped exit: the
    output is decoded after it, so a budget times the command and not the harness."""
    began = time.perf_counter()
    try:
        done = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, timeout=timeout, stdin=subprocess.DEVNULL)
    except subprocess.TimeoutExpired as e:
        out = e.stdout.decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
        raise Failed(f"{step}: `{' '.join(argv)}` in {cwd} took more than {timeout:.0f} s:\n{out[-3000:]}") from None
    except OSError as e:
        raise Failed(f"{step}: `{' '.join(argv)}` in {cwd} did not start: {e}") from None
    took = time.perf_counter() - began
    ran = Ran(tuple(argv), took, done.returncode, text(done.stdout), text(done.stderr))
    if check and ran.returncode != 0:
        raise Failed(f"{step}: `{' '.join(argv)}` in {cwd} exited {ran.returncode}:\n{ran.said()}")
    return ran


def layout(home: Path) -> storelib.Layout:
    """Where the daemon of `home` keeps everything (storelib.Layout, which states the daemon's
    rule and where its code is): the one place a v2 script gets these paths from."""
    return storelib.layout(str(home))


def store_of(home: Path) -> Path:
    """Where the daemon of `home` keeps its store, and reads no other (`layout`)."""
    return Path(layout(home).store)


def seed_store(source: Path | None, home: Path) -> None:
    """The store of `home` made a copy of the store directory `source` (another home's
    `store_of`, or a copy kept of one), or with None emptied, before the daemon of `home`
    starts: how a store is shared or seeded across homes, as the daemon reads only its own. A
    `source` that is no directory is a `Failed` naming it."""
    store = store_of(home)
    shutil.rmtree(store, ignore_errors=True)
    if source is None:
        return
    if not source.is_dir():
        raise Failed(f"seeding the store of the home {home}: no store directory {source}")
    store.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    shutil.copytree(source, store, symlinks=True)


def work_dir(given: str, prefix: str) -> Path:
    """Where a v2 script keeps its checkouts and daemon homes (storelib.work_dir): `given`, or
    a new directory on a file system blaze can place outputs on; probed either way, and a
    `Failed` naming the directory and the requirement when blaze would refuse it."""
    try:
        return Path(storelib.work_dir(given, prefix))
    except storelib.Failed as e:
        raise Failed(str(e)) from None


def empty_home(home: Path, keep_store: bool = False) -> None:
    """Everything the daemon of `home` kept removed (its state, its build directories, the
    packages it fetched), and with `keep_store` all but its store: what a build "from an empty
    home into the same store" starts from, the store being in the home. The daemon is stopped
    first by the caller."""
    store = store_of(home)
    if not keep_store or not store.is_dir():
        shutil.rmtree(home, ignore_errors=True)
        return
    aside = home.with_name(f"{home.name}.store-kept")
    shutil.rmtree(aside, ignore_errors=True)
    os.rename(store, aside)
    shutil.rmtree(home, ignore_errors=True)
    store.parent.mkdir(parents=True, mode=0o700)
    os.rename(aside, store)


BUILD_DIR_ID = re.compile(r"[0-9a-f]{32}")


def build_dir_name(ws: Path) -> str:
    """The front of the name of the default build directory of the workspace at `ws`, before
    its id: the last part of its path, each character but ASCII letters, digits, `-`, `_` and
    `.` as `_`, then `-` (blaze_manifest::home::build_dir); empty for a path with no last part."""
    name = "".join(c if c.isascii() and (c.isalnum() or c in "-_.") else "_" for c in ws.name)
    return f"{name}-" if name else ""


def default_build_dir(home: Path, ws: Path) -> Path:
    """The build directory the daemon of `home` gave the workspace at `ws` when no target
    directory was named: the one `<home>/build/<name>-<id>` (docs/cargo.md, "Where a build
    writes"), `<id>` 32 hex digits of a hash of the path as the daemon names it, which is not
    computed again here: the directory is found by its name's front, and none or more than one
    (two checkouts with one last part built in one home) is a `Failed` naming them."""
    builds, front = Path(layout(home).builds), build_dir_name(ws)
    try:
        found = sorted(p for p in builds.iterdir()
                       if p.is_dir() and p.name.startswith(front) and BUILD_DIR_ID.fullmatch(p.name[len(front):]))
    except FileNotFoundError:
        found = []
    if len(found) != 1:
        raise Failed(f"the build directory of {ws}: {len(found)} directories named `{front}<32 hex digits>` in "
                     f"{builds} ({', '.join(p.name for p in found) or 'none'}), not one; a build with no "
                     "`CARGO_TARGET_DIR` makes one there (crates/blaze-manifest/src/home.rs, `build_dir`)")
    return found[0]


def scrubbed() -> dict[str, str]:
    env = dict(os.environ)
    for name in SCRUBBED:
        env.pop(name, None)
    env.update(RUSTUP_TOOLCHAIN=TOOLCHAIN, RUST_BACKTRACE="0", CARGO_TERM_COLOR="never",
               CARGO_UNSTABLE_BUILD_DIR_NEW_LAYOUT="false")
    return env


def reference_env(target: Path, extra: dict[str, str] | None = None) -> dict[str, str]:
    """Plain cargo with the pinned nightly into `target`, to fetch and read sources with."""
    env = scrubbed()
    env.update(CARGO_TARGET_DIR=str(target), CARGO_INCREMENTAL="0", **(extra or {}))
    return env


def cargo(step: str, args: list[str], ws: Path, env: dict[str, str], timeout: float = 7200) -> Ran:
    return run(step, ["cargo", *args], ws, env, timeout)


# A file this long or longer is hashed on every core (BLAKE3's tree, split by the `blake3`
# package's threads); below it one core takes under 2 ms.
ACROSS_CORES = 8 << 20
# Below this a file is read whole (one `read`) rather than mapped (`mmap`, a fault a page and
# `munmap`), which costs more than copying a small file.
MAPPED_FROM = 1 << 20
# How many bytes of small files, at most, one task of `digests` hashes: one task a file costs
# more in the pool than hashing a small file does.
BATCH_BYTES = 4 << 20
BATCH_FILES = 256


def digest(path: Path, size: int | None = None) -> str:
    """The BLAKE3 hash (hex) of the file at `path`, in one call over its bytes: read whole when
    small, mapped otherwise (no copy) and from `ACROSS_CORES` on hashed by every core. The
    package releases the GIL while it hashes, so `digests` uses every core for many files too.
    The file is a finished build's output, which nothing writes while it is hashed."""
    with open(path, "rb", buffering=0) as f:
        if size is None:
            size = os.fstat(f.fileno()).st_size
        if size < MAPPED_FROM:
            return blake3.blake3(f.read()).hexdigest()
        with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mapped:
            threads = blake3.blake3.AUTO if size >= ACROSS_CORES else 1
            return blake3.blake3(mapped, max_threads=threads).hexdigest()


def digests(files: Sequence[tuple[Path, int]]) -> list[str]:
    """`digest` of each (path, size) in `files`, in their order, on every core at once. The
    largest go first, each a task of its own, so one big file does not run alone at the end;
    small ones go in tasks of up to `BATCH_FILES` files and `BATCH_BYTES` bytes, so the pool
    costs one task per batch, not one per file."""
    if len(files) < 2:
        return [digest(path, size) for path, size in files]
    order = sorted(range(len(files)), key=lambda i: -files[i][1])
    batches: list[list[int]] = []
    held = 0
    for i in order:
        size = files[i][1]
        if not batches or held + size > BATCH_BYTES or len(batches[-1]) >= BATCH_FILES:
            batches.append([])
            held = 0
        batches[-1].append(i)
        held += size
    out = [""] * len(files)

    def batch(chosen: list[int]) -> None:
        for i in chosen:
            out[i] = digest(*files[i])

    with concurrent.futures.ThreadPoolExecutor(min(len(batches), os.cpu_count() or 1)) as pool:
        for _ in pool.map(batch, batches):
            pass
    return out


OUTPUT_SUFFIXES = (".rlib", ".rmeta", ".so", ".dylib", ".dll", ".a", ".lib", ".exe", ".wasm", ".pdb")


def outputs(target: Path, profile: str = "debug") -> dict[str, str]:
    """The files a build of `target/<profile>` delivers, each by its path under it and its
    BLAKE3 digest: every executable and library at its top, in `examples/` and in `deps/` (test
    programs). What cargo keeps for itself (`.fingerprint`, `build`, `incremental`, dep-info,
    `.cargo-lock`) and blaze's own `target/blaze` are not outputs. A directory is listed once
    (its entries' types come with it); only the files chosen are `stat`ed."""
    top = target / profile
    chosen: list[tuple[str, Path, int]] = []
    for sub, prefix in ((top, ""), (top / "examples", "examples/"), (top / "deps", "deps/")):
        try:
            with os.scandir(sub) as listed:
                entries = sorted(listed, key=lambda e: e.name)
        except (FileNotFoundError, NotADirectoryError):
            continue
        for entry in entries:
            try:
                if not entry.is_file(follow_symlinks=False):
                    continue
                suffix = Path(entry.name).suffix
                if suffix and suffix not in OUTPUT_SUFFIXES:
                    continue
                info = entry.stat(follow_symlinks=False)
            except OSError:
                continue
            if suffix or info.st_mode & 0o111:
                chosen.append((prefix + entry.name, Path(entry.path), info.st_size))
    return dict(zip((name for name, _, _ in chosen), digests([(path, size) for _, path, size in chosen])))


def fill(f: io.FileIO, buffer: bytearray) -> int:
    """Reads into `buffer` until it is full or the file ends; how many bytes it holds."""
    view, got = memoryview(buffer), 0
    while got < len(buffer):
        n = f.readinto(view[got:])
        if not n:
            break
        got += n
    return got


def first_difference(a: Path, b: Path) -> str:
    """Where two files first differ: the byte offset, or which is longer. A megabyte of each at
    a time into two buffers made once, compared whole (one memcmp, no copy); the megabyte that
    differs is halved by memcmp too, so no byte is compared in Python but the last 64."""
    size = 1 << 20
    x, y = bytearray(size), bytearray(size)
    with open(a, "rb", buffering=0) as fa, open(b, "rb", buffering=0) as fb:
        at = 0
        while True:
            nx, ny = fill(fa, x), fill(fb, y)
            if nx == ny == size and x == y:
                at += size
                continue
            n = min(nx, ny)
            if n == size or x[:n] != y[:n]:
                lo, hi = 0, n
                while hi - lo > 64:
                    mid = (lo + hi) // 2
                    if x[lo:mid] == y[lo:mid]:
                        lo = mid
                    else:
                        hi = mid
                i = next(i for i in range(lo, hi) if x[i] != y[i])
                return f"byte {at + i:#x}: {x[i]:#04x} against {y[i]:#04x}"
            if nx != ny:
                return f"one ends at byte {at + n:#x}"
            if nx == 0:
                return "the same"
            at += nx


def proof_beside(blaze: str) -> Path:
    """The `blaze-v2-proof` binary built beside the blaze at `blaze`."""
    return Path(blaze).parent / f"blaze-v2-proof{EXE}"


def tree_digest(root: Path, skip: Sequence[str], proof: Path) -> str:
    """The BLAKE3 hash of a source tree: every file's path under `root`, whether it is
    executable and its digest, and every symbolic link's target, in path order; the directories
    named in `skip` at the top are left out (build outputs). `blaze-v2-proof` at `proof` walks
    the directories and hashes the files on every core (`blaze_v2_proof::tree`)."""
    if not proof.is_file():
        raise Failed(f"tree digest of {root}: no `blaze-v2-proof` at {proof}")
    given = json.dumps({"tree": str(root), "skip": list(skip)})
    ran = run(f"tree digest of {root}", [str(proof), given], root, scrubbed(), 600)
    if ran.returncode != 0:
        raise Failed(f"tree digest of {root}: `blaze-v2-proof` exited {ran.returncode}:\n{ran.said()}")
    return cast(str, json.loads(ran.stdout)["digest"])


def first_difference_by(proof: Path, a: Path, b: Path) -> str:
    """`first_difference`'s answer, worded as it words it, found by `blaze-v2-proof` at `proof`:
    both files mapped and compared in place by SIMD, across cores from 64 MiB
    (`blaze_v2_proof::bytes`), then the two bytes at that offset read."""
    if not proof.is_file():
        raise Failed(f"where {a} and {b} first differ: no `blaze-v2-proof` at {proof}")
    given = json.dumps({"first": str(a), "second": str(b)})
    ran = run(f"where {a} and {b} first differ", [str(proof), given], a.parent, scrubbed(), 600)
    if ran.returncode not in (0, 1):
        raise Failed(f"where {a} and {b} first differ: `blaze-v2-proof` exited {ran.returncode}:\n{ran.said()}")
    at = json.loads(ran.stdout)["differs_at"]
    if at is None:
        return "the same"
    with open(a, "rb", buffering=0) as fa, open(b, "rb", buffering=0) as fb:
        fa.seek(at)
        fb.seek(at)
        x, y = fa.read(1), fb.read(1)
    if x and y:
        return f"byte {at:#x}: {x[0]:#04x} against {y[0]:#04x}"
    return f"one ends at byte {at:#x}"


def toolchain_identity(blaze: Path, env: dict[str, str]) -> str:
    """What blaze says its toolchain is (`blaze toolchain --json`: name, nightly, `rustc -V`'s
    line, commit, host, sysroot), for a result to name. blaze answers; no rustc or cargo runs.
    Downloads blaze's Rust once when it is not there yet."""
    return run(f"`{blaze} toolchain --json`", [str(blaze), "toolchain", "--json"], ROOT, env, 900,
               check=True).stdout.strip()


class Engine(ABC):
    """The blaze under test. `Engine.of(spec)` gives the one of `spec.kind`."""

    def __init__(self, spec: EngineSpec):
        self.spec = spec
        self.blaze = Path(spec.blaze).resolve()
        if not self.blaze.is_file():
            raise Failed(f"engine.blaze: no file {self.blaze}")
        if spec.backend != "cranelift":
            raise Failed(f"engine.backend {spec.backend!r}: v2 picks each unit's backend from its profile "
                         "(opt-level), and BLAZE_BACKEND, BLAZE_OPT and BLAZE_EMIT are gone: the code "
                         "generator refuses every unit while one is set (CHANGES_CODE in "
                         "crates/hir-ty-codegen/v2/backend.rs); build with `--release` for LLVM")
        if spec.dev_backend != "default":
            if spec.backend != "cranelift":
                raise Failed(f"engine.dev_backend {spec.dev_backend!r} with engine.backend {spec.backend!r}: "
                             "BLAZE_BACKEND=llvm compiles every function with LLVM, so there is no dev tier to force")
            raise Failed(f"engine.dev_backend {spec.dev_backend!r}: {DEV_BACKEND_MISSING}")

    @staticmethod
    def of(spec: EngineSpec) -> "Engine":
        eng = V2(spec)
        eng.preflight()
        return eng

    @property
    def kind(self) -> Kind:
        return self.spec.kind

    def env(self, home: Path, target: Path | None = None, extra: dict[str, str] | None = None) -> dict[str, str]:
        """The environment of a `blaze` command whose daemon lives in `home` (`layout(home)`:
        socket, state, store, logs, build directories). There is no store to name: the daemon
        keeps it at `store_of(home)` and reads no other (`seed_store` copies one in). The build
        directory is the daemon's default under the home unless `target` is given
        (`CARGO_TARGET_DIR`); `build_dir` says where it is either way."""
        home.mkdir(parents=True, exist_ok=True, mode=0o700)
        env = scrubbed()
        env.update(BLAZE_HOME=str(home), BLAZE_STATS="1", PATH=f"{self.blaze.parent}{os.pathsep}{env.get('PATH', '')}")
        # blaze refuses every unit while BLAZE_BACKEND, BLAZE_OPT or BLAZE_EMIT is set (`scrubbed`
        # drops the caller's), so it gets none.
        if target is not None:
            env["CARGO_TARGET_DIR"] = str(target)
        env.update(extra or {})
        return env

    def command(self, step: str, command: str, args: list[str], ws: Path, env: dict[str, str],
                timeout: float = 7200) -> Ran:
        """`blaze COMMAND ARGS` in `ws`."""
        return run(step, [str(self.blaze), command, *args], ws, env, timeout)

    def stop(self, ws: Path, env: dict[str, str]) -> None:
        self.command("stopping the daemon", "stop", [], ws, env, 300)

    def build_dir(self, ws: Path, env: dict[str, str]) -> Path:
        """Where the builds of `ws` under `env` put their outputs (`debug/<program>`,
        `release/...`): the `CARGO_TARGET_DIR` `env` names, else the daemon's default for `ws`
        under its home (`default_build_dir`), which is there once a build has run."""
        if named := env.get("CARGO_TARGET_DIR"):
            return Path(named)
        return default_build_dir(Path(env["BLAZE_HOME"]), ws)

    def preflight(self) -> None:
        """Nothing here: an engine with assumptions to hold to its binary checks them."""

    @abstractmethod
    def not_blazes(self, ran: Ran) -> str | None:
        """None when blaze built what `ran` asked for; otherwise who did, and why."""

    def build(self, step: str, ws: Path, env: dict[str, str], args: Sequence[str] = (), command: str = "build",
              timeout: float = 7200) -> Ran:
        """`blaze COMMAND ARGS` (build, run, test, check), served by blaze: see `not_blazes`.
        A command that fails is returned as it is; what it said is the caller's to compare."""
        return self.command(step, command, list(args), ws, env, timeout)

    @abstractmethod
    def wait_ready(self, ws: Path, env: dict[str, str], timeout: float = 3600) -> float:
        """Seconds until the daemon has `ws` loaded and can serve it."""

    @abstractmethod
    def mark(self, home: Path) -> Mark:
        """Where the record of what the daemon of `home` did is now."""

    @abstractmethod
    def work_since(self, home: Path, mark: Mark) -> Work:
        """What the builds since `mark` did (see `Work`)."""

    @abstractmethod
    def codegen_inputs(self, ws: Path, env: dict[str, str]) -> dict[str, str]:
        """The input of every function's code generation in the last build of `ws`, by the
        instance's content key: what the oracle compares bit for bit (docs/v2/plan.md)."""

    @abstractmethod
    def critical_path_s(self, ws: Path, env: dict[str, str]) -> float:
        """The length in seconds of the longest chain of work the last cold build of `ws` waited on."""

    @abstractmethod
    def tables(self, ws: Path, env: dict[str, str]) -> dict[str, Table]:
        """The daemon's tables for `ws` by name (`token`, `syntax-node`, `hir-expr`,
        `mir-statement`, `memo`, `memo-edge`, `relocation`), for docs/v2/plan/design/data.md's size budgets."""

    @abstractmethod
    def trace(self, ws: Path, env: dict[str, str], to: Path) -> None:
        """Writes to `to` what the last build of `ws` left in the query database, as the
        deterministic CBOR of `blaze_v2_proof::Trace`, which `blaze-v2-proof` compares."""


class V2(Engine):
    """blaze v2 (docs/v2/plan.md): one daemon per machine, whose only interface is the agent
    API; the command line is its client. Every build is its own (there is no path to cargo), so
    a build that exits 0 was blaze's."""

    def not_blazes(self, ran: Ran) -> str | None:
        return None

    def wait_ready(self, ws: Path, env: dict[str, str], timeout: float = 3600) -> float:
        # `Request::Status` answers once the daemon serves requests; a view is opened by the
        # request that names it, so there is no loading of a workspace to wait out. `blaze
        # status` exits 0 with no daemon too (it starts none and says "no daemon is running",
        # crates/blaze-cargo/src/cli/daemon.rs `status`), so its exit says nothing: the answer
        # read is `blaze status --json`'s `daemon`, which is null until one answers.
        began = time.monotonic()
        said = "it was not asked"
        while time.monotonic() - began < timeout:
            try:
                if storelib.blaze_status(str(self.blaze), env, str(ws), 300).daemon is not None:
                    return time.monotonic() - began
                said = "`blaze status --json` says no daemon is running"
            except (storelib.Failed, subprocess.SubprocessError, OSError) as e:
                said = f"{type(e).__name__}: {e}"
            time.sleep(0.1)
        raise Failed(f"the daemon did not answer `blaze status --json` for {ws} in {timeout:.0f} s: {said}")

    def preflight(self) -> None:
        """What this harness assumes of the binary, held to the binary before any build; each
        miss is a `Failed` naming the assumption and what the binary said instead:

        - `blaze --version` exits 0 and names a version (kept as `self.version`);
        - every command the v2 scripts run is listed by `blaze --help`, in the form they run it
          (`HELP_LISTS`);
        - `blaze status --json`, asked with a home of its own and no daemon, exits 0, reads as
          storelib's format, says no daemon runs, and names the home, the state directory and
          the socket `layout` names (the daemon's rule, storelib.Layout)."""
        with tempfile.TemporaryDirectory(prefix="bz-pre-", dir="/tmp" if os.path.isdir("/tmp") else None) as tmp:
            home = Path(os.path.realpath(tmp)) / "h"
            env = self.env(home)
            what = f"preflight of {self.blaze}"
            said = run(f"{what}: `blaze --version`", [str(self.blaze), "--version"], home, env, 60, check=True).stdout
            words = said.split()
            if len(words) < 2 or words[0] != "blaze":
                raise Failed(f"{what}: `blaze --version` said {said.strip()!r}, not `blaze <version> ...`")
            self.version = said.strip()
            listed = run(f"{what}: `blaze --help`", [str(self.blaze), "--help"], home, env, 60, check=True).stdout
            missing = [name for name in HELP_LISTS if not help_lists(listed, name)]
            if missing:
                raise Failed(f"{what} ({self.version}): `blaze --help` lists no "
                             f"{', '.join(f'`{name}`' for name in missing)}, which the harness runs (engine.py and "
                             "the scripts over it); the harness is written against another blaze's commands")
            try:
                status = storelib.blaze_status(str(self.blaze), env, str(home))
            except (storelib.Failed, subprocess.SubprocessError, OSError) as e:
                raise Failed(f"{what} ({self.version}): `blaze status --json` with BLAZE_HOME={home}: {e}") from None
            if status.daemon is not None:
                raise Failed(f"{what}: a daemon (pid {status.daemon.pid}) answers in the new home {home}")
            want = layout(home)
            for name, got, wanted in (("home", status.home, want.home), ("state", status.state, want.state),
                                      ("socket", status.socket, want.socket)):
                if os.path.realpath(got) != os.path.realpath(wanted):
                    raise Failed(f"{what} ({self.version}): with BLAZE_HOME={home}, `blaze status --json` says its "
                                 f"{name} is {got}, and the harness lays out {wanted} (storelib.layout): the "
                                 "daemon's rule for where it keeps things moved, and the layout has to follow it")

    def __init__(self, spec: EngineSpec):
        super().__init__(spec)
        # `blaze --version`'s line, once `preflight` has asked.
        self.version = ""
        # Each build, test and run through `build`, by its daemon's home: its step and its
        # `blaze-work` line, or None where it printed none.
        self.worked: dict[str, list[tuple[str, dict[str, object] | None]]] = {}
        # Beside each of those, the `blaze-report` lines the command printed.
        self.reported: dict[str, list[list[str]]] = {}
        # Every command through `build`, in order: its step and its seconds, for a script to
        # say where its time went.
        self.took: list[tuple[str, float]] = []

    def build(self, step: str, ws: Path, env: dict[str, str], args: Sequence[str] = (), command: str = "build",
              timeout: float = 7200) -> Ran:
        args = dev_backend_args(self.spec.dev_backend, command, args)
        ran = self.command(step, command, list(args), ws, env, timeout)
        self.took.append((step, ran.seconds))
        if command in WORKING:
            label = f"{step} (`{' '.join(['blaze', command, *args])}`)"
            lines = worked_lines(ran.stdout) if "--message-format=json" in args else []
            if len(lines) > 1:
                raise Failed(f"{label}: {len(lines)} `blaze-work` lines; a request reports what it did once")
            self.worked.setdefault(env["BLAZE_HOME"], []).append((label, lines[0] if lines else None))
            self.reported.setdefault(env["BLAZE_HOME"], []).append(report_lines(ran.stdout))
        return ran

    def mark(self, home: Path) -> Mark:
        return Mark({"requests": len(self.worked.get(str(home), []))})

    def work_since(self, home: Path, mark: Mark) -> Work:
        since = self.worked.get(str(home), [])[mark.sizes["requests"]:]
        if not since:
            raise Failed(f"what the daemon of {home} did: no build, test or run since the mark")
        lines = [(label, required(label, line)) for label, line in since]

        def total(field: str) -> int:
            return sum(work_count(label, line, field) for label, line in lines)

        # A first link of an image none was kept of is not a fallback; one laid out whole
        # although an image was kept is.
        if any("links" not in line for _, line in lines):
            raise Failed(f"what the daemon of {home} did: a `blaze-work` line without `links`")
        links = [(label, it) for label, line in lines for it in cast(list[dict[str, Any]], line["links"])]
        full = [(label, it) for label, it in links if it["how"] == "full"]
        first = [(label, it) for label, it in links if it["how"] == "first"]
        if full:
            label, it = full[0]
            link = f"placed whole: {label}: {it['path']} ({', '.join(it['why'])})"
        elif first:
            label, it = first[0]
            link = f"first link: {label}: {it['path']}"
        else:
            link = "patched in place"
        reports = [text for texts in self.reported.get(str(home), [])[mark.sizes["requests"]:] for text in texts]
        return Work(compiled=total("compiled"), reran=total("reran"), found=total("found"), link=link,
                    said="\n".join([*(json.dumps(line, sort_keys=True) for _, line in lines), *reports]),
                    green=total("green"), reports=len(reports), causes=tuple(reports))

    def codegen_inputs(self, ws: Path, env: dict[str, str]) -> dict[str, str]:
        # `blaze codegen-inputs --format=json` (`blaze_ui::api::codegen_inputs_json`): one object
        # from each instance key's hex to its code hash's hex.
        ran = self.command("the codegen inputs", "codegen-inputs", ["--format=json"], ws, env, 600)
        inputs = json_answer("the codegen inputs", ran)
        if not isinstance(inputs, dict) or not all(isinstance(v, str) for v in cast(dict[str, object], inputs).values()):
            raise Failed(f"`blaze codegen-inputs --format=json` printed no object of hex strings:\n{ran.said()}")
        return cast(dict[str, str], inputs)

    def critical_path_s(self, ws: Path, env: dict[str, str]) -> float:
        kept = self.worked.get(env["BLAZE_HOME"], [])
        if not kept:
            raise Failed(f"the critical path of the last build of {ws}: no build of it went through the harness")
        label, line = kept[-1]
        return work_count(label, required(label, line), "critical_path_us") / 1e6

    def tables(self, ws: Path, env: dict[str, str]) -> dict[str, Table]:
        # `blaze tables --format=json`: `{"<name>": {"elements": N, "bytes": N}}`.
        ran = self.command("the tables", "tables", ["--format=json"], ws, env, 600)
        answer = json_answer("the tables", ran)
        if not isinstance(answer, dict):
            raise Failed(f"`blaze tables --format=json` printed no object:\n{ran.said()}")
        out: dict[str, Table] = {}
        for name, sizes in cast(dict[str, object], answer).items():
            label = f"`blaze tables` table {name}"
            if not isinstance(sizes, dict):
                raise Failed(f"{label}: {sizes!r} is not {{\"elements\": N, \"bytes\": N}}")
            sizes = cast(dict[str, object], sizes)
            out[name] = Table(work_count(label, sizes, "elements"), work_count(label, sizes, "bytes"))
        return out

    def trace(self, ws: Path, env: dict[str, str], to: Path) -> None:
        # `blaze trace --out PATH` (`blaze_ui::api::trace_cbor`): the CBOR of `blaze_v2_proof::Trace`.
        to.unlink(missing_ok=True)
        self.command("the trace", "trace", ["--out", str(to)], ws, env, 600).checked("the trace")
        if not to.is_file():
            raise Failed(f"`blaze trace --out {to}` exited 0 and wrote no file")


# The commands the v2 scripts run, each as `blaze --help` lists it (crates/blaze-cargo/src/cli.rs,
# `HELP`): `preflight` refuses a binary that lists one of them otherwise.
HELP_LISTS = ("build", "check", "run", "test", "stop", "status", "toolchain", "daemon",
              "codegen-inputs --format=json", "trace --out PATH", "tables --format=json", "debug sql")


def help_lists(listed: str, name: str) -> bool:
    """Whether `blaze --help`'s text `listed` has a line for the command `name`: the line
    starts with it, whole (`build, b`, `debug sql QUERY`; `tables` is not `tables-of`)."""
    for line in listed.splitlines():
        text = line.strip()
        if text.startswith(name) and (len(text) == len(name) or text[len(name)] in " ,"):
            return True
    return False


def profile_of(command: str, args: Sequence[str]) -> str:
    """The cargo profile `blaze COMMAND ARGS` builds with, as cargo picks it: `--profile`,
    `--release`, else `test` for `test` and `bench` for `bench`, else `dev`."""
    named: str | None = None
    words = list(args[:args.index("--")] if "--" in args else args)
    for at, word in enumerate(words):
        if word in ("--release", "-r"):
            named = named or "release"
        elif word == "--profile" and at + 1 < len(words):
            named = words[at + 1]
        elif word.startswith("--profile="):
            named = word.removeprefix("--profile=")
    return named or {"test": "test", "bench": "bench"}.get(command, "dev")


def dev_backend_args(dev_backend: DevBackend, command: str, args: Sequence[str]) -> tuple[str, ...]:
    """`args` of `blaze COMMAND`, with the dev tier forced to `dev_backend` through cargo's own
    setting (`-Zcodegen-backend` and the profile's `codegen-backend`), before any `--`;
    "default" and a command that compiles nothing (`check`, `status`) leave them as they are."""
    if dev_backend == "default" or command not in WORKING:
        return tuple(args)
    profile = profile_of(command, args)
    forced = ("-Zcodegen-backend", "--config", f'profile.{profile}.codegen-backend="{dev_backend}"')
    at = args.index("--") if "--" in args else len(args)
    return (*args[:at], *forced, *args[at:])


def json_answer(step: str, ran: Ran) -> object:
    ran.checked(step)
    try:
        return json.loads(ran.stdout)
    except json.JSONDecodeError as e:
        raise Failed(f"{step}: `{' '.join(ran.argv)}` printed no JSON ({e}):\n{ran.said()}") from None


def worked_lines(stdout: str) -> list[dict[str, object]]:
    """The `blaze-work` lines of a command's `--message-format=json` output."""
    found: list[dict[str, object]] = []
    for text in stdout.splitlines():
        if not text.startswith("{") or '"blaze-work"' not in text:
            continue
        try:
            line = json.loads(text)
        except json.JSONDecodeError:
            continue
        if isinstance(line, dict) and cast(dict[str, object], line).get("reason") == "blaze-work":
            found.append(cast(dict[str, object], line))
    return found


def report_lines(stdout: str) -> list[str]:
    """The `blaze-report` lines of a command's `--message-format=json` output that say why its
    build recomputed steps (a report whose rule is one of `why:...`, docs/v2/plan/design/why.md):
    one for each root cause, none when it recomputed nothing. Reports of other rules are not
    of work done."""
    found: list[str] = []
    for text in stdout.splitlines():
        if not text.startswith("{") or '"blaze-report"' not in text or '"why:' not in text:
            continue
        try:
            line = json.loads(text)
        except json.JSONDecodeError:
            continue
        if isinstance(line, dict) and cast(dict[str, object], line).get("reason") == "blaze-report":
            found.append(text)
    return found


def required(label: str, line: dict[str, object] | None) -> dict[str, object]:
    """`line`, checked whole; a request without one is a `Failed` naming it."""
    if line is None:
        raise Failed(f"{label}: no `blaze-work` line, so what it did is not known (it is printed only under "
                     "`--message-format=json`, and every build, test and run that runs reports it)")
    for field_name in WORK_COUNTS:
        work_count(label, line, field_name)
    if line.get("link") not in ("patched", "placed"):
        raise Failed(f"{label}: `blaze-work` has link {line.get('link')!r}, not \"patched\" or \"placed\": {line}")
    return line


def work_count(label: str, line: dict[str, object], field_name: str) -> int:
    value = line.get(field_name)
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise Failed(f"{label}: `blaze-work` has {field_name} {value!r}, not a count: {line}")
    return value


def platform_name() -> str:
    return {"linux": "linux", "darwin": "macos", "win32": "windows"}.get(sys.platform, sys.platform)
