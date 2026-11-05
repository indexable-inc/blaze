#!/usr/bin/env python3
"""loadretry.py: a failed load is retried by the same daemon after configuration edits (#597).

An unchanged refusal must still fail, and changing the refused flag must report the new
flag. Removing or rewriting the configuration then has to build and run without stopping
the daemon. Both cargo configuration filenames and a configuration above the workspace
are covered. BLAZE_BIN names the compiler, with blaze beside it.
"""
import os, shutil, subprocess, sys, tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from blaze_cli import base_env
from storelib import daemon_pid


def main():
    work = tempfile.mkdtemp(prefix="rj-loadretry-", dir="/tmp" if os.name != "nt" else None)
    print(f"work: {work}", flush=True)
    ws = os.path.join(work, "ws")
    os.makedirs(os.path.join(ws, "src"))
    with open(os.path.join(ws, "Cargo.toml"), "w") as out:
        out.write('[package]\nname = "loadretry"\nversion = "0.0.0"\nedition = "2021"\n')
    with open(os.path.join(ws, "src", "main.rs"), "w") as out:
        out.write('fn main() { println!("recovered"); }\n')
    home = os.path.join(work, "home")
    env = base_env(home)
    pid = None

    def blaze(*args):
        return subprocess.run(["blaze", *args], cwd=ws, env=env,
                              capture_output=True, text=True, timeout=300)

    def same_daemon():
        nonlocal pid
        # The daemon's lock names its pid (storelib.daemon_pid); `blaze status` says it for
        # people only.
        current = daemon_pid(home)
        assert current is not None, f"no daemon runs on {home}"
        if pid is None:
            pid = current
        assert current == pid, f"daemon restarted: {pid} -> {current}"

    def refused(flag):
        ran = blaze("run", "-q")
        assert ran.returncode != 0 and f"share-generics={flag}" in ran.stderr, ran.stdout + ran.stderr
        same_daemon()

    def recovered(label):
        ran = blaze("run", "-q")
        assert (ran.returncode, ran.stdout) == (0, "recovered\n"), ran.stdout + ran.stderr
        same_daemon()
        print(f"PASS {label}, same daemon {pid}", flush=True)

    def configure(path, flag):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as out:
            out.write(f'[build]\nrustflags = ["-Zshare-generics={flag}"]\n')

    try:
        # A failed worker is not kept just because none of its Rust files changed.
        for directory, name in [(ws, "config.toml"), (ws, "config"), (work, "config.toml")]:
            path = os.path.join(directory, ".cargo", name)
            configure(path, "y")
            refused("y")
            refused("y")
            configure(path, "n")
            refused("n")
            os.remove(path)
            recovered(f"remove {os.path.relpath(path, work)}")
        path = os.path.join(ws, ".cargo", "config.toml")
        configure(path, "y")
        refused("y")
        with open(path, "w") as out:
            out.write('[build]\nrustflags = []\n')
        recovered("rewrite configuration")
        # A source error is said again, under the same daemon, until its Rust source changes.
        ws = os.path.join(work, "broken")
        os.makedirs(os.path.join(ws, "src"))
        with open(os.path.join(ws, "Cargo.toml"), "w") as out:
            out.write('[package]\nname = "broken"\nversion = "0.0.0"\nedition = "2021"\n')
        source = os.path.join(ws, "src", "main.rs")
        with open(source, "w") as out:
            out.write('fn main() { let = ; }\n')
        for _ in range(2):
            ran = blaze("run", "-q")
            assert ran.returncode != 0 and "expected" in ran.stderr, ran.stdout + ran.stderr
            same_daemon()
        with open(source, "w") as out:
            out.write('fn main() { println!("recovered"); }\n')
        recovered("correct source diagnostics")
    finally:
        blaze("stop")
    shutil.rmtree(work)


if __name__ == "__main__":
    main()
