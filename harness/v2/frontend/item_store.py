"""item_store.py: the item store against fresh runs and against blaze's build from nothing, over
random edit sequences of real programs (docs/v2/plan/design/item-hash.md section 9, "Differential,
random edits").

    uv run --frozen python harness/v2/frontend/item_store.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/item-store.json"}'
    uv run --frozen python harness/v2/frontend/item_store.py '{"engine": {...}, "out": "...", "apps": ["ripgrep"], "seed": 3, "steps": 40}'
    uv run --frozen python harness/v2/frontend/item_store.py selftest

The input is one JSON object (inline, a file's path, or `-` for stdin):

    engine      the blaze under test (engine.EngineSpec); v2 only, as v1 has no item store
    out         the path the result JSON is written to (report.py)
    work        the directory the daemon's home and store go in; empty: a new one under /tmp
    apps        which entries of harness/apps/apps.json run, by name; empty: every one with `edits`
    seed        the seed (0): step `i` of app `a` is chosen by `random.Random("item-store:<seed>:<a>")`
                drawn in order, from what the files hold at that step, so a seed and a checkout
                give the same sequence on every machine
    steps       how many steps after the first build (20)
    synthetic   also make the edits of this script, not only the app's (true)
    check       what is run after each step: "edit" (the step's app edit's `check`, else the app's
                `edit_check`, else the program with no arguments, as appsedits.py runs) or "runs"
                (every command line of the app's `runs`, as appruns.py's workload)
    keep_going  go on with an app after a step fails (false: its later steps would start from
                a store the failure left)

Each app is checked out, its dependencies put beside it and its setup run as appruns.py does
(`checkout`, `prepare`, `localize`, `setup`). blaze builds it with `BLAZE_ITEM_STORE=verify`:
every body step the item store holds is run fresh as well and a difference stops the build with
an error naming the body. blaze also builds the same source from nothing, with
`BLAZE_ITEM_STORE=off`: a home and a store of its own, made for the step and removed after it
(as budgets.py's `wrong`), into the same target path, the other build's waiting aside (as
oracle.py's `bytes`), then moved to `target-scratch`. Then each step changes the source, one of:

    apply       one of the app's `edits` (pick_edits.py's file, old, new) not yet made
    revert      one made, undone
    statement   a `let` added after another in a function's body
    impl-add    a struct, an inherent impl and a `Display` impl of it added to the crate root
    impl-remove one of those taken out again
    move        a function at the top of a file moved to the file's end
    rename-local  a `let` binding of such a function renamed, with every use after it
    rename-fn   a private function at the top of a file renamed, with every call and path of it
                in the app's sources
    earlier     every file back to what it held after an earlier step

A rename is by words, only where every use of the name is one a rename by words gets right (no
string, char, comment or format string on its line, no field, method, macro, module or `use` of
the name, no app edit's text holding it); a renamed name ends in `_item_store_<step>`. Both sides
build again. One outcome per step, `<app>: step <i>: <what>`:

    pass   both builds succeed and the check prints the same from both programs (appruns.Program's
           behaviour: status, stdout and stderr, scrubbed as the app's `scrub` says), or both
           builds fail (the edit is one Rust rejects, as a synthetic one may be)
    fail   the build with the item store fails and the one from nothing does not (an item store
           mismatch is one: the detail has blaze's error), or the other way round, or the
           outputs differ; or, after a move or a rename of a tree the step before built, the
           build with the item store's program is not the same bytes as the one built from
           nothing (names and places are the presentation layer's, docs/v2/plan/design/item-hash.md
           section 3.7), or it reran an inference, MIR or borrow check step (a body's identity is
           anonymous, section 3.3, so every lookup hits). A `rename-fn` of a function something
           calls is held to the bytes alone: its callers' identities name it by key (a fact of it
           is keyed so) and rerun, and the note counts a whole build, not a body
    error  the harness could not say (a command timed out, the daemon did not answer)

Each outcome's numbers are what the build said: `reran`, `green`, `compiled`, `found` from its
`blaze-work` line, the item store's `written`, `kept`, `slots_full` and `refused` (the steps
kept out by reads neither covered nor a fact, summed) from its `-v` note (blaze-cargo's
`deps::noted`) of what the flush of the build before did, and `hits` and `ran` of this build's
lookups, summed over the steps, None where it printed none. `extra.steps` has every step's
reads that kept steps out, by name, and its lookups by step. Files are put back as the
checkout had them when an app ends.

`selftest` checks the parts that need neither blaze, cargo nor the network: the input, the
edits made here (renames among them), the reading of the note, the verdict.
"""

import json
import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, cast

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent), str(HERE.parent.parent)]
from typed_input import fail, parse  # noqa: E402
import engine, report  # noqa: E402

SCRIPT = "item_store.py"
type Check = Literal["edit", "runs"]
type Action = Literal["first", "apply", "revert", "statement", "impl-add", "impl-remove", "move", "rename-local",
                      "rename-fn", "earlier"]
# The edits after which nothing but names and places changed: the front end reruns no step and
# the program is the same bytes as the one built from nothing.
ANONYMOUS: tuple[Action, ...] = ("move", "rename-local", "rename-fn")
ITEM_STORE = "BLAZE_ITEM_STORE"
PROBE = "ItemStoreProbe"
RENAMED = "_item_store_"
# The `-v` note of blaze-cargo's `deps::noted`.
NOTE = re.compile(r"the item store: (\d+) written, (\d+) kept, (\d+) with every slot full")
KEPT_OUT = re.compile(r"steps kept out by reads of (.*?)(?:; \d+ |; the item store, this build|$)")
KEPT_OUT_READ = re.compile(r"`([^`]+)` \((\d+)\)")
STEP_NAMES = ("inference", "MIR", "borrow check")
STEPS = re.compile(r"the item store, this build: " + "; ".join(
    f"{name} (\\d+) hit, (\\d+) ran, (\\d+) refused" for name in STEP_NAMES))
STATEMENT = re.compile(r"^( {8,})let [^\n]*;\n$")
TOP_FN = re.compile(r"^(?:pub(?:\([^)]*\))? )?(?:(?:const|async|unsafe|extern \"[^\"]*\") )*fn \w+")
LET_NAME = re.compile(r"^\s+let (?:mut )?([a-z_][a-z0-9_]*)\s*[:=;]")
PRIVATE_FN = re.compile(r"^(?:(?:const|async|unsafe) )*fn ([a-z_][a-z0-9_]*)\b")
# A line a rename leaves alone: a word in it may be text (a string, a char, a comment, a
# format string's capture), which a rename by words would get wrong.
TEXTUAL = re.compile(r"[\"'#]|//|/\*")


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    apps: tuple[str, ...] = ()
    seed: int = 0
    steps: int = 20
    synthetic: bool = True
    check: Check = "edit"
    keep_going: bool = False


@dataclass(frozen=True)
class Edit:
    """An edit of apps.json's `edits`: `file` under the checkout, `old` replaced by `new` once."""

    file: str
    old: str
    new: str
    what: str = ""
    check: tuple[str, ...] | None = None


@dataclass
class Tree:
    """The files a sequence changes, by path under the checkout, as they are now, and what the
    sequence has made of them: which app edits are made and which probes are added."""

    files: dict[str, str]
    made: set[int] = field(default_factory=set[int])
    probes: list[int] = field(default_factory=list[int])

    def copy(self) -> "Tree":
        return Tree(dict(self.files), set(self.made), list(self.probes))


@dataclass(frozen=True)
class Step:
    """What one step did to the tree: its action, said for the report, the app edit it made or
    reverted, whose `check` it runs, and for a rename or a move whether bodies other than the
    edited ones change identity and rerun (`rename-fn` of a function something calls: a body's
    identity has each item a path names by key)."""

    action: Action
    what: str
    edit: int | None = None
    reruns: bool = False


@dataclass(frozen=True)
class Tally:
    """What a build said it did; None where it did not say."""

    reran: int | None = None
    green: int | None = None
    compiled: int | None = None
    found: int | None = None
    written: int | None = None
    kept: int | None = None
    slots_full: int | None = None
    refused_by: tuple[tuple[str, int], ...] = ()
    # This build's item store lookups, per step of `STEP_NAMES`: (hits, ran, refused).
    steps: tuple[tuple[int, int, int], ...] | None = None

    def numbers(self) -> dict[str, report.Number]:
        refused = sum(n for _, n in self.refused_by) if self.written is not None else None
        hits = sum(h for h, _, _ in self.steps) if self.steps is not None else None
        ran = sum(r for _, r, _ in self.steps) if self.steps is not None else None
        return dict(reran=self.reran, green=self.green, compiled=self.compiled, found=self.found,
                    written=self.written, kept=self.kept, slots_full=self.slots_full, refused=refused,
                    hits=hits, ran=ran)


def edits_of(spec: dict[str, Any]) -> list[Edit]:
    """The app's `edits`, read whole: an entry of another shape is an error naming it."""
    found: list[Edit] = []
    for at, given in enumerate(spec.get("edits", [])):
        try:
            found.append(parse(Edit, json.dumps(given), f"{SCRIPT}: edits[{at}]"))
        except SystemExit as e:
            raise engine.Failed(str(e)) from None
    return found


# The edits this script makes.

def statement_sites(text: str) -> list[int]:
    """The lines of `text` a statement can follow: a whole `let` of one line in a function's
    body, its brackets balanced, outside a comment."""
    lines = text.splitlines(keepends=True)
    return [at for at, line in enumerate(lines)
            if STATEMENT.match(line) and all(line.count(a) == line.count(b) for a, b in ("()", "[]", "{}"))]


def add_statement(text: str, site: int, n: int) -> str:
    lines = text.splitlines(keepends=True)
    indent = cast(re.Match[str], STATEMENT.match(lines[site]))[1]
    lines.insert(site + 1, f"{indent}let _item_store_{n} = {n}u32;\n")
    return "".join(lines)


def probe(n: int) -> str:
    """The items `impl-add` puts at the end of the crate root: a new type, an inherent impl and a
    trait impl, which change the crate's impl tables and nothing a body of the program reads."""
    return (f"\n#[allow(dead_code)]\nstruct {PROBE}{n};\n\n#[allow(dead_code)]\nimpl {PROBE}{n} {{\n"
            f"    fn probe(&self) -> u32 {{\n        {n}\n    }}\n}}\n\n"
            f"impl ::core::fmt::Display for {PROBE}{n} {{\n"
            f"    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {{\n"
            f"        f.write_str(\"{n}\")\n    }}\n}}\n")


def functions(text: str) -> list[tuple[int, int]]:
    """Each function at the top of `text` that can be moved, as (first line, line after it): its
    attributes and doc comments with it, ending at the first `}` at the start of a line, with
    nothing between at the start of a line. One that already ends the file is not listed."""
    lines = text.splitlines(keepends=True)
    found: list[tuple[int, int]] = []
    for at, line in enumerate(lines):
        if not TOP_FN.match(line) or line.rstrip().endswith(";"):
            continue
        end = next((i for i in range(at, len(lines)) if lines[i] == "}\n"), None)
        if end is None or any(l[:1] not in ("", " ", "\t", "\n") for l in lines[at + 1:end]):
            continue
        first = at
        while first > 0 and lines[first - 1].startswith(("#[", "///")):
            first -= 1
        if all(not l.strip() for l in lines[end + 1:]):
            continue
        found.append((first, end + 1))
    return found


def move_function(text: str, span: tuple[int, int]) -> str:
    lines = text.splitlines(keepends=True)
    first, after = span
    block = lines[first:after]
    tail = "".join(lines[:first] + lines[after:])
    return tail + ("" if tail.endswith("\n") else "\n") + "\n" + "".join(block)


def word(name: str) -> re.Pattern[str]:
    return re.compile(rf"(?<![\w$']){re.escape(name)}(?!\w)")


def renamable(lines: list[str], name: str, decl: int | None) -> bool:
    """Whether every use of `name` in `lines` is a name a rename by words renames right: none in
    a line that may hold text (`TEXTUAL`), after `.` or `::` (a field, a method, another path's
    segment), before `!`, `::` or a lone `:` (a macro, a module, a field of a literal; `decl`'s
    line, the binding's own, may have its type), or alone between `{`/`,` and `}`/`,` (a field
    shorthand)."""
    pattern = word(name)
    for at, line in enumerate(lines):
        for use in pattern.finditer(line):
            before, after = line[:use.start()].rstrip(), line[use.end():].lstrip()
            if TEXTUAL.search(line) or before.endswith((".", "::")) or after.startswith(("!", "::")):
                return False
            if after.startswith(":") and at != decl:
                return False
            if before.endswith(("{", ",")) and after.startswith(("}", ",")):
                return False
    return True


def local_renames(text: str) -> list[tuple[int, int, str]]:
    """Each `let` binding of a function `functions` lists that can be renamed by words from its
    line to the function's end, as (its line, the line after the function, its name): its name
    is in no line of the function before it, and every use is one `renamable` takes."""
    lines = text.splitlines(keepends=True)
    found: list[tuple[int, int, str]] = []
    for first, after in functions(text):
        for at in range(first, after):
            m = LET_NAME.match(lines[at])
            if m is None or m[1] == "_" or RENAMED in m[1]:
                continue
            name = m[1]
            # Once on its own line: the name in its initializer is another thing's.
            if any(word(name).search(l) for l in lines[first:at]) or len(word(name).findall(lines[at])) != 1:
                continue
            if renamable(lines[at:after], name, 0):
                found.append((at, after, name))
    return found


def rename_local(text: str, site: tuple[int, int, str], n: int) -> str:
    at, after, name = site
    lines = text.splitlines(keepends=True)
    pattern = word(name)
    lines[at:after] = [pattern.sub(f"{name}{RENAMED}{n}", l) for l in lines[at:after]]
    return "".join(lines)


def fn_renames(files: dict[str, str], sources: list[str]) -> list[str]:
    """The names of the private functions at the top of `sources` that can be renamed by words in
    all of them: no attribute above (a test, an exported name), not `main`, no module or `use` of
    the name, and every use one `renamable` takes, so each is a call or a path of the crate's own."""
    names: set[str] = set()
    for s in sources:
        lines = files[s].splitlines(keepends=True)
        for at, line in enumerate(lines):
            m = PRIVATE_FN.match(line)
            if m and m[1] != "main" and RENAMED not in m[1] and not (at > 0 and lines[at - 1].startswith("#[")):
                names.add(m[1])
    found: list[str] = []
    for name in sorted(names):
        pattern = word(name)
        ok = True
        for s in sources:
            lines = files[s].splitlines(keepends=True)
            used = [l for l in lines if pattern.search(l)]
            if any(l.lstrip().startswith(("use ", "pub use ", "mod ", "pub mod ")) for l in used):
                ok = False
            # A `fn` of the name in an impl or a trait is a method, called with `.`.
            if any(re.match(rf"\s+(?:pub(?:\([^)]*\))? )?(?:(?:const|async|unsafe) )*fn {re.escape(name)}\b", l)
                   for l in used):
                ok = False
            if not ok or not renamable(used, name, None):
                ok = False
                break
        if ok:
            found.append(name)
    return found


def called_elsewhere(files: dict[str, str], sources: list[str], name: str) -> bool:
    """Whether `name` is written anywhere in `sources` but its own `fn` line: a call or a path
    of it (a recursive call too), in a body whose identity then names another item."""
    pattern = word(name)
    return sum(len(pattern.findall(files[s])) for s in sources) > 1


def rename_fn(files: dict[str, str], sources: list[str], name: str, n: int) -> None:
    pattern = word(name)
    for s in sources:
        files[s] = pattern.sub(f"{name}{RENAMED}{n}", files[s])


def sources_of(ws: Path, dirs: list[Path]) -> list[str]:
    """The Rust files under each of `dirs`, by path under `ws`, leaving out build outputs and
    hidden directories."""
    found: set[str] = set()
    for d in dirs:
        for path in d.rglob("*.rs"):
            under = path.relative_to(ws)
            if not any(part.startswith(("target", ".")) for part in under.parts[:-1]):
                found.add(str(under))
    return sorted(found)


def leftovers(text: str) -> bool:
    """Whether `text` holds an edit of this script, left by a run that did not put it back."""
    return PROBE in text or RENAMED in text


# The sequence.

def untouched(name: str, edits: list[Edit]) -> bool:
    """Whether renaming `name` leaves every app edit's `old` and `new` as they are, so each can
    still be made and undone."""
    pattern = word(name)
    return not any(pattern.search(e.old) or pattern.search(e.new) for e in edits)


def local_sites(tree: Tree, edits: list[Edit], sources: list[str]) -> list[tuple[str, tuple[int, int, str]]]:
    return [(s, site) for s in sources for site in local_renames(tree.files[s]) if untouched(site[2], edits)]


def fn_sites(tree: Tree, edits: list[Edit], sources: list[str]) -> list[str]:
    return [name for name in fn_renames(tree.files, sources) if untouched(name, edits)]


def choices(tree: Tree, edits: list[Edit], sources: list[str], root: str | None, synthetic: bool,
            earlier: int) -> list[Action]:
    """The actions a step can take from `tree`."""
    out: list[Action] = []
    if any(i not in tree.made and e.old in tree.files[e.file] for i, e in enumerate(edits)):
        out.append("apply")
    if any(i in tree.made and edits[i].new in tree.files[edits[i].file] for i in tree.made):
        out.append("revert")
    if synthetic:
        if any(statement_sites(tree.files[s]) for s in sources):
            out.append("statement")
        if root is not None:
            out.append("impl-add")
        if root is not None and any(probe(k) in tree.files[root] for k in tree.probes):
            out.append("impl-remove")
        if any(functions(tree.files[s]) for s in sources):
            out.append("move")
        if local_sites(tree, edits, sources):
            out.append("rename-local")
        if fn_sites(tree, edits, sources):
            out.append("rename-fn")
    if earlier > 0:
        out.append("earlier")
    return out


def step(rng: random.Random, tree: Tree, history: list[Tree], edits: list[Edit], sources: list[str],
         root: str | None, synthetic: bool, n: int) -> Step:
    """One step from `tree`, made in it; `history` is the tree after each step before. `root` is
    the program's crate root, which `impl-add` adds to."""
    can = choices(tree, edits, sources, root, synthetic, len(history) - 1)
    if not can:
        raise engine.Failed("no step can be made: no edit applies, and nothing to revert or go back to")
    action = rng.choice(can)
    if action == "apply":
        i = rng.choice([i for i, e in enumerate(edits) if i not in tree.made and e.old in tree.files[e.file]])
        e = edits[i]
        tree.files[e.file] = tree.files[e.file].replace(e.old, e.new, 1)
        tree.made.add(i)
        return Step(action, f"{e.file}: {e.what or e.old.strip()[:40]}", i)
    if action == "revert":
        i = rng.choice(sorted(i for i in tree.made if edits[i].new in tree.files[edits[i].file]))
        e = edits[i]
        tree.files[e.file] = tree.files[e.file].replace(e.new, e.old, 1)
        tree.made.discard(i)
        return Step(action, f"{e.file}: {e.what or e.old.strip()[:40]} undone", i)
    if action == "statement":
        path = rng.choice([s for s in sources if statement_sites(tree.files[s])])
        site = rng.choice(statement_sites(tree.files[path]))
        tree.files[path] = add_statement(tree.files[path], site, n)
        return Step(action, f"{path}:{site + 2}: a `let` added")
    if action == "impl-add":
        path = cast(str, root)
        tree.files[path] += probe(n)
        tree.probes.append(n)
        return Step(action, f"{path}: {PROBE}{n} and its impls added")
    if action == "impl-remove":
        path = cast(str, root)
        k = rng.choice([k for k in tree.probes if probe(k) in tree.files[path]])
        tree.files[path] = tree.files[path].replace(probe(k), "", 1)
        tree.probes.remove(k)
        return Step(action, f"{path}: {PROBE}{k} and its impls removed")
    if action == "move":
        path = rng.choice([s for s in sources if functions(tree.files[s])])
        span = rng.choice(functions(tree.files[path]))
        name = cast(re.Match[str], re.search(r"fn (\w+)", "".join(tree.files[path].splitlines(keepends=True)[span[0]:span[1]])))[1]
        tree.files[path] = move_function(tree.files[path], span)
        return Step(action, f"{path}: fn {name} moved to the end")
    if action == "rename-local":
        path, site = rng.choice(local_sites(tree, edits, sources))
        tree.files[path] = rename_local(tree.files[path], site, n)
        return Step(action, f"{path}:{site[0] + 1}: `{site[2]}` renamed to `{site[2]}{RENAMED}{n}`")
    if action == "rename-fn":
        name = rng.choice(fn_sites(tree, edits, sources))
        called = called_elsewhere(tree.files, sources, name)
        rename_fn(tree.files, sources, name, n)
        return Step(action, f"fn {name} renamed to {name}{RENAMED}{n} everywhere"
                    + (", its callers with it" if called else ", nothing calls it"), reruns=called)
    back = rng.randrange(len(history) - 1)
    then = history[back]
    tree.files, tree.made, tree.probes = dict(then.files), set(then.made), list(then.probes)
    return Step("earlier", f"every file as after step {back}")


# What a build said.

def texts_of(output: str) -> Iterator[str]:
    """Every line of a command's output, and of a JSON line every string in it (a diagnostic's
    `message` and `rendered` under `--message-format=json`)."""
    for line in output.splitlines():
        yield line
        if line.startswith("{"):
            try:
                value: object = json.loads(line)
            except json.JSONDecodeError:
                continue
            stack = [value]
            while stack:
                it = stack.pop()
                if isinstance(it, str):
                    yield it
                elif isinstance(it, dict):
                    stack.extend(cast(dict[str, object], it).values())
                elif isinstance(it, list):
                    stack.extend(cast(list[object], it))


def tally(ran: engine.Ran) -> Tally:
    """What `ran`, a `blaze build -v --message-format=json`, said it did."""
    work = engine.worked_lines(ran.stdout)
    counts: dict[str, int | None] = dict.fromkeys(("reran", "green", "compiled", "found"))
    if len(work) == 1:
        for name in counts:
            value = work[0].get(name)
            counts[name] = value if isinstance(value, int) and not isinstance(value, bool) else None
    written = kept = full = None
    by: dict[str, int] = {}
    steps: tuple[tuple[int, int, int], ...] | None = None
    for text in texts_of(ran.stdout + "\n" + ran.stderr):
        if (counted := STEPS.search(text)) is not None:
            values = [int(x) for x in counted.groups()]
            steps = tuple((values[at], values[at + 1], values[at + 2]) for at in range(0, len(values), 3))
        if (note := NOTE.search(text)) is None:
            continue
        written, kept, full = (int(x) for x in note.groups())
        by = {}
        if (out := KEPT_OUT.search(text, note.end())) is not None:
            by = {name: int(count) for name, count in KEPT_OUT_READ.findall(out[1])}
    return Tally(counts["reran"], counts["green"], counts["compiled"], counts["found"], written, kept, full,
                 tuple(sorted(by.items(), key=lambda it: (-it[1], it[0]))), steps)


def unchanged(said: Tally, apart: str, reruns: bool = False) -> str:
    """Why a build after a rename or a move (`ANONYMOUS`) did not do what one must, or '': the
    program is the same bytes as the one built from nothing (`apart`, `engine.first_difference`'s
    words) and, unless other bodies change identity with it (`reruns`, `Step.reruns`), the
    front end reran no step (the edited bodies' identities are the same, so every lookup hits).
    The note counts a whole build, not a body, so a step whose callers rerun is held to the
    bytes alone."""
    problems: list[str] = []
    if apart != "the same":
        problems.append(f"the program is not the same bytes as the one built from nothing: {apart}")
    if not reruns:
        if said.steps is None:
            problems.append("the build's `-v` note has no item store step counts")
        elif reran := [f"{name} {r}" for name, (_, r, _) in zip(STEP_NAMES, said.steps) if r]:
            problems.append(f"the front end reran steps: {', '.join(reran)}")
    return "; ".join(problems)


type Behaviour = tuple[object, bytes, bytes]


def verdict(blaze_ok: bool, fresh_ok: bool, blaze_said: str, fresh_said: str,
            seen: list[tuple[str, Behaviour, Behaviour]], unlike: str = "") -> tuple[report.Verdict, str]:
    """A step's verdict from both builds and, when both built, each check's behaviour from the
    program built from nothing and the one built with the item store, and after a rename or a
    move what `unchanged` found (`unlike`)."""
    if blaze_ok != fresh_ok:
        if fresh_ok:
            return "fail", f"the build with the item store failed and the one from nothing did not:\n{blaze_said[-2500:]}"
        return "fail", f"the build from nothing failed and the one with the item store did not:\n{fresh_said[-2500:]}"
    if not blaze_ok:
        return "pass", "both builds fail"
    for name, want, got in seen:
        if want != got:
            import appruns
            return "fail", (f"`{name}` differs: the program built from nothing exits {want[0]!r}, the one built "
                            f"with the item store {got[0]!r}: {appruns.symptom(want, got)}")
    if unlike:
        return "fail", f"after a rename or a move: {unlike}"
    return "pass", ""


# Running it.

class App:
    """One app's checkout, its two builds and its source as the sequence changes it."""

    def __init__(self, eng: engine.Engine, name: str, spec: dict[str, Any], work: Path, check: Check):
        import appruns as apps
        self.apps, self.eng, self.name, self.spec, self.check = apps, eng, name, spec, check
        self.edits = edits_of(spec)
        self.ws = Path(apps.checkout(name, spec)).resolve()
        apps.prepare(str(self.ws), spec)
        apps.localize(str(self.ws))
        apps.setup(str(self.ws), spec)
        self.fixture = apps.Program(str(self.ws), spec)
        main = Path(self.fixture.touch).resolve()
        self.root = str(main.relative_to(self.ws))
        self.sources = sources_of(self.ws, [main.parent, *((self.ws / e.file).parent for e in self.edits)])
        self.originals = {s: (self.ws / s).read_text() for s in sorted({*self.sources, *(e.file for e in self.edits)})}
        for at, e in enumerate(self.edits):
            if e.old not in self.originals[e.file]:
                raise engine.Failed(f"{name}: edits[{at}]'s `old` is not in {e.file}: the checkout is not as pinned")
        if left := [s for s, text in self.originals.items() if leftovers(text)]:
            raise engine.Failed(f"{name}: {', '.join(left)} hold this script's edits, left by an earlier run: "
                                f"check them out again")
        flags = f"--cap-lints allow {apps.own_flags(spec)}".strip()
        extra = {**spec.get("build_env", {}), "RUSTFLAGS": flags}
        self.blaze_target, self.fresh_target = self.ws / "target-item-store", self.ws / "target-scratch"
        self.env = eng.env(work / name / "home", work / name / "store", self.blaze_target, {**extra, ITEM_STORE: "verify"})
        self.fresh_home, self.fresh_store = work / name / "scratch-home", work / name / "scratch-store"
        self.fresh_extra = {**extra, ITEM_STORE: "off"}
        self.fresh_env: dict[str, str] | None = None
        self.args = ["-p", spec["package"], *spec.get("cargo_args", [])]
        program = spec.get("bin", spec["package"]) + engine.EXE
        self.blaze_program, self.fresh_program = (t / "debug" / program for t in (self.blaze_target, self.fresh_target))

    def write(self, tree: Tree) -> None:
        for path, text in tree.files.items():
            target = self.ws / path
            if target.read_text() != text:
                target.write_text(text)

    def restore(self) -> None:
        self.write(Tree(dict(self.originals)))

    def checks(self, made: Step) -> list[tuple[str, object]]:
        """What is run after a step, each with its name."""
        if self.check == "runs":
            return [(self.apps.name_of(run), run) for run in self.spec.get("runs", [])]
        own = self.edits[made.edit].check if made.edit is not None else None
        argv = list(own if own is not None else self.spec.get("edit_check", []))
        return [(self.apps.name_of(argv) or "(no arguments)", argv)]

    def build(self, label: str) -> tuple[engine.Ran, engine.Ran]:
        """The build with the item store, then blaze's build of the same source from nothing,
        whose home, store and target directory stay until `forget_fresh`. The one from nothing
        is made at the same target path, the other's waiting aside, so that the two programs can
        be the same bytes; then it moves to its own."""
        ran = self.eng.build(label, self.ws, self.env, [*self.args, "-v", "--message-format=json"])
        self.forget_fresh()
        aside = self.ws / f"{self.blaze_target.name}-aside"
        shutil.rmtree(aside, ignore_errors=True)
        self.blaze_target.mkdir(parents=True, exist_ok=True)
        os.rename(self.blaze_target, aside)
        try:
            self.fresh_env = self.eng.env(self.fresh_home, self.fresh_store, self.blaze_target, self.fresh_extra)
            fresh = self.eng.build(f"{label}: from nothing", self.ws, self.fresh_env, self.args)
        finally:
            if self.blaze_target.exists():
                os.rename(self.blaze_target, self.fresh_target)
            os.rename(aside, self.blaze_target)
        return ran, fresh

    def apart(self) -> str:
        """Where the program built from nothing and the one built with the item store first
        differ, in `engine.first_difference`'s words ("the same" when they do not)."""
        return engine.first_difference(self.fresh_program, self.blaze_program)

    def forget_fresh(self) -> None:
        """Stops the daemon of the build from nothing and removes its home, store and target
        directory, so no later step's build from nothing reads them."""
        try:
            if self.fresh_env is not None:
                self.eng.stop(self.ws, self.fresh_env)
        except engine.Failed:
            pass
        finally:
            self.fresh_env = None
            for path in (self.fresh_home, self.fresh_store, self.fresh_target):
                shutil.rmtree(path, ignore_errors=True)

    def behaviours(self, made: Step) -> list[tuple[str, Behaviour, Behaviour]]:
        seen: list[tuple[str, Behaviour, Behaviour]] = []
        for name, run in self.checks(made):
            both: list[Behaviour] = []
            for program in (self.fresh_program, self.blaze_program):
                with self.apps.Served(self.fixture, str(program)) as served:
                    both.append(cast(Behaviour, served.behaviour(run)))
            seen.append((name, both[0], both[1]))
        return seen

    def stop(self) -> None:
        try:
            self.eng.stop(self.ws, self.env)
        except engine.Failed:
            pass
        finally:
            self.forget_fresh()


def run_app(eng: engine.Engine, name: str, spec: dict[str, Any], args: Input, work: Path,
            rep: report.Report) -> list[dict[str, object]]:
    """The app's first build and its steps, each an outcome of `rep`; the steps as `extra` keeps them."""
    kept: list[dict[str, object]] = []
    try:
        app = App(eng, name, spec, work, args.check)
    except (engine.Failed, SystemExit, subprocess.SubprocessError, OSError, ValueError) as e:
        rep.add(f"{name}: checkout", "error", str(e)[-3000:])
        return kept
    rng = random.Random(f"item-store:{args.seed}:{name}")
    tree = Tree(dict(app.originals))
    history = [tree.copy()]
    was_built = False
    try:
        for i in range(args.steps + 1):
            made = Step("first", "the first build")
            label = f"{name}: step {i}"
            try:
                if i > 0:
                    made = step(rng, tree, history, app.edits, app.sources, app.root, args.synthetic, i)
                    history.append(tree.copy())
                label = f"{name}: step {i}: {made.action}"
                app.write(tree)
                ran, fresh = app.build(label)
                built = ran.returncode == 0 and fresh.returncode == 0
                seen = app.behaviours(made) if built else []
                # After a step that did not build, a step's result may never have been stored (a
                # failing borrow check, a MIR error): only a rename or move of a tree that built
                # is held to rerunning nothing.
                held = built and was_built and made.action in ANONYMOUS
                was_built = built
                apart = app.apart() if held else "the same"
            except (engine.Failed, SystemExit, OSError) as e:
                rep.add(label, "error", f"{made.what}\n{e}")
                break
            finally:
                app.forget_fresh()
            said = tally(ran)
            unlike = unchanged(said, apart, made.reruns) if held else ""
            verdict_, detail = verdict(ran.returncode == 0, fresh.returncode == 0, ran.said(), fresh.said(), seen,
                                       unlike)
            rep.add(label, verdict_, f"{made.what}" + (f"\n{detail}" if detail else ""),
                    blaze_s=round(ran.seconds, 2), fresh_s=round(fresh.seconds, 2), **said.numbers())
            kept.append(dict(app=name, step=i, action=made.action, what=made.what, verdict=verdict_,
                             refused_by=dict(said.refused_by),
                             lookups=None if said.steps is None else {
                                 of: dict(hits=h, ran=r, refused=f) for of, (h, r, f) in zip(STEP_NAMES, said.steps)}))
            if verdict_ != "pass" and not args.keep_going:
                break
    finally:
        app.restore()
        app.stop()
    return kept


def main(args: Input) -> None:
    if args.engine.kind != "v2":
        fail(SCRIPT, f"engine.kind: v2, not {args.engine.kind!r}: v1 has no item store")
    if args.steps < 0:
        fail(SCRIPT, f"steps: at least 0, not {args.steps}")
    import appruns
    specs = cast(dict[str, dict[str, Any]], json.loads(Path(appruns.SPECS).read_text()))
    if unknown := sorted(set(args.apps) - set(specs)):
        fail(SCRIPT, f"apps: apps.json has no {', '.join(unknown)}")
    if without := sorted(n for n in args.apps if not specs[n].get("edits")):
        fail(SCRIPT, f"apps: {', '.join(without)} has no `edits` (pick_edits.py chooses them)")
    names = list(args.apps) or sorted(n for n, s in specs.items() if s.get("edits"))
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-item-store-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    rep = report.Report(SCRIPT, args.engine, args.out)
    rep.extra.update(work=str(work), apps=names, seed=args.seed, steps=args.steps, toolchain=engine.TOOLCHAIN)
    print(f"work: {work}", flush=True)
    steps: list[dict[str, object]] = []
    for name in names:
        steps.extend(run_app(eng, name, specs[name], args, work, rep))
    rep.extra["steps"] = steps
    rep.finish()


# selftest

SAMPLE = """use std::fmt;

/// Adds.
#[inline]
pub fn add(a: u32, b: u32) -> u32 {
    let sum = a + b;
    sum
}

fn main() {
    let total = add(1, 2);
    let call = (1, [2, 3]);
    let open = foo(
        1,
    );
    println!("{total} {call:?}");
}

#[cfg(test)]
mod tests {
    fn helper() {
        let x = 1;
    }
}
"""


def test_input() -> None:
    base = {"engine": {"kind": "v2", "blaze": "b"}, "out": ""}
    got = parse(Input, json.dumps(base), SCRIPT)
    assert (got.steps, got.seed, got.check, got.synthetic, got.keep_going) == (20, 0, "edit", True, False), got
    for wrong, said in (({**base, "step": 3}, "no field step"), ({**base, "check": "all"}, "check is one of"),
                        ({**base, "apps": "ripgrep"}, "apps is a list")):
        assert said in refused(lambda: parse(Input, json.dumps(wrong), SCRIPT)), wrong
    assert "edits[0]" in refused_failed(lambda: edits_of({"edits": [{"file": "a", "old": "x"}]}))
    assert edits_of({"edits": [{"file": "a", "old": "x", "new": "y", "what": "w", "check": ["--help"]}]}) == [
        Edit("a", "x", "y", "w", ("--help",))]


def test_statement() -> None:
    sites = statement_sites(SAMPLE)
    lines = SAMPLE.splitlines()
    assert [lines[s].strip() for s in sites] == ["let x = 1;"], [lines[s] for s in sites]
    added = add_statement(SAMPLE, sites[0], 7)
    assert "        let x = 1;\n        let _item_store_7 = 7u32;\n" in added, added


def test_statement_in_a_body() -> None:
    text = "fn f() {\n    if true {\n        let a = g(1);\n        let b = [1, 2];\n    }\n}\n"
    assert statement_sites(text) == [2, 3], statement_sites(text)
    assert add_statement(text, 3, 2).count("_item_store_2") == 1


def test_probe() -> None:
    text = SAMPLE + probe(3)
    assert text.replace(probe(3), "", 1) == SAMPLE
    assert f"impl ::core::fmt::Display for {PROBE}3" in probe(3) and probe(3) != probe(4)
    assert leftovers(text) and leftovers(add_statement(SAMPLE, statement_sites(SAMPLE)[0], 1)) and not leftovers(SAMPLE)


def test_sources() -> None:
    with tempfile.TemporaryDirectory() as d:
        ws = Path(d)
        for name in ("crates/core/main.rs", "crates/core/flags/mod.rs", "crates/core/target/x.rs",
                     "crates/core/.git/y.rs", "crates/other/lib.rs", "crates/core/notes.md"):
            (ws / name).parent.mkdir(parents=True, exist_ok=True)
            (ws / name).write_text("")
        got = sources_of(ws, [ws / "crates/core", ws / "crates/core/flags"])
        assert got == ["crates/core/flags/mod.rs", "crates/core/main.rs"], got


def test_functions() -> None:
    spans = functions(SAMPLE)
    lines = SAMPLE.splitlines(keepends=True)
    assert [lines[a].strip() for a, _ in spans] == ["/// Adds.", "fn main() {"], spans
    moved = move_function(SAMPLE, spans[0])
    assert moved.endswith("/// Adds.\n#[inline]\npub fn add(a: u32, b: u32) -> u32 {\n    let sum = a + b;\n    sum\n}\n"), moved
    assert moved.count("pub fn add") == 1 and moved.startswith("use std::fmt;\n\n\nfn main() {"), moved
    after_probe = move_function(SAMPLE + probe(1) + "\nfn tail() {\n}\n", functions(SAMPLE + probe(1) + "\nfn tail() {\n}\n")[1])
    assert probe(1) in after_probe, "moving a function keeps the text around it whole"
    assert functions("fn last() {\n}\n") == [], "a function that ends the file stays"
    assert functions("fn decl();\nfn x() {\n}\nstruct S;\n") == [(1, 3)]


HELPERS = """
fn double(x: u32) -> u32 {
    let twice = x * 2;
    let shown = format!("{twice}");
    twice + double_of(shown.len() as u32)
}

fn double_of(n: u32) -> u32 {
    n
}

impl Wrap {
    fn double(&self) -> u32 {
        self.0
    }
}
"""


def test_local_renames() -> None:
    sites = local_renames(SAMPLE + HELPERS)
    assert [name for _, _, name in sites] == ["sum", "open"], sites
    renamed = rename_local(SAMPLE, sites[0], 4)
    assert "    let sum_item_store_4 = a + b;\n    sum_item_store_4\n}" in renamed, renamed
    assert renamed.count("sum") == 2 and leftovers(renamed), renamed
    # `total` and `call` are read by a format string, `twice` too, `shown` by a method call.
    assert not any(name in ("total", "call", "twice") for _, _, name in sites)
    lines = ["    let s = S { x };\n"]
    assert not renamable(lines, "x", None) and not renamable(["    p.x\n"], "x", None)
    assert renamable(["    let x: u32 = 1;\n", "    f(x, 2)\n"], "x", 0)
    assert not renamable(["    let x: u32 = 1;\n", "    S { x: 1 }\n"], "x", 0)


def test_fn_renames() -> None:
    files = {"src/main.rs": SAMPLE + HELPERS}
    # `double` is a method too, `add` is public, `main` is `main`, `helper` is in a module.
    assert fn_renames(files, ["src/main.rs"]) == ["double_of"], fn_renames(files, ["src/main.rs"])
    assert called_elsewhere(files, ["src/main.rs"], "double_of")
    assert not called_elsewhere({"a.rs": "fn alone() {\n}\n"}, ["a.rs"], "alone")
    rename_fn(files, ["src/main.rs"], "double_of", 2)
    text = files["src/main.rs"]
    assert "fn double_of_item_store_2(n: u32)" in text and "double_of_item_store_2(shown.len()" in text, text
    assert fn_renames(files, ["src/main.rs"]) == [], "a renamed function is not renamed again"
    used = {"a.rs": "fn f() {\n}\n", "b.rs": "use crate::a::f;\nfn g() {\n    f();\n}\n"}
    assert fn_renames(used, ["a.rs", "b.rs"]) == ["g"], "a name a `use` names is left"


def test_sequence() -> None:
    edits = [Edit("src/main.rs", "    let total = add(1, 2);\n", "    let total = add(1, 3);\n", "a number"),
             Edit("src/main.rs", "    sum\n", "    sum + 0\n")]

    def run(seed: int) -> list[Step]:
        rng = random.Random(f"item-store:{seed}:sample")
        tree = Tree({"src/main.rs": SAMPLE + HELPERS})
        history = [tree.copy()]
        made: list[Step] = []
        for n in range(1, 41):
            made.append(step(rng, tree, history, edits, ["src/main.rs"], "src/main.rs", True, n))
            history.append(tree.copy())
            assert all(edits[i].new in tree.files[edits[i].file] for i in tree.made), (n, made[-1])
            assert all(probe(k) in tree.files["src/main.rs"] for k in tree.probes), (n, made[-1])
        return made

    first = run(1)
    assert first == run(1), "a seed gives one sequence"
    assert first != run(2)
    every = {s.action for seed in range(6) for s in run(seed)}
    assert every == {"apply", "revert", "statement", "impl-add", "impl-remove", "move", "rename-local", "rename-fn",
                     "earlier"}, every


def test_only_app_edits() -> None:
    edits = [Edit("src/main.rs", "    sum\n", "    sum + 0\n")]
    rng = random.Random(0)
    tree = Tree({"src/main.rs": SAMPLE})
    history = [tree.copy()]
    for n in range(1, 11):
        made = step(rng, tree, history, edits, ["src/main.rs"], "src/main.rs", False, n)
        history.append(tree.copy())
        assert made.action in ("apply", "revert", "earlier"), made
    assert "no step can be made" in refused_failed(
        lambda: step(random.Random(0), Tree({"a.rs": ""}), [Tree({"a.rs": ""})], [], ["a.rs"], "a.rs", False, 1))


def ran(stdout: str, stderr: str = "", code: int = 0) -> engine.Ran:
    return engine.Ran(("blaze", "build"), 1.0, code, stdout, stderr)


def test_tally() -> None:
    note = ("note: the persistent goal cache, of the last build: 3 written, 1 kept, 0 with every slot full; "
            "the item store: 12 written, 40 kept, 1 with every slot full; steps kept out by reads of "
            "`const_eval_query` (5), `rpit_hidden_types` (2); 4 of bodies with no identity")
    work = '{"reason": "blaze-work", "reran": 9, "green": 100, "compiled": 3, "found": 7, "link": "patched", "critical_path_us": 1}'
    got = tally(ran(work + "\n", note + "\n"))
    assert got == Tally(9, 100, 3, 7, 12, 40, 1, (("const_eval_query", 5), ("rpit_hidden_types", 2))), got
    assert got.numbers()["refused"] == 7
    message = json.dumps({"reason": "compiler-message", "message": {"message": note.removeprefix("note: "),
                                                                    "rendered": note + "\n"}})
    assert tally(ran(work + "\n" + message + "\n")) == got
    plain = tally(ran("", "note: the item store: 0 written, 2 kept, 0 with every slot full\n"))
    assert (plain.written, plain.kept, plain.refused_by, plain.reran, plain.steps) == (0, 2, (), None, None), plain
    none = tally(ran("", "error: something\n", 101))
    assert none == Tally() and none.numbers()["refused"] is None and none.numbers()["ran"] is None, none
    this = ("; the item store, this build: inference 6 hit, 1 ran, 0 refused; MIR 5 hit, 2 ran, 1 refused; "
            "borrow check 0 hit, 0 ran, 0 refused")
    counted = tally(ran("", note + this + "\n"))
    assert counted.steps == ((6, 1, 0), (5, 2, 1), (0, 0, 0)), counted
    assert counted.refused_by == got.refused_by and counted.numbers()["ran"] == 3, counted
    first = tally(ran("", "note: " + this.removeprefix("; ") + "\n"))
    assert first.steps == counted.steps and first.written is None, first
    assert unchanged(first, "the same") == "the front end reran steps: inference 1, MIR 2"
    assert unchanged(Tally(steps=((3, 0, 0),) * 3), "the same") == ""
    assert "no item store step counts" in unchanged(Tally(), "the same")
    assert "not the same bytes" in unchanged(Tally(steps=((3, 0, 0),) * 3), "byte 0x10: 0x01 against 0x02")
    assert unchanged(first, "the same", reruns=True) == "", "callers of a renamed function rerun"
    assert "not the same bytes" in unchanged(first, "one ends at byte 0x8", reruns=True)


def test_verdict() -> None:
    same: Behaviour = (0, b"out", b"")
    assert verdict(True, True, "", "", [("x", same, same)]) == ("pass", "")
    assert verdict(False, False, "e", "e", []) == ("pass", "both builds fail")
    v, d = verdict(False, True, "error: the item store: a stored hit of `f` differs from a fresh run", "", [])
    assert v == "fail" and "stored hit of `f`" in d, d
    assert verdict(True, False, "", "error[E0308]", [])[0] == "fail"
    v, d = verdict(True, True, "", "", [("--help", same, (1, b"out", b""))])
    assert v == "fail" and "`--help` differs" in d, d
    v, d = verdict(True, True, "", "", [("x", same, same)], "the front end reran steps: MIR 1")
    assert v == "fail" and d == "after a rename or a move: the front end reran steps: MIR 1", d
    assert verdict(False, False, "e", "e", [], "ignored")[0] == "pass"


def test_tree_written() -> None:
    with tempfile.TemporaryDirectory() as d:
        root = Path(d)
        (root / "src").mkdir()
        (root / "src" / "main.rs").write_text(SAMPLE)
        app = object.__new__(App)
        app.ws, app.originals = root, {"src/main.rs": SAMPLE}
        app.write(Tree({"src/main.rs": SAMPLE + probe(1)}))
        assert (root / "src" / "main.rs").read_text().endswith(probe(1))
        app.restore()
        assert (root / "src" / "main.rs").read_text() == SAMPLE


def refused(fn: Callable[[], object]) -> str:
    try:
        fn()
    except SystemExit as e:
        return str(e)
    raise AssertionError("not refused")


def refused_failed(fn: Callable[[], object]) -> str:
    try:
        fn()
    except engine.Failed as e:
        return str(e)
    raise AssertionError("not refused")


SELFTESTS = (test_input, test_statement, test_statement_in_a_body, test_probe, test_sources, test_functions,
             test_local_renames, test_fn_renames, test_sequence, test_only_app_edits, test_tally, test_verdict,
             test_tree_written)


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
