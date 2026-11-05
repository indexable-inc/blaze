//! MIPS, 32- and 64-bit, big and little endian. `EM_MIPS`.
//!
//! Four targets share one backend, differing only in byte order and register
//! width: `mips` and `mipsel` are 32-bit, `mips64` and `mips64el` 64-bit. The
//! instruction words are identical; only how they are laid down in memory and
//! which doubleword instructions are legal change.
//!
//! **Delay slots are the programmer's problem.** GNU as defaults to
//! `.set reorder`, in which the assembler may move an instruction into the
//! slot after a branch or insert a `nop` there. This backend behaves as
//! `.set noreorder` always: it emits exactly the instructions written, in the
//! order written, and never invents a `nop`. Multi-instruction *macros* such
//! as `li` and `la` are still expanded, since almost no real source avoids
//! them.

pub(crate) mod abi;
pub mod encode;
pub(crate) mod gp;
pub mod insn;
pub mod operand;
pub mod pseudo;
pub mod reg;
pub mod reloc;

use crate::arch::{ArchState, Architecture, AsmCtx, Endian, InsnRequest, Syntax};
use crate::cursor::Cursor;
use crate::dwarf::{CfiTarget, DwarfTarget, Flavor, cfi, numbered_register};
use crate::section::Variant;
use encode::Args;
use operand::{Operand, OperandParser};

pub const NAMES: &[&str] = &["mips", "mipsel", "mips64", "mips64el"];

/// `ArchState::features` bit: the source has said `.set noreorder`, which
/// the header records as `EF_MIPS_NOREORDER`.
const FEATURE_NOREORDER: u64 = 1;
/// `ArchState::features` bit: `.module fp=64`, a 64-bit floating-point file
/// on a 32-bit target.
pub(crate) const FEATURE_FP64: u64 = 2;
/// `ArchState::features` bit: `.module softfloat`, no floating-point unit.
pub(crate) const FEATURE_SOFTFLOAT: u64 = 4;
/// `ArchState::features` bit: `.module nooddspreg`, which gives up the odd
/// single-precision registers.
pub(crate) const FEATURE_NO_ODD_SPREG: u64 = 8;
/// `ArchState::features` bit: the file has said `.abicalls`, or the
/// `.option pic2` that means the same, so its code reaches everything
/// outside the object through the global offset table. It is what the
/// header's `EF_MIPS_PIC` records and what turns the `$gp` setup
/// directives on; see [`gp`].
pub(crate) const FEATURE_PIC: u64 = 16;

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let (canonical, endian, bits) = match name {
        "mips" | "mips32" => ("mips", Endian::Big, 32),
        "mipsel" | "mips32el" | "mipsle" => ("mipsel", Endian::Little, 32),
        "mips64" => ("mips64", Endian::Big, 64),
        "mips64el" | "mips64le" => ("mips64el", Endian::Little, 64),
        _ => return None,
    };
    Some(Box::new(Mips {
        name: canonical,
        endian,
        bits,
    }))
}

pub struct Mips {
    name: &'static str,
    endian: Endian,
    bits: u8,
}

impl Architecture for Mips {
    fn name(&self) -> &'static str {
        self.name
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["mips32", "mips32el", "mipsle", "mips64le"]
    }

    fn endian(&self) -> Endian {
        self.endian
    }

    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        self.bits / 8
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: self.bits,
            syntax: Syntax::Att,
            features: 0,
            cpu_features: [0; 3],
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    fn supports_syntax(&self, syntax: Syntax) -> bool {
        // MIPS assembly has only ever had one operand order.
        syntax == Syntax::Att
    }

    fn elf_machine(&self) -> u16 {
        8 // EM_MIPS
    }

    /// `.reginfo` (or, on n64, `.MIPS.options`) and `.MIPS.abiflags`, which
    /// llvm-mc writes into every MIPS object; see [`abi`].
    fn elf_attributes(&self, state: &ArchState) -> Vec<crate::arch::AttrSection> {
        abi::sections(self.bits, self.endian, state)
    }

    /// What llvm-mc writes for the default CPUs: MIPS32 with the o32 ABI and
    /// `EF_MIPS_CPIC`, or MIPS64 (n64 has no ABI bits) with `EF_MIPS_CPIC`,
    /// plus `EF_MIPS_NOREORDER` once the source has said `.set noreorder`
    /// and `EF_MIPS_PIC` once it has said `.abicalls`. Both references add
    /// the last two the same way, and both read the state at the end of the
    /// file, so an `.abicalls` taken back by a later `.option pic0` leaves
    /// `EF_MIPS_CPIC` alone.
    fn elf_flags(&self, state: &ArchState) -> u32 {
        let base = if self.bits == 64 {
            0x6000_0004
        } else {
            0x5000_1004
        };
        let noreorder = u32::from(state.features & FEATURE_NOREORDER != 0);
        let pic = 2 * u32::from(state.features & FEATURE_PIC != 0);
        base | noreorder | pic
    }

    fn align_is_log2(&self) -> bool {
        true
    }

    /// `R_MIPS_GOT16` against a local symbol names the GOT entry for the
    /// symbol's *page*, which a `%lo` of the same address completes, and
    /// BFD installs its `REL` addend the way it installs a `%hi`: shifted
    /// down by sixteen and biased by 0x8000, so that adding the
    /// sign-extended low half gets back to the address. Against a global
    /// symbol the entry is the symbol's own and the addend goes in plainly.
    /// Both references do this; it is `_bfd_mips_elf_got16_reloc` that
    /// decides which, by the same test as the `by_section` here.
    fn rel_field(&self, reloc: u32, addend: i64, _symbol_value: i64, by_section: bool) -> i64 {
        if reloc == reloc::GOT16 && by_section {
            addend.wrapping_add(0x8000) >> 16
        } else {
            addend
        }
    }

    /// llvm-mc, the reference, aligns `.text`, `.data` and `.bss` to 16
    /// bytes, as GNU as does for MIPS outside ELF; for ELF, GNU as aligns
    /// only `.text`, and to 4.
    fn section_align(
        &self,
        _state: &ArchState,
        name: &str,
        _flags: &crate::section::SectionFlags,
    ) -> u64 {
        match name {
            ".text" | ".data" | ".bss" => 16,
            _ => 1,
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

    /// llvm-mc's conventions, as for every MIPS encoding. Its FDE addresses
    /// are absolute, a pointer's width, where other targets measure them from
    /// the field.
    fn dwarf(&self, _state: &ArchState, _format: crate::output::Format) -> DwarfTarget {
        let wide = self.bits == 64;
        DwarfTarget {
            cfi: Some(CfiTarget {
                data_align: if wide { -8 } else { -4 },
                ra_column: 31,
                initial: vec![cfi::Insn::DefCfaRegister(29)],
                // DW_EH_PE_sdata8 or DW_EH_PE_sdata4.
                fde_encoding: if wide { 0x0c } else { 0x0b },
                eh_frame_align: if wide { 8 } else { 4 },
                cie_version: 1,
            }),
            private_prefix: if wide { ".L" } else { "$" },
            ..DwarfTarget::lines_only(Flavor::Llvm, 1)
        }
    }

    /// DWARF numbers MIPS registers by their number, so this is the `$`
    /// syntax's own lookup: `$31`, the O32 names, and for a 64-bit target
    /// the N64 names llvm-mc also takes (`$a4`-`$a7`, and `$t4`-`$t7` for
    /// 12-15 as well as `$t0`-`$t3`).
    fn dwarf_register(
        &self,
        _state: &ArchState,
        _format: crate::output::Format,
        name: &str,
    ) -> Option<u32> {
        let name = name.strip_prefix('$')?;
        if let Some(n) = numbered_register(name, "", 31) {
            return Some(n);
        }
        if self.bits == 64 {
            if let Some(n) = numbered_register(name, "a", 7).filter(|n| *n >= 4) {
                return Some(4 + n);
            }
            if let Some(n) = numbered_register(name, "t", 3) {
                return Some(12 + n);
            }
        }
        match name {
            "kt0" => Some(26),
            "kt1" => Some(27),
            _ => match reg::lookup(name) {
                Some(r) if r.is_gpr() => Some(r.num as u32),
                _ => None,
            },
        }
    }

    /// MIPS `nop` is `sll $zero, $zero, 0`, whose encoding is the all-zero
    /// word. Zero fill therefore *is* nop fill here, which is why this looks
    /// like the unimplemented default and is not.
    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        vec![0; len as usize]
    }

    /// `.set <option>`.
    ///
    /// rsasm assembles exactly what is written: it never moves an instruction
    /// into a delay slot and never inserts a `nop` after a branch. That is
    /// `.set noreorder`, so that option is accepted; all it changes is the
    /// header's `EF_MIPS_NOREORDER`, which both references set once a file
    /// has said it.
    ///
    /// `.set reorder` is refused rather than ignored, because the difference
    /// is not cosmetic. Under `reorder` the assembler fills the delay slot,
    /// so in `beq a, b, x` / `addiu t, t, 1` the `addiu` runs only when the
    /// branch falls through. Under `noreorder` that same `addiu` *is* the
    /// delay slot and runs on both paths. Accepting `reorder` while behaving
    /// as `noreorder` would assemble a different program without a word.
    fn directive(&self, cx: &mut AsmCtx<'_>, name: &str, cur: &mut Cursor<'_>) -> bool {
        if name == ".module" {
            self.module(cx, cur);
            return true;
        }
        // `.abicalls`, the `$gp` setup directives and the data directives
        // that need a relocation of their own; see [`gp`].
        if self.pic_directive(cx, name, cur, cur.peek().span) {
            return true;
        }
        if name != ".set" {
            return false;
        }
        let tok = cur.peek();
        let Some(n) = tok.ident() else {
            return false;
        };
        let opt = cx.name(n).to_ascii_lowercase();
        match opt.as_str() {
            "reorder" => {
                cur.advance();
                cx.error(
                    tok.span,
                    "`.set reorder` is not supported: rsasm never fills delay slots, so \
                     code written for it would run the instruction after each branch on \
                     both paths; fill the slots by hand and use `.set noreorder`",
                );
                true
            }
            // Options whose effect rsasm already has, or that only make an
            // assembler stricter about what it accepts and never change an
            // encoding, so accepting them silently is safe.
            // The object records that the file said it, and keeps the
            // record even if a `.set reorder` followed, as both references
            // do; rsasm refuses that one anyway.
            "noreorder" => {
                cur.advance();
                cx.state.features |= FEATURE_NOREORDER;
                true
            }
            "noat" | "at" | "nomacro" | "macro" | "push" | "pop" | "nomips16" | "nomicromips"
            | "mips1" | "mips2" | "mips3" | "mips4" | "mips5" | "mips32" | "mips32r2"
            | "mips32r6" | "mips64" | "mips64r2" | "mips64r6" | "hardfloat" | "softfloat"
            | "nodsp" | "oddspreg" | "nooddspreg" => {
                cur.advance();
                true
            }
            _ => false,
        }
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let mnemonic = cx.name(req.mnemonic).to_ascii_lowercase();

        let cur = req.cursor();
        let pieces = cur.split_commas();
        let mut ops: Vec<Operand> = Vec::with_capacity(pieces.len());
        for piece in &pieces {
            let mut p = OperandParser { cx };
            ops.push(p.parse_all(piece, req.span)?);
        }

        let args = Args {
            mnemonic: &mnemonic,
            ops: &ops,
            span: req.span,
        };

        // Macros are tried first: `b` and `move` are not real opcodes, so
        // there is nothing in the table for them to shadow.
        if pseudo::is_pseudo(&mnemonic) {
            return pseudo::expand(cx, &mnemonic, &args, self.endian, self.bits == 64)
                .map(|v| vec![v]);
        }

        let Some(def) = insn::lookup(&mnemonic) else {
            cx.error(
                req.mnemonic_span,
                format!("unknown instruction `{mnemonic}`"),
            );
            return None;
        };
        // Under `.abicalls` GNU as stops assembling these two as jumps: the
        // target is reached through the GOT, so `jal sym` becomes a load
        // and a `jalr`, and `j sym` becomes a branch. Assembling the direct
        // form instead would be a different program, and one GNU ld refuses
        // to put in a shared object, so rsasm refuses the spelling and
        // leaves the sequence to be written out. `jalx`, which changes ISA
        // mode, keeps its `R_MIPS_26` in both references.
        if matches!(mnemonic.as_str(), "j" | "jal") && cx.state.features & FEATURE_PIC != 0 {
            cx.error(
                req.mnemonic_span,
                format!(
                    "`{mnemonic}` of a symbol in position-independent code goes through \
                     the GOT, which rsasm does not expand; write the `lw` with `%call16` \
                     and the `jalr` yourself"
                ),
            );
            return None;
        }
        // Every MIPS instruction is one word wide, so there is never more than
        // one candidate for the layout pass to choose between.
        encode::encode(cx, &def, &args, self.endian, self.bits == 64).map(|v| vec![v])
    }
}

impl Mips {
    /// `.module <option>`, which says what the *file* needs of a processor
    /// rather than what one instruction does, and so lands in
    /// `.MIPS.abiflags`; see [`abi`].
    ///
    /// The options both references take to the same effect are here:
    /// `fp=32` and `fp=64` for the width of the floating-point file,
    /// `softfloat` and `hardfloat`, and `oddspreg` and `nooddspreg` for the
    /// odd single-precision registers. The rest — the ISA names, the
    /// application-specific extensions — would change which instructions are
    /// accepted, which this backend does not vary, and are refused rather
    /// than ignored.
    fn module(&self, cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>) {
        let tok = cur.peek();
        let Some(n) = tok.ident() else {
            cx.error(tok.span, "`.module` needs an option");
            cur.set_pos(cur.all().len());
            return;
        };
        let mut word = cx.name(n).to_ascii_lowercase();
        cur.advance();
        // `fp=32` lexes as `fp`, `=`, `32`.
        if word == "fp" && cur.peek().is_punct(crate::lexer::Punct::Eq) {
            cur.advance();
            if let crate::lexer::TokKind::Int(v) = cur.peek().kind {
                cur.advance();
                word = format!("fp={v}");
            }
        }
        let wide = self.bits == 64;
        match word.as_str() {
            // A 64-bit target's floating-point file is 64 bits already, and
            // neither reference lets it be anything else.
            "fp=64" if wide => {}
            "fp=32" if wide => cx.error(
                tok.span,
                "`.module fp=32` is not allowed on a 64-bit MIPS target",
            ),
            "fp=32" => cx.state.features &= !FEATURE_FP64,
            "fp=64" => cx.state.features |= FEATURE_FP64,
            "softfloat" => cx.state.features |= FEATURE_SOFTFLOAT,
            "hardfloat" => cx.state.features &= !FEATURE_SOFTFLOAT,
            "oddspreg" if wide => {}
            "nooddspreg" if wide => cx.error(
                tok.span,
                "`.module nooddspreg` is not allowed on a 64-bit MIPS target",
            ),
            "oddspreg" => cx.state.features &= !FEATURE_NO_ODD_SPREG,
            "nooddspreg" => cx.state.features |= FEATURE_NO_ODD_SPREG,
            _ => cx.error(
                tok.span,
                format!(
                    "`.module {word}` is not an option rsasm understands: it takes \
                     `fp=32`, `fp=64`, `softfloat`, `hardfloat`, `oddspreg` and \
                     `nooddspreg`, which are what `.MIPS.abiflags` records"
                ),
            ),
        }
        cur.set_pos(cur.all().len());
    }
}
