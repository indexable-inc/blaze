//! `blaze-bstd-convert`: sans-IO Rust-to-`bstd` converter and explicit state-machine compiler.
//!
//! - [`catalog`]: mappings from Rust `std`, `alloc`, and common third-party crates onto `bstd`,
//!   `blaze-*`, and `core`, plus IO/effect classification.
//! - [`analyze`]: pure sans-IO workspace & Rust 2024 AST analyzer (`syntax` + `blaze-toml`),
//!   computing external dependency tiers, intra-crate transitive effect closures, implicit-state /
//!   mixed-IO struct candidates, and custom proc-macro `comptime` reflection candidates.
//! - [`questions`]: grouped, deterministic decision engine (`Question` + JSON-roundtrippable
//!   `Decisions`) so ambiguous choices are answered explicitly instead of silently guessed.
//! - [`sm_compiler`]: compiler pass lowering imperative/blocking/async Rust functions into
//!   explicit `#![blaze::sans_io]` state machines (`Effect`/`Event`/`Step`/`State`/`Machine::step`)
//!   with exact backward dataflow liveness analysis, plus thin `bstd` drivers, simulation test
//!   skeletons, mixed-IO struct splitting, and custom proc-macro `comptime` reflection scaffolds.
//! - [`rewrite`]: lossless `Cargo.toml` rewriter (`blaze-toml`) and span-checked Rust AST rewriter.
//! - [`report`]: Markdown and strict RFC 8259 JSON reports (`blaze-json`).

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![feature(register_tool)]
#![register_tool(blaze)]


extern crate alloc;
pub mod analyze;
pub mod catalog;
pub mod questions;
pub mod report;
pub mod rewrite;
pub mod sm_compiler;

#[cfg(test)]
mod tests;

pub use analyze::{WorkspaceAnalysis, WorkspaceSnapshot, analyze_workspace};
pub use questions::{Decisions, Question, build_questions};
pub use rewrite::{ConversionResult, convert_workspace};
pub use sm_compiler::{
    CompiledStateMachine, LivenessInfo, SmAllocMode, SmCfg, compile_file_functions,
    compile_file_functions_opts, compile_fn, compile_struct_sm, generate_c_interop_scaffold,
    generate_comptime_scaffold,
};

/// Typed error returned by `blaze-bstd-convert` serialization and CLI operations (`errors-name-the-thing`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertError {
    /// Failed to serialize a JSON document (`blaze-json`).
    JsonWrite { detail: alloc::string::String },
    /// Failed to parse a JSON decisions document (`blaze-json`).
    JsonParse { detail: alloc::string::String },
    /// A field in `decisions.json` violated the expected schema.
    InvalidDecisionsField { field: alloc::string::String, expected: &'static str },
    /// Filesystem or CLI usage error in the thin binary driver.
    Cli { context: alloc::string::String, detail: alloc::string::String },
}

impl core::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::JsonWrite { detail } => write!(f, "json write failed: {detail}"),
            Self::JsonParse { detail } => write!(f, "json parse failed: {detail}"),
            Self::InvalidDecisionsField { field, expected } => {
                write!(f, "decisions field `{field}` must be {expected}")
            }
            Self::Cli { context, detail } => write!(f, "{context}: {detail}"),
        }
    }
}

impl core::error::Error for ConvertError {}
