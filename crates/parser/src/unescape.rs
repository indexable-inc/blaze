//! The values of Rust's literals, as rustc unescapes them: the one unescaper of the tree, in place
//! of the crates.io `rustc-literal-escaper` (rust-lang/literal-escaper 0.0.8), whose API and
//! errors it keeps, so a literal is refused, warned about and read exactly as the pinned nightly
//! does. The lexer finds where a literal ends (`rustc_lexer`); this says what is between its
//! quotes.
//!
//! - [`unescape_char`] and [`unescape_byte`]: the one value of `'..'` and `b'..'`.
//! - [`unescape_str`], [`unescape_byte_str`] and [`unescape_c_str`]: each value of a quoted
//!   string with the byte range of the text it came from, an error in place of a value, and the
//!   two warnings of a line continuation (`\` then a newline skips the whitespace after it).
//! - [`check_raw_str`] and its byte and C forms: a raw string's characters, refusing a bare
//!   `\r` (and in bytes anything not ASCII, in C strings a NUL).
//! - [`check_for_errors`]: only the errors, for any [`Mode`].
//!
//! The three string kinds differ only in what a character, a `\x` and a `\u` make ([`Unit`]):
//! a `char` (any `\x` under 0x80), a `u8` (any `\x`, no `\u`), or a [`MixedUnit`] of a C string
//! (a character or a high byte, never zero).

use core::{num::NonZero, ops::Range, str::Chars};

use crate::line_break::{is_bare_cr, line_break_len};

#[cfg(test)]
mod tests;

/// What is wrong with a literal's text; the last two are warnings ([`EscapeError::is_fatal`]).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum EscapeError {
    /// `''`.
    ZeroChars,
    /// More than one character in a character literal.
    MoreThanOneChar,
    /// A `\` that ends the literal.
    LoneSlash,
    /// `\` and a character that escapes nothing.
    InvalidEscape,
    /// A `\r` not followed by `\n`.
    BareCarriageReturn,
    /// A `\r` in a raw string.
    BareCarriageReturnInRawString,
    /// A character that must be escaped here (`'`, a tab, a newline in a char; `"` in a string).
    EscapeOnlyChar,
    /// `\x` with fewer than two digits.
    TooShortHexEscape,
    /// `\x` with a character that is no hex digit.
    InvalidCharInHexEscape,
    /// `\x80` and above in a `char` or a string.
    OutOfRangeHexEscape,
    /// `\u` without `{`.
    NoBraceInUnicodeEscape,
    /// `\u{..}` with a character that is no hex digit.
    InvalidCharInUnicodeEscape,
    /// `\u{}`.
    EmptyUnicodeEscape,
    /// `\u{` without `}`.
    UnclosedUnicodeEscape,
    /// `\u{_..}`.
    LeadingUnderscoreUnicodeEscape,
    /// `\u{..}` with more than six digits.
    OverlongUnicodeEscape,
    /// `\u{D800}` to `\u{DFFF}`.
    LoneSurrogateUnicodeEscape,
    /// `\u{..}` above `char::MAX`.
    OutOfRangeUnicodeEscape,
    /// `\u{..}` in a byte or byte string.
    UnicodeEscapeInByte,
    /// A character that is not ASCII in a byte or byte string.
    NonAsciiCharInByte,
    /// A zero in a C string.
    NulInCStr,
    /// A warning: a line continuation followed by whitespace it does not skip (not ASCII).
    UnskippedWhitespaceWarning,
    /// A warning: a line continuation that skips more than one line.
    MultipleSkippedLinesWarning,
}

impl EscapeError {
    /// Whether the literal is refused; the two warnings are not.
    pub fn is_fatal(&self) -> bool {
        !matches!(self, EscapeError::UnskippedWhitespaceWarning | EscapeError::MultipleSkippedLinesWarning)
    }
}

/// The kind of a literal, by its quotes and prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Char,
    Byte,
    Str,
    RawStr,
    ByteStr,
    RawByteStr,
    CStr,
    RawCStr,
}

impl Mode {
    pub fn in_double_quotes(self) -> bool {
        !matches!(self, Mode::Char | Mode::Byte)
    }

    /// The prefix without `r`: `""`, `"b"` or `"c"`.
    pub fn prefix_noraw(self) -> &'static str {
        match self {
            Mode::Char | Mode::Str | Mode::RawStr => "",
            Mode::Byte | Mode::ByteStr | Mode::RawByteStr => "b",
            Mode::CStr | Mode::RawCStr => "c",
        }
    }
}

/// A value of a C string: a character, or a byte of 0x80 and above from `\x`; never zero.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MixedUnit {
    Char(NonZero<char>),
    HighByte(NonZero<u8>),
}

/// What one kind of literal makes of a character, a simple escape, `\0`, `\x` and `\u`.
trait Unit: Sized {
    /// `\0`.
    const ZERO: Result<Self, EscapeError>;
    /// `\n`, `\t`, `\\` and the other simple escapes, and an ASCII `\x` where it is one.
    fn ascii(byte: NonZero<u8>) -> Self;
    fn char(c: char) -> Result<Self, EscapeError>;
    fn hex(byte: u8) -> Result<Self, EscapeError>;
    fn unicode(c: Result<char, EscapeError>) -> Result<Self, EscapeError>;
}

impl Unit for char {
    const ZERO: Result<char, EscapeError> = Ok('\0');

    fn ascii(byte: NonZero<u8>) -> char {
        char::from(byte.get())
    }

    fn char(c: char) -> Result<char, EscapeError> {
        Ok(c)
    }

    fn hex(byte: u8) -> Result<char, EscapeError> {
        if byte.is_ascii() { Ok(char::from(byte)) } else { Err(EscapeError::OutOfRangeHexEscape) }
    }

    fn unicode(c: Result<char, EscapeError>) -> Result<char, EscapeError> {
        c
    }
}

impl Unit for u8 {
    const ZERO: Result<u8, EscapeError> = Ok(0);

    fn ascii(byte: NonZero<u8>) -> u8 {
        byte.get()
    }

    fn char(c: char) -> Result<u8, EscapeError> {
        ascii_byte(c)
    }

    fn hex(byte: u8) -> Result<u8, EscapeError> {
        Ok(byte)
    }

    fn unicode(_: Result<char, EscapeError>) -> Result<u8, EscapeError> {
        Err(EscapeError::UnicodeEscapeInByte)
    }
}

impl Unit for MixedUnit {
    const ZERO: Result<MixedUnit, EscapeError> = Err(EscapeError::NulInCStr);

    fn ascii(byte: NonZero<u8>) -> MixedUnit {
        MixedUnit::Char(NonZero::new(char::from(byte.get())).expect("a byte that is not zero is a char that is not"))
    }

    fn char(c: char) -> Result<MixedUnit, EscapeError> {
        NonZero::new(c).map(MixedUnit::Char).ok_or(EscapeError::NulInCStr)
    }

    fn hex(byte: u8) -> Result<MixedUnit, EscapeError> {
        let byte = NonZero::new(byte).ok_or(EscapeError::NulInCStr)?;
        Ok(if byte.get().is_ascii() { MixedUnit::ascii(byte) } else { MixedUnit::HighByte(byte) })
    }

    fn unicode(c: Result<char, EscapeError>) -> Result<MixedUnit, EscapeError> {
        MixedUnit::char(c?)
    }
}

fn ascii_byte(c: char) -> Result<u8, EscapeError> {
    u8::try_from(c).ok().filter(u8::is_ascii).ok_or(EscapeError::NonAsciiCharInByte)
}

/// The value of `'..'`'s text.
pub fn unescape_char(src: &str) -> Result<char, EscapeError> {
    single(&mut src.chars())
}

/// The value of `b'..'`'s text.
pub fn unescape_byte(src: &str) -> Result<u8, EscapeError> {
    single(&mut src.chars())
}

/// Each value of `".."`'s text, with the range it came from.
pub fn unescape_str(src: &str, callback: impl FnMut(Range<usize>, Result<char, EscapeError>)) {
    quoted(src, line_break_len, callback);
}

/// [`unescape_str`] with the continuation's line break recognised by `line_break`: the seam a
/// test plants a wrong rule through (crate::line_break's tests).
#[cfg(test)]
pub(crate) fn unescape_str_breaking(
    src: &str,
    line_break: fn(&[u8]) -> Option<usize>,
    callback: impl FnMut(Range<usize>, Result<char, EscapeError>),
) {
    quoted(src, line_break, callback);
}

/// Each value of `b".."`'s text, with the range it came from.
pub fn unescape_byte_str(src: &str, callback: impl FnMut(Range<usize>, Result<u8, EscapeError>)) {
    quoted(src, line_break_len, callback);
}

/// Each value of `c".."`'s text, with the range it came from.
pub fn unescape_c_str(src: &str, callback: impl FnMut(Range<usize>, Result<MixedUnit, EscapeError>)) {
    quoted(src, line_break_len, callback);
}

/// Each character of `r".."`'s text.
pub fn check_raw_str(src: &str, callback: impl FnMut(Range<usize>, Result<char, EscapeError>)) {
    raw(src, Ok, callback);
}

/// Each byte of `br".."`'s text.
pub fn check_raw_byte_str(src: &str, callback: impl FnMut(Range<usize>, Result<u8, EscapeError>)) {
    raw(src, ascii_byte, callback);
}

/// Each character of `cr".."`'s text.
pub fn check_raw_c_str(src: &str, callback: impl FnMut(Range<usize>, Result<NonZero<char>, EscapeError>)) {
    raw(src, |c| NonZero::new(c).ok_or(EscapeError::NulInCStr), callback);
}

/// Only the errors (and warnings) of a literal of `mode`, with their ranges.
pub fn check_for_errors(src: &str, mode: Mode, mut error: impl FnMut(Range<usize>, EscapeError)) {
    fn errors<T>(error: &mut impl FnMut(Range<usize>, EscapeError)) -> impl FnMut(Range<usize>, Result<T, EscapeError>) {
        move |range, value| {
            if let Err(e) = value {
                error(range, e);
            }
        }
    }
    match mode {
        Mode::Char | Mode::Byte => {
            let mut chars = src.chars();
            let value = match mode {
                Mode::Char => single::<char>(&mut chars).map(drop),
                _ => single::<u8>(&mut chars).map(drop),
            };
            if let Err(e) = value {
                error(0..src.len() - chars.as_str().len(), e);
            }
        }
        Mode::Str => unescape_str(src, errors(&mut error)),
        Mode::ByteStr => unescape_byte_str(src, errors(&mut error)),
        Mode::CStr => unescape_c_str(src, errors(&mut error)),
        Mode::RawStr => check_raw_str(src, errors(&mut error)),
        Mode::RawByteStr => check_raw_byte_str(src, errors(&mut error)),
        Mode::RawCStr => check_raw_c_str(src, errors(&mut error)),
    }
}

fn raw<T>(
    src: &str,
    unit: impl Fn(char) -> Result<T, EscapeError>,
    mut callback: impl FnMut(Range<usize>, Result<T, EscapeError>),
) {
    let mut chars = src.char_indices();
    while let Some((at, c)) = chars.next() {
        let rest = &src.as_bytes()[at..];
        // A CR LF is one line break, whose value is LF (crate::line_break).
        let (value, len) = match line_break_len(rest) {
            Some(2) => {
                chars.next();
                (unit('\n'), 2)
            }
            _ if is_bare_cr(rest) => (Err(EscapeError::BareCarriageReturnInRawString), 1),
            _ => (unit(c), c.len_utf8()),
        };
        callback(at..at + len, value);
    }
}

/// The one value of a character or byte literal: nothing may follow it.
fn single<T: Unit>(chars: &mut Chars<'_>) -> Result<T, EscapeError> {
    let value = match chars.next().ok_or(EscapeError::ZeroChars)? {
        '\\' => escape(chars),
        '\n' | '\t' | '\'' => Err(EscapeError::EscapeOnlyChar),
        // A CR LF is a line break, which a character literal may not hold unescaped.
        '\r' if chars.as_str().starts_with('\n') => {
            chars.next();
            Err(EscapeError::EscapeOnlyChar)
        }
        '\r' => Err(EscapeError::BareCarriageReturn),
        c => T::char(c),
    }?;
    if chars.next().is_some() {
        return Err(EscapeError::MoreThanOneChar);
    }
    Ok(value)
}

/// The value of an escape, after its `\`.
fn escape<T: Unit>(chars: &mut Chars<'_>) -> Result<T, EscapeError> {
    let simple = match chars.next().ok_or(EscapeError::LoneSlash)? {
        '0' => return T::ZERO,
        '"' => b'"',
        'n' => b'\n',
        'r' => b'\r',
        't' => b'\t',
        '\\' => b'\\',
        '\'' => b'\'',
        'x' => return T::hex(hex_escape(chars)?),
        'u' => {
            let value = unicode_escape(chars)?;
            let c = if value > u32::from(char::MAX) {
                Err(EscapeError::OutOfRangeUnicodeEscape)
            } else {
                char::from_u32(value).ok_or(EscapeError::LoneSurrogateUnicodeEscape)
            };
            return T::unicode(c);
        }
        _ => return Err(EscapeError::InvalidEscape),
    };
    Ok(T::ascii(NonZero::new(simple).expect("a simple escape is not zero")))
}

/// The byte of `\x`'s two hex digits.
fn hex_escape(chars: &mut Chars<'_>) -> Result<u8, EscapeError> {
    let mut digit = || {
        let c = chars.next().ok_or(EscapeError::TooShortHexEscape)?;
        c.to_digit(16).ok_or(EscapeError::InvalidCharInHexEscape)
    };
    let (high, low) = (digit()?, digit()?);
    Ok(u8::try_from(high * 16 + low).expect("two hex digits are a byte"))
}

/// The number of `\u{..}`: up to six hex digits, `_` anywhere but first; digits past the sixth
/// are read (and refused at the `}`) but not added.
fn unicode_escape(chars: &mut Chars<'_>) -> Result<u32, EscapeError> {
    if chars.next() != Some('{') {
        return Err(EscapeError::NoBraceInUnicodeEscape);
    }
    let mut value = match chars.next().ok_or(EscapeError::UnclosedUnicodeEscape)? {
        '_' => return Err(EscapeError::LeadingUnderscoreUnicodeEscape),
        '}' => return Err(EscapeError::EmptyUnicodeEscape),
        c => c.to_digit(16).ok_or(EscapeError::InvalidCharInUnicodeEscape)?,
    };
    let mut digits = 1;
    loop {
        match chars.next().ok_or(EscapeError::UnclosedUnicodeEscape)? {
            '_' => {}
            '}' if digits > 6 => return Err(EscapeError::OverlongUnicodeEscape),
            '}' => return Ok(value),
            c => {
                let digit = c.to_digit(16).ok_or(EscapeError::InvalidCharInUnicodeEscape)?;
                digits += 1;
                if digits <= 6 {
                    value = value * 16 + digit;
                }
            }
        }
    }
}

/// Each value of a quoted string's text: a character, an escape, or a line continuation, which
/// yields no value, only its warnings.
///
/// `line_break` is [`line_break_len`], the one rule; a test passes a planted one.
fn quoted<T: Unit>(
    src: &str,
    line_break: fn(&[u8]) -> Option<usize>,
    mut callback: impl FnMut(Range<usize>, Result<T, EscapeError>),
) {
    let mut chars = src.chars();
    while let Some(c) = chars.next() {
        let start = src.len() - chars.as_str().len() - c.len_utf8();
        let value = match c {
            // `\` before a line break, LF or CR LF (crate::line_break): a continuation.
            '\\' if let Some(brk) = line_break(chars.as_str().as_bytes()) => {
                for _ in 0..brk {
                    chars.next();
                }
                skip_whitespace(&mut chars, start, 1 + brk, &mut callback);
                continue;
            }
            '\\' => escape(&mut chars),
            '"' => Err(EscapeError::EscapeOnlyChar),
            // A CR LF is one line break, whose value is LF.
            '\r' if chars.as_str().starts_with('\n') => {
                chars.next();
                T::char('\n')
            }
            '\r' => Err(EscapeError::BareCarriageReturn),
            c => T::char(c),
        };
        callback(start..src.len() - chars.as_str().len(), value);
    }
}

/// After `\` and a line break at `start`, `taken` bytes of both: skip ASCII whitespace, warning
/// when it spans another line break or stops at whitespace that is not ASCII.
fn skip_whitespace<T>(
    chars: &mut Chars<'_>,
    start: usize,
    taken: usize,
    callback: &mut impl FnMut(Range<usize>, Result<T, EscapeError>),
) {
    let rest = chars.as_str();
    let skipped = rest.bytes().position(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r')).unwrap_or(rest.len());
    let (space, rest) = rest.split_at(skipped);
    let end = start + taken + skipped;
    if space.contains('\n') {
        callback(start..end, Err(EscapeError::MultipleSkippedLinesWarning));
    }
    *chars = rest.chars();
    if let Some(c) = rest.chars().next()
        && c.is_whitespace()
    {
        callback(start..end + c.len_utf8(), Err(EscapeError::UnskippedWhitespaceWarning));
    }
}
