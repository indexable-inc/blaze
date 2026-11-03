//! CodeView debugging information: the `.cv_*` directives, and the
//! `.debug$S` subsection stream they write.
//!
//! This is the format MSVC-targeted toolchains read, and llvm-mc is the only
//! reference here that writes it — the mingw assembler has no `.cv_*`
//! directive at all — so every byte below follows llvm-mc's `MCCodeView`.
//!
//! Unlike DWARF, none of it is gathered and written at the end: each
//! directive that produces bytes produces them where it stands, in whatever
//! section is current, because that is how the subsections of a `.debug$S`
//! stream are put in order. What a `.cv_loc` cannot know yet is where the
//! code it names will be, so the line subsection's code offsets are written
//! as differences of two labels and left to layout, and the function's own
//! start is a `SECREL32`/`SECTION` relocation pair, which is the only thing
//! in the stream a linker has to fill in.
//!
//! Two tables are shared by the whole object rather than by one subsection:
//!
//! * The string table, which holds every `.cv_file` name and every
//!   `.cv_string`. `.cv_stringtable` reserves room for it where it stands and
//!   it is filled in once the source has been read, since a `.cv_string`
//!   after it still enters it; see [`Assembler::finish_codeview`].
//! * The file checksum table, which `.cv_filechecksums` writes as a snapshot
//!   of the files declared so far. A line subsection names a file by the byte
//!   offset of its entry in that table, and so does
//!   `.cv_filechecksumoffset`; llvm-mc records those offsets as it writes the
//!   table, so a file that is not in it has no offset to give and the
//!   directive that wanted one is an error.
//!
//! Not written yet: the inline-site machinery (`.cv_inline_site_id`,
//! `.cv_inline_linetable`), which re-attributes an inlinee's rows to the
//! call site and encodes them as binary annotations; `.cv_def_range`, which
//! says where a local variable lives; and the `.cv_fpo_*` frame data, which
//! is a `DEBUG_S_FRAMEDATA` subsection of its own. Each refuses rather than
//! writing something no reference checked.

use crate::assembler::Assembler;
use crate::cursor::Cursor;
use crate::dwarf::Pos;
use crate::dwarf::emit::Blob;
use crate::expr::{BinOp, ExprKind, ExprRef};
use crate::lexer::{Punct, TokKind};
use crate::reloc::RelocClass;
use crate::section::{Fixup, FixupKind, FragKind, SectionId};
use crate::source::Span;
use crate::symbol::SymbolId;
use std::collections::BTreeMap;

/// `DEBUG_S_STRINGTABLE`.
const SUB_STRINGTABLE: u64 = 0xf3;
/// `DEBUG_S_FILECHKSMS`.
const SUB_FILECHKSMS: u64 = 0xf4;
/// `DEBUG_S_LINES`.
const SUB_LINES: u64 = 0xf2;

/// `LF_HaveColumns`, the one flag a line subsection header carries.
const LINES_HAVE_COLUMNS: u64 = 1;

/// `LineInfo::StatementFlag`, the top bit of a line entry.
const STATEMENT_FLAG: u32 = 0x8000_0000;

/// One entry of the file table, as `.cv_file` gave it.
struct File {
    /// Where the name is in the string table.
    name_offset: u32,
    /// The checksum bytes, which are written only when `kind` is non-zero.
    checksum: Vec<u8>,
    /// `FileChecksumKind`: 0 none, 1 MD5, 2 SHA1, 3 SHA256. llvm-mc neither
    /// checks the number nor measures the checksum against it, and neither
    /// does this.
    kind: u8,
}

impl File {
    /// How much of the checksum table the entry takes: four bytes for the
    /// string table offset, one each for the checksum's length and kind, the
    /// checksum, and padding to four bytes.
    ///
    /// An entry with no checksum kind is written as the offset and a zero
    /// word, so its checksum never reaches the table however long it is.
    fn entry_size(&self) -> u32 {
        if self.kind == 0 {
            8
        } else {
            (6 + self.checksum.len() as u32).next_multiple_of(4)
        }
    }
}

/// One `.cv_loc`: where it stands, and the source position it names.
///
/// llvm-mc defines a label at the directive itself rather than holding the
/// position until the next instruction consumes it, as `.loc` does, so two
/// `.cv_loc`s in a row make two rows at one address.
#[derive(Copy, Clone)]
struct Loc {
    pos: Pos,
    file: u32,
    line: u32,
    /// Truncated to sixteen bits, which is what `MCCVLoc` holds it in; a
    /// column of zero means the row has none.
    column: u16,
    is_stmt: bool,
}

/// One block of a line subsection: the rows of a run that share a file, and
/// where that file's entry is in the checksum table, which is how the
/// subsection names it.
struct Block {
    name: u32,
    rows: Vec<Loc>,
}

/// A function id introduced by `.cv_func_id`.
#[derive(Default)]
struct Func {
    /// The section the first `.cv_loc` of this function was in; every later
    /// one has to be in the same.
    section: Option<SectionId>,
    locs: Vec<Loc>,
}

/// What the `.cv_*` directives have recorded so far.
pub(crate) struct State {
    /// The file table, by `.cv_file` number. A number skipped leaves a hole,
    /// which only the checksum table minds.
    files: BTreeMap<u32, File>,
    /// The string table's bytes, which start with the empty string so that
    /// no name is at offset zero.
    strings: Vec<u8>,
    /// Where each string already in the table starts.
    offsets: std::collections::HashMap<Vec<u8>, u32>,
    funcs: BTreeMap<u32, Func>,
    /// The fragment `.cv_stringtable` reserved for the table itself.
    strings_at: Option<Pos>,
    /// How many files `.cv_filechecksums` wrote, or `None` where it has not
    /// run: a file numbered higher than that has no entry in the table and
    /// so no offset to name it by.
    checksums: Option<u32>,
    /// The highest file number whose checksum table offset a directive has
    /// already written, and where to blame it.
    offset_used: Option<(u32, Span)>,
}

impl Default for State {
    fn default() -> State {
        State {
            files: BTreeMap::new(),
            strings: vec![0],
            offsets: std::collections::HashMap::new(),
            funcs: BTreeMap::new(),
            strings_at: None,
            checksums: None,
            offset_used: None,
        }
    }
}

impl State {
    /// The offset of `s` in the string table, entering it if it is new.
    fn string(&mut self, s: &[u8]) -> u32 {
        if let Some(&off) = self.offsets.get(s) {
            return off;
        }
        let off = self.strings.len() as u32;
        self.strings.extend_from_slice(s);
        self.strings.push(0);
        self.offsets.insert(s.to_vec(), off);
        off
    }

    /// The byte offset of file `num`'s entry in the checksum table, or `None`
    /// where a lower number is missing, which leaves every entry after it
    /// nowhere in particular.
    fn checksum_offset(&self, num: u32) -> Option<u32> {
        let mut off = 0;
        for i in 1..num {
            off += self.files.get(&i)?.entry_size();
        }
        Some(off)
    }

    /// The lowest file number that `.cv_file` has not given an entry, where
    /// a higher one has one: the gap that makes the checksum table
    /// unwritable.
    fn file_gap(&self) -> Option<u32> {
        let last = *self.files.keys().next_back()?;
        (1..last).find(|n| !self.files.contains_key(n))
    }
}

/// Whether `name` is one of the directives this module handles.
pub(crate) fn is_directive(name: &str) -> bool {
    name.starts_with(".cv_")
}

impl Assembler {
    /// Runs one of the `.cv_*` directives.
    pub(crate) fn codeview_directive(&mut self, name: &str, cur: &mut Cursor<'_>, span: Span) {
        match name {
            ".cv_file" => self.cv_file(cur, span),
            ".cv_func_id" => self.cv_func_id(cur, span),
            ".cv_loc" => self.cv_loc(cur, span),
            ".cv_linetable" => self.cv_linetable(cur, span),
            ".cv_filechecksums" => self.cv_filechecksums(span),
            ".cv_filechecksumoffset" => self.cv_filechecksumoffset(cur, span),
            ".cv_string" => self.cv_string(cur, span),
            ".cv_stringtable" => self.cv_stringtable(span),
            // The three families rsasm does not write. Each says what it
            // would have taken, since a source that uses one was written for
            // a compiler's output and will use the rest of the set too.
            ".cv_inline_site_id" | ".cv_inline_linetable" => self.diags.error(
                span,
                format!(
                    "`{name}` describes an inlined call site, which rsasm does not write yet: \
                     its rows belong to the caller's line table under the call site's own \
                     position, encoded as binary annotations"
                ),
            ),
            ".cv_def_range" => self.diags.error(
                span,
                "`.cv_def_range` says where a local variable lives, which rsasm does not \
                 write yet",
            ),
            _ if name.starts_with(".cv_fpo_") => self.diags.error(
                span,
                format!(
                    "`{name}` describes an i386 frame, which rsasm does not write yet: the \
                     `.cv_fpo_*` family becomes a `DEBUG_S_FRAMEDATA` subsection of its own"
                ),
            ),
            _ => self
                .diags
                .error(span, format!("`{name}` is not a directive rsasm knows")),
        }
    }

    /// `.cv_file num "name" ["checksum" kind]`.
    fn cv_file(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let Some(num) = self.cv_int(cur, "file number") else {
            return;
        };
        if num < 1 {
            self.diags.error(span, "file number less than one");
            return;
        }
        let Some(name) = self.expect_string(cur, "naming the file") else {
            return;
        };
        let mut checksum = Vec::new();
        let mut kind = 0u8;
        if !cur.peek().is_eol() {
            let Some(hex) = self.expect_string(cur, "holding the checksum") else {
                return;
            };
            let Some(k) = self.cv_int(cur, "checksum kind") else {
                return;
            };
            checksum = from_hex(&hex);
            // llvm-mc narrows the kind to a byte rather than refusing a
            // larger number, which turns 256 into "no checksum".
            kind = k as u8;
        }
        // llvm-mc has no upper bound: it resizes its file table to the
        // number and runs the host out of memory. There is nothing to follow
        // past four bytes anyway, which is the width of every field in the
        // stream that names a file.
        if num > u32::MAX as i64 {
            self.diags.error(span, "file number out of range");
            return;
        }
        let num = num as u32;
        if self.coff.cv.files.contains_key(&num) {
            self.diags.error(span, "file number already allocated");
            return;
        }
        let name_offset = self.coff.cv.string(&name);
        self.coff.cv.files.insert(
            num,
            File {
                name_offset,
                checksum,
                kind,
            },
        );
    }

    /// `.cv_func_id num`.
    fn cv_func_id(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let Some(id) = self.cv_func_number(cur, span) else {
            return;
        };
        if self.coff.cv.funcs.contains_key(&id) {
            self.diags.error(span, "function id already allocated");
            return;
        }
        self.coff.cv.funcs.insert(id, Func::default());
    }

    /// `.cv_loc funcid fileid [line [column]] [prologue_end] [is_stmt v]`,
    /// whose operands are bare numbers with no commas between them.
    fn cv_loc(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let Some(func) = self.cv_func_number(cur, span) else {
            return;
        };
        let Some(file) = self.cv_int(cur, "file number") else {
            return;
        };
        if file < 1 || file > u32::MAX as i64 || !self.coff.cv.files.contains_key(&(file as u32)) {
            self.diags
                .error(span, "unassigned file number in `.cv_loc` directive");
            return;
        }
        // The line and the column are taken only when a number stands where
        // they would: anything else is a sub-directive, or an error reported
        // as one.
        let mut line = 0u32;
        let mut column = 0u32;
        if let TokKind::Int(v) = cur.peek().kind {
            cur.advance();
            line = v as u32;
            if let TokKind::Int(v) = cur.peek().kind {
                cur.advance();
                column = v as u32;
            }
        }
        let mut is_stmt = false;
        while let Some(n) = cur.peek().ident() {
            let tok = cur.advance();
            let word = self.interner.get(n).to_string();
            match word.as_str() {
                // Recorded by llvm-mc and used by nothing it writes from
                // assembly: a prologue's end reaches the debugger through the
                // `S_FRAMEPROC` a compiler emits, not through the line table.
                "prologue_end" => {}
                "is_stmt" => match self.cv_int(cur, "`is_stmt` value") {
                    Some(0) => is_stmt = false,
                    Some(1) => is_stmt = true,
                    Some(_) => {
                        self.diags.error(tok.span, "is_stmt value not 0 or 1");
                        return;
                    }
                    None => return,
                },
                _ => {
                    self.diags.error(
                        tok.span,
                        format!("unknown `.cv_loc` sub-directive `{word}`"),
                    );
                    return;
                }
            }
        }
        if !self.coff.cv.funcs.contains_key(&func) {
            self.diags.error(
                span,
                "function id not introduced by `.cv_func_id` or `.cv_inline_site_id`",
            );
            return;
        }
        // The first `.cv_loc` of a function pins the section its rows are
        // measured in, since the subsection has one start to relocate.
        let section = self.cur;
        if let Some(f) = self.coff.cv.funcs.get(&func)
            && f.section.is_some_and(|pinned| pinned != section)
        {
            self.diags.error(
                span,
                "all `.cv_loc` directives for a function must be in the same section",
            );
            return;
        }
        let pos = self.dwarf_pos();
        if let Some(f) = self.coff.cv.funcs.get_mut(&func) {
            f.section = Some(section);
            f.locs.push(Loc {
                pos,
                file: file as u32,
                line,
                column: column as u16,
                is_stmt,
            });
        }
    }

    /// `.cv_linetable funcid, start, end`: the line subsection of one
    /// function, from the `.cv_loc`s read so far.
    ///
    /// The directive neither consumes those rows nor needs the function id to
    /// have been introduced: llvm-mc writes the header alone for a function
    /// it knows nothing about, and writes the whole table again for a second
    /// `.cv_linetable` of the same function.
    fn cv_linetable(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let Some(func) = self.cv_func_number(cur, span) else {
            return;
        };
        let Some(start) = self.cv_comma_symbol(cur) else {
            return;
        };
        let Some(end) = self.cv_comma_symbol(cur) else {
            return;
        };
        let rows: Vec<Loc> = self
            .coff
            .cv
            .funcs
            .get(&func)
            .map(|f| f.locs.clone())
            .unwrap_or_default();
        let have_columns = rows.iter().any(|l| l.column != 0);
        // Each run of rows that share a file is one block, so a file that
        // comes back gets a block of its own.
        let mut blocks: Vec<Block> = Vec::new();
        let mut missing = None;
        for loc in &rows {
            let Some(name) = self.coff.cv.checksum_offset(loc.file) else {
                missing = Some(loc.file);
                break;
            };
            match blocks.last_mut() {
                Some(b) if b.name == name => b.rows.push(*loc),
                _ => blocks.push(Block {
                    name,
                    rows: vec![*loc],
                }),
            }
            self.cv_note_checksum_offset(loc.file, span);
        }
        if let Some(file) = missing {
            self.diags.error(
                span,
                format!(
                    "file number {file} has no entry in the checksum table, since a lower \
                     number is missing from the file table"
                ),
            );
            return;
        }

        let Some(sec) = self.cv_section(span) else {
            return;
        };
        let mut b = Blob::new(self.target().endian());
        b.int(SUB_LINES, 4);
        let mut body = Blob::new(self.target().endian());
        self.cv_secrel(&mut body, start, span);
        self.cv_secidx(&mut body, start, span);
        body.int(if have_columns { LINES_HAVE_COLUMNS } else { 0 }, 2);
        self.cv_distance(&mut body, start, end, span);
        for block in &blocks {
            let n = block.rows.len() as u64;
            body.int(block.name as u64, 4);
            body.int(n, 4);
            body.int(12 + n * 8 + if have_columns { n * 4 } else { 0 }, 4);
            for loc in &block.rows {
                let e = self.cv_offset_expr(loc.pos, start, span);
                let kind = self.abs_kind(4);
                body.fixup(4, e, kind);
                let stmt = if loc.is_stmt { STATEMENT_FLAG } else { 0 };
                body.int((loc.line | stmt) as u64, 4);
            }
            if have_columns {
                for loc in &block.rows {
                    body.int(loc.column as u64, 2);
                    body.int(0, 2);
                }
            }
        }
        b.int(body.len(), 4);
        self.cv_append(&mut b, body);
        self.push_blob(sec, b, span);
    }

    /// `.cv_filechecksums`: the file table as a `DEBUG_S_FILECHKSMS`
    /// subsection.
    ///
    /// Each entry is padded to four bytes, and llvm-mc writes that padding as
    /// an alignment of the section rather than of the subsection, which is
    /// why the subsection's length is left to layout. Only an entry that has
    /// a checksum kind is padded that way: one without is written as two
    /// words and asks for no alignment at all, which is what decides whether
    /// the section ends up four-byte aligned.
    fn cv_filechecksums(&mut self, span: Span) {
        if self.coff.cv.files.is_empty() {
            // llvm-mc writes no subsection at all rather than an empty one.
            return;
        }
        if let Some(gap) = self.coff.cv.file_gap() {
            self.diags.error(
                span,
                format!(
                    "`.cv_filechecksums` needs every file number up to the highest declared; \
                     {gap} is missing"
                ),
            );
            return;
        }
        let Some(sec) = self.cv_section(span) else {
            return;
        };
        let endian = self.target().endian();
        let hdr = self.next_pos(sec);
        let mut b = Blob::new(endian);
        b.int(SUB_FILECHKSMS, 4);
        b.int(0, 4);
        let files: Vec<(u32, u8, Vec<u8>)> = self
            .coff
            .cv
            .files
            .values()
            .map(|f| (f.name_offset, f.kind, f.checksum.clone()))
            .collect();
        for (name_offset, kind, checksum) in files {
            b.int(name_offset as u64, 4);
            if kind == 0 {
                b.int(0, 4);
                continue;
            }
            b.u8(checksum.len() as u8);
            b.u8(kind);
            b.bytes.extend_from_slice(&checksum);
            self.push_blob(sec, std::mem::replace(&mut b, Blob::new(endian)), span);
            self.cv_align4(sec, span);
        }
        if !b.bytes.is_empty() {
            self.push_blob(sec, b, span);
        }
        let end = self.next_pos(sec);
        self.cv_length_fixup(hdr, end, span);
        self.coff.cv.checksums = Some(self.coff.cv.files.len() as u32);
    }

    /// `.cv_filechecksumoffset num`: where file `num`'s entry is in the
    /// checksum table.
    fn cv_filechecksumoffset(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let Some(num) = self.cv_int(cur, "file number") else {
            return;
        };
        if num < 1 {
            self.diags.error(span, "file number less than one");
            return;
        }
        if num > u32::MAX as i64 {
            self.diags.error(span, "file number out of range");
            return;
        }
        let num = num as u32;
        let Some(offset) = self
            .coff
            .cv
            .files
            .contains_key(&num)
            .then(|| self.coff.cv.checksum_offset(num))
            .flatten()
        else {
            self.diags
                .error(span, format!("unassigned file number {num}"));
            return;
        };
        let Some(sec) = self.cv_section(span) else {
            return;
        };
        self.cv_note_checksum_offset(num, span);
        let mut b = Blob::new(self.target().endian());
        b.int(offset as u64, 4);
        self.push_blob(sec, b, span);
    }

    /// `.cv_string "text"`: the offset of the text in the string table.
    fn cv_string(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let Some(s) = self.expect_string(cur, "to put in the string table") else {
            return;
        };
        let offset = self.coff.cv.string(&s);
        let Some(sec) = self.cv_section(span) else {
            return;
        };
        let mut b = Blob::new(self.target().endian());
        b.int(offset as u64, 4);
        self.push_blob(sec, b, span);
    }

    /// `.cv_stringtable`: a `DEBUG_S_STRINGTABLE` subsection holding the
    /// string table.
    ///
    /// The table itself goes where the first of these directives asks for it
    /// and is filled in once the source has been read, since a `.cv_string`
    /// written after it still enters it. A second directive therefore writes
    /// an empty subsection, which is what llvm-mc's does.
    fn cv_stringtable(&mut self, span: Span) {
        let Some(sec) = self.cv_section(span) else {
            return;
        };
        let endian = self.target().endian();
        let hdr = self.next_pos(sec);
        let mut b = Blob::new(endian);
        b.int(SUB_STRINGTABLE, 4);
        b.int(0, 4);
        self.push_blob(sec, b, span);
        if self.coff.cv.strings_at.is_none() {
            let at = self.push_blob(sec, Blob::new(endian), span);
            self.coff.cv.strings_at = Some(at);
        }
        self.cv_align4(sec, span);
        let end = self.next_pos(sec);
        self.cv_length_fixup(hdr, end, span);
    }

    /// Fills in the string table and reports what no `.cv_filechecksums`
    /// could give a number to. Called once the source has been read, before
    /// anything has been laid out.
    pub(crate) fn finish_codeview(&mut self) {
        if let Some((num, span)) = self.coff.cv.offset_used {
            match self.coff.cv.checksums {
                Some(n) if n >= num => {}
                Some(_) => self.diags.error(
                    span,
                    format!(
                        "file number {num} is named by its offset in the checksum table, but \
                         `.cv_filechecksums` ran before it was declared"
                    ),
                ),
                None => self.diags.error(
                    span,
                    "a file named by its offset in the checksum table needs a \
                     `.cv_filechecksums` to put the table somewhere",
                ),
            }
        }
        let Some(at) = self.coff.cv.strings_at else {
            return;
        };
        let bytes = std::mem::take(&mut self.coff.cv.strings);
        if let FragKind::Bytes { variants, chosen } =
            &mut self.sections[at.0.0 as usize].frags[at.1 as usize].kind
        {
            variants[*chosen].bytes = bytes;
        }
    }

    /// Records that a directive has written file `num`'s offset in the
    /// checksum table, which only a `.cv_filechecksums` covering that file
    /// can give it.
    fn cv_note_checksum_offset(&mut self, num: u32, span: Span) {
        if self.coff.cv.offset_used.is_none_or(|(n, _)| n < num) {
            self.coff.cv.offset_used = Some((num, span));
        }
    }

    /// A four-byte field holding the offset of `sym` within its section,
    /// which an `IMAGE_REL_*_SECREL` fills in.
    fn cv_secrel(&mut self, b: &mut Blob, sym: SymbolId, span: Span) {
        let e = self.exprs.alloc(ExprKind::SymId(sym), span);
        let kind = FixupKind::data(4)
            .with_reloc(self.target().data_reloc(4, false).unwrap_or(0))
            .with_class(RelocClass::SectionRelative)
            .linker_only();
        b.fixup(4, e, kind);
    }

    /// A two-byte field holding the index of `sym`'s section, which an
    /// `IMAGE_REL_*_SECTION` fills in.
    fn cv_secidx(&mut self, b: &mut Blob, sym: SymbolId, span: Span) {
        let e = self.exprs.alloc(ExprKind::SymId(sym), span);
        let kind = FixupKind::data(2)
            .with_reloc(self.target().data_reloc(2, false).unwrap_or(0))
            .with_class(RelocClass::SectionIndex)
            .linker_only();
        b.fixup(2, e, kind);
    }

    /// A four-byte field holding how far `end` is past `start`, which layout
    /// folds to a number wherever the two are in one section.
    fn cv_distance(&mut self, b: &mut Blob, start: SymbolId, end: SymbolId, span: Span) {
        let t = self.exprs.alloc(ExprKind::SymId(end), span);
        let f = self.exprs.alloc(ExprKind::SymId(start), span);
        let e = self.exprs.alloc(ExprKind::Binary(BinOp::Sub, t, f), span);
        let kind = self.abs_kind(4);
        b.fixup(4, e, kind);
    }

    /// How far a `.cv_loc`'s position is past the function's start.
    fn cv_offset_expr(&mut self, pos: Pos, start: SymbolId, span: Span) -> ExprRef {
        let t = self.pos_expr(pos, 0);
        let f = self.exprs.alloc(ExprKind::SymId(start), span);
        self.exprs.alloc(ExprKind::Binary(BinOp::Sub, t, f), span)
    }

    /// Appends one blob to another, keeping its fixups at their new offsets.
    fn cv_append(&mut self, b: &mut Blob, tail: Blob) {
        let base = b.bytes.len() as u32;
        b.bytes.extend_from_slice(&tail.bytes);
        for (offset, size, expr, kind) in tail.fixups {
            b.fixups.push((base + offset, size, expr, kind));
        }
    }

    /// Pads to four bytes with zeros, as llvm-mc's `emitValueToAlignment`
    /// does, which also raises the section's own alignment.
    fn cv_align4(&mut self, sec: SectionId, span: Span) {
        self.section_mut(sec).push(crate::section::Fragment::new(
            FragKind::Align {
                align: 4,
                fill: vec![0],
                max_skip: None,
                pad: 0,
                nop_state: None,
            },
            span,
        ));
        let align = self.section(sec).align.max(4);
        self.section_mut(sec).align = align;
    }

    /// Fills in the length of a subsection whose body is padded, and whose
    /// length is therefore only a number once layout has run: the distance
    /// from the first byte after the length field, eight bytes into the
    /// header fragment at `hdr`, to `end`.
    fn cv_length_fixup(&mut self, hdr: Pos, end: Pos, span: Span) {
        let e = self.difference_expr((hdr, 8), (end, 0));
        let kind = self.abs_kind(4);
        if let FragKind::Bytes { variants, chosen } =
            &mut self.sections[hdr.0.0 as usize].frags[hdr.1 as usize].kind
        {
            variants[*chosen].fixups.push(Fixup {
                offset: 4,
                expr: e,
                kind,
                span,
            });
        }
    }

    /// The section a subsection is about to go into, or `None` after
    /// reporting that it holds no file space: llvm-mc refuses a CodeView
    /// subsection in `.bss` as it refuses any other data there.
    fn cv_section(&mut self, span: Span) -> Option<SectionId> {
        if self.check_nobits(span) {
            return None;
        }
        Some(self.cur)
    }

    /// A bare decimal operand, which is all the `.cv_*` directives take: a
    /// sign is read only for the error it deserves.
    fn cv_int(&mut self, cur: &mut Cursor<'_>, what: &str) -> Option<i64> {
        let tok = cur.peek();
        let TokKind::Int(v) = tok.kind else {
            self.diags.error(tok.span, format!("expected a {what}"));
            return None;
        };
        cur.advance();
        Some(v as i64)
    }

    /// A function id. `UINT_MAX` is not one: llvm-mc stores an inline site's
    /// parent as the id plus one, so the largest id has no room for a child.
    fn cv_func_number(&mut self, cur: &mut Cursor<'_>, span: Span) -> Option<u32> {
        let id = self.cv_int(cur, "function id")?;
        if id >= u32::MAX as i64 {
            self.diags
                .error(span, "expected function id within range [0, UINT_MAX)");
            return None;
        }
        Some(id as u32)
    }

    /// A comma and a symbol name, as `.cv_linetable` separates its operands;
    /// a name rather than an expression, which is all llvm-mc reads there.
    fn cv_comma_symbol(&mut self, cur: &mut Cursor<'_>) -> Option<SymbolId> {
        if cur.eat_punct(Punct::Comma).is_none() {
            self.diags.error(cur.peek().span, "expected a comma");
            return None;
        }
        let (name, span) = self.expect_name(cur)?;
        Some(self.symbols.intern(name, span))
    }
}

/// Hexadecimal bytes as llvm's `fromHex` reads them: an odd number of digits
/// leaves the first one as the low half of the first byte.
fn from_hex(text: &[u8]) -> Vec<u8> {
    let digit = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
    let mut out = Vec::with_capacity(text.len().div_ceil(2));
    let mut rest = text;
    if !rest.is_empty() && !rest.len().is_multiple_of(2) {
        out.push(digit(rest[0]));
        rest = &rest[1..];
    }
    for pair in rest.chunks(2) {
        out.push(digit(pair[0]) << 4 | digit(pair[1]));
    }
    out
}
