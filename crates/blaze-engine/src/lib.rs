//! The program `blaze`: blaze's cargo, its client and its daemon ([`blaze_cargo::cli`]), and
//! what runs before it ([`main`]): the sandbox launcher, the stand-ins a build script runs, and
//! the release of blaze a workspace pins (`pin.rs`, `versions.rs`), and blaze's own updates
//! (`update.rs`).

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the program `blaze`: stdout is its output, stderr is where it speaks"
)]
#![deny(unsafe_code)]

mod as_rustc;
#[cfg(feature = "native")]
mod heaps;
#[cfg(feature = "native")]
mod huge;
#[cfg(feature = "native")]
pub mod malloc;
mod pin;
mod program;
mod update;
mod versions;

pub use program::main;

/// What the general allocator still serves (startup, configuration, rare large one-offs; the
/// bulk of a build comes from `blaze-mem`, which gives its pages back itself) wants the fastest
/// small-object allocator, mimalloc, with a heap per blaze-mem tag (`heaps.rs`). Counted, so
/// that the plane's `arena::account` and admission's `arena::held` see its bytes, and each
/// tag's share of them (`blaze_mem::tag::Counters::heap`) is named in the memory line. C's and
/// C++'s allocations (LLVM's) come through the same counts (`malloc.rs`).
#[cfg(all(not(test), feature = "native"))]
#[global_allocator]
static ALLOC: blaze_graph::arena::Counting<heaps::Heaps> =
    blaze_graph::arena::Counting(heaps::Heaps);

/// Without `native` (the seed profile, which has no C): std's allocator, counted the same way.
#[cfg(all(not(test), not(feature = "native")))]
#[global_allocator]
static ALLOC: blaze_graph::arena::Counting<std::alloc::System> =
    blaze_graph::arena::Counting(std::alloc::System);

/// The tests hold the hot paths to allocation budgets.
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
