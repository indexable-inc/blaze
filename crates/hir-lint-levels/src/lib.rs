//! The level of each lint where it is, as rustc decides it: the command line (`-A`, `-W`,
//! `--force-warn`, `-D`, `-F` in order, `--cap-lints`), the lint attributes on a node and
//! around it (through the macro call an expansion is of, the `mod` a file is of, to the
//! crate's, `-Z crate-attr`'s with them), `cfg_attr`, `#![warn(warnings)]`, `forbid`,
//! `expect`, `reason`. A front-end crate: what a program does can depend on a level (the const
//! evaluator stops on `deny(long_running_const_eval)`), so the rule is the front end's, and
//! blaze-diagnostics reads it from here.
//!
//! - [`attrs`]: the lint attributes of a node as rustc reads them, and the node around it.
//! - [`builder`]: rustc's `LintLevelsBuilder`, as the early lint pass has it and as the HIR has
//!   it, with what it finds wrong handed to a [`builder::Sink`] (blaze-diagnostics words it).
//! - [`query`]: the level at an item or an expression of its store, as queries: rustc's
//!   `shallow_lint_levels_on` and `lint_level_at_node`.
//!
//! The lints themselves, their groups and names, and the command line's type are
//! `blaze-lint-defs`', which has no queries (rustc's `rustc_lint_defs`).
//!
//! Follows rustc's `compiler/rustc_lint/src/levels.rs`, `compiler/rustc_middle/src/lint.rs`
//! and `compiler/rustc_session/src/config.rs` (`get_cmd_lint_options`) (MIT or Apache-2.0).

#![forbid(unsafe_code)]

pub mod attrs;
pub mod builder;
pub mod query;
#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

pub use self::{
    builder::{Event, LevelCx, Levels, Malformed, SetId, Sink, Source, SourceId, Spec},
    query::{LevelError, ScopeLevels, crate_lint_level, crate_lint_levels, lint_level, lint_level_at, lint_levels_on},
};
pub use blaze_lint_defs::{Level, LintId, LintOptions};
