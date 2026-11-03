//! ARM 64-bit (A64). `EM_AARCH64`.
//!
//! A64 is fixed-width: every instruction is exactly four bytes, so this
//! backend always returns a single [`Variant`] and never asks the layout pass
//! to choose between encodings. What relaxation buys elsewhere, a fixup's
//! `value_bits` buys here — an out-of-range branch is a diagnostic rather than
//! a longer encoding, because there is no longer encoding.
//!
//! The other consequence of fixed width is that displacements never fit in one
//! contiguous field. Every PC-relative fixup therefore uses
//! [`crate::section::FieldEncoding::Scatter`] to weave its value through the
//! instruction word; the scatter functions live in [`encode`].
//!
//! # Two encoders
//!
//! The general-purpose instruction set is written out family by family in
//! [`insn`], where the interesting work is in the aliases. SIMD, floating
//! point and SVE are thousands of forms that differ in a few opcode bits, and
//! come from a table measured against llvm-mc; see the `table` module and
//! `tools/tables/README.md`. A line goes to the table if only the table has
//! its mnemonic, or if an operand is a register only a table form takes.
//!
//! # The `#` sigil
//!
//! A64 source conventionally writes immediates as `#imm`. `#` is a comment
//! only in the first column (see `comments` below), so both `add x0, x1,
//! #1` and the bare `add x0, x1, 1` that GNU as and llvm-mc also accept work.

pub(crate) mod cpu;
#[doc(hidden)]
pub mod cpu_data;
pub mod encode;
pub mod insn;
pub mod operand;
pub mod reg;
pub mod reloc;
pub mod sysreg;
// Not public API: the generated tables and the table's matcher are
// internals, and `sysreg_data`, `table_data` and `table_names` are generated
// files.
mod sysreg_data;
pub(crate) mod table;
mod table_data;
mod table_names;

use crate::arch::{ArchState, Architecture, AsmCtx, CpuOption, Endian, InsnRequest, Syntax};
use crate::dwarf::{CfiTarget, DwarfTarget, Flavor, cfi, numbered_register};
use crate::section::Variant;

pub const NAMES: &[&str] = &["aarch64"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    match name {
        "aarch64" | "arm64" | "armv8" | "armv8-a" | "aarch64le" => Some(Box::new(AArch64)),
        _ => None,
    }
}

pub struct AArch64;

/// The canonical `nop`. Alignment padding in an executable section must stay
/// executable, and unlike x86 there is only one no-op worth emitting.
const NOP: u32 = 0xd503_201f;

impl Architecture for AArch64 {
    fn name(&self) -> &'static str {
        "aarch64"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["arm64", "armv8", "armv8-a", "aarch64le"]
    }

    fn endian(&self) -> Endian {
        Endian::Little
    }

    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        8
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: 64,
            syntax: Syntax::Att,
            features: 0,
            cpu_features: cpu::initial(),
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    /// `.arch` and `.cpu` name one of GNU as's AArch64 architectures or
    /// CPUs here rather than another backend, and decide which instructions
    /// assemble; see [`cpu`].
    fn selects_cpu(
        &self,
        state: &mut ArchState,
        name: &str,
        cpu_name: bool,
    ) -> Result<bool, String> {
        // The architecture or CPU has to be one of this backend's before a
        // `+name` suffix after it can be read as an extension rather than as
        // part of another target's name.
        if !cpu::knows(name, cpu_name) {
            return Ok(false);
        }
        cpu::directive_arch(state, name, cpu_name).map(|()| true)
    }

    /// `-march=` and `-mcpu=`, which take the `all` entry the directives
    /// pass over as well as the `+name` extension suffixes.
    fn select_option(
        &self,
        state: &mut ArchState,
        opt: CpuOption,
        name: &str,
    ) -> Result<(), String> {
        cpu::option(state, opt, name)
    }

    /// A64 has one operand syntax. `.intel_syntax` in a file that also has x86
    /// in it must not make A64 statements unassemblable, so both spellings are
    /// accepted and neither changes anything.
    fn supports_syntax(&self, _syntax: Syntax) -> bool {
        true
    }

    fn elf_machine(&self) -> u16 {
        183 // EM_AARCH64
    }

    fn align_is_log2(&self) -> bool {
        true
    }

    /// llvm-mc, the reference, aligns every executable section to the 4
    /// bytes of an instruction, whatever is in it. GNU as instead aligns a
    /// section of any kind once an instruction is assembled into it.
    fn section_align(
        &self,
        _state: &ArchState,
        _name: &str,
        flags: &crate::section::SectionFlags,
    ) -> u64 {
        if flags.exec { 4 } else { 1 }
    }

    /// AArch64 writes immediates as `#1`, so `#` is a comment only in the
    /// first column and `//` is the comment everywhere else.
    fn comments(&self) -> crate::arch::CommentSyntax {
        crate::arch::CommentSyntax {
            anywhere: &["//"],
            line_start: &["#"],
        }
    }

    fn word_bytes(&self) -> u8 {
        4
    }

    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32> {
        if pcrel {
            reloc::pcrel(size)
        } else {
            reloc::abs(size)
        }
    }

    /// `.xword %dtprel(sym)`, GNU as's one data modifier for AArch64, which
    /// it reads in these four directives and accepts only in the eight-byte
    /// two.
    fn percent_modifiers(&self, directive: &str) -> &'static [&'static str] {
        match directive {
            ".word" | ".long" | ".xword" | ".dword" => &["dtprel"],
            _ => &[],
        }
    }

    fn modifier_reloc(&self, name: &str, size: u8, pcrel: bool) -> Option<u32> {
        (name == "dtprel" && size == 8 && !pcrel).then_some(reloc::TLS_DTPREL64)
    }

    /// `%dtprel(sym)` makes an undefined `sym` thread-local, as GNU as does.
    fn modifier_symbols(&self, name: &str) -> crate::arch::ModifierSymbols {
        crate::arch::ModifierSymbols {
            needs: None,
            tls: name == "dtprel",
            ..crate::arch::ModifierSymbols::default()
        }
    }

    /// Darwin's page modifiers, each valid only on the field its name
    /// describes: `@PAGE` on an `adrp`, `@PAGEOFF` on the offset that
    /// completes it, the `@GOT` pair for a load through the GOT, and the
    /// `@TLVP` pair for the descriptor of a thread-local variable.
    fn modifier_class(
        &self,
        name: &str,
        kind: &crate::section::FixupKind,
    ) -> Option<crate::reloc::RelocClass> {
        use crate::reloc::RelocClass;
        let class = match name {
            "page" => RelocClass::Page,
            "pageoff" => RelocClass::PageOff,
            "gotpage" => RelocClass::GotPage,
            "gotpageoff" => RelocClass::GotPageOff,
            // A thread-local reference goes in the same two fields a plain
            // page reference does, so the field each of these belongs on is
            // the one the class it replaces already names.
            "tlvppage" if kind.class == RelocClass::Page => {
                return Some(RelocClass::ThreadVariablePage);
            }
            "tlvppageoff" if kind.class == RelocClass::PageOff => {
                return Some(RelocClass::ThreadVariablePageOff);
            }
            // In data, `sym@GOT` is the address of the symbol's slot.
            "got" if kind.class == RelocClass::Plain && !kind.pcrel => {
                return Some(RelocClass::Got);
            }
            _ => return None,
        };
        match kind.class {
            // llvm-mc branches to the symbol whatever page modifier it has.
            RelocClass::Branch => Some(RelocClass::Branch),
            k => (k == class).then_some(class),
        }
    }

    /// llvm-mc's conventions, as for every AArch64 encoding: code and
    /// addresses counted in bytes, where GNU as counts instructions.
    ///
    /// A PE object has no `DW_EH_PE_pcrel` FDE address: llvm-mc writes the
    /// plain pointer `MCAsmInfoCOFF` asks for, and the section-relative
    /// relocation COFF has instead. A Mach-O object has a distance like an
    /// ELF one, but a pointer-sized one, since `MCObjectFileInfo` gives
    /// Darwin `DW_EH_PE_pcrel` with no width of its own.
    fn dwarf(&self, _state: &ArchState, format: crate::output::Format) -> DwarfTarget {
        DwarfTarget {
            cfi: Some(CfiTarget {
                // The size of a callee-saved stack slot, which Darwin alone
                // gives its real width (`CalleeSaveStackSlotSize`); every
                // other AArch64 target leaves `MCAsmInfo`'s default of four.
                data_align: if format == crate::output::Format::MachO {
                    -8
                } else {
                    -4
                },
                ra_column: 30,
                initial: vec![cfi::Insn::DefCfa(31, 0)],
                fde_encoding: match format {
                    f if f.is_coff() => 0x00,
                    crate::output::Format::MachO => 0x10,
                    _ => 0x1b,
                },
                eh_frame_align: 8,
                cie_version: 1,
            }),
            ..DwarfTarget::lines_only(Flavor::Llvm, 1)
        }
    }

    /// The AAPCS64 DWARF numbering of the names llvm-mc accepts: `x`/`w`
    /// registers 0-30, the stack pointer and zero register both 31, and a
    /// vector register as 64 up by whichever width names it.
    fn dwarf_register(
        &self,
        _state: &ArchState,
        _format: crate::output::Format,
        name: &str,
    ) -> Option<u32> {
        match name {
            "sp" | "wsp" | "xzr" | "wzr" => return Some(31),
            "fp" => return Some(29),
            "lr" => return Some(30),
            _ => {}
        }
        numbered_register(name, "x", 31)
            .or_else(|| numbered_register(name, "w", 30))
            .or_else(|| {
                ["b", "h", "s", "d", "q"]
                    .iter()
                    .find_map(|p| numbered_register(name, p, 31))
                    .map(|n| 64 + n)
            })
    }

    /// The compact unwind word `__LD,__compact_unwind` holds for a frame,
    /// which on arm64 describes a standard frame pointer prologue, a
    /// frameless function with a fixed stack adjustment, or neither.
    ///
    /// `DarwinAArch64AsmBackend::generateCompactUnwindEncoding` reads the
    /// directives in the order a compiler writes them and gives up on
    /// anything else: `.cfi_def_cfa` has to name the frame pointer and be
    /// followed by the two `.cfi_offset`s that saved it and the return
    /// address, each pair of callee-saved registers has to be saved in
    /// register order and in one run downwards from the last offset, and
    /// there can be only one stack adjustment. Giving up means
    /// `UNWIND_ARM64_MODE_DWARF`, which asks the linker for the frame table
    /// instead.
    fn macho_compact_unwind(&self, insns: &[cfi::Insn], canonical: bool) -> Option<u32> {
        use cfi::Insn;

        const MODE_FRAMELESS: u32 = 0x0200_0000;
        const MODE_DWARF: u32 = 0x0300_0000;
        const MODE_FRAME: u32 = 0x0400_0000;
        const FRAMELESS_STACK_MASK: u32 = 0x00ff_f000;
        // The frame pointer and the return address, as `.cfi_offset` names
        // them, and the first of the vector registers.
        const FP: u32 = 29;
        const LR: u32 = 30;
        const V0: u32 = 64;

        // `.cfi_signal_frame` is not an instruction, only a note on the
        // frame, and llvm-mc's list of instructions never holds one.
        let insns: Vec<&Insn> = insns.iter().filter(|i| **i != Insn::Mark).collect();
        if insns.is_empty() {
            return Some(MODE_FRAMELESS);
        }
        if !canonical {
            return Some(MODE_DWARF);
        }

        // A pair of saved registers, lowest first, and the bit it sets,
        // together with the bits that a later pair may not already have set:
        // the pairs have to come in this order.
        const PAIRS: [(u32, u32, u32); 9] = [
            (19, 0x0000_0001, 0xf1e),
            (21, 0x0000_0002, 0xf1c),
            (23, 0x0000_0004, 0xf18),
            (25, 0x0000_0008, 0xf10),
            (27, 0x0000_0010, 0xf00),
            (V0 + 8, 0x0000_0100, 0xe00),
            (V0 + 10, 0x0000_0200, 0xc00),
            (V0 + 12, 0x0000_0400, 0x800),
            (V0 + 14, 0x0000_0800, 0x000),
        ];

        let mut word = 0u32;
        let mut has_fp = false;
        let mut stack = 0u64;
        let mut offset = 0i64;
        let mut i = 0;
        while i < insns.len() {
            match *insns[i] {
                Insn::DefCfa(reg, _) => {
                    // Only a frame pointer; any other CFA register is one
                    // the word cannot name.
                    if reg != FP || i + 2 >= insns.len() {
                        return Some(MODE_DWARF);
                    }
                    let (Insn::Offset(lr, lr_at), Insn::Offset(fp, fp_at)) =
                        (insns[i + 1], insns[i + 2])
                    else {
                        return Some(MODE_DWARF);
                    };
                    if *fp_at + 8 != *lr_at || *lr != LR || *fp != FP {
                        return Some(MODE_DWARF);
                    }
                    offset = *fp_at;
                    word |= MODE_FRAME;
                    has_fp = true;
                    i += 3;
                }
                Insn::DefCfaOffset(by) => {
                    if stack != 0 {
                        return Some(MODE_DWARF);
                    }
                    stack = by.unsigned_abs();
                    i += 1;
                }
                // Registers are saved in pairs, each `.cfi_offset` eight
                // bytes below the one before it.
                Insn::Offset(first, first_at) => {
                    if i + 1 >= insns.len() || (offset != 0 && first_at != offset - 8) {
                        return Some(MODE_DWARF);
                    }
                    let Insn::Offset(second, second_at) = *insns[i + 1] else {
                        return Some(MODE_DWARF);
                    };
                    if second_at != first_at - 8 {
                        return Some(MODE_DWARF);
                    }
                    offset = second_at;
                    match PAIRS
                        .iter()
                        .find(|&&(lo, _, _)| first == lo && second == lo + 1)
                    {
                        Some(&(_, bit, after)) if word & after == 0 => word |= bit,
                        _ => return Some(MODE_DWARF),
                    }
                    i += 2;
                }
                _ => return Some(MODE_DWARF),
            }
        }
        if !has_fp {
            // The stack adjustment is counted in sixteen-byte units, and
            // twelve bits is as far as it reaches.
            if stack > 65520 {
                return Some(MODE_DWARF);
            }
            word |= MODE_FRAMELESS | (((stack / 16) as u32) << 12) & FRAMELESS_STACK_MASK;
        }
        Some(word)
    }

    /// `.ltorg` and `.pool` write the section's literal pool out here.
    ///
    /// `.tlsdesccall sym`, `.tlsdescadd sym` and `.tlsdescldr sym` put a
    /// relocation covering no bytes on whatever comes next, which in a TLS
    /// descriptor sequence is the `blr`, `add` or `ldr` a linker rewrites
    /// when it relaxes the sequence to another model. llvm-mc knows only the
    /// first. GNU as writes nothing at all for a value that is a number, and
    /// neither does this; llvm-mc refuses one.
    fn directive(
        &self,
        cx: &mut AsmCtx<'_>,
        name: &str,
        cur: &mut crate::cursor::Cursor<'_>,
    ) -> bool {
        let mark = match name {
            ".ltorg" | ".pool" => {
                cx.requests.push(crate::arch::Request::FlushLiterals);
                return true;
            }
            // `.arch_extension` takes one name, with `no` in front of it
            // to take the extension away; `.arch` and `.cpu` arrive through
            // `selects_cpu`, since a name either knows is one this backend
            // claims rather than another target.
            ".arch_extension" => {
                let span = cur.peek().span;
                let name = arch_word(cx, cur);
                if name.is_empty() {
                    cx.error(span, "`.arch_extension` expects a name");
                } else if let Err(msg) = cpu::directive_extension(cx.state, &name) {
                    cx.error(span, msg);
                }
                return true;
            }
            ".tlsdesccall" => reloc::TLSDESC_CALL,
            ".tlsdescadd" => reloc::TLSDESC_ADD,
            ".tlsdescldr" => reloc::TLSDESC_LDR,
            _ => return false,
        };
        let Some(expr) = cx.expr_parser().parse(cur) else {
            return true;
        };
        if cx.constant(expr).is_none() {
            cx.requests.push(crate::arch::Request::Mark {
                expr,
                kind: encode::fixup_tls_mark(mark),
                as_data: false,
                within: 0,
            });
        }
        true
    }

    /// A64 code is `$x`, and the literal pools and data in a code section
    /// are `$d`, as GNU as marks them.
    fn code_mapping(&self, _state: &ArchState) -> Option<(&'static str, u64)> {
        Some(("$x", 4))
    }

    /// GNU as's `aarch64_init_frag` marks an alignment fragment in a code
    /// section as instructions, not as data.
    fn align_padding_is_code(&self) -> bool {
        true
    }

    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len as usize);
        // Padding to a boundary finer than four bytes cannot be instructions,
        // so the leftover is zeroed rather than pretending otherwise.
        let words = (len / 4) as usize;
        for _ in 0..words {
            out.extend_from_slice(&NOP.to_le_bytes());
        }
        out.resize(len as usize, 0);
        out
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let mnemonic = cx.name(req.mnemonic).to_ascii_lowercase();
        // A mnemonic only the table has is the table's. One the handwritten
        // encoders also have is theirs until an operand is something only a
        // table form takes: `add x0, x1, x2` is handwritten, `add v0.8b,
        // v1.8b, v2.8b` and `add d0, d1, d2` are not, and `ldr d0, [x0]` is
        // handwritten again, since loads and stores take the scalar SIMD
        // registers themselves.
        // The one SME instruction beyond `smstart`/`smstop`, whose `{za}` the
        // operand grammar has no other use for.
        if mnemonic == "zero" {
            has_mnemonic(cx, req.mnemonic_span, &mnemonic).then_some(())?;
            return insn::sme_zero(cx, req);
        }
        // FEAT_MOPS, whose operands are an address written back with no
        // offset (`[x0]!`) and a register written back with no brackets
        // (`x2!`), neither of which the shared operand parser reads.
        if insn::is_mops(&mnemonic) {
            has_mnemonic(cx, req.mnemonic_span, &mnemonic).then_some(())?;
            return insn::mops_insn(cx, req, &mnemonic);
        }
        if table::knows(&mnemonic)
            && (!insn::handwritten(&mnemonic)
                || table::has_simd_operand(cx, req.operands, !insn::loads(&mnemonic)))
        {
            return table::assemble(cx, &mnemonic, req.mnemonic_span, req.operands);
        }
        if !insn::handwritten(&mnemonic) {
            cx.error(
                req.mnemonic_span,
                format!("unknown instruction `{mnemonic}`"),
            );
            return None;
        }
        has_mnemonic(cx, req.mnemonic_span, &mnemonic).then_some(())?;
        let cur = req.cursor();
        let ops = operand::parse_list(cx, &cur)?;
        insn::assemble(cx, req, &mnemonic, &ops)
    }
}

/// Whether the target has a mnemonic one of the hand-written encoders owns,
/// reporting what it needs if not: the feature set GNU as's opcode table
/// gives every row of that name. A form of the generated table carries a set
/// of its own, which `table::assemble` reads and which is finer, one form
/// there being one operand shape.
fn has_mnemonic(cx: &mut AsmCtx<'_>, span: crate::source::Span, mnemonic: &str) -> bool {
    let feats = cpu::mnemonic_feats(mnemonic);
    if cpu::supports(cx.state, feats) {
        return true;
    }
    let msg = cpu::unsupported(cx.state, mnemonic, feats);
    cx.error(span, msg);
    false
}

/// The name a target-selecting directive was given: whatever is written
/// without spaces, since `armv8.5-a+memtag` is not one identifier.
fn arch_word(cx: &AsmCtx<'_>, cur: &mut crate::cursor::Cursor<'_>) -> String {
    if cur.peek().is_eol() {
        return String::new();
    }
    let first = cur.peek();
    let mut last = cur.advance();
    while !cur.peek().is_eol() && !cur.peek().preceded_by_space {
        last = cur.advance();
    }
    cx.sources
        .span_text(first.span.to(last.span))
        .to_ascii_lowercase()
}

/// True if `name` is a register, so the generic parser does not treat a
/// register name as a symbol.
#[allow(dead_code)]
pub fn is_register(name: &str) -> bool {
    reg::is_register(name) || table::is_register(name)
}
