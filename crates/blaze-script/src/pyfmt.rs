//! Values written as Python writes them in an f-string: `{x:g}` ([`g`]), `{x!r}` ([`repr`],
//! [`repr_str`], [`float_repr`]) and `{x}` ([`py_str`]) of a JSON value, and a float as
//! `fractions.Fraction` holds it ([`fraction`]), so a ported script's message is the bytes the
//! Python printed.

use serde_json::Value;

use crate::pyjson;

/// `f"{x:g}"`: six significant digits, trailing zeros dropped, an exponent (`1e+06`, `1e-05`)
/// when the number's is below -4 or at least 6.
pub fn g(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_owned();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".to_owned() } else { "-inf".to_owned() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0".to_owned() } else { "0".to_owned() };
    }
    // Rounded to six significant digits first: the exponent is the rounded number's.
    let sci = format!("{x:.5e}");
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if (-4..6).contains(&exponent) {
        let decimals = usize::try_from(5 - exponent).unwrap_or(0);
        trim_zeros(&format!("{x:.decimals$}"))
    } else {
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", trim_zeros(mantissa), exponent.unsigned_abs())
    }
}

fn trim_zeros(digits: &str) -> String {
    if digits.contains('.') { digits.trim_end_matches('0').trim_end_matches('.').to_owned() } else { digits.to_owned() }
}

/// Whether Python's `str.isprintable` holds for a character that is not ASCII. Close to
/// Python's: the controls, the separators but the space and the common format characters are
/// not printable; Python also counts unassigned and private-use code points out, which this
/// does not.
fn printable(c: char) -> bool {
    !(c.is_control()
        || matches!(c,
            '\u{a0}' | '\u{ad}' | '\u{1680}' | '\u{2000}'..='\u{200f}' | '\u{2028}'..='\u{202f}'
            | '\u{205f}'..='\u{2064}' | '\u{3000}' | '\u{feff}'))
}

/// `repr(s)` of a string: in `'` unless it holds a `'` and no `"`, with `\\`, `\n`, `\r`,
/// `\t`, the quote and what is not printable escaped.
pub fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c == '\u{7f}' => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if c.is_ascii() || printable(c) => out.push(c),
            c if (c as u32) < 0x100 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push(quote);
    out
}

/// A float as Python's `repr` writes it: `1.0`, `1e-05`, `inf`, `nan`.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        "nan".to_owned()
    } else if f.is_infinite() {
        if f > 0.0 { "inf".to_owned() } else { "-inf".to_owned() }
    } else {
        pyjson::float(f)
    }
}

/// `Fraction(x)` of a finite float: its exact value as a numerator (signed) and a denominator
/// (a power of two), in lowest terms, both in decimal: `(1, 2)` for 0.5, `(0, 1)` for -0.0.
pub fn fraction(x: f64) -> (String, String) {
    if x == 0.0 || !x.is_finite() {
        return ("0".to_owned(), "1".to_owned());
    }
    let bits = x.to_bits();
    let biased = i64::try_from((bits >> 52) & 0x7ff).unwrap_or(0);
    let fraction_bits = bits & ((1 << 52) - 1);
    // x = mantissa * 2^exponent, a subnormal without the hidden bit.
    let (mut mantissa, mut exponent) =
        if biased == 0 { (fraction_bits, -1074) } else { (fraction_bits | (1 << 52), biased - 1075) };
    let zeros = mantissa.trailing_zeros();
    mantissa >>= zeros;
    exponent += i64::from(zeros);
    let sign = if x < 0.0 { "-" } else { "" };
    let shift = u32::try_from(exponent.unsigned_abs()).unwrap_or(0);
    if exponent >= 0 {
        (format!("{sign}{}", times_pow2(mantissa, shift)), "1".to_owned())
    } else {
        (format!("{sign}{mantissa}"), times_pow2(1, shift))
    }
}

/// `m * 2^e` in decimal, of any size.
fn times_pow2(m: u64, e: u32) -> String {
    const BASE: u64 = 1_000_000_000;
    // Little-endian limbs of nine decimal digits.
    let mut limbs = vec![m % BASE, (m / BASE) % BASE, m / BASE / BASE];
    for _ in 0..e {
        let mut carry = 0;
        for limb in &mut limbs {
            let doubled = *limb * 2 + carry;
            *limb = doubled % BASE;
            carry = doubled / BASE;
        }
        if carry > 0 {
            limbs.push(carry);
        }
    }
    while limbs.len() > 1 && limbs.last() == Some(&0) {
        limbs.pop();
    }
    let mut out = limbs.last().map(u64::to_string).unwrap_or_default();
    for limb in limbs.iter().rev().skip(1) {
        out.push_str(&format!("{limb:09}"));
    }
    out
}

/// `repr(value)` of the Python value `json.loads` gave: `None`, `True`, `'a'`, `[1, 'b']`,
/// `{'k': 1.5}`.
pub fn repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => i.to_string(),
            (_, Some(u), _) => u.to_string(),
            (_, _, Some(f)) => float_repr(f),
            _ => n.to_string(),
        },
        Value::String(s) => repr_str(s),
        Value::Array(items) => format!("[{}]", items.iter().map(repr).collect::<Vec<_>>().join(", ")),
        Value::Object(fields) => format!(
            "{{{}}}",
            fields.iter().map(|(k, v)| format!("{}: {}", repr_str(k), repr(v))).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// `str(value)` of the Python value `json.loads` gave: a string as it is, anything else as
/// its `repr`.
pub fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => repr(other),
    }
}

/// `repr` of a list of strings, as `sorted(a_dict)` prints: `['a', 'b']`.
pub fn repr_strs<S: AsRef<str>>(items: &[S]) -> String {
    format!("[{}]", items.iter().map(|it| repr_str(it.as_ref())).collect::<Vec<_>>().join(", "))
}

/// `repr(b)` of bytes: `b'...'` in `'` unless they hold a `'` and no `"`, printable ASCII as
/// it is, `\\`, `\t`, `\n`, `\r` and the quote escaped, every other byte `\xNN`.
pub fn repr_bytes(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') { b'"' } else { b'\'' };
    let mut out = String::with_capacity(b.len() + 3);
    out.push('b');
    out.push(char::from(quote));
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(char::from(c));
            }
            0x20..0x7f => out.push(char::from(c)),
            c => out.push_str(&format!("\\x{c:02x}")),
        }
    }
    out.push(char::from(quote));
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Each expectation is what Python 3.13 prints for the same value.
    #[test]
    fn repr_bytes_is_pythons() {
        let cases: [(&[u8], &str); 5] = [
            (b"", "b''"),
            (b"\x00\x01ab\x7f\xff", "b'\\x00\\x01ab\\x7f\\xff'"),
            (b"it's", "b\"it's\""),
            (b"'\"", "b'\\'\"'"),
            (b"\\\t\n\r", "b'\\\\\\t\\n\\r'"),
        ];
        for (b, said) in cases {
            assert_eq!(repr_bytes(b), said, "{b:?}");
        }
    }

    #[test]
    fn g_is_pythons() {
        let cases = [
            (300.0, "300"),
            (0.0, "0"),
            (1.5, "1.5"),
            (25_000.0, "25000"),
            (10_001.0, "10001"),
            (1e6, "1e+06"),
            (1_234_567.0, "1.23457e+06"),
            (999_999.5, "1e+06"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (2.0 / 3.0, "0.666667"),
            (-4300.25, "-4300.25"),
            (123_456.0, "123456"),
        ];
        for (x, said) in cases {
            assert_eq!(g(x), said, "{x}");
        }
    }

    #[test]
    fn a_fraction_is_pythons() {
        let cases = [
            (0.5, "1", "2"),
            (-1.0, "-1", "1"),
            (-0.0, "0", "1"),
            (2.0, "2", "1"),
            (0.1, "3602879701896397", "36028797018963968"),
            (1e20, "100000000000000000000", "1"),
            (-2.5, "-5", "2"),
            (5e-324, "1", "202402253307310618352495346718917307049556649764142118356901358027430339567995346891960383701437124495187077864316811911389808737385793476867013399940738509921517424276566361364466907742093216341239767678472745068562007483424692698618103355649159556340810056512358769552333414615230502532186327508646006263307707741093494784"),
        ];
        for (x, numerator, denominator) in cases {
            assert_eq!(fraction(x), (numerator.to_owned(), denominator.to_owned()), "{x}");
        }
        assert_eq!(float_repr(f64::INFINITY), "inf");
        assert_eq!(float_repr(-0.0), "-0.0");
    }

    #[test]
    fn repr_is_pythons() {
        assert_eq!(repr_str("a"), "'a'");
        assert_eq!(repr_str("it's"), "\"it's\"");
        assert_eq!(repr_str("'\""), "'\\'\"'");
        assert_eq!(repr_str("a\tb\\\u{1}é\u{2028}"), "'a\\tb\\\\\\x01é\\u2028'");
        assert_eq!(repr(&json!({"a": [1, 2.5, null, true, "x"]})), "{'a': [1, 2.5, None, True, 'x']}");
        assert_eq!(py_str(&json!("x")), "x");
        assert_eq!(py_str(&json!(null)), "None");
        assert_eq!(repr_strs(&["a", "b"]), "['a', 'b']");
    }
}
