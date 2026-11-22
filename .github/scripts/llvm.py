#!/usr/bin/env python3
"""The one LLVM blaze's `llvm` feature links: built here, fetched everywhere else.

pins/llvm.json pins it: the source (a repository, a ref and its commit: our
fork indexable-inc/llvm, branch ix/23.x, LLVM 23.1.4 with our fixes, at the commit wasm-host's
ix-aot pins, so blaze and ix-aot run one LLVM), where the builds are (`release`: blaze's R2
bucket, https://blaze-releases.ix.dev/native, which llvm.yaml fills with .github/scripts/r2.py),
and per target the archive's name and sha256.

One JSON value, inline, a file's path, or `-` for stdin (AGENTS.md, scripts-take-typed-input);
`command` says which, the other fields are its arguments, and an unknown one is refused:

  {"command": "build", "target": T, "work": DIR, "out": DIR, "jobs": N, "origin": REPO}
      build the pinned commit, archive it into `out` (named by its sha256, beside a T.json);
      `jobs` (default: ninja's) and `origin`, a clone or bundle to fetch the commit from;
      `"cfront": true` also builds blaze-cfront (D174, crates/blaze-cc/helper) into it
  {"command": "fetch", "target": T, "dir": DIR}
      the pinned archive for T, checked and unpacked into `dir` (a no-op when it holds it)
  {"command": "pin", "root": DIR}
      llvm.json names the archives in each directory of `root`, and pins/cfront.json the
      helper of each archive built with `cfront`

`build` makes a prefix of what the feature and the harnesses read, and no more: the static
libraries of the X86 and AArch64 targets, the headers, `llvm-config`, `llvm-dwarfdump` and
`llvm-pdbutil`. Its own tools link the C++ runtime statically, and it is built with none of
zlib, zstd, libxml2 or terminfo, so neither it nor a binary that links it asks the system for
more than libc. On Linux it is built on the oldest hosted Ubuntu (22.04) like the release, on
macOS for macOS 13 like the release, and on Windows against the static C runtime like blaze's
Windows build. The prefix gets `blaze-llvm.json` (version, repository, commit, target), which
vendor/llvm-sys/pin.rs reads to refuse any other LLVM.

The fork is public: it is fetched anonymously, or with `origin` from a clone or a bundle that
has the commit.

Every step is bounded; a failure names the step, the command, its exit code and its output's
tail. Stdlib only (typed_input, tools/typed-input, is one file of it); Python 3.12+.
"""

import asyncio
import hashlib
import json
import lzma
import os
import shlex
import shutil
import sys
import tarfile
import tempfile
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

ROOT = Path(__file__).resolve().parents[2]
# Run by a bare python on a runner, and by build_release.py with -I, not by uv: the one stdlib-only
# modules it needs are found from the tree: typed_input, and the pins' one reader.
sys.path.insert(0, str(ROOT / "tools" / "typed-input" / "src"))
sys.path.insert(0, str(ROOT / "harness"))

import toolchain_pins
from typed_input import fail, parse

PIN = Path(toolchain_pins.LLVM)
MARKER = "blaze-llvm.json"
# The sha256 of the archive a directory was unpacked from, written by `fetch`.
UNPACKED = "blaze-llvm.sha256"
# What the prefix has: `ninja install-distribution` installs these components.
# D69: the C toolchain native build scripts run with comes from the same build: `clang` (also
# `clang++`), its resource headers, `libclang` (bindgen loads it), `lld` (`ld64.lld`, the
# linker the shelf's `clang.cfg` names) and `llvm-ar`. The pin in pins/clang.json
# moves to this archive once it carries them.
COMPONENTS = [
    "llvm-libraries", "llvm-headers", "cmake-exports", "llvm-config", "llvm-dwarfdump", "llvm-pdbutil",
    "clang", "clang-resource-headers", "libclang", "lld", "llvm-ar",
]
WINDOWS = sys.platform == "win32"
# D174 (docs/v2/plan/design/c.md "The toolchain side"): with `cfront`, the C toolchain helper
# blaze-cfront (crates/blaze-cc/helper) is built against the prefix after
# `install-distribution` and before the libraries are merged, from these extra components:
# clang's static libraries, headers and cmake config, which are removed again once it is built,
# so the archive carries the helper and not them. pins/cfront.json pins its sha256 per target.
CFRONT_COMPONENTS = ["clang-libraries", "clang-headers", "clang-cmake-exports"]
CFRONT_SOURCE = ROOT / "crates" / "blaze-cc" / "helper"
CFRONT_PIN = ROOT / "pins" / "cfront.json"
CFRONT_MEMBER = "bin/blaze-cfront"


def pin():
    return json.loads(PIN.read_text())


def archive_stem(p, target):
    """An archive's name before its content's hash: blaze-llvm-<version>-<commit12>-<target>. The
    full name ends `-<sha256 16>.tar.xz`, so a rebuild never replaces an asset a pin names."""
    return f"blaze-llvm-{p['version']}-{p['commit'][:12]}-{target}"


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


async def run(step, *cmd, cwd=None, env=None, limit=3600.0):
    print(f"== {step}: {shlex.join(map(str, cmd))}", flush=True)
    proc = await asyncio.create_subprocess_exec(
        *map(str, cmd), cwd=cwd, env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT
    )
    tail = bytearray()
    try:
        async with asyncio.timeout(limit):
            assert proc.stdout is not None
            while line := await proc.stdout.readline():
                # Long builds show progress; the tail is kept for a failure.
                sys.stdout.write(line.decode(errors="replace"))
                tail += line
                del tail[:-8000]
            await proc.wait()
    except TimeoutError:
        proc.kill()
        sys.exit(f"error: llvm.py: {step}: timed out after {limit:.0f} s: {shlex.join(map(str, cmd))}\n"
                 f"{tail.decode(errors='replace')[-4000:]}")
    if proc.returncode != 0:
        sys.exit(f"error: llvm.py: {step}: exit {proc.returncode}: {shlex.join(map(str, cmd))}\n"
                 f"{tail.decode(errors='replace')[-4000:]}")
    return tail.decode(errors="replace")


def source(p, given):
    """Where the pinned commit is fetched from: `given` (a clone or a bundle), else llvm.json's
    repository."""
    return str(Path(given).expanduser().resolve()) if given else p["repo"]


async def checkout(p, work, origin):
    """The pinned commit in `work/src`, fetched alone (depth 1) from `origin`."""
    src = work / "src"
    env = dict(os.environ, GIT_TERMINAL_PROMPT="0")
    if not (src / ".git").exists():
        await run("init", "git", "init", "-q", src, limit=60)
    await run("fetch", "git", "-C", src, "fetch", "-q", "--depth", "1", origin, p["commit"], env=env, limit=1800)
    await run("checkout", "git", "-C", src, "-c", "core.autocrlf=false", "-c", "core.longpaths=true", "checkout", "-q", "--force", p["commit"],
              limit=600)
    return src


def cmake_args(target, cfront=False):
    components = COMPONENTS + (CFRONT_COMPONENTS if cfront else [])
    args = [
        "-G", "Ninja",
        "-DCMAKE_BUILD_TYPE=Release",
        # WebAssembly: blaze builds proc-macro crates for wasm32-wasip1 and runs them in Wasmtime.
        "-DLLVM_TARGETS_TO_BUILD=X86;AArch64;WebAssembly",
        "-DLLVM_ENABLE_PROJECTS=clang;lld",
        # `clang` as `clang++` too; no clang tools beyond the driver and libclang.
        "-DCLANG_ENABLE_ARCMT=OFF",
        "-DCLANG_ENABLE_STATIC_ANALYZER=OFF",
        "-DCLANG_INCLUDE_TESTS=OFF",
        "-DCLANG_INCLUDE_DOCS=OFF",
        "-DLLVM_ENABLE_ASSERTIONS=OFF",
        "-DLLVM_INCLUDE_TESTS=OFF",
        "-DLLVM_INCLUDE_BENCHMARKS=OFF",
        "-DLLVM_INCLUDE_EXAMPLES=OFF",
        "-DLLVM_INCLUDE_DOCS=OFF",
        "-DLLVM_ENABLE_BINDINGS=OFF",
        "-DLLVM_ENABLE_ZLIB=OFF",
        "-DLLVM_ENABLE_ZSTD=OFF",
        "-DLLVM_ENABLE_LIBXML2=OFF",
        "-DLLVM_ENABLE_TERMINFO=OFF",
        "-DLLVM_ENABLE_LIBEDIT=OFF",
        "-DLLVM_ENABLE_LIBPFM=OFF",
        "-DLLVM_ENABLE_CURL=OFF",
        "-DLLVM_ENABLE_HTTPLIB=OFF",
        "-DLLVM_BUILD_LLVM_DYLIB=OFF",
        "-DLLVM_LINK_LLVM_DYLIB=OFF",
        f"-DLLVM_DISTRIBUTION_COMPONENTS={';'.join(components)}",
    ]
    if "linux" in target:
        args += ["-DCMAKE_C_COMPILER=gcc", "-DCMAKE_CXX_COMPILER=g++", "-DLLVM_STATIC_LINK_CXX_STDLIB=ON"]
    elif "apple" in target:
        # As the release: MACOSX_DEPLOYMENT_TARGET in release.yaml.
        args += ["-DCMAKE_OSX_DEPLOYMENT_TARGET=13.0", "-DCMAKE_OSX_ARCHITECTURES=arm64"]
    elif "windows" in target:
        # blaze's Windows build links the C runtime statically (+crt-static), so LLVM does too.
        args += ["-DCMAKE_C_COMPILER=cl", "-DCMAKE_CXX_COMPILER=cl", "-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded",
                 "-DLLVM_ENABLE_DIA_SDK=OFF"]
    return args


def cmake_env(target):
    """What cmake is run with. On Windows the STL's vectorized algorithms are off: the STL calls
    them in its static library under names that change between MSVC versions
    (`__std_max_element_8u` in 14.4x is `__std_max_element_8` in 14.50), and without them the
    archive links with any MSVC from the one it was built with on, Visual Studio 2022's and
    2026's (#1387). In CFLAGS and CXXFLAGS, which cmake adds to its own defaults when it first
    configures the build; `-DCMAKE_CXX_FLAGS` would replace them. Exactly these, not whatever
    flags the caller's environment has."""
    env = dict(os.environ)
    for name in ("CFLAGS", "CXXFLAGS"):
        env.pop(name, None)
        if "windows" in target:
            env[name] = "/D_USE_STD_VECTOR_ALGORITHMS=0"
    return env


def recipe(target, cfront=False):
    """What a build tree is configured with. cmake reads CFLAGS and CXXFLAGS only when it first
    configures a tree, so a tree configured with other ones is not built on."""
    env = cmake_env(target)
    return json.dumps({"args": cmake_args(target, cfront), "cflags": env.get("CFLAGS"), "cxxflags": env.get("CXXFLAGS")})


async def build_cfront(target, work, prefix, exe):
    """D174: blaze-cfront from CFRONT_SOURCE, configured against `prefix` (its clang and LLVM
    cmake config, before `merge` removes the component libraries), linked statically to them,
    installed as `prefix`/CFRONT_MEMBER; then the clang libraries, headers and config it was
    built from are removed from the prefix. Returns its sha256. A missing source is refused by
    name: nothing is built without it."""
    if not (CFRONT_SOURCE / "CMakeLists.txt").is_file():
        sys.exit(f"error: llvm.py: cfront: {CFRONT_SOURCE / 'CMakeLists.txt'} does not exist; "
                 "the helper's source is not in this tree")
    if WINDOWS:
        sys.exit("error: llvm.py: cfront: blaze-cfront has no Windows build (D174 covers macOS and Linux)")
    bld = work / "cfront-build"
    shutil.rmtree(bld, ignore_errors=True)
    bld.mkdir()
    args = ["-G", "Ninja", "-DCMAKE_BUILD_TYPE=Release", f"-DCMAKE_PREFIX_PATH={prefix}",
            f"-DCMAKE_INSTALL_PREFIX={prefix}"]
    if "linux" in target:
        args += ["-DCMAKE_C_COMPILER=gcc", "-DCMAKE_CXX_COMPILER=g++",
                 "-DCMAKE_EXE_LINKER_FLAGS=-static-libstdc++ -static-libgcc"]
    elif "apple" in target:
        args += ["-DCMAKE_OSX_DEPLOYMENT_TARGET=13.0", "-DCMAKE_OSX_ARCHITECTURES=arm64"]
    await run("cfront cmake", "cmake", *args, CFRONT_SOURCE, cwd=bld, env=cmake_env(target), limit=600)
    await run("cfront ninja", "ninja", "install", cwd=bld, limit=3600)
    helper = prefix / f"{CFRONT_MEMBER}{exe}"
    if not helper.is_file():
        sys.exit(f"error: llvm.py: cfront: the helper's install left no {helper}")
    lib = prefix / "lib"
    for it in sorted(lib.glob("libclang*.a")):
        # clang's static libraries (libclangAST.a, ...); `libclang` (the shared library bindgen
        # loads) is a .dylib or .so and stays.
        it.unlink()
    shutil.rmtree(prefix / "include" / "clang", ignore_errors=True)
    shutil.rmtree(lib / "cmake" / "clang", ignore_errors=True)
    return sha256(helper)


def pack(prefix, out, top):
    """`prefix` archived as `out` (tar.xz; the members under the one directory `top`, as LLVM's
    own releases are, so `tar --strip-components=1` and Nix's unpacker both take it)."""
    with lzma.open(out, "wb", preset=6) as raw, \
            tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for path in sorted(prefix.rglob("*")):
            info = tar.gettarinfo(path, f"{top}/{path.relative_to(prefix).as_posix()}")
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.mtime = 0
            if info.isfile():
                with open(path, "rb") as f:
                    tar.addfile(info, f)
            else:
                tar.addfile(info)


MERGED = "LLVMBlaze"


async def manifest(prefix, exe):
    """What a build that links this prefix needs to know of it, as data in `blaze-llvm.json`, so
    that building blaze runs no `llvm-config`: the archive for another platform is then linked
    from any host (vendor/llvm-sys/pin.rs reads it; docs/v2/plan/design/toolchain-c.md, "Cross
    compilation"). Asked of the `llvm-config` just built, on the machine that built it, once.

    The static libraries are merged into one, lib/libLLVMBlaze.a (LLVMBlaze.lib), and the
    components are removed: `libs` is then one name and the link needs no order. One static
    archive and not one shared library (`LLVM_BUILD_LLVM_DYLIB`): a link of blaze takes only the
    members it reaches (blaze-engine with LLVM is 302 MB against 280 MB of archives on
    aarch64 Linux, 2026-11-12), and a shared libLLVM would be loaded and bound whole at every
    start of the engine.

    A flag is a word with no quote, backslash or `{`: the reader is a few lines of std-only Rust.
    The prefix is written `{prefix}`, since the archive is unpacked somewhere else."""
    config = prefix / "bin" / f"llvm-config{exe}"

    async def said(*query):
        return (await run("llvm-config", config, *query, limit=60)).split()

    def words(what, items):
        root = str(prefix).replace("\\", "/")
        out = [it.replace("\\", "/").replace(root, "{prefix}") for it in items]
        for it in out:
            if any(c in it for c in '"\\') or ("{" in it and "{prefix}" not in it):
                sys.exit(f"error: llvm.py: llvm-config's {what} has `{it}`, which the manifest does not hold")
        return out

    names = await said("--libnames", "--link-static")
    lib = prefix / "lib"
    await merge(lib, names)
    return {
        "manifest": 1,
        "targets_built": words("--targets-built", await said("--targets-built")),
        "libs": [MERGED],
        "system_libs": words("--system-libs", await said("--system-libs", "--link-static")),
        "cflags": words("--cflags", await said("--cflags")),
        "cxxflags": words("--cxxflags", await said("--cxxflags")),
        "build_mode": words("--build-mode", await said("--build-mode"))[0],
    }


async def merge(lib, names):
    """lib/<names> as one archive, the components removed. The members keep their order, which
    no linker needs within one archive (each resolves an archive's members until none is new)."""
    files = [lib / name for name in names]
    missing = [str(it) for it in files if not it.is_file()]
    if missing:
        sys.exit(f"error: llvm.py: llvm-config names libraries the prefix lacks: {' '.join(missing)}")
    if WINDOWS:
        merged = lib / f"{MERGED}.lib"
        await run("lib", "lib", "/NOLOGO", f"/OUT:{merged}", *files, limit=1800)
    elif sys.platform == "darwin":
        merged = lib / f"lib{MERGED}.a"
        await run("libtool", "libtool", "-static", "-no_warning_for_no_symbols", "-o", merged, *files, limit=1800)
    else:
        merged = lib / f"lib{MERGED}.a"
        script = "".join([f"create {merged}\n", *[f"addlib {it}\n" for it in files], "save\nend\n"])
        mri = lib / "merge.mri"
        mri.write_text(script)
        await run("ar", "sh", "-c", f"ar -M < '{mri}'", limit=1800)
        mri.unlink()
    for it in files:
        it.unlink()
    # The cmake exports name the components, which are gone: nothing of blaze reads them.
    shutil.rmtree(lib / "cmake", ignore_errors=True)


async def build(args):
    p = pin()
    origin = source(p, args.origin)
    work, out = Path(args.work).expanduser().resolve(), Path(args.out).expanduser().resolve()
    work.mkdir(parents=True, exist_ok=True)
    out.mkdir(parents=True, exist_ok=True)
    src = await checkout(p, work, origin)
    bld, prefix = work / "build", work / "prefix"
    shutil.rmtree(prefix, ignore_errors=True)
    stamp = bld / "blaze-recipe.json"
    if bld.exists() and (not stamp.exists() or stamp.read_text() != recipe(args.target, args.cfront)):
        shutil.rmtree(bld)
    bld.mkdir(exist_ok=True)
    stamp.write_text(recipe(args.target, args.cfront))
    await run("cmake", "cmake", *cmake_args(args.target, args.cfront), f"-DCMAKE_INSTALL_PREFIX={prefix}", src / "llvm",
              cwd=bld, env=cmake_env(args.target), limit=900)
    jobs = ["-j", str(args.jobs)] if args.jobs else []
    await run("ninja", "ninja", *jobs, "install-distribution", cwd=bld, limit=5 * 3600)
    exe = ".exe" if WINDOWS else ""
    said = (await run("llvm-config", prefix / "bin" / f"llvm-config{exe}", "--version", limit=60)).strip()
    if said != p["version"]:
        sys.exit(f"error: llvm.py: the build says LLVM {said}, llvm.json pins {p['version']}")
    cfront = await build_cfront(args.target, work, prefix, exe) if args.cfront else None
    link = await manifest(prefix, exe)
    (prefix / MARKER).write_text(json.dumps(
        {"version": p["version"], "repo": p["repo"], "ref": p["ref"], "commit": p["commit"],
         "target": args.target, **link}, indent=2) + "\n")
    stem = archive_stem(p, args.target)
    packed = out / f"{stem}.tar.xz"
    await asyncio.to_thread(pack, prefix, packed, stem)
    digest = sha256(packed)
    archive = packed.rename(out / f"{stem}-{digest[:16]}.tar.xz")
    entry = {"target": args.target, "name": archive.name, "sha256": digest}
    if cfront:
        # D174: `pin` writes it to pins/cfront.json.
        entry["cfront_sha256"] = cfront
    (out / f"{args.target}.json").write_text(json.dumps(entry, indent=2) + "\n")
    print(f"llvm.py: {archive} ({archive.stat().st_size / 1e6:.0f} MB, sha256 {digest})", flush=True)


def fetch(args):
    p = pin()
    entry = p["targets"].get(args.target)
    if not entry:
        sys.exit(f"error: llvm.py: {PIN} has no LLVM for {args.target}: run the llvm workflow for it")
    dest = Path(args.dir).expanduser().resolve()
    marker = dest / MARKER
    unpacked = dest / UNPACKED
    if marker.exists():
        have = json.loads(marker.read_text())
        if have.get("commit") == p["commit"] and have.get("version") == p["version"]:
            # The same commit can be built again (a flag, a compiler): a directory is the pinned
            # archive when it was unpacked from it.
            if unpacked.exists() and unpacked.read_text().strip() == entry["sha256"]:
                print(dest)
                return
            sys.exit(f"error: llvm.py: {dest} holds LLVM {have.get('version')} at {have.get('commit')}, but not "
                     f"the archive {entry['name']} that llvm.json pins now: delete it or choose another directory")
        sys.exit(f"error: llvm.py: {dest} holds LLVM {have.get('version')} at {have.get('commit')}, not the pinned "
                 f"{p['version']} at {p['commit']}: delete it or choose another directory")
    if dest.exists() and any(dest.iterdir()):
        sys.exit(f"error: llvm.py: {dest} is not empty and holds no {MARKER}")
    url = f"{p['release']}/{entry['name']}"
    with tempfile.TemporaryDirectory(prefix="blaze-llvm-") as tmp:
        archive = Path(tmp) / entry["name"]
        print(f"llvm.py: downloading {url}", file=sys.stderr, flush=True)
        for attempt in range(5):
            try:
                # A User-Agent of its own: Cloudflare, in front of R2, answers Python's default
                # (`Python-urllib/3.x`) with a 403.
                ask = urllib.request.Request(url, headers={"User-Agent": "blaze-llvm.py (https://github.com/indexable-inc/blaze)"})
                with urllib.request.urlopen(ask, timeout=300) as got, open(archive, "wb") as f:
                    shutil.copyfileobj(got, f, 1 << 20)
                break
            except OSError as e:
                if attempt == 4:
                    sys.exit(f"error: llvm.py: downloading {url}: {e}")
        if (digest := sha256(archive)) != entry["sha256"]:
            sys.exit(f"error: llvm.py: {url} has sha256 {digest}, llvm.json pins {entry['sha256']}")
        staging = Path(tmp) / "x"
        with tarfile.open(archive, "r:xz") as tar:
            tar.extractall(staging, filter="tar")
        (top,) = staging.iterdir()
        dest.parent.mkdir(parents=True, exist_ok=True)
        if dest.exists():
            dest.rmdir()
        shutil.move(str(top), dest)
    if not marker.exists():
        sys.exit(f"error: llvm.py: {url} has no {MARKER}")
    unpacked.write_text(entry["sha256"] + "\n")
    print(dest)


def pin_dirs(root):
    p = pin()
    c = json.loads(CFRONT_PIN.read_text())
    cfront = dict(c["targets"])
    for d in sorted(Path(root).expanduser().iterdir()):
        if not d.is_dir():
            continue
        for entry in sorted(d.glob("*.json")):
            got = json.loads(entry.read_text())
            if sha256(d / got["name"]) != got["sha256"]:
                sys.exit(f"error: llvm.py: {d / got['name']} is not what {entry} says")
            if not got["name"].startswith(archive_stem(p, got["target"]) + "-"):
                sys.exit(f"error: llvm.py: {got['name']} is not a build of the pinned commit")
            p["targets"][got["target"]] = {"name": got["name"], "sha256": got["sha256"]}
            if "cfront_sha256" in got:
                cfront[got["target"]] = {"archive": got["name"], "sha256": got["cfront_sha256"]}
            else:
                # An archive without the helper: a pin of it would name a helper that is not there.
                cfront.pop(got["target"], None)
    p["targets"] = dict(sorted(p["targets"].items()))
    PIN.write_text(json.dumps(p, indent=2) + "\n")
    c["targets"] = dict(sorted(cfront.items()))
    CFRONT_PIN.write_text(json.dumps(c, indent=2) + "\n")


@dataclass(frozen=True)
class Input:
    command: Literal["build", "fetch", "pin"]
    target: str = ""
    work: str = ""
    out: str = ""
    jobs: int = 0
    origin: str = ""
    dir: str = ""
    root: str = ""
    # D174: build blaze-cfront into the archive too (CFRONT_SOURCE must exist).
    cfront: bool = False


# What each command needs, and what it may also be given.
FIELDS = {
    "build": ({"target", "work", "out"}, {"jobs", "origin", "cfront"}),
    "fetch": ({"target", "dir"}, set()),
    "pin": ({"root"}, set()),
}


def main():
    match sys.argv[1:]:
        case [text]:
            args = parse(Input, text, "llvm.py")
        case _:
            fail("llvm.py", "usage: llvm.py JSON (see the top of the file)")
    needs, may = FIELDS[args.command]
    given = {name for name in vars(args) if name != "command" and getattr(args, name) not in ("", 0)}
    if missing := needs - given:
        fail("llvm.py", f"{args.command} needs {', '.join(sorted(missing))}")
    if extra := given - needs - may:
        fail("llvm.py", f"{args.command} takes no {', '.join(sorted(extra))}")
    if args.command == "build":
        asyncio.run(build(args))
    elif args.command == "fetch":
        fetch(args)
    else:
        pin_dirs(args.root)


if __name__ == "__main__":
    main()
