#!/usr/bin/env python3
"""Open the Game of Life window; on macOS arm64, edits to its source show up live.

Started by the `blaze-life` wrapper in flake.nix, which passes everything in BLAZE_LIFE_*:

  SOURCE       store path holding demo/life and crates/blaze-live
  VENDOR       vendored crates.io sources for demo/life's Cargo.lock
  DAEMON_BINS  bin/ of blaze and the programs it runs (the flake's `dist`)
  PREBUILT     the demo built per cargo unit, run where live edits are not supported
  PATH, LIBS   toolchain (and linker) on PATH, and the GUI libraries to dlopen

The source is copied to a writable temporary directory. With live edits, the copy is built once
by cargo and the blaze-engine daemon, and started. From then on the window's editor sends each edit's
text straight to the daemon, which builds it from memory and patches the running window; the
editor writes the copy's src/main.rs after that, and nothing here runs again. Without live edits
(they are macOS on AArch64 only) the prebuilt window runs, and the edited copy is not used.
BLAZE_LIFE_DAEMON=1 forces the daemon path, BLAZE_LIFE_DAEMON=0 the prebuilt one.

Python 3.13, stdlib only.
"""

import asyncio
import os
import platform
import shutil
import signal
import sys
import tempfile
from pathlib import Path

BUILD_TIMEOUT = 1800  # the first build compiles eframe and its dependencies
READY_TIMEOUT = 600  # the daemon loads the workspace after the first build


class StepError(Exception):
    def __init__(self, message: str, output: str = "") -> None:
        super().__init__(message)
        self.output = output  # what the command said on stderr, for the window's error footer


def env(name: str) -> str:
    try:
        return os.environ[f"BLAZE_LIFE_{name}"]
    except KeyError:
        raise StepError(f"BLAZE_LIFE_{name} is not set: run this through the `blaze-life` wrapper") from None


async def run(step: str, cmd: list[str], timeout: float, cwd: Path, run_env: dict[str, str]) -> str:
    """Runs a command to completion; its stderr (where cargo reports) is returned."""
    try:
        async with asyncio.timeout(timeout):
            proc = await asyncio.create_subprocess_exec(
                *cmd, cwd=cwd, env=run_env, stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.PIPE
            )
            _, err = await proc.communicate()
    except TimeoutError:
        raise StepError(f"step '{step}' timed out after {timeout:.0f}s\n  command: {' '.join(cmd)}") from None
    text = err.decode(errors="replace")
    if proc.returncode != 0:
        tail = "\n".join(text.strip().splitlines()[-30:])
        raise StepError(
            f"step '{step}' failed\n  command: {' '.join(cmd)}\n  exit code: {proc.returncode}\n  stderr tail:\n{tail}", text
        )
    return text


def use_daemon() -> bool:
    forced = os.environ.get("BLAZE_LIFE_DAEMON")
    if forced is not None:
        return forced == "1"
    return sys.platform == "darwin" and platform.machine() == "arm64"


def child_env(target: Path) -> dict[str, str]:
    e = dict(os.environ)
    e["PATH"] = os.pathsep.join([env("DAEMON_BINS"), env("PATH"), e.get("PATH", "")])
    if libs := env("LIBS"):
        e["LD_LIBRARY_PATH"] = os.pathsep.join(filter(None, [libs, e.get("LD_LIBRARY_PATH", "")]))
    e["CARGO_TARGET_DIR"] = str(target)
    return e


def prepare(work: Path) -> Path:
    """Copies the sources somewhere writable and points cargo at the vendored crates."""
    source = Path(env("SOURCE"))
    shutil.copytree(source / "crates" / "blaze-live", work / "crates" / "blaze-live")
    shutil.copytree(source / "demo" / "life", work / "demo" / "life")
    for path in [work, *work.rglob("*")]:
        path.chmod(path.stat().st_mode | 0o700)
    life = work / "demo" / "life"
    (life / ".cargo").mkdir()
    (life / ".cargo" / "config.toml").write_text(
        '[source.crates-io]\nreplace-with = "vendored"\n\n'
        f'[source.vendored]\ndirectory = "{env("VENDOR")}"\n\n'
        "[net]\noffline = true\n"
    )
    return life


async def daemon_session(life: Path, run_env: dict[str, str], target: Path) -> None:
    build = ["blaze", "build", "--locked"]
    print("blaze: building the demo (the first build compiles eframe, a few minutes)", file=sys.stderr)
    await run("first build", build, BUILD_TIMEOUT, life, run_env)
    # The daemon loads the workspace after that build; until it has, a build is cargo's. A build
    # it served says `[blaze]`, and that one is the link a running program takes patches against.
    async with asyncio.timeout(READY_TIMEOUT):
        while "[blaze]" not in await run("wait for the daemon", build, BUILD_TIMEOUT, life, run_env):
            await asyncio.sleep(0.5)
    main_rs = life / "src" / "main.rs"
    print(f"blaze: type in the editor on the right ({main_rs}); the window changes as you type", file=sys.stderr)
    window = await asyncio.create_subprocess_exec(str(target / "debug" / "life"), cwd=life, env=run_env)
    await window.wait()


async def main() -> None:
    work = Path(tempfile.mkdtemp(prefix="blaze-life-"))
    cache = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "blaze-life"
    try:
        life = prepare(work)
        run_env = child_env(cache / "target")
        if use_daemon():
            await daemon_session(life, run_env, cache / "target")
        else:
            print("blaze: live edits need macOS on AArch64; running the prebuilt window", file=sys.stderr)
            window = await asyncio.create_subprocess_exec(env("PREBUILT"), cwd=life, env=run_env)
            await window.wait()
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    try:
        asyncio.run(main())
    except* StepError as eg:
        for e in eg.exceptions:
            print(f"error: {e}", file=sys.stderr)
        sys.exit(1)
