#!/usr/bin/env python3
"""artifactedits.py: an edit of a dependency reaches the file `cargo blaze build` says it built,
for each kind of artifact a package can ask for: a program (`bin`), a `cdylib`, a `dylib` and a
`staticlib` (#730). A build that cannot write the artifact fails saying why; it never reports
success over the bytes of before.

usage: artifactedits.py [case ...] [--release] [--keep]
       (RA_JIT_BIN names the ra-jit built; cargo-blaze is found next to it)

Each case is a workspace of two packages: `dep`, a library, and `top`, of the case's kind,
which calls it. It is built cold, again with nothing changed, after an edit of a body of `dep`
that changes no behaviour (`std::hint::black_box`, as in #730), after one that changes what it
returns, and after the edit is taken back. After each build:

  - a build that exits 0 has written the artifact, and what it computes is what the source
    says (the program is run, a `cdylib` or `dylib` loaded with ctypes, a `staticlib` linked
    into a C program);
  - after each edit, the artifact's sha256 differs from the build before: an artifact left as
    it was is STALE, whatever the build said;
  - a build that does not exit 0 names the kind of artifact it does not write (a refusal), and
    is only right where the case expects one.

`flags` is a program built with the rustflags of #731: `-Z threads=8` (RUSTC_BOOTSTRAP=1),
`--cfg tokio_unstable`, `-C target-cpu` of the host's family, `-C force-frame-pointers=yes` and,
on AArch64 Linux, lld. Each has to be built and its edits spliced by blaze, or the build refused
naming the flag; never handed to cargo.

Python 3, stdlib only. Linux and macOS.
"""
import argparse
import hashlib
import os
import platform
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
TIMEOUT = 900
AARCH64 = platform.machine().lower() in ("arm64", "aarch64")
DLL = ".dylib" if sys.platform == "darwin" else ".so"

DEP = """pub fn value() -> u32 {
    41
}
"""
# (what the step is, (old, new) in dep/src/lib.rs or None, what `top` computes after it)
STEPS = [
    ("cold", None, 41),
    ("nothing edited", None, 41),
    ("a body of dep, no behaviour changed", ("    41\n", "    std::hint::black_box(41)\n"), 41),
    ("a body of dep, what it returns", ("black_box(41)", "black_box(42)"), 42),
    ("the edits taken back", ("    std::hint::black_box(42)\n", "    41\n"), 41),
]
EXPORT = """#[no_mangle]
pub extern "C" fn top_value() -> u32 {
    dep::value()
}
"""
C_MAIN = """#include <stdio.h>
unsigned top_value(void);
int main(void) { printf("%u\\n", top_value()); return 0; }
"""


def target_cpu():
    if AARCH64:
        return "apple-m1" if sys.platform == "darwin" else "neoverse-n1"
    return "x86-64-v3"


def flags_rustflags():
    flags = ["-Z", "threads=8", "--cfg", "tokio_unstable", "-C", f"target-cpu={target_cpu()}",
             "-C", "force-frame-pointers=yes"]
    if AARCH64 and sys.platform == "linux":
        flags += ["-C", "link-arg=-fuse-ld=lld"]
    return flags


# name: (crate types of top, where the artifact is under the profile's directory, what blaze
# does: "built" or "refused", the environment of the build)
CASES = {
    "bin": (None, "top", "built", {}),
    "cdylib": (["cdylib"], f"{'lib' if sys.platform != 'win32' else ''}top{DLL}", "built", {}),
    "dylib": (["dylib"], f"libtop{DLL}", "refused", {}),
    "staticlib": (["staticlib"], "libtop.a", "refused", {}),
    "flags": (None, "top", "built", {"RUSTC_BOOTSTRAP": "1"}),
}


def write_case(ws, name):
    crate_types = CASES[name][0]
    files = {
        "Cargo.toml": '[workspace]\nmembers = ["dep", "top"]\nresolver = "2"\n',
        "dep/Cargo.toml": '[package]\nname = "dep"\nversion = "0.1.0"\nedition = "2021"\n',
        "dep/src/lib.rs": DEP,
    }
    top = '[package]\nname = "top"\nversion = "0.1.0"\nedition = "2021"\n\n'
    if crate_types:
        top += f"[lib]\ncrate-type = {crate_types!r}\n\n".replace("'", '"')
        files["top/src/lib.rs"] = EXPORT
    elif name == "flags":
        files["top/src/main.rs"] = (
            'fn main() {\n    println!("{} {}", dep::value(), cfg!(tokio_unstable));\n}\n'
        )
    else:
        files["top/src/main.rs"] = 'fn main() {\n    println!("{}", dep::value());\n}\n'
    files["top/Cargo.toml"] = top + '[dependencies]\ndep = { path = "../dep" }\n'
    if name == "flags":
        rustflags = ", ".join(f'"{it}"' for it in flags_rustflags())
        files[".cargo/config.toml"] = f"[build]\nrustflags = [{rustflags}]\n"
    for path, text in files.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)) or ws, exist_ok=True)
        with open(os.path.join(ws, path), "w") as out:
            out.write(text)


def computed(name, artifact, scratch):
    """What the artifact computes: the program's output, or `top_value()` of the library."""
    if CASES[name][0] is None:
        ran = subprocess.run([artifact], capture_output=True, text=True, timeout=60)
        return ran.stdout.strip() if ran.returncode == 0 else f"exit {ran.returncode}: {ran.stderr[-300:]}"
    if name == "staticlib":
        exe = os.path.join(scratch, "c-main")
        main = os.path.join(scratch, "main.c")
        with open(main, "w") as out:
            out.write(C_MAIN)
        libs = ["-lpthread", "-ldl", "-lm"] if sys.platform == "linux" else []
        subprocess.run(["cc", main, artifact, "-o", exe, *libs], check=True, timeout=120)
        return subprocess.run([exe], capture_output=True, text=True, timeout=60).stdout.strip()
    # In a process of its own, which a library that crashes does not take down.
    load = ("import ctypes, sys; lib = ctypes.CDLL(sys.argv[1]); "
            "lib.top_value.restype = ctypes.c_uint32; print(lib.top_value())")
    ran = subprocess.run([sys.executable, "-c", load, artifact], capture_output=True, text=True, timeout=60)
    return ran.stdout.strip() if ran.returncode == 0 else f"exit {ran.returncode}: {ran.stderr[-300:]}"


def sha256(path):
    if not os.path.exists(path):
        return None
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def run_case(name, root, env, release):
    _, file, expect, extra = CASES[name]
    ws = os.path.join(root, name)
    scratch = os.path.join(root, f"{name}-scratch")
    os.makedirs(scratch, exist_ok=True)
    write_case(ws, name)
    env = dict(env, **extra, CARGO_TARGET_DIR=os.path.join(root, f"{name}-target"))
    profile = "release" if release else "debug"
    artifact = os.path.join(env["CARGO_TARGET_DIR"], profile, file)
    args = ["cargo", "blaze", "build", *(["--release"] if release else [])]
    wrong, before = [], None
    for what, edit, want in STEPS:
        if edit:
            path = os.path.join(ws, "dep/src/lib.rs")
            text = open(path).read()
            assert edit[0] in text, (what, edit[0])
            with open(path, "w") as out:
                out.write(text.replace(edit[0], edit[1], 1))
        ran = subprocess.run(args, cwd=ws, env=env, capture_output=True, text=True, timeout=TIMEOUT)
        said = (ran.stdout + ran.stderr).strip()
        now = sha256(artifact)
        if ran.returncode != 0:
            kind = "dylib" if name == "dylib" else name
            names_it = f"`{kind}`" in said or (name == "flags" and "flag" in said)
            if expect == "refused" and names_it and "building with cargo" not in said:
                verdict = "refused, saying why"
            elif names_it:
                verdict = f"REFUSED: {said.splitlines()[-1][:200]}"
            else:
                verdict = f"FAILED WITHOUT NAMING IT: {said[-400:]}"
        elif now is None:
            verdict = f"SUCCESS WITH NO ARTIFACT: {file} is not there"
        elif edit and now == before:
            verdict = f"STALE: {file} is the bytes of the build before ({now[:12]})"
        elif "[blaze]" not in said:
            verdict = f"NOT BLAZE'S BUILD: {said[-300:]}"
        else:
            got = computed(name, artifact, scratch)
            want = f"{want} true" if name == "flags" else str(want)
            verdict = "ok" if got == want else f"COMPUTES {got!r}, NOT {want!r}"
        right = verdict == "ok" if expect == "built" else verdict == "refused, saying why"
        if not right:
            wrong.append(what)
        sha = now[:12] if now else "-"
        print(f"  {name:10} {what:38} rc={ran.returncode:<3} {sha:12} {verdict}", flush=True)
        before = now
        if expect == "refused" and ran.returncode != 0:
            break
    return wrong


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("cases", nargs="*", default=list(CASES))
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--keep", action="store_true")
    opts = parser.parse_args()
    bin_dir = os.path.dirname(os.path.abspath(os.environ.get("RA_JIT_BIN", os.path.join(HERE, "../target/release/ra-jit"))))
    if not os.path.exists(os.path.join(bin_dir, "cargo-blaze")):
        raise SystemExit(f"no cargo-blaze in {bin_dir}")
    root = tempfile.mkdtemp(prefix="rj-artifactedits-")
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
               RA_JIT_HOME=os.environ.get("RA_JIT_HOME") or os.path.join(root, "home"))
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        env.pop(name, None)
    wrong = {}
    try:
        for name in opts.cases:
            wrong[name] = run_case(name, root, env, opts.release)
    finally:
        subprocess.run(["cargo", "blaze", "stop"], cwd=root, env=env, capture_output=True, timeout=120)
    bad = {name: steps for name, steps in wrong.items() if steps}
    print(f"{len(wrong) - len(bad)} of {len(wrong)} cases right ({'release' if opts.release else 'dev'}); under {root}")
    for name, steps in bad.items():
        print(f"  {name}: {', '.join(steps)}")
    if bad:
        sys.exit(1)
    if not opts.keep:
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
