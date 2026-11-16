//! Literal values for the builtins: a string literal's value, `concat!`'s pieces, and the
//! escaped inside of a literal the expansion makes. Escapes are read by rustc's own reader,
//! `rustc-literal-escaper`.

use core::fmt;
use std::alloc::Allocator;

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
        }
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
            let char = rustc_literal_escaper::unescape_char(body).map_err(|_| LitError::BadEscape)?;
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

fn unescape_str<A: Allocator>(body: &str, out: &mut Buf<A>) -> Result<(), LitError> {
    let mut failed = false;
    rustc_literal_escaper::unescape_str(body, |_, char| match char {
        Ok(char) => out.push_char(char),
        Err(error) if error.is_fatal() => failed = true,
        // A warning (a `\` line continuation's trailing space): the value stands.
        Err(_) => {}
    });
    if failed { Err(LitError::BadEscape) } else { Ok(()) }
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
