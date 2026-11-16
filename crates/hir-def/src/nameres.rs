//! The def map's accessors and path resolution, narrowed to record reads
//! (docs/v2/plan/design/reconnect.md, "Narrow def-map accessors" and "The resolver").
//!
//! No whole-crate map is built. [`crate_def_map`] gives a small handle per crate with the old
//! accessor names, each a read of one record: the crate's root module (`CrateRoot`), one
//! module's names (`ModuleScope`, read per name), its entry (`KeyIndex`). A path is resolved
//! segment by segment ([`DefMap::resolve_path`]), each module step one probe of that module's
//! `ModuleScope` record. What only a walk of every module answers ([`DefMap::modules`]) is not
//! built; the uses that need it are listed in the design.
//!
//! Every read of a record the task lacks stops it with a [`base_db::Need`] naming its key.

pub mod assoc;
pub mod def_site;
pub mod diagnostics;

pub use def_site::{
    assoc_item_is, assoc_item_key, impl_item_name, param_name, path_param_name, trait_item_name,
    written_name,
};

use std::alloc::Global;

use base_db::{Crate, Db, ItemKey, OrNeed, SurfaceError};
use blaze_surface::{Binding, Items, Namespace as SurfaceNs, NotBuilt, Target};
use hir_expand::{
    mod_path::{ModPath, PathKind},
    name::Name,
};
use span::Edition;

use crate::{
    AdtId, BlockId, FunctionId, Lookup, MacroExpander, MacroId, ModuleDefId, ModuleId,
    ProcMacroId, UseId,
    builtin_type::BuiltinType,
    item_scope::{BuiltinShadowMode, ItemScope, item_scope},
    key::{self, FromKey},
    per_ns::{Namespace, PerNs},
    visibility::{Visibility, VisibilityExplicitness},
};

/// What a path resolution says beside its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResolvePathResultPrefixInfo {
    /// The path went through a module of another crate.
    pub(crate) differing_crate: bool,
    /// Path of the form `Enum::Variant` (and not `Variant` alone).
    pub enum_variant: bool,
}

/// The sub-namespace of macros a macro path is resolved in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MacroSubNs {
    /// Function-like macros, suffixed with `!`.
    Bang,
    /// Macros inside attributes, i.e. attribute macros and derive macros.
    Attr,
}

/// Whether `mac` is of the sub-namespace `expected` asks (any, for `None`).
pub(crate) fn sub_namespace_match(db: &Db, mac: MacroId, expected: Option<MacroSubNs>) -> bool {
    let Some(expected) = expected else { return true };
    let bang = match mac {
        MacroId::ProcMacroId(it) => {
            matches!(it.lookup(db).kind, hir_expand::proc_macro::ProcMacroKind::Bang)
        }
        MacroId::Macro2Id(it) => is_bang(it.lookup(db).expander),
        MacroId::MacroRulesId(it) => is_bang(it.lookup(db).expander),
    };
    bang == (expected == MacroSubNs::Bang)
}

fn is_bang(expander: MacroExpander) -> bool {
    !matches!(expander, MacroExpander::BuiltInAttr(_) | MacroExpander::BuiltInDerive(_))
}

/// A crate's (or a block's) names, as a handle: what it reads is per question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DefMap {
    krate: Crate,
    block: Option<BlockId>,
    root: ModuleId,
}

/// The crate's handle. Its one read is the crate's `CrateRoot` record, for the root module.
#[blaze_memo::tracked(returns(ref), mem_tag = Nameres)]
pub fn crate_def_map(db: &Db, krate: Crate) -> DefMap {
    DefMap { krate, block: None, root: key::crate_root_module(db, krate) }
}

/// A block's handle: its scope is the block's own items, then its module's.
#[blaze_memo::tracked(returns(ref), mem_tag = Nameres)]
pub fn block_def_map(db: &Db, block: BlockId) -> DefMap {
    let module = block.module(db).to_static();
    DefMap { krate: module.krate(db), block: Some(block), root: module }
}

/// One module, as the old map's entry for it was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleData {
    pub id: ModuleId,
}

impl ModuleData {
    /// The module's names and items, read from its records once ([`item_scope`]).
    pub fn scope(self, db: &Db) -> &ItemScope {
        item_scope(db, self.id)
    }

    /// The module's names, read one probe at a time (no listing materialized).
    pub fn names(self) -> ModuleScope {
        ModuleScope { module: self.id }
    }

    /// The traits the module has in scope.
    pub fn traits_in_scope(self, db: &Db) -> impl Iterator<Item = crate::TraitId> + '_ {
        item_scope(db, self.id).traits()
    }

    /// The module that holds this one, `None` for a root.
    pub fn parent(self, db: &Db) -> Option<ModuleId> {
        self.id.containing_module_inside_def_map(db).map(crate::ModuleIdLt::to_static)
    }
}

/// The key of the module whose scope record a question reads. A block's module is no item:
/// its items are in no record yet (reconnect.md, "What the new-layer crates lack": the scope
/// of a block), so a question of it stops naming the module the block is in.
pub(crate) fn scope_key(db: &Db, module: ModuleId) -> ItemKey {
    if let Some(key) = module.key(db) {
        return key;
    }
    // A block is in an item's body, and every item is in a module with a key.
    let mut around = module.to_db(db);
    let item = loop {
        around = match around.containing_module(db) {
            Some(it) => it,
            None => panic!("module {module:?}: a block module with no keyed module around it"),
        };
        if let Some(key) = around.key(db) {
            break key;
        }
    };
    Err(SurfaceError::NotBuilt(NotBuilt { method: "the ModuleScope of a block", item })).or_need()
}

/// The text a name is looked up by in a record. A name a `macro` keyed by its def-site
/// context is looked up by its key too, which records do not hold yet: a typed stop.
fn record_name<'a>(db: &Db, module: ModuleId, name: &'a Name) -> &'a str {
    if name.key().is_some() {
        let item = module.key(db).unwrap_or(ItemKey(blaze_id::ContentId([0; 32])));
        return Err(SurfaceError::NotBuilt(NotBuilt { method: "a scope lookup of a keyed name", item }))
            .or_need();
    }
    name.as_str()
}

fn surface_ns(ns: Namespace) -> SurfaceNs {
    match ns {
        Namespace::Types => SurfaceNs::Types,
        Namespace::Values => SurfaceNs::Values,
        Namespace::Macros => SurfaceNs::Macros,
    }
}

/// A binding's visibility as the type checker names it, seen from the scope of `module`.
fn visibility_of(db: &Db, module: ModuleId, vis: blaze_surface::Visibility) -> Visibility {
    match vis {
        blaze_surface::Visibility::Pub => Visibility::Public,
        blaze_surface::Visibility::Crate => Visibility::PubCrate(module.krate(db)),
        blaze_surface::Visibility::Restricted(at) => {
            Visibility::Module(key::module_of_key(db, at), VisibilityExplicitness::Explicit)
        }
        blaze_surface::Visibility::Private => {
            Visibility::Module(module, VisibilityExplicitness::Implicit)
        }
    }
}

/// What a binding's target names: the item, `None` for an ambiguity (two candidates of one
/// layer, an error the scope's own check reports, as rustc's E0659). An undetermined target in
/// a finished record is a scope solved short of its fixpoint: a typed stop naming the module.
fn binding_target(db: &Db, module: ModuleId, binding: Binding<'_>) -> Option<ItemKey> {
    match binding.target {
        Target::Item(key) => Some(key),
        Target::Ambiguous(..) => None,
        Target::Undetermined => {
            let item = scope_key(db, module);
            Err(SurfaceError::NotBuilt(NotBuilt { method: "an undetermined binding", item }))
                .or_need()
        }
    }
}

/// A module's names over its `ModuleScope` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleScope {
    module: ModuleId,
}

impl ModuleScope {
    pub fn of(module: ModuleId) -> ModuleScope {
        ModuleScope { module }
    }

    pub fn module(self) -> ModuleId {
        self.module
    }

    /// What `name` is bound to in `ns`.
    pub fn get(self, db: &Db, ns: SurfaceNs, name: &Name) -> Option<Target<ItemKey>> {
        let view = db.items().scope(scope_key(db, self.module)).or_need();
        view.get(ns, record_name(db, self.module, name)).map(|binding| binding.target)
    }

    /// `name` in every namespace of the scope, as the old item scope's `get` gave it.
    pub fn get_per_ns(self, db: &Db, name: &Name) -> PerNs {
        let view = db.items().scope(scope_key(db, self.module)).or_need();
        let text = record_name(db, self.module, name);
        let mut out = PerNs::none();
        for ns in [Namespace::Types, Namespace::Values, Namespace::Macros] {
            let Some(binding) = view.get(surface_ns(ns), text) else { continue };
            let Some(target) = binding_target(db, self.module, binding) else { continue };
            let vis = visibility_of(db, self.module, binding.vis);
            out = out.or(per_ns_of(db, ns, target, vis));
        }
        out
    }

    /// Every binding of the module, for a caller that needs them all (a glob, a listing).
    pub fn for_each(self, db: &Db, mut each: impl FnMut(Binding<'_>)) {
        let view = db.items().scope(scope_key(db, self.module)).or_need();
        view.bindings().for_each(&mut each);
    }

    /// Every name of the scope with what it binds in each namespace, in the record's order
    /// (namespace, then name). An ambiguous name binds nothing.
    pub fn for_each_per_ns(self, db: &Db, mut each: impl FnMut(Name, PerNs)) {
        let view = db.items().scope(scope_key(db, self.module)).or_need();
        for binding in view.bindings() {
            let Some(target) = binding_target(db, self.module, binding) else { continue };
            let vis = visibility_of(db, self.module, binding.vis);
            let ns = match binding.ns {
                SurfaceNs::Types => Namespace::Types,
                SurfaceNs::Values => Namespace::Values,
                SurfaceNs::Macros => Namespace::Macros,
            };
            let name = Name::new_symbol_root(intern::Symbol::intern(binding.name));
            each(name, per_ns_of(db, ns, target, vis));
        }
    }

    /// The traits the module has in scope, by name or by `use Trait as _`: the traits of its
    /// type namespace. `as _` imports bind no name, and the record lists no such binding yet
    /// (reconnect.md, "What the new-layer crates lack").
    pub fn for_each_trait(self, db: &Db, mut each: impl FnMut(crate::TraitId)) {
        let items = db.items();
        let view = items.scope(scope_key(db, self.module)).or_need();
        for binding in view.bindings() {
            if binding.ns != SurfaceNs::Types {
                continue;
            }
            let Some(target) = binding_target(db, self.module, binding) else { continue };
            if items.kind(target).or_need() == blaze_surface::ItemKind::Trait {
                each(crate::TraitId::of_key(db, target));
            }
        }
    }
}

/// The resolution of `key` in `ns`.
fn per_ns_of(db: &Db, ns: Namespace, key: ItemKey, vis: Visibility) -> PerNs {
    let def = key::module_def_of_key(db, key);
    match (ns, def) {
        (Namespace::Macros, ModuleDefId::MacroId(mac)) => PerNs::macros(mac, vis, None),
        (Namespace::Macros, other) => {
            panic!("item {key:x}: bound in the macro namespace, but it is {other:?}")
        }
        (Namespace::Types, def) => PerNs::types(def, vis, None),
        (Namespace::Values, def) => PerNs::values(def, vis, None),
    }
}

impl DefMap {
    pub fn krate(&self) -> Crate {
        self.krate
    }

    pub fn block_id(&self) -> Option<BlockId> {
        self.block
    }

    /// The crate's root module (a block's: its module).
    pub fn root_module_id(&self) -> ModuleId {
        self.root
    }

    pub fn crate_root(&self, db: &Db) -> ModuleId {
        match self.block {
            None => self.root,
            Some(_) => crate_def_map(db, self.krate).root,
        }
    }

    pub fn module(&self, _db: &Db, id: ModuleId) -> ModuleData {
        ModuleData { id }
    }

    /// Every module of the crate. Not built: the uses that need it (design, "Narrow def-map
    /// accessors") move to per-module or per-trait records first.
    pub fn modules(&self, db: &Db) -> std::iter::Empty<(ModuleId, ModuleData)> {
        let item = scope_key(db, crate_def_map(db, self.krate).root);
        Err::<(), _>(SurfaceError::NotBuilt(NotBuilt { method: "DefMap::modules", item })).or_need();
        std::iter::empty()
    }

    /// The prelude module the crate's modules start from, from its crate root record, and the
    /// `#[prelude_import]` naming it (the record names the module, not the `use`: `None`).
    /// `None` under `#![no_core]` or `#![no_implicit_prelude]`.
    pub fn prelude(&self, db: &Db) -> Option<(ModuleId, Option<UseId>)> {
        crate_prelude(db, self.krate).map(|module| (module, None))
    }

    /// The crate's `#![recursion_limit]`. Not built: the `CrateRoot` record has no field for
    /// it yet (design, "What the new-layer crates lack").
    pub fn recursion_limit(&self, db: &Db) -> u32 {
        let item = scope_key(db, crate_def_map(db, self.krate).root);
        Err(SurfaceError::NotBuilt(NotBuilt { method: "DefMap::recursion_limit", item })).or_need()
    }

    /// A builtin integer of any width (`u7`), under `#![blaze::arbitrary_ints]`. Not built:
    /// the crate root record has no field for the extension's attribute yet.
    pub(crate) fn arbitrary_int(&self, db: &Db, name: &Name) -> PerNs {
        if BuiltinType::arbitrary_by_name(name).is_none() {
            return PerNs::none();
        }
        let item = scope_key(db, crate_def_map(db, self.krate).root);
        Err(SurfaceError::NotBuilt(NotBuilt { method: "DefMap::arbitrary_int", item })).or_need()
    }

    /// The function a proc macro is: the same item, so the same key.
    pub fn proc_macro_as_fn(&self, db: &Db, id: ProcMacroId) -> Option<FunctionId> {
        Some(FunctionId::of_key(db, id.lookup(db).key))
    }

    /// `path` resolved from `module` as rustc resolves a path in an item scope: its first
    /// segment by the module's scope, the extern prelude, the standard prelude and the builtin
    /// types; each later one by one probe of the module the previous one named. Gives what it
    /// names, the index of the first segment it did not resolve (the rest are associated
    /// items, for the type checker), and what the walk saw.
    pub fn resolve_path(
        &self,
        db: &Db,
        module: ModuleId,
        path: &ModPath,
        shadow: BuiltinShadowMode,
        expected_macro: Option<MacroSubNs>,
    ) -> (PerNs, Option<usize>, ResolvePathResultPrefixInfo) {
        resolve_path(db, module, path, shadow, expected_macro)
    }
}

/// The crate's prelude module.
pub(crate) fn crate_prelude(db: &Db, krate: Crate) -> Option<ModuleId> {
    let root = db.items().crate_root(krate.key(db)).or_need();
    root.head().prelude.map(|module| key::module_of_key(db, module))
}

/// The root module of the crate `name` names in `krate`'s extern prelude.
pub(crate) fn extern_prelude_root(db: &Db, krate: Crate, name: &str) -> Option<ModuleId> {
    let key = krate.key(db);
    let dep = db.items().crate_graph(key).or_need().dep(key, name)?;
    let Some(dep) = db.crate_of_key(dep) else {
        return Err(SurfaceError::NoCrateRecord { fact: "the task's crate table", krate: dep })
            .or_need();
    };
    Some(key::crate_root_module(db, dep))
}

/// Each crate of `krate`'s extern prelude, by its name there, with its root module.
pub(crate) fn for_each_extern_prelude(db: &Db, krate: Crate, mut each: impl FnMut(Name, ModuleId)) {
    let key = krate.key(db);
    let graph = db.items().crate_graph(key).or_need();
    for (name, dep) in graph.deps_of(key) {
        let Some(dep) = db.crate_of_key(dep) else {
            return Err(SurfaceError::NoCrateRecord { fact: "the task's crate table", krate: dep })
                .or_need();
        };
        each(Name::new_symbol_root(intern::Symbol::intern(name)), key::crate_root_module(db, dep));
    }
}

/// The extern prelude of `krate`, in the order its crates are listed: each name with the root
/// of the crate it names. Which `extern crate` added a name is not in the crate-graph record,
/// so the second of each pair is `None` (reconnect.md, "What the new-layer crates lack").
#[blaze_memo::tracked(returns(ref), owns, mem_tag = Nameres)]
pub fn crate_extern_prelude(
    db: &Db,
    krate: Crate,
) -> Vec<(Name, (ModuleId, Option<crate::ExternCrateId>)), Global> {
    let mut out = Vec::new_in(Global);
    for_each_extern_prelude(db, krate, |name, root| out.push((name, (root, None))));
    out
}

fn module_per_ns(module: ModuleId) -> PerNs {
    PerNs::types(ModuleDefId::ModuleId(module), Visibility::Public, None)
}

/// The builtin type `name` names, in the type namespace.
fn builtin(name: &Name) -> PerNs {
    match BuiltinType::by_name(name) {
        Some(ty) => PerNs::types(ModuleDefId::BuiltinType(ty), Visibility::Public, None),
        None => PerNs::none(),
    }
}

/// `name` as the first segment of a plain path written in `module`: rustc's lexical lookup of
/// an item scope, then the extern prelude, the standard prelude and the builtin types. With
/// `BuiltinShadowMode::Other` a builtin type is preferred over a module of its name (`str` the
/// type, not `core::str`); with `Module`, a module is.
fn resolve_first(db: &Db, module: ModuleId, name: &Name, shadow: BuiltinShadowMode) -> PerNs {
    let mut found = ModuleScope { module }.get_per_ns(db, name);
    let builtin = builtin(name);
    if let (Some(types), Some(_)) = (found.types, builtin.types)
        && matches!(types.def, ModuleDefId::ModuleId(_))
        && shadow == BuiltinShadowMode::Other
    {
        found.types = builtin.types;
    }
    if found.is_full() {
        return found;
    }
    let krate = module.krate(db);
    if found.types.is_none()
        && let Some(root) = extern_prelude_root(db, krate, record_name(db, module, name))
    {
        found.types = module_per_ns(root).types;
    }
    if !found.is_full()
        && let Some(prelude) = crate_prelude(db, krate)
    {
        found = found.or(ModuleScope { module: prelude }.get_per_ns(db, name));
    }
    found.or(builtin)
}

/// The module `count` levels above `module` (`self` for 0), leaving blocks as rustc's
/// `nearest_parent_mod` does. `None` above the crate root.
fn ancestor(db: &Db, module: ModuleId, count: u8) -> Option<ModuleId> {
    let mut at = module;
    while at.block(db).is_some() && at.containing_module_inside_def_map(db).is_none() {
        at = at.containing_module(db)?.to_static();
    }
    for _ in 0..count {
        at = at.containing_module(db)?.to_static();
        while at.block(db).is_some() && at.containing_module_inside_def_map(db).is_none() {
            at = at.containing_module(db)?.to_static();
        }
    }
    Some(at)
}

pub(crate) fn resolve_path(
    db: &Db,
    module: ModuleId,
    path: &ModPath,
    shadow: BuiltinShadowMode,
    expected_macro: Option<MacroSubNs>,
) -> (PerNs, Option<usize>, ResolvePathResultPrefixInfo) {
    let segments = path.segments();
    let krate = module.krate(db);
    let mut info = ResolvePathResultPrefixInfo::default();
    let unresolved = |at: usize| (PerNs::none(), Some(at), ResolvePathResultPrefixInfo::default());
    let (mut curr, start) = match path.kind {
        PathKind::DollarCrate(of) => {
            info.differing_crate = of != krate;
            (module_per_ns(key::crate_root_module(db, of)), 0)
        }
        PathKind::Crate => (module_per_ns(crate_def_map(db, krate).crate_root(db)), 0),
        PathKind::Super(count) => match ancestor(db, module, count) {
            Some(at) => (module_per_ns(at), 0),
            None => return unresolved(0),
        },
        PathKind::Abs if krate.data(db).edition == Edition::Edition2015 => {
            (module_per_ns(crate_def_map(db, krate).crate_root(db)), 0)
        }
        PathKind::Abs => {
            let Some(first) = segments.first() else { return unresolved(0) };
            match extern_prelude_root(db, krate, record_name(db, module, first)) {
                Some(root) => {
                    info.differing_crate = true;
                    (module_per_ns(root), 1)
                }
                None => return unresolved(0),
            }
        }
        PathKind::Plain => {
            let Some(first) = segments.first() else { return unresolved(0) };
            (resolve_first(db, module, first, shadow), 1)
        }
    };
    for (at, segment) in segments.iter().enumerate().skip(start) {
        let Some(types) = curr.types else { return unresolved(at) };
        curr = match types.def {
            ModuleDefId::ModuleId(next) => {
                if next.krate(db) != krate {
                    info.differing_crate = true;
                }
                ModuleScope { module: next }.get_per_ns(db, segment)
            }
            ModuleDefId::AdtId(AdtId::EnumId(enum_)) => {
                let Some(variant) = variant_named(db, enum_, segment) else { return unresolved(at) };
                info.enum_variant = true;
                let def = ModuleDefId::EnumVariantId(variant);
                PerNs::both(def, def, Visibility::Public, None)
            }
            // A type or a trait: the rest are associated items, the type checker's to resolve.
            _ => return (curr, Some(at), info),
        };
    }
    if let Some(macros) = curr.macros
        && !sub_namespace_match(db, macros.def, expected_macro)
    {
        curr.macros = None;
    }
    (curr, None, info)
}

/// The variant of `enum_` named `name`, by its members' names in their file rows.
fn variant_named(db: &Db, enum_: crate::EnumId, name: &Name) -> Option<crate::EnumVariantId> {
    let text = name.as_str();
    key::members(db, enum_.key(db))
        .into_iter()
        .find(|&member| key::item_name(db, member).as_str() == text)
        .map(|member| crate::EnumVariantId::of_key(db, member))
}
