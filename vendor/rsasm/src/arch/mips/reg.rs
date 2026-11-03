//! MIPS register names.
//!
//! Registers are always written with a `$` sigil, and the general-purpose file
//! can be named three ways: by number (`$0`-`$31`), by O32 ABI role (`$a0`,
//! `$t3`, `$sp`), or — for two of them — by either of two ABI spellings
//! (`$fp` and `$s8` are both register 30). The number is what gets encoded, so
//! the table maps every accepted spelling onto one.
//!
//! Two smaller files sit beside it: the floating-point registers `$f0`-`$f31`,
//! and the eight flags `$fcc0`-`$fcc7` that MIPS IV gave the floating-point
//! comparisons to write and the branches to read.

use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RegClass {
    /// General purpose, `$0`-`$31`.
    Gpr,
    /// Floating point, `$f0`-`$f31`.
    Fpr,
    /// Floating-point condition flag, `$fcc0`-`$fcc7`. Before MIPS IV there
    /// was one flag and no way to name it, which is why every instruction
    /// that takes one also has a form that leaves `$fcc0` implied.
    Fcc,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Reg {
    pub class: RegClass,
    pub num: u8,
}

impl Reg {
    pub fn gpr(num: u8) -> Reg {
        Reg {
            class: RegClass::Gpr,
            num,
        }
    }

    pub fn fpr(num: u8) -> Reg {
        Reg {
            class: RegClass::Fpr,
            num,
        }
    }

    pub fn fcc(num: u8) -> Reg {
        Reg {
            class: RegClass::Fcc,
            num,
        }
    }

    pub fn is_gpr(self) -> bool {
        self.class == RegClass::Gpr
    }
}

/// `$zero`, hardwired to 0.
pub const ZERO: Reg = Reg {
    class: RegClass::Gpr,
    num: 0,
};
/// `$at`, the assembler temporary that macro expansions clobber.
pub const AT: Reg = Reg {
    class: RegClass::Gpr,
    num: 1,
};
/// `$gp`, which position-independent code points at the global offset
/// table, and `$sp`, which the `$gp` setup directives save it below.
pub const GP: Reg = Reg {
    class: RegClass::Gpr,
    num: 28,
};
pub const SP: Reg = Reg {
    class: RegClass::Gpr,
    num: 29,
};
/// `$ra`, the implicit link register of `jal` and of `jalr` written with one
/// operand.
pub const RA: Reg = Reg {
    class: RegClass::Gpr,
    num: 31,
};

/// The O32 ABI names, indexed by register number.
#[rustfmt::skip]
pub const ABI_NAMES: [&str; 32] = [
    "zero", "at", "v0", "v1", "a0", "a1", "a2", "a3",
    "t0",   "t1", "t2", "t3", "t4", "t5", "t6", "t7",
    "s0",   "s1", "s2", "s3", "s4", "s5", "s6", "s7",
    "t8",   "t9", "k0", "k1", "gp", "sp", "fp", "ra",
];

fn index() -> &'static HashMap<&'static str, Reg> {
    static INDEX: OnceLock<HashMap<&'static str, Reg>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut m = HashMap::new();
        for (n, name) in ABI_NAMES.iter().enumerate() {
            m.insert(*name, Reg::gpr(n as u8));
        }
        // `$s8` is the callee-saved spelling of the frame pointer; compilers
        // emit whichever matches how the function actually uses it.
        m.insert("s8", Reg::gpr(30));
        // `$rN` is not GAS syntax, but `$fN` is the only way to name an FPU
        // register, so those are spelled out.
        for n in 0..32u8 {
            m.insert(FPR_NAMES[n as usize], Reg::fpr(n));
        }
        for n in 0..8u8 {
            m.insert(FCC_NAMES[n as usize], Reg::fcc(n));
        }
        m
    })
}

#[rustfmt::skip]
const FPR_NAMES: [&str; 32] = [
    "f0",  "f1",  "f2",  "f3",  "f4",  "f5",  "f6",  "f7",
    "f8",  "f9",  "f10", "f11", "f12", "f13", "f14", "f15",
    "f16", "f17", "f18", "f19", "f20", "f21", "f22", "f23",
    "f24", "f25", "f26", "f27", "f28", "f29", "f30", "f31",
];

#[rustfmt::skip]
const FCC_NAMES: [&str; 8] = [
    "fcc0", "fcc1", "fcc2", "fcc3", "fcc4", "fcc5", "fcc6", "fcc7",
];

/// Looks up a register by the name written after the `$`, lowercased.
pub fn lookup(name: &str) -> Option<Reg> {
    index().get(name).copied()
}

/// The canonical name of a register, for diagnostics. Numbers above 31 cannot
/// be produced by the parser, so they are reported as-is rather than panicking.
pub fn name_of(r: Reg) -> String {
    match r.class {
        RegClass::Gpr => ABI_NAMES
            .get(r.num as usize)
            .map(|n| format!("${n}"))
            .unwrap_or_else(|| format!("${}", r.num)),
        RegClass::Fpr => format!("$f{}", r.num),
        RegClass::Fcc => format!("$fcc{}", r.num),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_names_map_to_numbers() {
        assert_eq!(lookup("zero"), Some(Reg::gpr(0)));
        assert_eq!(lookup("at"), Some(Reg::gpr(1)));
        assert_eq!(lookup("a0"), Some(Reg::gpr(4)));
        assert_eq!(lookup("t9"), Some(Reg::gpr(25)));
        assert_eq!(lookup("sp"), Some(Reg::gpr(29)));
        assert_eq!(lookup("ra"), Some(Reg::gpr(31)));
    }

    #[test]
    fn fp_and_s8_name_the_same_register() {
        assert_eq!(lookup("fp"), lookup("s8"));
        assert_eq!(lookup("fp"), Some(Reg::gpr(30)));
    }

    #[test]
    fn float_registers_are_a_separate_file() {
        assert_eq!(lookup("f0"), Some(Reg::fpr(0)));
        assert_eq!(lookup("f31"), Some(Reg::fpr(31)));
        assert_ne!(lookup("f0"), lookup("zero"));
        assert_eq!(lookup("f32"), None);
    }

    #[test]
    fn condition_flags_are_a_third_file() {
        assert_eq!(lookup("fcc0"), Some(Reg::fcc(0)));
        assert_eq!(lookup("fcc7"), Some(Reg::fcc(7)));
        assert_ne!(lookup("fcc0"), lookup("f0"));
        assert_eq!(lookup("fcc8"), None);
    }

    #[test]
    fn names_round_trip() {
        for n in ["$zero", "$a0", "$t9", "$ra", "$f12", "$fcc3"] {
            let r = lookup(n.trim_start_matches('$')).expect("known name");
            assert_eq!(name_of(r), n);
        }
    }
}
