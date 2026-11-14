//! Lint findings are records; they arrive with the answer when cheap and after it when not; and a
//! plain Rust project adopts blaze's checks one step at a time (docs/v2/plan/design/target.md,
//! "Findings").
//!
//! - [`finding`]: the finding record: lint, item-relative range, typed arguments, the fix as
//!   item-relative edits. No level and no drawn text, so a level change recomputes nothing.
//! - [`kinds`]: the four record kinds: [`kinds::BodyFindings`] (per body, traced like the body's
//!   check), [`kinds::ItemRows`] (per item, what a fold reads), [`kinds::FoldFindings`] (a
//!   workspace fold's leaves and root, keyed by the digest of what they folded),
//!   [`kinds::DeferredFindings`] (an expensive analysis, keyed by what it read).
//! - [`pass`]: a pass is a declared set of lints computed together, with its class and expected
//!   cost; the class rule.
//! - [`fold`]: a workspace fold as a fixed tree of leaves bucketed by subject, updated by the
//!   delta of the rows that moved.
//! - [`sched`]: the sans-IO scheduler of one workspace: the three classes, the budgets from the
//!   machine descriptor, cancellation by a newer snapshot, pending by (lint, item).
//! - [`wait`]: how long a command waits (`Answer`, `Cheap`, `All`) and the reply.
//! - [`view`]: levels joined at view time, the lint universe, and new/fixed since a base.
//! - [`ladder`]: the adoption ladder (`rust`, `suggest`, `edition`, `strict`) and the report of
//!   what the next step costs.
//!
//! Nothing here does IO: the host resolves each work item against the store by key, runs what
//! missed on its pool, and feeds the outcomes back.

#![forbid(unsafe_code)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
// `#![blaze::sans_io]` in each module; inert under rustc.
#![register_tool(blaze)]

pub mod finding;
pub mod fold;
pub mod kinds;
pub mod ladder;
pub mod pass;
pub mod sched;
pub mod view;
pub mod wait;

#[cfg(test)]
mod tests;

/// A bug a control plants, for the oracle of the simulation to catch (rule 43). Each names the
/// place it is planted in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// [`sched::Scheduler::command`] issues again work it already has queued or running for the
    /// same (pass, subject, key): a finding computed twice.
    ReissuesKeptWork,
    /// [`pass::class_of`] keeps a pass's declared class when one of its lints is at deny: a gate
    /// lint deferred.
    DefersGate,
    /// [`fold::Fold::apply`] rewrites the touched leaves from the new rows only, keeping the old
    /// rows of a contributor that moved.
    FoldKeepsStaleRows,
    /// A newer command cancels every deferred work, not only what reads something that moved.
    CancelsAll,
    /// A newer command keeps deferred work that reads something that moved.
    KeepsMoved,
    /// The reply leaves deferred work out of `pending`.
    PendingOmitsDeferred,
    /// [`kinds::BodyFindings`]' key covers the view's levels: a level change recomputes.
    KeyCoversLevels,
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
