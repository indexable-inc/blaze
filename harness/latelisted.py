#!/usr/bin/env python3
"""latelisted.py: what a file an `include!` names declares is listed in its crate, though the
file is loaded only once the names are resolved.

usage: latelisted.py    (BLAZE_BIN names the blaze built; blaze is found next to it)

bun's crates `include!` the code its generators write, from a directory a build script names in
a variable (`BUN_CODEGEN_DIR`). The VFS has none of those files when the daemon first lists a
crate: the files are read once the names are resolved. A listing kept from before was missing
the functions they declare from the program's entries until an edit had the crate listed again,
and that edit's patch then compiled every one of them as a function added. Here a library
includes `exports.in` (not a `.rs` file, so nothing loads it but the macro) with EXPORTS
functions of the C ABI. Once the daemon has built the program, a function is added to the
library: its patch has to be of a few functions, not of the EXPORTS, and the program has to
print what its source says (`PRINTS`).

Python 3, stdlib only.
"""
import glob, os, re, shutil, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import blaze_cli
from blaze_cli import Side

EXPORTS = 64
FILES = {
    "Cargo.toml": '[workspace]\nmembers = ["app", "gen"]\nresolver = "2"\n',
    "gen/Cargo.toml": '[package]\nname = "gen"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n',
    "gen/build.rs": 'fn main() {\n'
                    '    let dir = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("generated");\n'
                    '    println!("cargo:rerun-if-changed=generated/exports.in");\n'
                    '    println!("cargo:rustc-env=GEN_DIR={}", dir.display());\n'
                    '}\n',
    "gen/generated/exports.in": "".join(f'#[no_mangle]\npub extern "C" fn late_export_{i}() -> u32 {{\n    {i}\n}}\n\n'
                                        for i in range(EXPORTS)),
    "gen/src/lib.rs": 'include!(concat!(env!("GEN_DIR"), "/exports.in"));\n\n'
                      'pub fn value() -> u32 {\n    1\n}\n',
    "app/Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n'
                      '[dependencies]\ngen = { path = "../gen" }\n',
    "app/src/main.rs": 'fn main() {\n    println!("{} {}", gen::value(), gen::late_export_3());\n}\n',
}
# What the program prints once `value` adds what `added` gives.
PRINTS = "41 3\n"


def patches(home):
    """The `patch: N functions` lines of the workers' logs, in order."""
    found = []
    for log in sorted(glob.glob(os.path.join(home, "*", "*", "log"))):
        found += [int(n) for n in re.findall(r"^patch: (\d+) functions$", open(log, errors="replace").read(), re.M)]
    return found


def tails(home, lines=40):
    """The last lines of each of the daemon's logs, for a failure on a machine whose files are
    gone once it ends."""
    logs = sorted(glob.glob(os.path.join(home, "*.log")) + glob.glob(os.path.join(home, "*", "*", "log")))
    return "".join(f"\n--- {log}\n" + "".join(open(log, errors="replace").readlines()[-lines:]) for log in logs)


def build(side, step, home):
    _, ran = side.cargo("build")
    if ran.returncode != 0:
        raise SystemExit(f"{step}: the build failed:\n{(ran.stdout + ran.stderr)[-3000:]}{tails(home)}")
    return ran.stdout + ran.stderr


def main():
    bin_dir = os.path.dirname(os.path.abspath(os.environ["BLAZE_BIN"]))
    if not os.path.exists(os.path.join(bin_dir, "blaze")):
        raise SystemExit(f"no blaze beside {os.environ['BLAZE_BIN']}")
    # Short, for the daemon's sockets under its home.
    root = os.path.realpath(tempfile.mkdtemp(prefix="blaze-ll-", dir="/tmp"))
    ws, home = (os.path.join(root, n) for n in ("ws", "home"))
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = blaze_cli.base_env(home)
    env["PATH"] = bin_dir + os.pathsep + env["PATH"]
    for name in ("RUSTC_WRAPPER", "CARGO_INCREMENTAL"):
        env.pop(name, None)
    side = Side(ws, env, True)
    program = os.path.join(ws, "target", "debug", "app")
    wrong = []
    try:
        build(side, "cold", home)
        seen = len(patches(home))
        lib = os.path.join(ws, "gen/src/lib.rs")
        text = open(lib).read()
        open(lib, "w").write(text.replace("pub fn value() -> u32 {\n    1\n}",
                                          "pub fn value() -> u32 {\n    1 + added()\n}\n\nfn added() -> u32 {\n    40\n}"))
        said = build(side, "a function added", home)
        made = patches(home)[seen:]
        ran = subprocess.run([program], capture_output=True, text=True, timeout=60)
        if "[blaze]" not in said:
            wrong.append("the edit was not built by the daemon")
        elif not made:
            wrong.append("the edit was not patched")
        elif made[-1] >= EXPORTS:
            wrong.append(f"the patch is of {made[-1]} functions: the {EXPORTS} the included file declares were not listed")
        if ran.stdout != PRINTS:
            wrong.append(f"the program printed {ran.stdout.strip()!r}, not {PRINTS.strip()!r}")
        print(f"{'a function added':30} {'ok' if not wrong else 'WRONG':8} patch of {made[-1] if made else '-'} functions")
    finally:
        side.cargo("stop")
    for why in wrong:
        print(f"  {why}")
    if wrong:
        print(f"under {root}{tails(home)}")
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
