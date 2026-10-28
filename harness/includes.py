#!/usr/bin/env python3
# pyright: basic
"""includes.py: a daemon follows edits to a file that only `include!` names.

The file is not a `.rs` file, so nothing loads it until the macro is expanded. Each step edits
it or the file that includes it, asks the daemon for the program and compares what it prints
with what `cargo build` of the same source prints.

usage: includes.py '{}'

Its steps are fixed, so its input is the empty JSON object (inline, the path of a file, or `-`
for standard input): any field is refused. Run it in the repository's Python workspace: `uv run
--frozen python harness/includes.py '{}'` from the root. The scripts that import `follow` run
steps of their own.
"""

import errno
import os
import re
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

from typed_input import fail, parse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import linkline
from difftest import BLAZE, RUNNER, TARGET_ARGS, Fixture, built, sysroot_rlibs

TOML = """[package]
name = "sinc"
version = "0.1.0"
edition = "2021"
"""
DATA_TEXT = """pub const TABLE: [u32; 3] = [3, 5, 8];
pub fn pick(i: usize) -> u32 {
    TABLE[i % 3] * 2
}
macro_rules! make {
    ($name:ident, $v:expr) => {
        pub fn $name() -> u32 {
            $v + pick(1)
        }
    };
}
"""
MAIN_TEXT = """mod data {
    include!("table.rs.data");
    make!(made, 100);
}

fn main() {
    println!("{} {} {:?}", data::pick(2), data::made(), data::TABLE);
    println!("{} {:?}", include_str!("label.txt"), include_bytes!("bytes.dat"));
}
"""
DATA, MAIN = "src/table.rs.data", "src/main.rs"
LABEL, BYTES = "src/label.txt", "src/bytes.dat"
# (what the step is, [(file, old, new)], whether cargo rejects it). An `old` of None makes the
# file, a `new` of None removes it.
STEPS = [
    ("nothing edited", [], False),
    ("a body in the included file", [(DATA, "* 2", "* 7")], False),
    ("a constant in the included file", [(DATA, "[3, 5, 8]", "[4, 6, 9]")], False),
    ("a macro in the included file", [(DATA, "$v + pick(1)", "$v * 2 + pick(0)")], False),
    ("the included file broken", [(DATA, "* 7", '* "x"')], True),
    ("mended", [(DATA, '* "x"', "* 11")], False),
    (
        "a function added to it and used",
        [
            (DATA, "macro_rules!", "pub fn more() -> u32 {\n    77\n}\nmacro_rules!"),
            (MAIN, "fn main() {", 'fn main() {\n    println!("{}", data::more());'),
        ],
        False,
    ),
    ("the including file alone", [(MAIN, "make!(made, 100)", "make!(made, 200)")], False),
    ("the included file again", [(DATA, "77", "78")], False),
    ("remove include_str input", [(LABEL, "old label", None)], True),
    ("the input is still missing", [], True),
    ("remove the reference", [(MAIN, 'include_str!("label.txt")', '"fallback"')], False),
    ("restore the reference", [(MAIN, '"fallback"', 'include_str!("label.txt")')], True),
    ("restore include_str input", [(LABEL, None, "new label")], False),
    ("remove include_bytes input", [(BYTES, "old bytes", None)], True),
    ("restore include_bytes input", [(BYTES, None, "new bytes")], False),
]


def follow(files, steps):
    """Edits a crate called `sinc` a step at a time under a daemon, against cargo."""
    ws = tempfile.mkdtemp(prefix="blaze-includes-")
    os.makedirs(os.path.join(ws, "src"))
    for path, text in (("Cargo.toml", TOML), *files):
        Path(ws, path).write_text(text)
    work = tempfile.mkdtemp(prefix="blaze-includes-out-")
    socket, log_path = os.path.join(work, "blaze.sock"), os.path.join(work, "daemon.log")
    reference = os.path.join(work, "reference")
    fx = Fixture(ws, ["sinc"], ["-p", "sinc"])
    fx.rustc_build()
    obj, patch, binary = (os.path.join(work, f"sinc.{n}") for n in ("o", "patch.o", "bin"))
    link = list(fx.link_args)
    link = linkline.with_output(linkline.ahead(link, [patch, obj], BLAZE), binary)
    symbols = [x for rlib in fx.rlibs + sysroot_rlibs() for x in ("--symbols", rlib)]
    # Edits are added to the linked binary where that can be done; BLAZE_SPLICE=0 links each in full.
    splice = ["--binary", binary]
    ask = [
        "--crate",
        "sinc",
        "--out",
        obj,
        "--patch",
        patch,
        "--link-cmd",
        linkline.join(link),
        *splice,
        *symbols,
    ]
    with open(log_path, "a") as out:
        daemon = subprocess.Popen([BLAZE, ws, *TARGET_ARGS, "--socket", socket, *ask], stderr=out, stdout=out)
    while "watching" not in Path(log_path).read_text():
        if daemon.poll() is not None:
            raise SystemExit(f"the daemon did not start: {log_path}")
        time.sleep(0.05)
    # A daemon whose first build took code of the checkout from the store (BLAZE_CACHE) builds
    # again by itself before anything else (#12), and a request waits for that build: what it
    # compiles is no step's.
    subprocess.run([BLAZE, "sync", socket, *ask], check=False, capture_output=True, text=True, timeout=120)

    def run(path):
        # After a full link the daemon writes the debug info in place, and Linux runs no
        # program while its file is open for writing ("text file busy"): what is given the
        # program waits, as `blaze run` does.
        deadline = time.monotonic() + 120
        while True:
            try:
                ran = subprocess.run([*RUNNER, path], check=False, capture_output=True, text=True, timeout=30)
            except OSError as error:
                if error.errno == errno.ETXTBSY and time.monotonic() < deadline:
                    time.sleep(0.1)
                    continue
                return f"did not run: {error}"
            except subprocess.SubprocessError as error:
                return f"did not run: {error}"
            return ran.stdout + (f"[exit {ran.returncode}]" if ran.returncode else "")

    def linked(seen):
        """How the daemon made the binary since the log was `seen` long."""
        links = re.findall(r"^link: \S+ \((.*)\)$", Path(log_path).read_text()[seen:], re.MULTILINE)
        if not links:
            return "no link"
        if links[-1].startswith("spliced"):
            return "spliced"
        reason = re.search(r"not spliced: (.*)", links[-1])
        return "full: " + (reason[1] if reason else "not asked to splice")

    wrong = 0
    # A step may say after `broken` what the way the binary was made has to contain.
    for what, edits, broken, *how in steps:
        seen = len(Path(log_path).read_text())
        for path, old, new in edits:
            path = os.path.join(ws, path)
            if old is None:
                assert not os.path.exists(path), (what, path)
                Path(path).write_text(new)
            elif new is None:
                assert Path(path).read_text() == old, (what, path)
                os.unlink(path)
            else:
                text = Path(path).read_text()
                assert old in text, (what, old)
                Path(path).write_text(text.replace(old, new, 1))
        time.sleep(0.02)
        began = time.time()
        reply = subprocess.run(
            [BLAZE, "sync", socket, *ask], check=False, capture_output=True, text=True, timeout=120
        )
        took = (time.time() - began) * 1000
        reply = (reply.stdout + reply.stderr).strip()
        cargo = subprocess.run(
            ["cargo", "build", "-q", *TARGET_ARGS],
            check=False,
            cwd=ws,
            capture_output=True,
            text=True,
            env={**os.environ, "CARGO_TARGET_DIR": reference},
        )
        assert (cargo.returncode != 0) == broken, cargo.stderr[-2000:]
        if daemon.poll() is not None:
            raise SystemExit(f"the daemon died: {log_path}")
        if broken:
            verdict = "refused" if not reply.startswith("ok") else "ANSWERED OK"
        elif not reply.startswith("ok"):
            verdict = "REFUSED"
        else:
            verdict = "same" if run(binary) == run(built(reference, "sinc")) else "DIFFERENT OUTPUT"
        made, failed = linked(seen), verdict.isupper()
        # `compiled N` among what a step says: how many functions its builds may compile.
        limit = [int(it.split()[1]) for it in how if it.startswith("compiled ")]
        how = [it for it in how if not it.startswith("compiled ")]
        if how and not failed and how[0] not in made:
            verdict, failed = f"NOT {how[0]}: {made}"[:60], True
        compiled = sum(
            int(it) for it in re.findall(r"^stats: (\d+) of", Path(log_path).read_text()[seen:], re.MULTILINE)
        )
        if limit and not failed and compiled > limit[0]:
            verdict, failed = f"COMPILED {compiled} > {limit[0]}", True
        wrong += failed
        print(f"{what:40} {verdict:16} {took:6.0f} ms  {reply[:90]}", flush=True)
    daemon.kill()
    print(f"{len(steps) - wrong} of {len(steps)} right; log {log_path}")
    sys.exit(1 if wrong else 0)


SCRIPT = "includes.py"


@dataclass(frozen=True)
class Input:
    """No field: the steps are STEPS."""


if __name__ == "__main__":
    match sys.argv[1:]:
        case [text]:
            parse(Input, text, SCRIPT)
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} '{{}}' (see the top of the file)")
    follow([(DATA, DATA_TEXT), (MAIN, MAIN_TEXT), (LABEL, "old label"), (BYTES, "old bytes")], STEPS)
