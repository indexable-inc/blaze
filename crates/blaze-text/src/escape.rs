//! Text escaped for a quoted literal: Rust's string-literal form and JSON's. Each writes the
//! body only; the writers ([`Writes::escaped`](crate::Writes::escaped),
//! [`Writes::json_str`](crate::Writes::json_str)) add the quotes. A run of bytes that need no
//! escape is written as one `put_str`.

use crate::ascii::LOWER;
use crate::int::{Digits, radix_pow2_into};
use crate::put::Put;
use crate::simd;

/// The escape of one byte, or `None` to copy it.
type Rule = fn(u8) -> Option<&'static str>;

/// Copies the runs between escape bytes whole; the next escape byte is found sixteen at a
/// time ([`simd::next_special`]). Both escapes rewrite a subset of `"`, `\`, below 0x20 and
/// DEL (JSON copies DEL back through `other_control`).
#[inline]
fn runs<P: Put + ?Sized>(text: &str, rule: Rule, other_control: fn(u8, &mut P), out: &mut P) {
    let bytes = text.as_bytes();
    let mut start = 0;
    loop {
        let i = simd::next_special(bytes, start);
        if i == bytes.len() {
            break;
        }
        // Escaped bytes are ASCII, so `start..i` sits on character boundaries.
        out.put_str(&text[start..i]);
        match rule(bytes[i]) {
            Some(escape) => out.put_str(escape),
            None => other_control(bytes[i], out),
        }
        start = i + 1;
    }
    out.put_str(&text[start..]);
}

fn rust_rule(b: u8) -> Option<&'static str> {
    match b {
        b'"' => Some("\\\""),
        b'\\' => Some("\\\\"),
        b'\n' => Some("\\n"),
        b'\r' => Some("\\r"),
        b'\t' => Some("\\t"),
        0 => Some("\\0"),
        _ => None,
    }
}

fn rust_control<P: Put + ?Sized>(b: u8, out: &mut P) {
    out.put_str("\\u{");
    let mut d = Digits::at_end();
    radix_pow2_into(u128::from(b), 4, &LOWER, 1, &mut d);
    out.put_str(d.as_str());
    out.put_byte(b'}');
}

/// `text` as the body of a Rust string literal: `\" \\ \n \r \t \0`, other control bytes as
/// `\u{1b}`. Non-ASCII characters are copied as they are (`{:?}` escapes the non-printable
/// ones too; the text here stays readable and the literal still reads back the same).
pub fn rust<P: Put + ?Sized>(text: &str, out: &mut P) {
    runs(text, rust_rule, rust_control::<P>, out);
}

fn json_rule(b: u8) -> Option<&'static str> {
    match b {
        b'"' => Some("\\\""),
        b'\\' => Some("\\\\"),
        b'\n' => Some("\\n"),
        b'\r' => Some("\\r"),
        b'\t' => Some("\\t"),
        0x08 => Some("\\b"),
        0x0C => Some("\\f"),
        _ => None,
    }
}

fn json_control<P: Put + ?Sized>(b: u8, out: &mut P) {
    // JSON needs only bytes below 0x20 escaped; DEL is copied.
    if b == 0x7F {
        return out.put_byte(b);
    }
    out.put_str("\\u00");
    let mut d = Digits::at_end();
    radix_pow2_into(u128::from(b), 4, &LOWER, 2, &mut d);
    out.put_str(d.as_str());
}

/// `text` as the body of a JSON string (RFC 8259): `\" \\ \n \r \t \b \f`, other bytes below
/// 0x20 as `\u00XX` in lowercase hex; everything else copied.
pub fn json<P: Put + ?Sized>(text: &str, out: &mut P) {
    runs(text, json_rule, json_control::<P>, out);
}
