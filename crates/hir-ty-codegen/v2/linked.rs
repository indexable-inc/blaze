//! What the link takes of a build besides its code: the native libraries a crate's
//! `#[link(..)]`s name ([`HirFrontend::linked`]), and the span tables the code's positions
//! name ([`HirFrontend::link_inputs`]), kept between links so that a link after an edit makes
//! the tables the edit changed (docs/v2/plan/design/patch-in-place.md).

#[cfg(test)]
mod tests;

use super::{HashSource, Hashes, Kind, frontend::HirFrontend};
use base_db::{Crate, Db};
use blaze_codegen::{
    BuiltId, CodegenError, CompiledFunction, Init, InstanceKey, NativeLib, NativeLibKind, Static,
    Target, driver::Built,
};
use blaze_core::lock::Lock;
use blaze_format::cursor::Put;
use blaze_hash::{HashMap, HashSet};
use blaze_ids::ContentId;
use blaze_items::{ItemId, ItemKey, key::Resolved};
use hir_def::{
    AttrDefId, DefWithBodyId, ExpressionStoreOwnerId, FunctionId, HasModule as _, ItemContainerId,
    Lookup, ModuleDefId,
    attrs::{AttrFlags, NativeLibKind as Written},
    expr_store::Body as BodyStore,
    nameres::crate_def_map,
    signatures::{FunctionSignature, StaticSignature},
};
use hir_ty_mir::mir::MirSpan;
use hir_ty_share::code::{self, TABLES};
use std::{
    alloc::Global,
    sync::atomic::{AtomicU64, Ordering},
};

/// One span table: its anchor, the id of its encoding, and the encoding as the link reads it
/// (`blaze_link::position::SpanTable`).
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a test's copy of a snapshot
pub struct Table {
    pub anchor: ContentId,
    /// The hash of `bytes`: what the link keys the table's content by.
    pub id: ContentId,
    pub bytes: Vec<u8>,
}

/// What [`HirFrontend::link_inputs`] gives the link: the span tables the code of a build
/// names, as a change of the tables the link before it was given.
///
/// The tables of a program are a snapshot, named by `serial`: a count of this process, another
/// for every change of any table. With a `base`, `tables` and `removed` are everything that
/// differs from the snapshot `base`, so a link that holds that one applies them. With none,
/// `tables` is every table. A link that holds neither this snapshot nor its base asks for
/// every table ([`HirFrontend::span_tables`]).
#[derive(PartialEq, Eq, Debug, Default)]
pub struct LinkInputs {
    pub serial: u64,
    pub base: Option<u64>,
    /// The tables new or other than in `base` (every table, with no base), sorted by anchor.
    pub tables: Vec<Table>,
    /// The anchors the code no longer names, sorted; empty with no base.
    pub removed: Vec<ContentId>,
    /// What making this cost.
    pub work: InputsWork,
}

/// What making a link's inputs did, counted: after an edit each count follows the edit, but
/// `verified`, which follows the workspace's bodies the code names.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct InputsWork {
    /// Items resolved to what has span tables.
    pub items: usize,
    /// Items of the build looked up among those already resolved: all of them, only when
    /// the code names a table of an item new to the build.
    pub scanned: usize,
    /// Functions and statics whose relocations and lines were read for the anchors they name.
    pub functions: usize,
    /// Bodies and declarations whose positions were asked of the database again: those not
    /// of library inputs alone, once a view.
    pub verified: usize,
    /// Tables encoded, and their bytes.
    pub made: usize,
    pub bytes: usize,
    /// Tables held from the link before and not looked at.
    pub kept: usize,
    /// Why nothing kept was used and every table was made, when that is so.
    pub whole: Option<&'static str>,
}

/// What the running code of a live image assumes of one item of a workspace crate, which no
/// patch can change under it ([`HirFrontend::live_assumed`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Assumed {
    /// A function's interface hash: its signature and what its body leaks into it (an
    /// `impl Trait` return's hidden type), which its callers' code is built against.
    Signature(ContentId),
    /// A type's layout hash: how a value made before an edit is laid out.
    Layout(ContentId),
    /// An item blaze can't hash, by why: what code assumes of it is not known, so it is the
    /// same only for as long as the reason is.
    Unhashable(ContentId),
}

/// The domain of [`Assumed::Unhashable`]'s hash of a reason.
const UNHASHABLE_DOMAIN: &str = "blaze v2 live assumed unhashable 1";

/// The start of a span table's encoding (`blaze_link::position`'s `MAGIC`).
const SPAN_TABLE_MAGIC: [u8; 8] = *b"BZSPANS1";

impl HirFrontend<'_> {
    /// The native libraries `krate`'s `#[link(..)]`s link it against, as rustc's
    /// `rustc_metadata::native_libs` collects them: each `extern` block's in the order
    /// written, the blocks in the order of their modules, without those whose `cfg(..)` the
    /// crate leaves out. A form blaze can't link is an error naming it.
    pub fn linked(&self, krate: Crate) -> Result<Vec<NativeLib>, CodegenError> {
        self.with(|db| linked(db, krate)).map_err(error)
    }

    /// What the code of a live image built from `krate` assumes of its items, by key, sorted:
    /// each function's signature and each type's layout ([`Assumed`]). Only a workspace
    /// crate's code is patched (`Described::editable`), so another crate assumes nothing: an
    /// edit never reaches it. A later link whose assumptions differ can't be patched into a
    /// program running the earlier one.
    pub fn live_assumed(&self, krate: Crate) -> Vec<(ItemKey, Assumed)> {
        let _span = tracing::info_span!(target: "blaze", "live assumed").entered();
        self.with(|db| {
            if !krate.data(db).origin.is_local() {
                return Vec::new(); // alloc: none, empty
            }
            let columns = blaze_items::crate_hashes(db, krate);
            // alloc: per live link, a row per function and type of a workspace crate
            let mut rows = Vec::with_capacity(columns.len() + columns.unhashable().len());
            for (at, key) in columns.keys().iter().enumerate() {
                let hashes = columns.row(at);
                match (hashes.kind, hashes.layout) {
                    (blaze_items::ItemKind::Fn, _) => {
                        rows.push((*key, Assumed::Signature(hashes.interface.0)))
                    }
                    (_, Some(layout)) => rows.push((*key, Assumed::Layout(layout.0))),
                    _ => {}
                }
            }
            for (key, why) in columns.unhashable() {
                let why = ContentId::of(UNHASHABLE_DOMAIN, why.what.as_bytes());
                rows.push((*key, Assumed::Unhashable(why)));
            }
            rows.sort_unstable();
            rows
        })
    }

    /// The path of the item `key` names, for a message; its key when no crate of the build
    /// has it.
    pub fn item_path(&self, key: ItemKey) -> String {
        self.with(|db| match blaze_items::resolve(db, key) {
            Some(Resolved::Item(id)) => blaze_items::key::path(db, id),
            // alloc: a message's name of an item
            _ => format!("{key:?}"),
        })
    }

    /// The span tables `built`'s code names, as a change of the tables kept from the link
    /// before it ([`LinkInputs`]). What is kept is memory of this process beside the
    /// database, found by the build's id or its base's ([`Built::delta`]): a function the
    /// build did not change is not read, a body of library inputs alone is not asked again
    /// while no library input was written, and a table is encoded only when its body's
    /// positions are other than they were.
    pub fn link_inputs(&self, built: &Built) -> Result<LinkInputs, CodegenError> {
        self.with(|db| inputs(db, &self.hashes, built, false)).map_err(error)
    }

    /// Every span table `built`'s code names, by anchor, sorted, encoded: made whole, with
    /// nothing kept and nothing read from what is. What a link with no base, a test binary's
    /// table and a test's selection take, and what [`Self::link_inputs`] is checked against.
    pub fn span_tables(&self, built: &Built) -> Result<Vec<(ContentId, Vec<u8>)>, CodegenError> {
        self.with(|db| {
            let mut work = InputsWork::default();
            let (_, tables) = Kept::whole(db, &self.hashes, built, &mut work)?;
            // alloc: per call, the tables as their readers take them
            Ok(tables.into_iter().map(|it| (it.anchor, it.bytes)).collect())
        })
        .map_err(error)
    }
}

fn error(what: String) -> CodegenError {
    CodegenError {
        unit: InstanceKey(ContentId::default()),
        item: ItemKey(ContentId::default()),
        what,
        front: None,
    }
}

fn linked(db: &Db, krate: Crate) -> Result<Vec<NativeLib>, String> {
    // alloc: an error's name of the crate
    let named = || format!("crate `{}`", krate.named(db));
    // alloc: per crate linked, its libraries
    let mut libs = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for block in module.scope(db).extern_blocks() {
            // A block's `#[link(wasm_import_module = "..")]` names no library: on wasm its
            // functions are the module's imports from that module (`v2::wasm::imports`), and
            // elsewhere rustc ignores it.
            for lib in AttrFlags::native_libs(db, block).iter() {
                let name = lib.name.as_str();
                let kind = match lib.kind {
                    Written::Unspecified | Written::Dylib => NativeLibKind::Dylib,
                    Written::Static => {
                        NativeLibKind::Static { whole: lib.whole_archive, bundle: lib.bundle }
                    }
                    Written::Framework => NativeLibKind::Framework,
                    Written::RawDylib => {
                        // alloc: an error
                        return Err(format!(
                            "`#[link(name = \"{name}\", kind = \"raw-dylib\")]` in {}: blaze \
                             links no `raw-dylib` (nor its `link_ordinal` and `import_name_type`)",
                            named()
                        ));
                    }
                    Written::Other(kind) => {
                        // alloc: an error
                        return Err(format!(
                            "`#[link(name = \"{name}\", kind = \"{kind}\")]` in {}: blaze links \
                             no `{kind}` library",
                            named()
                        ));
                    }
                };
                if let Some(on) = lib.as_needed {
                    let sign = if on { '+' } else { '-' };
                    // alloc: an error
                    return Err(format!(
                        "`#[link(name = \"{name}\", modifiers = \"{sign}as-needed\")]` in {}: \
                         blaze passes the linker no `as-needed`",
                        named()
                    ));
                }
                if let Some(imported) = &lib.import_name_type {
                    // alloc: an error
                    return Err(format!(
                        "`#[link(name = \"{name}\", import_name_type = \"{imported}\")]` in {}: \
                         blaze links no `raw-dylib`, which `import_name_type` is for",
                        named()
                    ));
                }
                if name.is_empty() {
                    // alloc: an error
                    return Err(format!(
                        "`#[link(name = \"\")]` in {}: a library has a name",
                        named()
                    ));
                }
                if lib.whole_archive && !matches!(kind, NativeLibKind::Static { .. }) {
                    // alloc: an error
                    return Err(format!(
                        "`#[link(name = \"{name}\", modifiers = \"+whole-archive\")]` in {}: \
                         only a `static` library is linked whole",
                        named()
                    ));
                }
                // alloc: per library, its name for the link
                libs.push(NativeLib { name: name.to_owned(), kind, verbatim: lib.verbatim });
            }
        }
        for def in module.scope(db).declarations() {
            let (item, container, owner) = match def {
                ModuleDefId::FunctionId(it) => (
                    FunctionSignature::of(db, it).name.as_str(),
                    it.lookup(db).container,
                    AttrDefId::FunctionId(it),
                ),
                ModuleDefId::StaticId(it) => (
                    StaticSignature::of(db, it).name.as_str(),
                    it.lookup(db).container,
                    AttrDefId::StaticId(it),
                ),
                _ => continue,
            };
            if matches!(container, ItemContainerId::ExternBlockId(_))
                && AttrFlags::query(db, owner).contains(AttrFlags::HAS_LINK_ORDINAL)
            {
                // alloc: an error
                return Err(format!(
                    "`#[link_ordinal(..)]` on `{item}` in {}: blaze links no `raw-dylib`, \
                     which imports by ordinal",
                    named()
                ));
            }
        }
    }
    Ok(libs)
}

/// The domain of a [`Table::id`].
const TABLE_DOMAIN: &str = "blaze v2 link span table 1";

/// The id of a span table's encoding ([`Table::id`]).
pub fn span_table_id(bytes: &[u8]) -> ContentId {
    ContentId::of(TABLE_DOMAIN, bytes)
}

/// How many programs' link inputs a process keeps ([`KEPT`]): the least recently linked goes
/// when one more is made. Each holds hashes and counts, no table: about 300 bytes a function.
const KEPT_LINKS: usize = 4;

/// The link inputs kept, the last linked last.
// alloc: none until a link, then a few programs' link inputs for the process's life
static KEPT: Lock<Vec<Kept>> = Lock::new("codegen.linked.kept", Vec::new());

/// The next [`LinkInputs::serial`]; 0 is no snapshot's.
static SERIAL: AtomicU64 = AtomicU64::new(1);

const NOTHING_KEPT: &str = "no link inputs are kept for this build or its base: the first link \
                            of the program since the daemon started, or a link failed since";
const LIBRARY_WRITTEN: &str = "a library input was written since the last link";
const RENAMED: &str = "the build names a file otherwise than the last link did, or no longer";

/// What has span tables: a body, or a function's declaration.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Owner {
    Body(DefWithBodyId),
    Declared(FunctionId),
}

/// Whose table an anchor names: a body's, by which of its tables, or a declaration's one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Source {
    Body(DefWithBodyId, u8),
    Declared(FunctionId),
}

impl Source {
    fn owner(self) -> Owner {
        match self {
            Source::Body(owner, _) => Owner::Body(owner),
            Source::Declared(func) => Owner::Declared(func),
        }
    }
}

/// What names an anchor: a function, by key, or a static, by item and instance.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
enum Namer {
    Function(InstanceKey),
    Static(ItemKey, ContentId),
}

/// An owner whose positions are asked of the database again each link: its stamp
/// ([`stamp_of`]) as of the tables' encoding, and its tables the code names.
struct Watch {
    stamp: u64,
    tables: Vec<(ContentId, Source)>,
}

/// The link inputs of one program as of its last link: what the next link's are a change of.
///
/// The invariant: `named` counts, for each anchor, the functions and statics of the build
/// `built` that name it (`named_by`); `tables` holds exactly the anchors `named` holds, each
/// with the id of its table as encoded in the view `view`; `watched` holds every owner of a
/// table that is not of library inputs alone, with its stamp as of `view`.
struct Kept {
    database: base_db::Nonce,
    /// The tests' hashes ([`HashSource`]'s `ById`), whose anchors are others.
    by_id: bool,
    built: BuiltId,
    serial: u64,
    /// The database's view the tables are of, and the view its last library write opened.
    view: u64,
    durable_view: u64,
    /// The items resolved, and of those that resolve to nothing how many and the least.
    items: HashSet<ItemKey, Global>,
    unresolved: Option<(usize, ItemKey)>,
    /// Each anchor a resolved item's body or declaration has.
    sources: HashMap<ContentId, Source, Global>,
    named_by: HashMap<Namer, Box<[ContentId]>, Global>,
    named: HashMap<ContentId, u32, Global>,
    tables: HashMap<ContentId, ContentId, Global>,
    watched: HashMap<Owner, Watch, Global>,
    /// The owners seen durable ([`stamp_of`]) in `durable_view`: not asked again while it is
    /// the view.
    durable: HashSet<Owner, Global>,
    /// What the build calls each file a table names: the encodings hold the names, so a
    /// link that names one otherwise makes every table again.
    files: HashMap<base_db::FileId, String, Global>,
}

/// The anchors `targets` name.
fn positions_in(targets: &[Target]) -> impl Iterator<Item = ContentId> + '_ {
    targets.iter().filter_map(|it| match it {
        Target::Position(span) => Some(span.anchor),
        _ => None,
    })
}

fn sorted_once(mut anchors: Vec<ContentId>) -> Box<[ContentId]> {
    anchors.sort_unstable();
    anchors.dedup();
    anchors.into_boxed_slice() // alloc: the caller's list, at its exact size
}

/// Every anchor `function` names: in a position of its code or its data, or a line.
fn function_anchors(function: &CompiledFunction) -> Box<[ContentId]> {
    let data = function.data.iter().flat_map(|it| positions_in(it.relocs.targets()));
    let lines = function.lines.iter().map(|it| it.span.anchor);
    // alloc: per function read, the anchors it names, kept until it changes
    sorted_once(positions_in(function.relocs.targets()).chain(data).chain(lines).collect())
}

/// Every anchor `value` names, in its bytes or its data.
fn static_anchors(value: &Static) -> Box<[ContentId]> {
    let init: &[Target] = match &value.init {
        Init::Bytes { relocs, .. } => relocs.targets(),
        Init::Zeroed { .. } => &[],
    };
    let data = value.data.iter().flat_map(|it| positions_in(it.relocs.targets()));
    // alloc: per static read, the anchors it names, kept until it changes
    sorted_once(positions_in(init).chain(data).collect())
}

fn by_id(hashes: &HashSource) -> bool {
    match hashes {
        HashSource::Items(_) => false,
        #[cfg(test)]
        HashSource::ById(_) => true,
    }
}

/// A count that is another each time `owner`'s positions are found again
/// ([`positions_stamp`], [`declared_stamp`]), and whether they are of library inputs alone.
fn stamp_of(db: &Db, owner: Owner) -> (u64, bool) {
    match owner {
        Owner::Body(it) => (positions_stamp(db, it), positions_stamp::durable(db, it)),
        Owner::Declared(it) => (declared_stamp(db, it), declared_stamp::durable(db, it)),
    }
}

/// The table of `source`, encoded.
fn encoded(
    db: &Db,
    hashes: &HashSource,
    files: &mut HashMap<base_db::FileId, String, Global>,
    source: Source,
) -> Result<Vec<u8>, String> {
    match source {
        Source::Body(owner, table) => {
            let body = positions(db, owner).as_ref().map_err(Clone::clone)?;
            let table = table as usize;
            let rows = &body.rows[body.starts[table] as usize..body.starts[table + 1] as usize];
            encode(db, hashes, files, rows)
        }
        Source::Declared(func) => {
            let at = (*crate::declaration_location(db, func))
                // alloc: an error
                .ok_or_else(|| format!("{func:?} is declared nowhere a position can name"))?;
            encode(db, hashes, files, &[at])
        }
    }
}

impl Kept {
    fn empty(db: &Db, hashes: &HashSource, built: BuiltId) -> Kept {
        let (database, view) = db.nonce_and_revision();
        let durable_view = blaze_query::db::Database::storage(db).durable_view().as_u64();
        Kept {
            database,
            by_id: by_id(hashes),
            built,
            serial: 0,
            view: view.as_u64(),
            durable_view,
            // alloc: per program kept, its link inputs' index, which lives as long as it
            items: HashSet::new_in(Global),
            unresolved: None,
            sources: HashMap::new_in(Global), // alloc: as `items`
            named_by: HashMap::new_in(Global), // alloc: as `items`
            named: HashMap::new_in(Global), // alloc: as `items`
            tables: HashMap::new_in(Global), // alloc: as `items`
            watched: HashMap::new_in(Global), // alloc: as `items`
            durable: HashSet::new_in(Global), // alloc: as `items`
            files: HashMap::new_in(Global), // alloc: as `items`
        }
    }

    /// The link inputs of `built` made with nothing kept: every item resolved, every
    /// function and static read, every table the code names encoded.
    fn whole(
        db: &Db,
        hashes: &HashSource,
        built: &Built,
        work: &mut InputsWork,
    ) -> Result<(Kept, Vec<Table>), String> {
        let mut kept = Kept::empty(db, hashes, built.id);
        kept.index(db, hashes, &built.items, work)?;
        // alloc: per link made whole, the anchors the code names
        let (mut wanted, mut dropped) = (Vec::new(), Vec::new());
        for function in &built.functions {
            work.functions += 1;
            let anchors = function_anchors(function);
            kept.name(Namer::Function(function.key), anchors, &mut wanted, &mut dropped);
        }
        for value in &built.statics {
            work.functions += 1;
            let namer = Namer::Static(value.item, value.instance);
            kept.name(namer, static_anchors(value), &mut wanted, &mut dropped);
        }
        wanted.retain(|it| kept.named.contains_key(it));
        wanted.sort_unstable();
        wanted.dedup();
        let tables = kept.make(db, hashes, Some(built), &wanted, work)?;
        Ok((kept, tables))
    }

    /// Resolves each of `items` not resolved yet to what has span tables, and keeps the
    /// anchors of each. An `asm!` function's and the allocator shim's items are of no crate
    /// of the database: counted, for an error.
    fn index(
        &mut self,
        db: &Db,
        hashes: &HashSource,
        items: &[ItemKey],
        work: &mut InputsWork,
    ) -> Result<(), String> {
        for &item in items {
            work.scanned += 1;
            if !self.items.insert(item) {
                continue;
            }
            work.items += 1;
            let owner = match blaze_items::resolve(db, item) {
                None => {
                    let (count, least) = self.unresolved.get_or_insert((0, item));
                    *count += 1;
                    *least = (*least).min(item);
                    continue;
                }
                Some(Resolved::Item(ItemId::Function(func))) => {
                    let anchor = hashes.declaration(db, func)?.anchor;
                    self.sources.insert(anchor, Source::Declared(func));
                    DefWithBodyId::FunctionId(func)
                }
                Some(Resolved::Item(ItemId::Static(it))) => DefWithBodyId::StaticId(it),
                Some(Resolved::Item(ItemId::Const(it))) => DefWithBodyId::ConstId(it),
                Some(Resolved::Variant(it)) => DefWithBodyId::VariantId(it),
                Some(Resolved::Closure { store: ExpressionStoreOwnerId::Body(owner), .. }) => owner,
                // No body: drop glue's ADT, a shim's trait, an item of the allocator shim.
                Some(_) => continue,
            };
            // Asked for each item, never once an owner: an item moved has another key and
            // its body other anchors under the same id.
            for (table, anchor) in hashes.anchors(db, owner)?.into_iter().enumerate() {
                self.sources.insert(anchor, Source::Body(owner, table as u8));
            }
        }
        Ok(())
    }

    /// `namer` no longer names what it did: each anchor it was the last to name goes into
    /// `dropped`.
    fn unname(&mut self, namer: &Namer, dropped: &mut Vec<ContentId>) {
        let Some(anchors) = self.named_by.remove(namer) else { return };
        for anchor in anchors.iter() {
            let left = match self.named.get_mut(anchor) {
                Some(count) => {
                    *count -= 1;
                    *count
                }
                None => continue,
            };
            if left == 0 {
                self.named.remove(anchor);
                dropped.push(*anchor);
            }
        }
    }

    /// `namer` names `anchors` and nothing else: each anchor it is the first to name goes
    /// into `wanted`, each it was the last to name into `dropped`.
    fn name(
        &mut self,
        namer: Namer,
        anchors: Box<[ContentId]>,
        wanted: &mut Vec<ContentId>,
        dropped: &mut Vec<ContentId>,
    ) {
        self.unname(&namer, dropped);
        if anchors.is_empty() {
            return;
        }
        for anchor in anchors.iter() {
            let count = self.named.entry(*anchor).or_insert(0);
            *count += 1;
            if *count == 1 {
                wanted.push(*anchor);
            }
        }
        self.named_by.insert(namer, anchors);
    }

    /// Encodes the table of each of `wanted` (sorted, each once) and keeps its id, watching
    /// its owner where its positions are not of library inputs alone. An anchor of no table
    /// found is an error naming it and what names it.
    fn make(
        &mut self,
        db: &Db,
        hashes: &HashSource,
        built: Option<&Built>,
        wanted: &[ContentId],
        work: &mut InputsWork,
    ) -> Result<Vec<Table>, String> {
        if let Some(least) = wanted.iter().find(|it| !self.sources.contains_key(*it)) {
            let missing = wanted.iter().filter(|it| !self.sources.contains_key(*it)).count();
            return Err(self.unfound(*least, missing, built));
        }
        // alloc: per link, a table for each anchor newly named
        let mut tables = Vec::with_capacity(wanted.len());
        for &anchor in wanted {
            let source = self.sources[&anchor];
            let bytes = encoded(db, hashes, &mut self.files, source)?;
            let id = span_table_id(&bytes);
            work.made += 1;
            work.bytes += bytes.len();
            self.tables.insert(anchor, id);
            let owner = source.owner();
            if let Some(watch) = self.watched.get_mut(&owner) {
                if !watch.tables.iter().any(|it| it.0 == anchor) {
                    watch.tables.push((anchor, source));
                }
            } else if !self.durable.contains(&owner) {
                match stamp_of(db, owner) {
                    (_, true) => {
                        self.durable.insert(owner);
                    }
                    (stamp, false) => {
                        // alloc: per owner watched, its tables the code names, a few
                        let tables = vec![(anchor, source)];
                        self.watched.insert(owner, Watch { stamp, tables });
                    }
                }
            }
            tables.push(Table { anchor, id, bytes });
        }
        Ok(tables)
    }

    /// The error of an anchor the code names that is of no table: `anchor` is the least of
    /// the `missing` such.
    fn unfound(&self, anchor: ContentId, missing: usize, built: Option<&Built>) -> String {
        let namer = self.named_by.iter().filter(|it| it.1.contains(&anchor)).map(|it| *it.0).min();
        let by = match (namer, built) {
            (Some(Namer::Function(key)), Some(built)) => {
                let function = built.functions.iter().find(|it| it.key == key);
                let what = match function {
                    Some(it) if positions_in(it.relocs.targets()).any(|it| it == anchor) => {
                        "a position in"
                    }
                    Some(it) if it.lines.iter().any(|it| it.span.anchor == anchor) => {
                        "a line row of"
                    }
                    _ => "the data of",
                };
                let name = built.debug_names.iter().find(|it| it.0 == key);
                // alloc: an error
                let name = name.map_or_else(String::new, |it| format!(" `{}`", it.1));
                // alloc: an error
                format!("{what} the function {key:?}{name}")
            }
            // alloc: an error
            (Some(Namer::Function(key)), None) => format!("the function {key:?}"),
            // alloc: an error
            (Some(Namer::Static(item, _)), _) => format!("the static {item:?}"),
            // alloc: an error
            (None, _) => "the caller".to_owned(),
        };
        let items = built.map_or(self.items.len(), |it| it.items.len());
        let unresolved = match self.unresolved {
            // alloc: an error
            Some((count, least)) => format!(
                "; {count} of the build's {items} items resolve to no item of the database, among \
                 them {least:?}"
            ),
            None => String::new(),
        };
        // alloc: an error
        format!(
            "{missing} span tables named by the code are of no body or declaration of the build's \
             {items} items, among them {anchor:?}, named by {by}{unresolved}"
        )
    }

    /// Brings what is kept, of the build `self.built` in the view `self.view`, to `built` in
    /// the database's view now: the tables new or changed, and the anchors gone. `Ok(Err)`
    /// says why what is kept can't be brought there, and every table is to be made; what is
    /// kept is then of no use. `plant` is the bug of
    /// `the_oracle_catches_a_moved_body_not_encoded_again`: the watched owners are not asked.
    fn advance(
        &mut self,
        db: &Db,
        hashes: &HashSource,
        built: &Built,
        work: &mut InputsWork,
        plant: bool,
    ) -> Result<Result<(Vec<Table>, Vec<ContentId>), &'static str>, String> {
        let (_, view) = db.nonce_and_revision();
        let durable_view = blaze_query::db::Database::storage(db).durable_view().as_u64();
        if durable_view != self.durable_view {
            return Ok(Err(LIBRARY_WRITTEN));
        }
        // alloc: per link, the anchors an edit names anew or stops naming, a few
        let (mut wanted, mut dropped) = (Vec::new(), Vec::new());
        if self.built != built.id {
            let functions = &built.delta.functions;
            for key in &functions.removed {
                self.unname(&Namer::Function(*key), &mut dropped);
            }
            for key in functions.added.iter().chain(&functions.changed) {
                // One merged away by dedupe has no code of its own, and names nothing.
                match built.functions.binary_search_by(|it| it.key.cmp(key)) {
                    Ok(at) => {
                        work.functions += 1;
                        let anchors = function_anchors(&built.functions[at]);
                        self.name(Namer::Function(*key), anchors, &mut wanted, &mut dropped);
                    }
                    Err(_) => self.unname(&Namer::Function(*key), &mut dropped),
                }
            }
            let statics = &built.delta.statics;
            for (item, instance) in &statics.removed {
                self.unname(&Namer::Static(*item, *instance), &mut dropped);
            }
            for id in statics.added.iter().chain(&statics.changed) {
                let namer = Namer::Static(id.0, id.1);
                match built.statics.binary_search_by(|it| (it.item, it.instance).cmp(id)) {
                    Ok(at) => {
                        work.functions += 1;
                        let anchors = static_anchors(&built.statics[at]);
                        self.name(namer, anchors, &mut wanted, &mut dropped);
                    }
                    Err(_) => self.unname(&namer, &mut dropped),
                }
            }
            self.built = built.id;
        }
        // alloc: per link, the anchors gone, a few
        let mut removed = Vec::new();
        for anchor in dropped {
            if !self.named.contains_key(&anchor) && self.tables.remove(&anchor).is_some() {
                removed.push(anchor);
            }
        }
        removed.sort_unstable();
        removed.dedup();
        // The tables kept, as the database has their positions now.
        let mut tables = match view.as_u64() == self.view {
            true => Vec::new(), // alloc: none, empty
            false => match self.verify(db, hashes, work, plant)? {
                Ok(changed) => changed,
                Err(why) => return Ok(Err(why)),
            },
        };
        self.view = view.as_u64();
        // The tables named anew.
        wanted.retain(|it| self.named.contains_key(it) && !self.tables.contains_key(it));
        wanted.sort_unstable();
        wanted.dedup();
        if wanted.iter().any(|it| !self.sources.contains_key(it)) {
            // Of an item new to the build: the build's items say which, and only as a whole
            // list, so each is looked up among those resolved.
            self.index(db, hashes, &built.items, work)?;
        }
        tables.extend(self.make(db, hashes, Some(built), &wanted, work)?);
        tables.sort_unstable_by_key(|it| it.anchor);
        Ok(Ok((tables, removed)))
    }

    /// Asks the database again for the positions of each watched owner, and encodes again
    /// the kept tables of each whose positions were found again: those whose encoding is
    /// another. An owner none of whose tables is kept is no longer watched, and not asked.
    fn verify(
        &mut self,
        db: &Db,
        hashes: &HashSource,
        work: &mut InputsWork,
        plant: bool,
    ) -> Result<Result<Vec<Table>, &'static str>, String> {
        // A file gone from the build has no name: its tables are of no use either.
        for (file, name) in &self.files {
            if hashes.file_name(db, *file).ok().as_ref() != Some(name) {
                return Ok(Err(RENAMED));
            }
        }
        // The tables gone are forgotten, and an owner left with none is not watched, before
        // the database is asked: an owner whose item an edit removed has none, and its id is
        // no longer one to ask about.
        let tables = &self.tables;
        self.watched.retain(|_, watch| {
            watch.tables.retain(|it| tables.contains_key(&it.0));
            !watch.tables.is_empty()
        });
        // alloc: per link, the owners an edit moved, a few
        let mut moved: Vec<Owner> = Vec::new();
        if !plant {
            for (owner, watch) in self.watched.iter_mut() {
                work.verified += 1;
                let (now, _) = stamp_of(db, *owner);
                if now != watch.stamp {
                    watch.stamp = now;
                    moved.push(*owner);
                }
            }
        }
        // alloc: per link, the tables an edit changed, a few
        let mut changed = Vec::new();
        for owner in moved {
            // clone: per owner moved, its few tables' anchors
            let named = self.watched[&owner].tables.clone();
            for (anchor, source) in named {
                let Some(was) = self.tables.get(&anchor).copied() else { continue };
                let bytes = encoded(db, hashes, &mut self.files, source)?;
                let id = span_table_id(&bytes);
                work.made += 1;
                work.bytes += bytes.len();
                if id != was {
                    self.tables.insert(anchor, id);
                    changed.push(Table { anchor, id, bytes });
                }
            }
        }
        Ok(Ok(changed))
    }
}

/// [`HirFrontend::link_inputs`] over `db`.
fn inputs(db: &Db, hashes: &HashSource, built: &Built, plant: bool) -> Result<LinkInputs, String> {
    let (database, _) = db.nonce_and_revision();
    let tests = by_id(hashes);
    let taken = {
        let mut kept = KEPT.lock();
        let of = |it: &Kept, id: BuiltId| it.database == database && it.by_id == tests && it.built == id;
        let found = kept.iter().position(|it| of(it, built.id)).or_else(|| {
            built.delta.base.and_then(|base| kept.iter().position(|it| of(it, base)))
        });
        found.map(|at| kept.remove(at))
    };
    let mut work = InputsWork::default();
    // What was kept is taken out while it is brought forward: an error leaves none, and the
    // next link makes every table.
    let advanced = match taken {
        Some(mut kept) => match kept.advance(db, hashes, built, &mut work, plant)? {
            Ok((tables, removed)) => Ok((kept, tables, removed)),
            Err(why) => Err(why),
        },
        None => Err(NOTHING_KEPT),
    };
    let (kept, inputs) = match advanced {
        Ok((mut kept, tables, removed)) => {
            let base = kept.serial;
            if !tables.is_empty() || !removed.is_empty() {
                kept.serial = SERIAL.fetch_add(1, Ordering::Relaxed);
            }
            work.kept = kept.tables.len() - tables.len();
            let base = (kept.serial != base).then_some(base);
            let inputs = LinkInputs { serial: kept.serial, base, tables, removed, work };
            (kept, inputs)
        }
        Err(why) => {
            work = InputsWork { whole: Some(why), ..InputsWork::default() };
            let (mut kept, tables) = Kept::whole(db, hashes, built, &mut work)?;
            kept.serial = SERIAL.fetch_add(1, Ordering::Relaxed);
            // alloc: none, empty
            let inputs = LinkInputs { serial: kept.serial, base: None, tables, removed: Vec::new(), work };
            (kept, inputs)
        }
    };
    let mut all = KEPT.lock();
    if all.len() >= KEPT_LINKS {
        all.remove(0);
    }
    all.push(kept);
    Ok(inputs)
}

/// The span tables of `items` that `named` lists, encoded as the linker's `SpanTable::decode`
/// reads them: for code loaded in process with no [`Built`] to read the anchors from. An
/// anchor of no table found is an error naming it.
pub fn span_tables_of(
    db: &Db,
    hashes: &HashSource,
    items: &[ItemKey],
    named: impl IntoIterator<Item = ContentId>,
) -> Result<Vec<(ContentId, Vec<u8>)>, String> {
    let mut work = InputsWork::default();
    let mut kept = Kept::empty(db, hashes, BuiltId::default());
    kept.index(db, hashes, items, &mut work)?;
    // alloc: per call, the anchors asked for
    let mut wanted: Vec<ContentId> = named.into_iter().collect();
    wanted.sort_unstable();
    wanted.dedup();
    let tables = kept.make(db, hashes, None, &wanted, &mut work)?;
    // alloc: per call, the tables as their reader takes them
    Ok(tables.into_iter().map(|it| (it.anchor, it.bytes)).collect())
}

/// A count that is another each time the positions of `owner` are found again
/// ([`positions`] runs): its one input is that memo, so the database runs this only then,
/// and a link that sees the count it kept knows the positions are the ones it encoded
/// without reading them. The count itself means nothing and nothing is keyed by it.
#[blaze_query::tracked(returns(copy))]
fn positions_stamp(db: &Db, owner: DefWithBodyId) -> u64 {
    let _ = positions(db, owner);
    STAMP.fetch_add(1, Ordering::Relaxed)
}

/// [`positions_stamp`] of where `func` is declared.
#[blaze_query::tracked(returns(copy))]
fn declared_stamp(db: &Db, func: FunctionId) -> u64 {
    let _ = crate::declaration_location(db, func);
    STAMP.fetch_add(1, Ordering::Relaxed)
}

/// The next stamp.
static STAMP: AtomicU64 = AtomicU64::new(1);

/// A position: the file, its 1-based line and column, as `core::panic::Location` has them.
pub(super) type Row = (base_db::FileId, u32, u32);

/// The positions of a body's spans, by table in [`hir_ty_share::code::table_of`]'s order:
/// table `t` is `rows[starts[t]..starts[t + 1]]`, by the span's index.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Positions {
    pub(super) starts: [u32; TABLES + 1],
    pub(super) rows: Box<[Row]>,
}

/// Each span's position, as the program observes it; a span with no syntax of its own (a
/// binding, [`MirSpan::Unknown`]) is where its owner is. Memoized, so a link after an edit
/// finds again only the bodies it changed.
#[blaze_query::tracked(returns(ref))]
pub(super) fn positions(db: &Db, owner: DefWithBodyId) -> Result<Positions, String> {
    let store = BodyStore::of(db, owner);
    let at_owner = owner_location(db, owner);
    let row = |span: MirSpan| {
        crate::source_location(db, owner, span)
            .or(at_owner)
            // alloc: an error
            .ok_or_else(|| format!("{span:?} of {owner:?} has no position, nor has its owner"))
    };
    let mut starts = [0u32; TABLES + 1];
    // alloc: per body, its rows, memoized
    let mut rows = Vec::new();
    for table in 0..TABLES {
        starts[table] = rows.len() as u32;
        match table {
            code::EXPRS => {
                for it in store.expr_ids() {
                    rows.push(row(MirSpan::ExprId(it))?);
                }
            }
            code::EXPR_ENDS => {
                for it in store.expr_ids() {
                    rows.push(row(MirSpan::ExprEnd(it))?);
                }
            }
            code::PATS => {
                for it in store.pat_ids() {
                    rows.push(row(MirSpan::PatId(it))?);
                }
            }
            code::BINDINGS => {
                for (it, _) in store.bindings() {
                    rows.push(row(MirSpan::BindingId(it))?);
                }
            }
            code::SELF_PARAM => rows.push(row(MirSpan::SelfParam)?),
            code::UNKNOWN => rows.push(row(MirSpan::Unknown)?),
            _ => unreachable!("{TABLES} tables"),
        }
    }
    starts[TABLES] = rows.len() as u32;
    Ok(Positions { starts, rows: rows.into_boxed_slice() })
}

/// Where a body's owner is declared.
fn owner_location(db: &Db, owner: DefWithBodyId) -> Option<Row> {
    use hir_ty_mir::mir::location::{location_of, written_range};
    let source = match owner {
        DefWithBodyId::FunctionId(func) => return *crate::declaration_location(db, func),
        DefWithBodyId::StaticId(it) => crate::written_node(db, it.lookup(db)),
        DefWithBodyId::ConstId(it) => crate::written_node(db, it.lookup(db)),
        DefWithBodyId::VariantId(it) => crate::written_node(db, it.lookup(db)),
    };
    location_of(db, source.file_id, written_range(source.value))
}

/// `rows` as the link's span table: the files named in the order first named, then each
/// row as the file's index, the line and the column, little-endian.
fn encode(
    db: &Db,
    hashes: &HashSource,
    names: &mut HashMap<base_db::FileId, String, Global>,
    rows: &[Row],
) -> Result<Vec<u8>, String> {
    // alloc: per table, the files it names, a few
    let mut files: Vec<base_db::FileId> = Vec::new();
    // alloc: per table, its rows' file indices
    let mut indices = Vec::with_capacity(rows.len());
    for &(file, _, _) in rows {
        let at = match files.iter().position(|it| *it == file) {
            Some(at) => at,
            None => {
                files.push(file);
                files.len() - 1
            }
        };
        indices.push(at as u32);
    }
    for &file in &files {
        if !names.contains_key(&file) {
            names.insert(file, hashes.file_name(db, file)?);
        }
    }
    let file_bytes: usize = files.iter().map(|it| 4 + names[it].len()).sum();
    // alloc: per table, its encoding for the link
    let mut out = Vec::with_capacity(8 + 4 + file_bytes + 4 + 12 * rows.len());
    out.put_bytes(&SPAN_TABLE_MAGIC);
    out.put_u32(files.len() as u32);
    for file in &files {
        out.put_bytes_u32(names[file].as_bytes());
    }
    out.put_u32(rows.len() as u32);
    for (&(_, line, column), file) in rows.iter().zip(indices) {
        for word in [file, line, column] {
            out.put_u32(word);
        }
    }
    Ok(out)
}

/// What a debugger and a backtrace call an instance: rustc's demangled name of it.
pub(super) fn debug_name(db: &Db, kind: &Kind) -> String {
    match kind {
        Kind::Code(instance, _) => {
            blaze_query::db::attach(db, || crate::backtrace_name(db, instance))
        }
        Kind::Start(main) => match crate::target_of_crate(db, main.krate(db)) {
            // alloc: per instance described, its debug name
            Ok(target) => target.entry().0.to_owned(),
            // alloc: per instance described, its debug name, which names the trouble
            Err(error) => format!("{{start: {error}}}"),
        },
        // alloc: per instance described, its debug name
        Kind::Allocator(forward) => forward.symbol.to_owned(),
        // alloc: per instance described, its debug name
        Kind::Asm(..) => "{global_asm}".to_owned(),
    }
}
