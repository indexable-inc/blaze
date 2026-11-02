"""edits.py: blaze's incremental path held to blaze's own build of the same state from nothing,
over edit sequences, the proof of lane dbsp's maintained name resolution, impl tables and trait
candidate sets (docs/v2/plan/design/dbsp.md section 12; notes in docs/v2/plan/lanes/dbsp/proof.md).

    uv run --frozen python harness/v2/dbsp/edits.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/edits.json"}'
    uv run --frozen python harness/v2/dbsp/edits.py '{"engine": {...}, "out": "...", "projects": [{"kind": "harness", "name": "ripgrep"}]}'
    uv run --frozen python harness/v2/dbsp/edits.py '{"engine": {...}, "out": "...", "projects": [{"kind": "cases", "path": "harness/v2/frontend/cases/nameres"}]}'
    uv run --frozen python harness/v2/dbsp/edits.py selftest

A project is built once as it is (the base), then once after each edit of a script. blaze builds
every step in one workspace under one daemon, so each step after the base goes through its
incremental path: a file written, the maintainers run over the delta, the queries that read a
changed slice rerun. Each step is also built from nothing: a copy of the same files, a daemon
with an empty home, and a store holding only the standard library (oracle.std_seed). The two are
compared: accepted or rejected, the keys of the errors (cases.key_of: a code, or the start of a
message), and the program's exit status, output and panics when the project has a program to run.
No step is compared with rustc or cargo (2026-10-30).

An edit script is text, one edit a line, `op(arg, ...);`, after the line `//@ dbsp-edits 1`;
`//` lines and a `// keep` or `// break` after an edit are notes. Every edit is a total function
of the scaffold: a model of modules (inline, or a file of their own), structs, traits with
methods, type aliases, `use` re-exports, glob imports, impls (of a trait or through a `use` of
one, of a struct or through an alias), `macro_rules!` that define a struct, `#[derive]`s, and
probe functions that call a method. The scaffold is rendered between two marker lines appended
to one file of the project (the root of a case), every item `pub(crate)` and `#[allow(unused)]`,
every path relative (`self::`, `super::`), so it builds into any project of any edition and
names nothing of it:

    add_mod(m, inline|file)   remove_mod(m)         add_struct(m, S)        add_trait(m, T)
    add_alias(m, A, S)        retarget_alias(A, S)  add_impl(m, T, S)       remove_impl(m, T, S)
    add_use(m, U, T)          repoint_use(U, T)     add_glob(m, n)          remove_glob(m, n)
    add_macro(m, mac, S)      add_derive(S, D)      remove_derive(S, D)     add_method(T, f)
    remove_method(T, f)       add_probe(m, P, S, f) remove(name)            rename(old, new)
    rename_def(old, new)

(`m` is `top`, the file's own module, or a module the script added; `rename` renames a name
and every use of it, `rename_def` only its definition). Script `i` of seed `s` is made by
`random.Random("blaze-dbsp-edits:<s>:<i>")` alone, so a seed gives the same scripts everywhere.
Each edit keeps the program or breaks it on purpose (a `// break`: a duplicate impl or item, a
dangling path, a derive missing what it needs, a method a probe still calls removed, a probe
with its trait out of scope), about one edit in four, so both the accept and the reject paths
are hit. Every script holds at least one `add_impl` that keeps the program. The scripts of
`scripts/` (beside this file) run on every project too: hand-written ones and every mismatch
shrunk.

A step whose incremental build differs from the one from nothing ends its script, and is shrunk
(`shrink`: edits deleted, each pass's candidates run at once, each a whole script on a fresh
daemon) while its first mismatch keeps the same signature (`signature`: what differs and the
error keys it is about). The result is written to `into` as `<project>-s<seed>-<i>.edits`.

The cost check (design section 10): after each `add_impl` step, the daemon's
`{"reason": "blaze-maintained", "write", "maintainer", "crate", "work": {"ops", "woken",
"rebuilt", "region", "slices", "keys"}}` lines on stdout (blaze_dbsp::Work, one a maintainer, a
crate and a write) must say, summed over the request's writes: for the edited crate, each
maintainer applied one op (`ops == 1`) and set at most two slices (`slices <= 2`); for every
other crate, nothing (every count 0). That holds on every project whatever its size, or the
maintainers do work proportional to the crate rather than to the edit. A step with no such line
fails, naming the requests that add it: the check is never skipped.

With `trace`, the daemon's trace (`blaze trace`) is taken after every step, and each edit's
pair of traces is held to blaze-v2-proof's `dbsp` case of its kind
(crates/blaze-v2-proof/src/dbsp.rs), given the traits and (trait, self type) buckets whose
impls the model says the edit changed (`touched`): after an impl edit no query of another
trait's slice reruns; after an item edit no resolver query of another module does; after any
edit no round query does.

    pass   the incremental build of the step is the build from nothing's (outcome, error keys,
           run); the cost is the edit's; the trace is the case's
    fail   it is not; or no `blaze-maintained` line was printed
    error  the harness could not say (a timeout, a build that said nothing, a project that cannot
           be laid out), or a saved script does not parse
    unmeasured
           a trace case found a kind of query neither trace has

The input is one JSON object (inline, a file's path, or `-` for stdin):

    engine    the blaze under test (engine.EngineSpec)
    out       the path the result JSON is written to (report.py)
    work      the directory everything is built in; empty: a new one under /tmp
    projects  what the scripts run on (default: the oracle's fixture workspace), each one of
              {"kind": "fixture"}                    harness/v2/fixture.py's workspace, in oracle_base
              {"kind": "harness", "name": "ripgrep"} a project of harness/blaze_cli.py's PROJECTS,
                                                     fetched as blaze_cli.py fetches it, in the first
                                                     file its `edits` name, run with its `run`
              {"kind": "workspace", "path": DIR}     a cargo workspace or package (needs `file`)
              {"kind": "cases", "path": DIR|FILE}    each case of a directory of
                                                     harness/v2/frontend/cases/ (or one case file),
                                                     the scaffold in its root
            with, for the three workspace kinds, `file` (where the scaffold goes, under the
            workspace), `args` (after `blaze COMMAND`/`cargo COMMAND`, e.g. ["-p", "app"]),
            `command` ("build" or "check"), `run` (the program's arguments; null: not run)
            and `program` (its target name; "": the one program the build makes); for cases,
            `filter` (only the cases whose name has it)
    seed      the seed (0)
    scripts   how many generated scripts each project runs (3), beside those of `scripts/`
    steps     how many edits a generated script has (12)
    saved     run the scripts of `scripts/` too (true)
    shrink    shrink each mismatch and write it to `into` (true)
    rounds    at most this many batches of candidates a mismatch is shrunk with (30)
    into      where shrunk scripts go ("": scripts/ beside this file)
    trace     take the daemon's trace after every step and check it (false)
    jobs      how many scripts run at once (4), each with a daemon, a home and a workspace of its own
    keep      keep every workspace, not only those of scripts that did not pass
"""

import concurrent.futures
import copy
import importlib
import itertools
import json
import random
import re
import shutil
import sys
import tempfile
from collections import Counter
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field, replace
from pathlib import Path, PurePosixPath
from typing import Any, Literal, cast

HERE = Path(__file__).resolve().parent
V2 = HERE.parent
sys.path[:0] = [str(HERE), str(V2), str(V2 / "frontend"), str(V2.parent)]
from typed_input import fail, parse  # noqa: E402
import engine, report  # noqa: E402
import cases  # noqa: E402
import fixture, oracle  # noqa: E402

SCRIPT = "edits.py"
SCRIPTS = HERE / "scripts"
SUFFIX = ".edits"
HEADER = "//@ dbsp-edits 1"
BEGIN = "// blaze-dbsp-edits: begin"
END = "// blaze-dbsp-edits: end"
TOP = "top"
ALLOW = "#[allow(unused)]"
BAD = 0.25
MAINTAINED = "blaze-maintained"
WORK_FIELDS = ("ops", "woken", "rebuilt", "region", "slices", "keys")
MAX_OPS, MAX_SLICES = 1, 2
REQUESTED = ("blaze prints no such line yet: queue 00819 (lane ci: the line, in blaze-api and blaze-ui) and "
             "00820 (dbsp-engine: the per-write records in base-db's maintain.rs)")
FIXTURE_FILE = "base/src/lib.rs"
FIXTURE_PROGRAM = "oracle_app"
NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
STATEMENT = re.compile(r"([a-z_]+)\(([^()]*)\)\s*;\s*(?://.*)?")
DERIVES = ("Clone", "Copy", "Debug", "PartialEq", "Eq", "Hash", "Default")
# A derive and the one it needs beside it.
NEEDS = {"Copy": "Clone", "Eq": "PartialEq"}
# The traits a derive implements, where they are more than its own.
DERIVED = {"PartialEq": ("PartialEq", "StructuralPartialEq")}
# The probes a derive answers, by the method name a probe is given.
DERIVE_PROBES = {"clone": "Clone", "eq": "PartialEq", "default": "Default"}
MOD_KINDS = ("inline", "file")
# Each op and how many arguments it takes.
OPS: dict[str, int] = {
    "add_mod": 2, "remove_mod": 1, "add_struct": 2, "add_trait": 2, "add_alias": 3, "retarget_alias": 2,
    "add_impl": 3, "remove_impl": 3, "add_use": 3, "repoint_use": 2, "add_glob": 2, "remove_glob": 2,
    "add_macro": 3, "add_derive": 2, "remove_derive": 2, "add_method": 2, "remove_method": 2, "add_probe": 4,
    "remove": 1, "rename": 2, "rename_def": 2,
}
# The case of blaze-v2-proof's `dbsp` module each op's trace is held to; every other op is "item".
KIND_OF = {
    "add_impl": "impl", "remove_impl": "impl", "add_derive": "derive", "remove_derive": "derive",
    "retarget_alias": "alias", "repoint_use": "repoint", "add_method": "method", "remove_method": "method",
    "add_mod": "mod", "remove_mod": "mod",
}
KINDS = ("impl", "derive", "alias", "repoint", "method", "mod", "item")
WEIGHTS: tuple[tuple[str, int], ...] = (
    ("add_struct", 6), ("add_trait", 5), ("add_impl", 9), ("remove_impl", 3), ("add_alias", 3),
    ("retarget_alias", 3), ("add_use", 4), ("repoint_use", 4), ("add_glob", 3), ("remove_glob", 2), ("add_mod", 3),
    ("remove_mod", 1), ("add_macro", 2), ("add_derive", 4), ("remove_derive", 2), ("add_method", 4),
    ("remove_method", 2), ("add_probe", 5), ("remove", 3), ("rename", 2), ("rename_def", 1),
)
STEMS = {"struct": "DbspS", "trait": "DbspT", "alias": "DbspA", "use": "DbspU", "macro": "dbsp_mac",
         "probe": "dbsp_p", "module": "dbsp_m", "method": "dbsp_f"}
type Intent = Literal["keep", "break", ""]
type ProjectKind = Literal["fixture", "harness", "workspace", "cases"]
type Command = Literal["check", "build"]


@dataclass(frozen=True)
class Project:
    kind: ProjectKind
    name: str = ""
    path: str = ""
    file: str = ""
    args: tuple[str, ...] = ()
    command: Command = "build"
    run: tuple[str, ...] | None = None
    program: str = ""
    filter: str = ""


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    projects: tuple[Project, ...] = (Project("fixture"),)
    seed: int = 0
    scripts: int = 3
    steps: int = 12
    saved: bool = True
    shrink: bool = True
    rounds: int = 30
    into: str = ""
    trace: bool = False
    jobs: int = 4
    keep: bool = False


# --- edits and the scaffold they change

@dataclass(frozen=True)
class Edit:
    op: str
    args: tuple[str, ...]

    def __str__(self) -> str:
        return f"{self.op}({', '.join(self.args)});"

    @property
    def kind(self) -> str:
        return KIND_OF.get(self.op, "item")


def parse_script(text: str) -> list[Edit] | str:
    """The edits of a script's text, or what is wrong with it (the line, and why)."""
    lines = text.splitlines()
    if not lines or lines[0].strip() != HEADER:
        return f"its first line is not `{HEADER}`"
    edits: list[Edit] = []
    for at, line in enumerate(lines[1:], 2):
        line = line.strip()
        if not line or line.startswith("//"):
            continue
        m = STATEMENT.fullmatch(line)
        if m is None:
            return f"line {at}: `{line}` is not `op(arg, ...);`"
        op = m[1]
        if op not in OPS:
            return f"line {at}: no op `{op}` (the ops are {', '.join(OPS)})"
        args = tuple(a.strip() for a in m[2].split(",")) if m[2].strip() else ()
        if len(args) != OPS[op]:
            return f"line {at}: `{op}` takes {OPS[op]} arguments, not {len(args)}"
        if bad := [a for a in args if not NAME.fullmatch(a)]:
            return f"line {at}: `{bad[0]}` is not a name"
        edits.append(Edit(op, args))
    return edits


def script_text(edits: Sequence[tuple[Edit, Intent]], notes: Sequence[str] = ()) -> str:
    lines = [HEADER, *(f"// {note}" for note in notes)]
    lines += [f"{edit} // {intent}" if intent else str(edit) for edit, intent in edits]
    return "\n".join(lines) + "\n"


@dataclass
class Item:
    """One thing of the scaffold. `name` is what it defines (none for an impl or a glob); `to`
    is the alias's type, the use's trait, the glob's module, the impl's self type, the struct a
    macro defines or the probe's value; `trait` is the impl's trait or the probe's method."""

    kind: Literal["struct", "trait", "alias", "use", "glob", "impl", "macro", "probe"]
    module: str
    name: str = ""
    to: str = ""
    trait: str = ""
    methods: list[str] = field(default_factory=list[str])
    derives: list[str] = field(default_factory=list[str])


@dataclass
class Model:
    """The scaffold: its modules (name, inline or file; a name twice is a duplicate module) and
    its items in the order they were added."""

    modules: list[tuple[str, str]] = field(default_factory=list[tuple[str, str]])
    items: list[Item] = field(default_factory=list[Item])

    def module_names(self) -> list[str]:
        return [TOP, *dict.fromkeys(name for name, _ in self.modules)]

    def has_module(self, name: str) -> bool:
        return name == TOP or any(m == name for m, _ in self.modules)

    def of(self, kind: str) -> list[Item]:
        return [i for i in self.items if i.kind == kind]

    def first(self, kind: str, name: str) -> Item | None:
        return next((i for i in self.items if i.kind == kind and i.name == name), None)

    def named(self, name: str) -> Item | None:
        return next((i for i in self.items if i.name == name and i.kind not in ("impl", "glob")), None)

    def where(self, name: str) -> str | None:
        """The module that defines `name` in the type, value or trait namespace."""
        for i in self.items:
            if i.kind in ("struct", "trait", "alias", "use", "probe") and i.name == name:
                return i.module
            if i.kind == "macro" and i.to == name:
                return i.module
        return None

    def trait_of(self, name: str) -> str | None:
        """The trait `name` is, through `use`s."""
        seen: set[str] = set()
        while name not in seen:
            seen.add(name)
            if self.first("trait", name) is not None:
                return name
            use = self.first("use", name)
            if use is None:
                return None
            name = use.to
        return None

    def type_of(self, name: str) -> str | None:
        """The struct `name` is, through aliases."""
        seen: set[str] = set()
        while name not in seen:
            seen.add(name)
            if self.first("struct", name) is not None or any(i.to == name for i in self.of("macro")):
                return name
            alias = self.first("alias", name)
            if alias is None:
                return None
            name = alias.to
        return None

    def types(self) -> list[str]:
        return [i.name for i in self.of("struct")] + [i.to for i in self.of("macro")]

    def referenced(self) -> Counter[str]:
        """How many times each name is used: by an item's `to` or `trait`, a glob's module and a
        probe's method."""
        found: Counter[str] = Counter()
        for i in self.items:
            for name in (i.to, i.trait):
                if name:
                    found[name] += 1
        return found


def apply(model: Model, edit: Edit) -> None:
    """`edit` made to `model`. Total: an edit of something that is not there changes nothing."""
    a = edit.args
    op = edit.op
    items = model.items
    if op == "add_mod":
        if a[0] != TOP and a[1] in MOD_KINDS:
            model.modules.append((a[0], a[1]))
    elif op == "remove_mod":
        model.modules = [(m, k) for m, k in model.modules if m != a[0]]
        model.items = [i for i in items if i.module != a[0]]
    elif op in ("add_struct", "add_trait", "add_alias", "add_use", "add_macro", "add_probe", "add_impl", "add_glob"):
        if not model.has_module(a[0]):
            return
        made = {
            "add_struct": lambda: Item("struct", a[0], a[1]),
            "add_trait": lambda: Item("trait", a[0], a[1]),
            "add_alias": lambda: Item("alias", a[0], a[1], to=a[2]),
            "add_use": lambda: Item("use", a[0], a[1], to=a[2]),
            "add_macro": lambda: Item("macro", a[0], a[1], to=a[2]),
            "add_probe": lambda: Item("probe", a[0], a[1], to=a[2], trait=a[3]),
            "add_impl": lambda: Item("impl", a[0], to=a[2], trait=a[1]),
            "add_glob": lambda: Item("glob", a[0], to=a[1]),
        }[op]()
        items.append(made)
    elif op in ("retarget_alias", "repoint_use"):
        found = model.first("alias" if op == "retarget_alias" else "use", a[0])
        if found is not None:
            found.to = a[1]
    elif op == "remove_impl":
        at = next((n for n, i in enumerate(items) if i.kind == "impl" and (i.module, i.trait, i.to) == a), None)
        if at is not None:
            del items[at]
    elif op == "remove_glob":
        at = next((n for n, i in enumerate(items) if i.kind == "glob" and (i.module, i.to) == a), None)
        if at is not None:
            del items[at]
    elif op in ("add_derive", "remove_derive"):
        found = model.first("struct", a[0])
        if found is None or a[1] not in DERIVES:
            return
        if op == "add_derive":
            found.derives.append(a[1])
        elif a[1] in found.derives:
            found.derives.remove(a[1])
    elif op in ("add_method", "remove_method"):
        found = model.first("trait", a[0])
        if found is None:
            return
        if op == "add_method":
            found.methods.append(a[1])
        elif a[1] in found.methods:
            found.methods.remove(a[1])
    elif op == "remove":
        at = next((n for n, i in enumerate(items) if i.name == a[0] and i.kind not in ("impl", "glob")), None)
        if at is not None:
            del items[at]
    elif op == "rename":
        old, new = a
        model.modules = [(new if m == old else m, k) for m, k in model.modules]
        for i in items:
            i.module = new if i.module == old else i.module
            i.name = new if i.name == old else i.name
            i.to = new if i.to == old else i.to
            i.trait = new if i.trait == old else i.trait
            i.methods = [new if m == old else m for m in i.methods]
    elif op == "rename_def":
        old, new = a
        if any(m == old for m, _ in model.modules):
            model.modules = [(new if m == old else m, k) for m, k in model.modules]
            for i in items:
                i.module = new if i.module == old else i.module
            return
        found = model.named(old)
        if found is not None:
            found.name = new
            return
        for i in model.of("trait"):
            if old in i.methods:
                i.methods[i.methods.index(old)] = new
                return


def path(model: Model, here: str, name: str) -> str:
    """`name` as written in module `here`: relative to it, through `super` for depth one."""
    there = model.where(name)
    if there is None or there == here:
        return f"self::{name}"
    if here == TOP:
        return f"self::{there}::{name}"
    if there == TOP:
        return f"super::{name}"
    return f"super::{there}::{name}"


def module_path(model: Model, here: str, name: str) -> str:
    if name == here:
        return "self"
    if here == TOP:
        return f"self::{name}"
    return "super" if name == TOP else f"super::{name}"


def render_item(model: Model, i: Item) -> str:
    here = i.module
    if i.kind == "struct":
        derive = f"#[derive({', '.join(i.derives)})]\n" if i.derives else ""
        return f"{derive}{ALLOW}\npub(crate) struct {i.name};"
    if i.kind == "trait":
        methods = "".join(f"    fn {m}(&self) -> u32 {{\n        {n + 1}\n    }}\n" for n, m in enumerate(i.methods))
        return f"{ALLOW}\npub(crate) trait {i.name} {{\n{methods}}}"
    if i.kind == "alias":
        return f"{ALLOW}\npub(crate) type {i.name} = {path(model, here, i.to)};"
    if i.kind == "use":
        return f"{ALLOW}\npub(crate) use {path(model, here, i.to)} as {i.name};"
    if i.kind == "glob":
        return f"{ALLOW}\npub(crate) use {module_path(model, here, i.to)}::*;"
    if i.kind == "impl":
        return f"impl {path(model, here, i.trait)} for {path(model, here, i.to)} {{}}"
    if i.kind == "macro":
        return (f"{ALLOW}\nmacro_rules! {i.name} {{\n    () => {{\n        {ALLOW}\n        pub(crate) struct {i.to};\n"
                f"    }};\n}}\n{i.name}!();")
    value = path(model, here, i.to)
    body = {"clone": f"let _ = {value}.clone();", "eq": f"let _ = {value} == {value};",
            "default": f"let _: {value} = Default::default();"}.get(i.trait, f"let _ = {value}.{i.trait}();")
    return f"{ALLOW}\npub(crate) fn {i.name}() {{\n    {body}\n}}"


def indent(text: str) -> str:
    return "\n".join(("    " + line) if line else "" for line in text.splitlines())


def render(model: Model) -> tuple[str, dict[str, str]]:
    """The scaffold's block (from BEGIN to END) and the file of each out-of-line module, by its
    name beside the block's file (`<module>.rs`). A module named twice holds its items once."""
    top = [render_item(model, i) for i in model.items if i.module == TOP]
    files: dict[str, str] = {}
    seen: set[str] = set()
    for name, kind in model.modules:
        body = [] if name in seen else [render_item(model, i) for i in model.items if i.module == name]
        seen.add(name)
        text = "\n\n".join(body)
        if kind == "inline":
            top.append(f"{ALLOW}\npub(crate) mod {name} {{\n{indent(text)}\n}}" if text else f"pub(crate) mod {name} {{}}")
        else:
            top.append(f"{ALLOW}\npub(crate) mod {name};")
            files.setdefault(f"{name}.rs", text + "\n" if text else "")
    return "\n".join([BEGIN, *"\n\n".join(top).splitlines(), END]) + "\n", files


def edited(base: str, model: Model) -> tuple[str, dict[str, str]]:
    """The file the scaffold goes in, with it, and the scaffold's module files."""
    block, files = render(model)
    return base.rstrip("\n") + "\n\n" + block, files


def module_dir(file: str, root: bool) -> str:
    """Where `mod m;` in `file` looks for `m.rs` (posix, relative): beside a crate root or a
    `mod.rs`, else in the directory named after the file."""
    p = PurePosixPath(file)
    return str(p.parent) if root or p.name == "mod.rs" else str(p.parent / p.stem)


def is_root(file: str) -> bool:
    return PurePosixPath(file).name in ("lib.rs", "main.rs")


# --- generating scripts

class Gen:
    """A script being written, the scaffold it makes kept beside it. Only `random()` and
    `randrange` are drawn, whose sequences Python keeps across versions."""

    def __init__(self, rng: random.Random):
        self.rng = rng
        self.model = Model()
        self.counts: Counter[str] = Counter()
        self.edits: list[tuple[Edit, Intent]] = []
        self.ops = [op for op, weight in WEIGHTS for _ in range(weight)]

    def name(self, kind: str) -> str:
        stem = STEMS[kind]
        self.counts[stem] += 1
        return f"{stem}{self.counts[stem]}"

    def pick[T](self, options: Sequence[T]) -> T:
        return options[self.rng.randrange(len(options))]

    def chance(self, p: float) -> bool:
        return self.rng.random() < p

    def emit(self, op: str, intent: Intent, *args: str) -> tuple[Edit, Intent]:
        edit = Edit(op, args)
        apply(self.model, edit)
        self.edits.append((edit, intent))
        return edit, intent

    def step(self) -> None:
        for _ in range(40):
            op = self.pick(self.ops)
            bad = self.chance(BAD)
            if getattr(self, f"p_{op}")(bad) is not None:
                return
        self.p_add_struct(False)

    def module(self) -> str:
        return self.pick(self.model.module_names())

    def names_in(self, kind: str, module: str) -> list[str]:
        return [i.name for i in self.model.of(kind) if i.module == module]

    def p_add_mod(self, bad: bool) -> tuple[Edit, Intent] | None:
        if bad and self.model.modules:
            return self.emit("add_mod", "break", self.pick(self.model.modules)[0], self.pick(MOD_KINDS))
        return self.emit("add_mod", "keep", self.name("module"), self.pick(MOD_KINDS))

    def p_remove_mod(self, bad: bool) -> tuple[Edit, Intent] | None:
        names = self.model.module_names()[1:]
        used = self.model.referenced()
        defs = {m: {i.name for i in self.model.items if i.module == m and i.name} | {
            i.to for i in self.model.of("macro") if i.module == m} | {m} for m in names}
        outside = {m: sum(used[n] for n in defs[m]) - sum(
            1 for i in self.model.items if i.module == m for n in (i.to, i.trait) if n in defs[m]) for m in names}
        choose = [m for m in names if (outside[m] > 0) == bad]
        if not choose:
            return None
        return self.emit("remove_mod", "break" if bad else "keep", self.pick(choose))

    def p_add_struct(self, bad: bool) -> tuple[Edit, Intent] | None:
        m = self.module()
        if bad and (same := self.names_in("struct", m)):
            return self.emit("add_struct", "break", m, self.pick(same))
        return self.emit("add_struct", "keep", m, self.name("struct"))

    def p_add_trait(self, bad: bool) -> tuple[Edit, Intent] | None:
        m = self.module()
        if bad and (same := self.names_in("trait", m)):
            return self.emit("add_trait", "break", m, self.pick(same))
        return self.emit("add_trait", "keep", m, self.name("trait"))

    def p_add_alias(self, bad: bool) -> tuple[Edit, Intent] | None:
        types = self.model.types()
        if bad:
            return self.emit("add_alias", "break", self.module(), self.name("alias"), self.name("struct"))
        if not types:
            return None
        return self.emit("add_alias", "keep", self.module(), self.name("alias"), self.pick(types))

    def p_retarget_alias(self, bad: bool) -> tuple[Edit, Intent] | None:
        aliases = self.model.of("alias")
        if not aliases:
            return None
        alias = self.pick(aliases)
        if bad:
            return self.emit("retarget_alias", "break", alias.name, self.name("struct"))
        others = [t for t in self.model.types() if t != alias.to]
        if not others:
            return None
        return self.emit("retarget_alias", "keep", alias.name, self.pick(others))

    def p_add_impl(self, bad: bool) -> tuple[Edit, Intent] | None:
        model = self.model
        if bad:
            impls = model.of("impl")
            if impls and self.chance(0.6):
                i = self.pick(impls)
                return self.emit("add_impl", "break", i.module, i.trait, i.to)
            selves = model.types() or [self.name("struct")]
            return self.emit("add_impl", "break", self.module(), self.name("trait"), self.pick(selves))
        for _ in range(8):
            m = self.module()
            traits = [i.name for i in model.of("trait")] + self.names_in("use", m)
            selves = model.types() + [i.name for i in model.of("alias")]
            if not traits or not selves:
                return None
            t, s = self.pick(traits), self.pick(selves)
            key = (model.trait_of(t), model.type_of(s))
            if None in key or any((model.trait_of(i.trait), model.type_of(i.to)) == key for i in model.of("impl")):
                continue
            return self.emit("add_impl", "keep", m, t, s)
        return None

    def p_remove_impl(self, bad: bool) -> tuple[Edit, Intent] | None:
        impls = self.model.of("impl")
        if not impls:
            return None
        i = self.pick(impls)
        used = any(p.trait in self.methods_of(i.trait) and self.model.type_of(p.to) == self.model.type_of(i.to)
                   for p in self.model.of("probe"))
        return self.emit("remove_impl", "break" if used else "keep", i.module, i.trait, i.to)

    def methods_of(self, trait: str) -> list[str]:
        found = self.model.trait_of(trait)
        item = self.model.first("trait", found) if found else None
        return item.methods if item else []

    def p_add_use(self, bad: bool) -> tuple[Edit, Intent] | None:
        traits = [i.name for i in self.model.of("trait")]
        if bad or not traits:
            if not bad:
                return None
            return self.emit("add_use", "break", self.module(), self.name("use"), self.name("trait"))
        return self.emit("add_use", "keep", self.module(), self.name("use"), self.pick(traits))

    def p_repoint_use(self, bad: bool) -> tuple[Edit, Intent] | None:
        uses = self.model.of("use")
        if not uses:
            return None
        use = self.pick(uses)
        if bad:
            return self.emit("repoint_use", "break", use.name, self.name("trait"))
        others = [i.name for i in self.model.of("trait") if i.name != use.to]
        if not others:
            return None
        return self.emit("repoint_use", "keep", use.name, self.pick(others))

    def p_add_glob(self, bad: bool) -> tuple[Edit, Intent] | None:
        m = self.module()
        if bad:
            return self.emit("add_glob", "break", m, self.name("module"))
        others = [n for n in self.model.module_names() if n != m]
        if not others:
            return None
        return self.emit("add_glob", "keep", m, self.pick(others))

    def p_remove_glob(self, bad: bool) -> tuple[Edit, Intent] | None:
        globs = self.model.of("glob")
        if not globs:
            return None
        g = self.pick(globs)
        return self.emit("remove_glob", "keep", g.module, g.to)

    def p_add_macro(self, bad: bool) -> tuple[Edit, Intent] | None:
        m = self.module()
        if bad and (same := self.names_in("struct", m)):
            return self.emit("add_macro", "break", m, self.name("macro"), self.pick(same))
        return self.emit("add_macro", "keep", m, self.name("macro"), self.name("struct"))

    def p_add_derive(self, bad: bool) -> tuple[Edit, Intent] | None:
        structs = self.model.of("struct")
        if not structs:
            return None
        s = self.pick(structs)
        if bad:
            wrong = [d for d, need in NEEDS.items() if need not in s.derives and d not in s.derives]
            return self.emit("add_derive", "break", s.name, self.pick(wrong or s.derives or ["Clone"]))
        fits = [d for d in DERIVES if d not in s.derives and NEEDS.get(d, d) in (*s.derives, d)]
        if not fits:
            return None
        return self.emit("add_derive", "keep", s.name, self.pick(fits))

    def p_remove_derive(self, bad: bool) -> tuple[Edit, Intent] | None:
        structs = [s for s in self.model.of("struct") if s.derives]
        if not structs:
            return None
        s = self.pick(structs)
        probed = {DERIVE_PROBES[p.trait] for p in self.model.of("probe") if p.trait in DERIVE_PROBES and
                  self.model.type_of(p.to) == s.name}
        needed = {NEEDS[d] for d in s.derives if d in NEEDS} | probed
        choose = [d for d in s.derives if (d in needed) == bad]
        if not choose:
            return None
        return self.emit("remove_derive", "break" if bad else "keep", s.name, self.pick(choose))

    def p_add_method(self, bad: bool) -> tuple[Edit, Intent] | None:
        traits = self.model.of("trait")
        if not traits:
            return None
        t = self.pick(traits)
        if bad and t.methods:
            return self.emit("add_method", "break", t.name, self.pick(t.methods))
        return self.emit("add_method", "keep", t.name, self.name("method"))

    def p_remove_method(self, bad: bool) -> tuple[Edit, Intent] | None:
        traits = [t for t in self.model.of("trait") if t.methods]
        if not traits:
            return None
        t = self.pick(traits)
        called = {p.trait for p in self.model.of("probe")}
        choose = [m for m in t.methods if (m in called) == bad]
        if not choose:
            return None
        return self.emit("remove_method", "break" if bad else "keep", t.name, self.pick(choose))

    def p_add_probe(self, bad: bool) -> tuple[Edit, Intent] | None:
        model = self.model
        if self.chance(0.3):
            derived = [(s.name, p) for s in model.of("struct") for p, d in DERIVE_PROBES.items() if d in s.derives]
            if derived and not bad:
                s, method = self.pick(derived)
                return self.emit("add_probe", "keep", self.module(), self.name("probe"), s, method)
        calls = [(i, t, m) for i in model.of("impl") if (t := model.trait_of(i.trait)) and model.type_of(i.to)
                 for m in self.methods_of(t)]
        if not calls:
            return None
        i, t, method = self.pick(calls)
        home = model.where(t) or TOP
        value = model.type_of(i.to) or i.to
        if bad:
            away = [m for m in model.module_names() if m != home]
            if not away:
                return None
            return self.emit("add_probe", "break", self.pick(away), self.name("probe"), value, method)
        return self.emit("add_probe", "keep", home, self.name("probe"), value, method)

    def p_remove(self, bad: bool) -> tuple[Edit, Intent] | None:
        used = self.model.referenced()
        named = [i for i in self.model.items if i.name and i.kind not in ("impl", "glob")]
        choose = [i for i in named if (used[i.name] + (used[i.to] if i.kind == "macro" else 0) > 0) == bad]
        if not choose:
            return None
        return self.emit("remove", "break" if bad else "keep", self.pick(choose).name)

    def p_rename(self, bad: bool) -> tuple[Edit, Intent] | None:
        named = [i for i in self.model.items if i.name and i.kind not in ("impl", "glob", "macro")]
        if not named:
            return None
        i = self.pick(named)
        return self.emit("rename", "keep", i.name, self.name(i.kind))

    def p_rename_def(self, bad: bool) -> tuple[Edit, Intent] | None:
        used = self.model.referenced()
        named = [i for i in self.model.items if i.name and i.kind not in ("impl", "glob", "macro")]
        choose = [i for i in named if (used[i.name] > 0) == bad]
        if not choose:
            return None
        i = self.pick(choose)
        return self.emit("rename_def", "break" if bad else "keep", i.name, self.name(i.kind))

    def ensure_impl(self) -> None:
        """At least one `add_impl` that keeps the program, for the cost check."""
        if any(e.op == "add_impl" and intent == "keep" for e, intent in self.edits):
            return
        if self.p_add_impl(False) is not None:
            return
        trait = self.emit("add_trait", "keep", TOP, self.name("trait"))[0].args[1]
        struct = self.emit("add_struct", "keep", TOP, self.name("struct"))[0].args[1]
        self.emit("add_impl", "keep", TOP, trait, struct)


def generate(seed: int, index: int, steps: int) -> list[tuple[Edit, Intent]]:
    """Script `index` of `seed`: `steps` edits and, when none of them did, one `add_impl` that
    keeps the program (with the trait and struct it needs)."""
    g = Gen(random.Random(f"blaze-dbsp-edits:{seed}:{index}"))
    for _ in range(steps):
        g.step()
    g.ensure_impl()
    return g.edits


# --- the projects

@dataclass(frozen=True)
class Target:
    """A project made ready to run scripts on: a case (its root takes the scaffold), or a
    workspace copied and fetched (`source`) with the file that takes it."""

    name: str
    case: cases.Case | None = None
    source: Path | None = None
    file: str = ""
    args: tuple[str, ...] = ()
    command: Command = "build"
    run: tuple[str, ...] | None = None
    program: str = ""


def slug(name: str) -> str:
    return re.sub(r"[^A-Za-z0-9_.-]+", "-", name).strip("-")


def copy_tree(source: Path, to: Path) -> None:
    shutil.rmtree(to, ignore_errors=True)
    shutil.copytree(source, to, symlinks=True, ignore=shutil.ignore_patterns("target", "target-ref", ".git"))


def case_targets(project: Project) -> tuple[list[Target], list[tuple[str, str]]]:
    """Each case of `project.path` (a directory's `.rs` files, or one file) whose name has
    `filter`, and each file that is not one, with why."""
    root = Path(project.path).resolve()
    files = [root] if root.is_file() else sorted(root.glob("*.rs")) if root.is_dir() else []
    if not files:
        return [], [(project.path, f"no case at {root}")]
    found: list[Target] = []
    wrong: list[tuple[str, str]] = []
    for path in files:
        name = f"{path.parent.name}/{path.name}"
        if project.filter not in name:
            continue
        got = case_of(name, path, path.read_text(errors="replace"))
        if isinstance(got, str):
            wrong.append((name, got))
        else:
            found.append(Target(name, case=got))
    return found, wrong


def case_of(name: str, path: Path, text: str) -> cases.Case | str:
    """The case at `path` as a project: what its header expects is not used (edits change what
    the program does), and it is built once a step whatever its area."""
    got = cases.case_of(name, path, text)
    return got if isinstance(got, str) else replace(got, twice=False, refused=frozenset())


def workspace_target(project: Project, work: Path) -> Target:
    """A fixture, harness or workspace project copied under `work` (each script copies it again),
    its dependencies fetched once into its `Cargo.lock`."""
    name = project.name or project.kind
    source = work / "src" / slug(name)
    file, args, run, program = project.file, project.args, project.run, project.program
    if project.kind == "fixture":
        shutil.rmtree(source, ignore_errors=True)
        fixture.write_workspace(source)
        file, program = file or FIXTURE_FILE, program or FIXTURE_PROGRAM
        run = () if run is None else run
    elif project.kind == "harness":
        blaze_cli: Any = importlib.import_module("blaze_cli")
        spec = blaze_cli.PROJECTS.get(project.name)
        if spec is None:
            raise engine.Failed(f"no project {project.name!r} in harness/blaze_cli.py's PROJECTS")
        copy_tree(Path(blaze_cli.fetch(project.name, spec)), source)
        file = file or spec["edits"][0][0]
        args = args or tuple(spec.get("build", ()))
        run = tuple(spec["run"]) if run is None and "run" in spec else run
    else:
        if not project.path or not project.file:
            raise engine.Failed(f"a workspace project needs `path` and `file`: {project}")
        copy_tree(Path(project.path).resolve(), source)
    if not (source / file).is_file():
        raise engine.Failed(f"{name}: no file {file} in {source}")
    if BEGIN in (source / file).read_text(errors="replace"):
        raise engine.Failed(f"{name}: {file} already holds a scaffold ({BEGIN})")
    engine.run(f"{name}: cargo fetch", ["cargo", "fetch"], source, engine.reference_env(work / "fetch-target"), 1800,
               check=True)
    return Target(name, source=source, file=file, args=args, command=project.command, run=run, program=program)


# --- one step, built both ways

type Category = Literal["same", "accepted what the build from nothing rejects",
                        "rejected what the build from nothing accepts", "other codes", "other run", "not blaze's",
                        "error"]


@dataclass(frozen=True)
class Built:
    """One build of a step: what blaze said (cases.Said: its error keys, or why nothing can be
    read), and its program's run when it built one to run."""

    said: cases.Said
    run: cases.Run | None = None


@dataclass(frozen=True)
class Stepped:
    """One step: the incremental build and the one from nothing, the verdict on the two, and
    what the incremental build printed (for the cost check)."""

    verdict: report.Verdict
    category: Category
    detail: str
    incremental: Built | None = None
    scratch: Built | None = None
    stdout: str = ""
    ws: str = ""


def shown(keys: frozenset[str]) -> str:
    return ", ".join(cases.shown_key(k) for k in sorted(keys)) or "none"


def run_shown(ran: cases.Run) -> str:
    if ran.error is not None:
        return f"did not finish: {ran.error}"
    return f"exit {ran.status}, {len(ran.stdout.splitlines())} lines" + (f", panics {'; '.join(ran.panics[:2])}"
                                                                          if ran.panics else "")


def run_aspect(a: cases.Run, b: cases.Run) -> str:
    """What two runs differ in, as far as shrinking keeps it."""
    if (a.error is None) != (b.error is None):
        return "finished"
    if a.status != b.status:
        return "status"
    if a.panics != b.panics:
        return "panics"
    return "stdout"


def compare(inc: Built, scratch: Built) -> tuple[report.Verdict, Category, str]:
    """The incremental build of a step against the build of the same files from nothing."""
    for side, built in (("incremental", inc), ("from nothing", scratch)):
        if built.said.error is not None:
            return "error", "error", f"the build {side}: {built.said.error}"
        if built.said.who is not None:
            return "fail", "not blaze's", f"the build {side} was not blaze's: {built.said.who}"
    a, b = inc.said.keys, scratch.said.keys
    if a and not b:
        return "fail", "rejected what the build from nothing accepts", f"the incremental build gives {shown(a)}"
    if b and not a:
        return "fail", "accepted what the build from nothing rejects", f"the build from nothing gives {shown(b)}"
    if a != b:
        return "fail", "other codes", f"incremental {shown(a)}; from nothing {shown(b)}"
    if a:
        return "pass", "same", f"both reject with {shown(a)}"
    if (inc.run is None) != (scratch.run is None):
        return "error", "error", "one build made a program to run and the other did not"
    if inc.run is not None and scratch.run is not None and inc.run != scratch.run:
        return "fail", "other run", (f"{run_aspect(inc.run, scratch.run)}: incremental {run_shown(inc.run)}; "
                                     f"from nothing {run_shown(scratch.run)}")
    return "pass", "same", "both accept" + (f"; the program: {run_shown(inc.run)}" if inc.run is not None else "")


def signature(s: Stepped) -> tuple[str, ...] | None:
    """What a mismatch is, as far as shrinking keeps it: its category and the keys it is about
    (output moves as edits are deleted; the keys and what differs do not)."""
    if s.verdict != "fail" or s.incremental is None or s.scratch is None:
        return None
    a, b = sorted(s.incremental.said.keys), sorted(s.scratch.said.keys)
    if s.category == "rejected what the build from nothing accepts":
        return (s.category, *a)
    if s.category == "accepted what the build from nothing rejects":
        return (s.category, *b)
    if s.category == "other codes":
        return (s.category, ",".join(a), ",".join(b))
    if s.category == "other run" and s.incremental.run is not None and s.scratch.run is not None:
        return (s.category, run_aspect(s.incremental.run, s.scratch.run))
    return (s.category,)


def write_files(base: Path, files: dict[str, str], written: set[Path]) -> set[Path]:
    """`files` (by path under `base`) written, each written before and not among them removed;
    what is written now."""
    now = {base / name for name in files}
    for gone in written - now:
        gone.unlink(missing_ok=True)
    for path, text in ((base / name, text) for name, text in files.items()):
        path.parent.mkdir(parents=True, exist_ok=True)
        if not path.is_file() or path.read_text(errors="replace") != text:
            path.write_text(text)
    return now


def targeted(stdout: str) -> list[dict[str, Any]]:
    """The JSON lines of a build that name a target: its artifacts and the messages of its builds."""
    return [m for line in stdout.splitlines() if (m := cases.json_line(line)) is not None
            and isinstance(m.get("target"), dict)]


def artifacts(stdout: str) -> list[dict[str, Any]]:
    return [m for m in targeted(stdout) if m.get("reason") == "compiler-artifact"]


def crate_of(stdout: str, ws: Path, file: str) -> str | None:
    """The target (by its name) whose sources hold `file` under `ws`: the one whose root's
    directory is the deepest above it, of the lines that name a target (a target that did not
    build has messages and no artifact)."""
    at = (ws / file).resolve()
    best: tuple[int, str] | None = None
    for m in targeted(stdout):
        src, name = m["target"].get("src_path"), m["target"].get("name")
        if not isinstance(src, str) or not isinstance(name, str):
            continue
        root = Path(src).resolve()
        if root == at:
            return name
        if at.is_relative_to(root.parent) and (best is None or len(root.parent.parts) > best[0]):
            best = (len(root.parent.parts), name)
    return best[1] if best else None


def executable_of(stdout: str, program: str) -> Path | None:
    """The program a build made: the bin target named `program`, else the one bin built."""
    found = [(m["target"].get("name"), Path(m["executable"])) for m in artifacts(stdout)
             if isinstance(m.get("executable"), str)]
    if program:
        return next((exe for name, exe in found if name == program), None)
    return found[0][1] if len(found) == 1 else None


def run_with(program: Path, args: Sequence[str], cwd: Path, env: dict[str, str]) -> cases.Run:
    """`program ARGS` run in `cwd`, as cases.run_program runs a case's (which takes no arguments)."""
    if not program.is_file():
        return cases.Run(error=f"no program at {program}", started=False)
    try:
        ran = engine.run(f"running {program}", [str(program), *args], cwd, env, cases.RUN_SECS)
    except engine.Failed as e:
        return cases.Run(error=str(e).splitlines()[0])
    return cases.Run(ran.returncode, ran.stdout, cases.panics_of(ran.stderr))


class Subject:
    """A project opened for one script's run: one blaze workspace under one daemon, every step
    written into it and built incrementally; and each step copied, files and all, into a
    workspace of its own built from nothing (an empty home, a copy of the standard library's
    store), whose daemon is stopped after it. `base` is the text of the file the scaffold goes
    in, as the project has it; `crate` the name of the target that file is in ("" until a build
    has named it)."""

    def __init__(self, runner: "Runner", target: Target, index: int):
        self.runner, self.target, self.index = runner, target, index
        work = runner.work
        self.ws, self.home = work / "ws" / f"e{index}", work / "h" / f"e{index}"
        self.scratch_ws, self.scratch_home = work / "ws" / f"s{index}", work / "h" / f"s{index}"
        self.scratch_store = work / "ss" / f"s{index}"
        self.written: set[Path] = set()
        self.crate = self.program = ""
        flags = " ".join(cases.COMMON)
        if target.case is not None:
            case = target.case
            shutil.rmtree(self.ws, ignore_errors=True)
            members, out = cases.write_workspace(self.ws, index, [case])
            if out:
                raise engine.Failed(f"{target.name}: cannot be laid out as a package: {next(iter(out.values()))}")
            ((self.member, _),) = members.items()
            self.crate = self.member
            root = cases.root_of(self.ws, self.member, case)
            self.file = root.relative_to(self.ws).as_posix()
            self.base = Path(case.path).read_text(errors="replace")
            self.command: Command = "build" if case.kind == "bin" else "check"
            self.args = cases.check_args(case.kind, self.member)
            self.runs = case.kind == "bin"
        else:
            assert target.source is not None
            copy_tree(target.source, self.ws)
            self.member, self.file = "", target.file
            self.base = (target.source / target.file).read_text(errors="replace")
            self.command = target.command
            self.args = ["--message-format=json", *target.args]
            self.runs = target.run is not None and target.command == "build"
        if BEGIN in self.base:
            raise engine.Failed(f"{target.name}: {self.file} already holds a scaffold ({BEGIN})")
        self.dir = module_dir(self.file, is_root(self.file))
        self.extra = {"RUSTFLAGS": flags}
        self.env = runner.eng.env(self.home, runner.work / "store", extra=self.extra)

    def files(self, text: str, files: dict[str, str]) -> dict[str, str]:
        return {self.file: text, **{f"{self.dir}/{name}": body for name, body in files.items()}}

    def built(self, label: str, ws: Path, env: dict[str, str]) -> tuple[Built, str]:
        """`blaze COMMAND` of `ws`: what it said, its program's run, and what it printed."""
        try:
            ran = self.runner.eng.build(label, ws, env, self.args, command=self.command, timeout=cases.CHECK_SECS)
        except (engine.Failed, engine.Unmeasurable) as e:
            return Built(cases.Said(error=str(e))), ""
        said = cases.said_alone(ran.returncode, ran.stdout, ran.said(600), self.runner.eng.not_blazes(ran), self.command)
        if not self.crate:
            self.crate = crate_of(ran.stdout, ws, self.file) or ""
        if said.keys or said.error is not None or said.who is not None or not self.runs:
            return Built(said), ran.stdout
        if self.target.case is not None:
            exe = ws / "target" / "debug" / (self.member + engine.EXE)
            return Built(said, cases.run_program(exe, self.runner.work / "cwd", self.runner.run_env)), ran.stdout
        exe = executable_of(ran.stdout, self.target.program)
        if exe is None:
            why = f"the build made no one program {self.target.program or '(name it with `program`)'}"
            return Built(cases.Said(error=why)), ran.stdout
        return Built(said, run_with(exe, cast(tuple[str, ...], self.target.run), ws, self.runner.run_env)), ran.stdout

    def from_nothing(self, label: str) -> Built:
        """The workspace as it is now, copied without its target, built by a daemon of its own
        with an empty home and the standard library's store alone."""
        copy_tree(self.ws, self.scratch_ws)
        shutil.rmtree(self.scratch_home, ignore_errors=True)
        shutil.rmtree(self.scratch_store, ignore_errors=True)
        shutil.copytree(self.runner.std_store, self.scratch_store, symlinks=True)
        env = self.runner.eng.env(self.scratch_home, self.scratch_store, extra=self.extra)
        try:
            built, _ = self.built(f"blaze {self.command} of {self.target.name} {label}, from nothing", self.scratch_ws, env)
            return built
        finally:
            try:
                self.runner.eng.stop(self.scratch_ws, env)
            except engine.Failed:
                pass
            for path in (self.scratch_ws, self.scratch_home, self.scratch_store):
                shutil.rmtree(path, ignore_errors=True)

    def step(self, label: str, text: str, files: dict[str, str]) -> Stepped:
        """The scaffold's file as `text` and its module files `files`, built both ways."""
        self.written = write_files(self.ws, self.files(text, files), self.written)
        inc, stdout = self.built(f"blaze {self.command} of {self.target.name} {label}", self.ws, self.env)
        scratch = self.from_nothing(label)
        verdict, category, detail = compare(inc, scratch)
        return Stepped(verdict, category, detail, inc, scratch, stdout, str(self.ws))

    def trace(self, to: Path) -> None:
        """The daemon's trace of the last build, written to `to`."""
        self.runner.eng.trace(self.ws, self.env, to)

    def close(self, keep: bool) -> str | None:
        """The daemon stopped, the workspace removed unless `keep`; why it would not stop."""
        try:
            self.runner.eng.stop(self.ws, self.env)
        except engine.Failed as e:
            return str(e)
        finally:
            if not keep:
                shutil.rmtree(self.ws, ignore_errors=True)
                shutil.rmtree(self.home, ignore_errors=True)
        return None


# --- the cost check and the trace

def maintained(stdout: str) -> tuple[list[dict[str, Any]], list[str]]:
    """The `blaze-maintained` lines of a run's stdout, and those of another shape (as text)."""
    lines: list[dict[str, Any]] = []
    wrong: list[str] = []
    for text in stdout.splitlines():
        m = cases.json_line(text)
        if m is None or m.get("reason") != MAINTAINED:
            continue
        work = m.get("work")
        good = (isinstance(m.get("crate"), str) and isinstance(m.get("maintainer"), str) and isinstance(work, dict)
                and all(isinstance(w := cast(dict[str, Any], work).get(f), int) and not isinstance(w, bool) and w >= 0
                        for f in WORK_FIELDS))
        if good:
            lines.append(m)
        else:
            wrong.append(text)
    return lines, wrong


def cost(stdout: str, crate: str) -> tuple[report.Verdict, str, dict[str, report.Number]]:
    """What the maintainers did for one `add_impl`, against design section 10: for `crate`, each
    maintainer one op and at most two slices; for every other crate, nothing."""
    lines, wrong = maintained(stdout)
    if wrong:
        return "fail", f"a `{MAINTAINED}` line of another shape: {wrong[0][:300]}", {}
    if not lines:
        return "fail", f"no `{MAINTAINED}` line, so the maintainers' work is not known: {REQUESTED}", {}
    if not crate:
        return "error", "the edited crate is not known: the build named no target holding the file", {}
    totals: dict[tuple[str, str], Counter[str]] = {}
    for line in lines:
        counts = totals.setdefault((line["crate"], line["maintainer"]), Counter())
        for f in WORK_FIELDS:
            counts[f] += line["work"][f]
    mine = {maintainer: counts for (krate, maintainer), counts in totals.items() if krate == crate}
    if not mine:
        return "fail", f"no `{MAINTAINED}` line of the edited crate {crate}, only of {sorted({k for k, _ in totals})}", {}
    numbers: dict[str, report.Number] = {f"{maintainer}_{f}": counts[f] for maintainer, counts in sorted(mine.items())
                                         for f in ("ops", "slices")}
    wrong_here = [f"{maintainer}: ops {c['ops']}, slices {c['slices']}" for maintainer, c in sorted(mine.items())
                  if c["ops"] != MAX_OPS or c["slices"] > MAX_SLICES]
    if wrong_here:
        return "fail", (f"one impl added to {crate} cost more than its facts (ops == {MAX_OPS}, slices <= {MAX_SLICES}): "
                        + "; ".join(wrong_here)), numbers
    others = [f"{krate} ({maintainer}): " + ", ".join(f"{f} {c[f]}" for f in WORK_FIELDS if c[f])
              for (krate, maintainer), c in sorted(totals.items()) if krate != crate and any(c.values())]
    if others:
        return "fail", f"one impl added to {crate} made the maintainers work on other crates: {'; '.join(others)}", numbers
    return "pass", f"{crate}: " + "; ".join(f"{m} ops {c['ops']}, slices {c['slices']}" for m, c in sorted(mine.items())), numbers


def rows(model: Model) -> Counter[tuple[str, str, str]]:
    """Each impl of the scaffold as the impl tables hold it, (trait, module, self type), the
    trait and the type through `use`s and aliases; a derive as an impl of each trait it
    implements. An impl of no trait is in no trait's slice."""
    found: Counter[tuple[str, str, str]] = Counter()
    for i in model.of("impl"):
        trait = model.trait_of(i.trait)
        if trait is not None:
            found[(trait, i.module, model.type_of(i.to) or i.to)] += 1
    for s in model.of("struct"):
        for d in s.derives:
            for t in DERIVED.get(d, (d,)):
                found[(f"core::{t}", s.module, s.name)] += 1
    return found


def touched(before: Model, after: Model) -> tuple[int, int]:
    """How many traits' impl sets, and how many (trait, self type) buckets, an edit changed."""
    was, now = rows(before), rows(after)
    changed = {r for r in was.keys() | now.keys() if was[r] != now[r]}
    return len({t for t, _, _ in changed}), len({(t, s) for t, _, s in changed})


def dbsp_case(proof: Path, kind: str, moved: tuple[int, int], before: Path, after: Path) -> tuple[report.Verdict, str]:
    """blaze-v2-proof's `dbsp` case of `kind` on the traces of the build before an edit and after
    it, given the traits and buckets the edit `moved`."""
    given = json.dumps({"dbsp": kind, "traits": moved[0], "buckets": moved[1], "before": str(before),
                        "after": str(after)})
    try:
        ran = engine.run(f"blaze-v2-proof dbsp {kind}", [str(proof), given], HERE, engine.scrubbed(), 600)
    except engine.Failed as e:
        return "error", str(e)
    if ran.returncode == 2:
        return "error", f"blaze-v2-proof could not check the traces: {ran.said(600)}"
    try:
        out = json.loads(ran.stdout)
        findings = [(str(f["side"]), str(f["detail"])) for f in out["findings"]]
    except (json.JSONDecodeError, KeyError, TypeError) as e:
        return "error", f"blaze-v2-proof printed no `dbsp` answer ({e}): {ran.said(600)}"
    bad = [detail for side, detail in findings if side != "Unmeasured"]
    if bad:
        return "fail", f"{len(bad)} findings: " + "; ".join(bad[:4])
    if findings:
        return "unmeasured", "; ".join(detail for _, detail in findings[:4])
    return "pass", f"the {kind} case holds"


# --- shrinking

def shrink(text: str, still: Callable[[list[str]], list[bool]], rounds: int) -> tuple[str, int]:
    """`text`, a script, made as small as deleting its edit lines can while `still` holds of it
    (asked of a pass's candidates at once: each line deleted alone), and how many batches it
    took. The header and the notes are kept. After a pass, every deletion that held alone is
    tried together once."""
    used = 0
    while used < rounds:
        lines = text.splitlines(keepends=True)
        at = [k for k, line in enumerate(lines) if STATEMENT.fullmatch(line.strip())]
        variants = ["".join(line for j, line in enumerate(lines) if j != k) for k in at]
        if not variants:
            break
        held = still(variants)
        used += 1
        good = [k for k, ok in zip(at, held) if ok]
        if not good:
            break
        single = min((v for v, ok in zip(variants, held) if ok), key=lambda v: (len(v), v))
        combined = "".join(line for j, line in enumerate(lines) if j not in set(good))
        if len(good) > 1 and used < rounds:
            used += 1
            text = combined if still([combined])[0] else single
        else:
            text = single
    return text, used


@dataclass(frozen=True)
class Plan:
    """One script on one project: its name, and its edits with their intents."""

    target: Target
    name: str
    edits: tuple[tuple[Edit, Intent], ...]
    seed: int | None = None
    index: int = 0


@dataclass(frozen=True)
class Outcome:
    name: str
    verdict: report.Verdict
    detail: str
    numbers: dict[str, report.Number] = field(default_factory=dict[str, report.Number])


@dataclass(frozen=True)
class Ran:
    """A script's run: every outcome, the steps, and the first that did not pass."""

    outcomes: tuple[Outcome, ...]
    steps: tuple[Stepped, ...]
    bad: int | None
    stopped: str | None


def first_bad(steps: Sequence[Stepped]) -> int | None:
    return next((k for k, s in enumerate(steps) if s.verdict != "pass"), None)


class Runner:
    """Scripts run on targets through `eng`, each step built incrementally and from nothing."""

    def __init__(self, eng: engine.Engine, work: Path, jobs: int, trace: bool, keep: bool):
        self.eng, self.work, self.jobs, self.trace, self.keep = eng, work, jobs, trace, keep
        self.run_env = engine.scrubbed()
        self.indices = itertools.count()
        self.proof = engine.proof_beside(str(eng.blaze))
        self.std_store = oracle.std_seed(eng, work)

    def open(self, target: Target) -> Subject:
        return Subject(self, target, next(self.indices))

    def run(self, target: Target, name: str, edits: Sequence[Edit], keep: bool | None = None) -> Ran:
        """`edits` on `target`, from its base, in one workspace under one daemon; stops at the
        first step that does not pass."""
        outcomes: list[Outcome] = []
        steps: list[Stepped] = []
        model = Model()
        try:
            subject = self.open(target)
        except engine.Failed as e:
            return Ran((Outcome(f"{target.name}/{name}", "error", str(e)),), (), None, None)
        traces = self.work / "traces" / f"{slug(target.name)}-{slug(name)}-{id(subject)}"
        stopped: str | None = None
        try:
            for k, edit in enumerate([None, *edits]):
                was = copy.deepcopy(model)
                if edit is not None:
                    apply(model, edit)
                label = "base" if edit is None else f"step {k} {edit}"
                text, files = edited(subject.base, model)
                try:
                    stepped = subject.step(label, text, files)
                except engine.Failed as e:
                    stepped = Stepped("error", "error", str(e))
                steps.append(stepped)
                outcomes.append(Outcome(f"{target.name}/{name}/{label}", stepped.verdict, stepped.detail))
                if edit is not None and edit.op == "add_impl" and stepped.verdict != "error":
                    verdict, detail, numbers = cost(stepped.stdout, subject.crate)
                    outcomes.append(Outcome(f"{target.name}/{name}/step {k} cost", verdict, detail, numbers))
                if self.trace and stepped.verdict != "error":
                    traces.mkdir(parents=True, exist_ok=True)
                    now = traces / f"{k}.cbor"
                    try:
                        subject.trace(now)
                    except (engine.Failed, engine.Unmeasurable) as e:
                        outcomes.append(Outcome(f"{target.name}/{name}/step {k} trace", "error", str(e)))
                    else:
                        if edit is not None and (traces / f"{k - 1}.cbor").is_file():
                            verdict, detail = dbsp_case(self.proof, edit.kind, touched(was, model),
                                                        traces / f"{k - 1}.cbor", now)
                            outcomes.append(Outcome(f"{target.name}/{name}/step {k} trace", verdict, detail))
                if stepped.verdict != "pass":
                    break
        finally:
            bad = first_bad(steps)
            stopped = subject.close((self.keep or bad is not None) if keep is None else keep)
            if not self.keep:
                shutil.rmtree(traces, ignore_errors=True)
        if stopped is not None:
            outcomes.append(Outcome(f"{target.name}/{name}: stopping the daemon", "error", stopped))
        return Ran(tuple(outcomes), tuple(steps), first_bad(steps), stopped)

    def still(self, target: Target, wanted: tuple[str, ...]) -> Callable[[list[str]], list[bool]]:
        """shrink's test: each script text, run whole on a fresh daemon, first does not pass at a
        step of the same signature."""
        def one(text: str) -> bool:
            edits = parse_script(text)
            if isinstance(edits, str) or not edits:
                return False
            ran = self.run(target, "shrink", edits, keep=False)
            if ran.bad is None or ran.bad == 0:
                return False
            return signature(ran.steps[ran.bad]) == wanted

        def holds(texts: list[str]) -> list[bool]:
            with concurrent.futures.ThreadPoolExecutor(self.jobs) as pool:
                return list(pool.map(one, texts))

        return holds


def saved_scripts(root: Path) -> tuple[list[tuple[str, list[Edit]]], list[tuple[str, str]]]:
    """Each script of `root` with its edits, and each that does not parse, with why."""
    found: list[tuple[str, list[Edit]]] = []
    wrong: list[tuple[str, str]] = []
    for path in sorted(root.glob(f"*{SUFFIX}")) if root.is_dir() else []:
        got = parse_script(path.read_text(errors="replace"))
        if isinstance(got, str):
            wrong.append((path.name, got))
        elif not got:
            wrong.append((path.name, "it has no edit"))
        else:
            found.append((path.stem, got))
    return found, wrong


def main(args: Input) -> None:
    for name, value, least in (("jobs", args.jobs, 1), ("scripts", args.scripts, 0), ("steps", args.steps, 1),
                               ("rounds", args.rounds, 0)):
        if value < least:
            fail(SCRIPT, f"{name}: at least {least}, not {value}")
    if not args.projects:
        fail(SCRIPT, "projects: name at least one")
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-dbsp-edits-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    into = Path(args.into).resolve() if args.into else SCRIPTS
    print(f"work: {work}", flush=True)
    rep = report.Report(SCRIPT, args.engine, args.out)
    try:
        runner = Runner(eng, work, args.jobs, args.trace, args.keep)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    saved, wrong = saved_scripts(SCRIPTS) if args.saved else ([], [])
    for name, why in wrong:
        rep.add(f"scripts/{name}", "error", f"does not parse: {why}")
    targets: list[Target] = []
    for project in args.projects:
        try:
            if project.kind == "cases":
                found, bad = case_targets(project)
                targets += found
                for name, why in bad:
                    rep.add(name, "error", f"case wrong: {why}")
            else:
                targets.append(workspace_target(project, work))
        except engine.Failed as e:
            rep.add(project.name or project.path or project.kind, "error", str(e))
    unsaid: Intent = ""
    plans: list[Plan] = [Plan(t, name, tuple((e, unsaid) for e in edits)) for t in targets for name, edits in saved]
    plans += [Plan(t, f"s{args.seed}-{i}", tuple(generate(args.seed, i, args.steps)), args.seed, i)
              for t in targets for i in range(args.scripts)]
    if not plans:
        fail(SCRIPT, "no script to run: no project could be made ready, or no script")
    def ran_plan(plan: Plan) -> Ran:
        return runner.run(plan.target, plan.name, [e for e, _ in plan.edits])

    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        results = list(pool.map(ran_plan, plans))
    tally: Counter[str] = Counter()
    for plan, ran in zip(plans, results):
        for o in ran.outcomes:
            rep.add(o.name, o.verdict, o.detail, **o.numbers)
        for s in ran.steps:
            if s.scratch is not None:
                tally["rejected" if s.scratch.said.keys else "accepted"] += 1
            tally[s.category] += 1
        if ran.bad is None or ran.bad == 0 or not args.shrink:
            continue
        bad = ran.steps[ran.bad]
        wanted = signature(bad)
        if wanted is None:
            continue
        edits = plan.edits[:ran.bad]
        text = script_text(edits)
        small, used = shrink(text, runner.still(plan.target, wanted), args.rounds)
        if not runner.still(plan.target, wanted)([small])[0]:
            small = text
        note = (f"edits.py: {plan.target.name}, script {plan.name}, step {ran.bad} ({edits[-1][0]}), shrunk in {used} "
                f"batches; {bad.category}: {bad.detail.splitlines()[0][:160]}")
        got = parse_script(small)
        shrunk: list[tuple[Edit, Intent]] = [(e, unsaid) for e in got] if not isinstance(got, str) else list(edits)
        into.mkdir(parents=True, exist_ok=True)
        written = into / f"{slug(plan.target.name)}-{slug(plan.name)}{SUFFIX}"
        written.write_text(script_text(shrunk, [note]))
        rep.add(f"{plan.target.name}/{plan.name}: shrunk", "fail",
                f"{note}\nwritten as {written} ({len(shrunk)} edits, from {len(edits)})", edits=len(shrunk),
                from_edits=len(edits), batches=used)
    rep.extra.update(work=str(work), toolchain=engine.TOOLCHAIN, seed=args.seed, targets=len(targets),
                     scripts=len(plans), steps=dict(tally))
    if not args.keep:
        for name in ("store", "h", "ss", "std-store"):
            shutil.rmtree(work / name, ignore_errors=True)
    rep.finish()


# --- selftest

def balanced(text: str) -> bool:
    """Whether the brackets of rendered scaffold text balance (it has no strings; `//` lines are
    skipped)."""
    pairs = {")": "(", "]": "[", "}": "{"}
    stack: list[str] = []
    for line in text.splitlines():
        if line.lstrip().startswith("//"):
            continue
        for c in line:
            if c in "([{":
                stack.append(c)
            elif c in pairs:
                if not stack or stack.pop() != pairs[c]:
                    return False
    return not stack


def test_deterministic() -> None:
    for i in range(20):
        assert generate(5, i, 12) == generate(5, i, 12), i
    assert sum(generate(5, i, 12) != generate(6, i, 12) for i in range(20)) >= 18, "two seeds give mostly the same scripts"
    for i in range(20):
        edits = generate(5, i, 12)
        assert len(edits) >= 12, (i, len(edits))
        assert any(e.op == "add_impl" and intent == "keep" for e, intent in edits), f"script {i} adds no impl"
        assert parse_script(script_text(edits)) == [e for e, _ in edits], f"script {i} does not read back"


def test_coverage() -> None:
    """A few seeds' scripts use every op, keep and break both, and every edit kind."""
    ops: Counter[str] = Counter()
    intents: Counter[str] = Counter()
    for seed in range(4):
        for i in range(25):
            for e, intent in generate(seed, i, 16):
                ops[e.op] += 1
                intents[intent] += 1
    assert set(ops) == set(OPS), sorted(set(OPS) - set(ops))
    assert intents["keep"] > intents["break"] > 0, intents
    assert 0.1 < intents["break"] / sum(intents.values()) < 0.4, intents
    assert {KIND_OF.get(op, "item") for op in ops} == set(KINDS)


def test_render() -> None:
    """Every prefix of a script renders to a block whose brackets balance, with no two items of
    one name in one module but where the script breaks it on purpose, and a file for each
    out-of-line module."""
    for seed in range(3):
        for i in range(15):
            model = Model()
            for e, _ in generate(seed, i, 14):
                apply(model, e)
                block, files = render(model)
                assert block.startswith(BEGIN + "\n") and block.endswith(END + "\n"), block
                assert balanced(block), block
                for body in files.values():
                    assert balanced(body), body
                assert set(files) == {f"{m}.rs" for m, k in model.modules if k == "file"}, (files, model.modules)
                assert "@" not in block and "None" not in block, block
    assert not balanced("fn f() {") and not balanced("fn f() )(") and balanced("// {\nfn f() {}")
    model = Model()
    for e in parse_script("\n".join([HEADER, "add_mod(dbsp_m1, inline);", "add_trait(top, DbspT1);",
                                     "add_method(DbspT1, dbsp_f1);", "add_struct(dbsp_m1, DbspS1);",
                                     "add_use(dbsp_m1, DbspU1, DbspT1);", "add_impl(dbsp_m1, DbspU1, DbspS1);",
                                     "add_alias(top, DbspA1, DbspS1);", "add_glob(top, dbsp_m1);",
                                     "add_probe(dbsp_m1, dbsp_p1, DbspS1, dbsp_f1);", "add_derive(DbspS1, Clone);",
                                     "add_mod(dbsp_m2, file);", "add_macro(dbsp_m2, dbsp_mac1, DbspS2);"])):
        apply(model, cast(Edit, e))
    block, files = render(model)
    for want in ("pub(crate) trait DbspT1 {\n    fn dbsp_f1(&self) -> u32 {\n        1\n    }\n}",
                 "    pub(crate) use super::DbspT1 as DbspU1;", "    impl self::DbspU1 for self::DbspS1 {}",
                 "pub(crate) type DbspA1 = self::dbsp_m1::DbspS1;", "pub(crate) use self::dbsp_m1::*;",
                 "        let _ = self::DbspS1.dbsp_f1();", "    #[derive(Clone)]\n    #[allow(unused)]\n    pub(crate) struct DbspS1;",
                 "#[allow(unused)]\npub(crate) mod dbsp_m2;"):
        assert want in block, (want, block)
    assert "macro_rules! dbsp_mac1 {" in files["dbsp_m2.rs"] and "dbsp_mac1!();" in files["dbsp_m2.rs"], files
    assert model.where("DbspS2") == "dbsp_m2" and model.trait_of("DbspU1") == "DbspT1"
    assert model.type_of("DbspA1") == "DbspS1"
    text, _ = edited("fn main() {}\n\n\n", model)
    assert text.startswith("fn main() {}\n\n" + BEGIN + "\n"), text


def test_apply() -> None:
    model = Model()
    for line in ("add_trait(top, T1);", "add_trait(top, T2);", "add_struct(top, S1);", "add_use(top, U1, T1);",
                 "add_impl(top, U1, S1);", "repoint_use(U1, T2);", "rename(S1, S9);", "rename_def(T2, T7);",
                 "add_struct(nowhere, S5);", "remove_impl(top, U1, S2);", "add_method(T1, f1);", "add_method(T1, f1);",
                 "remove_method(T1, f1);", "add_derive(S9, Copy);", "add_derive(S9, Nope);"):
        edits = parse_script(f"{HEADER}\n{line}")
        assert isinstance(edits, list), (line, edits)
        apply(model, edits[0])
    impl = model.of("impl")[0]
    assert (impl.trait, impl.to) == ("U1", "S9"), impl
    assert model.first("use", "U1") is not None and cast(Item, model.first("use", "U1")).to == "T2"
    assert model.trait_of("U1") is None, "the use points at T2, whose definition is now T7"
    assert model.first("trait", "T7") is not None and model.first("struct", "S5") is None
    assert cast(Item, model.first("trait", "T1")).methods == ["f1"], "a method added twice, removed once"
    assert cast(Item, model.first("struct", "S9")).derives == ["Copy"]
    for bad, why in (("add_impl(top, T1);", "takes 3"), ("nope(a);", "no op"), ("add_struct(top, 1x);", "not a name"),
                     ("add_struct(top S1);", "takes 2"), ("add_struct(top, S1)", "is not")):
        got = parse_script(f"{HEADER}\n{bad}")
        assert isinstance(got, str) and why in got, (bad, got)
    assert isinstance(parse_script("add_struct(top, S1);\n"), str), "a script without its header"
    assert parse_script(f"{HEADER}\n// a note\n\nadd_struct(top, S1); // keep\n") == [Edit("add_struct", ("top", "S1"))]
    assert module_dir("src/lib.rs", True) == "src" and module_dir("src/a/mod.rs", False) == "src/a"
    assert module_dir("crates/printer/src/standard.rs", False) == "crates/printer/src/standard"


def test_touched() -> None:
    """The traits and buckets an edit changes, as the trace case is given them."""
    model = Model()

    def after(line: str) -> tuple[int, int]:
        was = copy.deepcopy(model)
        edits = parse_script(f"{HEADER}\n{line}")
        assert isinstance(edits, list), (line, edits)
        apply(model, edits[0])
        return touched(was, model)

    for line in ("add_trait(top, T1);", "add_struct(top, S1);", "add_struct(top, S2);", "add_alias(top, A1, S1);"):
        assert after(line) == (0, 0), line
    assert after("add_impl(top, T1, A1);") == (1, 1)
    assert after("retarget_alias(A1, S2);") == (1, 2), "the bucket of the old type and of the new"
    assert after("add_method(T1, f1);") == (0, 0)
    assert after("add_derive(S1, PartialEq);") == (2, 2), "PartialEq with StructuralPartialEq"
    assert after("add_impl(top, T9, S1);") == (0, 0), "an impl of no trait is in no slice"
    assert after("rename(S2, S7);") == (1, 2)
    assert after("add_mod(m1, inline);") == (0, 0)
    assert after("add_use(m1, U1, T1);") == (0, 0)
    assert after("add_impl(m1, U1, S1);") == (1, 1)
    assert after("remove_mod(m1);") == (1, 1)


def planted(texts: list[str]) -> list[bool]:
    """A mismatch planted in the model: two impls of one trait for one type, both defined."""
    out: list[bool] = []
    for text in texts:
        edits = parse_script(text)
        if isinstance(edits, str):
            out.append(False)
            continue
        model = Model()
        for e in edits:
            apply(model, e)
        keys = Counter((model.trait_of(i.trait), model.type_of(i.to)) for i in model.of("impl"))
        out.append(any(n > 1 and None not in key for key, n in keys.items()))
    return out


def test_shrink() -> None:
    """shrink reduces a script with a planted mismatch to the edits that make it."""
    edits = [Edit(op, args) for op, args in (
        ("add_mod", ("dbsp_m1", "inline")), ("add_trait", ("top", "DbspT1")), ("add_struct", ("dbsp_m1", "DbspS3")),
        ("add_struct", ("top", "DbspS1")), ("add_method", ("DbspT1", "dbsp_f1")), ("add_impl", ("top", "DbspT1", "DbspS1")),
        ("add_glob", ("top", "dbsp_m1")), ("add_alias", ("dbsp_m1", "DbspA1", "DbspS1")), ("add_derive", ("DbspS3", "Clone")),
        ("add_probe", ("top", "dbsp_p1", "DbspS1", "dbsp_f1")), ("add_use", ("dbsp_m1", "DbspU1", "DbspT1")),
        ("add_impl", ("dbsp_m1", "DbspU1", "DbspA1")), ("add_trait", ("dbsp_m1", "DbspT2")))]
    text = script_text([(e, "keep") for e in edits])
    assert planted([text]) == [True]
    small, used = shrink(text, planted, 60)
    got = parse_script(small)
    assert isinstance(got, list) and planted([small]) == [True], small
    assert len(got) <= 7 and {e.op for e in got} <= {"add_mod", "add_trait", "add_struct", "add_impl", "add_use",
                                                     "add_alias"}, got
    assert sum(e.op == "add_impl" for e in got) == 2 and small.startswith(HEADER + "\n"), small
    assert 0 < used <= 60
    assert shrink(text, lambda ts: [False] * len(ts), 10)[0] == text


def test_compare() -> None:
    """Two builds of a step compared, the mismatch's signature, and the targets a build's JSON names."""
    said = cases.said_alone
    e0119 = json.dumps({"reason": "compiler-message", "target": {"name": "u3_0"},
                        "message": {"level": "error", "message": "conflicting implementations", "code": {"code": "E0119"}}})
    rejected, accepted = Built(said(101, e0119, "", None)), Built(said(0, "", "", None))
    assert compare(rejected, rejected)[:2] == ("pass", "same")
    assert compare(accepted, accepted)[:2] == ("pass", "same")
    verdict, category, _ = compare(accepted, rejected)
    assert (verdict, category) == ("fail", "accepted what the build from nothing rejects")
    stepped = Stepped(verdict, category, "", accepted, rejected)
    assert signature(stepped) == ("accepted what the build from nothing rejects", "E0119"), signature(stepped)
    assert compare(rejected, accepted)[1] == "rejected what the build from nothing accepts"
    e0425 = e0119.replace("E0119", "E0425")
    other = compare(rejected, Built(said(101, e0425, "", None)))
    assert other[:2] == ("fail", "other codes"), other
    ran, panicked = cases.Run(0, "1\n"), cases.Run(101, "", ("main at src/main.rs:3:5",))
    runs = compare(Built(accepted.said, ran), Built(accepted.said, panicked))
    assert runs[:2] == ("fail", "other run") and runs[2].startswith("status"), runs
    assert compare(Built(accepted.said, ran), Built(accepted.said, ran))[:2] == ("pass", "same")
    assert compare(Built(cases.Said(error="timed out")), accepted)[0] == "error"
    assert compare(Built(cases.Said(who="cargo")), accepted)[1] == "not blaze's"
    stdout = "\n".join([json.dumps({"reason": "compiler-artifact", "target": {"name": "oracle_base",
                                                                              "src_path": "/r/base/src/lib.rs"}}),
                        json.dumps({"reason": "compiler-artifact", "target": {"name": "oracle_app",
                                                                              "src_path": "/r/app/src/main.rs"},
                                    "executable": "/r/target/debug/oracle_app"})])
    assert crate_of(stdout, Path("/r"), "base/src/lib.rs") == "oracle_base"
    assert crate_of(stdout, Path("/r"), "base/src/probe/deep.rs") == "oracle_base"
    assert crate_of(stdout, Path("/r"), "elsewhere/x.rs") is None
    assert executable_of(stdout, "") == Path("/r/target/debug/oracle_app")
    assert executable_of(stdout, "other") is None


def line(crate: str, maintainer: str, write: int = 0, **work: int) -> str:
    counts = {f: work.get(f, 0) for f in WORK_FIELDS}
    return json.dumps({"reason": MAINTAINED, "write": write, "maintainer": maintainer, "crate": crate, "work": counts})


def test_cost() -> None:
    good = "\n".join([line("u0_0", "nameres", ops=1, slices=0), line("u0_0", "impls", ops=1, slices=2, keys=1),
                      line("dep", "nameres"), line("dep", "impls"), '{"reason": "blaze-work"}', "text"])
    assert cost(good, "u0_0")[0] == "pass", cost(good, "u0_0")
    missing = cost('{"reason": "blaze-work"}\n', "u0_0")
    assert missing[0] == "fail" and "00819" in missing[1], missing
    assert cost(good, "other")[0] == "fail"
    two = good + "\n" + line("u0_0", "impls", write=1, ops=1)
    assert cost(two, "u0_0")[0] == "fail", "ops are summed over a request's writes"
    many = good.replace('"slices": 2', '"slices": 3')
    assert cost(many, "u0_0")[0] == "fail" and "slices 3" in cost(many, "u0_0")[1]
    spill = good + "\n" + line("dep", "nameres", woken=4)
    assert cost(spill, "u0_0")[0] == "fail" and "dep (nameres): woken 4" in cost(spill, "u0_0")[1]
    odd = good + "\n" + json.dumps({"reason": MAINTAINED, "crate": "u0_0", "maintainer": "impls", "work": {"ops": -1}})
    assert cost(odd, "u0_0")[0] == "fail" and "another shape" in cost(odd, "u0_0")[1]
    assert cost(good, "")[0] == "error"
    assert cost(good, "u0_0")[2] == {"impls_ops": 1, "impls_slices": 2, "nameres_ops": 1, "nameres_slices": 0}


def test_saved() -> None:
    """Every script of scripts/ parses, and together they use every op."""
    found, wrong = saved_scripts(SCRIPTS)
    assert not wrong, wrong
    assert {"cost", "every-edit"} <= {name for name, _ in found}, [name for name, _ in found]
    assert {e.op for _, edits in found for e in edits} == set(OPS)


def test_kinds() -> None:
    """Each edit kind is a case of blaze-v2-proof's `dbsp` module, and each case an edit kind."""
    text = (V2.parent.parent / "crates" / "blaze-v2-proof" / "src" / "dbsp.rs").read_text()
    named = set(re.findall(r'\bedit: "([a-z]+)"', text))
    assert named == set(KINDS), (sorted(named), KINDS)


def test_cases() -> None:
    """Each case of a directory is read as a project; a file that is no case is named with why."""
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "a.rs").write_text("//@ expect: reject E0425\n//@ edition: 2021\nfn main() { x; }\n")
        (root / "b.rs").write_text("//@ expect: accept\n//@ edition: 2018\n#![crate_type = \"lib\"]\npub struct S;\n")
        (root / "c.rs").write_text("//@ expect: accept\n//@ revisions: x y\nfn main() {}\n")
        found, wrong = case_targets(Project("cases", path=tmp))
        assert [t.name for t in found] == [f"{root.name}/a.rs", f"{root.name}/b.rs"], found
        assert [n for n, _ in wrong] == [f"{root.name}/c.rs"], wrong
        a, b = (cast(cases.Case, t.case) for t in found)
        assert a.edition == "2021" and a.kind == "bin", a
        assert b.edition == "2018" and b.kind == "lib", b
        none, missing = case_targets(Project("cases", path=str(root / "nothing")))
        assert not none and missing, missing


SELFTESTS = (test_deterministic, test_coverage, test_render, test_apply, test_touched, test_shrink, test_compare, test_cost,
             test_saved, test_kinds, test_cases)


def selftest() -> int:
    failed = 0
    for test in SELFTESTS:
        try:
            test()
            print(f"ok    {test.__name__}", flush=True)
        except (Exception, SystemExit) as e:
            failed += 1
            print(f"FAIL  {test.__name__}: {type(e).__name__}: {e}", flush=True)
    print(f"{SCRIPT} selftest: {len(SELFTESTS) - failed} of {len(SELFTESTS)} pass", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    if sys.argv[1:] == ["selftest"]:
        sys.exit(selftest())
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    main(parse(Input, sys.argv[1], SCRIPT))
