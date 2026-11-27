//! `bstd::sh`: blaze's own structured shell language core (docs/v2/plan/design/sh.md).
//!
//! - Structured values ([`Value`]: tables, records, lists, sizes, durations, dates, closures)
//!   flow through pipelines instead of raw text bytes.
//! - Persistent copy-on-write state ([`EngineState`], [`PMap`], [`PVec`]): `state.fork()` is O(1).
//! - Sans-IO (`#![no_std]`, `#![blaze::sans_io]`, `#![forbid(unsafe_code)]`): IO, clock,
//!   external processes, and `rust { ... }` cells come out as [`Request`]s answered by the host.

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod ast;
pub mod builtins;
pub mod error;
pub mod lex;
pub mod machine;
pub mod parse;
pub mod persist;
pub mod protocol;
pub mod schema;
pub mod span;
pub mod state;
pub mod value;

#[cfg(test)]
mod persist_tests;
#[cfg(test)]
mod tests;

pub use builtins::{BUILTIN_SPECS, find_spec, is_builtin};
pub use error::{ErrorKind, ShellError};
pub use lex::{HighlightKind, HighlightSpan, highlight, is_incomplete};
pub use machine::{DEFAULT_FUEL, Machine};
pub use parse::parse;
pub use persist::{PMap, PVec};
pub use protocol::{
    Answer, BuiltinSpec, DirEntry, EvalOutcome, ExternalMode, ExternalResult, FileKind, FsOp,
    ProcEntry, Request, RustEvalResult, Step, StreamChunk, SysSnapshot,
};
pub use span::{LineCol, Span, line_col};
pub use state::{DefData, EngineState, RustCell, VarSlot};
pub use value::{
    Bytes, ClosureData, ColDisplay, CommandOutput, DateTime, Nanos, OutputChunk, OutputStream,
    PathKind, PathMember, RangeVal, Record, Table, Value, format_date, format_exact_elapsed,
    format_relative_time, format_u64_commas,
};
