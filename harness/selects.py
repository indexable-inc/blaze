#!/usr/bin/env python3
"""selects.py: blaze runs no build script and no proc macro that cargo would not have.

A build script and a proc macro are programs of the checkout, and they run with the rights
of the user under cargo and under blaze alike. What blaze must not do is run one cargo
would have left alone: the script of a dev-dependency or of another member of the
workspace for a build of one program, or a proc macro only the tests use.

Every script and macro of the workspace here writes a line when it runs. The same
selection is given to cargo and to blaze, each on a copy of the workspace nothing was
built in, and every line blaze's build wrote has to be one cargo's wrote.

    python3 selects.py
"""
import os, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from storelib import scratch
from difftest import BLAZE, sysroot_rlibs

MARK = '''
fn mark(what: &str) {
    use std::io::Write;
    let to = concat!(env!("CARGO_MANIFEST_DIR"), "/../ran");
    let mut ran = std::fs::OpenOptions::new().create(true).append(true).open(to).unwrap();
    // One write of the whole line: `writeln!` writes the text and the newline apart, and
    // the scripts cargo runs at once would put theirs between.
    ran.write_all(format!("{what}\n").as_bytes()).unwrap();
}
'''
PACKAGE = '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n'

# What cargo is asked for, and the same said to blaze.
SELECTIONS = [
    ("one program", ["build", "-p", "app"], ["--package", "app"]),
    ("its tests", ["test", "--no-run", "-p", "app"], ["--package", "app", "--unit-test"]),
    ("the workspace", ["build", "--workspace"], ["--workspace"]),
]


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    open(path, "w").write(text)


def scripted(ws, name, body):
    """A library with a build script that says it ran."""
    write(f"{ws}/{name}/Cargo.toml", PACKAGE % name)
    write(f"{ws}/{name}/src/lib.rs", body)
    write(f"{ws}/{name}/build.rs", 'fn main() {\n    mark("build script of %s");\n}\n%s' % (name, MARK))


def workspace(ws):
    # The marks are written beside the packages, which a sandbox refuses (it lets a build script
    # write only its OUT_DIR, a proc macro nothing): the workspace runs them outside.
    write(f"{ws}/Cargo.toml", '[workspace]\nmembers = ["app", "dep", "devdep", "devmacro", "other"]\nresolver = "2"\n\n'
          '[workspace.metadata.blaze.sandbox]\nunsandboxed-build-scripts = ["dep", "devdep", "other"]\n'
          'unsandboxed-proc-macros = ["devmacro"]\n')
    write(f"{ws}/app/Cargo.toml", PACKAGE % "app" + '\n[dependencies]\ndep = { path = "../dep" }\n\n'
          '[dev-dependencies]\ndevdep = { path = "../devdep" }\ndevmacro = { path = "../devmacro" }\n')
    write(f"{ws}/app/src/main.rs",
          'fn main() {\n    println!("{}", dep::two());\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n'
          '    fn three() {\n        assert_eq!(devmacro::three!(), devdep::three());\n    }\n}\n')
    scripted(ws, "dep", "pub fn two() -> u32 {\n    2\n}\n")
    scripted(ws, "devdep", "pub fn three() -> u32 {\n    3\n}\n")
    scripted(ws, "other", "pub fn four() -> u32 {\n    4\n}\n")
    write(f"{ws}/devmacro/Cargo.toml", PACKAGE % "devmacro" + "\n[lib]\nproc-macro = true\n")
    write(f"{ws}/devmacro/src/lib.rs",
          '#[proc_macro]\npub fn three(_: proc_macro::TokenStream) -> proc_macro::TokenStream {\n'
          '    mark("proc macro of devmacro");\n    "3".parse().unwrap()\n}\n' + MARK)


def ran(ws):
    path = os.path.join(ws, "ran")
    return set(open(path).read().splitlines()) if os.path.exists(path) else set()


def fresh(work, name):
    ws = os.path.join(work, name)
    shutil.rmtree(ws, ignore_errors=True)
    workspace(ws)
    return ws


def by_cargo(work, args):
    ws = fresh(work, "cargo")
    done = subprocess.run(["cargo", *args], cwd=ws, capture_output=True, text=True)
    if done.returncode != 0:
        raise SystemExit(f"cargo {' '.join(args)} failed:\n{done.stderr[-2000:]}")
    return ran(ws)


def by_blaze(work, args):
    ws = fresh(work, "blaze")
    symbols = [x for rlib in sysroot_rlibs() for x in ("--symbols", rlib)]
    done = subprocess.run([BLAZE, ws, "--crate", "dep", "--crate", "app", "--out", os.path.join(work, "app.o"),
                           *args, *symbols], capture_output=True, text=True)
    if done.returncode != 0:
        raise SystemExit(f"blaze {' '.join(args)} failed:\n{done.stderr[-2000:]}")
    return ran(ws)


def main():
    work = scratch("blaze-selects-")
    wrong = 0
    for what, cargo_args, blaze_args in SELECTIONS:
        cargos, blazes = by_cargo(work, cargo_args), by_blaze(work, blaze_args)
        more = sorted(blazes - cargos)
        wrong += bool(more)
        print(f"  {what}: cargo ran {len(cargos)} ({', '.join(sorted(cargos))}); blaze ran {len(blazes)}"
              + (f", WRONG: cargo did not run {', '.join(more)}" if more else ", none that cargo did not"))
    shutil.rmtree(work, ignore_errors=True)
    if wrong:
        raise SystemExit(f"selects: {wrong} of {len(SELECTIONS)} selections WRONG")
    print(f"selects: {len(SELECTIONS)} of {len(SELECTIONS)} right")


if __name__ == "__main__":
    main()
