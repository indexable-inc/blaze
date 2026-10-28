#!/usr/bin/env python3
"""Own-path regressions adapted from Astral #35, #37, #51, #52, #83 and #87.

uv run harness/own_inputs.py '{"blaze":"/path/to/blaze"}'
Only blaze compiles the fixtures. Generated macro inputs change with the same
description and store, and lazy callbacks must preserve identity and current code.
An attribute macro also exercises anonymous-constant statics and an inlined
method's local TypeId, the own-path analogue of Astral #73's symbol recovery.
Crate-level cfg and cfg_attr must each leave an empty libtest harness (#1323).
"""

import asyncio
import os
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from typed_input import parse


@dataclass(frozen=True)
class Input:
    blaze: str
    mode: Literal["wasm", "native"] = "wasm"
    timeout_seconds: int = 900


def write(root: Path, files: dict[str, str]) -> None:
    for relative, contents in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)


async def command(inp: Input, root: Path, step: str, *args: str) -> str:
    argv = [inp.blaze, *args]
    env = {
        **os.environ,
        "RUST_BACKTRACE": "0",
        "CARGO_TARGET_DIR": str(root / "target"),
        "BLAZE_CACHE": str(root / "store"),
        "BLAZE_PROC_MACRO_WASM": "1" if inp.mode == "wasm" else "0",
        # Empty test roots must execute libtest rather than be considered cached.
        "BLAZE_TESTS": "all",
    }
    process = await asyncio.create_subprocess_exec(
        *argv, cwd=root, env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
    )
    try:
        async with asyncio.timeout(inp.timeout_seconds):
            stdout, stderr = await process.communicate()
    except TimeoutError:
        process.kill()
        _, stderr = await process.communicate()
        raise RuntimeError(
            f"{step}: {argv!r}, exit {process.returncode}, timed out; {stderr.decode()[-3000:]}"
        ) from None
    if process.returncode:
        raise RuntimeError(
            f"{step}: {argv!r}, exit {process.returncode}; {stderr.decode()[-3000:]}"
        )
    return stdout.decode().strip()


async def expect(inp: Input, root: Path, step: str, wanted: str) -> None:
    actual = await command(inp, root, step, "run", "-q")
    if actual != wanted:
        raise RuntimeError(f"{step}: blaze run exited 0 but printed {actual!r}, expected {wanted!r}")
    print(f"right: {step}: {actual}", flush=True)


async def generated(inp: Input, root: Path) -> None:
    write(root, {
        "Cargo.toml": '[workspace]\nmembers=["app","mac"]\nresolver="2"\n',
        "app/Cargo.toml": '[package]\nname="app"\nversion="0.1.0"\nedition="2021"\n'
        '[dependencies]\nmac={path="../mac"}\n',
        "app/src/main.rs": '''use std::any::TypeId;
#[mac::declare] struct Local;
fn id<T: 'static>(_: T) -> TypeId { TypeId::of::<T>() }
fn main() {
    Local::register(30);
    println!("{} {} {}", mac::value!(), Local::offset(), id(Local::key()) == id(Local::key()));
}
''',
        "mac/Cargo.toml": '[package]\nname="mac"\nversion="0.1.0"\nedition="2021"\n'
        '[lib]\nproc-macro=true\n',
        "mac/build.rs": '''fn main() {
    println!("cargo:rerun-if-changed=value.txt");
    let value = std::fs::read_to_string("value.txt").unwrap();
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::write(out.join("version.expr"), value).unwrap();
}
''',
        "mac/src/lib.rs": '''use proc_macro::TokenStream;
const VALUE: u32 = include!(concat!(env!("OUT_DIR"), "/version.expr"));
#[proc_macro]
pub fn value(_: TokenStream) -> TokenStream { format!("{}u32", VALUE).parse().unwrap() }
#[proc_macro_attribute]
pub fn declare(_: TokenStream, _: TokenStream) -> TokenStream {
    "struct Local; const _: () = {
        static OFFSET: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        impl Local {
            fn register(value: usize) { OFFSET.store(value, std::sync::atomic::Ordering::Relaxed); }
            fn offset() -> usize { OFFSET.load(std::sync::atomic::Ordering::Relaxed) }
            #[inline(always)] fn key() -> impl Copy {
                #[derive(Copy, Clone)] struct Key;
                const _: () = {
                    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                    impl Key { fn note() { CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed); } }
                };
                Key::note(); Key
            }
        }
    };".parse().unwrap()
}
''',
    })
    try:
        for value in [7, 9, 3, 7]:
            write(root, {"mac/value.txt": str(value)})
            if value == 3:
                await command(inp, root, "macro restart", "stop")
            await expect(inp, root, f"generated macro {value}", f"{value} 30 true")
    finally:
        await command(inp, root, "stop macro fixture", "stop")


async def callbacks(inp: Input, root: Path) -> None:
    write(root, {
        "Cargo.toml": '[package]\nname="callbacks"\nversion="0.1.0"\nedition="2021"\n',
        "src/main.rs": '''mod label;
use std::sync::LazyLock;
static RULE: &str = "assignment";
static REGISTRY: LazyLock<&'static &'static str> = LazyLock::new(|| &RULE);
static VALUE: LazyLock<usize> = LazyLock::new(helper);
#[inline(always)] fn helper() -> usize { 10 }
fn main() {
    println!("{} {} {} {}", std::ptr::eq(*REGISTRY, &RULE), *VALUE, helper(), label::value());
}
''',
        "src/label.rs": "pub fn value() -> usize { 1 }\n",
    })
    try:
        await expect(inp, root, "LazyLock cold identity", "true 10 10 1")
        write(root, {"src/label.rs": "pub fn value() -> usize { 2 }\n"})
        await expect(inp, root, "unrelated same-crate body edit", "true 10 10 2")
        main = root / "src/main.rs"
        main.write_text(main.read_text().replace("{ 10 }", "{ 11 }"))
        await expect(inp, root, "callback helper edit", "true 11 11 2")
        await command(inp, root, "callback restart", "stop")
        await expect(inp, root, "LazyLock identity after restart", "true 11 11 2")
    finally:
        await command(inp, root, "stop callback fixture", "stop")


async def cfg_roots(inp: Input, root: Path) -> None:
    # These roots need the candidate's fix, so they belong here rather than in
    # release smoke, which exercises the compiler published before this change.
    write(root, {
        "Cargo.toml": '[package]\nname="cfg-roots"\nversion="0.1.0"\nedition="2024"\n'
        '[workspace]\n',
        "src/main.rs": "fn main() {}\n",
        "tests/cfg_disabled.rs": '''//! A disabled crate still gets an empty libtest harness (#1323, Astral #35).
#![cfg(any())]

#[test]
fn disabled_test() {
    panic!("a test from a cfg-disabled crate ran");
}
''',
        "tests/cfg_attr_disabled.rs": '''//! A second disabled target joins the same worker, with its cfg supplied by cfg_attr.
#![cfg_attr(all(), cfg(any()))]

#[test]
fn disabled_test() {
    panic!("a test from a cfg_attr-disabled crate ran");
}
''',
    })
    try:
        for target in ("cfg_disabled", "cfg_attr_disabled"):
            step = f"empty cfg-root harness {target}"
            actual = await command(inp, root, step, "test", "--test", target)
            lines = actual.splitlines()
            summary = (
                "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;"
            )
            if lines.count("running 0 tests") != 1 or sum(
                line.startswith(summary) for line in lines
            ) != 1:
                raise RuntimeError(
                    f"{step}: blaze test --test {target} exited 0 but printed {actual!r}, "
                    "expected one successful empty harness"
                )
            print(f"right: {step}: 0 tests", flush=True)
    finally:
        await command(inp, root, "stop cfg-root fixture", "stop")


async def main(inp: Input) -> None:
    if inp.timeout_seconds <= 0:
        raise ValueError("timeout_seconds must be positive")
    with tempfile.TemporaryDirectory(prefix="blaze-own-inputs-") as work:
        async with asyncio.TaskGroup() as tasks:
            tasks.create_task(generated(inp, Path(work) / "macro"))
            tasks.create_task(callbacks(inp, Path(work) / "callbacks"))
            tasks.create_task(cfg_roots(inp, Path(work) / "cfg-roots"))


if __name__ == "__main__":
    asyncio.run(main(parse(Input, sys.argv[1], "own_inputs.py")))
