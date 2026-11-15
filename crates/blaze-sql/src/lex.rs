//! The lexer: source text to tokens, one at a time, each with the bytes it came from.
//!
//! What is not a token of the subset stops here with an error naming it: comments, a byte
//! outside the subset's alphabet, and every parameter form other than `$name` (`?`, `?1`, `$1`,
//! `:name`, `@name`), each by its own kind, so the message can say to name the parameter.
//! Keywords are reserved: a word equal to one (without regard to ASCII case) is always that
//! keyword, and the parser says to quote it where a name was meant.

#![blaze::sans_io]

use core::ops::Range;

use crate::ast::Span;
use crate::parse::{SyntaxError, SyntaxErrorKind};

/// The bytes of a two-byte operator or comment opener: `||`, `<=`, `<>`, `>=`, `!=`, `--`,
/// `/*`.
const PAIR: usize = "<=".len();

/// The bytes of a doubled quote, `''` in a text literal or `""` in a quoted name.
const DOUBLED_QUOTE: usize = "''".len();

/// What opens a blob literal, `x'`; the printer writes it and the lexer skips it.
pub(crate) const BLOB_OPEN: &str = "x'";

/// Integer literals are decimal.
const RADIX: u64 = 10;

/// The subset's keywords, every one reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Keyword {
    Select,
    Distinct,
    From,
    Where,
    Group,
    By,
    Having,
    Order,
    Asc,
    Desc,
    Limit,
    Offset,
    Join,
    Inner,
    Left,
    Outer,
    On,
    As,
    Insert,
    Into,
    Values,
    Update,
    Set,
    Delete,
    Returning,
    And,
    Or,
    Not,
    Is,
    Null,
    In,
    Between,
    Like,
    Case,
    When,
    Then,
    Else,
    End,
    Cast,
    True,
    False,
}

/// Each keyword with its canonical (lower-case) spelling.
const KEYWORDS: [(&str, Keyword); 41] = [
    ("select", Keyword::Select),
    ("distinct", Keyword::Distinct),
    ("from", Keyword::From),
    ("where", Keyword::Where),
    ("group", Keyword::Group),
    ("by", Keyword::By),
    ("having", Keyword::Having),
    ("order", Keyword::Order),
    ("asc", Keyword::Asc),
    ("desc", Keyword::Desc),
    ("limit", Keyword::Limit),
    ("offset", Keyword::Offset),
    ("join", Keyword::Join),
    ("inner", Keyword::Inner),
    ("left", Keyword::Left),
    ("outer", Keyword::Outer),
    ("on", Keyword::On),
    ("as", Keyword::As),
    ("insert", Keyword::Insert),
    ("into", Keyword::Into),
    ("values", Keyword::Values),
    ("update", Keyword::Update),
    ("set", Keyword::Set),
    ("delete", Keyword::Delete),
    ("returning", Keyword::Returning),
    ("and", Keyword::And),
    ("or", Keyword::Or),
    ("not", Keyword::Not),
    ("is", Keyword::Is),
    ("null", Keyword::Null),
    ("in", Keyword::In),
    ("between", Keyword::Between),
    ("like", Keyword::Like),
    ("case", Keyword::Case),
    ("when", Keyword::When),
    ("then", Keyword::Then),
    ("else", Keyword::Else),
    ("end", Keyword::End),
    ("cast", Keyword::Cast),
    ("true", Keyword::True),
    ("false", Keyword::False),
];

/// The keyword `word` is, without regard to ASCII case.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn keyword(word: &str) -> Option<Keyword> {
    KEYWORDS.iter().find(|(text, _)| text.eq_ignore_ascii_case(word)).map(|&(_, keyword)| keyword)
}

/// How `keyword` is written, in lower case: the one place a keyword's text is spelled, which the
/// printer writes from. `None` only if [`KEYWORDS`] missed it.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn keyword_text(keyword: Keyword) -> Option<&'static str> {
    KEYWORDS.iter().find(|&&(_, it)| it == keyword).map(|&(text, _)| text)
}

/// Operators and punctuation. `<>` and `!=` are one token, [`Punct::Ne`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Punct {
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    LParen,
    RParen,
    Comma,
    Dot,
    Semicolon,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Tok<'s> {
    Keyword(Keyword),
    /// An unquoted identifier that is not a keyword.
    Ident(&'s str),
    /// `"..."`: the text between the quotes, never empty and without a `"`.
    Quoted(&'s str),
    /// `$name`: the name without its `$`.
    Param(&'s str),
    Integer(u64),
    /// Finite.
    Real(f64),
    /// `'..'`: the text between the outer quotes, `''` still doubled.
    Text(&'s str),
    /// `x'..'`: the hex digits, an even count of them.
    Blob(&'s str),
    Punct(Punct),
    /// Past the last token; its span is empty, at the end of the text.
    End,
}

impl Tok<'_> {
    /// The keyword this token is, if it is one.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn keyword(self) -> Option<Keyword> {
        if let Tok::Keyword(keyword) = self { Some(keyword) } else { None }
    }

    /// The operator or punctuation this token is, if it is one.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn punct(self) -> Option<Punct> {
        if let Tok::Punct(punct) = self { Some(punct) } else { None }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Token<'s> {
    pub(crate) tok: Tok<'s>,
    pub(crate) span: Span,
}

/// A cursor over the text. It is `Copy`, so the parser looks ahead by lexing a copy.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Lexer<'s> {
    text: &'s str,
    at: usize,
}

/// A refusal of the bytes `range`.
fn error(kind: SyntaxErrorKind<'_>, range: Range<usize>) -> SyntaxError<'_> {
    SyntaxError { kind, span: Span::of(range) }
}

fn is_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_digit(byte: u8) -> bool {
    byte.is_ascii_digit()
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

impl<'s> Lexer<'s> {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn new(text: &'s str) -> Lexer<'s> {
        Lexer { text, at: 0 }
    }

    fn byte(&self, at: usize) -> Option<u8> {
        self.text.as_bytes().get(at).copied()
    }

    /// Bytes `range`; every caller passes offsets at ASCII bytes or the end, which are char
    /// boundaries.
    fn slice(&self, range: Range<usize>) -> &'s str {
        self.text.get(range).unwrap_or("")
    }

    /// The first offset from `at` whose byte is not `keep`, or the end of the text.
    fn scan(&self, at: usize, keep: fn(u8) -> bool) -> usize {
        let bytes = self.text.as_bytes();
        let rest = bytes.get(at..).unwrap_or(&[]);
        at + rest.iter().position(|&byte| !keep(byte)).unwrap_or(rest.len())
    }

    /// The first offset from `at` holding `byte`.
    fn find(&self, at: usize, byte: u8) -> Option<usize> {
        let rest = self.text.as_bytes().get(at..)?;
        rest.iter().position(|&it| it == byte).map(|found| at + found)
    }

    /// The next token, or the error that stops the text at it.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn next_token(&mut self) -> Result<Token<'s>, SyntaxError<'s>> {
        let start = self.scan(self.at, is_space);
        let Some(first) = self.byte(start) else {
            self.at = start;
            return Ok(Token { tok: Tok::End, span: Span::of(start..start) });
        };
        let second = self.byte(start + 1);
        let one = |punct: Punct| -> Result<(Tok<'s>, usize), SyntaxError<'s>> {
            Ok((Tok::Punct(punct), start + 1))
        };
        let two = |punct: Punct| -> Result<(Tok<'s>, usize), SyntaxError<'s>> {
            Ok((Tok::Punct(punct), start + PAIR))
        };
        let comment = || -> Result<(Tok<'s>, usize), SyntaxError<'s>> {
            let opener = start..start + PAIR;
            Err(error(SyntaxErrorKind::Comment(self.slice(opener.clone())), opener))
        };
        let (tok, end) = match first {
            b'+' => one(Punct::Plus),
            b'*' => one(Punct::Star),
            b'%' => one(Punct::Percent),
            b'=' => one(Punct::Eq),
            b'(' => one(Punct::LParen),
            b')' => one(Punct::RParen),
            b',' => one(Punct::Comma),
            b'.' => one(Punct::Dot),
            b';' => one(Punct::Semicolon),
            b'-' if second == Some(b'-') => comment(),
            b'-' => one(Punct::Minus),
            b'/' if second == Some(b'*') => comment(),
            b'/' => one(Punct::Slash),
            b'|' if second == Some(b'|') => two(Punct::Concat),
            b'<' if second == Some(b'=') => two(Punct::Le),
            b'<' if second == Some(b'>') => two(Punct::Ne),
            b'<' => one(Punct::Lt),
            b'>' if second == Some(b'=') => two(Punct::Ge),
            b'>' => one(Punct::Gt),
            b'!' if second == Some(b'=') => two(Punct::Ne),
            b'\'' => self.text_literal(start),
            b'"' => self.quoted_name(start),
            b'$' => self.dollar(start, second),
            b'?' => {
                let end = self.scan(start + 1, is_digit);
                let token = self.slice(start..end);
                let kind = if end == start + 1 {
                    SyntaxErrorKind::AnonymousParameter(token)
                } else {
                    SyntaxErrorKind::NumberedQuestionParameter(token)
                };
                Err(error(kind, start..end))
            }
            b':' | b'@' if second.is_some_and(is_name_start) => {
                let end = self.scan(start + 1, is_name_byte);
                let token = self.slice(start..end);
                let kind = if first == b':' {
                    SyntaxErrorKind::ColonParameter(token)
                } else {
                    SyntaxErrorKind::AtParameter(token)
                };
                Err(error(kind, start..end))
            }
            b'0'..=b'9' => self.number(start),
            b'x' | b'X' if second == Some(b'\'') => self.blob(start),
            byte if is_name_start(byte) => {
                let end = self.scan(start, is_name_byte);
                let word = self.slice(start..end);
                Ok((keyword(word).map_or(Tok::Ident(word), Tok::Keyword), end))
            }
            byte => Err(error(SyntaxErrorKind::UnexpectedByte(byte), start..start + 1)),
        }?;
        self.at = end;
        Ok(Token { tok, span: Span::of(start..end) })
    }

    /// `$name`; `$1` and a bare `$` are refused.
    fn dollar(
        &self,
        start: usize,
        second: Option<u8>,
    ) -> Result<(Tok<'s>, usize), SyntaxError<'s>> {
        match second {
            Some(byte) if is_name_start(byte) => {
                let end = self.scan(start + 1, is_name_byte);
                Ok((Tok::Param(self.slice(start + 1..end)), end))
            }
            Some(byte) if is_digit(byte) => {
                let end = self.scan(start + 1, is_name_byte);
                Err(error(
                    SyntaxErrorKind::NumberedDollarParameter(self.slice(start..end)),
                    start..end,
                ))
            }
            _ => Err(error(SyntaxErrorKind::NamelessParameter, start..start + 1)),
        }
    }

    /// Digits, then an optional `.` and digits, then an optional exponent. With a `.` or an
    /// exponent it is a real, else an integer. A name byte or a `.` right after it makes the
    /// whole run malformed (`12ab`, `1.2.3`), not two tokens.
    fn number(&self, start: usize) -> Result<(Tok<'s>, usize), SyntaxError<'s>> {
        let mut end = self.scan(start, is_digit);
        let mut real = false;
        if self.byte(end) == Some(b'.') {
            real = true;
            end = self.scan(end + 1, is_digit);
        }
        if matches!(self.byte(end), Some(b'e' | b'E')) {
            real = true;
            let mut digits = end + 1;
            if matches!(self.byte(digits), Some(b'+' | b'-')) {
                digits += 1;
            }
            let after = self.scan(digits, is_digit);
            if after == digits {
                let bad = self.scan(digits, is_name_byte);
                return Err(error(
                    SyntaxErrorKind::MalformedNumber(self.slice(start..bad)),
                    start..bad,
                ));
            }
            end = after;
        }
        if self.byte(end).is_some_and(|byte| is_name_byte(byte) || byte == b'.') {
            let bad = self.scan(end, |byte| is_name_byte(byte) || byte == b'.');
            return Err(error(
                SyntaxErrorKind::MalformedNumber(self.slice(start..bad)),
                start..bad,
            ));
        }
        let token = self.slice(start..end);
        if real {
            return match token.parse::<f64>() {
                Ok(value) if value.is_finite() => Ok((Tok::Real(value), end)),
                Ok(_) => Err(error(SyntaxErrorKind::RealNotFinite(token), start..end)),
                Err(unread) => Err(error(
                    SyntaxErrorKind::RealUnreadable { text: token, error: unread },
                    start..end,
                )),
            };
        }
        let mut value: u64 = 0;
        for byte in token.bytes() {
            let digit = u64::from(byte.wrapping_sub(b'0'));
            match value.checked_mul(RADIX).and_then(|it| it.checked_add(digit)) {
                Some(next) => value = next,
                None => return Err(error(SyntaxErrorKind::IntegerTooLarge(token), start..end)),
            }
        }
        Ok((Tok::Integer(value), end))
    }

    /// `'..'` with `''` standing for one quote.
    fn text_literal(&self, start: usize) -> Result<(Tok<'s>, usize), SyntaxError<'s>> {
        let mut at = start + 1;
        loop {
            let Some(quote) = self.find(at, b'\'') else {
                let len = self.text.len();
                return Err(error(
                    SyntaxErrorKind::UnterminatedText(self.slice(start..len)),
                    start..len,
                ));
            };
            if self.byte(quote + 1) == Some(b'\'') {
                at = quote + DOUBLED_QUOTE;
            } else {
                return Ok((Tok::Text(self.slice(start + 1..quote)), quote + 1));
            }
        }
    }

    /// `x'..'`: an even count of hex digits.
    fn blob(&self, start: usize) -> Result<(Tok<'s>, usize), SyntaxError<'s>> {
        let body = start + BLOB_OPEN.len();
        let Some(quote) = self.find(body, b'\'') else {
            let len = self.text.len();
            return Err(error(
                SyntaxErrorKind::UnterminatedBlob(self.slice(start..len)),
                start..len,
            ));
        };
        let token = self.slice(start..quote + 1);
        let digits = self.slice(body..quote);
        if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(error(SyntaxErrorKind::BlobNotHex(token), start..quote + 1));
        }
        if digits.len() % 2 != 0 {
            return Err(error(SyntaxErrorKind::BlobOddDigits(token), start..quote + 1));
        }
        Ok((Tok::Blob(digits), quote + 1))
    }

    /// `"..."`: not empty, and no `"` inside (there is no escape for one).
    fn quoted_name(&self, start: usize) -> Result<(Tok<'s>, usize), SyntaxError<'s>> {
        let body = start + 1;
        let Some(quote) = self.find(body, b'"') else {
            let len = self.text.len();
            return Err(error(
                SyntaxErrorKind::UnterminatedName(self.slice(start..len)),
                start..len,
            ));
        };
        if self.byte(quote + 1) == Some(b'"') {
            let after = quote + DOUBLED_QUOTE;
            let end = self.find(after, b'"').map_or(after, |it| it + 1);
            return Err(error(SyntaxErrorKind::QuoteInName(self.slice(start..end)), start..end));
        }
        if quote == body {
            return Err(error(SyntaxErrorKind::EmptyName, start..quote + 1));
        }
        Ok((Tok::Quoted(self.slice(body..quote)), quote + 1))
    }
}
