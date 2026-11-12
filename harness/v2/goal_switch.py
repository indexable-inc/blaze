#!/usr/bin/env python3
"""The owner's sequence on a hello-world with one test, in a fresh home: build, build, test, test,
test, build, build, test, test, build, each timed. A switch of goal that costs more than the same
goal again is the front end lowered anew (restore.md, F1). Input: one JSON value
{"blaze": "/path/to/blaze", "work": "/private/tmp/dir", "label": "base"}. Prints one line."""
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
said: list[str] = []
for goal in ("build", "build", "test", "test", "test", "build", "build", "test", "test", "build"):
    start = time.perf_counter()
    ran = subprocess.run([IN.blaze, goal, "-q"], cwd=PROJECT, env=ENV, capture_output=True, text=True, timeout=900)
    ms = (time.perf_counter() - start) * 1000
    said.append(f"{goal} {ms:.0f} ms" + ("" if ran.returncode == 0 else f" EXIT {ran.returncode} {ran.stderr.strip()[-200:]}"))
print(f"{IN.label} | load {os.getloadavg()[0]:.0f} | " + ", ".join(said), flush=True)
subprocess.run([IN.blaze, "stop"], cwd=PROJECT, env=ENV, capture_output=True, timeout=300)
