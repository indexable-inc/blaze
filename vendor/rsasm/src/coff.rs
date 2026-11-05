//! What the assembler has to know about PE/COFF.
//!
//! The writer is in [`crate::output::coff`]; this is the half that runs while
//! the source is read — the directives COFF has and ELF does not (`.def`,
//! `.secrel32`, `.rva`, `.linkonce`, the `.seh_*` family), the COMDAT and
//! attribute arguments of `.section`, and the x86-64 unwind data those `.seh_*`
//! directives describe, which is written out once layout has settled. The
//! `.cv_*` family is one of those directive sets too, and large enough to
//! live in [`crate::codeview`].
//!
//! Nothing here runs unless `-f coff` (or `-f win64` / `-f win32`) was asked
//! for: the directives refuse any other output format rather than quietly
//! assembling into something that cannot hold them.

use crate::assembler::Assembler;
use crate::cursor::Cursor;
use crate::expr::{ExprKind, ExprRef};
use crate::lexer::{Punct, TokKind};
use crate::output::coff;
use crate::reloc::RelocClass;
use crate::section::{FixupKind, SectionId, SectionKind};
use crate::source::Span;
use crate::symbol::{Binding, SymbolId};
use std::collections::HashMap;

/// What `.section` said about a COFF section beyond what [`SectionFlags`]
/// can hold: the characteristics its flag letters asked for, and its COMDAT.
///
/// [`SectionFlags`]: crate::section::SectionFlags
#[derive(Copy, Clone, Debug)]
pub(crate) struct SectionInfo {
    /// Everything but the alignment bits, which come from the section's own
    /// alignment when it is written.
    pub(crate) characteristics: u32,
    pub(crate) comdat: Option<Comdat>,
}

/// A section that the linker keeps one copy of.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Comdat {
    /// `IMAGE_COMDAT_SELECT_*`, in the section symbol's auxiliary record.
    pub(crate) selection: u8,
    /// The symbol the linker matches copies by; `None` for `.linkonce`,
    /// which uses the section's own symbol.
    pub(crate) symbol: Option<SymbolId>,
}

/// What `.def name; .scl n; .type t; .endef` recorded about a symbol.
#[derive(Copy, Clone, Default, Debug)]
pub(crate) struct Def {
    pub(crate) storage_class: Option<u8>,
    pub(crate) ty: Option<u16>,
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) sections: HashMap<SectionId, SectionInfo>,
    pub(crate) defs: HashMap<SymbolId, Def>,
    /// The `.file` names, which become file symbols at the end of the table.
    pub(crate) files: Vec<String>,
    /// The symbol an open `.def` is describing.
    def: Option<SymbolId>,
    /// What the `.cv_*` directives have recorded; see [`crate::codeview`].
    pub(crate) cv: crate::codeview::State,
    /// Finished `.seh_proc` blocks, in the order they were read.
    procs: Vec<Proc>,
    open: Option<Proc>,
}

impl State {
    pub(crate) fn in_def(&self) -> bool {
        self.def.is_some()
    }
}

/// One `.seh_proc` block: where the function starts and ends, and the unwind
/// codes its prologue needs.
///
/// Offsets are kept as labels rather than numbers, since nothing knows how
/// long an instruction is until relaxation has settled; the unwind data is
/// built from their addresses in [`Assembler::emit_coff_unwind`].
struct Proc {
    /// The section the function is in, which decides where its unwind data
    /// goes; see [`Assembler::coff_unwind_section`].
    section: SectionId,
    begin: SymbolId,
    end: Option<SymbolId>,
    /// Where the prologue ends, which is its size.
    prologue_end: Option<SymbolId>,
    codes: Vec<Code>,
    /// `.seh_setframe`: the frame register and its offset in 16-byte units.
    frame: Option<(u8, u8)>,
    /// `.seh_handler`, and whether it is an exception or a termination
    /// handler (`UNW_FLAG_EHANDLER` / `UNW_FLAG_UHANDLER`).
    handler: Option<(ExprRef, u8)>,
    /// Where `.seh_handlerdata` reserved room for the unwind information, in
    /// `.xdata`: the fragment the information is written into once its
    /// offsets are known, since the handler's data follows it there.
    info_at: Option<u32>,
    span: Span,
}

/// One unwind code: the operation, and the point in the prologue it undoes.
struct Code {
    at: SymbolId,
    op: u8,
    info: u8,
    /// The extra 16-bit slots the operation takes, in order.
    extra: Vec<u16>,
}

// Unwind operation numbers, from the x64 exception handling ABI.
const UWOP_PUSH_NONVOL: u8 = 0;
const UWOP_ALLOC_LARGE: u8 = 1;
const UWOP_ALLOC_SMALL: u8 = 2;
const UWOP_SET_FPREG: u8 = 3;
const UWOP_SAVE_NONVOL: u8 = 4;
const UWOP_SAVE_NONVOL_FAR: u8 = 5;
const UWOP_SAVE_XMM128: u8 = 8;
const UWOP_SAVE_XMM128_FAR: u8 = 9;
const UWOP_PUSH_MACHFRAME: u8 = 10;

const UNW_FLAG_EHANDLER: u8 = 1;
const UNW_FLAG_UHANDLER: u8 = 2;

/// The length of a block's `UNWIND_INFO`: a four-byte header, the unwind
/// codes in two-byte slots padded to a whole number of four-byte words, and
/// the handler's address if it has one.
///
/// A prologue with nothing to undo and no handler still takes eight bytes,
/// which is the smallest the structure can be.
fn unwind_info_size(p: &Proc) -> usize {
    let slots: usize = p.codes.iter().map(|c| 1 + c.extra.len()).sum();
    let handler = if p.handler.is_some() { 4 } else { 0 };
    if slots == 0 && handler == 0 {
        return 8;
    }
    4 + slots.next_multiple_of(2) * 2 + handler
}

/// Whether `name` is one of the directives this module handles.
pub(crate) fn is_directive(name: &str) -> bool {
    matches!(
        name,
        ".def" | ".endef" | ".scl" | ".linkonce" | ".rva" | ".secrel32" | ".secidx" | ".safeseh"
    ) || name.starts_with(".seh_")
        || crate::codeview::is_directive(name)
}

/// The fields that may appear between `.def` and `.endef`, where they mean
/// something other than the ELF directive of the same name.
pub(crate) fn is_def_field(name: &str) -> bool {
    matches!(
        name,
        ".scl" | ".type" | ".endef" | ".size" | ".dim" | ".tag" | ".val" | ".line"
    )
}

/// The `IMAGE_COMDAT_SELECT_*` a selection name asks for.
fn selection(name: &str) -> Option<u8> {
    Some(match name {
        "one_only" | "no_duplicates" => 1,
        "discard" | "any" => 2,
        "same_size" => 3,
        "same_contents" => 4,
        "associative" => 5,
        "largest" => 6,
        "newest" => 7,
        _ => return None,
    })
}

/// The register numbers the unwind codes use, which are the machine's own
/// encoding order rather than DWARF's — `rsp` is 4 here and 7 in DWARF — so
/// they are a property of the Windows unwind format rather than of the x86
/// backend, and live with the rest of it.
fn seh_register(name: &str) -> Option<u8> {
    let name = name.trim_start_matches('%').to_ascii_lowercase();
    let gpr = [
        "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12",
        "r13", "r14", "r15",
    ];
    if let Some(i) = gpr.iter().position(|r| *r == name) {
        return Some(i as u8);
    }
    name.strip_prefix("xmm")
        .and_then(|n| n.parse::<u8>().ok())
        .filter(|n| *n < 16)
}

/// Whether a symbol reaches the COFF symbol table.
///
/// The assembler's own labels — numeric locals, the anonymous ones standing
/// in for `.`, and the private ones — stay out of it, as llvm-mc keeps them
/// out; a relocation that would have named one names its section and an
/// offset instead. A private label is spelled `.L` as in ELF, except for
/// i386, where llvm-mc takes Microsoft's `L`: any local label whose name
/// starts with a capital L, `Loop` too, and not `.Lfoo`. NASM keeps every
/// label the source wrote.
pub(crate) fn keeps_symbol(asm: &Assembler, id: SymbolId) -> bool {
    let sym = asm.symbols.get(id);
    let name = asm.interner.get(sym.name);
    if sym.local_number.is_some() || name.contains('\u{0}') {
        return false;
    }
    if sym.ty == crate::symbol::SymType::Section {
        return false;
    }
    if !sym.is_defined() && !sym.used {
        return false;
    }
    if asm.options.dialect == crate::lexer::Dialect::Nasm {
        return true;
    }
    let private = match coff::machine(asm.target()) {
        Some(coff::MACHINE_I386) => "L",
        _ => ".L",
    };
    !(name.starts_with(private) && sym.binding == Binding::Local && sym.is_defined())
}

/// The storage class a symbol is written with: what `.def` said, or the
/// class its binding implies.
pub(crate) fn storage_class(asm: &Assembler, id: SymbolId) -> u8 {
    if let Some(c) = asm.coff.defs.get(&id).and_then(|d| d.storage_class) {
        return c;
    }
    let sym = asm.symbols.get(id);
    if sym.binding == Binding::Local && sym.is_defined() {
        coff::SYM_CLASS_STATIC
    } else {
        coff::SYM_CLASS_EXTERNAL
    }
}

/// The symbol's COFF type, which only `.def ...; .type 32; .endef` sets.
///
/// `.type foo,@function` is ELF's spelling and says nothing here, which is
/// also what llvm-mc makes of it.
pub(crate) fn symbol_type(asm: &Assembler, id: SymbolId) -> u16 {
    asm.coff.defs.get(&id).and_then(|d| d.ty).unwrap_or(0)
}

/// Whether `.def` gave the symbol the function type, `IMAGE_SYM_DTYPE_FUNCTION`
/// in the high half of its type.
pub(crate) fn is_function(asm: &Assembler, id: SymbolId) -> bool {
    symbol_type(asm, id) & 0xf0 == coff::SYM_TYPE_FUNCTION
}

impl Assembler {
    /// Whether COFF-only syntax may be used, reporting why not if it may not.
    fn coff_output(&mut self, span: Span, what: &str) -> bool {
        if self.options.format.is_coff() {
            return true;
        }
        self.diags.error(
            span,
            format!("`{what}` writes a COFF object; assemble with `-f coff`"),
        );
        false
    }

    /// Runs one of the COFF-only directives. Returns false if the name is not
    /// one of them.
    pub(crate) fn coff_directive(&mut self, name: &str, cur: &mut Cursor<'_>, span: Span) -> bool {
        if !is_directive(name) && !(self.coff.in_def() && is_def_field(name)) {
            return false;
        }
        if !self.coff_output(span, name) {
            return true;
        }
        match name {
            ".def" => self.coff_def(cur, span),
            ".endef" => self.coff.def = None,
            ".scl" | ".type" | ".size" | ".dim" | ".tag" | ".val" | ".line" => {
                self.coff_def_field(name, cur, span)
            }
            ".linkonce" => self.coff_linkonce(cur, span),
            ".rva" => self.coff_reloc_data(cur, span, RelocClass::ImageRelative, 4),
            ".secrel32" => self.coff_reloc_data(cur, span, RelocClass::SectionRelative, 4),
            ".secidx" => self.coff_reloc_data(cur, span, RelocClass::SectionIndex, 2),
            // `.safeseh` lists an i386 handler in `.sxdata` by its symbol
            // table index, a field no relocation describes. GNU as, which is
            // what checks the x86 PE objects rsasm writes, has no such
            // directive at all, so there is no reference to write it against.
            ".safeseh" => {
                self.diags.error(
                    span,
                    "`.safeseh` is llvm-mc's; GNU as, which is what rsasm's x86 PE/COFF \
                     objects are checked against, has no such directive",
                );
                cur.set_pos(cur.all().len());
            }
            _ if crate::codeview::is_directive(name) => self.codeview_directive(name, cur, span),
            _ => self.seh_directive(name, cur, span),
        }
        true
    }

    fn coff_def(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let Some((name, nspan)) = self.expect_name(cur) else {
            return;
        };
        // GNU as allows the fields on the same line, separated by `;`, which
        // the parser has already split into statements of their own.
        let id = self.symbols.intern(name, nspan);
        self.coff.defs.entry(id).or_default();
        self.coff.def = Some(id);
        let _ = span;
    }

    fn coff_def_field(&mut self, name: &str, cur: &mut Cursor<'_>, span: Span) {
        let Some(id) = self.coff.def else {
            self.diags
                .error(span, format!("`{name}` outside a `.def` block"));
            return;
        };
        let Some(e) = self.parse_expr(cur) else {
            return;
        };
        let Some(v) = self.eval_absolute(e, name) else {
            return;
        };
        let d = self.coff.defs.entry(id).or_default();
        match name {
            ".scl" => d.storage_class = Some(v as u8),
            ".type" => d.ty = Some(v as u16),
            // `.size`, `.dim`, `.tag`, `.val` and `.line` describe debug
            // information no linker needs from an assembler; GNU as accepts
            // them inside `.def` and so does this.
            _ => {}
        }
    }

    /// `.linkonce [selection]`: the current section becomes a COMDAT keyed on
    /// its own symbol.
    fn coff_linkonce(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let mut sel = 2; // `discard`, as GNU as defaults
        if !cur.at_end() && !cur.is_empty() {
            let Some((n, nspan)) = self.expect_name(cur) else {
                return;
            };
            let text = self.interner.get(n).to_ascii_lowercase();
            match selection(&text) {
                Some(s) => sel = s,
                None => {
                    self.diags
                        .error(nspan, format!("unknown `.linkonce` type `{text}`"));
                    return;
                }
            }
        }
        let id = self.cur;
        let info = self.coff_section_info(id);
        if info.comdat.is_some() {
            self.diags
                .error(span, "this section is already a COMDAT section");
            return;
        }
        info.comdat = Some(Comdat {
            selection: sel,
            symbol: None,
        });
    }

    /// The COFF description of a section, made from its name and flags if
    /// `.section` never gave it one.
    pub(crate) fn coff_section_info(&mut self, id: SectionId) -> &mut SectionInfo {
        let s = self.section(id);
        let (name, kind, flags) = (self.interner.get(s.name).to_string(), s.kind, s.flags);
        self.coff.sections.entry(id).or_insert(SectionInfo {
            characteristics: coff::default_characteristics(&name, kind, &flags),
            comdat: None,
        })
    }

    /// `.rva`, `.secrel32` and `.secidx`: a data field the linker fills in
    /// from something only it knows — where the image starts, or where a
    /// section ended up.
    fn coff_reloc_data(&mut self, cur: &mut Cursor<'_>, span: Span, class: RelocClass, size: u8) {
        loop {
            let Some(e) = self.parse_expr(cur) else {
                return;
            };
            self.bind_here_to_item(e);
            // The ELF number is the plain absolute one for the width; what
            // makes the field image- or section-relative is the class, which
            // is what the COFF writer reads.
            let kind = FixupKind::data(size)
                .with_reloc(self.arch.data_reloc(size, false).unwrap_or(0))
                .with_class(class)
                .linker_only();
            if self.check_nobits(span) {
                return;
            }
            self.cur_section().emit_fixup(size, e, kind, span);
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
    }

    /// Translates one relocation from the backend's ELF numbering into
    /// COFF's, and moves its addend into the field it relocates, which is
    /// where COFF keeps one. Returns false after reporting that COFF has no
    /// relocation for this field.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn coff_relocation(
        &mut self,
        machine: u16,
        r: &mut crate::assembler::Relocation,
        kind: &FixupKind,
        si: usize,
        fi: usize,
        off: u32,
        span: Span,
    ) -> bool {
        let Some(ty) = coff::reloc::map(machine, r.desc.class, r.kind) else {
            let msg = if r.desc.class == crate::reloc::RelocClass::ThreadLocal {
                "a COFF object has no relocation for ELF's thread-local access models; \
                 Windows reaches a thread-local variable through the image's TLS index"
                    .to_string()
            } else {
                format!(
                    "a COFF object has no relocation for this {}-byte {}reference",
                    kind.size,
                    if kind.pcrel { "PC-relative " } else { "" }
                )
            };
            self.diags.error(span, msg);
            return false;
        };
        // COFF has no null symbol: index 0 is the first section's. So a
        // reference relocated against no symbol at all, which ELF writes
        // against symbol 0, has nothing to name here.
        if r.symbol.is_none() {
            self.diags.error(
                span,
                "a COFF relocation must name a symbol; this refers to an absolute address",
            );
            return false;
        }
        // A section index has no room for an offset into the section, so a
        // reference to a symbol the table does not keep loses the offset
        // rather than adding it to the index; llvm-mc writes the field as
        // zero there too.
        let addend = if r.desc.class == crate::reloc::RelocClass::SectionIndex {
            0
        } else {
            r.addend + coff::reloc::pc_base(machine, ty)
        };
        if addend != 0 {
            let endian = self.frag_arch(si, fi).0.endian();
            if let crate::section::FragKind::Bytes { variants, chosen } =
                &mut self.sections[si].frags[fi].kind
            {
                let dst =
                    &mut variants[*chosen].bytes[off as usize..off as usize + kind.size as usize];
                coff::reloc::write_addend(machine, ty, kind, endian, dst, addend);
            }
        }
        r.kind = ty as u32;
        r.addend = 0;
        true
    }

    // ---- structured exception handling -------------------------------------

    fn seh_directive(&mut self, name: &str, cur: &mut Cursor<'_>, span: Span) {
        if crate::output::coff::machine(self.target()) != Some(crate::output::coff::MACHINE_AMD64) {
            self.diags.error(
                span,
                format!(
                    "`{name}` writes x86-64 unwind data; `{}` has unwind data of its own, \
                     with its own directives and a packed or extended record rather than \
                     x86-64's unwind codes, which rsasm does not write yet",
                    self.target().name()
                ),
            );
            return;
        }
        match name {
            ".seh_proc" => self.seh_proc(cur, span),
            ".seh_endproc" => self.seh_endproc(span),
            ".seh_endprologue" => {
                let at = self.anon_label(span);
                if let Some(p) = self.seh_open(span) {
                    p.prologue_end = Some(at);
                }
            }
            ".seh_pushreg" => {
                if let Some(reg) = self.seh_reg(cur, span) {
                    self.seh_code(span, UWOP_PUSH_NONVOL, reg, Vec::new());
                }
            }
            ".seh_pushframe" => {
                // `.seh_pushframe @code` records that the hardware pushed the
                // error code as well.
                let mut info = 0;
                if cur.eat_punct(Punct::At).is_some() {
                    let _ = self.expect_name(cur);
                    info = 1;
                } else if let Some((n, _)) = cur.peek().ident().map(|n| (n, ())) {
                    // A COFF name may start with `@`, so `@code` is one word.
                    if self.interner.get(n).eq_ignore_ascii_case("@code") {
                        cur.advance();
                        info = 1;
                    }
                }
                self.seh_code(span, UWOP_PUSH_MACHFRAME, info, Vec::new());
            }
            ".seh_stackalloc" => self.seh_stackalloc(cur, span),
            ".seh_setframe" => self.seh_setframe(cur, span),
            ".seh_savereg" | ".seh_savexmm" => self.seh_save(name, cur, span),
            ".seh_handler" => self.seh_handler(cur, span),
            ".seh_handlerdata" => self.seh_handlerdata(span),
            // Epilogues only reach the unwind data in version 2 of it, which
            // `.seh_unwindversion 2` asks for and rsasm does not write; in
            // version 1 llvm-mc checks them and writes nothing.
            ".seh_startepilogue" | ".seh_endepilogue" => {
                self.seh_open(span);
            }
            _ => self
                .diags
                .error(span, format!("`{name}` is not a directive rsasm knows")),
        }
    }

    /// `.seh_handlerdata`: what follows goes into `.xdata`, right after the
    /// block's unwind information, as llvm-mc writes it. The information's
    /// offsets are not known yet, but its size is — every unwind code of the
    /// prologue has been read by now — so its room is reserved here and
    /// filled in once layout has settled.
    fn seh_handlerdata(&mut self, span: Span) {
        let Some(p) = self.seh_open(span) else {
            return;
        };
        if p.info_at.is_some() {
            self.diags
                .error(span, "a second `.seh_handlerdata` in one `.seh_proc` block");
            return;
        }
        let size = unwind_info_size(p);
        let text = p.section;
        let xdata = self.coff_unwind_section(".xdata", text);
        self.set_section(xdata);
        self.push_unwind_align(xdata, span);
        let at = self.cur_section().push(crate::section::Fragment::new(
            crate::section::FragKind::Bytes {
                variants: vec![crate::section::Variant::new(vec![0; size])],
                chosen: 0,
            },
            span,
        ));
        if let Some(p) = self.coff.open.as_mut() {
            p.info_at = Some(at);
        }
    }

    /// Unwind information starts on a four-byte boundary of `.xdata`, which
    /// matters after handler data of another length.
    fn push_unwind_align(&mut self, xdata: SectionId, span: Span) {
        self.section_mut(xdata).push(crate::section::Fragment::new(
            crate::section::FragKind::Align {
                align: 4,
                fill: vec![0],
                max_skip: None,
                pad: 0,
                nop_state: None,
            },
            span,
        ));
    }

    fn seh_open(&mut self, span: Span) -> Option<&mut Proc> {
        if self.coff.open.is_none() {
            self.diags
                .error(span, "this needs an open `.seh_proc` block");
        }
        self.coff.open.as_mut()
    }

    fn seh_proc(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let _ = self.expect_name(cur);
        if self.coff.open.is_some() {
            self.diags
                .error(span, "`.seh_proc` inside another `.seh_proc` block");
            return;
        }
        let begin = self.anon_label(span);
        self.coff.open = Some(Proc {
            section: self.cur,
            begin,
            end: None,
            prologue_end: None,
            codes: Vec::new(),
            frame: None,
            handler: None,
            info_at: None,
            span,
        });
    }

    fn seh_endproc(&mut self, span: Span) {
        let end = self.anon_label(span);
        let Some(mut p) = self.coff.open.take() else {
            self.diags
                .error(span, "`.seh_endproc` without a `.seh_proc`");
            return;
        };
        if p.prologue_end.is_none() {
            self.diags
                .error(span, "this `.seh_proc` block has no `.seh_endprologue`");
        }
        p.end = Some(end);
        // llvm-mc writes the unwind information as the block ends, so the
        // `.xdata` section exists from here; only the runtime function table
        // waits for the end of the file. That is the order the sections come
        // out in.
        let text = p.section;
        self.coff_unwind_section(".xdata", text);
        self.coff.procs.push(p);
    }

    /// A register operand, written `%rbp` in AT&T syntax and `rbp` in Intel.
    fn seh_reg(&mut self, cur: &mut Cursor<'_>, span: Span) -> Option<u8> {
        cur.eat_punct(Punct::Percent);
        let tok = cur.peek();
        let name = match tok.kind {
            TokKind::Ident(n) => {
                cur.advance();
                self.interner.get(n).to_string()
            }
            _ => {
                self.diags.error(tok.span, "expected a register");
                return None;
            }
        };
        match seh_register(&name) {
            Some(r) => Some(r),
            None => {
                self.diags.error(
                    span,
                    format!("`{name}` is not a register unwind data names"),
                );
                None
            }
        }
    }

    fn seh_number(&mut self, cur: &mut Cursor<'_>, what: &str) -> Option<i64> {
        let e = self.parse_expr(cur)?;
        self.eval_absolute(e, what)
    }

    fn seh_code(&mut self, span: Span, op: u8, info: u8, extra: Vec<u16>) {
        let at = self.anon_label(span);
        if let Some(p) = self.seh_open(span) {
            p.codes.push(Code {
                at,
                op,
                info,
                extra,
            });
        }
    }

    fn seh_stackalloc(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let Some(n) = self.seh_number(cur, "`.seh_stackalloc` size") else {
            return;
        };
        if n <= 0 || n % 8 != 0 {
            self.diags.error(
                span,
                "`.seh_stackalloc` takes a positive multiple of 8 bytes",
            );
            return;
        }
        // Three encodings, by size: up to 128 bytes in the code itself, up to
        // 512 KB in one extra slot counted in eight-byte units, and anything
        // larger as a byte count in two.
        let (op, info, extra) = if n <= 128 {
            (UWOP_ALLOC_SMALL, (n / 8 - 1) as u8, Vec::new())
        } else if n < 512 * 1024 {
            (UWOP_ALLOC_LARGE, 0, vec![(n / 8) as u16])
        } else {
            (
                UWOP_ALLOC_LARGE,
                1,
                vec![(n & 0xffff) as u16, (n >> 16) as u16],
            )
        };
        self.seh_code(span, op, info, extra);
    }

    fn seh_setframe(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let Some(reg) = self.seh_reg(cur, span) else {
            return;
        };
        if cur.eat_punct(Punct::Comma).is_none() {
            self.diags
                .error(span, "`.seh_setframe` takes a register and an offset");
            return;
        }
        let Some(off) = self.seh_number(cur, "`.seh_setframe` offset") else {
            return;
        };
        if !(0..=240).contains(&off) || off % 16 != 0 {
            self.diags.error(
                span,
                "`.seh_setframe` offset must be a multiple of 16, at most 240",
            );
            return;
        }
        if let Some(p) = self.seh_open(span) {
            p.frame = Some((reg, (off / 16) as u8));
        }
        self.seh_code(span, UWOP_SET_FPREG, 0, Vec::new());
    }

    fn seh_save(&mut self, name: &str, cur: &mut Cursor<'_>, span: Span) {
        let xmm = name == ".seh_savexmm";
        let Some(reg) = self.seh_reg(cur, span) else {
            return;
        };
        if cur.eat_punct(Punct::Comma).is_none() {
            self.diags
                .error(span, format!("`{name}` takes a register and an offset"));
            return;
        }
        let Some(off) = self.seh_number(cur, "the offset") else {
            return;
        };
        let unit = if xmm { 16 } else { 8 };
        if off < 0 || off % unit != 0 {
            self.diags.error(
                span,
                format!("`{name}` offset must be a non-negative multiple of {unit}"),
            );
            return;
        }
        // The scaled form reaches 512 KB (or 1 MB for the 16-byte scale);
        // past that the offset takes two slots and is written in bytes.
        let scaled = off / unit;
        let (op, extra) = match (xmm, scaled <= u16::MAX as i64) {
            (false, true) => (UWOP_SAVE_NONVOL, vec![scaled as u16]),
            (true, true) => (UWOP_SAVE_XMM128, vec![scaled as u16]),
            (false, false) => (
                UWOP_SAVE_NONVOL_FAR,
                vec![(off & 0xffff) as u16, (off >> 16) as u16],
            ),
            (true, false) => (
                UWOP_SAVE_XMM128_FAR,
                vec![(off & 0xffff) as u16, (off >> 16) as u16],
            ),
        };
        self.seh_code(span, op, reg, extra);
    }

    fn seh_handler(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let Some(e) = self.parse_expr(cur) else {
            return;
        };
        let mut flags = 0u8;
        while cur.eat_punct(Punct::Comma).is_some() {
            cur.eat_punct(Punct::At);
            let Some((n, nspan)) = self.expect_name(cur) else {
                return;
            };
            match self
                .interner
                .get(n)
                .trim_start_matches('@')
                .to_ascii_lowercase()
                .as_str()
            {
                "except" => flags |= UNW_FLAG_EHANDLER,
                "unwind" => flags |= UNW_FLAG_UHANDLER,
                other => {
                    self.diags
                        .error(nspan, format!("unknown `.seh_handler` flag `{other}`"));
                    return;
                }
            }
        }
        if let Some(p) = self.seh_open(span) {
            p.handler = Some((e, flags));
        }
    }

    /// The `.xdata` or `.pdata` holding the unwind data of a function in
    /// `text`: read-only data aligned to four bytes, as llvm-mc creates it.
    ///
    /// Only a function in `.text` itself uses the one section of that name.
    /// Every other text section gets a pair of its own under the same COFF
    /// name (`MCStreamer::getAssociatedXDataSection`), so that a COMDAT
    /// function's unwind data can be an associative COMDAT and be dropped
    /// with the copy it describes. The sections are told apart in the
    /// assembler by the text section's name after a NUL, which no source can
    /// spell; see `crate::coff::section_name`.
    fn coff_unwind_section(&mut self, name: &str, text: SectionId) -> SectionId {
        let flags = crate::section::SectionFlags::rodata();
        let own = self.interner.get(self.section(text).name) != ".text";
        let n = if own {
            let text_name = self.interner.get(self.section(text).name).to_string();
            self.interner.intern(&format!("{name}\u{0}{text_name}"))
        } else {
            self.interner.intern(name)
        };
        let id = self.get_or_create_section(n, SectionKind::Progbits, flags, 4);
        self.section_mut(id).align = self.section(id).align.max(4);
        // A COMDAT text section hands its key symbol on; a section that is
        // only unique -- `.linkonce`, which keys on the section's own symbol,
        // or a plain section of another name -- leaves the unwind data an
        // ordinary section.
        let key = self
            .coff
            .sections
            .get(&text)
            .and_then(|i| i.comdat)
            .and_then(|c| c.symbol);
        if let Some(sym) = key {
            let characteristics =
                coff::default_characteristics(name, SectionKind::Progbits, &flags)
                    | coff::SCN_LNK_COMDAT;
            self.coff.sections.entry(id).or_insert(SectionInfo {
                characteristics,
                comdat: Some(Comdat {
                    selection: coff::SELECT_ASSOCIATIVE,
                    symbol: Some(sym),
                }),
            });
        }
        id
    }

    /// Writes the `.xdata` and `.pdata` the `.seh_*` directives described,
    /// once every instruction has its final length. Returns whether anything
    /// was written, so layout can settle again.
    pub(crate) fn emit_coff_unwind(&mut self) -> bool {
        if let Some(span) = self.coff.open.as_ref().map(|p| p.span) {
            self.diags
                .error(span, "unterminated `.seh_proc`, expected `.seh_endproc`");
        }
        let procs = std::mem::take(&mut self.coff.procs);
        if procs.is_empty() {
            return false;
        }
        // The image-relative fields of `.pdata` and of a handler's address
        // are four-byte data fields; their class is what makes them
        // image-relative. See `coff_reloc_data`.
        let imgrel = self.target().data_reloc(4, false).unwrap_or(0);
        // The unwind information of every function first, then every runtime
        // function table entry, as llvm-mc writes them: a function with a
        // `.pdata` of its own has its `.xdata` before any of them.
        let mut unwinds = Vec::with_capacity(procs.len());
        for p in &procs {
            let span = p.span;
            let xdata = self.coff_unwind_section(".xdata", p.section);
            let info = self.unwind_info(p);
            let at = match p.info_at {
                Some(frag) => {
                    // The room `.seh_handlerdata` reserved, which is exactly
                    // this long.
                    let fixups = info
                        .fixups
                        .into_iter()
                        .map(|(offset, _, expr, kind)| crate::section::Fixup {
                            offset,
                            expr,
                            kind,
                            span,
                        })
                        .collect();
                    self.sections[xdata.0 as usize].frags[frag as usize].kind =
                        crate::section::FragKind::Bytes {
                            variants: vec![crate::section::Variant {
                                bytes: info.bytes,
                                fixups,
                            }],
                            chosen: 0,
                        };
                    (xdata, frag)
                }
                None => {
                    self.push_unwind_align(xdata, span);
                    let at = self.next_pos(xdata);
                    self.push_blob(xdata, info, span);
                    at
                }
            };
            unwinds.push(self.dwarf_label(at, span));
        }

        // The runtime function table entry of each: where the code starts and
        // ends, and where its unwind information is, all as addresses
        // relative to the image base.
        for (p, unwind) in procs.iter().zip(unwinds) {
            let span = p.span;
            let pdata = self.coff_unwind_section(".pdata", p.section);
            let mut b = crate::dwarf::emit::Blob::new(self.target().endian());
            for sym in [p.begin, p.end.unwrap_or(p.begin), unwind] {
                let e = self.exprs.alloc(ExprKind::SymId(sym), span);
                b.fixup(
                    4,
                    e,
                    FixupKind::data(4)
                        .with_reloc(imgrel)
                        .with_class(RelocClass::ImageRelative)
                        .linker_only(),
                );
            }
            self.push_blob(pdata, b, span);
        }
        true
    }

    /// The `UNWIND_INFO` structure of one `.seh_proc` block; see
    /// [`unwind_info_size`] for its length.
    fn unwind_info(&mut self, p: &Proc) -> crate::dwarf::emit::Blob {
        let mut b = crate::dwarf::emit::Blob::new(self.target().endian());
        let imgrel = self.target().data_reloc(4, false).unwrap_or(0);
        let base = self.symbol_addr(p.begin).unwrap_or(0);
        let offset = |asm: &Assembler, s: SymbolId| -> u8 {
            (asm.symbol_addr(s).unwrap_or(0) - base).clamp(0, 255) as u8
        };
        let prologue = p.prologue_end.map_or(0, |s| offset(self, s));
        let mut flags = p.handler.map_or(0, |(_, f)| f);
        if flags == 0 && p.handler.is_some() {
            flags = UNW_FLAG_EHANDLER;
        }
        let slots: usize = p.codes.iter().map(|c| 1 + c.extra.len()).sum();
        b.u8(1 | (flags << 3)); // version 1
        b.u8(prologue);
        b.u8(slots.min(255) as u8);
        b.u8(match p.frame {
            Some((reg, off)) => reg | (off << 4),
            None => 0,
        });
        // The codes are read back to front, so the last operation of the
        // prologue comes first.
        for c in p.codes.iter().rev() {
            b.u8(offset(self, c.at));
            b.u8((c.info << 4) | c.op);
            for slot in &c.extra {
                b.int(*slot as u64, 2);
            }
        }
        // The array is a whole number of four-byte words.
        if !slots.is_multiple_of(2) {
            b.int(0, 2);
        }
        if slots == 0 && p.handler.is_none() {
            b.int(0, 4);
        }
        if let Some((e, _)) = p.handler {
            b.fixup(
                4,
                e,
                FixupKind::data(4)
                    .with_reloc(imgrel)
                    .with_class(RelocClass::ImageRelative)
                    .linker_only(),
            );
        }
        b
    }
}

/// What a COFF-only `@` modifier makes of a reference: `@IMGREL`,
/// `@SECREL32`, and NASM's `wrt ..imagebase`.
///
/// These name something no psABI has, so they are not the backends' to
/// answer with an ELF number; they name a [`RelocClass`] instead, which is
/// how [`crate::reloc`] carries a meaning no format owns.
pub(crate) fn modifier_class(name: &str) -> Option<RelocClass> {
    Some(match name {
        "imgrel" | "imagebase" => RelocClass::ImageRelative,
        "secrel" | "secrel32" => RelocClass::SectionRelative,
        "secidx" => RelocClass::SectionIndex,
        _ => return None,
    })
}

/// The core's view of a section with these characteristics: what decides
/// whether padding is no-ops, and whether data may be emitted into it.
pub(crate) fn section_flags(characteristics: u32) -> crate::section::SectionFlags {
    crate::section::SectionFlags {
        alloc: characteristics & coff::SCN_LNK_REMOVE == 0,
        write: characteristics & coff::SCN_MEM_WRITE != 0,
        exec: characteristics & coff::SCN_CNT_CODE != 0,
        ..Default::default()
    }
}

/// Parses the COFF attributes of `.section`: the flag letters, and the COMDAT
/// selection and symbol that may follow them.
pub(crate) fn parse_section_attributes(
    asm: &mut Assembler,
    cur: &mut Cursor<'_>,
    name: &str,
    span: Span,
) -> (u32, Option<Comdat>, SectionKind) {
    let mut characteristics = None;
    if let Some(s) = asm.expect_string(cur, "of section flags") {
        let letters = String::from_utf8_lossy(&s).into_owned();
        match coff::parse_flags(name, &letters) {
            Ok(v) => characteristics = Some(v),
            Err(c) => asm
                .diags
                .error(span, format!("unknown COFF section flag `{c}`")),
        }
    }
    let mut comdat = None;
    if cur.eat_punct(Punct::Comma).is_some()
        && let Some((n, nspan)) = asm.expect_name(cur)
    {
        let text = asm.interner.get(n).to_ascii_lowercase();
        match selection(&text) {
            Some(sel) => {
                let mut symbol = None;
                if cur.eat_punct(Punct::Comma).is_some()
                    && let Some((sn, sspan)) = asm.expect_name(cur)
                {
                    symbol = Some(asm.symbols.intern(sn, sspan));
                }
                comdat = Some(Comdat {
                    selection: sel,
                    symbol,
                });
            }
            None => asm
                .diags
                .error(nspan, format!("unknown COMDAT selection `{text}`")),
        }
    }
    // A section llvm-mc knows by name keeps its own characteristics, unless
    // a COMDAT symbol makes it another section of the same name.
    let keyed = comdat.is_some_and(|c| c.symbol.is_some());
    let characteristics = coff::preset_characteristics(name)
        .filter(|_| !keyed)
        .or(characteristics)
        .unwrap_or_else(|| coff::parse_flags(name, "").unwrap_or(0));
    let kind = if characteristics & coff::SCN_CNT_UNINITIALIZED_DATA != 0 {
        SectionKind::Nobits
    } else {
        SectionKind::Progbits
    };
    (characteristics, comdat, kind)
}

/// The name a section is written under. Sections of one name that COMDAT
/// symbols tell apart — `.rdata` once per constant a compiler folds — are
/// kept apart in the assembler under a name that adds the symbol after a NUL,
/// which no source can spell.
pub(crate) fn section_name(name: &str) -> &str {
    name.split('\u{0}').next().unwrap_or(name)
}
