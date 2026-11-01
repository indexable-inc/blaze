"""oracle.py: the invalidation oracle. After every edit, the incremental result must equal a
build from scratch of the same sources (docs/v2/plan/lanes/proof.md).

    uv run --frozen python harness/v2/oracle.py run '{"engine": {"kind": "v1", "blaze": "target/release/blaze"}}'
    uv run --frozen python harness/v2/oracle.py run '{"engine": {...}, "rounds": 2, "steps": 30, "kinds": ["closure"]}'
    uv run --frozen python harness/v2/oracle.py replay '{"engine": {...}, "found": "OUT/found/906-03.json"}'
    uv run --frozen python harness/v2/oracle.py selftest

The workspace is fixture.py's: three crates whose source is a vector of slots, one for each
kind of edit of docs/v2/plan/lanes/items.md section 6 (bodies, signatures, impls added and removed,
renames, moves, formatting, comments, cfgs and features, closure captures, a file `include_str!` reads deleted,
whitespace in literals, names `type_name` and `module_path!` read, positions `line!` and
`#[track_caller]` read, the environment of a build). A sequence starts from a state and edits
one slot at a time. Each step:

- the incremental build: one daemon kept for the whole sequence, with its home and store,
  builds the edited source (`blaze build`, then `blaze test`);
- the build from scratch: a daemon with an empty home and a store holding only the standard
  library (`seed_std`), into an empty target directory, of the same source at the same path.

A step fails when the incremental build and the one from scratch differ in anything: whether
the source is refused and with which errors, what the program prints and its exit status, the
outcome of every test, with `bytes` every output file byte for byte, and where the engine says
them (v2) the codegen input of every function and the query database's trace of both builds,
which `blaze-v2-proof` compares to name the query that went stale and the read it missed. That
is under-invalidation, and the whole point. It is unmeasured where either build was not blaze's (v1 hands some to cargo).
For a slot with a case in `blaze-v2-proof`'s `nameres` (the `nameres` regression edits each),
the traces are also held to what the facts say that edit changes: more is `over`, less is stale.
Apart from that:

- `refuses/...` fails where a state fixture.py marks as one the compiler must refuse
  (`fixture.rejects`: a deleted `include_str!` file, a non-`Send` type sent) was built: both
  builds have to refuse it;
- `over/...` fails where an edit docs/v2/plan/goals/targets.md says compiles nothing (`fixture.Slot.budget_zero`:
  formatting, comments, a rename or move nothing reads, PATH) compiled something. What every
  other kind of edit compiled is measured and summed by kind in the result (`work_by_kind`).

Sequences: `regressions` (fixture.REGRESSIONS: #906, #1325, #1326, #805, names, positions,
resolution, features), then `rounds` random sequences of `steps` edits from `seed` (round
`first` on), of the `kinds` given (all when none). A failing step is filed under `OUT/found/`
with every state of its sequence; `replay` builds them again with fresh daemons. With `reduce`
above 0 a finding is replayed as the last two states alone and then with each slot the edit did
not touch put back to its default, as long as it still fails, at most `reduce` replays.

Input (JSON inline, a file's path, or `-`):
    engine        the blaze under test (engine.EngineSpec)
    out           the result's path (report.py); found sequences go beside it, in `found/`
    work          where daemons' homes, stores and targets go (a new directory under /tmp)
    seed, first, rounds, steps, kinds, regressions    as above
    profiles      "dev" and/or "release" (release adds the inlining edges of section 6.1)
    tests         run `blaze test` too (default true)
    bytes         compare output files byte for byte (default: on for v2, off for v1, whose
                  incremental programs are spliced)
    seed_std      give each build from scratch a store with the standard library alone
    jobs          sequences run at once, each with daemons of its own
    proof         the `blaze-v2-proof` binary (crates/blaze-v2-proof), which compares two traces
                  and counts reruns for nothing (default: beside `engine.blaze`); needed only
                  when the engine gives traces
    reduce, keep
"""

import concurrent.futures
import json
import os
import re
import shutil
import sys
import tempfile
from collections import Counter
from dataclasses import asdict, dataclass, field
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Literal, cast

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]

import blaze_cli  # noqa: E402
import engine  # noqa: E402
import fixture  # noqa: E402
import report  # noqa: E402
from typed_input import fail, parse  # noqa: E402

SCRIPT = "oracle.py"
BUILD_SECS = 1800
RUN_SECS = 120
type Profile = Literal["dev", "release"]
SEED = {
    "Cargo.toml": '[package]\nname = "std_seed"\nversion = "0.1.0"\nedition = "2021"\n',
    "src/main.rs": 'fn main() {\n    println!("{:?}", std::env::args().count());\n}\n',
}


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str = ""
    work: str = ""
    seed: str = "oracle"
    first: int = 0
    rounds: int = 4
    steps: int = 12
    kinds: tuple[str, ...] = ()
    regressions: tuple[str, ...] = tuple(fixture.REGRESSIONS)
    profiles: tuple[Profile, ...] = ("dev",)
    tests: bool = True
    bytes: bool | None = None
    seed_std: bool = True
    jobs: int = 1
    reduce: int = 4
    proof: str = ""
    keep: bool = False


@dataclass(frozen=True)
class Replay:
    engine: engine.EngineSpec
    found: str
    out: str = ""
    work: str = ""
    profiles: tuple[Profile, ...] = ("dev",)
    tests: bool = True
    bytes: bool | None = None
    seed_std: bool = True
    proof: str = ""
    keep: bool = False


@dataclass(frozen=True)
class Settings:
    spec: engine.EngineSpec
    profiles: tuple[Profile, ...]
    tests: bool
    bytes: bool
    # `blaze-v2-proof`, which compares the traces of two builds.
    proof: str
    keep: bool


@dataclass
class Built:
    """One side's build of a state: refused (with its errors) or built, with what its programs
    printed, its tests' outcomes and its output files' digests, by profile."""

    ok: bool = True
    errors: tuple[str, ...] = ()
    programs: dict[str, tuple[int, str, str]] = field(default_factory=dict[str, tuple[int, str, str]])
    tests: dict[str, object] = field(default_factory=dict[str, object])
    outputs: dict[str, dict[str, str]] = field(default_factory=dict[str, dict[str, str]])
    codegen: dict[str, str] | None = None
    trace: Path | None = None
    not_blazes: str | None = None
    said: str = ""


@dataclass
class Step:
    name: str
    verdict: report.Verdict
    detail: str = ""
    kind: str = "start"
    slot: str = ""
    edit: str = ""
    budget_zero: bool = False
    rejects: bool = False
    compiled: int | None = None
    reran: int | None = None
    needless: int | None = None
    link: str | None = None
    # Which builds built a state fixture.py marks as refused.
    refusal: str | None = None
    state: dict[str, int] = field(default_factory=dict[str, int])
    found: str | None = None


def profile_dir(profile: Profile) -> str:
    return "release" if profile == "release" else "debug"


def errors_of(stdout: str, ws: Path) -> tuple[str, ...]:
    """The errors of a build's `--message-format=json` in the files of `ws`: each as its code
    (or the start of its message when it has none), file and line."""
    root = str(ws).rstrip("/") + "/"
    keyed: set[str] = set()
    for line in stdout.splitlines():
        if not line.startswith("{"):
            continue
        try:
            m = json.loads(line)
        except ValueError:
            continue
        if m.get("reason") != "compiler-message" or m["message"]["level"] != "error":
            continue
        d = m["message"]
        spans = [s for s in d["spans"] if s["is_primary"]] or d["spans"]
        # A summary ("aborting due to") has no place.
        if not spans:
            continue
        path = spans[0]["file_name"]
        if os.path.isabs(path):
            if not path.startswith(root):
                continue
            path = os.path.relpath(path, ws)
        code = d["code"] and d["code"]["code"]
        keyed.add(f"{code or d['message'][:60]} {path}:{spans[0]['line_start']}")
    return tuple(sorted(keyed))


def first_line_apart(a: str, b: str) -> str:
    x, y = a.splitlines(), b.splitlines()
    for at, (p, q) in enumerate(zip(x, y)):
        if p != q:
            return f"line {at + 1}: {p!r} against {q!r}"
    return f"one has {len(x)} lines, the other {len(y)}"


def compare(a: Built, b: Built, names: tuple[str, str]) -> list[str]:
    """How `a` and `b` differ in anything: refused or not and with which errors, what the
    programs print, the tests' outcomes, the output files and the codegen inputs."""
    la, lb = names
    if a.ok != b.ok:
        said = "".join(f"\n{side}: {x.said[-1500:]}" for side, x in ((la, a), (lb, b)) if not x.ok)
        return [f"{la} {'built' if a.ok else 'refused'} it, {lb} {'built' if b.ok else 'refused'} it{said}"]
    if not a.ok:
        return [] if a.errors == b.errors else [f"both refused it, with other errors: {la} {list(a.errors)}, {lb} {list(b.errors)}"]
    out: list[str] = []
    for profile, program in a.programs.items():
        other = b.programs.get(profile)
        if other is None or program == other:
            continue
        if program[0] != other[0]:
            out.append(f"{profile}: the program exits {program[0]} under {la}, {other[0]} under {lb}")
        for stream, mine, theirs in (("stdout", program[1], other[1]), ("stderr", program[2], other[2])):
            if mine != theirs:
                out.append(f"{profile}: the program's {stream} differs: {first_line_apart(mine, theirs)}")
    for profile, tests in a.tests.items():
        other = b.tests.get(profile)
        if other is not None and tests != other:
            out.append(f"{profile}: tests: {blaze_cli.difference(tests, other)}")
    for profile, files in a.outputs.items():
        other = b.outputs.get(profile, {})
        apart = sorted(n for n in set(files) | set(other) if files.get(n) != other.get(n))
        if apart:
            out.append(f"{profile}: output files not the same bytes: {', '.join(apart[:6])}")
    if a.codegen is not None and b.codegen is not None and a.codegen != b.codegen:
        keys = sorted(k for k in set(a.codegen) | set(b.codegen) if a.codegen.get(k) != b.codegen.get(k))
        out.append(f"codegen inputs differ for {len(keys)} instances: {', '.join(keys[:8])}")
    return out


class Round:
    """One sequence's workspace and its incremental daemon; a build from scratch beside it at
    each step."""

    def __init__(self, eng: engine.Engine, it: Settings, work: Path, std_store: Path | None):
        self.eng, self.it, self.work, self.std_store = eng, it, work, std_store
        self.ws = work / "ws"
        self.inc_home, self.inc_store, self.inc_target = work / "hi", work / "si", work / "ti"
        self.extra_dir = work / "path-extra"
        for path in (self.ws, self.extra_dir):
            path.mkdir(parents=True, exist_ok=True)
        self.fresh_store(self.inc_store)
        self.files: dict[str, str | None] | None = None
        self.last_env: dict[str, str] | None = None
        # Why the engine cannot say what a build did, its codegen inputs or its trace, once said.
        self.work_gap: tuple[report.Verdict, str] | None = None
        self.codegen_gap: tuple[report.Verdict, str] | None = None
        self.trace_gap: tuple[report.Verdict, str] | None = None
        self.nameres_gap: tuple[report.Verdict, str] | None = None
        self.traces = 0
        self.last_trace: Path | None = None

    def fresh_store(self, store: Path) -> None:
        shutil.rmtree(store, ignore_errors=True)
        if self.std_store is not None:
            shutil.copytree(self.std_store, store, symlinks=True)

    def env_of(self, base: dict[str, str], state: fixture.State) -> dict[str, str]:
        env = dict(base)
        want = fixture.environment(state)
        for name in ("ORACLE_SCRIPT_VALUE", "ORACLE_SOURCE_ENV"):
            env.pop(name, None)
            value = want[name]
            if value is not None:
                env[name] = value
        if want["PATH_EXTRA"]:
            env["PATH"] = f"{self.extra_dir}{os.pathsep}{env.get('PATH', '')}"
        return env

    def gap(self, which: str, error: Exception) -> tuple[report.Verdict, str]:
        verdict: report.Verdict = "unmeasured" if isinstance(error, engine.Unmeasurable) else "error"
        return verdict, f"{which}: {error}"

    def side(self, label: str, env: dict[str, str], target: Path) -> Built:
        built = Built()
        for profile in self.it.profiles:
            release = ["--release"] if profile == "release" else []
            step = f"{label} ({profile})"
            ran = self.eng.build(f"{step}: build", self.ws, env, ["--message-format=json", *release], timeout=BUILD_SECS)
            built.not_blazes = built.not_blazes or self.eng.not_blazes(ran)
            if ran.returncode != 0:
                built.ok, built.said = False, ran.said()
                built.errors = errors_of(ran.stdout, self.ws)
                return built
            program = target / profile_dir(profile) / "oracle_app"
            done = engine.run(f"{step}: the program", [str(program)], self.ws, engine.scrubbed(), RUN_SECS)
            built.programs[profile] = (done.returncode, done.stdout, done.stderr)
            if self.it.bytes:
                built.outputs[profile] = engine.outputs(target, profile_dir(profile))
            if self.it.tests:
                built.tests[profile] = self.tests(step, env, target, release, built)
        return built

    def tests(self, step: str, env: dict[str, str], target: Path, release: list[str], built: Built) -> object:
        # JSON for blaze-work (engine.V2.work_since).
        args = ["--message-format=json", "--no-fail-fast", *release]
        kept_file = target / "blaze" / "tests.ndjson"
        kept_file.unlink(missing_ok=True)
        ran = self.eng.build(f"{step}: test", self.ws, env, args, command="test", timeout=BUILD_SECS)
        built.not_blazes = built.not_blazes or self.eng.not_blazes(ran)
        kept = blaze_cli.kept_passes(str(kept_file))
        return cast(object, blaze_cli.test_results(SimpleNamespace(returncode=ran.returncode, stdout=ran.stdout, kept=kept)))

    def codegen(self, env: dict[str, str]) -> dict[str, str] | None:
        if self.codegen_gap is not None:
            return None
        try:
            return self.eng.codegen_inputs(self.ws, env)
        except engine.Unmeasurable as e:
            self.codegen_gap = self.gap("codegen inputs", e)
            return None

    def traced(self, env: dict[str, str], name: str) -> Path | None:
        if self.trace_gap is not None:
            return None
        to = self.work / name
        try:
            self.eng.trace(self.ws, env, to)
            return to
        except engine.Unmeasurable as e:
            self.trace_gap = self.gap("the query database's trace", e)
            return None

    def compare_traces(self, inc: Path, scratch: Path, slot: str) -> tuple[list[str], int | None]:
        """What `blaze-v2-proof` finds stale in the incremental trace, and how many results it
        reran for nothing against the trace of the build before; and where the edit's slot has a
        case in blaze-v2-proof's `nameres`, what that case finds."""
        proof = Path(self.it.proof)
        if not proof.is_file():
            raise engine.Failed(f"proof: no `blaze-v2-proof` at {proof}, to compare the traces")
        given: dict[str, str] = {"incremental": str(inc), "scratch": str(scratch)}
        if self.last_trace is not None:
            given["before"] = str(self.last_trace)
            if slot:
                given["slot"] = slot
        ran = engine.run("comparing the traces", [str(proof), json.dumps(given)], self.work, engine.scrubbed(), RUN_SECS)
        if ran.returncode not in (0, 1):
            raise engine.Failed(f"comparing the traces: `blaze-v2-proof` exited {ran.returncode}:\n{ran.said()}")
        found: dict[str, Any] = json.loads(ran.stdout)
        stale = [f"the trace: {s['what']} {s['query']}: {s['detail']}" for s in found["stale"]]
        case: dict[str, Any] = found.get("nameres") or {}
        findings: list[dict[str, Any]] = case["findings"] if case else []
        for f in findings:
            line = f"the {case['slot']} case ({case['claim']}): {f['side'].lower()}: {f['detail']}"
            if f["side"] != "Unmeasured":
                stale.append(line)
            elif self.nameres_gap is None:
                self.nameres_gap = ("unmeasured", f"the nameres cases: {line}")
        work = found["work"]
        return stale[:12], None if work is None else work["needless"]

    def incremental(self, state: fixture.State) -> tuple[Built, engine.Work | None]:
        env = self.env_of(self.eng.env(self.inc_home, self.inc_store, self.inc_target), state)
        self.last_env = env
        mark = None
        if self.work_gap is None:
            try:
                mark = self.eng.mark(self.inc_home)
            except engine.Unmeasurable as e:
                self.work_gap = self.gap("what a build did", e)
        built = self.side("incremental", env, self.inc_target)
        work = None
        if mark is not None:
            try:
                work = self.eng.work_since(self.inc_home, mark)
            except engine.Unmeasurable as e:
                self.work_gap = self.gap("what a build did", e)
        if built.ok:
            built.codegen = self.codegen(env)
            self.traces += 1
            built.trace = self.traced(env, f"trace-i{self.traces}.cbor")
        return built, work

    def scratch(self, state: fixture.State) -> Built:
        home, store = self.work / "hs", self.work / "ss"
        shutil.rmtree(home, ignore_errors=True)
        self.fresh_store(store)
        # Byte for byte needs the same target path as the incremental build: that one waits aside.
        aside = self.work / "ti-aside"
        target = self.inc_target if self.it.bytes else self.work / "ts"
        self.inc_target.mkdir(parents=True, exist_ok=True)
        if self.it.bytes:
            os.rename(self.inc_target, aside)
        else:
            shutil.rmtree(target, ignore_errors=True)
        env = self.env_of(self.eng.env(home, store, target), state)
        try:
            built = self.side("from scratch", env, target)
            if built.ok:
                built.codegen = self.codegen(env)
                built.trace = self.traced(env, "trace-s.cbor")
            return built
        finally:
            try:
                self.eng.stop(self.ws, env)
            finally:
                shutil.rmtree(home, ignore_errors=True)
                shutil.rmtree(store, ignore_errors=True)
                shutil.rmtree(target, ignore_errors=True)
                if self.it.bytes:
                    os.rename(aside, self.inc_target)

    def step(self, state: fixture.State, name: str, edit: fixture.Edit | None) -> Step:
        self.files = fixture.write(self.ws, state, self.files)
        slot = fixture.BY_NAME[edit.slot] if edit else None
        out = Step(name=name, verdict="pass", kind=edit.kind if edit else "start", slot=edit.slot if edit else "",
                   edit=edit.label() if edit else "", budget_zero=bool(slot and slot.budget_zero),
                   rejects=fixture.rejects(state), state=dict(state))
        try:
            inc, work = self.incremental(state)
            scratch = self.scratch(state)
            stale: list[str] = []
            if inc.trace is not None and scratch.trace is not None:
                stale, out.needless = self.compare_traces(inc.trace, scratch.trace, edit.slot if edit else "")
        except engine.Failed as e:
            out.verdict, out.detail = "error", str(e)
            return out
        if self.last_trace is not None:
            self.last_trace.unlink(missing_ok=True)
        self.last_trace = inc.trace
        if work is not None:
            out.compiled, out.reran, out.link = work.compiled, work.reran, work.link
        if inc.not_blazes or scratch.not_blazes:
            out.verdict = "unmeasured"
            out.detail = f"not blaze's: {inc.not_blazes or scratch.not_blazes}"
        else:
            apart = compare(inc, scratch, ("the incremental build", "the build from scratch")) + stale
            # An edit that laid its image out whole again instead of patching it is a finding,
            # never a time (AGENTS.md, no-fallback), as bench.py's `failed_link`.
            if out.kind != "start" and out.link and out.link.startswith(("full link", "placed whole")):
                apart.insert(0, out.link)
            if apart:
                out.verdict, out.detail = "fail", "\n".join(apart)
        if out.rejects:
            built = [label for label, b in (("the incremental build", inc), ("the build from scratch", scratch)) if b.ok]
            if built:
                out.refusal = f"{' and '.join(built)} built a program fixture.py marks as refused"
        return out

    def close(self) -> None:
        if self.last_env is not None:
            try:
                self.eng.stop(self.ws, self.last_env)
            except engine.Failed:
                pass
        if not self.it.keep:
            shutil.rmtree(self.work, ignore_errors=True)


@dataclass(frozen=True)
class Sequence:
    name: str
    start: dict[str, int]
    # The edits in order; or none, and `steps` random edits from `seed`.
    edits: tuple[fixture.Edit, ...] = ()
    seed: str = ""
    steps: int = 0
    kinds: tuple[str, ...] = ()


@dataclass
class Ran:
    steps: list[Step]
    gaps: list[tuple[report.Verdict, str]]


def run_sequence(eng: engine.Engine, it: Settings, seq: Sequence, work: Path, std_store: Path | None,
                 found_dir: Path | None) -> Ran:
    """The steps of one sequence under one fresh incremental daemon. A failing step is filed
    under `found_dir` with every state of the sequence up to it."""
    round_ = Round(eng, it, work, std_store)
    steps: list[Step] = []
    try:
        state = dict(seq.start)
        history = [dict(state)]
        steps.append(round_.step(state, f"{seq.name}/00 start", None))
        edits = iter(seq.edits) if seq.edits else None
        edit_of = fixture.editor(seq.seed, seq.kinds) if not seq.edits else None
        for at in range(1, (len(seq.edits) if seq.edits else seq.steps) + 1):
            if edits is not None:
                edit = next(edits)
                state[edit.slot] = edit.new
            else:
                assert edit_of is not None
                edit = edit_of(state)
            history.append(dict(state))
            step = round_.step(state, f"{seq.name}/{at:02d} {edit.slot}", edit)
            if step.verdict == "fail" and found_dir is not None:
                step.found = str(file_found(found_dir, seq.name, at, step, history))
            steps.append(step)
            if step.verdict == "error":
                break
    finally:
        round_.close()
    gaps = [g for g in (round_.work_gap, round_.codegen_gap, round_.trace_gap, round_.nameres_gap) if g is not None]
    return Ran(steps, gaps)


def file_found(directory: Path, name: str, at: int, step: Step, history: list[dict[str, int]]) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / f"{re.sub(r'[^\w.-]+', '-', name)}-{at:02d}.json"
    path.write_text(json.dumps(dict(name=step.name, edit=step.edit, detail=step.detail, history=history), indent=1) + "\n")
    return path


def replay_states(eng: engine.Engine, it: Settings, states: list[dict[str, int]], work: Path,
                  std_store: Path | None) -> Step | None:
    """The first step of `states` (each after the first an edit of the one before) whose
    incremental build differs from the one from scratch, under fresh daemons; None if none."""
    edits: list[fixture.Edit] = []
    for before, after in zip(states, states[1:]):
        for slot in fixture.SLOTS:
            if before[slot.name] != after[slot.name]:
                edits.append(fixture.Edit(slot.name, slot.kind, before[slot.name], after[slot.name]))
    seq = Sequence("replay", states[0], tuple(edits))
    # One edit a step: a step that changes several slots is replayed as one edit each.
    ran = run_sequence(eng, it, seq, Path(tempfile.mkdtemp(prefix="r", dir=work)), std_store, None)
    return next((s for s in ran.steps if s.verdict == "fail"), None)


def reduce(eng: engine.Engine, it: Settings, history: list[dict[str, int]], budget: int, work: Path,
           std_store: Path | None) -> tuple[list[dict[str, int]], str]:
    """A shorter sequence that still fails, and how it was found."""
    if budget <= 0:
        return history, "not reduced"
    pair = history[-2:]
    tries = 1
    if replay_states(eng, it, pair, work, std_store) is None:
        return history, "not alone: the two last states alone build the same; the steps before matter"
    defaults = fixture.default_state()
    before, after = dict(pair[0]), dict(pair[1])
    for slot in fixture.SLOTS:
        if tries >= budget:
            break
        name = slot.name
        if before[name] != after[name] or before[name] == defaults[name]:
            continue
        b, a = before | {name: defaults[name]}, after | {name: defaults[name]}
        tries += 1
        if fixture.rejects(b) or fixture.rejects(a):
            continue
        if replay_states(eng, it, [b, a], work, std_store) is not None:
            before, after = b, a
    return [before, after], f"alone, after {tries} replays"


def std_seed(eng: engine.Engine, work: Path) -> Path:
    """A store holding the standard library alone, from a build of a workspace that is not the
    fixture: every build from scratch starts from a copy of it."""
    ws = work / "seed"
    for name, text in SEED.items():
        (ws / name).parent.mkdir(parents=True, exist_ok=True)
        (ws / name).write_text(text)
    store = work / "std-store"
    env = eng.env(work / "seed-home", store, work / "seed-target")
    try:
        ran = eng.build("the standard library's store", ws, env)
        if ran.returncode != 0:
            raise engine.Failed(f"the standard library's store: `blaze build` failed:\n{ran.said()}")
    finally:
        eng.stop(ws, env)
    shutil.rmtree(work / "seed-home", ignore_errors=True)
    shutil.rmtree(work / "seed-target", ignore_errors=True)
    return store


def settings(spec: engine.EngineSpec, profiles: tuple[Profile, ...], tests: bool,
             bytes_: bool | None, proof: str, keep: bool) -> Settings:
    if not profiles:
        fail(SCRIPT, "profiles: name at least one")
    proof_bin = Path(proof) if proof else Path(spec.blaze).parent / f"blaze-v2-proof{engine.EXE}"
    return Settings(spec, profiles, tests, spec.kind == "v2" if bytes_ is None else bytes_,
                    str(proof_bin.resolve()), keep)


def work_root(given: str) -> Path:
    # A daemon's socket is under its home, and a socket's path has to be short.
    root = Path(given) if given else Path(tempfile.mkdtemp(prefix="bz-oracle-", dir="/tmp"))
    root.mkdir(parents=True, exist_ok=True)
    return root.resolve()


def record(out: report.Report, ran: Ran, gaps: dict[str, tuple[report.Verdict, str]],
           by_kind: dict[str, list[int]], refused: list[Step]) -> None:
    for step in ran.steps:
        numbers: dict[str, report.Number] = dict(kind=step.kind, compiled=step.compiled, reran=step.reran,
                                                 needless=step.needless, link=step.link)
        detail = step.edit + (f"\n{step.detail}" if step.detail else "") + (f"\nfiled: {step.found}" if step.found else "")
        out.add(f"oracle/{step.name}", step.verdict, detail.strip(), **numbers)
        if step.rejects and step.verdict != "error":
            refused.append(step)
            out.add(f"refuses/{step.name}", "fail" if step.refusal else "pass",
                    f"{step.edit}\n{step.refusal or 'both builds refused it'}".strip(), kind=step.kind)
        if step.kind != "start" and step.compiled is not None and step.verdict != "error" and not step.rejects:
            by_kind.setdefault(step.kind, []).append(step.compiled)
            if step.budget_zero and step.compiled > 0:
                out.add(f"over/{step.name}", "fail",
                        f"{step.edit}: docs/v2/plan/goals/targets.md says this compiles nothing; it compiled {step.compiled} functions",
                        kind=step.kind, compiled=step.compiled, budget=0)
    for verdict, why in ran.gaps:
        gaps.setdefault(why.split(":")[0], (verdict, why))


def run(it: Input) -> None:
    if it.rounds < 0 or it.steps < 1 or it.first < 0 or it.jobs < 1 or it.reduce < 0:
        fail(SCRIPT, "rounds and first are at least 0; steps and jobs at least 1; reduce at least 0")
    unknown = [k for k in it.kinds if k not in fixture.KINDS] + [r for r in it.regressions if r not in fixture.REGRESSIONS]
    if unknown:
        fail(SCRIPT, f"no kind or regression {unknown}: kinds {list(fixture.KINDS)}, regressions {list(fixture.REGRESSIONS)}")
    s = settings(it.engine, it.profiles, it.tests, it.bytes, it.proof, it.keep)
    out = report.Report(SCRIPT, it.engine, it.out)
    root = work_root(it.work)
    print(f"work: {root}", flush=True)
    # Without `out` the findings stay in the work directory, which then is not removed.
    found_dir = (Path(it.out).resolve().parent if it.out else root) / "found"
    try:
        eng = engine.Engine.of(it.engine)
        std_store = std_seed(eng, root) if it.seed_std else None
    except engine.Failed as e:
        out.add("oracle/setup", "error", str(e))
        out.finish()
    sequences = [Sequence(name, fixture.default_state(), tuple(fixture.regression(name))) for name in it.regressions]
    sequences += [Sequence(f"{it.seed}-{n}", fixture.random_state(f"{it.seed}-{n}"), seed=f"{it.seed}-{n}",
                           steps=it.steps, kinds=it.kinds) for n in range(it.first, it.first + it.rounds)]
    gaps: dict[str, tuple[report.Verdict, str]] = {}
    by_kind: dict[str, list[int]] = {}
    refused: list[Step] = []
    findings: list[Step] = []
    with concurrent.futures.ProcessPoolExecutor(it.jobs) as pool:
        futures = [pool.submit(run_sequence, eng, s, seq, root / f"q{at}", std_store, found_dir)
                   for at, seq in enumerate(sequences)]
        for future in futures:
            ran = future.result()
            record(out, ran, gaps, by_kind, refused)
            findings += [step for step in ran.steps if step.verdict == "fail" and step.found]
    built = sum(step.refusal is not None for step in refused)
    out.add("refuses/all", "fail" if built else "pass",
            f"{len(refused)} states fixture.py marks as refused, {built} built", steps=len(refused), built=built)
    for verdict, why in gaps.values():
        out.add(f"oracle/{why.split(':')[0]}", verdict, why)
    if not s.bytes:
        out.add("oracle/bytes", "unmeasured", "output files were not compared byte for byte (bytes: false)")
    for step in findings[:3] if it.reduce else []:
        assert step.found is not None
        history = json.loads(Path(step.found).read_text())["history"]
        try:
            states, how = reduce(eng, s, history, it.reduce, root, std_store)
        except engine.Failed as e:
            states, how = history, f"not reduced: {e}"
        Path(step.found).write_text(json.dumps(dict(json.loads(Path(step.found).read_text()), reduced=states, how=how),
                                               indent=1) + "\n")
        print(f"reduced {step.found}: {how}", flush=True)
    for kind, counts in sorted(by_kind.items()):
        zero = all(slot.budget_zero for slot in fixture.SLOTS if slot.kind == kind)
        over = sum(c > 0 for c in counts)
        out.add(f"over/{kind}", "fail" if zero and over else "pass",
                f"{len(counts)} edits, {over} compiled something" + (" (budget: nothing)" if zero else " (measured)"),
                edits=len(counts), mean=round(sum(counts) / len(counts), 1), most=max(counts), budget=0 if zero else None)
    out.extra.update(work_by_kind={k: dict(Counter(v)) for k, v in by_kind.items()}, sequences=len(sequences),
                     settings=asdict(s), found=[step.found for step in findings])
    if not it.keep and (it.out or not findings):
        shutil.rmtree(root, ignore_errors=True)
    out.finish()


def replay(it: Replay) -> None:
    s = settings(it.engine, it.profiles, it.tests, it.bytes, it.proof, it.keep)
    out = report.Report(SCRIPT, it.engine, it.out)
    found = json.loads(Path(it.found).read_text())
    states: list[dict[str, int]] = found.get("reduced") or found["history"]
    try:
        eng = engine.Engine.of(it.engine)
        root = work_root(it.work)
        std_store = std_seed(eng, root) if it.seed_std else None
        step = replay_states(eng, s, states, root, std_store)
    except engine.Failed as e:
        out.add(f"replay/{Path(it.found).stem}", "error", str(e))
        out.finish()
    out.add(f"replay/{Path(it.found).stem}", "fail" if step else "pass",
            f"{step.name}: {step.edit}\n{step.detail}" if step else f"{len(states)} states, every step the same")
    if not it.keep:
        shutil.rmtree(root, ignore_errors=True)
    out.finish()


def selftest() -> list[str]:
    wrong = fixture.selftest()
    a = Built(programs={"dev": (0, "x\ny\n", "")})
    b = Built(programs={"dev": (0, "x\nz\n", "")})
    if not compare(a, b, ("a", "b")) or compare(a, a, ("a", "b")):
        wrong.append("compare does not tell two programs apart, or tells one from itself")
    refused = Built(ok=False, errors=("E0277 app/src/main.rs:3",))
    moved = Built(ok=False, errors=("E0277 app/src/main.rs:4",))
    if not compare(refused, moved, ("a", "b")) or compare(refused, refused, ("a", "b")):
        wrong.append("compare: errors on other lines are the same, or one refusal differs from itself")
    if not compare(a, refused, ("a", "b")):
        wrong.append("compare: a program built and one refused are the same")
    stdout = "\n".join(json.dumps({"reason": "compiler-message", "message": {
        "level": level, "code": {"code": "E0277"}, "message": "not Send",
        "spans": [{"is_primary": True, "file_name": file, "line_start": 9}]}})
        for level, file in (("error", "app/src/main.rs"), ("warning", "app/src/lib.rs"), ("error", "/elsewhere/x.rs")))
    if errors_of(stdout, Path("/w")) != ("E0277 app/src/main.rs:9",):
        wrong.append(f"errors_of: {errors_of(stdout, Path('/w'))}")
    return wrong


def main() -> None:
    match sys.argv[1:]:
        case ["selftest"] | ["selftest", _]:
            wrong = selftest()
            for line in wrong:
                print(line)
            print(f"{SCRIPT} selftest: {len(wrong)} wrong")
            sys.exit(1 if wrong else 0)
        case ["run", text] | [text]:
            run(parse(Input, text, SCRIPT))
        case ["replay", text]:
            replay(parse(Replay, text, SCRIPT))
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} run|replay JSON, or selftest (see the top of the file)")


if __name__ == "__main__":
    main()
