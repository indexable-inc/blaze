"""mir_semantics.py: the Lean semantics of MIR held to the tree (objectives V6 and V8;
docs/v2/plan/design/formal-verification.md, "The semantics of MIR").

    uv run --frozen python harness/v2/mir_semantics.py differential '{"blaze": "target/release/blaze"}'
    uv run --frozen python harness/v2/mir_semantics.py validate '{"blaze": "target/release/blaze"}'
    uv run --frozen python harness/v2/mir_semantics.py selftest

`differential` (V6) draws small Rust programs in the subset and asks three things what each
call of each returns: the Lean semantics (`lean/Blaze/Mir`, `run` on the body
`crates/hir-ty-lean` embeds), blaze's MIR evaluator, and the executable blaze builds. All three
must give the same value or the same kind of panic (`arithmetic`, `bounds`, `panic`); undefined
behaviour, running out of fuel or a refusal from any of them is a disagreement. Every
disagreement is printed with its program.

`validate` (V8) holds the Rust MIR passes that have a Lean specification to it: for the corpus'
functions and the checked-in pairs (lean/BlazeValidated), Lean's kernel checks
`after = <specification> before` for the body before and after the pass run alone.

The input is one JSON object (inline, a file's path, or `-`):

    blaze     the blaze that runs crates/hir-ty-lean's test `mir_semantics_corpus` (the encoder,
              the evaluator, the passes) and builds the executable. Absolute, or relative to `root`
    lake      the pinned toolchain's `lake`, an absolute path; empty for the one elan keeps for
              lean/lean-toolchain's pin (`~/.elan/toolchains/<pin>/bin/lake`). Never PATH
    root      the checkout (".")
    programs  how many programs are drawn (21)
    cases     how many calls of each (10)
    seed      the seed both are drawn from (0)
    fuel      the steps (blocks run) the Lean semantics gives each call (100000)
    work      where the corpus is written and built; empty for a new directory under the system's
              temporary directory, removed at the end
    out       where the result is written (report.py's shape), for harness/v2/ledger.py

A program is a crate with `pub fn entry(..) -> T` over integers and one `pub fn case_N() -> T`
per call, which calls `entry` with that call's arguments. The Lean side runs `entry` with the
arguments; the evaluator and the executable run the `case_N` functions. The programs come from
seven shapes, each with drawn constants and operators: checked arithmetic with an early return;
a struct written through a `&mut` parameter; an enum built by a call and matched; a loop; an
array indexed by an argument; an explicit panic, an early return and a division; a shift, a
wrapping method and a signed shift. The arguments include each type's ends.

`selftest` runs none of the three: it checks the programs drawn are the same for a seed, each
has its entry and cases, the calls are written as the runner reads them, a panic's message is
read as its kind, and the comparison catches a planted disagreement of each side.
"""

import json
import os
import random
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import parse

SCRIPT = "mir_semantics.py"
TEST = "mir_semantics_corpus"
PAIRS = "the_passes_are_their_lean_specifications"
BITS = {"u8": 8, "u16": 16, "u32": 32, "u64": 64, "usize": 64, "i8": 8, "i16": 16, "i32": 32, "i64": 64, "isize": 64}
MANIFEST = """[package]
name = "mir_semantics_built"
version = "0.0.0"
edition = "2024"

[workspace]
"""
REPORT = """
fn report<T: std::fmt::Display>(name: &str, case: &str, run: fn() -> T) {
    match std::panic::catch_unwind(run) {
        Ok(value) => println!("{name} {case} ok {value}"),
        Err(payload) => {
            let said = payload
                .downcast_ref::<&str>()
                .map(|it| it.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            println!("{name} {case} panic {said}");
        }
    }
}
"""


@dataclass(frozen=True)
class Input:
    blaze: str
    lake: str = ""
    root: str = "."
    programs: int = 21
    cases: int = 10
    seed: int = 0
    fuel: int = 100000
    work: str = ""
    out: str = ""


@dataclass(frozen=True)
class Program:
    name: str
    source: str  # `entry` and what it uses
    params: tuple[str, ...]  # each parameter's type
    returns: str
    calls: tuple[tuple[int, ...], ...]  # each call's arguments

    def case(self, n: int) -> str:
        args = ", ".join(f"{value}{ty}" for value, ty in zip(self.calls[n], self.params))
        return f"pub fn case_{n}() -> {self.returns} {{ entry({args}) }}\n"

    def text(self) -> str:
        return self.source + "".join(self.case(n) for n in range(len(self.calls)))


def ends(ty: str) -> tuple[int, int]:
    bits = BITS[ty]
    return (-(2 ** (bits - 1)), 2 ** (bits - 1) - 1) if ty.startswith("i") else (0, 2**bits - 1)


def draw_value(rng: random.Random, ty: str, small: int) -> int:
    """A value of `ty`: an end, near one, small (below `small`), or anything."""
    low, high = ends(ty)
    kind = rng.randrange(6)
    if kind == 0:
        return rng.choice((low, high))
    if kind == 1:
        return rng.choice((low + 1, high - 1, 0, 1))
    if kind in (2, 3):
        return rng.randrange(small)
    return rng.randint(low, high)


def shapes(rng: random.Random) -> list[tuple[str, tuple[str, ...], str, tuple[int, ...]]]:
    """The seven shapes with constants drawn: the source, the parameters' types, the return
    type, and what counts as small for each parameter."""
    op = lambda: rng.choice(("+", "-", "*"))
    last = rng.choice(("+", "-", "*", "/", "%"))
    return [
        (f"""pub fn entry(a: u32, b: u32) -> u32 {{
    let c = a {op()} {rng.randrange(1, 9)};
    if c > b {{
        return c - b;
    }}
    (c {op()} b) {last} {rng.randrange(1, 7)}
}}
""", ("u32", "u32"), "u32", (40, 40)),
        (f"""pub struct Acc {{ pub sum: u64, pub n: u32 }}
fn push(acc: &mut Acc, x: u32) {{
    acc.sum = acc.sum + (x as u64) * {rng.choice((3, 2**20, 2**33))};
    acc.n += 1;
}}
pub fn entry(a: u32, b: u32) -> u64 {{
    let mut acc = Acc {{ sum: {rng.randrange(1000)}, n: 0 }};
    push(&mut acc, a);
    push(&mut acc, b);
    acc.sum + acc.n as u64
}}
""", ("u32", "u32"), "u64", (1000, 1000)),
        (f"""pub enum Op {{ Add(u32), Neg, Stop }}
fn pick(k: u32) -> Op {{
    if k % 3 == 0 {{ Op::Add(k) }} else if k % 3 == 1 {{ Op::Neg }} else {{ Op::Stop }}
}}
pub fn entry(a: u32, b: i32) -> i32 {{
    match pick(a) {{
        Op::Add(x) => b + (x as i32),
        Op::Neg => -b,
        Op::Stop => {rng.randrange(100)},
    }}
}}
""", ("u32", "i32"), "i32", (30, 30)),
        (f"""pub fn entry(n: u8, step: u32) -> u32 {{
    let mut total = {rng.randrange(5)};
    let mut i = 0;
    while i < n {{
        total = total * {rng.randrange(1, 5)} + step;
        i += 1;
    }}
    total
}}
""", ("u8", "u32"), "u32", (12, 9)),
        (f"""pub fn entry(i: usize, x: u8) -> u8 {{
    let xs = [{rng.randrange(256)}u8, {rng.randrange(256)}, {rng.randrange(256)}, {rng.randrange(256)}];
    xs[i] ^ x
}}
""", ("usize", "u8"), "u8", (6, 256)),
        (f"""pub fn entry(a: i64, b: i64) -> i64 {{
    if a == 3 {{
        panic!();
    }}
    if b < 0 {{
        return a;
    }}
    a / b + {rng.randrange(50)}
}}
""", ("i64", "i64"), "i64", (6, 4)),
        (f"""pub fn entry(a: i32, s: u32) -> i32 {{
    (a << s).wrapping_add({rng.randrange(1, 1000)}) >> 1
}}
""", ("i32", "u32"), "i32", (64, 36)),
    ]


def draw(programs: int, cases: int, seed: int) -> list[Program]:
    """The programs: the shapes in turn, each time with new constants and calls."""
    rng = random.Random(seed)
    out: list[Program] = []
    for n in range(programs):
        source, params, returns, small = shapes(rng)[n % 7]
        calls = tuple(tuple(draw_value(rng, ty, s) for ty, s in zip(params, small)) for _ in range(cases))
        out.append(Program(f"p{n}", source, params, returns, calls))
    return out


def call_lines(programs: list[Program], fuel: int) -> str:
    """What lean/Blaze/Mir/Runner.lean reads: `<program> <function> <fuel> <argument>..`."""
    return "".join(
        " ".join([p.name, "0", str(fuel), *(f"{ty}:{value}" for value, ty in zip(call, p.params))]) + "\n"
        for p in programs for call in p.calls)


def built_source(programs: list[Program]) -> str:
    """One crate: each program a module, and a `main` that runs every case and catches its panic."""
    mods = "".join(f"mod {p.name} {{\n{p.text()}}}\n" for p in programs)
    runs = "".join(f'    report("{p.name}", "case_{n}", {p.name}::case_{n});\n'
                   for p in programs for n in range(len(p.calls)))
    return ("#![allow(warnings, unconditional_panic, arithmetic_overflow)]\n" + mods + REPORT
            + "\nfn main() {\n    std::panic::set_hook(Box::new(|_| {}));\n" + runs + "}\n")


def kind_of(message: str) -> str:
    """The kind of panic a Rust panic message is of."""
    lower = message.lower()
    if "out of bounds" in lower:
        return "bounds"
    if "overflow" in lower or "by zero" in lower or "divisor of zero" in lower:
        return "arithmetic"
    return "panic"


def keyed(lines: list[str]) -> dict[tuple[str, str], str]:
    """`<program> <case> <answer>` lines by program and case; a built panic's message as its kind."""
    out: dict[tuple[str, str], str] = {}
    for line in lines:
        name, case, answer = (line.split(" ", 2) + ["", ""])[:3]
        if answer.startswith("panic ") and answer not in ("panic arithmetic", "panic bounds", "panic panic"):
            answer = "panic " + kind_of(answer[len("panic "):])
        elif answer == "panic":
            answer = "panic panic"
        out[(name, case)] = answer
    return out


def disagreements(programs: list[Program], lean: list[str], evaluator: dict[tuple[str, str], str],
                  built: dict[tuple[str, str], str]) -> list[str]:
    """Each call the three do not agree on, as a sentence with its program."""
    calls = [(p, n) for p in programs for n in range(len(p.calls))]
    if len(lean) != len(calls):
        return [f"{len(calls)} calls and {len(lean)} answers from the Lean semantics"]
    out: list[str] = []
    for (p, n), answer in zip(calls, lean):
        key = (p.name, f"case_{n}")
        three = (answer, evaluator.get(key, "no answer"), built.get(key, "no answer"))
        agree = three[0] == three[1] == three[2] and (three[0].startswith("ok ") or three[0].startswith("panic "))
        if not agree:
            out.append(f"{p.name} entry{p.calls[n]}: the Lean semantics says `{three[0]}`, the MIR evaluator "
                       f"`{three[1]}`, the executable `{three[2]}`\n{p.source}")
    return out


def lake_of(given: str, root: Path, home: Path) -> Path:
    """The pinned toolchain's lake: `given`, or the one elan keeps for lean/lean-toolchain's pin."""
    if given:
        if not Path(given).is_absolute():
            raise SystemExit(f"{SCRIPT}: `lake` is `{given}`: give the pinned toolchain's lake by an absolute path")
        return Path(given)
    pin = (root / "lean/lean-toolchain").read_text().strip()
    lake = home / ".elan/toolchains" / pin.replace("/", "--").replace(":", "---") / "bin/lake"
    if not lake.is_file():
        raise SystemExit(f"{SCRIPT}: no `{lake}` (the toolchain lean/lean-toolchain pins, `{pin}`): install it, "
                         "or give `lake` by an absolute path")
    return lake


def run(argv: list[str], cwd: Path, stdin: str = "", env: dict[str, str] | None = None) -> str:
    done = subprocess.run(argv, cwd=cwd, input=stdin, capture_output=True, text=True,
                          env=None if env is None else {**os.environ, **env})
    if done.returncode != 0:
        raise SystemExit(f"{SCRIPT}: `{' '.join(argv)}` in {cwd} failed ({done.returncode}):\n{done.stdout}{done.stderr}")
    return done.stdout


def result(it: Input, name: str, wrong: list[str]) -> None:
    """Prints the verdict, writes it where the ledger reads it, and exits 1 when anything is wrong."""
    if it.out:
        detail = wrong[0].splitlines()[0] if wrong else ""
        Path(it.out).write_text(json.dumps(
            {"outcomes": [{"name": name, "verdict": "fail" if wrong else "pass", "detail": detail}]}) + "\n")
    if wrong:
        raise SystemExit(f"{SCRIPT}: {name}: {len(wrong)} wrong\n\n" + "\n".join(wrong[:10]))


def corpus(it: Input, root: Path, blaze: Path, work: Path) -> list[Program]:
    """Writes the programs and runs the Rust half: crates/hir-ty-lean's `mir_semantics_corpus`
    writes Corpus.lean, evaluator.txt and Validate.lean beside them."""
    programs = draw(it.programs, it.cases, it.seed)
    (work / "programs").mkdir(parents=True, exist_ok=True)
    for p in programs:
        (work / "programs" / f"{p.name}.rs").write_text(p.text())
    run([str(blaze), "test", "-p", "hir-ty-lean", TEST], root, env={"BLAZE_MIR_SEMANTICS": str(work)})
    for made in ("Corpus.lean", "evaluator.txt", "Validate.lean"):
        if not (work / made).is_file():
            raise SystemExit(f"{SCRIPT}: `blaze test -p hir-ty-lean {TEST}` passed and wrote no {made} in {work}: "
                             "the test did not run (is the filter this blaze's?)")
    return programs


def differential(it: Input, root: Path, blaze: Path, lake: Path, work: Path) -> None:
    programs = corpus(it, root, blaze, work)
    run([str(lake), "build", "Blaze"], root / "lean")
    lean = run([str(lake), "env", "lean", "--run", str(work / "Corpus.lean")], root / "lean",
               call_lines(programs, it.fuel)).splitlines()
    evaluator = keyed((work / "evaluator.txt").read_text().splitlines())
    crate = work / "built"
    (crate / "src").mkdir(parents=True, exist_ok=True)
    (crate / "Cargo.toml").write_text(MANIFEST)
    (crate / "src/main.rs").write_text(built_source(programs))
    run([str(blaze), "build"], crate)
    built = keyed(run([str(crate / "target/debug/mir_semantics_built")], crate).splitlines())
    wrong = disagreements(programs, lean, evaluator, built)
    result(it, "the Lean semantics, the MIR evaluator and the executable agree", wrong)
    print(f"{SCRIPT}: the Lean semantics, the MIR evaluator and the executable agree on "
          f"{sum(len(p.calls) for p in programs)} calls of {len(programs)} programs (seed {it.seed})")


def validate(it: Input, root: Path, blaze: Path, lake: Path, work: Path) -> None:
    wrong: list[str] = []
    programs = corpus(it, root, blaze, work)
    steps = [
        ("the checked-in pairs are what the passes and the encoder write",
         [str(blaze), "test", "-p", "hir-ty-lean", PAIRS], root),
        ("Lean accepts the checked-in pairs (lean/BlazeValidated)", [str(lake), "build", "BlazeValidated"], root / "lean"),
        ("Lean accepts the corpus' pairs", [str(lake), "env", "lean", str(work / "Validate.lean")], root / "lean"),
    ]
    for what, argv, cwd in steps:
        done = subprocess.run(argv, cwd=cwd, capture_output=True, text=True)
        if done.returncode != 0:
            wrong.append(f"not so: {what} (`{' '.join(argv)}` in {cwd}, {done.returncode}): a pass did something "
                         f"its Lean specification does not\n{done.stdout}{done.stderr}")
    result(it, "the specified MIR passes do what their Lean specifications do", wrong)
    print(f"{SCRIPT}: RemoveStorageMarkers and SimplifyConstCondition did what their Lean specifications do on "
          f"the checked-in pairs and on {len(programs)} drawn programs (seed {it.seed})")


def main(command: str, it: Input) -> None:
    root = Path(it.root).resolve()
    blaze = Path(it.blaze) if Path(it.blaze).is_absolute() else root / it.blaze
    lake = lake_of(it.lake, root, Path.home())
    work = Path(it.work) if it.work else Path(tempfile.mkdtemp(prefix="blaze-mir-semantics-"))
    try:
        {"differential": differential, "validate": validate}[command](it, root, blaze, lake, work)
    finally:
        if not it.work:
            shutil.rmtree(work, ignore_errors=True)


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def selftest() -> None:
    programs = draw(14, 6, 5)
    expect(programs == draw(14, 6, 5) and programs != draw(14, 6, 6), "a seed's programs are its own, each time")
    expect([p.name for p in programs] == [f"p{n}" for n in range(14)], "programs are named in order")
    expect(programs[0].source != programs[7].source, "a shape drawn again has new constants")
    for p in programs:
        text = p.text()
        expect("pub fn entry(" in text and text.count("pub fn case_") == 6, f"{p.name} has its entry and cases")
        expect(all(len(call) == len(p.params) for call in p.calls), f"{p.name}'s calls have its arguments")
        expect(all(ends(ty)[0] <= v <= ends(ty)[1] for call in p.calls for v, ty in zip(call, p.params)),
               f"{p.name}'s arguments are of their types")
        expect(f"pub fn case_0() -> {p.returns} {{ entry(" in text, "a case names its return type as the test reads it")
    shapes_used = {p.source.splitlines()[0] for p in programs}
    expect(len(shapes_used) == 7, "the seven shapes are all drawn")
    joined = "".join(p.source for p in programs)
    for needs in ("&mut Acc", "match pick(a)", "while i < n", "xs[i]", "panic!()", "return ", "wrapping_add", " / "):
        expect(needs in joined, f"the programs have `{needs}`")
    expect(ends("i8") == (-128, 127) and ends("usize") == (0, 2**64 - 1), "the ends of the types")
    one = Program("p0", "pub fn entry(a: i32, s: u32) -> i32 { a }\n", ("i32", "u32"), "i32", ((-5, 2), (7, 0)))
    expect(one.case(0) == "pub fn case_0() -> i32 { entry(-5i32, 2u32) }\n", "a case calls the entry with its arguments")
    expect(call_lines([one], 50) == "p0 0 50 i32:-5 u32:2\np0 0 50 i32:7 u32:0\n", "a call line is the runner's")
    source = built_source([one])
    expect("mod p0 {" in source and 'report("p0", "case_1", p0::case_1);' in source, "the executable runs every case")
    expect(kind_of("attempt to add with overflow") == "arithmetic", "an overflow is arithmetic")
    expect(kind_of("attempt to divide by zero") == "arithmetic", "a division by zero is arithmetic")
    expect(kind_of("attempt to shift left with overflow") == "arithmetic", "a shift too far is arithmetic")
    expect(kind_of("index out of bounds: the len is 4 but the index is 9") == "bounds", "an index is bounds")
    expect(kind_of("explicit panic") == "panic", "any other panic is a panic")
    lines = ["p0 case_0 ok -5", "p0 case_1 panic attempt to add with overflow"]
    answers = keyed(lines)
    expect(answers == {("p0", "case_0"): "ok -5", ("p0", "case_1"): "panic arithmetic"}, f"answers by case: {answers}")
    lean = ["ok -5", "panic arithmetic"]
    expect(disagreements([one], lean, answers, answers) == [], "equal answers agree")
    expect(len(disagreements([one], ["ok -4", "panic arithmetic"], answers, answers)) == 1, "a wrong Lean answer is caught")
    other = dict(answers) | {("p0", "case_1"): "panic bounds"}
    expect("the MIR evaluator `panic bounds`" in disagreements([one], lean, other, answers)[0], "a wrong evaluator is caught")
    expect("the executable `panic bounds`" in disagreements([one], lean, answers, other)[0], "a wrong executable is caught")
    undefined = {key: "undefined" for key in answers}
    expect(len(disagreements([one], ["undefined uninit"] * 2, undefined, undefined)) == 2, "undefined behaviour never agrees")
    expect(len(disagreements([one], ["diverge", "diverge"], answers, answers)) == 2, "out of fuel never agrees")
    expect(disagreements([one], lean[:1], answers, answers) != [], "a missing answer disagrees")
    expect(len(disagreements([one], lean, {}, answers)) == 2, "a side with no answer disagrees")
    with tempfile.TemporaryDirectory() as tmp:
        root, home = Path(tmp) / "root", Path(tmp) / "home"
        (root / "lean").mkdir(parents=True)
        (root / "lean/lean-toolchain").write_text("leanprover/lean4:v4.20.0\n")
        pinned = home / ".elan/toolchains/leanprover--lean4---v4.20.0/bin"
        try:
            lake_of("", root, home)
            expect(False, "a missing toolchain is refused")
        except SystemExit:
            pass
        pinned.mkdir(parents=True)
        (pinned / "lake").write_text("")
        expect(lake_of("", root, home) == pinned / "lake", "the pin names elan's toolchain")
        expect(lake_of("/opt/lean/bin/lake", root, home) == Path("/opt/lean/bin/lake"), "a given lake is used")
        try:
            lake_of("lake", root, home)
            expect(False, "a lake from PATH is refused")
        except SystemExit:
            pass
    given = parse(Input, '{"blaze": "target/release/blaze", "programs": 7}', SCRIPT)
    expect(given.programs == 7 and given.cases == 10 and given.fuel == 100000, f"the input: {given}")
    try:
        parse(Input, '{"blaze": "b", "engine": 3}', SCRIPT)
        expect(False, "an unknown field is refused")
    except SystemExit:
        pass
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) == 2 and sys.argv[1] == "selftest":
        selftest()
    elif len(sys.argv) == 3 and sys.argv[1] in ("differential", "validate"):
        main(sys.argv[1], parse(Input, sys.argv[2], SCRIPT))
    else:
        sys.exit(f"usage: {SCRIPT} differential|validate '<json>' | selftest\n\n{__doc__}")
