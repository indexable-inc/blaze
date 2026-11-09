//! A model checker of blaze's store and kernel protocol (docs/v2/plan/design/storemodel.md).
//!
//! It drives the real state machines, `blaze_cas::Store` with its `Writer`s and `Reader`s and
//! `blaze_kernel::Kernel`, through every schedule of a few actors (two writers on one key or one
//! shape chain, a reader, the store's IO), and after every IO step restores the store from
//! every disk a crash may leave and checks it. No second spec stands in for the code: the
//! store is sans-IO (it hands back effects and takes completions), so a deterministic file
//! system ([`fs::CrashFs`]) and a scheduler of actor threads ([`actors`]) are all it takes.
//!
//! - [`fs`]: what a crash keeps, as ALICE models it (Pillai et al., OSDI 2014): a prefix of
//!   each file's unsynced changes, the last perhaps torn, and the directory entries a
//!   [`fs::Persist`] model allows.
//! - [`explore`]: stateless depth-first search over the choices, bounded, with a seeded random
//!   mode, shrinking, and a report naming the schedule and the crash point.
//! - [`store`]: the controller every scenario runs on, the checks of a restore, and the
//!   store's scenario.
//! - [`kernel`]: the kernel's scenario: two gets racing for one shape slot, and an impure step
//!   whose second value must be refused.
//!
//! Each property is a test named for it in `tests.rs`.

#![feature(register_tool)]
#![register_tool(blaze)]

pub mod actors;
pub mod explore;
pub mod fs;
pub mod kernel;
pub mod store;
#[cfg(test)]
mod tests;

/// The properties a failure names.
pub mod properties {
    pub const SERVES_ONLY_WRITTEN: &str =
        "a restore never serves a value other than one some writer fully wrote under that key";
    pub const NEVER_SILENTLY_EMPTY: &str = "a restore never silently starts empty";
    pub const REFUSED_NOT_OVERWRITTEN: &str =
        "two different values under one identity are refused, never overwritten";
    pub const ANSWERED_SURVIVES: &str = "after a build answers, its results survive any crash";
    pub const RESTORE_EQUALS_FRESH: &str = "the restore after any crash equals a fresh build";
    pub const EVERY_WAIT_ENDS: &str = "every wait ends";
    pub const KERNEL_GETS_END: &str =
        "a kernel get ends in a value, or for an impure step a conflict";
    pub const NO_PANIC: &str = "no step panics";
    pub const EXPLORED: &str = "the search covers every schedule within its bounds";
}
