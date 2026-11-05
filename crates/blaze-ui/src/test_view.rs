//! `blaze test`'s live view on a terminal: a sans-IO state machine fed what the build and the
//! test executables say ([`Event`]), with the time injected (`now`, since the command began),
//! drawn into a ratatui [`Buffer`]. It owns no terminal, thread or clock: the driver
//! (blaze-cargo's `cli/live.rs`) feeds it, draws [`TestView::render`] into an inline viewport
//! on a tick, prints [`TestView::take_above`] above that viewport as it comes (each failure,
//! boxed, and the compiler's own lines), and [`TestView::summary`] last, so what stays in the
//! scrollback is the failures and the summary, never the live region.
//!
//! Under the build's board ([`TestView::under_board`], how `blaze test` draws it) the header,
//! the build row and the held row go, the board says them, and the gauge waits for a total.
//! Alone, the region, at most a dozen lines:
//!
//! ```text
//!  ⠹ blaze test  compiling and testing                                         12.4s
//!  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━───  ✔ 405 passed  ✘ 2 failed  ⊘ 5 ignored  412 of 730+ tests run  ~6s left
//!  build  ▰▰▰▰▰▰▰▱▱▱  38/52  hir-ty lib 9.1s · blaze-cargo lib test 4.2s
//!  bins   ✔ 4  ◷ 2  ⋯ 3   blaze-cargo lib · hir-ty lib
//!  ⠹ hir_ty::tests::traits::big_one                          hir-ty lib    4.21s
//!  ⠹ blaze_cargo::serve::tests::cold_build              blaze-cargo lib    2.03s
//!    … 9 more running
//! ```
//!
//! Every list it prints that stays (the summary's slowest tests and failures) is sorted by
//! the executable's order (the driver's key: package, kind, target) and the test's name, never
//! by when a result came, so two runs with the same results print the same summary. Drawing
//! never panics on any size, 0 by 0 included: every write is clipped to the area.

use std::time::Duration;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar;

#[cfg(test)]
mod tests;

/// Whether the view paints in color: `NO_COLOR`, `--color never` and a terminal that has none
/// take [`Paint::Plain`], which keeps bold, dim and reverse but no color.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Paint {
    Color,
    Plain,
}

/// What became of one test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Passed,
    Failed,
    Ignored,
}

/// How a test executable ended.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BinaryEnd {
    /// It exited 0.
    Passed,
    /// It exited otherwise: the status as the command says it (`exit status: 101`, `signal: 11
    /// (SIGSEGV)`).
    Failed { status: String },
    /// The command stopped it: a test elsewhere failed under `--fail-fast`.
    Stopped,
    /// It could not be started: why.
    NeverRan { why: String },
}

/// What the view is told, in the order it happened.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
    /// A unit's compile began: its id (dense, the daemon's) and how it is named.
    UnitBegan {
        id: u32,
        label: String,
    },
    /// A unit's compile ended.
    UnitEnded {
        id: u32,
    },
    /// A unit said an error: it failed, and so does every test executable after it.
    UnitFailed {
        id: u32,
    },
    /// The build is over.
    BuildEnded {
        ok: bool,
    },
    /// A test executable or doc test never built: its unit, the unit whose failure held it
    /// back (itself when its own compile failed), and why that one failed. The summary lists it.
    Unbuilt {
        unit: String,
        behind: String,
        why: String,
    },
    /// A test executable is known and waits for its turn. `binary` is the driver's dense index;
    /// `order` sorts it among the others as cargo orders them; `rerun` is what reruns it.
    BinaryQueued {
        binary: u32,
        label: String,
        order: String,
        rerun: String,
    },
    BinaryStarted {
        binary: u32,
    },
    /// libtest's count of the tests the executable runs (its suite's `test_count`).
    SuiteTotal {
        binary: u32,
        tests: u32,
    },
    TestStarted {
        binary: u32,
        name: String,
    },
    /// libtest warned that the test has run a long time (its `timeout` event).
    TestSlow {
        binary: u32,
        name: String,
    },
    /// A test ended: libtest's time for it when it gave one, and its captured output (shown
    /// when it failed).
    TestEnded {
        binary: u32,
        name: String,
        verdict: Verdict,
        took: Option<Duration>,
        output: String,
    },
    /// The executable ended; `output` is what it said outside any test (stderr, stray stdout),
    /// shown when it failed with no test that did.
    BinaryEnded {
        binary: u32,
        end: BinaryEnd,
        output: String,
    },
    /// The command stops starting tests: why.
    Stopping {
        why: String,
    },
    /// Lines to print above the region as they are: the compiler's diagnostics and cargo's
    /// status lines, with their ANSI colors.
    Said(String),
    /// What the build is doing, about once a second (the daemon's `Response::Doing`): what it
    /// waits on while no unit compiles; the worker pool's busy and idle workers and what the
    /// longest busy one runs; and what each unit under way is inside, by unit id. Each replaces
    /// the last.
    Doing {
        on: Option<String>,
        busy: u32,
        idle: u32,
        running: Option<String>,
        units: Vec<(u32, String)>,
    },
}

/// The most lines of a failure's captured output a box shows: its last ones.
const OUTPUT_LINES: usize = 24;

/// How many of the slowest tests the summary names.
const SLOWEST: usize = 5;

/// How many failures the summary lists by name; the rest are counted.
const FAILURES: usize = 20;

/// A test is slow, and its row is marked, past this.
const SLOW: Duration = Duration::from_secs(5);

/// A unit open this long has what it is inside said after it.
const DOING_AFTER: Duration = Duration::from_secs(3);

/// A build running this long has the hint to `blaze explore` and `blaze debug live` under it.
const HINT_AFTER: Duration = Duration::from_secs(10);
const HINT: &str = " what each step does: `blaze explore`; live spans: `blaze debug live`";

/// How many of the steps a busy worker holds up are named; the rest are counted.
const BLOCKED_NAMED: usize = 2;

/// One frame of the spinner every this.
const SPIN: Duration = Duration::from_millis(80);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A gauge's eighths, for a bar that moves by less than a cell.
const EIGHTHS: [&str; 8] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

struct Unit {
    id: u32,
    label: String,
    since: Duration,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Queued,
    Running,
    Passed,
    Failed,
    Stopped,
}

struct Binary {
    label: String,
    order: String,
    rerun: String,
    state: State,
    /// libtest's count, once it said it.
    total: Option<u32>,
    failed: u32,
    since: Duration,
}

struct Running {
    binary: u32,
    name: String,
    since: Duration,
    slow: bool,
}

struct Timed {
    took: Duration,
    binary: u32,
    name: String,
}

struct Failure {
    binary: u32,
    name: String,
    location: Option<String>,
}

/// The build's last [`Event::Doing`].
struct Doing {
    on: Option<String>,
    busy: u32,
    idle: u32,
    running: Option<String>,
    units: Vec<(u32, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Build {
    Running,
    Ended { ok: bool, at: Duration },
}

/// The live view's state: see the module's docs.
pub struct TestView {
    paint: Paint,
    now: Duration,
    build: Build,
    units_began: u32,
    units_ended: u32,
    units_failed: u32,
    compiling: Vec<Unit>,
    binaries: Vec<Binary>,
    running: Vec<Running>,
    passed: u32,
    failed: u32,
    ignored: u32,
    /// When the first test started: the ETA's rate is from here.
    first_test: Option<Duration>,
    slowest: Vec<Timed>,
    failures: Vec<Failure>,
    stopping: Option<String>,
    /// The test units that never built, as (unit, behind, why), in the order said.
    unbuilt: Vec<(String, String, String)>,
    /// What the build is doing, as it last said; `None` before it said and once it ended.
    doing: Option<Doing>,
    /// Drawn under the build's board ([`crate::board::Board`]), which says the header, the
    /// build's progress and what holds it: the region is then only the tests' own rows.
    under_board: bool,
    /// Lines made to print above the region and not yet taken, each a block of styled lines
    /// to wrap to the width it is taken at.
    above: Vec<Above>,
}

/// A block for above the region.
enum Above {
    /// Text with ANSI escapes, printed as it is.
    Said(String),
    /// A failure's box.
    Failed { binary: u32, name: String, took: Option<Duration>, output: String },
    /// An executable that failed with no test that did (a crash, a harness of its own, a
    /// refused start).
    Crashed { binary: u32, status: String, output: String },
}

impl TestView {
    pub fn new(paint: Paint) -> TestView {
        TestView {
            paint,
            now: Duration::ZERO,
            build: Build::Running,
            units_began: 0,
            units_ended: 0,
            units_failed: 0,
            compiling: Vec::new(), // alloc: grows to the units under way at once
            binaries: Vec::new(),  // alloc: one a test executable
            running: Vec::new(),   // alloc: grows to the tests under way at once
            passed: 0,
            failed: 0,
            ignored: 0,
            first_test: None,
            slowest: Vec::with_capacity(SLOWEST + 1), // alloc: once a run
            failures: Vec::new(),                     // alloc: one a failure
            stopping: None,
            unbuilt: Vec::new(), // alloc: none until a test unit did not build
            doing: None,
            under_board: false,
            above: Vec::new(),   // alloc: drained at every draw
        }
    }

    /// The view drawn under the build's board: no header, build row or held row of its own,
    /// which the board says, and no gauge before a test has a total.
    pub fn under_board(mut self) -> TestView {
        self.under_board = true;
        self
    }

    /// Time passes with nothing said: the spinners and times move.
    pub fn tick(&mut self, now: Duration) {
        self.now = self.now.max(now);
    }

    /// Whether the run has failed so far: a test, an executable or the build.
    pub fn failing(&self) -> bool {
        self.failed > 0
            || self.units_failed > 0
            || matches!(self.build, Build::Ended { ok: false, .. })
            || self.binaries.iter().any(|it| it.state == State::Failed)
    }

    pub fn apply(&mut self, now: Duration, event: Event) {
        self.tick(now);
        let now = self.now;
        match event {
            Event::UnitBegan { id, label } => {
                self.units_began += 1;
                self.compiling.push(Unit { id, label, since: now });
            }
            Event::UnitEnded { id } => {
                if let Some(at) = self.compiling.iter().position(|it| it.id == id) {
                    self.compiling.remove(at);
                    self.units_ended += 1;
                }
            }
            Event::UnitFailed { id } => {
                if let Some(at) = self.compiling.iter().position(|it| it.id == id) {
                    self.compiling.remove(at);
                    self.units_ended += 1;
                }
                self.units_failed += 1;
            }
            Event::BuildEnded { ok } => {
                self.build = Build::Ended { ok: ok && self.units_failed == 0, at: now };
                self.compiling.clear();
                self.doing = None;
            }
            Event::Doing { on, busy, idle, running, units } => {
                if self.build == Build::Running {
                    self.doing = Some(Doing { on, busy, idle, running, units });
                }
            }
            Event::Unbuilt { unit, behind, why } => self.unbuilt.push((unit, behind, why)),
            Event::BinaryQueued { binary, label, order, rerun } => {
                let at = binary as usize;
                // The driver's indexes are dense from 0; one past is a row made for it.
                while self.binaries.len() <= at {
                    self.binaries.push(Binary {
                        label: String::new(), // alloc: none
                        order: String::new(), // alloc: none
                        rerun: String::new(), // alloc: none
                        state: State::Queued,
                        total: None,
                        failed: 0,
                        since: now,
                    });
                }
                let row = &mut self.binaries[at];
                (row.label, row.order, row.rerun) = (label, order, rerun);
            }
            Event::BinaryStarted { binary } => {
                if let Some(it) = self.binaries.get_mut(binary as usize) {
                    it.state = State::Running;
                    it.since = now;
                }
            }
            Event::SuiteTotal { binary, tests } => {
                if let Some(it) = self.binaries.get_mut(binary as usize) {
                    it.total = Some(tests);
                }
            }
            Event::TestStarted { binary, name } => {
                self.first_test.get_or_insert(now);
                self.running.push(Running { binary, name, since: now, slow: false });
            }
            Event::TestSlow { binary, name } => {
                if let Some(it) =
                    self.running.iter_mut().find(|it| it.binary == binary && it.name == name)
                {
                    it.slow = true;
                }
            }
            Event::TestEnded { binary, name, verdict, took, output } => {
                let started =
                    self.running.iter().position(|it| it.binary == binary && it.name == name);
                let since = started.map(|at| self.running.remove(at).since);
                let took = took.or_else(|| since.map(|it| now.saturating_sub(it)));
                match verdict {
                    Verdict::Passed => self.passed += 1,
                    Verdict::Ignored => self.ignored += 1,
                    Verdict::Failed => {
                        self.failed += 1;
                        if let Some(it) = self.binaries.get_mut(binary as usize) {
                            it.failed += 1;
                        }
                        let location = location(&output);
                        // clone: the summary keeps the name, the box prints it
                        self.failures.push(Failure { binary, name: name.clone(), location });
                        self.above.push(Above::Failed { binary, name: name.clone(), took, output }); // clone: as above
                    }
                }
                if let (Some(took), Verdict::Passed | Verdict::Failed) = (took, verdict) {
                    self.timed(Timed { took, binary, name });
                }
            }
            Event::BinaryEnded { binary, end, output } => {
                self.running.retain(|it| it.binary != binary);
                let Some(row) = self.binaries.get_mut(binary as usize) else { return };
                let failed_tests = row.failed;
                row.state = match &end {
                    BinaryEnd::Passed => State::Passed,
                    BinaryEnd::Stopped => State::Stopped,
                    BinaryEnd::Failed { .. } | BinaryEnd::NeverRan { .. } => State::Failed,
                };
                match end {
                    BinaryEnd::Failed { status } if failed_tests == 0 => {
                        self.above.push(Above::Crashed { binary, status, output });
                    }
                    BinaryEnd::NeverRan { why } => {
                        self.above.push(Above::Crashed { binary, status: why, output })
                    }
                    BinaryEnd::Passed | BinaryEnd::Stopped | BinaryEnd::Failed { .. } => {}
                }
            }
            Event::Stopping { why } => {
                self.stopping.get_or_insert(why);
            }
            Event::Said(text) => self.above.push(Above::Said(text)),
        }
    }

    /// Keeps `timed` if it is among the [`SLOWEST`], in the summary's order.
    fn timed(&mut self, timed: Timed) {
        let key = |it: &Timed, binaries: &[Binary]| {
            let order = binaries.get(it.binary as usize).map_or("", |b| b.order.as_str());
            (std::cmp::Reverse(it.took), order.to_owned(), it.name.clone()) // alloc: a key, compared a few times a test
        };
        let new = key(&timed, &self.binaries);
        let at = self
            .slowest
            .iter()
            .position(|it| key(it, &self.binaries) > new)
            .unwrap_or(self.slowest.len());
        if at < SLOWEST {
            self.slowest.insert(at, timed);
            self.slowest.truncate(SLOWEST);
        }
    }

    fn style(&self, color: Color) -> Style {
        match self.paint {
            Paint::Color => Style::new().fg(color),
            Paint::Plain => Style::new(),
        }
    }

    fn dim(&self) -> Style {
        match self.paint {
            Paint::Color => Style::new().fg(Color::DarkGray),
            Paint::Plain => Style::new().add_modifier(Modifier::DIM),
        }
    }

    fn label(&self, binary: u32) -> &str {
        self.binaries.get(binary as usize).map_or("?", |it| it.label.as_str())
    }

    fn spinner(&self) -> &'static str {
        let frame = (self.now.as_millis() / SPIN.as_millis()) as usize;
        SPINNER[frame % SPINNER.len()]
    }

    fn done(&self) -> u32 {
        self.passed + self.failed + self.ignored
    }

    /// The tests known to run: each executable's count, once it said it; and whether more may
    /// come (an executable that has not said, or a build still under way).
    fn total(&self) -> (u32, bool) {
        let known = self.binaries.iter().filter_map(|it| it.total).sum::<u32>();
        let more = self.build == Build::Running
            || self
                .binaries
                .iter()
                .any(|it| it.total.is_none() && matches!(it.state, State::Queued | State::Running));
        (known.max(self.done()), more)
    }

    /// The time left, once every executable said its count: the rate since the first test.
    fn eta(&self) -> Option<Duration> {
        let (total, more) = self.total();
        let done = self.done();
        let since = self.first_test?;
        if more || done == 0 || done >= total {
            return None;
        }
        let spent = self.now.saturating_sub(since).as_secs_f64();
        let left = spent * f64::from(total - done) / f64::from(done);
        left.is_finite().then(|| Duration::from_secs_f64(left.max(0.0)))
    }

    /// The region, drawn into `area` of `buf`; clipped to it, whatever its size.
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(buf.area);
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = area.width as usize;
        let mut rows: Vec<Line<'static>> = Vec::with_capacity(area.height as usize); // alloc: one a draw
        if !self.under_board {
            rows.push(self.header(width));
        }
        if !self.under_board || self.total().0 > 0 {
            rows.push(self.gauge(width));
        }
        if !self.under_board {
            rows.push(self.build_row(width));
            if let Some(row) = self.held_row() {
                rows.push(row);
            }
        }
        if !self.binaries.is_empty() || self.build != Build::Running {
            rows.push(self.binaries_row(width));
        }
        let hint = self.build == Build::Running && self.now >= HINT_AFTER;
        let room = (area.height as usize).saturating_sub(rows.len() + usize::from(hint));
        let mut running: Vec<&Running> = self.running.iter().collect(); // alloc: one a draw
        // The oldest first: the slowest under way leads, ties by name so a draw is stable.
        running.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| a.name.cmp(&b.name)));
        let shown = match running.len() > room {
            true => room.saturating_sub(1),
            false => running.len(),
        };
        for (at, it) in running.iter().take(shown).enumerate() {
            rows.push(self.running_row(it, at == 0, width));
        }
        if running.len() > shown && room > 0 {
            let more = running.len() - shown;
            rows.push(Line::from(vec![Span::styled(
                format!("   … {more} more running"),
                self.dim(),
            )])); // alloc: a row
        }
        if hint {
            rows.push(Line::from(vec![Span::styled(HINT, self.dim())]));
        }
        for (y, row) in (area.y..area.bottom()).zip(&rows) {
            buf.set_line(area.x, y, row, area.width);
        }
    }

    fn header(&self, width: usize) -> Line<'static> {
        let accent = self.style(Color::Cyan).add_modifier(Modifier::BOLD);
        // "compiling" only while a unit compiles: before one has begun, or with none under way,
        // the build is waiting (on a download, a lowering, its plan), or just started.
        let compiling = !self.compiling.is_empty();
        let waiting = match self.doing.as_ref().is_some_and(|it| it.on.is_some()) {
            true => "waiting",
            false => "starting",
        };
        let phase = match (self.build, self.binaries.iter().any(|it| it.state == State::Running)) {
            (Build::Running, true) if compiling => "compiling and testing",
            (Build::Running, true) => "testing",
            (Build::Running, false) if compiling => "compiling",
            (Build::Running, false) if self.units_began > 0 => "building",
            (Build::Running, false) => waiting,
            (Build::Ended { ok: false, .. }, _) if self.running.is_empty() => "build failed",
            (Build::Ended { .. }, true) => "testing",
            (Build::Ended { .. }, false) => "finishing",
        };
        let elapsed = duration(self.now);
        let left = vec![
            Span::styled(format!(" {} ", self.spinner()), accent), // alloc: a row
            Span::styled("blaze test", accent),
            Span::raw("  "),
            Span::styled(phase, self.dim()),
        ];
        right_aligned(
            left,
            vec![Span::styled(elapsed, Style::new().add_modifier(Modifier::BOLD))],
            width,
        )
    }

    fn gauge(&self, width: usize) -> Line<'static> {
        let (total, more) = self.total();
        let mut counts = vec![
            Span::raw("  "),
            Span::styled(format!("✔ {} passed", self.passed), self.style(Color::Green)), // alloc: a row
        ];
        if self.failed > 0 {
            counts.push(Span::raw("  "));
            counts.push(Span::styled(
                format!("✘ {} failed", self.failed),
                self.style(Color::Red).add_modifier(Modifier::BOLD),
            )); // alloc: a row
        }
        if self.ignored > 0 {
            counts.push(Span::raw("  "));
            counts.push(Span::styled(format!("⊘ {} ignored", self.ignored), self.style(Color::Yellow))); // alloc: a row
        }
        // `+`: more executables are still to say how many tests they hold.
        let plus = if more { "+" } else { "" };
        counts.push(Span::styled(format!("  {} of {total}{plus} tests run", self.done()), self.dim())); // alloc: a row
        if let Some(eta) = self.eta() {
            counts.push(Span::styled(format!("  ~{} left", duration(eta)), self.dim())); // alloc: a row
        }
        let counted: usize = counts.iter().map(Span::width).sum();
        let bar = width.saturating_sub(counted + 1).min(60);
        let mut line = vec![Span::raw(" ")];
        line.extend(self.stacked(bar, total));
        line.extend(counts);
        Line::from(line)
    }

    /// A bar of `cells` cells: passed, failed and ignored in their colors, the rest dim.
    fn stacked(&self, cells: usize, total: u32) -> Vec<Span<'static>> {
        let mut spans = Vec::with_capacity(4); // alloc: a row
        if cells == 0 {
            return spans;
        }
        let total = f64::from(total.max(1));
        let cell = |n: u32| ((f64::from(n) / total) * cells as f64).round() as usize;
        let failed = match self.failed {
            0 => 0,
            n => cell(n).max(1),
        };
        let ignored = cell(self.ignored);
        let passed = cell(self.passed).min(cells.saturating_sub(failed + ignored));
        let rest = cells.saturating_sub(passed + failed + ignored);
        let ok = if self.failed > 0 { Color::Red } else { Color::Green };
        spans.push(Span::styled("━".repeat(passed), self.style(ok))); // alloc: a row
        spans.push(Span::styled("━".repeat(failed), self.style(Color::Red))); // alloc: a row
        spans.push(Span::styled("━".repeat(ignored), self.style(Color::Yellow))); // alloc: a row
        spans.push(Span::styled("─".repeat(rest), self.dim())); // alloc: a row
        spans
    }

    fn build_row(&self, width: usize) -> Line<'static> {
        let mut line = vec![Span::styled(" build  ", self.dim())];
        match self.build {
            Build::Ended { ok: true, at } => {
                line.push(Span::styled("✔", self.style(Color::Green)));
                line.push(Span::raw(format!(" {} units", self.units_ended))); // alloc: a row
                line.push(Span::styled(format!("  {}", duration(at)), self.dim())); // alloc: a row
            }
            Build::Ended { ok: false, at } => {
                line.push(Span::styled("✘", self.style(Color::Red).add_modifier(Modifier::BOLD)));
                let failed = match self.units_failed {
                    0 => String::from(" the build failed"), // alloc: a row
                    1 => String::from(" 1 unit failed"),    // alloc: a row
                    n => format!(" {n} units failed"),      // alloc: a row
                };
                line.push(Span::styled(failed, self.style(Color::Red)));
                line.push(Span::styled(
                    format!(" · {} built  {}", self.units_ended, duration(at)),
                    self.dim(),
                )); // alloc: a row
            }
            Build::Running => {
                line.extend(self.bar(10, self.units_ended, self.units_began.max(1), Color::Blue));
                line.push(Span::styled(
                    format!(" {}/{}  ", self.units_ended, self.units_began),
                    self.dim(),
                )); // alloc: a row
                // No unit under way: what the build waits on instead (a download's progress, the
                // span it is in), clipped to the row.
                if self.compiling.is_empty() {
                    match self.doing.as_ref().and_then(|it| it.on.as_deref()) {
                        Some(on) => line.push(Span::styled(on.to_owned(), self.dim())), // alloc: a row's text
                        None if self.units_began == 0 => line.push(Span::styled("starting", self.dim())),
                        None => {}
                    }
                    return Line::from(line);
                }
                let doing = |id: u32| {
                    let units = self.doing.as_ref().map_or(&[][..], |it| &it.units[..]);
                    units.iter().find(|(of, _)| *of == id).map(|(_, it)| it.as_str())
                };
                let mut units: Vec<&Unit> = self.compiling.iter().collect(); // alloc: one a draw
                units.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| a.label.cmp(&b.label)));
                let mut used: usize = line.iter().map(Span::width).sum();
                for (at, unit) in units.iter().enumerate() {
                    let open = self.now.saturating_sub(unit.since);
                    let took = duration(open);
                    let sep = if at == 0 { "" } else { " · " };
                    // A unit open a while says what it is inside: `libc build script: waiting
                    // on the download of llvm.tar.xz (1.2 of 2.0 GB) 14.0s`.
                    let inside = doing(unit.id).filter(|it| open >= DOING_AFTER && !it.is_empty());
                    let said = inside.map_or(0, |it| 2 + text_width(it));
                    let need = sep.len() + text_width(&unit.label) + said + 1 + took.len();
                    if used + need > width {
                        let left = units.len() - at;
                        if used + 8 <= width {
                            line.push(Span::styled(format!(" +{left}"), self.dim())); // alloc: a row
                        }
                        break;
                    }
                    used += need;
                    line.push(Span::styled(sep, self.dim()));
                    line.push(Span::raw(unit.label.clone())); // clone: a row's text
                    if let Some(inside) = inside {
                        line.push(Span::styled(format!(": {inside}"), self.dim())); // alloc: a row
                    }
                    line.push(Span::styled(format!(" {took}"), self.dim())); // alloc: a row
                }
            }
        }
        Line::from(line)
    }

    /// While most workers are idle and steps under way wait behind what the busy ones run, the
    /// row that says so: `held   1 worker busy, 15 idle: waiting on codegen `std::fmt::write`
    /// in std (4 min 30 s) · blocks std build script, libc build script, +79`. A step blocks
    /// when it is under way with no thread working on it (the daemon names what each one that
    /// has a thread is inside). Clipped to the row by the draw.
    fn held_row(&self) -> Option<Line<'static>> {
        let doing = self.doing.as_ref()?;
        let running = doing.running.as_deref()?;
        if self.build != Build::Running || doing.busy == 0 || doing.idle <= doing.busy {
            return None;
        }
        let mut blocked: Vec<&Unit> = self
            .compiling
            .iter()
            .filter(|unit| !doing.units.iter().any(|(id, _)| *id == unit.id))
            .collect(); // alloc: one a draw
        if blocked.is_empty() {
            return None;
        }
        blocked.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| a.label.cmp(&b.label)));
        let workers = match doing.busy {
            1 => "worker",
            _ => "workers",
        };
        let mut said = format!(
            "{} {workers} busy, {} idle: waiting on {running} · blocks ",
            doing.busy, doing.idle
        ); // alloc: a row
        for (at, unit) in blocked.iter().take(BLOCKED_NAMED).enumerate() {
            if at > 0 {
                said.push_str(", ");
            }
            said.push_str(&unit.label);
        }
        if blocked.len() > BLOCKED_NAMED {
            said.push_str(&format!(", +{}", blocked.len() - BLOCKED_NAMED)); // alloc: a row
        }
        Some(Line::from(vec![
            Span::styled(" held   ", self.dim()),
            Span::styled(said, self.style(Color::Yellow)),
        ]))
    }

    /// A bar of `cells` cells filled `done` of `of`, by eighths.
    fn bar(&self, cells: usize, done: u32, of: u32, color: Color) -> Vec<Span<'static>> {
        let eighths =
            ((f64::from(done.min(of)) / f64::from(of.max(1))) * (cells * 8) as f64) as usize;
        let (full, part) = (eighths / 8, eighths % 8);
        let mut filled = "█".repeat(full); // alloc: a row
        if full < cells && part > 0 {
            filled.push_str(EIGHTHS[part]);
        }
        let used = full + usize::from(full < cells && part > 0);
        vec![
            Span::styled(filled, self.style(color)),
            Span::styled("░".repeat(cells.saturating_sub(used)), self.dim()), // alloc: a row
        ]
    }

    fn binaries_row(&self, width: usize) -> Line<'static> {
        let count = |state: State| self.binaries.iter().filter(|it| it.state == state).count();
        let mut line = vec![Span::styled(" bins   ", self.dim())];
        if let Some(why) = &self.stopping {
            line.push(Span::styled("■ ", self.style(Color::Red).add_modifier(Modifier::BOLD)));
            line.push(Span::styled(fit(why, width.saturating_sub(10)), self.style(Color::Red))); // alloc: a row
            return Line::from(line);
        }
        let (passed, failed, running, queued) = (
            count(State::Passed),
            count(State::Failed),
            count(State::Running),
            count(State::Queued),
        );
        line.push(Span::styled(format!("✔ {passed}"), self.style(Color::Green))); // alloc: a row
        if failed > 0 {
            line.push(Span::styled(format!("  ✘ {failed}"), self.style(Color::Red))); // alloc: a row
        }
        line.push(Span::styled(format!("  ◷ {running}"), self.style(Color::Cyan))); // alloc: a row
        if queued > 0 {
            line.push(Span::styled(format!("  ⋯ {queued}"), self.dim())); // alloc: a row
        }
        let mut names: Vec<&Binary> =
            self.binaries.iter().filter(|it| it.state == State::Running).collect(); // alloc: one a draw
        names.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| a.order.cmp(&b.order)));
        let mut used: usize = line.iter().map(Span::width).sum::<usize>() + 3;
        line.push(Span::raw("   "));
        for (at, it) in names.iter().enumerate() {
            let sep = if at == 0 { "" } else { " · " };
            let need = sep.len() + text_width(&it.label);
            if used + need > width {
                break;
            }
            used += need;
            line.push(Span::styled(sep, self.dim()));
            line.push(Span::raw(it.label.clone())); // clone: a row's text
        }
        Line::from(line)
    }

    fn running_row(&self, it: &Running, first: bool, width: usize) -> Line<'static> {
        let took = self.now.saturating_sub(it.since);
        let slow = it.slow || took >= SLOW;
        let time_style = match (slow, first && took >= Duration::from_secs(1)) {
            (true, _) => self.style(Color::Yellow).add_modifier(Modifier::BOLD),
            (false, true) => Style::new().add_modifier(Modifier::BOLD),
            (false, false) => self.dim(),
        };
        let name_style = match slow {
            true => self.style(Color::Yellow),
            false => Style::new(),
        };
        let time = duration(took);
        let label = self.label(it.binary);
        let fixed = 4 + 2 + text_width(label) + 2 + time.len();
        let name = fit(&it.name, width.saturating_sub(fixed).max(8));
        let left = vec![
            Span::styled(format!(" {} ", self.spinner()), self.style(Color::Cyan)), // alloc: a row
            Span::styled(name, name_style),
        ];
        let right = vec![
            Span::styled(fit(label, width / 3), self.dim()), // alloc: a row
            Span::raw("  "),
            Span::styled(time, time_style),
        ];
        right_aligned(left, right, width)
    }

    /// What was made to print above the region since the last call, as lines `width` columns
    /// wide at most, in the order it came.
    pub fn take_above(&mut self, width: u16) -> Vec<Line<'static>> {
        let width = width as usize;
        let mut lines = Vec::new(); // alloc: drained by the driver at its next draw
        for block in std::mem::take(&mut self.above) {
            match block {
                Above::Said(text) => {
                    for line in text.strip_suffix('\n').unwrap_or(text.as_str()).split('\n') {
                        lines.extend(wrap(ansi(line, self.paint), width));
                    }
                }
                Above::Failed { binary, name, took, output } => {
                    let title = vec![
                        Span::styled(" ✘ FAIL ", self.badge(Color::Red)),
                        Span::raw(" "),
                        Span::styled(name, Style::new().add_modifier(Modifier::BOLD)),
                        Span::raw(" "),
                    ];
                    let took = took.map(duration).unwrap_or_default();
                    // The box's footer names the executable.
                    self.boxed(&mut lines, title, &took, self.label(binary), &output, width);
                }
                Above::Crashed { binary, status, output } => {
                    let title = vec![
                        Span::styled(" ✘ FAIL ", self.badge(Color::Red)),
                        Span::raw(" "),
                        Span::styled(
                            self.label(binary).to_owned(),
                            Style::new().add_modifier(Modifier::BOLD),
                        ), // alloc: a title
                        Span::raw(" "),
                    ];
                    let status = fit(&status, width / 2);
                    let footer = self
                        .binaries
                        .get(binary as usize)
                        .map_or_else(String::new, |it| format!("rerun: blaze test {}", it.rerun)); // alloc: a footer
                    self.boxed(&mut lines, title, &status, &footer, &output, width);
                }
            }
        }
        lines
    }

    fn badge(&self, color: Color) -> Style {
        match self.paint {
            Paint::Color => Style::new().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD),
            Paint::Plain => Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD),
        }
    }

    /// A box: `title` and `right` on its top edge, `footer` on its bottom, and `output` inside:
    /// the panic's place and message first, then the last [`OUTPUT_LINES`] of the rest.
    fn boxed(
        &self,
        lines: &mut Vec<Line<'static>>,
        title: Vec<Span<'static>>,
        right: &str,
        footer: &str,
        output: &str,
        width: usize,
    ) {
        let edge = self.style(Color::Red);
        if width < 16 {
            lines.extend(wrap(title, width));
            for line in output.lines().take(OUTPUT_LINES) {
                lines.extend(wrap(ansi(line, self.paint), width));
            }
            return;
        }
        let inner = width - 4;
        let mut top = vec![Span::styled("╭─", edge)];
        top.extend(title);
        let used: usize = top.iter().map(Span::width).sum::<usize>() + right.len() + 4;
        top.push(Span::styled("─".repeat(width.saturating_sub(used)), edge)); // alloc: a line
        if !right.is_empty() {
            top.push(Span::styled(format!(" {right} "), self.dim())); // alloc: a line
        }
        top.push(Span::styled("─╮", edge));
        lines.push(clip(top, width));
        let mut body: Vec<Vec<Span<'static>>> = Vec::new(); // alloc: one a box
        let (panic, rest) = split_panic(output);
        if let Some((place, message)) = panic {
            body.push(vec![
                Span::styled("▸ ", self.style(Color::Red)),
                Span::styled(place, self.style(Color::Cyan).add_modifier(Modifier::BOLD)),
            ]);
            for line in message.lines() {
                body.push(
                    ansi(line, self.paint)
                        .into_iter()
                        .map(|it| Span::styled(it.content, it.style.add_modifier(Modifier::BOLD)))
                        .collect(),
                );
            }
        }
        let rest: Vec<&str> = rest.lines().collect(); // alloc: one a box
        let rest = trim_blank(&rest);
        if !rest.is_empty() {
            let skipped = rest.len().saturating_sub(OUTPUT_LINES);
            let said = match skipped {
                0 => String::from("output"), // alloc: a line
                n => format!("output, last {OUTPUT_LINES} of {} lines", n + OUTPUT_LINES), // alloc: a line
            };
            if !body.is_empty() {
                body.push(Vec::new()); // alloc: none
            }
            body.push(vec![Span::styled(format!("┄ {said} ┄"), self.dim())]); // alloc: a line
            for line in &rest[skipped..] {
                body.push(ansi(line, self.paint));
            }
        }
        if body.is_empty() {
            body.push(vec![Span::styled("(no output)", self.dim())]);
        }
        for spans in body {
            for row in wrap(spans, inner) {
                let pad = inner.saturating_sub(row.width());
                let mut line = vec![Span::styled("│ ", edge)];
                line.extend(row.spans);
                line.push(Span::raw(" ".repeat(pad))); // alloc: a line
                line.push(Span::styled(" │", edge));
                lines.push(Line::from(line));
            }
        }
        let footer = fit(footer, inner.saturating_sub(4));
        let mut bottom = vec![Span::styled("╰─", edge)];
        if !footer.is_empty() {
            bottom.push(Span::styled(format!(" {footer} "), self.dim())); // alloc: a line
        }
        let used: usize = bottom.iter().map(Span::width).sum::<usize>() + 2;
        bottom.push(Span::styled("─".repeat(width.saturating_sub(used)), edge)); // alloc: a line
        bottom.push(Span::styled("─╯", edge));
        lines.push(clip(bottom, width));
    }

    /// The summary that stays once the region is cleared: the counts and time, the slowest
    /// tests, each failure and how to rerun it, and the verdict. Sorted, never by arrival.
    pub fn summary(&self, width: u16) -> Vec<Line<'static>> {
        let width = width as usize;
        let mut lines = Vec::new(); // alloc: once a run
        lines.push(Line::from(Span::styled("─".repeat(width.min(80)), self.dim()))); // alloc: once a run
        let mut counts = vec![
            Span::raw(" "),
            Span::styled(
                format!("✔ {} passed", self.passed),
                self.style(Color::Green).add_modifier(Modifier::BOLD),
            ), // alloc: once a run
        ];
        if self.failed > 0 {
            counts.push(Span::raw("   "));
            counts.push(Span::styled(
                format!("✘ {} failed", self.failed),
                self.style(Color::Red).add_modifier(Modifier::BOLD),
            )); // alloc: once a run
        }
        if self.ignored > 0 {
            counts.push(Span::raw("   "));
            counts.push(Span::styled(
                format!("⊘ {} ignored", self.ignored),
                self.style(Color::Yellow),
            )); // alloc: once a run
        }
        let binaries = self.binaries.len();
        let noun = if binaries == 1 { "binary" } else { "binaries" };
        let build = match self.build {
            Build::Ended { at, .. } => format!(" (build {})", duration(at)), // alloc: once a run
            Build::Running => String::new(),                                 // alloc: none
        };
        counts.push(Span::styled(
            format!(
                "   {} tests in {binaries} {noun} · {}{build}",
                self.done(),
                duration(self.now)
            ),
            self.dim(),
        )); // alloc: once a run
        lines.extend(wrap(counts, width));
        if !self.slowest.is_empty() && self.done() > 1 {
            lines.push(Line::default());
            for (at, it) in self.slowest.iter().enumerate() {
                let head = if at == 0 { " slowest " } else { "         " };
                let time = format!("{:>7}", duration(it.took)); // alloc: once a slow test
                let label = self.label(it.binary);
                let room = width.saturating_sub(head.len() + time.len() + 4 + text_width(label));
                let line = vec![
                    Span::styled(head, self.dim()),
                    Span::styled(time, self.style(Color::Yellow)),
                    Span::raw("  "),
                    Span::raw(fit(&it.name, room.max(12))),
                    Span::raw("  "),
                    Span::styled(label.to_owned(), self.dim()), // alloc: once a slow test
                ];
                lines.extend(wrap(line, width));
            }
        }
        let mut failures: Vec<&Failure> = self.failures.iter().collect(); // alloc: once a run
        failures.sort_by(|a, b| {
            self.order(a.binary).cmp(self.order(b.binary)).then_with(|| a.name.cmp(&b.name))
        });
        let crashed = self
            .binaries
            .iter()
            .enumerate()
            .filter(|(_, it)| it.state == State::Failed && it.failed == 0);
        let mut crashed: Vec<(usize, &Binary)> = crashed.collect(); // alloc: once a run
        crashed.sort_by(|a, b| a.1.order.cmp(&b.1.order));
        if !failures.is_empty() || !crashed.is_empty() {
            lines.push(Line::default());
            for (at, it) in failures.iter().take(FAILURES).enumerate() {
                let head = if at == 0 { " failed  " } else { "         " };
                let mut line = vec![
                    Span::styled(head, self.style(Color::Red).add_modifier(Modifier::BOLD)),
                    Span::styled("✘ ", self.style(Color::Red)),
                    Span::styled(it.name.clone(), Style::new().add_modifier(Modifier::BOLD)), // clone: once a failure
                    Span::styled(format!("  {}", self.label(it.binary)), self.dim()), // alloc: once a failure
                ];
                if let Some(place) = &it.location {
                    line.push(Span::styled(format!("  {place}"), self.style(Color::Cyan))); // alloc: once a failure
                }
                lines.extend(wrap(line, width));
            }
            if failures.len() > FAILURES {
                let more = vec![Span::styled(
                    format!("          … and {} more", failures.len() - FAILURES),
                    self.dim(),
                )]; // alloc: once a run
                lines.extend(wrap(more, width));
            }
            for (_, it) in &crashed {
                let line = vec![
                    Span::styled("         ", self.dim()),
                    Span::styled("✘ ", self.style(Color::Red)),
                    Span::styled(it.label.clone(), Style::new().add_modifier(Modifier::BOLD)), // clone: once a failure
                    Span::styled("  exited abnormally", self.dim()),
                ];
                lines.extend(wrap(line, width));
            }
            // Each executable with a failure, once, with what reruns it.
            let mut reruns: Vec<&Binary> = self
                .binaries
                .iter()
                .filter(|it| it.state == State::Failed || it.failed > 0)
                .collect(); // alloc: once a run
            reruns.sort_by(|a, b| a.order.cmp(&b.order));
            reruns.dedup_by(|a, b| a.rerun == b.rerun);
            for (at, it) in reruns.iter().enumerate() {
                let head = if at == 0 { " rerun   " } else { "         " };
                lines.extend(wrap(
                    vec![
                        Span::styled(head, self.dim()),
                        Span::styled(format!("blaze test {}", it.rerun), self.style(Color::Cyan)),
                    ], // alloc: once a failure
                    width,
                ));
            }
        }
        // Each test executable that never built, and the failure that held it back.
        if !self.unbuilt.is_empty() {
            lines.push(Line::default());
            for (at, (unit, behind, why)) in self.unbuilt.iter().enumerate() {
                let head = if at == 0 { " unbuilt " } else { "         " };
                let held = match behind == unit {
                    true => format!("  {why}"), // alloc: once an unbuilt test
                    false => format!("  behind {behind} ({why})"), // alloc: once an unbuilt test
                };
                let line = vec![
                    Span::styled(head, self.style(Color::Red).add_modifier(Modifier::BOLD)),
                    Span::styled("✘ ", self.style(Color::Red)),
                    Span::styled(unit.clone(), Style::new().add_modifier(Modifier::BOLD)), // clone: once an unbuilt test
                    Span::styled(held, self.dim()),
                ];
                lines.extend(wrap(line, width));
            }
        }
        lines.push(Line::default());
        let (word, color, said) = match (self.failing(), self.stopping.as_deref()) {
            (false, _) => (" PASS ", Color::Green, String::from("every test passed")), // alloc: once a run
            (true, Some(why)) => (" FAIL ", Color::Red, format!("stopped: {why}")), // alloc: once a run
            (true, None) => (" FAIL ", Color::Red, String::from("some tests failed")), // alloc: once a run
        };
        let said = match self.build {
            Build::Ended { ok: false, .. } if self.failed == 0 => {
                String::from("the build failed; the tests it built ran")
            } // alloc: once a run
            Build::Ended { .. } | Build::Running => said,
        };
        lines.extend(wrap(
            vec![
                Span::raw(" "),
                Span::styled(word, self.badge(color)),
                Span::raw(" "),
                Span::raw(said),
            ],
            width,
        ));
        lines
    }

    fn order(&self, binary: u32) -> &str {
        self.binaries.get(binary as usize).map_or("", |it| it.order.as_str())
    }
}

/// `left` then `right` at the end of a line `width` wide; `left` cut where they meet.
pub(crate) fn right_aligned(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
) -> Line<'static> {
    let right_width: usize = right.iter().map(Span::width).sum();
    let left = clip(left, width.saturating_sub(right_width + 1));
    let gap = width.saturating_sub(left.width() + right_width);
    let mut spans = left.spans;
    spans.push(Span::raw(" ".repeat(gap))); // alloc: a row
    spans.extend(right);
    Line::from(spans)
}

/// `spans` as one line, cut to `width` columns.
fn clip(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    wrap(spans, width).into_iter().next().unwrap_or_default()
}

/// A time as the view says it: `0.42s`, `12.4s`, `3m05s`.
pub fn duration(it: Duration) -> String {
    let secs = it.as_secs_f64();
    match it.as_secs() {
        0..10 => format!("{secs:.2}s"),            // alloc: a row's text
        10..60 => format!("{secs:.1}s"),           // alloc: a row's text
        n => format!("{}m{:02}s", n / 60, n % 60), // alloc: a row's text
    }
}

pub(crate) fn text_width(text: &str) -> usize {
    text.chars().map(|it| it.width().unwrap_or(0)).sum()
}

/// `text` in `width` columns at most: its end kept (a test's path is told apart by its last
/// segments), `…` in front of what was cut.
pub fn fit(text: &str, width: usize) -> String {
    if text_width(text) <= width {
        return text.to_owned(); // alloc: a row's text
    }
    if width == 0 {
        return String::new(); // alloc: none
    }
    let mut kept = 0;
    let mut start = text.len();
    for (at, it) in text.char_indices().rev() {
        let w = it.width().unwrap_or(0);
        if kept + w + 1 > width {
            break;
        }
        kept += w;
        start = at;
    }
    format!("…{}", &text[start..]) // alloc: a row's text
}

/// The place and message of a panic in a test's output, and the rest of it: libtest's
/// `thread 'name' panicked at src/lib.rs:3:5:` then the message, up to the `note:` or a blank
/// line.
fn split_panic(output: &str) -> (Option<(String, String)>, String) {
    let Some(start) = output.find("panicked at ") else { return (None, output.to_owned()) }; // alloc: one a box
    let line_start = output[..start].rfind('\n').map_or(0, |it| it + 1);
    let after = &output[start + "panicked at ".len()..];
    let (place, rest) = after.split_once('\n').unwrap_or((after, ""));
    let place = place.trim_end().trim_end_matches(':').to_owned(); // alloc: one a box
    let mut message = String::new(); // alloc: one a box
    let mut end = rest.len();
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let bare = line.trim_end();
        if bare.is_empty()
            || bare.starts_with("note: run with `RUST_BACKTRACE")
            || bare.starts_with("stack backtrace:")
        {
            end = offset;
            break;
        }
        message.push_str(line);
        offset += line.len();
    }
    let tail = &rest[end..];
    let tail = tail
        .split_inclusive('\n')
        .filter(|it| !it.trim_start().starts_with("note: run with `RUST_BACKTRACE"))
        .collect::<String>(); // alloc: one a box
    let mut rest = String::with_capacity(line_start + tail.len()); // alloc: one a box
    rest.push_str(&output[..line_start]);
    rest.push_str(&tail);
    (Some((place, message.trim_end().to_owned())), rest) // alloc: one a box
}

/// Where a test's output says it panicked, for the summary.
fn location(output: &str) -> Option<String> {
    split_panic(output).0.map(|(place, _)| place)
}

fn trim_blank<'a, 'b>(lines: &'b [&'a str]) -> &'b [&'a str] {
    let start = lines.iter().position(|it| !it.trim().is_empty()).unwrap_or(lines.len());
    let end = lines.iter().rposition(|it| !it.trim().is_empty()).map_or(start, |it| it + 1);
    &lines[start..end.max(start)]
}

/// `spans` wrapped into lines of `width` columns at most, by character (a terminal's width
/// is what is drawn; there is no word to keep whole in a path or a panic).
pub fn wrap(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(1); // alloc: a block's lines
    let mut line: Vec<Span<'static>> = Vec::new(); // alloc: a line
    let mut used = 0;
    if width == 0 {
        return lines;
    }
    for span in spans {
        let style = span.style;
        let mut piece = String::new(); // alloc: a span
        for it in span.content.chars() {
            let w = it.width().unwrap_or(0);
            if used + w > width {
                line.push(Span::styled(std::mem::take(&mut piece), style));
                lines.push(Line::from(std::mem::take(&mut line)));
                used = 0;
            }
            piece.push(it);
            used += w;
        }
        if !piece.is_empty() {
            line.push(Span::styled(piece, style));
        }
    }
    lines.push(Line::from(line));
    lines
}

/// One line of text with ANSI SGR escapes (rustc's diagnostics, a test's colored output) as
/// styled spans: the colors and bold, dim, italic, underline and reverse; every other escape
/// and control character dropped, a tab as four spaces, a carriage return starting the line
/// again. Under [`Paint::Plain`] the colors are dropped too.
pub fn ansi(line: &str, paint: Paint) -> Vec<Span<'static>> {
    let mut spans = Vec::new(); // alloc: a line
    let mut style = Style::new();
    let mut piece = String::new(); // alloc: a span
    let mut chars = line.chars().peekable();
    while let Some(it) = chars.next() {
        match it {
            '\x1b' => {
                if chars.peek() != Some(&'[') {
                    chars.next();
                    continue;
                }
                chars.next();
                let mut params = String::new(); // alloc: an escape's parameters
                let mut last = None;
                for next in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&next) {
                        last = Some(next);
                        break;
                    }
                    params.push(next);
                }
                if last == Some('m') {
                    if !piece.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut piece), style));
                    }
                    style = sgr(style, &params, paint);
                }
            }
            '\t' => piece.push_str("    "),
            // A line ending `\r\n` keeps its text; a `\r` within it draws over what came before.
            '\r' if chars.peek().is_some_and(|next| *next != '\n') => {
                spans.clear();
                piece.clear();
            }
            it if it.is_control() => {}
            it => piece.push(it),
        }
    }
    if !piece.is_empty() {
        spans.push(Span::styled(piece, style));
    }
    spans
}

/// `style` after the SGR parameters `params` (`1;31`, `38;5;208`, `0`).
fn sgr(mut style: Style, params: &str, paint: Paint) -> Style {
    let mut codes = params.split(';').map(|it| it.parse::<u16>().unwrap_or(0));
    let color = |style: Style, color: Color, background: bool| match (paint, background) {
        (Paint::Plain, _) => style,
        (Paint::Color, false) => style.fg(color),
        (Paint::Color, true) => style.bg(color),
    };
    const BASIC: [Color; 8] = [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
    ];
    const BRIGHT: [Color; 8] = [
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ];
    while let Some(code) = codes.next() {
        style = match code {
            0 => Style::new(),
            1 => style.add_modifier(Modifier::BOLD),
            2 => style.add_modifier(Modifier::DIM),
            3 => style.add_modifier(Modifier::ITALIC),
            4 => style.add_modifier(Modifier::UNDERLINED),
            7 => style.add_modifier(Modifier::REVERSED),
            22 => style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style.remove_modifier(Modifier::ITALIC),
            24 => style.remove_modifier(Modifier::UNDERLINED),
            27 => style.remove_modifier(Modifier::REVERSED),
            30..=37 => color(style, BASIC[usize::from(code - 30)], false),
            40..=47 => color(style, BASIC[usize::from(code - 40)], true),
            90..=97 => color(style, BRIGHT[usize::from(code - 90)], false),
            100..=107 => color(style, BRIGHT[usize::from(code - 100)], true),
            39 => match paint {
                Paint::Color => style.fg(Color::Reset),
                Paint::Plain => style,
            },
            49 => match paint {
                Paint::Color => style.bg(Color::Reset),
                Paint::Plain => style,
            },
            38 | 48 => {
                let background = code == 48;
                match codes.next() {
                    Some(5) => {
                        let n = codes.next().unwrap_or(0).min(255) as u8;
                        color(style, Color::Indexed(n), background)
                    }
                    Some(2) => {
                        let mut part = || codes.next().unwrap_or(0).min(255) as u8;
                        let (r, g, b) = (part(), part(), part());
                        color(style, Color::Rgb(r, g, b), background)
                    }
                    Some(_) | None => style,
                }
            }
            _ => style,
        };
    }
    style
}
