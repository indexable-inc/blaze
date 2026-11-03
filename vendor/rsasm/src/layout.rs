//! Layout, relaxation and fixup resolution.
//!
//! Fragment sizes and symbol addresses depend on each other: an alignment's
//! padding depends on where it lands, and where it lands depends on how long
//! the branches before it turned out to be. The pass below iterates to a fixed
//! point. Instruction sizes normally only grow — `chosen` never decreases — so
//! the branch half of the loop always terminates. A backend can let sizes
//! shrink again too (RX does, as GNU as does there), under GNU as's limit on
//! how often one fragment may flip, or have them picked afresh each pass
//! until one grows where nothing before it did (ARM, likewise). The whole
//! loop is bounded as well, since a
//! `.org` or `.space` whose size depends on a later symbol can be written to
//! oscillate.

use crate::arch::{FlatModifier, Interwork, InterworkTarget, Relaxation};
use crate::assembler::{Assembler, Relocation};
use crate::expr::{self, EvalError, ExprKind, ExprRef, Value};
use crate::intern::Name;
use crate::lexer::LocalDir;
use crate::reloc::RelocDesc;
use crate::section::{
    FixupKind, FragKind, Fragment, LinkValue, RelocSymbol, SectionFlags, SectionId, SectionKind,
};
use crate::source::Span;
use crate::symbol::{Binding, SymbolId, SymbolValue, Visibility};
use std::collections::HashMap;

/// Enough passes for any realistic file; hitting the limit means the input is
/// self-referential in a way that cannot settle.
const MAX_PASSES: u32 = 32;

/// The pass limit where sizes may also shrink. A fragment can then flip
/// between two sizes around an alignment, and GNU as's guard only stops it
/// after ten shrinks and ten growths, so settling legitimately takes more
/// passes than growth alone ever does; the guard is what bounds it.
const MAX_PASSES_SHRINKING: u32 = 1000;

/// What a fragment's size depends on, extracted so the size computation can
/// call back into the assembler without holding a borrow on the fragment.
enum Task {
    Fixed(u64),
    Align {
        align: u64,
        max_skip: Option<u64>,
    },
    Org {
        target: ExprRef,
        span: Span,
    },
    Space {
        size: ExprRef,
        span: Span,
    },
    Leb {
        value: ExprRef,
        signed: bool,
        span: Span,
    },
}

impl Assembler {
    /// Resolves everything and prepares the sections for output. Returns false
    /// if errors were reported.
    pub fn finish(&mut self) -> bool {
        self.flush_all_literals();
        self.bind_section_names();
        self.report_undefined_locals();
        self.check_cc_bare_labels();
        // The CodeView string table is placed where `.cv_stringtable` asked
        // for it and filled in here, since a `.cv_string` after that
        // directive still enters it.
        self.finish_codeview();
        self.pad_section_tails();
        self.add_attributes_section();
        // Which references a Mach-O object resolves depends on where its
        // atoms start, which is settled once every label has been read.
        if self.macho_object() {
            self.macho_bind_indirect_symbols();
            self.macho.atoms = crate::output::macho::atoms(self);
            if let Some(open) = self.macho.data_regions.iter().find(|r| r.end.is_none()) {
                let span = open.span;
                self.diags.error(
                    span,
                    "`.data_region` is never ended with `.end_data_region`",
                );
            }
        }

        if !self.settle_layout() {
            return false;
        }
        // DWARF is written from the settled layout, into sections of its own
        // that nothing in the code refers to, so the layout of the code
        // cannot change when it runs again to place them. So are a target's
        // records of where the code was padded, and the unwind data a COFF
        // object's `.seh_*` directives describe, which counts the bytes of a
        // prologue.
        let dwarf = self.emit_dwarf();
        if (self.emit_layout_records() || dwarf) && !self.settle_layout() {
            return false;
        }
        if self.emit_coff_unwind() && !self.settle_layout() {
            return false;
        }

        self.assign_addresses();
        self.report_misaligned_data();
        self.report_uncontained_marks();
        self.apply_fixups();
        // After the fixups, which intern the symbols they name, so that these
        // follow them in the symbol table as they do in GNU as's.
        self.add_section_symbols();
        self.place_mapping_symbols();
        // Data references only enter the symbol table when their fixups are
        // built, so NASM's "symbol not defined" check runs after that.
        if self.options.dialect == crate::lexer::Dialect::Nasm {
            self.nasm_report_undefined();
        }
        self.materialize();
        !self.diags.has_errors()
    }

    /// Makes a name that no symbol is defined by, but a section is, stand for
    /// that section: `.long .debug_abbrev` is a reference to the section
    /// symbol, in GNU as and llvm-mc alike, however far ahead of the section
    /// it is written. Clang's DWARF refers to its sections this way. A name
    /// declared global or weak stays the linker's to find.
    fn bind_section_names(&mut self) {
        let sections: HashMap<Name, SectionId> =
            self.sections.iter().map(|s| (s.name, s.id)).collect();
        // A name only reaches the symbol table once something evaluates it,
        // which for most references is after this; so the expressions are
        // what say which section names are used.
        let mut used: Vec<(Name, SectionId, Span)> = Vec::new();
        for node in &self.exprs.nodes {
            if let ExprKind::Sym(n) = node.kind
                && let Some(&section) = sections.get(&n)
                && !used.iter().any(|(m, ..)| *m == n)
            {
                used.push((n, section, node.span));
            }
        }
        for (name, section, span) in used {
            let id = self.symbols.intern(name, span);
            let sym = self.symbols.get(id);
            if sym.is_defined() || sym.binding != Binding::Local {
                continue;
            }
            let sym = self.symbols.get_mut(id);
            sym.value = SymbolValue::Label { section, frag: 0 };
            sym.ty = crate::symbol::SymType::Section;
            if self.section(section).sym.is_none() {
                self.section_mut(section).sym = Some(id);
            }
        }
    }

    /// Runs layout to a fixed point. Returns false, after reporting it, if it
    /// does not settle.
    fn settle_layout(&mut self) -> bool {
        let mut settled = false;
        let mut history = HashMap::new();
        let limit = if self.relaxation() >= Relaxation::EachPass {
            MAX_PASSES_SHRINKING
        } else {
            MAX_PASSES
        };
        for _ in 0..limit {
            // Addresses are assigned from the previous pass's sizes before
            // this pass computes new ones. For relocatable output every
            // section sits at zero and this changes nothing; for a flat image
            // it is what lets `.space start + 4 - 0x8000`, or a zero-page
            // choice on the 6502, see where a label really is rather than its
            // offset within the section. The loop only ends on a pass where no
            // size changed, and addresses are a function of sizes, so the
            // addresses that pass used are the final ones.
            self.assign_addresses();
            let sizes_changed = self.assign_offsets();
            let relaxed = self.relax(&mut history);
            if !sizes_changed && !relaxed {
                settled = true;
                break;
            }
        }
        if !settled {
            self.diags.error(
                Span::DUMMY,
                "could not settle section layout; a `.org`, `.space` or `.align` \
                 probably depends on a symbol that it also moves",
            );
            return false;
        }
        self.note_moving_code();
        true
    }

    /// Records where a relaxing linker may delete bytes, now that the
    /// offsets are settled; see [`Assembler::code_moves_between`].
    ///
    /// Nothing is recorded for a target whose linker shortens nothing, nor
    /// for a flat image, which has no linker, so the scan costs those
    /// nothing beyond the walk itself.
    fn note_moving_code(&mut self) {
        self.moving_code.clear();
        self.moving_code.resize(self.sections.len(), Vec::new());
        self.aligning_code.clear();
        self.aligning_code.resize(self.sections.len(), Vec::new());
        if !self.options.relocatable {
            return;
        }
        for si in 0..self.sections.len() {
            let mut at = Vec::new();
            let mut aligns = Vec::new();
            for fi in 0..self.sections[si].frags.len() {
                let arch = self.frag_arch(si, fi).0;
                let frag = &self.sections[si].frags[fi];
                match &frag.kind {
                    FragKind::Bytes { variants, chosen } => {
                        for f in &variants[*chosen].fixups {
                            if arch.moves_code(&f.kind) {
                                at.push(frag.offset + f.offset as u64);
                            }
                        }
                    }
                    FragKind::Align { align, .. } if *align > 1 => aligns.push(fi as u32),
                    _ => {}
                }
            }
            at.sort_unstable();
            self.moving_code[si] = at;
            self.aligning_code[si] = aligns;
        }
    }

    /// Whether a relaxing linker may delete bytes between two offsets of one
    /// section, so that the distance between labels there is the linker's to
    /// work out rather than this file's.
    ///
    /// The range is half-open: an instruction starting where the second label
    /// stands is past the distance, and one starting at the first label is
    /// within it. That is where llvm-mc draws the line too, which walks the
    /// fragments between the two symbols and folds the difference only where
    /// none of them is one the linker may rewrite.
    pub(crate) fn code_moves_between(&self, section: SectionId, from: u64, to: u64) -> bool {
        let (lo, hi) = (from.min(to), from.max(to));
        let Some(at) = self.moving_code.get(section.0 as usize) else {
            return false;
        };
        at.partition_point(|&o| o < lo) < at.partition_point(|&o| o < hi)
    }

    /// Whether the distance between two fragments of one section is one a
    /// reference leaves room to relocate although it turns out to be known.
    ///
    /// GNU as writes every address advance of a line table in code as a
    /// 16-bit field for the linker; llvm-mc writes one wherever it cannot see
    /// through the fragments between the two rows while it chooses the shape
    /// of the advance, which an alignment in a section the linker may relax
    /// stops it doing: how much that alignment pads is settled only once the
    /// code before it is. The field then holds the distance after all, and no
    /// relocation goes with it.
    ///
    /// Only a section that has something for the linker to shorten is
    /// affected, since otherwise the alignment is settled before the advance
    /// is; and the fragments are counted rather than the bytes, because an
    /// alignment that pads nothing still sits between two rows.
    pub(crate) fn code_uncertain_between(&self, section: SectionId, from: u32, to: u32) -> bool {
        let si = section.0 as usize;
        if self.moving_code.get(si).is_none_or(|m| m.is_empty()) {
            return false;
        }
        let Some(at) = self.aligning_code.get(si) else {
            return false;
        };
        let (lo, hi) = (from.min(to), from.max(to));
        at.partition_point(|&f| f < lo) < at.partition_point(|&f| f < hi)
    }

    /// The same, for two labels of one section, as a difference of symbols
    /// names them: their values are addresses, which in a flat image is not
    /// where the fragments were counted from.
    fn code_moves_for(&self, section: SectionId, plus: SymbolId, minus: SymbolId) -> bool {
        let base = self.section(section).addr as i64;
        let (Some(p), Some(m)) = (self.symbol_addr(plus), self.symbol_addr(minus)) else {
            return false;
        };
        self.code_moves_between(section, (p - base) as u64, (m - base) as u64)
    }

    /// Refuses data that had to be padded to reach its boundary; see
    /// [`Architecture::aligns_data`](crate::arch::Architecture::aligns_data).
    fn report_misaligned_data(&mut self) {
        for &(si, fi) in &self.align_tests {
            let f = &self.sections[si.0 as usize].frags[fi as usize];
            if let FragKind::Align { align, pad, .. } = f.kind
                && pad != 0
            {
                self.diags.error(
                    f.span,
                    format!(
                        "misaligned data: the value does not start at a multiple of {align} bytes; \
                         `.{align}byte` places one without aligning it"
                    ),
                );
            }
        }
    }

    /// Refuses a mark that has fewer bytes after it than GNU as's relocation
    /// covers; see [`Request::Mark`](crate::arch::Request::Mark).
    ///
    /// GNU as checks that a relocation lies inside the fragment it was made
    /// in, and a mark's is made with a field of its own width, which the
    /// bytes after the mark have to fill. Its fragments end where these do:
    /// at an alignment, a `.space`, or the end of the section, with no-op
    /// padding still inside the fragment it ends, since `arm_handle_align`
    /// writes it there; and after an instruction relaxation sizes, whose
    /// bytes are the variable part of the fragment before.
    fn report_uncontained_marks(&mut self) {
        for &(sec, fi, off, within, span) in &self.mark_tests {
            let s = &self.sections[sec.0 as usize];
            let mut have = s.frags[fi as usize].size() - off as u64;
            for f in &s.frags[fi as usize + 1..] {
                if have >= within as u64 {
                    break;
                }
                match &f.kind {
                    FragKind::Bytes { variants, .. } => {
                        have += f.size();
                        if f.relaxable || variants.len() > 1 {
                            break;
                        }
                    }
                    FragKind::Align { fill, .. } if fill.is_empty() => {
                        have += f.size();
                        break;
                    }
                    _ => break,
                }
            }
            if have < within as u64 {
                self.diags.error(
                    span,
                    format!(
                        "the relocation this places covers the {within} bytes after it, and only \
                         {have} follow before an alignment, a `.space`, a relaxable instruction \
                         or the end of the section"
                    ),
                );
            }
        }
    }

    /// Adds the sections the target's reference assembler writes into every
    /// object of its own accord, with what the attribute directives asked
    /// for merged into the build attributes; see
    /// [`Architecture::elf_attributes`].
    ///
    /// [`Architecture::elf_attributes`]: crate::arch::Architecture::elf_attributes
    fn add_attributes_section(&mut self) {
        // These sections are ELF's: a COFF or Mach-O object says what it
        // needs in its own header, and nothing reads a `.ARM.attributes`
        // there.
        if !self.options.relocatable || self.options.format.is_coff() {
            return;
        }
        if self.options.format == crate::output::Format::MachO {
            return;
        }
        // The lengths in a build-attributes section are written in the
        // object's byte order.
        let endian = self.target_state().0.endian();
        for sec in self.attribute_sections() {
            let bytes = match &sec.body {
                crate::arch::AttrBody::Bytes(b) => b.clone(),
                crate::arch::AttrBody::Tags(vendors) => {
                    let mut vendors = vendors.clone();
                    for v in &mut vendors {
                        for (vendor, tag, value) in &self.attr_overrides {
                            if *vendor != v.name {
                                continue;
                            }
                            match v.tags.binary_search_by_key(tag, |&(t, _)| t) {
                                Ok(i) => v.tags[i].1 = value.clone(),
                                Err(i) => v.tags.insert(i, (*tag, value.clone())),
                            }
                        }
                    }
                    crate::arch::encode_attributes(&vendors, endian)
                }
            };
            let name = self.interner.intern(sec.name);
            let flags = SectionFlags {
                alloc: sec.sh_flags & 0x2 != 0,
                write: sec.sh_flags & 0x1 != 0,
                exec: sec.sh_flags & 0x4 != 0,
                merge: sec.sh_flags & 0x10 != 0,
                ..SectionFlags::default()
            };
            let id = self.get_or_create_section(name, SectionKind::Progbits, flags, sec.align);
            let s = self.section_mut(id);
            s.entsize = sec.entsize;
            s.mark_arch(0);
            s.emit_bytes(&bytes, Span::default());
        }
    }

    /// Refers to the undefined symbols the target asks for on behalf of each
    /// section with contents; see [`Architecture::section_symbols`].
    ///
    /// GNU as asks section by section in its own order, which starts with the
    /// `.text`, `.data` and `.bss` it creates before reading the source, so
    /// those three are asked about first here too.
    ///
    /// [`Architecture::section_symbols`]: crate::arch::Architecture::section_symbols
    fn add_section_symbols(&mut self) {
        let mut order: Vec<usize> = (0..self.sections.len()).collect();
        let rank = |asm: &Self, si: usize| match asm.interner.get(asm.sections[si].name) {
            ".text" => 0,
            ".data" => 1,
            ".bss" => 2,
            _ => 3,
        };
        order.sort_by_key(|&si| rank(self, si));
        for si in order {
            if self.sections[si].size == 0 {
                continue;
            }
            let name = self.interner.get(self.sections[si].name).to_string();
            for sym in self.target().section_symbols(&name) {
                self.refer_to_symbol(sym, Span::default());
            }
        }
    }

    /// Makes `name` a symbol the object refers to, undefined unless the
    /// source defines it.
    pub(crate) fn refer_to_symbol(&mut self, name: &str, span: Span) {
        let name = self.interner.intern(name);
        let id = self.symbols.intern(name, span);
        self.symbols.get_mut(id).used = true;
    }

    fn pad_section_tails(&mut self) {
        for si in 0..self.sections.len() {
            let s = &self.sections[si];
            // A section that ends in another backend's code is padded as
            // that backend's GNU as would.
            let (arch, state) = self.frag_arch(si, s.frags.len());
            let align = s.align.min(arch.section_tail_align_limit());
            if !arch.pads_section_tail(&s.flags) {
                continue;
            }
            let exec = s.flags.exec;
            let fill = if exec { Vec::new() } else { vec![0] };
            // The no-ops are for the last instruction's state, if nothing
            // but data has followed it; see `Section::nop_state`.
            let nop_state = s.nop_state.clone().unwrap_or_else(|| state.clone());
            // No-op padding is code, and marked as such where the target
            // marks code; see `crate::mapping`. GNU as makes that padding
            // even for an alignment of one byte, so the mark is made too,
            // which is what marks data in a code section that has no code.
            if let Some(names) = self.mapping_names_for(&nop_state)
                && exec
            {
                self.map_align_with(SectionId(si as u32), names);
            }
            if align <= 1 {
                continue;
            }
            self.tail_pads.push(SectionId(si as u32));
            self.sections[si].push(Fragment::new(
                FragKind::Align {
                    align,
                    fill,
                    max_skip: None,
                    pad: 0,
                    nop_state: exec.then_some(nop_state),
                },
                Span::DUMMY,
            ));
        }
    }

    /// Adds the section a target's assembler writes about where the settled
    /// layout padded its code, if it writes one; see
    /// [`Architecture::layout_records`](crate::arch::Architecture::layout_records).
    /// Returns whether a section was added.
    fn emit_layout_records(&mut self) -> bool {
        use crate::arch::{LayoutPlace, PlaceKind};
        if !self.options.relocatable {
            return false;
        }
        let mut places = Vec::new();
        for (si, s) in self.sections.iter().enumerate() {
            if !(s.flags.exec && s.flags.alloc) {
                continue;
            }
            for f in &s.frags {
                let offset = f.offset + f.size();
                let section = SectionId(si as u32);
                let (kind, fill) = match &f.kind {
                    FragKind::Align { align, fill, .. } if *align > 1 => (
                        PlaceKind::Align(align.trailing_zeros()),
                        fill.first().copied().unwrap_or(0),
                    ),
                    FragKind::Org { target, fill, .. } => {
                        let addend = self.eval_ref(*target).map_or(0, |v| v.addend);
                        (PlaceKind::Org(addend), *fill)
                    }
                    _ => continue,
                };
                places.push(LayoutPlace {
                    kind,
                    section,
                    offset,
                    fill,
                });
            }
        }
        let Some(records) = self.target().layout_records(&places) else {
            return false;
        };
        let name = self.interner.intern(records.name);
        let id = self.get_or_create_section(name, SectionKind::Progbits, Default::default(), 1);
        self.section_mut(id).mark_arch(0);
        let reloc = self.target().data_reloc(4, false).unwrap_or(0);
        let kind = FixupKind::data(4).with_reloc(reloc);
        let mut fixups = Vec::new();
        for (at, index) in records.refs {
            let place = places[index];
            let sym = self.section_symbol(place.section);
            let base = self.exprs.alloc(ExprKind::SymId(sym), Span::DUMMY);
            let off = self.exprs.int(place.offset, Span::DUMMY);
            let expr = self.exprs.alloc(
                ExprKind::Binary(crate::expr::BinOp::Add, base, off),
                Span::DUMMY,
            );
            fixups.push(crate::section::Fixup {
                offset: at,
                expr,
                kind,
                span: Span::DUMMY,
            });
        }
        let s = self.section_mut(id);
        s.seal();
        s.push(Fragment::new(
            FragKind::Bytes {
                variants: vec![crate::section::Variant {
                    bytes: records.bytes,
                    fixups,
                }],
                chosen: 0,
            },
            Span::DUMMY,
        ));
        true
    }

    /// Walks every section assigning fragment offsets, recomputing the sizes
    /// that depend on them. Returns true if any size changed.
    fn assign_offsets(&mut self) -> bool {
        let mut changed = false;
        for si in 0..self.sections.len() {
            let mut off: u64 = 0;
            for fi in 0..self.sections[si].frags.len() {
                self.sections[si].frags[fi].offset = off;
                let prev = self.sections[si].frags[fi].size();
                let task = self.task_for(si, fi);
                let (size, encoded) = self.compute_size(si, off, task);
                // Cache the result on the fragment so `Fragment::size` stays
                // cheap and consistent between passes.
                match &mut self.sections[si].frags[fi].kind {
                    FragKind::Align { pad, .. } => *pad = size,
                    FragKind::Org { size: slot, .. } => *slot = size,
                    FragKind::Space { resolved, .. } => *resolved = size,
                    FragKind::Leb128 { encoded: slot, .. } => {
                        if let Some(e) = encoded {
                            *slot = e;
                        }
                    }
                    FragKind::Bytes { .. } => {}
                }
                if prev != size {
                    changed = true;
                }
                off = off.saturating_add(size);
            }
            if self.sections[si].size != off {
                self.sections[si].size = off;
                changed = true;
            }
        }
        changed
    }

    fn task_for(&self, si: usize, fi: usize) -> Task {
        let f = &self.sections[si].frags[fi];
        match &f.kind {
            FragKind::Bytes { variants, chosen } => {
                Task::Fixed(variants[*chosen].bytes.len() as u64)
            }
            FragKind::Align {
                align, max_skip, ..
            } => Task::Align {
                align: *align,
                max_skip: *max_skip,
            },
            FragKind::Org { target, .. } => Task::Org {
                target: *target,
                span: f.span,
            },
            FragKind::Space { size, .. } => Task::Space {
                size: *size,
                span: f.span,
            },
            FragKind::Leb128 { value, signed, .. } => Task::Leb {
                value: *value,
                signed: *signed,
                span: f.span,
            },
        }
    }

    /// Computes a fragment's size, plus the encoded bytes for LEB128.
    fn compute_size(&mut self, si: usize, off: u64, task: Task) -> (u64, Option<Vec<u8>>) {
        match task {
            Task::Fixed(n) => (n, None),
            Task::Align { align, max_skip } => {
                let pad = if align <= 1 {
                    0
                } else {
                    off.next_multiple_of(align) - off
                };
                // `.align n,,max` skips the padding entirely when it would
                // cost more than `max` bytes.
                match max_skip {
                    Some(m) if pad > m => (0, None),
                    _ => (pad, None),
                }
            }
            Task::Org { target, span } => {
                let id = SectionId(si as u32);
                let Some(t) = self.resolve_section_relative(target, id) else {
                    self.diags
                        .error(span, "`.org` target must resolve to a fixed offset");
                    return (0, None);
                };
                if t < off as i64 {
                    self.diags.error(
                        span,
                        format!("`.org` cannot move backwards, from offset {off} to {t}"),
                    );
                    return (0, None);
                }
                ((t as u64) - off, None)
            }
            Task::Space { size, span } => {
                let Some(n) = self.eval_absolute_quiet(size) else {
                    self.diags
                        .error(span, "`.space` size must be an absolute value");
                    return (0, None);
                };
                if n < 0 {
                    self.diags.error(span, "`.space` size must not be negative");
                    return (0, None);
                }
                (n as u64, None)
            }
            Task::Leb {
                value,
                signed,
                span,
            } => {
                let v = match self.eval_absolute_quiet(value) {
                    Some(v) => v,
                    None => {
                        self.diags
                            .error(span, "LEB128 value must be an absolute value");
                        0
                    }
                };
                let encoded = if signed {
                    sleb128(v)
                } else {
                    uleb128(v as u64)
                };
                (encoded.len() as u64, Some(encoded))
            }
        }
    }

    /// Moves fragments to a larger candidate where the current one no longer
    /// reaches. `history` is only used by [`Self::repick`], for targets whose
    /// sizes may also shrink, and by [`Self::repick_each_pass`], where it
    /// holds the fragments whose size is settled.
    fn relax(&mut self, history: &mut HashMap<(usize, usize), (u32, u32)>) -> bool {
        match self.relaxation() {
            Relaxation::Shrinking => return self.repick(history),
            Relaxation::EachPass => return self.repick_each_pass(history),
            Relaxation::InOrder => return self.grow_in_order(),
            Relaxation::FromLastPass => {}
        }
        let mut changed = false;
        for si in 0..self.sections.len() {
            for fi in 0..self.sections[si].frags.len() {
                let FragKind::Bytes { variants, chosen } = &self.sections[si].frags[fi].kind else {
                    continue;
                };
                let (nvariants, chosen) = (variants.len(), *chosen);
                if chosen + 1 >= nvariants || self.variant_fits(si, fi, chosen, 0) {
                    continue;
                }
                if let FragKind::Bytes { chosen, .. } = &mut self.sections[si].frags[fi].kind {
                    *chosen += 1;
                }
                changed = true;
            }
        }
        changed
    }

    /// Re-picks every fragment's size as the smallest candidate that reaches,
    /// walking each section in order the way GNU as's `relax_segment` does:
    /// fragments behind the one being sized are already at this pass's
    /// addresses, and a target ahead of it is moved by how much everything
    /// before it has grown so far (GNU's `stretch`). The order matters
    /// because a section can have more than one layout where everything
    /// reaches, and this is how GNU as arrives at its one.
    ///
    /// "Ahead" is decided the way `rx_relax_frag` decides it: by the target's
    /// address from the last pass against the branch's address in this one.
    /// A target that the growth so far has carried past the branch therefore
    /// counts as behind it and is not moved, which can leave that branch a
    /// size larger than it needs. GNU as does exactly that, so this does too.
    ///
    /// Alignment padding is recomputed on the way, as GNU as does; `.org`,
    /// `.space` and LEB128 sizes wait for the next full pass.
    fn repick(&mut self, history: &mut HashMap<(usize, usize), (u32, u32)>) -> bool {
        let mut changed = false;
        for si in 0..self.sections.len() {
            let mut off: u64 = 0;
            for fi in 0..self.sections[si].frags.len() {
                let old_off = self.sections[si].frags[fi].offset;
                self.sections[si].frags[fi].offset = off;
                let stretch = off as i64 - old_off as i64;
                let size = match &self.sections[si].frags[fi].kind {
                    FragKind::Bytes { variants, chosen } if variants.len() > 1 => {
                        let (n, chosen) = (variants.len(), *chosen);
                        // In a file that also has code for a backend that
                        // does not shrink, that code keeps growing only.
                        let lowest =
                            if self.frag_arch(si, fi).0.relaxation() == Relaxation::Shrinking {
                                0
                            } else {
                                chosen
                            };
                        let first_fit = (lowest..n)
                            .find(|&k| self.variant_fits(si, fi, k, stretch))
                            .unwrap_or(n - 1);
                        let counts = history.entry((si, fi)).or_insert((0, 0));
                        let pick = if first_fit < chosen {
                            // GNU as's guard against a size that flips back
                            // and forth, as alignment padding can make it:
                            // after ten of each, a fragment stops shrinking.
                            let stuck = counts.0 > 10 && counts.1 > 10;
                            counts.0 += 1;
                            if stuck { chosen } else { first_fit }
                        } else {
                            if first_fit > chosen {
                                counts.1 += 1;
                            }
                            first_fit
                        };
                        if pick != chosen {
                            changed = true;
                        }
                        if let FragKind::Bytes { variants, chosen } =
                            &mut self.sections[si].frags[fi].kind
                        {
                            *chosen = pick;
                            variants[pick].bytes.len() as u64
                        } else {
                            unreachable!()
                        }
                    }
                    FragKind::Align { .. } => {
                        let task = self.task_for(si, fi);
                        let (pad, _) = self.compute_size(si, off, task);
                        if let FragKind::Align { pad: slot, .. } =
                            &mut self.sections[si].frags[fi].kind
                        {
                            *slot = pad;
                        }
                        pad
                    }
                    _ => self.sections[si].frags[fi].size(),
                };
                off = off.saturating_add(size);
            }
            if self.sections[si].size != off {
                self.sections[si].size = off;
                changed = true;
            }
        }
        changed
    }

    /// Picks every relaxable fragment's size afresh, walking each section in
    /// order; see [`Relaxation::EachPass`]. `settled` holds the
    /// fragments that took a larger size on a pass where nothing before them
    /// had grown, which keep it.
    ///
    fn repick_each_pass(&mut self, settled: &mut HashMap<(usize, usize), (u32, u32)>) -> bool {
        let mut changed = false;
        for si in 0..self.sections.len() {
            let mut off: u64 = 0;
            for fi in 0..self.sections[si].frags.len() {
                let old_off = self.sections[si].frags[fi].offset;
                self.sections[si].frags[fi].offset = off;
                let stretch = off as i64 - old_off as i64;
                let size = match &self.sections[si].frags[fi].kind {
                    FragKind::Bytes { variants, chosen }
                        if variants.len() > 1 && !settled.contains_key(&(si, fi)) =>
                    {
                        let (n, chosen) = (variants.len(), *chosen);
                        let pick = (0..n)
                            .find(|&k| self.fits_stretched(si, fi, k, stretch))
                            .unwrap_or(n - 1);
                        if pick != chosen {
                            changed = true;
                        }
                        if pick > 0 && stretch <= 0 {
                            settled.insert((si, fi), (0, 0));
                        }
                        if let FragKind::Bytes { variants, chosen } =
                            &mut self.sections[si].frags[fi].kind
                        {
                            *chosen = pick;
                            variants[pick].bytes.len() as u64
                        } else {
                            unreachable!()
                        }
                    }
                    FragKind::Align { .. } => {
                        let task = self.task_for(si, fi);
                        let (pad, _) = self.compute_size(si, off, task);
                        if let FragKind::Align { pad: slot, .. } =
                            &mut self.sections[si].frags[fi].kind
                        {
                            *slot = pad;
                        }
                        pad
                    }
                    _ => self.sections[si].frags[fi].size(),
                };
                off = off.saturating_add(size);
            }
            if self.sections[si].size != off {
                self.sections[si].size = off;
                changed = true;
            }
        }
        changed
    }

    /// Whether every fixup of candidate `k` of a fragment is in range for
    /// [`Self::repick_each_pass`], which has moved everything up to the
    /// fragment by `stretch` bytes this pass. A target in a later fragment
    /// moves by that much too, rounded down to each alignment it is behind,
    /// since the alignment would absorb the rest: GNU as's
    /// `relaxed_symbol_addr`.
    fn fits_stretched(&mut self, si: usize, fi: usize, k: usize, stretch: i64) -> bool {
        let id = SectionId(si as u32);
        let frag_off = self.sections[si].frags[fi].offset;
        let fixups: Vec<(u32, ExprRef, FixupKind)> = match &self.sections[si].frags[fi].kind {
            FragKind::Bytes { variants, .. } => variants[k]
                .fixups
                .iter()
                .map(|f| (f.offset, f.expr, f.kind))
                .collect(),
            _ => return true,
        };
        for (off, e, kind) in fixups {
            let Some(mut value) = self.fixup_value(e, &kind, id, fi, frag_off + off as u64) else {
                return false;
            };
            let target = match self.eval(e) {
                Ok(Value {
                    plus: Some(p),
                    minus: None,
                    ..
                }) if kind.pcrel && stretch != 0 => match self.symbols.get(p).value {
                    SymbolValue::Label { section, frag } if section == id && frag as usize > fi => {
                        Some(frag as usize)
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(frag) = target {
                let mut moved = stretch;
                for f in &self.sections[si].frags[fi + 1..frag.min(self.sections[si].frags.len())] {
                    if let FragKind::Align { align, .. } = f.kind
                        && align > 1
                    {
                        let mask = align as i64 - 1;
                        moved = moved.signum() * (moved.abs() & !mask);
                        if moved == 0 {
                            break;
                        }
                    }
                }
                value += moved;
            }
            if !kind.fits(value as i128) {
                return false;
            }
        }
        true
    }

    /// Grows fragments walking each section in order, the way GNU as's
    /// generic `relax_frag` does. Fragments behind the one being sized are
    /// already at this pass's addresses. A label ahead of it moves by the
    /// growth so far (GNU's `stretch`) only if no alignment or `.org` lies in
    /// between, since one might absorb it; past one, the label keeps its
    /// last-pass address, and a branch that the growth has carried beyond
    /// such a label keeps its size for this pass.
    ///
    /// A candidate is weighed at the addresses the current one has, as GNU
    /// as reads its relaxation table: the table's reach already allows for
    /// the longer form's extra instructions.
    ///
    /// Alignment padding is recomputed on the way; `.org`, `.space` and
    /// LEB128 sizes wait for the next full pass.
    fn grow_in_order(&mut self) -> bool {
        let mut changed = false;
        for si in 0..self.sections.len() {
            // GNU's regions: each alignment and `.org` ends one.
            let mut regions = Vec::with_capacity(self.sections[si].frags.len() + 1);
            let mut region = 0u32;
            for f in &self.sections[si].frags {
                regions.push(region);
                if matches!(f.kind, FragKind::Align { .. } | FragKind::Org { .. }) {
                    region += 1;
                }
            }
            regions.push(region);

            let mut off: u64 = 0;
            for fi in 0..self.sections[si].frags.len() {
                let old_off = self.sections[si].frags[fi].offset;
                self.sections[si].frags[fi].offset = off;
                let stretch = off as i64 - old_off as i64;
                let size = match &self.sections[si].frags[fi].kind {
                    FragKind::Bytes { variants, chosen } if variants.len() > 1 => {
                        let (n, mut pick) = (variants.len(), *chosen);
                        if pick + 1 < n && !self.fits_in_order(si, fi, pick, stretch, &regions) {
                            pick = (pick + 1..n)
                                .find(|&k| self.fits_in_order(si, fi, k, stretch, &regions))
                                .unwrap_or(n - 1);
                            changed = true;
                        }
                        if let FragKind::Bytes { variants, chosen } =
                            &mut self.sections[si].frags[fi].kind
                        {
                            *chosen = pick;
                            variants[pick].bytes.len() as u64
                        } else {
                            unreachable!()
                        }
                    }
                    FragKind::Align { .. } => {
                        let task = self.task_for(si, fi);
                        let (pad, _) = self.compute_size(si, off, task);
                        if let FragKind::Align { pad: slot, .. } =
                            &mut self.sections[si].frags[fi].kind
                        {
                            *slot = pad;
                        }
                        pad
                    }
                    _ => self.sections[si].frags[fi].size(),
                };
                off = off.saturating_add(size);
            }
            if self.sections[si].size != off {
                self.sections[si].size = off;
                changed = true;
            }
        }
        changed
    }

    /// Whether every fixup of candidate `k` of a fragment is in range for
    /// [`Self::grow_in_order`], which has moved everything up to the fragment
    /// by `stretch` bytes this pass. `regions` numbers each fragment's region.
    fn fits_in_order(
        &mut self,
        si: usize,
        fi: usize,
        k: usize,
        stretch: i64,
        regions: &[u32],
    ) -> bool {
        let id = SectionId(si as u32);
        let frag_off = self.sections[si].frags[fi].offset;
        let fixups: Vec<(u32, ExprRef, FixupKind)> = match &self.sections[si].frags[fi].kind {
            FragKind::Bytes { variants, .. } => variants[k]
                .fixups
                .iter()
                .map(|f| (f.offset, f.expr, f.kind))
                .collect(),
            _ => return true,
        };
        let here = self.section(id).addr as i64 + frag_off as i64;
        for (off, e, kind) in fixups {
            let Some(mut value) = self.fixup_value(e, &kind, id, fi, frag_off + off as u64) else {
                return false;
            };
            // A label ahead, in the fragments this pass has yet to reach.
            // An alias counts as the label it names.
            let ahead = match self.eval_ref(e) {
                Ok(v) if kind.pcrel && stretch != 0 => {
                    v.plus.and_then(|p| match self.symbols.get(p).value {
                        SymbolValue::Label { section, frag }
                            if section == id && frag as usize > fi =>
                        {
                            Some((frag as usize, self.resolve_value(v)?))
                        }
                        _ => None,
                    })
                }
                _ => None,
            };
            if let Some((frag, target)) = ahead {
                if stretch < 0 || regions[frag] == regions[fi] {
                    value += stretch;
                } else if target < here {
                    return true;
                }
            }
            if !kind.fits(value as i128) {
                return false;
            }
        }
        true
    }

    /// Whether every fixup of candidate `k` of a fragment is in range, with
    /// the fragment taking that candidate's size and everything after it
    /// moved by a further `stretch` bytes.
    fn variant_fits(&mut self, si: usize, fi: usize, k: usize, stretch: i64) -> bool {
        let frag_off = self.sections[si].frags[fi].offset;
        let (fixups, delta): (Vec<(u32, ExprRef, FixupKind)>, i64) =
            match &self.sections[si].frags[fi].kind {
                FragKind::Bytes { variants, chosen } => (
                    variants[k]
                        .fixups
                        .iter()
                        .map(|f| (f.offset, f.expr, f.kind))
                        .collect(),
                    variants[k].bytes.len() as i64 - variants[*chosen].bytes.len() as i64,
                ),
                _ => return true,
            };
        let id = SectionId(si as u32);
        if delta + stretch != 0 {
            self.relax_shift = Some((id, fi as u32, frag_off, delta + stretch));
        }
        let fits = fixups.iter().all(|(off, e, kind)| {
            if kind.relax_difference {
                // The smallest field whose signed range holds the value,
                // with no limit on the four-byte one.
                let half = 1i64 << (kind.size.min(4) as u32 * 8 - 1);
                return match self.relaxed_difference(*e, id, frag_off, stretch) {
                    Some(v) => {
                        (kind.size >= 4 || (-half..half).contains(&v)) && kind.fits(v as i128)
                    }
                    None => false,
                };
            }
            let at = frag_off + *off as u64;
            match self.fixup_value(*e, kind, id, fi, at) {
                Some(v) => kind.fits(v as i128),
                // An unresolved reference takes the widest form on offer.
                // Nothing is known about how far away its target will be, so
                // any shorter form is a guess the linker may not be able to
                // honour: a RISC-V `c.j` carries a relocation, but reaches only
                // ±2 KiB. llvm-mc makes the same choice (checked: `j sym` to
                // an undefined `sym` is a full `R_RISCV_JAL`).
                None => false,
            }
        });
        self.relax_shift = None;
        fits
    }

    /// The value GNU as's RX port sizes a symbolic immediate by, for a
    /// fixup marked [`FixupKind::relax_difference`] in an instruction at
    /// offset `pc` that the growth so far has moved by `stretch`.
    ///
    /// `rx_frag_fix_value` gives up, and the widest field is taken, unless
    /// the value is a difference of two labels in the instruction's own
    /// section that the linker could not move apart: a global or weak label
    /// always gets a relocation in ELF, so it counts as unknown. Labels ahead
    /// of the instruction are where the last pass put them, and the growth is
    /// then added to the difference as a whole, when the difference read as
    /// an unsigned address lies past `pc` — which is also true of every
    /// negative difference. The test makes little sense for a difference, but
    /// it is the one GNU as applies, and the sizes it picks follow from it.
    fn relaxed_difference(
        &mut self,
        e: ExprRef,
        section: SectionId,
        pc: u64,
        stretch: i64,
    ) -> Option<i64> {
        let v = self.eval(e).ok()?;
        let (Some(p), Some(m)) = (v.plus, v.minus) else {
            return None;
        };
        for s in [p, m] {
            if self.symbol_section(s) != Some(section)
                || self.symbols.get(s).binding != Binding::Local
            {
                return None;
            }
        }
        // Where the labels are on this pass, not where `variant_fits` is
        // weighing moving them.
        let shift = self.relax_shift.take();
        let diff = self.symbol_addr(p).zip(self.symbol_addr(m));
        self.relax_shift = shift;
        let diff = diff.map(|(p, m)| p.wrapping_sub(m))?;
        let pc = self.section(section).addr + pc;
        let grown = if diff as u64 > pc { stretch } else { 0 };
        Some(diff.wrapping_add(v.addend).wrapping_add(grown))
    }

    /// Gives each section a base address. Relocatable output leaves them all
    /// at zero; absolute output lays them out end to end, with the ones that
    /// occupy no file space last in NASM source.
    fn assign_addresses(&mut self) {
        if self.options.relocatable {
            for s in &mut self.sections {
                s.addr = 0;
            }
            return;
        }
        // NASM's flat output lays the sections that occupy file space out
        // first, in the order the source named them, and puts the ones that
        // allocate without writing after them, so that no `.bss` leaves a
        // hole in the image.
        let nasm = self.options.dialect == crate::lexer::Dialect::Nasm;
        let order: Vec<usize> = if nasm {
            let nobits = |i: &usize| self.sections[*i].kind == SectionKind::Nobits;
            (0..self.sections.len())
                .filter(|i| !nobits(i))
                .chain((0..self.sections.len()).filter(nobits))
                .collect()
        } else {
            (0..self.sections.len()).collect()
        };
        let mut addr = self.options.base_addr;
        for i in order {
            let s = &mut self.sections[i];
            // An empty section is not aligned: a linker drops it from the
            // image, alignment and all, rather than pad for nothing. NASM
            // aligns it all the same, and what follows starts after the hole,
            // so a `section .data align=8` with nothing in it still moves the
            // next section along.
            let align = if s.size == 0 && !nasm {
                1
            } else {
                s.align.max(1)
            };
            addr = match s.origin {
                Some(origin) => origin,
                None => addr.next_multiple_of(align),
            };
            s.addr = addr;
            addr += s.size;
        }
    }

    // ---- value resolution -------------------------------------------------

    /// The address a symbol resolves to, if it has one yet.
    pub(crate) fn symbol_addr(&self, id: SymbolId) -> Option<i64> {
        match self.symbols.get(id).value {
            SymbolValue::Label { section, frag } => {
                let s = self.section(section);
                let off = match s.frags.get(frag as usize) {
                    Some(f) => f.offset,
                    // A label at the very end of a section has no fragment of
                    // its own; it sits at the section's current size.
                    None => s.size,
                };
                let shift = match self.relax_shift {
                    Some((sec, fi, pc, shift)) if sec == section && frag > fi && off > pc => shift,
                    _ => 0,
                };
                Some((s.addr + off) as i64 + shift)
            }
            // An alias of a label, which evaluation keeps by its own name.
            SymbolValue::Expr(_) => {
                let v = self.eval_ref_symbol(id).ok()?;
                match (v.plus, v.minus) {
                    (Some(p), None) if p != id => Some(self.symbol_addr(p)?.wrapping_add(v.addend)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The section a label, or an alias of one, is in.
    pub(crate) fn symbol_section(&self, id: SymbolId) -> Option<SectionId> {
        match self.symbols.get(id).value {
            SymbolValue::Label { section, .. } => Some(section),
            SymbolValue::Expr(_) => self.symbol_target_section(id).map(|(s, _)| s),
            _ => None,
        }
    }

    /// Whether the linker has to treat `id` as a thread-local variable, so
    /// that its value is an offset into the thread-local block rather than an
    /// address.
    ///
    /// Being defined in a section with `SHF_TLS` is enough on its own, and it
    /// is what GNU as decides as it writes the object: a `.type` naming some
    /// other type does not take it back, whichever side of the label it is
    /// on. The declared type still counts for the symbols that are in no such
    /// section — `.tls_common`, and a `%tls_object` the linker must find
    /// elsewhere.
    pub(crate) fn is_thread_local(&self, id: SymbolId) -> bool {
        self.symbols.get(id).ty == crate::symbol::SymType::Tls
            || self
                .symbol_section(id)
                .is_some_and(|s| self.section(s).flags.tls)
    }

    /// The value a relocation's symbol has in the object: its offset in its
    /// section where it is defined in one, and zero where it is undefined,
    /// common or a section symbol. See [`Architecture::rel_field`].
    ///
    /// [`Architecture::rel_field`]: crate::arch::Architecture::rel_field
    fn named_value(&self, id: SymbolId) -> i64 {
        match (self.symbol_section(id), self.symbol_addr(id)) {
            (Some(s), Some(addr)) => addr - self.section(s).addr as i64,
            _ => 0,
        }
    }

    /// Reduces a [`Value`] to a number, if every symbol in it has an address.
    pub(crate) fn resolve_value(&self, v: Value) -> Option<i64> {
        let mut n = v.addend;
        if let Some(p) = v.plus {
            n = n.wrapping_add(self.symbol_addr(p)?);
        }
        if let Some(m) = v.minus {
            n = n.wrapping_sub(self.symbol_addr(m)?);
        }
        Some(n)
    }

    /// Evaluates an expression, ignoring errors (the caller reports its own).
    fn eval_absolute_quiet(&mut self, e: ExprRef) -> Option<i64> {
        match self.eval(e) {
            Ok(v) => self.resolve_value(v),
            Err(_) => self.placement_free_value(e),
        }
    }

    /// The value of an expression the symbolic evaluator cannot keep in
    /// `plus - minus + addend` form, such as NASM's `(($-$$) % 8)`, worked
    /// out from the labels' addresses, if it does not depend on where the
    /// sections end up.
    ///
    /// In a flat image the addresses are final, so one evaluation settles
    /// it. In an object each section may still move, so the expression is
    /// evaluated a second time with every section moved by a different odd
    /// amount, and only a value that stays the same counts.
    pub(crate) fn placement_free_value(&self, e: ExprRef) -> Option<i64> {
        struct Addresses<'a> {
            asm: &'a Assembler,
            moved: bool,
            depth: u32,
        }
        impl expr::EvalCtx for Addresses<'_> {
            fn lookup_symbol(&mut self, name: Name, span: Span) -> Result<Value, EvalError> {
                match self.asm.symbols.lookup(name) {
                    Some(id) => self.symbol_value(id, span),
                    None => Err(EvalError::new(span, "undefined symbol")),
                }
            }
            fn symbol_value(&mut self, id: SymbolId, span: Span) -> Result<Value, EvalError> {
                match self.asm.symbols.get(id).value {
                    SymbolValue::Expr(e) if self.depth < 64 => {
                        self.depth += 1;
                        let v = expr::eval(&self.asm.exprs, e, self);
                        self.depth -= 1;
                        v
                    }
                    SymbolValue::Label { section, .. } => {
                        let addr = self
                            .asm
                            .symbol_addr(id)
                            .ok_or_else(|| EvalError::new(span, "no address"))?;
                        let shift = if self.moved {
                            (section.0 as i64 + 1).wrapping_mul(0x1_0000_0001)
                        } else {
                            0
                        };
                        Ok(Value::abs(addr.wrapping_add(shift)))
                    }
                    _ => Err(EvalError::new(span, "not a number")),
                }
            }
            fn here(&mut self, span: Span) -> Result<Value, EvalError> {
                Err(EvalError::new(span, "not a number"))
            }
            fn section_start(&mut self, span: Span) -> Result<Value, EvalError> {
                Err(EvalError::new(span, "not a number"))
            }
            fn local_ref(&mut self, _: u32, _: LocalDir, span: Span) -> Result<Value, EvalError> {
                Err(EvalError::new(span, "not a number"))
            }
            fn modifier(&mut self, _: Name, _: Value, span: Span) -> Result<Value, EvalError> {
                Err(EvalError::new(span, "not a number"))
            }
        }
        let mut env = Addresses {
            asm: self,
            moved: false,
            depth: 0,
        };
        let v = expr::eval(&self.exprs, e, &mut env).ok()?.as_abs()?;
        if self.options.relocatable {
            env.moved = true;
            let w = expr::eval(&self.exprs, e, &mut env).ok()?.as_abs()?;
            if v != w {
                return None;
            }
        }
        Some(v)
    }

    /// Resolves an expression to an offset within `section`.
    ///
    /// `.org 64` gives a plain number, which is already section-relative,
    /// while `. = . + 16` gives an address, which has to have the section base
    /// taken off it.
    fn resolve_section_relative(&mut self, e: ExprRef, section: SectionId) -> Option<i64> {
        let v = self.eval(e).ok()?;
        if v.is_absolute() {
            return Some(v.addend);
        }
        if v.plus
            .is_some_and(|p| self.symbol_section(p) == Some(section))
            && v.minus.is_none()
        {
            let addr = self.resolve_value(v)?;
            return Some(addr - self.section(section).addr as i64);
        }
        None
    }

    /// The number a fixup should write, or `None` if it needs a relocation.
    ///
    /// In a flat image that number is what the linker would have computed
    /// from the relocation, which is more than the target's value for the
    /// kinds that say so in [`FixupKind::link`].
    fn fixup_value(
        &mut self,
        e: ExprRef,
        kind: &FixupKind,
        section: SectionId,
        fi: usize,
        at: u64,
    ) -> Option<i64> {
        if kind.always_reloc {
            return None;
        }
        let flat = !self.options.relocatable;
        match kind.link {
            LinkValue::Plain => {}
            LinkValue::Split(f) => {
                return self.plain_fixup_value(e, kind, section, fi, at).map(f);
            }
            LinkValue::Page(bits) if flat => {
                let target = self.eval(e).ok().and_then(|v| self.resolve_value(v))?;
                let here = (self.section(section).addr + at) as i64;
                let page = !((1i64 << bits) - 1);
                return Some((target & page) - (here & page));
            }
            LinkValue::Region { bits, numbers } if flat => {
                if self
                    .outside_region(e, kind, section, fi, at, bits, numbers)
                    .is_some()
                {
                    return None;
                }
            }
            LinkValue::PairedLow if flat => return self.paired_low_value(e),
            LinkValue::LinkerOnly(_) if flat => return None,
            LinkValue::Interwork(class) => match self.interwork(e, class, section, fi) {
                Interwork::AsWritten => {}
                Interwork::Relocate | Interwork::LinkerOnly(_) => return None,
                Interwork::Becomes { kind, .. } => {
                    return self.fixup_value(e, &kind, section, fi, at);
                }
            },
            LinkValue::Page(_)
            | LinkValue::Region { .. }
            | LinkValue::PairedLow
            | LinkValue::LinkerOnly(_) => {}
        }
        // A size modifier is either a number the file settles or a reference
        // only the linker can resolve; what the symbol itself stands for is
        // never the answer, so one that does not fold is left to a
        // relocation even where the expression has a value of its own. A
        // size the file settles is a number whatever the output format: an
        // object writes it in place of a relocation, and a flat image can
        // write it too, where the modifiers a linker has to answer cannot be
        // written at all.
        if let Some((name, operand)) = self.size_modifier(e, section, fi) {
            // Only where the field could have carried the relocation. GNU as
            // decides that from the field alone, so `.byte sized@SIZE` and
            // `movl sized@SIZE(%rax), %ecx` are refused although the number
            // was there to write; leaving them unfolded is what refuses them.
            let arch = self.frag_arch(section.0 as usize, fi).0;
            if matches!(
                arch.fixup_modifier_reloc(self.interner.get(name), kind),
                crate::arch::FixupModifier::Unknown
            ) {
                return None;
            }
            return self.folded_symbol_size(e, operand);
        }
        // A modifier on a plain reference (`.long foo@PLT`) only picks the
        // relocation in an object; in a flat image it decides the value. One
        // that takes part of the value (AVR's `lo8()`) does so as the field
        // is written, whichever the output.
        if flat && let Some(m) = self.find_modifier(e) {
            let name = self.interner.get(m);
            match self.frag_arch(section.0 as usize, fi).0.flat_modifier(name) {
                FlatModifier::Plain | FlatModifier::Field { .. } => {}
                FlatModifier::PcRelative if kind.pcrel => {}
                FlatModifier::PcRelative => {
                    let target = self.eval(e).ok().and_then(|v| self.resolve_value(v))?;
                    return Some(target - (self.section(section).addr + at) as i64);
                }
                FlatModifier::LinkerOnly => return None,
            }
        }
        let value = self.plain_fixup_value(e, kind, section, fi, at)?;
        // What a linker adds to the target in a field of this relocation
        // type, which in a flat image is the core's to add.
        if flat
            && kind.reloc != 0
            && let Ok(Value {
                plus: Some(p),
                minus: None,
                ..
            }) = self.eval(e)
        {
            let sym = self.symbols.get(p);
            let (flags, ty) = (sym.target_flags, sym.ty);
            let arch = self.frag_arch(section.0 as usize, fi).0;
            return Some(value + arch.link_bias(kind.reloc, flags, ty));
        }
        Some(value)
    }

    /// For a [`LinkValue::Interwork`] fixup, what its instruction becomes
    /// given the symbol it refers to; see [`Architecture::interwork`].
    ///
    /// [`Architecture::interwork`]: crate::arch::Architecture::interwork
    pub(crate) fn interwork(
        &mut self,
        e: ExprRef,
        class: u8,
        section: SectionId,
        fi: usize,
    ) -> Interwork {
        let Ok(Value {
            plus: Some(p),
            minus: None,
            ..
        }) = self.eval(e)
        else {
            return Interwork::AsWritten;
        };
        let sym = self.symbols.get(p);
        let defined = sym.is_defined();
        let target = InterworkTarget {
            flags: sym.target_flags,
            ty: sym.ty,
            same_section: self.symbol_section(p) == Some(section),
            global: sym.binding != Binding::Local || !defined,
            weak: sym.binding == Binding::Weak,
            preemptible: sym.binding == Binding::Weak
                || (sym.binding == Binding::Global && sym.visibility == Visibility::Default)
                || !defined,
            relocatable: self.options.relocatable,
        };
        self.frag_arch(section.0 as usize, fi)
            .0
            .interwork(class, &target)
    }

    /// For a [`LinkValue::Region`] fixup, the target and the address past the
    /// field, if the target is outside that address's region. Whether a
    /// target written as a plain number counts is the fixup's own choice;
    /// see [`LinkValue::Region`].
    #[allow(clippy::too_many_arguments)]
    fn outside_region(
        &mut self,
        e: ExprRef,
        kind: &FixupKind,
        section: SectionId,
        fi: usize,
        at: u64,
        bits: u8,
        numbers: bool,
    ) -> Option<(i64, u64)> {
        match self.eval(e).ok()?.plus {
            Some(label) => {
                self.symbol_section(label)?;
            }
            None if !numbers => return None,
            None => {}
        }
        let mut v = self.plain_fixup_value(e, kind, section, fi, at)?;
        let here = self.section(section).addr + at;
        // A PC-relative field holds a distance, and the region is the
        // target's: an MCS-51 branch cannot leave the 64 KiB address space
        // however short it is.
        if kind.pcrel {
            v += (here as i64 + kind.adjust as i64) & !(kind.pc_align.max(1) as i64 - 1);
        }
        let next = here + kind.size as u64;
        (v >> bits != next as i64 >> bits).then_some((v, next))
    }

    /// The value of a [`LinkValue::PairedLow`] fixup in a flat image: the
    /// value of the PC-relative fixup on the instruction its label names,
    /// computed where that fixup is.
    fn paired_low_value(&mut self, e: ExprRef) -> Option<i64> {
        let (section, fi, at, expr, kind) = self.paired_high(e)?;
        self.fixup_value(expr, &kind, section, fi, at)
    }

    /// Finds the high half a [`LinkValue::PairedLow`] expression names: a
    /// PC-relative fixup at the very address of a label, with no addend.
    /// GNU ld refuses an addend there as well, and the label has to be on
    /// the instruction itself, not merely near it.
    fn paired_high(&mut self, e: ExprRef) -> Option<(SectionId, usize, u64, ExprRef, FixupKind)> {
        let v = self.eval_ref(e).ok()?;
        let (Some(label), None, 0) = (v.plus, v.minus, v.addend) else {
            return None;
        };
        let SymbolValue::Label { section, frag } = self.symbols.get(label).value else {
            return None;
        };
        let frags = &self.section(section).frags;
        let off = frags.get(frag as usize)?.offset;
        // The label names a fragment index, and empty fragments (an
        // alignment that needed no padding) can sit between it and the
        // instruction at the same offset.
        frags[frag as usize..]
            .iter()
            .enumerate()
            .take_while(|(_, f)| f.offset == off)
            .find_map(|(i, f)| match &f.kind {
                FragKind::Bytes { variants, chosen } => variants[*chosen]
                    .fixups
                    .iter()
                    .find(|x| x.offset == 0 && x.kind.pcrel)
                    .map(|x| (section, frag as usize + i, off, x.expr, x.kind)),
                _ => None,
            })
    }

    /// Why a flat image cannot resolve a fixup that it could have relocated,
    /// if the reason is more specific than an undefined symbol.
    fn flat_refusal(
        &mut self,
        e: ExprRef,
        kind: &FixupKind,
        section: SectionId,
        fi: usize,
        at: u64,
    ) -> Option<String> {
        match kind.link {
            LinkValue::LinkerOnly(what) => {
                return Some(format!(
                    "this refers to {what}, which only a linker creates; a flat binary has none"
                ));
            }
            LinkValue::PairedLow
                if self.eval(e).is_ok_and(|v| self.resolve_value(v).is_some())
                    && self.paired_high(e).is_none() =>
            {
                return Some(
                    "the low half of a PC-relative pair must name the label on its high half, \
                     with no addend"
                        .into(),
                );
            }
            LinkValue::Region { bits, numbers } => {
                let (v, next) = self.outside_region(e, kind, section, fi, at, bits, numbers)?;
                let target = if v < 0 {
                    format!("-{:#x}", v.unsigned_abs())
                } else {
                    format!("{v:#x}")
                };
                return Some(format!(
                    "target {target} is outside the {} region this field reaches from {next:#x}",
                    byte_size(1u64 << bits)
                ));
            }
            LinkValue::Split(_) => return None,
            LinkValue::Interwork(class) => {
                if let Interwork::LinkerOnly(what) = self.interwork(e, class, section, fi) {
                    return Some(format!(
                        "this branch needs {what}, which only a linker builds; a flat binary \
                         has none"
                    ));
                }
            }
            _ => {}
        }
        let m = self.find_modifier(e)?;
        let name = self.interner.get(m);
        let arch = self.frag_arch(section.0 as usize, fi).0;
        if !matches!(arch.flat_modifier(name), FlatModifier::LinkerOnly) {
            return None;
        }
        let written = crate::arch::written_modifier(arch, name);
        // A name the backend has no relocation for names nothing at all, so
        // it is refused here for the reason an object refuses it rather than
        // for the linker a flat image lacks.
        Some(match arch.fixup_modifier_reloc(name, kind) {
            crate::arch::FixupModifier::Unknown => format!(
                "{written} is not a relocation modifier the `{}` backend supports on \
                 this operand",
                arch.name()
            ),
            _ => format!("{written} names something only a linker creates; a flat binary has none"),
        })
    }

    /// Whether a PC-relative reference from fragment `fi` to `target`, a
    /// symbol in the same section, is still left to the linker; see
    /// [`Architecture::defers_to_linker`](crate::arch::Architecture::defers_to_linker).
    fn defers_to_linker(
        &self,
        e: ExprRef,
        target: SymbolId,
        kind: &FixupKind,
        section: SectionId,
        fi: usize,
    ) -> bool {
        if kind.object_reloc {
            return true;
        }
        // A Mach-O object decides by atoms, not by binding; see
        // `crate::output::macho::defers_to_linker`.
        if self.macho_object() {
            // A modifier names something only the linker makes, such as a
            // GOT slot, however near the symbol is.
            return self.find_modifier(e).is_some()
                || crate::output::macho::defers_to_linker(self, target, section, fi as u32);
        }
        let (arch, _) = self.frag_arch(section.0 as usize, fi);
        let modifier = self.find_modifier(e).map(|m| self.interner.get(m));
        let reloc = modifier
            .and_then(|m| arch.modifier_reloc(m, kind.size, kind.pcrel))
            .unwrap_or(kind.reloc);
        let f = &self.section(section).frags[fi];
        let relaxable = f.relaxable
            || matches!(&f.kind, FragKind::Bytes { variants, .. } if variants.len() > 1);
        // Nothing is preempted in a COFF object: Windows has no symbol
        // interposition, so llvm-mc resolves a reference to a symbol in the
        // fixup's own section, weak ones included — a weak definition there
        // is an alias the reference can be bound to right here. Except to a
        // function (`.def f; .type 32; .endef`): the MSVC linker's
        // incremental linking and control flow guard find the calls between
        // functions by their relocations, so llvm-mc keeps every one it can.
        if self.options.format.is_coff() {
            let describable = crate::output::coff::machine(self.target())
                .and_then(|m| crate::output::coff::reloc::map(m, kind.class, reloc))
                .is_some();
            return (describable || relaxable) && crate::coff::is_function(self, target);
        }
        // A field no relocation can describe has to be filled in here, unless
        // the instruction has a larger form to move to that one can.
        if reloc == 0 && !relaxable {
            return false;
        }
        arch.defers_to_linker(&crate::arch::SameSectionRef {
            binding: self.symbols.get(target).binding,
            reloc,
            modifier,
            relaxable,
        })
    }

    /// What a target's GNU as leaves in the field of a PC-relative fixup at
    /// `at` that it relocates against a symbol in the same section; see
    /// [`Architecture::relocated_pcrel_field`](crate::arch::Architecture::relocated_pcrel_field).
    fn relocated_field(
        &mut self,
        e: ExprRef,
        kind: &FixupKind,
        section: SectionId,
        fi: usize,
        at: u64,
    ) -> Option<i64> {
        if !kind.pcrel || !self.options.relocatable {
            return None;
        }
        let target = self.eval(e).ok()?.plus?;
        if self.symbol_section(target) != Some(section) {
            return None;
        }
        let binding = self.symbols.get(target).binding;
        self.frag_arch(section.0 as usize, fi)
            .0
            .relocated_pcrel_field(binding, at)
    }

    /// [`Self::fixup_value`] for a fixup whose value is the target itself.
    fn plain_fixup_value(
        &mut self,
        e: ExprRef,
        kind: &FixupKind,
        section: SectionId,
        fi: usize,
        at: u64,
    ) -> Option<i64> {
        // A fixup that leaves its target out wants only the addend, which
        // the expression already holds; see `FixupKind::without_symbol`.
        if kind.without_symbol {
            let addend = self.eval(e).ok()?.addend;
            if !kind.pcrel {
                return Some(addend);
            }
            let here = (self.section(section).addr + at) as i64 + kind.adjust as i64;
            return Some(addend - (here & !(kind.pc_align.max(1) as i64 - 1)));
        }
        let v = match self.eval(e) {
            Ok(v) => v,
            // In a flat image every label has its address by now, so an
            // expression that only a number can go through — `label >> 8`,
            // `label & 0xff`, the 8-bit dialect's `<label` — has a value
            // too. A relocation could not have carried it, which is why
            // evaluating symbolically refused it.
            Err(_) if !self.options.relocatable => {
                let mut env = AddressEnv {
                    asm: self,
                    depth: 0,
                };
                let v = crate::expr::eval(&self.exprs, e, &mut env).ok()?;
                return v.as_abs().map(|target| {
                    if kind.pcrel {
                        let here = (self.section(section).addr + at) as i64 + kind.adjust as i64;
                        target - (here & !(kind.pc_align.max(1) as i64 - 1))
                    } else {
                        target
                    }
                });
            }
            Err(_) => return None,
        };
        // Within one section the two section bases cancel, so a PC-relative
        // reference resolves even in relocatable output. Across sections it
        // resolves only once the sections have real addresses.
        if kind.pcrel {
            // An absolute target is no closer, on targets where a number is
            // an address: where the field ends up is the linker's decision,
            // so `call 0x1000` needs a relocation too. A difference is left
            // to `relocations_for`, which folds or relocates the ones GNU as
            // does and refuses the rest, rather than being measured from the
            // field as if the subtrahend were not there.
            if self.options.relocatable
                && match v.plus {
                    Some(p) => {
                        v.minus.is_some()
                            || self.symbol_section(p) != Some(section)
                            || self.defers_to_linker(e, p, kind, section, fi)
                    }
                    None => {
                        v.minus.is_none()
                            && self
                                .frag_arch(section.0 as usize, fi)
                                .0
                                .pcrel_number_is_address()
                    }
                }
            {
                return None;
            }
            let mut target = self.resolve_value(v)?;
            let base = self.section(section).addr as i64;
            let mask = !(kind.pc_align.max(1) as i64 - 1);
            // A plain number, on a target where it is an offset into the
            // section rather than an address, is resolved in an object with
            // no relocation, so a linker placing the section keeps the
            // distance: a flat image measures it from the section's start
            // too.
            let offset = v.plus.is_none()
                && v.minus.is_none()
                && !self
                    .frag_arch(section.0 as usize, fi)
                    .0
                    .pcrel_number_is_address();
            if offset {
                target += base;
            }
            // A reference within its own section is one the assembler
            // resolves before any linker places the section, so the PC is
            // rounded from the section's start, as GNU as rounds it; that
            // differs only for a section a linker puts at an address that is
            // not itself a multiple of the rounding.
            let here = if offset
                || v.plus
                    .is_some_and(|p| self.symbol_section(p) == Some(section))
            {
                base + ((at as i64 + kind.adjust as i64) & mask)
            } else {
                (base + at as i64 + kind.adjust as i64) & mask
            };
            return Some(target - here);
        }
        // The distance between two labels in one section is fixed no matter
        // where the linker puts that section, so it resolves even in
        // relocatable output. In a flat image every label already has its
        // final address, so a distance across sections is fixed too.
        if let (Some(p), Some(m)) = (v.plus, v.minus) {
            let (ps, ms) = (self.symbol_section(p), self.symbol_section(m));
            // Unless the target's linker may move the labels apart; see
            // `Architecture::defers_difference`.
            let deferred = |asm: &Self| {
                asm.options.relocatable
                    && ps.is_some_and(|s| {
                        let flags = asm.section(s).flags;
                        let arch = asm.frag_arch(section.0 as usize, fi).0;
                        arch.defers_difference(kind, &flags, asm.code_moves_for(s, p, m))
                    })
            };
            if ps.is_some() && (ps == ms || !self.options.relocatable) && !deferred(self) {
                // Unless the linker may move them apart: a Mach-O object
                // keeps a difference between two atoms for the linker.
                if self.macho_object() && !crate::output::macho::folds_difference(self, p, m) {
                    return None;
                }
                return self.resolve_value(v);
            }
            return None;
        }
        // An absolute reference to a section-relative symbol can only be
        // resolved here when the output is not going to be relocated, and
        // one the linker is to fill in in any case, not even a number.
        if (!v.is_absolute() || kind.object_reloc) && self.options.relocatable {
            return None;
        }
        self.resolve_value(v)
    }

    // ---- writing ----------------------------------------------------------

    fn apply_fixups(&mut self) {
        let mut relocs = Vec::new();
        // Under a REL psABI the addend lives in the field being relocated
        // rather than in the relocation entry, so it has to be written here
        // while the field is still reachable. Relocation numbering is ELF's
        // throughout, so asking the ELF writer which convention the target
        // uses is consistent rather than a layering slip.
        let rela = crate::output::elf::uses_rela(
            self.target().elf_machine(),
            crate::output::elf::is_elf64(self.target()),
        );
        // COFF keeps every addend in the field, and numbers its relocations
        // its own way; the translation happens here, where a field that COFF
        // cannot describe still has a span to blame.
        let coff = self
            .options
            .format
            .is_coff()
            .then(|| crate::output::coff::machine(self.target()))
            .flatten();
        for si in 0..self.sections.len() {
            let id = SectionId(si as u32);
            for fi in 0..self.sections[si].frags.len() {
                let frag_off = self.sections[si].frags[fi].offset;
                let list: Vec<(u32, ExprRef, FixupKind, Span)> =
                    match &self.sections[si].frags[fi].kind {
                        FragKind::Bytes { variants, chosen } => variants[*chosen]
                            .fixups
                            .iter()
                            .map(|f| (f.offset, f.expr, f.kind, f.span))
                            .collect(),
                        FragKind::Leb128 {
                            value,
                            signed: false,
                            ..
                        } if self.options.relocatable => {
                            let value = *value;
                            relocs.extend(self.uleb128_relocations(value, id, fi, frag_off));
                            continue;
                        }
                        FragKind::Leb128 {
                            value,
                            signed: true,
                            ..
                        } if self.options.relocatable => {
                            let (value, span) = (*value, self.sections[si].frags[fi].span);
                            self.refuse_signed_leb128_difference(value, id, fi, span);
                            continue;
                        }
                        _ => continue,
                    };
                for (off, e, mut kind, span) in list {
                    let at = frag_off + off as u64;
                    // A branch that becomes another instruction is rewritten
                    // before its field is filled in, and filled in as the new
                    // instruction's.
                    if let LinkValue::Interwork(class) = kind.link
                        && let Interwork::Becomes { patch, kind: k } =
                            self.interwork(e, class, id, fi)
                    {
                        let endian = self.frag_arch(si, fi).0.endian();
                        if let FragKind::Bytes { variants, chosen } =
                            &mut self.sections[si].frags[fi].kind
                        {
                            let dst = &mut variants[*chosen].bytes
                                [off as usize..off as usize + kind.size as usize];
                            endian.write(dst, patch(endian.read(dst)));
                        }
                        kind = k;
                    }
                    let mut value = self.fixup_value(e, &kind, id, fi, at);
                    // A reference a relaxing linker may have to work out
                    // again picks the instruction from the distance as it
                    // stands and is then left to the linker; see
                    // `FixupKind::relocated_when_relaxed`. The range is not
                    // checked, because the distance the linker finally writes
                    // is not this one and GNU as says as much ("assume jumps
                    // are in range; the linker will catch any that aren't").
                    // Only a reference to a symbol, at that: a bare
                    // displacement names no place a linker could move, and
                    // GNU as resolves one outright.
                    if value.is_some()
                        && kind.relocated_when_relaxed
                        && self.options.relocatable
                        && self.eval(e).is_ok_and(|v| v.plus.is_some())
                    {
                        value = None;
                    }
                    match value {
                        Some(v) => {
                            if let Some(msg) = self.size_modifier_overflow(e, &kind, id, fi, v) {
                                self.diags.error(span, msg);
                                continue;
                            }
                            if !kind.fits(v as i128) {
                                self.diags.error(span, range_message(&kind, v));
                                continue;
                            }
                            let endian = self.frag_arch(si, fi).0.endian();
                            if let FragKind::Bytes { variants, chosen } =
                                &mut self.sections[si].frags[fi].kind
                            {
                                let dst = &mut variants[*chosen].bytes
                                    [off as usize..off as usize + kind.size as usize];
                                kind.write(endian, dst, v);
                            }
                        }
                        // The label is made only now, once it is certain to
                        // be needed, so a resolved `la` leaves no trace in the
                        // symbol table. Without a linker this half says
                        // nothing: the other half of the pair has the same
                        // expression, and has already said why it failed.
                        None if kind.reloc_symbol == RelocSymbol::FragmentStart => {
                            if !self.options.relocatable {
                                continue;
                            }
                            debug_assert_eq!(
                                off as i64 + kind.adjust as i64,
                                0,
                                "a fragment-start relocation must measure from the fragment's start"
                            );
                            let label = self.fragment_label(id, fi as u32, span);
                            relocs.push(Relocation {
                                section: id,
                                offset: at,
                                symbol: Some(label),
                                addend: 0,
                                kind: kind.reloc,
                                desc: RelocDesc::of(&kind),
                            });
                            relocs.extend(marker_relocation(&kind, id, at));
                        }
                        // A Mach-O field is filled in by the writer, which
                        // alone knows what each relocation will name.
                        None if self.macho_object() => {
                            relocs.extend(self.macho_relocation(e, &kind, id, fi, at, span));
                        }
                        None => {
                            let leftover = self.relocated_field(e, &kind, id, fi, at);
                            if let Some(v) = leftover {
                                let endian = self.frag_arch(si, fi).0.endian();
                                if let FragKind::Bytes { variants, chosen } =
                                    &mut self.sections[si].frags[fi].kind
                                {
                                    let dst = &mut variants[*chosen].bytes
                                        [off as usize..off as usize + kind.size as usize];
                                    kind.write(endian, dst, v);
                                }
                            }
                            for mut r in self.build_relocation(e, &kind, id, fi, at, span) {
                                if let Some(machine) = coff {
                                    if !self
                                        .coff_relocation(machine, &mut r, &kind, si, fi, off, span)
                                    {
                                        continue;
                                    }
                                    relocs.push(r);
                                    continue;
                                }
                                let (arch, _) = self.frag_arch(si, fi);
                                // An addend of zero still has to be written
                                // where the field is not a plain integer: a
                                // Thumb `b.w` encodes a displacement of zero
                                // with its `J1` and `J2` bits set, and the
                                // bits an instruction template left there
                                // would be read back as some other distance.
                                if arch.addend_in_field(r.kind, rela)
                                    && (r.addend != 0
                                        || !matches!(
                                            kind.encoding,
                                            crate::section::FieldEncoding::Whole
                                        ))
                                {
                                    // A byte or word field has no room for a
                                    // larger addend, which GNU as refuses
                                    // rather than truncate. It reads a 32-bit
                                    // one as signed first, as `0xffffffff`
                                    // for -1.
                                    let bits = kind.size as u32 * 8;
                                    let addend = if (0..=0xffff_ffff).contains(&r.addend) {
                                        r.addend as i32 as i64
                                    } else {
                                        r.addend
                                    };
                                    // A scattered field says its own bounds,
                                    // since its width is not its bytes'; see
                                    // `FixupKind::addend_limits`.
                                    let narrow = match kind.addend_limits {
                                        Some((lo, hi)) => !(lo..=hi).contains(&addend),
                                        None => {
                                            bits <= 16
                                                && matches!(
                                                    kind.encoding,
                                                    crate::section::FieldEncoding::Whole
                                                )
                                                && !(-(1i64 << (bits - 1))..(1i64 << bits))
                                                    .contains(&addend)
                                        }
                                    };
                                    if !rela && narrow {
                                        let where_ = match kind.addend_limits {
                                            Some((lo, hi)) => format!(
                                                "the field it is relocated in, which holds \
                                                 {lo} to {hi}"
                                            ),
                                            None => format!(
                                                "the {}-byte field it is relocated in",
                                                kind.size
                                            ),
                                        };
                                        self.diags.error(
                                            span,
                                            format!(
                                                "value {:#x} does not fit in {where_}",
                                                r.addend
                                            ),
                                        );
                                        continue;
                                    }
                                    let field = if rela {
                                        r.addend
                                    } else {
                                        let value = r.symbol.map_or(0, |s| self.named_value(s));
                                        let by_section = r.symbol.is_some_and(|s| {
                                            self.symbols.get(s).ty
                                                == crate::symbol::SymType::Section
                                        });
                                        arch.rel_field(r.kind, r.addend, value, by_section)
                                    };
                                    let endian = arch.endian();
                                    if let FragKind::Bytes { variants, chosen } =
                                        &mut self.sections[si].frags[fi].kind
                                    {
                                        let dst = &mut variants[*chosen].bytes
                                            [off as usize..off as usize + kind.size as usize];
                                        kind.write(endian, dst, field);
                                    }
                                    if rela {
                                        r.addend = 0;
                                    }
                                } else if r.addend != 0
                                    && arch.local_value_in_field(r.kind)
                                    && r.symbol.is_some_and(|s| {
                                        self.symbols.get(s).ty == crate::symbol::SymType::Section
                                    })
                                {
                                    let endian = arch.endian();
                                    if let FragKind::Bytes { variants, chosen } =
                                        &mut self.sections[si].frags[fi].kind
                                    {
                                        let dst = &mut variants[*chosen].bytes
                                            [off as usize..off as usize + kind.size as usize];
                                        kind.write(endian, dst, r.addend);
                                    }
                                }
                                relocs.push(r);
                            }
                        }
                    }
                }
            }
        }
        self.relocs = relocs;
    }

    /// Reports a `.sleb128` of a difference the linker would have to work
    /// out, on a target that refuses one; see
    /// [`Architecture::refuses_signed_leb128_difference`].
    fn refuse_signed_leb128_difference(
        &mut self,
        value: ExprRef,
        section: SectionId,
        fi: usize,
        span: Span,
    ) {
        let Ok(v) = self.eval(value) else {
            return;
        };
        let (Some(plus), Some(minus)) = (v.plus, v.minus) else {
            return;
        };
        let (ps, ms) = (self.symbol_section(plus), self.symbol_section(minus));
        let Some(sec) = ps.filter(|_| ps == ms) else {
            return;
        };
        let flags = self.section(sec).flags;
        let moves = self.code_moves_for(sec, plus, minus);
        let arch = self.frag_arch(section.0 as usize, fi).0;
        if !arch.refuses_signed_leb128_difference()
            || arch.uleb128_difference_relocs(&flags, moves).is_none()
        {
            return;
        }
        self.diags.emit(
            crate::diag::Diagnostic::error(
                span,
                "this distance is not known until the link, and a `.sleb128` has no \
                 relocation for one",
            )
            .with_help("write it as a `.uleb128`, or put `.option norelax` over the code between the two labels"),
        );
    }

    /// The relocations that leave a `.uleb128` of a difference of labels to
    /// the linker, on a target that has them; see
    /// [`Architecture::uleb128_difference_relocs`]. The field keeps the value
    /// layout computed.
    ///
    /// [`Architecture::uleb128_difference_relocs`]: crate::arch::Architecture::uleb128_difference_relocs
    fn uleb128_relocations(
        &mut self,
        value: ExprRef,
        section: SectionId,
        fi: usize,
        at: u64,
    ) -> Vec<Relocation> {
        let Ok(v) = self.eval(value) else {
            return Vec::new();
        };
        let (Some(plus), Some(minus)) = (v.plus, v.minus) else {
            return Vec::new();
        };
        let (ps, ms) = (self.symbol_section(plus), self.symbol_section(minus));
        let Some(sec) = ps.filter(|_| ps == ms) else {
            return Vec::new();
        };
        let flags = self.section(sec).flags;
        let si = section.0 as usize;
        let moves = self.code_moves_for(sec, plus, minus);
        let Some((sub, set)) = self
            .frag_arch(si, fi)
            .0
            .uleb128_difference_relocs(&flags, moves)
        else {
            return Vec::new();
        };
        // Both halves name their label, as a data pair's do; see
        // `Assembler::build_relocation`.
        let kind = FixupKind::data(0).with_reloc_symbol(RelocSymbol::Symbol);
        let mut out = Vec::new();
        // GNU as for MSP430 converts the subtrahend's fixup first and gives
        // both halves whatever the expression added; the RISC-V references
        // write the half that sets the field first and leave the subtrahend's
        // addend at zero, as the halves of a data pair do. These pairs exist
        // nowhere else, so one flag tells the two shapes apart.
        let halves = if self.frag_arch(si, fi).0.difference_subtrahend_first() {
            [(minus, sub, v.addend), (plus, set, v.addend)]
        } else {
            [(plus, set, v.addend), (minus, sub, 0)]
        };
        for (target, mut reloc, mut addend) in halves {
            let symbol =
                self.relocation_symbol(target, &kind, si, fi, false, &mut addend, &mut reloc);
            out.push(Relocation {
                section,
                offset: at,
                symbol: Some(symbol),
                addend,
                kind: reloc,
                desc: RelocDesc::of(&kind),
            });
        }
        out
    }

    /// The relocations that leave a fixup to the linker: one, or on a target
    /// that writes a difference as a pair, two. Empty after reporting why
    /// there can be none.
    fn build_relocation(
        &mut self,
        e: ExprRef,
        kind: &FixupKind,
        section: SectionId,
        fi: usize,
        at: u64,
        span: Span,
    ) -> Vec<Relocation> {
        let si = section.0 as usize;
        // A modifier can imply a symbol of its own, which GNU as creates as it
        // reads the modifier, before the target's.
        let effects = self
            .modifier_chain(e)
            .filter(|_| self.options.dialect != crate::lexer::Dialect::Nasm)
            // A COFF object has no GOT for a modifier to imply.
            .filter(|_| !self.options.format.is_coff())
            .map(|name| self.frag_arch(si, fi).0.modifier_symbols(&name));
        if let Some(needs) = effects.and_then(|x| x.needs) {
            let name = self.interner.intern(needs);
            let id = self.symbols.intern(name, span);
            self.symbols.get_mut(id).used = true;
        }
        let v = match self.eval(e) {
            Ok(v) => v,
            Err(err) => match self.eval_with_distances(e) {
                Some(v) => v,
                None => {
                    self.diags.emit(err.into_diagnostic());
                    return Vec::new();
                }
            },
        };
        // A thread-local access names a variable whose value only the linker
        // works out, so GNU as refuses one it can already see is not such a
        // variable: a number, a difference, a function, or a symbol defined
        // outside a thread-local section. An undefined symbol is taken to be
        // one, and is made one below. A mark on an instruction
        // (`.tlsdesccall`) makes no such claim, and names whatever it is
        // given.
        let tls_access = effects.is_some_and(|x| x.tls)
            || (kind.class == crate::reloc::RelocClass::ThreadLocal && kind.size > 0);
        if tls_access {
            let refusal = match (v.plus, v.minus) {
                (Some(t), None) if self.symbols.get(t).ty == crate::symbol::SymType::Func => {
                    Some(format!(
                        "`{}` is a function, and is accessed as a thread-local variable",
                        self.display_name(t)
                    ))
                }
                (Some(t), None) => (self.symbols.get(t).is_defined() && !self.is_thread_local(t))
                    .then(|| {
                        format!(
                            "`{}` is accessed as a thread-local variable, but is defined \
                             outside a thread-local section",
                            self.display_name(t)
                        )
                    }),
                _ => Some(
                    "a thread-local access needs a variable, not a number or a difference".into(),
                ),
            };
            if let Some(msg) = refusal {
                self.diags.error(span, msg);
                return Vec::new();
            }
        }
        // GNU as puts a mark on the four bytes after it, and refuses one with
        // nothing after it to mark.
        if kind.size == 0 && at >= self.section(section).size {
            self.diags.error(
                span,
                "this marks the instruction after it, and nothing follows it in the section",
            );
            return Vec::new();
        }
        if !self.options.relocatable
            && let Some(msg) = self.flat_refusal(e, kind, section, fi, at)
        {
            self.diags.error(span, msg);
            return Vec::new();
        }
        let mut kind = *kind;
        let mut v = v;
        // The subtrahend's half of a relocation pair, for a target that
        // relocates a difference that way.
        let mut subtrahend = None;
        if let (Some(_), Some(minus), false, true) =
            (v.plus, v.minus, kind.pcrel, self.options.relocatable)
            && let Some((add, sub)) = self.frag_arch(si, fi).0.difference_relocs(&kind)
        {
            // Both halves name their label rather than its section and an
            // offset: a linker that deletes bytes moves the labels after them
            // and adjusts the symbols it knows, and leaves an offset into a
            // section as it found it, so the pair would then stand for the
            // distance as it was before the link. GNU as says the same for
            // every local relocation on the targets that have these pairs,
            // with `TC_FORCE_RELOCATION_LOCAL`.
            kind.reloc_symbol = RelocSymbol::Symbol;
            let mut addend = 0;
            let mut reloc = sub;
            let symbol =
                self.relocation_symbol(minus, &kind, si, fi, false, &mut addend, &mut reloc);
            subtrahend = Some(Relocation {
                section,
                offset: at,
                symbol: Some(symbol),
                addend,
                kind: reloc,
                desc: RelocDesc::of(&kind),
            });
            v.minus = None;
            kind.reloc = add;
        }
        // Two labels in one section are a fixed distance apart whatever a
        // linker does with the section, so GNU as folds the difference once
        // the sections are sized. In a PC-relative field that fold comes too
        // late for the number to be the offset a number written there would
        // be — the backend decides that while it reads the line, see
        // `arch::fold_differences` — so what is left is an address, relocated
        // against no symbol.
        if let (Some(p), Some(m)) = (v.plus, v.minus)
            && kind.pcrel
            && self.symbol_section(p).is_some()
            && self.symbol_section(p) == self.symbol_section(m)
            && let (Some(pa), Some(ma)) = (self.symbol_addr(p), self.symbol_addr(m))
        {
            v.addend += pa - ma;
            v.plus = None;
            v.minus = None;
        }
        if let Some(minus) = v.minus {
            // `sym - label`, with the label in the fixup's own section, is
            // `sym` relative to the field plus a known distance, which a
            // plain data field can carry as a PC-relative relocation. This is
            // how `.long target - .` jump tables and unwind data are written.
            // A field that is PC-relative already measures from the same
            // place, so it needs no counterpart, only the distance: GNU as
            // writes `adr x0, there - here` as an ordinary `adr` relocation
            // on `there` with `here`'s distance from the field as its addend.
            let here = self.section(section).addr as i64 + at as i64;
            let pcrel = if self.symbol_section(minus) != Some(section) {
                None
            } else if kind.pcrel {
                Some(kind.reloc)
            } else {
                self.frag_arch(si, fi).0.pcrel_reloc(kind.reloc, kind.size)
            };
            let (Some(r), Some(label)) = (pcrel, self.symbol_addr(minus)) else {
                // A field whose relocation has no PC-relative counterpart —
                // ARM's `sym(GOT)` is one — cannot carry a difference even
                // where both symbols are here.
                let msg = if self.symbol_section(minus) == Some(section) {
                    "this field has no relocation for a difference of symbols"
                } else {
                    "the difference of two symbols in different sections cannot be relocated"
                };
                self.diags.error(span, msg);
                return Vec::new();
            };
            v.addend += here - label;
            v.minus = None;
            kind.pcrel = true;
            kind.reloc = r;
        }
        let kind = &kind;
        // Whatever `.type` said, GNU as makes the target of a thread-local
        // access thread-local; the function it refuses above.
        if tls_access
            && let Some(t) = v.plus
            && matches!(
                self.symbols.get(t).ty,
                crate::symbol::SymType::NoType | crate::symbol::SymType::Object
            )
        {
            self.symbols.get_mut(t).ty = crate::symbol::SymType::Tls;
        }
        let target = match v.plus {
            Some(t) => Some(t),
            // A PC-relative reference to a plain number, or a number left to
            // the linker, relocated against no symbol at all (ELF symbol 0).
            None if (kind.pcrel || kind.object_reloc)
                && v.minus.is_none()
                && self.options.relocatable =>
            {
                None
            }
            None => {
                self.diags.error(span, "cannot resolve this value");
                return Vec::new();
            }
        };
        if !self.options.relocatable && kind.always_reloc {
            self.diags.error(
                span,
                "this reference is resolved by the linker, which a flat binary does not have",
            );
            return Vec::new();
        }
        if !self.options.relocatable {
            let name = target.map_or_else(String::new, |t| self.display_name(t));
            self.diags.error(span, format!("undefined symbol `{name}`"));
            return Vec::new();
        }
        // Relocation numbers mean something only to one machine, and the
        // linker applies them in the object's byte order and word size, so
        // code for another target in the file cannot be relocated.
        let (arch, out) = (self.frag_arch(si, fi).0, self.target());
        if arch.elf_machine() != out.elf_machine()
            || arch.endian() != out.endian()
            || crate::output::elf::is_elf64(arch) != crate::output::elf::is_elf64(out)
        {
            let msg = format!(
                "this reference, in code for `{}`, needs a relocation, which an object \
                 for `{}` cannot hold",
                arch.name(),
                out.name()
            );
            self.diags.emit(
                crate::diag::Diagnostic::error(span, msg)
                    .with_help("resolve it within the file, or assemble this code on its own"),
            );
            return Vec::new();
        }

        // A modifier anywhere in the expression selects the relocation. The
        // COFF-only ones (`@IMGREL`, `@SECREL32`) name what the field holds
        // rather than a relocation number, which no psABI has for them, so
        // they come through as a class the COFF writer reads.
        let coff_class = self
            .options
            .format
            .is_coff()
            .then(|| {
                self.find_modifier(e)
                    .and_then(|m| crate::coff::modifier_class(self.interner.get(m)))
            })
            .flatten();
        // A modifier the backend has no relocation for is refused, as it is
        // in a data directive: leaving the plain relocation behind would turn
        // `movl x@FOOBAR(%rip), %eax` into an ordinary reference to `x`,
        // which is a different program, and GNU as refuses the junk after the
        // expression rather than assembling it.
        let reloc = match self.find_modifier(e).filter(|_| coff_class.is_none()) {
            Some(m) => {
                let name = self.interner.get(m).to_string();
                let arch = self.frag_arch(si, fi).0;
                match arch.fixup_modifier_reloc(&name, kind) {
                    crate::arch::FixupModifier::Reloc(r) => r,
                    crate::arch::FixupModifier::Encoded => kind.reloc,
                    crate::arch::FixupModifier::Unknown => {
                        let written = crate::arch::written_modifier(arch, &name);
                        self.diags.error(
                            span,
                            format!(
                                "{written} is not a relocation modifier the `{}` backend \
                                 supports on this operand",
                                arch.name()
                            ),
                        );
                        return Vec::new();
                    }
                }
            }
            None => kind.reloc,
        };
        let place = if self.relocs_by_fragment.contains(&section) {
            at - self.sections[si].frags[fi].offset
        } else {
            at
        };
        // And what the symbol makes of it, for the few relocations a target
        // picks by the name a reference happens to carry.
        let reloc = match target {
            Some(t) => {
                let name = self.symbols.get(t).name;
                let arch = self.frag_arch(si, fi).0;
                arch.reloc_for_symbol(reloc, self.interner.get(name))
            }
            None => reloc,
        };
        let reloc = self.frag_arch(si, fi).0.reloc_at(reloc, place);
        if reloc == 0 {
            self.diags.error(
                span,
                format!(
                    "no relocation exists for a {}-byte {}reference",
                    kind.size,
                    if kind.pcrel { "PC-relative " } else { "" }
                ),
            );
            return Vec::new();
        }

        let bias = if kind.pcrel && kind.bias_reloc_addend {
            kind.adjust as i64
        } else {
            0
        };
        let mut addend = v.addend - bias;
        let mut reloc = reloc;
        // A modifier that asks the linker for an entry of the target's own
        // names it rather than its section, since that is what the linker
        // looks up. In NASM source every GOT, PLT or `..sym` modifier does;
        // elsewhere the backend says which of its own do.
        let names_symbol = effects.is_some_and(|x| x.names_target)
            || (self.options.dialect == crate::lexer::Dialect::Nasm
                && self.find_modifier(e).is_some_and(|m| {
                    matches!(
                        self.interner.get(m),
                        "got" | "gotpcrel" | "plt" | "sym" | "gotoff" | "gotpc"
                    )
                }));
        let symbol = target.map(|t| {
            self.relocation_symbol(t, kind, si, fi, names_symbol, &mut addend, &mut reloc)
        });

        let mut desc = RelocDesc::of(kind);
        if let Some(class) = coff_class {
            desc.class = class;
        }
        let mut relocs = vec![Relocation {
            section,
            offset: at,
            symbol,
            addend,
            kind: reloc,
            desc,
        }];
        if symbol.is_some() {
            relocs.extend(marker_relocation(kind, section, at));
        }
        match subtrahend {
            Some(sub) if self.frag_arch(si, fi).0.difference_subtrahend_first() => {
                relocs.insert(0, sub)
            }
            sub => relocs.extend(sub),
        }
        relocs
    }

    /// The symbol a relocation of `kind` against `target` names, adjusting
    /// the addend and relocation type to suit.
    ///
    /// Local symbols are relocated against their section, which is what
    /// linkers expect and what keeps local labels out of the symbol table.
    /// So is a local alias of a label, even of a global one, and on some
    /// targets a global symbol too.
    ///
    /// NASM goes further: it relocates a reference to any symbol defined in
    /// the module against its section, keeping only external symbols by name,
    /// unless a modifier (`wrt ..plt`, `..got`, `..sym`) names the symbol.
    #[allow(clippy::too_many_arguments)]
    fn relocation_symbol(
        &mut self,
        target: SymbolId,
        kind: &FixupKind,
        si: usize,
        fi: usize,
        names_symbol: bool,
        addend: &mut i64,
        reloc: &mut u32,
    ) -> SymbolId {
        let binding = self.symbols.get(target).binding;
        let arch = self.frag_arch(si, fi).0;
        // A target may need the linker to see the symbol itself: an ARM
        // function, whose instruction set a linker reads from it, a
        // thread-local variable, whose section symbol is not itself
        // thread-local and so cannot stand in for it, and one a modifier
        // asks the linker for an entry of, which no section symbol and
        // offset can name; see `ModifierSymbols::names_target`.
        let sym = self.symbols.get(target);
        let keep = arch.keeps_reloc_symbol(sym.target_flags, sym.ty)
            || self.is_thread_local(target)
            || names_symbol;
        // NASM's rule holds for its COFF objects too. Otherwise COFF keeps
        // the local symbols the source named, and llvm-mc relocates against
        // them by name; only the assembler's own labels, which never reach
        // the symbol table, go through their section.
        let by_section = !keep
            && if self.options.dialect == crate::lexer::Dialect::Nasm {
                binding == Binding::Local || self.symbols.get(target).is_defined()
            } else if self.options.format.is_coff() {
                !crate::coff::keeps_symbol(self, target)
            } else {
                match binding {
                    Binding::Local => true,
                    Binding::Global => arch.relocates_globals_by_section(),
                    Binding::Weak => false,
                }
            };
        match self.symbol_section(target) {
            // Unless the target's reference names the label for this
            // relocation; see `Architecture::relocates_with_label`.
            Some(sec)
                if by_section
                    && kind.reloc_symbol == RelocSymbol::Section
                    && !(binding == Binding::Local && arch.relocates_with_label(*reloc)) =>
            {
                if kind.pcrel && binding == Binding::Local {
                    *reloc = arch.section_relative_reloc(*reloc);
                }
                *addend += self.symbol_addr(target).unwrap_or(0) - self.section(sec).addr as i64;
                self.section_symbol(sec)
            }
            _ => {
                self.symbols.get_mut(target).used = true;
                target
            }
        }
    }

    /// Evaluates `e` for a relocation with every difference of two labels in
    /// one section inside it taken as the distance it now is, for a value
    /// ordinary evaluation refuses because it holds a second symbol.
    ///
    /// The PIC idioms add a distance to the symbol they relocate: ARM's
    /// `.word x(TLSGD) + (. - .LPIC0 - 8)` is `x` plus the offset from the
    /// `add rn, pc, rn` at `.LPIC0` to the word, which the linker needs to
    /// find the GOT entry from the PC. GNU as folds that distance while it
    /// reads the line, or once the fragments between are sized, and
    /// relocates `x` with it as the addend. Only the parts of a sum are
    /// folded, never the whole value: `sym - label` on its own is a
    /// PC-relative reference, which has its own relocation.
    fn eval_with_distances(&mut self, e: ExprRef) -> Option<Value> {
        let node = self.exprs.get(e);
        let span = node.span;
        match node.kind {
            ExprKind::Binary(op @ (expr::BinOp::Add | expr::BinOp::Sub), l, r) => {
                let l = self.eval_with_distances(l)?;
                let r = self.eval_with_distances(r)?;
                let (l, r) = (self.fold_distance(l)?, self.fold_distance(r)?);
                expr::eval_binary(op, l, r, span).ok()
            }
            ExprKind::Modifier(_, inner) => self.eval_with_distances(inner),
            _ => self.eval(e).ok(),
        }
    }

    /// `v` as a number if it is the difference of two labels in one section.
    fn fold_distance(&self, v: Value) -> Option<Value> {
        match (v.plus, v.minus) {
            (Some(p), Some(m))
                if self.symbol_section(p).is_some()
                    && self.symbol_section(p) == self.symbol_section(m) =>
            {
                let d = self.symbol_addr(p)? - self.symbol_addr(m)?;
                Some(Value::abs(v.addend.wrapping_add(d)))
            }
            _ => Some(v),
        }
    }

    /// The first `@`-modifier appearing in an expression, if any.
    pub(crate) fn find_modifier(&self, e: ExprRef) -> Option<crate::intern::Name> {
        match &self.exprs.get(e).kind {
            ExprKind::Modifier(n, _) => Some(*n),
            ExprKind::Unary(_, a) => self.find_modifier(*a),
            ExprKind::Binary(_, a, b) => self.find_modifier(*a).or_else(|| self.find_modifier(*b)),
            _ => None,
        }
    }

    /// The first `@`-modifier in an expression together with what it is
    /// written on. The operand matters to a modifier that reads something
    /// stated about the symbol rather than its value, since evaluating the
    /// whole expression has by then replaced a `.set` alias with what it
    /// stands for.
    fn modifier_operand(&self, e: ExprRef) -> Option<(Name, ExprRef)> {
        match &self.exprs.get(e).kind {
            ExprKind::Modifier(n, inner) => Some((*n, *inner)),
            ExprKind::Unary(_, a) => self.modifier_operand(*a),
            ExprKind::Binary(_, a, b) => self
                .modifier_operand(*a)
                .or_else(|| self.modifier_operand(*b)),
            _ => None,
        }
    }

    /// The symbol an expression names outright, rather than the one it works
    /// out to.
    fn named_symbol(&self, e: ExprRef) -> Option<SymbolId> {
        match &self.exprs.get(e).kind {
            ExprKind::Sym(n) => self.symbols.lookup(*n),
            ExprKind::SymId(id) => Some(*id),
            _ => None,
        }
    }

    /// Where an expression names the symbol it is written around, which is
    /// what a `.set` alias copies its size from: `sym`, `sym + 4` and
    /// `4 + sym` all name `sym`, while a difference of two symbols names
    /// neither. That is the symbol the source mentions, not the one the
    /// expression works out to, so an alias of an alias copies from the
    /// alias.
    pub(crate) fn head_symbol(&self, e: ExprRef) -> Option<ExprRef> {
        match &self.exprs.get(e).kind {
            ExprKind::Sym(_) | ExprKind::SymId(_) => Some(e),
            ExprKind::Binary(expr::BinOp::Add, a, b) => {
                self.head_symbol(*a).xor(self.head_symbol(*b))
            }
            ExprKind::Binary(expr::BinOp::Sub, a, b) => self
                .head_symbol(*b)
                .is_none()
                .then(|| self.head_symbol(*a))
                .flatten(),
            _ => None,
        }
    }

    /// The size the file gives a symbol: the block a `.comm` or `.lcomm`
    /// reserved for it, what `.size` said, or the size of the symbol a
    /// `.set` made it an alias of before that symbol was defined.
    /// Not API.
    #[doc(hidden)]
    pub fn symbol_size(&self, id: SymbolId) -> Option<i64> {
        let mut id = id;
        // One hop is all `dir_set` ever records, since it follows a source
        // only while that source is undefined and defining it ends the
        // chain; the loop is there so nothing can spin on a chain anyway.
        for _ in 0..64 {
            let sym = self.symbols.get(id);
            if let SymbolValue::Common { size, .. } = sym.value {
                return Some(size as i64);
            }
            if let Some(e) = sym.size {
                return self.eval_const(e);
            }
            id = sym.size_from?;
        }
        None
    }

    /// True when the active backend reads a size modifier in this
    /// expression, which is what keeps the constant folds in front of the
    /// fixup from swallowing one.
    pub(crate) fn names_size_modifier(&self, e: ExprRef) -> bool {
        self.find_modifier(e)
            .is_some_and(|m| self.arch.modifier_is_size(self.interner.get(m)))
    }

    /// A size modifier written in this expression — x86's `@SIZE`, so far the
    /// only one — as its name and what it is written on.
    fn size_modifier(&self, e: ExprRef, section: SectionId, fi: usize) -> Option<(Name, ExprRef)> {
        let (name, operand) = self.modifier_operand(e)?;
        self.frag_arch(section.0 as usize, fi)
            .0
            .modifier_is_size(self.interner.get(name))
            .then_some((name, operand))
    }

    /// What a size modifier — x86's `@SIZE`, so far the only one — comes to
    /// where the file settles it, and `None` where the linker has to.
    ///
    /// GNU as works out the size of a symbol the file both defines and keeps
    /// to itself, and writes the number into the field with no relocation at
    /// all: nothing after this can change it. A symbol the file exports may
    /// be replaced at link time by a definition of another size, so that one
    /// is relocated however plainly sized it looks here. A weak definition is
    /// not, which is GNU as reading `STB_GLOBAL` rather than asking what a
    /// linker may preempt. An undefined symbol has no size here at all, and
    /// one `.set` to a number is no longer a symbol to ask: GNU as calls
    /// `n@SIZE` an invalid expression after `.set n, 7`, and refusing it
    /// falls out of leaving it to a relocation that nothing can name.
    ///
    /// Which symbol's size is read is the one the modifier is written on,
    /// not the one the expression works out to: GNU as copies a symbol's size
    /// onto a `.set` alias of it, so after `.set a, sym + 4` the size
    /// `a@SIZE` reads is `sym`'s, with the 4 no part of it -- see
    /// [`Assembler::dir_set`], which does the copying. What the expression adds
    /// around the modifier is part of it, in the field under a REL psABI and
    /// in the addend under a RELA one -- except that a folded reference has
    /// no relocation to carry an addend, so both end up in the field.
    fn folded_symbol_size(&mut self, e: ExprRef, operand: ExprRef) -> Option<i64> {
        let target = self.named_symbol(operand)?;
        match self.symbols.get(target).value {
            SymbolValue::Label { .. } | SymbolValue::Common { .. } => {}
            SymbolValue::Expr(x) => {
                self.eval_ref(x).ok()?.plus?;
            }
            SymbolValue::Undefined => return None,
        }
        if self.symbols.get(target).binding == Binding::Global {
            return None;
        }
        let size = self.symbol_size(target).unwrap_or(0);
        // The addend is what the expression adds around the modifier, which
        // is the difference between the whole expression and its operand.
        let whole = self.eval(e).ok()?;
        let inner = self.eval(operand).ok()?;
        if whole.plus != inner.plus || whole.minus.is_some() || inner.minus.is_some() {
            return None;
        }
        let value = size.wrapping_add(whole.addend.wrapping_sub(inner.addend));
        // `st_size` is as wide as the object's words, so on a 32-bit object
        // the size, and with it the sum, is a 32-bit quantity that simply
        // wraps. A 64-bit object holds the size in full, and there GNU as
        // refuses a sum the field cannot take; see
        // [`Assembler::size_modifier_overflow`].
        Some(match crate::output::elf::is_elf64(self.target()) {
            true => value,
            false => value as u32 as i64,
        })
    }

    /// Why a folded size does not fit its field, if it does not. A size is
    /// unsigned, so a four-byte field holds 0 to 0xffffffff where the same
    /// field would take a negative `.long`; GNU as refuses the rest --
    /// "symbol size computation overflow" -- rather than wrap it. Only on a
    /// 64-bit object: a 32-bit one has nowhere to keep a size the field
    /// could not hold, so there the sum wraps instead.
    fn size_modifier_overflow(
        &self,
        e: ExprRef,
        kind: &FixupKind,
        section: SectionId,
        fi: usize,
        v: i64,
    ) -> Option<String> {
        self.size_modifier(e, section, fi)?;
        if kind.size >= 8
            || (0..=0xffff_ffff).contains(&v)
            || !crate::output::elf::is_elf64(self.target())
        {
            return None;
        }
        Some(format!(
            "a symbol's size is unsigned, and {v} does not fit in a {}-byte field",
            kind.size
        ))
    }

    /// The first `@`-modifier in an expression together with the ones it is
    /// stacked on, joined in source order: `x@got@tprel` is `got@tprel`,
    /// where [`Assembler::find_modifier`] sees only the `tprel` applied last.
    /// A chain means one relocation as a whole on PowerPC, where the last
    /// modifier on its own can mean another: `x@tprel` is a thread-local
    /// offset and `x@tprel@l` its low half.
    pub(crate) fn modifier_chain(&self, e: ExprRef) -> Option<String> {
        match &self.exprs.get(e).kind {
            ExprKind::Modifier(n, inner) => {
                let name = self.interner.get(*n);
                Some(match self.modifier_chain(*inner) {
                    Some(first) => format!("{first}@{name}"),
                    None => name.to_string(),
                })
            }
            ExprKind::Unary(_, a) => self.modifier_chain(*a),
            ExprKind::Binary(_, a, b) => {
                self.modifier_chain(*a).or_else(|| self.modifier_chain(*b))
            }
            _ => None,
        }
    }

    /// A new local label at the start of fragment `frag`, for a relocation
    /// that has to name that position; see [`RelocSymbol::FragmentStart`].
    ///
    /// Its name cannot be spelled in source, like the labels behind `1:`, and
    /// the ELF writer gives it a printable one.
    fn fragment_label(&mut self, section: SectionId, frag: u32, span: Span) -> SymbolId {
        let n = self.symbols.len();
        let name = self.interner.intern(&format!(".L\u{0}frag.{n}"));
        let id = self.symbols.intern(name, span);
        let sym = self.symbols.get_mut(id);
        sym.value = SymbolValue::Label { section, frag };
        sym.def_span = span;
        id
    }

    /// The symbol standing for a whole section, created on first use.
    pub(crate) fn section_symbol(&mut self, id: SectionId) -> SymbolId {
        if let Some(s) = self.section(id).sym {
            return s;
        }
        let name = self.section(id).name;
        let sym = self.symbols.intern_section(name, id);
        self.section_mut(id).sym = Some(sym);
        sym
    }

    /// Turns alignment, `.org` and `.space` fragments into real bytes so the
    /// output writers only ever see byte runs.
    fn materialize(&mut self) {
        for si in 0..self.sections.len() {
            if self.sections[si].kind == SectionKind::Nobits {
                continue;
            }
            let exec = self.sections[si].flags.exec;
            for fi in 0..self.sections[si].frags.len() {
                let size = self.sections[si].frags[fi].size() as usize;
                let bytes = match &self.sections[si].frags[fi].kind {
                    FragKind::Bytes { .. } => continue,
                    FragKind::Align {
                        fill, nop_state, ..
                    } => {
                        // No-ops in code, and wherever an instruction asked
                        // for them, which a data section can hold too.
                        if fill.is_empty() && (exec || nop_state.is_some()) {
                            let (arch, state) = self.frag_arch(si, fi);
                            let state = nop_state.as_ref().unwrap_or(state);
                            // A COFF object follows llvm-mc, whose no-ops are
                            // not GNU as's; see `output::coff::nop_fill`.
                            self.options
                                .format
                                .is_coff()
                                .then(|| crate::output::coff::nop_fill(arch, state, size))
                                .flatten()
                                .unwrap_or_else(|| arch.nop_fill(state, size as u64))
                        } else {
                            let pattern: &[u8] = if fill.is_empty() { &[0] } else { fill };
                            pattern.iter().copied().cycle().take(size).collect()
                        }
                    }
                    FragKind::Org { fill, .. } => vec![*fill; size],
                    FragKind::Space { fill, .. } => {
                        let byte = self.eval_absolute_quiet(*fill).unwrap_or(0) as u8;
                        vec![byte; size]
                    }
                    FragKind::Leb128 { encoded, .. } => encoded.clone(),
                };
                debug_assert_eq!(bytes.len(), size, "materialized fragment changed size");
                self.sections[si].frags[fi].kind = FragKind::Bytes {
                    variants: vec![crate::section::Variant::new(bytes)],
                    chosen: 0,
                };
            }
        }
    }

    /// The final bytes of a section, in order.
    pub fn section_bytes(&self, id: SectionId) -> Vec<u8> {
        let s = self.section(id);
        if s.kind == SectionKind::Nobits {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(s.size as usize);
        for f in &s.frags {
            match &f.kind {
                FragKind::Bytes { variants, chosen } => {
                    out.extend_from_slice(&variants[*chosen].bytes)
                }
                _ => out.resize(out.len() + f.size() as usize, 0),
            }
        }
        out
    }
}

/// Evaluates with every label at its address, for flat output once layout has
/// given the sections theirs; see `plain_fixup_value`.
struct AddressEnv<'a> {
    asm: &'a Assembler,
    depth: u32,
}

impl crate::expr::EvalCtx for AddressEnv<'_> {
    fn lookup_symbol(
        &mut self,
        name: crate::intern::Name,
        span: Span,
    ) -> Result<Value, crate::expr::EvalError> {
        match self.asm.symbols.lookup(name) {
            Some(id) => self.symbol_value(id, span),
            None => Err(crate::expr::EvalError::new(span, "undefined symbol")),
        }
    }

    fn symbol_value(&mut self, id: SymbolId, span: Span) -> Result<Value, crate::expr::EvalError> {
        match self.asm.symbols.get(id).value {
            SymbolValue::Expr(e) => {
                if self.depth > 64 {
                    return Err(crate::expr::EvalError::new(
                        span,
                        "symbol definition is circular",
                    ));
                }
                self.depth += 1;
                let v = crate::expr::eval(&self.asm.exprs, e, self);
                self.depth -= 1;
                v
            }
            _ => match self.asm.symbol_addr(id) {
                Some(addr) => Ok(Value::abs(addr)),
                None => Ok(Value::sym(id, 0)),
            },
        }
    }

    fn here(&mut self, span: Span) -> Result<Value, crate::expr::EvalError> {
        Err(crate::expr::EvalError::new(span, "`.` cannot be used here"))
    }

    fn section_start(&mut self, span: Span) -> Result<Value, crate::expr::EvalError> {
        Err(crate::expr::EvalError::new(
            span,
            "`$$` is not supported yet",
        ))
    }

    fn local_ref(
        &mut self,
        n: u32,
        _: crate::lexer::LocalDir,
        span: Span,
    ) -> Result<Value, crate::expr::EvalError> {
        Err(crate::expr::EvalError::new(
            span,
            format!("local label `{n}` was not resolved"),
        ))
    }

    fn modifier(
        &mut self,
        _: crate::intern::Name,
        inner: Value,
        _: Span,
    ) -> Result<Value, crate::expr::EvalError> {
        Ok(inner)
    }
}

/// The mark that goes at the same offset as a relocation a linker may
/// rewrite; see [`FixupKind::marker_reloc`].
///
/// GNU as writes one only beside a relocation that names a symbol, since a
/// field the assembler resolved itself leaves a linker nothing to rewrite;
/// both callers have already established that.
fn marker_relocation(kind: &FixupKind, section: SectionId, at: u64) -> Option<Relocation> {
    (kind.marker_reloc != 0).then(|| Relocation {
        section,
        offset: at,
        symbol: None,
        addend: 0,
        kind: kind.marker_reloc,
        desc: RelocDesc::of(&FixupKind::data(0)),
    })
}

/// Explains why a value does not fit its field, naming the actual limit.
///
/// A field's byte width is rarely the constraint that matters: a MIPS branch
/// lives in a four-byte word but holds ±128 KiB in steps of four. Saying
/// "out of range for a 4-byte field" sends the reader to the wrong limit, and
/// calling a misaligned target "out of range" sends them to the wrong problem.
fn range_message(kind: &FixupKind, v: i64) -> String {
    let message = plain_range_message(kind, v);
    match kind.range_hint {
        Some(hint) => format!("{message}: {hint}"),
        None => message,
    }
}

/// [`range_message`] without the field's hint.
fn plain_range_message(kind: &FixupKind, v: i64) -> String {
    let what = if kind.pcrel { "offset" } else { "value" };
    let align = kind.value_align as i128;
    if align > 1 && (v as i128) % align != 0 {
        // Measured from a base rounded to the same boundary, the value is off
        // it exactly when the target is, and the number itself means little.
        if kind.pcrel && kind.pc_align as i128 >= align {
            return format!("the target is not on a {align}-byte boundary");
        }
        return format!("{what} {v} is not a multiple of {align}");
    }
    let (lo, hi) = kind.range();
    format!("{what} {v} is out of range ({} to {})", show(lo), show(hi))
}

/// Prints a bound in whichever base reads better: small limits in decimal,
/// the large ones as the power-of-two-ish hex they really are.
fn show(n: i128) -> String {
    if n.unsigned_abs() < 0x1_0000 {
        n.to_string()
    } else if n < 0 {
        format!("-{:#x}", n.unsigned_abs())
    } else {
        format!("{n:#x}")
    }
}

/// A power-of-two byte count as a diagnostic writes it: `2 KB`, `256 MB`.
fn byte_size(n: u64) -> String {
    for (unit, name) in [(1u64 << 30, "GB"), (1 << 20, "MB"), (1 << 10, "KB")] {
        if n >= unit {
            return format!("{} {name}", n / unit);
        }
    }
    format!("{n} bytes")
}

pub fn uleb128(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

pub fn sleb128(mut v: i64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        // Stop once the remaining bits are all copies of the sign bit that the
        // last emitted byte already carries.
        let done = (v == 0 && byte & 0x40 == 0) || (v == -1 && byte & 0x40 != 0);
        if done {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uleb_matches_the_dwarf_examples() {
        assert_eq!(uleb128(0), vec![0]);
        assert_eq!(uleb128(2), vec![2]);
        assert_eq!(uleb128(127), vec![127]);
        assert_eq!(uleb128(128), vec![0x80, 1]);
        assert_eq!(uleb128(624485), vec![0xe5, 0x8e, 0x26]);
    }

    #[test]
    fn sleb_matches_the_dwarf_examples() {
        assert_eq!(sleb128(2), vec![2]);
        assert_eq!(sleb128(-2), vec![0x7e]);
        assert_eq!(sleb128(127), vec![0xff, 0]);
        assert_eq!(sleb128(-127), vec![0x81, 0x7f]);
        assert_eq!(sleb128(128), vec![0x80, 1]);
        assert_eq!(sleb128(-128), vec![0x80, 0x7f]);
    }
}
