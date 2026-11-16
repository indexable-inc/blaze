//! [`MemSurface`]: an in-memory [`Items`] and [`Source`] of one crate over records built by
//! hand, for the tests of this crate and of the crates that ask through the two traits (the
//! `testing` feature).

use blaze_format::{Aligned, FormatError};
use blaze_id::{ContentId, CrateItemKey, ItemKey, NoItem};
use blaze_own::{AVec, Alloc};

use crate::crate_graph::CrateGraphView;
use crate::impl_index::NoImplsOfView;
use crate::lang_items::LangItemsView;
use crate::error::SurfaceError;
use crate::file_items::FileItemsView;
use crate::items::{Items, Source};
use crate::key_index::{KeyEntry, KeyIndexView};
use crate::module_decls::ModuleDeclsView;
use crate::module_scope::ScopeView;
use crate::macro_def::MacroDefView;
use crate::crate_root::CrateRootView;
use crate::names::{ModuleFileView, TextualEntryView};
use crate::proof::HasText;

/// Which record a buffer given to [`MemSurface::insert`] is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
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

/// The records and the file texts of one crate, held in memory. Each buffer is checked whole
/// when it is inserted; a question reads it in place again, the check standing (a test double:
/// the store's reader checks a segment once and hands out views with no check).
pub struct MemSurface<'a> {
    krate: CrateItemKey,
    records: AVec<'a, (Fact, ContentId, Aligned)>,
    texts: AVec<'a, (ContentId, AVec<'a, u8>)>,
    alloc: Alloc<'a>,
}

/// The view of a buffer [`MemSurface::insert`] checked.
fn checked<T>(view: Result<T, FormatError>) -> T {
    view.unwrap_or_else(|error| panic!("a record checked when it was inserted: {error}"))
}

impl<'a> MemSurface<'a> {
    /// An empty surface of the crate `krate`, its tables in `alloc`.
    pub fn new_in(krate: CrateItemKey, alloc: Alloc<'a>) -> MemSurface<'a> {
        MemSurface { krate, records: AVec::new_in(alloc), texts: AVec::new_in(alloc), alloc }
    }

    /// Holds `bytes`, a record of `fact`, under the key its view names (the file, the module,
    /// the macro or the crate), in place of any earlier one of that key. Refused when the bytes
    /// are not such a record.
    pub fn insert(&mut self, fact: Fact, bytes: &[u8]) -> Result<(), FormatError> {
        let buffer = Aligned::copy_from(bytes);
        let key = match fact {
            Fact::FileItems => FileItemsView::read(&buffer)?.file(),
            Fact::ModuleDecls => ModuleDeclsView::read(&buffer)?.module().0,
            Fact::ModuleScope => ScopeView::read(&buffer)?.module().0,
            Fact::TextualEntry => TextualEntryView::read(&buffer)?.module().0,
            Fact::ModuleFile => ModuleFileView::read(&buffer)?.module().0,
            Fact::CrateRoot => CrateRootView::read(&buffer)?.head().krate.0,
            Fact::MacroDef => MacroDefView::read(&buffer)?.head().item.0,
            Fact::KeyIndex => KeyIndexView::read(&buffer)?.module().0,
            Fact::LangItems => LangItemsView::read(&buffer)?.krate().0,
            // One graph per build, of every crate: filed under the crate this surface is of.
            Fact::CrateGraph => {
                CrateGraphView::read(&buffer)?;
                self.krate.0
            }
            Fact::NoImplsOf => NoImplsOfView::read(&buffer)?.krate().0,
        };
        self.records.retain(|(held, at, _)| (*held, *at) != (fact, key));
        self.records.push((fact, key, buffer));
        Ok(())
    }

    /// Holds `text` as the text of the file with content `file`.
    pub fn insert_text(&mut self, file: ContentId, text: &str) {
        self.texts.retain(|(at, _)| *at != file);
        let bytes = AVec::from_slice_in(text.as_bytes(), self.alloc);
        self.texts.push((file, bytes));
    }

    fn record(&self, fact: Fact, key: ContentId) -> Option<&[u8]> {
        self.records
            .iter()
            .find(|(held, at, _)| (*held, *at) == (fact, key))
            .map(|(_, _, buffer)| &buffer[..])
    }

    /// The record of `fact` of the item `item`, or why there is none: `NoItem` for a key of
    /// another crate, else `NoRecord`.
    fn of_item(&self, fact: Fact, item: ItemKey) -> Result<&[u8], SurfaceError> {
        if !item.is_of(self.krate) {
            return Err(NoItem { krate: self.krate, key: item }.into());
        }
        self.record(fact, item.0).ok_or(SurfaceError::NoRecord { fact: fact.name(), item })
    }

    fn of_crate(&self, fact: Fact, krate: CrateItemKey) -> Result<&[u8], SurfaceError> {
        self.record(fact, krate.0).ok_or(SurfaceError::NoCrateRecord { fact: fact.name(), krate })
    }
}

impl Items for MemSurface<'_> {
    fn entry(&self, item: ItemKey) -> Result<KeyEntry, SurfaceError> {
        let missing = NoItem { krate: self.krate, key: item };
        if !item.is_of(self.krate) {
            return Err(missing.into());
        }
        self.records
            .iter()
            .filter(|(fact, _, _)| *fact == Fact::KeyIndex)
            .find_map(|(_, _, buffer)| checked(KeyIndexView::read(buffer)).get(item))
            .ok_or(missing.into())
    }

    fn file_items(&self, file: ContentId) -> Result<FileItemsView<'_>, SurfaceError> {
        let bytes = self
            .record(Fact::FileItems, file)
            .ok_or(SurfaceError::NoFileRecord { fact: Fact::FileItems.name(), file })?;
        Ok(checked(FileItemsView::read(bytes)))
    }

    fn decls(&self, module: ItemKey) -> Result<ModuleDeclsView<'_>, SurfaceError> {
        Ok(checked(ModuleDeclsView::read(self.of_item(Fact::ModuleDecls, module)?)))
    }

    fn scope(&self, module: ItemKey) -> Result<ScopeView<'_>, SurfaceError> {
        Ok(checked(ScopeView::read(self.of_item(Fact::ModuleScope, module)?)))
    }

    fn textual_entry(&self, module: ItemKey) -> Result<TextualEntryView<'_>, SurfaceError> {
        Ok(checked(TextualEntryView::read(self.of_item(Fact::TextualEntry, module)?)))
    }

    fn module_file(&self, module: ItemKey) -> Result<ModuleFileView<'_>, SurfaceError> {
        Ok(checked(ModuleFileView::read(self.of_item(Fact::ModuleFile, module)?)))
    }

    fn macro_def(&self, item: ItemKey) -> Result<MacroDefView<'_>, SurfaceError> {
        Ok(checked(MacroDefView::read(self.of_item(Fact::MacroDef, item)?)))
    }

    fn crate_root(&self, krate: CrateItemKey) -> Result<CrateRootView<'_>, SurfaceError> {
        Ok(checked(CrateRootView::read(self.of_crate(Fact::CrateRoot, krate)?)))
    }

    fn lang_items(&self, krate: CrateItemKey) -> Result<LangItemsView<'_>, SurfaceError> {
        Ok(checked(LangItemsView::read(self.of_crate(Fact::LangItems, krate)?)))
    }

    fn crate_graph(&self, krate: CrateItemKey) -> Result<CrateGraphView<'_>, SurfaceError> {
        Ok(checked(CrateGraphView::read(self.of_crate(Fact::CrateGraph, krate)?)))
    }

    fn no_impls_of(&self, krate: CrateItemKey) -> Result<NoImplsOfView<'_>, SurfaceError> {
        Ok(checked(NoImplsOfView::read(self.of_crate(Fact::NoImplsOf, krate)?)))
    }
}

impl Source for MemSurface<'_> {
    fn krate(&self) -> CrateItemKey {
        self.krate
    }

    fn text(&self, proof: HasText, file: ContentId) -> Result<&str, SurfaceError> {
        self.check(proof)?;
        let (_, bytes) = self
            .texts
            .iter()
            .find(|(at, _)| *at == file)
            .ok_or(SurfaceError::NoFileRecord { fact: "text", file })?;
        Ok(core::str::from_utf8(bytes).expect("inserted as a str"))
    }
}
