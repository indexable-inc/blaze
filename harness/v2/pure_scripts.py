"""pure_scripts.py: build scripts proved pure from their MIR, checked and timed
(docs/v2/plan/lanes/pure-scripts/design.md section 8; how to run it and read it:
docs/v2/plan/lanes/pure-scripts/measure.md). A script is pure when its whole call graph only reads
cargo's variables and its package's files, writes its `OUT_DIR` and prints; its result is then kept
under a key of its code's identity and those inputs, and found there before it is compiled.

    uv run --frozen python harness/v2/pure_scripts.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/pure.json"}'
    uv run --frozen python harness/v2/pure_scripts.py '{"engine": {...}, "out": "...", "sets": ["crates"], "crates": ["serde_json", "libc"]}'
    uv run --frozen python harness/v2/pure_scripts.py selftest

The input is one JSON object (inline, a file's path, or `-`):

    engine    the blaze measured (engine.EngineSpec); v1 has no pure scripts, and says so
    out       the result's path (report.py's JSON)
    work      where each project's source, trees, homes and stores go; a new directory under
              /tmp when empty
    sets      which projects: "crates" (pure_scripts/corpus.json, each crate built as its own
              workspace from its .crate, checked against the sparse index as corpus.py does) and
              "apps" (harness/apps/apps.json, set up as reachable.py does); both by default
    crates    the corpus crates built, by name; empty: all of them
    apps      the apps built (ripgrep and every local app by default)
    profile   dev or release (dev)
    keep      keep each project's work directory (false)

Each project is built cold three times at one path into one store, each build with an empty home,
target directory and daemon of its own, timed from nothing to the end of
`blaze build --message-format=json` (design.md section 8: no store holds only the pure keys):

    sandboxed  the store empty: every script compiles and runs in the sandbox, and each pure
               script's result is kept under its pure key as well (crates/blaze-cargo/src/serve/
               unit/script/pure.rs)
    pure       RUSTFLAGS='--cfg blaze_pure_measure_2': every compile key changes and no pure key
               does (the MIR identity is the call graph's, the key's variables the proof's), so
               everything compiles again but the pure scripts, whose results are found under their
               pure keys; a pure script that reads CARGO_ENCODED_RUSTFLAGS or whose MIR uses that
               cfg misses, and is reported
    verify     RUSTFLAGS='--cfg blaze_pure_measure_3' and BLAZE_PURE=verify: everything compiles
               again and the pure scripts are built and run as well, the build failing where a run
               differs from the kept value (the differential); its time is the control

The time saved is the verify build's less the pure build's (the verify build's comparisons of the
kept values are in it, and are small beside a script's compile and run).

What a build says of each script run is its `blaze-script` line (blaze_api::BuildScriptRan's
`purity`, printed beside cargo's `build-script-executed`): pure with its identity and whether its
pure key hit, impure with the first impurity as hir_ty_pure::Impure::said says it, or unproved.

Outcomes: one per script run of the sandboxed build, `<project>: <package>@<version>`, which passes
when its purity is what pure_scripts/corpus.json or pure_scripts/apps.json expect of it, or what its
resolved build dependencies say (cc or cmake: cc; autocfg, version_check, rustc_version: rustc;
pkg-config). A script proved pure that has cc or cmake as a build dependency, or that is expected
not to be pure, fails: the proof is unsound. One expected pure and not proved so fails: the proof
is incomplete, or the expectation is wrong. Per project: `build` (the sandboxed build), `hits`
(every pure script of the sandboxed build hit in the pure one, with the same identity, and every
other script was judged alike), `time` (the three cold times and what the pure path saved) and `verify`.
The result's `extra` has every script (`scripts`), every project's times (`projects`) and the total
saved (`saved_s`); a table of both ends the output. `selftest` checks the data, the input, the
parsing of the lines, the expectations and the verdicts without blaze, cargo or the network.
"""

import contextlib
import hashlib
import json
import shutil
import sys
import tempfile
import time
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, cast

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse
import engine
import report
import budgets
import corpus
import reachable

SCRIPT = "pure_scripts.py"
DATA = HERE / "pure_scripts"
# The job's variable crates/blaze-cargo/src/serve/unit/script/pure.rs reads.
PURE = "BLAZE_PURE"
RUSTFLAGS = "RUSTFLAGS"
# The fresh cfg of each later pass: it changes every compile key and no pure key.
FRESH_CFG: dict[str, str] = {"pure": "blaze_pure_measure_2", "verify": "blaze_pure_measure_3"}
# The `reason` of the line --message-format=json prints for each build script run beside cargo's.
SCRIPT_LINE = "blaze-script"
EXECUTED = "build-script-executed"
BUILD_SECS = 4 * 3600
SHOWN = 10

type Expect = Literal["pure", "cc", "rustc", "pkg-config", "env", "listing", "process", "thread", "random", "other"]
type Seen = Literal["registry", "memory"]
type Group = Literal["crates", "apps"]
type Profile = Literal["dev", "release"]
type Pass = Literal["sandboxed", "pure", "verify"]
type Kind = Literal["unproved", "impure", "pure"]
type By = Literal["pinned", "name", "build dependency"]
KINDS: tuple[Kind, ...] = ("unproved", "impure", "pure")
# A build dependency that says what a script does: the first that matches names it.
BUILD_DEPENDENCIES: tuple[tuple[tuple[str, ...], Expect, str], ...] = (
    (("cc", "cmake"), "cc", "builds C with {dep}: spawns the C compiler"),
    (("pkg-config",), "pkg-config", "probes a system library with {dep}"),
    (("autocfg", "version_check", "rustc_version"), "rustc", "{dep} reads RUSTC and runs it"),
)
CC_DEPENDENCIES = BUILD_DEPENDENCIES[0][0]


@dataclass(frozen=True)
class Expected:
    """What a script is expected to be: pure, or why it is not. `version` empty is any version
    (apps.json only)."""

    name: str
    version: str
    expect: Expect
    why: str
    seen: Seen


@dataclass(frozen=True)
class Corpus:
    """pure_scripts/corpus.json."""

    about: str
    crates: tuple[Expected, ...]


@dataclass(frozen=True)
class AppScripts:
    """pure_scripts/apps.json."""

    about: str
    scripts: tuple[Expected, ...]


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    sets: tuple[Group, ...] = ("crates", "apps")
    crates: tuple[str, ...] = ()
    apps: tuple[str, ...] = reachable.DEFAULT_PROJECTS
    profile: Profile = "dev"
    keep: bool = False


def load_corpus(path: Path = DATA / "corpus.json") -> Corpus:
    return parse(Corpus, str(path), SCRIPT)


def load_apps(path: Path = DATA / "apps.json") -> AppScripts:
    return parse(AppScripts, str(path), SCRIPT)


# What a build says.

@dataclass(frozen=True)
class Purity:
    kind: Kind
    # The first impurity, as hir_ty_pure::Impure::said says it (impure).
    said: str | None = None
    # The identity's hex (pure).
    identity: str | None = None
    # Its value was found under its pure key: the script was neither built nor run, but in the
    # verify build, which builds and runs it as well (pure).
    hit: bool = False


@dataclass(frozen=True)
class ScriptRun:
    package_id: str
    name: str
    version: str
    out_dir: str
    purity: Purity


def package_of(step: str, package_id: str) -> tuple[str, str]:
    """The name and version of cargo's package id: `registry+https://...#serde@1.0.229`,
    `path+file:///w/foo#0.1.0` (the name is the directory's), or the old `serde 1.0.229 (...)`."""
    if "#" in package_id:
        url, fragment = package_id.rsplit("#", 1)
        if "@" in fragment:
            name, version = fragment.split("@", 1)
            return name, version
        return url.rstrip("/").rsplit("/", 1)[-1], fragment
    words = package_id.split(" ")
    if len(words) >= 2 and words[0] and words[1]:
        return words[0], words[1]
    raise engine.Failed(f"{step}: {package_id!r} is no package id cargo writes")


def script_run(step: str, line: Mapping[str, object]) -> ScriptRun:
    """One `blaze-script` line, checked whole."""
    package_id, out_dir, kind = line.get("package_id"), line.get("out_dir"), line.get("purity")
    said, identity, hit = line.get("said"), line.get("identity"), line.get("hit")
    wrong = f"{step}: the `{SCRIPT_LINE}` line {json.dumps(line)}"
    if not isinstance(package_id, str) or not isinstance(out_dir, str):
        raise engine.Failed(f"{wrong} names no package_id and out_dir")
    if kind not in KINDS:
        raise engine.Failed(f"{wrong} has purity {kind!r}, not one of {', '.join(KINDS)}")
    if not isinstance(hit, bool):
        raise engine.Failed(f"{wrong} has hit {hit!r}, not a bool")
    if kind == "impure" and not (isinstance(said, str) and said):
        raise engine.Failed(f"{wrong} is impure and says no first impurity")
    if kind == "pure" and not (isinstance(identity, str) and identity):
        raise engine.Failed(f"{wrong} is pure and has no identity")
    if hit and kind != "pure":
        raise engine.Failed(f"{wrong} hit a pure key and is not pure")
    name, version = package_of(step, package_id)
    purity = Purity(kind, cast(str | None, said if kind == "impure" else None),
                    cast(str | None, identity if kind == "pure" else None), hit)
    return ScriptRun(package_id, name, version, out_dir, purity)


def script_runs(step: str, stdout: str) -> list[ScriptRun]:
    """Every script run a build's --message-format=json output says; a `build-script-executed`
    line with no `blaze-script` line for its package and OUT_DIR is a `Failed`."""
    executed: list[tuple[str, str]] = []
    runs: list[ScriptRun] = []
    for text in stdout.splitlines():
        if not text.startswith("{") or ('"' + SCRIPT_LINE + '"' not in text and EXECUTED not in text):
            continue
        try:
            line = json.loads(text)
        except json.JSONDecodeError:
            continue
        if not isinstance(line, dict):
            continue
        line = cast(dict[str, object], line)
        if line.get("reason") == EXECUTED:
            executed.append((str(line.get("package_id")), str(line.get("out_dir"))))
        elif line.get("reason") == SCRIPT_LINE:
            runs.append(script_run(step, line))
    named = {(run.package_id, run.out_dir) for run in runs}
    missing = [ran for ran in executed if ran not in named]
    if missing:
        raise engine.Failed(f"{step}: {len(missing)} of {len(executed)} build script runs have no `{SCRIPT_LINE}` "
                            f"line saying whether the script is pure (blaze_api::BuildScriptRan's purity), first "
                            f"{missing[0][0]} at {missing[0][1]}")
    return runs


# What is expected.

@dataclass(frozen=True)
class Expectation:
    expect: Expect
    why: str
    by: By


class Expectations:
    """corpus.json's and apps.json's expectations: by name and version, else by name (the first
    listed), else from a script's resolved build dependencies."""

    def __init__(self, listed: Sequence[Expected]):
        self.pinned = {(e.name, e.version): e for e in listed if e.version}
        self.named: dict[str, Expected] = {}
        for e in listed:
            self.named.setdefault(e.name, e)

    def of(self, name: str, version: str, build_deps: Sequence[str]) -> Expectation | None:
        if (pinned := self.pinned.get((name, version))) is not None:
            return Expectation(pinned.expect, pinned.why, "pinned")
        if (named := self.named.get(name)) is not None:
            at = f"at {named.version}" if named.version else "at any version"
            return Expectation(named.expect, f"{named.why} ({at})", "name")
        return derived(build_deps)


def derived(build_deps: Sequence[str]) -> Expectation | None:
    for deps, expect, why in BUILD_DEPENDENCIES:
        if found := next((dep for dep in build_deps if dep in deps), None):
            return Expectation(expect, why.format(dep=found), "build dependency")
    return None


def judge(purity: Purity, expected: Expectation | None, build_deps: Sequence[str]) -> tuple[report.Verdict, str]:
    """A script run's verdict: its purity against what is expected of it."""
    if purity.kind == "unproved":
        return "error", "no proof was made of it: blaze proves no script pure when the sandbox is off (BLAZE_SANDBOX)"
    cc = next((dep for dep in build_deps if dep in CC_DEPENDENCIES), None)
    if purity.kind == "pure" and cc is not None:
        return "fail", (f"proved pure, but it builds with {cc}, which spawns the C compiler: the proof is unsound "
                        f"(identity {purity.identity})")
    if expected is None:
        if purity.kind == "pure":
            return "pass", f"pure (identity {purity.identity}); nothing is expected of it, its value is checked by the verify build"
        return "pass", f"not pure: {purity.said}; nothing is expected of it"
    if expected.expect == "pure":
        if purity.kind == "pure":
            return "pass", f"pure, as expected: {expected.why}"
        return "fail", f"expected pure ({expected.why}), not proved so: {purity.said}"
    if purity.kind == "pure":
        return "fail", (f"proved pure, but it is not ({expected.expect}: {expected.why}): the proof is unsound "
                        f"(identity {purity.identity})")
    return "pass", f"not pure ({expected.expect}, as expected): {purity.said}"


def pure_misses(sandboxed: Sequence[ScriptRun], pure: Sequence[ScriptRun]) -> list[str]:
    """What the pure build says differently of the scripts of the sandboxed one: a pure script
    whose key did not hit or whose identity changed, a script judged otherwise, one missing."""
    found = {(run.package_id, run.out_dir): run for run in pure}
    misses: list[str] = []
    for run in sandboxed:
        label = f"{run.name}@{run.version} at {run.out_dir}"
        other = found.get((run.package_id, run.out_dir))
        if other is None:
            misses.append(f"{label}: not in the build")
        elif other.purity.kind != run.purity.kind:
            misses.append(f"{label}: {run.purity.kind} in the sandboxed build, {other.purity.kind} in this one")
        elif run.purity.kind == "pure" and other.purity.identity != run.purity.identity:
            misses.append(f"{label}: identity {run.purity.identity} in the sandboxed build, {other.purity.identity} in this one")
        elif run.purity.kind == "pure" and not other.purity.hit:
            misses.append(f"{label}: its pure key did not hit")
        elif run.purity.kind == "impure" and other.purity.said != run.purity.said:
            misses.append(f"{label}: first impurity {run.purity.said!r} in the sandboxed build, {other.purity.said!r} in this one")
    return misses


# The builds.

@dataclass(frozen=True)
class Project:
    """One project set up once: its tree `tree` under `source` (with what lies beside it), its
    workspace in the tree (`sub`), the arguments and environment of `blaze build`, and each
    package's resolved build dependencies by (name, version)."""

    label: str
    source: Path
    tree: str
    sub: str
    build: tuple[str, ...]
    env: dict[str, str]
    beside: tuple[str, ...]
    build_deps: dict[tuple[str, str], tuple[str, ...]]
    lock: str


@dataclass
class Built:
    seconds: float = 0.0
    failed: str | None = None
    runs: list[ScriptRun] = field(default_factory=list[ScriptRun])


def resolved_build_deps(label: str, ws: Path, env: dict[str, str]) -> dict[tuple[str, str], tuple[str, ...]]:
    """Each package's build dependencies as cargo resolved them, by name, from `cargo metadata`
    of the lock file the builds read."""
    ran = engine.cargo(f"{label}: cargo metadata", ["metadata", "--format-version", "1", "--locked", "--offline"], ws, env)
    meta: dict[str, Any] = json.loads(ran.checked(f"{label}: cargo metadata").stdout)
    names = {p["id"]: (p["name"], p["version"]) for p in meta["packages"]}
    out: dict[tuple[str, str], tuple[str, ...]] = {}
    resolve: dict[str, Any] = meta.get("resolve") or {}
    nodes: list[dict[str, Any]] = resolve.get("nodes", [])
    for node in nodes:
        deps = {names[dep["pkg"]][0] for dep in node["deps"] if any(k["kind"] == "build" for k in dep["dep_kinds"])}
        out[names[node["id"]]] = tuple(sorted(deps))
    return out


def lock_digest(ws: Path, tree: Path) -> str:
    for at in (ws, *ws.parents):
        if (at / "Cargo.lock").is_file():
            return hashlib.sha256((at / "Cargo.lock").read_bytes()).hexdigest()
        if at == tree:
            break
    raise engine.Failed(f"no Cargo.lock in {ws} or above it in {tree} after `cargo fetch`")


def finished(label: str, top: Path, source: Path, tree: str, sub: str, build: tuple[str, ...],
             env: dict[str, str], fetch: bool) -> Project:
    ws = source / tree / sub if sub else source / tree
    cargo_env = engine.reference_env(top / "fetch-target", env)
    if fetch:
        engine.cargo(f"{label}: cargo fetch", ["fetch"], ws, cargo_env).checked(f"{label}: cargo fetch")
    deps = resolved_build_deps(label, ws, cargo_env)
    shutil.rmtree(top / "fetch-target", ignore_errors=True)
    beside = tuple(sorted(entry.name for entry in source.iterdir() if entry.name != tree))
    return Project(label, source, tree, sub, build, env, beside, deps, lock_digest(ws, source / tree))


def crate_project(crate: Expected, top: Path, cache: Path) -> Project:
    """A corpus crate as its own workspace: its .crate, verified against the index, unpacked."""
    source = top / "source"
    shutil.rmtree(source, ignore_errors=True)
    source.mkdir(parents=True)
    tree = f"{crate.name}-{crate.version}"
    corpus.unpack(corpus.fetch_crate(corpus.Crate(crate.name, crate.version), cache), tree, source / tree)
    return finished(f"{crate.name}@{crate.version}", top, source, tree, "", (), {}, fetch=True)


def app_project(name: str, top: Path) -> Project:
    """An app of harness/apps/apps.json, set up as reachable.py sets it up (fetched, vendored
    where its runs read the vendored source, its `setup` run)."""
    s = reachable.subject(name, top)
    return finished(s.label, top, s.source, s.tree, s.sub, s.build, s.env, fetch=False)


def pass_env(env: dict[str, str], which: Pass) -> dict[str, str]:
    """`env` for a build of `which`: the pass's fresh cfg after the project's own RUSTFLAGS in the
    pure and verify ones, BLAZE_PURE=verify in the verify one alone, never inherited."""
    out = dict(env)
    out.pop(PURE, None)
    if which in FRESH_CFG:
        out[RUSTFLAGS] = " ".join(filter(None, (out.get(RUSTFLAGS, ""), f"--cfg {FRESH_CFG[which]}")))
    if which == "verify":
        out[PURE] = "verify"
    return out


def cold(eng: engine.Engine, p: Project, top: Path, profile: Profile, which: Pass, store: Path) -> Built:
    """A cold build of `p` at `top/build/<tree>` with an empty home and target directory, `store`
    as its store (shared by the three passes) and a daemon of its own, timed; the daemon is
    stopped after it."""
    builds = top / "build"
    tree, home = builds / p.tree, top / "home"
    for path in (tree, home):
        shutil.rmtree(path, ignore_errors=True)
    builds.mkdir(parents=True, exist_ok=True)
    for entry in p.beside:
        if not (builds / entry).is_symlink() and not (builds / entry).exists():
            (builds / entry).symlink_to(p.source / entry)
    shutil.copytree(p.source / p.tree, tree, symlinks=True, ignore=budgets.left_out)
    ws = tree / p.sub if p.sub else tree
    env = pass_env(eng.env(home, store, None, p.env), which)
    release = ["--release"] if profile == "release" else []
    step = f"{p.label} {profile} ({which})"
    got = Built()
    try:
        began = time.monotonic()
        ran = eng.build(f"{step}: build", ws, env, ["--message-format=json", *p.build, *release], timeout=BUILD_SECS)
        got.seconds = time.monotonic() - began
        if ran.returncode != 0:
            got.failed = f"`blaze build` exited {ran.returncode}:\n{ran.said()}"
            # What the scripts that ran said, where it can be read: the failure is the finding.
            with contextlib.suppress(engine.Failed):
                got.runs = script_runs(step, ran.stdout)
            return got
        got.runs = script_runs(step, ran.stdout)
        return got
    finally:
        eng.stop(ws, env)


@dataclass
class Tally:
    """What the report's `extra` and the closing table are made of."""

    scripts: list[dict[str, report.Number]] = field(default_factory=list[dict[str, report.Number]])
    projects: list[dict[str, report.Number]] = field(default_factory=list[dict[str, report.Number]])


def judged(rep: report.Report, p: Project, runs: Sequence[ScriptRun], expectations: Expectations,
           tally: Tally) -> tuple[int, int]:
    """An outcome per script run of the sandboxed build; how many held and how many are pure."""
    held = pure = 0
    seen: dict[tuple[str, str], int] = {}
    for run in runs:
        key = (run.name, run.version)
        seen[key] = seen.get(key, 0) + 1
        build_deps = p.build_deps.get(key, ())
        expected = expectations.of(run.name, run.version, build_deps)
        verdict, detail = judge(run.purity, expected, build_deps)
        held += verdict == "pass"
        pure += run.purity.kind == "pure"
        nth = f" [{seen[key]}]" if seen[key] > 1 else ""
        numbers: dict[str, report.Number] = dict(
            purity=run.purity.kind, expected=expected.expect if expected else None, by=expected.by if expected else None)
        rep.add(f"{p.label}: {run.name}@{run.version}{nth}", verdict, detail, **numbers)
        tally.scripts.append(dict(project=p.label, package=run.name, version=run.version, out_dir=run.out_dir,
                                  purity=run.purity.kind, said=run.purity.said, identity=run.purity.identity,
                                  expected=expected.expect if expected else None, by=expected.by if expected else None,
                                  build_deps=",".join(build_deps), verdict=verdict))
    return held, pure


def measure(eng: engine.Engine, rep: report.Report, p: Project, top: Path, profile: Profile,
            expectations: Expectations, tally: Tally) -> None:
    store = top / "store"
    shutil.rmtree(store, ignore_errors=True)
    sandboxed = cold(eng, p, top, profile, "sandboxed", store)
    if sandboxed.failed is not None:
        rep.add(f"{p.label} build", "fail", f"the sandboxed build: {sandboxed.failed}", seconds=round(sandboxed.seconds, 1))
        return
    rep.add(f"{p.label} build", "pass", f"cold {sandboxed.seconds:.1f} s, {len(sandboxed.runs)} script runs",
            seconds=round(sandboxed.seconds, 1), scripts=len(sandboxed.runs))
    held, pure = judged(rep, p, sandboxed.runs, expectations, tally)
    pure_built = cold(eng, p, top, profile, "pure", store)
    verify = cold(eng, p, top, profile, "verify", store)
    saved = verify.seconds - pure_built.seconds
    verify_misses = pure_misses(sandboxed.runs, verify.runs) if verify.failed is None else []
    row: dict[str, report.Number] = dict(
        project=p.label, scripts=len(sandboxed.runs), pure=pure, held=held,
        hits=sum(run.purity.hit for run in pure_built.runs), seconds_sandboxed=round(sandboxed.seconds, 1),
        seconds_pure=round(pure_built.seconds, 1), seconds_control=round(verify.seconds, 1), saved_s=round(saved, 1),
        verified=verify.failed is None and not verify_misses, lock=p.lock)
    tally.projects.append(row)
    if pure_built.failed is not None:
        rep.add(f"{p.label} hits", "fail", f"the pure-path build failed where the sandboxed one built: {pure_built.failed}")
    elif misses := pure_misses(sandboxed.runs, pure_built.runs):
        more = f"\n... and {len(misses) - SHOWN} more" if len(misses) > SHOWN else ""
        rep.add(f"{p.label} hits", "fail", f"{len(misses)} scripts (a pure script that reads CARGO_ENCODED_RUSTFLAGS "
                f"or whose MIR uses cfg {FRESH_CFG['pure']} misses under {RUSTFLAGS}='--cfg {FRESH_CFG['pure']}'):\n"
                + "\n".join(misses[:SHOWN]) + more, pure=pure)
    else:
        rep.add(f"{p.label} hits", "pass", f"all {pure} pure scripts hit their pure keys", pure=pure)
    numbers: dict[str, report.Number] = dict(
        seconds_sandboxed=row["seconds_sandboxed"], seconds_pure=row["seconds_pure"],
        seconds_control=row["seconds_control"], saved_s=row["saved_s"],
        ratio=round(pure_built.seconds / verify.seconds, 3) if verify.seconds > 0 else None)
    if pure_built.failed is not None:
        rep.add(f"{p.label} time", "error", "not timed: the pure-path build failed", **numbers)
    elif verify.failed is not None:
        rep.add(f"{p.label} time", "error", "not timed: the control build (verify) failed", **numbers)
    else:
        rep.add(f"{p.label} time", "pass", f"cold {verify.seconds:.1f} s with every script built and run, "
                f"{pure_built.seconds:.1f} s with {pure} pure scripts kept: {saved:.1f} s saved "
                f"(the first, sandboxed build {sandboxed.seconds:.1f} s)", **numbers)
    if verify.failed is not None:
        rep.add(f"{p.label} verify", "fail", f"{PURE}=verify: {verify.failed}", pure=pure)
    elif verify_misses:
        rep.add(f"{p.label} verify", "fail", f"{PURE}=verify built, but {len(verify_misses)} scripts were judged otherwise:\n"
                + "\n".join(verify_misses[:SHOWN]), pure=pure)
    else:
        rep.add(f"{p.label} verify", "pass", f"every pure hit's value is its sandboxed run's ({pure} scripts)", pure=pure)


# The output.

COLUMNS = (("project", 34), ("scripts", 8), ("pure", 6), ("hits", 6), ("held", 8), ("sandboxed s", 12),
           ("control s", 10), ("pure s", 9), ("saved s", 9), ("verified", 9))


def table(projects: Sequence[dict[str, report.Number]]) -> str:
    """The closing table: a line per project and the totals."""
    def line(cells: Sequence[object]) -> str:
        return "".join(str(cell).ljust(width) if at == 0 else str(cell).rjust(width)
                       for at, (cell, (_, width)) in enumerate(zip(cells, COLUMNS)))

    out = [line([name for name, _ in COLUMNS])]
    for p in projects:
        out.append(line([p["project"], p["scripts"], p["pure"], p["hits"], f"{p['held']}/{p['scripts']}",
                         p["seconds_sandboxed"], p["seconds_control"], p["seconds_pure"], p["saved_s"],
                         "yes" if p["verified"] else "NO"]))

    def total(key: str) -> float:
        return sum(cast(float, p[key]) for p in projects)

    out.append(line(["total", int(total("scripts")), int(total("pure")), int(total("hits")),
                     f"{int(total('held'))}/{int(total('scripts'))}", round(total("seconds_sandboxed"), 1),
                     round(total("seconds_control"), 1), round(total("seconds_pure"), 1), round(total("saved_s"), 1),
                     f"{sum(bool(p['verified']) for p in projects)}/{len(projects)}"]))
    return "\n".join(out)


def main(args: Input) -> None:
    data = load_corpus()
    apps = load_apps()
    if not args.sets:
        fail(SCRIPT, "sets: name crates, apps or both")
    if unknown := sorted(set(args.crates) - {c.name for c in data.crates}):
        fail(SCRIPT, f"crates: corpus.json has no {', '.join(unknown)}")
    if unknown := sorted(set(args.apps) - set(reachable.APPS)):
        fail(SCRIPT, f"apps: no {', '.join(unknown)} in {reachable.APPS_JSON}")
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    rep = report.Report(SCRIPT, args.engine, args.out)
    if eng.kind == "v1":
        rep.add("pure scripts", "unmeasured", "v1 proves no build script pure: every script compiles and runs in the sandbox")
        rep.finish()
    expectations = Expectations((*data.crates, *apps.scripts))
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-pure-scripts-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    print(f"work: {work}", flush=True)
    cache = work / "crates.io"
    tally = Tally()
    chosen: list[tuple[str, Path, Expected | None]] = []
    if "crates" in args.sets:
        chosen += [(f"{c.name}@{c.version}", work / f"c-{c.name}-{c.version}", c) for c in data.crates
                   if not args.crates or c.name in args.crates]
    if "apps" in args.sets:
        chosen += [(name, work / f"a-{name}", None) for name in dict.fromkeys(args.apps)]
    for label, top, crate in chosen:
        shutil.rmtree(top, ignore_errors=True)
        top.mkdir(parents=True)
        p = rep.guarded(f"{label}: setting up", lambda: budgets.failing(
            label, lambda: crate_project(crate, top, cache) if crate is not None else app_project(label, top)))
        if p is not None:
            rep.guarded(label, lambda: budgets.failing(
                label, lambda: measure(eng, rep, cast(Project, p), top, args.profile, expectations, tally)))
        if not args.keep:
            shutil.rmtree(top, ignore_errors=True)
    saved = round(sum(cast(float, p["saved_s"]) for p in tally.projects), 1)
    rep.extra.update(work=str(work), profile=args.profile, sets=list(args.sets), scripts=tally.scripts,
                     projects=tally.projects, saved_s=saved)
    print(table(tally.projects), flush=True)
    rep.finish()


# The selftest.

def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def refused(text: str) -> str:
    try:
        parse(Input, text, SCRIPT)
    except SystemExit as e:
        return str(e)
    return ""


def test_data() -> None:
    data, apps = load_corpus(), load_apps()
    expect(len(data.crates) == 100, f"the corpus is 100 crates, not {len(data.crates)}")
    pins = [(c.name, c.version) for c in data.crates]
    expect(len(set(pins)) == len(pins), "each crate of the corpus is pinned once")
    expect(all(c.version and c.why for c in data.crates), "every crate of the corpus has a version and a reason")
    kinds = {c.expect for c in data.crates}
    expect({"pure", "cc", "rustc", "pkg-config", "env"} <= kinds, f"the corpus covers each main kind: {sorted(kinds)}")
    listed = [(s.name, s.version) for s in apps.scripts if s.version]
    expect(len(set(listed)) == len(listed) and not set(listed) & set(pins),
           "apps.json pins no script twice and none corpus.json pins")
    expect(all(s.why for s in apps.scripts), "every script of apps.json has a reason")


def test_input() -> None:
    given = parse(Input, '{"engine": {"kind": "v2", "blaze": "blaze"}, "out": "/tmp/x.json"}', SCRIPT)
    expect(given.sets == ("crates", "apps") and given.crates == () and given.profile == "dev", f"defaults: {given}")
    expect(given.apps == reachable.DEFAULT_PROJECTS, "the apps are reachable.py's by default")
    for bad in ('{"engine": {"kind": "v2", "blaze": "b"}, "out": "", "speed": 1}',
                '{"engine": {"kind": "v2", "blaze": "b"}, "out": "", "sets": ["bins"]}',
                '{"engine": {"kind": "v2", "blaze": "b"}, "out": "", "profile": "bench"}'):
        expect(refused(bad).startswith(SCRIPT), f"{bad} is refused")


def test_lines() -> None:
    expect(package_of("t", "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.229") == ("serde", "1.0.229"),
           "a registry package id")
    expect(package_of("t", "path+file:///w/foo#0.1.0") == ("foo", "0.1.0"), "a path package id named by its directory")
    expect(package_of("t", "path+file:///w/serde-1.0.229#serde@1.0.229") == ("serde", "1.0.229"), "a path package id")
    expect(package_of("t", "libc 0.2.191 (registry+https://github.com/rust-lang/crates.io-index)") == ("libc", "0.2.191"),
           "an old package id")
    reg = "registry+https://github.com/rust-lang/crates.io-index"
    lines = [
        {"reason": EXECUTED, "package_id": f"{reg}#serde_json@1.0.154", "out_dir": "/t/a/out"},
        {"reason": SCRIPT_LINE, "package_id": f"{reg}#serde_json@1.0.154", "out_dir": "/t/a/out", "purity": "pure",
         "said": None, "identity": "ab12", "hit": True},
        {"reason": EXECUTED, "package_id": f"{reg}#libc@0.2.191", "out_dir": "/t/b/out"},
        {"reason": SCRIPT_LINE, "package_id": f"{reg}#libc@0.2.191", "out_dir": "/t/b/out", "purity": "impure",
         "said": "`main` → `rustc_version`, which reads the variable `RUSTC`", "identity": None, "hit": False},
    ]
    stdout = "\n".join(["Compiling x", *(json.dumps(line) for line in lines), '{"reason": "blaze-work"}'])
    runs = script_runs("t", stdout)
    expect([(r.name, r.purity.kind, r.purity.hit) for r in runs] == [("serde_json", "pure", True), ("libc", "impure", False)],
           f"script lines read: {runs}")
    try:
        script_runs("t", "\n".join(json.dumps(line) for line in lines[:3]))
        expect(False, "a script run with no purity line is a Failed")
    except engine.Failed as e:
        expect("1 of 2 build script runs" in str(e), f"the run without a line is named: {e}")
    for wrong in ({**lines[1], "purity": "maybe"}, {**lines[1], "identity": None}, {**lines[3], "said": ""},
                  {**lines[3], "hit": True}, {**lines[1], "hit": "yes"}):
        try:
            script_run("t", wrong)
            expect(False, f"{wrong} is refused")
        except engine.Failed:
            pass


def test_expectations() -> None:
    listed = (Expected("libc", "0.2.191", "rustc", "probes", "registry"), Expected("ripgrep", "", "process", "git", "memory"),
              Expected("libc", "0.2.100", "pure", "older", "registry"))
    e = Expectations(listed)
    expect(e.of("libc", "0.2.191", ()) == Expectation("rustc", "probes", "pinned"), "by name and version")
    expect(cast(Expectation, e.of("libc", "0.2.150", ())).by == "name" and cast(Expectation, e.of("libc", "0.2.150", ())).expect == "rustc",
           "by name, the first listed")
    expect(cast(Expectation, e.of("ripgrep", "14.1.0", ())).expect == "process", "any version")
    expect(cast(Expectation, e.of("zstd-sys", "2.0.0", ("cc", "pkg-config"))).expect == "cc", "cc before pkg-config")
    expect(cast(Expectation, e.of("x", "1.0.0", ("autocfg",))).expect == "rustc", "autocfg is rustc")
    expect(cast(Expectation, e.of("x", "1.0.0", ("pkg-config",))).expect == "pkg-config", "pkg-config")
    expect(e.of("x", "1.0.0", ("serde",)) is None, "nothing expected")


def test_judge() -> None:
    pure, impure = Purity("pure", identity="ab", hit=False), Purity("impure", said="reads `RUSTC`")
    rustc = Expectation("rustc", "probes", "pinned")
    expect(judge(pure, Expectation("pure", "prints", "pinned"), ())[0] == "pass", "pure as expected")
    expect(judge(impure, rustc, ())[0] == "pass", "impure as expected")
    expect(judge(pure, rustc, ())[0] == "fail", "a rustc script proved pure fails")
    verdict, detail = judge(pure, None, ("cc",))
    expect(verdict == "fail" and "C compiler" in detail, f"a cc script proved pure fails whatever is expected: {detail}")
    expect(judge(pure, Expectation("pure", "x", "name"), ("cmake",))[0] == "fail", "cmake too")
    expect(judge(impure, Expectation("pure", "prints", "pinned"), ())[0] == "fail", "an expected pure script not proved")
    expect(judge(Purity("unproved"), rustc, ())[0] == "error", "unproved is an error")
    expect(judge(impure, None, ())[0] == "pass" and judge(pure, None, ())[0] == "pass", "nothing expected")


def test_misses() -> None:
    def run(name: str, purity: Purity, out: str = "/o") -> ScriptRun:
        return ScriptRun(f"r#{name}@1.0.0", name, "1.0.0", out, purity)

    sandboxed = [run("a", Purity("pure", identity="1")), run("b", Purity("impure", said="x")), run("c", Purity("pure", identity="2"))]
    expect(pure_misses(sandboxed, [run("a", Purity("pure", identity="1", hit=True)), run("b", Purity("impure", said="x")),
                                   run("c", Purity("pure", identity="2", hit=True))]) == [], "every pure key hit")
    misses = pure_misses(sandboxed, [run("a", Purity("pure", identity="1", hit=False)), run("b", Purity("pure", identity="3", hit=True)),
                                     run("c", Purity("pure", identity="9", hit=True))])
    expect(len(misses) == 3 and "did not hit" in misses[0] and "impure in the sandboxed build" in misses[1]
           and "identity 2" in misses[2], f"misses named: {misses}")
    expect(pure_misses(sandboxed[:1], []) == ["a@1.0.0 at /o: not in the build"], "a script missing")


def test_env() -> None:
    base = {"PATH": "/bin", PURE: "verify"}
    expect(pass_env(base, "sandboxed") == {"PATH": "/bin"}, "the sandboxed build has neither variable")
    expect(pass_env(base, "pure") == {"PATH": "/bin", RUSTFLAGS: "--cfg blaze_pure_measure_2"},
           "the pure build changes every compile key and does not verify")
    expect(pass_env(base, "verify") == {"PATH": "/bin", RUSTFLAGS: "--cfg blaze_pure_measure_3", PURE: "verify"},
           "the verify build changes them again and verifies")
    own = {"PATH": "/bin", RUSTFLAGS: "-C debuginfo=0"}
    expect(pass_env(own, "verify")[RUSTFLAGS] == "-C debuginfo=0 --cfg blaze_pure_measure_3",
           "the fresh cfg follows the project's own flags")
    expect(pass_env(own, "sandboxed")[RUSTFLAGS] == "-C debuginfo=0", "the sandboxed build keeps the project's flags")
    expect(base[PURE] == "verify" and RUSTFLAGS not in base, "the environment given is not changed")


def test_table() -> None:
    rows: list[dict[str, report.Number]] = [
        dict(project="serde_json@1.0.154", scripts=2, pure=1, hits=1, held=2, seconds_sandboxed=40.0, seconds_control=40.0,
             seconds_pure=38.5, saved_s=1.5, verified=True),
        dict(project="libc@0.2.191", scripts=1, pure=0, hits=0, held=1, seconds_sandboxed=20.0, seconds_control=20.0,
             seconds_pure=20.1, saved_s=-0.1, verified=False)]
    shown = table(rows).splitlines()
    expect(len(shown) == 4 and shown[0].startswith("project") and shown[-1].startswith("total"), f"the table: {shown}")
    expect("3/3" in shown[-1] and "1.4" in shown[-1] and "1/2" in shown[-1], f"the totals: {shown[-1]}")


def selftest() -> None:
    for test in (test_data, test_input, test_lines, test_expectations, test_judge, test_misses, test_env, test_table):
        test()
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
