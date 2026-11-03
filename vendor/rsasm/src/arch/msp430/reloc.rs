//! `R_MSP430_*` and `R_MSP430X_*` relocation numbers, and the fixup kinds
//! that carry them.
//!
//! MSP430 objects have *two* relocation tables, and which one a number is
//! read against is decided by the object's `e_flags`: an object for the
//! original 430 uses `elf_msp430_reloc_type` and one for the 430X uses
//! `elf_msp430x_reloc_type` (`include/elf/msp430.h`), so `3` is
//! `R_MSP430_16` in one and `R_MSP430_ABS8` in the other. Every number below
//! was read back with `msp430-elf-readelf -r` from an object `msp430-elf-as`
//! (binutils 2.47) produced for the operand in question, and which
//! relocation each operand gets is that assembler's choice, quirks included;
//! see [`Bfd`].
//!
//! # The 20-bit fields
//!
//! An MSP430X instruction carries the top four bits of a 20-bit address in
//! its extension word and the bottom sixteen in an operand word, which may
//! be two or three words further on. The relocation names the extension
//! word and the linker writes both halves, so these fixups are wide — six or
//! eight bytes — and scatter their value; see [`ext_src`] and its
//! neighbours.
//!
//! # Where the PC is
//!
//! Every PC-relative MSP430 relocation is computed by the GNU linker as
//! `S + A - P` with `P` the address the relocation names
//! (`bfd/elf32-msp430.c`), and GNU as writes the addend the source asked for
//! with no bias, so every kind here is unbiased and measured from the start
//! of its own field. For a plain symbolic operand that field is the operand
//! word, which is also where the hardware takes its PC from. For the
//! extension-word forms it is four bytes earlier than the hardware's PC,
//! which is a linker quirk rather than a choice; rsasm reproduces it so that
//! a flat image matches a linked one.

use super::Isa;
use crate::section::FixupKind;

/// A relocation as GNU as names it, before the object's ISA decides its
/// number. These are the `BFD_RELOC_*` codes `gas/config/tc-msp430.c` asks
/// for, and [`number`] is `msp430_reloc_map` and `msp430x_reloc_map` from
/// `bfd/elf32-msp430.c`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Bfd {
    /// `BFD_RELOC_8`: `.byte sym`.
    Data8,
    /// `BFD_RELOC_16`: `.word sym`, and an instruction word GNU as does not
    /// want the linker to range-check.
    Data16,
    /// `BFD_RELOC_32`: `.long sym`.
    Data32,
    /// `BFD_RELOC_MSP430_16`: a range-checked absolute instruction word.
    /// The 430X has no equivalent; there `Abs16` is used instead.
    Insn16,
    /// `BFD_RELOC_MSP430X_ABS16`: the 430X's range-checked absolute word.
    Abs16,
    /// `BFD_RELOC_MSP430_ABS_HI16`: the second word of a value, `#hi(sym)`.
    Hi16,
    /// `BFD_RELOC_16_PCREL`: an unchecked PC-relative word.
    Pcrel16,
    /// `BFD_RELOC_MSP430_16_PCREL`: a range-checked PC-relative word.
    Insn16Pcrel,
    /// `BFD_RELOC_MSP430X_PCR16`: the 430X's PC-relative word.
    Pcr16,
    /// `BFD_RELOC_MSP430_10_PCREL`: a conditional jump's displacement.
    Jump10,
    /// `BFD_RELOC_MSP430_RL_PCREL`: the branch a polymorphic jump grows to.
    RlPcrel,
    /// `BFD_RELOC_MSP430_SYM_DIFF`: the subtrahend of a difference the
    /// linker has to work out for itself.
    SymDiff,
    /// `BFD_RELOC_MSP430X_ABS20_EXT_SRC`: 20 bits in the extension word and
    /// the first operand word.
    Abs20ExtSrc,
    /// `BFD_RELOC_MSP430X_ABS20_EXT_DST`: the same, for a destination.
    Abs20ExtDst,
    /// `BFD_RELOC_MSP430X_ABS20_EXT_ODST`: a destination whose word comes
    /// after a source word.
    Abs20ExtOdst,
    /// The PC-relative counterparts of the three above.
    Pcr20ExtSrc,
    Pcr20ExtDst,
    Pcr20ExtOdst,
    /// `BFD_RELOC_MSP430X_ABS20_ADR_SRC`: 20 bits in an address instruction
    /// (`mova`, `calla`, `adda`), high nibble in bits 8 to 11.
    Abs20AdrSrc,
    /// `BFD_RELOC_MSP430X_ABS20_ADR_DST`: the same, high nibble in bits 0 to 3.
    Abs20AdrDst,
    /// `BFD_RELOC_MSP430X_PCR20_CALL`: `calla sym`.
    Pcr20Call,
}

/// The ELF relocation number `bfd` has in an object for `isa`.
///
/// `None` where the ISA's table has no entry, which only happens for
/// relocations the other ISA's instructions produce.
pub fn number(isa: Isa, bfd: Bfd) -> Option<u32> {
    use Bfd::*;
    if isa.is_430x() {
        // `msp430x_reloc_map`, into `elf_msp430x_reloc_type`.
        return Some(match bfd {
            Data32 => 1, // R_MSP430_ABS32
            Data16 => 2, // R_MSP430_ABS16
            Data8 => 3,  // R_MSP430_ABS8
            Pcr20ExtSrc => 5,
            Pcr20ExtDst => 6,
            Pcr20ExtOdst => 7,
            Abs20ExtSrc => 8,
            Abs20ExtDst => 9,
            Abs20ExtOdst => 10,
            Abs20AdrSrc => 11,
            Abs20AdrDst => 12,
            Pcr16 => 13,
            Pcr20Call => 14,
            Abs16 => 15,
            Hi16 => 16,
            Jump10 => 19,
            RlPcrel => 13, // R_MSP430X_PCR16
            SymDiff => 21,
            Insn16 | Pcrel16 | Insn16Pcrel => return None,
        });
    }
    // `msp430_reloc_map`, into `elf_msp430_reloc_type`.
    Some(match bfd {
        Data32 => 1,      // R_MSP430_32
        Jump10 => 2,      // R_MSP430_10_PCREL
        Insn16 => 3,      // R_MSP430_16
        Insn16Pcrel => 4, // R_MSP430_16_PCREL
        Data16 => 5,      // R_MSP430_16_BYTE
        Pcrel16 => 6,     // R_MSP430_16_PCREL_BYTE
        RlPcrel => 8,     // R_MSP430_RL_PCREL
        Data8 => 9,       // R_MSP430_8
        SymDiff => 10,
        Abs16 | Hi16 | Pcr16 | Pcr20Call => return None,
        Abs20ExtSrc | Abs20ExtDst | Abs20ExtOdst => return None,
        Pcr20ExtSrc | Pcr20ExtDst | Pcr20ExtOdst => return None,
        Abs20AdrSrc | Abs20AdrDst => return None,
    })
}

/// The relocation for a `size`-byte data directive.
///
/// PC-relative data has none: GNU as writes `.word sym - .` in a code
/// section as a `SYM_DIFF` pair instead, which is what
/// [`difference`] describes.
pub fn data(isa: Isa, size: u8, pcrel: bool) -> Option<u32> {
    if pcrel {
        return None;
    }
    number(
        isa,
        match size {
            1 => Bfd::Data8,
            2 => Bfd::Data16,
            4 => Bfd::Data32,
            _ => return None,
        },
    )
}

/// The pair a difference the file cannot fold is written as: the value
/// itself, and `SYM_DIFF` naming what is subtracted.
pub fn difference(isa: Isa, size: u8) -> Option<(u32, u32)> {
    Some((data(isa, size, false)?, number(isa, Bfd::SymDiff)?))
}

/// `R_MSP430_GNU_SUB_ULEB128` and `R_MSP430_GNU_SET_ULEB128`, in that order,
/// for a `.uleb128` of a difference of code labels.
pub fn uleb128(isa: Isa) -> (u32, u32) {
    if isa.is_430x() { (23, 22) } else { (12, 11) }
}

/// A plain 16-bit operand word holding an absolute value.
pub fn word16(reloc: u32) -> FixupKind {
    FixupKind::data(2).with_reloc(reloc)
}

/// A 16-bit operand word holding the second word of a value, `#hi(sym)`.
pub fn hi16(reloc: u32) -> FixupKind {
    FixupKind::data(2)
        .with_reloc(reloc)
        .link(crate::section::LinkValue::Split(|v| (v >> 16) & 0xffff))
}

/// A 16-bit operand word measured from itself: symbolic addressing, `x(PC)`.
pub fn pcrel16(reloc: u32) -> FixupKind {
    FixupKind::pcrel(2, 0).with_reloc(reloc).unbiased_reloc()
}

/// A conditional jump's displacement: ten bits of word offset, −512 to 511,
/// from the word after the instruction.
pub fn jump10(reloc: u32) -> FixupKind {
    FixupKind::pcrel(2, 2)
        .with_field(11, 2)
        .with_reloc(reloc)
        .unbiased_reloc()
        .scatter(|w, v| (w & 0xfc00) | (((v >> 1) as u64) & 0x3ff))
}

/// `[7,4]+[32,16]`: the top four bits in bits 7 to 10 of the extension word,
/// the bottom sixteen in the word two words later. The source operand of an
/// extended instruction.
pub fn ext_src(reloc: u32, pcrel: bool) -> FixupKind {
    ext(reloc, pcrel, 6, |w, v| put(w, v, 32, 7, 0x0780))
}

/// `[0,4]+[32,16]`: as [`ext_src`], but the top bits are the extension
/// word's low nibble. The destination of an extended instruction whose
/// source needs no word.
pub fn ext_dst(reloc: u32, pcrel: bool) -> FixupKind {
    ext(reloc, pcrel, 6, |w, v| put(w, v, 32, 0, 0x000f))
}

/// `[0,4]+[48,16]`: the destination of an extended instruction whose source
/// took a word of its own, so the destination's word is one further on.
pub fn ext_odst(reloc: u32, pcrel: bool) -> FixupKind {
    ext(reloc, pcrel, 8, |w, v| put(w, v, 48, 0, 0x000f))
}

/// `[8,4]+[16,16]`: the source of an address instruction — `mova`, `adda`,
/// `calla` — which has no extension word and keeps the top bits in the
/// opcode.
pub fn adr_src(reloc: u32) -> FixupKind {
    ext(reloc, false, 4, |w, v| put(w, v, 16, 8, 0x0f00))
}

/// `[0,4]+[16,16]`: the destination of an address instruction.
pub fn adr_dst(reloc: u32, pcrel: bool) -> FixupKind {
    ext(reloc, pcrel, 4, |w, v| put(w, v, 16, 0, 0x000f))
}

/// A 20-bit field spread over `size` bytes by `scatter`.
fn ext(reloc: u32, pcrel: bool, size: u8, scatter: fn(u64, i64) -> u64) -> FixupKind {
    let base = if pcrel {
        FixupKind::pcrel(size, 0).unbiased_reloc()
    } else {
        FixupKind::data(size)
    };
    base.with_field(20, 1).with_reloc(reloc).scatter(scatter)
}

/// Writes a 20-bit value into a field read as a little-endian integer: the
/// low sixteen bits at bit `word`, the top four at bit `shift` of the first
/// word, under `mask`.
fn put(w: u64, v: i64, word: u32, shift: u32, mask: u64) -> u64 {
    let v = v as u64;
    let cleared = w & !(0xffffu64 << word) & !mask;
    cleared | ((v & 0xffff) << word) | (((v >> 16) & 0xf) << shift)
}
