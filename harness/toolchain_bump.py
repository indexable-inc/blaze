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
    compiler/rustc_abi/src copied as it is, and vendor/proc-macro-srv is upstream's crates at
    the new commit with blaze's changes to them at the old commit carried over (a three-way
    apply; a conflict stops the bump and names the files);
 4. merges the new nightly's standard library into library/ (`library.py merge`): upstream's
    changes since library/UPSTREAM's nightly, applied three-way, so blaze's own changes to it
    are kept; a conflict stops the bump and names the files. It runs when
    the pin is already the nightly's too, so a bump branch made before library/ catches up;
 5. moves the pins written down elsewhere: the old toolchain name, rustc version line and
    commit in install.sh and vendor/proc-macro-srv (RUSTC_VERSION_STRING, the manifest's
    header), the toolchain's names and `rustc -V` in README.md, docs/toolchain.md and
    docs/cargo.md, Cargo.toml's rust-version, and the flake's rust-overlay input (`nix flake
    update rust-overlay`, when nix is on PATH);
 6. installs the nightly with rustup.

What it leaves to you, which it prints at the end: answers.py --write on each of x86-64 Linux,
AArch64 Linux and arm64 macOS (the target specs depend on the host rustc was built for), the
code that mirrors rustc by hand (the Rust ABI in the code generator, vtable.rs, type_id.rs,
symbol names, new intrinsics and lang items: the checks find them), and `checks '{}'`. The rustc
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
import tempfile
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
PINS = os.path.join(ROOT, "crates/blaze-cargo/src/cargo/toolchain/pins.txt")
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
MENTIONS = ["install.sh", "vendor/proc-macro-srv/Cargo.toml",
            "vendor/proc-macro-srv/crates/proc-macro-srv/src/lib.rs"]
# Docs that name the pin as it is today. Only its names and `rustc -V` move there: a bare commit
# in them can be what something was ported from.
DOCS = ["README.md", "docs/toolchain.md", "docs/cargo.md"]
# vendor/proc-macro-srv's manifest and lock file are blaze's, written from upstream's (their
# header says how) and carried as they are; its crates are upstream's (`server_dirs`).
ABI = ("crates/rustc_abi/src", "compiler/rustc_abi/src")


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


def server_dirs():
    """vendor/proc-macro-srv's crates, each with where it is in rust-lang/rust."""
    server = os.path.join(ROOT, "vendor/proc-macro-srv")
    dirs = {f"crates/{name}": f"src/tools/rust-analyzer/crates/{name}"
            for name in sorted(os.listdir(os.path.join(server, "crates")))}
    return dirs | {"compiler/rustc_lexer": "compiler/rustc_lexer"}


def files_under(root):
    """Every file and link under `root`, relative to it."""
    found = set()
    for dir, subdirs, names in os.walk(root):
        for name in names + [d for d in subdirs if os.path.islink(os.path.join(dir, d))]:
            found.add(os.path.relpath(os.path.join(dir, name), root))
    return found


def without_dev_dependencies(root):
    """Drops the `[dev-dependencies]` table of every manifest under `root`: the server is vendored
    without its tests, and dropping the table on both sides of the diff keeps it out of the patch,
    so that upstream editing next to it does not stop the bump."""
    for path in files_under(root):
        if os.path.basename(path) != "Cargo.toml":
            continue
        full = os.path.join(root, path)
        text = Path(full).read_text()
        kept = re.sub(r"(?ms)^\[dev-dependencies\]\n.*?(?=^\[|\Z)", "", text)
        if kept != text:
            Path(full).write_text(kept)


def prune(new_root, base, ours):
    """Removes from the server at the new commit what blaze leaves out: each file blaze deleted
    from upstream at the old commit, and each file upstream added under a directory blaze deleted
    (new tests in a `test_data` blaze does not carry). A file upstream added is kept when the
    nearest directory above it that upstream had at the old commit is still in blaze's copy."""
    base_dirs = {os.path.dirname(f) for f in base} | {""}
    base_dirs |= {d for f in base_dirs for d in ancestors(f)}
    ours_dirs = {os.path.dirname(f) for f in ours} | {""}
    ours_dirs |= {d for f in ours_dirs for d in ancestors(f)}
    for path in files_under(new_root):
        if path in ours:
            continue
        if path in base:
            dropped = True
        else:
            nearest = next(d for d in ancestors(path) if d in base_dirs)
            dropped = nearest not in ours_dirs
        if dropped:
            os.remove(os.path.join(new_root, path))
    for dir, subdirs, names in os.walk(new_root, topdown=False):
        if dir != new_root and not os.listdir(dir):
            os.rmdir(dir)


def ancestors(path):
    """`a/b/c` -> `a/b`, `a`, `` (the root last)."""
    while path:
        path = os.path.dirname(path)
        yield path


async def vendor(old, new):
    server = os.path.join(ROOT, "vendor/proc-macro-srv")
    dirs = server_dirs()
    paths = [ABI[1], *dirs.values()]
    with tempfile.TemporaryDirectory(prefix="blaze-bump-") as work:
        # blaze's changes to the server: upstream at the old commit against vendor/, as a patch
        # of the files both have and those blaze added. What blaze deleted is carried by `prune`.
        repo = await checkout(old, paths)
        for ours, theirs in dirs.items():
            shutil.copytree(os.path.join(repo, theirs), os.path.join(work, "base", ours), symlinks=True)
            shutil.copytree(os.path.join(server, ours), os.path.join(work, "ours", ours), symlinks=True)
        without_dev_dependencies(os.path.join(work, "base"))
        base_files = files_under(os.path.join(work, "base"))
        ours_files = files_under(os.path.join(work, "ours"))
        diff = await asyncio.create_subprocess_exec(
            "git", "diff", "--no-ext-diff", "--no-index", "--binary", "--no-color", "--no-renames",
            "--diff-filter=d", "base", "ours", cwd=work,
            env={k: v for k, v in os.environ.items() if k != "GIT_EXTERNAL_DIFF"},
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        patch, err = await diff.communicate()
        if diff.returncode not in (0, 1):
            raise Failed(f"vendor: diffing vendor/proc-macro-srv with upstream at {old[:9]} exited "
                         f"{diff.returncode}: {err.decode()[-2000:]}")
        # Paths are a/base/... and b/ours/... (b/base/... for a file blaze deleted).
        patch = re.sub(rb"(?m)(^diff --git | |^--- |^\+\+\+ )([ab])/(?:base|ours)/", rb"\1\2/", patch)
        repo = await checkout(new, paths)
        replace_tree(os.path.join(repo, ABI[1]), os.path.join(ROOT, ABI[0]))
        with tempfile.TemporaryDirectory(prefix="blaze-bump-new-") as fresh:
            for ours, theirs in dirs.items():
                shutil.copytree(os.path.join(repo, theirs), os.path.join(fresh, ours), symlinks=True)
            without_dev_dependencies(fresh)
            prune(fresh, base_files, ours_files)
            for ours in dirs:
                replace_tree(os.path.join(fresh, ours), os.path.join(server, ours))
        if not patch.strip():
            return
        patch_file = os.path.join(work, "blaze.patch")
        await asyncio.to_thread(Path(patch_file).write_bytes, patch)
        # A three-way apply needs the old blobs in this repository, which upstream's are not:
        # apply with rejects, so what applies does and each hunk that does not is a .rej file.
        proc = await asyncio.create_subprocess_exec(
            "git", "apply", "--reject", "--whitespace=nowarn", "--directory=vendor/proc-macro-srv",
            patch_file, cwd=ROOT, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        _, err = await proc.communicate()
        if proc.returncode != 0:
            os.makedirs(CACHE, exist_ok=True)
            kept = os.path.join(CACHE, f"proc-macro-srv-{old[:9]}-{new[:9]}.patch")
            shutil.copy(patch_file, kept)
            raise Failed("vendor: blaze's changes to vendor/proc-macro-srv did not all apply at "
                         f"{new[:9]}; resolve the *.rej files under vendor/proc-macro-srv "
                         f"(the whole patch is {kept}), then finish the bump's later steps "
                         f"(move_mentions, flake, rustup) by hand. git apply said:\n{err.decode()[-3000:]}")


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
    await vendor(old[2], new[2])
    await library(date)
    move_mentions(old, new)
    async with asyncio.TaskGroup() as group:
        if shutil.which("nix"):
            group.create_task(run("flake", "nix", "flake", "update", "rust-overlay", bound=600))
        else:
            print("note: nix is not on PATH: run `nix flake update rust-overlay` where it is")
        if shutil.which("rustup"):
            # For rustc's side of the harnesses and an editor's rust-analyzer; blaze reads no
            # rust-src.
            group.create_task(run("rustup", "rustup", "toolchain", "install", name, "--profile", "minimal",
                                  "-c", "rust-src", "-c", "llvm-tools", "-c", "rustfmt", "-c", "clippy",
                                  bound=1200))
    print(f"""
blaze is pinned to {name} ({new[1]}). Left to do:
  - on x86-64 Linux, AArch64 Linux and arm64 macOS each: `python3 harness/answers.py --write`
    (with that host's release build of blaze), then commit crates/blaze-cargo/src/answers/rustc.json;
  - `cargo build -p blaze-engine --release` and fix what the new rustc_abi and std ask for;
  - `uv run --frozen python harness/toolchain_bump.py checks '{{}}'` in an ix VM, under Cranelift and under LLVM
    (`nix develop -c ...` with BLAZE_BACKEND=llvm BLAZE_OPT=2 BLAZE_EMIT=fast);
  - the full tier (uitest.py, libsuite.py, borrowck_ui.py) re-measured at the new commit;
  - `git diff --stat` and one commit for the bump (docs/contributing.md).""")


async def checks():
    """Builds blaze and runs what proves it agrees with the pinned nightly, in parallel."""
    name = (await asyncio.to_thread(Path(PIN).read_text)).strip()
    env = dict(os.environ, RUSTUP_TOOLCHAIN=name)
    await run("build", "cargo", "build", "-p", "blaze-engine", "--release", env=env, bound=3600)
    env.setdefault("BLAZE_BIN", os.path.join(ROOT, "target/release/blaze-engine"))
    harness = [
        ["toolchain_pins.py", "{}"],
        ["answers.py"],
        ["macroserver.py"],
        ["difftest.py", "corpus", "--bin", "runner", "--crate", "cases", "--layouts"],
        *(["ci.py", "run", json.dumps({"part": part})]
          for part in ci.fast_parts(env.get("BLAZE_BACKEND") or "cranelift")),
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
    print(f"blaze agrees with {name} on every check")


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
