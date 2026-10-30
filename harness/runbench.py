#!/usr/bin/env python3
"""runbench.py: how fast the programs ra-jit's LLVM builds run, against rustc's own builds.

usage: runbench.py [NAME...] [--rounds 5] [--builds release,checked,ra-jit] [--variant ...]
                   [--json out.json] [--table out.md]

Every program is built each of these ways, then each of its benchmarks is run `--rounds`
times, the builds taking turns, and reported by its median wall time:

  release   `cargo build --release`
  checked   the same with `-C overflow-checks=on -C debug-assertions=on`: release optimization
            with what the dev profile checks, which is what ra-jit's code of a dev build has
            to keep
  ra-jit    blaze's own build, `cargo blaze build --release`, every crate of it compiled by
            blaze, LLVM at -O3 (`RA_JIT_BACKEND=llvm RA_JIT_OPT=3`)

`--variant NAME:VAR=VALUE,...` adds an ra-jit build with those variables set too (say
`--variant ra-jit-mir2:RA_JIT_MIR_OPT=2`). Before timing, each benchmark is run once with
every build and its exit status, stdout and stderr (scrubbed as apps.json says) have to be
release's; a build that differs is reported and not timed.

NAME is `kernels` (harness/corpus/kernels, a benchmark per kernel) or an entry of
apps/apps.json with a `bench` (checked out as apps.py does, under APPS_DIR). Default: kernels,
numeric, jsonrt, ripgrep, fd, tokei, eza, bat.

Memory is kept beside the times (memory.py, docs/benchmarks.md "Memory and disk"): each run's
peak resident memory as the kernel kept it (`wait4`), the size of each program, and what each
build held: `build_largest_mb`, the largest one process (rustc, ra-jit, the linker), exact, and
`build_peak_mb`, the most the build's processes held at once, sampled.

Every build is cold, its target directory removed first, and `build_s` is the whole of it.

The ratio is ra-jit's time over release's (1.00 is as fast, 1.10 is 10% slower), with the
geometric mean over the benchmarks. RA_JIT_BIN names the ra-jit used, and the `cargo-blaze` beside it; it needs
`--features llvm`. Python 3.11+, stdlib only.
"""
import argparse, json, math, os, shlex, shutil, statistics, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import apps, memory  # noqa: E402

KERNELS = ["sieve", "collatz", "fnv", "qsort", "matmul", "nbody", "mandelbrot", "words"]
DEFAULT = ["kernels", "numeric", "jsonrt", "ripgrep", "fd", "tokei", "eza", "bat"]
RA_JIT_ENV = {"RA_JIT_BACKEND": "llvm", "RA_JIT_OPT": "3"}
# What a variant may not inherit from the shell: the build is said by its flags alone.
OWN_VARS = ["RA_JIT_BACKEND", "RA_JIT_OPT", "RA_JIT_EMIT", "RA_JIT_MIR_OPT", "RA_JIT_INLINE",
            "RA_JIT_BUILT_IN", "RA_JIT_LLVM_INLINE_THRESHOLD",
            "RUSTFLAGS", "CARGO_INCREMENTAL"]


def spec_of(name):
    if name == "kernels":
        return {"package": "kernels", "path": os.path.join(HERE, "corpus", "kernels"),
                "benches": {k: [k] for k in KERNELS}}
    spec = json.load(open(apps.SPECS))[name]
    if "bench" not in spec:
        raise SystemExit(f"{name}: apps.json has no `bench` for it")
    return {**spec, "benches": {name: spec["bench"]}}


def workspace(name, spec):
    """The program's source, its dependencies vendored beside it."""
    if "path" in spec:
        ws = os.path.join(apps.APPS_DIR, name)
        if not os.path.isdir(ws):
            shutil.copytree(spec["path"], ws, ignore=shutil.ignore_patterns("target*"))
            with open(os.path.join(ws, "Cargo.toml"), "a") as out:
                out.write("\n[workspace]\n")
            subprocess.run(["cargo", "generate-lockfile"], cwd=ws, check=True, capture_output=True)
    else:
        ws = apps.checkout(name, spec)
        apps.prepare(ws, spec)
    apps.localize(ws)
    os.environ.update(spec.get("build_env", {}))
    apps.setup(ws, spec)
    return ws


def cargo_release(ws, spec, label, rustflags):
    target = os.path.join(ws, f"target-{label}")
    # Cold, as ra-jit's build is: the build time is published beside it.
    shutil.rmtree(target, ignore_errors=True)
    flags = f"--cap-lints allow {apps.own_flags(spec)} {rustflags}"
    env = {**os.environ, "CARGO_TARGET_DIR": target, "RUSTFLAGS": " ".join(flags.split())}
    for var in OWN_VARS[:-2]:
        env.pop(var, None)
    argv = ["cargo", "build", "--release", "-p", spec["package"], *spec.get("cargo_args", [])]
    took, ran, used = memory.run(argv, ws, env)
    if ran.returncode != 0:
        raise SystemExit(f"`{shlex.join(argv)}` failed:\n{(ran.stdout + ran.stderr)[-3000:]}")
    # Beside ra-jit's, as a program may look for files next to itself (helix its `runtime`).
    kept = os.path.join(ws, f"bin-{label}")
    shutil.copy2(os.path.join(target, "release", spec.get("bin", spec["package"])), kept)
    return kept, took, used


def ra_jit_release(ws, spec, label, extra):
    """blaze's build of the program, with the release profile: (binary, seconds, memory)."""
    target = os.path.join(ws, f"target-{label}")
    shutil.rmtree(target, ignore_errors=True)
    env = {**os.environ, "CARGO_TARGET_DIR": target}
    for var in OWN_VARS:
        env.pop(var, None)
    flags = f"--cap-lints allow {apps.own_flags(spec)}"
    env.update({**RA_JIT_ENV, **extra, "RUSTFLAGS": flags.strip()})
    blaze = os.path.join(os.path.dirname(apps.RA_JIT), "cargo-blaze")
    argv = [blaze, "blaze", "build", "--release", "-p", spec["package"], *spec.get("cargo_args", [])]
    took, ran, used = memory.run(argv, ws, env)
    if ran.returncode != 0:
        raise SystemExit(f"`{shlex.join(argv)}` failed:\n{(ran.stdout + ran.stderr)[-3000:]}")
    kept = os.path.join(ws, f"bin-{label}")
    shutil.copy2(os.path.join(target, "release", spec.get("bin", spec["package"])), kept)
    return kept, took, used


class Runner:
    """Runs a benchmark's command line with one build's program."""

    def __init__(self, ws, spec):
        self.ws, self.spec = ws, spec
        self.argv0 = spec.get("bin", spec["package"])
        self.env = {**os.environ, **spec.get("env", {})}
        for var in OWN_VARS:
            self.env.pop(var, None)

    def behaviour(self, binary, argv):
        ran = subprocess.run([self.argv0, *argv], executable=binary, cwd=self.ws, env=self.env,
                             capture_output=True, stdin=subprocess.DEVNULL, timeout=1800)
        scrub = apps.App.scrub.__get__(type("S", (), {"scrubbed": self.spec.get("scrub", [])})())
        where, directory = os.fsencode(binary), os.fsencode(os.path.dirname(binary))
        clean = lambda out: scrub(out.replace(where, b"<program>").replace(directory, b"<directory>"))
        return ran.returncode, clean(ran.stdout), clean(ran.stderr) if self.spec.get("compare_stderr") else b""

    def seconds(self, binary, argv):
        """The seconds of one run, and the peak resident memory of the program (its tree's
        largest process, as the kernel kept it)."""
        began = time.monotonic()
        child = subprocess.Popen([self.argv0, *argv], executable=binary, cwd=self.ws, env=self.env,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL)
        largest = memory.waited(child)
        return time.monotonic() - began, largest


def geomean(values):
    values = [v for v in values if v and v > 0]
    return math.exp(sum(math.log(v) for v in values) / len(values)) if values else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("names", nargs="*")
    ap.add_argument("--rounds", type=int, default=5)
    ap.add_argument("--builds", default="release,checked,ra-jit")
    ap.add_argument("--variant", action="append", default=[])
    ap.add_argument("--json")
    ap.add_argument("--table")
    a = ap.parse_args()
    names = a.names or DEFAULT
    builds = [it for it in a.builds.split(",") if it]
    variants = {}
    for variant in a.variant:
        label, _, assigns = variant.partition(":")
        variants[label] = dict(it.split("=", 1) for it in assigns.split(",") if it)
        builds.append(label)
    if "release" not in builds:
        builds.insert(0, "release")
    results = {"builds": builds, "variants": variants, "programs": {}}

    for name in names:
        spec = spec_of(name)
        print(f"== {name}", flush=True)
        ws = workspace(name, spec)
        programs, info = {}, {}
        for build in builds:
            try:
                if build == "release":
                    programs[build], secs, used = cargo_release(ws, spec, build, "")
                    info[build] = {"build_s": round(secs, 1)}
                elif build == "checked":
                    programs[build], secs, used = cargo_release(
                        ws, spec, build, "-C overflow-checks=on -C debug-assertions=on")
                    info[build] = {"build_s": round(secs, 1)}
                else:
                    extra = {} if build == "ra-jit" else variants[build]
                    programs[build], secs, used = ra_jit_release(ws, spec, build, extra)
                    info[build] = {"build_s": round(secs, 1)}
                # What building it held (memory.py), and what it built.
                info[build].update(build_peak_mb=used["peak_mb"], build_largest_mb=used["largest_mb"],
                                   size_mb=round(os.path.getsize(programs[build]) / 1e6, 2))
                print(f"  built {build} in {info[build]['build_s']} s", flush=True)
            except SystemExit as failed:
                info[build] = {"error": str(failed)[-2000:]}
                print(f"  {build}: FAILED {str(failed)[-1500:]}", flush=True)
        runner = Runner(ws, spec)
        benches = {}
        for bench, argv in spec["benches"].items():
            want = runner.behaviour(programs["release"], argv)
            timed = []
            for build in builds:
                if build not in programs:
                    continue
                got = want if build == "release" else runner.behaviour(programs[build], argv)
                if got != want:
                    print(f"  {bench}: {build} DIFFERS from release (status {got[0]} vs {want[0]})", flush=True)
                    info[build].setdefault("differs", []).append(bench)
                    continue
                timed.append(build)
            times, held = {build: [] for build in timed}, {build: [] for build in timed}
            for round_ in range(a.rounds):
                # Each round starts with another build, so what the machine does meanwhile
                # falls on all of them alike.
                order = timed[round_ % len(timed):] + timed[: round_ % len(timed)]
                for build in order:
                    took, largest = runner.seconds(programs[build], argv)
                    times[build].append(took)
                    held[build].append(largest)
            medians = {build: statistics.median(ts) for build, ts in times.items()}
            base = medians["release"]
            row = {build: {"median_s": round(m, 4), "ratio": round(m / base, 3),
                           "runs": [round(t, 4) for t in times[build]],
                           "peak_mb": round(statistics.median(held[build])), "peak_max_mb": max(held[build])}
                   for build, m in medians.items()}
            benches[bench] = row
            print(f"  {bench}: " + ", ".join(f"{b} {row[b]['median_s']:.3f}s ({row[b]['ratio']:.2f}) {row[b]['peak_mb']} MB"
                                             for b in timed), flush=True)
        results["programs"][name] = {"builds": info, "benches": benches}
        if a.json:
            json.dump(results, open(a.json, "w"), indent=1)

    all_benches = [(p, b, row) for p, prog in results["programs"].items() for b, row in prog["benches"].items()]
    results["geomean"] = {build: geomean([row.get(build, {}).get("ratio") for _, _, row in all_benches])
                          for build in builds}
    if a.json:
        json.dump(results, open(a.json, "w"), indent=1)
    lines = ["| Benchmark | release (s) | " + " | ".join(f"{b} / release" for b in builds if b != "release") + " |",
             "|---|---|" + "---|" * (len(builds) - 1)]
    for program, bench, row in all_benches:
        cells = [f"{row[b]['ratio']:.2f}" if b in row else "differs" if bench in results["programs"][program]["builds"].get(b, {}).get("differs", []) else "failed"
                 for b in builds if b != "release"]
        lines.append(f"| {bench} | {row['release']['median_s']:.3f} | " + " | ".join(cells) + " |")
    lines.append("| geomean | | " + " | ".join(
        f"{results['geomean'][b]:.2f}" if results["geomean"][b] else "-" for b in builds if b != "release") + " |")
    table = "\n".join(lines)
    print("\n" + table)
    if a.table:
        open(a.table, "w").write(table + "\n")


if __name__ == "__main__":
    main()
