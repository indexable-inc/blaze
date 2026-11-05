//! The lexer as it was before one pass over bytes with a class table: a `char` decoded per
//! step, a string's value built as it was read. Kept as the reference [`crate::elide::lex`]
//! is checked against over seeded sources.

#![allow(dead_code)]

use std::borrow::Cow;

/// What a token is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind<'a> {
    /// An identifier or keyword: `fn`, `main`, `println`, `true`.
    Ident,
    /// `r#name`.
    RawIdent,
    /// `'a`, a lifetime or a label.
    Lifetime,
    /// `"…"`, with its value once escapes are read (borrowed when it has none), and whether
    /// an escape made a `{` or `}` (which a format string would read as source braces only
    /// when written as braces).
    Str { value: Cow<'a, str>, escaped_brace: bool },
    /// `r"…"` or `r#"…"#`, with its value.
    RawStr { value: &'a str },
    /// `'x'`, with its value.
    Char(char),
    /// A decimal integer with no suffix, with its value.
    Int(u128),
    /// A number elision does not read: one with a suffix (`1u8`, `1e5`), not decimal (`0x1f`),
    /// or too big for a `u128`.
    Number,
    /// One character of punctuation; `::` is two.
    Punct(char),
    /// `/// …` or `/** … */`.
    OuterDoc,
    /// `//! …` or `/*! … */`.
    InnerDoc,
}

/// A token and its text in the normalized source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token<'a> {
    pub kind: Kind<'a>,
    pub text: &'a str,
}

impl Token<'_> {
    pub fn is_punct(&self, c: char) -> bool {
        self.kind == Kind::Punct(c)
    }

    pub fn is_ident(&self, name: &str) -> bool {
        self.kind == Kind::Ident && self.text == name
    }
}

/// Why a source was not lexed, and the byte of the normalized source where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unlexed {
    pub at: usize,
    pub why: &'static str,
}

/// The source as rustc lexes it: without a leading byte order mark, `\r\n` as `\n`.
pub fn normalize(source: &str) -> Cow<'_, str> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    match source.contains("\r\n") {
        true => Cow::Owned(source.replace("\r\n", "\n")),
        false => Cow::Borrowed(source),
    }
}

/// The punctuation characters of Rust's tokens.
fn is_punct(c: char) -> bool {
    matches!(
        c,
        ';' | ','
            | '.'
            | '('
            | ')'
            | '{'
            | '}'
            | '['
            | ']'
            | '@'
            | '#'
            | '~'
            | '?'
            | ':'
            | '$'
            | '='
            | '!'
            | '<'
            | '>'
            | '-'
            | '&'
            | '|'
            | '+'
            | '*'
            | '/'
            | '^'
            | '%'
    )
}

/// Where the first of the characters rustc's deny-by-default lints refuse in literals and
/// comments is: the explicit embedding, override and isolate controls, U+202A to U+202E
/// (`E2 80 AA`..`E2 80 AE` in UTF-8) and U+2066 to U+2069 (`E2 81 A6`..`E2 81 A9`). Found by
/// memchr's search for their lead byte, not by decoding every character.
fn text_direction(source: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    memchr::memchr_iter(0xe2, bytes).find(|&at| {
        matches!(
            (bytes.get(at + 1), bytes.get(at + 2)),
            (Some(0x80), Some(0xaa..=0xae)) | (Some(0x81), Some(0xa6..=0xa9))
        )
    })
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The tokens of a normalized source ([`normalize`]), comments other than doc comments left
/// out.
pub fn lex(source: &str) -> Result<Vec<Token<'_>>, Unlexed> {
    if let Some(at) = memchr::memchr(b'\r', source.as_bytes()) {
        return Err(Unlexed { at, why: "a carriage return not before a line feed" });
    }
    if let Some(at) = text_direction(source) {
        return Err(Unlexed { at, why: "a text direction code point" });
    }
    let mut lexer = Lexer { source, at: 0 };
    // About one token for every few bytes of a script's source.
    let mut tokens = Vec::with_capacity(source.len() / 4);
    while let Some(c) = lexer.peek() {
        let start = lexer.at;
        let kind = match c {
            ' ' | '\t' | '\n' | '\x0b' | '\x0c' => {
                lexer.at += 1;
                continue;
            }
            '/' if lexer.nth(1) == Some('/') => match lexer.line_comment() {
                Some(kind) => kind,
                None => continue,
            },
            '/' if lexer.nth(1) == Some('*') => match lexer.block_comment()? {
                Some(kind) => kind,
                None => continue,
            },
            '"' => {
                lexer.bump();
                lexer.string()?
            }
            '\'' => lexer.char_or_lifetime()?,
            '0'..='9' => lexer.number(),
            'r' if lexer.nth(1) == Some('#') && lexer.nth(2).is_some_and(is_ident_start) => {
                lexer.at += 2;
                lexer.ident_rest();
                Kind::RawIdent
            }
            'r' if matches!(lexer.nth(1), Some('"' | '#')) => {
                lexer.bump();
                lexer.raw_string()?
            }
            c if is_ident_start(c) => {
                lexer.bump();
                lexer.ident_rest();
                if matches!(lexer.peek(), Some('"' | '\'' | '#')) {
                    return Err(lexer.fail("a prefixed literal or a reserved prefix"));
                }
                Kind::Ident
            }
            c if is_punct(c) => {
                lexer.bump();
                Kind::Punct(c)
            }
            _ => return Err(lexer.fail("a character outside literals and comments elision reads")),
        };
        tokens.push(Token { kind, text: &source[start..lexer.at] });
    }
    Ok(tokens)
}
struct Lexer<'a> {
    source: &'a str,
    at: usize,
}

impl<'a> Lexer<'a> {
    /// The character at `at`: one byte read for ASCII, which is nearly all of a script.
    fn peek(&self) -> Option<char> {
        let byte = *self.source.as_bytes().get(self.at)?;
        match byte.is_ascii() {
            true => Some(char::from(byte)),
            false => self.source[self.at..].chars().next(),
        }
    }

    fn nth(&self, n: usize) -> Option<char> {
        self.source[self.at..].chars().nth(n)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += c.len_utf8();
        Some(c)
    }

    fn fail(&self, why: &'static str) -> Unlexed {
        Unlexed { at: self.at, why }
    }

    fn ident_rest(&mut self) {
        let rest = &self.source.as_bytes()[self.at..];
        self.at += rest.iter().take_while(|it| it.is_ascii_alphanumeric() || **it == b'_').count();
    }

    /// A literal's end: rustc reads an identifier right after it as its suffix, which no
    /// string or character literal may have.
    fn no_suffix(&self) -> Result<(), Unlexed> {
        match self.peek().is_some_and(|c| is_ident_continue(c) || !c.is_ascii()) {
            true => Err(self.fail("a literal with a suffix")),
            false => Ok(()),
        }
    }

    /// `//…` to the end of the line: a doc comment's kind, or none for a plain comment.
    fn line_comment(&mut self) -> Option<Kind<'a>> {
        let start = self.at;
        let end = memchr::memchr(b'\n', &self.source.as_bytes()[start..])
            .map_or(self.source.len(), |it| start + it);
        self.at = end;
        let text = &self.source[start..end];
        if text.starts_with("//!") {
            Some(Kind::InnerDoc)
        } else if text.starts_with("///") && !text.starts_with("////") {
            Some(Kind::OuterDoc)
        } else {
            None
        }
    }

    /// `/* … */`, nested: a doc comment's kind, or none for a plain comment.
    fn block_comment(&mut self) -> Result<Option<Kind<'a>>, Unlexed> {
        let start = self.at;
        self.at += 2;
        let mut depth = 1usize;
        while depth > 0 {
            match self.bump() {
                None => return Err(Unlexed { at: start, why: "an unterminated block comment" }),
                Some('/') if self.peek() == Some('*') => {
                    self.bump();
                    depth += 1;
                }
                Some('*') if self.peek() == Some('/') => {
                    self.bump();
                    depth -= 1;
                }
                Some(_) => {}
            }
        }
        let text = &self.source[start..self.at];
        Ok(if text.starts_with("/*!") {
            Some(Kind::InnerDoc)
        } else if text.starts_with("/**") && !text.starts_with("/***") && text != "/**/" {
            Some(Kind::OuterDoc)
        } else {
            None
        })
    }

    /// The rest of `"…"`, after the opening quote. Its value is the source between the quotes
    /// until an escape makes it differ; from there it is built.
    fn string(&mut self) -> Result<Kind<'a>, Unlexed> {
        let start = self.at - 1;
        let content = self.at;
        let mut built: Option<String> = None;
        let mut escaped_brace = false;
        loop {
            let at = self.at;
            match self.bump() {
                None => return Err(Unlexed { at: start, why: "an unterminated string" }),
                Some('"') => {
                    let value = match built {
                        Some(it) => Cow::Owned(it),
                        None => Cow::Borrowed(&self.source[content..at]),
                    };
                    self.no_suffix()?;
                    return Ok(Kind::Str { value, escaped_brace });
                }
                Some('\\') => {
                    let source = self.source;
                    let value = built.get_or_insert_with(|| source[content..at].to_owned());
                    if let Some(c) = self.escape(true)? {
                        escaped_brace |= matches!(c, '{' | '}');
                        value.push(c);
                    }
                }
                Some(c) => {
                    if let Some(value) = &mut built {
                        value.push(c);
                    }
                }
            }
        }
    }

    /// An escape, after its backslash: the character it stands for, or none for a string's
    /// line continuation, which skips the line feed and the ASCII whitespace after it.
    fn escape(&mut self, in_string: bool) -> Result<Option<char>, Unlexed> {
        let bad = |lexer: &Self| lexer.fail("an escape rustc refuses or elision does not read");
        let Some(c) = self.bump() else { return Err(bad(self)) };
        let c = match c {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '\\' => '\\',
            '0' => '\0',
            '\'' => '\'',
            '"' => '"',
            'x' => {
                let high = self.bump().and_then(|it| it.to_digit(16));
                let low = self.bump().and_then(|it| it.to_digit(16));
                match (high, low) {
                    (Some(high), Some(low)) if high < 8 => char::from((high * 16 + low) as u8),
                    _ => return Err(bad(self)),
                }
            }
            'u' => {
                if self.bump() != Some('{') {
                    return Err(bad(self));
                }
                let mut value = 0u32;
                let mut digits = 0;
                loop {
                    match self.bump() {
                        Some('}') if digits > 0 => break,
                        Some(c) if digits < 6 && c.is_ascii_hexdigit() => {
                            value = value * 16 + c.to_digit(16).unwrap_or(0);
                            digits += 1;
                        }
                        _ => return Err(bad(self)),
                    }
                }
                match char::from_u32(value) {
                    Some(c) => c,
                    None => return Err(bad(self)),
                }
            }
            '\n' if in_string => {
                while matches!(self.peek(), Some(' ' | '\t' | '\n')) {
                    self.at += 1;
                }
                return Ok(None);
            }
            _ => return Err(bad(self)),
        };
        Ok(Some(c))
    }

    /// The rest of `r"…"` or `r#"…"#`, after the `r`.
    fn raw_string(&mut self) -> Result<Kind<'a>, Unlexed> {
        let start = self.at - 1;
        let mut hashes = 0usize;
        while self.peek() == Some('#') {
            self.at += 1;
            hashes += 1;
        }
        if hashes > 255 || self.bump() != Some('"') {
            return Err(self.fail("a raw string rustc refuses"));
        }
        let content = self.at;
        loop {
            let Some(quote) = memchr::memchr(b'"', &self.source.as_bytes()[self.at..]) else {
                return Err(Unlexed { at: start, why: "an unterminated raw string" });
            };
            let close = self.at + quote;
            self.at = close + 1;
            let rest = self.source[self.at..].as_bytes();
            if rest.len() >= hashes && rest[..hashes].iter().all(|b| *b == b'#') {
                self.at += hashes;
                let value = &self.source[content..close];
                self.no_suffix()?;
                return Ok(Kind::RawStr { value });
            }
        }
    }

    /// `'x'`, `'\n'`, or a lifetime or label `'a`.
    fn char_or_lifetime(&mut self) -> Result<Kind<'a>, Unlexed> {
        self.bump();
        let kind = match self.peek() {
            Some('\\') => {
                self.bump();
                let Some(c) = self.escape(false)? else {
                    return Err(self.fail("a line continuation in a character"));
                };
                if self.bump() != Some('\'') {
                    return Err(self.fail("a character literal of more than one character"));
                }
                Kind::Char(c)
            }
            Some(c) if self.nth(1) == Some('\'') => {
                if matches!(c, '\n' | '\r' | '\t' | '\'') {
                    return Err(self.fail("a character rustc wants escaped"));
                }
                self.bump();
                self.bump();
                Kind::Char(c)
            }
            Some(c) if is_ident_start(c) => {
                self.ident_rest();
                return Ok(Kind::Lifetime);
            }
            _ => return Err(self.fail("a quote that starts neither a character nor a lifetime")),
        };
        self.no_suffix()?;
        Ok(kind)
    }

    fn number(&mut self) -> Kind<'a> {
        let start = self.at;
        if self.peek() == Some('0') && matches!(self.nth(1), Some('x' | 'o' | 'b')) {
            self.at += 2;
            self.ident_rest();
            return Kind::Number;
        }
        let rest = &self.source.as_bytes()[start..];
        self.at += rest.iter().take_while(|it| it.is_ascii_digit() || **it == b'_').count();
        let digits = &self.source.as_bytes()[start..self.at];
        if self.peek().is_some_and(is_ident_start) {
            self.ident_rest();
            return Kind::Number;
        }
        // The digits' value without their `_`s, as `str::parse` of them would give it: too
        // big for a `u128` is no value.
        let value = digits.iter().filter(|it| **it != b'_').try_fold(0u128, |value, digit| {
            value.checked_mul(10)?.checked_add(u128::from(digit - b'0'))
        });
        match value {
            Some(value) => Kind::Int(value),
            None => Kind::Number,
        }
    }
}
