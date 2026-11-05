//! Register, flag and condition names.
//!
//! GNU as matches all of these case-insensitively, so `R1`, `PSW` and `BEQ`
//! are as good as their lower-case spellings.

/// A general register, `r0`-`r15`.
///
/// `sp` is accepted as `r0`, the stack pointer. GNU as 2.47 does not know the
/// name (it reports a syntax error), but Renesas's documentation and CC-RX
/// source use it, and it cannot mean anything else.
pub fn gpr(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    if lower == "sp" {
        return Some(0);
    }
    let digits = lower.strip_prefix('r')?;
    // `r01` is not a register to GNU as; nor is `r+1`.
    if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
        return None;
    }
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u8 = digits.parse().ok()?;
    (n < 16).then_some(n)
}

/// A control register and its number in `mvtc`/`mvfc`/`pushc`/`popc`.
///
/// The numbering has gaps (4-6, 14-15) that the instruction set reserves.
/// `extb` (13) exists only from RXv2 on, and `pbp`/`pben`/`bbpsw`/`bbpc`
/// (16 and up) only on RXv3; GNU as accepts the latter in `mvtc`/`mvfc`
/// without a CPU check, so they are listed, while `extb` is refused by
/// [`Creg::v2`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Creg {
    pub num: u8,
}

impl Creg {
    /// Whether this register needs an RXv2 CPU, which this backend does not
    /// target.
    pub fn v2(self) -> bool {
        self.num == 13
    }
}

pub fn creg(name: &str) -> Option<Creg> {
    let num = match name.to_ascii_lowercase().as_str() {
        "psw" => 0,
        "pc" => 1,
        "usp" => 2,
        "fpsw" => 3,
        "wr" => 7,
        "bpsw" => 8,
        "bpc" => 9,
        "isp" => 10,
        "fintv" => 11,
        "intb" => 12,
        "extb" => 13,
        "pbp" => 16,
        "pben" => 17,
        "bbpsw" => 24,
        "bbpc" => 25,
        _ => return None,
    };
    Some(Creg { num })
}

/// A PSW flag bit, as `setpsw`/`clrpsw` name it.
pub fn flag(name: &str) -> Option<u8> {
    Some(match name.to_ascii_lowercase().as_str() {
        "c" => 0,
        "z" => 1,
        "s" => 2,
        "o" => 3,
        "i" => 8,
        "u" => 9,
        _ => return None,
    })
}

/// A condition code, as spelled after `b`, `bm` or `sc`.
///
/// Codes 14 and 15 ("always" and "never") have no name: `bra` is the
/// unconditional branch.
pub fn cond(name: &str) -> Option<u8> {
    Some(match name {
        "eq" | "z" => 0,
        "ne" | "nz" => 1,
        "geu" | "c" => 2,
        "ltu" | "nc" => 3,
        "gtu" => 4,
        "leu" => 5,
        "pz" => 6,
        "n" => 7,
        "ge" => 8,
        "lt" => 9,
        "gt" => 10,
        "le" => 11,
        "o" => 12,
        "no" => 13,
        _ => return None,
    })
}

/// An operand size, as `.b`, `.w` and `.l` spell it.
///
/// The discriminants are the two-bit code most RX instructions store: memory
/// displacements are scaled by the operand size, so this also gives the shift
/// applied to them.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Size {
    B = 0,
    W = 1,
    L = 2,
}

impl Size {
    pub fn code(self) -> u32 {
        self as u32
    }

    /// Bytes per unit of displacement for this operand size.
    pub fn scale(self) -> i64 {
        1 << (self as u32)
    }
}

/// The size of a memory operand in the arithmetic instructions, spelled as a
/// suffix on the operand (`4[r1].w`) rather than on the mnemonic.
///
/// `.ub` has its own, shorter encoding; the others share a two-bit field in
/// the `06` prefix form, where `.uw` takes the code a fourth size would have.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MemEx {
    B,
    W,
    L,
    Ub,
    Uw,
}

impl MemEx {
    pub fn from_suffix(s: &str) -> Option<MemEx> {
        Some(match s.to_ascii_lowercase().as_str() {
            ".b" => MemEx::B,
            ".w" => MemEx::W,
            ".l" => MemEx::L,
            ".ub" => MemEx::Ub,
            ".uw" => MemEx::Uw,
            _ => return None,
        })
    }

    /// The two-bit code in the `06` prefix form.
    pub fn code(self) -> u32 {
        match self {
            MemEx::B | MemEx::Ub => 0,
            MemEx::W => 1,
            MemEx::L => 2,
            MemEx::Uw => 3,
        }
    }

    /// The size displacements are scaled by.
    pub fn size(self) -> Size {
        match self {
            MemEx::B | MemEx::Ub => Size::B,
            MemEx::W | MemEx::Uw => Size::W,
            MemEx::L => Size::L,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn general_registers() {
        assert_eq!(gpr("r0"), Some(0));
        assert_eq!(gpr("R15"), Some(15));
        assert_eq!(gpr("sp"), Some(0));
        assert_eq!(gpr("r16"), None);
        assert_eq!(gpr("r01"), None);
        assert_eq!(gpr("r"), None);
        assert_eq!(gpr("rx"), None);
    }

    #[test]
    fn control_registers_keep_their_gaps() {
        assert_eq!(creg("PSW"), Some(Creg { num: 0 }));
        assert_eq!(creg("intb").map(|c| c.num), Some(12));
        assert!(creg("extb").is_some_and(Creg::v2));
        assert_eq!(creg("r1"), None);
    }
}
