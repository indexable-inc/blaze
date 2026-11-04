//! `blaze test` on a terminal: one live view of the build and the tests together, built the
//! way Elm builds a program. The model, its update and its view are blaze-ui's sans-IO
//! [`TestView`] (`apply` is the update, fed [`Event`]s with the time injected; `render` is the
//! view, into a ratatui buffer; `take_above` and `summary` are the effects, lines to print
//! above the live region). This file is only the runtime around it, three threads that do the
//! IO and nothing else:
//!
//! - the caller's thread talks to the daemon and turns each response into an [`Event`] (and a
//!   [`Run::Ready`] for each test executable as it is linked, [`Response::TestReady`]);
//! - the runner starts each test executable as soon as it is ready, a few at a time, while
//!   the build goes on, and reads libtest's JSON into more [`Event`]s;
//! - the screen owns the terminal: an inline viewport ([`Viewport::Inline`], never the
//!   alternate screen), redrawn on a tick, with what the view hands back printed above it, so
//!   the failures and the summary stay in the scrollback and the live region is cleared.
//!
//! Off a terminal (a pipe, CI, `--message-format json`, `-q`, `-v`, `BLAZE_UI=plain`) none of
//! this runs: [`wanted`] says no and `cli::drive` prints its plain lines as before. The exit
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
    allow(dead_code, unused_imports, reason = "without `native`, only `wanted` and the refusing `drive` are reached")
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
use blaze_ui::test_view::{BinaryEnd, Event, Paint, TestView, Verdict};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use parking_lot::Mutex;
use ratatui::text::Line;
#[cfg(feature = "native")]
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend};

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

/// How often the region is redrawn: the spinner's frame.
const TICK: Duration = Duration::from_millis(80);
/// The tallest the live region gets.
const REGION: u16 = 10;
/// The smallest terminal the view is drawn on; a smaller one gets the plain lines.
const COLUMNS: u16 = 40;
const ROWS: u16 = 8;
/// libtest's arguments that change what it prints, which the view can't read: with any of
/// them the run is plain.
/// How long a test executable's output is read after its process exited. Whatever it wrote
/// is in the pipe by then, so only a process it started and left running, which inherited its
/// stdout or stderr, holds the pipe open past this; the runner then stops reading and says so.
const DRAIN: Duration = Duration::from_secs(2);
const PLAIN_ARGS: [&str; 9] = ["--format", "--list", "--nocapture", "--no-capture", "--logfile", "-q", "--quiet", "-h", "--help"];

/// Whether `blaze test` draws its live view: a test run (not `--no-run`, `--reached`, a bench)
/// whose stdout and stderr are terminals big enough, printed for a person (not JSON, `-q` or
/// `-v`), with libtest's own output left as the view reads it, and not on CI ([`ci`]), whose
/// workers may hand it a pseudo-terminal whose log is read as text.
pub fn wanted(asked: &Invocation, shown: Shown) -> bool {
    let plain_ui = std::env::var_os("BLAZE_UI").is_some_and(|it| it == "plain");
    let dumb = std::env::var_os("TERM").is_some_and(|it| it == "dumb");
    if asked.command() != Command::Test
        || ci(|name| std::env::var_os(name))
        || asked.flags.no_run
        || asked.flags.reached
        || asked.flags.ub
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
    let reads_args = asked.program.iter().all(|arg| {
        let arg = arg.to_string_lossy();
        !PLAIN_ARGS.iter().any(|plain| arg == *plain || arg.starts_with(&format!("{plain}=")))
    });
    reads_args && fits()
}

/// Whether the terminal is big enough for the view.
#[cfg(feature = "native")]
fn fits() -> bool {
    ratatui::crossterm::terminal::size().is_ok_and(|(columns, rows)| columns >= COLUMNS && rows >= ROWS)
}

/// Without `native` there is no terminal library to ask the size, and the view is what this run
/// asks for whatever it is: [`drive`] refuses it, naming the feature.
#[cfg(not(feature = "native"))]
fn fits() -> bool {
    true
}

/// What a blaze built without blaze-cargo's `native` feature says when a run asks for the view.
#[cfg(not(feature = "native"))]
const NO_TUI: &str = "`blaze test` on a terminal draws a live view, and this blaze is built without blaze-cargo's \
                      `native` feature (the seed profile, which has no terminal library); `BLAZE_UI=plain` prints plain lines";

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

/// Whether this runs on CI: `CI` set to anything but empty, `0` or `false` (GitHub Actions,
/// GitLab, Buildkite, CircleCI, Travis and most others set it), or `TF_BUILD` set (Azure
/// Pipelines, which sets no `CI`).
fn ci(var: impl Fn(&str) -> Option<OsString>) -> bool {
    let ci = var("CI").is_some_and(|it| !(it.is_empty() || it == "0" || it.eq_ignore_ascii_case("false")));
    ci || var("TF_BUILD").is_some_and(|it| !it.is_empty())
}

/// What the screen is sent: the view's events in the order they happened, then its end.
enum Msg {
    View(Event),
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

/// The build and its tests, drawn live: what `cli::drive` does for `test` on a terminal.
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
    let terminal = open().map_err(|error| {
        // alloc: the error the command ends with, once
        io::Error::other(format!(
            "the live test view could not start on this terminal ({error}); `BLAZE_UI=plain` prints plain lines"
        ))
    })?;
    // Ctrl-C is a flag the screen reads: it puts the terminal back, then ends the command.
    // The test executables share the terminal's process group, so they get it too.
    signals::catch();
    let (to_screen, screen_gets) = unbounded::<Msg>();
    let (to_runner, runner_gets) = unbounded::<Run>();
    let screen = thread::Builder::new()
        .name("blaze test screen".into())
        .spawn(move || Screen { terminal, view: TestView::new(paint), broken: false, started }.run(&screen_gets))?;
    let runner = {
        let to_screen = to_screen.clone();
        let to_runner = to_runner.clone();
        // The workspace's member count comes with `Built`, after the first executables run, so
        // "to rerun pass" always names the package: `-p calc --lib` reruns it in any workspace.
        let cores = blaze_core::cores::get().map_err(io::Error::other)?;
        let runner = Runner::new(asked, cores, to_screen, to_runner, true, shown.color);
        thread::Builder::new().name("blaze test runner".into()).spawn(move || runner.run(&runner_gets))?
    };

    let (mut out, mut err) = (Above::new(to_screen.clone()), Above::new(to_screen.clone()));
    let mut printer = Printer::new(shown, &place.root, &mut out, &mut err);
    printer.place(std::env::var("HOME").ok(), super::observe::last_failure());
    let mut failed_units: HashSet<u32, Global> = HashSet::default(); // alloc: one a command
    let mut unwritten = None;
    let view = |event: Event| {
        let _ = to_screen.send(Msg::View(event));
    };
    let called = api.call(request, |response, more| {
        if let ResponseRef::Work(worked) = &response {
            super::client_rows::heard(worked.build_id);
        }
        let printed = match response {
            // The build row stands for cargo's `Compiling` lines.
            ResponseRef::Unit(_) => Ok(()),
            ResponseRef::UnitBegan(unit) => {
                view(Event::UnitBegan { id: unit.id, label: unit_label(say::package_name(unit.package_id), unit.target.kind, unit.target.name, unit.test) });
                Ok(())
            }
            ResponseRef::UnitEnded(unit) => {
                view(Event::UnitEnded { id: unit.id });
                printer.on(ResponseRef::UnitEnded(unit))
            }
            ResponseRef::Diagnostic(diagnostic) => {
                if diagnostic.level == Level::Error
                    && let Some(unit) = &diagnostic.unit
                    && failed_units.insert(unit.id)
                {
                    view(Event::UnitFailed { id: unit.id });
                }
                printer.on(ResponseRef::Diagnostic(diagnostic))
            }
            ResponseRef::Report { unit, report } => {
                if report.severity == blaze_api::Severity::Error
                    && let Some(unit) = &unit
                    && failed_units.insert(unit.id)
                {
                    view(Event::UnitFailed { id: unit.id });
                }
                printer.on(ResponseRef::Report { unit, report })
            }
                        ResponseRef::TestReady(artifact) => {
                let _ = to_runner.send(Run::Ready(artifact));
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
                Some(Ok(died)) => format!("{error}: {died}"),
                Some(Err(why)) => format!("{error}, and what came of the daemon is not known: {why:#}"),
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
    let _ = to_runner.send(Run::BuildOver { ok, artifacts });
    drop(to_runner);
    let code = runner.join().unwrap_or(Exit::Failed.code());
    let _ = to_screen.send(Msg::Finish);
    drop(to_screen);
    let shown = screen.join().map_err(|_| io::Error::other("the live test view's screen thread panicked"))?;
    shown?;
    if let Some(error) = unwritten {
        return Err(error);
    }
    Ok(Then::Exit(code))
}

/// The terminal with its inline region, on stderr as cargo's own lines are.
#[cfg(feature = "native")]
fn open() -> io::Result<Terminal<CrosstermBackend<io::Stderr>>> {
    let (_, rows) = ratatui::crossterm::terminal::size()?;
    let height = REGION.min(rows.saturating_sub(4)).max(3);
    Terminal::with_options(CrosstermBackend::new(io::stderr()), TerminalOptions { viewport: Viewport::Inline(height) })
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
        let _ = self.to.send(Msg::View(Event::Said(text)));
        Ok(())
    }
}

/// The runtime's screen: each message is the view's update, each tick its view, and what the
/// view hands back is printed above the region.
#[cfg(feature = "native")]
struct Screen {
    terminal: Terminal<CrosstermBackend<io::Stderr>>,
    view: TestView,
    /// Drawing panicked once: the region is gone, and what the view hands back is printed as
    /// plain lines from then on.
    broken: bool,
    started: Instant,
}

#[cfg(feature = "native")]
impl Screen {
    fn run(mut self, gets: &Receiver<Msg>) -> io::Result<()> {
        loop {
            let mut finish = false;
            match gets.recv_timeout(TICK) {
                Ok(Msg::View(event)) => self.view.apply(self.started.elapsed(), event),
                Ok(Msg::Finish) | Err(RecvTimeoutError::Disconnected) => finish = true,
                Err(RecvTimeoutError::Timeout) => {}
            }
            // A burst is one redraw.
            while let Ok(msg) = gets.try_recv() {
                match msg {
                    Msg::View(event) => self.view.apply(self.started.elapsed(), event),
                    Msg::Finish => finish = true,
                }
            }
            if signals::signalled() {
                self.interrupted();
            }
            self.view.tick(self.started.elapsed());
            self.frame();
            if finish {
                return self.finish();
            }
        }
    }

    /// One frame: the region fitted to the terminal, what the view printed above it, the
    /// region redrawn. A panic in drawing is said once and ends the region, not the run.
    fn frame(&mut self) {
        if self.broken {
            let width = ratatui::crossterm::terminal::size().map_or(80, |(columns, _)| columns);
            let lines = self.view.take_above(width);
            plain(&lines);
            return;
        }
        let drawn = catch_unwind(AssertUnwindSafe(|| -> io::Result<()> {
            self.terminal.autoresize()?;
            let width = self.terminal.size()?.width;
            let lines = self.view.take_above(width);
            above(&mut self.terminal, &lines)?;
            let view = &self.view;
            self.terminal.draw(|frame| {
                let area = frame.area();
                view.render(area, frame.buffer_mut());
            })?;
            Ok(())
        }));
        let why = match drawn {
            Ok(Ok(())) => return,
            Ok(Err(error)) => error.to_string(),
            Err(panic) => panic
                .downcast_ref::<&str>()
                .map(|it| (*it).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic with no message".to_owned()), // alloc: once, as the view breaks
        };
        self.broken = true;
        let _ = self.terminal.clear();
        let _ = self.terminal.show_cursor();
        let said = format!("the live test view stopped drawing: {why}; the rest is printed as plain lines");
        eprint!("{}", say::error(&said, false));
    }

    /// The run is over: the summary printed above, the region cleared, the cursor where the
    /// region began, so the shell's prompt follows the summary.
    fn finish(mut self) -> io::Result<()> {
        let width = match self.broken {
            true => ratatui::crossterm::terminal::size().map_or(80, |(columns, _)| columns),
            false => self.terminal.size()?.width,
        };
        let summary = self.view.summary(width);
        if self.broken {
            plain(&summary);
            return Ok(());
        }
        above(&mut self.terminal, &summary)?;
        self.restore()
    }

    fn restore(&mut self) -> io::Result<()> {
        self.terminal.clear()?;
        let top = self.terminal.get_frame().area().y;
        self.terminal.set_cursor_position((0, top))?;
        self.terminal.show_cursor()?;
        io::stderr().flush()
    }

    /// Ctrl-C: the region cleared and the cursor back, then the command ends as a shell's
    /// interrupted one does.
    fn interrupted(&mut self) -> ! {
        if !self.broken {
            let _ = self.restore();
        }
        let _ = io::stderr().write_all(b"interrupted\n");
        std::process::exit(blaze_core::exit::INTERRUPTED);
    }
}

/// `lines` printed above the region, into the scrollback.
#[cfg(feature = "native")]
fn above(terminal: &mut Terminal<CrosstermBackend<io::Stderr>>, lines: &[Line<'static>]) -> io::Result<()> {
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

/// `lines` as text, once the region is gone.
#[cfg(feature = "native")]
fn plain(lines: &[Line<'static>]) {
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
    captured: Arc<Mutex<Captured>>,
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
        Runner::of(asked.test_args(), asked.flags.fail_fast, cores, told, to_screen, to_runner, spec_flag, color)
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
            binaries: Vec::new(),      // alloc: one a command
            seen: HashSet::default(), // alloc: one a command
            queued: Vec::new(),        // alloc: one a command
            running: Vec::new(),       // alloc: one a command
            slots,
            threads: (!told).then(|| cores.split(slots).get()),
            failed: Vec::new(),        // alloc: one a command
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
            let deadline = self.running.iter().filter_map(|it| it.exited).min().map(|exited| exited + self.drain);
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
        let (name, version) = (say::package_name(&artifact.package), say::package_version(&artifact.package));
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
        let order = format!("{} {name} {version} {} {rank} {}", u8::from(doc), artifact.package, artifact.target.name);
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
                .min_by(|a, b| self.binaries[self.queued[*a] as usize].order.cmp(&self.binaries[self.queued[*b] as usize].order))
                .unwrap_or(0);
            let binary = self.queued.remove(at);
            self.spawn(binary);
        }
    }

    fn spawn(&mut self, binary: u32) {
        let it = &self.binaries[binary as usize];
        let mut args = self.args.clone();
        if it.json {
            args.extend(["--format", "json", "-Z", "unstable-options", "--report-time"].map(OsString::from));
        }
        let executable = PathBuf::from(it.artifact.executable.as_deref().unwrap_or(&it.artifact.path));
        let root = std::path::Path::new(&it.artifact.manifest_path).parent().map_or_else(PathBuf::new, std::path::Path::to_owned);
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
                let why = format!("could not execute process {} (never executed): {error}", exec.shown());
                let order = self.binaries[binary as usize].order.clone();
                self.say(Event::BinaryEnded { binary, end: BinaryEnd::NeverRan { why }, output: String::new() });
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
        let captured = Arc::new(Mutex::new(Captured::default())); // alloc: one an executable run
        let read = {
            let (to_screen, to_runner, captured) = (self.to_screen.clone(), self.to_runner.clone(), captured.clone());
            let label = self.binaries[binary as usize].label.clone();
            thread::Builder::new()
                .name("blaze test output".into())
                .spawn(move || read(binary, &label, json, stdout, stderr, &captured, &to_screen, &to_runner))
        };
        // Its exit is waited for apart from its output: a process it started may hold its
        // pipes open long after it exited.
        let wait = read.and_then(|_| {
            let to_runner = self.to_runner.clone();
            let label = self.binaries[binary as usize].label.clone();
            thread::Builder::new().name("blaze test wait".into()).spawn(move || {
                let _waiting = tracing::info_span!(target: "blaze", "test.wait_exit", test = %label, pid).entered();
                let waited = crate::raw::process::wait_exited(pid).map_err(|error| error.to_string());
                let _ = to_runner.send(Run::Exited { binary, waited });
            })
        });
        if let Err(error) = wait {
            let _ = child.kill();
            let _ = child.wait();
            let why = format!("could not read the output of {} or wait for it: {error}", exec.shown());
            let order = self.binaries[binary as usize].order.clone();
            self.say(Event::BinaryEnded { binary, end: BinaryEnd::NeverRan { why }, output: String::new() });
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
                let _ = self.to_screen.send(Msg::View(Event::Said(say::warning(&what, self.color))));
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
        self.say(Event::BinaryEnded { binary, end: BinaryEnd::Failed { status: say::ended(ended) }, output });
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
    captured: &Arc<Mutex<Captured>>,
    to_screen: &Sender<Msg>,
    to_runner: &Sender<Run>,
) {
    let _reading = tracing::info_span!(target: "blaze", "test.read_output", test = %label).entered();
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
                        let said = format!("\nblaze stopped reading this executable's stderr: {error}\n");
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
            let took = value.get("exec_time").and_then(serde_json::Value::as_f64).and_then(|it| Duration::try_from_secs_f64(it).ok());
            let mut output = value.get("stdout").and_then(|it| it.as_str()).unwrap_or_default().to_owned();
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
    use std::{ffi::OsString, time::Duration};

    use blaze_ui::test_view::{BinaryEnd, Event, Verdict};
    use crossbeam_channel::unbounded;

    use super::{Msg, Run, Runner, ci, said};

    #[test]
    fn ci_is_told_by_its_variables() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| pairs.iter().find(|it| it.0 == name).map(|it| OsString::from(it.1))
        };
        assert!(ci(env(&[("CI", "true")])));
        assert!(ci(env(&[("CI", "1")])));
        assert!(ci(env(&[("TF_BUILD", "True")])));
        assert!(!ci(env(&[])));
        assert!(!ci(env(&[("CI", "false")])));
        assert!(!ci(env(&[("CI", "0")])));
        assert!(!ci(env(&[("CI", "")])));
    }

    /// A test executable that starts a process which inherits its stdout and outlives it ends
    /// when it exits, its output read until then: the runner does not wait for the pipe to
    /// close, which happens only when that process ends, and says that it stopped reading.
    #[cfg(unix)]
    #[test]
    fn a_test_whose_child_outlives_it_ends_when_it_exits() {
        let dir = tempfile::tempdir().unwrap();
        let mut artifact = crate::cli::run::tests::artifact("calc", "test", "it", true, Some("deps/it-0"));
        artifact.harness = false;
        artifact.manifest_path = dir.path().join("Cargo.toml").to_string_lossy().into_owned();
        // The executable is `sh`'s `$0`: the script starts a process holding its stdout, says
        // its pid, and exits 0.
        artifact.runner = ["/bin/sh", "-c", "sleep 60 & echo \"child $!\""].map(str::to_owned).to_vec();
        let (to_screen, screen_gets) = unbounded::<Msg>();
        let (to_runner, runner_gets) = unbounded::<Run>();
        let one = std::num::NonZero::<usize>::MIN;
        let cores = blaze_core::cores::Cores::resolve(blaze_core::cores::Setting::Exactly(one), one);
        let mut runner = Runner::of(Vec::new(), true, cores, true, to_screen, to_runner.clone(), true, false);
        runner.drain = Duration::from_millis(300);
        to_runner.send(Run::BuildOver { ok: true, artifacts: vec![artifact] }).unwrap();
        let (done, finished) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || done.send(runner.run(&runner_gets)));
        let code = finished.recv_timeout(Duration::from_secs(20));

        let events: Vec<Event> = screen_gets.try_iter().filter_map(|msg| match msg {
            Msg::View(event) => Some(event),
            Msg::Finish => None,
        }).collect();
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
        let failed = said(3, r#"{ "type": "test", "name": "a::b", "event": "failed", "exec_time": 0.25, "stdout": "boom\n" }"#);
        let Some(Event::TestEnded { name, verdict, took, output, .. }) = failed else { panic!("{failed:?}") };
        assert_eq!((name.as_str(), verdict, output.as_str()), ("a::b", Verdict::Failed, "boom\n"));
        assert_eq!(took, Some(std::time::Duration::from_millis(250)));
        assert!(said(3, r#"{ "type": "suite", "event": "ok", "passed": 2 }"#).is_none());
        assert!(said(3, "running 2 tests").is_none());
    }
}
