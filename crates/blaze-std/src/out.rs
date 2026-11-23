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
//! **All or an error.** Every call takes all of what it was given or says why not (std.md,
//! "The second rule": what may do less would return how much). A write the host refuses is
//! the error of the call that made it: the call whose bytes filled the buffer, a
//! [`Out::flush`], or the entry's flush at `main`'s end, which makes the run fail.
//!
//! **Buffering, chosen and said** (std.md, "what std makes easy": one buffer a command). A
//! `line` or `write` formats into [`HELD`] bytes on the stack, then appends them to its world's
//! buffer for the stream, [`BUFFER`] bytes held inline in the world (no allocation, and every
//! `Out` of one world shares it, so lines stay in the order they were said). The buffer is
//! written:
//! - when it is full, as one write of exactly `BUFFER` bytes (what does not fit waits for the
//!   next), so N bytes of short lines reach a pipe or a file in `ceil(N / BUFFER)` writes;
//! - at the end of each call when the stream is a terminal ([`Out::is_terminal`]), so what a
//!   person watches, a prompt included, is on the screen when the call returns;
//! - at [`Out::flush`];
//! - when `main` ends, on every path the entry sees: a return, an error, an unwinding panic
//!   (`entry::run_in`), and [`crate::host::Host::exit`]. A panic that aborts, a
//!   process killed by a signal, and the runtime's own fatal stops (`rt::fatal`, `rt::hang`)
//!   lose what the buffer held;
//! - before a raw write of the world (`Io::write_out`) and before a test reads the simulated
//!   host ([`Io::script`]), so neither sees bytes out of order.
//!
//! A piece of [`HELD`] bytes or more is not copied into the stack block: it goes to the
//! world's buffer as it is, and one of [`BUFFER`] bytes or more is written with what the buffer
//! holds in one vectored write, with no copy at all. A [`Writer`] appends each piece `write!`
//! hands it, and on a terminal writes it.
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

/// The most bytes one `line` or `write` formats on the stack before appending them to the
/// world's buffer: a call that says no more than this is appended whole, at once.
pub const HELD: usize = 1024;

/// The bytes a world holds for each of its two streams before writing them: every write to a
/// pipe or a file but a flush's is exactly this long. Eight KiB: std's `BufWriter`'s size, and
/// two pages of a pipe's default 64 KiB on Linux, so a reader is woken per eight KiB, not per
/// line.
pub const BUFFER: usize = 8 * 1024;

/// One stream's buffer, inline in its world ([`Io`]'s): the bytes said and not yet written.
/// A sans-IO core: [`Pending::put`] and [`Pending::flush`] say what to write through the
/// `write` they are handed and hold the rest. After a refused write what it held is dropped
/// (the host had it and said no), so a refusal is said once.
pub(crate) struct Pending {
    bytes: [u8; BUFFER],
    len: usize,
}

impl Pending {
    pub(crate) const fn new() -> Pending {
        Pending { bytes: [0; BUFFER], len: 0 }
    }

    /// How many bytes it holds.
    pub(crate) fn held(&self) -> usize {
        self.len
    }

    /// `more` after what it holds. What fits is copied in. What does not fit fills the
    /// buffer to its end, the full buffer is one write and the rest is held; `more` too large
    /// for that (as long as the buffer or longer once it is topped up) is written with what is
    /// held in one write of two parts, not copied.
    ///
    /// Plain on purpose, as `Held::take` is: byte loops and built-in indexing, on every hosted
    /// hello's path.
    pub(crate) fn put<E>(&mut self, more: &[u8], write: &mut impl FnMut(&[&[u8]]) -> Result<(), E>) -> Result<(), E> {
        let room = BUFFER - self.len;
        if more.len() <= room {
            let mut at = 0;
            while at < more.len() {
                self.bytes[self.len + at] = more[at];
                at += 1;
            }
            self.len += more.len();
            return Ok(());
        }
        if more.len() - room < BUFFER {
            let mut at = 0;
            while at < room {
                self.bytes[self.len + at] = more[at];
                at += 1;
            }
            self.len = 0;
            write(&[&self.bytes[..]])?;
            let rest = more.len() - room;
            let mut at = 0;
            while at < rest {
                self.bytes[at] = more[room + at];
                at += 1;
            }
            self.len = rest;
            return Ok(());
        }
        let held = self.len;
        self.len = 0;
        write(&[&self.bytes[..held], more])
    }

    /// What it holds, as one write; nothing when it holds nothing.
    pub(crate) fn flush<E>(&mut self, write: &mut impl FnMut(&[&[u8]]) -> Result<(), E>) -> Result<(), E> {
        if self.len == 0 {
            return Ok(());
        }
        let held = self.len;
        self.len = 0;
        write(&[&self.bytes[..held]])
    }
}

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
    /// A custom [`OutSink`] refused the write, saying this.
    Sink(&'static str),
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
            OutWhy::Sink(said) => write!(f, "bstd::out write to {stream}: {said}"),
        }
    }
}

impl core::error::Error for OutError {}

/// A custom output sink backing an [`Out`] capability (e.g. a resident daemon streaming
/// `ServerFrame::Out` / `ServerFrame::Err` over a client Unix socket while faithfully reporting
/// the client's terminal capabilities).
pub trait OutSink: Send + Sync {
    /// Writes one chunk of bytes to the sink; `Err(why)` fails the caller's [`Out`] write.
    fn write_chunk(&self, cx: &Cx, bytes: &[u8]) -> Result<(), &'static str>;
}

/// Type-erased shared handle to a custom [`OutSink`].
pub type SharedSink = blaze_own::Arc<dyn OutSink, blaze_own::SharedAlloc<'static>>;

#[derive(Clone)]
enum Target {
    World(Io),
    Sink { tty: crate::term::Tty, sink: SharedSink },
}

/// One of the process's streams: standard output from [`Io::out`], standard error from
/// [`Io::err`].
#[derive(Clone)] // clone: copying authority held adds none
pub struct Out {
    target: Target,
    stream: Stream,
}

impl Out {
    pub(crate) fn new(io: Io, stream: Stream) -> Out {
        Out { target: Target::World(io), stream }
    }

    /// Creates an [`Out`] backed by a custom [`OutSink`] and terminal description `tty`.
    pub fn with_sink_in<S: OutSink + 'static>(
        stream: Stream,
        tty: crate::term::Tty,
        sink: S,
        alloc: blaze_own::SharedAlloc<'static>,
    ) -> Out {
        let shared: SharedSink = blaze_own::Arc::new_in(sink, alloc);
        Out { target: Target::Sink { tty, sink: shared }, stream }
    }

    /// Which stream this is.
    pub fn stream(&self) -> Stream {
        self.stream
    }

    /// `what` and a line feed, appended to the stream's buffer at once when they are at most
    /// [`HELD`] bytes (the module's doc says when the buffer is written).
    pub fn line(&self, cx: &Cx, what: impl Show) -> Result<(), OutError> {
        let mut held = Held::new(self, cx);
        what.show(&mut held);
        held.take(b"\n");
        held.end()
    }

    /// `what`, with nothing after it, appended as [`Out::line`] appends.
    pub fn write(&self, cx: &Cx, what: impl Show) -> Result<(), OutError> {
        let mut held = Held::new(self, cx);
        what.show(&mut held);
        held.end()
    }

    /// A chain of blaze-text writers and a line feed, appended as [`Out::line`] appends. What
    /// `println!` becomes:
    /// `out.line_with(cx, |o| o.str("x = ").int(x))?`.
    pub fn line_with(&self, cx: &Cx, write: impl for<'o, 'h> FnOnce(&'o mut Held<'h>) -> &'o mut Held<'h>) -> Result<(), OutError> {
        self.held(cx, |held| {
            blaze_text::Put::put_byte(write(held), b'\n');
        })
    }

    /// A chain of blaze-text writers, with nothing after it, appended as [`Out::line`]
    /// appends. What `print!` becomes.
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

    /// Every byte of `bytes`, in order, appended to the stream's buffer (written through when
    /// they are [`BUFFER`] bytes or more), or an error.
    pub fn write_all(&self, cx: &Cx, bytes: &[u8]) -> Result<(), OutError> {
        self.hand(cx, bytes, true)
    }

    /// What the stream's buffer holds, written now, as one write: for a program that must
    /// have its output out before it waits on something else (another process reading a pipe
    /// it shares). Nothing when the buffer is empty.
    pub fn flush(&self, cx: &Cx) -> Result<(), OutError> {
        match &self.target {
            Target::World(io) => {
                if let Err(stopped) = cx.stopped(io) {
                    return Err(OutError { stream: self.stream, why: OutWhy::Stopped(stopped) });
                }
                match io.flush_out(self.stream, cx.name()) {
                    Ok(()) => Ok(()),
                    Err(said) => Err(OutError { stream: self.stream, why: OutWhy::Host(said) }),
                }
            }
            Target::Sink { .. } => Ok(()),
        }
    }

    /// This stream with `cx`, as a `core::fmt::Write`: what `write!` and `writeln!` take.
    /// Each piece handed to it is appended as [`Out::write_all`] appends. Its `fmt::Error`
    /// says only that a write failed; [`Writer::failed`] has why.
    pub fn with<'a>(&'a self, cx: &'a Cx) -> Writer<'a> {
        Writer { out: self, cx, failed: None }
    }

    /// `bytes` appended to the stream's buffer under `cx`; `end` when they are the last of a
    /// call, which on a terminal writes the buffer. A write the append makes is recorded as
    /// the span `bstd.out.write` inside `cx`'s, and only then (an append records nothing).
    /// Written plainly (no closure, no `?` over a conversion): it is on every hello's path.
    fn hand(&self, cx: &Cx, bytes: &[u8], end: bool) -> Result<(), OutError> {
        match &self.target {
            Target::World(io) => {
                if let Err(stopped) = cx.stopped(io) {
                    return Err(OutError { stream: self.stream, why: OutWhy::Stopped(stopped) });
                }
                match io.put_out(self.stream, cx.name(), bytes, end) {
                    Ok(()) => Ok(()),
                    Err(said) => Err(OutError { stream: self.stream, why: OutWhy::Host(said) }),
                }
            }
            Target::Sink { sink, .. } => {
                if bytes.is_empty() {
                    return Ok(());
                }
                match sink.write_chunk(cx, bytes) {
                    Ok(()) => Ok(()),
                    Err(said) => Err(OutError { stream: self.stream, why: OutWhy::Sink(said) }),
                }
            }
        }
    }

    /// Whether this stream is a terminal (`isatty` on its descriptor), read once, at the
    /// first ask, and kept: whether colours and frames are drawn for someone.
    pub fn is_terminal(&self) -> bool {
        let tty = match &self.target {
            Target::World(io) => io.tty(self.stream),
            Target::Sink { tty, .. } => *tty,
        };
        matches!(tty, crate::term::Tty::Terminal(_))
    }

    /// This stream's terminal size in cells (`TIOCGWINSZ`), read with [`Out::is_terminal`]:
    /// `None` when it is not a terminal or the terminal says no size.
    pub fn size(&self) -> Option<crate::term::Size> {
        let tty = match &self.target {
            Target::World(io) => io.tty(self.stream),
            Target::Sink { tty, .. } => *tty,
        };
        match tty {
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

/// What one `line` or `write` formats into: [`HELD`] bytes on the stack, appended to the
/// world's buffer when they would overflow and when the call ends. A `blaze_text::Put`, so
/// [`Out::line_with`]'s chain writes straight into it; after a failed write it takes nothing
/// more, and the call returns the failure. A call that panics part way appends nothing it
/// still held.
pub struct Held<'a> {
    out: &'a Out,
    cx: &'a Cx,
    bytes: [u8; HELD],
    len: usize,
    /// Whether this call appended anything yet: its end then has a buffer to write on a
    /// terminal even when it holds nothing more.
    handed: bool,
    failed: Option<OutError>,
}

impl<'a> Held<'a> {
    fn new(out: &'a Out, cx: &'a Cx) -> Held<'a> {
        Held { out, cx, bytes: [0; HELD], len: 0, handed: false, failed: None }
    }

    /// The call's end: what is held, appended as the call's last bytes; the first failure,
    /// or `Ok`.
    fn end(mut self) -> Result<(), OutError> {
        self.hand(true);
        match self.failed {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// What is held, appended to the world's buffer (`end`: as the call's last bytes); after
    /// a failure nothing more is. A call that said nothing hands nothing over.
    ///
    /// Plain on purpose, as `take` is: matches and a byte loop, no `Option` or slice helpers,
    /// because this is on the hosted hello's path and every helper is one more function the
    /// new compiler must build (ladder README, "What the hosted hello executes").
    fn hand(&mut self, end: bool) {
        if let None = self.failed
            && (self.len > 0 || (end && self.handed))
        {
            if let Err(error) = self.out.hand(self.cx, &self.bytes[..self.len], end) {
                self.failed = Some(error);
            }
            self.handed = true;
        }
        self.len = 0;
    }
}

impl Held<'_> {
    /// `bytes` after what is held; `false` once a write failed. What would overflow the
    /// stack block first appends what it holds; `bytes` of [`HELD`] or more are then appended
    /// as they are, not copied here.
    fn take(&mut self, bytes: &[u8]) -> bool {
        if let Some(_) = self.failed {
            return false;
        }
        if bytes.len() > HELD - self.len {
            self.hand(false);
            if let Some(_) = self.failed {
                return false;
            }
            if bytes.len() >= HELD {
                if let Err(error) = self.out.hand(self.cx, bytes, false) {
                    self.failed = Some(error);
                    return false;
                }
                self.handed = true;
                return true;
            }
        }
        // A byte loop: built-in indexing calls no function, and an optimised build makes it
        // one copy.
        let mut at = 0;
        while at < bytes.len() {
            self.bytes[self.len + at] = bytes[at];
            at += 1;
        }
        self.len += bytes.len();
        true
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

/// Invariant: a chain through `line_with` and `write_with` puts on the stream exactly what
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
/// [`Out::with`]. It holds no bytes of its own: each piece is appended to the world's buffer
/// as it is handed over (and on a terminal written).
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
        match self.out.hand(self.cx, text.as_bytes(), true) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.failed = Some(error);
                Err(fmt::Error)
            }
        }
    }
}
