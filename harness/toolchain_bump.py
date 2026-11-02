# pyright: basic
"""toolchain_bump.py: move blaze to a newer Rust nightly. blaze supports one toolchain, the
dated nightly in harness/toolchain; everything that mirrors that rustc moves with it.

    uv run --frozen python harness/toolchain_bump.py '{}'
                                              the newest nightly that has every component blaze needs
    uv run --frozen python harness/toolchain_bump.py '{"date": "2026-11-02"}'
    uv run --frozen python harness/toolchain_bump.py checks '{}'
                                              after a bump, in an ix VM: build blaze and run the
                                               checks that prove blaze and the nightly agree

The JSON is inline, a file's path, or `-` for stdin.

A bump, in order (each step names itself when it fails, and stops there):

 1. picks the nightly: the given date, or the newest of the last two weeks whose manifest has
    rustc, cargo, rust-std, rust-src, llvm-tools, rustfmt, clippy, rust-analyzer and (but on
    Windows) rustc-codegen-cranelift-preview for every host blaze ships for;
 2. writes harness/toolchain and runs toolchain_pins.py '{"write": true}' (pins.txt: the files' hashes,
    the nightly's `rustc -V` and its rust-lang/rust commit);
 3. re-vendors from rust-lang/rust at the new commit: crates/rustc_abi/src is
    compiler/rustc_abi/src and crates/rustc_lexer/src is compiler/rustc_lexer/src, copied as
    they are;
 3a. moves the trait solver and the format-string parser (`tools/v2/solver_sync.py`):
    vendor/rustc_type_ir, vendor/rustc_next_trait_solver, vendor/rustc_type_ir_macros,
    vendor/rustc_transmute and vendor/rustc_parse_format become upstream's crates at the new commit with `PATCHES.md`'s series replayed (exact context, no fuzz); a patch that no
    longer applies, or a changed upstream `Cargo.toml`, stops the bump and names it;
 4. merges the new nightly's standard library into library/ (`library.py merge`): upstream's
    changes since library/UPSTREAM's nightly, applied three-way, so blaze's own changes to it
    are kept; a conflict stops the bump and names the files. It runs when
    the pin is already the nightly's too, so a bump branch made before library/ catches up;
 5. moves the pins written down elsewhere: the old toolchain name, rustc version line and
    commit in install.sh, the toolchain's names and `rustc -V` in README.md, docs/toolchain.md and
    docs/cargo.md, Cargo.toml's rust-version, and the flake's rust-overlay input (`nix flake
    update rust-overlay`, when nix is on PATH);
 6. installs the nightly with rustup.

What it leaves to you, which it prints at the end: answers.py --write on each of x86-64 Linux,
AArch64 Linux and arm64 macOS (the target specs depend on the host rustc was built for), the
code that mirrors rustc by hand (the Rust ABI in the code generator, vtable.rs, type_id.rs,
symbol names, new intrinsics and lang items: the checks find them), and `checks '{}'`. That
includes blaze's proc-macro server, crates/blaze-macro-eval, which speaks the pinned
`library/proc_macro` bridge's protocol by hand and checks it at load (`protocol::check`), so a
bridge that changed is an error there to fix, not a copy to re-vendor. The rustc
and the daemon match by the whole `rustc -V` line, so a binary built before the bump refuses
the new nightly until it is rebuilt. docs/contributing.md, "Moving to a newer nightly".
"""
import asyncio
import datetime
import json
import os
import re
import shutil
import sys
import tomllib
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path

import ci
from typed_input import fail, parse

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
PIN = os.path.join(HERE, "toolchain")
PINS = os.path.join(ROOT, "crates/blaze-cargo/src/toolchain/pins.txt")
DIST = "https://static.rust-lang.org/dist"
RUST = "https://github.com/rust-lang/rust"
CACHE = os.path.join(os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache"), "blaze-bump")
HOSTS = ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
         "x86_64-pc-windows-msvc"]
# rust-src is not blaze's (it compiles its own library/): the bump imports library/ from it
# (library.py merge), and rustc's side of the harnesses names std's files where it installs.
COMPONENTS = ["rustc", "cargo", "rust-std", "rust-src", "llvm-tools-preview", "rustfmt-preview",
              "clippy-preview", "rust-analyzer-preview", "rustc-codegen-cranelift-preview"]
# Where the old pin is written down besides harness/toolchain and pins.txt.
MENTIONS = ["install.sh", "crates/rustc_lexer/Cargo.toml"]
# Docs that name the pin as it is today. Only its names and `rustc -V` move there: a bare commit
# in them can be what something was ported from.
DOCS = ["README.md", "docs/toolchain.md", "docs/cargo.md"]
ABI = ("crates/rustc_abi/src", "compiler/rustc_abi/src")
LEXER = ("crates/rustc_lexer/src", "compiler/rustc_lexer/src")


class Failed(Exception):
    pass


async def run(step, *argv, cwd=ROOT, bound=600, env=None, stdin=None):
    """Runs a command; a failure names the step, the command, its exit code and its stderr."""
    try:
        async with asyncio.timeout(bound):
            proc = await asyncio.create_subprocess_exec(
                *argv, cwd=cwd, env=env, stdin=asyncio.subprocess.PIPE if stdin else None,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
            out, err = await proc.communicate(stdin)
    except TimeoutError:
        raise Failed(f"{step}: `{' '.join(argv)}` took over {bound} s") from None
    except FileNotFoundError:
        raise Failed(f"{step}: `{argv[0]}` is not on PATH") from None
    if proc.returncode != 0:
        tail = "\n".join(err.decode(errors="replace").splitlines()[-25:])
        raise Failed(f"{step}: `{' '.join(argv)}` exited {proc.returncode}:\n{tail}")
    return out.decode(errors="replace")


async def fetch(url, bound=60):
    def get():
        with urllib.request.urlopen(url, timeout=bound) as answer:
            return answer.read()
    async with asyncio.timeout(bound):
        return await asyncio.to_thread(get)


async def manifest(date):
    """The nightly of `date`'s channel manifest, or None when the Rust project published none
    (404). Any other failure is tried again and then stops the bump: read as "none", a slow
    answer would quietly pin an older nightly than the newest."""
    url = f"{DIST}/{date}/channel-rust-nightly.toml"
    for wait in (0, 5, 30):
        await asyncio.sleep(wait)
        try:
            data = await fetch(url)
        except urllib.error.HTTPError as error:
            if error.code == 404:
                return None
            why = f"HTTP {error.code}"
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            why = repr(error)
        else:
            return tomllib.loads(data.decode())
    raise Failed(f"pick: {url} could not be read after three tries: {why}")


def missing(channel):
    """What blaze needs that the nightly of `channel` lacks."""
    lacking = []
    for package in COMPONENTS:
        for host in HOSTS:
            if package == "rustc-codegen-cranelift-preview" and "windows" in host:
                continue
            target = "*" if package == "rust-src" else host
            built = channel["pkg"].get(package, {}).get("target", {}).get(target, {})
            if not built.get("available"):
                lacking.append(f"{package} for {target}")
    return lacking


async def pick(date):
    if date:
        channel = await manifest(date)
        if channel is None:
            raise Failed(f"pick: there is no nightly of {date} at {DIST}/{date}")
        if lacking := missing(channel):
            raise Failed(f"pick: the nightly of {date} lacks {', '.join(lacking)}")
        return date, channel
    today = datetime.date.today()
    dates = [(today - datetime.timedelta(days=back)).isoformat() for back in range(14)]
    async with asyncio.TaskGroup() as group:
        tasks = {date: group.create_task(manifest(date)) for date in dates}
    for candidate in dates:
        channel = tasks[candidate].result()
        if channel is not None and not missing(channel):
            return candidate, channel
        print(f"nightly-{candidate}: {'none published' if channel is None else 'lacks ' + ', '.join(missing(channel))}")
    raise Failed("pick: no nightly of the last two weeks has every component blaze needs")


def pinned():
    """The pin as pins.txt has it: toolchain name, `rustc -V` after `rustc `, commit."""
    lines = dict(line.split(" ", 1) for line in Path(PINS).read_text().splitlines() if line and not line.startswith("#"))
    return lines["release"].split()[0], lines["rustc"], lines["commit"]


async def checkout(commit, paths):
    """rust-lang/rust's `paths` at `commit`, in a cached sparse clone; returns its directory."""
    repo = os.path.join(CACHE, "rust")
    if not os.path.isdir(os.path.join(repo, ".git")):
        os.makedirs(repo, exist_ok=True)
        await run("vendor: clone", "git", "init", "-q", repo)
        await run("vendor: clone", "git", "-C", repo, "remote", "add", "origin", RUST)
    await run("vendor: sparse", "git", "-C", repo, "sparse-checkout", "set", *paths)
    await run(f"vendor: fetch {commit[:9]}", "git", "-C", repo, "fetch", "-q", "--depth", "1",
              "--filter=blob:none", "origin", commit, bound=1800)
    await run(f"vendor: checkout {commit[:9]}", "git", "-C", repo, "checkout", "-q", "-f", commit)
    return repo


def replace_tree(src, dst):
    shutil.rmtree(dst, ignore_errors=True)
    shutil.copytree(src, dst, symlinks=True)


async def vendor(new):
    """crates/rustc_abi/src and crates/rustc_lexer/src as rust-lang/rust has them at `new`."""
    repo = await checkout(new, [ABI[1], LEXER[1]])
    for ours, theirs in (ABI, LEXER):
        replace_tree(os.path.join(repo, theirs), os.path.join(ROOT, ours))


def move_mentions(old, new):
    (old_name, old_rustc, old_commit), (new_name, new_rustc, new_commit) = old, new
    pairs = [(old_name, new_name), (old_rustc, new_rustc), (old_commit, new_commit),
             (old_commit[:9], new_commit[:9])]
    for path in MENTIONS:
        full = os.path.join(ROOT, path)
        text = Path(full).read_text()
        moved = text
        for was, now in pairs:
            moved = moved.replace(was, now)
        if moved == text:
            print(f"note: {path} names none of the old pin ({old_name}, {old_commit[:9]})")
        Path(full).write_text(moved)
    blaze = lambda name: name.replace("nightly-", "blaze-", 1)
    for path in DOCS:
        full = os.path.join(ROOT, path)
        text = Path(full).read_text()
        moved = text
        for was, now in [(old_name, new_name), (blaze(old_name), blaze(new_name)), (old_rustc, new_rustc)]:
            moved = moved.replace(was, now)
        Path(full).write_text(moved)
    major_minor = ".".join(new_rustc.split(" ")[0].split(".")[:2])
    cargo = os.path.join(ROOT, "Cargo.toml")
    text = Path(cargo).read_text()
    moved, count = re.subn(r'(?m)^rust-version = "[^"]*"', f'rust-version = "{major_minor}"', text, count=1)
    if count != 1:
        raise Failed("mentions: Cargo.toml has no `rust-version = ...` line")
    Path(cargo).write_text(moved)


async def library(date):
    """library/ merged to the standard library of the nightly of `date` (library.py merge)."""
    said = await run("library", sys.executable, os.path.join(HERE, "library.py"), "merge",
                     json.dumps({"date": date}), bound=900)
    print(said, end="")


async def bump(date):
    old = pinned()
    date, channel = await pick(date)
    name = f"nightly-{date}"
    if name == old[0]:
        print(f"blaze is already on {name}")
        await library(date)
        return
    print(f"moving {old[0]} ({old[1]}) to {name} ({channel['pkg']['rustc']['version']})")
    await asyncio.to_thread(Path(PIN).write_text, name + "\n")
    await run("pins", sys.executable, os.path.join(HERE, "toolchain_pins.py"), '{"write": true}', bound=900)
    new = pinned()
    await vendor(new[2])
    await run("solver", sys.executable, os.path.join(ROOT, "tools/v2/solver_sync.py"),
              json.dumps({"nightly": name}), bound=1800)
    await library(date)
    move_mentions(old, new)
    async with asyncio.TaskGroup() as group:
        if shutil.which("nix"):
            group.create_task(run("flake", "nix", "flake", "update", "rust-overlay", bound=600))
        else:
            print("note: nix is not on PATH: run `nix flake update rust-overlay` where it is")
        if shutil.which("rustup"):
            # For cargo's link line in the harnesses and an editor's rust-analyzer; blaze reads
            # no rust-src.
            group.create_task(run("rustup", "rustup", "toolchain", "install", name, "--profile", "minimal",
                                  "-c", "rust-src", "-c", "llvm-tools", "-c", "rustfmt", "-c", "clippy",
                                  bound=1200))
    print(f"""
blaze is pinned to {name} ({new[1]}). Left to do:
  - on x86-64 Linux, AArch64 Linux and arm64 macOS each: `python3 harness/answers.py --write`,
    then commit crates/blaze-units/src/stdlib/rustc.json;
  - `cargo build -p blaze-engine --release` and fix what the new rustc_abi and std ask for;
  - `uv run --frozen python harness/toolchain_bump.py checks '{{}}'` in an ix VM (`nix develop -c ...`,
    a blaze built with `--features llvm`: the release profile is LLVM's);
  - the full tier (the train and the crates' own test suites) re-measured at the new commit;
  - `git diff --stat` and one commit for the bump (docs/contributing.md).""")


async def checks():
    """Builds blaze and runs its pins and the fast tier against the pinned nightly, in parallel."""
    name = (await asyncio.to_thread(Path(PIN).read_text)).strip()
    env = dict(os.environ, RUSTUP_TOOLCHAIN=name)
    await run("build", "cargo", "build", "-p", "blaze-engine", "--release", env=env, bound=3600)
    env.setdefault("BLAZE_BIN", os.path.join(ROOT, "target/release/blaze-engine"))
    harness = [
        ["toolchain_pins.py", "{}"],
        *(["ci.py", "run", json.dumps({"part": part})]
          for part in ci.fast_parts()),
    ]
    failed = []

    async def one(argv):
        try:
            await run(f"check {argv[0]}", sys.executable, *argv, cwd=HERE, env=env, bound=7200)
            print(f"ok      {' '.join(argv)}")
        except Failed as error:
            failed.append(str(error))
            print(f"FAILED  {' '.join(argv)}")

    async with asyncio.TaskGroup() as group:
        for argv in harness:
            group.create_task(one(argv))
    if failed:
        raise Failed("checks:\n\n" + "\n\n".join(failed))
    print(f"blaze passes every check on {name}")


@dataclass(frozen=True)
class Bump:
    # The nightly's date, YYYY-MM-DD; empty for the newest usable.
    date: str = ""


@dataclass(frozen=True)
class Checks:
    pass


def main():
    argv = sys.argv[1:]
    checking = bool(argv) and argv[0] == "checks"
    if checking:
        argv = argv[1:]
    if len(argv) != 1:
        fail("toolchain_bump.py", "usage: toolchain_bump.py [checks] JSON (see the top of the file)")
    given = argv[0]
    if checking:
        parse(Checks, given, "toolchain_bump.py")
        work = checks()
    else:
        options = parse(Bump, given, "toolchain_bump.py")
        if options.date and not re.fullmatch(r"\d{4}-\d\d-\d\d", options.date):
            fail("toolchain_bump.py", f"field date is YYYY-MM-DD, not {options.date!r}")
        work = bump(options.date)
    try:
        asyncio.run(work)
    except Failed as error:
        sys.exit(f"error: toolchain_bump: {error}")


if __name__ == "__main__":
    main()
