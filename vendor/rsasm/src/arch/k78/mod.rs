//! NEC/Renesas 78K0, in the syntax of Renesas's CA78K0 assembler.
//!
//! # How this is checked
//!
//! Neither GNU as nor llvm-mc knows the 78K0, and CA78K0 itself is a
//! proprietary Windows tool, so the table below was built from the vendor's
//! own documentation and the tests walk all of it. (The RL78, which binutils
//! does support, is a later and differently encoded architecture; its
//! encodings are no evidence for the 78K0's.)
//!
//! There is a second opinion all the same: the Macro Assembler AS, whose name
//! for the family is `78070`. `tools/xas-diff` assembles every form of the
//! table with both, and `tools/fuzz/nec78k0.py` does the same for random whole
//! programs; both say what AS reads differently and why.
//!
//! The documents, all published by NEC or Renesas:
//!
//! * *78K/0 Series User's Manual: Instructions*, U12326EJ4V0UM00, 4th
//!   edition (2001). Section 4.2 (pages 38–45) is the instruction code list
//!   that [`table`] transcribes; section 4.1.1 (page 32) the operand
//!   identifiers; chapter 3 (pages 20–31) the addressing modes, including how
//!   `CALLF`, `CALLT` and short direct addresses map onto memory.
//! * *RA78K0 Assembler Package User's Manual: Language*, U17198EJ1V0UM00
//!   (RA78K0 is the assembler in the CA78K0 package): the operand sigils
//!   (Table 2-8, page 38), bit terms (section 2.5, pages 65–67), operand
//!   ranges and which may be symbolic (Tables 2-19 and 2-21, pages 68–71), and
//!   the `BR` directive (section 3.7, pages 114–116).
//! * *78K0/Kx2 User's Manual: Hardware*, R01UH0008EJ0401, section 29.2
//!   (pages 761–768): the operation list, whose "Bytes" column the tests use
//!   as an independent check on every instruction length.
//!
//! # Layout
//!
//! * [`table`] — the code table, one [`table::Row`] per printed line.
//! * [`form`] — reads the rows, rejecting any whose columns disagree.
//! * [`operand`] — CA78K0 operand syntax.
//! * [`encode`] — matches operands to forms and emits bytes and fixups.
//!
//! # What is not here
//!
//! * **Device SFR names.** CA78K0 takes SFR symbols such as `P0` or `PM0`
//!   from a device file selected with `$PROCESSOR`; they differ between 78K0
//!   devices, and the core has no `$PROCESSOR`. Guessing one device's names
//!   would silently mis-assemble another's, so SFRs are written as addresses
//!   or `EQU`s (`PM0 EQU 0FF20H`), which the manual allows for `sfr`
//!   operands (Table 2-21, note 5). `PSW` and `SP` are recognised, because
//!   the code table names them itself.
//! * **`HIGH`, `LOW` and the other word operators** of RA78K0 expressions
//!   (Table A-2), which belong to the core's expression parser.
//! * **Bit symbols** (`FLAG EQU 0FE20H.3`), which need `EQU` to accept a bit
//!   term.
//!
//! Flat binary is the intended output: CA78K0 writes its own object format,
//! not ELF, and no `EM_*` number was ever assigned to the 78K0.

pub mod encode;
pub mod form;
pub mod operand;
pub mod table;

use crate::arch::{ArchState, Architecture, AsmCtx, Endian, InsnRequest, Syntax};
use crate::section::Variant;

pub const NAMES: &[&str] = &["78k0"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    match name {
        "78k0" | "78k" | "78k0s" | "upd78f" => Some(Box::new(K78)),
        _ => None,
    }
}

/// The 78K0 backend.
pub struct K78;

impl Architecture for K78 {
    fn name(&self) -> &'static str {
        "78k0"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["78k", "78k0s", "upd78f"]
    }

    fn endian(&self) -> Endian {
        // Every two-byte field in the code table is `Low` then `High`.
        Endian::Little
    }

    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        // A 16-bit address space.
        2
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: 16,
            // There is only one operand syntax; `Att` is merely the core's
            // starting value.
            syntax: Syntax::Att,
            features: 0,
            cpu_features: [0; 3],
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    fn supports_syntax(&self, syntax: Syntax) -> bool {
        // A `.intel_syntax` left over from an x86 part of the file must not be
        // taken to change how 78K0 operands read.
        syntax == Syntax::Att
    }

    fn elf_machine(&self) -> u16 {
        // No `EM_*` value exists for the 78K0, and CA78K0 does not produce
        // ELF: this backend targets flat binaries, so the ELF writer is given
        // `EM_NONE`.
        0
    }

    fn default_dialect(&self) -> crate::lexer::Dialect {
        crate::lexer::Dialect::Renesas
    }

    fn data_reloc(&self, _size: u8, _pcrel: bool) -> Option<u32> {
        // CA78K0 does not produce ELF, and this backend targets flat binaries,
        // so there is no relocation to emit: every reference must resolve by
        // the end of assembly, and one that does not is an error.
        None
    }

    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        // `NOP` is `0000 0000` (U12326EJ4V0UM page 45). Padding in code stays
        // executable.
        vec![0x00; len as usize]
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, insn: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let mnemonic = cx.name(insn.mnemonic).to_string();
        encode::assemble(cx, insn, &mnemonic)
    }
}
