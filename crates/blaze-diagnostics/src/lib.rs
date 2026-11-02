//! Compile errors and warnings (docs/v2/plan/process/building.md section 18; #1522, #1440, #780):
//! computed from the front end's queries that found them, each with the facts it read. They
//! are judged by usefulness, not by matching rustc (AGENTS.md `useful-not-parity`): an
//! error catches the mistake, points at the code that causes it, and says what is wrong and how
//! to fix it. The modules below started as ports of rustc's; their wording is free to change.
//!
//! - [`model`]: a diagnostic as data, rustc's `DiagInner`, with its facts.
//! - [`cx`]: what one is lowered with: spans as rustc gives them, files, facts.
//! - `lower`: each `hir::AnyDiagnostic` kind in rustc's words, one module per kind.
//! - `checks`: what rustc checks that the front end reports nothing of.
//! - [`lints`]: rustc's lints, their levels and the notes on where a level came from.
//! - [`print`]: rustc's printing of types and paths.
//! - [`emit`]: rustc's text (human, short, colour) and JSON, and its closing lines.
//! - [`check`]: a crate's diagnostics in the order rustc gives them, stopping where it stops.
//! - [`api`]: a diagnostic as the agent API sends it.
//!
//! A check is one task: its tables live in the task's [`blaze_mem::Frame`] ([`Cx::frame`]),
//! freed in one go when it returns. Only the diagnostics it gives outlive it.

#![feature(allocator_api, allocator_ext)]
#![forbid(unsafe_code)]

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

pub mod api;
pub mod cache;
pub mod check;
mod checks;
pub mod cx;
pub mod emit;
pub mod lints;
mod lower;
pub mod model;
pub mod print;
#[cfg(test)]
mod tests;

pub use check::{Options, check_crate, check_planned};
pub use cx::{Cx, Files};
pub use lower::{Found, Phase};
pub use model::{Code, Diag, Level, Span};
