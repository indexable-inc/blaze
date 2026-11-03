//! 680x0 register names.
//!
//! The same names serve both syntaxes. What differs is the spelling around
//! them: GNU as requires the `%` sigil (without it `d0` is an ordinary symbol,
//! checked against `m68k-elf-as`), while Motorola source writes registers bare
//! and GNU as `--mri` also tolerates the sigil. That decision belongs to the
//! operand parser; this module only answers "is this name a register".
//!
//! Beyond the general registers, GNU as keeps one namespace for everything
//! else an operand can name — `sr`, the FPU's `fpcr`, the MMU's `crp`, the
//! caches `ic` and `dc`, `movec`'s `vbr` — and lets each instruction decide
//! which of them it takes. So does this: those come back as [`Reg::Ctl`] with
//! GNU's own number for them.

use super::table::{MOVEC_REGS, rid};

/// A register an operand can name.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Reg {
    /// `d0`-`d7`.
    D(u8),
    /// `a0`-`a7`, including `sp` (and `fp`, in GNU syntax).
    A(u8),
    /// `fp0`-`fp7`.
    Fp(u8),
    /// The program counter, only meaningful as a base register.
    Pc,
    /// Any other register, by its number in `rid`.
    Ctl(u16),
}

impl Reg {
    /// The 4-bit number an index or `MOVEC` field uses: data registers are
    /// 0-7 and address registers 8-15.
    pub fn index_bits(self) -> Option<u8> {
        match self {
            Reg::D(n) => Some(n),
            Reg::A(n) => Some(8 | n),
            _ => None,
        }
    }
}

/// Registers named in `tc-m68k.c`'s `init_table` outside its block of `movec`
/// control registers, which [`MOVEC_REGS`] holds. Only those some instruction
/// here takes are listed: the ColdFire MAC's accumulators, the suppressed
/// `zd0`/`za0`/`zpc` and the coprocessor numbers `cop0`-`cop7` are left out,
/// and stay ordinary symbols.
const NAMED: &[(&str, u16)] = &[
    ("ac", rid::AC),
    ("ac0", rid::TT0),
    ("ac1", rid::TT1),
    ("acusr", rid::PSR),
    ("bac0", rid::BAC0),
    ("bac1", rid::BAC1),
    ("bac2", rid::BAC2),
    ("bac3", rid::BAC3),
    ("bac4", rid::BAC4),
    ("bac5", rid::BAC5),
    ("bac6", rid::BAC6),
    ("bac7", rid::BAC7),
    ("bad0", rid::BAD0),
    ("bad1", rid::BAD1),
    ("bad2", rid::BAD2),
    ("bad3", rid::BAD3),
    ("bad4", rid::BAD4),
    ("bad5", rid::BAD5),
    ("bad6", rid::BAD6),
    ("bad7", rid::BAD7),
    ("bc", rid::BC),
    ("cal", rid::CAL),
    ("cc", rid::CCR),
    ("ccr", rid::CCR),
    ("control", rid::FPC),
    ("crp", rid::CRP),
    ("dc", rid::DC),
    ("drp", rid::DRP),
    // `init_table` enters `fpc` twice, as `fpiar` and then as `fpcr`, and the
    // second entry wins.
    ("fpc", rid::FPC),
    ("fpcr", rid::FPC),
    ("fpi", rid::FPI),
    ("fpiar", rid::FPI),
    ("fps", rid::FPS),
    ("fpsr", rid::FPS),
    ("iaddr", rid::FPI),
    ("ic", rid::IC),
    ("nc", rid::NC),
    ("pcsr", rid::PCSR),
    ("psr", rid::PSR),
    ("scc", rid::SCC),
    ("sr", rid::SR),
    ("status", rid::FPS),
    ("tt0", rid::TT0),
    ("tt1", rid::TT1),
    ("val", rid::VAL),
];

/// Looks up a register by its lower-case name.
///
/// `fp` is GNU as's name for `a6`; Motorola assemblers have no such register,
/// and a symbol called `fp` is plausible there, so the caller says whether it
/// counts.
pub fn lookup(name: &str, gnu: bool) -> Option<Reg> {
    let b = name.as_bytes();
    if b.len() == 2 && (b'0'..=b'7').contains(&b[1]) {
        let n = b[1] - b'0';
        match b[0] {
            b'd' => return Some(Reg::D(n)),
            b'a' => return Some(Reg::A(n)),
            _ => {}
        }
    }
    if b.len() == 3 && name.starts_with("fp") && (b'0'..=b'7').contains(&b[2]) {
        return Some(Reg::Fp(b[2] - b'0'));
    }
    Some(match name {
        "sp" | "ssp" => Reg::A(7),
        "fp" if gnu => Reg::A(6),
        "pc" => Reg::Pc,
        _ => {
            let id = NAMED
                .binary_search_by(|(n, _)| (*n).cmp(name))
                .ok()
                .map(|i| NAMED[i].1)
                .or_else(|| {
                    MOVEC_REGS
                        .binary_search_by(|(n, ..)| (*n).cmp(name))
                        .ok()
                        .map(|i| MOVEC_REGS[i].1)
                })?;
            Reg::Ctl(id)
        }
    })
}

/// The name a control register is best known by, for messages.
pub(crate) fn name_of(id: u16) -> &'static str {
    match id {
        rid::SR => "sr",
        rid::CCR => "ccr",
        rid::USP => "usp",
        _ => NAMED
            .iter()
            .map(|&(n, i)| (n, i))
            .chain(MOVEC_REGS.iter().map(|&(n, i, _)| (n, i)))
            .find(|&(_, i)| i == id)
            .map_or("register", |(n, _)| n),
    }
}

/// The 12-bit `MOVEC` code of a control register, if the CPU whose list of
/// them is `ctrl` has it.
pub(crate) fn movec(id: u16, ctrl: &[u16]) -> Option<u16> {
    use super::table::{RAMBAR, RAMBAR_ALT};
    // On the few CPUs that list `RAMBAR_ALT`, `rambar` means that one.
    let id = if id == RAMBAR && ctrl.contains(&RAMBAR_ALT) {
        RAMBAR_ALT
    } else {
        id
    };
    if !ctrl.contains(&id) {
        return None;
    }
    // `RAMBAR_ALT` has no name of its own; it shares `rambar0`'s code.
    let id = if id == RAMBAR_ALT { rid::RAMBAR0 } else { id };
    MOVEC_REGS
        .iter()
        .find(|&&(_, i, _)| i == id)
        .map(|&(_, _, code)| code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(lookup("d0", false), Some(Reg::D(0)));
        assert_eq!(lookup("a7", false), Some(Reg::A(7)));
        assert_eq!(lookup("sp", false), Some(Reg::A(7)));
        assert_eq!(lookup("fp", true), Some(Reg::A(6)));
        assert_eq!(lookup("fp", false), None);
        assert_eq!(lookup("fp3", false), Some(Reg::Fp(3)));
        assert_eq!(lookup("fp8", false), None);
        assert_eq!(lookup("d8", false), None);
        assert_eq!(lookup("a", false), None);
        assert_eq!(lookup("fpcr", false), Some(Reg::Ctl(rid::FPC)));
        assert_eq!(lookup("vbr", false), Some(Reg::Ctl(rid::VBR)));
        assert_eq!(lookup("asid", false), Some(Reg::Ctl(rid::TC)));
        assert_eq!(lookup("label", false), None);
        assert_eq!(Reg::A(3).index_bits(), Some(11));
    }

    #[test]
    fn tables_are_sorted() {
        assert!(NAMED.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(MOVEC_REGS.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn movec_codes() {
        use super::super::table::M68K_ARCHS;
        let ctrl = |name: &str| M68K_ARCHS.iter().find(|c| c.name == name).unwrap().ctrl;
        assert_eq!(movec(rid::VBR, ctrl("68010")), Some(0x801));
        assert_eq!(movec(rid::CACR, ctrl("68010")), None);
        assert_eq!(movec(rid::MMUSR, ctrl("68040")), Some(0x805));
        assert_eq!(movec(rid::PCR, ctrl("68060")), Some(0x808));
        assert_eq!(movec(rid::CAAR, ctrl("68060")), None);
    }
}
