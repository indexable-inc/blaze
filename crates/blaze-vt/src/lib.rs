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
//! | [`page`] | 8-byte cells and rows in pages; the page list with byte-capped scrollback | built |
//! | [`screen`] | a screen: interned styles, out-of-line graphemes and links, cursor, reflowing resize | built |
//! | [`terminal`] | C0/C1, ESC, CSI and modes on two screens; replies as bytes, effects as data | built |
//! | [`plant`] | the planted bugs the simulation tests must catch | built |
//! | [`sgr`] | SGR parameters to attributes (colon groups on 4, 38, 48, 58), applied to a [`style::Style`] | built |
//! | [`color`] | colour specs (`rgb:`, `rgbi:`, `#…`, X11 names), the default and dynamic palettes, `rgb:` reply forms | built |
//! | [`osc`] | an OSC payload to a command (every OSC ghostty parses), and the bytes a colour query replies | built |
//! | [`dcs`] | the DCS handler: DECRQSS, XTGETTCAP, tmux control mode ([`dcs::tmux`]), capped buffer | built |
//! | [`terminfo`] | the terminfo entry XTGETTCAP answers from (ghostty's 268 capabilities, `TN`, `Co`, `RGB`) | built |
//! | [`reply`] | every reply's exact bytes and terminator, as encoders over plain state | built |
//! | [`kitty`] | APC: kitty graphics (parse, transmit, store, place, delete, reply, virtual placements) and the glyph protocol | built |
//! | [`input`] | key (legacy, modifyOtherKeys 2, kitty flags 1–16), mouse (9/1000/1002/1003; X10, 1005, 1006, 1015, 1016), paste (2004, unsafe bytes) and focus (1004) encoding into the caller's buffer | built |

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod color;
pub mod dcs;
pub mod input;
pub mod kitty;
pub mod osc;
pub mod page;
pub mod parser;
pub mod plant;
pub mod reply;
pub mod screen;
pub mod sgr;
pub mod style;
pub mod terminal;
pub mod terminfo;
mod utf8;

#[cfg(test)]
mod inventory_tests;
#[cfg(test)]
mod model;
#[cfg(test)]
mod parser_tests;
#[cfg(test)]
mod screen_tests;
#[cfg(test)]
mod sim_tests;
#[cfg(test)]
mod terminal_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod wire_tests;

pub use parser::{Csi, Dcs, Esc, MAX_INTERMEDIATES, MAX_PARAMS, Parser, Sink, Terminator};
