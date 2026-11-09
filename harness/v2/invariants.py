"""invariants.py: blaze's invariants, refinements and reference models as a list people and tools
read (docs/v2/plan.md, "Proof by check"). The code is their one home; this script finds them,
holds each to its proof, and writes the list, docs/v2/plan/invariants.md.

A statement is a doc comment on the item that states or checks it, opening with one of

    /// The invariant: <what always holds>          on the test whose oracle checks it
    /// The refinement: <what equals what>           on the test that runs both and compares
    /// The reference model: <what it is the simple truth of>   on the model's type or function

and running to the end of the doc comment. It is the doc comment's first line, so the words in the
middle of other prose are not a statement. `crates/harness`, a workspace of its own, is not read. An invariant or a refinement is proved only with a
control beside it: a test in the same file named `the_oracle_catches_<the planted bug>`, which
plants a bug and requires the oracle to find it (rule dst-not-review: a green test with a blind
oracle proves nothing).

    uv run --frozen python harness/v2/invariants.py '{"command": "check"}'
    uv run --frozen python harness/v2/invariants.py '{"command": "write"}'
    uv run --frozen python harness/v2/invariants.py selftest

    command   check   every invariant and refinement has a control in its file, every control
                      has a statement in its file, no two statements share an id, and
                      invariants.md is what `write` would write (CI). So removing or rewording
                      an invariant is a visible change to that file in the same pull request
              write   regenerate invariants.md
    root      the checkout (".")

An entry's id is `<crate>::<item>`, so it moves only when the item is renamed.

Each invariant and refinement has a level, the strength of the evidence for it (docs/v2/plan.md,
"The ladder"):

    tested         a property test found no counterexample
    simulated      nor did the deterministic simulation, with faults, crashes and interleavings
    model-checked  none exists up to a bound (`blaze_check::model`)
    proved         none exists: a Lean proof, named in the statement

The level is read from the file (it uses the simulation, or the model checker), or said in the
statement's opening, which wins: `/// The invariant (proved: proofs/Store/Map.lean): ...`. A proof
named must be a file in the tree.
"""

import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import parse

SCRIPT = "invariants.py"
LIST = "docs/v2/plan/invariants.md"
type Kind = Literal["invariant", "refinement", "model"]
KINDS: dict[Kind, str] = {"invariant": "Invariants", "refinement": "Refinements", "model": "Reference models"}
OPEN = re.compile(r"^\s*/// The (invariant|refinement|reference model)(?: \(([a-z-]+)(?:: ([^)]+))?\))?: (.*)$")
LEVELS = ("tested", "simulated", "model-checked", "proved")
DOC = re.compile(r"^\s*///(?: (.*))?$")
ITEM = re.compile(r"^\s*(?:pub(?:\([^)]*\))? )?(?:const |async |unsafe )*(?:fn|struct|enum|trait|type) (\w+)")
CONTROL = re.compile(r"^\s*fn (the_oracle_catches_\w+)\(")


@dataclass(frozen=True)
class Entry:
    kind: Kind
    krate: str
    item: str
    statement: str
    path: str
    line: int
    level: str = ""
    proof: str = ""

    @property
    def id(self) -> str:
        return f"{self.krate}::{self.item}"


@dataclass(frozen=True)
class Found:
    entries: tuple[Entry, ...]
    controls: dict[str, tuple[str, ...]]  # by file


@dataclass(frozen=True)
class Input:
    command: Literal["check", "write"]
    root: str = "."


def crate_of(path: str) -> str:
    parts = path.split("/")
    at = parts.index("src") if "src" in parts else len(parts) - 1
    return parts[at - 1] if at >= 1 else parts[0]


def inferred(text: str) -> str:
    """The level a file's own machinery gives its statements."""
    if "blaze_check::" in text:
        return "model-checked"
    if re.search(r"\bsim::|\bSim\b|hegel::sim", text):
        return "simulated"
    return "tested"


def scan(path: str, text: str) -> tuple[list[Entry], list[str]]:
    """The statements and the controls of one file."""
    entries: list[Entry] = []
    controls: list[str] = []
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        if control := CONTROL.match(lines[i]):
            controls.append(control.group(1))
        opened = OPEN.match(lines[i])
        if opened is None or (i > 0 and DOC.match(lines[i - 1]) is not None):
            i += 1
            continue
        start, words = i, [opened.group(4)]
        i += 1
        while i < len(lines) and (more := DOC.match(lines[i])) is not None:
            words.append(more.group(1) or "")
            i += 1
        while i < len(lines) and lines[i].lstrip().startswith(("#[", "//")):
            i += 1
        item = ITEM.match(lines[i]) if i < len(lines) else None
        if item is None:
            raise SystemExit(f"{SCRIPT}: {path}:{start + 1}: the statement is on no fn, struct, enum, trait or type")
        statement = " ".join(w.strip() for w in words if w.strip())
        kind = opened.group(1).removeprefix("reference ")
        level = opened.group(2) or ("" if kind == "model" else inferred(text))
        if level and level not in LEVELS:
            raise SystemExit(f"{SCRIPT}: {path}:{start + 1}: the level `{level}` is none of {', '.join(LEVELS)}")
        entries.append(Entry(kind, crate_of(path), item.group(1), statement, path, start + 1, level, opened.group(3) or ""))  # type: ignore[arg-type]
    return entries, controls


def find(root: Path) -> Found:
    listed = subprocess.run(["git", "-C", str(root), "ls-files", "crates/*.rs", "harness/*.rs"],
                            capture_output=True, text=True, check=True).stdout.splitlines()
    entries: list[Entry] = []
    controls: dict[str, tuple[str, ...]] = {}
    for path in sorted(listed):
        if path.startswith("crates/harness/"):
            continue
        try:
            text = (root / path).read_text()
        except (OSError, UnicodeDecodeError):
            continue
        if "/// The " not in text and "fn the_oracle_catches_" not in text:
            continue
        found, its = scan(path, text)
        entries += found
        if its:
            controls[path] = tuple(its)
    return Found(tuple(entries), controls)


def problems(found: Found, root: Path | None = None) -> list[str]:
    out: list[str] = []
    for e in found.entries:
        if e.level == "proved" and not e.proof:
            out.append(f"{e.path}:{e.line}: {e.id} says proved and names no proof: `(proved: <path to the proof>)`")
        if e.proof and root is not None and not (root / e.proof).is_file():
            out.append(f"{e.path}:{e.line}: {e.id}'s proof {e.proof} is not a file in the tree")
    ids = [e.id for e in found.entries]
    out += [f"{i}: two statements have this id; rename one item" for i in sorted({i for i in ids if ids.count(i) > 1})]
    stated = {e.path for e in found.entries if e.kind != "model"}
    for e in found.entries:
        if e.kind != "model" and e.path not in found.controls:
            out.append(f"{e.path}:{e.line}: {e.id} has no control: add a test `the_oracle_catches_<bug>` that "
                       "plants a bug and requires this oracle to find it")
    for path, its in sorted(found.controls.items()):
        if path not in stated:
            out.append(f"{path}: {its[0]} controls no stated invariant: open the checked test's doc comment "
                       "with `/// The invariant: ...`")
    return out


def listing(found: Found) -> str:
    counts = {k: sum(e.kind == k for e in found.entries) for k in KINDS}
    lines = [
        "---", "status: current",
        "summary: Every invariant, refinement and reference model blaze states in its code, with the test that "
        "checks it and the planted bugs that prove the check; generated by harness/v2/invariants.py.",
        "---", "# Invariants", "",
        "<!-- Generated by harness/v2/invariants.py from the code: edit the doc comment there, then run `write`. -->", "",
        "What blaze's code promises, stated once, beside the test that checks it. Each is a doc comment",
        "opening `/// The invariant:`, `/// The refinement:` or `/// The reference model:`; an invariant or a",
        "refinement counts only with a control in its file, a test that plants a bug its oracle must",
        "catch. A change to this file in a pull request is a promise added, reworded or dropped.", "",
        f"{counts['invariant']} invariants, {counts['refinement']} refinements, {counts['model']} reference models.",
        "By level: " + ", ".join(f"{sum(e.level == lv for e in found.entries)} {lv}" for lv in LEVELS) + ".", "",
    ]
    for kind, title in KINDS.items():
        of_kind = [e for e in found.entries if e.kind == kind]
        if not of_kind:
            continue
        lines += [f"## {title}", ""]
        for krate in sorted({e.krate for e in of_kind}):
            lines += [f"### {krate}", ""]
            for e in (e for e in of_kind if e.krate == krate):
                level = f" *{e.level}*" + (f" (`{e.proof}`)" if e.proof else "") if e.level else ""
                lines.append(f"- **`{e.item}`**{level} (`{e.path}`): {e.statement}")
                if kind != "model" and (its := found.controls.get(e.path)):
                    lines.append("  Controls: " + ", ".join(f"`{c.removeprefix('the_oracle_catches_')}`" for c in its) + ".")
            lines.append("")
    return "\n".join(lines).rstrip("\n") + "\n"


def main(it: Input) -> None:
    root = Path(it.root).resolve()
    found = find(root)
    text = listing(found)
    if it.command == "write":
        (root / LIST).write_text(text)
        return
    wrong = problems(found, root)
    if not (root / LIST).is_file() or (root / LIST).read_text() != text:
        wrong.append(f"{LIST} is stale: run {SCRIPT} with command `write` and commit it")
    if wrong:
        sys.exit("\n".join(f"{SCRIPT}: {w}" for w in wrong))


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def selftest() -> None:
    text = (
        "use x;\n\n"
        "/// The invariant: a read hands back the bytes put,\n"
        "/// whatever the faults.\n"
        "#[hegel::test(test_cases = 200)]\n"
        "fn reads_hand_back_what_was_put(tc: TestCase) {}\n\n"
        "#[test]\nfn the_oracle_catches_a_failed_load_kept() {}\n\n"
        "/// The reference model: a map from key to bytes.\n"
        "pub(crate) struct Model { }\n"
    )
    entries, controls = scan("crates/blaze-cas/src/pool/tests.rs", text)
    expect([(e.kind, e.item, e.line) for e in entries] == [("invariant", "reads_hand_back_what_was_put", 3), ("model", "Model", 11)],
           f"two statements: {entries}")
    expect(entries[0].statement == "a read hands back the bytes put, whatever the faults.", entries[0].statement)
    expect(entries[0].id == "blaze-cas::reads_hand_back_what_was_put", entries[0].id)
    expect(controls == ["the_oracle_catches_a_failed_load_kept"], f"{controls}")
    found = Found(tuple(entries), {"crates/blaze-cas/src/pool/tests.rs": tuple(controls)})
    expect(problems(found) == [], f"a stated and controlled invariant has no problems: {problems(found)}")
    blind = Found(tuple(entries), {})
    expect(any("has no control" in p for p in problems(blind)), "an invariant with no control is a problem")
    expect(sum("has no control" in p for p in problems(blind)) == 1, "a model needs no control")
    loose = Found((), {"crates/a/src/t.rs": ("the_oracle_catches_x",)})
    expect(any("controls no stated invariant" in p for p in problems(loose)), "a control with no statement is a problem")
    twice = Found((entries[0], entries[0]), found.controls)
    expect(any("two statements have this id" in p for p in problems(twice)), "a repeated id is a problem")
    page = listing(found)
    expect("1 invariants, 0 refinements, 1 reference models." in page and "Controls: `a_failed_load_kept`." in page, page)
    expect(crate_of("crates/harness/harness-proto/src/migrate.rs") == "harness-proto", "a nested crate's name")
    inside, _ = scan("a.rs", "/// A type.\n/// The invariant: words in prose.\nstruct S;\n")
    expect(inside == [], f"words inside a doc comment are no statement: {inside}")
    try:
        scan("a.rs", "/// The invariant: x\nlet y = 1;\n")
        expect(False, "a statement on no item is refused")
    except SystemExit as e:
        expect("a.rs:1" in str(e), f"the refusal names the place: {e}")
    expect(entries[0].level == "tested" and entries[1].level == "", f"levels: {entries}")
    simmed, _ = scan("a.rs", "use blaze_core::sim::Sim;\n\n/// The invariant: x.\nfn t() {}\n")
    expect(simmed[0].level == "simulated", f"a file on the simulation is simulated: {simmed}")
    proved, _ = scan("a.rs", "/// The refinement (proved: proofs/A.lean): the map is the model.\nfn t() {}\n")
    expect((proved[0].level, proved[0].proof, proved[0].statement) == ("proved", "proofs/A.lean", "the map is the model."), f"{proved}")
    bare, _ = scan("a.rs", "/// The invariant (proved): x.\nfn t() {}\n")
    expect(any("names no proof" in p for p in problems(Found(tuple(bare), {"a.rs": ("the_oracle_catches_y",)}))), "proved needs its proof")
    try:
        scan("a.rs", "/// The invariant (certain): x.\nfn t() {}\n")
        expect(False, "an unknown level is refused")
    except SystemExit as e:
        expect("certain" in str(e), f"{e}")
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
