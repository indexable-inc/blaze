#!/usr/bin/env python3
"""worktrees.py: several checkouts of one workspace built by one daemon.

The stress workspace is copied COUNT times, and each copy is built by `blaze build` in it with
one home, so one daemon builds them all. Each step edits files of some checkouts and asks for
the program of some. Every program asked for is run and held to what a fresh build of that
checkout does (rebuild.py).

With `--together` the programs of a step are asked for at once.
"""
import os, shutil, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from rebuild import WRONG, Project, copy, scratch
from storelib import Failed

COUNT = 3
MID, APP = "mid/src/lib.rs", "app/src/main.rs"
AREA = "sbase::square!(self.side)"
SUM = "items.iter().sum()"
# Not the same in any two checkouts from the start.
NOTE = "app/src/note.txt"
MORE = "app/src/more.rs"
# Of the workspace and of no package.
SHARED = "shared.txt"
# (what the step is, [(checkout, file, old or None to write it, new or None to remove it)], checkouts asked for in order, the ones that are not Rust)
STEPS = [
    ("nothing edited, the others asked for the first time", [], [1, 2, 0], []),
    ("nothing edited, all again", [], [0, 1, 2], []),
    ("one checkout edited", [(0, MID, AREA, AREA + " + 1")], [0, 1, 2], []),
    ("the same edit in another", [(1, MID, AREA, AREA + " + 1")], [1, 0, 2], []),
    ("a different edit to the same code in the third", [(2, MID, AREA, AREA + " + 7")], [2, 1, 0], []),
    ("two checkouts edited before either is asked",
     [(0, MID, AREA + " + 1", AREA + " + 2"), (1, APP, SUM, "items.iter().sum::<u32>() + 1")], [1, 0, 2], []),
    ("one checkout broken", [(1, MID, AREA + " + 1", AREA + ' + "x"')], [1, 0, 2, 1], [1]),
    ("another edited while it is broken", [(2, APP, SUM, "items.iter().sum::<u32>() + 5")], [2, 1, 0], [1]),
    ("mended", [(1, MID, AREA + ' + "x"', AREA + " + 4")], [0, 1, 2], []),
    ("a function added in one",
     [(2, APP, "fn main() {", "fn extra() -> u32 {\n    41\n}\n\nfn main() {\n    println!(\"extra {}\", extra());")],
     [2, 0, 1], []),
    ("one put back as another is", [(2, MID, AREA + " + 7", AREA + " + 2")], [2, 0], []),
    ("a file that is included edited in one", [(1, NOTE, "note of 1", "another note")], [0, 1, 2], []),
    ("a module with its file added in one",
     [(1, MORE, None, "pub fn more() -> u32 {\n    5\n}\n"),
      (1, APP, "fn main() {", "mod more;\n\nfn main() {\n    println!(\"more {}\", more::more());")],
     [1, 0, 2], []),
    ("the file edited", [(1, MORE, "5", "6")], [0, 1, 2], []),
    ("the file removed", [(1, MORE, "", None)], [1, 0, 2], [1]),
    ("and the module with it",
     [(1, APP, "mod more;\n\nfn main() {\n    println!(\"more {}\", more::more());", "fn main() {")],
     [1, 0, 2], []),
    ("a place asked for in one, by a crate that had not",
     [(1, MID, "pub fn seeded() -> u32 {",
       "pub fn here() -> &'static str {\n    env!(\"CARGO_MANIFEST_DIR\")\n}\n\npub fn seeded() -> u32 {"),
      (1, APP, 'println!("{}", HERE);', 'println!("{} {}", HERE, smid::here());')],
     [1, 0, 2, 1], []),
    ("that file edited in one", [(2, SHARED, "shared of 2", "shared anew")], [0, 2, 1], []),
    ("and made what another has", [(2, SHARED, "shared anew", "shared of 0")], [2, 1, 0], []),
    ("the manifest of one changed in what no crate reads",
     [(0, "app/Cargo.toml", "[package]", "[package.metadata]\nnote = 1\n\n[package]")], [1, 2, 0, 1], []),
    ("a manifest edited in one",
     [(2, "mid/Cargo.toml", "[dependencies]", "[features]\nmore = []\n\n[dependencies]")], [2, 0, 1], []),
    ("that one edited", [(2, MID, AREA + " + 2", AREA + " + 9")], [0, 2, 1], []),
    ("and another", [(0, MID, AREA + " + 2", AREA + " + 3")], [0, 2, 1], []),
]


def unanswered(daemon):
    """Prints where the daemon's threads wait and what it logged, for a build it never answered."""
    pid = daemon.process.pid
    stacks = ["sample", str(pid), "2"] if sys.platform == "darwin" else ["gdb", "-p", str(pid), "-batch", "-ex", "thread apply all bt"]
    try:
        print(subprocess.run(stacks, capture_output=True, text=True, timeout=60).stdout[-60000:], flush=True)
    except (OSError, subprocess.TimeoutExpired) as error:
        print(f"    no stacks of the daemon: {error}", flush=True)
    print("".join("    daemon: " + line for line in daemon.log().splitlines(True)[-300:]), flush=True)
    daemon.kill()


def checkout(work, at):
    ws = copy(os.path.join(HERE, "stress"), os.path.join(work, f"checkout{at}"))
    open(os.path.join(ws, NOTE), "w").write(f"note of {at}\n")
    main_rs = os.path.join(ws, APP)
    text = open(main_rs).read()
    assert "fn main() {" in text
    text += '\nconst HERE: &str = env!("CARGO_MANIFEST_DIR");\n'
    open(main_rs, "w").write(text.replace("fn main() {", 'fn main() {\n    print!("{}", include_str!("note.txt"));\n    println!("{}", HERE);\n    println!("shared {}", smid::shared());', 1))
    # A crate no step edits includes a file of the workspace that is outside its package.
    open(os.path.join(ws, SHARED), "w").write(f"shared of {at}")
    mid_rs = os.path.join(ws, MID)
    text = open(mid_rs).read()
    assert "pub fn seeded() -> u32 {" in text
    open(mid_rs, "w").write(text.replace("pub fn seeded() -> u32 {", "pub fn shared() -> &'static str {\n    include_str!(\"../../shared.txt\")\n}\n\npub fn seeded() -> u32 {", 1))
    return ws


def main():
    together = "--together" in sys.argv
    work = scratch("blaze-worktrees-")
    first = Project(checkout(work, 0), os.path.join(work, "c0"))
    checkouts = [first] + [Project(checkout(work, at), os.path.join(work, f"c{at}"), serves=first) for at in range(1, COUNT)]
    wrong = asked = 0
    try:
        for what, edits, order, broken in STEPS:
            wrong_before = wrong
            for at, path, old, new in edits:
                path = os.path.join(checkouts[at].ws, path)
                if old is None:
                    open(path, "w").write(new)
                    continue
                if new is None:
                    os.remove(path)
                    continue
                text = open(path).read()
                assert old in text, (what, old)
                open(path, "w").write(text.replace(old, new, 1))
            print(what, flush=True)
            # A program asked for twice in a step is asked for the second time after the others.
            asking = {at: (time.time(), checkouts[at].asking("sapp")) for at in order} if together else {}
            for at in order:
                project = checkouts[at]
                began, client = asking.pop(at, None) or (time.time(), project.asking("sapp"))
                try:
                    built = project.answer(client, "sapp")
                except Failed:
                    print(f"    checkout {at} NO ANSWER; the daemon's threads and what it logged:", flush=True)
                    unanswered(first.daemon)
                    raise
                took = (time.time() - began) * 1000
                verdict, said = project.held(built, "sapp", broken=at in broken)
                asked += 1
                wrong += verdict in WRONG
                print(f"    checkout {at} {verdict:16} {took:6.0f} ms  {said.splitlines()[0][:90] if said else ''}", flush=True)
                if verdict == "DAEMON DIED":
                    raise SystemExit(f"the daemon died: its log is in {first.home}")
            if wrong > wrong_before and first.daemon is not None:
                print("".join("    daemon: " + line for line in first.daemon.log().splitlines(True)[-300:]), flush=True)
    finally:
        first.stop()
    print(f"{asked - wrong} of {asked} right")
    if not wrong:
        shutil.rmtree(work, ignore_errors=True)
    sys.exit(1 if wrong else 0)


if __name__ == "__main__":
    main()
