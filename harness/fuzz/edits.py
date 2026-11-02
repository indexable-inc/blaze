#!/usr/bin/env python3
# pyright: basic
"""edits.py: generated edits, what the daemon makes of each against a cold blaze build.

usage: edits.py smoke '{"profiles": ["dev"]}'                  a short run on fixed seeds, for CI
       edits.py run '{"seed": "S", "rounds": N, "steps": K}'      N programs from seed S, K edits each
       edits.py run '{"seed": "S", "first": R, "rounds": 1}'      round R of that run again
       edits.py check '{"files": ["FILE.rs"]}'                   the sequences under found/edits/, each again
       edits.py report '{"dir": "DIR"}'                          adds up the logs a run left in DIR

After the subcommand comes one JSON object (inline, the path of a file, or `-` for standard
input) with the fields of its class below (Smoke, Run, Check, Report); a field it does not have
is refused. Run it in the repository's Python workspace: `uv run --frozen python
harness/fuzz/edits.py ...` from the root.

A round is one generated program (`gen.py`) under one daemon. Each step edits the source, asks
the daemon for the program by `blaze build`, and builds the same source in an empty home
(`rebuild.Project.fresh`). A finding is a step whose program prints something else than the
fresh one, a step the daemon refused, a daemon that died, or one that panicked and went on. An
edit the fresh build turns down is not shown to the daemon. A profile picks the backend: v2
compiles dev with Cranelift and release with LLVM.

The edits:
  regen.run         the body of a case's `run`, generated anew
  regen.fn          the functions of a case from one on: bodies, signatures, more or fewer
  regen.constants   the constants of a case, and what uses them
  regen.adts        the types of a case, and everything after them
  const.value       the value of one constant or static, and nothing else
  layout.fields     the fields of a struct, or the variants of an enum, in another order
  layout.repr       `#[repr(C)]` put on a type or taken off
  body.literal      one number in one function
  fn.add, fn.signature, fn.remove    a function of the edit's own, and the call of it
  undo              the source of an earlier step again

BLAZE_BIN names the blaze under test. The exit status is 1 when there is a finding.
"""

import collections
import concurrent.futures
import copy
import json
import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from typed_input import fail, parse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))
sys.path.insert(0, HERE)
import gen
from rebuild import Project, scratch
from store import MANIFEST, PROFILE_ARGS, differences, edition_of

EDITS = os.path.join(HERE, "found", "edits")
# The line between one source and the next in a file under found/edits/.
THEN = "//// then"
SCALARS = {
    "bool",
    "f32",
    "f64",
    "i8",
    "i16",
    "i32",
    "i64",
    "i128",
    "isize",
    "u8",
    "u16",
    "u32",
    "u64",
    "u128",
    "usize",
}
# The function `fn.add` adds, in the shapes `fn.signature` goes through: its text and its call.
EXTRA = [
    ("#[inline(never)] pub fn {name}(x: u64) -> u64 {{ x.wrapping_mul({k}) }}", "feed({name}(3));"),
    (
        "#[inline(never)] pub fn {name}(x: u64, y: u8) -> u64 {{ x.wrapping_mul({k}) + y as u64 }}",
        "feed({name}(3, 4));",
    ),
    (
        "#[inline(never)] pub fn {name}(x: u32, y: [u64; 3]) -> (u64, u8) {{ (y[1] ^ {k}, x as u8) }}",
        "feed({name}(3, [1, 2, 3]).0);",
    ),
    ("#[inline(never)] pub fn {name}<T: Mix>(x: T) -> T {{ x.mix(); feed({k}); x }}", "{name}(3u16).mix();"),
    (
        "#[inline(never)] pub fn {name}(x: &mut Vec<u64>, y: Option<Box<u8>>) {{ x.push({k} + y.map_or(0, |it| *it as u64)); }}",
        "{{ let mut seen = Vec::new(); {name}(&mut seen, Some(Box::new(5))); seen.mix(); }}",
    ),
]


class Case:
    """One case of the program as it is now: its lines, and how to generate it again."""

    def __init__(self, name, index, lines, phases):
        self.name, self.index, self.lines, self.phases = name, index, lines, phases
        self.extras = {}


class Editor:
    """Makes the next source from the one before. Every edit returns what it did, or None."""

    def __init__(self, seed, cases):
        self.seed, self.r = seed, random.Random(f"{seed}/edits")
        self.cases, self.count = [], 0
        for index in range(cases):
            lines, made = gen.case(seed, index)
            # gen.case records the retry's index dynamically on its generator.
            self.cases.append(Case(f"c{index}", vars(made)["index"], lines, made.phases))
        self.history = [copy.deepcopy(self.cases)]
        self.kinds = [
            (3, "regen.run", self.regen_run),
            (3, "regen.fn", self.regen_fn),
            (1.5, "regen.constants", self.regen_constants),
            (1.5, "regen.adts", self.regen_adts),
            (3, "const.value", self.const_value),
            (2, "layout.fields", self.layout_fields),
            (2, "layout.repr", self.layout_repr),
            (3, "body.literal", self.body_literal),
            (2, "fn.add", self.fn_add),
            (2, "fn.signature", self.fn_signature),
            (1.5, "fn.remove", self.fn_remove),
            (1.5, "undo", self.undo),
        ]

    def text(self):
        return gen.render([(case.name, case.lines) for case in self.cases])

    def step(self):
        """Edits the source. Returns the kind of edit and what it changed, or None when it did not apply."""
        before = copy.deepcopy(self.cases)
        _, kind, edit = self.r.choices(self.kinds, weights=[weight for weight, _, _ in self.kinds])[0]
        self.count += 1
        what = edit()
        if what is None or self.text() == gen.render([(case.name, case.lines) for case in before]):
            self.cases = before
            return None
        return kind, what

    def keep(self):
        self.history.append(copy.deepcopy(self.cases))

    def back(self):
        self.cases = copy.deepcopy(self.history[-1])

    def regen(self, phase_of):
        case = self.r.choice(self.cases)
        phase = phase_of(case)
        lines = phase and gen.variant(self.seed, case.index, case.name, (phase, self.count))
        if not lines:
            return None
        case.lines, case.extras = lines, {}
        return f"{case.name} from {phase}"

    def regen_run(self):
        return self.regen(lambda case: "run")

    def regen_fn(self):
        return self.regen(lambda case: self.r.choice([name for name in case.phases if name.startswith("fn")]))

    def regen_constants(self):
        return self.regen(lambda case: "constants")

    def regen_adts(self):
        return self.regen(lambda case: "adts")

    def lines_like(self, pattern):
        """Every `(case, line number, match)` of a pattern in the program."""
        return [
            (case, at, found)
            for case in self.cases
            for at, line in enumerate(case.lines)
            if (found := re.match(pattern, line))
        ]

    def const_value(self):
        items = [
            it
            for it in self.lines_like(r"(\s*(?:const|static|static mut) \w+: (\w+) = )(.*);$")
            if it[2].group(2) in SCALARS
        ]
        if not items:
            return None
        case, at, found = self.r.choice(items)
        ty = found.group(2)
        value = self.r.choice(
            ["true", "false"]
            if ty == "bool"
            else ["0.5", "2.0", "-1.0", "1e9"]
            if ty in ("f32", "f64")
            else ["0", "1", "2", "7", "100", f"{ty}::MAX"]
        )
        case.lines[at] = f"{found.group(1)}{value};"
        return f"{case.name}: {case.lines[at].strip()}"

    def layout_fields(self):
        structs = self.lines_like(r"(\s*pub struct \w+(?:<[^>]*>)? \{ )(.*)( \})$")
        enums = self.lines_like(r"\s*pub enum \w+(?:<[^>]*>)? \{$")
        if not structs + enums:
            return None
        case, at, found = self.r.choice(structs + enums)
        if found.re.groups:
            fields = re.split(r", (?=pub \w+: )", found.group(2))
            fields = fields[1:] + fields[:1] if self.r.random() < 0.5 else fields[::-1]
            case.lines[at] = found.group(1) + ", ".join(fields) + found.group(3)
        else:
            end = next(i for i in range(at + 1, len(case.lines)) if case.lines[i].strip() == "}")
            case.lines[at + 1 : end] = case.lines[at + 2 : end] + case.lines[at + 1 : at + 2]
        return f"{case.name}: {case.lines[at].strip()[:60]}"

    def layout_repr(self):
        items = self.lines_like(r"(\s*)pub (?:struct|enum) \w+")
        if not items:
            return None
        case, at, found = self.r.choice(items)
        if case.lines[at - 1].strip().startswith("#[repr"):
            del case.lines[at - 1]
            return f"{case.name}: no repr on {case.lines[at - 1].strip()[:40]}"
        case.lines.insert(at, f"{found.group(1)}#[repr(C)]")
        return f"{case.name}: repr(C) on {case.lines[at + 1].strip()[:40]}"

    def body_literal(self):
        items = self.lines_like(r"(.*\bfeed\()(0x[0-9a-f]+)(\);.*)$")
        if not items:
            return None
        case, at, found = self.r.choice(items)
        case.lines[at] = f"{found.group(1)}{self.r.randrange(1 << 32):#x}{found.group(3)}"
        return f"{case.name}: line {at}"

    def put_extra(self, case, name, shape):
        """Writes the function `name` of a case in a shape, in place of the one it has if it has one."""
        self.drop_extra(case, name)
        text, call = (part.format(name=name, k=self.r.randrange(1, 1 << 30)) for part in EXTRA[shape])
        start = next(i for i, line in enumerate(case.lines) if line.strip() == "pub fn run() {")
        case.lines[start + 1 : start + 1] = [f"        {call}"]
        case.lines[0:0] = [f"    {text}"]
        case.extras[name] = shape

    def drop_extra(self, case, name):
        case.lines = [line for line in case.lines if f"{name}(" not in line and f"{name}<" not in line]
        case.extras.pop(name, None)

    def fn_add(self):
        case = self.r.choice(self.cases)
        name = f"extra{self.count}"
        self.put_extra(case, name, self.r.randrange(len(EXTRA)))
        return f"{case.name}: {name}"

    def with_extra(self):
        cases = [case for case in self.cases if case.extras]
        if not cases:
            return None, None
        case = self.r.choice(cases)
        return case, self.r.choice(sorted(case.extras))

    def fn_signature(self):
        case, name = self.with_extra()
        if not case:
            return None
        self.put_extra(case, name, (case.extras[name] + self.r.randrange(1, len(EXTRA))) % len(EXTRA))
        return f"{case.name}: {name}"

    def fn_remove(self):
        case, name = self.with_extra()
        if not case:
            return None
        self.drop_extra(case, name)
        return f"{case.name}: {name}"

    def undo(self):
        if len(self.history) < 2:
            return None
        back = self.r.randrange(len(self.history) - 1)
        self.cases = copy.deepcopy(self.history[back])
        return f"the source of step {back}"


PROGRAM = "fz"
# What a step whose source the fresh build turned down shows of it: it was not shown to the daemon.
NOT_FRESH = "not accepted by a fresh build"


class Daemon:
    """A cargo project of one generated program, the daemon that builds it after each edit, and
    fresh builds of it (rebuild.Project)."""

    def __init__(self, directory, edition, profile, text):
        self.dir = directory
        self.edition, self.profile = edition, profile
        self.ws = os.path.join(directory, "ws")
        os.makedirs(os.path.join(self.ws, "src"))
        Path(self.ws, "Cargo.toml").write_text(MANIFEST.format(edition=edition))
        self.main = os.path.join(self.ws, "src", "main.rs")
        Path(self.main).write_text(text)
        self.project = Project(self.ws, os.path.join(directory, "work"))
        self.log = self.project.started().log_path

    def sync(self, text):
        """Writes the source and asks the daemon for the program: `storelib.Built`."""
        Path(self.main).write_text(text)
        return self.project.build(PROGRAM, *PROFILE_ARGS[self.profile])

    def cold(self, text):
        """What a fresh build of the source does, or None when it turns the source down."""
        Path(self.main).write_text(text)
        return self.project.fresh(PROGRAM, *PROFILE_ARGS[self.profile])

    def alive(self):
        return self.project.daemon is not None and self.project.daemon.alive()

    def gone(self):
        """Whether the daemon has exited, given a moment to: its reply can come before it does."""
        daemon = self.project.daemon
        if daemon is None:
            return True
        try:
            daemon.process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            return False
        return True

    def close(self):
        if self.alive():
            self.project.stop()
        shutil.rmtree(self.dir, ignore_errors=True)


def panic_of(log):
    """The first panic in a daemon's log: where it was and what it said."""
    found = re.search(r"panicked at ([^\n]*)\n([^\n]*)", log)
    return f"{found.group(2)} (at {found.group(1).rstrip(':')})" if found else log[-600:]


def verdict(daemon, text, expected):
    """Shows the daemon a source. Returns what went wrong (None when nothing did) and the time."""
    began = time.time()
    built = daemon.sync(text)
    took = int((time.time() - began) * 1000)
    if (built.status != 0 or not built.executable) and daemon.gone():
        return "died: " + panic_of(Path(daemon.log).read_text()), took
    if built.status != 0 or not built.executable:
        return "refused: " + built.said.strip()[-300:], took
    found = differences(expected, daemon.project.ran(built))
    return (f"differs in {found[0][:1500]}" if found else None), took


def modules_of(cases, names=None):
    return [(case.name, case.lines) for case in cases if names is None or case.name in names]


def replay(work, edition, profile, sources):
    """A fresh daemon shown each of the sources in turn, each a list of modules. Returns what
    went wrong at the first step that did, or None."""
    texts = [gen.render(modules) for modules in sources]
    daemon = Daemon(tempfile.mkdtemp(prefix="replay-", dir=work), edition, profile, texts[0])
    try:
        for at, text in enumerate(texts):
            expected = daemon.cold(text)
            if expected is None:
                return f"step {at}: {NOT_FRESH}"
            wrong = verdict(daemon, text, expected)[0]
            if wrong:
                return f"step {at}: {wrong}"
        log = Path(daemon.log).read_text()
        return "panicked: " + panic_of(log) if "panicked at" in log else None
    finally:
        daemon.close()


def reduce_pair(work, edition, profile, before, after, budget):
    """Drops the cases the finding does not need from the source before the edit and the one after.
    Returns the two as modules, or None when a daemon that has only seen the one before gets
    the one after right: the finding needs the edits before it."""
    tests = [0]

    def fails(names):
        if tests[0] >= budget:
            return False
        tests[0] += 1
        return (
            replay(work, edition, profile, [modules_of(before, names), modules_of(after, names)]) is not None
        )

    names = [case.name for case in after if any(case.name == other.name for other in before)]
    if not fails(names):
        return None
    names = minimize(names, fails)
    return [modules_of(before, names), modules_of(after, names)]


def minimize(items, fails):
    """Delta debugging: a smaller list for which `fails` still holds."""
    n = 2
    while len(items) >= 2:
        size = max(len(items) // n, 1)
        chunks = [items[i : i + size] for i in range(0, len(items), size)]
        for i, chunk in enumerate(chunks):
            rest = [x for j, c in enumerate(chunks) if j != i for x in c]
            if fails(chunk):
                items, n = chunk, 2
                break
            if len(chunks) > 2 and fails(rest):
                items, n = rest, max(n - 1, 2)
                break
        else:
            if size == 1:
                break
            n = min(n * 2, len(items))
    return items


def write_found(tag, edition, profile, kind, what, wrong, sources, alone):
    """Files a finding under found/edits/: the sources a daemon is to be shown in turn."""
    os.makedirs(EDITS, exist_ok=True)
    path = os.path.join(EDITS, f"{tag}.rs")
    head = [
        f"// Found by edits.py: seed {tag}, edition {edition}, profile {profile}.",
        f"// The edit ({kind}): {what}",
        f"// Again: edits.py check '{json.dumps({'files': [os.path.basename(path)]})}'"
        if alone
        else "// Not alone: a daemon that has only built the first of these gets the second right. It took the edits before.",
        *("// " + line for line in wrong.splitlines()[:30]),
    ]
    parts = ["\n\n".join(gen.module(name, lines) for name, lines in modules) for modules in sources]
    Path(path).write_text("\n".join(head) + "\n" + f"\n{THEN}\n".join(parts) + "\n")
    return path


def read_found(path):
    """The sources of a file under found/edits/, each as modules, and the edition they are of."""
    text = Path(path).read_text()
    found_edition = re.search(r"edition (\d+)", text)
    if found_edition is None:
        fail("edits.py", f"files: {path} has no edition")
    edition = found_edition.group(1)
    sources = []
    for part in text.split(f"\n{THEN}\n"):
        found = re.findall(r"^mod (c\d+) \{\n    use super::\*;\n(.*?)^\}$", part, re.MULTILINE | re.DOTALL)
        sources.append([(name, body.rstrip("\n").split("\n")) for name, body in found])
    return sources, edition


def check(paths, profiles, work):
    """Each file given, or all of found/edits/: whether a daemon shown its sources in turn gets one wrong."""
    paths = paths or sorted(os.path.join(EDITS, name) for name in os.listdir(EDITS) if name.endswith(".rs")) if os.path.isdir(EDITS) else paths
    wrong = 0
    for path in paths:
        path = path if os.path.exists(path) else os.path.join(EDITS, path)
        sources, edition = read_found(path)
        for profile in profiles:
            said = replay(work, edition, profile, sources)
            wrong += said is not None
            print(
                f"{'WRONG    ' if said else 'same     '} {os.path.basename(path)} ({profile}) {(said or '').splitlines()[0][:200] if said else ''}",
                flush=True,
            )
    return wrong


def round_of(seed, cases, steps, profile, work, out, budget):
    """One program and its edits under one profile. Returns what each step came to."""
    editor = Editor(seed, cases)
    edition = edition_of(seed)
    daemon = Daemon(tempfile.mkdtemp(prefix=f"{profile}-", dir=work), edition, profile, editor.text())
    rows = []
    try:
        expected = daemon.cold(editor.text())
        if expected is None:
            raise SystemExit(f"the fresh build of the program as generated failed: {seed}")
        wrong, took = verdict(daemon, editor.text(), expected)
        rows.append({"kind": "start", "what": "", "how": "built", "ms": took, "wrong": wrong})
        for step in range(steps):
            if not daemon.alive():
                break
            before = copy.deepcopy(editor.cases)
            edit = editor.step()
            if edit is None:
                continue
            kind, what = edit
            expected = daemon.cold(editor.text())
            if expected is None or expected[0] == "timeout":
                rows.append({"kind": kind, "what": what, "how": NOT_FRESH, "ms": 0, "wrong": None})
                editor.back()
                continue
            wrong, took = verdict(daemon, editor.text(), expected)
            row = {"kind": kind, "what": what, "how": "built", "ms": took, "wrong": wrong}
            if wrong and wrong.startswith("differs"):
                pair = reduce_pair(work, edition, profile, before, editor.cases, budget)
                sources = pair or [modules_of(before), modules_of(editor.cases)]
                row["found"] = [
                    write_found(
                        f"{seed}-s{step}-{profile}", edition, profile, kind, what, wrong, sources, bool(pair)
                    )
                ]
            rows.append(row)
            editor.keep()
            # What the daemon has after it refused is from before the edit: the round is over.
            if wrong and not wrong.startswith("differs"):
                break
        log = Path(daemon.log).read_text()
        if "panicked at" in log and not any(row["wrong"] for row in rows):
            rows.append(
                {"kind": "end", "what": "", "how": "", "ms": 0, "wrong": "panicked: " + panic_of(log)}
            )
        if any(row["wrong"] for row in rows):
            shutil.copy(daemon.log, os.path.join(out, f"{seed}-{profile}.daemon.log"))
    finally:
        daemon.close()
    return rows


def one(job):
    seed, cases, steps, profiles, work, out, budget = job
    result = {"seed": seed, "profiles": {}}
    for profile in profiles:
        try:
            result["profiles"][profile] = round_of(seed, cases, steps, profile, work, out, budget)
        except SystemExit as error:
            result["profiles"][profile] = [
                {"kind": "start", "what": "", "how": "no daemon", "ms": 0, "wrong": f"died: {error}"}
            ]
    Path(out, f"{seed}.json").write_text(json.dumps(result))
    return result


def say(result):
    for profile, rows in result["profiles"].items():
        wrong = [row for row in rows if row["wrong"]]
        print(f"{result['seed']} {profile}: {len(rows) - 1} edits, {len(wrong)} findings", flush=True)
        for row in wrong:
            print(f"  {row['kind']} ({row['what']}): {row['wrong'].splitlines()[0][:200]}", flush=True)
            for path in row.get("found", []):
                print(f"    {path}", flush=True)


def report(directory):
    """Adds up the logs of a run. Returns the number of findings."""
    results = [
        json.loads(Path(directory, name).read_text())
        for name in sorted(os.listdir(directory))
        if name.endswith(".json")
    ]
    findings = 0
    for profile in sorted({profile for result in results for profile in result["profiles"]}):
        rows = [row for result in results for row in result["profiles"].get(profile, [])]
        edits = [
            row
            for row in rows
            if row["kind"] not in ("start", "end") and row["how"] != NOT_FRESH
        ]
        wrong = [row for row in rows if row["wrong"]]
        findings += len(wrong)
        kinds = collections.Counter(row["wrong"].split(":")[0].split(" in ")[0] for row in wrong)
        print(
            f"{profile}: {len(results)} programs, {len(edits)} edits shown to the daemon, "
            f"{sum(row['how'] == NOT_FRESH for row in rows)} edits a fresh build turned down, {len(wrong)} findings"
            + "".join(f", {count} {kind}" for kind, count in sorted(kinds.items()))
        )
        print("  edit                 shown  wrong")
        for kind in sorted({row["kind"] for row in edits}):
            of_kind = [row for row in edits if row["kind"] == kind]
            print(f"  {kind:20} {len(of_kind):5}  {sum(bool(row['wrong']) for row in of_kind):5}")
    return findings


SCRIPT = "edits.py"
type ProfileName = Literal["dev", "release"]


@dataclass(frozen=True)
class Smoke:
    """The fixed seeds of CI: 2 programs of 4 cases, 8 edits each, then whatever is under found/edits/ again."""

    profiles: tuple[ProfileName, ...] = ("dev", "release")
    jobs: int = 2
    # The builds a reduction may take.
    reduce: int = 12
    # Where the logs go, and the daemons' projects; a new temporary directory when not given.
    out: str | None = None
    work: str | None = None


@dataclass(frozen=True)
class Run:
    seed: str = "edits"
    # The round to begin at, to run one again.
    first: int = 0
    rounds: int = 20
    cases: int = 6
    steps: int = 12
    jobs: int = 2
    reduce: int = 12
    profiles: tuple[ProfileName, ...] = ("dev", "release")
    out: str | None = None
    work: str | None = None


@dataclass(frozen=True)
class Check:
    # Files under found/edits/ (or paths); none is all of them.
    files: tuple[str, ...] = ()
    profiles: tuple[ProfileName, ...] = ("dev", "release")
    work: str | None = None


@dataclass(frozen=True)
class Report:
    dir: str


def positive(**fields):
    """Stops at a field below 1, naming it."""
    for name, value in fields.items():
        if value < 1:
            fail(SCRIPT, f"{name} is at least 1, not {value}")


def directory(path, prefix):
    path = path or scratch(prefix)
    os.makedirs(path, exist_ok=True)
    return path


def rounds(seed, first, count, cases, steps, it):
    """Runs rounds `first` .. `first + count` of `seed` and reports them. Returns (findings, out, work)."""
    if not it.profiles:
        fail(SCRIPT, "profiles is empty: name at least one")
    positive(jobs=it.jobs, reduce=it.reduce)
    out, work = directory(it.out, "blaze-edits-out-"), directory(it.work, "blaze-edits-")
    jobs = [
        (f"{seed}-{index}", cases, steps, list(it.profiles), work, out, it.reduce)
        for index in range(first, first + count)
    ]
    with concurrent.futures.ProcessPoolExecutor(it.jobs) as pool:
        for result in pool.map(one, jobs):
            say(result)
    return report(out), out, work


def main():
    match sys.argv[1:]:
        case ["smoke", text]:
            it = parse(Smoke, text, SCRIPT)
            findings, out, work = rounds("smoke", 0, 2, 4, 8, it)
            findings += check([], list(it.profiles), work)
        case ["run", text]:
            it = parse(Run, text, SCRIPT)
            if it.first < 0:
                fail(SCRIPT, f"first is at least 0, not {it.first}")
            positive(rounds=it.rounds, cases=it.cases, steps=it.steps)
            findings, out, _ = rounds(it.seed, it.first, it.rounds, it.cases, it.steps, it)
        case ["check", text]:
            it = parse(Check, text, SCRIPT)
            if not it.profiles:
                fail(SCRIPT, "profiles is empty: name at least one")
            sys.exit(1 if check(list(it.files), list(it.profiles), directory(it.work, "blaze-edits-")) else 0)
        case ["report", text]:
            sys.exit(1 if report(parse(Report, text, SCRIPT).dir) else 0)
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} smoke|run|check|report JSON (see the top of the file)")
    print(f"logs: {out}")
    sys.exit(1 if findings else 0)


if __name__ == "__main__":
    main()
