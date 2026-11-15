//! A crate's `DefMap` as pages: one record a name, a module's scope and the crate's head are
//! each read from alone (docs/v2/plan/design/restore.md, "Def maps read in place").
//!
//! [`super::encode`] writes a crate's `DefMap` as one stream, which a reader decodes whole
//! into heap values before it can answer anything. This writes the same things as segments
//! of one record, each found by an offset, so a reader asks the store for the bytes of what
//! it was asked and no others:
//!
//! | segment | holds | read when |
//! |---|---|---|
//! | header, rows | where every other segment is, a row a module | the crate is first asked for |
//! | files | the files the segments name, as [`Enc::finish`] writes them | with the header |
//! | head | the crate's data, its module tree (origin, visibility, children), preludes | with the header |
//! | a module's name table | open-addressed slots: a name's hash and where its entry is | a name is looked up in the module |
//! | a name's entry | the name and what it means in each namespace ([`PerNs`]) | that name is looked up |
//! | a module's traits | the traits in scope in it, named or not (`ItemScope::traits`) | method resolution asks for the traits in scope |
//! | a module's textual macro table, and an entry a name | slots as a name table's; the name and the `macro_rules!` it means textually | a textual macro name is looked up |
//! | a module's items | its impls, built-in derive impls and declared traits | the impls maintainer is handed a served crate whose impls have no tables |
//! | a module's scope | the rest of its `ItemScope` ([`Enc::scope_rest`]), each entry's length in its run, and each namespace's names by their entry's position | something lists the whole scope (globs, a walk of every item): with the runs of its name entries and its textual macro entries, three reads |
//! | tail | name records, diagnostics, derive helpers | a lint or a diagnostic asks |
//!
//! What each lookup reads (rule 67; the floor is one read of the bytes the answer is in): a
//! name, a textual macro: the window of four slots (64 bytes) and the entry, two reads, one
//! where the record is mapped; the traits in scope, the items: one read of their segment.
//! Nothing a lookup reads is kept by this module past the read: the answer is the memo's, and
//! the record is the driver's (a `blaze_cas::direct` mapping, or a read into the reader's
//! frame), so name resolution holds no state of a crate read from pages.
//!
//! Every offset is from the record's start and every number little-endian, so the record
//! means the same wherever it is read and nothing is fixed up after a read. A segment is the
//! encoding of [`super`], with one difference: a macro call or a syntax context is written
//! whole the first time a *segment* names it, so a segment reads without any other. The files
//! are one table for the record.
//!
//! Nothing here is named by a database's id. What each id kind is written as is [`super`]'s:
//! a crate by its index in the closure, a file by its owning crate and path, a module by its
//! crate and index, every interned id by the location it interns, a name by its text (a
//! keyed one with its context). Reading interns again, so two databases write the same bytes
//! and read ids of their own.
//!
//! A name's table slot is found by the hash of its text; a lookup reads a window of slots
//! and the entry of each slot whose hash is the name's: two reads where the table has no
//! collision, each a few dozen bytes, whatever the crate's size.
//!
//! The bytes come through a [`PageSource`], which holds them only while a read runs: the
//! driver's is the store's pool, whose pages go back when nothing reads them. Bytes that do
//! not read are [`Unreadable`], naming the crate, never a crate resolved instead.
//!
//!
//! A module's names are written once: as entries, one after another in one run (and its
//! textual macros in a run of their own). The name table points into the run; the scope
//! segment names each entry by its position there, so a whole scope is decoded from the same
//! bytes a lookup reads, and no name is in the record twice.

use std::{borrow::Cow, sync::Arc};

use base_db::{Crate, Db, FileId};
use blaze_format::cursor::Reader;
use blaze_hash::{HashMap, HashSet};
use blaze_ids::ContentId;
use either::Either;
use hir_expand::name::Name;
use std::alloc::Global;
use triomphe::Arc as TArc;

use super::{Dec, Enc, R, Unreadable, closure};
use crate::{
    BuiltinDeriveImplId, ExternCrateId, FxIndexMap, ImplId, MacroId, ModuleDefId, ModuleId,
    TraitId, UseId,
    builtin_type::BuiltinType,
    item_scope::ItemScope,
    nameres::{
        DefMap, DefMapCrateData, DiagnosticsByModule, LocalDefMap, ModuleData, ModuleOrigin,
        Modules, ModulesMap, ModulesSlot, OwnedRecords, RecordsSlot, ScopeSlot,
    },
    per_ns::{Item, PerNs},
    unstable_features::UnstableFeatures,
    visibility::Visibility,
};

/// What [`Unreadable`] calls these bytes.
const KIND: &str = "def map pages";

/// What a record of pages starts with.
const MAGIC: [u8; 8] = *b"blzdmpg\0";

/// The layout of a record: it changes with every change to the header, the rows, the slots or
/// which segments there are. What a segment holds is [`super::ENCODING`]'s, which the header
/// names as well.
/// Layout 1 had no traits, textual macro or items segment: its readers of those decoded the
/// module's whole scope. Refused by the header like any other.
/// Layout 2 wrote a module's names twice, in its scope segment and as entries, and had no
/// runs of entries in its rows.
pub const LAYOUT: u32 = 3;

/// Bytes of the header.
const HEADER: usize = 56;
/// Bytes of a module's row.
const ROW: usize = 64;
/// Bytes of a name table's slot.
pub(super) const SLOT: usize = 16;
/// The slots a lookup reads at once: a name is in the first of them unless that many names
/// before it share its slot.
pub(super) const WINDOW: u32 = 4;

/// FNV-1a over `bytes`: the same in every process, which a hash of an interned id is not.
pub(super) const fn fnv(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut at = 0;
    while at < bytes.len() {
        hash ^= bytes[at] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        at += 1;
    }
    hash
}

/// The function a name's slot is found by ([`fnv`]), by name: part of the encoding word, so
/// pages whose slots another function placed are refused by the header and never probed
/// with this one. Change it with the function.
const SLOT_HASH: &str = "fnv-1a 64 of the name's text";

/// The encoding of the segments, as the header names it: [`super::ENCODING`] and
/// [`SLOT_HASH`].
const ENCODING: u64 =
    fnv(super::ENCODING.as_bytes()) ^ fnv(SLOT_HASH.as_bytes()).rotate_left(1);

/// The hash a name's slot is found by: of its text alone, so a reader needs no context to
/// make it. Two names of one text (a keyed one and a plain one) share a slot's hash and are
/// told apart by their entries. Never 0, which marks an empty slot.
fn hash_of(name: &Name) -> u64 {
    match fnv(name.as_str().as_bytes()) {
        0 => 1,
        hash => hash,
    }
}

/// Where the bytes of a crate's pages are. A read holds its bytes in memory only while
/// `with` runs: the store's pool pins the pages for that long, and gives them back when
/// nothing has read them for a while.
pub trait PageSource: Send + Sync + 'static {
    /// The record's length in bytes.
    fn size(&self) -> u64;

    /// Calls `with` once with the `len` bytes at `at`, checked as the store checks what it
    /// reads; `Err` when they cannot be read.
    // dyn: one call per read of a segment; the reader is generic over nothing it is called by
    fn read(&self, at: u64, len: u64, with: &mut dyn FnMut(&[u8])) -> Result<(), String>;
}

/// Pages held whole in memory: a test's, and a value just fetched that the store does not
/// hold yet.
pub struct Whole(pub crate::deps::ImageBuf);

impl PageSource for Whole {
    fn size(&self) -> u64 {
        self.0.bytes().len() as u64
    }

    // dyn: as `PageSource::read`
    fn read(&self, at: u64, len: u64, with: &mut dyn FnMut(&[u8])) -> Result<(), String> {
        let bytes = self.0.bytes();
        let range = usize::try_from(at).ok().zip(usize::try_from(len).ok());
        let part = range.and_then(|(at, len)| bytes.get(at..at.checked_add(len)?));
        // alloc: error path
        let part = part.ok_or_else(|| format!("bytes {at}+{len} of {}", bytes.len()))?;
        with(part);
        Ok(())
    }
}

/// `read` of the `len` bytes of `source` at `at`.
// dyn: the crate's one source
pub(super) fn read<T>(
    source: &dyn PageSource,
    at: u32,
    len: u32,
    read: impl FnOnce(&[u8]) -> R<T>,
) -> R<T> {
    let mut read = Some(read);
    let mut out = None;
    source.read(u64::from(at), u64::from(len), &mut |bytes| {
        let Some(read) = read.take() else { return };
        out = Some(match bytes.len() == len as usize {
            true => read(bytes),
            // alloc: error path
            false => Err(format!("{} bytes read at {at}, {len} asked", bytes.len())),
        });
    })?;
    // alloc: error path
    out.unwrap_or_else(|| Err(format!("nothing read at {at}")))
}

pub(super) fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

pub(super) fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from(u32_at(bytes, at)) | u64::from(u32_at(bytes, at + 4)) << 32
}

/// The header: where the rows, the files, the head and the tail are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    modules: u32,
    files: (u32, u32),
    head: (u32, u32),
    tail: (u32, u32),
    rows_at: u32,
    total: u32,
}

impl Header {
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&LAYOUT.to_le_bytes());
        out.extend_from_slice(&self.modules.to_le_bytes());
        out.extend_from_slice(&ENCODING.to_le_bytes());
        for it in [
            self.files.0,
            self.files.1,
            self.head.0,
            self.head.1,
            self.tail.0,
            self.tail.1,
            self.rows_at,
            self.total,
        ] {
            out.extend_from_slice(&it.to_le_bytes());
        }
    }

    /// The header in `bytes`, or why they are no pages this blaze reads: another record, a
    /// record of another layout, or one whose segments are of another encoding, each said by
    /// name.
    fn parse(bytes: &[u8]) -> R<Header> {
        if bytes.len() != HEADER {
            // alloc: error path
            return Err(format!("a header of {} bytes", bytes.len()));
        }
        if bytes[..8] != MAGIC {
            // alloc: error path
            return Err(format!("no def map pages: the record starts with {:02x?}", &bytes[..8]));
        }
        let layout = u32_at(bytes, 8);
        if layout != LAYOUT {
            // alloc: error path
            return Err(format!(
                "def map pages of layout {layout}, where this blaze reads layout {LAYOUT}"
            ));
        }
        let encoding = u64_at(bytes, 16);
        if encoding != ENCODING {
            // alloc: error path
            return Err(format!(
                "def map pages whose segments are of encoding {encoding:#x}, where this blaze \
                 reads `{}` ({ENCODING:#x})",
                super::ENCODING
            ));
        }
        let pair = |at: usize| (u32_at(bytes, at), u32_at(bytes, at + 4));
        Ok(Header {
            modules: u32_at(bytes, 12),
            files: pair(24),
            head: pair(32),
            tail: pair(40),
            rows_at: u32_at(bytes, 48),
            total: u32_at(bytes, 52),
        })
    }
}

/// One open-addressed table of a module's: where its slots are, how many (a power of two, or
/// 0 for a table with no key) and how many keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Table {
    at: u32,
    slots: u32,
    keys: u32,
}

/// A module's row: where its segments are, and its two tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Row {
    scope: (u32, u32),
    /// Its names in the three namespaces.
    names: Table,
    traits: (u32, u32),
    /// Its textual (`macro_rules!`) macros, by name.
    legacy: Table,
    items: (u32, u32),
    /// Its name entries, one after another: what `names` points into and `scope` names by
    /// position.
    entries: (u32, u32),
    /// Its textual macro entries, one after another: what `legacy` points into.
    textual: (u32, u32),
}

impl Row {
    fn write(&self, out: &mut Vec<u8>) {
        for it in [
            self.scope.0,
            self.scope.1,
            self.names.at,
            self.names.slots,
            self.names.keys,
            self.traits.0,
            self.traits.1,
            self.legacy.at,
            self.legacy.slots,
            self.legacy.keys,
            self.items.0,
            self.items.1,
            self.entries.0,
            self.entries.1,
            self.textual.0,
            self.textual.1,
        ] {
            out.extend_from_slice(&it.to_le_bytes());
        }
    }

    fn parse(bytes: &[u8]) -> Row {
        let pair = |at: usize| (u32_at(bytes, at), u32_at(bytes, at + 4));
        let table = |at: usize| Table {
            at: u32_at(bytes, at),
            slots: u32_at(bytes, at + 4),
            keys: u32_at(bytes, at + 8),
        };
        Row {
            scope: pair(0),
            names: table(8),
            traits: pair(20),
            legacy: table(28),
            items: pair(40),
            entries: pair(48),
            textual: pair(56),
        }
    }

    /// The row with every place moved by `base`: from the body's start to the record's.
    fn moved(self, base: u32) -> Row {
        let pair = |(at, len): (u32, u32)| (at + base, len);
        let table = |it: Table| Table { at: it.at + base, ..it };
        Row {
            scope: pair(self.scope),
            names: table(self.names),
            traits: pair(self.traits),
            legacy: table(self.legacy),
            items: pair(self.items),
            entries: pair(self.entries),
            textual: pair(self.textual),
        }
    }
}

/// A name table's slot: the name's hash (0: empty) and where its entry is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Slot {
    pub(super) hash: u64,
    pub(super) at: u32,
    pub(super) len: u32,
}

impl Slot {
    pub(super) fn parse(bytes: &[u8]) -> Slot {
        Slot { hash: u64_at(bytes, 0), at: u32_at(bytes, 8), len: u32_at(bytes, 12) }
    }
}

/// The slots of a table of `entries` (each a name's hash and where its entry is), as bytes:
/// twice as many slots as names, each name in the first empty slot from its hash's.
pub(super) fn table_of(entries: &[(u64, (u32, u32))]) -> (u32, Vec<u8>) {
    if entries.is_empty() {
        // alloc: none, an empty list
        return (0, Vec::new());
    }
    let count = (entries.len() * 2).next_power_of_two();
    let mask = count - 1;
    // alloc: encoding, once per module per write of a crate's pages
    let mut slots = vec![Slot::default(); count];
    for &(hash, (at, len)) in entries {
        let mut slot = hash as u32 as usize & mask;
        while slots[slot].hash != 0 {
            slot = (slot + 1) & mask;
        }
        slots[slot] = Slot { hash, at, len };
    }
    // alloc: encoding, as above
    let mut bytes = Vec::with_capacity(count * SLOT);
    for slot in slots {
        bytes.extend_from_slice(&slot.hash.to_le_bytes());
        bytes.extend_from_slice(&slot.at.to_le_bytes());
        bytes.extend_from_slice(&slot.len.to_le_bytes());
    }
    (count as u32, bytes)
}

/// The pages a crate is read from.
#[derive(Clone)]
pub struct Paged {
    /// The key the record is kept under: equal keys are equal bytes, as the store checks.
    pub key: ContentId,
    // dyn: the driver's holder of one record, one a crate
    pub source: Arc<dyn PageSource>,
}

impl PartialEq for Paged {
    fn eq(&self, other: &Paged) -> bool {
        self.key == other.key
    }
}

impl Eq for Paged {}

impl std::hash::Hash for Paged {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl std::fmt::Debug for Paged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Paged").field("key", &self.key).field("len", &self.source.size()).finish()
    }
}

/// The crates whose `DefMap` is read from pages, set by whoever has the records: read
/// outside queries alone ([`all`]). A query reads its crate's own ([`PagesSlot`], [`paged`]).
#[blaze_query::input(singleton, debug)]
pub struct PagedDefMaps {
    #[returns(ref)]
    pub maps: Arc<HashMap<Crate, Paged, Global>>,
}

/// Has name resolution read each crate of `maps` from its pages.
pub fn set(db: &mut Db, maps: HashMap<Crate, Paged, Global>) {
    use blaze_query::db::{Durability, Setter};
    // clone: the handle of the map set before, to tell which crates' pages move
    let was = PagedDefMaps::try_get(&*db).map(|it| it.maps(&*db).clone());
    crate::deps::given::give_changed::<PagesSlot, Paged>(db, was.as_deref(), &maps);
    // alloc: once per serve that changes which crates are read from pages
    let maps = Arc::new(maps);
    match PagedDefMaps::try_get(db) {
        Some(it) => {
            it.set_maps(db).with_durability(Durability::HIGH).to(maps);
        }
        None => {
            _ = PagedDefMaps::new(db, Durability::HIGH, maps);
        }
    }
}

/// The crates read from pages, to add to or take from.
pub fn all(db: &Db) -> HashMap<Crate, Paged, Global> {
    // clone: what was set before, once per serve that changes it
    PagedDefMaps::try_get(db).map(|it| (**it.maps(db)).clone()).unwrap_or_default()
}

/// The pages of `krate`, by value: a [`set`] that leaves them as they were is backdated here,
/// so it runs none of the queries of `krate` again.
#[blaze_query::tracked(returns(as_ref))]
pub(super) fn paged(db: &Db, krate: Crate) -> Option<Paged> {
    // clone: a count of the source and a key
    krate.given::<PagesSlot, Paged>(db).cloned()
}

/// The kind of a crate's slot holding its entry of [`PagedDefMaps`].
pub struct PagesSlot;

/// Whether `krate` is read from pages.
pub fn is_paged(db: &Db, krate: Crate) -> bool {
    paged(db, krate).is_some()
}

/// A module of a crate read from pages: what a `ScopeSlot::Paged` names.
#[blaze_query::interned(debug, unsafe(no_lifetime))]
pub(crate) struct PagedScope {
    #[returns(copy)]
    pub(crate) krate: Crate,
    /// The module's index in its crate's `DefMap`.
    #[returns(copy)]
    pub(crate) module: u32,
}

/// One module as the head has it.
#[derive(Debug, PartialEq, Eq)]
struct HeadModule {
    origin: ModuleOrigin,
    visibility: Visibility,
    parent: Option<ModuleId>,
    children: FxIndexMap<Name, ModuleId>,
}

/// What every reader of a crate's pages needs first, read once: where each module's segments
/// are, the files the segments name, and the head.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Head {
    rows: Box<[Row]>,
    tail: (u32, u32),
    files: Box<[FileId]>,
    /// The crate's modules, in its `DefMap`'s order.
    own: Box<[ModuleId]>,
    data: TArc<DefMapCrateData>,
    modules: Box<[HeadModule]>,
    /// Whether a module of the crate has a primitive type's name in its type namespace
    /// ([`shadows_primitive_in`]).
    shadows_primitive: bool,
    prelude: Option<(ModuleId, Option<UseId>)>,
    macro_use_prelude: HashMap<Name, (MacroId, Option<ExternCrateId>), Global>,
    extern_prelude: FxIndexMap<Name, (ModuleId, Option<ExternCrateId>)>,
}

/// Whether `scope` has a primitive type's name (`u8`, `str`, `bool`, ..) in its type
/// namespace: one module's part of `crate_shadows_primitive_type`.
pub(in crate::nameres) fn shadows_primitive_in(scope: &ItemScope) -> bool {
    scope.types().any(|(name, _)| BuiltinType::by_name(name).is_some())
}

/// Whether a module of `krate` has a primitive type's name in its type namespace, when the
/// crate is read from pages: its head says, so no scope is decoded to answer. `None` for a
/// crate not read from pages.
pub(in crate::nameres) fn shadows_primitive(db: &Db, krate: Crate) -> Option<bool> {
    is_paged(db, krate).then(|| head(db, krate).shadows_primitive)
}

/// `krate` and the crates it depends on, as [`closure`] lists them: what every segment's
/// reader names crates by, made once a crate and not once a read.
#[blaze_query::tracked(returns(ref))]
fn closure_of(db: &Db, krate: Crate) -> Vec<Crate> {
    closure(db, krate)
}

/// The head of the pages of `krate`. Depends on which pages the crate has and on the files
/// they name.
#[blaze_query::tracked(returns(ref))]
fn head(db: &Db, krate: Crate) -> Head {
    // The value is the record's, found by content: told to the engine (`row::STORED`).
    blaze_query::db::report_stored_read(db);
    let Some(paged) = paged(db, krate) else {
        // alloc: error path
        Unreadable::of(db, krate, KIND, "the crate has no pages".to_owned()).throw()
    };
    match read_head(db, krate, &*paged.source) {
        Ok(head) => head,
        Err(why) => Unreadable::of(db, krate, KIND, why).throw(),
    }
}

/// A reader of the segment `bytes` of the pages of `krate`, whose files are `files` and
/// whose modules are `own`.
pub(super) fn segment<'a>(
    db: &'a Db,
    krate: Crate,
    files: &'a [FileId],
    own: &'a [ModuleId],
    bytes: &'a [u8],
) -> Dec<'a> {
    Dec {
        db,
        r: Reader::new(bytes),
        krate,
        closure: Cow::Borrowed(closure_of(db, krate).as_slice()),
        paths: HashMap::new_in(Global),
        files: Cow::Borrowed(files),
        // alloc: none until the segment names a macro call
        calls: Vec::new(),
        // alloc: none until the segment names a syntax context
        contexts: Vec::new(),
        own: Cow::Borrowed(own),
        in_def_map: true,
        modules: HashMap::new_in(Global),
    }
}

// dyn: the crate's one source
fn read_head(db: &Db, krate: Crate, source: &dyn PageSource) -> R<Head> {
    let header = read(source, 0, HEADER as u32, Header::parse)?;
    if u64::from(header.total) != source.size() {
        // alloc: error path
        return Err(format!(
            "a record of {} bytes whose header says {}",
            source.size(),
            header.total
        ));
    }
    let rows_len = (header.modules as usize)
        .checked_mul(ROW)
        .and_then(|it| u32::try_from(it).ok())
        .ok_or("rows over the record's size")?;
    let rows = read(source, header.rows_at, rows_len, |bytes| {
        // alloc: the head's own rows, once per crate read from pages
        Ok(bytes.chunks_exact(ROW).map(Row::parse).collect::<Box<[Row]>>())
    })?;
    let files = read(source, header.files.0, header.files.1, |bytes| {
        let mut dec = Dec::new(db, krate, bytes);
        dec.files()?;
        dec.finish()?;
        // alloc: the head's own files, once per crate read from pages
        Ok(dec.files.into_owned().into_boxed_slice())
    })?;
    let mut head = read(source, header.head.0, header.head.1, |bytes| {
        let mut dec = segment(db, krate, &files, &[], bytes);
        let head = dec.pages_head()?;
        dec.finish()?;
        Ok(head)
    })?;
    if head.modules.len() != rows.len() {
        // alloc: error path
        return Err(format!("{} modules and {} rows", head.modules.len(), rows.len()));
    }
    head.rows = rows;
    head.tail = header.tail;
    head.files = files;
    Ok(head)
}

/// Why the pages of `krate` in `source` cannot be read in this database, if they cannot: their
/// header does not parse or does not match the record's size, or a file they name is not
/// loaded or has other content than they were made from. Reads the header and the files
/// segment alone, a few hundred bytes: what a serve checks of a crate's pages before it gives
/// them, so pages a key left an input out of are an error of the serve, not of the first
/// query that reads them.
// dyn: the crate's one source
pub fn unreadable(db: &Db, krate: Crate, source: &dyn PageSource) -> Option<String> {
    let checked = || -> R {
        let header = read(source, 0, HEADER as u32, Header::parse)?;
        if u64::from(header.total) != source.size() {
            // alloc: error path
            return Err(format!(
                "a record of {} bytes whose header says {}",
                source.size(),
                header.total
            ));
        }
        read(source, header.files.0, header.files.1, |bytes| {
            let mut dec = Dec::new(db, krate, bytes);
            dec.files()?;
            dec.finish()
        })
    };
    checked().err()
}

/// What [`crate::nameres::crate_local_def_map`] serves for a crate read from pages: its head
/// as a `DefMap` whose modules' scopes and records are still in the record, read when asked.
pub(super) fn served(db: &Db, krate: Crate) -> (DefMap, LocalDefMap) {
    let head = head(db, krate);
    let module = |index: usize| {
        let HeadModule { origin, visibility, parent, children: _ } = &head.modules[index];
        let paged = PagedScope::new(db, krate, index as u32);
        ModuleData {
            origin: *origin,
            visibility: *visibility,
            parent: *parent,
            scope: ScopeSlot::Paged(blaze_query::db::AsId::as_id(&paged)),
        }
    };
    // clone: a count of the crate's data
    let mut def_map = DefMap::empty(db, krate, head.data.clone(), module(0), None);
    if head.own.first() != Some(&def_map.root) {
        // alloc: error path
        Unreadable::of(db, krate, KIND, "a head whose first module is not the root".to_owned())
            .throw()
    }
    let mut table = ModulesMap::new();
    for (index, &id) in head.own.iter().enumerate() {
        table.insert(id, module(index));
    }
    let modules = Modules::new(db, def_map.root, table);
    def_map.modules = ModulesSlot::Collected(blaze_query::db::AsId::as_id(&modules));
    def_map.prelude = head.prelude;
    // clone: the crate's `#[macro_use]` prelude, once per crate read from pages
    def_map.macro_use_prelude = head.macro_use_prelude.clone();
    def_map.records = Some(RecordsSlot::Paged(krate));
    // clone: the crate's extern prelude, once per crate read from pages
    let local = LocalDefMap { extern_prelude: head.extern_prelude.clone() };
    (def_map, local)
}

/// What a read of one of a paged scope's memos was of.
pub(crate) enum PagedRead {
    /// One name, in the scope's namespaces or among its textual macros.
    Name(Name),
    Traits,
    /// The whole scope, decoded.
    Whole,
}

/// The crate and paged scope a read of one of this file's memos was of, and what of it
/// (`crate::nameres::name_read`): `name` is the memo's name as the engine has it, `key` its
/// key. `None` for a row that is no memo of a scope (the head, the tail, the crate's slot).
pub(crate) fn read_of(
    db: &Db,
    name: &str,
    key: blaze_query::db::Id,
) -> Option<(Crate, blaze_query::db::Id, PagedRead)> {
    use blaze_query::db::AsId;
    let (paged, of) = match name {
        "name_memo" => {
            let (paged, name) = name_memo::arguments(db, key)?;
            (paged, PagedRead::Name(name))
        }
        "legacy_macro_memo" => {
            let (paged, name) = legacy_macro_memo::arguments(db, key)?;
            (paged, PagedRead::Name(name))
        }
        "scope_memo" => (key_of(db, key).0, PagedRead::Whole),
        "traits_memo" => (key_of(db, key).0, PagedRead::Traits),
        _ => return None,
    };
    Some((paged.krate(db), paged.as_id(), of))
}

/// The place of the paged scope `paged`'s module in its crate's table of modules.
pub(crate) fn paged_index(db: &Db, paged: blaze_query::db::Id) -> usize {
    key_of(db, paged).2
}

fn key_of(db: &Db, paged: blaze_query::db::Id) -> (PagedScope, Crate, usize) {
    let paged = <PagedScope as blaze_query::db::FromId>::from_id(paged);
    (paged, paged.krate(db), paged.module(db) as usize)
}

/// The source of the pages of `krate`, with its head.
// dyn: the crate's one source
fn source_of(db: &Db, krate: Crate) -> R<(&dyn PageSource, &Head)> {
    let paged = paged(db, krate).ok_or("the crate has no pages")?;
    Ok((&*paged.source, head(db, krate)))
}

/// What `name` means in the module `paged` names, read now from the page it is on: for the
/// one memo of a path segment (`Modules::name`), which keeps the answer.
pub(in crate::nameres) fn name_now(db: &Db, paged: blaze_query::db::Id, name: &Name) -> PerNs {
    let (_, krate, module) = key_of(db, paged);
    match look_up(db, krate, module, name) {
        Ok(found) => found,
        Err(why) => {
            // alloc: error path
            let why = format!("`{}` in module {module}: {why}", name.as_str());
            Unreadable::of(db, krate, KIND, why).throw()
        }
    }
}

#[cfg(test)]
thread_local! {
    /// The bug a test planted: a name's lookup serves the entry of the module's first name in
    /// slot order, whatever name it holds: an entry of another name served.
    pub(crate) static SERVES_ANOTHER_NAMES_ENTRY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The entry of `table` whose slot has `hash` and of which `entry` gives `Some`: `entry`
/// reads an entry's bytes, gives `None` for another key of the same hash. `None` when the
/// walk from the hash's slot ends at an empty one: the key is not in the table, known from
/// the one window read. Reads a window of [`WINDOW`] slots, then each entry of the hash.
// dyn: the crate's one source
fn find<T>(
    source: &dyn PageSource,
    table: Table,
    hash: u64,
    mut entry: impl FnMut(&[u8]) -> R<Option<T>>,
) -> R<Option<T>> {
    if table.slots == 0 {
        return Ok(None);
    }
    if !table.slots.is_power_of_two() {
        // alloc: error path
        return Err(format!("a table of {} slots", table.slots));
    }
    let mask = table.slots - 1;
    let mut at = hash as u32 & mask;
    let mut probed = 0;
    // The bound: every slot once. A table is at most half full, so a walk ends at an empty
    // slot long before; one that does not is a table this did not write.
    while probed < table.slots {
        let run = WINDOW.min(table.slots - at);
        let slots = read(source, table.at + at * SLOT as u32, run * SLOT as u32, |bytes| {
            let mut slots = [Slot::default(); WINDOW as usize];
            for (slot, bytes) in slots.iter_mut().zip(bytes.chunks_exact(SLOT)) {
                *slot = Slot::parse(bytes);
            }
            Ok(slots)
        })?;
        for slot in &slots[..run as usize] {
            if slot.hash == 0 {
                return Ok(None);
            }
            if slot.hash != hash {
                continue;
            }
            if let Some(found) = read(source, slot.at, slot.len, &mut entry)? {
                return Ok(Some(found));
            }
        }
        probed += run;
        at = (at + run) & mask;
    }
    // alloc: error path
    Err(format!("a table of {} slots with none empty", table.slots))
}

fn look_up(db: &Db, krate: Crate, module: usize, name: &Name) -> R<PerNs> {
    let (source, head) = source_of(db, krate)?;
    let row = *head.rows.get(module).ok_or("a module the pages have no row of")?;
    #[cfg(test)]
    if SERVES_ANOTHER_NAMES_ENTRY.get() && row.names.slots != 0 {
        let len = row.names.slots * SLOT as u32;
        let first = read(source, row.names.at, len, |bytes| {
            Ok(bytes.chunks_exact(SLOT).map(Slot::parse).find(|it| it.hash != 0))
        })?;
        if let Some(slot) = first {
            return read(source, slot.at, slot.len, |bytes| {
                let mut dec = segment(db, krate, &head.files, &head.own, bytes);
                dec.name()?;
                let found = dec.per_ns()?;
                dec.finish()?;
                Ok(found)
            });
        }
    }
    let found = find(source, row.names, hash_of(name), |bytes| {
        let mut dec = segment(db, krate, &head.files, &head.own, bytes);
        if dec.name()? != *name {
            return Ok(None);
        }
        let found = dec.per_ns()?;
        dec.finish()?;
        Ok(Some(found))
    })?;
    Ok(found.unwrap_or_else(PerNs::none))
}

/// The `macro_rules!` that `name` means textually at the end of the module `module` of
/// `krate`: its textual macro table's window and the name's entry, nothing else.
fn look_up_legacy(
    db: &Db,
    krate: Crate,
    module: usize,
    name: &Name,
) -> R<Option<Box<[MacroId]>>> {
    let (source, head) = source_of(db, krate)?;
    let row = *head.rows.get(module).ok_or("a module the pages have no row of")?;
    find(source, row.legacy, hash_of(name), |bytes| {
        let mut dec = segment(db, krate, &head.files, &head.own, bytes);
        if dec.name()? != *name {
            return Ok(None);
        }
        let ids = dec.many(Dec::macro_id)?;
        dec.finish()?;
        // alloc: the memo's value, once per textual macro name looked up
        Ok(Some(ids.into_boxed_slice()))
    })
}

/// The traits in scope in the module `module` of `krate`, from their segment alone.
fn read_traits(db: &Db, krate: Crate, module: usize) -> R<Box<[TraitId]>> {
    let (source, head) = source_of(db, krate)?;
    let row = *head.rows.get(module).ok_or("a module the pages have no row of")?;
    read(source, row.traits.0, row.traits.1, |bytes| {
        let mut dec = segment(db, krate, &head.files, &head.own, bytes);
        let traits = dec.many(trait_of)?;
        dec.finish()?;
        // alloc: the memo's value, once per module whose traits are asked for
        Ok(traits.into_boxed_slice())
    })
}

fn trait_of(dec: &mut Dec<'_>) -> R<TraitId> {
    match dec.def()? {
        ModuleDefId::TraitId(it) => Ok(it),
        // alloc: error path
        _ => Err("a trait in scope that is no trait".to_owned()),
    }
}

/// What the impls maintainer is handed of one module of a crate read from pages: its impls,
/// its built-in derive impls and the traits it declares, in its scope's order
/// (`delta::diff::Items::add`'s reads of a scope).
pub(in crate::nameres) struct ModuleItems {
    pub(in crate::nameres) impls: Vec<ImplId>,
    pub(in crate::nameres) derive_impls: Vec<BuiltinDeriveImplId>,
    pub(in crate::nameres) traits: Vec<TraitId>,
}

/// How many modules the crate read from pages `krate` has: its head's.
pub(in crate::nameres) fn modules(db: &Db, krate: Crate) -> usize {
    head(db, krate).rows.len()
}

/// The items of the module at `module` of the crate read from pages `krate`: one read of its
/// items segment, and no scope decoded. Not a memo: its one reader, the hand-over of a served
/// crate to the impls maintainer, reads each module once and keeps nothing.
pub(in crate::nameres) fn module_items(db: &Db, krate: Crate, module: usize) -> ModuleItems {
    let read_items = || {
        let (source, head) = source_of(db, krate)?;
        let row = *head.rows.get(module).ok_or("a module the pages have no row of")?;
        read(source, row.items.0, row.items.1, |bytes| {
            let mut dec = segment(db, krate, &head.files, &head.own, bytes);
            let impls = dec.many(Dec::impl_id)?;
            let derive_impls = dec.many(Dec::builtin_derive_impl)?;
            let traits = dec.many(trait_of)?;
            dec.finish()?;
            Ok(ModuleItems { impls, derive_impls, traits })
        })
    };
    match read_items() {
        Ok(items) => items,
        Err(why) => {
            // alloc: error path
            let why = format!("the items of module {module}: {why}");
            Unreadable::of(db, krate, KIND, why).throw()
        }
    }
}

/// What `name` means in the module `paged` names, kept: for a reader that has the module's
/// row and not the `DefMap` (`ModuleData::name_in_scope`).
#[blaze_query::tracked(returns(copy))]
fn name_memo(db: &Db, paged: PagedScope, name: Name) -> PerNs {
    // The value is the record's, found by content: told to the engine (`row::STORED`).
    blaze_query::db::report_stored_read(db);
    name_now(db, blaze_query::db::AsId::as_id(&paged), &name)
}

pub(in crate::nameres) fn name(db: &Db, paged: blaze_query::db::Id, name: Name) -> PerNs {
    name_memo(db, key_of(db, paged).0, name)
}

/// The whole scope of the module `paged` names, decoded: for a reader of all of it. A name
/// looked up never asks for this.
#[blaze_query::tracked(returns(ref))]
fn scope_memo(db: &Db, paged: PagedScope) -> ItemScope {
    // The value is the record's, found by content: told to the engine (`row::STORED`).
    blaze_query::db::report_stored_read(db);
    let (krate, module) = (paged.krate(db), paged.module(db) as usize);
    let scope = source_of(db, krate).and_then(|(source, head)| {
        let row = *head.rows.get(module).ok_or("a module the pages have no row of")?;
        let mut scope = ItemScope::default();
        // The scope segment: each entry's length in its run, each namespace's names by their
        // entry's position, and the rest of the scope.
        let (lens, [types, values, macros], textual) =
            read(source, row.scope.0, row.scope.1, |bytes| {
                let mut dec = segment(db, krate, &head.files, &head.own, bytes);
                // alloc: decoding a whole scope, once per module whose whole scope is asked for
                let lens = dec.many(Dec::u32)?;
                let columns = [dec.many(Dec::u32)?, dec.many(Dec::u32)?, dec.many(Dec::u32)?];
                let textual = dec.many(Dec::u32)?;
                dec.scope_rest(&mut scope, false)?;
                dec.finish()?;
                Ok((lens, columns, textual))
            })?;
        // The name entries, in one read of their run.
        let entries = read(source, row.entries.0, row.entries.1, |bytes| {
            // alloc: as above
            let mut entries = Vec::with_capacity(lens.len());
            let mut at = 0;
            for &len in &lens {
                let bytes = in_run(bytes, &mut at, len)?;
                let mut dec = segment(db, krate, &head.files, &head.own, bytes);
                let name = dec.name()?;
                let found = dec.per_ns()?;
                dec.finish()?;
                entries.push((name, found));
            }
            run_ends(bytes, at)?;
            Ok(entries)
        })?;
        let entry = |at: u32| {
            // alloc: error path
            entries.get(at as usize).ok_or_else(|| format!("entry {at} of {}", entries.len()))
        };
        // alloc: error path
        let none = |name: &Name, ns: &str| format!("`{}` named in {ns} by an entry with none", name.as_str());
        scope.types.reserve(types.len());
        for &at in &types {
            let (name, found) = entry(at)?;
            scope.types.push_unindexed(*name, found.types.ok_or_else(|| none(name, "types"))?);
        }
        scope.values.reserve(values.len());
        for &at in &values {
            let (name, found) = entry(at)?;
            scope.values.push_unindexed(*name, found.values.ok_or_else(|| none(name, "values"))?);
        }
        scope.macros.reserve(macros.len());
        for &at in &macros {
            let (name, found) = entry(at)?;
            scope.macros.push_unindexed(*name, found.macros.ok_or_else(|| none(name, "macros"))?);
        }
        // The textual macro entries, in one read of theirs.
        read(source, row.textual.0, row.textual.1, |bytes| {
            scope.legacy_macros.reserve(textual.len());
            let mut at = 0;
            for &len in &textual {
                let bytes = in_run(bytes, &mut at, len)?;
                let mut dec = segment(db, krate, &head.files, &head.own, bytes);
                let name = dec.name()?;
                let ids = dec.small(Dec::macro_id)?;
                dec.finish()?;
                scope.legacy_macros.push_unindexed(name, ids);
            }
            run_ends(bytes, at)
        })?;
        scope.seal().map_err(str::to_owned)?;
        Ok(scope)
    });
    match scope {
        Ok(scope) => scope,
        Err(why) => {
            // alloc: error path
            let why = format!("the scope of module {module}: {why}");
            Unreadable::of(db, krate, KIND, why).throw()
        }
    }
}

/// The `len` bytes of a run at `at`, which moves past them.
fn in_run<'b>(run: &'b [u8], at: &mut usize, len: u32) -> R<&'b [u8]> {
    let end = at.checked_add(len as usize).filter(|&end| end <= run.len());
    // alloc: error path
    let end = end.ok_or_else(|| format!("an entry of {len} bytes at {at} of a run of {}", run.len()))?;
    let bytes = &run[*at..end];
    *at = end;
    Ok(bytes)
}

/// A run read to `at` is read to its end: no bytes after its last entry.
fn run_ends(run: &[u8], at: usize) -> R {
    match at == run.len() {
        true => Ok(()),
        // alloc: error path
        false => Err(format!("a run of {} bytes whose entries end at {at}", run.len())),
    }
}

pub(in crate::nameres) fn scope(db: &Db, paged: blaze_query::db::Id) -> &ItemScope {
    scope_memo(db, key_of(db, paged).0)
}

#[blaze_query::tracked(returns(ref))]
fn legacy_macro_memo(db: &Db, paged: PagedScope, name: Name) -> Option<Box<[MacroId]>> {
    // The value is the record's, found by content: told to the engine (`row::STORED`).
    blaze_query::db::report_stored_read(db);
    let (krate, module) = (paged.krate(db), paged.module(db) as usize);
    match look_up_legacy(db, krate, module, &name) {
        Ok(found) => found,
        Err(why) => {
            // alloc: error path
            let why = format!("textual macro `{}` in module {module}: {why}", name.as_str());
            Unreadable::of(db, krate, KIND, why).throw()
        }
    }
}

pub(in crate::nameres) fn legacy_macro(
    db: &Db,
    paged: blaze_query::db::Id,
    name: Name,
) -> Option<&[MacroId]> {
    legacy_macro_memo(db, key_of(db, paged).0, name).as_deref()
}

#[blaze_query::tracked(returns(ref))]
fn traits_memo(db: &Db, paged: PagedScope) -> Box<[TraitId]> {
    // The value is the record's, found by content: told to the engine (`row::STORED`).
    blaze_query::db::report_stored_read(db);
    let (krate, module) = (paged.krate(db), paged.module(db) as usize);
    match read_traits(db, krate, module) {
        Ok(traits) => traits,
        Err(why) => {
            // alloc: error path
            let why = format!("the traits in scope in module {module}: {why}");
            Unreadable::of(db, krate, KIND, why).throw()
        }
    }
}

pub(in crate::nameres) fn traits(db: &Db, paged: blaze_query::db::Id) -> &[TraitId] {
    traits_memo(db, key_of(db, paged).0)
}

/// The modules declared in the module `paged` names, by name: the head's.
pub(in crate::nameres) fn children(
    db: &Db,
    paged: blaze_query::db::Id,
) -> &FxIndexMap<Name, ModuleId> {
    let (_, krate, module) = key_of(db, paged);
    match head(db, krate).modules.get(module) {
        Some(it) => &it.children,
        None => {
            // alloc: error path
            let why = format!("module {module}, which the head lacks");
            Unreadable::of(db, krate, KIND, why).throw()
        }
    }
}

/// The records of a crate read from pages: its tail, decoded when first asked for.
#[blaze_query::tracked(returns(ref))]
fn tail(db: &Db, krate: Crate) -> OwnedRecords {
    // The value is the record's, found by content: told to the engine (`row::STORED`).
    blaze_query::db::report_stored_read(db);
    let records = source_of(db, krate).and_then(|(source, head)| {
        read(source, head.tail.0, head.tail.1, |bytes| {
            let mut dec = segment(db, krate, &head.files, &head.own, bytes);
            let records = dec.pages_tail()?;
            dec.finish()?;
            Ok(records)
        })
    });
    match records {
        Ok(records) => records,
        // alloc: error path
        Err(why) => Unreadable::of(db, krate, KIND, format!("its records: {why}")).throw(),
    }
}

pub(in crate::nameres) fn records(db: &Db, krate: Crate) -> &OwnedRecords {
    tail(db, krate)
}

/// The pages of the `DefMap` of `krate` as name resolution computes it, or why it cannot be
/// written. Refuses what [`super::encode`] refuses.
pub fn encode(db: &Db, krate: Crate) -> R<Vec<u8>> {
    let pair = crate::nameres::crate_local_def_map(db, krate);
    encode_maps(db, krate, pair.def_map(db), pair.local(db), None)
}

/// The pages of `def_map` and `local`, the maps of `krate` (resolved, or read from pages):
/// what [`super::check`] and a verify of [`super::served`] compare with a crate's record.
pub(in crate::nameres) fn encode_of(
    db: &Db,
    krate: Crate,
    def_map: &DefMap,
    local: &LocalDefMap,
) -> R<Vec<u8>> {
    encode_maps(db, krate, def_map, local, None)
}

/// [`encode`] with `name` left out of every name table: a test's planted bug, for an oracle
/// outside this module to catch.
#[cfg(test)]
pub(crate) fn encode_without(db: &Db, krate: Crate, name: &str) -> R<Vec<u8>> {
    let pair = crate::nameres::crate_local_def_map(db, krate);
    encode_maps(db, krate, pair.def_map(db), pair.local(db), Some(name))
}

/// Writes one segment into `body`, and returns where it is there.
pub(super) fn write_segment(
    enc: &mut Enc<'_>,
    body: &mut Vec<u8>,
    write: impl FnOnce(&mut Enc<'_>) -> R,
) -> R<(u32, u32)> {
    // A segment reads without any other: what it names is written whole the first time it
    // names it, whatever the segments before it named.
    enc.calls.clear();
    enc.contexts.clear();
    debug_assert!(enc.out.is_empty(), "a segment begun with bytes left of the one before");
    write(enc)?;
    let bytes = std::mem::take(&mut enc.out);
    let at = u32::try_from(body.len()).map_err(|_| "pages over 4 GiB")?;
    let len = u32::try_from(bytes.len()).map_err(|_| "a segment over 4 GiB")?;
    body.extend_from_slice(&bytes);
    Ok((at, len))
}

/// The names `scope` binds, each once, in the order of its namespaces' entries.
fn names_of(scope: &ItemScope) -> Vec<Name> {
    let mut seen: HashSet<Name, Global> = HashSet::new_in(Global);
    // alloc: encoding, once per module per write of a crate's pages
    let mut names = Vec::new();
    let mut add = |name: &Name| {
        if seen.insert(*name) {
            names.push(*name);
        }
    };
    for (name, _) in scope.types.iter() {
        add(name);
    }
    for (name, _) in scope.values.iter() {
        add(name);
    }
    for (name, _) in scope.macros.iter() {
        add(name);
    }
    names
}

/// One namespace of a scope segment: how many names, then each one's entry's position in its
/// module's run, in the namespace's order.
fn positions<'n>(
    enc: &mut Enc<'_>,
    position: &HashMap<Name, u32, Global>,
    names: impl ExactSizeIterator<Item = &'n Name>,
) -> R {
    enc.usize(names.len())?;
    for name in names {
        // alloc: error path
        let at = position.get(name).ok_or_else(|| format!("`{}` with no entry", name.as_str()))?;
        enc.u32(*at);
    }
    Ok(())
}

/// Where the segments written to `body` since `from` are, as one run: from the body's start.
fn run_from(from: usize, body: &[u8]) -> R<(u32, u32)> {
    let at = u32::try_from(from).map_err(|_| "pages over 4 GiB")?;
    let len = u32::try_from(body.len() - from).map_err(|_| "a run over 4 GiB")?;
    Ok((at, len))
}

/// The table of `entries` ([`table_of`]) added to `body`: where it is, from the body's start.
fn table_in(body: &mut Vec<u8>, entries: &[(u64, (u32, u32))]) -> R<Table> {
    let (slots, bytes) = table_of(entries);
    let at = u32::try_from(body.len()).map_err(|_| "pages over 4 GiB")?;
    body.extend_from_slice(&bytes);
    Ok(Table { at, slots, keys: entries.len() as u32 })
}

/// [`encode`]; `without` leaves that name out of every name table: a test's planted bug.
fn encode_maps(
    db: &Db,
    krate: Crate,
    def_map: &DefMap,
    local: &LocalDefMap,
    without: Option<&str>,
) -> R<Vec<u8>> {
    let mut enc = Enc::new(db, krate);
    let table = def_map.table(db);
    for (index, &module) in table.keys().enumerate() {
        enc.modules.insert(module, (0, index as u32));
    }
    enc.indexed.insert(krate);

    // Every segment, one after another; where each is, from the body's start.
    // alloc: encoding, once per crate per write of its pages (not on a build's path)
    let mut body: Vec<u8> = Vec::new();
    let head = write_segment(&mut enc, &mut body, |enc| enc.pages_head(def_map, local))?;
    let tail = write_segment(&mut enc, &mut body, |enc| enc.pages_tail(def_map))?;
    // alloc: encoding, as above
    let mut rows: Vec<Row> = Vec::with_capacity(table.len());
    for (_, data) in table.iter() {
        let scope = data.scope(db);
        let names_here = names_of(scope);
        // Each name's entry, once, one after another: the name table points at it and the
        // scope segment names it by its position in the run.
        // alloc: encoding, as above
        let mut position: HashMap<Name, u32, Global> = HashMap::new_in(Global);
        // alloc: encoding, as above
        let mut entry_lens: Vec<u32> = Vec::with_capacity(names_here.len());
        // alloc: encoding, as above
        let mut entries: Vec<(u64, (u32, u32))> = Vec::with_capacity(names_here.len());
        let entries_from = body.len();
        for name in &names_here {
            let at = write_segment(&mut enc, &mut body, |enc| {
                enc.name(name)?;
                enc.per_ns(scope.get(name))
            })?;
            position.insert(*name, entry_lens.len() as u32);
            entry_lens.push(at.1);
            // The plant leaves the name out of the table only: its entry is the scope's too.
            if without.is_some_and(|it| it == name.as_str()) {
                continue;
            }
            entries.push((hash_of(name), at));
        }
        let entries_run = run_from(entries_from, &body)?;
        // alloc: encoding, as above
        let mut legacy: Vec<(u64, (u32, u32))> = Vec::new();
        // alloc: encoding, as above
        let mut textual_lens: Vec<u32> = Vec::new();
        // In the order of the names' text: a table's slots are placed in the order of their
        // entries, which must be no database's.
        let mut textual: Vec<(&Name, &[MacroId])> = scope.legacy_macros().collect(); // alloc: encoding, as above
        textual.sort_unstable_by(|a, b| a.0.symbol().cmp_order(b.0.symbol()));
        let textual_from = body.len();
        for (name, ids) in textual {
            let at = write_segment(&mut enc, &mut body, |enc| {
                enc.name(name)?;
                enc.usize(ids.len())?;
                ids.iter().try_for_each(|&it| enc.macro_id(it))
            })?;
            textual_lens.push(at.1);
            legacy.push((hash_of(name), at));
        }
        let textual_run = run_from(textual_from, &body)?;
        let whole = write_segment(&mut enc, &mut body, |enc| {
            enc.usize(entry_lens.len())?;
            entry_lens.iter().for_each(|&it| enc.u32(it));
            positions(enc, &position, scope.types.iter().map(|(name, _)| name))?;
            positions(enc, &position, scope.values.iter().map(|(name, _)| name))?;
            positions(enc, &position, scope.macros.iter().map(|(name, _)| name))?;
            enc.usize(textual_lens.len())?;
            textual_lens.iter().for_each(|&it| enc.u32(it));
            enc.scope_rest(scope, false)
        })?;
        let names = table_in(&mut body, &entries)?;
        let traits = write_segment(&mut enc, &mut body, |enc| {
            let traits: Vec<TraitId> = scope.traits().collect(); // alloc: encoding, as above
            enc.usize(traits.len())?;
            traits.into_iter().try_for_each(|it| enc.def(ModuleDefId::TraitId(it)))
        })?;
        let legacy = table_in(&mut body, &legacy)?;
        let items = write_segment(&mut enc, &mut body, |enc| {
            enc.usize(scope.impls().len())?;
            scope.impls().try_for_each(|it| enc.impl_id(it))?;
            enc.usize(scope.builtin_derive_impls().len())?;
            scope.builtin_derive_impls().try_for_each(|it| enc.builtin_derive_impl(it))?;
            let declared: Vec<ModuleDefId> = (scope.declarations())
                .filter(|it| matches!(it, ModuleDefId::TraitId(_)))
                .collect(); // alloc: encoding, as above
            enc.usize(declared.len())?;
            declared.into_iter().try_for_each(|it| enc.def(it))
        })?;
        rows.push(Row {
            scope: whole,
            names,
            traits,
            legacy,
            items,
            entries: entries_run,
            textual: textual_run,
        });
    }

    // The files every segment names, then the segments: `Enc::finish` writes the first before
    // the second and refuses bytes that name a place of one checkout, the slots among them.
    enc.calls.clear();
    enc.contexts.clear();
    let body_len = body.len();
    enc.out = body;
    let named = enc.finish()?;
    let files_len = named.len() - body_len;

    let rows_at = HEADER;
    let files_at = rows_at + rows.len() * ROW;
    let base = files_at + files_len;
    let total = u32::try_from(files_at + named.len()).map_err(|_| "pages over 4 GiB")?;
    // Every place so far is from the body's start: from the record's.
    let base = base as u32;
    let moved = |(at, len): (u32, u32)| (at + base, len);
    let header = Header {
        modules: rows.len() as u32,
        files: (files_at as u32, files_len as u32),
        head: moved(head),
        tail: moved(tail),
        rows_at: rows_at as u32,
        total,
    };
    // alloc: the record, once per crate per write of its pages
    let mut out = Vec::with_capacity(total as usize);
    header.write(&mut out);
    debug_assert_eq!(out.len(), HEADER);
    for row in &rows {
        row.moved(base).write(&mut out);
    }
    debug_assert_eq!(out.len(), files_at);
    // The slots' own places, in the body: each entry's, moved as the rows' are.
    let mut named = named;
    for table in rows.iter().flat_map(|row| [row.names, row.legacy]) {
        let slots_at = files_len + table.at as usize;
        for slot in 0..table.slots as usize {
            let at = slots_at + slot * SLOT;
            if u64_at(&named, at) == 0 {
                continue;
            }
            let moved = u32_at(&named, at + 8) + base;
            named[at + 8..at + 12].copy_from_slice(&moved.to_le_bytes());
        }
    }
    out.extend_from_slice(&named);
    debug_assert_eq!(out.len(), total as usize);
    Ok(out)
}

impl Enc<'_> {
    /// The head segment: the crate's data, its module tree and its preludes.
    fn pages_head(&mut self, it: &DefMap, local: &LocalDefMap) -> R {
        let DefMap {
            krate,
            block,
            root,
            modules: _,
            prelude,
            macro_use_prelude,
            derive_helpers_in_scope: _,
            diagnostics: _,
            diagnostics_by_module: _,
            records: _,
            data,
            session_macros,
            names: _,
        } = it;
        let db = self.db;
        if *session_macros {
            // alloc: error path
            return Err("compilation-local macro state".to_owned());
        }
        if block.is_some() {
            // alloc: error path
            return Err("a def map of a block".to_owned());
        }
        let DefMapCrateData {
            exported_derives,
            fn_proc_macro_mapping,
            fn_proc_macro_mapping_back,
            registered_tools,
            unstable_features,
            rustc_coherence_is_core,
            no_core,
            no_std,
            proc_macro,
            arbitrary_ints,
            handles,
            views,
            pattern_types,
            early_drop,
            edition,
            recursion_limit,
        } = &**data;
        if fn_proc_macro_mapping.len() != fn_proc_macro_mapping_back.len() {
            // alloc: error path
            return Err("procedural macros of more than one function".to_owned());
        }
        self.edition(*edition);
        self.bool(*proc_macro);
        self.bool(*rustc_coherence_is_core);
        self.bool(*no_core);
        self.bool(*no_std);
        self.bool(*arbitrary_ints);
        self.bool(*handles);
        self.bool(*views);
        self.bool(*pattern_types);
        self.bool(*early_drop);
        self.option(*recursion_limit, |enc, it| {
            enc.u32(it);
            Ok(())
        })?;
        self.usize(registered_tools.len())?;
        for it in registered_tools {
            self.symbol(it)?;
        }
        // alloc: encoding, once per crate per write of its pages (not on a build's path)
        let mut features: Vec<intern::Symbol> = unstable_features.iter().collect();
        features.sort_unstable_by(intern::Symbol::cmp_order);
        self.usize(features.len())?;
        for it in &features {
            self.symbol(it)?;
        }

        // The modules first, so that everything after can name any of them.
        let table = it.table(db);
        // What `crate_shadows_primitive_type` asks of every scope of the crate, answered once
        // here: read from pages, that question must not decode each module's scope.
        self.bool(table.iter().any(|(_, data)| shadows_primitive_in(data.scope(db))));
        if table.get_index(0).map(|it| *it.0) != Some(*root) {
            // alloc: error path
            return Err("a def map whose root is not its first module".to_owned());
        }
        self.usize(table.len())?;
        let index: HashMap<ModuleId, u32, Global> =
            // alloc: encoding, as above
            table.keys().enumerate().map(|(i, &it)| (it, i as u32)).collect();
        for (id, data) in table.iter() {
            if id.krate(db) != *krate {
                // alloc: error path
                return Err("a module of another crate in a def map".to_owned());
            }
            match data.parent {
                None => self.u8(0),
                Some(parent) => {
                    self.u8(1);
                    self.u32(*index.get(&parent).ok_or("a parent of another def map")?);
                }
            }
            let name = id.name(db);
            self.option(name.as_ref(), |enc, it| enc.name(it))?;
        }
        for (_, data) in table.iter() {
            self.origin(data.origin)?;
            self.visibility(data.visibility)?;
            let children = data.children(db);
            self.usize(children.len())?;
            for (name, &child) in children {
                self.name(name)?;
                self.u32(*index.get(&child).ok_or("a child of another def map")?);
            }
        }

        self.option(*prelude, |enc, (module, use_)| {
            enc.module(module)?;
            enc.option(use_, Self::use_id)
        })?;
        // alloc: encoding, as above
        let mut macro_use: Vec<_> = macro_use_prelude.iter().collect();
        macro_use.sort_unstable_by(|a, b| a.0.symbol().cmp_order(b.0.symbol()));
        self.usize(macro_use.len())?;
        for (name, &(id, extern_crate)) in macro_use {
            self.name(name)?;
            self.macro_id(id)?;
            self.option(extern_crate, Self::extern_crate)?;
        }
        let exported = self.sorted(exported_derives.iter(), |enc, k| enc.macro_id(**k))?;
        self.usize(exported.len())?;
        for (&k, names) in exported {
            self.macro_id(k)?;
            self.usize(names.len())?;
            for it in names {
                self.name(it)?;
            }
        }
        // Each function of a crate of procedural macros that is one, and the other way about.
        let functions =
            self.sorted(fn_proc_macro_mapping.iter(), |enc, k| enc.def((**k).into()))?;
        self.usize(functions.len())?;
        for (&function, &id) in functions {
            self.def(function.into())?;
            self.macro_id(id.into())?;
        }
        self.local(local)
    }

    /// The tail segment: what the def map recorded that no resolution reads.
    fn pages_tail(&mut self, it: &DefMap) -> R {
        let crate::nameres::Records { names, diagnostics, derive_helpers_in_scope, .. } =
            it.records(self.db);
        let helpers = self.sorted(derive_helpers_in_scope.iter(), |enc, k| enc.ast_id(**k))?;
        self.usize(helpers.len())?;
        for (&k, helpers) in helpers {
            self.ast_id(k)?;
            self.usize(helpers.len())?;
            for (name, id, call) in helpers {
                self.name(name)?;
                self.macro_id(*id)?;
                match *call {
                    Either::Left(call) => {
                        self.u8(0);
                        self.call(call)?;
                    }
                    Either::Right(it) => {
                        self.u8(1);
                        self.builtin_derive_impl(it)?;
                    }
                }
            }
        }
        self.name_records(names)?;
        self.usize(diagnostics.len())?;
        for it in diagnostics {
            self.diagnostic(it)?;
        }
        Ok(())
    }

    /// What one name means: its item in each namespace, as [`Enc::scope`] writes one.
    fn per_ns(&mut self, it: PerNs) -> R {
        let PerNs { types, values, macros } = it;
        self.option(types, |enc, Item { def, vis, import }| {
            enc.def(def)?;
            enc.visibility(vis)?;
            enc.option(import, Self::import_or_extern_crate)
        })?;
        self.option(values, |enc, Item { def, vis, import }| {
            enc.def(def)?;
            enc.visibility(vis)?;
            enc.option(import, Self::import_or_glob)
        })?;
        self.option(macros, |enc, Item { def, vis, import }| {
            enc.macro_id(def)?;
            enc.visibility(vis)?;
            enc.option(import, Self::import_or_extern_crate)
        })
    }
}

impl Dec<'_> {
    /// Reads [`Enc::pages_head`]: the head, its rows, tail and files still to be given.
    fn pages_head(&mut self) -> R<Head> {
        let db = self.db;
        let krate = self.krate;
        let edition = self.edition()?;
        let mut data = DefMapCrateData::new(edition);
        data.proc_macro = self.bool()?;
        data.rustc_coherence_is_core = self.bool()?;
        data.no_core = self.bool()?;
        data.no_std = self.bool()?;
        data.arbitrary_ints = self.bool()?;
        data.handles = self.bool()?;
        data.views = self.bool()?;
        data.pattern_types = self.bool()?;
        data.early_drop = self.bool()?;
        data.recursion_limit = self.option(Self::u32)?;
        data.registered_tools = self.many(Self::symbol)?;
        let mut features = UnstableFeatures::default();
        for it in self.many(Self::symbol)? {
            features.enable(it);
        }
        data.unstable_features = features;
        let shadows_primitive = self.bool()?;

        // The modules, each with the id it has when its crate is resolved: the root as
        // `DefMap::empty` makes it, the others as the collector does, each in its parent.
        let (n, room) = self.len_and_room()?;
        // alloc: the head's own storage, once per crate read from pages
        let mut skeleton = Vec::with_capacity(room);
        for _ in 0..n {
            let parent = self.option(Self::u32)?;
            let name = self.option(Self::name)?;
            skeleton.push((parent, name));
        }
        if skeleton.is_empty() {
            // alloc: error path
            return Err("a def map with no module".to_owned());
        }
        self.own.to_mut().push(crate::nameres::facts::queries::crate_root(db, krate));
        for (parent, name) in skeleton.iter().skip(1) {
            let parent = parent.ok_or("a module with no parent")? as usize;
            let parent = *self.own.get(parent).ok_or("a module before its parent")?;
            let name = (*name).ok_or("a module with no name")?;
            let module =
                crate::nameres::facts::queries::child_module(db, krate, None, parent, name);
            self.own.to_mut().push(module);
        }
        // alloc: the head's own storage, as above
        let mut modules = Vec::with_capacity(skeleton.len());
        for (parent, _) in &skeleton {
            let origin = self.origin()?;
            let visibility = self.visibility()?;
            let (n, room) = self.len_and_room()?;
            let mut children = FxIndexMap::with_capacity_and_hasher(room, Default::default());
            for _ in 0..n {
                let name = self.name()?;
                let child = self.len()?;
                children.insert(name, *self.own.get(child).ok_or("a child not read")?);
            }
            let parent = match parent {
                None => None,
                Some(it) => Some(*self.own.get(*it as usize).ok_or("a parent not read")?),
            };
            modules.push(HeadModule { origin, visibility, parent, children });
        }

        let prelude = self.option(|dec| {
            let module = dec.module()?;
            Ok((module, dec.option(Self::use_id)?))
        })?;
        let macro_use_prelude = self
            .many(|dec| {
                let name = dec.name()?;
                let id = dec.macro_id()?;
                Ok((name, (id, dec.option(Self::extern_crate)?)))
            })?
            .into_iter()
            // alloc: the head's own storage, as above
            .collect();
        let exported = self.many(|dec| {
            let id = dec.macro_id()?;
            Ok((id, dec.many(Self::name)?.into_boxed_slice()))
        })?;
        let functions = self.many(|dec| {
            let ModuleDefId::FunctionId(function) = dec.def()? else {
                // alloc: error path
                return Err("a procedural macro of no function".to_owned());
            };
            let MacroId::ProcMacroId(id) = dec.macro_id()? else {
                // alloc: error path
                return Err("a function of a macro no procedural one".to_owned());
            };
            Ok((function, id))
        })?;
        // alloc: the head's own storage, as above
        data.exported_derives = exported.into_iter().collect();
        // alloc: the head's own storage, as above
        data.fn_proc_macro_mapping_back = functions.iter().map(|&(f, id)| (id, f)).collect();
        // alloc: the head's own storage, as above
        data.fn_proc_macro_mapping = functions.into_iter().collect();
        let LocalDefMap { extern_prelude } = self.local()?;
        Ok(Head {
            // alloc: none, empty until `read_head` gives the rows
            rows: Box::default(),
            tail: (0, 0),
            // alloc: none, empty until `read_head` gives the files
            files: Box::default(),
            // alloc: the head's own storage, as above
            own: self.own.to_vec().into_boxed_slice(),
            // alloc: the crate's data, once per crate read from pages
            data: TArc::new(data),
            modules: modules.into_boxed_slice(),
            shadows_primitive,
            prelude,
            macro_use_prelude,
            extern_prelude,
        })
    }

    /// Reads [`Enc::pages_tail`].
    fn pages_tail(&mut self) -> R<OwnedRecords> {
        let derive_helpers_in_scope = self
            .many(|dec| {
                let ast_id = dec.ast_id()?;
                let helpers = dec.many(|dec| {
                    let name = dec.name()?;
                    let id = dec.macro_id()?;
                    let call = match dec.u8()? {
                        0 => Either::Left(dec.call()?),
                        1 => Either::Right(dec.builtin_derive_impl()?),
                        // alloc: error path
                        it => return Err(format!("derive helper {it}")),
                    };
                    Ok((name, id, call))
                })?;
                Ok((ast_id, helpers))
            })?
            .into_iter()
            // alloc: the records' own storage, once per crate whose records are asked for
            .collect();
        let names = self.name_records()?;
        let diagnostics = self.many(Self::diagnostic)?;
        Ok(OwnedRecords {
            names,
            diagnostics_by_module: DiagnosticsByModule::of(&diagnostics),
            diagnostics,
            derive_helpers_in_scope,
        })
    }

    /// Reads [`Enc::per_ns`].
    fn per_ns(&mut self) -> R<PerNs> {
        let types = self.option(|dec| {
            let def = dec.def()?;
            let vis = dec.visibility()?;
            let import = dec.option(Self::import_or_extern_crate)?;
            Ok(Item { def, vis, import })
        })?;
        let values = self.option(|dec| {
            let def = dec.def()?;
            let vis = dec.visibility()?;
            let import = dec.option(Self::import_or_glob)?;
            Ok(Item { def, vis, import })
        })?;
        let macros = self.option(|dec| {
            let def = dec.macro_id()?;
            let vis = dec.visibility()?;
            let import = dec.option(Self::import_or_extern_crate)?;
            Ok(Item { def, vis, import })
        })?;
        Ok(PerNs { types, values, macros })
    }
}

#[cfg(test)]
mod tests;
