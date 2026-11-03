//! The SuperH opcode table.
//!
//! Every instruction is one 16-bit word. An entry gives the word with its
//! operand fields zero and says, operand by operand, what is accepted and
//! which bits it fills. A mnemonic has as many entries as it has addressing
//! modes; they are tried in order and the first whose operands all match
//! wins, which is how GNU as picks among them too.
//!
//! Field positions are named by where they go, not by the source/destination
//! role: `n` is bits 11-8 and `m` is bits 7-4. That is how the manuals draw
//! the words (`0110nnnnmmmm0011` is `mov rm,rn`), but it means `lds rm,fpul`
//! uses an `N` argument, since its register sits in bits 11-8.

use super::cpu;
use super::reg::Ctl;

/// What one operand slot accepts, and where it goes.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Arg {
    /// `rn`, bits 11-8.
    RegN,
    /// `rm`, bits 7-4.
    RegM,
    /// Exactly `r0`; no bits.
    R0,
    /// `#imm`, bits 7-0.
    Imm8,
    /// `@rn` / `@rm`.
    IndN,
    IndM,
    /// `@rn+` / `@rm+`.
    IncN,
    IncM,
    /// `@-rn` / `@-rm`.
    DecN,
    #[allow(dead_code)]
    DecM,
    /// `@(r0,rn)` / `@(r0,rm)`.
    R0IdxN,
    R0IdxM,
    /// `@(disp,rn)` / `@(disp,rm)`: register plus a four-bit displacement in
    /// bits 3-0, counted in units of the operand size.
    DispN(u8),
    DispM(u8),
    /// `@(disp,gbr)`: an eight-bit displacement in bits 7-0, scaled likewise.
    GbrDisp(u8),
    /// `@(r0,gbr)`.
    R0Gbr,
    /// A PC-relative data address, bits 7-0, scaled by the operand size.
    PcRel(u8),
    /// A branch target: eight or twelve bits of signed word displacement.
    Branch8,
    Branch12,
    /// A named control register; no bits.
    Ctl(Ctl),
    /// `rN_bank`, bits 7-4 as `1nnn`.
    BankM,
    /// `frn` / `frm`.
    FrN,
    FrM,
    /// Exactly `fr0`, as `fmac` takes it.
    Fr0,
    /// `drn` / `drm`, written with the number of their first `fr` register.
    DrN,
    DrM,
    /// `fvn` in bits 11-8 as `nn00`, `fvm` as `00mm`: `fipr` packs both
    /// vectors into one nibble.
    FvN,
    FvM,
    Xmtrx,
}

impl Arg {
    /// How the operand is written, for listing an instruction's forms.
    pub fn spelling(self) -> String {
        match self {
            Arg::RegN => "rn".into(),
            Arg::RegM => "rm".into(),
            Arg::R0 => "r0".into(),
            Arg::Imm8 => "#imm".into(),
            Arg::IndN => "@rn".into(),
            Arg::IndM => "@rm".into(),
            Arg::IncN => "@rn+".into(),
            Arg::IncM => "@rm+".into(),
            Arg::DecN => "@-rn".into(),
            Arg::DecM => "@-rm".into(),
            Arg::R0IdxN => "@(r0,rn)".into(),
            Arg::R0IdxM => "@(r0,rm)".into(),
            Arg::DispN(_) => "@(disp,rn)".into(),
            Arg::DispM(_) => "@(disp,rm)".into(),
            Arg::GbrDisp(_) => "@(disp,gbr)".into(),
            Arg::R0Gbr => "@(r0,gbr)".into(),
            Arg::PcRel(_) => "label".into(),
            Arg::Branch8 | Arg::Branch12 => "label".into(),
            Arg::Ctl(c) => c.name().into(),
            Arg::BankM => "rN_bank".into(),
            Arg::FrN => "frn".into(),
            Arg::FrM => "frm".into(),
            Arg::Fr0 => "fr0".into(),
            Arg::DrN => "drn".into(),
            Arg::DrM => "drm".into(),
            Arg::FvN => "fvn".into(),
            Arg::FvM => "fvm".into(),
            Arg::Xmtrx => "xmtrx".into(),
        }
    }
}

pub struct Entry {
    pub name: &'static str,
    pub args: &'static [Arg],
    pub word: u16,
    /// The CPUs that have this form: its tag in GNU as's `opcodes/sh-opc.h`,
    /// as a [`cpu`] set.
    pub arch: u32,
}

use Arg::*;
use Ctl::{Dbr, Fpscr, Fpul, Gbr, Mach, Macl, Pr, Sgr, Spc, Sr, Ssr, Vbr};
use cpu::{
    SH_UP, SH2_UP, SH2A_NOFPU_OR_SH3_NOMMU_UP, SH2A_OR_SH3E_UP, SH2A_OR_SH4_UP, SH2E_UP,
    SH3_NOMMU_UP, SH3_UP, SH4_NOMMU_NOFPU_UP, SH4_UP, SH4A_NOFPU_UP, SH4A_UP,
};

const fn e(name: &'static str, args: &'static [Arg], word: u16, arch: u32) -> Entry {
    Entry {
        name,
        args,
        word,
        arch,
    }
}

/// Every instruction this backend assembles, grouped by mnemonic.
///
/// The order within a mnemonic matters only where two entries could accept
/// the same operands, and none here can. Across mnemonics it does not matter.
///
/// The last column is the form's tag in GNU as's `opcodes/sh-opc.h`: the
/// CPUs that have it. It decides both what a CPU name such as `sh2e` accepts
/// and the object's `e_flags`, so it follows GNU's table exactly, down to
/// `fsqrt frn` being SH-3E and up where the rest of the single-precision FPU
/// is SH-2E and up. Where GNU also has an SH-2A 32-bit form with the same
/// operands, as for `mov.l @(disp,rm),rn`, the 16-bit form comes first there
/// and is picked for every displacement its field holds, so its tag is the
/// one here.
#[rustfmt::skip]
pub static TABLE: &[Entry] = &[
    // ---- data transfer ----------------------------------------------------
    e("mov", &[Imm8, RegN], 0xe000, SH_UP),
    e("mov", &[RegM, RegN], 0x6003, SH_UP),

    e("mov.b", &[RegM, IndN], 0x2000, SH_UP),
    e("mov.b", &[RegM, DecN], 0x2004, SH_UP),
    e("mov.b", &[RegM, R0IdxN], 0x0004, SH_UP),
    e("mov.b", &[R0, DispM(1)], 0x8000, SH_UP),
    e("mov.b", &[R0, GbrDisp(1)], 0xc000, SH_UP),
    e("mov.b", &[IndM, RegN], 0x6000, SH_UP),
    e("mov.b", &[IncM, RegN], 0x6004, SH_UP),
    e("mov.b", &[R0IdxM, RegN], 0x000c, SH_UP),
    e("mov.b", &[DispM(1), R0], 0x8400, SH_UP),
    e("mov.b", &[GbrDisp(1), R0], 0xc400, SH_UP),

    e("mov.w", &[RegM, IndN], 0x2001, SH_UP),
    e("mov.w", &[RegM, DecN], 0x2005, SH_UP),
    e("mov.w", &[RegM, R0IdxN], 0x0005, SH_UP),
    e("mov.w", &[R0, DispM(2)], 0x8100, SH_UP),
    e("mov.w", &[R0, GbrDisp(2)], 0xc100, SH_UP),
    e("mov.w", &[IndM, RegN], 0x6001, SH_UP),
    e("mov.w", &[IncM, RegN], 0x6005, SH_UP),
    e("mov.w", &[R0IdxM, RegN], 0x000d, SH_UP),
    e("mov.w", &[DispM(2), R0], 0x8500, SH_UP),
    e("mov.w", &[GbrDisp(2), R0], 0xc500, SH_UP),
    e("mov.w", &[PcRel(2), RegN], 0x9000, SH_UP),

    e("mov.l", &[RegM, IndN], 0x2002, SH_UP),
    e("mov.l", &[RegM, DecN], 0x2006, SH_UP),
    e("mov.l", &[RegM, R0IdxN], 0x0006, SH_UP),
    e("mov.l", &[RegM, DispN(4)], 0x1000, SH_UP),
    e("mov.l", &[R0, GbrDisp(4)], 0xc200, SH_UP),
    e("mov.l", &[IndM, RegN], 0x6002, SH_UP),
    e("mov.l", &[IncM, RegN], 0x6006, SH_UP),
    e("mov.l", &[R0IdxM, RegN], 0x000e, SH_UP),
    e("mov.l", &[DispM(4), RegN], 0x5000, SH_UP),
    e("mov.l", &[GbrDisp(4), R0], 0xc600, SH_UP),
    e("mov.l", &[PcRel(4), RegN], 0xd000, SH_UP),

    e("mova", &[PcRel(4), R0], 0xc700, SH_UP),
    e("movt", &[RegN], 0x0029, SH_UP),
    e("swap.b", &[RegM, RegN], 0x6008, SH_UP),
    e("swap.w", &[RegM, RegN], 0x6009, SH_UP),
    e("xtrct", &[RegM, RegN], 0x200d, SH_UP),
    e("movca.l", &[R0, IndN], 0x00c3, SH4_NOMMU_NOFPU_UP),
    e("movli.l", &[IndN, R0], 0x0063, SH4A_NOFPU_UP),
    e("movco.l", &[R0, IndN], 0x0073, SH4A_NOFPU_UP),
    e("movua.l", &[IndN, R0], 0x40a9, SH4A_NOFPU_UP),
    e("movua.l", &[IncN, R0], 0x40e9, SH4A_NOFPU_UP),

    // ---- arithmetic ---------------------------------------------------------
    e("add", &[Imm8, RegN], 0x7000, SH_UP),
    e("add", &[RegM, RegN], 0x300c, SH_UP),
    e("addc", &[RegM, RegN], 0x300e, SH_UP),
    e("addv", &[RegM, RegN], 0x300f, SH_UP),
    e("sub", &[RegM, RegN], 0x3008, SH_UP),
    e("subc", &[RegM, RegN], 0x300a, SH_UP),
    e("subv", &[RegM, RegN], 0x300b, SH_UP),
    e("cmp/eq", &[Imm8, R0], 0x8800, SH_UP),
    e("cmp/eq", &[RegM, RegN], 0x3000, SH_UP),
    e("cmp/hs", &[RegM, RegN], 0x3002, SH_UP),
    e("cmp/ge", &[RegM, RegN], 0x3003, SH_UP),
    e("cmp/hi", &[RegM, RegN], 0x3006, SH_UP),
    e("cmp/gt", &[RegM, RegN], 0x3007, SH_UP),
    e("cmp/pz", &[RegN], 0x4011, SH_UP),
    e("cmp/pl", &[RegN], 0x4015, SH_UP),
    e("cmp/str", &[RegM, RegN], 0x200c, SH_UP),
    e("div0s", &[RegM, RegN], 0x2007, SH_UP),
    e("div0u", &[], 0x0019, SH_UP),
    e("div1", &[RegM, RegN], 0x3004, SH_UP),
    e("dmuls.l", &[RegM, RegN], 0x300d, SH2_UP),
    e("dmulu.l", &[RegM, RegN], 0x3005, SH2_UP),
    e("mul.l", &[RegM, RegN], 0x0007, SH2_UP),
    e("muls.w", &[RegM, RegN], 0x200f, SH_UP),
    e("muls", &[RegM, RegN], 0x200f, SH_UP),
    e("mulu.w", &[RegM, RegN], 0x200e, SH_UP),
    e("mulu", &[RegM, RegN], 0x200e, SH_UP),
    e("mac.w", &[IncM, IncN], 0x400f, SH_UP),
    e("mac.l", &[IncM, IncN], 0x000f, SH2_UP),
    e("neg", &[RegM, RegN], 0x600b, SH_UP),
    e("negc", &[RegM, RegN], 0x600a, SH_UP),
    e("dt", &[RegN], 0x4010, SH2_UP),
    e("exts.b", &[RegM, RegN], 0x600e, SH_UP),
    e("exts.w", &[RegM, RegN], 0x600f, SH_UP),
    e("extu.b", &[RegM, RegN], 0x600c, SH_UP),
    e("extu.w", &[RegM, RegN], 0x600d, SH_UP),

    // ---- logic --------------------------------------------------------------
    e("and", &[Imm8, R0], 0xc900, SH_UP),
    e("and", &[RegM, RegN], 0x2009, SH_UP),
    e("and.b", &[Imm8, R0Gbr], 0xcd00, SH_UP),
    e("or", &[Imm8, R0], 0xcb00, SH_UP),
    e("or", &[RegM, RegN], 0x200b, SH_UP),
    e("or.b", &[Imm8, R0Gbr], 0xcf00, SH_UP),
    e("xor", &[Imm8, R0], 0xca00, SH_UP),
    e("xor", &[RegM, RegN], 0x200a, SH_UP),
    e("xor.b", &[Imm8, R0Gbr], 0xce00, SH_UP),
    e("tst", &[Imm8, R0], 0xc800, SH_UP),
    e("tst", &[RegM, RegN], 0x2008, SH_UP),
    e("tst.b", &[Imm8, R0Gbr], 0xcc00, SH_UP),
    e("not", &[RegM, RegN], 0x6007, SH_UP),
    e("tas.b", &[IndN], 0x401b, SH_UP),

    // ---- shifts -------------------------------------------------------------
    e("shal", &[RegN], 0x4020, SH_UP),
    e("shar", &[RegN], 0x4021, SH_UP),
    e("shll", &[RegN], 0x4000, SH_UP),
    e("shlr", &[RegN], 0x4001, SH_UP),
    e("shll2", &[RegN], 0x4008, SH_UP),
    e("shlr2", &[RegN], 0x4009, SH_UP),
    e("shll8", &[RegN], 0x4018, SH_UP),
    e("shlr8", &[RegN], 0x4019, SH_UP),
    e("shll16", &[RegN], 0x4028, SH_UP),
    e("shlr16", &[RegN], 0x4029, SH_UP),
    e("rotl", &[RegN], 0x4004, SH_UP),
    e("rotr", &[RegN], 0x4005, SH_UP),
    e("rotcl", &[RegN], 0x4024, SH_UP),
    e("rotcr", &[RegN], 0x4025, SH_UP),
    e("shad", &[RegM, RegN], 0x400c, SH2A_NOFPU_OR_SH3_NOMMU_UP),
    e("shld", &[RegM, RegN], 0x400d, SH2A_NOFPU_OR_SH3_NOMMU_UP),

    // ---- branches -----------------------------------------------------------
    // The conditional branches are handled before the table is consulted,
    // since they may relax; their entries are here for the words and levels.
    e("bt", &[Branch8], 0x8900, SH_UP),
    e("bf", &[Branch8], 0x8b00, SH_UP),
    e("bt/s", &[Branch8], 0x8d00, SH2_UP),
    e("bt.s", &[Branch8], 0x8d00, SH2_UP),
    e("bf/s", &[Branch8], 0x8f00, SH2_UP),
    e("bf.s", &[Branch8], 0x8f00, SH2_UP),
    e("bra", &[Branch12], 0xa000, SH_UP),
    e("bsr", &[Branch12], 0xb000, SH_UP),
    e("braf", &[RegN], 0x0023, SH2_UP),
    e("bsrf", &[RegN], 0x0003, SH2_UP),
    e("jmp", &[IndN], 0x402b, SH_UP),
    e("jsr", &[IndN], 0x400b, SH_UP),
    e("rts", &[], 0x000b, SH_UP),
    e("rte", &[], 0x002b, SH_UP),
    e("trapa", &[Imm8], 0xc300, SH_UP),

    // ---- system -------------------------------------------------------------
    e("nop", &[], 0x0009, SH_UP),
    e("sleep", &[], 0x001b, SH_UP),
    e("clrmac", &[], 0x0028, SH_UP),
    e("clrt", &[], 0x0008, SH_UP),
    e("sett", &[], 0x0018, SH_UP),
    e("clrs", &[], 0x0048, SH3_NOMMU_UP),
    e("sets", &[], 0x0058, SH3_NOMMU_UP),
    e("ldtlb", &[], 0x0038, SH3_UP),
    e("pref", &[IndN], 0x0083, SH2A_NOFPU_OR_SH3_NOMMU_UP),
    e("ocbi", &[IndN], 0x0093, SH4_NOMMU_NOFPU_UP),
    e("ocbp", &[IndN], 0x00a3, SH4_NOMMU_NOFPU_UP),
    e("ocbwb", &[IndN], 0x00b3, SH4_NOMMU_NOFPU_UP),
    e("icbi", &[IndN], 0x00e3, SH4A_NOFPU_UP),
    e("prefi", &[IndN], 0x00d3, SH4A_NOFPU_UP),
    e("synco", &[], 0x00ab, SH4A_NOFPU_UP),

    e("ldc", &[RegN, Ctl(Sr)], 0x400e, SH_UP),
    e("ldc", &[RegN, Ctl(Gbr)], 0x401e, SH_UP),
    e("ldc", &[RegN, Ctl(Vbr)], 0x402e, SH_UP),
    e("ldc", &[RegN, Ctl(Ssr)], 0x403e, SH3_NOMMU_UP),
    e("ldc", &[RegN, Ctl(Spc)], 0x404e, SH3_NOMMU_UP),
    e("ldc", &[RegN, Ctl(Sgr)], 0x403a, SH4_NOMMU_NOFPU_UP),
    e("ldc", &[RegN, Ctl(Dbr)], 0x40fa, SH4_NOMMU_NOFPU_UP),
    e("ldc", &[RegN, BankM], 0x408e, SH3_NOMMU_UP),
    e("ldc.l", &[IncN, Ctl(Sr)], 0x4007, SH_UP),
    e("ldc.l", &[IncN, Ctl(Gbr)], 0x4017, SH_UP),
    e("ldc.l", &[IncN, Ctl(Vbr)], 0x4027, SH_UP),
    e("ldc.l", &[IncN, Ctl(Ssr)], 0x4037, SH3_NOMMU_UP),
    e("ldc.l", &[IncN, Ctl(Spc)], 0x4047, SH3_NOMMU_UP),
    e("ldc.l", &[IncN, Ctl(Sgr)], 0x4036, SH4_NOMMU_NOFPU_UP),
    e("ldc.l", &[IncN, Ctl(Dbr)], 0x40f6, SH4_NOMMU_NOFPU_UP),
    e("ldc.l", &[IncN, BankM], 0x4087, SH3_NOMMU_UP),
    e("stc", &[Ctl(Sr), RegN], 0x0002, SH_UP),
    e("stc", &[Ctl(Gbr), RegN], 0x0012, SH_UP),
    e("stc", &[Ctl(Vbr), RegN], 0x0022, SH_UP),
    e("stc", &[Ctl(Ssr), RegN], 0x0032, SH3_NOMMU_UP),
    e("stc", &[Ctl(Spc), RegN], 0x0042, SH3_NOMMU_UP),
    e("stc", &[Ctl(Sgr), RegN], 0x003a, SH4_NOMMU_NOFPU_UP),
    e("stc", &[Ctl(Dbr), RegN], 0x00fa, SH4_NOMMU_NOFPU_UP),
    e("stc", &[BankM, RegN], 0x0082, SH3_NOMMU_UP),
    e("stc.l", &[Ctl(Sr), DecN], 0x4003, SH_UP),
    e("stc.l", &[Ctl(Gbr), DecN], 0x4013, SH_UP),
    e("stc.l", &[Ctl(Vbr), DecN], 0x4023, SH_UP),
    e("stc.l", &[Ctl(Ssr), DecN], 0x4033, SH3_NOMMU_UP),
    e("stc.l", &[Ctl(Spc), DecN], 0x4043, SH3_NOMMU_UP),
    e("stc.l", &[Ctl(Sgr), DecN], 0x4032, SH4_NOMMU_NOFPU_UP),
    e("stc.l", &[Ctl(Dbr), DecN], 0x40f2, SH4_NOMMU_NOFPU_UP),
    e("stc.l", &[BankM, DecN], 0x4083, SH3_NOMMU_UP),
    e("lds", &[RegN, Ctl(Mach)], 0x400a, SH_UP),
    e("lds", &[RegN, Ctl(Macl)], 0x401a, SH_UP),
    e("lds", &[RegN, Ctl(Pr)], 0x402a, SH_UP),
    e("lds", &[RegN, Ctl(Fpul)], 0x405a, SH2E_UP),
    e("lds", &[RegN, Ctl(Fpscr)], 0x406a, SH2E_UP),
    e("lds.l", &[IncN, Ctl(Mach)], 0x4006, SH_UP),
    e("lds.l", &[IncN, Ctl(Macl)], 0x4016, SH_UP),
    e("lds.l", &[IncN, Ctl(Pr)], 0x4026, SH_UP),
    e("lds.l", &[IncN, Ctl(Fpul)], 0x4056, SH2E_UP),
    e("lds.l", &[IncN, Ctl(Fpscr)], 0x4066, SH2E_UP),
    e("sts", &[Ctl(Mach), RegN], 0x000a, SH_UP),
    e("sts", &[Ctl(Macl), RegN], 0x001a, SH_UP),
    e("sts", &[Ctl(Pr), RegN], 0x002a, SH_UP),
    e("sts", &[Ctl(Fpul), RegN], 0x005a, SH2E_UP),
    e("sts", &[Ctl(Fpscr), RegN], 0x006a, SH2E_UP),
    e("sts.l", &[Ctl(Mach), DecN], 0x4002, SH_UP),
    e("sts.l", &[Ctl(Macl), DecN], 0x4012, SH_UP),
    e("sts.l", &[Ctl(Pr), DecN], 0x4022, SH_UP),
    e("sts.l", &[Ctl(Fpul), DecN], 0x4052, SH2E_UP),
    e("sts.l", &[Ctl(Fpscr), DecN], 0x4062, SH2E_UP),

    // ---- floating point -----------------------------------------------------
    // A `dr` operand is written with the number of its first `fr` register,
    // so `fadd dr2,dr4` is the same word as `fadd fr2,fr4`; which one the
    // hardware does depends on FPSCR.PR, not on the opcode.
    e("fabs", &[FrN], 0xf05d, SH2E_UP),
    e("fabs", &[DrN], 0xf05d, SH2A_OR_SH4_UP),
    e("fadd", &[FrM, FrN], 0xf000, SH2E_UP),
    e("fadd", &[DrM, DrN], 0xf000, SH2A_OR_SH4_UP),
    e("fsub", &[FrM, FrN], 0xf001, SH2E_UP),
    e("fsub", &[DrM, DrN], 0xf001, SH2A_OR_SH4_UP),
    e("fmul", &[FrM, FrN], 0xf002, SH2E_UP),
    e("fmul", &[DrM, DrN], 0xf002, SH2A_OR_SH4_UP),
    e("fdiv", &[FrM, FrN], 0xf003, SH2E_UP),
    e("fdiv", &[DrM, DrN], 0xf003, SH2A_OR_SH4_UP),
    e("fcmp/eq", &[FrM, FrN], 0xf004, SH2E_UP),
    e("fcmp/eq", &[DrM, DrN], 0xf004, SH2A_OR_SH4_UP),
    e("fcmp/gt", &[FrM, FrN], 0xf005, SH2E_UP),
    e("fcmp/gt", &[DrM, DrN], 0xf005, SH2A_OR_SH4_UP),
    e("fneg", &[FrN], 0xf04d, SH2E_UP),
    e("fneg", &[DrN], 0xf04d, SH2A_OR_SH4_UP),
    e("fsqrt", &[FrN], 0xf06d, SH2A_OR_SH3E_UP),
    e("fsqrt", &[DrN], 0xf06d, SH2A_OR_SH4_UP),
    e("fldi0", &[FrN], 0xf08d, SH2E_UP),
    e("fldi1", &[FrN], 0xf09d, SH2E_UP),
    e("flds", &[FrN, Ctl(Fpul)], 0xf01d, SH2E_UP),
    e("fsts", &[Ctl(Fpul), FrN], 0xf00d, SH2E_UP),
    e("float", &[Ctl(Fpul), FrN], 0xf02d, SH2E_UP),
    e("float", &[Ctl(Fpul), DrN], 0xf02d, SH2A_OR_SH4_UP),
    e("ftrc", &[FrN, Ctl(Fpul)], 0xf03d, SH2E_UP),
    e("ftrc", &[DrN, Ctl(Fpul)], 0xf03d, SH2A_OR_SH4_UP),
    e("fcnvds", &[DrN, Ctl(Fpul)], 0xf0bd, SH2A_OR_SH4_UP),
    e("fcnvsd", &[Ctl(Fpul), DrN], 0xf0ad, SH2A_OR_SH4_UP),
    e("fmac", &[Fr0, FrM, FrN], 0xf00e, SH2E_UP),
    e("fsca", &[Ctl(Fpul), DrN], 0xf0fd, SH4_UP),
    e("fsrra", &[FrN], 0xf07d, SH4_UP),
    e("fipr", &[FvM, FvN], 0xf0ed, SH4_UP),
    e("ftrv", &[Xmtrx, FvN], 0xf1fd, SH4_UP),
    e("frchg", &[], 0xfbfd, SH4_UP),
    e("fschg", &[], 0xf3fd, SH2A_OR_SH4_UP),
    e("fpchg", &[], 0xf7fd, SH4A_UP),

    e("fmov", &[FrM, FrN], 0xf00c, SH2E_UP),
    e("fmov", &[IndM, FrN], 0xf008, SH2E_UP),
    e("fmov", &[FrM, IndN], 0xf00a, SH2E_UP),
    e("fmov", &[IncM, FrN], 0xf009, SH2E_UP),
    e("fmov", &[FrM, DecN], 0xf00b, SH2E_UP),
    e("fmov", &[R0IdxM, FrN], 0xf006, SH2E_UP),
    e("fmov", &[FrM, R0IdxN], 0xf007, SH2E_UP),
    e("fmov", &[DrM, DrN], 0xf00c, SH2A_OR_SH4_UP),
    e("fmov", &[IndM, DrN], 0xf008, SH2A_OR_SH4_UP),
    e("fmov", &[DrM, IndN], 0xf00a, SH2A_OR_SH4_UP),
    e("fmov", &[IncM, DrN], 0xf009, SH2A_OR_SH4_UP),
    e("fmov", &[DrM, DecN], 0xf00b, SH2A_OR_SH4_UP),
    e("fmov", &[R0IdxM, DrN], 0xf006, SH2A_OR_SH4_UP),
    e("fmov", &[DrM, R0IdxN], 0xf007, SH2A_OR_SH4_UP),
    e("fmov.s", &[IndM, FrN], 0xf008, SH2E_UP),
    e("fmov.s", &[FrM, IndN], 0xf00a, SH2E_UP),
    e("fmov.s", &[IncM, FrN], 0xf009, SH2E_UP),
    e("fmov.s", &[FrM, DecN], 0xf00b, SH2E_UP),
    e("fmov.s", &[R0IdxM, FrN], 0xf006, SH2E_UP),
    e("fmov.s", &[FrM, R0IdxN], 0xf007, SH2E_UP),
    e("fmov.d", &[IndM, DrN], 0xf008, SH2A_OR_SH4_UP),
    e("fmov.d", &[DrM, IndN], 0xf00a, SH2A_OR_SH4_UP),
    e("fmov.d", &[IncM, DrN], 0xf009, SH2A_OR_SH4_UP),
    e("fmov.d", &[DrM, DecN], 0xf00b, SH2A_OR_SH4_UP),
    e("fmov.d", &[R0IdxM, DrN], 0xf006, SH2A_OR_SH4_UP),
    e("fmov.d", &[DrM, R0IdxN], 0xf007, SH2A_OR_SH4_UP),
];

/// The entries for `name`, in table order. Empty if it is not an instruction.
pub fn lookup(name: &str) -> impl Iterator<Item = &'static Entry> + '_ {
    TABLE.iter().filter(move |e| e.name == name)
}

#[allow(dead_code)]
pub fn exists(name: &str) -> bool {
    lookup(name).next().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_two_entries_of_a_mnemonic_take_the_same_operands() {
        for (i, a) in TABLE.iter().enumerate() {
            for b in &TABLE[i + 1..] {
                assert!(
                    !(a.name == b.name && a.args == b.args),
                    "`{}` has two entries for the same operands",
                    a.name
                );
            }
        }
    }

    #[test]
    fn every_form_runs_on_an_sh4a() {
        // So on `sh`, where every CPU is allowed to start with, no mix of
        // forms can leave the running intersection without a CPU.
        for e in TABLE {
            assert_eq!(e.arch & cpu::SH4A, cpu::SH4A, "`{}`", e.name);
        }
    }

    #[test]
    fn operand_fields_are_zero_in_the_base_word() {
        // A nonzero field bit in the table would be ORed into every use.
        for e in TABLE {
            let mut mask = 0u16;
            for a in e.args {
                mask |= match a {
                    RegN | IndN | IncN | DecN | R0IdxN | DispN(_) | FrN | DrN => 0x0f00,
                    FvN => 0x0c00,
                    RegM | IndM | IncM | DecM | R0IdxM | DispM(_) | FrM | DrM => 0x00f0,
                    BankM => 0x0070,
                    FvM => 0x0300,
                    Imm8 | GbrDisp(_) | PcRel(_) | Branch8 => 0x00ff,
                    Branch12 => 0x0fff,
                    _ => 0,
                };
                if let DispN(_) | DispM(_) = a {
                    mask |= 0x000f;
                }
            }
            assert_eq!(e.word & mask, 0, "`{}` word {:#06x}", e.name, e.word);
        }
    }
}
