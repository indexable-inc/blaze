//! What name resolution resolved through: the imports, `extern crate`s and `#[macro_use]`s each
//! path went through, marked as rustc's `record_use` marks them, and how each macro call of a
//! def map resolved.
//!
//! A lookup records nothing as it goes, so the query database sees no side effect. Once a def map's fixed
//! point is reached, its collection resolves each `use` and each macro call's path again, as
//! rustc's `finalize_imports` does, into the def map's [`NameRecords`]. [`used_imports`] adds the
//! paths of every signature and body of the crate, resolved again through the [`Resolver`]
//! lookups type inference makes, and follows glob imports on to the imports they read through,
//! as `find_transitive_imports` does.
//!
//! A path's uses are its segments' bindings: each prefix is resolved on its own (paths have one
//! to three segments, and this runs once per path, not per lookup), the last segment in the
//! namespace the path is resolved in. Records are in the order collection and the walk made
//! them, never sorted by id, so they are the same bytes in every run.

use std::alloc::Allocator;

use base_db::{Crate, Db};
use blaze_hash::{HashMap, HashSet};
use hir_expand::{
    HirFileId, InFile, MacroCallId, MacroCallKind, MacroDefId,
    attrs::AttrId,
    mod_path::{ModPath, PathKind},
    name::Name,
};
use span::Edition;
use syntax::{
    ast,
    view::{self, AstNode as _},
};

use crate::{
    AdtId, AssocItemId, AstId, ExpressionStoreOwnerId, ExternCrateId, FxIndexMap, GenericDefId,
    HasModule, Lookup, MacroId, ModuleDefId, ModuleId, TraitId, VariantId,
    expr_store::{
        ExpressionStore, StoreVisitor,
        scope::{ExprScopes, ScopeId},
    },
    hir::{
        Expr, ExprOrPatId, Pat, PatId,
        type_ref::{TypeRef, TypeRefId},
    },
    item_scope::{GlobId, ImportId, ImportOrExternCrate, ItemScope},
    nameres::{
        BlockInfo, BuiltinShadowMode, DefMap, LocalDefMap, MacroSubNs,
        assoc::{ImplItems, TraitItems},
        crate_def_map,
        demand::{crate_body_owners, walk_bodies},
        path_resolution::ResolveMode,
    },
    per_ns::{MacrosItem, Namespace, PerNs},
    resolver::{HasResolver, Resolver, resolver_for_scope},
};

/// How a name was found, ordered as rustc's `Used`: in the lexical scope (the first segment of
/// a plain path, the extern prelude, the `#[macro_use]` prelude), or relative to a module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Used {
    Scope,
    Other,
}

/// One binding of this crate a path resolved through: the import, glob import or `extern crate`
/// that bound `name` in `ns` where a segment found it. A macro found in the `#[macro_use]`
/// prelude is an `ExternCrate` binding in the macro namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Use {
    pub import: ImportOrExternCrate,
    pub name: Name,
    pub ns: Namespace,
    pub used: Used,
}

/// Where a macro call's name was found, in rustc's order of lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacroVia {
    /// The `macro_rules!` textual scope.
    Textual,
    /// A module scope: the binding of the path's last segment there, `None` for the macro's own
    /// declaration.
    Path(Option<ImportOrExternCrate>),
    /// The `#[macro_use]` prelude, from that `extern crate` (`None`: from the crate the std
    /// prelude injects).
    MacroUse(Option<ExternCrateId>),
    /// The std prelude.
    Prelude,
}

/// How one macro call of a def map resolved: a function-like call, an attribute or a derive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacroCallRecord {
    pub call: MacroCallId,
    pub def: MacroId,
    pub via: MacroVia,
}

/// A derive helper attribute that resolved: on `item`, attribute `attr`, the helper of `derive`.
/// `before_derive` when the attribute comes before the `#[derive]` that brings the helper in,
/// which rustc accepts with the `legacy_derive_helpers` future-incompatibility lint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeriveHelperUse {
    pub item: AstId<ast::Item>,
    pub attr: AttrId,
    pub derive: MacroId,
    pub before_derive: bool,
}

/// What a def map's collection recorded of how its names resolved.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct NameRecords {
    /// The bindings the paths of this def map's imports and macro calls went through.
    pub(crate) uses: Box<[Use]>,
    /// The imports that did not resolve, which rustc counts as used (`Used::Other`).
    pub(crate) unresolved: Box<[ImportOrExternCrate]>,
    /// Each glob import of a module of this crate and the module it reads.
    pub(crate) glob_targets: Box<[(GlobId, ModuleId)]>,
    pub(crate) macro_calls: Box<[MacroCallRecord]>,
    pub(crate) derive_helpers: Box<[DeriveHelperUse]>,
    /// The `extern crate`s of the crate root that name a crate the extern prelude has from the
    /// crate's dependencies anyway, under the same name: rustc's extern prelude entries whose
    /// item declaration is not "introduced by item", which a lexical use does not mark used.
    pub(crate) not_introducing: Box<[ExternCrateId]>,
}

impl NameRecords {
    pub fn uses(&self) -> &[Use] {
        &self.uses
    }

    pub fn macro_calls(&self) -> &[MacroCallRecord] {
        &self.macro_calls
    }

    /// How `call` resolved, when it is a call of this def map.
    pub fn macro_call(&self, call: MacroCallId) -> Option<&MacroCallRecord> {
        self.macro_calls.iter().find(|it| it.call == call)
    }

    pub fn derive_helpers(&self) -> &[DeriveHelperUse] {
        &self.derive_helpers
    }
}

fn import_krate(db: &Db, import: ImportOrExternCrate) -> Crate {
    match import {
        ImportOrExternCrate::Glob(GlobId { use_, .. })
        | ImportOrExternCrate::Import(ImportId { use_, .. }) => {
            use_.lookup(db).container.krate(db)
        }
        ImportOrExternCrate::ExternCrate(it) => it.lookup(db).container.krate(db),
    }
}

impl DefMap {
    /// Appends to `out` the bindings of this crate that `path` resolves through from `module`:
    /// each segment's binding before the last in the types namespace, the last one's in `ns`
    /// (every namespace it has, for `None`), as rustc's `finalize` of a path records them.
    /// `around` resolves as [`Self::resolve_path_fp_with_macro`] does, in the def maps around a
    /// block too; without it only in this one.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn path_uses(
        &self,
        q: (&LocalDefMap, &Db, ResolveMode),
        module: ModuleId,
        path: &ModPath,
        shadow: BuiltinShadowMode,
        subns: Option<MacroSubNs>,
        ns: Option<Namespace>,
        around: bool,
        out: &mut Vec<Use, impl Allocator>,
    ) {
        let (_, db, mode) = q;
        let segments = path.segments();
        let kind = path.kind;
        let full = self.resolve_segments(q, module, kind, segments, shadow, subns, around);
        let reached = full.segment_index.unwrap_or(segments.len());
        let mut push = |at: usize, import: Option<ImportOrExternCrate>, ns: Namespace| {
            let Some(import) = import else { return };
            if import_krate(db, import) != self.krate {
                return;
            }
            let name = segments[at];
            let used = self.used_at(db, path, mode, at, import);
            out.push(Use { import, name, ns, used });
        };
        for len in 1..reached {
            let prefix = &segments[..len];
            let module_shadow = BuiltinShadowMode::Module;
            let res = self.resolve_segments(q, module, kind, prefix, module_shadow, None, around);
            push(len - 1, res.resolved_def.types.and_then(|it| it.import), Namespace::Types);
        }
        let Some(last) = reached.checked_sub(1) else { return };
        let res = full.resolved_def;
        if full.segment_index.is_some() {
            push(last, res.types.and_then(|it| it.import), Namespace::Types);
            return;
        }
        let bindings = [
            (Namespace::Types, res.types.and_then(|it| it.import)),
            (Namespace::Values, res.values.and_then(|it| it.import).map(Into::into)),
            (Namespace::Macros, res.macros.and_then(|it| it.import)),
        ];
        for (at_ns, import) in bindings {
            if ns.is_none_or(|ns| ns == at_ns) {
                push(last, import, at_ns);
            }
        }
    }

    /// [`Self::path_uses`] of a path outside an import: in the types or values namespace as
    /// type inference resolves it, or the macro one as a macro call's path.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn path_uses_outside_imports(
        &self,
        local_def_map: &LocalDefMap,
        db: &Db,
        module: ModuleId,
        path: &ModPath,
        ns: Namespace,
        subns: Option<MacroSubNs>,
        around: bool,
        out: &mut Vec<Use, impl Allocator>,
    ) {
        let q = (local_def_map, db, ResolveMode::Other);
        let shadow = BuiltinShadowMode::Other;
        self.path_uses(q, module, path, shadow, subns, Some(ns), around, out);
    }

    /// How segment `at` of `path` was found through `import`: rustc's `Used::Scope` for the
    /// first segment of a plain or `::` path, which the lexical scope or the extern prelude
    /// has; `Used::Other` for a segment relative to a module, and for the first segment of a
    /// 2015 import or `::` path that the crate root has.
    fn used_at(
        &self,
        db: &Db,
        path: &ModPath,
        mode: ResolveMode,
        at: usize,
        import: ImportOrExternCrate,
    ) -> Used {
        let kind = path.kind;
        if at > 0 {
            return Used::Other;
        }
        match kind {
            PathKind::Plain | PathKind::Abs
                if self.data.edition == Edition::Edition2015
                    && (kind == PathKind::Abs || mode == ResolveMode::Import) =>
            {
                let name = &path.segments()[0];
                let in_root = match self.block {
                    None => self.module(db, self.root).name_in_scope(db, name),
                    Some(_) => {
                        let map = crate_def_map(db, self.krate);
                        map.module(db, map.root).name_in_scope(db, name)
                    }
                };
                match in_root.types.and_then(|it| it.import) == Some(import) {
                    true => Used::Other,
                    false => Used::Scope,
                }
            }
            PathKind::Plain | PathKind::Abs => Used::Scope,
            PathKind::Super(_) | PathKind::Crate | PathKind::DollarCrate(_) => Used::Other,
        }
    }

    /// The `macro_rules!` in the textual scope of `module` whose definition is `def`.
    fn textual_macro(
        &self,
        db: &Db,
        module: ModuleId,
        name: &Name,
        def: MacroDefId,
    ) -> Option<MacroId> {
        let is_def = |it: &&MacroId| it.definition(db) == def;
        if let Some(found) = (self.module(db, module).legacy_macro_in_scope(db, name))
            .and_then(|all| all.iter().find(is_def))
        {
            return Some(*found);
        }
        let mut inner = self.block;
        while let Some(BlockInfo { parent, .. }) = inner {
            let map = parent.def_map(db);
            let defined = map.module(db, parent).legacy_macro_in_scope(db, name);
            if let Some(found) = defined.and_then(|all| all.iter().find(is_def)) {
                return Some(*found);
            }
            inner = map.block;
        }
        None
    }

    /// Where the macro call of `module` that names `path` and calls `def` found it, with the
    /// macro: `None` where its path does not name `def` once the def map is collected.
    pub(super) fn macro_via(
        &self,
        local_def_map: &LocalDefMap,
        db: &Db,
        module: ModuleId,
        path: &ModPath,
        subns: MacroSubNs,
        def: MacroDefId,
    ) -> Option<(MacroId, MacroVia)> {
        let name = path.as_ident();
        if let Some(name) = name
            && let Some(found) = self.textual_macro(db, module, name, def)
        {
            return Some((found, MacroVia::Textual));
        }
        let res = self.resolve_path_fp_with_macro(
            local_def_map,
            db,
            ResolveMode::Other,
            module,
            path,
            BuiltinShadowMode::Module,
            Some(subns),
        );
        if res.segment_index.is_some() {
            return None;
        }
        let item = res.resolved_def.macros.filter(|it| it.def.definition(db) == def)?;
        let via = match (item.import, name) {
            (Some(ImportOrExternCrate::ExternCrate(it)), _) => MacroVia::MacroUse(Some(it)),
            (import, None) => MacroVia::Path(import),
            (import, Some(name)) => self.ident_macro_via(db, module, name, item, import),
        };
        Some((item.def, via))
    }

    /// Where a macro named by one identifier was found, when it is not textual: a module scope
    /// of `module` or around it, the `#[macro_use]` prelude, or the std prelude.
    fn ident_macro_via(
        &self,
        db: &Db,
        module: ModuleId,
        name: &Name,
        item: MacrosItem,
        import: Option<ImportOrExternCrate>,
    ) -> MacroVia {
        let binds = |scope: PerNs| scope.macros.is_some_and(|it| it.def == item.def);
        if binds(self.module(db, module).name_in_scope(db, name)) {
            return MacroVia::Path(import);
        }
        let (mut map, mut at) = (self.block, module == self.root);
        while let Some(BlockInfo { parent, .. }) = map
            && at
        {
            let parent_map = parent.def_map(db);
            if binds(parent_map.module(db, parent).name_in_scope(db, name)) {
                return MacroVia::Path(import);
            }
            (map, at) = (parent_map.block, parent == parent_map.root);
        }
        match self.macro_use_prelude.get(name) {
            Some(&(it, None)) if it == item.def => MacroVia::MacroUse(None),
            _ => MacroVia::Prelude,
        }
    }
}

/// Which imports and `extern crate`s of a crate its paths use, and how: rustc's
/// `import_use_map`, `used_imports`, `glob_map` and the uses of ambiguous glob bindings.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UsedImports {
    /// Each import, glob import and `extern crate` a path used, with the strongest use.
    imports: FxIndexMap<ImportOrExternCrate, Used>,
    /// Each `#[macro_use] extern crate` a macro was found through, in the `#[macro_use]`
    /// prelude.
    macro_use: FxIndexMap<ExternCrateId, Used>,
    /// The names each glob import was used for (`find_transitive_imports` adds those a use
    /// read on through a glob).
    globs: FxIndexMap<GlobId, Box<[Name]>>,
    /// The uses of a name two globs give different items: the glob of the binding used.
    ambiguous: Box<[(GlobId, Name, Namespace)]>,
}

impl UsedImports {
    /// How `import` was used, `None` where no path used it: rustc's unused import.
    pub fn used(&self, import: ImportOrExternCrate) -> Option<Used> {
        self.imports.get(&import).copied()
    }

    pub fn imports(&self) -> impl ExactSizeIterator<Item = (ImportOrExternCrate, Used)> + '_ {
        self.imports.iter().map(|(&k, &v)| (k, v))
    }

    /// How a macro was found through the `#[macro_use]` of `extern_crate`.
    pub fn macro_use(&self, extern_crate: ExternCrateId) -> Option<Used> {
        self.macro_use.get(&extern_crate).copied()
    }

    pub fn macro_uses(&self) -> impl ExactSizeIterator<Item = (ExternCrateId, Used)> + '_ {
        self.macro_use.iter().map(|(&k, &v)| (k, v))
    }

    /// The names glob import `glob` was used for.
    pub fn glob_names(&self, glob: GlobId) -> &[Name] {
        self.globs.get(&glob).map_or(&[], |it| &**it)
    }

    /// The uses of names two globs give different items, rustc's glob-vs-glob ambiguity
    /// errors: the glob whose binding the use read, the name and the namespace.
    pub fn ambiguous(&self) -> &[(GlobId, Name, Namespace)] {
        &self.ambiguous
    }
}

/// The uses of the imports of `krate` by every path of the crate: its imports, macro calls,
/// signatures and bodies, block def maps included. The traits method resolution picks are
/// not paths: [`used_imports_with`] adds them.
#[blaze_query::tracked(returns(ref))]
pub fn used_imports(db: &Db, krate: Crate) -> UsedImports {
    finish(db, krate, crate_uses(db, krate), &[])
}

/// [`used_imports`] with `more` uses besides the paths': type inference's, the bindings
/// [`Resolver::record_trait_uses`] gives for each trait a method call or an associated item
/// path resolved through (rustc's `used_trait_imports`). Reads the same walk of the crate, so
/// a caller that keeps the result in a query of its own walks nothing twice.
pub fn used_imports_with(db: &Db, krate: Crate, more: &[Use]) -> UsedImports {
    finish(db, krate, crate_uses(db, krate), more)
}

/// What the walk of a crate's paths found, before glob imports are followed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CrateUses {
    uses: Box<[Use]>,
    unresolved: Box<[ImportOrExternCrate]>,
    glob_targets: Box<[(GlobId, ModuleId)]>,
}

#[blaze_query::tracked(returns(ref))]
pub(crate) fn crate_uses(db: &Db, krate: Crate) -> CrateUses {
    let def_map = crate_def_map(db, krate);
    let mut walk = Walk::new(db);
    walk.def_map(def_map);
    walk_bodies(
        db,
        crate_body_owners(db, krate),
        |_| true,
        |owner, blocks| {
            walk.store(ExpressionStoreOwnerId::Body(owner));
            for &(_, block_map) in &blocks.blocks {
                walk.def_map(block_map);
            }
        },
    );
    CrateUses {
        uses: walk.uses.into_boxed_slice(),
        unresolved: walk.unresolved.into_boxed_slice(),
        glob_targets: walk.glob_targets.into_boxed_slice(),
    }
}

/// Appends to `out` the uses of `trait_` through `binding`, the import of this crate that has
/// it in scope where a method or associated item of it was found, and on through each glob it
/// came by to the binding of the trait in the glob's module: rustc's `find_transitive_imports`
/// of a `TraitCandidate`. A single import stops the chain; its own path's uses are the def
/// map's records already. `name` is the binding's name, `_` for an unnamed one.
pub(crate) fn trait_uses(
    db: &Db,
    krate: Crate,
    trait_: TraitId,
    binding: (Name, ImportOrExternCrate),
    out: &mut Vec<Use>,
) {
    let (mut name, mut import) = binding;
    let start = out.len();
    loop {
        // Two modules may glob-import each other: a chain ends where it meets itself.
        if import_krate(db, import) != krate || out[start..].iter().any(|it| it.import == import)
        {
            return;
        }
        out.push(Use { import, name, ns: Namespace::Types, used: Used::Other });
        let ImportOrExternCrate::Glob(glob) = import else { return };
        let module = glob.use_.lookup(db).container;
        let names = module.def_map(db).name_records(db);
        let Some(&(_, target)) = names.glob_targets.iter().find(|&&(it, _)| it == glob) else {
            return;
        };
        let scope = target.def_map(db).module(db, target).scope(db);
        let Some((next_name, Some(next))) = scope.trait_binding(trait_) else { return };
        (name, import) = (next_name, next);
    }
}

struct Walk<'db> {
    db: &'db Db,
    uses: Vec<Use>,
    unresolved: Vec<ImportOrExternCrate>,
    glob_targets: Vec<(GlobId, ModuleId)>,
}

impl<'db> Walk<'db> {
    fn new(db: &'db Db) -> Self {
        Walk {
            db,
            // alloc: the crate's uses, gathered once per `crate_uses`.
            uses: Vec::new(),
            // alloc: as above.
            unresolved: Vec::new(),
            // alloc: as above.
            glob_targets: Vec::new(),
        }
    }

    /// The records of `def_map`, and the signatures of what its modules declare.
    fn def_map(&mut self, def_map: &'db DefMap) {
        let db = self.db;
        let NameRecords { uses, unresolved, glob_targets, .. } = def_map.name_records(db);
        self.uses.extend_from_slice(uses);
        self.unresolved.extend_from_slice(unresolved);
        self.glob_targets.extend(glob_targets.iter().copied());
        for (_, module) in def_map.modules(db) {
            self.signatures(module.scope(db));
        }
    }

    fn signatures(&mut self, scope: &'db ItemScope) {
        let db = self.db;
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(it) => self.signature(it.into()),
                ModuleDefId::ConstId(it) => self.signature(it.into()),
                ModuleDefId::StaticId(it) => self.signature(it.into()),
                ModuleDefId::TypeAliasId(it) => self.signature(it.into()),
                ModuleDefId::AdtId(adt) => {
                    self.signature(adt.into());
                    match adt {
                        AdtId::StructId(it) => self.variant(it.into()),
                        AdtId::UnionId(it) => self.variant(it.into()),
                        AdtId::EnumId(it) => {
                            for &(variant, _) in it.enum_variants(db).variants.values() {
                                self.variant(variant.into());
                            }
                        }
                    }
                }
                ModuleDefId::TraitId(it) => {
                    self.signature(it.into());
                    let items = TraitItems::query(db, it);
                    self.assoc(&items.items, &items.macro_calls, it.resolver(db));
                }
                ModuleDefId::ModuleId(_)
                | ModuleDefId::EnumVariantId(_)
                | ModuleDefId::MacroId(_)
                | ModuleDefId::BuiltinType(_) => {}
            }
        }
        for it in scope.unnamed_consts() {
            self.signature(it.into());
        }
        for it in scope.impls() {
            self.signature(it.into());
            let items = &ImplItems::of(db, it).0;
            self.assoc(&items.items, &items.macro_calls, it.resolver(db));
        }
    }

    fn assoc(
        &mut self,
        items: &[(Name, AssocItemId)],
        macro_calls: &[(AstId<ast::Item>, MacroCallId)],
        resolver: Resolver<'db>,
    ) {
        for &(_, item) in items {
            match item {
                AssocItemId::FunctionId(it) => self.signature(it.into()),
                AssocItemId::ConstId(it) => self.signature(it.into()),
                AssocItemId::TypeAliasId(it) => self.signature(it.into()),
            }
        }
        for &(ast_id, call) in macro_calls {
            let view::Item::MacroCall(node) = ast_id.to_view(self.db) else { continue };
            self.macro_call(&resolver, ast_id.file_id, node, call);
        }
    }

    fn signature(&mut self, def: GenericDefId) {
        self.store(ExpressionStoreOwnerId::Signature(def));
    }

    fn variant(&mut self, def: VariantId) {
        self.store(ExpressionStoreOwnerId::VariantFields(def));
    }

    /// Every path of the store of `owner`, resolved where it is.
    fn store(&mut self, owner: ExpressionStoreOwnerId) {
        let db = self.db;
        let (store, source_map) = ExpressionStore::with_source_map(db, owner);
        let scopes = ExprScopes::of(db, owner);
        let within = Within::of(store, scopes);
        // alloc: one resolver per scope a path of the store is in, built once.
        let mut resolvers: HashMap<Option<ScopeId>, Resolver<'db>> = HashMap::default();
        let out = &mut self.uses;
        for (expr, it) in store.exprs() {
            let (path, ns) = match it {
                Expr::Path(path) => (path, Namespace::Values),
                Expr::RecordLit { path, .. } => (path, Namespace::Types),
                _ => continue,
            };
            let hygiene = store.expr_path_hygiene(expr);
            let resolver = resolver_at(&mut resolvers, db, owner, scopes.scope_for(expr));
            resolver.record_path_uses(db, path, ns, hygiene, out);
        }
        for (pat, it) in store.pats() {
            let (path, ns) = match it {
                Pat::Path(path) | Pat::TupleStruct { path, .. } => (path, Namespace::Values),
                Pat::Record { path, .. } => (path, Namespace::Types),
                _ => continue,
            };
            let hygiene = store.pat_path_hygiene(pat);
            let resolver = resolver_at(&mut resolvers, db, owner, within.pat(pat));
            resolver.record_path_uses(db, path, ns, hygiene, out);
        }
        for (ty, it) in store.type_refs() {
            let TypeRef::Path(path) = it else { continue };
            let hygiene = store.type_path_hygiene(ty);
            let resolver = resolver_at(&mut resolvers, db, owner, within.ty(ty));
            resolver.record_path_uses(db, path, Namespace::Types, hygiene, out);
        }
        for (ptr, call) in source_map.macro_calls() {
            let node = ptr.value.to_view(ptr.file_id.root(db));
            let scope = node
                .syntax()
                .parent()
                .and_then(view::MacroExpr::cast)
                .and_then(|it| source_map.macro_expansion_expr_view(InFile::new(ptr.file_id, it)))
                .and_then(|it| match it {
                    ExprOrPatId::ExprId(expr) => scopes.scope_for(expr),
                    ExprOrPatId::PatId(pat) => within.pat(pat),
                });
            let resolver = resolver_at(&mut resolvers, db, owner, scope);
            self.macro_call(resolver, ptr.file_id, node, call);
        }
    }

    /// The uses of the path of a function-like macro call a store or an impl has, where the
    /// resolver's item scopes name the macro the call expanded (not a `macro_rules!` of the
    /// body's textual scope).
    fn macro_call(
        &mut self,
        resolver: &Resolver<'db>,
        file: HirFileId,
        node: view::MacroCall<'_>,
        call: MacroCallId,
    ) {
        let db = self.db;
        let loc = call.loc(db);
        let MacroCallKind::FnLike { .. } = loc.kind else { return };
        let Some(ast_path) = node.path() else { return };
        let span_map = file.span_map(db);
        let Some(path) =
            ModPath::from_src_view(db, ast_path, &mut |range| span_map.span_for_range(range).ctx)
        else {
            return;
        };
        let subns = Some(MacroSubNs::Bang);
        if resolver.resolve_path_as_macro_def(db, &path, subns) != Some(loc.def) {
            return;
        }
        resolver.record_macro_path_uses(db, &path, subns, &mut self.uses);
    }
}

/// Marks each use in `walked` and `more`, in that order, following glob imports on to the
/// binding they read (`find_transitive_imports`), into the crate's [`UsedImports`].
fn finish(db: &Db, krate: Crate, walked: &CrateUses, more: &[Use]) -> UsedImports {
    let not_introducing = &*crate_def_map(db, krate).name_records(db).not_introducing;
    // alloc: once per `finish`, which runs once per crate.
    let glob_targets: HashMap<GlobId, ModuleId> = walked.glob_targets.iter().copied().collect();
    let mut imports: FxIndexMap<ImportOrExternCrate, Used> = FxIndexMap::default();
    let mut macro_use: FxIndexMap<ExternCrateId, Used> = FxIndexMap::default();
    let mut globs: FxIndexMap<GlobId, Vec<Name>> = FxIndexMap::default();
    // alloc: part of the result.
    let mut ambiguous = Vec::new();
    let mut seen: HashSet<Use> = HashSet::default();
    // alloc: the worklist, the walk's uses and the globs they read through.
    let mut queue: Vec<Use> = Vec::with_capacity(walked.uses.len() + more.len());
    queue.extend_from_slice(&walked.uses);
    queue.extend_from_slice(more);
    {
        let mut at = 0;
        while let Some(&it) = queue.get(at) {
            at += 1;
            if !seen.insert(it) {
                continue;
            }
            let Use { import, name, ns, used } = it;
            if let (Namespace::Macros, ImportOrExternCrate::ExternCrate(extern_crate)) = (ns, import)
            {
                let old = macro_use.entry(extern_crate).or_insert(used);
                *old = (*old).max(used);
                continue;
            }
            if let ImportOrExternCrate::ExternCrate(extern_crate) = import
                && used == Used::Scope
                && not_introducing.contains(&extern_crate)
            {
                continue;
            }
            let old = imports.entry(import).or_insert(used);
            *old = (*old).max(used);
            let ImportOrExternCrate::Glob(glob) = import else { continue };
            let names = globs.entry(glob).or_default();
            if !names.contains(&name) {
                names.push(name);
            }
            let module = glob.use_.lookup(db).container;
            let scope = module.def_map(db).module(db, module).scope(db);
            if scope.glob_ambiguity(&name, ns).is_some() {
                ambiguous.push((glob, name, ns));
            }
            // `find_transitive_imports`: the binding the glob read the name from.
            let Some(&target) = glob_targets.get(&glob) else { continue };
            let res = target.def_map(db).module(db, target).scope(db).get(&name);
            let source = match ns {
                Namespace::Types => res.types.and_then(|it| it.import),
                Namespace::Values => res.values.and_then(|it| it.import).map(Into::into),
                Namespace::Macros => res.macros.and_then(|it| it.import),
            };
            if let Some(source) = source
                && import_krate(db, source) == krate
            {
                queue.push(Use { import: source, name, ns, used: Used::Other });
            }
        }
    }
    for &import in &walked.unresolved {
        imports.insert(import, Used::Other);
    }
    UsedImports {
        imports,
        macro_use,
        // alloc: the result, built once.
        globs: globs.into_iter().map(|(k, v)| (k, v.into_boxed_slice())).collect(),
        ambiguous: ambiguous.into_boxed_slice(),
    }
}

fn resolver_at<'a, 'db>(
    resolvers: &'a mut HashMap<Option<ScopeId>, Resolver<'db>>,
    db: &'db Db,
    owner: ExpressionStoreOwnerId,
    scope: Option<ScopeId>,
) -> &'a Resolver<'db> {
    resolvers.entry(scope).or_insert_with(|| resolver_for_scope(db, owner, scope))
}

/// The scope each pattern and type of a store is in: that of the expression it is part of,
/// or the store's own where none is.
struct Within {
    pats: HashMap<PatId, Option<ScopeId>>,
    types: HashMap<TypeRefId, Option<ScopeId>>,
}

impl Within {
    fn of(store: &ExpressionStore, scopes: &ExprScopes) -> Within {
        let mut within = Within { pats: HashMap::default(), types: HashMap::default() };
        // alloc: the worklist, once per store.
        let mut todo = Vec::new();
        for expr in store.expr_ids() {
            let scope = scopes.scope_for(expr);
            store.visit_expr_children(expr, Assign { scope, within: &mut within, todo: &mut todo });
        }
        while let Some(next) = todo.pop() {
            match next {
                Child::Pat(pat) => {
                    let scope = within.pat(pat);
                    let assign = Assign { scope, within: &mut within, todo: &mut todo };
                    store.visit_pat_children(pat, assign);
                }
                Child::Type(ty) => {
                    let scope = within.ty(ty);
                    let assign = Assign { scope, within: &mut within, todo: &mut todo };
                    store.visit_type_ref_children(ty, assign);
                }
            }
        }
        within
    }

    fn pat(&self, pat: PatId) -> Option<ScopeId> {
        self.pats.get(&pat).copied().flatten()
    }

    fn ty(&self, ty: TypeRefId) -> Option<ScopeId> {
        self.types.get(&ty).copied().flatten()
    }
}

enum Child {
    Pat(PatId),
    Type(TypeRefId),
}

/// Gives the patterns and types an expression, pattern or type holds its scope, the first
/// time one is met.
struct Assign<'a> {
    scope: Option<ScopeId>,
    within: &'a mut Within,
    todo: &'a mut Vec<Child>,
}

impl StoreVisitor for Assign<'_> {
    fn on_pat(&mut self, pat: PatId) {
        if let std::collections::hash_map::Entry::Vacant(it) = self.within.pats.entry(pat) {
            it.insert(self.scope);
            self.todo.push(Child::Pat(pat));
        }
    }

    fn on_type(&mut self, ty: TypeRefId) {
        if let std::collections::hash_map::Entry::Vacant(it) = self.within.types.entry(ty) {
            it.insert(self.scope);
            self.todo.push(Child::Type(ty));
        }
    }
}