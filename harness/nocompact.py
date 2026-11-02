#!/usr/bin/env python3
"""nocompact.py: a program linked with `-no_compact_unwind` is spliced, and unwinds through the
patch (macOS on AArch64, issue #33).

usage: nocompact.py    (BLAZE_BIN names the blaze built; blaze is found next
                        to it)

Such a program has `__eh_frame` and no `__unwind_info`, and the unwinder looks for every
frame in `__eh_frame`, to its end; bun links so. (What has only a compact entry, as most of
std has on AArch64, then has no frame at all, and a panic aborts, cargo's build too.) The
program is built with `blaze build` until the daemon serves the build, then edited a
few times: every edit after the first (which writes the whole object again) has to be
spliced, print what the source says, and have the unwinder find the frame of the edited
function where its symbol now is (`_Unwind_Find_FDE`). It is built for macOS 14, as nix's
shell builds.

Python 3, stdlib only.
"""
import glob
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(HERE, "toolchain")).read().strip())
TIMEOUT = 900
FILES = {
    "Cargo.toml": '[package]\nname = "ncu"\nversion = "0.1.0"\nedition = "2021"\n',
    ".cargo/config.toml": '[target.aarch64-apple-darwin]\n'
                          'rustflags = ["-C", "link-arg=-Wl,-no_compact_unwind"]\n',
    "src/main.rs": """use std::ffi::c_void;

unsafe extern "C" {
    fn _Unwind_Find_FDE(pc: *const c_void, bases: *mut [usize; 3]) -> *const c_void;
    fn _dyld_get_image_vmaddr_slide(image: u32) -> isize;
}

fn answer() -> u32 {
    1
}

/// Whether the unwinder has a frame for the function at `at` of the executable as linked. It is
/// asked one instruction in: macOS 15's `_Unwind_Find_FDE` looks up the byte before the pc it is
/// given, as for a return address, which for a function's first byte is the code before it.
fn has_frame(at: &str) -> bool {
    let at = usize::from_str_radix(at.trim_start_matches("0x"), 16).unwrap();
    let pc = at.wrapping_add(unsafe { _dyld_get_image_vmaddr_slide(0) } as usize) + 4;
    let mut bases = [0; 3];
    !unsafe { _Unwind_Find_FDE(pc as *const c_void, &mut bases) }.is_null()
}

fn main() {
    let framed = std::env::args().nth(1).map(|at| has_frame(&at));
    println!("{} {}", answer(), framed.map_or("unasked", |it| if it { "framed" } else { "unframed" }));
}
""",
}
EDITS = 4
# New functions enough that their frames outgrow the room behind the program's, which is what
# is left of the page: `__eh_frame` moves into the header's room.
MANY = 1000


def build(ws, env, step):
    ran = subprocess.run(["blaze", "build"], cwd=ws, env=env, capture_output=True, text=True,
                         timeout=TIMEOUT)
    said = ran.stdout + ran.stderr
    if ran.returncode != 0:
        raise SystemExit(f"{step}: the build failed:\n{said[-3000:]}")
    return said


def links(home):
    """The `link:` lines of the workers' logs, in order."""
    lines = []
    for log in sorted(glob.glob(os.path.join(home, "*", "*", "log"))):
        lines += re.findall(r"^link: \S+ \((.*)\)$", open(log, errors="replace").read(), re.M)
    return lines


def address_of(program, name):
    """Where the code of the function `name` is, as the executable was linked: blaze's symbol
    of it (rustc's is a jump there). The daemon's own link names it `blaze.<crate id>.<path>`,
    and a splice moves that symbol to the new code."""
    listed = subprocess.run(["nm", program], capture_output=True, text=True).stdout
    own = rf"_blaze(?:::{name}::\d+|\.[0-9a-f]+\.\S*::{name})"
    found = re.findall(rf"^([0-9a-f]+) [tTsS] {own}$", listed, re.M)
    return "0x" + found[0] if found else None


def sections(program):
    listed = subprocess.run(["otool", "-l", program], capture_output=True, text=True).stdout
    return set(re.findall(r"sectname (__unwind_info|__eh_frame)\b", listed))


def starts(program):
    """Where `__text` and `__eh_frame` start."""
    listed = subprocess.run(["otool", "-l", program], capture_output=True, text=True).stdout
    found = dict(re.findall(r"sectname (__text|__eh_frame)\s+segname __TEXT\s+addr (0x[0-9a-f]+)", listed))
    return {name: int(at, 16) for name, at in found.items()}


def edit(ws, env, home, program, step, want):
    """Builds, runs the program and says what is wrong with it, or "ok"."""
    seen = len(links(home))
    said = build(ws, env, step)
    at = address_of(program, "answer")
    ran = subprocess.run([program, at or "0"], capture_output=True, text=True, timeout=60)
    made = links(home)[seen:]
    how = made[-1] if made else "no link"
    verdict = "ok"
    if at is None:
        verdict = "NO SYMBOL OF `answer`"
    elif ran.returncode != 0 or ran.stdout.strip() != f"{want} framed":
        verdict = f"PRINTED {ran.stdout.strip()!r} (exit {ran.returncode}), NOT {want} framed"
    elif "[blaze]" not in said:
        verdict = "NOT SERVED BY THE DAEMON"
    elif not how.startswith("spliced"):
        verdict = "NOT SPLICED"
    return verdict, how


def main():
    if sys.platform != "darwin":
        print("nocompact.py: only Apple's linker writes `__unwind_info`; nothing to do")
        return
    bin_dir = os.path.dirname(os.path.abspath(os.environ["BLAZE_BIN"]))
    if not os.path.exists(os.path.join(bin_dir, "blaze")):
        raise SystemExit(f"no blaze beside {os.environ['BLAZE_BIN']}")
    root = tempfile.mkdtemp(prefix="blaze-nocompact-", dir="/tmp")
    ws, home = os.path.join(root, "ws"), os.path.join(root, "home")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", BLAZE_HOME=home,
               MACOSX_DEPLOYMENT_TARGET="14.0")
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        env.pop(name, None)
    program, source = os.path.join(ws, "target", "debug", "ncu"), os.path.join(ws, "src", "main.rs")
    wrong = 0
    try:
        build(ws, env, "first build")
        for _ in range(240):
            if "[blaze]" in build(ws, env, "wait for the daemon"):
                break
            time.sleep(0.5)
        else:
            raise SystemExit(f"no build said `[blaze]`; the daemon's log is in {home}")
        found = sections(program)
        verdict = "ok" if found == {"__eh_frame"} else f"SECTIONS {sorted(found)}"
        print(f"{'the link':30} {verdict:12} {'__eh_frame and no __unwind_info'}")
        wrong += verdict != "ok"
        for n in range(1, EDITS + 1):
            text = open(source).read()
            open(source, "w").write(text.replace(f"    {n}\n}}", f"    {n + 1}\n}}", 1))
            verdict, how = edit(ws, env, home, program, f"edit {n}", n + 1)
            # The first edit writes the whole object again.
            if n == 1 and verdict == "NOT SPLICED":
                verdict = "ok"
            wrong += verdict != "ok"
            print(f"{f'edit {n}':30} {verdict:12} {how[:110]}", flush=True)
        text = open(source).read()
        many = "".join(f"fn f{i}() -> u32 {{\n    {i}\n}}\n\n" for i in range(MANY))
        named = "".join(f"    std::hint::black_box(f{i} as fn() -> u32);\n" for i in range(MANY))
        used = "fn answer() -> u32 {\n" + named
        open(source, "w").write(text.replace("fn answer() -> u32 {\n", many + used, 1))
        verdict, how = edit(ws, env, home, program, f"{MANY} functions added", EDITS + 1)
        at = starts(program)
        if verdict == "ok" and not at.get("__eh_frame", 0) < at.get("__text", 0):
            verdict = "FRAMES NOT MOVED"
        wrong += verdict != "ok"
        print(f"{f'{MANY} functions added':30} {verdict:12} {how[:110]}", flush=True)
    finally:
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    print(f"{EDITS + 2 - wrong} of {EDITS + 2} right; under {root}")
    if wrong:
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
