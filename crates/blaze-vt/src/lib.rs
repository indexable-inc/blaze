//! `bstd::vt`: a terminal emulator core, sans-IO (docs/v2/plan/design/vt.md).
//!
//! Bytes from a PTY go in; a screen, the replies the program asked for and the effects the host
//! acts on come out as data. Nothing here reads a PTY, a clock or a file. The bar is ghostty's
//! terminal core: every row of docs/v2/plan/design/vt-inventory.md is accepted with ghostty's
//! effect, and each has a test.
//!
//! | module | what | state |
//! |---|---|---|
//! | [`parser`] | the vt100.net state machine as ghostty runs it, UTF-8 in ground, events to a [`parser::Sink`] | built |
//! | `utf8` | the decoder: one code point per call, U+FFFD for each maximal invalid subpart | built |
//! | [`style`] | a cell's style: colours by source (unset, palette, RGB), underline styles, flags | built |
//! | [`input`] | key (legacy, modifyOtherKeys 2, kitty flags 1–16), mouse (9/1000/1002/1003; X10, 1005, 1006, 1015, 1016), paste (2004, unsafe bytes) and focus (1004) encoding into the caller's buffer | built |
//!
//! The screen, terminal, SGR, OSC, DCS, kitty and input modules are written in lanes against
//! these interfaces (vt.md, "Modules").

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod input;
pub mod parser;
pub mod style;
mod utf8;

#[cfg(test)]
mod parser_tests;

pub use parser::{Csi, Dcs, Esc, MAX_INTERMEDIATES, MAX_PARAMS, Parser, Sink, Terminator};
