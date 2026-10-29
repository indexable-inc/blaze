# pyright: basic
"""check_json.py: `blaze check --message-format=...` against `cargo check` on the same errors.

    uv run --frozen python harness/check_json.py '{}'
    uv run --frozen python harness/check_json.py '{"cases": ["two_codes"], "keep": true}'

One JSON object, inline, a file's path, or `-` for stdin: `cases` selects case names (empty
runs every case), `keep` keeps the scratch directory, and `blaze_bin` names blaze-engine
(empty uses target/release/blaze-engine). Unknown fields and case names are refused.

Each case below is a small package with errors the daemon reports. It is checked by plain
cargo (rustc) and by `blaze check` (the daemon's checker, which runs no rustc), in a
daemon home of its own, with each `--message-format` an editor or a person asks for:

  json                          every error's `compiler-message` has to be rustc's: the package,
                                manifest and target cargo names, `$message_type`, level, code,
                                and the primary span (file, bytes, lines, columns, the source
                                lines and what of them is highlighted); then the failure notes,
                                `build-finished` and cargo's `could not compile` line on stderr
  json-diagnostic-rendered-ansi the `rendered` header, `-->` and source lines in rustc's colours
  short                         each `file:line:column: error[CODE]: ` on stderr
  human                         each `error[CODE]: ` header and `-->` line, the notes and the
                                `could not compile` line

What only rustc has is left out of the comparison and counted: a message worded otherwise
(the span and code are still compared), the labels and children (help, notes, the other
spans of an error) and `code.explanation`. A package without errors has to give
`build-finished` with success true; rustc's `compiler-artifact` lines are not compared.

`blaze_bin` (default target/release/blaze-engine) is the blaze with `blaze` and `blaze-rustc`
next to it. Exits 1 when anything compared differs.
"""
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from typed_input import fail, parse

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
EXE = ".exe" if os.name == "nt" else ""
BLAZE = os.path.join(ROOT, "target/release/blaze-engine" + EXE)
TOOLCHAIN = Path(HERE, "toolchain").read_text().strip()
ALLOW = "#![allow(unused, dead_code)]\n"

MANIFEST = """[package]
name = "{name}"
version = "0.1.0"
edition = "2021"
"""

CASES = {
    # Two codes, so both failure notes.
    "two_codes": {
        "src/main.rs": ALLOW + "fn main() {\n    let _ = y;\n}\n\nfn f() -> u32 {\n    \"a\"\n}\n",
    },
    # One code twice: the note of a single code, and the count.
    "one_code_twice": {
        "src/main.rs": ALLOW + "fn main() {\n    let _ = y;\n    let _ = z;\n}\n",
    },
    # A tab before the error and a character of more than one byte on its line: columns are
    # counted in characters, a tab as one, and the line shown with the tab as four spaces.
    "tab_and_wide_chars": {
        "src/main.rs": ALLOW + "fn main() {\n\tlet s = \"h\u00e9llo\"; let _: u32 = s;\n}\n",
    },
    # The error is in a module file of a library that a binary of the same package uses: the
    # unit is the library.
    "lib_module": {
        "src/lib.rs": ALLOW + "pub mod m;\n",
        "src/m.rs": "pub fn g() -> u32 {\n    true\n}\n",
        "src/main.rs": "fn main() {\n    probe::m::g();\n}\n",
    },
    # Calls and methods.
    "calls": {
        "src/main.rs": ALLOW + "fn two(a: u32, b: u32) {}\n\nfn main() {\n    two(1);\n    let v = 1u32;\n    v.no_such_method();\n}\n",
    },
    # A borrow check error, which rustc gives labels and notes.
    "moved": {
        "src/main.rs": ALLOW + "fn main() {\n    let s = String::new();\n    let t = s;\n    let u = s;\n}\n",
    },
    # Nothing wrong.
    "fine": {
        "src/main.rs": ALLOW + "fn main() {\n    let _ = 1;\n}\n",
    },
    # Borrow checks blaze got wrong in uv, ruff and ty, reduced by Charlie Marsh
    # (astral-experiments/blaze). A trait object's outlives components include both sides of
    # an associated type binding.
    "dyn_associated_type_outlives": {
        "src/main.rs": ALLOW + """trait Buffer { type Context; }
struct Formatter<'buf, T>(&'buf mut dyn Buffer<Context = T>);

fn source<'buf, 'ast>(_: Formatter<'buf, &'ast str>, text: &'ast str) -> &'buf str { text }
fn main() {}
""",
    },
    # A bound on an associated type also holds of what it normalizes to inside an aggregate.
    "nested_projection_outlives": {
        "src/main.rs": ALLOW + """struct State<T>(T);
trait Buffer {
    type Context;
    fn state(&self) -> &State<Self::Context>;
    fn state_mut(&mut self) -> &mut State<Self::Context>;
}
impl<W: Buffer<Context = C> + ?Sized, C> Buffer for &mut W {
    type Context = C;
    fn state(&self) -> &State<Self::Context> { (**self).state() }
    fn state_mut(&mut self) -> &mut State<Self::Context> { (**self).state_mut() }
}
fn main() {}
""",
    },
    # A promoted enum value is qualified by the fields of its active variant.
    "promoted_enum_array": {
        "src/main.rs": ALLOW + """enum Element { Token(&'static str), Owned(String) }
const OPEN: Element = Element::Token("(");
const ALIAS: Element = OPEN;
fn tokens() -> &'static [Element] { &[ALIAS, Element::Token(")")] }
fn main() { assert_eq!(tokens().len(), 2); }
""",
    },
    "unpromoted_enum_array": {
        "src/main.rs": ALLOW + """enum Element { Token, Owned(String) }
const OWNED: Element = Element::Owned(String::new());
fn tokens() -> &'static [Element] { &[Element::Token, OWNED] }
fn main() {}
""",
    },
    # An elided return lifetime is the borrow's, not the `'static` a trait object's supertrait
    # implies, also for a closure's parameter; only `escape` is wrong.
    "static_supertrait_object": {
        "src/main.rs": ALLOW + """trait Database: 'static {
    fn get(&self) -> &u32;
    fn get_mut(&mut self) -> &mut u32;
}
fn read(db: &dyn Database) -> &u32 { db.get() }
fn write(db: &mut dyn Database) -> &mut u32 { db.get_mut() }
fn named<'db>(db: &'db dyn Database) -> &u32 { db.get() }
fn through_closure<'db>(db: &'db dyn Database) -> &'db u32 {
    (|db: &'db dyn Database| db.get())(db)
}
fn escape(db: &dyn Database) -> &'static u32 {
    db.get()
}
fn main() {}
""",
    },
}

# Errors whose primary span rust-analyzer places otherwise than rustc, and why: only their
# codes are compared, and they are counted.
PLACED_OTHERWISE = {
    "E0061": "rust-analyzer points at where the missing argument goes, rustc at the function called",
}

FORMATS = ["json", "json-diagnostic-rendered-ansi", "short", "human"]
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def env(home):
    it = dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN, BLAZE_HOME=home, RUST_BACKTRACE="0")
    it["BLAZE_CACHE"] = os.path.join(home, "store")
    it["PATH"] = os.path.dirname(BLAZE) + os.pathsep + it["PATH"]
    for name in ("RUSTFLAGS", "CARGO_TARGET_DIR", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_TARGET", "CARGO_TERM_COLOR"):
        it.pop(name, None)
    return it


def make(work, name, files):
    at = os.path.join(work, name)
    shutil.rmtree(at, ignore_errors=True)
    os.makedirs(at)
    with open(os.path.join(at, "Cargo.toml"), "w") as f:
        f.write(MANIFEST.format(name="probe"))
    for path, text in files.items():
        os.makedirs(os.path.dirname(os.path.join(at, path)), exist_ok=True)
        with open(os.path.join(at, path), "w") as f:
            f.write(text)
    return at


def run(at, command, fmt, home):
    """stdout's JSON messages, stderr's lines and the status of `COMMAND check` (cargo's with none)."""
    args = [*(command or ["cargo"]), "check", f"--message-format={fmt}"]
    done = subprocess.run(args, cwd=at, env=env(home), capture_output=True, text=True, timeout=600, check=False)
    messages = [json.loads(line) for line in done.stdout.splitlines() if line.startswith("{")]
    return messages, done.stderr.splitlines(), done.returncode


def primary(message):
    spans = [s for s in message["spans"] if s["is_primary"]] or message["spans"]
    return spans[0] if spans else None


def key(message):
    """What finds rustc's error for ours: the code and where the primary span starts."""
    d = message["message"]
    span = primary(d)
    code = d["code"] and d["code"]["code"]
    if code in PLACED_OTHERWISE:
        return (code, span and span["file_name"])
    return (code, span and (span["file_name"], span["line_start"], span["column_start"]))


SPAN_FIELDS = ["file_name", "byte_start", "byte_end", "line_start", "line_end", "column_start", "column_end", "is_primary"]


def frame(rendered, span):
    """The lines of `rendered` that blaze writes as rustc does: the header without the message,
    the first `-->` line, and the source lines of the primary `span` with their gutter, the
    first time each is shown (rustc shows the lines of its other spans and suggestions too)."""
    lines = rendered.splitlines()
    out, arrow, shown = [], False, set()
    if lines:
        out.append(re.sub(r":.*", ":", lines[0], count=1))
    for line in lines[1:]:
        plain = ANSI.sub("", line)
        number = re.match(r"^\s*(\d+)\s*\|", plain)
        if "-->" in plain and not arrow:
            out.append(line)
            arrow = True
        elif number and span["line_start"] <= int(number[1]) <= span["line_end"] and number[1] not in shown:
            out.append(line)
            shown.add(number[1])
    return out


class Tally:
    def __init__(self):
        self.differs = []
        self.compared = 0
        self.placed_otherwise = 0
        self.rustc_only = {"worded otherwise": 0, "labels": 0, "children": 0, "explanation": 0, "other spans": 0}

    def same(self, case, fmt, what, ours, theirs):
        self.compared += 1
        if ours != theirs:
            self.differs.append(f"{case} {fmt}: {what}\n      blaze: {ours!r}\n      rustc:  {theirs!r}")


def compare_json(tally, case, fmt, ours, theirs):
    def errors(messages):
        return [m for m in messages if m.get("reason") == "compiler-message" and m["message"]["level"] == "error"]

    def notes(messages):
        return [m["message"]["rendered"] for m in messages if m.get("reason") == "compiler-message" and m["message"]["level"] == "failure-note"]

    def finished(messages):
        return [m for m in messages if m.get("reason") == "build-finished"]

    rustc = {key(m): m for m in errors(theirs)}
    mine = {key(m): m for m in errors(ours)}
    tally.same(case, fmt, "errors found", sorted(map(str, mine)), sorted(map(str, rustc)))
    for k, m in mine.items():
        r = rustc.get(k)
        if r is None:
            continue
        for field in ("reason", "package_id", "manifest_path", "target"):
            tally.same(case, fmt, field, m.get(field), r.get(field))
        d, e = m["message"], r["message"]
        tally.same(case, fmt, "$message_type", d.get("$message_type"), e.get("$message_type"))
        tally.same(case, fmt, "level", d["level"], e["level"])
        ps, pr = primary(d), primary(e)
        if k[0] in PLACED_OTHERWISE:
            tally.placed_otherwise += 1
            continue
        if ps is None or pr is None:
            fail("check_json.py", f"{case} {fmt}: compiler error {k} has no span")
        tally.same(case, fmt, "rendered frame", frame(d["rendered"], ps), frame(e["rendered"], pr))
        for field in SPAN_FIELDS:
            tally.same(case, fmt, f"span {field}", ps[field], pr[field])
        tally.same(case, fmt, "span text", ps["text"], pr["text"])
        tally.same(case, fmt, "rendered ends", d["rendered"][-2:], e["rendered"][-2:])
        if d["message"] != e["message"]:
            tally.rustc_only["worded otherwise"] += 1
        tally.rustc_only["labels"] += sum(1 for s in e["spans"] if s["label"])
        tally.rustc_only["children"] += len(e["children"])
        tally.rustc_only["explanation"] += 1 if e["code"] and e["code"].get("explanation") else 0
        tally.rustc_only["other spans"] += len(e["spans"]) - 1
    tally.same(case, fmt, "failure notes", notes(ours), notes(theirs))
    tally.same(case, fmt, "build-finished", finished(ours), finished(theirs))


def compare_text(tally, case, fmt, ours, theirs):
    def placed(code, at):
        return at.rsplit(":", 2)[0] if code in PLACED_OTHERWISE else at

    def heads(lines):
        if fmt == "short":
            found = (re.match(r"^(\S+:\d+:\d+): error(?:\[(E\d+)\])?: ", ANSI.sub("", l)) for l in lines)
            return sorted((m[2], placed(m[2], m[1])) for m in found if m)
        # Each error's code with the first `-->` after its header.
        out = []
        for line in map(ANSI.sub, [""] * len(lines), lines):
            head = re.match(r"^error(?:\[(E\d+)\])?: ", line)
            arrow = re.match(r"^\s*--> (\S+)", line)
            if head:
                out.append([head[1], None])
            elif arrow and out and out[-1][1] is None:
                out[-1][1] = placed(out[-1][0], arrow[1])
        return sorted(map(tuple, out), key=str)
    tally.same(case, fmt, "headers", heads(ours), heads(theirs))

    def tail(lines):
        plain = [ANSI.sub("", l) for l in lines]
        return [l for l in plain if l.startswith(("Some errors have", "For more information", "error: could not compile"))]

    tally.same(case, fmt, "notes and could not compile", tail(ours), tail(theirs))


Case = Literal[
    "two_codes", "one_code_twice", "tab_and_wide_chars", "lib_module", "calls", "moved", "fine",
    "dyn_associated_type_outlives", "nested_projection_outlives", "promoted_enum_array",
    "unpromoted_enum_array", "static_supertrait_object",
]


@dataclass(frozen=True)
class Input:
    cases: tuple[Case, ...] = ()
    keep: bool = False
    blaze_bin: str = ""


def main():
    global BLAZE
    match sys.argv[1:]:
        case [text]:
            options = parse(Input, text, "check_json.py")
        case _:
            fail("check_json.py", "usage: check_json.py JSON (see the top of the file)")
    BLAZE = options.blaze_bin or BLAZE
    cases = {name: CASES[name] for name in (options.cases or CASES)}
    # Under /tmp on macOS: the checkers' sockets are in BLAZE_HOME below, and under $TMPDIR
    # (/var/folders/...) their paths pass the 104 bytes macOS has for one.
    work = tempfile.mkdtemp(prefix="check-json-", dir="/tmp" if sys.platform == "darwin" else None)
    home = os.path.join(work, "home")
    tally = Tally()
    try:
        for name, files in cases.items():
            at = make(work, name, files)
            for fmt in FORMATS:
                theirs, their_err, their_status = run(at, [], fmt, home)
                ours, our_err, our_status = run(at, ["blaze"], fmt, home)
                tally.same(name, fmt, "status", our_status, their_status)
                if our_status != their_status and our_err:
                    print(f"{name} {fmt}: blaze exited {our_status}: {our_err[-1]}", flush=True)
                tally.same(name, fmt, "checked without rustc", [l for l in our_err if "Checking probe" in l], [])
                if fmt.startswith("json"):
                    compare_json(tally, name, fmt, ours, theirs)
                    tally.same(name, fmt, "could not compile", [l for l in our_err if "could not compile" in l], [l for l in their_err if "could not compile" in l])
                else:
                    compare_text(tally, name, fmt, our_err, their_err)
            print(f"{name}: done", flush=True)
    finally:
        subprocess.run([BLAZE, "stop"], env=env(home), capture_output=True, check=False)
        if not options.keep:
            shutil.rmtree(work, ignore_errors=True)
        else:
            print(f"kept {work}")
    print(f"\n{tally.compared - len(tally.differs)}/{tally.compared} compared fields the same as rustc's")
    print(f"{tally.placed_otherwise} errors placed otherwise, their codes compared: " + "; ".join(f"{c}: {why}" for c, why in PLACED_OTHERWISE.items()))
    print("rustc's only: " + ", ".join(f"{n} {what}" for what, n in tally.rustc_only.items()))
    for line in tally.differs:
        print("  DIFFERS " + line)
    sys.exit(1 if tally.differs else 0)


if __name__ == "__main__":
    main()
