//! [`Records`]: the concrete surface a [`crate::Db`] asks, every crate's records of one task's
//! snapshot and the text of the crate it compiles (docs/v2/plan/design/reconnect.md, "The
//! database").
//!
//! The driver fills it from the store's packs before the task runs ([`Records::insert`]), each
//! buffer checked once there, and seals it ([`Records::seal`]), which sorts the tables so a
//! question is a binary search. It implements [`Items`] for every crate and [`Source`] for the
//! crate whose text the driver opened (it holds that crate's [`HasText`]); a text question
//! with another proof is refused by [`Source::check`].
//!
//! It belongs in blaze-surface beside `MemSurface` once the store's reader is (its one home);
//! it is here until then because the database must name a concrete type
//! (docs/v2/plan/design/reconnect.md, "What the new-layer crates lack").

use std::alloc::Global;

use blaze_format::{Aligned, FormatError};
use blaze_surface::{
    ContentId, CrateGraphView, CrateItemKey, CrateRootView, FileItemsView, HasText, ItemKey,
    Items, KeyEntry, KeyIndexView, LangItemsView, MacroDefView, ModuleDeclsView, ModuleFileView,
    NoImplsOfView, ScopeView, Source, SurfaceError, TextualEntryView,
};

/// Which record a buffer is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum RecordKind {
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

impl RecordKind {
    /// The record's name, as a [`SurfaceError`] names the fact.
    pub fn name(self) -> &'static str {
        match self {
            RecordKind::FileItems => "FileItems",
            RecordKind::ModuleDecls => "ModuleDecls",
            RecordKind::ModuleScope => "ModuleScope",
            RecordKind::TextualEntry => "TextualEntry",
            RecordKind::ModuleFile => "ModuleFile",
            RecordKind::CrateRoot => "CrateRoot",
            RecordKind::MacroDef => "MacroDef",
            RecordKind::KeyIndex => "KeyIndex",
            RecordKind::LangItems => "LangItems",
            RecordKind::CrateGraph => "CrateGraph",
            RecordKind::NoImplsOf => "NoImplsOf",
        }
    }
}

/// Why [`Records::insert`] refused a buffer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InsertError {
    /// The bytes are not a record of the kind given.
    Format { kind: RecordKind, error: FormatError },
    /// A second record of one kind and key.
    Twice { kind: RecordKind, key: ContentId },
    /// A second text of one file.
    TextTwice { file: ContentId },
    /// An insert after [`Records::seal`].
    Sealed,
}

impl std::fmt::Display for InsertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InsertError::Format { kind, error } => {
                write!(f, "the bytes given as a {} record are not one: {error}", kind.name())
            }
            InsertError::Twice { kind, key } => {
                write!(f, "a second {} record of {key:x}", kind.name())
            }
            InsertError::TextTwice { file } => write!(f, "a second text of the file {file:x}"),
            InsertError::Sealed => f.write_str("a record inserted after the records were sealed"),
        }
    }
}

impl std::error::Error for InsertError {}

/// Every record of one task's snapshot, read in place.
pub struct Records {
    /// By (kind, key), sorted at [`Records::seal`].
    records: Vec<(RecordKind, ContentId, Aligned), Global>,
    /// Each item key of every `KeyIndex` page, to the page's row in `records`: one probe for
    /// [`Items::entry`].
    pages: Vec<(ItemKey, u32), Global>,
    /// The text of the files of the crate `text` names, by file content, sorted.
    texts: Vec<(ContentId, syntax::Text), Global>,
    text: Option<HasText>,
    sealed: bool,
}

/// The key the snapshot's one `CrateGraph` is held under: the graph is the build's, of every
/// crate in it, so no crate's key is its key.
const GRAPH_KEY: ContentId = ContentId([0; 32]);

/// The view of a buffer [`Records::insert`] checked.
fn checked<T>(kind: RecordKind, view: Result<T, FormatError>) -> T {
    view.unwrap_or_else(|error| {
        panic!("a {} record was checked when it was inserted: {error}", kind.name())
    })
}

impl Records {
    /// No records; `text` is the proof of the crate whose text the driver opened for the task,
    /// `None` for a task that holds only records.
    pub fn new(text: Option<HasText>) -> Records {
        Records {
            records: Vec::new_in(Global),
            pages: Vec::new_in(Global),
            texts: Vec::new_in(Global),
            text,
            sealed: false,
        }
    }

    /// Holds `bytes`, a record of `kind`, under the key its view names.
    pub fn insert(&mut self, kind: RecordKind, bytes: &[u8]) -> Result<(), InsertError> {
        if self.sealed {
            return Err(InsertError::Sealed);
        }
        let buffer = Aligned::copy_from(bytes);
        let format = |error| InsertError::Format { kind, error };
        let key = match kind {
            RecordKind::FileItems => FileItemsView::read(&buffer).map_err(format)?.file(),
            RecordKind::ModuleDecls => ModuleDeclsView::read(&buffer).map_err(format)?.module().0,
            RecordKind::ModuleScope => ScopeView::read(&buffer).map_err(format)?.module().0,
            RecordKind::TextualEntry => TextualEntryView::read(&buffer).map_err(format)?.module().0,
            RecordKind::ModuleFile => ModuleFileView::read(&buffer).map_err(format)?.module().0,
            RecordKind::CrateRoot => CrateRootView::read(&buffer).map_err(format)?.head().krate.0,
            RecordKind::MacroDef => MacroDefView::read(&buffer).map_err(format)?.head().item.0,
            RecordKind::KeyIndex => KeyIndexView::read(&buffer).map_err(format)?.module().0,
            RecordKind::LangItems => LangItemsView::read(&buffer).map_err(format)?.krate().0,
            RecordKind::CrateGraph => {
                            // One graph per snapshot, the build's: held under the one key no content
                            // has, a second refused as `Twice`.
                            CrateGraphView::read(&buffer).map_err(format)?;
                            GRAPH_KEY
                        },
            RecordKind::NoImplsOf => NoImplsOfView::read(&buffer).map_err(format)?.krate().0,
        };
        self.records.push((kind, key, buffer));
        Ok(())
    }

    /// Holds `text` as the text of the file with content `file`, of the crate whose proof the
    /// records were made with.
    pub fn insert_text(&mut self, file: ContentId, text: syntax::Text) -> Result<(), InsertError> {
        if self.sealed {
            return Err(InsertError::Sealed);
        }
        self.texts.push((file, text));
        Ok(())
    }

    /// The text of a file of the crate `proof` names, shared as the parser takes it.
    pub fn shared_text(&self, proof: HasText, file: ContentId) -> Result<&syntax::Text, SurfaceError> {
        self.check(proof)?;
        let at = self
            .texts
            .binary_search_by_key(&file, |(at, _)| *at)
            .map_err(|_| SurfaceError::NoFileRecord { fact: "text", file })?;
        Ok(&self.texts[at].1)
    }

    /// Sorts the tables: after it, no insert, and every question is a binary search.
    pub fn seal(&mut self) -> Result<(), InsertError> {
        self.records.sort_unstable_by_key(|(kind, key, _)| (*kind, *key));
        if let Some(pair) =
            self.records.windows(2).find(|pair| (pair[0].0, pair[0].1) == (pair[1].0, pair[1].1))
        {
            return Err(InsertError::Twice { kind: pair[1].0, key: pair[1].1 });
        }
        self.texts.sort_unstable_by_key(|(file, _)| *file);
        if let Some(pair) = self.texts.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(InsertError::TextTwice { file: pair[1].0 });
        }
        let mut pages = Vec::new_in(Global);
        for (row, (kind, _, buffer)) in self.records.iter().enumerate() {
            if *kind != RecordKind::KeyIndex {
                continue;
            }
            let row = u32::try_from(row).expect("fewer than 2^32 records in one snapshot");
            pages.extend(checked(*kind, KeyIndexView::read(buffer)).entries().map(|it| (it.key, row)));
        }
        pages.sort_unstable_by_key(|(key, _)| *key);
        self.pages = pages;
        self.sealed = true;
        Ok(())
    }

    fn record(&self, kind: RecordKind, key: ContentId) -> Option<&[u8]> {
        debug_assert!(self.sealed, "a question asked of records not yet sealed");
        let at = self.records.binary_search_by(|(k, at, _)| (*k, *at).cmp(&(kind, key))).ok()?;
        Some(&self.records[at].2[..])
    }

    fn of_item(&self, kind: RecordKind, item: ItemKey) -> Result<&[u8], SurfaceError> {
        self.record(kind, item.0).ok_or(SurfaceError::NoRecord { fact: kind.name(), item })
    }

    fn of_crate(&self, kind: RecordKind, krate: CrateItemKey) -> Result<&[u8], SurfaceError> {
        self.record(kind, krate.0).ok_or(SurfaceError::NoCrateRecord { fact: kind.name(), krate })
    }

    /// The proof this snapshot holds, if it holds a crate's text.
    pub fn text_proof(&self) -> Option<HasText> {
        self.text
    }

    /// The `KeyIndex` page of `module`: its items and the items inside them.
    pub fn page(&self, module: ItemKey) -> Result<KeyIndexView<'_>, SurfaceError> {
        let buffer = self.of_item(RecordKind::KeyIndex, module)?;
        Ok(checked(RecordKind::KeyIndex, KeyIndexView::read(buffer)))
    }

    /// The items whose parent is `parent` (a module's items, a trait's or an impl's associated
    /// items, an enum's variants, a struct's fields), in the order they are written: one read of
    /// the page of `module`, the module `parent` is in (or `parent` itself for a module). Pushed
    /// onto `out`, which the caller owns, so a walk reuses one buffer. blaze-surface has no
    /// `AssocItems` record yet (reconnect.md, "What the new-layer crates lack"): this is its
    /// stand-in over the page that already lists every member with its parent.
    pub fn members(
        &self,
        module: ItemKey,
        parent: ItemKey,
        out: &mut Vec<(blaze_surface::Place, ItemKey), Global>,
    ) -> Result<(), SurfaceError> {
        let start = out.len();
        out.extend(
            self.page(module)?
                .entries()
                .filter(|entry| entry.parent == Some(parent))
                .map(|entry| (entry.place, entry.key)),
        );
        // A file's rows are in source order, so the place orders the members as written.
        out[start..].sort_unstable();
        Ok(())
    }
}

impl Items for Records {
    fn entry(&self, item: ItemKey) -> Result<KeyEntry, SurfaceError> {
        let Ok(at) = self.pages.binary_search_by_key(&item, |(key, _)| *key) else {
            // No page holds it: the page of its module is a record its crate's task makes.
            return Err(SurfaceError::NoRecord { fact: RecordKind::KeyIndex.name(), item });
        };
        let row = self.pages[at].1 as usize;
        let view = checked(RecordKind::KeyIndex, KeyIndexView::read(&self.records[row].2));
        // The page was found by this key at `seal`, so it holds it.
        view.get(item).ok_or(SurfaceError::NoRecord { fact: RecordKind::KeyIndex.name(), item })
    }

    fn file_items(&self, file: ContentId) -> Result<FileItemsView<'_>, SurfaceError> {
        let kind = RecordKind::FileItems;
        let bytes =
            self.record(kind, file).ok_or(SurfaceError::NoFileRecord { fact: kind.name(), file })?;
        Ok(checked(kind, FileItemsView::read(bytes)))
    }

    fn decls(&self, module: ItemKey) -> Result<ModuleDeclsView<'_>, SurfaceError> {
        let kind = RecordKind::ModuleDecls;
        Ok(checked(kind, ModuleDeclsView::read(self.of_item(kind, module)?)))
    }

    fn scope(&self, module: ItemKey) -> Result<ScopeView<'_>, SurfaceError> {
        let kind = RecordKind::ModuleScope;
        Ok(checked(kind, ScopeView::read(self.of_item(kind, module)?)))
    }

    fn textual_entry(&self, module: ItemKey) -> Result<TextualEntryView<'_>, SurfaceError> {
        let kind = RecordKind::TextualEntry;
        Ok(checked(kind, TextualEntryView::read(self.of_item(kind, module)?)))
    }

    fn module_file(&self, module: ItemKey) -> Result<ModuleFileView<'_>, SurfaceError> {
        let kind = RecordKind::ModuleFile;
        Ok(checked(kind, ModuleFileView::read(self.of_item(kind, module)?)))
    }

    fn macro_def(&self, item: ItemKey) -> Result<MacroDefView<'_>, SurfaceError> {
        let kind = RecordKind::MacroDef;
        Ok(checked(kind, MacroDefView::read(self.of_item(kind, item)?)))
    }

    fn crate_root(&self, krate: CrateItemKey) -> Result<CrateRootView<'_>, SurfaceError> {
        let kind = RecordKind::CrateRoot;
        Ok(checked(kind, CrateRootView::read(self.of_crate(kind, krate)?)))
    }

    fn lang_items(&self, krate: CrateItemKey) -> Result<LangItemsView<'_>, SurfaceError> {
        let kind = RecordKind::LangItems;
        Ok(checked(kind, LangItemsView::read(self.of_crate(kind, krate)?)))
    }

    fn crate_graph(&self, krate: CrateItemKey) -> Result<CrateGraphView<'_>, SurfaceError> {
        let kind = RecordKind::CrateGraph;
        let missing = SurfaceError::NoCrateRecord { fact: kind.name(), krate };
        let graph = checked(kind, CrateGraphView::read(self.record(kind, GRAPH_KEY).ok_or(missing)?));
        // The build's graph, asked for a crate it holds; of any other crate there is none.
        graph.index_of(krate).ok_or(missing)?;
        Ok(graph)
    }

    fn no_impls_of(&self, krate: CrateItemKey) -> Result<NoImplsOfView<'_>, SurfaceError> {
        let kind = RecordKind::NoImplsOf;
        Ok(checked(kind, NoImplsOfView::read(self.of_crate(kind, krate)?)))
    }
}

impl Source for Records {
    /// The crate whose text this holds. Asked of records that hold none, it names no crate any
    /// proof is of, so every [`Source::check`] refuses.
    fn krate(&self) -> CrateItemKey {
        match self.text {
            Some(proof) => proof.krate(),
            None => CrateItemKey(ContentId([0; 32])),
        }
    }

    fn text(&self, proof: HasText, file: ContentId) -> Result<&str, SurfaceError> {
        Ok(self.shared_text(proof, file)?.as_str())
    }
}
