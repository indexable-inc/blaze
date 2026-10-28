#!/usr/bin/env python3
"""cold.py: a cold `blaze build` of each project by each arm, alternating, and what its worker
said of the back end: functions compiled, what the memo of this process found, the store.

    cold.py OUT_DIR ROUNDS ARM=BLAZE_ENGINE... -- NAME=CHECKOUT...

Each build starts from nothing (home, store and target directory of its own), with the standard
library's unit fetched from what the arm's `blaze-engine std` made once, as bench.py's cold phase
does. Everything the worker logged is kept as OUT_DIR/<name>-<arm>-<round>.log.
"""
import json, os, re, shutil, subprocess, sys, time

HARNESS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../../../harness")
sys.path.insert(0, os.path.realpath(HARNESS))

out, rounds = sys.argv[1], int(sys.argv[2])
at = sys.argv.index("--")
arms = dict(it.split("=", 1) for it in sys.argv[3:at])
projects = dict(it.split("=", 1) for it in sys.argv[at + 1:])
work = os.path.join(out, "work")
os.makedirs(work, exist_ok=True)

LINES = {
    "compiled": r"^compiled \d+ of \d+ fns.*$",
    "memo": r"^memo: .*$",
    "store": r"^store: .*$",
    "steps": r"^steps: .*$",
}


def env_of(blaze, home):
    env = dict(os.environ, BLAZE_HOME=home, BLAZE_CACHE=os.path.join(home, "store"), CARGO_TERM_COLOR="never",
               RUST_BACKTRACE="0", BLAZE_STATS="1", CARGO_UNSTABLE_BUILD_DIR_NEW_LAYOUT="false")
    env["PATH"] = os.path.dirname(blaze) + os.pathsep + env["PATH"]
    for name in ("RUSTFLAGS", "CARGO_TARGET_DIR", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_TARGET"):
        env.pop(name, None)
    return env


def std_of(arm, blaze):
    remote = os.path.join(work, f"std-{arm}")
    if not os.path.isdir(remote):
        env = env_of(blaze, os.path.join(work, f"home-std-{arm}"))
        sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True, env=env, check=True).stdout.strip()
        subprocess.run([blaze, "std", remote, sysroot], env=env, check=True, capture_output=True)
    return remote


def status(env, ws):
    return subprocess.run(["blaze", "status"], cwd=ws, env=env, capture_output=True, text=True).stdout


def cold(name, source, arm, blaze, turn):
    home = os.path.join(work, f"home-{name}-{arm}")
    ws = os.path.join(work, f"{name}-{arm}")
    for path in (home, ws):
        shutil.rmtree(path, ignore_errors=True)
    shutil.copytree(source, ws, symlinks=True, ignore=lambda _, names: [n for n in names if n in ("target", ".git")])
    env = dict(env_of(blaze, home), BLAZE_STD_FROM=std_of(arm, blaze))
    began = time.monotonic()
    built = subprocess.run(["blaze", "build"], cwd=ws, env=env, capture_output=True, text=True)
    took = time.monotonic() - began
    assert built.returncode == 0, built.stderr[-3000:]
    while True:
        said = status(env, ws)
        states = re.findall(rf"^    {re.escape(os.path.realpath(ws))}: (.*)$", said, re.M)
        if states and not any(s.startswith("loading") for s in states):
            break
        time.sleep(0.25)
    ready = time.monotonic() - began
    logs = re.findall(r"worker: pid \d+, .*, log (.*)$", said, re.M)
    text = "".join(open(log, errors="replace").read() for log in logs if os.path.exists(log))
    open(os.path.join(out, f"{name}-{arm}-{turn}.log"), "w").write(built.stderr + "\n--- workers\n" + text)
    subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True)
    row = dict(project=name, arm=arm, round=turn, build_s=round(took, 2), ready_s=round(ready, 2), states=states)
    for key, pattern in LINES.items():
        row[key] = re.findall(pattern, text, re.M)
    print(json.dumps(row), flush=True)
    return row


rows = []
for turn in range(rounds):
    for name, source in projects.items():
        order = list(arms.items()) if turn % 2 == 0 else list(arms.items())[::-1]
        for arm, blaze in order:
            rows.append(cold(name, source, arm, blaze, turn))
json.dump(rows, open(os.path.join(out, "cold.json"), "w"), indent=1)
