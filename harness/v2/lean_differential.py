"""lean_differential.py: the Lean model of a function, compiled, against the function blaze built,
on drawn inputs (docs/v2/plan/design/formal-verification.md, "How the translator is checked";
objective V5's second half). A proof is about the model; this is what ties the model to the code.

The function is `blaze_graph::ledger::fnv1a_from` (crates/blaze-graph/src/ledger.rs), the first
with a proof (crates/blaze-graph/lean/BlazeGraph/Ledger.lean). Its model is the checked-in module
lean/BlazeModel/blaze_graph/ledger/fnv1a_from.lean, which crates/hir-ty-lean's golden test holds to
what the translator emits; lean/Blaze/Tests/Fnv1aMain.lean reads inputs and runs it.

    uv run --frozen python harness/v2/lean_differential.py '{"blaze": "target/release/blaze", "lake": "/abs/path/to/lake"}'
    uv run --frozen python harness/v2/lean_differential.py selftest

The input is one JSON object (inline, a file's path, or `-`):

    blaze   the blaze that builds the Rust side: `blaze build` in a crate this script writes,
            holding the function's text as the tree has it and a `main` that reads the same
            lines. An absolute path, or one relative to `root`
    lake    the pinned toolchain's `lake` (lean/lean-toolchain), an absolute path: never PATH
    root    the checkout (".")
    cases   how many inputs are drawn (512)
    seed    the seed they are drawn from (0)
    most    the most bytes one input has (64)
    work    where the Rust crate is written and built; empty for a new directory under the
            system's temporary directory, removed at the end

Each input is a line of decimal numbers, the seed then each byte. Both sides print one line per
input: the hash in decimal (the model also `fail` or `div`, which the function never does and so
is a disagreement). The run fails naming the first input they disagree on. The inputs include the
empty slice, one byte, and the ledger's own offset basis as seed.

`selftest` runs neither side: it checks the function's text is found in the tree, the inputs drawn
are the same for a seed, and that the comparison catches a planted disagreement (against this
script's own FNV-1a).
"""

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

SCRIPT = "lean_differential.py"
SOURCE = "crates/blaze-graph/src/ledger.rs"
OPENS = "const fn fnv1a_from(mut hash: u64, bytes: &[u8]) -> u64 {"
OFFSET_BASIS = 0xCBF29CE484222325
PRIME = 0x0100000001B3
MAIN = """
fn main() {
    use std::io::BufRead as _;
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.expect("a line");
        let mut numbers = line.split_whitespace().map(|it| it.parse::<u64>().expect("a number"));
        let seed = numbers.next().expect("a seed");
        let bytes: Vec<u8> = numbers.map(|it| it as u8).collect();
        println!("{}", fnv1a_from(seed, &bytes));
    }
}
"""
MANIFEST = """[package]
name = "fnv1a_rust"
version = "0.0.0"
edition = "2024"

[workspace]
"""


@dataclass(frozen=True)
class Input:
    blaze: str
    lake: str
    root: str = "."
    cases: int = 512
    seed: int = 0
    most: int = 64
    work: str = ""


def function_text(ledger: str) -> str:
    """`fnv1a_from` as the tree has it, from its opening line to its closing brace."""
    start = ledger.find(OPENS)
    if start < 0:
        raise SystemExit(f"{SCRIPT}: {SOURCE} has no line `{OPENS}`: the function moved or changed its signature")
    end = ledger.find("\n}\n", start)
    if end < 0:
        raise SystemExit(f"{SCRIPT}: {SOURCE}: `fnv1a_from` has no closing brace at the start of a line")
    return ledger[start:end + 3]


def draw(cases: int, seed: int, most: int) -> list[tuple[int, list[int]]]:
    """The inputs: the edges first, then `cases` drawn from `seed`."""
    rng = random.Random(seed)
    out: list[tuple[int, list[int]]] = [(OFFSET_BASIS, []), (0, [0]), (2**64 - 1, [255] * most)]
    for _ in range(cases):
        first = rng.choice((OFFSET_BASIS, 0, 2**64 - 1, rng.getrandbits(64)))
        out.append((first, [rng.getrandbits(8) for _ in range(rng.randint(0, most))]))
    return out


def lines(inputs: list[tuple[int, list[int]]]) -> str:
    return "".join(" ".join(str(n) for n in (first, *rest)) + "\n" for first, rest in inputs)


def fnv1a(first: int, rest: list[int]) -> int:
    for byte in rest:
        first = ((first ^ byte) * PRIME) % 2**64
    return first


def disagreement(inputs: list[tuple[int, list[int]]], model: list[str], built: list[str]) -> str:
    """The first input the two sides disagree on, as a sentence; empty when they agree."""
    if len(model) != len(inputs) or len(built) != len(inputs):
        return f"{len(inputs)} inputs, {len(model)} answers from the model and {len(built)} from the function"
    for (first, rest), m, b in zip(inputs, model, built):
        if m != b:
            return f"from the seed {first} over the bytes {rest}: the Lean model gives `{m}` and the function blaze built `{b}`"
    return ""


def run(argv: list[str], cwd: Path, stdin: str = "") -> str:
    done = subprocess.run(argv, cwd=cwd, input=stdin, capture_output=True, text=True)
    if done.returncode != 0:
        raise SystemExit(f"{SCRIPT}: `{' '.join(argv)}` in {cwd} failed ({done.returncode}):\n{done.stdout}{done.stderr}")
    return done.stdout


def main(it: Input) -> None:
    root = Path(it.root).resolve()
    if not Path(it.lake).is_absolute():
        raise SystemExit(f"{SCRIPT}: `lake` is `{it.lake}`: give the pinned toolchain's lake by an absolute path")
    blaze = Path(it.blaze) if Path(it.blaze).is_absolute() else root / it.blaze
    inputs = draw(it.cases, it.seed, it.most)
    text = lines(inputs)

    run([it.lake, "build", "fnv1a_model"], root / "lean")
    model = run([str(root / "lean/.lake/build/bin/fnv1a_model")], root, text).splitlines()

    work = Path(it.work) if it.work else Path(tempfile.mkdtemp(prefix="blaze-lean-differential-"))
    try:
        (work / "src").mkdir(parents=True, exist_ok=True)
        (work / "Cargo.toml").write_text(MANIFEST)
        (work / "src/main.rs").write_text(function_text((root / SOURCE).read_text()) + MAIN)
        run([str(blaze), "build"], work)
        built = run([str(work / "target/debug/fnv1a_rust")], work, text).splitlines()
    finally:
        if not it.work:
            shutil.rmtree(work, ignore_errors=True)

    wrong = disagreement(inputs, model, built)
    if wrong:
        raise SystemExit(f"{SCRIPT}: the model and the code disagree {wrong}")
    print(f"{SCRIPT}: the Lean model and the function blaze built agree on {len(inputs)} inputs (seed {it.seed})")


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def selftest() -> None:
    root = HERE.parent.parent
    text = function_text((root / SOURCE).read_text())
    expect(text.startswith(OPENS) and text.endswith("}\n") and "wrapping_mul" in text, "the function's text is the tree's")
    try:
        function_text("fn other() {}\n")
        expect(False, "a file without the function is refused")
    except SystemExit:
        pass
    inputs = draw(32, 7, 16)
    expect(inputs == draw(32, 7, 16) and inputs != draw(32, 8, 16), "a seed's inputs are its own, each time")
    expect(inputs[0] == (OFFSET_BASIS, []) and len(inputs) == 35, "the edges come first")
    expect(all(len(rest) <= 16 and all(0 <= b < 256 for b in rest) for _, rest in inputs), "bytes are bytes, at most `most`")
    expect(lines([(5, [1, 2]), (9, [])]) == "5 1 2\n9\n", "a line is the seed then the bytes")
    expect(fnv1a(OFFSET_BASIS, []) == OFFSET_BASIS and fnv1a(OFFSET_BASIS, [0x61]) == 0xAF63DC4C8601EC8C, "FNV-1a of `a`")
    right = [str(fnv1a(first, rest)) for first, rest in inputs]
    expect(disagreement(inputs, right, right) == "", "equal answers agree")
    planted = list(right)
    planted[3] = str(int(planted[3]) ^ 1)
    expect("the Lean model gives" in disagreement(inputs, planted, right), "a planted disagreement is caught")
    expect("fail" in disagreement(inputs, ["fail"] * len(inputs), right), "a model that fails disagrees")
    expect(disagreement(inputs, right[:-1], right) != "", "a missing answer disagrees")
    given = parse(Input, '{"blaze": "target/release/blaze", "lake": "/opt/lean/bin/lake", "cases": 8}', SCRIPT)
    expect(given.cases == 8 and given.seed == 0 and given.most == 64, f"the input: {given}")
    try:
        parse(Input, '{"blaze": "b", "lake": "/l", "fuel": 3}', SCRIPT)
        expect(False, "an unknown field is refused")
    except SystemExit:
        pass
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
