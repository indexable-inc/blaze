//! Standard output and standard error (docs/v2/plan/design/std.md, `Out`): what a program
//! says to whoever started it. A program can write to either only if it was handed its
//! [`Out`]: [`Io::out`] for standard output, [`Io::err`] for standard error, two capabilities
//! of one type, so a function that takes `&Out` writes where its caller chose.
//!
//! **One way to say something.** [`Out::line`] writes a value and a line feed, [`Out::write`]
//! a value alone; the value is anything that is `Display`, so text and formatted text are the
//! same call and neither allocates:
//!
//! ```ignore
//! out.line(cx, "Hello, world!")?;
//! out.line(cx, format_args!("x = {x}, y = {y:?}"))?;
//! ```
//!
//! Each returns the [`OutError`] itself. [`Out::write_all`] is the same for bytes.
//! [`Out::with`] pairs the capability with a context as a `core::fmt::Write`, for code that
//! takes one and for `write!`/`writeln!`; `fmt::Error` cannot say why, so the writer keeps
//! the first [`OutError`] ([`Writer::failed`]). Prefer `line` and `write`.
//!
//! **All or an error.** Every call writes all of what it was given or says why not (std.md,
//! "The second rule": what may do less would return how much).
//!
//! **Buffering, chosen and said.** bstd holds nothing between calls. A `line` or `write`
//! formats into [`HELD`] bytes on the stack and hands them over as one write, so a line of up
//! to `HELD` bytes is one system call and cannot interleave with another write of this world;
//! a longer one is handed over `HELD` bytes at a time. The bytes are with the operating
//! system when the call returns: a program that crashes, is killed or exits leaves on the
//! screen every line whose call returned, and nothing is lost in a buffer. The price is one
//! system call a call; a loop of very many small writes that wants fewer builds its text in
//! an allocator it holds and writes it once. A [`Writer`] holds nothing either: each piece
//! `write!` hands it is one write.
//!
//! **Providers.** The simulation keeps what was written ([`crate::hosted::SimHost::out`],
//! [`crate::hosted::SimHost::err`]), for a test to read. A native world writes to the
//! process's streams through the one module that names Rust's `std` (`rt`): blaze-core has
//! no effect that writes to a handle the process was started with, so this write is not on
//! the driver, and once started it does not end at its context's deadline (a `cx` that
//! stopped already writes nothing). It moves onto the driver with that effect (std.md, row
//! S5e).

use alloc::string::String;
use core::fmt;

use crate::{Stopped, cx::Cx, io::Io};

/// The most bytes one `line` or `write` formats before handing them over: a call that says
/// no more than this is one write.
pub const HELD: usize = 1024;

/// Which of the process's streams an [`Out`] is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stream {
    /// Standard output.
    Out,
    /// Standard error.
    Err,
}

impl Stream {
    const fn name(self) -> &'static str {
        match self {
            Stream::Out => "standard output",
            Stream::Err => "standard error",
        }
    }
}

/// Why a write did not happen.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum OutWhy {
    /// The context stopped first: nothing more was written.
    Stopped(Stopped),
    /// The host refused the write, saying this (a closed pipe, a full disk behind a
    /// redirection).
    Host(String),
    /// The value's own `Display` returned an error: what it had written before is written.
    Format,
}

/// A write that failed: the stream, and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct OutError {
    pub stream: Stream,
    pub why: OutWhy,
}

impl fmt::Display for OutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let stream = self.stream.name();
        match &self.why {
            OutWhy::Stopped(stopped) => write!(f, "bstd::out write to {stream}: {stopped}"),
            OutWhy::Host(said) => write!(f, "bstd::out write to {stream}: the host refused: {said}"),
            OutWhy::Format => write!(f, "bstd::out write to {stream}: the value's `Display` failed"),
        }
    }
}

impl core::error::Error for OutError {}

/// One of the process's streams: standard output from [`Io::out`], standard error from
/// [`Io::err`].
#[derive(Clone)] // clone: copying authority held adds none
pub struct Out {
    io: Io,
    stream: Stream,
}

impl Out {
    pub(crate) fn new(io: Io, stream: Stream) -> Out {
        Out { io, stream }
    }

    /// Which stream this is.
    pub fn stream(&self) -> Stream {
        self.stream
    }

    /// `what` and a line feed: one write when they are at most [`HELD`] bytes.
    pub fn line(&self, cx: &Cx, what: impl fmt::Display) -> Result<(), OutError> {
        self.say(cx, format_args!("{what}\n"))
    }

    /// `what`, with nothing after it: one write when it is at most [`HELD`] bytes.
    pub fn write(&self, cx: &Cx, what: impl fmt::Display) -> Result<(), OutError> {
        self.say(cx, format_args!("{what}"))
    }

    /// Every byte of `bytes`, in order, as one write, or an error.
    pub fn write_all(&self, cx: &Cx, bytes: &[u8]) -> Result<(), OutError> {
        self.parts(cx, &[bytes])
    }

    /// This stream with `cx`, as a `core::fmt::Write`: what `write!` and `writeln!` take.
    /// Each piece handed to it is one write. Its `fmt::Error` says only that a write failed;
    /// [`Writer::failed`] has why.
    pub fn with<'a>(&'a self, cx: &'a Cx) -> Writer<'a> {
        Writer { out: self, cx, failed: None }
    }

    fn say(&self, cx: &Cx, args: fmt::Arguments<'_>) -> Result<(), OutError> {
        let mut held = Held { out: self, cx, bytes: [0; HELD], len: 0, failed: None };
        let formatted = fmt::Write::write_fmt(&mut held, args);
        held.flush();
        match (held.failed, formatted) {
            (Some(error), _) => Err(error),
            (None, Err(fmt::Error)) => Err(OutError { stream: self.stream, why: OutWhy::Format }),
            (None, Ok(())) => Ok(()),
        }
    }

    fn parts(&self, cx: &Cx, parts: &[&[u8]]) -> Result<(), OutError> {
        let error = |why| OutError { stream: self.stream, why };
        let cx = cx.span("bstd.out.write");
        cx.stopped_by(self.io.now()).map_err(|stopped| error(OutWhy::Stopped(stopped)))?;
        self.io.write_out(self.stream, parts).map_err(|said| error(OutWhy::Host(said)))
    }
}

impl fmt::Debug for Out {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Out({:?})", self.stream)
    }
}

/// What one `line` or `write` formats into: [`HELD`] bytes, handed over when full and when
/// the call ends. It never leaves its call, so nothing it holds can be lost.
struct Held<'a> {
    out: &'a Out,
    cx: &'a Cx,
    bytes: [u8; HELD],
    len: usize,
    failed: Option<OutError>,
}

impl Held<'_> {
    /// What is held, written; after a failure nothing more is.
    fn flush(&mut self) {
        if self.failed.is_none() && self.len > 0 {
            self.failed = self.out.parts(self.cx, &[&self.bytes[..self.len]]).err();
        }
        self.len = 0;
    }
}

impl fmt::Write for Held<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut rest = text.as_bytes();
        while !rest.is_empty() {
            if self.failed.is_some() {
                return Err(fmt::Error);
            }
            if self.len == HELD {
                self.flush();
                continue;
            }
            let take = rest.len().min(HELD - self.len);
            self.bytes[self.len..self.len + take].copy_from_slice(&rest[..take]);
            self.len += take;
            rest = &rest[take..];
        }
        Ok(())
    }
}

/// An [`Out`] with the context its writes are under: a `core::fmt::Write`, from
/// [`Out::with`]. It holds no bytes: each piece is written as it is handed over.
pub struct Writer<'a> {
    out: &'a Out,
    cx: &'a Cx,
    failed: Option<OutError>,
}

impl Writer<'_> {
    /// The first write of this writer that failed, with why; none when every write was made.
    /// After a failure the writer writes nothing more.
    pub fn failed(&self) -> Option<&OutError> {
        self.failed.as_ref()
    }

    /// `Ok` when every write was made, else the first failure.
    pub fn finish(self) -> Result<(), OutError> {
        match self.failed {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl fmt::Write for Writer<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.failed.is_some() {
            return Err(fmt::Error);
        }
        match self.out.parts(self.cx, &[text.as_bytes()]) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.failed = Some(error);
                Err(fmt::Error)
            }
        }
    }
}
