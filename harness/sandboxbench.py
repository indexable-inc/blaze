#!/usr/bin/env python3
"""sandboxbench.py: what the sandbox a test runs in costs, on Linux and macOS.

usage: sandboxbench.py [--cpus 0-15] [--rounds 5] [--runs 600] [--tests 600] [--io 50]
       (BLAZE_BIN names the blaze built; blaze is found next to it)

Two measurements, each in `--rounds` rounds whose setups alternate, as medians with the
lowest and highest of the rounds:

- a program that does nothing, run `--runs` times one after another and 16 at a time, bare
  and in `blaze sandbox` (with no record, which installs no seccomp filter, and with a
  record of what it wrote and reached);
- a generated library, opted into sandboxed tests, with `--tests` unit tests, `--io` of which
  read a file of the package and write one in the package and one in `std::env::temp_dir()`, under `blaze test`
  once the daemon serves its build: with `BLAZE_SANDBOX=off`, in the sandbox under
  `BLAZE_TESTS=all`, under `affected` with what passed forgotten before each run (so every
  test runs and records what it reads), and as one process per test (`--exact name`, 16 at a
  time), in a sandbox each and bare. The first three run each test in a process forked from
  the executable's fork server, blaze's default; the `one process` setups run them as
  libtest's threads in one process (`BLAZE_TEST_PROCESSES=one`), as cargo does. The command's wall time is given whole and less that of
  `blaze test --no-run` (the build and the daemon's answer, which every setup has).
  It is `--lib`: the doc tests, which load the workspace whatever runs them, are not measured.

On Linux the process and what it runs are kept to `--cpus` (`sched_setaffinity`, which
children inherit): the tests' harness runs as many tests at once as the CPUs it may use. macOS
has no affinity: there the tests' harness is told as many (`RUST_TEST_THREADS`), and the
programs run on any CPU.

Python 3.13, stdlib only.
"""

import argparse
import asyncio
import json
import os
import shutil
import statistics
import sys
import tempfile
import time

STEP_TIMEOUT_S = 900


class Failed(Exception):
    pass


async def run(argv, *, cwd=None, env=None, timeout=STEP_TIMEOUT_S, ok=(0,)):
    """Runs `argv` to its end: its status and output, or `Failed` naming what went wrong."""
    proc = await asyncio.create_subprocess_exec(*argv, cwd=cwd, env=env, stdin=asyncio.subprocess.DEVNULL,
                                                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    try:
        async with asyncio.timeout(timeout):
            out, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise Failed(f"{argv[:4]} did not end in {timeout} s")
    if ok is not None and proc.returncode not in ok:
        tail = (out + err).decode(errors="replace")[-3000:]
        raise Failed(f"{' '.join(map(str, argv[:6]))} ended {proc.returncode}:\n{tail}")
    return proc.returncode, (out + err).decode(errors="replace")


async def timed(argvs, parallel, **kw):
    """Seconds to run every command of `argvs`, `parallel` at a time."""
    gate = asyncio.Semaphore(parallel)

    async def one(argv):
        async with gate:
            await run(argv, **kw)

    start = time.perf_counter()
    async with asyncio.TaskGroup() as group:
        for argv in argvs:
            group.create_task(one(argv))
    return time.perf_counter() - start


def spec(path, cwd, env, overlays, record=None):
    with open(path, "w") as f:
        json.dump({"cwd": cwd, "env": sorted(env.items()), "overlays": overlays, "writable": [],
                   "record": record, "record_reads": False, "not_inputs": []}, f)
    return path


def library(root, tests, io):
    """A package of `tests` unit tests, the last `io` of which read and write files."""
    os.makedirs(os.path.join(root, "src"))
    os.makedirs(os.path.join(root, "data"))
    open(os.path.join(root, "Cargo.toml"), "w").write(
        '[package]\nname = "sbxbench"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n\n'
        '[workspace.metadata.blaze]\nexperimental = ["sandboxed-tests"]\n')
    open(os.path.join(root, "data", "in.txt"), "w").write("the input\n" * 64)
    lines = ["pub fn mix(x: u64) -> u64 { x.wrapping_mul(0x9e3779b97f4a7c15).rotate_left(17) ^ x }", "",
             "#[cfg(test)]", "mod tests {", "    use super::*;"]
    for i in range(tests - io):
        lines.append(f"    #[test] fn pure_{i:04}() {{ let mut x = {i + 1}; for _ in 0..1000 {{ x = mix(x); }} "
                     f"assert_ne!(x, 0); }}")
    for i in range(io):
        lines.append(f"""    #[test] fn io_{i:04}() {{
        let dir = env!("CARGO_MANIFEST_DIR");
        let input = std::fs::read_to_string(format!("{{dir}}/data/in.txt")).unwrap();
        std::fs::create_dir_all(format!("{{dir}}/out")).unwrap();
        let mine = format!("{{dir}}/out/io_{i:04}.txt");
        std::fs::write(&mine, &input).unwrap();
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), input);
        std::fs::remove_file(&mine).unwrap();
        let temp = std::env::temp_dir().join(format!("sbxbench-{{}}-{i:04}", std::process::id()));
        std::fs::write(&temp, &input).unwrap();
        std::fs::remove_file(&temp).unwrap();
    }}""")
    lines.append("}")
    open(os.path.join(root, "src", "lib.rs"), "w").write("\n".join(lines) + "\n")


def forget_passed(root):
    """Removes what `affected` keeps of the tests that passed, so that each runs again."""
    for top in root:
        for dirpath, _, names in os.walk(top):
            for name in names:
                if name in ("tests-passed", "tests-inputs"):
                    os.remove(os.path.join(dirpath, name))


def test_executable(pkg):
    """The program of the library's tests the daemon built: the newest `sbxbench-*` that runs."""
    found = []
    for dirpath, _, names in os.walk(os.path.join(pkg, "target")):
        for name in names:
            path = os.path.join(dirpath, name)
            if name.startswith("sbxbench-") and "." not in name and os.access(path, os.X_OK):
                found.append((os.path.getmtime(path), path))
    if not found:
        raise Failed(f"no program of tests under {pkg}/target")
    return max(found)[1]


def table(rows):
    width = max(len(name) for name in rows)
    print(f"{'':{width}}  median      lowest     highest   (seconds; {len(next(iter(rows.values())))} rounds)")
    for name, times in rows.items():
        print(f"{name:{width}}  {statistics.median(times):8.3f}  {min(times):9.3f}  {max(times):9.3f}")


async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--cpus", default="0-15")
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--runs", type=int, default=600)
    parser.add_argument("--tests", type=int, default=600)
    parser.add_argument("--io", type=int, default=50)
    args = parser.parse_args()
    low, _, high = args.cpus.partition("-")
    cpus = set(range(int(low), int(high or low) + 1))
    parallel = len(cpus)
    pinned = hasattr(os, "sched_setaffinity")
    if pinned:
        os.sched_setaffinity(0, cpus)
    blaze = os.environ["BLAZE_BIN"]
    bin_dir = os.path.dirname(os.path.abspath(blaze))
    where = f"cpus {args.cpus}" if pinned else f"{os.cpu_count()} cpus, not pinned"
    print(f"{sys.platform}: {where}, {parallel} at a time; load {os.getloadavg()}", flush=True)

    root = tempfile.mkdtemp(prefix="blaze-sandboxbench-")
    home = tempfile.mkdtemp(prefix="rj-sb-", dir="/tmp")
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", BLAZE_HOME=home,
               RUST_BACKTRACE="0")
    if not pinned:
        # Kept under `affected` too, which passes a test only a few variables.
        env.update(RUST_TEST_THREADS=str(parallel), BLAZE_TEST_ENV="RUST_TEST_THREADS")
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "BLAZE_TESTS", "BLAZE_SANDBOX"):
        env.pop(name, None)
    pkg = os.path.join(root, "sbxbench")
    try:
        # A program that does nothing.
        nothing = os.path.join(root, "nothing")
        open(nothing + ".rs", "w").write("fn main() {}\n")
        await run(["rustc", "-O", "-o", nothing, nothing + ".rs"], env=env)
        bare_spec = spec(os.path.join(root, "bare.json"), root, env, [root])
        rec_spec = spec(os.path.join(root, "rec.json"), root, env, [root], os.path.join(root, "rec"))
        setups = {
            "bare": [nothing],
            "sandbox": [blaze, "sandbox", bare_spec, "--", nothing],
            "sandbox+record": [blaze, "sandbox", rec_spec, "--", nothing],
        }
        trivial = {f"{name}, {way}": [] for name in setups for way in ("1 at a time", f"{parallel} at a time")}
        for _ in range(args.rounds):
            for name, argv in setups.items():
                trivial[f"{name}, 1 at a time"].append(await timed([argv] * args.runs, 1, env=env))
                trivial[f"{name}, {parallel} at a time"].append(await timed([argv] * args.runs, parallel, env=env))
        print(f"\n{args.runs} runs of a program that does nothing")
        table(trivial)
        print("per run, 1 at a time (ms): " + ", ".join(
            f"{name} {statistics.median(trivial[f'{name}, 1 at a time']) / args.runs * 1000:.2f}"
            for name in setups), flush=True)

        # The library of tests.
        library(pkg, args.tests, args.io)
        test = ["blaze", "test", "--lib"]
        deadline = time.monotonic() + 600
        while True:
            _, said = await run(test, cwd=pkg, env=env)
            if "[blaze]" in said:
                break
            if time.monotonic() > deadline:
                raise Failed(f"the daemon did not serve the build in 600 s:\n{said[-2000:]}")
            await asyncio.sleep(1)
        await run(test, cwd=pkg, env=dict(env, BLAZE_TESTS="affected"))
        exe = test_executable(pkg)
        _, listed = await run([exe, "--list", "--format", "terse"], cwd=pkg, env=env)
        names = [line.removesuffix(": test") for line in listed.splitlines() if line.endswith(": test")]
        if len(names) != args.tests:
            raise Failed(f"{exe} lists {len(names)} tests, not {args.tests}")
        specs = os.path.join(root, "specs")
        os.makedirs(specs)
        one_spec = spec(os.path.join(specs, "one.json"), pkg, dict(env, CARGO_MANIFEST_DIR=pkg), [pkg])

        async def per_test(sandboxed):
            argvs = []
            for i, name in enumerate(names):
                argv = [exe, "--exact", name, "-q"]
                if sandboxed:
                    with open(one_spec) as f:
                        it = json.load(f)
                    it["record"] = os.path.join(specs, f"{i}.rec")
                    path = os.path.join(specs, f"{i}.json")
                    with open(path, "w") as f:
                        json.dump(it, f)
                    argv = [blaze, "sandbox", path, "--", *argv]
                argvs.append(argv)
            return await timed(argvs, parallel, cwd=pkg, env=env)

        async def command(more, forget=False):
            if forget:
                forget_passed([pkg, home])
            start = time.perf_counter()
            _, said = await run(test, cwd=pkg, env=dict(env, **more))
            took = time.perf_counter() - start
            ran = sum(int(it.split()[0]) for it in said.split("test result: ok. ")[1:])
            if ran != args.tests:
                raise Failed(f"{more} ran {ran} tests, not {args.tests}:\n{said[-2000:]}")
            return took

        async def no_run():
            start = time.perf_counter()
            await run([*test, "--no-run"], cwd=pkg, env=env)
            return time.perf_counter() - start

        modes = {
            "build only (--no-run)": no_run,
            "BLAZE_SANDBOX=off": lambda: command({"BLAZE_SANDBOX": "off", "BLAZE_TESTS": "all"}),
            "sandbox, all": lambda: command({"BLAZE_TESTS": "all"}),
            "sandbox, affected, every test": lambda: command({"BLAZE_TESTS": "affected"}, forget=True),
            "BLAZE_SANDBOX=off, one process": lambda: command(
                {"BLAZE_SANDBOX": "off", "BLAZE_TESTS": "all", "BLAZE_TEST_PROCESSES": "one"}),
            "sandbox, all, one process": lambda: command({"BLAZE_TESTS": "all", "BLAZE_TEST_PROCESSES": "one"}),
            "sandbox, affected, every test, one process": lambda: command(
                {"BLAZE_TESTS": "affected", "BLAZE_TEST_PROCESSES": "one"}, forget=True),
            f"a sandbox per test, {parallel} at a time": lambda: per_test(True),
            f"a process per test, bare, {parallel} at a time": lambda: per_test(False),
        }
        times = {name: [] for name in modes}
        for _ in range(args.rounds):
            for name, measure in modes.items():
                times[name].append(await measure())
        print(f"\n`blaze test` of {args.tests} tests ({args.io} that read and write files), whole command")
        table(times)
        base = statistics.median(times["build only (--no-run)"])
        print(f"\nless the build's median ({base:.3f} s): the tests' part")
        for name in list(modes)[1:7]:
            print(f"  {name}: {statistics.median(times[name]) - base:.3f}")
    finally:
        if os.path.isdir(pkg):
            await run(["blaze", "stop"], cwd=pkg, env=env, timeout=120, ok=None)
        shutil.rmtree(root, ignore_errors=True)
        shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except Failed as error:
        sys.exit(f"sandboxbench: {error}")
