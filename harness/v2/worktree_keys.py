"""worktree_keys.py: `a_second_checkout_at_another_path_reuses_every_key`. One commit of a real
project in two checkouts at two different paths, built one after the other by one daemon with
one store: the second build has to find in the store everything the first compiled, because no
key may hold where a checkout lives (/tmp/blaze-lanes/v2-kernel.brief.md, "Identity";
docs/cargo.md, "Where a build writes"). The idea is kache's benchmark
(github.com/kunobi-ninja/kache), which builds one commit in two worktrees and fails unless
every key matches.

    uv run --frozen python harness/v2/worktree_keys.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"},
        "duckdb": "/nix/store/...-duckdb/bin/duckdb", "out": "/tmp/worktree-keys.json"}'
    uv run --frozen python harness/v2/worktree_keys.py selftest

The input is one JSON object (inline, a file's path, or `-` for stdin):

    engine    the blaze under test (engine.EngineSpec); v2 only
    duckdb    the DuckDB `blaze debug sql` runs, an absolute path
              (`nix build --print-out-paths .#duckdb`/bin/duckdb)
    out       the path the result JSON is written to (report.py)
    project   the project of bench.PROJECTS, at its pinned revision ("ripgrep": 15.1.0 at
              af60c2de, with build scripts of its own and of serde, libc and proc-macro2,
              serde_derive as its proc macro, and generics throughout)
    args      more arguments of both `blaze build`s (say ["--release"]); none by default
    daemon    "same": every checkout is built by the daemon that built A (the default, as two
              worktrees of one user are); "restarted": the daemon is stopped after each build and
              the next starts a new one on the same home and store, so nothing a later build
              finds can come from an earlier one's memory
    third     also build a third checkout C after B (true). The portable keys verify a run
              before sharing it: a checkout of another origin runs it again and compares
              (crates/blaze-usercode/src/roots/verify.rs `keeps`), so B's first build-script
              runs and expansions miss by design and C's are the first that can hit. C's
              outcomes say which of B's misses were that step and which are a key that differs
    tree      what the blaze under test was built from, as a content digest of its sources,
              kept in the result as given (rule 22); the blaze binary's BLAKE3 is added
    work      the directory everything goes in; empty: a new one under /tmp
    keep      keep the work directory after the run

The checkouts are copies of one fetched tree at `<work>/a/<project>`,
`<work>/b/second checkout/<project>` and `<work>/c/third/checkout/deeper/<project>`: the same
last component (so the build directories, `<home>/build/<name>-<id>`, differ only by the id
that hashes the path), other lengths and depths, and a space. One home `<work>/home`, empty at
the start, holds the daemon's socket, state, history and store and every build directory. They
are built in order, A first with the empty store, each by `blaze build --message-format=json`
and then asked `blaze codegen-inputs --format=json`; the run then stops the daemon (which
flushes its history) and reads the history with `blaze debug sql --json`.

Each later checkout is held to A. One outcome each, named
`a_second_checkout_at_another_path_reuses_every_key/<part>` for B and
`a_second_checkout_at_another_path_reuses_every_key/C/<part>` for C:

    trees       blaze's history names the same tree digest for both builds (`builds.tree`):
                otherwise the copies differ and nothing below follows. Error, not fail
    units       every unit (`compiler-artifact`) B built is `fresh`: replayed from the
                store's record, not compiled. A compiled unit is named by package, target
                and profile, with blaze's reason it kept no record when it gave one (the
                `unit record not kept` event). B's units have to be A's units too
    build-script-runs
                every build-script run of B was found. `build-script-executed` has no
                `fresh` yet, so the number compiled is `blaze-work`'s units compiled less the
                artifacts compiled, after checking that `found + compiled` is the artifacts
                and runs together (an error when not). Each of B's runs is also checked to
                name only B's places: A's checkout or build directory in its `out_dir`,
                `env`, `linked_paths` or `cfgs` is a stand-in that did not hide the path
    proc-macro-expansions
                B ran no expansion: a kept expansion opens no `macro expand` span, so each
                such span of B's build is one that missed, named by calling crate and macro.
                A has to have some (else the check proves nothing: an error). A `macro
                module` of B that was `compiled` (not from memo or store) is a macro library
                whose content ID differs between the checkouts
    instances   `blaze codegen-inputs --format=json` of A and of B, every instance's key and
                code hash, are the same set: an instance key only one checkout has hashes
                where that checkout is
    function-keys
                each function, unit, build-script run and expansion B compiled, by name, with
                its key's parts and which part differed from A's, and any part whose text
                holds a checkout's or a build directory's path. This reads `blaze-keys` lines
                (`KEYS_LINE` below), which blaze does not print yet: until it does this is an
                error naming what it needs and where the data is

A failing outcome also lists where the store's files hold a checkout's or build directory's
path (a lead, not a verdict: hot records are stored as they are, cold ones compressed, and a
record kept for one place may name it). `selftest` checks the parsing and the verdicts on
made-up output, without blaze, cargo or the network.
"""

import asyncio
import json
import mmap
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.parse
from collections import Counter
from collections.abc import Iterable, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal, cast

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
import budgets
import engine
import report
from typed_input import fail, parse

SCRIPT = "worktree_keys.py"
PROPERTY = "a_second_checkout_at_another_path_reuses_every_key"
BUILD_SECS = 3600
FETCH_SECS = 1800
QUERY_SECS = 300
STOP_SECS = 300
INPUTS_SECS = 600
SHOWN = 40
SECOND = "second checkout"
CHECKOUT, BUILD_DIR = "<checkout>", "<build dir>"

# What `function-keys` reads: one line a key blaze looked up, printed under
# `--message-format=json` beside `blaze-work`. Not printed yet; the data is in memory at
# crates/blaze-codegen/src/driver.rs (`Built.kernel`: `KernelEntry { instance, step, key, hit }`
# a dev template; `Built.debug_names`: each instance key's name), the unit's `record_key` at
# crates/blaze-cargo/src/serve/unit/compile.rs, `blaze_usercode::index::find` for build-script
# runs (crates/blaze-cargo/src/serve/unit/script.rs) and the kept expansion's lookup at
# crates/blaze-macro-eval/src/expander.rs.
KEYS_LINE = (
    '{"reason": "blaze-keys", "kind": "unit" | "function" | "build-script-run" | "expansion", '
    '"name": "<a unit as its span names it, an instance by path and arguments, a package\'s run, '
    'a macro path at its calling crate>", "key": "<hex of the store key looked up>", '
    '"from": "store" | "memo" | "compiled", "parts": {"<part>": "<hex>", ...}, '
    '"inputs": {"<part>": "<its text, where it is text: a variable, a root\'s layout, a path>", ...}}'
)
KEYS_MISSING = (
    "blaze prints no `blaze-keys` line, so what B compiled cannot be named function by function with its "
    "key's parts. Wanted, under `--message-format=json`, one line a key looked up: " + KEYS_LINE + ". The data "
    "is there: crates/blaze-codegen/src/driver.rs `Built.kernel` (KernelEntry { instance, step, key, hit }) and "
    "`Built.debug_names`, compile.rs `record_key` for units, script.rs `blaze_usercode::index::find` for "
    "build-script runs, expander.rs's kept lookup for expansions; through `Worked` "
    "(crates/blaze-api/src/lib.rs) and `work_json` (crates/blaze-ui/src/api.rs)"
)
SPANS_QUERY = (
    "SELECT build_id, name, \"crate\" AS krate, function, module_from, count(*) AS n FROM spans "
    "WHERE name IN ('macro expand', 'macro module') GROUP BY ALL ORDER BY ALL"
)
BUILDS_QUERY = "SELECT build_id, workspace, command, tree, blaze, outcome FROM builds ORDER BY build_id"
# The message is the message alone (schema version 4); `events_wide.fields` has its unit and crate.
EVENTS_QUERY = (
    "SELECT build_id, message || coalesce(' ' || fields, '') AS message FROM events_wide "
    "WHERE message LIKE 'unit record not kept:%' ORDER BY ALL"
)

type Daemon = Literal["same", "restarted"]
type Verdict = tuple[report.Verdict, str, dict[str, report.Number]]
type Row = dict[str, object]


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    duckdb: str
    out: str
    project: str = "ripgrep"
    args: tuple[str, ...] = ()
    daemon: Daemon = "same"
    third: bool = True
    tree: str = ""
    work: str = ""
    keep: bool = False


# Places, and their names in what blaze printed.

def spellings(path: Path) -> list[str]:
    """The ways `path` can be written that blaze may print: as given, resolved, on macOS with
    `/private` put in front or taken off (`/tmp` is `/private/tmp`), and each percent-escaped."""
    out = [str(path), os.path.realpath(path)]
    for it in list(out):
        if it.startswith("/private/"):
            out.append(it.removeprefix("/private"))
        elif sys.platform == "darwin" and it.startswith(("/tmp/", "/var/")):
            out.append("/private" + it)
    # A path package's id is a URL (`path+file:///...`, blaze-manifest's `SourceId::for_path`),
    # which writes a space as `%20`.
    out += [urllib.parse.quote(it, safe="/") for it in out]
    return sorted(set(out), key=lambda it: (-len(it), it))


@dataclass(frozen=True)
class Places:
    """The places of the run, by name ("checkout A", "build dir B", ...), each in every
    spelling, longest first."""

    named: dict[str, list[str]]

    @staticmethod
    def of(paths: dict[str, Path]) -> "Places":
        return Places({name: spellings(path) for name, path in paths.items()})

    def found(self, text: str, names: Iterable[str] | None = None) -> list[str]:
        """The places of `names` (all by default) that `text` holds, in any spelling."""
        chosen = self.named if names is None else {n: self.named[n] for n in names if n in self.named}
        return [name for name, ways in chosen.items() if any(way in text for way in ways)]

    def neutral(self, text: str) -> str:
        """`text` with either checkout written `<checkout>` and either build directory
        `<build dir>`: how A's and B's names of one thing are compared."""
        pairs = [(way, CHECKOUT if name.startswith("checkout") else BUILD_DIR)
                 for name, ways in self.named.items() for way in ways]
        for way, token in sorted(pairs, key=lambda it: -len(it[0])):
            text = text.replace(way, token)
        return text


# What a build printed.

@dataclass(frozen=True)
class Unit:
    """One `compiler-artifact` line, its package written with `Places.neutral`."""

    package: str
    kinds: tuple[str, ...]
    name: str
    test: bool
    opt_level: str
    fresh: bool
    filenames: tuple[str, ...]

    @property
    def ident(self) -> tuple[str, tuple[str, ...], str, bool, str]:
        return (self.package, self.kinds, self.name, self.test, self.opt_level)

    @property
    def label(self) -> str:
        return (f"{self.package} {'/'.join(self.kinds)} `{self.name}` (opt-level {self.opt_level}"
                f"{', test' if self.test else ''})")


@dataclass(frozen=True)
class ScriptRun:
    """One `build-script-executed` line: its package (neutral), what it handed the build, as
    printed, and whether it was found, where blaze says (None: it does not)."""

    package: str
    out_dir: str
    env: tuple[tuple[str, str], ...]
    linked_paths: tuple[str, ...]
    cfgs: tuple[str, ...]
    fresh: bool | None


@dataclass(frozen=True)
class KeyLine:
    """One `blaze-keys` line (`KEYS_LINE`), its name written with `Places.neutral`."""

    kind: str
    name: str
    key: str
    source: str
    parts: dict[str, str]
    inputs: dict[str, str]

    @property
    def ident(self) -> tuple[str, str]:
        return (self.kind, self.name)


@dataclass(frozen=True)
class Built:
    """What one `blaze build --message-format=json` printed."""

    which: str
    ws: Path
    seconds: float
    work: Row
    units: tuple[Unit, ...]
    scripts: tuple[ScriptRun, ...]
    keys: tuple[KeyLine, ...]
    served: tuple[Row, ...] = field(default=())

    def count(self, name: str) -> int:
        return engine.work_count(f"{self.which}'s blaze-work", self.work, name)


def need[T](label: str, row: Row, name: str, kind: type[T]) -> T:
    value = row.get(name)
    if not isinstance(value, kind) or (kind is not bool and isinstance(value, bool)):
        raise engine.Failed(f"{label}: `{name}` is {value!r}, not a {kind.__name__}: {json.dumps(row)[:600]}")
    return value


def strings(label: str, row: Row, name: str) -> tuple[str, ...]:
    value = cast(list[object], need(label, row, name, list))
    if not all(isinstance(it, str) for it in value):
        raise engine.Failed(f"{label}: `{name}` is not a list of strings: {value!r}")
    return tuple(cast(list[str], value))


def text_map(label: str, row: Row, name: str) -> dict[str, str]:
    value = row.get(name, {})
    if not isinstance(value, dict) or not all(isinstance(k, str) and isinstance(v, str)
                                              for k, v in cast(dict[object, object], value).items()):
        raise engine.Failed(f"{label}: `{name}` is not an object of strings: {value!r}")
    return dict(cast(dict[str, str], value))


def unit_of(label: str, row: Row, places: Places) -> Unit:
    target = cast(Row, need(label, row, "target", dict))
    profile = cast(Row, need(label, row, "profile", dict))
    return Unit(places.neutral(need(label, row, "package_id", str)), strings(label, target, "kind"),
                need(label, target, "name", str), need(label, profile, "test", bool),
                need(label, profile, "opt_level", str), need(label, row, "fresh", bool),
                strings(label, row, "filenames"))


def script_of(label: str, row: Row, places: Places) -> ScriptRun:
    pairs = cast(list[object], need(label, row, "env", list))
    env: list[tuple[str, str]] = []
    for pair in pairs:
        if (not isinstance(pair, list) or len(cast(list[object], pair)) != 2
                or not all(isinstance(it, str) for it in cast(list[object], pair))):
            raise engine.Failed(f"{label}: an `env` entry {pair!r} is not [name, value]")
        name, value = cast(list[str], pair)
        env.append((name, value))
    fresh = row.get("fresh")
    if fresh is not None and not isinstance(fresh, bool):
        raise engine.Failed(f"{label}: `fresh` is {fresh!r}, not a bool")
    return ScriptRun(places.neutral(need(label, row, "package_id", str)), need(label, row, "out_dir", str),
                     tuple(env), strings(label, row, "linked_paths"), strings(label, row, "cfgs"), fresh)


def key_of(label: str, row: Row, places: Places) -> KeyLine:
    source = need(label, row, "from", str)
    if source not in ("store", "memo", "compiled"):
        raise engine.Failed(f"{label}: `from` is {source!r}, not store, memo or compiled")
    return KeyLine(need(label, row, "kind", str), places.neutral(need(label, row, "name", str)),
                   need(label, row, "key", str), source, text_map(label, row, "parts"),
                   text_map(label, row, "inputs"))


def parsed(which: str, ws: Path, ran: engine.Ran, places: Places) -> Built:
    """The lines of `ran`'s stdout that say what the build did; a malformed one is a Failed."""
    label = f"{which}'s `blaze build --message-format=json`"
    work: list[Row] = []
    units: list[Unit] = []
    scripts: list[ScriptRun] = []
    keys: list[KeyLine] = []
    served: list[Row] = []
    for line in ran.stdout.splitlines():
        if not line.startswith("{"):
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as e:
            raise engine.Failed(f"{label}: a line is no JSON ({e}): {line[:300]}") from None
        if not isinstance(value, dict):
            continue
        row = cast(Row, value)
        match row.get("reason"):
            case "blaze-work":
                work.append(row)
            case "compiler-artifact":
                units.append(unit_of(label, row, places))
            case "build-script-executed":
                scripts.append(script_of(label, row, places))
            case "blaze-keys":
                keys.append(key_of(label, row, places))
            case "blaze-served":
                served.append(row)
            case _:
                pass
    if len(work) != 1:
        raise engine.Failed(f"{label}: {len(work)} `blaze-work` lines, not one:\n{ran.said()}")
    return Built(which, ws, ran.seconds, engine.required(label, work[0]), tuple(units), tuple(scripts),
                 tuple(keys), tuple(served))


def build_dir_of(built: Built, builds: Path) -> Path:
    """The one directory under `builds` (`<home>/build`) that `built`'s artifacts are in."""
    tops = {Path(way).resolve() for way in spellings(builds)}
    found: set[Path] = set()
    for unit in built.units:
        for name in unit.filenames:
            path = Path(name).resolve()
            for top in tops:
                if path.is_relative_to(top) and path != top:
                    found.add(top / path.relative_to(top).parts[0])
    if len(found) != 1:
        raise engine.Failed(f"{built.which}'s build directory: its artifacts are in {len(found)} directories under "
                            f"{builds} ({', '.join(sorted(map(str, found))) or 'none'}), not one")
    return found.pop()


# The verdicts. Each takes what the builds said and gives (verdict, detail, numbers).

def shown(lines: Sequence[str]) -> str:
    more = f"\n... and {len(lines) - SHOWN} more" if len(lines) > SHOWN else ""
    return "\n".join(lines[:SHOWN]) + more


def units_verdict(a: Built, b: Built, not_kept: Sequence[str]) -> Verdict:
    a_compiled = Counter(u.ident for u in a.units if not u.fresh)
    a_all, b_all = Counter(u.ident for u in a.units), Counter(u.ident for u in b.units)
    b_compiled = [u for u in b.units if not u.fresh]
    numbers: dict[str, report.Number] = {
        "units_a": len(a.units), "units_a_compiled": sum(a_compiled.values()), "units_b": len(b.units),
        "units_b_found": len(b.units) - len(b_compiled), "units_b_compiled": len(b_compiled)}
    if not a_compiled:
        return ("error", f"A compiled no unit with an empty store, so what {b.which} found proves nothing", numbers)
    problems = [f"{u.label}: compiled again" + ("" if u.ident in a_compiled else
                                                " (A built it but did not compile it)" if u.ident in a_all else
                                                " (A did not build it)") for u in b_compiled]
    only = [(f"only A built {Unit(*k, fresh=False, filenames=()).label}", n) for k, n in (a_all - b_all).items()]
    only += [(f"only {b.which} built {Unit(*k, fresh=False, filenames=()).label}", n)
             for k, n in (b_all - a_all).items()]
    problems += [what + (f" ({n} times)" if n > 1 else "") for what, n in sorted(only)]
    if not problems:
        return ("pass", f"{b.which} found all {len(b.units)} units in the store", numbers)
    why = ([f"blaze's reasons it kept no record ({b.which}'s build):", *("  " + it for it in not_kept)]
           if not_kept else [f"blaze gave no reason it kept no record (no `unit record not kept` event of {b.which}'s)"])
    return ("fail", shown(problems) + "\n" + shown(why), numbers)


def scripts_verdict(a: Built, b: Built, places: Places) -> Verdict:
    """B's build-script runs against A's. `b.which` names B's places in `places` ("checkout B",
    "build dir B"); every other place is another checkout's, which B's runs must not name."""
    numbers: dict[str, report.Number] = {"runs_a": len(a.scripts), "runs_b": len(b.scripts)}
    if not a.scripts:
        return ("error", "A ran no build script, so there is nothing for B to find", numbers)
    problems: list[str] = []
    own = (f"checkout {b.which}", f"build dir {b.which}")
    others = [name for name in places.named if name not in own]
    runs_a, runs_b = Counter(run.package for run in a.scripts), Counter(run.package for run in b.scripts)
    problems += [f"A ran {package}'s build script and {b.which} printed no run of it" for package in runs_a - runs_b]
    problems += [f"{b.which} ran {package}'s build script and A did not" for package in runs_b - runs_a]
    for run in b.scripts:
        printed = [("out_dir", run.out_dir), *((f"env {k}", v) for k, v in run.env),
                   *(("linked_paths", it) for it in run.linked_paths), *(("cfgs", it) for it in run.cfgs)]
        for where, value in printed:
            if held := places.found(value, others):
                problems.append(f"{b.which}'s run of {run.package} was handed {' and '.join(held)} in {where}: {value}")
        if not places.found(run.out_dir, own[1:]):
            problems.append(f"{b.which}'s run of {run.package} has its OUT_DIR {run.out_dir} outside its build "
                            "directory")
    if all(run.fresh is not None for run in b.scripts):
        ran = [run for run in b.scripts if run.fresh is False]
        problems += [f"{b.which} ran {run.package}'s build script again" for run in ran]
        numbers.update(runs_b_found=len(b.scripts) - len(ran), runs_b_compiled=len(ran))
    else:
        # `compiled` counts every unit compiled again, with an artifact line or without: a
        # build-script run, or a script's build that is inert and prints none
        # (crates/blaze-cargo/src/serve/job.rs `started_unit`). Which of them it was is not
        # printed: `build-script-executed` has no `fresh`. None compiled beyond the artifacts
        # means no run was; some is a failure of the property either way.
        artifacts_compiled = sum(1 for u in b.units if not u.fresh)
        beyond = b.count("compiled") - artifacts_compiled
        numbers.update(units_without_artifact_compiled=beyond)
        if beyond < 0:
            return ("error", (f"{b.which}'s blaze-work says {b.count('compiled')} units compiled and it printed "
                              f"{artifacts_compiled} compiled artifacts: the counts do not hold together"), numbers)
        if beyond:
            problems.append(f"{b.which} compiled {beyond} units again that print no artifact line (blaze-work's "
                            f"compiled {b.count('compiled')} less {artifacts_compiled} artifacts compiled): "
                            "build-script runs, or inert script builds; which, blaze does not say "
                            f"(`build-script-executed` has no `fresh`). {b.which}'s runs: "
                            + ", ".join(sorted({run.package for run in b.scripts})))
    if not problems:
        return ("pass", f"{b.which} found all {len(b.scripts)} build-script runs, each naming only its own places",
                numbers)
    return ("fail", shown(problems), numbers)


def expansions_verdict(which: str, a_ids: set[int], b_ids: set[int], rows: Sequence[Row]) -> Verdict:
    def of(ids: set[int], name: str) -> list[Row]:
        return [r for r in rows if r.get("build_id") in ids and r.get("name") == name]

    def n(r: Row) -> int:
        value = r.get("n")
        return value if isinstance(value, int) else 0

    a_expanded, b_expanded = of(a_ids, "macro expand"), of(b_ids, "macro expand")
    b_modules = of(b_ids, "macro module")
    compiled = [r for r in b_modules if r.get("module_from") == "compiled"]
    numbers: dict[str, report.Number] = {
        "expansions_a": sum(map(n, a_expanded)), "expansions_b_ran": sum(map(n, b_expanded)),
        "modules_b": sum(map(n, b_modules)), "modules_b_compiled": sum(map(n, compiled))}
    if not a_ids or not b_ids:
        return ("error", (f"the history has no build of {'A' if not a_ids else which}, so its spans cannot be "
                "told apart"), numbers)
    if not a_expanded:
        return ("error", (f"A's history has no `macro expand` span under A's build, so {which} having none proves "
                "nothing (are expansions attributed to the build that asked for them?)"), numbers)
    problems = [f"crate `{r.get('krate')}`: `{r.get('function')}` expanded again ({n(r)} times)" for r in b_expanded]
    problems += [f"macro module `{r.get('function')}` compiled again ({n(r)} times): its library's content ID "
                 "differs between the checkouts" for r in compiled]
    if not problems:
        return ("pass", f"{which} ran none of the {numbers['expansions_a']} expansions A ran", numbers)
    return ("fail", shown(problems), numbers)


def instances_verdict(which: str, a: dict[str, str], b: dict[str, str]) -> Verdict:
    numbers: dict[str, report.Number] = {"instances_a": len(a), "instances_b": len(b)}
    if not a:
        return ("error", "A's codegen inputs are empty, so the comparison proves nothing", numbers)
    only_a, only_b = sorted(a.keys() - b.keys()), sorted(b.keys() - a.keys())
    clash = sorted(k for k in a.keys() & b.keys() if a[k] != b[k])
    numbers.update(only_a=len(only_a), only_b=len(only_b), clashing=len(clash))
    if not (only_a or only_b or clash):
        return ("pass", f"the same {len(a)} instance keys and code hashes in both", numbers)
    lines = [(f"{len(only_b)} instance keys only {which} has (their identity holds where {which} is), "
             f"{len(only_a)} only A has; blaze names no instance by its key (`Built.debug_names` is not printed)")]
    lines += [f"only {which}: {k}" for k in only_b[:10]] + [f"only A: {k}" for k in only_a[:10]]
    lines += [f"key {k}: code hash {a[k]} in A, {b[k]} in {which} (two values under one key, rule 21)"
              for k in clash[:10]]
    return ("fail", "\n".join(lines), numbers)


def keys_verdict(a: Built, b: Built, places: Places) -> Verdict:
    if not a.keys and not b.keys:
        return ("error", KEYS_MISSING, {})
    by_kind = Counter((k.kind, k.source) for k in b.keys)
    numbers: dict[str, report.Number] = {f"{kind}_{source}": n for (kind, source), n in sorted(by_kind.items())}
    a_by = {k.ident: k for k in a.keys}
    problems: list[str] = []
    for key in b.keys:
        if key.source != "compiled":
            continue
        line = f"{key.kind} {key.name}: compiled again under {key.key}"
        before = a_by.get(key.ident)
        if before is None:
            line += "; A looked up no key of that name"
        elif before.key == key.key:
            line += "; A had the same key, so the store did not hand back A's entry (not a key that differs)"
        else:
            differ = [p for p in sorted(before.parts.keys() | key.parts.keys())
                      if before.parts.get(p) != key.parts.get(p)]
            line += f"; A's was {before.key}; the parts that differ: " + (", ".join(
                f"{p} ({before.parts.get(p, 'absent')} in A, {key.parts.get(p, 'absent')} in {b.which})"
                for p in differ)
                or "none (the key hashes something it does not list as a part)")
        held = [f"{part} holds {' and '.join(where)}: {text[:200]}" for part, text in sorted(key.inputs.items())
                if (where := places.found(text))]
        if held:
            line += "\n    the input that depends on the path: " + "; ".join(held)
        problems.append(line)
    if not problems:
        return ("pass", f"{b.which} compiled nothing: {len(b.keys)} keys, all found", numbers)
    return ("fail", shown(problems), numbers)


# The history.

def rows_of(label: str, stdout: str) -> list[Row]:
    """`blaze debug sql --json`'s answer: an object a row, a line each."""
    rows: list[Row] = []
    for line in stdout.splitlines():
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError as e:
            raise engine.Failed(f"{label}: `blaze debug sql --json` printed {line!r}, not a row: {e}") from None
        if not isinstance(row, dict):
            raise engine.Failed(f"{label}: `blaze debug sql --json` printed {line!r}, not an object")
        rows.append(cast(Row, row))
    return rows


def builds_of(rows: Sequence[Row], ws: Path) -> list[Row]:
    """The `builds` rows of the harness's `blaze build --message-format=json` requests on the
    workspace `ws` (the row's `command` is cargo's command and its arguments,
    crates/blaze-cargo/src/serve/job.rs `command_line`)."""
    real = os.path.realpath(ws)
    found: list[Row] = []
    for row in rows:
        workspace, said = row.get("workspace"), row.get("command")
        # `blaze codegen-inputs` is a `build` request too (crates/blaze-cargo/src/cli/inspect.rs);
        # only the harness's builds ask for JSON messages.
        if (isinstance(workspace, str) and isinstance(said, str) and os.path.realpath(workspace) == real
                and said.split()[:1] == ["build"] and "--message-format=json" in said.split()):
            found.append(row)
    return found


def trees_verdict(a_rows: Sequence[Row], b_rows: Sequence[Row]) -> Verdict:
    if len(a_rows) != 1 or len(b_rows) != 1:
        return ("error", (f"the history has {len(a_rows)} builds of A and {len(b_rows)} of B, not one each: "
                "B's spans and events cannot be told from A's"), {})
    a, b = a_rows[0].get("tree"), b_rows[0].get("tree")
    if not isinstance(a, str) or not isinstance(b, str):
        return ("error", f"the history names no tree digest of A ({a!r}) or B ({b!r})", {})
    if a != b:
        return ("error", f"the copies are not one tree: A is {a}, B is {b}; nothing about keys follows", {})
    return ("pass", f"both builds are of tree {a}", {"tree": a})


# The store, for leads.

def store_leads(store: Path, places: Places) -> dict[str, list[str]]:
    """Each place's name, and the store's files (under `store`) whose bytes hold it in any
    spelling. Cold records are compressed, so a place not found is no proof it is not there."""
    leads: dict[str, list[str]] = {}
    needles = {name: [way.encode() for way in ways] for name, ways in places.named.items()}
    for root, _, files in os.walk(store):
        for name in sorted(files):
            path = Path(root, name)
            try:
                if path.stat().st_size == 0:
                    continue
                with open(path, "rb") as f, mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as data:
                    for place, ways in needles.items():
                        if any(data.find(way) >= 0 for way in ways):
                            leads.setdefault(place, []).append(str(path.relative_to(store)))
            except (OSError, ValueError):
                continue
    return leads


def leads_text(leads: dict[str, list[str]]) -> str:
    if not leads:
        return "leads: no file of the store holds a checkout's or build directory's path as it is"
    return "leads: the store's files that hold a path as it is: " + "; ".join(
        f"{place} in {len(files)} ({', '.join(files[:5])}{', ...' if len(files) > 5 else ''})"
        for place, files in sorted(leads.items()))


# Commands.

async def command(step: str, argv: Sequence[str], cwd: Path, env: dict[str, str], secs: float) -> engine.Ran:
    """`argv` run to its end in `cwd`; one that does not start or outlives `secs` is a
    `Failed` naming the step and the command."""
    shown_argv = " ".join(argv)
    began = time.perf_counter()
    try:
        proc = await asyncio.create_subprocess_exec(*argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    except OSError as e:
        raise engine.Failed(f"{step}: `{shown_argv}` in {cwd} did not start: {e}") from None
    try:
        async with asyncio.timeout(secs):
            out, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise engine.Failed(f"{step}: `{shown_argv}` in {cwd} took more than {secs:.0f} s") from None
    return engine.Ran(tuple(argv), time.perf_counter() - began, proc.returncode or 0, engine.text(out),
                      engine.text(err))


@dataclass(frozen=True)
class Run:
    eng: engine.Engine
    env: dict[str, str]
    args: tuple[str, ...]

    def blaze(self, *words: str) -> list[str]:
        return [str(self.eng.blaze), *words]

    async def build(self, which: str, ws: Path) -> engine.Ran:
        args = engine.dev_backend_args(self.eng.spec.dev_backend, "build", ("--message-format=json", *self.args))
        step = f"{which}: blaze build"
        ran = await command(step, self.blaze("build", *args), ws, self.env, BUILD_SECS)
        if ran.returncode != 0:
            raise Wrong(f"{step}: `{' '.join(ran.argv)}` in {ws} exited {ran.returncode}:\n{ran.said()}")
        return ran

    async def inputs(self, which: str, ws: Path) -> dict[str, str]:
        step = f"{which}: blaze codegen-inputs"
        # The same profile and target as the build, or it would answer of other units.
        ran = await command(step, self.blaze("codegen-inputs", "--format=json", *self.args), ws, self.env,
                            INPUTS_SECS)
        answer = unique_json(step, ran.checked(step).stdout)
        if not isinstance(answer, dict) or not all(isinstance(k, str) and isinstance(v, str)
                                                   for k, v in cast(dict[object, object], answer).items()):
            raise engine.Failed(f"{step}: printed no object of hex strings:\n{ran.said()}")
        return dict(cast(dict[str, str], answer))

    async def stop(self, ws: Path) -> None:
        step = "blaze stop"
        (await command(step, self.blaze("stop"), ws, self.env, STOP_SECS)).checked(step)

    async def sql(self, what: str, query: str, ws: Path) -> list[Row]:
        step = f"the history's {what}"
        ran = await command(step, self.blaze("debug", "sql", "--json", query), ws, self.env, QUERY_SECS)
        return rows_of(step, ran.checked(step).stdout)


def unique_json(step: str, text: str) -> object:
    """`text` as JSON, refusing an object that names one key twice: two code hashes under one
    instance key would otherwise read as the last alone."""

    def pairs(items: list[tuple[str, object]]) -> dict[str, object]:
        out: dict[str, object] = {}
        for key, value in items:
            if key in out:
                raise engine.Failed(f"{step}: key {key} twice, {out[key]!r} and {value!r} (two values under one "
                                    "key, rule 21)")
            out[key] = value
        return out

    try:
        return json.loads(text, object_pairs_hook=pairs)
    except json.JSONDecodeError as e:
        raise engine.Failed(f"{step}: printed no JSON ({e}):\n{text[-3000:]}") from None


class Wrong(Exception):
    """A finding about blaze before anything could be compared: a build that failed."""


def checkouts(work: Path, project: str, third: bool) -> dict[str, Path]:
    """Where each checkout goes: the same last component, other depths and lengths, a space."""
    out = {"A": work / "a" / project, "B": work / "b" / SECOND / project}
    if third:
        out["C"] = work / "c" / "third" / "checkout" / "deeper" / project
    return out


async def prepared(args: Input, work: Path) -> tuple[dict[str, Path], dict[str, str]]:
    """The project fetched once, its dependencies fetched with cargo (so no build needs the
    network) and copied to every checkout at once; each checkout's workspace, and its env."""
    source = work / "source"
    spec, ws = await asyncio.to_thread(budgets.failing, args.project, lambda: budgets.checkout(args.project, source))
    env = budgets.project_env(spec)
    fetch_env = engine.reference_env(work / "fetch-target", env)
    step = f"{args.project}: cargo fetch"
    (await command(step, ["cargo", "fetch"], ws, fetch_env, FETCH_SECS)).checked(step)
    sub = ws.relative_to(source)
    places = checkouts(work, args.project, args.third)

    def copy(to: Path) -> None:
        budgets.failing(f"copying {source} to {to}", lambda: shutil.copytree(source, to, symlinks=True,
                                                                            ignore=budgets.left_out))

    async with asyncio.TaskGroup() as group:
        for to in places.values():
            group.create_task(asyncio.to_thread(copy, to))
    return {which: to / sub for which, to in places.items()}, env


async def measured(args: Input, rep: report.Report, eng: engine.Engine, work: Path) -> None:
    def name(which: str, part: str) -> str:
        return f"{PROPERTY}/{part}" if which == "B" else f"{PROPERTY}/{which}/{part}"

    ws: dict[str, Path] = {}
    project_env: dict[str, str] = {}
    unprepared: list[str] = []
    try:
        ws, project_env = await prepared(args, work)
    except* engine.Failed as e:
        unprepared = [str(each) for each in e.exceptions]
    if unprepared:
        for each in unprepared:
            rep.add(f"{PROPERTY}/setup", "error", each)
        return
    home = work / "home"
    shutil.rmtree(home, ignore_errors=True)
    run = Run(eng, eng.env(home, extra={**project_env, "BLAZE_DUCKDB": args.duckdb}), args.args)
    rep.extra.update(home=str(home), daemon=args.daemon, checkouts={k: str(v) for k, v in ws.items()})
    # Each checkout's build, then its codegen inputs; one after another, as their order is the test.
    ran: dict[str, engine.Ran] = {}
    inputs: dict[str, dict[str, str]] = {}
    running = False
    try:
        for which, at in ws.items():
            running = True
            ran[which] = await run.build(which, at)
            inputs[which] = await run.inputs(which, at)
            if args.daemon == "restarted" or which == list(ws)[-1]:
                await run.stop(at)
                running = False
    except Wrong as e:
        rep.add(f"{PROPERTY}/builds", "fail", str(e))
        return
    except engine.Failed as e:
        rep.add(f"{PROPERTY}/builds", "error", str(e))
        return
    finally:
        if running:
            try:
                await run.stop(ws["A"])
            except engine.Failed as e:
                rep.add(f"{PROPERTY}/stopping the daemon", "error", str(e))
    # Read twice: first for each build directory (the artifacts' file names), then with every
    # place known, so a name written with one checkout's places reads as the other's.
    try:
        bare = Places.of({f"checkout {which}": at for which, at in ws.items()})
        dirs = {which: build_dir_of(parsed(which, ws[which], it, bare), home / "build") for which, it in ran.items()}
        places = Places.of({**{f"checkout {w}": at for w, at in ws.items()},
                            **{f"build dir {w}": at for w, at in dirs.items()}})
        built = {which: parsed(which, ws[which], it, places) for which, it in ran.items()}
    except engine.Failed as e:
        rep.add(f"{PROPERTY}/builds", "error", str(e))
        return
    rep.extra.update(build_dirs={k: str(v) for k, v in dirs.items()},
                     seconds={k: round(v.seconds, 1) for k, v in built.items()},
                     work={k: v.work for k, v in built.items()}, served={k: list(v.served) for k, v in built.items()})
    history: dict[str, list[Row]] = {}
    leads: dict[str, list[str]] = {}
    try:
        async with asyncio.TaskGroup() as group:
            queries = {what: group.create_task(run.sql(what, query, ws["A"]))
                       for what, query in (("builds", BUILDS_QUERY), ("spans", SPANS_QUERY), ("events", EVENTS_QUERY))}
            scanned = group.create_task(asyncio.to_thread(store_leads, engine.store_of(home), places))
        history = {what: task.result() for what, task in queries.items()}
        leads = scanned.result()
    except* engine.Failed as e:
        for each in e.exceptions:
            rep.add(f"{PROPERTY}/history", "error", str(each))
    rep.extra["store_leads"] = leads
    if "builds" in history:
        rep.extra["builds"] = history["builds"]
    lead = leads_text(leads)
    rows = {which: builds_of(history.get("builds", []), at) for which, at in ws.items()}
    ids = {which: {cast(int, r["build_id"]) for r in it if isinstance(r.get("build_id"), int)}
           for which, it in rows.items()}
    a = built["A"]
    for which in list(ws)[1:]:
        b = built[which]

        def add(part: str, verdict: Verdict, which: str = which) -> None:
            state, detail, numbers = verdict
            rep.add(name(which, part), state, detail + (f"\n{lead}" if state == "fail" else ""), **numbers)

        if "builds" in history:
            add("trees", trees_verdict(rows["A"], rows[which]))
        not_kept = ([str(r.get("message")) for r in history.get("events", []) if r.get("build_id") in ids[which]]
                    if ids[which] else [f"(the history has no build of {which}: its events cannot be read)"])
        add("units", units_verdict(a, b, not_kept))
        add("build-script-runs", scripts_verdict(a, b, places))
        if "spans" in history:
            add("proc-macro-expansions", expansions_verdict(which, ids["A"], ids[which], history["spans"]))
        add("instances", instances_verdict(which, inputs["A"], inputs[which]))
        add("function-keys", keys_verdict(a, b, places))


def main(args: Input) -> None:
    if args.engine.kind != "v2":
        fail(SCRIPT, "engine.kind: v2; this reads the v2 daemon's JSON lines and history")
    if not os.path.isabs(args.duckdb) or not os.path.isfile(args.duckdb):
        fail(SCRIPT, f"duckdb: an absolute path to the DuckDB binary, not {args.duckdb!r}")
    if args.project not in budgets.PROJECTS:
        fail(SCRIPT, f"project: no {args.project} in bench.PROJECTS (they are {', '.join(budgets.PROJECTS)})")
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-worktree-keys-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    if any(work.iterdir()):
        fail(SCRIPT, f"work: {work} is not empty; the run fills it and removes it after")
    rep = report.Report(SCRIPT, args.engine, args.out)
    rep.extra.update(work=str(work), project=args.project, args=list(args.args), tree=args.tree,
                     blaze={"path": str(eng.blaze), "blake3": engine.digest(eng.blaze)})
    print(f"work: {work}", flush=True)
    try:
        asyncio.run(measured(args, rep, eng, work))
    finally:
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)
    rep.finish()


# selftest: the parsing and verdicts on made-up output.

def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def artifact(package: str, name: str, fresh: bool, files: Sequence[str] = ()) -> str:
    return json.dumps({"reason": "compiler-artifact", "package_id": package, "manifest_path": "",
                       "target": {"kind": ["lib"], "name": name}, "profile": {"opt_level": "0", "test": False},
                       "filenames": list(files), "fresh": fresh})


def script_line(package: str, out_dir: str, env: Sequence[tuple[str, str]] = (), fresh: bool | None = None) -> str:
    row: Row = {"reason": "build-script-executed", "package_id": package, "linked_libs": [], "linked_paths": [],
                "cfgs": [], "env": [list(it) for it in env], "out_dir": out_dir}
    if fresh is not None:
        row["fresh"] = fresh
    return json.dumps(row)


def work_line(found: int, compiled: int) -> str:
    return json.dumps({"reason": "blaze-work", "reran": 0, "green": 0, "compiled": compiled, "found": found,
                       "link": "placed", "critical_path_us": 0, "links": []})


def key_line(kind: str, name: str, key: str, source: str, parts: dict[str, str],
             inputs: dict[str, str] | None = None) -> str:
    return json.dumps({"reason": "blaze-keys", "kind": kind, "name": name, "key": key, "from": source,
                       "parts": parts, "inputs": inputs or {}})


def made(which: str, lines: Sequence[str], places: Places) -> Built:
    return parsed(which, Path("/w"), engine.Ran(("blaze",), 1.0, 0, "\n".join(["Compiling x", *lines]), ""), places)


def selftest() -> None:
    a_root, b_root = Path("/tmp/w/a/rg"), Path("/tmp/w/b/second checkout/rg")
    da, db = Path("/tmp/w/home/build/rg-aaaa"), Path("/tmp/w/home/build/rg-bbbb")
    places = Places.of({"checkout A": a_root, "checkout B": b_root, "build dir A": da, "build dir B": db})
    expect(places.neutral(f"path+file://{a_root}/crates/core#1") == places.neutral(f"path+file://{b_root}/crates/core#1")
           == "path+file://<checkout>/crates/core#1", "either checkout reads as one")
    expect(places.neutral(f"{db}/debug/build/x") == "<build dir>/debug/build/x", "a build directory reads as one")
    expect(places.found(f"-L {da}/out") == ["build dir A"], "a place is found by name")
    if sys.platform == "darwin":
        expect(places.found(f"/private{a_root}/x") == ["checkout A"], "macOS's /private spelling is found")
    expect(places.found("path+file:///tmp/w/b/second%20checkout/rg#rg@15") == ["checkout B"],
           "a package id's %20 spelling is found")
    expect(places.neutral("path+file:///tmp/w/b/second%20checkout/rg#rg@15") == "path+file://<checkout>#rg@15",
           "a package id's %20 spelling reads as the checkout")

    reg = "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.228"
    a_lines = [artifact(f"path+file://{a_root}#rg@15", "rg", False, [f"{da}/debug/rg"]), artifact(reg, "serde", False),
               script_line(reg, f"{da}/debug/build/serde-1/out"), work_line(0, 3)]
    a = made("A", a_lines, places)
    expect(len(a.units) == 2 and len(a.scripts) == 1 and a.units[0].package == "path+file://<checkout>#rg@15",
           f"A's lines are read: {a.units}")
    expect(build_dir_of(a, Path("/tmp/w/home/build")) == Path(os.path.realpath(da)), "A's build directory")

    b_id = "path+file:///tmp/w/b/second%20checkout/rg#rg@15"
    good = made("B", [artifact(b_id, "rg", True, [f"{db}/debug/rg"]), artifact(reg, "serde", True),
                      script_line(reg, f"{db}/debug/build/serde-1/out"), work_line(3, 0)], places)
    expect(units_verdict(a, good, [])[0] == "pass", f"all found: {units_verdict(a, good, [])}")
    expect(scripts_verdict(a, good, places)[0] == "pass", f"the run found: {scripts_verdict(a, good, places)}")

    bad = made("B", [artifact(f"path+file://{b_root}#rg@15", "rg", False), artifact(reg, "serde", True),
                     script_line(reg, f"{db}/debug/build/serde-1/out", [("SRC", f"{a_root}/x")]), work_line(1, 2)], places)
    verdict, detail, numbers = units_verdict(a, bad, ["unit record not kept: a read outside"])
    expect(verdict == "fail" and "path+file://<checkout>#rg@15 lib `rg`" in detail and "compiled again" in detail
           and "a read outside" in detail and numbers["units_b_compiled"] == 1, detail)
    verdict, detail, numbers = scripts_verdict(a, bad, places)
    expect(verdict == "fail" and "handed checkout A in env SRC" in detail and "compiled 1 units again that print no"
           in detail and numbers["units_without_artifact_compiled"] == 1, detail)
    inert = made("B", [artifact(reg, "serde", True), script_line(reg, f"{db}/o"), work_line(5, 0)], places)
    expect(scripts_verdict(a, inert, places)[0] == "pass", "inert script builds found, with no artifact line, pass")
    odd = made("B", [artifact(reg, "serde", False), script_line(reg, f"{db}/o"), work_line(5, 0)], places)
    expect(scripts_verdict(a, odd, places)[0] == "error", "fewer units compiled than artifacts is an error")
    named = made("B", [artifact(reg, "serde", True), script_line(reg, f"{db}/o", fresh=False), work_line(1, 1)], places)
    verdict, detail, _ = scripts_verdict(a, named, places)
    expect(verdict == "fail" and f"B ran {reg}'s build script again" in detail, detail)
    missing = made("B", [artifact(reg, "serde", True), work_line(1, 0)], places)
    expect(f"A ran {reg}'s build script and B printed no run of it" in scripts_verdict(a, missing, places)[1],
           "a run B did not print is named")
    elsewhere = made("B", [script_line(reg, "/somewhere/out", fresh=True), work_line(1, 0)], places)
    expect("outside its build directory" in scripts_verdict(a, elsewhere, places)[1], "an OUT_DIR elsewhere")

    rows: list[Row] = [{"build_id": 1, "name": "macro expand", "krate": "grep_printer", "function": "serde::Serialize",
                        "module_from": None, "n": 12},
                       {"build_id": 2, "name": "macro module", "krate": None, "function": "serde_derive",
                        "module_from": "memo", "n": 1}]
    expect(expansions_verdict("B", {1}, {2}, rows)[0] == "pass", "a module from memo and no expansion pass")
    more: list[Row] = [{"build_id": 2, "name": "macro expand", "krate": "grep_printer", "function": "serde::Serialize",
                        "module_from": None, "n": 3},
                       {"build_id": 2, "name": "macro module", "krate": None, "function": "serde_derive",
                        "module_from": "compiled", "n": 1}]
    rows += more
    verdict, detail, numbers = expansions_verdict("B", {1}, {2}, rows)
    expect(verdict == "fail" and "crate `grep_printer`: `serde::Serialize` expanded again (3 times)" in detail
           and "macro module `serde_derive` compiled again" in detail and numbers["expansions_b_ran"] == 3, detail)
    expect(expansions_verdict("B", {7}, {2}, rows)[0] == "error", "no expansion of A's is an error")
    expect(expansions_verdict("B", {1}, set(), rows)[0] == "error", "no build of B in the history is an error")

    expect(instances_verdict("B", {"k1": "c1"}, {"k1": "c1"})[0] == "pass", "the same instances pass")
    verdict, detail, numbers = instances_verdict("B", {"k1": "c1", "k2": "c2"}, {"k1": "c9", "k3": "c2"})
    expect(verdict == "fail" and numbers["only_a"] == 1 and numbers["only_b"] == 1 and numbers["clashing"] == 1
           and "only B: k3" in detail and "rule 21" in detail, detail)

    expect(keys_verdict(a, good, places) == ("error", KEYS_MISSING, {}), "no keys line is an error naming it")
    ka = made("A", [work_line(0, 1), key_line("function", "rg::search::<u8>", "f0", "compiled",
                                              {"code": "c", "args": "x", "roots": "r1"})], places)
    kb = made("B", [work_line(0, 1),
                    key_line("function", "rg::search::<u8>", "f1", "compiled", {"code": "c", "args": "x", "roots": "r2"},
                             {"roots": f"package {b_root}"}),
                    key_line("unit", "rg v15 (lib)", "u0", "store", {})], places)
    verdict, detail, numbers = keys_verdict(ka, kb, places)
    expect(verdict == "fail" and "function rg::search::<u8>: compiled again under f1" in detail
           and "the parts that differ: roots (r1 in A, r2 in B)" in detail
           and "roots holds checkout B" in detail and numbers == {"function_compiled": 1, "unit_store": 1}, detail)
    same = made("B", [work_line(0, 1), key_line("function", "rg::search::<u8>", "f0", "compiled", {})], places)
    expect("A had the same key" in keys_verdict(ka, same, places)[1], "a same key that missed is told apart")

    one: list[Row] = [{"build_id": 1, "workspace": str(a_root), "command": "build --message-format=json", "tree": "t"}]
    two: list[Row] = [{"build_id": 2, "workspace": str(b_root), "command": "build --message-format=json", "tree": "t"}]
    other: Row = {"build_id": 3, "workspace": str(a_root), "command": "codegen-inputs"}
    inputs: Row = {"build_id": 4, "workspace": str(a_root), "command": "build --release", "tree": "t"}
    expect(builds_of([*one, *two, other, inputs], a_root) == one,
           "only A's builds are A's, and codegen-inputs' build requests are not")
    expect(trees_verdict(one, two)[0] == "pass", "one tree")
    moved: Row = {**two[0], "tree": "u"}
    expect(trees_verdict(one, [moved])[0] == "error", "two trees are an error")
    expect(rows_of("q", '{"a": 1}\n\n{"a": 2}\n') == [{"a": 1}, {"a": 2}], "rows are read a line each")

    with tempfile.TemporaryDirectory() as tmp:
        store = Path(tmp)
        (store / "seg").write_bytes(b"\0\0" + str(a_root).encode() + b"/src\0")
        (store / "empty").write_bytes(b"")
        expect(store_leads(store, places) == {"checkout A": ["seg"]}, "a path in the store is a lead")
    expect(unique_json("q", '{"k1": "c1", "k2": "c2"}') == {"k1": "c1", "k2": "c2"}, "unique keys are read")
    try:
        unique_json("q", '{"k1": "c1", "k1": "c2"}')
        expect(False, "a key named twice is refused")
    except engine.Failed as e:
        expect("key k1 twice" in str(e), str(e))
    try:
        parse(Input, '{"engine": {"kind": "v2", "blaze": "b"}, "duckdb": "/d", "out": "", "daemon": "other"}', SCRIPT)
        expect(False, "an unknown daemon mode is refused")
    except SystemExit as e:
        expect("daemon" in str(e.code), str(e.code))
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
