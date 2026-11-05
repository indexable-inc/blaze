//! ELF relocation types for RX (`R_RX_*`).
//!
//! The numbers are the ones `rx-elf-readelf -r` printed for objects GNU as
//! 2.47 produced, cross-checked against binutils' `include/elf/rx.h`.
//!
//! RX's psABI spells its absolute data relocations as *signed* (`DIR8S`,
//! `DIR16S`, `DIR24S`), and those are what GNU as emits for `.byte`, `.short`
//! and `.3byte`. A 16-bit immediate inside an instruction is the unsigned-
//! agnostic `DIR16` instead, and `int #n` is the unsigned `DIR8U`.

pub const DIR32: u32 = 0x01;
pub const DIR24S: u32 = 0x02;
pub const DIR16: u32 = 0x03;
pub const DIR16S: u32 = 0x05;
pub const DIR8U: u32 = 0x07;
pub const DIR8S: u32 = 0x08;
pub const DIR24S_PCREL: u32 = 0x09;
pub const DIR16S_PCREL: u32 = 0x0a;
pub const DIR8S_PCREL: u32 = 0x0b;
/// The 3-bit displacement of `bra.s`, `beq.s` and `bne.s`.
///
/// Unlike the other PC-relative types, the linker does not add one to it:
/// the field lives in the opcode byte itself, so the displacement really is
/// measured from the relocated address.
pub const DIR3U_PCREL: u32 = 0x12;

/// The relocation for an `n`-byte data reference.
///
/// GNU as has no PC-relative data relocation for RX: `.long sym - .` becomes a
/// stack-machine expression (`R_RX_SYM`, `R_RX_OPsub`, `R_RX_ABS32`), which a
/// single fixup cannot express, so it is refused rather than approximated.
pub fn data(size: u8, pcrel: bool) -> Option<u32> {
    if pcrel {
        return None;
    }
    Some(match size {
        1 => DIR8S,
        2 => DIR16S,
        3 => DIR24S,
        4 => DIR32,
        _ => return None,
    })
}
