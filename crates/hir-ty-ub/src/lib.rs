//! blaze's own Miri: a checker of undefined behavior over `hir-ty-mir`'s MIR evaluator, for
//! `blaze test --ub` (docs/v2/plan/design/unsafe.md, docs/v2/plan/lanes/safety/notes.md).
//!
//! The evaluator runs the program and calls [`check::Checker`] through its `Machine` trait at
//! every allocation, access, typed copy, call, return and thread switch. The checker holds what
//! the evaluator's bytes don't say, and stops the run at the first undefined behavior:
//!
//! - [`memory`]: every allocation by address, which bytes are initialized, which carry a
//!   pointer's provenance, and the checks of bounds, liveness, alignment and frees, and leaks;
//! - [`borrows`]: Tree Borrows, ported from Miri rule for rule;
//! - [`race`]: vector clocks for data races, weak memory for atomics, and the seeded scheduler
//!   that picks the interleaving;
//! - [`check`]: the `Machine` itself, validity of values at their types (which retags every
//!   reference a typed copy writes, as Miri's validity visitor does), and [`check::run_test`].
//!
//! Without `--ub` nothing here runs: the evaluator's `Plain` machine does nothing and costs
//! nothing, and what Rust means is unchanged (superset-of-rust).

#![forbid(unsafe_code)]

pub mod borrows;
pub mod check;
pub mod ids;
pub mod memory;
pub mod race;
pub mod ub;

pub use check::{Checker, Config, Outcome, run_test};
pub use ub::{Found, Ub};
