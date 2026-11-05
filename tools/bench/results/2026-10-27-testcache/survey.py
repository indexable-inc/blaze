#!/usr/bin/env python3
"""Lane testcache: what hermetic-by-default tests break on the bench apps, and how many tests a
warm store skips. Runs in the lane's VM: nix develop /root/tc/src -c python3 /root/tc/survey.py [names]"""
import json, os, re, shutil, subprocess, sys, time

sys.path.insert(0, "/root/tc/src/harness")
import blaze_cli  # noqa: E402

BIN = "/root/tc/target/release"
APPS = "/root/apps"
PICK = ["ripgrep", "fd", "just", "bat", "hyperfine", "tokei", "axum", "cargo", "rust-analyzer", "nushell", "starship"]
RAN = re.compile(r"^test (\S+) \.\.\. (ok|ignored|FAILED)", re.M)
DENIED = re.compile(r"^(?:error: )?(\S+) fails: the sandbox denied it (.*?); tests are hermetic", re.M)
DENIED_ALL = re.compile(r"the tests of (\S+) fail: the sandbox denied (.*?) to a thread", re.M)
CACHED = re.compile(r"Cached (\d+) tests? from")


def run(ws, env, args, log):
    start = time.monotonic()
    out = subprocess.run(["blaze", "test", *args, "--no-fail-fast"], cwd=ws, env=env,
                         capture_output=True, text=True, timeout=3600)
    took = time.monotonic() - start
    said = out.stdout + out.stderr
    open(log, "w").write(said)
    ran = RAN.findall(said)
    cached = sum(int(it) for it in CACHED.findall(said))
    return dict(code=out.returncode, secs=round(took, 1), ran=len(ran),
                failed=sorted(n for n, e in ran if e == "FAILED"), cached=cached,
                denied=DENIED.findall(said), denied_all=DENIED_ALL.findall(said),
                served="[blaze]" in said)


def main():
    names = sys.argv[1:] or PICK
    os.makedirs(APPS, exist_ok=True)
    path = f"{APPS}/survey.json"
    results = json.load(open(path)) if os.path.exists(path) else {}
    for name in names:
        if name in results:
            continue
        spec = blaze_cli.PROJECTS[name]
        ws = os.path.join(APPS, name)
        if not os.path.isdir(ws):
            subprocess.run(["git", "clone", "-q", "--depth", "1", "--branch", spec["tag"],
                            f"https://github.com/{spec['repo']}", ws], check=True)
        env = dict(os.environ, PATH=f"{BIN}:{os.environ['PATH']}", BLAZE_HOME=f"/tmp/rjh-{name}",
                   BLAZE_CACHE=f"{APPS}/store", BLAZE_TESTS="affected",
                   # The build manifest (#550) parses `.md` inputs as Rust on a fresh target with a warm store.
                   BLAZE_MANIFEST="off", RUST_BACKTRACE="0", HOME="/root")
        env.pop("CARGO_TARGET_DIR", None)
        args = spec.get("test", [])
        if name == "starship":
            args = ["--lib"]
        runs = []
        # Cold, then as a CI runner after main's: a fresh target and daemon with the store.
        for i in range(2):
            if i:
                subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
                shutil.rmtree(os.path.join(ws, "target"), ignore_errors=True)
            r = run(ws, env, args, f"{APPS}/{name}.{i}.log")
            runs.append(r)
            print(name, i, json.dumps({k: v for k, v in r.items() if k not in ("failed",)})[:600], flush=True)
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
        results[name] = runs
        json.dump(results, open(f"{APPS}/survey.json", "w"), indent=1)
    print("DONE")


if __name__ == "__main__":
    main()
