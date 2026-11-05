#!/usr/bin/env python3
"""A reach on a fresh daemon builds the proc macros and build scripts its views apply first.

A fresh daemon (its own BLAZE_HOME, nothing built) opens a workspace whose library uses a
derive from a proc-macro crate of the same workspace and a cfg its build script sets. `blaze
reach` says the root, a function body is edited, and `blaze reach ROOT` must answer with the
edited function among the items it reaches: never the refusal that a crate's proc macros are
not built (`blaze_items::macros_built`), which a reach that read its views before building
their macros hit. Nothing else is built first: no check, no build.

Input, one JSON value (a path, `-` or inline), all fields optional:
  {"blaze": "/path/to/blaze", "keep": false, "timeout_s": 900}
`blaze` defaults to $BLAZE_BIN, then the release build; `keep` keeps the work directory.
"""

import asyncio
import dataclasses
import json
import os
import shutil
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from blaze_cli import BLAZE, base_env, write  # noqa: E402


@dataclasses.dataclass(frozen=True)
class Config:
    blaze: str = BLAZE
    keep: bool = False
    timeout_s: float = 900.0

    @staticmethod
    def parse(raw: str) -> "Config":
        if raw == "-":
            text = sys.stdin.read()
        elif raw.lstrip().startswith("{"):
            text = raw
        else:
            text = Path(raw).read_text()
        value = json.loads(text or "{}")
        if not isinstance(value, dict):
            raise SystemExit(f"reachmacros: the input is a JSON object, not {type(value).__name__}")
        known = {field.name for field in dataclasses.fields(Config)}
        unknown = sorted(set(value) - known)
        if unknown:
            raise SystemExit(f"reachmacros: unknown input fields {unknown}; known: {sorted(known)}")
        return Config(**value)


MACRO = """\
use proc_macro::{TokenStream, TokenTree};

/// `impl NAME { pub fn hello() -> u32 { 7 } }` for the struct NAME it derives on.
#[proc_macro_derive(Hello)]
pub fn hello(input: TokenStream) -> TokenStream {
    let mut words = input.into_iter().filter_map(|it| match it {
        TokenTree::Ident(it) => Some(it.to_string()),
        _ => None,
    });
    let name = words.by_ref().skip_while(|it| it != "struct").nth(1).expect("a struct's name");
    format!("impl {name} {{ pub fn hello() -> u32 {{ 7 }} }}").parse().unwrap()
}
"""

BUILD = """\
fn main() {
    println!("cargo::rustc-check-cfg=cfg(from_build)");
    println!("cargo::rustc-cfg=from_build");
}
"""

LIB = """\
#[derive(mac::Hello)]
pub struct Thing;

#[cfg(from_build)]
pub fn answer() -> u32 {
    Thing::hello() + ONE
}

pub const ONE: u32 = 1;

#[test]
fn answers() {
    assert!(answer() > 7);
}
"""


class StepFailed(Exception):
    pass


async def blaze(step: str, config: Config, cwd: Path, env: dict[str, str], *words: str) -> str:
    """Runs `blaze WORDS` in `cwd`, its stdout, or a failure naming the step and command."""
    argv = [config.blaze, *words]
    process = await asyncio.create_subprocess_exec(
        *argv, cwd=cwd, env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
    )
    try:
        async with asyncio.timeout(config.timeout_s):
            out, err = await process.communicate()
    except TimeoutError:
        process.kill()
        await process.wait()
        raise StepFailed(f"{step}: `{' '.join(argv)}` did not end in {config.timeout_s}s") from None
    stderr = err.decode(errors="replace")
    if process.returncode != 0:
        tail = "\n".join(stderr.splitlines()[-40:])
        raise StepFailed(f"{step}: `{' '.join(argv)}` exited {process.returncode}:\n{tail}")
    if "can't be told" in stderr:
        raise StepFailed(f"{step}: `{' '.join(argv)}` refused to tell what a change reaches:\n{stderr}")
    return out.decode()


def root_of(step: str, out: str) -> str:
    roots = [line.split()[1] for line in out.splitlines() if line.startswith("root ")]
    if len(roots) != 1 or len(roots[0]) != 64:
        raise StepFailed(f"{step}: no one `root <64 hex>` line in:\n{out}")
    return roots[0]


async def run(config: Config) -> None:
    # macOS: under /tmp, as a daemon's socket is under its home and macOS allows 104 bytes.
    # Elsewhere $TMPDIR: /tmp may be a tmpfs, which can't share blocks with blaze's cache.
    work = Path(tempfile.mkdtemp(prefix="reach-macros-", dir="/tmp" if sys.platform == "darwin" else None))
    print(f"work: {work}", flush=True)
    at, home = work / "workspace", work / "home"
    env = base_env(str(home))
    env["PATH"] = os.path.dirname(config.blaze) + os.pathsep + env["PATH"]
    write(at, "Cargo.toml", '[workspace]\nmembers = ["mac", "app"]\nresolver = "3"\n')
    write(at, "mac/Cargo.toml", '[package]\nname = "mac"\nversion = "0.1.0"\nedition = "2024"\n\n[lib]\nproc-macro = true\n')
    write(at, "mac/src/lib.rs", MACRO)
    write(
        at,
        "app/Cargo.toml",
        '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2024"\n\n[dependencies]\nmac = { path = "../mac" }\n',
    )
    write(at, "app/build.rs", BUILD)
    write(at, "app/src/lib.rs", LIB)
    passed = False
    try:
        before = root_of("open", await blaze("open", config, at, env, "reach"))
        write(at, "app/src/lib.rs", LIB.replace("Thing::hello() + ONE", "Thing::hello() + ONE + ONE"))
        out = await blaze("reach", config, at, env, "reach", before, "--json")
        answer = json.loads(out)
        reached = answer.get("reached") or {}
        items = [item["path"] for item in reached.get("items", [])]
        if not any(path.endswith("answer") for path in items):
            raise StepFailed(f"reach: the edited `answer` is not among the items reached: {items}\n{out}")
        if answer["root"] == before:
            raise StepFailed(f"reach: the root did not move with the edit: {before}")
        print(f"  reach from {before[:12]}: items {items}, compiled {reached.get('compiled')}", flush=True)
        passed = True
    finally:
        stop = await asyncio.create_subprocess_exec(
            config.blaze, "stop", cwd=at, env=env, stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL
        )
        await stop.wait()
        if passed and not config.keep:
            shutil.rmtree(work)
    print("PASS: a reach on a fresh daemon builds its views' proc macros and build scripts first", flush=True)


def main() -> None:
    config = Config.parse(sys.argv[1] if len(sys.argv) > 1 else "{}")
    try:
        asyncio.run(run(config))
    except StepFailed as failed:
        print(f"FAIL: {failed}", file=sys.stderr, flush=True)
        raise SystemExit(1) from None


if __name__ == "__main__":
    main()
