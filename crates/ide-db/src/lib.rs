//! This crate defines the core data structure representing IDE state -- `RootDatabase`.
//!
//! It is mainly a `HirDatabase` for semantic analysis, plus a `SymbolsDatabase`, for fuzzy search.

#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]

#[cfg(feature = "in-rust-tree")]
extern crate rustc_driver as _;

extern crate self as ide_db;

mod apply_change;

pub mod active_parameter;
pub mod assists;
pub mod defs;
pub mod documentation;
pub mod famous_defs;
pub mod helpers;
pub mod items_locator;
pub mod label;
pub mod path_transform;
pub mod prime_caches;
pub mod ra_fixture;
pub mod range_mapper;
pub mod rename;
pub mod rust_doc;
pub mod search;
pub mod source_change;
pub mod symbol_index;
pub mod text_edit;
pub mod traits;
pub mod ty_filter;
pub mod use_trivial_constructor;

pub mod imports {
    pub mod import_assets;
    pub mod insert_use;
    pub mod merge_imports;
}

pub mod generated {
    pub mod lints;
}

pub mod syntax_helpers {
    pub mod format_string;
    pub mod format_string_exprs;
    pub mod tree_diff;
    pub use hir::prettify_macro_expansion;
    pub mod node_ext;
    pub mod suggest_name;

    pub use parser::LexedStr;
}

pub use hir::{ChangeWithProcMacros, EditionedFileId, const_macro};
use salsa::Durability;

use std::{fmt, mem::ManuallyDrop};

use base_db::{
    CrateGraphBuilder, CratesMap, FileSourceRootInput, FileText, Files, Nonce, SourceDatabase,
    SourceRoot, SourceRootId, SourceRootInput, set_all_crates_with_durability,
};
use hir::{FilePositionWrapper, FileRangeWrapper, db::HirDatabase};
use triomphe::Arc;

use crate::line_index::LineIndex;
pub use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

pub use ::line_index;

/// `base_db` is normally also needed in places where `ide_db` is used, so this re-export is for convenience.
pub use base_db::{self, FxIndexMap, FxIndexSet, LibraryRoots, LocalRoots};
pub use span::{self, FileId};

pub type FilePosition = FilePositionWrapper<FileId>;
pub type FileRange = FileRangeWrapper<FileId>;

#[salsa::db]
pub struct RootDatabase {
    // FIXME: Revisit this commit now that we migrated to the new salsa, given we store arcs in this
    // db directly now
    // We use `ManuallyDrop` here because every codegen unit that contains a
    // `&RootDatabase -> &dyn OtherDatabase` cast will instantiate its drop glue in the vtable,
    // which duplicates `Weak::drop` and `Arc::drop` tens of thousands of times, which makes
    // compile times of all `ide_*` and downstream crates suffer greatly.
    storage: ManuallyDrop<salsa::Storage<Self>>,
    files: Arc<Files>,
    crates_map: Arc<CratesMap>,
    nonce: Nonce,
}

impl std::panic::RefUnwindSafe for RootDatabase {}

#[salsa::db]
impl salsa::Database for RootDatabase {}

impl Drop for RootDatabase {
    fn drop(&mut self) {
        unsafe { ManuallyDrop::drop(&mut self.storage) };
    }
}

impl Clone for RootDatabase {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            files: self.files.clone(),
            crates_map: self.crates_map.clone(),
            nonce: self.nonce,
        }
    }
}

impl fmt::Debug for RootDatabase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootDatabase").finish()
    }
}

impl RootDatabase {
    /// The files outside the VFS that an `include_bytes!` or `include_str!` has read.
    pub fn included_files(&self) -> Vec<vfs::AbsPathBuf> {
        self.files.included_files()
    }

    /// The files outside the VFS that `include_bytes!` and `include_str!` in `krate` have read,
    /// with the bytes read of each.
    pub fn included_by(
        &self,
        krate: base_db::Crate,
    ) -> Vec<(vfs::AbsPathBuf, Option<std::sync::Arc<[u8]>>)> {
        self.files.included_by(self, krate)
    }

    /// The files an `include!` named that the VFS has not, each with the source root of the
    /// file that includes it.
    pub fn included_sources(&self) -> Vec<(vfs::AbsPathBuf, SourceRootId)> {
        self.files.included_sources()
    }

    pub fn sources_included_by(&self) -> Vec<(base_db::Crate, vfs::AbsPathBuf)> {
        self.files.sources_included_by()
    }

    /// See [`base_db::Files::set_view`].
    pub fn set_view(
        &self,
        base: &std::path::Path,
        over: &std::path::Path,
        built: Vec<std::path::PathBuf>,
    ) {
        self.files.set_view(base, over, built);
    }

    /// The variables `env!` and `option_env!` have asked for, by the crate asking.
    pub fn env_read(&self) -> Vec<(base_db::Crate, String)> {
        self.files.env_read()
    }

    /// The variables `env!` has found unset, by the crate asking.
    pub fn env_unset(&self) -> Vec<(base_db::Crate, String)> {
        self.files.env_unset()
    }

    /// Has `krate` be compiled with `env`.
    pub fn set_env(&mut self, krate: base_db::Crate, env: base_db::Env) {
        use salsa::Setter as _;
        krate.set_env(self).to(env);
    }

    /// See [`base_db::Files::on_disk`].
    pub fn on_disk(&self, path: &std::path::Path) -> std::path::PathBuf {
        self.files.on_disk(path)
    }

    /// Whether such a file is on disk what was read of it.
    pub fn included_is_read(&self, path: &vfs::AbsPath) -> bool {
        self.files.included_is_read(self, path)
    }

    /// Reads such a file again, after it changed on disk.
    pub fn reload_included_file(&mut self, path: &vfs::AbsPath) -> bool {
        let files = Arc::clone(&self.files);
        files.reload_included_file(self, path)
    }
}

#[salsa::db]
impl SourceDatabase for RootDatabase {
    fn file_text(&self, file_id: vfs::FileId) -> FileText {
        self.files.file_text(file_id)
    }

    fn has_file_text(&self, file_id: vfs::FileId) -> bool {
        self.files.has_file_text(file_id)
    }

    fn set_file_text(&mut self, file_id: vfs::FileId, text: &str) {
        let files = Arc::clone(&self.files);
        files.set_file_text(self, file_id, text);
    }

    fn set_file_text_with_durability(
        &mut self,
        file_id: vfs::FileId,
        text: &str,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_file_text_with_durability(self, file_id, text, durability);
    }

    /// Source root of the file.
    fn source_root(&self, source_root_id: SourceRootId) -> SourceRootInput {
        self.files.source_root(source_root_id)
    }

    fn set_source_root_with_durability(
        &mut self,
        source_root_id: SourceRootId,
        source_root: Arc<SourceRoot>,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_source_root_with_durability(self, source_root_id, source_root, durability);
    }

    fn included_file(
        &self,
        krate: base_db::Crate,
        path: base_db::AnchoredPath<'_>,
    ) -> Option<base_db::IncludedFile> {
        let root = self.file_source_root(path.anchor).source_root_id(self);
        let root = self.source_root(root).source_root(self);
        let anchor = root.path_for_file(&path.anchor)?.as_path()?;
        let durability = if root.is_library { Durability::HIGH } else { Durability::LOW };
        let path = anchor.parent()?.absolutize(path.path);
        Some(self.files.included_file(self, krate, &path, durability))
    }

    fn env_is_read(&self, krate: base_db::Crate, key: &str) {
        self.files.env_is_read(krate, key);
    }

    fn env_is_unset(&self, krate: base_db::Crate, key: &str) {
        self.files.env_is_unset(krate, key);
    }

    fn include_source(&self, path: base_db::AnchoredPath<'_>, krate: base_db::Crate) {
        let root_id = self.file_source_root(path.anchor).source_root_id(self);
        let root = self.source_root(root_id).source_root(self);
        if let Some(anchor) = root.path_for_file(&path.anchor).and_then(|it| it.as_path())
            && let Some(parent) = anchor.parent()
        {
            self.files.include_source(&parent.absolutize(path.path), root_id, krate);
        }
    }

    fn file_source_root(&self, id: vfs::FileId) -> FileSourceRootInput {
        self.files.file_source_root(self, id)
    }

    fn set_file_source_root_with_durability(
        &mut self,
        id: vfs::FileId,
        source_root_id: SourceRootId,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_file_source_root_with_durability(self, id, source_root_id, durability);
    }

    fn crates_map(&self) -> Arc<CratesMap> {
        self.crates_map.clone()
    }

    fn nonce_and_revision(&self) -> (Nonce, salsa::Revision) {
        (self.nonce, salsa::plumbing::ZalsaDatabase::zalsa(self).current_revision())
    }

    fn line_column(&self, file: FileId, offset: syntax::TextSize) -> Result<(u32, u32), ()> {
        line_index(self, file).try_line_col(offset).map(|lc| (lc.line, lc.col)).ok_or(())
    }
}

hir::impl_hir_database!(RootDatabase);

/// Counts how often each query ran, when `RUSTD_TRACE` is set.
pub mod executions {
    use std::sync::Mutex;

    use rustc_hash::FxHashMap;
    use salsa::{Database, Event, EventKind, IngredientIndex};

    static RAN: Mutex<Option<FxHashMap<IngredientIndex, u32>>> = Mutex::new(None);

    pub(super) fn callback() -> Option<Box<dyn Fn(Event) + Send + Sync>> {
        std::env::var_os("RUSTD_TRACE")?;
        Some(Box::new(|event| {
            if let EventKind::WillExecute { database_key } = event.kind {
                let mut ran = RAN.lock().unwrap_or_else(|it| it.into_inner());
                *ran.get_or_insert_default().entry(database_key.ingredient_index()).or_default() +=
                    1;
                let mut order = ORDER.lock().unwrap_or_else(|it| it.into_inner());
                if order.len() < 300 {
                    order.push(database_key.ingredient_index());
                }
            }
        }))
    }

    static ORDER: Mutex<Vec<IngredientIndex>> = Mutex::new(Vec::new());

    /// The first queries that ran since the last call, in the order they started.
    pub fn order(db: &dyn Database) -> Vec<String> {
        let order = std::mem::take(&mut *ORDER.lock().unwrap_or_else(|it| it.into_inner()));
        order.into_iter().map(|query| db.ingredient_debug_name(query).into_owned()).collect()
    }

    /// The queries that ran since the last call, most often first.
    pub fn take(db: &dyn Database) -> Vec<(String, u32)> {
        let ran = RAN.lock().unwrap_or_else(|it| it.into_inner()).take().unwrap_or_default();
        let mut ran: Vec<_> = ran
            .into_iter()
            .map(|(query, count)| (db.ingredient_debug_name(query).into_owned(), count))
            .collect();
        ran.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ran
    }
}

impl Default for RootDatabase {
    fn default() -> RootDatabase {
        RootDatabase::new(None)
    }
}

impl RootDatabase {
    pub fn new(lru_capacity: Option<u16>) -> RootDatabase {
        let mut db = RootDatabase {
            storage: ManuallyDrop::new(salsa::Storage::new(executions::callback())),
            files: Default::default(),
            crates_map: Default::default(),
            nonce: Nonce::new(),
        };
        // This needs to be here otherwise `CrateGraphBuilder` will panic.
        set_all_crates_with_durability(&mut db, std::iter::empty(), Durability::HIGH);
        CrateGraphBuilder::default().set_in_db(&mut db);
        hir::ProcMacros::init_default(&db, Durability::MEDIUM);
        _ = base_db::LibraryRoots::builder(Default::default())
            .durability(Durability::MEDIUM)
            .new(&db);
        _ = base_db::LocalRoots::builder(Default::default())
            .durability(Durability::MEDIUM)
            .new(&db);
        hir::db::set_expand_proc_attr_macros(&mut db, false);
        db.update_base_query_lru_capacities(lru_capacity);
        db
    }

    pub fn enable_proc_attr_macros(&mut self) {
        hir::db::set_expand_proc_attr_macros(self, true);
    }

    pub fn update_base_query_lru_capacities(&mut self, _lru_capacity: Option<u16>) {
        // let lru_capacity = lru_capacity.unwrap_or(base_db::DEFAULT_PARSE_LRU_CAP);
        // base_db::FileTextQuery.in_db_mut(self).set_lru_capacity(DEFAULT_FILE_TEXT_LRU_CAP);
        // base_db::ParseQuery.in_db_mut(self).set_lru_capacity(lru_capacity);
        // // macro expansions are usually rather small, so we can afford to keep more of them alive
        // hir::db::ParseMacroExpansionQuery.in_db_mut(self).set_lru_capacity(4 * lru_capacity);
        // hir::db::BorrowckQuery.in_db_mut(self).set_lru_capacity(base_db::DEFAULT_BORROWCK_LRU_CAP);
        // hir::db::BodyWithSourceMapQuery.in_db_mut(self).set_lru_capacity(2048);
    }

    pub fn update_lru_capacities(&mut self, _lru_capacities: &FxHashMap<Box<str>, u16>) {
        // FIXME(salsa-transition): bring this back; allow changing LRU settings at runtime.
        // use hir::db as hir_db;

        // base_db::FileTextQuery.in_db_mut(self).set_lru_capacity(DEFAULT_FILE_TEXT_LRU_CAP);
        // base_db::ParseQuery.in_db_mut(self).set_lru_capacity(
        //     lru_capacities
        //         .get(stringify!(ParseQuery))
        //         .copied()
        //         .unwrap_or(base_db::DEFAULT_PARSE_LRU_CAP),
        // );
        // hir_db::ParseMacroExpansionQuery.in_db_mut(self).set_lru_capacity(
        //     lru_capacities
        //         .get(stringify!(ParseMacroExpansionQuery))
        //         .copied()
        //         .unwrap_or(4 * base_db::DEFAULT_PARSE_LRU_CAP),
        // );
        // hir_db::BorrowckQuery.in_db_mut(self).set_lru_capacity(
        //     lru_capacities
        //         .get(stringify!(BorrowckQuery))
        //         .copied()
        //         .unwrap_or(base_db::DEFAULT_BORROWCK_LRU_CAP),
        // );
        // hir::db::BodyWithSourceMapQuery.in_db_mut(self).set_lru_capacity(2048);
    }
}

pub fn line_index(db: &dyn SourceDatabase, file_id: FileId) -> &Arc<LineIndex> {
    #[salsa::interned]
    pub struct InternedFileId {
        #[returns(copy)]
        id: FileId,
    }
    #[salsa::tracked(returns(ref))]
    fn line_index<'db>(
        db: &'db dyn SourceDatabase,
        file_id: InternedFileId<'db>,
    ) -> Arc<LineIndex> {
        let text = db.file_text(file_id.id(db)).text(db);
        Arc::new(LineIndex::new(text))
    }
    line_index(db, InternedFileId::new(db, file_id))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SymbolKind {
    Attribute,
    BuiltinAttr,
    Const,
    ConstParam,
    CrateRoot,
    Derive,
    DeriveHelper,
    Enum,
    Field,
    Function,
    Method,
    Impl,
    InlineAsmRegOrRegClass,
    Label,
    LifetimeParam,
    Local,
    Macro,
    ProcMacro,
    Module,
    SelfParam,
    SelfType,
    Static,
    Struct,
    ToolModule,
    Trait,
    TypeAlias,
    TypeParam,
    Union,
    ValueParam,
    Variant,
}

impl From<hir::MacroKind> for SymbolKind {
    fn from(it: hir::MacroKind) -> Self {
        match it {
            hir::MacroKind::Declarative | hir::MacroKind::DeclarativeBuiltIn => SymbolKind::Macro,
            hir::MacroKind::ProcMacro => SymbolKind::ProcMacro,
            hir::MacroKind::Derive | hir::MacroKind::DeriveBuiltIn => SymbolKind::Derive,
            hir::MacroKind::Attr | hir::MacroKind::AttrBuiltIn => SymbolKind::Attribute,
        }
    }
}

impl SymbolKind {
    pub fn from_module_def(db: &dyn HirDatabase, it: hir::ModuleDef) -> Self {
        match it {
            hir::ModuleDef::Const(..) => SymbolKind::Const,
            hir::ModuleDef::EnumVariant(..) => SymbolKind::Variant,
            hir::ModuleDef::Function(..) => SymbolKind::Function,
            hir::ModuleDef::Macro(mac) if mac.is_proc_macro() => SymbolKind::ProcMacro,
            hir::ModuleDef::Macro(..) => SymbolKind::Macro,
            hir::ModuleDef::Module(m) if m.is_crate_root(db) => SymbolKind::CrateRoot,
            hir::ModuleDef::Module(..) => SymbolKind::Module,
            hir::ModuleDef::Static(..) => SymbolKind::Static,
            hir::ModuleDef::Adt(hir::Adt::Struct(..)) => SymbolKind::Struct,
            hir::ModuleDef::Adt(hir::Adt::Enum(..)) => SymbolKind::Enum,
            hir::ModuleDef::Adt(hir::Adt::Union(..)) => SymbolKind::Union,
            hir::ModuleDef::Trait(..) => SymbolKind::Trait,
            hir::ModuleDef::TypeAlias(..) => SymbolKind::TypeAlias,
            hir::ModuleDef::BuiltinType(..) => SymbolKind::TypeAlias,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnippetCap {
    _private: (),
}

impl SnippetCap {
    pub const fn new(allow_snippets: bool) -> Option<SnippetCap> {
        if allow_snippets { Some(SnippetCap { _private: () }) } else { None }
    }
}

pub struct Ranker<'a> {
    pub kind: parser::SyntaxKind,
    pub text: &'a str,
    pub ident_kind: bool,
}

impl<'a> Ranker<'a> {
    pub const MAX_RANK: usize = 0b1110;

    pub fn from_token(token: &'a syntax::SyntaxToken) -> Self {
        let kind = token.kind();
        Ranker { kind, text: token.text(), ident_kind: kind.is_any_identifier() }
    }

    /// A utility function that ranks a token again a given kind and text, returning a number that
    /// represents how close the token is to the given kind and text.
    pub fn rank_token(&self, tok: &syntax::SyntaxToken) -> usize {
        let tok_kind = tok.kind();

        let exact_same_kind = tok_kind == self.kind;
        let both_idents = exact_same_kind || (tok_kind.is_any_identifier() && self.ident_kind);
        let same_text = tok.text() == self.text;
        // anything that mapped into a token tree has likely no semantic information
        let no_tt_parent =
            tok.parent().is_some_and(|it| it.kind() != parser::SyntaxKind::TOKEN_TREE);
        (both_idents as usize)
            | ((exact_same_kind as usize) << 1)
            | ((same_text as usize) << 2)
            | ((no_tt_parent as usize) << 3)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Severity {
    Error,
    Warning,
    WeakWarning,
    Allow,
}

#[derive(Clone, Copy)]
pub struct MiniCore<'a>(&'a str);

impl<'a> MiniCore<'a> {
    #[inline]
    pub fn new(minicore: &'a str) -> Self {
        Self(minicore)
    }

    #[inline]
    pub const fn default() -> Self {
        Self(test_utils::MiniCore::RAW_SOURCE)
    }
}

impl std::fmt::Debug for MiniCore<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_tuple("MiniCore");
        if self.0 == test_utils::MiniCore::RAW_SOURCE {
            // Don't print the whole contents if they correspond to the default.
            // The `format_args!` makes it so that the output is
            // `MiniCore(<default>)` and not `MiniCore("<default>").
            d.field(&format_args!("<default>"));
        } else {
            d.field(&self.0);
        };
        d.finish()
    }
}

impl<'a> Default for MiniCore<'a> {
    #[inline]
    fn default() -> Self {
        Self::default()
    }
}
