//! SPARC register names.
//!
//! The 32 integer registers have two spellings. `%r0`-`%r31` is the flat one;
//! the usual one names the four windows the register file is divided into —
//! `%g` globals, `%o` outgoing, `%l` locals, `%i` incoming — eight each, in
//! that order. `save` rotates the window so the caller's `%o` registers become
//! the callee's `%i` registers, which is why the two spellings must agree on
//! the numbering.
//!
//! The floating-point file is the one place where a register's name and its
//! field value part company. V8 has 32 of them and numbers them directly; V9
//! doubled the file to 64 without widening the five-bit field, so the extra
//! registers are addressable only in pairs and quads and their numbers are
//! swizzled into the field by [`fp_field`].

/// What a register can be used for. SPARC keeps these in separate name
/// spaces, so `%f1` and `%r1` are unrelated.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RegClass {
    /// `%g0`-`%i7`, encoded as 0-31.
    Int,
    /// `%f0`-`%f31`, and on V9 the even `%f32`-`%f62`.
    Float,
    /// `%icc` (0) and `%xcc` (2): the integer condition codes a V9 predicted
    /// branch or conditional move selects between.
    Icc,
    /// `%fcc0`-`%fcc3`.
    Fcc,
    /// Ancillary state registers, reached through `rd`/`wr`. `%y` is ASR 0.
    Asr,
    /// `%fsr` (0) and `%fq` (1), the floating-point status register and the
    /// exception queue. Neither is a slot in any instruction's register
    /// fields: naming one picks a different opcode instead.
    ///
    /// `%efsr`, the third of them, is not here: the instruction that reads
    /// it arrived with OSA2011, and GNU as takes it only from `-Av9d`
    /// upwards, which is past what this backend claims.
    FpState,
}

/// How wide a floating-point operand is, which decides both which registers
/// can name it and how the name reaches its field.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FpWidth {
    Single,
    Double,
    Quad,
}

impl FpWidth {
    /// This width's name, for diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            FpWidth::Single => "single",
            FpWidth::Double => "double",
            FpWidth::Quad => "quad",
        }
    }

    /// The register numbers this width may start on: a double pair starts on
    /// an even register and a quad on a multiple of four.
    fn step(self) -> u8 {
        match self {
            FpWidth::Single => 1,
            FpWidth::Double => 2,
            FpWidth::Quad => 4,
        }
    }

    /// True for a register this width can name at all. Only the double and
    /// quad forms reach `%f32` and above, and a quad needs four registers, so
    /// `%f62` is a double but not a quad.
    pub fn allows(self, num: u8) -> bool {
        if self == FpWidth::Single {
            return num < 32;
        }
        num.is_multiple_of(self.step()) && num <= 64 - self.step()
    }
}

/// The five-bit field a floating-point register number encodes to.
///
/// V9 doubled the register file without widening the field, so `%f32`-`%f62`
/// take the field values a double or quad register can never use: bit 5 of
/// the number travels in bit 0, which is free because such a number is
/// always even. `%f32` is therefore field value 1, `%f34` is 3, and so on.
pub fn fp_field(width: FpWidth, num: u8) -> u32 {
    match width {
        FpWidth::Single => u32::from(num),
        _ => u32::from((num & 0x1e) | (num >> 5)),
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Reg {
    pub class: RegClass,
    /// The field value, except for `Float`, where it is the register number
    /// and [`fp_field`] turns it into one.
    pub num: u8,
}

impl Reg {
    pub fn is_int(&self) -> bool {
        self.class == RegClass::Int
    }

    pub fn is_float(&self) -> bool {
        self.class == RegClass::Float
    }
}

/// `%g0`, the register that reads as zero and discards writes. Half of SPARC's
/// synthetic instructions are some real instruction with `%g0` in one slot.
pub const G0: Reg = Reg {
    class: RegClass::Int,
    num: 0,
};

/// `%o7`, where `call` leaves the return address.
pub const O7: Reg = Reg {
    class: RegClass::Int,
    num: 15,
};

/// `%i7`, the return address in the caller's window after `save`.
pub const I7: Reg = Reg {
    class: RegClass::Int,
    num: 31,
};

/// Parses `<prefix><decimal>` and returns the number if it is within `max`.
///
/// Leading zeros are rejected so that `%g00` is not silently `%g0`.
fn indexed(name: &str, prefix: &str, max: u8) -> Option<u8> {
    let digits = name.strip_prefix(prefix)?;
    if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
        return None;
    }
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u16 = digits.parse().ok()?;
    (n <= max as u16).then_some(n as u8)
}

/// Looks up a register by its name without the `%` sigil, lowercased.
pub fn lookup(name: &str) -> Option<Reg> {
    let int = |num: u8| {
        Some(Reg {
            class: RegClass::Int,
            num,
        })
    };
    match name {
        // The stack and frame pointers are the two windowed registers with
        // names of their own: `%sp` is `%o6` and, after `save`, the same
        // physical register is the callee's `%fp` = `%i6`.
        "sp" => return int(14),
        "fp" => return int(30),
        "icc" => {
            return Some(Reg {
                class: RegClass::Icc,
                num: 0,
            });
        }
        "xcc" => {
            return Some(Reg {
                class: RegClass::Icc,
                num: 2,
            });
        }
        "y" => {
            return Some(Reg {
                class: RegClass::Asr,
                num: 0,
            });
        }
        "fsr" => {
            return Some(Reg {
                class: RegClass::FpState,
                num: 0,
            });
        }
        "fq" => {
            return Some(Reg {
                class: RegClass::FpState,
                num: 1,
            });
        }
        _ => {}
    }
    if let Some(n) = indexed(name, "g", 7) {
        return int(n);
    }
    if let Some(n) = indexed(name, "o", 7) {
        return int(8 + n);
    }
    if let Some(n) = indexed(name, "l", 7) {
        return int(16 + n);
    }
    if let Some(n) = indexed(name, "i", 7) {
        return int(24 + n);
    }
    if let Some(n) = indexed(name, "r", 31) {
        return int(n);
    }
    if let Some(n) = indexed(name, "f", 62) {
        // The upper half of V9's file is reached only through the double and
        // quad forms, so there is no `%f33` for any instruction to name, and
        // `%f63` would be the odd half of a pair that does not exist.
        return (n < 32 || n.is_multiple_of(2)).then_some(Reg {
            class: RegClass::Float,
            num: n,
        });
    }
    if let Some(n) = indexed(name, "fcc", 3) {
        return Some(Reg {
            class: RegClass::Fcc,
            num: n,
        });
    }
    // ASR 0 is `%y`; 1-6 are reserved but assemblers still let them be named.
    if let Some(n) = indexed(name, "asr", 31) {
        return Some(Reg {
            class: RegClass::Asr,
            num: n,
        });
    }
    None
}

/// A register's canonical spelling, for diagnostics.
pub fn name_of(r: Reg) -> String {
    match r.class {
        RegClass::Int => {
            let (letter, n) = match r.num {
                0..=7 => ('g', r.num),
                8..=15 => ('o', r.num - 8),
                16..=23 => ('l', r.num - 16),
                _ => ('i', r.num.saturating_sub(24)),
            };
            format!("%{letter}{n}")
        }
        RegClass::Float => format!("%f{}", r.num),
        RegClass::Icc => if r.num == 0 { "%icc" } else { "%xcc" }.to_string(),
        RegClass::Fcc => format!("%fcc{}", r.num),
        RegClass::Asr => {
            if r.num == 0 {
                "%y".to_string()
            } else {
                format!("%asr{}", r.num)
            }
        }
        RegClass::FpState => if r.num == 0 { "%fsr" } else { "%fq" }.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_windowed_and_flat_spellings_agree() {
        for (windowed, flat) in [("g0", "r0"), ("o0", "r8"), ("l0", "r16"), ("i7", "r31")] {
            assert_eq!(lookup(windowed), lookup(flat), "{windowed} vs {flat}");
        }
        assert_eq!(lookup("sp"), lookup("o6"));
        assert_eq!(lookup("fp"), lookup("i6"));
    }

    #[test]
    fn out_of_range_and_malformed_names_are_rejected() {
        for bad in [
            "g8", "o8", "r32", "f33", "f63", "f64", "fcc4", "asr32", "g", "g00", "g-1", "gx", "",
        ] {
            assert_eq!(lookup(bad), None, "`{bad}` should not be a register");
        }
    }

    /// V9's upper registers take the field values the low half cannot: the
    /// number's bit 5 lands in bit 0, which a double or quad register never
    /// uses.
    #[test]
    fn the_upper_float_registers_swizzle_into_five_bits() {
        for (num, field) in [
            (0, 0),
            (2, 2),
            (30, 30),
            (32, 1),
            (34, 3),
            (60, 29),
            (62, 31),
        ] {
            assert_eq!(fp_field(FpWidth::Double, num), field, "%f{num}");
        }
        // A single register is its own field value, odd ones included.
        assert_eq!(fp_field(FpWidth::Single, 31), 31);
    }

    #[test]
    fn each_width_starts_on_its_own_multiple() {
        assert!(FpWidth::Single.allows(31) && !FpWidth::Single.allows(32));
        assert!(FpWidth::Double.allows(62) && !FpWidth::Double.allows(31));
        // A quad is four registers, so `%f62` would run off the end.
        assert!(FpWidth::Quad.allows(60) && !FpWidth::Quad.allows(62));
    }
}
