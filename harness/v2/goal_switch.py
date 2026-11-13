#!/usr/bin/env python3
"""The owner's sequence on a hello-world with one test, in a fresh home: build, build, test, test,
test, build, build, test, test, build, each timed. A switch of goal that costs more than the same
goal again is the front end lowered anew (restore.md, "Sealed crates", F1): with one front end a
checkout (`blaze_lower::Lowered::relower`) a switch to a goal asked before costs about what the
same goal again costs when it walks. Each `test` must say its test ran and passed: an exit
status alone passes a test that was never run. Input: one JSON value
{"blaze": "/path/to/blaze", "work": "/private/tmp/dir", "label": "base"}. Prints one line: the
label, the load, each command's time (with `EXIT` or `NOT RUN` where it failed), then the
slowest switch to a goal asked before against the slowest repeat of a goal. Exits 1 when a
command failed or a test did not run."""
import json, os, shutil, subprocess, sys, time
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Input:
    blaze: str
    work: str
    label: str


RAW = json.loads(sys.argv[1])
assert set(RAW) == {"blaze", "work", "label"}, RAW
IN = Input(RAW["blaze"], RAW["work"], RAW["label"])
WORK = Path(IN.work)
HOME = WORK / "home"
shutil.rmtree(WORK, ignore_errors=True)
HOME.mkdir(parents=True)
ENV = {"HOME": os.environ["HOME"], "PATH": os.environ["PATH"], "BLAZE_HOME": str(HOME)}
PROJECT = WORK / "hello"
(PROJECT / "src").mkdir(parents=True)
(PROJECT / "Cargo.toml").write_text('[package]\nname = "hello"\nversion = "0.1.0"\nedition = "2024"\n')
(PROJECT / "src/main.rs").write_text(
    'fn add(a: u64) -> u64 { a + 1 }\nfn main() { println!("hello: {}", add(1)); }\n'
    "#[test]\nfn adds() { assert_eq!(add(1), 2); }\n"
)
# What `blaze test` says of the one test when it ran and passed.
RAN = "test adds ... ok"
GOALS = ("build", "build", "test", "test", "test", "build", "build", "test", "test", "build")
said: list[str] = []
times: list[float] = []
wrong = False
for goal in GOALS:
    # A test's own lines are what shows it ran: `-q` is for the builds alone.
    args = [IN.blaze, goal] + ([] if goal == "test" else ["-q"])
    start = time.perf_counter()
    ran = subprocess.run(args, cwd=PROJECT, env=ENV, capture_output=True, text=True, timeout=900)
    ms = (time.perf_counter() - start) * 1000
    times.append(ms)
    note = ""
    if ran.returncode != 0:
        note = f" EXIT {ran.returncode} {ran.stderr.strip()[-200:]}"
    elif goal == "test" and RAN not in ran.stdout + ran.stderr:
        note = f" NOT RUN (no `{RAN}` in what it said: {(ran.stdout + ran.stderr).strip()[-200:]!r})"
    wrong = wrong or bool(note)
    said.append(f"{goal} {ms:.0f} ms{note}")
# A switch: a goal other than the one before it, asked before (its first time builds it). A
# repeat: the goal of the command before it.
seen: set[str] = set()
switches: list[float] = []
repeats: list[float] = []
for at, goal in enumerate(GOALS):
    if at > 0 and goal == GOALS[at - 1]:
        repeats.append(times[at])
    elif goal in seen:
        switches.append(times[at])
    seen.add(goal)
print(
    f"{IN.label} | load {os.getloadavg()[0]:.0f} | "
    + ", ".join(said)
    + f" | slowest switch {max(switches):.0f} ms, slowest repeat {max(repeats):.0f} ms",
    flush=True,
)
subprocess.run([IN.blaze, "stop"], cwd=PROJECT, env=ENV, capture_output=True, timeout=300)
sys.exit(1 if wrong else 0)
