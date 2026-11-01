//! Tests of the per-module resolver against the old collector, and the helpers they share.
//!
//! The old and the new def map of one crate are made by different queries, so the `ModuleId`s
//! of their modules differ (tracked structs), and with them every item id whose loc names its
//! module. [`project`] maps a def map to a canonical form that names a module of the map by its
//! path and origin and every item by its kind, its container's canonical form and its `AstId`,
//! orders what the notes' §8 lets differ in order, and keeps everything else as it is.
//! [`compare`] says, field by field, where two such forms differ.

mod active;
pub(in crate::nameres) mod differential;
mod features;
pub(in crate::nameres) mod generated;
mod graph;
mod impls;
mod trace;

use std::{fmt, fmt::Write as _, sync};

use base_db::{Crate, Db, all_crates};
use blaze_hash::{HashMap, HashSet};
use blaze_mem::Tag;
use either::Either;
use hir_expand::{
    AstId, ErasedAstId, InFile, MacroCallId,
    attrs::AttrId,
    fixture,
    name::Name,
    proc_macro::{
        CustomProcMacroExpander, ProcMacro, ProcMacroCall, ProcMacroExpander,
        ProcMacroExpansionError, ProcMacroKind,
    },
};
use intern::Symbol;
use la_arena::Idx;
use span::Edition;
use syntax::ast;
use triomphe::Arc;

use crate::{
    AdtId, BlockIdLt, BuiltinDeriveImplId, BuiltinType, DefWithBodyId, ExternBlockId,
    ExternCrateId, ImplId, ItemContainerId, Lookup, MacroExpander, MacroId, MacroRulesLocFlags,
    ModuleDefId, ModuleId, TraitId, UseId,
    builtin_derive::BuiltinDeriveImplTrait,
    expr_store::Body,
    item_scope::{
        DeriveMacroInvocation, GlobAmbiguity, GlobId, ImportId, ImportOrDef, ImportOrExternCrate,
        ImportOrGlob, ItemInNs, ItemScope, ShadowedImport, UnderscoreImport,
    },
    item_tree::TreeId,
    nameres::{
        BlockInfo, DefMap, DefMapCrateData, DefMapPair, LocalDefMap, ModuleData, ModuleOrigin,
        ModulesSlot, ScopeSlot, collector, crate_local_def_map,
        diagnostics::{DefDiagnostic, DefDiagnosticKind},
    },
    per_ns::Item,
    test_db::TestDB,
    tt::TopSubtree,
    unstable_features::UnstableFeatures,
    visibility::{Visibility, VisibilityExplicitness},
};

/// The crate's def map as the old collector made it: `crate_local_def_map` before the per-module
/// resolver (`nameres.rs`, without `frozen`, which serves and checks images and is the same for
/// both). A query of its own, so the tracked structs it makes live in one.
#[blaze_query::tracked(returns(ref))]
pub(in crate::nameres) fn old_crate_local_def_map(db: &Db, krate: Crate) -> DefMapPair<'_> {
    let root_file_id = krate.root_file_id(db);
    let module_data = ModuleData::new(
        ModuleOrigin::CrateRoot { definition: root_file_id },
        Visibility::Public,
        None,
    );
    let def_map = DefMap::empty(
        db,
        krate,
        Arc::new(DefMapCrateData::new(krate.data(db).edition)),
        module_data,
        None,
    );
    let (mut def_map, local_def_map) =
        collector::collect_defs(db, def_map, TreeId::new(root_file_id.into(), None), None);
    def_map.hand_over_scopes(db);
    DefMapPair::new(db, def_map, local_def_map)
}

/// The crate's def map as the per-module resolver makes it, without `frozen`.
#[blaze_query::tracked(returns(ref))]
pub(in crate::nameres) fn new_crate_local_def_map(db: &Db, krate: Crate) -> DefMapPair<'_> {
    blaze_mem::frame::task(Tag::Nameres, |frame| {
        let (def_map, local_def_map) = super::collect_crate(db, frame, krate);
        DefMapPair::new(db, def_map, local_def_map)
    })
}

/// The empty def map of `block` that `block_def_map` starts from, and the tree it collects.
fn empty_block_def_map(db: &Db, block_id: BlockIdLt<'_>) -> (DefMap, TreeId) {
    let block_id = block_id.to_static();
    let ast_id = block_id.ast_id(db);
    let module = block_id.module(db).to_static();
    let visibility = Visibility::Module(module, VisibilityExplicitness::Implicit);
    let module_data =
        ModuleData::new(ModuleOrigin::BlockExpr { block: ast_id, id: block_id }, visibility, None);
    let krate = module.krate(db);
    let def_map = DefMap::empty(
        db,
        krate,
        crate_local_def_map(db, krate).def_map(db).data.clone(),
        module_data,
        Some(BlockInfo { block: block_id, parent: module }),
    );
    (def_map, TreeId::new(ast_id.file_id, Some(block_id)))
}

/// `block`'s def map as the old collector made it (`block_def_map` before the resolver).
#[blaze_query::tracked(returns(ref))]
pub(super) fn old_block_def_map<'db>(
    db: &'db Db,
    block_id: BlockIdLt<'db>,
) -> DefMap {
    let (def_map, tree_id) = empty_block_def_map(db, block_id);
    let krate = def_map.krate;
    let local = crate_local_def_map(db, krate).local(db);
    let (mut def_map, _) = collector::collect_defs(db, def_map, tree_id, Some(local));
    def_map.hand_over_scopes(db);
    def_map
}

/// `block`'s def map as the per-module resolver makes it.
#[blaze_query::tracked(returns(ref))]
pub(super) fn new_block_def_map<'db>(
    db: &'db Db,
    block_id: BlockIdLt<'db>,
) -> DefMap {
    blaze_mem::frame::task(Tag::Nameres, |frame| {
        let (def_map, tree_id) = empty_block_def_map(db, block_id);
        let krate = def_map.krate;
        let local = crate_local_def_map(db, krate).local(db);
        super::collect_block(db, frame, def_map, tree_id, local)
    })
}

// ---------------------------------------------------------------------------------------------
// The canonical form.

/// A module of the map being projected, by where it is: the names from its map's root and its
/// origin, which tells apart two modules of one path (a `mod` declared twice).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ModKey {
    pub(super) path: Box<[Name]>,
    pub(super) origin: ModuleOrigin,
}

impl ModKey {
    pub(super) fn display(&self) -> String {
        let mut out = String::from("crate");
        for name in &self.path {
            out.push_str("::");
            out.push_str(name.as_str());
        }
        out
    }
}

/// A module as the projection names it: by its [`ModKey`] when it belongs to the map, or itself
/// when it belongs to a map both sides share (a dependency's, an enclosing block's).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum CModule {
    Local(ModKey),
    Foreign(ModuleId),
}

/// An item whose loc is a module and an `AstId`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CLoc {
    pub(super) container: CModule,
    pub(super) ast: ErasedAstId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum CContainer {
    Module(CModule),
    ExternBlock(CLoc),
    Impl(CLoc),
    Trait(CLoc),
}

/// An item whose loc is an `ItemContainerId` and an `AstId`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CAssoc {
    pub(super) container: CContainer,
    pub(super) ast: ErasedAstId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum CMacro {
    Macro2 { loc: CLoc, expander: MacroExpander, allow_internal_unsafe: bool, edition: Edition },
    Rules { loc: CLoc, expander: MacroExpander, flags: MacroRulesLocFlags, edition: Edition },
    Proc { loc: CLoc, expander: CustomProcMacroExpander, kind: ProcMacroKind, edition: Edition },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum CDef {
    Module(CModule),
    Function(CAssoc),
    Struct(CLoc),
    Union(CLoc),
    Enum(CLoc),
    Variant { parent: CLoc, ast: ErasedAstId, name: Name },
    Const(CAssoc),
    Static(CAssoc),
    Trait(CLoc),
    TypeAlias(CAssoc),
    Builtin(BuiltinType),
    Macro(CMacro),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum CVis {
    Module(CModule, VisibilityExplicitness),
    PubCrate(Crate),
    Public,
}

/// A use tree: an `ImportId` or a `GlobId`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CUseTree {
    pub(super) use_: CLoc,
    pub(super) idx: Idx<ast::UseTree>,
}

/// What `ImportOrExternCrate`, `ImportOrGlob` and `ImportOrDef` name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum CImport {
    Import(CUseTree),
    Glob(CUseTree),
    ExternCrate(CLoc),
    Def(CDef),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CItem<D> {
    pub(super) def: D,
    pub(super) vis: CVis,
    pub(super) import: Option<CImport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CBuiltinDerive {
    pub(super) adt: CDef,
    pub(super) trait_: BuiltinDeriveImplTrait,
    pub(super) derive_attr_id: AttrId,
    pub(super) derive_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CDeriveInvocation {
    pub(super) attr_id: AttrId,
    pub(super) attr_call_id: MacroCallId,
    pub(super) derive_call_ids: Vec<Option<Either<MacroCallId, CBuiltinDerive>>>,
}

/// An `ItemScope`. The three namespaces are sorted by name, every other list and map sorted by
/// its canonical form: the notes' §8 lets their order follow the schedule. The macros of one
/// name in `legacy_macros` keep their order, which is definition order and decides which one a
/// call names.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct CScope {
    pub(super) types: Vec<(Name, CItem<CDef>)>,
    pub(super) values: Vec<(Name, CItem<CDef>)>,
    pub(super) macros: Vec<(Name, CItem<CMacro>)>,
    pub(super) unresolved: Vec<Name>,
    pub(super) declarations: Vec<CDef>,
    /// Each impl with whether it is a trait impl and whether it is a reservation impl.
    pub(super) impls: Vec<(CLoc, bool, bool)>,
    pub(super) builtin_derive_impls: Vec<CBuiltinDerive>,
    pub(super) extern_blocks: Vec<CLoc>,
    pub(super) unnamed_consts: Vec<CAssoc>,
    pub(super) unnamed_trait_imports: Vec<(CLoc, CItem<()>)>,
    /// Each `use path as _;` with what it named in the types namespace and its visibility.
    pub(super) underscore_imports: Vec<(CImport, Option<CDef>, CVis)>,
    /// Name, the other item, the glob, visibility.
    pub(super) glob_ambiguities: Vec<(Name, CItemInNs, CImport, CVis)>,
    /// Import, name, item, visibility.
    pub(super) shadowed_imports: Vec<(CImport, Name, CItemInNs, CVis)>,
    pub(super) use_imports_types: Vec<(CImport, CImport)>,
    pub(super) use_imports_values: Vec<(CImport, CImport)>,
    pub(super) use_imports_macros: Vec<(CImport, CImport)>,
    pub(super) use_decls: Vec<CLoc>,
    pub(super) extern_crate_decls: Vec<CLoc>,
    pub(super) legacy_macros: Vec<(Name, Vec<CMacro>)>,
    pub(super) attr_macros: Vec<(ErasedAstId, MacroCallId)>,
    pub(super) macro_invocations: Vec<(ErasedAstId, MacroCallId)>,
    pub(super) derive_macros: Vec<(ErasedAstId, Vec<CDeriveInvocation>)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum CItemInNs {
    Types(CDef),
    Values(CDef),
    Macros(CMacro),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct CModuleData<'a> {
    pub(super) origin: ModuleOrigin,
    pub(super) visibility: CVis,
    pub(super) parent: Option<CModule>,
    pub(super) children: Vec<(Name, CModule)>,
    pub(super) scope: CScope,
    /// The module's diagnostics, sorted.
    pub(super) diagnostics: Vec<&'a DefDiagnosticKind>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct CCrateData<'a> {
    pub(super) exported_derives: Vec<(CMacro, Box<[Name]>)>,
    pub(super) fn_proc_macro_mapping: Vec<(CAssoc, CMacro)>,
    pub(super) fn_proc_macro_mapping_back: Vec<(CMacro, CAssoc)>,
    pub(super) registered_tools: Vec<Symbol>,
    pub(super) unstable_features: &'a UnstableFeatures,
    pub(super) rustc_coherence_is_core: bool,
    pub(super) no_core: bool,
    pub(super) no_std: bool,
    pub(super) proc_macro: bool,
    pub(super) edition: Edition,
    pub(super) recursion_limit: Option<u32>,
}

/// A `DefMap` (and, for a crate, its `LocalDefMap`) in canonical form.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::nameres) struct CDefMap<'a> {
    pub(super) krate: Crate,
    pub(super) block: Option<BlockInfo>,
    pub(super) root: CModule,
    /// Sorted by path, then origin.
    pub(super) modules: Vec<(ModKey, CModuleData<'a>)>,
    pub(super) prelude: Option<(CModule, Option<CLoc>)>,
    pub(super) macro_use_prelude: Vec<(Name, CMacro, Option<CLoc>)>,
    pub(super) derive_helpers_in_scope:
        Vec<(ErasedAstId, Vec<(Name, CMacro, Either<MacroCallId, CBuiltinDerive>)>)>,
    pub(super) session_macros: bool,
    /// Diagnostics whose module is not one of the map's, sorted.
    pub(super) stray_diagnostics: Vec<(CModule, &'a DefDiagnosticKind)>,
    pub(super) data: CCrateData<'a>,
    /// The extern prelude in its order (a crate's map only).
    pub(super) extern_prelude: Option<Vec<(Name, CModule, Option<CLoc>)>>,
}

/// Names the projection leaves out: for the notes' §8 fixtures whose answer depends on the
/// schedule, the names whose resolution does (in every module), and whether unresolved-import
/// diagnostics do.
#[derive(Debug, Clone, Copy, Default)]
pub(in crate::nameres) struct Exempt {
    pub(in crate::nameres) names: &'static [&'static str],
    pub(in crate::nameres) unresolved_imports: bool,
}

impl Exempt {
    pub(in crate::nameres) const NONE: Exempt = Exempt { names: &[], unresolved_imports: false };

    fn name(&self, name: &Name) -> bool {
        self.names.contains(&name.as_str())
    }
}

fn erase<N>(id: AstId<N>) -> ErasedAstId {
    InFile::new(id.file_id, id.value.erase())
}

/// Sorts by the `Debug` text: canonical forms hold no id that differs between the sides, so
/// the text is the same for equal values on both.
fn sorted<T: fmt::Debug>(mut items: Vec<T>) -> Vec<T> {
    items.sort_by_cached_key(|it| format!("{it:?}"));
    items
}

struct Projector<'a> {
    db: &'a Db,
    local: HashMap<ModuleId, ModKey>,
}

impl<'a> Projector<'a> {
    fn new(db: &'a Db, def_map: &DefMap) -> Self {
        let table = def_map.table(db);
        let mut local = HashMap::default();
        for (id, _) in table.iter() {
            let mut path = Vec::new();
            let mut at = id;
            while let Some(parent) = table[at].parent {
                path.push(at.name(db).unwrap_or_else(|| {
                    panic!("module {at:?} has a parent and no name in the map of {:?}", def_map.krate)
                }));
                at = parent;
            }
            path.reverse();
            local.insert(id, ModKey { path: path.into(), origin: table[id].origin });
        }
        Projector { db, local }
    }

    fn module(&self, id: ModuleId) -> CModule {
        match self.local.get(&id) {
            Some(key) => CModule::Local(key.clone()),
            None => CModule::Foreign(id),
        }
    }

    fn vis(&self, vis: Visibility) -> CVis {
        match vis {
            Visibility::Module(module, explicitness) => CVis::Module(self.module(module), explicitness),
            Visibility::PubCrate(krate) => CVis::PubCrate(krate),
            Visibility::Public => CVis::Public,
        }
    }

    fn loc<N>(&self, container: ModuleId, id: AstId<N>) -> CLoc {
        CLoc { container: self.module(container), ast: erase(id) }
    }

    fn container(&self, container: ItemContainerId) -> CContainer {
        match container {
            ItemContainerId::ModuleId(it) => CContainer::Module(self.module(it)),
            ItemContainerId::ExternBlockId(it) => CContainer::ExternBlock(self.extern_block(it)),
            ItemContainerId::ImplId(it) => CContainer::Impl(self.impl_(it)),
            ItemContainerId::TraitId(it) => CContainer::Trait(self.trait_(it)),
        }
    }

    fn assoc<N>(&self, container: ItemContainerId, id: AstId<N>) -> CAssoc {
        CAssoc { container: self.container(container), ast: erase(id) }
    }

    fn extern_block(&self, id: ExternBlockId) -> CLoc {
        let loc = id.lookup(self.db);
        self.loc(loc.container, loc.id)
    }

    fn impl_(&self, id: ImplId) -> CLoc {
        let loc = id.lookup(self.db);
        self.loc(loc.container, loc.id)
    }

    fn trait_(&self, id: TraitId) -> CLoc {
        let loc = id.lookup(self.db);
        self.loc(loc.container, loc.id)
    }

    fn use_(&self, id: UseId) -> CLoc {
        let loc = id.lookup(self.db);
        self.loc(loc.container, loc.id)
    }

    fn extern_crate(&self, id: ExternCrateId) -> CLoc {
        let loc = id.lookup(self.db);
        self.loc(loc.container, loc.id)
    }

    fn adt(&self, id: AdtId) -> CDef {
        match id {
            AdtId::StructId(it) => {
                let loc = it.lookup(self.db);
                CDef::Struct(self.loc(loc.container, loc.id))
            }
            AdtId::UnionId(it) => {
                let loc = it.lookup(self.db);
                CDef::Union(self.loc(loc.container, loc.id))
            }
            AdtId::EnumId(it) => {
                let loc = it.lookup(self.db);
                CDef::Enum(self.loc(loc.container, loc.id))
            }
        }
    }

    fn makro(&self, id: MacroId) -> CMacro {
        match id {
            MacroId::Macro2Id(it) => {
                let loc = it.lookup(self.db);
                CMacro::Macro2 {
                    loc: self.loc(loc.container, loc.id),
                    expander: loc.expander,
                    allow_internal_unsafe: loc.allow_internal_unsafe,
                    edition: loc.edition,
                }
            }
            MacroId::MacroRulesId(it) => {
                let loc = it.lookup(self.db);
                CMacro::Rules {
                    loc: self.loc(loc.container, loc.id),
                    expander: loc.expander,
                    flags: loc.flags,
                    edition: loc.edition,
                }
            }
            MacroId::ProcMacroId(it) => {
                let loc = it.lookup(self.db);
                CMacro::Proc {
                    loc: self.loc(loc.container, loc.id),
                    expander: loc.expander,
                    kind: loc.kind,
                    edition: loc.edition,
                }
            }
        }
    }

    fn def(&self, def: ModuleDefId) -> CDef {
        match def {
            ModuleDefId::ModuleId(it) => CDef::Module(self.module(it)),
            ModuleDefId::FunctionId(it) => {
                let loc = it.lookup(self.db);
                CDef::Function(self.assoc(loc.container, loc.id))
            }
            ModuleDefId::AdtId(it) => self.adt(it),
            ModuleDefId::EnumVariantId(it) => {
                let loc = it.lookup(self.db);
                let parent = loc.parent.lookup(self.db);
                CDef::Variant {
                    parent: self.loc(parent.container, parent.id),
                    ast: erase(loc.id),
                    name: loc.name.clone(),
                }
            }
            ModuleDefId::ConstId(it) => {
                let loc = it.lookup(self.db);
                CDef::Const(self.assoc(loc.container, loc.id))
            }
            ModuleDefId::StaticId(it) => {
                let loc = it.lookup(self.db);
                CDef::Static(self.assoc(loc.container, loc.id))
            }
            ModuleDefId::TraitId(it) => CDef::Trait(self.trait_(it)),
            ModuleDefId::TypeAliasId(it) => {
                let loc = it.lookup(self.db);
                CDef::TypeAlias(self.assoc(loc.container, loc.id))
            }
            ModuleDefId::BuiltinType(it) => CDef::Builtin(it),
            ModuleDefId::MacroId(it) => CDef::Macro(self.makro(it)),
        }
    }

    fn tree(&self, use_: UseId, idx: Idx<ast::UseTree>) -> CUseTree {
        CUseTree { use_: self.use_(use_), idx }
    }

    fn import_id(&self, ImportId { use_, idx }: ImportId) -> CImport {
        CImport::Import(self.tree(use_, idx))
    }

    fn glob_id(&self, GlobId { use_, idx }: GlobId) -> CImport {
        CImport::Glob(self.tree(use_, idx))
    }

    fn import_or_extern_crate(&self, it: ImportOrExternCrate) -> CImport {
        match it {
            ImportOrExternCrate::Glob(it) => self.glob_id(it),
            ImportOrExternCrate::Import(it) => self.import_id(it),
            ImportOrExternCrate::ExternCrate(it) => CImport::ExternCrate(self.extern_crate(it)),
        }
    }

    fn import_or_glob(&self, it: ImportOrGlob) -> CImport {
        match it {
            ImportOrGlob::Glob(it) => self.glob_id(it),
            ImportOrGlob::Import(it) => self.import_id(it),
        }
    }

    fn import_or_def(&self, it: ImportOrDef) -> CImport {
        match it {
            ImportOrDef::Import(it) => self.import_id(it),
            ImportOrDef::Glob(it) => self.glob_id(it),
            ImportOrDef::ExternCrate(it) => CImport::ExternCrate(self.extern_crate(it)),
            ImportOrDef::Def(it) => CImport::Def(self.def(it)),
        }
    }

    fn builtin_derive(&self, id: BuiltinDeriveImplId) -> CBuiltinDerive {
        let loc = id.loc(self.db);
        CBuiltinDerive {
            adt: self.adt(loc.adt),
            trait_: loc.trait_,
            derive_attr_id: loc.derive_attr_id,
            derive_index: loc.derive_index,
        }
    }

    fn macro_or_builtin(
        &self,
        it: Either<MacroCallId, BuiltinDeriveImplId>,
    ) -> Either<MacroCallId, CBuiltinDerive> {
        it.map_right(|it| self.builtin_derive(it))
    }

    fn scope(&self, scope: &ItemScope, exempt: &Exempt) -> CScope {
        let ItemScope {
            types,
            values,
            macros,
            unresolved,
            names: _,
            sorted: _,
            by_def: _,
            declarations,
            impls,
            trait_impl,
            reservation_impl,
            builtin_derive_impls,
            extern_blocks,
            unnamed_consts,
            unnamed_trait_imports,
            underscore_imports,
            glob_ambiguities,
            shadowed_imports,
            use_imports_types,
            use_imports_values,
            use_imports_macros,
            use_decls,
            extern_crate_decls,
            legacy_macros,
            attr_macros,
            macro_invocations,
            derive_macros,
        } = scope;

        // Imports an exempt name's entry resolved through: their chains are left out too.
        let mut exempt_imports: HashSet<CImport> = HashSet::default();
        let column = |exempt_imports: &mut HashSet<CImport>,
                          name: &Name,
                          import: Option<CImport>| {
            if exempt.name(name) {
                exempt_imports.extend(import);
                false
            } else {
                true
            }
        };
        // A name two globs give different items is ambiguous (an error where a path uses it):
        // which of them it is bound to follows the order the globs were resolved in, which the
        // collectors don't share. Its glob binding is listed with the other items it is
        // ambiguous with, so both compare as the same set.
        let ambiguous: HashSet<(Name, u8)> = glob_ambiguities
            .iter()
            .map(|it| {
                let ns = match it.other {
                    ItemInNs::Types(_) => 0,
                    ItemInNs::Values(_) => 1,
                    ItemInNs::Macros(_) => 2,
                };
                (it.name, ns)
            })
            .collect();
        let mut c_ambiguities = Vec::new();
        let mut c_types = Vec::new();
        for (name, item) in types.iter() {
            let import = item.import.map(|it| self.import_or_extern_crate(it));
            if ambiguous.contains(&(*name, 0)) && matches!(import, Some(CImport::Glob(_))) && !exempt.name(name) {
                let vis = self.vis(item.vis);
                c_ambiguities.push((*name, CItemInNs::Types(self.def(item.def)), import.expect("a glob"), vis));
                continue;
            }
            if column(&mut exempt_imports, name, import.clone()) {
                c_types.push((
                    name.clone(),
                    CItem { def: self.def(item.def), vis: self.vis(item.vis), import },
                ));
            }
        }
        let mut c_values = Vec::new();
        for (name, item) in values.iter() {
            let import = item.import.map(|it| self.import_or_glob(it));
            if ambiguous.contains(&(*name, 1)) && matches!(import, Some(CImport::Glob(_))) && !exempt.name(name) {
                let vis = self.vis(item.vis);
                c_ambiguities.push((*name, CItemInNs::Values(self.def(item.def)), import.expect("a glob"), vis));
                continue;
            }
            if column(&mut exempt_imports, name, import.clone()) {
                c_values.push((
                    name.clone(),
                    CItem { def: self.def(item.def), vis: self.vis(item.vis), import },
                ));
            }
        }
        let mut c_macros = Vec::new();
        for (name, item) in macros.iter() {
            let import = item.import.map(|it| self.import_or_extern_crate(it));
            if ambiguous.contains(&(*name, 2)) && matches!(import, Some(CImport::Glob(_))) && !exempt.name(name) {
                let vis = self.vis(item.vis);
                c_ambiguities.push((*name, CItemInNs::Macros(self.makro(item.def)), import.expect("a glob"), vis));
                continue;
            }
            if column(&mut exempt_imports, name, import.clone()) {
                c_macros.push((
                    name.clone(),
                    CItem { def: self.makro(item.def), vis: self.vis(item.vis), import },
                ));
            }
        }
        let chain = |pairs: Vec<(CImport, CImport)>| {
            sorted(pairs.into_iter().filter(|(key, _)| !exempt_imports.contains(key)).collect())
        };

        CScope {
            types: sorted(c_types),
            values: sorted(c_values),
            macros: sorted(c_macros),
            unresolved: sorted(unresolved.iter().filter(|it| !exempt.name(it)).cloned().collect()),
            declarations: sorted(declarations.iter().map(|&it| self.def(it)).collect()),
            impls: sorted(
                impls
                    .iter()
                    .enumerate()
                    .map(|(at, &it)| (self.impl_(it), trait_impl.contains(at), reservation_impl.contains(at)))
                    .collect(),
            ),
            builtin_derive_impls: sorted(
                builtin_derive_impls.iter().map(|&it| self.builtin_derive(it)).collect(),
            ),
            extern_blocks: sorted(extern_blocks.iter().map(|&it| self.extern_block(it)).collect()),
            unnamed_consts: sorted(
                unnamed_consts
                    .iter()
                    .map(|&it| {
                        let loc = it.lookup(self.db);
                        self.assoc(loc.container, loc.id)
                    })
                    .collect(),
            ),
            unnamed_trait_imports: sorted(
                unnamed_trait_imports
                    .iter()
                    .map(|&(tr, item)| {
                        let Item { def: (), vis, import } = item;
                        (
                            self.trait_(tr),
                            CItem {
                                def: (),
                                vis: self.vis(vis),
                                import: import.map(|it| self.import_or_extern_crate(it)),
                            },
                        )
                    })
                    .collect(),
            ),
            underscore_imports: sorted(
                underscore_imports
                    .iter()
                    .map(|&UnderscoreImport { import, def, vis }| {
                        (self.import_id(import), def.map(|it| self.def(it)), self.vis(vis))
                    })
                    .collect(),
            ),
            glob_ambiguities: sorted(
                glob_ambiguities
                    .iter()
                    .filter(|it| !exempt.name(&it.name))
                    .map(|&GlobAmbiguity { name, other, glob, vis }| {
                        (name, self.item_in_ns(other), self.glob_id(glob), self.vis(vis))
                    })
                    .chain(c_ambiguities)
                    .collect(),
            ),
            shadowed_imports: sorted(
                shadowed_imports
                    .iter()
                    .filter(|it| !exempt.name(&it.name))
                    .map(|&ShadowedImport { import, name, item, vis }| {
                        (self.import_or_extern_crate(import), name, self.item_in_ns(item), self.vis(vis))
                    })
                    .collect(),
            ),
            use_imports_types: chain(
                use_imports_types
                    .iter()
                    .map(|(&k, &v)| (self.import_or_extern_crate(k), self.import_or_def(v)))
                    .collect(),
            ),
            use_imports_values: chain(
                use_imports_values
                    .iter()
                    .map(|(&k, &v)| (self.import_or_glob(k), self.import_or_def(v)))
                    .collect(),
            ),
            use_imports_macros: chain(
                use_imports_macros
                    .iter()
                    .map(|(&k, &v)| (self.import_or_extern_crate(k), self.import_or_def(v)))
                    .collect(),
            ),
            use_decls: sorted(use_decls.iter().map(|&it| self.use_(it)).collect()),
            extern_crate_decls: sorted(
                extern_crate_decls.iter().map(|&it| self.extern_crate(it)).collect(),
            ),
            legacy_macros: sorted(
                legacy_macros
                    .iter()
                    .map(|(name, macs)| {
                        (name.clone(), macs.iter().map(|&it| self.makro(it)).collect())
                    })
                    .collect(),
            ),
            attr_macros: sorted(attr_macros.iter().map(|(&k, &v)| (erase(k), v)).collect()),
            macro_invocations: sorted(
                macro_invocations.iter().map(|(&k, &v)| (erase(k), v)).collect(),
            ),
            derive_macros: sorted(
                derive_macros
                    .iter()
                    .map(|(&adt, invocations)| {
                        let invocations = invocations
                            .iter()
                            .map(|invocation| {
                                let DeriveMacroInvocation {
                                    attr_id,
                                    attr_call_id,
                                    derive_call_ids,
                                } = invocation;
                                CDeriveInvocation {
                                    attr_id: *attr_id,
                                    attr_call_id: *attr_call_id,
                                    derive_call_ids: derive_call_ids
                                        .iter()
                                        .map(|&it| it.map(|it| self.macro_or_builtin(it)))
                                        .collect(),
                                }
                            })
                            .collect();
                        (erase(adt), sorted(invocations))
                    })
                    .collect(),
            ),
        }
    }

    fn item_in_ns(&self, item: ItemInNs) -> CItemInNs {
        match item {
            ItemInNs::Types(def) => CItemInNs::Types(self.def(def)),
            ItemInNs::Values(def) => CItemInNs::Values(self.def(def)),
            ItemInNs::Macros(mac) => CItemInNs::Macros(self.makro(mac)),
        }
    }

    fn diagnostic_kind(kind: &DefDiagnosticKind, exempt: &Exempt) -> bool {
        !(exempt.unresolved_imports && matches!(kind, DefDiagnosticKind::UnresolvedImport { .. }))
    }

    fn crate_data<'d>(&self, data: &'d DefMapCrateData) -> CCrateData<'d> {
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
            edition,
            recursion_limit,
        } = data;
        let function = |it: crate::FunctionId| {
            let loc = it.lookup(self.db);
            self.assoc(loc.container, loc.id)
        };
        CCrateData {
            exported_derives: sorted(
                exported_derives.iter().map(|(&k, v)| (self.makro(k), v.clone())).collect(),
            ),
            fn_proc_macro_mapping: sorted(
                fn_proc_macro_mapping
                    .iter()
                    .map(|(&f, &p)| (function(f), self.makro(p.into())))
                    .collect(),
            ),
            fn_proc_macro_mapping_back: sorted(
                fn_proc_macro_mapping_back
                    .iter()
                    .map(|(&p, &f)| (self.makro(p.into()), function(f)))
                    .collect(),
            ),
            registered_tools: registered_tools.clone(),
            unstable_features,
            rustc_coherence_is_core: *rustc_coherence_is_core,
            no_core: *no_core,
            no_std: *no_std,
            proc_macro: *proc_macro,
            edition: *edition,
            recursion_limit: *recursion_limit,
        }
    }
}

/// `def_map` (with `local` for a crate's map) in canonical form, leaving out what `exempt` names.
pub(in crate::nameres) fn project<'a>(
    db: &'a Db,
    def_map: &'a DefMap,
    local: Option<&'a LocalDefMap>,
    exempt: &Exempt,
) -> CDefMap<'a> {
    let DefMap {
        krate,
        block,
        root,
        modules: _,
        prelude,
        macro_use_prelude,
        // Empty once handed over: read below from the records.
        derive_helpers_in_scope: _,
        session_macros,
        // Compared without ids by `driver.rs`'s `name_records_equal_the_old_ones`.
        names: _,
        diagnostics: _,
        diagnostics_by_module: _,
        records: _,
        data,
    } = def_map;
    let super::super::Records { diagnostics, diagnostics_by_module, derive_helpers_in_scope, .. } =
        def_map.records(db);
    let p = Projector::new(db, def_map);

    let mut by_module: HashMap<ModuleId, Vec<&DefDiagnosticKind>> = HashMap::default();
    let mut stray = Vec::new();
    for DefDiagnostic { in_module, kind } in diagnostics {
        if !Projector::diagnostic_kind(kind, exempt) {
            continue;
        }
        if p.local.contains_key(in_module) {
            by_module.entry(*in_module).or_default().push(kind);
        } else {
            stray.push((p.module(*in_module), kind));
        }
    }

    let mut modules = Vec::new();
    for (id, data) in def_map.table(db).iter() {
        let ModuleData { origin, visibility, parent, scope: _ } = data;
        let grouped: Vec<&DefDiagnosticKind> = diagnostics_by_module
            .of_module(id)
            .iter()
            .map(|&at| &diagnostics[at as usize].kind)
            .filter(|kind| Projector::diagnostic_kind(kind, exempt))
            .collect();
        let pushed = by_module.remove(&id).unwrap_or_default();
        assert!(
            grouped.len() == pushed.len() && grouped.iter().zip(&pushed).all(|(a, b)| a == b),
            "`diagnostics_by_module` of {} disagrees with `diagnostics`",
            p.local[&id].display(),
        );
        modules.push((
            p.local[&id].clone(),
            CModuleData {
                origin: *origin,
                visibility: p.vis(*visibility),
                parent: parent.map(|it| p.module(it)),
                children: sorted(
                    data.children(db).iter().map(|(name, &it)| (name.clone(), p.module(it))).collect(),
                ),
                scope: p.scope(data.scope(db), exempt),
                diagnostics: sorted(pushed),
            },
        ));
    }
    modules.sort_by_cached_key(|(key, _)| {
        (key.path.iter().map(|it| it.as_str().to_owned()).collect::<Vec<_>>(), format!("{:?}", key.origin))
    });

    CDefMap {
        krate: *krate,
        block: *block,
        root: p.module(*root),
        modules,
        prelude: prelude.map(|(module, use_)| (p.module(module), use_.map(|it| p.use_(it)))),
        macro_use_prelude: sorted(
            macro_use_prelude
                .iter()
                .map(|(name, &(mac, ext))| {
                    (name.clone(), p.makro(mac), ext.map(|it| p.extern_crate(it)))
                })
                .collect(),
        ),
        derive_helpers_in_scope: sorted(
            derive_helpers_in_scope
                .iter()
                .map(|(&item, helpers)| {
                    let helpers = helpers
                        .iter()
                        .map(|(name, mac, call)| {
                            (name.clone(), p.makro(*mac), p.macro_or_builtin(*call))
                        })
                        .collect();
                    (erase(item), sorted(helpers))
                })
                .collect(),
        ),
        session_macros: *session_macros,
        stray_diagnostics: sorted(stray),
        data: p.crate_data(data),
        extern_prelude: local.map(|LocalDefMap { extern_prelude }| {
            extern_prelude
                .iter()
                .map(|(name, &(module, ext))| {
                    (name.clone(), p.module(module), ext.map(|it| p.extern_crate(it)))
                })
                .collect()
        }),
    }
}

// ---------------------------------------------------------------------------------------------
// Comparing.

fn list<T: PartialEq + fmt::Debug>(out: &mut String, at: &str, field: &str, old: &[T], new: &[T]) {
    if old == new {
        return;
    }
    let missing: Vec<&T> = old.iter().filter(|it| !new.contains(it)).collect();
    let extra: Vec<&T> = new.iter().filter(|it| !old.contains(it)).collect();
    _ = writeln!(out, "{at}: `{field}` differs");
    if missing.is_empty() && extra.is_empty() {
        _ = writeln!(out, "  same entries, other multiplicity or order\n  old: {old:#?}\n  new: {new:#?}");
    } else {
        for it in missing {
            _ = writeln!(out, "  only old: {it:?}");
        }
        for it in extra {
            _ = writeln!(out, "  only new: {it:?}");
        }
    }
}

fn value<T: PartialEq + fmt::Debug>(out: &mut String, at: &str, field: &str, old: &T, new: &T) {
    if old != new {
        _ = writeln!(out, "{at}: `{field}` differs\n  old: {old:?}\n  new: {new:?}");
    }
}

fn compare_scope(out: &mut String, at: &str, old: &CScope, new: &CScope) {
    let CScope {
        types,
        values,
        macros,
        unresolved,
        declarations,
        impls,
        builtin_derive_impls,
        extern_blocks,
        unnamed_consts,
        unnamed_trait_imports,
        underscore_imports,
        glob_ambiguities,
        shadowed_imports,
        use_imports_types,
        use_imports_values,
        use_imports_macros,
        use_decls,
        extern_crate_decls,
        legacy_macros,
        attr_macros,
        macro_invocations,
        derive_macros,
    } = old;
    list(out, at, "types", types, &new.types);
    list(out, at, "values", values, &new.values);
    list(out, at, "macros", macros, &new.macros);
    list(out, at, "unresolved", unresolved, &new.unresolved);
    list(out, at, "declarations", declarations, &new.declarations);
    list(out, at, "impls", impls, &new.impls);
    list(out, at, "builtin_derive_impls", builtin_derive_impls, &new.builtin_derive_impls);
    list(out, at, "extern_blocks", extern_blocks, &new.extern_blocks);
    list(out, at, "unnamed_consts", unnamed_consts, &new.unnamed_consts);
    list(out, at, "unnamed_trait_imports", unnamed_trait_imports, &new.unnamed_trait_imports);
    list(out, at, "underscore_imports", underscore_imports, &new.underscore_imports);
    list(out, at, "glob_ambiguities", glob_ambiguities, &new.glob_ambiguities);
    list(out, at, "shadowed_imports", shadowed_imports, &new.shadowed_imports);
    list(out, at, "use_imports_types", use_imports_types, &new.use_imports_types);
    list(out, at, "use_imports_values", use_imports_values, &new.use_imports_values);
    list(out, at, "use_imports_macros", use_imports_macros, &new.use_imports_macros);
    list(out, at, "use_decls", use_decls, &new.use_decls);
    list(out, at, "extern_crate_decls", extern_crate_decls, &new.extern_crate_decls);
    list(out, at, "legacy_macros", legacy_macros, &new.legacy_macros);
    list(out, at, "attr_macros", attr_macros, &new.attr_macros);
    list(out, at, "macro_invocations", macro_invocations, &new.macro_invocations);
    list(out, at, "derive_macros", derive_macros, &new.derive_macros);
}

/// Where `old` and `new` differ, one finding per line block; empty when they are equal.
pub(in crate::nameres) fn compare(old: &CDefMap<'_>, new: &CDefMap<'_>) -> String {
    let mut out = String::new();
    let CDefMap {
        krate,
        block,
        root,
        modules,
        prelude,
        macro_use_prelude,
        derive_helpers_in_scope,
        session_macros,
        stray_diagnostics,
        data,
        extern_prelude,
    } = old;
    let at = "def map";
    value(&mut out, at, "krate", krate, &new.krate);
    value(&mut out, at, "block", block, &new.block);
    value(&mut out, at, "root", root, &new.root);
    value(&mut out, at, "prelude", prelude, &new.prelude);
    list(&mut out, at, "macro_use_prelude", macro_use_prelude, &new.macro_use_prelude);
    list(
        &mut out,
        at,
        "derive_helpers_in_scope",
        derive_helpers_in_scope,
        &new.derive_helpers_in_scope,
    );
    value(&mut out, at, "session_macros", session_macros, &new.session_macros);
    list(&mut out, at, "stray_diagnostics", stray_diagnostics, &new.stray_diagnostics);
    value(&mut out, at, "data", data, &new.data);
    value(&mut out, at, "extern_prelude", extern_prelude, &new.extern_prelude);

    let old_keys: Vec<&ModKey> = modules.iter().map(|(key, _)| key).collect();
    let new_keys: Vec<&ModKey> = new.modules.iter().map(|(key, _)| key).collect();
    list(&mut out, at, "modules", &old_keys, &new_keys);
    for (key, old) in modules {
        let Some((_, new)) = new.modules.iter().find(|(it, _)| it == key) else { continue };
        let at = key.display();
        let CModuleData { origin, visibility, parent, children, scope, diagnostics } = old;
        value(&mut out, &at, "origin", origin, &new.origin);
        value(&mut out, &at, "visibility", visibility, &new.visibility);
        value(&mut out, &at, "parent", parent, &new.parent);
        list(&mut out, &at, "children", children, &new.children);
        compare_scope(&mut out, &at, scope, &new.scope);
        list(&mut out, &at, "diagnostics", diagnostics, &new.diagnostics);
    }
    out
}

/// Panics naming `what` unless every scope of `def_map` and its modules were handed to the
/// database, as `crate_local_def_map` and `block_def_map` return them.
pub(super) fn assert_handed_over(db: &Db, def_map: &DefMap, what: &str) {
    assert!(
        matches!(def_map.modules, ModulesSlot::Collected(_)),
        "{what}: the modules of the def map were not handed over"
    );
    for (id, data) in def_map.table(db).iter() {
        assert!(
            matches!(data.scope, ScopeSlot::Collected(_)),
            "{what}: the scope of {id:?} was not handed over"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Running a fixture.

/// The `identity_when_valid` attribute macro `macro_expansion_tests` gives every fixture's
/// proc-macro crate: the item back when it parses as items, an error otherwise (the original
/// panics; an error keeps one bad fixture from hiding the others' differences).
#[derive(Debug)]
struct IdentityWhenValid;

impl ProcMacroExpander for IdentityWhenValid {
    fn expand(
        &self,
        _: &Db,
        ProcMacroCall { subtree, .. }: ProcMacroCall<'_>,
    ) -> Result<TopSubtree, ProcMacroExpansionError> {
        let (parse, _) = syntax_bridge::token_tree_to_syntax_node(
            subtree.view(),
            syntax_bridge::TopEntryPoint::MacroItems,
            &mut |_| Edition::CURRENT,
        );
        if parse.errors().is_empty() {
            Ok(subtree.copy())
        } else {
            Err(ProcMacroExpansionError::Panic(format!(
                "got invalid macro input: {:?}",
                parse.errors()
            )))
        }
    }

    // dyn: `ProcMacroExpander::eq_dyn`'s signature, compared when a crate's macros are set
    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        other.type_id() == std::any::TypeId::of::<Self>()
    }
}

pub(in crate::nameres) fn extra_proc_macros() -> Vec<(String, ProcMacro)> {
    vec![(
        r#"
#[proc_macro_attribute]
pub fn identity_when_valid(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}
"#
        .into(),
        ProcMacro {
            name: Symbol::intern("identity_when_valid"),
            kind: ProcMacroKind::Attr,
            expander: sync::Arc::new(IdentityWhenValid),
            disabled: false,
        },
    )]
}

/// A database of `fixture`, which may mark a position (`$0`): the comparisons read none.
pub(in crate::nameres) fn fixture_db(fixture: &str) -> TestDB {
    fixture::db_with_proc_macros(fixture, extra_proc_macros()).into_db()
}

/// Runs `f` with the builtin-derive fast path on or off (`ENABLE_BUILTIN_DERIVE_FAST_PATH`),
/// putting the setting back after, also when `f` panics.
pub(in crate::nameres) fn with_fast_path<R>(on: bool, f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            crate::nameres::ENABLE_BUILTIN_DERIVE_FAST_PATH.set(self.0);
        }
    }
    let _restore = Restore(crate::nameres::ENABLE_BUILTIN_DERIVE_FAST_PATH.replace(on));
    f()
}

/// Every body of the items of `def_map`'s modules: functions, consts, statics and enum
/// variants, and the items of its impls and traits.
fn bodies(db: &Db, def_map: &DefMap) -> Vec<DefWithBodyId> {
    let mut out = Vec::new();
    let assoc = |out: &mut Vec<DefWithBodyId>, items: &[(Name, crate::AssocItemId)]| {
        for &(_, item) in items {
            match item {
                crate::AssocItemId::FunctionId(it) => out.push(it.into()),
                crate::AssocItemId::ConstId(it) => out.push(it.into()),
                crate::AssocItemId::TypeAliasId(_) => {}
            }
        }
    };
    for (_, data) in def_map.modules(db) {
        let scope = data.scope(db);
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(it) => out.push(it.into()),
                ModuleDefId::ConstId(it) => out.push(it.into()),
                ModuleDefId::StaticId(it) => out.push(it.into()),
                ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                    let variants = it.enum_variants(db).variants.values();
                    out.extend(variants.map(|&(it, _)| DefWithBodyId::VariantId(it)))
                }
                ModuleDefId::TraitId(it) => assoc(&mut out, &it.trait_items(db).items),
                ModuleDefId::ModuleId(_)
                | ModuleDefId::AdtId(_)
                | ModuleDefId::EnumVariantId(_)
                | ModuleDefId::TypeAliasId(_)
                | ModuleDefId::BuiltinType(_)
                | ModuleDefId::MacroId(_) => {}
            }
        }
        for it in scope.impls() {
            assoc(&mut out, &it.impl_items(db).items);
        }
        for it in scope.unnamed_consts() {
            out.push(it.into());
        }
    }
    out
}

/// Compares the old and the new def map of every crate of `db`, and of every block of their
/// bodies (found through the database's own def maps, which both sides of a block share as
/// upstream), appending each difference to `out` under `name`.
pub(super) fn compare_db(db: &TestDB, name: &str, exempt: &Exempt, out: &mut String) {
    for &krate in all_crates(db).iter() {
        let old = old_crate_local_def_map(db, krate);
        let new = new_crate_local_def_map(db, krate);
        assert_handed_over(db, new.def_map(db), name);
        let diff = compare(
            &project(db, old.def_map(db), Some(old.local(db)), exempt),
            &project(db, new.def_map(db), Some(new.local(db)), exempt),
        );
        if !diff.is_empty() {
            _ = writeln!(out, "== {name}: crate {:?}\n{diff}", krate.extra_data(db).display_name);
        }

        let mut maps = vec![crate_local_def_map(db, krate).def_map(db)];
        while let Some(map) = maps.pop() {
            for body in bodies(db, map) {
                for (block, real) in Body::of(db, body).blocks(db) {
                    let block = block.to_db(db);
                    let old = old_block_def_map(db, block);
                    let new = new_block_def_map(db, block);
                    assert_handed_over(db, new, name);
                    let diff =
                        compare(&project(db, old, None, exempt), &project(db, new, None, exempt));
                    if !diff.is_empty() {
                        _ = writeln!(out, "== {name}: block {:?} of {body:?}\n{diff}", block.ast_id(db));
                    }
                    maps.push(real);
                }
            }
        }
    }
}

/// Compares `fixture` with the builtin-derive fast path on and off, each in a database of its
/// own (the fast path is read once per collection, and memos would carry it over).
pub(super) fn compare_fixture(name: &str, fixture: &str, exempt: &Exempt, out: &mut String) {
    for on in [true, false] {
        with_fast_path(on, || {
            let db = fixture_db(fixture);
            compare_db(&db, &format!("{name} (fast path {on})"), exempt, out);
        });
    }
}
