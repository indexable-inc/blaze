#!/usr/bin/env python3
"""Tally the refusals inventory.py collected: distinct functions per project, by reason.

    tally.py OUT [--examples]

A reason is the innermost one of the chain (`callee `f`: unsupported: X` counts as X), with
backticked names and hashes taken out; `root` counts only functions refused for themselves,
not for a callee.
"""
import collections, glob, os, re, sys


def innermost(error):
    # "callee `a`: callee `b`: unsupported: X" -> "unsupported: X"
    while True:
        m = re.match(r"(?:unsupported: )?callee `[^`]*`: (.*)$", error)
        if not m:
            return error
        error = m.group(1)


def normal(why):
    why = re.sub(r"`[^`]*`", "`…`", why)
    why = re.sub(r"\b[0-9a-f]{8,}\b", "…", why)
    why = re.sub(r"\b\d+\b", "N", why)
    return why[:120]


def read(path):
    funcs = {}
    try:
        text = open(path, errors="replace").read()
    except OSError:
        return funcs
    for line in text.splitlines():
        m = re.match(r"(.*?): ((?:unsupported|frontend|backend): .*|.*)$", line)
        if m:
            funcs.setdefault(m.group(1), m.group(2))
    return funcs


def main():
    out = sys.argv[1]
    examples = "--examples" in sys.argv
    sources = {"std": f"{out}/std.skipped"}
    for d in sorted(glob.glob(f"{out}/*/skipped")):
        sources[os.path.basename(os.path.dirname(d))] = d
    total = collections.Counter()
    std = read(sources["std"])
    for name, path in sources.items():
        funcs = read(path)
        if name != "std":
            funcs = {f: e for f, e in funcs.items() if f not in std}
        if "--no-generic" in sys.argv:
            funcs = {f: e for f, e in funcs.items() if "no rustc symbol" not in e}
        reasons = collections.Counter()
        roots = collections.Counter()
        ex = {}
        for func, error in funcs.items():
            why = normal(innermost(error))
            reasons[why] += 1
            ex.setdefault(why, func)
            if not error.startswith(("callee", "unsupported: callee")):
                roots[why] += 1
        total.update(reasons)
        print(f"== {name}: {len(funcs)} distinct functions refused" + (" beyond the std unit" if name != "std" else ""))
        for why, n in reasons.most_common(40):
            line = f"  {n:6d} (root {roots[why]:5d})  {why}"
            if examples:
                line += f"   e.g. {ex[why][:90]}"
            print(line)
    print("== all projects but std")


if __name__ == "__main__":
    main()
