"""Const-eval speed: each case built and run with and without its constant, arms A (main) and B (#1473) alternating."""

import json
import os
import shutil
import subprocess
import sys
import time

ROOT = "/root/cb"
CASES = {"ce_sha256": True, "ce_toml": True, "ce_inflate": True, "const_heap": False}
TIMEOUT = 3600
ROUNDS = [["A", "B"], ["B", "A"]]

out = open(f"{ROOT}/raw.jsonl", "a")
for rnd, arms in enumerate(ROUNDS, 1):
    for case, has_twin in CASES.items():
        for arm in arms:
            for variant in (["const", "rt"] if has_twin else ["const"]):
                tag = f"r{rnd}-{arm}-{case}-{variant}"
                work = f"{ROOT}/work/{tag}"
                home = f"{ROOT}/home/{tag}"
                shutil.rmtree(work, ignore_errors=True)
                shutil.rmtree(home, ignore_errors=True)
                shutil.copytree(f"{ROOT}/{arm}/harness/smoke/{case}", work, ignore=shutil.ignore_patterns("target"))
                env = dict(os.environ, BLAZE_HOME=home, CARGO_TERM_COLOR="never", RUST_BACKTRACE="0")
                argv = [f"{ROOT}/{arm}/target/release/blaze", "run", "--release"]
                if variant == "rt":
                    argv.append("--no-default-features")
                start = time.monotonic()
                try:
                    done = subprocess.run(argv, cwd=work, env=env, capture_output=True, text=True, timeout=TIMEOUT)
                    code, stdout, stderr = done.returncode, done.stdout, done.stderr
                except subprocess.TimeoutExpired as e:
                    code, stdout, stderr = None, e.stdout or "", e.stderr or ""
                    stdout = stdout.decode() if isinstance(stdout, bytes) else stdout
                    stderr = stderr.decode() if isinstance(stderr, bytes) else stderr
                secs = time.monotonic() - start
                subprocess.run([argv[0], "stop"], cwd=work, env=env, capture_output=True, timeout=120)
                expected = open(f"{work}/expected.txt").read()
                row = dict(round=rnd, arm=arm, case=case, variant=variant, seconds=round(secs, 1), exit=code,
                           correct=stdout == expected, stdout=stdout[-400:], stderr=stderr[-1500:])
                out.write(json.dumps(row) + "\n")
                out.flush()
                print(tag, row["seconds"], code, row["correct"], flush=True)
                shutil.rmtree(home, ignore_errors=True)
print("DONE", flush=True)
