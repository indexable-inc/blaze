//! Register and condition names.
//!
//! V850 source names four kinds of thing with bare words: general registers,
//! system registers (for `ldsr`/`stsr`), condition codes (for `setf`, `cmov`
//! and the FPU's `cmpf`), and a few operation names of the RH850 cache and
//! prefetch instructions. None of them has a sigil, so which table a word is
//! looked up in depends on the operand position; see `encode`.
//!
//! GNU as compares all of these case-insensitively, and so does this module.

/// Whether a name exists on the selected processor.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Avail {
    /// Every V850.
    All,
    /// Only the extended cores, which for rsasm means RH850.
    Rh850,
}

/// The general registers' ABI names. These are the only aliases GNU as
/// accepts: `fp` and `ra`, which some V850 documentation uses, are rejected
/// by it as unresolved symbols, so they are not registers here either.
const GPR_ALIASES: &[(&str, u8)] = &[
    ("zero", 0),
    ("hp", 2),
    ("sp", 3),
    ("gp", 4),
    ("tp", 5),
    ("ep", 30),
    ("lp", 31),
];

/// The element pointer, base register of the short `sld`/`sst` forms.
pub const EP: u8 = 30;
/// The stack pointer, the only register `prepare` can name as its last
/// operand.
pub const SP: u8 = 3;

/// Looks up a general register: `r0`-`r31` or an ABI alias.
pub fn gpr(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    if let Some(digits) = lower.strip_prefix('r') {
        // `r01` is not a register to GNU as, and neither is `r+5`.
        let canonical = !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && (digits.len() == 1 || !digits.starts_with('0'));
        if canonical {
            return digits.parse::<u8>().ok().filter(|n| *n < 32);
        }
    }
    GPR_ALIASES
        .iter()
        .find(|(n, _)| *n == lower)
        .map(|(_, r)| *r)
}

/// The canonical name of a general register, for diagnostics.
pub fn gpr_name(r: u8) -> String {
    format!("r{r}")
}

/// System register names and their numbers.
///
/// Numbers are all a system register really has; the names are a courtesy,
/// and several numbers carry more than one name because the cores assign the
/// same slot different meanings. On RH850 a system register is further
/// qualified by a group (`selID`), which the names here do not encode: `psw`
/// is regID 5 in whatever group the instruction names.
#[rustfmt::skip]
const SYSREGS: &[(&str, u8, Avail)] = &[
    ("eipc", 0, Avail::All), ("eipsw", 1, Avail::All), ("fepc", 2, Avail::All),
    ("fepsw", 3, Avail::All), ("ecr", 4, Avail::All), ("psw", 5, Avail::All),
    ("ctpc", 16, Avail::Rh850), ("ctpsw", 17, Avail::Rh850), ("dbpc", 18, Avail::Rh850),
    ("dbpsw", 19, Avail::Rh850), ("ctbp", 20, Avail::Rh850), ("dir", 21, Avail::Rh850),
    ("bpc", 22, Avail::Rh850), ("asid", 23, Avail::Rh850), ("bpav", 24, Avail::Rh850),
    ("bpam", 25, Avail::Rh850), ("bpdv", 26, Avail::Rh850), ("bpdm", 27, Avail::Rh850),
    ("eiic", 13, Avail::Rh850), ("feic", 14, Avail::Rh850), ("dbic", 15, Avail::Rh850),
    ("eiwr", 28, Avail::Rh850), ("fewr", 29, Avail::Rh850), ("dbwr", 30, Avail::Rh850),
    ("bsel", 31, Avail::Rh850),
    ("eh_cfg", 1, Avail::Rh850), ("eh_reset", 2, Avail::Rh850), ("eh_base", 3, Avail::Rh850),
    ("sw_ctl", 0, Avail::Rh850), ("sw_cfg", 1, Avail::Rh850), ("sw_base", 3, Avail::Rh850),
    ("fpsr", 6, Avail::Rh850), ("fpepc", 7, Avail::Rh850), ("fpst", 8, Avail::Rh850),
    ("fpcc", 9, Avail::Rh850), ("fpcfg", 10, Avail::Rh850), ("fpec", 11, Avail::Rh850),
    ("cfg", 7, Avail::Rh850), ("sccfg", 11, Avail::Rh850), ("scbp", 12, Avail::Rh850),
    ("pmcr0", 4, Avail::Rh850), ("pmis2", 14, Avail::Rh850),
    ("mpm", 0, Avail::Rh850), ("mpc", 1, Avail::Rh850), ("tid", 2, Avail::Rh850),
    ("pid", 6, Avail::Rh850), ("vmecr", 4, Avail::Rh850), ("vmtid", 5, Avail::Rh850),
    ("vmadr", 6, Avail::Rh850), ("vsecr", 0, Avail::Rh850), ("vstid", 1, Avail::Rh850),
    ("vsadr", 2, Avail::Rh850), ("mca", 24, Avail::Rh850), ("mcs", 25, Avail::Rh850),
    ("mcc", 26, Avail::Rh850), ("mcr", 27, Avail::Rh850), ("fpspc", 27, Avail::Rh850),
    ("ipa0l", 6, Avail::Rh850), ("ipa0u", 7, Avail::Rh850), ("ipa1l", 8, Avail::Rh850),
    ("ipa1u", 9, Avail::Rh850), ("ipa2l", 10, Avail::Rh850), ("ipa2u", 11, Avail::Rh850),
    ("ipa3l", 12, Avail::Rh850), ("ipa3u", 13, Avail::Rh850), ("ipa4l", 14, Avail::Rh850),
    ("ipa4u", 15, Avail::Rh850), ("dpa0l", 16, Avail::Rh850), ("dpa0u", 17, Avail::Rh850),
    ("dpa1l", 18, Avail::Rh850), ("dpa1u", 19, Avail::Rh850), ("dpa2l", 20, Avail::Rh850),
    ("dpa2u", 21, Avail::Rh850), ("dpa3l", 22, Avail::Rh850), ("dpa3u", 23, Avail::Rh850),
    ("dpa4l", 24, Avail::Rh850), ("dpa4u", 25, Avail::Rh850), ("dpa5l", 26, Avail::Rh850),
    ("dpa5u", 27, Avail::Rh850),
    ("mpu10_mpm", 0, Avail::Rh850), ("mpu10_mpc", 1, Avail::Rh850),
    ("mpu10_tid", 2, Avail::Rh850), ("mpu10_vmecr", 3, Avail::Rh850),
    ("mpu10_vmtid", 4, Avail::Rh850), ("mpu10_vmadr", 5, Avail::Rh850),
    ("mpu10_ipa0l", 6, Avail::Rh850), ("mpu10_ipa0u", 7, Avail::Rh850),
    ("mpu10_ipa1l", 8, Avail::Rh850), ("mpu10_ipa1u", 9, Avail::Rh850),
    ("mpu10_ipa2l", 10, Avail::Rh850), ("mpu10_ipa2u", 11, Avail::Rh850),
    ("mpu10_ipa3l", 12, Avail::Rh850), ("mpu10_ipa3u", 13, Avail::Rh850),
    ("mpu10_ipa4l", 14, Avail::Rh850), ("mpu10_ipa4u", 15, Avail::Rh850),
    ("mpu10_dpa0l", 16, Avail::Rh850), ("mpu10_dpa0u", 17, Avail::Rh850),
    ("mpu10_dpa1l", 18, Avail::Rh850), ("mpu10_dpa1u", 19, Avail::Rh850),
    ("mpu10_dpa2l", 20, Avail::Rh850), ("mpu10_dpa2u", 21, Avail::Rh850),
    ("mpu10_dpa3l", 22, Avail::Rh850), ("mpu10_dpa3u", 23, Avail::Rh850),
    ("mpu10_dpa4l", 24, Avail::Rh850), ("mpu10_dpa4u", 25, Avail::Rh850),
    ("mpu10_dpa5l", 26, Avail::Rh850), ("mpu10_dpa5u", 27, Avail::Rh850),
];

/// Looks up a system register by name. `sr0`-`sr31` work everywhere.
pub fn sysreg(name: &str, rh850: bool) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    if let Some(digits) = lower.strip_prefix("sr")
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits.len() == 1 || !digits.starts_with('0'))
    {
        return digits.parse::<u8>().ok().filter(|n| *n < 32);
    }
    SYSREGS
        .iter()
        .find(|(n, _, a)| *n == lower && (rh850 || *a == Avail::All))
        .map(|(_, r, _)| *r)
}

/// Integer condition codes, as `setf`, `sasf`, `cmov`, `adf` and `sbf` take
/// them. They are the low four bits of a `Bcond` opcode, which is why several
/// share a value: `c` and `l` are one condition read as carry or as unsigned
/// "lower".
#[rustfmt::skip]
const CONDITIONS: &[(&str, u8)] = &[
    ("v", 0x0), ("c", 0x1), ("l", 0x1), ("z", 0x2), ("e", 0x2), ("nh", 0x3),
    ("n", 0x4), ("s", 0x4), ("t", 0x5), ("lt", 0x6), ("le", 0x7), ("nv", 0x8),
    ("nc", 0x9), ("nl", 0x9), ("nz", 0xa), ("ne", 0xa), ("h", 0xb), ("p", 0xc),
    ("ns", 0xc), ("sa", 0xd), ("ge", 0xe), ("gt", 0xf),
];

/// The "saturated" condition, which `adf` and `sbf` cannot test.
pub const COND_SA: u8 = 0xd;

pub fn condition(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    CONDITIONS
        .iter()
        .find(|(n, _)| *n == lower)
        .map(|(_, c)| *c)
}

/// Floating-point comparison conditions for `cmpf.s`/`cmpf.d`.
///
/// Sixteen predicates, each spelled two ways: the IEEE name for the true
/// sense and its negation share an encoding, because the FPU sets the flag
/// and it is the following `bc`-style test that picks the sense.
#[rustfmt::skip]
const FLOAT_CONDITIONS: &[(&str, u8)] = &[
    ("f", 0x0), ("t", 0x0), ("un", 0x1), ("or", 0x1), ("eq", 0x2), ("neq", 0x2),
    ("ueq", 0x3), ("ogl", 0x3), ("olt", 0x4), ("uge", 0x4), ("ult", 0x5),
    ("oge", 0x5), ("ole", 0x6), ("ugt", 0x6), ("ule", 0x7), ("ogt", 0x7),
    ("sf", 0x8), ("st", 0x8), ("ngle", 0x9), ("gle", 0x9), ("seq", 0xa),
    ("sne", 0xa), ("ngl", 0xb), ("gl", 0xb), ("lt", 0xc), ("nlt", 0xc),
    ("nge", 0xd), ("ge", 0xd), ("le", 0xe), ("nle", 0xe), ("ngt", 0xf),
    ("gt", 0xf),
];

pub fn float_condition(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    FLOAT_CONDITIONS
        .iter()
        .find(|(n, _)| *n == lower)
        .map(|(_, c)| *c)
}

/// Operation names of the RH850 `cache` instruction.
#[rustfmt::skip]
const CACHE_OPS: &[(&str, u8)] = &[
    ("chbii", 0x00), ("chbid", 0x04), ("chbiwbd", 0x06), ("chbwbd", 0x07),
    ("cibii", 0x20), ("cibid", 0x24), ("cibiwbd", 0x26), ("cibwbd", 0x27),
    ("cfali", 0x40), ("cfald", 0x44), ("cisti", 0x60), ("cildi", 0x61),
    ("cistd", 0x64), ("cildd", 0x65),
];

pub fn cache_op(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    CACHE_OPS.iter().find(|(n, _)| *n == lower).map(|(_, c)| *c)
}

/// Operation names of the RH850 `pref` instruction.
pub fn pref_op(name: &str) -> Option<u8> {
    match name.to_ascii_lowercase().as_str() {
        "prefi" => Some(0),
        "prefd" => Some(4),
        _ => None,
    }
}

/// RH850 virtualisation register names, `vr0`-`vr31`.
pub fn vector_reg(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    let digits = lower.strip_prefix("vr")?;
    let canonical = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits.len() == 1 || !digits.starts_with('0'));
    if !canonical {
        return None;
    }
    digits.parse::<u8>().ok().filter(|n| *n < 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_and_abi_names() {
        assert_eq!(gpr("r0"), Some(0));
        assert_eq!(gpr("R31"), Some(31));
        assert_eq!(gpr("r32"), None);
        assert_eq!(gpr("r01"), None);
        assert_eq!(gpr("sp"), Some(3));
        assert_eq!(gpr("ep"), Some(30));
        assert_eq!(gpr("lp"), Some(31));
        assert_eq!(gpr("fp"), None);
        assert_eq!(gpr("ra"), None);
    }

    #[test]
    fn system_register_names_depend_on_the_core() {
        assert_eq!(sysreg("psw", false), Some(5));
        assert_eq!(sysreg("ctbp", false), None);
        assert_eq!(sysreg("ctbp", true), Some(20));
        assert_eq!(sysreg("sr31", false), Some(31));
        assert_eq!(sysreg("sr32", true), None);
    }

    #[test]
    fn condition_spellings_share_codes() {
        assert_eq!(condition("c"), condition("l"));
        assert_eq!(condition("Z"), Some(2));
        assert_eq!(float_condition("olt"), float_condition("uge"));
        assert_eq!(cache_op("cildd"), Some(0x65));
        assert_eq!(pref_op("prefd"), Some(4));
        assert_eq!(vector_reg("vr31"), Some(31));
        assert_eq!(vector_reg("vr32"), None);
    }
}
