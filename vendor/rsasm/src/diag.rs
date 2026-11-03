//! Diagnostics: collection and rendering of errors, warnings and notes.

use crate::source::{SourceMap, Span};
use std::fmt::Write as _;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[non_exhaustive]
pub enum Severity {
    Note,
    Warning,
    Error,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Note => "note",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }

    /// ANSI SGR color code used when rendering to a terminal.
    fn color(self) -> &'static str {
        match self {
            Severity::Note => "36",    // cyan
            Severity::Warning => "33", // yellow
            Severity::Error => "31",   // red
        }
    }
}

/// An extra span highlighted underneath the primary one.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SubDiag {
    pub span: Span,
    pub msg: String,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Diagnostic {
    pub severity: Severity,
    pub msg: String,
    pub span: Span,
    /// Additional labelled spans, rendered after the primary snippet.
    pub notes: Vec<SubDiag>,
    /// Free-form trailing help text.
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn new(severity: Severity, span: Span, msg: impl Into<String>) -> Diagnostic {
        Diagnostic {
            severity,
            msg: msg.into(),
            span,
            notes: Vec::new(),
            help: None,
        }
    }

    pub fn error(span: Span, msg: impl Into<String>) -> Diagnostic {
        Diagnostic::new(Severity::Error, span, msg)
    }

    pub fn warning(span: Span, msg: impl Into<String>) -> Diagnostic {
        Diagnostic::new(Severity::Warning, span, msg)
    }

    pub fn with_note(mut self, span: Span, msg: impl Into<String>) -> Diagnostic {
        self.notes.push(SubDiag {
            span,
            msg: msg.into(),
        });
        self
    }

    pub fn with_help(mut self, msg: impl Into<String>) -> Diagnostic {
        self.help = Some(msg.into());
        self
    }
}

/// Accumulates diagnostics for a whole run.
#[derive(Default)]
#[non_exhaustive]
pub struct DiagBag {
    diags: Vec<Diagnostic>,
    errors: usize,
    /// Stop recording once this many errors have been seen (0 = unlimited).
    pub max_errors: usize,
}

impl DiagBag {
    pub fn new() -> DiagBag {
        DiagBag {
            diags: Vec::new(),
            errors: 0,
            max_errors: 0,
        }
    }

    pub fn emit(&mut self, d: Diagnostic) {
        if d.severity == Severity::Error {
            self.errors += 1;
        }
        self.diags.push(d);
    }

    pub fn error(&mut self, span: Span, msg: impl Into<String>) {
        self.emit(Diagnostic::error(span, msg));
    }

    pub fn warning(&mut self, span: Span, msg: impl Into<String>) {
        self.emit(Diagnostic::warning(span, msg));
    }

    pub fn has_errors(&self) -> bool {
        self.errors > 0
    }

    pub fn error_count(&self) -> usize {
        self.errors
    }

    /// True once `max_errors` has been reached, so callers can bail out early.
    pub fn saturated(&self) -> bool {
        self.max_errors > 0 && self.errors >= self.max_errors
    }

    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diags.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.diags.is_empty()
    }

    pub fn len(&self) -> usize {
        self.diags.len()
    }

    pub fn take(&mut self) -> Vec<Diagnostic> {
        self.errors = 0;
        std::mem::take(&mut self.diags)
    }

    /// Renders every diagnostic to a string.
    pub fn render(&self, sm: &SourceMap, color: bool) -> String {
        let mut out = String::new();
        for d in &self.diags {
            render_one(&mut out, d, sm, color);
        }
        out
    }
}

fn render_one(out: &mut String, d: &Diagnostic, sm: &SourceMap, color: bool) {
    let (bold, reset, sev_col) = if color {
        (
            "\x1b[1m",
            "\x1b[0m",
            format!("\x1b[1;{}m", d.severity.color()),
        )
    } else {
        ("", "", String::new())
    };
    let sev_reset = if color { "\x1b[0m" } else { "" };

    let _ = writeln!(
        out,
        "{sev_col}{}{sev_reset}{bold}: {}{reset}",
        d.severity.label(),
        d.msg
    );
    render_snippet(out, d.span, None, sm, color);
    for n in &d.notes {
        let _ = writeln!(out, "  {bold}note{reset}: {}", n.msg);
        render_snippet(out, n.span, Some(&n.msg), sm, color);
    }
    if let Some(help) = &d.help {
        let _ = writeln!(out, "  {bold}help{reset}: {help}");
    }
}

fn render_snippet(out: &mut String, span: Span, _label: Option<&str>, sm: &SourceMap, color: bool) {
    if span.is_dummy() {
        return;
    }
    let Some(file) = sm.lookup(span.lo) else {
        return;
    };
    let start = file.line_col(span.lo);
    let end_pos = span.hi.max(span.lo).min(file.end());
    let end = file.line_col(end_pos);

    let (dim, reset) = if color {
        ("\x1b[1;34m", "\x1b[0m")
    } else {
        ("", "")
    };
    let gutter_w = end.line.to_string().len().max(1);
    let pad = " ".repeat(gutter_w);

    let _ = writeln!(
        out,
        "{pad}{dim}-->{reset} {}:{}:{}",
        file.name.display(),
        start.line,
        start.col
    );

    // Render at most a few lines; long multi-line spans get elided.
    const MAX_LINES: u32 = 4;
    let last = end.line.min(start.line + MAX_LINES - 1);
    let _ = writeln!(out, "{pad} {dim}|{reset}");
    for line in start.line..=last {
        let text = file.line_text(line);
        let _ = writeln!(
            out,
            "{dim}{line:>gutter_w$} |{reset} {text}",
            gutter_w = gutter_w
        );

        // Underline the covered part of this line.
        let from = if line == start.line { start.col } else { 1 };
        let to = if line == end.line {
            end.col
        } else {
            text.chars().count() as u32 + 1
        };
        let width = to.saturating_sub(from).max(1) as usize;
        let lead: String = text
            .chars()
            .take((from - 1) as usize)
            .map(|c| if c == '\t' { '\t' } else { ' ' })
            .collect();
        let caret = if color {
            format!("\x1b[1;31m{}\x1b[0m", "^".repeat(width))
        } else {
            "^".repeat(width)
        };
        let _ = writeln!(out, "{pad} {dim}|{reset} {lead}{caret}");
    }
    if end.line > last {
        let _ = writeln!(out, "{pad} {dim}| ...{reset}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_single_line_span() {
        let mut sm = SourceMap::new();
        let f = sm.add("t.s", "mov rax, rbx\nret\n");
        let base = sm.file(f).start;
        let mut bag = DiagBag::new();
        bag.error(Span::new(base + 4, base + 7), "bad register");
        let text = bag.render(&sm, false);
        assert!(text.contains("error: bad register"), "{text}");
        assert!(text.contains("t.s:1:5"), "{text}");
        assert!(text.contains("mov rax, rbx"), "{text}");
        assert!(text.contains("^^^"), "{text}");
    }

    #[test]
    fn dummy_span_renders_message_only() {
        let sm = SourceMap::new();
        let mut bag = DiagBag::new();
        bag.error(Span::DUMMY, "no location");
        assert_eq!(bag.render(&sm, false), "error: no location\n");
    }
}
