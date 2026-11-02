"""report.py: the one shape of a v2 proof script's result.

A script makes a `Report`, adds an `Outcome` for each thing it checked, and ends with
`finish()`, which writes the result to the script's `out` (JSON) and exits:

    pass        what was checked holds
    fail        it does not: a finding about the compiler (a wrong program, a stale result,
                a budget missed, a build that is not byte for byte the same)
    unmeasured  the engine has no answer to it (`engine.Unmeasurable`): allowed for v1, whose
                run sets the bar; a failure for v2, which has to measure everything
    error       the harness could not check it (`engine.Failed`: a command that timed out,
                an answer of the engine it could not read)

The exit status is 1 when anything failed or erred, or the engine is v2 and anything was
unmeasured; else 0. `proof.py` reads the results of every script into one report.

The file:

    {"script": "oracle.py", "engine": {...}, "host": {...}, "commit": "...", "started": "...",
     "ended": "...", "outcomes": [{"name", "verdict", "detail", "numbers"}], "counts": {...},
     "extra": {...}}
"""

import json
import os
import platform
import subprocess
import sys
import time
from collections import Counter
from collections.abc import Callable
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Literal, NoReturn

from engine import ROOT, EngineSpec, Failed, Unmeasurable

type Verdict = Literal["pass", "fail", "unmeasured", "error"]
type Number = int | float | str | bool | None


@dataclass(frozen=True)
class Outcome:
    name: str
    verdict: Verdict
    detail: str = ""
    numbers: dict[str, Number] = field(default_factory=dict[str, Number])


def commit() -> str | None:
    ran = subprocess.run(["git", "-C", str(ROOT), "rev-parse", "HEAD"], capture_output=True, text=True)
    return ran.stdout.strip() or None


def host() -> dict[str, Number]:
    """What the run was on."""
    cores = len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count()
    memory_gib: float | None = None
    try:
        with open("/proc/meminfo") as f:
            kb = int(next(line for line in f if line.startswith("MemTotal:")).split()[1])
        memory_gib = round(kb / 2**20, 1)
    except (OSError, StopIteration, ValueError):
        if sys.platform == "darwin":
            out = subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True, text=True).stdout.strip()
            memory_gib = round(int(out) / 2**30, 1) if out.isdigit() else None
    return dict(os=platform.system(), arch=platform.machine(), kernel=platform.release(), cores=cores,
                machine_cores=os.cpu_count(), memory_gib=memory_gib, name=os.environ.get("BLAZE_BENCH_MACHINE"))


def now() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


class Report:
    def __init__(self, script: str, engine: EngineSpec, out: str):
        self.script, self.engine = script, engine
        self.out = Path(out) if out else None
        self.started = now()
        self.outcomes: list[Outcome] = []
        self.extra: dict[str, object] = {}

    def add(self, name: str, verdict: Verdict, detail: str = "", **numbers: Number) -> Outcome:
        outcome = Outcome(name, verdict, detail, numbers)
        self.outcomes.append(outcome)
        shown = "  ".join(f"{k}={v}" for k, v in numbers.items())
        first = detail.splitlines()[0][:220] if detail else ""
        print(f"{verdict.upper():10} {name}  {shown}  {first}".rstrip(), flush=True)
        return outcome

    def guarded[T](self, name: str, fn: Callable[[], T]) -> T | None:
        """`fn()`, or None with an outcome for `name` when it could not be done: unmeasured for
        an engine without the answer, error for the harness or an interface v2 lacks."""
        try:
            return fn()
        except Unmeasurable as e:
            self.add(name, "unmeasured", str(e))
        except Failed as e:
            self.add(name, "error", str(e))
        return None

    def failing(self) -> list[Outcome]:
        bad: set[Verdict] = {"fail", "error"} | ({"unmeasured"} if self.engine.kind == "v2" else set())
        return [o for o in self.outcomes if o.verdict in bad]

    def finish(self) -> NoReturn:
        counts = Counter(o.verdict for o in self.outcomes)
        result = dict(script=self.script, engine=asdict(self.engine), host=host(), commit=commit(),
                      started=self.started, ended=now(), outcomes=[asdict(o) for o in self.outcomes],
                      counts=dict(counts), extra=self.extra)
        if self.out:
            self.out.parent.mkdir(parents=True, exist_ok=True)
            self.out.write_text(json.dumps(result, indent=1) + "\n")
        bad = self.failing()
        print(f"{self.script}: " + ", ".join(f"{n} {v}" for v, n in sorted(counts.items()))
              + (f"; result {self.out}" if self.out else ""), flush=True)
        sys.exit(1 if bad else 0)
