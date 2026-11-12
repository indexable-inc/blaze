//! The core: events in ([`Screen::take`]), the clock injected ([`Screen::advance_clock`]),
//! frames out ([`Screen::frame`]). It owns no terminal, clock, thread or file.
//!
//! What it shows (docs/terminal.md):
//!
//! - while a build runs on a terminal, a live region: a row for each unit under way (its
//!   stage, how long, how many wait on it, what it waits on, and the function it is inside
//!   once that has been slow), then one status row (done of total, the phase, the time).
//!   Memory and cores are named only when they are why something waits;
//! - when the build ends the region is erased and at most one line takes its place: nothing
//!   for a `run` that built nothing, what was recompiled and why for an edit, the counts and
//!   the time for a larger build, what failed and what to run next for a failure;
//! - off a terminal nothing is redrawn: a `blaze: progress` line every [`PLAIN_EVERY_MS`] of
//!   a long build, the diagnostics, and one `blaze: ok` or `blaze: failed` line.

use alloc::{
    boxed::Box,
    format,
    string::{String, ToString as _},
    vec::Vec,
};

use blaze_protocol::now::{Now, Phase};

use crate::{
    event::{BinaryEnd, Built, Command, Event, Test, Verdict, Why},
    frame::{Frame, Line, Mark, Progress, Role},
    text,
};

/// What the terminal is, as the driver found it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Caps {
    /// A terminal a region can be redrawn on. Off (a pipe, a file, CI, a dumb terminal) every
    /// line is written once and stays.
    pub live: bool,
    pub cols: u16,
    pub rows: u16,
    /// Glyphs outside ASCII are drawn.
    pub unicode: bool,
}

impl Caps {
    /// A pipe.
    pub const PIPE: Caps = Caps { live: false, cols: 0, rows: 0, unicode: false };
}

/// How the command was asked to speak.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Options {
    pub command: Command,
    /// `-q`: errors only.
    pub quiet: bool,
    /// `-v`: each unit made, each link and every why stay in the scrollback.
    pub verbose: bool,
    /// The checkout's root, for a diagnostic's path to become a link.
    pub root: Option<String>,
    /// A build at least this long ends with a notification, where the terminal has them.
    pub notify_after_ms: Option<u64>,
}

impl Options {
    pub fn of(command: Command) -> Options {
        Options { command, quiet: false, verbose: false, root: None, notify_after_ms: None }
    }
}

/// Off a terminal, how often a build under way says a progress line.
pub const PLAIN_EVERY_MS: u64 = 10_000;

/// A unit inside one function at least this long names the function on its row.
pub const SLOW_ITEM_MS: u64 = 2_000;

/// A build of at most this many tasks names the function each unit is inside from the start:
/// the work is a handful, so the function is the news.
pub const HANDFUL: u32 = 12;

/// Nothing settled for this long: the status row says so.
pub const QUIET_MS: u64 = 10_000;

/// A test running at least this long is named on its executable's row.
pub const SLOW_TEST_MS: u64 = 1_000;

/// The most lines of a failed test's output that stay: its last ones.
pub const OUTPUT_LINES: usize = 40;

/// The most functions an edit's line names.
const NAMED: usize = 3;

/// Whether a region is redrawn on the stream: it is a `terminal`, not a dumb one, not a CI
/// worker's (which may hand out a pseudo-terminal whose log is read as text), and
/// `BLAZE_UI=plain` does not ask for lines. `var` reads the environment for the caller.
pub fn live(var: impl Fn(&str) -> Option<String>, terminal: bool) -> bool {
    let set = |name: &str| var(name).is_some_and(|it| !it.is_empty() && it != "false" && it != "0");
    terminal
        && var("TERM").as_deref() != Some("dumb")
        && !set("CI")
        && !set("TF_BUILD")
        && var("BLAZE_UI").as_deref() != Some("plain")
}

/// Whether the terminal draws glyphs outside ASCII, as its locale says (`LC_ALL`, then
/// `LC_CTYPE`, then `LANG`, the first that is set).
pub fn unicode(var: impl Fn(&str) -> Option<String>) -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| var(name).filter(|it| !it.is_empty()))
        .is_some_and(|it| {
            let it = it.to_ascii_lowercase();
            it.contains("utf-8") || it.contains("utf8")
        })
}

/// A bug planted for a control: the property tests must catch each.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Plant {
    #[default]
    None,
    /// The region is not held to the terminal's height.
    Tall,
    /// A row is not cut to the terminal's width.
    Wide,
    /// The last frame erases nothing.
    KeepRegion,
    /// After a resize the rows to erase are counted at the old width.
    StaleResize,
}

/// One test executable.
#[derive(Clone, Debug)]
struct Binary {
    id: u32,
    label: String,
    rerun: String,
    total: Option<u32>,
    done: u32,
    failed: u32,
    started: bool,
    ended: bool,
    /// Each test under way and when it started.
    running: Vec<(String, u64)>,
}

#[derive(Clone, Debug, Default)]
struct Tests {
    binaries: Vec<Binary>,
    passed: u32,
    failed: u32,
    ignored: u32,
    /// The rerun command of each executable with a failure, once.
    reruns: Vec<String>,
    unbuilt: u32,
    began_ms: Option<u64>,
    over: bool,
}

/// The screen of one command.
#[derive(Debug)]
pub struct Screen {
    opts: Options,
    caps: Caps,
    now_ms: u64,
    model: Option<Box<Now>>,
    /// When the model came, on the injected clock: its timers run on from there.
    model_at: u64,
    errors: u32,
    warnings: u32,
    /// Each unit that said an error, once, in the order it did.
    failed: Vec<String>,
    built: Option<Built>,
    built_at: u64,
    /// The build's last line was said.
    ended: bool,
    /// Nothing live is left.
    cleared: bool,
    tests: Tests,
    above: Vec<Line>,
    /// The width of each region row drawn last, and the columns it was drawn at.
    drawn: Vec<u16>,
    drawn_cols: u16,
    region: Vec<Line>,
    hidden: bool,
    progress: Progress,
    notify: Option<(String, String)>,
    plain_at: u64,
    dirty: bool,
    plant: Plant,
}

impl Screen {
    pub fn new(opts: Options, caps: Caps) -> Screen {
        Screen {
            opts,
            caps,
            now_ms: 0,
            model: None,
            model_at: 0,
            errors: 0,
            warnings: 0,
            failed: Vec::new(), // alloc: none until a unit fails
            built: None,
            built_at: 0,
            ended: false,
            cleared: false,
            tests: Tests::default(),
            above: Vec::new(),  // alloc: none until a line stays
            drawn: Vec::new(),  // alloc: none until a region is drawn
            drawn_cols: 0,
            region: Vec::new(), // alloc: as above
            hidden: false,
            progress: Progress::Clear,
            notify: None,
            plain_at: 0,
            dirty: false,
            plant: Plant::None,
        }
    }

    /// The same screen with a bug planted, for a control.
    #[must_use]
    pub fn planted(mut self, plant: Plant) -> Screen {
        self.plant = plant;
        self
    }

    pub fn caps(&self) -> Caps {
        self.caps
    }

    /// The time now, in milliseconds since the command began. It never runs back.
    pub fn advance_clock(&mut self, now_ms: u64) {
        if now_ms > self.now_ms {
            self.now_ms = now_ms;
            self.dirty = true;
        }
    }

    /// When the next frame is due though no event comes: a timer on the region turns over.
    /// `None` when nothing live has a timer.
    pub fn next_deadline(&self) -> Option<u64> {
        let live = self.caps.live && !self.cleared && !self.opts.quiet;
        (live && (self.model.is_some() || self.tests.began_ms.is_some()))
            .then(|| self.now_ms / 1000 * 1000 + 1000)
    }

    /// Whether an error was said or a test failed.
    pub fn failing(&self) -> bool {
        self.errors > 0 || self.tests.failed > 0 || self.built.as_ref().is_some_and(|it| !it.ok)
    }

    pub fn take(&mut self, event: Event) {
        self.dirty = true;
        match event {
            Event::Resized { cols, rows } => {
                self.caps.cols = cols;
                self.caps.rows = rows;
            }
            Event::Now(now) => {
                self.model = Some(now);
                self.model_at = self.now_ms;
                let due = self.now_ms.saturating_sub(self.plain_at) >= PLAIN_EVERY_MS;
                if !self.caps.live && !self.opts.quiet && due && self.built.is_none() {
                    self.plain_at = self.now_ms;
                    if let Some(now) = &self.model {
                        let line = progress_line(now, self.now_ms);
                        self.above.push(Line::new().with(Role::Plain, line)); // alloc: a line every ten seconds
                    }
                }
            }
            Event::Unit { name, fresh } => {
                if self.opts.verbose && !fresh && !self.opts.quiet {
                    let line = match self.caps.live {
                        true => Line::new().with(Role::Dim, format!("  compiling {name}")), // alloc: `-v`, a line a unit
                        false => Line::new()
                            .with(Role::Plain, format!("blaze: compiling unit={}", text::value(&name))), // alloc: as above
                    };
                    self.above.push(line); // alloc: as above
                }
            }
            Event::Said(said) => {
                for line in said.strip_suffix('\n').unwrap_or(&said).split('\n') {
                    let links = marks(line, self.opts.root.as_deref());
                    self.above.push(Line::Raw { text: line.to_string(), links }); // alloc: a line that stays
                }
            }
            Event::Diagnosed { error, unit } => match error {
                true => {
                    self.errors += 1;
                    if let Some(unit) = unit
                        && !self.failed.contains(&unit)
                    {
                        self.failed.push(unit); // alloc: once a failed unit
                    }
                }
                false => self.warnings += 1,
            },
            Event::Built(built) => {
                let waits = built.why_follows;
                self.built_at = self.now_ms;
                self.built = Some(built);
                if !waits {
                    self.end_build(&[]);
                }
            }
            Event::Why(whys) => {
                if self.built.is_some() && !self.ended {
                    self.end_build(&whys);
                }
            }
            Event::Test(test) => self.test(test),
            Event::Clear => self.cleared = true,
        }
    }

    /// The next frame, or none when what the terminal holds is what it should.
    pub fn frame(&mut self) -> Option<Frame> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        let region = self.region_now();
        let above = core::mem::take(&mut self.above);
        let progress = self.progress_now();
        let mut frame = Frame::default();
        if progress != self.progress && self.caps.live {
            self.progress = progress;
            frame.progress = Some(progress);
        }
        frame.notify = self.notify.take();
        let hide = !region.is_empty();
        if hide != self.hidden {
            self.hidden = hide;
            frame.hide_cursor = Some(hide);
        }
        if above.is_empty() && region == self.region {
            return (!frame.is_empty()).then_some(frame);
        }
        // The rows the last region takes at the width the terminal has now: a terminal that
        // got narrower wrapped each row longer than its new width.
        let cols = match self.plant {
            Plant::StaleResize => self.drawn_cols.max(1),
            _ => self.caps.cols.max(1),
        };
        frame.erase = self.drawn.iter().map(|&width| width.max(1).div_ceil(cols)).sum();
        if self.plant == Plant::KeepRegion && region.is_empty() {
            frame.erase = 0;
        }
        self.drawn = region.iter().map(|it| it.width() as u16).collect(); // alloc: a row's width each, a frame
        self.drawn_cols = self.caps.cols;
        self.region.clone_from(&region); // clone: compared with the next frame's
        frame.above = above;
        frame.region = region;
        Some(frame)
    }

    fn glyph(&self, unicode: &'static str, ascii: &'static str) -> &'static str {
        match self.caps.unicode {
            true => unicode,
            false => ascii,
        }
    }

    fn progress_now(&self) -> Progress {
        if self.cleared || !self.caps.live || self.opts.quiet {
            return Progress::Clear;
        }
        if let Some(began) = self.tests.began_ms {
            let _ = began;
            let total: u32 = self.tests.binaries.iter().filter_map(|it| it.total).sum();
            let done: u32 = self.tests.binaries.iter().map(|it| it.done).sum();
            let percent = percent(done, total);
            return match (self.tests.failed > 0, total) {
                (true, _) => Progress::Failed(percent),
                (false, 0) => Progress::Unknown,
                (false, _) => Progress::Percent(percent),
            };
        }
        match &self.model {
            None => Progress::Clear,
            Some(now) if now.tally.total == 0 => Progress::Unknown,
            Some(now) => {
                let percent = percent(now.tally.done, now.tally.total);
                match now.tally.failed > 0 || self.errors > 0 {
                    true => Progress::Failed(percent),
                    false => Progress::Percent(percent),
                }
            }
        }
    }

    /// The most rows the region takes: a third of the terminal, at least four and at most
    /// twenty, and never the terminal's last row, so the cursor moves up over a region that
    /// is all on screen.
    fn most_rows(&self) -> usize {
        let rows = self.caps.rows as usize;
        (rows / 3).clamp(4, 20).min(rows.saturating_sub(1))
    }

    /// The columns a row may take: never the terminal's last, where terminals differ on when
    /// the cursor wraps.
    fn most_cols(&self) -> usize {
        (self.caps.cols as usize).saturating_sub(1)
    }

    fn region_now(&self) -> Vec<Line> {
        if !self.caps.live || self.cleared || self.opts.quiet || self.caps.cols < 2 {
            return Vec::new(); // alloc: none
        }
        let mut rows = match (&self.tests.began_ms, &self.model) {
            (Some(_), _) => self.test_rows(),
            (None, Some(now)) if self.built.is_none() => self.build_rows(now),
            _ => Vec::new(), // alloc: none
        };
        let most = self.most_rows();
        if rows.len() > most && self.plant != Plant::Tall {
            // The status row is the last: it stays, and the rows above it go from the bottom.
            let status = rows.pop();
            rows.truncate(most.saturating_sub(1));
            rows.extend(status);
            rows.truncate(most);
        }
        if self.plant != Plant::Wide {
            let cols = self.most_cols();
            for row in &mut rows {
                cut(row, cols);
            }
        }
        rows
    }

    /// The build's rows: a unit under way each, the longest first, then what waits with
    /// nothing running it, then the status row.
    fn build_rows(&self, now: &Now) -> Vec<Line> {
        let since = self.now_ms.saturating_sub(self.model_at);
        let cols = self.most_cols();
        let most = self.most_rows();
        let handful = now.tally.total <= HANDFUL;
        // One row a unit: its tasks under way are its stages.
        struct Row<'a> {
            name: &'a str,
            stages: Vec<&'static str>,
            for_ms: u64,
            item: Option<(&'a str, u64)>,
            awaits: Option<&'a str>,
            holds: u32,
        }
        let mut units: Vec<Row<'_>> = Vec::new(); // alloc: a row a unit under way, a frame
        for running in &now.running {
            let stage = stage(&running.task.kind);
            let holds = now
                .blockers
                .iter()
                .find(|it| it.task.id == running.task.id)
                .map_or(0, |it| it.holds);
            let span = running.span.as_ref();
            let item = span.and_then(|it| Some((it.item.as_deref()?, it.for_ms)));
            let awaits = span.and_then(|it| it.awaits.as_deref());
            match units.iter_mut().find(|it| it.name == running.task.name) {
                Some(row) => {
                    if !row.stages.contains(&stage) {
                        row.stages.push(stage); // alloc: as above
                    }
                    row.for_ms = row.for_ms.max(running.for_ms);
                    row.holds = row.holds.max(holds);
                    row.item = row.item.or(item);
                    row.awaits = row.awaits.or(awaits);
                }
                None => units.push(Row {
                    name: &running.task.name,
                    stages: alloc::vec![stage], // alloc: as above
                    for_ms: running.for_ms,
                    item,
                    awaits,
                    holds,
                }), // alloc: as above
            }
        }
        let room = most.saturating_sub(1);
        // Ready with no worker inside: said with why, when that is cores or memory.
        let held = now.memory.held_back;
        let idle = now.workers.saturating_sub(now.busy.len() as u32);
        let starved = match (held > 0, now.tally.ready > 0 && idle == 0 && now.workers > 0) {
            (true, _) => Some(format!(
                "{} ready, held back for memory: {} of {} GB",
                held,
                text::gb(now.memory.held),
                text::gb(now.memory.limit)
            )), // alloc: a row, a frame
            (false, true) => Some(format!(
                "{} ready, all {} cores busy",
                now.tally.ready, now.workers
            )), // alloc: as above
            (false, false) => None,
        };
        let shown = units.len().min(room.saturating_sub(usize::from(starved.is_some())));
        let more = units.len() - shown;
        let shown = if more > 0 { shown.saturating_sub(1) } else { shown };
        let more = units.len() - shown;
        let names: Vec<String> = units[..shown].iter().map(|it| text::unit(it.name)).collect(); // alloc: as above
        let name_col = names.iter().map(|it| text::width(it)).max().unwrap_or(0).min((cols / 3).max(12));
        let stage_col = units[..shown]
            .iter()
            .map(|it| it.stages.iter().map(|s| s.len() + 3).sum::<usize>().saturating_sub(3))
            .max()
            .unwrap_or(0);
        let mut rows = Vec::with_capacity(shown + 3); // alloc: the region's rows, a frame
        for (unit, name) in units[..shown].iter().zip(&names) {
            let mut row = Line::new();
            row.push(Role::Running, format!(" {} ", self.glyph("▶", ">"))); // alloc: a row's span
            let name = text::fit(name, name_col);
            let pad = name_col - text::width(&name);
            row.push(Role::Strong, name);
            let stages = unit.stages.join(" + "); // alloc: as above
            let stage_pad = stage_col.saturating_sub(stages.len());
            row.push(Role::Running, format!("{:pad$}  {stages}", "", pad = pad)); // alloc: as above
            let time = text::timer(unit.for_ms + since);
            row.push(Role::Plain, format!("{:stage_pad$}  {time:>6}", "", stage_pad = stage_pad)); // alloc: as above
            // What is left of the row says the graph first (who waits on it), then the wait
            // it is in, then the function, each only if it fits whole enough to read.
            let mut left = cols.saturating_sub(row.width());
            if unit.holds > 0 && left >= 12 {
                let said = format!("  {} {} wait on it", self.glyph("←", "<-"), unit.holds); // alloc: as above
                left = left.saturating_sub(text::width(&said));
                row.push(Role::Waiting, said);
            }
            if let Some(awaits) = unit.awaits
                && left >= 16
            {
                let said = format!("  waits on {}", text::middle(awaits, left - 11)); // alloc: as above
                left = left.saturating_sub(text::width(&said));
                row.push(Role::Waiting, said);
            }
            if let Some((item, for_ms)) = unit.item
                && (for_ms >= SLOW_ITEM_MS || handful)
                && left >= 16
            {
                row.push(Role::Dim, format!("  in {}", text::item(item, left - 5))); // alloc: as above
            }
            rows.push(row); // alloc: as above
        }
        if more > 0 {
            rows.push(Line::new().with(Role::Dim, format!("   {} {more} more running", self.glyph("…", "...")))); // alloc: as above
        }
        if let Some(starved) = starved {
            rows.push(Line::new().with(Role::Waiting, format!(" {} {starved}", self.glyph("◦", ".")))); // alloc: as above
        }
        rows.push(self.status_row(now, since));
        rows
    }

    /// ` building  ━━━━━━────────  32/147 · 3 running · 5.0s`, and what is off after it.
    fn status_row(&self, now: &Now, since: u64) -> Line {
        let cols = self.most_cols();
        let mut row = Line::new();
        let word = match now.phase {
            Phase::Compiling => match self.opts.command {
                Command::Check => "checking",
                _ => "building",
            },
            phase => phase.word(),
        };
        row.push(Role::Strong, format!(" {word}")); // alloc: the status row, a frame
        let tally = now.tally;
        if tally.total > 0 {
            let bar = (cols / 4).clamp(8, 24);
            if cols >= 50 {
                let filled = (u64::from(tally.done) * bar as u64 / u64::from(tally.total)) as usize;
                let (full, rest) = (self.glyph("━", "#"), self.glyph("─", "-"));
                row.push(Role::Done, format!("  {}", full.repeat(filled))); // alloc: as above
                row.push(Role::Dim, rest.repeat(bar - filled)); // alloc: as above
            }
            row.push(Role::Plain, format!("  {}/{}", tally.done, tally.total)); // alloc: as above
            if tally.running > 0 {
                row.push(Role::Dim, format!(" {} {} running", self.dot(), tally.running)); // alloc: as above
            }
            if tally.failed > 0 {
                row.push(Role::Failed, format!(" {} {} failed", self.dot(), tally.failed)); // alloc: as above
            }
        }
        row.push(Role::Dim, format!(" {} {}", self.dot(), text::timer(self.now_ms))); // alloc: as above
        // What the build waits on when no unit runs: a download, its plan, its inputs.
        if let Some(download) = now.downloads.first() {
            let total = download.total.map_or(String::new(), |it| format!(" of {} MB", it / 1_000_000)); // alloc: as above
            row.push(
                Role::Waiting,
                format!(
                    " {} fetching {} {} MB{total}",
                    self.dot(),
                    text::middle(&download.what, 32),
                    download.done / 1_000_000
                ),
            ); // alloc: as above
        } else if tally.running == 0 && !now.stage.is_empty() {
            row.push(Role::Waiting, format!(" {} {}", self.dot(), now.stage)); // alloc: as above
        }
        if let Some(over) = &now.memory.over {
            row.push(
                Role::Warned,
                format!(
                    " {} memory {} of {} GB, {over} over its share",
                    self.dot(),
                    text::gb(now.memory.held),
                    text::gb(now.memory.limit)
                ),
            ); // alloc: as above
        }
        let quiet = now.quiet_ms + since;
        if quiet >= QUIET_MS {
            row.push(
                Role::Warned,
                format!(" {} nothing finished for {}", self.glyph("⚠", "!"), text::timer(quiet)),
            ); // alloc: as above
        }
        if let Some(stall) = &now.stall {
            row.push(Role::Failed, format!(" {} {stall}", self.glyph("⚠", "!"))); // alloc: as above
        }
        row
    }

    fn dot(&self) -> &'static str {
        self.glyph("·", "-")
    }

    /// The build is over: its last line, into what stays.
    fn end_build(&mut self, whys: &[Why]) {
        self.ended = true;
        let Some(built) = self.built.clone() else { return }; // clone: read once, at the build's end
        if !built.tests_follow {
            self.cleared = true;
        }
        let took = self.built_at;
        let noop = built.ok && built.compiled == 0 && !built.links.iter().any(|it| it.whole);
        let wasted: Vec<&Why> = whys.iter().filter(|it| it.wasted).collect(); // alloc: once a build
        let quiet = self.opts.quiet;
        if !built.ok {
            let line = match self.caps.live {
                true => self.failed_line(&built, took),
                false => Line::new().with(Role::Plain, self.failed_plain(&built, took)),
            };
            self.above.push(line); // alloc: a line that stays
        } else if !quiet {
            match self.caps.live {
                true => {
                    // A `run` or a `test` that built nothing says nothing: what follows is
                    // the program's or the tests'.
                    let silent = noop
                        && matches!(self.opts.command, Command::Run | Command::Test)
                        && self.warnings == 0;
                    if !silent && !(built.tests_follow && noop) {
                        let line = self.built_line(&built, whys, took, noop);
                        self.above.push(line); // alloc: as above
                    }
                }
                false => {
                    let line = self.built_plain(&built, whys, took);
                    self.above.push(Line::new().with(Role::Plain, line)); // alloc: as above
                }
            }
        }
        if !quiet {
            for why in &wasted {
                let line = match self.caps.live {
                    true => {
                        let mut line = Line::new()
                            .with(Role::Warned, format!("{} ", self.glyph("⚠", "!"))) // alloc: as above
                            .with(Role::Plain, why.headline.clone()); // clone: as above
                        if let Some(more) = &why.more {
                            line.push(Role::Dim, format!(" {} {more}", self.dot())); // alloc: as above
                        }
                        line
                    }
                    false => {
                        let more = why.more.as_ref().map_or(String::new(), |it| format!(" next={}", text::value(it))); // alloc: as above
                        Line::new().with(
                            Role::Plain,
                            format!("blaze: wasted why={}{more}", text::value(&why.headline)),
                        ) // alloc: as above
                    }
                };
                self.above.push(line); // alloc: as above
            }
            if self.opts.verbose {
                for link in &built.links {
                    self.above.push(
                        Line::new().with(Role::Dim, format!("  link: {} ({})", link.path, link.how)),
                    ); // alloc: `-v`
                }
                for why in whys.iter().filter(|it| !it.wasted) {
                    let mut said = format!("  why: {}", why.headline); // alloc: `-v`
                    if !why.functions.is_empty() {
                        said.push_str(": ");
                        said.push_str(&why.functions.join(", ")); // alloc: `-v`
                    }
                    self.above.push(Line::new().with(Role::Dim, said)); // alloc: `-v`
                }
            }
        }
        if self.caps.live
            && !built.tests_follow
            && self.opts.notify_after_ms.is_some_and(|after| took >= after)
        {
            let title = match built.ok {
                true => format!("blaze {} done", self.opts.command.word()), // alloc: once a long build
                false => format!("blaze {} failed", self.opts.command.word()), // alloc: as above
            };
            self.notify = Some((title, text::time(took)));
        }
    }

    /// `✓ hello::add recompiled (src/main.rs changed) · tier 0 · 0.70s`.
    fn built_line(&self, built: &Built, whys: &[Why], took: u64, noop: bool) -> Line {
        let mut line = Line::new();
        let dot = self.dot();
        if noop {
            line.push(Role::Dim, format!("up to date {dot} {}", text::time(took))); // alloc: the build's line, once
        } else {
            line.push(Role::Done, format!("{} ", self.glyph("✓", "ok"))); // alloc: as above
            let (what, cause) = self.made(built, whys);
            line.push(Role::Plain, what);
            if let Some(cause) = cause {
                line.push(Role::Dim, format!(" ({cause})")); // alloc: as above
            }
            if let Some(tier) = &built.tier {
                line.push(Role::Dim, format!(" {dot} {tier}")); // alloc: as above
            }
            for link in built.links.iter().filter(|it| it.whole) {
                line.push(Role::Warned, format!(" {dot} {}", link.how)); // alloc: as above
            }
            line.push(Role::Dim, format!(" {dot} {}", text::time(took))); // alloc: as above
        }
        if self.warnings > 0 {
            line.push(
                Role::Warned,
                format!(" {dot} {}", text::count(u64::from(self.warnings), "warning", "warnings")),
            ); // alloc: as above
        }
        if let Some(next) = &built.next {
            line.push(Role::Dim, format!(" {dot} {next}")); // alloc: as above
        }
        // Where the program is, for a command that does not run it.
        if matches!(self.opts.command, Command::Build | Command::Other)
            && let [link] = built.links.as_slice()
        {
            let left = self.most_cols().saturating_sub(line.width() + 3);
            if left >= 12 {
                line.push(Role::Dim, format!(" {} ", self.glyph("→", "->"))); // alloc: as above
                line.push_link(Role::Dim, text::middle(&link.path, left), file_url(&link.path));
            }
        }
        line
    }

    /// What the build made, in the words of its size: the functions for a handful, the units
    /// otherwise; and the cause, when one cause is all of it.
    fn made(&self, built: &Built, whys: &[Why]) -> (String, Option<String>) {
        let real: Vec<&Why> = whys.iter().filter(|it| !it.wasted).collect(); // alloc: once a build
        let functions: Vec<&str> =
            real.iter().flat_map(|it| it.functions.iter().map(String::as_str)).collect(); // alloc: as above
        let cause = match real.as_slice() {
            [one] if !one.cause.is_empty() => Some(one.cause.clone()), // clone: as above
            _ => None,
        };
        let edit = built.compiled <= 2 && !functions.is_empty();
        let what = match (edit, functions.len()) {
            (true, 1..=NAMED) => format!("{} recompiled", functions.join(", ")), // alloc: as above
            (true, n) => format!(
                "{n} functions recompiled ({}, {} {} more)",
                functions[..2].join(", "),
                self.glyph("…", "and"),
                n - 2
            ), // alloc: as above
            (false, _) => {
                let mut what = format!("built {}", text::count(built.compiled, "unit", "units")); // alloc: as above
                if built.cached > 0 {
                    what.push_str(&format!(", {} cached", built.cached)); // alloc: as above
                }
                what
            }
        };
        (what, cause)
    }

    /// `✗ build failed · 1 error in hello (bin) · 2 units not built · 0.31s · blaze explain hello`.
    fn failed_line(&self, built: &Built, took: u64) -> Line {
        let dot = self.dot();
        let mut line = Line::new()
            .with(Role::Failed, format!("{} ", self.glyph("✗", "x"))) // alloc: the build's line, once
            .with(Role::Strong, format!("{} failed", self.opts.command.word())); // alloc: as above
        if self.errors > 0 {
            let mut said = format!(" {dot} {}", text::count(u64::from(self.errors), "error", "errors")); // alloc: as above
            match self.failed.as_slice() {
                [] => {}
                [one] => said.push_str(&format!(" in {one}")), // alloc: as above
                [one, rest @ ..] => said.push_str(&format!(
                    " in {one} and {}",
                    text::count(rest.len() as u64, "more unit", "more units")
                )), // alloc: as above
            }
            line.push(Role::Plain, said);
        }
        if built.unbuilt > 0 {
            line.push(
                Role::Plain,
                format!(" {dot} {} not built", text::count(u64::from(built.unbuilt), "unit", "units")),
            ); // alloc: as above
        }
        if self.warnings > 0 {
            line.push(
                Role::Warned,
                format!(" {dot} {}", text::count(u64::from(self.warnings), "warning", "warnings")),
            ); // alloc: as above
        }
        line.push(Role::Dim, format!(" {dot} {}", text::time(took))); // alloc: as above
        if let Some(next) = &built.next {
            line.push(Role::Dim, format!(" {dot} ")); // alloc: as above
            line.push(Role::Plain, next.clone()); // clone: as above
        }
        line
    }

    /// `blaze: failed errors=1 warnings=0 unbuilt=2 elapsed_ms=310 unit="hello (bin)" next="blaze explain hello"`.
    fn failed_plain(&self, built: &Built, took: u64) -> String {
        let mut line = format!(
            "blaze: failed errors={} warnings={} unbuilt={} elapsed_ms={took}",
            self.errors, self.warnings, built.unbuilt
        ); // alloc: the build's line, once
        if let Some(unit) = self.failed.first() {
            line.push_str(&format!(" unit={}", text::value(unit))); // alloc: as above
        }
        if let Some(next) = &built.next {
            line.push_str(&format!(" next={}", text::value(next))); // alloc: as above
        }
        line
    }

    /// `blaze: ok built=1 cached=31 warnings=0 elapsed_ms=700 tier="tier 0" cause="src/main.rs changed" recompiled="hello::add"`.
    fn built_plain(&self, built: &Built, whys: &[Why], took: u64) -> String {
        let mut line = format!(
            "blaze: ok built={} cached={} warnings={} elapsed_ms={took}",
            built.compiled, built.cached, self.warnings
        ); // alloc: the build's line, once
        if let Some(tier) = &built.tier {
            line.push_str(&format!(" tier={}", text::value(tier))); // alloc: as above
        }
        let real: Vec<&Why> = whys.iter().filter(|it| !it.wasted).collect(); // alloc: as above
        if let [one] = real.as_slice()
            && !one.cause.is_empty()
        {
            line.push_str(&format!(" cause={}", text::value(&one.cause))); // alloc: as above
        }
        let functions: Vec<&str> =
            real.iter().flat_map(|it| it.functions.iter().map(String::as_str)).collect(); // alloc: as above
        if !functions.is_empty() {
            line.push_str(&format!(" recompiled={}", text::value(&functions.join(",")))); // alloc: as above
        }
        for link in &built.links {
            if link.how != "unchanged" {
                line.push_str(&format!(" linked={}", text::value(&link.path))); // alloc: as above
                line.push_str(&format!(" link={}", text::value(&link.how))); // alloc: as above
            }
        }
        line
    }

    fn binary(&mut self, id: u32) -> Option<&mut Binary> {
        self.tests.binaries.iter_mut().find(|it| it.id == id)
    }

    fn test(&mut self, test: Test) {
        let now = self.now_ms;
        self.tests.began_ms.get_or_insert(now);
        match test {
            Test::Queued { binary, label, rerun } => self.tests.binaries.push(Binary {
                id: binary,
                label,
                rerun,
                total: None,
                done: 0,
                failed: 0,
                started: false,
                ended: false,
                running: Vec::new(), // alloc: none until a test starts
            }), // alloc: once a test executable
            Test::Started { binary } => {
                if let Some(it) = self.binary(binary) {
                    it.started = true;
                }
            }
            Test::Total { binary, tests } => {
                if let Some(it) = self.binary(binary) {
                    it.total = Some(tests);
                }
            }
            Test::TestStarted { binary, name } => {
                if let Some(it) = self.binary(binary) {
                    it.running.push((name, now)); // alloc: a test under way
                }
            }
            Test::TestEnded { binary, name, verdict, took_ms, output } => {
                let mut label = String::new(); // alloc: none unless it failed
                let mut rerun = None;
                if let Some(it) = self.binary(binary) {
                    it.running.retain(|running| running.0 != name);
                    it.done += 1;
                    if verdict == Verdict::Failed {
                        it.failed += 1;
                        label.clone_from(&it.label); // clone: a failure's, once
                        rerun = Some(it.rerun.clone()); // clone: as above
                    }
                }
                match verdict {
                    Verdict::Passed => self.tests.passed += 1,
                    Verdict::Ignored => self.tests.ignored += 1,
                    Verdict::Failed => {
                        self.tests.failed += 1;
                        if let Some(rerun) = rerun
                            && !self.tests.reruns.contains(&rerun)
                        {
                            self.tests.reruns.push(rerun); // alloc: once a failing executable
                        }
                        self.failure(&name, &label, took_ms, &output);
                    }
                }
            }
            Test::Ended { binary, end, output } => {
                let mut said = None;
                if let Some(it) = self.binary(binary) {
                    it.ended = true;
                    it.running.clear();
                    let label = it.label.clone(); // clone: read once, as it ends
                    let quiet = it.failed > 0;
                    match end {
                        BinaryEnd::Passed | BinaryEnd::Stopped => {}
                        // Its tests' failures said why already.
                        BinaryEnd::Failed { .. } if quiet => {}
                        BinaryEnd::Failed { status } => said = Some((label, status, true)),
                        BinaryEnd::NeverRan { why } => said = Some((label, why, false)),
                    }
                }
                if let Some((label, status, ran)) = said {
                    self.tests.failed += 1;
                    let what = match ran {
                        true => format!("{label} ended with {status}"), // alloc: a failure's, once
                        false => format!("{label} never ran: {status}"), // alloc: as above
                    };
                    self.failure(&what, "", None, &output);
                }
            }
            Test::Unbuilt { unit, behind, why } => {
                self.tests.unbuilt += 1;
                let said = match unit == behind {
                    true => format!("{unit} never built: {why}"), // alloc: a failure's, once
                    false => format!("{unit} never built, behind {behind}: {why}"), // alloc: as above
                };
                let line = Line::new()
                    .with(Role::Failed, format!("{} ", self.glyph("✗", "x"))) // alloc: as above
                    .with(Role::Plain, said);
                self.above.push(line); // alloc: as above
            }
            Test::Over => {
                self.tests.over = true;
                self.cleared = true;
                let lines = self.test_summary();
                self.above.extend(lines);
            }
        }
    }

    /// A failure as it happens: a line naming it, then the last lines it printed.
    fn failure(&mut self, name: &str, label: &str, took_ms: Option<u64>, output: &str) {
        let mut line = Line::new()
            .with(Role::Failed, format!("{} ", self.glyph("✗", "x"))) // alloc: a failure's, once
            .with(Role::Strong, name.to_string()); // alloc: as above
        if !label.is_empty() {
            line.push(Role::Dim, format!("  {label}")); // alloc: as above
        }
        if let Some(took) = took_ms {
            line.push(Role::Dim, format!(" {} {}", self.dot(), text::time(took))); // alloc: as above
        }
        self.above.push(line); // alloc: as above
        let lines: Vec<&str> = output.trim_end().lines().collect(); // alloc: as above
        let skipped = lines.len().saturating_sub(OUTPUT_LINES);
        if skipped > 0 {
            self.above.push(Line::new().with(
                Role::Dim,
                format!("  {} {skipped} earlier lines", self.glyph("…", "...")),
            )); // alloc: as above
        }
        for text in &lines[skipped..] {
            let links = marks(text, self.opts.root.as_deref());
            self.above.push(Line::Raw { text: format!("  {text}"), links: shifted(links, 2) }); // alloc: as above
        }
    }

    /// The tests' rows: an executable under way each, then the status row.
    fn test_rows(&self) -> Vec<Line> {
        let tests = &self.tests;
        let cols = self.most_cols();
        let running: Vec<&Binary> =
            tests.binaries.iter().filter(|it| it.started && !it.ended).collect(); // alloc: a frame's rows
        let name_col =
            running.iter().map(|it| text::width(&it.label)).max().unwrap_or(0).min((cols / 3).max(12));
        let room = self.most_rows().saturating_sub(1);
        let shown = match running.len() > room {
            true => room.saturating_sub(1),
            false => running.len(),
        };
        let mut rows = Vec::with_capacity(shown + 2); // alloc: as above
        for binary in &running[..shown] {
            let mut row = Line::new();
            row.push(Role::Running, format!(" {} ", self.glyph("▶", ">"))); // alloc: a row's span
            let name = text::fit(&binary.label, name_col);
            let pad = name_col - text::width(&name);
            row.push(Role::Strong, name);
            let count = match binary.total {
                Some(total) => format!("{}/{total}", binary.done), // alloc: as above
                None => format!("{}", binary.done),                // alloc: as above
            };
            row.push(Role::Plain, format!("{:pad$}  {count:>9}", "", pad = pad)); // alloc: as above
            if binary.failed > 0 {
                row.push(Role::Failed, format!(" {} {} failed", self.dot(), binary.failed)); // alloc: as above
            }
            // The tests that have run long enough to be the wait, longest first.
            let mut left = cols.saturating_sub(row.width());
            for (name, since) in &binary.running {
                let for_ms = self.now_ms.saturating_sub(*since);
                if for_ms < SLOW_TEST_MS || left < 20 {
                    continue;
                }
                let said = format!("  {} {}", text::item(name, left.min(48) - 10), text::timer(for_ms)); // alloc: as above
                left = left.saturating_sub(text::width(&said));
                row.push(Role::Dim, said);
            }
            rows.push(row); // alloc: as above
        }
        if running.len() > shown {
            rows.push(Line::new().with(
                Role::Dim,
                format!("   {} {} more running", self.glyph("…", "..."), running.len() - shown),
            )); // alloc: as above
        }
        // ` testing  ━━━━────  12/40 · 12 passed · 1 failed · 4.2s`
        let total: u32 = tests.binaries.iter().filter_map(|it| it.total).sum();
        let done: u32 = tests.binaries.iter().map(|it| it.done).sum();
        let waiting = tests.binaries.iter().filter(|it| !it.started).count();
        let mut row = Line::new().with(Role::Strong, " testing");
        if total > 0 && cols >= 50 {
            let bar = (cols / 4).clamp(8, 24);
            let filled = (u64::from(done.min(total)) * bar as u64 / u64::from(total)) as usize;
            row.push(Role::Done, format!("  {}", self.glyph("━", "#").repeat(filled))); // alloc: as above
            row.push(Role::Dim, self.glyph("─", "-").repeat(bar - filled)); // alloc: as above
        }
        let dot = self.dot();
        row.push(Role::Plain, format!("  {done}/{total}")); // alloc: as above
        row.push(Role::Done, format!(" {dot} {} passed", tests.passed)); // alloc: as above
        if tests.failed > 0 {
            row.push(Role::Failed, format!(" {dot} {} failed", tests.failed)); // alloc: as above
        }
        if tests.ignored > 0 {
            row.push(Role::Dim, format!(" {dot} {} ignored", tests.ignored)); // alloc: as above
        }
        if waiting > 0 {
            let building = match self.built.is_some() {
                true => "",
                false => ", build under way",
            };
            row.push(
                Role::Waiting,
                format!(" {dot} {} waiting{building}", text::count(waiting as u64, "binary", "binaries")),
            ); // alloc: as above
        }
        row.push(Role::Dim, format!(" {dot} {}", text::timer(self.now_ms))); // alloc: as above
        rows.push(row); // alloc: as above
        rows
    }

    /// `✓ 40 passed · 1 ignored · 3 binaries · 2.1s`, or what failed and how to rerun it.
    fn test_summary(&self) -> Vec<Line> {
        let tests = &self.tests;
        let dot = self.dot();
        let failed = tests.failed > 0 || tests.unbuilt > 0;
        let mut line = Line::new();
        match failed {
            true => {
                line.push(Role::Failed, format!("{} ", self.glyph("✗", "x"))); // alloc: the summary, once
                line.push(Role::Strong, format!("{} failed", tests.failed)); // alloc: as above
                line.push(Role::Plain, format!(" {dot} {} passed", tests.passed)); // alloc: as above
            }
            false => {
                line.push(Role::Done, format!("{} ", self.glyph("✓", "ok"))); // alloc: as above
                line.push(Role::Plain, format!("{} passed", tests.passed)); // alloc: as above
            }
        }
        if tests.ignored > 0 {
            line.push(Role::Dim, format!(" {dot} {} ignored", tests.ignored)); // alloc: as above
        }
        if tests.unbuilt > 0 {
            line.push(Role::Failed, format!(" {dot} {} never built", tests.unbuilt)); // alloc: as above
        }
        line.push(
            Role::Dim,
            format!(
                " {dot} {} {dot} {}",
                text::count(tests.binaries.len() as u64, "binary", "binaries"),
                text::time(self.now_ms)
            ),
        ); // alloc: as above
        let mut lines = alloc::vec![line]; // alloc: as above
        for rerun in &tests.reruns {
            lines.push(Line::new().with(Role::Dim, "  rerun: ").with(Role::Plain, rerun.clone())); // alloc: once a failing executable
        }
        lines
    }
}

/// The word a row names a task's kind by: a build's stages as a person says them.
fn stage(kind: &str) -> &'static str {
    match kind {
        "manifest" => "manifest",
        "fetch" => "fetch",
        "script" => "build script",
        "script-run" => "run build script",
        "macro" => "macro",
        "std" => "std",
        "names" => "names",
        "bodies" => "check",
        "codegen" => "codegen",
        "store" => "store",
        "link" => "link",
        "test" => "test",
        _ => "work",
    }
}

fn percent(done: u32, total: u32) -> u8 {
    match total {
        0 => 0,
        _ => (u64::from(done.min(total)) * 100 / u64::from(total)) as u8,
    }
}

/// `row` in at most `cols` columns: its spans cut from the end.
fn cut(row: &mut Line, cols: usize) {
    let Line::Spans(spans) = row else { return };
    let mut used = 0;
    let mut keep = spans.len();
    for (at, span) in spans.iter_mut().enumerate() {
        let width = text::width(&span.text);
        if used + width > cols {
            span.text = text::fit(&span.text, cols - used);
            keep = at + 1;
            break;
        }
        used += width;
    }
    spans.truncate(keep);
}

/// Off a terminal, a build under way: `blaze: progress done=32/147 running=3 elapsed_ms=10000
/// longest="codegen core 0.0.0 lib (host)" for_ms=4200`. Also what `blaze status` says of each
/// build.
pub fn progress_line(now: &Now, elapsed_ms: u64) -> String {
    let tally = now.tally;
    let mut line = format!(
        "blaze: progress phase={} done={}/{} running={} elapsed_ms={elapsed_ms}",
        now.phase.word(),
        tally.done,
        tally.total,
        tally.running
    ); // alloc: a line every ten seconds
    if tally.failed > 0 {
        line.push_str(&format!(" failed={}", tally.failed)); // alloc: as above
    }
    if let Some(longest) = now.running.first() {
        let what = format!("{} {}", longest.task.kind, longest.task.name); // alloc: as above
        line.push_str(&format!(" longest={} for_ms={}", text::value(&what), longest.for_ms)); // alloc: as above
        if let Some(item) = longest.span.as_ref().and_then(|it| it.item.as_deref()) {
            line.push_str(&format!(" in={}", text::value(&text::item(item, 80)))); // alloc: as above
        }
    } else if let Some(download) = now.downloads.first() {
        line.push_str(&format!(" fetching={} bytes={}", text::value(&download.what), download.done)); // alloc: as above
    } else if !now.stage.is_empty() {
        line.push_str(&format!(" waits={}", text::value(&now.stage))); // alloc: as above
    }
    if now.memory.over.is_some() || now.memory.held_back > 0 {
        line.push_str(&format!(
            " memory_gb={}/{} held_back={}",
            text::gb(now.memory.held),
            text::gb(now.memory.limit),
            now.memory.held_back
        )); // alloc: as above
    }
    if let Some(stall) = &now.stall {
        line.push_str(&format!(" stall={}", text::value(stall))); // alloc: as above
    }
    line
}

/// A `file://` URL of `path`, absolute.
pub fn file_url(path: &str) -> String {
    let mut url = String::with_capacity(path.len() + 8); // alloc: a link, once
    url.push_str("file://");
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                url.push(byte as char);
            }
            _ => url.push_str(&format!("%{byte:02X}")), // alloc: as above
        }
    }
    url
}

/// What in a drawn diagnostic's `line` leads somewhere: the file of ` --> src/main.rs:1:37`
/// (a `file://` URL of it under `root`), and the code of `error[E0425]` (its page in the
/// error index). The text is not changed: each mark is a byte range of it and a URL.
fn marks(line: &str, root: Option<&str>) -> Vec<Mark> {
    let mut found = Vec::new(); // alloc: none unless the line has a link
    if let Some(at) = line.find("--> ") {
        let from = at + 4;
        let rest = &line[from..];
        let end = rest.find([' ', '\x1b', '\t']).unwrap_or(rest.len());
        let place = &rest[..end];
        // `path:line:col`: the path is what is before the last two colons.
        let mut parts = place.rsplitn(3, ':');
        let (col, row, path) = (parts.next(), parts.next(), parts.next());
        let numbers = |it: Option<&str>| it.is_some_and(|it| !it.is_empty() && it.bytes().all(|b| b.is_ascii_digit()));
        if let Some(path) = path
            && numbers(col)
            && numbers(row)
            && !path.is_empty()
        {
            let whole = match (path.starts_with('/'), root) {
                (true, _) => Some(path.to_string()),                          // alloc: a link
                (false, Some(root)) => Some(format!("{}/{path}", root.trim_end_matches('/'))), // alloc: as above
                (false, None) => None,
            };
            if let Some(whole) = whole {
                found.push(Mark { from, to: from + end, url: file_url(&whole) }); // alloc: as above
            }
        }
    }
    if let Some(at) = line.find("[E")
        && let Some(code) = line.get(at + 1..at + 6)
        && code[1..].bytes().all(|b| b.is_ascii_digit())
        && line[at + 6..].starts_with(']')
    {
        found.push(Mark {
            from: at + 1,
            to: at + 6,
            url: format!("https://doc.rust-lang.org/error_codes/{code}.html"), // alloc: a link
        }); // alloc: as above
    }
    found.sort_by_key(|it| it.from);
    found
}

fn shifted(mut marks: Vec<Mark>, by: usize) -> Vec<Mark> {
    for mark in &mut marks {
        mark.from += by;
        mark.to += by;
    }
    marks
}
