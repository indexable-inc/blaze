//! Names at module grain (docs/v2/plan/design/drive.md, "Names at module grain", D116): a
//! module's tables as records of LINKS, and its `Imports`, each read one CELL at a time.
//!
//! # `ModuleLinks`: a module's two tables
//!
//! A module has two tables of one form ([`Table`]): its OUTLINE, what its own text declares and
//! writes, and its MADE table, what its item-position macro calls made once expanded (written
//! only for a module that has such calls). A row is one cell, a `(namespace, name)` under the
//! 128-bit key the caller gives (blaze-scope's `cell_key`, the one home of the recipe), and holds
//! a LINK, never a copy of another module's names:
//!
//! - [`Target::Def`]: the name is this definition (an item, a `mod` by its identity, an
//!   `extern crate`, a `#[macro_export]`): a declaration answers.
//! - [`Target::Path`]: the name is what this written path names (a `use`): the reader follows
//!   it, segment by segment, each a cell read of the table it reaches.
//!
//! A glob is a link of the same two kinds in the record's glob list: a name reached through it is
//! found by looking it up in the source module's own tables, so the record's bytes never grow with
//! the source's size, and an export change of the source moves only the readers that looked that
//! name up there.
//!
//! # Part reads
//!
//! [`links_part`] and [`imports_part`] are the kinds' `part(record bytes, cell) -> digest |
//! Absent` (drive.md "Part reads"): a binary search of the mapped bytes, no allocation, no
//! hashing (each row's digest is written with it). A name absent from a table whose module has
//! glob links answers the digest of those links (the head's): a reader that did not find the name
//! there went on through them, so a glob added or removed moves it, and only it.
//!
//! # Bytes (all integers little-endian; every list sorted, so two builders write equal bytes)
//!
//! `ModuleLinks`, form [`LINKS_FORM`]: a head of 96 bytes (`BZML`, form u32, the module's key,
//! the row count u32, the glob count u32, the tail's length u32, the table byte, 11 zero bytes,
//! the glob links' digest or zeros); the rows, 144 bytes each, sorted by cell (cell, namespace,
//! link tag, kind tag, shapes or `super` depth, visibility tag, origin, expansion u16, the name's
//! (offset, length) in the tail, the path's segment list (offset, count) in the tail (a
//! definition's: its written `pub(in path)`'s), the textual order u32, a written visibility's
//! path start and `super` depth, 2 zero bytes, the target key or `$crate`'s crate, the
//! visibility's key (a written one's `$crate` crate), the row's digest); the glob
//! links, 112 bytes each, sorted by digest (a row's layout from byte 16 to 112, cell and name
//! zero); the tail: names, each written once (a name and a path segment of one text share it),
//! and segment lists, each 4-aligned, of (offset, length) u32 pairs.
//!
//! `Imports`, form [`IMPORTS_FORM`]: a head of 96 bytes (`BZMI`, form u32, the module's key, the
//! counts of rows, traits, glob sources and findings u32 each, the count of imports left
//! undecided u32, 4 zero bytes, the TRAITS digest); the rows, 128 bytes each, sorted by cell
//! (cell, namespace, kind tag, shapes, visibility tag, the `use`'s index u32, 8 zero bytes, the
//! definition's key, the visibility's key, the row's digest); the traits in scope (keys, sorted);
//! the glob sources whose traits are in scope too (module keys, sorted); the findings, 16 bytes
//! each, sorted (the `use`'s index u32, the failed segment u32, the reason and ambiguity bytes).

use core::cmp::Ordering;
use core::fmt;

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_id::{ContentId, ItemKey};
use blaze_own::{AVec, Alloc};

use crate::column::RecordBytes;
use crate::error::BuildError;
use crate::kind::{Def, DefKind, DefKindTag, MacroShapes, Namespace, Origin, Visibility, VisibilityTag, usize_of};
use crate::module_scope::Unresolved;
use crate::path::{PathKind, PathTag};

pub const LINKS_MAGIC: [u8; 4] = *b"BZML";
/// 1: the first form (D116 piece (e), 2026-11-17). 2: the `Textual` and `Exported` tables, a
/// row's text order (bytes 40..44), a `pub(in path)` written as its path (2026-11-17).
pub const LINKS_FORM: u32 = 2;
pub const IMPORTS_MAGIC: [u8; 4] = *b"BZMI";
/// 1: the first form (D116 piece (e), 2026-11-17).
pub const IMPORTS_FORM: u32 = 1;

/// The domain of one `ModuleLinks` row's digest.
pub const LINK_DOMAIN: &str = "blaze v2 module link 1";
/// The domain of a table's glob links' digest.
pub const GLOB_LINKS_DOMAIN: &str = "blaze v2 module glob links 1";
/// The domain of one `Imports` row's digest.
pub const IMPORT_DOMAIN: &str = "blaze v2 module import 1";
/// The domain of an `Imports` record's TRAITS column digest.
pub const TRAITS_DOMAIN: &str = "blaze v2 module traits 1";

/// The cell of an `Imports` record's TRAITS column (its traits in scope and its glob sources):
/// what a glob importer's traits read of it. A names cell is the first 16 bytes of a BLAKE3
/// output, so it is this one only by a 2^-128 chance, which the writer refuses (rule 21).
pub const TRAITS_CELL: [u8; 16] = [0xFF; 16];

const CELL: usize = 16;
const KEY: usize = 32;
const LINKS_HEAD: usize = 96;
const LINK_ROW: usize = 144;
const GLOB_ROW: usize = 112;
const IMPORTS_HEAD: usize = 96;
const IMPORT_ROW: usize = 128;
const FINDING: usize = 16;

const TAG_DEF: u8 = 0;
const TAG_PATH: u8 = 1;

/// A row's visibility byte when the visibility is a written `pub(in path)` that leaves the
/// module chain: no `VisibilityTag`. The path is the row's segment list (a definition row has
/// none of its own), its start at bytes 44 and 45, a `$crate`'s crate in the visibility key.
const VIS_WRITTEN: u8 = 0xFE;

/// The `use` index of an `Imports` RESOLVED row that is no import: the decided visibility of a
/// declaration whose outline row holds its `pub(in path)` as written ([`RowVis::Written`]).
pub const DECLARED_VIS: u32 = u32::MAX;

/// The cell of a `Textual` table's mark row for `child`: where the child's `mod` line falls in
/// this module's textual sequence. The first 16 bytes of the child's key, a 2^-128 chance of a
/// name's cell, which the writer refuses as `Twice` (rule 21).
pub fn mark_cell(child: ItemKey) -> [u8; CELL] {
    let mut cell = [0u8; CELL];
    cell.copy_from_slice(&child.0.0[..CELL]);
    cell
}

/// Which of a module's tables a `ModuleLinks` record is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Table {
    /// What the module's own text declares and writes: made from text alone, no wait.
    Outline,
    /// What the module's item-position macro calls made once expanded.
    Made,
    /// The module's textual `macro_rules!` scope (its own and its `#[macro_use]` children's, in
    /// the order they are visible): one row a name, the last definition, with its place in the
    /// order; and a mark row a `mod` child ([`mark_cell`]) whose order is the length of the
    /// sequence at the child's `mod` line. Written only for a module whose sequence is not empty.
    Textual,
    /// A crate root's `#[macro_export]` macros (D116 point 6, one table a crate): bindings of
    /// the root for other crates, read after the root's outline and made table.
    Exported,
}

impl Table {
    const fn byte(self) -> u8 {
        match self {
            Table::Outline => 0,
            Table::Made => 1,
            Table::Textual => 2,
            Table::Exported => 3,
        }
    }

    const fn from_byte(byte: u8) -> Option<Table> {
        match byte {
            0 => Some(Table::Outline),
            1 => Some(Table::Made),
            2 => Some(Table::Textual),
            3 => Some(Table::Exported),
            _ => None,
        }
    }
}

/// What a cell of a record holds today: its digest, or nothing there.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CellSeen {
    Is(ContentId),
    Absent,
}

/// A names record refused a read: its bytes are not of the form its reader reads (torn, of
/// another kind or form). `what` names the field.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TableRefused {
    pub what: &'static str,
}

impl fmt::Display for TableRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a names record at module grain refused: {}", self.what)
    }
}

impl core::error::Error for TableRefused {}

const fn refused(what: &'static str) -> TableRefused {
    TableRefused { what }
}

/// Where a link leads, as a builder is given it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum To {
    Def(Def),
    /// A written path: its start and its segments at `segs[at..at + len]` of the builder.
    Path { kind: PathKind, at: usize, len: usize },
}

#[derive(Clone, Copy)]
struct NameRow<'n> {
    cell: [u8; CELL],
    ns: Namespace,
    name: &'n str,
    to: To,
    vis: Visibility,
    origin: Origin,
    expansion: u16,
    /// A `Textual` row's place in the module's textual order; a mark row's length; else 0.
    order: u32,
    /// A `Textual` row's first definition's place (bytes 44..48, which only a written
    /// visibility uses otherwise); else 0.
    first: u32,
    /// A definition's `pub(in path)` as written (a `To::Path`), in place of `vis`.
    written: Option<To>,
}

#[derive(Clone, Copy)]
struct GlobRow {
    to: To,
    vis: Visibility,
    expansion: u16,
}

/// One table of one module, as its writer fills it: [`LinksBuilder::write`] writes the
/// `ModuleLinks` record. Names are borrowed for `'n`; rows are kept in `'a`.
pub struct LinksBuilder<'n, 'a> {
    module: ItemKey,
    table: Table,
    names: AVec<'a, NameRow<'n>>,
    globs: AVec<'a, GlobRow>,
    segs: AVec<'a, &'n str>,
}

impl<'n, 'a> LinksBuilder<'n, 'a> {
    pub fn new_in(module: ItemKey, table: Table, alloc: Alloc<'a>) -> LinksBuilder<'n, 'a> {
        LinksBuilder { module, table, names: AVec::new_in(alloc), globs: AVec::new_in(alloc), segs: AVec::new_in(alloc) }
    }

    pub fn module(&self) -> ItemKey {
        self.module
    }

    pub fn table(&self) -> Table {
        self.table
    }

    /// Rows and glob links given so far.
    pub fn len(&self) -> usize {
        self.names.len() + self.globs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The cell `(ns, name)` (its key `cell`) is the definition `def`.
    #[expect(clippy::too_many_arguments, reason = "one row's fields, each named at the call")]
    pub fn def(&mut self, cell: [u8; CELL], ns: Namespace, name: &'n str, def: Def, vis: Visibility, origin: Origin, expansion: u16) {
        self.names.push(NameRow { cell, ns, name, to: To::Def(def), vis, origin, expansion, order: 0, first: 0, written: None });
    }

    /// The cell `(ns, name)` is the definition `def`, whose visibility is the `pub(in kind
    /// segments)` written on it: decided in the module's `Imports` (a [`DECLARED_VIS`] row), so
    /// this row stays a function of text (rule 21).
    #[expect(clippy::too_many_arguments, reason = "one row's fields, each named at the call")]
    pub fn def_written_vis<I: IntoIterator<Item = &'n str>>(
        &mut self,
        cell: [u8; CELL],
        ns: Namespace,
        name: &'n str,
        def: Def,
        kind: PathKind,
        segments: I,
        origin: Origin,
        expansion: u16,
    ) {
        let written = Some(self.take_path(kind, segments));
        self.names.push(NameRow { cell, ns, name, to: To::Def(def), vis: Visibility::Public, origin, expansion, order: 0, first: 0, written });
    }

    /// A `Textual` row: `name`'s last `macro_rules!` in the module's textual sequence, at place
    /// `order` there; `first` is the place of its first one (a child whose mark is at or before
    /// it sees none of them).
    pub fn textual(&mut self, cell: [u8; CELL], name: &'n str, def: Def, expansion: u16, order: u32, first: u32) {
        let row = NameRow { cell, ns: Namespace::Macros, name, to: To::Def(def), vis: Visibility::Public, origin: Origin::Declared, expansion, order, first, written: None };
        self.names.push(row);
    }

    /// A `Textual` mark row: `child`'s `mod` line falls after the first `len` entries.
    pub fn mark(&mut self, child: ItemKey, len: u32) {
        let row = NameRow {
            cell: mark_cell(child),
            ns: Namespace::Types,
            name: "",
            to: To::Def(Def::module(child)),
            vis: Visibility::Public,
            origin: Origin::Declared,
            expansion: 0,
            order: len,
            first: 0,
            written: None,
        };
        self.names.push(row);
    }

    /// The cell `(ns, name)` is what the written path `kind segments` names (a `use`).
    #[expect(clippy::too_many_arguments, reason = "one row's fields, each named at the call")]
    pub fn path<I: IntoIterator<Item = &'n str>>(
        &mut self,
        cell: [u8; CELL],
        ns: Namespace,
        name: &'n str,
        kind: PathKind,
        segments: I,
        vis: Visibility,
        origin: Origin,
        expansion: u16,
    ) {
        let to = self.take_path(kind, segments);
        self.names.push(NameRow { cell, ns, name, to, vis, origin, expansion, order: 0, first: 0, written: None });
    }

    /// A glob link whose source the writer knows by identity (a module or an enum).
    pub fn glob_def(&mut self, source: Def, vis: Visibility, expansion: u16) {
        self.globs.push(GlobRow { to: To::Def(source), vis, expansion });
    }

    /// A glob link as written: `use kind segments::*`.
    pub fn glob_path<I: IntoIterator<Item = &'n str>>(&mut self, kind: PathKind, segments: I, vis: Visibility, expansion: u16) {
        let to = self.take_path(kind, segments);
        self.globs.push(GlobRow { to, vis, expansion });
    }

    fn take_path<I: IntoIterator<Item = &'n str>>(&mut self, kind: PathKind, segments: I) -> To {
        let at = self.segs.len();
        for s in segments {
            self.segs.push(s);
        }
        To::Path { kind, at, len: self.segs.len() - at }
    }

    /// The record. Two rows of one cell are refused (`Twice`): the writer keeps one binding a
    /// cell (rustc's `Duplicate` is the solve's finding, not a second row), and two names under
    /// one key is a collision of the key, never a merge (rule 21).
    pub fn write<'o>(&self, alloc: Alloc<'o>) -> Result<RecordBytes<'o>, BuildError> {
        let mut order: AVec<'o, usize> = AVec::with_capacity_in(self.names.len(), alloc);
        for i in 0..self.names.len() {
            order.push(i);
        }
        order.sort_unstable_by(|&a, &b| self.names[a].cell.cmp(&self.names[b].cell));
        for pair in order.windows(2) {
            let cell = self.names[pair[1]].cell;
            if self.names[pair[0]].cell == cell {
                return Err(BuildError::Twice { table: "ModuleLinks names", key: cell_id(cell) });
            }
        }
        let mut tail: AVec<'o, u8> = AVec::new_in(alloc);
        let mut interned: Map<&'n str, (u32, u32), FixedState, Alloc<'o>> = Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None });
        let mut scratch: AVec<'o, u8> = AVec::new_in(alloc);
        let mut spans: AVec<'o, (u32, u32)> = AVec::new_in(alloc);
        let mut rows: AVec<'o, u8> = AVec::with_capacity_in(self.names.len() * LINK_ROW, alloc);
        for &i in order.iter() {
            let r = self.names[i];
            let name = intern(&mut tail, &mut interned, r.name)?;
            // A definition has no path of its own: its list slot holds a written visibility's.
            let list = self.segment_list(r.written.unwrap_or(r.to), &mut tail, &mut interned, &mut spans)?;
            let mut row = [0u8; LINK_ROW];
            row[..CELL].copy_from_slice(&r.cell);
            row[16] = r.ns.byte();
            code_link(&mut row, r.to, r.vis, r.expansion);
            if let Some(To::Path { kind, .. }) = r.written {
                let (tag, depth, krate) = kind.to_record();
                row[20] = VIS_WRITTEN;
                row[44] = tag.byte();
                row[45] = depth;
                row[80..112].copy_from_slice(&krate.0);
            }
            row[21] = r.origin.byte();
            row[24..28].copy_from_slice(&name.0.to_le_bytes());
            row[28..32].copy_from_slice(&name.1.to_le_bytes());
            row[32..36].copy_from_slice(&list.0.to_le_bytes());
            row[36..40].copy_from_slice(&list.1.to_le_bytes());
            row[40..44].copy_from_slice(&r.order.to_le_bytes());
            if r.written.is_none() {
                row[44..48].copy_from_slice(&r.first.to_le_bytes());
            }
            scratch.clear();
            scratch.extend_from_slice(&row[16..24]);
            scratch.extend_from_slice(&row[40..112]);
            text_part(&mut scratch, r.name)?;
            self.path_part(&mut scratch, r.written.unwrap_or(r.to))?;
            row[112..].copy_from_slice(&ContentId::of(LINK_DOMAIN, &scratch).0);
            rows.extend_from_slice(&row);
        }
        // Glob links: each coded with its digest, then sorted by it.
        let mut globs: AVec<'o, [u8; GLOB_ROW + KEY]> = AVec::with_capacity_in(self.globs.len(), alloc);
        for g in self.globs.iter() {
            let list = self.segment_list(g.to, &mut tail, &mut interned, &mut spans)?;
            let mut row = [0u8; GLOB_ROW + KEY];
            code_link(&mut row, g.to, g.vis, g.expansion);
            row[32..36].copy_from_slice(&list.0.to_le_bytes());
            row[36..40].copy_from_slice(&list.1.to_le_bytes());
            scratch.clear();
            scratch.extend_from_slice(&row[16..24]);
            scratch.extend_from_slice(&row[48..112]);
            self.path_part(&mut scratch, g.to)?;
            row[GLOB_ROW..].copy_from_slice(&ContentId::of(LINK_DOMAIN, &scratch).0);
            globs.push(row);
        }
        // Two equal digests are one link written twice (`use a::*;` twice): one fact, one row.
        globs.sort_unstable_by(|a, b| a[GLOB_ROW..].cmp(&b[GLOB_ROW..]));
        globs.dedup_by(|a, b| a[GLOB_ROW..] == b[GLOB_ROW..]);
        let globs_digest = if globs.is_empty() {
            ContentId([0; KEY])
        } else {
            scratch.clear();
            for g in globs.iter() {
                scratch.extend_from_slice(&g[GLOB_ROW..]);
            }
            ContentId::of(GLOB_LINKS_DOMAIN, &scratch)
        };
        let mut out = RecordBytes::new_in(alloc);
        out.extend_from_slice(&LINKS_MAGIC);
        out.extend_from_slice(&LINKS_FORM.to_le_bytes());
        out.extend_from_slice(&self.module.0.0);
        out.extend_from_slice(&len32(self.names.len(), "ModuleLinks names")?.to_le_bytes());
        out.extend_from_slice(&len32(globs.len(), "ModuleLinks globs")?.to_le_bytes());
        out.extend_from_slice(&len32(tail.len(), "ModuleLinks tail")?.to_le_bytes());
        out.push(self.table.byte());
        out.extend_from_slice(&[0u8; 11]);
        out.extend_from_slice(&globs_digest.0);
        out.extend_from_slice(&rows);
        for g in globs.iter() {
            out.extend_from_slice(&g[..GLOB_ROW]);
        }
        out.extend_from_slice(&tail);
        Ok(out)
    }

    /// A path link's segment list written into the tail (4-aligned); `(0, 0)` for a `Def`.
    fn segment_list<'o>(
        &self,
        to: To,
        tail: &mut AVec<'o, u8>,
        interned: &mut Map<&'n str, (u32, u32), FixedState, Alloc<'o>>,
        spans: &mut AVec<'o, (u32, u32)>,
    ) -> Result<(u32, u32), BuildError> {
        let To::Path { at, len, .. } = to else { return Ok((0, 0)) };
        spans.clear();
        for &s in self.segs[at..at + len].iter() {
            spans.push(intern(tail, interned, s)?);
        }
        while tail.len() % 4 != 0 {
            tail.push(0);
        }
        let start = len32(tail.len(), "ModuleLinks tail")?;
        for &(a, l) in spans.iter() {
            tail.extend_from_slice(&a.to_le_bytes());
            tail.extend_from_slice(&l.to_le_bytes());
        }
        Ok((start, len32(len, "ModuleLinks path")?))
    }

    /// The text of a path link's segments, length-prefixed, for its digest.
    fn path_part(&self, scratch: &mut AVec<'_, u8>, to: To) -> Result<(), BuildError> {
        if let To::Path { at, len, .. } = to {
            scratch.extend_from_slice(&len32(len, "ModuleLinks path")?.to_le_bytes());
            for &s in self.segs[at..at + len].iter() {
                text_part(scratch, s)?;
            }
        }
        Ok(())
    }
}

/// A cell as a key a `Twice` error names: its 16 bytes, then zeros.
fn cell_id(cell: [u8; CELL]) -> ContentId {
    let mut key = [0u8; KEY];
    key[..CELL].copy_from_slice(&cell);
    ContentId(key)
}

fn len32(n: usize, table: &'static str) -> Result<u32, BuildError> {
    u32::try_from(n).map_err(|_| BuildError::TooLarge { table })
}

fn text_part(scratch: &mut AVec<'_, u8>, text: &str) -> Result<(), BuildError> {
    scratch.extend_from_slice(&len32(text.len(), "ModuleLinks name")?.to_le_bytes());
    scratch.extend_from_slice(text.as_bytes());
    Ok(())
}

/// `text` in the tail, written once: its (offset, length).
fn intern<'n>(tail: &mut AVec<'_, u8>, interned: &mut Map<&'n str, (u32, u32), FixedState, Alloc<'_>>, text: &'n str) -> Result<(u32, u32), BuildError> {
    if let Some(&span) = interned.get(&text) {
        return Ok(span);
    }
    let span = (len32(tail.len(), "ModuleLinks tail")?, len32(text.len(), "ModuleLinks name")?);
    tail.extend_from_slice(text.as_bytes());
    interned.insert(text, span);
    Ok(span)
}

/// Bytes 17..24 and 48..112 of a row: the link's tag, kind, shapes or depth, visibility,
/// expansion, target and visibility key.
fn code_link(row: &mut [u8], to: To, vis: Visibility, expansion: u16) {
    match to {
        To::Def(def) => {
            let (kind, shapes) = def.kind.to_record();
            row[17] = TAG_DEF;
            row[18] = kind.byte();
            row[19] = shapes.byte();
            row[48..80].copy_from_slice(&def.key.0.0);
        }
        To::Path { kind, .. } => {
            let (tag, depth, krate) = kind.to_record();
            row[17] = TAG_PATH;
            row[18] = tag.byte();
            row[19] = depth;
            row[48..80].copy_from_slice(&krate.0);
        }
    }
    let (tag, key) = vis.to_record();
    row[20] = tag.byte();
    row[22..24].copy_from_slice(&expansion.to_le_bytes());
    row[80..112].copy_from_slice(&key.0);
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn key_at(b: &[u8], at: usize) -> ContentId {
    let mut key = [0u8; KEY];
    key.copy_from_slice(&b[at..at + KEY]);
    ContentId(key)
}

fn head_of(bytes: &[u8], magic: [u8; 4], form: u32, head: usize) -> Result<(), TableRefused> {
    if bytes.len() < head {
        return Err(refused("a record shorter than its head (torn)"));
    }
    if bytes[..4] != magic {
        return Err(refused("a record of another kind (its magic)"));
    }
    if u32_at(bytes, 4) != form {
        return Err(refused("a record of another form"));
    }
    Ok(())
}

/// The row of `cell` among `n` rows of `width` bytes sorted by their first 16: a binary search.
pub(crate) fn find_cell<'b>(rows: &'b [u8], width: usize, n: usize, cell: &[u8; CELL]) -> Option<&'b [u8]> {
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let row = &rows[mid * width..(mid + 1) * width];
        match row[..CELL].cmp(&cell[..]) {
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid,
            Ordering::Equal => return Some(row),
        }
    }
    None
}

/// That `n` rows of `width` bytes are sorted by cell, each cell once: what a fetch verifies
/// once ([`LinksView::check`], [`ImportsView::check`]); a part read trusts it.
fn check_sorted(rows: &[u8], width: usize, n: usize) -> Result<(), TableRefused> {
    for i in 1..n {
        if rows[(i - 1) * width..(i - 1) * width + CELL] >= rows[i * width..i * width + CELL] {
            return Err(refused("rows not sorted by cell, or a cell twice"));
        }
    }
    Ok(())
}

/// A `ModuleLinks` record read in place.
#[derive(Clone, Copy, Debug)]
pub struct LinksView<'b> {
    module: ItemKey,
    table: Table,
    n_rows: usize,
    n_globs: usize,
    rows: &'b [u8],
    globs: &'b [u8],
    tail: &'b [u8],
    globs_digest: ContentId,
}

/// One row of a `ModuleLinks` record.
#[derive(Clone, Copy, Debug)]
pub struct LinkView<'b> {
    pub ns: Namespace,
    pub name: &'b str,
    pub to: TargetView<'b>,
    pub vis: RowVis<'b>,
    pub origin: Origin,
    pub expansion: u16,
    /// A `Textual` row's place in the textual order, a mark row's length; else 0.
    pub order: u32,
    /// A `Textual` row's first definition's place; else 0.
    pub first: u32,
    /// What a part read of this cell sees.
    pub digest: ContentId,
}

/// A row's visibility as the record holds it.
#[derive(Clone, Copy, Debug)]
pub enum RowVis<'b> {
    /// Decided from text alone (`pub`, `pub(crate)`, private, `pub(super)`, a `pub(in path)`
    /// that walks the module chain).
    Is(Visibility),
    /// A definition's `pub(in path)` whose path leaves the module chain, as written: its value
    /// is the module's `Imports` row of the cell with index [`DECLARED_VIS`].
    Written { kind: PathKind, segments: Segments<'b> },
}

/// One glob link of a `ModuleLinks` record.
#[derive(Clone, Copy, Debug)]
pub struct GlobView<'b> {
    pub to: TargetView<'b>,
    pub vis: Visibility,
    pub expansion: u16,
}

/// Where a link leads, read from a record.
#[derive(Clone, Copy, Debug)]
pub enum TargetView<'b> {
    Def(Def),
    Path { kind: PathKind, segments: Segments<'b> },
}

/// A path link's segments, read in place.
#[derive(Clone, Copy, Debug)]
pub struct Segments<'b> {
    list: &'b [u8],
    tail: &'b [u8],
}

impl<'b> Segments<'b> {
    pub fn len(&self) -> usize {
        self.list.len() / 8
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn get(&self, i: usize) -> Result<&'b str, TableRefused> {
        if i >= self.len() {
            return Err(refused("a path segment past its list"));
        }
        str_at(self.tail, u32_at(self.list, i * 8), u32_at(self.list, i * 8 + 4))
    }
}

fn str_at(tail: &[u8], at: u32, len: u32) -> Result<&str, TableRefused> {
    let (at, len) = (usize_of(at), usize_of(len));
    let end = at.checked_add(len).filter(|&e| e <= tail.len()).ok_or(refused("a name past the record's tail"))?;
    core::str::from_utf8(&tail[at..end]).map_err(|_| refused("a name that is not UTF-8"))
}

fn target_of<'b>(row: &'b [u8], tail: &'b [u8]) -> Result<TargetView<'b>, TableRefused> {
    match row[17] {
        TAG_DEF => {
            let kind = DefKindTag::from_byte(row[18]).ok_or(refused("a byte that is no definition kind"))?;
            let shapes = MacroShapes::from_byte(row[19]).ok_or(refused("a byte that is no set of macro call shapes"))?;
            Ok(TargetView::Def(Def { key: ItemKey(key_at(row, 48)), kind: DefKind::from_record(kind, shapes) }))
        }
        TAG_PATH => {
            let tag = PathTag::from_byte(row[18]).ok_or(refused("a byte that is no path start"))?;
            let kind = PathKind::from_record(tag, row[19], key_at(row, 48));
            Ok(TargetView::Path { kind, segments: segments_of(row, tail)? })
        }
        _ => Err(refused("a byte that is no link kind")),
    }
}

/// The segment list a row's bytes 32..40 name in the tail.
fn segments_of<'b>(row: &[u8], tail: &'b [u8]) -> Result<Segments<'b>, TableRefused> {
    let at = usize_of(u32_at(row, 32));
    let end = usize_of(u32_at(row, 36))
        .checked_mul(8)
        .and_then(|l| at.checked_add(l))
        .filter(|&e| e <= tail.len())
        .ok_or(refused("a segment list past the record's tail"))?;
    Ok(Segments { list: &tail[at..end], tail })
}

fn vis_at(row: &[u8], tag_at: usize, key: usize) -> Result<Visibility, TableRefused> {
    let tag = VisibilityTag::from_byte(row[tag_at]).ok_or(refused("a byte that is no visibility"))?;
    Ok(Visibility::from_record(tag, key_at(row, key)))
}

impl<'b> LinksView<'b> {
    /// The record's head and sizes checked: a record shorter or longer than its counts say
    /// (torn) is refused. The rows' order is [`LinksView::check`]'s, once a fetch.
    pub fn open(bytes: &'b [u8]) -> Result<LinksView<'b>, TableRefused> {
        head_of(bytes, LINKS_MAGIC, LINKS_FORM, LINKS_HEAD)?;
        let n_rows = usize_of(u32_at(bytes, 40));
        let n_globs = usize_of(u32_at(bytes, 44));
        let tail_len = usize_of(u32_at(bytes, 48));
        let table = Table::from_byte(bytes[52]).ok_or(refused("a byte that is no table"))?;
        let rows_end = n_rows.checked_mul(LINK_ROW).and_then(|r| r.checked_add(LINKS_HEAD)).ok_or(refused("a row count past the record (torn)"))?;
        let globs_end = n_globs.checked_mul(GLOB_ROW).and_then(|g| g.checked_add(rows_end)).ok_or(refused("a glob count past the record (torn)"))?;
        if globs_end.checked_add(tail_len) != Some(bytes.len()) {
            return Err(refused("a record whose length is not its counts' (torn)"));
        }
        Ok(LinksView {
            module: ItemKey(key_at(bytes, 8)),
            table,
            n_rows,
            n_globs,
            rows: &bytes[LINKS_HEAD..rows_end],
            globs: &bytes[rows_end..globs_end],
            tail: &bytes[globs_end..],
            globs_digest: key_at(bytes, 64),
        })
    }

    /// The rows' order: what a fetch verifies once.
    pub fn check(&self) -> Result<(), TableRefused> {
        check_sorted(self.rows, LINK_ROW, self.n_rows)
    }

    pub fn module(&self) -> ItemKey {
        self.module
    }

    pub fn table(&self) -> Table {
        self.table
    }

    pub fn names(&self) -> usize {
        self.n_rows
    }

    pub fn globs(&self) -> usize {
        self.n_globs
    }

    /// What a part read of `cell` sees: the row's digest; else, with glob links, theirs (a
    /// reader that missed here went on through them); else `Absent`.
    pub fn part(&self, cell: [u8; CELL]) -> CellSeen {
        match find_cell(self.rows, LINK_ROW, self.n_rows, &cell) {
            Some(row) => CellSeen::Is(key_at(row, 112)),
            None if self.n_globs > 0 => CellSeen::Is(self.globs_digest),
            None => CellSeen::Absent,
        }
    }

    /// The row of `cell`, if the table has one.
    pub fn get(&self, cell: [u8; CELL]) -> Result<Option<LinkView<'b>>, TableRefused> {
        let Some(row) = find_cell(self.rows, LINK_ROW, self.n_rows, &cell) else { return Ok(None) };
        self.view_of(row).map(Some)
    }

    /// Row `i`, in cell order: what a reader that lists the table (a check of every row) reads.
    pub fn row(&self, i: usize) -> Result<LinkView<'b>, TableRefused> {
        if i >= self.n_rows {
            return Err(refused("a row past the record's rows"));
        }
        self.view_of(&self.rows[i * LINK_ROW..(i + 1) * LINK_ROW])
    }

    fn view_of(&self, row: &'b [u8]) -> Result<LinkView<'b>, TableRefused> {
        let ns = Namespace::from_byte(row[16]).ok_or(refused("a byte that is no namespace"))?;
        let origin = Origin::from_byte(row[21]).ok_or(refused("a byte that is no binding origin"))?;
        let vis = if row[20] == VIS_WRITTEN {
            if row[17] != TAG_DEF {
                return Err(refused("a written visibility on a row that is no definition"));
            }
            let tag = PathTag::from_byte(row[44]).ok_or(refused("a byte that is no path start"))?;
            let kind = PathKind::from_record(tag, row[45], key_at(row, 80));
            RowVis::Written { kind, segments: segments_of(row, self.tail)? }
        } else {
            RowVis::Is(vis_at(row, 20, 80)?)
        };
        Ok(LinkView {
            ns,
            name: str_at(self.tail, u32_at(row, 24), u32_at(row, 28))?,
            to: target_of(row, self.tail)?,
            vis,
            origin,
            expansion: u16::from_le_bytes([row[22], row[23]]),
            order: u32_at(row, 40),
            first: if row[20] == VIS_WRITTEN { 0 } else { u32_at(row, 44) },
            digest: key_at(row, 112),
        })
    }

    /// Glob link `i`, in the record's order.
    pub fn glob(&self, i: usize) -> Result<GlobView<'b>, TableRefused> {
        if i >= self.n_globs {
            return Err(refused("a glob link past the record's list"));
        }
        let row = &self.globs[i * GLOB_ROW..(i + 1) * GLOB_ROW];
        Ok(GlobView { to: target_of(row, self.tail)?, vis: vis_at(row, 20, 80)?, expansion: u16::from_le_bytes([row[22], row[23]]) })
    }
}

/// The `ModuleLinks` kind's part read (drive.md "Part reads"): pure over the record's bytes.
pub fn links_part(bytes: &[u8], cell: [u8; CELL]) -> Result<CellSeen, TableRefused> {
    Ok(LinksView::open(bytes)?.part(cell))
}

/// A module's `Imports` as its writer fills it: its own named `use` items resolved to final
/// definitions (column RESOLVED), its traits in scope (column TRAITS), the glob sources whose
/// traits it reads by a part read of their TRAITS, and the findings.
pub struct ImportsBuilder<'a> {
    module: ItemKey,
    rows: AVec<'a, ([u8; CELL], Namespace, Def, Visibility, u32)>,
    traits: AVec<'a, ItemKey>,
    sources: AVec<'a, ItemKey>,
    findings: AVec<'a, (u32, u32, Unresolved)>,
    unknown: u32,
}

impl<'a> ImportsBuilder<'a> {
    pub fn new_in(module: ItemKey, alloc: Alloc<'a>) -> ImportsBuilder<'a> {
        ImportsBuilder {
            module,
            rows: AVec::new_in(alloc),
            traits: AVec::new_in(alloc),
            sources: AVec::new_in(alloc),
            findings: AVec::new_in(alloc),
            unknown: 0,
        }
    }

    /// The `use` at `index` binds the cell `(ns, ..)` (key `cell`) to `def`, as visible as `vis`.
    pub fn resolved(&mut self, cell: [u8; CELL], ns: Namespace, def: Def, vis: Visibility, index: u32) {
        self.rows.push((cell, ns, def, vis, index));
    }

    /// A trait whose methods are in scope here (declared, imported, `as _`).
    pub fn trait_in_scope(&mut self, def: ItemKey) {
        self.traits.push(def);
    }

    /// A glob's source module or enum: its traits are in scope here too.
    pub fn glob_source(&mut self, source: ItemKey) {
        self.sources.push(source);
    }

    /// The `use` at `index` failed at `segment`.
    pub fn finding(&mut self, index: u32, segment: u32, why: Unresolved) {
        self.findings.push((index, segment, why));
    }

    /// An import the solve left outside what it follows: with one, the record is not the whole
    /// answer, and a reader of a missing cell may not take it as absent.
    pub fn undecided(&mut self) {
        self.unknown = self.unknown.saturating_add(1);
    }

    pub fn write<'o>(&mut self, alloc: Alloc<'o>) -> Result<RecordBytes<'o>, BuildError> {
        self.rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for pair in self.rows.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(BuildError::Twice { table: "Imports rows", key: cell_id(pair[1].0) });
            }
        }
        if self.rows.iter().any(|r| r.0 == TRAITS_CELL) {
            return Err(BuildError::Twice { table: "Imports rows (the TRAITS cell)", key: cell_id(TRAITS_CELL) });
        }
        self.traits.sort_unstable();
        self.traits.dedup();
        self.sources.sort_unstable();
        self.sources.dedup();
        self.findings.sort_unstable();
        self.findings.dedup();
        let mut scratch: AVec<'o, u8> = AVec::new_in(alloc);
        for t in self.traits.iter() {
            scratch.extend_from_slice(&t.0.0);
        }
        scratch.extend_from_slice(&len32(self.sources.len(), "Imports sources")?.to_le_bytes());
        for s in self.sources.iter() {
            scratch.extend_from_slice(&s.0.0);
        }
        let traits_digest = ContentId::of(TRAITS_DOMAIN, &scratch);
        let mut out = RecordBytes::new_in(alloc);
        out.extend_from_slice(&IMPORTS_MAGIC);
        out.extend_from_slice(&IMPORTS_FORM.to_le_bytes());
        out.extend_from_slice(&self.module.0.0);
        out.extend_from_slice(&len32(self.rows.len(), "Imports rows")?.to_le_bytes());
        out.extend_from_slice(&len32(self.traits.len(), "Imports traits")?.to_le_bytes());
        out.extend_from_slice(&len32(self.sources.len(), "Imports sources")?.to_le_bytes());
        out.extend_from_slice(&len32(self.findings.len(), "Imports findings")?.to_le_bytes());
        out.extend_from_slice(&self.unknown.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&traits_digest.0);
        for &(cell, ns, def, vis, index) in self.rows.iter() {
            let mut row = [0u8; IMPORT_ROW];
            row[..CELL].copy_from_slice(&cell);
            row[16] = ns.byte();
            let (kind, shapes) = def.kind.to_record();
            row[17] = kind.byte();
            row[18] = shapes.byte();
            let (tag, key) = vis.to_record();
            row[19] = tag.byte();
            row[20..24].copy_from_slice(&index.to_le_bytes());
            row[32..64].copy_from_slice(&def.key.0.0);
            row[64..96].copy_from_slice(&key.0);
            // The digest covers what a reader takes: not the `use`'s index, so a `use` moved
            // within the module moves nobody.
            scratch.clear();
            scratch.extend_from_slice(&row[16..20]);
            scratch.extend_from_slice(&row[32..96]);
            row[96..].copy_from_slice(&ContentId::of(IMPORT_DOMAIN, &scratch).0);
            out.extend_from_slice(&row);
        }
        for t in self.traits.iter() {
            out.extend_from_slice(&t.0.0);
        }
        for s in self.sources.iter() {
            out.extend_from_slice(&s.0.0);
        }
        for &(index, segment, why) in self.findings.iter() {
            let (tag, kind) = why.to_record();
            let mut f = [0u8; FINDING];
            f[0..4].copy_from_slice(&index.to_le_bytes());
            f[4..8].copy_from_slice(&segment.to_le_bytes());
            f[8] = tag.byte();
            f[9] = kind.byte();
            out.extend_from_slice(&f);
        }
        Ok(out)
    }
}

/// An `Imports` record read in place.
#[derive(Clone, Copy, Debug)]
pub struct ImportsView<'b> {
    module: ItemKey,
    n_rows: usize,
    rows: &'b [u8],
    traits: &'b [u8],
    sources: &'b [u8],
    findings: &'b [u8],
    unknown: u32,
    traits_digest: ContentId,
}

/// One RESOLVED row of an `Imports` record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ImportView {
    pub ns: Namespace,
    pub def: Def,
    pub vis: Visibility,
    /// The `use`'s index among the module's.
    pub index: u32,
    pub digest: ContentId,
}

impl<'b> ImportsView<'b> {
    pub fn open(bytes: &'b [u8]) -> Result<ImportsView<'b>, TableRefused> {
        head_of(bytes, IMPORTS_MAGIC, IMPORTS_FORM, IMPORTS_HEAD)?;
        let n = [40usize, 44, 48, 52].map(|at| usize_of(u32_at(bytes, at)));
        let widths = [IMPORT_ROW, KEY, KEY, FINDING];
        let mut ends = [0usize; 4];
        let mut at = IMPORTS_HEAD;
        for i in 0..4 {
            at = n[i].checked_mul(widths[i]).and_then(|l| at.checked_add(l)).ok_or(refused("a count past the record (torn)"))?;
            ends[i] = at;
        }
        if at != bytes.len() {
            return Err(refused("a record whose length is not its counts' (torn)"));
        }
        Ok(ImportsView {
            module: ItemKey(key_at(bytes, 8)),
            n_rows: n[0],
            rows: &bytes[IMPORTS_HEAD..ends[0]],
            traits: &bytes[ends[0]..ends[1]],
            sources: &bytes[ends[1]..ends[2]],
            findings: &bytes[ends[2]..ends[3]],
            unknown: u32_at(bytes, 56),
            traits_digest: key_at(bytes, 64),
        })
    }

    pub fn check(&self) -> Result<(), TableRefused> {
        check_sorted(self.rows, IMPORT_ROW, self.n_rows)
    }

    pub fn module(&self) -> ItemKey {
        self.module
    }

    /// Imports the solve left undecided: with one, a missing cell is not "not imported".
    pub fn undecided(&self) -> u32 {
        self.unknown
    }

    /// What a part read of `cell` sees; [`TRAITS_CELL`] is the TRAITS column's digest.
    pub fn part(&self, cell: [u8; CELL]) -> CellSeen {
        if cell == TRAITS_CELL {
            return CellSeen::Is(self.traits_digest);
        }
        match find_cell(self.rows, IMPORT_ROW, self.n_rows, &cell) {
            Some(row) => CellSeen::Is(key_at(row, 96)),
            None => CellSeen::Absent,
        }
    }

    pub fn get(&self, cell: [u8; CELL]) -> Result<Option<ImportView>, TableRefused> {
        let Some(row) = find_cell(self.rows, IMPORT_ROW, self.n_rows, &cell) else { return Ok(None) };
        let ns = Namespace::from_byte(row[16]).ok_or(refused("a byte that is no namespace"))?;
        let kind = DefKindTag::from_byte(row[17]).ok_or(refused("a byte that is no definition kind"))?;
        let shapes = MacroShapes::from_byte(row[18]).ok_or(refused("a byte that is no set of macro call shapes"))?;
        Ok(Some(ImportView {
            ns,
            def: Def { key: ItemKey(key_at(row, 32)), kind: DefKind::from_record(kind, shapes) },
            vis: vis_at(row, 19, 64)?,
            index: u32_at(row, 20),
            digest: key_at(row, 96),
        }))
    }

    /// The traits in scope, sorted.
    pub fn traits(&self) -> impl Iterator<Item = ItemKey> + 'b {
        self.traits.chunks_exact(KEY).map(|k| ItemKey(key_at(k, 0)))
    }

    /// The glob sources whose TRAITS are read too, sorted.
    pub fn sources(&self) -> impl Iterator<Item = ItemKey> + 'b {
        self.sources.chunks_exact(KEY).map(|k| ItemKey(key_at(k, 0)))
    }

    /// The findings: each failed `use`'s index, the segment it failed at, and why.
    pub fn findings(&self) -> impl Iterator<Item = Result<(u32, u32, Unresolved), TableRefused>> + 'b {
        self.findings.chunks_exact(FINDING).map(|f| {
            let why = Unresolved::from_record_bytes(f[8], f[9]).ok_or(refused("a byte that is no unresolved reason"))?;
            Ok((u32_at(f, 0), u32_at(f, 4), why))
        })
    }
}

/// The `Imports` kind's part read (drive.md "Part reads"): pure over the record's bytes.
pub fn imports_part(bytes: &[u8], cell: [u8; CELL]) -> Result<CellSeen, TableRefused> {
    Ok(ImportsView::open(bytes)?.part(cell))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(n: u8) -> [u8; CELL] {
        let mut c = [0u8; CELL];
        c[0] = n;
        c
    }

    fn module(n: u8) -> ItemKey {
        ItemKey(ContentId([n; KEY]))
    }

    /// Expected line: `test module_links::tests::a_cell_is_found_absent_or_refused_when_torn ... ok`.
    /// A part read finds a row's digest, answers `Absent` for a cell not there (and the glob
    /// links' digest once the table has a glob), and refuses a torn payload by name.
    #[test]
    fn a_cell_is_found_absent_or_refused_when_torn() {
        let alloc = Alloc::global();
        let mut b = LinksBuilder::new_in(module(1), Table::Outline, alloc);
        let def = Def { key: module(9), kind: DefKind::Other };
        b.def(cell(3), Namespace::Types, "Three", def, Visibility::Public, Origin::Declared, 0);
        b.path(cell(1), Namespace::Values, "one", PathKind::SelfMod, ["inner", "one"], Visibility::Public, Origin::Import, 0);
        let bytes = b.write(alloc).expect("the record");
        let view = LinksView::open(&bytes).expect("opens");
        view.check().expect("sorted");
        let row = view.get(cell(3)).expect("reads").expect("found");
        assert_eq!(links_part(&bytes, cell(3)), Ok(CellSeen::Is(row.digest)));
        assert!(matches!(row.to, TargetView::Def(d) if d == def));
        let one = view.get(cell(1)).expect("reads").expect("found");
        let TargetView::Path { kind, segments } = one.to else { panic!("a path link") };
        assert_eq!((kind, segments.len(), segments.get(1)), (PathKind::SelfMod, 2, Ok("one")));
        assert_eq!(links_part(&bytes, cell(2)), Ok(CellSeen::Absent));
        // Torn: one byte short, one byte long, a cut head.
        assert!(links_part(&bytes[..bytes.len() - 1], cell(3)).is_err());
        let mut long = bytes.clone();
        long.push(0);
        assert!(links_part(&long, cell(3)).is_err());
        assert_eq!(links_part(&bytes[..10], cell(3)), Err(refused("a record shorter than its head (torn)")));
        // With a glob link, a missing cell is the glob links' digest.
        b.glob_path(PathKind::Super(1), [], Visibility::Public, 0);
        let with_glob = b.write(alloc).expect("the record");
        let CellSeen::Is(globs) = links_part(&with_glob, cell(2)).expect("reads") else { panic!("the globs' digest") };
        assert_ne!(globs, row.digest);
        assert_eq!(links_part(&with_glob, cell(3)), Ok(CellSeen::Is(row.digest)), "a found cell is unmoved by a glob");
        // Two rows of one cell are refused.
        b.def(cell(3), Namespace::Types, "Three", def, Visibility::Public, Origin::Declared, 0);
        assert!(matches!(b.write(alloc), Err(BuildError::Twice { .. })));
    }

    /// Expected line: `test module_links::tests::an_imports_cell_and_its_traits_column ... ok`.
    #[test]
    fn an_imports_cell_and_its_traits_column() {
        let alloc = Alloc::global();
        let mut b = ImportsBuilder::new_in(module(1), alloc);
        b.resolved(cell(5), Namespace::Types, Def { key: module(7), kind: DefKind::Trait }, Visibility::Public, 0);
        b.trait_in_scope(module(7));
        b.glob_source(module(8));
        b.finding(1, 0, Unresolved::NotFound);
        let bytes = b.write(alloc).expect("the record");
        let view = ImportsView::open(&bytes).expect("opens");
        let row = view.get(cell(5)).expect("reads").expect("found");
        assert_eq!(imports_part(&bytes, cell(5)), Ok(CellSeen::Is(row.digest)));
        assert_eq!(imports_part(&bytes, cell(6)), Ok(CellSeen::Absent));
        assert!(matches!(imports_part(&bytes, TRAITS_CELL), Ok(CellSeen::Is(_))));
        assert_eq!(view.traits().count(), 1);
        assert_eq!(view.findings().next(), Some(Ok((1, 0, Unresolved::NotFound))));
        assert!(imports_part(&bytes[..bytes.len() - 3], cell(5)).is_err());
    }
}
