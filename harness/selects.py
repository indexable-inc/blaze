#!/usr/bin/env python3
"""selects.py: blaze runs no build script and no proc macro the selection does not need.

A build script and a proc macro are programs of the checkout. What blaze must not do is run
one the build asked for does not need: the script of a dev-dependency or of another member
of the workspace for a build of one program, or a proc macro only the tests use.

Every script and macro of the workspace here writes a line when it runs. Each selection is
given to blaze (`blaze build` or `blaze test --no-run`) on a copy of the workspace nothing was
built in, in an empty home, and every line blaze's build wrote has to be one of the selection's
(`SELECTIONS`).

    python3 selects.py
"""
import os, shutil, sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from rebuild import Project, scratch

MARK = '''
fn mark(what: &str) {
    use std::io::Write;
    let to = concat!(env!("CARGO_MANIFEST_DIR"), "/../ran");
    let mut ran = std::fs::OpenOptions::new().create(true).append(true).open(to).unwrap();
    // One write of the whole line: `writeln!` writes the text and the newline apart, and
    // the scripts run at once would put theirs between.
    ran.write_all(format!("{what}\n").as_bytes()).unwrap();
}
'''
PACKAGE = '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n'

SCRIPT, MACRO = "build script of %s", "proc macro of devmacro"
# What blaze is asked for, and every script and macro the selection needs, which are all it
# may run: a program needs the scripts of what it depends on, its tests those of its
# dev-dependencies and the macro they call too, the workspace every member's script and no
# macro, as nothing it builds calls one.
SELECTIONS = [
    ("one program", "build", ["--package", "app"], {SCRIPT % "dep"}),
    ("its tests", "test", ["--package", "app", "--no-run"], {SCRIPT % "dep", SCRIPT % "devdep", MACRO}),
    ("the workspace", "build", ["--workspace"], {SCRIPT % "dep", SCRIPT % "devdep", SCRIPT % "other"}),
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


def by_blaze(work, command, args):
    ws = fresh(work, "blaze")
    home = os.path.join(work, "home")
    shutil.rmtree(home, ignore_errors=True)
    project = Project(ws, work, home=home)
    try:
        done = project.build("app", *args, command=command)
    finally:
        project.stop()
    if done.status != 0:
        raise SystemExit(f"blaze {command} {' '.join(args)} failed:\n{done.said[-2000:]}")
    return ran(ws)


def main():
    work = scratch("blaze-selects-")
    wrong = 0
    for what, command, blaze_args, needed in SELECTIONS:
        blazes = by_blaze(work, command, blaze_args)
        more = sorted(blazes - needed)
        wrong += bool(more)
        print(f"  {what}: {len(needed)} needed ({', '.join(sorted(needed))}); blaze ran {len(blazes)}"
              + (f", WRONG: not needed {', '.join(more)}" if more else ", none not needed"))
    shutil.rmtree(work, ignore_errors=True)
    if wrong:
        raise SystemExit(f"selects: {wrong} of {len(SELECTIONS)} selections WRONG")
    print(f"selects: {len(SELECTIONS)} of {len(SELECTIONS)} right")


if __name__ == "__main__":
    main()
