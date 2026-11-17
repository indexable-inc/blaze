//! A crate's names read BY CELL (drive.md, "Names at module grain" point 3, READS ARE PER CELL;
//! lane CELL, 2026-11-17). Until the module-grain kinds replace it (point 7), a body or head
//! reads its crate's `CrateScope` record, and read WHOLE it is dirtied by every change of the
//! crate's names: one private fn added made every `Signature` and `BodyCheck` of the crate again
//! (blaze-cli signature_grain_tests.rs `a_private_item_added_makes_itself_alone`: (7, 7) where
//! the floor is (1, 1)). Read by cell, a reader is dirtied only when a cell its walks consulted
//! holds another digest today.
//!
//! THE CELLS ([`NameCell`]), each a 16-byte key ([`NameCell::key`]) in its own domain:
//! - `Binding(module, ns, name)`: the module's own binding of the name ([`cell_key`]), or
//!   absent: what [`ScopeRead::get`] answered;
//! - `Module(module)`: everything else the walk reads of the module: its chain and flags
//!   (`#[no_implicit_prelude]`, a block's module), its glob edges, its first refused call, its
//!   textual macro table and marks, and the traits it binds (named and `as _`); absent for a
//!   module the record does not hold;
//! - `Root(crate)`: the crate root record (the extern prelude, the `#[macro_use]` prelude, the
//!   `#[macro_export]` macros, the head) but the root file's content, which moves with every
//!   edit of that file and which [`Logged`] does not hand out ([`walked_head`]);
//! - `Variants(owner)`: an enum's variants, absent for no enum.
//!
//! A private fn added moves exactly one cell, its own `Binding`, which a walk consulted only if
//! it looked that name up in that module.
//!
//! WHO WRITES AND WHO READS. [`table`] writes a crate's cell table from its names record:
//! sorted (key, digest) rows, [`TABLE_ROW`] bytes each, every cell a walk can name that holds
//! something. [`seen_in`] is the lookup a reader's part is decided by: one binary search of the
//! table, `None` for a cell absent. [`Logged`] is the walk's world wrapped at the one seam
//! (rule 78, names/seam.rs): every `scope`, `get`, `chain`, flag, root and variant read logs its
//! cell, so what a walk consulted is complete by construction (rule 21): a read the seam does
//! not see cannot reach the walk. The reader never logs a digest: its digest is decided from
//! the same table it would be checked against, so the two cannot disagree.

use core::cell::RefCell;

use blaze_hash::HashMap;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ids::{Domain, Hasher};
use blaze_own::{AVec, Alloc};
use blaze_surface::{Binding, CrateRootHead, CrateRootView, DefKind, Globs, ItemKind, MacroShapes, Namespace, RefusedWhy, ScopeView};

use crate::cell_key::{CellKey, cell_key};
use crate::facts::Name;
use crate::items_record::{ItemsRecord, ItemsRecordError, TextOf, TextSpan};
use crate::names::{RootRead, ScopeRead, VariantHit, World};
use crate::names_record::{NamesRecord, NamesRecordError, Verified};
use crate::resolve::{CrateNames, aligned};

/// The domain of a cell's key: the schema's name, so a change of the recipe is a new domain
/// and old reads miss.
pub const KEY_DOMAIN: &str = "blaze v2 crate names cell key 1";
/// The domain of a cell's digest.
pub const DIGEST_DOMAIN: &str = "blaze v2 crate names cell digest 1";

static KEYS: Domain = Domain::new(KEY_DOMAIN);
static DIGESTS: Domain = Domain::new(DIGEST_DOMAIN);

/// One row of a cell table: the cell's key, then its digest.
pub const TABLE_ROW: usize = 16 + 32;

/// One cell of a crate's names: see the module doc.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum NameCell {
    Binding { module: ItemKey, name: CellKey },
    Module { module: ItemKey },
    /// The crate root of the crate whose keys begin with these 8 bytes.
    Root { krate: [u8; 8] },
    Variants { owner: ItemKey },
    /// THE ITEMS RECORD's rows of one key: its item row and its module row, each or both
    /// absent ([`ItemsRecord::item`], [`ItemsRecord::module`]).
    Item { key: ItemKey },
    /// An owner's member rows, in order (`RecordItems::members`); absent for no owner.
    Members { owner: ItemKey },
    /// A macro call's expansion row (its text's content id and length); absent for none.
    Expansion { call: ItemKey },
    /// A section of the crate's record read WHOLE, by a question no cell answers (every impl,
    /// every item, the lang rows, the files): its digest is the section's bytes'.
    Whole { krate: [u8; 8], section: Section },
}

/// A section of a crate's `CrateScope` record a [`NameCell::Whole`] stands for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Section {
    Names = 1,
    Items = 2,
    Expansions = 3,
}

impl NameCell {
    /// The cell's key: the first 16 bytes of BLAKE3 in [`KEY_DOMAIN`] over a tag, the key it is
    /// about, and for a binding its name's [`cell_key`].
    pub fn key(self) -> [u8; 16] {
        let mut h = Hasher::of(&KEYS);
        match self {
            NameCell::Binding { module, name } => h.update(&[1]).id(&module.0).update(&name.0),
            NameCell::Module { module } => h.update(&[2]).id(&module.0),
            NameCell::Root { krate } => h.update(&[3]).update(&krate),
            NameCell::Variants { owner } => h.update(&[4]).id(&owner.0),
            NameCell::Item { key } => h.update(&[5]).id(&key.0),
            NameCell::Members { owner } => h.update(&[6]).id(&owner.0),
            NameCell::Expansion { call } => h.update(&[7]).id(&call.0),
            NameCell::Whole { krate, section } => h.update(&[8, section as u8]).update(&krate),
        };
        let mut key = [0; 16];
        key.copy_from_slice(&h.finish().0[..16]);
        key
    }

    /// The first 8 bytes of every key of the crate the cell is of: whose `CrateScope` holds it.
    pub fn crate_prefix(self) -> [u8; 8] {
        match self {
            NameCell::Binding { module, .. } | NameCell::Module { module } => module.crate_prefix(),
            NameCell::Root { krate } | NameCell::Whole { krate, .. } => krate,
            NameCell::Variants { owner } | NameCell::Members { owner } => owner.crate_prefix(),
            NameCell::Item { key } => key.crate_prefix(),
            NameCell::Expansion { call } => call.crate_prefix(),
        }
    }
}

/// How an items cell's digest reads a row's text: the crate's file or expansion, as the record
/// proves it ([`ItemsRecord::slice`]).
pub type ReadText<'t, 'a> = &'t dyn Fn(TextOf<'_>) -> Option<&'a str>;

/// A span's TEXT into `h`, never its place (rule 71): an item moved in its file, its text the
/// same, holds the same cell.
fn span_text<'a>(h: &mut Hasher, items: &ItemsRecord<'a>, span: Option<TextSpan>, read: ReadText<'_, 'a>) -> Result<(), ItemsRecordError> {
    match span {
        None => {
            h.update(&[0]);
        }
        Some(span) => {
            let text = items.slice(span, read)?;
            h.update(&[1]).std(&span.at).update(&(text.len() as u64).to_le_bytes()).update(text.as_bytes());
        }
    }
    Ok(())
}

/// What `Item(key)` holds: the item row of `key` (its meaning and its text; its place in the
/// file and its outline index left out) and its module row, `None` when both are absent. A
/// row that does not read is the record's error.
fn item_digest<'a>(items: &ItemsRecord<'a>, key: ItemKey, read: ReadText<'_, 'a>) -> Result<Option<ContentId>, ItemsRecordError> {
    let (item, module) = (items.item(key)?, items.module(key)?);
    if item.is_none() && module.is_none() {
        return Ok(None);
    }
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[5]);
    match item {
        None => {
            h.update(&[0]);
        }
        Some(row) => {
            h.update(&[1]).id(&row.key.0).std(&row.kind).id(&row.module.0).std(&row.name);
            h.update(&[u8::from(row.body_holds_items), u8::from(row.ctor), u8::from(row.impl_row.is_some())]);
            if row.kind == ItemKind::Module {
                // A module's row holds its HEAD only (attributes, visibility, name): what it
                // holds is its own cells', so an item added inside an inline module moves that
                // module's cells and never the cell of its row in the parent.
                match row.text {
                    None => {
                        h.update(&[0]);
                    }
                    Some(span) => {
                        let head = module_head(items.slice(span, read)?);
                        h.update(&[2]).std(&span.at).update(&(head.len() as u64).to_le_bytes()).update(head.as_bytes());
                    }
                }
            } else {
                span_text(&mut h, items, row.text, read)?;
            }
            span_text(&mut h, items, row.impl_header, read)?;
        }
    }
    h.std(&module);
    Ok(Some(h.finish()))
}

/// The head of a `mod` item's text: up to its body's `{` or its `;`, past its attributes and
/// comments (whose brackets, strings and braces are skipped as the lexer would).
pub fn module_head(text: &str) -> &str {
    let b = text.as_bytes();
    let mut i = 0;
    // Depth of `[` inside an attribute: a `{` or `;` there is the attribute's, not the head's.
    let mut depth = 0u32;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut nest = 1u32;
                i += 2;
                while i < b.len() && nest > 0 {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        nest += 1;
                        i += 1;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        nest -= 1;
                        i += 1;
                    }
                    i += 1;
                }
                continue;
            }
            // A raw string, `r"..."` or `r#"..."#`: ends at a quote and as many `#`.
            b'r' if matches!(b.get(i + 1), Some(b'"' | b'#')) && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) => {
                let mut j = i + 1;
                let mut hashes = 0;
                while b.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if b.get(j) == Some(&b'"') {
                    j += 1;
                    while j < b.len() && !(b[j] == b'"' && b.len() - j > hashes && b[j + 1..=j + hashes].iter().all(|c| *c == b'#')) {
                        j += 1;
                    }
                    i = j + hashes;
                }
            }
            // A char literal (`'{'`, `'\''`); a lifetime has no closing quote and is skipped as
            // its letters are.
            b'\'' if b.get(i + 2) == Some(&b'\'') && b.get(i + 1) != Some(&b'\\') => i += 2,
            b'\'' if b.get(i + 1) == Some(&b'\\') => {
                i += 2;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'[' => depth += 1,
            b']' => depth = depth.saturating_sub(1),
            b'{' | b';' if depth == 0 => return &text[..i],
            _ => {}
        }
        i += 1;
    }
    text
}

/// What `Members(owner)` holds, `None` for no owner: each member's meaning and text, in order.
fn members_digest<'a>(items: &ItemsRecord<'a>, owner: ItemKey, read: ReadText<'_, 'a>) -> Result<Option<ContentId>, ItemsRecordError> {
    let Some(members) = items.members(owner)? else { return Ok(None) };
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[6]).update(&(members.len() as u64).to_le_bytes());
    for member in members.iter() {
        let member = member?;
        h.id(&member.key.0).std(&member.name).std(&member.kind).update(&[u8::from(member.body_holds_items)]).std(&member.written_by);
        span_text(&mut h, items, member.text, read)?;
    }
    Ok(Some(h.finish()))
}

/// What `Expansion(call)` holds, `None` for no expansion row.
fn expansion_digest(items: &ItemsRecord<'_>, call: ItemKey) -> Option<ContentId> {
    items.expansion(call).map(|row| Hasher::of(&DIGESTS).update(&[7]).std(&row).finish())
}

/// What `Whole(section)` holds: the digest of the section's bytes as the record holds them.
pub fn whole_digest(section: Section, bytes: &[u8]) -> ContentId {
    Hasher::of(&DIGESTS).update(&[8, section as u8]).part(bytes).finish()
}

fn binding_digest(binding: Binding) -> ContentId {
    Hasher::of(&DIGESTS).update(&[1]).std(&binding).finish()
}

/// The head a walk through [`Logged`] is handed: the root file's content left out. It moves with
/// every edit of the root file, and nothing a walk answers depends on it, so the `Root` cell
/// does not hold it and the seam does not hand it out (what a reader cannot see, it cannot
/// read stale).
pub fn walked_head(head: CrateRootHead) -> CrateRootHead {
    CrateRootHead { root_file: ContentId([0; 32]), ..head }
}

fn root_digest(record: &NamesRecord<'_>) -> ContentId {
    let root = record.crate_root();
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[3]).std(&walked_head(CrateRootView::head(&root)));
    h.update(&(root.externs().len() as u64).to_le_bytes());
    for extern_crate in root.externs() {
        h.std(&extern_crate);
    }
    h.update(&(root.deps().len() as u64).to_le_bytes());
    for dep in root.deps() {
        h.std(&dep);
    }
    // Inherent, named: [`RootRead::macro_use`] takes an index and would be picked first.
    h.update(&(CrateRootView::macro_use(&root).len() as u64).to_le_bytes());
    for key in CrateRootView::macro_use(&root) {
        h.id(&key.0);
    }
    h.update(&(root.macro_exports().len() as u64).to_le_bytes());
    for export in root.macro_exports() {
        h.std(&export);
    }
    names(&mut h, root.tools());
    names(&mut h, root.builtin_attrs());
    names(&mut h, root.features());
    h.finish()
}

fn names<'x>(h: &mut Hasher, list: impl ExactSizeIterator<Item = &'x str>) {
    h.update(&(list.len() as u64).to_le_bytes());
    for name in list {
        h.std(name);
    }
}

fn variants_digest(record: &NamesRecord<'_>, owner: ItemKey) -> Option<ContentId> {
    let variants = record.variants(owner)?;
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[4]);
    for variant in variants.iter() {
        h.std(&variant);
    }
    Some(h.finish())
}

/// What `Module(module)` holds, `None` for a module the record has no row of.
fn module_digest(record: &NamesRecord<'_>, module: ItemKey) -> Option<ContentId> {
    let chain = record.chain(module);
    let scope = record.scope(module);
    if chain.is_none() && scope.is_none() {
        return None;
    }
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[2]);
    match chain {
        Some(chain) => {
            h.update(&(chain.len() as u64).to_le_bytes());
            for key in chain {
                h.id(&key.0);
            }
        }
        None => {
            h.update(&[0xff]);
        }
    }
    h.update(&[u8::from(record.no_implicit_prelude(module)), u8::from(record.anonymous(module))]);
    match scope {
        None => {
            h.update(&[0]);
        }
        Some(view) => {
            h.update(&[1]);
            // The inherent reads, named: [`ScopeRead`]'s by-value methods of the same names
            // would be picked first.
            for edge in ScopeView::globs(&view) {
                h.std(&edge);
            }
            h.update(&[0xfe]).std(&ScopeView::refused(&view));
            for def in ScopeView::textual(&view) {
                h.std(&def);
            }
            h.update(&[0xfd]);
            for mark in view.marks() {
                h.std(&mark);
            }
            h.update(&[0xfc]);
            for anon in view.traits() {
                h.std(&anon);
            }
            h.update(&[0xfb]);
            for entry in view.bindings() {
                if entry.ns == Namespace::Types && entry.binding.def.kind == DefKind::Trait {
                    h.id(&entry.binding.def.key.0);
                }
            }
        }
    }
    Some(h.finish())
}

/// Why a cell table was not written or a lookup refused.
#[derive(Debug)]
pub enum CellsError {
    /// Two cells of one crate under one key (rule 21): a collision of 128 bits, refused.
    TwoUnderOneKey { key: [u8; 16] },
    /// A table's bytes are not whole rows in increasing key order.
    NotATable { at: usize },
    /// A row of the items record did not read.
    Items(ItemsRecordError),
    /// An items cell asked of a crate whose items record was not given.
    NoItemsRecord,
    /// The names record just written does not check.
    Names(NamesRecordError),
}

/// The cell table of a crate just solved, from the two records written of it: what the crate's
/// `CrateScope` record files beside them (blaze-cli names.rs). Item texts are read from the
/// solve's own files and expansions; each of `wholes` is a [`NameCell::Whole`].
pub fn crate_table<'a>(names: &CrateNames<'a>, names_record: &[u8], items_record: &[u8], wholes: &[(Section, &[u8])], alloc: Alloc<'a>) -> Result<AVec<'a, u8>, CellsError> {
    let record = NamesRecord::open(Verified::check(aligned(names_record, alloc)).map_err(CellsError::Names)?);
    let items = ItemsRecord::open(aligned(items_record, alloc), alloc).map_err(CellsError::Items)?;
    let read = |of: TextOf<'_>| -> Option<&'a str> {
        match of {
            TextOf::File { index, .. } => names.files.get(index as usize).map(|file| file.text),
            TextOf::Expansion { call } => names.expansions.get(&call).and_then(|expansion| expansion.made).map(|made| made.text),
        }
    };
    table(&record, Some((&items, &read)), wholes, alloc)
}

/// The cell table of `record`'s crate: every cell with something in it, sorted by key, as
/// [`TABLE_ROW`]-byte rows. With `items`, its rows' cells too (every item and module key,
/// every owner's members, every expansion); each of `wholes` is a [`NameCell::Whole`] over
/// those bytes.
pub fn table<'a, 'b>(record: &NamesRecord<'_>, items: Option<(&ItemsRecord<'a>, ReadText<'_, 'a>)>, wholes: &[(Section, &[u8])], alloc: Alloc<'b>) -> Result<AVec<'b, u8>, CellsError> {
    let mut rows: AVec<'b, ([u8; 16], ContentId)> = AVec::new_in(alloc);
    let krate = record.key().prefix();
    for (section, bytes) in wholes {
        rows.push((NameCell::Whole { krate, section: *section }.key(), whole_digest(*section, bytes)));
    }
    if let Some((items, read)) = items {
        let mut keys: AVec<'_, ItemKey> = AVec::with_capacity_in(items.items() + items.modules(), alloc);
        for i in 0..items.items() {
            if let Some(row) = items.item_at(i).map_err(CellsError::Items)? {
                keys.push(row.key);
                if let Some(digest) = members_digest(items, row.key, read).map_err(CellsError::Items)? {
                    rows.push((NameCell::Members { owner: row.key }.key(), digest));
                }
            }
        }
        for i in 0..items.modules() {
            if let Some(row) = items.module_at(i).map_err(CellsError::Items)? {
                keys.push(row.key);
            }
        }
        keys.sort_unstable();
        keys.dedup();
        for key in keys.iter() {
            if let Some(digest) = item_digest(items, *key, read).map_err(CellsError::Items)? {
                rows.push((NameCell::Item { key: *key }.key(), digest));
            }
        }
        for i in 0..items.expansions() {
            if let Some(row) = items.expansion_at(i) {
                if let Some(digest) = expansion_digest(items, row.call) {
                    rows.push((NameCell::Expansion { call: row.call }.key(), digest));
                }
            }
        }
    }
    for module in record.modules() {
        if let Some(digest) = module_digest(record, module) {
            rows.push((NameCell::Module { module }.key(), digest));
        }
        if let Some(view) = record.scope(module) {
            for entry in view.bindings() {
                let cell = NameCell::Binding { module, name: cell_key(entry.ns, entry.name) };
                rows.push((cell.key(), binding_digest(entry.binding)));
            }
        }
    }
    for owner in record.enums() {
        if let Some(digest) = variants_digest(record, owner) {
            rows.push((NameCell::Variants { owner }.key(), digest));
        }
    }
    rows.push((NameCell::Root { krate: record.key().prefix() }.key(), root_digest(record)));
    rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut bytes = AVec::with_capacity_in(rows.len() * TABLE_ROW, alloc);
    for (i, (key, digest)) in rows.iter().enumerate() {
        if i > 0 && rows[i - 1].0 == *key {
            return Err(CellsError::TwoUnderOneKey { key: *key });
        }
        bytes.extend_from_slice(key);
        bytes.extend_from_slice(&digest.0);
    }
    Ok(bytes)
}

/// What the table holds at `key`: its digest, or `None` for a cell absent. One binary search.
pub fn seen_in(table: &[u8], key: [u8; 16]) -> Result<Option<ContentId>, CellsError> {
    if table.len() % TABLE_ROW != 0 {
        return Err(CellsError::NotATable { at: table.len() });
    }
    let (mut lo, mut hi) = (0usize, table.len() / TABLE_ROW);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let row = &table[mid * TABLE_ROW..(mid + 1) * TABLE_ROW];
        match row[..16].cmp(&key[..]) {
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
            core::cmp::Ordering::Equal => {
                let mut digest = [0; 32];
                digest.copy_from_slice(&row[16..]);
                return Ok(Some(ContentId(digest)));
            }
        }
    }
    Ok(None)
}

/// Where a walk through [`Logged`] puts each cell it consults: the names half of a task's
/// recorder (engine.md E2: one per-task recorder of every read, names, heads and impls, over
/// read-only tables). [`Log`] is the plain one. A recorder over shared tables is per task and
/// the tables hold no per-task cache, so what it is handed is a function of the task's questions
/// alone, never of which task asked first (reads.rs's "what is not listed").
pub trait CellSink {
    fn put(&self, cell: NameCell);
}

impl CellSink for Log<'_> {
    fn put(&self, cell: NameCell) {
        Log::put(self, cell)
    }
}

/// The cells a walk consulted, each once: by key, with the cell it is.
pub struct Log<'a> {
    cells: RefCell<HashMap<[u8; 16], NameCell, Alloc<'a>>>,
}

impl<'a> Log<'a> {
    pub fn new(alloc: Alloc<'a>) -> Log<'a> {
        Log { cells: RefCell::new(HashMap::new_in(alloc)) }
    }

    /// Logs `cell`: what a reader outside the walk (an items record question) consulted.
    pub fn put(&self, cell: NameCell) {
        self.cells.borrow_mut().insert(cell.key(), cell);
    }

    /// Every cell logged so far, by key order, into `alloc`.
    pub fn cells<'b>(&self, alloc: Alloc<'b>) -> AVec<'b, ([u8; 16], NameCell)> {
        let mut all: AVec<'b, ([u8; 16], NameCell)> = AVec::new_in(alloc);
        for (key, cell) in self.cells.borrow().iter() {
            all.push((*key, *cell));
        }
        all.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        all
    }

    pub fn len(&self) -> usize {
        self.cells.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A walk's world whose every read is put in `log`: the seam a reader by cell walks through.
pub struct Logged<'l, W> {
    pub world: &'l W,
    pub log: &'l dyn CellSink,
}

/// A module's scope as [`Logged`] hands it out: each `get` logs its binding's cell (the
/// scope's other reads are its `Module` cell, logged when it was handed out).
#[derive(Clone, Copy)]
pub struct LoggedScope<'w, S> {
    scope: S,
    module: ItemKey,
    log: &'w dyn CellSink,
}

impl<'w, N: Name, S: ScopeRead<'w, N>> ScopeRead<'w, N> for LoggedScope<'w, S> {
    fn get(self, ns: Namespace, name: N) -> Option<Binding> {
        self.log.put(NameCell::Binding { module: self.module, name: cell_key(ns, name.as_ref()) });
        self.scope.get(ns, name)
    }

    fn globs(self) -> Globs<'w> {
        self.scope.globs()
    }

    fn refused(self) -> Option<(ItemKey, RefusedWhy)> {
        self.scope.refused()
    }

    fn textual(self, child: Option<ItemKey>, name: N) -> Option<(ItemKey, MacroShapes)> {
        self.scope.textual(child, name)
    }

    fn each_trait(self, each: &mut dyn FnMut(ItemKey)) {
        self.scope.each_trait(each)
    }
}

/// A crate root as [`Logged`] hands it out: its `Root` cell was logged when it was, and its
/// head is [`walked_head`].
#[derive(Clone, Copy)]
pub struct LoggedRoot<R>(R);

impl<N, R: RootRead<N>> RootRead<N> for LoggedRoot<R> {
    fn head(self) -> CrateRootHead {
        walked_head(self.0.head())
    }

    fn extern_crate(self, name: N) -> Option<ItemKey> {
        self.0.extern_crate(name)
    }

    fn macro_use(self, i: usize) -> Option<ItemKey> {
        self.0.macro_use(i)
    }

    fn macro_export(self, name: N) -> Option<(ItemKey, MacroShapes)> {
        self.0.macro_export(name)
    }
}

impl<'l, N: Name, W: World<N>> World<N> for Logged<'l, W> {
    type Scope<'w>
        = LoggedScope<'w, W::Scope<'w>>
    where
        Self: 'w;
    type Root<'w>
        = LoggedRoot<W::Root<'w>>
    where
        Self: 'w;

    fn scope(&self, module: ItemKey) -> Option<Self::Scope<'_>> {
        self.log.put(NameCell::Module { module });
        let scope = self.world.scope(module)?;
        Some(LoggedScope { scope, module, log: self.log })
    }

    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        self.log.put(NameCell::Module { module });
        self.world.chain(module)
    }

    fn root_of(&self, key: ItemKey) -> Option<Self::Root<'_>> {
        self.log.put(NameCell::Root { krate: key.crate_prefix() });
        self.world.root_of(key).map(LoggedRoot)
    }

    fn root(&self, krate: CrateItemKey) -> Option<Self::Root<'_>> {
        self.log.put(NameCell::Root { krate: krate.prefix() });
        self.world.root(krate).map(LoggedRoot)
    }

    fn variant(&self, owner: ItemKey, name: N) -> Option<Option<VariantHit>> {
        self.log.put(NameCell::Variants { owner });
        self.world.variant(owner, name)
    }

    fn no_implicit_prelude(&self, module: ItemKey) -> bool {
        self.log.put(NameCell::Module { module });
        self.world.no_implicit_prelude(module)
    }

    fn anonymous(&self, module: ItemKey) -> bool {
        self.log.put(NameCell::Module { module });
        self.world.anonymous(module)
    }
}

/// The digest `key` holds in a crate's names, read from the record itself rather than its
/// table: the second home a test holds the table to ([`table`] over the same record).
pub fn seen_by_record<'a>(record: &NamesRecord<'_>, items: Option<(&ItemsRecord<'a>, ReadText<'_, 'a>)>, wholes: &[(Section, &[u8])], cell: NameCell) -> Result<Option<ContentId>, CellsError> {
    let items_of = || items.map(|(items, _)| items).ok_or(CellsError::NoItemsRecord);
    let read_of = || items.map(|(_, read)| read).ok_or(CellsError::NoItemsRecord);
    Ok(match cell {
        NameCell::Binding { module, name } => {
            let Some(view) = record.scope(module) else { return Ok(None) };
            let view: ScopeView<'_> = view;
            view.bindings().find(|entry| cell_key(entry.ns, entry.name) == name).map(|entry| binding_digest(entry.binding))
        }
        NameCell::Module { module } => module_digest(record, module),
        NameCell::Root { krate } => (krate == record.key().prefix()).then(|| root_digest(record)),
        NameCell::Variants { owner } => variants_digest(record, owner),
        NameCell::Item { key } => item_digest(items_of()?, key, read_of()?).map_err(CellsError::Items)?,
        NameCell::Members { owner } => members_digest(items_of()?, owner, read_of()?).map_err(CellsError::Items)?,
        NameCell::Expansion { call } => expansion_digest(items_of()?, call),
        NameCell::Whole { section, .. } => wholes.iter().find(|(s, _)| *s == section).map(|(s, bytes)| whole_digest(*s, bytes)),
    })
}
