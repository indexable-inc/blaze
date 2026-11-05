//! The chat pane (`hn.ChatPane`, scene/widgets/chat.flecs): a chat session (the daemon's chat mode: an agent over
//! structured events, not a terminal) drawn as a chat. The transcript comes from the model's [`model::chat::Transcript`]
//! (agent-neutral events); each of its items becomes rows (a user message, the agent's markdown split into paragraphs,
//! headings, list items, quotes and code blocks, thinking and tool calls folded to a line, a turn's foot) in a
//! `ui.LazyList`, so only the rows in view are entities. It scrolls on AppKit's physics and sticks to the bottom while
//! the agent writes unless the person scrolled up. The input is a `ui.TextArea` edited here (`Draft`, keys in
//! scene/20-keys.flecs ctx.Chat): Return sends, Shift+Return breaks the line, Escape interrupts.
use super::*;
use harness_native_scroll_physics::{Engine, Input, Mutation, Phase, Vec2};
use harness_native_ui_layout_api::ScrollState;
use harness_native_ui_text_api::{Highlight, TextEdit};
use model::chat::{Item, Transcript};
use std::collections::HashSet;

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

    /// The same column (in characters) on the line above (`up`) or below, or the text's end.
    fn vertical(&self, i: usize, up: bool) -> usize {
        let start = self.line_start(i);
        let col = self.text[start..i].chars().count();
        let (from, to) = if up {
            if start == 0 {
                return 0;
            }
            (self.line_start(start - 1), start - 1)
        } else {
            let end = self.line_end(i);
            if end == self.text.len() {
                return end;
            }
            (end + 1, self.line_end(end + 1))
        };
        self.text[from..to].char_indices().nth(col).map_or(to, |(k, _)| from + k)
    }

    fn target(&self, m: Move) -> usize {
        let i = self.caret;
        match m {
            Move::Left => self.prev(i),
            Move::Right => self.next(i),
            Move::Up => self.vertical(i, true),
            Move::Down => self.vertical(i, false),
            Move::WordLeft => self.word_left(i),
            Move::WordRight => self.word_right(i),
            Move::LineStart => self.line_start(i),
            Move::LineEnd => self.line_end(i),
            Move::DocStart => 0,
            Move::DocEnd => self.text.len(),
        }
    }

    /// Move the caret; with `select` the selection's other end stays. A plain left or right over a selection collapses it
    /// to that side, as AppKit does.
    fn go(&mut self, m: Move, select: bool) {
        let (a, b) = self.sel();
        if !select && a != b && matches!(m, Move::Left | Move::Right) {
            self.caret = if m == Move::Left { a } else { b };
        } else {
            self.caret = self.target(m);
        }
        if !select {
            self.anchor = self.caret;
        }
    }

    /// Erase the selection, or from the caret to where `m` goes.
    fn erase(&mut self, m: Move) {
        if self.anchor == self.caret {
            self.anchor = self.target(m);
        }
        self.insert("");
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
    Thinking { block: String, open: bool, text: String, live: bool },
    Note(String),
    Error(String),
    Foot(String),
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
            Row::Thinking { open: true, .. } => "hn.ChatThinkingOpen",
            Row::Thinking { .. } => "hn.ChatThinking",
            Row::Note(_) => "hn.ChatNote",
            Row::Error(_) => "hn.ChatError",
            Row::Foot(_) => "hn.ChatFoot",
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
            Item::TurnEnd { stop, duration_ms, cost_usd, .. } => {
                let mut bits = Vec::new();
                if let Some(ms) = duration_ms {
                    bits.push(if *ms >= 60_000 {
                        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
                    } else {
                        format!("{:.1}s", *ms as f64 / 1000.0)
                    });
                }
                if let Some(c) = cost_usd.filter(|c| *c > 0.0) {
                    bits.push(if c < 0.01 { format!("${c:.4}") } else { format!("${c:.2}") });
                }
                if stop == "interrupted" {
                    bits.push("interrupted".into());
                }
                part.push(Row::Foot(bits.join(" \u{00B7} ")));
            }
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

impl App {
    /// Whether the terminal's ground (hn.Main's Fill, `tone_term_ground`) is dark in the window's appearance.
    fn term_ground_dark(&self) -> bool {
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
            let fresh_rows = |row: View<'_>, i: usize, new: bool| {
                if new {
                    row.set(HoverAmt::default());
                    if let Some(h) = row.part("head") {
                        h.set(HoverAmt::default());
                    }
                }
                fill_row(row, &rows[i].1);
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
                h.text(if shown.is_empty() { "Message Claude\u{2026}" } else { "" });
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
                    area.entity().set(ScrollState { x: 0.0, y });
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
                list.entity().set(ScrollState { x: 0.0, y });
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

    /// Open or fold a tool call's or thinking's row.
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
            self.with_draft(|d| d.go(m, select));
            return;
        }
        match op {
            "backspace" => _ = self.with_draft(|d| d.erase(Move::Left)),
            "delete" => _ = self.with_draft(|d| d.erase(Move::Right)),
            "word_erase" => _ = self.with_draft(|d| d.erase(Move::WordLeft)),
            "word_delete" => _ = self.with_draft(|d| d.erase(Move::WordRight)),
            "line_erase" => _ = self.with_draft(|d| d.erase(Move::LineStart)),
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

    /// The selection of the focused chat's input, for Command-C.
    pub(super) fn chat_copy(&self) -> Option<Option<String>> {
        let (_, _, sid) = self.focused_chat()?;
        Some(self.chat.borrow().get(&sid).map(|s| s.draft.selected().to_owned()).filter(|s| !s.is_empty()))
    }

    /// Command-A in the focused chat's input. Whether a chat took it.
    pub(super) fn chat_select_all(&mut self) -> bool {
        self.with_draft(|d| {
            d.anchor = 0;
            d.caret = d.text.len();
        })
    }

    /// Where the focused chat's input is (window points), for the input method's candidate window: its last line.
    pub(super) fn chat_caret_rect(&self) -> Option<Option<[f32; 4]>> {
        let (p, ..) = self.focused_chat()?;
        let w = self.world.world_ref();
        let txt = View::of(w.entity_from_id(p)).part("foot")?.part("input")?.part("area")?.part("text")?;
        let l = txt.entity().try_cloned::<&Laid>()?;
        let z = self.zoom();
        let line = 20.0;
        Some(Some([(l.x + l.w.min(4.0)) * z, (l.y + l.h - line) * z, 2.0 * z, line * z]))
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
        Row::Prose(s) | Row::Heading(_, s) | Row::Quote(s) | Row::Note(s) | Row::Error(s) | Row::Foot(s) => text("text", s),
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
                    o.text(if result.is_empty() { "\u{2026}" } else { result.trim_end() });
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
        let mut d = Draft::default();
        d.insert("hello world");
        d.go(Move::WordLeft, false);
        assert_eq!(d.caret, 6);
        d.go(Move::WordRight, true);
        assert_eq!(d.selected(), "world");
        d.insert("there");
        assert_eq!(d.text, "hello there");
        d.erase(Move::WordLeft);
        assert_eq!(d.text, "hello ");
        d.insert("\nab");
        d.go(Move::Up, false);
        assert_eq!(d.caret, 2);
    }
}
