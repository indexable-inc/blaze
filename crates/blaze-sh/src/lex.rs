//! Lexer, live syntax highlighter, and multiline completeness checker for `blaze-sh`.
//!
//! Pure, sans-IO, `#![no_std]`, `#![forbid(unsafe_code)]`. Every allocation goes into the
//! caller's [`Alloc`], and every recursive descent or scan loop is explicitly bounded.

use blaze_own::{ARc, AString, AVec, Alloc, Rc};

use crate::error::{ErrorKind, ShellError};
use crate::span::Span;

/// Maximum nesting depth for interpolated strings (`"a ("b ($c)")"`) and raw Rust blocks.
pub const MAX_LEX_DEPTH: usize = 64;

/// Maximum number of `#` delimiters on raw hash strings (`r###'...'###`).
const MAX_RAW_HASHES: usize = 64;

/// Maximum number of tokens emitted from a single source submission.
pub const MAX_TOKENS: usize = 262_144;

/// Token category for live syntax highlighting in the GUI editor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HighlightKind {
    Keyword,
    Command,
    Flag,
    String,
    Number,
    Unit,
    Var,
    Operator,
    Pipe,
    Punct,
    Comment,
    RustCode,
    Error,
}

/// One highlighted byte range returned by [`highlight`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HighlightSpan {
    pub kind: HighlightKind,
    pub span: Span,
}

/// Keywords of `blaze-sh`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Keyword {
    Let,
    Mut,
    Def,
    Alias,
    If,
    Else,
    Match,
    For,
    In,
    While,
    Loop,
    Break,
    Continue,
    Return,
    Try,
    Catch,
    True,
    False,
    Null,
    And,
    Or,
    Xor,
    Not,
    NotIn,
    StartsWith,
    EndsWith,
    Mod,
    Rust,
}

impl Keyword {
    pub fn from_name(s: &str) -> Option<Keyword> {
        match s {
            "let" => Some(Keyword::Let),
            "mut" => Some(Keyword::Mut),
            "def" => Some(Keyword::Def),
            "alias" => Some(Keyword::Alias),
            "if" => Some(Keyword::If),
            "else" => Some(Keyword::Else),
            "match" => Some(Keyword::Match),
            "for" => Some(Keyword::For),
            "in" => Some(Keyword::In),
            "while" => Some(Keyword::While),
            "loop" => Some(Keyword::Loop),
            "break" => Some(Keyword::Break),
            "continue" => Some(Keyword::Continue),
            "return" => Some(Keyword::Return),
            "try" => Some(Keyword::Try),
            "catch" => Some(Keyword::Catch),
            "true" => Some(Keyword::True),
            "false" => Some(Keyword::False),
            "null" => Some(Keyword::Null),
            "and" => Some(Keyword::And),
            "or" => Some(Keyword::Or),
            "xor" => Some(Keyword::Xor),
            "not" => Some(Keyword::Not),
            "not-in" => Some(Keyword::NotIn),
            "starts-with" => Some(Keyword::StartsWith),
            "ends-with" => Some(Keyword::EndsWith),
            "mod" => Some(Keyword::Mod),
            "rust" => Some(Keyword::Rust),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Keyword::Let => "let",
            Keyword::Mut => "mut",
            Keyword::Def => "def",
            Keyword::Alias => "alias",
            Keyword::If => "if",
            Keyword::Else => "else",
            Keyword::Match => "match",
            Keyword::For => "for",
            Keyword::In => "in",
            Keyword::While => "while",
            Keyword::Loop => "loop",
            Keyword::Break => "break",
            Keyword::Continue => "continue",
            Keyword::Return => "return",
            Keyword::Try => "try",
            Keyword::Catch => "catch",
            Keyword::True => "true",
            Keyword::False => "false",
            Keyword::Null => "null",
            Keyword::And => "and",
            Keyword::Or => "or",
            Keyword::Xor => "xor",
            Keyword::Not => "not",
            Keyword::NotIn => "not-in",
            Keyword::StartsWith => "starts-with",
            Keyword::EndsWith => "ends-with",
            Keyword::Mod => "mod",
            Keyword::Rust => "rust",
        }
    }
}

/// One piece inside an interpolated double-quoted string (`"hello ($name)!"`).
#[derive(Clone, Debug)]
pub enum LexInterpPart<'a> {
    Literal(ARc<'a, str>, Span),
    Tokens(AVec<'a, Token<'a>>, Span),
}

/// Lexer token payload.
#[derive(Clone, Debug)]
pub enum TokKind<'a> {
    Kw(Keyword),
    Ident(ARc<'a, str>),
    /// Variable reference without the leading `$` (`$x` -> `"x"`).
    Var(ARc<'a, str>),
    /// Bare `$` (for `$.a.b` bare cell-path literals).
    Dollar,
    Int(i64),
    Float(f64),
    Size(i64),
    Duration(i64),
    Str(ARc<'a, str>),
    Interp(AVec<'a, LexInterpPart<'a>>),
    /// Captured Rust source from `rust { ... }` or `:rs ...`.
    RustCell(AString<'a>),

    // Multi-char punctuation & operators
    DotDotDot,
    DotDotLt,
    DotDot,
    PlusPlusEq,
    PlusPlus,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    StarStar,
    SlashSlash,
    EqEq,
    BangEq,
    LtEq,
    GtEq,
    EqTilde,
    BangTilde,
    FatArrow,
    QuestionDot,

    // Single-char punctuation & operators
    Pipe,
    Semi,
    Newline,
    Comma,
    Colon,
    Dot,
    Question,
    Eq,
    Plus,
    Minus,
    Star,
    Slash,
    Lt,
    Gt,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Caret,
    Tilde,
    At,
    Bang,
    /// Any other single ASCII/Unicode symbol that can appear inside unquoted command paths/args.
    Other(char),

    Eof,
}

/// A single lexical token with its source byte span.
#[derive(Clone, Debug)]
pub struct Token<'a> {
    pub kind: TokKind<'a>,
    pub span: Span,
}

/// Unit multiplier kind for numeric literals (`10mb`, `5sec`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum UnitKind {
    Size(i64),
    Duration(i64),
}

fn lookup_unit(raw: &str) -> Option<UnitKind> {
    if raw.len() > 4 || !raw.is_ascii() {
        return None;
    }
    let mut lower = [0u8; 4];
    for (i, b) in raw.bytes().enumerate() {
        lower[i] = b.to_ascii_lowercase();
    }
    match &lower[..raw.len()] {
        // Sizes (decimal SI and binary IEC, case-insensitive)
        b"b" => Some(UnitKind::Size(1)),
        b"kb" => Some(UnitKind::Size(1_000)),
        b"mb" => Some(UnitKind::Size(1_000_000)),
        b"gb" => Some(UnitKind::Size(1_000_000_000)),
        b"tb" => Some(UnitKind::Size(1_000_000_000_000)),
        b"pb" => Some(UnitKind::Size(1_000_000_000_000_000)),
        b"kib" => Some(UnitKind::Size(1_024)),
        b"mib" => Some(UnitKind::Size(1_024 * 1_024)),
        b"gib" => Some(UnitKind::Size(1_024 * 1_024 * 1_024)),
        b"tib" => Some(UnitKind::Size(1_024 * 1_024 * 1_024 * 1_024)),
        b"pib" => Some(UnitKind::Size(1_024 * 1_024 * 1_024 * 1_024 * 1_024)),

        // Durations (nanoseconds, Nushell-compatible units)
        b"ns" => Some(UnitKind::Duration(1)),
        b"us" => Some(UnitKind::Duration(1_000)),
        b"ms" => Some(UnitKind::Duration(1_000_000)),
        b"s" | b"sec" | b"secs" => Some(UnitKind::Duration(1_000_000_000)),
        b"m" | b"min" | b"mins" => Some(UnitKind::Duration(60 * 1_000_000_000)),
        b"h" | b"hr" | b"hrs" => Some(UnitKind::Duration(3_600 * 1_000_000_000)),
        b"d" | b"day" | b"days" => Some(UnitKind::Duration(86_400 * 1_000_000_000)),
        b"wk" | b"wks" => Some(UnitKind::Duration(7 * 86_400 * 1_000_000_000)),
        _ => None,
    }
}

#[inline]
fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

#[inline]
fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Tokenize `src` into a vector of tokens terminated by [`TokKind::Eof`].
pub fn tokenize<'a>(src: &str, alloc: Alloc<'a>) -> Result<AVec<'a, Token<'a>>, ShellError<'a>> {
    let mut lex = Lexer::new(src, alloc, 0);
    let mut out = lex.lex_until(false)?;
    let end = Span::from_usize(src.len(), src.len());
    out.push(Token { kind: TokKind::Eof, span: end });
    Ok(out)
}

struct Lexer<'s, 'a> {
    src: &'s str,
    bytes: &'s [u8],
    pos: usize,
    alloc: Alloc<'a>,
    depth: usize,
    /// True when at the start of a statement (start of input, or immediately after `\n`, `;`, `{`).
    at_stmt_start: bool,
}

impl<'s, 'a> Lexer<'s, 'a> {
    fn new(src: &'s str, alloc: Alloc<'a>, depth: usize) -> Self {
        Lexer {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            alloc,
            depth,
            at_stmt_start: true,
        }
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    #[inline]
    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    #[inline]
    fn skip_hws(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn lex_until(&mut self, stop_rparen: bool) -> Result<AVec<'a, Token<'a>>, ShellError<'a>> {
        let mut tokens = AVec::new_in(self.alloc);
        let mut paren_depth: usize = 0;

        while self.pos < self.bytes.len() {
            if tokens.len() >= MAX_TOKENS {
                return Err(ShellError::new(
                    ErrorKind::Budget,
                    Span::from_usize(self.pos, self.pos),
                    "token count limit exceeded",
                    self.alloc,
                ));
            }

            self.skip_hws();
            let Some(b) = self.peek() else {
                break;
            };

            // Line continuation: `\` followed by optional horizontal whitespace and newline.
            if b == b'\\' {
                let mut probe = self.pos + 1;
                while probe < self.bytes.len()
                    && (self.bytes[probe] == b' ' || self.bytes[probe] == b'\t')
                {
                    probe += 1;
                }
                if probe < self.bytes.len() && self.bytes[probe] == b'\r' {
                    probe += 1;
                }
                if probe < self.bytes.len() && self.bytes[probe] == b'\n' {
                    self.pos = probe + 1;
                    continue;
                }
            }

            // Comment `# ...` to end of line (does not consume `\n`).
            if b == b'#' {
                while let Some(c) = self.peek() {
                    if c == b'\n' {
                        break;
                    }
                    self.pos += 1;
                }
                continue;
            }

            // Newline (`\n` or `\r\n`).
            if b == b'\n' || b == b'\r' {
                let start = self.pos;
                if b == b'\r' {
                    self.pos += 1;
                    if self.peek() == Some(b'\n') {
                        self.pos += 1;
                    }
                } else {
                    self.pos += 1;
                }
                tokens.push(Token {
                    kind: TokKind::Newline,
                    span: Span::from_usize(start, self.pos),
                });
                self.at_stmt_start = true;
                continue;
            }

            // Stop at matching `)` when lexing inside a string interpolation `($expr)`.
            if stop_rparen && b == b')' && paren_depth == 0 {
                break;
            }

            // `:rs ` or `:rs\t` REPL shorthand at statement start.
            if self.at_stmt_start && self.matches_rs_shorthand() {
                let start = self.pos;
                self.pos += 3; // consume `:rs`
                self.skip_hws();
                let code_start = self.pos;
                while let Some(c) = self.peek() {
                    if c == b'\n' || c == b'\r' {
                        break;
                    }
                    self.pos += 1;
                }
                let code_slice = self.src[code_start..self.pos].trim();
                let code = AString::from_str_in(code_slice, self.alloc);
                tokens.push(Token {
                    kind: TokKind::RustCell(code),
                    span: Span::from_usize(start, self.pos),
                });
                self.at_stmt_start = false;
                continue;
            }

            let tok = self.lex_one()?;
            match tok.kind {
                TokKind::LParen => {
                    paren_depth = paren_depth.saturating_add(1);
                    self.at_stmt_start = false;
                }
                TokKind::RParen => {
                    paren_depth = paren_depth.saturating_sub(1);
                    self.at_stmt_start = false;
                }
                TokKind::Semi | TokKind::LBrace => {
                    self.at_stmt_start = true;
                }
                _ => {
                    self.at_stmt_start = false;
                }
            }
            tokens.push(tok);
        }

        Ok(tokens)
    }

    fn matches_rs_shorthand(&self) -> bool {
        if !self.src[self.pos..].starts_with(":rs") {
            return false;
        }
        matches!(self.peek_at(3), Some(b' ' | b'\t'))
    }

    fn lex_one(&mut self) -> Result<Token<'a>, ShellError<'a>> {
        let start = self.pos;
        let b = self.bytes[self.pos];

        // Double-quoted string `"..."`
        if b == b'"' {
            return self.lex_double_string(start);
        }

        // Single-quoted raw string `'...'`
        if b == b'\'' {
            return self.lex_single_string(start);
        }

        // `$`: `$"..."` string, `$var`, or bare `$` (for `$.a.b`)
        if b == b'$' {
            if self.peek_at(1) == Some(b'"') {
                self.pos += 1; // skip `$`, lex_double_string expects `"` at self.pos
                return self.lex_double_string(start);
            }
            self.pos += 1;
            if let Some(next) = self.peek()
                && is_ident_start(next)
            {
                let name_start = self.pos;
                while let Some(c) = self.peek() {
                    if is_ident_continue(c) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let name = Rc::from_str_in(&self.src[name_start..self.pos], self.alloc);
                return Ok(Token {
                    kind: TokKind::Var(name),
                    span: Span::from_usize(start, self.pos),
                });
            }
            return Ok(Token {
                kind: TokKind::Dollar,
                span: Span::from_usize(start, self.pos),
            });
        }

        // Raw hash string `r#'...'#`
        if b == b'r'
            && self.peek_at(1) == Some(b'#')
            && let Some(tok) = self.try_lex_raw_hash_string(start)?
        {
            return Ok(tok);
        }

        // Numbers (int, hex, bin, oct, float, size, duration)
        if b.is_ascii_digit() {
            return self.lex_number(start);
        }

        // Identifiers, keywords, and `rust { ... }`
        if is_ident_start(b) {
            return self.lex_ident_or_kw(start);
        }

        // Multi-character operators and punctuation
        let rem = &self.bytes[self.pos..];
        if rem.starts_with(b"...") {
            self.pos += 3;
            return Ok(Token { kind: TokKind::DotDotDot, span: Span::from_usize(start, self.pos) });
        }
        if rem.starts_with(b"..<") {
            self.pos += 3;
            return Ok(Token { kind: TokKind::DotDotLt, span: Span::from_usize(start, self.pos) });
        }
        if rem.starts_with(b"..=") {
            self.pos += 3;
            return Ok(Token { kind: TokKind::DotDot, span: Span::from_usize(start, self.pos) });
        }
        if rem.starts_with(b"++=") {
            self.pos += 3;
            return Ok(Token { kind: TokKind::PlusPlusEq, span: Span::from_usize(start, self.pos) });
        }
        if rem.len() >= 2 {
            let two = match (rem[0], rem[1]) {
                (b'.', b'.') => Some(TokKind::DotDot),
                (b'+', b'+') => Some(TokKind::PlusPlus),
                (b'+', b'=') => Some(TokKind::PlusEq),
                (b'-', b'=') => Some(TokKind::MinusEq),
                (b'*', b'=') => Some(TokKind::StarEq),
                (b'/', b'=') => Some(TokKind::SlashEq),
                (b'*', b'*') => Some(TokKind::StarStar),
                (b'/', b'/') => Some(TokKind::SlashSlash),
                (b'=', b'=') => Some(TokKind::EqEq),
                (b'!', b'=') => Some(TokKind::BangEq),
                (b'<', b'=') => Some(TokKind::LtEq),
                (b'>', b'=') => Some(TokKind::GtEq),
                (b'=', b'~') => Some(TokKind::EqTilde),
                (b'!', b'~') => Some(TokKind::BangTilde),
                (b'=', b'>') => Some(TokKind::FatArrow),
                (b'?', b'.') => Some(TokKind::QuestionDot),
                _ => None,
            };
            if let Some(kind) = two {
                self.pos += 2;
                return Ok(Token { kind, span: Span::from_usize(start, self.pos) });
            }
        }

        // Single-character punctuation & operators
        let single = match b {
            b'|' => Some(TokKind::Pipe),
            b';' => Some(TokKind::Semi),
            b',' => Some(TokKind::Comma),
            b':' => Some(TokKind::Colon),
            b'.' => Some(TokKind::Dot),
            b'?' => Some(TokKind::Question),
            b'=' => Some(TokKind::Eq),
            b'+' => Some(TokKind::Plus),
            b'-' => Some(TokKind::Minus),
            b'*' => Some(TokKind::Star),
            b'/' => Some(TokKind::Slash),
            b'<' => Some(TokKind::Lt),
            b'>' => Some(TokKind::Gt),
            b'(' => Some(TokKind::LParen),
            b')' => Some(TokKind::RParen),
            b'[' => Some(TokKind::LBracket),
            b']' => Some(TokKind::RBracket),
            b'{' => Some(TokKind::LBrace),
            b'}' => Some(TokKind::RBrace),
            b'^' => Some(TokKind::Caret),
            b'~' => Some(TokKind::Tilde),
            b'@' => Some(TokKind::At),
            b'!' => Some(TokKind::Bang),
            _ => None,
        };
        if let Some(kind) = single {
            self.pos += 1;
            return Ok(Token { kind, span: Span::from_usize(start, self.pos) });
        }

        // Any other UTF-8 char (can appear in unquoted command arguments or paths)
        let ch = self.src[self.pos..].chars().next().unwrap_or('\0');
        self.pos += ch.len_utf8();
        Ok(Token { kind: TokKind::Other(ch), span: Span::from_usize(start, self.pos) })
    }

    fn lex_single_string(&mut self, start: usize) -> Result<Token<'a>, ShellError<'a>> {
        self.pos += 1; // opening `'`
        let content_start = self.pos;
        while self.pos < self.bytes.len() {
            if self.bytes[self.pos] == b'\'' {
                let content = Rc::from_str_in(&self.src[content_start..self.pos], self.alloc);
                self.pos += 1; // closing `'`
                return Ok(Token {
                    kind: TokKind::Str(content),
                    span: Span::from_usize(start, self.pos),
                });
            }
            self.pos += 1;
        }
        Err(ShellError::new(
            ErrorKind::Lex,
            Span::from_usize(start, self.pos),
            "unterminated single-quoted string",
            self.alloc,
        ))
    }

    fn try_lex_raw_hash_string(&mut self, start: usize) -> Result<Option<Token<'a>>, ShellError<'a>> {
        let mut hashes = 0usize;
        let mut p = self.pos + 1;
        while p < self.bytes.len() && self.bytes[p] == b'#' {
            hashes += 1;
            p += 1;
            if hashes > MAX_RAW_HASHES {
                return Err(ShellError::new(
                    ErrorKind::Budget,
                    Span::from_usize(start, p),
                    "raw string delimiter exceeds maximum `#` count",
                    self.alloc,
                ));
            }
        }
        if hashes == 0 || p >= self.bytes.len() || self.bytes[p] != b'\'' {
            return Ok(None);
        }
        p += 1; // opening `'`
        let content_start = p;
        while p < self.bytes.len() {
            if self.bytes[p] == b'\'' {
                let mut matched = 0usize;
                while matched < hashes
                    && p + 1 + matched < self.bytes.len()
                    && self.bytes[p + 1 + matched] == b'#'
                {
                    matched += 1;
                }
                if matched == hashes {
                    let content = Rc::from_str_in(&self.src[content_start..p], self.alloc);
                    self.pos = p + 1 + hashes;
                    return Ok(Some(Token {
                        kind: TokKind::Str(content),
                        span: Span::from_usize(start, self.pos),
                    }));
                }
            }
            p += 1;
        }
        Err(ShellError::new(
            ErrorKind::Lex,
            Span::from_usize(start, self.bytes.len()),
            "unterminated raw string",
            self.alloc,
        ))
    }

    fn lex_double_string(&mut self, start: usize) -> Result<Token<'a>, ShellError<'a>> {
        if self.depth >= MAX_LEX_DEPTH {
            return Err(ShellError::new(
                ErrorKind::Budget,
                Span::from_usize(start, self.pos),
                "string interpolation nesting depth exceeded",
                self.alloc,
            ));
        }
        self.pos += 1; // opening `"`
        let mut parts = AVec::new_in(self.alloc);
        let mut buf = AString::new_in(self.alloc);
        let mut lit_start = self.pos;
        let mut has_interp = false;

        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'"' {
                let lit_end = self.pos;
                self.pos += 1; // closing `"`
                if !has_interp {
                    let text = Rc::from_str_in(buf.as_str(), self.alloc);
                    return Ok(Token {
                        kind: TokKind::Str(text),
                        span: Span::from_usize(start, self.pos),
                    });
                }
                if !buf.is_empty() {
                    parts.push(LexInterpPart::Literal(
                        Rc::from_str_in(buf.as_str(), self.alloc),
                        Span::from_usize(lit_start, lit_end),
                    ));
                }
                return Ok(Token {
                    kind: TokKind::Interp(parts),
                    span: Span::from_usize(start, self.pos),
                });
            }

            if b == b'\\' {
                let esc_start = self.pos;
                self.pos += 1;
                let Some(next) = self.peek() else {
                    return Err(ShellError::new(
                        ErrorKind::Lex,
                        Span::from_usize(esc_start, self.pos),
                        "unterminated escape sequence in string",
                        self.alloc,
                    ));
                };
                self.pos += 1;
                match next {
                    b'"' => buf.push('"'),
                    b'\\' => buf.push('\\'),
                    b'n' => buf.push('\n'),
                    b't' => buf.push('\t'),
                    b'r' => buf.push('\r'),
                    b'(' => buf.push('('),
                    b'\'' => buf.push('\''),
                    b'0' => buf.push('\0'),
                    b'/' => buf.push('/'),
                    _ => {
                        return Err(ShellError::new(
                            ErrorKind::Lex,
                            Span::from_usize(esc_start, self.pos),
                            "unknown escape sequence in double-quoted string",
                            self.alloc,
                        )
                        .with_help("use single quotes `'...'` for raw strings without escapes", self.alloc));
                    }
                }
                continue;
            }

            if b == b'(' {
                has_interp = true;
                if !buf.is_empty() {
                    parts.push(LexInterpPart::Literal(
                        Rc::from_str_in(buf.as_str(), self.alloc),
                        Span::from_usize(lit_start, self.pos),
                    ));
                    buf.clear();
                }
                let interp_start = self.pos;
                self.pos += 1; // consume `(`
                let mut sub = Lexer {
                    src: self.src,
                    bytes: self.bytes,
                    pos: self.pos,
                    alloc: self.alloc,
                    depth: self.depth + 1,
                    at_stmt_start: false,
                };
                let inner_tokens = sub.lex_until(true)?;
                self.pos = sub.pos;
                if self.peek() != Some(b')') {
                    return Err(ShellError::new(
                        ErrorKind::Lex,
                        Span::from_usize(interp_start, self.pos),
                        "unterminated `(...)` interpolation inside double-quoted string",
                        self.alloc,
                    )
                    .with_help("escape a literal parenthesis as `\\(`", self.alloc));
                }
                self.pos += 1; // consume `)`
                parts.push(LexInterpPart::Tokens(
                    inner_tokens,
                    Span::from_usize(interp_start, self.pos),
                ));
                lit_start = self.pos;
                continue;
            }

            // Copy one full UTF-8 character.
            let ch = self.src[self.pos..].chars().next().unwrap_or('\0');
            buf.push(ch);
            self.pos += ch.len_utf8();
        }

        Err(ShellError::new(
            ErrorKind::Lex,
            Span::from_usize(start, self.pos),
            "unterminated double-quoted string",
            self.alloc,
        ))
    }

    fn lex_number(&mut self, start: usize) -> Result<Token<'a>, ShellError<'a>> {
        // Radix prefixes: 0x, 0b (only if followed by binary digits so `0b` = 0 bytes), 0o
        if self.bytes[self.pos] == b'0'
            && let Some(prefix) = self.peek_at(1)
        {
            let radix = match prefix {
                b'x' | b'X' => Some(16u32),
                b'b' | b'B'
                    if self.peek_at(2).is_some_and(|c| c == b'0' || c == b'1' || c == b'_') =>
                {
                    Some(2u32)
                }
                b'o' | b'O' => Some(8u32),
                _ => None,
            };
            if let Some(radix) = radix {
                self.pos += 2;
                let digits_start = self.pos;
                while let Some(c) = self.peek() {
                    let ok = match radix {
                        16 => c.is_ascii_hexdigit() || c == b'_',
                        2 => c == b'0' || c == b'1' || c == b'_',
                        8 => (b'0'..=b'7').contains(&c) || c == b'_',
                        _ => false,
                    };
                    if ok {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let val = parse_i64_radix(&self.src[digits_start..self.pos], radix).ok_or_else(
                    || {
                        ShellError::new(
                            ErrorKind::Lex,
                            Span::from_usize(start, self.pos),
                            "invalid integer literal",
                            self.alloc,
                        )
                    },
                )?;
                return Ok(Token {
                    kind: TokKind::Int(val),
                    span: Span::from_usize(start, self.pos),
                });
            }
        }

        // Decimal integer part
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }

        let mut is_float = false;

        // Fractional part: `.` followed immediately by an ASCII digit (so `1..10` stays Int + DotDot).
        if self.peek() == Some(b'.')
            && self.peek_at(1).is_some_and(|c| c.is_ascii_digit())
        {
            is_float = true;
            self.pos += 1; // consume `.`
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() || c == b'_' {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }

        // Exponent part: `e`/`E` (+ optional `+`/`-`) followed by at least one ASCII digit.
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let mut probe = self.pos + 1;
            if matches!(self.bytes.get(probe), Some(b'+' | b'-')) {
                probe += 1;
            }
            if self.bytes.get(probe).is_some_and(|c| c.is_ascii_digit()) {
                is_float = true;
                self.pos = probe;
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() || c == b'_' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
            }
        }

        let num_end = self.pos;

        // Optional unit suffix (`kb`, `MiB`, `ms`, `sec`, etc.).
        if self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            let unit_start = self.pos;
            let mut probe = self.pos;
            while probe < self.bytes.len() && self.bytes[probe].is_ascii_alphabetic() {
                probe += 1;
            }
            let unit_str = &self.src[unit_start..probe];
            if let Some(unit) = lookup_unit(unit_str) {
                self.pos = probe;
                let span = Span::from_usize(start, self.pos);
                let scaled = if is_float {
                    let f = parse_f64_clean(&self.src[start..num_end]).ok_or_else(|| {
                        ShellError::new(ErrorKind::Lex, span, "invalid float literal", self.alloc)
                    })?;
                    let mult = match unit {
                        UnitKind::Size(m) | UnitKind::Duration(m) => m as f64,
                    };
                    let prod = f * mult;
                    if !prod.is_finite() || prod < i64::MIN as f64 || prod > i64::MAX as f64 {
                        return Err(ShellError::new(
                            ErrorKind::Overflow,
                            span,
                            "unit literal overflows 64-bit signed integer",
                            self.alloc,
                        ));
                    }
                    prod as i64
                } else {
                    let n = parse_i64_radix(&self.src[start..num_end], 10).ok_or_else(|| {
                        ShellError::new(ErrorKind::Lex, span, "invalid integer literal", self.alloc)
                    })?;
                    let mult = match unit {
                        UnitKind::Size(m) | UnitKind::Duration(m) => m,
                    };
                    n.checked_mul(mult).ok_or_else(|| {
                        ShellError::new(
                            ErrorKind::Overflow,
                            span,
                            "unit literal overflows 64-bit signed integer",
                            self.alloc,
                        )
                    })?
                };
                let kind = match unit {
                    UnitKind::Size(_) => TokKind::Size(scaled),
                    UnitKind::Duration(_) => TokKind::Duration(scaled),
                };
                return Ok(Token { kind, span });
            }
        }

        let span = Span::from_usize(start, num_end);
        if is_float {
            let f = parse_f64_clean(&self.src[start..num_end]).ok_or_else(|| {
                ShellError::new(ErrorKind::Lex, span, "invalid float literal", self.alloc)
            })?;
            Ok(Token { kind: TokKind::Float(f), span })
        } else {
            let n = parse_i64_radix(&self.src[start..num_end], 10).ok_or_else(|| {
                ShellError::new(ErrorKind::Lex, span, "invalid integer literal", self.alloc)
            })?;
            Ok(Token { kind: TokKind::Int(n), span })
        }
    }

    fn lex_ident_or_kw(&mut self, start: usize) -> Result<Token<'a>, ShellError<'a>> {
        self.pos += 1;
        while let Some(c) = self.peek() {
            if is_ident_continue(c) {
                self.pos += 1;
            } else if c == b'-' && self.peek_at(1).is_some_and(is_ident_continue) {
                // Allow hyphenated identifiers like `sort-by`, `not-in`, `starts-with`.
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = &self.src[start..self.pos];
        if let Some(kw) = Keyword::from_name(text) {
            if kw == Keyword::Rust {
                // Check if `rust` is followed by optional horizontal whitespace and `{`.
                let mut probe = self.pos;
                while probe < self.bytes.len()
                    && (self.bytes[probe] == b' ' || self.bytes[probe] == b'\t')
                {
                    probe += 1;
                }
                if self.bytes.get(probe) == Some(&b'{') {
                    let brace_open = probe;
                    match scan_rust_braced_block(self.src, brace_open) {
                        RustBlockScan::Closed { body_start, body_end, close_end } => {
                            self.pos = close_end;
                            let code = AString::from_str_in(
                                self.src[body_start..body_end].trim(),
                                self.alloc,
                            );
                            return Ok(Token {
                                kind: TokKind::RustCell(code),
                                span: Span::from_usize(start, close_end),
                            });
                        }
                        RustBlockScan::Unclosed => {
                            return Err(ShellError::new(
                                ErrorKind::Lex,
                                Span::from_usize(start, self.bytes.len()),
                                "unterminated `rust { ... }` block",
                                self.alloc,
                            ));
                        }
                        RustBlockScan::DepthExceeded(at) => {
                            return Err(ShellError::new(
                                ErrorKind::Budget,
                                Span::from_usize(start, at),
                                "maximum brace nesting depth exceeded inside `rust { ... }` block",
                                self.alloc,
                            ));
                        }
                    }
                }
            }
            return Ok(Token {
                kind: TokKind::Kw(kw),
                span: Span::from_usize(start, self.pos),
            });
        }
        Ok(Token {
            kind: TokKind::Ident(Rc::from_str_in(text, self.alloc)),
            span: Span::from_usize(start, self.pos),
        })
    }
}

fn parse_i64_radix(s: &str, radix: u32) -> Option<i64> {
    let mut buf = [0u8; 128];
    let mut len = 0usize;
    for b in s.bytes() {
        if b == b'_' {
            continue;
        }
        if len >= buf.len() {
            return None;
        }
        buf[len] = b;
        len += 1;
    }
    if len == 0 {
        return None;
    }
    let clean = core::str::from_utf8(&buf[..len]).ok()?;
    i64::from_str_radix(clean, radix).ok()
}

fn parse_f64_clean(s: &str) -> Option<f64> {
    let mut buf = [0u8; 128];
    let mut len = 0usize;
    for b in s.bytes() {
        if b == b'_' {
            continue;
        }
        if len >= buf.len() {
            return None;
        }
        buf[len] = b;
        len += 1;
    }
    if len == 0 {
        return None;
    }
    let clean = core::str::from_utf8(&buf[..len]).ok()?;
    let val = clean.parse::<f64>().ok()?;
    if val.is_finite() { Some(val) } else { None }
}

/// Result of scanning a balanced `{ ... }` block of raw Rust source.
enum RustBlockScan {
    Closed {
        body_start: usize,
        body_end: usize,
        close_end: usize,
    },
    Unclosed,
    DepthExceeded(usize),
}

const MAX_RUST_BRACE_DEPTH: usize = 256;

/// Scan balanced `{ ... }` raw Rust source starting at the opening `{` (`open_pos`),
/// respecting Rust comments (`//`, nested `/* ... */`), normal/byte/raw strings,
/// char literals (`'{'`, `'\''`, `'\u{7b}'`), and lifetimes (`'a`, `'static`).
fn scan_rust_braced_block(src: &str, open_pos: usize) -> RustBlockScan {
    let bytes = src.as_bytes();
    if bytes.get(open_pos) != Some(&b'{') {
        return RustBlockScan::Unclosed;
    }
    let body_start = open_pos + 1;
    let mut i = body_start;
    let mut depth: usize = 1;

    while i < bytes.len() {
        let b = bytes[i];

        // Line comment `// ...`
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Nested block comment `/* ... */`
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            let mut cdepth: usize = 1;
            while i < bytes.len() && cdepth > 0 {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    if cdepth >= MAX_RUST_BRACE_DEPTH {
                        return RustBlockScan::DepthExceeded(i);
                    }
                    cdepth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    cdepth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if cdepth > 0 {
                return RustBlockScan::Unclosed;
            }
            continue;
        }

        // Raw Rust string: `r"..."`, `r#"..."#`, `br#"..."#`, `cr#"..."#`
        if (b == b'r' || ((b == b'b' || b == b'c') && bytes.get(i + 1) == Some(&b'r')))
            && (i == 0 || !is_ident_continue(bytes[i - 1]))
        {
            let r_pos = if b == b'r' { i } else { i + 1 };
            let mut p = r_pos + 1;
            let mut hashes = 0usize;
            while p < bytes.len() && bytes[p] == b'#' && hashes <= MAX_RAW_HASHES {
                hashes += 1;
                p += 1;
            }
            if bytes.get(p) == Some(&b'"') {
                p += 1;
                let mut closed = false;
                while p < bytes.len() {
                    if bytes[p] == b'"' {
                        let mut m = 0usize;
                        while m < hashes && bytes.get(p + 1 + m) == Some(&b'#') {
                            m += 1;
                        }
                        if m == hashes {
                            i = p + 1 + hashes;
                            closed = true;
                            break;
                        }
                    }
                    p += 1;
                }
                if !closed {
                    return RustBlockScan::Unclosed;
                }
                continue;
            }
        }

        // Normal or byte/c string literal `"..."`
        if b == b'"' {
            i += 1;
            let mut closed = false;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == b'"' {
                    i += 1;
                    closed = true;
                    break;
                } else {
                    i += 1;
                }
            }
            if !closed {
                return RustBlockScan::Unclosed;
            }
            continue;
        }

        // Char literal vs lifetime: `'x'`, `'\''`, `'\u{7b}'` vs `'a`, `'static`
        if b == b'\'' {
            if bytes.get(i + 1) == Some(&b'\\') {
                // Escaped char literal: scan until closing `'` (bounded to 16 bytes for `\u{10FFFF}`).
                let mut p = i + 2;
                let limit = (i + 20).min(bytes.len());
                let mut found = false;
                while p < limit {
                    if bytes[p] == b'\\' {
                        p += 2;
                    } else if bytes[p] == b'\'' {
                        i = p + 1;
                        found = true;
                        break;
                    } else if bytes[p] == b'\n' {
                        break;
                    } else {
                        p += 1;
                    }
                }
                if found {
                    continue;
                }
            } else if i + 1 < bytes.len()
                && bytes[i + 1] != b'\''
                && bytes[i + 1] != b'\n'
                && let Some(ch) = src[i + 1..].chars().next()
                && bytes.get(i + 1 + ch.len_utf8()) == Some(&b'\'')
            {
                i = i + 1 + ch.len_utf8() + 1;
                continue;
            }
            // Otherwise it is a lifetime (`'a`, `'static`, `'_`); just advance past `'`.
            i += 1;
            continue;
        }

        if b == b'{' {
            if depth >= MAX_RUST_BRACE_DEPTH {
                return RustBlockScan::DepthExceeded(i);
            }
            depth += 1;
            i += 1;
            continue;
        }

        if b == b'}' {
            depth -= 1;
            if depth == 0 {
                return RustBlockScan::Closed {
                    body_start,
                    body_end: i,
                    close_end: i + 1,
                };
            }
            i += 1;
            continue;
        }

        i += 1;
    }

    RustBlockScan::Unclosed
}

/// Returns `true` if `src` is syntactically incomplete and should continue onto a new line
/// in the interactive shell editor:
/// - unclosed `{`, `[`, or `(`,
/// - unclosed `"..."`, `'...'`, `r#'...'#`, or `rust { ... }`,
/// - or ends with a trailing `|` or `\` (ignoring trailing spaces/tabs/comments).
pub fn is_incomplete(src: &str) -> bool {
    is_incomplete_inner(src, 0, src.len(), 0).is_incomplete
}

struct IncompleteScan {
    is_incomplete: bool,
    end_pos: usize,
}

fn is_incomplete_inner(src: &str, start: usize, limit: usize, depth: usize) -> IncompleteScan {
    if depth >= MAX_LEX_DEPTH {
        return IncompleteScan { is_incomplete: false, end_pos: limit };
    }
    let bytes = src.as_bytes();
    let mut i = start;
    let mut brace_depth: usize = 0;
    let mut bracket_depth: usize = 0;
    let mut paren_depth: usize = 0;
    let mut at_stmt_start = depth == 0;
    let mut trailing_pipe = false;
    let mut trailing_backslash = false;

    while i < limit {
        let b = bytes[i];

        // Horizontal whitespace does not clear trailing pipe/backslash state.
        if b == b' ' || b == b'\t' {
            i += 1;
            continue;
        }

        // Comment `# ...` to end of line: does not clear trailing `|` before comment.
        if b == b'#' {
            while i < limit && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Newline clears trailing backslash (it continued the line!) and resets statement start.
        if b == b'\n' || b == b'\r' {
            trailing_backslash = false;
            at_stmt_start = true;
            i += 1;
            continue;
        }

        // Inside nested string interpolation, `)` at depth 0 closes the interpolation.
        if depth > 0 && b == b')' && paren_depth == 0 && brace_depth == 0 && bracket_depth == 0 {
            return IncompleteScan { is_incomplete: false, end_pos: i };
        }

        // Trailing backslash check (line continuation if followed by newline, incomplete if at EOF).
        if b == b'\\' {
            let mut probe = i + 1;
            while probe < limit && (bytes[probe] == b' ' || bytes[probe] == b'\t') {
                probe += 1;
            }
            if probe >= limit {
                trailing_backslash = true;
                i = probe;
                break;
            }
            if bytes[probe] == b'\r' {
                probe += 1;
            }
            if probe < limit && bytes[probe] == b'\n' {
                i = probe + 1;
                trailing_backslash = false;
                continue;
            }
            trailing_pipe = false;
            trailing_backslash = false;
            at_stmt_start = false;
            i += 1;
            continue;
        }

        // `:rs ` / `:rs\t` at statement start: consumes to end of line.
        if at_stmt_start
            && src[i..limit].starts_with(":rs")
            && matches!(bytes.get(i + 3), Some(b' ' | b'\t'))
        {
            while i < limit && bytes[i] != b'\n' {
                i += 1;
            }
            trailing_pipe = false;
            trailing_backslash = false;
            at_stmt_start = false;
            continue;
        }

        // Single-quoted string
        if b == b'\'' {
            trailing_pipe = false;
            trailing_backslash = false;
            at_stmt_start = false;
            i += 1;
            let mut closed = false;
            while i < limit {
                if bytes[i] == b'\'' {
                    i += 1;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                return IncompleteScan { is_incomplete: true, end_pos: i };
            }
            continue;
        }

        // Double-quoted string (`"..."` or `$"..."`)
        if b == b'"' || (b == b'$' && bytes.get(i + 1) == Some(&b'"')) {
            trailing_pipe = false;
            trailing_backslash = false;
            at_stmt_start = false;
            if b == b'$' {
                i += 1;
            }
            i += 1; // opening `"`
            let mut closed = false;
            while i < limit {
                if bytes[i] == b'\\' {
                    if i + 1 >= limit {
                        return IncompleteScan { is_incomplete: true, end_pos: limit };
                    }
                    i += 2;
                } else if bytes[i] == b'(' {
                    let sub = is_incomplete_inner(src, i + 1, limit, depth + 1);
                    if sub.is_incomplete || sub.end_pos >= limit || bytes[sub.end_pos] != b')' {
                        return IncompleteScan { is_incomplete: true, end_pos: sub.end_pos };
                    }
                    i = sub.end_pos + 1;
                } else if bytes[i] == b'"' {
                    i += 1;
                    closed = true;
                    break;
                } else {
                    i += 1;
                }
            }
            if !closed {
                return IncompleteScan { is_incomplete: true, end_pos: i };
            }
            continue;
        }

        // Raw hash string `r#'...'#`
        if b == b'r' && bytes.get(i + 1) == Some(&b'#') {
            let mut p = i + 1;
            let mut hashes = 0usize;
            while p < limit && bytes[p] == b'#' && hashes <= MAX_RAW_HASHES {
                hashes += 1;
                p += 1;
            }
            if hashes > 0 && p < limit && bytes[p] == b'\'' {
                trailing_pipe = false;
                trailing_backslash = false;
                at_stmt_start = false;
                p += 1;
                let mut closed = false;
                while p < limit {
                    if bytes[p] == b'\'' {
                        let mut m = 0usize;
                        while m < hashes && p + 1 + m < limit && bytes[p + 1 + m] == b'#' {
                            m += 1;
                        }
                        if m == hashes {
                            i = p + 1 + hashes;
                            closed = true;
                            break;
                        }
                    }
                    p += 1;
                }
                if !closed {
                    return IncompleteScan { is_incomplete: true, end_pos: limit };
                }
                continue;
            }
        }

        // Identifier / keyword (specifically `rust { ... }`)
        if is_ident_start(b) {
            let id_start = i;
            i += 1;
            while i < limit {
                if is_ident_continue(bytes[i]) {
                    i += 1;
                } else if bytes[i] == b'-' && bytes.get(i + 1).is_some_and(|&c| is_ident_continue(c))
                {
                    i += 1;
                } else {
                    break;
                }
            }
            trailing_pipe = false;
            trailing_backslash = false;
            at_stmt_start = false;
            if &src[id_start..i] == "rust" {
                let mut probe = i;
                while probe < limit && (bytes[probe] == b' ' || bytes[probe] == b'\t') {
                    probe += 1;
                }
                if probe < limit && bytes[probe] == b'{' {
                    match scan_rust_braced_block(&src[..limit], probe) {
                        RustBlockScan::Closed { close_end, .. } => {
                            i = close_end;
                        }
                        RustBlockScan::Unclosed => {
                            return IncompleteScan { is_incomplete: true, end_pos: limit };
                        }
                        RustBlockScan::DepthExceeded(at) => {
                            return IncompleteScan { is_incomplete: false, end_pos: at };
                        }
                    }
                }
            }
            continue;
        }

        match b {
            b'{' => {
                brace_depth = brace_depth.saturating_add(1);
                trailing_pipe = false;
                trailing_backslash = false;
                at_stmt_start = true;
            }
            b'}' => {
                brace_depth = brace_depth.saturating_sub(1);
                trailing_pipe = false;
                trailing_backslash = false;
                at_stmt_start = false;
            }
            b'[' => {
                bracket_depth = bracket_depth.saturating_add(1);
                trailing_pipe = false;
                trailing_backslash = false;
                at_stmt_start = false;
            }
            b']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                trailing_pipe = false;
                trailing_backslash = false;
                at_stmt_start = false;
            }
            b'(' => {
                paren_depth = paren_depth.saturating_add(1);
                trailing_pipe = false;
                trailing_backslash = false;
                at_stmt_start = false;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                trailing_pipe = false;
                trailing_backslash = false;
                at_stmt_start = false;
            }
            b'|' => {
                trailing_pipe = true;
                trailing_backslash = false;
                at_stmt_start = false;
            }
            b';' => {
                trailing_pipe = false;
                trailing_backslash = false;
                at_stmt_start = true;
            }
            _ => {
                trailing_pipe = false;
                trailing_backslash = false;
                at_stmt_start = false;
            }
        }
        i += 1;
    }

    let incomplete = brace_depth > 0
        || bracket_depth > 0
        || paren_depth > 0
        || trailing_pipe
        || trailing_backslash;
    IncompleteScan { is_incomplete: incomplete, end_pos: i }
}

/// Infallible live syntax highlighter for the GUI shell editor.
///
/// Never fails even on partial or malformed input; malformed runs emit [`HighlightKind::Error`].
pub fn highlight<'a>(src: &str, alloc: Alloc<'a>) -> AVec<'a, HighlightSpan> {
    let mut out = AVec::new_in(alloc);
    highlight_range(src, 0, src.len(), 0, &mut out);
    out
}

fn highlight_range<'a>(
    src: &str,
    start: usize,
    limit: usize,
    depth: usize,
    out: &mut AVec<'a, HighlightSpan>,
) -> usize {
    if depth >= MAX_LEX_DEPTH {
        if start < limit {
            out.push(HighlightSpan {
                kind: HighlightKind::Error,
                span: Span::from_usize(start, limit),
            });
        }
        return limit;
    }

    let bytes = src.as_bytes();
    let mut i = start;
    let mut expect_cmd = true;
    let mut at_stmt_start = depth == 0;
    let mut in_cmd_args = false;
    let mut paren_depth: usize = 0;

    while i < limit && out.len() < MAX_TOKENS {
        let b = bytes[i];

        // Horizontal whitespace
        if b == b' ' || b == b'\t' {
            i += 1;
            continue;
        }

        // Newline
        if b == b'\n' || b == b'\r' {
            i += 1;
            expect_cmd = true;
            at_stmt_start = true;
            in_cmd_args = false;
            continue;
        }

        // Inside string interpolation, `)` at paren_depth 0 ends the interpolation.
        if depth > 0 && b == b')' && paren_depth == 0 {
            return i;
        }

        // Comment `# ...`
        if b == b'#' {
            let cstart = i;
            while i < limit && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(HighlightSpan {
                kind: HighlightKind::Comment,
                span: Span::from_usize(cstart, i),
            });
            continue;
        }

        // `:rs ` / `:rs\t` at statement start
        if at_stmt_start
            && src[i..limit].starts_with(":rs")
            && matches!(bytes.get(i + 3), Some(b' ' | b'\t'))
        {
            out.push(HighlightSpan {
                kind: HighlightKind::Keyword,
                span: Span::from_usize(i, i + 3),
            });
            i += 3;
            while i < limit && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            let code_start = i;
            while i < limit && bytes[i] != b'\n' && bytes[i] != b'\r' {
                i += 1;
            }
            if code_start < i {
                out.push(HighlightSpan {
                    kind: HighlightKind::RustCode,
                    span: Span::from_usize(code_start, i),
                });
            }
            expect_cmd = false;
            at_stmt_start = false;
            in_cmd_args = false;
            continue;
        }

        // Forced external `^cmd`
        if b == b'^' && expect_cmd {
            let cmd_start = i;
            i += 1;
            while i < limit {
                let c = bytes[i];
                if is_ident_continue(c) || c == b'-' || c == b'/' || c == b'.' || c == b'~' {
                    i += 1;
                } else {
                    break;
                }
            }
            out.push(HighlightSpan {
                kind: HighlightKind::Command,
                span: Span::from_usize(cmd_start, i),
            });
            expect_cmd = false;
            at_stmt_start = false;
            in_cmd_args = true;
            continue;
        }

        // Single-quoted raw string
        if b == b'\'' {
            let sstart = i;
            i += 1;
            let mut closed = false;
            while i < limit {
                if bytes[i] == b'\'' {
                    i += 1;
                    closed = true;
                    break;
                }
                i += 1;
            }
            out.push(HighlightSpan {
                kind: if closed { HighlightKind::String } else { HighlightKind::Error },
                span: Span::from_usize(sstart, i),
            });
            expect_cmd = false;
            at_stmt_start = false;
            continue;
        }

        // Double-quoted string (`"..."` or `$"..."`) with interpolation highlighting
        if b == b'"' || (b == b'$' && bytes.get(i + 1) == Some(&b'"')) {
            i = highlight_double_string(src, i, limit, depth, out);
            expect_cmd = false;
            at_stmt_start = false;
            continue;
        }

        // Variables `$name` or bare `$`
        if b == b'$' {
            let vstart = i;
            i += 1;
            while i < limit && is_ident_continue(bytes[i]) {
                i += 1;
            }
            out.push(HighlightSpan {
                kind: HighlightKind::Var,
                span: Span::from_usize(vstart, i),
            });
            expect_cmd = false;
            at_stmt_start = false;
            continue;
        }

        // Raw hash string `r#'...'#`
        if b == b'r' && bytes.get(i + 1) == Some(&b'#') {
            let sstart = i;
            let mut p = i + 1;
            let mut hashes = 0usize;
            while p < limit && bytes[p] == b'#' && hashes <= MAX_RAW_HASHES {
                hashes += 1;
                p += 1;
            }
            if hashes > 0 && p < limit && bytes[p] == b'\'' {
                p += 1;
                let mut closed = false;
                while p < limit {
                    if bytes[p] == b'\'' {
                        let mut m = 0usize;
                        while m < hashes && p + 1 + m < limit && bytes[p + 1 + m] == b'#' {
                            m += 1;
                        }
                        if m == hashes {
                            p = p + 1 + hashes;
                            closed = true;
                            break;
                        }
                    }
                    p += 1;
                }
                out.push(HighlightSpan {
                    kind: if closed { HighlightKind::String } else { HighlightKind::Error },
                    span: Span::from_usize(sstart, p),
                });
                i = p;
                expect_cmd = false;
                at_stmt_start = false;
                continue;
            }
        }

        // Numbers (+ optional unit suffix highlighted as `HighlightKind::Unit`)
        if b.is_ascii_digit() {
            let nstart = i;
            let is_radix = b == b'0'
                && match bytes.get(i + 1) {
                    Some(b'x' | b'X' | b'o' | b'O') => true,
                    Some(b'b' | b'B') => {
                        bytes.get(i + 2).is_some_and(|&c| c == b'0' || c == b'1' || c == b'_')
                    }
                    _ => false,
                };
            if is_radix {
                let prefix = bytes[i + 1];
                i += 2;
                let dstart = i;
                while i < limit {
                    let c = bytes[i];
                    let ok = match prefix {
                        b'x' | b'X' => c.is_ascii_hexdigit() || c == b'_',
                        b'b' | b'B' => c == b'0' || c == b'1' || c == b'_',
                        _ => (b'0'..=b'7').contains(&c) || c == b'_',
                    };
                    if ok {
                        i += 1;
                    } else {
                        break;
                    }
                }
                let kind = if i == dstart { HighlightKind::Error } else { HighlightKind::Number };
                out.push(HighlightSpan { kind, span: Span::from_usize(nstart, i) });
            } else {
                while i < limit && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
                    i += 1;
                }
                if i < limit
                    && bytes[i] == b'.'
                    && bytes.get(i + 1).is_some_and(|c| c.is_ascii_digit())
                {
                    i += 1;
                    while i < limit && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
                        i += 1;
                    }
                }
                if i < limit && matches!(bytes[i], b'e' | b'E') {
                    let mut p = i + 1;
                    if matches!(bytes.get(p), Some(b'+' | b'-')) {
                        p += 1;
                    }
                    if bytes.get(p).is_some_and(|c| c.is_ascii_digit()) {
                        i = p;
                        while i < limit && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
                            i += 1;
                        }
                    }
                }
                let num_end = i;
                out.push(HighlightSpan {
                    kind: HighlightKind::Number,
                    span: Span::from_usize(nstart, num_end),
                });
                if i < limit && bytes[i].is_ascii_alphabetic() {
                    let ustart = i;
                    let mut p = i;
                    while p < limit && bytes[p].is_ascii_alphabetic() {
                        p += 1;
                    }
                    if lookup_unit(&src[ustart..p]).is_some() {
                        out.push(HighlightSpan {
                            kind: HighlightKind::Unit,
                            span: Span::from_usize(ustart, p),
                        });
                        i = p;
                    }
                }
            }
            expect_cmd = false;
            at_stmt_start = false;
            continue;
        }

        // Flags (`--long` anywhere, `-short` in command args)
        if b == b'-' {
            let fstart = i;
            if bytes.get(i + 1) == Some(&b'-')
                && bytes.get(i + 2).is_some_and(|&c| is_ident_start(c))
            {
                i += 2;
                while i < limit && (is_ident_continue(bytes[i]) || bytes[i] == b'-') {
                    i += 1;
                }
                out.push(HighlightSpan {
                    kind: HighlightKind::Flag,
                    span: Span::from_usize(fstart, i),
                });
                continue;
            }
            if in_cmd_args && bytes.get(i + 1).is_some_and(|&c| is_ident_start(c)) {
                i += 1;
                while i < limit && (is_ident_continue(bytes[i]) || bytes[i] == b'-') {
                    i += 1;
                }
                out.push(HighlightSpan {
                    kind: HighlightKind::Flag,
                    span: Span::from_usize(fstart, i),
                });
                continue;
            }
        }

        // Identifiers, keywords, commands, `rust { ... }`
        if is_ident_start(b) {
            let id_start = i;
            i += 1;
            while i < limit {
                if is_ident_continue(bytes[i]) {
                    i += 1;
                } else if bytes[i] == b'-' && bytes.get(i + 1).is_some_and(|&c| is_ident_continue(c))
                {
                    i += 1;
                } else {
                    break;
                }
            }
            let word = &src[id_start..i];
            if let Some(kw) = Keyword::from_name(word) {
                out.push(HighlightSpan {
                    kind: HighlightKind::Keyword,
                    span: Span::from_usize(id_start, i),
                });
                if kw == Keyword::Rust {
                    let mut probe = i;
                    while probe < limit && (bytes[probe] == b' ' || bytes[probe] == b'\t') {
                        probe += 1;
                    }
                    if probe < limit && bytes[probe] == b'{' {
                        out.push(HighlightSpan {
                            kind: HighlightKind::Punct,
                            span: Span::from_usize(probe, probe + 1),
                        });
                        match scan_rust_braced_block(&src[..limit], probe) {
                            RustBlockScan::Closed { body_start, body_end, close_end } => {
                                if body_start < body_end {
                                    out.push(HighlightSpan {
                                        kind: HighlightKind::RustCode,
                                        span: Span::from_usize(body_start, body_end),
                                    });
                                }
                                out.push(HighlightSpan {
                                    kind: HighlightKind::Punct,
                                    span: Span::from_usize(body_end, close_end),
                                });
                                i = close_end;
                            }
                            RustBlockScan::Unclosed | RustBlockScan::DepthExceeded(_) => {
                                if probe + 1 < limit {
                                    out.push(HighlightSpan {
                                        kind: HighlightKind::RustCode,
                                        span: Span::from_usize(probe + 1, limit),
                                    });
                                }
                                i = limit;
                            }
                        }
                    }
                }
                expect_cmd = matches!(kw, Keyword::Else | Keyword::Return);
                at_stmt_start = false;
                in_cmd_args = false;
            } else if expect_cmd {
                // Bare structured data file at stage start (`Cargo.lock`, `blaze.lock`,
                // `Cargo.toml.package.version`, `data.json[n > 1]`): highlight the filename
                // as a data-file path (`String`), NEVER as an unknown red command, and highlight
                // any immediately chained `.field.0` steps cleanly!
                if let Some(file_end) = scan_structured_data_file_end(&src[..limit], id_start, i) {
                    out.push(HighlightSpan {
                        kind: HighlightKind::String,
                        span: Span::from_usize(id_start, file_end),
                    });
                    i = file_end;
                    while i < limit
                        && (bytes[i] == b'.'
                            || (bytes[i] == b'?' && bytes.get(i + 1) == Some(&b'.')))
                    {
                        let sep_len = if bytes[i] == b'?' { 2 } else { 1 };
                        out.push(HighlightSpan {
                            kind: HighlightKind::Punct,
                            span: Span::from_usize(i, i + sep_len),
                        });
                        i += sep_len;
                        let seg_start = i;
                        if i < limit && bytes[i].is_ascii_digit() {
                            while i < limit && bytes[i].is_ascii_digit() {
                                i += 1;
                            }
                            out.push(HighlightSpan {
                                kind: HighlightKind::Number,
                                span: Span::from_usize(seg_start, i),
                            });
                        } else if i < limit && is_ident_start(bytes[i]) {
                            while i < limit && (is_ident_continue(bytes[i]) || bytes[i] == b'-') {
                                i += 1;
                            }
                            out.push(HighlightSpan {
                                kind: HighlightKind::Var,
                                span: Span::from_usize(seg_start, i),
                            });
                        } else {
                            break;
                        }
                        if i < limit && bytes[i] == b'?' && bytes.get(i + 1) != Some(&b'.') {
                            out.push(HighlightSpan {
                                kind: HighlightKind::Punct,
                                span: Span::from_usize(i, i + 1),
                            });
                            i += 1;
                        }
                    }
                    expect_cmd = false;
                    at_stmt_start = false;
                    in_cmd_args = false;
                } else {
                    out.push(HighlightSpan {
                        kind: HighlightKind::Command,
                        span: Span::from_usize(id_start, i),
                    });
                    expect_cmd = false;
                    at_stmt_start = false;
                    in_cmd_args = true;
                }
            } else {
                // Non-keyword bare word (field, subcommand, argument)
                out.push(HighlightSpan {
                    kind: HighlightKind::String,
                    span: Span::from_usize(id_start, i),
                });
                at_stmt_start = false;
            }
            continue;
        }

        // Pipe
        if b == b'|' {
            out.push(HighlightSpan {
                kind: HighlightKind::Pipe,
                span: Span::from_usize(i, i + 1),
            });
            i += 1;
            expect_cmd = true;
            at_stmt_start = false;
            in_cmd_args = false;
            continue;
        }

        // Multi-char operators & punct
        let rem = &bytes[i..limit];
        if rem.starts_with(b"...") {
            out.push(HighlightSpan {
                kind: HighlightKind::Punct,
                span: Span::from_usize(i, i + 3),
            });
            i += 3;
            expect_cmd = false;
            at_stmt_start = false;
            continue;
        }
        if rem.starts_with(b"..<") || rem.starts_with(b"++=") {
            out.push(HighlightSpan {
                kind: HighlightKind::Operator,
                span: Span::from_usize(i, i + 3),
            });
            i += 3;
            expect_cmd = false;
            at_stmt_start = false;
            continue;
        }
        if rem.len() >= 2 {
            let two_kind = match (rem[0], rem[1]) {
                (b'=', b'>') | (b'?', b'.') => Some(HighlightKind::Punct),
                (b'.', b'.')
                | (b'+', b'+')
                | (b'+', b'=')
                | (b'-', b'=')
                | (b'*', b'=')
                | (b'/', b'=')
                | (b'*', b'*')
                | (b'/', b'/')
                | (b'=', b'=')
                | (b'!', b'=')
                | (b'<', b'=')
                | (b'>', b'=')
                | (b'=', b'~')
                | (b'!', b'~') => Some(HighlightKind::Operator),
                _ => None,
            };
            if let Some(kind) = two_kind {
                out.push(HighlightSpan { kind, span: Span::from_usize(i, i + 2) });
                i += 2;
                expect_cmd = false;
                at_stmt_start = false;
                continue;
            }
        }

        // Single-char punct & operators
        match b {
            b'(' => {
                paren_depth = paren_depth.saturating_add(1);
                out.push(HighlightSpan {
                    kind: HighlightKind::Punct,
                    span: Span::from_usize(i, i + 1),
                });
                i += 1;
                expect_cmd = true;
                at_stmt_start = false;
                in_cmd_args = false;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                out.push(HighlightSpan {
                    kind: HighlightKind::Punct,
                    span: Span::from_usize(i, i + 1),
                });
                i += 1;
                expect_cmd = false;
                at_stmt_start = false;
            }
            b'{' | b';' => {
                out.push(HighlightSpan {
                    kind: HighlightKind::Punct,
                    span: Span::from_usize(i, i + 1),
                });
                i += 1;
                expect_cmd = true;
                at_stmt_start = true;
                in_cmd_args = false;
            }
            b'.' if expect_cmd && bytes.get(i + 1) != Some(&b'/') => {
                // Leading `.` / `.package.version` implicit-subject cell path!
                while i < limit && (bytes[i] == b'.' || (bytes[i] == b'?' && bytes.get(i + 1) == Some(&b'.'))) {
                    let sep_len = if bytes[i] == b'?' { 2 } else { 1 };
                    out.push(HighlightSpan {
                        kind: HighlightKind::Punct,
                        span: Span::from_usize(i, i + sep_len),
                    });
                    i += sep_len;
                    let seg_start = i;
                    if i < limit && bytes[i].is_ascii_digit() {
                        while i < limit && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                        out.push(HighlightSpan {
                            kind: HighlightKind::Number,
                            span: Span::from_usize(seg_start, i),
                        });
                    } else if i < limit && is_ident_start(bytes[i]) {
                        while i < limit && (is_ident_continue(bytes[i]) || bytes[i] == b'-') {
                            i += 1;
                        }
                        out.push(HighlightSpan {
                            kind: HighlightKind::Var,
                            span: Span::from_usize(seg_start, i),
                        });
                    } else {
                        break;
                    }
                    if i < limit && bytes[i] == b'?' && bytes.get(i + 1) != Some(&b'.') {
                        out.push(HighlightSpan {
                            kind: HighlightKind::Punct,
                            span: Span::from_usize(i, i + 1),
                        });
                        i += 1;
                    }
                }
                expect_cmd = false;
                at_stmt_start = false;
                in_cmd_args = false;
            }
            b'}' | b'[' | b']' | b',' | b':' | b'.' | b'?' => {
                out.push(HighlightSpan {
                    kind: HighlightKind::Punct,
                    span: Span::from_usize(i, i + 1),
                });
                i += 1;
                expect_cmd = false;
                at_stmt_start = false;
            }
            b'=' | b'+' | b'-' | b'*' | b'/' | b'<' | b'>' | b'!' | b'^' | b'~' => {
                out.push(HighlightSpan {
                    kind: HighlightKind::Operator,
                    span: Span::from_usize(i, i + 1),
                });
                i += 1;
                expect_cmd = false;
                at_stmt_start = false;
            }
            b'\\' => {
                out.push(HighlightSpan {
                    kind: HighlightKind::Punct,
                    span: Span::from_usize(i, i + 1),
                });
                i += 1;
            }
            _ => {
                let ch = src[i..limit].chars().next().unwrap_or('\0');
                let next = i + ch.len_utf8();
                out.push(HighlightSpan {
                    kind: if in_cmd_args { HighlightKind::String } else { HighlightKind::Error },
                    span: Span::from_usize(i, next),
                });
                i = next;
                expect_cmd = false;
                at_stmt_start = false;
            }
        }
    }

    i
}

fn highlight_double_string<'a>(
    src: &str,
    start: usize,
    limit: usize,
    depth: usize,
    out: &mut AVec<'a, HighlightSpan>,
) -> usize {
    let bytes = src.as_bytes();
    let mut i = start;
    if bytes[i] == b'$' {
        i += 1;
    }
    i += 1; // opening `"`
    let mut seg_start = start;

    while i < limit && out.len() < MAX_TOKENS {
        let b = bytes[i];
        if b == b'"' {
            i += 1;
            out.push(HighlightSpan {
                kind: HighlightKind::String,
                span: Span::from_usize(seg_start, i),
            });
            return i;
        }
        if b == b'\\' {
            if i + 1 >= limit {
                if seg_start < i {
                    out.push(HighlightSpan {
                        kind: HighlightKind::String,
                        span: Span::from_usize(seg_start, i),
                    });
                }
                out.push(HighlightSpan {
                    kind: HighlightKind::Error,
                    span: Span::from_usize(i, limit),
                });
                return limit;
            }
            let valid = matches!(
                bytes[i + 1],
                b'"' | b'\\' | b'n' | b't' | b'r' | b'(' | b'\'' | b'0' | b'/'
            );
            if !valid {
                if seg_start < i {
                    out.push(HighlightSpan {
                        kind: HighlightKind::String,
                        span: Span::from_usize(seg_start, i),
                    });
                }
                out.push(HighlightSpan {
                    kind: HighlightKind::Error,
                    span: Span::from_usize(i, i + 2),
                });
                i += 2;
                seg_start = i;
                continue;
            }
            i += 2;
            continue;
        }
        if b == b'(' {
            if seg_start < i {
                out.push(HighlightSpan {
                    kind: HighlightKind::String,
                    span: Span::from_usize(seg_start, i),
                });
            }
            out.push(HighlightSpan {
                kind: HighlightKind::Punct,
                span: Span::from_usize(i, i + 1),
            });
            let after = highlight_range(src, i + 1, limit, depth + 1, out);
            if after < limit && bytes[after] == b')' {
                out.push(HighlightSpan {
                    kind: HighlightKind::Punct,
                    span: Span::from_usize(after, after + 1),
                });
                i = after + 1;
                seg_start = i;
            } else {
                return after;
            }
            continue;
        }
        let ch = src[i..limit].chars().next().unwrap_or('\0');
        i += ch.len_utf8();
    }

    // Unterminated string
    out.push(HighlightSpan {
        kind: HighlightKind::Error,
        span: Span::from_usize(seg_start, limit),
    });
    limit
}

/// Starting at an identifier `src[start..first_ident_end]` in command position, scan forward
/// across touching path/dot segments (`Cargo.lock`, `crates/foo/Cargo.toml`, `data.json`) and
/// return the byte end of the structured data file (stopping right before postfix `.field` or `[`).
fn scan_structured_data_file_end(src: &str, start: usize, first_ident_end: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut p = first_ident_end;
    while p < bytes.len() {
        let c = bytes[p];
        if c == b'.' {
            let dot_pos = p;
            p += 1;
            let ext_start = p;
            while p < bytes.len() && is_ident_continue(bytes[p]) {
                p += 1;
            }
            if ext_start < p {
                let ext = &src[ext_start..p];
                let is_data_ext = matches!(
                    ext,
                    "toml" | "lock" | "json" | "jsonl" | "ndjson" | "csv" | "tsv"
                        | "TOML" | "LOCK" | "JSON" | "JSONL" | "NDJSON" | "CSV" | "TSV"
                );
                if is_data_ext {
                    // Valid end if EOF, whitespace, postfix `.`/`?.`/`[`, or delimiter (`|`, `;`, `)`, etc.).
                    let ok = match bytes.get(p) {
                        None => true,
                        Some(b' ' | b'\t' | b'\n' | b'\r' | b'.' | b'?' | b'[' | b'|' | b';' | b')' | b']' | b'}' | b',') => true,
                        _ => false,
                    };
                    if ok && start < dot_pos {
                        return Some(p);
                    }
                }
            }
        } else if c == b'/' || c == b'-' || is_ident_continue(c) {
            p += 1;
        } else {
            break;
        }
    }
    None
}
