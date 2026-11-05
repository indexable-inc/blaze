//! Syntax highlighting for the editor: a small Rust lexer, enough to colour one file the way
//! the Islands Dark scheme does (keywords, literals, comments, attributes, function names,
//! macros, constants, fields, lifetimes). It works on any text, half-typed code included. The
//! egui LayoutJob is cached by text hash, and built synchronously: the frame that shows new
//! text shows its colours.

use std::hash::{DefaultHasher, Hash, Hasher};

use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{Color32, FontId};

use crate::theme::Theme;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Plain,
    Comment,
    Doc,
    Keyword,
    Number,
    Str,
    Function,
    Macro,
    Constant,
    Field,
    Lifetime,
    Attribute,
}

const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "yield",
];

const PRIMITIVES: &[&str] = &[
    "bool", "char", "str", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64",
    "i128", "isize", "f32", "f64",
];

/// The single mapping from roles to theme colours: (colour, italic).
fn style(t: &Theme, role: Role) -> (Color32, bool) {
    match role {
        Role::Plain => (t.fg, false),
        Role::Comment => (t.comment, false),
        Role::Doc => (t.doc_comment, true),
        Role::Keyword => (t.keyword, false),
        Role::Number => (t.number, false),
        Role::Str => (t.string, false),
        Role::Function => (t.function, false),
        Role::Macro => (t.macro_call, false),
        Role::Constant => (t.constant, true),
        Role::Field => (t.constant, false),
        Role::Lifetime => (t.type_param, false),
        Role::Attribute => (t.attribute, false),
    }
}

/// Splits `text` into (byte range, role) spans that cover it end to end.
fn lex(text: &str) -> Vec<(usize, usize, Role)> {
    let b = text.as_bytes();
    let n = b.len();
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let ident_start = |c: u8| c.is_ascii_alphabetic() || c == b'_' || c >= 0x80;
    let ident_cont = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
    let mut out = Vec::new();
    let mut i = 0;
    // The last identifier or keyword, for `fn name`.
    let mut last_word = "";
    let mut prev_dot = false;
    while i < n {
        let start = i;
        let c = b[i];
        let role = if c == b'/' && at(i + 1) == b'/' {
            let doc = (at(i + 2) == b'/' && at(i + 3) != b'/') || at(i + 2) == b'!';
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            if doc { Role::Doc } else { Role::Comment }
        } else if c == b'/' && at(i + 1) == b'*' {
            let doc = matches!(at(i + 2), b'*' | b'!') && at(i + 3) != b'/';
            i += 2;
            let mut depth = 1;
            while i < n && depth > 0 {
                if b[i] == b'/' && at(i + 1) == b'*' {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && at(i + 1) == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if doc { Role::Doc } else { Role::Comment }
        } else if c == b'"' || (c == b'b' && at(i + 1) == b'"') {
            i += if c == b'b' { 2 } else { 1 };
            while i < n && b[i] != b'"' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(n);
            Role::Str
        } else if (c == b'r' || (c == b'b' && at(i + 1) == b'r')) && {
            let j = i + if c == b'b' { 2 } else { 1 };
            let mut k = j;
            while at(k) == b'#' {
                k += 1;
            }
            at(k) == b'"'
        } {
            i += if c == b'b' { 2 } else { 1 };
            let mut hashes = 0;
            while at(i) == b'#' {
                hashes += 1;
                i += 1;
            }
            i += 1;
            loop {
                if i >= n {
                    break;
                }
                if b[i] == b'"' && (1..=hashes).all(|h| at(i + h) == b'#') {
                    i += 1 + hashes;
                    break;
                }
                i += 1;
            }
            Role::Str
        } else if c == b'\'' {
            // A char literal ('a', '\n', '\u{..}') or a lifetime / label ('a).
            if at(i + 1) == b'\\' {
                i += 2;
                while i < n && b[i] != b'\'' && b[i] != b'\n' {
                    i += 1;
                }
                i = (i + 1).min(n);
                Role::Str
            } else {
                let ch_len = text[i + 1..].chars().next().map_or(0, char::len_utf8);
                if ch_len > 0 && at(i + 1 + ch_len) == b'\'' {
                    i += 2 + ch_len;
                    Role::Str
                } else if ident_start(at(i + 1)) {
                    i += 1;
                    while i < n && ident_cont(b[i]) {
                        i += 1;
                    }
                    Role::Lifetime
                } else {
                    i += 1;
                    Role::Plain
                }
            }
        } else if c.is_ascii_digit() {
            while i < n && (ident_cont(b[i]) || (b[i] == b'.' && at(i + 1).is_ascii_digit())) {
                i += 1;
            }
            Role::Number
        } else if c == b'#' && (at(i + 1) == b'[' || (at(i + 1) == b'!' && at(i + 2) == b'[')) {
            let mut depth = 0;
            while i < n {
                match b[i] {
                    b'[' => depth += 1,
                    b']' => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            Role::Attribute
        } else if ident_start(c) {
            while i < n && ident_cont(b[i]) {
                i += 1;
            }
            let word = &text[start..i];
            let role = if at(i) == b'!' && at(i + 1) != b'=' {
                i += 1;
                Role::Macro
            } else if KEYWORDS.contains(&word) || PRIMITIVES.contains(&word) {
                Role::Keyword
            } else if last_word == "fn"
                || at(i) == b'('
                || (at(i) == b':' && at(i + 1) == b':' && at(i + 2) == b'<')
            {
                Role::Function
            } else if word.bytes().any(|c| c.is_ascii_uppercase())
                && !word.bytes().any(|c| c.is_ascii_lowercase())
            {
                Role::Constant
            } else if prev_dot {
                Role::Field
            } else {
                Role::Plain
            };
            last_word = &text[start..start + word.len()];
            prev_dot = false;
            out.push((start, i, role));
            continue;
        } else {
            // Punctuation, operators and whitespace: one char at a time.
            i += text[i..].chars().next().map_or(1, char::len_utf8);
            if !c.is_ascii_whitespace() {
                prev_dot = c == b'.' && at(i) != b'.';
                if c != b'.' {
                    last_word = "";
                }
            }
            Role::Plain
        };
        if role != Role::Plain {
            last_word = "";
            prev_dot = false;
        }
        out.push((start, i.min(n), role));
    }
    out
}

fn build(t: &Theme, text: &str, font: &FontId) -> LayoutJob {
    let fmt = |(color, italics): (Color32, bool)| TextFormat {
        font_id: font.clone(),
        color,
        italics,
        line_height: Some(t.metrics.line_height),
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    // Merge runs of one role so the job has few sections.
    let mut run: Option<(usize, usize, Role)> = None;
    for (s, e, role) in lex(text) {
        match &mut run {
            Some((_, end, r)) if *r == role && *end == s => *end = e,
            _ => {
                if let Some((s0, e0, r0)) = run.take() {
                    job.append(&text[s0..e0], 0.0, fmt(style(t, r0)));
                }
                run = Some((s, e, role));
            }
        }
    }
    if let Some((s0, e0, r0)) = run {
        job.append(&text[s0..e0], 0.0, fmt(style(t, r0)));
    }
    if text.is_empty() {
        job.append("", 0.0, fmt(style(t, Role::Plain)));
    }
    job
}

/// Cached by (text, font) hash.
#[derive(Default)]
pub struct Highlighter {
    key: Option<u64>,
    job: LayoutJob,
}

impl Highlighter {
    pub fn job(&mut self, t: &Theme, text: &str, font: FontId, wrap_width: f32) -> LayoutJob {
        let mut h = DefaultHasher::new();
        text.hash(&mut h);
        font.hash(&mut h);
        let key = h.finish();
        if self.key != Some(key) {
            self.job = build(t, text, &font);
            self.key = Some(key);
        }
        let mut job = self.job.clone();
        job.wrap.max_width = wrap_width;
        job
    }
}
