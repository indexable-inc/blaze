#!/usr/bin/env python3
"""coldprof.py: where the time of a cold `blaze build` goes until the daemon is ready.

    coldprof.py PROJECT_DIR --work DIR [--store DIR] [--edit FILE:ANCHOR] [--after S] [--max-wait S]

Runs `blaze build` in PROJECT_DIR with an empty target directory and BLAZE_HOME, and
the store DIR as it is (an empty one when it does not exist), and prints, by seconds since the
command started: every line the daemon and its workers log, when the command returned, when
the daemon said the checkout is ready, and twice a second the cores busy in blaze and in
cargo's processes. `--edit` then adds a statement after ANCHOR in FILE and times the
`blaze build` of that. The cores busy are read from /proc, on Linux alone.
`--max-wait` makes it a check: it exits 1 when the daemon was ready more than S seconds
after the command returned (a fraction of a second on just and ripgrep, some 3 s on nushell,
where reading the program's symbols waits for cargo's last file).
`--until TEXT` waits, once ready, until a line the daemon or a worker logs has TEXT in it (the
build the daemon makes of its own after a cold start that patches what rustc linked:
`memory after request-1` with `BLAZE_MEMORY` set), before `--after` and `--edit`.
`--memory` also follows what the command and the daemon it starts hold (harness/memory.py):
the most they held at once until ready, the largest process's peak, and what the largest
process (the worker of the project) holds once ready.
"""
import os, re, shutil, subprocess, sys, threading, time

import memory

HERE = os.path.dirname(os.path.abspath(__file__))
TOOLCHAIN = open(os.path.join(HERE, "toolchain")).read().strip()
TICK = os.sysconf("SC_CLK_TCK")


def opt(name, default=None):
    if name in sys.argv:
        return sys.argv[sys.argv.index(name) + 1]
    return default


def cpu_by_group():
    """Seconds of CPU so far of the processes alive: blaze's, the rest of this user's, and
    other users', which a timing taken beside is not one to trust."""
    out = {"blaze": 0.0, "build": 0.0, "others": 0.0}
    if not os.path.isdir("/proc"):
        return out
    me = os.getuid()
    for pid in filter(str.isdigit, os.listdir("/proc")):
        try:
            stat = open(f"/proc/{pid}/stat").read()
            uid = os.stat(f"/proc/{pid}").st_uid
        except OSError:
            continue
        comm = stat[stat.index("(") + 1:stat.rindex(")")]
        fields = stat[stat.rindex(")") + 2:].split()
        used = (int(fields[11]) + int(fields[12])) / TICK
        if uid != me:
            out["others"] += used
        elif comm.startswith("blaze"):
            out["blaze"] += used
        else:
            out["build"] += used
    return out


def main():
    project = os.path.realpath(sys.argv[1])
    work = os.path.realpath(opt("--work"))
    store = opt("--store") or os.path.join(work, "store")
    after = float(opt("--after", "0"))
    home, target = os.path.join(work, "home"), os.path.join(work, "target")
    for path in (home, target):
        subprocess.run(["rm", "-rf", path], check=True)
    os.makedirs(home, mode=0o700)
    # The daemon's store is `<home>/v2/store`: DIR is copied in now and the store copied back
    # once the daemon stops, so DIR carries from run to run while the home starts empty.
    own_store = os.path.join(home, "v2", "store")
    if os.path.isdir(store):
        shutil.copytree(store, own_store, symlinks=True)
    env = dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN, BLAZE_HOME=home, CARGO_TARGET_DIR=target,
               CARGO_TERM_COLOR="never", CARGO_NET_OFFLINE="true", BLAZE_IDLE_SECS="0",
               BLAZE_STATS="1")
    for name in ("RUSTFLAGS", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_TARGET", "RUST_BACKTRACE"):
        env.pop(name, None)
    events, done = [], threading.Event()
    began = time.monotonic()

    def note(what):
        events.append((time.monotonic() - began, what))

    def follow():
        offsets = {}
        while not done.is_set():
            for root, _, names in os.walk(home):
                for name in names:
                    if name not in ("log", "daemon.log"):
                        continue
                    path = os.path.join(root, name)
                    try:
                        with open(path, errors="replace") as f:
                            f.seek(offsets.get(path, 0))
                            text = f.read()
                    except OSError:
                        continue
                    end = text.rfind("\n") + 1
                    offsets[path] = offsets.get(path, 0) + len(text[:end].encode())
                    who = os.path.relpath(root, home).replace("workers/", "w:")[:14] if name == "log" else "daemon"
                    for line in text[:end].splitlines():
                        note(f"[{who}] {line[:220]}")
            time.sleep(0.02)

    others = []

    def sample():
        last, at = cpu_by_group(), time.monotonic()
        while not done.is_set():
            time.sleep(0.5)
            now, t = cpu_by_group(), time.monotonic()
            # A process that ended takes its time along: only what went up counts.
            busy = {k: max(0.0, now[k] - last[k]) / (t - at) for k in now}
            others.append(busy["others"])
            note(f"  cpu: blaze {busy['blaze']:5.1f}  build {busy['build']:5.1f}  others {busy['others']:5.1f}")
            last, at = now, t

    threads = [threading.Thread(target=follow, daemon=True), threading.Thread(target=sample, daemon=True)]
    for t in threads:
        t.start()
    sampler = memory.Sampler(0.05).start() if "--memory" in sys.argv else None
    _, run, _ = memory.run(["blaze", "build"], cwd=project, env=env, sampler=sampler or memory.Sampler())
    note(f"=== command returned ({run.returncode})")
    if run.returncode:
        print(run.stderr[-2000:])
    built = time.monotonic() - began
    while run.returncode == 0:
        out = subprocess.run(["blaze", "status"], cwd=project, env=env, capture_output=True, text=True).stdout
        states = re.findall(rf"^    {re.escape(project)}: (.*)$", out, re.M)
        if states and not any(s.startswith("loading") for s in states):
            note(f"=== status: {states}")
            break
        time.sleep(0.05)
    ready = time.monotonic() - began
    if opt("--until") and run.returncode == 0:
        while not done.is_set() and not any(opt("--until") in what for _, what in list(events)):
            time.sleep(0.1)
        note(f"=== logged {opt('--until')!r}")
    time.sleep(after)
    held = None
    if sampler:
        sampler.sample()
        alive = {p for root in list(sampler.roots) for p in memory.tree(root)}
        held = max((memory.held_kb(p) or (0, 0))[0] for p in alive | {0}) // 1024
        note(f"=== memory: peak {sampler.peak_mb} MB at once, largest process {sampler.largest_mb} MB, "
             f"the largest holds {held} MB now")
    edit = None
    if opt("--edit"):
        file, anchor = opt("--edit").split(":", 1)
        path = os.path.join(project, file)
        pristine = open(path).read()
        assert pristine.count(anchor) == 1, anchor
        try:
            open(path, "w").write(pristine.replace(anchor, anchor + "\n    let _ = std::hint::black_box(7u64);"))
            t = time.monotonic()
            note("=== edit")
            r = subprocess.run(["blaze", "build"], cwd=project, env=env, capture_output=True, text=True)
            edit = time.monotonic() - t
            note(f"=== edit built ({r.returncode}) in {edit:.2f}s")
        finally:
            open(path, "w").write(pristine)
    time.sleep(0.3)
    done.set()
    if sampler:
        sampler.stop()
    subprocess.run(["blaze", "stop"], cwd=project, env=env, capture_output=True)
    if os.path.isdir(own_store):
        shutil.rmtree(store, ignore_errors=True)
        shutil.copytree(own_store, store, symlinks=True)
    quiet = None
    for at, what in sorted(events, key=lambda e: e[0]):
        # Runs of the same cpu line are folded.
        if what.startswith("  cpu:"):
            if what == quiet:
                continue
            quiet = what
        print(f"{at:8.2f}  {what}")
    busy = sum(others) / max(1, len(others))
    print(f"\nbuild {built:.2f}s  ready {ready:.2f}s" + (f"  first edit {edit:.2f}s" if edit is not None else "")
          + f"  others {busy:.2f} cores"
          + (f"  peak {sampler.peak_mb} MB  largest {sampler.largest_mb} MB  ready {held} MB" if sampler else ""))
    most = opt("--max-wait")
    if most is not None and run.returncode:
        sys.exit(1)
    if most is not None and ready - built > float(most):
        print(f"READY {ready - built:.2f}s AFTER THE COMMAND RETURNED, MORE THAN {most}s")
        sys.exit(1)


if __name__ == "__main__":
    main()
