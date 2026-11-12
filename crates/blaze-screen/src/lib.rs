//! What `blaze build`, `run`, `check` and `test` show (docs/terminal.md): one sans-IO core,
//! [`Screen`], that takes the build's events ([`Event`]: the daemon's model of the build as
//! it runs, units, lines that stay, the build's end and its why, test results), the injected
//! clock and the terminal's size, and returns [`Frame`]s as data: the lines appended to the
//! scrollback, the live region under them, the terminal's progress report, each span with
//! what it means ([`Role`]) and where it leads. [`encode`] is the one place a frame becomes
//! bytes, for one terminal's capabilities (`blaze_report::terminal::Terminal`); a driver writes them and does nothing
//! else. The daemon sends events, never drawn progress, so nothing about a terminal is an
//! input of any build key, and a transcript of the events ([`Stamped`], one a line as JSON)
//! replays into the frames the build showed.
//!
//! It is `no_std` on `core` and `alloc`. It is not on bstd (docs/v2/plan/design/std.md) yet:
//! bstd uses syntax only blaze accepts, and while cargo with the pinned nightly builds and
//! tests blaze in the dev loop (AGENTS.md, `blaze-builds-blaze`; decision D25) a crate that
//! must be verified now cannot depend on it. It moves to bstd when that rule returns to
//! blaze-only.

#![no_std]
#![forbid(unsafe_code)]
// `#![blaze::sans_io]` marks a sans-IO core for `blaze::sans_io_core`; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]

extern crate alloc;

mod encode;
mod event;
mod frame;
mod screen;
mod tally;
pub mod text;

pub use encode::encode;
pub use event::{BinaryEnd, Built, Command, Event, Link, Stamped, Test, Verdict, Why};
pub use frame::{Ended, Frame, Line, Mark, Progress, Role, Span};
pub use screen::{
    Caps, HANDFUL, OUTPUT_LINES, Options, PLAIN_EVERY_MS, Plant, QUIET_MS, SLOW_ITEM_MS,
    SLOW_TEST_MS, Screen, file_url, live, progress_fields, progress_line, unicode,
};
pub use tally::{Tally, tests_line};
