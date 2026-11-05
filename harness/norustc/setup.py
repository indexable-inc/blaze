#!/usr/bin/env python3
"""setup.py: prepares a root for count.py on Linux: blaze at a commit, built by a blaze given
(stage 0, never cargo: AGENTS.md rule blaze-builds-blaze), and a copy of blaze's own toolchain
whose rustc, rustdoc and cargo log every run (wrap.sh). That copy is what count.py measures
with: blaze is to run none of the three.

    uv run --frozen python harness/norustc/setup.py '{"stage0": "/path/to/blaze"}'
    uv run --frozen python harness/norustc/setup.py \\
        '{"stage0": "/path/to/blaze", "root": "~/no-rustc", "rev": "origin/main", "cpus": "96-191"}'

The input is one JSON object, inline, the path of a file, or `-` for standard input:
  stage0  a blaze binary that builds the checkout (a release, or a blaze built earlier)
  root    where everything goes (default ~/no-rustc); count.py, classify.py and left.py read it
          from NR_ROOT
  rev     the commit of blaze to count (default origin/main)
  cpus    the cores the build may use (`taskset -c`), "" for any

In order, each step naming itself, its command, its exit status and its stderr's tail when it
fails:
  1. `src/`: blaze's checkout, cloned once, fetched and detached at `rev`;
  2. `bin/`: `blaze build -p blaze-engine --release --bins` of it by stage 0, in a home of its
     own (`stage0-home/`, kept, so the next setup reuses its store), and the `blaze` and
     `blaze-engine` it printed (`compiler-artifact` lines) copied here: count.py puts this
     first on PATH;
  3. `tc-pin/`: a copy of the toolchain the built blaze names (`blaze toolchain --json`'s
     sysroot, downloaded once into the user's `~/.blaze/toolchains`), its `bin/rustc`, `bin/rustdoc` and
     `bin/cargo` replaced by wrap.sh with the real tool beside each as `<tool>.real`;
  4. `setup.json`: the commit, the built blaze and its sha256, and the toolchain's directory
     name. count.py links `tc-pin` under that name into a user's home of each run's own
     (`.blaze/toolchains/<name>`, where blaze looks for its toolchain whatever BLAZE_HOME says),
     which the run's HOME names, and puts `tc-pin/bin` on PATH,
     so a run of any of the three, by blaze or by anything it starts, is logged.

Linux only: wrap.sh reads /proc and takes flock. Remove it all with `rm -rf` of the root.
"""

import asyncio
import hashlib
import json
import os
import shlex
import shutil
import sys
from dataclasses import dataclass
from pathlib import Path

from typed_input import fail, parse

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
from storelib import SCRUBBED, artifact, blaze_toolchain_of
from storelib import Failed as Refused

SCRIPT = "norustc/setup.py"
REPO = "https://github.com/indexable-inc/blaze"
PACKAGE = "blaze-engine"
PROGRAMS = ("blaze", "blaze-engine")
COUNTED = ("rustc", "rustdoc", "cargo")
WRAP = HERE / "wrap.sh"
# What the copy of the toolchain was made from, in it: its directory name.
FROM = ".nr-from"
GIT_S, BUILD_S, STOP_S, TOOLCHAIN_S = 900, 10800, 300, 900


@dataclass(frozen=True)
class Setup:
    stage0: str
    root: str = "~/no-rustc"
    rev: str = "origin/main"
    cpus: str = ""


class Failed(Exception):
    """A step that could not be done, said whole."""


async def run(step: str, argv: list[str], *, bound: float, cwd: Path | None = None,
              env: dict[str, str] | None = None) -> str:
    """Runs `argv` to its end and returns its stdout; a failure names the step, the command,
    its exit status and the tail of its stderr."""
    command = shlex.join(argv)
    try:
        proc = await asyncio.create_subprocess_exec(
            *argv, cwd=cwd, env=env, stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    except OSError as error:
        raise Failed(f"{step}: `{command}` did not start: {error}") from None
    try:
        async with asyncio.timeout(bound):
            out, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise Failed(f"{step}: `{command}` took over {bound:.0f} s") from None
    if proc.returncode != 0:
        tail = "\n".join(err.decode(errors="replace").splitlines()[-25:])
        raise Failed(f"{step}: `{command}` exited {proc.returncode}:\n{tail}")
    return out.decode(errors="replace")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as f:
        while chunk := f.read(1 << 20):
            digest.update(chunk)
    return digest.hexdigest()


def counted_copy(sysroot: Path, counted: Path) -> None:
    """`counted` as a copy of the toolchain at `sysroot` whose rustc, rustdoc and cargo are
    wrap.sh, made once per toolchain: a copy of another is made again."""
    if counted.is_dir():
        made = counted / FROM
        if made.is_file() and made.read_text() == sysroot.name:
            missing = [t for t in COUNTED if not (counted / "bin" / f"{t}.real").is_file()]
            if missing:
                raise Failed(f"toolchain: {counted} has no real {', '.join(missing)} beside wrap.sh: "
                             "remove it and run setup.py again")
            return
        shutil.rmtree(counted)
    part = counted.with_name(counted.name + ".part")
    shutil.rmtree(part, ignore_errors=True)
    shutil.copytree(sysroot, part, symlinks=True)
    for tool in COUNTED:
        stand_in = part / "bin" / tool
        if not stand_in.is_file():
            raise Failed(f"toolchain: {sysroot} has no bin/{tool} to count")
        stand_in.rename(stand_in.with_name(f"{tool}.real"))
        shutil.copy2(WRAP, stand_in)
        stand_in.chmod(0o755)
    (part / FROM).write_text(sysroot.name)
    part.rename(counted)


def placed(source: str, into: Path) -> None:
    """`source` copied to `into`, replacing what was there at once."""
    part = into.with_name(into.name + ".part")
    shutil.copy2(source, part)
    os.replace(part, into)


async def prepare(setup: Setup) -> None:
    root = Path(setup.root).expanduser().resolve()
    stage0 = Path(setup.stage0).expanduser().resolve()
    if not (stage0.is_file() and os.access(stage0, os.X_OK)):
        raise Failed(f"stage0: no executable blaze at {stage0}")
    pin: list[str] = []
    if setup.cpus:
        taskset = shutil.which("taskset")
        if taskset is None:
            raise Failed(f"cpus {setup.cpus!r}: no taskset on PATH")
        pin = [taskset, "-c", setup.cpus]
    for directory in ("logs", "apps", "bin"):
        (root / directory).mkdir(parents=True, exist_ok=True)

    src = root / "src"
    if not src.is_dir():
        await run("clone", ["git", "clone", "-q", REPO, str(src)], bound=GIT_S)
    await run("fetch", ["git", "-C", str(src), "fetch", "-q", "origin"], bound=GIT_S)
    await run("checkout", ["git", "-C", str(src), "checkout", "-q", "--detach", setup.rev], bound=GIT_S)
    commit = (await run("rev-parse", ["git", "-C", str(src), "rev-parse", "HEAD"], bound=GIT_S)).strip()

    env = {name: value for name, value in os.environ.items() if name not in SCRUBBED}
    build_env = dict(env, BLAZE_HOME=str(root / "stage0-home"), RUST_BACKTRACE="0")
    argv = [*pin, str(stage0), "build", "-p", PACKAGE, "--release", "--bins",
            "--message-format=json-render-diagnostics"]
    try:
        out = await run("build", argv, cwd=src, env=build_env, bound=BUILD_S)
    finally:
        await run("stop stage 0's daemon", [str(stage0), "stop"], cwd=src, env=build_env, bound=STOP_S)
    for program in PROGRAMS:
        executable: str | None = artifact(out, program)
        if not executable:
            raise Failed(f"build: `{shlex.join(argv)}` printed no executable of `{program}`")
        await asyncio.to_thread(placed, executable, root / "bin" / program)
    blaze = root / "bin" / "blaze"

    said = await run("toolchain", [str(blaze), "toolchain", "--json"], env=env, bound=TOOLCHAIN_S)
    toolchain = blaze_toolchain_of(said)
    sysroot = Path(toolchain.sysroot)
    counted = root / "tc-pin"
    await asyncio.to_thread(counted_copy, sysroot, counted)

    record = {"rev": commit, "blaze": str(blaze), "blaze_sha256": await asyncio.to_thread(sha256, blaze),
              "toolchain": sysroot.name, "nightly": toolchain.nightly, "counted": str(counted)}
    (root / "setup.json").write_text(json.dumps(record, indent=1, sort_keys=True) + "\n")
    print(f"{root}: blaze at {commit} ({record['blaze_sha256'][:16]}), counted {toolchain.name} in {counted}",
          flush=True)


def main() -> None:
    if len(sys.argv) != 2:
        fail(SCRIPT, "usage: setup.py JSON (see the top of the file)")
    setup = parse(Setup, sys.argv[1], SCRIPT)
    if sys.platform != "linux":
        fail(SCRIPT, f"Linux only (wrap.sh reads /proc and takes flock), not {sys.platform}")
    try:
        asyncio.run(prepare(setup))
    except (Failed, Refused) as error:
        fail(SCRIPT, str(error))


if __name__ == "__main__":
    main()
