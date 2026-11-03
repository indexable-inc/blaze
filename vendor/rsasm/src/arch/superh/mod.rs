//! Hitachi/Renesas SuperH, big-endian (`sh`) and little-endian (`shl`). `EM_SH`.
//!
//! Every instruction is one 16-bit word; the two targets differ only in the
//! order its two bytes are laid down. The operand syntax is GNU as's: bare
//! register names, `#` immediates, `@`-prefixed memory operands, and `!` for
//! comments (with `#` a comment only at the start of a line, since it is the
//! immediate prefix everywhere else).
//!
//! The pieces: [`reg`] names the registers, [`operand`] parses the addressing
//! modes, [`insn`] is the opcode table, [`encode`] matches operands against it
//! and fills in the fields, and [`pcrel`] handles branches and PC-relative
//! loads, whose displacement is measured from the instruction plus four.
//!
//! `sh` and `shl` accept the whole SH-1 to SH-4A instruction set with the FPU,
//! which is what GNU as does by default. The aliases `sh1`, `sh2`, `sh2e`,
//! `sh3`, `sh3e`, `sh4` and `sh4a` select a big-endian `sh` restricted to that
//! CPU's instructions, as `sh-elf-as --isa=sh2` and so on do (`--isa=sh` for
//! `sh1`), so `.arch sh2` rejects an FPU instruction. SH-2A's 32-bit
//! instructions and the SH-DSP extensions are not assembled.
//!
//! The ELF header names the least capable CPU that has every instruction the
//! file uses, which [`cpu`] works out the way GNU as does.

pub mod cpu;
pub mod encode;
pub mod insn;
pub mod operand;
pub mod pcrel;
pub mod reg;
pub mod reloc;

use crate::arch::{
    ArchState, Architecture, AsmCtx, CommentSyntax, Endian, FlatModifier, InsnRequest, Syntax,
};
use crate::cursor::Cursor;
use crate::dwarf::{CfiTarget, DwarfTarget, Flavor, cfi, numbered_register};
use crate::intern::Interner;
use crate::lexer::{Punct, TokKind, Token};
use crate::section::Variant;
use operand::OperandParser;
use reg::{Ctl, Reg};

pub const NAMES: &[&str] = &["sh", "shl"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let (canonical, endian, cpus) = match name {
        "sh" | "superh" => ("sh", Endian::Big, cpu::DEFAULT),
        "shl" => ("shl", Endian::Little, cpu::DEFAULT),
        "sh1" => ("sh", Endian::Big, cpu::SH1),
        "sh2" => ("sh", Endian::Big, cpu::SH2),
        "sh2e" => ("sh", Endian::Big, cpu::SH2E),
        "sh3" => ("sh", Endian::Big, cpu::SH3),
        "sh3e" => ("sh", Endian::Big, cpu::SH3E),
        "sh4" => ("sh", Endian::Big, cpu::SH4),
        "sh4a" => ("sh", Endian::Big, cpu::SH4A),
        _ => return None,
    };
    Some(Box::new(SuperH {
        name: canonical,
        endian,
        cpus,
    }))
}

pub struct SuperH {
    name: &'static str,
    endian: Endian,
    /// The [`cpu`] set this target starts from: every CPU, or just one.
    cpus: u32,
}

impl Architecture for SuperH {
    fn name(&self) -> &'static str {
        self.name
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["superh", "sh1", "sh2", "sh2e", "sh3", "sh3e", "sh4", "sh4a"]
    }

    fn endian(&self) -> Endian {
        self.endian
    }

    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        4
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: 32,
            syntax: Syntax::Att,
            features: u64::from(self.cpus),
            cpu_features: [0; 3],
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    /// There is only the one operand syntax.
    fn supports_syntax(&self, syntax: Syntax) -> bool {
        syntax == Syntax::Att
    }

    /// `EM_SH`, as `sh-elf-readelf -h` reports for both byte orders.
    fn elf_machine(&self) -> u16 {
        42
    }

    /// The `EF_SH_*` machine of the least capable CPU that has every
    /// instruction in the file, as `sh-elf-as` picks it: SH-1 for a file of
    /// data alone, SH-4 for one that uses `fipr`. Byte order plays no part.
    fn elf_flags(&self, state: &ArchState) -> u32 {
        cpu::elf_flags(cpu::remaining(state))
    }

    fn align_is_log2(&self) -> bool {
        true
    }

    fn pads_section_tail(&self, flags: &crate::section::SectionFlags) -> bool {
        flags.exec
    }

    /// `sh-elf-as` writes every addend into the field and zero into the
    /// entry, except for `R_SH_DIR16`, the one data relocation BFD does not
    /// mark `partial_inplace`.
    fn addend_in_field(&self, reloc: u32, _rela: bool) -> bool {
        reloc != reloc::DIR16
    }

    /// GNU as for SH comments with `!` anywhere, and with `#` only at the
    /// start of a line, where it cannot be confused with an immediate.
    /// `//` is not a comment: `mov r1,r2 // x` is an error there.
    fn comments(&self) -> CommentSyntax {
        CommentSyntax {
            anywhere: &["!"],
            line_start: &["#"],
        }
    }

    /// `.word` is 16 bits, matching the instruction width.
    fn word_bytes(&self) -> u8 {
        2
    }

    /// One byte, although SH instructions must sit on even addresses: GNU as
    /// does not align code for you. `.byte 1` followed by `nop` puts the
    /// `nop` at offset 1 (checked against `sh-elf-as`), and so does rsasm.
    fn align_unit(&self) -> u64 {
        1
    }

    /// `sh-elf-as` sizes branches with GNU as's generic relaxation.
    fn relaxation(&self) -> crate::arch::Relaxation {
        crate::arch::Relaxation::InOrder
    }

    /// `sh-elf-as` resolves a branch or PC-relative load to any symbol in the
    /// same section, weak ones included.
    fn defers_to_linker(&self, _r: &crate::arch::SameSectionRef<'_>) -> bool {
        false
    }

    /// `sh-elf-as` refuses a `.word` or `.long` off its own boundary
    /// ("misaligned data"), though not a `.2byte` or `.4byte`.
    fn aligns_data(&self) -> bool {
        true
    }

    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32> {
        reloc::data(size, pcrel)
    }

    fn modifier_reloc(&self, name: &str, size: u8, _pcrel: bool) -> Option<u32> {
        reloc::modifier(name, size)
    }

    fn flat_modifier(&self, name: &str) -> FlatModifier {
        reloc::flat_modifier(name)
    }

    /// GNU as's conventions, as for every SuperH encoding: code counted in
    /// words.
    fn dwarf(&self, _state: &ArchState, _format: crate::output::Format) -> DwarfTarget {
        DwarfTarget {
            cfi: Some(CfiTarget {
                data_align: -4,
                ra_column: 17,
                initial: vec![cfi::Insn::DefCfa(15, 0)],
                fde_encoding: 0x1b,
                eh_frame_align: 4,
                cie_version: 1,
            }),
            ..DwarfTarget::lines_only(Flavor::Gnu, 2)
        }
    }

    /// GNU as's `sh_regname_to_dw2regnum`, for the names it accepts: `r0`-`r15`,
    /// `pr` 17, `gbr` 19, `mach` 20, `macl` 21, `fpul` 23 and `fr0`-`fr15`
    /// from 25. Not `sp`.
    fn dwarf_register(
        &self,
        _state: &ArchState,
        _format: crate::output::Format,
        name: &str,
    ) -> Option<u32> {
        match name {
            "pr" => Some(17),
            "gbr" => Some(19),
            "mach" => Some(20),
            "macl" => Some(21),
            "fpul" => Some(23),
            _ => numbered_register(name, "r", 15)
                .or_else(|| numbered_register(name, "fr", 15).map(|n| 25 + n)),
        }
    }

    /// `nop` is `0009`. An odd pad gets a zero byte first, which is what
    /// GNU as's SH alignment handler writes, so the `nop`s after it land on
    /// the same boundaries.
    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        let len = len as usize;
        let mut out = Vec::with_capacity(len);
        if len % 2 == 1 {
            out.push(0);
        }
        while out.len() < len {
            out.extend_from_slice(&self.endian.bytes(0x0009, 2));
        }
        out
    }

    /// `@(8,pc)` is `. + 8`, so any operand naming `pc` may need `.`.
    fn operands_use_location(&self, interner: &Interner, operands: &[Token]) -> bool {
        operands.iter().any(|t| match t.kind {
            TokKind::Ident(n) => reg::lookup(interner.get(n)) == Some(Reg::Ctl(Ctl::Pc)),
            _ => false,
        })
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let mut cur = req.cursor();
        let mnemonic = mnemonic(cx, req, &mut cur)?;
        let entries: Vec<&'static insn::Entry> = insn::lookup(&mnemonic).collect();
        if entries.is_empty() {
            cx.error(
                req.mnemonic_span,
                format!("unknown instruction `{mnemonic}`"),
            );
            return None;
        }
        let ops = OperandParser { cx }.parse_list(&cur)?;
        encode::encode(cx, &mnemonic, &entries, &ops, req.span, self.endian)
    }
}

/// The full mnemonic, with the cursor moved past any part of it the lexer
/// split off.
///
/// Several SH mnemonics contain a `/`: `cmp/eq`, `cmp/hs`, `bt/s`,
/// `fcmp/gt`. The shared lexer stops an identifier at `/` (it is division
/// everywhere else), so `cmp/eq r1,r2` arrives as the mnemonic `cmp` followed
/// by the tokens `/`, `eq`, `r1`, and so on. The pieces are joined back here,
/// but only when nothing separates them: GNU as reads a mnemonic up to the
/// first space, so `cmp / eq` and `cmp /eq` are not `cmp/eq` there either.
fn mnemonic(cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>, cur: &mut Cursor<'_>) -> Option<String> {
    let mut m = cx.name(req.mnemonic).to_ascii_lowercase();
    let slash = cur.peek();
    if !slash.is_punct(Punct::Slash) || slash.preceded_by_space {
        return Some(m);
    }
    let suffix = cur.nth(1);
    match suffix.kind {
        TokKind::Ident(n) if !suffix.preceded_by_space => {
            m.push('/');
            m.push_str(&cx.name(n).to_ascii_lowercase());
            cur.advance();
            cur.advance();
            Some(m)
        }
        _ => {
            cx.error(
                req.mnemonic_span.to(slash.span),
                format!("expected the rest of a mnemonic such as `{m}/eq` after `/`"),
            );
            None
        }
    }
}
