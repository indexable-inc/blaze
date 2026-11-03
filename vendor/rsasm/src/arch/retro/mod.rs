//! The 8-bit families: Zilog Z80, MOS 6502, Intel 8080 and Intel 8051.
//!
//! Four backends share this module because they share a problem shape: byte
//! streams with 8-bit opcodes and little-endian 16-bit operands, no fixed
//! instruction width, and a 64 KiB address space. The Z80 and the 8080 also
//! share their *opcode* values — the 8080 set is the Z80 main page minus the
//! `EX AF,AF'`/`DJNZ`/`JR` corners — so [`i8080`] builds Intel mnemonics on top
//! of the tables in [`z80`] rather than repeating the bytes.
//!
//! **Flat binary is the intended output.** These machines predate ELF and have
//! no `EM_*` number, so [`Architecture::elf_machine`] returns 0 and
//! [`Architecture::data_reloc`] returns `None`; assemble with `-f bin`. An
//! unresolved external reference is therefore an error rather than a
//! relocation, which is the right answer for a target with no linker.
//!
//! ## Syntax
//!
//! Source is read in the 8-bit dialect unless another is asked for:
//! `lda #$12` and `lda (ptr),y` as cc65's ca65 reads them, `ld a,(ix+5)` and
//! `ex af,af'` as GNU as and vasm read Zilog source, `MVI A,12H` and `MOV
//! A,#12H` as the Macro Assembler AS reads Intel's. That dialect's
//! directives and the rules where those assemblers disagree are described
//! in the crate's `dialect` module; each backend was checked against its
//! reference in `tools/xas-diff`.
//!
//! In the GNU dialect the Z80 is lexed as GNU as for the Z80 lexes it: `;`
//! comments, `$12` and `12H` numbers. There is no GNU as for the other two,
//! and there the GNU lexer's `#` comment makes `lda #$12` unwritable, so the
//! 6502 backend accepts `$` as the immediate marker too (`lda $0x12`). See
//! [`mos6502`] for the details.

pub mod common;
pub mod i8080;
pub mod mcs51;
pub mod mos6502;
pub mod z80;

use crate::arch::{ArchState, Architecture, AsmCtx, CommentSyntax, Endian, InsnRequest, Syntax};
use crate::section::Variant;

pub const NAMES: &[&str] = &["z80", "6502", "i8080", "8051"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    Some(match name {
        "z80" | "zilog-z80" => Box::new(Retro::Z80) as Box<dyn Architecture>,
        "6502" | "mos6502" | "m6502" => Box::new(Retro::Mos6502),
        "i8080" | "8080" | "intel-8080" => Box::new(Retro::I8080),
        "8051" | "i8051" | "mcs51" | "mcs-51" => Box::new(Retro::Mcs51),
        _ => return None,
    })
}

/// The four backends, distinguished only by which instruction table they
/// consult: everything else about them (byte order, pointer width, output
/// format) is identical.
pub enum Retro {
    Z80,
    Mos6502,
    I8080,
    Mcs51,
}

impl Architecture for Retro {
    fn name(&self) -> &'static str {
        match self {
            Retro::Z80 => "z80",
            Retro::Mos6502 => "6502",
            Retro::I8080 => "i8080",
            Retro::Mcs51 => "8051",
        }
    }

    fn aliases(&self) -> &'static [&'static str] {
        match self {
            Retro::Z80 => &["zilog-z80"],
            Retro::Mos6502 => &["mos6502", "m6502"],
            Retro::I8080 => &["8080", "intel-8080"],
            Retro::Mcs51 => &["i8051", "mcs51", "mcs-51"],
        }
    }

    fn endian(&self) -> Endian {
        Endian::Little
    }

    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        2
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: 16,
            // These assemblers never had two operand syntaxes to choose
            // between; `Att` is simply the default the core starts from.
            syntax: Syntax::Att,
            features: 0,
            cpu_features: [0; 3],
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    fn supports_syntax(&self, syntax: Syntax) -> bool {
        // Only one operand grammar exists per machine, so a `.intel_syntax`
        // carried over from an x86 part of the file must not silently change
        // how these operands are read.
        syntax == Syntax::Att
    }

    fn elf_machine(&self) -> u16 {
        // No `EM_*` value was ever assigned to any of these. Returning 0
        // (`EM_NONE`) keeps the ELF writer honest; it refuses a non-64-bit
        // target anyway, and `-f bin` is what these backends are for.
        0
    }

    fn data_reloc(&self, _size: u8, _pcrel: bool) -> Option<u32> {
        // No relocation format exists for a flat 16-bit binary: everything
        // must be resolved by the time the assembler finishes.
        None
    }

    /// `JR 110H` jumps to address 0x110: with no object for a number to be
    /// an offset into, it can only be an address.
    fn pcrel_number_is_address(&self) -> bool {
        true
    }

    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        // Padding inside a code section has to be executable, so it is the
        // machine's real no-op rather than zero. On the 6502 zero is `BRK`.
        let nop = match self {
            Retro::Z80 | Retro::I8080 | Retro::Mcs51 => 0x00,
            Retro::Mos6502 => 0xea,
        };
        vec![nop; len as usize]
    }

    fn comments(&self) -> CommentSyntax {
        match self {
            // GNU as for the Z80 comments with `;`, and with `#` only in the
            // first column, where it is also a C preprocessor line marker.
            // The 8051 is the same shape: `;` comments, and `#` is the
            // immediate marker everywhere but the first column.
            Retro::Z80 | Retro::Mcs51 => CommentSyntax {
                anywhere: &[";"],
                line_start: &["#"],
            },
            Retro::Mos6502 | Retro::I8080 => CommentSyntax::HASH,
        }
    }

    fn tune_lexer(&self, cfg: &mut crate::lexer::LexConfig) {
        // GNU as for the Z80 also reads `$12` and `%1010`, `12H`-style
        // suffixes, and `AF'`; a lowercase `b` after digits stays a local
        // label reference, as it does there.
        if let Retro::Z80 | Retro::Mcs51 = self {
            cfg.radix_suffix = true;
            cfg.number_prefixes = vec![('$', 16), ('%', 2)];
            cfg.primed_af = matches!(self, Retro::Z80);
        }
    }

    fn default_dialect(&self) -> crate::lexer::Dialect {
        // Conventional 8-bit source is what people have for these machines;
        // see the module comment.
        crate::lexer::Dialect::EightBit
    }

    fn mnemonics(&self) -> Option<fn(&str) -> bool> {
        Some(match self {
            Retro::Z80 => z80::is_mnemonic,
            Retro::Mos6502 => mos6502::is_mnemonic,
            Retro::I8080 => i8080::is_mnemonic,
            Retro::Mcs51 => mcs51::is_mnemonic,
        })
    }

    fn equates(&self) -> &'static [&'static str] {
        match self {
            Retro::Mcs51 => mcs51::EQUATES,
            _ => &[],
        }
    }

    fn prelude(&self, dialect: crate::lexer::Dialect) -> String {
        match self {
            Retro::Mcs51 => mcs51::prelude(dialect),
            _ => String::new(),
        }
    }

    fn relaxation(&self) -> crate::arch::Relaxation {
        // AS sizes the 8051's generic `JMP` and `CALL` afresh on every pass;
        // see [`mcs51`]. The others keep the default.
        match self {
            Retro::Mcs51 => crate::arch::Relaxation::Shrinking,
            _ => crate::arch::Relaxation::FromLastPass,
        }
    }

    fn bit_addressing(&self) -> bool {
        // Only the MCS-51 numbers bit addresses, so only there does `P1.3`
        // mean one; see [`mcs51`].
        matches!(self, Retro::Mcs51)
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, insn: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let mnemonic = cx.name(insn.mnemonic).to_ascii_lowercase();
        match self {
            Retro::Z80 => z80::assemble(cx, insn, &mnemonic),
            Retro::Mos6502 => mos6502::assemble(cx, insn, &mnemonic),
            Retro::I8080 => i8080::assemble(cx, insn, &mnemonic),
            Retro::Mcs51 => mcs51::assemble(cx, insn, &mnemonic),
        }
    }
}
