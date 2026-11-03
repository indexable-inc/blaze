//! Call frame information: the `.cfi_*` directives, and `.eh_frame` and
//! `.debug_frame` built from them.
//!
//! Each directive is kept as written, with its position, and turned into CFA
//! instructions only when the frame sections are written, because what it
//! becomes depends on the reference being followed:
//!
//! - GNU as puts a frame's initial instructions into the FDE, then moves every
//!   instruction before the first address advance into a CIE, sharing a CIE
//!   between FDEs whose leading instructions match. It writes each CIE just
//!   before the first FDE that needs it. llvm-mc puts only the target's
//!   initial instructions in a CIE, sorts the FDEs by the CIE they need, and
//!   gives `.debug_frame` a single CIE.
//! - `.cfi_adjust_cfa_offset` and `.cfi_rel_offset` count from the CFA offset
//!   at the directive. GNU as restores that offset at `.cfi_restore_state`;
//!   llvm-mc does not.
//! - GNU as writes a negative CFA offset in a `_sf` instruction; llvm-mc
//!   writes its two's complement as an unsigned number.
//! - `.debug_frame`'s CIE is version 1 in GNU as and follows the DWARF
//!   version in llvm-mc.

use super::emit::Blob;
use super::{Flavor, Pos};
use crate::assembler::Assembler;
use crate::cursor::Cursor;
use crate::expr::{BinOp, ExprKind, ExprRef};
use crate::lexer::{Punct, TokKind};
use crate::output::macho;
use crate::reloc::RelocClass;
use crate::section::{SectionFlags, SectionId};
use crate::source::Span;

const DW_CFA_ADVANCE_LOC: u8 = 0x40;
const DW_CFA_OFFSET: u8 = 0x80;
const DW_CFA_RESTORE: u8 = 0xc0;
const DW_CFA_ADVANCE_LOC1: u8 = 0x02;
const DW_CFA_ADVANCE_LOC2: u8 = 0x03;
const DW_CFA_ADVANCE_LOC4: u8 = 0x04;
const DW_CFA_OFFSET_EXTENDED: u8 = 0x05;
const DW_CFA_RESTORE_EXTENDED: u8 = 0x06;
const DW_CFA_UNDEFINED: u8 = 0x07;
const DW_CFA_SAME_VALUE: u8 = 0x08;
const DW_CFA_REGISTER: u8 = 0x09;
const DW_CFA_REMEMBER_STATE: u8 = 0x0a;
const DW_CFA_RESTORE_STATE: u8 = 0x0b;
const DW_CFA_DEF_CFA: u8 = 0x0c;
const DW_CFA_DEF_CFA_REGISTER: u8 = 0x0d;
const DW_CFA_DEF_CFA_OFFSET: u8 = 0x0e;
const DW_CFA_OFFSET_EXTENDED_SF: u8 = 0x11;
const DW_CFA_DEF_CFA_SF: u8 = 0x12;
const DW_CFA_DEF_CFA_OFFSET_SF: u8 = 0x13;
const DW_CFA_VAL_OFFSET: u8 = 0x14;
const DW_CFA_VAL_OFFSET_SF: u8 = 0x15;
const DW_CFA_GNU_WINDOW_SAVE: u8 = 0x2d;

const DW_EH_PE_OMIT: u8 = 0xff;
const DW_EH_PE_PCREL: u8 = 0x10;

/// A CFA instruction, or a directive that becomes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Insn {
    DefCfa(u32, i64),
    DefCfaRegister(u32),
    DefCfaOffset(i64),
    /// `.cfi_adjust_cfa_offset`: a `DefCfaOffset` relative to the current one.
    AdjustCfaOffset(i64),
    Offset(u32, i64),
    /// `.cfi_rel_offset`: an `Offset` relative to the CFA offset.
    RelOffset(u32, i64),
    ValOffset(u32, i64),
    Register(u32, u32),
    Restore(u32),
    Undefined(u32),
    SameValue(u32),
    RememberState,
    RestoreState,
    Escape(Vec<u8>),
    /// `DW_CFA_GNU_window_save`, which AArch64 reads as
    /// `DW_CFA_AARCH64_negate_ra_state`.
    WindowSave,
    /// No instruction: a directive GNU as still advances the location for.
    Mark,
    /// An address advance, between two positions; only in GNU as's lists.
    Advance(u64),
}

/// One `.cfi_startproc` to `.cfi_endproc`.
#[derive(Clone, Debug)]
pub struct Fde {
    pub start: Pos,
    pub end: Option<Pos>,
    pub span: Span,
    pub simple: bool,
    pub signal: bool,
    pub ra_column: Option<u32>,
    pub personality: Option<(u8, ExprRef)>,
    pub lsda: Option<(u8, ExprRef)>,
    pub insns: Vec<(Pos, Insn)>,
}

/// Frame state gathered while the source is read.
pub struct CfiState {
    pub fdes: Vec<Fde>,
    /// The frame between `.cfi_startproc` and `.cfi_endproc`, if any.
    pub open: Option<usize>,
    pub eh_frame: bool,
    pub debug_frame: bool,
}

impl Default for CfiState {
    fn default() -> CfiState {
        CfiState {
            fdes: Vec::new(),
            open: None,
            eh_frame: true,
            debug_frame: false,
        }
    }
}

/// A pointer's encoding and value, as a symbol, a symbol subtracted and an
/// addend.
type PointerKey = (
    u8,
    Option<crate::symbol::SymbolId>,
    Option<crate::symbol::SymbolId>,
    i64,
);

/// The size of a pointer in encoding `enc`.
fn encoding_size(enc: u8, ptr: u8) -> u8 {
    match enc & 7 {
        2 => 2,
        3 => 4,
        4 => 8,
        _ => ptr,
    }
}

impl Assembler {
    /// Runs a `.cfi_*` directive. Returns false for a name that is not one.
    pub(crate) fn dir_cfi(&mut self, name: &str, cur: &mut Cursor<'_>, span: Span) -> bool {
        if !name.starts_with(".cfi_") {
            return false;
        }
        if self.dwarf_target().cfi.is_none() {
            self.diags
                .error(span, "CFI is not supported for this target");
            cur.set_pos(cur.all().len());
            return true;
        }
        match name {
            ".cfi_sections" => self.cfi_sections(cur),
            ".cfi_startproc" => self.cfi_startproc(cur, span),
            ".cfi_endproc" => match self.dwarf.cfi.open.take() {
                Some(i) => {
                    let pos = self.dwarf_pos();
                    self.dwarf.cfi.fdes[i].end = Some(pos);
                }
                None => self
                    .diags
                    .error(span, ".cfi_endproc without corresponding .cfi_startproc"),
            },
            _ => {
                let Some(i) = self.dwarf.cfi.open else {
                    self.diags
                        .error(span, "CFI instruction used without previous .cfi_startproc");
                    cur.set_pos(cur.all().len());
                    return true;
                };
                if let Some(insn) = self.cfi_insn(name, cur, span, i) {
                    let pos = self.dwarf_pos();
                    self.dwarf.cfi.fdes[i].insns.push((pos, insn));
                }
            }
        }
        true
    }

    /// `.cfi_sections .eh_frame, .debug_frame`.
    fn cfi_sections(&mut self, cur: &mut Cursor<'_>) {
        let (mut eh, mut debug) = (false, false);
        while !cur.at_end() {
            let tok = cur.peek();
            let text = match tok.kind {
                TokKind::Ident(n) => self.interner.get(n).to_string(),
                TokKind::Str(i) => String::from_utf8_lossy(self.pool.get(i)).into_owned(),
                _ => {
                    self.diags.error(tok.span, "expected a section name");
                    cur.set_pos(cur.all().len());
                    return;
                }
            };
            cur.advance();
            match text.as_str() {
                ".eh_frame" => eh = true,
                ".debug_frame" => debug = true,
                _ => {
                    self.diags
                        .error(tok.span, format!("unknown CFI section `{text}`"));
                }
            }
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
        self.dwarf.cfi.eh_frame = eh;
        self.dwarf.cfi.debug_frame = debug;
    }

    fn cfi_startproc(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let mut simple = false;
        if let Some(n) = cur.peek().ident() {
            let tok = cur.advance();
            if self.interner.get(n) == "simple" {
                simple = true;
            } else {
                self.diags.error(
                    tok.span,
                    "expected `simple` or nothing after `.cfi_startproc`",
                );
            }
        }
        if self.dwarf.cfi.open.is_some() {
            self.diags
                .error(span, "previous CFI entry not closed (missing .cfi_endproc)");
            return;
        }
        let start = self.dwarf_pos();
        self.dwarf.cfi.fdes.push(Fde {
            start,
            end: None,
            span,
            simple,
            signal: false,
            ra_column: None,
            personality: None,
            lsda: None,
            insns: Vec::new(),
        });
        self.dwarf.cfi.open = Some(self.dwarf.cfi.fdes.len() - 1);
    }

    /// Parses one instruction directive. Directives that set a property of
    /// the frame rather than add an instruction return `None`, or `Mark`
    /// where GNU as still advances the location for them.
    fn cfi_insn(
        &mut self,
        name: &str,
        cur: &mut Cursor<'_>,
        span: Span,
        fde: usize,
    ) -> Option<Insn> {
        let reg_off = |asm: &mut Assembler, cur: &mut Cursor<'_>| -> Option<(u32, i64)> {
            let r = asm.cfi_register(cur)?;
            asm.cfi_comma(cur)?;
            let o = asm.cfi_const(cur)?;
            Some((r, o))
        };
        let insn = match name {
            ".cfi_def_cfa" => reg_off(self, cur).map(|(r, o)| Insn::DefCfa(r, o)),
            ".cfi_def_cfa_register" => self.cfi_register(cur).map(Insn::DefCfaRegister),
            ".cfi_def_cfa_offset" => self.cfi_const(cur).map(Insn::DefCfaOffset),
            ".cfi_adjust_cfa_offset" => self.cfi_const(cur).map(Insn::AdjustCfaOffset),
            ".cfi_offset" => reg_off(self, cur).map(|(r, o)| Insn::Offset(r, o)),
            ".cfi_rel_offset" => reg_off(self, cur).map(|(r, o)| Insn::RelOffset(r, o)),
            ".cfi_val_offset" => reg_off(self, cur).map(|(r, o)| Insn::ValOffset(r, o)),
            ".cfi_register" => {
                let a = self.cfi_register(cur)?;
                self.cfi_comma(cur)?;
                let b = self.cfi_register(cur)?;
                Some(Insn::Register(a, b))
            }
            // GNU as takes a list; each register is an instruction of its own.
            ".cfi_restore" | ".cfi_undefined" => {
                let mut regs = vec![self.cfi_register(cur)?];
                while cur.eat_punct(Punct::Comma).is_some() {
                    regs.push(self.cfi_register(cur)?);
                }
                let last = regs.pop()?;
                for r in regs {
                    let pos = self.dwarf_pos();
                    let insn = if name == ".cfi_restore" {
                        Insn::Restore(r)
                    } else {
                        Insn::Undefined(r)
                    };
                    self.dwarf.cfi.fdes[fde].insns.push((pos, insn));
                }
                Some(if name == ".cfi_restore" {
                    Insn::Restore(last)
                } else {
                    Insn::Undefined(last)
                })
            }
            ".cfi_same_value" => self.cfi_register(cur).map(Insn::SameValue),
            ".cfi_remember_state" => Some(Insn::RememberState),
            ".cfi_restore_state" => Some(Insn::RestoreState),
            ".cfi_window_save" | ".cfi_negate_ra_state" => Some(Insn::WindowSave),
            ".cfi_escape" => {
                let mut bytes = Vec::new();
                loop {
                    let v = self.cfi_const(cur)?;
                    if !(-128..=255).contains(&v) {
                        self.diags
                            .error(span, format!("value {v} does not fit in 1 byte(s)"));
                        return None;
                    }
                    bytes.push(v as u8);
                    if cur.eat_punct(Punct::Comma).is_none() {
                        break;
                    }
                }
                Some(Insn::Escape(bytes))
            }
            ".cfi_signal_frame" => {
                self.dwarf.cfi.fdes[fde].signal = true;
                Some(Insn::Mark)
            }
            ".cfi_return_column" => {
                let r = self.cfi_register(cur)?;
                self.dwarf.cfi.fdes[fde].ra_column = Some(r);
                None
            }
            ".cfi_personality" | ".cfi_lsda" => {
                let v = self.cfi_pointer(cur, name, span)?;
                let f = &mut self.dwarf.cfi.fdes[fde];
                if name == ".cfi_personality" {
                    f.personality = v;
                } else {
                    f.lsda = v;
                }
                None
            }
            _ => {
                self.diags
                    .error(span, format!("unsupported CFI directive `{name}`"));
                cur.set_pos(cur.all().len());
                None
            }
        };
        if insn.is_none() {
            // Whatever went wrong has been reported; the rest of the line
            // would only add noise.
            if !cur.at_end() && self.diags.has_errors() {
                cur.set_pos(cur.all().len());
            }
        }
        insn
    }

    fn cfi_comma(&mut self, cur: &mut Cursor<'_>) -> Option<()> {
        if cur.eat_punct(Punct::Comma).is_none() {
            let tok = cur.peek();
            self.diags.error(tok.span, "missing separator");
            cur.set_pos(cur.all().len());
            return None;
        }
        Some(())
    }

    fn cfi_const(&mut self, cur: &mut Cursor<'_>) -> Option<i64> {
        let e = self.parse_expr(cur)?;
        self.eval_absolute(e, "CFI value")
    }

    /// A register operand: a DWARF register number, or a name the backend
    /// knows, with whatever prefix its syntax gives registers.
    fn cfi_register(&mut self, cur: &mut Cursor<'_>) -> Option<u32> {
        let tok = cur.peek();
        let numeric = matches!(
            tok.kind,
            TokKind::Int(_) | TokKind::Punct(Punct::Minus | Punct::LParen | Punct::Tilde)
        );
        if numeric {
            let v = self.cfi_const(cur)?;
            if !(0..=u32::MAX as i64).contains(&v) {
                self.diags.error(tok.span, "bad register expression");
                return None;
            }
            return Some(v as u32);
        }
        // The register is everything up to the next comma, as written.
        let first = cur.pos();
        while !cur.at_end() && !cur.check_punct(Punct::Comma) {
            cur.advance();
        }
        let toks = &cur.all()[first..cur.pos()];
        let (Some(a), Some(b)) = (toks.first(), toks.last()) else {
            self.diags.error(tok.span, "bad register expression");
            return None;
        };
        let span = a.span.to(b.span);
        let text = self.sm.span_text(span).to_ascii_lowercase();
        match self
            .arch
            .dwarf_register(&self.arch_state, self.options.format, &text)
        {
            Some(r) => Some(r),
            None => {
                self.diags.error(span, "bad register expression");
                None
            }
        }
    }

    /// The operands of `.cfi_personality` and `.cfi_lsda`: an encoding, and
    /// unless that is `DW_EH_PE_omit`, the value.
    fn cfi_pointer(
        &mut self,
        cur: &mut Cursor<'_>,
        name: &str,
        span: Span,
    ) -> Option<Option<(u8, ExprRef)>> {
        let enc = self.cfi_const(cur)?;
        if enc == DW_EH_PE_OMIT as i64 {
            return Some(None);
        }
        let valid = (0..=0xff).contains(&enc)
            && matches!(enc & 0x0f, 0 | 2 | 3 | 4 | 0xa | 0xb | 0xc)
            && matches!(enc & 0x70, 0 | 0x10);
        if !valid {
            self.diags
                .error(span, format!("invalid or unsupported encoding in {name}"));
            cur.set_pos(cur.all().len());
            return None;
        }
        if cur.eat_punct(Punct::Comma).is_none() {
            self.diags.error(
                span,
                format!("{name} requires encoding and symbol arguments"),
            );
            cur.set_pos(cur.all().len());
            return None;
        }
        let e = self.parse_expr(cur)?;
        Some(Some((enc as u8, e)))
    }

    // ---- writing ----------------------------------------------------------

    pub(crate) fn emit_frames(&mut self) {
        if let Some(i) = self.dwarf.cfi.open.take() {
            let span = self.dwarf.cfi.fdes[i].span;
            self.diags.error(
                span,
                "open CFI at the end of file; missing .cfi_endproc directive",
            );
            let start = self.dwarf.cfi.fdes[i].start;
            self.dwarf.cfi.fdes[i].end = Some(start);
        }
        let target = self.dwarf_target();
        let Some(cfi) = target.cfi.clone() else {
            return;
        };
        let (eh, debug) = (self.dwarf.cfi.eh_frame, self.dwarf.cfi.debug_frame);
        let ptr = self.target().pointer_bytes(&self.target().initial_state());
        // Mach-O's compact unwind table, which stands where the frame table
        // would and comes before it in the object. Only `.eh_frame` has one:
        // it is the linker that reads it, and `.debug_frame` is for a
        // debugger.
        let mut in_eh_frame = vec![true; self.dwarf.cfi.fdes.len()];
        if eh && let Some((table, words)) = self.compact_unwind_words() {
            // A word of zero is the machine saying the frame is of a shape
            // the table has nothing to say about, as x86's is for a frame
            // with no directives at all: such a frame gets no entry, and a
            // table that would hold none is not written.
            if words.iter().any(|&w| w != 0) {
                self.emit_compact_unwind(&table, &words, ptr);
            }
            if table.without_eh_frame {
                for (keep, word) in in_eh_frame.iter_mut().zip(&words) {
                    *keep = *word == table.dwarf_only;
                }
            }
        }
        if eh && in_eh_frame.contains(&true) {
            match target.flavor {
                Flavor::Gnu => self.gnu_frames(&target, &cfi, true, ptr),
                Flavor::Llvm => self.llvm_frames(&target, &cfi, true, ptr, &in_eh_frame),
            }
        }
        if debug {
            // A compact word never keeps a frame out of `.debug_frame`.
            let all = vec![true; self.dwarf.cfi.fdes.len()];
            match target.flavor {
                Flavor::Gnu => self.gnu_frames(&target, &cfi, false, ptr),
                Flavor::Llvm => self.llvm_frames(&target, &cfi, false, ptr, &all),
            }
        }
    }

    /// The compact unwind word of every frame, where the object has a compact
    /// unwind table at all.
    ///
    /// A word of the table's `dwarf_only` is the machine's way of saying that
    /// this frame's shape is one no word can describe, and that the linker has
    /// to read the frame table for it. A word of zero says the frame has no
    /// entry in the compact table at all.
    fn compact_unwind_words(&self) -> Option<(macho::CompactUnwind, Vec<u32>)> {
        if self.options.format != crate::output::Format::MachO {
            return None;
        }
        let table = self.macho.deployment.compact_unwind?;
        let words = self
            .dwarf
            .cfi
            .fdes
            .iter()
            .map(|fde| {
                let insns: Vec<Insn> = fde.insns.iter().map(|(_, i)| i.clone()).collect();
                let canonical = self.canonical_personality(fde);
                self.target()
                    .macho_compact_unwind(&insns, canonical)
                    .unwrap_or(table.dwarf_only)
            })
            .collect();
        Some((table, words))
    }

    /// Whether a frame's personality routine is one of the two Darwin's
    /// compact unwind table keeps a slot for, a frame with none included.
    ///
    /// `MCAsmBackend::isDarwinCanonicalPersonality` leaves out
    /// `___gcc_personality_v0`, common as it is elsewhere, on the grounds
    /// that Darwin hardly uses it.
    fn canonical_personality(&self, fde: &Fde) -> bool {
        let Some((_, e)) = fde.personality else {
            return true;
        };
        let Some(sym) = self.eval_ref(e).ok().and_then(|v| v.plus) else {
            return true;
        };
        matches!(
            self.interner.get(self.symbols.get(sym).name),
            "___gxx_personality_v0" | "___objc_personality_v0"
        )
    }

    /// Writes `__LD,__compact_unwind`: for each frame its function, that
    /// function's length, the compact word, and the personality routine and
    /// language-specific data area the linker is to record with it.
    ///
    /// A frame left to the frame table still has an entry, saying so, but
    /// with neither pointer: the linker will read both out of the frame table
    /// instead.
    fn emit_compact_unwind(&mut self, table: &macho::CompactUnwind, words: &[u32], ptr: u8) {
        let endian = self.target().endian();
        let flags = SectionFlags {
            alloc: true,
            ..SectionFlags::default()
        };
        let sec = self.dwarf_section(".compact_unwind", flags, 0, ptr as u64);
        let mut b = Blob::new(endian);
        let fdes = std::mem::take(&mut self.dwarf.cfi.fdes);
        for (fde, &word) in fdes.iter().zip(words) {
            if word == 0 {
                continue;
            }
            let dwarf = word == table.dwarf_only;
            let begin = self.pos_expr(fde.start, 0);
            let kind = self.abs_kind(ptr);
            b.fixup(ptr, begin, kind);
            let start = self.pos_offset(fde.start);
            let end = fde.end.map_or(start, |p| self.pos_offset(p));
            b.int(end - start, 4);
            let lsda = fde.lsda.filter(|_| !dwarf);
            let has_lsda = if lsda.is_some() {
                macho::UNWIND_HAS_LSDA
            } else {
                0
            };
            b.int(u64::from(word | has_lsda), 4);
            match fde.personality.filter(|_| !dwarf) {
                Some((_, e)) => {
                    let kind = self.abs_kind(ptr);
                    b.fixup(ptr, e, kind);
                }
                None => b.int(0, ptr as usize),
            }
            // The data area's field is as wide as its encoding makes it,
            // which can leave the entry short of the thirty-two bytes the
            // linker reads; llvm-mc writes it that way too.
            let size = encoding_size(fde.lsda.map_or(0, |(enc, _)| enc), ptr);
            match lsda {
                Some((_, e)) => {
                    let kind = self.abs_kind(size);
                    b.fixup(size, e, kind);
                }
                None => b.int(0, size as usize),
            }
        }
        self.dwarf.cfi.fdes = fdes;
        self.push_blob(sec, b, Span::DUMMY);
    }

    /// The section a frame table goes into.
    fn frame_section(&mut self, eh: bool, align: u64) -> SectionId {
        if eh {
            let flags = SectionFlags {
                alloc: true,
                ..SectionFlags::default()
            };
            self.dwarf_section(".eh_frame", flags, 0, align)
        } else {
            self.dwarf_section(".debug_frame", SectionFlags::default(), 0, align)
        }
    }

    /// An address advance, in the target's instruction units.
    fn advance(b: &mut Blob, delta: u64, unit: u64) {
        let scaled = delta / unit.max(1);
        if scaled == 0 {
        } else if scaled < 0x40 {
            b.u8(DW_CFA_ADVANCE_LOC | scaled as u8);
        } else if scaled <= 0xff {
            b.u8(DW_CFA_ADVANCE_LOC1);
            b.u8(scaled as u8);
        } else if scaled <= 0xffff {
            b.u8(DW_CFA_ADVANCE_LOC2);
            b.int(scaled, 2);
        } else {
            b.u8(DW_CFA_ADVANCE_LOC4);
            b.int(scaled, 4);
        }
    }

    /// An address advance a relaxing linker is to work out, for a target that
    /// has a field for one; false where it has none, and the caller writes
    /// the distance as it stands.
    ///
    /// The form is still chosen by the distance as the file sees it, which is
    /// all either reference has to choose by, and the operand is left empty
    /// and relocated as the difference of labels at the two positions.
    fn deferred_advance(&mut self, b: &mut Blob, from: Pos, to: Pos, unit: u64) -> bool {
        let delta = self.pos_offset(to).saturating_sub(self.pos_offset(from));
        let scaled = delta / unit.max(1);
        let (opcode, size, bits) = if scaled < 0x40 {
            (DW_CFA_ADVANCE_LOC, 1u8, 6u8)
        } else if scaled <= 0xff {
            (DW_CFA_ADVANCE_LOC1, 1, 8)
        } else if scaled <= 0xffff {
            (DW_CFA_ADVANCE_LOC2, 2, 16)
        } else {
            (DW_CFA_ADVANCE_LOC4, 4, 32)
        };
        let Some(kind) = self.target().cfa_advance_field(size, bits) else {
            return false;
        };
        let e = self.difference_expr((from, 0), (to, 0));
        b.u8(opcode);
        if bits == 6 {
            b.fixup_written(1, e, kind);
        } else {
            b.fixup(size, e, kind);
        }
        true
    }

    /// `insn` with its registers numbered as `.debug_frame` numbers them;
    /// see [`crate::arch::Architecture::dwarf_debug_register`].
    ///
    /// Only the sections llvm-mc writes need this: the targets whose frames
    /// follow GNU as are all ELF ones, where the exception tables and the
    /// debugging ones number the registers alike.
    fn debug_frame_insn(&self, insn: &Insn) -> Insn {
        let map = |reg: u32| self.target().dwarf_debug_register(self.options.format, reg);
        match *insn {
            Insn::DefCfa(r, o) => Insn::DefCfa(map(r), o),
            Insn::DefCfaRegister(r) => Insn::DefCfaRegister(map(r)),
            Insn::Offset(r, o) => Insn::Offset(map(r), o),
            Insn::RelOffset(r, o) => Insn::RelOffset(map(r), o),
            Insn::ValOffset(r, o) => Insn::ValOffset(map(r), o),
            Insn::Register(a, b) => Insn::Register(map(a), map(b)),
            Insn::Restore(r) => Insn::Restore(map(r)),
            Insn::Undefined(r) => Insn::Undefined(map(r)),
            Insn::SameValue(r) => Insn::SameValue(map(r)),
            ref other => other.clone(),
        }
    }

    /// Writes an instruction whose relative forms have been resolved.
    fn write_insn(b: &mut Blob, insn: &Insn, flavor: Flavor, data_align: i64, unit: u64) {
        let factored = |o: i64| o / data_align;
        match *insn {
            Insn::Advance(delta) => Self::advance(b, delta, unit),
            Insn::DefCfa(r, o) => {
                if o < 0 && flavor == Flavor::Gnu {
                    b.u8(DW_CFA_DEF_CFA_SF);
                    b.uleb(r as u64);
                    b.sleb(factored(o));
                } else {
                    b.u8(DW_CFA_DEF_CFA);
                    b.uleb(r as u64);
                    b.uleb(o as u64);
                }
            }
            Insn::DefCfaRegister(r) => {
                b.u8(DW_CFA_DEF_CFA_REGISTER);
                b.uleb(r as u64);
            }
            Insn::DefCfaOffset(o) => {
                if o < 0 && flavor == Flavor::Gnu {
                    b.u8(DW_CFA_DEF_CFA_OFFSET_SF);
                    b.sleb(factored(o));
                } else {
                    b.u8(DW_CFA_DEF_CFA_OFFSET);
                    b.uleb(o as u64);
                }
            }
            Insn::Offset(r, o) => {
                let f = factored(o);
                if f < 0 {
                    b.u8(DW_CFA_OFFSET_EXTENDED_SF);
                    b.uleb(r as u64);
                    b.sleb(f);
                } else if r < 0x40 {
                    b.u8(DW_CFA_OFFSET | r as u8);
                    b.uleb(f as u64);
                } else {
                    b.u8(DW_CFA_OFFSET_EXTENDED);
                    b.uleb(r as u64);
                    b.uleb(f as u64);
                }
            }
            Insn::ValOffset(r, o) => {
                let f = factored(o);
                if f < 0 {
                    b.u8(DW_CFA_VAL_OFFSET_SF);
                    b.uleb(r as u64);
                    b.sleb(f);
                } else {
                    b.u8(DW_CFA_VAL_OFFSET);
                    b.uleb(r as u64);
                    b.uleb(f as u64);
                }
            }
            Insn::Register(r1, r2) => {
                b.u8(DW_CFA_REGISTER);
                b.uleb(r1 as u64);
                b.uleb(r2 as u64);
            }
            Insn::Restore(r) => {
                if r < 0x40 {
                    b.u8(DW_CFA_RESTORE | r as u8);
                } else {
                    b.u8(DW_CFA_RESTORE_EXTENDED);
                    b.uleb(r as u64);
                }
            }
            Insn::Undefined(r) => {
                b.u8(DW_CFA_UNDEFINED);
                b.uleb(r as u64);
            }
            Insn::SameValue(r) => {
                b.u8(DW_CFA_SAME_VALUE);
                b.uleb(r as u64);
            }
            Insn::RememberState => b.u8(DW_CFA_REMEMBER_STATE),
            Insn::RestoreState => b.u8(DW_CFA_RESTORE_STATE),
            Insn::WindowSave => b.u8(DW_CFA_GNU_WINDOW_SAVE),
            Insn::Escape(ref bytes) => b.bytes.extend_from_slice(bytes),
            Insn::AdjustCfaOffset(_) | Insn::RelOffset(..) | Insn::Mark => {}
        }
    }

    /// What the CIE's personality pointer relocates as. Darwin reaches the
    /// personality routine through the GOT whatever the encoding says, which
    /// is what `X86_64MCAsmInfoDarwin::getExprForPersonalitySymbol` writes;
    /// ELF and COFF relocate the symbol itself.
    fn personality_class(&self) -> RelocClass {
        if self.options.format == crate::output::Format::MachO {
            RelocClass::Got
        } else {
            RelocClass::Plain
        }
    }

    /// Writes the field of an FDE that holds the distance to a symbol: the
    /// function the frame describes, or its language-specific data area.
    ///
    /// Mach-O's arm64 relocations are all external, so a distance there is
    /// the difference of the symbol and the field — a `SUBTRACTOR` pair
    /// naming the field's own position — rather than one PC-relative entry.
    /// `MCAsmInfo::getExprForFDESymbol` subtracts in the same way, and
    /// x86-64 is left with a number the writer works out; see
    /// [`macho::Cpu::resolves_frame_address`].
    fn fde_pointer(&mut self, b: &mut Blob, at: Pos, enc: u8, e: ExprRef, ptr: u8) -> u8 {
        let relocated = self.options.format == crate::output::Format::MachO
            && enc & 0x70 == DW_EH_PE_PCREL
            && !macho::Cpu::for_arch(self.target()).is_some_and(macho::Cpu::resolves_frame_address);
        if !relocated {
            self.encoded_pointer(b, enc, e, ptr, RelocClass::FrameSymbol);
            return encoding_size(enc, ptr);
        }
        let size = encoding_size(enc, ptr);
        let here = self.pos_expr(at, b.len());
        let diff = self
            .exprs
            .alloc(ExprKind::Binary(BinOp::Sub, e, here), Span::DUMMY);
        let kind = self.abs_kind(size);
        b.fixup(size, diff, kind);
        size
    }

    /// Writes a pointer field in encoding `enc`, relocated against `e`.
    ///
    /// `class` says what the field is for where a format writes some of them
    /// differently; only an FDE's `initial_location` is such a field, and
    /// only in a Mach-O object (see [`RelocClass::FrameSymbol`]).
    fn encoded_pointer(&mut self, b: &mut Blob, enc: u8, e: ExprRef, ptr: u8, class: RelocClass) {
        let size = encoding_size(enc, ptr);
        // Darwin reaches the personality routine through the GOT relative to
        // the field whatever the encoding says, so a `Got` field is always a
        // distance.
        let kind = if enc & 0x70 == DW_EH_PE_PCREL || class == RelocClass::Got {
            self.pcrel_kind(size)
        } else {
            self.abs_kind(size)
        };
        b.fixup(size, e, kind.with_class(class));
    }

    // ---- GNU as ------------------------------------------------------------

    /// A frame's instructions as GNU as lists them: the initial instructions,
    /// then each directive, with an advance wherever the address moved, and
    /// the relative forms resolved against the CFA offset they were written
    /// at.
    fn gnu_insns(&self, fde: &Fde, cfi: &super::CfiTarget) -> Vec<Insn> {
        let mut out = Vec::new();
        let mut cfa = 0i64;
        let mut stack = Vec::new();
        if !fde.simple {
            for insn in &cfi.initial {
                if let Insn::DefCfa(_, o) = insn {
                    cfa = *o;
                }
                out.push(insn.clone());
            }
        }
        let mut last = self.pos_offset(fde.start);
        for (pos, insn) in &fde.insns {
            let addr = self.pos_offset(*pos);
            if addr != last {
                out.push(Insn::Advance(addr.saturating_sub(last)));
                last = addr;
            }
            let insn = match *insn {
                Insn::DefCfa(r, o) => {
                    cfa = o;
                    Insn::DefCfa(r, o)
                }
                Insn::DefCfaOffset(o) => {
                    cfa = o;
                    Insn::DefCfaOffset(o)
                }
                Insn::AdjustCfaOffset(o) => {
                    cfa += o;
                    Insn::DefCfaOffset(cfa)
                }
                Insn::RelOffset(r, o) => Insn::Offset(r, o - cfa),
                Insn::RememberState => {
                    stack.push(cfa);
                    Insn::RememberState
                }
                Insn::RestoreState => {
                    if let Some(c) = stack.pop() {
                        cfa = c;
                    }
                    Insn::RestoreState
                }
                Insn::Mark => continue,
                ref other => other.clone(),
            };
            out.push(insn);
        }
        out
    }

    fn gnu_frames(
        &mut self,
        target: &super::DwarfTarget,
        cfi: &super::CfiTarget,
        eh: bool,
        ptr: u8,
    ) {
        let endian = self.target().endian();
        let unit = target.min_insn_length as u64;
        let data_align = cfi.data_align as i64;
        let align = if eh { cfi.eh_frame_align } else { ptr as u64 };
        let sec = self.frame_section(eh, align);
        let base = self.next_pos(sec);
        let mut b = Blob::new(endian);
        let start_off = 0u64;

        struct Cie {
            ra: u32,
            signal: bool,
            per: Option<(u8, ExprRef)>,
            per_key: Option<PointerKey>,
            lsda: u8,
            insns: Vec<Insn>,
            offset: u64,
        }
        let mut cies: Vec<Cie> = Vec::new();
        let fdes = self.dwarf.cfi.fdes.clone();
        let count = fdes.len();
        for (n, fde) in fdes.iter().enumerate() {
            let insns = self.gnu_insns(fde, cfi);
            let ra = fde.ra_column.unwrap_or(cfi.ra_column);
            let (per, lsda) = if eh {
                (fde.personality, fde.lsda.map_or(DW_EH_PE_OMIT, |l| l.0))
            } else {
                (None, DW_EH_PE_OMIT)
            };
            let per_key = self.pointer_key(per);
            // The leading instructions a CIE can take (`initial_cie_insn`).
            let lead = insns
                .iter()
                .position(|i| !gnu_cie_insn(i))
                .unwrap_or(insns.len());
            // Any CIE whose instructions start the FDE's will do, the most
            // recently written first.
            let found = cies.iter().rposition(|c| {
                c.ra == ra
                    && c.signal == fde.signal
                    && c.lsda == lsda
                    && c.per_key == per_key
                    && c.insns.len() <= insns.len()
                    && c.insns[..] == insns[..c.insns.len()]
            });
            let (cie, first) = match found {
                Some(c) => (c, cies[c].insns.len()),
                None => {
                    let offset = start_off + b.len();
                    let cie = Cie {
                        ra,
                        signal: fde.signal,
                        per,
                        per_key,
                        lsda,
                        insns: insns[..lead].to_vec(),
                        offset,
                    };
                    self.gnu_cie(
                        &mut b, eh, &cie.insns, cie.ra, cie.signal, cie.per, lsda, cfi, target, ptr,
                    );
                    b.align(0, if eh { 4 } else { ptr as u64 }, 0);
                    cies.push(cie);
                    (cies.len() - 1, lead)
                }
            };

            // The FDE.
            let len_at = b.bytes.len();
            b.int(0, 4);
            let after_len = b.len();
            if eh {
                b.int(after_len - cies[cie].offset, 4);
            } else {
                let off = cies[cie].offset;
                self.dwarf_offset(&mut b, base, off);
            }
            let start = self.pos_offset(fde.start);
            let end = fde.end.map_or(start, |p| self.pos_offset(p));
            let begin = self.pos_expr(fde.start, 0);
            if eh {
                self.encoded_pointer(
                    &mut b,
                    cfi.fde_encoding,
                    begin,
                    ptr,
                    RelocClass::FrameSymbol,
                );
                let size = encoding_size(cfi.fde_encoding, ptr);
                b.int(end - start, size as usize);
                let lsize = fde
                    .lsda
                    .map_or(0, |(enc, _)| encoding_size(enc, ptr) as u64);
                b.uleb(lsize);
                if let Some((enc, e)) = fde.lsda {
                    self.encoded_pointer(&mut b, enc, e, ptr, RelocClass::FrameSymbol);
                }
            } else {
                let kind = self.abs_kind(ptr);
                b.fixup(ptr, begin, kind);
                b.int(end - start, ptr as usize);
            }
            for insn in &insns[first..] {
                Self::write_insn(&mut b, insn, Flavor::Gnu, data_align, unit);
            }
            let fde_align = match (eh, n + 1 == count) {
                (true, true) => cfi.eh_frame_align,
                (true, false) => 4,
                (false, _) => ptr as u64,
            };
            b.align(0, fde_align, 0);
            let len = b.len() - after_len;
            b.patch(len_at, len, 4);
        }
        self.push_blob(sec, b, Span::DUMMY);
    }

    /// What two personality pointers are compared by when GNU as decides
    /// whether FDEs can share a CIE: the encoding and the value, as a symbol
    /// and an addend.
    fn pointer_key(&mut self, p: Option<(u8, ExprRef)>) -> Option<PointerKey> {
        let (enc, e) = p?;
        let v = self.eval(e).ok()?;
        Some((enc, v.plus, v.minus, v.addend))
    }

    #[allow(clippy::too_many_arguments)]
    fn gnu_cie(
        &mut self,
        b: &mut Blob,
        eh: bool,
        insns: &[Insn],
        ra: u32,
        signal: bool,
        per: Option<(u8, ExprRef)>,
        lsda: u8,
        cfi: &super::CfiTarget,
        target: &super::DwarfTarget,
        ptr: u8,
    ) {
        let len_at = b.bytes.len();
        b.int(0, 4);
        let after_len = b.len();
        b.int(if eh { 0 } else { 0xffff_ffff }, 4);
        let version = if eh { cfi.cie_version } else { 1 };
        b.u8(version);
        if eh {
            b.u8(b'z');
            if per.is_some() {
                b.u8(b'P');
            }
            if lsda != DW_EH_PE_OMIT {
                b.u8(b'L');
            }
            b.u8(b'R');
        }
        if signal {
            b.u8(b'S');
        }
        b.u8(0);
        b.uleb(target.min_insn_length as u64);
        b.sleb(cfi.data_align as i64);
        if version == 1 {
            b.u8(ra as u8);
        } else {
            b.uleb(ra as u64);
        }
        if eh {
            let mut size = 1 + (lsda != DW_EH_PE_OMIT) as u64;
            if let Some((enc, _)) = per {
                size += 1 + encoding_size(enc, ptr) as u64;
            }
            b.uleb(size);
            if let Some((enc, e)) = per {
                b.u8(enc);
                let class = self.personality_class();
                self.encoded_pointer(b, enc, e, ptr, class);
            }
            if lsda != DW_EH_PE_OMIT {
                b.u8(lsda);
            }
            b.u8(cfi.fde_encoding);
        }
        for insn in insns {
            Self::write_insn(
                b,
                insn,
                Flavor::Gnu,
                cfi.data_align as i64,
                target.min_insn_length as u64,
            );
        }
        b.align(0, if eh { 4 } else { ptr as u64 }, 0);
        let len = b.len() - after_len;
        b.patch(len_at, len, 4);
    }

    // ---- llvm-mc -----------------------------------------------------------

    /// `keep` says which frames this table describes, which in a Mach-O
    /// object is only those the compact unwind table left to it.
    fn llvm_frames(
        &mut self,
        target: &super::DwarfTarget,
        cfi: &super::CfiTarget,
        eh: bool,
        ptr: u8,
        keep: &[bool],
    ) {
        let endian = self.target().endian();
        let unit = target.min_insn_length as u64;
        let data_align = cfi.data_align as i64;
        let align = if eh { cfi.eh_frame_align } else { ptr as u64 };
        let sec = self.frame_section(eh, align);
        let base = self.next_pos(sec);
        let mut b = Blob::new(endian);

        // FDEs grouped by the CIE they need, in `CIEKey` order.
        let mut fdes: Vec<Fde> = self
            .dwarf
            .cfi
            .fdes
            .iter()
            .zip(keep)
            .filter(|&(_, &k)| k)
            .map(|(f, _)| f.clone())
            .collect();
        let key = |asm: &Assembler, f: &Fde| {
            let per_name = f
                .personality
                .and_then(|(_, e)| asm.eval_ref(e).ok())
                .and_then(|v| v.plus)
                .map(|s| asm.interner.get(asm.symbols.get(s).name).to_string())
                .unwrap_or_default();
            (
                per_name,
                f.personality.map_or(0, |p| p.0),
                f.lsda.map_or(0, |l| l.0),
                f.signal,
                f.simple,
                f.ra_column.unwrap_or(cfi.ra_column),
            )
        };
        let mut keyed: Vec<_> = fdes.drain(..).map(|f| (key(self, &f), f)).collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));

        let version = self.dwarf_line_version();
        let cie_version = if eh {
            1
        } else {
            match version {
                2 => 1,
                3 => 3,
                _ => 4,
            }
        };
        let mut cies: Vec<(_, u64, i64)> = Vec::new();
        let mut cfa = 0i64;
        let count = keyed.len();
        for (n, (k, fde)) in keyed.iter().enumerate() {
            let found = if eh {
                cies.iter().find(|c| c.0 == *k).map(|c| (c.1, c.2))
            } else {
                cies.first().map(|c| (c.1, c.2))
            };
            let (cie_off, initial_cfa) = match found {
                Some(c) => c,
                None => {
                    let off = b.len();
                    let len_at = b.bytes.len();
                    b.int(0, 4);
                    let after = b.len();
                    b.int(if eh { 0 } else { 0xffff_ffff }, 4);
                    b.u8(cie_version);
                    if eh {
                        b.u8(b'z');
                        if fde.personality.is_some() {
                            b.u8(b'P');
                        }
                        if fde.lsda.is_some() {
                            b.u8(b'L');
                        }
                        b.u8(b'R');
                        if fde.signal {
                            b.u8(b'S');
                        }
                    }
                    b.u8(0);
                    if cie_version >= 4 {
                        b.u8(ptr);
                        b.u8(0);
                    }
                    b.uleb(target.min_insn_length as u64);
                    b.sleb(data_align);
                    let ra = fde.ra_column.unwrap_or(cfi.ra_column);
                    if cie_version == 1 {
                        b.u8(ra as u8);
                    } else {
                        b.uleb(ra as u64);
                    }
                    if eh {
                        let mut size = 1u64;
                        if let Some((enc, _)) = fde.personality {
                            size += 1 + encoding_size(enc, ptr) as u64;
                        }
                        if fde.lsda.is_some() {
                            size += 1;
                        }
                        b.uleb(size);
                        if let Some((enc, e)) = fde.personality {
                            b.u8(enc);
                            let class = self.personality_class();
                            self.encoded_pointer(&mut b, enc, e, ptr, class);
                        }
                        if let Some((enc, _)) = fde.lsda {
                            b.u8(enc);
                        }
                        b.u8(cfi.fde_encoding);
                    }
                    if !fde.simple {
                        for insn in &cfi.initial {
                            match insn {
                                Insn::DefCfa(_, o) | Insn::DefCfaOffset(o) => cfa = *o,
                                _ => {}
                            }
                            let insn = if eh {
                                insn.clone()
                            } else {
                                self.debug_frame_insn(insn)
                            };
                            Self::write_insn(&mut b, &insn, Flavor::Llvm, data_align, unit);
                        }
                    }
                    b.align(0, if eh { 4 } else { ptr as u64 }, 0);
                    let len = b.len() - after;
                    b.patch(len_at, len, 4);
                    cies.push((k.clone(), off, cfa));
                    (off, cfa)
                }
            };

            let len_at = b.bytes.len();
            b.int(0, 4);
            let after = b.len();
            if eh {
                b.int(after - cie_off, 4);
            } else {
                self.dwarf_offset(&mut b, base, cie_off);
            }
            let start = self.pos_offset(fde.start);
            let end = fde.end.map_or(start, |p| self.pos_offset(p));
            let begin = self.pos_expr(fde.start, 0);
            let size = if eh {
                self.fde_pointer(&mut b, base, cfi.fde_encoding, begin, ptr)
            } else {
                let kind = self.abs_kind(ptr);
                b.fixup(ptr, begin, kind);
                ptr
            };
            // How far the function reaches is a difference of two labels in
            // it, so a relaxing linker between them works it out, as it works
            // out the advances below.
            let end_pos = fde.end.unwrap_or(fde.start);
            if self.code_moves_between(fde.start.0, start, end) {
                let e = self.difference_expr((fde.start, 0), (end_pos, 0));
                let kind = self.abs_kind(size);
                b.fixup(size, e, kind);
            } else {
                b.int(end - start, size as usize);
            }
            if eh {
                let lsize = fde
                    .lsda
                    .map_or(0, |(enc, _)| encoding_size(enc, ptr) as u64);
                b.uleb(lsize);
                if let Some((enc, e)) = fde.lsda {
                    self.fde_pointer(&mut b, base, enc, e, ptr);
                }
            }
            cfa = initial_cfa;
            let mut last = start;
            let mut last_pos = fde.start;
            for (pos, insn) in &fde.insns {
                if *insn == Insn::Mark {
                    continue;
                }
                let addr = self.pos_offset(*pos);
                if !self.code_moves_between(pos.0, last, addr)
                    || !self.deferred_advance(&mut b, last_pos, *pos, unit)
                {
                    Self::advance(&mut b, addr.saturating_sub(last), unit);
                }
                last = addr;
                last_pos = *pos;
                let insn = match *insn {
                    Insn::DefCfa(r, o) => {
                        cfa = o;
                        Insn::DefCfa(r, o)
                    }
                    Insn::DefCfaOffset(o) => {
                        cfa = o;
                        Insn::DefCfaOffset(o)
                    }
                    Insn::AdjustCfaOffset(o) => {
                        cfa += o;
                        Insn::DefCfaOffset(cfa)
                    }
                    Insn::RelOffset(r, o) => Insn::Offset(r, o - cfa),
                    ref other => other.clone(),
                };
                let insn = if eh {
                    insn
                } else {
                    self.debug_frame_insn(&insn)
                };
                Self::write_insn(&mut b, &insn, Flavor::Llvm, data_align, unit);
            }
            // To the width of the address fields, and the last FDE to a
            // pointer's.
            let fde_align = if n + 1 < count {
                size as u64
            } else {
                ptr as u64
            };
            b.align(0, fde_align, 0);
            let len = b.len() - after;
            b.patch(len_at, len, 4);
        }
        self.push_blob(sec, b, Span::DUMMY);
    }
}

/// Whether GNU as lets an instruction into a CIE (`initial_cie_insn`): the
/// ones that set a rule, not an advance, an escape or a state change.
fn gnu_cie_insn(insn: &Insn) -> bool {
    matches!(
        insn,
        Insn::DefCfa(..)
            | Insn::DefCfaRegister(_)
            | Insn::DefCfaOffset(_)
            | Insn::Offset(..)
            | Insn::ValOffset(..)
            | Insn::Register(..)
            | Insn::Restore(_)
            | Insn::Undefined(_)
            | Insn::SameValue(_)
    )
}
