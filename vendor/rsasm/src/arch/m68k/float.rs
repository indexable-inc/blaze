//! Floating-point immediates: `#1.5` in Motorola source, `#0r1.5` in GNU
//! source, written as a single, double, extended or packed-decimal real.
//!
//! No expression can hold a float, so the operand parser reads the literal
//! from the source text and keeps it as an `f64`. Each format is then made
//! from that `f64` exactly as vasm makes it from its C `double`, which is
//! what decides the one place the references part:
//!
//! - Single and double precision are the IEEE bits, and both references
//!   write the same ones.
//! - Extended precision is the 68881's 96-bit format: sign and 15-bit
//!   exponent, 16 zero bits, and a 64-bit mantissa with its integer bit
//!   explicit. vasm writes that. GNU as 2.47 writes its immediates without the
//!   16 zero bits (it asks `gen_to_words` for six words where the gap is only
//!   inserted for five), a pattern its own disassembler cannot read back and
//!   no FPU loads as the number written, while its `.extend` directive is
//!   right. rsasm follows vasm, whose mantissa is the `double`'s 53 bits.
//! - Packed decimal is 17 significant digits and a three-digit exponent, the
//!   digits being what C's `%.16e` prints. GNU as refuses packed immediates.

use crate::lexer::Dialect;

/// Reads a floating-point literal, or returns `None` for anything that is not
/// one (an integer, a symbol, an expression).
///
/// GNU source marks a float with GNU as's `0r` prefix, or any of the other
/// letters its m68k port takes there (`0d1.5`, `0e1.5`, `0f1.5`, `0s1.5`).
/// Motorola source writes the number itself, with a decimal point: `1.5`,
/// `-2.5e-3`. GNU as `--mri` takes both spellings, and so does the Motorola
/// dialect here; vasm takes only the second.
pub fn parse(text: &str, dialect: Dialect) -> Option<f64> {
    let text = text.trim();
    let (neg, body) = match text.as_bytes().first() {
        Some(b'-') => (true, text[1..].trim_start()),
        Some(b'+') => (false, text[1..].trim_start()),
        _ => (false, text),
    };
    let prefixed = body.len() > 2
        && body.as_bytes()[0] == b'0'
        && matches!(body.as_bytes()[1] | 0x20, b'r' | b'd' | b'e' | b'f' | b's');
    // GNU as takes a sign after the prefix as well as before it: `0r-1.5`.
    let (neg, number) = if prefixed {
        match body.as_bytes()[2] {
            b'-' => (!neg, &body[3..]),
            b'+' => (neg, &body[3..]),
            _ => (neg, &body[2..]),
        }
    } else if dialect == Dialect::Gas {
        return None;
    } else {
        // Without a prefix it has to look like a real: a point, not just
        // digits, or `1e5` would stop being a symbol.
        if !body.contains('.') {
            return None;
        }
        (neg, body)
    };
    if !number
        .bytes()
        .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
        || !number
            .as_bytes()
            .first()
            .is_some_and(|c| c.is_ascii_digit() || *c == b'.')
    {
        return None;
    }
    let v: f64 = number.parse().ok()?;
    if !v.is_finite() {
        return None;
    }
    Some(if neg { -v } else { v })
}

/// The four sizes a floating-point operand comes in.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Float {
    Single,
    Double,
    Extended,
    Packed,
}

impl Float {
    /// How many bytes an immediate of this size takes.
    pub fn bytes_len(self) -> usize {
        match self {
            Float::Single => 4,
            Float::Double => 8,
            Float::Extended | Float::Packed => 12,
        }
    }

    /// A literal written in this size.
    pub fn bytes(self, f: f64) -> Vec<u8> {
        match self {
            Float::Single => single(f).to_vec(),
            Float::Double => double(f).to_vec(),
            Float::Extended => extended(f).to_vec(),
            Float::Packed => packed(f).to_vec(),
        }
    }
}

pub fn single(f: f64) -> [u8; 4] {
    (f as f32).to_bits().to_be_bytes()
}

pub fn double(f: f64) -> [u8; 8] {
    f.to_bits().to_be_bytes()
}

/// The 68881 extended format, from a `double` as vasm's `conv2ieee80` makes
/// it: the 52 fraction bits under an explicit integer bit, and the exponent
/// rebiased.
pub fn extended(f: f64) -> [u8; 12] {
    let x = f.to_bits();
    let mut out = [0u8; 12];
    if x == 0 {
        return out;
    }
    if x == 1 << 63 {
        out[0] = 0x80;
        return out;
    }
    let man = ((x & 0xf_ffff_ffff_ffff) << 11) | 1 << 63;
    let exp = (((x >> 52) & 0x7ff) as u32)
        .wrapping_sub(0x3ff)
        .wrapping_add(0x3fff);
    out[0] = ((x >> 56) as u8 & 0x80) | (exp >> 8) as u8;
    out[1] = exp as u8;
    out[4..].copy_from_slice(&man.to_be_bytes());
    out
}

/// The 68881 packed-decimal format, as vasm's `conv2packed` makes it from
/// `printf("%.16e")`: the mantissa's sign and the exponent's sign in the top
/// bits, three exponent digits, one integer digit and sixteen fraction digits,
/// all in BCD.
pub fn packed(f: f64) -> [u8; 12] {
    let text = format!("{f:.16e}");
    let (mantissa, exp) = text.split_once('e').unwrap_or((&text, "0"));
    let (neg, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mantissa),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut e: i32 = exp.parse().unwrap_or(0);
    let int: u32 = int.parse().unwrap_or(0);
    let mut out = [0u8; 12];
    let mut sign = 0u8;
    // `sscanf("%d")` of the integer digit loses the sign of `-0.0`, so only a
    // nonzero digit carries it.
    if neg && int != 0 {
        sign |= 0x80;
    }
    if e < 0 {
        e = -e;
        sign |= 0x40;
    }
    let e = e as u32;
    let bcd = |v: u32| ((((v / 10) % 10) << 4) | (v % 10)) as u8;
    out[0] = sign | bcd((e / 100) % 10);
    out[1] = bcd(e % 100);
    out[3] = bcd(int % 10);
    for (i, d) in frac.bytes().take(16).enumerate() {
        let d = d.wrapping_sub(b'0') & 0x0f;
        out[4 + i / 2] |= if i % 2 == 0 { d << 4 } else { d };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spellings() {
        assert_eq!(parse("0r1.5", Dialect::Gas), Some(1.5));
        assert_eq!(parse("-0r1.5", Dialect::Gas), Some(-1.5));
        assert_eq!(parse("0e2.5e1", Dialect::Gas), Some(25.0));
        assert_eq!(parse("0d-3.5", Dialect::Gas), Some(-3.5));
        assert_eq!(parse("1.5", Dialect::Gas), None);
        assert_eq!(parse("1.5", Dialect::Motorola), Some(1.5));
        assert_eq!(parse("-1.0e-5", Dialect::Motorola), Some(-1.0e-5));
        assert_eq!(parse("15", Dialect::Motorola), None);
        assert_eq!(parse("label", Dialect::Motorola), None);
        assert_eq!(parse("0x15", Dialect::Gas), None);
    }

    // Expected bytes from vasmm68k_mot -m68881 (`fadd.x #...,fp0` and
    // friends); single and double also from m68k-elf-as.
    #[test]
    fn formats() {
        assert_eq!(single(1.5), [0x3f, 0xc0, 0, 0]);
        assert_eq!(single(0.1), [0x3d, 0xcc, 0xcc, 0xcd]);
        assert_eq!(
            double(0.1),
            [0x3f, 0xb9, 0x99, 0x99, 0x99, 0x99, 0x99, 0x9a]
        );
        assert_eq!(extended(1.5), [0x3f, 0xff, 0, 0, 0xc0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            extended(0.1),
            [
                0x3f, 0xfb, 0, 0, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xd0, 0
            ]
        );
        assert_eq!(packed(1.5), [0, 0, 0, 1, 0x50, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(packed(0.1), [0x40, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(packed(-1.0e-5), [0xc0, 5, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(
            packed(12345.678),
            [0, 4, 0, 1, 0x23, 0x45, 0x67, 0x80, 0, 0, 0, 0]
        );
    }
}
