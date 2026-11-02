#!/usr/bin/env python3
"""tools/rename.py: renames ra-jit to blaze throughout a checkout (#270).

usage: python3 tools/rename.py [--env] [--check] [--dry-run]

Run from anywhere inside a checkout of blaze, on a branch with nothing uncommitted. It rewrites
every tracked text file and `git mv`s every tracked path whose name says ra-jit, then prints the
places a rule may have made two names one (an identifier the file already had). It changes
nothing it has already changed, so a branch made before the rename runs it on itself and then
rebases: `python3 tools/rename.py && git commit -am 'rename.py' && git rebase origin/main`.

  --env      only the settings: `RA_JIT_<NAME>` becomes `BLAZE_<NAME>`, and nothing else
  --check    changes nothing; lists what still says ra-jit or cargo blaze, and fails when anything does
  --dry-run  changes nothing; says how many lines of which files it would change

The names (docs/rename.md has the reasons):

  crates/ra-jit            the crate blaze-engine, published under that name
  crates/ra-jit-<name>     the crate blaze-<name>; ra-jit-cache's library `ra_jit` is `blaze`
  ra-jit (the program)     blaze-engine, which `blaze` runs
  cargo blaze, cargo ra-jit  blaze: cargo-blaze and cargo-ra-jit are gone
  ra-jit-rustc             blaze-rustc
  RA_JIT_<NAME>            BLAZE_<NAME>
  ra-jit, ra_jit, RaJit    blaze, blaze, Blaze, in everything else
  rowan                    syntax-tree (crates/syntax-tree, `syntax_tree::`)
  ra-ap-rustc_abi, ...     rustc_abi, rustc_type_ir, rustc_next_trait_solver

CHANGELOG.md keeps its history, and this script and docs/rename.md the old names they map.
"""

import argparse
import os
import re
import subprocess
import sys

# Kept as they are: history and the map itself.
SKIPPED = {
    "CHANGELOG.md",
    "tools/rename.py",
    "docs/rename.md",
    # What blaze says of the old names, which it has to name.
    "crates/ra-jit/src/renamed.rs",
}
# A line that names an old name on purpose, as install.sh's clean-up of a blaze from before the
# rename does, says so with this in a comment, and is left as it is.
MARK = "rename: old name"

ENV = [(r"RA_JIT_(?=[A-Z0-9])", "BLAZE_")]

# The program the other binaries run, where a name means it rather than blaze as a whole.
ENGINE = "blaze-engine"
COMMANDS = (
    "cargo|std|cache|closure|mir|daemon|rustc-wrapper|cargo-answer|compile-time-deps|deps|"
    "borrowck|weaken|--check|--spare|--version|-V|build-script|proc-macro-wasm"
)
BIN_DIRS = (
    r"release|debug|dist|dev-rel|profiling|bin|shipped|unpacked|\$NAME|\$bin|\$unpacked|\$out/bin|\$\{bin\}|"
    r"\$PWD|\$RUNNER_TEMP/\w+"
)
END = r"(?=(?:\.exe)?(?![\w.-]))"

RULES = ENV + [
    # Other spellings of the settings' prefix and of the name.
    (r"RA_JIT", "BLAZE"),
    (r"RAJIT", "BLAZE"),
    (r"RA-JIT", "BLAZE"),
    (r"RA JIT", "BLAZE"),
    (r"RaJit", "Blaze"),
    (r"Ra[-_ ]jit", "Blaze"),
    # The repository's old name.
    (r"indexable-inc/ra-jit(?![\w-])", "indexable-inc/blaze"),
    # `cargo blaze` (before it `cargo ra-jit`) is gone: `blaze` is the command. The crate that
    # was published as cargo-blaze is published under its own name.
    (r'"(?:cargo|cargo-blaze|cargo-ra-jit)", "(?:blaze|ra-jit)"', '"blaze"'),
    (r'"cargo", \*\(\["(?:ra-jit|blaze)"\] if ([^\]\n]+?) else \[\]\)', r'*(["blaze"] if \1 else ["cargo"])'),
    (r'\["cargo"\] \+ \(\["(?:ra-jit|blaze)"\] if ([^\]\n]+?) else \[\]\)', r'(["blaze"] if \1 else ["cargo"])'),
    (r"\b(?:cargo-blaze blaze|cargo-ra-jit ra-jit)\b", "blaze"),
    (r"(\b(?:install|binstall)(?: --?[\w=+.$-]+)*) cargo-blaze\b", r"\1 " + ENGINE),
    (r"cargo_ra_jit", "blaze_cli"),
    (r"(?<![\w-])cargo[- ](?:ra-jit|blaze)(?![\w-])", "blaze"),
    # The engine's crate, its directory and its program.
    (r"crates/ra-jit(?![\w-])", "crates/" + ENGINE),
    (r"(-p|--package|--bin|--exclude)([ =])ra-jit(?![\w-])", r"\1\2" + ENGINE),
    (r'"(-p|--package|--bin|--exclude)", "ra-jit"', r'"\1", "' + ENGINE + '"'),
    (r'(?m)^(name|package) = "ra-jit"', r'\1 = "' + ENGINE + '"'),
    (r'(package = )"ra-jit"', r'\1"' + ENGINE + '"'),
    (r"(?m)^ra-jit = ", ENGINE + " = "),
    (r'"ra-jit\{', '"' + ENGINE + "{"),
    (r"(?<![\w.-])((?:" + BIN_DIRS + r")/)ra-jit" + END, r"\1" + ENGINE),
    (r'((?:"release"|"debug"|"dist"|"bin"|\bbin|"shipped"|"crates")(?:, | / ))"ra-jit"', r'\1"' + ENGINE + '"'),
    (r'(?<="BLAZE_BIN", )"ra-jit"', '"' + ENGINE + '"'),
    (r'(?<=\[)"ra-jit"(?=, )', '"' + ENGINE + '"'),
    (r'(?<=argv\[0\] == )"ra-jit"', '"' + ENGINE + '"'),
    (r'(?<=\borig [!=]= )"ra-jit"', '"' + ENGINE + '"'),
    (r'(?<=\bp\.name == )"ra-jit"', '"' + ENGINE + '"'),
    (r'(?<=if name == )"ra-jit"(?=:\n +return "daemon")', '"' + ENGINE + '"'),
    (r'(?m)^    "ra-jit": \[', '    "' + ENGINE + '": ['),
    (r'(?m)^(?=.*"ra-jit-\w)(.*?)"ra-jit"(?=[,\]\s)])', r'\1"' + ENGINE + '"'),
    (r"(?<![\w/.-])ra-jit(?= (?:" + COMMANDS + r")(?![\w-]))", ENGINE),
    # Everything else: the crates `ra-jit-<name>` and their paths, then the name itself.
    (r"ra-jit-", "blaze-"),
    (r"ra_jit_", "blaze_"),
    (r"rajit", "blaze"),
    (r"ra-jit", "blaze"),
    (r"ra_jit", "blaze"),
    (r"(?i)ra[-_ ]?jit", "blaze"),
]

# Forks renamed with it: rowan is syntax-tree, and the rustc crates lose rust-analyzer's
# `ra-ap-` (their own names stay). Not in what docs/upstream and docs/research quote of others,
# nor on a line that cites the upstream crate: one that says crates.io, or a name and its version.
FORKS_ELSEWHERE = ("docs/upstream/", "docs/research/")
RUSTC_FORKS = "rustc_abi|rustc_type_ir|rustc_next_trait_solver"
FORKS = [
    (r"crates/rowan(?![\w-])", "crates/syntax-tree"),
    (r"(?<![\w./-])rowan(?=::)", "syntax_tree"),
    (r"\b(use|crate) rowan\b", r"\1 syntax_tree"),
    (r"(?<![\w./-])rowan(?=@\d)", "syntax-tree"),
    (r'"rowan(?=[" ])', '"syntax-tree'),
    (r"(?m)^rowan(?=\s*=|\.)", "syntax-tree"),
    (r"(?<![\w./-])rowan(?![\w/.-])(?!`? \d)", "syntax-tree"),
    (r"extern crate ra_ap_(" + RUSTC_FORKS + r") as \1;", r"extern crate \1;"),
    (r"ra-ap-(" + RUSTC_FORKS + r")(?![\w-])(?!`? \d)", r"\1"),
    (r"ra_ap_(" + RUSTC_FORKS + r")(?!\w)", r"\1"),
]
UPSTREAM = re.compile(r"crates\.io")


def forks(text):
    return "\n".join(line if MARK in line or UPSTREAM.search(line) else rewrite(line, FORKS) for line in text.split("\n"))

# A path's new name: the engine's crate directory first, then the rules of text.
PATH_RULES = [(r"(^|/)crates/ra-jit(?=/)", r"\1crates/" + ENGINE), (r"(^|/)crates/rowan(?=/)", r"\1crates/syntax-tree")] + RULES

# Words a rule turns into another word a file may already have.
IDENTS = {
    "ra_jit": "blaze",
    "cargo_ra_jit": "blaze_cli",
    "RA_JIT": "BLAZE",
    "RaJit": "Blaze",
}


def git(*args, check=True):
    return subprocess.run(["git", *args], check=check, capture_output=True, text=True).stdout


def rewrite(text, rules):
    kept = {}
    lines = text.split("\n")
    for n, line in enumerate(lines):
        if MARK in line:
            mark = f"\x00line{n}\x00"
            kept[mark] = line
            lines[n] = mark
    text = "\n".join(lines)
    for pattern, new in rules:
        text = re.sub(pattern, new, text)
    for mark, token in kept.items():
        text = text.replace(mark, token)
    return text


def collisions(path, text):
    said = []
    for old, new in IDENTS.items():
        word = lambda w: re.compile(r"(?<![A-Za-z0-9_])" + re.escape(w) + r"(?![A-Za-z0-9_])")
        if word(old).search(text) and word(new).search(text):
            said.append(f"{path}: `{old}` becomes `{new}`, which it already has")
    return said


def check():
    out = git("grep", "-InP", r"(?i)ra[-_ ]?jit|(?<![\w-])cargo[- _]blaze", "--", ".", *[f":!{it}" for it in SKIPPED], check=False)
    lines = [it for it in out.splitlines() if MARK not in it]
    lines += [f"{it}: its path" for it in git("ls-files").splitlines() if re.search(r"(?i)ra[-_ ]?jit|(^|/)crates/rowan/", it)]
    old = git("grep", "-InP", r"(?<![\w./-])rowan(?![\w/.-])(?!`? \d)|ra[-_]ap[-_](" + RUSTC_FORKS + r")(?![\w-])(?!`? \d)", "--", ".",
              *[f":!{it}" for it in [*SKIPPED, *FORKS_ELSEWHERE]], check=False)
    lines += [it for it in old.splitlines() if MARK not in it and not UPSTREAM.search(it.split(":", 2)[2])]
    for line in lines:
        print(line)
    print(f"{len(lines)} lines still say an old name", file=sys.stderr)
    return 1 if lines else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--env", action="store_true")
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()
    os.chdir(git("rev-parse", "--show-toplevel").strip())
    if args.check:
        return check()
    if not args.dry_run and git("status", "--porcelain", "--untracked-files=no").strip():
        sys.exit("rename.py: commit or drop the changes of this checkout first")
    rules = ENV if args.env else RULES
    tracked = [it for it in git("ls-files", "-z").split("\0") if it]
    files = [it for it in tracked if it not in SKIPPED]

    # A file kept as it is still moves with its directory.
    moves = []
    if not args.env:
        for path in tracked:
            new = rewrite(path, PATH_RULES)
            if new != path:
                if os.path.lexists(new):
                    sys.exit(f"rename.py: {path} would become {new}, which is there already")
                moves.append((path, new))

    changed, warned = 0, []
    for path in files:
        if not os.path.isfile(path) or os.path.islink(path):
            continue
        with open(path, "rb") as f:
            raw = f.read()
        if b"\0" in raw[:8192]:
            continue
        try:
            text = raw.decode()
        except UnicodeDecodeError:
            continue
        new = rewrite(text, rules)
        if not args.env and not path.startswith(FORKS_ELSEWHERE):
            new = forks(new)
        if new == text:
            continue
        changed += 1
        if not args.env:
            warned += collisions(path, text)
        if args.dry_run:
            lines = sum(a != b for a, b in zip(text.splitlines(), new.splitlines()))
            print(f"{lines:6} {path}")
            continue
        with open(path, "w", newline="") as f:
            f.write(new)

    for old, new in moves:
        if args.dry_run:
            print(f"  move {old} -> {new}")
            continue
        os.makedirs(os.path.dirname(new) or ".", exist_ok=True)
        git("mv", old, new)
    for line in warned:
        print("check:", line, file=sys.stderr)
    print(f"rename.py: {changed} files rewritten, {len(moves)} moved", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
