//! Integer text as Zig's `std.fmt.parseInt` reads it, so a key value or a glyph option is
//! accepted or refused exactly as ghostty does: an optional `+` or `-`, digits of the radix in
//! either case, `_` between digits (not first or last), and the range of the target type checked
//! after each digit (Overflow), before a later bad character (InvalidCharacter) is seen.

/// Why a text is not an integer of the asked range.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntError {
    /// Empty, a sign alone, a leading or trailing `_`, or a byte that is no digit of the radix.
    InvalidCharacter,
    /// The value left the range of the target type.
    Overflow,
}

/// The value of `text` in `radix` (2..=36), within `min..=max`.
pub(crate) fn parse_int(text: &[u8], radix: u32, min: i64, max: i64) -> Result<i64, IntError> {
    let (negative, body) = match text.first() {
        None => return Err(IntError::InvalidCharacter),
        Some(b'+') => (false, &text[1..]),
        Some(b'-') => (true, &text[1..]),
        Some(_) => (false, text),
    };
    let (Some(&first), Some(&last)) = (body.first(), body.last()) else {
        return Err(IntError::InvalidCharacter);
    };
    if first == b'_' || last == b'_' {
        return Err(IntError::InvalidCharacter);
    }
    let mut x: i64 = 0;
    for &c in body {
        if c == b'_' {
            continue;
        }
        let digit = digit_of(c, radix).ok_or(IntError::InvalidCharacter)?;
        x = x.checked_mul(i64::from(radix)).ok_or(IntError::Overflow)?;
        x = match negative {
            false => x.checked_add(digit),
            true => x.checked_sub(digit),
        }
        .ok_or(IntError::Overflow)?;
        if x < min || x > max {
            return Err(IntError::Overflow);
        }
    }
    Ok(x)
}

fn digit_of(c: u8, radix: u32) -> Option<i64> {
    let v = match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'z' => c - b'a' + 10,
        b'A'..=b'Z' => c - b'A' + 10,
        _ => return None,
    };
    (u32::from(v) < radix).then_some(i64::from(v))
}

/// `u32` in decimal.
pub(crate) fn parse_u32(text: &[u8]) -> Result<u32, IntError> {
    parse_int(text, 10, 0, i64::from(u32::MAX)).map(|v| v as u32)
}

/// `i32` in decimal.
pub(crate) fn parse_i32(text: &[u8]) -> Result<i32, IntError> {
    parse_int(text, 10, i64::from(i32::MIN), i64::from(i32::MAX)).map(|v| v as i32)
}

/// Zig's `u21` in hexadecimal: a code point as the glyph protocol writes it (`cp=e0a0`).
pub(crate) fn parse_u21_hex(text: &[u8]) -> Result<u32, IntError> {
    parse_int(text, 16, 0, 0x1F_FFFF).map(|v| v as u32)
}

/// `@round` for the non-negative and negative values a placement computes: half away from zero.
pub(crate) fn round(x: f64) -> f64 {
    let t = trunc(x);
    let frac = x - t;
    if frac >= 0.5 {
        t + 1.0
    } else if frac <= -0.5 {
        t - 1.0
    } else {
        t
    }
}

/// `@trunc` without std: values at or past 2^52 are already whole.
fn trunc(x: f64) -> f64 {
    if !x.is_finite() || x.abs() >= 4_503_599_627_370_496.0 {
        return x;
    }
    (x as i64) as f64
}

/// `@intFromFloat(@round(x))` into `u32`, saturating where Zig's would be undefined.
pub(crate) fn round_u32(x: f64) -> u32 {
    round(x) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zig_parse_int_forms() {
        assert_eq!(parse_u32(b"10"), Ok(10));
        assert_eq!(parse_u32(b"+10"), Ok(10));
        assert_eq!(parse_u32(b"1_0"), Ok(10));
        assert_eq!(parse_u32(b"1__0"), Ok(10));
        assert_eq!(parse_u32(b"-0"), Ok(0));
        assert_eq!(parse_u32(b"-1"), Err(IntError::Overflow));
        assert_eq!(parse_u32(b""), Err(IntError::InvalidCharacter));
        assert_eq!(parse_u32(b"-"), Err(IntError::InvalidCharacter));
        assert_eq!(parse_u32(b"_1"), Err(IntError::InvalidCharacter));
        assert_eq!(parse_u32(b"1_"), Err(IntError::InvalidCharacter));
        assert_eq!(parse_u32(b"4294967295"), Ok(u32::MAX));
        assert_eq!(parse_u32(b"4294967296"), Err(IntError::Overflow));
        // The range is checked digit by digit, so the overflow is seen before the `x`.
        assert_eq!(parse_u32(b"9999999999x"), Err(IntError::Overflow));
        assert_eq!(parse_i32(b"-2147483648"), Ok(i32::MIN));
        assert_eq!(parse_i32(b"-2147483649"), Err(IntError::Overflow));
        assert_eq!(parse_u21_hex(b"E0a0"), Ok(0xE0A0));
        assert_eq!(parse_u21_hex(b"1fffff"), Ok(0x1F_FFFF));
        assert_eq!(parse_u21_hex(b"200000"), Err(IntError::Overflow));
        assert_eq!(parse_u21_hex(b"zz"), Err(IntError::InvalidCharacter));
    }

    #[test]
    fn round_half_away_from_zero() {
        assert_eq!(round(56.25), 56.0);
        assert_eq!(round(177.77), 178.0);
        assert_eq!(round(0.5), 1.0);
        assert_eq!(round(-0.5), -1.0);
        assert_eq!(round(2.4999), 2.0);
        assert_eq!(round_u32(-3.0), 0);
    }
}
