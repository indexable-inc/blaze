//! blaze's C toolchain driver (D174; docs/v2/plan/design/c.md "The toolchain side"). The
//! pinned clang runs natively, as blaze's own tool, inside blaze-cfront: a small helper linked
//! to the pinned clang's libraries whose virtual file system has nothing but what blaze serves
//! it, over one channel, file by file. So blaze learns every lookup, found or missing, and a
//! path under no root is refused before anything is read. The roots, paths, mounts and read
//! list are crates/blaze-roots'.
//!
//! This crate is a DRIVER crate: its cores are sans-IO modules, each marked so, and
//! [`driver`] and [`front`] alone touch a process.
//! - [`flags`]: the hermetic arguments of one unit.
//! - [`wire`]: the messages between blaze and the helper (helper/wire.h is their C++ side).
//! - [`job`]: one run of the helper, as a machine.
//! - [`decls`]: the `cfront-decls 2` codec, read straight into blaze-c's model.
//! - [`driver`]: the thin driver over bstd's served child, answering from a view of the roots.
//! - [`front`]: the helper as blaze-c's `CFrontEnd`, answered by the import.
//!
//! The helper's C++ is helper/ beside this crate: blaze's one piece of non-Rust code (c.md
//! decision 2), the file-system adapter, the argument hand-off and the declaration dump, with
//! nothing that decides. .github/scripts/llvm.py builds it against the pinned LLVM with
//! `"cfront": true`.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

// `alloc`'s `Vec` alone, for bstd's channel read in driver.rs and front.rs
// (`RunChild::recv_message` takes one): kept until bstd reads into blaze-own's.
extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod decls;
pub mod driver;
pub mod flags;
pub mod front;
pub mod job;
mod plant;
pub mod wire;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod ladder;

pub use plant::{DeclsPlant, JobPlant};
