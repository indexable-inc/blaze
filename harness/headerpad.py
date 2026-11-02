#!/usr/bin/env python3
"""headerpad.py: a program whose unwind table is larger than the room its header was given
is spliced all the same (macOS on AArch64, issue #13).

usage: headerpad.py    (BLAZE_BIN names the blaze built; blaze is found next
                        to it)

A splice moves the program's `__unwind_info` into the room Apple's linker leaves after the
load commands, which is as much as `-headerpad` says. A program of a few hundred crates has
more table than the 1 MiB the daemon starts with; here a small one is made short of room the
cheap way, by a `-headerpad` of its own in `.cargo/config.toml` that asks for 256 bytes.
The program is built with `blaze build` until the daemon serves the build, then
edited a few times. The first link is told the room the unwind information of what it links
wants, so it is linked once (#173: a large program was linked a second time, 260 s more), and
every edit after the first one (which writes the whole object again) has to be spliced, and
print what the source says.

It is built for macOS 14, as nix's shell builds, for which Apple's linker writes the loader's
fixups as chains unless the daemon's link line says otherwise; a splice declines those
(issue #30).

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
    "Cargo.toml": '[package]\nname = "spad"\nversion = "0.1.0"\nedition = "2021"\n',
    ".cargo/config.toml": '[target.aarch64-apple-darwin]\n'
                          'rustflags = ["-C", "link-arg=-Wl,-headerpad,0x100"]\n',
    "src/main.rs": """fn answer() -> u32 {
    1
}

fn main() {
    println!("{}", answer());
}
""",
}
EDITS = 4


def build(ws, env, step):
    ran = subprocess.run(["blaze", "build"], cwd=ws, env=env, capture_output=True, text=True,
                         timeout=TIMEOUT)
    said = ran.stdout + ran.stderr
    if ran.returncode != 0:
        raise SystemExit(f"{step}: the build failed:\n{said[-3000:]}")
    return said


def links(home):
    """The `link:` lines of the workers' logs, in order. A worker that was a spare keeps the
    spare's log."""
    lines = []
    for log in sorted(glob.glob(os.path.join(home, "*", "*", "log"))):
        lines += re.findall(r"^link: \S+ \((.*)\)$", open(log, errors="replace").read(), re.M)
    return lines


def main():
    if sys.platform != "darwin":
        print("headerpad.py: only Apple's linker leaves no more room than it is told; nothing to do")
        return
    bin_dir = os.path.dirname(os.path.abspath(os.environ["BLAZE_BIN"]))
    if not os.path.exists(os.path.join(bin_dir, "blaze")):
        raise SystemExit(f"no blaze beside {os.environ['BLAZE_BIN']}")
    root = tempfile.mkdtemp(prefix="blaze-headerpad-", dir="/tmp")
    ws, home = os.path.join(root, "ws"), os.path.join(root, "home")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", BLAZE_HOME=home,
               MACOSX_DEPLOYMENT_TARGET="14.0")
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        env.pop(name, None)
    program, source = os.path.join(ws, "target", "debug", "spad"), os.path.join(ws, "src", "main.rs")
    wrong = 0
    try:
        build(ws, env, "first build")
        for _ in range(240):
            if "[blaze]" in build(ws, env, "wait for the daemon"):
                break
            time.sleep(0.5)
        else:
            raise SystemExit(f"no build said `[blaze]`; the daemon's log is in {home}")
        made = links(home)
        again = [it for it in made if "again with -Wl,-headerpad" in it]
        verdict = "NO LINK" if not made else f"LINKED AGAIN: {again[0]}" if again else "ok"
        print(f"{'the first link':30} {verdict:12} {made[0][:110] if made else ''}")
        wrong += verdict != "ok"
        for n in range(1, EDITS + 1):
            text = open(source).read()
            open(source, "w").write(text.replace(f"    {n}\n}}", f"    {n + 1}\n}}", 1))
            seen = len(links(home))
            said = build(ws, env, f"edit {n}")
            printed = subprocess.run([program], capture_output=True, text=True, timeout=60).stdout.strip()
            made = links(home)[seen:]
            how = made[-1] if made else "no link"
            verdict = "ok"
            if printed != str(n + 1):
                verdict = f"PRINTED {printed!r}, NOT {n + 1}"
            elif "[blaze]" not in said:
                verdict = "NOT SERVED BY THE DAEMON"
            elif n > 1 and not how.startswith("spliced"):
                verdict = "NOT SPLICED"
            wrong += verdict != "ok"
            print(f"{f'edit {n}':30} {verdict:12} {how[:110]}", flush=True)
        # A daemon started anew links with the room the last one found, and no more than once.
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
        # The next daemon's workers write their logs where these were.
        for log in glob.glob(os.path.join(home, "*", "*", "log")):
            os.rename(log, log + ".old")
        for _ in range(240):
            if "[blaze]" in build(ws, env, "wait for the next daemon"):
                break
            time.sleep(0.5)
        text = open(source).read()
        open(source, "w").write(text.replace(f"    {EDITS + 1}\n}}", f"    {EDITS + 2}\n}}", 1))
        build(ws, env, "an edit for the next daemon")
        made = links(home)
        again = [it for it in made if "again with -Wl,-headerpad" in it]
        verdict = "NO LINK" if not made else f"LINKED AGAIN: {again[0]}" if again else "ok"
        print(f"{'a new daemon':30} {verdict:12} {made[0][:110] if made else ''}")
        wrong += verdict != "ok"
    finally:
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    print(f"{EDITS + 2 - wrong} of {EDITS + 2} right; under {root}")
    if wrong:
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
