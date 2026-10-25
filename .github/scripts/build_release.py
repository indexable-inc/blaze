#!/usr/bin/env python3
"""Build ra-jit the one way it ships: the `dist` profile, optimized with a profile of its own
work (PGO). The release workflow, the flake's `daemon` and `cargo install` from a checkout all
build it with this.

  build_release.py --target T [--profdata FILE] [--features F] [--out DIR [--server]]
                                                  # the build, into target/T/dist; DIR gets every binary of ra-jit,
                                                  # and with --server blaze's proc-macro server
  build_release.py --target T --train DIR         # a new profile for T, into DIR (the pgo workflow)
  build_release.py --pin DIR                      # pgo/profiles.json names the profiles in DIR
  build_release.py --target T --show              # the command the build runs, for any other driver

The build is `cargo build -p ra-jit --profile dist --locked --target T` with
`RUSTFLAGS=-Cprofile-use=<profile>`. The profile is the one pgo/profiles.json pins for T: it
is downloaded from the `pgo-profiles` release and refused unless its sha256 is the pinned
one; `--profdata` gives the file instead (the flake fetches it with the same hash, since a nix
build has no network).

A profile is trained by `--train`, in three steps:

  1. the same build instrumented (`-Cprofile-generate`), into target/pgo-instrumented;
  2. that ra-jit run on edits of projects the README's benchmarks do not measure (fd, bat,
     just and hyperfine, at the commits harness/bench.py pins: a cold start and every kind of
     edit, twice, through harness/abbench.py --train);
  3. what it counted merged with the toolchain's llvm-profdata (`rustup component add
     llvm-tools`), refused when it is too small to be the workers' (see below).

A profile trained on older code still applies: LLVM uses it for every function whose control
flow is unchanged and leaves the others unoptimized by it. The build counts what no longer
matches and says so; the pgo workflow trains new profiles when that grows.

Against `cargo build --release` on Linux (docs/reports/RELEASE-BUILD.md): the cold start 14%
shorter on nushell and ripgrep, an edit of a function 8% shorter on nushell, the same memory.

A killed process writes no counters: `cargo ra-jit stop` kills the workers, so abbench ends a
training session by ending the daemon, after which each worker sees its input close and exits
by itself. The `cc` crate passes `-Cprofile-generate`/`-Cprofile-use` on to the C compiler;
on macOS Apple clang's profile format is not rustc's LLVM's and the instrumented ra-jit
crashes at start (rust-lang/rust#163640), so the C code (mimalloc, ring) is built without
them. macOS and Linux; stdlib only.

With `--server` the ra-jit just built builds blaze's proc-macro server (`vendor/proc-macro-srv`,
docs/no-rustc.md step J1) into DIR as `blaze-proc-macro-srv`, the server that expands with the
proc-macro libraries ra-jit links, found beside ra-jit (`RA_JIT_PROC_MACRO_SERVER=1`). It is
built whole by ra-jit (`--exe`), over the standard library ra-jit compiles, so it speaks the
bridge of the libraries that ra-jit compiles; its own serde derives are expanded by the
toolchain's server with rustc's libraries, as blaze's own executable may be built with rustc.
Linux only, where ra-jit links proc-macro libraries so far.
"""

import argparse
import collections
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
HARNESS = ROOT / "harness"
TOOLCHAIN = (HARNESS / "toolchain").read_text().strip()
PINS = ROOT / "pgo" / "profiles.json"
# Where pgo/profiles.json says its files are, when it is first written.
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
# The hash rustc gives the crate ra-jit, in each of its symbols. Cargo derives it from the
# whole graph of the build: every crate's name, version, source and features, the profile
# (whether it builds incrementally too), the toolchain, and a wrapper of rustc around the
# workspace's crates. LLVM finds a function of the
# profile by its symbol, so a build whose hash is not the profile's finds next to nothing in it
# (the crates that did not change below, none of the workspace's), and says nothing.
CRATE = re.compile(rb"Cs([0-9A-Za-z]+)_6ra_jit")


def run(argv, **kwargs):
    print("+", " ".join(map(str, argv)), flush=True)
    return subprocess.run(argv, check=True, **kwargs)


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def recipe(target, rustflags, cflags, features=None):
    """The environment and command of the build for `target`: `cargo build -p ra-jit --profile
    dist` with `rustflags`, and `cflags` for the C code that crates build with `cc`. No wrapper
    of rustc: one around the workspace's crates (`ra-jit enable`'s, nixpkgs' cargo-auditable)
    changes their symbols, and the profile no longer finds them. Cargo hashes the empty
    `RUSTC_WORKSPACE_WRAPPER` too, unlike an unset one, so every build sets it, the training's
    included, whatever cargo's configuration says. Nor incremental, which a developer's
    `CARGO_INCREMENTAL=1` would otherwise turn on for the workspace's crates, changing them as
    well (`0` is `dist`'s own setting, so the hashes are those of a build without it)."""
    triple = target.replace("-", "_")
    env = {"RUSTUP_TOOLCHAIN": TOOLCHAIN, "RUSTFLAGS": rustflags, "RUSTC_WRAPPER": "", "RUSTC_WORKSPACE_WRAPPER": "",
           "CARGO_INCREMENTAL": "0"}
    for name in ("CFLAGS", "CXXFLAGS"):
        env[f"{name}_{triple}"] = " ".join(filter(None, [os.environ.get(f"{name}_{triple}"), cflags]))
    return env, ["cargo", "build", "-p", "ra-jit", "--profile", "dist", "--locked", "--target", target,
                 *(["--features", features] if features else [])]


def build(target, target_dir, rustflags, cflags, features=None):
    """The build into `target_dir`; the result is the binaries it built (every one of ra-jit's:
    what ships is all of them) and how many functions LLVM found changed since the profile."""
    own, argv = recipe(target, rustflags, cflags, features)
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
    """The crate hash of ra-jit, read from the symbols of the binary."""
    binary = next(b for b in binaries if b.stem == "ra-jit")
    found = collections.Counter(CRATE.findall(binary.read_bytes()))
    return found.most_common(1)[0][0].decode() if found else None


def pins():
    return json.loads(PINS.read_text())


def pinned(target):
    table = pins()
    if table["toolchain"] != TOOLCHAIN:
        sys.exit(f"error: build_release: {PINS} has profiles of Rust {table['toolchain']}, ra-jit builds with {TOOLCHAIN}: "
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
    """A new profile for `target` in `out`, and beside it what pgo/profiles.json pins of it."""
    profdata = llvm_tool("llvm-profdata")
    # Short: the daemon's sockets live under it, and a Unix socket's path has a limit.
    work = Path(tempfile.mkdtemp(prefix="rjpgo-", dir="/tmp"))
    raw = work / "raw"
    raw.mkdir()
    built, _ = build(target, ROOT / "target" / "pgo-instrumented", f"-Cprofile-generate={raw}", "-fno-profile-generate")
    binaries, crate = built[0].parent, crate_hash(built)

    # Each process of the instrumented ra-jit writes its counters to a file of its own as it exits.
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

    merged = work / "ra-jit.profdata"
    run([profdata, "merge", "-o", merged, *sorted(raw.glob("*.profraw"))])
    shown = subprocess.run([profdata, "show", merged], capture_output=True, text=True, check=True).stdout
    total = int(re.search(r"^Total count: (\d+)", shown, re.M)[1])
    print(f"build_release: the profile counts {total:,}", flush=True)
    if total < MIN_COUNT:
        sys.exit(f"error: build_release: the profile counts {total:,}, fewer than {MIN_COUNT:,}: the workers' counters were lost")

    digest = sha256(merged)
    commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True, check=True).stdout.strip()
    name = f"ra-jit-{TOOLCHAIN}-{target}-{digest[:16]}.profdata"
    out.mkdir(parents=True, exist_ok=True)
    shutil.move(merged, out / name)
    (out / f"{target}.json").write_text(json.dumps(
        {"target": target, "name": name, "sha256": digest, "commit": commit, "count": total, "crate": crate}, indent=2) + "\n")
    shutil.rmtree(work, ignore_errors=True)
    print(f"build_release: {out / name}", flush=True)


def pin(dirs):
    """pgo/profiles.json names the profiles trained into `dirs`, beside those of other targets."""
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


def proc_macro_server(ra_jit, out):
    """blaze's proc-macro server, built and linked by `ra-jit` into `out`, from a store of its
    own; the path of the executable."""
    exe = out / SERVER_BIN
    with tempfile.TemporaryDirectory(prefix="blaze-server-") as work:
        env = dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN, RA_JIT_CACHE=str(Path(work) / "store"),
                   RA_JIT_STD_FROM="off", RUST_BACKTRACE="0")
        start = time.monotonic()
        # `proc_macro` is not in the standard library's unit: the program that links it has it.
        run([str(ra_jit), str(SERVER), "--crate", SERVER_CRATE, "--crate", "proc_macro",
             "--out", str(Path(work) / "server.o"), "--exe", str(exe)], cwd=SERVER, env=env)
    said = subprocess.run([str(exe), "--version"], capture_output=True, text=True, check=True,
                          env=dict(os.environ, RUST_ANALYZER_INTERNALS_DO_NOT_USE="this is unstable"))
    print(f"build_release: {said.stdout.strip()} as {exe}, built by ra-jit in {time.monotonic() - start:.1f} s",
          flush=True)
    return exe


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--target")
    parser.add_argument("--profdata", help="the pinned profile, already fetched")
    parser.add_argument("--train", type=Path, metavar="DIR", help="train a new profile into DIR")
    parser.add_argument("--pin", nargs="+", metavar="DIR", help="pin the profiles trained into DIR")
    parser.add_argument("--out", type=Path, metavar="DIR", help="copy the binaries into DIR")
    parser.add_argument("--features", help="cargo features of ra-jit (`llvm`); a release has none")
    parser.add_argument("--server", action="store_true", help="build blaze's proc-macro server into --out with that ra-jit")
    parser.add_argument("--strict", action="store_true", help="fail when the profile is of other crates (a release)")
    parser.add_argument("--show", action="store_true", help="print the build's environment and command")
    args = parser.parse_args()
    if args.pin:
        return pin(args.pin)
    if not args.target:
        parser.error("--target is required")
    if args.server and not args.out:
        parser.error("--server builds into --out")
    if args.train:
        return train(args.target, args.train)
    if args.show:
        chosen = pinned(args.target)
        env, argv = recipe(args.target, f"-Cprofile-use=<{chosen['url']}, sha256 {chosen['sha256']}>", "-fno-profile-use", args.features)
        print(" ".join(f"{k}={shlex.quote(v)}" for k, v in env.items()), " ".join(argv))
        return
    path, chosen = profile(args.target, args.profdata)
    binaries, stale = build(args.target, Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")),
                            f"-Cprofile-use={path}", "-fno-profile-use", args.features)
    crate = crate_hash(binaries)
    if crate != chosen.get("crate"):
        said = (f"build_release: this build's crates are not the ones the profile was trained on (ra-jit is crate "
                f"{crate}, the profile's is {chosen.get('crate')}): Cargo.lock, the toolchain, the features or cargo's "
                "settings differ, and the profile finds almost none of the workspace's functions. The pgo workflow trains "
                "a profile of these crates.")
        if args.strict:
            sys.exit(f"error: {said}")
        print(f"warning: {said}", file=sys.stderr, flush=True)
    if args.out:
        args.out.mkdir(parents=True, exist_ok=True)
        for binary in binaries:
            shutil.copy2(binary, args.out / binary.name)
        if args.server:
            proc_macro_server(args.out / "ra-jit", args.out)
    shown = args.out or binaries[0].parent
    print(f"build_release: {', '.join(b.name for b in binaries)} in {shown}, with the profile trained at "
          f"{chosen['commit'][:10]}; {stale} functions changed since and are not optimized by it", flush=True)


if __name__ == "__main__":
    main()
