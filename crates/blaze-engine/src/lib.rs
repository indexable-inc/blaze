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
// `#[expect(blaze::..)]` names blaze's own lints; inert under rustc.
#![feature(register_tool)]
#![register_tool(blaze)]

mod as_rustc;
mod pin;
mod program;
mod update;
mod versions;

pub use program::main;

/// The system's allocator (the bulk of a build comes from `blaze-mem`'s frames, which give
/// their pages back themselves), counted, so the plane's `arena::account` and the reports see
/// its bytes per task. It never refuses, waits or limits: the kernel's limit is the one
/// ceiling. The same in every profile, the native one and the seed.
#[cfg(not(test))]
#[global_allocator]
static ALLOC: blaze_graph::arena::Counting<std::alloc::System> =
    blaze_graph::arena::Counting(std::alloc::System);

/// The tests hold the hot paths to allocation budgets.
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
