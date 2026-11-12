//! Standard output (docs/v2/plan/design/std.md, `Out`): the bytes a program says to whoever
//! started it. A program can write there only if it was handed an [`Out`], from [`Io::out`].
//! Standard error is not a capability (std.md, "Decided"): the entry writes a failed `main`'s
//! error there itself ([`crate::entry`]), and a program has no call for it.
//!
//! **All or an error.** [`Out::write_all`] and [`Out::line`] write every byte or say why not
//! (std.md, "The second rule": what may do less would return how much).
//!
//! **Providers.** The simulation keeps what was written ([`crate::hosted::SimHost::out`]), for
//! a test to read. A native world writes to the process's standard output through the one
//! module that names Rust's `std` (`rt`): blaze-core has no effect that writes to a handle the
//! process was started with, so this write is not on the driver, and once started it does not
//! end at its context's deadline (a `cx` that stopped already writes nothing). It moves onto
//! the driver with that effect (std.md, row S5e).

use alloc::string::String;
use core::fmt;

use crate::{Stopped, cx::Cx, io::Io};

/// Why a write to standard output did not happen.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum OutWhy {
    /// The context stopped first: nothing was written.
    Stopped(Stopped),
    /// The host refused the write, saying this (a closed pipe, a full disk behind a
    /// redirection).
    Host(String),
}

/// A write to standard output that failed: the stream is the thing, and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct OutError {
    pub why: OutWhy,
}

impl fmt::Display for OutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.why {
            OutWhy::Stopped(stopped) => write!(f, "bstd::out write to standard output: {stopped}"),
            OutWhy::Host(said) => write!(f, "bstd::out write to standard output: the host refused: {said}"),
        }
    }
}

impl core::error::Error for OutError {}

/// Standard output: from [`Io::out`].
#[derive(Clone)] // clone: copying authority held adds none
pub struct Out {
    io: Io,
}

impl Out {
    pub(crate) fn new(io: Io) -> Out {
        Out { io }
    }

    /// Every byte of `bytes`, in order, or an error.
    pub fn write_all(&self, cx: &Cx, bytes: &[u8]) -> Result<(), OutError> {
        self.parts(cx, &[bytes])
    }

    /// `text` and a line feed, as one write: no other write of this world lands between them.
    pub fn line(&self, cx: &Cx, text: &str) -> Result<(), OutError> {
        self.parts(cx, &[text.as_bytes(), b"\n"])
    }

    fn parts(&self, cx: &Cx, parts: &[&[u8]]) -> Result<(), OutError> {
        let cx = cx.span("bstd.out.write");
        cx.stopped_by(self.io.now()).map_err(|stopped| OutError { why: OutWhy::Stopped(stopped) })?;
        self.io.write_out(parts).map_err(|said| OutError { why: OutWhy::Host(said) })
    }
}

impl fmt::Debug for Out {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Out")
    }
}
