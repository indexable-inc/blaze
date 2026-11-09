"""Build blaze the one way it ships: with the stage-0 blaze (.github/blaze-version), the `dist`
profile, every binary of blaze-engine. release.yaml and install.yaml run it; blaze builds blaze,
never cargo or rustc (AGENTS.md rule blaze-builds-blaze).

One typed input (rule scripts-take-typed-input):

  uv run --frozen --no-dev python .github/scripts/build_release.py \\
      '{"target": "x86_64-unknown-linux-gnu", "out": "shipped"}'

The build is `blaze build -p blaze-engine --profile dist --locked --target T`, run by `blaze`
(the stage 0 .github/actions/blaze installs), against the LLVM `LLVM_SYS_231_PREFIX` names (the
pin crates/hir-ty-codegen/llvm.json, which .github/actions/blaze fetches below /usr/local/lib:
a build script in blaze's sandbox may name only system directories of libraries). T must be the
host's: every release target builds on a machine of its own kind, nothing is cross-compiled.

`dist` is `release` with `lto = "thin"` and `codegen-units = 1`. blaze's release codegen is one
LLVM unit per function with ThinLTO's summaries and imports across the whole program
(docs/v2/plan/design/codegen.md, "Release"), so what thin LTO and one unit per crate ask of rustc, inlining
across functions and crates, is what blaze's release build always does; both settings are
in the units' keys.

PGO (`"pgo": "pinned"`): blaze builds with `-C profile-use` of the profile tools/pgo/profiles.json
pins for T, downloaded and checked by its sha256 (or `profdata`, the same file already fetched).
Only a profile trained on blaze's own build of blaze applies (`"builder": "blaze"`, made by
tools/pgo/pgo.py with the LLVM pinned now): a profile of cargo's build names rustc's symbols and
LLVM's format of another LLVM. pgo.py cannot train one yet, since blaze refuses
`-C profile-generate` (its linker writes no profile sections: crates/blaze-cargo/src/serve/unit/
codegen.rs, `pgo`), so today `pinned` stops with that error and releases ship `"pgo": "off"`.

The binaries are refused when one loads libLLVM, or on Linux a C++ runtime (a machine that
downloads them has neither), and on Linux when one asks for a glibc newer than `GLIBC` (RHEL 9's;
the release builds on the oldest hosted Ubuntu; install.sh says the same floor).

Every step is bounded, and a failure stops the script naming the step, the command, its exit
status and the tail of what it printed; nothing is skipped or done another way.
"""

import asyncio
import collections
import hashlib
import json
import os
import platform
import re
import shutil
import sys
import tomllib
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Literal, NoReturn

SCRIPT = ".github/scripts/build_release.py"
ROOT = Path(__file__).resolve().parents[2]
ENGINE = ROOT / "crates" / "blaze-engine"
PACKAGE = "blaze-engine"
PROFILE = "dist"
PINS = ROOT / "tools" / "pgo" / "profiles.json"
LLVM_PIN = ROOT / "crates" / "hir-ty-codegen" / "llvm.json"
LLVM_PREFIX = "LLVM_SYS_231_PREFIX"
# The newest glibc a Linux binary of a release may ask for (install.sh's `glibc_floor`), and how a
# binary names the versions it asks for (in its dynamic string table).
GLIBC = (2, 34)
GLIBC_VERSION = re.compile(rb"GLIBC_(\d+)\.(\d+)(?:\.\d+)?\x00")
# The libraries no shipped binary may load: LLVM is linked in, and on Linux libstdc++ with it.
NOT_LOADED = {"linux": re.compile(r"libLLVM|libstdc\+\+|libc\+\+"), "darwin": re.compile(r"libLLVM")}
# Flags from the environment would change what ships without the recipe saying so.
FOREIGN_FLAGS = ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER")
HOSTS = {
    ("linux", "x86_64"): "x86_64-unknown-linux-gnu",
    ("linux", "aarch64"): "aarch64-unknown-linux-gnu",
    ("darwin", "arm64"): "aarch64-apple-darwin",
}
TAIL_LINES = 80


@dataclass(frozen=True)
class Build:
    # The Rust target triple, which must be the host's.
    target: str
    # Where every binary of blaze-engine is copied; created when missing.
    out: str
    # `off`: no profile. `pinned`: the blaze-trained profile tools/pgo/profiles.json pins for T.
    pgo: Literal["off", "pinned"] = "off"
    # With `pinned`: the pinned profile already fetched, in place of a download.
    profdata: str = ""
    # The blaze that builds blaze: the stage 0 on PATH.
    blaze: str = "blaze"
    # The target directory; `target` in the checkout when empty.
    target_dir: str = ""
    # The bound of the build, in seconds: a cold store of blaze's release builds for about 97
    # minutes on hosted Linux x86-64 and longer on macOS (blaze-platform.yaml's build job).
    build_secs: int = 4 * 3600
    # The bound of `blaze stop` after the build, and of each check of a binary.
    stop_secs: int = 120
    check_secs: int = 60
    download_secs: int = 300


class Failed(Exception):
    """A step that did not do what it must, naming it."""


def fail(message: str) -> NoReturn:
    sys.exit(f"{SCRIPT}: {message}")


@dataclass(frozen=True)
class Ran:
    code: int
    stdout: str
    tail: str


async def run(
    step: str, cmd: list[str], *, secs: float, env: dict[str, str] | None = None, echo: bool = False
) -> Ran:
    """`cmd` run to its end within `secs`. With `echo`, each line it prints is passed on as it
    comes (a CI log shows a slow build while it is slow) and only the last `TAIL_LINES` kept.
    A nonzero exit or the bound running out is a `Failed` naming the step."""
    shown = " ".join(cmd)
    print(f"+ {shown}", flush=True)
    proc = await asyncio.create_subprocess_exec(
        *cmd, cwd=ROOT, env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT
    )
    assert proc.stdout is not None
    tail: collections.deque[str] = collections.deque(maxlen=TAIL_LINES)
    kept: list[str] = []

    async def pump() -> None:
        assert proc.stdout is not None
        async for raw in proc.stdout:
            line = raw.decode(errors="replace")
            tail.append(line)
            if echo:
                sys.stdout.write(line)
                sys.stdout.flush()
            else:
                kept.append(line)
        await proc.wait()

    try:
        async with asyncio.timeout(secs):
            await pump()
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise Failed(
            f"{step}: `{shown}` did not end in {secs:.0f} s; its last lines:\n{''.join(tail)}"
        ) from None
    code = proc.returncode if proc.returncode is not None else -1
    if code != 0:
        said = "".join(tail)
        raise Failed(f"{step}: `{shown}` exited {code}; its last lines:\n{said}{daemon_log(said)}")
    return Ran(code, "".join(kept), "".join(tail))


# The daemon's log, as blaze's client names it when the daemon did not start or listen ("its log
# is <path>", "its log <path> says why"). The daemon says there why it ended; the client does not.
DAEMON_LOG = re.compile(r"its log (?:is )?(/\S+?serve\.log)")
DAEMON_LOG_LINES = 60


def daemon_log(said: str) -> str:
    """The last lines of the daemon log `said` names, or nothing when it names none."""
    found = DAEMON_LOG.search(said)
    if found is None:
        return ""
    path = Path(found.group(1))
    try:
        lines = path.read_text(errors="replace").splitlines()[-DAEMON_LOG_LINES:]
    except OSError as error:
        return f"\nthe daemon's log {path}: {error}"
    return f"\nthe daemon's log {path}, its last lines:\n" + "\n".join(lines) + "\n"


def host() -> str:
    key = (sys.platform, platform.machine())
    found = HOSTS.get(key)
    if found is None:
        raise Failed(
            f"host: blaze ships no release for {key[0]} on {key[1]} (the releases' hosts: "
            f"{', '.join(sorted(HOSTS.values()))})"
        )
    return found


def sha256(path: Path) -> str:
    with open(path, "rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


def binaries() -> list[str]:
    """The binaries blaze-engine builds: its `[[bin]]`s and, unless `autobins` is off, the files of
    `src/bin`. What ships is all of them."""
    manifest = tomllib.loads((ENGINE / "Cargo.toml").read_text())
    names = [b["name"] for b in manifest.get("bin", [])]
    if manifest.get("package", {}).get("autobins", True):
        names += [p.stem for p in sorted((ENGINE / "src" / "bin").glob("*.rs")) if p.stem not in names]
    return sorted(names)


async def pinned_profile(build: Build) -> Path:
    """The profile tools/pgo/profiles.json pins for the target, checked by its sha256; refused
    unless blaze's own build of blaze trained it with the LLVM pinned now."""
    table = json.loads(PINS.read_text())
    pin = table["targets"].get(build.target)
    if pin is None:
        raise Failed(f"pgo: {PINS} pins no profile for {build.target}: tools/pgo/pgo.py trains one")
    if pin.get("builder") != "blaze":
        raise Failed(
            f"pgo: {PINS}'s profile for {build.target} ({pin['name']}) was trained on cargo's build of blaze, "
            "whose symbols and LLVM are rustc's: blaze builds blaze (rule blaze-builds-blaze) and applies only a "
            "profile tools/pgo/pgo.py trained on blaze's own build. pgo.py cannot train one yet: blaze refuses "
            "`-C profile-generate` because its linker writes no profile sections (`__llvm_prf_*` and the profile "
            'runtime; crates/blaze-cargo/src/serve/unit/codegen.rs, `pgo`). Build with "pgo": "off" until it does'
        )
    llvm = json.loads(LLVM_PIN.read_text())["commit"]
    if pin.get("llvm") != llvm:
        raise Failed(
            f"pgo: {PINS}'s profile for {build.target} was made by LLVM {pin.get('llvm')}, blaze links "
            f"{llvm} ({LLVM_PIN}): tools/pgo/pgo.py trains one with this LLVM"
        )
    if build.profdata:
        path = Path(build.profdata).resolve()
    else:
        path = ROOT / "target" / "pgo" / pin["name"]
        if not path.is_file() or sha256(path) != pin["sha256"]:
            url = f"{table['release']}/{pin['name']}"
            print(f"build_release: downloading {url}", flush=True)
            path.parent.mkdir(parents=True, exist_ok=True)

            def fetch() -> None:
                with urllib.request.urlopen(url, timeout=build.download_secs) as got, open(path, "wb") as f:
                    shutil.copyfileobj(got, f)

            try:
                async with asyncio.timeout(build.download_secs):
                    await asyncio.to_thread(fetch)
            except (TimeoutError, OSError) as e:
                raise Failed(f"pgo: the download of {url} failed: {e!r}") from None
    if sha256(path) != pin["sha256"]:
        raise Failed(
            f"pgo: {path} is not the profile {PINS} pins for {build.target} (sha256 {pin['sha256']})"
        )
    return path


def build_env(build: Build, target_dir: Path, profile: Path | None) -> dict[str, str]:
    """The environment of the build: the caller's, with the target directory, the pinned LLVM and
    the profile's flag, and none of the flags or wrappers a caller's environment could add."""
    if set_ := [name for name in FOREIGN_FLAGS if os.environ.get(name)]:
        raise Failed(
            f"environment: {', '.join(set_)} set; the release's recipe takes no flags or wrappers from the "
            "environment, as they would change what ships without it saying so"
        )
    prefix = os.environ.get(LLVM_PREFIX, "")
    if not prefix:
        raise Failed(
            f"llvm: {LLVM_PREFIX} is unset; blaze-engine's `native` links the pinned LLVM "
            f"({LLVM_PIN}), which .github/actions/blaze (`llvm: true`) fetches below /usr/local/lib "
            "and names there"
        )
    if not Path(prefix, "bin").is_dir():
        raise Failed(f"llvm: {LLVM_PREFIX}={prefix} has no bin/: it is not an unpacked LLVM of {LLVM_PIN}")
    env = {k: v for k, v in os.environ.items() if k not in FOREIGN_FLAGS}
    env["CARGO_TARGET_DIR"] = str(target_dir)
    env["CARGO_INCREMENTAL"] = "0"
    if profile is not None:
        env["CARGO_ENCODED_RUSTFLAGS"] = f"-Cprofile-use={profile}"
    return env


def glibc_asked(data: bytes) -> tuple[int, int] | None:
    """The newest glibc a binary asks for, or None for one that asks for none."""
    found = {(int(major), int(minor)) for major, minor in GLIBC_VERSION.findall(data)}
    return max(found, default=None)


async def check_binary(build: Build, binary: Path) -> list[str]:
    """What is wrong with one shipped binary: a library it loads that a user's machine lacks, or a
    glibc newer than the floor."""
    data = binary.read_bytes()
    wrong: list[str] = []
    if data[:4] == b"\x7fELF":
        shown = (
            await run(f"check {binary.name}", ["readelf", "-d", str(binary)], secs=build.check_secs)
        ).stdout
        lines = [line for line in shown.splitlines() if "(NEEDED)" in line]
        wrong += [f"{binary.name} loads {line.strip()}" for line in lines if NOT_LOADED["linux"].search(line)]
        asked = glibc_asked(data)
        if asked and asked > GLIBC:
            wrong.append(
                f"{binary.name} asks for glibc {'.'.join(map(str, asked))}, newer than RHEL 9's "
                f"{'.'.join(map(str, GLIBC))}, so it does not start there: build on Ubuntu 22.04, as the "
                "release does"
            )
    elif data[:4] == b"\xcf\xfa\xed\xfe":
        shown = (
            await run(f"check {binary.name}", ["otool", "-L", str(binary)], secs=build.check_secs)
        ).stdout
        lines = shown.splitlines()[1:]
        wrong += [
            f"{binary.name} loads {line.strip()}" for line in lines if NOT_LOADED["darwin"].search(line)
        ]
    else:
        wrong.append(f"{binary.name} is neither ELF nor Mach-O")
    return wrong


async def main_async(build: Build) -> None:
    if build.target != host():
        raise Failed(
            f"target: {build.target} is not this machine's ({host()}); each release target builds on a "
            "machine of its own kind, and blaze cross-compiles no release"
        )
    if not shutil.which(build.blaze):
        raise Failed(
            f"blaze: `{build.blaze}` is not on PATH; .github/actions/blaze installs the stage 0 "
            ".github/blaze-version names"
        )
    said = await run("blaze --version", [build.blaze, "--version"], secs=build.check_secs)
    print(f"build_release: stage 0 is {said.stdout.strip()}", flush=True)
    profile = await pinned_profile(build) if build.pgo == "pinned" else None
    target_dir = Path(build.target_dir).resolve() if build.target_dir else ROOT / "target"
    env = build_env(build, target_dir, profile)
    cmd = [build.blaze, "build", "-p", PACKAGE, "--profile", PROFILE, "--locked", "--target", build.target]
    try:
        await run("build", cmd, secs=build.build_secs, env=env, echo=True)
    except Failed as built_wrong:
        # The daemon ends either way: its last pack seals, and its memory is the next step's. The
        # build's failure is the one said; a stop that fails too is said after it.
        try:
            await run("blaze stop", [build.blaze, "stop"], secs=build.stop_secs, env=env, echo=True)
        except Failed as stop_wrong:
            raise Failed(f"{built_wrong}\nand then {stop_wrong}") from None
        raise
    await run("blaze stop", [build.blaze, "stop"], secs=build.stop_secs, env=env, echo=True)

    built = target_dir / build.target / PROFILE
    out = Path(build.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    shipped: list[Path] = []
    for name in binaries():
        path = built / name
        if not path.is_file():
            raise Failed(f"build: `{' '.join(cmd)}` ended well and made no {path}")
        shutil.copy2(path, out / name)
        shipped.append(out / name)
    async with asyncio.TaskGroup() as group:
        checks = [group.create_task(check_binary(build, binary)) for binary in shipped]
    wrong = [item for task in checks for item in task.result()]
    if wrong:
        raise Failed(
            f"check: a shipped binary would not run on a user's machine: {'; '.join(wrong)} (LLVM and, on "
            "Linux, libstdc++ are linked in statically: vendor/llvm-sys/build.rs)"
        )
    how = f"with the profile {profile.name}" if profile else "without PGO"
    print(
        f"build_release: {', '.join(p.name for p in shipped)} in {out}, the `{PROFILE}` profile {how}, built by "
        f"{said.stdout.strip()}; each loads no libLLVM"
        + (
            ""
            if sys.platform == "darwin"
            else f" or C++ runtime and asks for glibc {'.'.join(map(str, GLIBC))} or older"
        ),
        flush=True,
    )


def main() -> None:
    if len(sys.argv) > 1 and sys.argv[1].startswith("--"):
        fail(
            f"`{' '.join(sys.argv[1:])}` is the retired cargo recipe's flags. The script takes one JSON object; "
            "blaze builds blaze (rule blaze-builds-blaze), so the flake's `dist` (flake.nix), which built blaze "
            "with cargo through those flags, has no recipe here; training a profile is tools/pgo/pgo.py's"
        )
    if len(sys.argv) != 2:
        fail("usage: build_release.py CONFIG, one JSON object (see the top of the file)")
    # Here, not at the top: the retired flags above are refused even where the workspace's
    # Python (and so typed_input) is not the one running this, as in the flake's build.
    import typed_input

    build = typed_input.parse(Build, sys.argv[1], SCRIPT)
    try:
        asyncio.run(main_async(build))
    except Failed as e:
        fail(str(e))
    except ExceptionGroup as group:
        failed, rest = group.split(Failed)
        if rest is not None or failed is None:
            raise
        fail("; ".join(str(e) for e in failed.exceptions))


if __name__ == "__main__":
    main()
