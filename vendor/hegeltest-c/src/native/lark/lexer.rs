//! Tokenizer for `.lark` grammar text, reproducing the terminal table Lark
//! defines in `load_grammar.py` (`TERMINALS`), including its lookahead
//! quirks: `?` is a rule modifier only when a name follows it, `.` and `..`
//! are distinct, `//` can never start a regex literal, and a comment
//! swallows the whitespace (newlines included) in front of it.

use super::{GrammarError, GrammarResult};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// A position in the grammar text, 1-based like Lark's error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    LPar,
    RPar,
    LBra,
    RBra,
    LBrace,
    RBrace,
    /// `?`, `*` or `+` after an atom.
    Op(char),
    Colon,
    Comma,
    Or,
    Dot,
    DotDot,
    Tilde,
    /// `?`, `!`, `?!` or `!?` in front of a rule name.
    RuleModifiers(String),
    Rule(String),
    Terminal(String),
    /// A string literal: the text between the quotes, unprocessed, and the
    /// flags after the closing quote (`i` or nothing).
    Str {
        body: String,
        flags: String,
    },
    /// A regex literal: the text between the slashes, unprocessed, and the
    /// flags after the closing slash.
    Regexp {
        body: String,
        flags: String,
    },
    /// A newline (with any following whitespace).
    Nl,
    /// A newline followed by `|`: an alternative continued on the next line.
    NlOr,
    /// `->`
    To,
    Ignore,
    Override,
    Declare,
    Extend,
    Import,
    Number(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub tok: Tok,
    pub pos: Pos,
}

struct Scanner<'a> {
    chars: Vec<char>,
    text: &'a str,
    i: usize,
    line: u32,
    column: u32,
}

fn is_lower_start(c: char) -> bool {
    c.is_ascii_lowercase()
}

fn is_upper_start(c: char) -> bool {
    c.is_ascii_uppercase()
}

fn is_lower_continue(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
}

fn is_upper_continue(c: char) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'
}

/// Python's `\s` on a `str` pattern: the characters `str.isspace` is
/// true of, which are Rust's whitespace and the four separators
/// `\x1c` to `\x1f`.
fn is_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\x1c'..='\x1f')
}

impl<'a> Scanner<'a> {
    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.i + offset).copied()
    }

    fn pos(&self) -> Pos {
        Pos {
            line: self.line,
            column: self.column,
        }
    }

    fn advance(&mut self) -> char {
        let c = self.chars[self.i];
        self.i += 1;
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        c
    }

    fn error(&self, pos: Pos, what: &str) -> GrammarError {
        let line_text = self.text.lines().nth(pos.line as usize - 1).unwrap_or("");
        GrammarError::new(format!(
            "{what} at line {} column {}\n\n{line_text}",
            pos.line, pos.column
        ))
    }

    /// Lark's `COMMENT` terminal is `\s*//[^\n]*|\s*#[^\n]*`: a comment
    /// claims any whitespace in front of it. Returns whether one was
    /// skipped.
    fn skip_comment(&mut self) -> bool {
        let mut j = self.i;
        while self.chars.get(j).is_some_and(|&c| is_space(c)) {
            j += 1;
        }
        let starts_comment = match self.chars.get(j) {
            Some('#') => true,
            Some('/') => self.chars.get(j + 1) == Some(&'/'),
            _ => false,
        };
        if !starts_comment {
            return false;
        }
        while self.i < j {
            self.advance();
        }
        while self.peek(0).is_some_and(|c| c != '\n') {
            self.advance();
        }
        true
    }

    /// Skip the ignored terminals (`WS`, `COMMENT`, `BACKSLASH`).
    fn skip_ignored(&mut self) {
        loop {
            if self.skip_comment() {
                continue;
            }
            match self.peek(0) {
                Some(' ' | '\t') => {
                    self.advance();
                }
                Some('\\') => {
                    let mut j = self.i + 1;
                    while self.chars.get(j) == Some(&' ') {
                        j += 1;
                    }
                    if self.chars.get(j) == Some(&'\n') {
                        while self.i <= j {
                            self.advance();
                        }
                    } else {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    /// `_NL` is `(\r?\n)+\s*` and `_NL_OR` is `(\r?\n)+\s*\|`; the second
    /// wins when it matches. A comment after the newlines is Lark's
    /// `COMMENT` claiming them, so the newline token is the one after it.
    fn newline(&mut self) -> Tok {
        loop {
            while self.peek(0).is_some_and(is_space) {
                self.advance();
            }
            if !self.skip_comment() {
                break;
            }
        }
        if self.peek(0) == Some('|') {
            self.advance();
            Tok::NlOr
        } else {
            Tok::Nl
        }
    }

    fn name(&mut self) -> GrammarResult<Tok> {
        let pos = self.pos();
        let mut s = String::new();
        if self.peek(0) == Some('_') {
            s.push(self.advance());
        }
        let first = self.peek(0).unwrap_or('\0');
        let (is_rule, cont): (bool, fn(char) -> bool) = if is_lower_start(first) {
            (true, is_lower_continue)
        } else if is_upper_start(first) {
            (false, is_upper_continue)
        } else {
            return Err(self.error(pos, "Expecting a name"));
        };
        while self.peek(0).is_some_and(cont) {
            s.push(self.advance());
        }
        if self
            .peek(0)
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(self.error(pos, "Illegal name for rules or terminals"));
        }
        Ok(if is_rule {
            Tok::Rule(s)
        } else {
            Tok::Terminal(s)
        })
    }

    /// `STRING` is `"(\\"|\\\\|[^"\n])*?"i?`.
    fn string(&mut self) -> GrammarResult<Tok> {
        let pos = self.pos();
        self.advance();
        let mut body = String::new();
        loop {
            match self.peek(0) {
                None | Some('\n') => {
                    return Err(self.error(pos, "Unterminated string literal"));
                }
                Some('\\') => {
                    body.push(self.advance());
                    if let Some('"' | '\\') = self.peek(0) {
                        body.push(self.advance());
                    }
                }
                Some('"') => {
                    self.advance();
                    break;
                }
                Some(_) => body.push(self.advance()),
            }
        }
        let mut flags = String::new();
        if self.peek(0) == Some('i') {
            flags.push(self.advance());
        }
        Ok(Tok::Str { body, flags })
    }

    /// `REGEXP` is `/(?!/)(\\/|\\\\|[^/])*?/[imslux]*`.
    fn regexp(&mut self) -> GrammarResult<Tok> {
        let pos = self.pos();
        self.advance();
        let mut body = String::new();
        loop {
            match self.peek(0) {
                None => return Err(self.error(pos, "Unterminated regular expression")),
                Some('\\') => {
                    body.push(self.advance());
                    if let Some('/' | '\\') = self.peek(0) {
                        body.push(self.advance());
                    }
                }
                Some('/') => {
                    self.advance();
                    break;
                }
                Some(_) => body.push(self.advance()),
            }
        }
        let mut flags = String::new();
        while self
            .peek(0)
            .is_some_and(|c| matches!(c, 'i' | 'm' | 's' | 'l' | 'u' | 'x'))
        {
            flags.push(self.advance());
        }
        Ok(Tok::Regexp { body, flags })
    }

    fn number(&mut self) -> GrammarResult<Tok> {
        let pos = self.pos();
        let mut s = String::new();
        if matches!(self.peek(0), Some('+' | '-')) {
            s.push(self.advance());
        }
        while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
            s.push(self.advance());
        }
        s.parse::<i64>()
            .map(Tok::Number)
            .map_err(|_| self.error(pos, "Number is too large"))
    }

    fn directive(&mut self) -> GrammarResult<Tok> {
        let pos = self.pos();
        self.advance();
        let mut s = String::new();
        while self.peek(0).is_some_and(|c| c.is_ascii_lowercase()) {
            s.push(self.advance());
        }
        match s.as_str() {
            "ignore" => Ok(Tok::Ignore),
            "override" => Ok(Tok::Override),
            "declare" => Ok(Tok::Declare),
            "extend" => Ok(Tok::Extend),
            "import" => Ok(Tok::Import),
            _ => Err(self.error(pos, &format!("Unknown directive %{s}"))),
        }
    }

    fn next_token(&mut self) -> GrammarResult<Option<Token>> {
        self.skip_ignored();
        let pos = self.pos();
        let Some(c) = self.peek(0) else {
            return Ok(None);
        };
        let tok = match c {
            '\n' | '\r' => self.newline(),
            '(' => {
                self.advance();
                Tok::LPar
            }
            ')' => {
                self.advance();
                Tok::RPar
            }
            '[' => {
                self.advance();
                Tok::LBra
            }
            ']' => {
                self.advance();
                Tok::RBra
            }
            '{' => {
                self.advance();
                Tok::LBrace
            }
            '}' => {
                self.advance();
                Tok::RBrace
            }
            ':' => {
                self.advance();
                Tok::Colon
            }
            ',' => {
                self.advance();
                Tok::Comma
            }
            '|' => {
                self.advance();
                Tok::Or
            }
            '~' => {
                self.advance();
                Tok::Tilde
            }
            '.' => {
                self.advance();
                if self.peek(0) == Some('.') {
                    self.advance();
                    Tok::DotDot
                } else {
                    Tok::Dot
                }
            }
            '-' => {
                if self.peek(1) == Some('>') {
                    self.advance();
                    self.advance();
                    Tok::To
                } else if self.peek(1).is_some_and(|c| c.is_ascii_digit()) {
                    self.number()?
                } else {
                    return Err(self.error(pos, "Unexpected input"));
                }
            }
            '+' => {
                if self.peek(1).is_some_and(|c| c.is_ascii_digit()) {
                    self.number()?
                } else {
                    self.advance();
                    Tok::Op('+')
                }
            }
            '*' => {
                self.advance();
                Tok::Op('*')
            }
            '?' | '!' => {
                let mut m = String::new();
                m.push(c);
                let second = self.peek(1);
                let (len, after) = match (c, second) {
                    ('?', Some('!')) | ('!', Some('?')) => (2, self.peek(2)),
                    _ => (1, second),
                };
                if after.is_some_and(|c| c == '_' || c.is_ascii_lowercase()) {
                    self.advance();
                    if len == 2 {
                        m.push(self.advance());
                    }
                    Tok::RuleModifiers(m)
                } else if c == '?' {
                    self.advance();
                    Tok::Op('?')
                } else {
                    return Err(self.error(pos, "Unexpected input"));
                }
            }
            '"' => self.string()?,
            '/' => self.regexp()?,
            '%' => self.directive()?,
            '_' | 'a'..='z' | 'A'..='Z' => self.name()?,
            '0'..='9' => self.number()?,
            _ => return Err(self.error(pos, "Unexpected input")),
        };
        Ok(Some(Token { tok, pos }))
    }
}

/// Tokenize grammar text. A newline is appended first, as Lark does, so
/// the last definition is terminated like every other.
pub fn tokenize(text: &str) -> GrammarResult<Vec<Token>> {
    let mut owned = String::with_capacity(text.len() + 1);
    owned.push_str(text);
    owned.push('\n');
    let mut scanner = Scanner {
        chars: owned.chars().collect(),
        text: &owned,
        i: 0,
        line: 1,
        column: 1,
    };
    let mut tokens = Vec::new();
    while let Some(token) = scanner.next_token()? {
        tokens.push(token);
    }
    Ok(tokens)
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/lark/lexer_tests.rs"]
mod tests;
