//! Line number information: `.file`, `.loc` and `.loc_mark_labels`.
//!
//! A `.loc` names a source position; the row it makes is placed at the next
//! thing that consumes it. What consumes one is where the references part
//! ways, and so is most of what a `.loc` carries over to the next:
//!
//! | | GNU as | llvm-mc |
//! |---|---|---|
//! | consumed by | instructions | instructions, `.byte`-style data |
//! | a section switch | keeps it | discards it |
//! | column, `isa` | carry over | reset |
//! | `is_stmt` | carries over | carries over |
//! | line 0 | no row | a row |
//! | `view` | numbered | ignored |
//!
//! Either way a second `.loc` with the first unconsumed makes the first's row
//! where the second is written. The file table is built as each reference
//! builds it, which is different again; see [`GnuFiles`] and [`LlvmFiles`].

use super::{Flavor, Pos};
use crate::assembler::Assembler;
use crate::cursor::Cursor;
use crate::lexer::{Punct, TokKind};
use crate::section::SectionId;
use crate::source::Span;
use crate::symbol::SymbolId;
use std::collections::HashMap;

/// A `view` operand of `.loc`, which GNU as numbers the rows at one address
/// by, for consumers that need to tell them apart.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum View {
    /// `view -0`: this row starts a new numbering, at 0.
    Reset,
    /// `view 0`: the row's number is asserted to be 0.
    Zero,
    /// `view .LVU3`: the symbol is defined as the row's number.
    Sym(SymbolId),
}

/// The source position a `.loc` names, and its flags.
#[derive(Clone, Debug)]
pub struct Loc {
    pub file: u32,
    pub line: u32,
    pub column: u32,
    pub is_stmt: bool,
    pub basic_block: bool,
    pub prologue_end: bool,
    pub epilogue_begin: bool,
    pub isa: u32,
    pub discriminator: u32,
    pub view: Option<View>,
    /// The `.loc` directive, for diagnostics about the row.
    pub span: Span,
}

impl Default for Loc {
    fn default() -> Loc {
        Loc {
            file: 1,
            line: 1,
            column: 0,
            is_stmt: true,
            basic_block: false,
            prologue_end: false,
            epilogue_begin: false,
            isa: 0,
            discriminator: 0,
            view: None,
            span: Span::DUMMY,
        }
    }
}

/// One row of a line table, at the position that consumed its `.loc`.
#[derive(Clone, Debug)]
pub struct Row {
    pub pos: Pos,
    pub loc: Loc,
    /// Where the row is when it is not at `pos`: this many bytes before the
    /// end of the fragment there. See
    /// [`Architecture::dwarf_row_back`](crate::arch::Architecture::dwarf_row_back).
    pub back: Option<u32>,
    /// For a row generated for the assembly source (`-g`), the source file
    /// it is in, which is only given a number when the table is written.
    pub gen_path: Option<String>,
}

/// A file table entry.
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub name: String,
    pub dir: u32,
    /// The checksum, as the number the source wrote.
    pub md5: Option<u128>,
}

/// The file and directory tables as GNU as builds them (`dwarf2dbg.c`).
///
/// A name with a directory in it is split at its last separator, and the
/// directory entered into one table shared by every version. Entry 0 of that
/// table is `.file 0`'s directory, or the working directory; before DWARF 5
/// it is not written.
#[derive(Default, Debug)]
pub struct GnuFiles {
    pub dirs: Vec<Option<String>>,
    pub files: Vec<Option<FileEntry>>,
}

/// The file and directory tables as llvm-mc builds them
/// (`MCDwarfLineTableHeader`).
///
/// File 0, the root file, is kept apart and never split; `.file 0`'s
/// directory is the compilation directory, which is directory 0 and is not
/// looked up when later names are split.
#[derive(Debug)]
pub struct LlvmFiles {
    pub comp_dir: Option<String>,
    pub root: Option<FileEntry>,
    /// Directories 1 and up.
    pub dirs: Vec<String>,
    pub files: Vec<Option<FileEntry>>,
    pub all_md5: bool,
    pub any_md5: bool,
}

impl Default for LlvmFiles {
    fn default() -> LlvmFiles {
        LlvmFiles {
            comp_dir: None,
            root: None,
            dirs: Vec::new(),
            files: Vec::new(),
            all_md5: true,
            any_md5: false,
        }
    }
}

/// Line table state gathered while the source is read.
#[derive(Default)]
pub struct LineState {
    pub gnu: GnuFiles,
    pub llvm: LlvmFiles,
    /// A `.file 0` was seen, which asks for DWARF 5.
    pub version5: bool,
    /// The last `.loc`, as the next one starts from it.
    pub current: Loc,
    /// `current` has not made its row yet.
    pub pending: bool,
    /// The rows of each section, sections in the order their first row was
    /// made.
    pub sequences: Vec<(SectionId, Vec<Row>)>,
    by_section: HashMap<SectionId, usize>,
    /// `.loc_mark_labels` is on.
    pub mark_labels: bool,
    /// A numbered `.file` or a `.loc` was seen, so the object has a line
    /// table even if no row was made.
    pub used: bool,
    /// The rows are GNU as's, and a `.loc` number of `view` rows was given.
    pub has_views: bool,
    /// The spans of the `.file` directives, for the table's own diagnostics.
    pub file_spans: Vec<(u32, Span)>,
    /// Rows are being generated for the assembly source; see
    /// [`super::source`].
    pub source: super::source::GenState,
}

impl LineState {
    /// Records a row.
    pub fn push_row(&mut self, row: Row) {
        let section = row.pos.0;
        let idx = *self.by_section.entry(section).or_insert_with(|| {
            self.sequences.push((section, Vec::new()));
            self.sequences.len() - 1
        });
        self.sequences[idx].1.push(row);
    }

    /// Indexes the sequences again after some were removed.
    pub fn rebuild_index(&mut self) {
        self.by_section = self
            .sequences
            .iter()
            .enumerate()
            .map(|(i, (s, _))| (*s, i))
            .collect();
    }

    /// Whether anything asks for a `.debug_line` section.
    pub fn is_used(&self) -> bool {
        self.used || !self.sequences.is_empty()
    }
}

/// The part of a path after its last `/`, as `lbasename` finds it, except that
/// GNU as keeps a name whose only separator is a leading one whole.
fn gnu_basename(path: &str) -> usize {
    match path.rfind('/') {
        Some(0) => 0,
        Some(i) => i + 1,
        None => 0,
    }
}

impl GnuFiles {
    /// `allocate_filenum`: the number of a source file a generated row is in,
    /// entering it in the table if it is new. Slot 0 is left to `.file 0`.
    pub(super) fn allocate_generated(&mut self, path: &str, pwd: &str) -> u32 {
        let base = gnu_basename(path);
        let dir = self.directory(path, base, None, false, false, pwd);
        let name = &path[base..];
        let found = (1..self.files.len()).find(|&i| {
            self.files[i]
                .as_ref()
                .is_some_and(|f| f.dir == dir && f.name == name)
        });
        if let Some(i) = found {
            return i as u32;
        }
        let i = self.files.len().max(1);
        self.files.resize(i + 1, None);
        self.files[i] = Some(FileEntry {
            name: name.to_string(),
            dir,
            md5: None,
        });
        i as u32
    }

    /// `get_directory_table_entry`.
    pub(super) fn directory(
        &mut self,
        dirname: &str,
        dirlen: usize,
        file0_dirname: Option<&str>,
        can_use_zero: bool,
        v5: bool,
        pwd: &str,
    ) -> u32 {
        let mut dirlen = dirlen;
        if dirlen == 0 {
            return 0;
        }
        if dirname.as_bytes()[dirlen - 1] == b'/' {
            dirlen -= 1;
            if dirlen == 0 {
                return 0;
            }
        }
        let want = &dirname[..dirlen];
        if let Some(d) = self.dirs.iter().position(|d| d.as_deref() == Some(want)) {
            return d as u32;
        }
        let mut d = self.dirs.len();
        if can_use_zero {
            if self.dirs.first().is_none_or(|d| d.is_none()) {
                let pwd = file0_dirname.unwrap_or(pwd);
                if v5 && dirname != pwd {
                    // DWARF 5's directory 0 is the compilation directory, so
                    // that goes in first.
                    self.directory(pwd, pwd.len(), file0_dirname, true, v5, pwd);
                    d = 1;
                } else {
                    d = 0;
                }
            }
        } else if d == 0 {
            d = 1;
        }
        if self.dirs.len() <= d {
            self.dirs.resize(d + 1, None);
        }
        self.dirs[d] = Some(want.to_string());
        d as u32
    }

    /// `allocate_filename_to_slot`.
    fn allocate(
        &mut self,
        dirname: Option<&str>,
        filename: &str,
        num: u32,
        md5: Option<u128>,
        v5: bool,
        pwd: &str,
    ) -> Result<(), String> {
        let n = num as usize;
        if let Some(Some(existing)) = self.files.get(n) {
            // The same file named again is fine; anything else is not.
            let dir = self.dirs.get(existing.dir as usize).and_then(|d| d.clone());
            let same = md5.is_none_or(|m| existing.md5 == Some(m))
                && match (dirname, &dir) {
                    (Some(dn), Some(d)) => d == dn && filename == existing.name,
                    (Some(_), None) => filename == existing.name,
                    (None, Some(d)) => {
                        filename.len() > d.len()
                            && filename.starts_with(d.as_str())
                            && filename.as_bytes()[d.len()] == b'/'
                            && filename[d.len() + 1..] == existing.name
                    }
                    (None, None) => filename[gnu_basename(filename)..] == existing.name,
                };
            if same {
                return Ok(());
            }
            let show = |d: Option<&str>, f: &str| match d {
                Some(d) => format!("{d}/{f}"),
                None => f.to_string(),
            };
            return Err(format!(
                "file table slot {num} is already occupied by a different file ({} vs {})",
                show(dir.as_deref(), &existing.name),
                show(dirname, filename)
            ));
        }

        let (dir, file) = if num == 0 {
            let base = gnu_basename(filename);
            match dirname {
                Some(dn) if base == 0 => {
                    let d = self.directory(dn, dn.len(), dirname, true, v5, pwd);
                    (d, filename)
                }
                _ => {
                    let d = self.directory(filename, base, dirname, true, v5, pwd);
                    (d, &filename[base..])
                }
            }
        } else {
            match dirname {
                None => {
                    let base = gnu_basename(filename);
                    let d = self.directory(filename, base, None, false, v5, pwd);
                    (d, &filename[base..])
                }
                Some(dn) => {
                    let d = self.directory(dn, dn.len(), None, false, v5, pwd);
                    (d, filename)
                }
            }
        };
        if self.files.len() <= n {
            self.files.resize(n + 1, None);
        }
        self.files[n] = Some(FileEntry {
            name: file.to_string(),
            dir,
            md5,
        });
        Ok(())
    }
}

impl LlvmFiles {
    /// `tryGetFile`, for a numbered `.file` other than 0. Returns the number
    /// the file was given, which is 0 where it is the root file again.
    fn try_get_file(
        &mut self,
        dirname: Option<&str>,
        filename: &str,
        num: u32,
        md5: Option<u128>,
        v5: bool,
    ) -> Result<u32, String> {
        let mut directory = dirname.unwrap_or("");
        if self.comp_dir.as_deref() == Some(directory) {
            directory = "";
        }
        let (mut directory, mut filename) = (directory, filename);
        if filename.is_empty() {
            filename = "<stdin>";
            directory = "";
        }
        if self.files.is_empty() {
            self.track_md5(md5.is_some());
        }
        if v5
            && let Some(root) = &self.root
            && root.name == filename
            && root.md5 == md5
        {
            return Ok(0);
        }
        let n = num as usize;
        if self.files.len() <= n {
            self.files.resize(n + 1, None);
        }
        if self.files[n].is_some() {
            return Err("file number already allocated".into());
        }
        if directory.is_empty()
            && let Some(i) = filename.rfind('/')
            && i + 1 < filename.len()
            && i > 0
        {
            directory = &filename[..i];
            filename = &filename[i + 1..];
        }
        let dir = if directory.is_empty() {
            0
        } else {
            match self.dirs.iter().position(|d| d == directory) {
                Some(i) => i as u32 + 1,
                None => {
                    self.dirs.push(directory.to_string());
                    self.dirs.len() as u32
                }
            }
        };
        self.files[n] = Some(FileEntry {
            name: filename.to_string(),
            dir,
            md5,
        });
        self.track_md5(md5.is_some());
        Ok(num)
    }

    /// The file `-g` describes, entered as the first file.
    pub(super) fn try_get_file_pub(&mut self, name: &str) -> Result<u32, String> {
        self.try_get_file(None, name, 1, None, false)
    }

    fn track_md5(&mut self, used: bool) {
        self.all_md5 &= used;
        self.any_md5 |= used;
    }
}

impl Assembler {
    /// `.file`, in its numbered DWARF forms. The bare `.file "name"` names
    /// the object's source file and is left alone.
    pub(crate) fn dir_dwarf_file(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let (TokKind::Int(_) | TokKind::Punct(Punct::Minus)) = cur.peek().kind else {
            // `.file "name"`: nothing to do for the line table.
            cur.set_pos(cur.all().len());
            return;
        };
        let Some(e) = self.parse_expr(cur) else {
            cur.set_pos(cur.all().len());
            return;
        };
        let Some(num) = self.eval_absolute(e, "file number") else {
            cur.set_pos(cur.all().len());
            return;
        };
        let flavor = self.dwarf_target().flavor;
        if num < 0 {
            self.diags.error(span, "negative file number");
            cur.set_pos(cur.all().len());
            return;
        }
        let Some(first) = self.expect_string_arg(cur) else {
            cur.set_pos(cur.all().len());
            return;
        };
        let (dirname, filename) = match cur.peek().kind {
            TokKind::Str(_) => {
                let Some(second) = self.expect_string_arg(cur) else {
                    return;
                };
                (Some(first), second)
            }
            _ => (None, first),
        };
        let mut md5 = None;
        if let Some(n) = cur.peek().ident()
            && self.interner.get(n).eq_ignore_ascii_case("md5")
        {
            cur.advance();
            let tok = cur.advance();
            let text = match tok.kind {
                TokKind::Int(_) | TokKind::BadNumber(_) => self.sm.span_text(tok.span).to_string(),
                _ => String::new(),
            };
            match parse_md5(&text) {
                Some(v) => md5 = Some(v),
                None => {
                    self.diags.error(
                        tok.span,
                        "expected a hexadecimal MD5 checksum of up to 32 digits",
                    );
                    return;
                }
            }
        }
        // A numbered `.file` means the source brings its own line table, and
        // any made up for it so far goes.
        self.dwarf.line.used = true;
        if self.dwarf.line.source.on {
            self.dwarf_stop_generating();
        }
        let num = num as u32;
        if num == 0 {
            self.dwarf.line.version5 = true;
        }
        let v5 = self.dwarf_line_version() >= 5;
        let result = match flavor {
            Flavor::Gnu => {
                let pwd = current_dir();
                self.dwarf
                    .line
                    .gnu
                    .allocate(dirname.as_deref(), &filename, num, md5, v5, &pwd)
            }
            Flavor::Llvm if num == 0 => {
                let l = &mut self.dwarf.line.llvm;
                l.comp_dir = dirname.clone();
                l.root = Some(FileEntry {
                    name: filename.clone(),
                    dir: 0,
                    md5,
                });
                l.track_md5(md5.is_some());
                Ok(())
            }
            Flavor::Llvm => self
                .dwarf
                .line
                .llvm
                .try_get_file(dirname.as_deref(), &filename, num, md5, v5)
                .map(|_| ()),
        };
        if let Err(msg) = result {
            self.diags.error(span, msg);
            return;
        }
        self.dwarf.line.file_spans.push((num, span));
    }

    /// Reads a string argument, decoded as bytes of UTF-8 where possible.
    fn expect_string_arg(&mut self, cur: &mut Cursor<'_>) -> Option<String> {
        let tok = cur.peek();
        let TokKind::Str(i) = tok.kind else {
            self.diags.error(tok.span, "expected a string");
            return None;
        };
        cur.advance();
        Some(String::from_utf8_lossy(self.pool.get(i)).into_owned())
    }

    /// The DWARF version of the line table.
    pub(crate) fn dwarf_line_version(&self) -> u16 {
        let flavor = self.dwarf_target().flavor;
        let requested = self.options.dwarf_version;
        if self.dwarf.line.version5 {
            return 5;
        }
        match (flavor, requested) {
            (Flavor::Gnu, Some(v)) => v as u16,
            // GNU as's `-g` alone asks for version 2.
            (Flavor::Gnu, None) if self.options.debug_source => 2,
            (Flavor::Gnu, None) => 3,
            (Flavor::Llvm, Some(v)) => v as u16,
            (Flavor::Llvm, None) => 4,
        }
    }

    /// Whether file number `num` is in the table.
    fn dwarf_file_assigned(&self, num: u32) -> bool {
        let line = &self.dwarf.line;
        match self.dwarf_target().flavor {
            Flavor::Gnu => line
                .gnu
                .files
                .get(num as usize)
                .is_some_and(|f| f.is_some()),
            Flavor::Llvm if num == 0 => line.llvm.root.is_some(),
            Flavor::Llvm => line
                .llvm
                .files
                .get(num as usize)
                .is_some_and(|f| f.is_some()),
        }
    }

    /// `.loc fileno lineno [column] [options]`.
    pub(crate) fn dir_loc(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let flavor = self.dwarf_target().flavor;
        // Two `.loc`s in a row: the first makes its row here.
        if self.dwarf.line.pending {
            self.dwarf_loc_row();
        }

        let Some(file) = self.loc_number(cur, "file number") else {
            cur.set_pos(cur.all().len());
            return;
        };
        if file < 1 && (file < 0 || self.dwarf_line_version() < 5) {
            self.diags.error(span, "file number less than one");
            cur.set_pos(cur.all().len());
            return;
        }
        if !self.dwarf_file_assigned(file as u32) {
            self.diags
                .error(span, format!("unassigned file number {file}"));
            cur.set_pos(cur.all().len());
            return;
        }
        let line = if matches!(
            cur.peek().kind,
            TokKind::Int(_) | TokKind::Punct(Punct::Minus)
        ) {
            match self.loc_number(cur, "line number") {
                Some(n) if n >= 0 => n,
                Some(_) => {
                    self.diags.error(span, "line number less than zero");
                    cur.set_pos(cur.all().len());
                    return;
                }
                None => {
                    cur.set_pos(cur.all().len());
                    return;
                }
            }
        } else {
            0
        };

        let prev = self.dwarf.line.current.clone();
        let mut loc = match flavor {
            // GNU as keeps everything but the discriminator and the one-shot
            // flags, which the last row consumed.
            Flavor::Gnu => Loc {
                discriminator: 0,
                basic_block: false,
                prologue_end: false,
                epilogue_begin: false,
                view: None,
                ..prev
            },
            // llvm-mc keeps only `is_stmt`.
            Flavor::Llvm => Loc {
                is_stmt: prev.is_stmt,
                ..Loc::default()
            },
        };
        loc.file = file as u32;
        loc.line = line as u32;
        loc.span = span;
        if let TokKind::Int(_) = cur.peek().kind {
            match self.loc_number(cur, "column") {
                Some(c) if c >= 0 => loc.column = c as u32,
                Some(_) => {
                    self.diags.error(span, "column position less than zero");
                    cur.set_pos(cur.all().len());
                    return;
                }
                None => {
                    cur.set_pos(cur.all().len());
                    return;
                }
            }
        }

        while let Some(n) = cur.peek().ident() {
            let tok = cur.advance();
            let word = self.interner.get(n).to_string();
            match word.as_str() {
                "basic_block" => loc.basic_block = true,
                "prologue_end" => loc.prologue_end = true,
                "epilogue_begin" => loc.epilogue_begin = true,
                "is_stmt" => match self.loc_number(cur, "`is_stmt` value") {
                    Some(0) => loc.is_stmt = false,
                    Some(1) => loc.is_stmt = true,
                    Some(_) => {
                        self.diags.error(tok.span, "is_stmt value not 0 or 1");
                        cur.set_pos(cur.all().len());
                        return;
                    }
                    None => return,
                },
                "isa" => match self.loc_number(cur, "`isa` value") {
                    Some(v) if v >= 0 => loc.isa = v as u32,
                    Some(_) => {
                        self.diags.error(tok.span, "isa number less than zero");
                        cur.set_pos(cur.all().len());
                        return;
                    }
                    None => return,
                },
                "discriminator" => match self.loc_number(cur, "discriminator") {
                    Some(v) if v >= 0 => loc.discriminator = v as u32,
                    Some(_) => {
                        self.diags.error(tok.span, "discriminator less than zero");
                        cur.set_pos(cur.all().len());
                        return;
                    }
                    None => return,
                },
                "view" => {
                    let Some(view) = self.loc_view(cur) else {
                        cur.set_pos(cur.all().len());
                        return;
                    };
                    // llvm-mc has no views; the row is made as if none was
                    // given.
                    if flavor == Flavor::Gnu {
                        loc.view = Some(view);
                    }
                }
                _ => {
                    self.diags
                        .error(tok.span, format!("unknown `.loc` sub-directive `{word}`"));
                    cur.set_pos(cur.all().len());
                    return;
                }
            }
        }

        self.dwarf.line.used = true;
        let has_view = loc.view.is_some();
        self.dwarf.line.current = loc;
        self.dwarf.line.pending = true;
        // GNU as makes a row with a view where the `.loc` is, not at the
        // next instruction.
        if has_view {
            self.dwarf.line.has_views = true;
            self.dwarf_loc_row();
        }
    }

    /// An absolute expression operand of `.loc`.
    fn loc_number(&mut self, cur: &mut Cursor<'_>, what: &str) -> Option<i64> {
        let tok = cur.peek();
        if tok.is_eol() {
            self.diags.error(tok.span, format!("expected a {what}"));
            return None;
        }
        // Only a plain number, so that `5 is_stmt` is not read as one
        // expression; a sign is allowed for the error it deserves.
        let neg = cur.eat_punct(Punct::Minus).is_some();
        let tok = cur.peek();
        let TokKind::Int(v) = tok.kind else {
            self.diags.error(tok.span, format!("expected a {what}"));
            return None;
        };
        cur.advance();
        Some(if neg { -(v as i64) } else { v as i64 })
    }

    /// The operand of `view`: `-0`, `0` or a symbol to define.
    fn loc_view(&mut self, cur: &mut Cursor<'_>) -> Option<View> {
        let tok = cur.peek();
        if let Some(name) = tok.ident() {
            cur.advance();
            let id = self.symbols.intern(name, tok.span);
            if self.symbols.get(id).is_defined() {
                let shown = self.display_name(id);
                self.diags
                    .error(tok.span, format!("symbol `{shown}` is already defined"));
                return None;
            }
            // Defined for now as a difference of two labels at the row, which
            // is 0 but not a constant, so nothing folds it before layout gives
            // it its real value; see `Assembler::assign_views`.
            let pos = self.dwarf_pos();
            let a = self.dwarf_label(pos, tok.span);
            let b = self.dwarf_label(pos, tok.span);
            let ea = self.exprs.alloc(crate::expr::ExprKind::SymId(a), tok.span);
            let eb = self.exprs.alloc(crate::expr::ExprKind::SymId(b), tok.span);
            let e = self.exprs.alloc(
                crate::expr::ExprKind::Binary(crate::expr::BinOp::Sub, ea, eb),
                tok.span,
            );
            let sym = self.symbols.get_mut(id);
            sym.value = crate::symbol::SymbolValue::Expr(e);
            sym.def_span = tok.span;
            self.symbols.mark_defined(id);
            return Some(View::Sym(id));
        }
        let neg = cur.eat_punct(Punct::Minus).is_some();
        let tok = cur.peek();
        match tok.kind {
            TokKind::Int(0) => {
                cur.advance();
                Some(if neg { View::Reset } else { View::Zero })
            }
            _ => {
                self.diags
                    .error(tok.span, "numeric view can only be asserted to zero");
                None
            }
        }
    }

    /// Makes the pending `.loc`'s row at the current position, as a second
    /// `.loc` does.
    fn dwarf_loc_row(&mut self) {
        let pos = self.dwarf_pos();
        self.dwarf_consume_loc(pos, None);
    }

    /// Makes the row of the pending `.loc`, if there is one, at `pos`, or
    /// `back` bytes before the end of the fragment there: called where an
    /// instruction is emitted, and on llvm-mc's targets where data is. The
    /// row is dropped where the reference drops it.
    fn dwarf_consume_loc(&mut self, pos: Pos, back: Option<u32>) {
        if !self.dwarf.line.pending {
            return;
        }
        self.dwarf.line.pending = false;
        let flavor = self.dwarf_target().flavor;
        let loc = self.dwarf.line.current.clone();
        if flavor == Flavor::Gnu {
            // The one-shot flags belong to this row only.
            let c = &mut self.dwarf.line.current;
            c.basic_block = false;
            c.prologue_end = false;
            c.epilogue_begin = false;
            c.discriminator = 0;
            c.view = None;
            if loc.line == 0 {
                return;
            }
            let flags = &self.section(pos.0).flags;
            if !(flags.alloc && flags.exec) {
                let name = self.interner.get(self.section(pos.0).name).to_string();
                self.diags.warning(
                    loc.span,
                    format!("dwarf line number information for {name} ignored"),
                );
                return;
            }
        }
        self.dwarf.line.push_row(Row {
            pos,
            loc,
            back,
            gen_path: None,
        });
    }

    /// Called where an instruction is about to be emitted as fragment `pos`,
    /// with its candidate encodings.
    pub(crate) fn dwarf_instruction(
        &mut self,
        pos: Pos,
        variants: &[crate::section::Variant],
        span: Span,
    ) {
        let back = match variants.first() {
            Some(v) => self.arch.dwarf_row_back(v),
            None => None,
        };
        if self.dwarf.line.source.on {
            self.dwarf_source_row(pos, back, span);
        }
        self.dwarf_consume_loc(pos, back);
    }

    /// Called where `.byte`-style data is emitted: llvm-mc gives it the
    /// pending `.loc`'s row, GNU as does not.
    pub(crate) fn dwarf_data(&mut self) {
        if self.dwarf.line.pending && self.dwarf_target().flavor == Flavor::Llvm {
            self.dwarf_loc_row();
        }
    }

    /// Called on a section switch: llvm-mc forgets a `.loc` that nothing in
    /// the old section consumed.
    pub(crate) fn dwarf_section_switch(&mut self) {
        if self.dwarf.line.pending && self.dwarf_target().flavor == Flavor::Llvm {
            self.dwarf.line.pending = false;
        }
    }

    /// Called where a label is defined: with `.loc_mark_labels` on, GNU as
    /// makes a row there, marked as the start of a basic block.
    pub(crate) fn dwarf_label_defined(&mut self) {
        let line = &self.dwarf.line;
        if !line.mark_labels || self.dwarf_target().flavor != Flavor::Gnu {
            return;
        }
        if line.gnu.files.iter().all(|f| f.is_none()) || !self.section(self.cur).flags.exec {
            return;
        }
        let pos = self.dwarf_pos();
        let mut loc = self.dwarf.line.current.clone();
        loc.basic_block = true;
        self.dwarf.line.push_row(Row {
            pos,
            loc,
            back: None,
            gen_path: None,
        });
        self.dwarf.line.pending = false;
        let c = &mut self.dwarf.line.current;
        c.basic_block = false;
        c.prologue_end = false;
        c.epilogue_begin = false;
        c.discriminator = 0;
        c.view = None;
    }

    /// `.loc_mark_labels value`.
    pub(crate) fn dir_loc_mark_labels(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let Some(e) = self.parse_expr(cur) else {
            return;
        };
        if let Some(v) = self.eval_absolute(e, "`.loc_mark_labels` value") {
            self.dwarf.line.mark_labels = v != 0;
        }
        let _ = span;
    }

    /// A label for a DWARF position, which the object's symbol table never
    /// sees unless a relocation has to name it.
    pub(crate) fn dwarf_label(&mut self, pos: Pos, span: Span) -> SymbolId {
        let n = self.symbols.len();
        let name = self.interner.intern(&format!(".L\u{0}dwarf.{n}"));
        let id = self.symbols.intern(name, span);
        let sym = self.symbols.get_mut(id);
        sym.value = crate::symbol::SymbolValue::Label {
            section: pos.0,
            frag: pos.1,
        };
        sym.def_span = span;
        // A label the assembler makes for itself is defined after everything
        // the source defined, which is what decides which atom of a Mach-O
        // section it belongs to when several labels sit at one position.
        self.symbols.mark_defined(id);
        id
    }
}

/// Parses an MD5 checksum written as a hexadecimal number.
fn parse_md5(text: &str) -> Option<u128> {
    let hex = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))?;
    if hex.is_empty() || hex.len() > 32 {
        return None;
    }
    u128::from_str_radix(hex, 16).ok()
}

/// The working directory, which is directory 0 of a DWARF 5 table unless
/// `.file 0` names one.
pub(crate) fn current_dir() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_is_read_as_one_128_bit_number() {
        assert_eq!(
            parse_md5("0x00112233445566778899aabbccddeeff"),
            Some(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff)
        );
        assert_eq!(parse_md5("0x1"), Some(1));
        assert_eq!(parse_md5("1234"), None);
        assert_eq!(parse_md5("0x"), None);
    }

    #[test]
    fn gnu_splits_names_into_directories_as_dwarf2dbg_does() {
        // `.file 0 "/w" "a.c"`, then a directory of its own, then one found
        // again as directory 0.
        let mut t = GnuFiles::default();
        t.allocate(Some("/w"), "a.c", 0, None, true, "/pwd")
            .unwrap();
        t.allocate(None, "sub/b.h", 1, None, true, "/pwd").unwrap();
        t.allocate(None, "/w/c.h", 2, None, true, "/pwd").unwrap();
        assert_eq!(
            t.dirs,
            vec![Some("/w".to_string()), Some("sub".to_string())]
        );
        let dirs: Vec<u32> = t.files.iter().map(|f| f.as_ref().unwrap().dir).collect();
        assert_eq!(dirs, vec![0, 1, 0]);
    }
}
