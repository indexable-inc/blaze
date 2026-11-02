#!/usr/bin/env python3
"""growth.py: the code of one function grows in a straight line with its source.

usage: growth.py [ARMS [FIELDS]]

Builds a program whose `serialize` is the shape `#[derive(Serialize)]` gives an internally tagged
enum: a match of ARMS arms (default 160), each opening a value with a destructor through `?`, then
`?` on every field of the variant. It is built again with twice the arms, and blaze's `serialize`
in the second has to be less than GROWTH_RATIO (default 2.6) times the size of the first. A
straight line doubles; issue #5 was 4 KB per arm squared, 329 MB of code for 285 arms, out of
reach of an AArch64 branch.

Then the same of a `load` that fills a struct of FIELDS fields (default 200), each `lib.symbol(..)?`
while a value with a destructor is live, as x11-dl's `x11_link!` opens Xlib's 781 functions: every
`?` leaves through what the fields before it hold. Leaving was a drop chain per `?` over every
field before it, 3 blocks a field squared, and bevy's build with Xlib's took 364 GB. Every program
also has to print what its source says (`printed`, `fields_printed`).

Each program is built by `blaze build` in an empty home, and the size of a function is the size
of its symbol in the executable as the pinned LLVM's `llvm-nm` reads it: the size ELF gives it,
or on Mach-O, which gives none, the distance to the next symbol. A PE executable has no symbols
of its own to read, so it runs on Linux and macOS.
"""
import os, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from dwarf import last_part
from rebuild import Project, scratch

# The pinned LLVM's, on PATH in the flake's dev shell and in CI.
NM = shutil.which("llvm-nm") or "llvm-nm"
SYMBOL = re.compile(r"^([0-9a-f]+) (?:([0-9a-f]{8,}) )?([A-Za-z]) (.*)$")
# The hash a legacy-mangled symbol keeps after demangling.
HASH = re.compile(r"::h[0-9a-f]{16}$")

CARGO_TOML = """[package]
name = "growth"
version = "0.1.0"
edition = "2021"

[workspace]
"""

HEAD = """use std::fmt::Write;

pub struct Error(pub String);

pub struct Out<'a>(pub &'a mut String);

pub struct State<'a> {
    out: &'a mut String,
    open: bool,
}

impl Drop for State<'_> {
    fn drop(&mut self) {
        if self.open {
            self.out.push('!');
        }
    }
}

pub trait Ser: Sized {
    type St: St;
    fn begin(self, name: &'static str, len: usize) -> Result<Self::St, Error>;
}

pub trait St {
    fn field<T: std::fmt::Display + ?Sized>(&mut self, key: &'static str, value: &T) -> Result<(), Error>;
    fn end(self) -> Result<(), Error>;
}

impl<'a> Ser for Out<'a> {
    type St = State<'a>;
    fn begin(self, name: &'static str, len: usize) -> Result<State<'a>, Error> {
        if len > 1000 {
            return Err(Error(name.to_owned()));
        }
        self.0.push('{');
        Ok(State { out: self.0, open: true })
    }
}

impl St for State<'_> {
    fn field<T: std::fmt::Display + ?Sized>(&mut self, key: &'static str, value: &T) -> Result<(), Error> {
        if key.is_empty() {
            return Err(Error(String::new()));
        }
        write!(self.out, "{key}:{value},").map_err(|_| Error(String::new()))
    }

    fn end(mut self) -> Result<(), Error> {
        self.open = false;
        self.out.push('}');
        Ok(())
    }
}
"""


def program(arms):
    variants = "".join(f"    V{i} {{ id: String, n: u32 }},\n" if i % 2 else f"    V{i},\n" for i in range(arms))
    cases = "".join(
        f'            Action::V{i} {{ ref id, ref n }} => {{\n'
        f'                let mut state = serializer.begin("Action", 3)?;\n'
        f'                state.field("type", "a/v{i}")?;\n'
        f'                state.field("id", id)?;\n'
        f'                state.field("n", n)?;\n'
        f'                state.end()\n'
        f'            }}\n' if i % 2 else
        f'            Action::V{i} => {{\n'
        f'                let mut state = serializer.begin("Action", 1)?;\n'
        f'                state.field("type", "a/v{i}")?;\n'
        f'                state.end()\n'
        f'            }}\n'
        for i in range(arms))
    return (HEAD + f"\npub enum Action {{\n{variants}}}\n\nimpl Action {{\n"
            "    pub fn serialize<S: Ser>(&self, serializer: S) -> Result<(), Error> {\n"
            f"        match *self {{\n{cases}        }}\n    }}\n}}\n\n"
            "fn main() {\n    let mut out = String::new();\n"
            f'    for action in [Action::V0, Action::V1 {{ id: "x".to_owned(), n: 3 }}, Action::V{arms - 1} {{ id: "y".to_owned(), n: 4 }}] {{\n'
            "        if action.serialize(Out(&mut out)).is_err() {\n            out.push('?');\n        }\n    }\n"
            '    println!("{out}");\n}\n')


def printed(arms):
    """What `program(arms)` prints."""
    return f"{{type:a/v0,}}{{type:a/v1,id:x,n:3,}}{{type:a/v{arms - 1},id:y,n:4,}}\n"


FIELDS_HEAD = """pub struct OpenError(pub String);

pub struct Lib(Vec<u8>);

impl Drop for Lib {
    fn drop(&mut self) {
        println!("closed {}", self.0.len());
    }
}

impl Lib {
    fn symbol(&self, name: &str) -> Result<usize, OpenError> {
        if name == "missing" { Err(OpenError(name.to_owned())) } else { Ok(name.len() + self.0.len()) }
    }
}
"""


def fields_program(fields):
    names = [f"f{i}" for i in range(fields)]
    names[-1] = "missing"
    decls = "".join(f"    pub f{i}: usize,\n" for i in range(fields))
    inits = "".join(f'        f{i}: lib.symbol("{name}")?,\n' for i, name in enumerate(names))
    return (FIELDS_HEAD + f"\npub struct Funcs {{\n{decls}}}\n\n"
            "pub fn load(missing: bool) -> Result<(Lib, Funcs), OpenError> {\n"
            "    let lib = Lib(vec![1; 3]);\n"
            "    if !missing {\n"
            "        return Err(OpenError(\"not asked\".to_owned()));\n"
            "    }\n"
            f"    let funcs = Funcs {{\n{inits}    }};\n"
            "    Ok((lib, funcs))\n}\n\n"
            "fn main() {\n"
            "    for missing in [false, true] {\n"
            "        match load(missing) {\n"
            '            Ok((_, funcs)) => println!("{}", funcs.f0),\n'
            '            Err(e) => println!("error {}", e.0),\n'
            "        }\n    }\n}\n")


def fields_printed(fields):
    """What `fields_program(fields)` prints: the library is closed as each `load` leaves."""
    return "closed 3\nerror not asked\nclosed 3\nerror missing\n"


def code_size(executable, function):
    """The bytes of the largest copy of `function` in the executable's symbols."""
    listed = subprocess.run([NM, "--demangle", "--numeric-sort", "--defined-only", "--print-size", executable],
                            capture_output=True, text=True, timeout=300)
    if listed.returncode != 0:
        raise SystemExit(f"{NM} {executable} failed:\n{listed.stderr[-2000:]}")
    symbols = [m.groups() for line in listed.stdout.splitlines() if (m := SYMBOL.match(line))]
    found = []
    for at, (address, size, kind, name) in enumerate(symbols):
        if kind not in "tT" or last_part(HASH.sub("", name)) != function:
            continue
        if size is not None:
            found.append(int(size, 16))
        elif at + 1 < len(symbols):
            found.append(int(symbols[at + 1][0], 16) - int(address, 16))
    if not found:
        raise SystemExit(f"no symbol of `{function}` in {executable}")
    return max(found)


def build(source, expected, function, what):
    work = scratch("blaze-growth-")
    ws = os.path.join(work, "ws")
    os.makedirs(os.path.join(ws, "src"))
    open(os.path.join(ws, "Cargo.toml"), "w").write(CARGO_TOML)
    open(os.path.join(ws, "src/main.rs"), "w").write(source)
    project = Project(ws, work)
    try:
        built = project.build("growth")
    finally:
        project.stop()
    if built.status != 0 or not built.executable:
        raise SystemExit(f"{what}: blaze's build failed ({built.status}):\n{built.said[-3000:]}")
    got = project.ran(built)
    if got != (0, expected, ""):
        raise SystemExit(f"{what}: blaze's program differs from what its source says:\n  source: {expected!r}\n  blaze:  {got}")
    size = code_size(built.executable, function)
    shutil.rmtree(work, ignore_errors=True)
    return size


def grows(function, unit, n, program, printed, limit):
    small = build(program(n), printed(n), function, f"{n} {unit}s")
    large = build(program(2 * n), printed(2 * n), function, f"{2 * n} {unit}s")
    ratio = large / small
    print(f"{function}: {small:,} bytes for {n} {unit}s, {large:,} for {2 * n}: x{ratio:.2f} "
          f"({small // n:,} and {large // (2 * n):,} bytes a {unit})")
    if ratio > limit:
        raise SystemExit(f"FAIL: twice the {unit}s is x{ratio:.2f} the code, more than x{limit}")


def main():
    arms = int(sys.argv[1]) if sys.argv[1:] else 160
    fields = int(sys.argv[2]) if sys.argv[2:] else 200
    limit = float(os.environ.get("GROWTH_RATIO", "2.6"))
    grows("serialize", "arm", arms, program, printed, limit)
    grows("load", "field", fields, fields_program, fields_printed, limit)
    print("ok")


if __name__ == "__main__":
    main()
