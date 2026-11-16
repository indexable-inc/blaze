//! A literal token's form: its kind, its suffix's type, a byte string's length. The value
//! itself stays the token's text, read by whoever needs it (const evaluation, codegen).

use parser::SyntaxKind::{self, *};

use crate::form::{FloatTy, IntSuffix, IntTy, Lit, UintTy};

/// The literal of a token of `kind` with `text`; `None` for a suffix rustc refuses.
pub(crate) fn lit_of(kind: SyntaxKind, text: &str) -> Option<Lit> {
    Some(match kind {
        TRUE_KW => Lit::Bool(true),
        FALSE_KW => Lit::Bool(false),
        CHAR => Lit::Char,
        BYTE => Lit::Byte,
        STRING => Lit::Str,
        BYTE_STRING => Lit::ByteStr(byte_string_len(text)?),
        C_STRING => Lit::CStr,
        INT_NUMBER => int(text)?,
        FLOAT_NUMBER => Lit::Float(float_suffix(text)?),
        _ => return None,
    })
}

fn int_suffix(s: &str) -> Option<IntSuffix> {
    Some(match s {
        "i8" => IntSuffix::Int(IntTy::I8),
        "i16" => IntSuffix::Int(IntTy::I16),
        "i32" => IntSuffix::Int(IntTy::I32),
        "i64" => IntSuffix::Int(IntTy::I64),
        "i128" => IntSuffix::Int(IntTy::I128),
        "isize" => IntSuffix::Int(IntTy::Isize),
        "u8" => IntSuffix::Uint(UintTy::U8),
        "u16" => IntSuffix::Uint(UintTy::U16),
        "u32" => IntSuffix::Uint(UintTy::U32),
        "u64" => IntSuffix::Uint(UintTy::U64),
        "u128" => IntSuffix::Uint(UintTy::U128),
        "usize" => IntSuffix::Uint(UintTy::Usize),
        _ => return None,
    })
}

fn float_ty(s: &str) -> Option<FloatTy> {
    Some(match s {
        "f16" => FloatTy::F16,
        "f32" => FloatTy::F32,
        "f64" => FloatTy::F64,
        "f128" => FloatTy::F128,
        _ => return None,
    })
}

/// An integer token: `1u8`, `0xffi32`, `0b1_0`; a decimal one with a float suffix (`1f32`) is a
/// float, as rustc reads it.
fn int(text: &str) -> Option<Lit> {
    let (digits, hex) = match text.get(..2) {
        Some("0x" | "0X") => (&text[2..], true),
        Some("0o" | "0O" | "0b" | "0B") => (&text[2..], false),
        _ => (text, false),
    };
    let at = digits
        .char_indices()
        .find(|&(_, c)| if hex { c == 'i' || c == 'u' } else { c.is_alphabetic() })
        .map_or(digits.len(), |(i, _)| i);
    let suffix = &digits[at..];
    if suffix.is_empty() {
        return Some(Lit::Int(None));
    }
    if let Some(s) = int_suffix(suffix) {
        return Some(Lit::Int(Some(s)));
    }
    let decimal = digits.len() == text.len();
    match float_ty(suffix) {
        Some(f) if decimal => Some(Lit::Float(Some(f))),
        _ => None,
    }
}

fn float_suffix(text: &str) -> Option<Option<FloatTy>> {
    // A float's suffix starts at its `f`: no digit or exponent of a float is one.
    match text.rfind('f') {
        None => Some(None),
        Some(at) => float_ty(&text[at..]).map(Some),
    }
}

/// The bytes a byte string literal makes: its escapes as the bytes they stand for, a line
/// continuation as none, `\r\n` as one (rustc reads a file's `\r\n` as `\n`).
fn byte_string_len(text: &str) -> Option<u64> {
    let body = text.strip_prefix('b')?;
    if let Some(raw) = body.strip_prefix('r') {
        let hashes = raw.len() - raw.trim_start_matches('#').len();
        let inner = raw.get(hashes + 1..raw.len().checked_sub(hashes + 1)?)?;
        return Some(crlf_len(inner));
    }
    let inner = body.strip_prefix('"')?.strip_suffix('"')?;
    let bytes = inner.as_bytes();
    let mut len = 0u64;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => match bytes.get(i + 1)? {
                b'x' => i += 4,
                b'\n' | b'\r' => {
                    i += 1;
                    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
                        i += 1;
                    }
                    continue;
                }
                _ => i += 2,
            },
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => i += 2,
            _ => i += 1,
        }
        len += 1;
    }
    Some(len)
}

fn crlf_len(s: &str) -> u64 {
    (s.len() - s.matches("\r\n").count()) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes_and_lengths() {
        assert_eq!(lit_of(INT_NUMBER, "1"), Some(Lit::Int(None)));
        assert_eq!(lit_of(INT_NUMBER, "0xffu8"), Some(Lit::Int(Some(IntSuffix::Uint(UintTy::U8)))));
        assert_eq!(lit_of(INT_NUMBER, "0x1f32"), Some(Lit::Int(None)));
        assert_eq!(lit_of(INT_NUMBER, "1f32"), Some(Lit::Float(Some(FloatTy::F32))));
        assert_eq!(lit_of(INT_NUMBER, "1_000_usize"), Some(Lit::Int(Some(IntSuffix::Uint(UintTy::Usize)))));
        assert_eq!(lit_of(FLOAT_NUMBER, "1e10f64"), Some(Lit::Float(Some(FloatTy::F64))));
        assert_eq!(lit_of(FLOAT_NUMBER, "2.5"), Some(Lit::Float(None)));
        assert_eq!(lit_of(INT_NUMBER, "1foo"), None);
        assert_eq!(lit_of(BYTE_STRING, r#"b"a\x00\n""#), Some(Lit::ByteStr(3)));
        assert_eq!(lit_of(BYTE_STRING, "b\"a\\\n   b\""), Some(Lit::ByteStr(2)));
        assert_eq!(lit_of(BYTE_STRING, r##"br#"a"b"#"##), Some(Lit::ByteStr(3)));
    }
}
