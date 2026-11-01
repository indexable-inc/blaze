"""bounds.py: the speed-of-light bounds every lane writes (docs/v2/plan/goals/principles.md, "Within
10% of the theoretical maximum"), read as budgets: at most the bound × 1.1, or for a rate at least
the bound / 1.1. `budgets.py` measures them as its budget `bounds`.

Each lane's `docs/v2/plan/lanes/<lane>/speed-of-light.md` says, per component, its bound with the
arithmetic and what `proof` should measure. A bound a program can measure is also written there,
beside that arithmetic, as a fenced block with the info string `bound` holding one JSON object:

    ```bound
    {"name": "link: ripgrep's full link", "bound": 9.0, "unit": "ms",
     "program": "blaze-link-bench", "args": ["ripgrep"]}
    ```

    name      the outcome's name, unique across every lane
    bound     the speed of light, in `unit`
    unit      what the program's `value` is in ("ms", "GB/s", ...)
    rate      true when more is better (a throughput): the budget is at least bound / 1.1 (false)
    program   a program the build being measured delivers beside `blaze` (built by blaze as the
              rest is), which does the component's reference workload once and prints, as the
              last line of its standard output, a JSON object with `value` in `unit`
    args      its arguments

A block that is not one such object stops the run, naming the file and line: a bound nobody can
read is not a bound that passes.

    uv run --frozen python harness/v2/bounds.py '{"root": "."}'
    uv run --frozen python harness/v2/bounds.py selftest

prints every bound found (name, bound, budget, unit, where).
"""

import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import cast

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse

SCRIPT = "bounds.py"
# docs/v2/plan/goals/principles.md: a component more than 10% off its bound is a bug.
SLACK = 1.1
DOCS = "docs/v2/plan/lanes/*/speed-of-light.md"
BLOCK = re.compile(r"^```bound[ \t]*\n(.*?)^```[ \t]*$", re.M | re.S)


@dataclass(frozen=True)
class Bound:
    name: str
    bound: float
    unit: str
    program: str
    args: tuple[str, ...] = ()
    rate: bool = False


@dataclass(frozen=True)
class Found:
    """A bound and where it is written (`doc:line`)."""

    bound: Bound
    where: str

    @property
    def budget(self) -> float:
        return self.bound.bound / SLACK if self.bound.rate else self.bound.bound * SLACK


def blocks(text: str, doc: str) -> list[Found]:
    """Every `bound` block of `text`, the file `doc`, in order."""
    found: list[Found] = []
    for block in BLOCK.finditer(text):
        where = f"{doc}:{text.count(chr(10), 0, block.start()) + 1}"
        if not (said := block.group(1).strip()).startswith("{"):
            fail(where, f"a bound block holds one JSON object, not {said[:80]!r}")
        found.append(Found(parse(Bound, said, where), where))
        if found[-1].bound.bound <= 0:
            fail(where, f"bound: more than 0, not {found[-1].bound.bound}")
    return found


def read_all(root: Path) -> list[Found]:
    """Every lane's bounds, by lane in path order; two of one name stop the run."""
    found: list[Found] = []
    for doc in sorted(root.glob(DOCS)):
        found += blocks(doc.read_text(), doc.relative_to(root).as_posix())
    seen: dict[str, str] = {}
    for f in found:
        if (first := seen.setdefault(f.bound.name, f.where)) != f.where:
            fail(SCRIPT, f"two bounds named {f.bound.name!r}: {first} and {f.where}")
    return found


def value_of(stdout: str, what: str) -> float:
    """The `value` of the JSON object a bound's program prints last."""
    lines = stdout.strip().splitlines()
    try:
        said = json.loads(lines[-1]) if lines else None
    except json.JSONDecodeError:
        said = None
    value = cast("dict[str, object]", said).get("value") if isinstance(said, dict) else None
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        raise ValueError(f"{what}: the last line is not a JSON object with a number `value`: {lines[-1:]!r:.300}")
    return float(value)


@dataclass(frozen=True)
class Input:
    root: str = "."


def main(it: Input) -> None:
    for f in read_all(Path(it.root).resolve()):
        b = f.bound
        rule = "at least" if b.rate else "at most"
        print(f"{b.name}: bound {b.bound:g} {b.unit}, budget {rule} {f.budget:.4g} {b.unit} ({f.where})")


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def selftest() -> None:
    doc = (
        "# A lane\n\nThe bound is 2 × 10 ms.\n\n"
        "```bound\n"
        '{"name": "a: link", "bound": 20.0, "unit": "ms", "program": "a-bench", "args": ["x"]}\n'
        "```\n\nprose\n\n```rust\nfn f() {}\n```\n\n"
        "```bound\n"
        '{"name": "a: hash", "bound": 6.0, "unit": "GB/s", "program": "h", "rate": true}\n'
        "```\n"
    )
    found = blocks(doc, "lanes/a/speed-of-light.md")
    expect([f.bound.name for f in found] == ["a: link", "a: hash"], f"two bound blocks, not {found}")
    expect(found[0].where == "lanes/a/speed-of-light.md:5", f"where the first is: {found[0].where}")
    expect(found[0].bound.args == ("x",) and found[1].bound.args == (), "the arguments")
    expect(abs(found[0].budget - 22.0) < 1e-9 and abs(found[1].budget - 6.0 / 1.1) < 1e-9,
           f"budgets: {[f.budget for f in found]}")
    for bad in ('{"name": "x", "bound": 1.0, "unit": "ms"}', '{"name": "x", "bound": 1, "unit": "ms", "program": "p"}',
                '{"name": "x", "bound": 1.0, "unit": "ms", "program": "p", "extra": 1}',
                '{"name": "x", "bound": 0.0, "unit": "ms", "program": "p"}', ""):
        try:
            blocks(f"```bound\n{bad}\n```\n", "d.md")
            expect(False, f"the block {bad!r} is refused")
        except SystemExit as e:
            expect(str(e).startswith("d.md:1: "), f"a refusal names the place: {e}")
    expect(value_of("warming up\n{\"value\": 8.5}\n", "p") == 8.5, "the last line's value")
    for bad in ("", "8.5\n", '{"value": "8"}\n', '{"value": true}\n', '{"v": 1}\n'):
        try:
            value_of(bad, "p")
            expect(False, f"the output {bad!r} is refused")
        except ValueError:
            pass
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
