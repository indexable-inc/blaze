//! A module's items and names as the type checker asks them (`def_map[module].scope`), over
//! the records (docs/v2/plan/design/reconnect.md, "Narrow def-map accessors").
//!
//! [`item_scope`] reads two records of a module once: its `ModuleScope` (the names in scope,
//! in every namespace) and the key-index page of its module (the items declared in it, in the
//! order they are written, for `declarations`, `impls` and the rest). The import that brought a
//! name is not in the scope record (a binding says only that it is an import), so every
//! [`Item::import`] here is `None`; the uses that need it (unused-import lints) are listed in
//! the design. What a module's macro calls made is in `Expansion` records, not built: those
//! accessors stop with `NotBuilt`.

use std::alloc::Global;

use base_db::{Crate, Db, OrNeed, SurfaceError};
use blaze_surface::{ItemKind, Items, NotBuilt};
use hir_expand::{AstId, MacroCallId, name::Name};
use la_arena::Idx;
use syntax::ast;

use crate::{
    AdtId, BuiltinDeriveImplId, ConstId, ExternBlockId, ExternCrateId, HasModule, ImplId,
    MacroId, ModuleDefId, ModuleId, TraitId, UseId,
    key::{self, FromKey},
    nameres::{ModuleScope, scope_key},
    per_ns::{Item, Namespace, PerNs},
    signatures::ImplSignature,
    visibility::Visibility,
};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum ImportOrExternCrate {
    Glob(GlobId),
    Import(ImportId),
    ExternCrate(ExternCrateId),
}

impl From<ImportOrGlob> for ImportOrExternCrate {
    fn from(it: ImportOrGlob) -> Self {
        match it {
            ImportOrGlob::Glob(it) => ImportOrExternCrate::Glob(it),
            ImportOrGlob::Import(it) => ImportOrExternCrate::Import(it),
        }
    }
}

impl ImportOrExternCrate {
    pub fn import_or_glob(self) -> Option<ImportOrGlob> {
        match self {
            ImportOrExternCrate::Import(it) => Some(ImportOrGlob::Import(it)),
            ImportOrExternCrate::Glob(it) => Some(ImportOrGlob::Glob(it)),
            ImportOrExternCrate::ExternCrate(_) => None,
        }
    }

    pub fn import(self) -> Option<ImportId> {
        match self {
            ImportOrExternCrate::Import(it) => Some(it),
            _ => None,
        }
    }

    pub fn glob(self) -> Option<GlobId> {
        match self {
            ImportOrExternCrate::Glob(id) => Some(id),
            _ => None,
        }
    }

    pub fn use_(self) -> Option<UseId> {
        match self {
            ImportOrExternCrate::Glob(id) => Some(id.use_),
            ImportOrExternCrate::Import(id) => Some(id.use_),
            ImportOrExternCrate::ExternCrate(_) => None,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum ImportOrGlob {
    Glob(GlobId),
    Import(ImportId),
}

impl ImportOrGlob {
    pub fn into_import(self) -> Option<ImportId> {
        match self {
            ImportOrGlob::Import(it) => Some(it),
            ImportOrGlob::Glob(_) => None,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum ImportOrDef {
    Import(ImportId),
    Glob(GlobId),
    ExternCrate(ExternCrateId),
    Def(ModuleDefId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct ImportId {
    pub use_: UseId,
    pub idx: Idx<ast::UseTree>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct GlobId {
    pub use_: UseId,
    pub idx: Idx<ast::UseTree>,
}

/// Shadow mode for builtin type which can be shadowed by module.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum BuiltinShadowMode {
    /// Prefer user-defined modules (or other types) over builtins.
    Module,
    /// Prefer builtins over user-defined modules (but not other types).
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ItemInNs {
    Types(ModuleDefId),
    Values(ModuleDefId),
    Macros(MacroId),
}

pub(crate) fn namespace_of(item: ItemInNs) -> Namespace {
    match item {
        ItemInNs::Types(_) => Namespace::Types,
        ItemInNs::Values(_) => Namespace::Values,
        ItemInNs::Macros(_) => Namespace::Macros,
    }
}

impl ItemInNs {
    pub fn as_module_def_id(self) -> Option<ModuleDefId> {
        match self {
            ItemInNs::Types(id) | ItemInNs::Values(id) => Some(id),
            ItemInNs::Macros(_) => None,
        }
    }

    /// Returns the crate defining this item (or `None` if `self` is built-in).
    pub fn krate(&self, db: &Db) -> Option<Crate> {
        self.module(db).map(|module_id| module_id.krate(db))
    }

    pub fn module(&self, db: &Db) -> Option<ModuleId> {
        match self {
            ItemInNs::Types(id) | ItemInNs::Values(id) => id.module(db),
            ItemInNs::Macros(id) => Some(id.module(db)),
        }
    }
}

/// A module's names and declared items, read from its records once ([`item_scope`]).
#[derive(Debug, PartialEq, Eq)]
pub struct ItemScope {
    module: ModuleId,
    /// Every name in scope with what it binds, in the record's order of names.
    entries: Vec<(Name, PerNs), Global>,
    /// The items declared here that a name binds, in the order they are written.
    declarations: Vec<ModuleDefId, Global>,
    impls: Vec<ImplId, Global>,
    use_decls: Vec<UseId, Global>,
    extern_crate_decls: Vec<ExternCrateId, Global>,
    unnamed_consts: Vec<ConstId, Global>,
}

/// The [`ItemScope`] of `module`: its `ModuleScope` record and the members of its key-index
/// page, read once.
#[blaze_memo::tracked(returns(ref), owns, mem_tag = Nameres)]
pub fn item_scope(db: &Db, module: ModuleId) -> ItemScope {
    let mut entries: Vec<(Name, PerNs), Global> = Vec::new_in(Global);
    ModuleScope::of(module).for_each_per_ns(db, |name, def| entries.push((name, def)));
    // The record orders by namespace first, so one name's namespaces may be apart.
    entries.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    let mut merged: Vec<(Name, PerNs), Global> = Vec::with_capacity_in(entries.len(), Global);
    for (name, def) in entries {
        match merged.last_mut() {
            Some((last, at)) if *last == name => *at = at.or(def),
            _ => merged.push((name, def)),
        }
    }
    let mut scope = ItemScope {
        module,
        entries: merged,
        declarations: Vec::new_in(Global),
        impls: Vec::new_in(Global),
        use_decls: Vec::new_in(Global),
        extern_crate_decls: Vec::new_in(Global),
        unnamed_consts: Vec::new_in(Global),
    };
    let items = db.items();
    for member in key::members(db, scope_key(db, module)) {
        match items.kind(member).or_need() {
            ItemKind::Impl => scope.impls.push(ImplId::of_key(db, member)),
            ItemKind::Use => scope.use_decls.push(UseId::of_key(db, member)),
            ItemKind::ExternCrate => scope.extern_crate_decls.push(ExternCrateId::of_key(db, member)),
            ItemKind::Const if key::item_name(db, member).as_str() == "_" => {
                scope.unnamed_consts.push(ConstId::of_key(db, member))
            }
            ItemKind::Variant | ItemKind::Field => {
                panic!("item {member:x}: a variant or a field whose parent is a module")
            }
            _ => scope.declarations.push(key::module_def_of_key(db, member)),
        }
    }
    scope
}

impl ItemScope {
    fn stop<T>(&self, db: &Db, method: &'static str) -> T {
        let item = scope_key(db, self.module);
        Err(SurfaceError::NotBuilt(NotBuilt { method, item })).or_need()
    }

    pub fn entries(&self) -> impl Iterator<Item = (&Name, PerNs)> + '_ {
        self.entries.iter().map(|(name, def)| (name, *def))
    }

    pub fn values(&self) -> impl Iterator<Item = (&Name, Item<ModuleDefId, ImportOrGlob>)> + '_ {
        self.entries.iter().filter_map(|(name, def)| Some((name, def.values?)))
    }

    pub fn types(&self) -> impl Iterator<Item = (&Name, Item<ModuleDefId, ImportOrExternCrate>)> + '_ {
        self.entries.iter().filter_map(|(name, def)| Some((name, def.types?)))
    }

    pub fn macros(&self) -> impl Iterator<Item = (&Name, Item<MacroId, ImportOrExternCrate>)> + '_ {
        self.entries.iter().filter_map(|(name, def)| Some((name, def.macros?)))
    }

    /// What `name` binds in each namespace.
    pub fn get(&self, name: &Name) -> PerNs {
        match self.entries.binary_search_by(|(at, _)| at.as_str().cmp(name.as_str())) {
            Ok(at) => self.entries[at].1,
            Err(_) => PerNs::none(),
        }
    }

    pub fn declarations(&self) -> impl Iterator<Item = ModuleDefId> + '_ {
        self.declarations.iter().copied()
    }

    pub fn impls(&self) -> impl ExactSizeIterator<Item = ImplId> + '_ {
        self.impls.iter().copied()
    }

    /// The impls of no trait, by each one's lowered header.
    pub fn inherent_impls<'a>(&'a self, db: &'a Db) -> impl Iterator<Item = ImplId> + 'a {
        self.impls.iter().copied().filter(move |&it| ImplSignature::of(db, it).target_trait.is_none())
    }

    pub fn use_decls(&self) -> impl ExactSizeIterator<Item = UseId> + '_ {
        self.use_decls.iter().copied()
    }

    pub fn extern_crate_decls(&self) -> impl ExactSizeIterator<Item = ExternCrateId> + '_ {
        self.extern_crate_decls.iter().copied()
    }

    pub fn unnamed_consts(&self) -> impl Iterator<Item = ConstId> + '_ {
        self.unnamed_consts.iter().copied()
    }

    /// The traits in scope by a name of the type namespace.
    pub(crate) fn traits(&self) -> impl Iterator<Item = TraitId> + '_ {
        self.types().filter_map(|(_, item)| match item.def {
            ModuleDefId::TraitId(it) => Some(it),
            _ => None,
        })
    }

    /// The name `tr` is in scope by, with the import that brought it (never known here).
    pub fn trait_binding(&self, tr: TraitId) -> Option<(Name, Option<ImportOrExternCrate>)> {
        self.types().find_map(|(name, item)| match item.def {
            ModuleDefId::TraitId(it) if it == tr => Some((*name, None)),
            _ => None,
        })
    }

    /// The extern blocks declared here. The records list an extern block's items under its
    /// module and no block of its own (reconnect.md, "What the new-layer crates lack").
    pub fn extern_blocks(&self, db: &Db) -> std::iter::Empty<ExternBlockId> {
        self.stop(db, "ItemScope::extern_blocks")
    }

    /// The macro calls of the module: in `Expansion` records, not built.
    pub fn all_macro_calls(&self, db: &Db) -> std::iter::Empty<MacroCallId> {
        self.stop(db, "ItemScope::all_macro_calls")
    }

    /// The macro calls of the module by their ast ids: in `Expansion` records, not built.
    pub fn iter_macro_invoc(&self, db: &Db) -> std::iter::Empty<(&AstId<ast::MacroCall>, &MacroCallId)> {
        self.stop(db, "ItemScope::iter_macro_invoc")
    }

    /// The impls builtin derives made: in `Expansion` records, not built.
    pub fn builtin_derive_impls(&self, db: &Db) -> std::iter::Empty<BuiltinDeriveImplId> {
        self.stop(db, "ItemScope::builtin_derive_impls")
    }

    /// The `macro_rules!` of the module's textual scope (`TextualEntry`), not read here yet.
    pub fn legacy_macros(&self, db: &Db) -> std::iter::Empty<(&Name, &[MacroId])> {
        self.stop(db, "ItemScope::legacy_macros")
    }
}

/// The resolution of a definition with its visibility, as a scope binds it.
impl PerNs {
    pub(crate) fn from_def(
        def: ModuleDefId,
        vis: Visibility,
        value_ns_ctor_vis: Option<Visibility>,
        import: Option<ImportOrExternCrate>,
    ) -> PerNs {
        match def {
            ModuleDefId::ModuleId(_)
            | ModuleDefId::TraitId(_)
            | ModuleDefId::TypeAliasId(_)
            | ModuleDefId::BuiltinType(_)
            | ModuleDefId::AdtId(AdtId::UnionId(_) | AdtId::EnumId(_)) => PerNs::types(def, vis, import),
            ModuleDefId::FunctionId(_) | ModuleDefId::ConstId(_) | ModuleDefId::StaticId(_) => {
                PerNs::values(def, vis, import.and_then(ImportOrExternCrate::import_or_glob))
            }
            ModuleDefId::AdtId(AdtId::StructId(_)) | ModuleDefId::EnumVariantId(_) => {
                match value_ns_ctor_vis {
                    Some(ctor) => PerNs {
                        types: Some(Item { def, vis, import }),
                        values: Some(Item {
                            def,
                            vis: ctor,
                            import: import.and_then(ImportOrExternCrate::import_or_glob),
                        }),
                        macros: None,
                    },
                    None => PerNs::types(def, vis, import),
                }
            }
            ModuleDefId::MacroId(mac) => PerNs::macros(mac, vis, import),
        }
    }
}
