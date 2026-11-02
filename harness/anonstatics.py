#!/usr/bin/env python3
"""anonstatics.py: a static in a `const _` block of a crate rustc built, which an edit reaches
through code blaze compiles, is rustc's static. Linux and macOS: on Windows `blaze`
hands builds to cargo.

usage: anonstatics.py    (BLAZE_BIN names the blaze built; blaze is found next to
                          it)

objc2's `declare_class!` (winit 0.30's) keeps a class's ivar offsets in `static mut`s inside a
`const _: () = { .. }`, written by rustc's code when the class registers and read by an
`#[inline]` method that objc2's generic code calls. rustc exports them under the path
`<module>::_::__OBJC2_IVAR_OFFSET`. With the program at opt-level 1, rustc inlines the reader
and the generic code that calls it, so an edit to anything inlined with them has the daemon
compile that chain itself, and it has to refer to rustc's static. blaze looked the static up
without the `_`, found no symbol and refused it ("writable static `__OBJC2_IVAR_OFFSET` without
a rustc symbol"), so every edit of a winit program's frame function fell back to cargo.

Here `mac` is objc2 (the macro, the trait, the generic reader), `dep` is winit (the class and
generic code the program instantiates) and `app` the program, whose `frame` inlines all of it.
Both libraries are outside the workspace, so rustc builds them. Every edit has to be served by
the daemon and the program has to print the offset rustc's code wrote plus the edited value.

Python 3, stdlib only.
"""
import os
import shutil
import subprocess
import sys
import tempfile
import time
import glob

HERE = os.path.dirname(os.path.abspath(__file__))
os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(HERE, "toolchain")).read().strip())
TIMEOUT = 900
FILES = {
    "ws/Cargo.toml": '[workspace]\nmembers = ["app"]\nresolver = "2"\n\n[profile.dev]\nopt-level = 1\n',
    "ws/app/Cargo.toml": '[package]\nname = "aapp"\nversion = "0.1.0"\nedition = "2021"\n\n'
                         '[dependencies]\nadep = { path = "../../dep" }\n',
    "ws/app/src/main.rs": """fn value() -> isize {
    1
}

#[inline(never)]
fn frame() -> isize {
    adep::on_main(|offset| offset + value())
}

fn main() {
    adep::register();
    println!("{}", frame());
}
""",
    "mac/Cargo.toml": '[package]\nname = "amac"\nversion = "0.1.0"\nedition = "2021"\n',
    "mac/src/lib.rs": """pub trait Declared {
    fn __offset() -> isize;
}

#[inline]
pub fn ivar_ptr<T: Declared>(base: isize) -> isize {
    base + T::__offset()
}

#[macro_export]
macro_rules! declare {
    ($name:ident) => {
        pub struct $name;
        const _: () = {
            static mut __OFFSET: isize = 0;
            impl $name {
                #[inline(never)]
                pub fn register() {
                    unsafe { __OFFSET = 42 };
                }
            }
            impl $crate::Declared for $name {
                #[inline]
                fn __offset() -> isize {
                    unsafe { __OFFSET }
                }
            }
        };
    };
}
""",
    "dep/Cargo.toml": '[package]\nname = "adep"\nversion = "0.1.0"\nedition = "2021"\n\n'
                      '[dependencies]\namac = { path = "../mac" }\n',
    "dep/src/lib.rs": """amac::declare!(Delegate);

#[inline]
pub fn on_main<R, F: FnOnce(isize) -> R>(f: F) -> R {
    f(amac::ivar_ptr::<Delegate>(0))
}

pub fn register() {
    Delegate::register();
}
""",
}
# (what the step is, (old, new) in app/src/main.rs, what the program prints after it)
STEPS = [
    ("a value inlined with the static's reader", ("    1\n", "    2\n"), "44"),
    ("and again", ("    2\n", "    5\n"), "47"),
]


def build(ws, env, step):
    ran = subprocess.run(["blaze", "build"], cwd=ws, env=env, capture_output=True, text=True,
                         timeout=TIMEOUT)
    said = ran.stdout + ran.stderr
    if ran.returncode != 0:
        raise SystemExit(f"{step}: the build failed:\n{said[-3000:]}")
    return said


def main():
    bin_dir = os.path.dirname(os.path.abspath(os.environ["BLAZE_BIN"]))
    if not glob.glob(os.path.join(bin_dir, "blaze*")):
        raise SystemExit(f"no blaze beside {os.environ['BLAZE_BIN']}")
    # The daemon's sockets are under its home, and a socket's path has to be short: macOS's
    # temporary directory is too long a start.
    root = tempfile.mkdtemp(prefix="blaze-anonstatics-", dir=None if os.name == "nt" else "/tmp")
    ws, home = os.path.join(root, "ws"), os.path.join(root, "h")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(root, path)), exist_ok=True)
        open(os.path.join(root, path), "w").write(text)
    # A home of its own, so its store has no function compiled by another build of blaze.
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", BLAZE_HOME=home)
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        env.pop(name, None)
    program, source = os.path.join(ws, "target", "debug", "aapp"), os.path.join(ws, "app", "src", "main.rs")
    wrong = 0
    try:
        build(ws, env, "first build")
        for _ in range(240):
            if "[blaze]" in build(ws, env, "wait for the daemon"):
                break
            time.sleep(0.5)
        else:
            raise SystemExit(f"no build said `[blaze]`; the daemon's log is in {home}")
        for what, (old, new), expected in STEPS:
            text = open(source).read()
            assert old in text, (what, old)
            open(source, "w").write(text.replace(old, new, 1))
            said = build(ws, env, what)
            printed = subprocess.run([program], capture_output=True, text=True, timeout=60).stdout.strip()
            verdict = "ok"
            if printed != expected:
                verdict = f"PRINTED {printed!r}, NOT {expected!r}"
            elif "[blaze]" not in said:
                why = [line for line in said.splitlines() if "blaze" in line]
                verdict = "NOT SERVED BY THE DAEMON: " + " | ".join(why)[-600:]
            wrong += verdict != "ok"
            print(f"{what:42} {verdict}", flush=True)
    finally:
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    print(f"{len(STEPS) - wrong} of {len(STEPS)} right; under {root}")
    if wrong:
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
