#!/usr/bin/env python3
"""spare.py: a second project's start under a daemon that is already running, and what the
daemon holds meanwhile, for each way of running it (issue #56).

    spare.py [PROJECT...] --variant LABEL:BIN[:VAR=VALUE...]... [--rounds N] [--out FILE] [--work DIR]

A variant is a blaze and the variables it runs with, for example
`--variant asked:target/release/blaze-engine --variant ready:target/release/blaze-engine:BLAZE_SPARE=ready`.
Each round runs every variant once, in turn, for each PROJECT (bench.py's, default ripgrep fd
nushell), each with a home and a store of its own:
  first   a small package of no dependencies is built with `blaze build`, which starts the
          daemon; once the daemon has it loaded and any spare worker has loaded the library, what
          the daemon's tree holds (`first_mb`)
  cold    the project from a copy with no target directory: `blaze build` and the wait
          until the daemon has it loaded (`cold_s`, with `cold_build_s` the command alone), the
          most the daemon's tree and the command's held meanwhile (`cold_peak_mb`), and what the
          daemon holds once it is ready (`cold_mb`)
  warm    the daemon is stopped, started again by the small package, and the project, whose
          target directory and store are now full, is built again: cargo has nothing to do, and
          the time is the daemon's loading of the workspace (`warm_s`, `warm_mb`)
The result has every sample and the median of each; the table printed at the end has the medians.
"""
import glob, json, os, shutil, statistics, subprocess, sys, tempfile, time

import blaze_cli, memory
from bench import PROJECTS, options
from blaze_cli import Side, fetch, how_built, wait_warm

FIRST_TOML = '[package]\nname = "first"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n'
FIRST_MAIN = 'fn main() {\n    println!("{}", (1..10u32).sum::<u32>());\n}\n'
SETTLE_SECS = 120


def daemon_pid(side):
    _, status = side.cargo("status")
    for line in status.stdout.splitlines():
        if line.startswith("daemon: pid "):
            return int(line.split()[2].rstrip(","))
    return None


def settled(home):
    """Waits until every spare worker of the daemon in `home` has loaded the library (each says
    `spare: N crates` or that it holds nothing), and a moment more for the memory it freed."""
    time.sleep(2)
    began = time.time()
    while time.time() - began < SETTLE_SECS:
        logs = glob.glob(os.path.join(home, "spares", "*", "log"))
        texts = [open(log, errors="replace").read() for log in logs]
        if all("spare: nothing held" in t or " crates in " in t for t in texts):
            break
        time.sleep(0.25)
    time.sleep(1)


def held(side):
    pid = daemon_pid(side)
    return memory.resident_mb([pid]) if pid else 0


def started(side, label):
    """`blaze build` on `side` and the wait until its daemon has it loaded: the seconds of
    the command, of the command and the wait, and the most the daemon's tree and the command's
    held at once meanwhile."""
    with memory.Sampler() as sampler:
        pid = daemon_pid(side)
        if pid:
            sampler.add(pid)
        took, ran, used = memory.run(side.argv("build"), side.ws, side.env, 7200, sampler=sampler)
        if ran.returncode != 0:
            raise SystemExit(f"{label}: `blaze build` failed:\n{ran.stderr[-3000:]}")
        waited, states = wait_warm(side, side.ws)
    if states != ["ready"]:
        raise SystemExit(f"{label}: the daemon did not get ready: {states}\n{ran.stderr[-3000:]}")
    return took, took + waited, max(sampler.peak_mb, used["largest_mb"]), how_built(ran.stderr)


def sequence(name, spec, source, variant, work):
    label, binary, variables = variant
    home = os.path.join(work, f"home-{label}")
    first_ws, ws = os.path.join(work, f"first-{label}"), os.path.realpath(os.path.join(work, f"{name}-{label}"))
    for path in (home, first_ws, ws):
        shutil.rmtree(path, ignore_errors=True)
    os.makedirs(os.path.join(first_ws, "src"))
    open(os.path.join(first_ws, "Cargo.toml"), "w").write(FIRST_TOML)
    open(os.path.join(first_ws, "src/main.rs"), "w").write(FIRST_MAIN)
    shutil.copytree(source, ws, symlinks=True, ignore=shutil.ignore_patterns("target", "target-ref", ".git"))
    env = dict(blaze_cli.base_env(home), **spec.get("env", {}), **variables)
    env["PATH"] = os.path.dirname(os.path.abspath(binary)) + os.pathsep + env["PATH"]
    env.pop("RUSTC_WRAPPER", None)
    if "BLAZE_SPARE" not in variables:
        env.pop("BLAZE_SPARE", None)
    first, project = Side(os.path.realpath(first_ws), env, True), Side(ws, env, True)
    first.cargo("stop")
    row = {}
    try:
        started(first, f"{label} first")
        settled(home)
        row["first_mb"] = held(first)
        row["cold_build_s"], row["cold_s"], row["cold_peak_mb"], row["cold_built"] = started(project, f"{name} {label} cold")
        settled(home)
        row["cold_mb"] = held(project)
        first.cargo("stop")
        started(first, f"{label} first again")
        settled(home)
        row["first_again_mb"] = held(first)
        row["warm_build_s"], row["warm_s"], row["warm_peak_mb"], row["warm_built"] = started(project, f"{name} {label} warm")
        settled(home)
        row["warm_mb"] = held(project)
    finally:
        first.cargo("stop")
    for path in (home, first_ws, ws):
        shutil.rmtree(path, ignore_errors=True)
    return row


def main():
    argv = sys.argv[1:]
    variants = []
    for given in options(argv, "--variant"):
        label, binary, *assigned = given.split(":")
        variants.append((label, binary, dict(a.split("=", 1) for a in assigned)))
    if not variants:
        raise SystemExit("spare.py needs at least one --variant LABEL:BIN[:VAR=VALUE...]")
    rounds = int(options(argv, "--rounds", "3")[0])
    out = options(argv, "--out", os.path.join(blaze_cli.HERE, "spare.json"))[0]
    work = (options(argv, "--work") or [None])[0] or tempfile.mkdtemp(prefix="blaze-spare-")
    names = argv or ["ripgrep", "fd", "nushell"]
    result = dict(machine=os.environ.get("BLAZE_BENCH_MACHINE", ""), rounds=rounds,
                  variants=[dict(label=l, bin=b, env=v) for l, b, v in variants], projects={})
    for name in names:
        spec = PROJECTS[name]
        source = fetch(name, spec)
        subprocess.run(["cargo", "fetch"], cwd=source, capture_output=True,
                       env=dict(os.environ, RUSTUP_TOOLCHAIN=blaze_cli.TOOLCHAIN))
        samples = {label: [] for label, _, _ in variants}
        for turn in range(rounds):
            for variant in variants:
                row = sequence(name, spec, source, variant, work)
                samples[variant[0]].append(row)
                print(f"{name} {variant[0]:10} {turn + 1}/{rounds}  " + "  ".join(
                    f"{k} {v:.2f}" if isinstance(v, float) else f"{k} {v}" for k, v in row.items()), flush=True)
        medians = {label: {k: round(statistics.median(r[k] for r in rows), 3) for k in rows[0] if not k.endswith("built")}
                   for label, rows in samples.items()}
        result["projects"][name] = dict(samples=samples, medians=medians)
        json.dump(result, open(out, "w"), indent=1)
    keys = ["first_mb", "cold_s", "cold_mb", "cold_peak_mb", "warm_s", "warm_mb", "warm_peak_mb"]
    print(f"\n{'project':10} {'variant':10} " + " ".join(f"{k:>13}" for k in keys))
    for name, data in result["projects"].items():
        for label, row in data["medians"].items():
            print(f"{name:10} {label:10} " + " ".join(f"{row[k]:>13}" for k in keys))
    print(f"\n{rounds} rounds; every sample in {out}")


if __name__ == "__main__":
    main()
