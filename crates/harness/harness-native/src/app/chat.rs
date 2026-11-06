//! The chat pane (`hn.ChatPane`, scene/widgets/chat.flecs): a chat session (the daemon's chat mode: an agent over
//! structured events, not a terminal) drawn as a chat. The transcript comes from the model's [`model::chat::Transcript`]
//! (agent-neutral events); each of its items becomes rows (a user message, the agent's markdown split into paragraphs,
//! headings, list items, quotes and code blocks, thinking and tool calls folded to a line, a turn's foot) in a
//! `ui.LazyList`, so only the rows in view are entities. It scrolls on AppKit's physics and sticks to the bottom while
//! the agent writes unless the person scrolled up. The input is a `ui.TextArea` edited here (`Draft`, keys in
//! scene/20-keys.flecs ctx.Chat): Return sends, Shift+Return breaks the line, Escape interrupts.
use super::*;
use harness_native_ui_motion_api::CopyFlash;
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

mod genui;
mod pick;
pub(in crate::app) use pick::kind_title;

pub(super) use genui::Live as GenLive;
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
/// open or folded, over the default [`Grouped::default_open`] gives), the input, its scroll.
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
    /// Kip on the composer ([`App::chat_kip`]): this pane saw its agent at work, and how the turn it ended ended (ok or
    /// not) and when (the clock), while he still shows it.
    kip_busy: bool,
    kip_end: Option<(bool, f32)>,
    /// The model and effort picker's menu open under the composer (chat/pick.rs), and which; its highlight.
    pick: Option<pick::Pick>,
    pick_nav: pick::PickNav,
}

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
            kip_busy: false,
            kip_end: None,
            pick: None,
            pick_nav: pick::PickNav::default(),
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
    /// An agent's own tool call: its line (the tool's name, its main argument, how it is going: running,
    /// no result) and, opened, its input and result.
    Tool { block: String, name: String, arg: String, state: String, shown: Shown, failed: bool, input: String, result: String },
    /// A harness REPL cell's line (`open`: shown past its line): what it did in words, a detail ("5 results"), and how long it took as
    /// the line's tooltip ([`line`]).
    Card { block: String, words: String, detail: String, tip: String, error: bool, open: bool },
    /// A run of tool calls folded to one line ([`Run::summary`]): how many failed, whether one still runs, whether it is open.
    Run { key: String, words: String, failed: usize, running: bool, open: bool },
    /// One step of an opened card that took several.
    Step { words: String, detail: String, error: bool },
    /// A table, record or list (the kernel's shown value), its first `cap` rows.
    Grid { v: Arc<Value>, cap: usize },
    /// A markdown table in the agent's words.
    Table(Rc<view::table::Table>),
    /// Search results a cell gave (a value the kernel tags `kind: "search"`), their first `cap`.
    Results { v: Arc<Value>, cap: usize },
    /// Text a cell printed or returned: a command's output, a string, an error's stack.
    Output { text: String, error: bool },
    /// What a cell draws (its Svelte markup's tree, [`genui`]), the cell (`<repl>.<task>`) its events name, and the call
    /// (`block`) whose code its hover-only code button shows (`code`: shown now).
    Ui { v: Arc<Value>, cell: String, block: String, code: bool },
    /// "Show code" under an opened card.
    CodeToggle { block: String, open: bool },
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
            Row::ListItem { .. } => "hn.ChatItem",
            Row::Quote(_) => "hn.ChatQuote",
            Row::Code { .. } => "hn.ChatCode",
            Row::Tool { shown: Shown::Full, failed: true, .. } => "hn.ChatToolOpenFailed",
            Row::Tool { shown: Shown::Full, .. } => "hn.ChatToolOpen",
            Row::Tool { failed: true, .. } => "hn.ChatToolFailed",
            Row::Tool { .. } => "hn.ChatTool",
            Row::Run { failed: 1.., .. } => "hn.ChatRunFailed",
            Row::Run { .. } => "hn.ChatRun",
            Row::Card { error: true, .. } => "hn.ChatCardFailed",
            Row::Card { .. } => "hn.ChatCard",
            Row::Step { error: true, .. } => "hn.ChatStepFailed",
            Row::Step { .. } => "hn.ChatStep",
            Row::Grid { .. } => "hn.ChatGrid",
            Row::Table(_) => "hn.ChatTable",
            Row::Results { .. } => "hn.ChatResults",
            Row::Output { error: true, .. } => "hn.ChatOutputError",
            Row::Output { .. } => "hn.ChatOutput",
            Row::Ui { .. } => "hn.ChatUi",
            Row::CodeToggle { .. } => "hn.ChatCodeToggle",
            Row::Thinking { open: true, .. } => "hn.ChatThinkingOpen",
            Row::Thinking { .. } => "hn.ChatThinking",
            Row::Note(_) => "hn.ChatNote",
            Row::Error(_) => "hn.ChatError",
        }
    }
}

/// Markdown split into the rows it is drawn as: paragraphs (soft breaks joined), headings, list items (continuation lines
/// joined), quotes, fenced code (an unclosed fence, still streaming, runs to the end) and GitHub tables (lines of pipes
/// that are no table, as code). While it is `live` (still streaming) a table's row arrives whole: a last row not yet ended
/// is held back, and so is a head whose delimiter line has not come.
fn blocks(md: &str, live: bool, out: &mut Vec<Row>) {
    let lines: Vec<&str> = md.lines().collect();
    let mut para: Vec<&str> = Vec::new();
    let flush = |para: &mut Vec<&str>, out: &mut Vec<Row>| {
        if !para.is_empty() {
            out.push(Row::Prose(para.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ")));
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
            let mut text = vec![q.trim()];
            i += 1;
            while i < lines.len() && lines[i].trim_start().starts_with('>') {
                text.push(lines[i].trim_start()[1..].trim());
                i += 1;
            }
            out.push(Row::Quote(text.join(" ")));
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
            let mut text = vec![rest.trim()];
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
            out.push(Row::ListItem { depth, mark, text: text.join(" ") });
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

/// Rows or lines a call opened by default (the last shell call) previews of its output.
const PREVIEW_ROWS: usize = 6;

/// How much of a tool call shows: its line alone (every call at first, a failure too); its line and a short preview of its
/// output (the last shell call, the one call the default opens); or all of it (a call the person opened).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    Folded,
    Preview,
    Full,
}

impl Shown {
    /// Call `block` given the folds set (`fold`) and whether it opens by default.
    fn of(fold: &HashMap<String, bool>, block: &str, default_open: bool) -> Self {
        match fold.get(block) {
            Some(true) => Shown::Full,
            Some(false) => Shown::Folded,
            None if default_open => Shown::Preview,
            None => Shown::Folded,
        }
    }
}

/// A step's detail as its line shows it: a command that exited 0 says nothing more.
fn step_detail(d: &str) -> String {
    if d == "exit 0" { String::new() } else { d.to_owned() }
}

/// A drawn view's code button's glyph (cod-code).
const CODE_ICON: &str = "\u{EAC4}";

/// A line's words with the dot that parts them from its detail ("Ran 2 cells · 2 failed"), when it has one.
fn dotted(words: String, detail: &str) -> String {
    if detail.is_empty() || words.is_empty() { words } else { format!("{words} \u{00B7}") }
}

fn secs(ms: u64) -> String {
    if ms >= 60_000 { format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000) } else { format!("{:.1}s", ms as f64 / 1000.0) }
}

fn first_line(s: &str) -> String {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_owned()
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

/// A card's head: the step running now, else the one step it took, else how many of one kind, else its value.
fn headline(d: &Detail) -> (String, String, bool) {
    if let Some(e) = d.effects.iter().rev().find(|e| e.status == "running") {
        return (format!("{}\u{2026}", e.doing), String::new(), false);
    }
    if d.status == "error" {
        let why = d.value["message"].as_str().map_or_else(|| "failed".to_owned(), first_line);
        // One sign of the failure: its words in the failed tone (hn.ChatLineFailed).
        return (why, String::new(), true);
    }
    let failed = d.effects.iter().any(|e| e.status == "error");
    match d.effects.as_slice() {
        [] if d.status == "running" => ("Running\u{2026}".into(), String::new(), false),
        [] => {
            let v = &d.value;
            let n = v["total"].as_u64().unwrap_or(0);
            let words = match v["t"].as_str().unwrap_or("") {
                "table" => format!("{n} row{}", if n == 1 { "" } else { "s" }),
                "list" => format!("{n} item{}", if n == 1 { "" } else { "s" }),
                "record" => format!("{n} field{}", if n == 1 { "" } else { "s" }),
                "scalar" => cell_text(&v["v"]),
                "text" => first_line(v["v"].as_str().unwrap_or("")),
                _ if !empty_value(&d.ui) => "Drew a view".into(),
                _ => "Done".into(),
            };
            (words, String::new(), false)
        }
        [e] => (e.done.clone(), step_detail(&e.detail), failed),
        [e, rest @ ..] if rest.iter().all(|r| r.kind == e.kind) => (group::summary(d.effects.iter().map(|e| Act::of_effect(&e.kind))), String::new(), failed),
        // Steps of more than one kind: what they did, counted by kind ("Read 3 files, searched 2 times"), as a run sums its calls.
        [..] => (group::summary(d.effects.iter().map(|e| Act::of_effect(&e.kind))), String::new(), failed),
    }
}

/// The rows a shown value is drawn as: a grid, or output text (`cap` rows or lines of either).
fn value_rows(v: &Arc<Value>, cap: usize, out: &mut Vec<Row>) {
    // An empty value (no rows, blank text: a handle with nothing to show) draws no body, only the card's head.
    if empty_value(v) {
        return;
    }
    if v["kind"] == "search" {
        out.push(Row::Results { v: v.clone(), cap });
        return;
    }
    match v["t"].as_str().unwrap_or("") {
        "table" | "record" | "list" => out.push(Row::Grid { v: v.clone(), cap }),
        "text" => out.push(Row::Output { text: head_lines(v["v"].as_str().unwrap_or(""), cap), error: false }),
        "error" => {
            let stack = v["stack"].as_str().filter(|s| !s.is_empty()).or_else(|| v["message"].as_str()).unwrap_or("");
            out.push(Row::Output { text: head_lines(stack, cap), error: true });
        }
        _ => {}
    }
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
            _ => true,
        },
    }
}

/// A cell's result when it is the turn's content rather than plumbing: a table, records or a list (what a person asked
/// to see), never a command's stdout or a scalar its head already says. It shows under the call however it is folded.
fn result_value(d: &Detail) -> Option<&Arc<Value>> {
    Some(&d.value).filter(|v| matches!(v["t"].as_str(), Some("table" | "record" | "list")) && !empty_value(v))
}

/// The value a previewed card shows under its head: the cell's own, else its last step's; none when it draws (the view is
/// its preview) or its whole value is its head ("42").
fn preview_value(d: &Detail) -> Option<&Arc<Value>> {
    if !empty_value(&d.ui) {
        return None;
    }
    let main = if empty_value(&d.value) { d.effects.iter().rev().map(|e| &e.value).find(|v| !empty_value(v)) } else { Some(&d.value) };
    main.filter(|m| !(d.effects.is_empty() && m["t"] == "scalar"))
}

/// Whether call item `n` opens by default: it would ([`Grouped::default_open`]) and its preview has something to show.
fn opens(t: &Transcript, g: &Grouped, n: usize, c: &group::Call) -> bool {
    if !g.default_open(n, c) {
        return false;
    }
    match t.items.get(n) {
        Some(Item::Tool { block, result, .. }) => match t.details.get(block) {
            Some(d) => preview_value(d).is_some(),
            None => result.as_ref().is_some_and(|r| !r.text.trim().is_empty()),
        },
        _ => false,
    }
}

/// A harness REPL cell (`tool_detail`): what it did in words, never its code. Folded, its line alone; previewed (opened by
/// default), its line and its main value (the cell's own, else its last step's), [`PREVIEW_ROWS`] of it; opened, each step
/// and its value, the cell's value and logs in full, then "Show code". Its result ([`result_value`]) shows under its line
/// however it is folded. A cell that drew (and did not fail) is its view alone, no line over it: the view is the message,
/// and its code shows under it from the view's hover-only code button.
fn card_rows(block: &str, d: &Detail, shown: Shown, fold: &HashMap<String, bool>, out: &mut Vec<Row>) {
    let (words, detail, error) = headline(d);
    let code = fold.get(&code_key(block)).copied().unwrap_or(false);
    let draws = !empty_value(&d.ui);
    // A cell that drew is its view: no line over it. Its code is a hover-only button beside the view.
    if draws && !error {
        out.push(Row::Ui { v: d.ui.clone(), cell: d.cell.clone(), block: block.to_owned(), code });
        if code {
            out.push(Row::Code { lang: "ts".into(), text: d.code.clone() });
        }
        return;
    }
    let tip = d.ms.filter(|_| d.status != "running").map(|ms| format!("Took {}", secs(ms))).unwrap_or_default();
    out.push(Row::Card { block: block.to_owned(), words: dotted(words, &detail), detail, tip, error, open: shown != Shown::Folded });
    if draws {
        out.push(Row::Ui { v: d.ui.clone(), cell: d.cell.clone(), block: block.to_owned(), code: false });
    }
    match shown {
        Shown::Folded => {
            if let Some(v) = result_value(d) {
                value_rows(v, PREVIEW_ROWS, out);
            }
        }
        Shown::Preview => {
            if let Some(main) = preview_value(d) {
                value_rows(main, PREVIEW_ROWS, out);
            }
        }
        Shown::Full => {
            let many = d.effects.len() > 1;
            for e in &d.effects {
                if many {
                    let words = if e.status == "running" { format!("{}\u{2026}", e.doing) } else { e.done.clone() };
                    let detail = step_detail(&e.detail);
                    out.push(Row::Step { words: dotted(words, &detail), detail, error: e.status == "error" });
                }
                value_rows(&e.value, usize::MAX, out);
            }
            if d.dropped > 0 {
                out.push(Row::Step { words: format!("{} more steps, not kept", d.dropped), detail: String::new(), error: false });
            }
            if d.value["t"] == "scalar" && !d.effects.is_empty() {
                out.push(Row::Output { text: cell_text(&d.value["v"]), error: false });
            }
            value_rows(&d.value, usize::MAX, out);
            if !d.logs.trim().is_empty() {
                out.push(Row::Output { text: d.logs.clone(), error: false });
            }
            out.push(Row::CodeToggle { block: block.to_owned(), open: code });
            if code {
                out.push(Row::Code { lang: "ts".into(), text: d.code.clone() });
            }
        }
    }
}

/// Item `n`'s rows, keyed `i{n}.{k}` so a row keeps its key as it streams and as runs form around it.
fn item_rows(t: &Transcript, g: &Grouped, n: usize, fold: &HashMap<String, bool>, out: &mut Vec<(String, Row)>) {
    let mut part = Vec::new();
    match &t.items[n] {
        Item::User { text } => part.push(Row::User(text.clone())),
        Item::Text { text, done, .. } => blocks(text, !done, &mut part),
        Item::Thinking { block, text, done } => {
            if !text.trim().is_empty() || !done {
                let open = fold.get(block).copied().unwrap_or(false);
                part.push(Row::Thinking { block: block.clone(), open, text: text.clone(), live: !done });
            }
        }
        Item::Tool { block, name, input, partial, result, done } => {
            let call = group::call(t, n);
            let shown = call.as_ref().map_or(Shown::Folded, |c| Shown::of(fold, block, opens(t, g, n, c)));
            if let Some(d) = t.details.get(block) {
                card_rows(block, d, shown, fold, &mut part);
            } else {
                let failed = result.as_ref().is_some_and(|r| r.error);
                // A failure says so in its words' tone, not in words of its own.
                let state = match result {
                    Some(_) => String::new(),
                    None if *done && !t.busy => "no result".to_owned(),
                    None => "running\u{2026}".to_owned(),
                };
                let input_text = if *done { tool_input(input) } else { partial.clone() };
                let result = result.as_ref().map(|r| r.text.clone()).unwrap_or_default();
                // Previewed: the head of its output under its line.
                let preview = (shown == Shown::Preview && !result.trim().is_empty()).then(|| head_lines(&result, PREVIEW_ROWS));
                part.push(Row::Tool {
                    block: block.clone(),
                    name: name.clone(),
                    arg: model::chat::main_arg(input),
                    state,
                    shown,
                    failed,
                    input: input_text,
                    result,
                });
                if let Some(text) = preview {
                    part.push(Row::Output { text, error: failed });
                }
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
/// A folded run shows its line ("Ran 2 cells · 2 failed"), its failed calls as one folded line each, its open calls (the
/// last shell call, one the person opened), and the rest that returned a result ([`result_value`]: a table is the answer, not plumbing) folded, their line over their
/// table, in call order; an opened one, every call and the reasoning between them, each folded as it is.
fn rows_nested(t: &Transcript, fold: &HashMap<String, bool>) -> (Vec<(String, Row)>, HashSet<String>) {
    let g = Grouped::of(t);
    let mut out = Vec::new();
    let mut nested = HashSet::new();
    for piece in &g.pieces {
        match piece {
            Piece::Item(n) => item_rows(t, &g, *n, fold, &mut out),
            Piece::Run(run) => {
                let key = run.key();
                let open = fold.get(&key).copied().unwrap_or(false);
                let (failed, running) = (run.failed(), run.running());
                let words = if failed > 0 || running { dotted(run.summary(), "x") } else { run.summary() };
                out.push((key.clone(), Row::Run { key, words, failed, running, open }));
                for &n in &run.items {
                    let call = run.calls.iter().find(|(i, _)| *i == n).map(|(_, c)| c);
                    let shows = open
                        || call.is_some_and(|c| c.failed || fold.get(&c.block).copied().unwrap_or_else(|| opens(t, &g, n, c)));
                    // A call kept for its result shows folded, its own line over its table, so the table never reads as the
                    // line above it's (a failed call's).
                    let result = call.and_then(|c| t.details.get(&c.block)).and_then(result_value).is_some();
                    let from = out.len();
                    if shows || result {
                        item_rows(t, &g, n, fold, &mut out);
                    }
                    nested.extend(out[from..].iter().map(|(k, _)| k.clone()));
                }
            }
        }
    }
    (out, nested)
}

/// The transcript as rows.
fn rows(t: &Transcript, fold: &HashMap<String, bool>) -> Vec<(String, Row)> {
    rows_nested(t, fold).0
}

/// Whether row key `key` (a call's block, a run's `run:` key, a thinking's block, a card's `block#code`) shows open now:
/// as the person set it, else by default.
fn is_open(t: &Transcript, fold: &HashMap<String, bool>, key: &str) -> bool {
    if let Some(&o) = fold.get(key) {
        return o;
    }
    let g = Grouped::of(t);
    t.items.iter().enumerate().find_map(|(i, it)| match it {
        Item::Tool { block, .. } if block == key => group::call(t, i).map(|c| opens(t, &g, i, &c)),
        _ => None,
    }) == Some(true)
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
        Row::Card { words, .. } | Row::Run { words, .. } => vec![(&["head", "words"], words.clone(), false)],
        Row::Step { words, .. } => vec![(&["words"], words.clone(), false)],
        Row::Output { text, .. } => vec![(&["card", "text"], text.clone(), false)],
        Row::Tool { .. }
        | Row::Thinking { .. }
        | Row::Grid { .. }
        | Row::CodeToggle { .. }
        | Row::Table(_)
        | Row::Results { .. }
        | Row::Ui { .. } => Vec::new(),
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
            Row::Output { text, .. } => format!("```\n{text}\n```"),
            Row::Table(t) => t.markdown(),
            Row::Grid { v, .. } => grid_table(v).markdown(),
            Row::Results { v, cap } => {
                let items = result_items(v, *cap);
                let line = |r: &view::table::Result| match (r.title.is_empty(), r.url.is_empty()) {
                    (_, true) => r.title.clone(),
                    (true, false) => r.url.clone(),
                    (false, false) => format!("[{}]({})", r.title, r.url),
                };
                items.iter().map(|r| format!("- {}", line(r))).collect::<Vec<_>>().join("\n")
            }
            Row::Ui { v, .. } => genui::plain(v),
            Row::Tool { .. }
            | Row::Thinking { .. }
            | Row::Card { .. }
            | Row::Run { .. }
            | Row::Step { .. }
            | Row::CodeToggle { .. } => {
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

/// Words node `e` laid out as the painter draws it (at the width layout gave it), and where it is (window points).
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

/// Chat pane `p`'s input words.
fn input_text(w: WorldRef<'_>, p: u64) -> Option<View<'_>> {
    View::of(w.entity_from_id(p)).part("foot")?.part("input")?.part("area")?.part("text")
}

/// How a turn that just ended ended, for Kip: false when the agent failed, None when the person stopped it (or no turn
/// end came).
fn turn_outcome(t: &Transcript) -> Option<bool> {
    let turn = t.items.iter().rposition(|i| matches!(i, Item::User { .. })).map_or(&t.items[..], |u| &t.items[u..]);
    let failed = turn.iter().any(|i| matches!(i, Item::Error { .. }));
    match turn.iter().rev().find(|i| matches!(i, Item::TurnEnd { .. })) {
        Some(Item::TurnEnd { stop, .. }) if stop == "interrupted" => None,
        Some(Item::TurnEnd { ok, .. }) => Some(*ok && !failed),
        _ => failed.then_some(false),
    }
}

/// How long a chat must have been attached and still empty before Kip greets in it: a past chat's log replays in less.
const KIP_GREET_AFTER: f32 = 0.35;

impl App {
    /// Kip on a chat pane's composer, as its transcript `t` stands (`attached`: its stream is in): the look and mood to
    /// show him in (None: not there) and his fade. Perched while the agent works (busy with a tool: working; else
    /// thinking); happy, or oops on an error, for a moment after a turn this pane saw ends; greeting in an empty chat.
    fn chat_kip(&self, st: &mut ChatState, t: &Transcript, attached: bool) -> (Option<(&'static str, Option<&'static str>)>, f32) {
        if t.busy {
            st.kip_busy = true;
            st.kip_end = None;
            let tool = matches!(t.items.last(), Some(Item::Tool { result: None, .. }));
            return (Some(("hn.KipPerch", Some(if tool { "working" } else { "thinking" }))), 0.0);
        }
        if std::mem::take(&mut st.kip_busy) {
            st.kip_end = turn_outcome(t).map(|ok| (ok, self.time));
        }
        if let Some((ok, at)) = st.kip_end {
            let w = self.world.world_ref();
            let linger = view::prefab(w, "hn.KipPerch")
                .and_then(|p| p.try_cloned::<&harness_native_ui_text_api::KipLinger>())
                .unwrap_or_else(|| panic!("the scene's hn.KipPerch has no KipLinger"));
            let since = self.time - at;
            if since < linger.secs {
                let fade = (since - (linger.secs - linger.fade)) / linger.fade.max(1e-3);
                return (Some(("hn.KipPerch", Some(if ok { "happy" } else { "oops" }))), fade);
            }
            st.kip_end = None;
        }
        let settled = attached && st.tried.is_some_and(|at| self.time - at > KIP_GREET_AFTER);
        if t.items.is_empty() && settled {
            return (Some(("hn.KipGreet", None)), 0.0);
        }
        (None, 0.0)
    }

    /// The genui gallery (`HARNESS_NATIVE_GALLERY=genui`, src/gallery.rs): fill its `hn.ChatUi` with `tree`, a cell's
    /// tree as the kernel sends it, measured by this app's typesetter and drawn in its ground's scheme, as a chat card's is.
    pub(crate) fn fill_genui_gallery(&self, root: u64, tree: &Value) {
        let w = self.world.world_ref();
        let v = View::of(w.entity_from_id(root));
        let scheme = harness_native_ui_look_api::Scheme { dark: self.term_ground_dark() };
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
                self.chats.borrow_mut().remove(&sid);
                self.link.attach(sid, 80, 24, true);
            }
        }
        // Drawn in the appearance of the ground it sits on (the terminal's, dark in both appearances unless the Ghostty
        // theme says otherwise), so its inks, plates and washes read on it whatever the window's appearance.
        let scheme = harness_native_ui_look_api::Scheme { dark: self.term_ground_dark() };
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
        let (rows, nested) = rows_nested(t, &st.fold);
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
                if matches!(rows[i].1, Row::Tool { .. } | Row::Card { .. } | Row::Thinking { .. })
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
                for (k, (path, src, md)) in texts(&rows[i].1).iter().enumerate() {
                    let Some(node) = path.iter().try_fold(row, |v, n| v.part(n)) else { continue };
                    let e = node.entity();
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
            self.focused && self.focused_pane() == Some(pane) && !self.tree_focus && self.palette.is_none() && !self.genui_editing();
        if let Some(area) = pv.part("foot").and_then(|f| f.part("input")).and_then(|i| i.part("area")) {
            let (shown, [caret, anchor, lo, hi]) = st.draft.shown();
            if let Some(h) = area.part("hint_at").and_then(|a| a.part("hint")) {
                // Its agent's name: "Message Codex…" in a Codex session.
                let agent = match model::kind_name(w.entity_from_id(e)) {
                    "codex" => "Codex",
                    "cursor" => "Cursor",
                    _ => "Claude",
                };
                h.text(&if shown.is_empty() { format!("Message {agent}\u{2026}") } else { String::new() });
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
                // Grown past its most: the caret's end in view (the end when the caret is at the end, else the top).
                let th = txt.entity().try_cloned::<&Laid>().map_or(0.0, |l| l.h);
                let ah = area.entity().try_cloned::<&Laid>().map_or(0.0, |l| l.h);
                let y = if st.draft.caret == st.draft.text.len() { (th - ah).max(0.0) } else { 0.0 };
                if area.entity().try_cloned::<&ScrollState>().is_none_or(|s| (s.y - y).abs() > 0.5) {
                    scroll_quietly(area.entity(), ScrollState { x: 0.0, y });
                }
            }
        }
        if let Some(kip) = pv.part("foot").and_then(|f| f.part("input")).and_then(|i| i.part("kip")) {
            // Its own stream is in (a transcript gets an entry only once an event comes, and a new chat may send none).
            let attached = st.ours && self.link.attached(sid);
            let (look, fade) = self.chat_kip(st, t, attached);
            panes::show_kip(kip, look, fade);
        }
        if let Some(status) = pv.part("foot").and_then(|f| f.part("status")) {
            let o = w.entity_from_id(e);
            // The model and the effort it runs with, as den's picker (chat/pick.rs): the transcript's when its `session`
            // event (or a choice) said them, else the daemon's listing; the model alone when the effort is unknown.
            let listed = o.try_cloned::<&model::AgentModel>().unwrap_or_default();
            let (model, effort) = if t.model.is_empty() {
                (listed.s.as_str().to_owned(), listed.effort.as_str().to_owned())
            } else {
                let effort = if t.effort.is_empty() { listed.effort.as_str() } else { t.effort.as_str() };
                (t.model.clone(), effort.to_owned())
            };
            self.fill_chat_pick(status, sid, t, model::kind_name(o), st.pick, &mut st.pick_nav, &model, &effort);
            let state = if self.status_of(e) != "running" {
                "ended \u{00B7} a message resumes it"
            } else if t.busy {
                "Working\u{2026}  esc to interrupt"
            } else {
                ""
            };
            if let Some(s) = status.part("state") {
                s.text(state);
            }
        }
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
            let max = (st.content - l.h).max(0.0);
            let mut origin = st.y;
            let busy = st.engine.needs_frame();
            if st.stick && !busy {
                if (st.y - max).abs() > 0.5 {
                    origin = max;
                    st.rev += 1;
                }
            } else if st.shift.abs() > 0.01 && !busy {
                origin = (st.y + st.shift).clamp(0.0, max);
                st.rev += 1;
            } else if st.y > max && !busy {
                origin = max;
                st.rev += 1;
            }
            st.shift = 0.0;
            let t = st.clock.elapsed().as_secs_f64();
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
            if list.entity().try_cloned::<&ScrollState>().is_none_or(|s| (s.y - y).abs() > 0.001) {
                // Following the agent at the bottom moves it quietly; a scroll of the reader's shows the scroller.
                if st.stick && !st.engine.needs_frame() {
                    scroll_quietly(list.entity(), ScrollState { x: 0.0, y });
                } else {
                    list.entity().set(ScrollState { x: 0.0, y });
                }
            }
        }
        if moving {
            self.poke();
        }
        // A focused input's caret fades on the clock: keep drawing.
        if self.focused
            && self.look.themes[usize::from(self.dark)].blink
            && self.chat_focused()
            && !self.tree_focus
            && self.palette.is_none()
            && !self.genui_editing()
        {
            self.poke_why("caret blink");
        }
        // A cell's controls: its field's typing, a dragged slider, an open select.
        self.step_genui();
        // A press off the picker's open menu closes it.
        self.step_chat_pick();
        self.step_genui_common();
    }

    /// A scroll over a chat pane's transcript: its physics take it (scrolling up lets go of the bottom). Whether it was
    /// over one.
    pub(super) fn chat_wheel(&mut self, ev: &sidebar::Wheel, z: f32) -> bool {
        let w = self.world.world_ref();
        let (x, y) = (self.pointer.x, self.pointer.y);
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
        let k = if ev.precise { 1.0 / z } else { 1.0 };
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

    /// Open or fold a tool call's, a run's or thinking's row, or a card's code (`block#code`): the other way from how it
    /// shows now, kept by its key as more calls stream in.
    pub(super) fn chat_toggle(&mut self, block: &str) {
        let Some((_, _, sid)) = self.focused_chat().or_else(|| self.any_chat_with(block)) else { return };
        let chats = self.chats.borrow();
        let empty = Transcript::default();
        let t = chats.get(&sid).unwrap_or(&empty);
        let mut states = self.chat.borrow_mut();
        let st = states.entry(sid).or_default();
        let open = is_open(t, &st.fold, block);
        st.fold.insert(block.to_owned(), !open);
        drop(states);
        drop(chats);
        self.poke();
    }

    fn any_chat_with(&self, block: &str) -> Option<(u64, u64, u64)> {
        let block = block.strip_suffix("#code").unwrap_or(block);
        let block = block.strip_prefix("run:").unwrap_or(block);
        let chats = self.chats.borrow();
        let sid = chats.iter().find(|(_, t)| t.items.iter().any(|i| matches!(i, Item::Tool { block: b, .. } | Item::Thinking { block: b, .. } if b == block)))?.0;
        Some((0, 0, *sid))
    }

    /// Run `f` on the focused chat's draft; true when there is one.
    fn with_draft(&mut self, f: impl FnOnce(&mut Draft)) -> bool {
        // A cell's field that has the keys takes them before the chat's input.
        if self.genui_editing() {
            return self.genui_draft(f);
        }
        let Some((_, _, sid)) = self.focused_chat() else { return false };
        f(&mut self.chat.borrow_mut().entry(sid).or_default().draft);
        self.poke();
        true
    }

    /// An `act.Edit` on the focused chat's input (scene/20-keys.flecs ctx.Chat).
    pub(super) fn chat_edit(&mut self, op: &str) {
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
        let mv = match name {
            "left" => Some(Move::Left),
            "right" => Some(Move::Right),
            "up" => Some(Move::Up),
            "down" => Some(Move::Down),
            "word_left" => Some(Move::WordLeft),
            "word_right" => Some(Move::WordRight),
            "line_start" => Some(Move::LineStart),
            "line_end" => Some(Move::LineEnd),
            "doc_start" => Some(Move::DocStart),
            "doc_end" => Some(Move::DocEnd),
            _ => None,
        };
        if let Some(m) = mv {
            if let Some(to) = self.chat_target(m) {
                self.with_draft(|d| d.go(m, to, select));
            }
            return;
        }
        let erase = match op {
            "backspace" => Some(Move::Left),
            "delete" => Some(Move::Right),
            "word_erase" => Some(Move::WordLeft),
            "word_delete" => Some(Move::WordRight),
            "line_erase" => Some(Move::LineStart),
            _ => None,
        };
        if let Some(m) = erase {
            if let Some(to) = self.chat_target(m) {
                self.with_draft(|d| d.erase(to));
            }
            return;
        }
        match op {
            "newline" => _ = self.with_draft(|d| d.insert("\n")),
            // Return, or Command-Return (which submits a cell's TextArea).
            "send" | "submit" => {
                let mut text = String::new();
                self.with_draft(|d| {
                    if d.text.trim().is_empty() {
                        return;
                    }
                    text = std::mem::take(&mut d.text).trim_end().to_owned();
                    d.caret = 0;
                    d.anchor = 0;
                    d.marked = None;
                });
                if !text.is_empty() {
                    self.link.chat_send(sid, &text);
                    if let Some(st) = self.chat.borrow_mut().get_mut(&sid) {
                        st.stick = true;
                    }
                }
            }
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
        if let Some(m) = self.genui_marked() {
            return Some(m);
        }
        let (_, _, sid) = self.focused_chat()?;
        Some(self.chat.borrow().get(&sid).and_then(|s| s.draft.marked.clone()))
    }

    /// What the focused chat has selected, for Command-C: its transcript's selection (a row taken whole as markdown),
    /// else its input's.
    pub(super) fn chat_copy(&self) -> Option<Option<(String, Copied)>> {
        if let Some((text, node)) = self.genui_copy() {
            return Some((!text.is_empty()).then_some((text, Copied::Field(node))));
        }
        let (_, _, sid) = self.focused_chat()?;
        let states = self.chat.borrow();
        let Some(st) = states.get(&sid) else { return Some(None) };
        if let Some((a, b)) = st.sel.filter(|(a, b)| a != b) {
            let chats = self.chats.borrow();
            let empty = Transcript::default();
            let rows = rows(chats.get(&sid).unwrap_or(&empty), &st.fold);
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
        let rows = rows(chats.get(&sid).unwrap_or(&empty), &st.fold);
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
        let (in_input, in_list) = (over(pv.part("foot").and_then(|f| f.part("input"))), over(pv.part("list")));
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
        for st in self.chat.borrow_mut().values_mut() {
            if st.drag.take().is_some() {
                any = true;
                if st.sel.is_some_and(|(a, b)| a == b) {
                    st.sel = None;
                }
            }
        }
        if any {
            self.poke();
        }
        any
    }

    /// Session `sid`'s transcript as rows.
    fn chat_rows(&self, sid: u64) -> Vec<(String, Row)> {
        let chats = self.chats.borrow();
        let empty = Transcript::default();
        let states = self.chat.borrow();
        let fold = states.get(&sid).map(|s| s.fold.clone()).unwrap_or_default();
        rows(chats.get(&sid).unwrap_or(&empty), &fold)
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
                let want = *depth as f32 * 18.0;
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
        Row::Tool { block, name, arg, state, shown, input, result, .. } => {
            if let Some(h) = row.part("head") {
                h.action(&format!("chat:open:{block}"));
                line(h, name, arg, state, "", Some(*shown != Shown::Folded));
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
                line(h, if *live { "Thinking\u{2026}" } else { "Thought" }, "", "", "", Some(*open));
            }
            if let Some(t) = row.part("body").and_then(|b| b.part("text")) {
                t.text(s.trim());
            }
        }
        Row::Card { block, words, detail, tip, open, .. } => {
            if let Some(h) = row.part("head") {
                h.action(&format!("chat:open:{block}"));
                line(h, words, "", detail, tip, Some(*open));
            }
        }
        Row::Run { key, words, failed, running, open } => {
            if let Some(h) = row.part("head") {
                h.action(&format!("chat:open:{key}"));
                let detail = if *failed > 0 { format!("{failed} failed") } else if *running { "running\u{2026}".into() } else { String::new() };
                line(h, words, "", &detail, "", Some(*open));
            }
        }
        Row::Step { words, detail, .. } => line(row, words, "", detail, "", None),
        Row::Grid { v, cap } => fill_grid(row, v, *cap, ctx, measure),
        Row::Table(t) => {
            if let Some(grid) = row.part("card").and_then(|c| c.part("grid")) {
                let action = ctx.row_action();
                let opts = view::table::Opts { look: "hn.ChatTable", shown: t.rows.len(), action: action.as_deref(), open: ctx.open, ..view::table::Opts::default() };
                view::table(grid, t, opts, measure);
            }
        }
        Row::Results { v, cap } => fill_results(row, v, *cap),
        Row::Ui { v, cell, block, code } => {
            genui::fill_in(row, v, cell, ctx.cells, ctx.ui, ctx.playing, measure);
            // Its code: a quiet button in the gutter, shown while the view is hovered or its code is open.
            if let Some(b) = row.part("code_at").and_then(|z| z.part("code")) {
                b.action(&format!("chat:open:{}", code_key(block)));
                tooltip(b, if *code { "Hide code" } else { "Show code" });
                if let Some(i) = b.part("icon") {
                    i.icon(CODE_ICON);
                    crate::look::hover_show(i.entity(), *row.entity().id(), *code);
                }
            }
        }
        Row::Output { text: s, .. } => {
            if let Some(t) = row.part("card").and_then(|c| c.part("text")) {
                t.text(s);
            }
        }
        Row::CodeToggle { block, open } => {
            if let Some(h) = row.part("head") {
                h.action(&format!("chat:open:{}", code_key(block)));
                if let Some(p) = h.part("words") {
                    p.text(if *open { "Hide code" } else { "Show code" });
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

/// Search results (a table the kernel tags `kind: "search"`: title, url, site, snippet) as a `ui.ResultList`: each a
/// link titled as its page, its host (an unresolved Google link: the site Google shows), two lines of its snippet.
fn fill_results(row: View<'_>, v: &Value, cap: usize) {
    let items = result_items(v, cap);
    if let Some(list) = row.part("list") {
        view::table::results(list, &items, "hn.ChatResult");
    }
    let total = v["total"].as_u64().map_or(v["rows"].as_array().map_or(0, Vec::len), |t| t as usize);
    if let Some(f) = row.part("foot") {
        foot(f, &if items.len() < total { format!("{} of {total} results", items.len()) } else { String::new() });
    }
}

/// Search results' first `cap` as [`view::table::Result`]s (see [`fill_results`]).
fn result_items(v: &Value, cap: usize) -> Vec<view::table::Result> {
    let cols: Vec<&str> = v["cols"].as_array().into_iter().flatten().map(|c| c["name"].as_str().unwrap_or("")).collect();
    let at = |name: &str| cols.iter().position(|c| *c == name);
    let (title, url, site, snippet) = (at("title"), at("url"), at("site"), at("snippet"));
    let rows: &[Value] = v["rows"].as_array().map_or(&[], Vec::as_slice);
    let get = |r: &Value, c: Option<usize>| c.map(|c| cell_text(&r[c])).filter(|s| s != "null").unwrap_or_default();
    rows
        .iter()
        .take(cap)
        .map(|r| {
            let url = get(r, url);
            let host = if url.contains("google.") || url.is_empty() {
                get(r, site).split(['\u{203A}', ' ']).next().unwrap_or("").to_owned()
            } else {
                view::table::host(&url).to_owned()
            };
            view::table::Result { title: get(r, title), url, host, snippet: get(r, snippet) }
        })
        .collect()
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
/// on hover `tip` (how long it took; none when empty), and its hover-only chevron turned to `open` (None: a step, which
/// opens nothing).
fn line(h: View<'_>, words: &str, arg: &str, detail: &str, tip: &str, open: Option<bool>) {
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
    for (part, s) in [("arg", arg)] {
        if let Some(p) = h.part(part) {
            p.text(s);
            let e = p.entity();
            let gone = s.is_empty();
            if gone != e.try_cloned::<&Size>().is_some_and(|z| z.w == Sizing::fixed(0.0)) {
                if gone {
                    p.set(Size { w: Sizing::fixed(0.0), h: Sizing::FIT });
                    p.set(harness_native_ui_layout_api::Padding::default());
                } else {
                    // Back to the prefab's own: its natural width, the room after it.
                    e.remove(Size::id());
                    let lead = view::prefab(e.world(), "hn.ChatLineLook")
                        .and_then(|l| View::of(l).part(part))
                        .and_then(|a| a.entity().try_cloned::<&harness_native_ui_layout_api::Padding>())
                        .unwrap_or_default();
                    p.set(lead);
                }
            }
        }
    }
    tooltip(h, tip);
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

    /// In a folded run, a call's table (the answer the person asked for) shows under the run's line without opening it; a
    /// failed call is one folded line, its run says how many failed; a cell that drew is its view, no line over it.
    #[test]
    fn folded_runs_keep_results_failures_fold_and_views_have_no_line() {
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
        let (rows, nested) = rows_nested(&t, &HashMap::new());
        let kinds: Vec<(&str, &str)> = rows.iter().map(|(k, r)| (k.as_str(), r.prefab())).collect();
        assert_eq!(kinds[1].1, "hn.ChatRunFailed", "{kinds:?}");
        assert!(matches!(&rows[1].1, Row::Run { words, failed: 1, .. } if words.ends_with('\u{00B7}')), "{kinds:?}");
        // The failed call: one line in the failed tone, folded although it has output to show, and no timing on it.
        let (k, failed) = &rows[2];
        assert!(matches!(failed, Row::Card { error: true, tip, .. } if tip == "Took 0.0s"), "{kinds:?}");
        assert_eq!(k, "i1.0");
        // The table under its own call's line, folded, so it never reads as the failure's; the echo's output folds away.
        assert_eq!(rows[3].0, "i2.0", "{kinds:?}");
        assert!(matches!(&rows[3].1, Row::Card { error: false, .. }), "{kinds:?}");
        assert_eq!(rows[4].0, "i2.1");
        assert!(matches!(&rows[4].1, Row::Grid { cap: PREVIEW_ROWS, .. }), "{kinds:?}");
        assert!(nested.contains("i1.0") && nested.contains("i2.0") && nested.contains("i2.1"));
        // The drawn view stands alone with no line over it, its code hidden until asked for.
        assert!(matches!(&rows[5].1, Row::Ui { block, code: false, .. } if block == "d"), "{kinds:?}");
        assert_eq!(rows.len(), 7, "user, run, failure, table's call, table, view, prose: {kinds:?}");
        // Its code button shows the cell's code under the view.
        let code = HashMap::from([(code_key("d"), true)]);
        let shown = super::rows(&t, &code);
        assert!(matches!(&shown[6].1, Row::Code { lang, .. } if lang == "ts"), "{:?}", shown.iter().map(|(_, r)| r.prefab()).collect::<Vec<_>>());
    }

    #[test]
    fn markdown_splits_into_rows() {
        let mut out = Vec::new();
        blocks("# Title\n\nSome *words*\nwrapped.\n\n- one\n  more\n- two\n\n```ts\nconst x = 1;\n```\n> quoted", false, &mut out);
        let kinds: Vec<&str> = out.iter().map(Row::prefab).collect();
        assert_eq!(kinds, ["hn.ChatH1", "hn.ChatProse", "hn.ChatItem", "hn.ChatItem", "hn.ChatCode", "hn.ChatQuote"]);
        assert!(matches!(&out[1], Row::Prose(s) if s == "Some *words* wrapped."));
        assert!(matches!(&out[2], Row::ListItem { text, .. } if text == "one more"));
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
