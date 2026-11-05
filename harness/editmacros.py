#!/usr/bin/env python3
"""An edit on a fresh daemon builds the proc macros and build scripts its view applies first.

A fresh daemon (its own BLAZE_HOME, nothing built) opens a workspace whose library holds a
const a derive of a proc-macro crate of the same workspace writes (`DERIVED`), read only in a
function its build script's cfg turns on. `blaze check --replace app::DERIVED=app::SEVEN` is
an edit by meaning ([`Request::Edit`]): it resolves `app::DERIVED`, which only the derive's
output holds, and finds its one reference, which only the build script's cfg leaves in. It
must rewrite that reference and say it replaced the item: never fail to find `app::DERIVED`
(an edit that read the crate with its derive not expanded) nor call the reference cfg'd out
(one that read it without its build script's run). Nothing else is built first.

Run after harness/reachmacros.py's pattern; the same input, one JSON value (a path, `-` or
inline), all fields optional:
  {"blaze": "/path/to/blaze", "keep": false, "timeout_s": 900}
"""

import asyncio
import os
import re
import shutil
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from blaze_cli import base_env, write  # noqa: E402
from reachmacros import BUILD, Config, StepFailed  # noqa: E402

MACRO = """\
use proc_macro::{TokenStream, TokenTree};

/// `impl NAME { pub fn hello() -> u32 { 7 } }` and `pub const DERIVED: u32 = 7;` for the
/// struct NAME it derives on.
#[proc_macro_derive(Hello)]
pub fn hello(input: TokenStream) -> TokenStream {
    let mut words = input.into_iter().filter_map(|it| match it {
        TokenTree::Ident(it) => Some(it.to_string()),
        _ => None,
    });
    let name = words.by_ref().skip_while(|it| it != "struct").nth(1).expect("a struct's name");
    format!("impl {name} {{ pub fn hello() -> u32 {{ 7 }} }} pub const DERIVED: u32 = 7;").parse().unwrap()
}
"""

LIB = """\
#[derive(mac::Hello)]
pub struct Thing;

pub const SEVEN: u32 = 7;

#[cfg(from_build)]
pub fn answer() -> u32 {
    DERIVED + Thing::hello()
}
"""


def unexpected_cfg(step: str, stderr: str) -> None:
    """Fails `step` when its stderr warns of a cfg the build script declares
    (`cargo::rustc-check-cfg=cfg(from_build)`): the run's check-cfgs did not reach the lint."""
    if re.search(r"unexpected `?cfg`? condition name: `?from_build", stderr):
        raise StepFailed(f"{step}: `from_build` was called unexpected, though app's build script declares it:\n{stderr}")


async def run(config: Config) -> None:
    work = Path(tempfile.mkdtemp(prefix="edit-macros-", dir="/tmp" if os.name != "nt" else None))
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
        process = await asyncio.create_subprocess_exec(
            config.blaze,
            "check",
            "--replace",
            "app::DERIVED=app::SEVEN",
            cwd=at,
            env=env,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        try:
            async with asyncio.timeout(config.timeout_s):
                out, err = await process.communicate()
        except TimeoutError:
            process.kill()
            await process.wait()
            raise StepFailed(f"edit: `blaze check --replace` did not end in {config.timeout_s}s") from None
        stderr = err.decode(errors="replace")
        if process.returncode != 0:
            tail = "\n".join(stderr.splitlines()[-40:])
            raise StepFailed(f"edit: `blaze check --replace app::DERIVED=app::SEVEN` exited {process.returncode}:\n{tail}")
        if "can't be told" in stderr:
            raise StepFailed(f"edit: the edit refused to tell its answer:\n{stderr}")
        unexpected_cfg("edit", stderr)
        if "Replaced" not in stderr:
            raise StepFailed(f"edit: the replace said nothing replaced:\n{stderr}\n{out.decode(errors='replace')}")
        text = (at / "app/src/lib.rs").read_text()
        if "SEVEN + Thing::hello()" not in text or "DERIVED" in text:
            raise StepFailed(f"edit: the derive's const's reference was not rewritten; app/src/lib.rs is:\n{text}")
        print(f"  {stderr.strip().splitlines()[-1]}", flush=True)
        # The edited workspace still checks on the same daemon: the reference now names the
        # item the edit chose, and the proc macro the edit built is loaded again from what that
        # build kept.
        check = await asyncio.create_subprocess_exec(
            config.blaze, "check", cwd=at, env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
        )
        try:
            async with asyncio.timeout(config.timeout_s):
                _, err = await check.communicate()
        except TimeoutError:
            check.kill()
            await check.wait()
            raise StepFailed(f"check: `blaze check` did not end in {config.timeout_s}s") from None
        stderr = err.decode(errors="replace")
        if check.returncode != 0:
            tail = "\n".join(stderr.splitlines()[-40:])
            raise StepFailed(f"check: `blaze check` after the edit, on the same daemon, exited {check.returncode}:\n{tail}")
        unexpected_cfg("check", stderr)
        passed = True
    finally:
        stop = await asyncio.create_subprocess_exec(
            config.blaze, "stop", cwd=at, env=env, stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL
        )
        await stop.wait()
        if passed and not config.keep:
            shutil.rmtree(work)
    print("PASS: an edit on a fresh daemon builds its view's proc macros and build scripts first", flush=True)


def main() -> None:
    config = Config.parse(sys.argv[1] if len(sys.argv) > 1 else "{}")
    try:
        asyncio.run(run(config))
    except StepFailed as failed:
        print(f"FAIL: {failed}", file=sys.stderr, flush=True)
        raise SystemExit(1) from None


if __name__ == "__main__":
    main()
