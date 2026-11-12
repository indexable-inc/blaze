#!/usr/bin/env python3
"""ready_check.py: whether a lane's ready list may be snapshotted (committed) now.

Lanes write in one checkout and the runner commits only the paths a lane lists in
`<ready>/<lane>.txt` (one repository path a line). A list can outlive the content it declared
(annoyance records-1112c-1): the lane, or another lane in a shared file, writes on after the
list was filed. This refuses a lane's list when

  (a) a `mod x;` or `pub mod x;` in a listed Rust file names a module whose file is neither
      tracked by git nor covered by the list (2026-11-12: `pub mod awaited;` in a listed
      deps.rs, awaited.rs in nobody's list; the commit was reverted), or
  (b) a listed file changed in the last `quiet_seconds` (someone is still writing it), or
  (c) a listed path does not exist.

Input, one JSON value:

    {"repo": "/Volumes/Projects/blaze-int-1112", "ready": "/tmp/round2/ready",
     "lanes": ["sealgroups"], "quiet_seconds": 60}

`lanes` is optional: every `<ready>/*.txt` by default. Prints one line a lane, `ok <lane> N
paths` or `REFUSED <lane>: <what and where>` for each finding, and exits 1 when any lane is
refused. It reads only: the runner tells the lane what failed and commits nothing of it.
"""
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path

# Run with `python3 -I`, which leaves the script's directory off the path: ran.py is beside it.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ran

MOD = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;", re.MULTILINE)
ROOTS = ("lib.rs", "main.rs", "mod.rs")
# String literals, raw first: a `mod x;` inside one is a test fixture's source, not a module.
STRINGS = re.compile(r'r(#+)".*?"\1|r".*?"|"(?:\\.|[^"\\])*"', re.DOTALL)


def code(text: str) -> str:
    """`text` with every string literal's inside blanked, its newlines kept so lines count."""
    return STRINGS.sub(lambda hit: "".join(it if it == "\n" else " " for it in hit.group(0)), text)


@dataclass(frozen=True)
class Input:
    repo: str
    ready: str
    lanes: tuple[str, ...] = ()
    quiet_seconds: int = 60


def listed_files(repo: Path, paths: list[str]) -> tuple[list[Path], list[str]]:
    """The files the list covers (a directory stands for every file under it), and the listed
    paths that do not exist."""
    files: list[Path] = []
    missing: list[str] = []
    for path in paths:
        at = repo / path
        if at.is_dir():
            files.extend(it for it in sorted(at.rglob("*")) if it.is_file())
        elif at.is_file():
            files.append(at)
        else:
            missing.append(path)
    return files, missing


def module_files(file: Path, name: str) -> tuple[Path, Path]:
    """Where `mod name;` in `file` looks: beside a crate root or a mod.rs, else under the
    directory named after the file."""
    home = file.parent if file.name in ROOTS else file.parent / file.stem
    return home / f"{name}.rs", home / name / "mod.rs"


def findings(repo: Path, tracked: set[str], paths: list[str], quiet: int, now: float) -> list[str]:
    files, missing = listed_files(repo, paths)
    found = [f"{path}: listed, and there is no such file or directory" for path in missing]
    covered = {str(it.relative_to(repo)) for it in files}
    for file in files:
        age = now - file.stat().st_mtime
        if age < quiet:
            found.append(f"{file.relative_to(repo)}: changed {age:.0f} s ago (still being written; the bound is {quiet} s)")
        if file.suffix != ".rs":
            continue
        text = code(file.read_text(errors="replace"))
        for hit in MOD.finditer(text):
            before = text[: hit.start()].rstrip().rsplit("\n", 1)[-1]
            if "#[path" in before:
                continue
            candidates = [str(it.relative_to(repo)) for it in module_files(file, hit.group(1))]
            if not any(it in tracked or it in covered for it in candidates):
                line = text.count("\n", 0, hit.start()) + 1
                found.append(
                    f"{file.relative_to(repo)}:{line}: `mod {hit.group(1)};` names {candidates[0]}, which is neither tracked nor in the list"
                )
    return found


def main(argv: list[str]) -> None:
    if len(argv) != 2:
        sys.exit("ready_check.py takes one JSON value (see its module docs)")
    try:
        ask = ran.parsed(Input, json.loads(argv[1]))
    except ValueError as refused:
        sys.exit(f"ready_check.py: {refused}")
    repo, ready = Path(ask.repo), Path(ask.ready)
    done = subprocess.run(["git", "-C", str(repo), "ls-files"], capture_output=True, text=True, check=False)
    if done.returncode != 0:
        sys.exit(f"ready_check.py: git ls-files in {repo}: {done.stderr.strip()}")
    tracked = set(done.stdout.splitlines())
    lanes = ask.lanes or tuple(sorted(it.stem for it in ready.glob("*.txt")))
    now, refused = time.time(), False
    # A module file another lane of this run lists counts as there: lanes that share a file
    # are snapshotted together (2026-11-12: `mod found;` in a lib.rs four lanes list, found.rs
    # on one lane's list), and a refusal of any lane of the run stops the whole snapshot.
    together: set[str] = set()
    for lane in lanes:
        file = ready / f"{lane}.txt"
        if file.is_file():
            listed = [it.strip().rstrip("/") for it in file.read_text().splitlines() if it.strip()]
            together |= {str(it.relative_to(repo)) for it in listed_files(repo, listed)[0]}
    for lane in lanes:
        file = ready / f"{lane}.txt"
        if not file.is_file():
            print(f"REFUSED {lane}: {file} is absent (the lane is mid-write or has no list)")
            refused = True
            continue
        paths = [it.strip().rstrip("/") for it in file.read_text().splitlines() if it.strip()]
        found = findings(repo, tracked | together, paths, ask.quiet_seconds, now)
        list_age = now - os.stat(file).st_mtime
        if list_age < ask.quiet_seconds:
            found.append(f"{file}: the list itself changed {list_age:.0f} s ago")
        if found:
            refused = True
            for it in found:
                print(f"REFUSED {lane}: {it}")
        else:
            print(f"ok {lane} {len(paths)} paths")
    sys.exit(1 if refused else 0)


if __name__ == "__main__":
    main(sys.argv)
