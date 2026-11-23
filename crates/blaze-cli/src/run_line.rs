//! `blaze run`'s live status line (docs/terminal.md, "While `run` works"): blaze-screen's
//! sans-IO [`RunStatus`] fed the run's events (it is the [`Watch`] the run is handed) and the
//! command's clock, its frames written to the error stream. Only here are bytes of the line
//! written; the run's parent, its loop and the request say what happens and never draw.
//!
//! Two threads, one lock. The command's own thread tells each event as it happens, and writes
//! at once a frame the event makes due: so the erase that [`Step::Started`], [`Step::Answers`]
//! and [`Step::Says`] call for is on the terminal before the program starts, before the answer
//! frees it, before the command's own line. A ticker thread, started for the command and
//! joined before it returns, writes the frames time makes due (the first after
//! [`blaze_screen::RUN_QUIET_MS`], the timer's tenths, a need waited on), and nothing else; it
//! sleeps on a named signal until the core's next time or an event that moves it earlier, and
//! at most [`IDLE`] (rule 18). Every write is made holding the lock, so two frames never mix.
//!
//! Off a terminal (a pipe, a file, `CI`, `TERM=dumb`, `BLAZE_UI=plain`: blaze-screen `live`)
//! the run is handed [`Unwatched`] and no thread is started.

use core::fmt;
use core::time::Duration;

use blaze_core::lock::{Lock, Signal};
use blaze_report::frame::Choice;
use blaze_report::frame::plain::View;
use blaze_report::terminal::{self, Color, Config, Terminal};
use blaze_screen::{Caps, Command, LineBuf, RunStatus, RunStatusOpts, StatusFrame, Step, Unwatched, Watch, encode_status};
use bstd::clock::{Clock, Instant};
use bstd::cx::CxSpawn;
use bstd::env::Env;
use bstd::out::{Out, OutError};
use bstd::{Cx, Stopped};

/// The longest the ticker sleeps with nothing due; it reads the terminal's width when it wakes.
const IDLE: Duration = Duration::from_secs(1);
/// The ticker's own context's scratch: it allocates nothing.
const TICKER_SCRATCH: usize = 1 << 16;
/// The widest line drawn: a multi-line frame of it fits [`FRAME_BYTES`].
const MOST_COLS: u16 = 512;
const FRAME_BYTES: usize = 16384;
/// Bounded wait slice when `ProgressSink::see` waits for the ticker to flush an erase.
const ERASE_WAIT: Duration = Duration::from_millis(50);

/// Why the live line failed: said when the command ends (the run itself went on).
#[derive(Debug)]
pub(crate) enum RunLineError {
    /// The ticker's thread did not start.
    Ticker { kind: std::io::ErrorKind },
    /// The ticker's context was refused.
    Context(Stopped),
    /// A frame did not fit [`FRAME_BYTES`] (the line is cut to [`MOST_COLS`]: blaze's bug).
    Encode,
    /// A frame's write to the error stream failed.
    Out(OutError),
}

impl fmt::Display for RunLineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunLineError::Ticker { kind } => write!(f, "blaze run: the live status line's thread did not start: {kind}"),
            RunLineError::Context(why) => write!(f, "blaze run: the live status line's thread got no context: {why}"),
            RunLineError::Encode => write!(f, "blaze run: a frame of the live status line did not fit its {FRAME_BYTES} bytes (run_line.rs)"),
            RunLineError::Out(why) => write!(f, "blaze run: the live status line could not be written: {why}"),
        }
    }
}

impl core::error::Error for RunLineError {}

/// What the inline status is drawn on, read at the command's edge.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LineOpts {
    /// The error stream is a terminal a live status block can be redrawn on (blaze-screen `live`),
    /// and neither `--view json`/`agent` nor `--status-every` suppressed it.
    pub live: bool,
    pub caps: Caps,
    pub terminal: Terminal,
    pub command: Command,
}

impl LineOpts {
    /// From `env`'s environment variables, whether `err` is a terminal and its size, `--color`,
    /// which command runs, `--view`, and whether `--status-every` is active.
    pub(crate) fn from_env(env: Option<&Env>, err: &Out, color: Choice, command: Command, view: View, status_every: bool) -> LineOpts {
        let var = |name: &str| -> Option<&str> { env?.get(&bstd::env::VarName::new(name).ok()?).ok().flatten() };
        Self::new(&|name| var(name).map(String::from), err, color, command, view, status_every)
    }

    /// From the command's variables (`var`, read through `Env` by the caller), whether
    /// `err` is a terminal and its size, `--color`, the command, `--view`, and `--status-every`.
    pub(crate) fn new(var: &dyn Fn(&str) -> Option<String>, err: &Out, color: Choice, command: Command, view: View, status_every: bool) -> LineOpts {
        let on_terminal = err.is_terminal();
        let view_allows = matches!(view, View::Person);
        let live = blaze_screen::live(var, on_terminal) && view_allows && !status_every;
        let size = err.size();
        let cols = size.map_or(80, |size| size.cols).min(MOST_COLS);
        let rows = size.map_or(24, |size| size.rows);
        let config = Config {
            color: match color {
                Choice::Auto => Color::Auto,
                Choice::Always => Color::Always,
                Choice::Never => Color::Never,
            },
            ..Config::DEFAULT
        };
        let terminal = terminal::detect(&config, var, on_terminal, Some(cols));
        LineOpts { live, caps: Caps { live, cols, rows, unicode: blaze_screen::unicode(var) }, terminal, command }
    }
}

/// What both threads touch, behind [`Line::shared`].
struct Shared<'a> {
    core: RunStatus,
    err: &'a Out,
    terminal: Terminal,
    /// The first failure; nothing more is written after it.
    failed: Option<RunLineError>,
}

impl Shared<'_> {
    /// The frame due now, if any, written.
    fn due(&mut self, cx: &Cx) {
        let Shared { core, err, terminal, failed } = self;
        if let Some(frame) = core.frame() {
            put(cx, err, terminal, failed, &frame);
        }
    }
}

/// `frame` written to `err` (nothing once a write failed: `failed` holds the first failure).
fn put(cx: &Cx, err: &Out, terminal: &Terminal, failed: &mut Option<RunLineError>, frame: &StatusFrame<'_>) {
    if failed.is_some() {
        return;
    }
    let mut bytes = LineBuf::<FRAME_BYTES>::new();
    if encode_status(frame, terminal, &mut bytes).is_err() {
        *failed = Some(RunLineError::Encode);
        return;
    }
    if let Err(why) = err.write_all(cx, bytes.as_bytes()).and_then(|()| err.flush(cx)) {
        *failed = Some(RunLineError::Out(why));
    }
}

/// The line: the core and the error stream behind one lock, the ticker's signal, the erase
/// acknowledgement signal, and the clock the times are read from (milliseconds since `began`).
struct Line<'a> {
    shared: Lock<Shared<'a>>,
    woken: Signal,
    erased: Signal,
    clock: &'a Clock,
    began: Instant,
}

impl Line<'_> {
    fn now_ms(&self) -> u64 {
        let since = self.clock.now().since(self.began).unwrap_or(Duration::ZERO);
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    }

    /// The ticker's loop: what time makes due written, until the line is over.
    fn tick(&self, spawner: &CxSpawn) {
        let cx = match spawner.cx(Duration::MAX, TICKER_SCRATCH) {
            Ok(cx) => cx,
            Err(why) => {
                let mut shared = self.shared.lock();
                if shared.failed.is_none() {
                    shared.failed = Some(RunLineError::Context(why));
                }
                self.erased.notify_all();
                return;
            }
        };
        let mut shared = self.shared.lock();
        while !shared.core.is_over() {
            let now = self.now_ms();
            shared.core.at(now);
            if let Some(size) = shared.err.size() {
                shared.core.resize_size(size.cols.min(MOST_COLS), size.rows);
            }
            shared.due(&cx);
            if !shared.core.is_drawn() {
                self.erased.notify_all();
            }
            let wait = shared.core.next_ms().map_or(IDLE, |at| Duration::from_millis(at.saturating_sub(now).max(1)));
            self.woken.wait_for(&mut shared, wait.min(IDLE));
        }
        self.erased.notify_all();
    }

    /// The line over: erased if it is on the terminal, the ticker told to end, the first
    /// failure returned. A second call writes nothing.
    fn finish(&self, cx: &Cx) -> Result<(), RunLineError> {
        let mut shared = self.shared.lock();
        let Shared { core, err, terminal, failed } = &mut *shared;
        if let Some(frame) = core.finish() {
            put(cx, err, terminal, failed, &frame);
        }
        self.woken.notify_all();
        self.erased.notify_all();
        shared.failed.take().map_or(Ok(()), Err)
    }
}

/// The sink the run is handed, on the command's thread with its context.
struct Watching<'l, 'a> {
    line: &'l Line<'a>,
    cx: &'l Cx,
}

impl Watch for Watching<'_, '_> {
    fn see(&self, step: Step<'_>) {
        let now = self.line.now_ms();
        let mut shared = self.line.shared.lock();
        let before = shared.core.next_ms();
        shared.core.see(now, step);
        shared.due(self.cx);
        // The ticker is woken only when the event moved its next time earlier.
        if shared.core.next_ms().is_some_and(|at| before.is_none_or(|was| at < was)) {
            self.line.woken.notify_one();
        }
    }

    fn paths(&self) -> bool {
        true
    }
}

/// Ends the line on every way out of [`watched`]'s work, an unwind too: the ticker then ends
/// and the scope's join returns.
struct Over<'l, 'a> {
    line: &'l Line<'a>,
    cx: &'l Cx,
}

impl Drop for Over<'_, '_> {
    fn drop(&mut self) {
        // On the way out after `watched` finished, this writes nothing; on an unwind, the
        // panic is the command's error, and the erase is all that is wanted of it.
        _ = self.line.finish(self.cx);
    }
}

/// Thread-safe progress sink for background watchers (e.g. `engine_status::watched`): updates
/// `RunStatus` and wakes the ticker thread to draw the next frame without touching `Cx`.
pub(crate) struct ProgressSink<'l, 'a> {
    line: Option<&'l Line<'a>>,
}

impl ProgressSink<'_, '_> {
    pub(crate) const NONE: ProgressSink<'static, 'static> = ProgressSink { line: None };
}

impl Watch for ProgressSink<'_, '_> {
    fn see(&self, step: Step<'_>) {
        let Some(line) = self.line else { return };
        let now = line.now_ms();
        let mut shared = line.shared.lock();
        shared.core.see(now, step);
        line.woken.notify_one();
        // When clearing before command/diagnostic output, wait until the ticker has actually
        // flushed the erase frame to stderr so lines never interleave.
        if matches!(step, Step::Says | Step::Started | Step::Answers) {
            let mut spins = 0u8;
            while shared.core.is_drawn() && !shared.core.is_over() && shared.failed.is_none() && spins < 20 {
                line.erased.wait_for(&mut shared, ERASE_WAIT);
                spins += 1;
            }
        }
    }

    fn paths(&self) -> bool {
        self.line.is_some()
    }
}

/// `work` run with the live inline TUI sink when `opts` says the error stream is a live terminal,
/// else with [`Unwatched`]: the inline TUI is erased and its thread joined before this returns, on
/// every path. `Err` only when the line itself failed (the work's value is then dropped: the
/// command ends with that error, rule 3).
pub(crate) fn watched<R>(
    cx: &Cx,
    err: &Out,
    clock: &Clock,
    opts: LineOpts,
    work: impl FnOnce(&dyn Watch, &(dyn Watch + Sync)) -> R,
) -> Result<R, RunLineError> {
    if !opts.live {
        return Ok(work(&Unwatched, &ProgressSink::NONE));
    }
    let spawner = cx.spawner();
    let core = RunStatus::new(RunStatusOpts { caps: opts.caps, command: opts.command });
    let line = Line {
        shared: Lock::new("run.line", Shared { core, err, terminal: opts.terminal, failed: None }),
        woken: Signal::new("run.line.due"),
        erased: Signal::new("run.line.erased"),
        clock,
        began: clock.now(),
    };
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name(std::string::String::from("blaze run line"))
            .spawn_scoped(scope, || line.tick(&spawner))
            .map_err(|why| RunLineError::Ticker { kind: why.kind() })?;
        let over = Over { line: &line, cx };
        let bg = ProgressSink { line: Some(&line) };
        let got = work(&Watching { line: &line, cx }, &bg);
        let done = line.finish(cx);
        drop(over);
        done.map(|()| got)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::term::{Size, Tty};

    #[test]
    fn non_tty_and_machine_views_disable_inline_tui() {
        let pipe = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let err = pipe.need_err().expect("err");
        let opts = LineOpts::new(&|_| None, &err, Choice::Auto, Command::Check, View::Person, false);
        assert!(!opts.live, "non-TTY stderr must never paint inline TUI");

        let tty_io = Io::simulated(SimWorld::new(2, Limits::USUAL));
        tty_io.script(|host| {
            host.ttys[1] = Tty::Terminal(Some(Size { cols: 100, rows: 24 }));
        }).expect("sim host");
        let tty_err = tty_io.need_err().expect("err");

        let person = LineOpts::new(&|_| None, &tty_err, Choice::Auto, Command::Check, View::Person, false);
        assert!(person.live, "TTY person view enables inline TUI");
        assert_eq!((person.caps.cols, person.caps.rows), (100, 24));

        let json = LineOpts::new(&|_| None, &tty_err, Choice::Auto, Command::Check, View::Json, false);
        assert!(!json.live, "--view json must disable inline TUI");

        let agent = LineOpts::new(&|_| None, &tty_err, Choice::Auto, Command::Check, View::Agent, false);
        assert!(!agent.live, "--view agent must disable inline TUI");

        let status_every = LineOpts::new(&|_| None, &tty_err, Choice::Auto, Command::Check, View::Person, true);
        assert!(!status_every.live, "--status-every must disable ANSI inline TUI");
    }

    #[test]
    fn fast_command_on_tty_writes_zero_stderr_bytes() {
        let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
        io.script(|host| {
            host.ttys[1] = Tty::Terminal(Some(Size { cols: 100, rows: 24 }));
        }).expect("sim host");
        let cx = io.cx(Duration::from_secs(5), 1 << 20).expect("cx");
        let err = io.need_err().expect("err");
        let clock = io.need_clock().expect("clock");
        let opts = LineOpts::new(&|_| None, &err, Choice::Auto, Command::Check, View::Person, false);
        assert!(opts.live);
        let result = watched(&cx, &err, &clock, opts, |watch, _bg| {
            watch.see(Step::Began(blaze_screen::Stage::Checking));
            42u32
        });
        assert_eq!(result.expect("watched ok"), 42);
        let stderr_len = io.script(|host| host.err.len()).expect("sim host");
        assert_eq!(stderr_len, 0, "fast command under grace delay must write 0 bytes to stderr");
    }
}
