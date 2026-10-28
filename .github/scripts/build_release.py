#!/usr/bin/env python3
"""Build blaze the one way it ships: the `dist` profile, optimized with a profile of its own
work (PGO). The release workflow, the flake's `daemon` and `cargo install` from a checkout all
build it with this.

  build_release.py --target T [--profdata FILE] [--features F] [--out DIR [--server]]
                                                  # the build, into target/T/dist; DIR gets every binary of blaze,
                                                  # and with --server blaze's proc-macro server
  build_release.py --target T --train DIR         # a new profile for T, into DIR (the pgo workflow)
  build_release.py --pin DIR                      # tools/pgo/profiles.json names the profiles in DIR
  build_release.py --target T --show              # the command the build runs, for any other driver
  build_release.py --server-beside BLAZE         # blaze's proc-macro server beside a blaze already built (CI)

The build is `cargo build -p blaze-engine --profile dist --locked --target T --features llvm` with
`RUSTFLAGS=-Cprofile-use=<profile>`. The `llvm` feature is part of every build of it, the
training's included, so that every user gets the LLVM tiers (docs/tiers.md): LLVM is linked in
statically from the build crates/hir-ty-codegen/llvm.json pins, which `LLVM_SYS_231_PREFIX`
names (the flake's) or, unset, .github/scripts/llvm.py fetches into target/llvm/T. The binaries
are refused when one loads libLLVM, or on Linux a C++ runtime: a machine that downloads them has
neither. The profile is the one tools/pgo/profiles.json pins for T: it
is downloaded from the `pgo-profiles` release and refused unless its sha256 is the pinned
one; `--profdata` gives the file instead (the flake fetches it with the same hash, since a nix
build has no network).

A profile is trained by `--train`, in three steps:

  1. the same build instrumented (`-Cprofile-generate`), into target/pgo-instrumented;
  2. that blaze run on edits of projects the README's benchmarks do not measure (fd, bat,
     just and hyperfine, at the commits harness/bench.py pins: a cold start and every kind of
     edit, twice, through harness/abbench.py --train);
  3. what it counted merged with the toolchain's llvm-profdata (`rustup component add
     llvm-tools`), refused when it is too small to be the workers' (see below).

A profile trained on older code still applies: LLVM uses it for every function whose control
flow is unchanged and leaves the others unoptimized by it. The build counts what no longer
matches and says so; the pgo workflow trains new profiles when that grows.

Against `cargo build --release` on Linux (docs/reports/RELEASE-BUILD.md): the cold start 14%
shorter on nushell and ripgrep, an edit of a function 8% shorter on nushell, the same memory.

A killed process writes no counters: `blaze stop` kills the workers, so abbench ends a
training session by ending the daemon, after which each worker sees its input close and exits
by itself. The `cc` crate passes `-Cprofile-generate`/`-Cprofile-use` on to the C compiler;
on macOS Apple clang's profile format is not rustc's LLVM's and the instrumented blaze
crashes at start (rust-lang/rust#163640), so the C code (mimalloc, ring) is built without
them. macOS and Linux; stdlib only.

With `--server` the blaze just built builds blaze's proc-macro server (`vendor/proc-macro-srv`,
docs/no-rustc.md step J1) into DIR as `blaze-proc-macro-srv`, the server that expands with the
proc-macro libraries blaze links, found beside blaze (`BLAZE_PROC_MACRO_SERVER=1`). It is
built whole by blaze (`--exe`), over the standard library blaze compiles, so it speaks the
bridge of the libraries that blaze compiles; its own serde derives are expanded by the
toolchain's server with rustc's libraries, as blaze's own executable may be built with rustc.
Not on Windows, whose release blaze does not build yet.

On Linux every binary in DIR, the server's included, is refused when it asks for a glibc newer
than `GLIBC` (RHEL 9's; Debian 12 and Ubuntu 22.04 have newer): the release builds on the
oldest hosted Ubuntu, and a binary linked on a newer system asks for that system's glibc
(GitHub issue #351). install.sh says the same floor to a system below it. With `--nix` (the
flake's `dist`) every binary has to load nixpkgs' glibc through a loader under /nix/store
instead, as a binary built by Nix does: it never meets the system's glibc, so the floor is not
its own, and a binary of the build that would meet it is refused.
"""

import argparse
import collections
import hashlib
import json
import os
import re
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
HARNESS = ROOT / "harness"
TOOLCHAIN = (HARNESS / "toolchain").read_text().strip()
PINS = ROOT / "tools" / "pgo" / "profiles.json"
# Where tools/pgo/profiles.json says its files are, when it is first written.
RELEASE = "https://github.com/indexable-inc/blaze/releases/download/pgo-profiles"
# A training run that reached the workers counts tens of billions; one that lost them a few
# million (the client's own short runs).
MIN_COUNT = 1_000_000_000
# blaze's proc-macro server: its workspace, the crate of its executable, and the name it ships as.
SERVER = ROOT / "vendor" / "proc-macro-srv"
SERVER_CRATE = "rust_analyzer_proc_macro_srv"
SERVER_BIN = "blaze-proc-macro-srv"
# What LLVM says of a function whose control flow changed since the profile was trained.
STALE = re.compile(r"function control flow change detected \(hash mismatch\)")
# The hash rustc gives the crate blaze-engine, in each of its symbols. Cargo derives it from the
# whole graph of the build: every crate's name, version, source and features, the profile
# (whether it builds incrementally too), the toolchain, and a wrapper of rustc around the
# workspace's crates. LLVM finds a function of the
# profile by its symbol, so a build whose hash is not the profile's finds next to nothing in it
# (the crates that did not change below, none of the workspace's), and says nothing.
CRATE = re.compile(rb"Cs([0-9A-Za-z]+)_6blaze")
# The newest glibc a Linux binary of a release may ask for (install.sh's `glibc_floor`), and
# how a binary names the versions it asks for (in its dynamic string table).
GLIBC = (2, 34)
GLIBC_VERSION = re.compile(rb"GLIBC_(\d+)\.(\d+)(?:\.\d+)?\x00")
# The LLVM the `llvm` feature links (crates/hir-ty-codegen/build.rs), and the libraries no
# shipped binary may load: LLVM is linked in, and on Linux libstdc++ with it.
LLVM_PREFIX = "LLVM_SYS_231_PREFIX"
NOT_LOADED = {"linux": re.compile(r"libLLVM|libstdc\+\+|libc\+\+"), "darwin": re.compile(r"libLLVM")}


def run(argv, **kwargs):
    print("+", " ".join(map(str, argv)), flush=True)
    return subprocess.run(argv, check=True, **kwargs)


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def recipe(target, rustflags, cflags, features=None, llvm="<LLVM_SYS_231_PREFIX>"):
    """The environment and command of the build for `target`: `cargo build -p blaze-engine --profile
    dist --features llvm` with `rustflags`, `cflags` for the C code that crates build with `cc`,
    and the LLVM prefix `llvm`. No wrapper
    of rustc: one around the workspace's crates (`blaze enable`'s, nixpkgs' cargo-auditable)
    changes their symbols, and the profile no longer finds them. Cargo hashes the empty
    `RUSTC_WORKSPACE_WRAPPER` too, unlike an unset one, so every build sets it, the training's
    included, whatever cargo's configuration says. Nor incremental, which a developer's
    `CARGO_INCREMENTAL=1` would otherwise turn on for the workspace's crates, changing them as
    well (`0` is `dist`'s own setting, so the hashes are those of a build without it)."""
    triple = target.replace("-", "_")
    env = {"RUSTUP_TOOLCHAIN": TOOLCHAIN, "RUSTFLAGS": rustflags, "RUSTC_WRAPPER": "", "RUSTC_WORKSPACE_WRAPPER": "",
           "CARGO_INCREMENTAL": "0", LLVM_PREFIX: str(llvm)}
    for name in ("CFLAGS", "CXXFLAGS"):
        env[f"{name}_{triple}"] = " ".join(filter(None, [os.environ.get(f"{name}_{triple}"), cflags]))
    return env, ["cargo", "build", "-p", "blaze-engine", "--profile", "dist", "--locked", "--target", target,
                 "--features", ",".join(["llvm", *filter(None, [features])])]


def llvm_prefix(target):
    """The prefix of the pinned LLVM: `LLVM_SYS_231_PREFIX` when it is set (build.rs refuses any
    but the pinned build), else the pinned archive for `target`, fetched into target/llvm."""
    given = os.environ.get(LLVM_PREFIX)
    if given:
        return Path(given)
    prefix = ROOT / "target" / "llvm" / target
    run([sys.executable, "-I", ROOT / ".github" / "scripts" / "llvm.py", "fetch", "--target", target, "--dir", prefix])
    return prefix


def build(target, target_dir, rustflags, cflags, features=None):
    """The build into `target_dir`; the result is the binaries it built (every one of blaze's:
    what ships is all of them) and how many functions LLVM found changed since the profile."""
    own, argv = recipe(target, rustflags, cflags, features, llvm_prefix(target))
    own["CARGO_TARGET_DIR"] = str(target_dir)
    env = dict(os.environ, **own)
    print("+", " ".join(f"{k}={shlex.quote(v)}" for k, v in own.items()), " ".join(argv), flush=True)
    # LLVM's word on the profile reaches cargo as rustc warnings, one per changed function, for
    # the crates that are not capped (the workspace's, the vendored rust-analyzer among them):
    # counted here instead of printed, the other diagnostics printed as cargo would.
    proc = subprocess.Popen(argv + ["--message-format=json"], cwd=ROOT, env=env,
                            stdout=subprocess.PIPE, text=True)
    stale, binaries = set(), []
    for line in proc.stdout:
        try:
            message = json.loads(line)
        except ValueError:
            sys.stdout.write(line)
            continue
        if message.get("reason") == "compiler-artifact" and message.get("executable"):
            binaries.append(Path(message["executable"]))
        elif message.get("reason") == "compiler-message":
            text = message["message"].get("message", "")
            if STALE.search(text):
                stale.add((message.get("package_id"), text))
            else:
                sys.stderr.write(message["message"].get("rendered") or text + "\n")
    if proc.wait():
        sys.exit(f"error: build_release: cargo failed ({proc.returncode})")
    return binaries, len(stale)


def crate_hash(binaries):
    """The crate hash of blaze, read from the symbols of the binary."""
    binary = next(b for b in binaries if b.stem == "blaze-engine")
    found = collections.Counter(CRATE.findall(binary.read_bytes()))
    return found.most_common(1)[0][0].decode() if found else None


def pins():
    return json.loads(PINS.read_text())


def pinned(target):
    table = pins()
    if table["toolchain"] != TOOLCHAIN:
        sys.exit(f"error: build_release: {PINS} has profiles of Rust {table['toolchain']}, blaze builds with {TOOLCHAIN}: "
                 "run the pgo workflow")
    pin = table["targets"].get(target)
    if not pin:
        sys.exit(f"error: build_release: {PINS} has no profile for {target}: run the pgo workflow for it")
    return dict(pin, url=f"{table['release']}/{pin['name']}")


def profile(target, given):
    """The profile pinned for `target`: the file `given`, or a download; either way checked
    against the pin."""
    pin = pinned(target)
    if given:
        path = Path(given)
    else:
        path = ROOT / "target" / "pgo" / pin["name"]
        if not path.exists() or sha256(path) != pin["sha256"]:
            path.parent.mkdir(parents=True, exist_ok=True)
            print(f"build_release: downloading {pin['url']}", flush=True)
            with urllib.request.urlopen(pin["url"], timeout=300) as got, open(path, "wb") as f:
                shutil.copyfileobj(got, f)
    if sha256(path) != pin["sha256"]:
        sys.exit(f"error: build_release: {path} is not the profile {PINS} pins for {target} (sha256 {pin['sha256']})")
    return path, pin


def llvm_tool(name):
    libdir = subprocess.run(["rustc", "--print", "target-libdir"], capture_output=True, text=True, check=True,
                            env=dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN)).stdout.strip()
    tool = Path(libdir).parent / "bin" / name
    if not tool.exists():
        sys.exit(f"error: build_release: {tool} is missing: `rustup component add llvm-tools --toolchain {TOOLCHAIN}`")
    return tool


def train(target, out):
    """A new profile for `target` in `out`, and beside it what tools/pgo/profiles.json pins of it."""
    profdata = llvm_tool("llvm-profdata")
    # Short: the daemon's sockets live under it, and a Unix socket's path has a limit.
    work = Path(tempfile.mkdtemp(prefix="rjpgo-", dir="/tmp"))
    raw = work / "raw"
    raw.mkdir()
    # On AArch64 a direct call reaches 128 MiB, and the instrumented blaze's code is larger. GNU ld
    # then fails its calls into libgcc and the profiling runtime (R_AARCH64_CALL26 truncated);
    # lld puts a range-extension thunk in front of them (pgo.yaml installs it on that runner).
    rustflags = f"-Cprofile-generate={raw}"
    if target.startswith("aarch64-unknown-linux"):
        rustflags += " -Clink-arg=-fuse-ld=lld"
    built, _ = build(target, ROOT / "target" / "pgo-instrumented", rustflags, "-fno-profile-generate")
    binaries, crate = built[0].parent, crate_hash(built)

    # Each process of the instrumented blaze writes its counters to a file of its own as it exits.
    env = dict(os.environ, LLVM_PROFILE_FILE=str(raw / "%p.profraw"))
    env.pop("RUST_BACKTRACE", None)
    run([sys.executable, "abbench.py", "--bin", f"train={binaries}", "fd", "bat", "just", "hyperfine",
         "--rounds", "1", "--runs", "2", "--kinds", "body,leaf,signature,generic,no-op", "--train",
         "--work", str(work / "abbench"), "--out", str(work / "train.json")], cwd=HARNESS, env=env)
    # A process still exiting is still writing its file.
    began = time.time()
    while subprocess.run(["pgrep", "-f", str(binaries)], capture_output=True).returncode == 0:
        if time.time() - began > 300:
            sys.exit(f"error: build_release: programs of {binaries} still run 5 minutes after the training")
        time.sleep(1)

    merged = work / "blaze.profdata"
    run([profdata, "merge", "-o", merged, *sorted(raw.glob("*.profraw"))])
    shown = subprocess.run([profdata, "show", merged], capture_output=True, text=True, check=True).stdout
    total = int(re.search(r"^Total count: (\d+)", shown, re.M)[1])
    print(f"build_release: the profile counts {total:,}", flush=True)
    if total < MIN_COUNT:
        sys.exit(f"error: build_release: the profile counts {total:,}, fewer than {MIN_COUNT:,}: the workers' counters were lost")

    digest = sha256(merged)
    commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True, check=True).stdout.strip()
    name = f"blaze-{TOOLCHAIN}-{target}-{digest[:16]}.profdata"
    out.mkdir(parents=True, exist_ok=True)
    shutil.move(merged, out / name)
    (out / f"{target}.json").write_text(json.dumps(
        {"target": target, "name": name, "sha256": digest, "commit": commit, "count": total, "crate": crate}, indent=2) + "\n")
    shutil.rmtree(work, ignore_errors=True)
    print(f"build_release: {out / name}", flush=True)


def pin(dirs):
    """tools/pgo/profiles.json names the profiles trained into `dirs`, beside those of other targets."""
    table = pins() if PINS.exists() else {"toolchain": TOOLCHAIN, "release": RELEASE, "targets": {}}
    if table["toolchain"] != TOOLCHAIN:
        table = dict(table, toolchain=TOOLCHAIN, targets={})
    for d in dirs:
        for entry in sorted(Path(d).glob("*.json")):
            got = json.loads(entry.read_text())
            if sha256(Path(d) / got["name"]) != got["sha256"]:
                sys.exit(f"error: build_release: {Path(d) / got['name']} is not what {entry} says")
            table["targets"][got.pop("target")] = got
    table["targets"] = dict(sorted(table["targets"].items()))
    PINS.parent.mkdir(exist_ok=True)
    PINS.write_text(json.dumps(table, indent=2) + "\n")


def proc_macro_server(blaze, out):
    """blaze's proc-macro server, built and linked by `blaze` into `out`, from a store of its
    own; the path of the executable."""
    exe = out / SERVER_BIN
    with tempfile.TemporaryDirectory(prefix="blaze-server-") as work:
        env = dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN, BLAZE_CACHE=str(Path(work) / "store"),
                   BLAZE_STD_FROM="off", RUST_BACKTRACE="0")
        start = time.monotonic()
        # `proc_macro` is not in the standard library's unit: the program that links it has it.
        run([str(blaze), str(SERVER), "--crate", SERVER_CRATE, "--crate", "proc_macro",
             "--out", str(Path(work) / "server.o"), "--exe", str(exe)], cwd=SERVER, env=env)
    said = subprocess.run([str(exe), "--version"], capture_output=True, text=True, check=True,
                          env=dict(os.environ, RUST_ANALYZER_INTERNALS_DO_NOT_USE="this is unstable"))
    print(f"build_release: {said.stdout.strip()} as {exe}, built by blaze in {time.monotonic() - start:.1f} s",
          flush=True)
    return exe


def glibc_asked(binary):
    """The newest glibc `binary` asks for, as (major, minor), or None for one that asks for none."""
    found = {(int(major), int(minor)) for major, minor in GLIBC_VERSION.findall(binary.read_bytes())}
    return max(found, default=None)


def check_glibc(out):
    """Refuses the build when a binary in `out` asks for a glibc newer than `GLIBC`."""
    shown = ".".join(map(str, GLIBC))
    newer = []
    for binary in sorted(out.iterdir()):
        if binary.is_file() and binary.read_bytes()[:4] == b"\x7fELF":
            asked = glibc_asked(binary)
            if asked and asked > GLIBC:
                newer.append(f"{binary.name} asks for glibc {'.'.join(map(str, asked))}")
    if newer:
        sys.exit(f"error: build_release: {'; '.join(newer)}, newer than RHEL 9's {shown}, so it does not start "
                 "there: build on Ubuntu 22.04, as the release does")
    print(f"build_release: the binaries in {out} ask for glibc {shown} or older", flush=True)


def check_loads(out):
    """Refuses the build when a binary in `out` loads a library `NOT_LOADED` names."""
    system = "darwin" if sys.platform == "darwin" else "linux"
    wrong = []
    for binary in sorted(out.iterdir()):
        if not binary.is_file() or binary.read_bytes()[:4] not in (b"\x7fELF", b"\xcf\xfa\xed\xfe"):
            continue
        argv = ["otool", "-L", binary] if system == "darwin" else ["readelf", "-d", binary]
        shown = subprocess.run(argv, capture_output=True, text=True, check=True).stdout
        lines = shown.splitlines()[1:] if system == "darwin" else [l for l in shown.splitlines() if "(NEEDED)" in l]
        wrong += [f"{binary.name}: {line.strip()}" for line in lines if NOT_LOADED[system].search(line)]
    if wrong:
        sys.exit(f"error: build_release: a shipped binary loads what a user's machine lacks: {'; '.join(wrong)}. "
                 "LLVM and, on Linux, libstdc++ are linked in statically (crates/hir-ty-codegen/build.rs)")
    print(f"build_release: the binaries in {out} load no libLLVM" + ("" if system == "darwin" else " or C++ runtime"),
          flush=True)


def interpreter(binary):
    """The loader a 64-bit ELF executable names (PT_INTERP), or None for one that names none."""
    data = binary.read_bytes()
    if data[:4] != b"\x7fELF" or data[4] != 2:
        return None
    endian = "<" if data[5] == 1 else ">"
    (phoff,) = struct.unpack_from(endian + "Q", data, 0x20)
    phentsize, phnum = struct.unpack_from(endian + "HH", data, 0x36)
    for index in range(phnum):
        header = phoff + index * phentsize
        if struct.unpack_from(endian + "I", data, header)[0] == 3:
            offset, size = struct.unpack_from(endian + "Q", data, header + 8)[0], struct.unpack_from(endian + "Q", data, header + 32)[0]
            return data[offset:offset + size].rstrip(b"\0").decode()
    return None


def check_nix(out):
    """Refuses the build when a binary in `out` would load the system's glibc and not nixpkgs'."""
    system = []
    for binary in sorted(out.iterdir()):
        if binary.is_file() and binary.read_bytes()[:4] == b"\x7fELF":
            loader = interpreter(binary)
            if loader is not None and not loader.startswith("/nix/store/"):
                system.append(f"{binary.name} loads {loader}")
    if system:
        sys.exit(f"error: build_release: --nix, but {'; '.join(system)}, the system's glibc: build it in the flake")
    print(f"build_release: the binaries in {out} load nixpkgs' glibc from /nix/store (--nix: RHEL 9's glibc floor is the release's)", flush=True)


def ship(args, binaries):
    """The binaries copied into `--out`, with blaze's proc-macro server for `--server`."""
    if not args.out:
        return
    args.out.mkdir(parents=True, exist_ok=True)
    for binary in binaries:
        shutil.copy2(binary, args.out / binary.name)
    if args.server:
        proc_macro_server(args.out / "blaze-engine", args.out)
    check_loads(args.out)
    if "linux" in args.target:
        check_nix(args.out) if args.nix else check_glibc(args.out)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--target")
    parser.add_argument("--profdata", help="the pinned profile, already fetched")
    parser.add_argument("--train", type=Path, metavar="DIR", help="train a new profile into DIR")
    parser.add_argument("--pin", nargs="+", metavar="DIR", help="pin the profiles trained into DIR")
    parser.add_argument("--out", type=Path, metavar="DIR", help="copy the binaries into DIR")
    parser.add_argument("--features", help="cargo features of blaze besides `llvm`, which every build has")
    parser.add_argument("--server", action="store_true", help="build blaze's proc-macro server into --out with that blaze")
    parser.add_argument("--server-beside", type=Path, metavar="BLAZE",
                        help="only build blaze's proc-macro server, with BLAZE, beside it (CI's build of blaze)")
    parser.add_argument("--strict", action="store_true", help="fail when the profile is of other crates (a release)")
    parser.add_argument("--no-pgo", action="store_true", help="build without a profile (releases, while PGO is off)")
    parser.add_argument("--show", action="store_true", help="print the build's environment and command")
    parser.add_argument("--nix", action="store_true",
                        help="the flake's build: its Linux binaries load nixpkgs' glibc, not the system's (no RHEL 9 floor)")
    args = parser.parse_args()
    if args.pin:
        return pin(args.pin)
    if args.server_beside:
        blaze = args.server_beside.resolve()
        proc_macro_server(blaze, blaze.parent)
        return
    if not args.target:
        parser.error("--target is required")
    if args.server and not args.out:
        parser.error("--server builds into --out")
    # The server is built from its own directory, where a relative `--out` names nothing.
    args.out = args.out and args.out.resolve()
    if args.train:
        return train(args.target, args.train)
    if args.show:
        chosen = pinned(args.target)
        env, argv = recipe(args.target, f"-Cprofile-use=<{chosen['url']}, sha256 {chosen['sha256']}>", "-fno-profile-use", args.features)
        print(" ".join(f"{k}={shlex.quote(v)}" for k, v in env.items()), " ".join(argv))
        return
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    if args.no_pgo:
        binaries, _ = build(args.target, target_dir, "", "", args.features)
        ship(args, binaries)
        print(f"build_release: {', '.join(b.name for b in binaries)} in {args.out or binaries[0].parent}, "
              "without PGO (--no-pgo)", flush=True)
        return
    if pins()["toolchain"] != TOOLCHAIN and not args.strict:
        # The toolchain moved and the pgo workflow has not trained on it yet (it runs when
        # harness/toolchain changes on main): a build that is not a release goes without PGO.
        print(f"warning: build_release: {PINS} has profiles of Rust {pins()['toolchain']}, blaze builds with "
              f"{TOOLCHAIN}: built without PGO until the pgo workflow trains on {TOOLCHAIN}", file=sys.stderr, flush=True)
        binaries, _ = build(args.target, target_dir, "", "", args.features)
        ship(args, binaries)
        print(f"build_release: {', '.join(b.name for b in binaries)} in {args.out or binaries[0].parent}, "
              "without PGO", flush=True)
        return
    path, chosen = profile(args.target, args.profdata)
    binaries, stale = build(args.target, target_dir, f"-Cprofile-use={path}", "-fno-profile-use", args.features)
    crate = crate_hash(binaries)
    if crate != chosen.get("crate"):
        said = (f"build_release: this build's crates are not the ones the profile was trained on (blaze is crate "
                f"{crate}, the profile's is {chosen.get('crate')}): Cargo.lock, the toolchain, the features or cargo's "
                "settings differ, and the profile finds almost none of the workspace's functions. The pgo workflow trains "
                "a profile of these crates.")
        if args.strict:
            sys.exit(f"error: {said}")
        print(f"warning: {said}", file=sys.stderr, flush=True)
    ship(args, binaries)
    shown = args.out or binaries[0].parent
    print(f"build_release: {', '.join(b.name for b in binaries)} in {shown}, with the profile trained at "
          f"{chosen['commit'][:10]}; {stale} functions changed since and are not optimized by it", flush=True)


if __name__ == "__main__":
    main()
