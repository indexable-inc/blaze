//! A64 encodings of the instructions the glue writes, each as Cranelift 0.136.2's
//! `isa/aarch64/inst/emit.rs` encodes it, and the few sequences built of them (a constant into
//! a register, a load or store at any offset, an add of any immediate).

use crate::buf::Buf;

pub(super) const X0: u8 = 0;
pub(super) const X1: u8 = 1;
/// The indirect result register (`StructReturn`).
pub(super) const X8: u8 = 8;
/// `IP0` and `IP1`: the glue's scratch, Cranelift's `spilltmp` and `tmp2`.
pub(super) const X16: u8 = 16;
pub(super) const X17: u8 = 17;
/// The stencils' frame register: the base of the value area.
pub(super) const X20: u8 = 20;
pub(super) const FP: u8 = 29;
/// The link register: free in a body, as the frame record keeps the return address.
pub(super) const LR: u8 = 30;
/// `sp` as a base or an `add`/`sub` operand, `xzr` elsewhere.
pub(super) const SP: u8 = 31;
pub(super) const ZR: u8 = 31;

/// Condition codes.
pub(super) const LO: u32 = 0b0011;
pub(super) const GT: u32 = 0b1100;

pub(super) const RET: u32 = 0xd65f_03c0;
/// `udf #0xc11f`: Cranelift's `TRAP_OPCODE`.
pub(super) const UDF: u32 = 0x0000_c11f;
/// `brk #0xf000`.
pub(super) const BRK: u32 = 0xd43e_0000;
/// `bl 0`, for an `Arm64Call` relocation.
pub(super) const BL: u32 = 0x9400_0000;
/// `mov x29, sp` (`add x29, sp, #0`).
pub(super) const MOV_FP_SP: u32 = 0x9100_03fd;

/// A load or store with an unsigned, scaled 12-bit offset, by its opcode and the log2 of its
/// access size; [`ldst`] turns it into the unscaled or register-offset form as the offset
/// needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Mem {
    pub(super) bits: u32,
    pub(super) scale: u8,
}

pub(super) const LDRB: Mem = Mem { bits: 0x3940_0000, scale: 0 };
pub(super) const LDRH: Mem = Mem { bits: 0x7940_0000, scale: 1 };
pub(super) const LDRW: Mem = Mem { bits: 0xb940_0000, scale: 2 };
pub(super) const LDRX: Mem = Mem { bits: 0xf940_0000, scale: 3 };
pub(super) const STRB: Mem = Mem { bits: 0x3900_0000, scale: 0 };
pub(super) const STRH: Mem = Mem { bits: 0x7900_0000, scale: 1 };
pub(super) const STRW: Mem = Mem { bits: 0xb900_0000, scale: 2 };
pub(super) const STRX: Mem = Mem { bits: 0xf900_0000, scale: 3 };
pub(super) const LDRD: Mem = Mem { bits: 0xfd40_0000, scale: 3 };
pub(super) const LDRQ: Mem = Mem { bits: 0x3dc0_0000, scale: 4 };
pub(super) const STRD: Mem = Mem { bits: 0xfd00_0000, scale: 3 };
pub(super) const STRQ: Mem = Mem { bits: 0x3d80_0000, scale: 4 };
/// `ldrsb`/`ldrsh`/`ldrsw` into an X register.
pub(super) const LDRSB: Mem = Mem { bits: 0x3980_0000, scale: 0 };
pub(super) const LDRSH: Mem = Mem { bits: 0x7980_0000, scale: 1 };
pub(super) const LDRSW: Mem = Mem { bits: 0xb980_0000, scale: 2 };
/// `ldr`/`str` of an H or S register.
pub(super) const LDRH_V: Mem = Mem { bits: 0x7d40_0000, scale: 1 };
pub(super) const LDRS_V: Mem = Mem { bits: 0xbd40_0000, scale: 2 };
pub(super) const STRH_V: Mem = Mem { bits: 0x7d00_0000, scale: 1 };
pub(super) const STRS_V: Mem = Mem { bits: 0xbd00_0000, scale: 2 };

/// The load of `bytes` (1, 2, 4 or 8) into an X register, sign- or zero-extended to 64 bits.
pub(super) const fn ldr_ext(bytes: u32, signed: bool) -> Mem {
    match (bytes, signed) {
        (1, true) => LDRSB,
        (2, true) => LDRSH,
        (4, true) => LDRSW,
        (1, false) => LDRB,
        (2, false) => LDRH,
        (4, false) => LDRW,
        _ => LDRX,
    }
}

/// The load and store of `bytes` (2, 4, 8 or 16) of a V register.
pub(super) const fn ldr_v(bytes: u32) -> Mem {
    match bytes {
        2 => LDRH_V,
        4 => LDRS_V,
        8 => LDRD,
        _ => LDRQ,
    }
}

pub(super) const fn str_v(bytes: u32) -> Mem {
    match bytes {
        2 => STRH_V,
        4 => STRS_V,
        8 => STRD,
        _ => STRQ,
    }
}

/// The integer load and store of `bytes` (1, 2, 4 or 8).
pub(super) const fn ldr_int(bytes: u8) -> Mem {
    match bytes {
        1 => LDRB,
        2 => LDRH,
        4 => LDRW,
        _ => LDRX,
    }
}

pub(super) const fn str_int(bytes: u8) -> Mem {
    match bytes {
        1 => STRB,
        2 => STRH,
        4 => STRW,
        _ => STRX,
    }
}

const fn r(reg: u8) -> u32 {
    (reg & 31) as u32
}

pub(super) const fn movz(rd: u8, imm16: u16, hw: u8) -> u32 {
    0xd280_0000 | (hw as u32) << 21 | (imm16 as u32) << 5 | r(rd)
}

pub(super) const fn movk(rd: u8, imm16: u16, hw: u8) -> u32 {
    0xf280_0000 | (hw as u32) << 21 | (imm16 as u32) << 5 | r(rd)
}

pub(super) const fn movn(rd: u8, imm16: u16, hw: u8) -> u32 {
    0x9280_0000 | (hw as u32) << 21 | (imm16 as u32) << 5 | r(rd)
}

/// `mov xd, sp` (`add xd, sp, #0`).
pub(super) const fn mov_from_sp(rd: u8) -> u32 {
    0x9100_0000 | r(SP) << 5 | r(rd)
}

/// `add`/`sub xd, xn, #imm12{, lsl #12}`; `xd` and `xn` may be `sp`.
pub(super) const fn add_imm(rd: u8, rn: u8, imm12: u16, shift12: bool, sub: bool) -> u32 {
    let op = if sub { 0xd100_0000 } else { 0x9100_0000 };
    op | (shift12 as u32) << 22 | ((imm12 & 0xfff) as u32) << 10 | r(rn) << 5 | r(rd)
}

/// `add`/`sub xd, xn, xm, uxtx`: the form that takes `sp` as `xd` or `xn`.
pub(super) const fn add_ext(rd: u8, rn: u8, rm: u8, sub: bool) -> u32 {
    let op = if sub { 0xcb20_6000 } else { 0x8b20_6000 };
    op | r(rm) << 16 | r(rn) << 5 | r(rd)
}

/// `add xd, xn, xm`.
pub(super) const fn add(rd: u8, rn: u8, rm: u8) -> u32 {
    0x8b00_0000 | r(rm) << 16 | r(rn) << 5 | r(rd)
}

/// `and xd, xn, #-align` (a power of two, at least 2): the bitmask immediate of ones from
/// bit `log2(align)` up. `xd` may be `sp`, `xn` may not.
pub(super) const fn and_align(rd: u8, rn: u8, align: u32) -> u32 {
    let k = align.trailing_zeros();
    let immr = (64 - k) & 63;
    let imms = 63 - k;
    0x9200_0000 | 1 << 22 | immr << 16 | imms << 10 | r(rn) << 5 | r(rd)
}

/// `cmp wn, #imm12{, lsl #12}`.
pub(super) const fn cmp_w_imm(rn: u8, imm12: u16, shift12: bool) -> u32 {
    0x7100_001f | (shift12 as u32) << 22 | ((imm12 & 0xfff) as u32) << 10 | r(rn) << 5
}

/// `cmp wn, wm`.
pub(super) const fn cmp_w(rn: u8, rm: u8) -> u32 {
    0x6b00_001f | r(rm) << 16 | r(rn) << 5
}

/// `cmn xn, xm` (`adds xzr, xn, xm`).
pub(super) const fn cmn(rn: u8, rm: u8) -> u32 {
    0xab00_001f | r(rm) << 16 | r(rn) << 5
}

/// `csel xd, xn, xm, cond`.
pub(super) const fn csel(rd: u8, rn: u8, rm: u8, cond: u32) -> u32 {
    0x9a80_0000 | r(rm) << 16 | cond << 12 | r(rn) << 5 | r(rd)
}

/// `b` with a resolved word offset.
pub(super) const fn b(words: i32) -> u32 {
    0x1400_0000 | (words as u32 & 0x03ff_ffff)
}

/// `b.cond` with a resolved word offset.
pub(super) const fn b_cond_to(cond: u32, words: i32) -> u32 {
    0x5400_0000 | (words as u32 & 0x7_ffff) << 5 | cond
}

pub(super) const fn blr(rn: u8) -> u32 {
    0xd63f_0000 | r(rn) << 5
}

pub(super) const fn br(rn: u8) -> u32 {
    0xd61f_0000 | r(rn) << 5
}

/// `adr xd, #off` (bytes from the instruction, ±1 MB).
pub(super) const fn adr(rd: u8, off: i32) -> u32 {
    let off = off as u32;
    0x1000_0000 | (off & 3) << 29 | ((off >> 2) & 0x7_ffff) << 5 | r(rd)
}

/// `adrp xd, 0`, for a page relocation.
pub(super) const fn adrp(rd: u8) -> u32 {
    0x9000_0000 | r(rd)
}

/// `ldr xt, #off` (literal, bytes from the instruction, a multiple of 4).
pub(super) const fn ldr_lit(rt: u8, off: i32) -> u32 {
    0x5800_0000 | ((off >> 2) as u32 & 0x7_ffff) << 5 | r(rt)
}

/// `op rt, [rn]`.
pub(super) const fn at0(op: Mem, rt: u8, rn: u8) -> u32 {
    op.bits | r(rn) << 5 | r(rt)
}

/// The scaled-offset form of `op rt, [rn, #off]`, when `off` fits it.
pub(super) const fn ldst_scaled(op: Mem, rt: u8, rn: u8, off: u32) -> Option<u32> {
    let mask = (1u32 << op.scale) - 1;
    if off & mask == 0 && off >> op.scale < 4096 {
        Some(op.bits | (off >> op.scale) << 10 | r(rn) << 5 | r(rt))
    } else {
        None
    }
}

/// `ldrsw xt, [xn, wm, uxtw #2]`.
pub(super) const fn ldrsw_uxtw2(rt: u8, rn: u8, rm: u8) -> u32 {
    0xb8a0_5800 | r(rm) << 16 | r(rn) << 5 | r(rt)
}

/// `mrs xt, tpidr_el0`.
pub(super) const fn mrs_tpidr_el0(rt: u8) -> u32 {
    0xd53b_d040 | r(rt)
}

/// `str wzr, [sp, xm]`.
pub(super) const fn str_wzr_sp_reg(rm: u8) -> u32 {
    0xb820_6800 | r(rm) << 16 | r(SP) << 5 | r(ZR)
}

/// `stp`/`ldp` of two X registers, pre-indexed (store) or post-indexed (load) by `imm`
/// bytes, a multiple of 8 in -512..=504.
pub(super) const fn stp_pre(rt: u8, rt2: u8, rn: u8, imm: i32) -> u32 {
    0xa980_0000 | ((imm / 8) as u32 & 0x7f) << 15 | r(rt2) << 10 | r(rn) << 5 | r(rt)
}

pub(super) const fn ldp_post(rt: u8, rt2: u8, rn: u8, imm: i32) -> u32 {
    0xa8c0_0000 | ((imm / 8) as u32 & 0x7f) << 15 | r(rt2) << 10 | r(rn) << 5 | r(rt)
}

/// `stp` of two X registers at `[rn, #imm]`, the signed-offset form.
pub(super) const fn stp_off(rt: u8, rt2: u8, rn: u8, imm: i32) -> u32 {
    0xa900_0000 | ((imm / 8) as u32 & 0x7f) << 15 | r(rt2) << 10 | r(rn) << 5 | r(rt)
}

/// `stp`/`ldp` of two D registers, as [`stp_pre`] and [`ldp_post`].
pub(super) const fn stp_d_pre(rt: u8, rt2: u8, rn: u8, imm: i32) -> u32 {
    0x6d80_0000 | ((imm / 8) as u32 & 0x7f) << 15 | r(rt2) << 10 | r(rn) << 5 | r(rt)
}

pub(super) const fn ldp_d_post(rt: u8, rt2: u8, rn: u8, imm: i32) -> u32 {
    0x6cc0_0000 | ((imm / 8) as u32 & 0x7f) << 15 | r(rt2) << 10 | r(rn) << 5 | r(rt)
}

/// `fmov dd, xn`.
pub(super) const fn fmov_d_x(rd: u8, rn: u8) -> u32 {
    0x9e67_0000 | r(rn) << 5 | r(rd)
}

/// `fmov sd, wn`.
pub(super) const fn fmov_s_w(rd: u8, rn: u8) -> u32 {
    0x1e27_0000 | r(rn) << 5 | r(rd)
}

/// `imm` as an `add`/`sub` immediate: 12 bits, or 12 bits shifted by 12 (Cranelift's
/// `Imm12::maybe_from_u64`).
pub(super) const fn imm12(imm: u64) -> Option<(u16, bool)> {
    if imm < 0x1000 {
        Some((imm as u16, false))
    } else if imm & 0xfff == 0 && imm < 0x100_0000 {
        Some(((imm >> 12) as u16, true))
    } else {
        None
    }
}

/// `value` into `rd`: a `movz` (or a `movn` where more halfwords are all ones) and a `movk`
/// for each other halfword that differs, at most four instructions.
pub(super) fn mov_imm(buf: &mut Buf<'_>, rd: u8, value: u64) {
    let half = |hw: u8| (value >> (u32::from(hw) * 16)) as u16;
    let zeros = (0..4).filter(|hw| half(*hw) == 0).count();
    let ones = (0..4).filter(|hw| half(*hw) == 0xffff).count();
    let inverted = ones > zeros;
    let skip = if inverted { 0xffff } else { 0 };
    let mut first = true;
    for hw in 0..4u8 {
        let it = half(hw);
        if it == skip {
            continue;
        }
        let word = match (first, inverted) {
            (true, false) => movz(rd, it, hw),
            (true, true) => movn(rd, !it, hw),
            (false, _) => movk(rd, it, hw),
        };
        buf.put4(word);
        first = false;
    }
    if first {
        buf.put4(if inverted { movn(rd, 0, 0) } else { movz(rd, 0, 0) });
    }
}

/// The most bytes [`ldst`] writes.
pub(super) const LDST_MAX: u32 = 12;

/// `op rt, [rn, #off]` at any offset below 2^32, in the form Cranelift's `mem_finalize`
/// picks: the unscaled one below 256, the scaled one where the offset is a multiple of the
/// access size below 4096 of them, and otherwise the offset put in `tmp` and the
/// register form (`sxtx`). `tmp` must be neither `rn` nor (for a store) `rt`.
pub(super) fn ldst(buf: &mut Buf<'_>, op: Mem, rt: u8, rn: u8, off: u32, tmp: u8) {
    if off < 256 {
        buf.put4(ldur(op, rt, rn, off as i32));
    } else if let Some(word) = ldst_scaled(op, rt, rn, off) {
        buf.put4(word);
    } else {
        mov_imm(buf, tmp, u64::from(off));
        buf.put4(ldst_reg(op, rt, rn, tmp));
    }
}

/// `op rt, [rn, xm, sxtx]`.
const fn ldst_reg(op: Mem, rt: u8, rn: u8, rm: u8) -> u32 {
    (op.bits & !0x0100_0000) | 0x0020_e800 | r(rm) << 16 | r(rn) << 5 | r(rt)
}

/// The unscaled form of `op rt, [rn, #off]` (`ldur`/`stur`), `off` in -256..256.
pub(super) const fn ldur(op: Mem, rt: u8, rn: u8, off: i32) -> u32 {
    (op.bits & !0x0100_0000) | (off as u32 & 0x1ff) << 12 | r(rn) << 5 | r(rt)
}

/// `stur wzr, [sp]`: a probe's touch of the page `sp` is in.
pub(super) const STUR_WZR_SP: u32 = ldur(STRW, ZR, SP, 0);

/// The most bytes [`add_any`] writes.
pub(super) const ADD_ANY_MAX: u32 = 20;

/// `rd = rn ± imm` for any `imm`, as Cranelift's `gen_add_imm` and `gen_sp_reg_adjust` write
/// it: one immediate `add`/`sub`, or the constant put in `tmp` and an extended-register
/// one. `rd` and `rn` may be `sp`; `tmp` may be neither.
pub(super) fn add_any(buf: &mut Buf<'_>, rd: u8, rn: u8, imm: u64, sub: bool, tmp: u8) {
    match imm12(imm) {
        Some((imm12, shift12)) => buf.put4(add_imm(rd, rn, imm12, shift12, sub)),
        None => {
            mov_imm(buf, tmp, imm);
            buf.put4(add_ext(rd, rn, tmp, sub));
        }
    }
}
