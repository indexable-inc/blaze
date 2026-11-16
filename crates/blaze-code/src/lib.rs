//! From checked bodies to a running program (docs/v2/plan/design/run.md, "End state").
//!
//! | module | what |
//! |---|---|
//! | [`a64`] | the AArch64 instructions a layout completes at a site (`adrp`, `ldr`, `add`, `bl`), in one home for the segment and blaze-export |
//! | [`record`] | one function instance's code record: position-independent code, a relro part, its targets by (space, symbol), its sites and words, its frame word and landing pads; sealed into one canonical form, encoded, read in place |
//! | [`unwind`] | the record's unwind rows: what the frame word and the landing pads mean, read in place and checked |
//! | [`eh`] | the bytes a platform's unwinder reads for one placed record: a CIE, its FDE and the LSDA, as the pinned library's personality routine parses them |
//! | [`place`] | one record placed alone in a running program on its first call: its sites completed against the run's slot words, its relro words filled (a function's address is its entry thunk) |
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

pub mod a64;
pub mod image;
pub mod place;
pub mod record;
pub mod unwind;
pub mod eh;

#[cfg(test)]
mod eh_tests;
#[cfg(test)]
mod place_tests;
#[cfg(test)]
mod tests;
