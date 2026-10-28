#!/usr/bin/env python3
"""stdstatics.py: a program reaches the writable statics declared in the standard library's
bodies (`thread::min_stack`'s `MIN`, each `thread_local!`'s `__RUST_STD_INTERNAL_VAL`, the
`KEYS` of `RandomState::new`) through std's generic or inlined code it compiles itself, and
links to the one copy the standard library's unit defines, when that unit was built on another
machine (#840, #224).

usage: stdstatics.py    (BLAZE_BIN names the blaze built; blaze is found next to it)

A release ships the standard library's unit made by `blaze-engine std` on CI's runner, and a
build fetches it (BLAZE_STD_FROM). A static in a body is named after where it is written, and
that named the file by its path on the machine: CI's runner had the sources of std under
/Users/runner/.rustup/..., a user under their own home, so the program asked for names the unit
does not define and was refused ("writable static `MIN` without a rustc symbol"): every program
that spawned a thread or made a `HashMap` (anim, #840). Built where it is used, as the other
harnesses have it, the unit names the same paths and nothing shows.

Here the unit is made by `blaze-engine std` from a copy of blaze's library/ in another
directory (`BLAZE_LIBRARY`), and each program is built by `blaze run`, dev and `--release`, with
the toolchain where it is: it has to fetch the unit, not build one, and print what cargo's
build of the same source prints.

Linux and macOS. Python 3, stdlib only.
"""
import glob, os, re, shutil, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from blaze_cli import BLAZE, base_env

TIMEOUT = 1800
LIBRARY = os.path.join(os.path.dirname(HERE), "library")
MEMBERS = ("khash", "kspawn", "ktls")
FILES = {
    "Cargo.toml": '[workspace]\nmembers = ["khash", "kspawn", "ktls"]\nresolver = "2"\n',
    # #840: `HashMap::new()` reaches `RandomState::new`'s `KEYS`, a `thread_local!`.
    "khash/src/main.rs": """use std::collections::HashMap;

fn main() {
    let mut map = HashMap::new();
    map.insert("a", 1);
    map.insert("b", 2);
    let mut pairs: Vec<_> = map.iter().collect();
    pairs.sort();
    println!("{:?} {:?}", map.get("a"), pairs);
}
""",
    # `spawn` reaches `MIN` of the stack size; the name std's code sets in the thread it starts
    # is read back through `thread::current`, std's thread-local on both sides.
    "kspawn/src/main.rs": """fn main() {
    let joined = std::thread::spawn(|| 6 * 7).join().unwrap();
    let named = std::thread::Builder::new()
        .name("worker".into())
        .stack_size(1 << 20)
        .spawn(|| std::thread::current().name().map(str::to_owned))
        .unwrap();
    println!("joined {joined} named {:?}", named.join().unwrap());
    println!("main {:?}", std::thread::current().name());
}
""",
    "ktls/src/main.rs": """use std::cell::{Cell, RefCell};

thread_local! {
    static COUNT: Cell<u32> = const { Cell::new(0) };
    static NAME: RefCell<String> = RefCell::new(String::from("main"));
}

fn bump() -> u32 {
    COUNT.with(|count| {
        count.set(count.get() + 1);
        count.get()
    })
}

fn main() {
    bump();
    bump();
    let other = std::thread::spawn(|| {
        NAME.with(|name| name.borrow_mut().push_str("-other"));
        (bump(), NAME.with(|name| name.borrow().clone()))
    });
    let other = other.join().unwrap();
    println!("main {} {} other {:?}", bump(), NAME.with(|name| name.borrow().clone()), other);
}
""",
}
for member in MEMBERS:
    FILES[f"{member}/Cargo.toml"] = f'[package]\nname = "{member}"\nversion = "0.1.0"\nedition = "2024"\n'
PRINTS = {
    "khash": 'Some(1) [("a", 1), ("b", 2)]\n',
    "kspawn": 'joined 42 named Some("worker")\nmain Some("main")\n',
    "ktls": 'main 3 main other (1, "main-other")\n',
}


def write(root, files):
    for path, text in files.items():
        os.makedirs(os.path.dirname(os.path.join(root, path)), exist_ok=True)
        with open(os.path.join(root, path), "w") as f:
            f.write(text)


def elsewhere(sysroot, to):
    """The toolchain at `sysroot` and blaze's `library/` as another machine has them: under
    `to`, the library a copy of its own (`to/library`, which the unit is made from with
    `BLAZE_LIBRARY` naming it), and the toolchain linked to where it is. No toolchain's
    `rust-src` is read: the standard library's source is blaze's library/ alone."""
    os.makedirs(to)
    os.symlink(sysroot, os.path.join(to, "toolchain"))
    library = os.path.join(to, "library")
    shutil.copytree(LIBRARY, library, symlinks=True)
    return os.path.join(to, "toolchain"), library


def main():
    if not shutil.which("blaze", path=os.path.dirname(BLAZE)):
        sys.exit(f"no blaze beside {BLAZE}")
    root = tempfile.mkdtemp(prefix="blaze-stdstatics-", dir="/tmp")
    env = base_env(os.path.join(root, "home"))
    sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True, env=env,
                             check=True).stdout.strip()
    other, library = elsewhere(sysroot, os.path.join(root, "other-machine"))
    remote = os.path.join(root, "std")
    made = subprocess.run([BLAZE, "std", remote, other], capture_output=True, text=True,
                          env=dict(base_env(os.path.join(root, "home-std")), BLAZE_LIBRARY=library),
                          timeout=TIMEOUT)
    if made.returncode != 0:
        sys.exit(f"`blaze-engine std` failed:\n{made.stderr[-3000:]}")
    env["BLAZE_STD_FROM"] = remote
    ws = os.path.join(root, "ws")
    write(ws, FILES)
    wrong = []

    def check(what, ok, said=""):
        print(f"{what:62} {'ok' if ok else 'WRONG'}", flush=True)
        if not ok:
            wrong.append(what)
            if said:
                print(said[-3000:], flush=True)

    try:
        for flags in ([], ["--release"]):
            for member in MEMBERS:
                named = " ".join(["`blaze run -p", member, *flags]) + "`"
                ran = subprocess.run(["blaze", "run", "-q", "-p", member, *flags], cwd=ws, env=env,
                                     capture_output=True, text=True, timeout=TIMEOUT)
                check(f"{named} runs", (ran.returncode, ran.stdout) == (0, PRINTS[member]),
                      f"exit {ran.returncode} {ran.stdout!r}\n{ran.stderr}")
                check("  ... and building with cargo is not what ran", "building with cargo" not in ran.stderr,
                      ran.stderr)
                reference = subprocess.run(["cargo", "run", "-q", "-p", member, *flags], cwd=ws,
                                           env=dict(env, CARGO_TARGET_DIR=os.path.join(root, "ref")),
                                           capture_output=True, text=True, timeout=TIMEOUT)
                check("  ... and cargo's build prints the same", reference.stdout == PRINTS[member],
                      reference.stderr)
        logs = "".join(open(it, errors="replace").read()
                       for it in glob.glob(os.path.join(root, "home", "**", "log"), recursive=True))
        check("the standard library was fetched, not built here",
              "standard library: from the store" in logs
              and not re.search(r"standard library: \d+ crates, ", logs), logs[-3000:])
    finally:
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    if wrong:
        sys.exit(f"stdstatics.py: {len(wrong)} wrong; under {root}")
    print("stdstatics.py: all right")
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
