"""storerevisions.py: a fresh daemon on a checkout edited before it started, or on the undo of
that edit, gets nothing stale from a store another revision filled. Linux and macOS.

    uv run --frozen python harness/storerevisions.py '{"blaze_bin": "target/release/blaze-engine"}'
    uv run --frozen python harness/storerevisions.py '{"blaze_bin": "...", "cases": ["leaf", "layout"], "jobs": 2}'

`blaze_bin` names the blaze-engine a build made; `blaze` is found beside it. `cases` picks some
of CASES (all of them by default), `jobs` how many cases run at once (3; each runs two daemons at a time). The JSON is inline, a file's
path, or `-` for stdin. Python 3.13, from the workspace at the repository's root. The backend is
the environment's (BLAZE_BACKEND), as for every harness.

Today the store keys a checkout crate's functions by the crate's source hash, so any edit makes
a fresh worker lower the crate again. A per-function key would let it reuse what an edit did not
touch (#1336, #1291); this is that key's acceptance test. Each case changes one thing a
function's code depends on without the function's own text changing: a callee's body, an inline
helper folded into a caller and into a constant's array length, a public return type with a
caller in another crate, an import rebound to another function, a module constant a `match`
reads, the field a method reads, a struct's layout, a constant table's contents, layout and enum
discriminants. For each:

- checkout B, a copy of A at another path, is edited before any daemon has seen it, and built
  from a copy of the store holding A only;
- B is also built cold, in a store holding the standard library only: what B has to print;
- the undo, B's copy with the edit taken back, is built from that store, which holds B only and
  nothing of A, so no exact hit for A can hide a dependency the key left out.

Each build is a fresh daemon with a home of its own and an empty target directory, and the
program runs with no arguments and with one. B from A's store has to print what B prints cold,
the undo what A printed, and the edit has to change what is printed (else the case tests
nothing). It does not ask for anything to be reused: it prints what the workers say they found
in the store and compiled, for when #1291 makes that more than nothing.

Adapted from astral-experiments/blaze#66, #69, #70, #74 and #78 (Charlie Marsh), whose
`test:storerevisions` was the check for their per-function proofs: the fixture and the edit-B-
first, undo-from-B-only design are theirs. Their reference was a clean cargo build; here it is
blaze's own cold build, so the check runs nothing of rustc's, and it asserts no reuse.
"""

import asyncio
import os
import re
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


@dataclass(frozen=True)
class Input:
    blaze_bin: str
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
STORE_LINE = re.compile(r"^store: (\d+) functions found, (\d+) compiled and added$", re.MULTILINE)


class Failed(Exception):
    """A step that went wrong, said whole."""


@dataclass(frozen=True)
class Env:
    bin_dir: Path
    root: Path

    def of(self, home: Path, store: Path) -> dict[str, str]:
        env = dict(os.environ, PATH=f"{self.bin_dir}{os.pathsep}{os.environ['PATH']}", BLAZE_HOME=str(home),
                   BLAZE_CACHE=str(store), BLAZE_STATS="1", RUST_BACKTRACE="0")
        for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
            env.pop(name, None)
        return env


async def run(step: str, argv: list[str], cwd: Path, env: dict[str, str], secs: float) -> str:
    """What `argv` printed, both streams; a Failed naming the step, the command, its exit status
    and the end of what it said when it fails or takes longer than `secs`."""
    proc = await asyncio.create_subprocess_exec(*argv, cwd=cwd, env=env, stdout=asyncio.subprocess.PIPE,
                                                stderr=asyncio.subprocess.STDOUT)
    try:
        async with asyncio.timeout(secs):
            out, _ = await proc.communicate()
    except TimeoutError:
        proc.kill()
        out, _ = await proc.communicate()
        said = out.decode(errors="replace")
        raise Failed(f"{step}: `{' '.join(argv)}` in {cwd} took more than {secs:.0f} s:\n{said[-3000:]}") from None
    said = out.decode(errors="replace")
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


@dataclass(frozen=True)
class Built:
    printed: tuple[str, str]
    found: int
    compiled: int


async def build(env: Env, ws: Path, store: Path, label: str, exe: str) -> Built:
    """A fresh daemon of its own home builds `ws` from `store` and stops; what the program prints
    with no argument and with one, and what its workers say they took from the store."""
    if (ws / "target").exists():
        raise Failed(f"{label}: {ws} has a target directory before its first build")
    home = Path(tempfile.mkdtemp(prefix="h", dir=env.root))
    variables = env.of(home, store)
    try:
        await run(f"{label}: the first build", ["blaze", "build"], ws, variables, BUILD_SECS)
        # The first build can come before the daemon is up; the program is the one it built.
        async with asyncio.timeout(BUILD_SECS):
            while "[blaze]" not in await run(f"{label}: a build the daemon serves", ["blaze", "build"], ws, variables,
                                             BUILD_SECS):
                await asyncio.sleep(0.5)
        program = ws / "target" / "debug" / exe
        printed = (await run(f"{label}: the program", [str(program)], ws, variables, RUN_SECS),
                   await run(f"{label}: the program with an argument", [str(program), "second"], ws, variables, RUN_SECS))
        # What the daemon does after a build (writing the store) is done before it is stopped,
        # so the store a later build copies holds all of this one.
        async with asyncio.timeout(IDLE_SECS):
            while re.search(r"loading|building",
                            await run(f"{label}: the daemon's status", ["blaze", "status"], ws, variables, IDLE_SECS)):
                await asyncio.sleep(0.25)
    finally:
        await run(f"{label}: stopping the daemon", ["blaze", "stop"], ws, variables, IDLE_SECS)
    logs = "".join(p.read_text(errors="replace") for p in sorted(home.glob("*/*/log")))
    counts = [(int(f), int(c)) for f, c in STORE_LINE.findall(logs)]
    return Built(printed, sum(f for f, _ in counts), sum(c for _, c in counts))


async def case(env: Env, name: Case, a: Path, only_a: Path, std_only: Path, original: Built,
               jobs: asyncio.Semaphore) -> list[str]:
    """What is wrong in case `name`, if anything; a line of what it found either way."""
    file, old, new = CASES[name]
    at = env.root / name
    edited = checkout(a, at / "b")
    edit(edited, file, old, new)
    cold = checkout(edited, at / "b-cold")
    from_a, only_b = at / "store-from-a", at / "store-only-b"
    shutil.copytree(only_a, from_a)
    shutil.copytree(std_only, only_b)
    async with jobs:
        async with asyncio.TaskGroup() as group:
            forward = group.create_task(build(env, edited, from_a, f"{name}: B from A's store", "store_app"))
            reference = group.create_task(build(env, cold, only_b, f"{name}: B cold", "store_app"))
        undo = checkout(cold, at / "undo")
        edit(undo, file, new, old)
        restored = await build(env, undo, only_b, f"{name}: the undo from B's store", "store_app")
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
    engine = Path(args.blaze_bin).resolve()
    if not (engine.parent / "blaze").is_file():
        fail(SCRIPT, f"blaze_bin: no blaze beside {engine}")
    if args.jobs < 1:
        fail(SCRIPT, f"jobs: at least 1, not {args.jobs}")
    if sys.platform not in ("linux", "darwin"):
        fail(SCRIPT, f"Linux and macOS only, not {sys.platform}")
    # A daemon's socket is under its home, and a socket's path has to be short: macOS's
    # temporary directory is too long a start.
    env = Env(engine.parent, Path(tempfile.mkdtemp(prefix="bz-revs-", dir="/tmp")).resolve())
    print(f"work: {env.root}", flush=True)
    try:
        std_only = env.root / "store-std"
        await build(env, write(env.root / "seed" / "ws", SEED), std_only, "the standard library's store", "std_seed")
        a, only_a = write(env.root / "a" / "ws", FILES), env.root / "store-only-a"
        shutil.copytree(std_only, only_a)
        original = await build(env, a, only_a, "A", "store_app")
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
