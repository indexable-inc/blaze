//! `R_AVR_*` relocation numbers, the fixup kinds that carry them, and the
//! arithmetic behind the `lo8()` family of modifiers.
//!
//! The numbers are `include/elf/avr.h` of binutils 2.47, and each one was
//! seen with `avr-elf-readelf -r` on an object `avr-elf-as` produced for the
//! operand in question. Which relocation goes with which operand is that
//! assembler's choice too (`gas/config/tc-avr.c`, `avr_operand`): an I/O
//! address has one of its own, `R_AVR_PORT6` or `R_AVR_PORT5`, and every
//! `ldi` immediate has one even where it is a plain number, because the
//! linker has to be able to relax around it.
//!
//! # How a value reaches its field
//!
//! An AVR instruction is one or two 16-bit words and no operand field is
//! contiguous, so every fixup here scatters its value through the word with
//! [`FieldEncoding::Scatter`]. The scatter functions are `md_apply_fix`'s
//! arithmetic, one for one.
//!
//! [`FieldEncoding::Scatter`]: crate::section::FieldEncoding::Scatter

use crate::section::FixupKind;

/// `.long sym`.
pub const R_AVR_32: u32 = 1;
/// A conditional branch: ±64 words from the instruction after it.
pub const R_AVR_7_PCREL: u32 = 2;
/// `rjmp` and `rcall`: ±2048 words.
pub const R_AVR_13_PCREL: u32 = 3;
/// `.word sym`, and the second word of a two-word `lds`/`sts`.
pub const R_AVR_16: u32 = 4;
/// `.word pm(sym)` or `gs(sym)`: the address as a word number.
pub const R_AVR_16_PM: u32 = 5;
/// `ldi Rd, lo8(sym)` and its relatives, one per byte of the address, with a
/// `_NEG` form for `lo8(-(sym))`, a `_PM` form for `lo8(pm(sym))`, which
/// counts in words, and a `_GS` form for `lo8(gs(sym))`, which lets the
/// linker build a stub.
pub const R_AVR_LO8_LDI: u32 = 6;
pub const R_AVR_HI8_LDI: u32 = 7;
pub const R_AVR_HH8_LDI: u32 = 8;
pub const R_AVR_LO8_LDI_NEG: u32 = 9;
pub const R_AVR_HI8_LDI_NEG: u32 = 10;
pub const R_AVR_HH8_LDI_NEG: u32 = 11;
pub const R_AVR_LO8_LDI_PM: u32 = 12;
pub const R_AVR_HI8_LDI_PM: u32 = 13;
pub const R_AVR_HH8_LDI_PM: u32 = 14;
pub const R_AVR_LO8_LDI_PM_NEG: u32 = 15;
pub const R_AVR_HI8_LDI_PM_NEG: u32 = 16;
pub const R_AVR_HH8_LDI_PM_NEG: u32 = 17;
/// `call` and `jmp`: 22 bits of word address spread over two words.
pub const R_AVR_CALL: u32 = 18;
/// An `ldi`-family immediate with no modifier on it.
pub const R_AVR_LDI: u32 = 19;
/// The `ldd`/`std` displacement, 0 to 63.
pub const R_AVR_6: u32 = 20;
/// The `adiw`/`sbiw` constant, 0 to 63.
pub const R_AVR_6_ADIW: u32 = 21;
/// The top byte of a 32-bit value: `hhi8(sym)`.
pub const R_AVR_MS8_LDI: u32 = 22;
pub const R_AVR_MS8_LDI_NEG: u32 = 23;
pub const R_AVR_LO8_LDI_GS: u32 = 24;
pub const R_AVR_HI8_LDI_GS: u32 = 25;
/// `.byte sym`.
pub const R_AVR_8: u32 = 26;
/// `.byte lo8(sym)`, `hi8(sym)` and `hlo8(sym)`, which are a different
/// relocation from the `ldi` ones: they fill a whole byte rather than the
/// two nibbles of an instruction word.
pub const R_AVR_8_LO8: u32 = 27;
pub const R_AVR_8_HI8: u32 = 28;
pub const R_AVR_8_HLO8: u32 = 29;
// 30 to 32 are `R_AVR_DIFF8`, `DIFF16` and `DIFF32`, which GNU as adds to a
// difference of two labels the linker may still change by relaxing the code
// between them. rsasm lays out the section itself and writes the number; see
// the module documentation of [`super`].
/// The AVR-tiny `lds`/`sts` address, 0x40 to 0xbf.
pub const R_AVR_LDS_STS_16: u32 = 33;
/// The `in`/`out` I/O address, 0 to 63.
pub const R_AVR_PORT6: u32 = 34;
/// The `cbi`/`sbi`/`sbic`/`sbis` I/O address, 0 to 31.
pub const R_AVR_PORT5: u32 = 35;
/// `.long sym - .`.
pub const R_AVR_32_PCREL: u32 = 36;

/// The relocation for a data directive of `size` bytes.
pub fn data(size: u8, pcrel: bool) -> Option<u32> {
    match (size, pcrel) {
        (1, false) => Some(R_AVR_8),
        (2, false) => Some(R_AVR_16),
        (4, false) => Some(R_AVR_32),
        // `tc_gen_reloc` turns a PC-relative `BFD_RELOC_32` into
        // `R_AVR_32_PCREL`; there is no such form at any other width.
        (4, true) => Some(R_AVR_32_PCREL),
        _ => None,
    }
}

/// The relocation a data-directive modifier selects, or `None` where the
/// modifier does not go with a field of that width — which is what
/// `avr_cons_fix_new` calls an "illegal relocation size".
///
/// `exp_mod_data` in `gas/config/tc-avr.c` is the whole list: `lo8`, `hi8`,
/// `hlo8` and `hh8` for one byte, `pm` and `gs` for two. `hh8` on data is
/// the *third* byte, not the fourth — the same relocation as `hlo8` — which
/// is not what `hh8` means in an `ldi`.
pub fn modifier(name: &str, size: u8, pcrel: bool) -> Option<u32> {
    if pcrel {
        return None;
    }
    match (name, size) {
        ("lo8", 1) => Some(R_AVR_8_LO8),
        ("hi8", 1) => Some(R_AVR_8_HI8),
        ("hlo8" | "hh8", 1) => Some(R_AVR_8_HLO8),
        ("pm" | "gs", 2) => Some(R_AVR_16_PM),
        _ => None,
    }
}

/// A field's [`FieldEncoding::Scatter`] function.
///
/// [`FieldEncoding::Scatter`]: crate::section::FieldEncoding::Scatter
pub type Write = fn(u64, i64) -> u64;

/// How a data-directive modifier writes its field, and what the value it
/// takes part of has to be a multiple of: `md_apply_fix`'s
/// `BFD_RELOC_AVR_8_*` and `16_PM` cases. `pm()` counts in words, and GNU ld
/// refuses an odd address for it ("relocation target address is odd").
pub fn modifier_field(name: &str) -> Option<(Write, u8)> {
    let write: Write = match name {
        "lo8" => |_, v| v as u64 & 0xff,
        "hi8" => |_, v| (v >> 8) as u64 & 0xff,
        "hlo8" | "hh8" => |_, v| (v >> 16) as u64 & 0xff,
        "pm" | "gs" => |_, v| (v >> 1) as u64 & 0xffff,
        _ => return None,
    };
    Some((write, if matches!(name, "pm" | "gs") { 2 } else { 1 }))
}

/// The names [`modifier`] knows, as the expression parser reads them:
/// `lo8(expr)` is a call, not a `@` suffix.
pub const MODIFIERS: &[&str] = &["lo8", "hi8", "hlo8", "hh8", "pm", "gs"];

// ---- instruction fields ---------------------------------------------------

/// `LDI_IMMEDIATE` from `gas/config/tc-avr.c`: the two nibbles an `ldi`-family
/// immediate lives in.
fn ldi_immediate(word: u64, v: i64) -> u64 {
    word | (v as u64 & 0xf) | ((v as u64) << 4) & 0xf00
}

/// An `ldi`, `andi`, `ori`, `cpi`, `subi` or `sbci` immediate with no
/// modifier on it: `R_AVR_LDI`, whose linker writes the low byte.
///
/// `md_apply_fix` refuses a value above 255 and `avr_ldi_expression` warns
/// below -255, so those are the bounds. The field itself is eight bits; a
/// negative value reaches it as its low byte, which is how `ldi r16, -1`
/// loads 0xff.
pub fn ldi() -> FixupKind {
    FixupKind::data(2)
        .with_reloc(R_AVR_LDI)
        .with_limits(-255, 255)
        .scatter(ldi_immediate)
}

/// One byte of an address in an `ldi`-family immediate, as `lo8()` and its
/// relatives select it with relocation `reloc`.
///
/// The byte is taken as the field is written, from the whole value: where
/// the value is known that is here, and where it is not the relocation tells
/// the linker which byte to take. The value itself can be anything, except
/// that one counted in words (the `_PM` and `_GS` forms) has to be even —
/// GNU ld refuses an odd one, and GNU as an odd `call` target.
pub fn ldi_part(reloc: u32) -> FixupKind {
    let write: Write = match reloc {
        R_AVR_LO8_LDI => |w, v| ldi_immediate(w, v),
        R_AVR_HI8_LDI => |w, v| ldi_immediate(w, v >> 8),
        R_AVR_HH8_LDI => |w, v| ldi_immediate(w, v >> 16),
        R_AVR_MS8_LDI => |w, v| ldi_immediate(w, v >> 24),
        R_AVR_LO8_LDI_NEG => |w, v| ldi_immediate(w, v.wrapping_neg()),
        R_AVR_HI8_LDI_NEG => |w, v| ldi_immediate(w, v.wrapping_neg() >> 8),
        R_AVR_HH8_LDI_NEG => |w, v| ldi_immediate(w, v.wrapping_neg() >> 16),
        R_AVR_MS8_LDI_NEG => |w, v| ldi_immediate(w, v.wrapping_neg() >> 24),
        R_AVR_LO8_LDI_PM | R_AVR_LO8_LDI_GS => |w, v| ldi_immediate(w, v >> 1),
        R_AVR_HI8_LDI_PM | R_AVR_HI8_LDI_GS => |w, v| ldi_immediate(w, v >> 9),
        R_AVR_HH8_LDI_PM => |w, v| ldi_immediate(w, v >> 17),
        R_AVR_LO8_LDI_PM_NEG => |w, v| ldi_immediate(w, v.wrapping_neg() >> 1),
        R_AVR_HI8_LDI_PM_NEG => |w, v| ldi_immediate(w, v.wrapping_neg() >> 9),
        R_AVR_HH8_LDI_PM_NEG => |w, v| ldi_immediate(w, v.wrapping_neg() >> 17),
        _ => unreachable!("relocation {reloc} is not an ldi modifier"),
    };
    let words = matches!(
        reloc,
        R_AVR_LO8_LDI_PM
            | R_AVR_HI8_LDI_PM
            | R_AVR_HH8_LDI_PM
            | R_AVR_LO8_LDI_PM_NEG
            | R_AVR_HI8_LDI_PM_NEG
            | R_AVR_HH8_LDI_PM_NEG
            | R_AVR_LO8_LDI_GS
            | R_AVR_HI8_LDI_GS
    );
    FixupKind::data(2)
        .with_reloc(reloc)
        .with_field(63, if words { 2 } else { 1 })
        .scatter(write)
}

/// A conditional branch: `R_AVR_7_PCREL`, ±64 words from the instruction
/// after this one.
///
/// A branch to a symbol is always left to the linker in an object, even to a
/// label a few words away in the same section: `TC_VALIDATE_FIX` in
/// `gas/config/tc-avr.h` sends every such fixup straight to a relocation,
/// since relaxation may delete code in between.
///
/// The relocation is unbiased. `elf32_avr_relocate_section` computes
/// `S + A - P - 2` with `P` the address of the instruction itself, so the
/// step to the next instruction is already in the linker's arithmetic and
/// GNU as writes an addend of 0.
pub fn rel7() -> FixupKind {
    FixupKind::pcrel(2, 2)
        .with_reloc(R_AVR_7_PCREL)
        .with_field(8, 2)
        .unbiased_reloc()
        .relocated_in_objects()
        .scatter(|word, v| word | ((((v >> 1) << 3) as u64) & 0x3f8))
}

/// `rjmp` and `rcall`: `R_AVR_13_PCREL`, ±2048 words. See [`rel7`] for why
/// the relocation is always written and unbiased.
///
/// `wraps` is for a device small enough that its address space wraps around,
/// where a displacement is kept to its low 12 bits whatever its size. Which
/// devices those are depends on who works the displacement out: GNU as, for a
/// target that is a number, wraps on any device with no more than 8K of
/// program memory (`md_apply_fix`); GNU ld, for a label, only in an object
/// for the avr2, avr25 and avr4 machines (`elf32_avr_relocate_section`).
pub fn rel13(wraps: bool) -> FixupKind {
    FixupKind::pcrel(2, 2)
        .with_reloc(R_AVR_13_PCREL)
        .unbiased_reloc()
        .relocated_in_objects()
        .with_field(if wraps { 64 } else { 13 }, 2)
        .scatter(|word, v| word | (((v >> 1) as u64) & 0xfff))
}

/// `call` and `jmp`: `R_AVR_CALL`, a 22-bit word address in the two words of
/// the instruction. Bits 16 and 17-21 of the word address go into the first
/// word, the low 16 into the second.
pub fn call() -> FixupKind {
    FixupKind::data(4)
        .with_reloc(R_AVR_CALL)
        .with_field(23, 2)
        .scatter(|word, v| {
            let w = (v >> 1) as u64;
            word | ((w & 0x10000) | ((w << 3) & 0x1f0_0000)) >> 16 | ((w & 0xffff) << 16)
        })
}

/// The second word of a two-word `lds` or `sts`: a plain 16-bit data address
/// at offset 2 in the instruction, which is where GNU as puts its fixup too.
pub fn data16() -> FixupKind {
    FixupKind::data(2).with_reloc(R_AVR_16)
}

/// The AVR-tiny one-word `lds`/`sts` address: seven bits scattered through
/// the word, naming 0x40 to 0xbf.
///
/// GNU as warns rather than refuses outside that range; rsasm refuses, since
/// the bits outside it cannot be encoded.
pub fn lds_sts_16() -> FixupKind {
    FixupKind::data(2)
        .with_reloc(R_AVR_LDS_STS_16)
        .with_limits(0x40, 0xbf)
        .scatter(|word, v| {
            let v = v as u64;
            word | (v & 0xf) | ((v & 0x30) << 5) | ((v & 0x40) << 2)
        })
}

/// The `ldd`/`std` displacement: six bits, 0 to 63, in three pieces.
pub fn disp6() -> FixupKind {
    FixupKind::data(2)
        .with_reloc(R_AVR_6)
        .with_limits(0, 63)
        .scatter(|word, v| {
            let v = v as u64;
            word | (v & 7) | ((v & 0x18) << 7) | ((v & 0x20) << 8)
        })
}

/// The `adiw`/`sbiw` constant: six bits, 0 to 63, in two pieces.
pub fn adiw6() -> FixupKind {
    FixupKind::data(2)
        .with_reloc(R_AVR_6_ADIW)
        .with_limits(0, 63)
        .scatter(|word, v| word | (v as u64 & 0xf) | ((v as u64 & 0x30) << 2))
}

/// The `in`/`out` I/O address: six bits, 0 to 63, in two pieces.
pub fn port6() -> FixupKind {
    FixupKind::data(2)
        .with_reloc(R_AVR_PORT6)
        .with_limits(0, 63)
        .scatter(|word, v| word | ((v as u64 & 0x30) << 5) | (v as u64 & 0xf))
}

/// The `cbi`/`sbi`/`sbic`/`sbis` I/O address: five bits, 0 to 31.
pub fn port5() -> FixupKind {
    FixupKind::data(2)
        .with_reloc(R_AVR_PORT5)
        .with_limits(0, 31)
        .scatter(|word, v| word | ((v as u64 & 0x1f) << 3))
}
