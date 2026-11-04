#!/usr/bin/env python3
"""rustcsites.py: the places in crates/ that start rustc, rustdoc, cargo or rustup, or read
which one to start, counted per file against norustc/sites.json. One more in a file fails:
blaze is to run no rustc (docs/no-rustc.md), so no new place may start one, and a place that
goes lowers the count.

    rustcsites.py [--record]

--record writes the counts as they are, after a change that removed a place.
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


def places():
    found = {}
    for dirpath, dirnames, filenames in os.walk(os.path.join(ROOT, "crates")):
        # crates/harness is its own cargo workspace, not blaze's code (crates/harness/README.md).
        if os.path.relpath(dirpath, ROOT) == "crates":
            dirnames[:] = [it for it in dirnames if it != "harness"]
        dirnames[:] = sorted(it for it in dirnames if it != "target")
        for filename in sorted(filenames):
            if not filename.endswith(".rs"):
                continue
            path = os.path.join(dirpath, filename)
            lines = [n for n, line in enumerate(open(path, encoding="utf-8"), 1) if PLACE.search(line)]
            if lines:
                found[os.path.relpath(path, ROOT)] = lines
    return found


def main():
    found = places()
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
