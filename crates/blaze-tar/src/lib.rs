//! blaze's own tar, in place of the `tar` crate: a reader of the archives crates.io's `.crate`
//! files are (ustar and GNU headers, pax `path`/`linkpath`/`size`, GNU `L`/`K` long names) that
//! refuses whatever would escape its destination, and a deterministic writer. `no_std`, no IO:
//! bytes in, events out ([`Reader`]); entries in, bytes out ([`Writer`]).
//!
//! The reader checks the format and the paths; what to do with an entry (which names to keep,
//! case folding, modes, times, writing files) is its caller's, as in blaze-fetch's `unpack`. A
//! `.crate` file is a gzip member: `blaze_deflate::GzipDecoder` feeds this reader.

#![no_std]
#![cfg_attr(test, feature(allocator_api))]
// `#![blaze::sans_io]` in the modules names blaze's tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

#[cfg(test)]
extern crate std;

mod error;
mod path;
mod plant;
mod read;
mod write;

#[cfg(test)]
mod tests;

pub use error::{Error, Field, Name, PathWhy, Why};
pub use read::{Entry, Event, Kind, Limits, Links, Reader, Step};
pub use write::{Make, WriteError, WriteWhy, Writer};
