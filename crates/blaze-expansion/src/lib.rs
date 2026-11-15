//! Macro calls and build-script runs as records keyed before the work (target decisions 2 and
//! 7 of 2026-11-13; AGENTS.md rules 21, 65 and 71). The one place in blaze where a record keeps
//! a list of reads for outside inputs: everything else is keyed by its text alone.
//!
//! - [`macro_call`]: the record of one proc-macro or declarative-macro call. Its key, made
//!   before anything runs, is the expander's code identity, the input tokens with every span
//!   relative to its anchor and every anchor and syntax context numbered by first appearance
//!   ([`tokens`]), the attribute or derive context, the settings that can change the output,
//!   and the state the expander starts from (the core's `StartState`). Its value is the
//!   output tokens (flat, position-free, with the hygiene marks a later name resolution needs),
//!   the diagnostics, and the state the call left.
//! - [`script_run`]: the record of one build-script run. Its key is the script's unit code key,
//!   the toolchain, the host and exactly what the script is given; its value is the directives
//!   it printed (typed by cargo's own reader, `blaze_build_script::output`), its `OUT_DIR` as a
//!   content-addressed tree, its exit, and its declarations checked against its trace.
//! - [`reads`]: what a run asked of the outside, each with the digest of its answer
//!   ([`reads::Ask`], [`reads::ReadList`]); [`host`] says, for every call the wasm host and the
//!   script sandbox serve, whether it makes an entry, is in the key, is answered from the key,
//!   or is refused.
//! - [`machine`]: the sans-IO machine. A call's key is asked of the store ([`machine::lookup`]):
//!   each read list filed under the key is verified against the answers now, and a list whose
//!   answers are all equal is a hit. A miss goes to the flight table ([`machine::Flights`]),
//!   which runs each key at most once per request and parks every other asker of that key on
//!   it; the run's reads are settled against the answers after the run ([`machine::settle`]),
//!   and a read that moved under the run ends the run as raced, with nothing written.
//! - [`chain`]: the calls of an expander that keeps state across calls, in the calling crate's
//!   source order, each keyed by the state it starts from, run speculatively and run again at
//!   most once when the state before it turned out otherwise.
//! - [`modules`]: an expander's compiled module is itself a record, keyed by its crate's unit
//!   key, and "which expander does this path name" is a fact of that record. A task that needs
//!   a module not built yet waits on that one key; no other task waits (the seam that replaces
//!   `hir_expand::proc_macro::ProcMacros::give`, a write that stopped every reader).
//! - Kinds, keys, records, the two-level outside form, proofs and the store are the core's
//!   (`blaze_keyed`): a call is its `Expansion`, a run its `ScriptRun`, a module its
//!   `ExpanderModule`. [`record`] keeps only this crate's names for what a refusal is about.
//!
//! Everything here is a sans-IO core (`#![blaze::sans_io]`): it reads no clock, file or
//! variable. The store is the core's `blaze_keyed::store::Store` (records are immutable and
//! read into the asker's frame); the answers of a read list come through [`reads::Answers`]
//! (the view's tree after the request's barrier); a run is a [`machine::Begin::Run`] ticket the
//! driver carries out and ends.

#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

pub mod chain;
pub mod flat;
pub mod host;
pub mod machine;
pub mod macro_call;
pub mod modules;
pub mod reads;
pub mod record;
pub mod script_run;
pub mod tokens;

#[cfg(test)]
mod tests;

pub use record::{Digest, NOTHING, Subject, What};
