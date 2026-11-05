//! The one renderer of a [`Report`]: a pure function of the report, what the terminal can do
//! ([`Caps`]) and the directories paths are shown from ([`Roots`]). Nothing here reads the
//! environment or a clock; [`crate::terminal`] works out the caps.
//!
//! On a terminal (colour): the severity in its colour, names bold, metadata (versions, paths,
//! hashes, counts, the box) dim, code and commands as a code span (red-ish on a dim
//! background), the rest in the terminal's own colour; never a whole line one colour. Without
//! colour (a pipe, CI, `NO_COLOR`), code is in backticks and nothing else changes, so a log
//! reads the same. With a width, the lines of the box wrap at words; a command never wraps,
//! so it pastes whole.

use std::borrow::Cow;

use unicode_width::UnicodeWidthStr as _;

use crate::{Report, Severity, Span, Text};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
/// The severities' colours: cargo's, bold and bright.
const ERROR: &str = "\x1b[1m\x1b[91m";
const WARN: &str = "\x1b[1m\x1b[93m";
const NOTE: &str = "\x1b[1m\x1b[92m";
/// A code span: red-ish text on a dim background, as a notebook shows inline code.
const CODE: &str = "\x1b[38;5;203m\x1b[48;5;236m";

/// What the terminal a report is rendered for can do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Caps {
    /// ANSI colour and styles.
    pub color: bool,
    /// OSC 8 hyperlinks, for paths.
    pub links: bool,
    /// The columns to wrap the box's lines at; `None` never wraps.
    pub width: Option<u16>,
}

impl Caps {
    /// A pipe, a file, CI: no escape of any kind, nothing wrapped.
    pub const PLAIN: Caps = Caps { color: false, links: false, width: None };
}

/// The directories a path is shown from: below the workspace's root as a relative path, else
/// below home as `~/...`, else whole.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Roots<'a> {
    pub workspace: Option<&'a str>,
    pub home: Option<&'a str>,
}

impl Roots<'static> {
    pub const NONE: Roots<'static> = Roots { workspace: None, home: None };
}

/// `report` rendered for `caps`, each line ending in a newline.
pub fn render(report: &Report, caps: Caps, roots: &Roots<'_>) -> String {
    let mut out = String::with_capacity(256); // alloc: a report's text, once
    render_into(&mut out, report, caps, roots);
    out
}

/// [`render`] at the end of `out`.
pub fn render_into(out: &mut String, report: &Report, caps: Caps, roots: &Roots<'_>) {
    let style = match report.severity {
        Severity::Error => ERROR,
        Severity::Warning => WARN,
        Severity::Note => NOTE,
    };
    painted(out, caps, style, report.severity.label());
    out.push_str(": ");
    headline(out, report, caps, roots);
    body(out, report, caps, roots, 0);
}

/// `report` as one line of JSON, without its newline: every field, nothing shortened.
pub fn json(report: &Report) -> String {
    // A report keys no map by anything but text and holds no float: it always has JSON.
    sonic_rs::to_string(report).expect("a report has a JSON rendering")
}

/// `report` as a `--message-format=json` line, without its newline:
/// `{"reason":"blaze-report","report":{...}}`.
pub fn json_message(report: &Report) -> String {
    #[derive(serde::Serialize)]
    struct Message<'a> {
        reason: &'static str,
        report: &'a Report,
    }
    sonic_rs::to_string(&Message { reason: "blaze-report", report })
        .expect("a report has a JSON rendering")
}

/// The headline, its rule in brackets, and the newline.
fn headline(out: &mut String, report: &Report, caps: Caps, roots: &Roots<'_>) {
    inline(out, &report.headline, caps, roots);
    if let Some(rule) = &report.rule {
        out.push_str("  ");
        styled(out, caps, DIM, |out| {
            out.push('[');
            match rule.split_once(':') {
                Some((area, name)) => {
                    out.push_str(area);
                    out.push_str(": ");
                    out.push_str(name);
                }
                None => out.push_str(rule),
            }
            out.push(']');
        });
    }
    out.push('\n');
}

/// The box (where, the facts, what it blocks), the causes, the fixes and the footer, each line
/// `indent` columns in.
fn body(out: &mut String, report: &Report, caps: Caps, roots: &Roots<'_>, indent: usize) {
    let blocks = report.blocks.as_ref().map(crate::Blocks::text);
    // alloc: a report's lines, once
    let items: Vec<&Text> = report
        .at
        .iter()
        .chain(report.facts.iter().map(|it| &it.text))
        .chain(blocks.iter())
        .collect();
    for (at, text) in items.iter().enumerate() {
        let last = at + 1 == items.len();
        let glyph = match (items.len(), at) {
            (1, _) => "─",
            (_, 0) => "┌",
            _ if last => "└",
            _ => "│",
        };
        boxed(out, text, caps, roots, indent, glyph, if last { " " } else { "│" });
    }
    for cause in &report.causes {
        spaces(out, indent + 2);
        painted(out, caps, DIM, "caused by");
        out.push_str(": ");
        headline(out, cause, caps, roots);
        body(out, cause, caps, roots, indent + 2);
    }
    fixes(out, report, caps, roots, indent);
    footer(out, report, caps, roots, indent);
}

/// One line of the box, `glyph` before it and `rest` before each line it wraps onto.
fn boxed(
    out: &mut String,
    text: &Text,
    caps: Caps,
    roots: &Roots<'_>,
    indent: usize,
    glyph: &str,
    rest: &str,
) {
    spaces(out, indent + 2);
    painted(out, caps, DIM, glyph);
    out.push(' ');
    let Some(width) = caps.width else {
        inline(out, text, caps, roots);
        out.push('\n');
        return;
    };
    // The words wrap at the width less the margin, and never below 20 columns.
    let room = (width as usize).saturating_sub(indent + 4).max(20);
    let mut atoms = Atoms::default();
    atoms.of(text, caps, roots);
    let mut column = 0;
    let mut at = 0;
    while at < atoms.list.len() {
        // A group: an atom and every one glued to it (no space between), never split.
        let end = (at + 1..atoms.list.len())
            .find(|&it| atoms.list[it].space > 0)
            .unwrap_or(atoms.list.len());
        let group: usize = atoms.list[at..end].iter().map(|it| it.width).sum();
        let mut space = atoms.list[at].space;
        if column > 0 && column + space + group > room {
            out.push('\n');
            spaces(out, indent + 2);
            match rest {
                " " => out.push(' '),
                _ => painted(out, caps, DIM, rest),
            }
            out.push(' ');
            column = 0;
            space = 0;
        }
        spaces(out, space);
        for atom in &atoms.list[at..end] {
            out.push_str(&atoms.text[atom.start..atom.end]);
        }
        column += space + group;
        at = end;
    }
    out.push('\n');
}

/// `fix   <command>`, ` or   <command>`: each alternative, its commands under each other.
fn fixes(out: &mut String, report: &Report, caps: Caps, roots: &Roots<'_>, indent: usize) {
    if report.fixes.is_empty() {
        return;
    }
    out.push('\n');
    for (at, fix) in report.fixes.iter().enumerate() {
        spaces(out, indent + 2);
        painted(out, caps, DIM, if at == 0 { "fix" } else { " or" });
        out.push_str("   ");
        let mut first = true;
        if let Some(note) = &fix.note {
            inline(out, note, caps, roots);
            out.push('\n');
            first = false;
        }
        for line in &fix.lines {
            if !first {
                spaces(out, indent + 8);
            }
            first = false;
            // A command is pasted: no backticks around it, ever.
            match caps.color {
                true => painted(out, caps, CODE, line),
                false => out.push_str(line),
            }
            out.push('\n');
        }
        if first {
            out.push('\n');
        }
    }
}

/// `log   <path>      more: blaze explain <name>`, or `more  blaze explain <name>`.
fn footer(out: &mut String, report: &Report, caps: Caps, roots: &Roots<'_>, indent: usize) {
    // The package's full list of what it blocks when the box names only some, else the rule's
    // long text.
    let more = match (&report.blocks, report.subject()) {
        (Some(blocks), Some(subject)) if blocks.more() => Some(subject),
        _ => report.rule.as_deref(),
    };
    if report.log.is_none() && more.is_none() {
        return;
    }
    out.push('\n');
    spaces(out, indent + 2);
    let explain = |out: &mut String, name: &str| {
        let command = format!("blaze explain {name}"); // alloc: a report's, once
        match caps.color {
            true => painted(out, caps, CODE, &command),
            false => out.push_str(&command),
        }
    };
    match (&report.log, more) {
        (Some(log), more) => {
            painted(out, caps, DIM, "log");
            out.push_str("   ");
            path(out, log, caps, roots);
            if let Some(name) = more {
                out.push_str("      ");
                painted(out, caps, DIM, "more:");
                out.push(' ');
                explain(out, name);
            }
        }
        (None, Some(name)) => {
            painted(out, caps, DIM, "more");
            out.push_str("  ");
            explain(out, name);
        }
        (None, None) => {}
    }
    out.push('\n');
}

/// `text` on the current line, as it is: nothing wrapped, every space kept.
fn inline(out: &mut String, text: &Text, caps: Caps, roots: &Roots<'_>) {
    for span in &text.0 {
        piece(out, span, caps, roots);
    }
}

/// One span, styled for `caps`; its width on the terminal is that of the words it wrote
/// outside escapes.
fn piece(out: &mut String, span: &Span, caps: Caps, roots: &Roots<'_>) {
    match span {
        Span::Plain(words) => out.push_str(words),
        Span::Code(code) | Span::Var(code) => match caps.color {
            true => painted(out, caps, CODE, code),
            false => {
                out.push('`');
                out.push_str(code);
                out.push('`');
            }
        },
        Span::Path(it) => path(out, it, caps, roots),
        Span::Package { name, version } => {
            painted(out, caps, BOLD, name);
            if let Some(version) = version {
                out.push(' ');
                painted(out, caps, DIM, version);
            }
        }
        Span::Version(version) => painted(out, caps, DIM, version),
        Span::Hash(hash) => {
            let cut = hash.char_indices().nth(8).map_or(hash.len(), |(at, _)| at);
            painted(out, caps, DIM, &hash[..cut]);
        }
        Span::Count(count) => styled(out, caps, DIM, |out| crate::push_number(out, *count)),
        Span::Took(ns) => styled(out, caps, DIM, |out| took_into(out, *ns)),
    }
}

/// A path as shown from `roots`, dim, and a link to the file where the terminal has links.
fn path(out: &mut String, path: &str, caps: Caps, roots: &Roots<'_>) {
    let shown = shown(path, roots);
    let target = match (path.starts_with('/'), roots.workspace) {
        (true, _) => Some(Cow::Borrowed(path)),
        (false, Some(workspace)) => {
            Some(Cow::Owned(format!("{}/{path}", workspace.trim_end_matches('/'))))
        } // alloc: a report's, once
        (false, None) => None,
    };
    let link = caps.links && target.is_some();
    if let (true, Some(target)) = (link, &target) {
        out.push_str("\x1b]8;;file://");
        for byte in target.bytes() {
            match byte {
                b'%' | b' ' | 0..=0x1f | 0x7f.. => {
                    const HEX: &[u8; 16] = b"0123456789ABCDEF";
                    out.push('%');
                    out.push(HEX[(byte >> 4) as usize] as char);
                    out.push(HEX[(byte & 0xf) as usize] as char);
                }
                _ => out.push(byte as char),
            }
        }
        out.push_str("\x1b\\");
    }
    painted(out, caps, DIM, &shown);
    if link {
        out.push_str("\x1b]8;;\x1b\\");
    }
}

/// `path` from the workspace's root (`.` for the root), else from home as `~/...`, else whole.
pub(crate) fn shown<'p>(path: &'p str, roots: &Roots<'_>) -> Cow<'p, str> {
    if let Some(rest) = roots.workspace.and_then(|root| below(path, root)) {
        return Cow::Borrowed(if rest.is_empty() { "." } else { rest });
    }
    if let Some(rest) = roots.home.and_then(|home| below(path, home)) {
        return match rest.is_empty() {
            true => Cow::Borrowed("~"),
            false => Cow::Owned(format!("~/{rest}")), // alloc: a report's, once
        };
    }
    Cow::Borrowed(path)
}

/// What of `path` is below `dir`: `""` for `dir` itself, `None` for a path outside it.
fn below<'p>(path: &'p str, dir: &str) -> Option<&'p str> {
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        return None;
    }
    let rest = path.strip_prefix(dir)?;
    match rest.strip_prefix('/') {
        Some(rest) => Some(rest),
        None if rest.is_empty() => Some(rest),
        None => None,
    }
}

/// A time: `0.12s` below 10 seconds, `38s` below a minute, `1m12s`, `2h05m`.
pub(crate) fn took_into(out: &mut String, ns: u64) {
    use std::fmt::Write as _;
    let secs = ns / 1_000_000_000;
    // Writing to a String cannot fail.
    let _ = match secs {
        0..10 => write!(out, "{secs}.{:02}s", (ns % 1_000_000_000) / 10_000_000),
        10..60 => write!(out, "{secs}s"),
        60..3600 => write!(out, "{}m{:02}s", secs / 60, secs % 60),
        _ => write!(out, "{}h{:02}m", secs / 3600, (secs % 3600) / 60),
    };
}

/// `text` in `style` where there is colour.
fn painted(out: &mut String, caps: Caps, style: &str, text: &str) {
    styled(out, caps, style, |out| out.push_str(text));
}

/// What `write` writes, in `style` where there is colour.
fn styled(out: &mut String, caps: Caps, style: &str, write: impl FnOnce(&mut String)) {
    if caps.color {
        out.push_str(style);
    }
    write(out);
    if caps.color {
        out.push_str(RESET);
    }
}

fn spaces(out: &mut String, count: usize) {
    out.extend(std::iter::repeat_n(' ', count));
}

/// A text cut at its spaces for wrapping: each atom a word or a styled span, the spaces
/// before it, and its width on the terminal.
#[derive(Default)]
struct Atoms {
    text: String,
    list: Vec<Atom>,
}

struct Atom {
    space: usize,
    start: usize,
    end: usize,
    width: usize,
}

impl Atoms {
    fn of(&mut self, text: &Text, caps: Caps, roots: &Roots<'_>) {
        let mut space = 0;
        for span in &text.0 {
            match span {
                Span::Plain(words) => {
                    let mut rest = words.as_str();
                    loop {
                        let word = rest.trim_start_matches(' ');
                        space += rest.len() - word.len();
                        if word.is_empty() {
                            break;
                        }
                        let end = word.find(' ').unwrap_or(word.len());
                        let start = self.text.len();
                        self.text.push_str(&word[..end]);
                        // alloc: a report's atoms, once
                        self.list.push(Atom {
                            space,
                            start,
                            end: self.text.len(),
                            width: word[..end].width(),
                        });
                        space = 0;
                        rest = &word[end..];
                    }
                }
                span => {
                    let start = self.text.len();
                    piece(
                        &mut self.text,
                        span,
                        Caps { color: caps.color, links: caps.links, width: None },
                        roots,
                    );
                    let width = visible_width(&self.text[start..]);
                    // alloc: as above
                    self.list.push(Atom { space, start, end: self.text.len(), width });
                    space = 0;
                }
            }
        }
    }
}

/// The columns `text` takes on a terminal, its escapes (CSI to the final byte, OSC to ST or
/// BEL) taking none.
fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut rest = text;
    while let Some(at) = rest.find('\x1b') {
        width += rest[..at].width();
        let after = &rest[at + 1..];
        rest = match after.as_bytes().first() {
            Some(b'[') => {
                let params = &after[1..];
                params
                    .find(|c: char| ('\x40'..='\x7e').contains(&c))
                    .map_or("", |end| &params[end + 1..])
            }
            Some(b']') => match (after.find('\x07'), after.find("\x1b\\")) {
                (Some(bel), Some(st)) if bel < st => &after[bel + 1..],
                (_, Some(st)) => &after[st + 2..],
                (Some(bel), None) => &after[bel + 1..],
                (None, None) => "",
            },
            _ => after,
        };
    }
    width + rest.width()
}
