//! The production [`Items`] and [`Source`] (drive.md, "Tasks and Need"): the records one task
//! was handed, each checked once when it is added and then read in place (rule 74). A question
//! about a record the task does not hold is the typed [`SurfaceError`] naming the record, which a
//! task turns into a `Need` of the name that makes it: the asker never makes another item's
//! record (bottom.md 5).
//!
//! The records are the payloads of the drive's whole records, which start aligned
//! ([`crate::record::PAYLOAD_ALIGN`]) in store buffers aligned the same: blaze-format's views
//! read them where they are.

use core::fmt;

use blaze_format::FormatError;
use blaze_id::{ContentId, CrateItemKey, ItemKey, NoItem};
use blaze_own::{ASortedMap, Alloc, SortedMap};
use blaze_surface::{
    CrateGraphView, CrateRootView, FileItemsView, HasText, Items, KeyEntry, KeyIndexView,
    LangItemsView, MacroDefView, ModuleDeclsView, ModuleFileView, NoImplsOfView, ScopeView,
    Source, SurfaceError, TextualEntryView,
};

/// Which of blaze-surface's records a buffer is. blaze-surface names these only in its
/// `testing` module (`testing::Fact`); this is the production list until that one moves to its
/// crate root (a gap for blaze-surface's lane, rule 30).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Fact {
    FileItems,
    ModuleDecls,
    ModuleScope,
    TextualEntry,
    ModuleFile,
    CrateRoot,
    MacroDef,
    KeyIndex,
    LangItems,
    CrateGraph,
    NoImplsOf,
}

impl Fact {
    /// The record's name, as a [`SurfaceError`] names the fact.
    pub fn name(self) -> &'static str {
        match self {
            Fact::FileItems => "FileItems",
            Fact::ModuleDecls => "ModuleDecls",
            Fact::ModuleScope => "ModuleScope",
            Fact::TextualEntry => "TextualEntry",
            Fact::ModuleFile => "ModuleFile",
            Fact::CrateRoot => "CrateRoot",
            Fact::MacroDef => "MacroDef",
            Fact::KeyIndex => "KeyIndex",
            Fact::LangItems => "LangItems",
            Fact::CrateGraph => "CrateGraph",
            Fact::NoImplsOf => "NoImplsOf",
        }
    }
}

/// A record handed to a task that is not the record it is handed as.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HeldError {
    pub fact: Fact,
    pub error: FormatError,
}

impl fmt::Display for HeldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a {} record handed to a task: {} (rule 3: refused, not read past)", self.fact.name(), self.error)
    }
}

impl core::error::Error for HeldError {}

/// The records and texts one task holds, borrowed for `'a` from the buffers its driver read.
pub struct TaskItems<'a, 'b> {
    krate: CrateItemKey,
    records: ASortedMap<'b, (Fact, ContentId), &'a [u8]>,
    /// The crate's `KeyIndex` records, by module: scanned for [`Items::entry`].
    texts: ASortedMap<'b, ContentId, &'a str>,
}

impl<'a, 'b> TaskItems<'a, 'b> {
    /// The records of a task compiling `krate` (or reading it, when it holds no text).
    pub fn new_in(krate: CrateItemKey, alloc: Alloc<'b>) -> TaskItems<'a, 'b> {
        TaskItems { krate, records: SortedMap::new_in(alloc), texts: SortedMap::new_in(alloc) }
    }

    /// Hold `bytes` as a `fact` record, checked whole now; a second record of one key replaces
    /// nothing and is refused only if its bytes differ (two values under one key, rule 21).
    pub fn insert(&mut self, fact: Fact, bytes: &'a [u8]) -> Result<(), HeldError> {
        let refused = |error: FormatError| HeldError { fact, error };
        let key = match fact {
            Fact::FileItems => FileItemsView::read(bytes).map_err(refused)?.file(),
            Fact::ModuleDecls => ModuleDeclsView::read(bytes).map_err(refused)?.module().0,
            Fact::ModuleScope => ScopeView::read(bytes).map_err(refused)?.module().0,
            Fact::TextualEntry => TextualEntryView::read(bytes).map_err(refused)?.module().0,
            Fact::ModuleFile => ModuleFileView::read(bytes).map_err(refused)?.module().0,
            Fact::CrateRoot => CrateRootView::read(bytes).map_err(refused)?.head().krate.0,
            Fact::MacroDef => MacroDefView::read(bytes).map_err(refused)?.head().item.0,
            Fact::KeyIndex => KeyIndexView::read(bytes).map_err(refused)?.module().0,
            Fact::LangItems => LangItemsView::read(bytes).map_err(refused)?.krate().0,
            Fact::CrateGraph => {
                CrateGraphView::read(bytes).map_err(refused)?;
                self.krate.0
            }
            Fact::NoImplsOf => NoImplsOfView::read(bytes).map_err(refused)?.krate().0,
        };
        self.records.insert((fact, key), bytes);
        Ok(())
    }

    /// Hold the text of `file`, for a task that compiles this crate.
    pub fn insert_text(&mut self, file: ContentId, text: &'a str) {
        self.texts.insert(file, text);
    }

    fn record(&self, fact: Fact, key: ContentId) -> Option<&'a [u8]> {
        self.records.get(&(fact, key)).copied()
    }

    fn of_item(&self, fact: Fact, item: ItemKey) -> Result<&'a [u8], SurfaceError> {
        if !item.is_of(self.krate) {
            return Err(NoItem { krate: self.krate, key: item }.into());
        }
        self.record(fact, item.0).ok_or(SurfaceError::NoRecord { fact: fact.name(), item })
    }

    fn of_crate(&self, fact: Fact, krate: CrateItemKey) -> Result<&'a [u8], SurfaceError> {
        self.record(fact, krate.0).ok_or(SurfaceError::NoCrateRecord { fact: fact.name(), krate })
    }
}

/// A record checked by [`TaskItems::insert`] opens again without failing: the second open
/// re-derives the view (blaze-format checks on open; blaze-surface has no unchecked open), so
/// the error is unreachable and named so.
fn reopened<T>(view: Result<T, FormatError>) -> T {
    match view {
        Ok(view) => view,
        Err(error) => panic!("a record TaskItems checked on insert failed to open again: {error}"),
    }
}

impl<'a> Items for TaskItems<'a, '_> {
    fn entry(&self, item: ItemKey) -> Result<KeyEntry, SurfaceError> {
        let missing = NoItem { krate: self.krate, key: item };
        if !item.is_of(self.krate) {
            return Err(missing.into());
        }
        let lo = (Fact::KeyIndex, ContentId([0; 32]));
        let hi = (Fact::KeyIndex, ContentId([0xff; 32]));
        self.records
            .range(lo..=hi)
            .find_map(|(_, bytes)| reopened(KeyIndexView::read(bytes)).get(item))
            .ok_or(missing.into())
    }

    fn file_items(&self, file: ContentId) -> Result<FileItemsView<'_>, SurfaceError> {
        let bytes = self
            .record(Fact::FileItems, file)
            .ok_or(SurfaceError::NoFileRecord { fact: Fact::FileItems.name(), file })?;
        Ok(reopened(FileItemsView::read(bytes)))
    }

    fn decls(&self, module: ItemKey) -> Result<ModuleDeclsView<'_>, SurfaceError> {
        Ok(reopened(ModuleDeclsView::read(self.of_item(Fact::ModuleDecls, module)?)))
    }

    fn scope(&self, module: ItemKey) -> Result<ScopeView<'_>, SurfaceError> {
        Ok(reopened(ScopeView::read(self.of_item(Fact::ModuleScope, module)?)))
    }

    fn textual_entry(&self, module: ItemKey) -> Result<TextualEntryView<'_>, SurfaceError> {
        Ok(reopened(TextualEntryView::read(self.of_item(Fact::TextualEntry, module)?)))
    }

    fn module_file(&self, module: ItemKey) -> Result<ModuleFileView<'_>, SurfaceError> {
        Ok(reopened(ModuleFileView::read(self.of_item(Fact::ModuleFile, module)?)))
    }

    fn macro_def(&self, item: ItemKey) -> Result<MacroDefView<'_>, SurfaceError> {
        Ok(reopened(MacroDefView::read(self.of_item(Fact::MacroDef, item)?)))
    }

    fn crate_root(&self, krate: CrateItemKey) -> Result<CrateRootView<'_>, SurfaceError> {
        Ok(reopened(CrateRootView::read(self.of_crate(Fact::CrateRoot, krate)?)))
    }

    fn lang_items(&self, krate: CrateItemKey) -> Result<LangItemsView<'_>, SurfaceError> {
        Ok(reopened(LangItemsView::read(self.of_crate(Fact::LangItems, krate)?)))
    }

    fn crate_graph(&self, krate: CrateItemKey) -> Result<CrateGraphView<'_>, SurfaceError> {
        Ok(reopened(CrateGraphView::read(self.of_crate(Fact::CrateGraph, krate)?)))
    }

    fn no_impls_of(&self, krate: CrateItemKey) -> Result<NoImplsOfView<'_>, SurfaceError> {
        Ok(reopened(NoImplsOfView::read(self.of_crate(Fact::NoImplsOf, krate)?)))
    }
}

impl<'a> Source for TaskItems<'a, '_> {
    fn krate(&self) -> CrateItemKey {
        self.krate
    }

    fn text(&self, proof: HasText, file: ContentId) -> Result<&str, SurfaceError> {
        self.check(proof)?;
        self.texts.get(&file).copied().ok_or(SurfaceError::NoFileRecord { fact: "text", file })
    }
}
