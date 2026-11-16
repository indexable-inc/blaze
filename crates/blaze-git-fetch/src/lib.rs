//! A commit of a git repository, fetched as Cargo's git dependencies need it, with no `git2`,
//! `gix` or `git` binary: smart HTTP protocol v2 ([`fetch()`]: `ls-refs`, then `fetch` with
//! `deepen`), the pack read whole ([`Pack`]: offset and reference deltas, every size bounded),
//! and the commit's files handed out ([`files`]), each name checked so none leaves the
//! checkout.
//!
//! **SHA-1 is git's object id here, never trust** ([`sha1`]). A git dependency is trusted by
//! the commit its lock file pins, fetched over TLS from its host; blaze's records use BLAKE3
//! and SHA-256.
//!
//! Everything but [`fetch()`] is sans-IO: bytes in, bytes out.

#![no_std]
#![cfg_attr(test, feature(allocator_api))]
// `#![blaze::sans_io]` in the modules names blaze's tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

#[cfg(test)]
extern crate std;

mod fetch;
mod oid;
mod pack;
mod pkt;
mod plant;
mod protocol;
pub mod sha1;
mod tree;

#[cfg(test)]
mod tests;

pub use fetch::{Fetched, GitError, GitWhy, Step, Want, fetch};
pub use oid::{Kind, Oid};
pub use pack::{DeltaWhy, Limits as PackLimits, Object, Pack, PackError, PackWhy};
pub use pkt::{Lines, Pkt, PktWhy};
pub use protocol::{check_advertisement, fetch as fetch_request, fetched_pack, find_ref, ls_refs};
pub use tree::{File, Limits as TreeLimits, Mode, TreeError, TreeWhy, files};
