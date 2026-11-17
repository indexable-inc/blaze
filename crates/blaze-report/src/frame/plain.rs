//! The views of a finding that are not drawn for eyes (decision D70): `agent`, one or two plain
//! lines a finding with nothing decorative, and `json`, the same fields as data, one object a
//! line. Both read the same facts the frame does ([`Plain`]); the caller picks the view once,
//! at the command's edge ([`view`]).
//!
//! The agent view, exactly, for a finding with a fix and its closing line:
//!
//! ```text
//! e7k2q  error[E0425]  src/main.rs:1:37  cannot find value `c` in this scope
//!        fix: replace `c` with `b` (1:37-1:38): `b` is in scope, is a `u32`, and is never used
//! 1 error, 1 fix: blaze check --fix e7k2q
//! ```
//!
//! JSON fields, in this order: `id`, `severity`, `code`, `path`, `line`, `col`, `end_line`,
//! `end_col`, `message`, `fix` (`null`, or `start_line`, `start_col`, `end_line`, `end_col`,
//! `replace`, `with`, `reason`). A note is `{"note": ..}`; the closing object's fields are
//! `errors`, `warnings`, `fixes`, `command`, `not_checked`.

use blaze_own::AString;
use blaze_text::Writes;

use super::{Counts, Coverage, Edit, line_col};
use crate::Severity;

/// Which view a command prints its findings in (`--view person|agent|json`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    /// The drawn diagnostic ([`super::render`]).
    Person,
    /// Plain lines for a program that reads them ([`agent`]).
    Agent,
    /// One JSON object a line ([`json`]).
    Json,
}

impl View {
    /// `person`, `agent` or `json`.
    pub fn parse(word: &str) -> Option<View> {
        match word {
            "person" => Some(View::Person),
            "agent" => Some(View::Agent),
            "json" => Some(View::Json),
            _ => None,
        }
    }
}

/// The view a command prints in: the one asked, else `person` on a terminal and `agent` off one
/// (a pipe, a file, a tool call), so a program that runs the command gets the compact view
/// without knowing a flag. `terminal` is the one test the command's edge made of its output, the
/// same value [`super::detect`] is given.
pub fn view(asked: Option<View>, terminal: bool) -> View {
    match (asked, terminal) {
        (Some(view), _) => view,
        (None, true) => View::Person,
        (None, false) => View::Agent,
    }
}

/// One finding, as the plain views say it.
#[derive(Clone, Copy, Debug)]
pub struct Plain<'a> {
    /// Its id as shown (D70: five characters, longer only where two share a prefix).
    pub id: &'a str,
    pub severity: Severity,
    /// The rule's name (`syntax`, `unresolved`, `E0425`).
    pub code: &'a str,
    /// Where it is: the file as shown, or, for a finding of no file, what it is about.
    pub path: &'a str,
    /// The file's text, for `line:col`; `None` for a finding of no file.
    pub text: Option<&'a [u8]>,
    /// File-relative: the printer added the item's place to the record's range.
    pub start: u32,
    pub end: u32,
    /// Plain words, code in backticks.
    pub message: &'a str,
    /// The one edit its fix makes, and why it is the fix.
    pub fix: Option<(Edit<'a>, &'a str)>,
}

/// Why a plain view was not written: the caller's fix does not fit its text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlainError {
    /// The edit's range is not in the text, ends before it starts, or splits a character.
    OutOfText { start: u32, end: u32, len: usize },
}

impl core::fmt::Display for PlainError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let PlainError::OutOfText { start, end, len } = *self;
        write!(f, "blaze_report::frame::plain: a fix's edit {start}..{end} is not in its file of {len} bytes")
    }
}

impl core::error::Error for PlainError {}

/// What a fix replaces: the text under it, checked to be text.
fn replaced<'t>(text: &'t [u8], edit: &Edit<'_>) -> Result<&'t str, PlainError> {
    let bad = PlainError::OutOfText { start: edit.start, end: edit.end, len: text.len() };
    let bytes = text.get(edit.start as usize..edit.end as usize).ok_or(bad)?;
    core::str::from_utf8(bytes).map_err(|_| bad)
}

/// `text` with every control character a space: a plain line stays one line.
fn plain_str(out: &mut AString<'_>, text: &str) {
    for c in text.chars() {
        out.push(if c.is_control() { ' ' } else { c });
    }
}

/// `line:col`.
fn at(out: &mut AString<'_>, (line, col): (u32, u32)) {
    out.int(line).str(":").int(col);
}

/// One finding in the agent view: `<id>  error[<code>]  <file:line:col>  <message>`, then, for a
/// fix, `fix: replace `x` with `y` (l:c-l:c): <reason>` under the code (`delete` and `insert`
/// where one side is empty; a replaced text that ends a line is shown without its newline).
pub fn agent(out: &mut AString<'_>, plain: &Plain<'_>) -> Result<(), PlainError> {
    // The fix's words first, so nothing is written for a fix that does not fit.
    let fix = match (plain.fix, plain.text) {
        (Some((edit, reason)), Some(text)) => Some((edit, reason, replaced(text, &edit)?, text)),
        _ => None,
    };
    out.str(plain.id).str("  ").str(plain.severity.label()).str("[").str(plain.code).str("]  ");
    plain_str(out, plain.path);
    if let Some(text) = plain.text {
        out.str(":");
        at(out, line_col(text, plain.start));
    }
    out.str("  ");
    plain_str(out, plain.message);
    out.str("\n");
    if let Some((edit, reason, old, text)) = fix {
        for _ in 0..plain.id.chars().count() + 2 {
            out.push(' ');
        }
        out.str("fix: ");
        let old = old.strip_suffix('\n').unwrap_or(old);
        match (old.is_empty(), edit.insert.is_empty()) {
            (false, false) => {
                out.str("replace `");
                plain_str(out, old);
                out.str("` with `");
                plain_str(out, edit.insert);
                out.str("`");
            }
            (false, true) => {
                out.str("delete `");
                plain_str(out, old);
                out.str("`");
            }
            (true, _) => {
                out.str("insert `");
                plain_str(out, edit.insert);
                out.str("`");
            }
        }
        out.str(" (");
        at(out, line_col(text, edit.start));
        out.str("-");
        at(out, line_col(text, edit.end));
        out.str("): ");
        plain_str(out, reason);
        out.str("\n");
    }
    Ok(())
}

/// `s` as a JSON string.
fn json_str(out: &mut AString<'_>, s: &str) {
    out.push('"');
    for c in s.chars() {
        let escaped = match c {
            '"' => "\\\"",
            '\\' => "\\\\",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            c if (c as u32) < 0x20 => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.push_str("\\u00");
                out.push(HEX[(c as usize) >> 4] as char);
                out.push(HEX[(c as usize) & 15] as char);
                continue;
            }
            c => {
                out.push(c);
                continue;
            }
        };
        out.push_str(escaped);
    }
    out.push('"');
}

/// One finding as one JSON object and a newline, fields in the module's stated order.
pub fn json(out: &mut AString<'_>, plain: &Plain<'_>) -> Result<(), PlainError> {
    let fix = match (plain.fix, plain.text) {
        (Some((edit, reason)), Some(text)) => Some((edit, reason, replaced(text, &edit)?, text)),
        _ => None,
    };
    out.str("{\"id\":");
    json_str(out, plain.id);
    out.str(",\"severity\":");
    json_str(out, plain.severity.label());
    out.str(",\"code\":");
    json_str(out, plain.code);
    out.str(",\"path\":");
    json_str(out, plain.path);
    match plain.text {
        Some(text) => {
            let (line, col) = line_col(text, plain.start);
            let (end_line, end_col) = line_col(text, plain.end);
            out.str(",\"line\":").int(line).str(",\"col\":").int(col).str(",\"end_line\":").int(end_line).str(",\"end_col\":").int(end_col);
        }
        None => {
            out.str(",\"line\":null,\"col\":null,\"end_line\":null,\"end_col\":null");
        }
    }
    out.str(",\"message\":");
    json_str(out, plain.message);
    out.str(",\"fix\":");
    match fix {
        None => {
            out.str("null");
        }
        Some((edit, reason, old, text)) => {
            let (sl, sc) = line_col(text, edit.start);
            let (el, ec) = line_col(text, edit.end);
            out.str("{\"start_line\":").int(sl).str(",\"start_col\":").int(sc).str(",\"end_line\":").int(el).str(",\"end_col\":").int(ec);
            out.str(",\"replace\":");
            json_str(out, old);
            out.str(",\"with\":");
            json_str(out, edit.insert);
            out.str(",\"reason\":");
            json_str(out, reason);
            out.str("}");
        }
    }
    out.str("}\n");
    Ok(())
}

/// A note about the command in the JSON view: `{"note": ..}`. (The agent view's note is the
/// line form's `note: words`, [`super::note`] off a terminal.)
pub fn json_note(out: &mut AString<'_>, words: &str) {
    out.str("{\"note\":");
    json_str(out, words);
    out.str("}\n");
}

/// `--fix` refused, in the JSON view: `{"refused": ..}`.
pub fn json_refused(out: &mut AString<'_>, words: &str) {
    out.str("{\"refused\":");
    json_str(out, words);
    out.str("}\n");
}

/// What a plain view's last line says.
#[derive(Clone, Copy, Debug)]
pub struct PlainClosing<'a> {
    pub counts: Counts,
    /// The ids of the findings whose fix this command can apply, in the order printed.
    pub fixes: &'a [&'a str],
    /// The stages the command is defined to run that it did not.
    pub coverage: Option<Coverage<'a>>,
    /// The command that was run: `blaze check`.
    pub command: &'a str,
}

/// The command that applies `fixes`: by its one id, or `all`.
fn fix_command<'a>(fixes: &[&'a str]) -> Option<&'a str> {
    match fixes {
        [] => None,
        [one] => Some(one),
        _ => Some("all"),
    }
}

/// The agent view's end: the stages NOT checked, one line, when any (an agent must never read a
/// pass that is not one), then `1 error, 1 fix: blaze check --fix e7k2q`, or `0 errors: blaze
/// check`: the counts, and the command to run next.
pub fn agent_closing(out: &mut AString<'_>, closing: &PlainClosing<'_>) {
    if let Some(coverage) = closing.coverage {
        out.str("incomplete: NOT checked: ");
        for (i, stage) in coverage.not.iter().enumerate() {
            if i > 0 {
                out.str(", ");
            }
            out.str(stage);
        }
        out.str("\n");
    }
    let counts = closing.counts;
    // A task that failed on its own is no error of the code: counted first, by itself.
    if counts.failures > 0 {
        out.int(counts.failures).str(if counts.failures == 1 { " failure, " } else { " failures, " });
    }
    out.int(counts.errors).str(if counts.errors == 1 { " error" } else { " errors" });
    if counts.warnings > 0 {
        out.str(", ").int(counts.warnings).str(if counts.warnings == 1 { " warning" } else { " warnings" });
    }
    let n = closing.fixes.len() as u32;
    if n > 0 {
        out.str(", ").int(n).str(if n == 1 { " fix" } else { " fixes" });
    }
    out.str(": ").str(closing.command);
    if let Some(selector) = fix_command(closing.fixes) {
        out.str(" --fix ").str(selector);
    }
    out.str("\n");
}

/// The JSON view's end: one object, `{"errors":1,"warnings":0,"fixes":1,"command":"blaze check
/// --fix e7k2q","not_checked":["types","borrows"]}`.
pub fn json_closing(out: &mut AString<'_>, closing: &PlainClosing<'_>) {
    let counts = closing.counts;
    out.str("{");
    // Only when there are any: a run with none keeps its object as before.
    if counts.failures > 0 {
        out.str("\"failures\":").int(counts.failures).str(",");
    }
    out.str("\"errors\":").int(counts.errors).str(",\"warnings\":").int(counts.warnings);
    out.str(",\"fixes\":").int(closing.fixes.len() as u32).str(",\"command\":\"").str(closing.command);
    if let Some(selector) = fix_command(closing.fixes) {
        out.str(" --fix ").str(selector);
    }
    out.str("\",\"not_checked\":[");
    for (i, stage) in closing.coverage.map_or(&[][..], |coverage| coverage.not).iter().enumerate() {
        if i > 0 {
            out.str(",");
        }
        json_str(out, stage);
    }
    out.str("]}\n");
}
