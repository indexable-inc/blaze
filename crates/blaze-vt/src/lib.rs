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
//! | [`sgr`] | SGR parameters to attributes (colon groups on 4, 38, 48, 58), applied to a [`style::Style`] | built |
//! | [`color`] | colour specs (`rgb:`, `rgbi:`, `#…`, X11 names), the default and dynamic palettes, `rgb:` reply forms | built |
//! | [`osc`] | an OSC payload to a command (every OSC ghostty parses), and the bytes a colour query replies | built |
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

pub mod color;
pub mod osc;
pub mod parser;
pub mod sgr;
pub mod style;
mod utf8;

#[cfg(test)]
mod parser_tests;

pub use parser::{Csi, Dcs, Esc, MAX_INTERMEDIATES, MAX_PARAMS, Parser, Sink, Terminator};
