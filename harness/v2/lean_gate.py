"""lean_gate.py: blaze's Lean proofs, held by the gate (rule verified-compiler; ledger V5 to V10;
docs/v2/plan/design/formal-verification.md). A proof nothing rebuilds is a claim: this builds both
Lake packages with the pinned Lean, asks Lean which axioms each named theorem rests on, and holds
the sources and the list people read to what is checked in.

    uv run --frozen python harness/v2/lean_gate.py '{"command": "check"}'
    uv run --frozen python harness/v2/lean_gate.py '{"command": "sources"}'
    uv run --frozen python harness/v2/lean_gate.py '{"command": "write"}'
    uv run --frozen python harness/v2/lean_gate.py selftest

The input is one JSON object:

    command   sources  what needs no Lean (the `scripts` job, seconds): lean/theorems.json is well
                       formed, every theorem it names is stated in the module it names, each
                       ledger row it names is a row, no Lean source says `sorry`, `admit`,
                       `native_decide`, `decide +native`, `axiom`, `partial def` or
                       `debug.skipKernelTC` outside comments unless lean/theorems.json's `allow`
                       lists that file and word, the two packages pin one toolchain,
                       lean/analysis/lake-manifest.json is checked in and pins Mathlib at
                       lakefile.toml's `rev`, and docs/v2/plan/proofs.md is what `write` writes
              check    `sources`, then with the pinned `lake`: `lake build` in lean/; in
                       lean/analysis `lake exe cache get` (Mathlib's prebuilt files: Mathlib is
                       never compiled) and `lake build`; neither log may say `declaration uses
                       'sorry'`; then one generated Lean file per package runs `#print axioms` on
                       each theorem of lean/theorems.json, and each must exist and rest on nothing
                       but the axioms the file allows (`propext`, `Classical.choice`,
                       `Quot.sound`). Prints the table
              write    regenerate docs/v2/plan/proofs.md
    root      the checkout (".")
    lake      the pinned toolchain's `lake`, an absolute path; empty for the one elan keeps for
              lean/lean-toolchain's pin (`~/.elan/toolchains/<pin>/bin/lake`). Never PATH
    packages  which packages `check` builds: `core` (lean/) and `analysis` (lean/analysis); both
    out       where the verdict goes for the ledger (scripts/src/bin/ledger.rs), when given

lean/theorems.json is the one list of what blaze claims is proved:

    axioms    the axioms a theorem may rest on
    allow     `{path, word, why}`: a source that may say a refused word
    theorems  `{name, package, module, says, row}`: the theorem's full name, its package, the
              module that states it, what it says in words (one line), and its ledger row

`lean/analysis/lake-manifest.json` is checked in, so Mathlib's own dependencies are pinned too
(lakefile.toml pins Mathlib alone). It is written by `lake update` in lean/analysis with the pinned
toolchain, and only then: the gate never runs `lake update`, so a build never resolves anything.
"""

import json
import re
import subprocess
import sys
import tempfile
import time
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from mir_semantics import lake_of
from typed_input import parse

SCRIPT = "lean_gate.py"
LIST = "lean/theorems.json"
PAGE = "docs/v2/plan/proofs.md"
LEDGER = "docs/v2/plan/objectives.json"
MANIFEST = "lean/analysis/lake-manifest.json"
type Package = Literal["core", "analysis"]
# A package's directory, and the directories its modules are found under, in order.
PACKAGES: dict[str, tuple[str, tuple[str, ...]]] = {
    "core": ("lean", ("lean", "crates/blaze-graph/lean")),
    "analysis": ("lean/analysis", ("lean/analysis",)),
}
# What no proof may say, outside comments: each defeats the kernel's check or adds to what is trusted.
REFUSED: dict[str, re.Pattern[str]] = {
    "sorry": re.compile(r"(?<![\w.'])sorry(?![\w'])"),
    "admit": re.compile(r"(?<![\w.'])admit(?![\w'])"),
    "native_decide": re.compile(r"native_decide"),
    "decide +native": re.compile(r"\+native\b"),
    "axiom": re.compile(r"^\s*(?:(?:private|protected|noncomputable|unsafe)\s+)*axiom\s", re.MULTILINE),
    "partial def": re.compile(r"\bpartial\s+def\b"),
    "debug.skipKernelTC": re.compile(r"skipKernelTC"),
}
SORRY = "declaration uses 'sorry'"
AXIOMS = re.compile(r"'([^'\s]+)' depends on axioms: \[([^\]]*)\]")
NO_AXIOMS = re.compile(r"'([^'\s]+)' does not depend on any axioms")
NAME = re.compile(r"^[A-Za-z_][\w']*(\.[A-Za-z_][\w']*)*$")


@dataclass(frozen=True)
class Theorem:
    name: str
    package: Package
    module: str
    says: str
    row: str


@dataclass(frozen=True)
class Allow:
    path: str
    word: str
    why: str


@dataclass(frozen=True)
class Listed:
    axioms: tuple[str, ...]
    theorems: tuple[Theorem, ...]
    allow: tuple[Allow, ...] = ()


@dataclass(frozen=True)
class Input:
    command: Literal["sources", "check", "write"]
    root: str = "."
    lake: str = ""
    packages: tuple[Package, ...] = ("core", "analysis")
    out: str = ""


def without_comments(text: str) -> str:
    """Lean source with its comments (`--` to the line's end, `/- .. -/` nested) and the insides of
    its string literals blanked; newlines stay, so a line number holds."""
    out: list[str] = []
    i, depth, n = 0, 0, len(text)
    while i < n:
        two = text[i:i + 2]
        if depth:
            if two == "/-":
                depth, i = depth + 1, i + 2
            elif two == "-/":
                depth, i = depth - 1, i + 2
            else:
                out.append("\n" if text[i] == "\n" else " ")
                i += 1
        elif two == "/-":
            depth, i = 1, i + 2
        elif two == "--":
            while i < n and text[i] != "\n":
                i += 1
        elif text[i] == '"':
            out.append('"')
            i += 1
            while i < n and text[i] != '"':
                i += 2 if text[i] == "\\" else 1
                out.append(" ")
            out.append('"')
            i += 1
        else:
            out.append(text[i])
            i += 1
    return "".join(out)


def refused_words(path: str, text: str, allow: tuple[Allow, ...]) -> list[str]:
    code = without_comments(text)
    allowed = {a.word for a in allow if a.path == path}
    out: list[str] = []
    for word, pattern in REFUSED.items():
        if word in allowed:
            continue
        for found in pattern.finditer(code):
            line = code.count("\n", 0, found.start()) + 1
            out.append(f"{path}:{line}: `{word}`: a proof may not say it (allowed only by an entry of {LIST}'s `allow`)")
    return out


def module_path(root: Path, t: Theorem) -> Path | None:
    for under in PACKAGES[t.package][1]:
        path = root / under / (t.module.replace(".", "/") + ".lean")
        if path.is_file():
            return path
    return None


def statement(text: str, name: str) -> str | None:
    """The statement of `name` as its module states it: from `theorem` up to its `:=` or `where`.
    The module may state it by a suffix of its full name (inside a namespace); the longest wins."""
    parts = name.split(".")
    for at in range(len(parts)):
        short = re.escape(".".join(parts[at:]))
        found = re.search(rf"^theorem {short}(?=[\s:({{\[])", text, re.MULTILINE)
        if found is None:
            continue
        rest = text[found.start():]
        end = re.search(r"\s*:=|\s+where\s*$", rest, re.MULTILINE)
        return (rest[:end.start()] if end else rest).rstrip()
    return None


def lean_sources(root: Path) -> list[str]:
    listed = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-co", "--exclude-standard", "lean/*.lean", "crates/*.lean"],
        capture_output=True, text=True, check=True).stdout.splitlines()
    return sorted(p for p in listed if "/.lake/" not in p)


def mathlib_rev(lakefile: str) -> str:
    for need in tomllib.loads(lakefile).get("require", []):
        if need.get("name") == "mathlib":
            return str(need.get("rev", ""))
    return ""


def manifest_problems(lakefile: str, manifest: str | None) -> list[str]:
    """The checked-in manifest pins Mathlib at the commit lakefile.toml names."""
    want = mathlib_rev(lakefile)
    if not want:
        return ["lean/analysis/lakefile.toml requires no `mathlib` with a `rev`"]
    if manifest is None:
        return [(f"{MANIFEST} is not checked in: in lean/analysis, with the pinned toolchain, run `lake update` "
                 "(it resolves Mathlib and its dependencies and writes the file), then commit it")]
    try:
        packages = json.loads(manifest)["packages"]
        pinned = {p["name"]: p.get("rev", "") for p in packages}
    except (json.JSONDecodeError, KeyError, TypeError) as e:
        return [f"{MANIFEST} is not a Lake manifest: {e}"]
    if pinned.get("mathlib") != want:
        return [(f"{MANIFEST} pins mathlib at `{pinned.get('mathlib')}` and lakefile.toml at `{want}`: "
                 "run `lake update` in lean/analysis and commit the manifest")]
    return [f"{MANIFEST}: `{name}` has no pinned commit" for name, rev in sorted(pinned.items())
            if not rev and name != "blaze"]


def list_problems(listed: Listed, rows: set[str] | None) -> list[str]:
    out: list[str] = []
    names = [t.name for t in listed.theorems]
    out += [f"{LIST}: `{n}` is listed twice" for n in sorted({n for n in names if names.count(n) > 1})]
    for t in listed.theorems:
        if NAME.match(t.name) is None or NAME.match(t.module) is None:
            out.append(f"{LIST}: `{t.name}` in `{t.module}`: a name is dotted identifiers")
        if not t.says.strip() or "\n" in t.says:
            out.append(f"{LIST}: `{t.name}` says nothing in one line")
        if rows is not None and t.row not in rows:
            out.append(f"{LIST}: `{t.name}` names the ledger row `{t.row}`, which is no row of {LEDGER}")
    if not listed.axioms:
        out.append(f"{LIST}: `axioms` is empty: say which axioms a theorem may rest on")
    return out


def page(listed: Listed, statements: dict[str, str]) -> str:
    by_row: dict[str, list[Theorem]] = {}
    for t in listed.theorems:
        by_row.setdefault(t.row, []).append(t)
    lines = [
        "---", "status: current",
        ("summary: Every theorem blaze claims is proved in Lean, with its statement as its module states it and its "
         "ledger row; generated by harness/v2/lean_gate.py from lean/theorems.json, and held by the gate to the axioms listed."),
        "---", "# Proofs", "",
        "<!-- Generated by harness/v2/lean_gate.py from lean/theorems.json and the Lean sources: edit those, then run `write`. -->", "",
        "What blaze has proved in Lean, by ledger row ([objectives](objectives.md)). The gate",
        "(`harness/v2/lean_gate.py`, command `check`) builds `lean/` and `lean/analysis` with the pinned Lean,",
        "refuses a `sorry` anywhere, and requires each theorem here to exist and to rest on no axiom but",
        ", ".join(f"`{a}`" for a in listed.axioms) + ". What each proof still trusts is in",
        "[formal-verification.md](design/formal-verification.md) (\"The trusted base\"). A change to this file",
        "in a pull request is a theorem added, restated or dropped.", "",
        f"{len(listed.theorems)} theorems: " + ", ".join(
            f"{sum(t.package == p for t in listed.theorems)} in `{PACKAGES[p][0]}`" for p in PACKAGES) + ".", "",
    ]
    for row in sorted(by_row, key=lambda r: (r[:1], int(r[1:]) if r[1:].isdigit() else 0)):
        lines += [f"## {row}", ""]
        for t in by_row[row]:
            lines += [f"**`{t.name}`** (`{t.module}`, `{PACKAGES[t.package][0]}`): {t.says}", "", "```lean",
                      statements.get(t.name, "-- not found in its module"), "```", ""]
    return "\n".join(lines).rstrip("\n") + "\n"


def read_list(root: Path) -> Listed:
    return parse(Listed, str(root / LIST), SCRIPT)


def sources(root: Path, listed: Listed) -> tuple[list[str], dict[str, str]]:
    """What is wrong that needs no Lean, and each theorem's statement."""
    rows = {r["id"] for r in json.loads((root / LEDGER).read_text())["rows"]}
    wrong = list_problems(listed, rows)
    statements: dict[str, str] = {}
    for t in listed.theorems:
        path = module_path(root, t)
        said = statement(path.read_text(), t.name) if path else None
        if said is None:
            wrong.append(f"{LIST}: `{t.name}` is not stated in `{t.module}` ({PACKAGES[t.package][0]})")
        else:
            statements[t.name] = said
    files = lean_sources(root)
    for path in files:
        wrong += refused_words(path, (root / path).read_text(), listed.allow)
    for a in listed.allow:
        text = (root / a.path).read_text() if (root / a.path).is_file() else ""
        if not REFUSED.get(a.word) or REFUSED[a.word].search(without_comments(text)) is None:
            wrong.append(f"{LIST}: `allow` lets {a.path} say `{a.word}`, which it does not say: remove the entry")
    pins = {p: (root / d / "lean-toolchain").read_text().strip() for p, (d, _) in PACKAGES.items()}
    if len(set(pins.values())) != 1:
        wrong.append(f"the packages pin different toolchains: {pins}")
    manifest = root / MANIFEST
    wrong += manifest_problems((root / "lean/analysis/lakefile.toml").read_text(),
                               manifest.read_text() if manifest.is_file() else None)
    if not (root / PAGE).is_file() or (root / PAGE).read_text() != page(listed, statements):
        wrong.append(f"{PAGE} is stale: run {SCRIPT} with command `write` and commit it")
    return wrong, statements


def axioms_file(theorems: list[Theorem]) -> str:
    """The Lean file that asks, per theorem, which axioms it rests on."""
    modules = sorted({t.module for t in theorems})
    return "\n".join([*(f"import {m}" for m in modules), "", *(f"#print axioms {t.name}" for t in theorems)]) + "\n"


def axioms_said(output: str) -> dict[str, tuple[str, ...]]:
    """Per theorem, the axioms Lean printed for it."""
    said: dict[str, tuple[str, ...]] = {}
    for found in AXIOMS.finditer(output):
        said[found.group(1)] = tuple(a.strip() for a in found.group(2).split(",") if a.strip())
    for found in NO_AXIOMS.finditer(output):
        said[found.group(1)] = ()
    return said


def axiom_problems(theorems: list[Theorem], said: dict[str, tuple[str, ...]], allowed: tuple[str, ...]) -> list[str]:
    out: list[str] = []
    for t in theorems:
        if t.name not in said:
            out.append(f"`{t.name}`: Lean printed no axioms for it: it does not exist in `{t.module}`, or the module did not load")
            continue
        extra = [a for a in said[t.name] if a not in allowed]
        if extra:
            out.append(f"`{t.name}` rests on {', '.join(f'`{a}`' for a in extra)}, beyond {', '.join(allowed)}"
                       + (": it is not proved (`sorryAx` is a `sorry`)" if "sorryAx" in extra else ""))
    return out


def lake_run(argv: list[str], cwd: Path) -> tuple[int, str, float]:
    start = time.monotonic()
    done = subprocess.run(argv, cwd=cwd, capture_output=True, text=True, check=False)
    took = time.monotonic() - start
    print(f"{SCRIPT}: `{' '.join(argv[1:])}` in {cwd}: exit {done.returncode}, {took:.0f} s", flush=True)
    return done.returncode, done.stdout + done.stderr, took


def build(root: Path, lake: Path, package: str) -> list[str]:
    cwd = root / PACKAGES[package][0]
    steps = [[str(lake), "build"]]
    if package == "analysis":
        steps.insert(0, [str(lake), "exe", "cache", "get"])
    for argv in steps:
        code, log, _ = lake_run(argv, cwd)
        if code != 0:
            return [f"`lake {' '.join(argv[1:])}` in {PACKAGES[package][0]} failed ({code}):\n{log[-6000:]}"]
        if SORRY in log:
            lines = [ln for ln in log.splitlines() if SORRY in ln]
            return [f"`lake build` in {PACKAGES[package][0]}: {len(lines)} declarations use `sorry`:\n" + "\n".join(lines[:20])]
    return []


def axioms(root: Path, lake: Path, package: str, listed: Listed) -> tuple[list[str], dict[str, tuple[str, ...]]]:
    theorems = [t for t in listed.theorems if t.package == package]
    if not theorems:
        return [], {}
    with tempfile.TemporaryDirectory(prefix="lean-gate-") as tmp:
        file = Path(tmp) / "Axioms.lean"
        file.write_text(axioms_file(theorems))
        code, log, _ = lake_run([str(lake), "env", "lean", str(file)], root / PACKAGES[package][0])
    said = axioms_said(log)
    wrong = axiom_problems(theorems, said, listed.axioms)
    if wrong and code != 0:
        wrong.append(f"Lean said, of {PACKAGES[package][0]}'s axioms file:\n{log[-3000:]}")
    return wrong, said


def table(listed: Listed, said: dict[str, tuple[str, ...]]) -> str:
    width = max((len(t.name) for t in listed.theorems), default=0)
    rows = [f"{'theorem':<{width}}  row  axioms"]
    for t in listed.theorems:
        got = said.get(t.name)
        rows.append(f"{t.name:<{width}}  {t.row:<3}  " + ("not asked" if got is None else ", ".join(got) or "none"))
    return "\n".join(rows)


def main(it: Input) -> None:
    root = Path(it.root).resolve()
    listed = read_list(root)
    wrong, statements = sources(root, listed)
    if it.command == "write":
        (root / PAGE).write_text(page(listed, statements))
        return
    said: dict[str, tuple[str, ...]] = {}
    if it.command == "check" and not wrong:
        lake = lake_of(it.lake, root, Path.home())
        for package in it.packages:
            wrong += build(root, lake, package)
            if wrong:
                break
            its, heard = axioms(root, lake, package, listed)
            wrong += its
            said |= heard
        print(table(listed, said))
    if it.out:
        Path(it.out).write_text(json.dumps({"outcomes": [
            {"name": "lean-gate", "verdict": "fail" if wrong else "pass", "detail": wrong[0].splitlines()[0] if wrong else ""}]}) + "\n")
    if wrong:
        sys.exit("\n".join(f"{SCRIPT}: {w}" for w in wrong))
    asked = "sources hold" if it.command == "sources" else f"{len(said)} theorems rest on the allowed axioms alone"
    print(f"{SCRIPT}: ok: {asked}")


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def selftest() -> None:
    code = without_comments('def a := 1 -- sorry here\n/- sorry /- nested sorry -/ still -/ def b := "sorry \\" sorry"\nexact x\n')
    expect("sorry" not in code and "def b" in code and code.count("\n") == 3, f"comments and strings are blanked: {code!r}")
    none: tuple[Allow, ...] = ()
    expect(refused_words("A.lean", "theorem t : True := by\n  sorry\n", none)[0].startswith("A.lean:2: `sorry`"), "a sorry is refused, by line")
    expect(refused_words("A.lean", "-- sorry\n/-- no sorry -/\ntheorem t : True := trivial\n", none) == [], "a sorry in a comment is not one")
    expect(refused_words("A.lean", "theorem sorry_free : True := trivial\ndef x := Foo.sorry'\n", none) == [], "a name holding the word is not one")
    for text, word in (("example : 1 = 1 := by native_decide\n", "native_decide"), ("example : 1 = 1 := by decide +native\n", "decide +native"),
                       ("axiom choice : False\n", "axiom"), ("private axiom c : False\n", "axiom"), ("partial def f : Nat := f\n", "partial def"),
                       ("example : True := by admit\n", "admit"), ("set_option debug.skipKernelTC true\n", "debug.skipKernelTC")):
        expect(any(f"`{word}`" in p for p in refused_words("A.lean", text, none)), f"`{word}` is refused in {text!r}")
    expect(refused_words("A.lean", "theorem t (h : axiom_free) : True := trivial\n", none) == [], "`axiom` inside a line is not a declaration")
    allow = (Allow("A.lean", "partial def", "a loop over stdin"),)
    expect(refused_words("A.lean", "partial def f : Nat := f\n", allow) == [], "an allowed word in its file passes")
    expect(refused_words("B.lean", "partial def f : Nat := f\n", allow) != [], "and in no other file")

    text = ("namespace Blaze.Mir\n\n/-- Doc. -/\ntheorem run_mono (prog : Program) :\n    ∀ (n k : Nat), run prog (n + k) = r := by\n  intro n\n\n"
            "theorem State.read_write_other (st : State) : a = b := rfl\n\ntheorem run_monotone : True := trivial\n"
            "theorem sound : Rewrite.AllSound where\n  a := b\n")
    expect(statement(text, "Blaze.Mir.run_mono") == "theorem run_mono (prog : Program) :\n    ∀ (n k : Nat), run prog (n + k) = r", f"{statement(text, 'Blaze.Mir.run_mono')!r}")
    expect(statement(text, "Blaze.Mir.State.read_write_other") == "theorem State.read_write_other (st : State) : a = b", "a dotted suffix")
    expect(statement(text, "Blaze.Mir.sound") == "theorem sound : Rewrite.AllSound", "a structure instance ends at `where`")
    expect(statement(text, "Blaze.Mir.run_mon") is None, "a prefix of another name is not it")

    theorems = [Theorem("Blaze.Mir.run_mono", "core", "Blaze.Mir.Run", "a run that ends, ends the same with more fuel", "V6"),
                Theorem("Blaze.Mir.Opt.validate_sound", "core", "Blaze.Mir.Opt.Validate", "an accepted pair runs alike", "V8")]
    listed = Listed(("propext", "Classical.choice", "Quot.sound"), tuple(theorems))
    expect(axioms_file(theorems) == "import Blaze.Mir.Opt.Validate\nimport Blaze.Mir.Run\n\n#print axioms Blaze.Mir.run_mono\n#print axioms Blaze.Mir.Opt.validate_sound\n",
           axioms_file(theorems))
    said = axioms_said("'Blaze.Mir.run_mono' depends on axioms: [propext,\n Classical.choice]\n'Blaze.Mir.Opt.validate_sound' does not depend on any axioms\n")
    expect(said == {"Blaze.Mir.run_mono": ("propext", "Classical.choice"), "Blaze.Mir.Opt.validate_sound": ()}, f"{said}")
    expect(axiom_problems(theorems, said, listed.axioms) == [], "the three axioms and none are allowed")
    sorried = axioms_said("'Blaze.Mir.run_mono' depends on axioms: [sorryAx]\n")
    wrong = axiom_problems(theorems, sorried, listed.axioms)
    expect(len(wrong) == 2 and "`sorryAx`" in wrong[0] and "printed no axioms" in wrong[1], f"a sorry and a missing theorem: {wrong}")
    native = axioms_said("'Blaze.Mir.run_mono' depends on axioms: [propext, Lean.ofReduceBool]\n")
    expect("`Lean.ofReduceBool`" in axiom_problems(theorems[:1], native, listed.axioms)[0], "native evaluation is refused")
    expect("Blaze.Mir.run_mono" in table(listed, said) and "none" in table(listed, said), table(listed, said))

    expect(list_problems(listed, {"V6", "V8"}) == [], f"{list_problems(listed, {'V6', 'V8'})}")
    expect(any("no row" in p for p in list_problems(listed, {"V6"})), "an unknown ledger row is a problem")
    expect(any("listed twice" in p for p in list_problems(Listed(listed.axioms, (theorems[0], theorems[0])), None)), "a theorem listed twice")
    expect(any("`axioms` is empty" in p for p in list_problems(Listed((), tuple(theorems)), None)), "no allowed axioms is a problem")

    lakefile = '[[require]]\nname = "mathlib"\ngit = "https://x"\nrev = "abc"\n\n[[require]]\nname = "blaze"\npath = ".."\n'
    expect(mathlib_rev(lakefile) == "abc", mathlib_rev(lakefile))
    expect("is not checked in" in manifest_problems(lakefile, None)[0], "a missing manifest is a problem")
    good = json.dumps({"packages": [{"name": "mathlib", "rev": "abc"}, {"name": "batteries", "rev": "d"}, {"name": "blaze", "type": "path"}]})
    expect(manifest_problems(lakefile, good) == [], f"{manifest_problems(lakefile, good)}")
    moved = json.dumps({"packages": [{"name": "mathlib", "rev": "abd"}]})
    expect("pins mathlib at `abd`" in manifest_problems(lakefile, moved)[0], "a manifest at another commit is a problem")
    expect("not a Lake manifest" in manifest_problems(lakefile, "{}")[0], "a file that is no manifest is a problem")

    text = page(listed, {"Blaze.Mir.run_mono": "theorem run_mono : True"})
    expect("## V6\n\n**`Blaze.Mir.run_mono`** (`Blaze.Mir.Run`, `lean`): a run that ends" in text and "```lean\ntheorem run_mono : True\n```" in text, text)
    expect("2 theorems: 2 in `lean`, 0 in `lean/analysis`." in text and "-- not found in its module" in text, text)
    given = parse(Input, '{"command": "check", "packages": ["core"]}', SCRIPT)
    expect(given.root == "." and given.lake == "" and given.packages == ("core",), f"the input: {given}")
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
