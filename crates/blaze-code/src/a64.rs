//! The AArch64 instructions a layout completes at a site, in one home: the segment's
//! [`crate::image::lay_out`] and blaze-export's whole-program layout both patch through these.
//!
//! Each encoder takes the instruction the record holds (its registers stand) and returns it
//! completed, or [`ImageError::OutOfReach`] naming the function and the offset. Encodings: the
//! Arm Architecture Reference Manual (ADRP, LDR (immediate, unsigned offset), ADD (immediate),
//! BL, BLR, NOP).

use crate::image::ImageError;
use crate::record::Symbol;

/// `nop`.
pub const NOP: u32 = 0xD503_201F;
/// `adrp`: bit 31 and bits 28..24 fixed.
pub const ADRP: (u32, u32) = (0x9F00_0000, 0x9000_0000);
/// `ldr Xt, [Xn, #imm12 * 8]` (64-bit, unsigned offset).
pub const LDR64: (u32, u32) = (0xFFC0_0000, 0xF940_0000);
/// `add Xd, Xn, #imm12` (64-bit, no shift).
pub const ADD64: (u32, u32) = (0xFFC0_0000, 0x9100_0000);
/// `blr Xn`.
pub const BLR: (u32, u32) = (0xFFFF_FC1F, 0xD63F_0000);
/// `bl imm26`.
pub const BL: (u32, u32) = (0xFC00_0000, 0x9400_0000);

/// `n` rounded up to a multiple of `to`, a power of two.
pub fn up(n: u64, to: u64) -> u64 {
    (n + to - 1) & !(to - 1)
}

/// The little-endian word at `at`; the caller checked `at + 4 <= code.len()`.
pub fn read32(code: &[u8], at: u64) -> u32 {
    let a = at as usize;
    u32::from_le_bytes([code[a], code[a + 1], code[a + 2], code[a + 3]])
}

/// Writes `value` little-endian at `at`; the caller checked `at + 4 <= code.len()`.
pub fn write32(code: &mut [u8], at: u64, value: u32) {
    let a = at as usize;
    code[a..a + 4].copy_from_slice(&value.to_le_bytes());
}

/// The instruction at `at`, refused unless it is the `want` the site says.
pub fn expect(code: &[u8], at: u64, (mask, value): (u32, u32), symbol: Symbol, want: &'static str) -> Result<u32, ImageError> {
    let inst = read32(code, at);
    match inst & mask == value {
        true => Ok(inst),
        false => Err(ImageError::NotInstruction { symbol, at, want }),
    }
}

/// `adrp` at `pc` made to reach the 4 KiB page of `to`.
pub fn adrp(inst: u32, pc: u64, to: u64, symbol: Symbol) -> Result<u32, ImageError> {
    let pages = (to >> 12) as i64 - (pc >> 12) as i64;
    if !(-(1i64 << 20)..(1i64 << 20)).contains(&pages) {
        return Err(ImageError::OutOfReach { symbol, at: pc, what: "adrp (4 GiB)" });
    }
    let imm = (pages as u32) & 0x1F_FFFF;
    Ok((inst & 0x9F00_001F) | ((imm & 3) << 29) | (((imm >> 2) & 0x7_FFFF) << 5))
}

/// `ldr Xt, [Xn, #lo12(to)]`; `to` is 8-aligned (an import word).
pub fn ldr_lo12(inst: u32, to: u64) -> u32 {
    (inst & !(0xFFF << 10)) | ((((to & 0xFFF) >> 3) as u32) << 10)
}

/// `add Xd, Xn, #lo12(to)`, from an `add` or from the `ldr` it replaces (same registers).
pub fn add_lo12(inst: u32, to: u64) -> u32 {
    let rd = inst & 0x1F;
    let rn = (inst >> 5) & 0x1F;
    0x9100_0000 | (((to & 0xFFF) as u32) << 10) | (rn << 5) | rd
}

/// `bl to` at `pc`.
pub fn bl(pc: u64, to: u64, symbol: Symbol) -> Result<u32, ImageError> {
    let delta = to as i64 - pc as i64;
    if delta % 4 != 0 || !(-(1i64 << 27)..(1i64 << 27)).contains(&delta) {
        return Err(ImageError::OutOfReach { symbol, at: pc, what: "bl (128 MiB)" });
    }
    Ok(0x9400_0000 | (((delta >> 2) as u32) & 0x03FF_FFFF))
}

/// Where the `bl` at `pc` goes, or `None` when `inst` is not a `bl`.
pub fn bl_target(inst: u32, pc: u64) -> Option<u64> {
    if inst & BL.0 != BL.1 {
        return None;
    }
    // Sign-extend the 26-bit word count.
    let words = ((inst << 6) as i32 >> 6) as i64;
    Some((pc as i64 + words * 4) as u64)
}

/// Where the `adrp` at `pc` plus the `add` after it point, or `None` when they are not that pair.
pub fn adrp_add_target(first: u32, second: u32, pc: u64) -> Option<u64> {
    if first & ADRP.0 != ADRP.1 || second & ADD64.0 != ADD64.1 {
        return None;
    }
    let lo = u64::from((first >> 29) & 3);
    let hi = u64::from((first >> 5) & 0x7_FFFF);
    // Sign-extend the 21-bit page count.
    let pages = ((((hi << 2) | lo) << 43) as i64) >> 43;
    let page = ((pc >> 12) as i64 + pages) as u64;
    Some((page << 12) + u64::from((second >> 10) & 0xFFF))
}

/// Where the `adrp` at `pc` plus the `ldr` after it load from, or `None` when they are not that
/// pair.
pub fn adrp_ldr_target(first: u32, second: u32, pc: u64) -> Option<u64> {
    if first & ADRP.0 != ADRP.1 || second & LDR64.0 != LDR64.1 {
        return None;
    }
    let lo = u64::from((first >> 29) & 3);
    let hi = u64::from((first >> 5) & 0x7_FFFF);
    let pages = ((((hi << 2) | lo) << 43) as i64) >> 43;
    let page = ((pc >> 12) as i64 + pages) as u64;
    Some((page << 12) + u64::from((second >> 10) & 0xFFF) * 8)
}
