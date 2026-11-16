//! From checked bodies to a running program (docs/v2/plan/design/run.md, "End state").
//!
//! | module | what |
//! |---|---|
//! | [`record`] | one function instance's code record: position-independent code, a relro part, its targets by (space, symbol), its sites and words, its frame word and landing pads; sealed into one canonical form, encoded, read in place |
//! | [`image`] | the segment (records placed together, mapped read-only and executable in place) and the program image (every segment's import words and relro words bound at fixed addresses; an edit appends a segment and rewrites only the words naming what moved) |
//!
//! Everything here is sans-IO: bytes in, bytes and writes out. Nothing maps memory, opens a file
//! or calls `dlsym`: the launcher does, from what [`image::Appended`] says.

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

pub mod image;
pub mod record;

#[cfg(test)]
mod tests;
