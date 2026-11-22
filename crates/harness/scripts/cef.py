"""Package harness's macOS apps and fetch the pinned CEF.

    uv run --frozen crates/harness/scripts/cef.py '{"op": "fetch"}'
    uv run --frozen crates/harness/scripts/cef.py '{"op": "bundle", "profile": "release"}'
    uv run --frozen crates/harness/scripts/cef.py '{"op": "install"}'

`fetch` downloads the macOS arm64 minimal distribution of the CEF in `PIN` from Spotify's CEF build
server, checks its sha1 (the server's index) and sha256 (ours), and extracts it to
`~/Library/Caches/inc.indexable.harness/cef/<version>/cef_macos_aarch64/` with a `.verified` file
recording both: the directory `vendor/cef-dll-sys/build.rs` reads. A verified copy is never fetched
again. Nothing is installed on the host.

`bundle` makes `<target>/<profile>/Harness Browser.app` from the `harness-browser` binary cargo
built: the framework cloned (APFS clonefile) from the cache, five helper apps that are the same
binary, Info.plists, and an ad-hoc signature on every bundle, inside out.

`install` installs blaze.app from the existing release binaries, with its icon, daemon,
kernel, Bun runtime and browser bundle. It never rebuilds or stops running sessions.
"""

import asyncio
import hashlib
import json
import os
import plistlib
import shutil
import sys
import tarfile
import tempfile
import tomllib
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
NATIVE_APP = "blaze"
# What macOS shows when it asks the person to let the browser read where the Mac is.
LOCATION_WHY = "Pages you open in harness, and agents you ask, can use where this Mac is."
# Why the browser may use each privacy-protected device, in every plist that can be asked: the host's own, and
# blaze.app's. macOS (TCC) checks the *responsible* app's plist, and once blaze.app launches the daemon that launches
# the host, that is blaze.app, not Harness Browser.app. A request whose responsible plist lacks the key is not refused:
# the process is killed (SIGABRT, "attempted to access privacy-sensitive data without a usage description"). Chromium
# reaches Bluetooth when a page offers a passkey (its phone-as-passkey path, e.g. a sign-in page), the camera and the
# microphone when a page asks for them, and Core Location for navigator.geolocation (and ts location()); without the
# location keys that times out and macOS never asks.
PRIVACY_WHY = {
    "NSBluetoothAlwaysUsageDescription": "Pages you open in harness can sign you in with a passkey on your phone or a security key.",
    "NSCameraUsageDescription": "Pages you open in harness can use the camera when you allow it.",
    "NSMicrophoneUsageDescription": "Pages you open in harness can use the microphone when you allow it.",
    "NSLocationUsageDescription": LOCATION_WHY,
    "NSLocationWhenInUseUsageDescription": LOCATION_WHY,
}
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
    op: Literal["fetch", "bundle", "install"]
    # bundle: the cargo profile directory under the target dir.
    profile: str = "release"
    # bundle: the cargo target dir, default crates/harness/target.
    target_dir: str | None = None
    # fetch: the cache root, default ~/Library/Caches/inc.indexable.harness/cef (CEF_PATH for cef-dll-sys).
    cache: str | None = None
    # install: the app destination (also supports ~/Applications/blaze.app).
    destination: str = f"/Applications/{NATIVE_APP}.app"


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
            **PRIVACY_WHY,
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


async def install(inp: Input) -> None:
    out = (Path(inp.target_dir) if inp.target_dir is not None else HARNESS / "target") / inp.profile
    app = Path(inp.destination).expanduser().absolute()
    if app.suffix != ".app":
        raise StepError(f"install: destination {app} must end in .app")
    ident = "inc.indexable.harness"
    if app.exists():
        plist = app / "Contents/Info.plist"
        if not plist.is_file() or plistlib.loads(plist.read_bytes()).get("CFBundleIdentifier") != ident:
            raise StepError(f"install: {app} is not a harness app; refusing to replace it")
    bun = shutil.which("bun")
    required = [out / "harness-native", out / "harness", out / f"{APP}.app",
                HARNESS / "mcp/src/mcp.ts", HARNESS / "mcp/node_modules"]
    for path in required:
        if not path.exists():
            raise StepError(f"install: missing {path}; build the release binaries, bundle the browser, and bun install in mcp first")
    if bun is None:
        raise StepError("install: bun is not on PATH; install Bun before packaging harness")
    app.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".harness-install-", dir=app.parent) as work:
        staging = Path(work) / app.name
        contents = staging / "Contents"
        macos = contents / "MacOS"
        resources = contents / "Resources"
        macos.mkdir(parents=True)
        resources.mkdir()
        version = tomllib.loads((HARNESS / "harness-native/Cargo.toml").read_text())["package"]["version"]
        (contents / "Info.plist").write_bytes(plistlib.dumps({
            "CFBundleDevelopmentRegion": "en",
            "CFBundleDisplayName": NATIVE_APP,
            "CFBundleName": NATIVE_APP,
            "CFBundleIdentifier": ident,
            "CFBundleExecutable": NATIVE_APP,
            "CFBundleIconFile": f"{NATIVE_APP}.icns",
            "CFBundlePackageType": "APPL",
            "CFBundleInfoDictionaryVersion": "6.0",
            "CFBundleShortVersionString": version,
            "CFBundleVersion": version,
            "LSApplicationCategoryType": "public.app-category.developer-tools",
            "LSMinimumSystemVersion": "13.0",
            "NSHighResolutionCapable": True,
            "NSPrincipalClass": "NSApplication",
            "NSSupportsAutomaticGraphicsSwitching": True,
            # The browser host runs under this app, so macOS checks its privacy requests against this plist.
            **PRIVACY_WHY,
        }))
        (contents / "PkgInfo").write_bytes(b"APPL????")
        # Only PATH is carried from the install environment: never credentials or the rest of env.
        (resources / "tool-path").write_text(os.environ["PATH"])
        provenance = {}
        for name, source in (("harness-native", out / "harness-native"), ("harness", out / "harness"), ("bun", Path(bun).resolve())):
            await run(f"copy {name}", "/bin/cp", "-c", str(source), str(macos / name), timeout=30)
            provenance[name] = hashlib.sha256(source.read_bytes()).hexdigest()
        (macos / "bunx").symlink_to("bun")
        await run("copy browser", "/bin/cp", "-cR", str(out / f"{APP}.app"), str(macos / f"{APP}.app"), timeout=60)
        kernel = resources / "mcp"
        kernel.mkdir()
        for name in ("src", "skills", "node_modules", "package.json", "bun.lock", "bunfig.toml"):
            await run(f"copy kernel {name}", "/bin/cp", "-cR", str(HARNESS / "mcp" / name), str(kernel / name), timeout=60)
        (resources / "binaries.json").write_text(json.dumps(provenance, indent=2) + "\n")
        await run("compile app launcher", "/usr/bin/clang", "-Os", "-Wall", "-Wextra", "-Werror",
                  str(HARNESS / "harness-native/macos/launcher.c"), "-o", str(macos / NATIVE_APP), timeout=60)
        iconset = Path(work) / f"{NATIVE_APP}.iconset"
        iconset.mkdir()
        # The canonical blaze brand asset, shared with the website; never invent a second app mark.
        icon = HARNESS.parents[1] / "docs/brand/blaze-icon-dark-1024.png"
        for size in (16, 32, 128, 256, 512):
            for scale in (1, 2):
                pixels = str(size * scale)
                suffix = "@2x" if scale == 2 else ""
                await run("size blaze app icon", "/usr/bin/sips", "-z", pixels, pixels, str(icon), "--out",
                          str(iconset / f"icon_{size}x{size}{suffix}.png"), timeout=10)
        await run("package app icon", "/usr/bin/iconutil", "-c", "icns", str(iconset),
                  "-o", str(resources / f"{NATIVE_APP}.icns"), timeout=30)
        sign = ("/usr/bin/codesign", "--force", "--sign", "-", "--timestamp=none")
        for name in ("harness", "harness-native", "bun"):
            await run(f"sign {name}", *sign, str(macos / name), timeout=30)
        await run(f"sign {NATIVE_APP}.app", *sign, str(staging), timeout=60)
        await run(f"verify {NATIVE_APP}.app", "/usr/bin/codesign", "--verify", "--deep", "--strict", str(staging), timeout=60)
        # Keep the old installation intact until the complete replacement has passed verification.
        backup = Path(work) / "previous.app"
        if app.exists():
            app.rename(backup)
        try:
            staging.rename(app)
        except OSError:
            if backup.exists():
                backup.rename(app)
            raise
    await run(f"register {NATIVE_APP}.app", "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister",
              "-f", str(app), timeout=30)
    print(f"harness: installed {app}")


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
            case "install":
                await install(inp)
    except* StepError as group:
        fail(SCRIPT, "\n".join(str(e) for e in group.exceptions))


if __name__ == "__main__":
    asyncio.run(main())
