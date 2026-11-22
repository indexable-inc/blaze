//! What a command does while it works, as events ([`Step`]) to one sink ([`Watch`]), and the one
//! live status line drawn from them (docs/terminal.md, "While `run` works"). The command
//! emits events and never draws: the sink is handed in from the command's edge (`blaze run`'s
//! main.rs), so the run's parent, its loop and the front end say what happens and a driver
//! decides what a terminal shows. Off a terminal the sink is [`Unwatched`], which keeps
//! nothing.
//!
//! The events are shaped for any command that works before it answers (`run` today; `check`
//! and `engine-check` later): a stage begun, the item being worked on, how a first call was
//! answered, and the three moments a program's own output may start (it was started, a need
//! is about to be answered, the command is about to write a line itself).
//!
//! [`RunStatus`] is the line's sans-IO core: it takes the events and the injected clock's
//! milliseconds and answers with the frame to write now ([`StatusFrame`]: the line drawn, or
//! erased) and the time it may next have one ([`RunStatus::next_ms`]). It is not [`Screen`]:
//! `Screen` draws a region of rows below the scrollback, each ending in a line feed, and erases
//! it by moving the cursor up over them, which assumes the cursor sits where the region ended.
//! Once a program runs, the cursor is the program's: it may have left it in the middle of a
//! line, and the line feed would move its text. The run's line is drawn at the cursor with the
//! cursor saved and restored and autowrap off ([`crate::encode_status`]), and erased in place,
//! so nothing the program wrote moves and no partial line is left.
//!
//! [`Screen`]: crate::Screen

use core::fmt;

use crate::screen::Caps;

/// Nothing is drawn before the work has gone on this long: a command that is quick shows
/// nothing; and once the program runs, a need answered quicker than this shows nothing.
pub const RUN_QUIET_MS: u64 = 100;
/// The line is redrawn at most this often.
pub const RUN_EVERY_MS: u64 = 50;
/// The bytes of an item's path the line keeps: its end, the part that names the item.
pub const PATH_BYTES: usize = 192;
/// A terminal narrower than this gets no line: the glyph and a word would not fit.
const MIN_COLS: u16 = 10;

/// A stretch of a command's work before its items: what the status line names while no item
/// is being worked on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    /// The workspace checked (the gate: a body that is not clean is never built).
    Checking,
    /// The crates of the program found (`run_names::found_answered`).
    Crates,
    /// The body ids read (the check's proofs, by item).
    BodyIds,
    /// The names records opened (the front end's tables of the program's crates).
    Names,
    /// The program's root instances found (`main`, the runtime's start, the specials).
    Roots,
}

impl Stage {
    /// The stage as the status line says it.
    pub const fn word(self) -> &'static str {
        match self {
            Stage::Checking => "checking",
            Stage::Crates => "finding crates",
            Stage::BodyIds => "reading body ids",
            Stage::Names => "opening names",
            Stage::Roots => "finding roots",
        }
    }
}

/// How a function's first call was answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answered {
    /// Its code record was in the store under its key.
    Found,
    /// It was built now and put under its key.
    Made,
    /// It was built now and has no key (a body read no record keys).
    Unkeyed,
}

/// One thing a command did, as it happens. `path` borrows the caller's words for the event's
/// moment only: a sink that keeps it copies it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step<'a> {
    /// A stage begun.
    Began(Stage),
    /// `rows` functions placed ahead from today's start list: answered before they are asked.
    Placed { rows: u32 },
    /// A function's first call being answered: its item path when the sink wants paths
    /// ([`Watch::paths`]) and the front end names it, else `None`.
    Resolving { path: Option<&'a str> },
    /// That first call answered.
    Answered(Answered),
    /// The program is about to start: from now on it may write to the terminal.
    Started,
    /// The program asked (it is blocked on a need) and the command is answering it.
    Waits,
    /// The answer is about to be sent: the program may write again once it reads it.
    Answers,
    /// A check/build stage making live progress (`done` of `total` tasks), with an optional
    /// active item label (`detail`, e.g. crate or file path).
    Progress { stage: &'static str, done: u32, total: u32, detail: Option<&'a str> },
    /// The command is about to write a line of its own: nothing live until the next event.
    Says,
}

/// The one sink a command's events go to. It is told each event as it happens, on the thread
/// that does the work; what it does with one must be quick, and a sink that draws must have
/// erased what it drew by the time it returns from [`Step::Started`], [`Step::Answers`] and
/// [`Step::Says`] (the terminal is the program's, or the command's own line's, after them).
pub trait Watch {
    /// One event.
    fn see(&self, step: Step<'_>);
    /// Whether [`Step::Resolving`] should carry the item's path (naming it costs a lookup in
    /// the names records: a sink that shows nothing asks for none).
    fn paths(&self) -> bool;
}

/// The sink that keeps nothing: a command off a terminal, and every caller with no sink.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Unwatched;

impl Watch for Unwatched {
    fn see(&self, _step: Step<'_>) {}

    fn paths(&self) -> bool {
        false
    }
}

/// What the live line says at one moment, as data: [`crate::encode_status`] lays it out in
/// `cols - 1` columns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StatusLine<'a> {
    /// What is being done: a stage's word, `compiling` while a function is being answered.
    pub word: &'static str,
    /// The function's path, when there is one to show; `cut` when its start was dropped
    /// ([`PATH_BYTES`]).
    pub path: Option<&'a str>,
    pub cut: bool,
    /// Functions found in the store and made (built now, keyed or not), so far.
    pub found: u32,
    pub made: u32,
    /// Stage progress `(done, total)` when reporting check/build stage progress.
    pub progress: Option<(u32, u32)>,
    /// The time since the command began, by the injected clock.
    pub elapsed_ms: u64,
    pub cols: u16,
    /// Glyphs outside ASCII are drawn.
    pub unicode: bool,
}

/// What to write now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatusFrame<'a> {
    /// The line drawn at the cursor (over what was drawn before).
    Draw(StatusLine<'a>),
    /// What was drawn erased: the terminal is as it was before the first draw.
    Erase,
}

/// How [`RunStatus`] is made: what the terminal can do ([`Caps`]: off a terminal, `live` is
/// false and nothing is ever drawn).
#[derive(Clone, Copy, Debug)]
pub struct RunStatusOpts {
    pub caps: Caps,
}

/// The live line's core (the module's doc). Times are the injected clock's milliseconds since
/// the command began; a time earlier than one already seen is taken as that one.
#[derive(Clone, Debug)]
pub struct RunStatus {
    caps: Caps,
    now: u64,
    stage: Option<Stage>,
    /// Live stage progress verb and `(done, total)` counts when inside a checking/building phase.
    progress: Option<(&'static str, u32, u32)>,
    /// A function is being answered (since the last stage began).
    item: bool,
    path: [u8; PATH_BYTES],
    path_len: usize,
    path_cut: bool,
    found: u32,
    made: u32,
    /// The program has been started: from now on the line shows only while it waits.
    started: bool,
    /// Since when the program has been blocked on a need being answered.
    waiting_since: Option<u64>,
    /// The command is writing a line of its own: nothing until the next event.
    held: bool,
    /// The command is over: nothing more is drawn.
    over: bool,
    /// The line is on the terminal.
    drawn: bool,
    last_draw: Option<u64>,
    /// Something to say has changed since the last draw.
    changed: bool,
    /// The tenth of a second the last draw showed.
    tenth: u64,
}

impl RunStatus {
    pub fn new(opts: RunStatusOpts) -> RunStatus {
        RunStatus {
            caps: opts.caps,
            now: 0,
            stage: None,
            progress: None,
            item: false,
            path: [0; PATH_BYTES],
            path_len: 0,
            path_cut: false,
            found: 0,
            made: 0,
            started: false,
            waiting_since: None,
            held: false,
            over: false,
            drawn: false,
            last_draw: None,
            changed: false,
            tenth: 0,
        }
    }

    /// Time has passed: it is `now_ms`.
    pub fn at(&mut self, now_ms: u64) {
        self.now = self.now.max(now_ms);
    }

    /// The terminal is now `cols` wide.
    pub fn resize(&mut self, cols: u16) {
        if cols != self.caps.cols {
            self.caps.cols = cols;
            self.changed = true;
        }
    }

    /// One event, at `now_ms`.
    pub fn see(&mut self, now_ms: u64, step: Step<'_>) {
        self.at(now_ms);
        self.held = false;
        self.changed = true;
        match step {
            Step::Began(stage) => {
                self.stage = Some(stage);
                self.progress = None;
                self.item = false;
                self.path_len = 0;
            }
            Step::Placed { rows } => self.found = self.found.saturating_add(rows),
            Step::Resolving { path } => {
                self.progress = None;
                self.item = true;
                self.keep_path(path.unwrap_or(""));
            }
            Step::Answered(Answered::Found) => self.found = self.found.saturating_add(1),
            Step::Answered(Answered::Made | Answered::Unkeyed) => self.made = self.made.saturating_add(1),
            Step::Started => {
                self.progress = None;
                self.started = true;
                self.waiting_since = None;
            }
            Step::Waits => self.waiting_since = Some(self.now),
            Step::Answers => self.waiting_since = None,
            Step::Progress { stage, done, total, detail } => {
                self.progress = Some((stage, done, total));
                self.item = false;
                self.keep_path(detail.unwrap_or(""));
            }
            Step::Says => self.held = true,
        }
    }

    /// The end of `path` that fits [`PATH_BYTES`], cut at a character's start.
    fn keep_path(&mut self, path: &str) {
        let mut from = path.len().saturating_sub(PATH_BYTES);
        while !path.is_char_boundary(from) {
            from += 1;
        }
        let kept = &path.as_bytes()[from..];
        self.path[..kept.len()].copy_from_slice(kept);
        self.path_len = kept.len();
        self.path_cut = from > 0;
    }

    /// Whether the line may be on the terminal now.
    fn visible(&self) -> bool {
        let quiet_over = match (self.started, self.waiting_since) {
            (false, _) => self.now >= RUN_QUIET_MS,
            (true, Some(since)) => self.now.saturating_sub(since) >= RUN_QUIET_MS,
            (true, None) => false,
        };
        self.caps.live && self.caps.cols >= MIN_COLS && !self.held && !self.over && quiet_over
    }

    /// What to write now, if anything: the line erased the moment it may no longer show (the
    /// program started, a need's answer about to be sent, the command about to write), drawn
    /// when it may show and was not, or redrawn when what it says changed, at most once a
    /// [`RUN_EVERY_MS`].
    pub fn frame(&mut self) -> Option<StatusFrame<'_>> {
        if !self.visible() {
            if !self.drawn {
                return None;
            }
            self.drawn = false;
            self.changed = true;
            return Some(StatusFrame::Erase);
        }
        let tenth = self.now / 100;
        let rested = self.last_draw.is_none_or(|at| self.now.saturating_sub(at) >= RUN_EVERY_MS);
        if !rested || (self.drawn && !self.changed && tenth == self.tenth) {
            return None;
        }
        self.drawn = true;
        self.last_draw = Some(self.now);
        self.changed = false;
        self.tenth = tenth;
        Some(StatusFrame::Draw(self.line()))
    }

    /// The line as it would be drawn now.
    pub fn line(&self) -> StatusLine<'_> {
        let has_path = self.item || self.progress.is_some();
        let path = core::str::from_utf8(&self.path[..self.path_len]).ok().filter(|path| has_path && !path.is_empty());
        let (word, progress) = match (self.item, self.progress, self.stage) {
            (true, _, _) => ("compiling", None),
            (false, Some((stage, done, total)), _) => (stage, Some((done, total))),
            (false, None, Some(stage)) => (stage.word(), None),
            (false, None, None) => ("starting", None),
        };
        StatusLine { word, path, cut: self.path_cut, found: self.found, made: self.made, progress, elapsed_ms: self.now, cols: self.caps.cols, unicode: self.caps.unicode }
    }

    /// When [`RunStatus::frame`] may next have something with no event in between: `None`
    /// when only an event can change what it answers (off a terminal, held, over, or a
    /// program running that asks for nothing).
    pub fn next_ms(&self) -> Option<u64> {
        if self.over || self.held || !self.caps.live || self.caps.cols < MIN_COLS {
            return None;
        }
        let shows_at = match (self.started, self.waiting_since) {
            (false, _) => RUN_QUIET_MS,
            (true, Some(since)) => since.saturating_add(RUN_QUIET_MS),
            (true, None) => return None,
        };
        let mut at = shows_at.max(self.now);
        if let Some(last) = self.last_draw {
            at = at.max(last.saturating_add(RUN_EVERY_MS));
        }
        if self.drawn && !self.changed {
            // The timer's next tenth.
            at = at.max((self.now / 100 + 1) * 100);
        }
        Some(at)
    }

    /// The command is over: the line erased if it is on the terminal, and nothing drawn again.
    pub fn finish(&mut self) -> Option<StatusFrame<'static>> {
        self.over = true;
        match core::mem::replace(&mut self.drawn, false) {
            true => Some(StatusFrame::Erase),
            false => None,
        }
    }

    /// Whether [`RunStatus::finish`] was called.
    pub fn is_over(&self) -> bool {
        self.over
    }
}

/// A few bytes of text written with `core::fmt`, on the stack: a number or a frame's bytes.
/// A write past its end fails ([`fmt::Error`]) and keeps what fit.
#[derive(Clone, Debug)]
pub struct LineBuf<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> LineBuf<N> {
    pub const fn new() -> LineBuf<N> {
        LineBuf { bytes: [0; N], len: 0 }
    }

    pub fn as_str(&self) -> &str {
        // Only whole `str`s are written in.
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }
}

impl<const N: usize> Default for LineBuf<N> {
    fn default() -> LineBuf<N> {
        LineBuf::new()
    }
}

impl<const N: usize> fmt::Write for LineBuf<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len + s.len();
        let Some(room) = self.bytes.get_mut(self.len..end) else { return Err(fmt::Error) };
        room.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! The line's promises (docs/terminal.md, "While `run` works"): nothing in the first
    //! [`RUN_QUIET_MS`]; at most one draw a [`RUN_EVERY_MS`]; the line fits the terminal's
    //! width less one column and never wraps; it is erased before the program starts, before
    //! each answer, before the command's own line, and at the end; off a terminal, nothing.

    use blaze_report::terminal::Terminal;

    use super::*;
    use crate::encode_status;

    fn caps(cols: u16) -> Caps {
        Caps { live: true, cols, rows: 24, unicode: true }
    }

    fn status(cols: u16) -> RunStatus {
        RunStatus::new(RunStatusOpts { caps: caps(cols) })
    }

    fn is_draw(frame: Option<StatusFrame<'_>>) -> bool {
        matches!(frame, Some(StatusFrame::Draw(_)))
    }

    /// Nothing to write now, and nothing to wake for before the next event.
    fn still(line: &mut RunStatus) -> bool {
        line.frame().is_none() && line.next_ms().is_none()
    }

    /// The text a draw puts on the terminal: its bytes between the autowrap's off and the
    /// erase to the line's end, colour taken out.
    fn shown(line: StatusLine<'_>, terminal: &Terminal) -> LineBuf<512> {
        let mut bytes = LineBuf::<1024>::new();
        encode_status(&StatusFrame::Draw(line), terminal, &mut bytes).expect("a line fits the buffer");
        let all = bytes.as_str();
        let from = all.find("\x1b[?7l").expect("autowrap off") + "\x1b[?7l".len();
        let to = all.rfind("\x1b[K").expect("the line's end erased");
        let mut text = LineBuf::<512>::new();
        let mut rest = &all[from..to];
        while let Some(at) = rest.find('\x1b') {
            fmt::Write::write_str(&mut text, &rest[..at]).expect("fits");
            let end = rest[at..].find('m').expect("only SGR inside the line");
            rest = &rest[at + end + 1..];
        }
        fmt::Write::write_str(&mut text, rest).expect("fits");
        text
    }

    /// Nothing before 100 ms, the first draw at 100 ms. A core that drew at the first event
    /// fails.
    #[test]
    fn nothing_is_drawn_in_the_first_100_ms() {
        let mut line = status(80);
        line.see(0, Step::Began(Stage::Checking));
        for now in 0..RUN_QUIET_MS {
            line.at(now);
            assert_eq!(line.frame(), None, "drawn at {now} ms");
            assert_eq!(line.next_ms(), Some(RUN_QUIET_MS), "at {now} ms");
        }
        line.at(RUN_QUIET_MS);
        assert!(is_draw(line.frame()), "nothing drawn at {RUN_QUIET_MS} ms");
    }

    /// An event a millisecond for two seconds, each followed by a look at the frame: the draws are at
    /// least [`RUN_EVERY_MS`] apart, and there are many (the line keeps up). Control: a core
    /// asked at the times `next_ms` names draws at each.
    #[test]
    fn at_most_one_draw_a_50_ms() {
        let mut line = status(80);
        let (mut draws, mut last) = (0u32, None::<u64>);
        for now in 0..2000u64 {
            let step = match now % 3 {
                0 => Step::Resolving { path: Some("core::fmt::write") },
                1 => Step::Answered(Answered::Made),
                _ => Step::Answered(Answered::Found),
            };
            line.see(now, step);
            if is_draw(line.frame()) {
                if let Some(last) = last {
                    assert!(now - last >= RUN_EVERY_MS, "a draw at {now} ms, {} ms after the last", now - last);
                }
                (draws, last) = (draws + 1, Some(now));
            }
        }
        assert!(draws >= 30, "the line fell behind: {draws} draws");
        let mut quiet = status(80);
        quiet.see(0, Step::Began(Stage::Names));
        let mut asked = 0;
        while let Some(next) = quiet.next_ms().filter(|next| *next < 2000) {
            quiet.at(next);
            assert!(is_draw(quiet.frame()), "nothing to draw at the {next} ms `next_ms` named");
            asked += 1;
        }
        assert!(asked >= 19, "the timer stopped: {asked} draws");
    }

    /// The three states' text: the words, path, counts and time, in that order; the
    /// path cut in its middle when it does not fit, and dropped when even a cut one would not.
    #[test]
    fn the_line_says_the_stage_the_function_and_the_counts() {
        let plain = Terminal::PIPE;
        let mut line = status(80);
        line.see(0, Step::Began(Stage::Checking));
        line.at(300);
        let checking = shown(line.line(), &plain);
        assert_eq!(checking.as_str(), "▶ checking · 0.3s");
        line.see(1200, Step::Began(Stage::Names));
        let names = shown(line.line(), &plain);
        assert_eq!(names.as_str(), "▶ opening names · 1.2s");
        line.see(1300, Step::Placed { rows: 11 });
        for _ in 0..37 {
            line.see(1300, Step::Answered(Answered::Made));
        }
        line.see(1300, Step::Answered(Answered::Found));
        line.see(2400, Step::Resolving { path: Some("std::io::stdio::_print") });
        let compiling = shown(line.line(), &plain);
        assert_eq!(compiling.as_str(), "▶ compiling std::io::stdio::_print · 37 made, 12 found · 2.4s");
        line.resize(60);
        assert_eq!(shown(line.line(), &plain).as_str(), "▶ compiling std::…stdio::_print · 37 made, 12 found · 2.4s");
        line.resize(40);
        assert_eq!(shown(line.line(), &plain).as_str(), "▶ compiling · 37 made, 12 found · 2.4s");
        let mut ascii = line.clone();
        ascii.caps.unicode = false;
        ascii.resize(80);
        assert_eq!(shown(ascii.line(), &plain).as_str(), "> compiling std::io::stdio::_print - 37 made, 12 found - 2.4s");
    }

    /// For each width from 1 to 200 columns, a long path
    /// and large counts, plain and in colour: the text is at most `cols - 1` columns, so the
    /// cursor never reaches the last column and nothing wraps.
    #[test]
    fn the_line_fits_the_width() {
        let colour = Terminal { caps: blaze_report::Caps { color: true, links: false, width: Some(80) }, ..Terminal::PIPE };
        let long = "blaze_ladder::front2::program::lazy::walk::Walk::one::<blaze_ladder::run::make_one::Maker>";
        for cols in 1..=200u16 {
            let mut line = status(cols);
            line.see(0, Step::Placed { rows: 4_000_000 });
            line.see(61_000, Step::Resolving { path: Some(long) });
            for terminal in [&Terminal::PIPE, &colour] {
                let text = shown(line.line(), terminal);
                let width = crate::text::width(text.as_str());
                assert!(width < usize::from(cols).max(1), "{cols} columns: {width} wide: {:?}", text.as_str());
            }
        }
    }

    /// Drawn, then `Started` erases it at once and nothing is drawn while the program
    /// runs on its own; a need waited on for 100 ms draws it, and `Answers` erases it; `Says`
    /// erases it until the next event; `finish` erases it and nothing is drawn after.
    #[test]
    fn the_line_is_erased_before_the_program_may_write() {
        let mut line = status(80);
        line.see(0, Step::Began(Stage::Roots));
        line.at(200);
        assert!(is_draw(line.frame()));
        line.see(250, Step::Started);
        assert_eq!(line.frame(), Some(StatusFrame::Erase), "the program started under the line");
        line.at(5000);
        assert!(still(&mut line), "drawn while the program runs on its own");
        line.see(6000, Step::Waits);
        line.see(6000, Step::Resolving { path: Some("a::b") });
        line.at(6099);
        assert_eq!(line.frame(), None, "drawn for a need answered in under {RUN_QUIET_MS} ms");
        assert_eq!(line.next_ms(), Some(6100));
        line.at(6100);
        assert!(is_draw(line.frame()), "a slow need shows nothing");
        line.see(6300, Step::Answers);
        assert_eq!(line.frame(), Some(StatusFrame::Erase), "the answer was sent under the line");
        line.see(7000, Step::Waits);
        line.at(7200);
        assert!(is_draw(line.frame()));
        line.see(7250, Step::Says);
        assert_eq!(line.frame(), Some(StatusFrame::Erase), "the command's line written under the live one");
        line.at(9000);
        assert!(still(&mut line), "drawn while held");
        line.see(9000, Step::Answered(Answered::Made));
        assert!(is_draw(line.frame()), "an event after the command's line does not draw again");
        assert_eq!(line.finish(), Some(StatusFrame::Erase));
        line.see(9500, Step::Waits);
        line.at(20_000);
        assert!(still(&mut line), "drawn after the end");
        let mut quiet = status(80);
        assert_eq!(quiet.finish(), None, "an end erased a line never drawn");
    }

    /// A core whose terminal is not live never
    /// draws and never asks to be woken.
    #[test]
    fn off_a_terminal_nothing_is_drawn() {
        let mut line = RunStatus::new(RunStatusOpts { caps: Caps { live: false, ..caps(80) } });
        for now in (0..10_000).step_by(7) {
            line.see(now, Step::Resolving { path: Some("x::y") });
            assert!(still(&mut line), "drawn off a terminal at {now} ms");
        }
        assert_eq!(line.finish(), None);
    }

    /// The bytes of a draw and an erase: one synchronized update, the cursor saved, autowrap
    /// off, the text, the rest of the line erased, autowrap on, the cursor restored; an erase
    /// is the line's rest erased from the cursor, which a draw left where it was.
    #[test]
    fn a_draw_saves_the_cursor_and_an_erase_is_in_place() {
        let mut line = status(80);
        line.see(300, Step::Began(Stage::Checking));
        let mut bytes = LineBuf::<256>::new();
        encode_status(&StatusFrame::Draw(line.line()), &Terminal::PIPE, &mut bytes).expect("fits");
        assert_eq!(bytes.as_str(), "\x1b[?2026h\x1b7\x1b[?7l▶ checking · 0.3s\x1b[K\x1b[?7h\x1b8\x1b[?2026l");
        bytes.clear();
        encode_status(&StatusFrame::Erase, &Terminal::PIPE, &mut bytes).expect("fits");
        assert_eq!(bytes.as_str(), "\x1b[K");
    }
}
