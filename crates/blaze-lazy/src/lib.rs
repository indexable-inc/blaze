//! Lazy compilation, per function (docs/v2/plan/design/compile-time.md, change 1;
//! docs/v2/plan/design/bottom.md, "The commands").
//!
//! A run compiles only the functions that execute. Every function is called through its
//! slot; a lazy function's slot starts at its *entry*, a 16-byte stub of this crate's that
//! hands its [`LazyId`] to one shared *resolver* ([`stub`]). The resolver saves every register
//! a call can carry a value in, calls the hook its region's cell names for the function's
//! code, restores them and jumps to the code published in the slot. The next call goes
//! through the slot straight to the code.
//!
//! The serving side is [`fetch::Fetcher`], a deterministic state machine: it maps a
//! [`LazyId`] to its instance, compiles it once however many threads ask (single flight),
//! places and publishes it, answers every asker, and meanwhile compiles what is likely to run
//! next at lower priority: the callees of what it compiled, and what history says the run
//! reaches. [`runner::Runner`] drives it on threads against a [`runner::Compiler`] and a
//! [`runner::Placer`]. The daemon's implementations of both, the run process's hook that asked
//! the daemon (`hook`) and codegen's lazy build kept in the daemon (`compile`) were deleted
//! with the daemon on 2026-11-15: the stub is to call a resolver that reads the code record by
//! its key, made on a miss by the program's helper (docs/v2/plan/parked.md).
//!
//! [`plan::Plan`] says what is compiled ahead instead: what runs before `main` and the entry.
//! Every function's address is its slot's stub, compiled or not, so laziness changes no
//! address the program can see ([`plan`] says why each case is exact).
//!
//! Release builds are compiled whole: [`mode::allow`] refuses lazy compilation for them,
//! naming the profile or the output.

#![deny(unsafe_code)]
#![feature(allocator_api)]

pub mod fetch;
pub mod mode;
pub mod plan;
pub mod runner;
pub mod session;
pub mod stub;
pub mod wire;

#[cfg(test)]
mod tests;

/// Counts allocations so the hot paths' tests hold them to a budget.
#[cfg(all(test, not(blaze_check)))]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

pub use mode::{Purpose, Refused, allow};
pub use plan::Plan;
pub use session::Session;

/// A lazy function's index among its session's lazy functions: what its entry hands the
/// resolver, and the index of every per-function column of the serving side.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct LazyId(pub u32);

impl LazyId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// The most lazy functions one session has: an entry hands its index to the resolver as a
/// sign-extended 32-bit immediate on x86-64, so it stays below 2^31.
pub const MOST_LAZY: u32 = 1 << 31;
