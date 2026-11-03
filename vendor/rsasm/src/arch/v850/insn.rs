//! The instruction table.
//!
//! V850 instructions are one, two or three 16-bit words, little-endian. An
//! entry holds the first two words as one `u32` (word 0 in the low half, so
//! the value reads the way the bytes are laid down) plus how many of those
//! bytes the instruction really has; operands that live in a trailing third
//! word or a 32-bit immediate append their own bytes.
//!
//! Operand fields, in the positions the V850 manuals call reg1/reg2/reg3:
//!
//! ```text
//! word 0:  15..11 reg2   10..5 opcode   4..0 reg1
//! word 1:  15..11 reg3                                  (bits 31..27 of the u32)
//! ```
//!
//! A mnemonic can have several entries, tried in order; the first whose
//! operands all fit wins. That order is significant and follows GNU as, whose
//! choices are the reference: `mov 16, r1` misses the 5-bit form and lands on
//! the 48-bit one, and `shl r1, r2, r3` is tried before `shl r1, r2`.

/// Which cores an entry exists on.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Cpu {
    /// The original V850 and everything after it.
    All,
    /// The original V850 only: RH850 re-encoded `ldsr`/`stsr` operands.
    V850Only,
    /// RH850 (GNU as `-mv850e3v5`). This includes the V850E1/E2 additions,
    /// since rsasm offers no level between the two.
    Rh850,
}

impl Cpu {
    pub fn allows(self, rh850: bool) -> bool {
        match self {
            Cpu::All => true,
            Cpu::V850Only => !rh850,
            Cpu::Rh850 => rh850,
        }
    }
}

/// A general-register field.
#[derive(Copy, Clone, Debug)]
pub struct RegF {
    pub shift: u8,
    pub not_r0: bool,
    /// Only even registers: the FPU's double-precision operands name the low
    /// register of a pair. The field still holds the register number, since
    /// an even number's low bit is zero anyway.
    pub even: bool,
}

/// reg1, bits 4..0.
pub const R1: RegF = RegF {
    shift: 0,
    not_r0: false,
    even: false,
};
/// reg2, bits 15..11.
pub const R2: RegF = RegF {
    shift: 11,
    not_r0: false,
    even: false,
};
/// reg3, bits 31..27 (word 1, bits 15..11).
pub const R3: RegF = RegF {
    shift: 27,
    not_r0: false,
    even: false,
};
/// reg4 of `mac`, bits 20..16 (word 1, bits 4..0).
pub const R4: RegF = RegF {
    shift: 16,
    not_r0: false,
    even: false,
};

const fn nz(r: RegF) -> RegF {
    RegF { not_r0: true, ..r }
}

const fn ev(r: RegF) -> RegF {
    RegF { even: true, ..r }
}

/// How a plain immediate's range is checked.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Range {
    /// GNU as's default for a field without special rules: either reading of
    /// the bit pattern, `-2^(n-1)` to `2^n - 1`. `add 31, r1` and `add -1, r1`
    /// both fit a 5-bit field.
    Either,
    /// `0` to `2^n - 1`.
    Unsigned,
    /// `-2^(n-1)` to `2^(n-1) - 1`, and a value outside it only means this
    /// entry does not match, so the next, wider one is tried. GNU as does this
    /// for `mov`, `jr`, `jarl` and the `ld.`/`st.` family.
    SignedOrWider,
}

/// Immediate operands.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ImmF {
    /// A contiguous field.
    Bits { shift: u8, bits: u8, range: Range },
    /// Like `Bits`, but zero is refused (`fetrap`).
    NonZero { shift: u8, bits: u8 },
    /// The 16-bit immediate in word 1 of `movea`, `addi` and friends, which
    /// takes `hi()`, `lo()`, `hi0()` and `zdaoff()`.
    Imm16,
    /// `mul`'s 9-bit immediate: bits 4..0 where reg1 would be, bits 8..5 at
    /// 21..18.
    Imm9 { signed: bool },
    /// `syscall`'s 8-bit vector: bits 4..0 and 29..27.
    Vector8,
    /// `dbtag`'s 10-bit value: bits 4..0 and 31..27.
    Imm10,
    /// The 32-bit immediate trailing the 48-bit `mov`.
    Imm32,
    /// A 22-bit displacement, `jr` and `jarl`.
    Disp22,
    /// A 32-bit displacement trailing the 48-bit `jr` and `jarl`.
    Disp32,
}

/// Displacements of `disp[reg]` operands.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Disp {
    /// 16 bits in word 1. `strict` is `ld.b`/`st.b`, which move on to the
    /// 23-bit form rather than accept the unsigned reading.
    D16 { strict: bool },
    /// 16 bits in word 1 whose bit 0 belongs to the opcode, so the
    /// displacement must be even: `ld.h`, `ld.w`, `st.h`, `st.w`.
    D16Even,
    /// `ld.bu`: 16 bits with the low bit moved to bit 5, because word 1's bit
    /// 0 is again the opcode's.
    D16Split,
    /// The 23-bit displacement of the 48-bit loads and stores: bits 6..0 in
    /// word 1 above the opcode, the rest in word 2.
    D23 { even: bool },
    /// `jmp disp32[reg]`: a trailing 32-bit word, bit 0 always clear.
    D32,
}

/// Displacements of the 16-bit `sld`/`sst` forms, which are always relative
/// to `ep`. Their fields are narrow and the width scales with the access
/// size, since an element pointer's words are aligned.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum EpDisp {
    /// `sld.b`/`sst.b`: 7 bits.
    D7,
    /// `sld.h`/`sst.h`: 8 bits, even, stored halved in 7.
    D8Half,
    /// `sld.w`/`sst.w`: 8 bits, a multiple of 4, stored halved in bits 6..1
    /// because bit 0 is the opcode.
    D8Word,
    /// `sld.bu`: 4 bits.
    D4,
    /// `sld.hu`: 5 bits, even, stored halved.
    D5Half,
}

/// The value `prepare` loads into `ep`, and how it is stored after the
/// instruction.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PrepImm {
    /// A 16-bit value, sign-extended by the CPU.
    Lo,
    /// A 16-bit value, shifted left 16 by the CPU.
    Hi,
    /// A full 32-bit value.
    Full,
}

#[derive(Copy, Clone, Debug)]
pub enum Slot {
    Reg(RegF),
    /// A register used as an address: `[r1]`. A bare `r1` is accepted too,
    /// as GNU as does, so `jmp r1` is `jmp [r1]`.
    Base(RegF),
    Mem(Disp, RegF),
    /// `disp[ep]`.
    Ep(EpDisp),
    Imm(ImmF),
    /// A condition name or number, 4 bits.
    Cond {
        shift: u8,
        allow_sa: bool,
    },
    /// A floating-point condition, bits 30..27.
    FloatCond,
    /// The FPU condition-flag index, bits 19..17.
    Fff,
    /// A V850 system register number, 5 bits.
    OldSysReg {
        shift: u8,
    },
    /// An RH850 system register: regID at `shift`, and a group number in bits
    /// 31..27 if the value is written as `selID * 32 + regID`.
    SysReg {
        shift: u8,
    },
    /// The RH850 system register group, bits 31..27.
    SelId,
    /// An RH850 virtualisation register, `vr0`-`vr31`.
    VReg {
        shift: u8,
    },
    CacheOp,
    PrefOp,
    /// `prepare`/`dispose`'s register list.
    List,
    /// `prepare`'s `sp` operand.
    Sp,
    PrepImm(PrepImm),
    /// `bins`'s bit position; the encoding depends on the width that follows.
    BinsPos,
    BinsWidth,
}

#[derive(Copy, Clone, Debug)]
pub struct Entry {
    pub name: &'static str,
    /// Words 0 and 1, with operand fields zero.
    pub word: u32,
    /// Bytes of `word` the instruction has: 2 or 4.
    pub len: u8,
    pub slots: &'static [Slot],
    pub cpu: Cpu,
}

const fn e16(name: &'static str, word: u32, slots: &'static [Slot], cpu: Cpu) -> Entry {
    Entry {
        name,
        word,
        len: 2,
        slots,
        cpu,
    }
}

const fn e32(name: &'static str, w0: u32, w1: u32, slots: &'static [Slot], cpu: Cpu) -> Entry {
    Entry {
        name,
        word: w0 | (w1 << 16),
        len: 4,
        slots,
        cpu,
    }
}

/// Format I and II opcodes: the 6-bit opcode in bits 10..5.
const fn op(o: u32) -> u32 {
    o << 5
}

use Cpu::{All, Rh850, V850Only};
use Slot::*;

const I5: Slot = Imm(ImmF::Bits {
    shift: 0,
    bits: 5,
    range: Range::Either,
});
const I5_MOV: Slot = Imm(ImmF::Bits {
    shift: 0,
    bits: 5,
    range: Range::SignedOrWider,
});
const U5: Slot = Imm(ImmF::Bits {
    shift: 0,
    bits: 5,
    range: Range::Either,
});
const I16: Slot = Imm(ImmF::Imm16);
/// The bit number of `set1` and friends, bits 13..11.
const B3: Slot = Imm(ImmF::Bits {
    shift: 11,
    bits: 3,
    range: Range::Either,
});
/// `prepare`/`dispose`'s stack-frame size in words, bits 5..1.
const IMM5_FRAME: Slot = Imm(ImmF::Bits {
    shift: 1,
    bits: 5,
    range: Range::Unsigned,
});

const RR: &[Slot] = &[Reg(R1), Reg(R2)];
const RR_NZ: &[Slot] = &[Reg(R1), Reg(nz(R2))];
const RRR: &[Slot] = &[Reg(R1), Reg(R2), Reg(R3)];
const R2R3: &[Slot] = &[Reg(R2), Reg(R3)];
const IRR: &[Slot] = &[I16, Reg(R1), Reg(R2)];
const IRR_NZ: &[Slot] = &[I16, Reg(R1), Reg(nz(R2))];
const NONE: &[Slot] = &[];
const BIT_MEM: &[Slot] = &[B3, Mem(Disp::D16 { strict: false }, R1)];
const REG_BASE: &[Slot] = &[Reg(R2), Base(R1)];

/// Two-word opcodes with an otherwise all-zero first word's operand area,
/// `0x07e0` being "format IX/X" in the V850 manuals.
const X: u32 = 0x07e0;

#[rustfmt::skip]
pub static TABLE: &[Entry] = &[
    // ---- format I/II: register-register and 5-bit immediate ---------------
    e16("mov",     op(0x00), &[Reg(R1), Reg(nz(R2))], All),
    e16("mov",     op(0x10), &[I5_MOV, Reg(nz(R2))], All),
    // The 48-bit form. Its register is reg1 and nothing forbids r0, so
    // `mov 5, r0` quietly becomes this rather than an error: GNU as does it.
    e16("mov",     0x0620, &[Imm(ImmF::Imm32), Reg(R1)], Rh850),
    e16("not",     op(0x01), RR, All),
    e16("divh",    op(0x02), &[Reg(nz(R1)), Reg(nz(R2))], All),
    e16("satsubr", op(0x04), RR_NZ, All),
    e16("satsub",  op(0x05), RR_NZ, All),
    e16("satadd",  op(0x11), &[I5, Reg(nz(R2))], All),
    e16("satadd",  op(0x06), RR_NZ, All),
    e16("mulh",    op(0x17), &[I5, Reg(nz(R2))], All),
    e16("mulh",    op(0x07), RR_NZ, All),
    e16("or",      op(0x08), RR, All),
    e16("xor",     op(0x09), RR, All),
    e16("and",     op(0x0a), RR, All),
    e16("tst",     op(0x0b), RR, All),
    e16("subr",    op(0x0c), RR, All),
    e16("sub",     op(0x0d), RR, All),
    e16("add",     op(0x0e), RR, All),
    e16("add",     op(0x12), &[I5, Reg(R2)], All),
    e16("cmp",     op(0x0f), RR, All),
    e16("cmp",     op(0x13), &[I5, Reg(R2)], All),
    e16("shr",     op(0x14), &[U5, Reg(R2)], All),
    e16("sar",     op(0x15), &[U5, Reg(R2)], All),
    e16("shl",     op(0x16), &[U5, Reg(R2)], All),
    e16("nop",     0x0000, NONE, All),
    // `mov 1, r0`: a no-op GDB plants as a software breakpoint.
    e16("breakpoint", 0x0001, NONE, All),
    e16("jmp",     0x0060, &[Base(R1)], All),
    e16("switch",  0x0040, &[Reg(nz(R1))], Rh850),
    e16("zxb",     0x0080, &[Reg(R1)], Rh850),
    e16("sxb",     0x00a0, &[Reg(R1)], Rh850),
    e16("zxh",     0x00c0, &[Reg(R1)], Rh850),
    e16("sxh",     0x00e0, &[Reg(R1)], Rh850),
    e16("callt",   0x0200, &[Imm(ImmF::Bits { shift: 0, bits: 6, range: Range::Either })], Rh850),
    e16("fetrap",  0x0040, &[Imm(ImmF::NonZero { shift: 11, bits: 4 })], Rh850),
    e16("rie",     0x0040, NONE, Rh850),
    e16("synci",   0x001c, NONE, Rh850),
    e16("synce",   0x001d, NONE, Rh850),
    e16("syncm",   0x001e, NONE, Rh850),
    e16("syncp",   0x001f, NONE, Rh850),
    e16("dbtrap",  0xf840, NONE, Rh850),
    e16("rmtrap",  0xf040, NONE, Rh850),
    e16("dbcp",    0xe840, NONE, Rh850),
    e16("dbhvtrap", 0xe040, NONE, Rh850),

    // ---- format IV: short loads and stores relative to ep ------------------
    e16("sld.b",   0x0300, &[Ep(EpDisp::D7), Reg(R2)], All),
    e16("sld.bu",  0x0060, &[Ep(EpDisp::D4), Reg(nz(R2))], Rh850),
    e16("sld.h",   0x0400, &[Ep(EpDisp::D8Half), Reg(R2)], All),
    e16("sld.hu",  0x0070, &[Ep(EpDisp::D5Half), Reg(nz(R2))], Rh850),
    e16("sld.w",   0x0500, &[Ep(EpDisp::D8Word), Reg(R2)], All),
    e16("sst.b",   0x0380, &[Reg(R2), Ep(EpDisp::D7)], All),
    e16("sst.h",   0x0480, &[Reg(R2), Ep(EpDisp::D8Half)], All),
    e16("sst.w",   0x0501, &[Reg(R2), Ep(EpDisp::D8Word)], All),

    // ---- format VI: 16-bit immediate in word 1 -----------------------------
    e32("addi",    op(0x30), 0, IRR, All),
    e32("movea",   op(0x31), 0, IRR_NZ, All),
    e32("movhi",   op(0x32), 0, IRR_NZ, All),
    e32("satsubi", op(0x33), 0, IRR_NZ, All),
    e32("ori",     op(0x34), 0, IRR, All),
    e32("xori",    op(0x35), 0, IRR, All),
    e32("andi",    op(0x36), 0, IRR, All),
    e32("mulhi",   op(0x37), 0, IRR_NZ, All),

    // ---- format VII: 16-bit displacement loads and stores ------------------
    e32("ld.b",    0x0700, 0x0000, &[Mem(Disp::D16 { strict: true }, R1), Reg(R2)], All),
    e32("ld.b",    0x0780, 0x0005, &[Mem(Disp::D23 { even: false }, R1), Reg(R3)], Rh850),
    e32("ld.bu",   0x0780, 0x0001, &[Mem(Disp::D16Split, R1), Reg(nz(R2))], Rh850),
    e32("ld.bu",   0x07a0, 0x0005, &[Mem(Disp::D23 { even: false }, R1), Reg(R3)], Rh850),
    e32("ld.h",    0x0720, 0x0000, &[Mem(Disp::D16Even, R1), Reg(R2)], All),
    e32("ld.h",    0x0780, 0x0007, &[Mem(Disp::D23 { even: true }, R1), Reg(R3)], Rh850),
    e32("ld.hu",   0x07e0, 0x0001, &[Mem(Disp::D16Even, R1), Reg(nz(R2))], Rh850),
    e32("ld.hu",   0x07a0, 0x0007, &[Mem(Disp::D23 { even: true }, R1), Reg(R3)], Rh850),
    e32("ld.w",    0x0720, 0x0001, &[Mem(Disp::D16Even, R1), Reg(R2)], All),
    e32("ld.w",    0x0780, 0x0009, &[Mem(Disp::D23 { even: true }, R1), Reg(R3)], Rh850),
    e32("ld.dw",   0x07a0, 0x0009, &[Mem(Disp::D23 { even: true }, R1), Reg(ev(R3))], Rh850),
    e32("st.b",    0x0740, 0x0000, &[Reg(R2), Mem(Disp::D16 { strict: true }, R1)], All),
    e32("st.b",    0x0780, 0x000d, &[Reg(R3), Mem(Disp::D23 { even: false }, R1)], Rh850),
    e32("st.h",    0x0760, 0x0000, &[Reg(R2), Mem(Disp::D16Even, R1)], All),
    e32("st.h",    0x07a0, 0x000d, &[Reg(R3), Mem(Disp::D23 { even: true }, R1)], Rh850),
    e32("st.w",    0x0760, 0x0001, &[Reg(R2), Mem(Disp::D16Even, R1)], All),
    e32("st.w",    0x0780, 0x000f, &[Reg(R3), Mem(Disp::D23 { even: true }, R1)], Rh850),
    e32("st.dw",   0x07a0, 0x000f, &[Reg(ev(R3)), Mem(Disp::D23 { even: true }, R1)], Rh850),
    e32("ldl.w",   X, 0x0378, &[Base(R1), Reg(R3)], Rh850),
    e32("stc.w",   X, 0x037a, &[Reg(R3), Base(R1)], Rh850),

    // ---- format V: jumps -----------------------------------------------------
    e32("jarl",    0xc7e0, 0x0160, &[Base(R1), Reg(nz(R3))], Rh850),
    e32("jarl",    0x0780, 0x0000, &[Imm(ImmF::Disp22), Reg(nz(R2))], All),
    e16("jarl",    0x02e0, &[Imm(ImmF::Disp32), Reg(nz(R1))], Rh850),
    e32("jr",      0x0780, 0x0000, &[Imm(ImmF::Disp22)], All),
    e16("jr",      0x02e0, &[Imm(ImmF::Disp32)], Rh850),
    e16("jmp",     0x06e0, &[Mem(Disp::D32, R1)], Rh850),

    // ---- format VIII: bit manipulation ---------------------------------------
    e32("set1",    0x07c0, 0, BIT_MEM, All),
    e32("set1",    X, 0x00e0, REG_BASE, Rh850),
    e32("not1",    0x47c0, 0, BIT_MEM, All),
    e32("not1",    X, 0x00e2, REG_BASE, Rh850),
    e32("clr1",    0x87c0, 0, BIT_MEM, All),
    e32("clr1",    X, 0x00e4, REG_BASE, Rh850),
    e32("tst1",    0xc7c0, 0, BIT_MEM, All),
    e32("tst1",    X, 0x00e6, REG_BASE, Rh850),

    // ---- format IX/X: two-word register and control instructions -----------
    e32("setf",    X, 0x0000, &[Cond { shift: 0, allow_sa: true }, Reg(R2)], All),
    e32("sasf",    X, 0x0200, &[Cond { shift: 0, allow_sa: true }, Reg(R2)], Rh850),
    e32("ldsr",    X, 0x0020, &[Reg(R1), SysReg { shift: 11 }, SelId], Rh850),
    e32("ldsr",    X, 0x0020, &[Reg(R1), SysReg { shift: 11 }], Rh850),
    e32("ldsr",    X, 0x0020, &[Reg(R1), OldSysReg { shift: 11 }], V850Only),
    e32("stsr",    X, 0x0040, &[SysReg { shift: 0 }, Reg(R2), SelId], Rh850),
    e32("stsr",    X, 0x0040, &[SysReg { shift: 0 }, Reg(R2)], Rh850),
    e32("stsr",    X, 0x0040, &[OldSysReg { shift: 0 }, Reg(R2)], V850Only),
    e32("shr",     X, 0x0082, RRR, Rh850),
    e32("shr",     X, 0x0080, RR, All),
    e32("sar",     X, 0x00a2, RRR, Rh850),
    e32("sar",     X, 0x00a0, RR, All),
    e32("shl",     X, 0x00c2, RRR, Rh850),
    e32("shl",     X, 0x00c0, RR, All),
    e32("rotl",    X, 0x00c6, RRR, Rh850),
    e32("rotl",    X, 0x00c4, &[U5, Reg(R2), Reg(R3)], Rh850),
    e32("caxi",    X, 0x00ee, &[Base(R1), Reg(R2), Reg(R3)], Rh850),
    e32("trap",    X, 0x0100, &[U5], All),
    e32("halt",    X, 0x0120, NONE, All),
    e32("snooze",  0x0fe0, 0x0120, NONE, Rh850),
    e32("reti",    X, 0x0140, NONE, All),
    e32("ctret",   X, 0x0144, NONE, Rh850),
    e32("dbret",   X, 0x0146, NONE, Rh850),
    e32("eiret",   X, 0x0148, NONE, Rh850),
    e32("feret",   X, 0x014a, NONE, Rh850),
    e32("est",     X, 0x0132, NONE, Rh850),
    e32("dst",     X, 0x0134, NONE, Rh850),
    e32("di",      X, 0x0160, NONE, All),
    e32("ei",      0x87e0, 0x0160, NONE, All),
    e32("mul",     X, 0x0220, RRR, Rh850),
    e32("mul",     X, 0x0240, &[Imm(ImmF::Imm9 { signed: true }), Reg(R2), Reg(R3)], Rh850),
    e32("mulu",    X, 0x0222, RRR, Rh850),
    e32("mulu",    X, 0x0242, &[Imm(ImmF::Imm9 { signed: false }), Reg(R2), Reg(R3)], Rh850),
    e32("divh",    X, 0x0280, RRR, Rh850),
    e32("divhu",   X, 0x0282, RRR, Rh850),
    e32("div",     X, 0x02c0, RRR, Rh850),
    e32("divu",    X, 0x02c2, RRR, Rh850),
    e32("divq",    X, 0x02fc, RRR, Rh850),
    e32("divqu",   X, 0x02fe, RRR, Rh850),
    e32("cmov",    X, 0x0320, &[Cond { shift: 17, allow_sa: true }, Reg(R1), Reg(R2), Reg(R3)], Rh850),
    e32("cmov",    X, 0x0300, &[Cond { shift: 17, allow_sa: true }, I5, Reg(R2), Reg(R3)], Rh850),
    e32("bsw",     X, 0x0340, R2R3, Rh850),
    e32("bsh",     X, 0x0342, R2R3, Rh850),
    e32("hsw",     X, 0x0344, R2R3, Rh850),
    e32("hsh",     X, 0x0346, R2R3, Rh850),
    e32("sch0r",   X, 0x0360, R2R3, Rh850),
    e32("sch1r",   X, 0x0362, R2R3, Rh850),
    e32("sch0l",   X, 0x0364, R2R3, Rh850),
    e32("sch1l",   X, 0x0366, R2R3, Rh850),
    e32("sbf",     X, 0x0380, &[Cond { shift: 17, allow_sa: false }, Reg(R1), Reg(R2), Reg(R3)], Rh850),
    e32("satsub",  X, 0x039a, RRR, Rh850),
    e32("adf",     X, 0x03a0, &[Cond { shift: 17, allow_sa: false }, Reg(R1), Reg(R2), Reg(R3)], Rh850),
    e32("satadd",  X, 0x03ba, RRR, Rh850),
    e32("mac",     X, 0x03c0, &[Reg(R1), Reg(R2), Reg(ev(R3)), Reg(ev(R4))], Rh850),
    e32("macu",    X, 0x03e0, &[Reg(R1), Reg(R2), Reg(ev(R3)), Reg(ev(R4))], Rh850),
    e32("rie",     0x07f0, 0x0000, &[
        Imm(ImmF::Bits { shift: 11, bits: 5, range: Range::Either }),
        Imm(ImmF::Bits { shift: 0, bits: 4, range: Range::Either }),
    ], Rh850),
    e32("syscall", 0xd7e0, 0x0160, &[Imm(ImmF::Vector8)], Rh850),
    e32("hvcall",  0xd7e0, 0x4160, &[Imm(ImmF::Vector8)], Rh850),
    e32("hvtrap",  X, 0x0110, &[Imm(ImmF::Bits { shift: 0, bits: 5, range: Range::Unsigned })], Rh850),
    e32("dbtag",   0xcfe0, 0x0160, &[Imm(ImmF::Imm10)], Rh850),
    e32("dbpush",  0x5fe0, 0x0160, &[Reg(R1), Reg(R3)], Rh850),
    e32("pushsp",  0x47e0, 0x0160, &[Reg(R1), Reg(R3)], Rh850),
    e32("popsp",   0x67e0, 0x0160, &[Reg(R1), Reg(R3)], Rh850),
    e32("cache",   0xe7e0, 0x0160, &[CacheOp, Base(R1)], Rh850),
    e32("pref",    0xdfe0, 0x0160, &[PrefOp, Base(R1)], Rh850),
    e32("tlbvi",   0x87e0, 0x8160, NONE, Rh850),
    e32("tlbai",   0x87e0, 0x8960, NONE, Rh850),
    e32("tlbs",    0x87e0, 0xc160, NONE, Rh850),
    e32("tlbw",    0x87e0, 0xe160, NONE, Rh850),
    e32("tlbr",    0x87e0, 0xe960, NONE, Rh850),
    e32("bins",    X, 0x0000, &[Reg(R1), BinsPos, BinsWidth, Reg(R2)], Rh850),
    e32("ldtc.gr", X, 0x0032, RR, Rh850),
    e32("ldtc.sr", X, 0x0030, &[Reg(R1), SysReg { shift: 11 }, SelId], Rh850),
    e32("ldtc.sr", X, 0x0030, &[Reg(R1), SysReg { shift: 11 }], Rh850),
    e32("ldtc.vr", X, 0x0832, &[Reg(R1), VReg { shift: 11 }], Rh850),
    e32("ldtc.pc", X, 0xf832, &[Reg(R1)], Rh850),
    e32("ldvc.sr", X, 0x0034, &[Reg(R1), SysReg { shift: 11 }, SelId], Rh850),
    e32("ldvc.sr", X, 0x0034, &[Reg(R1), SysReg { shift: 11 }], Rh850),
    e32("sttc.gr", X, 0x0052, RR, Rh850),
    e32("sttc.sr", X, 0x0050, &[SysReg { shift: 0 }, Reg(R2), SelId], Rh850),
    e32("sttc.sr", X, 0x0050, &[SysReg { shift: 0 }, Reg(R2)], Rh850),
    e32("sttc.vr", X, 0x0852, &[VReg { shift: 0 }, Reg(R2)], Rh850),
    e32("sttc.pc", X, 0xf852, &[Reg(R2)], Rh850),
    e32("stvc.sr", X, 0x0054, &[SysReg { shift: 0 }, Reg(R2), SelId], Rh850),
    e32("stvc.sr", X, 0x0054, &[SysReg { shift: 0 }, Reg(R2)], Rh850),

    // ---- format XIII: prepare and dispose ------------------------------------
    //
    // The low five bits of word 1 pick what follows the list: nothing, `sp`,
    // or a 16-bit, shifted 16-bit or 32-bit value for ep. A value is tried
    // narrowest first.
    e32("prepare", 0x0780, 0x0003, &[List, IMM5_FRAME, Sp], Rh850),
    e32("prepare", 0x0780, 0x000b, &[List, IMM5_FRAME, PrepImm(PrepImm::Lo)], Rh850),
    e32("prepare", 0x0780, 0x0013, &[List, IMM5_FRAME, PrepImm(PrepImm::Hi)], Rh850),
    e32("prepare", 0x0780, 0x001b, &[List, IMM5_FRAME, PrepImm(PrepImm::Full)], Rh850),
    e32("prepare", 0x0780, 0x0001, &[List, IMM5_FRAME], Rh850),
    e32("dispose", 0x0640, 0x0000, &[IMM5_FRAME, List, Base(RegF { shift: 16, not_r0: true, even: false })], Rh850),
    e32("dispose", 0x0640, 0x0000, &[IMM5_FRAME, List], Rh850),
];

/// Every entry for `name`, in table order, followed by the FPU's.
pub fn entries(name: &str) -> impl Iterator<Item = &'static Entry> + '_ {
    TABLE
        .iter()
        .chain(super::fpu::TABLE.iter())
        .filter(move |e| e.name == name)
}
