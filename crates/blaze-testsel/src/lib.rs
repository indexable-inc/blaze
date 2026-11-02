//! Exact test selection (docs/v2/plan/design/compile-time.md, idea 3;
//! docs/v2/plan/lanes/fastcompile/select.md): a test runs again only when a function it
//! executed is not as it was, or something it read is not; every other test reports its last
//! pass, marked cached, with how long that run took and what it was selected by.
//!
//! - [`record`]: what a test process says it executed: a byte per jump-table slot in a region
//!   the fork server maps shared and the runtime writes on each slot's first call.
//! - [`table`]: a test binary's functions by slot, each by its identity ([`FnId`]) and its
//!   bound code ([`Bound`]: the code and what each of its refs binds to), and the hash of all
//!   code not called through a slot.
//! - [`select`]: [`select::choose`] whether a test runs, [`select::keep`] how it ran. A pass is
//!   kept under the set of functions it executed, or, when its record is not whole, the set it
//!   reaches, saying why ([`Basis`]); never under a set that is not one of these.
//! - [`selector`]: the same choices for a whole suite across edits, each edit costing the
//!   functions it changed times the sets that hold them ([`Selector`]).
//! - [`api`]: the cached mark as the agent API carries it.
//!
//! The store, the shapes of what a run read and the traced inputs are `blaze-usercode`'s
//! ([`blaze_usercode::test::Passes`], [`blaze_usercode::index`], [`blaze_usercode::trace`]).

#![forbid(unsafe_code)]

pub mod api;
pub mod record;
pub mod select;
pub mod selector;
pub mod table;

pub use record::Record;
pub use select::{Basis, Cached, Choice, Ran, Subject, Why};
pub use selector::Selector;
pub use table::{Bound, FnId, Image, Set, Version};

/// Counts allocations so the hot paths' tests hold them to a budget.
#[cfg(all(test, not(blaze_check)))]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

#[cfg(test)]
mod sim_tests;
