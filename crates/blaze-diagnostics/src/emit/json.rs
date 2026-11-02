//! rustc's `--error-format=json`: `compiler/rustc_errors/src/json.rs` (`Diagnostic`,
//! `DiagnosticSpan` and the rest, their keys in its order) at rust-lang/rust
//! ad3d0bc141a02cf446e384136d250a1f6950fed5 (MIT or Apache-2.0).
//!
//! The structs borrow every text from the diagnostic and its files; only `rendered` and a
//! macro's name are made.

use serde::Serialize;
use smallvec::SmallVec;

use super::{File, Placed, codes};
use crate::model::{Applicability, Child, Code, Diag, Expansion, MultiSpan, Span, Suggestion};

#[derive(Serialize)]
struct Diagnostic<'a> {
    /// Only on a diagnostic of its own, not on its children: rustc's `EmitTyped` tag.
    #[serde(rename = "$message_type", skip_serializing_if = "Option::is_none")]
    message_type: Option<&'static str>,
    message: &'a str,
    code: Option<DiagnosticCode<'a>>,
    level: &'static str,
    spans: Vec<DiagnosticSpan<'a>>,
    children: Vec<Diagnostic<'a>>,
    rendered: Option<String>,
}

#[derive(Serialize)]
struct DiagnosticSpan<'a> {
    file_name: &'a str,
    byte_start: u32,
    byte_end: u32,
    line_start: usize,
    line_end: usize,
    column_start: usize,
    column_end: usize,
    is_primary: bool,
    text: Vec<DiagnosticSpanLine<'a>>,
    label: Option<&'a str>,
    suggested_replacement: Option<&'a str>,
    suggestion_applicability: Option<&'static str>,
    expansion: Option<Box<DiagnosticSpanMacroExpansion<'a>>>,
}

#[derive(Serialize)]
struct DiagnosticSpanLine<'a> {
    text: &'a str,
    highlight_start: usize,
    highlight_end: usize,
}

#[derive(Serialize)]
struct DiagnosticSpanMacroExpansion<'a> {
    span: DiagnosticSpan<'a>,
    macro_decl_name: String,
    /// `null` where the model knows no definition of the macro, which rustc always has.
    def_site_span: Option<DiagnosticSpan<'a>>,
}

#[derive(Serialize)]
struct DiagnosticCode<'a> {
    code: &'a str,
    explanation: Option<&'static str>,
}

/// The expansions a span is in, outermost last: few enough to sit inline.
type Backtrace<'a> = SmallVec<[&'a Expansion; 4]>;

/// `diag` as rustc's JSON line, with `rendered` as its text: `from_errors_diagnostic`. Its
/// children are the ones it was given, without the note on the macro it originates in that
/// rustc's text emitter adds, then a `help` for each suggestion.
pub(super) fn line<'a>(diag: &'a Diag, placed: &Placed<'a>, rendered: String) -> String {
    let added = diag.origin_note();
    let suggestions: &'a [Suggestion] = match diag.suggestions_disabled {
        true => &[],
        false => &diag.suggestions,
    };
    let mut children = Vec::with_capacity(diag.children.len() + suggestions.len());
    children.extend(
        diag.children
            .iter()
            .filter(|it| !(it.span.is_empty() && Some(&it.message) == added.as_ref()))
            .map(|it| child(it, placed)),
    );
    children.extend(suggestions.iter().map(|it| suggestion(it, placed)));
    let code = diag.code.as_ref().map(|code| match code {
        Code::Error(code) | Code::LintWithCode { code, .. } => {
            DiagnosticCode { code, explanation: codes::explanation(code) }
        }
        Code::Lint(name) => DiagnosticCode { code: name, explanation: None },
    });
    let diagnostic = Diagnostic {
        message_type: Some("diagnostic"),
        message: &diag.message,
        code,
        level: diag.level.name(),
        spans: spans(&diag.span, placed),
        children,
        rendered: Some(rendered),
    };
    serde_json::to_string(&diagnostic).unwrap_or_else(|error| {
        panic!("blaze-diagnostics: `{}` as JSON: {error}", diag.message)
    })
}

fn child<'a>(child: &'a Child, placed: &Placed<'a>) -> Diagnostic<'a> {
    Diagnostic {
        message_type: None,
        message: &child.message,
        code: None,
        level: child.level.name(),
        spans: spans(&child.span, placed),
        children: Vec::new(), // alloc: empty, allocates nothing
        rendered: None,
    }
}

/// rustc's `DiagnosticSpan::from_suggestion`: each part of each way to substitute, as a
/// primary span carrying its replacement.
fn suggestion<'a>(suggestion: &'a Suggestion, placed: &Placed<'a>) -> Diagnostic<'a> {
    let spans = suggestion
        .substitutions
        .iter()
        .flatten()
        .map(|(span, replacement)| {
            // alloc: the JSON of a suggestion written, the edge
            let backtrace: Backtrace<'a> = span.macro_backtrace().collect();
            let suggested = Some((replacement.as_str(), suggestion.applicability));
            self::span(&span.file, span.lo, span.hi, true, None, suggested, &backtrace, placed)
        })
        .collect(); // alloc: the JSON of a suggestion written, the edge
    Diagnostic {
        message_type: None,
        message: &suggestion.message,
        code: None,
        level: "help",
        spans,
        children: Vec::new(), // alloc: empty, allocates nothing
        rendered: None,
    }
}

fn spans<'a>(span: &'a MultiSpan, placed: &Placed<'a>) -> Vec<DiagnosticSpan<'a>> {
    span.span_labels()
        .into_iter()
        .map(|it| {
            // alloc: the JSON of a diagnostic written, the edge
            let backtrace: Backtrace<'a> = it.span.macro_backtrace().collect();
            let at = it.span;
            self::span(&at.file, at.lo, at.hi, it.is_primary, it.label, None, &backtrace, placed)
        })
        .collect() // alloc: the JSON of a diagnostic written, the edge
}

/// rustc's `DiagnosticSpan::from_span_full`: `lo..hi` of `name` placed, with the expansions it
/// is in from `backtrace` on, each call placed with the ones after it.
#[allow(clippy::too_many_arguments)]
fn span<'a>(
    name: &'a str,
    lo: u32,
    hi: u32,
    is_primary: bool,
    label: Option<&'a str>,
    suggestion: Option<(&'a str, Applicability)>,
    backtrace: &[&'a Expansion],
    placed: &Placed<'a>,
) -> DiagnosticSpan<'a> {
    let file = placed.file_named(name, lo, hi);
    let (start, mut end) = (file.norm(lo), file.norm(hi));
    let mut byte_end = hi;
    let (line_start, column_start) = file.position(start);
    // A suggestion that removes from the start of a line to its end removes the line.
    if column_start == 0
        && suggestion.is_some_and(|(replacement, _)| replacement.is_empty())
        && file.shown
        && file.text[end..].starts_with('\n')
    {
        end += 1;
        byte_end = file.original(end);
    }
    let (line_end, column_end) = file.position(end);
    let expansion = backtrace.split_first().map(|(expansion, outer)| {
        let def_site_span = expansion.def_site.as_ref().map(|def| {
            let head = guess_head_span(placed.file_of(def), def);
            self::span(&def.file, def.lo, head, false, None, None, &[], placed)
        });
        let call = &expansion.call_site;
        // alloc: the JSON of a span in a macro, written, the edge; rustc's shape is recursive
        Box::new(DiagnosticSpanMacroExpansion {
            span: self::span(&call.file, call.lo, call.hi, false, None, None, outer, placed),
            macro_decl_name: expansion.decl_name(),
            def_site_span,
        })
    });
    DiagnosticSpan {
        file_name: super::shown(name),
        byte_start: lo,
        byte_end,
        line_start,
        line_end,
        column_start: column_start + 1,
        column_end: column_end + 1,
        is_primary,
        text: lines(file, start, end),
        label,
        suggested_replacement: suggestion.map(|(replacement, _)| replacement),
        suggestion_applicability: suggestion.map(|(_, applicability)| applicability.name()),
        expansion,
    }
}

/// rustc's `DiagnosticSpanLine::from_span` (`span_to_lines`): each line `lo..hi` is on, with
/// the columns of it on that line, 1-based in chars; none in a file whose source rustc doesn't
/// show.
fn lines(file: &File, lo: usize, hi: usize) -> Vec<DiagnosticSpanLine<'_>> {
    if lo > hi || !file.shown {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let ((lo_line, lo_column), (hi_line, hi_column)) = (file.position(lo), file.position(hi));
    let mut start = lo_column;
    let mut lines = Vec::with_capacity(hi_line - lo_line + 1);
    for line in lo_line..hi_line {
        let text = file.line_text(line - 1);
        let end = match file.ascii {
            true => text.len(),
            false => text.chars().count(),
        };
        lines.push(DiagnosticSpanLine { text, highlight_start: start + 1, highlight_end: end + 1 });
        start = 0;
    }
    lines.push(DiagnosticSpanLine {
        text: file.line_text(hi_line - 1),
        highlight_start: start + 1,
        highlight_end: hi_column + 1,
    });
    lines
}

/// rustc's `guess_head_span` (`span_until_char(sp, '{')`): the end of a definition up to its
/// `{`.
fn guess_head_span(file: &File, span: &Span) -> u32 {
    let Some(snippet) = file.snippet(span) else { return span.hi };
    let head = snippet.split('{').next().unwrap_or("").trim_end();
    match !head.is_empty() && !head.contains('\n') {
        true => span.lo + head.len() as u32,
        false => span.hi,
    }
}
