//! Exact test selection (docs/v2/plan/design/compile-time.md, idea 3;
//! docs/v2/plan/design/compile-time.md): a test runs again only when a function it
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
//! - [`ask`]: what a kept result asks, as a blaze-keyed outside shape, and the value filed at
//!   its address (docs/v2/plan/design/test-selection.md).
//! - [`index`]: a suite's index head and postings: each kept test's row, and which tests ask
//!   each function or read, so an edit finds its candidates without reading every record.
//! - [`machine`]: one request's selection as a sans-IO machine on those records: cached with
//!   why, ran, or not kept with why, per test. It replaces [`select`] and [`selector`] once the
//!   runner is wired to it.
//!
//! The store, the shapes of what a run read and the traced inputs are `blaze-usercode`'s
//! ([`blaze_usercode::test::Passes`], [`blaze_usercode::index`], [`blaze_usercode::trace`]) for
//! [`select`]; [`machine`] files its records through blaze-keyed's.

#![forbid(unsafe_code)]
#![feature(allocator_api, btreemap_alloc)]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

pub mod api;
pub mod ask;
pub mod index;
pub mod machine;
pub mod record;
pub mod select;
pub mod selector;
pub mod table;

/// A bug planted on purpose, so a test shows its oracle catches it (rule 43). `None` in every
/// real use. Each names the place it is planted in, all in [`machine`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Plant {
    #[default]
    None,
    /// A kept pass's shape leaves out one function the run executed.
    AskLeavesOutFunction,
    /// A kept pass's shape leaves out the run's first outside read.
    AskLeavesOutRead,
    /// A failing run is reported as a cached pass.
    FailureReportedCached,
    /// The program's changed functions are dropped: the changed-set shortcut reports a test
    /// cached that executed one.
    ShortcutSkipsChanged,
}

#[cfg(test)]
thread_local! {
    pub static PLANT: std::cell::Cell<Plant> = const { std::cell::Cell::new(Plant::None) };
}

/// Whether `plant` is planted: only ever in this crate's tests.
#[cfg(test)]
pub fn planted(plant: Plant) -> bool {
    PLANT.get() == plant
}

/// Whether `plant` is planted: only ever in this crate's tests.
#[cfg(not(test))]
#[inline(always)]
pub fn planted(_plant: Plant) -> bool {
    false
}

pub use record::Record;
pub use select::{Basis, Cached, Choice, Ran, Reached, Subject, Why};
pub use selector::Selector;
pub use table::{Bound, FnId, Image, Set, Version};

/// Counts allocations so the hot paths' tests hold them to a budget.
#[cfg(all(test, not(blaze_check)))]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

#[cfg(test)]
mod sim_tests;
