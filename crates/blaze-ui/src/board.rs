//! The live view of a build (`blaze build`, `check` and `test`), drawn from one [`Now`]
//! (docs/v2/plan/design/build-view.md, "The renderer"). A sans-IO state machine: a [`Board`]
//! is fed the build's model as it comes ([`Board::take`]) and the keys pressed ([`Board::key`]),
//! and draws it into a ratatui [`Buffer`] ([`Board::render`]) in one of two [`Form`]s, or, off a
//! terminal, as one line of text when one is due ([`Board::line`]). It owns no terminal, thread
//! or clock: its time is the model's `at_ms`, and what it draws depends only on the last `Now`
//! and whether it is expanded.
//!
//! [`Form::Full`], top to bottom, each section only when it has rows:
//!
//! ```text
//!  blaze test  compiling                 488 of 570 tasks done · 12 in the last 30s   15m14s
//!  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━  3 running · 81 waiting
//!  3 of 16 workers busy · 13 idle
//!  memory  106.3 GB, no limit
//!  ⚠ slow: no task settled for 4m30s; 3 of 16 workers busy
//!  ⚠ longest: codegen core 1.0.0 lib (host), 4m30s, on blaze-worker-3 in core::ptr::is_freeze
//!
//!  running 3, longest first
//!    4m30s  codegen     core 1.0.0 lib (host)               blaze-worker-3  core::ptr::is_freeze  core
//!    2m01s  test        blaze-ui 0.1.0 lib test (host)      waiting on the test process
//!    31.0s  script-run  ring 0.17.8 build-script (host)     a process, on no worker
//!
//!  proc macros  2 of 32 expanding
//!    serde_derive::Serialize  blaze-api  1.2s
//!
//!  waiting  78 on their inputs · 3 ready for a worker
//!    codegen core 1.0.0 lib (host)  running 4m30s  holds 82  chain 41
//!      std build script, libc build script, alloc lib, +79
//!
//!  failed 1
//!    bodies foo 0.1.0 lib (host)
//!
//!  slowest so far
//!    bodies hir-ty 1m12s · codegen syntax 48.0s · script-run ring 31.0s
//!
//!  e expand · blaze explore for the graph · blaze debug live for every thread
//! ```
//!
//! Running and waiting stay apart: a running task whose worker thread waits says `waiting on`
//! what its innermost span awaits, never counted busy, and one no worker is inside (a build
//! script's or a test's process) says so. The running section takes the rows the rest leaves:
//! one task a row, as many as fit, the rest counted (`… 12 more running`). The memory line is
//! always there, colored by pressure; over the limit it moves above the header. When the area
//! is shorter than the content, the running rows shrink first, then the other sections go from
//! the bottom up, then the footer; the header, the workers, the memory and the warnings stay
//! while there is room, and so does the stall: the daemon's top stall in its wait-for graph
//! ([`Now::stall`], ` ⚠ convoy: 61 behind thread 7 on lock flight.intern ...`), under the
//! warning in both forms and at the end of the compact line, while one lasts. Drawing never panics on any size, 0 by 0 included: every write is
//! clipped to the area.

#![blaze::sans_io]

use std::time::Duration;

use blaze_protocol::now::{
    Blocker, Busy, Download, Memory, Now, Phase, Pressure, Running, STALL_MS,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::test_view::{Paint, duration, fit, right_aligned, text_width};

#[cfg(test)]
mod tests;

/// How the board is drawn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Form {
    /// The whole terminal (the alternate screen): every section, as many as fit.
    Full,
    /// A few rows under the scrollback, at most [`INLINE_ROWS`]: the header with the workers
    /// and the memory, the warning, the longest running tasks, the first blocker, the first
    /// download.
    Inline,
}

/// A key the driver read in raw mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// `e`: every section to all its rows, and back.
    Expand,
    /// Any other key: nothing.
    Other,
}

/// The most rows [`Form::Inline`] draws.
pub const INLINE_ROWS: usize = 10;

/// How often a compact line is due when nothing it names changed, in the model's time.
pub const LINE_EVERY_MS: u64 = 10_000;

/// Collapsed, the most busy workers the full form lists.
const BUSY_ROWS: usize = 4;

/// Collapsed, the most blockers the full form lists.
const BLOCKER_ROWS: usize = 3;

/// Collapsed, the most downloads the full form lists.
const DOWNLOAD_ROWS: usize = 2;

/// Collapsed, the most slowest tasks the full form names.
const SLOWEST_SHOWN: usize = 5;

/// The longest running tasks the compact line names.
const COMPACT_RUNNING: usize = 3;

/// Collapsed, the most failed tasks the full form names.
const FAILURE_ROWS: usize = 5;

/// The slowest settled tasks the scrollback summary names.
const SUMMARY_SLOWEST: usize = 5;

/// Cells of the memory line's bar.
const MEMORY_BAR: usize = 20;

/// Cells of a download's bar.
const DOWNLOAD_BAR: usize = 12;

/// The memory line's label.
const MEMORY: &str = " memory  ";

/// The footer after its `e expand`.
const FOOTER: &str = " · blaze explore for the graph · blaze debug live for every thread";

/// The live view's state: the last model, and whether it is expanded. See the module's docs.
pub struct Board {
    command: String,
    paint: Paint,
    now: Now,
    /// Whether a model was taken: no line is due before one.
    taken: bool,
    expanded: bool,
    /// What the last compact line was said of, to tell when the next is due.
    last: Option<Mark>,
}

/// What a compact line was said of: a change of any of these but the time makes the next due.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Mark {
    at_ms: u64,
    phase: Phase,
    /// The first blocker's task, by its id.
    blocker: Option<u64>,
    pressure: Pressure,
    stalled: bool,
    slow: bool,
    /// The daemon named a stall in its wait-for graph ([`Now::stall`]).
    locked: bool,
}

/// Whether a block of the full form stays when the area is too short for every block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Keep {
    /// Dropped first, the lowest first.
    Drop,
    /// Dropped once no [`Keep::Drop`] block is left.
    Footer,
    /// Kept while there is room; what does not fit is cut at the bottom.
    Always,
    /// The running rows: shortened to the rows the rest leaves, before any block goes, to its
    /// heading and one `… N more running` row; dropped with the [`Keep::Drop`] ones after.
    Fill,
}

/// A section of the full form: its rows, the blank one above it included.
struct Block {
    keep: Keep,
    rows: Vec<Line<'static>>,
    /// For the [`Keep::Fill`] block: the running tasks the model counts but does not list,
    /// which its last row (`… N more running`) counts when it has one.
    hidden: usize,
}

impl Board {
    /// A board for `command` (`blaze test`), as it names it in its header.
    pub fn new(command: &str, paint: Paint) -> Board {
        Board {
            command: command.to_owned(), // alloc: once a build
            paint,
            now: Now::default(),
            taken: false,
            expanded: false,
            last: None,
        }
    }

    /// The build as it is now: what the next draw and line show.
    pub fn take(&mut self, now: Now) {
        self.now = now;
        self.taken = true;
    }

    pub fn key(&mut self, key: Key) {
        match key {
            Key::Expand => self.expanded = !self.expanded,
            Key::Other => {}
        }
    }

    /// Whether every section shows all its rows.
    pub fn expanded(&self) -> bool {
        self.expanded
    }

    /// The rows `form` wants now at `width` columns: every section of the full form, or the
    /// inline form's rows. A row never wraps, so the width only matters at 0.
    pub fn height(&self, form: Form, width: u16) -> u16 {
        if width == 0 {
            return 0;
        }
        let rows: usize = match form {
            Form::Full => self.full(width as usize).iter().map(|it| it.rows.len()).sum(),
            Form::Inline => self.inline(width as usize).len(),
        };
        u16::try_from(rows).unwrap_or(u16::MAX)
    }

    /// The board in `form`, drawn into `area` of `buf`: the area is cleared first, and every
    /// write is clipped to it, whatever its size.
    pub fn render(&self, form: Form, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(buf.area);
        if area.width == 0 || area.height == 0 {
            return;
        }
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.reset();
                }
            }
        }
        let (width, height) = (area.width as usize, area.height as usize);
        let rows = match form {
            Form::Full => shorten(self.full(width), height),
            Form::Inline => self.inline(width),
        };
        for (y, row) in (area.y..area.bottom()).zip(&rows) {
            buf.set_line(area.x, y, row, area.width);
        }
    }

    /// The compact line for CI, pipes and agents, appended to `out`; false, with nothing
    /// appended, when none is due. One is due for the first model, and after that when the
    /// phase, the first blocker's task or the memory's pressure changed since the last line,
    /// when the build turned stalled or slow, or when [`LINE_EVERY_MS`] of the model's time
    /// passed since the last line. No color, no trailing blank, no newline.
    pub fn line(&mut self, out: &mut String) -> bool {
        if !self.taken {
            return false;
        }
        let now = &self.now;
        let mark = Mark {
            at_ms: now.at_ms,
            phase: now.phase,
            blocker: now.blockers.first().map(|it| it.task.id),
            pressure: now.memory.pressure(),
            stalled: now.stalled(),
            slow: now.slow(),
            locked: now.stall.is_some(),
        };
        let due = match self.last {
            None => true,
            Some(last) => {
                last.phase != mark.phase
                    || last.blocker != mark.blocker
                    || last.pressure != mark.pressure
                    || (mark.stalled && !last.stalled)
                    || (mark.slow && !last.slow)
                    || (mark.locked && !last.locked)
                    || mark.at_ms >= last.at_ms.saturating_add(LINE_EVERY_MS)
            }
        };
        if !due {
            return false;
        }
        self.last = Some(mark);
        compact(now, out);
        true
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

    /// The memory's style by its pressure: calm dim, rising yellow, squeezed and over red and
    /// bold.
    fn pressed(&self, pressure: Pressure) -> Style {
        match pressure {
            Pressure::Calm => self.dim(),
            Pressure::Rising => self.style(Color::Yellow),
            Pressure::Squeezed | Pressure::Over => {
                self.style(Color::Red).add_modifier(Modifier::BOLD)
            }
        }
    }

    /// How many of `len` rows a section lists: all when expanded, else at most `limit`.
    fn shown(&self, len: usize, limit: usize) -> usize {
        match self.expanded {
            true => len,
            false => len.min(limit),
        }
    }

    /// `… N more what` for the rows a section left out, when it left any.
    fn more(&self, len: usize, shown: usize, what: &str) -> Option<Line<'static>> {
        (len > shown).then(|| {
            let more = format!("   … {} more {what}", len - shown); // alloc: a row
            Line::from(Span::styled(more, self.dim()))
        })
    }

    /// The full form's sections, top to bottom, each only when it has rows.
    fn full(&self, width: usize) -> Vec<Block> {
        let now = &self.now;
        let over = now.memory.pressure() == Pressure::Over;
        let one = |keep: Keep, row: Line<'static>| Block { keep, rows: vec![row], hidden: 0 }; // alloc: a row
        let mut blocks = Vec::with_capacity(16); // alloc: one a draw
        if over {
            blocks.push(one(Keep::Always, self.memory_row(width)));
        }
        blocks.push(one(Keep::Always, self.header(width)));
        if now.tally.total > 0 {
            blocks.push(one(Keep::Drop, self.gauge(width)));
        }
        if now.workers > 0 {
            blocks.push(one(Keep::Always, self.workers_row()));
        }
        if !now.stage.is_empty() && now.tally.running == 0 && !now.stalled() {
            let stage = Span::styled(now.stage.clone(), self.dim()); // clone: a row's text
            blocks.push(one(Keep::Drop, Line::from(vec![Span::raw(" "), stage]))); // alloc: a row
        }
        if !over {
            blocks.push(one(Keep::Always, self.memory_row(width)));
        }
        if let Some(warning) = self.warning() {
            blocks.push(one(Keep::Always, warning));
        }
        if let Some(stall) = self.stall_row(width) {
            blocks.push(one(Keep::Always, stall));
        }
        if let Some(longest) = self.longest() {
            blocks.push(one(Keep::Always, longest));
        }
        if !now.running.is_empty() || now.tally.running > 0 {
            let (rows, hidden) = self.running(width);
            blocks.push(Block { keep: Keep::Fill, rows, hidden });
        }
        let unowned = now.busy.iter().filter(|it| it.task.is_none()).count();
        if unowned > 0 {
            blocks.push(Block { keep: Keep::Drop, rows: self.workers(width), hidden: 0 });
        }
        if now.expanding.threads > 0 || !now.expanding.busy.is_empty() {
            blocks.push(Block { keep: Keep::Drop, rows: self.expanding(width), hidden: 0 });
        }
        if now.tally.waiting > 0 || now.tally.ready > 0 || !now.blockers.is_empty() {
            blocks.push(Block { keep: Keep::Drop, rows: self.blocked(width), hidden: 0 });
        }
        if !now.downloads.is_empty() {
            blocks.push(Block { keep: Keep::Drop, rows: self.downloads(width), hidden: 0 });
        }
        if !now.failures.is_empty() {
            blocks.push(Block { keep: Keep::Drop, rows: self.failures(width), hidden: 0 });
        }
        if !now.slowest.is_empty() {
            blocks.push(Block { keep: Keep::Drop, rows: self.slowest(width), hidden: 0 });
        }
        let footer = vec![Line::default(), self.footer()]; // alloc: a row
        blocks.push(Block { keep: Keep::Footer, rows: footer, hidden: 0 });
        blocks
    }

    /// The inline form's rows: the header with the workers and the memory, the warning (or
    /// the memory line when it is squeezed or over), the longest running tasks in the rows the
    /// first blocker and the first download leave, then those two.
    fn inline(&self, width: usize) -> Vec<Line<'static>> {
        let now = &self.now;
        let mut rows = Vec::with_capacity(INLINE_ROWS); // alloc: one a draw
        rows.push(self.inline_header(width));
        if let Some(warning) = self.warning() {
            rows.push(warning);
        } else if matches!(now.memory.pressure(), Pressure::Squeezed | Pressure::Over) {
            rows.push(self.memory_row(width));
        }
        if let Some(stall) = self.stall_row(width) {
            rows.push(stall);
        }
        let mut tail = Vec::with_capacity(2); // alloc: a draw's last rows
        if let Some(blocker) = now.blockers.first() {
            let label = fit(&label(blocker), width / 2);
            tail.push(self.blocker_row(" blocked on ", blocker, label, 0, state(blocker), 0));
        }
        if let Some(download) = now.downloads.first() {
            tail.push(self.download_row(" downloading ", download, width));
        }
        let room = INLINE_ROWS.saturating_sub(rows.len() + tail.len());
        let columns = Columns::of(&now.running, width);
        let shown = now.running.len().min(room);
        let shown = match shown < now.running.len() && shown > 0 {
            true => shown - 1,
            false => shown,
        };
        for running in &now.running[..shown] {
            rows.push(self.running_row(running, &columns, width));
        }
        rows.extend(self.more(now.running.len(), shown, "running"));
        rows.extend(tail);
        rows.truncate(INLINE_ROWS);
        rows
    }

    /// ` blaze test  compiling` with `488 of 570 tasks   15m14s` at the end.
    fn header(&self, width: usize) -> Line<'static> {
        let now = &self.now;
        let accent = self.style(Color::Cyan).add_modifier(Modifier::BOLD);
        let left = vec![
            Span::raw(" "),
            Span::styled(self.command.clone(), accent), // clone: a row's text
            Span::raw("  "),
            Span::styled(now.phase.word(), bold()),
        ]; // alloc: a row
        let mut right = Vec::with_capacity(3); // alloc: a row
        if now.tally.total > 0 {
            // alloc: a row
            let mut tasks = format!("{} of {} tasks done", now.tally.done, now.tally.total);
            if now.recent > 0 {
                let secs = blaze_protocol::now::RECENT_MS / 1000;
                put!(&mut tasks, " · {} in the last {secs}s", now.recent);
            }
            right.push(Span::styled(tasks, self.dim()));
            right.push(Span::raw("   "));
        }
        right.push(Span::styled(time(now.at_ms), bold()));
        right_aligned(left, right, width)
    }

    /// The inline form's one row for the header, the gauge and the memory:
    /// ` blaze test  compiling  488/570  1/16 busy  mem 20.1/24.0 GB` and the time at the end.
    fn inline_header(&self, width: usize) -> Line<'static> {
        let now = &self.now;
        let accent = self.style(Color::Cyan).add_modifier(Modifier::BOLD);
        let mut left = Vec::with_capacity(7); // alloc: a row
        left.push(Span::raw(" "));
        left.push(Span::styled(self.command.clone(), accent)); // clone: a row's text
        left.push(Span::raw("  "));
        left.push(Span::styled(now.phase.word(), bold()));
        if now.tally.total > 0 {
            let done = format!("  {} of {} done", now.tally.done, now.tally.total); // alloc: a row
            left.push(Span::styled(done, self.dim()));
        }
        if now.workers > 0 {
            let busy = format!("  {} of {} busy", now.busy.len(), now.workers); // alloc: a row
            left.push(Span::styled(busy, bold()));
        }
        let memory = format!("  mem {}", memory_short(&now.memory)); // alloc: a row
        left.push(Span::styled(memory, self.pressed(now.memory.pressure())));
        right_aligned(left, vec![Span::styled(time(now.at_ms), bold())], width) // alloc: a row
    }

    /// The tasks done of all as a bar, then `1 running · 81 waiting`.
    fn gauge(&self, width: usize) -> Line<'static> {
        let tally = &self.now.tally;
        let mut counts = String::new(); // alloc: a row's text
        put!(&mut counts, "{} running", tally.running);
        if tally.ready > 0 {
            put!(&mut counts, " · {} ready", tally.ready);
        }
        put!(&mut counts, " · {} waiting", tally.waiting);
        if tally.failed > 0 {
            put!(&mut counts, " · {} failed", tally.failed);
        }
        let cells = width.saturating_sub(text_width(&counts) + 4);
        let fill = self.style(Color::Green);
        let mut line = Vec::with_capacity(4); // alloc: a row
        line.push(Span::raw(" "));
        line.extend(self.bar(cells, u64::from(tally.done), u64::from(tally.total), fill));
        line.push(Span::raw("  "));
        line.push(Span::styled(counts, self.dim()));
        Line::from(line)
    }

    /// A bar of `cells` cells filled `done` of `of` (empty when `of` is 0), the filled part in
    /// `fill`.
    fn bar(&self, cells: usize, done: u64, of: u64, fill: Style) -> [Span<'static>; 2] {
        let filled = match of {
            0 => 0,
            of => (u128::from(done.min(of)) * cells as u128 / u128::from(of)) as usize,
        };
        [
            Span::styled("━".repeat(filled), fill), // alloc: a row
            Span::styled("─".repeat(cells.saturating_sub(filled)), self.dim()), // alloc: a row
        ]
    }

    /// ` memory  ━━━━───  20.1 of 24.0 GB  peak 22.3 GB` and what squeezes it. The bar goes
    /// when the row would not fit with it.
    fn memory_row(&self, width: usize) -> Line<'static> {
        let memory = &self.now.memory;
        let pressure = memory.pressure();
        let style = self.pressed(pressure);
        let amount = match memory.limit {
            0 => format!("{}, no limit", bytes(memory.held)), // alloc: a row's text
            limit => pair(memory.held, limit, " of "),
        };
        let mut tail = Vec::with_capacity(3); // alloc: a row
        tail.push(Span::styled(amount, style));
        if memory.peak > memory.held {
            let peak = format!("  peak {}", bytes(memory.peak)); // alloc: a row's text
            tail.push(Span::styled(peak, self.dim()));
        }
        match pressure {
            Pressure::Calm | Pressure::Rising => {}
            Pressure::Squeezed => tail.push(Span::styled(squeeze("squeezed", memory), style)),
            Pressure::Over => tail.push(Span::styled(squeeze("OVER THE LIMIT", memory), style)),
        }
        let tail_width: usize = tail.iter().map(Span::width).sum();
        let mut line = Vec::with_capacity(tail.len() + 4); // alloc: a row
        line.push(Span::styled(MEMORY, self.dim()));
        if memory.limit > 0 && text_width(MEMORY) + MEMORY_BAR + 2 + tail_width <= width {
            let fill = match pressure {
                Pressure::Calm => self.style(Color::Green),
                Pressure::Rising | Pressure::Squeezed | Pressure::Over => style,
            };
            line.extend(self.bar(MEMORY_BAR, memory.held, memory.limit, fill));
            line.push(Span::raw("  "));
        }
        line.extend(tail);
        Line::from(line)
    }

    /// ` ⚠ slow: ...` when a worker is busy and nothing settled for a while, ` ⚠ stalled: ...`
    /// when nothing runs and nothing arrives; none otherwise.
    fn warning(&self) -> Option<Line<'static>> {
        let now = &self.now;
        let mut text = String::new(); // alloc: a row's text
        let style = if now.stalled() {
            put!(
                &mut text,
                "⚠ stalled: nothing settled for {}; no worker busy, no download moving",
                time(now.quiet_ms)
            );
            if !now.stage.is_empty() {
                put!(&mut text, "; {}", now.stage);
            }
            self.style(Color::Red).add_modifier(Modifier::BOLD)
        } else if now.slow() {
            put!(
                &mut text,
                "⚠ slow: no task settled for {}; {} of {} workers busy",
                time(now.quiet_ms),
                now.busy.len(),
                now.workers
            );
            match now.downloads.len() {
                0 => {}
                1 => text.push_str(", 1 download moving"),
                n => put!(&mut text, ", {n} downloads moving"),
            }
            self.style(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            return None;
        };
        Some(Line::from(vec![Span::raw(" "), Span::styled(text, style)])) // alloc: a row
    }

    /// ` ⚠ convoy: 61 behind thread 7 on lock flight.intern (oldest 2400 ms); the head is
    /// running`: the daemon's top stall in its wait-for graph ([`Now::stall`]), red and bold,
    /// cut to the width; none while no stall lasts.
    fn stall_row(&self, width: usize) -> Option<Line<'static>> {
        let line = self.now.stall.as_deref()?;
        let text = fit(&format!("⚠ {line}"), width.saturating_sub(1)); // alloc: a row's text
        let style = self.style(Color::Red).add_modifier(Modifier::BOLD);
        Some(Line::from(vec![Span::raw(" "), Span::styled(text, style)])) // alloc: a row
    }

    /// ` 3 of 16 workers busy · 13 idle`: a worker is busy while its innermost span is work,
    /// not a wait.
    fn workers_row(&self) -> Line<'static> {
        let now = &self.now;
        let busy = format!(" {} of {} workers busy", now.busy.len(), now.workers); // alloc: a row
        let idle = format!(" · {} idle", now.idle()); // alloc: a row
        Line::from(vec![Span::styled(busy, bold()), Span::styled(idle, self.dim())]) // alloc: a row
    }

    /// The ` longest:` line naming the task running longest, once it runs past [`STALL_MS`]:
    /// its name, how long, and where it is (its worker and span, or what it waits on).
    fn longest(&self) -> Option<Line<'static>> {
        let running = self.now.running.first().filter(|it| it.for_ms >= STALL_MS)?;
        let mut text = format!(
            "⚠ longest: {} {}, {}, ",
            running.task.kind,
            running.task.name,
            time(running.for_ms)
        ); // alloc: a row's text
        text.push_str(&whereabouts(running));
        let style = self.style(Color::Yellow).add_modifier(Modifier::BOLD);
        Some(Line::from(vec![Span::raw(" "), Span::styled(text, style)])) // alloc: a row
    }

    /// ` running N, longest first`, then a row for each running task, as many as the area
    /// leaves ([`Keep::Fill`]), and how many running tasks the model counts but does not list.
    fn running(&self, width: usize) -> (Vec<Line<'static>>, usize) {
        let now = &self.now;
        let mut rows = Vec::with_capacity(now.running.len() + 3); // alloc: a section
        rows.push(Line::default());
        let count = now.tally.running.max(u32::try_from(now.running.len()).unwrap_or(u32::MAX));
        rows.push(Line::from(vec![
            Span::styled(format!(" running {count}"), bold()), // alloc: a row
            Span::styled(", longest first", self.dim()),
        ])); // alloc: a row
        let columns = Columns::of(&now.running, width);
        for running in &now.running {
            rows.push(self.running_row(running, &columns, width));
        }
        let hidden = (count as usize).saturating_sub(now.running.len());
        rows.extend(self.more(hidden, 0, "running"));
        (rows, hidden)
    }

    /// A running task: how long, its kind, its name, then where it is: its worker and its
    /// span's item and crate, `waiting on` what its worker's span awaits, or that no worker is
    /// inside it.
    fn running_row(&self, running: &Running, columns: &Columns, width: usize) -> Line<'static> {
        let pad = |text: String, to: usize| {
            let gap = to.saturating_sub(text_width(&text));
            text + &" ".repeat(gap) // alloc: a row's text
        };
        let took = time(running.for_ms);
        let took_style = match running.for_ms >= STALL_MS {
            true => self.style(Color::Yellow).add_modifier(Modifier::BOLD),
            false => self.dim(),
        };
        let mut line = Vec::with_capacity(10); // alloc: a row
        line.push(Span::raw("   "));
        line.push(Span::styled(pad(took, columns.time), took_style));
        line.push(Span::raw("  "));
        line.push(Span::styled(pad(running.task.kind.clone(), columns.kind), self.dim())); // clone: a row's text
        line.push(Span::raw("  "));
        line.push(Span::raw(pad(fit(&running.task.name, columns.name), columns.name)));
        line.push(Span::raw("  "));
        let used: usize = line.iter().map(Span::width).sum();
        let room = width.saturating_sub(used);
        match (&running.thread, &running.span) {
            (Some(thread), Some(span)) if span.awaits.is_some() => {
                let awaits = span.awaits.as_deref().unwrap_or_default();
                let text = format!("waiting on {awaits}"); // alloc: a row's text
                line.push(Span::styled(fit(&text, room), self.style(Color::Yellow)));
                let thread = format!("  {thread}"); // alloc: a row's text
                line.push(Span::styled(thread, self.dim()));
            }
            (Some(thread), span) => {
                line.push(Span::styled(thread.clone(), self.style(Color::Cyan))); // clone: a row's text
                let mut rest = String::new(); // alloc: a row's text
                if let Some(span) = span {
                    rest.push_str("  ");
                    rest.push_str(span.item.as_deref().unwrap_or(&span.name));
                    if let Some(krate) = &span.krate {
                        put!(&mut rest, "  {krate}");
                    }
                }
                let room = room.saturating_sub(text_width(thread));
                line.push(Span::styled(fit(&rest, room), self.dim()));
            }
            (None, _) => {
                let text = unowned(&running.task.kind);
                line.push(Span::styled(fit(text, room), self.dim()));
            }
        }
        Line::from(line)
    }

    /// ` other busy workers`, then a row for each busy worker no running task names (a pool job
    /// of no plane task).
    fn workers(&self, width: usize) -> Vec<Line<'static>> {
        let now = &self.now;
        // alloc: a section
        let unowned: Vec<&Busy> = now.busy.iter().filter(|it| it.task.is_none()).collect();
        let shown = self.shown(unowned.len(), BUSY_ROWS);
        let mut rows = Vec::with_capacity(shown + 3); // alloc: a section
        rows.push(Line::default());
        rows.push(Line::from(vec![
            Span::styled(" busy on no task", bold()),
            Span::styled(format!("  {}", unowned.len()), self.dim()), // alloc: a row
        ])); // alloc: a row
        for busy in &unowned[..shown] {
            rows.push(self.busy_row(busy, width));
        }
        rows.extend(self.more(unowned.len(), shown, "busy"));
        rows
    }

    /// ` proc macros  2 of 32 expanding`, then a row for each expansion thread inside one: the
    /// macro, the crate it expands in, how long.
    fn expanding(&self, width: usize) -> Vec<Line<'static>> {
        let lane = &self.now.expanding;
        let shown = self.shown(lane.busy.len(), BUSY_ROWS);
        let mut rows = Vec::with_capacity(shown + 3); // alloc: a section
        rows.push(Line::default());
        let threads = lane.threads.max(u32::try_from(lane.busy.len()).unwrap_or(u32::MAX));
        rows.push(Line::from(vec![
            Span::styled(" proc macros", bold()),
            Span::styled(format!("  {} of {threads} expanding", lane.busy.len()), self.dim()), // alloc: a row
        ])); // alloc: a row
        for busy in &lane.busy[..shown] {
            let span = &busy.span;
            let mut left = vec![Span::raw("   ")]; // alloc: a row
            left.push(Span::raw(span.item.clone().unwrap_or_else(|| span.name.clone()))); // clone: a row's text
            if let Some(krate) = &span.krate {
                left.push(Span::styled(format!("  in {krate}"), self.dim())); // alloc: a row
            }
            if let Some(awaits) = &span.awaits {
                let text = format!("  waiting on {awaits}"); // alloc: a row
                left.push(Span::styled(text, self.style(Color::Yellow)));
            }
            let took = Span::styled(time(span.for_ms), self.dim());
            rows.push(right_aligned(left, vec![took], width)); // alloc: a row
        }
        rows.extend(self.more(lane.busy.len(), shown, "expanding"));
        rows
    }

    /// ` failed N`, then the failed tasks, a row each.
    fn failures(&self, width: usize) -> Vec<Line<'static>> {
        let failures = &self.now.failures;
        let shown = self.shown(failures.len(), FAILURE_ROWS);
        let mut rows = Vec::with_capacity(shown + 3); // alloc: a section
        rows.push(Line::default());
        let style = self.style(Color::Red).add_modifier(Modifier::BOLD);
        rows.push(Line::from(Span::styled(format!(" failed {}", self.now.tally.failed.max(failures.len() as u32)), style))); // alloc: a row
        for task in &failures[..shown] {
            let text = format!("   {} {}", task.kind, task.name); // alloc: a row's text
            rows.push(Line::from(Span::raw(fit(&text, width))));
        }
        rows.extend(self.more(failures.len(), shown, "failed"));
        rows
    }

    /// The summary the scrollback keeps when the board ends: how long the build took, its
    /// tasks done of all, the slowest few with their times, and every failed task, a line each,
    /// no color, appended to `out`.
    pub fn summary(&self, out: &mut String) {
        let now = &self.now;
        if !self.taken {
            return;
        }
        put!(out, "{} {} in {}", self.command, now.phase.word(), time(now.at_ms));
        if now.tally.total > 0 {
            put!(out, ": {} of {} tasks done", now.tally.done, now.tally.total);
        }
        if now.tally.failed > 0 {
            put!(out, ", {} failed", now.tally.failed);
        }
        out.push('\n');
        if !now.slowest.is_empty() {
            out.push_str("slowest:");
            for (at, took) in now.slowest.iter().take(SUMMARY_SLOWEST).enumerate() {
                let sep = if at == 0 { " " } else { " · " };
                put!(out, "{sep}{} {} {}", took.task.kind, took.task.name, time(took.ms));
            }
            out.push('\n');
        }
        for task in &now.failures {
            put!(out, "failed: {} {}\n", task.kind, task.name);
        }
    }

    /// A busy worker: its thread, its task's kind, the item its span is of (or the task's
    /// name), the crate, the span's path ending in its own name, and how long it is in it at
    /// the end. The path is cut at its front to fit.
    fn busy_row(&self, busy: &Busy, width: usize) -> Line<'static> {
        let span = &busy.span;
        let mut left = Vec::with_capacity(10); // alloc: a row
        left.push(Span::raw("   "));
        let thread = busy.thread.clone(); // clone: a row's text
        left.push(Span::styled(thread, self.style(Color::Cyan)));
        if let Some(task) = &busy.task {
            left.push(Span::raw("  "));
            left.push(Span::styled(task.kind.clone(), self.dim())); // clone: a row's text
        }
        let item = span.item.as_deref().or(busy.task.as_ref().map(|it| it.name.as_str()));
        if let Some(item) = item {
            left.push(Span::raw("  "));
            left.push(Span::raw(item.to_owned())); // alloc: a row's text
        }
        if let Some(krate) = &span.krate {
            left.push(Span::raw("  "));
            left.push(Span::styled(krate.clone(), self.dim())); // clone: a row's text
        }
        let mut path = String::new(); // alloc: a row's text
        for name in &busy.path {
            path.push_str(name);
            path.push_str(" > ");
        }
        path.push_str(&span.name);
        let took = time(span.for_ms);
        let fixed = left.iter().map(Span::width).sum::<usize>() + 2;
        let room = width.saturating_sub(fixed + text_width(&took) + 1);
        left.push(Span::raw("  "));
        left.push(Span::styled(fit(&path, room), self.dim()));
        let took_style = match span.for_ms >= STALL_MS {
            true => self.style(Color::Yellow).add_modifier(Modifier::BOLD),
            false => self.dim(),
        };
        right_aligned(left, vec![Span::styled(took, took_style)], width) // alloc: a row
    }

    /// ` waiting  78 on their inputs · 3 ready for a worker · 2 held back for memory`, then a
    /// row for each blocker (what most waits on) with what it holds back under it, the labels
    /// and states in columns.
    fn blocked(&self, width: usize) -> Vec<Line<'static>> {
        let now = &self.now;
        let blockers = &now.blockers[..self.shown(now.blockers.len(), BLOCKER_ROWS)];
        let labels: Vec<String> =
            blockers.iter().map(|it| fit(&label(it), width / 2)).collect(); // alloc: a section
        let states: Vec<String> = blockers.iter().map(state).collect(); // alloc: a section
        let label_width = labels.iter().map(|it| text_width(it)).max().unwrap_or(0);
        let state_width = states.iter().map(|it| text_width(it)).max().unwrap_or(0);
        let mut rows = Vec::with_capacity(blockers.len() * 2 + 3); // alloc: a section
        rows.push(Line::default());
        let mut counts = format!("  {} on their inputs", now.tally.waiting); // alloc: a row's text
        if now.tally.ready > 0 {
            put!(&mut counts, " · {} ready for a worker", now.tally.ready);
        }
        if now.memory.held_back > 0 {
            put!(&mut counts, " · {} held back for memory", now.memory.held_back);
        }
        rows.push(Line::from(vec![
            Span::styled(" waiting", bold()),
            Span::styled(counts, self.dim()),
        ])); // alloc: a row
        for ((blocker, label), state) in blockers.iter().zip(labels).zip(states) {
            rows.push(self.blocker_row("   ", blocker, label, label_width, state, state_width));
            rows.extend(self.behind(blocker));
        }
        rows.extend(self.more(self.now.blockers.len(), blockers.len(), "blockers"));
        rows
    }

    /// `codegen core  running 4m30s  holds 82  chain 41` after `prefix`, the label and the
    /// state padded to their columns' widths.
    fn blocker_row(
        &self,
        prefix: &'static str,
        blocker: &Blocker,
        label: String,
        label_width: usize,
        state: String,
        state_width: usize,
    ) -> Line<'static> {
        let pad = |text: String, to: usize| {
            let gap = to.saturating_sub(text_width(&text));
            text + &" ".repeat(gap) // alloc: a row's text
        };
        let state_style = match blocker.running {
            true => self.style(Color::Cyan),
            false => self.style(Color::Yellow),
        };
        let counts = format!("  holds {}  chain {}", blocker.holds, blocker.chain); // alloc: a row
        Line::from(vec![
            Span::raw(prefix),
            Span::styled(pad(label, label_width), bold()),
            Span::raw("  "),
            Span::styled(pad(state, state_width), state_style),
            Span::styled(counts, self.dim()),
        ]) // alloc: a row
    }

    /// The tasks a blocker holds back, dimmed under it: the first few by name and `+K` for the
    /// rest; none when it holds nothing back.
    fn behind(&self, blocker: &Blocker) -> Option<Line<'static>> {
        let rest = (blocker.holds as usize).saturating_sub(blocker.behind.len());
        if blocker.behind.is_empty() && rest == 0 {
            return None;
        }
        let mut text = String::from("     "); // alloc: a row's text
        for (at, name) in blocker.behind.iter().enumerate() {
            if at > 0 {
                text.push_str(", ");
            }
            text.push_str(name);
        }
        if rest > 0 {
            if !blocker.behind.is_empty() {
                text.push_str(", ");
            }
            put!(&mut text, "+{rest}");
        }
        Some(Line::from(Span::styled(text, self.dim())))
    }

    /// ` downloads`, then a row for each.
    fn downloads(&self, width: usize) -> Vec<Line<'static>> {
        let downloads = &self.now.downloads;
        let shown = self.shown(downloads.len(), DOWNLOAD_ROWS);
        let mut rows = Vec::with_capacity(shown + 3); // alloc: a section
        rows.push(Line::default());
        rows.push(Line::from(Span::styled(" downloads", bold())));
        for download in &downloads[..shown] {
            rows.push(self.download_row("   ", download, width));
        }
        rows.extend(self.more(downloads.len(), shown, "downloads"));
        rows
    }

    /// A download after `prefix`: what, a bar when its size is known, how much of it came, its
    /// rate, the time left, and how long since its last byte when that is long.
    fn download_row(
        &self,
        prefix: &'static str,
        download: &Download,
        width: usize,
    ) -> Line<'static> {
        let mut line = Vec::with_capacity(10); // alloc: a row
        line.push(Span::raw(prefix));
        line.push(Span::raw(fit(&download.what, width / 2)));
        if let Some(total) = download.total {
            line.push(Span::raw("  "));
            line.extend(self.bar(DOWNLOAD_BAR, download.done, total, self.style(Color::Blue)));
        }
        line.push(Span::raw("  "));
        line.push(Span::raw(amount(download)));
        if let Some(rate) = download.rate {
            line.push(Span::styled(format!("  {}", per_second(rate)), self.dim())); // alloc: a row
        }
        if let Some(eta) = download.eta_ms() {
            line.push(Span::styled(format!("  {} left", time(eta)), self.dim())); // alloc: a row
        }
        if download.idle_ms >= STALL_MS {
            let idle = format!("  no byte for {}", time(download.idle_ms)); // alloc: a row
            line.push(Span::styled(idle, self.style(Color::Yellow)));
        }
        Line::from(line)
    }

    /// ` slowest so far`, then the slowest settled tasks on one row, as many whole as fit.
    fn slowest(&self, width: usize) -> Vec<Line<'static>> {
        let limit = match self.expanded {
            true => usize::MAX,
            false => SLOWEST_SHOWN,
        };
        let mut line = vec![Span::raw("   ")]; // alloc: a row
        let mut used = 3;
        for (at, took) in self.now.slowest.iter().take(limit).enumerate() {
            let (kind, name) = (&took.task.kind, &took.task.name);
            let name = format!("{kind} {name} {}", time(took.ms)); // alloc: a row's text
            let sep = if at == 0 { "" } else { " · " };
            let need = text_width(sep) + text_width(&name);
            if at > 0 && used + need > width {
                break;
            }
            used += need;
            line.push(Span::styled(sep, self.dim()));
            line.push(Span::raw(name));
        }
        vec![
            Line::default(),
            Line::from(Span::styled(" slowest so far", bold())),
            Line::from(line),
        ] // alloc: a section
    }

    /// ` e expand · blaze explore for the graph · ...`, `e collapse` when expanded.
    fn footer(&self) -> Line<'static> {
        let word = match self.expanded {
            true => " collapse",
            false => " expand",
        };
        Line::from(vec![
            Span::raw(" "),
            Span::styled("e", bold()),
            Span::styled(word, self.dim()),
            Span::styled(FOOTER, self.dim()),
        ]) // alloc: a row
    }
}

/// The full form's rows in `height` rows: the [`Keep::Fill`] block shortens to what the rest
/// leaves (its heading, then as many rows as fit and a `… N more running` row), then the
/// [`Keep::Drop`] blocks (the running one with them) go from the bottom up, then the footer,
/// and what still does not fit is cut at the bottom.
fn shorten(mut blocks: Vec<Block>, height: usize) -> Vec<Line<'static>> {
    let mut total: usize = blocks.iter().map(|it| it.rows.len()).sum();
    if total > height
        && let Some(fill) = blocks.iter_mut().find(|it| it.keep == Keep::Fill)
    {
        // The blank row and the heading, then the task rows, then a `… more` row when some
        // are hidden.
        let head = 2;
        let len = fill.rows.len();
        let listed = len - head - usize::from(fill.hidden > 0);
        let rest = total - len;
        let keep = height.saturating_sub(rest).max(head + 1).min(len);
        if keep < len {
            let shown = keep - head - 1;
            fill.rows.truncate(head + shown);
            let text = format!("   … {} more running", listed - shown + fill.hidden); // alloc: a row
            fill.rows.push(Line::from(text));
            total = rest + fill.rows.len();
        }
        fill.keep = Keep::Drop;
    }
    for keep in [Keep::Drop, Keep::Footer] {
        while total > height {
            let Some(at) = blocks.iter().rposition(|it| it.keep == keep) else { break };
            total -= blocks.remove(at).rows.len();
        }
    }
    let mut rows = Vec::with_capacity(total); // alloc: one a draw
    for block in blocks {
        rows.extend(block.rows);
    }
    rows.truncate(height);
    rows
}

/// The compact line of `now`, appended to `out`:
/// `[15m14s] compiling 488 of 570 done · 1 of 16 busy · 3 running: codegen core 1.0.0 lib
/// (host) 4m30s, ... · mem 20.1/24.0 GB squeezed · on codegen core (core::ptr::is_freeze,
/// 4m30s), holds 82 · slow: nothing settled for 4m30s`. After the memory it names the first
/// blocker, else the first download, else the stage; last, the
/// daemon's stall in its wait-for graph while one lasts (`· convoy: 61 behind thread 7 ...`).
fn compact(now: &Now, out: &mut String) {
    let start = out.len();
    put!(out, "[{}] {}", time(now.at_ms), now.phase.word());
    if now.tally.total > 0 {
        put!(out, " {} of {} done", now.tally.done, now.tally.total);
    }
    if now.workers > 0 {
        put!(out, " · {} of {} busy", now.busy.len(), now.workers);
    }
    if !now.running.is_empty() {
        put!(out, " · {} running:", now.tally.running.max(now.running.len() as u32));
        for (at, running) in now.running.iter().take(COMPACT_RUNNING).enumerate() {
            let sep = if at == 0 { " " } else { ", " };
            put!(out, "{sep}{} {} {}", running.task.kind, running.task.name, time(running.for_ms));
            if running.span.as_ref().is_some_and(|it| it.awaits.is_some()) {
                out.push_str(" waiting");
            }
        }
    }
    put!(out, " · mem {}", memory_short(&now.memory));
    match now.memory.pressure() {
        Pressure::Calm => {}
        Pressure::Rising => out.push_str(" rising"),
        Pressure::Squeezed => out.push_str(" squeezed"),
        Pressure::Over => out.push_str(" over the limit"),
    }
    if let Some(blocker) = now.blockers.first() {
        put!(out, " · on {} (", label(blocker));
        if let Some(item) = blocker.span.as_ref().and_then(|it| it.item.as_deref()) {
            put!(out, "{item}, ");
        }
        if !blocker.running {
            out.push_str("ready ");
        }
        put!(out, "{}), holds {}", time(blocker.for_ms), blocker.holds);
    } else if let Some(download) = now.downloads.first() {
        put!(out, " · downloading {} {}", download.what, amount(download));
        if let Some(rate) = download.rate {
            put!(out, ", {}", per_second(rate));
        }
        if let Some(eta) = download.eta_ms() {
            put!(out, ", {} left", time(eta));
        }
    } else if !now.stage.is_empty() {
        put!(out, " · {}", now.stage);
    }
    if now.stalled() {
        put!(out, " · stalled: nothing settled for {}", time(now.quiet_ms));
    } else if now.slow() {
        put!(out, " · slow: nothing settled for {}", time(now.quiet_ms));
    }
    if let Some(stall) = &now.stall {
        put!(out, " · {stall}");
    }
    let end = start + out[start..].trim_end().len();
    out.truncate(end);
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// The widths of the running rows' columns: the time, the kind, and the name, which takes at
/// most half the width.
struct Columns {
    time: usize,
    kind: usize,
    name: usize,
}

impl Columns {
    fn of(running: &[Running], width: usize) -> Columns {
        let time_width = running.iter().map(|it| text_width(&time(it.for_ms))).max().unwrap_or(0);
        let kind = running.iter().map(|it| text_width(&it.task.kind)).max().unwrap_or(0);
        let name = running.iter().map(|it| text_width(&it.task.name)).max().unwrap_or(0);
        Columns { time: time_width, kind, name: name.min(width / 2) }
    }
}

/// What a running task no worker thread is inside is doing, by its kind: a build script's run
/// or a test is its process; anything else is parked on its fiber, waiting.
fn unowned(kind: &str) -> &'static str {
    match kind {
        "script-run" | "test" => "a process, on no worker",
        "fetch" => "a download, on no worker",
        _ => "parked, on no worker",
    }
}

/// Where a running task is, for the `longest` line and the compact line: `on blaze-worker-3 in
/// core::ptr::is_freeze`, `waiting on the lock of X`, or what [`unowned`] says.
fn whereabouts(running: &Running) -> String {
    match (&running.thread, &running.span) {
        (Some(_), Some(span)) if span.awaits.is_some() => {
            format!("waiting on {}", span.awaits.as_deref().unwrap_or_default()) // alloc: a row's text
        }
        (Some(thread), Some(span)) => {
            format!("on {thread} in {}", span.item.as_deref().unwrap_or(&span.name)) // alloc: a row's text
        }
        (Some(thread), None) => format!("on {thread}"), // alloc: a row's text
        (None, _) => unowned(&running.task.kind).to_owned(), // alloc: a row's text
    }
}

/// Milliseconds as the view says a time ([`duration`]).
fn time(ms: u64) -> String {
    duration(Duration::from_millis(ms))
}

/// A blocker's task: `codegen core`.
fn label(blocker: &Blocker) -> String {
    format!("{} {}", blocker.task.kind, blocker.task.name) // alloc: a row's text
}

/// Whether a blocker runs or waits for a worker, and since when: `running 4m30s`, `ready 12.0s`.
fn state(blocker: &Blocker) -> String {
    let word = if blocker.running { "running" } else { "ready" };
    format!("{word} {}", time(blocker.for_ms)) // alloc: a row's text
}

/// The memory in short: `20.1/24.0 GB` under a limit, `106.3 GB` with none.
fn memory_short(memory: &Memory) -> String {
    match memory.limit {
        0 => bytes(memory.held),
        limit => pair(memory.held, limit, "/"),
    }
}

/// `  {word}` and, after a colon, what squeezes the memory, only the parts there are: the
/// subsystem over its share, the tasks held back, the releases.
fn squeeze(word: &str, memory: &Memory) -> String {
    let mut out = format!("  {word}"); // alloc: a row's text
    let mut sep = ": ";
    if let Some(over) = &memory.over {
        put!(&mut out, "{sep}{over} over its share");
        sep = ", ";
    }
    if memory.held_back > 0 {
        let tasks = if memory.held_back == 1 { "task" } else { "tasks" };
        put!(&mut out, "{sep}{} {tasks} held back", memory.held_back);
        sep = ", ";
    }
    if memory.releases > 0 {
        let releases = if memory.releases == 1 { "release" } else { "releases" };
        put!(&mut out, "{sep}{} {releases}", memory.releases);
    }
    out
}

/// How much of a download came: `34.0 of 410.0 MB`, or `34.0 MB` when its size is not known.
fn amount(download: &Download) -> String {
    match download.total {
        Some(total) => pair(download.done, total, " of "),
        None => bytes(download.done),
    }
}

/// The unit a count of bytes is said in, by decimal powers: its size and its name.
fn unit(n: u64) -> (u64, &'static str) {
    match n {
        0..1_000 => (1, "B"),
        1_000..1_000_000 => (1_000, "kB"),
        1_000_000..1_000_000_000 => (1_000_000, "MB"),
        _ => (1_000_000_000, "GB"),
    }
}

/// `n` bytes in `unit`, rounded to one decimal; whole bytes as they are.
fn in_unit(n: u64, unit: u64) -> String {
    if unit == 1 {
        return n.to_string(); // alloc: a row's text
    }
    let tenths = (u128::from(n) * 10 + u128::from(unit / 2)) / u128::from(unit);
    format!("{}.{}", tenths / 10, tenths % 10) // alloc: a row's text
}

/// A count of bytes: `34.0 MB`, `512 B`.
fn bytes(n: u64) -> String {
    let (size, name) = unit(n);
    format!("{} {name}", in_unit(n, size)) // alloc: a row's text
}

/// Two counts of bytes in the unit of the larger: `20.1 of 24.0 GB` (`sep` between them).
fn pair(a: u64, b: u64, sep: &str) -> String {
    let (size, name) = unit(a.max(b));
    format!("{}{sep}{} {name}", in_unit(a, size), in_unit(b, size)) // alloc: a row's text
}

/// A rate of bytes a second: whole from 10 of its unit (`260 kB/s`), one decimal under
/// (`1.2 MB/s`).
fn per_second(n: u64) -> String {
    let (size, name) = unit(n);
    let whole = n.saturating_add(size / 2) / size;
    match size == 1 || whole >= 10 {
        true => format!("{whole} {name}/s"),                 // alloc: a row's text
        false => format!("{} {name}/s", in_unit(n, size)), // alloc: a row's text
    }
}
