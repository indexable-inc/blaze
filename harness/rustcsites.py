#!/usr/bin/env python3
"""rustcsites.py: the places in crates/ that start rustc, rustdoc, cargo or rustup, or read
which one to start, counted per file against norustc/sites.json. One more in a file fails:
blaze is to run no rustc (docs/no-rustc.md), so no new place may start one, and a place that
goes lowers the count.

    rustcsites.py [--record]

--record writes the counts as they are, after a change that removed a place.

Three things look like a place and are not one, and are told apart here, each precisely:

- `CARGO_BIN_EXE_<name>`, the variable cargo gives a test with the path of the package's
  executable `<name>`: it names that program, not cargo, so only the `<name>` is matched
  (`Command::new(env!("CARGO_BIN_EXE_blaze"))` starts blaze; `..._rustc` would still count).
- The text of a raw string that spans lines: a test's fixture of Rust source (`r#"..."#`),
  which is data a test compiles or checks, never run. No command name spans lines, so a raw
  string on one line is still matched.
- A line under a comment with an `// asks blaze: <why>` line: a build script asking its `$RUSTC`, which
  blaze sets to its own stand-in for rustc (blaze-cargo `serve/unit/script.rs`, the `rustc` of
  its tools; docs/no-rustc.md, end state 3), so blaze answers and no rustc starts. The comment
  must say why; such places are listed on every run so they stay in view.
"""
import json, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
SITES = os.path.join(HERE, "norustc", "sites.json")
PLACE = re.compile(
    r"Command::new\([^)]*(rustc|rustdoc|cargo|rustup)"
    r"|toolchain::command\([^,]*(rustc|rustdoc|cargo|rustup)"
    r"|var(_os)?\(\"(RUSTC|RUSTDOC|CARGO)\"\)"
    r"|MetadataCommand::new\(\)"
    r"|\.tool\(Tool::(Rustc|Cargo)",
    re.IGNORECASE,
)
# The prefix of the variable naming a package's executable to its tests: not cargo.
BIN_EXE = re.compile(r"CARGO_BIN_EXE_")
# A raw string literal: `r`, `br` or `cr`, then n `#`s, a quote, the text, a quote and n `#`s.
RAW = re.compile(r'(?<![\w"\'])[bc]?r(#*)"(.*?)"\1', re.DOTALL)
ASKS_BLAZE = re.compile(r"^\s*// asks blaze: \S")


def code_of(text):
    """`text` with the text of each raw string that spans lines blanked, its lines kept."""
    def blank(match):
        body = match.group(2)
        if "\n" not in body:
            return match.group(0)
        start = match.start(2) - match.start(0)
        whole = match.group(0)
        return whole[:start] + re.sub(r"[^\n]", " ", body) + whole[start + len(body):]

    return RAW.sub(blank, text)


def asks_blaze(lines, index):
    """Whether the run of `//` comment lines just above `lines[index]` has an `asks blaze:` line."""
    above = index - 1
    while above >= 0 and lines[above].lstrip().startswith("//"):
        if ASKS_BLAZE.match(lines[above]):
            return True
        above -= 1
    return False


def places():
    """The places that start or name rustc, rustdoc, cargo or rustup, by file, and the places
    marked as asking blaze, by file."""
    found, asks = {}, {}
    for dirpath, dirnames, filenames in os.walk(os.path.join(ROOT, "crates")):
        # crates/harness is its own cargo workspace, not blaze's code (crates/harness/README.md).
        if os.path.relpath(dirpath, ROOT) == "crates":
            dirnames[:] = [it for it in dirnames if it != "harness"]
        dirnames[:] = sorted(it for it in dirnames if it != "target")
        for filename in sorted(filenames):
            if not filename.endswith(".rs"):
                continue
            path = os.path.join(dirpath, filename)
            with open(path, encoding="utf-8") as file:
                text = file.read()
            raw_lines = text.split("\n")
            lines, asked = [], []
            for n, line in enumerate(code_of(text).split("\n"), 1):
                if not PLACE.search(BIN_EXE.sub("", line)):
                    continue
                if asks_blaze(raw_lines, n - 1):
                    asked.append(n)
                else:
                    lines.append(n)
            rel = os.path.relpath(path, ROOT)
            if lines:
                found[rel] = lines
            if asked:
                asks[rel] = asked
    return found, asks


def main():
    found, asks = places()
    for path, lines in asks.items():
        print(f"{path}: {len(lines)} place(s) ask blaze as `rustc`, not rustc (lines {', '.join(map(str, lines))})")
    counts = {path: len(lines) for path, lines in found.items()}
    if "--record" in sys.argv:
        with open(SITES, "w") as out:
            json.dump(counts, out, indent=1, sort_keys=True)
            out.write("\n")
        print(f"recorded {sum(counts.values())} places in {len(counts)} files")
        return
    recorded = json.load(open(SITES))
    more = []
    for path, lines in found.items():
        was = recorded.get(path, 0)
        if len(lines) > was:
            more.append(f"{path}: {len(lines)} places, {was} recorded (lines {', '.join(map(str, lines))})")
        elif len(lines) < was:
            print(f"{path}: {len(lines)} places, {was} recorded; rustcsites.py --record lowers it")
    for path, was in recorded.items():
        if path not in found:
            print(f"{path}: no place, {was} recorded; rustcsites.py --record lowers it")
    for line in more:
        print(f"NEW: {line}")
    if more:
        print("a new place starts rustc, rustdoc, cargo or rustup: docs/no-rustc.md says why that is refused")
        sys.exit(1)
    print(f"{sum(counts.values())} places start rustc, rustdoc, cargo or rustup, none new")


if __name__ == "__main__":
    main()
