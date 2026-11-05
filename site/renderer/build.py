# /// script
# requires-python = ">=3.13"
# ///
"""Builds the renderer's two wasm modules, the only Rust artifacts the site needs. Both are platform-independent:
build them once (CI does, on every run of the site workflow) and fetch them anywhere with renderer/fetch.py.

    uv run --script renderer/build.py [--out DIR]     # from site/; the Rust toolchain (rust-toolchain.toml) and clang

    <out>/site.wasm            site-wasm: the renderer the worker runs and gen/build.ts renders the static pages with
    <out>/site-snapshot.wasm   site-snapshot: content scripts to snapshot, with Flecs's C inside (build time only)

`<out>` defaults to renderer/target/site/, where gen/build.ts looks. Each is built with the `wasm` profile (opt-level
z, LTO, one codegen unit, panic=abort), then `wasm-opt -Oz` if wasm-opt is on PATH (else copied with a warning).

Flecs's C is compiled by clang for wasm32 (flecs-c's build.rs): set CC_wasm32_unknown_unknown and
AR_wasm32_unknown_unknown to an unwrapped clang and llvm-ar when the default `clang` is a wrapper that adds host flags
(nix).
"""

import argparse
import asyncio
import gzip
import shutil
import sys
from pathlib import Path

RENDERER = Path(__file__).resolve().parent
TARGET = RENDERER / "target"
WASM_TARGET = "wasm32-unknown-unknown"
# The features rustc's wasm32 target emits by default; V8 (Workers) and JavaScriptCore (Bun) have them all.
FEATURES = [
    "--enable-bulk-memory",
    "--enable-nontrapping-float-to-int",
    "--enable-sign-ext",
    "--enable-mutable-globals",
    "--enable-multivalue",
    "--enable-reference-types",
]


class StepError(Exception):
    pass


async def run(step: str, *cmd: str, bound: float = 1800) -> None:
    """Runs `cmd` in renderer/, its stderr shown as it comes; on failure names the step, the command, its exit code
    and the stderr tail."""
    proc = await asyncio.create_subprocess_exec(*cmd, cwd=RENDERER, stderr=asyncio.subprocess.PIPE)
    assert proc.stderr is not None
    tail: list[str] = []
    try:
        async with asyncio.timeout(bound):
            while line := await proc.stderr.readline():
                text = line.decode(errors="replace").rstrip()
                print(f"[{step}] {text}", file=sys.stderr)
                tail = [*tail, text][-30:]
            await proc.wait()
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise StepError(f"{step}: {' '.join(cmd)}: no exit after {bound:.0f} s") from None
    if proc.returncode != 0:
        raise StepError(f"{step}: {' '.join(cmd)}: exit {proc.returncode}\n" + "\n".join(tail))


async def module(package: str, out: Path) -> None:
    # Its own cargo invocation: in one, site-render's features would unify and the worker's module would get Flecs.
    await run(package, "cargo", "build", "--locked", "--profile", "wasm", "-p", package, "--target", WASM_TARGET)
    raw = TARGET / WASM_TARGET / "wasm" / f"{package.replace('-', '_')}.wasm"
    if wasm_opt := shutil.which("wasm-opt"):
        await run(f"wasm-opt {package}", wasm_opt, "-Oz", *FEATURES, "--strip-debug", "--strip-producers", str(raw), "-o", str(out))
    else:
        print(f"build.py: wasm-opt not on PATH: {out.name} is not wasm-opt'd", file=sys.stderr)
        shutil.copyfile(raw, out)
    data = out.read_bytes()
    print(f"{out.name}: {raw.stat().st_size} bytes from cargo, {len(data)} bytes, {len(gzip.compress(data, 9))} gzipped")


async def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, default=TARGET / "site")
    a = ap.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)
    # One after the other: both share target/, and cargo's lock would serialise them anyway.
    await module("site-wasm", a.out / "site.wasm")
    await module("site-snapshot", a.out / "site-snapshot.wasm")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except* StepError as eg:
        for e in eg.exceptions:
            print(e, file=sys.stderr)
        sys.exit(1)
