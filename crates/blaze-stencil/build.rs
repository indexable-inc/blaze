//! Makes the stencil libraries (docs/v2/plan/lanes/fastcompile/stencil.md, "Why a build
//! script"): the generator in `gen/` writes each stencil's LLVM IR, compiles it through
//! llvm-sys and writes one library per variant to `OUT_DIR`. It runs no LLVM tool: llvm-sys, a
//! build-dependency, is built first, and its build script refuses any LLVM but the pinned one
//! (vendor/llvm-sys/pin.rs).
//!
//! The shape set, the hole forms and the library format are the crate's own files, included
//! here so the generator and the patcher read one definition. They use nothing but `std`,
//! `blaze_ids::bits` and each other.

// The generator uses part of what the shared files hold for the patcher.
#[allow(dead_code)]
#[path = "src/enc.rs"]
mod enc;
#[allow(dead_code)]
#[path = "src/table/format.rs"]
mod format;
#[path = "gen/mod.rs"]
mod generate;
#[allow(dead_code)]
#[path = "src/shape.rs"]
mod shape;

fn main() {
    generate::main();
}
