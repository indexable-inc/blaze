#!/usr/bin/env python3
"""commits.py PROJECT REPO COMMIT... [--work DIR] [--rounds N] [--keep]: edit-to-result of real commits.

For each commit, a checkout of its parent (no `.git`, as leanprof.py copies one) is built with
`cargo ra-jit build` and left until the daemon has built its own base and made its items
(`items_s` in the JSON, the seconds that took after the base); then the commit's files
are written into it (removed files removed) and `cargo ra-jit build` is timed, then the parent's
are written back and it is timed again, `--rounds` times (default 2). The project's build
arguments and environment are bench.py's; REPO is a full clone with the commits. Prints a line
per commit and writes them as JSON to WORK/commits.json, with the lines of what the daemon said
of each edit that tell how it took it (all it said is in WORK/<commit>/said-*.log). A commit's
checkout, home and store are deleted once it is measured, a nushell one being 15 GB, unless
`--keep`. It measures and checks nothing.
"""
import json, os, re, shutil, subprocess, sys, time

import bench, cargo_ra_jit
from cargo_ra_jit import Side, wait_warm
from memsplit import logs, wait_own_base

# What the daemon says of an edit with RA_JIT_STATS: how it took the change and what it looked at.
SAID = re.compile(r"(bodies only|signatures only|functions added|items|items-us|items-made|items-before|items-pushed|items-refused|items-settled|items-moved|items-unmoved|items-codes|patch-made|read-us|check-all|apply-us|check-us|steps|backend|stats|patch|asked|timeline-us|edit-to-binary): ")


def wait_items(daemon, timeout=300):
    """Seconds until the base's items are made (`items-us:`), which an edit that changes items
    is pushed through: a daemon makes them on threads of its own once its base is built, and an
    edit before then asks about all of each crate the edit's files are of. `None` if not made."""
    began = time.time()
    while time.time() - began < timeout:
        text = logs(daemon)
        at = text.rfind("compiling the program for edits to patch")
        # A daemon that writes the code of the items' functions, which moves are found by,
        # is waited for until it has.
        made = at >= 0 and "items-us: " in text[at:]
        if made and ("items-coding: " not in text[at:] or "items-codes: " in text[at:]):
            return round(time.time() - began, 1)
        time.sleep(0.5)
    return None


def wait_said(daemon, said, timeout=10):
    """Seconds until the daemon says `said` after the last build: what it does on threads of
    its own once the build is over, which the next edit would wait for, as the debug info of
    the program laid out where splices write it (`laid out: `). `None` if it isn't said in
    `timeout`."""
    began = time.time()
    while time.time() - began < timeout:
        text = logs(daemon)
        at = text.rfind("edit-to-binary: ")
        if at >= 0 and said in text[at:]:
            return round(time.time() - began, 1)
        time.sleep(0.1)
    return None


def git(repo, *args, text=True):
    return subprocess.run(["git", "-C", repo, *args], capture_output=True, text=text, check=True).stdout


def files_of(repo, commit, paths):
    """The bytes of each path at `commit`, None where it is not there."""
    present = set(git(repo, "ls-tree", "-r", "--name-only", commit, "--", *paths).split("\n")) if paths else set()
    return {p: git(repo, "show", f"{commit}:{p}", text=False) if p in present else None for p in paths}


def write(ws, files):
    for path, data in files.items():
        at = os.path.join(ws, path)
        if data is None:
            if os.path.exists(at):
                os.remove(at)
            continue
        os.makedirs(os.path.dirname(at), exist_ok=True)
        with open(at, "wb") as f:
            f.write(data)


def one(name, spec, repo, commit, work, rounds, extra_env, keep):
    sha = git(repo, "rev-parse", commit).strip()
    parent = git(repo, "rev-parse", f"{sha}^").strip()
    subject = git(repo, "log", "-1", "--format=%s", sha).strip()
    paths = [p for p in git(repo, "diff", "--name-only", "--no-renames", parent, sha).split("\n") if p]
    after, before = files_of(repo, sha, paths), files_of(repo, parent, paths)
    root = os.path.join(work, sha[:10])
    shutil.rmtree(root, ignore_errors=True)
    ws = os.path.join(root, name)
    os.makedirs(ws)
    archive = subprocess.Popen(["git", "-C", repo, "archive", parent], stdout=subprocess.PIPE)
    subprocess.run(["tar", "-x", "-C", ws], stdin=archive.stdout, check=True)
    archive.wait()
    home = os.path.join(root, "home")
    env = dict(cargo_ra_jit.base_env(home), **spec.get("env", {}), **extra_env)
    env["PATH"] = os.path.dirname(bench.ra_jit_of("cranelift")) + os.pathsep + env["PATH"]
    env.update(bench.BACKENDS["cranelift"], RA_JIT_STATS="1", RA_JIT_CACHE=os.path.join(root, "store"))
    env.pop("RUSTC_WRAPPER", None)
    env["RA_JIT_STD_FROM"], _ = bench.shipped_std(bench.ra_jit_of("cranelift"), "cranelift", root)
    build = spec.get("build", [])
    subprocess.run(["cargo", "fetch"], cwd=ws, env=env, capture_output=True)
    ours = Side(ws, env, True)
    daemon = bench.Daemon(ours)
    row = dict(commit=sha[:10], subject=subject, files=len(paths))
    try:
        began = time.monotonic()
        took, ran = ours.cargo("build", *build)
        if ran.returncode != 0:
            row["error"] = ran.stderr[-2000:]
            return row
        wait_warm(ours, ws)
        row["cold_s"] = round(time.monotonic() - began, 1)
        wait_own_base(daemon, None)
        row["items_s"] = wait_items(daemon)
        row["laid_out_s"] = wait_said(daemon, "laid out: ")
        walls, said = [], []
        row["base_said"] = [line[:200] for line in daemon.said().splitlines() if SAID.match(line)][-20:]
        for n in range(rounds):
            for label, files in (("commit", after), ("back", before)):
                write(ws, files)
                took, ran = ours.cargo("build", *build)
                walls.append((label, round(took, 3), ran.returncode, bench.how_built(ran.stderr)[:60]))
                text = daemon.said(1)
                with open(os.path.join(root, f"said-{n}-{label}.log"), "w") as f:
                    f.write(text)
                said.append([line[:200] for line in text.splitlines() if SAID.match(line)][:40])
        row["walls"], row["said"] = walls, said
        return row
    finally:
        ours.cargo("stop")
        if not keep:
            for entry in os.listdir(root):
                if not entry.startswith("said-"):
                    shutil.rmtree(os.path.join(root, entry), ignore_errors=True)


def main():
    argv = sys.argv[1:]
    work = os.path.realpath(bench.options(argv, "--work", "/tmp/commits")[0])
    rounds = int(bench.options(argv, "--rounds", "2")[0])
    extra_env = dict(it.split("=", 1) for it in bench.options(argv, "--env"))
    keep = "--keep" in argv
    argv = [it for it in argv if it != "--keep"]
    name, repo, *commits = argv
    spec = bench.PROJECTS[name]
    os.makedirs(work, exist_ok=True)
    rows = []
    for commit in commits:
        row = one(name, spec, repo, commit, work, rounds, extra_env, keep)
        rows.append(row)
        print(json.dumps(row), flush=True)
        open(os.path.join(work, "commits.json"), "w").write(json.dumps(rows, indent=1))


if __name__ == "__main__":
    main()
