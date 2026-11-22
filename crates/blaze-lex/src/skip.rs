//! The body skipper: given text that starts at `{`, the offset of the `}` that closes it, equal
//! on every input to tokenizing the text (`FrontmatterAllowed::No`) and counting `OpenBrace`
//! and `CloseBrace` tokens, but without making tokens.
//!
//! A structural scan in the manner of simdjson's first stage: each 64-byte block is classified
//! once into one mask of the bytes that can open or close anything (`{ } " ' / #`), and the scan
//! jumps from one such byte to the next by `trailing_zeros`. Every other byte is *boring*: ASCII
//! letters, digits, whitespace, the other punctuation, and non-ASCII characters, none of which
//! can start a token that swallows a brace.
//!
//! At each structural byte `q`:
//! - `{` and `}` are tokens: the depth moves.
//! - `/` starts a token (no token that starts on a boring byte holds a `/`): a line comment runs
//!   to the next `\n` by byte search, a block comment to its matching `*/` by searching `/` and
//!   `*` with nesting, and a lone `/` is a `Slash`.
//! - `"` starts a double-quoted string unless an `r`, `br` or `cr` that starts a token is right
//!   before it. What is before it is read back: the run of ASCII identifier bytes before `q`,
//!   and the byte before that run. When that byte is whitespace or punctuation that is always a
//!   token of its own (`( ) [ ] { } , ; = : ! & | * < > @ ~ ? $ % ^`), or the run starts where
//!   the scan last stood on a token boundary, the run starts a token: if the run is not a raw
//!   prefix, the string's end is found by searching `"` and `\` (a `\` takes the `\` or `"`
//!   after it, rustc_lexer's rule), and its suffix by the exact lexer's `eat_identifier`.
//! - Anything else, and a `"` the masks cannot place, goes to the exact lexer.
//!
//! The fallback rule, and why it is sound: the exact lexer is started only at a token boundary
//! (the run's start, when the byte before it is one of the delimiters above or the last
//! boundary; else after the last ASCII whitespace byte before `q`, or the last boundary), and it
//! lexes whole tokens until one ends past `q`; the scan resumes at that token's end, which is a
//! token boundary. Between the last boundary and `q` there is no structural byte, so the tokens
//! there are identifiers, numbers, whitespace, single-byte punctuation and non-ASCII tokens:
//! none holds a brace, and none extends over whitespace, so a position just after an ASCII
//! whitespace byte there is either a boundary or inside a whitespace token, after which
//! lexing gives the same tokens. Every brace the scan counts is thus a brace token, and every
//! brace inside a string, character, comment or raw string is skipped with the token that holds
//! it, as tokenizing would.

use blaze_int::Int;
use blaze_memchr::memchr;

use crate::Cpu;
use crate::classify::{Structure, select_at};
use crate::lexer::{Cursor, Plant, block_comment_end, quoted_end};

/// Why there is no closing brace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipError {
    /// The text does not start with `{`.
    NotAtBrace,
    /// The text ends before the brace closes (inside a string or comment, or with braces open).
    Unclosed,
}

impl core::fmt::Display for SkipError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SkipError::NotAtBrace => f.write_str("a body to skip starts with `{`, and this text does not"),
            SkipError::Unclosed => f.write_str("the body's `{` is never closed: the text ends inside it"),
        }
    }
}

impl core::error::Error for SkipError {}

/// The offset in `text` of the `}` that closes the `{` at its start.
pub fn skip_body(cpu: Cpu, text: &str) -> Result<usize, SkipError> {
    skip_body_in(cpu, text, Plant::Clean)
}

pub(crate) fn skip_body_in(cpu: Cpu, text: &str, plant: Plant) -> Result<usize, SkipError> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'{') {
        return Err(SkipError::NotAtBrace);
    }
    let mut scan = Scan { cpu, bytes, block: usize::MAX, mask: 0 };
    let mut depth = 0usize;
    // Everything before `pos` is resolved, and `pos` is a token boundary.
    let mut pos = 0;
    loop {
        let q = scan.next(pos).ok_or(SkipError::Unclosed)?;
        pos = match bytes[q] {
            b'{' => {
                depth += 1;
                q + 1
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(q);
                }
                q + 1
            }
            b'/' => match bytes.get(q + 1) {
                Some(b'/') => q + 2 + memchr(b'\n', &bytes[q + 2..]).ok_or(SkipError::Unclosed)?,
                Some(b'*') => match block_comment_end(bytes, q + 2) {
                    (end, true) => end,
                    (_, false) => return Err(SkipError::Unclosed),
                },
                _ => q + 1,
            },
            b'"' => {
                if q == pos || bytes[q - 1] != b'r' {
                    let end = quoted_end(bytes, q + 1, plant).ok_or(SkipError::Unclosed)?;
                    match bytes.get(end) {
                        Some(&b) if b.is_ascii_alphabetic() || b == b'_' || b >= 0x80 => {
                            string_suffix(cpu, text, end, plant)
                        }
                        _ => end,
                    }
                } else {
                    match token_start(bytes, pos, q) {
                        Some(start) if !matches!(&bytes[start..q], b"r" | b"br" | b"cr") => {
                            let end = quoted_end(bytes, q + 1, plant).ok_or(SkipError::Unclosed)?;
                            match bytes.get(end) {
                                Some(&b) if b.is_ascii_alphabetic() || b == b'_' || b >= 0x80 => {
                                    string_suffix(cpu, text, end, plant)
                                }
                                _ => end,
                            }
                        }
                        Some(start) => exact(cpu, text, start, q, plant),
                        None => exact(cpu, text, resync(bytes, pos, q), q, plant),
                    }
                }
            }
            b'#' => {
                if !matches!(bytes.get(q + 1), Some(b'"' | b'#'))
                    && (q == pos || !matches!(bytes[q - 1], b'r' | b'k'))
                {
                    q + 1
                } else {
                    let start = token_start(bytes, pos, q).unwrap_or_else(|| resync(bytes, pos, q));
                    exact(cpu, text, start, q, plant)
                }
            }
            b'\'' => match (bytes.get(q + 1).copied(), bytes.get(q + 2).copied()) {
                // Single-byte ASCII char/byte literal (`'x'` or `b'x'`) without suffix.
                (Some(b1), Some(b'\''))
                    if b1 < 0x80
                        && b1 != b'\\'
                        && b1 != b'\''
                        && !matches!(
                            bytes.get(q + 3),
                            Some(&b) if b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
                        ) =>
                {
                    q + 3
                }
                // Simple ASCII lifetime (`'a`, `'static`, ...) not preceded by `b` (which could be `b'`).
                (Some(b1), b2)
                    if plant == Plant::Clean
                        && (b1.is_ascii_alphabetic() || b1 == b'_')
                        && (q == pos || bytes[q - 1] != b'b')
                        && b2 != Some(b'\'') =>
                {
                    let mut end = q + 2;
                    while let Some(&b) = bytes.get(end) {
                        if is_ident_byte(b) {
                            end += 1;
                        } else {
                            break;
                        }
                    }
                    match bytes.get(end) {
                        Some(&b) if b == b'\'' || b == b'#' || b >= 0x80 => {
                            let start = token_start(bytes, pos, q).unwrap_or_else(|| resync(bytes, pos, q));
                            exact(cpu, text, start, q, plant)
                        }
                        _ => end,
                    }
                }
                _ => {
                    let start = token_start(bytes, pos, q).unwrap_or_else(|| resync(bytes, pos, q));
                    exact(cpu, text, start, q, plant)
                }
            },
            _ => {
                let start = token_start(bytes, pos, q).unwrap_or_else(|| resync(bytes, pos, q));
                exact(cpu, text, start, q, plant)
            }
        };
    }
}

/// The structural bytes, a block of masks at a time.
struct Scan<'a> {
    cpu: Cpu,
    bytes: &'a [u8],
    block: usize,
    mask: u64,
}

impl Scan<'_> {
    /// The first structural byte at or after `from`.
    #[inline(always)]
    fn next(&mut self, from: usize) -> Option<usize> {
        let mut at = from;
        while at < self.bytes.len() {
            let block = at / 64;
            if block != self.block {
                let [mask] = select_at::<Structure, 1>(self.cpu, self.bytes, block * 64);
                self.mask = mask;
                self.block = block;
            }
            let rest = self.mask >> (at % 64);
            if rest != 0 {
                return Some(at + rest.trailing_zeros().widen_to::<usize>());
            }
            at = (block + 1) * 64;
        }
        None
    }
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// A byte that is always a whole token, or whitespace, outside strings and comments.
fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'\t' | b'\n'
            | 0x0B
            | 0x0C
            | b'\r'
            | b' '
            | b'('
            | b')'
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b','
            | b';'
            | b'='
            | b':'
            | b'!'
            | b'&'
            | b'|'
            | b'*'
            | b'<'
            | b'>'
            | b'@'
            | b'~'
            | b'?'
            | b'$'
            | b'%'
            | b'^'
    )
}

/// The start of the token that holds `q` or ends right before it, when the bytes say so: the run
/// of ASCII identifier bytes before `q` starts a token when the byte before it is a delimiter
/// or the boundary.
fn token_start(bytes: &[u8], boundary: usize, q: usize) -> Option<usize> {
    let mut start = q;
    while start > boundary && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    if start == boundary || is_delimiter(bytes[start - 1]) { Some(start) } else { None }
}

/// The last place before `q` lexing may restart: just after the last ASCII whitespace byte
/// after `boundary`, or `boundary`.
fn resync(bytes: &[u8], boundary: usize, q: usize) -> usize {
    let mut at = q;
    while at > boundary {
        if matches!(bytes[at - 1], b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ') {
            return at;
        }
        at -= 1;
    }
    boundary
}

/// Eats an identifier suffix after a double-quoted string literal (rare in Rust).
#[cold]
#[inline(never)]
fn string_suffix(cpu: Cpu, text: &str, end: usize, plant: Plant) -> usize {
    let mut suffix = Cursor::new(text, end, false, cpu, plant);
    suffix.eat_identifier();
    suffix.pos()
}

/// The end of the token that holds `q`, lexing whole tokens from the boundary `start`.
#[cold]
#[inline(never)]
fn exact(cpu: Cpu, text: &str, start: usize, q: usize, plant: Plant) -> usize {
    let mut cursor = Cursor::new(text, start, false, cpu, plant);
    loop {
        let end = cursor.next_end();
        if end > q {
            return end;
        }
    }
}
