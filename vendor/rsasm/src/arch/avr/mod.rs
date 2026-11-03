//! Microchip (formerly Atmel) AVR, the 8-bit microcontroller family. `EM_AVR`
//! (83).
//!
//! # Reference
//!
//! GNU binutils 2.47's `avr-elf-as`, which `tools/xas-diff/run.sh` compares
//! against for four cores (the default, `avr51`, `avrtiny` and an XMEGA with
//! the read-modify-write instructions), whole objects included, and whose
//! objects `tools/flat-diff/run.sh` links with `avr-elf-ld` to check flat
//! images. `tools/fuzz/avr.py` does both for random programs. The opcode
//! table, the MCU table and the operand rules are transcribed from
//! `include/opcode/avr.h` and `gas/config/tc-avr.c`; see [`insn`], [`isa`]
//! and [`operand`].
//!
//! # Targets
//!
//! `avr` is what `avr-elf-as` assembles without `-mmcu`: the AVR2 instruction
//! set, which has no `jmp`, `call`, `movw` or `mul`. The family names `avr1`
//! to `avr6`, `avrxmega1` to `avrxmega7` and `avrtiny`, and every device name
//! GNU as knows (`atmega328p`, `attiny85`, ...), select that core, from the
//! command line or with `.arch`. An instruction the core does not have is an
//! error, as it is there.
//!
//! # Linker relaxation
//!
//! `avr-elf-as` prepares every object for linker relaxation unless told
//! `-mno-link-relax`, and so does rsasm, which has no such option:
//!
//! * `e_flags` carries `EF_AVR_LINKRELAX_PREPARED` (0x80) next to the machine
//!   number.
//! * A branch or call to a label is relocated even within its own section,
//!   with the field left zero, since the linker may delete code between the
//!   two.
//! * A relocation against a local label names the label, not its section, so
//!   local labels that are referred to reach the symbol table.
//! * An `.align` or `.org` in a code section is recorded in `.avr.prop`, for
//!   the linker to keep while it deletes code; see [`prop`].
//!
//! # Lexing
//!
//! `;` starts a comment anywhere and `#` only in the first column, and `$`
//! separates statements, so it is not part of a name. Mnemonics and register
//! names are read in either case, and so are the modifiers of data (`.word
//! PM(main)`), but the modifiers of an `ldi` are lower-case only, as
//! `avr_ldi_expression` looks them up.
//!
//! `.` in an operand is the address of the next instruction: GNU as reserves
//! the instruction's bytes before it reads the operands, so `rjmp .` jumps
//! past itself.
//!
//! # Deliberate differences from the reference
//!
//! * A value that does not fit its field is an error naming the limit, where
//!   GNU as keeps the low bits of some of them, with a warning or without (a
//!   `call` target past 22 bits of word address, an AVR-tiny `lds`/`sts`
//!   address outside 0x40-0xbf, an `ldi` constant below -255). A number
//!   counted in words, `pm(3)`, has to be even, as GNU ld requires of a label.
//! * `ldd` and `std` need their displacement. GNU as reads one character past
//!   `Y` or `Z` looking for its `+`, which refuses `ldd r0, Y` except on the
//!   last line of a file.
//! * `lo8(gs(1))`, of a number, is `lo8(pm(1))`; GNU as stops with "unknown
//!   relocation type".
//! * Differences of two labels are written as numbers. Where GNU as writes a
//!   difference that crosses sections as an `R_AVR_DIFF*` relocation — in
//!   practice only in the DWARF it makes, since a difference within a section
//!   folds there too — rsasm writes the number the linker would start from
//!   without the relocation.
//! * `.arch` selects exactly the named core. GNU as adds the new name's
//!   instructions to those already allowed, and refuses a core with another
//!   machine number outright.
//!
//! # Not implemented
//!
//! The `__gcc_isr` pseudo-instruction (`-mgcc-isr`), and the warnings GNU as
//! gives for a skip over a two-word instruction and for operand combinations
//! with undefined results (`ld r26, X+`).

pub mod encode;
pub mod insn;
pub mod isa;
pub mod operand;
pub mod prop;
pub mod reloc;

use crate::arch::{
    ArchState, Architecture, AsmCtx, CommentSyntax, Endian, FlatModifier, InsnRequest, Syntax,
};
use crate::dwarf::{CfiTarget, DwarfTarget, Flavor, cfi};
use crate::section::Variant;
use isa::Mcu;

pub const NAMES: &[&str] = isa::FAMILIES;

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let mcu = if name == "avr" {
        isa::DEFAULT
    } else {
        isa::lookup(name)?
    };
    Some(Box::new(Avr { mcu }))
}

/// `EF_AVR_LINKRELAX_PREPARED`: the object refers to local symbols rather
/// than section offsets, so a linker may relax it.
pub const EF_AVR_LINKRELAX_PREPARED: u32 = 0x80;

/// The AVR backend, for one core.
pub struct Avr {
    mcu: Mcu,
}

impl Architecture for Avr {
    fn name(&self) -> &'static str {
        self.mcu.name
    }

    fn endian(&self) -> Endian {
        Endian::Little
    }

    /// ELF32, and the four-byte addresses GNU as gives DWARF
    /// (`DWARF2_ADDR_SIZE`), though no AVR has a four-byte program counter.
    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        4
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: 16,
            syntax: Syntax::Att,
            features: 0,
            cpu_features: [0; 3],
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    fn supports_syntax(&self, syntax: Syntax) -> bool {
        syntax == Syntax::Att
    }

    /// `EM_AVR`, which `avr-elf-readelf -h` calls "Atmel AVR 8-bit
    /// microcontroller".
    fn elf_machine(&self) -> u16 {
        83
    }

    /// The core's machine number (2 for `avr`, 5 for `avr5` and the devices
    /// in it, 102 for `avrxmega2`, 100 for `avrtiny`) with the relaxation
    /// flag, as `avr_elf_final_processing` and `bfd_elf_avr_final_write_processing`
    /// write them.
    fn elf_flags(&self, _state: &ArchState) -> u32 {
        u32::from(self.mcu.mach) | EF_AVR_LINKRELAX_PREPARED
    }

    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32> {
        reloc::data(size, pcrel)
    }

    /// GNU as for AVR aligns nothing in `.bss` by its size.
    fn local_common(&self) -> crate::arch::LocalCommon {
        crate::arch::LocalCommon::PACKED
    }

    fn modifier_reloc(&self, name: &str, size: u8, pcrel: bool) -> Option<u32> {
        reloc::modifier(name, size, pcrel)
    }

    fn flat_modifier(&self, name: &str) -> FlatModifier {
        match reloc::modifier_field(name) {
            Some((write, unit)) => FlatModifier::Field { write, unit },
            None => FlatModifier::LinkerOnly,
        }
    }

    fn expr_modifiers(&self) -> &'static [&'static str] {
        reloc::MODIFIERS
    }

    /// `comment_chars` is `;` and `line_comment_chars` `#`. `//` is not a
    /// comment.
    fn comments(&self) -> CommentSyntax {
        CommentSyntax {
            anywhere: &[";"],
            line_start: &["#"],
        }
    }

    /// `$` separates statements (`avr_line_separator_chars`) and is not a
    /// name character (`LEX_DOLLAR 0`). `;` is the comment character, so it
    /// cannot separate anything.
    fn tune_lexer(&self, cfg: &mut crate::lexer::LexConfig) {
        cfg.stmt_sep = vec!['$'];
        cfg.dollar_in_idents = false;
    }

    /// `avr_fix_adjustable` keeps the symbol of every relocation against a
    /// label in a section that is not mergeable, which under linker
    /// relaxation is all of them.
    fn relocates_with_label(&self, _reloc: u32) -> bool {
        true
    }

    /// `.align 3` is eight bytes, as on the other targets GNU as does not
    /// list as counting bytes.
    fn align_is_log2(&self) -> bool {
        true
    }

    /// `md_section_align` rounds every section's size up to its alignment.
    fn pads_section_tail(&self, _flags: &crate::section::SectionFlags) -> bool {
        true
    }

    /// GNU as's conventions: code counted in words, and, since the linker
    /// may relax it, every line-table advance written as a fixed one. The
    /// frame starts with the CFA two bytes above the stack pointer (DWARF
    /// register 32), or three on a core with a 22-bit program counter, and
    /// the return address just above it (`tc_cfi_frame_initial_instructions`);
    /// the stack post-decrements, so the data alignment is -1.
    fn dwarf(&self, _state: &ArchState, _format: crate::output::Format) -> DwarfTarget {
        let pc_bytes = if matches!(self.mcu.mach, 6 | 106 | 107) {
            3
        } else {
            2
        };
        DwarfTarget {
            fixed_advance_pc: true,
            cfi: Some(CfiTarget {
                data_align: -1,
                ra_column: 36,
                initial: vec![
                    cfi::Insn::DefCfa(32, pc_bytes),
                    cfi::Insn::Offset(36, 1 - pc_bytes),
                ],
                fde_encoding: 0x1b,
                eh_frame_align: 4,
                cie_version: 1,
            }),
            ..DwarfTarget::lines_only(Flavor::Gnu, 2)
        }
    }

    fn is_mnemonic(&self, name: &str) -> bool {
        insn::is_mnemonic(&name.to_ascii_lowercase())
    }

    fn layout_records(
        &self,
        places: &[crate::arch::LayoutPlace],
    ) -> Option<crate::arch::LayoutRecords> {
        prop::records(places)
    }

    /// `nop` is `0000`, so code padding is zeros, which is also what GNU as
    /// pads `.balign` with in `.text`.
    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        vec![0; len as usize]
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let mnemonic = cx.name(req.mnemonic).to_ascii_lowercase();
        encode::assemble(cx, req, &mnemonic, self.mcu)
    }
}
