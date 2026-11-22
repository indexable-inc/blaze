//! `rustc_lexer::tokenize`, token for token: the same kinds, lengths and error kinds on every
//! input (rule 4), from a port of its `Cursor` that steps by byte offsets and takes its runs by
//! mask.
//!
//! What is fast: identifier, whitespace and digit runs by the classifier's masks and
//! `trailing_ones`, a 64-byte block classified once as the cursor reaches it; line comments,
//! strings, raw strings and block comments by byte search for the only bytes that can end them
//! (`\n`; `"` and `\`; `"`; `/` and `*`). What stays a character at a time, exactly as
//! rustc_lexer has it: numeric bases and exponents, lifetimes against characters, prefixes, and
//! every non-ASCII character, which calls the same `blaze_unicode` tables rustc_lexer does.

use blaze_int::Int;
use blaze_memchr::{memchr, memchr2, memmem, memrchr};

use crate::Cpu;
use crate::classify::{RUN_DIGITS, RUN_IDENT, RUN_SPACE, Runs, select_at};

/// The first-byte table `advance_token` dispatches through, and the `char` match it replaced
/// (the tests' reference).
pub(crate) mod first;

/// A token: its kind and its length in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub len: u32,
}

/// rustc_lexer's `TokenKind`, variant for variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    LineComment { doc_style: Option<DocStyle> },
    BlockComment { doc_style: Option<DocStyle>, terminated: bool },
    Whitespace,
    Frontmatter { has_invalid_preceding_whitespace: bool, invalid_infostring: bool },
    Ident,
    InvalidIdent,
    RawIdent,
    ForcedKeywordIdent,
    UnknownPrefix,
    UnknownPrefixLifetime,
    RawLifetime,
    GuardedStrPrefix,
    Literal { kind: LiteralKind, suffix_start: u32 },
    Lifetime { starts_with_number: bool },
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
    Pound,
    Tilde,
    Question,
    Colon,
    Dollar,
    Eq,
    Bang,
    Lt,
    Gt,
    Minus,
    And,
    Or,
    Plus,
    Star,
    Slash,
    Caret,
    Percent,
    Unknown,
    Eof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocStyle {
    Outer,
    Inner,
}

/// rustc_lexer's `LiteralKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LiteralKind {
    Int { base: Base, empty_int: bool },
    Float { base: Base, empty_exponent: bool },
    Char { terminated: bool },
    Byte { terminated: bool },
    Str { terminated: bool },
    ByteStr { terminated: bool },
    CStr { terminated: bool },
    RawStr { n_hashes: Option<u8> },
    RawByteStr { n_hashes: Option<u8> },
    RawCStr { n_hashes: Option<u8> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Base {
    Binary = 2,
    Octal = 8,
    Decimal = 10,
    Hexadecimal = 16,
}

/// Whether a frontmatter may open the text: yes for a whole file (after its shebang), no for a
/// slice of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontmatterAllowed {
    Yes,
    No,
}

/// The tokens of `input`, as `rustc_lexer::tokenize(input, frontmatter)` gives them.
pub fn tokenize(input: &str, frontmatter: FrontmatterAllowed, cpu: Cpu) -> Tokens<'_> {
    Tokens { cursor: Cursor::new(input, 0, frontmatter == FrontmatterAllowed::Yes, cpu, Plant::Clean) }
}

/// The iterator [`tokenize`] returns.
pub struct Tokens<'a> {
    cursor: Cursor<'a>,
}

impl Iterator for Tokens<'_> {
    type Item = Token;

    #[inline]
    fn next(&mut self) -> Option<Token> {
        let token = self.cursor.advance_token();
        if token.kind == TokenKind::Eof { None } else { Some(token) }
    }
}

/// A bug planted on purpose, for the controls of the tests (rule 53); `Clean` everywhere else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    Clean,
    /// A `\` that is the last byte of a 64-byte block does not escape the `"` after it.
    #[cfg_attr(not(test), expect(dead_code, reason = "planted only by the tests' controls"))]
    EscapedQuoteAtBlockEnd,
    /// A `'` followed by an identifier always opens a character literal.
    #[cfg_attr(not(test), expect(dead_code, reason = "planted only by the tests' controls"))]
    LifetimeAsChar,
}

#[cfg(test)]
pub(crate) fn tokenize_planted(input: &str, frontmatter: FrontmatterAllowed, cpu: Cpu, plant: Plant) -> Tokens<'_> {
    Tokens { cursor: Cursor::new(input, 0, frontmatter == FrontmatterAllowed::Yes, cpu, plant) }
}

const EOF_CHAR: char = '\0';

/// Pattern_White_Space, rustc_lexer's `is_whitespace`.
pub(crate) fn is_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0085}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{0009}'
            | '\u{0020}'
    )
}

fn is_horizontal_whitespace(c: char) -> bool {
    matches!(c, '\u{0009}' | '\u{0020}')
}

/// `_` or XID_Start; ASCII answered without the table (its XID_Start is the letters).
pub(crate) fn is_id_start(c: char) -> bool {
    if c.is_ascii() { c == '_' || c.is_ascii_alphabetic() } else { blaze_unicode::is_xid_start(c) }
}

/// XID_Continue; ASCII answered without the table (its XID_Continue is `0-9 A-Z a-z _`).
pub(crate) fn is_id_continue(c: char) -> bool {
    if c.is_ascii() { c == '_' || c.is_ascii_alphanumeric() } else { blaze_unicode::is_xid_continue(c) }
}

fn is_emoji(c: char) -> bool {
    !c.is_ascii() && blaze_unicode::is_emoji(c)
}

/// The end of a double-quoted string whose opening quote ends at `from`: the offset after its
/// closing quote, or `None` when the text ends first. rustc_lexer's rule: a `\` followed by `\`
/// or `"` takes that byte with it.
pub(crate) fn quoted_end(bytes: &[u8], from: usize, plant: Plant) -> Option<usize> {
    let mut at = from;
    loop {
        let found = at + memchr2(b'"', b'\\', bytes.get(at..)?)?;
        at = found + 1;
        if bytes[found] == b'"' {
            return Some(at);
        }
        if plant == Plant::EscapedQuoteAtBlockEnd && at % 64 == 0 {
            continue;
        }
        if matches!(bytes.get(at), Some(b'\\' | b'"')) {
            at += 1;
        }
    }
}

/// The end of a block comment whose `/*` ends at `from`, and whether it closed: nested `/*`
/// and `*/` counted as rustc_lexer counts them, a character at a time, which only `/` and `*`
/// can change (no other character's bytes hold either).
pub(crate) fn block_comment_end(bytes: &[u8], from: usize) -> (usize, bool) {
    let mut depth = 1usize;
    let mut at = from;
    loop {
        let Some(found) = bytes.get(at..).and_then(|rest| memchr2(b'/', b'*', rest)) else {
            return (bytes.len(), false);
        };
        let found = at + found;
        at = found + 1;
        let next = bytes.get(at).copied();
        if bytes[found] == b'/' && next == Some(b'*') {
            at += 1;
            depth += 1;
        } else if bytes[found] == b'*' && next == Some(b'/') {
            at += 1;
            depth -= 1;
            if depth == 0 {
                return (at, true);
            }
        }
    }
}

/// rustc_lexer's `Cursor`, by byte offset into the whole text.
pub(crate) struct Cursor<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
    /// Where the token being lexed starts.
    start: usize,
    frontmatter: bool,
    cpu: Cpu,
    plant: Plant,
    /// The block whose run masks `runs` holds, by index; `usize::MAX` for none yet.
    block: usize,
    runs: [u64; 3],
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(text: &'a str, pos: usize, frontmatter: bool, cpu: Cpu, plant: Plant) -> Self {
        Cursor { text, bytes: text.as_bytes(), pos, start: pos, frontmatter, cpu, plant, block: usize::MAX, runs: [0; 3] }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    #[inline(always)]
    fn char_at(&self, at: usize) -> Option<char> {
        let &byte = self.bytes.get(at)?;
        if byte < 0x80 {
            return Some(char::from(byte));
        }
        // `at` is a character boundary: every step of the cursor is a whole character.
        self.text[at..].chars().next()
    }

    /// The `n`th character from the cursor (0 is the next), `'\0'` past the end.
    #[inline(always)]
    fn peek(&self, n: usize) -> char {
        let mut at = self.pos;
        for _ in 0..n {
            match self.char_at(at) {
                Some(c) => at += c.len_utf8(),
                None => return EOF_CHAR,
            }
        }
        self.char_at(at).unwrap_or(EOF_CHAR)
    }

    #[inline(always)]
    fn first(&self) -> char {
        self.peek(0)
    }

    #[inline(always)]
    fn second(&self) -> char {
        self.peek(1)
    }

    #[inline(always)]
    fn third(&self) -> char {
        self.peek(2)
    }

    #[inline(always)]
    fn is_eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    #[inline(always)]
    fn pos_within_token(&self) -> u32 {
        // rustc_lexer's lengths are `u32` too; a token of 4 GiB is past every source file.
        (self.pos - self.start).narrow_or_stop::<u32>()
    }

    #[inline(always)]
    fn bump(&mut self) -> Option<char> {
        let c = self.char_at(self.pos)?;
        self.pos += c.len_utf8();
        Some(c)
    }

    #[inline(always)]
    fn eat_while(&mut self, mut predicate: impl FnMut(char) -> bool) {
        while predicate(self.first()) && !self.is_eof() {
            self.bump();
        }
    }

    fn eat_until(&mut self, byte: u8) {
        self.pos = match memchr(byte, &self.bytes[self.pos..]) {
            Some(index) => self.pos + index,
            None => self.bytes.len(),
        };
    }

    fn starts_with(&self, prefix: &[u8]) -> bool {
        self.bytes[self.pos..].starts_with(prefix)
    }

    /// Past the run of bytes of run class `class` at the cursor: whole 64-byte blocks of masks,
    /// `trailing_ones` of each from the cursor's lane.
    #[inline(always)]
    fn run(&mut self, class: usize) {
        // The scalar twin tests each byte once, as rustc_lexer does: its masks would be three
        // predicates on each of 64 bytes per block, for a run of a few.
        if self.cpu.path == crate::cpu::Path::Scalar {
            let holds = |byte: u8| match class {
                RUN_IDENT => byte.is_ascii_alphanumeric() || byte == b'_',
                RUN_SPACE => matches!(byte, b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' '),
                _ => byte.is_ascii_digit() || byte == b'_',
            };
            while self.bytes.get(self.pos).is_some_and(|&byte| holds(byte)) {
                self.pos += 1;
            }
            return;
        }
        loop {
            if self.pos >= self.bytes.len() {
                return;
            }
            let block = self.pos / 64;
            let lane = self.pos % 64;
            if block != self.block {
                self.runs = select_at::<Runs, 3>(self.cpu, self.bytes, block * 64);
                self.block = block;
            }
            let ran = (self.runs[class] >> lane).trailing_ones().widen_to::<usize>();
            self.pos += ran;
            if ran < 64 - lane {
                return;
            }
        }
    }

    /// `eat_while(is_id_continue)`: ASCII by mask, then a character at a time past each
    /// non-ASCII one that continues an identifier.
    #[inline(always)]
    fn eat_id_continue(&mut self) {
        loop {
            self.run(RUN_IDENT);
            match self.bytes.get(self.pos) {
                Some(&byte) if byte >= 0x80 => {
                    let c = self.first();
                    if !is_id_continue(c) {
                        return;
                    }
                    self.pos += c.len_utf8();
                }
                _ => return,
            }
        }
    }

    /// Parses a token from the input string (rustc_lexer's `advance_token`): by its first byte
    /// through `first::FIRST`, a table load and a jump; only a byte of `0x80` and above decodes
    /// a `char` (`first.rs` says which of the Reference's rules that path keeps). The `char`
    /// match this replaced is `advance_token_reference`, which the tests compare it against.
    #[inline]
    pub(crate) fn advance_token(&mut self) -> Token {
        let Some(&byte) = self.bytes.get(self.pos) else {
            return Token { kind: TokenKind::Eof, len: 0 };
        };
        let fronted = if self.frontmatter { self.frontmatter_start() } else { None };
        let kind = match fronted {
            Some(kind) => kind,
            None => self.by_first_byte(byte),
        };
        if self.frontmatter && kind != TokenKind::Whitespace {
            self.frontmatter = false;
        }
        let token = Token { kind, len: self.pos_within_token() };
        self.start = self.pos;
        token
    }

    /// The token at the cursor, lexed, and the cursor past it.
    pub(crate) fn next_end(&mut self) -> usize {
        self.advance_token();
        self.pos
    }

    /// Given that one `-` was eaten, eat the rest of the frontmatter.
    fn frontmatter(&mut self, has_invalid_preceding_whitespace: bool) -> TokenKind {
        let dashes_from = self.pos;
        self.eat_while(|c| c == '-');
        // One `-` is eaten by the caller; a dash is one byte.
        let length_opening = self.pos - dashes_from + 1;
        self.eat_while(|ch| ch != '\n' && is_horizontal_whitespace(ch));
        if is_id_start(self.first()) {
            self.bump();
            self.eat_while(|c| is_id_continue(c) || c == '-' || c == '.');
        }
        self.eat_while(|ch| ch != '\n' && is_horizontal_whitespace(ch));
        let invalid_infostring = self.first() != '\n';

        let rest = &self.bytes[self.pos..];
        if let Some(closing) = find_fence(rest, length_opening) {
            self.pos += closing + 1 + length_opening;
            self.eat_until(b'\n');
        } else {
            let potential = memmem::find(rest, b"\n---")
                .map(|x| x + 1)
                .or_else(|| memmem::find(rest, b"\nuse "))
                .or_else(|| memmem::find(rest, b"\n//!"))
                .or_else(|| memmem::find(rest, b"\n#!["))
                .or_else(|| dashes_alone(rest));
            match potential {
                Some(potential) => {
                    self.pos += potential;
                    self.eat_until(b'\n');
                }
                None => self.pos = self.bytes.len(),
            }
        }
        TokenKind::Frontmatter { has_invalid_preceding_whitespace, invalid_infostring }
    }

    /// After the first `/`, with a second `/` at the cursor. Lookahead by byte: `!` and `/` are
    /// ASCII, and no byte of a multi-byte character equals either.
    fn line_comment(&mut self) -> TokenKind {
        self.pos += 1;
        let doc_style = match (self.bytes.get(self.pos), self.bytes.get(self.pos + 1)) {
            (Some(b'!'), _) => Some(DocStyle::Inner),
            (Some(b'/'), next) if next != Some(&b'/') => Some(DocStyle::Outer),
            _ => None,
        };
        self.eat_until(b'\n');
        TokenKind::LineComment { doc_style }
    }

    /// After the `/`, with a `*` at the cursor. Lookahead by byte, as in `line_comment`.
    fn block_comment(&mut self) -> TokenKind {
        self.pos += 1;
        let doc_style = match (self.bytes.get(self.pos), self.bytes.get(self.pos + 1)) {
            (Some(b'!'), _) => Some(DocStyle::Inner),
            (Some(b'*'), next) if !matches!(next, Some(b'*' | b'/')) => Some(DocStyle::Outer),
            _ => None,
        };
        let (end, terminated) = block_comment_end(self.bytes, self.pos);
        self.pos = end;
        TokenKind::BlockComment { doc_style, terminated }
    }

    fn whitespace(&mut self) -> TokenKind {
        loop {
            self.run(RUN_SPACE);
            match self.bytes.get(self.pos) {
                Some(&byte) if byte >= 0x80 => {
                    let c = self.first();
                    if !is_whitespace(c) {
                        break;
                    }
                    self.pos += c.len_utf8();
                }
                _ => break,
            }
        }
        TokenKind::Whitespace
    }

    fn ident_or_unknown_prefix(&mut self) -> TokenKind {
        self.eat_id_continue();
        // By byte: the three prefix quotes are ASCII, and only a non-ASCII character can be an
        // emoji, so only a byte of `0x80` and above decodes.
        match self.bytes.get(self.pos) {
            Some(b'#' | b'"' | b'\'') => TokenKind::UnknownPrefix,
            Some(&byte) if byte >= 0x80 && is_emoji(self.first()) => self.invalid_ident(),
            _ => TokenKind::Ident,
        }
    }

    fn invalid_ident(&mut self) -> TokenKind {
        const ZERO_WIDTH_JOINER: char = '\u{200d}';
        self.eat_while(|c| is_id_continue(c) || is_emoji(c) || c == ZERO_WIDTH_JOINER);
        TokenKind::InvalidIdent
    }

    fn c_or_byte_string(&mut self, prefix: Prefix) -> TokenKind {
        match (self.first(), self.second(), prefix) {
            ('\'', _, Prefix::Byte) => {
                self.bump();
                let terminated = self.single_quoted_string();
                let suffix_start = self.pos_within_token();
                if terminated {
                    self.eat_identifier();
                }
                TokenKind::Literal { kind: LiteralKind::Byte { terminated }, suffix_start }
            }
            ('"', _, _) => {
                self.bump();
                let terminated = self.double_quoted_string();
                let suffix_start = self.pos_within_token();
                if terminated {
                    self.eat_identifier();
                }
                let kind = match prefix {
                    Prefix::Byte => LiteralKind::ByteStr { terminated },
                    Prefix::C => LiteralKind::CStr { terminated },
                };
                TokenKind::Literal { kind, suffix_start }
            }
            ('r', '"', _) | ('r', '#', _) => {
                self.bump();
                let n_hashes = self.raw_double_quoted_string();
                let suffix_start = self.pos_within_token();
                if n_hashes.is_some() {
                    self.eat_identifier();
                }
                let kind = match prefix {
                    Prefix::Byte => LiteralKind::RawByteStr { n_hashes },
                    Prefix::C => LiteralKind::RawCStr { n_hashes },
                };
                TokenKind::Literal { kind, suffix_start }
            }
            _ => self.ident_or_unknown_prefix(),
        }
    }

    fn number(&mut self, first_digit: char) -> LiteralKind {
        let mut base = Base::Decimal;
        if first_digit == '0' {
            match self.first() {
                'b' => {
                    base = Base::Binary;
                    self.bump();
                    if !self.eat_decimal_digits() {
                        return LiteralKind::Int { base, empty_int: true };
                    }
                }
                'o' => {
                    base = Base::Octal;
                    self.bump();
                    if !self.eat_decimal_digits() {
                        return LiteralKind::Int { base, empty_int: true };
                    }
                }
                'x' => {
                    base = Base::Hexadecimal;
                    self.bump();
                    if !self.eat_hexadecimal_digits() {
                        return LiteralKind::Int { base, empty_int: true };
                    }
                }
                '0'..='9' | '_' => {
                    self.eat_decimal_digits();
                }
                '.' | 'e' | 'E' => {}
                _ => return LiteralKind::Int { base, empty_int: false },
            }
        } else {
            self.eat_decimal_digits();
        }

        match self.first() {
            '.' if self.second() != '.' && !is_id_start(self.second()) => {
                self.bump();
                let mut empty_exponent = false;
                if self.first().is_ascii_digit() {
                    self.eat_decimal_digits();
                    if matches!(self.first(), 'e' | 'E') {
                        self.bump();
                        empty_exponent = !self.eat_float_exponent();
                    }
                }
                LiteralKind::Float { base, empty_exponent }
            }
            'e' | 'E' => {
                self.bump();
                let empty_exponent = !self.eat_float_exponent();
                LiteralKind::Float { base, empty_exponent }
            }
            _ => LiteralKind::Int { base, empty_int: false },
        }
    }

    fn lifetime_or_char(&mut self) -> TokenKind {
        let can_be_a_lifetime = if self.second() == '\'' {
            false
        } else {
            is_id_start(self.first()) || self.first().is_ascii_digit()
        };
        let can_be_a_lifetime = can_be_a_lifetime && self.plant != Plant::LifetimeAsChar;

        if !can_be_a_lifetime {
            let terminated = self.single_quoted_string();
            let suffix_start = self.pos_within_token();
            if terminated {
                self.eat_identifier();
            }
            return TokenKind::Literal { kind: LiteralKind::Char { terminated }, suffix_start };
        }

        if self.first() == 'r' && self.second() == '#' && is_id_start(self.third()) {
            self.bump();
            self.bump();
            self.bump();
            self.eat_id_continue();
            return TokenKind::RawLifetime;
        }

        let starts_with_number = self.first().is_ascii_digit();
        self.bump();
        self.eat_id_continue();

        match self.first() {
            '\'' => {
                self.bump();
                TokenKind::Literal { kind: LiteralKind::Char { terminated: true }, suffix_start: self.pos_within_token() }
            }
            '#' if !starts_with_number => TokenKind::UnknownPrefixLifetime,
            _ => TokenKind::Lifetime { starts_with_number },
        }
    }

    fn single_quoted_string(&mut self) -> bool {
        if self.second() == '\'' && self.first() != '\\' {
            self.bump();
            self.bump();
            return true;
        }
        loop {
            match self.first() {
                '\'' => {
                    self.bump();
                    return true;
                }
                '/' => break,
                '\n' if self.second() != '\'' => break,
                EOF_CHAR if self.is_eof() => break,
                '\\' => {
                    self.bump();
                    self.bump();
                }
                _ => {
                    self.bump();
                }
            }
        }
        false
    }

    fn double_quoted_string(&mut self) -> bool {
        match quoted_end(self.bytes, self.pos, self.plant) {
            Some(end) => {
                self.pos = end;
                true
            }
            None => {
                self.pos = self.bytes.len();
                false
            }
        }
    }

    /// rustc_lexer's `raw_double_quoted_string`, its error kept only as `None`, which is all
    /// a token says of it.
    fn raw_double_quoted_string(&mut self) -> Option<u8> {
        let mut n_start_hashes = 0u32;
        while self.first() == '#' {
            n_start_hashes += 1;
            self.bump();
        }
        if self.bump() != Some('"') {
            return None;
        }
        loop {
            self.eat_until(b'"');
            if self.is_eof() {
                return None;
            }
            self.bump();
            let mut n_end_hashes = 0;
            while self.first() == '#' && n_end_hashes < n_start_hashes {
                n_end_hashes += 1;
                self.bump();
            }
            if n_end_hashes == n_start_hashes {
                return u8::try_from(n_start_hashes).ok();
            }
        }
    }

    /// Digits and `_` by mask; whether any was a digit.
    fn eat_decimal_digits(&mut self) -> bool {
        let from = self.pos;
        self.run(RUN_DIGITS);
        self.bytes[from..self.pos].iter().any(u8::is_ascii_digit)
    }

    fn eat_hexadecimal_digits(&mut self) -> bool {
        let mut has_digits = false;
        loop {
            match self.first() {
                '_' => {
                    self.bump();
                }
                '0'..='9' | 'a'..='f' | 'A'..='F' => {
                    has_digits = true;
                    self.bump();
                }
                _ => break,
            }
        }
        has_digits
    }

    fn eat_float_exponent(&mut self) -> bool {
        if self.first() == '-' || self.first() == '+' {
            self.bump();
        }
        self.eat_decimal_digits()
    }

    /// An identifier (a literal's suffix, a raw identifier's name), `_` included.
    pub(crate) fn eat_identifier(&mut self) {
        if !is_id_start(self.first()) {
            return;
        }
        self.bump();
        self.eat_id_continue();
    }
}

#[derive(Clone, Copy)]
enum Prefix {
    Byte,
    C,
}

/// The first `\n` followed by `length` dashes in `rest`: `str::find` of that pattern, which can
/// start only at a newline.
fn find_fence(rest: &[u8], length: usize) -> Option<usize> {
    let mut from = 0;
    while let Some(found) = memchr(b'\n', &rest[from..]) {
        let newline = from + found;
        let dashes = &rest[newline + 1..];
        if dashes.len() >= length && dashes[..length].iter().all(|&b| b == b'-') {
            return Some(newline);
        }
        from = newline + 1;
    }
    None
}

/// rustc_lexer's last recovery: the first `---` whose line, from the start of what is left
/// after the previous `---` tried, holds only horizontal whitespace before it.
fn dashes_alone(rest: &[u8]) -> Option<usize> {
    let mut base = 0;
    let mut left = rest;
    while let Some(closing) = memmem::find(left, b"---") {
        let line_start = memrchr(b'\n', &left[..closing]).map_or(0, |i| i + 1);
        if left[line_start..closing].iter().all(|&b| b == b' ' || b == b'\t') {
            return Some(closing + base);
        }
        left = &left[closing + 3..];
        base += closing + 3;
    }
    None
}
