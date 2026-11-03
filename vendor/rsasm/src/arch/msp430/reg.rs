//! Register names.
//!
//! The MSP430 has sixteen registers, four of which have a second name and a
//! job: `r0`/`pc`, `r1`/`sp`, `r2`/`sr` (which is also the first constant
//! generator) and `r3`, the second constant generator. There is no `r3`
//! alias, since nothing reads it as a register.
//!
//! `check_reg` in `gas/config/tc-msp430.c` is deliberately loose, and this
//! mirrors it: the `r` is optional, so `mov 5, r6` moves `r5`, and the name
//! ends at the first character that is not a letter or digit, so `r4)` is
//! `r4`.

/// `r0`, the program counter.
pub const PC: u8 = 0;
/// `r1`, the stack pointer.
pub const SP: u8 = 1;
/// `r2`, the status register, and the first constant generator.
pub const SR: u8 = 2;
/// `r3`, the second constant generator.
pub const CG: u8 = 3;

/// True where a register name ends: at the end of the string, or at anything
/// that is not a letter or a digit (GNU as's `is_regname_end`).
fn name_end(c: Option<char>) -> bool {
    !matches!(c, Some(c) if c.is_ascii_alphanumeric())
}

/// The register `name` denotes, or `None`.
///
/// A leading `r` or `R` is skipped, `pc`, `sp` and `sr` are 0, 1 and 2, and
/// anything else is read as a number from 0 to 15 in C's `strtol` base 0 —
/// so `r0x5` and `0x5` are both `r5`.
pub fn check_reg(name: &str) -> Option<u8> {
    let t = match name.strip_prefix(['r', 'R']) {
        Some(rest) => rest,
        None => name,
    };
    let lower = t.to_ascii_lowercase();
    for (alias, n) in [("pc", PC), ("sp", SP), ("sr", SR)] {
        if lower.starts_with(alias) && name_end(t[2.min(t.len())..].chars().next()) {
            return Some(n);
        }
    }
    if t.starts_with('0') && name_end(t[1..].chars().next()) {
        return Some(PC);
    }
    let (value, rest) = strtol(t)?;
    if !(1..=15).contains(&value) || !name_end(rest.chars().next()) {
        return None;
    }
    Some(value as u8)
}

/// C's `strtol` with base 0: an optional sign, then `0x` hexadecimal, a
/// leading `0` for octal, or decimal. Returns the value and what is left.
fn strtol(s: &str) -> Option<(i64, &str)> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (radix, digits) = if let Some(hex) = body.strip_prefix("0x").or(body.strip_prefix("0X")) {
        (16, hex)
    } else if body.starts_with('0') {
        (8, body)
    } else {
        (10, body)
    };
    let n = digits
        .find(|c: char| !c.is_digit(radix))
        .unwrap_or(digits.len());
    if n == 0 {
        return None;
    }
    let value = i64::from_str_radix(&digits[..n], radix).ok()?;
    Some((if neg { -value } else { value }, &digits[n..]))
}

/// Whether a bare number names a register: `check_reg` reads the operand's
/// text, so a number small enough to be a register number is one.
///
/// The one difference from the reference is the spelling of numbers C's
/// `strtol` does not read: `0b101` is `5` here and a symbolic address of 5
/// there, since rsasm's lexer has already turned it into a number.
pub fn number_reg(v: i64) -> Option<u8> {
    (0..=15).contains(&v).then_some(v as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_names_follow_gnu_as() {
        assert_eq!(check_reg("r0"), Some(0));
        assert_eq!(check_reg("R15"), Some(15));
        assert_eq!(check_reg("pc"), Some(0));
        assert_eq!(check_reg("rpc"), Some(0));
        assert_eq!(check_reg("sp"), Some(1));
        assert_eq!(check_reg("SR"), Some(2));
        // The `r` is optional, and so `5` is a register.
        assert_eq!(check_reg("5"), Some(5));
        assert_eq!(check_reg("0x5"), Some(5));
        // Out of range, or not a number at all.
        assert_eq!(check_reg("r16"), None);
        assert_eq!(check_reg("foo"), None);
        assert_eq!(check_reg("spam"), None);
        assert_eq!(check_reg("0x200"), None);
    }
}
