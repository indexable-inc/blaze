//! A token by its first byte: [`FIRST`] maps each byte to the class of token it opens, so
//! [`Cursor::advance_token`] takes one table load and one jump where it used to decode a `char`
//! and walk a chain of guarded `char` patterns. Every ASCII path reads bytes only; a byte of
//! `0x80` and above (the lead byte of a multi-byte character: a lexer cursor is always at a
//! character boundary) takes [`Cursor::non_ascii`], the one path that decodes.
//!
//! What the non-ASCII path keeps, from the Reference:
//! - "Whitespace": "Whitespace is any non-empty string containing only characters that have the
//!   `Pattern_White_Space` Unicode property", which beyond ASCII is U+0085 (next line), U+200E
//!   (left-to-right mark), U+200F (right-to-left mark), U+2028 (line separator) and U+2029
//!   (paragraph separator): [`is_whitespace`].
//! - "Identifiers": "IDENTIFIER_OR_KEYWORD : ( XID_Start | `_` ) XID_Continue*": a non-ASCII
//!   character that is XID_Start opens an identifier: [`is_id_start`].
//! - "Tokens" lists no other token a non-ASCII character can open, so anything else is
//!   rustc_lexer's recovery: an emoji opens an invalid identifier, the rest is one unknown
//!   token: [`is_emoji`].
//!
//! The order of the old `char` match is kept where it matters: with frontmatter allowed, any
//! whitespace (ASCII or not) and a `-` before `--` are tried first ([`Cursor::frontmatter_start`]),
//! then the byte's class.

use blaze_int::Cast;

use super::{Cursor, LiteralKind, Prefix, TokenKind, is_emoji, is_id_start, is_whitespace};

/// What token a byte opens. One variant per single-byte punctuation token, so the second
/// dispatch is a jump too.
#[derive(Clone, Copy)]
pub(super) enum First {
    /// `\t \n \x0B \x0C \r` and space: the ASCII part of Pattern_White_Space.
    Space,
    Slash,
    /// `_` and every ASCII letter but the four prefixes below.
    Ident,
    /// `r`: a raw identifier, a raw string, or an identifier.
    R,
    /// `k`: a forced keyword identifier (`k#`), or an identifier.
    K,
    /// `b`: a byte, byte string or raw byte string, or an identifier.
    B,
    /// `c`: a C string or raw C string, or an identifier.
    C,
    Digit,
    Hash,
    Minus,
    Quote,
    DoubleQuote,
    Semi,
    Comma,
    Dot,
    OpenParen,
    CloseParen,
    OpenBrace,
    CloseBrace,
    OpenBracket,
    CloseBracket,
    At,
    Tilde,
    Question,
    Colon,
    Dollar,
    Eq,
    Bang,
    Lt,
    Gt,
    And,
    Or,
    Plus,
    Star,
    Caret,
    Percent,
    /// Every other ASCII byte (control bytes, `\0`, `` ` ``, `\`, DEL): one unknown token.
    Unknown,
    /// `0x80` and above: the lead byte of a multi-byte character.
    NonAscii,
}

const fn first_of(byte: u8) -> First {
    match byte {
        b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ' => First::Space,
        b'/' => First::Slash,
        b'r' => First::R,
        b'k' => First::K,
        b'b' => First::B,
        b'c' => First::C,
        b'a'..=b'z' | b'A'..=b'Z' | b'_' => First::Ident,
        b'0'..=b'9' => First::Digit,
        b'#' => First::Hash,
        b'-' => First::Minus,
        b'\'' => First::Quote,
        b'"' => First::DoubleQuote,
        b';' => First::Semi,
        b',' => First::Comma,
        b'.' => First::Dot,
        b'(' => First::OpenParen,
        b')' => First::CloseParen,
        b'{' => First::OpenBrace,
        b'}' => First::CloseBrace,
        b'[' => First::OpenBracket,
        b']' => First::CloseBracket,
        b'@' => First::At,
        b'~' => First::Tilde,
        b'?' => First::Question,
        b':' => First::Colon,
        b'$' => First::Dollar,
        b'=' => First::Eq,
        b'!' => First::Bang,
        b'<' => First::Lt,
        b'>' => First::Gt,
        b'&' => First::And,
        b'|' => First::Or,
        b'+' => First::Plus,
        b'*' => First::Star,
        b'^' => First::Caret,
        b'%' => First::Percent,
        0x80..=0xFF => First::NonAscii,
        _ => First::Unknown,
    }
}

const fn first_table() -> [First; 256] {
    let mut table = [First::Unknown; 256];
    let mut index = 0usize;
    while index < 256 {
        table[index] = first_of(Cast::<usize, u8>::truncate(index));
        index += 1;
    }
    table
}

/// Each byte's [`First`], indexed by the byte (a `u8` index into 256 entries: no bounds check).
pub(super) static FIRST: [First; 256] = first_table();

impl Cursor<'_> {
    /// With frontmatter still allowed: a whitespace token (or a frontmatter after it), or a
    /// frontmatter opened by `---`; `None` when the token at the cursor is neither and the byte's
    /// class decides it. rustc_lexer's first two arms, character for character.
    #[cold]
    pub(super) fn frontmatter_start(&mut self) -> Option<TokenKind> {
        let first = self.first();
        if is_whitespace(first) {
            self.bump();
            let mut last = first;
            while is_whitespace(self.first()) {
                let Some(c) = self.bump() else {
                    break;
                };
                last = c;
            }
            let kind = if last != '\n' && self.starts_with(b"---") {
                self.bump();
                self.frontmatter(true)
            } else {
                TokenKind::Whitespace
            };
            return Some(kind);
        }
        if first == '-' && self.bytes[self.pos + 1..].starts_with(b"--") {
            self.bump();
            return Some(self.frontmatter(false));
        }
        None
    }

    /// The token whose first byte, `byte`, is at the cursor; the cursor past the token.
    #[inline(always)]
    pub(super) fn by_first_byte(&mut self, byte: u8) -> TokenKind {
        let class = FIRST[usize::from(byte)];
        // Past the first byte: every class but `NonAscii` is that one byte, and `non_ascii`
        // decodes from the byte before the cursor.
        self.pos += 1;
        match class {
            First::Space => self.whitespace(),
            First::Slash => match self.bytes.get(self.pos) {
                Some(b'/') => self.line_comment(),
                Some(b'*') => self.block_comment(),
                _ => TokenKind::Slash,
            },
            First::Ident => self.ident_or_unknown_prefix(),
            First::R => match self.bytes.get(self.pos) {
                // `second` is the character after the `#`, which may be non-ASCII.
                Some(b'#') if is_id_start(self.second()) => {
                    self.pos += 1;
                    self.eat_identifier();
                    TokenKind::RawIdent
                }
                Some(b'#' | b'"') => {
                    let n_hashes = self.raw_double_quoted_string();
                    let suffix_start = self.pos_within_token();
                    if n_hashes.is_some() {
                        self.eat_identifier();
                    }
                    TokenKind::Literal { kind: LiteralKind::RawStr { n_hashes }, suffix_start }
                }
                _ => self.ident_or_unknown_prefix(),
            },
            First::K => match self.bytes.get(self.pos) {
                Some(b'#') if is_id_start(self.second()) => {
                    self.pos += 1;
                    self.eat_identifier();
                    TokenKind::ForcedKeywordIdent
                }
                _ => self.ident_or_unknown_prefix(),
            },
            // `c_or_byte_string` opens a literal only before `'` (bytes only), `"` or `r`; before
            // anything else it is `ident_or_unknown_prefix`, called here without its lookahead.
            First::B => match self.bytes.get(self.pos) {
                Some(b'\'' | b'"' | b'r') => self.c_or_byte_string(Prefix::Byte),
                _ => self.ident_or_unknown_prefix(),
            },
            First::C => match self.bytes.get(self.pos) {
                Some(b'\'' | b'"' | b'r') => self.c_or_byte_string(Prefix::C),
                _ => self.ident_or_unknown_prefix(),
            },
            First::Digit => {
                let kind = self.number(char::from(byte));
                let suffix_start = self.pos_within_token();
                self.eat_identifier();
                TokenKind::Literal { kind, suffix_start }
            }
            First::Hash => match self.bytes.get(self.pos) {
                Some(b'"' | b'#') => {
                    self.pos += 1;
                    TokenKind::GuardedStrPrefix
                }
                _ => TokenKind::Pound,
            },
            First::Minus => TokenKind::Minus,
            First::Quote => self.lifetime_or_char(),
            First::DoubleQuote => {
                let terminated = self.double_quoted_string();
                let suffix_start = self.pos_within_token();
                if terminated {
                    self.eat_identifier();
                }
                TokenKind::Literal { kind: LiteralKind::Str { terminated }, suffix_start }
            }
            First::Semi => TokenKind::Semi,
            First::Comma => TokenKind::Comma,
            First::Dot => TokenKind::Dot,
            First::OpenParen => TokenKind::OpenParen,
            First::CloseParen => TokenKind::CloseParen,
            First::OpenBrace => TokenKind::OpenBrace,
            First::CloseBrace => TokenKind::CloseBrace,
            First::OpenBracket => TokenKind::OpenBracket,
            First::CloseBracket => TokenKind::CloseBracket,
            First::At => TokenKind::At,
            First::Tilde => TokenKind::Tilde,
            First::Question => TokenKind::Question,
            First::Colon => TokenKind::Colon,
            First::Dollar => TokenKind::Dollar,
            First::Eq => TokenKind::Eq,
            First::Bang => TokenKind::Bang,
            First::Lt => TokenKind::Lt,
            First::Gt => TokenKind::Gt,
            First::And => TokenKind::And,
            First::Or => TokenKind::Or,
            First::Plus => TokenKind::Plus,
            First::Star => TokenKind::Star,
            First::Caret => TokenKind::Caret,
            First::Percent => TokenKind::Percent,
            First::Unknown => TokenKind::Unknown,
            First::NonAscii => self.non_ascii(),
        }
    }

    /// The token opened by the multi-byte character whose lead byte is just before the cursor;
    /// the cursor past the token. The Reference's order (see the module's doc): whitespace, then
    /// an identifier, then rustc_lexer's recovery.
    #[cold]
    fn non_ascii(&mut self) -> TokenKind {
        let lead = self.pos - 1;
        // `lead` is a character boundary (the cursor steps by whole characters) holding a byte,
        // so a character starts there.
        let Some(c) = self.text[lead..].chars().next() else {
            unreachable!("byte {lead} is in the text and a character boundary, so a character starts there")
        };
        self.pos = lead + c.len_utf8();
        if is_whitespace(c) {
            self.whitespace()
        } else if is_id_start(c) {
            self.ident_or_unknown_prefix()
        } else if is_emoji(c) {
            self.invalid_ident()
        } else {
            TokenKind::Unknown
        }
    }

    /// The `char` match `advance_token` had before [`FIRST`], kept as the reference the tests
    /// compare the table against, token for token.
    #[cfg(test)]
    pub(crate) fn advance_token_reference(&mut self) -> super::Token {
        let Some(first_char) = self.bump() else {
            return super::Token { kind: TokenKind::Eof, len: 0 };
        };
        let kind = match first_char {
            c if self.frontmatter && is_whitespace(c) => {
                let mut last = first_char;
                while is_whitespace(self.first()) {
                    let Some(c) = self.bump() else {
                        break;
                    };
                    last = c;
                }
                if last != '\n' && self.starts_with(b"---") {
                    self.bump();
                    self.frontmatter(true)
                } else {
                    TokenKind::Whitespace
                }
            }
            '-' if self.frontmatter && self.starts_with(b"--") => self.frontmatter(false),
            '/' => match self.first() {
                '/' => self.line_comment(),
                '*' => self.block_comment(),
                _ => TokenKind::Slash,
            },
            c if is_whitespace(c) => self.whitespace(),
            'r' => match (self.first(), self.second()) {
                ('#', c1) if is_id_start(c1) => {
                    self.bump();
                    self.eat_identifier();
                    TokenKind::RawIdent
                }
                ('#', _) | ('"', _) => {
                    let n_hashes = self.raw_double_quoted_string();
                    let suffix_start = self.pos_within_token();
                    if n_hashes.is_some() {
                        self.eat_identifier();
                    }
                    TokenKind::Literal { kind: LiteralKind::RawStr { n_hashes }, suffix_start }
                }
                _ => self.ident_or_unknown_prefix(),
            },
            'k' => match (self.first(), self.second()) {
                ('#', c1) if is_id_start(c1) => {
                    self.bump();
                    self.eat_identifier();
                    TokenKind::ForcedKeywordIdent
                }
                _ => self.ident_or_unknown_prefix(),
            },
            'b' => self.c_or_byte_string(Prefix::Byte),
            'c' => self.c_or_byte_string(Prefix::C),
            c if is_id_start(c) => self.ident_or_unknown_prefix(),
            c @ '0'..='9' => {
                let kind = self.number(c);
                let suffix_start = self.pos_within_token();
                self.eat_identifier();
                TokenKind::Literal { kind, suffix_start }
            }
            '#' if matches!(self.first(), '"' | '#') => {
                self.bump();
                TokenKind::GuardedStrPrefix
            }
            ';' => TokenKind::Semi,
            ',' => TokenKind::Comma,
            '.' => TokenKind::Dot,
            '(' => TokenKind::OpenParen,
            ')' => TokenKind::CloseParen,
            '{' => TokenKind::OpenBrace,
            '}' => TokenKind::CloseBrace,
            '[' => TokenKind::OpenBracket,
            ']' => TokenKind::CloseBracket,
            '@' => TokenKind::At,
            '#' => TokenKind::Pound,
            '~' => TokenKind::Tilde,
            '?' => TokenKind::Question,
            ':' => TokenKind::Colon,
            '$' => TokenKind::Dollar,
            '=' => TokenKind::Eq,
            '!' => TokenKind::Bang,
            '<' => TokenKind::Lt,
            '>' => TokenKind::Gt,
            '-' => TokenKind::Minus,
            '&' => TokenKind::And,
            '|' => TokenKind::Or,
            '+' => TokenKind::Plus,
            '*' => TokenKind::Star,
            '^' => TokenKind::Caret,
            '%' => TokenKind::Percent,
            '\'' => self.lifetime_or_char(),
            '"' => {
                let terminated = self.double_quoted_string();
                let suffix_start = self.pos_within_token();
                if terminated {
                    self.eat_identifier();
                }
                TokenKind::Literal { kind: LiteralKind::Str { terminated }, suffix_start }
            }
            c if is_emoji(c) => self.invalid_ident(),
            _ => TokenKind::Unknown,
        };
        if self.frontmatter && kind != TokenKind::Whitespace {
            self.frontmatter = false;
        }
        let token = super::Token { kind, len: self.pos_within_token() };
        self.start = self.pos;
        token
    }
}

/// The tokens of `input` by [`Cursor::advance_token_reference`], the `char` match the table
/// replaced: the tests' second reference beside rustc_lexer.
#[cfg(test)]
pub(crate) fn tokenize_reference(
    input: &str,
    frontmatter: super::FrontmatterAllowed,
    cpu: crate::Cpu,
) -> impl Iterator<Item = super::Token> + '_ {
    let mut cursor = Cursor::new(input, 0, frontmatter == super::FrontmatterAllowed::Yes, cpu, super::Plant::Clean);
    core::iter::from_fn(move || {
        let token = cursor.advance_token_reference();
        if token.kind == TokenKind::Eof { None } else { Some(token) }
    })
}
