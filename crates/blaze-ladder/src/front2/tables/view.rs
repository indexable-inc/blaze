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
use blaze_scope::task::{ItemText, MacroBody, Variant};
use blaze_surface::{Edition, ItemDetail, ItemKind, Namespace};

use super::super::index::ImplAt;
use super::super::project_records::BodySource;
use super::super::source::{self, MacroAt, MemberOf, NamesSource};
use super::recorder::{INCOHERENT, NamesInto, ReadSink, TableRead, UNFILED, bucket_cell};

/// What a [`Tables`] is made from.
pub struct TablesOpts<'t, 'n> {
    /// Every crate's two records, read in place (this worker's reader over the shared bytes).
    pub records: &'n source::Records<'n>,
    /// Where each read goes: the task's recorder.
    pub reads: &'t dyn ReadSink,
}

/// The read-only world of one task (module docs).
#[derive(Clone, Copy)]
pub struct Tables<'t, 'n> {
    records: &'n source::Records<'n>,
    reads: &'t dyn ReadSink,
}

impl<'t, 'n> Tables<'t, 'n> {
    pub fn new(opts: TablesOpts<'t, 'n>) -> Tables<'t, 'n> {
        Tables { records: opts.records, reads: opts.reads }
    }

    /// The records the view answers from.
    pub fn records(self) -> &'n source::Records<'n> {
        self.records
    }

    fn cell(self, cell: NameCell) {
        self.reads.read(TableRead::Names(cell));
    }

    fn item(self, key: ItemKey) {
        self.cell(NameCell::Item { key });
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

    /// WHOLE: the link closure walks every module's scope of every crate it reaches; every
    /// crate's names and items are recorded whole (the closure does not say which it reached).
    fn linked<'a>(self, roots: &[usize], alloc: Alloc<'a>) -> Option<Result<super::super::linked::Linked<'a>, super::super::linked::RootsError>> {
        for krate in 0..self.records.crate_count() {
            self.whole(krate, Section::Names);
            self.whole(krate, Section::Items);
        }
        self.records.linked(roots, alloc)
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

    fn variants(self, owner: ItemKey) -> Option<&'n [Variant<&'n str>]> {
        self.cell(NameCell::Variants { owner });
        self.records.variants(owner)
    }

    /// Found: the enum's variants cell. Not found: the crate's items whole (no cell says "no
    /// enum has this variant").
    fn variant_of(self, variant: ItemKey) -> Option<(ItemKey, u32)> {
        let found = self.records.variant_of(variant);
        match found {
            Some((owner, _)) => self.cell(NameCell::Variants { owner }),
            None => self.whole_of(variant, Section::Items),
        }
        found
    }

    fn has_text(self, key: ItemKey) -> bool {
        self.item(key);
        self.records.has_text(key)
    }

    fn text_of(self, key: ItemKey) -> Option<ItemText<'n>> {
        self.item(key);
        self.records.text_of(key)
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

    /// WHOLE: the items record's file rows have no cell.
    fn file_shown(self, krate: usize, file: u32) -> Option<&'n str> {
        self.whole(krate, Section::Items);
        self.records.file_shown(krate, file)
    }

    /// WHOLE: a whole file's text, read only to print a position (rule 71).
    fn file_text(self, krate: usize, file: u32) -> Option<&'n str> {
        self.whole(krate, Section::Items);
        self.records.file_text(krate, file)
    }

    /// Over records always refused by name: nothing is read.
    fn call_origin(self, krate: usize, call: ItemKey) -> Result<super::super::expansion_origin::CallOrigin<'n>, super::super::expansion_origin::Unmapped> {
        self.records.call_origin(krate, call)
    }

    fn text_marks(self, krate: usize, call: ItemKey) -> Result<Option<blaze_scope::crate_roots::TextMarks<'n>>, super::super::item_site::MarksLost> {
        self.cell(NameCell::Expansion { call });
        self.records.text_marks(krate, call)
    }

    /// The item's cell; for a fn without a body or a static, WHOLE items too: the answer is
    /// read from its file's outline (the `extern` block around it), which its cell does not
    /// hold.
    fn extern_abi(self, key: ItemKey) -> Option<Option<&'n str>> {
        self.item(key);
        if let Some((owner, _)) = self.records.member_owner(key) {
            self.cell(NameCell::Members { owner });
        } else if matches!(self.records.kind_of_item(key), Some(ItemKind::Function | ItemKind::Static)) && !self.records.text_of(key).is_some_and(|t| t.text.trim_end().ends_with('}')) {
            self.whole_of(key, Section::Items);
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

    /// Found: the owner's members cell. Not found: the crate's items whole.
    fn member_owner(self, member: ItemKey) -> Option<(ItemKey, &'n str)> {
        let found = self.records.member_owner(member);
        match found {
            Some((owner, _)) => self.cell(NameCell::Members { owner }),
            None => self.whole_of(member, Section::Items),
        }
        found
    }

    /// WHOLE: every impl of the crate.
    fn each_impl(self, krate: usize, each: &mut dyn FnMut(ImplAt<'n>)) {
        self.whole(krate, Section::Items);
        self.records.each_impl(krate, each)
    }

    /// WHOLE: every impl row of the crate.
    fn each_impl_row(self, krate: usize, each: &mut dyn FnMut(blaze_scope::items_record::ImplRow<'n>)) -> bool {
        self.whole(krate, Section::Items);
        self.records.each_impl_row(krate, each)
    }

    fn has_buckets(self) -> bool {
        self.records.has_buckets()
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
        self.reads.read(TableRead::Impls { krate: [0; 16], bucket: INCOHERENT });
        self.records.incoherent_crates()
    }

    fn impl_with_header(self, krate: usize, key: ItemKey, module: ItemKey) -> Option<ImplAt<'n>> {
        self.item(key);
        self.records.impl_with_header(krate, key, module)
    }

    /// The crate's lang rows, whole (one table a crate).
    fn each_lang(self, krate: usize, each: &mut dyn FnMut(&'n str, ItemKey)) {
        if let Some(prefix) = self.prefix(krate) {
            self.reads.read(TableRead::Lang { krate: prefix });
        }
        self.records.each_lang(krate, each)
    }

    /// The walk's cells, then the definition's item cell.
    fn macro_body_of(self, module: ItemKey, written: Written<'_, &str>) -> Result<Option<MacroBody<'n>>, PathError> {
        let found = self.walked(module, Namespace::Macros, written)?;
        if let Some(def) = found.whole(written.segments.len()) {
            self.item(def.key);
        }
        self.records.macro_body_of(module, written)
    }

    /// The walk's cells, the definition's item cell and its crate's root cell (its key, its
    /// proc-macro runner, its edition).
    fn macro_at(self, module: ItemKey, written: Written<'_, &str>) -> Result<MacroAt<'n>, PathError> {
        let found = self.walked(module, Namespace::Macros, written)?;
        if let Some(def) = found.whole(written.segments.len()) {
            self.item(def.key);
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
    fn failed_call<'b>(self, call: ItemKey, alloc: Alloc<'b>) -> Option<blaze_scope::names_record::FailedRow<'b>>
    where
        'n: 'b,
    {
        self.whole_of(call, Section::Names);
        self.records.failed_call(call, alloc)
    }

    /// WHOLE: every item of the crate (a whole-crate walk, never one body's).
    fn each_item(self, krate: usize, each: &mut dyn FnMut(ItemKey, ItemKey)) -> bool {
        self.whole(krate, Section::Items);
        self.records.each_item(krate, each)
    }

    fn binds(self, module: ItemKey, ns: Namespace, name: &'n str) -> bool {
        let _ = self.walked(module, ns, Written { kind: blaze_surface::PathKind::SelfMod, segments: &[name] });
        self.records.binds(module, ns, name)
    }

    /// Found: its item cell. Not found: the crate's items whole (no cell says "no key begins
    /// so").
    fn key_beginning(self, first: [u8; 16]) -> Option<ItemKey> {
        let found = self.records.key_beginning(first);
        match found {
            Some(key) => self.item(key),
            None => {
                let mut krate = [0u8; 8];
                krate.copy_from_slice(&first[..8]);
                self.cell(NameCell::Whole { krate, section: Section::Items });
            }
        }
        found
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
