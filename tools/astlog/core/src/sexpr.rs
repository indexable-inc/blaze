//! Minimal S-expression reader for the rule DSL.
//!
//! Three forms: bare atoms (variables, rule names), double-quoted strings
//! (literals, tree-sitter queries, templates), and parenthesized lists.
//! `;` comments run to end of line. Every form remembers the 1-based line it
//! started on so later passes can report positions.

use crate::error::{DslSnafu, Error};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sexpr {
    Atom { text: String, line: usize },
    Str { text: String, line: usize },
    List { items: Vec<Self>, line: usize },
}

impl Sexpr {
    #[must_use]
    pub(crate) const fn line(&self) -> usize {
        match self {
            Self::Atom { line, .. } | Self::Str { line, .. } | Self::List { line, .. } => *line,
        }
    }
}

struct Reader<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    line: usize,
}

impl Reader<'_> {
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

    fn read_string(&mut self) -> Result<Sexpr, Error> {
        let line = self.line;
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
                            line: self.line,
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
                                line: self.line,
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
        let line = self.line;
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

    fn read_form(&mut self) -> Result<Option<Sexpr>, Error> {
        self.skip_trivia();
        let Some(&c) = self.chars.peek() else {
            return Ok(None);
        };
        match c {
            '(' => {
                let line = self.line;
                self.bump();
                let mut items = Vec::new();
                loop {
                    self.skip_trivia();
                    if self.chars.peek() == Some(&')') {
                        self.bump();
                        return Ok(Some(Sexpr::List { items, line }));
                    }
                    let Some(item) = self.read_form()? else {
                        return DslSnafu { line, message: "unclosed `(`".to_owned() }.fail();
                    };
                    items.push(item);
                }
            }
            ')' => DslSnafu { line: self.line, message: "unexpected `)`".to_owned() }.fail(),
            '"' => self.read_string().map(Some),
            _ => Ok(Some(self.read_atom())),
        }
    }
}

/// Read every top-level form in `src`.
///
/// # Errors
///
/// Returns [`Error::Dsl`] on unbalanced parentheses, unterminated strings, or
/// unknown string escapes.
pub(crate) fn parse(src: &str) -> Result<Vec<Sexpr>, Error> {
    let mut reader = Reader { chars: src.chars().peekable(), line: 1 };
    let mut forms = Vec::new();
    while let Some(form) = reader.read_form()? {
        forms.push(form);
    }
    Ok(forms)
}
