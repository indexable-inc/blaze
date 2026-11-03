//! The RH850 floating-point instructions (FPU-3; V850E2V3 FPU-2 plus the
//! half-precision conversions and fused multiply-adds).
//!
//! Every one is a two-word format-XI instruction whose word 1 holds the
//! operation and whose spare reg1 bits in word 0 select a rounding or
//! conversion variant. Double-precision operands are 64-bit values in a
//! register *pair*, named by the even register that holds the low half, so
//! their fields refuse odd registers (see [`super::insn::RegF::even`]).
//!
//! The FPU-2 four-operand forms (`maddf.s` and friends) belong to V850E2V3
//! alone and were replaced on RH850 by the three-operand `fmaf.s` family, so
//! they are not here: GNU as rejects them with `-mv850e3v5` too.

use super::insn::{Cpu::Rh850, Entry, R1, R2, R3, RegF, Slot, Slot::*};

const fn ev(r: RegF) -> RegF {
    RegF { even: true, ..r }
}

const fn nz(r: RegF) -> RegF {
    RegF { not_r0: true, ..r }
}

const fn f(name: &'static str, w0: u32, w1: u32, slots: &'static [Slot]) -> Entry {
    Entry {
        name,
        word: w0 | (w1 << 16),
        len: 4,
        slots,
        cpu: Rh850,
    }
}

/// Single to single, or any conversion whose both sides fit one register.
const SS: &[Slot] = &[Reg(R2), Reg(R3)];
/// Double to double.
const DD: &[Slot] = &[Reg(ev(R2)), Reg(ev(R3))];
/// Double to a 32-bit result.
const DW: &[Slot] = &[Reg(ev(R2)), Reg(R3)];
/// 32-bit source to a double.
const WD: &[Slot] = &[Reg(R2), Reg(ev(R3))];
const SSS: &[Slot] = &[Reg(R1), Reg(R2), Reg(R3)];
const DDD: &[Slot] = &[Reg(ev(R1)), Reg(ev(R2)), Reg(ev(R3))];

#[rustfmt::skip]
pub static TABLE: &[Entry] = &[
    f("absf.d",    0x07e0, 0x0458, DD),
    f("absf.s",    0x07e0, 0x0448, SS),
    f("addf.d",    0x07e0, 0x0470, DDD),
    f("addf.s",    0x07e0, 0x0460, SSS),
    f("ceilf.dl",  0x07e2, 0x0454, DD),
    f("ceilf.dul", 0x07f2, 0x0454, DD),
    f("ceilf.duw", 0x07f2, 0x0450, DW),
    f("ceilf.dw",  0x07e2, 0x0450, DW),
    f("ceilf.sl",  0x07e2, 0x0444, WD),
    f("ceilf.sul", 0x07f2, 0x0444, WD),
    f("ceilf.suw", 0x07f2, 0x0440, SS),
    f("ceilf.sw",  0x07e2, 0x0440, SS),
    // The flag index is optional and defaults to 0.
    f("cmovf.d",   0x07e0, 0x0410, &[Fff, Reg(ev(R1)), Reg(ev(R2)), Reg(nz(ev(R3)))]),
    f("cmovf.d",   0x07e0, 0x0410, &[Reg(ev(R1)), Reg(ev(R2)), Reg(nz(ev(R3)))]),
    f("cmovf.s",   0x07e0, 0x0400, &[Fff, Reg(R1), Reg(R2), Reg(nz(R3))]),
    f("cmovf.s",   0x07e0, 0x0400, &[Reg(R1), Reg(R2), Reg(nz(R3))]),
    // The operands are written reg2 first: `cmpf.s cond, a, b` compares a
    // with b, and a is the one in the reg2 field.
    f("cmpf.d",    0x07e0, 0x0430, &[FloatCond, Reg(ev(R2)), Reg(ev(R1)), Fff]),
    f("cmpf.d",    0x07e0, 0x0430, &[FloatCond, Reg(ev(R2)), Reg(ev(R1))]),
    f("cmpf.s",    0x07e0, 0x0420, &[FloatCond, Reg(R2), Reg(R1), Fff]),
    f("cmpf.s",    0x07e0, 0x0420, &[FloatCond, Reg(R2), Reg(R1)]),
    f("cvtf.dl",   0x07e4, 0x0454, DD),
    f("cvtf.ds",   0x07e3, 0x0452, DW),
    f("cvtf.dul",  0x07f4, 0x0454, DD),
    f("cvtf.duw",  0x07f4, 0x0450, DW),
    f("cvtf.dw",   0x07e4, 0x0450, DW),
    f("cvtf.hs",   0x07e2, 0x0442, SS),
    f("cvtf.ld",   0x07e1, 0x0452, DD),
    f("cvtf.ls",   0x07e1, 0x0442, DW),
    f("cvtf.sd",   0x07e2, 0x0452, WD),
    f("cvtf.sh",   0x07e3, 0x0442, SS),
    f("cvtf.sl",   0x07e4, 0x0444, WD),
    f("cvtf.sul",  0x07f4, 0x0444, WD),
    f("cvtf.suw",  0x07f4, 0x0440, SS),
    f("cvtf.sw",   0x07e4, 0x0440, SS),
    f("cvtf.uld",  0x07f1, 0x0452, DD),
    f("cvtf.uls",  0x07f1, 0x0442, DW),
    f("cvtf.uwd",  0x07f0, 0x0452, WD),
    f("cvtf.uws",  0x07f0, 0x0442, SS),
    f("cvtf.wd",   0x07e0, 0x0452, WD),
    f("cvtf.ws",   0x07e0, 0x0442, SS),
    f("divf.d",    0x07e0, 0x047e, DDD),
    f("divf.s",    0x07e0, 0x046e, &[Reg(nz(R1)), Reg(R2), Reg(R3)]),
    f("floorf.dl", 0x07e3, 0x0454, DD),
    f("floorf.dul", 0x07f3, 0x0454, DD),
    f("floorf.duw", 0x07f3, 0x0450, DW),
    f("floorf.dw", 0x07e3, 0x0450, DW),
    f("floorf.sl", 0x07e3, 0x0444, WD),
    f("floorf.sul", 0x07f3, 0x0444, WD),
    f("floorf.suw", 0x07f3, 0x0440, SS),
    f("floorf.sw", 0x07e3, 0x0440, SS),
    f("fmaf.s",    0x07e0, 0x04e0, SSS),
    f("fmsf.s",    0x07e0, 0x04e2, SSS),
    f("fnmaf.s",   0x07e0, 0x04e4, SSS),
    f("fnmsf.s",   0x07e0, 0x04e6, SSS),
    f("maxf.d",    0x07e0, 0x0478, DDD),
    f("maxf.s",    0x07e0, 0x0468, SSS),
    f("minf.d",    0x07e0, 0x047a, DDD),
    f("minf.s",    0x07e0, 0x046a, SSS),
    f("mulf.d",    0x07e0, 0x0474, DDD),
    f("mulf.s",    0x07e0, 0x0464, SSS),
    f("negf.d",    0x07e1, 0x0458, DD),
    f("negf.s",    0x07e1, 0x0448, SS),
    f("recipf.d",  0x07e1, 0x045e, DD),
    f("recipf.s",  0x07e1, 0x044e, SS),
    f("rsqrtf.d",  0x07e2, 0x045e, DD),
    f("rsqrtf.s",  0x07e2, 0x044e, SS),
    f("sqrtf.d",   0x07e0, 0x045e, DD),
    f("sqrtf.s",   0x07e0, 0x044e, SS),
    f("subf.d",    0x07e0, 0x0472, DDD),
    f("subf.s",    0x07e0, 0x0462, SSS),
    f("trfsr",     0x07e0, 0x0400, &[Fff]),
    f("trfsr",     0x07e0, 0x0400, &[]),
    f("trncf.dl",  0x07e1, 0x0454, DD),
    f("trncf.dul", 0x07f1, 0x0454, DD),
    f("trncf.duw", 0x07f1, 0x0450, DW),
    f("trncf.dw",  0x07e1, 0x0450, DW),
    f("trncf.sl",  0x07e1, 0x0444, WD),
    // GNU as accepts an odd result register here, unlike every other
    // single-to-long conversion. The encoding is the same for the even
    // registers anyone would write, and the reference is followed.
    f("trncf.sul", 0x07f1, 0x0444, SS),
    f("trncf.suw", 0x07f1, 0x0440, SS),
    f("trncf.sw",  0x07e1, 0x0440, SS),
];
