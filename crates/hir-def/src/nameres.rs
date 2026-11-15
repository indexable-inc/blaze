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
//! This happens in `facts::collect` (`module_decls`). In this phase, we recursively walk
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
mod batch;
pub mod boundary;
pub(crate) mod def_site;
pub use def_site::{
    assoc_item_binding_name, for_each_hygienic_trait_in_scope, implemented_name, implements,
    trait_item_is_written, trait_item_name, trait_item_written, written_key, written_name,
};
pub use def_site::{assoc_item_is, assoc_item_key, impl_item_name};
pub use def_site::{
    binding_key_at, def_binding_key, import_binding_key, param_name, path_param_name,
};
pub use def_site::{impl_expansion, privacy_scope, variant_expansion};
pub mod demand;
pub mod diagnostics;
mod facts;
pub use self::facts::kinds;
pub mod frozen;
pub use hir_expand::proc_macro::may_have_session_macros;
mod mod_resolution;
pub mod named;
mod path_resolution;
pub mod proc_macro;
mod trait_items;
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
use std::alloc::Global;
use stdx::format_to;
use syntax::{
    AstNode, SmolStr, TextSize, ast,
    view::{self, AstNode as _},
};
use triomphe::Arc;
use tt::TextRange;

use crate::{
    AstId, BlockId, BuiltinDeriveImplId, ExternCrateId, FunctionId, FxIndexMap, Lookup,
    MacroCallStyles, MacroExpander, MacroId, ModuleId, ModuleIdLt, ProcMacroId, UseId,
    builtin_type::BuiltinType,
    item_scope::{BuiltinShadowMode, ItemScope},
    item_tree::TreeId,
    nameres::{diagnostics::DefDiagnostic, path_resolution::ResolveMode},
    per_ns::PerNs,
    unstable_features::UnstableFeatures,
    visibility::{Visibility, VisibilityExplicitness},
};

pub(crate) use self::def_site::Hygienic;
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

/// Builtin derives expanded in one database only: an input of the session arena, so a def map
/// collected on any thread of the plane sees it. Every crate's collection reads it, so a
/// change of it is a change of the session arena, which every crate arena borrows: the next
/// snapshot drops them all.
#[blaze_memo::input(singleton, debug)]
pub struct BuiltinDerives {
    #[returns(copy)]
    pub expand: bool,
}

/// The fact [`BuiltinDerives`] is read by: one id, and the value as its digest.
pub fn builtin_derives_fact(expand: bool) -> blaze_memo::Fact {
    blaze_memo::Fact {
        id: blaze_ids::ContentId::of("hir-def builtin derives", &[]),
        digest: blaze_ids::ContentId::of("hir-def builtin derives value", &[u8::from(expand)]),
    }
}

/// Has the def maps of the snapshot `inputs` builds (its session arena's) expand builtin
/// derives (`true`) or only record them. Called by the driver while it builds a snapshot,
/// before any query is asked; a snapshot's inputs never change after.
pub fn set_expand_builtin_derives(inputs: &blaze_memo::Inputs<'_>, expand: bool) {
    BuiltinDerives::set(inputs, builtin_derives_fact(expand), expand);
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
    macro_use_prelude: HashMap<Name, (MacroId, Option<ExternCrateId>), Global>,

    /// Tracks which custom derives are in scope for an item, to allow resolution of derive helper
    /// attributes.
    // FIXME: Figure out a better way for the IDE layer to resolve these?
    derive_helpers_in_scope: DeriveHelpers,

    /// Whether collecting declarations ran a proc-macro call whose output may depend on its
    /// session's state (`hir_expand::proc_macro::StateUse::Session`).
    pub session_macros: bool,

    /// What the paths of this def map's imports and macro calls resolved through.
    names: uses::NameRecords,

    /// The diagnostics that need to be emitted for this crate.
    diagnostics: Vec<DefDiagnostic>,
    /// `diagnostics` grouped by module, made once collection is done ([`Self::hand_over_scopes`]).
    diagnostics_by_module: DiagnosticsByModule,
    /// Where the records are when they are not the four fields above: a crate read from pages
    /// keeps them in its tail segment. `None` for a def map collected here, whose records are
    /// its own fields.
    records: Option<RecordsSlot>,

    /// The crate data that is shared between a crate's def map and all its block def maps.
    data: Arc<DefMapCrateData>,
}

type DeriveHelpers = HashMap<
    AstId<ast::Item>,
    Vec<(Name, MacroId, Either<MacroCallId, BuiltinDeriveImplId>)>,
    Global,
>;

/// Where a def map's records are when it does not hold them itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordsSlot {
    /// The records of a crate read from pages (`frozen::pages`): its tail segment, decoded
    /// when first asked for.
    Paged(Crate),
}

/// What a def map recorded that no resolution reads: how its imports and macro calls
/// resolved, its diagnostics and the derive helpers in scope of its items.
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
    #[expect(
        blaze::narrowed_len,
        reason = "the columns hold u32 indices of a def map's diagnostics; a def map with 2^32 diagnostics needs a typed refusal at collection"
    )]
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
    exported_derives: HashMap<MacroId, Box<[Name]>, Global>,
    fn_proc_macro_mapping: HashMap<FunctionId, ProcMacroId, Global>,
    fn_proc_macro_mapping_back: HashMap<ProcMacroId, FunctionId, Global>,

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
    /// blaze: `#![blaze::arbitrary_ints]`, under which `u1`..`u128` and `i1`..`i128` are
    /// primitive integer types (docs/v2/plan/design/arbitrary-ints.md).
    arbitrary_ints: bool,
    /// blaze: `#![blaze::handles]`, under which a move of a `Share` value that is used again
    /// later is a copy (docs/v2/plan/design/edition.md, change 1; `hir_ty_mir::mir::handles`).
    handles: bool,
    /// blaze: `#![blaze::views]`, under which a method's receiver may name the fields it
    /// borrows, `&self.{a, b}` (docs/v2/plan/design/edition.md, change 3).
    views: bool,
    /// blaze: `#![blaze::pattern_types]`, under which `T is PAT` is a type and a pattern type
    /// may name enum variants (docs/v2/plan/design/edition.md, section 8).
    pattern_types: bool,
    /// blaze: `#![blaze::early_drop]`, under which a local whose destructor only frees memory
    /// is dropped after its last use (docs/v2/plan/design/early-drop.md;
    /// `hir_ty_mir::mir::early_drop`).
    early_drop: bool,

    edition: Edition,
    recursion_limit: Option<u32>,
}

impl DefMapCrateData {
    fn new(edition: Edition) -> Self {
        Self {
            exported_derives: HashMap::new_in(Global),
            fn_proc_macro_mapping: HashMap::new_in(Global),
            fn_proc_macro_mapping_back: HashMap::new_in(Global),
            // alloc: once per crate
            registered_tools: PREDEFINED_TOOLS.iter().map(|it| Symbol::intern(it)).collect(),
            unstable_features: UnstableFeatures::default(),
            rustc_coherence_is_core: false,
            no_core: false,
            no_std: false,
            proc_macro: false,
            arbitrary_ints: false,
            handles: false,
            views: false,
            pattern_types: false,
            early_drop: false,
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
            arbitrary_ints: _,
            handles: _,
            views: _,
            pattern_types: _,
            early_drop: _,
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

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash, blaze_memo::Home)]
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

impl ModulesSlot {
    /// The modules of a `DefMap` that is being collected.
    fn building(&mut self) -> &mut ModulesMap {
        match self {
            ModulesSlot::Building(modules) | ModulesSlot::Unit { own: modules, .. } => modules,
            ModulesSlot::Collected(_) => {
                panic!("the modules of a collected `DefMap` are read-only")
            }
        }
    }
}

/// The modules of a `DefMap`. They are part of the value of the query that collected it
/// (`crate_resolution`, `block_def_map`): a crate's arena holds them for the snapshot, and a
/// change of any input of the crate drops them with the arena (decision 4), so nothing
/// tracks which module a reader read.
#[derive(Debug, PartialEq, Eq)]
enum ModulesSlot {
    /// While the `DefMap` is being collected.
    Building(ModulesMap),
    /// Once it is: read-only.
    Collected(ModulesMap),
    /// A view of the per-module resolver (`facts`): the modules it holds, and every other
    /// module of its crate read through `facts::row` of `rows`.
    Unit { own: ModulesMap, rows: facts::queries::RowsKey },
}

/// What a module holds while its `DefMap` is collected, and after: boxed together, so a
/// `ModuleData` carries 16 bytes for its scope, not the box and the map's 56.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct BuildingScope {
    scope: ItemScope,
    children: FxIndexMap<Name, ModuleId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ScopeSlot {
    /// While the `DefMap` is being collected.
    Building(Box<BuildingScope>),
    /// Once it is: read-only.
    Collected(Box<BuildingScope>),
    /// The scope a resolver fact names (a `facts` `ScopeRef`), looked up on first read.
    Fact(facts::queries::ScopeRef),
    /// The module's scope in its crate's pages (a `frozen::pages::PagedScope`): a name is read
    /// from the page it is on, the whole scope decoded only for a reader of all of it.
    Paged(frozen::pages::PagedScope),
    /// The scope of a module of a crate's finished def map (what `crate_resolution` holds):
    /// the final round's fact, as `Fact`, read only through name resolution's fact boundaries
    /// ([`boundary`]), so a reader's list holds one row per name it looked up there and never
    /// the crate's files. Made by [`DefMap::hand_over_scopes`] from the final table's `Fact`
    /// slots; the views collection reads keep `Fact`, read directly.
    Final(facts::queries::ScopeRef),
}

impl ScopeSlot {
    /// The scope and children of a slot that holds them, or that a fact names. Never a
    /// `Final` slot's, which is read through its boundaries alone.
    fn held<'a>(&'a self, db: &'a Db) -> Option<&'a BuildingScope> {
        match self {
            ScopeSlot::Building(it) | ScopeSlot::Collected(it) => Some(it),
            ScopeSlot::Fact(fact) => Some(facts::fact_scope(db, *fact)),
            ScopeSlot::Paged(_) | ScopeSlot::Final(_) => None,
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
#[blaze_memo::tracked(returns(copy))]
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
#[blaze_memo::tracked(returns(copy))]
pub fn crate_shadows_primitive_type(db: &Db, krate: Crate) -> bool {
    let pair = crate_local_def_map(db, krate);
    let def_map = pair.def_map(db);
    let shadows = frozen::pages::shadows_primitive_in;
    // A crate read from pages says this of its own modules in its head: asking each scope
    // would decode every module of it for one answer.
    let own = match frozen::pages::shadows_primitive(db, krate) {
        Some(own) => own,
        None => def_map.modules(db).any(|(_, module)| shadows(module.scope(db))),
    };
    own || pair.local(db).extern_prelude().any(|(name, _)| BuiltinType::by_name(name).is_some())
        || def_map
            .prelude()
            .is_some_and(|(prelude, _)| shadows(prelude.def_map(db).module(db, prelude).scope(db)))
}

/// A crate's resolution: its `DefMap` and its `LocalDefMap`, one value of the crate's arena.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CrateResolution {
    def_map: DefMap,
    local: LocalDefMap,
}

impl CrateResolution {
    pub(crate) fn def_map<'a>(&'a self, _db: &Db) -> &'a DefMap {
        &self.def_map
    }

    pub(crate) fn local<'a>(&'a self, _db: &Db) -> &'a LocalDefMap {
        &self.local
    }
}

/// The resolution of `krate` read in place from its pages, when its image serves it
/// ([`frozen::served`]); `None` for a crate resolved here.
#[blaze_memo::tracked(returns(as_ref), owns)]
fn crate_served(db: &Db, krate: Crate) -> Option<CrateResolution> {
    let (def_map, local) = frozen::served(db, krate)?;
    Some(CrateResolution { def_map, local })
}

/// The resolution of `krate` resolved from its sources, in its own arena (how-to item 8:
/// the per-crate query that replaces the name resolution maintainer); asked only for a crate
/// no image serves.
#[blaze_memo::tracked(returns(ref), owns)]
pub(crate) fn crate_resolution(db: &Db, krate: Crate) -> CrateResolution {
    let _p = tracing::info_span!("crate_resolution", krate = %krate.named(db)).entered();
    let (def_map, local) =
        blaze_mem::frame::task(Tag::Nameres, |frame| facts::collect_crate(db, frame, krate));
    frozen::check(db, krate, &def_map, &local);
    CrateResolution { def_map, local }
}

/// The resolution of `krate`: served from its image when one serves it, else resolved
/// ([`crate_resolution`]); never both, and never another route.
pub(crate) fn crate_local_def_map(db: &Db, krate: Crate) -> &CrateResolution {
    crate::signatures::Serve::new(crate_served(db, krate), || crate_resolution(db, krate)).get()
}

/// The scope of `module`, as its crate's resolution has it (how-to item 8). A projection of
/// [`crate_local_def_map`] (or of a block's [`block_def_map`]), not a memo entry of its own:
/// the crate's arena is dropped whole on a change, so a second entry would only copy a
/// reference.
pub fn module_scope(db: &Db, module: ModuleId) -> &ItemScope {
    module.def_map(db).module(db, module).scope(db)
}

fn write_crate(
    f: &mut std::fmt::Formatter<'_>,
    krate: Crate,
    name: Option<&str>,
) -> std::fmt::Result {
    match name {
        Some(name) => write!(f, "`{name}` ({krate:?})"),
        None => write!(f, "{krate:?}"),
    }
}

/// Name resolution of a crate, or of a block of it, that still made progress after
/// `FIXED_POINT_LIMIT` macro rounds: every round resolved a macro call or reseeded an
/// attribute, so it would never reach its fixed point. A bug in blaze, or a program whose
/// macros expand without end below the recursion limit. It unwinds with itself as the
/// payload, and the driver that catches it stops the build with it as the error.
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

    /// Unwinds with `self` as the payload. It is logged first: a catcher that reads only a
    /// panic's text finds none in it, and the panic hook prints only where it was thrown.
    pub fn throw(self) -> ! {
        tracing::error!(target: "blaze", "{self}");
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

/// The error a caught panic's payload is, when it is one of name resolution's typed stops
/// ([`FixpointLimit`], [`frozen::Unreadable`]) or base-db's refused parse of a file with no
/// text ([`base_db::NoText`]): what a driver fails the one job that asked with (rule 20).
/// `None` for any other payload.
// dyn: a caught panic's payload, read once
pub fn stopped(payload: &(dyn std::any::Any + Send)) -> Option<String> {
    if let Some(it) = FixpointLimit::of_payload(payload) {
        return Some(it.to_string());
    }
    if let Some(it) = base_db::NoText::of_payload(payload) {
        return Some(it.to_string());
    }
    frozen::Unreadable::of_payload(payload).map(|it| it.to_string())
}

#[blaze_memo::tracked(returns(ref), owns)]
pub fn block_def_map(db: &Db, block_id: BlockId) -> DefMap {
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
        // The invariant: a crate has one root module id, whoever makes its `DefMap`.
        //
        // A crate's own `DefMap` (no block) takes the root `facts::queries::crate_root`
        // makes, as the name resolution maintainer does and as `child_module` keys every
        // other module by: a module's id is a function of its crate and its path, never of
        // which query built the map. Until 2026-11-11 a `DefMap` decoded from a dependency
        // image made its root here, a tracked struct of the decoding query and so another id
        // than the maintainer's for the same crate: a crate that went from resolved to read
        // from its image (its image stored by one build and served in the next) changed the
        // id of every module it has, and with them of every item (a location is interned by
        // its module), while the rows the build before made were still keyed by the old ones.
        // Asked again, those rows ran against a `DefMap` that held none of their modules
        // (reth, one edit after a cold build: `module .. vec of an item is not in its crate's
        // def map`, `ModuleId not found in ModulesMap` with root Id(24250551) for a module
        // whose root was Id(82944), both of `core`).
        //
        // A block's root is made here: `block_def_map` is the one query that builds a
        // block's `DefMap`, and the id goes only into the map it returns.
        let root = match &block {
            None => facts::queries::crate_root(db, krate),
            Some(block) => ModuleIdLt::new(
                db,
                krate,
                Some(block.block),
                None,
                Name::new_symbol_root(sym::__empty),
            )
            .to_static(),
        };
        modules.insert(root, module_data);

        DefMap {
            block,
            root,
            modules: ModulesSlot::Building(modules),
            krate,
            prelude: None,
            macro_use_prelude: HashMap::new_in(Global),
            derive_helpers_in_scope: HashMap::new_in(Global),
            session_macros: false,
            names: uses::NameRecords::default(),
            diagnostics: Vec::new_in(Global),
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

    /// Seals the `DefMap` once collection is done: every module and scope becomes read-only and
    /// the diagnostics are grouped by module. Called once.
    ///
    /// A crate's own def map (no block) has every scope a fact of the final round: each
    /// becomes `ScopeSlot::Final`, read through name resolution's fact boundaries
    /// ([`boundary`]) from then on.
    fn hand_over_scopes(&mut self, _db: &Db) {
        let ModulesSlot::Building(modules) = &mut self.modules else { return };
        self.diagnostics_by_module = DiagnosticsByModule::of(&self.diagnostics);
        let finished = self.block.is_none();
        for (_, module) in modules.iter_mut() {
            match &mut module.scope {
                ScopeSlot::Building(building) => {
                    module.scope = ScopeSlot::Collected(std::mem::take(building));
                }
                ScopeSlot::Fact(fact) if finished => module.scope = ScopeSlot::Final(*fact),
                ScopeSlot::Fact(_)
                | ScopeSlot::Collected(_)
                | ScopeSlot::Paged(_)
                | ScopeSlot::Final(_) => {}
            }
        }
        self.modules = ModulesSlot::Collected(std::mem::replace(modules, ModulesMap::new()));
    }

    /// What `name` means in module `id` of this `DefMap`.
    pub(crate) fn name_in_module(&self, db: &Db, id: ModuleIdLt<'_>, name: &Name) -> PerNs {
        self.module(db, id).name_in_scope(db, name)
    }

    /// One module of the `DefMap`.
    pub fn module<'a>(&'a self, db: &'a Db, id: ModuleIdLt<'_>) -> &'a ModuleData {
        let id = id.to_static();
        let found = match &self.modules {
            ModulesSlot::Building(modules) | ModulesSlot::Collected(modules) => modules.get(&id),
            ModulesSlot::Unit { own, rows } => match own.get(&id) {
                Some(it) => Some(it),
                None => facts::row(db, *rows, id),
            },
        };
        found
            .unwrap_or_else(|| panic!("ModuleId not found in ModulesMap {:#?}: {id:#?}", self.root))
    }

    /// Every module of the `DefMap`. A crate's finished def map lists the read of all of its
    /// modules ([`boundary::all_modules`]): what it gives changes with any module added,
    /// removed or moved, which no name's row stands for. A block's table is its one module,
    /// made from the text of the body that holds the block (its key covers it) and the rows of
    /// what the block's resolution looked up outside it, which its reader lists
    /// (`block_def_map` is no boundary, [`boundary`]).
    fn table<'a>(&'a self, db: &'a Db) -> &'a ModulesMap {
        match &self.modules {
            ModulesSlot::Building(modules) => modules,
            ModulesSlot::Collected(modules) => {
                if self.block.is_none() {
                    boundary::all_modules(db, self.krate);
                }
                modules
            }
            ModulesSlot::Unit { .. } => panic!("the modules of a resolver view are not listed"),
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

    /// This def map's records: its own, or, for a crate read from pages, its tail's.
    pub fn records<'a>(&'a self, db: &'a Db) -> Records<'a> {
        match self.records {
            Some(slot) => {
                let held = match slot {
                    RecordsSlot::Paged(krate) => frozen::pages::records(db, krate),
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

    /// Whether the crate says `#![blaze::arbitrary_ints]`, so that `u3` and `i24` name
    /// integers of those widths wherever no item of that name shadows them.
    #[inline]
    pub fn arbitrary_ints(&self) -> bool {
        self.data.arbitrary_ints
    }

    /// Whether the crate says `#![blaze::handles]`, so that a move of a handle (a `Share`
    /// value) that is used again on some later path is a copy (`hir_ty_mir::mir::handles`).
    #[inline]
    pub fn handles(&self) -> bool {
        self.data.handles
    }

    /// Whether the crate says `#![blaze::views]`: a method's receiver may name the fields it
    /// borrows, and a private method's are inferred from its body (`hir_ty_borrowck::views`).
    #[inline]
    pub fn views(&self) -> bool {
        self.data.views
    }

    /// Whether the crate says `#![blaze::pattern_types]`, so that `T is PAT` is a type and its
    /// pattern may name variants of an enum (`Conn is Conn::Open { .. }`), and a match, `if let`
    /// or `let else` narrows a plain local to the variants its pattern admits.
    #[inline]
    pub fn pattern_types(&self) -> bool {
        self.data.pattern_types
    }

    /// Whether the crate says `#![blaze::early_drop]`, so that a local whose destructor only
    /// frees memory is dropped after its last use (`hir_ty_mir::mir::early_drop`).
    #[inline]
    pub fn early_drop(&self) -> bool {
        self.data.early_drop
    }

    /// The integer of an arbitrary width `name` names in this crate, as the builtin scope's
    /// `u8` is: only under `#![blaze::arbitrary_ints]`, and shadowed by an item of that name
    /// exactly as `u8` is, since it is read where the builtin scope is.
    pub(crate) fn arbitrary_int(&self, name: &Name) -> PerNs {
        if !self.data.arbitrary_ints {
            return PerNs::none();
        }
        match BuiltinType::arbitrary_by_name(name) {
            Some(ty) => PerNs::types(ty.into(), Visibility::Public, None),
            None => PerNs::none(),
        }
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
    pub fn containing_module(&self, db: &Db, local_mod: ModuleIdLt<'_>) -> Option<ModuleId> {
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

        fn go(buf: &mut String, db: &Db, map: &DefMap, path: &str, module: ModuleId) {
            format_to!(buf, "{}\n", path);

            map.module(db, module).scope(db).dump(db, buf);

            // By text, and modules of one name keyed apart (`Name::keyed`) in the order they
            // were declared: a stable sort over the children's order.
            for (name, child) in
                map.module(db, module).children(db).iter().sorted_by(|a, b| a.0.cmp_written(b.0))
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

    pub(crate) fn macro_use_prelude(
        &self,
    ) -> &HashMap<Name, (MacroId, Option<ExternCrateId>), Global> {
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
    /// points at the unresolved segments. `table`: the table `path`'s contexts are of, the
    /// [`DefMap::ctx_table`] of the def map it is written in, which def-site hygiene may have
    /// moved the lookup out of.
    pub(crate) fn resolve_path_locally(
        &self,
        table: blaze_memo::ArenaId,
        local_def_map: &LocalDefMap,
        db: &Db,
        original_module: ModuleId,
        path: &ModPath,
        shadow: BuiltinShadowMode,
    ) -> (PerNs, Option<usize>, ResolvePathResultPrefixInfo) {
        let res = self.resolve_path_fp_with_macro_single(
            table,
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

    /// [`Self::resolve_path_locally`], going on in the def maps around a block (as an import
    /// does): where def-site hygiene names a block, its lookup is rustc's lexical walk out of it.
    pub(crate) fn resolve_path_around(
        &self,
        table: blaze_memo::ArenaId,
        local_def_map: &LocalDefMap,
        db: &Db,
        original_module: ModuleId,
        path: &ModPath,
        shadow: BuiltinShadowMode,
    ) -> (PerNs, Option<usize>, ResolvePathResultPrefixInfo) {
        let res = self.resolve_path_written_in(
            table,
            local_def_map,
            db,
            ResolveMode::Other,
            original_module,
            path,
            shadow,
            None,
            &mut |_| {},
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
    fn latest_before(db: &Db, macros: &[MacroId], node: ErasedAstId) -> Option<MacroId> {
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
        ModuleData { origin, visibility, parent, scope: ScopeSlot::Building(Box::default()) }
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
            scope: ScopeSlot::Building(Box::new_in(BuildingScope { scope, children }, Global)),
        }
    }

    /// The whole of this module's scope. Of a crate's finished def map or its pages, read
    /// through a boundary whose row stands for every name of the scope (a scan of it).
    pub fn scope<'a>(&'a self, db: &'a Db) -> &'a ItemScope {
        match &self.scope {
            ScopeSlot::Paged(paged) => boundary::paged_scope(db, *paged),
            ScopeSlot::Final(scope) => boundary::final_scope(db, *scope),
            slot => &slot.held(db).expect("a slot that holds its scope").scope,
        }
    }

    /// What `name` means in this module. Of a crate's finished def map or its pages, read
    /// through a boundary keyed by the scope and the name: one row for this name alone.
    pub fn name_in_scope(&self, db: &Db, name: &Name) -> PerNs {
        match &self.scope {
            ScopeSlot::Paged(paged) => boundary::paged_name(db, *paged, *name),
            ScopeSlot::Final(scope) => boundary::final_name(db, *scope, *name),
            slot => slot.held(db).expect("a slot that holds its scope").scope.get(name),
        }
    }

    pub(crate) fn legacy_macro_in_scope<'a>(
        &'a self,
        db: &'a Db,
        name: &Name,
    ) -> Option<&'a [MacroId]> {
        match &self.scope {
            ScopeSlot::Paged(paged) => boundary::paged_legacy_macro(db, *paged, *name),
            ScopeSlot::Final(scope) => boundary::final_legacy_macro(db, *scope, *name),
            slot => slot.held(db).expect("a slot that holds its scope").scope.get_legacy_macro(name),
        }
    }

    /// The traits in scope in this module.
    pub fn traits_in_scope<'a>(&'a self, db: &'a Db) -> impl Iterator<Item = crate::TraitId> + 'a {
        let (held, listed) = match &self.scope {
            ScopeSlot::Paged(paged) => (None, Some(boundary::paged_traits(db, *paged).iter().copied())),
            ScopeSlot::Final(scope) => (Some(boundary::final_traits(db, *scope).traits()), None),
            slot => (Some(slot.held(db).expect("a slot that holds its scope").scope.traits()), None),
        };
        held.into_iter().flatten().chain(listed.into_iter().flatten())
    }

    /// The modules declared in this one, by name.
    pub fn children<'a>(&'a self, db: &'a Db) -> &'a FxIndexMap<Name, ModuleId> {
        match &self.scope {
            ScopeSlot::Paged(paged) => boundary::paged_children(db, *paged),
            ScopeSlot::Final(scope) => boundary::final_children(db, *scope),
            slot => &slot.held(db).expect("a slot that holds its scope").children,
        }
    }

    pub(crate) fn children_mut(&mut self) -> &mut FxIndexMap<Name, ModuleId> {
        match &mut self.scope {
            ScopeSlot::Building(building) => &mut building.children,
            ScopeSlot::Collected(_)
            | ScopeSlot::Fact(_)
            | ScopeSlot::Paged(_)
            | ScopeSlot::Final(_) => {
                panic!("the children of a collected module are read-only")
            }
        }
    }

    /// The scope of a module of the `DefMap` being collected.
    pub(crate) fn scope_mut(&mut self) -> &mut ItemScope {
        match &mut self.scope {
            ScopeSlot::Building(building) => &mut building.scope,
            ScopeSlot::Collected(_)
            | ScopeSlot::Fact(_)
            | ScopeSlot::Paged(_)
            | ScopeSlot::Final(_) => {
                panic!("the scope of a collected module is read-only")
            }
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
    pub fn declaration_source_view<'db>(&self, db: &'db Db) -> Option<InFile<view::Module<'db>>> {
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
fn sub_namespace_match(db: &Db, macro_id: MacroId, expected: Option<MacroSubNs>) -> bool {
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

/// What one read of a row of name resolution was a read of, for whoever records a result's
/// reads as facts (the item store, `blaze_item_hash::reads`): the rows themselves are private
/// to this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameRead {
    /// One name looked up in a module's scope, in its namespaces or among its textual macros;
    /// found or not.
    Name(ModuleId, Name),
    /// The traits in scope in a module.
    Traits(ModuleId),
    /// The whole of a module's scope: an iteration of it.
    WholeScope(ModuleId),
    /// The modules declared in a module, by name.
    Children(ModuleId),
    /// The table of every module of a crate (`Modules`' `table`): an iteration of all of
    /// them, which no one name or scope stands for.
    AllModules(Crate),
    /// What a crate's `DefMap` holds outside its modules' scopes: its prelude, extern prelude
    /// and `#[macro_use]` prelude, what its root's attributes decide, whether a module of it
    /// shadows a primitive type.
    Crate(Crate),
    /// A row whose value is ids (a module's, a scope's, a def map's) that name the same thing
    /// for as long as they are current, and nothing a name means: nothing to record.
    Handle,
}

/// What `krate`'s def map holds outside its modules' scopes, for a hash of it
/// ([`NameRead::Crate`]): `each` is given, in an order that is the same in every database,
/// a tag and one row at a time.
pub fn crate_names(db: &Db, krate: Crate, each: &mut dyn FnMut(CrateName<'_>)) {
    let pair = crate_local_def_map(db, krate);
    let def_map = pair.def_map(db);
    if let Some((prelude, _)) = def_map.prelude() {
        each(CrateName::Prelude(prelude));
    }
    // alloc: the two preludes' names, sorted for an order no map decides; once per change of
    // a crate's def map, a few dozen names
    let mut externs: Vec<(&Name, ModuleId)> =
        pair.local(db).extern_prelude().map(|(name, (root, _))| (name, root)).collect();
    externs.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    for (name, root) in externs {
        each(CrateName::Extern(name, root));
    }
    // alloc: as above
    let mut macros: Vec<(&Name, MacroId)> =
        def_map.macro_use_prelude().iter().map(|(name, &(it, _))| (name, it)).collect();
    macros.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    for (name, it) in macros {
        each(CrateName::MacroUse(name, it));
    }
    let data = &*def_map.data;
    for tool in &data.registered_tools {
        each(CrateName::Tool(tool.as_str()));
    }
    let flags = [
        data.rustc_coherence_is_core,
        data.no_core,
        data.no_std,
        data.proc_macro,
        data.arbitrary_ints,
        data.handles,
        data.views,
        data.pattern_types,
        data.early_drop,
        def_map.session_macros,
        crate_shadows_primitive_type(db, krate),
    ];
    each(CrateName::Flags(&flags, data.edition));
    // The crate's features, sorted for an order no set decides: once per change of a crate's
    // def map, a few names.
    let mut features: Vec<Symbol, Global> = Vec::new_in(Global);
    features.extend(data.unstable_features.iter());
    features.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    for feature in &features {
        each(CrateName::Feature(feature.as_str()));
    }
    each(CrateName::Limit(def_map.recursion_limit()));
    // In the maps' order: the reader sorts them by key, which no map here can.
    for (&it, helpers) in &data.exported_derives {
        each(CrateName::Derive(it, helpers));
    }
    for (&function, &it) in &data.fn_proc_macro_mapping {
        each(CrateName::ProcMacro(function, it));
    }
    // Read through its boundary, as a block's collection reads it: the reader of this crate's
    // names is the one row of it, never the root file the edition is said in.
    each(CrateName::BlazeEdition(boundary::blaze_edition(db, krate)));
}

/// One row of [`crate_names`].
#[derive(Debug, Clone, Copy)]
pub enum CrateName<'a> {
    /// The module whose names every module of the crate sees last.
    Prelude(ModuleId),
    /// A name of the extern prelude and the crate root it names.
    Extern(&'a Name, ModuleId),
    /// A macro a `#[macro_use] extern crate` brought to every module.
    MacroUse(&'a Name, MacroId),
    /// A registered tool, in the order the root registers them.
    Tool(&'a str),
    /// What the root's attributes and the crate's names decide for every module, and the
    /// edition.
    Flags(&'a [bool], Edition),
    /// A `#![feature]` of the crate, in name order.
    Feature(&'a str),
    /// The crate's recursion limit.
    Limit(u32),
    /// A derive macro the crate exports and its helper attributes, in no set order.
    Derive(MacroId, &'a [Name]),
    /// A proc macro function of the crate and the macro it defines, in no set order.
    ProcMacro(FunctionId, ProcMacroId),
    /// The crate's blaze edition (`#![blaze::edition]`), `None` for a crate of Rust's own: it
    /// decides which dependency is the crate's `std` and which prelude its modules and blocks
    /// see.
    BlazeEdition(Option<span::BlazeEdition>),
}
