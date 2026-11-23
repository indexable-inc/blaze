//! THE READ-ONLY VIEW a task's checker asks (engine.md, E2): [`Tables`], `Copy`, answering
//! [`NamesSource`] (and [`BodySource`]) from each crate's records ([`source::Records`]), each
//! answer's cells put in the task's recorder ([`ReadSink`]) as it is asked.
//!
//! EVERY QUESTION RECORDS. A method either records the cell(s) its answer is a function of, or
//! records a whole section ([`NameCell::Whole`]) and says why in its doc. The view itself holds
//! nothing: no answer is kept above the records, so a read is recorded for every task that asks
//! it (the old world's per-job caches listed it for the first body only: reads.rs, "What is not
//! listed"). `source::Records` below keeps what it DECODED (a crate's record opened, a key's
//! beginning searched, a file outlined): each a pure function of the record's bytes, so a later
//! task that is answered from it gets the same answer, and records the same cells here.
//!
//! The crate facts the items record's header holds (edition, cfg atoms, proc-macro, which std,
//! the root's attributes and features) are recorded as the crate's `Root` cell: the crate's
//! key covers its manifest, and the root cell its root file's head.
//!
//! What `source::Records` is NOT: shared between threads. It opens records lazily (`RefCell`,
//! `OnceCell`), so each worker holds its own over the same immutable bytes (the loader's): what
//! is shared is bytes, never a mutable table (parallel.md point 4).

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_scope::names::{PathError, Resolved, Written};
use blaze_scope::names_cells::{NameCell, Section};
use blaze_scope::resolve::{Names, Place};
use blaze_scope::task::{ItemText, MacroBody};
use blaze_surface::{Edition, ItemDetail, ItemKind, Namespace};

use super::super::buckets::TableSet;
use super::super::heads::{AdtRow, CoherenceMarks, Store};
use super::super::index::ImplAt;
use super::super::keys::checker_of;
use super::super::project_records::BodySource;
use super::super::source::{self, MacroAt, MemberOf, NamesSource};
use super::recorder::{INCOHERENT, NamesInto, ReadSink, TableRead, UNFILED, bucket_cell};

#[derive(Clone, Copy)]
pub struct BodyHeadFacts<'s, 't, S> {
    store: &'s Store<'t>,
    names: S,
}

impl<'s, 't, 'n, S: NamesSource<'n>> BodyHeadFacts<'s, 't, S> {
    pub fn new(store: &'s Store<'t>, names: S) -> Self {
        Self { store, names }
    }

    pub(in crate::front2) fn library(self, keys: &super::super::keys::Keys, key: blaze_typeck::ItemKey) -> Option<ItemKey> {
        keys.library_in(self.names, key)
    }

    pub(in crate::front2) fn crate_of(self, lib: ItemKey) -> Option<usize> {
        self.names.crate_of(lib)
    }

    fn read(self, key: blaze_typeck::ItemKey) {
        // A block-local item's head belongs to the enclosing body. Only an item with
        // its own Signature task has a distinct head cell (task.rs `fact_read`).
        if self.store.locals.head(key).is_none() {
            self.names.impls_read(TableRead::Head { item: key.0.to_be_bytes(), part: 0 });
        }
    }

    pub fn adt(self, key: blaze_typeck::ItemKey) -> Option<&'s AdtRow> {
        self.read(key);
        self.store.adts.get(&key)
    }

    pub fn marks(self, lib: ItemKey) -> Option<CoherenceMarks> {
        let key = checker_of(lib);
        self.read(key);
        self.store.adt_declarations.get(&key).map(|decl| decl.marks).or_else(|| self.store.maps.traits.get(&lib).map(|row| row.marks))
    }

    /// Type and const parameter count (including `Self`), and whether every parameter
    /// after `Self` defaults to it. A bare impl path uses those defaults.
    pub fn trait_parameters(self, lib: ItemKey) -> Option<(u32, bool)> {
        self.read(checker_of(lib));
        self.store.maps.traits.get(&lib).map(|head| (head.generics, head.all_self_defaults))
    }
}

/// What a [`Tables`] is made from.
pub struct TablesOpts<'t, 'n> {
    /// Every crate's two records, read in place (this worker's reader over the shared bytes).
    pub records: &'n source::Records<'n>,
    /// Every crate's impl tables (lane BK's E3, buckets/records.rs), which the check files a
    /// question's impls from; `None`: today's per-job filing from the items records' bucket
    /// section (only to hold the two equal while both exist).
    pub impls: Option<&'n TableSet<'n>>,
    /// The crates whose lang rows name the lang items (the crate graph's: `core` and the
    /// program's own crate, as `ProjectWorld::from_records` takes them).
    pub lang_crates: &'n [usize],
    /// Where each read goes: the task's recorder.
    pub reads: &'t dyn ReadSink,
}

/// The read-only world of one task (module docs).
#[derive(Clone, Copy)]
pub struct Tables<'t, 'n> {
    records: &'n source::Records<'n>,
    impls: Option<&'n TableSet<'n>>,
    lang_crates: &'n [usize],
    reads: &'t dyn ReadSink,
}

impl<'t, 'n> Tables<'t, 'n> {
    pub fn new(opts: TablesOpts<'t, 'n>) -> Tables<'t, 'n> {
        Tables { records: opts.records, impls: opts.impls, lang_crates: opts.lang_crates, reads: opts.reads }
    }

    /// The records the view answers from.
    pub fn records(self) -> &'n source::Records<'n> {
        self.records
    }

    /// The crates whose lang rows name the lang items.
    pub fn lang_crates(self) -> &'n [usize] {
        self.lang_crates
    }

    /// The same view putting its reads in `reads` (a task's own recorder).
    pub fn reading_into<'u>(self, reads: &'u dyn ReadSink) -> Tables<'u, 'n> {
        Tables { records: self.records, impls: self.impls, lang_crates: self.lang_crates, reads }
    }

    /// Where the view puts its reads.
    pub fn reads(self) -> &'t dyn ReadSink {
        self.reads
    }

    fn cell(self, cell: NameCell) {
        self.reads.read(TableRead::Names(cell));
    }

    fn item(self, key: ItemKey) {
        self.cell(NameCell::Item { key });
    }

    fn member_source(self, key: ItemKey, written_by: Option<ItemKey>, facet: TableRead) {
        self.cell(NameCell::MemberOf { member: key });
        match written_by {
            Some(call) => self.cell(NameCell::Expansion { call }),
            None => self.reads.read(facet),
        }
    }

    /// The 8 bytes every key of crate `krate` begins with; `None` for no such crate.
    fn prefix(self, krate: usize) -> Option<[u8; 8]> {
        self.records.crate_key(krate).map(|k: CrateItemKey| k.prefix())
    }

    fn root(self, krate: usize) {
        if let Some(krate) = self.prefix(krate) {
            self.cell(NameCell::Root { krate });
        }
    }

    /// Whether crate `krate` may write an incoherent impl: its one names cell
    /// ([`NameCell::Incoherent`]).
    fn incoherent(self, krate: usize) {
        if let Some(krate) = self.prefix(krate) {
            self.cell(NameCell::Incoherent { krate });
        }
    }

    fn whole(self, krate: usize, section: Section) {
        if let Some(krate) = self.prefix(krate) {
            self.cell(NameCell::Whole { krate, section });
        }
    }

    fn whole_of(self, key: ItemKey, section: Section) {
        self.cell(NameCell::Whole { krate: key.crate_prefix(), section });
    }

    /// A crate's key's first 16 bytes (an impl table's crate).
    fn crate16(self, krate: usize) -> [u8; 16] {
        let mut out = [0u8; 16];
        if let Some(key) = self.records.crate_key(krate) {
            out.copy_from_slice(&key.0.0[..16]);
        }
        out
    }

    /// The walk's cells are recorded by walking it here (a macro path, `binds`): `macro_at`
    /// and `binds` of the records walk again inside, unlogged, over the same cells.
    fn walked(self, module: ItemKey, ns: Namespace, written: Written<'_, &str>) -> Result<Resolved, PathError> {
        self.records.walk_logged(&NamesInto(self.reads), module, ns, written)
    }
}

impl<'n> NamesSource<'n> for Tables<'_, 'n> {
    fn head_lowerings(self) -> Option<&'n super::super::heads::HeadLowerings> {
        NamesSource::head_lowerings(self.records)
    }

    fn solved(self) -> Option<&'n Names<'n>> {
        None
    }

    fn gap(self, question: &'static str, key: Option<ItemKey>) {
        self.records.gap(question, key)
    }

    /// The crate graph: the task's key covers it; no record is read.
    fn crate_count(self) -> usize {
        self.records.crate_count()
    }

    /// Key arithmetic: no record is read.
    fn crate_of(self, key: ItemKey) -> Option<usize> {
        self.records.crate_of(key)
    }

    /// The crate graph: no record is read.
    fn crate_key(self, krate: usize) -> Option<CrateItemKey> {
        self.records.crate_key(krate)
    }

    fn root_module(self, krate: usize) -> Option<ItemKey> {
        self.root(krate);
        self.records.root_module(krate)
    }

    fn edition_of(self, krate: usize) -> Option<Edition> {
        self.root(krate);
        self.records.edition_of(krate)
    }

    fn cfg_atoms(self, krate: usize) -> Option<&'n [(&'n str, Option<&'n str>)]> {
        self.root(krate);
        if let Some(prefix) = self.prefix(krate) {
            self.cell(NameCell::CfgAtoms { krate: prefix });
        }
        self.records.cfg_atoms(krate)
    }

    fn proc_macro(self, krate: usize) -> Option<bool> {
        self.root(krate);
        self.records.proc_macro(krate)
    }

    fn std_is(self, krate: usize) -> Option<blaze_unit_graph::StdIs> {
        self.root(krate);
        self.records.std_is(krate)
    }

    fn root_attrs(self, krate: usize) -> Option<blaze_scope::graph::RootAttrs> {
        self.root(krate);
        self.records.root_attrs(krate)
    }

    fn has_feature(self, krate: usize, name: &str) -> Option<bool> {
        self.root(krate);
        self.records.has_feature(krate, name)
    }

    fn blaze_extensions(self, krate: usize) -> parser::Extensions {
        if let Some(prefix) = self.prefix(krate) {
            self.cell(NameCell::Extensions { krate: prefix });
        }
        self.records.blaze_extensions(krate)
    }

    /// Each crate whose link facts (`proc_macro`, extern prelude, `Origin::ExternCrate` bindings)
    /// are inspected by the link closure gets one [`NameCell::Links`] read, instead of recording
    /// every crate's names and items sections whole.
    fn linked<'a>(self, roots: &[usize], alloc: Alloc<'a>) -> Option<Result<super::super::linked::Linked<'a>, super::super::linked::RootsError>> {
        self.records.linked_logged(roots, alloc, |krate| {
            if let Some(prefix) = self.prefix(krate) {
                self.cell(NameCell::Links { krate: prefix });
            }
        })
    }

    /// Reads every crate's name: every crate's root cell.
    fn sysroot_std(self) -> Option<Option<usize>> {
        for krate in 0..self.records.crate_count() {
            self.root(krate);
        }
        self.records.sysroot_std()
    }

    fn may_write_incoherent(self, krate: usize) -> Option<bool> {
        self.root(krate);
        // The flag itself: the root cell's digest leaves the root file's text out, and the
        // flag is read from it (`#![rustc_attrs]`, `#![rustc_coherence_is_core]`).
        self.incoherent(krate);
        self.records.may_write_incoherent(krate)
    }

    fn walk(self, module: ItemKey, ns: Namespace, written: Written<'_, &str>) -> Result<Resolved, PathError> {
        self.walked(module, ns, written)
    }

    fn written_module(self, module: ItemKey, n: u8) -> Result<ItemKey, PathError> {
        self.records.written_module_logged(&NamesInto(self.reads), module, n)
    }

    fn traits_in_scope<'b>(self, module: ItemKey, alloc: Alloc<'b>) -> Result<AVec<'b, ItemKey>, PathError> {
        self.records.traits_in_scope_logged(&NamesInto(self.reads), module, alloc)
    }

    fn variants(self, owner: ItemKey) -> Option<source::VariantRows<'n>> {
        self.cell(NameCell::Variants { owner });
        self.records.variants(owner)
    }

    /// The variant's `VariantOf` cell on both paths: its digest is the answer (enum and index,
    /// or the refusal of a key under two enums), and absent is "no variant of that key".
    fn variant_of(self, variant: ItemKey) -> Option<(ItemKey, u32)> {
        self.cell(NameCell::VariantOf { variant });
        self.records.variant_of(variant)
    }

    fn has_text(self, key: ItemKey) -> bool {
        self.item(key);
        self.records.has_text(key)
    }

    fn text_of(self, key: ItemKey) -> Option<ItemText<'n>> {
        self.item(key);
        self.reads.read(TableRead::Text { key });
        self.records.text_of(key)
    }

    fn signature_text_of(self, key: ItemKey) -> Option<ItemText<'n>> {
        self.item(key);
        self.reads.read(TableRead::Signature { key });
        self.records.text_of(key)
    }

    fn seeded_text_of(self, key: ItemKey) -> Option<ItemText<'n>> {
        // The frozen record authenticates the source span it needs when it is imported.
        // Its producer owns that signature. Evaluation records the body when it uses it.
        self.item(key);
        self.records.text_of(key)
    }

    fn read_member_body(self, member: ItemKey, written_by: Option<ItemKey>) {
        self.member_source(member, written_by, TableRead::Text { key: member });
    }

    fn read_member_signature(self, member: ItemKey, written_by: Option<ItemKey>) {
        self.member_source(member, written_by, TableRead::Signature { key: member });
    }

    fn name_of(self, key: ItemKey) -> Option<&'n str> {
        self.item(key);
        self.records.name_of(key)
    }

    fn home_of(self, key: ItemKey) -> Option<ItemKey> {
        self.item(key);
        self.records.home_of(key)
    }

    /// The item's cell: its place in the file is not in the cell (rule 71), and a place is
    /// only printed.
    fn place_of(self, key: ItemKey) -> Option<Place> {
        self.item(key);
        self.records.place_of(key)
    }

    fn item_start(self, key: ItemKey) -> Option<(blaze_scope::resolve::At, u32)> {
        self.item(key);
        self.records.item_start(key)
    }

    /// The crate's file paths cell ([`NameCell::Files`]): only the package and shown paths, never
    /// the items or surfaces.
    fn file_paths(self, krate: usize, file: u32) -> Option<source::FilePaths<'n>> {
        if let Some(prefix) = self.prefix(krate) {
            self.cell(NameCell::Files { krate: prefix });
        }
        self.records.file_paths(krate, file)
    }

    /// WHOLE: a whole file's text, read only to print a position (rule 71).
    fn file_text(self, krate: usize, file: u32) -> Option<&'n str> {
        self.whole(krate, Section::Items);
        self.records.file_text(krate, file)
    }

    /// The expansion's cell (its record: the call part and the column, as `call_text` reads
    /// them) and the WHOLE items section: the answer finds the calling entry among the items
    /// rows and in today's text of the calling file (seam_rules.rs `recorded_call_origin`), and
    /// a finding filed through it holds that entry's line (expansion_origin.rs `file_range`), so
    /// an edit that moves the entry moves the reader. A `#[track_caller]` site asks it too, and
    /// already reads that section for the file's shown path (`file_paths`).
    fn call_origin(self, krate: usize, call: ItemKey) -> Result<super::super::expansion_origin::CallOrigin<'n>, super::super::expansion_origin::Unmapped> {
        self.cell(NameCell::Expansion { call });
        self.whole(krate, Section::Items);
        self.records.call_origin(krate, call)
    }

    fn text_marks(self, krate: usize, call: ItemKey) -> Result<Option<blaze_scope::crate_roots::TextMarks<'n>>, super::super::item_site::MarksLost> {
        self.cell(NameCell::Expansion { call });
        self.records.text_marks(krate, call)
    }

    /// The text the expansion `call` is written in, from the call part of its record's origins
    /// (source.rs `Records::call_text`), read in the expansion's cell as its marks are. Before,
    /// this view kept the trait's default, which asks `call_origin`, refused over records by
    /// design: a macro-made body's `include_str!` was `Refused` (body_host.rs `body_file`;
    /// body_host_include_tests.rs, run t67 2026-11-21).
    fn call_text(self, krate: usize, call: ItemKey) -> Option<blaze_scope::resolve::At> {
        self.cell(NameCell::Expansion { call });
        self.records.call_text(krate, call)
    }

    /// WHOLE: the answer is a search of the items record's owned-members index.
    fn member_beginning(self, krate: usize, item: blaze_typeck::ItemKey) -> Result<Option<ItemKey>, crate::seam::PositionGap> {
        self.whole(krate, Section::Items);
        self.records.member_beginning(krate, item)
    }

    /// WHOLE: every item row of the crate, and each impl's and trait's members, are visited.
    fn enclosing_keys(self, krate: usize, visit: impl FnMut(ItemKey) -> Result<(), crate::seam::PositionGap>) -> Result<(), crate::seam::PositionGap> {
        self.whole(krate, Section::Items);
        self.records.enclosing_keys(krate, visit)
    }

    /// The expansion's cell: its record holds the text.
    fn expansion_text(self, krate: usize, call: ItemKey) -> Option<&'n str> {
        self.cell(NameCell::Expansion { call });
        self.records.expansion_text(krate, call)
    }

    /// The item's cell alone, for an item of a file and of a macro call's expansion alike: the
    /// answer is its items row's `foreign` field (the block's ABI, written by the names task),
    /// which the `Item` cell's digest covers (blaze-scope names_cells.rs `item_digest`).
    fn extern_abi(self, key: ItemKey) -> Option<Option<&'n str>> {
        self.item(key);
        // Whether it is a member, and of which owner: its `MemberOf` cell on both paths.
        self.cell(NameCell::MemberOf { member: key });
        if let Some((owner, _)) = self.records.member_owner(key) {
            self.cell(NameCell::Members { owner });
        }
        self.records.extern_abi(key)
    }

    fn kind_of_item(self, key: ItemKey) -> Option<ItemKind> {
        self.item(key);
        self.records.kind_of_item(key)
    }

    fn detail_of(self, key: ItemKey) -> Option<ItemDetail> {
        self.item(key);
        self.records.detail_of(key)
    }

    fn is_ctor(self, key: ItemKey) -> bool {
        self.item(key);
        self.records.is_ctor(key)
    }

    fn each_member(self, owner: ItemKey, each: &mut dyn FnMut(MemberOf<'n>)) -> bool {
        self.cell(NameCell::Members { owner });
        self.records.each_member(owner, each)
    }

    /// The member's `MemberOf` cell on both paths: its digest is the answer (owner and name),
    /// and absent is "no named member of that key".
    fn member_owner(self, member: ItemKey) -> Option<(ItemKey, &'n str)> {
        self.cell(NameCell::MemberOf { member });
        self.records.member_owner(member)
    }

    /// WHOLE: every impl of the crate.
    fn each_impl(self, krate: usize, each: &mut dyn FnMut(ImplAt<'n>)) {
        self.whole(krate, Section::Items);
        self.records.each_impl(krate, each)
    }

    /// The crate's impl rows cell ([`NameCell::ImplRows`]).
    fn each_impl_row(self, krate: usize, each: &mut dyn FnMut(blaze_scope::items_record::ImplRow<'n>)) -> bool {
        if let Some(prefix) = self.prefix(krate) {
            self.cell(NameCell::ImplRows { krate: prefix });
        }
        self.records.each_impl_row(krate, each)
    }

    fn has_buckets(self) -> bool {
        self.records.has_buckets()
    }

    /// The impl tables; each cell a lookup in them read comes back through
    /// [`NamesSource::impls_read`].
    fn impl_tables(self) -> Option<&'n TableSet<'n>> {
        self.impls
    }

    fn impls_read(self, read: TableRead) {
        self.reads.read(read);
    }

    fn asking(self, ask: Option<blaze_typeck::Asking>) {
        self.reads.asking(ask);
    }

    fn bucket(self, krate: usize, key: &blaze_scope::items_record::buckets::BucketKey, each: &mut dyn FnMut(blaze_scope::items_record::buckets::BucketImpl)) -> Option<blaze_id::ContentId> {
        self.reads.read(TableRead::Impls { krate: self.crate16(krate), bucket: bucket_cell(key.bytes()) });
        self.records.bucket(krate, key, each)
    }

    fn unfiled(self, krate: usize, each: &mut dyn FnMut(blaze_scope::items_record::ImplRow<'n>, blaze_scope::items_record::buckets::NotFiled)) -> Option<blaze_id::ContentId> {
        self.reads.read(TableRead::Impls { krate: self.crate16(krate), bucket: UNFILED });
        self.records.unfiled(krate, each)
    }

    fn incoherent_crates(self) -> Option<(&'n [usize], blaze_id::ContentId)> {
        // The fact at cell grain (lane BH1): which crates are asked is the lang set, fixed by
        // the owner's root (its `#![no_core]` and its dependencies: each lang crate's `Root`),
        // and each one's answer is its `Incoherent` cell. The coarse read below stays for the
        // engine's `Whole` strategy (blaze-cli engine_scope_reads.rs), which widens it to every
        // scope crate's record; `Cells` drops it, these cells covering every fact behind it.
        for krate in self.lang_crates.iter().copied() {
            self.root(krate);
            self.incoherent(krate);
        }
        self.reads.read(TableRead::Impls { krate: [0; 16], bucket: INCOHERENT });
        self.records.incoherent_crates()
    }

    fn impl_with_header(self, krate: usize, key: ItemKey, module: ItemKey) -> Option<ImplAt<'n>> {
        self.item(key);
        self.reads.read(TableRead::Signature { key });
        self.records.impl_with_header(krate, key, module)
    }

    /// The crate's lang rows: one names cell a crate ([`NameCell::Lang`]).
    fn each_lang(self, krate: usize, each: &mut dyn FnMut(&'n str, ItemKey)) {
        if let Some(prefix) = self.prefix(krate) {
            self.reads.read(TableRead::Names(NameCell::Lang { krate: prefix }));
        }
        self.records.each_lang(krate, each)
    }

    /// The walk's cells, then the definition's item cell.
    fn macro_body_of(self, module: ItemKey, written: Written<'_, &str>) -> Result<Option<MacroBody<'n>>, PathError> {
        let found = self.walked(module, Namespace::Macros, written)?;
        if let Some(def) = found.whole(written.segments.len()) {
            self.item(def.key);
            self.reads.read(TableRead::Text { key: def.key });
        }
        self.records.macro_body_of(module, written)
    }

    /// The walk's cells, the definition's item cell and its crate's root cell (its key, its
    /// proc-macro runner, its edition).
    fn macro_at(self, module: ItemKey, written: Written<'_, &str>) -> Result<MacroAt<'n>, PathError> {
        let found = self.walked(module, Namespace::Macros, written)?;
        if let Some(def) = found.whole(written.segments.len()) {
            self.item(def.key);
            self.reads.read(TableRead::Text { key: def.key });
            self.cell(NameCell::Root { krate: def.key.crate_prefix() });
        }
        self.records.macro_at(module, written)
    }

    /// The module's own cell (its module row).
    fn module_path(self, module: ItemKey) -> Option<&'n str> {
        self.item(module);
        self.records.module_path(module)
    }

    /// WHOLE: the names record's failed-call rows have no cell (read only on an error's path).
    fn failed_call<'b>(self, call: ItemKey, bump: blaze_own::Bump<'b>) -> Option<blaze_scope::names_record::FailedRow<'b>>
    where
        'n: 'b,
    {
        self.whole_of(call, Section::Names);
        self.records.failed_call(call, bump)
    }

    /// WHOLE: every item of the crate (a whole-crate walk, never one body's).
    fn each_item(self, krate: usize, each: &mut dyn FnMut(ItemKey, ItemKey)) -> bool {
        self.whole(krate, Section::Items);
        self.records.each_item(krate, each)
    }

    fn block_binding(self, module: ItemKey, ns: Namespace, name: &str) -> Option<Option<blaze_surface::Binding>> {
        self.cell(NameCell::Module { module });
        blaze_scope::names_cells::CellSink::put_binding(&NamesInto(self.reads), module, ns, name);
        self.records.block_binding(module, ns, name)
    }

    fn binds(self, module: ItemKey, ns: Namespace, name: &'n str) -> bool {
        let _ = self.walked(module, ns, Written { kind: blaze_surface::PathKind::SelfMod, segments: &[name] });
        self.records.binds(module, ns, name)
    }

    /// The prefix's `Beginning` cell on both paths: every key of the crate beginning so, so a
    /// second key added with the prefix (which turns the answer into a refusal) moves it, and
    /// absent is "no key begins so".
    fn key_beginning(self, first: [u8; 16]) -> Option<ItemKey> {
        self.cell(NameCell::Beginning { first });
        self.records.key_beginning(first)
    }

    /// The item's `Item` cell: its row holds the count (D155), and the row's digest covers
    /// its text, so an edit to its generics moves the cell.
    fn declared_lifetimes(self, key: ItemKey) -> Option<u32> {
        self.item(key);
        self.records.declared_lifetimes(key)
    }

    /// The item's `Item` cell: its digest covers the item's value rows (blaze-scope
    /// names_cells.rs `item_digest`), so a value that moves while the item's text stays moves it.
    fn attr_value(self, key: ItemKey, attr: &'static str) -> Option<Option<blaze_scope::attr_values::RowValue<'n>>> {
        self.item(key);
        self.records.attr_value(key, attr)
    }

    /// The trait's `Members` cell: the member rows hold each associated type's count (D155).
    fn assoc_lifetimes(self, trait_: ItemKey, name: blaze_typeck::Name) -> Option<u32> {
        self.cell(NameCell::Members { owner: trait_ });
        self.records.assoc_lifetimes(trait_, name)
    }
}

impl<'n> BodySource<'n> for Tables<'_, 'n> {
    /// WHOLE: a whole file of the program's own crate (what a body is listed and found by,
    /// never what it is checked by).
    fn own_file(self, file: u32) -> Option<&'n str> {
        self.whole(0, Section::Items);
        BodySource::own_file(self.records, file)
    }

    /// WHOLE, as [`BodySource::own_file`].
    fn with_outline(self, file: u32, alloc: Alloc<'_>, each: &mut dyn FnMut(&str, &blaze_file_items::Outline<'_>)) -> bool {
        self.whole(0, Section::Items);
        BodySource::with_outline(self.records, file, alloc, each)
    }
}
