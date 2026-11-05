#!/usr/bin/env python3
"""imports.py: every script of the harness imports, each in an interpreter of its own.

usage: imports.py

A script that is run by hand, or only by a tier that runs seldom, can break unnoticed (a name
it imports from another script that no longer has it); this finds that in seconds. Importing
a script runs no test: what it does is under `if __name__ == "__main__":`.

Python 3, stdlib only.
"""
import concurrent.futures
import glob
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))


def imported(name):
    try:
        ran = subprocess.run([sys.executable, "-c", f"import {name}"], cwd=HERE, stdin=subprocess.DEVNULL,
                             capture_output=True, text=True, timeout=120)
    except subprocess.TimeoutExpired:
        return name, None, "still importing after 120 s"
    return name, ran.returncode, (ran.stdout + ran.stderr).strip()


def main():
    names = sorted(os.path.splitext(os.path.basename(path))[0] for path in glob.glob(os.path.join(HERE, "*.py")))
    with concurrent.futures.ThreadPoolExecutor(os.cpu_count() or 4) as pool:
        results = list(pool.map(imported, names))
    wrong = [(name, said) for name, status, said in results if status != 0]
    for name, said in wrong:
        print(f"{name}.py:\n{said[-2000:]}\n")
    print(f"imports: {len(names) - len(wrong)} of {len(names)} harness scripts import")
    sys.exit(bool(wrong))


if __name__ == "__main__":
    main()
