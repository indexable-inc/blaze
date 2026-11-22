//! A commit of a git repository, as Cargo's git dependencies need it, with no `git2`, `gix` or
//! `git` binary: smart HTTP protocol v2's requests and answers ([`fetch_request`],
//! [`fetched_pack`]; blaze-fetch carries them over HTTPS), the pack indexed in place and read
//! one object at a time ([`Index`]: offset and reference deltas, every size and the base cache
//! bounded), and the commit's files handed out ([`files`]), each name checked so none leaves
//! the checkout.
//!
//! **SHA-1 is git's object id here, never trust** ([`sha1`]). A git dependency is trusted by
//! the commit its lock file pins, fetched over TLS from its host; blaze's records use BLAKE3
//! and SHA-256.
//!
//! All of it is sans-IO: bytes in, bytes out.

#![no_std]
#![cfg_attr(test, feature(allocator_api))]
// `#![blaze::sans_io]` names blaze's tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

mod oid;
mod pack;
mod pkt;
mod plant;
mod protocol;
pub mod sha1;
pub mod store;
mod tree;

#[cfg(test)]
mod tests;

pub use oid::{Kind, Oid};
pub use pack::{CacheStats, DeltaWhy, Found, Index, Limits as PackLimits, PackError, PackWhy, PlantedIndex};
pub use plant::GitFetchPlant;
pub use pkt::{Lines, Pkt, PktWhy};
pub use protocol::{check_advertisement, fetch as fetch_request, fetched_pack, find_ref, ls_refs};
pub use tree::{File, Limits as TreeLimits, Mode, TreeError, TreeWhy, files};
