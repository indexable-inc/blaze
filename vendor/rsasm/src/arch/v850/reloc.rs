//! ELF relocations, and where their values go in an instruction.
//!
//! GNU as writes V850 and RH850 objects for the RH850 ABI by default: machine
//! `EM_V800` rather than `EM_V850`, and the Renesas relocation numbering
//! (`R_V810_*` for the generic ones, `R_V850_*` from 0x3d up) rather than the
//! older GCC-ABI table that starts at 0. Its `-mgcc-abi` still produces the
//! old format, but nothing in the RH850 toolchain world reads it, so rsasm
//! follows the default. Every number below was read from `readelf -r` on an
//! object GNU as 2.47 produced.
//!
//! The RH850 ABI has no equivalent for several of GNU's V850 relocations —
//! `callt` table offsets, tiny-data offsets, 32-bit absolute jump
//! displacements, and the split or even-only 16-bit fields when written
//! without `lo()` — and GNU as fails on those with "reloc not supported". The
//! corresponding fixups here carry no relocation, so they must resolve at
//! assembly time or be reported.
//!
//! Two choices differ from GNU as on purpose, and neither changes a byte of
//! code:
//!
//! - A bare symbol in a 16-bit immediate, `movea sym, r0, r1`, gets
//!   `R_V810_HWORD` at the field. GNU as puts it at the start of the
//!   instruction, where a linker would overwrite the opcode.
//! - `ld.b lo(sym)[r1]` and the other plain 16-bit displacements get
//!   `R_V810_WLO` (or `HWORD` without `lo()`) at the field. GNU as uses
//!   `R_V850_BLO`, which is `ld.bu`'s split field and also writes bit 5 of the
//!   first word — in `ld.b`, part of the opcode — whenever the value is odd.
//!
//! PC-relative relocations are measured from the start of the instruction,
//! and GNU as writes their addends without any bias. Every branch fixup here
//! sits at the instruction's first byte with `adjust = 0`, so the core's
//! addend is already GNU's; the one exception, `loop`, is explained where it
//! is built.

/// `.byte sym`.
pub const BYTE: u32 = 0x31;
/// `.short sym`, and a plain 16-bit immediate such as `movea sym, r0, r1`.
pub const HWORD: u32 = 0x32;
/// `.long sym`, and `hilo(sym)`.
pub const WORD: u32 = 0x33;
/// `lo(sym)` in a 16-bit field.
pub const WLO: u32 = 0x34;
/// `hi0(sym)`: the high half, unadjusted.
pub const WHI: u32 = 0x35;
/// `hi(sym)`: the high half, adjusted for a sign-extended `lo`.
pub const WHI1: u32 = 0x36;
/// `jr` / `jarl`'s 22-bit displacement.
pub const PCR22: u32 = 0x46;
/// `lo(sym)` in `ld.bu`'s split field: bit 0 at bit 5, bits 15..1 in word 1.
pub const BLO: u32 = 0x47;
/// `lo(sym)` in a field whose bit 0 is part of the opcode.
pub const WLO_1: u32 = 0x4c;
/// 32-bit PC-relative displacement.
#[allow(dead_code)]
pub const PC32: u32 = 0x58;
/// `loop`'s 16-bit backward displacement.
pub const PC16U: u32 = 0x5f;
/// The 17-bit conditional branch.
pub const PC17: u32 = 0x60;
/// The 9-bit conditional branch.
pub const PC9: u32 = 0x64;
/// The 23-bit displacement of the 48-bit loads and stores.
pub const WLO23: u32 = 0x71;

/// The relocation for an `n`-byte data reference.
///
/// There is no PC-relative data relocation: GNU as turns `.short sym - .`
/// into a plain `R_V810_HWORD` against `sym`, silently dropping the `- .`,
/// which is wrong. rsasm reports it instead.
pub fn data(n: u8, pcrel: bool) -> Option<u32> {
    if pcrel {
        return None;
    }
    match n {
        1 => Some(BYTE),
        2 => Some(HWORD),
        4 => Some(WORD),
        _ => None,
    }
}

// ---- scatter functions ------------------------------------------------------
//
// Each receives the bytes a fixup covers, read little-endian, and the resolved
// value, and returns the patched bytes. Word 0 of an instruction is the low 16
// bits of a 4-byte read.

/// The 9-bit conditional branch, in a 16-bit read: bits 8..4 of the
/// displacement at 15..11, bits 3..1 at 6..4. Bit 0 is always zero and not
/// stored; bits 3..0 of the word are the condition.
pub fn disp9(word: u64, v: i64) -> u64 {
    let v = v as u64;
    (word & !0xf870) | ((v & 0x1f0) << 7) | ((v & 0x0e) << 3)
}

/// The 17-bit conditional branch, in a 32-bit read: bit 16 of the
/// displacement at bit 4 of word 0, bits 15..1 in word 1. Word 1's bit 0 is
/// part of the opcode.
pub fn disp17(word: u64, v: i64) -> u64 {
    let v = v as u64;
    (word & !0xfffe_0010) | ((v & 0xfffe) << 16) | ((v & 0x1_0000) >> 12)
}

/// `jr`/`jarl`'s 22-bit displacement, in a 32-bit read: bits 21..16 in word 0
/// below the opcode, bits 15..1 in word 1.
pub fn disp22(word: u64, v: i64) -> u64 {
    let v = v as u64;
    (word & !0xfffe_003f) | ((v & 0xfffe) << 16) | ((v >> 16) & 0x3f)
}

/// `loop`'s displacement, in a 16-bit read of word 1. The field holds the
/// *negated* displacement: the loop always branches backwards, so the CPU
/// subtracts it. Bit 0 is the opcode's.
pub fn loop16(word: u64, v: i64) -> u64 {
    (word & 1) | ((v.wrapping_neg() as u64) & 0xfffe)
}

/// A fixup that changes nothing. Used to add a range test to a variant
/// without writing a second copy of a field; see `branch::loop_variants`.
pub fn unchanged(word: u64, _v: i64) -> u64 {
    word
}

/// A plain 16-bit value.
pub fn imm16(word: u64, v: i64) -> u64 {
    (word & !0xffff) | (v as u64 & 0xffff)
}

/// `lo(x)`: the low half of `x`.
pub fn lo16(word: u64, v: i64) -> u64 {
    imm16(word, v)
}

/// `hi(x)`: see [`super::operand::hi_adjusted`].
pub fn hi16(word: u64, v: i64) -> u64 {
    imm16(word, super::operand::hi_adjusted(v))
}

/// `hi0(x)`: the plain high half.
pub fn hi0_16(word: u64, v: i64) -> u64 {
    imm16(word, v >> 16)
}

/// A 16-bit field whose bit 0 belongs to the opcode.
pub fn even16(word: u64, v: i64) -> u64 {
    (word & !0xfffe) | (v as u64 & 0xfffe)
}

/// `ld.bu`'s split displacement in a 32-bit read: bit 0 at bit 5 of word 0,
/// bits 15..1 in word 1.
pub fn split16(word: u64, v: i64) -> u64 {
    let v = v as u64;
    (word & !0xfffe_0020) | ((v & 0xfffe) << 16) | ((v & 1) << 5)
}

/// The 23-bit displacement, in a 32-bit read of words 1 and 2: bits 6..0 at
/// bits 10..4 of word 1, bits 22..7 as word 2.
pub fn disp23(word: u64, v: i64) -> u64 {
    let v = v as u64;
    (word & !0xffff_07f0) | ((v & 0x7f) << 4) | (((v >> 7) & 0xffff) << 16)
}

/// As [`disp23`] for the word-aligned accesses, whose bit 0 is not stored.
pub fn disp23_even(word: u64, v: i64) -> u64 {
    disp23(word, v & !1)
}

/// A 32-bit value.
pub fn imm32(word: u64, v: i64) -> u64 {
    (word & !0xffff_ffff) | (v as u64 & 0xffff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_fields_round_trip_negative_offsets() {
        // `bz -2` is f2 fd, from GNU as.
        assert_eq!(disp9(0x0582, -2), 0xfdf2);
        // `bne +0x21e`, 17-bit: ea 07 1f 02.
        assert_eq!(disp17(0x0001_07ea, 0x21e), 0x021f_07ea);
        // `jr -8`: bf 07 f8 ff.
        assert_eq!(disp22(0x0000_0780, -8), 0xfff8_07bf);
    }

    #[test]
    fn the_loop_field_is_negated() {
        // `loop r1` back 10 bytes: word 1 is 0x000b.
        assert_eq!(loop16(0x0001, -10), 0x000b);
    }

    #[test]
    fn split_and_23_bit_fields() {
        // `ld.bu 5[r1], r2`: a1 17 05 00.
        assert_eq!(split16(0x0001_1781, 5), 0x0005_17a1);
        // `ld.b 0x8000[r1], r2`: word 1 0x1005, word 2 0x0100.
        assert_eq!(disp23(0x1005, 0x8000), 0x0100_1005);
    }
}
