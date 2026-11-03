//! The PowerPC instruction table.
//!
//! Every instruction is a 32-bit word made of fixed opcode bits plus a handful
//! of fields at known positions, so a definition is just the word with its
//! fields zeroed plus a list saying which operand goes where. [`F`] names the
//! fields; the bit positions live in [`super::encode`]. POWER10's prefixed
//! instructions are two such words, and keep the prefix in the upper half of
//! the same value. The AltiVec, VSX and POWER10 instructions are in a table of
//! their own, [`super::vector`], which is looked up together with this one.
//!
//! Field names follow the ISA manual: the 6:10 slot is `RT`/`RS` depending on
//! whether the instruction reads or writes it, 11:15 is `RA`, 16:20 is `RB`.
//! Here they are always [`F::Rt`], [`F::Ra`], [`F::Rb`] — the direction does
//! not change the encoding, and a single name keeps the table readable.
//!
//! Operands are listed in *written* order, which is often not field order:
//! `and rA, rS, rB` writes the destination first but encodes it in the RA
//! slot, so its pattern is `[Ra, Rt, Rb]`.

use std::collections::HashMap;
use std::sync::OnceLock;

/// A field of the instruction word, and which operand fills it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum F {
    /// GPR in bits 6:10 (RT / RS).
    Rt,
    /// GPR in bits 11:15 (RA).
    Ra,
    /// GPR in bits 16:20 (RB).
    Rb,
    /// One GPR written into both 6:10 and 16:20, which is how `mr rA, rS`
    /// becomes `or rA, rS, rS`.
    RtRb,
    /// FPR in bits 6:10, 11:15, 16:20 and 21:25.
    Ft,
    Fa,
    Fb,
    Fc,
    /// Signed 16-bit immediate in bits 16:31.
    Simm,
    /// A 16-bit immediate that may be written signed or unsigned. Only the
    /// instructions that load the *high* half of a value take this, since
    /// `lis 3, 0x8000` is how the upper half of an address is written.
    SimmU,
    /// The negation of a signed 16-bit immediate: `subi` is `addi` with -SI.
    NegSimm,
    /// Unsigned 16-bit immediate in bits 16:31.
    Uimm,
    /// M-form rotate fields: SH in 16:20, MB in 21:25, ME in 26:30.
    Sh5,
    Mb5,
    Me5,
    /// MD-form rotate fields, each six bits split across the word.
    Sh6,
    M6,
    /// CR field number: bits 6:8 (BF) and 11:13 (BFA).
    CrfD,
    CrfS,
    /// CR *bit* number: bits 6:10, 11:15, 16:20.
    CrbD,
    CrbA,
    CrbB,
    /// One CR bit written into both the A and B slots (`crnot`, `crmove`).
    CrbAB,
    /// One CR bit written into all three slots (`crclr`, `crset`).
    CrbAll,
    /// A CR field number, added times four to the BI value already in the
    /// word: `beq cr7, x` is `bc 12, 4*7+2, x`.
    CrfBi,
    /// Branch BO (6:10) and BI (11:15).
    Bo,
    Bi,
    /// Branch hint, bits 19:20.
    Bh,
    /// Trap condition, bits 6:10.
    To,
    /// `mtcrf` field mask, bits 12:19.
    Crm,
    /// `mtfsf` field mask, bits 7:14.
    Flm,
    /// SPR number, bits 11:20 with its two halves swapped.
    Spr,
    /// `sc` level, bits 20:26.
    Lev,
    /// Comparison width, bit 10: clear for 32-bit, set for 64-bit.
    L,
    /// `d(rA)`: displacement into 16:31, base register into 11:15.
    MemD,
    /// `ds(rA)`: displacement into 16:29 only, so it must be a multiple of 4.
    MemDS,
    /// Branch displacement: 24 encoded bits (I-form) or 14 (B-form).
    Rel24,
    Rel14,
    /// The same fields holding an absolute address (`ba`, `bca`).
    Abs24,
    Abs14,
    /// An extended rotate mnemonic taking one immediate.
    RotN(Rot),
    /// An extended rotate mnemonic taking a width and a bit position.
    RotNB(Rot2),

    // ---- AltiVec and VSX; see [`super::vector`] ---------------------------
    /// Vector register (VR) in bits 6:10, 11:15, 16:20 and 21:25 — the four
    /// slots the GPR and FPR fields also use.
    Vt,
    Va,
    Vb,
    Vc,
    /// One VR written into both the A and B slots, which is how `vmr vD, vS`
    /// becomes `vor vD, vS, vS`.
    VaVb,
    /// VSX register (VSR), six bits: the low five in one of the same four
    /// slots, and the sixth in the extension bit that slot owns at the bottom
    /// of the word — TX is bit 31, AX 29, BX 30, CX 28.
    Xt,
    Xa,
    Xb,
    Xc,
    /// One VSR written into both the A and B slots (`xxswapd`, `xxlnot`).
    Xab,
    /// The VSR of a DQ-form load or store, whose extension bit is bit 28.
    Xtq,
    /// The VSR of an 8RR-form prefixed instruction, extension bit 15.
    Xts,
    /// The VSR of `plxv` and `pstxv`, whose six bits are contiguous: the
    /// suffix's primary opcode has a spare bit immediately below the slot.
    Xtop,
    /// An even-numbered VSR naming a pair, in the T, A and B slots. The pair
    /// number is written out in full and the low bit dropped, and the
    /// extension bit is again elsewhere: bit 10 for T, 29 and 30 for A and B.
    Xtp,
    #[allow(dead_code)]
    Xap,
    #[allow(dead_code)]
    Xbp,
    /// GPR in bits 21:25, where `maddld` keeps its addend.
    Rc,
    /// `paste.`'s L, bit 10, which reads 1 when it is left out: the
    /// definition carries the bit set, and writing 0 clears it.
    L1,
    /// An unsigned immediate `bits` wide whose least significant bit is bit
    /// `lsb` of the word, counted from the bottom as Rust counts bits.
    ///
    /// The vector forms carry two dozen one-off immediate fields — `vsldoi`'s
    /// SHB, `xxpermdi`'s DM, `vspltw`'s UIM, `xxeval`'s IMM8 — with nothing
    /// in common but a width and a position, so they share one field rather
    /// than each earning a name.
    Uim(u8, u8),
    /// The same, read as signed: `vspltisb`'s SIM.
    Sim(u8, u8),
    /// The same again, accepting either sign, for `xxspltib`, whose byte is
    /// written -128 to 255.
    SimU(u8, u8),
    /// `xxspltd`'s DM, which is written 0 or 1 and encoded as 0 or 3.
    DmEx,
    /// `xvtstdc*`'s DCMX, seven bits in three pieces.
    Dcmxs,
    /// `addpcis`'s D, a 16-bit signed value in three pieces, and the negated
    /// form `subpcis` takes.
    Dx,
    NegDx,
    /// `dq(rA)`: the displacement into bits 4:15 of the low halfword, so it
    /// must be a multiple of 16.
    MemDQ,

    // ---- prefixed (POWER10) ------------------------------------------------
    /// `d(rA)` of a prefixed instruction: a 34-bit displacement, its top 18
    /// bits in the prefix word and the rest in the suffix.
    MemD34,
    /// The same 34-bit field holding an immediate rather than a displacement,
    /// and the negated form `psubi` takes.
    Simm34,
    NegSimm34,
    /// A 32-bit immediate split half into the prefix word and half into the
    /// suffix (`xxspltiw`, `xxspltidp`).
    Imm32,
    /// The R bit of a prefixed instruction, bit 20 of the prefix word: the
    /// displacement is relative to the instruction rather than to `rA`.
    Pcrel,
}

/// Extended rotate mnemonics with a single immediate operand.
///
/// Each is one particular `rlwinm`/`rldicl`/`rldicr` with its SH, MB and ME
/// derived from the shift amount; `encode::rot1` does the sums.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Rot {
    Slwi,
    Srwi,
    Clrlwi,
    Clrrwi,
    Rotlwi,
    Rotrwi,
    Sldi,
    Srdi,
    Clrldi,
    Clrrdi,
    Rotldi,
    Rotrdi,
}

/// Extended rotate mnemonics taking a field width `n` and a start bit `b`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Rot2 {
    Extlwi,
    Extrwi,
    Inslwi,
    Insrwi,
    Extldi,
    Extrdi,
    Insrdi,
}

/// The instruction takes the `.` suffix, setting Rc (bit 31).
pub const RC: u8 = 1 << 0;
/// The instruction takes the `o` suffix, setting OE (bit 21).
pub const OE: u8 = 1 << 1;
/// 64-bit only; rejected in 32-bit mode.
pub const P64: u8 = 1 << 2;
/// The first operand may be omitted and defaults to zero. Used for the CR
/// field of `cmpw`/`beq` and the level of `sc`.
pub const OPT1: u8 = 1 << 3;
/// The last operand may be omitted and defaults to zero (`bclr`'s BH).
pub const OPTL: u8 = 1 << 4;
/// The instruction takes the `.` suffix, setting the *vector* record bit,
/// which is bit 21 rather than bit 31: `vcmpequb.` sets CR6 where
/// `add.` sets CR0.
pub const VRC: u8 = 1 << 5;

pub struct Def {
    pub name: &'static str,
    /// The instruction word with every operand field zero. A prefixed
    /// (POWER10) instruction keeps its prefix word in the upper 32 bits, and
    /// is eight bytes long; every other instruction leaves them clear.
    pub word: u64,
    pub ops: &'static [F],
    pub flags: u8,
}

/// What a mnemonic resolved to, including the suffixes stripped off it.
pub struct Resolved {
    pub def: &'static Def,
    pub rc: bool,
    pub oe: bool,
}

/// Looks up a mnemonic, peeling off the `.` (record) and `o` (overflow-enable)
/// suffixes if the table entry underneath accepts them.
///
/// Order matters: `addo.` is `add` with both, and an exact hit always wins so
/// that `andi.`, whose dot is part of the name, is never read as `andi` with a
/// record bit it does not have.
pub fn lookup(name: &str) -> Option<Resolved> {
    if let Some(def) = index().get(name) {
        return Some(Resolved {
            def,
            rc: false,
            oe: false,
        });
    }
    let (base, rc) = match name.strip_suffix('.') {
        Some(b) => (b, true),
        None => (name, false),
    };
    if rc
        && let Some(def) = index().get(base)
        && def.flags & (RC | VRC) != 0
    {
        return Some(Resolved {
            def,
            rc: true,
            oe: false,
        });
    }
    let base = base.strip_suffix('o')?;
    let def = index().get(base)?;
    if def.flags & OE == 0 || (rc && def.flags & RC == 0) {
        return None;
    }
    Some(Resolved { def, rc, oe: true })
}

fn index() -> &'static HashMap<&'static str, &'static Def> {
    static INDEX: OnceLock<HashMap<&'static str, &'static Def>> = OnceLock::new();
    INDEX.get_or_init(|| {
        DEFS.iter()
            .chain(super::vector::DEFS)
            .map(|d| (d.name, d))
            .collect()
    })
}

pub(super) const fn d(name: &'static str, word: u64, ops: &'static [F], flags: u8) -> Def {
    Def {
        name,
        word,
        ops,
        flags,
    }
}

/// Primary opcode in bits 0:5.
pub(super) const fn op(primary: u32) -> u64 {
    (primary as u64) << 26
}

/// XO-form extended opcode, bits 22:30 (opcode 31 arithmetic).
const fn xo(primary: u32, x: u32) -> u64 {
    op(primary) | ((x as u64) << 1)
}

/// X- and XL-form extended opcode, bits 21:30. Same shift as [`xo`]; named
/// apart only because the field is one bit wider and so cannot hold OE.
pub(super) const fn x(primary: u32, ext: u32) -> u64 {
    op(primary) | ((ext as u64) << 1)
}

/// A-form extended opcode, bits 26:30.
const fn a(primary: u32, ext: u32) -> u64 {
    op(primary) | ((ext as u64) << 1)
}

/// B-form branch word with BO and BI already placed.
const fn bcond(bo: u32, bi: u32, aalk: u32) -> u64 {
    op(16) | ((bo as u64) << 21) | ((bi as u64) << 16) | aalk as u64
}

/// XL-form branch-to-register word (`bclr`/`bcctr`) with BO and BI placed.
const fn breg(bo: u32, bi: u32, ext: u32, lk: u32) -> u64 {
    op(19) | ((bo as u64) << 21) | ((bi as u64) << 16) | ((ext as u64) << 1) | lk as u64
}

/// `mfspr`/`mtspr` with a fixed SPR number, for `mflr` and friends. The 10-bit
/// SPR field is stored with its two five-bit halves swapped.
const fn spr(ext: u32, n: u32) -> u64 {
    op(31) | ((((n & 0x1f) << 16) | ((n >> 5) << 11)) as u64) | ((ext as u64) << 1)
}

use F::*;
use Rot::*;
use Rot2::*;

#[rustfmt::skip]
static DEFS: &[Def] = &[
    // ---- fixed-point arithmetic, XO-form -------------------------------
    d("add",     xo(31, 266), &[Rt, Ra, Rb], RC | OE),
    d("addc",    xo(31, 10),  &[Rt, Ra, Rb], RC | OE),
    d("adde",    xo(31, 138), &[Rt, Ra, Rb], RC | OE),
    d("addme",   xo(31, 234), &[Rt, Ra],     RC | OE),
    d("addze",   xo(31, 202), &[Rt, Ra],     RC | OE),
    d("subf",    xo(31, 40),  &[Rt, Ra, Rb], RC | OE),
    d("subfc",   xo(31, 8),   &[Rt, Ra, Rb], RC | OE),
    d("subfe",   xo(31, 136), &[Rt, Ra, Rb], RC | OE),
    d("subfme",  xo(31, 232), &[Rt, Ra],     RC | OE),
    d("subfze",  xo(31, 200), &[Rt, Ra],     RC | OE),
    d("neg",     xo(31, 104), &[Rt, Ra],     RC | OE),
    d("mullw",   xo(31, 235), &[Rt, Ra, Rb], RC | OE),
    d("mulld",   xo(31, 233), &[Rt, Ra, Rb], RC | OE | P64),
    d("mulhw",   xo(31, 75),  &[Rt, Ra, Rb], RC),
    d("mulhwu",  xo(31, 11),  &[Rt, Ra, Rb], RC),
    d("mulhd",   xo(31, 73),  &[Rt, Ra, Rb], RC | P64),
    d("mulhdu",  xo(31, 9),   &[Rt, Ra, Rb], RC | P64),
    d("divw",    xo(31, 491), &[Rt, Ra, Rb], RC | OE),
    d("divwu",   xo(31, 459), &[Rt, Ra, Rb], RC | OE),
    d("divd",    xo(31, 489), &[Rt, Ra, Rb], RC | OE | P64),
    d("divdu",   xo(31, 457), &[Rt, Ra, Rb], RC | OE | P64),

    // ---- fixed-point arithmetic, D-form --------------------------------
    d("addi",    op(14), &[Rt, Ra, Simm], 0),
    d("addis",   op(15), &[Rt, Ra, SimmU], 0),
    d("addic",   op(12), &[Rt, Ra, Simm], 0),
    d("addic.",  op(13), &[Rt, Ra, Simm], 0),
    d("subfic",  op(8),  &[Rt, Ra, Simm], 0),
    d("mulli",   op(7),  &[Rt, Ra, Simm], 0),

    // ---- logical, X-form ------------------------------------------------
    d("and",     x(31, 28),  &[Ra, Rt, Rb], RC),
    d("or",      x(31, 444), &[Ra, Rt, Rb], RC),
    d("xor",     x(31, 316), &[Ra, Rt, Rb], RC),
    d("nand",    x(31, 476), &[Ra, Rt, Rb], RC),
    d("nor",     x(31, 124), &[Ra, Rt, Rb], RC),
    d("eqv",     x(31, 284), &[Ra, Rt, Rb], RC),
    d("andc",    x(31, 60),  &[Ra, Rt, Rb], RC),
    d("orc",     x(31, 412), &[Ra, Rt, Rb], RC),
    d("slw",     x(31, 24),  &[Ra, Rt, Rb], RC),
    d("srw",     x(31, 536), &[Ra, Rt, Rb], RC),
    d("sraw",    x(31, 792), &[Ra, Rt, Rb], RC),
    d("srawi",   x(31, 824), &[Ra, Rt, Sh5], RC),
    d("sld",     x(31, 27),  &[Ra, Rt, Rb], RC | P64),
    d("srd",     x(31, 539), &[Ra, Rt, Rb], RC | P64),
    d("srad",    x(31, 794), &[Ra, Rt, Rb], RC | P64),
    // XS-form: the extended opcode is only nine bits, since bit 30 carries
    // the top bit of the six-bit shift.
    d("sradi",   op(31) | (413 << 2), &[Ra, Rt, Sh6], RC | P64),
    d("extsb",   x(31, 954), &[Ra, Rt], RC),
    d("extsh",   x(31, 922), &[Ra, Rt], RC),
    d("extsw",   x(31, 986), &[Ra, Rt], RC | P64),
    d("cntlzw",  x(31, 26),  &[Ra, Rt], RC),
    d("cntlzd",  x(31, 58),  &[Ra, Rt], RC | P64),
    d("popcntw", x(31, 378), &[Ra, Rt], 0),
    d("popcntd", x(31, 506), &[Ra, Rt], P64),

    // ---- logical, D-form. The destination is the RA field here, so the
    // written order is the reverse of the arithmetic D-form's.
    d("ori",     op(24), &[Ra, Rt, Uimm], 0),
    d("oris",    op(25), &[Ra, Rt, Uimm], 0),
    d("xori",    op(26), &[Ra, Rt, Uimm], 0),
    d("xoris",   op(27), &[Ra, Rt, Uimm], 0),
    d("andi.",   op(28), &[Ra, Rt, Uimm], 0),
    d("andis.",  op(29), &[Ra, Rt, Uimm], 0),

    // ---- rotate and mask -------------------------------------------------
    d("rlwinm",  op(21), &[Ra, Rt, Sh5, Mb5, Me5], RC),
    d("rlwimi",  op(20), &[Ra, Rt, Sh5, Mb5, Me5], RC),
    d("rlwnm",   op(23), &[Ra, Rt, Rb,  Mb5, Me5], RC),
    d("rldicl",  op(30)          , &[Ra, Rt, Sh6, M6], RC | P64),
    d("rldicr",  op(30) | (1 << 2), &[Ra, Rt, Sh6, M6], RC | P64),
    d("rldic",   op(30) | (2 << 2), &[Ra, Rt, Sh6, M6], RC | P64),
    d("rldimi",  op(30) | (3 << 2), &[Ra, Rt, Sh6, M6], RC | P64),
    d("rldcl",   op(30) | (8 << 1), &[Ra, Rt, Rb, M6], RC | P64),
    d("rldcr",   op(30) | (9 << 1), &[Ra, Rt, Rb, M6], RC | P64),

    // ---- extended rotates ------------------------------------------------
    d("slwi",    op(21), &[Ra, Rt, RotN(Slwi)],   RC),
    d("srwi",    op(21), &[Ra, Rt, RotN(Srwi)],   RC),
    d("clrlwi",  op(21), &[Ra, Rt, RotN(Clrlwi)], RC),
    d("clrrwi",  op(21), &[Ra, Rt, RotN(Clrrwi)], RC),
    d("rotlwi",  op(21), &[Ra, Rt, RotN(Rotlwi)], RC),
    d("rotrwi",  op(21), &[Ra, Rt, RotN(Rotrwi)], RC),
    d("rotlw",   op(23) | (31 << 1), &[Ra, Rt, Rb], RC),
    d("extlwi",  op(21), &[Ra, Rt, RotNB(Extlwi)], RC),
    d("extrwi",  op(21), &[Ra, Rt, RotNB(Extrwi)], RC),
    d("inslwi",  op(20), &[Ra, Rt, RotNB(Inslwi)], RC),
    d("insrwi",  op(20), &[Ra, Rt, RotNB(Insrwi)], RC),
    d("sldi",    op(30) | (1 << 2), &[Ra, Rt, RotN(Sldi)],   RC | P64),
    d("clrrdi",  op(30) | (1 << 2), &[Ra, Rt, RotN(Clrrdi)], RC | P64),
    d("extldi",  op(30) | (1 << 2), &[Ra, Rt, RotNB(Extldi)], RC | P64),
    d("srdi",    op(30), &[Ra, Rt, RotN(Srdi)],   RC | P64),
    d("clrldi",  op(30), &[Ra, Rt, RotN(Clrldi)], RC | P64),
    d("rotldi",  op(30), &[Ra, Rt, RotN(Rotldi)], RC | P64),
    d("rotrdi",  op(30), &[Ra, Rt, RotN(Rotrdi)], RC | P64),
    d("extrdi",  op(30), &[Ra, Rt, RotNB(Extrdi)], RC | P64),
    d("rotld",   op(30) | (8 << 1), &[Ra, Rt, Rb], RC | P64),
    d("insrdi",  op(30) | (3 << 2), &[Ra, Rt, RotNB(Insrdi)], RC | P64),

    // ---- other extended mnemonics ----------------------------------------
    d("li",      op(14), &[Rt, Simm], 0),
    d("lis",     op(15), &[Rt, SimmU], 0),
    // `la rD, d(rA)` is exactly `addi rD, rA, d`: MemD fills the same fields.
    d("la",      op(14), &[Rt, MemD], 0),
    d("subi",    op(14), &[Rt, Ra, NegSimm], 0),
    d("subis",   op(15), &[Rt, Ra, NegSimm], 0),
    d("subic",   op(12), &[Rt, Ra, NegSimm], 0),
    d("subic.",  op(13), &[Rt, Ra, NegSimm], 0),
    // `sub rD, rA, rB` is `subf rD, rB, rA`: the pattern swaps the fields.
    d("sub",     xo(31, 40), &[Rt, Rb, Ra], RC | OE),
    d("subc",    xo(31, 8),  &[Rt, Rb, Ra], RC | OE),
    d("mr",      x(31, 444), &[Ra, RtRb], RC),
    d("not",     x(31, 124), &[Ra, RtRb], RC),
    d("nop",     op(24), &[], 0),

    // ---- load and store, D-form ------------------------------------------
    d("lbz",     op(34), &[Rt, MemD], 0),
    d("lbzu",    op(35), &[Rt, MemD], 0),
    d("lhz",     op(40), &[Rt, MemD], 0),
    d("lhzu",    op(41), &[Rt, MemD], 0),
    d("lha",     op(42), &[Rt, MemD], 0),
    d("lhau",    op(43), &[Rt, MemD], 0),
    d("lwz",     op(32), &[Rt, MemD], 0),
    d("lwzu",    op(33), &[Rt, MemD], 0),
    d("stb",     op(38), &[Rt, MemD], 0),
    d("stbu",    op(39), &[Rt, MemD], 0),
    d("sth",     op(44), &[Rt, MemD], 0),
    d("sthu",    op(45), &[Rt, MemD], 0),
    d("stw",     op(36), &[Rt, MemD], 0),
    d("stwu",    op(37), &[Rt, MemD], 0),
    d("lmw",     op(46), &[Rt, MemD], 0),
    d("stmw",    op(47), &[Rt, MemD], 0),

    // DS-form: the low two bits of the displacement field are opcode, so the
    // displacement must be a multiple of four.
    d("ld",      op(58),     &[Rt, MemDS], P64),
    d("ldu",     op(58) | 1, &[Rt, MemDS], P64),
    d("lwa",     op(58) | 2, &[Rt, MemDS], P64),
    d("std",     op(62),     &[Rt, MemDS], P64),
    d("stdu",    op(62) | 1, &[Rt, MemDS], P64),

    // ---- load and store, X-form (indexed) --------------------------------
    d("lbzx",    x(31, 87),  &[Rt, Ra, Rb], 0),
    d("lbzux",   x(31, 119), &[Rt, Ra, Rb], 0),
    d("lhzx",    x(31, 279), &[Rt, Ra, Rb], 0),
    d("lhzux",   x(31, 311), &[Rt, Ra, Rb], 0),
    d("lhax",    x(31, 343), &[Rt, Ra, Rb], 0),
    d("lhaux",   x(31, 375), &[Rt, Ra, Rb], 0),
    d("lwzx",    x(31, 23),  &[Rt, Ra, Rb], 0),
    d("lwzux",   x(31, 55),  &[Rt, Ra, Rb], 0),
    d("lwax",    x(31, 341), &[Rt, Ra, Rb], P64),
    d("lwaux",   x(31, 373), &[Rt, Ra, Rb], P64),
    d("ldx",     x(31, 21),  &[Rt, Ra, Rb], P64),
    d("ldux",    x(31, 53),  &[Rt, Ra, Rb], P64),
    d("stbx",    x(31, 215), &[Rt, Ra, Rb], 0),
    d("stbux",   x(31, 247), &[Rt, Ra, Rb], 0),
    d("sthx",    x(31, 407), &[Rt, Ra, Rb], 0),
    d("sthux",   x(31, 439), &[Rt, Ra, Rb], 0),
    d("stwx",    x(31, 151), &[Rt, Ra, Rb], 0),
    d("stwux",   x(31, 183), &[Rt, Ra, Rb], 0),
    d("stdx",    x(31, 149), &[Rt, Ra, Rb], P64),
    d("stdux",   x(31, 181), &[Rt, Ra, Rb], P64),
    d("lhbrx",   x(31, 790), &[Rt, Ra, Rb], 0),
    d("lwbrx",   x(31, 534), &[Rt, Ra, Rb], 0),
    d("sthbrx",  x(31, 918), &[Rt, Ra, Rb], 0),
    d("stwbrx",  x(31, 662), &[Rt, Ra, Rb], 0),
    d("lwarx",   x(31, 20),  &[Rt, Ra, Rb], 0),
    d("stwcx.",  x(31, 150) | 1, &[Rt, Ra, Rb], 0),
    d("ldarx",   x(31, 84),  &[Rt, Ra, Rb], P64),
    d("stdcx.",  x(31, 214) | 1, &[Rt, Ra, Rb], P64),

    // ---- branches ---------------------------------------------------------
    d("b",       op(18),     &[Rel24], 0),
    d("bl",      op(18) | 1, &[Rel24], 0),
    d("ba",      op(18) | 2, &[Abs24], 0),
    d("bla",     op(18) | 3, &[Abs24], 0),
    d("bc",      op(16),     &[Bo, Bi, Rel14], 0),
    d("bcl",     op(16) | 1, &[Bo, Bi, Rel14], 0),
    d("bca",     op(16) | 2, &[Bo, Bi, Abs14], 0),
    d("bcla",    op(16) | 3, &[Bo, Bi, Abs14], 0),

    // Conditional branches. BO selects "branch if the CR bit is set" (12) or
    // "clear" (4); BI selects the bit, counted from the start of CR field 0.
    // The optional CR-field operand adds four per field, which is what
    // [`F::CrfBi`] does.
    d("blt",     bcond(12, 0, 0), &[CrfBi, Rel14], OPT1),
    d("bgt",     bcond(12, 1, 0), &[CrfBi, Rel14], OPT1),
    d("beq",     bcond(12, 2, 0), &[CrfBi, Rel14], OPT1),
    d("bso",     bcond(12, 3, 0), &[CrfBi, Rel14], OPT1),
    d("bun",     bcond(12, 3, 0), &[CrfBi, Rel14], OPT1),
    d("bge",     bcond(4, 0, 0),  &[CrfBi, Rel14], OPT1),
    d("bnl",     bcond(4, 0, 0),  &[CrfBi, Rel14], OPT1),
    d("ble",     bcond(4, 1, 0),  &[CrfBi, Rel14], OPT1),
    d("bng",     bcond(4, 1, 0),  &[CrfBi, Rel14], OPT1),
    d("bne",     bcond(4, 2, 0),  &[CrfBi, Rel14], OPT1),
    d("bns",     bcond(4, 3, 0),  &[CrfBi, Rel14], OPT1),
    d("bnu",     bcond(4, 3, 0),  &[CrfBi, Rel14], OPT1),
    d("bltl",    bcond(12, 0, 1), &[CrfBi, Rel14], OPT1),
    d("bgtl",    bcond(12, 1, 1), &[CrfBi, Rel14], OPT1),
    d("beql",    bcond(12, 2, 1), &[CrfBi, Rel14], OPT1),
    d("bgel",    bcond(4, 0, 1),  &[CrfBi, Rel14], OPT1),
    d("blel",    bcond(4, 1, 1),  &[CrfBi, Rel14], OPT1),
    d("bnel",    bcond(4, 2, 1),  &[CrfBi, Rel14], OPT1),
    // `bt`/`bf` take the CR bit itself rather than a condition name.
    d("bt",      bcond(12, 0, 0), &[Bi, Rel14], 0),
    d("bf",      bcond(4, 0, 0),  &[Bi, Rel14], 0),

    // CTR-decrementing branches. BO bit 1 selects "decrement CTR", bit 3
    // whether to branch when the result is zero.
    d("bdnz",    bcond(16, 0, 0), &[Rel14], 0),
    d("bdnzl",   bcond(16, 0, 1), &[Rel14], 0),
    d("bdz",     bcond(18, 0, 0), &[Rel14], 0),
    d("bdzl",    bcond(18, 0, 1), &[Rel14], 0),
    d("bdnzt",   bcond(8, 0, 0),  &[Bi, Rel14], 0),
    d("bdnzf",   bcond(0, 0, 0),  &[Bi, Rel14], 0),
    d("bdzt",    bcond(10, 0, 0), &[Bi, Rel14], 0),
    d("bdzf",    bcond(2, 0, 0),  &[Bi, Rel14], 0),

    // Branch to LR or CTR. BO 20 is "always".
    d("bclr",    breg(0, 0, 16, 0),  &[Bo, Bi, Bh], OPTL),
    d("bclrl",   breg(0, 0, 16, 1),  &[Bo, Bi, Bh], OPTL),
    d("bcctr",   breg(0, 0, 528, 0), &[Bo, Bi, Bh], OPTL),
    d("bcctrl",  breg(0, 0, 528, 1), &[Bo, Bi, Bh], OPTL),
    d("blr",     breg(20, 0, 16, 0),  &[], 0),
    d("blrl",    breg(20, 0, 16, 1),  &[], 0),
    d("bctr",    breg(20, 0, 528, 0), &[], 0),
    d("bctrl",   breg(20, 0, 528, 1), &[], 0),
    d("bltlr",   breg(12, 0, 16, 0), &[CrfBi], OPT1),
    d("bgtlr",   breg(12, 1, 16, 0), &[CrfBi], OPT1),
    d("beqlr",   breg(12, 2, 16, 0), &[CrfBi], OPT1),
    d("bgelr",   breg(4, 0, 16, 0),  &[CrfBi], OPT1),
    d("blelr",   breg(4, 1, 16, 0),  &[CrfBi], OPT1),
    d("bnelr",   breg(4, 2, 16, 0),  &[CrfBi], OPT1),
    d("bltctr",  breg(12, 0, 528, 0), &[CrfBi], OPT1),
    d("bgtctr",  breg(12, 1, 528, 0), &[CrfBi], OPT1),
    d("beqctr",  breg(12, 2, 528, 0), &[CrfBi], OPT1),
    d("bgectr",  breg(4, 0, 528, 0),  &[CrfBi], OPT1),
    d("blectr",  breg(4, 1, 528, 0),  &[CrfBi], OPT1),
    d("bnectr",  breg(4, 2, 528, 0),  &[CrfBi], OPT1),
    d("bdnzlr",  breg(16, 0, 16, 0), &[], 0),
    d("bdzlr",   breg(18, 0, 16, 0), &[], 0),

    // ---- comparison --------------------------------------------------------
    // L (bit 10) selects a 32- or 64-bit comparison; the extended mnemonics
    // bake it in and drop it from the operand list.
    d("cmp",     x(31, 0),  &[CrfD, L, Ra, Rb], 0),
    d("cmpl",    x(31, 32), &[CrfD, L, Ra, Rb], 0),
    d("cmpi",    op(11),    &[CrfD, L, Ra, Simm], 0),
    d("cmpli",   op(10),    &[CrfD, L, Ra, Uimm], 0),
    d("cmpw",    x(31, 0),                 &[CrfD, Ra, Rb], OPT1),
    d("cmpd",    x(31, 0) | (1 << 21),     &[CrfD, Ra, Rb], OPT1 | P64),
    d("cmplw",   x(31, 32),                &[CrfD, Ra, Rb], OPT1),
    d("cmpld",   x(31, 32) | (1 << 21),    &[CrfD, Ra, Rb], OPT1 | P64),
    d("cmpwi",   op(11),                   &[CrfD, Ra, Simm], OPT1),
    d("cmpdi",   op(11) | (1 << 21),       &[CrfD, Ra, Simm], OPT1 | P64),
    d("cmplwi",  op(10),                   &[CrfD, Ra, Uimm], OPT1),
    d("cmpldi",  op(10) | (1 << 21),       &[CrfD, Ra, Uimm], OPT1 | P64),

    // ---- condition-register logic ------------------------------------------
    d("crand",   x(19, 257), &[CrbD, CrbA, CrbB], 0),
    d("crandc",  x(19, 129), &[CrbD, CrbA, CrbB], 0),
    d("creqv",   x(19, 289), &[CrbD, CrbA, CrbB], 0),
    d("crnand",  x(19, 225), &[CrbD, CrbA, CrbB], 0),
    d("crnor",   x(19, 33),  &[CrbD, CrbA, CrbB], 0),
    d("cror",    x(19, 449), &[CrbD, CrbA, CrbB], 0),
    d("crorc",   x(19, 417), &[CrbD, CrbA, CrbB], 0),
    d("crxor",   x(19, 193), &[CrbD, CrbA, CrbB], 0),
    d("crnot",   x(19, 33),  &[CrbD, CrbAB], 0),
    d("crmove",  x(19, 449), &[CrbD, CrbAB], 0),
    d("crclr",   x(19, 193), &[CrbAll], 0),
    d("crset",   x(19, 289), &[CrbAll], 0),
    d("mcrf",    x(19, 0),   &[CrfD, CrfS], 0),

    // ---- system and special registers ---------------------------------------
    d("mfspr",   x(31, 339), &[Rt, Spr], 0),
    d("mtspr",   x(31, 467), &[Spr, Rt], 0),
    d("mflr",    spr(339, 8),   &[Rt], 0),
    d("mtlr",    spr(467, 8),   &[Rt], 0),
    d("mfctr",   spr(339, 9),   &[Rt], 0),
    d("mtctr",   spr(467, 9),   &[Rt], 0),
    d("mfxer",   spr(339, 1),   &[Rt], 0),
    d("mtxer",   spr(467, 1),   &[Rt], 0),
    // `mftb` is its own X-form opcode, not `mfspr` with SPR 268.
    d("mftb",    spr(371, 268), &[Rt], 0),
    d("mfcr",    x(31, 19),  &[Rt], 0),
    d("mtcrf",   x(31, 144), &[Crm, Rt], 0),
    d("mtcr",    x(31, 144) | (0xff << 12), &[Rt], 0),
    d("mfmsr",   x(31, 83),  &[Rt], 0),
    d("mtmsr",   x(31, 146), &[Rt], 0),
    d("sync",    x(31, 598), &[], 0),
    d("lwsync",  x(31, 598) | (1 << 21), &[], 0),
    d("ptesync", x(31, 598) | (2 << 21), &[], 0),
    d("isync",   x(19, 150), &[], 0),
    d("eieio",   x(31, 854), &[], 0),
    d("dcbz",    x(31, 1014), &[Ra, Rb], 0),
    d("icbi",    x(31, 982),  &[Ra, Rb], 0),
    d("sc",      op(17) | 2, &[Lev], OPT1),
    d("trap",    x(31, 4) | (31 << 21), &[], 0),
    d("tw",      x(31, 4), &[To, Ra, Rb], 0),
    d("twi",     op(3),    &[To, Ra, Simm], 0),
    d("tweq",    x(31, 4) | (4 << 21),  &[Ra, Rb], 0),
    d("twne",    x(31, 4) | (24 << 21), &[Ra, Rb], 0),
    d("twlt",    x(31, 4) | (16 << 21), &[Ra, Rb], 0),
    d("twgt",    x(31, 4) | (8 << 21),  &[Ra, Rb], 0),
    d("tweqi",   op(3) | (4 << 21),  &[Ra, Simm], 0),
    d("twnei",   op(3) | (24 << 21), &[Ra, Simm], 0),

    // ---- floating point ------------------------------------------------------
    d("lfs",     op(48), &[Ft, MemD], 0),
    d("lfsu",    op(49), &[Ft, MemD], 0),
    d("lfd",     op(50), &[Ft, MemD], 0),
    d("lfdu",    op(51), &[Ft, MemD], 0),
    d("stfs",    op(52), &[Ft, MemD], 0),
    d("stfsu",   op(53), &[Ft, MemD], 0),
    d("stfd",    op(54), &[Ft, MemD], 0),
    d("stfdu",   op(55), &[Ft, MemD], 0),
    d("lfsx",    x(31, 535), &[Ft, Ra, Rb], 0),
    d("lfsux",   x(31, 567), &[Ft, Ra, Rb], 0),
    d("lfdx",    x(31, 599), &[Ft, Ra, Rb], 0),
    d("lfdux",   x(31, 631), &[Ft, Ra, Rb], 0),
    d("stfsx",   x(31, 663), &[Ft, Ra, Rb], 0),
    d("stfsux",  x(31, 695), &[Ft, Ra, Rb], 0),
    d("stfdx",   x(31, 727), &[Ft, Ra, Rb], 0),
    d("stfdux",  x(31, 759), &[Ft, Ra, Rb], 0),
    // A-form. `fmul` has no B operand and `fmadd` takes C before B, which is
    // why the multiply-carrying forms list `Fc` where the others list `Fb`.
    d("fadd",    a(63, 21), &[Ft, Fa, Fb], RC),
    d("fadds",   a(59, 21), &[Ft, Fa, Fb], RC),
    d("fsub",    a(63, 20), &[Ft, Fa, Fb], RC),
    d("fsubs",   a(59, 20), &[Ft, Fa, Fb], RC),
    d("fdiv",    a(63, 18), &[Ft, Fa, Fb], RC),
    d("fdivs",   a(59, 18), &[Ft, Fa, Fb], RC),
    d("fmul",    a(63, 25), &[Ft, Fa, Fc], RC),
    d("fmuls",   a(59, 25), &[Ft, Fa, Fc], RC),
    d("fsqrt",   a(63, 22), &[Ft, Fb], RC),
    d("fsqrts",  a(59, 22), &[Ft, Fb], RC),
    d("fmadd",   a(63, 29), &[Ft, Fa, Fc, Fb], RC),
    d("fmadds",  a(59, 29), &[Ft, Fa, Fc, Fb], RC),
    d("fmsub",   a(63, 28), &[Ft, Fa, Fc, Fb], RC),
    d("fmsubs",  a(59, 28), &[Ft, Fa, Fc, Fb], RC),
    d("fnmadd",  a(63, 31), &[Ft, Fa, Fc, Fb], RC),
    d("fnmadds", a(59, 31), &[Ft, Fa, Fc, Fb], RC),
    d("fnmsub",  a(63, 30), &[Ft, Fa, Fc, Fb], RC),
    d("fnmsubs", a(59, 30), &[Ft, Fa, Fc, Fb], RC),
    d("fsel",    a(63, 23), &[Ft, Fa, Fc, Fb], RC),
    d("fmr",     x(63, 72),  &[Ft, Fb], RC),
    d("fneg",    x(63, 40),  &[Ft, Fb], RC),
    d("fabs",    x(63, 264), &[Ft, Fb], RC),
    d("fnabs",   x(63, 136), &[Ft, Fb], RC),
    d("frsp",    x(63, 12),  &[Ft, Fb], RC),
    d("fctiw",   x(63, 14),  &[Ft, Fb], RC),
    d("fctiwz",  x(63, 15),  &[Ft, Fb], RC),
    d("fctid",   x(63, 814), &[Ft, Fb], RC | P64),
    d("fctidz",  x(63, 815), &[Ft, Fb], RC | P64),
    d("fcfid",   x(63, 846), &[Ft, Fb], RC | P64),
    d("fcmpu",   x(63, 0),   &[CrfD, Fa, Fb], 0),
    d("fcmpo",   x(63, 32),  &[CrfD, Fa, Fb], 0),
    d("mffs",    x(63, 583), &[Ft], RC),
    d("mtfsf",   x(63, 711), &[Flm, Fb], RC),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> impl Iterator<Item = &'static Def> {
        DEFS.iter().chain(super::super::vector::DEFS)
    }

    #[test]
    fn names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for d in all() {
            assert!(seen.insert(d.name), "duplicate mnemonic `{}`", d.name);
        }
    }

    #[test]
    fn vector_mnemonics_resolve_with_their_record_bits() {
        let r = lookup("vcmpequb.").expect("vcmpequb. exists");
        assert_eq!(r.def.name, "vcmpequb");
        assert!(r.rc && !r.oe);
        // `bcdadd.` has no unrecorded form, so the dot is part of its name.
        assert!(!lookup("bcdadd.").expect("bcdadd.").rc);
        assert!(lookup("bcdadd").is_none());
        // `vaddubm` has no record form at all.
        assert!(lookup("vaddubm.").is_none());
        assert!(lookup("xxlor").is_some() && lookup("paddi").is_some());
    }

    #[test]
    fn suffixes_are_peeled_only_when_the_base_allows_them() {
        let r = lookup("addo.").expect("addo. exists");
        assert_eq!(r.def.name, "add");
        assert!(r.rc && r.oe);
        // `andi.` is a name, not `andi` with a record bit.
        assert_eq!(lookup("andi.").expect("andi.").def.name, "andi.");
        assert!(!lookup("andi.").expect("andi.").rc);
        // `addi` has no Rc form.
        assert!(lookup("addi.").is_none());
        // Nothing turns `lwz` into an overflow-enabled instruction.
        assert!(lookup("lwzo").is_none());
    }

    #[test]
    fn every_definition_has_room_for_its_flags() {
        for def in all() {
            if def.flags & RC != 0 {
                assert_eq!(def.word & 1, 0, "`{}` already sets Rc", def.name);
            }
            if def.flags & VRC != 0 {
                assert_eq!(def.word & (1 << 10), 0, "`{}` already sets Rc", def.name);
                assert_eq!(def.flags & RC, 0, "`{}` has two record bits", def.name);
            }
            if def.word >> 32 != 0 {
                // A prefix word has primary opcode 1.
                assert_eq!(def.word >> 58, 1, "`{}` has a stray prefix", def.name);
            }
            if def.flags & OE != 0 {
                // OE is manual bit 21, which is word bit 10.
                assert_eq!(def.word & (1 << 10), 0, "`{}` already sets OE", def.name);
            }
        }
    }
}
