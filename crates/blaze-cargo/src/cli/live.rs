//! `blaze build`, `check` and `test` on a terminal: one live view of the build (and, for
//! `test`, of its tests), built the way Elm builds a program. The models, their updates and
//! their views are blaze-ui's sans-IO [`Board`] (fed each [`ResponseRef::Doing`]'s model of
//! the build, [`blaze_protocol::now::Now`], and the keys pressed) and, for `test`, its
//! [`TestView`] (`apply` is the update, fed [`Event`]s with the time injected; `render` is the
//! view, into a ratatui buffer; `take_above` and `summary` are the effects, lines to print
//! above the live region), drawn under the board's sections
//! (docs/v2/plan/design/build-view.md, "The renderer"). This file is only the runtime around
//! them, threads that do the IO and nothing else:
//!
//! - the caller's thread talks to the daemon and turns each response into a message for the
//!   screen (and, for `test`, a [`Run::Ready`] for each test executable as it is linked,
//!   [`Response::TestReady`]);
//! - for `test`, the runner starts each test executable as soon as it is ready, a few at a
//!   time, while the build goes on, and reads libtest's JSON into more [`Event`]s;
//! - the screen owns the terminal, in raw mode so it reads keys (`e` expands the board,
//!   Ctrl-C cancels as the signal does), in one of two [`Form`]s:
//!   - [`Form::Full`], on a terminal at least [`FULL_COLUMNS`] by [`FULL_ROWS`] and unless
//!     `BLAZE_UI=inline`: the alternate screen, so the scrollback keeps only what stays. What
//!     the printer and the test view print (diagnostics, failures) is held while the view is
//!     up, counted on its last row, and printed in order once it leaves, then the summary;
//!   - [`Form::Inline`]: a few rows under the scrollback ([`Viewport::Inline`]), with what is
//!     printed going above it as it comes, so the failures and the summary stay in the
//!     scrollback and the live region is cleared.
//!
//! Off a terminal (a pipe, CI, `--message-format json`, `-q`, `-v`, `BLAZE_UI=plain`) none of
//! this runs: [`wanted`] says no and `cli::drive` prints its plain lines, the build's compact
//! line among them ([`blaze_ui::board::Board::line`], from the printer). For `test` the exit
//! code is cargo's either way: 0, the code of the first failing executable in cargo's order,
//! under `--fail-fast`, else 101 when anything failed (the default runs every executable).
//!
//! Without blaze-cargo's `native` feature (the wasm seed, which has no terminal library) a run
//! that would draw the view is refused, naming the feature ([`drive`]); the runtime around the
//! terminal still builds and its tests still run.
//!
//! [`Response::TestReady`]: blaze_api::Response::TestReady

#![cfg_attr(
    not(feature = "native"),
    allow(
        dead_code,
        unused_imports,
        reason = "without `native`, only `wanted` and the refusing `drive` are reached"
    )
)]

use std::{
    alloc::Global,
    ffi::OsString,
    io::{self, BufRead as _, BufReader, IsTerminal as _, Read as _, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    process::{Child, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use blaze_api::{Artifact, ArtifactKind, Level, Request, ResponseRef, TargetKind};
use blaze_core::exit::Exit;
use blaze_hash::HashSet;
use blaze_protocol::now::Now;
use blaze_ui::{
    board::{Board, Form, INLINE_ROWS, Key},
    test_view::{BinaryEnd, Event, Paint, TestView, Verdict, ansi, wrap},
};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use blaze_core::lock::Lock;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};
#[cfg(feature = "native")]
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    crossterm::{
        cursor,
        event::{self as keys, KeyCode, KeyEventKind, KeyModifiers},
        execute,
        terminal::{self as tty, EnterAlternateScreen, LeaveAlternateScreen},
    },
};

use super::{
    Then,
    args::{Command, Invocation},
    daemon::{self, Api},
    place::Place,
    print::{Printer, Shown},
    run::Exec,
    say,
    test::{self, Kind},
};
use crate::serve::signals;

/// How often the region is redrawn and the keys read: the spinner's frame.
const TICK: Duration = Duration::from_millis(80);
/// The rows the test view's own section gets: in the inline form all of them under the
/// board's, in the full form at least these (or half the screen, the fewer) under the board.
const REGION: u16 = 10;
/// The smallest terminal the view is drawn on; a smaller one gets the plain lines.
const COLUMNS: u16 = 40;
const ROWS: u16 = 8;
/// The smallest terminal the full form is drawn on; a smaller one gets the inline form.
const FULL_COLUMNS: u16 = 80;
const FULL_ROWS: u16 = 20;
/// The most keys read in one tick: a paste or a held key is read over a few ticks, never in
/// an open loop.
const KEYS_A_TICK: usize = 64;
/// How long a test executable's output is read after its process exited. Whatever it wrote
/// is in the pipe by then, so only a process it started and left running, which inherited its
/// stdout or stderr, holds the pipe open past this; the runner then stops reading and says so.
const DRAIN: Duration = Duration::from_secs(2);
/// libtest's arguments that change what it prints, which the view can't read: with any of
/// them the run is plain.
const PLAIN_ARGS: [&str; 9] = [
    "--format",
    "--list",
    "--nocapture",
    "--no-capture",
    "--logfile",
    "-q",
    "--quiet",
    "-h",
    "--help",
];

/// Whether the command draws its live view: `build`, `check` or `test`, whose stdout and
/// stderr are terminals big enough, printed for a person (not JSON, `-q` or `-v`), not
/// `BLAZE_UI=plain` or a dumb terminal, and not on CI ([`blaze_units::lock::ci_says`]), whose
/// workers may hand it a pseudo-terminal whose log is read as text. A `test` also runs its tests
/// (not `--no-run`, `--reached`, `--ub`) with libtest's own output left as the view reads it;
/// a `check` makes no fixes and counts no baselines (`--fix`, `--baselines`), whose work after
/// the build is the plain path's; neither is `blaze lock`'s.
pub fn wanted(asked: &Invocation, shown: Shown) -> bool {
    let plain_ui = std::env::var_os("BLAZE_UI").is_some_and(|it| it == "plain");
    let dumb = std::env::var_os("TERM").is_some_and(|it| it == "dumb");
    let command = asked.command();
    if !matches!(command, Command::Build | Command::Check | Command::Test)
        || asked.lock
        || blaze_units::lock::ci_says(
            std::env::var("CI").ok().as_deref(),
            std::env::var("TF_BUILD").ok().as_deref(),
        )
        || shown.format.json()
        || shown.quiet
        || shown.verbose > 0
        || plain_ui
        || dumb
        || !io::stdout().is_terminal()
        || !io::stderr().is_terminal()
    {
        return false;
    }
    let own = match command {
        Command::Test => {
            let reads_args = asked.program.iter().all(|arg| {
                let arg = arg.to_string_lossy();
                !PLAIN_ARGS
                    .iter()
                    .any(|plain| arg == *plain || arg.starts_with(&format!("{plain}=")))
            });
            !asked.flags.no_run && !asked.flags.reached && !asked.flags.ub && reads_args
        }
        Command::Check => !asked.flags.fix && asked.flags.baselines.is_none(),
        _ => true,
    };
    own && fits()
}

/// Whether the terminal is big enough for the view.
#[cfg(feature = "native")]
fn fits() -> bool {
    tty::size().is_ok_and(|(columns, rows)| columns >= COLUMNS && rows >= ROWS)
}

/// Without `native` there is no terminal library to ask the size, and the view is what this run
/// asks for whatever it is: [`drive`] refuses it, naming the feature.
#[cfg(not(feature = "native"))]
fn fits() -> bool {
    true
}

/// The form the view takes on this terminal: full on one big enough unless `BLAZE_UI=inline`.
#[cfg(feature = "native")]
fn form() -> Form {
    let inline = std::env::var_os("BLAZE_UI").is_some_and(|it| it == "inline");
    let big =
        tty::size().is_ok_and(|(columns, rows)| columns >= FULL_COLUMNS && rows >= FULL_ROWS);
    match big && !inline {
        true => Form::Full,
        false => Form::Inline,
    }
}

/// What a blaze built without blaze-cargo's `native` feature says when a run asks for the view.
#[cfg(not(feature = "native"))]
const NO_TUI: &str = "`blaze build`, `check` and `test` on a terminal draw a live view, and this blaze is built \
                      without blaze-cargo's `native` feature (the seed profile, which has no terminal library); \
                      `BLAZE_UI=plain` prints plain lines";

/// [`NO_TUI`]: the run is refused before anything is built.
#[cfg(not(feature = "native"))]
pub fn drive<A: Api>(
    _api: &mut A,
    _asked: &Invocation,
    _place: &Place,
    _request: Request,
    _shown: Shown,
    _started: Instant,
) -> io::Result<Then> {
    Err(io::Error::other(NO_TUI))
}

/// What the screen is sent, in the order it happened, then its end.
enum Msg {
    /// For the test view (`test` only).
    View(Event),
    /// The build as it runs now, for the board.
    Now(Now),
    /// A burst of the printer's whole lines, as it wrote them: for `test` the test view's to
    /// print, else printed above the region (inline) or held until the view leaves (full).
    Said(String),
    Finish,
}

/// What the runner is sent.
enum Run {
    /// A test executable is linked and placed.
    Ready(Artifact),
    /// One's stdout and stderr closed: all it printed is in its [`Captured`].
    Drained { binary: u32 },
    /// One's process exited, and is not yet reaped (the runner reaps it): its output is read
    /// for at most [`DRAIN`] more. `Err` says why its exit could not be waited for.
    Exited { binary: u32, waited: Result<(), String> },
    /// The build is over: whether it passed, and everything it placed (the executables that
    /// came [`Run::Ready`] among them; the rest, the doc tests and any the daemon did not
    /// stream, are queued now).
    BuildOver { ok: bool, artifacts: Vec<Artifact> },
}

/// The build (and for `test` its tests), drawn live: what `cli::drive` does for `build`,
/// `check` and `test` on a terminal.
#[cfg(feature = "native")]
pub fn drive<A: Api>(
    api: &mut A,
    asked: &Invocation,
    place: &Place,
    request: Request,
    shown: Shown,
    started: Instant,
) -> io::Result<Then> {
    let paint = if shown.color { Paint::Color } else { Paint::Plain };
    let command = asked.command();
    let testing = command == Command::Test;
    let form = form();
    let terminal = open(form, testing).map_err(|error| {
        // alloc: the error the command ends with, once
        io::Error::other(format!(
            "the live view could not start on this terminal ({error}); `BLAZE_UI=plain` prints plain lines"
        ))
    })?;
    // Ctrl-C, as the signal or as a key in raw mode, is a flag or a key the screen reads: it
    // puts the terminal back, then ends the command. The test executables share the
    // terminal's process group, so they get it too.
    signals::catch();
    let (to_screen, screen_gets) = unbounded::<Msg>();
    // alloc: once a command
    let board = Board::new(&format!("blaze {}", command.name()), paint);
    // Made before its thread, so a thread that can't start drops it, which puts the terminal back.
    let screen = Screen {
        terminal,
        form,
        board,
        view: testing.then(|| TestView::new(paint).under_board()),
        paint,
        said: Vec::new(), // alloc: one a command
        held: Vec::new(), // alloc: one a command
        broken: false,
        restored: false,
        started,
    };
    let screen = thread::Builder::new()
        .name("blaze live screen".into())
        .spawn(move || screen.run(&screen_gets))?;
    let (to_runner, runner) = match testing {
        true => {
            let (to_runner, runner_gets) = unbounded::<Run>();
            let to_screen = to_screen.clone();
            let for_runner = to_runner.clone();
            // The workspace's member count comes with `Built`, after the first executables run,
            // so "to rerun pass" always names the package: `-p calc --lib` reruns it in any
            // workspace.
            let cores = blaze_core::cores::get().map_err(io::Error::other)?;
            let runner = Runner::new(asked, cores, to_screen, for_runner, true, shown.color);
            let runner = thread::Builder::new()
                .name("blaze test runner".into())
                .spawn(move || runner.run(&runner_gets))?;
            (Some(to_runner), Some(runner))
        }
        false => (None, None),
    };

    let (mut out, mut err) = (Above::new(to_screen.clone()), Above::new(to_screen.clone()));
    let mut printer = Printer::new(shown, &place.root, &mut out, &mut err);
    printer.place(std::env::var("HOME").ok(), super::observe::last_failure());
    let mut failed_units: HashSet<u32, Global> = HashSet::default(); // alloc: one a command
    let mut unwritten = None;
    // The test view's events; a build or check has none.
    let view = |event: Event| {
        if testing {
            let _ = to_screen.send(Msg::View(event));
        }
    };
    let called = api.call(request, |response, more| {
        if let ResponseRef::Work(worked) = &response {
            super::client_rows::heard(worked.build_id);
        }
        let printed = match response {
            // The board stands for cargo's `Compiling` lines.
            ResponseRef::Unit(_) => Ok(()),
            ResponseRef::UnitBegan(unit) => {
                if testing {
                    view(Event::UnitBegan {
                        id: unit.id,
                        label: unit_label(
                            say::package_name(unit.package_id),
                            unit.target.kind,
                            unit.target.name,
                            unit.test,
                        ),
                    });
                }
                Ok(())
            }
            ResponseRef::UnitEnded(unit) => {
                view(Event::UnitEnded { id: unit.id });
                printer.on(ResponseRef::UnitEnded(unit))
            }
            ResponseRef::Diagnostic(diagnostic) => {
                if testing
                    && diagnostic.level == Level::Error
                    && let Some(unit) = &diagnostic.unit
                    && failed_units.insert(unit.id)
                {
                    view(Event::UnitFailed { id: unit.id });
                }
                printer.on(ResponseRef::Diagnostic(diagnostic))
            }
            ResponseRef::Report { unit, report } => {
                if testing
                    && report.severity == blaze_api::Severity::Error
                    && let Some(unit) = &unit
                    && failed_units.insert(unit.id)
                {
                    view(Event::UnitFailed { id: unit.id });
                }
                printer.on(ResponseRef::Report { unit, report })
            }
            ResponseRef::TestReady(artifact) => {
                if let Some(to_runner) = &to_runner {
                    let _ = to_runner.send(Run::Ready(artifact));
                }
                Ok(())
            }
            // What the build waits on and what its units are inside: the views draw it, so
            // nothing is printed above them.
            ResponseRef::Doing(doing) => {
                if testing {
                    view(Event::Doing {
                        on: doing.on,
                        busy: doing.busy,
                        idle: doing.idle,
                        running: doing.running,
                        units: doing.units.into_iter().map(|it| (it.id, it.doing)).collect(), // alloc: once a second
                    });
                }
                let _ = to_screen.send(Msg::Now(doing.now));
                Ok(())
            }
            response => printer.on(response),
        };
        let written = printed.and_then(|()| match more {
            true => Ok(()),
            false => printer.flush(),
        });
        if let Err(error) = written {
            unwritten.get_or_insert(error);
        }
    });
    let ok = match called {
        Ok(ok) => ok && !printer.refused(),
        Err(error) => {
            // alloc: the error a build ends with, once
            let what = match daemon::hung_up(&error).then(|| api.died()).flatten() {
                // Stopped at its memory ceiling, or killed by the kernel at its limit: that is
                // the failure, said as itself, never as a hang-up.
                Some(Ok(died)) if died.at_ceiling().is_some() => died.to_string(),
                Some(Ok(died)) => format!("{error}: {died}"),
                Some(Err(why)) => {
                    format!("{error}, and what came of the daemon is not known: {why:#}")
                }
                None => error.to_string(),
            };
            if let Err(error) = printer.err().write_all(say::error(&what, shown.color).as_bytes()) {
                unwritten.get_or_insert(error);
            }
            false
        }
    };
    // Nothing returns before the runner and the screen are told the build is over: the
    // terminal is put back on every path.
    if let Err(error) = printer.finish(ok, started.elapsed()) {
        unwritten.get_or_insert(error);
    }
    let artifacts = printer.take_artifacts();
    for it in printer.take_unbuilt() {
        view(Event::Unbuilt { unit: it.unit, behind: it.behind, why: it.why });
    }
    drop(printer);
    for flushed in [out.flush(), err.flush()] {
        if let Err(error) = flushed {
            unwritten.get_or_insert(error);
        }
    }
    view(Event::BuildEnded { ok });
    let code = match (to_runner, runner) {
        (Some(to_runner), Some(runner)) => {
            let _ = to_runner.send(Run::BuildOver { ok, artifacts });
            drop(to_runner);
            runner.join().unwrap_or(Exit::Failed.code())
        }
        _ if ok => 0,
        _ => Exit::Failed.code(),
    };
    let _ = to_screen.send(Msg::Finish);
    drop(to_screen);
    let shown = screen
        .join()
        .map_err(|_| io::Error::other("the live view's screen thread panicked"))?;
    shown?;
    if let Some(error) = unwritten {
        return Err(error);
    }
    Ok(Then::Exit(code))
}

/// The terminal in raw mode, on stderr as cargo's own lines are: the alternate screen for
/// [`Form::Full`], an inline region for [`Form::Inline`] tall enough for the board's inline
/// rows and, for `test`, the test view's under them. What was entered before a step failed is
/// left again: a view that can't start leaves the terminal as it found it.
#[cfg(feature = "native")]
fn open(form: Form, testing: bool) -> io::Result<Terminal<CrosstermBackend<io::Stderr>>> {
    let (_, rows) = tty::size()?;
    tty::enable_raw_mode().map_err(|error| io::Error::other(format!("raw mode: {error}")))?;
    let opened = match form {
        Form::Full => execute!(io::stderr(), EnterAlternateScreen)
            .map_err(|error| io::Error::other(format!("the alternate screen: {error}")))
            .and_then(|()| {
                Terminal::with_options(
                    CrosstermBackend::new(io::stderr()),
                    TerminalOptions { viewport: Viewport::Fullscreen },
                )
            }),
        Form::Inline => {
            let wanted = INLINE_ROWS as u16 + if testing { REGION } else { 0 };
            let height = wanted.min(rows.saturating_sub(4)).max(3);
            Terminal::with_options(
                CrosstermBackend::new(io::stderr()),
                TerminalOptions { viewport: Viewport::Inline(height) },
            )
        }
    };
    if opened.is_err() {
        if form == Form::Full {
            let _ = execute!(io::stderr(), LeaveAlternateScreen);
        }
        let _ = tty::disable_raw_mode();
    }
    opened
}

/// `calc lib test`, `calc test it`, `calc build script`: a unit as the build row names it.
fn unit_label(package: &str, kind: TargetKind, name: &str, test: bool) -> String {
    // alloc: one label a unit
    match kind {
        TargetKind::Lib if test => format!("{package} lib test"),
        TargetKind::Lib => format!("{package} lib"),
        TargetKind::Bin => format!("{package} bin {name}"),
        TargetKind::Test => format!("{package} test {name}"),
        TargetKind::Bench => format!("{package} bench {name}"),
        TargetKind::Example => format!("{package} example {name}"),
        TargetKind::CustomBuild => format!("{package} build script"),
    }
}

/// The printer's lines, sent to the screen whole: what it writes between flushes is a burst of
/// whole lines, printed above the region.
struct Above {
    to: Sender<Msg>,
    held: Vec<u8>,
}

impl Above {
    fn new(to: Sender<Msg>) -> Above {
        Above { to, held: Vec::new() } // alloc: one buffer a stream, for the command
    }
}

impl Write for Above {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.held.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.held.is_empty() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&self.held).into_owned(); // alloc: one burst of lines
        self.held.clear();
        let _ = self.to.send(Msg::Said(text));
        Ok(())
    }
}

/// The runtime's screen: each message is a view's update, each tick the keys read and the
/// views drawn, and what is printed goes above the region (inline) or is held until the view
/// leaves (full).
#[cfg(feature = "native")]
struct Screen {
    terminal: Terminal<CrosstermBackend<io::Stderr>>,
    form: Form,
    board: Board,
    /// `test`'s own view, drawn under the board's sections.
    view: Option<TestView>,
    paint: Paint,
    /// The printer's bursts not yet taken, for a run with no test view (which takes them).
    said: Vec<String>,
    /// In the full form, every line printed while the view is up: printed, in order, once it
    /// leaves the alternate screen.
    held: Vec<Line<'static>>,
    /// Drawing panicked once (or the keys could not be read): the region is gone, the terminal
    /// is put back, and what the views hand back is printed as plain lines from then on.
    broken: bool,
    /// The terminal is back as it was found: out of raw mode and the alternate screen.
    restored: bool,
    started: Instant,
}

#[cfg(feature = "native")]
impl Screen {
    fn run(mut self, gets: &Receiver<Msg>) -> io::Result<()> {
        loop {
            let mut finish = false;
            match gets.recv_timeout(TICK) {
                Ok(msg) => finish |= self.apply(msg),
                Err(RecvTimeoutError::Disconnected) => finish = true,
                Err(RecvTimeoutError::Timeout) => {}
            }
            // A burst is one redraw.
            while let Ok(msg) = gets.try_recv() {
                finish |= self.apply(msg);
            }
            if signals::signalled() {
                self.interrupted(false);
            }
            if !self.broken
                && let Err(error) = self.keys()
            {
                self.break_view(&format!("its keys could not be read: {error}"));
            }
            if let Some(view) = &mut self.view {
                view.tick(self.started.elapsed());
            }
            self.frame();
            if finish {
                return self.finish();
            }
        }
    }

    /// One message: whether it is the end.
    fn apply(&mut self, msg: Msg) -> bool {
        let now = self.started.elapsed();
        match msg {
            Msg::View(event) => {
                if let Some(view) = &mut self.view {
                    view.apply(now, event);
                }
            }
            Msg::Now(model) => self.board.take(model),
            Msg::Said(text) => match &mut self.view {
                Some(view) => view.apply(now, Event::Said(text)),
                None => self.said.push(text),
            },
            Msg::Finish => return true,
        }
        false
    }

    /// The keys pressed since the last tick, [`KEYS_A_TICK`] at most, none waited for: raw mode
    /// makes Ctrl-C a key, which cancels as the signal does; `e` expands the board.
    fn keys(&mut self) -> io::Result<()> {
        for _ in 0..KEYS_A_TICK {
            if !keys::poll(Duration::ZERO)? {
                return Ok(());
            }
            let keys::Event::Key(key) = keys::read()? else { continue };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('c' | 'C') if control => self.interrupted(true),
                KeyCode::Char('e') if !control => self.board.key(Key::Expand),
                _ => self.board.key(Key::Other),
            }
        }
        Ok(())
    }

    /// What was printed since the last call, as lines `width` columns wide at most, in order.
    fn take_above(&mut self, width: u16) -> Vec<Line<'static>> {
        let mut lines = match &mut self.view {
            Some(view) => view.take_above(width),
            None => Vec::new(), // alloc: none until one is pushed
        };
        for text in std::mem::take(&mut self.said) {
            for line in text.strip_suffix('\n').unwrap_or(text.as_str()).split('\n') {
                lines.extend(wrap(ansi(line, self.paint), width as usize));
            }
        }
        lines
    }

    /// One frame: what was printed above the region (inline) or held (full), the views
    /// redrawn. A panic in drawing is said once and ends the region, not the run.
    fn frame(&mut self) {
        if self.broken {
            let lines = self.take_above(width());
            plain(&lines);
            return;
        }
        let drawn = catch_unwind(AssertUnwindSafe(|| self.draw()));
        let why = match drawn {
            Ok(Ok(())) => return,
            Ok(Err(error)) => error.to_string(),
            Err(panic) => panic
                .downcast_ref::<&str>()
                .map(|it| (*it).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic with no message".to_owned()), // alloc: once, as the view breaks
        };
        self.break_view(&why);
    }

    fn draw(&mut self) -> io::Result<()> {
        self.terminal.autoresize()?;
        let width = self.terminal.size()?.width;
        let lines = self.take_above(width);
        match self.form {
            Form::Full => self.held.extend(lines),
            Form::Inline => above(&mut self.terminal, &lines)?,
        }
        let (form, board, view) = (self.form, &self.board, self.view.as_ref());
        let held = self.held.len();
        self.terminal.draw(|frame| {
            let area = frame.area();
            draw_into(form, board, view, held, area, frame.buffer_mut());
        })?;
        Ok(())
    }

    /// The region is gone for the rest of the run: the terminal put back, why said, and what
    /// was held printed as plain lines.
    fn break_view(&mut self, why: &str) {
        self.broken = true;
        if self.form == Form::Inline {
            let _ = self.terminal.clear();
        }
        let _ = self.terminal.show_cursor();
        self.leave();
        // alloc: once, as the view breaks
        let said =
            format!("the live view stopped drawing: {why}; the rest is printed as plain lines");
        eprint!("{}", say::error(&said, false));
        plain(&std::mem::take(&mut self.held));
    }

    /// The run is over: what is left to print and the summary printed above the region (and in
    /// the full form everything held, after the alternate screen is left), the region cleared,
    /// the cursor after them, so the shell's prompt follows the summary.
    fn finish(mut self) -> io::Result<()> {
        let width = match self.broken {
            true => width(),
            false => self.terminal.size()?.width,
        };
        let lines = self.take_above(width);
        // The board's summary first (the build's time, its slowest tasks, what failed), then
        // the tests'.
        let mut said = String::new(); // alloc: once, at the end
        self.board.summary(&mut said);
        // alloc: once, at the end
        let mut summary: Vec<Line<'static>> = said.lines().map(|it| Line::from(it.to_owned())).collect();
        if let Some(view) = &self.view {
            summary.extend(view.summary(width));
        }
        if self.broken {
            plain(&lines);
            plain(&summary);
            return Ok(());
        }
        match self.form {
            Form::Inline => {
                above(&mut self.terminal, &lines)?;
                above(&mut self.terminal, &summary)?;
                self.restore()
            }
            Form::Full => {
                self.restore()?;
                let mut held = std::mem::take(&mut self.held);
                held.extend(lines);
                held.extend(summary);
                if let Err(error) = after(&held) {
                    plain(&held);
                    let said = format!(
                        "the live view could not print what it held in color: {error}"
                    ); // alloc: once, at the end
                    eprint!("{}", say::warning(&said, false));
                }
                Ok(())
            }
        }
    }

    /// The region cleared (inline) or the alternate screen left (full), the cursor shown and
    /// back where the region began, raw mode off.
    fn restore(&mut self) -> io::Result<()> {
        if self.form == Form::Inline {
            self.terminal.clear()?;
            let top = self.terminal.get_frame().area().y;
            self.terminal.set_cursor_position((0, top))?;
        }
        self.terminal.show_cursor()?;
        self.leave();
        io::stderr().flush()
    }

    /// Out of the alternate screen and raw mode, once.
    fn leave(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        if self.form == Form::Full {
            let _ = execute!(io::stderr(), LeaveAlternateScreen, cursor::Show);
        }
        let _ = tty::disable_raw_mode();
    }

    /// Ctrl-C, as the signal or (`key`) as the key raw mode made of it: the terminal put back,
    /// what was held printed, the test executables sent the SIGINT the terminal would have
    /// sent them, then the command ends as a shell's interrupted one does.
    fn interrupted(&mut self, key: bool) -> ! {
        if !self.broken {
            let _ = self.restore();
        }
        plain(&std::mem::take(&mut self.held));
        if key {
            crate::raw::process::interrupt_own_group();
        }
        let _ = io::stderr().write_all(b"interrupted\n");
        std::process::exit(blaze_core::exit::INTERRUPTED);
    }
}

/// A screen dropped without its end (its thread never started, or it panicked outside
/// drawing) still leaves the terminal as it found it.
#[cfg(feature = "native")]
impl Drop for Screen {
    fn drop(&mut self) {
        if !self.restored {
            let _ = self.terminal.show_cursor();
            self.leave();
        }
    }
}

/// The terminal's width, once the region is gone.
#[cfg(feature = "native")]
fn width() -> u16 {
    tty::size().map_or(80, |(columns, _)| columns)
}

/// The views into `area`: the board's sections on top and, for `test`, the test view's under
/// them; in the full form with lines held, a row at the bottom saying how many.
fn draw_into(
    form: Form,
    board: &Board,
    view: Option<&TestView>,
    held: usize,
    area: Rect,
    buf: &mut Buffer,
) {
    let mut rest = area;
    if form == Form::Full && held > 0 && area.height >= 2 {
        rest.height -= 1;
        let what = match held {
            1 => " 1 line held, printed when the view closes".to_owned(), // alloc: a row
            n => format!(" {n} lines held, printed when the view closes"), // alloc: a row
        };
        let row = Line::from(Span::styled(what, Style::new().add_modifier(Modifier::DIM)));
        buf.set_line(area.x, area.bottom() - 1, &row, area.width);
    }
    let Some(view) = view else {
        board.render(form, rest, buf);
        return;
    };
    let tests = REGION.min(rest.height / 2);
    let rows = board.height(form, rest.width).min(rest.height - tests);
    let top = Rect { height: rows, ..rest };
    let under = Rect { y: rest.y + rows, height: rest.height - rows, ..rest };
    board.render(form, top, buf);
    view.render(under, buf);
}

/// `lines` printed above the region, into the scrollback.
#[cfg(feature = "native")]
fn above(
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    lines: &[Line<'static>],
) -> io::Result<()> {
    // A chunk at most a screen's worth of rows, so no height overflows.
    for chunk in lines.chunks(256) {
        terminal.insert_before(chunk.len() as u16, |buf| {
            let area = buf.area;
            for (row, line) in chunk.iter().enumerate() {
                let y = area.y.saturating_add(row as u16);
                if y < area.bottom() {
                    buf.set_line(area.x, y, line, area.width);
                }
            }
        })?;
    }
    Ok(())
}

/// What the full form held, printed into the scrollback once the alternate screen is left:
/// through a one-row inline region, so its colors are drawn as the inline form draws them.
#[cfg(feature = "native")]
fn after(lines: &[Line<'static>]) -> io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(io::stderr()),
        TerminalOptions { viewport: Viewport::Inline(1) },
    )?;
    above(&mut terminal, lines)?;
    terminal.clear()?;
    let top = terminal.get_frame().area().y;
    terminal.set_cursor_position((0, top))?;
    terminal.show_cursor()?;
    io::stderr().flush()
}

/// `lines` as text, once the region is gone (raw mode is off by then).
#[cfg(feature = "native")]
fn plain(lines: &[Line<'static>]) {
    if lines.is_empty() {
        return;
    }
    let mut text = String::new(); // alloc: once a frame after the view broke
    for line in lines {
        for span in &line.spans {
            text.push_str(&span.content);
        }
        text.push('\n');
    }
    let _ = io::stderr().write_all(text.as_bytes());
}

/// libtest's variable for how many threads it runs tests on.
const RUST_TEST_THREADS: &str = "RUST_TEST_THREADS";

/// One test executable the runner knows.
struct Binary {
    artifact: Artifact,
    /// cargo's order of it, as text: the summary sorts by it, and the first failing in it
    /// gives the exit code.
    order: String,
    label: String,
    /// libtest's JSON is read from it; otherwise it is one test, its exit status the verdict.
    json: bool,
    began: Option<Instant>,
}

/// One running.
struct Running {
    binary: u32,
    child: Child,
    pid: u32,
    /// What it printed, as read so far.
    captured: Arc<Lock<Captured>>,
    /// When its process exited, once [`Run::Exited`] said so: its output is read until
    /// [`Runner::drain`] past this.
    exited: Option<Instant>,
    /// Stopped by `fail-fast`: its end is no failure of its own.
    killed: bool,
}

/// What one executable printed outside libtest's JSON, and its stderr, as read so far: its
/// readers fill it, and the runner takes it once both pipes closed, or once the drain's
/// deadline passed with a pipe still open.
#[derive(Default)]
struct Captured {
    out: String,
    err: Vec<u8>,
}

impl Captured {
    /// Its stdout's lines, then its stderr.
    fn text(&self) -> String {
        let mut text = self.out.clone(); // alloc: one executable's output, once it ended
        text.push_str(&String::from_utf8_lossy(&self.err));
        text
    }
}

/// Starts each test executable as soon as it is ready, a few at a time, and says how each
/// went; its result is the command's exit code.
struct Runner {
    args: Vec<OsString>,
    fail_fast: bool,
    spec_flag: bool,
    to_screen: Sender<Msg>,
    to_runner: Sender<Run>,
    binaries: Vec<Binary>,
    seen: HashSet<PathBuf, Global>,
    queued: Vec<u32>,
    running: Vec<Running>,
    slots: usize,
    /// The `RUST_TEST_THREADS` each executable is started with, a share of blaze's core budget;
    /// `None` when the command set `RUST_TEST_THREADS` itself, which then reaches each as set.
    threads: Option<usize>,
    /// The failures, as (order, exit code): the first in cargo's order is the exit code.
    failed: Vec<(String, Option<i32>)>,
    stopping: bool,
    build: Option<bool>,
    /// Whether the warnings it prints are colored.
    color: bool,
    /// How long an executable's output is read after it exited: [`DRAIN`].
    drain: Duration,
}

impl Runner {
    fn new(
        asked: &Invocation,
        cores: blaze_core::cores::Cores,
        to_screen: Sender<Msg>,
        to_runner: Sender<Run>,
        spec_flag: bool,
        color: bool,
    ) -> Runner {
        let told = std::env::var_os(RUST_TEST_THREADS).is_some();
        Runner::of(
            asked.test_args(),
            asked.flags.fail_fast,
            cores,
            told,
            to_screen,
            to_runner,
            spec_flag,
            color,
        )
    }

    /// `told`: whether the command's environment sets `RUST_TEST_THREADS` itself.
    ///
    /// The budget is split two ways at once, so the threads running stay near it: a quarter of
    /// it in executables at once ([`blaze_core::cores::NESTED`]), each with libtest told the
    /// share that leaves it (`RUST_TEST_THREADS`), where libtest alone would start a thread per
    /// core in each. Executables are not run one at a time with the whole budget: they start as
    /// each is linked, while the build goes on, and most are small; a few at once keeps the
    /// cores busy while one long suite runs. A command's own `RUST_TEST_THREADS` or
    /// `--test-threads` wins.
    #[expect(clippy::too_many_arguments, reason = "the runner's whole setup, made once a command")]
    fn of(
        args: Vec<OsString>,
        fail_fast: bool,
        cores: blaze_core::cores::Cores,
        told: bool,
        to_screen: Sender<Msg>,
        to_runner: Sender<Run>,
        spec_flag: bool,
        color: bool,
    ) -> Runner {
        let slots = cores.split(blaze_core::cores::NESTED).get();
        Runner {
            args,
            fail_fast,
            spec_flag,
            to_screen,
            to_runner,
            binaries: Vec::new(),     // alloc: one a command
            seen: HashSet::default(), // alloc: one a command
            queued: Vec::new(),       // alloc: one a command
            running: Vec::new(),      // alloc: one a command
            slots,
            threads: (!told).then(|| cores.split(slots).get()),
            failed: Vec::new(), // alloc: one a command
            stopping: false,
            build: None,
            color,
            drain: DRAIN,
        }
    }

    fn say(&self, event: Event) {
        let _ = self.to_screen.send(Msg::View(event));
    }

    fn run(mut self, gets: &Receiver<Run>) -> i32 {
        loop {
            if self.build.is_some() && self.queued.is_empty() && self.running.is_empty() {
                return self.code();
            }
            // The first exited executable whose output is still open: its drain's deadline.
            let deadline = self
                .running
                .iter()
                .filter_map(|it| it.exited)
                .min()
                .map(|exited| exited + self.drain);
            let got = {
                let _waiting = tracing::info_span!(
                    target: "blaze",
                    "test.runner.wait",
                    running = self.running.len(),
                    queued = self.queued.len(),
                    build_over = self.build.is_some(),
                )
                .entered();
                match deadline {
                    Some(deadline) => gets.recv_deadline(deadline),
                    None => gets.recv().map_err(|_| RecvTimeoutError::Disconnected),
                }
            };
            let run = match got {
                Ok(run) => run,
                Err(RecvTimeoutError::Timeout) => {
                    self.undrained();
                    self.start();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // Every sender gone with work left: the caller's thread ended early.
                    self.stop_all();
                    return Exit::Failed.code();
                }
            };
            match run {
                Run::Ready(artifact) => self.ready(artifact),
                Run::Drained { binary } => self.ended(binary, false),
                Run::Exited { binary, waited } => self.exited(binary, waited),
                Run::BuildOver { ok, artifacts } => {
                    let (tests, docs) = test::executables(&artifacts);
                    for artifact in tests.into_iter().chain(docs) {
                        // clone: the artifact outlives the build's list
                        self.ready(artifact.clone());
                    }
                    self.build = Some(ok);
                }
            }
            self.start();
        }
    }

    /// 0, the first failing executable's code in cargo's order (fail-fast), or 101.
    fn code(&mut self) -> i32 {
        if self.build == Some(false) {
            return Exit::Failed.code();
        }
        if self.failed.is_empty() {
            return 0;
        }
        if !self.fail_fast {
            return Exit::Failed.code();
        }
        self.failed.sort();
        self.failed[0].1.unwrap_or(Exit::Failed.code())
    }

    fn ready(&mut self, artifact: Artifact) {
        let Some(executable) = artifact.executable.as_deref() else { return };
        if !self.seen.insert(PathBuf::from(executable)) {
            return;
        }
        let doc = artifact.kind == ArtifactKind::Doctest;
        let kind = if doc { Kind::Doctest } else { Kind::Test };
        let (name, version) =
            (say::package_name(&artifact.package), say::package_version(&artifact.package));
        let rank = match artifact.target.kind {
            TargetKind::Lib => 0,
            TargetKind::Bin => 1,
            TargetKind::Test => 2,
            TargetKind::Bench => 3,
            TargetKind::Example => 4,
            TargetKind::CustomBuild => 5,
        };
        // cargo's: every unit test executable by package and target, then the doc tests.
        // alloc: one key and label an executable
        let order = format!(
            "{} {name} {version} {} {rank} {}",
            u8::from(doc),
            artifact.package,
            artifact.target.name
        );
        let label = match (doc, artifact.target.kind) {
            (true, _) => format!("{name} doctests"),
            (false, TargetKind::Lib) => format!("{name} lib"),
            (false, _) => format!("{name} {}", artifact.target.name),
        };
        let rerun = test::rerun(&artifact, kind, self.spec_flag);
        let binary = self.binaries.len() as u32;
        self.say(Event::BinaryQueued { binary, label: label.clone(), order: order.clone(), rerun });
        let json = artifact.harness && !doc;
        self.binaries.push(Binary { artifact, order, label, json, began: None });
        if self.stopping {
            self.say(Event::BinaryEnded { binary, end: BinaryEnd::Stopped, output: String::new() });
            return;
        }
        self.queued.push(binary);
    }

    /// Starts the queued, first in cargo's order, while there are slots.
    fn start(&mut self) {
        while !self.stopping && self.running.len() < self.slots && !self.queued.is_empty() {
            let at = (0..self.queued.len())
                .min_by(|a, b| {
                    self.binaries[self.queued[*a] as usize]
                        .order
                        .cmp(&self.binaries[self.queued[*b] as usize].order)
                })
                .unwrap_or(0);
            let binary = self.queued.remove(at);
            self.spawn(binary);
        }
    }

    fn spawn(&mut self, binary: u32) {
        let it = &self.binaries[binary as usize];
        let mut args = self.args.clone();
        if it.json {
            args.extend(
                ["--format", "json", "-Z", "unstable-options", "--report-time"].map(OsString::from),
            );
        }
        let executable =
            PathBuf::from(it.artifact.executable.as_deref().unwrap_or(&it.artifact.path));
        let root = std::path::Path::new(&it.artifact.manifest_path)
            .parent()
            .map_or_else(PathBuf::new, std::path::Path::to_owned);
        let exec = Exec::of(&it.artifact, executable, args, root);
        let mut command = exec.command();
        if let Some(threads) = self.threads {
            command.env(RUST_TEST_THREADS, threads.to_string()); // alloc: one an executable run
        }
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let json = it.json;
        let name = it.label.clone();
        self.say(Event::BinaryStarted { binary });
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let why =
                    format!("could not execute process {} (never executed): {error}", exec.shown());
                let order = self.binaries[binary as usize].order.clone();
                self.say(Event::BinaryEnded {
                    binary,
                    end: BinaryEnd::NeverRan { why },
                    output: String::new(),
                });
                self.failed(order, None, binary);
                return;
            }
        };
        self.binaries[binary as usize].began = Some(Instant::now());
        if !json {
            self.say(Event::SuiteTotal { binary, tests: 1 });
            self.say(Event::TestStarted { binary, name });
        }
        let mut child = child;
        let pid = child.id();
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let captured = Arc::new(Lock::new("live.run.captured", Captured::default())); // alloc: one an executable run
        let read = {
            let (to_screen, to_runner, captured) =
                (self.to_screen.clone(), self.to_runner.clone(), captured.clone());
            let label = self.binaries[binary as usize].label.clone();
            thread::Builder::new().name("blaze test output".into()).spawn(move || {
                read(binary, &label, json, stdout, stderr, &captured, &to_screen, &to_runner)
            })
        };
        // Its exit is waited for apart from its output: a process it started may hold its
        // pipes open long after it exited.
        let wait = read.and_then(|_| {
            let to_runner = self.to_runner.clone();
            let label = self.binaries[binary as usize].label.clone();
            thread::Builder::new().name("blaze test wait".into()).spawn(move || {
                let _waiting =
                    tracing::info_span!(target: "blaze", "test.wait_exit", test = %label, pid)
                        .entered();
                let waited =
                    crate::raw::process::wait_exited(pid).map_err(|error| error.to_string());
                let _ = to_runner.send(Run::Exited { binary, waited });
            })
        });
        if let Err(error) = wait {
            let _ = child.kill();
            let _ = child.wait();
            let why =
                format!("could not read the output of {} or wait for it: {error}", exec.shown());
            let order = self.binaries[binary as usize].order.clone();
            self.say(Event::BinaryEnded {
                binary,
                end: BinaryEnd::NeverRan { why },
                output: String::new(),
            });
            self.failed(order, None, binary);
            return;
        }
        self.running.push(Running { binary, child, pid, captured, exited: None, killed: false });
    }

    /// One's process exited: its output is read for at most [`Runner::drain`] more.
    fn exited(&mut self, binary: u32, waited: Result<(), String>) {
        // Gone: its output was drained first, and it is reaped.
        let Some(running) = self.running.iter_mut().find(|it| it.binary == binary) else { return };
        match waited {
            Ok(()) => {
                running.exited.get_or_insert_with(Instant::now);
            }
            Err(why) => {
                // Its output is read to its end, with no deadline: only its pipes' closing
                // ends it now.
                let label = &self.binaries[binary as usize].label;
                let what = format!(
                    "could not wait for {label} (pid {}) to exit: {why}; its end is now when its stdout and \
                     stderr close, which a process it started and left running can hold off",
                    running.pid
                );
                let _ =
                    self.to_screen.send(Msg::View(Event::Said(say::warning(&what, self.color))));
            }
        }
    }

    /// Ends each executable that exited [`Runner::drain`] ago with a pipe still open, with
    /// what it printed until now, and says so.
    fn undrained(&mut self) {
        let now = Instant::now();
        let drain = self.drain;
        // alloc: the few executables whose drain ended at once
        let late: Vec<u32> = self
            .running
            .iter()
            .filter(|it| it.exited.is_some_and(|exited| now >= exited + drain))
            .map(|it| it.binary)
            .collect();
        for binary in late {
            self.ended(binary, true);
        }
    }

    /// One's output is drained, or `held` past its drain's deadline: its status, then what it
    /// ended as.
    fn ended(&mut self, binary: u32, held: bool) {
        let Some(at) = self.running.iter().position(|it| it.binary == binary) else { return };
        let mut running = self.running.swap_remove(at);
        let output = running.captured.lock().text();
        if held {
            let label = &self.binaries[binary as usize].label;
            let what = format!(
                "{label} (pid {}) exited, but a process it started still holds its stdout or stderr open; \
                 its output was read for {} ms after it exited, and what that process prints from now on \
                 is not shown",
                running.pid,
                self.drain.as_millis()
            );
            let _ = self.to_screen.send(Msg::View(Event::Said(say::warning(&what, self.color))));
        }
        let ended = running.child.wait().map(say::Ended::of);
        let it = &self.binaries[binary as usize];
        let took = it.began.map(|began| began.elapsed());
        let (order, json, name) = (it.order.clone(), it.json, it.label.clone());
        if running.killed {
            self.say(Event::BinaryEnded { binary, end: BinaryEnd::Stopped, output });
            return;
        }
        let ended = match ended {
            Ok(ended) => ended,
            Err(error) => {
                let why = format!("could not wait for {name}: {error}");
                self.say(Event::BinaryEnded { binary, end: BinaryEnd::NeverRan { why }, output });
                self.failed(order, None, binary);
                return;
            }
        };
        let ok = ended.success();
        if !json {
            let verdict = if ok { Verdict::Passed } else { Verdict::Failed };
            // clone: the box and the binary's end both show it
            self.say(Event::TestEnded { binary, name, verdict, took, output: output.clone() });
        }
        if ok {
            self.say(Event::BinaryEnded { binary, end: BinaryEnd::Passed, output });
            return;
        }
        let code = match ended {
            say::Ended::Code(code) => Some(code),
            say::Ended::Signal(_) => None,
        };
        self.say(Event::BinaryEnded {
            binary,
            end: BinaryEnd::Failed { status: say::ended(ended) },
            output,
        });
        self.failed(order, code, binary);
    }

    /// One failed: kept for the exit code, and under fail-fast the rest stopped.
    fn failed(&mut self, order: String, code: Option<i32>, binary: u32) {
        self.failed.push((order, code));
        if !self.fail_fast || self.stopping {
            return;
        }
        self.stopping = true;
        let label = &self.binaries[binary as usize].label;
        let why = format!("{label} failed; stopping the rest (`--fail-fast`; without it they run)");
        self.say(Event::Stopping { why });
        self.stop_all();
    }

    fn stop_all(&mut self) {
        for binary in std::mem::take(&mut self.queued) {
            self.say(Event::BinaryEnded { binary, end: BinaryEnd::Stopped, output: String::new() });
        }
        for running in &mut self.running {
            running.killed = true;
            let _ = running.child.kill();
        }
    }
}

/// Reads one executable's stdout (libtest's JSON when `json`) into the view's events, and its
/// stderr beside it, each into `captured` as it comes, then tells the runner both closed.
#[expect(clippy::too_many_arguments, reason = "one thread's whole input, given once")]
fn read(
    binary: u32,
    label: &str,
    json: bool,
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
    captured: &Arc<Lock<Captured>>,
    to_screen: &Sender<Msg>,
    to_runner: &Sender<Run>,
) {
    let _reading =
        tracing::info_span!(target: "blaze", "test.read_output", test = %label).entered();
    let errors = stderr.map(|mut stderr| {
        let (captured, into) = (captured.clone(), captured.clone());
        let read = thread::Builder::new().name("blaze test stderr".into()).spawn(move || {
            let captured = into;
            let mut chunk = [0; 8192];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) => return,
                    Ok(n) => captured.lock().err.extend_from_slice(&chunk[..n]),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        let said =
                            format!("\nblaze stopped reading this executable's stderr: {error}\n");
                        captured.lock().err.extend_from_slice(said.as_bytes());
                        return;
                    }
                }
            }
        });
        (read, captured)
    });
    if let Some(stdout) = stdout {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            match json.then(|| said(binary, &line)).flatten() {
                Some(event) => {
                    let _ = to_screen.send(Msg::View(event));
                }
                None if json && line.trim_start().starts_with('{') => {}
                None => {
                    let mut captured = captured.lock();
                    captured.out.push_str(&line);
                    captured.out.push('\n');
                }
            }
        }
    }
    match errors {
        Some((Ok(read), _)) => {
            let _ = read.join();
        }
        Some((Err(error), captured)) => {
            let said = format!("blaze could not read this executable's stderr: {error}\n");
            captured.lock().err.extend_from_slice(said.as_bytes());
        }
        None => {}
    }
    let _ = to_runner.send(Run::Drained { binary });
}

/// One line of libtest's JSON as the view's event: a suite's start, a test's start, end or
/// "running for over 60 seconds". Anything else is `None`.
fn said(binary: u32, line: &str) -> Option<Event> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let kind = value.get("type")?.as_str()?;
    let event = value.get("event")?.as_str()?;
    let name = || value.get("name").and_then(|it| it.as_str()).map(str::to_owned);
    match (kind, event) {
        ("suite", "started") => {
            let tests = value.get("test_count").and_then(serde_json::Value::as_u64).unwrap_or(0);
            Some(Event::SuiteTotal { binary, tests: u32::try_from(tests).unwrap_or(u32::MAX) })
        }
        ("test", "started") => Some(Event::TestStarted { binary, name: name()? }),
        ("test", "timeout") => Some(Event::TestSlow { binary, name: name()? }),
        ("test", "ok" | "failed" | "ignored") => {
            let verdict = match event {
                "ok" => Verdict::Passed,
                "failed" => Verdict::Failed,
                _ => Verdict::Ignored,
            };
            let took = value
                .get("exec_time")
                .and_then(serde_json::Value::as_f64)
                .and_then(|it| Duration::try_from_secs_f64(it).ok());
            let mut output =
                value.get("stdout").and_then(|it| it.as_str()).unwrap_or_default().to_owned();
            if let Some(message) = value.get("message").and_then(|it| it.as_str())
                && verdict == Verdict::Failed
            {
                output.push_str(message);
                output.push('\n');
            }
            Some(Event::TestEnded { binary, name: name()?, verdict, took, output })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use blaze_ui::test_view::{BinaryEnd, Event, Verdict};
    use crossbeam_channel::unbounded;

    use super::{Msg, Run, Runner, said};

    /// A test executable that starts a process which inherits its stdout and outlives it ends
    /// when it exits, its output read until then: the runner does not wait for the pipe to
    /// close, which happens only when that process ends, and says that it stopped reading.
    #[cfg(unix)]
    #[test]
    fn a_test_whose_child_outlives_it_ends_when_it_exits() {
        let dir = tempfile::tempdir().unwrap();
        let mut artifact =
            crate::cli::run::tests::artifact("calc", "test", "it", true, Some("deps/it-0"));
        artifact.harness = false;
        artifact.manifest_path = dir.path().join("Cargo.toml").to_string_lossy().into_owned();
        // The executable is `sh`'s `$0`: the script starts a process holding its stdout, says
        // its pid, and exits 0.
        artifact.runner =
            ["/bin/sh", "-c", "sleep 60 & echo \"child $!\""].map(str::to_owned).to_vec();
        let (to_screen, screen_gets) = unbounded::<Msg>();
        let (to_runner, runner_gets) = unbounded::<Run>();
        let one = std::num::NonZero::<usize>::MIN;
        let cores =
            blaze_core::cores::Cores::resolve(blaze_core::cores::Setting::Exactly(one), one);
        let mut runner =
            Runner::of(Vec::new(), true, cores, true, to_screen, to_runner.clone(), true, false);
        runner.drain = Duration::from_millis(300);
        to_runner.send(Run::BuildOver { ok: true, artifacts: vec![artifact] }).unwrap();
        let (done, finished) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || done.send(runner.run(&runner_gets)));
        let code = finished.recv_timeout(Duration::from_secs(20));

        let events: Vec<Event> = screen_gets
            .try_iter()
            .filter_map(|msg| match msg {
                Msg::View(event) => Some(event),
                Msg::Now(_) | Msg::Said(_) | Msg::Finish => None,
            })
            .collect();
        let output = events.iter().find_map(|event| match event {
            Event::BinaryEnded { end: BinaryEnd::Passed, output, .. } => Some(output.clone()),
            _ => None,
        });
        // The process left running is ended here, whatever the runner did.
        if let Some(pid) = output.as_deref().and_then(|it| it.trim().strip_prefix("child ")) {
            let _ = std::process::Command::new("kill").arg(pid).status();
        }
        assert_eq!(code, Ok(0), "the runner did not end within 20 s: {events:#?}");
        assert!(output.as_deref().is_some_and(|it| it.starts_with("child ")), "{events:#?}");
        let warned = events.iter().any(|event| matches!(event, Event::Said(text) if text.contains("calc it (pid ") && text.contains("still holds its stdout or stderr open")));
        assert!(warned, "{events:#?}");
    }

    #[test]
    fn libtest_json_becomes_the_views_events() {
        let started = said(3, r#"{ "type": "suite", "event": "started", "test_count": 2 }"#);
        assert!(matches!(started, Some(Event::SuiteTotal { binary: 3, tests: 2 })));
        let failed = said(
            3,
            r#"{ "type": "test", "name": "a::b", "event": "failed", "exec_time": 0.25, "stdout": "boom\n" }"#,
        );
        let Some(Event::TestEnded { name, verdict, took, output, .. }) = failed else {
            panic!("{failed:?}")
        };
        assert_eq!((name.as_str(), verdict, output.as_str()), ("a::b", Verdict::Failed, "boom\n"));
        assert_eq!(took, Some(std::time::Duration::from_millis(250)));
        assert!(said(3, r#"{ "type": "suite", "event": "ok", "passed": 2 }"#).is_none());
        assert!(said(3, "running 2 tests").is_none());
    }
}
