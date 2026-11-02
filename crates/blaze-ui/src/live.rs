//! The terminal's rendering: a region redrawn in place below the lines that stay.
//!
//! The region is written into one buffer kept from draw to draw, and only what changed is
//! sent: ratatui's double buffer, a line at a time. A draw that changes nothing writes
//! nothing; one that changes a line moves up to it, rewrites it and moves back. What the
//! screen shows after each draw is what clearing the region and writing it whole would
//! show (tests: `the_diffed_region_shows_what_redrawing_it_whole_did`).

use blaze_ids::radix;
use blaze_protocol::events::{CrateState, Record};

use crate::{
    Build, CYAN, Change, Count, DIM, Dur, GREEN, RESET, StripRow, digits, label, last_lines,
    staying,
};

/// The most crates the region lists by name; the rest under way are counted.
const SHOWN: usize = 6;

/// The most lines of the tree of crates ([`Pen::tree`]).
const TREE: usize = 10;

/// The most functions compiling shown under a crate ([`Pen::functions`]).
const FUNCTIONS: usize = 3;

/// How many half seconds the line of how busy the cores were shows ([`Pen::cores`]).
const SPARK: usize = 20;

/// The least time between two draws: a screen shows at most 60 frames a second, so a draw
/// sooner than this after the last one is held, and written by the first call once it is due
/// ([`Live::due_ns`]). A cold build's thousands of records a second are then at most 60
/// writes a second, each what changed since the last.
pub const FRAME_NS: u64 = 1_000_000_000 / 60;

/// Draws the region again as records come and time passes, writing what changed of it.
/// What it returns is written to the terminal as it is.
pub struct Live {
    build: Build,
    width: usize,
    color: bool,
    errors_said: bool,
    /// The region as it is on the screen, and the one drawn next: two buffers, swapped.
    shown: Frame,
    next: Frame,
    /// Whether the screen may not show `shown`: a terminal rewraps what it shows when its
    /// width changes. The next draw clears the region and writes it whole.
    stale: bool,
    /// The lines records added that stay, until the next draw writes them.
    said: String,
    scratch: Scratch,
    /// The least time between draws ([`FRAME_NS`]; 0 draws for every call).
    least_ns: u64,
    /// When the last draw was written.
    drawn_ns: Option<u64>,
    /// Whether records came since the last draw, so one is owed.
    owed: bool,
}

/// Lines of text in one buffer, each ending in a newline, and where each ends: 4 bytes a
/// line besides its text.
#[derive(Default)]
struct Frame {
    text: String,
    ends: Vec<u32>,
}

impl Frame {
    fn clear(&mut self) {
        self.text.clear();
        self.ends.clear();
    }

    fn len(&self) -> usize {
        self.ends.len()
    }

    fn start(&self, line: usize) -> usize {
        line.checked_sub(1).map_or(0, |it| self.ends[it] as usize)
    }

    /// Line `line`, with its newline.
    fn line(&self, line: usize) -> &str {
        &self.text[self.start(line)..self.ends[line] as usize]
    }

    /// Ends the line written last.
    fn end(&mut self) {
        self.text.push('\n');
        self.ends.push(self.text.len() as u32);
    }

    fn lines(&self) -> impl Iterator<Item = &str> {
        (0..self.len()).map(|it| self.line(it).strip_suffix('\n').unwrap_or_default())
    }
}

/// What drawing the tree needs besides the build, kept from draw to draw.
#[derive(Default)]
struct Scratch {
    strip: Vec<StripRow>,
    /// By row: whether the tree has shown it.
    shown: Vec<bool>,
    stack: Vec<Visit>,
    /// The dependencies of a node to show: how long each was busy, its place among the
    /// node's, and the node.
    deps: Vec<(u64, u32, u32)>,
    /// The rows that held the build up ([`Build::critical_path_into`]).
    chain: Vec<usize>,
}

/// A node of the tree to show: what its lines start with, and whether it is its parent's
/// last (`None` for a root). 8 bytes.
#[derive(Clone, Copy)]
struct Visit {
    node: u32,
    indent: Indent,
    last: Option<bool>,
}

const _: () = assert!(size_of::<Visit>() == 8);

/// What the lines of a node's children start with: a column for each of its ancestors (but
/// the root), `│  ` or, under one that was its parent's last, three spaces. Five at most:
/// deeper, a chain goes on at the same indent.
#[derive(Clone, Copy, Default)]
struct Indent {
    depth: u8,
    lasts: u8,
}

impl Indent {
    const MOST: u8 = 5;

    fn with(self, last: bool) -> Indent {
        Indent { depth: self.depth + 1, lasts: self.lasts | u8::from(last) << self.depth }
    }

    fn write(self, out: &mut String) {
        for at in 0..self.depth {
            out.push_str(if (self.lasts >> at) & 1 == 1 { "   " } else { "\u{2502}  " });
        }
    }
}

impl Live {
    pub fn new(width: usize, color: bool) -> Live {
        Live {
            build: Build::default(),
            width: width.max(20),
            color,
            errors_said: false,
            shown: Frame::default(),
            next: Frame::default(),
            stale: false,
            // alloc: none until the first line that stays, then kept from draw to draw
            said: String::new(),
            scratch: Scratch::default(),
            least_ns: FRAME_NS,
            drawn_ns: None,
            owed: false,
        }
    }

    /// Draws at most once every `least_ns` ([`FRAME_NS`] by default); 0 draws for each
    /// record and tick, as a test that checks the screen after each one wants.
    pub fn frames(self, least_ns: u64) -> Live {
        Live { least_ns, ..self }
    }

    /// When the draw held back for records that came may be written: the caller ticks then.
    /// `None` when nothing is owed.
    pub fn due_ns(&self) -> Option<u64> {
        self.owed.then(|| self.drawn_ns.map_or(0, |drawn| drawn.saturating_add(self.least_ns)))
    }

    /// For the command that asked for the build, which says its errors in full after it: a
    /// failed build has no last line of its own ([`crate::last_lines`]).
    pub fn errors_said(self) -> Live {
        Live { errors_said: true, ..self }
    }

    pub fn build(&self) -> &Build {
        &self.build
    }

    pub fn resize(&mut self, width: usize) {
        let width = width.max(20);
        if width != self.width {
            self.width = width;
            self.stale = true;
        }
    }

    /// What to write for `record` at `now_ns`: the lines it adds that stay, and what changed
    /// of the region below them; at the build's end, the summary in the region's place.
    pub fn record(&mut self, record: &Record, now_ns: u64) -> String {
        // alloc: the one-call form for the CLI; a loop writes `records_into` one buffer
        let mut out = String::new();
        self.record_into(record, now_ns, &mut out);
        out
    }

    /// [`Live::record`], written at the end of `out`.
    pub fn record_into(&mut self, record: &Record, now_ns: u64, out: &mut String) {
        self.records_into(std::slice::from_ref(record), now_ns, out);
    }

    /// What to write for `records`, come together, at `now_ns`: the lines each adds that
    /// stay, in order, and the region drawn once below them. What the screen shows after it
    /// is what it shows after [`Live::record`] of each. Within a frame of the last draw,
    /// nothing is written but a build's end; the lines and the region wait for the draw that
    /// is due next.
    pub fn records_into(&mut self, records: &[Record], now_ns: u64, out: &mut String) {
        let mut said = std::mem::take(&mut self.said);
        for record in records {
            let change = self.build.apply(record);
            staying(&self.build, &change, self.color, true, self.errors_said, &mut said);
            if change == Change::Finished {
                self.finish_with(&said, out);
                said.clear();
            }
        }
        self.said = said;
        self.owed = true;
        self.frame(now_ns, out);
    }

    /// The draw at `now_ns`, when a frame has passed since the last one.
    fn frame(&mut self, now_ns: u64, out: &mut String) {
        if self.drawn_ns.is_some_and(|drawn| now_ns.saturating_sub(drawn) < self.least_ns) {
            return;
        }
        let said = std::mem::take(&mut self.said);
        self.draw(&said, now_ns, out);
        self.said = said;
        self.said.clear();
        self.drawn_ns = Some(now_ns);
        self.owed = false;
    }

    /// What changed of the region for the time that passed.
    pub fn tick(&mut self, now_ns: u64) -> String {
        // alloc: the one-call form for the CLI; a loop writes `tick_into` one buffer
        let mut out = String::new();
        self.tick_into(now_ns, &mut out);
        out
    }

    /// [`Live::tick`], written at the end of `out`: the draw held back, or the region again
    /// for the time that passed, once a frame has passed since the last draw.
    pub fn tick_into(&mut self, now_ns: u64, out: &mut String) {
        self.frame(now_ns, out);
    }

    /// The region cleared, and the summary in its place when the build finished; lines
    /// held for a draw are written first.
    pub fn finish(&mut self) -> String {
        // alloc: once per build, its end
        let mut out = String::new();
        let said = std::mem::take(&mut self.said);
        self.finish_with(&said, &mut out);
        self.said = said;
        self.said.clear();
        self.owed = false;
        out
    }

    fn finish_with(&mut self, said: &str, out: &mut String) {
        self.clear(out);
        out.push_str(said);
        if let Some(finished) = &self.build.finished {
            for line in last_lines(finished, self.color, self.errors_said) {
                out.push_str(&line);
                out.push('\n');
            }
        }
    }

    /// Clears the region: up to its first line, and everything below.
    fn clear(&mut self, out: &mut String) {
        let drawn = self.shown.len();
        if drawn > 0 {
            put!(out, "\r\x1b[{drawn}A\x1b[J");
        }
        self.shown.clear();
    }

    /// Writes `said`, the lines that stay, and the region at `now_ns` below them. With none
    /// to write above it, only the lines of the region that changed.
    fn draw(&mut self, said: &str, now_ns: u64, out: &mut String) {
        let pen = Pen { build: &self.build, width: self.width, color: self.color };
        pen.region(now_ns, &mut self.next, &mut self.scratch);
        if self.stale || !said.is_empty() {
            self.clear(out);
            out.push_str(said);
            out.push_str(&self.next.text);
        } else {
            changed(&self.shown, &self.next, out);
        }
        std::mem::swap(&mut self.shown, &mut self.next);
        self.stale = false;
    }

    /// What `blaze status` says of the build: the region at `now_ns` while it goes on, and
    /// once it is over every kind of step it did and its last lines.
    pub fn snapshot(&self, now_ns: u64) -> Vec<String> {
        let pen = Pen { build: &self.build, width: self.width, color: self.color };
        let (mut frame, mut scratch) = (Frame::default(), Scratch::default());
        let Some(finished) = &self.build.finished else {
            pen.region(now_ns, &mut frame, &mut scratch);
            // alloc: `blaze status`, once per ask
            return frame.lines().map(str::to_owned).collect();
        };
        pen.strip(finished.at_ns, &mut frame, &mut scratch.strip);
        // alloc: `blaze status`, once per ask
        let mut lines: Vec<String> = frame.lines().map(str::to_owned).collect();
        lines.extend(last_lines(finished, self.color, false));
        lines
    }

    /// How many lines of the region are on the screen.
    #[cfg(test)]
    pub(crate) fn drawn(&self) -> usize {
        self.shown.len()
    }
}

/// Writes what turns the region `was`, on the screen with the cursor on the line below it,
/// into `now`, leaving the cursor below that: each line that differs rewritten in place,
/// the lines `now` adds written after, the ones it drops cleared. When rewriting everything
/// from the first line that differs is shorter, that.
fn changed(was: &Frame, now: &Frame, out: &mut String) {
    let (drawn, lines) = (was.len(), now.len());
    if drawn == lines && was.text == now.text {
        return;
    }
    let common = drawn.min(lines);
    let first = (0..common).find(|&it| was.line(it) != now.line(it)).unwrap_or(common);
    let from = out.len();
    if drawn > 0 {
        out.push('\r');
    }
    let mut row = drawn;
    for at in first..common {
        let line = now.line(at);
        if was.line(at) != line {
            go(out, row, at);
            out.push_str("\x1b[2K");
            out.push_str(line);
            row = at + 1;
        }
    }
    if lines > drawn {
        go(out, row, drawn);
        out.push_str(&now.text[now.start(drawn)..]);
    } else if lines < drawn {
        go(out, row, lines);
        out.push_str("\x1b[J");
    } else {
        go(out, row, drawn);
    }
    // Up to the first that differs, clear below and write the rest.
    let up = drawn - first;
    let moves = if up > 0 { 3 + digits(up as u64) + 3 } else { 0 };
    let whole = usize::from(drawn > 0) + moves + (now.text.len() - now.start(first));
    if out.len() - from > whole {
        out.truncate(from);
        if drawn > 0 {
            out.push('\r');
        }
        if up > 0 {
            put!(out, "\x1b[{up}A\x1b[J");
        }
        out.push_str(&now.text[now.start(first)..]);
    }
}

/// Moves the cursor from line `from` of the region to line `to`, in its first column.
fn go(out: &mut String, from: usize, to: usize) {
    match to.cmp(&from) {
        std::cmp::Ordering::Less => put!(out, "\x1b[{}A", from - to),
        std::cmp::Ordering::Greater => put!(out, "\x1b[{}B", to - from),
        std::cmp::Ordering::Equal => {}
    }
}

/// Writes the region of a build: its lines, each cut to the width.
struct Pen<'a> {
    build: &'a Build,
    width: usize,
    color: bool,
}

impl Pen<'_> {
    /// The region at `now_ns`: the command and how long it has taken, every kind of step so
    /// far, what is under way with the time it has taken, and a count of what is done. Each
    /// line fits the width, so the region is as many rows as lines.
    ///
    /// ```text
    /// blaze build · cold: the store is empty                                     12.4s
    /// ✓ workspace 0.31s  ✓ names 2.37s  ● build scripts 3/14 steps 2.10s
    ///     Building anyhow, compiling its build script 1.20s · 3/14 steps
    /// ```
    fn region(&self, now_ns: u64, out: &mut Frame, scratch: &mut Scratch) {
        out.clear();
        if self.build.finished.is_some() {
            return;
        }
        self.header(now_ns, out);
        self.strip(now_ns, out, &mut scratch.strip);
        self.cores(now_ns, out);
        self.under_way(now_ns, out, scratch);
    }

    /// `blaze build · cold: the store is empty`, and how long it has taken at the right.
    fn header(&self, now_ns: u64, out: &mut Frame) {
        let build = self.build;
        let text = &mut out.text;
        let right = Dur(now_ns);
        let room = self.width.saturating_sub(right.chars() + 2);
        if self.color {
            text.push_str(GREEN);
        }
        let from = text.len();
        text.push_str("blaze ");
        text.push_str(build.command.as_deref().unwrap_or("build"));
        if let Some(why) = build.cache.as_ref().and_then(|it| it.cold.as_deref()) {
            text.push_str(" \u{b7} cold: ");
            text.push_str(why);
        }
        let left = cut(text, from, room);
        if self.color {
            text.push_str(RESET);
        }
        let pad = self.width.saturating_sub(left + right.chars() + 1);
        text.extend(std::iter::repeat_n(' ', pad));
        put!(text, "{}", crate::Painted(DIM, right, self.color));
        out.end();
    }

    /// Each kind of step so far, done or under way, with its count and time; the first done
    /// ones counted when they don't fit.
    fn strip(&self, now_ns: u64, out: &mut Frame, rows: &mut Vec<StripRow>) {
        let build = self.build;
        build.strip_into(now_ns, rows);
        if rows.is_empty() {
            return;
        }
        for row in rows.iter_mut() {
            // `✓ label 3/14 steps 0.21s`
            let count = self.count(row).map_or(0, |it| 1 + it.chars());
            row.chars = (2 + label(row.step).len() + count + 1 + Dur(row.took_ns).chars()) as u32;
        }
        let counted = |from: usize| if from > 0 { digits(from as u64) + 9 } else { 0 };
        let width = |from: usize| {
            let shown: usize = rows[from..].iter().map(|it| it.chars as usize + 2).sum();
            counted(from) + shown - 2
        };
        let room = self.width.saturating_sub(1);
        let mut first = 0;
        while first + 1 < rows.len() && !rows[first].running && width(first) > room {
            first += 1;
        }
        let text = &mut out.text;
        let from = text.len();
        if first > 0 {
            put!(text, "\u{2713} {first} more  ");
        }
        // Too narrow for even the last: its text alone, cut to the width.
        let fits = width(first) <= room;
        for (at, row) in rows.iter().enumerate().skip(first) {
            if at > first {
                text.push_str("  ");
            }
            let mark = if row.running { "\u{25cf}" } else { "\u{2713}" };
            match fits && self.color {
                true => {
                    let color = if row.running { CYAN } else { GREEN };
                    put!(text, "{color}{mark}{RESET}");
                }
                false => text.push_str(mark),
            }
            put!(text, " {}", label(row.step));
            if let Some(count) = self.count(row) {
                put!(text, " {count}");
            }
            put!(text, " {}", Dur(row.took_ns));
        }
        if !fits && let Some((at, _)) = text[from..].char_indices().nth(room) {
            text.truncate(from + at);
        }
        out.end();
    }

    /// The count a kind of step in the line of steps shows.
    fn count(&self, row: &StripRow) -> Option<Count<'_>> {
        let stepped = self.build.steps.get(row.count as usize)?;
        Some(Count(stepped.progress.as_ref()?, stepped.step))
    }

    /// How busy the worker's cores were over the last [`SPARK`] half seconds, and now:
    /// `cores ▁▂▇█████▇▆▃▁▁▁ 1.0/16 busy`, so that a build on one core shows at once.
    fn cores(&self, now_ns: u64, out: &mut Frame) {
        const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let build = self.build;
        let Some(now) = build.busy_at(now_ns) else { return };
        let of = build.cores_of.max(1);
        self.fit(out, DIM, |it| it.push_str("cores"), |it| {
            for at in 0..SPARK {
                let back = (SPARK - 1 - at) as u64 * 500_000_000;
                it.push(match now_ns.checked_sub(back).and_then(|it| build.busy_at(it)) {
                    Some(busy) => {
                        let level =
                            (u64::from(busy) * 7 + u64::from(of) * 50) / (u64::from(of) * 100);
                        BLOCKS[level.min(7) as usize]
                    }
                    None => ' ',
                });
            }
            put!(it, " {}.{}/{of} busy", now / 100, now % 100 / 10);
        });
    }

    /// The step under way and what it does, and the crates under way.
    fn under_way(&self, now_ns: u64, out: &mut Frame, scratch: &mut Scratch) {
        let build = self.build;
        let since = |began: u64| Dur(now_ns.saturating_sub(began));
        // Between one step's end and the next one's start, the step that ended.
        let step = build.running().last().or_else(|| build.steps.last());
        let word = step.map_or("Waiting", |it| it.step.word());
        self.fit(out, GREEN, |it| put!(it, "{word:>12}"), |it| {
            const AND: &str = " \u{b7} ";
            match step {
                Some(step) => put!(it, "{} {}", step.what(), since(step.began_ns)),
                None => put!(it, "for the daemon {}", since(0)),
            }
            // The step's own count says more than the totals: what it is doing now.
            if let Some(step) = step
                && let Some(progress) = &step.progress
            {
                put!(it, "{AND}{}", Count(progress, step.step));
            }
            // With a graph, the tree's last line counts the crates.
            let crates = build.crates.len();
            if crates > 0 && build.graph.is_empty() {
                put!(it, "{AND}{}/{crates} crates", build.index.ended);
            }
            // A crate whose total is not known yet counts in the done and not in the total.
            let fns = build.fns();
            if fns.total > 0 && build.index.unknown == 0 {
                put!(it, "{AND}{}/{} fns", fns.done, fns.total);
            } else if fns.done > 0 {
                put!(it, "{AND}{} fns", fns.done);
            }
            if let Some(longest) = step.and_then(|it| it.progress.as_ref()?.longest.as_deref()) {
                put!(it, "{AND}longest {longest}");
            }
        });
        if !build.graph.is_empty() {
            self.tree(now_ns, out, scratch);
            return;
        }
        // The longest under way first: what the build waits on.
        let running = build.in_state(CrateState::Running);
        let (mut top, mut shown) = ([0; SHOWN], 0);
        for (row, krate) in build.crates.iter().enumerate() {
            if krate.state != CrateState::Running {
                continue;
            }
            let began = |it: &usize| build.crates[*it].began_ns;
            let place = top[..shown].iter().position(|it| began(it) > krate.began_ns);
            let place = place.unwrap_or(shown);
            if place == SHOWN {
                continue;
            }
            shown = (shown + 1).min(SHOWN);
            top.copy_within(place..shown - 1, place + 1);
            top[place] = row;
        }
        for (at, &row) in top[..shown].iter().enumerate() {
            let krate = &build.crates[row];
            let last = at + 1 == shown && running == shown;
            let branch = if last { "\u{2514}\u{2500}" } else { "\u{251c}\u{2500}" };
            self.fit(out, CYAN, |it| put!(it, "{branch:>14}"), |it| {
                it.push_str(&krate.name);
                // A compile under way knows how many it did, and not yet of how many.
                match (krate.fns.total, krate.fns.done) {
                    (0, 0) => {}
                    (0, done) => put!(it, "  {done} fns"),
                    (total, done) => put!(it, "  {done}/{total} fns"),
                }
                put!(it, "  {}", since(krate.began_ns));
            });
            let under = if last { "" } else { "\u{2502}" };
            self.functions(out, &krate.name, now_ns, |it| put!(it, "{under:>13}"));
        }
        if running > shown {
            let more = running - shown;
            let branch = |it: &mut String| put!(it, "{:>14}", "\u{2514}\u{2500}");
            self.fit(out, CYAN, branch, |it| put!(it, "{more} more under way"));
        }
        let up_to_date = build.in_state(CrateState::UpToDate);
        let built = build.in_state(CrateState::Built);
        if up_to_date + built > 0 {
            let done = |it: &mut String| put!(it, "{built} built, {up_to_date} up to date");
            self.fit(out, DIM, |it| put!(it, "{:>12}", ""), done);
        }
    }

    /// The crates under way as a tree, from the program down what it waits for: each crate
    /// being resolved, checked or compiled with what it did so far and for how long, under
    /// the crates waiting for it; what is done is left out, and counted in the last line with
    /// what held the build up once the graph says it ([`Build::critical_path`]).
    ///
    /// ```text
    ///             anim waiting
    ///             ├─ wgpu  compiling 1201 fns  4.10s
    ///             └─ winit  resolving  0.80s
    ///             98 done (40 up to date) · 2 under way · 21 waiting
    /// ```
    fn tree(&self, now_ns: u64, out: &mut Frame, scratch: &mut Scratch) {
        let build = self.build;
        let (graph, index) = (&build.graph, &build.index);
        let krate = |node: u32| &build.crates[graph[node as usize].row as usize];
        let Scratch { shown, stack, deps, chain, .. } = scratch;
        // By row: the nodes of a crate are one line.
        shown.clear();
        shown.resize(build.crates.len(), false);
        stack.clear();
        // Depth first from each node nothing waits for, the branch busy longest first,
        // taking the roots in order as the stack runs out.
        let root = |at: &usize| !index.waited_for[*at] && index.under_way[*at];
        let mut roots = (0..graph.len()).filter(root);
        let first = out.len();
        let mut shown_busy = 0;
        loop {
            let Visit { node, indent, last } = match stack.pop() {
                Some(visit) => visit,
                None => match roots.next() {
                    Some(at) => Visit { node: at as u32, indent: Indent::default(), last: None },
                    None => break,
                },
            };
            let row = graph[node as usize].row as usize;
            if shown[row] {
                continue;
            }
            if out.len() - first == TREE {
                break;
            }
            shown[row] = true;
            let it = &build.crates[row];
            let busy = it.is_busy();
            shown_busy += usize::from(busy);
            let glyph = match last {
                None => "",
                Some(true) => "\u{2514}\u{2500}",
                Some(false) => "\u{251c}\u{2500}",
            };
            // A crate's branches start under its name.
            let word = |out: &mut String| match last {
                None => put!(out, "{:12}", ""),
                Some(_) => {
                    put!(out, "{:13}", "");
                    indent.write(out);
                    out.push_str(glyph);
                }
            };
            self.fit(out, CYAN, word, |out| {
                out.push_str(&it.name);
                if !busy {
                    out.push_str(" waiting");
                    return;
                }
                put!(out, "  {}", it.word());
                match (it.fns.total, it.fns.done) {
                    (_, 0) => {}
                    (0, done) => put!(out, " {done} fns"),
                    (total, done) => put!(out, " {done}/{total} fns"),
                }
                put!(out, "  {}", Dur(now_ns.saturating_sub(it.since_ns)));
            });
            if busy {
                // Under the name, beside the branches its children hang from.
                let under = |out: &mut String| match last {
                    None => put!(out, "{:12}", ""),
                    Some(last) => {
                        put!(out, "{:13}", "");
                        indent.write(out);
                        out.push_str(if last { "  " } else { "\u{2502} " });
                    }
                };
                self.functions(out, &it.name, now_ns, under);
            }
            deps.clear();
            for (place, &dep) in build.deps(node as usize).iter().enumerate() {
                if index.under_way[dep as usize] && !shown[graph[dep as usize].row as usize] {
                    deps.push((krate(dep).busy_ns(now_ns), place as u32, dep));
                }
            }
            radix::sort_copy_by_key(deps, |&(busy, place, _)| radix::pack_64(!busy, u64::from(place)));
            // Deeper than this, a chain goes on at the same indent.
            let indent = match last {
                Some(last) if indent.depth < Indent::MOST => indent.with(last),
                _ => indent,
            };
            let count = deps.len();
            for (at, &(_, _, dep)) in deps.iter().enumerate().rev() {
                stack.push(Visit { node: dep, indent, last: Some(at + 1 == count) });
            }
        }
        let blank = |out: &mut String| put!(out, "{:12}", "");
        let hidden = index.busy_graphed as usize - shown_busy;
        if hidden > 0 {
            self.fit(out, CYAN, blank, |out| put!(out, "{hidden} more under way"));
        }
        let state = |state| build.in_state(state);
        let up_to_date = state(CrateState::UpToDate);
        let done = up_to_date + state(CrateState::Built) + state(CrateState::Failed);
        let busy_now =
            state(CrateState::Resolving) + state(CrateState::Checking) + state(CrateState::Running);
        let waiting = build.crates.len() - done - busy_now;
        let chain_ns = build.critical_path_into(now_ns, chain);
        let chain = &*chain;
        self.fit(out, DIM, blank, |out| {
            put!(out, "{done} done");
            if up_to_date > 0 {
                put!(out, " ({up_to_date} up to date)");
            }
            put!(out, " \u{b7} {busy_now} under way \u{b7} {waiting} waiting");
            if chain.len() > 1 && chain_ns > 0 {
                put!(out, " \u{b7} longest chain {}: ", Dur(chain_ns));
                for (at, &row) in chain.iter().enumerate() {
                    if at > 0 {
                        out.push_str(" \u{2190} ");
                    }
                    out.push_str(&build.crates[row].name);
                }
            }
        });
    }

    /// The functions of crate `krate` compiling longest (`Build::compiling_top`), the top
    /// [`FUNCTIONS`] of them, each a line under the crate's starting with `under`.
    ///
    /// ```text
    ///             ├─ regex_automata  compiling 7318 fns  2.55s
    ///             │    regex_automata::meta::Regex::search 1.20s
    /// ```
    fn functions(&self, out: &mut Frame, krate: &str, now_ns: u64, under: impl Fn(&mut String)) {
        let (top, len) = self.build.compiling_top::<FUNCTIONS>(krate, now_ns);
        for &(at, ns) in &top[..len] {
            let name = &self.build.compiling[at].0.name;
            self.fit(out, DIM, &under, |out| put!(out, "  {name} {}", Dur(ns)));
        }
    }

    /// A line of `word` painted and `rest` after it, cut to the width: a line that wrapped
    /// would be two rows, and the region would no longer be the rows it is drawn on.
    fn fit(
        &self,
        out: &mut Frame,
        color: &str,
        word: impl FnOnce(&mut String),
        rest: impl FnOnce(&mut String),
    ) {
        let text = &mut out.text;
        if self.color {
            text.push_str(color);
        }
        let from = text.len();
        word(text);
        let word = text[from..].chars().count();
        if self.color {
            text.push_str(RESET);
        }
        text.push(' ');
        let from = text.len();
        rest(text);
        cut(text, from, self.width.saturating_sub(word + 2));
        out.end();
    }
}

/// Cuts what `text` has from `from` on to `room` characters, its last an ellipsis when it
/// was longer, and says how many characters it is.
fn cut(text: &mut String, from: usize, room: usize) -> usize {
    let chars = text[from..].chars().count();
    if chars <= room {
        return chars;
    }
    let keep = room.saturating_sub(1);
    let at = text[from..].char_indices().nth(keep).map_or(text.len(), |it| from + it.0);
    text.truncate(at);
    text.push('\u{2026}');
    keep + 1
}
