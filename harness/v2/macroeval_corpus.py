"""macroeval_corpus.py: blaze-macro-eval's corpus of real proc macros, each call expanded by
blaze's evaluator and held to what the corpus says of it: the fixture crate's own tests of what
each expansion does, built by blaze, the corpus's checks of every call, and kept expansions
found again only when every input matches (`fastcompile-macros 6` in v2/requests/fastcompile.md; the tests are
crates/blaze-macro-eval/tests/corpus.rs and kept.rs, and the fixture's
tests/corpus/ws/calls/src/tests.rs). Nothing is compared with rustc's build (docs/harness.md).

    uv run --frozen python harness/v2/macroeval_corpus.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/macroeval.json"}'
    uv run --frozen python harness/v2/macroeval_corpus.py selftest

The input is one JSON object (inline, a file's path, or `-` for stdin):

    engine    the blaze under test (engine.EngineSpec)
    out       the path the result JSON is written to (report.py)
    work      the directory everything is built in; empty: a new one under /tmp
    target    the target directory of `blaze test` of this repository; empty: `WORK/target`
    keep      keep the work directory after the run

In order, each step one outcome, the run stopping at the first that does not pass:

    copy        `crates/blaze-macro-eval/tests/corpus/ws` copied as `WORK/workspace`,
                `WORK/scratch` (which kept.rs changes) and `WORK/fixture` (its own tests)
    fetch       `cargo fetch --locked` of its registry packages into `WORK/cargo-home`, a
                CARGO_HOME of its own; every package of its Cargo.lock has to be unpacked there
    build ...   `blaze build -p corpus-calls` in each of `workspace` and `scratch` with that
                CARGO_HOME, one daemon for both; each build script's run (std's among them)
                has to be left as `output` beside its `OUT_DIR`, where load.rs reads it
    fixture     `blaze test -p corpus-calls` in `fixture`, same daemon: one outcome per test of
                the fixture crate (calls/src/tests.rs, and `tokio_test`, a test tokio's
                `#[test]` makes), each built with the expansions blaze made
    test        the `BLAZE_MACRO_CORPUS` config (crates/blaze-macro-eval/tests/corpus/config.rs:
                `workspace`, `scratch`, `cargo_home`) written to
                `WORK/corpus.json`, then `blaze test -p blaze-macro-eval --test corpus --test
                kept -- --ignored` in this repository, BLAZE_PROC_MACRO_CACHE unset; then
                one outcome per test of the two files, by libtest's name

Every tool, fetch or build missing is an error naming it: nothing is skipped. cargo runs only to
fetch the registry packages.

A step's verdict: pass; fail for a finding about blaze (its build fails, a run is not left
where the plan names it, a test fails, is ignored, does not run, or was kept from an earlier
run rather than run); error for what the harness could not do (a copy, a fetch, a command
that timed out). `selftest` checks the parts that need no build: the input, the config against
config.rs's fields, reading the lock, and reading what libtest printed.
"""

import json
import re
import shutil
import sys
import tempfile
import time
import tomllib
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse  # noqa: E402

import engine  # noqa: E402
import report  # noqa: E402

SCRIPT = "macroeval_corpus.py"
TESTS = engine.ROOT / "crates" / "blaze-macro-eval" / "tests"
FIXTURE = TESTS / "corpus" / "ws"
FIXTURE_TESTS = FIXTURE / "calls" / "src" / "tests.rs"
# The fixture's tests outside tests.rs: `tokio_macros::test` makes `tokio_test` one.
FIXTURE_OWN = ("tokio_test",)
CONFIG_RS = TESTS / "corpus" / "config.rs"
TEST_FILES = ("corpus", "kept")
PACKAGE = "corpus-calls"
COPIES = ("workspace", "scratch")
TEST_ARGS = ("-p", "blaze-macro-eval", *(w for t in TEST_FILES for w in ("--test", t)), "--", "--ignored")
UNSET = ("BLAZE_PROC_MACRO_CACHE",)
FETCH_SECS = 1800
BUILD_SECS = 3600
TEST_SECS = 7200
LABEL = "macroeval"


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    target: str = ""
    keep: bool = False


class Stop(Exception):
    """A step did not pass: its outcome is in the report, and nothing after it can run."""


def config_fields(source: str) -> tuple[list[str], bool]:
    """The fields of config.rs's `Corpus`, in order, and whether it refuses unknown ones."""
    found = re.search(r"((?:#\[[^\n]*\]\n)*)pub\(crate\) struct Corpus \{\n(.*?)\n\}", source, re.S)
    if found is None:
        raise engine.Failed(f"{CONFIG_RS} has no `pub(crate) struct Corpus`")
    attrs, body = found.groups()
    return re.findall(r"^\s*pub\(crate\) (\w+):", body, re.M), "deny_unknown_fields" in attrs


def corpus_config(workspace: Path, scratch: Path, cargo_home: Path) -> dict[str, str]:
    """The value of `BLAZE_MACRO_CORPUS`, every path absolute."""
    return dict(workspace=str(workspace.resolve()), scratch=str(scratch.resolve()),
                cargo_home=str(cargo_home.resolve()))


def registry_packages(lock: str) -> list[tuple[str, str]]:
    """Each package of the Cargo.lock text `lock` that comes from a registry, by name and version."""
    packages = tomllib.loads(lock).get("package", [])
    return sorted((p["name"], p["version"]) for p in packages if str(p.get("source", "")).startswith("registry+"))


def unfetched(cargo_home: Path, packages: list[tuple[str, str]]) -> list[str]:
    """The packages of `packages` not unpacked under `cargo_home`'s registry sources."""
    roots = [d for d in (cargo_home / "registry" / "src").glob("*") if d.is_dir()]
    return [f"{name} {version}" for name, version in packages
            if not any((root / f"{name}-{version}").is_dir() for root in roots)]


def runs_left(target: Path) -> tuple[int, list[str]]:
    """How many build scripts' runs `target` holds (`build/<package>-<hash>/out`, cargo's first
    layout, as the harness's builds ask), and each whose `output` is not beside its `OUT_DIR`."""
    outs = sorted(d for d in target.glob("**/build/*/out") if d.is_dir())
    return len(outs), [str(d) for d in outs if not (d.parent / "output").is_file()]


def expected_tests(sources: dict[str, str], prefix: str = "") -> list[str]:
    """The name of every `#[test]` function of each test file's source, in order, as libtest
    names it under the module path `prefix`."""
    return [prefix + name for text in sources.values()
            for name in re.findall(r"^#\[test\]\n(?:#\[[^\n]*\]\n)*(?:pub )?fn (\w+)", text, re.M)]


@dataclass(frozen=True)
class Judged:
    name: str
    verdict: report.Verdict
    detail: str = ""


def judge_tests(expected: list[str], stdout: str, kept: list[str]) -> list[Judged]:
    """One verdict per test of `expected`, and one per test that ran and is not in it, from
    what libtest printed and the tests `blaze test` kept rather than ran."""
    results = dict(re.findall(r"^test (\S+) \.\.\. (ok|FAILED|ignored)\b", stdout, re.M))
    failed = {n.strip() for block in re.findall(r"^failures:\n((?:    \S.*\n)+)", stdout, re.M)
              for n in block.splitlines() if n.strip()}
    said = dict(re.findall(r"^---- (\S+) stdout ----\n(.*?)(?=^---- |^failures:|\Z)", stdout, re.M | re.S))
    out: list[Judged] = []
    for name in [*expected, *sorted(set(results) - set(expected))]:
        result = "FAILED" if name in failed else results.get(name)
        if name in kept:
            out.append(Judged(name, "fail", "kept from an earlier run, not run: the corpus is run, never taken "
                                            "from a result kept before (no-fallback)"))
        elif result == "ok":
            out.append(Judged(name, "pass"))
        elif result == "FAILED":
            out.append(Judged(name, "fail", f"{name} failed:\n{said.get(name, '').strip()[-3000:]}"))
        elif result == "ignored":
            out.append(Judged(name, "fail", f"{name} was ignored: `-- --ignored` did not reach libtest"))
        else:
            out.append(Judged(name, "fail", f"{name} did not run"))
    return out


def step[T](rep: report.Report, name: str, fn: Callable[[], T]) -> T:
    """`fn()`, its outcome `LABEL/<name>`: pass with its seconds, or the `Failed` that stopped it
    as an error; a `Stop` it raised has its outcome already."""
    try:
        began = time.monotonic()
        value = fn()
    except Stop:
        raise
    except engine.Failed as e:
        rep.add(f"{LABEL}/{name}", "error", str(e))
        raise Stop from None
    except OSError as e:
        rep.add(f"{LABEL}/{name}", "error", f"{type(e).__name__}: {e}")
        raise Stop from None
    rep.add(f"{LABEL}/{name}", "pass", seconds=round(time.monotonic() - began, 2))
    return value


def copy(work: Path) -> dict[str, Path]:
    copies: dict[str, Path] = {}
    for name in (*COPIES, "fixture"):
        dest = work / name
        shutil.rmtree(dest, ignore_errors=True)
        shutil.copytree(FIXTURE, dest, symlinks=True, ignore=shutil.ignore_patterns("target"))
        copies[name] = dest
    return copies


def fetch(ws: Path, cargo_home: Path, work: Path) -> None:
    env = engine.reference_env(work / "target-fetch", {"CARGO_HOME": str(cargo_home)})
    engine.run("cargo fetch", ["cargo", "fetch", "--locked", "--manifest-path", str(ws / "Cargo.toml")], work, env,
               FETCH_SECS, check=True)
    if missing := unfetched(cargo_home, registry_packages((ws / "Cargo.lock").read_text())):
        raise engine.Failed(f"`cargo fetch --locked` exited 0 and {cargo_home}/registry/src has no "
                            f"{', '.join(missing)}")


def build(rep: report.Report, eng: engine.Engine, name: str, ws: Path, env: dict[str, str]) -> None:
    """`blaze build -p corpus-calls` in `ws`: a fail when it does not build or leaves a run
    where load.rs does not find it."""
    label = f"{LABEL}/build {name}"
    try:
        ran = eng.build(label, ws, env, ("-p", PACKAGE), timeout=BUILD_SECS)
    except engine.Failed as e:
        rep.add(label, "error", str(e))
        raise Stop from None
    who = eng.not_blazes(ran)
    if ran.returncode != 0 or who is not None:
        detail = f"not blaze's: {who}" if who is not None else f"exit {ran.returncode}"
        rep.add(label, "fail", f"`blaze build -p {PACKAGE}` in {ws}: {detail}:\n{ran.said()}")
        raise Stop
    count, left = runs_left(ws / "target")
    if not count or left:
        what = (f"no build script's run under {ws / 'target'}" if not count
                else f"{len(left)} of {count} runs have no `output` beside their OUT_DIR: {', '.join(left[:5])}")
        rep.add(label, "fail", f"`blaze build -p {PACKAGE}` in {ws}: {what}")
        raise Stop
    rep.add(label, "pass", seconds=round(ran.seconds, 2), runs=count)


def fixture_tests(rep: report.Report, eng: engine.Engine, ws: Path, env: dict[str, str]) -> None:
    """`blaze test -p corpus-calls` in `ws`: one outcome per test of the fixture crate."""
    label = f"{LABEL}/fixture"
    kept_at = ws / "target" / "blaze" / "tests.ndjson"
    try:
        ran = eng.build(label, ws, env, ("-p", PACKAGE), command="test", timeout=TEST_SECS)
    except engine.Failed as e:
        rep.add(label, "error", str(e))
        raise Stop from None
    who = eng.not_blazes(ran)
    if who is not None:
        rep.add(label, "fail", f"not blaze's: {who}")
    elif ran.returncode != 0:
        rep.add(label, "fail", f"`blaze test -p {PACKAGE}` in {ws} exited {ran.returncode}:\n{ran.said()}")
    else:
        rep.add(label, "pass", seconds=round(ran.seconds, 2))
    import blaze_cli

    kept: list[str] = blaze_cli.kept_passes(str(kept_at)) if kept_at.is_file() else []
    expected = [*expected_tests({"tests": FIXTURE_TESTS.read_text()}, "tests::"), *FIXTURE_OWN]
    for judged in judge_tests(expected, ran.stdout + "\n" + ran.stderr, kept):
        rep.add(f"{LABEL}/fixture/{judged.name}", judged.verdict, judged.detail)


def run_tests(rep: report.Report, eng: engine.Engine, work: Path, target: Path, config: Path) -> None:
    home = work / "h-test"
    env = eng.env(home, target=target, extra={"BLAZE_MACRO_CORPUS": str(config)})
    for name in UNSET:
        env.pop(name, None)
    kept_at = target / "blaze" / "tests.ndjson"
    kept_at.unlink(missing_ok=True)
    label = f"{LABEL}/blaze test"
    try:
        ran = eng.build(label, engine.ROOT, env, TEST_ARGS, command="test", timeout=TEST_SECS)
    except engine.Failed as e:
        rep.add(label, "error", str(e))
        return
    finally:
        try:
            if home.is_dir():
                eng.stop(engine.ROOT, env)
        except engine.Failed as e:
            rep.add(f"{LABEL}: stopping the test daemon", "error", str(e))
    who = eng.not_blazes(ran)
    if who is not None:
        rep.add(label, "fail", f"not blaze's: {who}")
    elif ran.returncode != 0:
        rep.add(label, "fail", f"`blaze test {' '.join(TEST_ARGS)}` exited {ran.returncode}:\n{ran.said()}")
    else:
        rep.add(label, "pass", seconds=round(ran.seconds, 2))
    import blaze_cli

    kept: list[str] = blaze_cli.kept_passes(str(kept_at))
    sources = {t: (TESTS / f"{t}.rs").read_text() for t in TEST_FILES}
    for judged in judge_tests(expected_tests(sources), ran.stdout + "\n" + ran.stderr, kept):
        rep.add(f"{LABEL}/{judged.name}", judged.verdict, judged.detail)


def main(args: Input) -> None:
    for needed in (FIXTURE / "Cargo.lock", FIXTURE_TESTS, CONFIG_RS, *(TESTS / f"{t}.rs" for t in TEST_FILES)):
        if not needed.is_file():
            fail(SCRIPT, f"no file {needed}")
    fields, refuses = config_fields(CONFIG_RS.read_text())
    if not refuses or sorted(fields) != sorted(corpus_config(Path(), Path(), Path())):
        fail(SCRIPT, f"{CONFIG_RS}'s `Corpus` has the fields {fields} (deny_unknown_fields: {refuses}); "
                     "this writes another config")
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-macroeval-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    target = Path(args.target).resolve() if args.target else work / "target"
    cargo_home, home, store = work / "cargo-home", work / "h", work / "store"
    rep = report.Report(SCRIPT, args.engine, args.out)
    rep.extra.update(work=str(work), target=str(target), toolchain=engine.TOOLCHAIN)
    print(f"work: {work}", flush=True)
    build_env = eng.env(home, store, extra={"CARGO_HOME": str(cargo_home)})
    try:
        rep.extra["toolchain_identity"] = step(rep, "toolchain", lambda: engine.toolchain_identity(engine.scrubbed()))
        copies = step(rep, "copy", lambda: copy(work))
        step(rep, "fetch", lambda: fetch(copies["workspace"], cargo_home, work))
        try:
            for name in COPIES:
                build(rep, eng, name, copies[name], build_env)
            fixture_tests(rep, eng, copies["fixture"], build_env)
        finally:
            try:
                if home.is_dir():
                    eng.stop(copies["workspace"], build_env)
            except engine.Failed as e:
                rep.add(f"{LABEL}: stopping the daemon", "error", str(e))
        config = corpus_config(copies["workspace"], copies["scratch"], cargo_home)
        rep.extra.update(config=config)
        config_at = work / "corpus.json"
        config_at.write_text(json.dumps(config, indent=1) + "\n")
        run_tests(rep, eng, work, target, config_at)
    except Stop:
        pass
    finally:
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)
    rep.finish()


def refused(fn: Callable[[], object]) -> str:
    try:
        fn()
    except engine.Failed as e:
        return str(e)
    except SystemExit as e:
        return str(e.code)
    raise AssertionError(f"{fn} was taken")


def test_input() -> None:
    args = parse(Input, '{"engine": {"kind": "v2", "blaze": "b"}, "out": ""}', SCRIPT)
    assert (args.work, args.target, args.keep) == ("", "", False), args
    assert "no field wasm" in refused(
        lambda: parse(Input, '{"engine": {"kind": "v2", "blaze": "b"}, "out": "", "wasm": []}', SCRIPT))


def test_config() -> None:
    fields, refuses = config_fields(CONFIG_RS.read_text())
    assert refuses and fields == ["workspace", "scratch", "cargo_home"], fields
    with tempfile.TemporaryDirectory() as tmp:
        top = Path(tmp).resolve()
        config = corpus_config(top / "workspace", top / "scratch", top / "cargo-home")
    assert list(config) == fields, list(config)
    assert config["workspace"] == str(top / "workspace") and config["cargo_home"] == str(top / "cargo-home"), config
    assert json.loads(json.dumps(config)) == config
    assert config_fields("#[derive(Deserialize)]\npub(crate) struct Corpus {\n    pub(crate) a: String,\n}") == (["a"], False)
    assert "no `pub(crate) struct Corpus`" in refused(lambda: config_fields("struct Other {}"))


def test_lock() -> None:
    packages = registry_packages((FIXTURE / "Cargo.lock").read_text())
    names = dict(packages)
    assert names.get("serde_derive") == "1.0.228" and names.get("thiserror-impl") == "2.0.18", packages
    assert names.get("tokio-macros") == "2.6.1" and names.get("syn", "").startswith("2."), packages
    assert not {"corpus-calls", "corpus-macros", "corpus-state", "corpus-panics"} & set(names), packages
    with tempfile.TemporaryDirectory() as tmp:
        home = Path(tmp)
        assert unfetched(home, [("syn", "2.0.117")]) == ["syn 2.0.117"]
        (home / "registry" / "src" / "index.crates.io-1949cf8c6b5b557f" / "syn-2.0.117").mkdir(parents=True)
        assert unfetched(home, [("syn", "2.0.117"), ("quote", "1.0.45")]) == ["quote 1.0.45"]


def test_fixture() -> None:
    """The fixture's own proc-macro crates, and the tests this runs of this repository."""
    own = []
    for manifest in sorted(FIXTURE.glob("*/Cargo.toml")):
        parsed = tomllib.loads(manifest.read_text())
        if parsed.get("lib", {}).get("proc-macro"):
            own.append(parsed["package"]["name"].replace("-", "_"))
    assert own == ["corpus_macros", "corpus_state"], own
    assert TEST_ARGS == ("-p", "blaze-macro-eval", "--test", "corpus", "--test", "kept", "--", "--ignored"), TEST_ARGS


def test_runs() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        target = Path(tmp)
        assert runs_left(target) == (0, [])
        for package in ("serde-0123456789abcdef", "thiserror-fedcba9876543210"):
            (target / "debug" / "build" / package / "out").mkdir(parents=True)
        (target / "debug" / "build" / "serde-0123456789abcdef" / "output").write_text("cargo:rustc-cfg=x\n")
        count, left = runs_left(target)
        assert count == 2 and left == [str(target / "debug" / "build" / "thiserror-fedcba9876543210" / "out")], left


LIBTEST = """running 2 tests
test every_call_is_expanded ... ok
test a_static_counts_calls_in_rustcs_order ... FAILED

failures:

---- a_static_counts_calls_in_rustcs_order stdout ----
thread 'a_static_counts_calls_in_rustcs_order' panicked at tests/corpus.rs:9:1:
counted [("FIRST", ["1"])]

failures:
    a_static_counts_calls_in_rustcs_order

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 5 filtered out; finished in 0.10s

running 1 test
test a_kept_expansion_is_found_and_one_whose_input_changed_is_not ... ignored

test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s
"""


def test_tests() -> None:
    sources = {t: (TESTS / f"{t}.rs").read_text() for t in TEST_FILES}
    expected = expected_tests(sources)
    assert {"every_call_is_expanded", "a_static_counts_calls_in_rustcs_order", "a_panic_is_its_message",
            "a_kept_expansion_is_found_and_one_whose_input_changed_is_not"} <= set(expected), expected
    assert len(expected) == len(set(expected)), expected
    assert expected_tests({"x": "#[test]\n#[ignore = \"why\"]\nfn a() {}\n#[test]\nfn b() {}\nfn c() {}\n"}) == ["a", "b"]
    wanted = ["every_call_is_expanded", "a_static_counts_calls_in_rustcs_order",
              "a_kept_expansion_is_found_and_one_whose_input_changed_is_not", "a_panic_is_its_message",
              "every_macro_kind_is_called"]
    judged = {j.name: j for j in judge_tests(wanted, LIBTEST + "test stray ... ok\n", ["every_macro_kind_is_called"])}
    assert list(judged) == [*wanted, "stray"], list(judged)
    assert judged["every_call_is_expanded"].verdict == "pass"
    failed = judged["a_static_counts_calls_in_rustcs_order"]
    assert failed.verdict == "fail" and "counted" in failed.detail and "failures:" not in failed.detail, failed
    ignored = judged["a_kept_expansion_is_found_and_one_whose_input_changed_is_not"]
    assert ignored.verdict == "fail" and "ignored" in ignored.detail, ignored
    assert judged["a_panic_is_its_message"] == Judged("a_panic_is_its_message", "fail", "a_panic_is_its_message did not run")
    kept = judged["every_macro_kind_is_called"]
    assert kept.verdict == "fail" and "kept from an earlier run" in kept.detail, kept
    assert judged["stray"].verdict == "pass"


def test_fixture_tests() -> None:
    expected = expected_tests({"tests": FIXTURE_TESTS.read_text()}, "tests::")
    assert "tests::serde_tags_variants_inside" in expected and "tests::the_corpus_macros_do_what_they_say" in expected, expected
    assert len(expected) == len(set(expected)), expected
    assert expected_tests({"x": "#[test]\nfn a() {}\n"}, "m::") == ["m::a"]


SELFTESTS = (test_input, test_config, test_lock, test_fixture, test_runs, test_tests, test_fixture_tests)


def selftest() -> int:
    failed = 0
    for check in SELFTESTS:
        try:
            check()
            print(f"ok    {check.__name__}", flush=True)
        except (Exception, SystemExit) as e:
            failed += 1
            print(f"FAIL  {check.__name__}: {type(e).__name__}: {e}", flush=True)
    print(f"{SCRIPT} selftest: {len(SELFTESTS) - failed} of {len(SELFTESTS)} pass", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    if sys.argv[1:] == ["selftest"]:
        sys.exit(selftest())
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    main(parse(Input, sys.argv[1], SCRIPT))
