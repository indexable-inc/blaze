//! The instruction words of the stack-probe tail (pass/probe.rs; THREADS.md D3) that src/enc.rs
//! has no form for. The tail's other words are enc.rs's own forms, not repeated here: `movz`
//! (C6.2.226), `movk` with `hw = 1` (`lsl #16`, C6.2.224), `str xzr, [sp]` as `str(Size::X, ZR,
//! SP, 0)` (C6.2.322, unsigned offset), `sub sp, sp, #1, lsl #12` as `sub_imm` (C6.2.388), `b`
//! and `b.cond` (C6.2.26, 25). Each word below is pinned field by field by
//! src/probe_tests.rs, `the_probe_words`.

use crate::enc::Reg;

/// `x17` (`ip1`): an intra-procedure-call scratch register (AAPCS64 6.1.1), free at the
/// prologue, where the tail counts the bytes still to go down.
pub(crate) const X17: Reg = 17;

/// `nop` (C6.2.238, a hint with `CRm:op2 = 0000:000`): `1101 0101 0000 0011 0010 0000 0001
/// 1111`.
pub(crate) const NOP: u32 = 0xd503_201f;

/// `sub xd|sp, xn|sp, xm, uxtx` (SUB, extended register, C6.2.387): `sf=1 op=1 S=0 01011 opt=00
/// 1 Rm option imm3 Rn Rd`, `option = 011` (UXTX: the whole 64-bit `xm`), `imm3 = 0` (no
/// shift). Register 31 in `Rd` and `Rn` is `sp` in this form (it is `xzr` in the shifted-
/// register `sub`, which cannot write `sp`): the only form that takes a register amount off
/// `sp`. Word: `0xcb20_6000 | Rm << 16 | Rn << 5 | Rd`.
pub(crate) const fn sub_ext_uxtx(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0xcb20_6000 | (rm << 16) | (rn << 5) | rd
}

/// `cmp xn|sp, #imm12, lsl #12` (the alias of SUBS, immediate, C6.2.393, with `Rd = xzr`):
/// `sf=1 op=1 S=1 100010 sh=1 imm12 Rn 11111`. Word: `0xf140_001f | imm12 << 10 | Rn << 5`.
/// src/enc.rs's `cmp_imm` is the `sh = 0` form only.
pub(crate) const fn cmp_imm_lsl12(rn: Reg, imm12: u32) -> u32 {
    0xf140_001f | (imm12 << 10) | (rn << 5)
}
