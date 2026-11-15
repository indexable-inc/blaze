//! Minimal S-expression reader for the rule DSL.
//!
//! Three forms: bare atoms (variables, rule names), double-quoted strings
//! (literals, tree-sitter queries, templates), and parenthesized lists.
//! `;` comments run to end of line. Every form remembers the file and the 1-based line it
//! started on so later passes can report positions.

use std::sync::Arc;

use crate::error::{DslSnafu, Error, TooDeepSnafu};
use crate::line::Line;

/// The deepest a list may nest. The reader, and every pass over what it read, recurses once a
/// level, so request text nested without bound would overflow the stack; a real rule nests a
/// handful of levels. Each file an `(include ...)` reads is held to it on its own.
pub(crate) const MAX_DEPTH: usize = 64;

/// One form as read: a fact source parses its own atoms from these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sexpr {
    Atom { text: String, line: Line },
    Str { text: String, line: Line },
    List { items: Vec<Self>, line: Line },
}

impl Sexpr {
    #[must_use]
    pub fn line(&self) -> Line {
        match self {
            Self::Atom { line, .. } | Self::Str { line, .. } | Self::List { line, .. } => {
                line.clone() // clone: a file name's count, for the caller's error or form
            }
        }
    }
}

struct Reader<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    file: Option<Arc<str>>,
    line: usize,
}

impl Reader<'_> {
    fn here(&self) -> Line {
        match &self.file {
            Some(file) => Line::of(Arc::clone(file), self.line),
            None => Line::rules(self.line),
        }
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.next();
        if c == Some('\n') {
            self.line += 1;
        }
        c
    }

    fn skip_trivia(&mut self) {
        while let Some(&c) = self.chars.peek() {
            if c == ';' {
                while let Some(c) = self.bump() {
                    if c == '\n' {
                        break;
                    }
                }
            } else if c.is_whitespace() {
                self.bump();
            } else {
                break;
            }
        }
    }

    fn read_string<S: std::error::Error + 'static>(&mut self) -> Result<Sexpr, Error<S>> {
        let line = self.here();
        self.bump();
        let mut text = String::new();
        loop {
            let Some(c) = self.bump() else {
                return DslSnafu { line, message: "unterminated string".to_owned() }.fail();
            };
            match c {
                '"' => return Ok(Sexpr::Str { text, line }),
                '\\' => {
                    let Some(escaped) = self.bump() else {
                        return DslSnafu {
                            line: self.here(),
                            message: "dangling `\\` at end of input".to_owned(),
                        }
                        .fail();
                    };
                    match escaped {
                        'n' => text.push('\n'),
                        't' => text.push('\t'),
                        '"' | '\\' => text.push(escaped),
                        other => {
                            return DslSnafu {
                                line: self.here(),
                                message: format!("unknown escape `\\{other}`"),
                            }
                            .fail();
                        }
                    }
                }
                other => text.push(other),
            }
        }
    }

    fn read_atom(&mut self) -> Sexpr {
        let line = self.here();
        let mut text = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_whitespace() || matches!(c, '(' | ')' | '"' | ';') {
                break;
            }
            text.push(c);
            self.bump();
        }
        Sexpr::Atom { text, line }
    }

    /// The next form, itself inside `depth` lists.
    fn read_form<S: std::error::Error + 'static>(
        &mut self,
        depth: usize,
    ) -> Result<Option<Sexpr>, Error<S>> {
        self.skip_trivia();
        let Some(&c) = self.chars.peek() else {
            return Ok(None);
        };
        match c {
            '(' => {
                let line = self.here();
                if depth == MAX_DEPTH {
                    return TooDeepSnafu { line, max: MAX_DEPTH }.fail();
                }
                self.bump();
                let mut items = Vec::new();
                loop {
                    self.skip_trivia();
                    if self.chars.peek() == Some(&')') {
                        self.bump();
                        return Ok(Some(Sexpr::List { items, line }));
                    }
                    let Some(item) = self.read_form(depth + 1)? else {
                        return DslSnafu { line, message: "unclosed `(`".to_owned() }.fail();
                    };
                    items.push(item);
                }
            }
            ')' => DslSnafu { line: self.here(), message: "unexpected `)`".to_owned() }.fail(),
            '"' => self.read_string().map(Some),
            _ => Ok(Some(self.read_atom())),
        }
    }
}

/// Read every top-level form in `src`, the text of `file` (`None`: the request's own rules).
///
/// # Errors
///
/// Returns [`Error::Dsl`] on unbalanced parentheses, unterminated strings, or
/// unknown string escapes, and [`Error::TooDeep`] on lists nested past [`MAX_DEPTH`].
pub(crate) fn parse<S: std::error::Error + 'static>(
    src: &str,
    file: Option<Arc<str>>,
) -> Result<Vec<Sexpr>, Error<S>> {
    let mut reader = Reader { chars: src.chars().peekable(), file, line: 1 };
    let mut forms = Vec::new();
    while let Some(form) = reader.read_form(0)? {
        forms.push(form);
    }
    Ok(forms)
}
