#!/usr/bin/env python3
"""memsplit.py PROJECT [--work DIR] [--edits KIND,...] [--env NAME=VALUE]...: what each process
of the daemon holds, and of what, at each step of a project's first hour.

`bench.py` says what the daemon's whole tree holds. This says it per process (the daemon, the
project's worker, the spare) and per kind of page, on Linux from `/proc/PID/status`: anonymous
(the heap, what the allocator kept), file-backed (the executable, mapped store files) and
shared. The steps: once ready, once the daemon has built its own base (the build in full
nobody asked for, `build_own_base`), with the most the tree held while it did, and after each
edit of `--edits` (default `body,newfn`, from bench.py's `PROJECTS`) and its undoing.
`--env` is given to the daemon (it takes its environment from the first build), to try an
allocator setting or `BLAZE_SPARE=0`. Writes the steps as JSON on stdout's last line.
Linux only; the projects are bench.py's, fetched as it fetches them.
"""
import json, os, re, shutil, subprocess, sys, time

import bench, blaze_cli, memory
from blaze_cli import Side, fetch, wait_warm

FIELDS = ("VmRSS", "RssAnon", "RssFile", "RssShmem", "VmHWM", "VmSwap")


def role(pid):
    try:
        args = open(f"/proc/{pid}/cmdline", "rb").read().split(b"\0")
    except OSError:
        return "gone"
    name = os.path.basename(args[0].decode(errors="replace"))
    if b"--spare" in args:
        return "spare"
    if name == "blaze-engine":
        return "daemon" if args[1:2] == [b"daemon"] else "worker"
    return name


def held(pid):
    try:
        status = open(f"/proc/{pid}/status").read()
    except OSError:
        return None
    return {f: round(int(re.search(rf"^{f}:\s+(\d+) kB", status, re.M)[1]) / 1024) for f in FIELDS
            if re.search(rf"^{f}:", status, re.M)}


def split(daemon, label, steps, **extra):
    pid = daemon.pid()
    rows = [dict(pid=p, role=role(p), **(held(p) or {})) for p in memory.tree(pid)] if pid else []
    total = {f: sum(r.get(f, 0) for r in rows) for f in FIELDS[:4]}
    steps.append(dict(step=label, total=total, processes=rows, **extra))
    print(f"{label:14} tree {total['VmRSS']:6} MB (anon {total['RssAnon']:6}, file {total['RssFile']:5}, shmem {total['RssShmem']:4})"
          + "".join(f"  {r['role']} {r.get('VmRSS', 0)} (anon {r.get('RssAnon', 0)}, hwm {r.get('VmHWM', 0)})" for r in rows if r.get("VmRSS", 0) > 50)
          + "".join(f"  {k} {v}" for k, v in extra.items()), flush=True)


def logs(daemon):
    return "".join(open(log, errors="replace").read() for _, log in daemon.workers() if os.path.exists(log))


def wait_own_base(daemon, sampler, timeout=3600):
    """Waits until the build of the daemon's own base is over: the line it starts with, then the
    `edit-to-binary:` line once it is linked (or the line that says it was interrupted or given
    up), or the worker's memory still for a minute when nothing says. An edit before the link is
    over interrupts it, and is built on top of all the base holds."""
    began, still, last = time.time(), time.time(), 0
    while time.time() - began < timeout:
        text = logs(daemon)
        at = text.rfind("compiling the program for edits to patch")
        if at >= 0 and re.search(r"^(edit-to-binary: |interrupted: |not compiling the program)", text[at:], re.M):
            return time.time() - began, text[at:].splitlines()[0]
        now = daemon.resident_mb()
        if abs(now - last) > 64:
            last, still = now, time.time()
        elif time.time() - still > 60:
            return time.time() - began, "still for a minute" + (" (started)" if at >= 0 else " (never started)")
        time.sleep(0.5)
    return time.time() - began, "timed out"


def main():
    argv = sys.argv[1:]
    work = bench.options(argv, "--work", "/tmp/memsplit")[0]
    kinds = bench.options(argv, "--edits", "body,newfn")[0].split(",")
    extra_env = dict(it.split("=", 1) for it in bench.options(argv, "--env"))
    name, = argv
    spec = bench.PROJECTS[name]
    source, prepared = fetch(name, spec), {}
    if "prepare" in spec:
        import importlib
        source, prepared = importlib.import_module(spec["prepare"]).prepare(source)
    ws = os.path.realpath(os.path.join(work, name))
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    shutil.copytree(source, ws, symlinks=True, ignore=lambda at, names: [n for n in names if n in ("target", "target-ref", ".git")
                                                                         or ("prepare" in spec and n == "build" and os.path.samefile(at, source))])
    home = os.path.join(work, "home")
    env = dict(blaze_cli.base_env(home), **spec.get("env", {}), **prepared, **extra_env)
    env["PATH"] = os.path.dirname(bench.blaze_of("cranelift")) + os.pathsep + env["PATH"]
    env.update(BLAZE_STATS="1")
    env.pop("RUSTC_WRAPPER", None)
    ours = Side(ws, env, True)
    build = spec.get("build", [])
    subprocess.run(["cargo", "fetch"], cwd=ws, env=env, capture_output=True)
    daemon, steps = bench.Daemon(ours), []
    try:
        took, ran = ours.cargo("build", *build)
        assert ran.returncode == 0, ran.stderr[-3000:]
        warm, states = wait_warm(ours, ws)
        split(daemon, "ready", steps, build_s=round(took, 1), warm_s=round(warm, 1))
        with memory.Sampler() as sampler:
            sampler.add(daemon.pid())
            waited, how = wait_own_base(daemon, sampler)
        split(daemon, "own base", steps, waited_s=round(waited, 1), peak_mb=sampler.peak_mb, how=how)
        for kind in kinds:
            for back in (False, True):
                bench.write_edit(ws, spec["edits"][kind], back)
                with memory.Sampler() as sampler:
                    sampler.add(daemon.pid())
                    took, ran = ours.cargo("build", *build)
                split(daemon, kind + (" back" if back else ""), steps, wall_s=round(took, 3), peak_mb=sampler.peak_mb,
                      built=bench.how_built(ran.stderr)[:40])
    finally:
        ours.cargo("stop")
    print(json.dumps(dict(project=name, env=extra_env, steps=steps)))


if __name__ == "__main__":
    main()
