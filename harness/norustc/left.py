#!/usr/bin/env python3
"""Why functions were left to rustc in one counting run of count.py.

    left.py NAME

Reads the workers' logs under the run's RA_JIT_HOME and the RA_JIT_SKIPPED file.
"""
import glob
import os
import re
import sys
from collections import Counter

L = os.path.expanduser(os.environ.get("NR_ROOT", "~/no-rustc"))


def main():
    name = sys.argv[1]
    logs = glob.glob(f"{L}/home/{name}/**/log", recursive=True)
    reasons = Counter()
    funcs = set()
    for path in logs + [f"{L}/logs/{name}.skipped"]:
        try:
            text = open(path, errors="replace").read()
        except OSError:
            continue
        for line in text.splitlines():
            line = line.strip()
            # "refused: " in a build with nothing of rustc's, "left to rustc: " in one with it.
            prefix = next((it for it in ("left to rustc: ", "refused: ") if line.startswith(it)), None)
            if prefix:
                line = line[len(prefix):]
            elif not path.endswith(".skipped"):
                continue
            m = re.match(r"(.*?): (unsupported: .*|.*)$", line)
            if not m:
                continue
            func, why = m.groups()
            why = re.sub(r"`[^`]*`", "`…`", why)
            why = re.sub(r"\b[0-9a-f]{8,}\b", "…", why)[:110]
            if (func, why) not in funcs:
                funcs.add((func, why))
                reasons[why] += 1
    compiled = [tuple(map(int, m)) for path in logs
                for m in re.findall(r"compiled (\d+) of (\d+) fns", open(path, errors="replace").read())]
    print(f"{name}: {len(funcs)} distinct functions left to rustc; builds compiled {sum(a for a, _ in compiled)} "
          f"of {sum(b for _, b in compiled)} fns over {len(compiled)} builds")
    for why, n in reasons.most_common(25):
        print(f"  {n:6d}  {why}")


if __name__ == "__main__":
    main()
