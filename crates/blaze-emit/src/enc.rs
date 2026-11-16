//! AArch64 instruction words, one function per form the baseline tier uses (the Arm ARM's
//! A64 encodings, section C6). Every function is pure: registers are numbers 0..=31 (31 is
//! `xzr` or `sp` as the form says), immediates are checked by the caller to fit, and each
//! form's word is pinned by a test against the assembler's (src/tests.rs, `encodings`).
//!
//! All arithmetic forms here are the 64-bit (`x`) ones: the baseline tier computes every
//! integer in a whole register and narrows after (lib.rs, "Values").

/// A general register, 0..=31.
pub(crate) type Reg = u32;

pub(crate) const X0: Reg = 0;
pub(crate) const X9: Reg = 9;
pub(crate) const X10: Reg = 10;
pub(crate) const X11: Reg = 11;
pub(crate) const X12: Reg = 12;
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
    /// Unsigned `>`.
    Hi = 8,
    /// Unsigned `<=`.
    Ls = 9,
    Ge = 10,
    Lt = 11,
    Gt = 12,
    Le = 13,
}

impl Cond {
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
pub(crate) const fn movz_w(rd: Reg, imm16: u32) -> u32 {
    0x5280_0000 | (imm16 << 5) | rd
}

// Arithmetic and logic, shifted register (C6.2.5, 389, 15, 223, 121, 222).

pub(crate) const fn add(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0x8b00_0000 | (rm << 16) | (rn << 5) | rd
}

pub(crate) const fn sub(rd: Reg, rn: Reg, rm: Reg) -> u32 {
    0xcb00_0000 | (rm << 16) | (rn << 5) | rd
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

pub(crate) const fn sxtb(rd: Reg, rn: Reg) -> u32 {
    sext(rd, rn, 8)
}

pub(crate) const fn sxth(rd: Reg, rn: Reg) -> u32 {
    sext(rd, rn, 16)
}

pub(crate) const fn sxtw(rd: Reg, rn: Reg) -> u32 {
    sext(rd, rn, 32)
}

pub(crate) const fn uxtb(rd: Reg, rn: Reg) -> u32 {
    zext(rd, rn, 8)
}

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

/// `stp xt, xt2, [rn, #off]!`, `off` a multiple of 8 in -512..=504.
pub(crate) const fn stp_pre(rt: Reg, rt2: Reg, rn: Reg, off: i32) -> u32 {
    0xa980_0000 | ((((off / 8) as u32) & 0x7f) << 15) | (rt2 << 10) | (rn << 5) | rt
}

/// `ldp xt, xt2, [rn], #off`, `off` a multiple of 8 in -512..=504.
pub(crate) const fn ldp_post(rt: Reg, rt2: Reg, rn: Reg, off: i32) -> u32 {
    0xa8c0_0000 | ((((off / 8) as u32) & 0x7f) << 15) | (rt2 << 10) | (rn << 5) | rt
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

// Branches (C6.2.26, 25, 66, 65, 37, 35, 290, 49).

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

/// The offset of a branch's or literal's field, for patching once its target is placed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Field {
    /// `b`: a 26-bit word offset in bits 0..26, ±128 MiB.
    Imm26,
    /// `b.cond`, `cbz`, `cbnz`, `ldr` (literal): a 19-bit word offset in bits 5..24, ±1 MiB.
    Imm19,
    /// `adr`: a 21-bit byte offset, its low 2 bits in 29..31 and the rest in 5..24, ±1 MiB.
    Adr,
}

impl Field {
    /// `word` with its field set to reach `bytes` from it, or `None` past the field's reach.
    pub(crate) const fn patch(self, word: u32, bytes: i64) -> Option<u32> {
        match self {
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
        }
    }
}
