//! AArch64 instruction words, one function per form the baseline tier uses (the Arm ARM's
//! A64 encodings, section C6). Every function is pure: registers are numbers 0..=31 (31 is
//! `xzr` or `sp` as the form says), immediates are checked by the caller to fit, and each
//! form's word is pinned by a test against the assembler's (src/tests.rs, `encodings`).
//!
//! All arithmetic forms here are the 64-bit (`x`) ones: the baseline tier computes every
//! integer in a whole register and narrows after (lib.rs, "Values"). An `asm!` template's
//! 32-bit forms (asm/table.rs) are these words through [`sf`].

/// A general register, 0..=31.
pub(crate) type Reg = u32;

pub(crate) const X0: Reg = 0;
pub(crate) const X1: Reg = 1;
pub(crate) const X2: Reg = 2;
/// The indirect result register: an `sret` pointer (AAPCS64 6.9, B.4).
pub(crate) const X8: Reg = 8;
pub(crate) const X9: Reg = 9;
pub(crate) const X10: Reg = 10;
pub(crate) const X11: Reg = 11;
pub(crate) const X12: Reg = 12;
pub(crate) const X13: Reg = 13;
pub(crate) const X14: Reg = 14;
pub(crate) const X15: Reg = 15;
pub(crate) const X16: Reg = 16;
pub(crate) const FP: Reg = 29;
pub(crate) const LR: Reg = 30;
/// `xzr` in a data-processing form, `sp` in an address or `add`/`sub` immediate form.
pub(crate) const ZR: Reg = 31;
pub(crate) const SP: Reg = 31;

/// A condition code (C1.2.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub(crate) enum Cond {
    Eq = 0,
    Ne = 1,
    /// Unsigned `>=` (`cs`).
    Hs = 2,
    /// Unsigned `<` (`cc`).
    Lo = 3,
    /// Negative (after `fcmp`: ordered less).
    Mi = 4,
    /// Not negative (after `fcmp`: greater, equal or unordered).
    Pl = 5,
    /// Signed overflow (after `fcmp`: unordered).
    Vs = 6,
    /// No overflow (after `fcmp`: ordered).
    Vc = 7,
    /// Unsigned `>`.
    Hi = 8,
    /// Unsigned `<=`.
    Ls = 9,
    Ge = 10,
    Lt = 11,
    Gt = 12,
    Le = 13,
    Al = 14,
    Nv = 15,
}

impl Cond {
    /// The 4-bit condition code encoding.
    const fn bits(self) -> u32 {
        match self {
            Cond::Eq => 0,
            Cond::Ne => 1,
            Cond::Hs => 2,
            Cond::Lo => 3,
            Cond::Mi => 4,
            Cond::Pl => 5,
            Cond::Vs => 6,
            Cond::Vc => 7,
            Cond::Hi => 8,
            Cond::Ls => 9,
            Cond::Ge => 10,
            Cond::Lt => 11,
            Cond::Gt => 12,
            Cond::Le => 13,
            Cond::Al => 14,
            Cond::Nv => 15,
        }
    }

    /// The opposite condition: the low bit flipped.
    pub(crate) const fn invert(self) -> u32 {
        self as u32 ^ 1
    }
}

/// The width of a memory access: log2 of its bytes, the `size` field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub(crate) enum Size {
    B = 0,
    H = 1,
    W = 2,
    X = 3,
}

impl Size {
    const fn bits(self) -> u32 {
        match self {
            Size::B => 0,
            Size::H => 1,
            Size::W => 2,
            Size::X => 3,
        }
    }
}

const fn bit(flag: bool) -> u32 {
    if flag { 1 } else { 0 }
}

// Moves of immediates (C6.2.226, 224, 225).

/// `movz xd, #imm16, lsl #(16 * hw)`.
pub(crate) const fn movz(rd: Reg, imm16: u32, hw: u32) -> u32 {
    0xd280_0000 | (hw << 21) | (imm16 << 5) | rd
}

/// `movk xd, #imm16, lsl #(16 * hw)`.
pub(crate) const fn movk(rd: Reg, imm16: u32, hw: u32) -> u32 {
    0xf280_0000 | (hw << 21) | (imm16 << 5) | rd
}

/// `movn xd, #imm16, lsl #(16 * hw)`: `!(imm16 << 16 * hw)`.
pub(crate) const fn movn(rd: Reg, imm16: u32, hw: u32) -> u32 {
    0x9280_0000 | (hw << 21) | (imm16 << 5) | rd
}

/// `movz wd, #imm16`: the 32-bit form, which tests use as `mov w0, #n`.
#[cfg_attr(not(test), expect(dead_code, reason = "a form pinned by the encoding tests, which the pass does not use yet"))]
pub(crate) const fn movz_w(rd: Reg, imm16: u32) -> u32 {
    0x5280_0000 | (imm16 << 5) | rd
}

// Arithmetic and logic, shifted register (C6.2.5, 389, 15, 223, 121, 222).

pub(crate) const fn add(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x8b00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `add xd, xn, xm, lsl #shift` (`shift` in `0..=63`).
pub(crate) const fn add_lsl(rd: Reg, rn: Reg, rm: Reg, shift: u32) -> u32 {
    0x8b00_0000 | (rm << 16) | ((shift & 63) << 10) | (rn << 5) | rd
}

/// `add xd, xn, xm, lsr #shift` (`shift` in `0..=63`).
pub(crate) const fn add_lsr(rd: Reg, rn: Reg, rm: Reg, shift: u32) -> u32 {
    0x8b40_0000 | (rm << 16) | ((shift & 63) << 10) | (rn << 5) | rd
}

pub(crate) const fn sub(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0xcb00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `sub xd, xn, xm, lsl #shift` (`shift` in `0..=63`).
pub(crate) const fn sub_lsl(rd: Reg, rn: Reg, rm: Reg, shift: u32) -> u32 {
    0xcb00_0000 | (rm << 16) | ((shift & 63) << 10) | (rn << 5) | rd
}

/// `subs xzr, xn, xm`.
pub(crate) const fn cmp(rn: Reg, rm: Reg) -> u32 {
    0xeb00_0000 | (rm << 16) | (rn << 5) | ZR
}

/// `sub xd, xzr, xm`.
pub(crate) const fn neg(rd: Reg, rm: Reg) -> u32 {
    sub(rd, ZR, rm)
}

pub(crate) const fn and(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x8a00_0000 | (rm << 16) | (rn << 5) | rd
}

pub(crate) const fn orr(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0xaa00_0000 | (rm << 16) | (rn << 5) | rd
}

pub(crate) const fn eor(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0xca00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `eor xd, xn, xm, lsl #shift` (`shift` in `0..=63`).
pub(crate) const fn eor_lsl(rd: Reg, rn: Reg, rm: Reg, shift: u32) -> u32 {
    0xca00_0000 | (rm << 16) | ((shift & 63) << 10) | (rn << 5) | rd
}

/// `eor xd, xn, xm, lsr #shift` (`shift` in `0..=63`).
pub(crate) const fn eor_lsr(rd: Reg, rn: Reg, rm: Reg, shift: u32) -> u32 {
    0xca40_0000 | (rm << 16) | ((shift & 63) << 10) | (rn << 5) | rd
}

/// `bic xd, xn, xm`: `xn & !xm` (C6.2.32).
pub(crate) const fn bic(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x8a20_0000 | (rm << 16) | (rn << 5) | rd
}

/// `orn xd, xn, xm`: `xn | !xm` (C6.2.244).
pub(crate) const fn orn(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0xaa20_0000 | (rm << 16) | (rn << 5) | rd
}

/// `eon xd, xn, xm`: `xn ^ !xm` (C6.2.101).
pub(crate) const fn eon(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0xca20_0000 | (rm << 16) | (rn << 5) | rd
}

/// `and xd, xn, #imm` of [`logical_imm`]'s `N:immr:imms` (`nrs`).
pub(crate) const fn and_imm_nrs(rd: Reg, rn: Reg, nrs: u32) -> u32 {
    0x9200_0000 | (nrs << 10) | (rn << 5) | rd
}

/// `orr xd, xn, #imm` of [`logical_imm`]'s `N:immr:imms` (`nrs`).
pub(crate) const fn orr_imm_nrs(rd: Reg, rn: Reg, nrs: u32) -> u32 {
    0xb200_0000 | (nrs << 10) | (rn << 5) | rd
}

/// `eor xd, xn, #imm` of [`logical_imm`]'s `N:immr:imms` (`nrs`).
pub(crate) const fn eor_imm_nrs(rd: Reg, rn: Reg, nrs: u32) -> u32 {
    0xd200_0000 | (nrs << 10) | (rn << 5) | rd
}

/// `extr xd, xn, xm, #lsb` (C6.2.106; `extr xd, xn, xn, #k` is 1-inst `ror #k`).
pub(crate) const fn extr(rd: Reg, rn: Reg, rm: Reg, lsb: u32) -> u32 {
    0x93c0_0000 | (rm << 16) | ((lsb & 63) << 10) | (rn << 5) | rd
}

/// `extr wd, wn, wm, #lsb` (32-bit extract / rotate-right immediate).
pub(crate) const fn extr_w(rd: Reg, rn: Reg, rm: Reg, lsb: u32) -> u32 {
    0x1380_0000 | (rm << 16) | ((lsb & 31) << 10) | (rn << 5) | rd
}

/// `ubfm wd, wn, #immr, #imms` (32-bit bitfield unsigned move; `N=0`).
pub(crate) const fn ubfm_w(rd: Reg, rn: Reg, immr: u32, imms: u32) -> u32 {
    0x5300_0000 | ((immr & 31) << 16) | ((imms & 31) << 10) | (rn << 5) | rd
}

/// `sbfm wd, wn, #immr, #imms` (32-bit bitfield signed move; `N=0`).
pub(crate) const fn sbfm_w(rd: Reg, rn: Reg, immr: u32, imms: u32) -> u32 {
    0x1300_0000 | ((immr & 31) << 16) | ((imms & 31) << 10) | (rn << 5) | rd
}

/// `lsl wd, wn, #shift` (`shift` in 1..=31).
pub(crate) const fn lsl_imm_w(rd: Reg, rn: Reg, shift: u32) -> u32 {
    ubfm_w(rd, rn, 32 - shift, 31 - shift)
}

/// `lsr wd, wn, #shift` (`shift` in 0..=31).
pub(crate) const fn lsr_imm_w(rd: Reg, rn: Reg, shift: u32) -> u32 {
    ubfm_w(rd, rn, shift, 31)
}

/// `asr wd, wn, #shift` (`shift` in 0..=31).
pub(crate) const fn asr_imm_w(rd: Reg, rn: Reg, shift: u32) -> u32 {
    sbfm_w(rd, rn, shift, 31)
}

/// `csinc xd, xn, xm, cond`: `if cond { xn } else { xm + 1 }` (C6.2.72).
pub(crate) const fn csinc(rd: Reg, rn: Reg, rm: Reg, cond: Cond) -> u32 {
    0x9a80_0400 | (rm << 16) | (cond.bits() << 12) | (rn << 5) | rd
}

/// `csinv xd, xn, xm, cond`: `if cond { xn } else { !xm }` (C6.2.73).
pub(crate) const fn csinv(rd: Reg, rn: Reg, rm: Reg, cond: Cond) -> u32 {
    0xda80_0000 | (rm << 16) | (cond.bits() << 12) | (rn << 5) | rd
}

/// `csneg xd, xn, xm, cond`: `if cond { xn } else { -xm }` (C6.2.74).
pub(crate) const fn csneg(rd: Reg, rn: Reg, rm: Reg, cond: Cond) -> u32 {
    0xda80_0400 | (rm << 16) | (cond.bits() << 12) | (rn << 5) | rd
}

/// `stp xt, xt2, [rn, #off]` (signed offset, no writeback), `off` a multiple of 8 in -512..=504.
pub(crate) const fn stp_off(rt: Reg, rt2: Reg, rn: Reg, off: i32) -> u32 {
    0xa900_0000 | (((off / 8).cast_unsigned() & 0x7f) << 15) | (rt2 << 10) | (rn << 5) | rt
}

/// `ldp xt, xt2, [rn, #off]` (signed offset, no writeback), `off` a multiple of 8 in -512..=504.
pub(crate) const fn ldp_off(rt: Reg, rt2: Reg, rn: Reg, off: i32) -> u32 {
    0xa940_0000 | (((off / 8).cast_unsigned() & 0x7f) << 15) | (rt2 << 10) | (rn << 5) | rt
}

/// `ldr{b,h,,} rt, [rn, rm, lsl #size]` (scaled register-index load).
pub(crate) const fn ldr_reg_scaled(size: Size, rt: Reg, rn: Reg, rm: Reg) -> u32 {
    0x3860_7800 | (size.bits() << 30) | (rm << 16) | (rn << 5) | rt
}

/// `str{b,h,,} rt, [rn, rm, lsl #size]` (scaled register-index store).
pub(crate) const fn str_reg_scaled(size: Size, rt: Reg, rn: Reg, rm: Reg) -> u32 {
    0x3820_7800 | (size.bits() << 30) | (rm << 16) | (rn << 5) | rt
}

/// `ldrsw xt, [xn, xm, lsl #2]` (signed 32-bit scaled load for dense PC-relative jump tables).
pub(crate) const fn ldrsw_reg_scaled(rt: Reg, rn: Reg, rm: Reg) -> u32 {
    0xb8a0_7800 | (rm << 16) | (rn << 5) | rt
}

/// `umull xd, wn, wm`: unsigned 32x32->64 widening multiply (C6.2.404).
pub(crate) const fn umull(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x9ba0_7c00 | (rm << 16) | (rn << 5) | rd
}

/// `smull xd, wn, wm`: signed 32x32->64 widening multiply (C6.2.307).
pub(crate) const fn smull(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x9b20_7c00 | (rm << 16) | (rn << 5) | rd
}

/// `orn xd, xzr, xm`.
pub(crate) const fn mvn(rd: Reg, rm: Reg) -> u32 {
    0xaa20_0000 | (rm << 16) | (ZR << 5) | rd
}

/// `orr xd, xzr, xm`.
pub(crate) const fn mov(rd: Reg, rm: Reg) -> u32 {
    orr(rd, ZR, rm)
}

// Arithmetic, immediate (C6.2.4, 388, 386, 51): `rn` and, but for `subs`/`adds`, `rd` may be
// `sp`. `imm12` is shifted left by 12 when `lsl12`.

pub(crate) const fn add_imm(rd: Reg, rn: Reg, imm12: u32, lsl12: bool) -> u32 {
    0x9100_0000 | ((lsl12 as u32) << 22) | (imm12 << 10) | (rn << 5) | rd
}

pub(crate) const fn sub_imm(rd: Reg, rn: Reg, imm12: u32, lsl12: bool) -> u32 {
    0xd100_0000 | ((lsl12 as u32) << 22) | (imm12 << 10) | (rn << 5) | rd
}

/// `subs xzr, xn, #imm12`.
pub(crate) const fn cmp_imm(rn: Reg, imm12: u32) -> u32 {
    0xf100_0000 | (imm12 << 10) | (rn << 5) | ZR
}

/// `adds xzr, xn, #imm12`.
pub(crate) const fn cmn_imm(rn: Reg, imm12: u32) -> u32 {
    0xb100_0000 | (imm12 << 10) | (rn << 5) | ZR
}

// Multiply and divide (C6.2.218, 220, 397, 403).

/// `madd xd, xn, xm, xzr`.
pub(crate) const fn mul(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x9b00_0000 | (rm << 16) | (ZR << 10) | (rn << 5) | rd
}

/// `msub xd, xn, xm, xa`: `xa - xn * xm`.
pub(crate) const fn msub(rd: Reg, rn: Reg, rm: Reg, ra: Reg) -> u32 {
    0x9b00_8000 | (rm << 16) | (ra << 10) | (rn << 5) | rd
}

pub(crate) const fn udiv(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x9ac0_0800 | (rm << 16) | (rn << 5) | rd
}

pub(crate) const fn sdiv(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x9ac0_0c00 | (rm << 16) | (rn << 5) | rd
}

// Shifts by register (C6.2.212, 215, 21, 292): the amount is taken modulo 64.

pub(crate) const fn lslv(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x9ac0_2000 | (rm << 16) | (rn << 5) | rd
}

pub(crate) const fn lsrv(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x9ac0_2400 | (rm << 16) | (rn << 5) | rd
}

pub(crate) const fn asrv(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x9ac0_2800 | (rm << 16) | (rn << 5) | rd
}

pub(crate) const fn rorv(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x9ac0_2c00 | (rm << 16) | (rn << 5) | rd
}

// Bitfield moves (C6.2.416, 340): the extensions and the immediate shifts are their aliases.

pub(crate) const fn sbfm(rd: Reg, rn: Reg, immr: u32, imms: u32) -> u32 {
    0x9340_0000 | (immr << 16) | (imms << 10) | (rn << 5) | rd
}

pub(crate) const fn ubfm(rd: Reg, rn: Reg, immr: u32, imms: u32) -> u32 {
    0xd340_0000 | (immr << 16) | (imms << 10) | (rn << 5) | rd
}

/// `sbfx xd, xn, #0, #bits`: the low `bits` (1..=63) sign-extended; `sxtb`/`sxth`/`sxtw` at
/// 8, 16 and 32.
pub(crate) const fn sext(rd: Reg, rn: Reg, bits: u32) -> u32 {
    sbfm(rd, rn, 0, bits - 1)
}

/// `ubfx xd, xn, #0, #bits`: the low `bits` (1..=63) zero-extended; `uxtb`/`uxth` at 8 and 16.
pub(crate) const fn zext(rd: Reg, rn: Reg, bits: u32) -> u32 {
    ubfm(rd, rn, 0, bits - 1)
}

/// The named extensions: the pass reaches them through [`sext`] and [`zext`] by width.
#[cfg(test)]
pub(crate) const fn sxtb(rd: Reg, rn: Reg) -> u32 {
    sext(rd, rn, 8)
}

#[cfg(test)]
pub(crate) const fn sxth(rd: Reg, rn: Reg) -> u32 {
    sext(rd, rn, 16)
}

#[cfg(test)]
pub(crate) const fn sxtw(rd: Reg, rn: Reg) -> u32 {
    sext(rd, rn, 32)
}

#[cfg(test)]
pub(crate) const fn uxtb(rd: Reg, rn: Reg) -> u32 {
    zext(rd, rn, 8)
}

#[cfg(test)]
pub(crate) const fn uxth(rd: Reg, rn: Reg) -> u32 {
    zext(rd, rn, 16)
}

/// `lsl xd, xn, #shift`, `shift` in 1..=63.
pub(crate) const fn lsl_imm(rd: Reg, rn: Reg, shift: u32) -> u32 {
    ubfm(rd, rn, 64 - shift, 63 - shift)
}

/// `lsr xd, xn, #shift`, `shift` in 0..=63.
pub(crate) const fn lsr_imm(rd: Reg, rn: Reg, shift: u32) -> u32 {
    ubfm(rd, rn, shift, 63)
}

// Conditional select (C6.2.70, 72).

pub(crate) const fn csel(rd: Reg, rn: Reg, rm: Reg, cond: Cond) -> u32 {
    0x9a80_0000 | (rm << 16) | ((cond as u32) << 12) | (rn << 5) | rd
}

/// `cset xd, cond`: `csinc xd, xzr, xzr, !cond`.
pub(crate) const fn cset(rd: Reg, cond: Cond) -> u32 {
    0x9a80_0400 | (ZR << 16) | (cond.invert() << 12) | (ZR << 5) | rd
}

// Loads and stores (C6.2.132, 160, 166, 324, 338): `ldr`/`str` of `size` with the unsigned
// offset scaled by the size, and `ldur`/`stur` with the unscaled signed 9-bit one. A load of
// less than 8 bytes zero-extends.

/// `ldr{b,h,,} rt, [rn, #(imm12 << size)]`.
pub(crate) const fn ldr(size: Size, rt: Reg, rn: Reg, imm12: u32) -> u32 {
    0x3940_0000 | ((size as u32) << 30) | (imm12 << 10) | (rn << 5) | rt
}

/// `str{b,h,,} rt, [rn, #(imm12 << size)]`.
pub(crate) const fn str(size: Size, rt: Reg, rn: Reg, imm12: u32) -> u32 {
    0x3900_0000 | ((size as u32) << 30) | (imm12 << 10) | (rn << 5) | rt
}

/// `ldur{b,h,,} rt, [rn, #imm9]`, `imm9` in -256..=255.
pub(crate) const fn ldur(size: Size, rt: Reg, rn: Reg, imm9: i32) -> u32 {
    0x3840_0000 | ((size as u32) << 30) | (((imm9 as u32) & 0x1ff) << 12) | (rn << 5) | rt
}

/// `stur{b,h,,} rt, [rn, #imm9]`, `imm9` in -256..=255.
pub(crate) const fn stur(size: Size, rt: Reg, rn: Reg, imm9: i32) -> u32 {
    0x3800_0000 | ((size as u32) << 30) | (((imm9 as u32) & 0x1ff) << 12) | (rn << 5) | rt
}

/// `ldr{b,h,,} rt, [rn, rm]` (LDR (register): `option 15:13 = 011` (LSL), `S 12 = 0`):
/// the address `rn + rm`, the offset a whole 64-bit register.
pub(crate) const fn ldr_reg(size: Size, rt: Reg, rn: Reg, rm: Reg) -> u32 {
    0x3860_6800 | ((size as u32) << 30) | (rm << 16) | (rn << 5) | rt
}

/// `str{b,h,,} rt, [rn, rm]` (STR (register)), as [`ldr_reg`] with `opc 23:22 = 00`.
pub(crate) const fn str_reg(size: Size, rt: Reg, rn: Reg, rm: Reg) -> u32 {
    0x3820_6800 | ((size as u32) << 30) | (rm << 16) | (rn << 5) | rt
}

/// The SIMD&FP form of one of the three loads and three stores above (LDR, LDUR, STR, STUR
/// (immediate) and LDR, STR (register), SIMD&FP): the same word with `V 26 = 1`, `size` naming the register (`W`
/// an `s` register, `X` a `d` one). The upper lanes of a loaded register are zeroed.
pub(crate) const fn fp_access(word: u32) -> u32 {
    word | (1 << 26)
}

/// `stp xt, xt2, [rn, #off]!`, `off` a multiple of 8 in -512..=504.
pub(crate) const fn stp_pre(rt: Reg, rt2: Reg, rn: Reg, off: i32) -> u32 {
    0xa980_0000 | ((((off / 8) as u32) & 0x7f) << 15) | (rt2 << 10) | (rn << 5) | rt
}

/// `ldp xt, xt2, [rn], #off`, `off` a multiple of 8 in -512..=504.
pub(crate) const fn ldp_post(rt: Reg, rt2: Reg, rn: Reg, off: i32) -> u32 {
    0xa8c0_0000 | ((((off / 8) as u32) & 0x7f) << 15) | (rt2 << 10) | (rn << 5) | rt
}

/// `stp dt, dt2, [rn, #off]!` (C7.2.309, SIMD&FP 64-bit pre-index), `off` a multiple of 8 in -512..=504.
pub(crate) const fn stp_pre_d(rt: Reg, rt2: Reg, rn: Reg, off: i32) -> u32 {
    0x6d80_0000 | ((((off / 8) as u32) & 0x7f) << 15) | (rt2 << 10) | (rn << 5) | rt
}

/// `ldp dt, dt2, [rn], #off` (C7.2.155, SIMD&FP 64-bit post-index), `off` a multiple of 8 in -512..=504.
pub(crate) const fn ldp_post_d(rt: Reg, rt2: Reg, rn: Reg, off: i32) -> u32 {
    0x6cc0_0000 | ((((off / 8) as u32) & 0x7f) << 15) | (rt2 << 10) | (rn << 5) | rt
}

// PC-relative (C6.2.168, 10): `words` and `bytes` from this instruction.

/// `ldr xt, <pc + 4 * words>`.
pub(crate) const fn ldr_lit(rt: Reg, words: i32) -> u32 {
    0x5800_0000 | (((words as u32) & 0x7_ffff) << 5) | rt
}

/// `adr xd, <pc + bytes>`, `bytes` within ±1 MiB.
pub(crate) const fn adr(rd: Reg, bytes: i32) -> u32 {
    let imm = bytes as u32;
    0x1000_0000 | ((imm & 3) << 29) | (((imm >> 2) & 0x7_ffff) << 5) | rd
}

// PC-relative to a page (C6.2.11): the record form's import and relro sites (pass/sites.rs).

/// `adrp xd, <the 4 KiB page of pc, plus pages>`, `pages` within ±2^20 (±4 GiB): the low 2 bits
/// in 29..31 and the rest in 5..24, as `adr`'s bytes. A site is emitted with 0 pages; the
/// segment's layout sets the field and checks the reach.
pub(crate) const fn adrp(rd: Reg, pages: i32) -> u32 {
    let imm = pages as u32;
    0x9000_0000 | ((imm & 3) << 29) | (((imm >> 2) & 0x7_ffff) << 5) | rd
}

// Branches (C6.2.26, 25, 66, 65, 37, 35, 290, 49).

/// `bl <pc + 4 * words>` (C6.2.34: direct PC-relative call within ±128 MiB).
pub(crate) const fn bl(words: i32) -> u32 {
    0x9400_0000 | (words.cast_unsigned() & 0x3ff_ffff)
}

/// `b <pc + 4 * words>`.
pub(crate) const fn b(words: i32) -> u32 {
    0x1400_0000 | ((words as u32) & 0x3ff_ffff)
}

/// `b.cond <pc + 4 * words>`.
pub(crate) const fn b_cond(cond: Cond, words: i32) -> u32 {
    0x5400_0000 | (((words as u32) & 0x7_ffff) << 5) | cond as u32
}

/// `cbz xt, <pc + 4 * words>`.
pub(crate) const fn cbz(rt: Reg, words: i32) -> u32 {
    0xb400_0000 | (((words as u32) & 0x7_ffff) << 5) | rt
}

/// `cbnz xt, <pc + 4 * words>`.
pub(crate) const fn cbnz(rt: Reg, words: i32) -> u32 {
    0xb500_0000 | (((words as u32) & 0x7_ffff) << 5) | rt
}

/// `tbz xt, #bit, <pc + 4 * words>` (C6.2.398).
pub(crate) const fn tbz(rt: Reg, bit: u32, words: i32) -> u32 {
    test_branch(false, rt, bit) | ((words.cast_unsigned() & 0x3fff) << 5)
}

/// `tbnz xt, #bit, <pc + 4 * words>` (C6.2.397).
pub(crate) const fn tbnz(rt: Reg, bit: u32, words: i32) -> u32 {
    test_branch(true, rt, bit) | ((words.cast_unsigned() & 0x3fff) << 5)
}

/// Test bit and branch: b5 011011 op b40 imm14 Rt. The displacement starts at zero.
pub(crate) const fn test_branch(nonzero: bool, rt: Reg, bit: u32) -> u32 {
    0x3600_0000 | ((bit >> 5) << 31) | ((nonzero as u32) << 24) | ((bit & 31) << 19) | rt
}

pub(crate) const fn br(rn: Reg) -> u32 {
    0xd61f_0000 | (rn << 5)
}

pub(crate) const fn blr(rn: Reg) -> u32 {
    0xd63f_0000 | (rn << 5)
}

/// `ret` (to `x30`).
pub(crate) const fn ret() -> u32 {
    0xd65f_0000 | (LR << 5)
}

pub(crate) const fn brk(imm16: u32) -> u32 {
    0xd420_0000 | (imm16 << 5)
}

// The forms below are pinned by `new_encodings` (src/enc_tests.rs) against words computed by
// hand from the Arm ARM's field layouts (C6.2, each instruction's page named in the test).

/// `adds xd, xn, xm` (shifted register, no shift).
pub(crate) const fn adds(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0xab00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `subs xd, xn, xm` (shifted register, no shift).
pub(crate) const fn subs(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0xeb00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `umulh xd, xn, xm`: the high 64 bits of the unsigned 128-bit product.
pub(crate) const fn umulh(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x9bc0_7c00 | (rm << 16) | (rn << 5) | rd
}

/// `smulh xd, xn, xm`: the high 64 bits of the signed 128-bit product.
pub(crate) const fn smulh(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x9b40_7c00 | (rm << 16) | (rn << 5) | rd
}

/// `asr xd, xn, #shift` (`sbfm xd, xn, #shift, #63`).
pub(crate) const fn asr_imm(rd: Reg, rn: Reg, shift: u32) -> u32 {
    sbfm(rd, rn, shift, 63)
}

/// `rorv wd, wn, wm`: the 32-bit rotation, amount modulo 32, the upper half zeroed.
pub(crate) const fn rorv_w(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x1ac0_2c00 | (rm << 16) | (rn << 5) | rd
}

// Data processing, one source.

pub(crate) const fn clz(rd: Reg, rn: Reg) -> u32 {
    0xdac0_1000 | (rn << 5) | rd
}

pub(crate) const fn rbit(rd: Reg, rn: Reg) -> u32 {
    0xdac0_0000 | (rn << 5) | rd
}

/// `rev16 wd, wn`: the bytes of each halfword swapped, the upper half zeroed.
pub(crate) const fn rev16_w(rd: Reg, rn: Reg) -> u32 {
    0x5ac0_0400 | (rn << 5) | rd
}

/// `rev wd, wn`, the upper half zeroed.
pub(crate) const fn rev_w(rd: Reg, rn: Reg) -> u32 {
    0x5ac0_0800 | (rn << 5) | rd
}

/// `rev xd, xn`.
pub(crate) const fn rev(rd: Reg, rn: Reg) -> u32 {
    0xdac0_0c00 | (rn << 5) | rd
}

// Logical, immediate: the bitmask fields as the encoding has them (`n`, `immr`, `imms`).

pub(crate) const fn and_imm(rd: Reg, rn: Reg, n: u32, immr: u32, imms: u32) -> u32 {
    0x9200_0000 | (n << 22) | (immr << 16) | (imms << 10) | (rn << 5) | rd
}

pub(crate) const fn orr_imm(rd: Reg, rn: Reg, n: u32, immr: u32, imms: u32) -> u32 {
    0xb200_0000 | (n << 22) | (immr << 16) | (imms << 10) | (rn << 5) | rd
}

/// `orr xd, xn, #(1 << bit)`, `bit` in 0..=63: one set bit is the 64-bit element of one 1
/// (`imms` 0) rotated right by `64 - bit`.
pub(crate) const fn orr_bit(rd: Reg, rn: Reg, bit: u32) -> u32 {
    orr_imm(rd, rn, 1, (64 - bit) % 64, 0)
}

// SIMD: a population count through a vector register (no CSSC `cnt` on M1).

/// `fmov dd, xn`.
pub(crate) const fn fmov_to_d(rd: Reg, rn: Reg) -> u32 {
    0x9e67_0000 | (rn << 5) | rd
}

/// `fmov xd, dn`.
pub(crate) const fn fmov_from_d(rd: Reg, rn: Reg) -> u32 {
    0x9e66_0000 | (rn << 5) | rd
}

// Scalar floating point (C7.2; `ftype` 00 single, 01 double at bits 22..24) and the 128-bit
// integer carries (C6.2): pass/float.rs and pass/wide.rs.

const fn ftype(double: bool) -> u32 {
    (double as u32) << 22
}

/// `fmov sd, wn`.
pub(crate) const fn fmov_to_s(rd: Reg, rn: Reg) -> u32 {
    0x1e27_0000 | (rn << 5) | rd
}

/// `fmov wd, sn`.
pub(crate) const fn fmov_from_s(rd: Reg, rn: Reg) -> u32 {
    0x1e26_0000 | (rn << 5) | rd
}

// `f128` and `f16` moves (pass/float_width.rs).

/// `ldr qt, [rn, #imm12 * 16]` (C7.2 LDR (immediate, SIMD&FP), unsigned offset: size=00, opc=11).
pub(crate) const fn ldr_q(rt: Reg, rn: Reg, imm12: u32) -> u32 {
    0x3dc0_0000 | (imm12 << 10) | (rn << 5) | rt
}

/// `str qt, [rn, #imm12 * 16]` (C7.2 STR (immediate, SIMD&FP), unsigned offset: size=00, opc=10).
pub(crate) const fn str_q(rt: Reg, rn: Reg, imm12: u32) -> u32 {
    0x3d80_0000 | (imm12 << 10) | (rn << 5) | rt
}

/// `umov wd, vn.h[0]`: the low 16 bits of `vn`, zero-extended (C7.2 UMOV, imm5=00010).
pub(crate) const fn umov_h0(rd: Reg, rn: Reg) -> u32 {
    0x0e02_3c00 | (rn << 5) | rd
}

/// `fcvt dd, hn` (`to_double`) or `fcvt sd, hn`: exact (C7.2 FCVT, ftype=11, opc 01 or 00).
pub(crate) const fn fcvt_from_h(to_double: bool, rd: Reg, rn: Reg) -> u32 {
    match to_double {
        true => fcvt_between(Ftype::H, Ftype::D, rd, rn),
        false => fcvt_between(Ftype::H, Ftype::S, rd, rn),
    }
}

/// `fcvt hd, dn` (`from_double`) or `fcvt hd, sn`, rounding by FPCR (C7.2 FCVT, opc=11).
pub(crate) const fn fcvt_to_h(from_double: bool, rd: Reg, rn: Reg) -> u32 {
    match from_double {
        true => fcvt_between(Ftype::D, Ftype::H, rd, rn),
        false => fcvt_between(Ftype::S, Ftype::H, rd, rn),
    }
}

/// `fadd`, `fsub`, `fmul`, `fdiv` (`opcode` 0010, 0011, 0000, 0001 at bits 12..16).
pub(crate) const fn fbin(opcode: u32, double: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x1e20_0800 | ftype(double) | (rm << 16) | (opcode << 12) | (rn << 5) | rd
}

/// `fneg`.
pub(crate) const fn fneg(double: bool, rd: Reg, rn: Reg) -> u32 {
    0x1e21_4000 | ftype(double) | (rn << 5) | rd
}

/// `fmov dd, dn` / `fmov sd, sn` (C7.2.119, register).
pub(crate) const fn fmov_reg(double: bool, rd: Reg, rn: Reg) -> u32 {
    0x1e20_4000 | ftype(double) | (rn << 5) | rd
}

/// `fcsel dd, dn, dm, cond` / `fcsel sd, sn, sm, cond` (C7.2.67).
pub(crate) const fn fcsel(double: bool, rd: Reg, rn: Reg, rm: Reg, cond: Cond) -> u32 {
    0x1e20_0c00 | ftype(double) | (rm << 16) | ((cond as u32) << 12) | (rn << 5) | rd
}

/// Scalar FP 1-source by opcode (`000001` fabs, `000011` fsqrt, `001000` frintn, `001001` frintp,
/// `001010` frintm, `001011` frintz, `001100` frinta).
pub(crate) const fn fp1_op(opcode: u32, double: bool, rd: Reg, rn: Reg) -> u32 {
    0x1e20_4000 | ftype(double) | (opcode << 15) | (rn << 5) | rd
}

/// `fcmp n, m`: NZCV is 0110 equal, 1000 less, 0010 greater, 0011 unordered.
pub(crate) const fn fcmp(double: bool, rn: Reg, rm: Reg) -> u32 {
    0x1e20_2000 | ftype(double) | (rm << 16) | (rn << 5)
}

/// `fcvt dd, sn` (`to_double`) or `fcvt sd, dn`.
pub(crate) const fn fcvt(to_double: bool, rd: Reg, rn: Reg) -> u32 {
    match to_double {
        true => fcvt_between(Ftype::S, Ftype::D, rd, rn),
        false => fcvt_between(Ftype::D, Ftype::S, rd, rn),
    }
}

/// `fcvtzs` / `fcvtzu` (`signed`) to `x` (`wide`) or `w`: round toward zero, saturating, NaN
/// to 0 (C7.2, "FCVTZS (scalar, integer)": "out of range ... saturates").
pub(crate) const fn fcvtz(signed: bool, wide: bool, double: bool, rd: Reg, rn: Reg) -> u32 {
    let op = if signed { 0x1e38_0000 } else { 0x1e39_0000 };
    op | ((wide as u32) << 31) | ftype(double) | (rn << 5) | rd
}

/// `scvtf` / `ucvtf` from `x` (`wide`) or `w`, rounding by FPCR (round to nearest, ties to even).
pub(crate) const fn cvtf(signed: bool, wide: bool, double: bool, rd: Reg, rn: Reg) -> u32 {
    let op = if signed { 0x1e22_0000 } else { 0x1e23_0000 };
    op | ((wide as u32) << 31) | ftype(double) | (rn << 5) | rd
}

/// `adc xd, xn, xm`.
pub(crate) const fn adc(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x9a00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `adcs xd, xn, xm`: `adc` setting the flags.
pub(crate) const fn adcs(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0xba00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `sbc xd, xn, xm`.
pub(crate) const fn sbc(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0xda00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `sbcs xd, xn, xm`.
pub(crate) const fn sbcs(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0xfa00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `madd xd, xn, xm, xa`: `xa + xn * xm`.
pub(crate) const fn madd(rd: Reg, rn: Reg, rm: Reg, ra: Reg) -> u32 {
    0x9b00_0000 | (rm << 16) | (ra << 10) | (rn << 5) | rd
}

/// `cnt vd.8b, vn.8b`.
pub(crate) const fn cnt_8b(rd: Reg, rn: Reg) -> u32 {
    0x0e20_5800 | (rn << 5) | rd
}

/// `addv bd, vn.8b`.
pub(crate) const fn addv_8b(rd: Reg, rn: Reg) -> u32 {
    0x0e31_b800 | (rn << 5) | rd
}

// Loads and stores, more forms.

/// `ldr xt, [xn, xm, lsl #3]`.
pub(crate) const fn ldr_lsl3(rt: Reg, rn: Reg, rm: Reg) -> u32 {
    0xf860_7800 | (rm << 16) | (rn << 5) | rt
}

// Exclusive and ordered accesses (Armv8.0): `size` in bits 30..32; the unused register
// fields are all ones.

/// `ldar{b,h,,} rt, [rn]`: a load-acquire.
pub(crate) const fn ldar(size: Size, rt: Reg, rn: Reg) -> u32 {
    0x08df_fc00 | ((size as u32) << 30) | (rn << 5) | rt
}

/// `stlr{b,h,,} rt, [rn]`: a store-release.
pub(crate) const fn stlr(size: Size, rt: Reg, rn: Reg) -> u32 {
    0x089f_fc00 | ((size as u32) << 30) | (rn << 5) | rt
}

/// `ld{a}xr{b,h,,} rt, [rn]`: a load-exclusive, acquiring when `acq`.
pub(crate) const fn ldxr(size: Size, acq: bool, rt: Reg, rn: Reg) -> u32 {
    0x085f_7c00 | ((size as u32) << 30) | ((acq as u32) << 15) | (rn << 5) | rt
}

/// `st{l}xr{b,h,,} ws, rt, [rn]`: a store-exclusive, releasing when `rel`; `ws` is 0 when it
/// stored, 1 when it lost the reservation.
pub(crate) const fn stxr(size: Size, rel: bool, rs: Reg, rt: Reg, rn: Reg) -> u32 {
    0x0800_7c00 | ((size as u32) << 30) | (rs << 16) | ((rel as u32) << 15) | (rn << 5) | rt
}

/// `ld{a}xp xt, xt2, [rn]` (C6.2, LDXP/LDAXP, 64-bit: `1 sz=1 001000 0 L=1 1 Rs=11111 o0 Rt2
/// Rn Rt`): an exclusive load of the pair at `rn` (16 bytes, 16-aligned), acquiring when `acq`
/// (`o0`). Single-copy atomic only once a store-exclusive of the pair succeeds.
pub(crate) const fn ldxp(acq: bool, rt: Reg, rt2: Reg, rn: Reg) -> u32 {
    0xc87f_0000 | ((acq as u32) << 15) | (rt2 << 10) | (rn << 5) | rt
}

/// `st{l}xp ws, xt, xt2, [rn]` (C6.2, STXP/STLXP, 64-bit: `1 sz=1 001000 0 L=0 1 Rs o0 Rt2 Rn
/// Rt`): an exclusive store of the pair, releasing when `rel`; `ws` is 0 when it stored, 1 when
/// it lost the reservation. `ws` differs from `xt`, `xt2` and `rn`.
pub(crate) const fn stxp(rel: bool, rs: Reg, rt: Reg, rt2: Reg, rn: Reg) -> u32 {
    0xc820_0000 | (rs << 16) | ((rel as u32) << 15) | (rt2 << 10) | (rn << 5) | rt
}

/// Select the element width of an LDXP/STXP word: sz (bit 30) is 1 for X, 0 for W.
pub(crate) const fn exclusive_pair_width(wide: bool, word64: u32) -> u32 {
    if wide { word64 } else { word64 & !(1 << 30) }
}

/// `casp{a,l,al} xs, x(s+1), xt, x(t+1), [rn]` (C6.2, CASP, FEAT_LSE, 64-bit: `0 sz=1 001000 0
/// L 1 Rs o0 Rt2=11111 Rn Rt`): if the 16 bytes at `rn` equal `xs:x(s+1)` (low word first),
/// they become `xt:x(t+1)`; `xs:x(s+1)` get the old pair either way. `rs` and `rt` are even.
/// Acquires when `acq` (`L`), releases when `rel` (`o0`).
pub(crate) const fn casp(acq: bool, rel: bool, rs: Reg, rt: Reg, rn: Reg) -> u32 {
    0x4820_7c00 | ((acq as u32) << 22) | (rs << 16) | ((rel as u32) << 15) | (rn << 5) | rt
}

/// `clrex`: drops this core's exclusive reservation.
pub(crate) const fn clrex() -> u32 {
    0xd503_3f5f
}

/// The `dmb` domain and kind (`CRm`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub(crate) enum Barrier {
    /// Inner shareable, loads before loads and stores.
    IshLd = 0b1001,
    /// Inner shareable, all accesses.
    Ish = 0b1011,
}

pub(crate) const fn dmb(barrier: Barrier) -> u32 {
    dmb_crm(barrier as u32)
}

// Barriers and hints (C6.2.81 DMB, 83 DSB, 131 ISB, 238 NOP, 274 SB): `1101 0101 0000 0011
// 0011 CRm op2 11111`, the option in CRm (`asm!`'s table, asm/table.rs).

/// `nop` (the hint space, CRm 0000 op2 000).
pub(crate) const fn nop() -> u32 {
    0xd503_201f
}

/// `dmb <option>`: op2 101.
pub(crate) const fn dmb_crm(crm: u32) -> u32 {
    0xd503_30bf | (crm << 8)
}

/// `dsb <option>`: op2 100.
pub(crate) const fn dsb(crm: u32) -> u32 {
    0xd503_309f | (crm << 8)
}

/// `isb <option>`: op2 110 (`sy` is CRm 1111).
pub(crate) const fn isb(crm: u32) -> u32 {
    0xd503_30df | (crm << 8)
}

/// `sb` (FEAT_SB): CRm 0000 op2 111.
pub(crate) const fn sb() -> u32 {
    0xd503_30ff
}

// Large System Extensions (FEAT_LSE, Armv8.1): atomic memory operations and compare-and-swap.

/// The operation of an LSE atomic (`o3` and `opc`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub(crate) enum Lse {
    Add = 0b0_000,
    Clr = 0b0_001,
    Eor = 0b0_010,
    Set = 0b0_011,
    Smax = 0b0_100,
    Smin = 0b0_101,
    Umax = 0b0_110,
    Umin = 0b0_111,
    Swp = 0b1_000,
}

/// `ld<op>{a,l,al}{b,h,,} rs, rt, [rn]` (`swp` for [`Lse::Swp`]): memory becomes `old op rs`,
/// `rt` the old value, zero-extended from `size`.
pub(crate) const fn lse(op: Lse, size: Size, acq: bool, rel: bool, rs: Reg, rt: Reg, rn: Reg) -> u32 {
    0x3820_0000
        | ((size as u32) << 30)
        | ((acq as u32) << 23)
        | ((rel as u32) << 22)
        | (rs << 16)
        | ((op as u32) << 12)
        | (rn << 5)
        | rt
}

/// `cas{a,l,al}{b,h,,} rs, rt, [rn]`: if memory equals `rs`, it becomes `rt`; `rs` gets the
/// old value either way, zero-extended from `size`.
pub(crate) const fn cas(size: Size, acq: bool, rel: bool, rs: Reg, rt: Reg, rn: Reg) -> u32 {
    0x08a0_7c00 | ((size as u32) << 30) | ((acq as u32) << 22) | (rs << 16) | ((rel as u32) << 15) | (rn << 5) | rt
}

// System registers.

/// `mrs xt, tpidrro_el0` (op0 3, op1 3, CRn 13, CRm 0, op2 3): the read-only thread pointer,
/// which macOS points at the thread's specific-data slots.
pub(crate) const fn mrs_tpidrro_el0(rt: Reg) -> u32 {
    mrs(SysReg::TPIDRRO_EL0, rt)
}

/// A system register by its encoding (C5.3): `op0` 2 or 3, `op1` and `op2` 0..=7, `CRn` and
/// `CRm` 0..=15.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SysReg {
    pub(crate) op0: u32,
    pub(crate) op1: u32,
    pub(crate) crn: u32,
    pub(crate) crm: u32,
    pub(crate) op2: u32,
}

impl SysReg {
    pub(crate) const TPIDRRO_EL0: SysReg = SysReg { op0: 3, op1: 3, crn: 13, crm: 0, op2: 3 };

    const fn fields(self) -> u32 {
        (self.op0 << 19) | (self.op1 << 16) | (self.crn << 12) | (self.crm << 8) | (self.op2 << 5)
    }
}

/// `mrs xt, <sysreg>` (C6.2.229): `1101 0101 0011 o0 op1 CRn CRm op2 Rt`, op0 = `1:o0`.
pub(crate) const fn mrs(reg: SysReg, rt: Reg) -> u32 {
    0xd520_0000 | reg.fields() | rt
}

/// `msr <sysreg>, xt` (C6.2.231, register): as [`mrs`] with L (bit 21) clear.
pub(crate) const fn msr(reg: SysReg, rt: Reg) -> u32 {
    0xd500_0000 | reg.fields() | rt
}

/// A PSTATE field `msr` writes an immediate to (C6.2.230): its `op1` and `op2`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Pstate {
    pub(crate) op1: u32,
    pub(crate) op2: u32,
}

/// `msr <pstatefield>, #imm` (C6.2.230, immediate): `1101 0101 0000 0 op1 0100 CRm op2 11111`,
/// the immediate in CRm.
pub(crate) const fn msr_imm(field: Pstate, imm: u32) -> u32 {
    0xd500_401f | (field.op1 << 16) | (imm << 8) | (field.op2 << 5)
}

/// The offset of a branch's or literal's field, for patching once its target is placed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Field {
    /// `b`: a 26-bit word offset in bits 0..26, ±128 MiB.
    Imm26,
    /// `b.cond`, `cbz`, `cbnz`, `ldr` (literal): a 19-bit word offset in bits 5..24, ±1 MiB.
    Imm19,
    /// `tbz`, `tbnz`: a 14-bit word offset in bits 5..19.
    Imm14,
    /// `adr`: a 21-bit byte offset, its low 2 bits in 29..31 and the rest in 5..24, ±1 MiB.
    Adr,
    /// The far own-data form (pass/reach.rs): `adr xD, #0; movz x10, #lo; movk x10, #hi, lsl
    /// #16; add xD, xD, x10`, the patch at the `adr`; the offset from the `adr` to the target,
    /// at least 0 and below 4 GiB, goes in the `movz` and the `movk` (the pass writes both words:
    /// [`Field::patch`] answers `None` for it).
    Far32,
}

impl Field {
    /// `word` with its field set to reach `bytes` from it, or `None` past the field's reach.
    pub(crate) const fn patch(self, word: u32, bytes: i64) -> Option<u32> {
        match self {
            Field::Imm14 => {
                if bytes % 4 != 0 || bytes < -(1 << 15) || bytes >= 1 << 15 {
                    return None;
                }
                Some((word & !(0x3fff << 5)) | ((((bytes / 4) as u32) & 0x3fff) << 5))
            }
            Field::Imm26 => {
                if bytes % 4 != 0 || bytes < -(1 << 27) || bytes >= 1 << 27 {
                    return None;
                }
                Some((word & !0x3ff_ffff) | (((bytes / 4) as u32) & 0x3ff_ffff))
            }
            Field::Imm19 => {
                if bytes % 4 != 0 || bytes < -(1 << 20) || bytes >= 1 << 20 {
                    return None;
                }
                Some((word & !(0x7_ffff << 5)) | ((((bytes / 4) as u32) & 0x7_ffff) << 5))
            }
            Field::Adr => {
                if bytes < -(1 << 20) || bytes >= 1 << 20 {
                    return None;
                }
                let imm = bytes as u32;
                Some((word & !((3 << 29) | (0x7_ffff << 5))) | ((imm & 3) << 29) | (((imm >> 2) & 0x7_ffff) << 5))
            }
            // Two words, not one: the pass's `settle` writes them (pass/reach.rs `far_words`).
            Field::Far32 => None,
        }
    }
}

/// The two words of the far own-data form's offset ([`Field::Far32`]): `movz x10, #lo` and
/// `movk x10, #hi, lsl #16` for `bytes` from the form's `adr`, or `None` unless `0 <= bytes <
/// 4 GiB`.
pub(crate) const fn far32_words(bytes: i64) -> Option<(u32, u32)> {
    if bytes < 0 || bytes >= 1 << 32 {
        return None;
    }
    let off = bytes as u32;
    Some((movz(X10, off & 0xffff, 0), movk(X10, off >> 16, 1)))
}

// The forms `asm!` templates name (asm/table.rs), beside the tier's own: 32-bit forms by
// [`sf`], the logical immediate, pointer authentication, CRC, floating point by its type, and
// three NEON forms.

/// The 32-bit (`w`) form of a data-processing word given in its 64-bit form: `sf` (bit 31)
/// clear (C4.1: every data-processing form has `sf` there).
pub(crate) const fn sf(wide: bool, word64: u32) -> u32 {
    match wide {
        true => word64,
        false => word64 & !(1 << 31),
    }
}

/// `ands xd, xn, xm` (C6.2.15, shifted register, no shift); `tst xn, xm` is `xd` = `xzr`.
pub(crate) const fn ands(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0xea00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `ands xd, xn, #imm` (C6.2.14) of [`logical_imm`]'s `N:immr:imms` (`nrs`); `tst xn, #imm`
/// is `xd` = `xzr`. The 32-bit form by [`sf`] takes an `N` of 0 only.
pub(crate) const fn ands_imm(rd: Reg, rn: Reg, nrs: u32) -> u32 {
    0xf200_0000 | (nrs << 10) | (rn << 5) | rd
}

/// `pacib xd, xn` (C6.2.208, FEAT_PAuth): sign `xd` with key B and modifier `xn` (or `sp`).
pub(crate) const fn pacib(rd: Reg, rn: Reg) -> u32 {
    0xdac1_0400 | (rn << 5) | rd
}

/// `crc32{b,h,w,x}` (`c` clear) or `crc32c{..}` (C6.2.79, 80): `sf 0 0 11010110 Rm 010 C sz Rn
/// Rd`, `sz` log2 of the bytes `rm` gives (`x` is `sf` 1 and `sz` 11).
pub(crate) const fn crc32(c: bool, sz: u32, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    let sf = (sz == 3) as u32;
    0x1ac0_4000 | (sf << 31) | (rm << 16) | ((c as u32) << 12) | (sz << 10) | (rn << 5) | rd
}

/// A scalar floating-point type, its `ftype` field (C7.2, bits 22..24).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub(crate) enum Ftype {
    S = 0b00,
    D = 0b01,
    H = 0b11,
}

/// `fmadd d, n, m, a` of `ft` (C7.2.105): `0001 1111 ftype 0 Rm 0 Ra Rn Rd`, `d = a + n * m`
/// rounded once.
pub(crate) const fn fmadd(ft: Ftype, rd: Reg, rn: Reg, rm: Reg, ra: Reg) -> u32 {
    0x1f00_0000 | ((ft as u32) << 22) | (rm << 16) | (ra << 10) | (rn << 5) | rd
}

/// `fmsub d, n, m, a` of `ft` (C7.2.126): `0001 1111 ftype 0 Rm 1 Ra Rn Rd`, `d = a - n * m`
/// rounded once.
pub(crate) const fn fmsub(ft: Ftype, rd: Reg, rn: Reg, rm: Reg, ra: Reg) -> u32 {
    0x1f00_8000 | ((ft as u32) << 22) | (rm << 16) | (ra << 10) | (rn << 5) | rd
}

/// The opcode of a floating-point data-processing (1 source) form (C4.1.96): `fsqrt` 000011,
/// `frintn` 001000.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub(crate) enum Fp1 {
    Fsqrt = 0b00_0011,
    Frintn = 0b00_1000,
}

/// A floating-point data-processing (1 source) word: `0001 1110 ftype 1 opcode 10000 Rn Rd`.
pub(crate) const fn fp1(op: Fp1, ft: Ftype, rd: Reg, rn: Reg) -> u32 {
    0x1e20_4000 | ((ft as u32) << 22) | ((op as u32) << 15) | (rn << 5) | rd
}

/// `fcvt <to>d, <from>n` (C7.2.69): the 1-source opcode `0001 opc`, `opc` the destination's
/// type, `ftype` the source's. The three [`fcvt`] forms of the tier are its instances.
pub(crate) const fn fcvt_between(from: Ftype, to: Ftype, rd: Reg, rn: Reg) -> u32 {
    0x1e20_4000 | ((from as u32) << 22) | ((0b00_0100 | to as u32) << 15) | (rn << 5) | rd
}

/// `cmeq vd.T, vn.T, vm.T` (C7.2.27, register): `0 Q 1 01110 size 1 Rm 10001 1 Rn Rd`.
pub(crate) const fn cmeq_v(q: bool, size: u32, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x2e20_8c00 | ((q as u32) << 30) | (size << 22) | (rm << 16) | (rn << 5) | rd
}

/// `and vd.T, vn.T, vm.T` (C7.2.11, `T` 8b or 16b): `0 Q 0 01110 00 1 Rm 00011 1 Rn Rd`.
pub(crate) const fn and_v(q: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x0e20_1c00 | ((q as u32) << 30) | (rm << 16) | (rn << 5) | rd
}

/// `shrn vd.Tb, vn.Ta, #shift` (C7.2.283, Q 0): `0 0 0 011110 immh immb 1000 0 1 Rn Rd`, with
/// `immh:immb` = 2 * `esize` - `shift` for a destination element of `esize` bits (8, 16, 32)
/// and `shift` in 1..=`esize`.
pub(crate) const fn shrn(esize: u32, shift: u32, rd: Reg, rn: Reg) -> u32 {
    0x0f00_8400 | ((2 * esize - shift) << 16) | (rn << 5) | rd
}

/// The low `width` bits set.
const fn low_ones(width: u32) -> u64 {
    match width {
        64.. => u64::MAX,
        w => (1u64 << w) - 1,
    }
}

/// `value` (the low `width` bits) as an arm64 logical immediate of a `size`-bit operation,
/// encoded `N:immr:imms` (Arm ARM C6.2, `DecodeBitMasks`): an element of 2..=64 bits repeated,
/// each a rotated run of ones. `None` for 0, all ones, or any other value.
pub(crate) fn logical_imm(value: u64, size: u32) -> Option<u64> {
    let value = match size {
        32 => (value & 0xffff_ffff) | ((value & 0xffff_ffff) << 32),
        _ => value,
    };
    if value == 0 || value == u64::MAX {
        return None;
    }
    let mut esize: u32 = 64;
    while esize > 2 {
        let half = esize / 2;
        let m = low_ones(half);
        if value & m != (value >> half) & m {
            break;
        }
        esize = half;
    }
    let emask = low_ones(esize);
    let elem = value & emask;
    let ones = elem.count_ones();
    let run = low_ones(ones);
    let mut rotation = None;
    for r in 0..esize {
        let rotated = match r {
            0 => elem,
            r => ((elem >> r) | (elem << (esize - r))) & emask,
        };
        if rotated == run {
            rotation = Some(r);
            break;
        }
    }
    let r = rotation?;
    let immr = u64::from((esize - r) % esize);
    let imms = (u64::from(!(esize * 2 - 1)) & 0x3f) | u64::from(ones - 1);
    let n = u64::from(esize == 64);
    if size == 32 && n == 1 {
        return None;
    }
    Some((n << 12) | (immr << 6) | imms)
}

// -----------------------------------------------------------------------------
// 128-bit NEON SIMD encoders for `TierName::Sota` auto-vectorization (C7.2)
// -----------------------------------------------------------------------------

/// `ldur qt, [rn, #imm9]` (C7.2 LDUR SIMD&FP, 128-bit unscaled signed 9-bit byte offset).
pub(crate) const fn ldur_q(rt: Reg, rn: Reg, imm9: i32) -> u32 {
    0x3cc0_0000 | ((imm9.cast_unsigned() & 0x1ff) << 12) | (rn << 5) | rt
}

/// `stur qt, [rn, #imm9]` (C7.2 STUR SIMD&FP, 128-bit unscaled signed 9-bit byte offset).
pub(crate) const fn stur_q(rt: Reg, rn: Reg, imm9: i32) -> u32 {
    0x3c80_0000 | ((imm9.cast_unsigned() & 0x1ff) << 12) | (rn << 5) | rt
}

/// `ldr qt, [rn, rm, lsl #(4 if scaled else 0)]` (C7.2 LDR register, SIMD&FP 128-bit).
pub(crate) const fn ldr_q_reg(rt: Reg, rn: Reg, rm: Reg, scaled: bool) -> u32 {
    0x3ce0_6800 | (rm << 16) | (bit(scaled) << 12) | (rn << 5) | rt
}

/// `str qt, [rn, rm, lsl #(4 if scaled else 0)]` (C7.2 STR register, SIMD&FP 128-bit).
pub(crate) const fn str_q_reg(rt: Reg, rn: Reg, rm: Reg, scaled: bool) -> u32 {
    0x3ca0_6800 | (rm << 16) | (bit(scaled) << 12) | (rn << 5) | rt
}

/// `ldp qt1, qt2, [rn, #imm7 * 16]` (C7.2 LDP SIMD&FP, 128-bit pair = 32 bytes in one op).
pub(crate) const fn ldp_q_off(rt1: Reg, rt2: Reg, rn: Reg, imm7: i32) -> u32 {
    0xad40_0000 | ((imm7.cast_unsigned() & 0x7f) << 15) | (rt2 << 10) | (rn << 5) | rt1
}

/// `stp qt1, qt2, [rn, #imm7 * 16]` (C7.2 STP SIMD&FP, 128-bit pair = 32 bytes in one op).
pub(crate) const fn stp_q_off(rt1: Reg, rt2: Reg, rn: Reg, imm7: i32) -> u32 {
    0xad00_0000 | ((imm7.cast_unsigned() & 0x7f) << 15) | (rt2 << 10) | (rn << 5) | rt1
}

/// `orr vd.16b, vn.16b, vm.16b` (`mov vd.16b, vn.16b` when `vn == vm`).
pub(crate) const fn orr_v(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x4ea0_1c00 | (rm << 16) | (rn << 5) | rd
}

/// `eor vd.16b, vn.16b, vm.16b`.
pub(crate) const fn eor_v(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x6e20_1c00 | (rm << 16) | (rn << 5) | rd
}

/// `movi vd.2d, #0`: 0-cycle zero idiom on Apple Silicon & Neoverse.
pub(crate) const fn movi_zero_v(rd: Reg) -> u32 {
    0x6f00_e400 | rd
}

/// `dup vd.T, rn` (C7.2 DUP general, 128-bit `Q=1`): `size = 0:16B, 1:8H, 2:4S, 3:2D`.
pub(crate) const fn dup_gen_v(size: u32, rd: Reg, rn: Reg) -> u32 {
    0x4e00_0c00 | ((1u32 << size) << 16) | (rn << 5) | rd
}

/// `dup vd.T, vn.Ts[0]` (C7.2 DUP element lane 0, 128-bit `Q=1`): `size = 2:4S, 3:2D`.
pub(crate) const fn dup_elem0_v(size: u32, rd: Reg, rn: Reg) -> u32 {
    0x4e00_0400 | ((1u32 << size) << 16) | (rn << 5) | rd
}

/// `add vd.T, vn.T, vm.T` (`Q=1`, `size = 0:16B, 1:8H, 2:4S, 3:2D`).
pub(crate) const fn add_v(size: u32, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x4e20_8400 | (size << 22) | (rm << 16) | (rn << 5) | rd
}

/// `sub vd.T, vn.T, vm.T` (`Q=1`, `size = 0:16B, 1:8H, 2:4S, 3:2D`).
pub(crate) const fn sub_v(size: u32, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x6e20_8400 | (size << 22) | (rm << 16) | (rn << 5) | rd
}

/// `mul vd.T, vn.T, vm.T` (`Q=1`, `size = 0:16B, 1:8H, 2:4S`).
pub(crate) const fn mul_v(size: u32, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x4e20_9c00 | (size << 22) | (rm << 16) | (rn << 5) | rd
}

/// `mla vd.T, vn.T, vm.T` (`vd += vn * vm`, `Q=1`, `size = 0:16B, 1:8H, 2:4S`).
pub(crate) const fn mla_v(size: u32, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x4e20_9400 | (size << 22) | (rm << 16) | (rn << 5) | rd
}

/// `smin`/`smax`/`umin`/`umax` (`Q=1`, `size = 0:16B, 1:8H, 2:4S`).
pub(crate) const fn minmax_v(unsigned: bool, is_min: bool, size: u32, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    let base = 0x4e20_6400 | (bit(unsigned) << 29) | (bit(is_min) << 11);
    base | (size << 22) | (rm << 16) | (rn << 5) | rd
}

/// `fadd vd.T, vn.T, vm.T` (`Q=1`, `double ? 2D : 4S`).
pub(crate) const fn fadd_v(double: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x4e20_d400 | (bit(double) << 22) | (rm << 16) | (rn << 5) | rd
}

/// `fsub vd.T, vn.T, vm.T` (`Q=1`, `double ? 2D : 4S`).
pub(crate) const fn fsub_v(double: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x4ea0_d400 | (bit(double) << 22) | (rm << 16) | (rn << 5) | rd
}

/// `fmul vd.T, vn.T, vm.T` (`Q=1`, `double ? 2D : 4S`).
pub(crate) const fn fmul_v(double: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x6e20_dc00 | (bit(double) << 22) | (rm << 16) | (rn << 5) | rd
}

/// `fmla vd.T, vn.T, vm.T` (`vd += vn * vm`, `Q=1`, `double ? 2D : 4S`).
pub(crate) const fn fmla_v(double: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x4e20_cc00 | (bit(double) << 22) | (rm << 16) | (rn << 5) | rd
}

/// `addv d/s/h/b, vn.T` (`Q=1`, `size = 0:16B, 1:8H, 2:4S`).
pub(crate) const fn addv_v(size: u32, rd: Reg, rn: Reg) -> u32 {
    0x4e31_b800 | (size << 22) | (rn << 5) | rd
}

/// `addp dd, vn.2d` (C7.2 ADDP scalar: sums both 64-bit lanes of `vn` into `dd`).
pub(crate) const fn addp_d(rd: Reg, rn: Reg) -> u32 {
    0x5ef1_b800 | (rn << 5) | rd
}

/// `faddp sd/dd, vn.2s/2d` (C7.2 FADDP scalar pairwise FP sum).
pub(crate) const fn faddp_scalar(double: bool, rd: Reg, rn: Reg) -> u32 {
    0x7e30_d800 | (bit(double) << 22) | (rn << 5) | rd
}

/// `faddp vd.4s, vn.4s, vm.4s` (C7.2 FADDP vector pairwise FP sum).
pub(crate) const fn faddp_v(double: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x6e20_d400 | (bit(double) << 22) | (rm << 16) | (rn << 5) | rd
}

/// `umov wd/xd, vn.Ts[idx]` (C7.2 UMOV): `size = 0:B, 1:H, 2:S, 3:D`.
pub(crate) const fn umov_lane(size: u32, idx: u32, rd: Reg, rn: Reg) -> u32 {
    let q = bit(size == 3);
    let imm5 = (idx << (size + 1)) | (1u32 << size);
    0x0e00_3c00 | (q << 30) | (imm5 << 16) | (rn << 5) | rd
}

/// `ext vd.16b, vn.16b, vm.16b, #imm4` (C7.2 EXT byte extract/shift).
pub(crate) const fn ext_v(rd: Reg, rn: Reg, rm: Reg, imm4: u32) -> u32 {
    0x6e00_0000 | (rm << 16) | ((imm4 & 0xf) << 11) | (rn << 5) | rd
}

// -----------------------------------------------------------------------------
// ARMv8.9/v9.4 CSSC (Common Short Sequence Compression) hardware intrinsics
// -----------------------------------------------------------------------------

/// `smax`/`umax`/`smin`/`umin xd, xn, xm` (CSSC 1-instruction scalar integer min/max).
pub(crate) const fn cssc_minmax(unsigned: bool, is_min: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    let opc = (bit(is_min) << 1) | bit(unsigned);
    0x9ac0_6000 | (rm << 16) | (opc << 10) | (rn << 5) | rd
}

/// `cnt xd, xn` (CSSC 1-instruction scalar integer popcount without NEON domain crossing).
pub(crate) const fn cssc_cnt(rd: Reg, rn: Reg) -> u32 {
    0xdac0_1c00 | (rn << 5) | rd
}

/// `abs xd, xn` (CSSC 1-instruction scalar integer absolute value).
pub(crate) const fn cssc_abs(rd: Reg, rn: Reg) -> u32 {
    0xdac0_2000 | (rn << 5) | rd
}

// -----------------------------------------------------------------------------
// AArch64 NEON 128-bit Vector Comparisons & Bitwise Select (Masked SIMD)
// -----------------------------------------------------------------------------

/// `cmgt`/`cmhi vd.T, vn.T, vm.T` (`Q=1`, `unsigned ? cmhi : cmgt`, `size = 0..=3`).
pub(crate) const fn cmgt_v(unsigned: bool, size: u32, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x4e20_3400 | (bit(unsigned) << 29) | (size << 22) | (rm << 16) | (rn << 5) | rd
}

/// `cmge`/`cmhs vd.T, vn.T, vm.T` (`Q=1`, `unsigned ? cmhs : cmge`, `size = 0..=3`).
pub(crate) const fn cmge_v(unsigned: bool, size: u32, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x4e20_3c00 | (bit(unsigned) << 29) | (size << 22) | (rm << 16) | (rn << 5) | rd
}

/// `mvn vd.16b, vn.16b` (`Q=1`, bitwise NOT of 128-bit mask).
pub(crate) const fn not_v(rd: Reg, rn: Reg) -> u32 {
    0x6e20_5800 | (rn << 5) | rd
}

/// `bsl vd.16b, vn.16b, vm.16b` (`Q=1`: `vd = (vd & vn) | (~vd & vm)`).
pub(crate) const fn bsl_v(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x6e60_1c00 | (rm << 16) | (rn << 5) | rd
}

/// `bit vd.16b, vn.16b, vm.16b` (`Q=1`: `vd = (vm & vn) | (~vm & vd)`).
pub(crate) const fn bit_v(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x6ea0_1c00 | (rm << 16) | (rn << 5) | rd
}

/// `bif vd.16b, vn.16b, vm.16b` (`Q=1`: `vd = (~vm & vn) | (vm & vd)`).
pub(crate) const fn bif_v(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x6ee0_1c00 | (rm << 16) | (rn << 5) | rd
}

// -----------------------------------------------------------------------------
// ARMv8.2-SHA3 (`eor3`, `xar`) & ARMv8.0-CRC32 hardware intrinsics
// -----------------------------------------------------------------------------

/// `eor3 vd.16b, vn.16b, vm.16b, va.16b` (3-way 128-bit XOR in 1 instruction — Keccak/SHA3!).
pub(crate) const fn eor3_v(rd: Reg, rn: Reg, rm: Reg, ra: Reg) -> u32 {
    0xce00_0000 | (rm << 16) | (ra << 10) | (rn << 5) | rd
}

/// `xar vd.2d, vn.2d, vm.2d, #imm6` (128-bit XOR + 64-bit lane rotate-right in 1 instruction!).
#[allow(dead_code)]
pub(crate) const fn xar_v(rd: Reg, rn: Reg, rm: Reg, imm6: u32) -> u32 {
    0xce80_0000 | (rm << 16) | ((imm6 & 0x3f) << 10) | (rn << 5) | rd
}

/// `bcax vd.16b, vn.16b, vm.16b, va.16b` (ARMv8.2-SHA3: `vd = vn ^ (vm & ~va)` — Keccak Chi step in 1 op!).
pub(crate) const fn bcax_v(rd: Reg, rn: Reg, rm: Reg, ra: Reg) -> u32 {
    0xce20_0000 | (rm << 16) | (ra << 10) | (rn << 5) | rd
}

/// `rax1 vd.2d, vn.2d, vm.2d` (ARMv8.2-SHA3: `vd = vn ^ ROL64(vm, 1)` — Keccak Theta step in 1 op!).
pub(crate) const fn rax1_v(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0xce60_8c00 | (rm << 16) | (rn << 5) | rd
}

/// `tbl vd.16b, {vn.16b}, vm.16b` (NEON 16-byte single-table vectorized LUT lookup in 1 op!).
pub(crate) const fn tbl1_v(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x4e00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `ins vd.d[lane], xn` (insert 64-bit GPR into 64-bit lane 0 or 1 of a 128-bit NEON vector).
pub(crate) const fn ins_d_gpr(rd: Reg, lane: u32, rn: Reg) -> u32 {
    0x4e08_1c00 | ((lane & 1) << 20) | (rn << 5) | rd
}

// -----------------------------------------------------------------------------
// ARMv8.2-DotProd (`sdot`/`udot`), ARMv8.6-I8MM (`smmla`/`ummla`), AES & SHA2
// -----------------------------------------------------------------------------

/// `sdot`/`udot vd.4s, vn.16b, vm.16b` (4x 4-lane int8 dot product accumulate into 4x i32!).
#[allow(dead_code)]
pub(crate) const fn dotprod_v(unsigned: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x4e80_9400 | (bit(unsigned) << 29) | (rm << 16) | (rn << 5) | rd
}

/// `smmla`/`ummla vd.4s, vn.16b, vm.16b` (ARMv8.6-I8MM 2x8 * 8x2 int8 matrix multiply-accumulate!).
#[allow(dead_code)]
pub(crate) const fn mmla_v(unsigned: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x4e80_a400 | (bit(unsigned) << 29) | (rm << 16) | (rn << 5) | rd
}

/// `aese vd.16b, vn.16b` (ARMv8-AES single-round encryption + AddRoundKey/SubBytes/ShiftRows).
#[allow(dead_code)]
pub(crate) const fn aese_v(rd: Reg, rn: Reg) -> u32 {
    0x4e28_4800 | (rn << 5) | rd
}

/// `aesmc vd.16b, vn.16b` (ARMv8-AES MixColumns).
#[allow(dead_code)]
pub(crate) const fn aesmc_v(rd: Reg, rn: Reg) -> u32 {
    0x4e28_6800 | (rn << 5) | rd
}

/// `sha256h qd, qn, vm.4s` (ARMv8-SHA2 hash update part 1).
#[allow(dead_code)]
pub(crate) const fn sha256h_v(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x5e00_4000 | (rm << 16) | (rn << 5) | rd
}

/// `sha256su0 vd.4s, vn.4s` (ARMv8-SHA2 schedule update 0).
#[allow(dead_code)]
pub(crate) const fn sha256su0_v(rd: Reg, rn: Reg) -> u32 {
    0x5e28_2800 | (rn << 5) | rd
}

/// `prfm pldl1keep, [rn, #(imm12 * 8)]` (C6.2.253: hardware L1 read prefetch hint, non-faulting).
pub(crate) const fn prfm_pldl1keep(rn: Reg, imm12: u32) -> u32 {
    0xf980_0000 | ((imm12 & 0xfff) << 10) | (rn << 5)
}

/// `uminv bd, vn.16b` (C7.2.369: unsigned horizontal minimum across 16 bytes -> `0` iff any lane is `0`).
pub(crate) const fn uminv_b(rd: Reg, rn: Reg) -> u32 {
    0x6e31_a800 | (rn << 5) | rd
}

/// `smaxv`/`umaxv`/`sminv`/`uminv b/h/s, vn.16b/8h/4s` (C7.2 AdvSIMD across-lanes min/max; `size = 0..=2`).
pub(crate) const fn minmaxv_v(unsigned: bool, is_min: bool, size: u32, rd: Reg, rn: Reg) -> u32 {
    0x4e30_a800 | (bit(unsigned) << 29) | (size << 22) | (bit(is_min) << 16) | (rn << 5) | rd
}

/// `sqadd`/`uqadd`/`sqsub`/`uqsub vd.T, vn.T, vm.T` (C7.2 AdvSIMD saturating add/sub; `Q=1`, `size = 0..=3`).
pub(crate) const fn sat_addsub_v(unsigned: bool, is_sub: bool, size: u32, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    let base = if is_sub { 0x4e20_2c00 } else { 0x4e20_0c00 };
    base | (bit(unsigned) << 29) | (size << 22) | (rm << 16) | (rn << 5) | rd
}

/// `ldp dt1, dt2, [rn, #off]` (`off` in `-512..=504`, 8-byte aligned).
pub(crate) const fn ldp_d_off(rt1: Reg, rt2: Reg, rn: Reg, off: i32) -> u32 {
    let imm7 = (off >> 3).cast_unsigned() & 0x7f;
    0x6d40_0000 | (imm7 << 15) | (rt2 << 10) | (rn << 5) | rt1
}

/// `stp dt1, dt2, [rn, #off]` (`off` in `-512..=504`, 8-byte aligned).
pub(crate) const fn stp_d_off(rt1: Reg, rt2: Reg, rn: Reg, off: i32) -> u32 {
    let imm7 = (off >> 3).cast_unsigned() & 0x7f;
    0x6d00_0000 | (imm7 << 15) | (rt2 << 10) | (rn << 5) | rt1
}

/// `xtn vd.2s, vn.2d` (C7.2.383: extract low 32 bits of each 64-bit lane into `vd.2s`).
pub(crate) const fn xtn_2s(rd: Reg, rn: Reg) -> u32 {
    0x0ea1_2800 | (rn << 5) | rd
}

/// `umull vd.2d, vn.2s, vm.2s` (C7.2.374: unsigned 32x32->64 widening vector multiply, low 2 lanes).
pub(crate) const fn umull_2d(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x2ea0_c000 | (rm << 16) | (rn << 5) | rd
}

/// `umull2 vd.2d, vn.4s, vm.4s` (C7.2.374: unsigned 32x32->64 widening vector multiply, high 2 lanes).
pub(crate) const fn umull2_2d(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x6ea0_c000 | (rm << 16) | (rn << 5) | rd
}

/// `uzp2 vd.4s, vn.4s, vm.4s` (C7.2.380: unzip odd 32-bit lanes = upper 32 bits of 4 64-bit products).
pub(crate) const fn uzp2_4s(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x4e80_5800 | (rm << 16) | (rn << 5) | rd
}

/// `uadalp vd.2d, vn.4s` (C7.2.357: unsigned zero-extending pairwise add-accumulate `<4 x u32>` into `<2 x u64>`).
pub(crate) const fn uadalp_2d(rd: Reg, rn: Reg) -> u32 {
    0x6ea0_6800 | (rn << 5) | rd
}

/// `ushr vd.4s, vn.4s, #shift` (`shift` in `1..=32`, C7.2.376).
pub(crate) const fn ushr_4s_imm(rd: Reg, rn: Reg, shift: u32) -> u32 {
    let imm = (64u32.saturating_sub(shift)) & 0x3f;
    0x6f00_0400 | (imm << 16) | (rn << 5) | rd
}

/// `shl vd.4s, vn.4s, #shift` (`shift` in `0..=31`, C7.2.266).
pub(crate) const fn shl_4s_imm(rd: Reg, rn: Reg, shift: u32) -> u32 {
    0x4f20_5400 | ((shift & 31) << 16) | (rn << 5) | rd
}

/// `ins vd.s[lane], wn` (`lane` in `0..=3`, C7.2.172).
pub(crate) const fn ins_s_gpr(rd: Reg, lane: u32, rn: Reg) -> u32 {
    0x4e04_1c00 | ((lane & 3) << 19) | (rn << 5) | rd
}

/// `ushr vd.2d, vn.2d, #shift` (`shift` in `1..=64`, C7.2.376).
pub(crate) const fn ushr_2d_imm(rd: Reg, rn: Reg, shift: u32) -> u32 {
    let imm = (128u32.saturating_sub(shift)) & 0x7f;
    0x6f00_0400 | (imm << 16) | (rn << 5) | rd
}

/// `shl vd.2d, vn.2d, #shift` (`shift` in `0..=63`, C7.2.266).
pub(crate) const fn shl_2d_imm(rd: Reg, rn: Reg, shift: u32) -> u32 {
    0x4f40_5400 | ((shift & 63) << 16) | (rn << 5) | rd
}
