//! rustc's cooking of a `rustc_lexer` literal or doc comment into one of its tokens
//! (`rustc_parse::lexer::Lexer::{cook_lexer_literal, cook_quoted, report_raw_str_error,
//! cook_doc_comment}` and the primary messages of `unescape_error_reporting::
//! emit_unescape_error`), with every error rustc reports on the way. blaze-fmt's and
//! blaze-macro-eval's ports of rustc's lexer both cook through this; each keeps its own tokens
//! and decides what an error does to it.

use std::borrow::Cow;
use std::ops::Range;

use rustc_lexer::{Base, LiteralKind, RawStrError, validate_raw_str};
use rustc_literal_escaper::{EscapeError, Mode, check_for_errors};

/// `rustc_ast::token::LitKind` as rustc's lexer makes it: never `Bool`, and `Err` without the
/// `ErrorGuaranteed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LitKind {
    Byte,
    Char,
    Integer,
    Float,
    Str,
    StrRaw(u8),
    ByteStr,
    ByteStrRaw(u8),
    CStr,
    CStrRaw(u8),
    Err,
}

/// An error rustc reports while cooking, at `range`: byte offsets from the token's first byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CookError {
    pub range: Range<u32>,
    pub message: Cow<'static, str>,
}

impl CookError {
    fn new(range: Range<u32>, message: impl Into<Cow<'static, str>>) -> CookError {
        CookError { range, message: message.into() }
    }
}

/// `cook_lexer_literal` for the literal token at the start of `rest` (the text from the
/// token's first byte to the end of the file, which a bad raw string is checked against),
/// `token_len` bytes long with its suffix from `suffix_start`.
///
/// Each error rustc emits and goes on from goes to `report`; the one rustc stops the lexer at
/// (an unterminated literal, a bad raw string) is the `Err`. The symbol is the literal between
/// its quotes, or all of it before the suffix when it has an error, as rustc's is.
pub fn cook_lexer_literal<'s>(
    rest: &'s str,
    token_len: u32,
    suffix_start: u32,
    kind: LiteralKind,
    mut report: impl FnMut(CookError),
) -> Result<(LitKind, &'s str), CookError> {
    let end = suffix_start;
    let unterminated = |terminated: bool, message: &'static str| match terminated {
        true => Ok(()),
        false => Err(CookError::new(0..end, message)),
    };
    let report = &mut report;
    Ok(match kind {
        LiteralKind::Char { terminated } => {
            unterminated(terminated, "unterminated character literal")?;
            cook_quoted(rest, LitKind::Char, Mode::Char, end, 1, 1, report)
        }
        LiteralKind::Byte { terminated } => {
            unterminated(terminated, "unterminated byte constant")?;
            cook_quoted(rest, LitKind::Byte, Mode::Byte, end, 2, 1, report)
        }
        LiteralKind::Str { terminated } => {
            unterminated(terminated, "unterminated double quote string")?;
            cook_quoted(rest, LitKind::Str, Mode::Str, end, 1, 1, report)
        }
        LiteralKind::ByteStr { terminated } => {
            unterminated(terminated, "unterminated double quote byte string")?;
            cook_quoted(rest, LitKind::ByteStr, Mode::ByteStr, end, 2, 1, report)
        }
        LiteralKind::CStr { terminated } => {
            unterminated(terminated, "unterminated C string")?;
            cook_quoted(rest, LitKind::CStr, Mode::CStr, end, 2, 1, report)
        }
        LiteralKind::RawStr { n_hashes } => match n_hashes {
            Some(n) => {
                let n32 = u32::from(n);
                cook_quoted(rest, LitKind::StrRaw(n), Mode::RawStr, end, 2 + n32, 1 + n32, report)
            }
            None => return Err(raw_str_error(rest, token_len, 1)),
        },
        LiteralKind::RawByteStr { n_hashes } => match n_hashes {
            Some(n) => {
                let n32 = u32::from(n);
                let kind = LitKind::ByteStrRaw(n);
                cook_quoted(rest, kind, Mode::RawByteStr, end, 3 + n32, 1 + n32, report)
            }
            None => return Err(raw_str_error(rest, token_len, 2)),
        },
        LiteralKind::RawCStr { n_hashes } => match n_hashes {
            Some(n) => {
                let n32 = u32::from(n);
                cook_quoted(rest, LitKind::CStrRaw(n), Mode::RawCStr, end, 3 + n32, 1 + n32, report)
            }
            None => return Err(raw_str_error(rest, token_len, 2)),
        },
        LiteralKind::Int { base, empty_int } => {
            let mut kind = LitKind::Integer;
            if empty_int {
                report(CookError::new(0..end, "no valid digits found for number"));
                kind = LitKind::Err;
            } else if matches!(base, Base::Binary | Base::Octal) {
                let radix = base as u32;
                for (idx, c) in rest[2..end as usize].char_indices() {
                    if c != '_' && c.to_digit(radix).is_none() {
                        let lo = 2 + idx as u32;
                        // alloc: error path
                        let message = format!("invalid digit for a base {radix} literal");
                        report(CookError::new(lo..lo + c.len_utf8() as u32, message));
                        kind = LitKind::Err;
                    }
                }
            }
            (kind, &rest[..end as usize])
        }
        LiteralKind::Float { base, empty_exponent } => {
            let mut kind = LitKind::Float;
            if empty_exponent {
                report(CookError::new(0..token_len, "expected at least one digit in exponent"));
                kind = LitKind::Err;
            }
            let base = match base {
                Base::Hexadecimal => Some("hexadecimal"),
                Base::Octal => Some("octal"),
                Base::Binary => Some("binary"),
                Base::Decimal => None,
            };
            if let Some(base) = base {
                // alloc: error path
                report(CookError::new(0..end, format!("{base} float literal is not supported")));
                kind = LitKind::Err;
            }
            (kind, &rest[..end as usize])
        }
    })
}

/// `cook_quoted`: the literal's contents between `prefix_len` and `postfix_len` bytes of
/// quotes checked for escape errors.
fn cook_quoted<'s>(
    rest: &'s str,
    mut kind: LitKind,
    mode: Mode,
    end: u32,
    prefix_len: u32,
    postfix_len: u32,
    report: &mut impl FnMut(CookError),
) -> (LitKind, &'s str) {
    let content = &rest[prefix_len as usize..(end - postfix_len) as usize];
    check_for_errors(content, mode, |range, err| {
        // clone: a range, two words
        if let Some(message) = escape_message(content, mode, range.clone(), &err) {
            debug_assert!(err.is_fatal());
            let range = prefix_len + range.start as u32..prefix_len + range.end as u32;
            report(CookError::new(range, message));
            kind = LitKind::Err;
        }
    });
    // An error keeps the quotes in the symbol, as rustc's does.
    match kind {
        LitKind::Err => (kind, &rest[..end as usize]),
        _ => (kind, content),
    }
}

/// `report_raw_str_error`, always fatal: why the raw string at the start of `rest` (after a
/// `prefix_len`-byte `r` or `br`) is bad.
fn raw_str_error(rest: &str, token_len: u32, prefix_len: u32) -> CookError {
    let message: Cow<'static, str> = match validate_raw_str(rest, prefix_len) {
        Err(RawStrError::InvalidStarter { bad_char }) => {
            // alloc: error path
            let mut message = String::from(
                "found invalid character; only `#` is allowed in raw string delimitation: ",
            );
            escaped_char(bad_char, &mut message);
            message.into()
        }
        Err(RawStrError::NoTerminator { .. }) => "unterminated raw string".into(),
        // alloc: error path
        Err(RawStrError::TooManyDelimiters { found }) => format!(
            "too many `#` symbols: raw strings may be delimited by up to 255 `#` symbols, but found {found}"
        )
        .into(),
        Ok(()) => panic!("no error found for supposedly invalid raw string literal"),
    };
    CookError::new(0..token_len, message)
}

/// `cook_doc_comment`'s errors: one for each bare CR in `content` (the comment after its
/// `///`, `//!`, `/**` or `/*!`), at its offset in `content`.
pub fn doc_comment_errors(content: &str, block: bool, mut report: impl FnMut(CookError)) {
    let message = match block {
        true => "bare CR not allowed in block doc-comment",
        false => "bare CR not allowed in doc-comment",
    };
    for (idx, _) in content.bytes().enumerate().filter(|&(_, b)| b == b'\r') {
        let idx = idx as u32;
        report(CookError::new(idx..idx + 1, message));
    }
}

/// `escaped_char`: a character as rustc's messages show it.
pub fn escaped_char(c: char, out: &mut String) {
    match c {
        '\u{20}'..='\u{7e}' => out.push(c),
        _ => out.extend(c.escape_default()),
    }
}

/// The primary message `emit_unescape_error` emits for `error` at `range` of `lit` (the
/// literal between its quotes), or `None` for the two that are warnings.
pub fn escape_message(lit: &str, mode: Mode, range: Range<usize>, error: &EscapeError) -> Option<String> {
    let last_char =
        || lit[range.start..range.end].chars().next_back().expect("an escape error covers a character");
    // The message, and the character it ends with between backquotes.
    let (head, shown): (&str, Option<char>) = match error {
        EscapeError::LoneSurrogateUnicodeEscape | EscapeError::OutOfRangeUnicodeEscape => {
            ("invalid unicode character escape", None)
        }
        EscapeError::MoreThanOneChar => ("character literal may only contain one codepoint", None),
        EscapeError::EscapeOnlyChar => match mode {
            Mode::Byte => ("byte constant must be escaped: ", Some(last_char())),
            _ => ("character constant must be escaped: ", Some(last_char())),
        },
        EscapeError::BareCarriageReturn => match mode.in_double_quotes() {
            true => ("bare CR not allowed in string, use `\\r` instead", None),
            false => ("character constant must be escaped: `\\r`", None),
        },
        EscapeError::BareCarriageReturnInRawString => ("bare CR not allowed in raw string", None),
        EscapeError::InvalidEscape => match mode {
            Mode::Byte | Mode::ByteStr => ("unknown byte escape: ", Some(last_char())),
            _ => ("unknown character escape: ", Some(last_char())),
        },
        EscapeError::TooShortHexEscape => ("numeric character escape is too short", None),
        EscapeError::InvalidCharInHexEscape => {
            ("invalid character in numeric character escape: ", Some(last_char()))
        }
        EscapeError::InvalidCharInUnicodeEscape => {
            ("invalid character in unicode escape: ", Some(last_char()))
        }
        EscapeError::NonAsciiCharInByte => match mode {
            Mode::Byte => ("non-ASCII character in byte literal", None),
            Mode::ByteStr => ("non-ASCII character in byte string literal", None),
            Mode::RawByteStr => ("non-ASCII character in raw byte string literal", None),
            _ => panic!("non-is_byte literal paired with NonAsciiCharInByte"),
        },
        EscapeError::OutOfRangeHexEscape => ("out of range hex escape", None),
        EscapeError::LeadingUnderscoreUnicodeEscape => ("invalid start of unicode escape: `_`", None),
        EscapeError::OverlongUnicodeEscape => ("overlong unicode escape", None),
        EscapeError::UnclosedUnicodeEscape => ("unterminated unicode escape", None),
        EscapeError::NoBraceInUnicodeEscape => ("incorrect unicode escape sequence", None),
        EscapeError::UnicodeEscapeInByte => ("unicode escape in byte string", None),
        EscapeError::EmptyUnicodeEscape => ("empty unicode escape", None),
        EscapeError::ZeroChars => ("empty character literal", None),
        EscapeError::LoneSlash => ("invalid trailing slash in literal", None),
        EscapeError::NulInCStr => ("null characters in C string literals are not supported", None),
        EscapeError::UnskippedWhitespaceWarning | EscapeError::MultipleSkippedLinesWarning => {
            return None;
        }
    };
    // alloc: error path, once per error
    let mut message = String::from(head);
    if let Some(c) = shown {
        message.push('`');
        escaped_char(c, &mut message);
        message.push('`');
    }
    Some(message)
}

#[cfg(test)]
mod tests;
