//! The chat pane (`hn.ChatPane`, scene/widgets/chat.flecs): a chat session (the daemon's chat mode: an agent over
//! structured events, not a terminal) drawn as a chat. The transcript comes from the model's [`model::chat::Transcript`]
//! (agent-neutral events); each of its items becomes rows (a user message, the agent's markdown split into paragraphs,
//! headings, list items, quotes and code blocks, thinking and tool calls folded to a line, a turn's foot) in a
//! `ui.LazyList`, so only the rows in view are entities. It scrolls on AppKit's physics and sticks to the bottom while
//! the agent writes unless the person scrolled up. The input is a `ui.TextArea` edited here (`Draft`, keys in
//! scene/20-keys.flecs ctx.Chat): Return sends, Shift+Return breaks the line, Escape interrupts.
use super::*;
use harness_native_ui_motion_api::{CopyFlash, Follow, Paced};
use harness_native_ui_timeline_api::GlyphsArrive;
use harness_native_scroll_physics::{Engine, Input, Mutation, Phase, Vec2};
use harness_native_ui_layout_api::{ScrollState, ScrollerGrip, scroll_quietly};
use harness_native_ui_text_api::{Highlight, TextEdit};
use crate::pane::Unit;
use harness_native_render::text::Layout;
use harness_native_ui_input_api::Tooltip;
use harness_native_ui_look_api::TextStyle;
use harness_native_ui_text_api::{MarkdownText, Text};
use model::chat::group::{self, Act, Grouped, Piece};
use model::chat::{Detail, Item, Transcript};
use serde_json::Value;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

mod context;
mod genui;
mod peek;
mod pick;
mod rename;
mod steps;

pub(super) use rename::Rename;

pub(super) use genui::Live as GenLive;
pub(super) use peek::Live as PeekLive;
pub(super) use genui::{GenFeel, GenOn, Prims as GenPrims};

/// The text being written in a chat's input: its words, the caret and the selection's other end (byte offsets), and an
/// input method's marked text at the caret.
#[derive(Clone, Debug, Default)]
pub struct Draft {
    pub text: String,
    pub caret: usize,
    pub anchor: usize,
    pub marked: Option<String>,
    /// The x (points) up and down keep to, from the first of a run of them.
    pub goal: Option<f32>,
}

/// One edit of a [`Draft`] (an `act.Edit`'s `EditOp`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Move {
    Left,
    Right,
    Up,
    Down,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    DocStart,
    DocEnd,
}

impl Move {
    /// The move an `EditOp` names, less its `select_` (`left`, `word_right`, `doc_end`, ...), if it names one.
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "left" => Self::Left,
            "right" => Self::Right,
            "up" => Self::Up,
            "down" => Self::Down,
            "word_left" => Self::WordLeft,
            "word_right" => Self::WordRight,
            "line_start" => Self::LineStart,
            "line_end" => Self::LineEnd,
            "doc_start" => Self::DocStart,
            "doc_end" => Self::DocEnd,
            _ => return None,
        })
    }

    /// The move an erasing `EditOp` erases over (`backspace`: back a character, `line_erase`: to the line's start), if
    /// it is one.
    fn erasing(op: &str) -> Option<Self> {
        Some(match op {
            "backspace" => Self::Left,
            "delete" => Self::Right,
            "word_erase" => Self::WordLeft,
            "word_delete" => Self::WordRight,
            "line_erase" => Self::LineStart,
            _ => return None,
        })
    }
}

impl Draft {
    fn sel(&self) -> (usize, usize) {
        (self.anchor.min(self.caret), self.anchor.max(self.caret))
    }

    pub fn selected(&self) -> &str {
        let (a, b) = self.sel();
        &self.text[a..b]
    }

    /// Put `s` where the selection is.
    pub fn insert(&mut self, s: &str) {
        let (a, b) = self.sel();
        self.text.replace_range(a..b, s);
        self.caret = a + s.len();
        self.anchor = self.caret;
        self.goal = None;
    }

    fn prev(&self, i: usize) -> usize {
        self.text[..i].char_indices().next_back().map_or(0, |(k, _)| k)
    }

    fn next(&self, i: usize) -> usize {
        self.text[i..].chars().next().map_or(i, |c| i + c.len_utf8())
    }

    fn line_start(&self, i: usize) -> usize {
        self.text[..i].rfind('\n').map_or(0, |k| k + 1)
    }

    fn line_end(&self, i: usize) -> usize {
        self.text[i..].find('\n').map_or(self.text.len(), |k| i + k)
    }

    fn word_left(&self, mut i: usize) -> usize {
        let word = |c: char| c.is_alphanumeric() || c == '_';
        while i > 0 && !self.text[..i].chars().next_back().is_some_and(word) {
            i = self.prev(i);
        }
        while i > 0 && self.text[..i].chars().next_back().is_some_and(word) {
            i = self.prev(i);
        }
        i
    }

    fn word_right(&self, mut i: usize) -> usize {
        let word = |c: char| c.is_alphanumeric() || c == '_';
        let n = self.text.len();
        while i < n && !self.text[i..].chars().next().is_some_and(word) {
            i = self.next(i);
        }
        while i < n && self.text[i..].chars().next().is_some_and(word) {
            i = self.next(i);
        }
        i
    }

    /// Where `m` takes the caret, for the moves along the source; None for the moves along the laid-out lines (up, down,
    /// a line's start and end), which [`App::chat_target`] finds on the input's layout.
    fn target(&self, m: Move) -> Option<usize> {
        let i = self.caret;
        Some(match m {
            Move::Left => self.prev(i),
            Move::Right => self.next(i),
            Move::WordLeft => self.word_left(i),
            Move::WordRight => self.word_right(i),
            Move::DocStart => 0,
            Move::DocEnd => self.text.len(),
            Move::Up | Move::Down | Move::LineStart | Move::LineEnd => return None,
        })
    }

    /// Move the caret by `m` to `to`; with `select` the selection's other end stays. A plain left or right over a
    /// selection collapses it to that side, as AppKit does. Up and down keep the column they started from (`goal`).
    fn go(&mut self, m: Move, to: usize, select: bool) {
        let (a, b) = self.sel();
        if !select && a != b && matches!(m, Move::Left | Move::Right) {
            self.caret = if m == Move::Left { a } else { b };
        } else {
            self.caret = to;
        }
        if !select {
            self.anchor = self.caret;
        }
        if !matches!(m, Move::Up | Move::Down) {
            self.goal = None;
        }
    }

    /// Erase the selection, or from the caret to `to`.
    fn erase(&mut self, to: usize) {
        if self.anchor == self.caret {
            self.anchor = to;
        }
        self.insert("");
    }

    /// What a press at byte `i` selects by `unit`: nothing (the caret there), its word, or its line.
    fn span(&self, i: usize, unit: Unit) -> (usize, usize) {
        match unit {
            Unit::Cell => (i, i),
            Unit::Word => word_around(&self.text, i),
            Unit::Line => (self.line_start(i), self.line_end(i)),
        }
    }

    /// A press at byte `i` by `unit`: what it selects ([`Self::span`]) is the selection, the caret at its end. The span,
    /// for a drag from it to keep whole ([`Self::drag_to`]).
    fn press(&mut self, i: usize, unit: Unit) -> (usize, usize) {
        let span = self.span(i, unit);
        (self.anchor, self.caret, self.goal) = (span.0, span.1, None);
        span
    }

    /// A press that selected `held` dragged to byte `i`: the selection runs from `held`'s far end to the far end of what
    /// `unit` takes at `i`, so the pressed word or line stays whole whichever way the drag goes.
    fn drag_to(&mut self, i: usize, unit: Unit, held: (usize, usize)) {
        let s = self.span(i, unit);
        (self.anchor, self.caret) = if s.0 < held.0 { (held.1, s.0) } else { (held.0, s.1) };
    }

    /// The words shown (the marked text in at the caret) and the TextEdit's units: caret, anchor, marked range (UTF-16).
    fn shown(&self) -> (String, [u32; 4]) {
        let u = |s: &str| s.encode_utf16().count() as u32;
        match &self.marked {
            Some(m) => {
                let mut t = self.text.clone();
                t.insert_str(self.caret, m);
                let lo = u(&self.text[..self.caret]);
                let hi = lo + u(m);
                (t, [hi, hi, lo, hi])
            }
            None => (self.text.clone(), [u(&self.text[..self.caret]), u(&self.text[..self.anchor]), 0, 0]),
        }
    }
}

/// A chat pane's own state, by session: its rows' heights, the folds the person set (by call, run, thinking or code key:
/// open or folded; everything starts folded), the input, its scroll.
pub struct ChatState {
    heights: view::Heights,
    fold: HashMap<String, bool>,
    pub draft: Draft,
    engine: Engine,
    clock: std::time::Instant,
    y: f32,
    rev: usize,
    /// Kept at the bottom as the agent writes (until the person scrolls up, again once they scroll back down).
    stick: bool,
    content: f32,
    shift: f32,
    /// When the last attach was tried, and whether the link's stream is this pane's (events, not a terminal's text).
    tried: Option<f32>,
    ours: bool,
    /// The transcript's selection (where it was started, where it reaches), and the press-and-drag making one.
    sel: Option<(Spot, Spot)>,
    drag: Option<Drag>,
    /// The last press was in the transcript (not the input) and nothing was typed since: Command-A selects the
    /// transcript, not the draft.
    reading: bool,
    /// The table rows opened below their rows as records, by the transcript row the table is (its key).
    rows_open: HashMap<String, usize>,
    /// The model and effort picker's menu open under the composer (chat/pick.rs), and which; its highlight.
    pick: Option<pick::Pick>,
    /// The model and effort a spawn asked it to start on ([`App::chat_started_on`]): seeded into its transcript at every
    /// attach, since an attach replays the log from nothing and a Claude chat's log names neither until its agent is up.
    started: Option<(Option<String>, Option<String>)>,
    pick_nav: pick::PickNav,
    /// The composer's scroll once its words outgrow it: wheels on AppKit's physics, the caret kept in view.
    input_scroll: view::field::FieldScroll,
    /// The agent's words as they show (hn.Theme.chat_stream): each text block's pace through the bursts it comes in, by
    /// block; how much of each block not yet all shown shows ([`settled_len`], then its pace); and when each row of a
    /// block that streamed in here was first laid out (its glyphs fade in from then on, [`GlyphsArrive`]).
    stream: HashMap<String, Stream>,
    cut: HashMap<String, usize>,
    row_born: HashMap<String, f32>,
    /// Kept at the bottom on a spring while it sticks; the pane clock's time it was last stepped.
    follow: Follow,
    stepped: f64,
    /// The chip under the pointer, its hover card, and the GitHub refs its words name (chat/peek.rs).
    peek: peek::Peek,
}

/// One text block of a chat's transcript as it shows: its pace, and whether it streamed in while the pane watched (its
/// glyphs fade in) or was there when it attached (history: shown whole, at rest).
struct Stream {
    paced: Paced,
    live: bool,
}

/// Seconds after an attach during which the blocks that come are the log replayed (history), not words being written.
const REPLAY_SECS: f32 = 0.5;

/// What a Cmd+C in a chat took, so its pulse plays there: the transcript's selection, the input's, or row `.1` of the
/// table in transcript row `.0` (its key).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Copied {
    Transcript,
    Input,
    TableRow(String, usize),
    /// A cell's field's words (its words node).
    Field(u64),
}

/// A place in a chat's transcript: row `row`, its `part`th words ([`texts`]), UTF-16 unit `at` of what they show (the
/// markers out of markdown). Places order as they read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Spot {
    row: usize,
    part: usize,
    at: usize,
}

/// A press-and-drag selecting in a chat pane: in its input (byte offsets of the draft) or its transcript, by `unit`,
/// the press's own span kept whole as it extends.
#[derive(Clone, Copy, Debug)]
enum Drag {
    Input { unit: Unit, span: (usize, usize) },
    List { unit: Unit, span: (Spot, Spot) },
}

impl Default for ChatState {
    fn default() -> Self {
        Self {
            heights: view::Heights::default(),
            fold: HashMap::new(),
            draft: Draft::default(),
            engine: Engine::new(sidebar::config(0.0, 0.0, 0.0, 0.0, 0, 0.5), Mutation::None),
            clock: std::time::Instant::now(),
            y: 0.0,
            rev: 0,
            stick: true,
            content: 0.0,
            shift: 0.0,
            tried: None,
            ours: false,
            sel: None,
            drag: None,
            reading: false,
            rows_open: HashMap::new(),
            pick: None,
            pick_nav: pick::PickNav::default(),
            started: None,
            input_scroll: view::field::FieldScroll::default(),
            stream: HashMap::new(),
            cut: HashMap::new(),
            row_born: HashMap::new(),
            follow: Follow::default(),
            stepped: 0.0,
            peek: peek::Peek::default(),
        }
    }
}

/// A row of the transcript.
#[derive(Clone, Debug)]
enum Row {
    User(String),
    Prose(String),
    Heading(u8, String),
    ListItem { depth: usize, mark: String, text: String },
    Quote(String),
    Code { lang: String, text: String },
    /// An agent's own tool call: its line (the tool's name, its main argument, how it is going: its `hn.chat_status`,
    /// running or no_result, while it has no result; a small mark when it failed) and, opened, its input and result.
    Tool { block: String, name: String, arg: String, state: Option<&'static str>, shown: Shown, failed: bool, input: String, result: String },
    /// The one quiet line a run of calls or a lone harness REPL cell folds to ([`steps::Line`]).
    Steps(steps::Line),
    /// One output of an opened line's steps ([`steps::Out`]).
    Out(steps::Out),
    /// A table, record or list (the kernel's shown value, opened from its one-line preview), its first `cap` rows.
    Grid { v: Arc<Value>, cap: usize },
    /// A markdown table in the agent's words.
    Table(Rc<view::table::Table>),
    /// What a cell draws (its Svelte markup's tree, [`genui`]), the cell (`<repl>.<task>`) its events name, and the call
    /// (`block`) whose code its hover-only code button shows (`code`: shown now).
    Ui { v: Arc<Value>, cell: String, block: String, code: bool },
    Thinking { block: String, open: bool, text: String, live: bool },
    Note(String),
    Error(String),
}

impl Row {
    fn prefab(&self) -> &'static str {
        match self {
            Row::User(_) => "hn.ChatUser",
            Row::Prose(_) => "hn.ChatProse",
            Row::Heading(1, _) => "hn.ChatH1",
            Row::Heading(2, _) => "hn.ChatH2",
            Row::Heading(..) => "hn.ChatH3",
            // A bullet small and faint, a number or a task's box at the words' size, muted.
            Row::ListItem { mark, .. } if mark.ends_with('.') => "hn.ChatItemNumber",
            Row::ListItem { mark, .. } if mark == "\u{2610}" || mark == "\u{2611}" => "hn.ChatItemTask",
            Row::ListItem { .. } => "hn.ChatItem",
            Row::Quote(_) => "hn.ChatQuote",
            Row::Code { .. } => "hn.ChatCode",
            Row::Tool { shown: Shown::Full, failed: true, .. } => "hn.ChatToolOpenFailed",
            Row::Tool { shown: Shown::Full, .. } => "hn.ChatToolOpen",
            Row::Tool { failed: true, .. } => "hn.ChatToolFailed",
            Row::Tool { .. } => "hn.ChatTool",
            Row::Steps(l) => l.prefab(),
            Row::Out(o) => o.prefab(),
            Row::Grid { .. } => "hn.StepGrid",
            Row::Table(_) => "hn.ChatTable",
            Row::Ui { .. } => "hn.ChatUi",
            Row::Thinking { open: true, .. } => "hn.ChatThinkingOpen",
            Row::Thinking { .. } => "hn.ChatThinking",
            Row::Note(_) => "hn.ChatNote",
            Row::Error(_) => "hn.ChatError",
        }
    }
}

/// Markdown split into the rows it is drawn as: paragraphs, headings, list items (with their continuation lines), quotes
/// (each kept a line apart, less its indent and the block's trailing white space, so inline markdown reads a soft break
/// as a space and two trailing spaces or a backslash as a hard one), fenced code (an unclosed fence, still streaming, runs to the end) and GitHub tables (lines of pipes
/// that are no table, as code). While it is `live` (still streaming) a table's row arrives whole: a last row not yet ended
/// is held back, and so is a head whose delimiter line has not come.
fn blocks(md: &str, live: bool, out: &mut Vec<Row>) {
    /// A block's lines as its inline markdown: a line apart, each less its indent, the whole less its trailing white space.
    fn lines_of<'a>(ls: impl Iterator<Item = &'a str>) -> String {
        ls.map(str::trim_start).collect::<Vec<_>>().join("\n").trim_end().to_owned()
    }
    let lines: Vec<&str> = md.lines().collect();
    let mut para: Vec<&str> = Vec::new();
    let flush = |para: &mut Vec<&str>, out: &mut Vec<Row>| {
        if !para.is_empty() {
            out.push(Row::Prose(lines_of(para.iter().copied())));
            para.clear();
        }
    };
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            flush(&mut para, out);
            let fence = &t[..3];
            let lang = t[3..].trim().split_whitespace().next().unwrap_or("").to_owned();
            let mut code = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with(fence) {
                code.push(lines[i]);
                i += 1;
            }
            out.push(Row::Code { lang, text: code.join("\n") });
            i += 1;
            continue;
        }
        if t.is_empty() {
            flush(&mut para, out);
            i += 1;
            continue;
        }
        let hashes = t.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            flush(&mut para, out);
            out.push(Row::Heading(hashes as u8, t[hashes..].trim().trim_end_matches('#').trim().to_owned()));
            i += 1;
            continue;
        }
        if t.contains('|') {
            if let Some((mut table, used)) = harness_native_render::markdown::table(&lines[i..]) {
                flush(&mut para, out);
                if live && i + used == lines.len() && !md.ends_with('\n') {
                    table.rows.pop();
                }
                out.push(Row::Table(Rc::new(md_table(&table))));
                i += used;
                continue;
            }
            let last = i + 1 == lines.len() || (i + 2 == lines.len() && !md.ends_with('\n'));
            if live && t.starts_with('|') && last {
                break;
            }
        }
        if t.starts_with('|') {
            flush(&mut para, out);
            let mut rows = Vec::new();
            while i < lines.len() && lines[i].trim_start().starts_with('|') {
                rows.push(lines[i].trim());
                i += 1;
            }
            out.push(Row::Code { lang: String::new(), text: rows.join("\n") });
            continue;
        }
        if let Some(q) = t.strip_prefix('>') {
            flush(&mut para, out);
            let mut text = vec![q];
            i += 1;
            while i < lines.len() && lines[i].trim_start().starts_with('>') {
                text.push(&lines[i].trim_start()[1..]);
                i += 1;
            }
            out.push(Row::Quote(lines_of(text.into_iter())));
            continue;
        }
        if t.chars().all(|c| c == '-' || c == '*' || c == '_' || c == ' ') && t.len() >= 3 {
            flush(&mut para, out);
            i += 1;
            continue;
        }
        if let Some((mark, rest)) = item(t) {
            flush(&mut para, out);
            let depth = (line.len() - t.len()) / 2;
            let mut text = vec![rest];
            i += 1;
            while i < lines.len() {
                let n = lines[i];
                let nt = n.trim_start();
                if nt.is_empty() || item(nt).is_some() || n.len() == nt.len() || nt.starts_with("```") {
                    break;
                }
                text.push(nt);
                i += 1;
            }
            out.push(Row::ListItem { depth, mark, text: lines_of(text.into_iter()) });
            continue;
        }
        para.push(line);
        i += 1;
    }
    flush(&mut para, out);
}

/// A markdown table as `ui.Table` shows it: its cells inline markdown (a plain number a number, at the end unless the
/// delimiter line says otherwise), its columns aligned as the delimiter line says.
fn md_table(t: &harness_native_render::markdown::Table) -> view::table::Table {
    use harness_native_render::markdown::Align as A;
    use view::table::{Align, Cell, Column, Kind};
    let number = |s: &str| {
        let s = s.trim_end_matches('%').trim_start_matches(['$', '+', '-']).replace(',', "");
        !s.is_empty() && s.parse::<f64>().is_ok()
    };
    let cols = t
        .head
        .iter()
        .zip(&t.align)
        .map(|(h, a)| Column {
            name: harness_native_render::markdown::inline(h).text,
            align: match a {
                A::None => Align::Auto,
                A::Left => Align::Start,
                A::Center => Align::Center,
                A::Right => Align::End,
            },
        })
        .collect();
    let rows = t
        .rows
        .iter()
        .map(|r| r.iter().map(|c| Cell::new(if number(c) { Kind::Number } else { Kind::Markdown }, c.as_str())).collect())
        .collect();
    view::table::Table { cols, rows, head: true }
}

/// A list item's mark (`•` for a bullet, `3.` for a number) and its words.
fn item(t: &str) -> Option<(String, &str)> {
    for b in ["- ", "* ", "+ "] {
        if let Some(r) = t.strip_prefix(b) {
            let r = r.strip_prefix("[ ] ").map(|r| ("\u{2610}", r)).or_else(|| r.strip_prefix("[x] ").map(|r| ("\u{2611}", r)));
            return Some(match r {
                Some((m, r)) => (m.to_owned(), r),
                None => ("\u{2022}".to_owned(), &t[2..]),
            });
        }
    }
    let n = t.chars().take_while(char::is_ascii_digit).count();
    if n > 0 && n < 4 {
        let rest = &t[n..];
        if let Some(r) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some((format!("{}.", &t[..n]), r));
        }
    }
    None
}

/// Pretty JSON of a tool's input, its main string fields shown as they read (a command or code with real newlines).
fn tool_input(v: &serde_json::Value) -> String {
    match v.as_object() {
        Some(o) if o.len() == 1 && o.values().all(serde_json::Value::is_string) => {
            o.values().next().and_then(|s| s.as_str()).unwrap_or("").to_owned()
        }
        _ => serde_json::to_string_pretty(v).unwrap_or_default(),
    }
}

/// The key in a chat's `open` set that shows card `block`'s code.
fn code_key(block: &str) -> String {
    format!("{block}#code")
}

/// How much of an agent's own tool call shows: its line alone (every call at first, a failure too), or all of it (a call
/// the person opened). Nothing opens by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    Folded,
    Full,
}

impl Shown {
    /// Call `block` given the folds set (`fold`).
    fn of(fold: &HashMap<String, bool>, block: &str) -> Self {
        if fold.get(block).copied().unwrap_or(false) { Shown::Full } else { Shown::Folded }
    }
}

/// A drawn view's code button's glyph (cod-code).
const CODE_ICON: &str = "\u{EAC4}";

fn secs(ms: u64) -> String {
    if ms >= 60_000 { format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000) } else { format!("{:.1}s", ms as f64 / 1000.0) }
}

/// A shown scalar or cell as words: null as `null`, strings as they are.
fn cell_text(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// The first `n` lines of `s`, and a line saying how many more there were.
fn head_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.trim_end().lines().collect();
    if lines.len() <= n {
        return lines.join("\n");
    }
    format!("{}\n\u{2026} {} more lines", lines[..n].join("\n"), lines.len() - n)
}

/// Whether a shown value (or a genui tree) has nothing to draw: null, a grid with no rows, blank text or error, an
/// empty list of views.
fn empty_value(v: &Value) -> bool {
    let blank = |k: &str| v[k].as_str().is_none_or(|s| s.trim().is_empty());
    match v {
        Value::Null => true,
        Value::Array(a) => a.iter().all(empty_value),
        Value::Object(_) if v.get("type").is_some() => false, // a genui element
        _ => match v["t"].as_str().unwrap_or("") {
            "table" | "record" | "list" => v["rows"].as_array().is_none_or(Vec::is_empty),
            "text" => blank("v"),
            "error" => blank("stack") && blank("message"),
            "scalar" => false,
            "browser" => v["view"].as_u64().is_none_or(|n| n == 0),
            _ => true,
        },
    }
}

/// A harness REPL cell standing alone (not in a run): its one line ([`steps::call_line`]), and opened, its outputs
/// ([`steps::outs`]). A cell that drew (and did not fail) is its view alone, no line over it: the view is the message,
/// and its code shows under it from the view's hover-only code button.
fn cell_rows(block: &str, d: &Detail, call: Option<&group::Call>, fold: &HashMap<String, bool>, out: &mut Vec<Row>) {
    let failed = call.is_some_and(|c| c.failed);
    let code = fold.get(&code_key(block)).copied().unwrap_or(false);
    let draws = !empty_value(&d.ui);
    if draws && !failed {
        out.push(Row::Ui { v: d.ui.clone(), cell: d.cell.clone(), block: block.to_owned(), code });
        if code {
            out.push(Row::Code { lang: "ts".into(), text: d.code.clone() });
        }
        return;
    }
    let open = fold.get(block).copied().unwrap_or(false);
    if let Some(c) = call {
        out.push(Row::Steps(steps::call_line(block, c, d, open)));
    }
    if draws {
        out.push(Row::Ui { v: d.ui.clone(), cell: d.cell.clone(), block: block.to_owned(), code: false });
    }
    if open {
        steps::outs(block, d, fold, out);
    }
}

/// Item `n`'s rows, keyed `i{n}.{k}` so a row keeps its key as it streams and as runs form around it.
/// A text block shows as far as its cut says ([`Shows`]), as words still coming.
fn item_rows(t: &Transcript, n: usize, shows: Shows<'_>, out: &mut Vec<(String, Row)>) {
    let fold = shows.fold;
    let mut part = Vec::new();
    match &t.items[n] {
        Item::User { text } => part.push(Row::User(text.clone())),
        Item::Text { block, text, done } => match shows.cut.get(block) {
            Some(&k) => blocks(&text[..k.min(text.len())], true, &mut part),
            None => blocks(text, !done, &mut part),
        },
        Item::Thinking { block, text, done } => {
            if !text.trim().is_empty() || !done {
                let open = fold.get(block).copied().unwrap_or(false);
                part.push(Row::Thinking { block: block.clone(), open, text: text.clone(), live: !done });
            }
        }
        Item::Tool { block, name, input, partial, result, done } => {
            if let Some(d) = t.details.get(block) {
                cell_rows(block, d, group::call(t, n).as_ref(), fold, &mut part);
            } else {
                let failed = result.as_ref().is_some_and(|r| r.error);
                // A failure is a small mark in its detail (src/app/chat.rs fill_row), never its words in red.
                // How it is going, as a status icon (scene/widgets/chat_status.flecs): none once it has a result.
                let state = match result {
                    Some(_) => None,
                    None if *done && !t.busy => Some("no_result"),
                    None => Some("running"),
                };
                let input_text = if *done { tool_input(input) } else { partial.clone() };
                part.push(Row::Tool {
                    block: block.clone(),
                    name: name.clone(),
                    arg: model::chat::main_arg(input),
                    state,
                    shown: Shown::of(fold, block),
                    failed,
                    input: input_text,
                    result: result.as_ref().map(|r| r.text.clone()).unwrap_or_default(),
                });
            }
        }
        Item::Note { text } => part.push(Row::Note(text.clone())),
        Item::Error { text } => part.push(Row::Error(text.clone())),
        // A turn ends quietly: no time or cost under it (an interruption shows as its own note).
        Item::TurnEnd { .. } => {}
    }
    for (k, r) in part.into_iter().enumerate() {
        out.push((format!("i{n}.{k}"), r));
    }
}

/// The transcript as rows ([`Grouped`]: runs of calls under one line), and the keys of the rows nested under a run.
/// A run is its one line ([`steps::run_line`]) and nothing else until the person opens it; opened, each cell's outputs
/// ([`steps::outs`], set in by their own prefabs) and each agent tool call and reasoning between them as its own folded
/// line, nested. A text block shows as far as its cut says ([`Shows`]).
fn rows_nested(t: &Transcript, shows: Shows<'_>) -> (Vec<(String, Row)>, HashSet<String>) {
    let fold = shows.fold;
    let g = Grouped::of(t);
    let mut out = Vec::new();
    let mut nested = HashSet::new();
    for piece in &g.pieces {
        match piece {
            Piece::Item(n) => item_rows(t, *n, shows, &mut out),
            Piece::Run(run) => {
                let key = run.key();
                let open = fold.get(&key).copied().unwrap_or(false);
                out.push((key.clone(), Row::Steps(steps::run_line(&key, run, t, open))));
                if !open {
                    continue;
                }
                for &n in &run.items {
                    let from = out.len();
                    let cell = match &t.items[n] {
                        Item::Tool { block, .. } => t.details.get(block).map(|d| (block, d)),
                        _ => None,
                    };
                    match cell {
                        Some((block, d)) => {
                            let mut part = Vec::new();
                            steps::outs(block, d, fold, &mut part);
                            out.extend(part.into_iter().enumerate().map(|(k, r)| (format!("i{n}.{k}"), r)));
                        }
                        None => {
                            item_rows(t, n, shows, &mut out);
                            nested.extend(out[from..].iter().map(|(k, _)| k.clone()));
                        }
                    }
                }
            }
        }
    }
    (out, nested)
}

/// The transcript as rows.
fn rows(t: &Transcript, shows: Shows<'_>) -> Vec<(String, Row)> {
    rows_nested(t, shows).0
}

/// What decides how the transcript shows besides itself: the folds the person set, and how far each text block still
/// streaming shows (byte offset, by block; a block not in it shows whole).
#[derive(Clone, Copy)]
struct Shows<'a> {
    fold: &'a HashMap<String, bool>,
    cut: &'a HashMap<String, usize>,
}

impl<'a> Shows<'a> {
    fn of(st: &'a ChatState) -> Self {
        Self { fold: &st.fold, cut: &st.cut }
    }
}

/// How much of a text block still streaming in can show without what shows changing as more comes (byte offset): up to
/// its last whitespace (a word still coming would hop to the next line as it grows), short of an inline code span or a
/// bold run not closed yet (its markers would show, then vanish and restyle it), and short of a last line that is only a
/// block's opening so far (a list's `-`, a heading's `#`, a fence: the line would change kind). Done, all of it.
fn settled_len(text: &str, done: bool) -> usize {
    if done {
        return text.len();
    }
    let mut end = text.char_indices().rev().find(|(_, c)| c.is_whitespace()).map_or(0, |(i, c)| i + c.len_utf8());
    // The line still being written: held back while it only opens a block.
    let line = text[..end].rfind('\n').map_or(0, |i| i + 1);
    let head = text[line..end].trim_start();
    let fence = |l: &str| l.starts_with("```") || l.starts_with("~~~");
    let marker = |w: &str| {
        matches!(w, "-" | "*" | "+" | ">" | "|")
            || w.chars().all(|c| c == '#')
            || w.strip_suffix(['.', ')']).is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
    };
    if fence(head) || (!head.is_empty() && head.split_whitespace().all(marker)) {
        end = line;
    }
    // Inside an open fence the words are code: no inline markers to balance.
    if text[..end].lines().filter(|l| fence(l.trim_start())).count() % 2 == 1 {
        return end;
    }
    let para = text[..end].rfind("\n\n").map_or(0, |i| i + 2);
    let tail = &text[para..end];
    if tail.matches('`').count() % 2 == 1 {
        end = para + tail.rfind('`').unwrap_or(0);
    }
    let tail = &text[para..end];
    if tail.matches("**").count() % 2 == 1 {
        end = para + tail.rfind("**").unwrap_or(0);
    }
    end
}

/// Whether row key `key` (a call's block, a run's `run:` key, a thinking's block, a card's `block#code`, a value's
/// `block#v…`) shows open now: only when the person opened it.
fn is_open(fold: &HashMap<String, bool>, key: &str) -> bool {
    fold.get(key).copied().unwrap_or(false)
}

/// UTF-16 units in `s`.
fn units(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The byte offset of UTF-16 unit `u` of `s` (a unit inside a character: that character's start; past the end: the end).
fn byte_of(s: &str, u: usize) -> usize {
    let mut n = 0;
    for (b, c) in s.char_indices() {
        n += c.len_utf16();
        if n > u {
            return b;
        }
    }
    s.len()
}

/// The word (letters, digits, `_`) at byte `i` of `s` (at its end, the word before), or the one character there.
fn word_around(s: &str, i: usize) -> (usize, usize) {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    match s[i..].chars().next() {
        Some(c) if word(c) => {
            let a = s[..i].char_indices().rev().take_while(|&(_, c)| word(c)).last().map_or(i, |(k, _)| k);
            let b = s[i..].char_indices().find(|&(_, c)| !word(c)).map_or(s.len(), |(k, _)| i + k);
            (a, b)
        }
        Some(c) => (i, i + c.len_utf8()),
        None => match s[..i].chars().next_back() {
            Some(c) if word(c) => word_around(s, i - c.len_utf8()),
            _ => (i, i),
        },
    }
}

/// The words of a row a selection can cover, in reading order: each one's path below the row, what it is given (as
/// [`fill_row`] gives it) and whether it is inline markdown.
fn texts(r: &Row) -> Vec<(&'static [&'static str], String, bool)> {
    match r {
        Row::User(s) => vec![(&["bubble", "text"], s.clone(), false)],
        Row::Prose(s) | Row::Heading(_, s) | Row::Quote(s) => vec![(&["text"], s.clone(), true)],
        Row::ListItem { mark, text, .. } => vec![(&["mark"], mark.clone(), false), (&["text"], text.clone(), true)],
        Row::Code { text, .. } => vec![(&["card", "text"], text.clone(), false)],
        Row::Tool { shown: Shown::Full, input, result, .. } => vec![
            (&["body", "input"], input.clone(), false),
            (&["body", "result"], tool_result(result).to_owned(), false),
        ],
        Row::Thinking { open: true, text, .. } => vec![(&["body", "text"], text.trim().to_owned(), false)],
        Row::Note(s) | Row::Error(s) => vec![(&["text"], s.clone(), false)],
        Row::Steps(l) => vec![(&["head", "words"], l.words.clone(), false)],
        Row::Out(o) => o.texts(),
        Row::Tool { .. } | Row::Thinking { .. } | Row::Grid { .. } | Row::Table(_) | Row::Ui { .. } => Vec::new(),
    }
}

/// A tool's result as its open row shows it (an ellipsis while it has none).
fn tool_result(result: &str) -> &str {
    if result.is_empty() { "\u{2026}" } else { result.trim_end() }
}

/// What words given `src` show (inline markdown: the markers out), which a [`Spot`]'s units count in.
fn shown(src: &str, md: bool) -> String {
    if md { harness_native_render::markdown::inline(src).text } else { src.to_owned() }
}

/// The units of row `row`'s `part`th words (`len` long) the selection `lo..hi` covers, if any.
fn covered(lo: Spot, hi: Spot, row: usize, part: usize, len: usize) -> Option<(usize, usize)> {
    let here = (row, part);
    if here < (lo.row, lo.part) || here > (hi.row, hi.part) {
        return None;
    }
    let a = if here == (lo.row, lo.part) { lo.at.min(len) } else { 0 };
    let b = if here == (hi.row, hi.part) { hi.at.min(len) } else { len };
    Some((a, b))
}

/// A row selected only whole (a table, search results, a drawn card): no words of its own to place a selection in,
/// copied as [`Row::markdown`]. A selection takes it when it reaches past its middle: its one place is `at` 0 (above
/// the middle) or 1 (below), in part 0.
fn atomic(r: &Row) -> bool {
    texts(r).is_empty() && r.markdown().is_some_and(|m| !m.trim().is_empty())
}

/// Whether the selection `lo..hi` takes atomic row `row` whole.
fn takes_whole(lo: Spot, hi: Spot, row: usize) -> bool {
    lo <= Spot { row, part: 0, at: 0 } && Spot { row, part: 0, at: 1 } <= hi
}

impl Row {
    /// The row as markdown, for a copy that takes it whole (None: its words as shown).
    fn markdown(&self) -> Option<String> {
        Some(match self {
            Row::User(s) | Row::Prose(s) | Row::Note(s) | Row::Error(s) => s.clone(),
            Row::Heading(n, s) => format!("{} {s}", "#".repeat(usize::from(*n))),
            Row::ListItem { depth, mark, text } => {
                let mark = match mark.as_str() {
                    "\u{2022}" => "-",
                    "\u{2610}" => "- [ ]",
                    "\u{2611}" => "- [x]",
                    m => m,
                };
                format!("{}{mark} {text}", "  ".repeat(*depth))
            }
            Row::Quote(s) => format!("> {s}"),
            Row::Code { lang, text } => format!("```{lang}\n{text}\n```"),
            Row::Table(t) => t.markdown(),
            Row::Grid { v, .. } => grid_table(v).markdown(),
            Row::Out(o) => return o.markdown(),
            Row::Ui { v, .. } => genui::plain(v),
            Row::Tool { .. } | Row::Thinking { .. } | Row::Steps(_) => {
                return None;
            }
        })
    }
}

/// The transcript's text from `lo` to `hi`, for the pasteboard: a row taken whole as markdown, a row cut by the
/// selection as it shows; rows a blank line apart (list items a line).
fn transcript_text(rows: &[(String, Row)], lo: Spot, hi: Spot) -> String {
    let mut out = String::new();
    let mut last: Option<&Row> = None;
    for (i, (_, r)) in rows.iter().enumerate().take(hi.row + 1).skip(lo.row) {
        let mut whole = true;
        let mut bits = Vec::new();
        for (k, (_, src, md)) in texts(r).iter().enumerate() {
            let s = shown(src, *md);
            let n = units(&s);
            let Some((a, b)) = covered(lo, hi, i, k, n) else {
                whole = false;
                continue;
            };
            whole &= a == 0 && b == n;
            if a < b {
                bits.push(s[byte_of(&s, a)..byte_of(&s, b)].to_owned());
            }
        }
        // A row with no words to select (a table) is taken whole when the selection reaches past its middle.
        let piece = if bits.is_empty() {
            match r.markdown().filter(|_| atomic(r) && takes_whole(lo, hi, i)) {
                Some(md) => md,
                None => continue,
            }
        } else if whole {
            r.markdown().unwrap_or_else(|| bits.join("\n"))
        } else {
            bits.join(" ")
        };
        if let Some(prev) = last {
            let list = matches!(prev, Row::ListItem { .. }) && matches!(r, Row::ListItem { .. });
            out.push_str(if list { "\n" } else { "\n\n" });
        }
        out.push_str(&piece);
        last = Some(r);
    }
    out
}

/// Words node `e` laid out as the painter draws it (its lines broken at its laid width), and where it is (window points).
fn words_layout(paint: &mut Painter, e: EntityView<'_>) -> Option<(Rc<Layout>, Laid)> {
    let text = e.try_cloned::<&Text>()?;
    let laid = e.try_cloned::<&Laid>()?;
    let style = e.try_cloned::<&TextStyle>().unwrap_or_default();
    let md = e.has(MarkdownText::id());
    Some((paint.layout_as(text.s.as_str(), &style, laid.w.max(1.0) + 0.5, md), laid))
}

/// Where editable words node `e` shows its caret (window points, x, y, width, height), for the input method's candidate
/// window: at the start of its marked text, else at its caret (its `TextEdit`), on its layout ([`words_layout`]), a line
/// tall. None while it is not laid out or edits nothing.
fn words_caret(paint: &mut Painter, e: EntityView<'_>) -> Option<[f32; 4]> {
    let (lay, l) = words_layout(paint, e)?;
    let te = e.try_cloned::<&TextEdit>()?;
    let at = if te.mark_hi > te.mark_lo { te.mark_lo } else { te.caret } as usize;
    let (x, top, h) = match lay.lines.is_empty() {
        true => (0.0, 0.0, l.h),
        false => {
            let (x, li) = lay.caret(at);
            (x, lay.lines[li].top, lay.lines[li].height)
        }
    };
    Some([l.x + x, l.y + top, 2.0, h])
}

/// The scroll along one axis that keeps place `at` (points from the content's start) in a view `view` long over content
/// `len` long, moved from `from` as little as it can: kept while `at` shows, else just far enough to show it at the near
/// edge, and never past the content's end (so it comes back as the content shrinks).
fn keep_in_view(from: f32, at: f32, view: f32, len: f32) -> f32 {
    let to = if at < from {
        at
    } else if at > from + view {
        at - view
    } else {
        from
    };
    to.clamp(0.0, (len - view).max(0.0))
}

/// Chat pane `pv`'s composer card (scene/widgets/chat.flecs hn.ChatPane's foot.card: the words over the bar).
fn composer(pv: View<'_>) -> Option<View<'_>> {
    pv.part("foot")?.part("card")
}

/// Chat pane `pv`'s composer's words' area (its ui.field.GrowingTextArea).
fn composer_area(pv: View<'_>) -> Option<View<'_>> {
    composer(pv)?.part("entry")?.part("area")
}

/// Chat pane `p`'s input words.
fn input_text(w: WorldRef<'_>, p: u64) -> Option<View<'_>> {
    composer_area(View::of(w.entity_from_id(p)))?.part("text")
}

impl App {
    /// The genui gallery (`HARNESS_NATIVE_GALLERY=genui`, src/gallery.rs): fill its `hn.ChatUi` with `tree`, a cell's
    /// tree as the kernel sends it, measured by this app's typesetter and drawn in its ground's scheme, as a chat card's is.
    pub(crate) fn fill_genui_gallery(&self, root: u64, tree: &Value) {
        let w = self.world.world_ref();
        let v = View::of(w.entity_from_id(root));
        let scheme = harness_native_ui_look_api::Scheme { dark: self.chat_dark() };
        if v.entity().try_cloned::<&harness_native_ui_look_api::Scheme>() != Some(scheme) {
            v.entity().set(scheme);
        }
        let Some(ui) = v.part("ui") else { return };
        let mut paint = self.paint.borrow_mut();
        let mut measure = |s: &str, style: &TextStyle, md: bool| paint.width(s, style, md);
        // HARNESS_NATIVE_GENUI_CELLS: the cells a CellRef in the tree shows (checked as the shot starts).
        let cells = crate::gallery::genui_cells().ok().flatten();
        genui::fill_in(ui, tree, "gallery.0", cells, Some(&self.genui), true, &mut measure);
    }

    /// Whether a chat (its transcript, composer and cells, on `tone_chat_ground`) draws dark: the window's appearance.
    pub(super) fn chat_dark(&self) -> bool {
        self.dark
    }

    /// Whether the terminal's ground (hn.Main's Fill, `tone_term_ground`) is dark in the window's appearance.
    pub(super) fn term_ground_dark(&self) -> bool {
        let w = self.world.world_ref();
        let Some(f) = view::prefab(w, "hn.Main").and_then(|e| e.try_cloned::<&Fill>()) else { return self.dark };
        let c = if self.dark { f.dark } else { f.light };
        0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b < 0.5
    }

    /// Whether the focused pane shows a chat session (keys go to its input).
    pub(super) fn chat_focused(&self) -> bool {
        self.focused_chat().is_some()
    }

    /// The focused pane, its chat session's object and session id, when it shows one.
    fn focused_chat(&self) -> Option<(u64, u64, u64)> {
        let p = self.focused_pane()?;
        let o = model::shows(self.world.world_ref(), p)?;
        if model::pane_kind(o) != model::PaneKind::Chat {
            return None;
        }
        let e = *o.id();
        Some((p, e, self.session_of(e)?))
    }

    /// Chat `sid` was spawned on `model` at `effort` (cmd+N's remembered choice, or a pick of another kind's model): its
    /// choice from now, so the frame that first shows it already names them ([`Transcript::start_on`]), and again at each
    /// attach. Neither given (a deep link's session, a plain spawn): nothing to seed.
    pub(in crate::app) fn chat_started_on(&self, sid: u64, model: Option<&str>, effort: Option<&str>) {
        if model.is_none() && effort.is_none() {
            return;
        }
        self.chat.borrow_mut().entry(sid).or_default().started = Some((model.map(str::to_owned), effort.map(str::to_owned)));
        self.chats.borrow_mut().entry(sid).or_default().start_on(model, effort);
    }

    /// Fill a chat pane from its session's transcript: attach for its events, its rows in view, its input and status.
    pub(super) fn fill_chat_pane(&self, pane: u64, e: u64) {
        let Some(sid) = self.session_of(e) else { return };
        let w = self.world.world_ref();
        let pv = View::of(w.entity_from_id(pane));
        let mut states = self.chat.borrow_mut();
        let st = states.entry(sid).or_default();
        // Its own stream of events: attach (again a second after a try while it runs, once when it ended). The stream
        // outlives the agent (a resume's events come on it), and a past chat's is its log replayed by the daemon. A stream
        // the link holds from before the session was known to be a chat is a terminal's: dropped for ours.
        let attached = self.link.attached(sid);
        if attached && !st.ours {
            self.link.detach(sid);
        } else if !attached {
            st.ours = false;
            let running = self.status_of(e) == "running";
            let due = match st.tried {
                None => true,
                Some(t) => running && self.time - t > 1.0,
            };
            if due {
                st.tried = Some(self.time);
                st.ours = true;
                // The log replays from the start: what comes first is history again.
                st.stream.clear();
                st.cut.clear();
                st.row_born.clear();
                let mut chats = self.chats.borrow_mut();
                chats.remove(&sid);
                if let Some((m, e)) = &st.started {
                    chats.entry(sid).or_default().start_on(m.as_deref(), e.as_deref());
                }
                drop(chats);
                self.link.attach(sid, 80, 24, true);
            }
        }
        // Drawn in the window's appearance on the chat's own ground (`tone_chat_ground`, not the terminal's): a chat is a
        // page, so it takes the app's palette, not the Ghostty theme's.
        let scheme = harness_native_ui_look_api::Scheme { dark: self.chat_dark() };
        for part in ["list", "foot"] {
            if let Some(v) = pv.part(part)
                && v.entity().try_cloned::<&harness_native_ui_look_api::Scheme>() != Some(scheme)
            {
                v.entity().set(scheme);
            }
        }
        let chats = self.chats.borrow();
        let empty = Transcript::default();
        let t = chats.get(&sid).unwrap_or(&empty);
        let (rows, nested) = rows_nested(t, Shows::of(st));
        // The rows of blocks that streamed in while this pane watched: their glyphs fade in as they come, from when the row
        // was first laid out (a row first painted long after, scrolled back to, shows at rest).
        let came = crate::look::arrive(w, "chat_stream");
        let mut arrives: HashMap<&str, GlyphsArrive> = HashMap::new(); // alloc: the frame's live rows, read by fresh_rows
        for (key, _) in &rows {
            let item = key.strip_prefix('i').and_then(|k| k.split_once('.')).and_then(|(n, _)| n.parse::<usize>().ok());
            let live = item.and_then(|n| t.items.get(n)).is_some_and(|it| {
                matches!(it, Item::Text { block, .. } if st.stream.get(block).is_some_and(|s| s.live))
            });
            if live {
                let born = match st.row_born.get(key) {
                    Some(&b) => b,
                    None => {
                        st.row_born.insert(key.clone(), self.time); // alloc: one key per streamed row, kept with the pane
                        self.time
                    }
                };
                arrives.insert(key.as_str(), GlyphsArrive { born, ..came });
            }
        }
        // A run's rows sit in from its line by `chat_nest` (each row's own padding, from its prefab, plus that).
        let nest_by = view::prefab(w, "hn.ChatNest").and_then(|p| p.try_cloned::<&harness_native_ui_layout_api::Padding>()).map_or(0.0, |p| p.l);
        if let Some(list) = pv.part("list")
            && let Some(stack) = list.part("rows")
        {
            let room = list.entity().try_cloned::<&Laid>().map_or(600.0, |l| l.h);
            let pad = stack.entity().try_cloned::<&harness_native_ui_layout_api::Padding>().unwrap_or_default();
            // The transcript's selection: the selection plate under the words it covers (harness-native-render select.rs).
            let sel = st.sel.map(|(a, b)| (a.min(b), a.max(b))).filter(|(a, b)| a != b);
            // Tables measure their columns with the typesetter.
            let mut paint = self.paint.borrow_mut();
            let mut measure = |s: &str, style: &TextStyle, md: bool| paint.width(s, style, md);
            let rows_open = &st.rows_open;
            let fresh_rows = |row: View<'_>, i: usize, new: bool| {
                if new {
                    row.set(HoverAmt::default());
                    if let Some(h) = row.part("head") {
                        h.set(HoverAmt::default());
                    }
                }
                let ctx = RowCtx { sid, key: &rows[i].0, open: rows_open.get(&rows[i].0).copied(), ui: Some(&self.genui), playing: t.busy, cells: Some(&t.details) };
                fill_row(row, &rows[i].1, &ctx, &mut measure);
                if let Some(mut pad) = view::prefab(w, rows[i].1.prefab()).and_then(|p| p.try_cloned::<&harness_native_ui_layout_api::Padding>()) {
                    if nested.contains(&rows[i].0) {
                        pad.l += nest_by;
                    }
                    if row.entity().try_cloned::<&harness_native_ui_layout_api::Padding>() != Some(pad) {
                        row.set(pad);
                    }
                }
                // A line's own guide (hn.ChatGuideOff, clear) shows as hn.ChatGuide's when the line is one of an opened
                // run's calls, so the run's calls hang on one guide with what they opened.
                if matches!(rows[i].1, Row::Tool { .. } | Row::Thinking { .. })
                    && let Some(g) = row.part("guide").and_then(|g| g.part("line"))
                {
                    let look = if nested.contains(&rows[i].0) { "hn.ChatGuide" } else { "hn.ChatGuideOff" };
                    let want = view::prefab(w, look).and_then(|p| View::of(p).part("line")).and_then(|l| l.entity().try_cloned::<&Fill>());
                    if let Some(f) = want
                        && g.entity().try_cloned::<&Fill>() != Some(f)
                    {
                        g.set(f);
                    }
                }
                if atomic(&rows[i].1) {
                    let whole = sel.is_some_and(|(lo, hi)| takes_whole(lo, hi, i));
                    select_all_words(row.entity(), whole);
                }
                let arrive = arrives.get(rows[i].0.as_str()).copied();
                for (k, (path, src, md)) in texts(&rows[i].1).iter().enumerate() {
                    let Some(node) = path.iter().try_fold(row, |v, n| v.part(n)) else { continue };
                    let e = node.entity();
                    match arrive {
                        Some(a) if e.try_cloned::<&GlyphsArrive>() != Some(a) => _ = e.set(a),
                        None if e.has(GlyphsArrive::id()) => _ = e.remove(GlyphsArrive::id()),
                        _ => {}
                    }
                    let range = sel.and_then(|(lo, hi)| covered(lo, hi, i, k, units(&shown(src, *md))));
                    match range.filter(|(a, b)| a < b) {
                        Some((a, b)) => {
                            let te = TextEdit { caret: b as u32, anchor: a as u32, ..TextEdit::default() };
                            if e.try_cloned::<&TextEdit>() != Some(te) {
                                e.set(te);
                            }
                        }
                        None if e.has(TextEdit::id()) => _ = e.remove(TextEdit::id()),
                        None => {}
                    }
                }
            };
            let s = view::lazy_stack(
                stack,
                &mut st.heights,
                (st.y - pad.t).max(0.0),
                room,
                rows.len(),
                |i| rows[i].0.as_str(),
                |i| rows[i].1.prefab(),
                // A cell with a dialog or a sheet open keeps its row out of view: a modal stays up until it is dismissed.
                |i| matches!(&rows[i].1, Row::Ui { v, .. } if genui::holds_modal(v)),
                fresh_rows,
            );
            st.content = s.content + pad.t + pad.b;
            st.shift += s.shift;
        }
        // The input: its words (the marked text in), caret and selection, focused while the pane has the keyboard.
        let focused =
            self.focused && self.focused_pane() == Some(pane) && !self.tree_focus && self.palette.is_none() && !self.genui_editing()
                && !self.renaming();
        // Its chips: the one under the pointer, its card, and the live state of the pull requests and issues it names.
        self.fill_chat_peek(pv, st, t, &rows, focused);
        let ended = self.status_of(e) != "running";
        if let Some(area) = composer_area(pv) {
            let (shown, [caret, anchor, lo, hi]) = st.draft.shown();
            if let Some(h) = area.part("hint_at").and_then(|a| a.part("hint")) {
                // Its agent's name ("Message Codex" in a Codex session), and that a message resumes an ended one.
                let agent = match model::kind_name(w.entity_from_id(e)) {
                    "codex" => "Codex",
                    "cursor" => "Cursor",
                    _ => "Claude",
                };
                let hint = match (shown.is_empty(), ended) {
                    (false, _) => String::new(),
                    (true, false) => format!("Message {agent}"),
                    (true, true) => format!("Message {agent} to resume"),
                };
                h.text(&hint);
            }
            if let Some(txt) = area.part("text") {
                txt.text(&shown);
                let mut te = txt.entity().try_cloned::<&TextEdit>().unwrap_or_default();
                // The caret blinks as the terminal's cursor does (Ghostty's cursor-style-blink), solid again as it moves.
                let blink = self.look.themes[usize::from(self.dark)].blink;
                let moved = (te.caret, te.anchor, te.mark_lo, te.mark_hi) != (caret, anchor, lo, hi) || (focused && !te.focused);
                let since = if moved { self.time } else { te.since };
                let next = TextEdit { caret, anchor, mark_lo: lo, mark_hi: hi, focused, blink, since, ..te };
                if next != te {
                    te = next;
                    txt.entity().set(te);
                }
                // Grown past its most, it scrolls on its FieldScroll (step_chat_inputs), the caret kept in view.
            }
        }
        if let Some(bar) = composer(pv).and_then(|c| c.part("bar")) {
            let o = w.entity_from_id(e);
            // The model and the effort it runs with, as den's picker (chat/pick.rs `Label`), the daemon's listing its last
            // say.
            let listed = o.try_cloned::<&model::AgentModel>().unwrap_or_default();
            let listed = (listed.s.as_str(), listed.effort.as_str());
            self.fill_chat_pick(bar, sid, t, model::kind_name(o), st.pick, &mut st.pick_nav, listed);
            // While the agent works, its breathing ring (hn.ChatWorking), else nothing: Return sends, Escape stops.
            if let Some(act) = bar.part("act") {
                let working = t.busy && !ended;
                view::for_each_of(act, working.then_some(("working", "hn.ChatWorking", ())), |_, (), _| {});
            }
        }
    }

    /// Once a frame's layout has settled: every chat pane whose transcript was laid out at a new width this frame (a cmd+S
    /// fold, a resize: its words re-wrapped live) holds its view still in this same frame. Stuck to the bottom, the bottom
    /// stays; scrolled up, the row at the top of the view keeps its share above the top (`view::lazy_anchor`), so the line
    /// being read stays put while its words re-wrap. Whether a scroll moved (the frame is laid out once more).
    pub(super) fn anchor_chats(&mut self) -> bool {
        let w = self.world.world_ref();
        let mut moved = false;
        for p in self.panes() {
            let Some(o) = model::shows(w, p) else { continue };
            if model::pane_kind(o) != model::PaneKind::Chat {
                continue;
            }
            let Some(sid) = self.session_of(*o.id()) else { continue };
            let Some(list) = View::of(w.entity_from_id(p)).part("list") else { continue };
            let Some(stack) = list.part("rows") else { continue };
            let Some(l) = list.entity().try_cloned::<&Laid>().filter(|l| l.h > 0.0) else { continue };
            let mut states = self.chat.borrow_mut();
            let Some(st) = states.get_mut(&sid) else { continue };
            let pad = stack.entity().try_cloned::<&harness_native_ui_layout_api::Padding>().unwrap_or_default();
            let Some(a) = view::lazy_anchor(stack, &mut st.heights, (st.y - pad.t).max(0.0)) else { continue };
            st.content += a.grew;
            let max = (st.content - l.h).max(0.0);
            let by = if st.stick { a.grew } else { a.shift };
            let y = (st.y + by).clamp(0.0, max);
            if (y - st.y).abs() <= 0.001 {
                continue;
            }
            // The follow keeps its distance to the end; the physics adopts the new place on its next step (`rev`).
            st.follow.shift(y - st.y);
            st.y = y;
            st.rev += 1;
            scroll_quietly(list.entity(), ScrollState { x: 0.0, y });
            moved = true;
        }
        moved
    }

    /// Step every chat pane's scroll a frame: stuck to the bottom while it is, kept still when rows above grew, the
    /// physics advanced.
    pub(super) fn step_chat_scroll(&mut self) {
        let w = self.world.world_ref();
        let mut moving = false;
        for p in self.panes() {
            let Some(o) = model::shows(w, p) else { continue };
            if model::pane_kind(o) != model::PaneKind::Chat {
                continue;
            }
            let Some(sid) = self.session_of(*o.id()) else { continue };
            let Some(list) = View::of(w.entity_from_id(p)).part("list") else { continue };
            let Some(l) = list.entity().try_cloned::<&Laid>().filter(|l| l.h > 0.0) else { continue };
            let mut states = self.chat.borrow_mut();
            let st = states.entry(sid).or_default();
            // A drag or a page on the overlay scroller moved the list: that is the origin now, off the bottom.
            if let Some(s) = list.entity().try_cloned::<&ScrollState>().filter(|s| (s.y - st.y).abs() > 0.01)
                && list.entity().try_cloned::<&ScrollerGrip>().is_some_and(|g| g.drag != 0 || g.page != 0)
            {
                st.y = s.y;
                st.stick = false;
                st.rev += 1;
            }
            let t = st.clock.elapsed().as_secs_f64();
            // A frame's step, capped so a pane first shown (or a stall) does not leap.
            let dt = ((t - st.stepped) as f32).clamp(0.0, 0.1);
            st.stepped = t;
            if self.pace_chat(sid, st, dt) {
                moving = true;
            }
            let max = (st.content - l.h).max(0.0);
            let mut origin = st.y;
            let busy = st.engine.needs_frame();
            if st.stick && !busy {
                // Following the agent: the end glides in on a spring as rows grow under it (rows above it that changed
                // height moved it as far), instead of the view jumping a line at a time.
                st.follow.shift(st.shift);
                // While the log replays (a pane just opened) it lands on the end: the history was there, not written now.
                let replay = st.tried.is_some_and(|a| self.time - a < REPLAY_SECS);
                let far = if replay { 0.0 } else { crate::look::width(w, "chat_stream") };
                let (y, going) = st.follow.step(max, crate::look::tune(w, "chat_stream"), far, dt);
                if (y - st.y).abs() > 0.01 {
                    origin = y;
                    st.rev += 1;
                }
                moving |= going;
            } else if st.shift.abs() > 0.01 && !busy {
                origin = (st.y + st.shift).clamp(0.0, max);
                st.rev += 1;
            } else if st.y > max && !busy {
                origin = max;
                st.rev += 1;
            }
            st.shift = 0.0;
            st.engine.configure(sidebar::config(
                l.w,
                l.h,
                st.content,
                origin,
                st.rev,
                1.0 / f64::from(self.grid.scale.max(0.5)),
            ));
            st.engine.advance(t);
            let y = st.engine.report().y as f32;
            if st.engine.needs_frame() {
                moving = true;
            } else if !st.stick && y >= max - 2.0 {
                // Scrolled back to the bottom: it follows the agent again.
                st.stick = true;
            }
            st.y = y;
            if !st.stick || st.engine.needs_frame() {
                // The reader's scroll has it: a follow begins where they leave it.
                st.follow = Follow::at(y);
            }
            if list.entity().try_cloned::<&ScrollState>().is_none_or(|s| (s.y - y).abs() > 0.001) {
                // Following the agent at the bottom moves it quietly; a scroll of the reader's shows the scroller.
                if st.stick && !st.engine.needs_frame() {
                    scroll_quietly(list.entity(), ScrollState { x: 0.0, y });
                } else {
                    list.entity().set(ScrollState { x: 0.0, y });
                }
            }
        }
        moving |= self.step_chat_inputs();
        if moving {
            self.poke();
        }
        // A focused input's caret (or a name's being renamed) fades on the clock: keep drawing.
        if self.focused
            && self.look.themes[usize::from(self.dark)].blink
            && (self.renaming()
                || (self.chat_focused() && !self.tree_focus && self.palette.is_none() && !self.genui_editing()))
        {
            self.poke_why("caret blink");
        }
        // A cell's controls: its field's typing, a dragged slider, an open select.
        self.step_genui();
        // A press off the picker's open menu closes it.
        self.step_chat_pick();
        self.step_genui_common();
    }

    /// Step session `sid`'s text blocks' pace a frame `dt` seconds long: each shows as far as it can without changing
    /// ([`settled_len`]), reached at its pace (hn.Theme.chat_stream's `Pace`), so a burst of words reads in evenly instead
    /// of landing as a paragraph in one frame. A block first seen just after an attach is the log replayed: whole at once.
    /// Whether one still has words to show (another frame is wanted).
    fn pace_chat(&self, sid: u64, st: &mut ChatState, dt: f32) -> bool {
        let pace = crate::look::pace(self.world.world_ref(), "chat_stream");
        let chats = self.chats.borrow();
        let Some(t) = chats.get(&sid) else { return false };
        let replay = st.tried.is_some_and(|a| self.time - a < REPLAY_SECS);
        let mut behind = false;
        for item in &t.items {
            let Item::Text { block, text, done } = item else { continue };
            let have = settled_len(text, *done);
            if !st.stream.contains_key(block) {
                let s = Stream { paced: Paced::at(if replay { have } else { 0 }), live: !replay };
                st.stream.insert(block.clone(), s); // alloc: one entry per text block, kept with the pane
            }
            let Some(s) = st.stream.get_mut(block) else { continue };
            let mut k = s.paced.step(have, pace, dt);
            while !text.is_char_boundary(k) {
                k -= 1;
            }
            behind |= !s.paced.caught_up(have);
            if k >= text.len() {
                st.cut.remove(block);
            } else if let Some(c) = st.cut.get_mut(block) {
                *c = k;
            } else {
                st.cut.insert(block.clone(), k); // alloc: one key per block still streaming
            }
        }
        behind
    }

    /// Step every chat pane's composer scroll a frame (harness_native_ui_view::field): its caret's line from the last
    /// layout kept in view, a wheel's physics advanced. Whether one still moves.
    fn step_chat_inputs(&self) -> bool {
        let w = self.world.world_ref();
        let points_per_pixel = 1.0 / f64::from(self.grid.scale.max(0.5));
        let mut moving = false;
        for p in self.panes() {
            let Some(o) = model::shows(w, p) else { continue };
            if model::pane_kind(o) != model::PaneKind::Chat {
                continue;
            }
            let Some(sid) = self.session_of(*o.id()) else { continue };
            let Some(area) = composer_area(View::of(w.entity_from_id(p))) else {
                continue;
            };
            let caret =
                input_text(w, p).and_then(|t| words_caret(&mut self.paint.borrow_mut(), t.entity())).map(|[_, y, _, h]| [y, y + h]);
            let mut states = self.chat.borrow_mut();
            moving |= states.entry(sid).or_default().input_scroll.step(area.entity(), caret, points_per_pixel);
        }
        moving
    }

    /// A scroll over a chat pane's composer while its words outgrow it scrolls them (its FieldScroll's physics); else
    /// over its transcript, the transcript's physics take it (scrolling up lets go of the bottom). Whether it was over
    /// either.
    pub(super) fn chat_wheel(&mut self, ev: &sidebar::Wheel, z: f32) -> bool {
        let w = self.world.world_ref();
        let (x, y) = (self.pointer.x, self.pointer.y);
        let k = if ev.precise { 1.0 / z } else { 1.0 };
        let input = self.panes().into_iter().find_map(|p| {
            let o = model::shows(w, p)?;
            if model::pane_kind(o) != model::PaneKind::Chat {
                return None;
            }
            let area = composer_area(View::of(w.entity_from_id(p)))?;
            area.entity().try_cloned::<&Laid>()?.contains(x, y).then_some(())?;
            Some((self.session_of(*o.id())?, area.entity()))
        });
        if let Some((sid, area)) = input {
            let took = self.chat.borrow_mut().entry(sid).or_default().input_scroll.wheel(
                area,
                [ev.dx * k, ev.dy * k],
                ev.precise,
                ev.phase,
                ev.momentum,
            );
            if took {
                self.poke();
                return true;
            }
        }
        let Some((sid, _)) = self.panes().into_iter().find_map(|p| {
            let o = model::shows(w, p)?;
            if model::pane_kind(o) != model::PaneKind::Chat {
                return None;
            }
            let l = View::of(w.entity_from_id(p)).part("list")?.entity().try_cloned::<&Laid>()?;
            l.contains(x, y).then(|| (self.session_of(*o.id()), l)).and_then(|(s, l)| s.map(|s| (s, l)))
        }) else {
            return false;
        };
        let mut states = self.chat.borrow_mut();
        let st = states.entry(sid).or_default();
        let t = st.clock.elapsed().as_secs_f64();
        st.engine.wheel(
            Input {
                delta: Vec2::new(f64::from(ev.dx * k), f64::from(ev.dy * k)),
                precise: ev.precise,
                direct: Phase::from_ns(ev.phase),
                momentum: Phase::from_ns(ev.momentum),
                timestamp: t,
            },
            t,
        );
        if ev.dy > 0.0 {
            st.stick = false;
        }
        drop(states);
        self.poke();
        true
    }

    /// Open or fold a tool call's, a run's or thinking's row, a card's code (`block#code`) or a value (`block#v…`): the other way from how it
    /// shows now, kept by its key as more calls stream in.
    pub(super) fn chat_toggle(&mut self, block: &str) {
        let Some((_, _, sid)) = self.focused_chat().or_else(|| self.any_chat_with(block)) else { return };
        let mut states = self.chat.borrow_mut();
        let st = states.entry(sid).or_default();
        let open = is_open(&st.fold, block);
        st.fold.insert(block.to_owned(), !open);
        drop(states);
        self.poke();
    }

    fn any_chat_with(&self, block: &str) -> Option<(u64, u64, u64)> {
        // A card's code (`block#code`) and a value (`block#v…`) belong to their call's block.
        let block = block.split('#').next().unwrap_or(block);
        let block = block.strip_prefix("run:").unwrap_or(block);
        let chats = self.chats.borrow();
        let sid = chats.iter().find(|(_, t)| t.items.iter().any(|i| matches!(i, Item::Tool { block: b, .. } | Item::Thinking { block: b, .. } if b == block)))?.0;
        Some((0, 0, *sid))
    }

    /// Run `f` on the focused chat's draft; true when there is one.
    fn with_draft(&mut self, f: impl FnOnce(&mut Draft)) -> bool {
        // A name being renamed takes them first (chat/rename.rs), then a cell's field that has the keys, then the input.
        if self.renaming() {
            return self.rename_draft(f);
        }
        if self.genui_editing() {
            return self.genui_draft(f);
        }
        let Some((_, _, sid)) = self.focused_chat() else { return false };
        f(&mut self.chat.borrow_mut().entry(sid).or_default().draft);
        self.poke();
        true
    }

    /// Send chat `sid`'s draft (Return), the draft emptied;
    /// nothing while it holds only blanks.
    pub(in crate::app) fn chat_send_draft(&mut self, sid: u64) {
        let text = {
            let mut states = self.chat.borrow_mut();
            let Some(st) = states.get_mut(&sid) else { return };
            let d = &mut st.draft;
            if d.text.trim().is_empty() {
                return;
            }
            let text = std::mem::take(&mut d.text).trim_end().to_owned();
            (d.caret, d.anchor, d.marked) = (0, 0, None);
            st.stick = true;
            text
        };
        self.link.chat_send(sid, &text);
        self.poke();
    }

    /// An `act.Edit` on the focused chat's input (scene/20-keys.flecs ctx.Chat).
    pub(super) fn chat_edit(&mut self, op: &str) {
        // A name being renamed has the keys before anything in the pane (chat/rename.rs).
        if self.renaming() {
            return self.rename_edit(op);
        }
        if self.genui_editing() {
            return self.genui_edit(op);
        }
        if op == "pick_model" || op == "effort_next" {
            return self.chat_pick_key(op);
        }
        // An open picker menu takes the arrows and Return (chat/pick.rs).
        if let Some((_, _, sid)) = self.focused_chat()
            && self.chat_pick_nav(sid, op)
        {
            return;
        }
        // Escape closes the picker's menu, and what a cell floats over the transcript (a popover, a menu, a dialog),
        // before it interrupts.
        if op == "interrupt" && (self.chat_pick_close() || self.genui_escape()) {
            return;
        }
        let Some((_, _, sid)) = self.focused_chat() else { return };
        self.tree_focus = false;
        let (select, name) = match op.strip_prefix("select_") {
            Some(n) => (true, n),
            None => (false, op),
        };
        if let Some(m) = Move::named(name) {
            if let Some(to) = self.chat_target(m) {
                self.with_draft(|d| d.go(m, to, select));
            }
            return;
        }
        if let Some(m) = Move::erasing(op) {
            if let Some(to) = self.chat_target(m) {
                self.with_draft(|d| d.erase(to));
            }
            return;
        }
        match op {
            "newline" => _ = self.with_draft(|d| d.insert("\n")),
            // Return, or Command-Return (which submits a cell's TextArea).
            "send" | "submit" => self.chat_send_draft(sid),
            "interrupt" => {
                let busy = self.chats.borrow().get(&sid).is_some_and(|t| t.busy);
                if busy {
                    self.link.chat_interrupt(sid);
                } else {
                    self.with_draft(|d| d.anchor = d.caret);
                    if let Some(st) = self.chat.borrow_mut().get_mut(&sid) {
                        st.sel = None;
                    }
                }
            }
            _ => eprintln!("harness-native: keys: act.Edit has no edit {op:?}"),
        }
        self.poke();
    }

    /// Text typed (a key no binding took), committed by an input method or pasted, into the focused chat's input.
    /// Whether a chat took it.
    pub(super) fn chat_type(&mut self, text: &str) -> bool {
        // An open searchable menu (the models') has the keys: what is typed filters it.
        if let Some((_, _, sid)) = self.focused_chat()
            && self.chat_pick_type(sid, harness_native_ui_view::search::Edit::Type(text))
        {
            return true;
        }
        // A pasteboard's line ends (CRLF from Windows text, a lone CR from old Mac text) as the input's newlines.
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let typed: String = text.chars().filter(|c| *c == '\n' || *c == '\t' || (!c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(c))).collect();
        if let Some((_, _, sid)) = self.focused_chat()
            && let Some(st) = self.chat.borrow_mut().get_mut(&sid)
        {
            st.reading = false;
        }
        self.with_draft(|d| {
            d.marked = None;
            if !typed.is_empty() {
                d.insert(&typed);
            }
        })
    }

    /// Command-X in the focused chat: its input's selection taken out, for the pasteboard (None: nothing selected there;
    /// the transcript is read-only, so a selection in it is not cut).
    pub(super) fn chat_cut(&mut self) -> Option<String> {
        let mut cut = String::new();
        self.with_draft(|d| {
            cut = d.selected().to_owned();
            if !cut.is_empty() {
                d.insert("");
            }
        });
        (!cut.is_empty()).then_some(cut)
    }

    /// An input method's marked text on the focused chat's input. Whether a chat took it.
    pub(super) fn chat_set_marked(&mut self, text: Option<&str>) -> bool {
        self.with_draft(|d| d.marked = text.filter(|s| !s.is_empty()).map(str::to_owned))
    }

    pub(super) fn chat_marked(&self) -> Option<Option<String>> {
        if let Some(r) = &self.rename {
            return Some(r.marked());
        }
        if let Some(m) = self.genui_marked() {
            return Some(m);
        }
        let (_, _, sid) = self.focused_chat()?;
        Some(self.chat.borrow().get(&sid).and_then(|s| s.draft.marked.clone()))
    }

    /// What the focused chat has selected, for Command-C: its transcript's selection (a row taken whole as markdown),
    /// else its input's.
    pub(super) fn chat_copy(&self) -> Option<Option<(String, Copied)>> {
        if let Some(c) = self.rename_copy() {
            return Some(c);
        }
        if let Some((text, node)) = self.genui_copy() {
            return Some((!text.is_empty()).then_some((text, Copied::Field(node))));
        }
        let (_, _, sid) = self.focused_chat()?;
        let states = self.chat.borrow();
        let Some(st) = states.get(&sid) else { return Some(None) };
        if let Some((a, b)) = st.sel.filter(|(a, b)| a != b) {
            let chats = self.chats.borrow();
            let empty = Transcript::default();
            let rows = rows(chats.get(&sid).unwrap_or(&empty), Shows::of(st));
            let text = transcript_text(&rows, a.min(b), a.max(b));
            return Some((!text.is_empty()).then_some((text, Copied::Transcript)));
        }
        let picked = st.draft.selected();
        if !picked.is_empty() {
            return Some(Some((picked.to_owned(), Copied::Input)));
        }
        // A table row: the one under the pointer, else the first one opened, as a markdown table of its header and it.
        let chats = self.chats.borrow();
        let empty = Transcript::default();
        let rows = rows(chats.get(&sid).unwrap_or(&empty), Shows::of(st));
        let at = |key: &str, r: usize| {
            let t = rows.iter().find(|(k, _)| k == key).and_then(|(_, row)| row_table(row))?;
            (r < t.rows.len()).then(|| (t.row_markdown(r), Copied::TableRow(key.to_owned(), r)))
        };
        if let Some((key, r)) = self.hovered_table_row()
            && let Some(copied) = at(&key, r)
        {
            return Some(Some(copied));
        }
        Some(rows.iter().find_map(|(k, _)| st.rows_open.get(k).and_then(|&r| at(k, r))))
    }

    /// Pulse what a copy in the focused chat took (`what`, from [`Self::chat_copy`]): the transcript's selected words, the
    /// input's, or a table's row. Each gets a fresh `CopyFlash` of the selection's CopyLook.
    pub(super) fn chat_flash(&self, what: &Copied) {
        let pane = self.focused_chat().map(|c| c.0);
        if pane.is_none() && !matches!(what, Copied::Field(_)) {
            return;
        }
        let w = self.world.world_ref();
        let list = pane.and_then(|p| View::of(w.entity_from_id(p)).part("list")).and_then(|l| l.part("rows"));
        let mut hit: Vec<u64> = Vec::new(); // alloc: the few nodes one copy pulses
        match what {
            Copied::Input => hit.extend(pane.and_then(|p| input_text(w, p)).map(|t| t.id())),
            Copied::Field(node) => hit.push(*node),
            Copied::Transcript => {
                if let Some(list) = list {
                    let root = list.id();
                    w.new_query::<&TextEdit>().each_entity(|e, te| {
                        if te.anchor != te.caret && std::iter::successors(e.parent(), |n| n.parent()).take(16).any(|n| *n.id() == root) {
                            hit.push(*e.id());
                        }
                    });
                }
            }
            Copied::TableRow(key, r) => {
                if let Some(row) = list.and_then(|l| l.part(key)) {
                    hit.extend(view::table::row_node(row.entity(), *r).map(|n| *n.id()));
                }
            }
        }
        if hit.is_empty() {
            return;
        }
        let look = harness_native_render::select::Look::of(w).copy;
        let whole = matches!(what, Copied::TableRow(..));
        for id in hit {
            w.entity_from_id(id).set(CopyFlash { whole, ..CopyFlash::kicked(look.secs, look.rise) });
        }
    }

    /// The transcript row (its key) and table row under the pointer, when it is over a table in a chat.
    fn hovered_table_row(&self) -> Option<(String, usize)> {
        let w = self.world.world_ref();
        let t = w.entity_named(APP).target(Hovered::id(), 0)?;
        let (table, r) = view::table::row_under(t)?;
        let row = std::iter::successors(Some(table), |n| n.parent())
            .take(8)
            .find(|n| n.parent().is_some_and(|p| p.name() == "rows" && p.parent().is_some_and(|l| l.name() == "list")))?;
        Some((row.name().to_string(), r))
    }

    /// A press on a table row in a chat (`{sid}:{key}:{row}`): open it below as a record, or close it when it is open.
    pub(super) fn chat_row_toggle(&mut self, rest: &str) {
        let Some((head, r)) = rest.rsplit_once(':') else { return };
        let Some((sid, key)) = head.split_once(':') else { return };
        let (Ok(sid), Ok(r)) = (sid.parse::<u64>(), r.parse::<usize>()) else { return };
        let mut states = self.chat.borrow_mut();
        let open = &mut states.entry(sid).or_default().rows_open;
        if open.get(key) == Some(&r) {
            open.remove(key);
        } else {
            open.insert(key.to_owned(), r);
        }
        drop(states);
        self.poke();
    }

    /// A sideways scroll over a table: it takes it while its columns overhang its room. Whether one did.
    pub(super) fn table_wheel(&mut self, ev: &sidebar::Wheel, z: f32) -> bool {
        if ev.dx.abs() <= ev.dy.abs() {
            return false;
        }
        let w = self.world.world_ref();
        let Some(t) = w.entity_named(APP).target(Hovered::id(), 0) else { return false };
        let dx = if ev.precise { ev.dx / z } else { ev.dx * 12.0 };
        if view::table::scroll_sideways(t, -dx) {
            self.poke();
            return true;
        }
        false
    }

    /// Command-A in the focused chat: the whole transcript after a press in it, else the input's draft. Whether a chat
    /// took it.
    pub(super) fn chat_select_all(&mut self) -> bool {
        if self.renaming() {
            return self.with_draft(|d| (d.anchor, d.caret) = (0, d.text.len()));
        }
        let Some((_, _, sid)) = self.focused_chat() else { return false };
        let n = self.chat_rows(sid).len();
        let reading = self.chat.borrow().get(&sid).is_some_and(|st| st.reading);
        if reading {
            let mut states = self.chat.borrow_mut();
            let st = states.entry(sid).or_default();
            st.sel = (n > 0).then_some((Spot { row: 0, part: 0, at: 0 }, Spot { row: n, part: 0, at: 0 }));
            st.draft.anchor = st.draft.caret;
            drop(states);
            self.poke();
            return true;
        }
        self.with_draft(|d| {
            d.anchor = 0;
            d.caret = d.text.len();
        })
    }

    /// The focused chat's input laid out as drawn, where it is, and its session.
    fn input_layout(&mut self) -> Option<(Rc<Layout>, Laid, u64, u64)> {
        let (p, _, sid) = self.focused_chat()?;
        let w = self.world.world_ref();
        let txt = input_text(w, p)?;
        let (lay, laid) = words_layout(&mut self.paint.borrow_mut(), txt.entity())?;
        Some((lay, laid, p, sid))
    }

    /// Where `m` takes the focused chat's caret (a byte offset of its draft): along the source, or along the input's
    /// lines as they wrap (up and down keep their column's x, a line's start and end are the wrapped line's). None
    /// while there is no input laid out, or an input method holds marked text.
    fn chat_target(&mut self, m: Move) -> Option<usize> {
        let (_, _, sid) = self.focused_chat()?;
        let along = self.chat.borrow().get(&sid).map_or(Some(0), |st| st.draft.target(m));
        if along.is_some() {
            return along;
        }
        let (lay, _, _, sid) = self.input_layout()?;
        let mut states = self.chat.borrow_mut();
        along_lines(&mut states.entry(sid).or_default().draft, &lay, m)
    }

    /// Where the focused chat's caret is (window points, zoomed), for the input method's candidate window: the start of
    /// the marked text, else the caret ([`words_caret`]): a cell's field's while it has the keys, else the input's.
    pub(super) fn chat_caret_rect(&mut self) -> Option<Option<[f32; 4]>> {
        let z = self.zoom();
        let zoomed = |r: Option<[f32; 4]>| r.map(|r| r.map(|v| v * z));
        if let Some(node) = self.rename.as_ref().and_then(|r| self.rename_node(r.obj)) {
            let w = self.world.world_ref();
            return Some(zoomed(words_caret(&mut self.paint.borrow_mut(), w.entity_from_id(node))));
        }
        if let Some(r) = self.genui_caret() {
            return Some(zoomed(r));
        }
        let (p, ..) = self.focused_chat()?;
        let w = self.world.world_ref();
        let Some(t) = input_text(w, p) else { return Some(None) };
        Some(zoomed(words_caret(&mut self.paint.borrow_mut(), t.entity())))
    }

    /// The byte of the focused chat's draft nearest window point `(x, y)` on its input.
    fn input_index(&mut self, x: f32, y: f32) -> Option<usize> {
        let (lay, l, _, sid) = self.input_layout()?;
        let u = lay.index_at(x - l.x, y - l.y);
        self.chat.borrow().get(&sid).map(|st| byte_of(&st.draft.text, u))
    }

    /// The transcript place of chat pane `p` nearest window point `(x, y)`: in the words under it, else the start of the
    /// next words below, else the end of the last. None when no words are in view.
    fn transcript_spot(&mut self, p: u64, rows: &[(String, Row)], x: f32, y: f32) -> Option<Spot> {
        let w = self.world.world_ref();
        let stack = View::of(w.entity_from_id(p)).part("list")?.part("rows")?;
        let keys: HashMap<&str, usize> = rows.iter().enumerate().map(|(i, (k, _))| (k.as_str(), i)).collect();
        let mut found: Vec<(usize, View<'_>)> = Vec::new();
        stack.entity().each_child(|c| {
            if let Some(&i) = keys.get(c.name().as_str()) {
                found.push((i, View::of(c)));
            }
        });
        found.sort_by_key(|(i, _)| *i);
        let mut last = None;
        for (i, row) in found {
            if atomic(&rows[i].1) {
                let Some(l) = row.entity().try_cloned::<&Laid>() else { continue };
                if y < l.y + l.h {
                    return Some(Spot { row: i, part: 0, at: usize::from(y >= l.y + l.h * 0.5) });
                }
                last = Some(Spot { row: i, part: 0, at: 1 });
                continue;
            }
            for (k, (path, src, md)) in texts(&rows[i].1).iter().enumerate() {
                let Some(node) = path.iter().try_fold(row, |v, n| v.part(n)) else { continue };
                let Some(l) = node.entity().try_cloned::<&Laid>() else { continue };
                if y < l.y {
                    return Some(Spot { row: i, part: k, at: 0 });
                }
                if y < l.y + l.h {
                    let (lay, l) = words_layout(&mut self.paint.borrow_mut(), node.entity())?;
                    return Some(Spot { row: i, part: k, at: lay.index_at(x - l.x, y - l.y) });
                }
                last = Some(Spot { row: i, part: k, at: units(&shown(src, *md)) });
            }
        }
        last
    }

    /// What a press at `s` selects by `unit`: nothing (a place), its word, or its words whole.
    fn spot_span(rows: &[(String, Row)], s: Spot, unit: Unit) -> (Spot, Spot) {
        if rows.get(s.row).is_some_and(|(_, r)| atomic(r)) {
            let whole = (Spot { at: 0, ..s }, Spot { at: 1, ..s });
            return if unit == Unit::Cell { (s, s) } else { whole };
        }
        let text = rows.get(s.row).and_then(|(_, r)| texts(r).into_iter().nth(s.part)).map(|(_, src, md)| shown(&src, md));
        let Some(t) = text else { return (s, s) };
        match unit {
            Unit::Cell => (s, s),
            Unit::Word => {
                let (a, b) = word_around(&t, byte_of(&t, s.at));
                (Spot { at: units(&t[..a]), ..s }, Spot { at: units(&t[..b]), ..s })
            }
            Unit::Line => (Spot { at: 0, ..s }, Spot { at: units(&t), ..s }),
        }
    }

    /// A left press: in the focused chat's input it places the caret (a double press a word, a triple its line), in
    /// its transcript it starts a selection there; a drag extends either. A press on a cell's field is the field's
    /// ([`Self::genui_pointer_down`]). Whether a chat took it.
    pub(super) fn chat_pointer_down(&mut self, clicks: isize) -> bool {
        let took = self.genui_common_down();
        if self.genui_pointer_down(clicks) || took {
            return true;
        }
        let Some((p, _, sid)) = self.focused_chat() else { return false };
        let (x, y) = (self.pointer.x, self.pointer.y);
        let unit = Unit::of_clicks(clicks);
        let w = self.world.world_ref();
        let pv = View::of(w.entity_from_id(p));
        let over = |v: Option<View<'_>>| v.and_then(|v| v.entity().try_cloned::<&Laid>()).is_some_and(|l| l.contains(x, y));
        // On the composer's card but off its bar of controls is on the words (its insets too).
        let card = composer(pv);
        let in_input = over(card) && !over(card.and_then(|c| c.part("bar")));
        let in_list = over(pv.part("list"));
        // A press on a cell's control is the control's (genui/control.rs): it starts no selection in the transcript.
        let on_control = w.entity_named(APP).target(Hovered::id(), 0).is_some_and(|h| {
            std::iter::successors(Some(h), |n| n.parent())
                .take(6)
                .any(|n| n.try_cloned::<&Action>().is_some_and(|a| a.name.as_str().starts_with("ui:")))
        });
        if in_list && on_control {
            return true;
        }
        if in_input {
            let at = self.input_index(x, y);
            let mut states = self.chat.borrow_mut();
            let st = states.entry(sid).or_default();
            (st.sel, st.reading) = (None, false);
            if let Some(i) = at.filter(|_| st.draft.marked.is_none()) {
                let span = st.draft.press(i, unit);
                st.drag = Some(Drag::Input { unit, span });
            }
        } else if in_list {
            let rows = self.chat_rows(sid);
            let spot = self.transcript_spot(p, &rows, x, y);
            let mut states = self.chat.borrow_mut();
            let st = states.entry(sid).or_default();
            st.draft.anchor = st.draft.caret;
            (st.sel, st.reading) = (None, true);
            if let Some(s) = spot {
                let span = Self::spot_span(&rows, s, unit);
                st.sel = Some(span);
                st.drag = Some(Drag::List { unit, span });
            }
        } else {
            return false;
        }
        self.poke();
        true
    }

    /// The pointer moved with a chat's press held: its selection follows (the press's own span kept whole), a cell's
    /// field's as well ([`Self::genui_pointer_drag`]). Whether a chat is being dragged in.
    pub(super) fn chat_pointer_drag(&mut self) -> bool {
        if self.genui_common_drag() || self.genui_pointer_drag() {
            return true;
        }
        let held = self.chat.borrow().iter().find_map(|(sid, st)| st.drag.map(|d| (*sid, d)));
        let Some((sid, drag)) = held else { return false };
        let Some((p, ..)) = self.focused_chat().filter(|c| c.2 == sid) else {
            if let Some(st) = self.chat.borrow_mut().get_mut(&sid) {
                st.drag = None;
            }
            return false;
        };
        let (x, y) = (self.pointer.x, self.pointer.y);
        match drag {
            Drag::Input { unit, span } => {
                if let Some(i) = self.input_index(x, y) {
                    self.chat.borrow_mut().entry(sid).or_default().draft.drag_to(i, unit, span);
                }
            }
            Drag::List { unit, span: (a0, a1) } => {
                let rows = self.chat_rows(sid);
                if let Some(at) = self.transcript_spot(p, &rows, x, y) {
                    let s = Self::spot_span(&rows, at, unit);
                    let sel = if s.0 < a0 { (a1, s.0) } else { (a0, s.1) };
                    self.chat.borrow_mut().entry(sid).or_default().sel = Some(sel);
                }
            }
        }
        self.poke();
        true
    }

    /// The press let go: a drag ends (a cell's field's too), and a transcript selection that covers nothing goes. Whether
    /// a chat was dragged in.
    pub(super) fn chat_pointer_up(&mut self) -> bool {
        let mut any = self.genui_common_up() | self.genui_pointer_up();
        let mut pressed = None;
        for (sid, st) in self.chat.borrow_mut().iter_mut() {
            if let Some(drag) = st.drag.take() {
                any = true;
                if st.sel.is_some_and(|(a, b)| a == b) {
                    st.sel = None;
                    // A single press that selected nothing: a press on an inline component or a link there opens it.
                    if let Drag::List { unit: Unit::Cell, span: (s, _) } = drag {
                        pressed = Some((*sid, s));
                    }
                }
            }
        }
        if let Some((sid, s)) = pressed {
            self.chat_open_at(sid, s);
        }
        if any {
            self.poke();
        }
        any
    }

    /// Open what transcript place `s` of session `sid` is on: an inline component (`<Path loc="...">`, through the scene's
    /// `ui.inline`, relative paths from the session's directory) or a markdown link, in the app its kind opens in; an
    /// Option-press shows a file in the Finder. A component that cannot open says why in the log, naming it.
    fn chat_open_at(&mut self, sid: u64, s: Spot) {
        let rows = self.chat_rows(sid);
        let Some((_, src, true)) = rows.get(s.row).and_then(|(_, r)| texts(r).into_iter().nth(s.part)) else { return };
        let m = harness_native_render::markdown::inline(&src);
        let w = self.world.world_ref();
        let target = if let Some(t) = m.tags.iter().find(|t| (t.start..t.end).contains(&s.at)) {
            let home = std::env::var("HOME").unwrap_or_default();
            let cwd = self
                .focused_chat()
                .filter(|c| c.2 == sid)
                .and_then(|(_, e, _)| w.entity_from_id(e).try_cloned::<&model::Cwd>())
                .map(|c| c.s.as_str().to_owned())
                .unwrap_or_else(|| home.clone());
            match harness_native_render::inline::target(w, &t.tag, &cwd, &home) {
                Ok(target) => target,
                Err(why) => {
                    eprintln!("harness-native: chat {sid}: {why}");
                    return;
                }
            }
        } else if let Some(l) = m.links.iter().find(|l| (l.0..l.1).contains(&s.at)) {
            l.2.clone()
        } else {
            return;
        };
        crate::open::open(&target, self.press.1.alt);
    }

    /// Session `sid`'s transcript as rows.
    fn chat_rows(&self, sid: u64) -> Vec<(String, Row)> {
        let chats = self.chats.borrow();
        let empty = Transcript::default();
        let states = self.chat.borrow();
        let none = ChatState::default();
        rows(chats.get(&sid).unwrap_or(&empty), Shows::of(states.get(&sid).unwrap_or(&none)))
    }
}

/// Where `m` takes draft `d`'s caret along its lines as laid out in `lay` (up and down keep their column's x, a line's
/// start and end are the wrapped line's). None while an input method holds marked text.
fn along_lines(d: &mut Draft, lay: &Layout, m: Move) -> Option<usize> {
    if d.marked.is_some() {
        return None;
    }
    let n = units(&d.text);
    if lay.lines.is_empty() {
        return Some(if matches!(m, Move::Down | Move::LineEnd) { d.text.len() } else { 0 });
    }
    let (x, li) = lay.caret(units(&d.text[..d.caret]));
    let line = &lay.lines[li];
    let u = match m {
        Move::Up | Move::Down => {
            let goal = *d.goal.get_or_insert(x);
            let next = if m == Move::Up { li.checked_sub(1) } else { Some(li + 1).filter(|&i| i < lay.lines.len()) };
            match next {
                Some(i) => {
                    let l = &lay.lines[i];
                    lay.index_at(goal, l.top + l.height * 0.5)
                }
                None if m == Move::Up => 0,
                None => n,
            }
        }
        Move::LineStart => line.start,
        // Before the newline that ends it, or before the space a soft wrap broke at (after it is the next line's start).
        Move::LineEnd if line.hard_break => line.end.saturating_sub(1).max(line.start),
        Move::LineEnd if line.end < n => {
            let b = byte_of(&d.text, line.end);
            let space = d.text[..b].chars().next_back().is_some_and(char::is_whitespace);
            if space { line.end - 1 } else { line.end }
        }
        _ => line.end,
    };
    Some(byte_of(&d.text, u))
}

/// Where a transcript row is, for the parts that act on it: its session, its key, and the table row opened in it.
struct RowCtx<'k> {
    sid: u64,
    key: &'k str,
    open: Option<usize>,
    /// The app's cell controls in use, for a cell's UI (None: drawn from the tree alone).
    ui: Option<&'k std::cell::RefCell<GenLive>>,
    /// Its agent is at work: a cell's UI new to the transcript plays (genui's `playing`).
    playing: bool,
    /// The transcript's cells by tool call, for a cell's UI that shows another (a CellRef).
    cells: Option<&'k HashMap<String, Detail>>,
}

impl RowCtx<'static> {
    /// A row drawn outside the transcript (in a cell's generated UI): its tables' rows do not open.
    const NOWHERE: Self = RowCtx { sid: 0, key: "", open: None, ui: None, playing: false, cells: None };
}

impl RowCtx<'_> {
    /// What a press on a table row in this row sends (the row's number after it); None outside the transcript.
    fn row_action(&self) -> Option<String> {
        (!self.key.is_empty()).then(|| format!("chat:row:{}:{}", self.sid, self.key))
    }
}

/// A table's measure: words (in a style, inline markdown or not) to their one-line width in points.
type Measure<'m> = dyn FnMut(&str, &TextStyle, bool) -> f32 + 'm;

/// Fill one transcript row from what it shows.
fn fill_row(row: View<'_>, r: &Row, ctx: &RowCtx<'_>, measure: &mut Measure<'_>) {
    let text = |name: &str, s: &str| {
        if let Some(p) = row.part(name) {
            p.text(s);
        }
    };
    match r {
        Row::User(s) => {
            if let Some(t) = row.part("bubble").and_then(|b| b.part("text")) {
                t.text(s);
            }
        }
        Row::Prose(s) | Row::Heading(_, s) | Row::Quote(s) | Row::Note(s) | Row::Error(s) => text("text", s),
        Row::ListItem { depth, mark, text: s } => {
            text("mark", mark);
            text("text", s);
            if let Some(i) = row.part("indent") {
                let want = *depth as f32 * crate::look::width(row.entity().world(), "chat_list_nest");
                let e = i.entity();
                if e.try_cloned::<&Size>().is_none_or(|z| (z.w.value - want).abs() > 0.01) {
                    e.set(Size {
                        w: Sizing { kind: SizeKind::Fixed, value: want, min: 0.0, max: 0.0 },
                        h: Sizing { kind: SizeKind::Fixed, value: 1.0, min: 0.0, max: 0.0 },
                    });
                }
            }
        }
        Row::Code { lang, text: s } => {
            if let Some(t) = row.part("card").and_then(|c| c.part("text")) {
                t.text(s);
                let file = format!("code.{lang}");
                let e = t.entity();
                if !lang.is_empty() && harness_native_render::highlight::knows(&file) {
                    if e.try_cloned::<&Highlight>().is_none_or(|h| h.file.as_str() != file) {
                        e.set(Highlight { file: file.as_str().into() });
                    }
                } else if e.has(Highlight::id()) {
                    e.remove(Highlight::id());
                }
            }
        }
        Row::Tool { block, name, arg, state, shown, failed, input, result } => {
            if let Some(h) = row.part("head") {
                h.action(&format!("chat:open:{block}"));
                // A failure is one small dot in its detail, in the failed tone (hn.ChatToolFailed), never its words in red.
                let mark = if *failed { "\u{2022}" } else { "" };
                line(h, name, arg, mark, "", Some(*shown != Shown::Folded), (*state).map(|s| ("state", s)));
            }
            if let Some(c) = row.part("body").and_then(|b| b.part("card")) {
                if let Some(i) = c.part("input") {
                    i.text(input);
                }
                if let Some(o) = c.part("result") {
                    o.text(tool_result(result));
                }
            }
        }
        Row::Thinking { block, text: s, live, open } => {
            if let Some(h) = row.part("head") {
                h.action(&format!("chat:open:{block}"));
                line(h, "", "", "", "", Some(*open), Some(("glyph", if *live { "thinking" } else { "thought" })));
            }
            if let Some(t) = row.part("body").and_then(|b| b.part("text")) {
                t.text(s.trim());
            }
        }
        Row::Steps(l) => steps::fill_line(row, l),
        Row::Out(o) => steps::fill(row, o),
        Row::Grid { v, cap } => fill_grid(row, v, *cap, ctx, measure),
        Row::Table(t) => {
            if let Some(grid) = row.part("card").and_then(|c| c.part("grid")) {
                let action = ctx.row_action();
                let opts = view::table::Opts { look: "hn.ChatTable", shown: t.rows.len(), action: action.as_deref(), open: ctx.open, ..view::table::Opts::default() };
                view::table(grid, t, opts, measure);
            }
        }
        Row::Ui { v, cell, block, code } => {
            genui::fill_in(row, v, cell, ctx.cells, ctx.ui, ctx.playing, measure);
            // Its code: a quiet button in the gutter, shown while the view is hovered or its code is open.
            if let Some(b) = row.part("code_at").and_then(|z| z.part("code")).filter(|_| !block.is_empty()) {
                b.action(&format!("chat:open:{}", code_key(block)));
                tooltip(b, if *code { "Hide code" } else { "Show code" });
                if let Some(i) = b.part("icon") {
                    i.icon(CODE_ICON);
                    crate::look::hover_show(i.entity(), *row.entity().id(), *code);
                }
            }
        }
    }
}

/// A shown table, record or list (mcp/src/shown.ts) as a `ui.Table`'s data, Nushell's way: a table gets an index column
/// and a header, a record its keys (dim) beside its values, a list an index; each cell typed by its value and its column's
/// type (numbers, bools, urls, nulls, code-ish strings in the monospace face).
fn grid_table(v: &Value) -> view::table::Table {
    use view::table::{Align, Cell, Column, Kind};
    let kind = v["t"].as_str().unwrap_or("");
    let cols: Vec<(&str, &str)> =
        v["cols"].as_array().into_iter().flatten().map(|c| (c["name"].as_str().unwrap_or(""), c["type"].as_str().unwrap_or(""))).collect();
    let rows: &[Value] = v["rows"].as_array().map_or(&[], Vec::as_slice);
    let index = kind != "record";
    let mut columns = Vec::new();
    if index {
        columns.push(Column { name: "#".into(), align: Align::End });
    }
    columns.extend(cols.iter().map(|(name, _)| Column { name: (*name).to_owned(), align: Align::Auto }));
    let rows = rows
        .iter()
        .enumerate()
        .map(|(r, row)| {
            let mut out = Vec::with_capacity(columns.len());
            if index {
                out.push(Cell::new(Kind::Dim, r.to_string()));
            }
            for (c, (_, ty)) in cols.iter().enumerate() {
                let x = &row[c];
                out.push(if kind == "record" && c == 0 { Cell::new(Kind::Dim, cell_text(x)) } else { value_cell(x, ty) });
            }
            out
        })
        .collect();
    view::table::Table { cols: columns, rows, head: kind == "table" }
}

/// The table a transcript row shows, as `ui.Table` holds it.
fn row_table(r: &Row) -> Option<view::table::Table> {
    match r {
        Row::Table(t) => Some((**t).clone()),
        Row::Grid { v, .. } => Some(grid_table(v)),
        _ => None,
    }
}

/// A JSON value as a typed table cell, `ty` its column's type (mcp/src/shown.ts; "" when none): a null dim, a bool a
/// mark, a number or a number column's string at the end, a web link its host, a date, nested value or code-ish string in
/// the code face, else words.
fn value_cell(x: &Value, ty: &str) -> view::table::Cell {
    use view::table::{Cell, Kind};
    let url = |s: &str| (s.starts_with("https://") || s.starts_with("http://")) && !s.contains(char::is_whitespace);
    match x {
        Value::Null => Cell::new(Kind::Dim, "null"),
        Value::Bool(b) => Cell::new(Kind::Bool, b.to_string()),
        Value::Number(n) => Cell::new(Kind::Number, n.to_string()),
        Value::String(s) if url(s) => Cell::new(Kind::Url, s.as_str()),
        Value::String(s) if ty == "number" => Cell::new(Kind::Number, s.as_str()),
        Value::String(s) if ty == "nested" || ty == "date" || codeish(s) => Cell::new(Kind::Code, s.as_str()),
        Value::String(s) => Cell::new(Kind::Text, s.as_str()),
        x => Cell::new(Kind::Code, x.to_string()),
    }
}

/// Whether a string reads as code rather than words: one token (no spaces) with a path, id or symbol's marks in it, or
/// letters and digits mixed (`r12`, `src/app.rs`, `snake_case`, `a1b2c3`).
fn codeish(s: &str) -> bool {
    if s.chars().count() < 2 || s.contains(char::is_whitespace) {
        return false;
    }
    let marks = s.contains(['/', '_', ':', '=', '#', '@', '(', '{', '[', '<', '\\']);
    let digit = s.contains(|c: char| c.is_ascii_digit());
    let letter = s.contains(char::is_alphabetic);
    marks || (digit && letter)
}

/// Set a foot's words, folding it away when it has none.
fn foot(f: View<'_>, s: &str) {
    f.text(s);
    let want = s.is_empty().then_some(Size { w: Sizing::GROW, h: Sizing::fixed(0.0) });
    if f.entity().try_cloned::<&Size>() != want {
        match want {
            Some(z) => {
                f.set(z);
            }
            None => {
                f.entity().remove(Size::id());
            }
        }
    }
}

/// A shown table, record or list (mcp/src/shown.ts) in a `ui.Table` ([`grid_table`]): its first `cap` rows, a press on a
/// row of a table or list opening it below as a record. A foot says how many rows there are when not all show.
fn fill_grid(row: View<'_>, v: &Value, cap: usize, ctx: &RowCtx<'_>, measure: &mut Measure<'_>) {
    let kind = v["t"].as_str().unwrap_or("");
    let data = grid_table(v);
    let total = v["total"].as_u64().map_or(data.rows.len(), |t| t as usize);
    let shown = data.rows.len().min(cap);
    if let Some(grid) = row.part("card").and_then(|c| c.part("grid")) {
        let action = ctx.row_action().filter(|_| kind != "record");
        let opts = view::table::Opts { look: "hn.ChatTable", shown, action: action.as_deref(), open: ctx.open, ..view::table::Opts::default() };
        view::table(grid, &data, opts, measure);
    }
    if let Some(f) = row.part("foot") {
        let unit = if kind == "record" { "fields" } else if kind == "list" { "items" } else { "rows" };
        foot(f, &if shown < total { format!("{shown} of {total} {unit}") } else { String::new() });
    }
}

/// Mark every word under `e` selected whole (`on`) or not, so the selection plate covers an atomic row the transcript's
/// selection takes and a copy's pulse plays on it.
fn select_all_words(e: EntityView<'_>, on: bool) {
    let mut todo = vec![e]; // alloc: the walk of one row's nodes
    while let Some(n) = todo.pop() {
        if let Some(t) = n.try_cloned::<&Text>() {
            let len = units(&shown(t.s.as_str(), n.has(MarkdownText::id())));
            match on && len > 0 {
                true => {
                    let te = TextEdit { anchor: 0, caret: len as u32, ..TextEdit::default() };
                    if n.try_cloned::<&TextEdit>() != Some(te) {
                        n.set(te);
                    }
                }
                false if n.has(TextEdit::id()) => _ = n.remove(TextEdit::id()),
                false => {}
            }
        }
        n.each_child(|c| todo.push(c));
    }
}

/// Fill one line (hn.ChatLine: a call's, a run's, a step's, a thought's): its words, argument in the code face, detail,
/// its hover-only chevron turned to `open` (None: a step, which opens nothing), and `status` (the line part that shows it,
/// `glyph` leading or `state` trailing, and its `hn.chat_status` name; the other part takes no room). On hover its
/// tooltip: the status's words, then `tip` (how long it took; none when both are empty).
fn line(h: View<'_>, words: &str, arg: &str, detail: &str, tip: &str, open: Option<bool>, status: Option<(&str, &str)>) {
    if let (Some(p), Some(open)) = (h.part("chevron"), open) {
        p.icon(crate::look::CHEVRON);
        crate::look::disclose(p.entity(), open);
    }
    for (part, s) in [("words", words), ("detail", detail)] {
        if let Some(p) = h.part(part) {
            p.text(s);
        }
    }
    // An argument with nothing to say takes no room, the room after it neither.
    if let Some(p) = h.part("arg") {
        p.text(arg);
        room(p, "arg", arg.is_empty());
    }
    // A status icon where one says it, nothing (and no room) where none does.
    let mut said = String::new();
    for part in ["glyph", "state"] {
        if let Some(p) = h.part(part) {
            let name = status.filter(|(at, _)| *at == part).map(|(_, name)| name);
            let words = status_on(p, name);
            room(p, part, name.is_none());
            if !words.is_empty() {
                said = words;
            }
        }
    }
    match (said.is_empty(), tip.is_empty()) {
        (true, _) => tooltip(h, tip),
        (false, true) => tooltip(h, &said),
        (false, false) => tooltip(h, &format!("{said} \u{00B7} {tip}")),
    }
}

/// Fold line part `p` (its `part` in hn.ChatLineLook) to nothing (`gone`): no width, no room after it; or give it back
/// the prefab's own size (none: its natural width) and the room after it. Written only on change.
fn room(p: View<'_>, part: &str, gone: bool) {
    let e = p.entity();
    if gone == e.try_cloned::<&Size>().is_some_and(|z| z.w == Sizing::fixed(0.0)) {
        return;
    }
    if gone {
        p.set(Size { w: Sizing::fixed(0.0), h: Sizing::FIT });
        p.set(harness_native_ui_layout_api::Padding::default());
        return;
    }
    let look = view::prefab(e.world(), "hn.ChatLineLook")
        .and_then(|l| View::of(l).part(part))
        .unwrap_or_else(|| panic!("the scene's hn.ChatLineLook has no {part}"));
    match look.entity().try_cloned::<&Size>() {
        Some(z) => {
            p.set(z);
        }
        None => {
            e.remove(Size::id());
        }
    }
    p.set(look.entity().try_cloned::<&harness_native_ui_layout_api::Padding>().unwrap_or_default());
}

/// Show chat status `name` (`hn.chat_status.<name>`, scene/widgets/chat_status.flecs) on icon node `v`: its glyph (or
/// its drawing, a Vector in its Stroke), its tone and, for one that breathes, the running dot's breath (src/look.rs) while it shows, so nothing moves once the agent
/// is done; `None` shows nothing. Returns its words (the tooltip of whatever shows it), empty for none. Written only on
/// change. The scene is compiled in, so a status it lacks is a bug.
fn status_on(v: View<'_>, name: Option<&str>) -> String {
    use crate::look::Breath;
    use harness_native_ui_look_api::Stroke;
    use harness_native_ui_text_api::{Icon, Vector};
    let e = v.entity();
    let Some(name) = name else {
        v.icon("");
        if e.has(Breath::id()) {
            e.remove(Breath::id());
        }
        if e.has(Vector::id()) {
            e.remove(Vector::id());
        }
        return String::new();
    };
    let s = view::prefab(e.world(), &format!("hn.chat_status.{name}"))
        .unwrap_or_else(|| panic!("the scene has no hn.chat_status.{name} (scene/widgets/chat_status.flecs)"));
    // Its glyph, or its drawing (a Vector in its Stroke: the draw pass paints an Icon first, so the Icon goes).
    match (s.try_cloned::<&Icon>(), s.try_cloned::<&Vector>()) {
        (_, Some(vec)) => {
            if e.has(Icon::id()) {
                e.remove(Icon::id());
            }
            let stroke = s.try_cloned::<&Stroke>().unwrap_or_else(|| panic!("hn.chat_status.{name} has a Vector but no Stroke"));
            if e.try_cloned::<&Stroke>() != Some(stroke) {
                v.set(stroke);
            }
            if e.try_cloned::<&Vector>().as_ref() != Some(&vec) {
                v.set(vec);
            }
        }
        (Some(glyph), None) => {
            if e.has(Vector::id()) {
                e.remove(Vector::id());
            }
            v.icon(glyph.glyph.as_str());
        }
        (None, None) => panic!("hn.chat_status.{name} has neither an Icon nor a Vector"),
    }
    let fill = s.try_cloned::<&Fill>().unwrap_or_else(|| panic!("hn.chat_status.{name} has no Fill"));
    if e.try_cloned::<&Fill>() != Some(fill) {
        v.set(fill);
    }
    match (s.try_cloned::<&Breath>(), e.has(Breath::id())) {
        (Some(b), false) => {
            v.set(b);
        }
        (None, true) => {
            e.remove(Breath::id());
        }
        _ => {}
    }
    s.try_cloned::<&Tooltip>().map(|t| t.text.as_str().to_owned()).unwrap_or_default()
}

/// Set node `v`'s tooltip to `s`, or take it away when `s` is empty (written only on change).
fn tooltip(v: View<'_>, s: &str) {
    let had = v.entity().try_cloned::<&Tooltip>();
    if s.is_empty() {
        if had.is_some() {
            v.entity().remove(Tooltip::id());
        }
    } else if had.is_none_or(|t| t.text.as_str() != s) {
        v.set(Tooltip { text: s.into() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What of a block still streaming may show: whole words, never a bold run or code span still open, never a line that
    /// so far only opens a list item, a heading or a fence; all of it once done.
    #[test]
    fn streaming_words_show_only_what_more_cannot_change() {
        fn at(t: &str) -> &str {
            &t[..settled_len(t, false)]
        }
        assert_eq!(at("The quick bro"), "The quick ");
        assert_eq!(at("see **the bo"), "see ");
        assert_eq!(at("see **the** bo"), "see **the** ");
        assert_eq!(at("run `cargo te"), "run ");
        assert_eq!(at("Done.\n\n- "), "Done.\n\n");
        assert_eq!(at("Done.\n12. "), "Done.\n");
        assert_eq!(at("Done.\n```rus"), "Done.\n");
        assert_eq!(at("```\nlet a = **b "), "```\nlet a = **b ");
        assert_eq!(settled_len("half a wo", true), 9);
    }

    /// A run is one folded line and nothing under it, its table and its failure included, until the person opens it; its
    /// words say what failed; opened, its cells' outputs show (the table as a one-line preview, folded); a cell that drew
    /// is its view, no line over it.
    #[test]
    fn runs_fold_to_one_line_and_open_only_when_asked() {
        let cell = |id: &str, exec: Value, value: Value, ui: Value| {
            vec![
                serde_json::json!({"ev": "tool_call", "block": id, "name": "ts", "input": {"code": "x"}}),
                serde_json::json!({"ev": "tool_result", "block": id, "text": "", "error": false}),
                serde_json::json!({"ev": "tool_detail", "block": id, "status": "done", "ms": 3, "code": "x",
                    "effects": [exec], "value": value, "ui": ui}),
            ]
        };
        let exec = |code: u8, out: &str| {
            serde_json::json!({"kind": "exec", "done": "Ran x", "status": if code == 0 { "ok" } else { "error" },
                "detail": format!("exit {code}"), "value": {"t": "text", "v": out}})
        };
        let table = serde_json::json!({"t": "table", "total": 1, "rows": [["abc", "t"]],
            "cols": [{"name": "hash", "type": "string"}, {"name": "author", "type": "string"}]});
        let view = serde_json::json!({"type": "Text", "props": {}, "children": ["hi"]});
        let mut lines = vec![serde_json::json!({"ev": "user", "text": "go"})];
        lines.extend(cell("a", exec(1, "boom"), Value::Null, Value::Null));
        lines.extend(cell("b", exec(0, "abc|t"), table, Value::Null));
        lines.extend(cell("c", exec(0, "ok"), Value::Null, Value::Null));
        lines.extend(cell("d", serde_json::json!({"kind": "exec", "done": "Ran y", "status": "ok", "detail": "exit 0",
            "value": {"t": "text", "v": ""}}), Value::Null, view));
        lines.push(serde_json::json!({"ev": "block", "block": "m", "kind": "text", "text": "Done."}));
        lines.push(serde_json::json!({"ev": "turn_end", "ok": true, "stop": "end_turn"}));
        let mut t = Transcript::default();
        for l in &lines {
            t.apply(&harness_proto::event::Event::from_json(l).unwrap());
        }
        let (fold, cut) = (HashMap::new(), HashMap::new());
        let (rows, nested) = rows_nested(&t, Shows { fold: &fold, cut: &cut });
        let kinds: Vec<(&str, &str)> = rows.iter().map(|(k, r)| (k.as_str(), r.prefab())).collect();
        assert_eq!(kinds[1].1, "hn.StepLineFailed", "{kinds:?}");
        assert!(matches!(&rows[1].1, Row::Steps(l) if !l.open && !l.why.is_empty() && l.words.contains("couldn't")), "{kinds:?}");
        // Nothing under the folded run: no failure, no table, no output.
        assert!(nested.is_empty(), "{kinds:?}");
        assert!(matches!(&rows[2].1, Row::Ui { block, code: false, .. } if block == "d"), "{kinds:?}");
        assert_eq!(rows.len(), 4, "user, run, view, prose: {kinds:?}");
        // Opened, its cells' outputs show; the table only as its folded preview.
        let key = rows[1].0.clone();
        let opened = super::rows(&t, Shows { fold: &HashMap::from([(key, true)]), cut: &cut });
        let shown: Vec<&str> = opened.iter().map(|(_, r)| r.prefab()).collect();
        assert!(opened.iter().any(|(_, r)| matches!(r, Row::Out(steps::Out::Value { open: false, .. }))), "{shown:?}");
        assert!(!opened.iter().any(|(_, r)| matches!(r, Row::Grid { .. })), "{shown:?}");
        // The view's code button shows the cell's code under the view.
        let code = HashMap::from([(code_key("d"), true)]);
        let cut = HashMap::new();
        let shown = super::rows(&t, Shows { fold: &code, cut: &cut });
        assert!(matches!(&shown[3].1, Row::Code { lang, .. } if lang == "ts"), "{:?}", shown.iter().map(|(_, r)| r.prefab()).collect::<Vec<_>>());
    }

    #[test]
    fn markdown_splits_into_rows() {
        let mut out = Vec::new();
        blocks("# Title\n\nSome *words*\nwrapped.\n\n- one\n  more\n- two\n\n```ts\nconst x = 1;\n```\n> quoted", false, &mut out);
        let kinds: Vec<&str> = out.iter().map(Row::prefab).collect();
        assert_eq!(kinds, ["hn.ChatH1", "hn.ChatProse", "hn.ChatItem", "hn.ChatItem", "hn.ChatCode", "hn.ChatQuote"]);
        assert!(matches!(&out[1], Row::Prose(s) if s == "Some *words*\nwrapped."));
        assert!(matches!(&out[2], Row::ListItem { text, .. } if text == "one\nmore"));
        // An ordered item and a task take their own markers' prefabs (a number and a box at the words' size).
        let mut out = Vec::new();
        blocks("1. first
2. second
- [ ] todo
- [x] done", false, &mut out);
        let kinds: Vec<&str> = out.iter().map(Row::prefab).collect();
        assert_eq!(kinds, ["hn.ChatItemNumber", "hn.ChatItemNumber", "hn.ChatItemTask", "hn.ChatItemTask"]);
    }

    /// A markdown table becomes one table row: typed cells, its alignment, numbers at the end; while it streams, its last
    /// line (maybe cut) waits, and a table still on its header shows as nothing yet.
    #[test]
    fn markdown_tables_become_tables() {
        use view::table::{Align, Kind};
        let src = "Here:\n\n| name | `a|b` | n |\n|:-|:-:|-|\n| **x** | y \\| z | 12 |\n| [w](https://w.io) | | 3.5 |\n\nafter";
        let mut out = Vec::new();
        blocks(src, false, &mut out);
        let kinds: Vec<&str> = out.iter().map(Row::prefab).collect();
        assert_eq!(kinds, ["hn.ChatProse", "hn.ChatTable", "hn.ChatProse"]);
        let Row::Table(t) = &out[1] else { panic!("not a table: {:?}", out[1].prefab()) };
        assert_eq!(t.cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["name", "a|b", "n"]);
        assert_eq!(t.cols.iter().map(|c| c.align).collect::<Vec<_>>(), [Align::Start, Align::Center, Align::Auto]);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.cell(0, 2).kind, Kind::Number);
        assert_eq!(t.align(2), Align::End);
        assert_eq!(t.row_markdown(0), "| name | a\\|b | n |\n| :-- | :-: | --- |\n| **x** | y \\| z | 12 |");

        // Streaming: the cut last line waits; a header alone shows nothing.
        let mut live = Vec::new();
        blocks("| a | b |\n|---|---|\n| 1 | 2 |\n| 3", true, &mut live);
        let Some(Row::Table(t)) = live.first() else { panic!("no table while streaming") };
        assert_eq!(t.rows.len(), 1);
        let mut head = Vec::new();
        blocks("Hi\n\n| a | b |", true, &mut head);
        assert_eq!(head.iter().map(Row::prefab).collect::<Vec<_>>(), ["hn.ChatProse"]);
    }

    #[test]
    fn draft_edits_like_a_mac_text_field() {
        let step = |d: &mut Draft, m: Move, select: bool| {
            let to = d.target(m).unwrap();
            d.go(m, to, select);
        };
        let mut d = Draft::default();
        d.insert("hello world");
        step(&mut d, Move::WordLeft, false);
        assert_eq!(d.caret, 6);
        step(&mut d, Move::WordRight, true);
        assert_eq!(d.selected(), "world");
        d.insert("there");
        assert_eq!(d.text, "hello there");
        d.erase(d.target(Move::WordLeft).unwrap());
        assert_eq!(d.text, "hello ");
        assert_eq!(d.span(2, Unit::Word), (0, 5));
        assert_eq!(word_around("a \u{e9}t\u{e9} b", 4), (2, 7));
    }

    /// A copy across the transcript takes the rows it covers whole as markdown, and the cut ones as they show (the
    /// markdown markers out, counted in UTF-16).
    #[test]
    fn a_transcript_copy_is_markdown_whole_and_shown_cut() {
        let rows: Vec<(String, Row)> = [
            Row::Prose("say **h\u{e9}llo** there".into()),
            Row::ListItem { depth: 0, mark: "\u{2022}".into(), text: "one".into() },
            Row::ListItem { depth: 0, mark: "\u{2022}".into(), text: "two".into() },
            Row::Code { lang: "rs".into(), text: "fn main() {}".into() },
        ]
        .into_iter()
        .enumerate()
        .map(|(i, r)| (format!("i{i}"), r))
        .collect();
        let at = |row, part, at| Spot { row, part, at };
        assert_eq!(transcript_text(&rows, at(0, 0, 4), at(2, 1, 3)), "h\u{e9}llo there\n\n- one\n- two");
        assert_eq!(transcript_text(&rows, at(1, 0, 0), at(3, 0, 2)), "- one\n- two\n\nfn");
        assert_eq!(transcript_text(&rows, at(3, 0, 0), at(3, 0, 12)), "```rs\nfn main() {}\n```");
    }
}
