//! Mnemonics, condition codes and the suffix grammar that joins them.
//!
//! An ARM mnemonic is not one word but three glued together: a base operation,
//! an optional `s` (update the flags) and an optional two-letter condition, in
//! that order — `add` + `s` + `eq` = `addseq`. Thumb adds a fourth part, the
//! `.n`/`.w` width hint. Peeling those apart is the ARM equivalent of the
//! x86 backend's AT&T size suffixes, and it has the same hazard: several base
//! mnemonics end in letters that also spell a suffix (`bics` ends in `cs`,
//! `movs` in `vs`, `bls` is `b` + `ls` and not `bl` + `s`). The rule that
//! resolves all of them is to accept a split only when what is left is a real
//! base mnemonic, and to try the splits in a fixed order.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Condition code encoding, `AL` when unconditional.
pub const AL: u8 = 14;

#[rustfmt::skip]
static CONDS: &[(&str, u8)] = &[
    ("eq", 0), ("ne", 1),
    ("cs", 2), ("hs", 2), ("cc", 3), ("lo", 3),
    ("mi", 4), ("pl", 5), ("vs", 6), ("vc", 7),
    ("hi", 8), ("ls", 9), ("ge", 10), ("lt", 11),
    ("gt", 12), ("le", 13), ("al", 14),
];

pub fn condition(name: &str) -> Option<u8> {
    CONDS.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

/// The usual spelling of a condition code, for diagnostics.
pub fn condition_name(cond: u8) -> &'static str {
    CONDS
        .iter()
        .find(|(n, c)| *c == cond && !matches!(*n, "hs" | "lo"))
        .map_or("nv", |(n, _)| n)
}

/// Direction and index-before/after mode of a block transfer.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct BlockMode {
    /// P: index before the transfer.
    pub before: bool,
    /// U: addresses increase.
    pub increment: bool,
}

/// Every operation this backend can encode, after suffix stripping.
///
/// The same enum serves both instruction sets; each encoder rejects what its
/// own set cannot express.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Mnem {
    // Data processing, in ARM opcode-field order so the encoder can use the
    // discriminant directly.
    And,
    Eor,
    Sub,
    Rsb,
    Add,
    Adc,
    Sbc,
    Rsc,
    Tst,
    Teq,
    Cmp,
    Cmn,
    Orr,
    Mov,
    Bic,
    Mvn,
    /// `orr` with the second operand complemented, which only Thumb has.
    Orn,
    /// `rsb rd, rm, #0` under a shorter name.
    Neg,
    // Shifts, which A32 encodes as forms of `mov`.
    Lsl,
    Lsr,
    Asr,
    Ror,
    Rrx,
    // Loads and stores.
    Ldr,
    Str,
    Ldrb,
    Strb,
    Ldrh,
    Strh,
    Ldrsb,
    Ldrsh,
    /// The doubleword transfers, which move a register pair.
    Ldrd,
    Strd,
    /// The `t` suffix: a transfer made with user-mode privileges, which is
    /// post-indexed whatever the source wrote.
    Ldrt,
    Strt,
    Ldrbt,
    Strbt,
    Ldrht,
    Strht,
    Ldrsbt,
    Ldrsht,
    /// The preloads, which are loads into no register at all.
    Pld,
    Pldw,
    Pli,
    Ldm(BlockMode),
    Stm(BlockMode),
    Push,
    Pop,
    // PC-relative addresses: `adr` is one instruction, `adrl` two.
    Adr,
    Adrl,
    // Branches.
    B,
    Bl,
    Bx,
    Blx,
    /// The Thumb compare-and-branch, which tests a register against zero.
    Cbz,
    Cbnz,
    // Multiplies.
    Mul,
    Mla,
    Mls,
    Umull,
    Umlal,
    Smull,
    Smlal,
    // Move-wide, and the Thumb `add`/`sub` restricted to a plain twelve-bit
    // immediate.
    Movw,
    Movt,
    Addw,
    Subw,
    // The status registers, whose operands are neither registers nor
    // immediates but field specifiers and banked register names.
    Mrs,
    Msr,
    /// `it`, `itt`, `ite` and so on: the letters after the first `t`, as the
    /// mask field for an even condition; see `thumb::it_block`.
    It(u8),
    /// An instruction from the generated table, held as the index of its
    /// first form in [`super::table::FORMS`]; `super::generic` encodes it.
    Ext(u16),
}

/// What a core load or store moves, and how, which is all that separates
/// the twenty spellings of one addressing mode.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Transfer {
    pub load: bool,
    /// How many bytes move: 1, 2, 4 or 8.
    pub size: u8,
    /// Whether a byte or halfword is sign-extended into the register.
    pub signed: bool,
    /// The `t` suffix: the transfer takes user-mode privileges, which makes
    /// it post-indexed however the source wrote the address.
    pub translate: bool,
}

const fn tr(load: bool, size: u8, signed: bool, translate: bool) -> Transfer {
    Transfer {
        load,
        size,
        signed,
        translate,
    }
}

impl Mnem {
    /// The transfer a load or store makes, for the mnemonics that are one.
    pub fn transfer(self) -> Option<Transfer> {
        use Mnem::*;
        Some(match self {
            Ldr => tr(true, 4, false, false),
            Str => tr(false, 4, false, false),
            Ldrb => tr(true, 1, false, false),
            Strb => tr(false, 1, false, false),
            Ldrh => tr(true, 2, false, false),
            Strh => tr(false, 2, false, false),
            Ldrsb => tr(true, 1, true, false),
            Ldrsh => tr(true, 2, true, false),
            Ldrd => tr(true, 8, false, false),
            Strd => tr(false, 8, false, false),
            Ldrt => tr(true, 4, false, true),
            Strt => tr(false, 4, false, true),
            Ldrbt => tr(true, 1, false, true),
            Strbt => tr(false, 1, false, true),
            Ldrht => tr(true, 2, false, true),
            Strht => tr(false, 2, false, true),
            Ldrsbt => tr(true, 1, true, true),
            Ldrsht => tr(true, 2, true, true),
            _ => return None,
        })
    }

    /// The 4-bit A32 data-processing opcode, for the sixteen operations that
    /// have one.
    pub fn dp_opcode(self) -> Option<u32> {
        use Mnem::*;
        Some(match self {
            And => 0,
            Eor => 1,
            Sub => 2,
            Rsb => 3,
            Add => 4,
            Adc => 5,
            Sbc => 6,
            Rsc => 7,
            Tst => 8,
            Teq => 9,
            Cmp => 10,
            Cmn => 11,
            Orr => 12,
            Mov => 13,
            Bic => 14,
            Mvn => 15,
            _ => return None,
        })
    }

    /// True for the comparisons, which have no destination and always set the
    /// flags.
    pub fn is_compare(self) -> bool {
        matches!(self, Mnem::Tst | Mnem::Teq | Mnem::Cmp | Mnem::Cmn)
    }

    /// True for `mov`/`mvn`, which take a destination but no first source.
    pub fn is_move(self) -> bool {
        matches!(self, Mnem::Mov | Mnem::Mvn)
    }

    /// The operation encoding the same value with the immediate complemented.
    ///
    /// ARM has no encoding for `add r0, r1, #-1`, but `sub r0, r1, #1` is the
    /// same instruction, and every assembler makes that substitution rather
    /// than rejecting the line. The `negate` flag says whether the partner
    /// wants the arithmetic negation or the bitwise complement.
    pub fn immediate_partner(self) -> Option<(Mnem, bool)> {
        use Mnem::*;
        Some(match self {
            Add => (Sub, true),
            Sub => (Add, true),
            Cmp => (Cmn, true),
            Cmn => (Cmp, true),
            And => (Bic, false),
            Bic => (And, false),
            Mov => (Mvn, false),
            Mvn => (Mov, false),
            Orr => (Orn, false),
            Orn => (Orr, false),
            Adc => (Sbc, false),
            Sbc => (Adc, false),
            _ => return None,
        })
    }

    /// True when an `s` suffix is meaningful. The comparisons already set the
    /// flags, and nothing else in this table has an S bit.
    pub fn allows_s(self) -> bool {
        use Mnem::*;
        self.dp_opcode().is_some() && !self.is_compare()
            || matches!(
                self,
                Lsl | Lsr | Asr | Ror | Rrx | Orn | Neg | Mul | Mla | Umull | Umlal | Smull | Smlal
            )
    }
}

#[rustfmt::skip]
fn table() -> &'static HashMap<&'static str, Mnem> {
    static T: OnceLock<HashMap<&'static str, Mnem>> = OnceLock::new();
    T.get_or_init(|| {
        use Mnem::*;
        let ia = BlockMode { before: false, increment: true };
        let ib = BlockMode { before: true, increment: true };
        let da = BlockMode { before: false, increment: false };
        let db = BlockMode { before: true, increment: false };
        let mut m: HashMap<&'static str, Mnem> = HashMap::new();
        let mut add = |n: &'static str, v: Mnem| { m.insert(n, v); };
        add("and", And); add("eor", Eor); add("sub", Sub); add("rsb", Rsb);
        add("add", Add); add("adc", Adc); add("sbc", Sbc); add("rsc", Rsc);
        add("tst", Tst); add("teq", Teq); add("cmp", Cmp); add("cmn", Cmn);
        add("orr", Orr); add("mov", Mov); add("bic", Bic); add("mvn", Mvn);
        add("orn", Orn); add("neg", Neg);
        add("lsl", Lsl); add("lsr", Lsr); add("asr", Asr); add("ror", Ror);
        add("rrx", Rrx);
        add("ldr", Ldr); add("str", Str);
        add("ldrb", Ldrb); add("strb", Strb);
        add("ldrh", Ldrh); add("strh", Strh);
        add("ldrsb", Ldrsb); add("ldrsh", Ldrsh);
        add("ldrd", Ldrd); add("strd", Strd);
        add("ldrt", Ldrt); add("strt", Strt);
        add("ldrbt", Ldrbt); add("strbt", Strbt);
        add("ldrht", Ldrht); add("strht", Strht);
        add("ldrsbt", Ldrsbt); add("ldrsht", Ldrsht);
        add("pld", Pld); add("pldw", Pldw); add("pli", Pli);
        // The stack-oriented spellings are the same instructions: a full
        // descending stack pushes with `stmdb` and pops with `ldmia`.
        add("ldm", Ldm(ia)); add("ldmia", Ldm(ia)); add("ldmfd", Ldm(ia));
        add("ldmib", Ldm(ib)); add("ldmed", Ldm(ib));
        add("ldmda", Ldm(da)); add("ldmfa", Ldm(da));
        add("ldmdb", Ldm(db)); add("ldmea", Ldm(db));
        add("stm", Stm(ia)); add("stmia", Stm(ia)); add("stmea", Stm(ia));
        add("stmib", Stm(ib)); add("stmfa", Stm(ib));
        add("stmda", Stm(da)); add("stmed", Stm(da));
        add("stmdb", Stm(db)); add("stmfd", Stm(db));
        add("push", Push); add("pop", Pop);
        add("adr", Adr); add("adrl", Adrl);
        add("b", B); add("bl", Bl); add("bx", Bx); add("blx", Blx);
        add("cbz", Cbz); add("cbnz", Cbnz);
        add("mul", Mul); add("mla", Mla); add("mls", Mls);
        add("umull", Umull); add("umlal", Umlal);
        add("smull", Smull); add("smlal", Smlal);
        add("movw", Movw); add("movt", Movt);
        add("addw", Addw); add("subw", Subw);
        add("mrs", Mrs); add("msr", Msr);

        // Every `it` spelling: up to three more instructions, each `t` (the
        // condition, a clear bit) or `e` (its inverse, a set one), from the
        // top bit down, then a set bit that ends them.
        add("it", It(0x8));
        add("itt", It(0x4)); add("ite", It(0xc));
        add("ittt", It(0x2)); add("itte", It(0x6));
        add("itet", It(0xa)); add("itee", It(0xe));
        add("itttt", It(0x1)); add("ittte", It(0x3));
        add("ittet", It(0x5)); add("ittee", It(0x7));
        add("itett", It(0x9)); add("itete", It(0xb));
        add("iteet", It(0xd)); add("iteee", It(0xf));
        // Everything the generated table holds, under every spelling GNU as
        // takes for it. A hand-written mnemonic always wins, so a name in
        // both -- `rev` in Thumb, say -- keeps the encoder that knows the
        // width rules.
        for (i, f) in super::table::FORMS.iter().enumerate() {
            m.entry(f.name).or_insert(Ext(i as u16));
        }
        for (spelling, real) in super::table::SPELLINGS {
            if let Some(v) = m.get(real).copied() {
                m.entry(*spelling).or_insert(v);
            }
        }
        m
    })
}

/// The operation this name spells, with the table's own spelling of it,
/// which outlives the text that was looked up and is the key the
/// instruction's feature set is read by.
fn lookup(name: &str) -> Option<(&'static str, Mnem)> {
    table().get_key_value(name).map(|(k, v)| (*k, *v))
}

/// Whether a mnemonic takes a data type that means nothing to its encoding:
/// the width of a VFP transfer is the width of the register it names, so
/// `vldr.64 d0, [r0]`, `vldr.32 d0, [r0]` and `vldr d0, [r0]` are all the
/// same instruction. `parse_neon_type` reads the type for every `v`
/// mnemonic and these ones never consult it.
fn ignores_type(base: &str) -> bool {
    matches!(base, "vldr" | "vstr" | "vpush" | "vpop")
        || base
            .strip_prefix("vldm")
            .or_else(|| base.strip_prefix("vstm"))
            .is_some_and(|rest| matches!(rest, "" | "ia" | "db"))
}

/// Whether every dotted word of `types` is one GNU as's `parse_neon_type`
/// reads: a size of 8, 16, 32 or 64, with an optional kind letter in front
/// of it, or one of the shorthands `d`, `f` and `bf16`.
fn neon_types(types: &str) -> bool {
    types.split('.').all(|t| {
        if matches!(t, "d" | "f" | "bf16") {
            return true;
        }
        let size = t.strip_prefix(['i', 'f', 'p', 's', 'u']).unwrap_or(t);
        matches!(size, "8" | "16" | "32" | "64")
    })
}

/// Requested instruction width, from a Thumb `.n` / `.w` suffix.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Width {
    Any,
    /// `.n`: the caller insists on a 16-bit encoding.
    Narrow,
    /// `.w`: the caller insists on a 32-bit encoding.
    Wide,
}

/// A mnemonic split into its parts.
#[derive(Copy, Clone, Debug)]
pub struct Resolved {
    pub mnem: Mnem,
    pub cond: u8,
    /// Whether a condition was written, as opposed to defaulting to `al`.
    pub cond_written: bool,
    pub set_flags: bool,
    pub width: Width,
    /// The spelling the mnemonic was found under, which is the key
    /// [`super::cpu::mnemonic_feats`] reads the instruction's feature set
    /// by; the condition, the `s` and the width hint are not part of it.
    pub stem: &'static str,
}

/// Splits `text` into base mnemonic, `s` flag, condition and width.
///
/// Returns `None` if no split yields a known mnemonic, which the caller
/// reports as an unknown instruction.
pub fn resolve(text: &str) -> Option<Resolved> {
    // The width hint is the last dotted part; everything from the first dot
    // is the data type a vector instruction carries, which is part of its
    // name: `vcvt` + `eq` + `.f32.u32`.
    // A mnemonic that carries a data type has no width hint: GNU as reads
    // the whole tail as the type, so `vadd.i8.w` is not an instruction.
    let (head, width) = match text.rsplit_once('.') {
        Some((h, "n")) if !h.is_empty() && !h.contains('.') => (h, Width::Narrow),
        Some((h, "w")) if !h.is_empty() && !h.contains('.') => (h, Width::Wide),
        _ => (text, Width::Any),
    };
    let (stem, types) = match head.split_once('.') {
        Some((s, t)) => (s, Some(t)),
        None => (head, None),
    };

    let mk = |(stem, mnem): (&'static str, Mnem), cond: u8, cond_written: bool, set_flags: bool| {
        Resolved {
            mnem,
            cond,
            cond_written,
            set_flags,
            width,
            stem,
        }
    };
    // The name to look up is the stem with the type suffix put back on --
    // unless the instruction is one of those whose type GNU as parses and
    // then never looks at, where the register says the width instead.
    let named = |base: &str| match types {
        None => lookup(base),
        Some(t) => lookup(&format!("{base}.{t}")).or_else(|| {
            if ignores_type(base) && neon_types(t) {
                lookup(base)
            } else {
                None
            }
        }),
    };

    // An exact match always wins, so `bl`, `mrs` and `mls` are never taken
    // apart into a shorter mnemonic plus a suffix.
    if let Some(m) = named(stem) {
        return Some(mk(m, AL, false, false));
    }

    // Condition first: `bls` is `b` + `ls`, not `bl` + `s`. The base may still
    // carry an `s`, giving the UAL order `add` + `s` + `eq`. The split is
    // checked because the mnemonic is user text and need not be ASCII.
    if let Some((base, suffix)) = stem
        .len()
        .checked_sub(2)
        .and_then(|at| stem.split_at_checked(at))
        && !base.is_empty()
        && let Some(cond) = condition(suffix)
    {
        if let Some(m) = named(base) {
            return Some(mk(m, cond, true, false));
        }
        if let Some(base) = base.strip_suffix('s')
            && let Some(m) = named(base)
            && m.1.allows_s()
        {
            return Some(mk(m, cond, true, true));
        }
    }

    // A bare `s`: `movs`, `bics`, `adds`.
    if let Some(base) = stem.strip_suffix('s')
        && let Some(m) = named(base)
        && m.1.allows_s()
    {
        return Some(mk(m, AL, false, true));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(text: &str) -> Option<(Mnem, u8, bool)> {
        resolve(text).map(|r| (r.mnem, r.cond, r.set_flags))
    }

    #[test]
    fn suffixes_that_spell_other_mnemonics_resolve_correctly() {
        // `bls` is a conditional `b`, not a flag-setting `bl`.
        assert_eq!(split("bls"), Some((Mnem::B, 9, false)));
        assert_eq!(split("bl"), Some((Mnem::Bl, AL, false)));
        assert_eq!(split("blls"), Some((Mnem::Bl, 9, false)));
        // `bics` ends in the condition `cs` and `movs` in `vs`.
        assert_eq!(split("bics"), Some((Mnem::Bic, AL, true)));
        assert_eq!(split("movs"), Some((Mnem::Mov, AL, true)));
        assert_eq!(split("smlals"), Some((Mnem::Smlal, AL, true)));
        // An exact entry is never split: `mls` is not `ml` + `s`.
        assert_eq!(split("mls"), Some((Mnem::Mls, AL, false)));
        assert_eq!(split("addseq"), Some((Mnem::Add, 0, true)));
        assert_eq!(split("ldrhs"), Some((Mnem::Ldr, 2, false)));
    }

    #[test]
    fn meaningless_suffixes_are_rejected() {
        // The pre-UAL order, which LLVM also rejects.
        assert!(resolve("addeqs").is_none());
        // Nothing to set flags on.
        assert!(resolve("cmps").is_none());
        assert!(resolve("revs").is_none());
        assert!(resolve("add.q").is_none());
        // Not ASCII, so the two-byte condition split lands inside a character.
        assert!(resolve("\u{e9}eq").is_none());
        assert!(resolve("a\u{e9}").is_none());
    }
}
