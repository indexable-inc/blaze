//! Literal values for the builtins: a string literal's value, `concat!`'s pieces, and the
//! escaped inside of a literal the expansion makes. Escapes are read here, by the rules of the
//! Reference's "Tokens" chapter (no crate of the tree reads them: the lexer leaves a literal's
//! text as written).

use core::fmt;

use blaze_own::{AVec, Alloc, Vec};

use crate::tokens::LitKind;

/// UTF-8 text built in an allocator the caller names.
pub(crate) struct Buf<'a>(pub(crate) AVec<'a, u8>);

impl<'a> Buf<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Buf<'a> {
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

impl fmt::Write for Buf<'_> {
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
pub(crate) fn bytes_piece(
    kind: LitKind,
    text: &str,
    suffix_len: u32,
    out: &mut AVec<'_, u8>,
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
            let mut refused = None;
            parser::unescape::check_raw_byte_str(text, |_, unit| match unit {
                Ok(byte) => out.push(byte),
                Err(error) => {
                    refused.get_or_insert(lit_error(error));
                }
            });
            refused.map_or(Ok(()), Err)
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

/// The value of a `b'…'` literal's inside (parser::unescape).
fn unescape_byte(body: &str) -> Result<u8, LitError> {
    parser::unescape::unescape_byte(body).map_err(lit_error)
}

/// The value of a `b"…"` literal's inside, appended (parser::unescape: its escapes, a `\`
/// before a line break, CR LF as LF, a bare CR refused). A warning (whitespace a continuation
/// leaves) is no refusal, as in rustc.
fn unescape_bytes(body: &str, out: &mut AVec<'_, u8>) -> Result<(), LitError> {
    let mut refused = None;
    parser::unescape::unescape_byte_str(body, |_, unit| match unit {
        Ok(byte) => out.push(byte),
        Err(error) if error.is_fatal() => {
            refused.get_or_insert(lit_error(error));
        }
        Err(_) => {}
    });
    refused.map_or(Ok(()), Err)
}

/// The value of a string literal (`"…"` or `r#"…"#`), whose tt text is `text`, appended.
pub(crate) fn string_value(
    kind: LitKind,
    text: &str,
    suffix_len: u32,
    out: &mut Buf<'_>,
) -> Result<(), LitError> {
    let body = without_suffix(text, suffix_len);
    match kind {
        LitKind::Str => unescape_str(body, out),
        LitKind::StrRaw(_) => raw_str(body, out),
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
pub(crate) fn concat_piece(
    kind: LitKind,
    text: &str,
    suffix_len: u32,
    negative: bool,
    out: &mut Buf<'_>,
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

/// The value of a `"…"` literal's inside, appended (parser::unescape, by the one line-break
/// rule of parser::line_break).
fn unescape_str(body: &str, out: &mut Buf<'_>) -> Result<(), LitError> {
    let mut refused = None;
    parser::unescape::unescape_str(body, |_, unit| match unit {
        Ok(char) => out.push_char(char),
        Err(error) if error.is_fatal() => {
            refused.get_or_insert(lit_error(error));
        }
        Err(_) => {}
    });
    refused.map_or(Ok(()), Err)
}

/// The value of a raw string's inside, appended: each CR LF one LF, a bare CR refused
/// (parser::unescape).
fn raw_str(body: &str, out: &mut Buf<'_>) -> Result<(), LitError> {
    let mut refused = None;
    parser::unescape::check_raw_str(body, |_, unit| match unit {
        Ok(char) => out.push_char(char),
        Err(error) => {
            refused.get_or_insert(lit_error(error));
        }
    });
    refused.map_or(Ok(()), Err)
}

/// The parser's escape error as a `LitError`: a non-ASCII character in a byte literal does
/// not lex; any other is an escape that is not valid.
fn lit_error(error: parser::unescape::EscapeError) -> LitError {
    match error {
        parser::unescape::EscapeError::NonAsciiCharInByte => LitError::Invalid,
        _ => LitError::BadEscape,
    }
}

/// The value of a `'…'` literal's inside (parser::unescape).
fn unescape_char(body: &str) -> Result<char, LitError> {
    parser::unescape::unescape_char(body).map_err(lit_error)
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
pub(crate) fn escape_str(value: &str, out: &mut Buf<'_>) {
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
pub(crate) fn escape_bytes(bytes: &[u8], out: &mut Buf<'_>) {
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
