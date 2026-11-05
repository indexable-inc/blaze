"""focused.py: named harnesses of ci.py, alone, against one build (focused.yaml).

    focused.py parts '{"harnesses": ["anonstatics.py", "coldbuild.py statics"]}'
        CI's matrix for this machine, as ci.py's `parts` gives it: {"include": [{"part":
        "focused", "apt"}]}.
    focused.py run '{"harnesses": ["anonstatics.py"]}'
        each named harness that runs here, as ci.py runs a part: with a daemon home of its own,
        every one even when one before it fails, then the times and a `failed:` line.

A name is a harness's label in ci.py (`ci.py list '{}'`), or the start of one up to a space, as
FAST and CORE name them: `coldbuild.py` is both of its runs, `coldbuild.py statics` the one. A
name that is no harness's, or one whose harnesses run nowhere on this machine, stops it with an
error rather than run nothing. Harnesses run as the full tier's do (BLAZE_CI_TIER=full): a known
flaky step counts. There is no backend to choose: v2 picks it by the profile (ci.py).

This is how a change runs only the regressions it can affect, before it lands or after, in place
of the whole train: what Astral's fork had a model pick on each pull request
(astral-experiments/blaze#58, Charlie Marsh), here named by whoever makes the change.

The JSON is inline, a file's path, or `-` for stdin. Python 3.13, from the workspace at the
repository's root: `uv run --frozen python harness/focused.py ...`. BLAZE_BIN is the blaze the
harnesses run, as for ci.py.
"""

import importlib
import json
import os
import sys
import time
from dataclasses import dataclass
from types import ModuleType
from typing import Any

from typed_input import fail, parse

SCRIPT = "focused.py"


@dataclass(frozen=True)
class Parts:
    harnesses: tuple[str, ...]


@dataclass(frozen=True)
class Run:
    harnesses: tuple[str, ...]


def ci_module() -> ModuleType:
    """ci.py, with its tables checked."""
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    ci = importlib.import_module("ci")
    ci.check_tables()
    return ci


def named(ci: ModuleType, names: tuple[str, ...]) -> list[Any]:
    """The harnesses `names` picks that run here, in ci.py's order; stops at a name that is no
    harness's, or one none of whose harnesses runs here."""
    if not names:
        fail(SCRIPT, "harnesses is empty: name at least one (`ci.py list '{}'` lists them)")
    labels = [(harness, ci.label(harness)) for harness in ci.HARNESSES]
    for name in names:
        matched = [h for h, label in labels if label == name or label.startswith(name + " ")]
        if not matched:
            fail(SCRIPT, f"no harness of ci.py is {name!r}: `ci.py list '{{}}'` lists them")
        if not any(ci.here(h[3]) for h in matched):
            fail(SCRIPT, f"{name!r} runs nowhere on {ci.SYSTEM} {ci.ARCH}")
    return [
        harness
        for harness, label in labels
        if any(label == name or label.startswith(name + " ") for name in names) and ci.here(harness[3])
    ]


def parts(it: Parts) -> None:
    ci = ci_module()
    chosen = named(ci, it.harnesses)
    apt = sorted({ci.APT[h[1][0]] for h in chosen if h[1][0] in ci.APT}) if ci.SYSTEM == "linux" else []
    print(json.dumps({"include": [{"part": "focused", "apt": " ".join(apt)}]}))


def run(it: Run) -> None:
    ci = ci_module()
    chosen = named(ci, it.harnesses)
    os.environ["BLAZE_CI_TIER"] = "full"
    failed: list[str] = []
    times: list[tuple[float, str]] = []
    for harness in chosen:
        _, args, env, _, _ = harness
        what = ci.label(harness)
        print(f"::group::{what}", flush=True)
        start = time.monotonic()
        status = ci.run_one(args, env)
        times.append((time.monotonic() - start, what))
        print("::endgroup::", flush=True)
        if status:
            failed.append(what)
            print(f"::error::{what} failed (exit status {status})", flush=True)
    for seconds, what in times:
        print(f"{seconds:7.0f} s  {what}")
    print(f"failed: {', '.join(failed) or 'none'}")
    sys.exit(1 if failed else 0)


def main() -> None:
    match sys.argv[1:]:
        case ["parts", text]:
            parts(parse(Parts, text, SCRIPT))
        case ["run", text]:
            run(parse(Run, text, SCRIPT))
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} parts|run JSON (see the top of the file)")


if __name__ == "__main__":
    main()
