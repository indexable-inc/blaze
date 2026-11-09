"""Moves a vendored third-party crate to a crates.io version: `vendor/<crate>` becomes that
version's published `.crate`, checked against the crates.io index (and `Cargo.lock`, when the lock
has it), with blaze's patch series (`vendor/<crate>/PATCHES.md`) replayed on top
(docs/v2/plan/design/no-tracing.md, section 6.2). Run it on every bump of the crate, and with
the current version to re-check that the tree is upstream plus the series.

    uv run --frozen tools/vendor/sync.py '{"crate": "quinn-proto", "version": "0.11.19"}'

Input (inline JSON, a file's path, or `-`):
- `crate`: `quinn-proto` or `ra-ap-rustc_pattern_analysis`.
- `version`: the crates.io version to move to, exactly as the index lists it.
- `check`: true to write nothing and fail when `vendor/<crate>` (its tree or its patches) is not
  what the replay makes; false (the default) to write the result.
- `root`: the workspace; empty for the one holding this script.

In order, each step stopping the sync with an error that names it (and, for a patch, the hunks
that no longer apply):
1. `vendor/<crate>/PATCHES.md`: its `Base:` and `Left out:` lines, and its `### NNNN-name`
   headings, one for one with `patches/`;
2. the version's checksum from the crates.io sparse index (a yanked version is refused) and, in
   parallel, from `Cargo.lock` when the lock has the crate at that version: the two must agree;
3. the `.crate`, downloaded into a new, empty directory, its SHA-256 checked against the index,
   and extracted there with tarfile's `data` filter (no links out, no absolute paths, nothing
   outside `<crate>-<version>/`);
4. the base: the extracted crate without the `Left out:` entries; then, in a scratch tree, each
   patch in order with `git apply` (exact context, no fuzz). The first patch that does not apply
   stops the sync, naming the crate, the patch and each hunk (file and `@@` line) that failed,
   or saying upstream already has it;
5. the replayed tree must name no `tracing`: no `tracing` in a manifest, no `tracing` path, span
   macro, `#[instrument]`, `.instrument(..)` or bare `trace!`/`debug!`/`info!`/`warn!`/`error!`
   in Rust. A hit stops the sync naming file and line: upstream added a site the series must
   delete (a new patch, or `0001-no-tracing` ported);
6. only when all of that passed: `vendor/<crate>` (all but `PATCHES.md`, `patches/` and the
   gate's `Cargo.lock`) is replaced by the replayed tree, each patch is rewritten as the diff between its stages (so its
   context is the new base's), and the `Base:` line names the new version and checksum. With
   `check`, nothing is written; a difference is the error.

`Cargo.lock` is not touched: after a bump, `blaze generate-lockfile` (or the next build) moves it,
and the lockfile ban in `blaze check` refuses it if `tracing` came back.
"""

import asyncio
import difflib
import hashlib
import json
import os
import re
import shutil
import sys
import tarfile
import tempfile
import tomllib
import typing
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from typed_input import fail, parse

SCRIPT = "sync.py"
type Crate = Literal["quinn-proto", "ra-ap-rustc_pattern_analysis"]
INDEX = "https://index.crates.io"
DOWNLOAD = "https://static.crates.io/crates"
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
VERSION = re.compile(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?")
SHA256 = re.compile(r"[0-9a-f]{64}")
BASE = re.compile(r"(?m)^Base: crates\.io `(?P<crate>[a-z0-9_-]+)` (?P<version>\S+), sha256 `(?P<sha>[0-9a-f]{64})`\.$")
LEFT_OUT = re.compile(r"(?m)^Left out: (?P<names>(?:`[^`/]+`(?:, )?)+)\.$")
HEADING = re.compile(r"(?m)^### (\d{4}-[a-z0-9][a-z0-9-]*)$")
# What the replayed tree must not name (step 5), per kind of file.
IN_MANIFEST = re.compile(r"\btracing\b")
IN_RUST = re.compile(
    r"\btracing\b|\b(?:trace|debug|info|warn|error)_span!|#\[(?:tracing::)?instrument\b|\.instrument\("
    r"|(?<![\w:])(?:trace|debug|info|warn|error)!\s*\("
)
# Kept as upstream ships them; the patches never touch them.
UNSCANNED = frozenset({"Cargo.toml.orig"})
# blaze's own entries of `vendor/<crate>`, which the sync keeps: the series, and the lock the gate
# (`blaze test` run in the crate's directory, its own workspace) writes and the checkout keeps.
OURS = frozenset({"PATCHES.md", "patches", "Cargo.lock"})
CRATE_BYTES = 64 << 20
# Git, as the sync runs it: no user diff drivers, colors, pagers or repository of the caller.
GIT_ENV = {k: v for k, v in os.environ.items() if k not in ("GIT_EXTERNAL_DIFF", "GIT_DIR", "GIT_WORK_TREE")}


@dataclass(frozen=True)
class Input:
    crate: Crate
    version: str
    check: bool = False
    root: str = ""


class Failed(Exception):
    pass


@dataclass(frozen=True)
class Series:
    """A crate's `PATCHES.md`: the base it was made on, what is left out of it, and its patches."""

    version: str
    sha256: str
    left_out: frozenset[str]
    patches: tuple[str, ...]


def read_series(crate: Crate, vendored: Path) -> Series:
    """`vendored`'s series; its headings and `patches/` must name the same files."""
    step = f"{crate}: PATCHES.md"
    path = vendored / "PATCHES.md"
    if not path.is_file():
        raise Failed(f"{step}: vendor/{crate}/PATCHES.md does not exist")
    text = path.read_text()
    base = BASE.search(text)
    if base is None:
        raise Failed(f"{step}: no line `Base: crates.io `{crate}` <version>, sha256 `<hex>`.`")
    if base["crate"] != crate:
        raise Failed(f"{step}: its Base line names crate `{base['crate']}`, not `{crate}`")
    left = LEFT_OUT.search(text)
    if left is None:
        raise Failed(f"{step}: no line `Left out: `<name>`, `<name>`.` (top-level entries of the .crate)")
    names = tuple(HEADING.findall(text))
    if not names:
        raise Failed(f"{step}: no `### NNNN-name` heading; the series starts with 0001-no-tracing")
    if len(set(names)) != len(names):
        raise Failed(f"{step}: a patch is named twice")
    if names != tuple(sorted(names)):
        raise Failed(f"{step}: the patches are not in the order of their numbers")
    patches = vendored / "patches"
    on_disk = {p.stem for p in patches.glob("*.patch")} if patches.is_dir() else set[str]()
    if unlisted := sorted(on_disk - set(names)):
        raise Failed(f"{step}: patches/ has {', '.join(unlisted)}, which PATCHES.md doesn't list")
    if missing := sorted(set(names) - on_disk):
        raise Failed(f"{step}: PATCHES.md lists {', '.join(missing)}, which patches/ doesn't have")
    left_out = frozenset(re.findall(r"`([^`]+)`", left["names"]))
    return Series(base["version"], base["sha"], left_out, names)


def write_base(vendored: Path, crate: Crate, version: str, sha256: str) -> None:
    path = vendored / "PATCHES.md"
    line = f"Base: crates.io `{crate}` {version}, sha256 `{sha256}`."
    path.write_text(BASE.sub(line.replace("\\", "\\\\"), path.read_text(), count=1))


def index_path(crate: str) -> str:
    """The crate's file in the sparse index (cargo's layout)."""
    name = crate.lower()
    match len(name):
        case 1:
            return f"1/{name}"
        case 2:
            return f"2/{name}"
        case 3:
            return f"3/{name[0]}/{name}"
        case _:
            return f"{name[:2]}/{name[2:4]}/{name}"


def http_get(step: str, url: str, limit: int) -> bytes:
    """`url`'s body, at most `limit` bytes, or an error naming `step` and the URL."""
    request = urllib.request.Request(url, headers={"User-Agent": "blaze tools/vendor/sync.py"})
    try:
        with urllib.request.urlopen(request, timeout=60) as answer:
            body = typing.cast(bytes, answer.read(limit + 1))
    except urllib.error.HTTPError as error:
        raise Failed(f"{step}: {url} answered HTTP {error.code}") from None
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        raise Failed(f"{step}: {url} could not be read: {error!r}") from None
    if len(body) > limit:
        raise Failed(f"{step}: {url} is over {limit} bytes")
    return body


async def from_index(crate: Crate, version: str) -> str:
    """`version`'s SHA-256 as the crates.io index lists it."""
    step = f"{crate} {version}: index"
    url = f"{INDEX}/{index_path(crate)}"
    async with asyncio.timeout(120):
        body = await asyncio.to_thread(http_get, step, url, 16 << 20)
    for number, line in enumerate(body.decode().splitlines(), 1):
        try:
            entry: object = json.loads(line)
        except json.JSONDecodeError as error:
            raise Failed(f"{step}: {url} line {number} is not JSON: {error}") from None
        if not isinstance(entry, dict):
            raise Failed(f"{step}: {url} line {number} is not a JSON object")
        fields = typing.cast(dict[str, object], entry)
        if fields.get("vers") != version:
            continue
        if fields.get("yanked") is True:
            raise Failed(f"{step}: {crate} {version} is yanked on crates.io; pick another version")
        sha = fields.get("cksum")
        if not isinstance(sha, str) or not SHA256.fullmatch(sha):
            raise Failed(f"{step}: {url} line {number} has no SHA-256 `cksum`")
        return sha
    raise Failed(f"{step}: crates.io has no {crate} {version} ({url})")


async def from_lock(crate: Crate, version: str, root: Path) -> str | None:
    """`version`'s checksum in `Cargo.lock`, or None when the lock does not have it."""
    step = f"{crate} {version}: Cargo.lock"
    try:
        lock = tomllib.loads((root / "Cargo.lock").read_text())
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise Failed(f"{step}: {root / 'Cargo.lock'} could not be read: {error}") from None
    packages = lock.get("package", [])
    if not isinstance(packages, list):
        raise Failed(f"{step}: `package` is not an array of tables")
    for package in typing.cast(list[object], packages):
        if not isinstance(package, dict):
            continue
        fields = typing.cast(dict[str, object], package)
        if fields.get("name") == crate and fields.get("version") == version and fields.get("source") == REGISTRY:
            sha = fields.get("checksum")
            if not isinstance(sha, str) or not SHA256.fullmatch(sha):
                raise Failed(f"{step}: {crate} {version} has no SHA-256 checksum")
            return sha
    return None


def extract(step: str, archive: Path, into: Path, top: str) -> Path:
    """`archive` (a .crate, gzipped tar) extracted in `into`, everything under `top/`."""
    try:
        with tarfile.open(archive, mode="r:gz") as tar:
            for member in tar.getmembers():
                parts = Path(member.name).parts
                if not parts or parts[0] != top or ".." in parts:
                    raise Failed(f"{step}: {archive.name} has entry {member.name!r}, outside {top}/")
                if not (member.isfile() or member.isdir()):
                    raise Failed(f"{step}: {archive.name} has {member.name!r}, which is not a file or directory")
            tar.extractall(into, filter="data")
    except (tarfile.TarError, OSError) as error:
        raise Failed(f"{step}: {archive.name} could not be extracted: {error}") from None
    tree = into / top
    if not (tree / "Cargo.toml").is_file():
        raise Failed(f"{step}: {archive.name} has no {top}/Cargo.toml")
    return tree


async def fetch(crate: Crate, version: str, sha256: str, scratch: Path) -> Path:
    """The published crate, checked against `sha256`, extracted in a new, empty directory."""
    step = f"{crate} {version}: download"
    url = f"{DOWNLOAD}/{crate}/{crate}-{version}.crate"
    async with asyncio.timeout(600):
        body = await asyncio.to_thread(http_get, step, url, CRATE_BYTES)
    got = hashlib.sha256(body).hexdigest()
    if got != sha256:
        raise Failed(f"{step}: {url} has SHA-256 {got}, not the index's {sha256}")
    into = Path(tempfile.mkdtemp(prefix="crate-", dir=scratch))
    archive = into / f"{crate}-{version}.crate"
    archive.write_bytes(body)
    return await asyncio.to_thread(extract, f"{crate} {version}: extract", archive, into, f"{crate}-{version}")


async def git(step: str, *argv: str, cwd: Path, bound: float = 300, ok: tuple[int, ...] = (0,)) -> tuple[int, bytes, bytes]:
    """`git argv` in `cwd`, finding no repository at or above it: inside one, `git apply` would
    take the patch's paths from that repository's top, not from `cwd`."""
    env = dict(GIT_ENV, GIT_CEILING_DIRECTORIES=str(cwd.parent))
    try:
        process = await asyncio.create_subprocess_exec(
            "git", *argv, cwd=cwd, env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
        )
    except FileNotFoundError:
        raise Failed(f"{step}: `git` is not on PATH") from None
    try:
        async with asyncio.timeout(bound):
            out, err = await process.communicate()
    except TimeoutError:
        process.kill()
        await process.wait()
        raise Failed(f"{step}: `git {' '.join(argv)}` took over {bound} s") from None
    code = process.returncode
    assert code is not None
    if code not in ok:
        tail = "\n".join(err.decode(errors="replace").splitlines()[-25:])
        raise Failed(f"{step}: `git {' '.join(argv)}` exited {code}:\n{tail}")
    return code, out, err


@dataclass(frozen=True)
class Hunk:
    path: str
    header: str
    text: str


def hunks(patch: str) -> list[Hunk]:
    """Each hunk of `patch` as a patch of its own: its file's header, then the hunk."""
    found: list[Hunk] = []
    for section in re.split(r"(?m)^(?=diff --git )", patch):
        if not section.startswith("diff --git "):
            continue
        parts = re.split(r"(?m)^(?=@@ )", section)
        head = parts[0]
        target = re.search(r"(?m)^\+\+\+ (?:b/)?(\S+)", head) or re.search(r"(?m)^--- (?:a/)?(\S+)", head)
        path = target.group(1) if target else head.split("\n", 1)[0]
        for body in parts[1:]:
            found.append(Hunk(path, body.split("\n", 1)[0], head + body))
    return found


async def failing_hunks(step: str, patch: Path, tree: Path) -> list[str]:
    """The hunks of `patch` that do not apply to `tree`, each checked on its own."""
    lines: list[str] = []
    with tempfile.TemporaryDirectory(prefix="hunk-") as held:
        for number, hunk in enumerate(hunks(patch.read_text()), 1):
            one = Path(held) / f"{number}.patch"
            one.write_text(hunk.text)
            code, _, _ = await git(step, "apply", "--check", str(one), cwd=tree, ok=(0, 1, 128))
            if code != 0:
                lines.append(f"  hunk {number}: {hunk.path} {hunk.header}")
    return lines


async def apply(crate: Crate, name: str, patch: Path, tree: Path, version: str) -> None:
    """`patch` applied to `tree`, or an error naming it, its failing hunks, or that upstream has it."""
    step = f"{crate}: {name}"
    code, _, err = await git(step, "apply", "--check", str(patch), cwd=tree, ok=(0, 1, 128))
    if code == 0:
        await git(step, "apply", str(patch), cwd=tree)
        return
    reverse, _, _ = await git(step, "apply", "--check", "--reverse", str(patch), cwd=tree, ok=(0, 1, 128))
    if reverse == 0:
        raise Failed(
            f"{crate}: patch {name} no longer applies to {version}: upstream already has it. Drop it from "
            f"vendor/{crate}/PATCHES.md and patches/, saying so in PATCHES.md, and run again."
        )
    failing = await failing_hunks(step, patch, tree)
    said = "\n".join(err.decode(errors="replace").splitlines()[-15:])
    where = "\n".join(failing) if failing else "  (every hunk applies alone; together they conflict)"
    raise Failed(
        f"{crate}: patch {name} no longer applies to {version}. Port it to upstream's new code "
        f"(vendor/{crate}/patches/{name}.patch, what and why in PATCHES.md) and run again. Failing hunks:\n"
        f"{where}\ngit apply said:\n{said}"
    )


async def stage_diff(crate: Crate, name: str, stage: Path) -> bytes:
    """The patch from `stage/a` to `stage/b`, with paths `a/...` and `b/...`."""
    argv = ("-c", "core.quotepath=off", "diff", "--no-index", "--no-prefix", "--no-color", "--no-renames")
    _, out, _ = await git(f"{crate}: {name}: diff", *argv, "--no-ext-diff", "a", "b", cwd=stage, ok=(0, 1))
    return out


def copy_tree(src: Path, dst: Path, left_out: frozenset[str] = frozenset()) -> None:
    def ignore(directory: str, names: list[str]) -> set[str]:
        return {n for n in names if n in left_out} if Path(directory) == src else set()

    shutil.copytree(src, dst, symlinks=True, ignore=ignore)


async def replay(crate: Crate, base: Path, series: Series, patches: Path, work: Path, version: str) -> tuple[Path, list[bytes]]:
    """`base` without the left-out entries, with the series applied, and each patch
    regenerated against its stage."""
    tree = work / "tree"
    copy_tree(base, tree, series.left_out)
    regenerated: list[bytes] = []
    for name in series.patches:
        stage = work / "stage"
        shutil.rmtree(stage, ignore_errors=True)
        stage.mkdir()
        copy_tree(tree, stage / "a")
        await apply(crate, name, patches / f"{name}.patch", tree, version)
        copy_tree(tree, stage / "b")
        regenerated.append(await stage_diff(crate, name, stage))
    return tree, regenerated


def leftovers(crate: Crate, tree: Path) -> list[str]:
    """Each line of `tree` that still names tracing (step 5)."""
    hits: list[str] = []
    for path in sorted(tree.rglob("*")):
        if not path.is_file() or path.name in UNSCANNED:
            continue
        if path.name == "Cargo.toml":
            pattern = IN_MANIFEST
        elif path.suffix == ".rs":
            pattern = IN_RUST
        else:
            continue
        for number, line in enumerate(path.read_text(errors="replace").splitlines(), 1):
            if pattern.search(line):
                hits.append(f"  vendor/{crate}/{path.relative_to(tree)}:{number}: {line.strip()}")
    return hits


def files(top: Path, skip: frozenset[str] = frozenset()) -> dict[str, Path]:
    """Every file and symlink under `top` by its path relative to `top`, but the top-level `skip`."""
    found: dict[str, Path] = {}
    for path in top.rglob("*"):
        relative = path.relative_to(top)
        if relative.parts[0] in skip or (path.is_dir() and not path.is_symlink()):
            continue
        found[relative.as_posix()] = path
    return found


def same(left: Path, right: Path) -> bool:
    if left.is_symlink() or right.is_symlink():
        return left.is_symlink() and right.is_symlink() and os.readlink(left) == os.readlink(right)
    return left.read_bytes() == right.read_bytes()


def differences(vendored: Path, tree: Path) -> list[str]:
    """Paths where `vendored` (but `OURS`) and `tree` differ, byte for byte."""
    ours = files(vendored, OURS)
    theirs = files(tree)
    found = [f"  only in vendor/{vendored.name}: {name}" for name in sorted(ours.keys() - theirs.keys())]
    found += [f"  missing from vendor/{vendored.name}: {name}" for name in sorted(theirs.keys() - ours.keys())]
    found += [f"  differs: {name}" for name in sorted(ours.keys() & theirs.keys()) if not same(ours[name], theirs[name])]
    return found


def unified(old: bytes, new: bytes, label: str) -> str:
    lines = difflib.unified_diff(
        old.decode(errors="replace").splitlines(keepends=True),
        new.decode(errors="replace").splitlines(keepends=True),
        f"{label} (stored)",
        f"{label} (replayed)",
    )
    return "".join(list(lines)[:40])


async def sync(given: Input) -> None:
    if not VERSION.fullmatch(given.version):
        raise Failed(f"field version is a crates.io version such as 0.11.12, not {given.version!r}")
    root = Path(given.root) if given.root else Path(__file__).resolve().parents[2]
    vendored = root / "vendor" / given.crate
    series = read_series(given.crate, vendored)
    try:
        async with asyncio.TaskGroup() as group:
            indexed = group.create_task(from_index(given.crate, given.version))
            locked = group.create_task(from_lock(given.crate, given.version, root))
    except* Failed as failed:
        raise Failed("\n".join(str(error) for error in failed.exceptions)) from None
    sha256, in_lock = indexed.result(), locked.result()
    if in_lock is not None and in_lock != sha256:
        raise Failed(f"{given.crate} {given.version}: Cargo.lock has checksum {in_lock}, the index {sha256}")
    with tempfile.TemporaryDirectory(prefix="blaze-vendor-sync-") as held:
        scratch = Path(held)
        base = await fetch(given.crate, given.version, sha256, scratch)
        work = scratch / "work"
        work.mkdir()
        tree, patches = await replay(given.crate, base, series, vendored / "patches", work, given.version)
        if hits := leftovers(given.crate, tree):
            raise Failed(
                f"{given.crate} {given.version}: the replayed tree still names tracing. Delete these "
                f"sites in the series (0001-no-tracing, or a new patch), and run again:\n" + "\n".join(hits)
            )
        if clash := sorted(entry.name for entry in tree.iterdir() if entry.name in OURS):
            raise Failed(
                f"{given.crate} {given.version}: upstream ships {', '.join(clash)}, which vendor/{given.crate} "
                f"keeps as blaze's own; add them to PATCHES.md's `Left out:` line, and run again"
            )
        stored = [(vendored / "patches" / f"{name}.patch").read_bytes() for name in series.patches]
        if given.check:
            problems = differences(vendored, tree)
            if series.version != given.version or series.sha256 != sha256:
                problems.append(f"  PATCHES.md's Base is {series.version} ({series.sha256}), not {given.version} ({sha256})")
            for name, old, new in zip(series.patches, stored, patches, strict=True):
                if old != new:
                    problems.append(f"  patches/{name}.patch is not the diff of its stages:\n{unified(old, new, name)}")
            if problems:
                raise Failed(f"{given.crate} {given.version}: vendor/{given.crate} is not upstream plus its series:\n" + "\n".join(problems))
            print(f"{given.crate}: {given.version} plus {len(series.patches)} patches is vendor/{given.crate}, byte for byte")
            return
        for entry in vendored.iterdir():
            if entry.name in OURS:
                continue
            if entry.is_dir() and not entry.is_symlink():
                shutil.rmtree(entry)
            else:
                entry.unlink()
        for entry in tree.iterdir():
            target = vendored / entry.name
            if entry.is_dir() and not entry.is_symlink():
                shutil.copytree(entry, target, symlinks=True)
            else:
                shutil.copy2(entry, target, follow_symlinks=False)
        for name, patch in zip(series.patches, patches, strict=True):
            (vendored / "patches" / f"{name}.patch").write_bytes(patch)
        write_base(vendored, given.crate, given.version, sha256)
    print(f"{given.crate}: {series.version} -> {given.version} (sha256 {sha256[:12]}), {len(series.patches)} patches replayed")


def main() -> None:
    match sys.argv[1:]:
        case [text]:
            given = parse(Input, text, SCRIPT)
        case _:
            fail(SCRIPT, "usage: sync.py JSON (see the top of the file)")
    try:
        asyncio.run(sync(given))
    except Failed as error:
        sys.exit(f"error: {SCRIPT}: {error}")


if __name__ == "__main__":
    main()
