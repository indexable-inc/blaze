"""changed.py: the packages a change touches and the packages that depend on them, in the order
to check them: what `blaze check -p ...` takes to check a change in minutes, not the workspace
in an hour.

    uv run --frozen python harness/v2/changed.py '{"base": "origin/main"}'
    uv run --frozen python harness/v2/changed.py '{"base": "HEAD~3", "print": "args", "dependents": false}'
    uv run --frozen python harness/v2/changed.py selftest

The rule it serves: code is checked as it is written, a commit at a time, on the packages it
touched (docs/contributing.md, "Checking a change fast"); thousands of lines nobody compiled are
found out by an hour-long build, one error at a time (2026-11-10: a shadowed binding, then a
struct named like its crate).

The input is one JSON object:

    base         the ref the change is measured from: every file that differs between its merge
                 base with HEAD and the working tree (committed or not, untracked too) is the change
    direct       measure from `base` itself, not from its merge base with HEAD: for a shallow
                 checkout, which has no merge base to find (false)
    root         the checkout (".")
    dependents   also the workspace packages that depend on a changed one, through any number of
                 normal, build or dev dependencies (true)
    print        `json` (the default): one object, `{"changed", "dependents", "order", "all",
                 "why_all", "args", "elsewhere"}`; `packages`: `order`, a name a line; `args`:
                 `-p <name>` for each of `order` on one line, or `--workspace` when `all`

`changed` is the packages a changed file is in (the member directory that holds it), `dependents`
those that depend on one, and `order` both with every package after the packages it depends on
(normal and build dependencies; a dev-dependency orders nothing, as cargo lets those cycle), so
the first error is the earliest cause. A file that every package's build reads (`EVERYTHING`: the
root manifest, the lock file, the lints' and the formatter's configuration, the pinned toolchain)
makes it all of them: `all` is true and `why_all` names the file. `elsewhere` counts the changed
files in no package (docs, harness scripts, workflows).

A `print` of `args` that would name no package prints nothing and exits 0: there is nothing to
check. A base that is no ref, or a checkout that is no workspace, stops naming it.
"""

import glob
import json
import os
import subprocess
import sys
import tempfile
import tomllib
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal, cast

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse

SCRIPT = "changed.py"
# What every package's check reads besides its own files.
EVERYTHING = ("Cargo.toml", "Cargo.lock", ".clippy.toml", ".rustfmt.toml", "harness/toolchain", ".cargo/")
TABLES = ("dependencies", "build-dependencies", "dev-dependencies")


@dataclass(frozen=True)
class Input:
    base: str
    direct: bool = False
    root: str = "."
    dependents: bool = True
    print: Literal["json", "packages", "args"] = "json"


@dataclass(frozen=True)
class Package:
    """A workspace member: its name, its directory from the root (`/` between parts), and the
    members it depends on, the dev-dependencies apart."""

    name: str
    dir: str
    needs: frozenset[str]
    dev_needs: frozenset[str]


def tables(manifest: Mapping[str, Any]) -> list[tuple[str, Mapping[str, Any]]]:
    """Every dependency table of a manifest with its kind: its own and each `[target.*]`'s."""
    out: list[tuple[str, Mapping[str, Any]]] = []
    targets = cast(dict[str, dict[str, Any]], manifest.get("target", {}))
    for holder in (manifest, *targets.values()):
        for kind in TABLES:
            table = holder.get(kind)
            if isinstance(table, dict):
                out.append((kind, cast(dict[str, Any], table)))
    return out


def workspace(root: Path) -> list[Package]:
    """The workspace's members (the root manifest's `members` less its `exclude`), each with
    the members it depends on: by a `path`, or by `workspace = true` through the root's
    `[workspace.dependencies]` entry of that name."""
    top = tomllib.loads((root / "Cargo.toml").read_text())
    ws = cast(dict[str, Any], top.get("workspace") or {})
    if not ws:
        raise ValueError(f"{root / 'Cargo.toml'} has no [workspace]")
    excluded = {os.path.normpath(it) for it in ws.get("exclude", [])}
    found: dict[str, tuple[str, dict[str, Any]]] = {}
    for pattern in ws.get("members", []):
        for path in sorted(glob.glob(str(root / pattern))):
            rel = os.path.normpath(os.path.relpath(path, root))
            manifest = Path(path) / "Cargo.toml"
            if rel in excluded or not manifest.is_file():
                continue
            toml = tomllib.loads(manifest.read_text())
            name = toml.get("package", {}).get("name")
            if isinstance(name, str):
                found[rel.replace(os.sep, "/")] = (name, toml)
    shared = cast(dict[str, Any], ws.get("dependencies", {}))

    def member(directory: str, key: str, spec: object) -> str | None:
        """The member the dependency `key = spec` of the member in `directory` is, if it is one."""
        if isinstance(spec, dict):
            spec = cast(dict[str, Any], spec)
            if spec.get("workspace") is True:
                return member("", key, shared.get(key))
            if isinstance(spec.get("path"), str):
                rel = os.path.normpath(os.path.join(directory, spec["path"])).replace(os.sep, "/")
                return found[rel][0] if rel in found else None
        return None

    out: list[Package] = []
    for directory, (name, toml) in found.items():
        needs: set[str] = set()
        dev: set[str] = set()
        for kind, table in tables(toml):
            for key, spec in table.items():
                if (it := member(directory, key, spec)) is not None and it != name:
                    (dev if kind == "dev-dependencies" else needs).add(it)
        out.append(Package(name, directory, frozenset(needs), frozenset(dev - needs)))
    return sorted(out, key=lambda it: it.name)


def owner(packages: Sequence[Package], file: str) -> str | None:
    """The package whose directory holds `file` (the deepest, where one is inside another)."""
    best: Package | None = None
    for it in packages:
        if file.startswith(it.dir + "/") and (best is None or len(it.dir) > len(best.dir)):
            best = it
    return None if best is None else best.name


def everything(file: str) -> bool:
    return any(file.startswith(it) if it.endswith("/") else file == it for it in EVERYTHING)


def ordered(packages: Sequence[Package], chosen: set[str]) -> list[str]:
    """`chosen` with each package after those of `chosen` it needs (dev-dependencies order
    nothing); among packages free to go, by name. A cycle of normal dependencies is a ValueError."""
    by_name = {it.name: it for it in packages}
    left = {name: set(by_name[name].needs & chosen) for name in chosen}
    out: list[str] = []
    while left:
        free = sorted(name for name, needs in left.items() if not needs)
        if not free:
            raise ValueError(f"the packages {', '.join(sorted(left))} depend on each other in a cycle")
        out += free
        for name in free:
            del left[name]
        for needs in left.values():
            needs.difference_update(free)
    return out


@dataclass(frozen=True)
class Found:
    changed: list[str]
    dependents: list[str]
    order: list[str]
    all: bool
    why_all: str
    elsewhere: int

    @property
    def args(self) -> list[str]:
        return ["--workspace"] if self.all else [word for name in self.order for word in ("-p", name)]


def affected(packages: Sequence[Package], files: Sequence[str], dependents: bool = True) -> Found:
    """What the change of `files` (paths from the root, `/` between parts) asks to be checked."""
    names = {it.name for it in packages}
    whole = next((file for file in files if everything(file)), "")
    owners = [owner(packages, file) for file in files]
    changed = names if whole else {it for it in owners if it is not None}
    reached = set(changed)
    if dependents:
        grew = True
        while grew:
            more = {it.name for it in packages if it.name not in reached and (it.needs | it.dev_needs) & reached}
            grew = bool(more)
            reached |= more
    return Found(sorted(changed), sorted(reached - changed), ordered(packages, reached), bool(whole), whole,
                 sum(it is None and not everything(file) for it, file in zip(owners, files)))


def git(root: Path, *args: str) -> str:
    ran = subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)
    if ran.returncode != 0:
        raise ValueError(f"`git {' '.join(args)}` in {root} exited {ran.returncode}: {ran.stderr.strip()}")
    return ran.stdout


def changed_files(root: Path, base: str, direct: bool = False) -> list[str]:
    """Every file that differs between the merge base of `base` and HEAD (with `direct`, `base`
    itself) and the working tree, the untracked ones too."""
    fork = git(root, "rev-parse", "--verify", f"{base}^{{commit}}").strip() if direct else git(
        root, "merge-base", base, "HEAD").strip()
    differing = git(root, "diff", "--name-only", "-z", fork).split("\0")
    untracked = git(root, "ls-files", "--others", "--exclude-standard", "-z").split("\0")
    return sorted({it for it in (*differing, *untracked) if it})


def main(args: Input) -> None:
    root = Path(args.root).resolve()
    try:
        found = affected(workspace(root), changed_files(root, args.base, args.direct), args.dependents)
    except (ValueError, OSError, tomllib.TOMLDecodeError) as e:
        fail(SCRIPT, str(e))
    if args.print == "packages":
        for name in found.order:
            print(name)
    elif args.print == "args":
        if found.args:
            print(" ".join(found.args))
    else:
        print(json.dumps(dict(changed=found.changed, dependents=found.dependents, order=found.order, all=found.all,
                              why_all=found.why_all, args=found.args, elsewhere=found.elsewhere), indent=1))


# The self-test: a workspace written to a temporary directory; no git, no blaze.

def expect(holds: bool, what: str) -> None:
    if not holds:
        sys.exit(f"{SCRIPT} selftest: {what}")


def selftest() -> None:
    manifests = {
        "Cargo.toml": '[workspace]\nmembers = ["crates/*", "tools/lint/cli"]\nexclude = ["crates/app-shell"]\n'
                      '[workspace.dependencies]\nids = { path = "crates/ids" }\nhash = { path = "crates/hash" }\n'
                      'serde = "1"\nlinker = { path = "crates/link", package = "link" }\n',
        "crates/hash/Cargo.toml": '[package]\nname = "hash"\n[dev-dependencies]\ncargo-front = { path = "../cargo" }\n',
        "crates/ids/Cargo.toml": '[package]\nname = "ids"\n[dependencies]\nhash.workspace = true\nserde.workspace = true\n',
        "crates/link/Cargo.toml": '[package]\nname = "link"\n[dependencies]\nids = { workspace = true }\n',
        "crates/codegen/Cargo.toml": '[package]\nname = "codegen"\n[build-dependencies]\nids.workspace = true\n',
        "crates/cargo/Cargo.toml": '[package]\nname = "cargo-front"\n[dependencies]\nlinker.workspace = true\n'
                                   "[target.'cfg(unix)'.dependencies]\ncodegen = { path = \"../codegen\" }\n",
        "crates/lone/Cargo.toml": '[package]\nname = "lone"\n',
        "crates/app-shell/Cargo.toml": '[package]\nname = "app-shell"\n[dependencies]\nids = { path = "../ids" }\n',
        "tools/lint/cli/Cargo.toml": '[package]\nname = "lint-cli"\n[dependencies]\nhash = { path = "../../../crates/hash" }\n',
    }
    with tempfile.TemporaryDirectory(prefix="bz-changed-") as tmp:
        root = Path(tmp)
        for relative, text in manifests.items():
            (root / relative).parent.mkdir(parents=True, exist_ok=True)
            (root / relative).write_text(text)
        packages = workspace(root)
    by_name = {it.name: it for it in packages}
    expect(sorted(by_name) == ["cargo-front", "codegen", "hash", "ids", "link", "lint-cli", "lone"],
           f"the members, the excluded one left out: {sorted(by_name)}")
    needs = {name: set(it.needs) for name, it in by_name.items()}
    expect(needs["ids"] == {"hash"} and needs["link"] == {"ids"} and needs["codegen"] == {"ids"},
           "a `workspace = true` dependency is the member the root names, and a registry crate is none")
    expect(needs["cargo-front"] == {"link", "codegen"}, "a renamed dependency and a target's are followed")
    expect(set(by_name["hash"].dev_needs) == {"cargo-front"} and not needs["hash"], "a dev-dependency is kept apart")
    expect(needs["lint-cli"] == {"hash"}, "a path out of the member's directory is followed")

    # The finding in miniature: a type moved into `ids` (a struct named like its crate) and its
    # two users edited; all three and everything over them are checked, `ids` before its users
    # (`hash`, which `ids` needs, depends on a changed package only to test itself, and goes first).
    found = affected(packages, ["crates/ids/src/keyed.rs", "crates/link/src/lib.rs", "crates/codegen/src/delta.rs",
                                "docs/lints.md"])
    expect(found.changed == ["codegen", "ids", "link"] and found.dependents == ["cargo-front", "hash", "lint-cli"],
           f"the changed packages and what depends on them, through a dev-dependency too: {found}")
    expect(found.order == ["hash", "ids", "lint-cli", "codegen", "link", "cargo-front"],
           f"each after what it needs; a dev-dependency's cycle orders nothing: {found.order}")
    expect(found.args[:4] == ["-p", "hash", "-p", "ids"] and len(found.args) == 12
           and not found.all and found.elsewhere == 1, f"the arguments, and the doc counted elsewhere: {found}")
    alone = affected(packages, ["crates/ids/src/lib.rs"], dependents=False)
    expect(alone.order == ["ids"] and alone.dependents == [], "without dependents, the changed alone")
    leaf = affected(packages, ["crates/lone/src/lib.rs", "crates/lone/tests/it.rs"])
    expect(leaf.order == ["lone"] and leaf.changed == ["lone"], "a package nothing depends on is checked alone")
    expect(affected(packages, ["crates/app-shell/src/main.rs", "README.md"]).order == []
           and affected(packages, ["crates/app-shell/src/main.rs", "README.md"]).elsewhere == 2,
           "a file of an excluded directory or of none asks for no check")
    expect(affected(packages, ["tools/lint/cli/src/main.rs"]).order == ["lint-cli"], "a member outside crates/")
    for file in ("Cargo.lock", ".clippy.toml", "harness/toolchain", ".cargo/config.toml"):
        whole = affected(packages, [file, "crates/lone/src/lib.rs"])
        expect(whole.all and whole.why_all == file and whole.args == ["--workspace"] and len(whole.order) == len(packages),
               f"{file} is read by every package's check: {whole}")
    expect(not affected(packages, ["crates/ids/Cargo.toml"]).all, "a member's own manifest is its own")
    cycle = [Package("a", "a", frozenset({"b"}), frozenset()), Package("b", "b", frozenset({"a"}), frozenset())]
    try:
        affected(cycle, ["a/src/lib.rs"])
        expect(False, "a cycle of normal dependencies is refused")
    except ValueError as e:
        expect("a, b" in str(e), f"the cycle is named: {e}")
    try:
        parse(Input, '{"base": "main", "since": "x"}', SCRIPT)
        expect(False, "an unknown field is refused")
    except SystemExit:
        pass
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
