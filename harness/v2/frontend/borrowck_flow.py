"""borrowck_flow.py: `BLAZE_BORROWCK=polonius-flow` accepts more than the default checker and
never less, on rustc's polonius UI tests (docs/v2/plan/design/borrowck-cursor.md).

    uv run --frozen python harness/v2/frontend/borrowck_flow.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/borrowck-flow.json"}'
    uv run --frozen python harness/v2/frontend/borrowck_flow.py selftest

The tests are the 46 of docs/upstream/borrowck/rust-lang-rust/ui-polonius-revisions-table.md,
vendored verbatim in its `tests-ui/` (SOURCE.md there). Each is made a cases.py case (`//@ expect:
accept`, `#![crate_type = "lib"]` so nothing is run, its `//@` lines turned into plain comments
but the edition of its Polonius-alpha revision) and checked by cases.py's runner twice, once with
`BLAZE_BORROWCK=polonius` (the default) and once with `BLAZE_BORROWCK=polonius-flow`, the two at
once in work directories of their own. A test:

    pass   both modes say the same; or polonius-flow accepts what the default rejects and an
           upstream revision accepts it too
    fail   the default accepts it and polonius-flow does not (it must only ever accept more);
           polonius-flow accepts what every upstream revision rejects; one of TARGETS (rejected
           by the alpha, accepted by legacy Polonius) is not accepted by polonius-flow; or a test
           every upstream revision rejects is accepted by polonius-flow
    error  either mode's check could not be read (a timeout, an error of the harness)

The input is one JSON object (inline, a file's path, or `-` for stdin):

    engine    the blaze under test (engine.EngineSpec; v2)
    out       the path the result JSON is written to (report.py)
    work      the directory everything is built in; empty: a new one under /tmp
    suite     the vendored tests ("": docs/upstream/borrowck/rust-lang-rust/tests-ui)
    table     their verdicts upstream ("": ui-polonius-revisions-table.md beside `suite`)
    filter    only the tests whose upstream path has this in it
    batch     how many tests one workspace has (8)
    jobs      how many workspaces of one mode at once (4)
    keep      keep every workspace
"""

import asyncio
import re
import shutil
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent), str(HERE.parent.parent)]
from typed_input import fail, parse  # noqa: E402
import cases, engine, report  # noqa: E402

SCRIPT = "borrowck_flow.py"
ROOT = HERE.parent.parent.parent
SUITE = ROOT / "docs" / "upstream" / "borrowck" / "rust-lang-rust" / "tests-ui"
TABLE_NAME = "ui-polonius-revisions-table.md"
AREA = "borrowck"
MODES = ("polonius", "polonius-flow")
# fed1eb428d:docs/research/borrowck.md section 2.5: rejected by the alpha, accepted by legacy Polonius.
TARGETS = frozenset({
    "tests/ui/nll/polonius/iterating-updating-cursor-issue-57165.rs",
    "tests/ui/nll/polonius/iterating-updating-cursor-issue-63908.rs",
    "tests/ui/nll/polonius/iterating-updating-cursor-issue-108704.rs",
    "tests/ui/nll/polonius/flow-sensitive-invariance.rs",
    "tests/ui/nll/issue-46589.rs",
})
ROW = re.compile(r"^\| `(tests/ui/[^`]+\.rs)` \| (.*) \|$", re.M)
REVISION = re.compile(r"^(?P<name>[\w-]+) \((?P<mode>nll|next|legacy|default)\) = (?P<verdict>pass|error)")
EDITION = re.compile(r"^\s*//@\s*(?:\[(?P<revs>[\w, -]+)\]\s*)?edition\s*:\s*(?P<year>\d{4})\s*$", re.M)
DIRECTIVE_LINE = re.compile(r"^(\s*)//@", re.M)

Said = Literal["accepted", "rejected", "unknown"]
UpstreamMode = Literal["nll", "next", "legacy", "default"]
UPSTREAM_MODES: dict[str, UpstreamMode] = {"nll": "nll", "next": "next", "legacy": "legacy", "default": "default"}


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    suite: str = ""
    table: str = ""
    filter: str = ""
    batch: int = 8
    jobs: int = 4
    keep: bool = False


@dataclass(frozen=True)
class Revision:
    name: str
    mode: UpstreamMode
    accepts: bool


@dataclass(frozen=True)
class Upstream:
    """A test of the table: its path under the rust repository and its revisions' verdicts."""

    path: str
    revisions: tuple[Revision, ...]

    @property
    def file(self) -> str:
        """Its file name in the vendored suite (SOURCE.md)."""
        return self.path.removeprefix("tests/ui/").replace("/", "_")

    def accepts(self, mode: UpstreamMode) -> bool | None:
        got = [r.accepts for r in self.revisions if r.mode == mode]
        return None if not got else all(got)

    @property
    def alpha(self) -> Revision | None:
        """The revision that is Polonius alpha: `-Zpolonius=next`, else no flag (which is the
        alpha on the pinned nightly)."""
        for mode in ("next", "default"):
            for r in self.revisions:
                if r.mode == mode:
                    return r
        return None

    @property
    def any_accepts(self) -> bool:
        return any(r.accepts for r in self.revisions)


def table_of(text: str) -> list[Upstream] | str:
    """The table's tests, or what in it cannot be read."""
    found: list[Upstream] = []
    for m in ROW.finditer(text):
        revisions: list[Revision] = []
        for part in m[2].split(", "):
            got = REVISION.match(part.strip())
            if got is None:
                return f"{m[1]}: cannot read the revision `{part}`"
            revisions.append(Revision(got["name"], UPSTREAM_MODES[got["mode"]], got["verdict"] == "pass"))
        found.append(Upstream(m[1], tuple(revisions)))
    if not found:
        return "no rows"
    return found


def edition_for(text: str, revision: str | None) -> str:
    """The edition the revision `revision` is checked in: its own `//@ [rev] edition:`, else the
    file's `//@ edition:`, else 2015."""
    base, own = "2015", None
    for m in EDITION.finditer(text):
        revs = m["revs"]
        if revs is None:
            base = m["year"]
        elif revision is not None and revision in {r.strip() for r in revs.split(",")}:
            own = m["year"]
    return own or base


def as_case(text: str, edition: str) -> str:
    """The test as a cases.py case: accepted, a library, in `edition`, with no other directive."""
    body = DIRECTIVE_LINE.sub(r"\1// @", text)
    header = f"//@ expect: accept\n//@ edition: {edition}\n"
    if not cases.CRATE_TYPE.search(body):
        header += '#![crate_type = "lib"]\n'
    return header + body


class ModeV2(engine.V2):
    """blaze v2 with `BLAZE_BORROWCK` set for every command, and so for each daemon it starts."""

    def __init__(self, spec: engine.EngineSpec, mode: str):
        super().__init__(spec)
        self.mode = mode

    def env(self, home: Path, store: Path | None = None, target: Path | None = None,
            extra: dict[str, str] | None = None) -> dict[str, str]:
        return super().env(home, store, target, {**(extra or {}), "BLAZE_BORROWCK": self.mode})


def said_of(judged: cases.Judged) -> Said:
    if judged.category == "as expected":
        return "accepted"
    if judged.category == "rejected what the case accepts":
        return "rejected"
    return "unknown"


def verdict_of(test: Upstream, alpha: cases.Judged, flow: cases.Judged) -> tuple[report.Verdict, str]:
    """The test's outcome from what the default checker and polonius-flow said of it."""
    a, f = said_of(alpha), said_of(flow)
    upstream = ", ".join(f"{r.name} ({r.mode}) {'accepts' if r.accepts else 'rejects'}" for r in test.revisions)
    shown = f"polonius {a}, polonius-flow {f}; upstream {upstream}"
    if "unknown" in (a, f):
        return "error", f"{shown}; polonius: {alpha.detail}; polonius-flow: {flow.detail}"
    if a == "accepted" and f == "rejected":
        return "fail", f"{shown}: polonius-flow rejects what the default accepts ({flow.detail})"
    if f == "accepted" and not test.any_accepts:
        why = "the default accepts it too" if a == "accepted" else "polonius-flow accepts it alone"
        return "fail", f"{shown}: every upstream revision rejects it and {why}"
    if test.path in TARGETS and f != "accepted":
        return "fail", f"{shown}: legacy Polonius accepts it and polonius-flow does not ({flow.detail})"
    if a == "rejected" and f == "accepted":
        return "pass", f"{shown}: newly accepted"
    return "pass", shown


def judge_mode(eng: engine.Engine, work: Path, made: list[cases.Case], args: Input) -> list[cases.Judged]:
    runner = cases.Runner(eng, work, args.jobs, args.batch, keep=args.keep)
    found = runner.judge(made)
    for ws, why in runner.stopped:
        raise engine.Failed(f"stopping the daemon of {ws}: {why}")
    return found


async def judge_both(spec: engine.EngineSpec, work: Path, made: list[cases.Case], args: Input) -> dict[str, list[cases.Judged]]:
    engines = {mode: ModeV2(spec, mode) for mode in MODES}
    async with asyncio.TaskGroup() as group:
        tasks = {mode: group.create_task(asyncio.to_thread(judge_mode, engines[mode], work / mode, made, args))
                 for mode in MODES}
    return {mode: task.result() for mode, task in tasks.items()}


def main(args: Input) -> None:
    if args.engine.kind != "v2":
        fail(SCRIPT, f"engine.kind: v2 (BLAZE_BORROWCK is v2's), not {args.engine.kind!r}")
    for name, value in (("jobs", args.jobs), ("batch", args.batch)):
        if value < 1:
            fail(SCRIPT, f"{name}: at least 1, not {value}")
    suite = Path(args.suite).resolve() if args.suite else SUITE
    table_path = Path(args.table).resolve() if args.table else suite.parent / TABLE_NAME
    if not suite.is_dir():
        fail(SCRIPT, f"suite: no directory {suite}")
    if not table_path.is_file():
        fail(SCRIPT, f"table: no file {table_path}")
    tests = table_of(table_path.read_text())
    if isinstance(tests, str):
        fail(SCRIPT, f"table {table_path}: {tests}")
    if missing := sorted(TARGETS - {t.path for t in tests}):
        fail(SCRIPT, f"table {table_path} lacks the targets {missing}")
    tests = [t for t in tests if args.filter in t.path]
    if not tests:
        fail(SCRIPT, f"no test of {table_path} has {args.filter!r} in its path")
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-borrowck-flow-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    print(f"work: {work}", flush=True)
    rep = report.Report(SCRIPT, args.engine, args.out)
    root = work / "cases" / AREA
    shutil.rmtree(root, ignore_errors=True)
    root.mkdir(parents=True)
    made: list[cases.Case] = []
    by_name: dict[str, Upstream] = {}
    for test in tests:
        source = suite / test.file
        if not source.is_file():
            rep.add(test.path, "error", f"not vendored: no {source}")
            continue
        alpha = test.alpha
        text = source.read_text()
        path = root / test.file
        path.write_text(as_case(text, edition_for(text, alpha.name if alpha else None)))
        name = f"{AREA}/{test.file}"
        case = cases.case_of(name, path, path.read_text())
        if isinstance(case, str):
            rep.add(test.path, "error", f"not a cases.py case: {case}")
            continue
        made.append(case)
        by_name[name] = test
    try:
        spec = args.engine
        found = asyncio.run(judge_both(spec, work, made, args))
    except* engine.Failed as group:
        fail(SCRIPT, "; ".join(str(e) for e in group.exceptions))
    newly: list[str] = []
    for alpha, flow in zip(found["polonius"], found["polonius-flow"], strict=True):
        test = by_name[alpha.case.name]
        verdict, detail = verdict_of(test, alpha, flow)
        if said_of(alpha) == "rejected" and said_of(flow) == "accepted":
            newly.append(test.path)
        rep.add(test.path, verdict, detail)
    rep.extra.update(work=str(work), suite=str(suite), table=str(table_path), tests=len(tests),
                     newly_accepted=newly, targets=sorted(TARGETS))
    if not args.keep:
        for mode in MODES:
            shutil.rmtree(work / mode / "store", ignore_errors=True)
            shutil.rmtree(work / mode / "h", ignore_errors=True)
    rep.finish()


def judged(name: str, category: cases.Category, detail: str = "") -> cases.Judged:
    case = cases.Case(name, name, "lib", "2021", cases.Expect(True))
    return cases.Judged(case, "pass" if category == "as expected" else "fail", category, detail)


def test_table() -> None:
    text = ("| test | revisions |\n|---|---|\n"
            "| `tests/ui/nll/issue-46589.rs` | nll (nll) = error [known-bug], polonius (next) = error [known-bug], "
            "legacy (legacy) = pass |\n"
            "| `tests/ui/nll/polonius/x.rs` | nll (default) = error, polonius (next) = error |\n")
    got = table_of(text)
    assert not isinstance(got, str), got
    assert [t.path for t in got] == ["tests/ui/nll/issue-46589.rs", "tests/ui/nll/polonius/x.rs"]
    assert got[0].accepts("legacy") is True and got[0].accepts("next") is False
    assert got[0].alpha == Revision("polonius", "next", False)
    assert got[1].accepts("legacy") is None and not got[1].any_accepts
    assert got[0].file == "nll_issue-46589.rs"
    assert isinstance(table_of("| `tests/ui/a.rs` | a (b) = pass |\n"), str)


def test_case() -> None:
    text = ("//@ revisions: e15 alpha\n//@ [e15] edition: 2015\n//@ [alpha] edition: 2024\n"
            "//@ [alpha] compile-flags: -Zpolonius=next\n//@ check-pass\nfn main() {}\n")
    assert edition_for(text, "alpha") == "2024" and edition_for(text, None) == "2015"
    assert edition_for("//@ edition: 2021\n", "alpha") == "2021"
    made = as_case(text, "2024")
    assert made.startswith('//@ expect: accept\n//@ edition: 2024\n#![crate_type = "lib"]\n// @ revisions'), made
    assert cases.directives(made) == [("expect", "accept"), ("edition", "2024")]
    kept = as_case('#![crate_type = "lib"]\n', "2021")
    assert kept.count("crate_type") == 1


def test_verdicts() -> None:
    target = Upstream("tests/ui/nll/issue-46589.rs", (Revision("polonius", "next", False), Revision("legacy", "legacy", True)))
    rejected = Upstream("tests/ui/nll/get_default.rs", (Revision("polonius", "next", False), Revision("legacy", "legacy", False)))
    a, r = judged("a", "as expected"), judged("r", "rejected what the case accepts", "blaze gives E0499")
    unknown = judged("u", "error", "timed out")
    assert verdict_of(target, r, a)[0] == "pass"
    assert verdict_of(target, r, r)[0] == "fail"
    assert verdict_of(rejected, a, r)[0] == "fail"
    assert verdict_of(rejected, r, a)[0] == "fail"
    assert verdict_of(rejected, a, a)[0] == "fail"
    assert verdict_of(rejected, r, r)[0] == "pass"
    assert verdict_of(rejected, r, unknown)[0] == "error"


SELFTESTS = (test_table, test_case, test_verdicts)


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
