//! The compilation unit that goes with a line table the assembler made up:
//! `.debug_info`, `.debug_abbrev`, `.debug_aranges`, and `.debug_str` or the
//! range lists where the reference writes them.
//!
//! GNU as writes one whenever it writes a line table and the source has no
//! `.debug_info` of its own, which includes plain `.loc` source; llvm-mc only
//! for `-g`. The producer is `rsasm` and the version, or what the
//! `DEBUG_PRODUCER` environment variable says, which llvm-mc also reads.

use super::emit::Blob;
use super::{Flavor, Pos};
use crate::assembler::Assembler;
use crate::expr::{BinOp, ExprKind};
use crate::section::{SectionFlags, SectionId};
use crate::source::Span;
use crate::symbol::{Binding, SymType, SymbolValue};

const DW_TAG_COMPILE_UNIT: u64 = 0x11;
const DW_TAG_SUBPROGRAM: u64 = 0x2e;
const DW_TAG_UNSPECIFIED_TYPE: u64 = 0x3b;
const DW_TAG_LABEL: u64 = 0x0a;
const DW_AT_NAME: u64 = 0x03;
const DW_AT_STMT_LIST: u64 = 0x10;
const DW_AT_LOW_PC: u64 = 0x11;
const DW_AT_HIGH_PC: u64 = 0x12;
const DW_AT_LANGUAGE: u64 = 0x13;
const DW_AT_COMP_DIR: u64 = 0x1b;
const DW_AT_PRODUCER: u64 = 0x25;
const DW_AT_DECL_FILE: u64 = 0x3a;
const DW_AT_DECL_LINE: u64 = 0x3b;
const DW_AT_EXTERNAL: u64 = 0x3f;
const DW_AT_TYPE: u64 = 0x49;
const DW_AT_RANGES: u64 = 0x55;
const DW_FORM_ADDR: u64 = 0x01;
const DW_FORM_DATA2: u64 = 0x05;
const DW_FORM_DATA4: u64 = 0x06;
const DW_FORM_STRING: u64 = 0x08;
const DW_FORM_BLOCK: u64 = 0x09;
const DW_FORM_FLAG: u64 = 0x0c;
const DW_FORM_STRP: u64 = 0x0e;
const DW_FORM_UDATA: u64 = 0x0f;
const DW_FORM_REF_UDATA: u64 = 0x15;
const DW_FORM_SEC_OFFSET: u64 = 0x17;
const DW_FORM_FLAG_PRESENT: u64 = 0x19;
const DW_LANG_MIPS_ASSEMBLER: u64 = 0x8001;
const DW_UT_COMPILE: u8 = 0x01;
const DW_RLE_END_OF_LIST: u8 = 0x00;
const DW_RLE_START_LENGTH: u8 = 0x07;

/// The producer string of a generated unit.
fn producer() -> String {
    std::env::var("DEBUG_PRODUCER")
        .unwrap_or_else(|_| format!("rsasm {}", env!("CARGO_PKG_VERSION")))
}

/// A function GNU as describes: a symbol typed `@function` with a size.
struct Function {
    name: String,
    external: bool,
    symbol: crate::symbol::SymbolId,
    size: u64,
}

impl Assembler {
    /// The size of a section, for a generated section list: where its code
    /// ends, before any padding added to round it up.
    fn code_end(&self, section: SectionId) -> u64 {
        let s = self.section(section);
        match s.frags.last() {
            Some(f) if self.tail_pads.contains(&section) => f.offset,
            _ => s.size,
        }
    }

    /// Whether a DWARF section of that name exists and holds anything.
    pub(crate) fn section_has_bytes(&self, name: &str) -> bool {
        let name = self.dwarf_object_name(name);
        self.sections
            .iter()
            .any(|s| self.interner.get(s.name) == name && s.size > 0)
    }

    fn debug_section(&mut self, name: &str, align: u64) -> SectionId {
        self.dwarf_section(name, SectionFlags::default(), 0, align)
    }

    /// An address field relocated against `pos` plus `offset`.
    fn address(&mut self, b: &mut Blob, pos: Pos, offset: u64, size: u8) {
        let e = self.pos_expr(pos, offset);
        let kind = self.abs_kind(size);
        b.fixup(size, e, kind);
    }

    /// A four-byte offset into `section`, `offset` bytes past `pos`.
    fn section_offset(&mut self, b: &mut Blob, pos: Pos, offset: u64) {
        self.dwarf_offset(b, pos, offset);
    }

    /// The unit's `DW_AT_stmt_list`: the offset of its line program, which is
    /// at the start of `.debug_line`.
    ///
    /// llvm-mc writes the line table after the unit that describes it, so the
    /// section does not exist yet. Where the field is a relocation the section
    /// is made here, last of the unit's; a Mach-O object needs only the
    /// number, and making it would put it in the object ahead of the frame
    /// table.
    fn line_table_offset(&mut self, b: &mut Blob) {
        if self.options.format == crate::output::Format::MachO {
            b.int(0, 4);
            return;
        }
        let pos = (self.debug_section(".debug_line", 1), 0);
        self.dwarf_offset(b, pos, 0);
    }

    // ---- GNU as ------------------------------------------------------------

    /// GNU as's `out_debug_info` and the sections around it.
    pub(crate) fn gnu_unit(&mut self, version: u16) {
        let endian = self.target().endian();
        let ptr = self.target().pointer_bytes(&self.target().initial_state());
        let segs: Vec<SectionId> = self.dwarf.line.sequences.iter().map(|s| s.0).collect();
        let single = segs.len() == 1;

        let info_sec = self.debug_section(".debug_info", 1);
        let abbrev_sec = self.debug_section(".debug_abbrev", 1);
        let aranges_sec = self.debug_section(".debug_aranges", 2 * ptr as u64);
        let str_flags = SectionFlags {
            merge: true,
            strings: true,
            ..SectionFlags::default()
        };
        let str_sec = self.dwarf_section(".debug_str", str_flags, 1, 1);
        let info_start = self.next_pos(info_sec);
        let abbrev_start = self.next_pos(abbrev_sec);
        let str_start = self.next_pos(str_sec);
        let line_start = match self.section_id(".debug_line") {
            Some(s) => (s, 0),
            None => return,
        };

        // Range lists, for code in more than one section.
        let mut ranges_at = None;
        if !single {
            if version < 5 {
                let sec = self.debug_section(".debug_ranges", 2 * ptr as u64);
                let start = self.next_pos(sec);
                let mut b = Blob::new(endian);
                b.int(u64::MAX >> (64 - 8 * ptr as u32), ptr as usize);
                b.int(0, ptr as usize);
                for &s in &segs {
                    let end = self.code_end(s);
                    self.address(&mut b, (s, 0), 0, ptr);
                    self.address(&mut b, (s, 0), end, ptr);
                }
                b.int(0, ptr as usize);
                b.int(0, ptr as usize);
                self.push_blob(sec, b, Span::DUMMY);
                ranges_at = Some((start, 0));
            } else {
                let sec = self.debug_section(".debug_rnglists", 1);
                let start = self.next_pos(sec);
                let mut b = Blob::new(endian);
                b.int(0, 4);
                b.int(5, 2);
                b.u8(ptr);
                b.u8(0);
                b.int(0, 4);
                for &s in &segs {
                    b.u8(DW_RLE_START_LENGTH);
                    self.address(&mut b, (s, 0), 0, ptr);
                    b.uleb(self.code_end(s));
                }
                b.u8(DW_RLE_END_OF_LIST);
                let len = b.len() - 4;
                b.patch(0, len, 4);
                self.push_blob(sec, b, Span::DUMMY);
                ranges_at = Some((start, 12));
            }
        }

        // .debug_aranges
        let mut b = Blob::new(endian);
        b.int(0, 4);
        b.int(2, 2);
        self.section_offset(&mut b, (info_sec, 0), 0);
        b.u8(ptr);
        b.u8(0);
        b.align(0, 2 * ptr as u64, 0);
        for &s in &segs {
            let end = self.code_end(s);
            self.address(&mut b, (s, 0), 0, ptr);
            b.int(end, ptr as usize);
        }
        b.int(0, ptr as usize);
        b.int(0, ptr as usize);
        let len = b.len() - 4;
        b.patch(0, len, 4);
        self.push_blob(aranges_sec, b, Span::DUMMY);

        // The functions, in symbol table order.
        let mut functions = Vec::new();
        for (id, sym) in self.symbols.iter() {
            if sym.ty != SymType::Func || !matches!(sym.value, SymbolValue::Label { .. }) {
                continue;
            }
            let size = sym.size.and_then(|e| self.eval_const(e)).unwrap_or(0);
            if size == 0 {
                continue;
            }
            functions.push(Function {
                name: self.interner.get(sym.name).to_string(),
                external: sym.binding == Binding::Global,
                symbol: id,
                size: size as u64,
            });
        }
        let have_efunc = functions.iter().any(|f| f.external);
        let have_lfunc = functions.iter().any(|f| !f.external);

        // .debug_abbrev
        let mut a = Blob::new(endian);
        let secoff = if version < 4 {
            DW_FORM_DATA4
        } else {
            DW_FORM_SEC_OFFSET
        };
        a.uleb(1);
        a.uleb(DW_TAG_COMPILE_UNIT);
        a.u8(!functions.is_empty() as u8);
        let attr = |a: &mut Blob, at: u64, form: u64| {
            a.uleb(at);
            a.uleb(form);
        };
        attr(&mut a, DW_AT_STMT_LIST, secoff);
        if single {
            attr(&mut a, DW_AT_LOW_PC, DW_FORM_ADDR);
            attr(
                &mut a,
                DW_AT_HIGH_PC,
                if version < 4 {
                    DW_FORM_ADDR
                } else {
                    DW_FORM_UDATA
                },
            );
        } else {
            attr(&mut a, DW_AT_RANGES, secoff);
        }
        attr(&mut a, DW_AT_NAME, DW_FORM_STRP);
        attr(&mut a, DW_AT_COMP_DIR, DW_FORM_STRP);
        attr(&mut a, DW_AT_PRODUCER, DW_FORM_STRP);
        attr(&mut a, DW_AT_LANGUAGE, DW_FORM_DATA2);
        attr(&mut a, 0, 0);
        let mut func_form = 0;
        if !functions.is_empty() {
            a.uleb(2);
            a.uleb(DW_TAG_SUBPROGRAM);
            a.u8(0);
            attr(&mut a, DW_AT_NAME, DW_FORM_STRP);
            if have_efunc {
                func_form = if have_lfunc || version < 4 {
                    DW_FORM_FLAG
                } else {
                    DW_FORM_FLAG_PRESENT
                };
                attr(&mut a, DW_AT_EXTERNAL, func_form);
            } else {
                func_form = DW_FORM_BLOCK;
            }
            if version > 2 {
                attr(&mut a, DW_AT_TYPE, DW_FORM_REF_UDATA);
            }
            attr(&mut a, DW_AT_LOW_PC, DW_FORM_ADDR);
            attr(
                &mut a,
                DW_AT_HIGH_PC,
                if version < 4 {
                    DW_FORM_ADDR
                } else {
                    DW_FORM_UDATA
                },
            );
            attr(&mut a, 0, 0);
            if version > 2 {
                a.uleb(3);
                a.uleb(DW_TAG_UNSPECIFIED_TYPE);
                a.u8(0);
                attr(&mut a, 0, 0);
            }
        }
        a.u8(0);
        self.push_blob(abbrev_sec, a, Span::DUMMY);

        // .debug_str: the unit's name, directory and producer, then the
        // functions' names.
        let mut s = Blob::new(endian);
        let first = if version >= 5
            && self
                .dwarf
                .line
                .gnu
                .files
                .first()
                .is_some_and(|f| f.is_some())
        {
            0
        } else {
            1
        };
        let (dir, name) = match self.dwarf.line.gnu.files.get(first).cloned().flatten() {
            Some(f) => (f.dir, f.name),
            None => (0, String::new()),
        };
        if dir != 0
            && let Some(Some(d)) = self.dwarf.line.gnu.dirs.get(dir as usize)
        {
            s.bytes.extend_from_slice(d.as_bytes());
            s.bytes.push(b'/');
        }
        s.str(&name);
        let comp_dir_at = s.len();
        s.str(&super::line::current_dir());
        let producer_at = s.len();
        s.str(&producer());
        let mut name_at = Vec::new();
        for f in &functions {
            name_at.push(s.len());
            s.str(&f.name);
        }
        self.push_blob(str_sec, s, Span::DUMMY);

        // .debug_info. The subprograms refer to the unspecified type that
        // follows them, by its offset in the unit, which a first pass finds.
        let body = |asm: &mut Assembler, no_type: u64| -> Blob {
            let mut b = Blob::new(endian);
            b.int(0, 4);
            b.int(version as u64, 2);
            if version < 5 {
                asm.section_offset(&mut b, abbrev_start, 0);
                b.u8(ptr);
            } else {
                b.u8(DW_UT_COMPILE);
                b.u8(ptr);
                asm.section_offset(&mut b, abbrev_start, 0);
            }
            b.uleb(1);
            asm.section_offset(&mut b, line_start, 0);
            match ranges_at {
                None => {
                    let s = segs[0];
                    let end = asm.code_end(s);
                    asm.address(&mut b, (s, 0), 0, ptr);
                    if version < 4 {
                        asm.address(&mut b, (s, 0), end, ptr);
                    } else {
                        b.uleb(end);
                    }
                }
                Some((pos, off)) => asm.section_offset(&mut b, pos, off),
            }
            asm.section_offset(&mut b, str_start, 0);
            asm.section_offset(&mut b, str_start, comp_dir_at);
            asm.section_offset(&mut b, str_start, producer_at);
            b.int(DW_LANG_MIPS_ASSEMBLER, 2);
            if !functions.is_empty() {
                for (f, &at) in functions.iter().zip(&name_at) {
                    b.uleb(2);
                    asm.section_offset(&mut b, str_start, at);
                    if func_form == DW_FORM_FLAG {
                        b.u8(f.external as u8);
                    }
                    if version > 2 {
                        b.uleb(no_type);
                    }
                    let e = asm.exprs.alloc(ExprKind::SymId(f.symbol), Span::DUMMY);
                    let kind = asm.abs_kind(ptr);
                    b.fixup(ptr, e, kind);
                    if version < 4 {
                        let sym = asm.exprs.alloc(ExprKind::SymId(f.symbol), Span::DUMMY);
                        let size = asm.exprs.int(f.size, Span::DUMMY);
                        let e = asm
                            .exprs
                            .alloc(ExprKind::Binary(BinOp::Add, sym, size), Span::DUMMY);
                        let kind = asm.abs_kind(ptr);
                        b.fixup(ptr, e, kind);
                    } else {
                        b.uleb(f.size);
                    }
                }
                if version > 2 {
                    b.uleb(3);
                }
                b.uleb(0);
            }
            b
        };
        let base = self.section(info_sec).size;
        let mut no_type = 0;
        let mut b = body(self, 0);
        if !functions.is_empty() && version > 2 {
            // Where the unspecified type lands, given how wide its own
            // offset is in each subprogram.
            loop {
                let at = base + b.len() - 2;
                if at == no_type {
                    break;
                }
                no_type = at;
                b = body(self, no_type);
            }
        }
        let len = b.len() - 4;
        b.patch(0, len, 4);
        let _ = info_start;
        self.push_blob(info_sec, b, Span::DUMMY);
    }

    // ---- llvm-mc -----------------------------------------------------------

    /// llvm-mc's `MCGenDwarfInfo::Emit`, for `-g`.
    pub(crate) fn llvm_unit(&mut self, version: u16) {
        let endian = self.target().endian();
        let ptr = self.target().pointer_bytes(&self.target().initial_state());
        let with_rows: Vec<SectionId> = self.dwarf.line.sequences.iter().map(|s| s.0).collect();
        let segs: Vec<SectionId> = self
            .dwarf
            .line
            .source
            .sections
            .iter()
            .copied()
            .filter(|s| with_rows.contains(s))
            .collect();
        if segs.is_empty() {
            return;
        }
        let use_ranges = segs.len() > 1 && version >= 3;
        let info_sec = self.debug_section(".debug_info", 1);
        let abbrev_sec = self.debug_section(".debug_abbrev", 1);
        let aranges_sec = self.debug_section(".debug_aranges", 1);
        let info_start = self.next_pos(info_sec);
        let abbrev_start = self.next_pos(abbrev_sec);

        // .debug_aranges
        let mut b = Blob::new(endian);
        let header = 4 + 2 + 4 + 1 + 1;
        let unit = 2 * ptr as u64;
        let pad = (unit - header % unit) % unit;
        let length = header + pad + unit * segs.len() as u64 + unit;
        b.int(length - 4, 4);
        b.int(2, 2);
        self.section_offset(&mut b, info_start, 0);
        b.u8(ptr);
        b.u8(0);
        for _ in 0..pad {
            b.u8(0);
        }
        for &s in &segs {
            let end = self.section(s).size;
            self.address(&mut b, (s, 0), 0, ptr);
            // How far the code reaches is a difference of two labels in it,
            // so a relaxing linker between them is the one that knows it.
            if self.code_moves_between(s, 0, end) {
                let last = (s, self.section(s).frags.len() as u32);
                let e = self.difference_expr(((s, 0), 0), (last, 0));
                let kind = self.abs_kind(ptr);
                b.fixup(ptr, e, kind);
            } else {
                b.int(end, ptr as usize);
            }
        }
        b.int(0, ptr as usize);
        b.int(0, ptr as usize);
        self.push_blob(aranges_sec, b, Span::DUMMY);

        let mut ranges_at = None;
        if use_ranges {
            if version >= 5 {
                let sec = self.debug_section(".debug_rnglists", 1);
                let start = self.next_pos(sec);
                let mut b = Blob::new(endian);
                b.int(0, 4);
                b.int(5, 2);
                b.u8(ptr);
                b.u8(0);
                b.int(0, 4);
                for &s in &segs {
                    let end = self.section(s).size;
                    b.u8(DW_RLE_START_LENGTH);
                    self.address(&mut b, (s, 0), 0, ptr);
                    b.uleb(end);
                }
                b.u8(DW_RLE_END_OF_LIST);
                let len = b.len() - 4;
                b.patch(0, len, 4);
                self.push_blob(sec, b, Span::DUMMY);
                ranges_at = Some((start, 12));
            } else {
                let sec = self.debug_section(".debug_ranges", 1);
                let start = self.next_pos(sec);
                let mut b = Blob::new(endian);
                for &s in &segs {
                    let end = self.section(s).size;
                    b.int(u64::MAX >> (64 - 8 * ptr as u32), ptr as usize);
                    self.address(&mut b, (s, 0), 0, ptr);
                    b.int(0, ptr as usize);
                    b.int(end, ptr as usize);
                }
                b.int(0, ptr as usize);
                b.int(0, ptr as usize);
                self.push_blob(sec, b, Span::DUMMY);
                ranges_at = Some((start, 0));
            }
        }

        // .debug_abbrev
        let comp_dir = super::line::current_dir();
        let mut a = Blob::new(endian);
        let secoff = if version >= 4 {
            DW_FORM_SEC_OFFSET
        } else {
            DW_FORM_DATA4
        };
        let attr = |a: &mut Blob, at: u64, form: u64| {
            a.uleb(at);
            a.uleb(form);
        };
        a.uleb(1);
        a.uleb(DW_TAG_COMPILE_UNIT);
        a.u8(1);
        attr(&mut a, DW_AT_STMT_LIST, secoff);
        if use_ranges {
            attr(&mut a, DW_AT_RANGES, secoff);
        } else {
            attr(&mut a, DW_AT_LOW_PC, DW_FORM_ADDR);
            attr(&mut a, DW_AT_HIGH_PC, DW_FORM_ADDR);
        }
        attr(&mut a, DW_AT_NAME, DW_FORM_STRING);
        if !comp_dir.is_empty() {
            attr(&mut a, DW_AT_COMP_DIR, DW_FORM_STRING);
        }
        attr(&mut a, DW_AT_PRODUCER, DW_FORM_STRING);
        attr(&mut a, DW_AT_LANGUAGE, DW_FORM_DATA2);
        attr(&mut a, 0, 0);
        a.uleb(2);
        a.uleb(DW_TAG_LABEL);
        a.u8(0);
        attr(&mut a, DW_AT_NAME, DW_FORM_STRING);
        attr(&mut a, DW_AT_DECL_FILE, DW_FORM_DATA4);
        attr(&mut a, DW_AT_DECL_LINE, DW_FORM_DATA4);
        attr(&mut a, DW_AT_LOW_PC, DW_FORM_ADDR);
        attr(&mut a, 0, 0);
        a.u8(0);
        self.push_blob(abbrev_sec, a, Span::DUMMY);

        // .debug_info
        let mut b = Blob::new(endian);
        b.int(0, 4);
        b.int(version as u64, 2);
        if version >= 5 {
            b.u8(DW_UT_COMPILE);
            b.u8(ptr);
        }
        self.section_offset(&mut b, abbrev_start, 0);
        if version <= 4 {
            b.u8(ptr);
        }
        b.uleb(1);
        self.line_table_offset(&mut b);
        match ranges_at {
            Some((pos, off)) => self.section_offset(&mut b, pos, off),
            None => {
                let s = segs[0];
                let end = self.section(s).size;
                self.address(&mut b, (s, 0), 0, ptr);
                self.address(&mut b, (s, 0), end, ptr);
            }
        }
        let l = &self.dwarf.line.llvm;
        if let Some(d) = l.dirs.first() {
            b.bytes.extend_from_slice(d.as_bytes());
            b.bytes.push(b'/');
        }
        let root = match l.files.get(1).cloned().flatten() {
            Some(f) => f.name,
            None => l.root.clone().map(|f| f.name).unwrap_or_default(),
        };
        b.str(&root);
        if !comp_dir.is_empty() {
            b.str(&comp_dir);
        }
        b.str(&producer());
        b.int(DW_LANG_MIPS_ASSEMBLER, 2);
        let file = if version >= 5 { 0 } else { 1 };
        let labels = self.dwarf.line.source.labels.clone();
        for (name, line, pos) in labels {
            b.uleb(2);
            b.str(&name);
            b.int(file, 4);
            b.int(line as u64, 4);
            self.address(&mut b, pos, 0, ptr);
        }
        b.u8(0);
        let len = b.len() - 4;
        b.patch(0, len, 4);
        self.push_blob(info_sec, b, Span::DUMMY);
    }

    /// The DWARF section of that name, if the source or the assembler made
    /// one.
    pub(crate) fn section_id(&self, name: &str) -> Option<SectionId> {
        let name = self.dwarf_object_name(name);
        self.sections
            .iter()
            .find(|s| self.interner.get(s.name) == name)
            .map(|s| s.id)
    }

    /// Gives generated rows their file numbers, entering the source files
    /// in the file table as the reference does.
    pub(crate) fn number_generated_files(&mut self, version: u16) {
        let flavor = self.dwarf_target().flavor;
        let pwd = super::line::current_dir();
        match flavor {
            Flavor::Gnu => {
                let mut seqs = std::mem::take(&mut self.dwarf.line.sequences);
                for (_, rows) in &mut seqs {
                    for row in rows.iter_mut() {
                        if let Some(path) = &row.gen_path {
                            row.loc.file = self.dwarf.line.gnu.allocate_generated(path, &pwd);
                        }
                    }
                }
                self.dwarf.line.sequences = seqs;
            }
            Flavor::Llvm => {
                if !self.dwarf.line.source.on {
                    return;
                }
                let Some(main) = self.dwarf.line.source.main else {
                    return;
                };
                let source = self.sm.file(main);
                let mut name = source.name.to_string_lossy().into_owned();
                if let Some(rest) = name.strip_prefix(&pwd)
                    && let Some(rest) = rest.strip_prefix('/')
                {
                    name = rest.to_string();
                }
                let md5 = (version >= 5).then(|| super::md5::md5(source.src.as_bytes()));
                let file = if version >= 5 {
                    let l = &mut self.dwarf.line.llvm;
                    l.comp_dir = Some(pwd.clone());
                    l.root = Some(super::line::FileEntry { name, dir: 0, md5 });
                    l.all_md5 &= md5.is_some();
                    l.any_md5 |= md5.is_some();
                    0
                } else {
                    if self.dwarf.line.source.touched {
                        let _ = self.dwarf.line.llvm.try_get_file_pub(&name);
                    }
                    1
                };
                self.dwarf.line.used = true;
                for (_, rows) in &mut self.dwarf.line.sequences {
                    for row in rows.iter_mut() {
                        if row.gen_path.is_some() {
                            row.loc.file = file;
                        }
                    }
                }
            }
        }
    }
}
