//! LIR, blaze's low-level IR (docs/v2/plan/design/lir.md): one monomorphized function after
//! layout, the calling convention, drop elaboration and the UB checks, before instruction
//! selection. It is the one input of code generation: the copy-and-patch stencils, LLVM and
//! blaze's wasm emitter each read it, and none of them needs the front end.
//!
//! # Encoding
//!
//! A function ([`Func`]) is columns, Zig's tag plus one word ([docs/v2/plan/design/data.md]):
//! per instruction an [`Op`] (`ops`, a byte), its [`Flags`] (`flags`, a byte), its result
//! type ([`Ty`], `tys`, two bytes) and one word (`data`): an operand, an immediate, or where
//! its operands start in `extra`. Cold per-instruction columns sit apart: the memory-attribute
//! row (`attrs`) and the source location (`locs`). Signatures, slots, references, data, asm
//! and debug tables are columns too, lists in CSR.
//!
//! **A value is the index of the instruction that makes it.** A block's parameters are its
//! leading [`Op::Param`] instructions, so values are numbered in layout order (each block's
//! parameters, then each instruction's result) and two builds of one function are the same
//! bytes with no renumbering pass. The type of any operand is `tys[v]`, with no table beside.
//! Blocks are numbered in layout order too: block `b` is `ops[block_starts[b]..]` up to the
//! next block's start.
//!
//! # Semantics
//!
//! Every operation without flags is defined, as Cranelift's are: a shift masks its amount,
//! division by zero traps, `fpto*i` saturates, a load reads the bytes there. A flag is one of
//! three kinds ([`FlagKind`]): a *promise* is opt-in UB a consumer may drop (`nsw`,
//! `inbounds`, a load's range), a *meaning* must be honoured or strengthened (`volatile`, an
//! atomic ordering, a fence's scope), a *hint* may be dropped (`cold`, `expect_true`). The
//! facts are exactly those rustc's LLVM backend gives LLVM, no TBAA.
//!
//! # Parts
//!
//! - [`ty`], [`op`] and [`flags`]: the vocabulary, with [`Op::info`] saying each
//!   operation's operands, result, flags and whether it ends a block.
//! - [`func`]: the columns ([`Func`], owned in an allocator; [`View`], borrowed, of a
//!   `Func` or of a record read in place) and the rows of its tables.
//! - [`builder`]: [`Builder`], what the MIR lowering calls, with a full sink and a
//!   requests-only one (the reference walk).
//! - [`verify`]: the structure every reader relies on, then types, flags and dominance.
//! - [`print`] and [`parse`]: the text form, which round-trips byte for byte.
//! - [`persist`]: the blaze-format record, the columns as one buffer read in place.
//! - [`coroutine`]: the pass that splits a coroutine body, the one place `suspend` and
//!   `cancelled` are taken.
//!
//! The crate is a sans-IO core: it reads no clock, file or thread.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

/// LIR's integer arithmetic, one home: what the evaluator runs and the simplify stage folds by.

extern crate alloc;
mod arith;
/// Bodies by identity and what a direct call reaches: the evaluator's and the inliner's seam.
pub mod body;
pub mod builder;
/// The coroutine pass (decision D73): a body with `suspend`s to its `poll`, its state's drop
/// glue and the state's layout.
pub mod coroutine;
/// The reference evaluator backends are tested against: in this crate's tests, and for a
/// backend's tests through the `eval` feature. Never in a build.
#[cfg(any(test, feature = "eval"))]
pub mod eval;
pub mod flags;
pub mod func;
/// The trivial inliner (D98's first point): small loop-free bodies and `#[inline(always)]`
/// spliced in front of the fast tier.
pub mod inline;
/// Legalize (INTRINSICS.md 3.2, 3.5): vectors to scalars, `f16` widened, `f128` to
/// compiler_builtins calls, in front of the Reference and Fast tiers only.
pub mod legalize;
pub mod loop_entry;
/// Cross-function bottom-up Tarjan SCC ThinLTO and JIT tier-up planning.
pub mod lto;
pub mod op;
pub mod parse;
pub mod persist;
/// PGO site and body weight tables for guided inlining, LTO, and JIT tier-up.
pub mod pgo;
pub mod print;
/// The linear simplify stage in front of the Fast tier (backend-next.md D100 point 1): scalar
/// replacement of slots that do not escape, then (S2) propagation and dead code.
pub mod simplify;
/// ThinLTO function summaries for cost-model inlining and incremental early cutoff.
pub mod summary;
pub mod ty;
pub mod verify;

pub use builder::{BuildError, Builder, DataPosition, Full, Requests, RequestsOnly, Sink};
pub use func::PositionField;
pub use flags::{AtomicRmw, FlagClass, FlagKind, Flags, FloatCond, IntCond, Ordering};
pub use func::{Block, BlockKind, Func, Head, Value, View};
pub use op::{End, Field, Info, Layout, Op, Operands, Res};
pub use parse::{ParseError, parse};
pub use print::print;
pub use simplify::{
    bce, coldsplit, coro_opt, devirt, egraph, escape, ifconv, loopnest, memopt, prefetch, rvsdg, sched, semproof, slp,
    soa, speculate, superopt, taildup, unroll, unswitch, vectorize,
};
#[cfg(any(test, feature = "eval"))]
pub use simplify::validate;
pub use ty::{Scalar, Ty};
pub use verify::{Verified, VerifiedFunc, VerifyError, verify};

/// The version of what LIR's bytes mean: its operations, their operands and flags, its types
/// and tables. Part of the record's schema ([`persist::SCHEMA`]) and of every key of a LIR, so
/// a change to any of them is a new version. 3: data rows hold source positions
/// (`DataRec::positions_first`/`positions_len`, the `data_positions` column), filled when the
/// program is placed (rule 71); 2 had none.
pub const VERSION: u64 = 3;

#[cfg(test)]
mod coroutine_tests;
#[cfg(test)]
mod inline_tests;
#[cfg(test)]
mod loop_entry_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tiers;
