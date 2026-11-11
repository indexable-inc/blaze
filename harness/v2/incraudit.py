"""incraudit.py: how close blaze's incremental path is to the least work an edit needs.

    uv run --frozen python harness/v2/incraudit.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"},
        "out": "/tmp/incraudit.json"}'
    uv run --frozen python harness/v2/incraudit.py '{"engine": {...}, "out": "...", "command": "build"}'
    uv run --frozen python harness/v2/incraudit.py '{"engine": {...}, "out": "...", "target": {"kind": "workspace",
        "path": ".", "base": {"package": "blaze-rng", "file": "crates/blaze-rng/src/lib.rs"},
        "mid": {"package": "blaze-radix", "file": "crates/blaze-radix/src/lib.rs"},
        "top": {"package": "blaze-ids", "file": "crates/blaze-ids/src/lib.rs"}}}'
    uv run --frozen python harness/v2/incraudit.py selftest

A workspace is built cold, then warm (a no-op build, whose trace is the baseline). Then, for each edit
of `EDITS`, the edit is written, the workspace built, the edit reverted, built again, and built once
more with nothing changed. After every build the harness reads what re-executed from blaze's own
records:

- `blaze trace --out PATH`: every result the view's last build ran or kept, with its fingerprint and
  its read edges, each with the fingerprint it saw (crates/blaze-v2-proof/src/wire.rs; read here
  with a small CBOR reader, no Rust needed). A key is re-executed when its `ran` bit is set: new
  (no result before the edit), a rerun, or a rerun to the same fingerprint (an early cutoff).
- the history through `blaze debug sql --json`: the build's `builds` row (wall time, memo hits
  and misses, the tree digest, the blaze binary's hash), its `query_counts` (each query function's
  executed, verified and reran counts), its `misses` (what `blaze why` says it computed again: codegen
  templates and units by item path, with the read or key part that changed) and its spans.

Each edit carries its ideal re-execution set (`Ideal`): per band of query kinds
(blaze_query::kinds' ranges: syntax, names, infer, expand, mir, codegen, link, ...), how many keys of
one kind may run, and which functions' codegen and which crates' units may be redone. The result
says, per edit, what actually ran against that ideal, the extra keys, and the read edge that pulled
each extra in:

- a rerun key: the first of its edges in the trace before the edit whose read now has another
  fingerprint (what blaze's verify walks, in read order, stopping at the first that differs); none
  changed is "reran though nothing it read changed" (an untracked read, or a verify bug);
- a new key (its key holds a changed content id): the key that asked for it in the trace after;
- the frontier: an extra key's kind over its allowance whose pulling read is of a kind within its
  allowance. That edge is where the cutoff should have held, so it is where to look first.

The no-op and revert checks hold re-execution to zero. Keys are content ids, so the results of
the state before the edit are still in the store after a revert. A revert that re-executes
anything is a finding too.

The scaffold the edits change is the same text for the fixture and a real workspace: a module
`incraudit` added to the end of the base crate's root file, `incraudit_mid` to the mid crate (a
dependent of base) and `incraudit_top` to the top crate (a dependent of mid; optional for a real
workspace). Edits keep byte lengths where they can (`Edit.shifts` says where they do not). So an
edit tests its own meaning, not positions moving, except for edit 1, which tests exactly that.

`command` "check" (the default, as asked) runs no codegen, so the codegen columns read zero by
construction there. "build" fills them.

The input is one JSON object (inline, a file's path, or `-` for stdin):

    engine    the blaze under test (engine.EngineSpec); v2 only (it needs `blaze trace` and the history)
    out       where the result JSON goes (report.py); the markdown goes beside it, `.md`
    duckdb    the DuckDB `blaze debug sql` runs, an absolute path; "": `nix build .#duckdb` in this tree
    target    {"kind": "fixture"} (default: three crates written by the script) or
              {"kind": "workspace", "path": DIR, "base": CRATE, "mid": CRATE, "top": CRATE|null}, each
              CRATE {"package": NAME, "file": ROOT FILE under DIR}; DIR is copied (its git-tracked and
              untracked-not-ignored files when it is a git checkout), never built in place
    command   "check" or "build"
    args      more arguments of every build (a workspace target gets `-p` for each of its crates too)
    edits     the edits to run by name (`1-comment`, ..., `9-generic-body`); empty: all
    tree      what the blaze under test was built from, a content digest, kept as given (rule 22)
    work      the directory everything goes in; "": a new one under /tmp
    keep      keep the work directory (true: the exact tree measured stays, rule 22)
    step_s    the bound of each command, in seconds (1800)
    history_s the bound on waiting for a build's history rows to be flushed, in seconds (60)
"""

import asyncio
import json
import re
import shutil
import subprocess
import sys
import time
from array import array
from collections import Counter
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal, cast

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
import engine
import report
from typed_input import fail, parse

SCRIPT = "incraudit.py"
PROPERTY = "incraudit"
TRACE_VERSION = 2
POLL_S = 0.25

type Role = Literal["base", "mid", "top"]
type Row = dict[str, object]


# The input.

@dataclass(frozen=True)
class Crate:
    package: str
    file: str


@dataclass(frozen=True)
class Target:
    kind: Literal["fixture", "workspace"] = "fixture"
    path: str = ""
    base: Crate | None = None
    mid: Crate | None = None
    top: Crate | None = None


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    duckdb: str = ""
    target: Target = field(default_factory=Target)
    command: Literal["check", "build"] = "check"
    args: tuple[str, ...] = ()
    edits: tuple[str, ...] = ()
    tree: str = ""
    work: str = ""
    keep: bool = True
    step_s: int = 1800
    history_s: int = 60


# The scaffold and the edits.

def lib_name(package: str) -> str:
    return package.replace("-", "_")


ALLOWS = "#[allow(missing_docs, dead_code, unused, clippy::all, clippy::pedantic, clippy::nursery)]"


def scaffold_base() -> str:
    return f"""
// incraudit scaffold: begin
{ALLOWS}
pub mod incraudit {{
    pub fn ia_comment_fn(x: u32) -> u32 {{
        let y = x * 3;
        y + 1
    }}

    pub fn ia_body_fn(x: u32) -> u32 {{ x + 1 }}

    fn ia_private(x: u32) -> u32 {{ u32::from(x) + 1 }}
    pub fn ia_priv_caller_a(x: u32) -> u32 {{ ia_private(x) as u32 }}
    pub fn ia_priv_caller_b(x: u32) -> u32 {{ (ia_private(x + 1) as u32).wrapping_mul(2) }}
    pub fn ia_unrelated(x: u32) -> u32 {{ x ^ 7 }}

    pub fn ia_pub_sig(x: u32) -> u32 {{ u32::from(x) * 5 }}

    /// incraudit doc: version one.
    pub fn ia_documented(x: u32) -> u32 {{ x + 9 }}

    pub fn ia_dep_body(x: u32) -> u32 {{ x + 11 }}

    pub fn ia_generic<T: Copy>(t: T, n: u32) -> (T, u32) {{ (t, n + 1) }}

    pub struct IaLaid {{ pub a: u32 }}
    pub fn ia_laid() -> IaLaid {{ IaLaid {{ a: 1 }} }}
    pub fn ia_read_laid(l: &IaLaid) -> u32 {{ l.a }}
    // incraudit: added items go here
}}
// incraudit scaffold: end
"""


def scaffold_mid(base: str) -> str:
    return f"""
// incraudit scaffold: begin
{ALLOWS}
pub mod incraudit_mid {{
    use {base}::incraudit::*;
    pub use {base}::incraudit::ia_dep_body;

    pub fn ia_mid_calls_body(x: u32) -> u32 {{ ia_body_fn(x) + 1 }}
    pub fn ia_mid_calls_pub(x: u32) -> u64 {{ u64::from(ia_pub_sig(x)) }}
    pub fn ia_mid_dep(x: u32) -> u32 {{ ia_dep_body(x) * 2 }}
    pub fn ia_mid_size() -> usize {{ core::mem::size_of::<IaLaid>() }}
    pub fn ia_mid_field() -> u32 {{ ia_read_laid(&ia_laid()) + ia_laid().a }}
    pub fn ia_mid_generic(x: u32) -> u32 {{ ia_generic(x, 1).1 }}
    pub fn ia_generic_again<T: Copy>(t: T, n: u32) -> (T, u32) {{ ia_generic(t, n) }}
    pub fn ia_mid_documented(x: u32) -> u32 {{ ia_documented(x) }}
    pub fn ia_mid_comment(x: u32) -> u32 {{ ia_comment_fn(x) }}
    pub fn ia_mid_priv(x: u32) -> u32 {{ ia_priv_caller_a(x) + ia_priv_caller_b(x) }}
    pub fn ia_mid_unrelated(x: u32) -> u32 {{ x.rotate_left(3) ^ ia_unrelated(x) }}
}}
// incraudit scaffold: end
"""


def scaffold_top(mid: str) -> str:
    return f"""
// incraudit scaffold: begin
{ALLOWS}
pub mod incraudit_top {{
    use {mid}::incraudit_mid as m;

    pub fn ia_top_calls_mid(x: u32) -> u64 {{ m::ia_mid_calls_pub(x) + 1 }}
    pub fn ia_top_dep(x: u32) -> u32 {{ m::ia_dep_body(x) }}
    pub fn ia_top_generic(x: u64) -> u64 {{ m::ia_generic_again(x, 3).0 }}
    pub fn ia_top_unrelated(x: u32) -> u32 {{ x + 100 }}
    pub fn ia_top_all(x: u32) -> u64 {{
        ia_top_calls_mid(x) + u64::from(ia_top_dep(x)) + ia_top_generic(u64::from(x))
            + u64::from(ia_top_unrelated(x)) + u64::from(m::ia_mid_calls_body(x)) + u64::from(m::ia_mid_dep(x))
            + m::ia_mid_size() as u64 + u64::from(m::ia_mid_field()) + u64::from(m::ia_mid_generic(x))
            + u64::from(m::ia_mid_documented(x)) + u64::from(m::ia_mid_comment(x)) + u64::from(m::ia_mid_priv(x))
            + u64::from(m::ia_mid_unrelated(x))
    }}
}}
// incraudit scaffold: end
"""


@dataclass(frozen=True)
class Ideal:
    """The least an edit needs re-executed. Per band, how many keys of one query kind may run (None:
    any); `functions`, the scaffold functions whose codegen may be redone; `units`, the crates whose
    unit may be redone. Bands not named here allow none, but inputs and directory trees, which
    follow the files."""

    says: str
    bands: tuple[tuple[str, int], ...]
    functions: tuple[str, ...] = ()
    units: tuple[Role, ...] = ("base",)

    def allows(self, band: str) -> int | None:
        if band in FREE_BANDS:
            return None
        return dict(self.bands).get(band, 0)


@dataclass(frozen=True)
class Edit:
    name: str
    what: str
    changes: tuple[tuple[str, str], ...]
    ideal: Ideal
    # Where the edit moves the bytes after it: "" when it keeps every length.
    shifts: str = ""


def bands(syntax: int = 1, facts: int = 0, names: int = 0, infer: int = 0, mir: int = 0, codegen: int = 0,
          link: int = 0, cargo: int = 0) -> tuple[tuple[str, int], ...]:
    return (("syntax", syntax), ("facts", facts), ("names", names), ("infer", infer), ("mir", mir),
            ("codegen", codegen), ("link", link), ("cargo", cargo))


EDITS: tuple[Edit, ...] = (
    Edit("1-comment", "a comment line added inside a fn body (ia_comment_fn), first in the scaffold",
         (("pub fn ia_comment_fn(x: u32) -> u32 {\n", "pub fn ia_comment_fn(x: u32) -> u32 {\n        // incraudit: a comment, one line more\n"),),
         Ideal("parse of that file only, zero semantic queries", bands(syntax=1)),
         shifts="every item after it in the file moves one line down"),
    Edit("2-body", "a fn body changed, its signature kept (ia_body_fn: x + 1 to x + 2), called from mid",
         (("ia_body_fn(x: u32) -> u32 { x + 1 }", "ia_body_fn(x: u32) -> u32 { x + 2 }"),),
         Ideal("that fn's inference, MIR and codegen only; dependents untouched (early cutoff)",
               bands(facts=1, infer=1, mir=1, codegen=1, link=1, cargo=1), functions=("ia_body_fn",))),
    Edit("3-private-sig", "a private fn's signature changed (ia_private: -> u32 to -> u64), two callers in its crate",
         (("fn ia_private(x: u32) -> u32 { u32::from(x) + 1 }", "fn ia_private(x: u32) -> u64 { u64::from(x) + 1 }"),),
         Ideal("its callers in the same crate only", bands(facts=3, infer=3, mir=3, codegen=3, link=1, cargo=1),
               functions=("ia_private", "ia_priv_caller_a", "ia_priv_caller_b"))),
    Edit("4-pub-sig", "a pub fn's signature changed (ia_pub_sig: -> u32 to -> u64), read by mid, mid's caller by top",
         (("pub fn ia_pub_sig(x: u32) -> u32 { u32::from(x) * 5 }", "pub fn ia_pub_sig(x: u32) -> u64 { u64::from(x) * 5 }"),),
         Ideal("the callers that read it, in dependent crates, with cutoff past them",
               bands(facts=2, infer=2, mir=2, codegen=2, link=1, cargo=1),
               functions=("ia_pub_sig", "ia_mid_calls_pub"), units=("base", "mid"))),
    Edit("5-add-struct", "a struct added at the end of the base module, which mid glob-imports",
         (("    // incraudit: added items go here\n", "    pub struct IaAdded { pub z: u8 }\n    // incraudit: added items go here\n"),),
         Ideal("only what its name resolution or trait solving can affect", bands(facts=1, names=2)),
         shifts="the end of the scaffold only"),
    Edit("6-add-field", "a field added to a struct (IaLaid gets b: u64) and its one constructor",
         (("pub struct IaLaid { pub a: u32 }", "pub struct IaLaid { pub a: u32, pub b: u64 }"),
          ("IaLaid { a: 1 }", "IaLaid { a: 1, b: 0 }")),
         Ideal("its layout and the users of the layout", bands(facts=2, infer=5, mir=4, codegen=4, link=1, cargo=1),
               functions=("ia_laid", "ia_read_laid", "ia_mid_size", "ia_mid_field"), units=("base", "mid")),
         shifts="the three items at the end of the scaffold"),
    Edit("7-doc", "a doc comment's words changed (ia_documented), its length kept",
         (("/// incraudit doc: version one.", "/// incraudit doc: version two."),),
         Ideal("nothing semantic (docs only)", bands(syntax=1))),
    Edit("8-dep-body", "a dependency's non-generic, non-inline body changed (ia_dep_body: x + 11 to x + 12), "
         "called from mid and top",
         (("ia_dep_body(x: u32) -> u32 { x + 11 }", "ia_dep_body(x: u32) -> u32 { x + 12 }"),),
         Ideal("zero re-codegen in dependents", bands(facts=1, infer=1, mir=1, codegen=1, link=1, cargo=1),
               functions=("ia_dep_body",))),
    Edit("9-generic-body", "a generic fn's body changed (ia_generic: n + 1 to n + 2), instantiated in mid (u32) and "
         "top (u64)",
         (("-> (T, u32) { (t, n + 1) }", "-> (T, u32) { (t, n + 2) }"),),
         Ideal("its instantiations in dependents only", bands(facts=1, infer=1, mir=3, codegen=3, link=1, cargo=1),
               functions=("ia_generic",), units=("base", "mid", "top"))),
)


# The fixture.

FIXTURE: dict[Role, str] = {"base": "audit_base", "mid": "audit_mid", "top": "audit_top"}


def filler(prefix: str, count: int) -> str:
    """Code the edits never touch, so work outside the edit shows up as extra."""
    out = [f"pub trait {prefix.title()}Shape {{ fn area(&self) -> u32; }}"]
    for i in range(count):
        out.append(f"pub struct {prefix.title()}Filler{i} {{ pub v: u32 }}")
        out.append(f"impl {prefix.title()}Shape for {prefix.title()}Filler{i} {{ fn area(&self) -> u32 {{ self.v * {i + 1} }} }}")
        out.append(f"pub fn {prefix}_filler_{i}(x: u32) -> u32 {{ x.wrapping_mul({i + 3}) ^ {i} }}")
    return "\n".join(out) + "\n"


def fixture_files() -> dict[str, str]:
    package = '[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n'
    base, mid, top = FIXTURE["base"], FIXTURE["mid"], FIXTURE["top"]
    return {
        "Cargo.toml": '[workspace]\nmembers = ["base", "mid", "top"]\nresolver = "2"\n',
        "base/Cargo.toml": package.format(name=base),
        "base/src/lib.rs": filler("base", 24) + scaffold_base(),
        "mid/Cargo.toml": package.format(name=mid) + f'\n[dependencies]\n{base} = {{ path = "../base" }}\n',
        "mid/src/lib.rs": filler("mid", 24) + scaffold_mid(base),
        "top/Cargo.toml": package.format(name=top) + f'\n[dependencies]\n{mid} = {{ path = "../mid" }}\n',
        "top/src/main.rs": filler("top", 8)
        + "fn main() {\n    let n = std::env::args().count() as u32;\n"
        + "    println!(\"{}\", incraudit_top::ia_top_all(n) + u64::from(top_filler_0(n)));\n}\n"
        + scaffold_top(mid),
    }


@dataclass(frozen=True)
class Laid:
    """The workspace as built: where it is, each role's package and root file, the build arguments."""

    ws: Path
    packages: dict[Role, str]
    files: dict[Role, Path]
    args: tuple[str, ...]


async def lay_out(target: Target, ws: Path, args: tuple[str, ...], step_s: int) -> Laid:
    if target.kind == "fixture":
        for rel, text in fixture_files().items():
            (ws / rel).parent.mkdir(parents=True, exist_ok=True)
            (ws / rel).write_text(text)
        return Laid(ws, dict(FIXTURE), {"base": ws / "base/src/lib.rs", "mid": ws / "mid/src/lib.rs",
                                        "top": ws / "top/src/main.rs"}, args)
    source = Path(target.path).resolve()
    given: tuple[tuple[Role, Crate | None], ...] = (("base", target.base), ("mid", target.mid), ("top", target.top))
    crates: dict[Role, Crate] = {r: c for r, c in given if c is not None}
    await copy_tree(source, ws, step_s)
    files: dict[Role, Path] = {}
    for role, crate in crates.items():
        path = ws / crate.file
        if not path.is_file():
            raise engine.Failed(f"target.{role}.file: no file {crate.file} under {source}")
        files[role] = path
    texts: dict[Role, str] = {"base": scaffold_base(), "mid": scaffold_mid(lib_name(crates["base"].package))}
    if "top" in crates:
        texts["top"] = scaffold_top(lib_name(crates["mid"].package))
    for role, text in texts.items():
        with files[role].open("a") as f:
            f.write(text)
    picked = tuple(word for c in crates.values() for word in ("-p", c.package))
    return Laid(ws, {r: c.package for r, c in crates.items()}, files, picked + args)


async def copy_tree(source: Path, to: Path, step_s: int) -> None:
    """`source`'s files into `to`: a git checkout's tracked and untracked-not-ignored files (read with
    `git ls-files`, which writes nothing), else every file but `target/`."""
    if (source / ".git").exists():
        step = f"listing the files of {source}"
        ran = await run(step, ["git", "-C", str(source), "ls-files", "-z", "--cached", "--others",
                               "--exclude-standard"], source, None, step_s)
        rels = [it for it in ran.checked(step).stdout.split("\0") if it]
    else:
        rels = [str(p.relative_to(source)) for p in source.rglob("*")
                if p.is_file() and p.relative_to(source).parts[:1] != ("target",)]

    def copy() -> None:
        for rel in rels:
            src, dst = source / rel, to / rel
            if not src.is_file():  # a file deleted but not yet staged
                continue
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, dst, follow_symlinks=False)

    await asyncio.to_thread(copy)


# Commands.

async def run(step: str, argv: Sequence[str], cwd: Path, env: dict[str, str] | None, secs: float) -> engine.Ran:
    """`argv` run to its end; one that does not start or outlives `secs` is a `Failed` naming the step
    and the command. `Ran.checked` names a nonzero exit with its status and output's tail."""
    shown = " ".join(argv)
    began = time.perf_counter()
    try:
        proc = await asyncio.create_subprocess_exec(*argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    except OSError as e:
        raise engine.Failed(f"{step}: `{shown}` in {cwd} did not start: {e}") from None
    try:
        async with asyncio.timeout(secs):
            out, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise engine.Failed(f"{step}: `{shown}` in {cwd} took more than {secs:.0f} s") from None
    return engine.Ran(tuple(argv), time.perf_counter() - began, proc.returncode or 0, engine.text(out),
                      engine.text(err))


async def resolve_duckdb(given: str, step_s: int) -> str:
    if given:
        if not Path(given).is_absolute() or not Path(given).is_file():
            raise engine.Failed(f"duckdb: an absolute path to the DuckDB binary, not {given!r}")
        return given
    step = "finding DuckDB (`nix build .#duckdb`)"
    ran = (await run(step, ["nix", "build", "--no-link", "--print-out-paths", ".#duckdb"], engine.ROOT, None,
                     step_s)).checked(step)
    lines = [it for it in ran.stdout.splitlines() if it.strip()]
    found = Path(lines[-1].strip()) / "bin" / "duckdb" if lines else None
    if found is None or not found.is_file():
        raise engine.Failed(f"{step}: `{' '.join(ran.argv)}` printed no store path with bin/duckdb:\n{ran.said()}")
    return str(found)


# The query kinds' names, read from the tree's sources.

KIND_ROW = re.compile(r"^\s*([A-Z][A-Z0-9_]*)\s*=\s*(0x[0-9a-fA-F]+|\d+)\s*,\s*\"([^\"]+)\"\s*;", re.MULTILINE)
KIND_CONST = re.compile(r"const\s+([A-Z][A-Z0-9_]*)\s*:\s*QueryKind\s*=\s*QueryKind\(\s*(0x[0-9a-fA-F]+|\d+)\s*\)")


def kind_names(root: Path) -> dict[int, str]:
    """Each kind number's query: the name its `query_kinds!` table gives, else its constant's name
    (blaze-v2-proof's copies left out), first by path. A kind made by arithmetic stays unnamed."""
    named: dict[int, str] = {}
    consts: dict[int, str] = {}
    for path in sorted((root / "crates").rglob("*.rs")):
        if "blaze-v2-proof" in path.parts:
            continue
        try:
            text = path.read_text(errors="replace")
        except OSError:
            continue
        if "QueryKind" not in text and "query_kinds!" not in text:
            continue
        if "query_kinds!" in text:
            for m in KIND_ROW.finditer(text):
                named.setdefault(int(m.group(2), 0), m.group(3))
        for m in KIND_CONST.finditer(text):
            consts.setdefault(int(m.group(2), 0), m.group(1).lower())
    return {**consts, **named}


BAND_RANGES: tuple[tuple[int, int, str], ...] = (
    (1, 4, "input"), (4, 5, "tree"), (5, 7, "input"), (0x1000, 0x2000, "facts"), (0x2000, 0x2200, "syntax"),
    (0x2200, 0x2400, "names"), (0x2400, 0x2C00, "infer"), (0x2C00, 0x3000, "expand"), (0x3000, 0x4000, "mir"),
    (0x4000, 0x4800, "codegen"), (0x4800, 0x5000, "link"), (0x5000, 0x6000, "user"), (0x6000, 0x6800, "cargo"),
    (0x6800, 0x7000, "kernel"),
)
FREE_BANDS = frozenset({"input", "tree"})
BAND_ORDER = ("input", "tree", "facts", "syntax", "names", "expand", "infer", "mir", "codegen", "link", "user",
              "cargo", "kernel", "other")


def band_of(kind: int) -> str:
    for lo, hi, name in BAND_RANGES:
        if lo <= kind < hi:
            return name
    return "other"


def kind_label(kind: int, names: dict[int, str]) -> str:
    return f"{names.get(kind, 'kind')}[{kind:#06x}]"


# The trace file.

def cbor_head(data: bytes, at: int) -> tuple[int, int, int]:
    """A CBOR item's major type, argument and where its content starts."""
    first = data[at]
    major, info = first >> 5, first & 31
    at += 1
    if info < 24:
        return major, info, at
    width = {24: 1, 25: 2, 26: 4, 27: 8}.get(info)
    if width is None:
        raise ValueError(f"a CBOR item of indefinite or reserved length (byte {first:#04x} at {at - 1})")
    return major, int.from_bytes(data[at:at + width], "big"), at + width


def cbor_columns(data: bytes) -> dict[str, int | bytes]:
    """The trace file: one map of text keys to an unsigned integer or a byte string."""
    major, count, at = cbor_head(data, 0)
    if major != 5:
        raise ValueError(f"the trace is a CBOR item of major type {major}, not a map")
    out: dict[str, int | bytes] = {}
    for _ in range(count):
        major, n, at = cbor_head(data, at)
        if major != 3:
            raise ValueError(f"a key of the trace's map of major type {major}, not text")
        key = data[at:at + n].decode()
        at += n
        major, n, at = cbor_head(data, at)
        if major == 0:
            out[key] = n
        elif major == 2:
            out[key] = data[at:at + n]
            at += n
        else:
            raise ValueError(f"the trace's `{key}` is of major type {major}, not an integer or bytes")
    return out


def bit(words: bytes, i: int) -> bool:
    """Bit `i` of little-endian u64 words (blaze_ids::bits::locate): byte i / 8, bit i % 8."""
    return bool(words[i >> 3] >> (i & 7) & 1)


def words(raw: bytes, code: Literal["H", "I"]) -> array[int]:
    out: array[int] = array("H") if code == "H" else array("I")
    if out.itemsize != {"H": 2, "I": 4}[code]:
        raise engine.Failed(f"this Python's array {code!r} is {out.itemsize} bytes wide")
    out.frombytes(raw)
    if sys.byteorder == "big":
        out.byteswap()
    return out


@dataclass(frozen=True)
class Trace:
    """`blaze_v2_proof::Trace` as read from its file: keys (34 bytes: kind LE and input), whether
    each has a result and ran in the build, its fingerprint, and its read edges in order."""

    kinds: array[int]
    keys: list[bytes]
    has: bytes
    ran: bytes
    fps: bytes
    edges_from: array[int]
    reads: array[int]
    saw: bytes
    index: dict[bytes, int]

    @staticmethod
    def parse(data: bytes, where: str) -> "Trace":
        try:
            cols = cbor_columns(data)
        except (ValueError, IndexError, UnicodeDecodeError) as e:
            raise engine.Failed(f"the trace {where}: {e}") from None
        if cols.get("version") != TRACE_VERSION:
            raise engine.Failed(f"the trace {where} is of version {cols.get('version')!r}; this reads {TRACE_VERSION}")
        names = ("kinds", "inputs", "has", "ran", "fingerprints", "edges_from", "reads", "saw")
        raw: dict[str, bytes] = {}
        for name in names:
            value = cols.get(name)
            if not isinstance(value, bytes):
                raise engine.Failed(f"the trace {where} has no byte column `{name}`")
            raw[name] = value
        kinds = words(raw["kinds"], "H")
        n = len(kinds)
        edges_from = words(raw["edges_from"], "I") if raw["edges_from"] else array("I", [0])
        reads = words(raw["reads"], "I")
        m = len(reads)
        checks = (("inputs", len(raw["inputs"]), 32 * n), ("fingerprints", len(raw["fingerprints"]), 16 * n),
                  ("has", len(raw["has"]), 8 * ((n + 63) // 64)), ("ran", len(raw["ran"]), 8 * ((n + 63) // 64)),
                  ("saw", len(raw["saw"]), 16 * m), ("edges_from", len(edges_from), n + 1))
        for what, got, want in checks:
            if got != want:
                raise engine.Failed(f"the trace {where} has {n} keys and {m} edges, and {got} of `{what}`, not {want}")
        k, inputs = raw["kinds"], raw["inputs"]
        keys = [k[2 * i:2 * i + 2] + inputs[32 * i:32 * i + 32] for i in range(n)]
        return Trace(kinds, keys, raw["has"], raw["ran"], raw["fingerprints"], edges_from, reads, raw["saw"],
                     {key: i for i, key in enumerate(keys)})

    @property
    def n(self) -> int:
        return len(self.kinds)

    def fp(self, i: int) -> bytes:
        return self.fps[16 * i:16 * i + 16]

    def saw_of(self, e: int) -> bytes:
        return self.saw[16 * e:16 * e + 16]

    def edges(self, i: int) -> range:
        return range(self.edges_from[i], self.edges_from[i + 1])

    def short(self, i: int) -> str:
        return f"{self.kinds[i]:04x}:{self.keys[i][2:10].hex()}"


# What re-executed, against the ideal.

@dataclass
class KindCount:
    ran: int = 0
    new: int = 0
    rerun: int = 0
    same: int = 0


def diff(before: Trace, after: Trace, names: dict[int, str], allows: Callable[[str], int | None]) -> dict[str, object]:
    """What `after`'s build re-executed relative to `before`'s state: per kind and band, the kinds
    over their allowance, and what pulled each of their keys in."""
    ran = [i for i in range(after.n) if bit(after.ran, i)]
    counts: dict[int, KindCount] = {}
    was: dict[int, int | None] = {}
    for i in ran:
        c = counts.setdefault(after.kinds[i], KindCount())
        c.ran += 1
        j = before.index.get(after.keys[i])
        if j is None or not bit(before.has, j):
            c.new += 1
            was[i] = None
        else:
            c.rerun += 1
            was[i] = j
            if after.fp(i) == before.fp(j):
                c.same += 1
    suspect = {k for k, c in counts.items() if (most := allows(band_of(k))) is not None and c.ran > most}
    asked: dict[int, list[int]] = {}
    for a in ran:
        for e in after.edges(a):
            asked.setdefault(after.reads[e], []).append(a)
    pulled: Counter[tuple[int, str, int]] = Counter()
    samples: dict[tuple[int, str, int], list[str]] = {}
    changed_reads: Counter[tuple[int, str, int]] = Counter()
    for i in ran:
        k = after.kinds[i]
        if k not in suspect:
            continue
        j = was[i]
        if j is not None:
            first: int | None = None
            changed = 0
            for e in before.edges(j):
                read = before.reads[e]
                now_at = after.index.get(before.keys[read])
                if now_at is None or after.fp(now_at) != before.saw_of(e):
                    changed += 1
                    if first is None:
                        first = read
            how, via = ("read changed", before.kinds[first]) if first is not None else ("nothing it read changed", -1)
            changed_reads[(k, how, via)] += changed
        else:
            askers = sorted(asked.get(i, []), key=lambda a: (after.kinds[a], after.keys[a]))
            how, via = ("new, asked by", after.kinds[askers[0]]) if askers else ("new, asked by nothing traced", -1)
        pulled[(k, how, via)] += 1
        got = samples.setdefault((k, how, via), [])
        if len(got) < 3:
            got.append(after.short(i))
    by_kind: list[dict[str, object]] = []
    extra = 0
    for k in sorted(counts, key=lambda k: (BAND_ORDER.index(band_of(k)), k)):
        c, most = counts[k], allows(band_of(k))
        over = 0 if most is None else max(0, c.ran - most)
        extra += over
        by_kind.append({"kind": k, "name": kind_label(k, names), "band": band_of(k), "ran": c.ran, "new": c.new, "rerun": c.rerun,
                            "same": c.same, "allowed": most, "extra": over})
    by_band: dict[str, dict[str, int]] = {}
    for k, c in counts.items():
        b = by_band.setdefault(band_of(k), {"ran": 0, "new": 0, "rerun": 0, "same": 0})
        for name in ("ran", "new", "rerun", "same"):
            b[name] += getattr(c, name)
    pulls: list[dict[str, object]] = []
    for (k, how, via), n in sorted(pulled.items(), key=lambda it: (-it[1], it[0])):
        pulls.append({"kind": kind_label(k, names), "how": how, "via": kind_label(via, names) if via >= 0 else None,
                          "via_band": band_of(via) if via >= 0 else None, "count": n,
                          "frontier": via >= 0 and via not in suspect, "changed_reads": changed_reads[(k, how, via)],
                          "samples": samples[(k, how, via)]})
    return {"ran": len(ran), "new": sum(c.new for c in counts.values()), "rerun": sum(c.rerun for c in counts.values()),
                "same": sum(c.same for c in counts.values()), "extra": extra,
                "by_band": {b: by_band[b] for b in BAND_ORDER if b in by_band}, "by_kind": by_kind, "pulled_by": pulls}


IDENT = re.compile(r"\bia_[a-z0-9_]+")


def judge_misses(rows: Sequence[Row], ideal: Ideal, packages: dict[Role, str]) -> dict[str, object]:
    """`blaze why`'s rows for the build against the ideal: a codegen step for a function the ideal
    names, a unit of a crate it names; anything else (another function, a build-script run) extra."""
    units = {lib_name(packages[r]) for r in ideal.units if r in packages}
    allowed: list[Row] = []
    extra: list[Row] = []
    steps: Counter[str] = Counter()
    for row in rows:
        step, item = str(row.get("step")), str(row.get("item"))
        steps[step] += 1
        if step == "unit":
            ok = lib_name(item.split(" ", 1)[0]) in units
        elif "codegen" in step:
            ok = bool(set(IDENT.findall(item)) & set(ideal.functions))
        else:
            ok = False
        (allowed if ok else extra).append(row)
    return {"count": len(rows), "steps": dict(sorted(steps.items())), "allowed": len(allowed), "extra": len(extra),
                "extra_rows": [{k: r.get(k) for k in ("step", "item", "cause", "what", "names")} for r in extra[:40]]}


# A session: one daemon, one workspace, one build after another.

@dataclass(frozen=True)
class Measured:
    label: str
    seconds: float
    ok: bool
    said: str
    trace: Trace | None
    build: Row | None
    query_counts: list[Row]
    misses: list[Row]
    spans: list[Row]
    work: Row | None
    history_error: str | None


@dataclass
class Session:
    blaze: Path
    laid: Laid
    env: dict[str, str]
    command: str
    traces: Path
    step_s: int
    history_s: int
    last_build: int = 0
    steps: int = 0

    def argv(self, *words: str) -> list[str]:
        return [str(self.blaze), *words]

    async def sql(self, what: str, query: str) -> list[Row]:
        step = f"the history's {what}"
        ran = (await run(step, self.argv("debug", "sql", "--json", query), self.laid.ws, self.env, self.step_s)
               ).checked(step)
        rows: list[Row] = []
        for line in ran.stdout.splitlines():
            if not line.strip():
                continue
            try:
                row = json.loads(line)
            except json.JSONDecodeError as e:
                raise engine.Failed(f"{step}: `blaze debug sql --json` printed {line!r}, not a row: {e}") from None
            if not isinstance(row, dict):
                raise engine.Failed(f"{step}: `blaze debug sql --json` printed {line!r}, not an object")
            rows.append(cast(Row, row))
        return rows

    def ours(self, row: Row) -> bool:
        said, where = row.get("command"), row.get("workspace")
        return (isinstance(said, str) and isinstance(where, str) and said.split()[:1] == [self.command]
                and "--message-format=json" in said.split()
                and Path(where).resolve() == self.laid.ws.resolve())

    async def settled(self, label: str) -> Row:
        """The `builds` row of the build just made, once flushed (the daemon flushes the history as
        a build ends, on its history thread). No event says it was written, so this polls on that
        readiness, bounded by `history_s`."""
        query = ("SELECT build_id, workspace, command, outcome, dur_ns, hits, misses, joined, tree, blaze "
                 f"FROM builds WHERE build_id > {self.last_build} ORDER BY build_id")
        try:
            async with asyncio.timeout(self.history_s):
                while True:
                    rows = [r for r in await self.sql(f"builds after {label}", query) if self.ours(r)]
                    done = [r for r in rows if r.get("outcome") != "running"]
                    if done:
                        row = done[-1]
                        build = row.get("build_id")
                        if isinstance(build, int):
                            self.last_build = build
                        return row
                    await asyncio.sleep(POLL_S)
        except TimeoutError:
            raise engine.Failed(f"{label}: no ended `builds` row of `blaze {self.command}` in {self.laid.ws} after "
                                f"build {self.last_build} within {self.history_s} s") from None

    async def history(self, label: str) -> tuple[Row, list[Row], list[Row], list[Row]]:
        row = await self.settled(label)
        build = row["build_id"]
        async with asyncio.TaskGroup() as group:
            counts = group.create_task(self.sql(
                f"query counts of {label}", "SELECT query, executed, verified, reran, evicted_asked FROM query_counts "
                f"WHERE build_id = {build} ORDER BY executed DESC, query"))
            misses = group.create_task(self.sql(
                f"misses of {label}", "SELECT step, item, cause, what, names, changed, \"of\" FROM misses "
                f"WHERE build_id = {build} ORDER BY step, item"))
            spans = group.create_task(self.sql(
                f"spans of {label}", "SELECT name, count(*) AS n, sum(dur_ns) AS dur_ns, sum(self_cpu_ns) AS self_cpu_ns "
                f"FROM spans WHERE build_id = {build} AND target = 'blaze' GROUP BY name "
                "ORDER BY self_cpu_ns DESC NULLS LAST, name LIMIT 15"))
        return row, counts.result(), misses.result(), spans.result()

    async def trace(self, label: str) -> Trace:
        self.steps += 1
        to = self.traces / f"{self.steps:02d}-{re.sub(r'[^a-z0-9]+', '-', label.lower()).strip('-')}.cbor"
        to.unlink(missing_ok=True)
        step = f"{label}: blaze trace"
        (await run(step, self.argv("trace", "--out", str(to), *self.laid.args), self.laid.ws, self.env, self.step_s)
         ).checked(step)
        if not to.is_file():
            raise engine.Failed(f"{step}: `blaze trace --out {to}` exited 0 and wrote no file")
        data = await asyncio.to_thread(to.read_bytes)
        return await asyncio.to_thread(Trace.parse, data, str(to))

    async def step(self, label: str) -> Measured:
        step = f"{label}: blaze {self.command}"
        ran = await run(step, self.argv(self.command, "--message-format=json", *self.laid.args), self.laid.ws,
                        self.env, self.step_s)
        if ran.returncode != 0:
            return Measured(label, ran.seconds, False,
                            f"{step}: `{' '.join(ran.argv)}` exited {ran.returncode}:\n{ran.said()}",
                            None, None, [], [], [], None, None)
        work = engine.worked_lines(ran.stdout)
        trace = asyncio.create_task(self.trace(label))
        history_error: str | None = None
        row: Row | None = None
        counts: list[Row] = []
        misses: list[Row] = []
        spans: list[Row] = []
        try:
            row, counts, misses, spans = await self.history(label)
        except* engine.Failed as e:
            history_error = "; ".join(str(it) for it in e.exceptions)
        return Measured(label, ran.seconds, True, "", await trace, row, counts, misses, spans,
                        work[-1] if work else None, history_error)


# The run.

def by_kind_text(d: dict[str, object], top: int = 12) -> str:
    rows = cast(list[dict[str, object]], d["by_kind"])
    shown = [f"{r['name']} {r['ran']} ({r['new']} new, {r['rerun']} rerun, {r['same']} same)"
             + (f" > {r['allowed']}" if r["extra"] else "") for r in rows[:top]]
    return "; ".join(shown) + ("; ..." if len(rows) > top else "")


def first_leak(d: dict[str, object]) -> str:
    pulls = cast(list[dict[str, object]], d["pulled_by"])
    frontier = [p for p in pulls if p["frontier"]] or pulls
    if not frontier:
        return ""
    p = frontier[0]
    return f"{p['kind']} x{p['count']} {p['how']} {p['via'] or ''}".strip()


def wall(m: Measured) -> str:
    dur = m.build.get("dur_ns") if m.build else None
    return f"{m.seconds:.2f}" + (f" ({int(dur) / 1e9:.2f})" if isinstance(dur, int) else "")


def markdown(results: list[dict[str, object]], header: dict[str, object]) -> str:
    out = ["# incraudit: blaze's re-execution per edit against the ideal", ""]
    out += [f"- {k}: {v}" for k, v in header.items()]
    out += ["", ("| edit | ideal | re-executed (new/rerun/same) | extra keys | codegen and units outside the ideal "
            "| first leak edge | wall s (daemon) | revert re-executed | no-op after |"),
            "|---|---|---|---|---|---|---|---|---|"]
    for r in results:
        if "error" in r:
            out.append(f"| {r['edit']} | {r['ideal']} | error: {str(r['error']).splitlines()[0][:120]} | | | | | | |")
            continue
        d = cast(dict[str, object], r["diff"])
        mi = cast(dict[str, object] | None, r.get("misses"))
        rv = cast(dict[str, object] | None, r.get("revert"))
        no = cast(dict[str, object] | None, r.get("noop"))
        out.append(f"| {r['edit']} | {r['ideal']} | {d['ran']} ({d['new']}/{d['rerun']}/{d['same']}) | {d['extra']} "
                   f"| {mi['extra'] if mi else 'n/a'} of {mi['count'] if mi else 'n/a'} | {first_leak(d)} "
                   f"| {r['wall']} | {rv['ran'] if rv else 'n/a'} | {no['ran'] if no else 'n/a'} |")
    for r in results:
        out += ["", f"## {r['edit']}: {r['what']}", "", f"Ideal: {r['ideal']}."]
        if r.get("shifts"):
            out.append(f"Moves bytes: {r['shifts']}.")
        if "error" in r:
            out += ["", "```", str(r["error"])[-3000:], "```"]
            continue
        d = cast(dict[str, object], r["diff"])
        out += ["", "| band | ran | new | rerun | same |", "|---|---|---|---|---|"]
        for band, c in cast(dict[str, dict[str, int]], d["by_band"]).items():
            out.append(f"| {band} | {c['ran']} | {c['new']} | {c['rerun']} | {c['same']} |")
        out += ["", "| kind | band | ran | new | rerun | same | allowed | extra |", "|---|---|---|---|---|---|---|---|"]
        for k in cast(list[dict[str, object]], d["by_kind"]):
            out.append(f"| {k['name']} | {k['band']} | {k['ran']} | {k['new']} | {k['rerun']} | {k['same']} "
                       f"| {'any' if k['allowed'] is None else k['allowed']} | {k['extra']} |")
        pulls = cast(list[dict[str, object]], d["pulled_by"])
        if pulls:
            out += ["", ("What pulled the extra keys in (frontier: the read is within its allowance, so the cutoff "
                    "belongs on this edge):"), "", "| extra kind | count | how | read / asker | frontier | samples |",
                    "|---|---|---|---|---|---|"]
            for p in pulls[:25]:
                out.append(f"| {p['kind']} | {p['count']} | {p['how']} | {p['via'] or ''} "
                           f"| {'yes' if p['frontier'] else ''} | {' '.join(cast(list[str], p['samples']))} |")
        mi = cast(dict[str, object] | None, r.get("misses"))
        if mi and cast(int, mi["extra"]):
            out += ["", "Computed again outside the ideal (`blaze why`):", ""]
            for row in cast(list[Row], mi["extra_rows"]):
                out.append(f"- {row['step']} {row['item']}: {row['cause']}"
                           + (f", {row['what']}" if row.get("what") else "")
                           + (f" (of {row['names']})" if row.get("names") else ""))
        counts = cast(list[Row], r.get("query_counts") or [])
        if counts:
            out += ["", "Query functions by runs (`query_counts`): " + "; ".join(
                f"{c['query']} {c['executed']} run, {c['verified']} verified" for c in counts[:12])]
        for which in ("revert", "noop"):
            x = cast(dict[str, object] | None, r.get(which))
            if x and cast(int, x["ran"]):
                out += ["", (f"{'After the revert' if which == 'revert' else 'The no-op build after'}: "
                        f"{by_kind_text(x)}; first leak: {first_leak(x)}")]
    return "\n".join(out) + "\n"


async def audit(args: Input, rep: report.Report, eng: engine.Engine, work: Path) -> None:
    ws, home, traces = work / "ws", work / "home", work / "traces"
    for d in (ws, traces):
        d.mkdir(parents=True, exist_ok=True)
    chosen = [e for e in EDITS if not args.edits or e.name in args.edits]
    unknown = sorted(set(args.edits) - {e.name for e in EDITS})
    if unknown:
        raise engine.Failed(f"edits: no edit {', '.join(unknown)}; the edits are {', '.join(e.name for e in EDITS)}")
    async with asyncio.TaskGroup() as group:
        names_t = group.create_task(asyncio.to_thread(kind_names, engine.ROOT))
        duckdb_t = group.create_task(resolve_duckdb(args.duckdb, args.step_s))
        laid_t = group.create_task(lay_out(args.target, ws, args.args, args.step_s))
        digest_t = group.create_task(asyncio.to_thread(engine.digest, eng.blaze))
    names, laid = names_t.result(), laid_t.result()
    env = eng.env(home, extra={"BLAZE_DUCKDB": duckdb_t.result()})
    rep.extra.update(work=str(work), workspace=str(ws), blaze_blake3=digest_t.result(), tree=args.tree,
                     command=args.command, args=list(laid.args), packages=dict(laid.packages), kinds_named=len(names))
    s = Session(eng.blaze, laid, env, args.command, traces, args.step_s, args.history_s)
    results: list[dict[str, object]] = []
    try:
        cold = await s.step("cold")
        if not cold.ok:
            rep.add(f"{PROPERTY}/cold", "error", cold.said)
            return
        rep.add(f"{PROPERTY}/cold", "pass", f"built cold in {cold.seconds:.2f} s", seconds=round(cold.seconds, 3))
        warm = await s.step("warm")
        if not warm.ok or warm.trace is None:
            rep.add(f"{PROPERTY}/no-op", "error", warm.said)
            return
        assert cold.trace is not None
        zero: Callable[[str], int | None] = lambda band: None if band in FREE_BANDS else 0
        base = diff(cold.trace, warm.trace, names, zero)
        rep.add(f"{PROPERTY}/no-op", "pass" if base["ran"] == 0 else "fail",
                "a build with nothing changed re-executed " + (by_kind_text(base) if base["ran"] else "nothing"),
                ran=cast(int, base["ran"]), seconds=round(warm.seconds, 3))
        rep.extra["no_op"] = {"diff": base, "wall": wall(warm), "history_error": warm.history_error, "build": warm.build}
        before = warm
        for edit in chosen:
            results.append(await one_edit(edit, s, before, names, rep))
            last = results[-1].get("_after")
            if isinstance(last, Measured):
                before = last
    finally:
        for r in results:
            r.pop("_after", None)
        rep.extra["edits"] = results
        header: dict[str, object] = {"blaze": str(eng.blaze), "blaze_blake3": rep.extra.get("blaze_blake3"),
                                         "tree": args.tree or "(not given)", "command": args.command,
                                         "workspace": str(ws), "packages": dict(laid.packages)}
        if args.command == "check":
            header["note"] = "`blaze check` runs no codegen, so the codegen columns are zero by construction"
        text = markdown(results, header)
        if args.out:
            md = Path(args.out).with_suffix(".md")
            md.parent.mkdir(parents=True, exist_ok=True)
            md.write_text(text)
            rep.extra["markdown"] = str(md)
        print(text, flush=True)
        stopped = await run("blaze stop", s.argv("stop"), ws, env, 300)
        if stopped.returncode != 0:
            rep.add(f"{PROPERTY}/stop", "error", f"`blaze stop` exited {stopped.returncode}:\n{stopped.said()}")


async def one_edit(edit: Edit, s: Session, before: Measured, names: dict[int, str],
                   rep: report.Report) -> dict[str, object]:
    out: dict[str, object] = {"edit": edit.name, "what": edit.what, "ideal": edit.ideal.says, "shifts": edit.shifts}
    path = s.laid.files["base"]
    original = path.read_text()
    text = original
    for find, replace in edit.changes:
        if text.count(find) != 1:
            out["error"] = f"{edit.name}: {find!r} is in {path} {text.count(find)} times, not once"
            rep.add(f"{PROPERTY}/{edit.name}", "error", str(out["error"]))
            return out
        text = text.replace(find, replace)
    try:
        path.write_text(text)
        edited = await s.step(f"{edit.name}: edited")
    finally:
        path.write_text(original)
    reverted = await s.step(f"{edit.name}: reverted")
    after = await s.step(f"{edit.name}: no-op after")
    out["_after"] = after if after.ok and after.trace is not None else None
    if not edited.ok or edited.trace is None or before.trace is None:
        out["error"] = edited.said or "no trace of the state before the edit"
        rep.add(f"{PROPERTY}/{edit.name}", "error", str(out["error"]))
        return out
    d = diff(before.trace, edited.trace, names, edit.ideal.allows)
    out.update(diff=d, wall=wall(edited), build=edited.build, work=edited.work, spans=edited.spans,
               query_counts=edited.query_counts[:40], history_error=edited.history_error)
    misses = judge_misses(edited.misses, edit.ideal, s.laid.packages) if edited.history_error is None else None
    out["misses"] = misses
    zero: Callable[[str], int | None] = lambda band: None if band in FREE_BANDS else 0
    if reverted.ok and reverted.trace is not None:
        out["revert"] = diff(edited.trace, reverted.trace, names, zero)
    else:
        out["revert_error"] = reverted.said
    if after.ok and after.trace is not None and reverted.trace is not None:
        out["noop"] = diff(reverted.trace, after.trace, names, zero)
    extra = cast(int, d["extra"]) + (cast(int, misses["extra"]) if misses else 0)
    verdict: report.Verdict = "pass" if extra == 0 else "fail"
    rep.add(f"{PROPERTY}/{edit.name}", verdict,
            f"ideal: {edit.ideal.says}; re-executed {by_kind_text(d, 6)}; first leak: {first_leak(d)}"
            + (f"; history: {edited.history_error}" if edited.history_error else ""),
            ran=cast(int, d["ran"]), extra=cast(int, d["extra"]),
            codegen_extra=cast(int, misses["extra"]) if misses else None, seconds=round(edited.seconds, 3))
    if edited.history_error:
        rep.add(f"{PROPERTY}/{edit.name}/history", "error", edited.history_error)
    rv = cast(dict[str, object] | None, out.get("revert"))
    if rv is not None:
        rep.add(f"{PROPERTY}/{edit.name}/revert", "pass" if rv["ran"] == 0 else "fail",
                "the build after the revert re-executed " + (by_kind_text(rv, 6) if rv["ran"] else "nothing"),
                ran=cast(int, rv["ran"]))
    else:
        rep.add(f"{PROPERTY}/{edit.name}/revert", "error", reverted.said)
    no = cast(dict[str, object] | None, out.get("noop"))
    if no is not None:
        rep.add(f"{PROPERTY}/{edit.name}/no-op", "pass" if no["ran"] == 0 else "fail",
                "the no-op build after the revert re-executed " + (by_kind_text(no, 6) if no["ran"] else "nothing"),
                ran=cast(int, no["ran"]))
    elif not after.ok:
        rep.add(f"{PROPERTY}/{edit.name}/no-op", "error", after.said)
    return out


def check_target(t: Target) -> None:
    if t.kind == "fixture":
        if t.path or t.base or t.mid or t.top:
            fail(SCRIPT, "target: the fixture takes no path or crates")
        return
    if not t.path or t.base is None or t.mid is None:
        fail(SCRIPT, "target: a workspace needs `path`, `base` and `mid` (`top` is optional)")
    if not Path(t.path).is_dir():
        fail(SCRIPT, f"target.path: no directory {t.path}")


def main() -> None:
    if sys.argv[1:] == ["selftest"]:
        selftest()
        return
    if len(sys.argv) != 2:
        fail(SCRIPT, "takes one JSON value (inline, a file's path, or -), or `selftest`")
    args = parse(Input, sys.argv[1], SCRIPT)
    check_target(args.target)
    rep = report.Report(SCRIPT, args.engine, args.out)
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        rep.add(f"{PROPERTY}/setup", "error", str(e))
        rep.finish()
    work = engine.work_dir(args.work, "incraudit-")
    work.mkdir(parents=True, exist_ok=True)
    try:
        asyncio.run(audit(args, rep, eng, work))
    except* engine.Failed as e:
        for it in e.exceptions:
            rep.add(f"{PROPERTY}/setup", "error", str(it))
    finally:
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)
    rep.finish()


# The self-test: the trace reader and the diff on a hand-made pair of traces.

def cbor_item(major: int, n: int) -> bytes:
    if n < 24:
        return bytes([major << 5 | n])
    for info, width in ((24, 1), (25, 2), (26, 4), (27, 8)):
        if n < 1 << (8 * width):
            return bytes([major << 5 | info]) + n.to_bytes(width, "big")
    raise ValueError(n)


def trace_bytes(rows: Sequence[tuple[int, int, int, bool, Sequence[tuple[int, int]]]]) -> bytes:
    """A trace file of `(kind, input byte, fingerprint, ran, [(read index, saw)])` rows."""
    n = len(rows)
    has, ran = bytearray(8 * ((n + 63) // 64)), bytearray(8 * ((n + 63) // 64))
    cols: dict[str, bytearray] = {k: bytearray() for k in ("kinds", "inputs", "fingerprints", "edges_from",
                                                            "reads", "saw")}
    edges = 0
    for i, (kind, byte, fp, did, reads) in enumerate(rows):
        cols["kinds"] += kind.to_bytes(2, "little")
        cols["inputs"] += bytes([byte]) * 32
        cols["fingerprints"] += fp.to_bytes(16, "little")
        cols["edges_from"] += edges.to_bytes(4, "little")
        has[i >> 3] |= 1 << (i & 7)
        if did:
            ran[i >> 3] |= 1 << (i & 7)
        for read, saw in reads:
            cols["reads"] += read.to_bytes(4, "little")
            cols["saw"] += saw.to_bytes(16, "little")
            edges += 1
    cols["edges_from"] += edges.to_bytes(4, "little")
    out = cbor_item(5, 9) + cbor_item(3, 7) + b"version" + cbor_item(0, TRACE_VERSION)
    for name, value in (("kinds", cols["kinds"]), ("inputs", cols["inputs"]), ("has", has), ("ran", ran),
                        ("fingerprints", cols["fingerprints"]), ("edges_from", cols["edges_from"]),
                        ("reads", cols["reads"]), ("saw", cols["saw"])):
        out += cbor_item(3, len(name)) + name.encode() + cbor_item(2, len(value)) + bytes(value)
    return out


def selftest() -> None:
    file_, syntax, infer, names_kind, codegen = 1, 0x2000, 0x2400, 0x2200, 0x4000
    # A file A; its parse B; C infers from B; D reads a names key X; E, new after, is asked by C.
    before = Trace.parse(trace_bytes([
        (file_, 1, 11, False, []), (syntax, 2, 21, False, [(0, 11)]), (infer, 3, 31, False, [(1, 21)]),
        (infer, 4, 41, False, [(4, 51)]), (names_kind, 5, 51, False, []),
    ]), "selftest before")
    after = Trace.parse(trace_bytes([
        (file_, 1, 12, False, []), (syntax, 2, 22, True, [(0, 12)]), (infer, 3, 31, True, [(1, 22), (5, 61)]),
        (infer, 4, 41, False, [(4, 51)]), (names_kind, 5, 51, False, []), (codegen, 6, 61, True, [(2, 31)]),
    ]), "selftest after")
    names = {syntax: "parse", infer: "infer", codegen: "codegen_fn"}
    ideal = Ideal("test", bands(syntax=1))
    d = diff(before, after, names, ideal.allows)
    problems: list[str] = []

    def want(what: str, got: object, expected: object) -> None:
        if got != expected:
            problems.append(f"{what}: {got!r}, not {expected!r}")

    want("ran", d["ran"], 3)
    want("new", d["new"], 1)
    want("rerun", d["rerun"], 2)
    want("same", d["same"], 1)
    want("extra", d["extra"], 2)
    pulls = {(p["kind"], p["how"]): p for p in cast(list[dict[str, object]], d["pulled_by"])}
    infer_pull = pulls.get(("infer[0x2400]", "read changed"))
    want("infer pulled by", infer_pull and (infer_pull["via"], infer_pull["frontier"]), ("parse[0x2000]", True))
    codegen_pull = pulls.get(("codegen_fn[0x4000]", "new, asked by"))
    want("codegen pulled by", codegen_pull and (codegen_pull["via"], codegen_pull["frontier"]),
         ("infer[0x2400]", False))
    want("bit", [bit(b"\x05", i) for i in range(4)], [True, False, True, False])
    misses = judge_misses([{"step": "codegen dev template", "item": "audit_base::incraudit::ia_body_fn"},
                           {"step": "codegen dev template", "item": "audit_mid::incraudit_mid::ia_mid_calls_body"},
                           {"step": "unit", "item": "audit_base v0.1.0 (lib)"},
                           {"step": "unit", "item": "audit_mid v0.1.0 (lib)"}],
                          EDITS[1].ideal, FIXTURE)
    want("misses extra", misses["extra"], 2)
    for e in EDITS:
        text = scaffold_base()
        for find, _ in e.changes:
            want(f"{e.name} finds {find!r} once", text.count(find), 1)
    try:
        Trace.parse(b"\xa0", "an empty map")
        problems.append("a trace with no columns was read")
    except engine.Failed:
        pass
    if problems:
        sys.exit(f"{SCRIPT} selftest:\n  " + "\n  ".join(problems))
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    main()
