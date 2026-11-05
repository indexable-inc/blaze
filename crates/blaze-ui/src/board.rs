//! The live view of a build (`blaze build`, `check` and `test`), drawn from one [`Now`]
//! (docs/v2/plan/observe/build-view.md, "The renderer"). A sans-IO state machine: a [`Board`]
//! is fed the build's model as it comes ([`Board::take`]) and the keys pressed ([`Board::key`]),
//! and draws it into a ratatui [`Buffer`] ([`Board::render`]) in one of two [`Form`]s, or, off a
//! terminal, as one line of text when one is due ([`Board::line`]). It owns no terminal, thread
//! or clock: its time is the model's `at_ms`, and what it draws depends only on the last `Now`
//! and whether it is expanded.
//!
//! [`Form::Full`], top to bottom, each section only when it has rows:
//!
//! ```text
//!  blaze test  compiling                                   488 of 570 tasks   15m14s
//!  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━──────────  1 running · 81 waiting
//!  memory  106.3 GB, no limit
//!  ⚠ slow: no task settled for 4m30s; 1 of 16 workers busy
//!
//!  workers  1 busy · 15 idle
//!    blaze-worker-3  codegen  core::ptr::is_freeze  core  task > codegen > is_freeze 4m30s
//!
//!  blocked on
//!    codegen core  running 4m30s  holds 82  chain 41
//!      std build script, libc build script, alloc lib, +79
//!
//!  slowest so far
//!    bodies hir-ty 1m12s · codegen syntax 48.0s · script-run ring 31.0s
//!
//!  e expand · blaze explore for the graph · blaze debug live for every thread
//! ```
//!
//! The memory line is always there, colored by pressure; over the limit it moves above the
//! header. When the area is shorter than the content, sections go from the bottom up, then
//! the footer; the header, the memory and the warning stay while there is room. Drawing never
//! panics on any size, 0 by 0 included: every write is clipped to the area.

#![blaze::sans_io]

use std::time::Duration;

use blaze_protocol::now::{Blocker, Busy, Download, Memory, Now, Phase, Pressure, STALL_MS};
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
    /// A few rows under the scrollback, at most [`INLINE_ROWS`]: the header with the memory,
    /// the warning, two busy workers, the first blocker, the first download.
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
pub const INLINE_ROWS: usize = 6;

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

/// Busy workers the inline form lists.
const INLINE_BUSY: usize = 2;

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
    /// The first blocker's task, by its plane node.
    blocker: Option<u32>,
    pressure: Pressure,
    stalled: bool,
    slow: bool,
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
}

/// A section of the full form: its rows, the blank one above it included.
struct Block {
    keep: Keep,
    rows: Vec<Line<'static>>,
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
        };
        let due = match self.last {
            None => true,
            Some(last) => {
                last.phase != mark.phase
                    || last.blocker != mark.blocker
                    || last.pressure != mark.pressure
                    || (mark.stalled && !last.stalled)
                    || (mark.slow && !last.slow)
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
        let one = |keep: Keep, row: Line<'static>| Block { keep, rows: vec![row] }; // alloc: a row
        let mut blocks = Vec::with_capacity(12); // alloc: one a draw
        if over {
            blocks.push(one(Keep::Always, self.memory_row(width)));
        }
        blocks.push(one(Keep::Always, self.header(width)));
        if now.tally.total > 0 {
            blocks.push(one(Keep::Drop, self.gauge(width)));
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
        if now.workers > 0 || !now.busy.is_empty() {
            blocks.push(Block { keep: Keep::Drop, rows: self.workers(width) });
        }
        if !now.blockers.is_empty() {
            blocks.push(Block { keep: Keep::Drop, rows: self.blocked(width) });
        }
        if !now.downloads.is_empty() {
            blocks.push(Block { keep: Keep::Drop, rows: self.downloads(width) });
        }
        if !now.slowest.is_empty() {
            blocks.push(Block { keep: Keep::Drop, rows: self.slowest(width) });
        }
        let footer = vec![Line::default(), self.footer()]; // alloc: a row
        blocks.push(Block { keep: Keep::Footer, rows: footer });
        blocks
    }

    /// The inline form's rows: the header with the memory, the warning (or the memory line
    /// when it is squeezed or over), the first busy workers, the first blocker and the first
    /// download.
    fn inline(&self, width: usize) -> Vec<Line<'static>> {
        let now = &self.now;
        let mut rows = Vec::with_capacity(INLINE_ROWS); // alloc: one a draw
        rows.push(self.inline_header(width));
        if let Some(warning) = self.warning() {
            rows.push(warning);
        } else if matches!(now.memory.pressure(), Pressure::Squeezed | Pressure::Over) {
            rows.push(self.memory_row(width));
        }
        for busy in now.busy.iter().take(INLINE_BUSY) {
            rows.push(self.busy_row(busy, width));
        }
        if let Some(blocker) = now.blockers.first() {
            let label = fit(&label(blocker), width / 2);
            rows.push(self.blocker_row(" blocked on ", blocker, label, 0, state(blocker), 0));
        }
        if let Some(download) = now.downloads.first() {
            rows.push(self.download_row(" downloading ", download, width));
        }
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
            let tasks = format!("{} of {} tasks", now.tally.done, now.tally.total); // alloc: a row
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
            let done = format!("  {}/{}", now.tally.done, now.tally.total); // alloc: a row
            left.push(Span::styled(done, self.dim()));
            if now.workers > 0 {
                let busy = format!("  {}/{} busy", now.busy.len(), now.workers); // alloc: a row
                left.push(Span::styled(busy, self.dim()));
            }
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

    /// ` workers  1 busy · 15 idle`, then a row for each busy worker.
    fn workers(&self, width: usize) -> Vec<Line<'static>> {
        let now = &self.now;
        let shown = self.shown(now.busy.len(), BUSY_ROWS);
        let mut rows = Vec::with_capacity(shown + 3); // alloc: a section
        rows.push(Line::default());
        let counts = format!("  {} busy · {} idle", now.busy.len(), now.idle()); // alloc: a row
        rows.push(Line::from(vec![
            Span::styled(" workers", bold()),
            Span::styled(counts, self.dim()),
        ])); // alloc: a row
        for busy in &now.busy[..shown] {
            rows.push(self.busy_row(busy, width));
        }
        rows.extend(self.more(now.busy.len(), shown, "busy"));
        rows
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
            left.push(Span::styled(task.kind.name(), self.dim()));
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

    /// ` blocked on`, then a row for each blocker with what it holds back under it, the labels
    /// and states in columns.
    fn blocked(&self, width: usize) -> Vec<Line<'static>> {
        let blockers = &self.now.blockers[..self.shown(self.now.blockers.len(), BLOCKER_ROWS)];
        let labels: Vec<String> =
            blockers.iter().map(|it| fit(&label(it), width / 2)).collect(); // alloc: a section
        let states: Vec<String> = blockers.iter().map(state).collect(); // alloc: a section
        let label_width = labels.iter().map(|it| text_width(it)).max().unwrap_or(0);
        let state_width = states.iter().map(|it| text_width(it)).max().unwrap_or(0);
        let mut rows = Vec::with_capacity(blockers.len() * 2 + 3); // alloc: a section
        rows.push(Line::default());
        rows.push(Line::from(Span::styled(" blocked on", bold())));
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
            let (kind, name) = (took.task.kind.name(), &took.task.name);
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

/// The full form's rows in `height` rows: the [`Keep::Drop`] blocks go from the bottom up, then
/// the footer, and what still does not fit is cut at the bottom.
fn shorten(mut blocks: Vec<Block>, height: usize) -> Vec<Line<'static>> {
    let mut total: usize = blocks.iter().map(|it| it.rows.len()).sum();
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
/// `[15m14s] compiling 488/570 · 1/16 busy · mem 20.1/24.0 GB squeezed · on codegen core
/// (core::ptr::is_freeze, 4m30s), holds 82 · slow: nothing settled for 4m30s`. After the
/// memory it names the first blocker, else the first download, else the stage.
fn compact(now: &Now, out: &mut String) {
    let start = out.len();
    put!(out, "[{}] {}", time(now.at_ms), now.phase.word());
    if now.tally.total > 0 {
        put!(out, " {}/{}", now.tally.done, now.tally.total);
        if now.workers > 0 {
            put!(out, " · {}/{} busy", now.busy.len(), now.workers);
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
    let end = start + out[start..].trim_end().len();
    out.truncate(end);
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// Milliseconds as the view says a time ([`duration`]).
fn time(ms: u64) -> String {
    duration(Duration::from_millis(ms))
}

/// A blocker's task: `codegen core`.
fn label(blocker: &Blocker) -> String {
    format!("{} {}", blocker.task.kind.name(), blocker.task.name) // alloc: a row's text
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
