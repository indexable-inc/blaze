#!/usr/bin/env python3
"""A proc macro that calls C a library's build script compiled, expanded as cargo's (zed's).

zed's `include_flate_codegen` links zstd-sys, whose build script compiles zstd with the C
compiler of `TARGET`. blaze ran that script for the host alone, so the macro's module, built
for wasm32-wasip1, was linked with the host's `libzstd.a` ("neither Wasm object file nor LLVM
bitcode") and failed (promote run 40895100165). For a module, the build script of each library
it links runs for wasm32-wasip1, as cargo runs it for a library built for that target
(compile_time.rs).

The macro here hashes its input with a C function of `csum`, whose build script compiles it
with the compiler and flags of `TARGET` (`CC_<target>`, `CFLAGS_<target>`, else `CC` or `cc`, as
cc-rs looks) and archives it (`AR_<target>`, `AR` or `ar`). The C includes libc's headers, as
zstd's does. The program prints the hash; blaze's has to print cargo's, with the macro expanded
natively and as WebAssembly. The harness gives no C toolchain for wasm32-wasip1: blaze's own
(wasi-sdk, docs/c-toolchain.md) compiles it, where the runner's clang found glibc's
`/usr/include` on Linux and has no wasm32 backend on macOS (promote run 40968467065).
"""
import os, shutil, subprocess, tempfile

from blaze_cli import base_env
from checktargets import write

C = r'''#include <limits.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

uint32_t csum(const unsigned char *bytes, size_t len) {
    uint32_t hash = 2166136261u;
    for (size_t i = 0; i < len && i < (size_t)INT_MAX; i++) {
        hash = (hash ^ bytes[i]) * 16777619u;
    }
    return hash + (uint32_t)strlen("");
}
'''

BUILD = r'''use std::{env, path::PathBuf, process::Command};

fn tool(kind: &str, plain: &str) -> String {
    let target = env::var("TARGET").unwrap().replace('-', "_");
    env::var(format!("{kind}_{target}")).or_else(|_| env::var(kind)).unwrap_or(plain.into())
}

fn flags() -> Vec<String> {
    let target = env::var("TARGET").unwrap().replace('-', "_");
    let flags = env::var(format!("CFLAGS_{target}")).or_else(|_| env::var("CFLAGS"));
    flags.unwrap_or_default().split_whitespace().map(str::to_owned).collect()
}

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let object = out.join("csum.o");
    let target = env::var("TARGET").unwrap();
    let mut cc = Command::new(tool("CC", "cc"));
    match target.starts_with("wasm32") {
        true => cc.arg(format!("--target={target}")),
        false => cc.arg("-fPIC"),
    };
    cc.args(flags());
    let compiled = cc.args(["-O2", "-c", "csum.c", "-o"]).arg(&object).status().unwrap();
    assert!(compiled.success(), "compiling csum.c");
    let archive = out.join("libcsum.a");
    let _ = std::fs::remove_file(&archive);
    let archived = Command::new(tool("AR", "ar")).arg("crs").arg(&archive).arg(&object).status();
    assert!(archived.unwrap().success(), "archiving csum.o");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=csum");
    println!("cargo:rerun-if-changed=csum.c");
}
'''

LIB = r'''unsafe extern "C" {
    fn csum(bytes: *const u8, len: usize) -> u32;
}

pub fn hash(text: &str) -> u32 {
    unsafe { csum(text.as_ptr(), text.len()) }
}
'''

MACRO = r'''use proc_macro::TokenStream;

#[proc_macro]
pub fn hashed(input: TokenStream) -> TokenStream {
    format!("{}u32", csum::hash(&input.to_string())).parse().unwrap()
}
'''

MAIN = r'''fn main() {
    println!("{} {}", hasher::hashed!(hello), hasher::hashed!(zed));
}
'''


def main():
    work = tempfile.mkdtemp(prefix="wasm-c-macro-", dir="/tmp" if os.name != "nt" else None)
    print(f"work: {work}", flush=True)
    at = os.path.join(work, "workspace")
    write(at, "Cargo.toml", '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2024"\n'
          '[dependencies]\nhasher = { path = "hasher" }\n[workspace]\nmembers = ["hasher", "csum"]\n')
    write(at, "src/main.rs", MAIN)
    write(at, "hasher/Cargo.toml", '[package]\nname = "hasher"\nversion = "0.1.0"\nedition = "2024"\n'
          '[lib]\nproc-macro = true\n[dependencies]\ncsum = { path = "../csum" }\n')
    write(at, "hasher/src/lib.rs", MACRO)
    write(at, "csum/Cargo.toml", '[package]\nname = "csum"\nversion = "0.1.0"\nedition = "2024"\n'
          'links = "csum"\n')
    write(at, "csum/build.rs", BUILD)
    write(at, "csum/csum.c", C)
    write(at, "csum/src/lib.rs", LIB)
    passed = False
    base = None

    def run(command):
        ran = subprocess.run(command, cwd=at, env=base, capture_output=True, text=True, timeout=900)
        assert ran.returncode == 0, f"{command}: exit {ran.returncode}\n{ran.stdout}\n{ran.stderr}"
        return ran.stdout

    try:
        for mode in ("0", "1"):
            base = base_env(os.path.join(work, "home-" + mode))
            base["BLAZE_PROC_MACRO_WASM"] = mode
            for name in ("CC", "AR", "CFLAGS"):
                for target in ("wasm32_wasip1", "wasm32-wasip1"):
                    base.pop(f"{name}_{target}", None)
            base.pop("WASI_SYSROOT", None)
            expected = run(["cargo", "run", "-q"])
            ours = run(["blaze", "run", "-q"])
            assert ours == expected, f"wasm={mode}: {ours!r} != {expected!r}"
            print(f"  wasm={mode}: {ours.strip()}, as cargo's", flush=True)
            run(["blaze", "stop"])
        passed = True
    finally:
        if base:
            subprocess.run(["blaze", "stop"], cwd=at, env=base, capture_output=True)
        if passed:
            shutil.rmtree(work)
    print("PASS: a proc macro calling a build script's C, native and as WebAssembly", flush=True)


if __name__ == "__main__":
    main()
