//! s7-env-01: `std::env::consts::ARCH`. std writes it `pub const ARCH: &str =
//! env!("STD_ENV_ARCH");` (library/std/src/env.rs:1112), and std's build script sets the
//! variable to the target's arch: `println!("cargo:rustc-env=STD_ENV_ARCH={}",
//! env::var("CARGO_CFG_TARGET_ARCH").unwrap());` (library/std/build.rs:82). Both targets blaze
//! builds for are `aarch64` (macOS and Linux), so the line is one on either; `OS` and `FAMILY`
//! differ between them and are not printed.
//!
//! The cause (rule 23): with no `STD_ENV_ARCH` in std's environment (blaze-world toolchain.rs
//! `STD_SCRIPT_CFG`, which on 2026-11-21 carries the script's cfg and not its env pair), the
//! const's `env!` is "the variable is not set" (blaze-expand failed.rs) and this rung is REFUSED
//! naming it. Twin: the same const compared, so a wrong value is WRONG (status 1), never a
//! printed line that only looks right.

fn main() {
    let arch = std::env::consts::ARCH;
    println!("{arch}");
    println!("{}", arch == "aarch64");
    std::process::exit(if arch == "aarch64" { 0 } else { 1 });
}
