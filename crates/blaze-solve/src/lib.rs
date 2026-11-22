//! blaze's own trait solver (docs/v2/plan/design/solve.md).
//!
//! Rust's semantics as the pinned nightly's next-generation solver implements them, ported rule
//! by rule from `vendor/rustc_next_trait_solver` (each rule names the reference function it
//! ports), on a data-oriented engine: goals, candidates and nested goals as fixed-size rows, one
//! explicit stack of frames (no recursion on a goal's depth), the reference's search graph for
//! cycles (`search`), canonical inputs and responses as words (`canon`), stalled obligations on
//! wake lists (`fulfill`), the ground fast path (`eval`, a term's markers byte) and a SIMD fingerprint prefilter
//! over impl buckets (`prefilter`).
//!
//! The solver learns about the program only through [`facts::Facts`], whose methods only the
//! recorder can call, so every solved goal carries the read list it is valid by (rule 77).

#![no_std]
#![forbid(unsafe_code)]
#![feature(allocator_api, portable_simd, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

#[cfg(any(test, feature = "testing"))]
extern crate std;

pub mod assembly;
pub mod canon;
pub mod error;
pub mod eval;
pub mod facts;
pub mod fold;
pub mod fulfill;
pub mod object_shape;
pub mod prefilter;
pub mod readlog;
pub mod search;
pub mod share;
pub mod term;
pub mod trait_goals;
pub mod tys;
pub mod witness;
pub mod why_no;
pub mod why_maybe;

// The helpers' modules (solve.md, "Candidates and merging").
pub mod alias;
pub mod async_fn;
pub mod auto_summary;
pub mod builtin;
pub mod coherence;
pub mod structural;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(test)]
mod tests;

pub use assembly::{Clause, EnvId, Goal};
pub use error::{SolveError, Stop};
pub use eval::{Answer, Solver};
pub use facts::{Facts, Read, Source, Stopping};
pub use term::{Abi, Certainty, Def, FnPtrHeader, Pred, Safety, Ty, TyKind, TypingMode};
pub use tys::Tys;
