//! A build script's run as blaze keys and keeps it (docs/v2/plan/design/build-scripts.md), sans-IO.
//!
//! A build script is user code: it runs only in the sandbox (rule 9), as a `wasm32-wasip1`
//! module in crates/blaze-proc-macro's runner, with the environment Cargo documents and nothing
//! else. This crate is the part of that run that does no IO:
//!
//! - [`instr`]: the instruction lines a script prints, read into typed values, every documented
//!   form of the old `cargo:` and the new `cargo::` syntax, the output bounded before it is read;
//! - [`env`]: the environment a script is given, computed from its unit;
//! - [`outputs`]: the record of a run (instructions and the files written into `OUT_DIR`),
//!   bounded, read in place, round-tripping byte for byte;
//! - [`key`]: the record's base key, over every input known before the run;
//! - [`probe`]: the compiler a script spawns, answered by blaze (never rustc: rule 1);
//! - [`git`]: the `git` a script spawns, answered from the checkout's `.git`, read-only (D53);
//! - [`cx`]: the outputs laid over a crate's expansion host (`env!("OUT_DIR")`, `include!`).
//!
//! `examples/census.rs` is the census of a workspace's scripts by reading alone.

#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![forbid(unsafe_code)]

pub mod cx;
pub mod env;
pub mod git;
mod git_objects;
pub mod host_tools;
pub mod instr;
pub mod key;
pub mod native;
pub mod outputs;
pub mod pinned;
pub mod probe;
pub mod toolchain;

#[cfg(test)]
mod git_tests;
#[cfg(test)]
mod tests;

pub use env::{Env, Given, env};
pub use instr::{Instr, Line, parse};
pub use key::{CMode, KeyInputs, base};
pub use outputs::{Outputs, OutputsRef};
