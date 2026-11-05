//! Rust's lexical grammar, as far as [`super::elide`] reads a script: identifiers, raw
//! identifiers, lifetimes, punctuation, string literals with every escape form, raw strings,
//! character and decimal integer literals, comments (nested block comments included) and doc
//! comments, after the normalization rustc applies to a source file first (a byte order mark
//! dropped, `\r\n` read as `\n`). The rules are `rustc_lexer` and `rustc_lexer::unescape`'s.
//!
//! What elision never accepts is refused here rather than lexed exactly: identifiers that are
//! not ASCII, literals with a prefix (`b"…"`, `c"…"`, `b'…'`) or a reserved one (`foo"…"`,
//! `foo#`), a lone carriage return, and the text direction code points rustc's
//! `text_direction_codepoint_in_literal` and `…_in_comment` lints deny. A refusal only means
//! a script is run rather than elided.
//!
//! One pass over the bytes, on demand ([`Tokens`]), so elision stops at the first token of a
//! form it does not read. Each byte's class is one lookup in a table; whitespace and
//! identifiers are runs of a class; strings, block comments, line comments and raw strings
//! jump to their next byte of interest with memchr's SIMD search, as simdjson's first stage
//! finds quotes and backslashes; `\r` and the text direction code points are found the same
//! way over the whole source before the first token. A token is a kind and a slice of the
//! source: nothing is allocated per token, and a string's value, when an escape makes it
//! differ from its text, is read again only where it is used ([`Token::push_value`]).

use std::borrow::Cow;

use blaze_mem::Frame;

/// What a token is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind<'a> {
    /// An identifier or keyword: `fn`, `main`, `println`, `true`.
    Ident,
    /// `r#name`.
    RawIdent,
    /// `'a`, a lifetime or a label.
    Lifetime,
    /// `"…"`: whether it has an escape (its value then differs from the text between its
    /// quotes), and whether an escape made a `{` or `}` (which a format string would read
    /// as source braces only when written as braces).
    Str { escaped: bool, escaped_brace: bool },
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

impl<'a> Token<'a> {
    pub fn is_punct(&self, c: char) -> bool {
        self.kind == Kind::Punct(c)
    }

    pub fn is_ident(&self, name: &str) -> bool {
        self.kind == Kind::Ident && self.text == name
    }

    /// The value of a string, borrowed from the source unless an escape made it differ;
    /// `None` for any other token.
    pub fn str_value(&self) -> Option<Cow<'a, str>> {
        match self.kind {
            Kind::Str { escaped: false, .. } => {
                Some(Cow::Borrowed(&self.text[1..self.text.len() - 1]))
            }
            Kind::Str { escaped: true, .. } => {
                let mut value = String::with_capacity(self.text.len());
                unescape_into(self.text, &mut value)?;
                Some(Cow::Owned(value))
            }
            Kind::RawStr { value } => Some(Cow::Borrowed(value)),
            _ => None,
        }
    }

    /// The value of a string or raw string added to `out`; `None` for any other token.
    pub fn push_value(&self, out: &mut String) -> Option<()> {
        match self.kind {
            Kind::Str { escaped: false, .. } => out.push_str(&self.text[1..self.text.len() - 1]),
            Kind::Str { escaped: true, .. } => unescape_into(self.text, out)?,
            Kind::RawStr { value } => out.push_str(value),
            _ => return None,
        }
        Some(())
    }
}

/// The value of the string literal `text` (quotes included, as lexed), added to `out`: the
/// runs between escapes copied whole, each escape read as the lexer read it.
fn unescape_into(text: &str, out: &mut String) -> Option<()> {
    // Every escape is longer than what it stands for, so the value fits in its text's length
    // less the quotes: `out` grows at most once.
    out.reserve(text.len().saturating_sub(2));
    let bytes = text.as_bytes();
    let mut lexer = Lexer { source: text, at: 1 };
    loop {
        let end = lexer.at + memchr::memchr2(b'"', b'\\', &bytes[lexer.at..])?;
        out.push_str(&text[lexer.at..end]);
        lexer.at = end + 1;
        if bytes[end] == b'"' {
            return Some(());
        }
        if let Some(c) = lexer.escape(true).ok()? {
            out.push(c);
        }
    }
}

/// Why a source was not lexed, and the byte of the normalized source where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unlexed {
    pub at: usize,
    pub why: &'static str,
}

/// The source as rustc lexes it: without a leading byte order mark, `\r\n` as `\n`.
/// Borrowed when it has no `\r\n`; else copied once into `frame`.
pub fn normalize<'a>(source: &'a str, frame: &'a Frame) -> &'a str {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let bytes = source.as_bytes();
    let mut crlf = memchr::memmem::find_iter(bytes, b"\r\n").peekable();
    if crlf.peek().is_none() {
        return source;
    }
    let mut out = Vec::with_capacity_in(bytes.len(), frame);
    let mut copied = 0;
    for at in crlf {
        out.extend_from_slice(&bytes[copied..at]);
        copied = at + 1;
    }
    out.extend_from_slice(&bytes[copied..]);
    std::str::from_utf8(out.leak()).expect("dropping a \\r before each \\n leaves UTF-8 as UTF-8")
}

/// A byte's classes, as bits of [`CLASS`].
const SPACE: u8 = 1;
/// `[A-Za-z0-9_]`: what continues an identifier.
const IDENT: u8 = 2;
/// `[A-Za-z_]`: what starts one.
const START: u8 = 4;
/// The punctuation of Rust's tokens.
const PUNCT: u8 = 8;

/// Every byte's classes: what the lexer asks of a byte is one load and one mask.
static CLASS: [u8; 256] = classes();

const fn classes() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut at = 0;
    while at < 256 {
        let byte = at as u8;
        let mut class = 0;
        if matches!(byte, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c') {
            class |= SPACE;
        }
        if byte.is_ascii_alphanumeric() || byte == b'_' {
            class |= IDENT;
        }
        if byte.is_ascii_alphabetic() || byte == b'_' {
            class |= START;
        }
        if matches!(
            byte,
            b';' | b','
                | b'.'
                | b'('
                | b')'
                | b'{'
                | b'}'
                | b'['
                | b']'
                | b'@'
                | b'#'
                | b'~'
                | b'?'
                | b':'
                | b'$'
                | b'='
                | b'!'
                | b'<'
                | b'>'
                | b'-'
                | b'&'
                | b'|'
                | b'+'
                | b'*'
                | b'/'
                | b'^'
                | b'%'
        ) {
            class |= PUNCT;
        }
        table[at] = class;
        at += 1;
    }
    table
}

fn class(byte: Option<&u8>) -> u8 {
    byte.map_or(0, |it| CLASS[usize::from(*it)])
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

/// The tokens of a normalized source ([`normalize`]), comments other than doc comments left
/// out, read one at a time. After an error it ends.
pub struct Tokens<'a> {
    lexer: Lexer<'a>,
}

impl<'a> Tokens<'a> {
    /// The tokens of `source`, or why it is refused as a whole: a lone `\r` or a text
    /// direction code point anywhere.
    pub fn new(source: &'a str) -> Result<Tokens<'a>, Unlexed> {
        if let Some(at) = memchr::memchr(b'\r', source.as_bytes()) {
            return Err(Unlexed { at, why: "a carriage return not before a line feed" });
        }
        if let Some(at) = text_direction(source) {
            return Err(Unlexed { at, why: "a text direction code point" });
        }
        Ok(Tokens { lexer: Lexer { source, at: 0 } })
    }
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Result<Token<'a>, Unlexed>;

    fn next(&mut self) -> Option<Self::Item> {
        let token = self.lexer.token();
        if token.is_err() {
            self.lexer.at = self.lexer.source.len();
        }
        token.transpose()
    }
}

/// The tokens of a normalized source ([`normalize`]), comments other than doc comments left
/// out.
pub fn lex(source: &str) -> Result<Vec<Token<'_>>, Unlexed> {
    let tokens = Tokens::new(source)?;
    // About one token for every few bytes of a script's source.
    let mut lexed = Vec::with_capacity(source.len() / 4);
    for token in tokens {
        lexed.push(token?);
    }
    Ok(lexed)
}

struct Lexer<'a> {
    source: &'a str,
    at: usize,
}

impl<'a> Lexer<'a> {
    /// The next token, or none at the end.
    fn token(&mut self) -> Result<Option<Token<'a>>, Unlexed> {
        let bytes = self.source.as_bytes();
        loop {
            while class(bytes.get(self.at)) & SPACE != 0 {
                self.at += 1;
            }
            let Some(&byte) = bytes.get(self.at) else { return Ok(None) };
            let start = self.at;
            let after = bytes.get(start + 1).copied();
            let kind = match byte {
                b'/' if after == Some(b'/') => match self.line_comment() {
                    Some(kind) => kind,
                    None => continue,
                },
                b'/' if after == Some(b'*') => match self.block_comment()? {
                    Some(kind) => kind,
                    None => continue,
                },
                b'"' => {
                    self.at += 1;
                    self.string()?
                }
                b'\'' => self.char_or_lifetime()?,
                b'0'..=b'9' => self.number(),
                b'r' if after == Some(b'#') && class(bytes.get(start + 2)) & START != 0 => {
                    self.at += 2;
                    self.ident_rest();
                    Kind::RawIdent
                }
                b'r' if matches!(after, Some(b'"' | b'#')) => {
                    self.at += 1;
                    self.raw_string()?
                }
                _ if CLASS[usize::from(byte)] & START != 0 => {
                    self.at += 1;
                    self.ident_rest();
                    if matches!(bytes.get(self.at), Some(b'"' | b'\'' | b'#')) {
                        return Err(self.fail("a prefixed literal or a reserved prefix"));
                    }
                    Kind::Ident
                }
                _ if CLASS[usize::from(byte)] & PUNCT != 0 => {
                    self.at += 1;
                    Kind::Punct(char::from(byte))
                }
                _ => {
                    return Err(
                        self.fail("a character outside literals and comments elision reads")
                    );
                }
            };
            return Ok(Some(Token { kind, text: &self.source[start..self.at] }));
        }
    }

    /// The character at `at`: one byte read for ASCII, which is nearly all of a script.
    fn peek(&self) -> Option<char> {
        let byte = *self.source.as_bytes().get(self.at)?;
        match byte.is_ascii() {
            true => Some(char::from(byte)),
            false => self.source[self.at..].chars().next(),
        }
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
        let bytes = self.source.as_bytes();
        while class(bytes.get(self.at)) & IDENT != 0 {
            self.at += 1;
        }
    }

    /// A literal's end: rustc reads an identifier right after it as its suffix, which no
    /// string or character literal may have.
    fn no_suffix(&self) -> Result<(), Unlexed> {
        let next = self.source.as_bytes().get(self.at);
        match class(next) & IDENT != 0 || next.is_some_and(|it| !it.is_ascii()) {
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
        let text = &self.source.as_bytes()[start..end];
        if text.starts_with(b"//!") {
            Some(Kind::InnerDoc)
        } else if text.starts_with(b"///") && !text.starts_with(b"////") {
            Some(Kind::OuterDoc)
        } else {
            None
        }
    }

    /// `/* … */`, nested: a doc comment's kind, or none for a plain comment. Only `/` and `*`
    /// matter inside, and memchr jumps between them.
    fn block_comment(&mut self) -> Result<Option<Kind<'a>>, Unlexed> {
        let bytes = self.source.as_bytes();
        let start = self.at;
        self.at += 2;
        let mut depth = 1usize;
        while depth > 0 {
            let Some(found) = memchr::memchr2(b'/', b'*', &bytes[self.at..]) else {
                return Err(Unlexed { at: start, why: "an unterminated block comment" });
            };
            let at = self.at + found;
            match (bytes[at], bytes.get(at + 1)) {
                (b'/', Some(b'*')) => {
                    depth += 1;
                    self.at = at + 2;
                }
                (b'*', Some(b'/')) => {
                    depth -= 1;
                    self.at = at + 2;
                }
                _ => self.at = at + 1,
            }
        }
        let text = &bytes[start..self.at];
        Ok(if text.starts_with(b"/*!") {
            Some(Kind::InnerDoc)
        } else if text.starts_with(b"/**") && !text.starts_with(b"/***") && text != b"/**/" {
            Some(Kind::OuterDoc)
        } else {
            None
        })
    }

    /// The rest of `"…"`, after the opening quote: memchr jumps to each quote or backslash,
    /// and each escape is checked as rustc reads it.
    fn string(&mut self) -> Result<Kind<'a>, Unlexed> {
        let start = self.at - 1;
        let bytes = self.source.as_bytes();
        let (mut escaped, mut escaped_brace) = (false, false);
        loop {
            let Some(found) = memchr::memchr2(b'"', b'\\', &bytes[self.at..]) else {
                return Err(Unlexed { at: start, why: "an unterminated string" });
            };
            self.at += found + 1;
            if bytes[self.at - 1] == b'"' {
                self.no_suffix()?;
                return Ok(Kind::Str { escaped, escaped_brace });
            }
            escaped = true;
            if let Some(c) = self.escape(true)? {
                escaped_brace |= matches!(c, '{' | '}');
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
                while matches!(self.source.as_bytes().get(self.at), Some(b' ' | b'\t' | b'\n')) {
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
        let bytes = self.source.as_bytes();
        let mut hashes = 0usize;
        while bytes.get(self.at) == Some(&b'#') {
            self.at += 1;
            hashes += 1;
        }
        if hashes > 255 || self.bump() != Some('"') {
            return Err(self.fail("a raw string rustc refuses"));
        }
        let content = self.at;
        loop {
            let Some(quote) = memchr::memchr(b'"', &bytes[self.at..]) else {
                return Err(Unlexed { at: start, why: "an unterminated raw string" });
            };
            let close = self.at + quote;
            self.at = close + 1;
            let rest = &bytes[self.at..];
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
        self.at += 1;
        let kind = match self.peek() {
            Some('\\') => {
                self.at += 1;
                let Some(c) = self.escape(false)? else {
                    return Err(self.fail("a line continuation in a character"));
                };
                if self.bump() != Some('\'') {
                    return Err(self.fail("a character literal of more than one character"));
                }
                Kind::Char(c)
            }
            Some(c) if self.source.as_bytes().get(self.at + c.len_utf8()) == Some(&b'\'') => {
                if matches!(c, '\n' | '\r' | '\t' | '\'') {
                    return Err(self.fail("a character rustc wants escaped"));
                }
                self.at += c.len_utf8() + 1;
                Kind::Char(c)
            }
            Some(c) if c.is_ascii() && CLASS[c as usize] & START != 0 => {
                self.ident_rest();
                return Ok(Kind::Lifetime);
            }
            _ => return Err(self.fail("a quote that starts neither a character nor a lifetime")),
        };
        self.no_suffix()?;
        Ok(kind)
    }

    fn number(&mut self) -> Kind<'a> {
        let bytes = self.source.as_bytes();
        let start = self.at;
        if bytes[start] == b'0' && matches!(bytes.get(start + 1), Some(b'x' | b'o' | b'b')) {
            self.at += 2;
            self.ident_rest();
            return Kind::Number;
        }
        self.at +=
            bytes[start..].iter().take_while(|it| it.is_ascii_digit() || **it == b'_').count();
        let digits = &bytes[start..self.at];
        if class(bytes.get(self.at)) & START != 0 {
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

#[cfg(test)]
pub(super) mod tests;
