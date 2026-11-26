//! Typed errors carrying source spans and optional help notes (rule 19).

use core::fmt;

use blaze_own::{AString, Alloc};

use crate::span::{Span, line_col};

/// Category of a shell error, used for structured rendering and `try { } catch {|e| }`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ErrorKind {
    Lex,
    Parse,
    Type,
    Undefined,
    Immutable,
    Arity,
    DivisionByZero,
    Overflow,
    IndexOutOfBounds,
    MissingColumn,
    Format,
    Io,
    External,
    RustCell,
    Budget,
    User,
}

impl ErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Lex => "lex",
            ErrorKind::Parse => "parse",
            ErrorKind::Type => "type",
            ErrorKind::Undefined => "undefined",
            ErrorKind::Immutable => "immutable",
            ErrorKind::Arity => "arity",
            ErrorKind::DivisionByZero => "division_by_zero",
            ErrorKind::Overflow => "overflow",
            ErrorKind::IndexOutOfBounds => "index_out_of_bounds",
            ErrorKind::MissingColumn => "missing_column",
            ErrorKind::Format => "format",
            ErrorKind::Io => "io",
            ErrorKind::External => "external",
            ErrorKind::RustCell => "rust_cell",
            ErrorKind::Budget => "budget",
            ErrorKind::User => "user",
        }
    }
}

/// A rich diagnostic error produced by lexing, parsing, or evaluating a shell command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellError<'a> {
    pub kind: ErrorKind,
    pub message: AString<'a>,
    pub span: Span,
    pub help: Option<AString<'a>>,
}

impl<'a> ShellError<'a> {
    pub fn new(kind: ErrorKind, span: Span, message: &str, alloc: Alloc<'a>) -> ShellError<'a> {
        ShellError {
            kind,
            message: AString::from_str_in(message, alloc),
            span,
            help: None,
        }
    }

    pub fn with_help(mut self, help: &str, alloc: Alloc<'a>) -> ShellError<'a> {
        self.help = Some(AString::from_str_in(help, alloc));
        self
    }

    /// Format a single-line summary plus source underline into `out` for plain-text consumers.
    pub fn format_with_source(&self, src: &str, out: &mut impl fmt::Write) -> fmt::Result {
        let lc = line_col(src, self.span.start);
        writeln!(out, "error[{}]: {}", self.kind.as_str(), self.message.as_str())?;
        if !src.is_empty() {
            let line_text = &src[lc.line_start..lc.line_end];
            writeln!(out, "  --> line {}:{}", lc.line, lc.col)?;
            writeln!(out, "   |")?;
            writeln!(out, "{:>2} | {}", lc.line, line_text)?;
            write!(out, "   | ")?;
            let col0 = lc.col.saturating_sub(1) as usize;
            for _ in 0..col0 {
                out.write_char(' ')?;
            }
            let raw_len = self.span.end.saturating_sub(self.span.start) as usize;
            let caret_len = raw_len.min(lc.line_end.saturating_sub(lc.line_start + col0)).max(1);
            for _ in 0..caret_len {
                out.write_char('^')?;
            }
            writeln!(out)?;
        }
        if let Some(help) = &self.help {
            writeln!(out, "   = help: {}", help.as_str())?;
        }
        Ok(())
    }
}

impl fmt::Display for ShellError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error[{}]: {}", self.kind.as_str(), self.message.as_str())
    }
}
