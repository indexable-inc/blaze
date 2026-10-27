#!/usr/bin/env python3
"""edits.py: generated edits, what the daemon makes of each against a cold cargo build.

usage: edits.py smoke                                         a short run on fixed seeds, for CI
       edits.py run --seed S --rounds N [--steps K] [--jobs J]   N programs from seed S, K edits each
       edits.py run --seed S --first R --rounds 1                round R of that run again
       edits.py check [FILE.rs ...]                              the sequences under found/edits/, each again
       edits.py report DIR                                    adds up the logs a run left in DIR

A round is one generated program (`gen.py`) under a daemon that keeps its binary up to date,
as `splices.py` runs one. Each step edits the source, asks the daemon for the binary (which it
splices, patches or links in full, as it sees fit) and builds the same source with cargo from
nothing. A finding is a step whose binary prints something else than cargo's, a step the
daemon refused, a daemon that died, or one that panicked and went on. An edit rustc turns down is not shown to the daemon.

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

RA_JIT_BIN names the ra-jit under test. Under RA_JIT_CHECK_ALL the daemon also looks at what
no edit can have changed, and a change it finds there is a finding. The exit status is 1 when
there is a finding.
"""
import argparse, collections, concurrent.futures, copy, json, os, random, re, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))
sys.path.insert(0, HERE)
import gen, linkline
from difftest import Fixture, RA_JIT, TARGET_ARGS, built, sysroot_rlibs, minimize
from fuzz import BACKENDS, FOUND, MANIFEST, differences, edition_of, run

EDITS = os.path.join(FOUND, "edits")
# The line between one source and the next in a file under found/edits/.
THEN = "//// then"
SMOKE = {"seed": "smoke", "rounds": 2, "cases": 4, "steps": 8}
SCALARS = {"bool", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize"}
# The function `fn.add` adds, in the shapes `fn.signature` goes through: its text and its call.
EXTRA = [
    ("#[inline(never)] pub fn {name}(x: u64) -> u64 {{ x.wrapping_mul({k}) }}", "feed({name}(3));"),
    ("#[inline(never)] pub fn {name}(x: u64, y: u8) -> u64 {{ x.wrapping_mul({k}) + y as u64 }}", "feed({name}(3, 4));"),
    ("#[inline(never)] pub fn {name}(x: u32, y: [u64; 3]) -> (u64, u8) {{ (y[1] ^ {k}, x as u8) }}", "feed({name}(3, [1, 2, 3]).0);"),
    ("#[inline(never)] pub fn {name}<T: Mix>(x: T) -> T {{ x.mix(); feed({k}); x }}", "{name}(3u16).mix();"),
    ("#[inline(never)] pub fn {name}(x: &mut Vec<u64>, y: Option<Box<u8>>) {{ x.push({k} + y.map_or(0, |it| *it as u64)); }}",
     "{{ let mut seen = Vec::new(); {name}(&mut seen, Some(Box::new(5))); seen.mix(); }}"),
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
            self.cases.append(Case(f"c{index}", made.index, lines, made.phases))
        self.history = [copy.deepcopy(self.cases)]
        self.kinds = [
            (3, "regen.run", self.regen_run), (3, "regen.fn", self.regen_fn), (1.5, "regen.constants", self.regen_constants),
            (1.5, "regen.adts", self.regen_adts), (3, "const.value", self.const_value), (2, "layout.fields", self.layout_fields),
            (2, "layout.repr", self.layout_repr), (3, "body.literal", self.body_literal), (2, "fn.add", self.fn_add),
            (2, "fn.signature", self.fn_signature), (1.5, "fn.remove", self.fn_remove), (1.5, "undo", self.undo),
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
        return [(case, at, found) for case in self.cases for at, line in enumerate(case.lines) if (found := re.match(pattern, line))]

    def const_value(self):
        items = [it for it in self.lines_like(r"(\s*(?:const|static|static mut) \w+: (\w+) = )(.*);$") if it[2].group(2) in SCALARS]
        if not items:
            return None
        case, at, found = self.r.choice(items)
        ty = found.group(2)
        value = self.r.choice(["true", "false"] if ty == "bool" else ["0.5", "2.0", "-1.0", "1e9"] if ty in ("f32", "f64") else ["0", "1", "2", "7", "100", f"{ty}::MAX"])
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
            case.lines[at + 1:end] = case.lines[at + 2:end] + case.lines[at + 1:at + 2]
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
        case.lines[start + 1:start + 1] = [f"        {call}"]
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


class Daemon:
    """A cargo project, a daemon that keeps its binary up to date, and cargo's own build beside it."""

    def __init__(self, directory, edition, backend, text):
        self.dir = directory
        self.ws, self.work, self.cold = (os.path.join(directory, name) for name in ("ws", "work", "cold"))
        os.makedirs(os.path.join(self.ws, "src"))
        os.makedirs(self.work)
        open(os.path.join(self.ws, "Cargo.toml"), "w").write(MANIFEST.format(edition=edition))
        self.main = os.path.join(self.ws, "src", "main.rs")
        open(self.main, "w").write(text)
        self.env = {**os.environ, **BACKENDS[backend]}
        self.fx = Fixture(self.ws, ["fz"], ["-p", "fz"], touch=self.main)
        self.fx.rustc_build()
        obj, patch, self.binary = (os.path.join(self.work, f"fz.{name}") for name in ("o", "patch.o", "bin"))
        link = linkline.with_output(linkline.ahead(list(self.fx.link_args), [patch, obj], RA_JIT), self.binary)
        symbols = [x for rlib in self.fx.rlibs + sysroot_rlibs() for x in ("--symbols", rlib)]
        self.ask = ["--crate", "fz", "--out", obj, "--patch", patch, "--link-cmd", linkline.join(link), "--binary", self.binary, *symbols]
        self.socket, self.log = os.path.join(self.work, "ra-jit.sock"), os.path.join(self.work, "daemon.log")
        out = open(self.log, "a")
        self.process = subprocess.Popen([RA_JIT, self.ws, *TARGET_ARGS, "--socket", self.socket, *self.ask], stderr=out, stdout=out, env=self.env)
        began = time.time()
        while "watching" not in open(self.log).read():
            if self.process.poll() is not None or time.time() - began > 600:
                raise SystemExit(f"the daemon did not start:\n{open(self.log).read()[-2000:]}")
            time.sleep(0.05)

    def sync(self, text):
        """Writes the source and asks the daemon for the binary. Returns its reply and how it linked."""
        open(self.main, "w").write(text)
        seen = len(open(self.log).read())
        try:
            reply = subprocess.run([RA_JIT, "sync", self.socket, *self.ask], capture_output=True, text=True, timeout=600, env=self.env)
            said = (reply.stdout + reply.stderr).strip()
        except subprocess.TimeoutExpired:
            said = "no reply after 600s"
        links = re.findall(r"^link: \S+ \((.*)\)$", open(self.log).read()[seen:], re.M)
        if not links:
            return said, "no link"
        if links[-1].startswith("spliced"):
            return said, "spliced"
        reason = re.search(r"not spliced: (.*)", links[-1])
        return said, "full: " + (reason.group(1) if reason else "not asked to splice")

    def cargo(self, text):
        """What cargo's build of the source from nothing prints, or None when rustc turns it down."""
        open(self.main, "w").write(text)
        build = subprocess.run(["cargo", "build", "-q", *TARGET_ARGS], cwd=self.ws, capture_output=True, text=True,
                               env={**os.environ, "CARGO_TARGET_DIR": self.cold, "CARGO_INCREMENTAL": "0"})
        return run(built(self.cold, "fz")) if build.returncode == 0 else None

    def alive(self):
        return self.process.poll() is None

    def gone(self):
        """Whether the daemon has exited, given a moment to: its reply can come before it does."""
        try:
            self.process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            return False
        return True

    def close(self):
        self.process.kill()
        self.process.wait()
        shutil.rmtree(self.fx.work, ignore_errors=True)
        shutil.rmtree(self.dir, ignore_errors=True)


def panic_of(log):
    """The first panic in a daemon's log: where it was and what it said."""
    found = re.search(r"panicked at ([^\n]*)\n([^\n]*)", log)
    return f"{found.group(2)} (at {found.group(1).rstrip(':')})" if found else log[-600:]


def verdict(daemon, text, expected):
    """Shows the daemon a source. Returns what went wrong (None when nothing did), how it linked, and the time."""
    began = time.time()
    reply, how = daemon.sync(text)
    took = int((time.time() - began) * 1000)
    if not reply.startswith("ok") and daemon.gone():
        return "died: " + panic_of(open(daemon.log).read()), how, took
    if not reply.startswith("ok"):
        return "refused: " + reply[:300], how, took
    found = differences(expected, run(daemon.binary))
    return (f"differs in {found[0].case}: {found[0].key}\n{found[0].detail[:1500]}" if found else None), how, took


def modules_of(cases, names=None):
    return [(case.name, case.lines) for case in cases if names is None or case.name in names]


def replay(work, edition, backend, sources):
    """A fresh daemon shown each of the sources in turn, each a list of modules. Returns what
    went wrong at the first step that did, or None."""
    texts = [gen.render(modules) for modules in sources]
    daemon = Daemon(tempfile.mkdtemp(prefix="replay-", dir=work), edition, backend, texts[0])
    try:
        for at, text in enumerate(texts):
            expected = daemon.cargo(text)
            if expected is None:
                return f"step {at}: not accepted by rustc"
            wrong = verdict(daemon, text, expected)[0]
            if wrong:
                return f"step {at}: {wrong}"
        log = open(daemon.log).read()
        return "panicked: " + panic_of(log) if "panicked at" in log else None
    finally:
        daemon.close()


def reduce_pair(work, edition, backend, before, after, budget):
    """Drops the cases the finding does not need from the source before the edit and the one after.
    Returns the two as modules, or None when a daemon that has only seen the one before gets
    the one after right: the finding needs the edits before it."""
    tests = [0]

    def fails(names):
        if tests[0] >= budget:
            return False
        tests[0] += 1
        return replay(work, edition, backend, [modules_of(before, names), modules_of(after, names)]) is not None

    names = [case.name for case in after if any(case.name == other.name for other in before)]
    if not fails(names):
        return None
    names = minimize(names, fails)
    return [modules_of(before, names), modules_of(after, names)]


def write_found(tag, edition, backend, kind, what, wrong, sources, alone):
    """Files a finding under found/edits/: the sources a daemon is to be shown in turn."""
    os.makedirs(EDITS, exist_ok=True)
    path = os.path.join(EDITS, f"{tag}.rs")
    head = [f"// Found by edits.py: seed {tag}, edition {edition}, backend {backend}.", f"// The edit ({kind}): {what}",
            f"// Again: edits.py check {os.path.basename(path)}" if alone else
            "// Not alone: a daemon that has only built the first of these gets the second right. It took the edits before.",
            *("// " + line for line in wrong.splitlines()[:30])]
    parts = ["\n\n".join(gen.module(name, lines) for name, lines in modules) for modules in sources]
    open(path, "w").write("\n".join(head) + "\n" + f"\n{THEN}\n".join(parts) + "\n")
    return path


def read_found(path):
    """The sources of a file under found/edits/, each as modules, and the edition they are of."""
    text = open(path).read()
    edition = re.search(r"edition (\d+)", text).group(1)
    sources = []
    for part in text.split(f"\n{THEN}\n"):
        found = re.findall(r"^mod (c\d+) \{\n    use super::\*;\n(.*?)^\}$", part, re.M | re.S)
        sources.append([(name, body.rstrip("\n").split("\n")) for name, body in found])
    return sources, edition


def check(paths, backends, work):
    """Each file given, or all of found/edits/: whether a daemon shown its sources in turn gets one wrong."""
    paths = paths or sorted(os.path.join(EDITS, name) for name in os.listdir(EDITS) if name.endswith(".rs"))
    wrong = 0
    for path in paths:
        path = path if os.path.exists(path) else os.path.join(EDITS, path)
        sources, edition = read_found(path)
        for backend in backends:
            said = replay(work, edition, backend, sources)
            wrong += said is not None
            print(f"{'WRONG    ' if said else 'same     '} {os.path.basename(path)} ({backend}) {(said or '').splitlines()[0][:200] if said else ''}", flush=True)
    return wrong


def round_of(seed, cases, steps, backend, work, out, budget):
    """One program and its edits under one backend. Returns what each step came to."""
    editor = Editor(seed, cases)
    edition = edition_of(seed)
    daemon = Daemon(tempfile.mkdtemp(prefix=f"{seed}-{backend}-", dir=work), edition, backend, editor.text())
    rows = []
    try:
        expected = daemon.cargo(editor.text())
        wrong, how, took = verdict(daemon, editor.text(), expected)
        rows.append({"kind": "start", "what": "", "how": how, "ms": took, "wrong": wrong})
        for step in range(steps):
            if not daemon.alive():
                break
            before = copy.deepcopy(editor.cases)
            edit = editor.step()
            if edit is None:
                continue
            kind, what = edit
            expected = daemon.cargo(editor.text())
            if expected is None or expected[0] == "timeout":
                rows.append({"kind": kind, "what": what, "how": "not accepted by rustc", "ms": 0, "wrong": None})
                editor.back()
                continue
            wrong, how, took = verdict(daemon, editor.text(), expected)
            row = {"kind": kind, "what": what, "how": how, "ms": took, "wrong": wrong}
            if wrong and wrong.startswith("differs"):
                pair = reduce_pair(work, edition, backend, before, editor.cases, budget)
                sources = pair or [modules_of(before), modules_of(editor.cases)]
                row["found"] = [write_found(f"{seed}-s{step}-{backend}", edition, backend, kind, what, wrong, sources, bool(pair))]
            rows.append(row)
            editor.keep()
            # What the daemon has after it refused is from before the edit: the round is over.
            if wrong and not wrong.startswith("differs"):
                break
        log = open(daemon.log).read()
        if "panicked at" in log and not any(row["wrong"] for row in rows):
            rows.append({"kind": "end", "what": "", "how": "", "ms": 0, "wrong": "panicked: " + panic_of(log)})
        # Under RA_JIT_CHECK_ALL: what changed that the look after an edit would have skipped.
        missed = [line for line in log.splitlines() if line.startswith("check-all: MISSED")]
        if missed and not any(row["wrong"] for row in rows):
            rows.append({"kind": "end", "what": "", "how": "", "ms": 0, "wrong": "missed: " + missed[0]})
        if any(row["wrong"] for row in rows):
            shutil.copy(daemon.log, os.path.join(out, f"{seed}-{backend}.daemon.log"))
    finally:
        daemon.close()
    return rows


def one(job):
    seed, cases, steps, backends, work, out, budget = job
    result = {"seed": seed, "backends": {}}
    for backend in backends:
        try:
            result["backends"][backend] = round_of(seed, cases, steps, backend, work, out, budget)
        except SystemExit as error:
            result["backends"][backend] = [{"kind": "start", "what": "", "how": "no daemon", "ms": 0, "wrong": f"died: {error}"}]
    json.dump(result, open(os.path.join(out, f"{seed}.json"), "w"))
    return result


def say(result):
    for backend, rows in result["backends"].items():
        wrong = [row for row in rows if row["wrong"]]
        print(f"{result['seed']} {backend}: {len(rows) - 1} edits, {len(wrong)} findings", flush=True)
        for row in wrong:
            print(f"  {row['kind']} ({row['what']}): {row['wrong'].splitlines()[0][:200]}", flush=True)
            for path in row.get("found", []):
                print(f"    {path}", flush=True)


def report(directory):
    """Adds up the logs of a run. Returns the number of findings."""
    results = [json.load(open(os.path.join(directory, name))) for name in sorted(os.listdir(directory)) if name.endswith(".json")]
    findings = 0
    for backend in sorted({backend for result in results for backend in result["backends"]}):
        rows = [row for result in results for row in result["backends"].get(backend, [])]
        edits = [row for row in rows if row["kind"] not in ("start", "end") and row["how"] != "not accepted by rustc"]
        wrong = [row for row in rows if row["wrong"]]
        findings += len(wrong)
        kinds = collections.Counter(row["wrong"].split(":")[0].split(" in ")[0] for row in wrong)
        print(f"{backend}: {len(results)} programs, {len(edits)} edits shown to the daemon, "
              f"{sum(row['how'] == 'not accepted by rustc' for row in rows)} edits rustc turned down, {len(wrong)} findings"
              + "".join(f", {count} {kind}" for kind, count in sorted(kinds.items())))
        print("  edit                 shown  wrong")
        for kind in sorted({row["kind"] for row in edits}):
            of_kind = [row for row in edits if row["kind"] == kind]
            print(f"  {kind:20} {len(of_kind):5}  {sum(bool(row['wrong']) for row in of_kind):5}")
        print("  how the daemon linked")
        for how, count in collections.Counter(row["how"] for row in edits).most_common():
            print(f"  {count:5}  {how[:110]}")
    return findings


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("mode", choices=["smoke", "run", "check", "report"])
    parser.add_argument("targets", nargs="*")
    parser.add_argument("--seed", default="edits")
    parser.add_argument("--first", type=int, default=0, help="the round to begin at, to run one again")
    parser.add_argument("--rounds", type=int, default=20)
    parser.add_argument("--cases", type=int, default=6)
    parser.add_argument("--steps", type=int, default=12)
    parser.add_argument("--jobs", type=int, default=2)
    parser.add_argument("--reduce", type=int, default=12, help="builds a reduction may take")
    parser.add_argument("--backends", default="cranelift,llvm")
    parser.add_argument("--out")
    parser.add_argument("--work")
    args = parser.parse_args()
    if args.mode == "report":
        sys.exit(1 if report(args.targets[0]) else 0)
    if args.mode == "smoke":
        args.seed, args.rounds, args.cases, args.steps = (SMOKE[name] for name in ("seed", "rounds", "cases", "steps"))
    out = args.out or tempfile.mkdtemp(prefix="ra-jit-edits-out-")
    work = args.work or tempfile.mkdtemp(prefix="ra-jit-edits-")
    os.makedirs(out, exist_ok=True)
    os.makedirs(work, exist_ok=True)
    if args.mode == "check":
        sys.exit(1 if check(args.targets, args.backends.split(","), work) else 0)
    jobs = [(f"{args.seed}-{index}", args.cases, args.steps, args.backends.split(","), work, out, args.reduce) for index in range(args.first, args.first + args.rounds)]
    with concurrent.futures.ProcessPoolExecutor(args.jobs) as pool:
        for result in pool.map(one, jobs):
            say(result)
    findings = report(out)
    if args.mode == "smoke":
        findings += check([], args.backends.split(","), work)
    print(f"logs: {out}")
    sys.exit(1 if findings else 0)


if __name__ == "__main__":
    main()
