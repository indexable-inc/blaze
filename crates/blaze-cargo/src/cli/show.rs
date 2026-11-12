//! The driver of the screen (docs/terminal.md): what `blaze build`, `run`, `check` and `test`
//! write to stderr goes through blaze-screen's one core, and this is the IO around it. It
//! reads the clock and the terminal's size, hands the core each event with them, and writes
//! the bytes of the frame the core hands back ([`blaze_screen::encode`]). It decides nothing:
//! what a frame holds, when one is due, how many rows are erased and whether anything is
//! drawn at all are the core's.
//!
//! The terminal is put back on every way out: a [`Show`] dropped with a region on screen
//! erases it, shows the cursor and clears the terminal's progress report, a panic's unwind
//! included; an interrupt (SIGINT, SIGTERM) does the same from [`watch_interrupts`]'s thread,
//! which knows the region's rows from one atomic, then ends the command as a shell's
//! interrupted one ends.
//!
//! `BLAZE_UI_TRANSCRIPT=<file>` records the event stream as it came, one event a line as
//! JSON with its time ([`blaze_screen::Stamped`]): what the tests of blaze-screen replay.

use std::{
    io::{self, Write},
    sync::atomic::{AtomicU16, Ordering},
    time::Instant,
};

use blaze_report::terminal::Terminal;
use blaze_screen::{Caps, Event, Frame, Options, Paint, Screen, Stamped};

/// What takes the screen's events: the driver itself, or the channel to the thread that owns
/// it (`blaze test`'s live run, whose test executables speak from another thread).
pub trait Sink {
    fn take(&mut self, event: Event);
    /// The frame that is due, written.
    fn draw(&mut self) -> io::Result<()>;
}

/// The rows of the live region on the terminal now: what an interrupt erases.
static REGION_ROWS: AtomicU16 = AtomicU16::new(0);

/// The screen of one command and the stream it is written to.
pub struct Show<E: Write> {
    screen: Screen,
    paint: Paint,
    err: E,
    /// One frame's bytes, reused.
    bytes: String,
    /// The clock: milliseconds since the command began.
    clock: Box<dyn FnMut() -> u64 + Send>,
    /// The terminal's columns and rows now; none off a terminal.
    size: fn() -> Option<(u16, u16)>,
    transcript: Option<io::BufWriter<std::fs::File>>,
    /// Rows of the region this wrote last.
    rows: u16,
}

/// What stderr is for a region: its capabilities as the core takes them.
pub fn caps(terminal: bool, var: impl Fn(&str) -> Option<String> + Copy) -> Caps {
    let live = blaze_screen::live(var, terminal);
    match (live, stderr_size()) {
        (true, Some((cols, rows))) => Caps { live, cols, rows, unicode: blaze_screen::unicode(var) },
        _ => Caps::PIPE,
    }
}

/// The columns and rows of the terminal stderr is.
#[cfg(unix)]
pub fn stderr_size() -> Option<(u16, u16)> {
    crate::raw::system::stderr_size()
}

/// blaze's system calls ask a terminal's size of Unix's `TIOCGWINSZ` only: on Windows no
/// region is drawn, and the lines are a pipe's.
#[cfg(not(unix))]
pub fn stderr_size() -> Option<(u16, u16)> {
    None
}

impl<E: Write> Show<E> {
    /// The screen of a command that began at `started`, on `err`.
    pub fn new(options: Options, caps: Caps, terminal: Terminal, started: Instant, err: E) -> Show<E> {
        let clock = move || u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut show = Show::with_clock(options, caps, terminal, Box::new(clock), err); // alloc: once a command
        if caps.live {
            show.size = stderr_size;
        }
        show.transcript = std::env::var_os("BLAZE_UI_TRANSCRIPT")
            .and_then(|path| std::fs::File::create(path).ok())
            .map(io::BufWriter::new);
        show
    }

    /// The same on an injected clock and a terminal that never changes size: what a test
    /// drives.
    pub fn with_clock(
        options: Options,
        caps: Caps,
        terminal: Terminal,
        clock: Box<dyn FnMut() -> u64 + Send>,
        err: E,
    ) -> Show<E> {
        let mut terminal = terminal;
        if !caps.live {
            // Off a terminal no sequence but a colour asked for by name is written.
            terminal = Terminal { caps: blaze_report::Caps { links: false, ..terminal.caps }, ..Terminal::PIPE };
        }
        Show {
            screen: Screen::new(options, caps),
            paint: Paint { live: caps.live, terminal },
            err,
            bytes: String::with_capacity(4096), // alloc: once a command, a frame's bytes
            clock,
            size: || None,
            transcript: None,
            rows: 0,
        }
    }

    /// A pipe's screen with a clock that stands still: what a test of the printer reads.
    pub fn piped(options: Options, err: E) -> Show<E> {
        Show::with_clock(options, Caps::PIPE, Terminal::PIPE, Box::new(|| 0), err) // alloc: a test's
    }

    pub fn paint(&self) -> Paint {
        self.paint
    }

    /// Whether an error was said or a test failed.
    pub fn failing(&self) -> bool {
        self.screen.failing()
    }

    /// When a frame is due though no event comes, in milliseconds since the command began.
    pub fn next_deadline(&self) -> Option<u64> {
        self.screen.next_deadline()
    }

    /// The stream, for what is written once nothing live is on it.
    pub fn stream(&mut self) -> &mut E {
        &mut self.err
    }

    fn record(&mut self, at_ms: u64, event: &Event) {
        if let Some(transcript) = &mut self.transcript {
            // clone: only while a transcript is recorded
            let stamped = Stamped { at_ms, event: event.clone() };
            if let Ok(line) = serde_json::to_string(&stamped) {
                let _ = writeln!(transcript, "{line}");
            }
        }
    }

    fn write(&mut self, frame: &Frame) -> io::Result<()> {
        self.bytes.clear();
        blaze_screen::encode(frame, &self.paint, &mut self.bytes);
        self.rows = frame.region.len() as u16;
        REGION_ROWS.store(self.rows, Ordering::Release);
        if self.bytes.is_empty() {
            return Ok(());
        }
        self.err.write_all(self.bytes.as_bytes())?;
        self.err.flush()
    }
}

impl<E: Write> Sink for Show<E> {
    fn take(&mut self, event: Event) {
        let at = (self.clock)();
        self.record(at, &event);
        self.screen.advance_clock(at);
        self.screen.take(event);
    }

    fn draw(&mut self) -> io::Result<()> {
        if let Some((cols, rows)) = (self.size)() {
            let caps = self.screen.caps();
            if (caps.cols, caps.rows) != (cols, rows) {
                self.take(Event::Resized { cols, rows });
            }
        }
        let at = (self.clock)();
        self.screen.advance_clock(at);
        match self.screen.frame() {
            Some(frame) => self.write(&frame),
            None => Ok(()),
        }
    }
}

impl<E: Write> Drop for Show<E> {
    fn drop(&mut self) {
        if let Some(transcript) = &mut self.transcript {
            let _ = transcript.flush();
        }
        if self.rows > 0 {
            let _ = self.write(&Frame::restore(self.rows));
        }
    }
}

/// From here an interrupt (SIGINT, SIGTERM) puts the terminal back before the command ends:
/// a thread waits for the signal, erases the region's rows, shows the cursor, clears the
/// terminal's progress report, and exits as an interrupted command does. Only for a screen
/// that redraws; a pipe's has nothing to put back, and its command dies of the signal as
/// before.
#[cfg(unix)]
pub fn watch_interrupts(paint: Paint) -> io::Result<()> {
    use std::io::Read as _;
    crate::serve::signals::catch();
    let mut pipe = crate::serve::signals::stop_pipe()?;
    std::thread::Builder::new().name("blaze screen interrupt".into()).spawn(move || {
        let mut byte = [0u8; 1];
        let got = {
            let _waiting = tracing::info_span!(
                target: "blaze",
                "screen: wait for an interrupt (SIGINT or SIGTERM) to put the terminal back"
            )
            .entered();
            pipe.read(&mut byte)
        };
        if got.is_err() {
            return;
        }
        interrupted(paint);
    })?;
    Ok(())
}

#[cfg(not(unix))]
pub fn watch_interrupts(_paint: Paint) -> io::Result<()> {
    Ok(())
}

/// The terminal put back, then the command's end as an interrupted one.
pub fn interrupted(paint: Paint) -> ! {
    let mut bytes = String::new(); // alloc: once, as the command ends
    let rows = REGION_ROWS.swap(0, Ordering::AcqRel);
    blaze_screen::encode(&Frame::restore(rows), &paint, &mut bytes);
    let mut err = io::stderr();
    let _ = err.write_all(bytes.as_bytes());
    let _ = err.write_all(b"interrupted\n");
    let _ = err.flush();
    std::process::exit(blaze_core::exit::INTERRUPTED);
}

/// A writer of whole lines that stay: what a caller with its own words (an error, a test
/// executable's output) writes through, so they come above the region and in order.
pub struct Stay<'a, S: Sink> {
    sink: &'a mut S,
    held: Vec<u8>,
}

impl<'a, S: Sink> Stay<'a, S> {
    pub fn new(sink: &'a mut S) -> Stay<'a, S> {
        Stay { sink, held: Vec::new() } // alloc: none until something is written
    }

    /// The whole lines held, said.
    fn say(&mut self, all: bool) -> io::Result<()> {
        let end = match all {
            true => self.held.len(),
            false => self.held.iter().rposition(|&b| b == b'\n').map_or(0, |at| at + 1),
        };
        if end == 0 {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&self.held[..end]).into_owned(); // alloc: a burst of lines
        self.held.drain(..end);
        self.sink.take(Event::Said(text));
        self.sink.draw()
    }
}

impl<S: Sink> Write for Stay<'_, S> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.held.extend_from_slice(bytes);
        self.say(false)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.say(false)
    }
}

impl<S: Sink> Drop for Stay<'_, S> {
    fn drop(&mut self) {
        let _ = self.say(true);
    }
}
