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
/// A code span and a command, as the palette gives them to a terminal of 16 colours: a
/// report's renderer is told only whether there is colour ([`Caps`]), so it takes the values
/// every terminal has.
const CODE: &str = "\x1b[1m";
const COMMAND: &str = "\x1b[1;4m";

/// `text` as one line for `caps`, its paths shown from `roots`: a message outside a report's
/// box.
pub fn text_into(out: &mut String, text: &crate::Text, caps: Caps, roots: &Roots<'_>) {
    for span in &text.0 {
        piece(out, span, caps, roots);
    }
}

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
            match atom.what {
                Piece::Word(word) => out.push_str(word),
                Piece::Span(span) => piece(out, span, Atoms::caps(caps), roots),
            }
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
        Span::Command(command) => match caps.color {
            true => painted(out, caps, COMMAND, command),
            false => {
                out.push('`');
                out.push_str(command);
                out.push('`');
            }
        },
        Span::Strong(words) => painted(out, caps, BOLD, words),
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
/// before it, and its width on the terminal. A word is the text's own; a span is rendered
/// once into `scratch` to be measured and again where it is written, so no atom is an index
/// into a buffer.
#[derive(Default)]
struct Atoms<'t> {
    /// The span being measured, cleared for each.
    scratch: String,
    list: Vec<Atom<'t>>,
}

struct Atom<'t> {
    space: usize,
    what: Piece<'t>,
    width: usize,
}

/// What an atom writes: a word of a plain span, or a whole styled span.
#[derive(Clone, Copy)]
enum Piece<'t> {
    Word(&'t str),
    Span(&'t Span),
}

impl<'t> Atoms<'t> {
    /// The caps a span of the box is rendered with, measured and written alike: never wrapped
    /// inside.
    fn caps(caps: Caps) -> Caps {
        Caps { width: None, ..caps }
    }

    fn of(&mut self, text: &'t Text, caps: Caps, roots: &Roots<'_>) {
        // The spaces before the next atom; those at the end of a span carry into the next.
        let mut space = 0;
        for span in &text.0 {
            match span {
                Span::Plain(words) => {
                    // Each `' '` is one column of space; the empty pieces between two in a row
                    // are no word.
                    for (at, word) in words.split(' ').enumerate() {
                        if at > 0 {
                            space += 1;
                        }
                        if word.is_empty() {
                            continue;
                        }
                        let width = word.width();
                        // alloc: a report's atoms, once
                        self.list.push(Atom { space, what: Piece::Word(word), width });
                        space = 0;
                    }
                }
                span => {
                    self.scratch.clear();
                    piece(&mut self.scratch, span, Atoms::caps(caps), roots);
                    let width = visible_width(&self.scratch);
                    // alloc: as above
                    self.list.push(Atom { space, what: Piece::Span(span), width });
                    space = 0;
                }
            }
        }
    }
}

/// The columns `text` takes on a terminal, its escapes (CSI to the final byte, OSC to ST or
/// BEL, whichever comes first) taking none; an escape never ended takes the rest.
fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut rest = text;
    while let Some((before, after)) = rest.split_once('\x1b') {
        width += before.width();
        rest = if let Some(params) = after.strip_prefix('[') {
            params
                .split_once(|c: char| ('\x40'..='\x7e').contains(&c))
                .map_or("", |(_, past)| past)
        } else if let Some(osc) = after.strip_prefix(']') {
            match (osc.split_once('\x07'), osc.split_once("\x1b\\")) {
                (Some((to_bel, past_bel)), Some((to_st, past_st))) => {
                    match to_bel.len() < to_st.len() {
                        true => past_bel,
                        false => past_st,
                    }
                }
                (Some((_, past)), None) | (None, Some((_, past))) => past,
                (None, None) => "",
            }
        } else {
            after
        };
    }
    width + rest.width()
}

#[cfg(test)]
mod tests {
    use super::visible_width;

    #[test]
    fn escapes_take_no_columns_around_multi_byte_characters() {
        // CSI, ended by its final byte.
        assert_eq!(visible_width("\u{e9}\x1b[1m\u{1f980}\x1b[0m\u{fc}"), 4);
        // OSC ended by BEL, and by ST.
        assert_eq!(visible_width("\x1b]0;t\u{ee}tle\x07\u{e9}"), 1);
        assert_eq!(visible_width("\x1b]8;;file:///\u{e9}\x1b\\\u{1f980}\x1b]8;;\x1b\\"), 2);
        // An OSC with both: the first of them ends it.
        assert_eq!(visible_width("\x1b]8;;\u{e9}\x1b\\\u{fc}\x1b]0;x\x07"), 1);
        assert_eq!(visible_width("\x1b]0;\u{e9}\x07\u{fc}\x1b]8;;\x1b\\"), 1);
        // An escape never ended takes the rest; a lone ESC takes only itself.
        assert_eq!(visible_width("\u{e9}\x1b]8;;\u{1f980}"), 1);
        assert_eq!(visible_width("\u{e9}\x1b[\u{1f980}"), 1);
        assert_eq!(visible_width("\u{e9}\x1b\u{fc}"), 2);
    }
}
