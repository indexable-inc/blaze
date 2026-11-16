//! Literal values for the builtins: a string literal's value, `concat!`'s pieces, and the
//! escaped inside of a literal the expansion makes. Escapes are read here, by the rules of the
//! Reference's "Tokens" chapter (no crate of the tree reads them: the lexer leaves a literal's
//! text as written).

use core::alloc::Allocator;
use core::fmt;

use alloc::vec::Vec;

use crate::tokens::LitKind;

/// UTF-8 text built in `A`.
pub(crate) struct Buf<A: Allocator>(pub(crate) Vec<u8, A>);

impl<A: Allocator> Buf<A> {
    pub(crate) fn new_in(alloc: A) -> Buf<A> {
        Buf(Vec::new_in(alloc))
    }

    pub(crate) fn push_str(&mut self, text: &str) {
        self.0.extend_from_slice(text.as_bytes());
    }

    pub(crate) fn push_char(&mut self, char: char) {
        let mut bytes = [0; 4];
        self.push_str(char.encode_utf8(&mut bytes));
    }

    pub(crate) fn as_str(&self) -> &str {
        core::str::from_utf8(&self.0).expect("a `Buf` is pushed only whole `str`s and `char`s")
    }
}

impl<A: Allocator> fmt::Write for Buf<A> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.push_str(text);
        Ok(())
    }
}

/// Why a literal is not the value a builtin takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LitError {
    NotAString,
    BadEscape,
    ByteLiteral,
    CStr,
    IntegerTooLarge,
    Invalid,
    /// `concat_bytes!` of a character or string literal.
    CharOrStr,
    /// `concat_bytes!` of a numeric literal outside an array.
    NumberAlone,
    /// A `concat_bytes!` array element that is a numeric literal but not a `u8`.
    NotAU8,
    /// `concat_bytes!` of a float or C string literal.
    NotBytes,
}

impl LitError {
    pub(crate) fn message(self) -> &'static str {
        match self {
            LitError::NotAString => "expected a string literal",
            LitError::BadEscape => "the literal holds an escape that is not valid",
            LitError::ByteLiteral => "cannot concatenate a byte string literal",
            LitError::CStr => "cannot concatenate a C string literal",
            LitError::IntegerTooLarge => "the integer literal is too large",
            LitError::Invalid => "the literal does not lex",
            LitError::CharOrStr => {
                "cannot concatenate a character or string literal into bytes: write a byte literal"
            }
            LitError::NumberAlone => {
                "a numeric literal is concatenated into bytes only inside an array: write `[n]`"
            }
            LitError::NotAU8 => "the numeric literal is not a `u8`",
            LitError::NotBytes => "cannot concatenate this literal into bytes",
        }
    }
}

/// `concat_bytes!`'s bytes of one literal outside an array, appended: a byte or a byte string.
pub(crate) fn bytes_piece<B: Allocator>(
    kind: LitKind,
    text: &str,
    suffix_len: u32,
    out: &mut Vec<u8, B>,
) -> Result<(), LitError> {
    if suffix_len != 0 && !matches!(kind, LitKind::Integer | LitKind::Float) {
        return Err(LitError::Invalid);
    }
    match kind {
        LitKind::Byte => {
            out.push(unescape_byte(text)?);
            Ok(())
        }
        LitKind::ByteStr => unescape_bytes(text, out),
        LitKind::ByteStrRaw(_) => {
            if !text.is_ascii() {
                return Err(LitError::Invalid);
            }
            out.extend_from_slice(text.as_bytes());
            Ok(())
        }
        LitKind::Char | LitKind::Str | LitKind::StrRaw(_) => Err(LitError::CharOrStr),
        LitKind::Integer => Err(LitError::NumberAlone),
        LitKind::Float | LitKind::CStr | LitKind::CStrRaw(_) => Err(LitError::NotBytes),
        LitKind::Err => Err(LitError::Invalid),
    }
}

/// One element of a `concat_bytes!` array: a byte literal, or an integer literal up to 255,
/// unsuffixed or `u8`.
pub(crate) fn byte_element(kind: LitKind, text: &str, suffix_len: u32) -> Result<u8, LitError> {
    match kind {
        LitKind::Byte if suffix_len == 0 => unescape_byte(text),
        LitKind::Integer => {
            let suffix = &text[text.len() - suffix_len as usize..];
            if !matches!(suffix, "" | "u8") {
                return Err(LitError::NotAU8);
            }
            let value = integer_value(without_suffix(text, suffix_len))?;
            u8::try_from(value).map_err(|_| LitError::NotAU8)
        }
        LitKind::Char | LitKind::Str | LitKind::StrRaw(_) => Err(LitError::CharOrStr),
        LitKind::Byte
        | LitKind::ByteStr
        | LitKind::ByteStrRaw(_)
        | LitKind::Float
        | LitKind::CStr
        | LitKind::CStrRaw(_) => Err(LitError::NotBytes),
        LitKind::Err => Err(LitError::Invalid),
    }
}

/// An integer literal's value, its suffix whatever it is; `None` when it does not read.
pub(crate) fn integer_of(text: &str, suffix_len: u32) -> Option<u128> {
    integer_value(without_suffix(text, suffix_len)).ok()
}

/// The value of a `b'…'` literal's inside: one ASCII character, or one byte escape.
fn unescape_byte(body: &str) -> Result<u8, LitError> {
    let (byte, len) = match body.as_bytes().first() {
        Some(b'\\') => {
            let (byte, len) = byte_escape(&body.as_bytes()[1..])?;
            (byte, 1 + len)
        }
        Some(b'\'' | b'\n' | b'\r' | b'\t') | None => return Err(LitError::BadEscape),
        Some(&byte) if byte.is_ascii() => (byte, 1),
        Some(_) => return Err(LitError::Invalid),
    };
    if len == body.len() { Ok(byte) } else { Err(LitError::BadEscape) }
}

/// The value of a `b"…"` literal's inside, appended, read as [`unescape_str`] reads a string's
/// but in bytes: ASCII only, `\x` up to `ff`, no `\u`.
fn unescape_bytes<B: Allocator>(body: &str, out: &mut Vec<u8, B>) -> Result<(), LitError> {
    let bytes = body.as_bytes();
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'\\' if matches!(bytes.get(at + 1), Some(b'\n' | b'\r')) => {
                at += 1;
                if bytes[at] == b'\r' && bytes.get(at + 1) != Some(&b'\n') {
                    return Err(LitError::BadEscape);
                }
                while bytes.get(at).is_some_and(|it| matches!(it, b' ' | b'\t' | b'\n' | b'\r')) {
                    at += 1;
                }
            }
            b'\\' => {
                let (value, len) = byte_escape(&bytes[at + 1..])?;
                out.push(value);
                at += 1 + len;
            }
            b'\r' if bytes.get(at + 1) == Some(&b'\n') => {
                out.push(b'\n');
                at += 2;
            }
            b'\r' => return Err(LitError::BadEscape),
            byte if byte.is_ascii() => {
                out.push(byte);
                at += 1;
            }
            _ => return Err(LitError::Invalid),
        }
    }
    Ok(())
}

/// The byte escape at the start of `rest`, the bytes after a `\`: its value and length.
fn byte_escape(rest: &[u8]) -> Result<(u8, usize), LitError> {
    let simple = match rest.first() {
        Some(b'n') => Some(b'\n'),
        Some(b'r') => Some(b'\r'),
        Some(b't') => Some(b'\t'),
        Some(b'\\') => Some(b'\\'),
        Some(b'0') => Some(0),
        Some(b'\'') => Some(b'\''),
        Some(b'"') => Some(b'"'),
        _ => None,
    };
    if let Some(byte) = simple {
        return Ok((byte, 1));
    }
    let hex = |byte: u8| char::from(byte).to_digit(16);
    match rest.first() {
        Some(b'x') => {
            let high = rest.get(1).copied().and_then(hex).ok_or(LitError::BadEscape)?;
            let low = rest.get(2).copied().and_then(hex).ok_or(LitError::BadEscape)?;
            let value = u8::try_from(high * 16 + low).map_err(|_| LitError::BadEscape)?;
            Ok((value, 3))
        }
        _ => Err(LitError::BadEscape),
    }
}

/// The value of a string literal (`"…"` or `r#"…"#`), whose tt text is `text`, appended.
pub(crate) fn string_value<A: Allocator>(
    kind: LitKind,
    text: &str,
    suffix_len: u32,
    out: &mut Buf<A>,
) -> Result<(), LitError> {
    let body = without_suffix(text, suffix_len);
    match kind {
        LitKind::Str => unescape_str(body, out),
        LitKind::StrRaw(_) => {
            out.push_str(body);
            Ok(())
        }
        LitKind::Byte
        | LitKind::Char
        | LitKind::Integer
        | LitKind::Float
        | LitKind::ByteStr
        | LitKind::ByteStrRaw(_)
        | LitKind::CStr
        | LitKind::CStrRaw(_)
        | LitKind::Err => Err(LitError::NotAString),
    }
}

/// `concat!`'s text of one literal (with a `-` before it when `negative`), appended.
pub(crate) fn concat_piece<A: Allocator>(
    kind: LitKind,
    text: &str,
    suffix_len: u32,
    negative: bool,
    out: &mut Buf<A>,
) -> Result<(), LitError> {
    let body = without_suffix(text, suffix_len);
    match kind {
        LitKind::Str | LitKind::StrRaw(_) => string_value(kind, text, suffix_len, out),
        LitKind::Char => {
            let char = unescape_char(body)?;
            out.push_char(char);
            Ok(())
        }
        LitKind::Integer => {
            let value = integer_value(body)?;
            if negative {
                out.push_str("-");
            }
            fmt::Write::write_fmt(out, format_args!("{value}")).expect("a `Buf` takes any text");
            Ok(())
        }
        LitKind::Float => {
            if negative {
                out.push_str("-");
            }
            out.push_str(body);
            Ok(())
        }
        LitKind::Byte | LitKind::ByteStr | LitKind::ByteStrRaw(_) => Err(LitError::ByteLiteral),
        LitKind::CStr | LitKind::CStrRaw(_) => Err(LitError::CStr),
        LitKind::Err => Err(LitError::Invalid),
    }
}

fn without_suffix(text: &str, suffix_len: u32) -> &str {
    &text[..text.len() - suffix_len as usize]
}

/// The value of a `"…"` literal's inside, appended: its escapes read, a `\` before a line
/// break skipping the break and the ASCII whitespace after it, and `\r\n` read as `\n` (rustc
/// reads a file's CRLFs as LFs); a lone `\r` is refused, as rustc's lexer refuses it.
fn unescape_str<A: Allocator>(body: &str, out: &mut Buf<A>) -> Result<(), LitError> {
    let bytes = body.as_bytes();
    let mut at = 0;
    while let Some(char) = body[at..].chars().next() {
        match char {
            '\\' if matches!(bytes.get(at + 1), Some(b'\n' | b'\r')) => {
                at += 1;
                if bytes[at] == b'\r' && bytes.get(at + 1) != Some(&b'\n') {
                    return Err(LitError::BadEscape);
                }
                while bytes.get(at).is_some_and(|it| matches!(it, b' ' | b'\t' | b'\n' | b'\r')) {
                    at += 1;
                }
            }
            '\\' => {
                let (char, len) = escape(&body[at + 1..])?;
                out.push_char(char);
                at += 1 + len;
            }
            '\r' if bytes.get(at + 1) == Some(&b'\n') => {
                out.push_char('\n');
                at += 2;
            }
            '\r' => return Err(LitError::BadEscape),
            char => {
                out.push_char(char);
                at += char.len_utf8();
            }
        }
    }
    Ok(())
}

/// The value of a `'…'` literal's inside: one character, or one escape.
fn unescape_char(body: &str) -> Result<char, LitError> {
    let (char, len) = match body.chars().next() {
        Some('\\') => {
            let (char, len) = escape(&body[1..])?;
            (char, 1 + len)
        }
        Some('\'' | '\n' | '\r' | '\t') | None => return Err(LitError::BadEscape),
        Some(char) => (char, char.len_utf8()),
    };
    if len == body.len() { Ok(char) } else { Err(LitError::BadEscape) }
}

/// The escape at the start of `rest`, the text after a `\`: its character, and how many bytes
/// of `rest` it is. `\x` takes two hex digits up to `7f`; `\u{…}` one to six hex digits
/// (`_` after the first) naming a scalar value.
fn escape(rest: &str) -> Result<(char, usize), LitError> {
    let bytes = rest.as_bytes();
    let simple = match bytes.first() {
        Some(b'n') => Some('\n'),
        Some(b'r') => Some('\r'),
        Some(b't') => Some('\t'),
        Some(b'\\') => Some('\\'),
        Some(b'0') => Some('\0'),
        Some(b'\'') => Some('\''),
        Some(b'"') => Some('"'),
        _ => None,
    };
    if let Some(char) = simple {
        return Ok((char, 1));
    }
    let hex = |byte: u8| char::from(byte).to_digit(16);
    match bytes.first() {
        Some(b'x') => {
            let high = bytes.get(1).copied().and_then(hex).ok_or(LitError::BadEscape)?;
            let low = bytes.get(2).copied().and_then(hex).ok_or(LitError::BadEscape)?;
            let value = high * 16 + low;
            if value > 0x7f {
                return Err(LitError::BadEscape);
            }
            let char = char::from_u32(value).ok_or(LitError::BadEscape)?;
            Ok((char, 3))
        }
        Some(b'u') => {
            if bytes.get(1) != Some(&b'{') {
                return Err(LitError::BadEscape);
            }
            let (mut value, mut digits, mut at) = (0u32, 0u32, 2);
            loop {
                match bytes.get(at) {
                    Some(b'}') if digits > 0 => break,
                    Some(b'_') if digits > 0 => {}
                    Some(&byte) => {
                        let digit = hex(byte).ok_or(LitError::BadEscape)?;
                        digits += 1;
                        if digits > 6 {
                            return Err(LitError::BadEscape);
                        }
                        value = value * 16 + digit;
                    }
                    None => return Err(LitError::BadEscape),
                }
                at += 1;
            }
            let char = char::from_u32(value).ok_or(LitError::BadEscape)?;
            Ok((char, at + 1))
        }
        _ => Err(LitError::BadEscape),
    }
}

/// An integer literal's value: its radix prefix and `_`s read.
fn integer_value(body: &str) -> Result<u128, LitError> {
    let (radix, digits) = match body.get(..2) {
        Some("0x") => (16, &body[2..]),
        Some("0o") => (8, &body[2..]),
        Some("0b") => (2, &body[2..]),
        _ => (10, body),
    };
    let mut value: u128 = 0;
    let mut any = false;
    for char in digits.chars() {
        if char == '_' {
            continue;
        }
        let digit = char.to_digit(radix).ok_or(LitError::Invalid)?;
        value = value
            .checked_mul(u128::from(radix))
            .and_then(|it| it.checked_add(u128::from(digit)))
            .ok_or(LitError::IntegerTooLarge)?;
        any = true;
    }
    if any { Ok(value) } else { Err(LitError::Invalid) }
}

/// The inside of a `"…"` literal whose value is `value`, appended: `"`, `\` and the
/// characters a string literal cannot hold as they are written as escapes.
pub(crate) fn escape_str<A: Allocator>(value: &str, out: &mut Buf<A>) {
    for char in value.chars() {
        match char {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            char if char.is_control() => {
                fmt::Write::write_fmt(out, format_args!("\\u{{{:x}}}", u32::from(char)))
                    .expect("a `Buf` takes any text");
            }
            char => out.push_char(char),
        }
    }
}

/// The inside of a `b"…"` literal whose value is `bytes`, appended.
pub(crate) fn escape_bytes<A: Allocator>(bytes: &[u8], out: &mut Buf<A>) {
    for &byte in bytes {
        match byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push_char(char::from(byte)),
            _ => fmt::Write::write_fmt(out, format_args!("\\x{byte:02x}"))
                .expect("a `Buf` takes any text"),
        }
    }
}
