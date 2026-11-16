//! Standard output and standard error (docs/v2/plan/design/std.md, `Out`): what a program
//! says to whoever started it. A program can write to either only if it was handed its
//! [`Out`]: [`Io::out`] for standard output, [`Io::err`] for standard error, two capabilities
//! of one type, so a function that takes `&Out` writes where its caller chose.
//!
//! **One way to say something.** [`Out::line`] writes a value and a line feed, [`Out::write`]
//! a value alone; the value is anything that is a [`Show`] (blaze-text's human form, written
//! straight into the buffer), so text, numbers and formatted text are the same call and none
//! allocates. Text goes into the buffer as it is, with no formatter between: a hello world
//! runs no `core::fmt`. `format_args!` is a `Show` too (blaze-text's adapter, through
//! `core::fmt`), for what is not ported to a chain ([`Out::line_with`]) yet:
//!
//! ```ignore
//! out.line(cx, "Hello, world!")?;
//! out.line_with(cx, |o| o.str("x = ").int(x))?;
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

use blaze_text::Show;

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
    pub fn line(&self, cx: &Cx, what: impl Show) -> Result<(), OutError> {
        let mut held = Held::new(self, cx);
        what.show(&mut held);
        held.take(b"\n");
        held.end()
    }

    /// `what`, with nothing after it: one write when it is at most [`HELD`] bytes.
    pub fn write(&self, cx: &Cx, what: impl Show) -> Result<(), OutError> {
        let mut held = Held::new(self, cx);
        what.show(&mut held);
        held.end()
    }

    /// A chain of blaze-text writers and a line feed, written as [`Out::line`] writes: one
    /// write when at most [`HELD`] bytes. What `println!` becomes:
    /// `out.line_with(cx, |o| o.str("x = ").int(x))?`.
    pub fn line_with(&self, cx: &Cx, write: impl for<'o, 'h> FnOnce(&'o mut Held<'h>) -> &'o mut Held<'h>) -> Result<(), OutError> {
        self.held(cx, |held| {
            blaze_text::Put::put_byte(write(held), b'\n');
        })
    }

    /// A chain of blaze-text writers, with nothing after it: one write when at most [`HELD`]
    /// bytes. What `print!` becomes.
    pub fn write_with(&self, cx: &Cx, write: impl for<'o, 'h> FnOnce(&'o mut Held<'h>) -> &'o mut Held<'h>) -> Result<(), OutError> {
        self.held(cx, |held| {
            write(held);
        })
    }

    fn held(&self, cx: &Cx, fill: impl FnOnce(&mut Held<'_>)) -> Result<(), OutError> {
        let mut held = Held::new(self, cx);
        fill(&mut held);
        held.end()
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

    /// `parts`, in order, as one write, under `cx`'s span `bstd.out.write`. Written plainly
    /// (no closure, no `?` over a conversion): it is on every hello's path.
    fn parts(&self, cx: &Cx, parts: &[&[u8]]) -> Result<(), OutError> {
        let cx = cx.span("bstd.out.write");
        if let Err(stopped) = cx.stopped(&self.io) {
            return Err(OutError { stream: self.stream, why: OutWhy::Stopped(stopped) });
        }
        match self.io.write_out(self.stream, parts) {
            Ok(()) => Ok(()),
            Err(said) => Err(OutError { stream: self.stream, why: OutWhy::Host(said) }),
        }
    }

    /// Whether this stream is a terminal (`isatty` on its descriptor), read once, at the
    /// first ask, and kept: whether colours and frames are drawn for someone.
    pub fn is_terminal(&self) -> bool {
        matches!(self.io.tty(self.stream), crate::term::Tty::Terminal(_))
    }

    /// This stream's terminal size in cells (`TIOCGWINSZ`), read with [`Out::is_terminal`]:
    /// `None` when it is not a terminal or the terminal says no size.
    pub fn size(&self) -> Option<crate::term::Size> {
        match self.io.tty(self.stream) {
            crate::term::Tty::Terminal(size) => size,
            crate::term::Tty::NotTerminal => None,
        }
    }
}

impl fmt::Debug for Out {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Out({:?})", self.stream)
    }
}

/// What one `line` or `write` formats into: [`HELD`] bytes, handed over when full and when
/// the call ends. It never leaves its call, so nothing it holds can be lost. A
/// `blaze_text::Put`, so [`Out::line_with`]'s chain writes straight into it; after a failed
/// write it takes nothing more, and the call returns the failure.
pub struct Held<'a> {
    out: &'a Out,
    cx: &'a Cx,
    bytes: [u8; HELD],
    len: usize,
    failed: Option<OutError>,
}

impl<'a> Held<'a> {
    fn new(out: &'a Out, cx: &'a Cx) -> Held<'a> {
        Held { out, cx, bytes: [0; HELD], len: 0, failed: None }
    }

    /// The call's end: what is held, written; the first failure, or `Ok`.
    fn end(mut self) -> Result<(), OutError> {
        self.flush();
        match self.failed {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// What is held, written; after a failure nothing more is.
    ///
    /// Plain on purpose, as `take` is: matches and a byte loop, no `Option` or slice helpers,
    /// because this is on the hosted hello's path and every helper is one more function the
    /// new compiler must build (ladder README, "What the hosted hello executes").
    fn flush(&mut self) {
        if let None = self.failed
            && self.len > 0
        {
            if let Err(error) = self.out.parts(self.cx, &[&self.bytes[..self.len]]) {
                self.failed = Some(error);
            }
        }
        self.len = 0;
    }
}

impl Held<'_> {
    /// `bytes` after what is held, handed over each time `HELD` fill; `false` once a write
    /// failed.
    fn take(&mut self, bytes: &[u8]) -> bool {
        let mut from = 0;
        while from < bytes.len() {
            if let Some(_) = self.failed {
                return false;
            }
            if self.len == HELD {
                self.flush();
                continue;
            }
            let mut take = HELD - self.len;
            if bytes.len() - from < take {
                take = bytes.len() - from;
            }
            // A byte loop: built-in indexing calls no function, and an optimised build makes
            // it one copy.
            let mut at = 0;
            while at < take {
                self.bytes[self.len + at] = bytes[from + at];
                at += 1;
            }
            self.len += take;
            from += take;
        }
        match self.failed {
            Some(_) => false,
            None => true,
        }
    }
}

impl fmt::Write for Held<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        match self.take(text.as_bytes()) {
            true => Ok(()),
            false => Err(fmt::Error),
        }
    }
}

/// The invariant: a chain through `line_with` and `write_with` puts on the stream exactly what
/// `line` and `write` put for the same values through `format_args!`, for lines shorter and
/// longer than [`HELD`].
#[cfg(test)]
#[test]
fn chains_say_what_format_says() {
    use blaze_sim::{Broke, Config, Tape};
    use blaze_text::Writes as _;

    let written = |io: &Io| io.script(|host| host.out.clone()).expect("a simulated world");
    blaze_sim::check(Config::cases(200), |tape: &Tape| {
        let n = tape.next_u64("n");
        let x = tape.f64("x");
        let word = *tape.one_of(&["", "a", "naïve", "0123456789abcdef"], "word");
        let times = tape.below(3 * HELD as u64 / 16, "times");
        let long = word.repeat(usize::try_from(times).unwrap_or(0));
        let (chain, format) = (Io::sim(1), Io::sim(1));
        let (cx_chain, cx_format) = (chain.cx(core::time::Duration::from_secs(60), crate::mem::TASK_FRAME), format.cx(core::time::Duration::from_secs(60), crate::mem::TASK_FRAME));
        let (Ok(cx_chain), Ok(cx_format)) = (cx_chain, cx_format) else {
            return Err(Broke::Harness("no root frame".into()));
        };
        let (Some(out_chain), Some(out_format)) = (chain.out(), format.out()) else {
            return Err(Broke::Harness("Io::sim offers out".into()));
        };
        let said = out_chain
            .line_with(&cx_chain, |o| o.str("n = ").int(n).str(", x = ").float(x).str(" <").str(&long).byte(b'>'))
            .and_then(|()| out_chain.write_with(&cx_chain, |o| o.int(n).byte(b'|')));
        let formatted = out_format
            .line(&cx_format, format_args!("n = {n}, x = {x} <{long}>"))
            .and_then(|()| out_format.write(&cx_format, format_args!("{n}|")));
        if let (Err(error), _) | (_, Err(error)) = (said, formatted) {
            return Err(Broke::Harness(alloc::format!("a write failed: {error}")));
        }
        match written(&chain) == written(&format) {
            true => Ok(()),
            false => Err(Broke::Oracle(alloc::format!(
                "line_with wrote {:?}; line wrote {:?}",
                String::from_utf8_lossy(&written(&chain)),
                String::from_utf8_lossy(&written(&format))
            ))),
        }
    });
}

/// A failed write is kept in the `Held` and returned by the call that made it.
impl blaze_text::Put for Held<'_> {
    #[inline]
    fn put_str(&mut self, text: &str) {
        let _ = self.take(text.as_bytes());
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
