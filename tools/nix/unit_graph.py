#!/usr/bin/env python3
"""Cargo unit graph -> per-unit Nix build description, one graph per nix system.

  tools/nix/unit_graph.py   # cargo --unit-graph per target triple + cargo metadata -> tools/nix/units/<project>.json

Two projects: the daemon (the workspace at the repo root, `-p blaze-engine`) and the Game of Life demo
(demo/life, its own workspace and lock). Each `tools/nix/units/<project>.json` is read by tools/nix/units.nix,
which picks `systems.<nix system>` and makes one derivation per unit, calling rustc directly
(tools/nix/unit_build.py). Platform-specific dependencies (num_threads -> libc
on apple, inotify on linux, ...) differ per target, so each nix system gets its own graph. Rerun whenever
Cargo.toml, Cargo.lock or a feature set changes; tools/nix/units.nix refuses to evaluate when units.json is
stale against Cargo.lock / Cargo.toml.

Units are keyed by `unit_key`, not by their position in cargo's list, and name their dependencies
by key, so that a unit added or removed leaves the others' lines as they were. The staleness check
reads `lock` and `manifest`, the parsed Cargo.lock and Cargo.toml the graph was made
from, which units.nix compares with `builtins.fromTOML` of the files themselves. Not a hash of each file:
two pull requests that change Cargo.lock in different places then change these in different places too,
and git merges them as it merges the lock (#353).

Python 3.13, stdlib only. Every step has its own timeout; failures name the step, command, exit code
and stderr tail.
"""

import argparse
import asyncio
import json
import os
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
# nix system -> rust target triple. Builds are native, so the triple is also the host of that system.
TARGETS = {
    "x86_64-linux": "x86_64-unknown-linux-gnu",
    "aarch64-linux": "aarch64-unknown-linux-gnu",
    "aarch64-darwin": "aarch64-apple-darwin",
    "x86_64-darwin": "x86_64-apple-darwin",
}
# project -> (directory with its Cargo.toml/Cargo.lock, extra cargo flags).
PROJECTS = {
    "daemon": (".", ["-p", "blaze-engine"]),
    "life": ("demo/life", []),
}
UNITS_DIR = ROOT / "tools" / "nix" / "units"


class StepError(Exception):
    pass


async def run_step(step: str, cmd: list[str], timeout: float, cwd: Path, env: dict | None = None) -> str:
    full_env = {**os.environ, **(env or {})}
    try:
        async with asyncio.timeout(timeout):
            proc = await asyncio.create_subprocess_exec(
                *cmd, cwd=cwd, env=full_env,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            )
            out, err = await proc.communicate()
    except TimeoutError:
        raise StepError(f"step '{step}' timed out after {timeout:.0f}s\n  command: {' '.join(cmd)}") from None
    if proc.returncode != 0:
        tail = "\n".join(err.decode(errors="replace").strip().splitlines()[-30:])
        raise StepError(f"step '{step}' failed\n  command: {' '.join(cmd)}\n  exit code: {proc.returncode}\n  stderr tail:\n{tail}")
    return out.decode()


def toolchain_prefix() -> list[str]:
    """Run cargo from the flake dev shell (the pinned nightly) unless we are already in it."""
    if os.environ.get("IN_NIX_SHELL"):
        return []
    return ["nix", "develop", str(ROOT), "--command"]


# `--unit-graph` is unstable; the pinned stable cargo takes it with RUSTC_BOOTSTRAP.
CARGO_ENV = {"RUSTC_BOOTSTRAP": "1"}


async def fetch_graphs(project_dir: Path, flags: list[str]) -> tuple[dict[str, dict], dict]:
    pre = toolchain_prefix()
    await run_step("cargo fetch", [*pre, "cargo", "fetch", "--locked"], 900, project_dir)
    async with asyncio.TaskGroup() as tg:
        graphs = {
            system: tg.create_task(run_step(
                f"cargo unit-graph {triple}",
                [*pre, "cargo", "build", "--unit-graph", "-Z", "unstable-options", "--release", "--locked", "--target", triple, *flags],
                300, project_dir, CARGO_ENV))
            for system, triple in TARGETS.items()
        }
        t_meta = tg.create_task(run_step(
            "cargo metadata", [*pre, "cargo", "metadata", "--format-version", "1", "--locked"], 300, project_dir))
    return {s: json.loads(t.result()) for s, t in graphs.items()}, json.loads(t_meta.result())


def key_of(pkg_id: str) -> str:
    # `<source>#name@version` (or `<source>#version` when the name is the path's last segment).
    src, _, frag = pkg_id.partition("#")
    if "@" in frag:
        return frag
    return f"{src.rstrip('/').rsplit('/', 1)[-1].split('?')[0]}@{frag}"


def build_packages(meta: dict, project_dir: Path) -> dict[str, dict]:
    lock = tomllib.loads((project_dir / "Cargo.lock").read_text())
    checksums = {(p["name"], p["version"]): p.get("checksum") for p in lock["package"]}
    packages: dict[str, dict] = {}
    for p in meta["packages"]:
        key = f"{p['name']}@{p['version']}"
        if key in packages:
            raise StepError(f"step 'nix': two packages named {key}; the key scheme needs the source too")
        mdir = Path(p["manifest_path"]).parent
        if p["source"] and p["source"].startswith("registry+"):
            cks = checksums[(p["name"], p["version"])]
            source = {"kind": "registry", "checksum": cks}
        elif p["source"] is None:
            source = {"kind": "path", "dir": str(mdir.relative_to(ROOT))}
        else:
            raise StepError(f"step 'nix': unsupported source {p['source']} for {key} (only registry and path)")
        packages[key] = {
            "name": p["name"], "version": p["version"], "source": source, "links": p.get("links"),
            "authors": ":".join(p["authors"]), "description": p["description"] or "",
            "homepage": p["homepage"] or "", "repository": p["repository"] or "",
            "license": p["license"] or "", "license_file": p["license_file"] or "",
            "rust_version": p["rust_version"] or "", "readme": p["readme"] or "",
            "manifest_dir": str(mdir),
        }

    return packages


def unit_key(pkg: str, u: dict) -> str:
    """A name for a unit that stays the same when units are added or removed around it, unlike its
    position in cargo's list: what it builds (package, target kind and name), how (mode) and for
    which side (`host` for build scripts and proc macros)."""
    t = u["target"]
    return " ".join([pkg, t["kind"][0], t["name"], u["mode"], *(["host"] if u["platform"] is None else [])])


def build_system(system: str, graph: dict, packages: dict[str, dict], triple: str) -> dict:
    keys = []
    for u in graph["units"]:
        pkg = key_of(u["pkg_id"])
        if pkg not in packages:
            raise StepError(f"step 'nix': unit package {u['pkg_id']} is not in cargo metadata")
        keys.append(unit_key(pkg, u))
    # A package built for normal dependencies and for build dependencies with different features
    # (resolver 2) has two build scripts, both on the host: only those keys name their features.
    named = [k for k in keys if keys.count(k) > 1]
    keys = [
        f"{k} features={','.join(sorted(u['features'])) or '-'}" if k in named else k
        for k, u in zip(keys, graph["units"])
    ]
    if len(set(keys)) != len(keys):
        twice = sorted({k for k in keys if keys.count(k) > 1})
        raise StepError(f"step 'nix {system}': two units are both named {twice}; unit_key needs to tell them apart")
    units = {}
    for key, u in zip(keys, graph["units"]):
        pkg = key_of(u["pkg_id"])
        t = u["target"]
        # platform None = host unit (build script / proc-macro); otherwise the target triple. Builds are
        # native, so both are the same machine in nix.
        if u["platform"] not in (None, triple):
            raise StepError(f"step 'nix {system}': unit platform {u['platform']} is neither host nor {triple}")
        if u["mode"] not in ("build", "run-custom-build"):
            raise StepError(f"step 'nix': unsupported unit mode {u['mode']}")
        pdir = Path(packages[pkg]["manifest_dir"])
        pr = u["profile"]
        units[key] = {
            "pkg": pkg,
            "target": t["name"],
            "kind": t["kind"][0],
            "crate_type": t["crate_types"][0],
            "mode": u["mode"],
            "edition": t["edition"],
            "src": str(Path(t["src_path"]).relative_to(pdir)),
            "features": sorted(u["features"]),
            "profile": {k: pr[k] for k in ("name", "opt_level", "debuginfo", "debug_assertions", "overflow_checks", "panic", "codegen_units")},
            "deps": sorted(
                ({"unit": keys[d["index"]], "name": d["extern_crate_name"]} for d in u["dependencies"]),
                key=lambda d: (d["unit"], d["name"]),
            ),
        }
    return {"units": units, "roots": [keys[i] for i in graph["roots"]]}


def write_units(project: str, project_dir: Path, graphs: dict[str, dict], meta: dict) -> None:
    packages = build_packages(meta, project_dir)
    systems = {system: build_system(system, g, packages, TARGETS[system]) for system, g in graphs.items()}
    for p in packages.values():
        del p["manifest_dir"]  # absolute, host specific
    out = {
        "version": 4,
        "dir": str(project_dir.relative_to(ROOT)),
        "lock": tomllib.loads((project_dir / "Cargo.lock").read_text()),
        "manifest": tomllib.loads((project_dir / "Cargo.toml").read_text()),
        "packages": packages,
        "systems": systems,
    }
    UNITS_DIR.mkdir(exist_ok=True)
    target = UNITS_DIR / f"{project}.json"
    target.write_text(json.dumps(out, indent=1, sort_keys=True) + "\n")
    counts = ", ".join(f"{s}: {len(v['units'])}" for s, v in systems.items())
    print(f"wrote {target.relative_to(ROOT)}: {len(packages)} packages; units per system: {counts}")


async def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("projects", nargs="*", help="daemon and/or life; default: all")
    args = ap.parse_args()
    if unknown := [p for p in args.projects if p not in PROJECTS]:
        ap.error(f"unknown project {unknown}")
    for name in args.projects or PROJECTS:
        rel, flags = PROJECTS[name]
        project_dir = (ROOT / rel).resolve()
        graphs, meta = await fetch_graphs(project_dir, flags)
        write_units(name, project_dir, graphs, meta)


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except* StepError as eg:
        for e in eg.exceptions:
            print(f"error: {e}", file=sys.stderr)
        sys.exit(1)
