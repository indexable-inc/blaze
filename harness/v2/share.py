"""share.py: the programs of `crates/hir-ty-share/programs/` built by blaze, each program's
stdout held to the `<name>.stdout` beside it
(`fastcompile-ident-share 2` in v2/requests/fastcompile.md; docs/v2/plan/lanes/fastcompile/share.md).

    uv run --frozen python harness/v2/share.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/share.json"}'
    uv run --frozen python harness/v2/share.py '{"engine": {...}, "out": "...", "programs": ["type_id", "statics"]}'
    uv run --frozen python harness/v2/share.py selftest

These are where sharing one instance's code between type arguments (idea 2) would show if it
were wrong: `TypeId`, `type_name`, `needs_drop`, statics in generic functions, associated
consts, const generics, drop order, `dyn` vtables and function pointers. The expected outputs
were written from what Rust guarantees.

The input is one JSON object (inline, a file's path, or `-` for stdin):

    engine    the blaze under test (engine.EngineSpec)
    out       the path the result JSON is written to (report.py)
    work      the directory everything is built in; empty: a new one under /tmp
    dir       the programs' directory ("crates/hir-ty-share/programs", from the repository)
    programs  which programs run, by name; empty: all of them
    jobs      how many programs build at once (4); blaze's side is one daemon for all
    keep      keep the work directory after the run

The cases are the directory's: every `<name>.rs` with a `<name>.stdout` beside it. A `.rs`
without one is an error naming it, not a program left out.

blaze builds each program as a one-file package (`src/main.rs`) in a workspace of its own,
a binary in the dev profile at edition 2024, as corpus.py's steps are built, and it is run
from an empty directory. One outcome per program:

    pass   it exits 0 and its stdout is `<name>.stdout`
    fail   its stdout or exit is not that (named with both texts), or blaze does not build
           it, or the engine says the build was not blaze's
    error  a command timed out, or the harness could not do a step
"""

import concurrent.futures
import shutil
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse  # noqa: E402

import engine  # noqa: E402
import report  # noqa: E402

SCRIPT = "share.py"
PROGRAMS = "crates/hir-ty-share/programs"
EDITION = "2024"
BUILD_SECS = 1800
RUN_SECS = 120


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    dir: str = PROGRAMS
    programs: tuple[str, ...] = ()
    jobs: int = 4
    keep: bool = False


@dataclass(frozen=True)
class Case:
    """A program and the stdout it has to print."""

    name: str
    source: Path
    expected: str


def cases(directory: Path) -> tuple[list[Case], list[str]]:
    """The cases of `directory` in order of name, and each `.rs` in it that has no `.stdout`."""
    found: list[Case] = []
    lone: list[str] = []
    for source in sorted(directory.glob("*.rs")):
        expected = source.with_suffix(".stdout")
        if expected.is_file():
            found.append(Case(source.stem, source, lines(expected.read_text())))
        else:
            lone.append(source.name)
    return found, lone


def lines(text: str) -> str:
    """`text` with Windows line ends as Unix's, for comparing what a program printed."""
    return text.replace("\r\n", "\n")


def indented(text: str) -> str:
    return "\n".join("    " + line for line in text.splitlines()) or "    (nothing)"


def against(case: Case, who: str, got: str) -> str:
    """`who`'s stdout of `case` beside the one expected, with the first line that differs."""
    want, have = case.expected.splitlines(), lines(got).splitlines()
    at = next((i for i, (a, b) in enumerate(zip(want, have)) if a != b), min(len(want), len(have)))
    return (f"{who}'s stdout is not {case.name}.stdout (first difference at line {at + 1}):\n"
            f"  expected:\n{indented(case.expected)}\n  {who} printed:\n{indented(lines(got))}")


def judge(case: Case, blaze_build: engine.Ran, blaze_run: engine.Ran | None,
          who: str | None) -> tuple[report.Verdict, str]:
    """The verdict of `case` from what blaze's build and run gave (the run is None when the
    build failed), and who built it when blaze did not (`Engine.not_blazes`)."""
    problems: list[str] = []
    if who is not None:
        problems.append(f"blaze's build of {case.name} is not blaze's: {who}")
    elif blaze_build.returncode != 0 or blaze_run is None:
        problems.append(f"blaze does not build {case.name} (exit {blaze_build.returncode}):\n{blaze_build.said(1500)}")
    else:
        if blaze_run.returncode != 0:
            problems.append(f"blaze's {case.name} exits {blaze_run.returncode}:\n{blaze_run.said(1500)}")
        if lines(blaze_run.stdout) != case.expected:
            problems.append(against(case, "blaze", blaze_run.stdout))
    if problems:
        return "fail", f"{case.name}: " + "\n".join(problems)
    return "pass", ""


def manifest(name: str) -> str:
    """The manifest of the one-file package of program `name`, a workspace of its own."""
    return (f'[package]\nname = "{name}"\nversion = "0.0.0"\nedition = "{EDITION}"\npublish = false\n\n'
            f'[[bin]]\nname = "{name}"\npath = "src/main.rs"\n\n[workspace]\n')


def run_case(eng: engine.Engine, case: Case, root: Path, home: Path, store: Path) -> report.Outcome:
    """`case` built and run by blaze under `root`. Never raises for what the harness
    could not do: that is an outcome too."""
    label = f"share/{case.name}"
    try:
        ws, cwd = root / "ws", root / "cwd"
        shutil.rmtree(root, ignore_errors=True)
        (ws / "src").mkdir(parents=True)
        cwd.mkdir()
        (ws / "Cargo.toml").write_text(manifest(case.name))
        shutil.copyfile(case.source, ws / "src" / "main.rs")
        plain = engine.scrubbed()
        env = eng.env(home, store)
        blaze_build = eng.build(f"{label}: blaze build", ws, env, timeout=BUILD_SECS)
        who = eng.not_blazes(blaze_build)
        built = ws / "target" / "debug" / (case.name + engine.EXE)
        blaze_run = (engine.run(f"{label}: blaze's program", [str(built)], cwd, plain, RUN_SECS)
                     if blaze_build.returncode == 0 and who is None else None)
    except engine.Unmeasurable as e:
        return report.Outcome(label, "unmeasured", str(e))
    except engine.Failed as e:
        return report.Outcome(label, "error", str(e))
    except OSError as e:
        return report.Outcome(label, "error", f"{type(e).__name__}: {e}")
    verdict, detail = judge(case, blaze_build, blaze_run, who)
    numbers: dict[str, report.Number] = {"blaze_s": round(blaze_build.seconds, 2)}
    return report.Outcome(label, verdict, detail, numbers)


def chosen(args: Input, found: list[Case]) -> list[Case]:
    """The cases `programs` names, or all; a name with no case is refused."""
    if unknown := sorted(set(args.programs) - {c.name for c in found}):
        fail(SCRIPT, f"programs: {args.dir} has no {', '.join(unknown)} (with a .stdout)")
    return [c for c in found if not args.programs or c.name in args.programs]


def main(args: Input) -> None:
    if args.jobs < 1:
        fail(SCRIPT, f"jobs: at least 1, not {args.jobs}")
    directory = Path(args.dir) if Path(args.dir).is_absolute() else engine.ROOT / args.dir
    if not directory.is_dir():
        fail(SCRIPT, f"dir: no directory {directory}")
    found, lone = cases(directory)
    picked = chosen(args, found)
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-share-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    rep = report.Report(SCRIPT, args.engine, args.out)
    rep.extra.update(work=str(work), dir=str(directory), programs=[c.name for c in picked],
                     toolchain=engine.TOOLCHAIN)
    print(f"work: {work}", flush=True)
    for name in lone:
        rep.add(f"share/{name}", "error", f"{directory / name} has no {Path(name).stem}.stdout beside it: "
                "every program of the directory is a case")
    home, store = work / "h", work / "store"
    try:
        with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
            running = [pool.submit(run_case, eng, c, work / c.name, home, store) for c in picked]
            # In the order of the programs, not the order they end in: one report for one input.
            for done in running:
                outcome = done.result()
                rep.add(outcome.name, outcome.verdict, outcome.detail, **outcome.numbers)
    finally:
        try:
            if home.is_dir():
                eng.stop(work, eng.env(home, store))
        except engine.Failed as e:
            rep.add("share: stopping the daemon", "error", str(e))
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)
    rep.finish()


def ran(returncode: int, stdout: str = "", stderr: str = "") -> engine.Ran:
    return engine.Ran(("x",), 0.5, returncode, stdout, stderr)


CASE = Case("type_id", Path("type_id.rs"), "false\ntrue\ntrue\nfalse\n")


def test_cases() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        top = Path(tmp)
        for name, body in (("b.rs", "fn main() {}\n"), ("b.stdout", "x\n"), ("a.rs", ""), ("a.stdout", ""),
                           ("lone.rs", ""), ("orphan.stdout", "")):
            (top / name).write_text(body)
        found, lone = cases(top)
        assert [(c.name, c.expected) for c in found] == [("a", ""), ("b", "x\n")], found
        assert lone == ["lone.rs"], lone
    found, lone = cases(engine.ROOT / PROGRAMS)
    names = [c.name for c in found]
    assert not lone and len(names) >= 9 and {"type_id", "statics", "drop_order"} <= set(names), (names, lone)


def test_judge() -> None:
    good = ran(0, CASE.expected)
    assert judge(CASE, ran(0), good, None) == ("pass", "")
    assert judge(CASE, ran(0), ran(0, CASE.expected.replace("\n", "\r\n")), None) == ("pass", "")
    verdict, detail = judge(CASE, ran(0), ran(0, "false\ntrue\ntrue\n"), None)
    assert verdict == "fail" and "blaze's stdout is not type_id.stdout" in detail and "line 4" in detail, detail
    assert "expected:\n    false" in detail and "blaze printed:\n    false" in detail, detail
    verdict, detail = judge(CASE, ran(101, "", "error: x"), None, None)
    assert verdict == "fail" and "blaze does not build type_id (exit 101)" in detail and "error: x" in detail, detail
    verdict, detail = judge(CASE, ran(0), ran(101, CASE.expected, "panicked"), None)
    assert verdict == "fail" and "blaze's type_id exits 101" in detail, detail
    verdict, detail = judge(CASE, ran(0), None, "cargo: no daemon")
    assert verdict == "fail" and "not blaze's: cargo: no daemon" in detail, detail
    # A wrong exit and a wrong stdout: both are said.
    verdict, detail = judge(CASE, ran(0), ran(1, "y\n"), None)
    assert verdict == "fail" and "exits 1" in detail and "blaze's stdout" in detail, detail


def test_package() -> None:
    text = manifest("drop_order")
    assert '[package]\nname = "drop_order"' in text and 'edition = "2024"' in text and text.endswith("[workspace]\n")


def test_input() -> None:
    args = parse(Input, '{"engine": {"kind": "v2", "blaze": "b"}, "out": "", "programs": ["type_id"]}', SCRIPT)
    assert args.programs == ("type_id",) and args.dir == PROGRAMS and args.jobs == 4
    assert [c.name for c in chosen(args, [CASE, Case("statics", Path("s.rs"), "")])] == ["type_id"]
    try:
        chosen(Input(args.engine, "", programs=("nope",)), [CASE])
    except SystemExit as e:
        assert "nope" in str(e.code), e.code
    else:
        raise AssertionError("an unknown program was taken")
    try:
        parse(Input, '{"engine": {"kind": "v2", "blaze": "b"}, "out": "", "program": []}', SCRIPT)
    except SystemExit as e:
        assert "no field program" in str(e.code), e.code
    else:
        raise AssertionError("an unknown field was taken")


SELFTESTS = (test_cases, test_judge, test_package, test_input)


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
