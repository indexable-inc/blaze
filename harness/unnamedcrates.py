#!/usr/bin/env python3
"""unnamedcrates.py: a program links the dependencies its code names, as rustc loads them, and
not the others cargo hands it: a crate given with `--extern` that no path looks up and no
`extern crate` names is not loaded by rustc, so its constructors, `#[used]` statics and
`#[global_allocator]` are not in the program. blaze linked every crate of the graph, which put
blaze-engine's library (salsa's constructors, jemalloc, LLVM) into `blaze-rustc` too.

usage: unnamedcrates.py    (BLAZE_BIN names the blaze built; blaze is found next to it)

`noisy` is a library with a constructor that prints. Each program depends on it and names it
in one way or none, and is built by `blaze run`, dev and `--release`: it has to print what
cargo's build of the same source prints, the constructor's line or not.

Linux and macOS. Python 3, stdlib only.
"""
import os, shutil, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from blaze_cli import BLAZE, base_env

TIMEOUT = 1800
NOISY = """#[used]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
#[cfg_attr(not(target_os = "macos"), unsafe(link_section = ".init_array"))]
static CONSTRUCTOR: extern "C" fn() = constructor;

extern "C" fn constructor() {
    println!("noisy");
}

pub fn hello() -> &'static str {
    "hello"
}
"""
# Each program's `main.rs`, and what it prints.
PROGRAMS = {
    # Named nowhere: rustc does not load it.
    "bare": ("fn main() {\n    println!(\"main\");\n}\n", "main\n"),
    "underscore": ("use noisy as _;\n\nfn main() {\n    println!(\"main\");\n}\n", "noisy\nmain\n"),
    "externcrate": ("extern crate noisy;\n\nfn main() {\n    println!(\"main\");\n}\n", "noisy\nmain\n"),
    # Named only in a macro's expansion.
    "inmacro": ("macro_rules! hello {\n    () => {\n        noisy::hello()\n    };\n}\n\n"
                "fn main() {\n    println!(\"{}\", hello!());\n}\n", "noisy\nhello\n"),
    # Named only where a false `#[cfg]` leaves the code out.
    "cfgout": ("#[cfg(any())]\nuse noisy as _;\n\nfn main() {\n    println!(\"main\");\n}\n", "main\n"),
}
FILES = {
    "Cargo.toml": "[workspace]\nmembers = [%s]\nresolver = \"2\"\n"
                  % ", ".join(f'"{it}"' for it in ["noisy", *PROGRAMS]),
    "noisy/Cargo.toml": '[package]\nname = "noisy"\nversion = "0.1.0"\nedition = "2024"\n',
    "noisy/src/lib.rs": NOISY,
}
for program, (main, _) in PROGRAMS.items():
    FILES[f"{program}/Cargo.toml"] = (f'[package]\nname = "{program}"\nversion = "0.1.0"\nedition = "2024"\n\n'
                                      '[dependencies]\nnoisy = { path = "../noisy" }\n')
    FILES[f"{program}/src/main.rs"] = main


def write(root, files):
    for path, text in files.items():
        os.makedirs(os.path.dirname(os.path.join(root, path)), exist_ok=True)
        with open(os.path.join(root, path), "w") as f:
            f.write(text)


def main():
    if not shutil.which("blaze", path=os.path.dirname(BLAZE)):
        sys.exit(f"no blaze beside {BLAZE}")
    root = tempfile.mkdtemp(prefix="blaze-unnamedcrates-", dir="/tmp")
    env = base_env(os.path.join(root, "home"))
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
            for program, (_, prints) in PROGRAMS.items():
                named = " ".join(["`blaze run -p", program, *flags]) + "`"
                reference = subprocess.run(["cargo", "run", "-q", "-p", program, *flags], cwd=ws,
                                           env=dict(env, CARGO_TARGET_DIR=os.path.join(root, "ref")),
                                           capture_output=True, text=True, timeout=TIMEOUT)
                check(f"`cargo run -p {program}` prints {prints!r}", reference.stdout == prints,
                      f"{reference.stdout!r}\n{reference.stderr}")
                ran = subprocess.run(["blaze", "run", "-q", "-p", program, *flags], cwd=ws, env=env,
                                     capture_output=True, text=True, timeout=TIMEOUT)
                check(f"{named} prints the same", (ran.returncode, ran.stdout) == (0, reference.stdout),
                      f"exit {ran.returncode} {ran.stdout!r}\n{ran.stderr}")
                check("  ... and building with cargo is not what ran", "building with cargo" not in ran.stderr,
                      ran.stderr)
    finally:
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    if wrong:
        sys.exit(f"unnamedcrates.py: {len(wrong)} wrong; under {root}")
    print("unnamedcrates.py: all right")
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
