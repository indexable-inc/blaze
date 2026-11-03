//! NEC/Renesas V850, and its successor RH850 (the V850E3V5 instruction set).
//!
//! Two targets share one backend:
//!
//! - `v850` is the original V850 instruction set and nothing more, which is
//!   what `v850-elf-as` assembles when given no `-m` option. The V850E and
//!   V850E2 additions (`callt`, `prepare`, `switch`, the 32-bit `mul`, ...)
//!   are refused, with a note that RH850 has them.
//! - `rh850` is GNU as's `-mv850e3v5`: everything V850E1, V850E2 and V850E2V3
//!   added, the RH850 instructions (`bins`, `rotl`, `loop`, `pushsp`, the
//!   17-bit branches, the 48-bit loads and stores), and the FPU-3
//!   floating-point set. `v850e3v5` and `v850e2v4` name it too.
//!
//! The syntax is GNU as's, which is what could be checked; see
//! `tools/xas-diff`. With `-d ccrh`, source written for Renesas's own CC-RH is
//! accepted too, following its manual: label-reference sigils, its
//! condition-suffixed mnemonics, and the instruction expansions that make its
//! `mov 0x10, r10` a `movea`. See [`ccrh`], whose output is checked against
//! the GNU-syntax sequence each expansion stands for.
//!
//! Instructions are 16, 32 or 48 bits, little-endian, and GNU as's choices
//! between forms are followed exactly, because they are what existing object
//! code contains: `mov 16, r1` takes 48 bits on RH850 because 16 does not fit
//! the 5-bit form, and a conditional branch grows from 2 to 4 to 6 bytes as
//! its target moves away. Objects use the RH850 ELF ABI, as GNU as's do by
//! default; see [`reloc`].

pub mod branch;
pub mod ccrh;
pub mod encode;
pub mod fpu;
pub mod insn;
pub mod operand;
pub mod reg;
pub mod reloc;

use crate::arch::{ArchState, Architecture, AsmCtx, CommentSyntax, Endian, InsnRequest, Syntax};
use crate::cursor::Cursor;
use crate::section::Variant;
use encode::Miss;

pub const NAMES: &[&str] = &["v850", "rh850"];

/// `ArchState::features` bit selecting the RH850 instruction set.
pub const FEATURE_RH850: u64 = 1;

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let (canonical, rh850) = match name {
        "v850" => ("v850", false),
        "rh850" | "v850e3v5" | "v850e2v4" => ("rh850", true),
        _ => return None,
    };
    Some(Box::new(V850 {
        name: canonical,
        rh850,
    }))
}

pub struct V850 {
    name: &'static str,
    rh850: bool,
}

impl Architecture for V850 {
    fn name(&self) -> &'static str {
        self.name
    }

    fn aliases(&self) -> &'static [&'static str] {
        if self.rh850 {
            &["v850e3v5", "v850e2v4"]
        } else {
            &[]
        }
    }

    fn endian(&self) -> Endian {
        Endian::Little
    }

    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        4
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: 32,
            syntax: Syntax::Att,
            features: if self.rh850 { FEATURE_RH850 } else { 0 },
            cpu_features: [0; 3],
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    fn supports_syntax(&self, syntax: Syntax) -> bool {
        syntax == Syntax::Att
    }

    /// `EM_V800`, not `EM_V850` (87).
    ///
    /// GNU as 2.47 marks both V850 and RH850 objects as `EM_V800` with the
    /// RH850 ABI unless told `-mgcc-abi`, and the relocation numbers in
    /// [`reloc`] are that ABI's. The two go together: GNU ld picks the
    /// relocation table by machine, so an `EM_V850` object with these numbers
    /// would be misread.
    fn elf_machine(&self) -> u16 {
        36
    }

    /// GNU as measures a branch it leaves to the linker from the start of
    /// the section when the target is a global symbol in the branch's own
    /// section, and writes that into the displacement anyway. To a weak one
    /// it leaves zero, as to a symbol elsewhere.
    fn relocated_pcrel_field(&self, binding: crate::symbol::Binding, pc: u64) -> Option<i64> {
        (binding != crate::symbol::Binding::Weak).then_some(-(pc as i64))
    }

    /// `EF_V850_RH850_ABI` (the top nibble GNU as writes for both CPU
    /// names), plus `EF_RH850_V3` (0x0010_0000) when the file ends in RH850
    /// mode, whether that came from the target name or `.v850e3v5`.
    fn elf_flags(&self, state: &ArchState) -> u32 {
        if state.features & FEATURE_RH850 != 0 {
            0xf010_0000
        } else {
            0xf000_0000
        }
    }

    /// `.note.renesas`, which GNU as writes into every V850 object: six
    /// notes naming what a linker may put this object together with.
    ///
    /// Each is a note with the name `REL` and a one-word descriptor, in the
    /// order `bfd/elf32-v850.c` numbers them: the data alignment (1, four
    /// bytes, which is `v850-elf-as`'s default), the size of a `double` (2,
    /// eight bytes), the floating-point unit, and then the SIMD, cache and
    /// MMU settings, which GNU as leaves at zero because it has no option
    /// that sets them. The unit is FPU-3 on RH850 (`-mv850e3v5`) and none on
    /// the plain V850, which has no floating point at all.
    ///
    /// The section is `SHT_LOUSER + 0x2000_0000`, mergeable with a 20-byte
    /// entry size, so that a link keeps one copy of each note.
    fn elf_attributes(&self, state: &ArchState) -> Vec<crate::arch::AttrSection> {
        let fpu = if state.features & FEATURE_RH850 != 0 {
            2
        } else {
            0
        };
        let mut bytes = Vec::with_capacity(120);
        for (kind, desc) in [(1u32, 1u32), (2, 2), (3, fpu), (4, 0), (5, 0), (6, 0)] {
            bytes.extend_from_slice(&4u32.to_le_bytes()); // n_namesz
            bytes.extend_from_slice(&4u32.to_le_bytes()); // n_descsz
            bytes.extend_from_slice(&kind.to_le_bytes()); // n_type
            bytes.extend_from_slice(b"REL\0");
            bytes.extend_from_slice(&desc.to_le_bytes());
        }
        vec![crate::arch::AttrSection {
            name: ".note.renesas",
            sh_type: 0xa000_0000,
            sh_flags: 0x10, // SHF_MERGE
            align: 4,
            entsize: 20,
            body: crate::arch::AttrBody::Bytes(bytes),
        }]
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

    /// `#` starts a comment anywhere; `//` does not, as in GNU as for V850.
    fn comments(&self) -> CommentSyntax {
        CommentSyntax {
            anywhere: &["#"],
            line_start: &[],
        }
    }

    /// GNU as for V850 makes `.word` 32 bits.
    fn word_bytes(&self) -> u8 {
        4
    }

    /// The V850 `nop` is `mov r0, r0`, whose encoding is two zero bytes, so
    /// zero fill is nop fill. Every instruction is an even number of bytes,
    /// so an odd count can only follow data, where one zero byte is as good
    /// as any.
    /// GNU as's conventions, as for every V850 encoding: code counted in
    /// words, and no call frame information.
    fn dwarf(
        &self,
        _state: &ArchState,
        _format: crate::output::Format,
    ) -> crate::dwarf::DwarfTarget {
        crate::dwarf::DwarfTarget::lines_only(crate::dwarf::Flavor::Gnu, 2)
    }

    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        vec![0; len as usize]
    }

    /// `.v850` and `.v850e3v5` (or `.v850e2v4`), which switch the instruction
    /// set the way GNU as's pseudo-ops of those names do. The intermediate
    /// cores are refused rather than mapped to either, since rsasm has no
    /// instruction set that matches them.
    fn directive(&self, cx: &mut AsmCtx<'_>, name: &str, cur: &mut Cursor<'_>) -> bool {
        match name {
            // CC-RH's `$NOMACRO` and `$MACRO` turn its instruction expansions
            // off and on (R20UT3516EJ0113 pages 471-472).
            "$nomacro" if cx.dialect == crate::lexer::Dialect::CcRh => {
                cx.state.features |= ccrh::FEATURE_NOMACRO
            }
            "$macro" if cx.dialect == crate::lexer::Dialect::CcRh => {
                cx.state.features &= !ccrh::FEATURE_NOMACRO
            }
            ".v850" => cx.state.features &= !FEATURE_RH850,
            ".v850e3v5" | ".v850e2v4" => cx.state.features |= FEATURE_RH850,
            ".v850e" | ".v850e1" | ".v850e2" | ".v850e2v3" => {
                let span = cur.remaining_span();
                cx.error(
                    span,
                    format!(
                        "`{name}` is not supported; rsasm assembles either the V850 \
                         (`.v850`) or the RH850 (`.v850e3v5`) instruction set"
                    ),
                );
            }
            _ => return false,
        }
        true
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let mnemonic = cx.name(req.mnemonic).to_ascii_lowercase();
        let rh850 = cx.state.features & FEATURE_RH850 != 0;
        if cx.dialect == crate::lexer::Dialect::CcRh {
            return ccrh::assemble(cx, req, &mnemonic, rh850);
        }
        let ranges = matches!(mnemonic.as_str(), "pushsp" | "popsp" | "dbpush");
        let args = operand::parse_operands(cx, req.operands, req.span, ranges)?;
        encode_insn(
            cx,
            &mnemonic,
            &args,
            req.span,
            req.mnemonic_span,
            rh850,
            |_| true,
        )
    }
}

/// Encodes one instruction from parsed operands, trying only the table
/// entries `allow` accepts: all of them for GNU syntax, and a chosen width
/// for CC-RH's `mov32` or `ld23.w`.
pub(crate) fn encode_insn(
    cx: &mut AsmCtx<'_>,
    mnemonic: &str,
    args: &[operand::Arg],
    span: crate::source::Span,
    mnemonic_span: crate::source::Span,
    rh850: bool,
    allow: impl Fn(&insn::Entry) -> bool,
) -> Option<Vec<Variant>> {
    if let Some(cc) = branch::condition(mnemonic) {
        return branch::bcond(cx, mnemonic, cc, args, span, rh850);
    }
    if mnemonic == "loop" {
        if !rh850 {
            cx.error(mnemonic_span, needs_rh850("loop"));
            return None;
        }
        return branch::loop_insn(cx, args, span);
    }

    let mut best: Option<Miss> = None;
    let mut any_entry = false;
    let mut any_here = false;
    for e in insn::entries(mnemonic).filter(|e| allow(e)) {
        any_entry = true;
        if !e.cpu.allows(rh850) {
            continue;
        }
        any_here = true;
        match encode::match_entry(cx, e, args, rh850, span) {
            Ok(enc) => return Some(vec![enc.finish()]),
            // On a tie the later entry's complaint wins: later entries
            // are the wider forms, whose limits are the real ones.
            Err(m) => {
                if best.as_ref().is_none_or(|b| m.progress >= b.progress) {
                    best = Some(m);
                }
            }
        }
    }

    if !any_entry {
        cx.error(mnemonic_span, format!("unknown instruction `{mnemonic}`"));
        return None;
    }
    // Say so when the statement is fine on RH850, rather than reporting
    // why the V850 forms did not fit.
    if !rh850
        && insn::entries(mnemonic)
            .filter(|e| allow(e) && e.cpu.allows(true))
            .any(|e| encode::match_entry(cx, e, args, true, span).is_ok())
    {
        let msg = if any_here {
            format!("this form of `{mnemonic}` needs the RH850 instruction set (`--arch rh850`)")
        } else {
            needs_rh850(mnemonic)
        };
        cx.error(span, msg);
        return None;
    }
    match best {
        Some(m) => cx.error(m.span, m.msg),
        None => cx.error(mnemonic_span, needs_rh850(mnemonic)),
    }
    None
}

fn needs_rh850(mnemonic: &str) -> String {
    format!("`{mnemonic}` is an RH850 instruction; it needs `--arch rh850`, not `v850`")
}
