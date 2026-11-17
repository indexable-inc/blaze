//! A literal's value from its token's text (the Reference, "Tokens": "Literals"). The body form
//! keeps only a literal's kind and suffix (blaze-lower's lit.rs: "the value itself stays the
//! token's text, read by whoever needs it"); the text is read here at the expression's
//! item-relative range. Owed: this module moves to blaze-lower's lit.rs, the one home of
//! literal syntax, when that crate's owner takes it.

use blaze_own::{Alloc, Vec};

/// Why a literal's text gave no value. The checker has accepted the token, so each is a
/// mismatch between the text and the body (a wrong range), or a form not built here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LitError {
    /// The text is not a literal of the kind the body says.
    NotThatKind,
    /// An integer literal past `u128` (the Reference: an error at the literal).
    TooLarge,
    /// A NUL inside a C string literal (the Reference, "Tokens", "C string and raw C string
    /// literals": "it is an error for a C string literal to contain a NUL character", written
    /// or escaped). `at` is the byte offset of the NUL or its escape in the token's text.
    InteriorNul { at: u32 },
}

/// The magnitude of an integer literal: decimal, `0x`, `0o`, `0b`, `_` separators, an optional
/// suffix; or a byte literal `b'x'` (a `u8`).
pub(crate) fn int(text: &str) -> Result<u128, LitError> {
    if let Some(rest) = text.strip_prefix("b'") {
        let inner = rest.strip_suffix('\'').ok_or(LitError::NotThatKind)?;
        let (c, used) = unescape_one(inner.as_bytes(), true)?;
        if used != inner.len() {
            return Err(LitError::NotThatKind);
        }
        return Ok(u128::from(c));
    }
    let bytes = text.as_bytes();
    let (radix, digits) = match bytes {
        [b'0', b'x', rest @ ..] => (16, rest),
        [b'0', b'o', rest @ ..] => (8, rest),
        [b'0', b'b', rest @ ..] => (2, rest),
        _ => (10, bytes),
    };
    let mut value: u128 = 0;
    let mut any = false;
    for &b in digits {
        if b == b'_' {
            continue;
        }
        let d = match b {
            b'0'..=b'9' => u32::from(b - b'0'),
            b'a'..=b'f' if radix == 16 => u32::from(b - b'a') + 10,
            b'A'..=b'F' if radix == 16 => u32::from(b - b'A') + 10,
            // The suffix (`u8`, `i32`, ...) starts with a letter no digit of the radix is.
            b'i' | b'u' => break,
            // A float suffix on a decimal integer (`1f32`, `0f128`); in hex `f` is a digit.
            b'f' if radix == 10 => break,
            _ => return Err(LitError::NotThatKind),
        };
        if d >= radix {
            return Err(LitError::NotThatKind);
        }
        value = value.checked_mul(u128::from(radix)).and_then(|v| v.checked_add(u128::from(d))).ok_or(LitError::TooLarge)?;
        any = true;
    }
    if any { Ok(value) } else { Err(LitError::NotThatKind) }
}

/// A float literal's value at `f32` (`wide == false`) or `f64`, rounded to nearest as the
/// Reference requires (core's parser, which is exact).
pub(crate) fn float(text: &str, wide: bool) -> Result<u64, LitError> {
    let mut buf = [0u8; 128];
    let mut n = 0;
    for &b in text.as_bytes() {
        match b {
            b'_' => continue,
            b'f' => break,
            _ => {
                *buf.get_mut(n).ok_or(LitError::TooLarge)? = b;
                n += 1;
            }
        }
    }
    let s = core::str::from_utf8(&buf[..n]).map_err(|_| LitError::NotThatKind)?;
    if wide {
        s.parse::<f64>().map(f64::to_bits).map_err(|_| LitError::NotThatKind)
    } else {
        s.parse::<f32>().map(|f| u64::from(f.to_bits())).map_err(|_| LitError::NotThatKind)
    }
}

/// A character literal's scalar value.
pub(crate) fn char(text: &str) -> Result<u32, LitError> {
    let inner = text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')).ok_or(LitError::NotThatKind)?;
    let (c, used) = unescape_one(inner.as_bytes(), false)?;
    if used != inner.len() {
        return Err(LitError::NotThatKind);
    }
    Ok(c)
}

/// The bytes of a C string literal (`c"..."`, `cr#"..."#`) and the NUL that ends them: the
/// Reference, "Tokens", "C string and raw C string literals": `\xNN` is that byte (any of
/// 0x01..=0xFF), `\u{..}` and every other character its UTF-8, and no NUL may be inside.
pub(crate) fn c_bytes<'a>(text: &str, alloc: Alloc<'a>) -> Result<Vec<u8, Alloc<'a>>, LitError> {
    let rest = text.strip_prefix('c').ok_or(LitError::NotThatKind)?;
    let mut out = Vec::new_in(alloc);
    if let Some(raw) = rest.strip_prefix('r') {
        let hashes = raw.bytes().take_while(|&b| b == b'#').count();
        let body = raw.get(hashes..raw.len().saturating_sub(hashes)).ok_or(LitError::NotThatKind)?;
        let body = body.strip_prefix('"').and_then(|b| b.strip_suffix('"')).ok_or(LitError::NotThatKind)?;
        // The text before the body: `c`, `r`, the hashes and the quote.
        let start = 3 + hashes;
        if let Some(i) = body.bytes().position(|b| b == 0) {
            return Err(LitError::InteriorNul { at: (start + i) as u32 });
        }
        out.extend_from_slice(body.as_bytes());
        out.push(0);
        return Ok(out);
    }
    let body = rest.strip_prefix('"').and_then(|b| b.strip_suffix('"')).ok_or(LitError::NotThatKind)?.as_bytes();
    // The text before the body: `c` and the quote.
    let start = 2;
    let mut at = 0;
    while at < body.len() {
        if body[at] == b'\\' && body.get(at + 1) == Some(&b'\n') {
            at += 2;
            while at < body.len() && matches!(body[at], b' ' | b'\t' | b'\n' | b'\r') {
                at += 1;
            }
            continue;
        }
        let here = at;
        if body[at] == b'\\' && body.get(at + 1) == Some(&b'x') {
            // A `\x` escape is one byte, past ASCII too.
            let (c, used) = unescape_one(&body[at..], true)?;
            at += used;
            if c == 0 {
                return Err(LitError::InteriorNul { at: (start + here) as u32 });
            }
            out.push(u8::try_from(c).map_err(|_| LitError::NotThatKind)?);
        } else if body[at] == b'\\' {
            let (c, used) = unescape_one(&body[at..], false)?;
            at += used;
            if c == 0 {
                return Err(LitError::InteriorNul { at: (start + here) as u32 });
            }
            let ch = core::char::from_u32(c).ok_or(LitError::NotThatKind)?;
            let mut b = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
        } else {
            if body[at] == 0 {
                return Err(LitError::InteriorNul { at: (start + here) as u32 });
            }
            out.push(body[at]);
            at += 1;
        }
    }
    out.push(0);
    Ok(out)
}

/// The bytes of a string or byte string literal (`"..."`, `r#"..."#`, `b"..."`, `br"..."`):
/// UTF-8 for a string.
pub(crate) fn bytes<'a>(text: &str, alloc: Alloc<'a>) -> Result<Vec<u8, Alloc<'a>>, LitError> {
    let mut out = Vec::new_in(alloc);
    let (byte, rest) = match text.strip_prefix('b') {
        Some(r) => (true, r),
        None => (false, text),
    };
    if let Some(raw) = rest.strip_prefix('r') {
        let hashes = raw.bytes().take_while(|&b| b == b'#').count();
        let body = raw.get(hashes..raw.len().saturating_sub(hashes)).ok_or(LitError::NotThatKind)?;
        let body = body.strip_prefix('"').and_then(|b| b.strip_suffix('"')).ok_or(LitError::NotThatKind)?;
        // A raw string's `\r\n` is kept as written: rustc refuses a bare CR in one.
        out.extend_from_slice(body.as_bytes());
        return Ok(out);
    }
    let body = rest.strip_prefix('"').and_then(|b| b.strip_suffix('"')).ok_or(LitError::NotThatKind)?.as_bytes();
    let mut at = 0;
    while at < body.len() {
        if body[at] == b'\\' && body.get(at + 1) == Some(&b'\n') {
            // A line continuation: the newline and the whitespace after it are skipped.
            at += 2;
            while at < body.len() && matches!(body[at], b' ' | b'\t' | b'\n' | b'\r') {
                at += 1;
            }
            continue;
        }
        if body[at] == b'\\' {
            let (c, used) = unescape_one(&body[at..], byte)?;
            at += used;
            if byte {
                out.push(u8::try_from(c).map_err(|_| LitError::NotThatKind)?);
            } else {
                let ch = core::char::from_u32(c).ok_or(LitError::NotThatKind)?;
                let mut b = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
            }
        } else {
            out.push(body[at]);
            at += 1;
        }
    }
    Ok(out)
}

/// One character or escape at the start of `s`: its value and the bytes it used. `byte`: a
/// byte literal's escapes (`\xFF` allowed, `\u{..}` not).
fn unescape_one(s: &[u8], byte: bool) -> Result<(u32, usize), LitError> {
    let first = *s.first().ok_or(LitError::NotThatKind)?;
    if first != b'\\' {
        if byte {
            return Ok((u32::from(first), 1));
        }
        let len = match first {
            0..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            _ => 4,
        };
        let text = core::str::from_utf8(s.get(..len).ok_or(LitError::NotThatKind)?).map_err(|_| LitError::NotThatKind)?;
        let c = text.chars().next().ok_or(LitError::NotThatKind)?;
        return Ok((c as u32, len));
    }
    let kind = *s.get(1).ok_or(LitError::NotThatKind)?;
    Ok(match kind {
        b'n' => (u32::from(b'\n'), 2),
        b'r' => (u32::from(b'\r'), 2),
        b't' => (u32::from(b'\t'), 2),
        b'\\' => (u32::from(b'\\'), 2),
        b'0' => (0, 2),
        b'\'' => (u32::from(b'\''), 2),
        b'"' => (u32::from(b'"'), 2),
        b'x' => {
            let hex = core::str::from_utf8(s.get(2..4).ok_or(LitError::NotThatKind)?).map_err(|_| LitError::NotThatKind)?;
            let v = u32::from_str_radix(hex, 16).map_err(|_| LitError::NotThatKind)?;
            if !byte && v > 0x7f {
                return Err(LitError::NotThatKind);
            }
            (v, 4)
        }
        b'u' if !byte => {
            let close = s.iter().position(|&b| b == b'}').ok_or(LitError::NotThatKind)?;
            let mut v: u32 = 0;
            for &b in s.get(3..close).ok_or(LitError::NotThatKind)? {
                if b == b'_' {
                    continue;
                }
                let d = char::from(b).to_digit(16).ok_or(LitError::NotThatKind)?;
                v = v.checked_mul(16).and_then(|v| v.checked_add(d)).ok_or(LitError::NotThatKind)?;
            }
            (v, close + 1)
        }
        _ => return Err(LitError::NotThatKind),
    })
}
