//! x86 register names and properties.

use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum RegClass {
    /// General purpose, addressed through ModRM/SIB.
    Gpr,
    /// `ah`/`ch`/`dh`/`bh`: encoded as GPR numbers 4-7 but unusable with REX.
    GprHigh,
    Segment,
    /// The `rip` pseudo-register, only valid as a memory base.
    Rip,
    Xmm,
    /// AVX 256-bit vector registers.
    Ymm,
    /// AVX-512 512-bit vector registers.
    Zmm,
    /// AVX-512 opmask registers `k0`-`k7`.
    Mask,
    /// AMX tile registers `tmm0`-`tmm7`.
    Tmm,
    Mmx,
    Control,
    Debug,
    /// x87 stack registers.
    St,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Reg {
    pub class: RegClass,
    /// Encoding number: 0-15 for most classes, 0-31 for `xmm`/`ymm`/`zmm`.
    pub num: u8,
    /// Width in bytes.
    pub size: u8,
    /// True for `spl`/`bpl`/`sil`/`dil`, which only exist when a REX prefix is
    /// present. Without REX those encodings mean `ah`/`ch`/`dh`/`bh`.
    pub rex_required: bool,
}

impl Reg {
    pub fn is_gpr(&self) -> bool {
        matches!(self.class, RegClass::Gpr | RegClass::GprHigh)
    }

    /// True for `xmm`/`ymm`/`zmm`, the three classes that share one register
    /// file and one set of encoding extension bits.
    pub fn is_vector(&self) -> bool {
        matches!(self.class, RegClass::Xmm | RegClass::Ymm | RegClass::Zmm)
    }

    /// True if the register number needs the extension bit in REX.
    ///
    /// `xmm16`-`xmm31` answer true as well, but REX cannot reach them: only
    /// EVEX has the fourth and fifth bits. The encoder rejects them before it
    /// gets as far as building a REX byte.
    #[allow(dead_code)]
    pub fn needs_rex_ext(&self) -> bool {
        self.num >= 8
    }

    /// True for the upper half of the EVEX register file, which needs the
    /// `R'`/`V'`/`X` bits that only EVEX supplies.
    pub fn needs_evex_ext(&self) -> bool {
        self.num >= 16
    }

    /// True for APX's `r16`-`r31`, whose fourth number bit only the REX2
    /// prefix and the extended EVEX one carry.
    pub fn needs_rex2(&self) -> bool {
        self.is_gpr() && self.num >= 16
    }

    /// x86 forbids `rsp`/`esp` as a SIB index. A vector register is legal
    /// there only in the VSIB form used by gather and scatter, which the
    /// encoder checks separately.
    pub fn valid_index(&self) -> bool {
        match self.class {
            RegClass::Gpr => !(self.num == 4 && self.size >= 4),
            RegClass::Xmm | RegClass::Ymm | RegClass::Zmm => true,
            _ => false,
        }
    }
}

struct Entry {
    name: &'static str,
    class: RegClass,
    num: u8,
    size: u8,
    rex_required: bool,
}

/// Every register this backend understands, keyed by lowercase name.
///
/// Laid out one line per group so it reads the way the manuals tabulate it.
#[rustfmt::skip]
static REGS: &[Entry] = &{
    // Built as a literal list rather than generated, so the table reads the
    // way the manuals do.
    const fn e(name: &'static str, class: RegClass, num: u8, size: u8, rex_required: bool) -> Entry {
        Entry { name, class, num, size, rex_required }
    }
    use RegClass::*;
    [
        // 64-bit
        e("rax", Gpr, 0, 8, false), e("rcx", Gpr, 1, 8, false),
        e("rdx", Gpr, 2, 8, false), e("rbx", Gpr, 3, 8, false),
        e("rsp", Gpr, 4, 8, false), e("rbp", Gpr, 5, 8, false),
        e("rsi", Gpr, 6, 8, false), e("rdi", Gpr, 7, 8, false),
        e("r8", Gpr, 8, 8, false), e("r9", Gpr, 9, 8, false),
        e("r10", Gpr, 10, 8, false), e("r11", Gpr, 11, 8, false),
        e("r12", Gpr, 12, 8, false), e("r13", Gpr, 13, 8, false),
        e("r14", Gpr, 14, 8, false), e("r15", Gpr, 15, 8, false),
        // 32-bit
        e("eax", Gpr, 0, 4, false), e("ecx", Gpr, 1, 4, false),
        e("edx", Gpr, 2, 4, false), e("ebx", Gpr, 3, 4, false),
        e("esp", Gpr, 4, 4, false), e("ebp", Gpr, 5, 4, false),
        e("esi", Gpr, 6, 4, false), e("edi", Gpr, 7, 4, false),
        e("r8d", Gpr, 8, 4, false), e("r9d", Gpr, 9, 4, false),
        e("r10d", Gpr, 10, 4, false), e("r11d", Gpr, 11, 4, false),
        e("r12d", Gpr, 12, 4, false), e("r13d", Gpr, 13, 4, false),
        e("r14d", Gpr, 14, 4, false), e("r15d", Gpr, 15, 4, false),
        // 16-bit
        e("ax", Gpr, 0, 2, false), e("cx", Gpr, 1, 2, false),
        e("dx", Gpr, 2, 2, false), e("bx", Gpr, 3, 2, false),
        e("sp", Gpr, 4, 2, false), e("bp", Gpr, 5, 2, false),
        e("si", Gpr, 6, 2, false), e("di", Gpr, 7, 2, false),
        e("r8w", Gpr, 8, 2, false), e("r9w", Gpr, 9, 2, false),
        e("r10w", Gpr, 10, 2, false), e("r11w", Gpr, 11, 2, false),
        e("r12w", Gpr, 12, 2, false), e("r13w", Gpr, 13, 2, false),
        e("r14w", Gpr, 14, 2, false), e("r15w", Gpr, 15, 2, false),
        // 8-bit, low
        e("al", Gpr, 0, 1, false), e("cl", Gpr, 1, 1, false),
        e("dl", Gpr, 2, 1, false), e("bl", Gpr, 3, 1, false),
        // These four require REX; without it the same encodings mean ah..bh.
        e("spl", Gpr, 4, 1, true), e("bpl", Gpr, 5, 1, true),
        e("sil", Gpr, 6, 1, true), e("dil", Gpr, 7, 1, true),
        e("r8b", Gpr, 8, 1, false), e("r9b", Gpr, 9, 1, false),
        e("r10b", Gpr, 10, 1, false), e("r11b", Gpr, 11, 1, false),
        e("r12b", Gpr, 12, 1, false), e("r13b", Gpr, 13, 1, false),
        e("r14b", Gpr, 14, 1, false), e("r15b", Gpr, 15, 1, false),
        // 8-bit, high halves of the legacy registers
        e("ah", GprHigh, 4, 1, false), e("ch", GprHigh, 5, 1, false),
        e("dh", GprHigh, 6, 1, false), e("bh", GprHigh, 7, 1, false),
        // Segments
        e("es", Segment, 0, 2, false), e("cs", Segment, 1, 2, false),
        e("ss", Segment, 2, 2, false), e("ds", Segment, 3, 2, false),
        e("fs", Segment, 4, 2, false), e("gs", Segment, 5, 2, false),
        // Control and debug registers, for `mov cr0, eax` and the like.
        e("cr0", Control, 0, 4, false), e("cr2", Control, 2, 4, false),
        e("cr3", Control, 3, 4, false), e("cr4", Control, 4, 4, false),
        e("cr8", Control, 8, 4, false),
        e("dr0", Debug, 0, 4, false), e("dr1", Debug, 1, 4, false),
        e("dr2", Debug, 2, 4, false), e("dr3", Debug, 3, 4, false),
        e("dr6", Debug, 6, 4, false), e("dr7", Debug, 7, 4, false),
        // Instruction pointer, usable only as a memory base.
        e("rip", Rip, 0, 8, false), e("eip", Rip, 0, 4, false),
        // MMX. The eight registers alias the x87 stack, which is why `emms`
        // exists and why they are numbered 0-7 with no extension bits.
        e("mm0", Mmx, 0, 8, false), e("mm1", Mmx, 1, 8, false),
        e("mm2", Mmx, 2, 8, false), e("mm3", Mmx, 3, 8, false),
        e("mm4", Mmx, 4, 8, false), e("mm5", Mmx, 5, 8, false),
        e("mm6", Mmx, 6, 8, false), e("mm7", Mmx, 7, 8, false),
        // AVX-512 opmask registers. `k0` is a real register everywhere except
        // in a `{k}` writemask decorator, where it means "no masking".
        e("k0", Mask, 0, 8, false), e("k1", Mask, 1, 8, false),
        e("k2", Mask, 2, 8, false), e("k3", Mask, 3, 8, false),
        e("k4", Mask, 4, 8, false), e("k5", Mask, 5, 8, false),
        e("k6", Mask, 6, 8, false), e("k7", Mask, 7, 8, false),
        // AMX tile registers, whose size is set by the tile configuration.
        e("tmm0", Tmm, 0, 0, false), e("tmm1", Tmm, 1, 0, false),
        e("tmm2", Tmm, 2, 0, false), e("tmm3", Tmm, 3, 0, false),
        e("tmm4", Tmm, 4, 0, false), e("tmm5", Tmm, 5, 0, false),
        e("tmm6", Tmm, 6, 0, false), e("tmm7", Tmm, 7, 0, false),
        // The top of the x87 stack. `st(1)`-`st(7)` are spelled with an index
        // in parentheses, which the operand parsers read as one register.
        e("st", St, 0, 10, false),
        // xmm0-31, ymm0-31 and zmm0-31, the control and debug registers, and
        // APX's `r16`-`r31` are added by `tables()`: rows of one shape each,
        // which read better generated than tabulated.
    ]
};

/// The three vector classes, with their name stem and width in bytes.
const VECTOR_FAMILIES: [(&str, RegClass, u8); 3] = [
    ("xmm", RegClass::Xmm, 16),
    ("ymm", RegClass::Ymm, 32),
    ("zmm", RegClass::Zmm, 64),
];

struct Tables {
    by_name: HashMap<&'static str, Reg>,
    /// Reverse map for diagnostics, keyed by everything `Reg` encodes.
    by_reg: HashMap<(RegClass, u8, u8), &'static str>,
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut by_name = HashMap::new();
        let mut by_reg = HashMap::new();
        let mut add = |name: &'static str, r: Reg| {
            by_name.insert(name, r);
            by_reg.entry((r.class, r.num, r.size)).or_insert(name);
        };
        for e in REGS {
            add(
                e.name,
                Reg {
                    class: e.class,
                    num: e.num,
                    size: e.size,
                    rex_required: e.rex_required,
                },
            );
        }
        // `cr0`-`cr15` and `db0`-`db15`, which GNU as also spells `dr0`-`dr15`.
        // Only some exist on any CPU, but the encoding has room for all of
        // them and both reference assemblers accept every one; `cr8` and above
        // need REX, which limits them to 64-bit mode. Their width is the
        // mode's, so it is left for the instruction table to decide.
        for (stems, class) in [
            (&["cr"][..], RegClass::Control),
            (&["dr", "db"][..], RegClass::Debug),
        ] {
            for stem in stems {
                for num in 0..16u8 {
                    let name: &'static str = Box::leak(format!("{stem}{num}").into_boxed_str());
                    add(
                        name,
                        Reg {
                            class,
                            num,
                            size: 0,
                            rex_required: false,
                        },
                    );
                }
            }
        }
        // APX's sixteen further general registers. They are spelled by number
        // in every width -- `r16`, `r16d`, `r16w`, `r16b` -- so there is no
        // historical name to tabulate, and no high-byte half.
        for num in 16..32u8 {
            for (suffix, size) in [("", 8u8), ("d", 4), ("w", 2), ("b", 1)] {
                let name: &'static str = Box::leak(format!("r{num}{suffix}").into_boxed_str());
                add(
                    name,
                    Reg {
                        class: RegClass::Gpr,
                        num,
                        size,
                        rex_required: false,
                    },
                );
            }
        }
        for (stem, class, size) in VECTOR_FAMILIES {
            for num in 0..32u8 {
                // Leaked so the rest of the backend can pass `&'static str`
                // names around; there are 96 of them and they live as long as
                // the process anyway.
                let name: &'static str = Box::leak(format!("{stem}{num}").into_boxed_str());
                add(
                    name,
                    Reg {
                        class,
                        num,
                        size,
                        rex_required: false,
                    },
                );
            }
        }
        Tables { by_name, by_reg }
    })
}

/// Looks up a register by its lowercase name.
pub fn lookup(name: &str) -> Option<Reg> {
    tables().by_name.get(name).copied()
}

/// Looks up a register that exists in `bits`-bit mode. Outside 64-bit mode
/// the registers only REX, EVEX or long mode can reach are not registers at
/// all to GNU as: an unknown name in AT&T syntax, and an ordinary symbol in
/// Intel syntax.
pub fn lookup_in_mode(name: &str, bits: u8) -> Option<Reg> {
    lookup(name).filter(|r| bits == 64 || !r.only_64())
}

impl Reg {
    /// True for a register that only exists in 64-bit mode.
    pub fn only_64(&self) -> bool {
        match self.class {
            RegClass::Gpr => self.size == 8 || self.num >= 8 || self.rex_required,
            RegClass::Rip => self.size == 8,
            RegClass::Xmm | RegClass::Ymm | RegClass::Zmm | RegClass::Control | RegClass::Debug => {
                self.num >= 8
            }
            _ => false,
        }
    }
}

/// `st(n)`: the x87 stack register `n` places from the top.
pub fn st(n: u8) -> Option<Reg> {
    (n < 8).then_some(Reg {
        class: RegClass::St,
        num: n,
        size: 10,
        rex_required: false,
    })
}

/// The canonical name of a register, for diagnostics.
pub fn name_of(r: Reg) -> &'static str {
    if r.class == RegClass::St && r.num > 0 {
        const ST: [&str; 8] = [
            "st", "st(1)", "st(2)", "st(3)", "st(4)", "st(5)", "st(6)", "st(7)",
        ];
        return ST[r.num as usize & 7];
    }
    tables()
        .by_reg
        .get(&(r.class, r.num, r.size))
        .copied()
        .unwrap_or("?")
}

/// True if `name` is a register in this architecture.
#[allow(dead_code)]
pub fn is_register(name: &str) -> bool {
    lookup(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_up_by_size() {
        assert_eq!(
            lookup("rax").unwrap(),
            Reg {
                class: RegClass::Gpr,
                num: 0,
                size: 8,
                rex_required: false
            }
        );
        assert_eq!(lookup("eax").unwrap().size, 4);
        assert_eq!(lookup("ax").unwrap().size, 2);
        assert_eq!(lookup("al").unwrap().size, 1);
        assert_eq!(lookup("r15b").unwrap().num, 15);
    }

    #[test]
    fn high_byte_registers_are_distinct() {
        let ah = lookup("ah").unwrap();
        assert_eq!(ah.class, RegClass::GprHigh);
        assert_eq!(ah.num, 4);
        // spl shares ah's number but is a normal GPR needing REX.
        let spl = lookup("spl").unwrap();
        assert_eq!(spl.class, RegClass::Gpr);
        assert_eq!(spl.num, 4);
        assert!(spl.rex_required);
    }

    #[test]
    fn rsp_cannot_be_an_index() {
        assert!(!lookup("rsp").unwrap().valid_index());
        assert!(lookup("rbp").unwrap().valid_index());
        assert!(lookup("r12").unwrap().valid_index());
    }

    #[test]
    fn names_round_trip() {
        for n in [
            "rax", "r13d", "sil", "ah", "xmm7", "gs", "mm3", "k5", "ymm12", "zmm31",
        ] {
            assert_eq!(name_of(lookup(n).unwrap()), n);
        }
    }

    #[test]
    fn apx_registers_exist_in_every_width() {
        for (name, size) in [("r16", 8u8), ("r23d", 4), ("r28w", 2), ("r31b", 1)] {
            let r = lookup(name).unwrap();
            assert_eq!(r.class, RegClass::Gpr);
            assert_eq!(r.size, size);
            assert!(r.needs_rex2(), "`{name}` needs REX2");
            assert!(r.only_64());
            assert_eq!(name_of(r), name);
        }
        assert_eq!(lookup("r16").unwrap().num, 16);
        assert_eq!(lookup("r31b").unwrap().num, 31);
        assert!(lookup("r32").is_none());
        assert!(lookup("r16l").is_none());
        assert!(!lookup("r15").unwrap().needs_rex2());
        // `r20` is a legal SIB index: only `rsp`'s number is reserved there,
        // and the fourth bit tells the two apart.
        assert!(lookup("r20").unwrap().valid_index());
        assert!(lookup_in_mode("r16", 32).is_none());
    }

    #[test]
    fn vector_classes_are_distinct_but_share_numbers() {
        assert_eq!(lookup("xmm31").unwrap().num, 31);
        assert_eq!(lookup("ymm31").unwrap().class, RegClass::Ymm);
        assert_eq!(lookup("zmm0").unwrap().size, 64);
        assert!(lookup("zmm16").unwrap().needs_evex_ext());
        assert!(!lookup("zmm15").unwrap().needs_evex_ext());
        assert!(lookup("xmm32").is_none());
        assert!(lookup("k8").is_none());
        assert!(lookup("mm8").is_none());
    }
}
