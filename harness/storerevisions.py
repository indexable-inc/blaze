"""storerevisions.py: a fresh v2 daemon on a checkout edited before it started, or on the undo of
that edit, gets nothing stale from a store another revision filled. Linux and macOS.

    uv run --frozen python harness/storerevisions.py '{"blaze": "target/release/blaze"}'
    uv run --frozen python harness/storerevisions.py '{"blaze": "...", "cases": ["leaf", "layout"], "jobs": 2}'

`blaze` is the v2 command line. `cases` picks some of CASES (all of them by default), `jobs` how
many cases run at once (3; each runs two daemons at a time). The JSON is inline, a file's path,
or `-` for stdin. Python 3.13, from the workspace at the repository's root.

v2's store keys every value by its schema, the code that made it and every input it was made
from (#1439, docs/v2-store.md "Keys"): a key that misses an input is a bug, and this is that
bug's acceptance test (#1336, #1291). Each case changes one thing a function's code depends on
without the function's own text changing: a callee's body, an inline helper folded into a
caller and into a constant's array length, a public return type with a caller in another
crate, an import rebound to another function, a module constant a `match` reads, the field a
method reads, a struct's layout, a constant table's contents, layout and enum discriminants.
A daemon keeps its store in `<home>/v2/store`, so a build "from a store" is a fresh daemon in a
home of its own whose store starts as a copy of that one. For each case:

- checkout B, a copy of A at another path, is edited before any daemon has seen it, and built
  from a copy of the store holding A only;
- B is also built cold, from a copy of a store holding the standard library only: what B has
  to print;
- the undo, B's copy with the edit taken back, is built from a copy of that cold build's store,
  which holds B only and nothing of A, so no exact hit for A can hide an input a key left out.

Each build is a fresh daemon with a home of its own and an empty target directory, and the
program runs with no arguments and with one. B from A's store has to print what B prints cold,
the undo what A printed, and the edit has to change what is printed (else the case tests
nothing). It does not ask for anything to be reused: it prints what each build's `blaze-work`
line says it found in the store and compiled.

Adapted from astral-experiments/blaze#66, #69, #70, #74 and #78 (Charlie Marsh), whose
`test:storerevisions` was the check for their per-function proofs: the fixture and the edit-B-
first, undo-from-B-only design are theirs. Their reference was a clean cargo build; here it is
blaze's own cold build, so the check runs nothing of rustc's, and it asserts no reuse.
"""

import asyncio
import json
import os
import shutil
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Literal, get_args

from typed_input import fail, parse

SCRIPT = "storerevisions.py"
Case = Literal[
    "leaf", "inline", "signature", "binding", "tag", "field", "layout", "table", "table-layout", "table-tag"
]
BUILD_SECS = 900
RUN_SECS = 60
IDLE_SECS = 120
# Read by blaze or cargo from the caller's environment; set here alone.
SCRUBBED = ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC", "RUSTC_WRAPPER",
            "BLAZE_HOME", "XDG_RUNTIME_DIR")


@dataclass(frozen=True)
class Input:
    blaze: str
    cases: tuple[Case, ...] = get_args(Case)
    jobs: int = 3


LIB = """pub mod fields;
pub mod tables;
mod tags;
use cache_leaf as selected;

impl fields::Header {
    #[inline(never)]
    pub fn cache_field(&self) -> u8 { self.value }
    #[inline(never)]
    pub fn cache_contains(&self, byte: u8) -> bool { self.value <= byte && byte <= self.prefix }
}

impl tables::Folder {
    #[inline(never)]
    pub fn cache_folder() -> Result<Self, tables::FoldError> {
        Ok(Self { table: tables::FOLDS, last: None, flag: tables::Flag::Idle, next: 0 })
    }
}

#[inline(never)]
pub fn cache_leaf(x: i32) -> i32 { x + 1 }
#[inline(never)]
pub fn cache_leaf_caller(x: i32) -> i32 { selected(x) + 10 }
#[inline(always)]
pub const fn cache_inline(x: i32) -> i32 { x + 2 }
#[inline(never)]
pub fn cache_inline_caller(x: i32) -> i32 { cache_inline(x) + 20 }
const INLINE_COUNT: usize = cache_inline(0) as usize;
#[inline(never)]
pub fn cache_embedded() -> i32 { [0_u8; INLINE_COUNT].len() as i32 }
#[inline(never)]
pub fn cache_public(x: u8) -> u8 { (128u8 | x) as _ }
#[inline(never)]
pub fn cache_sibling(x: i32) -> i32 { x + 40 }
#[inline(never)]
pub fn cache_if(x: i32) -> i32 { if x < 0 { x - 9 } else { x + 31 } }
#[inline(never)]
pub fn cache_switch(x: i32) -> i32 {
    match x { tags::TAG => x + 41, -1 => x - 11, _ => x + 50 }
}
#[inline(never)]
pub fn cache_arm(x: i32) -> i32 { if x < 0 { cache_inline(x) + 20 } else { 0 } }
"""
FIELDS = "#[repr(C)] pub struct Header { pub prefix: u8, pub value: u8, pub tail: u32 }\n"
TABLES = """pub const FOLDS: &[(char, &[char])] = &[('K', &['k', 'K']), ('S', &['s', 'ſ'])];
#[repr(u8)]
pub enum Flag { Idle = 1, Other = 7 }
#[repr(C)]
pub struct Folder { pub last: Option<char>, pub flag: Flag, pub table: &'static [(char, &'static [char])], pub next: usize }
pub struct FoldError(());
"""
MAIN = """#[inline(never)]
fn cache_public_caller(x: u8) -> i32 { store_dep::cache_public(x) as i32 }
fn main() {
    let x = std::env::args().len() as i32;
    let signed = x - 2;
    // Both layouts are eight bytes of integers. Only the repr(C) value is at byte one.
    let mut bytes = [x as u8 + 100; 8];
    bytes[1] = x as u8 + 20;
    bytes[4] = x as u8 + 150;
    let header: store_dep::fields::Header = unsafe { std::mem::transmute(bytes) };
    let needle = if x == 1 { 50 } else { 125 };
    let Ok(folder) = store_dep::tables::Folder::cache_folder() else { std::process::exit(2) };
    let folded = folder.table[folder.next].1[1] as u32;
    println!("{} {} {} {} {} {} {} {} {} {} {} {} {} {}", store_dep::cache_leaf_caller(x), store_dep::cache_inline_caller(x),
             header.cache_field(), header.cache_contains(needle),
             folded, std::mem::offset_of!(store_dep::tables::Folder, table), folder.flag as u8,
             cache_public_caller(x as u8), store_dep::cache_sibling(x), store_dep::cache_if(signed),
             store_dep::cache_switch(x), store_dep::cache_switch(signed), store_dep::cache_arm(signed),
             store_dep::cache_embedded());
}
"""
FILES = {
    "Cargo.toml": """[package]
name = "store_app"
version = "0.1.0"
edition = "2021"
[workspace]
members = ["lib"]
[dependencies]
store_dep = { path = "lib" }
[profile.dev]
overflow-checks = false
""",
    "lib/Cargo.toml": '[package]\nname = "store_dep"\nversion = "0.1.0"\nedition = "2021"\n',
    "lib/src/lib.rs": LIB,
    "lib/src/fields.rs": FIELDS,
    "lib/src/tables.rs": TABLES,
    "lib/src/tags.rs": "pub const TAG: i32 = 1;\n",
    "src/main.rs": MAIN,
}
# A workspace that is neither A nor B, whose build leaves the standard library in a store: each
# store below starts as a copy of it, so no build compiles the library again.
SEED = {
    "Cargo.toml": '[package]\nname = "std_seed"\nversion = "0.1.0"\nedition = "2021"\n',
    "src/main.rs": 'fn main() {\n    println!("{} {}", std::env::args().len() as i32, true);\n}\n',
}
# (file, old, new): what the edit of checkout B is, and what its undo takes back. The functions
# whose code depends on it are named in the comment; none of their own text changes.
CASES: dict[Case, tuple[str, str, str]] = {
    # cache_leaf_caller, by its callee.
    "leaf": ("lib/src/lib.rs", "{ x + 1 }", "{ x + 4 }"),
    # cache_inline_caller and cache_arm, which inline it, and cache_embedded, whose array length
    # const evaluation took from it.
    "inline": ("lib/src/lib.rs", "{ x + 2 }", "{ x + 7 }"),
    # cache_public_caller in the other crate, by the callee's signature.
    "signature": ("lib/src/lib.rs", "cache_public(x: u8) -> u8", "cache_public(x: u8) -> i8"),
    # cache_leaf_caller: the same name now resolves to another function of the same signature.
    "binding": ("lib/src/lib.rs", "use cache_leaf as selected;", "use cache_sibling as selected;"),
    # cache_switch, by a constant of another file it matches on.
    "tag": ("lib/src/tags.rs", "TAG: i32 = 1", "TAG: i32 = 2"),
    # cache_field reads the other field; cache_contains, beside it, does not change.
    "field": ("lib/src/lib.rs", "{ self.value }", "{ self.prefix }"),
    # cache_field and cache_contains, by the offsets of the fields they read.
    "layout": ("lib/src/fields.rs", "#[repr(C)]", "#[repr(Rust)]"),
    # cache_folder, by the contents of the constant table it returns.
    "table": ("lib/src/tables.rs", "&['k', 'K']", "&['k', 'Å']"),
    # cache_folder, by the layout of the struct it builds.
    "table-layout": ("lib/src/tables.rs", "#[repr(C)]\npub struct Folder", "#[repr(Rust)]\npub struct Folder"),
    # cache_folder, by the discriminant it writes.
    "table-tag": ("lib/src/tables.rs", "Idle = 1, Other = 7", "Idle = 7, Other = 1"),
}
assert all(FILES[file].count(old) == 1 for file, old, _ in CASES.values())


class Failed(Exception):
    """A step that went wrong, said whole."""


@dataclass(frozen=True)
class Env:
    blaze: Path
    root: Path

    def of(self, home: Path) -> dict[str, str]:
        env = {name: value for name, value in os.environ.items() if name not in SCRUBBED}
        env.update(BLAZE_HOME=str(home), RUST_BACKTRACE="0")
        return env


async def run(step: str, argv: list[str], cwd: Path, env: dict[str, str], secs: float) -> str:
    """What `argv` printed on stdout, then stderr; a Failed naming the step, the command, its
    exit status and the end of what it said when it fails or takes longer than `secs`."""
    proc = await asyncio.create_subprocess_exec(*argv, cwd=cwd, env=env, stdout=asyncio.subprocess.PIPE,
                                                stderr=asyncio.subprocess.PIPE)
    try:
        async with asyncio.timeout(secs):
            out, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        out, err = await proc.communicate()
        said = (out + err).decode(errors="replace")
        raise Failed(f"{step}: `{' '.join(argv)}` in {cwd} took more than {secs:.0f} s:\n{said[-3000:]}") from None
    said = out.decode(errors="replace") + err.decode(errors="replace")
    if proc.returncode != 0:
        raise Failed(f"{step}: `{' '.join(argv)}` in {cwd} exited {proc.returncode}:\n{said[-3000:]}")
    return said


def write(ws: Path, files: dict[str, str]) -> Path:
    for name, text in files.items():
        path = ws / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    return ws


def checkout(source: Path, at: Path) -> Path:
    """A copy of the checkout `source` at another path, with no target directory."""
    return Path(shutil.copytree(source, at, ignore=shutil.ignore_patterns("target")))


def edit(ws: Path, file: str, old: str, new: str) -> None:
    path = ws / file
    text = path.read_text()
    if text.count(old) != 1:
        raise Failed(f"{ws}: {old!r} is not once in {file}")
    path.write_text(text.replace(old, new))


def messages(said: str, reason: str) -> list[dict[str, object]]:
    """The `--message-format=json` lines of `reason` in what a build printed."""
    out: list[dict[str, object]] = []
    for line in said.splitlines():
        if line.startswith("{") and reason in line:
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(message, dict) and message.get("reason") == reason:
                out.append(message)
    return out


@dataclass(frozen=True)
class Built:
    printed: tuple[str, str]
    found: int
    compiled: int
    # The home's store as the daemon left it, for a later build to start from a copy of.
    store: Path


async def build(env: Env, ws: Path, seed: Path | None, label: str, exe: str) -> Built:
    """A fresh daemon of its own home, its store a copy of `seed` (or empty), builds `ws` and
    stops; what the program prints with no argument and with one, and what the build says it
    took from the store and compiled."""
    if (ws / "target").exists():
        raise Failed(f"{label}: {ws} has a target directory before its first build")
    home = Path(tempfile.mkdtemp(prefix="h", dir=env.root))
    store = home / "v2" / "store"
    if seed is not None:
        shutil.copytree(seed, store)
    variables = env.of(home)
    blaze = str(env.blaze)
    try:
        said = await run(f"{label}: the build", [blaze, "build", "--message-format=json"], ws, variables, BUILD_SECS)
        programs = [m["executable"] for m in messages(said, "compiler-artifact")
                    if m.get("executable") and isinstance(m.get("target"), dict) and m["target"].get("name") == exe]
        if not programs:
            raise Failed(f"{label}: the build names no executable of {exe}:\n{said[-3000:]}")
        program = str(programs[-1])
        printed = (await run(f"{label}: the program", [program], ws, variables, RUN_SECS),
                   await run(f"{label}: the program with an argument", [program, "second"], ws, variables, RUN_SECS))
        worked = messages(said, "blaze-work")
        if len(worked) != 1:
            raise Failed(f"{label}: {len(worked)} `blaze-work` lines; a build says what it did once:\n{said[-3000:]}")
    finally:
        # `blaze stop` makes what the daemon keeps durable before it ends, so a copy of the
        # store taken after it holds all of this build.
        await run(f"{label}: stopping the daemon", [blaze, "stop"], ws, variables, IDLE_SECS)
    found, compiled = worked[0].get("found"), worked[0].get("compiled")
    if not isinstance(found, int) or not isinstance(compiled, int):
        raise Failed(f"{label}: the `blaze-work` line has no counts: {worked[0]}")
    return Built(printed, found, compiled, store)


async def case(env: Env, name: Case, a: Path, only_a: Path, std_only: Path, original: Built,
               jobs: asyncio.Semaphore) -> list[str]:
    """What is wrong in case `name`, if anything; a line of what it found either way."""
    file, old, new = CASES[name]
    at = env.root / name
    edited = checkout(a, at / "b")
    edit(edited, file, old, new)
    cold = checkout(edited, at / "b-cold")
    async with jobs:
        async with asyncio.TaskGroup() as group:
            forward = group.create_task(build(env, edited, only_a, f"{name}: B from A's store", "store_app"))
            reference = group.create_task(build(env, cold, std_only, f"{name}: B cold", "store_app"))
        undo = checkout(cold, at / "undo")
        edit(undo, file, new, old)
        restored = await build(env, undo, reference.result().store, f"{name}: the undo from B's store", "store_app")
    wrong: list[str] = []
    if reference.result().printed == original.printed:
        wrong.append(f"{name}: the edit changes nothing the program prints, {original.printed!r}: the case tests nothing")
    if forward.result().printed != reference.result().printed:
        wrong.append(f"{name}: B from A's store printed {forward.result().printed!r}, "
                     f"B cold {reference.result().printed!r}: code of A was reused for B")
    if restored.printed != original.printed:
        wrong.append(f"{name}: the undo from B's store printed {restored.printed!r}, "
                     f"A {original.printed!r}: code of B was reused for A")
    verdict = "WRONG" if wrong else "ok"
    print(f"{name:13} {verdict:5} B from A: {forward.result().found} found, {forward.result().compiled} compiled; "
          f"undo from B: {restored.found} found, {restored.compiled} compiled", flush=True)
    return wrong


async def main(args: Input) -> None:
    blaze = Path(args.blaze).resolve()
    if not blaze.is_file():
        fail(SCRIPT, f"blaze: no blaze at {blaze}")
    if args.jobs < 1:
        fail(SCRIPT, f"jobs: at least 1, not {args.jobs}")
    if sys.platform not in ("linux", "darwin"):
        fail(SCRIPT, f"Linux and macOS only, not {sys.platform}")
    # A daemon's socket is under its home, and a socket's path has to be short: macOS's
    # temporary directory is too long a start.
    env = Env(blaze, Path(tempfile.mkdtemp(prefix="bz-revs-", dir="/tmp")).resolve())
    print(f"work: {env.root}", flush=True)
    try:
        seeded = await build(env, write(env.root / "seed" / "ws", SEED), None, "the standard library's store", "std_seed")
        std_only = seeded.store
        a = write(env.root / "a" / "ws", FILES)
        original = await build(env, a, std_only, "A", "store_app")
        only_a = original.store
        print(f"A printed {original.printed!r}", flush=True)
        jobs = asyncio.Semaphore(args.jobs)
        async with asyncio.TaskGroup() as group:
            tasks = [group.create_task(case(env, name, a, only_a, std_only, original, jobs))
                     for name in dict.fromkeys(args.cases)]
    except* Failed as failed:
        for error in failed.exceptions:
            print(error, flush=True)
        sys.exit(f"{SCRIPT}: a build failed; everything is under {env.root}")
    wrong = [line for task in tasks for line in task.result()]
    for line in wrong:
        print(line, flush=True)
    if wrong:
        sys.exit(f"{SCRIPT}: {len(wrong)} wrong; everything is under {env.root}")
    print(f"{len(tasks)} cases, none wrong", flush=True)
    shutil.rmtree(env.root, ignore_errors=True)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>'\n\n{__doc__}")
    asyncio.run(main(parse(Input, sys.argv[1], SCRIPT)))
