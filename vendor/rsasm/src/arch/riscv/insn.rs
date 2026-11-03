//! The instruction table.
//!
//! RISC-V is regular enough that a definition is just an operand shape plus a
//! word with every operand field left at zero: opcode, `funct3`, `funct7` and
//! any register the encoding pins down (the `rs2` of `fsqrt.s`, say) are all
//! baked into `base`.

/// How an instruction's operands are spelled and where they go.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `rd, rs1, rs2`
    R,
    /// `rd, rs1, simm12`
    I,
    /// `rd, rs1, shamt`
    Shift,
    /// `rd, rs1, shamt`, with the shift amount capped at 31 on RV64 too
    ShiftW,
    /// `rd, off(rs1)`
    Load,
    /// `rs2, off(rs1)`
    Store,
    /// `rs1, rs2, target`
    Branch,
    /// `rd, imm20`
    U,
    /// `rd, target`, or `target` with `ra` implied
    Jal,
    /// `rd, rs1, imm`, `rd, off(rs1)`, or a bare `rs1`
    Jalr,
    /// `rd, rs2, (rs1)`
    Amo,
    /// `rd, (rs1)` — the load half of a reservation pair
    AmoLoad,
    /// `frd, off(rs1)`
    FLoad,
    /// `frs2, off(rs1)`
    FStore,
    /// `frd, frs1, frs2`
    F3,
    /// `frd, frs1`
    F2,
    /// `frd, frs1, frs2, frs3`
    F4,
    /// `rd, frs1, frs2`
    FCmp,
    /// `rd, frs1`
    FToX,
    /// `frd, rs1`
    XToF,
    /// `rd, csr, rs1`
    Csr,
    /// `rd, csr, uimm5`
    CsrI,
    /// `fence`, optionally `fence pred, succ`
    Fence,
    /// No operands.
    Nullary,
}

/// Only assembles for RV64.
pub const RV64: u8 = 1 << 0;
/// Takes an optional trailing rounding mode.
pub const RM: u8 = 1 << 1;

pub struct Def {
    pub name: &'static str,
    pub kind: Kind,
    pub base: u32,
    pub flags: u8,
}

const fn d(name: &'static str, kind: Kind, base: u32) -> Def {
    Def {
        name,
        kind,
        base,
        flags: 0,
    }
}

const fn d64(name: &'static str, kind: Kind, base: u32) -> Def {
    Def {
        name,
        kind,
        base,
        flags: RV64,
    }
}

const fn f(name: &'static str, kind: Kind, base: u32, flags: u8) -> Def {
    Def {
        name,
        kind,
        base,
        flags,
    }
}

use Kind::*;

#[rustfmt::skip]
pub static TABLE: &[Def] = &[
    // ---- RV32I ----
    d("lui",    U,      0x0000_0037),
    d("auipc",  U,      0x0000_0017),
    d("jal",    Jal,    0x0000_006f),
    d("jalr",   Jalr,   0x0000_0067),
    d("beq",    Branch, 0x0000_0063),
    d("bne",    Branch, 0x0000_1063),
    d("blt",    Branch, 0x0000_4063),
    d("bge",    Branch, 0x0000_5063),
    d("bltu",   Branch, 0x0000_6063),
    d("bgeu",   Branch, 0x0000_7063),
    d("lb",     Load,   0x0000_0003),
    d("lh",     Load,   0x0000_1003),
    d("lw",     Load,   0x0000_2003),
    d("lbu",    Load,   0x0000_4003),
    d("lhu",    Load,   0x0000_5003),
    d("sb",     Store,  0x0000_0023),
    d("sh",     Store,  0x0000_1023),
    d("sw",     Store,  0x0000_2023),
    d("addi",   I,      0x0000_0013),
    d("slti",   I,      0x0000_2013),
    d("sltiu",  I,      0x0000_3013),
    d("xori",   I,      0x0000_4013),
    d("ori",    I,      0x0000_6013),
    d("andi",   I,      0x0000_7013),
    d("slli",   Shift,  0x0000_1013),
    d("srli",   Shift,  0x0000_5013),
    d("srai",   Shift,  0x4000_5013),
    d("add",    R,      0x0000_0033),
    d("sub",    R,      0x4000_0033),
    d("sll",    R,      0x0000_1033),
    d("slt",    R,      0x0000_2033),
    d("sltu",   R,      0x0000_3033),
    d("xor",    R,      0x0000_4033),
    d("srl",    R,      0x0000_5033),
    d("sra",    R,      0x4000_5033),
    d("or",     R,      0x0000_6033),
    d("and",    R,      0x0000_7033),
    d("fence",  Fence,  0x0000_000f),
    d("fence.i", Nullary, 0x0000_100f),
    d("fence.tso", Nullary, 0x8330_000f),
    d("ecall",  Nullary, 0x0000_0073),
    d("ebreak", Nullary, 0x0010_0073),
    d("unimp",  Nullary, 0xc000_1073),
    d("wfi",    Nullary, 0x1050_0073),
    d("mret",   Nullary, 0x3020_0073),
    d("sret",   Nullary, 0x1020_0073),
    d("dret",   Nullary, 0x7b20_0073),

    // ---- RV64I ----
    d64("lwu",   Load,   0x0000_6003),
    d64("ld",    Load,   0x0000_3003),
    d64("sd",    Store,  0x0000_3023),
    d64("addiw", I,      0x0000_001b),
    d64("slliw", ShiftW, 0x0000_101b),
    d64("srliw", ShiftW, 0x0000_501b),
    d64("sraiw", ShiftW, 0x4000_501b),
    d64("addw",  R,      0x0000_003b),
    d64("subw",  R,      0x4000_003b),
    d64("sllw",  R,      0x0000_103b),
    d64("srlw",  R,      0x0000_503b),
    d64("sraw",  R,      0x4000_503b),

    // ---- M ----
    d("mul",    R, 0x0200_0033),
    d("mulh",   R, 0x0200_1033),
    d("mulhsu", R, 0x0200_2033),
    d("mulhu",  R, 0x0200_3033),
    d("div",    R, 0x0200_4033),
    d("divu",   R, 0x0200_5033),
    d("rem",    R, 0x0200_6033),
    d("remu",   R, 0x0200_7033),
    d64("mulw",  R, 0x0200_003b),
    d64("divw",  R, 0x0200_403b),
    d64("divuw", R, 0x0200_503b),
    d64("remw",  R, 0x0200_603b),
    d64("remuw", R, 0x0200_703b),

    // ---- A ----
    // `funct5` sits in bits 31:27, leaving 26 and 25 for the `.aq` / `.rl`
    // ordering bits the mnemonic suffix sets.
    d("lr.w",       AmoLoad, 0x1000_202f),
    d("sc.w",       Amo,     0x1800_202f),
    d("amoswap.w",  Amo,     0x0800_202f),
    d("amoadd.w",   Amo,     0x0000_202f),
    d("amoxor.w",   Amo,     0x2000_202f),
    d("amoand.w",   Amo,     0x6000_202f),
    d("amoor.w",    Amo,     0x4000_202f),
    d("amomin.w",   Amo,     0x8000_202f),
    d("amomax.w",   Amo,     0xa000_202f),
    d("amominu.w",  Amo,     0xc000_202f),
    d("amomaxu.w",  Amo,     0xe000_202f),
    d64("lr.d",      AmoLoad, 0x1000_302f),
    d64("sc.d",      Amo,     0x1800_302f),
    d64("amoswap.d", Amo,     0x0800_302f),
    d64("amoadd.d",  Amo,     0x0000_302f),
    d64("amoxor.d",  Amo,     0x2000_302f),
    d64("amoand.d",  Amo,     0x6000_302f),
    d64("amoor.d",   Amo,     0x4000_302f),
    d64("amomin.d",  Amo,     0x8000_302f),
    d64("amomax.d",  Amo,     0xa000_302f),
    d64("amominu.d", Amo,     0xc000_302f),
    d64("amomaxu.d", Amo,     0xe000_302f),

    // ---- F ----
    // The default rounding mode `dyn` is `funct3 = 7`, so it is part of the
    // base word and an explicit mode overwrites it.
    f("flw",       FLoad, 0x0000_2007, 0),
    f("fsw",       FStore, 0x0000_2027, 0),
    f("fadd.s",    F3,    0x0000_7053, RM),
    f("fsub.s",    F3,    0x0800_7053, RM),
    f("fmul.s",    F3,    0x1000_7053, RM),
    f("fdiv.s",    F3,    0x1800_7053, RM),
    f("fsqrt.s",   F2,    0x5800_7053, RM),
    f("fsgnj.s",   F3,    0x2000_0053, 0),
    f("fsgnjn.s",  F3,    0x2000_1053, 0),
    f("fsgnjx.s",  F3,    0x2000_2053, 0),
    f("fmin.s",    F3,    0x2800_0053, 0),
    f("fmax.s",    F3,    0x2800_1053, 0),
    f("fmadd.s",   F4,    0x0000_7043, RM),
    f("fmsub.s",   F4,    0x0000_7047, RM),
    f("fnmsub.s",  F4,    0x0000_704b, RM),
    f("fnmadd.s",  F4,    0x0000_704f, RM),
    f("feq.s",     FCmp,  0xa000_2053, 0),
    f("flt.s",     FCmp,  0xa000_1053, 0),
    f("fle.s",     FCmp,  0xa000_0053, 0),
    f("fclass.s",  FToX,  0xe000_1053, 0),
    f("fmv.x.w",   FToX,  0xe000_0053, 0),
    f("fmv.w.x",   XToF,  0xf000_0053, 0),
    f("fcvt.w.s",  FToX,  0xc000_7053, RM),
    f("fcvt.wu.s", FToX,  0xc010_7053, RM),
    f("fcvt.s.w",  XToF,  0xd000_7053, RM),
    f("fcvt.s.wu", XToF,  0xd010_7053, RM),
    f("fcvt.l.s",  FToX,  0xc020_7053, RM | RV64),
    f("fcvt.lu.s", FToX,  0xc030_7053, RM | RV64),
    f("fcvt.s.l",  XToF,  0xd020_7053, RM | RV64),
    f("fcvt.s.lu", XToF,  0xd030_7053, RM | RV64),

    // ---- D ----
    f("fld",       FLoad,  0x0000_3007, 0),
    f("fsd",       FStore, 0x0000_3027, 0),
    f("fadd.d",    F3,    0x0200_7053, RM),
    f("fsub.d",    F3,    0x0a00_7053, RM),
    f("fmul.d",    F3,    0x1200_7053, RM),
    f("fdiv.d",    F3,    0x1a00_7053, RM),
    f("fsqrt.d",   F2,    0x5a00_7053, RM),
    f("fsgnj.d",   F3,    0x2200_0053, 0),
    f("fsgnjn.d",  F3,    0x2200_1053, 0),
    f("fsgnjx.d",  F3,    0x2200_2053, 0),
    f("fmin.d",    F3,    0x2a00_0053, 0),
    f("fmax.d",    F3,    0x2a00_1053, 0),
    f("fmadd.d",   F4,    0x0200_7043, RM),
    f("fmsub.d",   F4,    0x0200_7047, RM),
    f("fnmsub.d",  F4,    0x0200_704b, RM),
    f("fnmadd.d",  F4,    0x0200_704f, RM),
    f("feq.d",     FCmp,  0xa200_2053, 0),
    f("flt.d",     FCmp,  0xa200_1053, 0),
    f("fle.d",     FCmp,  0xa200_0053, 0),
    f("fclass.d",  FToX,  0xe200_1053, 0),
    f("fcvt.s.d",  F2,    0x4010_7053, RM),
    f("fcvt.d.s",  F2,    0x4200_0053, 0),
    f("fcvt.w.d",  FToX,  0xc200_7053, RM),
    f("fcvt.wu.d", FToX,  0xc210_7053, RM),
    f("fcvt.d.w",  XToF,  0xd200_0053, 0),
    f("fcvt.d.wu", XToF,  0xd210_0053, 0),
    f("fcvt.l.d",  FToX,  0xc220_7053, RM | RV64),
    f("fcvt.lu.d", FToX,  0xc230_7053, RM | RV64),
    f("fcvt.d.l",  XToF,  0xd220_7053, RM | RV64),
    f("fcvt.d.lu", XToF,  0xd230_7053, RM | RV64),
    f("fmv.x.d",   FToX,  0xe200_0053, RV64),
    f("fmv.d.x",   XToF,  0xf200_0053, RV64),

    // ---- Zicsr ----
    d("csrrw",  Csr,  0x0000_1073),
    d("csrrs",  Csr,  0x0000_2073),
    d("csrrc",  Csr,  0x0000_3073),
    d("csrrwi", CsrI, 0x0000_5073),
    d("csrrsi", CsrI, 0x0000_6073),
    d("csrrci", CsrI, 0x0000_7073),
];

pub fn lookup(name: &str) -> Option<&'static Def> {
    TABLE.iter().find(|d| d.name == deprecated(name))
}

/// Spellings binutils still reads for source written before an instruction
/// was renamed. They are `INSN_ALIAS` rows of the same encoding in
/// `opcodes/riscv-opc.c`, and llvm-mc takes them too.
fn deprecated(name: &str) -> &str {
    match name {
        "scall" => "ecall",
        "sbreak" => "ebreak",
        "fmv.s.x" => "fmv.w.x",
        "fmv.x.s" => "fmv.x.w",
        other => other,
    }
}

/// The immediate form a three-operand mnemonic names when its last operand
/// is written as a value rather than a register: `and a0, a1, 4` is `andi`,
/// `csrrs a0, frm, 3` is `csrrsi`.
///
/// These are aliases in `opcodes/riscv-opc.c` — one mnemonic with a second
/// row whose operand string ends in an immediate — rather than instructions
/// of their own, which is why they are resolved here and not in the table.
pub fn immediate_alias(name: &str) -> Option<&'static str> {
    Some(match name {
        "add" => "addi",
        "and" => "andi",
        "or" => "ori",
        "xor" => "xori",
        "sll" => "slli",
        "srl" => "srli",
        "sra" => "srai",
        "slt" => "slti",
        "sltu" => "sltiu",
        "addw" => "addiw",
        "sllw" => "slliw",
        "srlw" => "srliw",
        "sraw" => "sraiw",
        "csrrw" => "csrrwi",
        "csrrs" => "csrrsi",
        "csrrc" => "csrrci",
        _ => return None,
    })
}

/// Rounding-mode names, in `funct3` order. `dyn` reads the mode out of `fcsr`.
pub fn rounding_mode(name: &str) -> Option<u32> {
    Some(match name {
        "rne" => 0,
        "rtz" => 1,
        "rdn" => 2,
        "rup" => 3,
        "rmm" => 4,
        "dyn" => 7,
        _ => return None,
    })
}

/// The `iorw` letters of a `fence` operand, as a four-bit set.
pub fn fence_set(text: &str) -> Option<u32> {
    let mut bits = 0;
    for c in text.chars() {
        bits |= match c {
            'i' => 8,
            'o' => 4,
            'r' => 2,
            'w' => 1,
            _ => return None,
        };
    }
    Some(bits)
}

/// The control and status registers that have a name in common use. Anything
/// else can still be named by number.
#[rustfmt::skip]
pub fn csr(name: &str) -> Option<u32> {
    Some(match name {
        "fflags" => 0x001, "frm" => 0x002, "fcsr" => 0x003,
        "cycle" => 0xc00, "time" => 0xc01, "instret" => 0xc02,
        "sstatus" => 0x100, "sie" => 0x104, "stvec" => 0x105,
        "scounteren" => 0x106, "sscratch" => 0x140, "sepc" => 0x141,
        "scause" => 0x142, "stval" => 0x143, "sip" => 0x144, "satp" => 0x180,
        "mstatus" => 0x300, "misa" => 0x301, "medeleg" => 0x302,
        "mideleg" => 0x303, "mie" => 0x304, "mtvec" => 0x305,
        "mcounteren" => 0x306, "mscratch" => 0x340, "mepc" => 0x341,
        "mcause" => 0x342, "mtval" => 0x343, "mip" => 0x344,
        "mvendorid" => 0xf11, "marchid" => 0xf12, "mimpid" => 0xf13,
        "mhartid" => 0xf14,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_mnemonic_is_defined_twice() {
        let mut names: Vec<&str> = TABLE.iter().map(|d| d.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate mnemonic in the table");
    }

    #[test]
    fn every_base_word_has_a_plausible_opcode() {
        for def in TABLE {
            // The low two bits of a 32-bit instruction are always set; a base
            // word with them clear means a typo in the opcode column.
            assert_eq!(def.base & 3, 3, "`{}` has a compressed opcode", def.name);
        }
    }
}
