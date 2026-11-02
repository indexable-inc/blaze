#!/usr/bin/env python3
"""How fast tiered code runs (macOS on AArch64, blaze built with `--features llvm`):
demo/lifebench, the Game of Life on a 512 x 512 grid run flat out, built each way the
table of docs/tiers.md has, each run for the same time, the steady rate the mean of the
half-second rates from `--from` seconds on.

usage: tierbench.py [--seconds 25] [--from 15] [--rounds 1] [--together] [--only NAME,...]
                    (BLAZE_BIN names the blaze built; blaze is found next to it)

The builds, by NAME:

  cranelift  level 1: a daemon with BLAZE_TIER=off
  llvm       every function by LLVM at -O3, nothing built in: a daemon with BLAZE_TIER=off and
             the dev profile at opt-level 3 (CARGO_PROFILE_DEV_OPT_LEVEL=3), which v2 compiles
             with LLVM
  level2     a daemon with BLAZE_TIER=2: LLVM -O3, no counting, no PGO
  level3     a daemon as it comes: level 2 counts, level 3 is compiled with what it counted
  checked    cargo, `-C opt-level=3 -C overflow-checks -C debug-assertions`: what the dev
             profile means, optimized
  outline    the same with `step` kept out of `main` (`#[inline(never)]`): what a tier, which
             compiles `step` and cannot build it into the `main` that runs it, could reach at
             most with rustc's own code (`--only` it; it is not one of the defaults)
  release    `cargo build --release`

With `--rounds N` every build runs N times, the builds taking turns, so that load from
elsewhere on the machine falls on all of them alike; each is reported by its median. With
`--together` the builds of a round run at the same time instead, one program each, so that
they share whatever load there is second for second: on a machine busy with other work the
shares of release then hold where the rates of builds run in turn swing by half. It needs a
core free per build.

Python 3.13, stdlib only.
"""

import argparse
import asyncio
import os
import re
import shutil
import statistics
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import tiers  # noqa: E402

ROOT = HERE.parent
NAMES = ["cranelift", "level2", "level3", "checked", "release"]


async def run_program(binary: Path, cwd: Path, env: dict[str, str], seconds: int) -> list[str]:
    proc = await asyncio.create_subprocess_exec(
        str(binary), cwd=cwd, env=dict(env, LIFE_SECONDS=str(seconds), LIFE_SIZE="512"),
        stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.PIPE,
    )
    _, err = await asyncio.wait_for(proc.communicate(), seconds + 60)
    return err.decode(errors="replace").splitlines()


def steady(lines: list[str], since: float) -> tuple[float, list[str]]:
    rates = []
    events = []
    for line in lines:
        if m := re.match(r"rate t=(\S+) gens_per_s=(\S+)", line):
            if float(m[1]) >= since:
                rates.append(float(m[2]))
        elif line.startswith("tier: "):
            events.append(line.removeprefix("tier: "))
    return (statistics.mean(rates) if rates else 0.0), events


async def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--seconds", type=int, default=25)
    parser.add_argument("--from", dest="since", type=float, default=15)
    parser.add_argument("--rounds", type=int, default=1)
    parser.add_argument("--only", default=",".join(NAMES))
    parser.add_argument("--together", action="store_true", help="run the builds of a round at the same time")
    parser.add_argument("--events", action="store_true", help="print the tier events of each daemon run")
    args = parser.parse_args()
    names = [it for it in args.only.split(",") if it]
    bin_dir = Path(os.environ["BLAZE_BIN"]).resolve().parent
    work = Path(tempfile.mkdtemp(prefix="rjb-", dir="/tmp"))
    try:
        shutil.copytree(ROOT / "crates" / "blaze-live", work / "crates" / "blaze-live")
        shutil.copytree(ROOT / "demo" / "lifebench", work / "demo" / "lifebench", ignore=shutil.ignore_patterns("target"))
        project = work / "demo" / "lifebench"
        base = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", RUSTUP_TOOLCHAIN=tiers.PIN)
        for name in ("CARGO_INCREMENTAL", "BLAZE_TIER", "RUSTFLAGS"):
            base.pop(name, None)

        binaries: dict[str, tuple[Path, dict[str, str]]] = {}
        outline = work / "outline" / "demo" / "lifebench"
        if "outline" in names:
            for part in ("crates", "demo"):
                shutil.copytree(work / part, work / "outline" / part, ignore=shutil.ignore_patterns("target"))
            source = outline / "src" / "main.rs"
            text = source.read_text()
            if "    fn step(&mut self) {" not in text:
                raise tiers.StepError("outline: `fn step(&mut self)` is not in demo/lifebench")
            source.write_text(text.replace("    fn step(&mut self) {", "    #[inline(never)]\n    fn step(&mut self) {"))
        for name in names:
            env = dict(base, CARGO_TARGET_DIR=str(work / name))
            if name in ("release", "checked", "outline"):
                if name in ("checked", "outline"):
                    env["RUSTFLAGS"] = "-C overflow-checks=on -C debug-assertions=on"
                cwd = outline if name == "outline" else project
                await tiers.run(f"cargo build ({name})", ["cargo", "build", "--locked", "--release"], cwd, env)
                binaries[name] = (work / name / "release" / "lifebench", env)
                continue
            env["BLAZE_HOME"] = str(work / f"home-{name}")
            if name == "cranelift":
                env["BLAZE_TIER"] = "off"
            elif name == "llvm":
                env.update(BLAZE_TIER="off", CARGO_PROFILE_DEV_OPT_LEVEL="3")
            elif name == "level2":
                env["BLAZE_TIER"] = "2"
            await tiers.serve(project, env)
            binaries[name] = (work / name / "debug" / "lifebench", env)

        results: dict[str, list[float]] = {name: [] for name in names}
        for round_ in range(args.rounds):
            runs = [run_program(binaries[name][0], project, binaries[name][1], args.seconds) for name in names]
            outputs = await asyncio.gather(*runs) if args.together else [await run for run in runs]
            for name, lines in zip(names, outputs):
                rate, events = steady(lines, args.since)
                results[name].append(rate)
                print(f"round {round_ + 1} {name}: {rate:.0f} generations per second", flush=True)
                if args.events:
                    for event in events:
                        print(f"  {event}")
        release = statistics.median(results["release"]) if results.get("release") else None
        print()
        print("| Build | Generations per second | vs release |")
        print("|---|---|---|")
        for name in names:
            rate = statistics.median(results[name])
            share = f"{100 * rate / release:.0f}%" if release else "?"
            print(f"| {name} | {rate:.0f} | {share} |")
    finally:
        for name in names:
            home = work / f"home-{name}"
            if not home.exists():
                continue
            stop = dict(os.environ, BLAZE_HOME=str(home), PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}")
            try:
                proc = await asyncio.create_subprocess_exec("blaze", "stop", env=stop, stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL)
                await asyncio.wait_for(proc.wait(), 30)
            except (OSError, TimeoutError):
                pass
        if not os.environ.get("TIERBENCH_KEEP"):
            shutil.rmtree(work, ignore_errors=True)
        else:
            print(f"kept {work}")


if __name__ == "__main__":
    if sys.platform != "darwin" or os.uname().machine != "arm64":
        print("tierbench.py: live edits run on macOS on AArch64 only; nothing to measure here")
        sys.exit(0)
    try:
        asyncio.run(main())
    except tiers.StepError as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
