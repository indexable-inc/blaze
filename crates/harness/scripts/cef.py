"""Fetch the pinned CEF and bundle `Harness Browser.app` (crates/harness/harness-browser/README.md).

    uv run --frozen crates/harness/scripts/cef.py '{"op": "fetch"}'
    uv run --frozen crates/harness/scripts/cef.py '{"op": "bundle", "profile": "release"}'

`fetch` downloads the macOS arm64 minimal distribution of the CEF in `PIN` from Spotify's CEF build
server, checks its sha1 (the server's index) and sha256 (ours), and extracts it to
`~/Library/Caches/inc.indexable.harness/cef/<version>/cef_macos_aarch64/` with a `.verified` file
recording both: the directory `vendor/cef-dll-sys/build.rs` reads. A verified copy is never fetched
again. Nothing is installed on the host.

`bundle` makes `<target>/<profile>/Harness Browser.app` from the `harness-browser` binary cargo
built: the framework cloned (APFS clonefile) from the cache, five helper apps that are the same
binary, Info.plists, and an ad-hoc signature on every bundle, inside out.
"""

import asyncio
import hashlib
import os
import plistlib
import shutil
import sys
import tarfile
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from typed_input import fail, parse

SCRIPT = "cef.py"
HARNESS = Path(__file__).resolve().parents[1]


@dataclass(frozen=True)
class Pin:
    version: str
    archive: str
    sha1: str
    sha256: str


# CEF 156 stable (2026-10-31). vendor/cef-dll-sys/build.rs names the same version and sha256, and the
# `cef` crate in harness-browser/Cargo.toml (=156.3.0+156.0.15) binds this CEF's C API; move all three together.
PIN = Pin(
    version="156.0.15+gcf671c0+chromium-156.0.8078.80",
    archive="cef_binary_156.0.15+gcf671c0+chromium-156.0.8078.80_macosarm64_minimal.tar.bz2",
    sha1="bcdd1877edb300b0fb91d8f71c0b14aae256cd0f",
    sha256="a27f1d856e84047657ed2a14caf355fb04480c4fe1963f04249a8a3184af1c30",
)
SERVER = "https://cef-builds.spotifycdn.com/"
FRAMEWORK = "Chromium Embedded Framework.framework"
APP = "Harness Browser"
BUNDLE_ID = "inc.indexable.harness.browser"
# What macOS shows when it asks the person to let the browser read where the Mac is.
LOCATION_WHY = "Pages you open in harness, and agents you ask, can use where this Mac is."
# CEF's helper kinds on macOS: the suffix Chromium appends to the helper's name, and our bundle id's. No "(Alerts)"
# helper: it exists only to post macOS notifications, which the host turns off (harness-browser/src/handlers.rs), and its
# first launch makes macOS ask the person for notification permission. Anything that still asks for it fails to launch.
HELPERS = (
    ("", ""),
    (" (GPU)", ".gpu"),
    (" (Renderer)", ".renderer"),
    (" (Plugin)", ".plugin"),
)


@dataclass(frozen=True)
class Input:
    op: Literal["fetch", "bundle"]
    # bundle: the cargo profile directory under the target dir.
    profile: str = "release"
    # bundle: the cargo target dir, default crates/harness/target.
    target_dir: str | None = None
    # fetch: the cache root, default ~/Library/Caches/inc.indexable.harness/cef (CEF_PATH for cef-dll-sys).
    cache: str | None = None


class StepError(Exception):
    """A step that failed, naming the step and what refused it."""


def cache_root(given: str | None) -> Path:
    if given is not None:
        return Path(given)
    return Path.home() / "Library/Caches/inc.indexable.harness/cef"


def verified_line() -> str:
    return f"sha256 {PIN.sha256}"


def is_verified(dist: Path) -> bool:
    marker = dist / ".verified"
    return marker.is_file() and verified_line() in marker.read_text().splitlines()


def download(url: str, dest: Path) -> tuple[str, str]:
    """`url` written to `dest`; its (sha1, sha256). Runs in a worker thread."""
    sha1, sha256 = hashlib.sha1(), hashlib.sha256()
    with urllib.request.urlopen(url, timeout=60) as resp, dest.open("wb") as out:
        while chunk := resp.read(1 << 20):
            sha1.update(chunk)
            sha256.update(chunk)
            out.write(chunk)
    return sha1.hexdigest(), sha256.hexdigest()


def extract(archive: Path, into: Path) -> Path:
    """The archive's one top directory, extracted under `into` (the `data` filter: no path escapes)."""
    with tarfile.open(archive, "r:bz2") as tar:
        tops = {Path(m.name).parts[0] for m in tar.getmembers() if m.name}
        if len(tops) != 1:
            raise StepError(f"extract {archive}: expected one top directory, found {sorted(tops)}")
        tar.extractall(into, filter="data")
    return into / tops.pop()


async def fetch(inp: Input) -> None:
    version_dir = cache_root(inp.cache) / PIN.version
    dist = version_dir / "cef_macos_aarch64"
    if is_verified(dist):
        print(f"cef: {PIN.version} already at {dist}")
        return
    version_dir.mkdir(parents=True, exist_ok=True)
    work = version_dir / ".fetch"
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir()
    url = SERVER + urllib.parse.quote(PIN.archive)
    archive = work / PIN.archive
    print(f"cef: downloading {url}")
    try:
        async with asyncio.timeout(1800):
            sha1, sha256 = await asyncio.to_thread(download, url, archive)
    except TimeoutError as e:
        raise StepError(f"download {url}: no complete archive after 30 minutes") from e
    except OSError as e:
        raise StepError(f"download {url}: {e}") from e
    if sha1 != PIN.sha1:
        raise StepError(f"download {url}: sha1 {sha1}, the pin says {PIN.sha1}")
    if sha256 != PIN.sha256:
        raise StepError(f"download {url}: sha256 {sha256}, the pin says {PIN.sha256}")
    async with asyncio.timeout(600):
        top = await asyncio.to_thread(extract, archive, work)
    if not (top / "Release" / FRAMEWORK).is_dir():
        raise StepError(f"extract {archive}: no Release/{FRAMEWORK} in {top}")
    shutil.rmtree(dist, ignore_errors=True)
    top.rename(dist)
    (dist / ".verified").write_text(f"{verified_line()}\nsha1 {PIN.sha1}\nversion {PIN.version}\n")
    shutil.rmtree(work)
    print(f"cef: {PIN.version} verified at {dist}")


async def run(step: str, *cmd: str, timeout: float = 300) -> None:
    proc = await asyncio.create_subprocess_exec(
        *cmd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
    )
    try:
        async with asyncio.timeout(timeout):
            _, err = await proc.communicate()
    except TimeoutError as e:
        proc.kill()
        raise StepError(f"{step}: `{' '.join(cmd)}` still running after {timeout:.0f} s") from e
    if proc.returncode != 0:
        tail = err.decode(errors="replace").strip().splitlines()[-15:]
        raise StepError(f"{step}: `{' '.join(cmd)}` exited {proc.returncode}:\n" + "\n".join(tail))


def info_plist(name: str, ident: str) -> bytes:
    return plistlib.dumps(
        {
            "CFBundleDevelopmentRegion": "en",
            "CFBundleDisplayName": name,
            "CFBundleExecutable": name,
            "CFBundleIdentifier": ident,
            "CFBundleInfoDictionaryVersion": "6.0",
            "CFBundleName": name,
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": PIN.version.split("+")[0],
            "CFBundleVersion": PIN.version.split("+")[0],
            "LSMinimumSystemVersion": "13.0",
            # No Dock icon and no menu bar: the host draws nothing of its own but the manage window.
            "LSUIElement": True,
            # Chromium reads the Mac's position through Core Location, which answers only an app whose plist says
            # why: without these a page's navigator.geolocation (and ts location()) times out and macOS never asks.
            "NSLocationUsageDescription": LOCATION_WHY,
            "NSLocationWhenInUseUsageDescription": LOCATION_WHY,
            "NSPrincipalClass": "NSApplication",
            "NSSupportsAutomaticGraphicsSwitching": True,
        }
    )


def make_app(app: Path, name: str, ident: str, binary: Path) -> None:
    (app / "Contents/MacOS").mkdir(parents=True)
    (app / "Contents/Info.plist").write_bytes(info_plist(name, ident))
    exe = app / "Contents/MacOS" / name
    shutil.copy2(binary, exe)
    exe.chmod(0o755)


async def bundle(inp: Input) -> None:
    target = Path(inp.target_dir) if inp.target_dir is not None else HARNESS / "target"
    out = target / inp.profile
    binary = out / "harness-browser"
    if not binary.is_file():
        raise StepError(
            f"bundle: no {binary}; build it first: cd crates/harness && cargo build "
            f"--profile {inp.profile} -p harness-browser"
        )
    dist = cache_root(inp.cache) / PIN.version / "cef_macos_aarch64"
    if not is_verified(dist):
        raise StepError(
            f'bundle: CEF {PIN.version} is not fetched at {dist}; run {SCRIPT} \'{{"op": "fetch"}}\''
        )
    app = out / f"{APP}.app"
    staging = out / f".{APP}.app.new"
    shutil.rmtree(staging, ignore_errors=True)
    make_app(staging, APP, BUNDLE_ID, binary)
    frameworks = staging / "Contents/Frameworks"
    frameworks.mkdir()
    # macOS's `/bin/cp -c` clones (APFS clonefile): the 300 MB framework costs no space and no copy time. Not `cp` from
    # PATH: GNU coreutils (nix) has no `-c`.
    await run("clone framework", "/bin/cp", "-cR", str(dist / "Release" / FRAMEWORK), str(frameworks / FRAMEWORK))
    helpers: list[Path] = []
    for suffix, ident in HELPERS:
        name = f"{APP} Helper{suffix}"
        helper = frameworks / f"{name}.app"
        make_app(helper, name, f"{BUNDLE_ID}.helper{ident}", binary)
        helpers.append(helper)
    # Inside out: the framework, then each helper, then the app that holds them. Independent bundles in parallel.
    sign = ("codesign", "--force", "--sign", "-", "--timestamp=none")
    await run("sign framework", *sign, str(frameworks / FRAMEWORK))
    async with asyncio.TaskGroup() as tg:
        for helper in helpers:
            tg.create_task(run(f"sign {helper.name}", *sign, str(helper)))
    await run(f"sign {APP}.app", *sign, str(staging))
    await run(f"verify {APP}.app", "codesign", "--verify", "--deep", "--strict", str(staging))
    shutil.rmtree(app, ignore_errors=True)
    os.rename(staging, app)
    print(f"cef: bundled {app}")


async def main() -> None:
    inp = parse(Input, sys.argv[1] if len(sys.argv) > 1 else "{}", SCRIPT)
    if sys.platform != "darwin" or os.uname().machine != "arm64":
        fail(SCRIPT, f"the pinned CEF is the macOS arm64 build; this is {sys.platform} {os.uname().machine}")
    try:
        match inp.op:
            case "fetch":
                await fetch(inp)
            case "bundle":
                await bundle(inp)
    except* StepError as group:
        fail(SCRIPT, "\n".join(str(e) for e in group.exceptions))


if __name__ == "__main__":
    asyncio.run(main())
