"""Moves blaze's trait solver and format-string parser to a nightly: `vendor/rustc_type_ir`,
`vendor/rustc_next_trait_solver`, `vendor/rustc_type_ir_macros`, `vendor/rustc_transmute` and
`vendor/rustc_parse_format` become rust-lang/rust's `compiler/<crate>` at that nightly's rustc
commit, with blaze's patch series (`vendor/<crate>/PATCHES.md`) replayed on top
(`docs/v2/plan/design/compile-time.md`, change 18). `harness/toolchain_bump.py` runs it on every
bump.

    python3 tools/v2/solver_sync.py '{"nightly": "nightly-2026-10-27"}'

Input (inline JSON, a file's path, or `-`):
- `nightly`: the nightly to move to, `nightly-YYYY-MM-DD`. It must be the one `harness/toolchain`
  pins.
- `root`: the workspace; empty for the one holding this script.
- `checkout`: a rust-lang/rust git checkout already at the nightly's commit, with the five
  directories checked out; empty to fetch them (a sparse, blob-less clone cached under
  `$XDG_CACHE_HOME/blaze-solver-sync`).

In order, each step stopping the sync with an error that names it:
1. the nightly's rustc commit, from `static.rust-lang.org/dist/<date>/
   channel-rust-nightly-git-commit-hash.txt`;
2. `compiler/{rustc_type_ir,rustc_next_trait_solver,rustc_type_ir_macros,rustc_transmute,rustc_parse_format}`
   at that commit;
3. for each crate, upstream's `Cargo.toml` must be `vendor/<crate>/Cargo.toml.orig` byte for
   byte: blaze's `Cargo.toml` is written from it, so a changed one stops the sync until both are
   updated by hand;
4. for each crate, in a scratch tree: upstream's `src/`, then each patch of `PATCHES.md` in order
   with `git apply` (exact context, no fuzz). The first patch that does not apply stops the sync,
   naming the crate and the patch, and saying whether upstream already has it;
5. only when every crate replayed: each `vendor/<crate>/src` is replaced by its scratch tree, each
   patch is rewritten as the diff between its stages (so its context is the new base's), and
   `PATCHES.md`'s `Base:` line names the new commit and nightly.
"""

import difflib
import os
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from typed_input import fail, parse

SCRIPT = "solver_sync.py"
CRATES = ("rustc_type_ir", "rustc_next_trait_solver", "rustc_type_ir_macros", "rustc_transmute", "rustc_parse_format")
DIST = "https://static.rust-lang.org/dist"
RUST = "https://github.com/rust-lang/rust"
NIGHTLY = re.compile(r"nightly-(\d{4}-\d{2}-\d{2})")
COMMIT = re.compile(r"[0-9a-f]{40}")
BASE = re.compile(r"(?m)^Base: rust-lang/rust `([0-9a-f]{40})`, (nightly-\d{4}-\d{2}-\d{2})\.$")
HEADING = re.compile(r"(?m)^### (\d{4}-[a-z0-9][a-z0-9-]*)$")
# Git, as the sync runs it: no user diff drivers, colors or pagers.
GIT_ENV = {k: v for k, v in os.environ.items() if k not in ("GIT_EXTERNAL_DIFF", "GIT_DIR", "GIT_WORK_TREE")}


@dataclass(frozen=True)
class Input:
    nightly: str
    root: str = ""
    checkout: str = ""


class Failed(Exception):
    pass


@dataclass(frozen=True)
class Series:
    """A crate's `PATCHES.md`: the base it was made on and its patches' names, in order."""

    commit: str
    nightly: str
    patches: tuple[str, ...]


def read_series(crate: Path) -> Series:
    """`crate`'s series; its headings and `patches/` must name the same files."""
    text = (crate / "PATCHES.md").read_text()
    base = BASE.search(text)
    if base is None:
        raise Failed(f"{crate.name}: PATCHES.md has no line `Base: rust-lang/rust `<commit>`, nightly-YYYY-MM-DD.`")
    names = tuple(HEADING.findall(text))
    if len(set(names)) != len(names):
        raise Failed(f"{crate.name}: PATCHES.md names a patch twice")
    if names != tuple(sorted(names)):
        raise Failed(f"{crate.name}: PATCHES.md's patches are not in the order of their numbers")
    on_disk: set[str] = {p.stem for p in (crate / "patches").glob("*.patch")} if (crate / "patches").is_dir() else set()
    if unlisted := sorted(on_disk - set(names)):
        raise Failed(f"{crate.name}: patches/ has {', '.join(unlisted)}, which PATCHES.md doesn't list")
    if missing := sorted(set(names) - on_disk):
        raise Failed(f"{crate.name}: PATCHES.md lists {', '.join(missing)}, which patches/ doesn't have")
    return Series(base.group(1), base.group(2), names)


def write_base(crate: Path, commit: str, nightly: str) -> None:
    path = crate / "PATCHES.md"
    text = path.read_text()
    path.write_text(BASE.sub(f"Base: rust-lang/rust `{commit}`, {nightly}.", text, count=1))


def git(
    step: str,
    *argv: str,
    cwd: Path,
    bound: int = 600,
    ok: tuple[int, ...] = (0,),
    ceiling: Path | None = None,
) -> subprocess.CompletedProcess[bytes]:
    """`git argv` in `cwd`. With `ceiling`, git finds no repository at or above it: inside one,
    `git apply` would take the patch's paths from that repository's top, not from `cwd`."""
    env = GIT_ENV if ceiling is None else dict(GIT_ENV, GIT_CEILING_DIRECTORIES=str(ceiling.parent))
    try:
        done = subprocess.run(["git", *argv], cwd=cwd, env=env, capture_output=True, timeout=bound)
    except subprocess.TimeoutExpired:
        raise Failed(f"{step}: `git {' '.join(argv)}` took over {bound} s") from None
    except FileNotFoundError:
        raise Failed(f"{step}: `git` is not on PATH") from None
    if done.returncode not in ok:
        tail = "\n".join(done.stderr.decode(errors="replace").splitlines()[-25:])
        raise Failed(f"{step}: `git {' '.join(argv)}` exited {done.returncode}:\n{tail}")
    return done


def commit_of(nightly: str) -> str:
    """The rust-lang/rust commit `nightly`'s rustc was built from."""
    date = NIGHTLY.fullmatch(nightly)
    assert date is not None
    url = f"{DIST}/{date.group(1)}/channel-rust-nightly-git-commit-hash.txt"
    try:
        with urllib.request.urlopen(url, timeout=60) as answer:
            text = answer.read().decode().strip()
    except urllib.error.HTTPError as error:
        raise Failed(f"commit: {url} answered HTTP {error.code}") from None
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        raise Failed(f"commit: {url} could not be read: {error!r}") from None
    if not COMMIT.fullmatch(text):
        raise Failed(f"commit: {url} holds {text[:80]!r}, not a commit hash")
    return text


def fetch(commit: str) -> Path:
    """rust-lang/rust's crate directories at `commit`, in a cached sparse clone."""
    cache = Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache") / "blaze-solver-sync"
    repo = cache / "rust"
    if not (repo / ".git").is_dir():
        repo.mkdir(parents=True, exist_ok=True)
        git("fetch: clone", "init", "-q", str(repo), cwd=cache)
        git("fetch: clone", "remote", "add", "origin", RUST, cwd=repo)
    git("fetch: sparse", "sparse-checkout", "set", *(f"compiler/{c}" for c in CRATES), cwd=repo)
    git(f"fetch {commit[:9]}", "fetch", "-q", "--depth", "1", "--filter=blob:none", "origin", commit, cwd=repo, bound=1800)
    git(f"fetch: checkout {commit[:9]}", "checkout", "-q", "-f", commit, cwd=repo, bound=1800)
    return repo


def checked_out(repo: Path, commit: str) -> Path:
    """`repo`, after checking it is at `commit` and has every crate of `CRATES`."""
    head = git("checkout", "rev-parse", "HEAD", cwd=repo).stdout.decode().strip()
    if head != commit:
        raise Failed(f"checkout: {repo} is at {head[:9]}, not the nightly's {commit[:9]}")
    for crate in CRATES:
        if not (repo / "compiler" / crate / "src").is_dir():
            raise Failed(f"checkout: {repo} has no compiler/{crate}/src")
    return repo


def check_manifest(upstream: Path, vendored: Path, commit: str) -> None:
    """Upstream's manifest is the one blaze's `Cargo.toml` was written from, or the sync stops."""
    theirs = (upstream / "Cargo.toml").read_text()
    recorded = vendored / "Cargo.toml.orig"
    ours = recorded.read_text() if recorded.is_file() else ""
    if theirs == ours:
        return
    diff = "".join(
        difflib.unified_diff(
            ours.splitlines(keepends=True),
            theirs.splitlines(keepends=True),
            f"vendor/{vendored.name}/Cargo.toml.orig",
            f"compiler/{vendored.name}/Cargo.toml at {commit[:9]}",
        )
    )
    raise Failed(
        f"{vendored.name}: upstream's Cargo.toml changed at {commit[:9]}. Carry the change into "
        f"vendor/{vendored.name}/Cargo.toml (built with `nightly` off, as its header says), copy "
        f"upstream's to Cargo.toml.orig, and run again:\n{diff}"
    )


def apply(crate: str, name: str, patch: Path, tree: Path, commit: str) -> None:
    """`patch` applied to `tree`, or an error naming it (and whether upstream already has it)."""
    step = f"{crate}: {name}"
    check = git(step, "apply", "--check", str(patch), cwd=tree, ok=(0, 1, 128), ceiling=tree)
    if check.returncode == 0:
        git(step, "apply", str(patch), cwd=tree, ceiling=tree)
        return
    said = "\n".join(check.stderr.decode(errors="replace").splitlines()[-15:])
    reverse = git(step, "apply", "--check", "--reverse", str(patch), cwd=tree, ok=(0, 1, 128), ceiling=tree)
    if reverse.returncode == 0:
        raise Failed(
            f"{crate}: patch {name} no longer applies at {commit[:9]}: upstream already has it. Drop it "
            f"from vendor/{crate}/PATCHES.md and patches/, saying so under the dropped patches, and run again."
        )
    raise Failed(
        f"{crate}: patch {name} no longer applies at {commit[:9]}. Port it to upstream's new code "
        f"(vendor/{crate}/patches/{name}.patch, what and why in PATCHES.md), and run again. git apply said:\n{said}"
    )


def stage_diff(before: Path, after: Path) -> bytes:
    """The patch from `before/src` to `after/src`, with paths `a/src/...` and `b/src/...`."""
    work = before.parent
    if after.parent != work or before.name != "a" or after.name != "b":
        raise ValueError("stage_diff takes sibling directories named a and b")
    argv = ("-c", "core.quotepath=off", "diff", "--no-index", "--no-prefix", "--no-color", "--no-renames")
    return git("diff", *argv, "--no-ext-diff", "a/src", "b/src", cwd=work, ok=(0, 1), ceiling=work).stdout


def replay(crate: str, upstream: Path, series: Series, patches: Path, work: Path, commit: str) -> tuple[Path, list[bytes]]:
    """Upstream's `src` with the series applied, and each patch regenerated against its stage."""
    tree = work / crate / "tree"
    shutil.copytree(upstream / "src", tree / "src", symlinks=True)
    regenerated: list[bytes] = []
    for name in series.patches:
        stage = work / crate / "stage"
        shutil.rmtree(stage, ignore_errors=True)
        stage.mkdir(parents=True)
        shutil.copytree(tree / "src", stage / "a" / "src", symlinks=True)
        apply(crate, name, patches / f"{name}.patch", tree, commit)
        shutil.copytree(tree / "src", stage / "b" / "src", symlinks=True)
        regenerated.append(stage_diff(stage / "a", stage / "b"))
    return tree / "src", regenerated


def replace_tree(src: Path, dst: Path) -> None:
    shutil.rmtree(dst, ignore_errors=True)
    shutil.copytree(src, dst, symlinks=True)


def sync(given: Input) -> None:
    if not NIGHTLY.fullmatch(given.nightly):
        raise Failed(f"field nightly is nightly-YYYY-MM-DD, not {given.nightly!r}")
    root = Path(given.root) if given.root else Path(__file__).resolve().parents[2]
    pinned = (root / "harness" / "toolchain").read_text().strip()
    if pinned != given.nightly:
        raise Failed(f"harness/toolchain pins {pinned}, not {given.nightly}: the solver follows the pin")
    vendor = root / "vendor"
    series = {crate: read_series(vendor / crate) for crate in CRATES}
    commit = commit_of(given.nightly)
    repo = checked_out(Path(given.checkout), commit) if given.checkout else checked_out(fetch(commit), commit)
    for crate in CRATES:
        check_manifest(repo / "compiler" / crate, vendor / crate, commit)
    with tempfile.TemporaryDirectory(prefix="blaze-solver-sync-") as scratch:
        work = Path(scratch)
        results = {
            crate: replay(crate, repo / "compiler" / crate, series[crate], vendor / crate / "patches", work, commit)
            for crate in CRATES
        }
        for crate, (src, patches) in results.items():
            replace_tree(src, vendor / crate / "src")
            for name, patch in zip(series[crate].patches, patches, strict=True):
                (vendor / crate / "patches" / f"{name}.patch").write_bytes(patch)
            write_base(vendor / crate, commit, given.nightly)
    for crate in CRATES:
        was = series[crate]
        print(f"{crate}: {was.nightly} ({was.commit[:9]}) -> {given.nightly} ({commit[:9]}), {len(was.patches)} patches replayed")


def main() -> None:
    match sys.argv[1:]:
        case [text]:
            given = parse(Input, text, SCRIPT)
        case _:
            fail(SCRIPT, "usage: solver_sync.py JSON (see the top of the file)")
    try:
        sync(given)
    except Failed as error:
        sys.exit(f"error: {SCRIPT}: {error}")


if __name__ == "__main__":
    main()
