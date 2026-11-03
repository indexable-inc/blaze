//! Renesas RL78, the 16-bit microcontroller family that succeeded the 78K0R.
//! `EM_RL78` (197).
//!
//! # Reference
//!
//! The GNU syntax of `rl78-elf-as` (binutils 2.47), which is what
//! `tools/xas-diff/run.sh rl78` compares against byte for byte. There is no
//! independent specification of that syntax, so its grammar
//! (`gas/config/rl78-parse.y`) decided which operand shapes exist and the
//! reference's output decided every byte; see [`insn`] for the opcode map and
//! [`operand`] for the addressing syntax.
//!
//! # Lexing
//!
//! RL78's GNU port is unlike the GNU default in four ways, all confirmed
//! against the reference and applied through [`Architecture::tune_lexer`] and
//! [`Architecture::comments`]:
//!
//! * `;` starts a comment anywhere, and `#` only in the first column, since
//!   `#` is the immediate prefix. `//` is not a comment.
//! * `@` separates statements; `;` cannot, being the comment character.
//! * Renesas radix suffixes work: `0FFH`, `1010B`, `17O`, `99D`.
//! * A leading zero does not make a number octal: `010` is ten.
//!
//! `.word` is four bytes wide, as `.int` and `.long` are.
//!
//! # Deliberate differences from the reference
//!
//! * A constant that does not fit its field is an error naming the limit.
//!   The reference keeps the low bits of `mov a, #256` without a word.
//! * Branch relaxation picks the short form exactly when its displacement
//!   fits. The reference measures from the start of the instruction rather
//!   than its end, so it gives up on the short form a few bytes early going
//!   forward, and going backward it keeps it a few bytes too long — writing a
//!   displacement that has wrapped around (`bz` 127 bytes back becomes
//!   `dd 7f`, a branch forward).
//! * A relative branch to a target outside its section is an error. See
//!   [`reloc::rel8`] for why no relocation can be written for it.
//!
//! # CC-RL syntax
//!
//! Renesas's CC-RL writes operands with the same sigils GNU as uses, so with
//! `-d ccrl` the one difference left for the backend is CC-RL's shorthand
//! `[DE]` and `[HL]` for a zero displacement; see
//! `insn::implicit_zero_displacement`. Its directives and expressions are the
//! core's (`crate::dialect_cc`).
//!
//! # Not implemented
//!
//! The `%lo16`/`%hi16`/`%hi8`/`%code` relocation functions, `.3byte`, and the
//! `-mrelax` linker-relaxation relocations.

pub mod encode;
pub mod insn;
pub mod operand;
pub mod reg;
pub mod reloc;

use crate::arch::{ArchState, Architecture, AsmCtx, CommentSyntax, Endian, InsnRequest, Syntax};
use crate::section::Variant;
use insn::Isa;

pub const NAMES: &[&str] = &["rl78"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let isa = match name {
        "rl78" | "rl78g14" => Isa::G14,
        "rl78g13" => Isa::G13,
        _ => return None,
    };
    Some(Box::new(Rl78 { isa }))
}

/// The RL78 backend. The cores differ only in which instructions exist, so
/// one backend serves them all.
pub struct Rl78 {
    isa: Isa,
}

impl Architecture for Rl78 {
    fn name(&self) -> &'static str {
        "rl78"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["rl78g13", "rl78g14"]
    }

    fn endian(&self) -> Endian {
        Endian::Little
    }

    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        // The address space is 20 bits, but RL78 objects are ELF32 and code
        // pointers in data are `.long`s; four is the class, not the bus.
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
        // There is one operand grammar; a `.intel_syntax` left over from an
        // x86 part of the file must not quietly apply to it.
        syntax == Syntax::Att
    }

    fn elf_machine(&self) -> u16 {
        // `EM_RL78`, as `rl78-elf-readelf -h` reports on the reference's
        // objects ("Machine: Renesas RL78").
        197
    }

    fn pcrel_number_is_address(&self) -> bool {
        true
    }

    /// `rl78-elf-as` resolves a branch to any symbol in the same section,
    /// weak ones included.
    fn defers_to_linker(&self, _r: &crate::arch::SameSectionRef<'_>) -> bool {
        false
    }

    fn align_is_log2(&self) -> bool {
        true
    }

    fn pads_section_tail(&self, _flags: &crate::section::SectionFlags) -> bool {
        true
    }

    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32> {
        reloc::data(size, pcrel)
    }

    fn comments(&self) -> CommentSyntax {
        CommentSyntax {
            anywhere: &[";"],
            line_start: &["#"],
        }
    }

    fn tune_lexer(&self, cfg: &mut crate::lexer::LexConfig) {
        // `line_separator_chars` is "@" in `gas/config/tc-rl78.c`; the GNU
        // default `;` is the comment character here.
        cfg.stmt_sep = vec!['@'];
        // The port is built with suffix numbers (`0FFH`), which in GNU as also
        // turns off the leading-zero octal reading: the reference assembles
        // `mov a, #010` as ten. A `1b`/`2f` is still a local label reference,
        // which the lexer checks before any suffix.
        cfg.radix_suffix = true;
        cfg.octal_leading_zero = false;
    }

    fn word_bytes(&self) -> u8 {
        // `md_pseudo_table` in `tc-rl78.c` makes `.word` a 4-byte `cons`.
        4
    }

    fn align_unit(&self) -> u64 {
        1
    }

    /// GNU as's conventions, as for every RL78 encoding. Its linker relaxes
    /// code, so GNU as gives every row an explicit address advance; it has no
    /// call frame information.
    /// GNU as leaves a local label's value in the relocated field, as its RX
    /// port does.
    fn local_value_in_field(&self, _reloc: u32) -> bool {
        true
    }

    fn dwarf(
        &self,
        _state: &ArchState,
        _format: crate::output::Format,
    ) -> crate::dwarf::DwarfTarget {
        crate::dwarf::DwarfTarget {
            fixed_advance_pc: true,
            ..crate::dwarf::DwarfTarget::lines_only(crate::dwarf::Flavor::Gnu, 1)
        }
    }

    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        // `nop` is `00`, so executable padding is zeros — which is also what
        // the reference pads `.balign` with in `.text`.
        vec![0x00; len as usize]
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let mnemonic = cx.name(req.mnemonic).to_ascii_lowercase();
        insn::assemble(cx, req, &mnemonic, self.isa)
    }
}
