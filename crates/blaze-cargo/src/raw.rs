//! The calls into the kernel, the C library and the allocator that std and the crate's
//! dependencies have no safe wrapper for, each behind a safe function that no argument can
//! make unsound (docs/v2/plan/design/unsafe.md). Every one is a plain system call, not a fast
//! path: none has a safe twin it could be checked against, and none views bytes in place.
//!
//! The rest of the crate's own unsafe code is `socket::unix` and `socket::windows` (who is at
//! the other end of a socket, and Windows' access control lists), and the `unsafe fn`s that set
//! the process's environment before any thread starts (`cargo::command` and what it calls in
//! `cargo::toolchain`, `cargo::layout` and `cargo::backend`), which only their caller can
//! uphold.

pub mod memory;
pub mod process;
pub mod system;
