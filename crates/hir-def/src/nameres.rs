//! This module implements import-resolution/macro expansion algorithm.
//!
//! The result of this module is `DefMap`: a data structure which contains:
//!
//!   * a tree of modules for the crate
//!   * for each module, a set of items visible in the module (directly declared
//!     or imported)
//!
//! Note that `DefMap` contains fully macro expanded code.
//!
//! Computing `DefMap` can be partitioned into several logically
//! independent "phases". The phases are mutually recursive though, there's no
//! strict ordering.
//!
//! ## Collecting RawItems
//!
//! This happens in the `raw` module, which parses a single source file into a
//! set of top-level items. Nested imports are desugared to flat imports in this
//! phase. Macro calls are represented as a triple of `(Path, Option<Name>,
//! TokenTree)`.
//!
//! ## Collecting Modules
//!
//! This happens in the `collector` module. In this phase, we recursively walk
//! tree of modules, collect raw items from submodules, populate module scopes
//! with defined items (so, we assign item ids in this phase) and record the set
//! of unresolved imports and macros.
//!
//! While we walk tree of modules, we also record macro_rules definitions and
//! expand calls to macro_rules defined macros.
//!
//! ## Resolving Imports
//!
//! We maintain a list of currently unresolved imports. On every iteration, we
//! try to resolve some imports from this list. If the import is resolved, we
//! record it, by adding an item to current module scope and, if necessary, by
//! recursively populating glob imports.
//!
//! ## Resolving Macros
//!
//! macro_rules from the same crate use a global mutable namespace. We expand
//! them immediately, when we collect modules.
//!
//! Macros from other crates (including proc-macros) can be used with
//! `foo::bar!` syntax. We handle them similarly to imports. There's a list of
//! unexpanded macros. On every iteration, we try to resolve each macro call
//! path and, upon success, we run macro expansion and "collect module" phase on
//! the result

pub mod assoc;
pub mod attr_resolution;
#[cfg(test)]
mod collector;
pub mod delta;
pub mod demand;
pub mod diagnostics;
mod facts;
pub use self::facts::kinds;
pub mod frozen;
pub use hir_expand::proc_macro::may_have_session_macros;
mod mod_resolution;
pub mod named;
mod path_resolution;
mod trait_items;
pub mod proc_macro;
pub mod uses;

#[cfg(test)]
mod demand_tests;
#[cfg(test)]
mod tests;

use std::{
    hash::Hash,
    ops::{Deref, DerefMut, Index, IndexMut},
};

use base_db::{Crate, Db};
use blaze_hash::HashMap;
use blaze_mem::Tag;
use either::Either;
use hir_expand::{
    EditionedFileId, ErasedAstId, HirFileId, InFile, MacroCallId, mod_path::ModPath, name::Name,
    proc_macro::ProcMacroKind,
};
use intern::{Symbol, sym};
use itertools::Itertools;
use smallvec::SmallVec;
use span::{Edition, FileAstId, FileId, ROOT_ERASED_FILE_AST_ID};
use stdx::format_to;
use syntax::{
    AstNode, SmolStr, TextSize, ToSmolStr, ast,
    view::{self, AstNode as _},
};
use triomphe::Arc;
use tt::TextRange;

use crate::{
    AstId, BlockId, BlockIdLt, BuiltinDeriveImplId, ExternCrateId, FunctionId, FxIndexMap, Lookup,
    MacroCallStyles, MacroExpander, MacroId, ModuleId, ModuleIdLt, ProcMacroId, UseId,
    builtin_type::BuiltinType,
    item_scope::{BuiltinShadowMode, ItemScope},
    item_tree::TreeId,
    nameres::{diagnostics::DefDiagnostic, path_resolution::ResolveMode},
    per_ns::PerNs,
    unstable_features::UnstableFeatures,
    visibility::{Visibility, VisibilityExplicitness},
};

pub use self::facts::impls::{ModuleImpls, module_impls};
pub use self::path_resolution::ResolvePathResultPrefixInfo;

#[cfg(test)]
thread_local! {
    /// Tests only: `macro_expansion_tests` turn the fast path off around one `TestDB`, whose
    /// batches run on the calling thread (`base_db::RunOrder`), so the thread that set it
    /// is the one that collects.
    pub(crate) static ENABLE_BUILTIN_DERIVE_FAST_PATH: std::cell::Cell<bool> =
        const { std::cell::Cell::new(true) };
}

/// Whether builtin derives are recorded instead of expanded, for every database of the process.
/// A code generator needs the bodies of the derived methods, so it sets this once at startup,
/// before any database exists; it is never written after, so the relaxed load a collection does
/// reads a line no core writes.
pub static EXPAND_BUILTIN_DERIVES: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Builtin derives expanded in one database only: an input, so a def map collected on any
/// thread of the plane sees it, and setting it reruns what it changes.
#[blaze_query::input(singleton, debug)]
pub struct BuiltinDerives {
    #[returns(copy)]
    pub expand: bool,
}

/// Has the def maps of `db` expand builtin derives (`true`) or only record them. Every crate's
/// collection reads it, so a change is a crate-wide write: the name resolution maintainer
/// resolves every crate it has already published again.
pub fn set_expand_builtin_derives(db: &mut Db, expand: bool) {
    use blaze_query::db::{Durability, Setter};
    match BuiltinDerives::try_get(db) {
        Some(it) if it.expand(db) == expand => return,
        Some(it) => {
            it.set_expand(db).with_durability(Durability::HIGH).to(expand);
        }
        None => {
            _ = BuiltinDerives::builder(expand).durability(Durability::HIGH).new(db);
        }
    }
    db.note_crates_changed();
}

/// Whether the collection of a def map of `db` records builtin derives instead of expanding
/// them. Read once per collection.
fn enable_builtin_derive_fast_path(db: &Db) -> bool {
    #[cfg(test)]
    if !ENABLE_BUILTIN_DERIVE_FAST_PATH.get() {
        return false;
    }
    !(EXPAND_BUILTIN_DERIVES.load(std::sync::atomic::Ordering::Relaxed)
        || BuiltinDerives::try_get(db).is_some_and(|it| it.expand(db)))
}

const PREDEFINED_TOOLS: &[SmolStr] = &[
    SmolStr::new_static("clippy"),
    SmolStr::new_static("rustfmt"),
    SmolStr::new_static("diagnostic"),
    SmolStr::new_static("miri"),
    SmolStr::new_static("rust_analyzer"),
];

/// Parts of the def map that are only needed when analyzing code in the same crate.
///
/// There are some data in the def map (e.g. extern prelude) that is only needed when analyzing
/// things in the same crate (and maybe in the IDE layer), e.g. the extern prelude. If we put
/// it in the DefMap dependant DefMaps will be invalidated when they change (e.g. when we add
/// a dependency to the crate). Instead we split them out of the DefMap into a LocalDefMap struct.
/// `crate_local_def_map()` returns both, and `crate_def_map()` returns only the external-relevant
/// DefMap.
#[derive(Debug, PartialEq, Eq, Default)]
pub struct LocalDefMap {
    // FIXME: There are probably some other things that could be here, but this is less severe and you
    // need to be careful with things that block def maps also have.
    /// The extern prelude which contains all root modules of external crates that are in scope.
    extern_prelude: FxIndexMap<Name, (ModuleId, Option<ExternCrateId>)>,
}

impl std::hash::Hash for LocalDefMap {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let LocalDefMap { extern_prelude } = self;
        extern_prelude.len().hash(state);
        for (name, (crate_root, extern_crate)) in extern_prelude {
            name.hash(state);
            crate_root.hash(state);
            extern_crate.hash(state);
        }
    }
}

impl LocalDefMap {
    pub(crate) const EMPTY: &Self =
        &Self { extern_prelude: FxIndexMap::with_hasher(blaze_hash::FixedState) };

    fn shrink_to_fit(&mut self) {
        let Self { extern_prelude } = self;
        extern_prelude.shrink_to_fit();
    }

    pub(crate) fn extern_prelude(
        &self,
    ) -> impl DoubleEndedIterator<Item = (&Name, (ModuleId, Option<ExternCrateId>))> + '_ {
        self.extern_prelude.iter().map(|(name, &def)| (name, def))
    }
}

/// Contains the results of (early) name resolution.
///
/// A `DefMap` stores the module tree and the definitions that are in scope in every module after
/// item-level macros have been expanded.
///
/// Every crate has a primary `DefMap` whose root is the crate's main file (`main.rs`/`lib.rs`),
/// computed by the `crate_def_map` query. Additionally, every block expression introduces the
/// opportunity to write arbitrary item and module hierarchies, and thus gets its own `DefMap` that
/// is computed by the `block_def_map` query.
#[derive(Debug, PartialEq, Eq)]
pub struct DefMap {
    /// The crate this `DefMap` belongs to.
    krate: Crate,
    /// When this is a block def map, this will hold the block id of the block and module that
    /// contains this block.
    block: Option<BlockInfo>,
    pub root: ModuleId,
    /// The modules and their data declared in this crate.
    modules: ModulesSlot,
    /// The prelude module for this crate. This either comes from an import
    /// marked with the `prelude_import` attribute, or (in the normal case) from
    /// a dependency (`std` or `core`).
    /// The prelude is empty for non-block DefMaps (unless `#[prelude_import]` was used,
    /// but that attribute is nightly and when used in a block, it affects resolution globally
    /// so we aren't handling this correctly anyways).
    prelude: Option<(ModuleId, Option<UseId>)>,
    /// `macro_use` prelude that contains macros from `#[macro_use]`'d external crates. Note that
    /// this contains all kinds of macro, not just `macro_rules!` macro.
    /// ExternCrateId being None implies it being imported from the general prelude import.
    macro_use_prelude: HashMap<Name, (MacroId, Option<ExternCrateId>)>,

    /// Tracks which custom derives are in scope for an item, to allow resolution of derive helper
    /// attributes.
    // FIXME: Figure out a better way for the IDE layer to resolve these?
    derive_helpers_in_scope: DeriveHelpers,

    /// Whether collecting declarations ran a macro with compilation-local state.
    pub session_macros: bool,

    /// What the paths of this def map's imports and macro calls resolved through.
    names: uses::NameRecords,

    /// The diagnostics that need to be emitted for this crate.
    diagnostics: Vec<DefDiagnostic>,
    /// `diagnostics` grouped by module, made once collection is done ([`Self::hand_over_scopes`]).
    diagnostics_by_module: DiagnosticsByModule,
    /// The [`HeldRecords`] that `names`, `diagnostics`, `diagnostics_by_module` and
    /// `derive_helpers_in_scope` move to when collection is done, leaving them empty: a `use` or a
    /// derive added changes the records and leaves this value equal, so what reads the `DefMap`
    /// (every resolver, every solver) is not run again for it.
    records: Option<RecordsSlot>,

    /// The crate data that is shared between a crate's def map and all its block def maps.
    data: Arc<DefMapCrateData>,
}

type DeriveHelpers =
    HashMap<AstId<ast::Item>, Vec<(Name, MacroId, Either<MacroCallId, BuiltinDeriveImplId>)>>;

/// What a collected def map recorded that no resolution reads: how its imports and macro calls
/// resolved, its diagnostics and the derive helpers in scope of its items.
#[blaze_query::tracked]
struct HeldRecords<'db> {
    root: ModuleIdLt<'db>,
    #[tracked]
    #[returns(ref)]
    records: OwnedRecords,
}

/// Where a handed-over def map's records are: a block's `HeldRecords`, or a crate's
/// `delta::CrateResolution`, which the name resolution maintainer sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordsSlot {
    Held(blaze_query::db::Id),
    Published(blaze_query::db::Id),
}

#[derive(Debug, Default, PartialEq, Eq)]
struct OwnedRecords {
    names: uses::NameRecords,
    diagnostics: Vec<DefDiagnostic>,
    diagnostics_by_module: DiagnosticsByModule,
    derive_helpers_in_scope: DeriveHelpers,
}

/// A def map's records wherever they are ([`DefMap::records`]).
#[derive(Clone, Copy)]
pub struct Records<'a> {
    pub names: &'a uses::NameRecords,
    pub diagnostics: &'a [DefDiagnostic],
    diagnostics_by_module: &'a DiagnosticsByModule,
    derive_helpers_in_scope: &'a DeriveHelpers,
}

/// A def map's diagnostics by module: the indices of the diagnostics in module order (stable,
/// so each module's are in push order), and each module that has any with where its run of
/// indices begins, sorted by module.
#[derive(Debug, Default, PartialEq, Eq)]
struct DiagnosticsByModule {
    order: Box<[u32]>,
    modules: Box<[(ModuleId, u32)]>,
}

impl DiagnosticsByModule {
    fn of(diagnostics: &[DefDiagnostic]) -> DiagnosticsByModule {
        let len = u32::try_from(diagnostics.len()).expect("a def map has 2^32 diagnostics");
        // alloc: once per def map, when collection is done; the memo's own columns
        let mut order: Vec<u32> = (0..len).collect();
        order.sort_by_key(|&at| diagnostics[at as usize].in_module);
        // alloc: once per def map, when collection is done; the memo's own columns
        let mut modules: Vec<(ModuleId, u32)> = Vec::new();
        for (start, &at) in (0u32..).zip(&order) {
            let module = diagnostics[at as usize].in_module;
            if modules.last().is_none_or(|&(last, _)| last != module) {
                modules.push((module, start));
            }
        }
        DiagnosticsByModule { order: order.into(), modules: modules.into() }
    }

    /// The indices of `module`'s diagnostics, in push order.
    fn of_module(&self, module: ModuleId) -> &[u32] {
        let Ok(at) = self.modules.binary_search_by_key(&module, |&(it, _)| it) else {
            return &[];
        };
        let start = self.modules[at].1 as usize;
        let end = self.modules.get(at + 1).map_or(self.order.len(), |&(_, it)| it as usize);
        &self.order[start..end]
    }
}

/// Data that belongs to a crate which is shared between a crate's def map and all its block def maps.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DefMapCrateData {
    /// Side table for resolving derive helpers.
    exported_derives: HashMap<MacroId, Box<[Name]>>,
    fn_proc_macro_mapping: HashMap<FunctionId, ProcMacroId>,
    fn_proc_macro_mapping_back: HashMap<ProcMacroId, FunctionId>,

    /// Custom tool modules registered with `#![register_tool]`.
    registered_tools: Vec<Symbol>,
    /// Unstable features of Rust enabled with `#![feature(A, B)]`.
    unstable_features: UnstableFeatures,
    /// `#[rustc_coherence_is_core]`
    rustc_coherence_is_core: bool,
    no_core: bool,
    no_std: bool,
    /// A crate of procedural macros, which gives other crates its macros and nothing else.
    proc_macro: bool,

    edition: Edition,
    recursion_limit: Option<u32>,
}

impl DefMapCrateData {
    fn new(edition: Edition) -> Self {
        Self {
            exported_derives: HashMap::default(),
            fn_proc_macro_mapping: HashMap::default(),
            fn_proc_macro_mapping_back: HashMap::default(),
            // alloc: once per crate
            registered_tools: PREDEFINED_TOOLS.iter().map(|it| Symbol::intern(it)).collect(),
            unstable_features: UnstableFeatures::default(),
            rustc_coherence_is_core: false,
            no_core: false,
            no_std: false,
            proc_macro: false,
            edition,
            recursion_limit: None,
        }
    }

    fn shrink_to_fit(&mut self) {
        let Self {
            exported_derives,
            fn_proc_macro_mapping,
            fn_proc_macro_mapping_back,
            registered_tools,
            unstable_features,
            rustc_coherence_is_core: _,
            no_core: _,
            no_std: _,
            proc_macro: _,
            edition: _,
            recursion_limit: _,
        } = self;
        exported_derives.shrink_to_fit();
        fn_proc_macro_mapping.shrink_to_fit();
        fn_proc_macro_mapping_back.shrink_to_fit();
        registered_tools.shrink_to_fit();
        unstable_features.shrink_to_fit();
    }
}

/// For `DefMap`s computed for a block expression, this stores its location in the parent map.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct BlockInfo {
    /// The `BlockId` this `DefMap` was created from.
    block: BlockId,
    /// The containing module.
    parent: ModuleId,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum ModuleOrigin {
    CrateRoot {
        definition: EditionedFileId,
    },
    /// Note that non-inline modules, by definition, live inside non-macro file.
    File {
        is_mod_rs: bool,
        declaration: FileAstId<ast::Module>,
        declaration_tree_id: TreeId,
        definition: EditionedFileId,
    },
    Inline {
        definition_tree_id: TreeId,
        definition: FileAstId<ast::Module>,
    },
    /// Pseudo-module introduced by a block scope (contains only inner items).
    BlockExpr {
        id: BlockId,
        block: AstId<ast::BlockExpr>,
    },
}

impl ModuleOrigin {
    pub fn declaration(&self) -> Option<AstId<ast::Module>> {
        match self {
            &ModuleOrigin::File { declaration, declaration_tree_id, .. } => {
                Some(AstId::new(declaration_tree_id.file_id(), declaration))
            }
            &ModuleOrigin::Inline { definition, definition_tree_id } => {
                Some(AstId::new(definition_tree_id.file_id(), definition))
            }
            ModuleOrigin::CrateRoot { .. } | ModuleOrigin::BlockExpr { .. } => None,
        }
    }

    pub fn file_id(&self) -> Option<EditionedFileId> {
        match self {
            ModuleOrigin::File { definition, .. } | ModuleOrigin::CrateRoot { definition } => {
                Some(*definition)
            }
            _ => None,
        }
    }

    pub fn is_inline(&self) -> bool {
        match self {
            ModuleOrigin::Inline { .. } | ModuleOrigin::BlockExpr { .. } => true,
            ModuleOrigin::CrateRoot { .. } | ModuleOrigin::File { .. } => false,
        }
    }

    /// Returns a node which defines this module, a view of the tree the database keeps.
    /// That is, a file or a `mod foo {}` with items.
    pub fn definition_source_view<'db>(&self, db: &'db Db) -> InFile<ModuleSourceView<'db>> {
        match self {
            &ModuleOrigin::File { definition: editioned_file_id, .. }
            | &ModuleOrigin::CrateRoot { definition: editioned_file_id } => InFile::new(
                editioned_file_id.into(),
                ModuleSourceView::SourceFile(editioned_file_id.parse(db).view()),
            ),
            &ModuleOrigin::Inline { definition, definition_tree_id } => InFile::new(
                definition_tree_id.file_id(),
                ModuleSourceView::Module(
                    AstId::new(definition_tree_id.file_id(), definition).to_view(db),
                ),
            ),
            ModuleOrigin::BlockExpr { block, .. } => {
                InFile::new(block.file_id, ModuleSourceView::BlockExpr(block.to_view(db)))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleData {
    /// Where does this module come from?
    pub origin: ModuleOrigin,
    /// Declared visibility of this module.
    pub visibility: Visibility,
    /// Parent module in the same `DefMap`.
    ///
    /// [`None`] for block modules because they are always its `DefMap`'s root.
    pub parent: Option<ModuleId>,
    scope: ScopeSlot,
}

/// The modules of a `DefMap`, held by the database on their own: a `DefMap` stays what it was
/// when a module is added to it, and so does everything that was resolved through it.
#[blaze_query::tracked]
struct Modules<'db> {
    root: ModuleIdLt<'db>,
    #[tracked]
    #[returns(ref)]
    table: ModulesMap,
}

#[blaze_query::tracked]
impl<'db> Modules<'db> {
    /// One module of a collected `DefMap`. Depends on the modules there are, and is the same
    /// for as long as the module is.
    #[blaze_query::tracked(returns(ref))]
    fn module(self, db: &'db Db, module: ModuleId) -> Option<ModuleData> {
        self.table(db).get(&module).cloned()
    }

    /// What `name` means in `module`: one memo for a path segment, where reading the module
    /// and then its scope is two. Depends on the modules there are and on that module's scope;
    /// backdated while the name means what it did.
    #[blaze_query::tracked]
    fn name(self, db: &'db Db, module: ModuleId, name: Name) -> PerNs {
        let Some(data) = self.table(db).get(&module) else {
            panic!("ModuleId not found in ModulesMap: {module:#?}")
        };
        data.scope(db).get(&name)
    }
}

impl ModulesSlot {
    /// The modules of a `DefMap` that is being collected.
    fn building(&mut self) -> &mut ModulesMap {
        match self {
            ModulesSlot::Building(modules)
            | ModulesSlot::Unit { own: modules, .. }
            | ModulesSlot::Maintained { own: modules, .. } => modules,
            ModulesSlot::Collected(_) | ModulesSlot::Published(_) => {
                panic!("the modules of a collected `DefMap` are read-only")
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ModulesSlot {
    /// While the `DefMap` is being collected.
    Building(ModulesMap),
    /// A `Modules`, once it is.
    Collected(blaze_query::db::Id),
    /// A view of the per-module resolver (`facts`): the modules it holds, and every other
    /// module of its crate read through `facts::row` of `rows` (a `facts` `RowsKey`).
    Unit { own: ModulesMap, rows: blaze_query::db::Id },
    /// A view of the name resolution maintainer (`delta`): the modules its run holds, and every
    /// other module of the crate as the maintainer's state has it. Every read is noted
    /// (`delta::reads`) with its reader: it is what wakes that reader again when the module
    /// changes.
    Maintained { own: ModulesMap, outside: ModulesMap },
    /// A crate's modules as the maintainer published them: `delta::CrateResolution::modules`.
    Published(blaze_query::db::Id),
}

/// The items of one module, held by the database on their own. A query that reads the scope of
/// a module then depends on that scope and not on the whole `DefMap`, so adding an item to one
/// module leaves what was resolved through the others alone.
#[blaze_query::tracked]
struct ModuleScope<'db> {
    module: ModuleIdLt<'db>,
    #[tracked]
    #[returns(ref)]
    scope: ItemScope,
    #[tracked]
    #[returns(ref)]
    children: FxIndexMap<Name, ModuleId>,
}

#[blaze_query::tracked]
impl<'db> ModuleScope<'db> {
    /// What one name means in the module. Adding or removing another item leaves this equal,
    /// so a query that only looked names up is not re-run.
    #[blaze_query::tracked]
    fn name(self, db: &'db Db, name: Name) -> PerNs {
        self.scope(db).get(&name)
    }

    #[blaze_query::tracked(returns(ref))]
    fn legacy_macro(self, db: &'db Db, name: Name) -> Option<Box<[MacroId]>> {
        self.scope(db).get_legacy_macro(&name).map(Box::from)
    }

    #[blaze_query::tracked(returns(ref))]
    fn traits(self, db: &'db Db) -> Box<[crate::TraitId]> {
        // alloc: the memo's value, once per module scope
        self.scope(db).traits().collect()
    }
}

/// What a module holds while its `DefMap` is collected: boxed together, so a collected
/// `ModuleData` (every module of every def map the database keeps) carries 16 bytes for its
/// scope, not the box and the map's 56.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct BuildingScope {
    scope: ItemScope,
    children: FxIndexMap<Name, ModuleId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ScopeSlot {
    /// While the `DefMap` is being collected.
    Building(Box<BuildingScope>),
    /// A `ModuleScope`, once it is.
    Collected(blaze_query::db::Id),
    /// The `ModuleScope` a resolver fact names (a `facts` `ScopeRef`), looked up on first read.
    Fact(blaze_query::db::Id),
    /// The module's scope as the name resolution maintainer published it (`delta::ScopeInput`).
    Input(blaze_query::db::Id),
}

impl ScopeSlot {
    /// The `ModuleScope` of a handed-over or fact slot.
    fn handle(&self, db: &Db) -> Option<blaze_query::db::Id> {
        match self {
            ScopeSlot::Building(_) | ScopeSlot::Input(_) => None,
            ScopeSlot::Collected(id) => Some(*id),
            ScopeSlot::Fact(fact) => Some(facts::fact_scope(db, *fact)),
        }
    }
}

const _: () = assert!(size_of::<ScopeSlot>() <= 16);

#[inline]
pub fn crate_def_map(db: &Db, crate_id: Crate) -> &DefMap {
    crate_local_def_map(db, crate_id).def_map(db)
}

/// The recursion limit of `krate` (`#![recursion_limit]`, 128 by default): what the solver
/// reads of the def map, so that it depends on that alone.
#[blaze_query::tracked(returns(copy))]
pub fn crate_recursion_limit(db: &Db, krate: Crate) -> u32 {
    crate_def_map(db, krate).recursion_limit()
}

/// Whether a primitive type's name (`u8`, `str`, `bool`, ..) can mean something else in
/// `krate`: a module of the crate has that name in its type namespace (an item, a named or glob
/// import, an `extern crate`), or the crate's extern prelude or prelude does. When it is
/// `false`, a one-segment path of a primitive type's name written in a module of the crate is
/// that primitive type, unless a generic parameter or a block scope around the path has the
/// name: blocks are not counted (each has its own def map), nor are generic parameters, so a
/// caller checks those itself.
#[blaze_query::tracked(returns(copy))]
pub fn crate_shadows_primitive_type(db: &Db, krate: Crate) -> bool {
    let pair = crate_local_def_map(db, krate);
    let def_map = pair.def_map(db);
    let shadows = |scope: &ItemScope| {
        scope.types().any(|(name, _)| BuiltinType::by_name(name).is_some())
    };
    def_map.modules(db).any(|(_, module)| shadows(module.scope(db)))
        || pair.local(db).extern_prelude().any(|(name, _)| BuiltinType::by_name(name).is_some())
        || def_map.prelude().is_some_and(|(prelude, _)| {
            shadows(prelude.def_map(db).module(db, prelude).scope(db))
        })
}

#[blaze_query::tracked]
pub(crate) struct DefMapPair<'db> {
    #[tracked]
    #[returns(ref)]
    pub(crate) def_map: DefMap,
    #[returns(ref)]
    pub(crate) local: LocalDefMap,
}

/// `krate`'s display name, empty without one: what a span names the crate by, backdated when the
/// rest of its extra data (version, potential cfg options) changes.
#[blaze_query::tracked(returns(ref))]
fn crate_display_name(db: &Db, krate: Crate) -> SmolStr {
    krate.extra_data(db).display_name.as_ref().map(|it| it.crate_name().to_smolstr()).unwrap_or_default()
}

#[blaze_query::tracked(returns(ref))]
pub(crate) fn crate_local_def_map(db: &Db, crate_id: Crate) -> DefMapPair<'_> {
    let _p = tracing::info_span!("crate_def_map_query", krate = ?crate_display_name(db, crate_id)).entered();

    if let Some((def_map, local_def_map)) = frozen::served(db, crate_id) {
        return DefMapPair::new(db, def_map, local_def_map);
    }
    let Some((def_map, local_def_map)) = delta::published(db, crate_id) else {
        Unmaintained::of(db, crate_id, delta::maintaining()).throw()
    };
    frozen::check(db, crate_id, &def_map, &local_def_map);
    DefMapPair::new(db, def_map, local_def_map)
}

/// A crate's `DefMap` asked for when no frozen image serves it and the name resolution
/// maintainer published none: a bug in whoever asked or in the maintainer, never resolved
/// another way. It unwinds out of the query with this as its payload, and the driver that
/// catches it ([`Unmaintained::catch`]) stops the build with it as the error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unmaintained {
    pub krate: Crate,
    /// Its display name, if it has one.
    pub name: Option<String>,
    /// The crate the maintainer was resolving when it asked, with its display name: the
    /// crate asked for is not maintained before it. `None` when no maintainer ran.
    pub asking: Option<(Crate, Option<String>)>,
}

impl Unmaintained {
    fn of(db: &Db, krate: Crate, asking: Option<Crate>) -> Unmaintained {
        let name = |krate: Crate| {
            // alloc: an error's crate name, once
            krate.extra_data(db).display_name.as_ref().map(|it| it.crate_name().to_string())
        };
        Unmaintained { krate, name: name(krate), asking: asking.map(|it| (it, name(it))) }
    }

    /// Unwinds with `self` as the payload. It is logged first: a catcher that reads only a
    /// panic's text finds none in it, and the panic hook prints only where it was thrown.
    pub fn throw(self) -> ! {
        tracing::error!("{self}");
        std::panic::panic_any(self)
    }

    /// Runs `f`; an [`Unmaintained`] it unwinds with is returned, any other panic goes on.
    pub fn catch<T>(f: impl FnOnce() -> T + std::panic::UnwindSafe) -> Result<T, Unmaintained> {
        std::panic::catch_unwind(f).map_err(|payload| match payload.downcast::<Unmaintained>() {
            Ok(it) => *it,
            Err(payload) => std::panic::resume_unwind(payload),
        })
    }

    /// The `Unmaintained` a caught panic's payload is, if it is one.
    // dyn: a caught panic's payload, read once
    pub fn of_payload(payload: &(dyn std::any::Any + Send)) -> Option<&Unmaintained> {
        payload.downcast_ref::<Unmaintained>()
    }
}

fn write_crate(f: &mut std::fmt::Formatter<'_>, krate: Crate, name: Option<&str>) -> std::fmt::Result {
    match name {
        Some(name) => write!(f, "`{name}` ({krate:?})"),
        None => write!(f, "{krate:?}"),
    }
}

impl std::fmt::Display for Unmaintained {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("name resolution: no maintained resolution for crate ")?;
        write_crate(f, self.krate, self.name.as_deref())?;
        match &self.asking {
            Some((asking, name)) => {
                f.write_str(", read while maintaining crate ")?;
                write_crate(f, *asking, name.as_deref())?;
                f.write_str(", which it is not maintained before")
            }
            None => f.write_str(": no maintainer ran for it and no frozen image serves it"),
        }
    }
}

impl std::error::Error for Unmaintained {}

/// Name resolution of a crate, or of a block of it, that still made progress after
/// `FIXED_POINT_LIMIT` macro rounds: every round resolved a macro call or reseeded an
/// attribute, so it would never reach its fixed point. A bug in blaze, or a program whose
/// macros expand without end below the recursion limit. It unwinds like [`Unmaintained`], and
/// the driver that catches it stops the build with it as the error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixpointLimit {
    pub krate: Crate,
    /// Its display name, if it has one.
    pub name: Option<String>,
    /// Whether it was a block's resolution, not the crate's.
    pub block: bool,
    /// The rounds run.
    pub rounds: u32,
    /// How many macro calls were still waiting to resolve, and how many imports.
    pub pending_macros: usize,
    pub pending_imports: usize,
    /// The first of them, each as "macro `path` in module `name`" or "import `path` in ...".
    pub first: Vec<String>,
}

impl FixpointLimit {
    /// How many of the pending macro calls and imports [`FixpointLimit::first`] lists.
    pub(crate) const LISTED: usize = 8;

    /// Unwinds with `self` as the payload, logged first as [`Unmaintained::throw`] is.
    pub fn throw(self) -> ! {
        tracing::error!("{self}");
        std::panic::panic_any(self)
    }

    /// The `FixpointLimit` a caught panic's payload is, if it is one.
    // dyn: a caught panic's payload, read once
    pub fn of_payload(payload: &(dyn std::any::Any + Send)) -> Option<&FixpointLimit> {
        payload.downcast_ref::<FixpointLimit>()
    }
}

impl std::fmt::Display for FixpointLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("name resolution: ")?;
        if self.block {
            f.write_str("a block of ")?;
        }
        f.write_str("crate ")?;
        write_crate(f, self.krate, self.name.as_deref())?;
        write!(
            f,
            " still made progress after {} macro rounds, with {} macro calls and {} imports pending",
            self.rounds, self.pending_macros, self.pending_imports
        )?;
        for it in &self.first {
            write!(f, "\n  {it}")?;
        }
        Ok(())
    }
}

impl std::error::Error for FixpointLimit {}

#[blaze_query::tracked(returns(ref))]
pub fn block_def_map<'db>(db: &'db Db, block_id: BlockIdLt<'db>) -> DefMap {
    // The `'static` ids go only into the `DefMap` this query returns, invalidated with its key.
    let block_id = block_id.to_static();
    let ast_id = block_id.ast_id(db);
    let module = block_id.module(db).to_static();

    let visibility = Visibility::Module(module, VisibilityExplicitness::Implicit);
    let module_data =
        ModuleData::new(ModuleOrigin::BlockExpr { block: ast_id, id: block_id }, visibility, None);

    blaze_mem::frame::task(Tag::Nameres, |frame| {
        let krate = module.krate(db);
        let local_def_map = crate_local_def_map(db, krate);
        let def_map = DefMap::empty(
            db,
            krate,
            // clone: an Arc count bump, once per block def map
            local_def_map.def_map(db).data.clone(),
            module_data,
            Some(BlockInfo { block: block_id, parent: module }),
        );
        facts::collect_block(
            db,
            frame,
            def_map,
            TreeId::new(ast_id.file_id, Some(block_id)),
            local_def_map.local(db),
        )
    })
}

impl DefMap {
    pub fn edition(&self) -> Edition {
        self.data.edition
    }

    fn empty(
        db: &Db,
        krate: Crate,
        crate_data: Arc<DefMapCrateData>,
        module_data: ModuleData,
        block: Option<BlockInfo>,
    ) -> DefMap {
        let mut modules = ModulesMap::new();
        // `empty` runs only inside the queries that build a def map, and the root id goes only
        // into the `DefMap` that query returns.
        let root = ModuleIdLt::new(
            db,
            krate,
            block.map(|it| it.block),
            None,
            Name::new_symbol_root(sym::__empty),
        )
        .to_static();
        modules.insert(root, module_data);

        DefMap {
            block,
            root,
            modules: ModulesSlot::Building(modules),
            krate,
            prelude: None,
            macro_use_prelude: HashMap::default(),
            derive_helpers_in_scope: HashMap::default(),
            session_macros: false,
            names: uses::NameRecords::default(),
            // alloc: empty; allocates nothing until a diagnostic
            diagnostics: Vec::new(),
            diagnostics_by_module: DiagnosticsByModule::default(),
            records: None,
            data: crate_data,
        }
    }
    fn shrink_to_fit(&mut self) {
        // Exhaustive match to require handling new fields.
        let Self {
            macro_use_prelude,
            diagnostics,
            diagnostics_by_module: _,
            modules,
            derive_helpers_in_scope,
            root: _,
            block: _,
            krate: _,
            prelude: _,
            data: _,
            session_macros: _,
            names: _,
            records: _,
        } = self;

        macro_use_prelude.shrink_to_fit();
        diagnostics.shrink_to_fit();
        derive_helpers_in_scope.shrink_to_fit();
        if let ModulesSlot::Building(modules) = modules {
            modules.shrink_to_fit();
            for (_, module) in modules.iter_mut() {
                module.children_mut().shrink_to_fit();
                module.scope_mut().settle_glob_ambiguities();
                module.scope_mut().settle();
            }
        }
    }

    /// Hands the scope of every module to the database. Called once, when collection is done.
    fn hand_over_scopes(&mut self, db: &Db) {
        let ModulesSlot::Building(modules) = &mut self.modules else { return };
        let diagnostics = std::mem::take(&mut self.diagnostics);
        let records = OwnedRecords {
            names: std::mem::take(&mut self.names),
            diagnostics_by_module: DiagnosticsByModule::of(&diagnostics),
            diagnostics,
            derive_helpers_in_scope: std::mem::take(&mut self.derive_helpers_in_scope),
        };
        let held = HeldRecords::new(db, self.root, records);
        self.records = Some(RecordsSlot::Held(blaze_query::db::AsId::as_id(&held)));
        for (id, module) in modules.iter_mut() {
            if let ScopeSlot::Building(building) = &mut module.scope {
                let BuildingScope { scope, children } = std::mem::take(&mut **building);
                let scope = ModuleScope::new(db, id, scope, children);
                module.scope = ScopeSlot::Collected(blaze_query::db::AsId::as_id(&scope));
            }
        }
        let modules = Modules::new(db, self.root, std::mem::replace(modules, ModulesMap::new()));
        self.modules = ModulesSlot::Collected(blaze_query::db::AsId::as_id(&modules));
    }

    /// What `name` means in module `id` of this `DefMap`; depends on that name only.
    pub(crate) fn name_in_module(&self, db: &Db, id: ModuleIdLt<'_>, name: &Name) -> PerNs {
        match &self.modules {
            ModulesSlot::Building(_) | ModulesSlot::Unit { .. } | ModulesSlot::Published(_) => {
                self.module(db, id).name_in_scope(db, name)
            }
            ModulesSlot::Maintained { own, outside } => {
                let id = id.to_static();
                delta::reads::note(id, Some(*name));
                if let Some(data) = own.get(&id) {
                    return data.name_in_scope(db, name);
                }
                let Some(data) = outside.get(&id) else {
                    panic!("ModuleId not found in ModulesMap {:#?}: {id:#?}", self.root)
                };
                data.name_in_scope(db, name)
            }
            ModulesSlot::Collected(modules) => {
                let id = id.to_static();
                *<Modules<'_> as blaze_query::db::FromId>::from_id(*modules).name(db, id, *name)
            }
        }
    }

    /// One module of the `DefMap`; depends on that module only.
    pub fn module<'a>(&'a self, db: &'a Db, id: ModuleIdLt<'_>) -> &'a ModuleData {
        // The `'static` copy only keys the lookup in `self.modules` (and in a collected map the
        // `Modules::module` query); the query database never frees an id, so none is reused.
        let id = id.to_static();
        let found = match &self.modules {
            ModulesSlot::Building(modules) => modules.get(&id),
            ModulesSlot::Unit { own, rows } => match own.get(&id) {
                Some(it) => Some(it),
                None => facts::row(db, *rows, id),
            },
            ModulesSlot::Maintained { own, outside } => {
                delta::reads::note(id, None);
                own.get(&id).or_else(|| outside.get(&id))
            }
            ModulesSlot::Published(resolution) => delta::published_module(db, *resolution, id),
            ModulesSlot::Collected(modules) => {
                <Modules<'a> as blaze_query::db::FromId>::from_id(*modules).module(db, id).as_ref()
            }
        };
        found
            .unwrap_or_else(|| panic!("ModuleId not found in ModulesMap {:#?}: {id:#?}", self.root))
    }

    /// Every module of the `DefMap`; depends on which there are.
    fn table<'a>(&'a self, db: &'a Db) -> &'a ModulesMap {
        match &self.modules {
            ModulesSlot::Building(modules) => modules,
            ModulesSlot::Unit { .. } | ModulesSlot::Maintained { .. } => {
                panic!("the modules of a resolver view are not listed")
            }
            ModulesSlot::Published(resolution) => delta::published_modules(db, *resolution),
            ModulesSlot::Collected(id) => {
                <Modules<'a> as blaze_query::db::FromId>::from_id(*id).table(db)
            }
        }
    }
}

impl DefMap {
    /// Returns all modules in the crate that are associated with the given file.
    pub fn modules_for_file<'a>(
        &'a self,
        db: &'a Db,
        file_id: FileId,
    ) -> impl Iterator<Item = ModuleId> + 'a {
        self.table(db)
            .iter()
            .filter(move |(_, data)| {
                data.origin.file_id().map(|file_id| file_id.file_id(db)) == Some(file_id)
            })
            .map(|(id, _)| id)
    }

    pub fn modules<'a>(
        &'a self,
        db: &'a Db,
    ) -> impl Iterator<Item = (ModuleId, &'a ModuleData)> + 'a {
        self.table(db).iter()
    }

    /// Where `module` is in [`DefMap::modules`]' order, in one probe: what a table merged from
    /// per-module parts (hir-ty's maintained impl slices) orders its modules by.
    pub fn module_rank(&self, db: &Db, module: ModuleId) -> Option<u32> {
        let at = self.table(db).get_index_of(&module)?;
        Some(u32::try_from(at).expect("more than 2^32 modules in a crate"))
    }

    /// Returns all inline modules (mod name { ... }) in the crate that are associated with the given macro expansion.
    pub fn inline_modules_for_macro_file<'a>(
        &'a self,
        db: &'a Db,
        file_id: MacroCallId,
    ) -> impl Iterator<Item = ModuleId> + 'a {
        self.table(db)
            .iter()
            .filter(move |(_, data)| {
                matches!(
                    data.origin,
                    ModuleOrigin::Inline { definition_tree_id, .. }
                    if definition_tree_id.file_id().macro_file() == Some(file_id)
                )
            })
            .map(|(id, _)| id)
    }

    /// This def map's records: its own while it is collected, the database's once handed over,
    /// so that a read of them depends on them and not on the `DefMap`.
    pub fn records<'a>(&'a self, db: &'a Db) -> Records<'a> {
        match self.records {
            Some(slot) => {
                let held = match slot {
                    RecordsSlot::Held(id) => {
                        <HeldRecords<'a> as blaze_query::db::FromId>::from_id(id).records(db)
                    }
                    RecordsSlot::Published(resolution) => delta::published_records(db, resolution),
                };
                Records {
                    names: &held.names,
                    diagnostics: &held.diagnostics,
                    diagnostics_by_module: &held.diagnostics_by_module,
                    derive_helpers_in_scope: &held.derive_helpers_in_scope,
                }
            }
            None => Records {
                names: &self.names,
                diagnostics: &self.diagnostics,
                diagnostics_by_module: &self.diagnostics_by_module,
                derive_helpers_in_scope: &self.derive_helpers_in_scope,
            },
        }
    }

    pub fn derive_helpers_in_scope<'a>(
        &'a self,
        db: &'a Db,
        id: AstId<ast::Adt>,
    ) -> Option<&'a [(Name, MacroId, Either<MacroCallId, BuiltinDeriveImplId>)]> {
        self.records(db).derive_helpers_in_scope.get(&id.map(|it| it.upcast())).map(Deref::deref)
    }

    /// What the paths of this def map's imports and macro calls resolved through, and how its
    /// macro calls and derive helpers resolved.
    pub fn name_records<'a>(&'a self, db: &'a Db) -> &'a uses::NameRecords {
        self.records(db).names
    }

    pub fn registered_tools(&self) -> &[Symbol] {
        &self.data.registered_tools
    }

    #[inline]
    pub fn features(&self) -> &UnstableFeatures {
        &self.data.unstable_features
    }

    pub fn is_rustc_coherence_is_core(&self) -> bool {
        self.data.rustc_coherence_is_core
    }

    pub fn is_no_std(&self) -> bool {
        self.data.no_std || self.data.no_core
    }

    /// Whether this is a crate of procedural macros (`proc-macro = true`).
    pub fn is_proc_macro(&self) -> bool {
        self.data.proc_macro
    }

    pub fn is_no_core(&self) -> bool {
        self.data.no_core
    }

    pub fn fn_as_proc_macro(&self, id: FunctionId) -> Option<ProcMacroId> {
        self.data.fn_proc_macro_mapping.get(&id).copied()
    }

    pub fn proc_macro_as_fn(&self, id: ProcMacroId) -> Option<FunctionId> {
        self.data.fn_proc_macro_mapping_back.get(&id).copied()
    }

    pub fn krate(&self) -> Crate {
        self.krate
    }

    #[inline]
    pub fn crate_root(&self, db: &Db) -> ModuleId {
        match self.block {
            Some(_) => crate_def_map(db, self.krate()).root,
            None => self.root,
        }
    }

    /// This is the same as [`Self::crate_root`] for crate def maps, but for block def maps, it
    /// returns the root block module.
    pub fn root_module_id(&self) -> ModuleId {
        self.root
    }

    /// If this `DefMap` is for a block expression, returns the module containing the block (which
    /// might again be a block, or a module inside a block).
    pub fn parent(&self) -> Option<ModuleId> {
        Some(self.block?.parent)
    }

    /// Returns the module containing `local_mod`, either the parent `mod`, or the module (or block) containing
    /// the block, if `self` corresponds to a block expression.
    pub fn containing_module(
        &self,
        db: &Db,
        local_mod: ModuleIdLt<'_>,
    ) -> Option<ModuleId> {
        match self.module(db, local_mod).parent {
            Some(parent) => Some(parent),
            None => self.block.map(|BlockInfo { parent, .. }| parent),
        }
    }

    /// Get a reference to the def map's diagnostics.
    pub fn diagnostics<'a>(&'a self, db: &'a Db) -> &'a [DefDiagnostic] {
        self.records(db).diagnostics
    }

    /// The diagnostics of `module` ([`DefDiagnostic::in_module`]), in the order of
    /// [`Self::diagnostics`]: a binary search, not a scan of them all.
    pub fn diagnostics_of<'a>(
        &'a self,
        db: &'a Db,
        module: ModuleId,
    ) -> impl ExactSizeIterator<Item = &'a DefDiagnostic> + 'a {
        let records = self.records(db);
        records
            .diagnostics_by_module
            .of_module(module)
            .iter()
            .map(move |&at| &records.diagnostics[at as usize])
    }

    pub fn recursion_limit(&self) -> u32 {
        // 128 is the default in rustc
        self.data.recursion_limit.unwrap_or(128)
    }

    // FIXME: this can use some more human-readable format (ideally, an IR
    // even), as this should be a great debugging aid.
    pub fn dump(&self, db: &Db) -> String {
        // alloc: a debug dump (the edge)
        let mut buf = String::new();
        let mut current_map = self;
        while let Some(block) = current_map.block {
            go(&mut buf, db, current_map, "(block scope)", current_map.root);
            buf.push('\n');
            current_map = block.parent.def_map(db);
        }
        go(&mut buf, db, current_map, "crate", current_map.root);
        return buf;

        fn go(
            buf: &mut String,
            db: &Db,
            map: &DefMap,
            path: &str,
            module: ModuleId,
        ) {
            format_to!(buf, "{}\n", path);

            map.module(db, module).scope(db).dump(db, buf);

            for (name, child) in
                map.module(db, module).children(db).iter().sorted_by(|a, b| Ord::cmp(&a.0, &b.0))
            {
                // alloc: a debug dump (the edge)
                let path = format!("{path}::{}", name.display(db, Edition::LATEST));
                buf.push('\n');
                go(buf, db, map, &path, *child);
            }
        }
    }
}

impl DefMap {
    pub(crate) fn block_id(&self) -> Option<BlockId> {
        self.block.map(|block| block.block)
    }

    pub fn prelude(&self) -> Option<(ModuleId, Option<UseId>)> {
        self.prelude
    }

    pub(crate) fn macro_use_prelude(&self) -> &HashMap<Name, (MacroId, Option<ExternCrateId>)> {
        &self.macro_use_prelude
    }

    pub(crate) fn resolve_path(
        &self,
        local_def_map: &LocalDefMap,
        db: &Db,
        original_module: ModuleId,
        path: &ModPath,
        shadow: BuiltinShadowMode,
        expected_macro_subns: Option<MacroSubNs>,
    ) -> (PerNs, Option<usize>) {
        let res = self.resolve_path_fp_with_macro(
            local_def_map,
            db,
            ResolveMode::Other,
            original_module,
            path,
            shadow,
            expected_macro_subns,
        );
        (res.resolved_def, res.segment_index)
    }

    /// The first `Option<usize>` points at the `Enum` segment in case of `Enum::Variant`, the second
    /// points at the unresolved segments.
    pub(crate) fn resolve_path_locally(
        &self,
        local_def_map: &LocalDefMap,
        db: &Db,
        original_module: ModuleId,
        path: &ModPath,
        shadow: BuiltinShadowMode,
    ) -> (PerNs, Option<usize>, ResolvePathResultPrefixInfo) {
        let res = self.resolve_path_fp_with_macro_single(
            local_def_map,
            db,
            ResolveMode::Other,
            original_module,
            path,
            shadow,
            None, // Currently this function isn't used for macro resolution.
        );
        (res.resolved_def, res.segment_index, res.prefix_info)
    }

    /// Ascends the `DefMap` hierarchy and calls `f` with every `DefMap` and containing module.
    ///
    /// If `f` returns `Some(val)`, iteration is stopped and `Some(val)` is returned. If `f` returns
    /// `None`, iteration continues.
    pub(crate) fn with_ancestor_maps<'db, T>(
        &self,
        db: &'db Db,
        local_mod: ModuleIdLt<'db>,
        f: &mut impl FnMut(&DefMap, ModuleIdLt<'db>) -> Option<T>,
    ) -> Option<T> {
        if let Some(it) = f(self, local_mod) {
            return Some(it);
        }
        let mut block = self.block;
        while let Some(block_info) = block {
            let parent = block_info.parent.def_map(db);
            if let Some(it) = f(parent, block_info.parent) {
                return Some(it);
            }
            block = parent.block;
        }

        None
    }
}

impl DefMap {
    /// The `macro_rules!` that `name` is in the textual scope of `module`: the latest one of
    /// this map, or the latest one an enclosing block defines before the block inside it.
    pub(crate) fn legacy_macro_in_textual_scope<'db>(
        &self,
        db: &'db Db,
        module: ModuleIdLt<'db>,
        name: &Name,
    ) -> Option<MacroId> {
        if let Some(&latest) =
            self.module(db, module).legacy_macro_in_scope(db, name).and_then(<[_]>::last)
        {
            return Some(latest);
        }
        let mut inner = self.block;
        while let Some(BlockInfo { block, parent }) = inner {
            let map = parent.def_map(db);
            let defined =
                map.module(db, parent).legacy_macro_in_scope(db, name).unwrap_or_default();
            let visible = SourcePosition::latest_before(db, defined, block.ast_id(db).erase());
            if visible.is_some() {
                return visible;
            }
            inner = map.block;
        }
        None
    }

    /// The `macro_rules!` of `module` that a call at `call` names by `name`, when the module
    /// defines one before the call. Items of a trait or an impl are collected after the
    /// module is, when a later `macro_rules!` of the same name has joined the scope.
    pub(crate) fn legacy_macro_before<'db>(
        &self,
        db: &'db Db,
        module: ModuleIdLt<'db>,
        name: &Name,
        call: ErasedAstId,
    ) -> Option<MacroId> {
        let defined = self.module(db, module).legacy_macro_in_scope(db, name)?;
        SourcePosition::latest_before(db, defined, call)
    }
}

/// Where a node starts in its file and, inside a macro expansion, where each call that led
/// to that expansion starts.
struct SourcePosition(SmallVec<[(HirFileId, TextSize); 2]>);

impl SourcePosition {
    fn new(db: &Db, mut file: HirFileId, range: TextRange) -> Self {
        let mut calls = SmallVec::from_iter([(file, range.start())]);
        while let Some(call) = file.macro_file() {
            let node = call.call_node_view(db);
            file = node.file_id;
            calls.push((file, node.value.text_range().start()));
        }
        SourcePosition(calls)
    }

    /// The last of `macros`, which are in definition order, that is defined before `node`.
    fn latest_before(
        db: &Db,
        macros: &[MacroId],
        node: ErasedAstId,
    ) -> Option<MacroId> {
        match macros {
            [] => None,
            [only] => Some(*only),
            _ => {
                let node = Self::new(db, node.file_id, node.to_range(db));
                macros
                    .iter()
                    .rev()
                    .copied()
                    .find(|&it| Self::of_macro(db, it).is_none_or(|it| it.precedes(&node)))
            }
        }
    }

    fn of_macro(db: &Db, id: MacroId) -> Option<Self> {
        let MacroId::MacroRulesId(id) = id else { return None };
        let ast_id = id.lookup(db).id;
        Some(Self::new(db, ast_id.file_id, ast_id.to_range(db)))
    }

    /// Whether this is before `other` in the innermost file both are in.
    fn precedes(&self, other: &Self) -> bool {
        let shared = self.0.iter().find_map(|(file, offset)| {
            other.0.iter().find(|it| it.0 == *file).map(|it| *offset < it.1)
        });
        shared.unwrap_or(true)
    }
}

impl ModuleData {
    pub(crate) fn new(
        origin: ModuleOrigin,
        visibility: Visibility,
        parent: Option<ModuleId>,
    ) -> Self {
        ModuleData {
            origin,
            visibility,
            parent,
            scope: ScopeSlot::Building(Box::default()),
        }
    }

    /// A module of a `DefMap` being built whose scope and children are already known (a frozen
    /// `DefMap` read back).
    pub(crate) fn with_scope(
        origin: ModuleOrigin,
        visibility: Visibility,
        parent: Option<ModuleId>,
        scope: ItemScope,
        children: FxIndexMap<Name, ModuleId>,
    ) -> Self {
        ModuleData {
            origin,
            visibility,
            parent,
            // alloc: one box per module of a def map being built, moved to the database when it is done
            scope: ScopeSlot::Building(Box::new(BuildingScope { scope, children })),
        }
    }

    /// A module of a `DefMap` whose scope is final (a frozen `DefMap` read back): the scope
    /// goes to the database at once, with no `BuildingScope` in between.
    pub(crate) fn collected(
        db: &Db,
        id: ModuleId,
        origin: ModuleOrigin,
        visibility: Visibility,
        parent: Option<ModuleId>,
        scope: ItemScope,
        children: FxIndexMap<Name, ModuleId>,
    ) -> Self {
        let scope = ModuleScope::new(db, id, scope, children);
        ModuleData {
            origin,
            visibility,
            parent,
            scope: ScopeSlot::Collected(blaze_query::db::AsId::as_id(&scope)),
        }
    }

    pub fn scope<'a>(&'a self, db: &'a Db) -> &'a ItemScope {
        match &self.scope {
            ScopeSlot::Building(building) => &building.scope,
            ScopeSlot::Input(input) => delta::input_scope(db, *input),
            ScopeSlot::Collected(_) | ScopeSlot::Fact(_) => {
                let id = self.scope.handle(db).expect("a handed-over slot");
                <ModuleScope<'a> as blaze_query::db::FromId>::from_id(id).scope(db)
            }
        }
    }

    /// What `name` means in this module; depends on that name only.
    pub fn name_in_scope(&self, db: &Db, name: &Name) -> PerNs {
        match &self.scope {
            ScopeSlot::Building(building) => building.scope.get(name),
            ScopeSlot::Input(input) => delta::input_name(db, *input, *name),
            ScopeSlot::Collected(_) | ScopeSlot::Fact(_) => {
                let id = self.scope.handle(db).expect("a handed-over slot");
                *<ModuleScope<'_> as blaze_query::db::FromId>::from_id(id).name(db, *name)
            }
        }
    }

    pub(crate) fn legacy_macro_in_scope<'a>(
        &'a self,
        db: &'a Db,
        name: &Name,
    ) -> Option<&'a [MacroId]> {
        match &self.scope {
            ScopeSlot::Building(building) => building.scope.get_legacy_macro(name),
            ScopeSlot::Input(input) => delta::input_legacy_macro(db, *input, *name),
            ScopeSlot::Collected(_) | ScopeSlot::Fact(_) => {
                let id = self.scope.handle(db).expect("a handed-over slot");
                <ModuleScope<'a> as blaze_query::db::FromId>::from_id(id)
                    .legacy_macro(db, *name)
                    .as_deref()
            }
        }
    }

    /// The traits in scope in this module; depends on those only.
    pub fn traits_in_scope<'a>(
        &'a self,
        db: &'a Db,
    ) -> impl Iterator<Item = crate::TraitId> + 'a {
        let (building, collected) = match &self.scope {
            ScopeSlot::Building(building) => (Some(building.scope.traits()), None),
            ScopeSlot::Input(input) => {
                (None, Some(delta::input_traits(db, *input).iter().copied()))
            }
            ScopeSlot::Collected(_) | ScopeSlot::Fact(_) => (
                None,
                Some(
                    <ModuleScope<'a> as blaze_query::db::FromId>::from_id(self.scope.handle(db).expect("a handed-over slot"))
                        .traits(db)
                        .iter()
                        .copied(),
                ),
            ),
        };
        building.into_iter().flatten().chain(collected.into_iter().flatten())
    }

    /// The modules declared in this one, by name.
    pub fn children<'a>(&'a self, db: &'a Db) -> &'a FxIndexMap<Name, ModuleId> {
        match &self.scope {
            ScopeSlot::Building(building) => &building.children,
            ScopeSlot::Input(input) => delta::input_children(db, *input),
            ScopeSlot::Collected(_) | ScopeSlot::Fact(_) => {
                let id = self.scope.handle(db).expect("a handed-over slot");
                <ModuleScope<'a> as blaze_query::db::FromId>::from_id(id).children(db)
            }
        }
    }

    pub(crate) fn children_mut(&mut self) -> &mut FxIndexMap<Name, ModuleId> {
        match &mut self.scope {
            ScopeSlot::Building(building) => &mut building.children,
            ScopeSlot::Collected(_) | ScopeSlot::Fact(_) | ScopeSlot::Input(_) => {
                panic!("the children of a collected module are read-only")
            },
        }
    }

    /// The scope of a module of the `DefMap` being collected.
    pub(crate) fn scope_mut(&mut self) -> &mut ItemScope {
        match &mut self.scope {
            ScopeSlot::Building(building) => &mut building.scope,
            ScopeSlot::Collected(_) | ScopeSlot::Fact(_) | ScopeSlot::Input(_) => {
                panic!("the scope of a collected module is read-only")
            },
        }
    }

    /// Returns a node which defines this module, as a view. That is, a file or a `mod foo {}`
    /// with items.
    pub fn definition_source_view<'db>(&self, db: &'db Db) -> InFile<ModuleSourceView<'db>> {
        self.origin.definition_source_view(db)
    }

    /// Same as [`ModuleData::definition_source_view`] but only returns the file id to prevent parsing the AST.
    pub fn definition_source_file_id(&self) -> HirFileId {
        match self.origin {
            ModuleOrigin::File { definition, .. } | ModuleOrigin::CrateRoot { definition } => {
                definition.into()
            }
            ModuleOrigin::Inline { definition_tree_id, .. } => definition_tree_id.file_id(),
            ModuleOrigin::BlockExpr { block, .. } => block.file_id,
        }
    }

    pub fn definition_source_range(&self, db: &Db) -> InFile<TextRange> {
        match &self.origin {
            &ModuleOrigin::File { definition, .. } | &ModuleOrigin::CrateRoot { definition } => {
                InFile::new(
                    definition.into(),
                    ErasedAstId::new(definition.into(), ROOT_ERASED_FILE_AST_ID).to_range(db),
                )
            }
            &ModuleOrigin::Inline { definition, definition_tree_id } => InFile::new(
                definition_tree_id.file_id(),
                AstId::new(definition_tree_id.file_id(), definition).to_range(db),
            ),
            ModuleOrigin::BlockExpr { block, .. } => InFile::new(block.file_id, block.to_range(db)),
        }
    }

    /// Returns a node which declares this module, either a `mod foo;` or a `mod foo {}`.
    /// `None` for the crate root or block.
    pub fn declaration_source(&self, db: &Db) -> Option<InFile<ast::Module>> {
        let decl = self.origin.declaration()?;
        let value = decl.to_node(db);
        Some(InFile { file_id: decl.file_id, value })
    }

    /// [`ModuleData::declaration_source`] as a view.
    pub fn declaration_source_view<'db>(
        &self,
        db: &'db Db,
    ) -> Option<InFile<view::Module<'db>>> {
        let decl = self.origin.declaration()?;
        Some(InFile::new(decl.file_id, decl.to_view(db)))
    }

    /// Returns the range which declares this module, either a `mod foo;` or a `mod foo {}`.
    /// `None` for the crate root or block.
    pub fn declaration_source_range(&self, db: &Db) -> Option<InFile<TextRange>> {
        let decl = self.origin.declaration()?;
        Some(InFile { file_id: decl.file_id, value: decl.to_range(db) })
    }
}

/// The node that defines a module, as views of the tree the database keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleSourceView<'db> {
    SourceFile(view::SourceFile<'db>),
    Module(view::Module<'db>),
    BlockExpr(view::BlockExpr<'db>),
}

impl<'db> ModuleSourceView<'db> {
    pub fn node(self) -> view::Node<'db> {
        match self {
            ModuleSourceView::SourceFile(it) => it.syntax(),
            ModuleSourceView::Module(it) => it.syntax(),
            ModuleSourceView::BlockExpr(it) => it.syntax(),
        }
    }
}

/// See `sub_namespace_match()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacroSubNs {
    /// Function-like macros, suffixed with `!`.
    Bang,
    /// Macros inside attributes, i.e. attribute macros and derive macros.
    Attr,
}

pub(crate) fn macro_styles_from_id(db: &Db, macro_id: MacroId) -> MacroCallStyles {
    let expander = match macro_id {
        MacroId::Macro2Id(it) => it.lookup(db).expander,
        MacroId::MacroRulesId(it) => it.lookup(db).expander,
        MacroId::ProcMacroId(it) => {
            return match it.lookup(db).kind {
                ProcMacroKind::CustomDerive => MacroCallStyles::DERIVE,
                ProcMacroKind::Bang => MacroCallStyles::FN_LIKE,
                ProcMacroKind::Attr => MacroCallStyles::ATTR,
            };
        }
    };

    match expander {
        MacroExpander::Declarative { styles } => styles,
        // Eager macros aren't *guaranteed* to be bang macros, but they *are* all bang macros currently.
        MacroExpander::BuiltIn(_) | MacroExpander::BuiltInEager(_) => MacroCallStyles::FN_LIKE,
        MacroExpander::BuiltInAttr(_) => MacroCallStyles::ATTR,
        MacroExpander::BuiltInDerive(_) => MacroCallStyles::DERIVE,
        MacroExpander::UnimplementedBuiltIn => MacroCallStyles::all(), // Unknown.
    }
}

/// Quoted from [rustc]:
/// Macro namespace is separated into two sub-namespaces, one for bang macros and
/// one for attribute-like macros (attributes, derives).
/// We ignore resolutions from one sub-namespace when searching names in scope for another.
///
/// [rustc]: https://github.com/rust-lang/rust/blob/1.69.0/compiler/rustc_resolve/src/macros.rs#L75
fn sub_namespace_match(
    db: &Db,
    macro_id: MacroId,
    expected: Option<MacroSubNs>,
) -> bool {
    let candidate = macro_styles_from_id(db, macro_id);
    match expected {
        Some(MacroSubNs::Bang) => candidate.contains(MacroCallStyles::FN_LIKE),
        Some(MacroSubNs::Attr) => {
            candidate.contains(MacroCallStyles::ATTR) || candidate.contains(MacroCallStyles::DERIVE)
        }
        // If we aren't expecting a specific sub-namespace
        // (e.g. in `use` declarations), match any macro.
        None => true,
    }
}

/// A newtype wrapper around `HashMap<ModuleId, ModuleData>` that implements `IndexMut`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ModulesMap {
    inner: FxIndexMap<ModuleId, ModuleData>,
}

impl ModulesMap {
    fn new() -> Self {
        Self { inner: FxIndexMap::default() }
    }

    fn iter(&self) -> impl Iterator<Item = (ModuleId, &ModuleData)> + '_ {
        self.inner.iter().map(|(&k, v)| (k, v))
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = (ModuleId, &mut ModuleData)> + '_ {
        self.inner.iter_mut().map(|(&k, v)| (k, v))
    }
}

impl Deref for ModulesMap {
    type Target = FxIndexMap<ModuleId, ModuleData>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for ModulesMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Index<ModuleIdLt<'_>> for ModulesMap {
    type Output = ModuleData;

    fn index(&self, id: ModuleIdLt<'_>) -> &ModuleData {
        self.inner
            .get(&id.to_static())
            .unwrap_or_else(|| panic!("ModuleId not found in ModulesMap: {id:#?}"))
    }
}

impl IndexMut<ModuleId> for ModulesMap {
    fn index_mut(&mut self, id: ModuleId) -> &mut ModuleData {
        self.inner
            .get_mut(&id)
            .unwrap_or_else(|| panic!("ModuleId not found in ModulesMap: {id:#?}"))
    }
}
