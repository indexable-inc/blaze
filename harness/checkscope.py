#!/usr/bin/env python3
"""checkscope.py: the daemon's check of an edit for errors looks at what the edit changed, and
still finds every error cargo does.

usage: checkscope.py    (RA_JIT_BIN names the ra-jit built; cargo-ra-jit is found next to it)

A toy workspace is built and loaded, and then edited step by step. After each step
`cargo ra-jit build` has to fail when cargo's build of the same source fails, and succeed, as
the daemon's build, when cargo's does. The steps:

  body error     a type error in a function's body
  still broken   another file's body edited, the error left: the check of the edit looks at the
                 function that changed since the check last found no error, not since the edit
  mended         the error taken out
  caller         a method gains a parameter and its caller in the same file is left as it was
  callers        the caller written for it
  test error     an error in `#[cfg(test)]` code, which the build does not compile
  test mended
  type           a struct's field changes type, and a body that is not edited no longer
                 compiles: anything but functions changed, and the file is checked whole
  type undone

Each worker's log says how much of each file the check looked at (`check-scope:`), and the
steps that change functions alone have to be checked by function.

Python 3, stdlib only.
"""
import glob, os, re, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import cargo_ra_jit
from cargo_ra_jit import Side, wait_warm

LIB = """pub struct V(pub u8);

impl V {
    pub fn get(&self) -> u8 {
        self.0 + 1
    }
}

pub fn user(v: &V) -> u8 {
    v.get()
}

pub fn other() -> u8 {
    2
}

#[cfg(test)]
mod tests {
    #[test]
    fn t() {
        assert_eq!(super::V(1).get(), 2);
    }
}
"""
FILES = {
    "Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n',
    "src/lib.rs": LIB,
    "src/main.rs": 'fn main() {\n    let v = app::V(3);\n    println!("{} {}", app::user(&v), app::other());\n}\n',
}
LIB_RS, MAIN_RS = "src/lib.rs", "src/main.rs"
# (step, edits, whether the step changes functions alone); an edit is (file, old, new).
STEPS = [
    ("body error", [(LIB_RS, "        self.0 + 1\n", '        let _wrong: u8 = "no";\n        self.0 + 1\n')], True),
    ("still broken", [(MAIN_RS, "app::V(3)", "app::V(4)")], True),
    ("mended", [(LIB_RS, '        let _wrong: u8 = "no";\n', "")], True),
    ("caller", [(LIB_RS, "pub fn get(&self)", "pub fn get(&self, more: u8)"), (LIB_RS, "self.0 + 1", "self.0 + more")], True),
    ("callers", [(LIB_RS, "    v.get()\n", "    v.get(1)\n")], True),
    ("test error", [(LIB_RS, "super::V(1).get()", 'super::V(1).get("no")')], True),
    ("test mended", [(LIB_RS, 'super::V(1).get("no")', "super::V(1).get(1)")], True),
    ("type", [(LIB_RS, "pub struct V(pub u8);", "pub struct V(pub String);")], False),
    ("type undone", [(LIB_RS, "pub struct V(pub String);", "pub struct V(pub u8);")], False),
]


def scopes(home):
    """Each `check-scope:` line the workers wrote, in order."""
    found = []
    for log in sorted(glob.glob(os.path.join(home, "*", "*", "log"))):
        found += re.findall(r"^check-scope: (\d+) files whole, (\d+) functions of (\d+) others$", open(log, errors="replace").read(), re.M)
    return [tuple(map(int, it)) for it in found]


def main():
    bin_dir = os.path.dirname(os.path.abspath(os.environ["RA_JIT_BIN"]))
    root = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-checkscope-", dir="/tmp"))
    ws, home = os.path.join(root, "ws"), os.path.join(root, "home")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = cargo_ra_jit.base_env(home)
    env["PATH"] = bin_dir + os.pathsep + env["PATH"]
    for name in ("RUSTC_WRAPPER", "CARGO_INCREMENTAL", "RA_JIT_SPARE"):
        env.pop(name, None)
    env["RA_JIT_STATS"] = "1"
    side, reference = Side(ws, env, True), Side(ws, env, False)
    wrong = []
    try:
        _, built = side.cargo("build")
        if built.returncode != 0:
            raise SystemExit(f"the cold build failed:\n{(built.stdout + built.stderr)[-3000:]}")
        took, states = wait_warm(side, ws)
        if states != ["ready"]:
            raise SystemExit(f"the daemon did not get ready: {states}")
        for step, edits, by_function in STEPS:
            for path, old, new in edits:
                path = os.path.join(ws, path)
                text = open(path).read()
                if old not in text:
                    raise SystemExit(f"{step}: {old!r} is not in {path}")
                open(path, "w").write(text.replace(old, new, 1))
            seen = len(scopes(home))
            _, ours = side.cargo("build")
            _, theirs = reference.cargo("build")
            said = ours.stdout + ours.stderr
            right = (ours.returncode == 0) == (theirs.returncode == 0)
            if ours.returncode == 0:
                right &= "[ra-jit]" in said
            else:
                right &= "error" in said
            scope = scopes(home)[seen:]
            if by_function:
                right &= bool(scope) and all(whole == 0 for whole, _, _ in scope)
            detail = f"cargo exit {theirs.returncode}, ours {ours.returncode}; checked {scope}"
            print(f"{step:14} {'ok' if right else 'WRONG':6} {detail}")
            if not right:
                wrong.append(f"{step}: {detail}\n{said[-1500:]}")
    finally:
        side.cargo("stop")
    if wrong:
        raise SystemExit("\n".join(wrong))


if __name__ == "__main__":
    main()
