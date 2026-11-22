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

mod error;
mod path;
mod plant;
mod read;
mod write;

#[cfg(test)]
mod tests;

pub use error::{Error, Field, Name, PathWhy, Why};
pub use plant::TarPlant;
pub use read::{Entry, Event, Kind, Limits, Links, PlantedReader, Reader, Step};
pub use write::{Make, PlantedWriter, WriteError, WriteWhy, Writer};
