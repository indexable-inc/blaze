"""focused.py: named harnesses of ci.py, alone, against one build (focused.yaml).

    focused.py parts '{"harnesses": ["anonstatics.py", "worktrees.py --overlay"]}'
        CI's matrix for this machine, as ci.py's `parts` gives it: {"include": [{"backend",
        "part": "focused", "apt"}, ...]}, one entry per backend that runs any of them here.
    focused.py run '{"harnesses": ["anonstatics.py"], "backend": "llvm"}'
        each named harness that runs here under `backend`, as ci.py runs a part: with a daemon
        home of its own, every one even when one before it fails, then the times and a
        `failed:` line.

A name is a harness's label in ci.py (`ci.py list '{}'`), or the start of one up to a space, as
FAST and CORE name them: `worktrees.py` is all three of its runs, `worktrees.py --overlay` the
one. A name that is no harness's, or one whose harnesses run on no backend of this machine,
stops it with an error rather than run nothing. Harnesses run as the full tier's do
(BLAZE_CI_TIER=full): a known flaky step counts.

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
from typing import Any, Literal

from typed_input import fail, parse

SCRIPT = "focused.py"
type Backend = Literal["cranelift", "llvm"]


@dataclass(frozen=True)
class Parts:
    harnesses: tuple[str, ...]


@dataclass(frozen=True)
class Run:
    harnesses: tuple[str, ...]
    backend: Backend


def ci_for(backend: Backend) -> ModuleType:
    """ci.py, read for `backend`: its table puts BLAZE_BACKEND into some harnesses' arguments
    when it is imported, and the harnesses read it."""
    os.environ["BLAZE_BACKEND"] = backend
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    ci = importlib.import_module("ci")
    ci.check_tables()
    return ci


def named(ci: ModuleType, names: tuple[str, ...], backend: str) -> list[Any]:
    """The harnesses `names` picks that run here under `backend`, in ci.py's order; stops at a
    name that is no harness's."""
    if not names:
        fail(SCRIPT, "harnesses is empty: name at least one (`ci.py list '{}'` lists them)")
    labels = [(harness, ci.label(harness)) for harness in ci.HARNESSES]
    picked: list[Any] = []
    for name in names:
        matched = [h for h, label in labels if label == name or label.startswith(name + " ")]
        if not matched:
            fail(SCRIPT, f"no harness of ci.py is {name!r}: `ci.py list '{{}}'` lists them")
    for harness, label in labels:
        if any(label == name or label.startswith(name + " ") for name in names) and harness[3](backend):
            picked.append(harness)
    return picked


def runs_somewhere_here(ci: ModuleType, names: tuple[str, ...]) -> None:
    for name in names:
        if not any(named(ci, (name,), backend) for backend in ci.BACKENDS):
            fail(SCRIPT, f"{name!r} runs on no backend of {ci.SYSTEM} {ci.ARCH}")


def parts(it: Parts) -> None:
    ci = ci_for("cranelift")
    runs_somewhere_here(ci, it.harnesses)
    include: list[dict[str, str]] = []
    for backend in ci.BACKENDS:
        if chosen := named(ci, it.harnesses, backend):
            apt = (
                sorted({ci.APT[h[1][0]] for h in chosen if h[1][0] in ci.APT}) if ci.SYSTEM == "linux" else []
            )
            include.append({"backend": backend, "part": "focused", "apt": " ".join(apt)})
    print(json.dumps({"include": include}))


def run(it: Run) -> None:
    ci = ci_for(it.backend)
    runs_somewhere_here(ci, it.harnesses)
    chosen = named(ci, it.harnesses, it.backend)
    if not chosen:
        fail(SCRIPT, f"none of {list(it.harnesses)} runs under {it.backend} on {ci.SYSTEM} {ci.ARCH}")
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
