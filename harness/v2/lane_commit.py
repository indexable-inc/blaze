#!/usr/bin/env python3
"""A commit of one lane's work out of a checkout several lanes write in (how-to H12): whole
files that are the lane's alone, and, from files other lanes also have uncommitted lines in,
only the hunks whose added lines match the lane's pattern. Made in an index of its own on top
of `base`, so no branch moves, nothing is staged in the checkout and no other lane's line is
taken. Input: one JSON value

    {"repo": "/Volumes/Projects/blaze-int-1112", "base": "origin/int-1112", "message": "...",
     "whole": ["crates/x/src/mine.rs", "docs/mine.md"],
     "hunks": {"crates/x/src/shared.rs": "ahead|Ahead"},
     "push": "onsave-1112"}

`push` (optional) force-pushes the commit to that branch of `origin`: a scratch branch to build
before the work goes to the shared one. Prints, for each shared file, how many hunks were kept
and left out, then the commit.

Why not `git commit -- <file>`: it commits the file as the checkout has it, with the other
lane's half-written hunk (row onsave-1112-1). Why the hunks are renumbered: a hunk with no
context is placed by its line in the new file, which the hunks left out had shifted. A kept
hunk that removes a line is refused: say which lines are yours by adding them."""
import json, os, re, subprocess, sys, tempfile
from dataclasses import dataclass, field


@dataclass(frozen=True)
class Input:
    repo: str
    base: str
    message: str
    whole: tuple[str, ...] = ()
    hunks: dict[str, str] = field(default_factory=dict)
    push: str | None = None


def main() -> None:
    raw = json.loads(sys.argv[1])
    assert set(raw) <= set(Input.__dataclass_fields__), sorted(raw)
    ask = Input(**{**raw, "whole": tuple(raw.get("whole", ()))})
    with tempfile.TemporaryDirectory() as tmp:
        env = dict(os.environ, GIT_INDEX_FILE=os.path.join(tmp, "index"))

        def git(*args: str, stdin: str | None = None) -> str:
            done = subprocess.run(["git", "-C", ask.repo, *args], input=stdin, capture_output=True, text=True, env=env)
            if done.returncode != 0:
                sys.exit(f"lane_commit.py: git {' '.join(args)}: {done.stderr.strip()}")
            return done.stdout

        git("read-tree", ask.base)
        if ask.whole:
            git("add", "--", *ask.whole)
        for path, pattern in ask.hunks.items():
            mine = re.compile(pattern)
            head, *hunks = re.split(r"(?m)^(?=@@ )", git("diff", "--no-ext-diff", "-U0", ask.base, "--", path))
            added, kept = 0, []
            for hunk in hunks:
                lines = hunk.splitlines(keepends=True)
                if not any(mine.search(line) for line in lines[1:] if line.startswith("+")):
                    continue
                at = re.match(r"@@ -(\d+)(?:,(\d+))? \+\d+(?:,(\d+))? @@(.*)", lines[0], re.S)
                if at is None or at.group(2) != "0":
                    sys.exit(f"lane_commit.py: {path}: a hunk matching `{pattern}` removes lines, so whose it is cannot be told:\n{hunk}")
                old, count = int(at.group(1)), int(at.group(3) or 1)
                kept.append(f"@@ -{old},0 +{old + 1 + added},{count} @@{at.group(4)}" + "".join(lines[1:]))
                added += count
            print(f"{path}: {len(kept)} hunks kept, {len(hunks) - len(kept)} left out", file=sys.stderr)
            if kept:
                git("apply", "--cached", "--unidiff-zero", "-", stdin=head + "".join(kept))
        commit = git("commit-tree", git("write-tree").strip(), "-p", ask.base, "-m", ask.message).strip()
        if ask.push:
            git("push", "-q", "-f", "origin", f"{commit}:refs/heads/{ask.push}")
        print(commit)


main()
