#!/usr/bin/env python3
# pyright: basic
"""depkeys.py: whether the store hands a build code of a dependency built with something else.

A crate of a registry is the same source wherever it is built, but not the same code: its
features, the cfgs and variables its build script sets and the files the script writes differ
between checkouts that share one store (the default) and between runs of the script, as a
change to the sandbox reruns it. A program on such a crate is built once into a store; each case
changes one of those and builds again, with a daemon of its own on a copy of that store. The
program has to print what the case says it prints, the same as a build of it in an empty home
(rebuild.py's `Project.check`), and the second build of `nothing` has to compile nothing (every
artifact `fresh`).

usage: depkeys.py '{}'
       depkeys.py '{"cases": ["a feature"]}'

The input is one JSON object (inline, the path of a file, or `-` for standard input), an Input
below; a field it does not have is refused. `cases` picks the cases whose names contain any of
its strings (all of them when there are none). BLAZE_BIN names the blaze under test
(blaze_cli.BLAZE).
"""

import hashlib
import json
import os
import shutil
import subprocess
import sys
from dataclasses import dataclass

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from rebuild import Project
from storelib import scratch
from typed_input import fail, parse

SCRIPT = "depkeys.py"

DEP_MANIFEST = """[package]
name = "dep"
version = "0.1.0"
edition = "2021"

[features]
extra = []
"""
# Each input reaches the code through the build script alone, as what a sandbox gives a script
# would.
DEP_BUILD = """fn main() {
    for name in ["DEPKEYS_CFG", "DEPKEYS_ENV", "DEPKEYS_GEN"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    println!("cargo::rustc-check-cfg=cfg(depkeys_set)");
    if std::env::var_os("DEPKEYS_CFG").is_some() {
        println!("cargo:rustc-cfg=depkeys_set");
    }
    let set = std::env::var("DEPKEYS_ENV").unwrap_or_else(|_| "unset".to_owned());
    println!("cargo:rustc-env=DEPKEYS_SET={set}");
    let generated = std::env::var("DEPKEYS_GEN").unwrap_or_else(|_| "11".to_owned());
    let out = std::env::var("OUT_DIR").unwrap();
    let code = format!("#[inline(never)]\\npub fn generated() -> u32 {{ {generated} }}\\n");
    std::fs::write(format!("{out}/gen.rs"), code).unwrap();
}
"""
DEP_LIB = """#[inline(never)]
pub fn feature() -> &'static str {
    if cfg!(feature = "extra") { "extra" } else { "plain" }
}

#[inline(never)]
pub fn cfg() -> &'static str {
    if cfg!(depkeys_set) { "set" } else { "unset" }
}

#[inline(never)]
pub fn variable() -> &'static str {
    env!("DEPKEYS_SET")
}

include!(concat!(env!("OUT_DIR"), "/gen.rs"));
"""
APP_MANIFEST = """[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
dep = {{ version = "0.1", registry = "local"{features} }}

[workspace]
"""
APP_MAIN = """fn main() {
    println!("{} {} {} {}", dep::feature(), dep::cfg(), dep::variable(), dep::generated());
}
"""


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)


def features(ws, env):
    write(os.path.join(ws, "Cargo.toml"), APP_MANIFEST.format(features=', features = ["extra"]'))


def setenv(name, value):
    return lambda ws, env: env.__setitem__(name, value)


# What the program prints before any change.
BEFORE = "plain unset unset 11"

# (name, the change, what the program prints after it)
CASES = [
    ("nothing", lambda ws, env: None, BEFORE),
    ("a feature of a dependency", features, "extra unset unset 11"),
    ("a cfg a dependency's build script sets", setenv("DEPKEYS_CFG", "1"), "plain set unset 11"),
    ("a variable a dependency's build script sets", setenv("DEPKEYS_ENV", "other"), "plain unset other 11"),
    ("a file a dependency's build script writes", setenv("DEPKEYS_GEN", "12"), "plain unset unset 12"),
]


@dataclass(frozen=True)
class Input:
    cases: tuple[str, ...] = ()


def registry(top):
    """A registry of this machine's own with `dep` in it: a local-registry the `local`
    registry's source is replaced with, as blaze reads no git index. Returns its config."""
    stage, files = os.path.join(top, "stage"), os.path.join(top, "registry")
    write(os.path.join(stage, "dep-0.1.0/Cargo.toml"), DEP_MANIFEST)
    write(os.path.join(stage, "dep-0.1.0/build.rs"), DEP_BUILD)
    write(os.path.join(stage, "dep-0.1.0/src/lib.rs"), DEP_LIB)
    os.makedirs(files)
    crate = os.path.join(files, "dep-0.1.0.crate")
    subprocess.run(["tar", "-czf", crate, "-C", stage, "dep-0.1.0"], check=True)
    with open(crate, "rb") as f:
        cksum = hashlib.sha256(f.read()).hexdigest()
    entry = {"name": "dep", "vers": "0.1.0", "deps": [], "cksum": cksum, "features": {"extra": []}, "yanked": False}
    write(os.path.join(files, "index/3/d/dep"), json.dumps(entry) + "\n")
    index = f"file://{files}/index"
    return (
        f'[registries.local]\nindex = "{index}"\n\n[source.local-index]\nregistry = "{index}"\n'
        f'replace-with = "local-files"\n\n[source.local-files]\nlocal-registry = "{files}"\n'
    )


def checkout(at, config):
    write(os.path.join(at, ".cargo/config.toml"), config)
    write(os.path.join(at, "Cargo.toml"), APP_MANIFEST.format(features=""))
    write(os.path.join(at, "src/main.rs"), APP_MAIN)


def compiled(said):
    """The targets a `blaze build --message-format=json` compiled rather than took as built."""
    names = []
    for line in said.splitlines():
        if not line.startswith("{"):
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") == "compiler-artifact" and message.get("fresh") is False:
            names.append(message.get("target", {}).get("name", "?"))
    return names


def build(ws, work, home, env, name):
    """The verdict of a build of `app` by a daemon of `home` held to a fresh one, what the program
    printed, and the targets it compiled. The daemon is stopped after, so its store is whole."""
    project = Project(ws, work, env=env, home=home, name=name)
    try:
        built = project.build("app")
        verdict, said = project.held(built, "app")
        printed = project.ran(built)[1].strip() if built.status == 0 and built.executable else None
        return verdict, said, printed, compiled(built.said)
    finally:
        project.stop()


def main():
    match sys.argv[1:]:
        case [text]:
            it = parse(Input, text, SCRIPT)
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} JSON (see the top of the file)")
    if unknown := [case for case in it.cases if not any(case in name for name, _, _ in CASES)]:
        fail(SCRIPT, f"cases: {unknown} is in the name of no case")
    work = scratch("blaze-depkeys-")
    config = registry(work)
    ws, cargo_home, filled = (os.path.join(work, name) for name in ("app", "cargo-home", "filled"))
    # The first build once: each case starts from a copy of the store it filled.
    checkout(ws, config)
    verdict, said, printed, _ = build(ws, os.path.join(work, "first"), filled, {"CARGO_HOME": cargo_home}, "first")
    if verdict != "same" or printed != BEFORE:
        print(f"WRONG the first build: {verdict}, printed {printed!r}, not {BEFORE!r}\n{said}")
        sys.exit(1)
    wrong = 0
    chosen = [case for case in CASES if not it.cases or any(name in case[0] for name in it.cases)]
    for number, (name, change, want) in enumerate(chosen, 1):
        home = os.path.join(work, f"home-{number}")
        shutil.copytree(filled, home)
        # What the case's build reuses comes from the store alone.
        shutil.rmtree(os.path.join(ws, "target"), ignore_errors=True)
        checkout(ws, config)
        env = {"CARGO_HOME": cargo_home}
        change(ws, env)
        verdict, said, printed, rebuilt = build(ws, os.path.join(work, f"case-{number}"), home, env, f"case-{number}")
        problems = []
        if verdict != "same":
            problems.append(f"{verdict}: {said}")
        if printed != want:
            problems.append(f"the program printed {printed!r}, not {want!r}")
        if name == "nothing" and rebuilt:
            problems.append(f"the second build compiled {', '.join(rebuilt)}, which the first kept")
        wrong += bool(problems)
        print(f"{'same     ' if not problems else 'DIFFERENT'} {name}: compiled {len(rebuilt)} targets", flush=True)
        for problem in problems:
            print(f"    {problem}")
    print(f"{len(chosen) - wrong} of {len(chosen)} right; {work}")
    if not wrong:
        shutil.rmtree(work, ignore_errors=True)
    sys.exit(1 if wrong else 0)


if __name__ == "__main__":
    main()
