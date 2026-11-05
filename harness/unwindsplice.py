#!/usr/bin/env python3
"""unwindsplice.py: a panic unwinds through spliced code, also once an edit's new functions
outgrow the room the image keeps for unwind tables (v1's nocompact.py, issue #33).

usage: unwindsplice.py    (BLAZE_BIN names the blaze built; blaze is found next to it)

v1 linked with Apple's linker, and a program linked with `-no_compact_unwind` (as bun is) had
only `__eh_frame`, which a splice had to keep finding frames in. blaze links with its own
linker (crates/blaze-link), which writes the unwind tables itself and takes no link argument
from RUSTFLAGS, so what is checked is what that guarded, on the program as it runs: a function
the edits change panics under `catch_unwind` with a guard on its frame, and the program has to
print that the guard was dropped and the panic caught, after every edit. Every edit after the
first (which may write the whole object again) has to be spliced, as its build's `blaze-work`
line says (`--message-format=json`, storelib.links); the last adds a thousand functions,
which outgrow the room kept for their unwind information, and a new function the panic
unwinds through: that one is spliced or laid out in full saying why.

Python 3, stdlib only.
"""
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from storelib import linked  # noqa: E402

os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(HERE, "toolchain")).read().strip())
PROGRAM = "usplice"
FILES = {
    "Cargo.toml": f'[package]\nname = "{PROGRAM}"\nversion = "0.1.0"\nedition = "2021"\n',
    "src/main.rs": """struct Guard(&'static str);

impl Drop for Guard {
    fn drop(&mut self) {
        println!("dropped {}", self.0);
    }
}

fn answer(n: u32) -> u32 {
    let _guard = Guard("answer");
    if n > 0 {
        panic!("unwound");
    }
    1
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(|| answer(1)).is_err();
    println!("{} {}", answer(0), if caught { "caught" } else { "not caught" });
}
""",
}
EDITS = 4
# New functions enough that their unwind information outgrows the room the image keeps.
MANY = 1000
DEEP = """fn deep(n: u32) -> u32 {
    let _guard = Guard("deep");
    answer(n) + 1
}

"""


def main() -> None:
    bin_dir = os.path.dirname(os.path.abspath(os.environ["BLAZE_BIN"]))
    if not os.path.exists(os.path.join(bin_dir, "blaze")):
        raise SystemExit(f"no blaze beside {os.environ['BLAZE_BIN']}")
    root = os.path.realpath(tempfile.mkdtemp(prefix="blaze-unwindsplice-", dir=None if os.name == "nt" else "/tmp"))
    ws, home = os.path.join(root, "ws"), os.path.join(root, "home")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", BLAZE_HOME=home)
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        env.pop(name, None)
    program, source = os.path.join(ws, "target", "debug", PROGRAM), os.path.join(ws, "src", "main.rs")
    rows: list[str] = []

    def edit(old: str, new: str) -> None:
        text = open(source).read()
        if old not in text:
            raise SystemExit(f"{source} has no {old!r}")
        open(source, "w").write(text.replace(old, new, 1))

    def step(what: str, want: str, hows: tuple[str, ...]) -> None:
        made = linked(ws, env, what, PROGRAM)
        ran = subprocess.run([program], capture_output=True, text=True, timeout=60)
        printed = ran.stdout.strip()
        how = ", ".join(str(it) for it in made) or "no link"
        verdict = "ok"
        if ran.returncode != 0 or printed != want:
            verdict = f"PRINTED {printed!r} (exit {ran.returncode}), NOT {want!r}"
        elif len(made) != 1:
            verdict = f"LINKED {len(made)} TIMES"
        elif hows and made[0].how not in hows:
            verdict = "NOT SPLICED"
        rows.append(verdict)
        print(f"{what:30} {verdict:12} {how[:110]}", flush=True)

    try:
        step("the first link", "dropped answer\ndropped answer\n1 caught", ())
        for n in range(1, EDITS + 1):
            edit(f"    {n}\n}}", f"    {n + 1}\n}}")
            # The first edit may write the whole object again.
            step(f"edit {n}", f"dropped answer\ndropped answer\n{n + 1} caught",
                 ("spliced",) if n > 1 else ("spliced", "full"))
        many = "".join(f"fn f{i}() -> u32 {{\n    {i}\n}}\n\n" for i in range(MANY))
        named = "".join(f"    std::hint::black_box(f{i} as fn() -> u32);\n" for i in range(MANY))
        edit("fn answer(n: u32) -> u32 {\n", many + DEEP + "fn answer(n: u32) -> u32 {\n" + named)
        edit("catch_unwind(|| answer(1))", "catch_unwind(|| deep(1))")
        step(f"{MANY} functions added", f"dropped answer\ndropped deep\ndropped answer\n{EDITS + 1} caught",
             ("spliced", "full"))
    finally:
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    wrong = sum(it != "ok" for it in rows)
    print(f"{len(rows) - wrong} of {len(rows)} right; under {root}")
    if wrong:
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
