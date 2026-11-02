#!/usr/bin/env python3
"""Publish blaze (the crate `blaze-engine`) and the forked crates it depends on to crates.io.

  publish_crates.py --stage DIR          # write the crates to publish into DIR, nothing else
  publish_crates.py --dry-run            # stage, then `cargo package --workspace` (builds each)
  publish_crates.py --publish            # stage, verify as above, then publish what is missing

blaze's crate, `crates/blaze-engine` in the repository, is published as `blaze-engine`, so that
`cargo install blaze-engine` installs it and its programs (`blaze` among them). It depends by path on
the front end's crates, derived from rust-analyzer's (crates/hir-ty, hir-def and the rest), and
on rustc's rustc_abi (crates/rustc_abi), which crates.io does not have in this form. Each one in
blaze's dependency closure is published as `blaze-<name>` at the workspace version, without
touching the tree: every crate is copied into a staging workspace and its manifest is
rewritten there (name, version, metadata, path dependencies pinned with `=<version>` and
renamed with `package = ...`, so `use hir::...` still compiles). A crate of blaze's own whose
name does not start with `blaze` (hir-ty-codegen) is published as `blaze-<name>` the same way.
Dev-dependencies, tests and benches are left out. One file outside a crate is copied into it:
blaze's pinned toolchain (harness/toolchain).

Idempotent: a version already on crates.io is skipped, so a run that stopped can be run again.
Publishing goes in dependency order and waits out crates.io's rate limit on new crates (a burst,
then one per ten minutes): on a 429 it sleeps until the time the response names, then retries.
Needs CARGO_REGISTRY_TOKEN for --publish. Stdlib only.
"""

import argparse
import email.utils
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, NoReturn

import tomllib

ROOT = Path(__file__).resolve().parents[2]
ROOT_CRATE = ROOT / "crates" / "blaze-engine"
# The root crate's name on crates.io.
ROOT_NAME = "blaze-engine"
REPOSITORY = "https://github.com/indexable-inc/blaze"
USER_AGENT = "blaze-publish (https://github.com/indexable-inc/blaze)"
DEP_TABLES = ("dependencies", "build-dependencies")
UPSTREAM = "upstream"  # where crates from crates.io that are forked too are unpacked
SKIPPED_DIRS = {"tests", "benches", "examples", "test_data", "target"}


def fail(message: str) -> NoReturn:
    print(f"error: publish_crates: {message}", file=sys.stderr)
    sys.exit(1)


def log(message: str) -> None:
    print(f"publish_crates: {message}", file=sys.stderr, flush=True)


@dataclass(frozen=True)
class Extra:
    """A file or directory from outside a crate that it needs, and the line naming it."""

    src: str  # relative to ROOT
    dest: str  # relative to the staged crate
    patch: str  # the staged file that names it
    old: str
    new: str


# Forks whose name with the prefix another project has on crates.io: each is published as
# `<prefix>fork-<name>` instead.
TAKEN = {"macros"}

EXTRAS: dict[str, list[Extra]] = {
    "blaze-cargo": [
        Extra(
            "harness/toolchain",
            "toolchain",
            "src/cargo/toolchain.rs",
            'include_str!("../../../../harness/toolchain")',
            'include_str!("../../toolchain")',
        ),
    ],
    "blaze-engine": [Extra("README.md", "README.md", "", "", "")],
}


def tt_span_with_query(m: dict[str, Any]) -> None:
    # tt calls `SyntaxContext::is_root`, which span has only with its default `query` feature:
    # in the repository another crate turns it on, but a crate on crates.io is built alone.
    m["dependencies"]["span"].pop("default-features")


def macros_syn_full(m: dict[str, Any]) -> None:
    # The same for macros, which needs syn's `full` feature and does not ask for it.
    m["dependencies"]["syn"] = {
        "version": m["dependencies"]["syn"],
        "features": ["full"],
    }


# Manifest changes a crate needs to build on its own, by its name in the repository.
FIXES: dict[str, Any] = {"tt": tt_span_with_query, "macros": macros_syn_full}


@dataclass
class Crate:
    orig: str  # its name in the repository
    name: str  # its name on crates.io
    version: str
    dir: Path
    manifest: dict[str, Any]
    fork: bool
    # Local crates it depends on, by directory.
    deps: set[Path] = field(default_factory=set)
    # Optional ones: kept in the manifest when they are published anyway, so they have to be
    # on crates.io first (cargo publish resolves optional dependencies too).
    optional: set[Path] = field(default_factory=set)

    @property
    def requirement(self) -> str:
        # A fork is pinned exactly: blaze is only known to work with the one it was built with.
        return f"={self.version}" if self.fork else self.version


# ---------------------------------------------------------------------------------------------
# Reading the repository


def load(path: Path) -> dict[str, Any]:
    try:
        return tomllib.loads(path.read_text())
    except (OSError, tomllib.TOMLDecodeError) as e:
        fail(f"cannot read {path}: {e}")


def dep_tables(manifest: dict[str, Any], dev: bool = False) -> list[dict[str, Any]]:
    """The dependency tables of a manifest, the per-target ones included."""
    names = DEP_TABLES + (("dev-dependencies",) if dev else ())
    tables = [manifest[n] for n in names if n in manifest]
    for target in manifest.get("target", {}).values():
        tables += [target[n] for n in names if n in target]
    return tables


class Repo:
    def __init__(self) -> None:
        root = load(ROOT / "Cargo.toml")
        self.workspace: dict[str, Any] = root["workspace"]
        self.ws_deps: dict[str, Any] = self.workspace.get("dependencies", {})
        self.patches: dict[str, Any] = root.get("patch", {}).get("crates-io", {})
        self.version: str = self.workspace["package"]["version"]
        # Crates from crates.io that use a patched one (rustc_type_ir uses rustc_abi): built
        # against the fork in the repository, they have to be forked as well, or crates.io
        # would build them against upstream's and the two would not be the same types.
        self.upstream: dict[str, Path] = {}
        self.lock = {p["name"]: p for p in load(ROOT / "Cargo.lock")["package"]}

    def fetch_upstream(self, into: Path) -> None:
        deps = {
            n: [d.split()[0] for d in p.get("dependencies", [])]
            for n, p in self.lock.items()
        }
        tainted = set(self.patches)
        while grown := {
            n
            for n, ds in deps.items()
            if n not in tainted and any(d in tainted for d in ds)
        }:
            tainted |= grown
        for name in sorted(tainted):
            p = self.lock[name]
            if not p.get("source", "").startswith("registry+"):
                continue
            self.upstream[name] = fetch_crate(name, p["version"], p["checksum"], into)

    def local_dir(self, crate_dir: Path, key: str, spec: Any) -> Path | None:
        """The directory of the local crate a dependency entry names, or None."""
        if isinstance(spec, str):
            spec = {"version": spec}
        if spec.get("workspace"):
            spec = self.ws_deps[key]
            if isinstance(spec, str):
                spec = {"version": spec}
            base = ROOT
        else:
            base = crate_dir
        if "path" in spec:
            return (base / spec["path"]).resolve()
        name = spec.get("package", key)
        patched = self.patches.get(name)
        if patched and "path" in patched:
            return (ROOT / patched["path"]).resolve()
        return self.upstream.get(name)

    def closure(self) -> dict[Path, Crate]:
        crates: dict[Path, Crate] = {}
        todo = [ROOT_CRATE.resolve()]
        while todo:
            d = todo.pop()
            if d in crates:
                continue
            manifest = load(d / "Cargo.toml")
            package = manifest["package"]
            orig = package["name"]
            # Forks: vendor/, and the crates in crates/ derived from others (rust-analyzer's, syntax-tree,
            # rustc_abi), which are at version 0.0.0.
            fork = not d.is_relative_to(ROOT / "crates") or package["version"] == "0.0.0"
            if fork:
                name, version = "blaze-" + ("fork-" if orig in TAKEN else "") + orig, self.version
            else:
                if d == ROOT_CRATE.resolve():
                    name = ROOT_NAME
                else:
                    name = orig if orig.startswith("blaze") else "blaze-" + orig
                version = package["version"]
                if isinstance(version, dict):
                    version = self.version
            crate = Crate(orig, name, version, d, manifest, fork)
            for table in dep_tables(manifest):
                for key, spec in table.items():
                    # An optional local crate is left out with the features that name it:
                    # blaze enables none.
                    if isinstance(spec, dict) and spec.get("optional"):
                        if (dep := self.local_dir(d, key, spec)) is not None:
                            crate.optional.add(dep)
                        continue
                    if (dep := self.local_dir(d, key, spec)) is not None:
                        crate.deps.add(dep)
                        todo.append(dep)
            crates[d] = crate
        for crate in crates.values():
            crate.deps |= crate.optional & crates.keys()
        return crates


def fetch_crate(name: str, version: str, checksum: str, into: Path) -> Path:
    """Download and unpack a crate from crates.io, checked against Cargo.lock."""
    url = f"https://static.crates.io/crates/{name}/{name}-{version}.crate"
    status, body = http_get(url)
    if status != 200:
        fail(f"GET {url} answered {status}")
    if (got := hashlib.sha256(body).hexdigest()) != checksum:
        fail(f"{name}-{version}.crate has sha256 {got}, Cargo.lock says {checksum}")
    into.mkdir(parents=True, exist_ok=True)
    with tarfile.open(fileobj=io.BytesIO(body), mode="r:gz") as tar:
        tar.extractall(into, filter="data")
    return (into / f"{name}-{version}").resolve()


def topo_order(crates: dict[Path, Crate]) -> list[Crate]:
    done: set[Path] = set()
    order: list[Crate] = []
    while len(order) < len(crates):
        ready = sorted(
            (c for d, c in crates.items() if d not in done and c.deps <= done),
            key=lambda c: c.name,
        )
        if not ready:
            fail("the local crates depend on each other in a cycle")
        for c in ready:
            done.add(c.dir)
            order.append(c)
    return order


# ---------------------------------------------------------------------------------------------
# Writing TOML (the standard library only reads it)


def toml_key(key: str) -> str:
    return key if re.fullmatch(r"[A-Za-z0-9_-]+", key) else json.dumps(key)


def toml_value(value: Any) -> str:
    match value:
        case bool():
            return "true" if value else "false"
        case int() | float():
            return str(value)
        case str():
            return json.dumps(value, ensure_ascii=False)
        case list():
            return "[" + ", ".join(toml_value(v) for v in value) + "]"
        case dict():
            return (
                "{ "
                + ", ".join(
                    f"{toml_key(k)} = {toml_value(v)}" for k, v in value.items()
                )
                + " }"
            )
    raise TypeError(f"cannot write {value!r} as TOML")


def toml_dump(doc: dict[str, Any]) -> str:
    out: list[str] = []

    def table(path: list[str], d: dict[str, Any]) -> None:
        inline = bool(path) and path[-1] in DEP_TABLES + ("dev-dependencies",)
        subtables: list[tuple[str, dict[str, Any]]] = []
        arrays: list[tuple[str, list[dict[str, Any]]]] = []
        lines: list[str] = []
        for k, v in d.items():
            # `edition = { workspace = true }` rather than a table of its own.
            if path == ["package"] and isinstance(v, dict) and k != "metadata":
                lines.append(f"{toml_key(k)} = {toml_value(v)}")
            elif isinstance(v, dict) and not inline:
                subtables.append((k, v))
            elif isinstance(v, list) and v and all(isinstance(x, dict) for x in v):
                arrays.append((k, v))
            else:
                lines.append(f"{toml_key(k)} = {toml_value(v)}")
        if path and (lines or not subtables):
            out.append(f"\n[{'.'.join(map(toml_key, path))}]")
        out.extend(lines)
        for k, v in subtables:
            table(path + [k], v)
        for k, items in arrays:
            for item in items:
                out.append(f"\n[[{'.'.join(map(toml_key, path + [k]))}]]")
                out.extend(
                    f"{toml_key(ik)} = {toml_value(iv)}" for ik, iv in item.items()
                )

    table([], doc)
    return "\n".join(out).lstrip("\n") + "\n"


# ---------------------------------------------------------------------------------------------
# Staging


def tracked_files(d: Path) -> list[Path]:
    """The files git tracks under `d` (a crate in this repository, vendor/ included)."""
    if d.is_file():
        return [Path()]
    if d in REPO.upstream.values():
        skip = {"Cargo.toml.orig", ".cargo_vcs_info.json", "Cargo.lock"}
        return [
            f.relative_to(d) for f in d.rglob("*") if f.is_file() and f.name not in skip
        ]
    r = subprocess.run(
        ["git", "ls-files", "-z"], cwd=d, capture_output=True, text=True, check=False
    )
    if r.returncode != 0:
        fail(f"git ls-files in {d} exited {r.returncode}: {r.stderr.strip()[-500:]}")
    files = [Path(p) for p in r.stdout.split("\0") if p]
    if not files:
        fail(f"git tracks no files in {d}")
    return files


def copy_tree(src: Path, dest: Path, skip_dirs: set[str]) -> None:
    for rel in tracked_files(src):
        if rel.parts and rel.parts[0] in skip_dirs:
            continue
        target = dest / rel if rel.parts else dest
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src / rel, target)


def fork_readme(c: Crate) -> str:
    upstream = f"rust-analyzer's `{c.orig}` crate"
    if c.orig == "rustc_abi":
        upstream = "rustc's `rustc_abi` (as in the pinned nightly, harness/toolchain)"
    elif c.dir in REPO.upstream.values():
        upstream = (
            f"`{c.orig}` {REPO.lock[c.orig]['version']}, built on `blaze-rustc_abi`,"
        )
    return (
        f"# {c.name}\n\n"
        f"A fork of {upstream}, published only so that [blaze]({REPOSITORY}) can be installed "
        "with `cargo install blaze-engine`. It is not a supported API and follows blaze's version, "
        "not upstream's. For rust-analyzer's crates as a library, use the `ra_ap_*` crates.\n\n"
        "Licensed under MIT or Apache-2.0, as upstream.\n"
    )


def staged_manifest(c: Crate, by_dir: dict[Path, Crate], stage: Path) -> dict[str, Any]:
    m = json.loads(json.dumps(c.manifest))  # a deep copy
    package = m["package"]
    package["name"] = c.name
    package["version"] = c.version
    package.pop("publish", None)
    if c.fork:
        upstream = f"rust-analyzer's {c.orig} crate"
        if c.orig == "rustc_abi":
            upstream = "rustc's rustc_abi"
        elif c.dir in REPO.upstream.values():
            upstream = f"{c.orig} {REPO.lock[c.orig]['version']} on blaze-rustc_abi"
        package["description"] = f"Fork of {upstream}, used by blaze"
        package["repository"] = REPOSITORY
        package["authors"] = ["rust-analyzer team", "blaze contributors"]
        package["readme"] = "README.md"
        package["license"] = "MIT OR Apache-2.0"
        for key in ("homepage", "documentation", "keywords", "categories"):
            package.pop(key, None)
    if c.name != c.orig and c.orig != "blaze-engine":
        # Its crate name stays the original one, which is what the code that uses it says. The
        # root crate has no library, and its binaries are named in its manifest.
        lib = m.setdefault("lib", {})
        lib.setdefault("name", c.orig.replace("-", "_"))
    package["autotests"] = False
    package["autobenches"] = False
    package["autoexamples"] = False
    for k in ("dev-dependencies", "test", "bench", "example"):
        m.pop(k, None)
    for target in m.get("target", {}).values():
        target.pop("dev-dependencies", None)
    if fix := FIXES.get(c.orig):
        fix(m)
    pruned: set[str] = set()
    for table in dep_tables(m):
        for key, spec in list(table.items()):
            if (d := REPO.local_dir(c.dir, key, spec)) is None:
                continue
            if d not in by_dir:
                del table[key]
                pruned.add(key)
                continue
            if isinstance(spec, str):
                spec = {"version": spec}
            if spec.get("workspace"):
                continue  # the staged workspace's table names it
            dep = by_dir[d]
            new = {
                k: v for k, v in (spec.items() if isinstance(spec, dict) else ()) if k not in ("path", "version", "package")
            }
            new = {"path": f"../{dep.name}", "version": dep.requirement, **new}
            if dep.name != key:
                new["package"] = dep.name
            table[key] = new
    if pruned:
        features = m.get("features", {})
        for feature, enables in features.items():
            features[feature] = [
                e
                for e in enables
                if re.split(r"[/?]", e.removeprefix("dep:"))[0] not in pruned
            ]
    return m


def shipped_profiles() -> dict[str, Any]:
    """The profiles `cargo install` builds blaze with. Cargo reads profiles only from the root
    manifest, and an installed crate is its own root, so blaze's carries them: `--profile dist`
    is the build a release ships (docs/reports/RELEASE-BUILD.md), short of its PGO."""
    profiles = load(ROOT / "Cargo.toml")["profile"]
    return {name: profiles[name] for name in ("release", "dist")}


def staged_workspace(crates: list[Crate], by_dir: dict[Path, Crate]) -> dict[str, Any]:
    ws = REPO.workspace
    deps: dict[str, Any] = {}
    for key, spec in REPO.ws_deps.items():
        spec = {"version": spec} if isinstance(spec, str) else dict(spec)
        d = REPO.local_dir(ROOT, key, spec)
        if d is None:
            deps[key] = spec
            continue
        if d not in by_dir:
            continue  # a local crate outside blaze's closure: not published, not needed
        dep = by_dir[d]
        rest = {
            k: v for k, v in spec.items() if k not in ("path", "version", "package")
        }
        deps[key] = {"path": dep.name, "version": dep.requirement, **rest}
        if dep.name != key:
            deps[key]["package"] = dep.name
    package = {k: v for k, v in ws["package"].items() if k not in ("version",)}
    return {
        "workspace": {
            "members": [c.name for c in crates],
            "exclude": [UPSTREAM],
            "resolver": "2",
            "package": package,
            "dependencies": deps,
            "lints": ws.get("lints", {}),
        },
        "profile": shipped_profiles(),
    }


def stage(dest: Path) -> list[Crate]:
    if dest.exists() and any(dest.iterdir()):
        fail(f"{dest} is not empty")
    dest.mkdir(parents=True, exist_ok=True)
    REPO.fetch_upstream(dest / UPSTREAM)
    by_dir = REPO.closure()
    crates = topo_order(by_dir)
    for c in crates:
        out = dest / c.name
        copy_tree(c.dir, out, SKIPPED_DIRS)
        for lic in ("LICENSE-MIT", "LICENSE-APACHE"):
            if not (out / lic).exists():
                shutil.copy2(ROOT / lic, out / lic)
        if c.fork:
            (out / "README.md").write_text(fork_readme(c))
        for extra in EXTRAS.get(c.orig, []):
            src = ROOT / extra.src
            if src.is_dir():
                copy_tree(src, out / extra.dest, {"target"})
            else:
                shutil.copy2(src, out / extra.dest)
            if extra.patch:
                f = out / extra.patch
                text = f.read_text()
                if text.count(extra.old) != 1:
                    fail(
                        f"{c.orig}: expected {extra.old!r} once in {extra.patch}, found {text.count(extra.old)}"
                    )
                f.write_text(text.replace(extra.old, extra.new))
        m = staged_manifest(c, by_dir, dest)
        if c.orig == "blaze-engine":
            m["package"]["readme"] = "README.md"
            m["profile"] = shipped_profiles()
        (out / "Cargo.toml").write_text(toml_dump(m))
    (dest / "Cargo.toml").write_text(toml_dump(staged_workspace(crates, by_dir)))
    shutil.copy2(ROOT / "Cargo.lock", dest / "Cargo.lock")
    log(
        f"staged {len(crates)} crates in {dest}: "
        + " ".join(f"{c.name}@{c.version}" for c in crates)
    )
    return crates


# ---------------------------------------------------------------------------------------------
# crates.io


def http_get(url: str) -> tuple[int, bytes]:
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    for attempt in range(5):
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                return r.status, r.read()
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return 404, b""
            if e.code < 500 and e.code != 429:
                raise
        except (urllib.error.URLError, TimeoutError):
            pass
        time.sleep(2 + 4 * attempt)
    fail(f"GET {url} kept failing")


def published(name: str, version: str) -> bool:
    status, _ = http_get(f"https://crates.io/api/v1/crates/{name}/{version}")
    return status == 200


def index_path(name: str) -> str:
    n = name.lower()
    if len(n) <= 2:
        return f"{len(n)}/{n}"
    if len(n) == 3:
        return f"3/{n[0]}/{n}"
    return f"{n[:2]}/{n[2:4]}/{n}"


def wait_for_index(name: str, version: str, limit: float = 900) -> None:
    """Wait until the sparse index lists the version, so the next crate resolves it."""
    deadline = time.monotonic() + limit
    url = f"https://index.crates.io/{index_path(name)}"
    while time.monotonic() < deadline:
        status, body = http_get(f"{url}?t={int(time.time())}")
        if status == 200 and any(
            json.loads(line).get("vers") == version
            for line in body.decode().splitlines()
            if line
        ):
            return
        time.sleep(10)
    fail(f"{name}@{version} was published but is not in the index after {limit:.0f} s")


def retry_after(output: str) -> float | None:
    """Seconds to wait if crates.io refused for its rate limit, else None."""
    if "429" not in output and "too many" not in output.lower():
        return None
    if m := re.search(
        r"try again after ([A-Z][a-z]{2}, \d{1,2} [A-Z][a-z]{2} \d{4} [\d:]+ GMT)",
        output,
    ):
        when = email.utils.parsedate_to_datetime(m.group(1))
        return max(0.0, (when - datetime.now(UTC)).total_seconds()) + 15
    return 600.0


def run(cmd: list[str], cwd: Path, timeout: float) -> subprocess.CompletedProcess[str]:
    log(f"$ {' '.join(cmd)}")
    try:
        r = subprocess.run(
            cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout, check=False
        )
    except subprocess.TimeoutExpired:
        fail(f"`{' '.join(cmd)}` did not finish in {timeout:.0f} s")
    sys.stderr.write(r.stderr[-20000:])
    sys.stdout.write(r.stdout[-5000:])
    return r


def verify(dest: Path) -> None:
    r = run(["cargo", "package", "--workspace", "--allow-dirty"], dest, 4 * 3600)
    if r.returncode != 0:
        fail(
            f"`cargo package --workspace` in {dest} exited {r.returncode}; tail:\n{r.stderr[-3000:]}"
        )


def publish(crates: list[Crate], dest: Path, deadline: float) -> None:
    if not os.environ.get("CARGO_REGISTRY_TOKEN"):
        fail("CARGO_REGISTRY_TOKEN is not set")
    todo = [c for c in crates if not published(c.name, c.version)]
    log(
        f"{len(crates) - len(todo)} already on crates.io, {len(todo)} to publish: {' '.join(c.name for c in todo)}"
    )
    for c in todo:
        while True:
            cmd = ["cargo", "publish", "-p", c.name, "--no-verify", "--allow-dirty"]
            r = run(cmd, dest, 1800)
            if r.returncode == 0:
                break
            if "already exists" in r.stderr:
                log(f"{c.name}@{c.version} is already on crates.io")
                break
            wait = retry_after(r.stderr)
            if wait is None:
                fail(
                    f"`{' '.join(cmd)}` exited {r.returncode}; tail:\n{r.stderr[-3000:]}"
                )
            if time.monotonic() + wait > deadline:
                fail(
                    f"crates.io asks to wait {wait:.0f} s for {c.name}, past --max-hours"
                )
            log(f"rate limited on {c.name}: waiting {wait:.0f} s")
            time.sleep(wait)
        wait_for_index(c.name, c.version)
        log(f"published {c.name}@{c.version}")


REPO: Repo


def main() -> None:
    global REPO
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument(
        "--stage",
        type=Path,
        metavar="DIR",
        help="only write the staging workspace to DIR",
    )
    mode.add_argument(
        "--dry-run", action="store_true", help="stage and verify, publish nothing"
    )
    mode.add_argument(
        "--publish",
        action="store_true",
        help="stage, verify and publish what is missing",
    )
    parser.add_argument(
        "--dir", type=Path, help="staging directory (default: a temporary one)"
    )
    parser.add_argument(
        "--no-verify", action="store_true", help="skip building each crate first"
    )
    parser.add_argument(
        "--max-hours",
        type=float,
        default=8,
        help="give up waiting on rate limits after this",
    )
    args = parser.parse_args()
    deadline = time.monotonic() + args.max_hours * 3600

    REPO = Repo()
    if args.stage:
        stage(args.stage)
        return
    dest = args.dir or Path(tempfile.mkdtemp(prefix="blaze-publish-"))
    crates = stage(dest)
    if args.publish and all(published(c.name, c.version) for c in crates):
        log("every crate is on crates.io already")
        return
    if not args.no_verify:
        verify(dest)
    if args.publish:
        publish(crates, dest, deadline)
    log("done")


if __name__ == "__main__":
    main()
