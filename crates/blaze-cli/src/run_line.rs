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

use core::cell::RefCell;
use core::fmt::{self, Write as _};
use core::time::Duration;

use blaze_core::lock::{Lock, Signal};
use blaze_report::frame::Choice;
use blaze_report::frame::plain::View;
use blaze_report::terminal::{self, Color, Config, Terminal};
use blaze_screen::{
    Caps, Command, LineBuf, RunStatus, RunStatusOpts, StatusFrame, StatusVerb, Step, Unwatched, Watch, encode_status,
    write_command_line, write_elapsed_secs, write_status_line,
};
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
/// Maximum UTF-8 bytes kept for the package name in a `Finished` line.
const LAUNCH_PKG_BYTES: usize = 128;
/// Maximum UTF-8 bytes kept for the formatted `` `<bin> <args...>` `` command line.
const LAUNCH_CMD_BYTES: usize = 1024;
/// Stack buffer for a persistent Cargo-style status line batch (`Checking`, or `Finished` + `Running`).
const STATUS_BYTES: usize = 2048;

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

/// Persistent Cargo-style status line configuration for `blaze run` on stderr.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StatusOpts {
    /// Emit persistent `Checking` / `Finished` / `Running` lines on stderr (`Command::Run`,
    /// `View::Person`, `!quiet`), both on TTYs (colored) and non-TTYs (plain).
    pub enabled: bool,
    pub terminal: Terminal,
}

impl StatusOpts {
    pub(crate) const OFF: StatusOpts = StatusOpts { enabled: false, terminal: Terminal::PIPE };

    /// Enables Cargo-style persistent stderr status lines for human-facing non-quiet `blaze run`.
    pub(crate) fn for_run(command: Command, view: View, quiet: bool, terminal: Terminal) -> StatusOpts {
        StatusOpts { enabled: command == Command::Run && matches!(view, View::Person) && !quiet, terminal }
    }
}

/// Truncates `text` to the longest UTF-8 prefix fitting in `max_bytes`.
fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut take = max_bytes;
    while take > 0 && !text.is_char_boundary(take) {
        take -= 1;
    }
    &text[..take]
}

/// Bounded inline writer that caps output at `N - 4` bytes and flags truncation cleanly.
struct CappedLine<const N: usize> {
    buf: LineBuf<N>,
    truncated: bool,
}

impl<const N: usize> CappedLine<N> {
    const fn new() -> Self {
        Self { buf: LineBuf::new(), truncated: false }
    }
}

impl<const N: usize> fmt::Write for CappedLine<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.truncated {
            return Ok(());
        }
        let limit = N.saturating_sub(4);
        let room = limit.saturating_sub(self.buf.as_bytes().len());
        if s.len() <= room {
            let _ = self.buf.write_str(s);
        } else {
            let _ = self.buf.write_str(truncate_utf8(s, room));
            self.truncated = true;
        }
        Ok(())
    }
}

/// Resolved target metadata saved once `run_package::Program` is picked and emitted atomically
/// on `Step::Started` right after erasing the inline TUI and right before spawning the child.
struct LaunchInfo {
    package: LineBuf<LAUNCH_PKG_BYTES>,
    cmd: LineBuf<LAUNCH_CMD_BYTES>,
}

impl LaunchInfo {
    fn new(package: &str, bin: &str, args: &[&str]) -> Self {
        let mut pkg_buf = LineBuf::<LAUNCH_PKG_BYTES>::new();
        let _ = pkg_buf.write_str(truncate_utf8(package, LAUNCH_PKG_BYTES));
        let mut capped = CappedLine::<LAUNCH_CMD_BYTES>::new();
        let _ = write_command_line(&mut capped, bin, args);
        if capped.truncated {
            let _ = capped.buf.write_str("...`");
        }
        Self { package: pkg_buf, cmd: capped.buf }
    }
}

fn write_checking_line(cx: &Cx, err: &Out, terminal: &Terminal, failed: &mut Option<RunLineError>, target: &str) {
    if failed.is_some() {
        return;
    }
    let mut buf = LineBuf::<STATUS_BYTES>::new();
    let trimmed = truncate_utf8(target, 512);
    if write_status_line(&mut buf, terminal, StatusVerb::Checking, format_args!("{trimmed}")).is_err() {
        *failed = Some(RunLineError::Encode);
        return;
    }
    if let Err(why) = err.write_all(cx, buf.as_bytes()).and_then(|()| err.flush(cx)) {
        *failed = Some(RunLineError::Out(why));
    }
}

fn write_started_lines(
    cx: &Cx,
    err: &Out,
    terminal: &Terminal,
    failed: &mut Option<RunLineError>,
    launch: &LaunchInfo,
    elapsed_ms: u64,
) {
    if failed.is_some() {
        return;
    }
    let mut dur = LineBuf::<32>::new();
    if write_elapsed_secs(&mut dur, elapsed_ms).is_err() {
        *failed = Some(RunLineError::Encode);
        return;
    }
    let mut buf = LineBuf::<STATUS_BYTES>::new();
    let pkg = launch.package.as_str();
    let finished_ok = if pkg.is_empty() {
        write_status_line(&mut buf, terminal, StatusVerb::Finished, format_args!("in {}", dur.as_str()))
    } else {
        write_status_line(&mut buf, terminal, StatusVerb::Finished, format_args!("{pkg} in {}", dur.as_str()))
    };
    if finished_ok.is_err()
        || write_status_line(&mut buf, terminal, StatusVerb::Running, format_args!("{}", launch.cmd.as_str())).is_err()
    {
        *failed = Some(RunLineError::Encode);
        return;
    }
    if let Err(why) = err.write_all(cx, buf.as_bytes()).and_then(|()| err.flush(cx)) {
        *failed = Some(RunLineError::Out(why));
    }
}

/// What both threads touch, behind [`Line::shared`].
struct Shared<'a> {
    core: RunStatus,
    err: &'a Out,
    terminal: Terminal,
    status: StatusOpts,
    launch: Option<LaunchInfo>,
    /// The first failure; nothing more is written after it.
    failed: Option<RunLineError>,
}

impl Shared<'_> {
    /// The frame due now, if any, written.
    fn due(&mut self, cx: &Cx) {
        let Shared { core, err, terminal, failed, .. } = self;
        if let Some(frame) = core.frame() {
            put(cx, err, terminal, failed, &frame);
        }
    }

    /// Emits `Finished` and `Running` once when `Step::Started` fires (after `due` erased the TUI).
    fn emit_started(&mut self, cx: &Cx, elapsed_ms: u64) {
        if !self.status.enabled {
            return;
        }
        let Some(launch) = self.launch.take() else {
            return;
        };
        let Shared { err, status, failed, .. } = self;
        write_started_lines(cx, err, &status.terminal, failed, &launch, elapsed_ms);
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
        let Shared { core, err, terminal, failed, .. } = &mut *shared;
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
        if matches!(step, Step::Started) {
            shared.emit_started(self.cx, now);
        }
        // The ticker is woken only when the event moved its next time earlier.
        if shared.core.next_ms().is_some_and(|at| before.is_none_or(|was| at < was)) {
            self.line.woken.notify_one();
        }
    }

    fn paths(&self) -> bool {
        true
    }
}

/// Non-TTY persistent status sink (no ticker thread, no inline TUI redraws).
struct PlainState {
    launch: Option<LaunchInfo>,
    failed: Option<RunLineError>,
}

struct PlainStatus<'l, 'a> {
    cx: &'l Cx,
    err: &'a Out,
    clock: &'a Clock,
    began: Instant,
    status: StatusOpts,
    state: RefCell<PlainState>,
}

impl PlainStatus<'_, '_> {
    fn now_ms(&self) -> u64 {
        let since = self.clock.now().since(self.began).unwrap_or(Duration::ZERO);
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    }

    fn checking(&self, target: &str) {
        if !self.status.enabled {
            return;
        }
        let mut state = self.state.borrow_mut();
        write_checking_line(self.cx, self.err, &self.status.terminal, &mut state.failed, target);
    }

    fn set_launch(&self, package: &str, bin: &str, args: &[&str]) {
        if !self.status.enabled {
            return;
        }
        self.state.borrow_mut().launch = Some(LaunchInfo::new(package, bin, args));
    }

    fn finish(&self) -> Result<(), RunLineError> {
        self.state.borrow_mut().failed.take().map_or(Ok(()), Err)
    }
}

impl Watch for PlainStatus<'_, '_> {
    fn see(&self, step: Step<'_>) {
        if !self.status.enabled || !matches!(step, Step::Started) {
            return;
        }
        let now = self.now_ms();
        let mut state = self.state.borrow_mut();
        let Some(launch) = state.launch.take() else {
            return;
        };
        write_started_lines(self.cx, self.err, &self.status.terminal, &mut state.failed, &launch, now);
    }

    fn paths(&self) -> bool {
        false
    }
}

enum StatusTarget<'l, 'a> {
    Off,
    Live { line: &'l Line<'a>, cx: &'l Cx },
    Plain { plain: &'l PlainStatus<'l, 'a> },
}

/// Handle for emitting persistent Cargo-style `blaze run` stderr status lines without
/// interleaving with the live inline TUI or the child process.
pub(crate) struct RunStatusOut<'l, 'a> {
    target: StatusTarget<'l, 'a>,
}

impl RunStatusOut<'_, '_> {
    pub(crate) const NONE: RunStatusOut<'static, 'static> = RunStatusOut { target: StatusTarget::Off };

    /// Writes `    Checking <target>` to stderr (erasing any active live TUI first).
    pub(crate) fn checking(&self, target: &str) {
        match self.target {
            StatusTarget::Off => {}
            StatusTarget::Live { line, cx } => {
                let now = line.now_ms();
                let mut shared = line.shared.lock();
                if !shared.status.enabled {
                    return;
                }
                shared.core.see(now, Step::Says);
                shared.due(cx);
                let Shared { err, status, failed, .. } = &mut *shared;
                write_checking_line(cx, err, &status.terminal, failed, target);
            }
            StatusTarget::Plain { plain } => plain.checking(target),
        }
    }

    /// Registers the resolved binary target and arguments so `Step::Started` emits
    /// `    Finished <package> in <X.XXs>` and ``     Running `<bin> <args...>` ``
    /// atomically right after erasing the inline TUI and right before spawning the child.
    pub(crate) fn set_launch(&self, package: &str, bin: &str, args: &[&str]) {
        match self.target {
            StatusTarget::Off => {}
            StatusTarget::Live { line, .. } => {
                let mut shared = line.shared.lock();
                if shared.status.enabled {
                    shared.launch = Some(LaunchInfo::new(package, bin, args));
                }
            }
            StatusTarget::Plain { plain } => plain.set_launch(package, bin, args),
        }
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
    watched_run(cx, err, clock, opts, StatusOpts::OFF, |watch, bg, _| work(watch, bg))
}

/// Like [`watched`], additionally coordinating persistent Cargo-style `blaze run` stderr status
/// lines (`Checking`, `Finished`, `Running`) behind the same lock as the live inline TUI on TTYs
/// and directly on `err` off TTYs (without spawning a ticker thread).
pub(crate) fn watched_run<R>(
    cx: &Cx,
    err: &Out,
    clock: &Clock,
    opts: LineOpts,
    status: StatusOpts,
    work: impl FnOnce(&dyn Watch, &(dyn Watch + Sync), &RunStatusOut<'_, '_>) -> R,
) -> Result<R, RunLineError> {
    if !opts.live {
        if !status.enabled {
            return Ok(work(&Unwatched, &ProgressSink::NONE, &RunStatusOut::NONE));
        }
        let plain = PlainStatus {
            cx,
            err,
            clock,
            began: clock.now(),
            status,
            state: RefCell::new(PlainState { launch: None, failed: None }),
        };
        let status_out = RunStatusOut { target: StatusTarget::Plain { plain: &plain } };
        let got = work(&plain, &ProgressSink::NONE, &status_out);
        return plain.finish().map(|()| got);
    }
    let spawner = cx.spawner();
    let core = RunStatus::new(RunStatusOpts { caps: opts.caps, command: opts.command });
    let line = Line {
        shared: Lock::new(
            "run.line",
            Shared { core, err, terminal: opts.terminal, status, launch: None, failed: None },
        ),
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
        let status_out = if status.enabled {
            RunStatusOut { target: StatusTarget::Live { line: &line, cx } }
        } else {
            RunStatusOut { target: StatusTarget::Off }
        };
        let got = work(&Watching { line: &line, cx }, &bg, &status_out);
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

    #[test]
    fn run_status_lines_emit_on_tty_with_color_and_on_pipe_without_ansi() {
        // Non-TTY (piped/CI/agent): plain right-aligned verbs, zero ANSI escape sequences.
        let pipe = Io::simulated(SimWorld::new(11, Limits::USUAL));
        let cx = pipe.cx(Duration::from_secs(5), 1 << 20).expect("cx");
        let err = pipe.need_err().expect("err");
        let clock = pipe.need_clock().expect("clock");
        let opts = LineOpts::new(&|_| None, &err, Choice::Auto, Command::Run, View::Person, false);
        assert!(!opts.live);
        let status = StatusOpts::for_run(Command::Run, View::Person, false, opts.terminal);
        assert!(status.enabled);
        let got = watched_run(&cx, &err, &clock, opts, status, |watch, _bg, out| {
            out.checking("hello");
            watch.see(Step::Began(blaze_screen::Stage::Checking));
            out.set_launch("hello", "hello", &["--port", "8080"]);
            watch.see(Step::Started);
            7u32
        })
        .expect("watched_run ok");
        assert_eq!(got, 7);
        let pipe_err = pipe
            .script(|host| std::string::String::from_utf8_lossy(&host.err).into_owned())
            .expect("sim host");
        assert!(!pipe_err.contains('\x1b'), "non-TTY stderr must contain zero ANSI escapes: {pipe_err:?}");
        assert!(pipe_err.contains("    Checking hello\n"), "missing Checking line: {pipe_err:?}");
        assert!(pipe_err.contains("    Finished hello in "), "missing Finished line: {pipe_err:?}");
        assert!(pipe_err.contains("     Running `hello --port 8080`\n"), "missing Running line: {pipe_err:?}");

        // TTY (color-capable): ANSI bold-cyan Checking, bold-green Finished + Running.
        let tty = Io::simulated(SimWorld::new(12, Limits::USUAL));
        tty.script(|host| {
            host.ttys[1] = Tty::Terminal(Some(Size { cols: 100, rows: 24 }));
        })
        .expect("sim host");
        let cx = tty.cx(Duration::from_secs(5), 1 << 20).expect("cx");
        let err = tty.need_err().expect("err");
        let clock = tty.need_clock().expect("clock");
        let opts = LineOpts::new(&|_| None, &err, Choice::Auto, Command::Run, View::Person, false);
        assert!(opts.live);
        assert!(opts.terminal.caps.color);
        let status = StatusOpts::for_run(Command::Run, View::Person, false, opts.terminal);
        assert!(status.enabled);
        watched_run(&cx, &err, &clock, opts, status, |watch, _bg, out| {
            out.checking("scripts (rules)");
            watch.see(Step::Began(blaze_screen::Stage::Checking));
            out.set_launch("scripts", "rules", &["write"]);
            watch.see(Step::Started);
        })
        .expect("watched_run tty ok");
        let tty_err = tty
            .script(|host| std::string::String::from_utf8_lossy(&host.err).into_owned())
            .expect("sim host");
        assert!(tty_err.contains("\x1b[1;"), "TTY stderr must style status verbs in bold color: {tty_err:?}");
        assert!(tty_err.contains("Checking\x1b[0m scripts (rules)\n"), "missing colored Checking: {tty_err:?}");
        assert!(tty_err.contains("Finished\x1b[0m scripts in "), "missing colored Finished: {tty_err:?}");
        assert!(tty_err.contains("Running\x1b[0m `rules write`\n"), "missing colored Running: {tty_err:?}");
    }

    #[test]
    fn run_status_lines_suppressed_by_quiet_and_machine_views() {
        for (is_tty, view, quiet, cmd) in [
            (false, View::Person, true, Command::Run),
            (true, View::Person, true, Command::Run),
            (false, View::Json, false, Command::Run),
            (true, View::Json, false, Command::Run),
            (false, View::Agent, false, Command::Run),
            (true, View::Agent, false, Command::Run),
            (false, View::Person, false, Command::Test),
            (false, View::Person, false, Command::Check),
        ] {
            let io = Io::simulated(SimWorld::new(20, Limits::USUAL));
            if is_tty {
                io.script(|host| {
                    host.ttys[1] = Tty::Terminal(Some(Size { cols: 100, rows: 24 }));
                })
                .expect("sim host");
            }
            let cx = io.cx(Duration::from_secs(5), 1 << 20).expect("cx");
            let err = io.need_err().expect("err");
            let clock = io.need_clock().expect("clock");
            let base = LineOpts::new(&|_| None, &err, Choice::Auto, cmd, view, false);
            let opts = LineOpts { live: base.live && !quiet, ..base };
            let status = StatusOpts::for_run(cmd, view, quiet, opts.terminal);
            assert!(!status.enabled, "status must be off for tty={is_tty} view={view:?} quiet={quiet} cmd={cmd:?}");
            watched_run(&cx, &err, &clock, opts, status, |watch, _bg, out| {
                out.checking("hello");
                watch.see(Step::Began(blaze_screen::Stage::Checking));
                out.set_launch("hello", "hello", &[]);
                watch.see(Step::Started);
            })
            .expect("watched_run ok");
            let stderr_len = io.script(|host| host.err.len()).expect("sim host");
            assert_eq!(stderr_len, 0, "expected 0 stderr bytes for tty={is_tty} view={view:?} quiet={quiet} cmd={cmd:?}");
        }
    }

    #[test]
    fn run_status_lines_omit_finished_and_running_when_aborted_before_start() {
        let io = Io::simulated(SimWorld::new(30, Limits::USUAL));
        let cx = io.cx(Duration::from_secs(5), 1 << 20).expect("cx");
        let err = io.need_err().expect("err");
        let clock = io.need_clock().expect("clock");
        let opts = LineOpts::new(&|_| None, &err, Choice::Auto, Command::Run, View::Person, false);
        let status = StatusOpts::for_run(Command::Run, View::Person, false, opts.terminal);
        watched_run(&cx, &err, &clock, opts, status, |watch, _bg, out| {
            out.checking("broken");
            watch.see(Step::Began(blaze_screen::Stage::Checking));
            out.set_launch("broken", "broken", &[]);
            // Early error return before `Step::Started` is ever reached.
        })
        .expect("watched_run ok");
        let stderr = io
            .script(|host| std::string::String::from_utf8_lossy(&host.err).into_owned())
            .expect("sim host");
        assert_eq!(stderr, "    Checking broken\n");
    }
}
