//! PowerPC register names.
//!
//! PowerPC assembly conventionally writes register operands as bare numbers —
//! `add 3, 4, 5` — because the ISA gives every field a fixed meaning, so the
//! name adds nothing. There is no register table to consult in that spelling;
//! the numbers are matched by the field the instruction form asks for, in
//! [`super::encode`]. This module only covers the *named* spellings that GNU
//! as also accepts, so that both styles work.

/// Which bank a named register belongs to.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RegClass {
    /// `r0`-`r31`.
    Gpr,
    /// `f0`-`f31`, also spelled `fr0`-`fr31`.
    Fpr,
    /// `cr0`-`cr7`, a *field* of the condition register rather than a register.
    Cr,
    /// A special-purpose register named rather than numbered: `lr`, `ctr`,
    /// `xer`. These stand for the SPR *number*, which is what `mfspr` and
    /// `mtspr` encode.
    Spr,
    /// `v0`-`v31`, the AltiVec vector registers.
    Vr,
    /// `vs0`-`vs63`, the VSX registers. The upper half of the bank is the
    /// AltiVec registers seen again: `vs32` is `v0`.
    Vsr,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Reg {
    pub class: RegClass,
    /// Register number, or SPR number for [`RegClass::Spr`].
    pub num: u16,
}

/// SPR numbers of the three special registers that have names.
pub const SPR_XER: u16 = 1;
pub const SPR_LR: u16 = 8;
pub const SPR_CTR: u16 = 9;

/// Parses a lowercase register name.
pub fn lookup(name: &str) -> Option<Reg> {
    let named = |class, num| Some(Reg { class, num });
    match name {
        "lr" => return named(RegClass::Spr, SPR_LR),
        "ctr" => return named(RegClass::Spr, SPR_CTR),
        "xer" => return named(RegClass::Spr, SPR_XER),
        // `sp` and `toc` are the ABI names of r1 and r2; GNU as accepts them
        // under `-mregnames`, and accepting them costs nothing.
        "sp" => return named(RegClass::Gpr, 1),
        "toc" | "rtoc" => return named(RegClass::Gpr, 2),
        _ => {}
    }
    // `fr` before `f`, `vs` before `v`, and `cr` before nothing, so that the
    // longer prefix wins.
    for (prefix, class, max) in [
        ("fr", RegClass::Fpr, 31),
        ("cr", RegClass::Cr, 7),
        ("vs", RegClass::Vsr, 63),
        ("r", RegClass::Gpr, 31),
        ("f", RegClass::Fpr, 31),
        ("v", RegClass::Vr, 31),
    ] {
        if let Some(rest) = name.strip_prefix(prefix)
            && let Some(n) = decimal(rest)
            && n <= max
        {
            return Some(Reg { class, num: n });
        }
    }
    None
}

/// A one- or two-digit decimal number with no leading zero, which is how
/// register names are spelled. Rejecting `r007` keeps a symbol named `r007`
/// usable.
fn decimal(s: &str) -> Option<u16> {
    let b = s.as_bytes();
    match b {
        [d] if d.is_ascii_digit() => Some((d - b'0') as u16),
        [a, d] if a.is_ascii_digit() && *a != b'0' && d.is_ascii_digit() => {
            Some(((a - b'0') * 10 + (d - b'0')) as u16)
        }
        _ => None,
    }
}

/// True if `name` (already lowercased) is a register in this architecture.
pub fn is_register(name: &str) -> bool {
    lookup(name).is_some()
}

/// A register's canonical spelling, for diagnostics.
pub fn describe(r: Reg) -> String {
    match r.class {
        RegClass::Gpr => format!("r{}", r.num),
        RegClass::Fpr => format!("f{}", r.num),
        RegClass::Cr => format!("cr{}", r.num),
        RegClass::Vr => format!("v{}", r.num),
        RegClass::Vsr => format!("vs{}", r.num),
        RegClass::Spr => match r.num {
            SPR_XER => "xer".into(),
            SPR_LR => "lr".into(),
            SPR_CTR => "ctr".into(),
            n => format!("spr{n}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_banks() {
        assert_eq!(
            lookup("r0"),
            Some(Reg {
                class: RegClass::Gpr,
                num: 0
            })
        );
        assert_eq!(
            lookup("r31"),
            Some(Reg {
                class: RegClass::Gpr,
                num: 31
            })
        );
        assert_eq!(
            lookup("f9"),
            Some(Reg {
                class: RegClass::Fpr,
                num: 9
            })
        );
        assert_eq!(
            lookup("fr9"),
            Some(Reg {
                class: RegClass::Fpr,
                num: 9
            })
        );
        assert_eq!(
            lookup("cr7"),
            Some(Reg {
                class: RegClass::Cr,
                num: 7
            })
        );
    }

    #[test]
    fn out_of_range_and_padded_numbers_are_not_registers() {
        assert_eq!(lookup("r32"), None);
        assert_eq!(lookup("cr8"), None);
        assert_eq!(lookup("r007"), None);
        assert_eq!(lookup("r"), None);
        assert_eq!(lookup("rx"), None);
    }

    #[test]
    fn vector_banks() {
        assert_eq!(
            lookup("v31").map(|r| (r.class, r.num)),
            Some((RegClass::Vr, 31))
        );
        assert_eq!(
            lookup("vs0").map(|r| (r.class, r.num)),
            Some((RegClass::Vsr, 0))
        );
        // The VSX bank is twice as wide as the others.
        assert_eq!(
            lookup("vs63").map(|r| (r.class, r.num)),
            Some((RegClass::Vsr, 63))
        );
        assert_eq!(lookup("vs64"), None);
        assert_eq!(lookup("v32"), None);
    }

    #[test]
    fn special_registers_carry_their_spr_number() {
        assert_eq!(lookup("lr").map(|r| r.num), Some(SPR_LR));
        assert_eq!(lookup("ctr").map(|r| r.num), Some(SPR_CTR));
        assert_eq!(lookup("xer").map(|r| r.num), Some(SPR_XER));
    }
}
