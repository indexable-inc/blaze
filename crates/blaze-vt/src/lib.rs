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
//! | [`dcs`] | the DCS handler: DECRQSS, XTGETTCAP, tmux control mode ([`dcs::tmux`]), capped buffer | built |
//! | [`terminfo`] | the terminfo entry XTGETTCAP answers from (ghostty's 268 capabilities, `TN`, `Co`, `RGB`) | built |
//! | [`reply`] | every reply's exact bytes and terminator, as encoders over plain state | built |
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

pub mod dcs;
pub mod parser;
pub mod reply;
pub mod style;
pub mod terminfo;
mod utf8;

#[cfg(test)]
mod parser_tests;

pub use parser::{Csi, Dcs, Esc, MAX_INTERMEDIATES, MAX_PARAMS, Parser, Sink, Terminator};
