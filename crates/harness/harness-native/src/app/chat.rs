//! The chat pane (`hn.ChatPane`, scene/widgets/chat.flecs): a chat session (the daemon's chat mode: an agent over
//! structured events, not a terminal) drawn as a chat. The transcript comes from the model's [`model::chat::Transcript`]
//! (agent-neutral events); each of its items becomes rows (a user message, the agent's markdown split into paragraphs,
//! headings, list items, quotes and code blocks, thinking and tool calls folded to a line, a turn's foot) in a
//! `ui.LazyList`, so only the rows in view are entities. It scrolls on AppKit's physics and sticks to the bottom while
//! the agent writes unless the person scrolled up. The input is a `ui.TextArea` edited here (`Draft`, keys in
//! scene/20-keys.flecs ctx.Chat): Return sends, Shift+Return breaks the line, Escape interrupts.
use super::*;
use harness_native_scroll_physics::{Engine, Input, Mutation, Phase, Vec2};
use harness_native_ui_layout_api::{ScrollState, ScrollerGrip, scroll_quietly};
use harness_native_ui_text_api::{Highlight, TextEdit};
use crate::pane::Unit;
use harness_native_render::text::Layout;
use harness_native_ui_input_api::Tooltip;
use harness_native_ui_look_api::TextStyle;
use harness_native_ui_text_api::{MarkdownText, Text};
use model::chat::{Detail, Item, Transcript};
use serde_json::Value;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

mod genui;

/// cod-chevron-right, cod-chevron-down.
const CLOSED: &str = "\u{EAB6}";
const OPENED: &str = "\u{EAB4}";

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

/// A chat pane's own state, by session: its rows' heights, the tool calls and thinking opened, the input, its scroll.
pub struct ChatState {
    heights: view::Heights,
    open: HashSet<String>,
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
            open: HashSet::new(),
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
    Tool { block: String, name: String, arg: String, state: String, open: bool, input: String, result: String },
    /// A harness REPL cell's card head: its step's icon, what it did in words, a detail ("5 results"), how long it took.
    Card { block: String, icon: &'static str, words: String, detail: String, state: String, error: bool, open: bool },
    /// One step of an opened card that took several.
    Step { icon: &'static str, words: String, detail: String, error: bool },
    /// A table, record or list (the kernel's shown value), its first `cap` rows.
    Grid { v: Arc<Value>, cap: usize },
    /// Text a cell printed or returned: a command's output, a string, an error's stack.
    Output { text: String, error: bool },
    /// What a cell draws (its Svelte markup's tree, [`genui`]).
    Ui { v: Arc<Value> },
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
            Row::Tool { open: true, .. } => "hn.ChatToolOpen",
            Row::Tool { .. } => "hn.ChatTool",
            Row::Card { error: true, .. } => "hn.ChatCardFailed",
            Row::Card { .. } => "hn.ChatCard",
            Row::Step { error: true, .. } => "hn.ChatStepFailed",
            Row::Step { .. } => "hn.ChatStep",
            Row::Grid { .. } => "hn.ChatGrid",
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
/// joined), quotes, fenced code (an unclosed fence, still streaming, runs to the end) and tables (as code).
fn blocks(md: &str, out: &mut Vec<Row>) {
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

/// Rows a folded card's table or output shows.
const FOLDED_ROWS: usize = 8;

/// codicons by step kind (mcp/src/effects.ts), and for a cell that took no step, an error, a running cell.
fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "exec" => "\u{EA85}",   // cod-terminal
        "read" => "\u{EA7B}",   // cod-file
        "write" => "\u{EA73}",  // cod-edit
        "delete" => "\u{EA81}", // cod-trash
        "net" => "\u{EB01}",    // cod-globe
        "search" => "\u{EA6D}", // cod-search
        "spawn" => "\u{EC10}",  // cod-sparkle
        "sql" => "\u{EACE}",    // cod-database
        "error" => "\u{EA87}",  // cod-error
        _ => "\u{EAC4}",        // cod-code
    }
}

/// How many steps of one kind did, in words: "Wrote 3 files".
fn steps_of(kind: &str, n: usize) -> String {
    match kind {
        "exec" => format!("Ran {n} commands"),
        "read" => format!("Read {n} files"),
        "write" => format!("Wrote {n} files"),
        "delete" => format!("Deleted {n} files"),
        "net" => format!("Fetched {n} URLs"),
        "search" => format!("Ran {n} searches"),
        "spawn" => format!("Started {n} objects"),
        "sql" => format!("Ran {n} queries"),
        _ => format!("{n} steps"),
    }
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
fn headline(d: &Detail) -> (&'static str, String, String, bool) {
    if let Some(e) = d.effects.iter().rev().find(|e| e.status == "running") {
        return (kind_icon(&e.kind), format!("{}\u{2026}", e.doing), String::new(), false);
    }
    if d.status == "error" {
        let why = d.value["message"].as_str().map_or_else(|| "failed".to_owned(), first_line);
        return (kind_icon("error"), why, String::new(), true);
    }
    let failed = d.effects.iter().any(|e| e.status == "error");
    match d.effects.as_slice() {
        [] if d.status == "running" => (kind_icon(""), "Running\u{2026}".into(), String::new(), false),
        [] => {
            let v = &d.value;
            let n = v["total"].as_u64().unwrap_or(0);
            let words = match v["t"].as_str().unwrap_or("") {
                "table" => format!("{n} row{}", if n == 1 { "" } else { "s" }),
                "list" => format!("{n} item{}", if n == 1 { "" } else { "s" }),
                "record" => format!("{n} field{}", if n == 1 { "" } else { "s" }),
                "scalar" => cell_text(&v["v"]),
                "text" => first_line(v["v"].as_str().unwrap_or("")),
                _ if !d.ui.is_null() => "Drew a view".into(),
                _ => "Done".into(),
            };
            (kind_icon(""), words, String::new(), false)
        }
        [e] => (kind_icon(&e.kind), e.done.clone(), e.detail.clone(), failed),
        [e, rest @ ..] if rest.iter().all(|r| r.kind == e.kind) => (kind_icon(&e.kind), steps_of(&e.kind, rest.len() + 1), String::new(), failed),
        [e, rest @ ..] => (kind_icon(&e.kind), format!("{} and {} more", e.done, rest.len()), String::new(), failed),
    }
}

/// The rows a shown value is drawn as: a grid, or output text (`cap` rows or lines of either).
fn value_rows(v: &Arc<Value>, cap: usize, out: &mut Vec<Row>) {
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

/// A harness REPL cell's card (`tool_detail`): what it did in words, never its code. Folded, its head and its main value
/// (the cell's own, else its last step's), [`FOLDED_ROWS`] of it; opened, each step and its value, the cell's value and
/// logs in full, then "Show code".
fn card_rows(block: &str, d: &Detail, open: &HashSet<String>, out: &mut Vec<Row>) {
    let is_open = open.contains(block);
    let (icon, words, detail, error) = headline(d);
    let state = d.ms.filter(|_| d.status != "running").map(secs).unwrap_or_default();
    out.push(Row::Card { block: block.to_owned(), icon, words, detail, state, error, open: is_open });
    // What it draws shows under its head, folded or open, in place of its value.
    if !d.ui.is_null() {
        out.push(Row::Ui { v: d.ui.clone() });
    }
    if !is_open {
        if !d.ui.is_null() {
            return;
        }
        let main = if d.value.is_null() { d.effects.iter().rev().map(|e| &e.value).find(|v| !v.is_null()) } else { Some(&d.value) };
        // A cell whose whole value is its head ("42") shows nothing more.
        if let Some(main) = main.filter(|m| !(d.effects.is_empty() && m["t"] == "scalar")) {
            value_rows(main, FOLDED_ROWS, out);
        }
        return;
    }
    let many = d.effects.len() > 1;
    for e in &d.effects {
        if many {
            let words = if e.status == "running" { format!("{}\u{2026}", e.doing) } else { e.done.clone() };
            out.push(Row::Step { icon: kind_icon(&e.kind), words, detail: e.detail.clone(), error: e.status == "error" });
        }
        value_rows(&e.value, usize::MAX, out);
    }
    if d.dropped > 0 {
        out.push(Row::Step { icon: kind_icon(""), words: format!("{} more steps, not kept", d.dropped), detail: String::new(), error: false });
    }
    if d.value["t"] == "scalar" && !d.effects.is_empty() {
        out.push(Row::Output { text: cell_text(&d.value["v"]), error: false });
    }
    value_rows(&d.value, usize::MAX, out);
    if !d.logs.is_empty() {
        out.push(Row::Output { text: d.logs.clone(), error: false });
    }
    let code = open.contains(&code_key(block));
    out.push(Row::CodeToggle { block: block.to_owned(), open: code });
    if code {
        out.push(Row::Code { lang: "ts".into(), text: d.code.clone() });
    }
}

/// The transcript as rows, keyed so a row keeps its key as it streams.
fn rows(t: &Transcript, open: &HashSet<String>) -> Vec<(String, Row)> {
    let mut out = Vec::new();
    for (n, it) in t.items.iter().enumerate() {
        let mut part = Vec::new();
        match it {
            Item::User { text } => part.push(Row::User(text.clone())),
            Item::Text { text, .. } => blocks(text, &mut part),
            Item::Thinking { block, text, done } => {
                if !text.trim().is_empty() || !done {
                    part.push(Row::Thinking { block: block.clone(), open: open.contains(block), text: text.clone(), live: !done });
                }
            }
            Item::Tool { block, .. } if t.details.contains_key(block) => card_rows(block, &t.details[block], open, &mut part),
            Item::Tool { block, name, input, partial, result, done } => {
                let state = match result {
                    Some(r) if r.error => "failed".to_owned(),
                    Some(_) => String::new(),
                    None if *done && !t.busy => "no result".to_owned(),
                    None => "running\u{2026}".to_owned(),
                };
                let input_text = if *done { tool_input(input) } else { partial.clone() };
                part.push(Row::Tool {
                    block: block.clone(),
                    name: name.clone(),
                    arg: model::chat::main_arg(input),
                    state,
                    open: open.contains(block),
                    input: input_text,
                    result: result.as_ref().map(|r| r.text.clone()).unwrap_or_default(),
                });
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
    out
}

/// Put the macOS pasteboard's string.
fn pasteboard_set(text: &str) {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    // SAFETY: NSPasteboardTypeString is a valid static pasteboard type.
    pb.setString_forType(&objc2_foundation::NSString::from_str(text), unsafe { NSPasteboardTypeString });
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
        Row::Tool { open: true, input, result, .. } => vec![
            (&["body", "input"], input.clone(), false),
            (&["body", "result"], tool_result(result).to_owned(), false),
        ],
        Row::Thinking { open: true, text, .. } => vec![(&["text"], text.trim().to_owned(), false)],
        Row::Note(s) | Row::Error(s) => vec![(&["text"], s.clone(), false)],
        Row::Card { words, .. } => vec![(&["head", "words"], words.clone(), false)],
        Row::Step { words, .. } => vec![(&["words"], words.clone(), false)],
        Row::Output { text, .. } => vec![(&["card", "text"], text.clone(), false)],
        Row::Tool { .. } | Row::Thinking { .. } | Row::Grid { .. } | Row::CodeToggle { .. } | Row::Ui { .. } => Vec::new(),
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
            Row::Tool { .. } | Row::Thinking { .. } | Row::Card { .. } | Row::Step { .. } | Row::Grid { .. } | Row::CodeToggle { .. } | Row::Ui { .. } => {
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
        if bits.is_empty() {
            continue;
        }
        let piece = if whole { r.markdown().unwrap_or_else(|| bits.join("\n")) } else { bits.join(" ") };
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

/// Chat pane `p`'s input words.
fn input_text(w: WorldRef<'_>, p: u64) -> Option<View<'_>> {
    View::of(w.entity_from_id(p)).part("foot")?.part("input")?.part("area")?.part("text")
}

impl App {
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
        // Its own stream of events: attach (again a second after a try while it runs, once when it ended). A stream the
        // link holds from before the session was known to be a chat is a terminal's: dropped for ours.
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
        let rows = rows(t, &st.open);
        if let Some(list) = pv.part("list")
            && let Some(stack) = list.part("rows")
        {
            let room = list.entity().try_cloned::<&Laid>().map_or(600.0, |l| l.h);
            let pad = stack.entity().try_cloned::<&harness_native_ui_layout_api::Padding>().unwrap_or_default();
            // The transcript's selection, a wash over the words it covers in the input's selection colours.
            let sel = st.sel.map(|(a, b)| (a.min(b), a.max(b))).filter(|(a, b)| a != b);
            let wash = input_text(w, pane).and_then(|t| t.entity().try_cloned::<&TextEdit>()).unwrap_or_default();
            let fresh_rows = |row: View<'_>, i: usize, new: bool| {
                if new {
                    row.set(HoverAmt::default());
                    if let Some(h) = row.part("head") {
                        h.set(HoverAmt::default());
                    }
                }
                fill_row(row, &rows[i].1);
                for (k, (path, src, md)) in texts(&rows[i].1).iter().enumerate() {
                    let Some(node) = path.iter().try_fold(row, |v, n| v.part(n)) else { continue };
                    let e = node.entity();
                    let range = sel.and_then(|(lo, hi)| covered(lo, hi, i, k, units(&shown(src, *md))));
                    match range.filter(|(a, b)| a < b) {
                        Some((a, b)) => {
                            let te = TextEdit {
                                caret: b as u32,
                                anchor: a as u32,
                                select_dark: wash.select_dark,
                                select_light: wash.select_light,
                                ..TextEdit::default()
                            };
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
                fresh_rows,
            );
            st.content = s.content + pad.t + pad.b;
            st.shift += s.shift;
        }
        // The input: its words (the marked text in), caret and selection, focused while the pane has the keyboard.
        let focused = self.focused && self.focused_pane() == Some(pane) && !self.tree_focus && self.palette.is_none();
        if let Some(area) = pv.part("foot").and_then(|f| f.part("input")).and_then(|i| i.part("area")) {
            let (shown, [caret, anchor, lo, hi]) = st.draft.shown();
            if let Some(h) = area.part("hint") {
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
        if let Some(status) = pv.part("foot").and_then(|f| f.part("status")) {
            let o = w.entity_from_id(e);
            let model = if t.model.is_empty() {
                o.try_cloned::<&model::AgentModel>().map(|m| m.s.as_str().to_owned()).unwrap_or_default()
            } else {
                t.model.clone()
            };
            if let Some(m) = status.part("model") {
                m.text(&model);
            }
            let state = if self.status_of(e) != "running" {
                "ended"
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
        if self.focused && self.look.themes[usize::from(self.dark)].blink && self.chat_focused() && !self.tree_focus && self.palette.is_none() {
            self.poke_why("caret blink");
        }
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

    /// Open or fold a tool call's or thinking's row, or a card's code (`block#code`).
    pub(super) fn chat_toggle(&mut self, block: &str) {
        let Some((_, _, sid)) = self.focused_chat().or_else(|| self.any_chat_with(block)) else { return };
        let mut states = self.chat.borrow_mut();
        let st = states.entry(sid).or_default();
        if !st.open.remove(block) {
            st.open.insert(block.to_owned());
        }
        drop(states);
        self.poke();
    }

    fn any_chat_with(&self, block: &str) -> Option<(u64, u64, u64)> {
        let block = block.strip_suffix("#code").unwrap_or(block);
        let chats = self.chats.borrow();
        let sid = chats.iter().find(|(_, t)| t.items.iter().any(|i| matches!(i, Item::Tool { block: b, .. } | Item::Thinking { block: b, .. } if b == block)))?.0;
        Some((0, 0, *sid))
    }

    /// Run `f` on the focused chat's draft; true when there is one.
    fn with_draft(&mut self, f: impl FnOnce(&mut Draft)) -> bool {
        let Some((_, _, sid)) = self.focused_chat() else { return false };
        f(&mut self.chat.borrow_mut().entry(sid).or_default().draft);
        self.poke();
        true
    }

    /// An `act.Edit` on the focused chat's input (scene/20-keys.flecs ctx.Chat).
    pub(super) fn chat_edit(&mut self, op: &str) {
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
            "cut" => {
                let mut cut = String::new();
                self.with_draft(|d| {
                    cut = d.selected().to_owned();
                    d.insert("");
                });
                if !cut.is_empty() {
                    pasteboard_set(&cut);
                }
            }
            "send" => {
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
        let typed: String = text.chars().filter(|c| *c == '\n' || *c == '\t' || (!c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(c))).collect();
        self.with_draft(|d| {
            d.marked = None;
            if !typed.is_empty() {
                d.insert(&typed);
            }
        })
    }

    /// An input method's marked text on the focused chat's input. Whether a chat took it.
    pub(super) fn chat_set_marked(&mut self, text: Option<&str>) -> bool {
        self.with_draft(|d| d.marked = text.filter(|s| !s.is_empty()).map(str::to_owned))
    }

    pub(super) fn chat_marked(&self) -> Option<Option<String>> {
        let (_, _, sid) = self.focused_chat()?;
        Some(self.chat.borrow().get(&sid).and_then(|s| s.draft.marked.clone()))
    }

    /// What the focused chat has selected, for Command-C: its transcript's selection (a row taken whole as markdown),
    /// else its input's.
    pub(super) fn chat_copy(&self) -> Option<Option<String>> {
        let (_, _, sid) = self.focused_chat()?;
        let states = self.chat.borrow();
        let Some(st) = states.get(&sid) else { return Some(None) };
        if let Some((a, b)) = st.sel.filter(|(a, b)| a != b) {
            let chats = self.chats.borrow();
            let empty = Transcript::default();
            let rows = rows(chats.get(&sid).unwrap_or(&empty), &st.open);
            return Some(Some(transcript_text(&rows, a.min(b), a.max(b))).filter(|s| !s.is_empty()));
        }
        Some(Some(st.draft.selected().to_owned()).filter(|s| !s.is_empty()))
    }

    /// Command-A in the focused chat's input. Whether a chat took it.
    pub(super) fn chat_select_all(&mut self) -> bool {
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
        let (lay, laid) = words_layout(&mut self.paint, txt.entity())?;
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
        let d = &mut states.entry(sid).or_default().draft;
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

    /// Where the focused chat's caret is (window points), for the input method's candidate window: the start of the
    /// marked text, else the caret, on the input's layout.
    pub(super) fn chat_caret_rect(&mut self) -> Option<Option<[f32; 4]>> {
        let (p, ..) = self.focused_chat()?;
        let Some((lay, l, ..)) = self.input_layout() else { return Some(None) };
        let w = self.world.world_ref();
        let Some(te) = input_text(w, p).and_then(|t| t.entity().try_cloned::<&TextEdit>()) else { return Some(None) };
        let at = if te.mark_hi > te.mark_lo { te.mark_lo } else { te.caret } as usize;
        let (x, top, h) = match lay.lines.is_empty() {
            true => (0.0, 0.0, l.h),
            false => {
                let (x, li) = lay.caret(at);
                (x, lay.lines[li].top, lay.lines[li].height)
            }
        };
        let z = self.zoom();
        Some(Some([(l.x + x) * z, (l.y + top) * z, 2.0 * z, h * z]))
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
            for (k, (path, src, md)) in texts(&rows[i].1).iter().enumerate() {
                let Some(node) = path.iter().try_fold(row, |v, n| v.part(n)) else { continue };
                let Some(l) = node.entity().try_cloned::<&Laid>() else { continue };
                if y < l.y {
                    return Some(Spot { row: i, part: k, at: 0 });
                }
                if y < l.y + l.h {
                    let (lay, l) = words_layout(&mut self.paint, node.entity())?;
                    return Some(Spot { row: i, part: k, at: lay.index_at(x - l.x, y - l.y) });
                }
                last = Some(Spot { row: i, part: k, at: units(&shown(src, *md)) });
            }
        }
        last
    }

    /// What a press at `s` selects by `unit`: nothing (a place), its word, or its words whole.
    fn spot_span(rows: &[(String, Row)], s: Spot, unit: Unit) -> (Spot, Spot) {
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
    /// its transcript it starts a selection there; a drag extends either. Whether a chat took it.
    pub(super) fn chat_pointer_down(&mut self, clicks: isize) -> bool {
        let Some((p, _, sid)) = self.focused_chat() else { return false };
        let (x, y) = (self.pointer.x, self.pointer.y);
        let unit = match clicks {
            ..=1 => Unit::Cell,
            2 => Unit::Word,
            _ => Unit::Line,
        };
        let w = self.world.world_ref();
        let pv = View::of(w.entity_from_id(p));
        let over = |v: Option<View<'_>>| v.and_then(|v| v.entity().try_cloned::<&Laid>()).is_some_and(|l| l.contains(x, y));
        let (in_input, in_list) = (over(pv.part("foot").and_then(|f| f.part("input"))), over(pv.part("list")));
        if in_input {
            let at = self.input_index(x, y);
            let mut states = self.chat.borrow_mut();
            let st = states.entry(sid).or_default();
            st.sel = None;
            if let Some(i) = at.filter(|_| st.draft.marked.is_none()) {
                let span = st.draft.span(i, unit);
                (st.draft.anchor, st.draft.caret, st.draft.goal) = (span.0, span.1, None);
                st.drag = Some(Drag::Input { unit, span });
            }
        } else if in_list {
            let rows = self.chat_rows(sid);
            let spot = self.transcript_spot(p, &rows, x, y);
            let mut states = self.chat.borrow_mut();
            let st = states.entry(sid).or_default();
            st.draft.anchor = st.draft.caret;
            st.sel = None;
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

    /// The pointer moved with a chat's press held: its selection follows (the press's own span kept whole). Whether a
    /// chat is being dragged in.
    pub(super) fn chat_pointer_drag(&mut self) -> bool {
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
            Drag::Input { unit, span: (a0, a1) } => {
                if let Some(i) = self.input_index(x, y) {
                    let mut states = self.chat.borrow_mut();
                    let d = &mut states.entry(sid).or_default().draft;
                    let s = d.span(i, unit);
                    (d.anchor, d.caret) = if s.0 < a0 { (a1, s.0) } else { (a0, s.1) };
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

    /// The press let go: a drag ends, and a transcript selection that covers nothing goes. Whether a chat was dragged in.
    pub(super) fn chat_pointer_up(&mut self) -> bool {
        let mut any = false;
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
        let open = states.get(&sid).map(|s| s.open.clone()).unwrap_or_default();
        rows(chats.get(&sid).unwrap_or(&empty), &open)
    }
}

/// Fill one transcript row from what it shows.
fn fill_row(row: View<'_>, r: &Row) {
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
        Row::Tool { block, name, arg, state, open, input, result } => {
            if let Some(h) = row.part("head") {
                h.action(&format!("chat:open:{block}"));
                head(h, if *open { OPENED } else { CLOSED }, name, arg, state);
            }
            if let Some(b) = row.part("body") {
                if let Some(i) = b.part("input") {
                    i.text(input);
                }
                if let Some(o) = b.part("result") {
                    o.text(tool_result(result));
                }
            }
        }
        Row::Thinking { block, open, text: s, live } => {
            if let Some(h) = row.part("head") {
                h.action(&format!("chat:open:{block}"));
                let first = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                head(h, if *open { OPENED } else { CLOSED }, if *live { "Thinking\u{2026}" } else { "Thought" }, if *open { "" } else { first }, "");
            }
            text("text", s.trim());
        }
        Row::Card { block, icon, words, detail, state, open, .. } => {
            if let Some(h) = row.part("head") {
                h.action(&format!("chat:open:{block}"));
                if let Some(p) = h.part("disclose") {
                    p.icon(if *open { OPENED } else { CLOSED });
                }
                if let Some(p) = h.part("icon") {
                    p.icon(icon);
                }
                for (part, s) in [("words", words), ("detail", detail), ("state", state)] {
                    if let Some(p) = h.part(part) {
                        p.text(s);
                    }
                }
            }
        }
        Row::Step { icon, words, detail, .. } => {
            if let Some(p) = row.part("icon") {
                p.icon(icon);
            }
            text("words", words);
            text("detail", detail);
        }
        Row::Grid { v, cap } => fill_grid(row, v, *cap),
        Row::Ui { v } => genui::fill(row, v),
        Row::Output { text: s, .. } => {
            if let Some(t) = row.part("card").and_then(|c| c.part("text")) {
                t.text(s);
            }
        }
        Row::CodeToggle { block, open } => {
            if let Some(h) = row.part("head") {
                h.action(&format!("chat:open:{}", code_key(block)));
                if let Some(p) = h.part("disclose") {
                    p.icon(if *open { OPENED } else { CLOSED });
                }
                if let Some(p) = h.part("words") {
                    p.text(if *open { "Hide code" } else { "Show code" });
                }
            }
        }
    }
}

/// A shown table, record or list (mcp/src/shown.ts) in a `ui.Table`, Nushell's way: a table gets an index column and a
/// header, a record its keys beside its values, a list an index; numbers sit at the end, nulls are dim, links in the
/// accent; a cell cut short shows its whole text on hover. A foot says how many rows there are, and how many are shown.
fn fill_grid(row: View<'_>, v: &Value, cap: usize) {
    let kind = v["t"].as_str().unwrap_or("");
    let cols: Vec<(&str, &str)> =
        v["cols"].as_array().into_iter().flatten().map(|c| (c["name"].as_str().unwrap_or(""), c["type"].as_str().unwrap_or(""))).collect();
    let rows: &[Value] = v["rows"].as_array().map_or(&[], Vec::as_slice);
    let total = v["total"].as_u64().unwrap_or(rows.len() as u64) as usize;
    let shown = rows.len().min(cap);
    let index = kind != "record";
    let width = |c: usize| rows[..shown].iter().map(|r| cell_text(&r[c]).chars().count()).max().unwrap_or(0);
    let mut columns = Vec::new();
    if index {
        columns.push(view::TableColumn { name: "#", chars: total.saturating_sub(1).to_string().len().max(1), end: true });
    }
    for (c, (name, ty)) in cols.iter().enumerate() {
        let head = if kind == "table" { name.chars().count() } else { 0 };
        columns.push(view::TableColumn { name, chars: width(c).max(head), end: *ty == "number" });
    }
    let off = usize::from(index);
    if let Some(grid) = row.part("card").and_then(|c| c.part("grid")) {
        let head = (kind == "table").then_some("hn.ChatHead");
        let prefab = |r: u32, c: usize| {
            if c < off || (kind == "record" && c == 0) {
                return "hn.ChatCellDim";
            }
            match &rows[r as usize][c - off] {
                Value::Null => "hn.ChatCellDim",
                Value::String(_) if cols[c - off].1 == "url" => "hn.ChatCellLink",
                _ => "hn.ChatCell",
            }
        };
        view::table(grid, &columns, shown as u32, head, prefab, |cell, r, c| {
            let s = if c < off { r.to_string() } else { cell_text(&rows[r as usize][c - off]) };
            if let Some(t) = cell.part("text") {
                t.text(&s);
            }
            // The whole text on hover when the column cuts it.
            let cut = s.chars().count() > columns[c].chars.min(view::TABLE_MAX_CHARS) || s.ends_with('\u{2026}');
            let tip = cell.entity().try_cloned::<&Tooltip>();
            if cut && tip.as_ref().is_none_or(|t| t.text.as_str() != s) {
                cell.set(Tooltip { text: s.as_str().into() });
            } else if !cut && tip.is_some() {
                cell.entity().remove(Tooltip::id());
            }
        });
    }
    if let Some(f) = row.part("foot") {
        let unit = if kind == "record" { "field" } else if kind == "list" { "item" } else { "row" };
        let s = if total == 1 { "" } else { "s" };
        f.text(&if shown < total { format!("{shown} of {total} {unit}s") } else { format!("{total} {unit}{s}") });
    }
}

/// A tool call's or thinking's line.
fn head(h: View<'_>, icon: &str, name: &str, arg: &str, state: &str) {
    if let Some(p) = h.part("disclose") {
        p.icon(icon);
    }
    for (part, s) in [("name", name), ("arg", arg), ("state", state)] {
        if let Some(p) = h.part(part) {
            p.text(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_splits_into_rows() {
        let mut out = Vec::new();
        blocks("# Title\n\nSome *words*\nwrapped.\n\n- one\n  more\n- two\n\n```ts\nconst x = 1;\n```\n> quoted", &mut out);
        let kinds: Vec<&str> = out.iter().map(Row::prefab).collect();
        assert_eq!(kinds, ["hn.ChatH1", "hn.ChatProse", "hn.ChatItem", "hn.ChatItem", "hn.ChatCode", "hn.ChatQuote"]);
        assert!(matches!(&out[1], Row::Prose(s) if s == "Some *words* wrapped."));
        assert!(matches!(&out[2], Row::ListItem { text, .. } if text == "one more"));
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
