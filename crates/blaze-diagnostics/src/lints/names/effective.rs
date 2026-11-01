//! The crate's effective visibilities, rustc's `EffectiveVisibilities`: of each definition, the
//! visibility it has at each `Level`. `Direct`: through its own path. `Reexported`: through
//! `use`s too. `Reachable`: or named in the interface of what is. `ReachableThroughImplTrait`:
//! or in the hidden type of an `impl Trait`. Computed once for the crate, read by the dead-code
//! pass (`dead`), `check_private_in_public` and the late lint passes over items (`items`).
//!
//! Two passes, as rustc's. Resolve's walks the names of each module, enum and trait, and the
//! fields of each struct and variant, from the crate's root: a name's definition is `Direct` at
//! the least of its own visibility and its scope's, through a `use` `Reexported`, until nothing
//! rises. The embargo visitor's then raises the hidden types of the crate's `impl Trait`s at
//! `ReachableThroughImplTrait`, and at `Reachable` what the interface of each definition with a
//! visibility names (its parameters' defaults and const types, its clauses, its type or
//! signature, an impl's trait), the members of each, and each impl to the least of what its
//! self type and trait are, until nothing rises. Each rise of a type or trait walks again the
//! impls whose self type or trait it is.
//!
//! Every definition met is numbered once; its visibilities, its own and its private one are
//! columns over the numbering. Types are walked as `DefIdVisitorSkeleton` walks them, aliases
//! expanded: each ADT, foreign type and function item, each `dyn` type's trait, a projection's
//! trait, an opaque type's bounds' traits, and their arguments.
//!
//! Not followed: a `use` chain past its first `use` (an import of an import raises the first
//! `use` and the definition, not the `use`s between), what a `pub macro` makes reachable
//! (`update_reachability_from_macro`, `macro_reachable_adts`), the most visible of ambiguous
//! glob imports (`ambiguity_vis_max`), the hidden types of type alias `impl Trait`s, the
//! arguments of a trait's supertraits and an associated type's default.
//!
//! Follows `compiler/rustc_middle/src/middle/privacy.rs` (`EffectiveVisibility`,
//! `EffectiveVisibilities::update`), `compiler/rustc_resolve/src/effective_visibilities.rs` and
//! `compiler/rustc_privacy/src/lib.rs` (`EmbargoVisitor`, `ReachEverythingInTheInterface
//! Visitor`, `DefIdVisitorSkeleton`, `FindMin`, `effective_visibilities`) (MIT or Apache-2.0).

use std::cmp::Ordering;

use base_db::Db;
use blaze_hash::{HashMap, HashSet};
use hir::{
    AsAssocItem as _, AssocItem, AssocItemContainer, GenericDef, HasVisibility as _, ModuleDef,
    db::HirDatabase as _,
};
use hir_def::{
    CallableDefId, FunctionId, ItemContainerId, Lookup as _, ModuleDefId, TypeAliasId, UseId,
    expr_store::Body,
    item_scope::{ImportOrExternCrate, ImportOrGlob},
    nameres::demand::every_body,
};
use hir_ty::next_solver::{AliasTy, Clause, ClauseKind, DbInterner, Ty, TyKind};
use rustc_type_ir::{
    AliasTyKind, TypeSuperVisitable, TypeVisitable, TypeVisitableExt as _, TypeVisitor,
    inherent::IntoKind as _,
};

use crate::cx::Cx;

/// What has an effective visibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Def {
    Item(ModuleDef),
    Field(hir::Field),
    Impl(hir::Impl),
    Use(UseId),
}

/// rustc's `Level`, least direct first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Level {
    ReachableThroughImplTrait,
    Reachable,
    Reexported,
    Direct,
}

impl Level {
    /// `Level::all_levels`: most direct first.
    const ALL: [Level; 4] = [Level::Direct, Level::Reexported, Level::Reachable, Level::ReachableThroughImplTrait];
}

/// A visibility: everywhere, or in a module and the modules in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Vis {
    Public,
    In(hir::Module),
}

/// rustc's `EffectiveVisibility`: a visibility at each level, by `Level as usize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Ev([Vis; 4]);

impl Ev {
    fn from_vis(vis: Vis) -> Ev {
        Ev([vis; 4])
    }

    pub(super) fn at(self, level: Level) -> Vis {
        self.0[level as usize]
    }

    fn at_mut(&mut self, level: Level) -> &mut Vis {
        &mut self.0[level as usize]
    }

    /// `EffectiveVisibility::min`: at each level the lesser.
    fn min(mut self, cx: &Cx<'_, '_>, other: Ev) -> Ev {
        for level in Level::ALL {
            *self.at_mut(level) = min(cx, self.at(level), other.at(level));
        }
        self
    }
}

/// The effective visibilities of the crate's definitions.
#[derive(Default)]
pub(crate) struct Effective {
    ids: HashMap<Def, u32>,
    defs: Vec<Def>,
    /// rustc's table: `None` where it has no entry.
    ev: Vec<Option<Ev>>,
    /// The visibility written (`local_visibility`), the most a level can be.
    nominal: Vec<Vis>,
    /// Private to its module: `Restricted(parent_module_from_def_id)`.
    private: Vec<Vis>,
    local: Vec<bool>,
}

impl Effective {
    pub(crate) fn of(cx: &Cx<'_, '_>) -> Effective {
        let db = cx.db;
        let modules = crate::checks::modules(cx);
        // alloc: setup, once per crate: the scopes of blocks hold impls and `impl Trait`s too.
        let mut scopes = modules.clone();
        for owner in every_body(db, cx.krate.base()) {
            for (_, def_map) in Body::of(db, owner).blocks(db) {
                scopes.extend(def_map.modules(db).map(|(id, _)| hir::Module::from(id)));
            }
        }
        let mut build = Build {
            cx,
            t: Effective::default(),
            queue: Vec::new(),  // alloc: setup, a definition is queued once at a time
            queued: Vec::new(), // alloc: setup, a column over the numbering
            impls_of: HashMap::default(),
            names: Names::new(db, false),
        };
        build.resolve(&modules);
        build.embargo(&scopes);
        build.t
    }

    /// rustc's `effective_vis`.
    pub(super) fn get(&self, def: Def) -> Option<Ev> {
        self.ids.get(&def).and_then(|&id| self.ev[id as usize])
    }

    /// rustc's `is_public_at_level`.
    pub(super) fn is_public_at(&self, def: Def, level: Level) -> bool {
        self.get(def).is_some_and(|ev| ev.at(level) == Vis::Public)
    }

    /// rustc's `is_exported`: another crate can name it.
    pub(super) fn is_exported(&self, def: Def) -> bool {
        self.is_public_at(def, Level::Reexported)
    }

    /// rustc's `is_reachable`: another crate can name it or use it through what it can name.
    pub(crate) fn is_reachable(&self, def: Def) -> bool {
        self.is_public_at(def, Level::Reachable)
    }

    /// The crate's own definitions [`Effective::is_reachable`] holds of, in numbering order.
    pub(crate) fn reachable_local(&self) -> impl Iterator<Item = Def> + '_ {
        (self.defs.iter().zip(&self.ev).zip(&self.local))
            .filter(|((_, ev), local)| **local && matches!(ev, Some(ev) if ev.at(Level::Reachable) == Vis::Public))
            .map(|((def, _), _)| *def)
    }

    /// `EffectiveVisibility::of_impl::<false>`: the least effective visibility of the local
    /// definitions its self type and trait name, arguments too.
    pub(super) fn of_impl(&self, cx: &Cx<'_, '_>, imp: hir::Impl) -> Ev {
        impl_defs(cx, imp, false).into_iter().fold(Ev::from_vis(Vis::Public), |at, def| {
            let ev = self.get(def).unwrap_or_else(|| Ev::from_vis(private(cx, def)));
            ev.min(cx, at)
        })
    }
}

/// The local definitions an impl's self type and trait name, as `of_impl` reads them: only the
/// outermost ones when `shallow`, associated types not looked into.
pub(super) fn impl_defs(cx: &Cx<'_, '_>, imp: hir::Impl, shallow: bool) -> Vec<Def> {
    let db = cx.db;
    let mut names = Names::new(db, true);
    let self_ty = imp.self_ty(db).to_next_solver();
    match shallow {
        true => names.outermost(self_ty),
        false => names.ty(self_ty),
    }
    match (shallow, imp.trait_(db)) {
        (true, Some(tr)) => names.push(Def::Item(ModuleDef::Trait(tr))),
        (false, Some(_)) => names.trait_ref(imp),
        (_, None) => {}
    }
    let mut found = names.found;
    found.retain(|it| is_local(cx, *it));
    found
}

pub(super) fn vis_of(cx: &Cx<'_, '_>, vis: hir::Visibility) -> Vis {
    match vis {
        hir::Visibility::Public => Vis::Public,
        hir::Visibility::PubCrate(_) => Vis::In(cx.krate.root_module(cx.db)),
        hir::Visibility::Module(id, _) => Vis::In(hir::Module::from(id).nearest_non_block_module(cx.db)),
    }
}

/// rustc's `is_at_least`.
pub(super) fn at_least(cx: &Cx<'_, '_>, a: Vis, b: Vis) -> bool {
    match (a, b) {
        (Vis::Public, _) => true,
        (_, Vis::Public) => false,
        (Vis::In(a), Vis::In(b)) => {
            let mut at = Some(b);
            while let Some(module) = at {
                if module == a {
                    return true;
                }
                at = module.parent(cx.db);
            }
            false
        }
    }
}

/// rustc's `Visibility::partial_cmp`: modules apart are unordered.
fn partial_cmp(cx: &Cx<'_, '_>, a: Vis, b: Vis) -> Option<Ordering> {
    if a == b {
        Some(Ordering::Equal)
    } else if at_least(cx, a, b) {
        Some(Ordering::Greater)
    } else if at_least(cx, b, a) {
        Some(Ordering::Less)
    } else {
        None
    }
}

pub(super) fn min(cx: &Cx<'_, '_>, a: Vis, b: Vis) -> Vis {
    if at_least(cx, a, b) { b } else { a }
}

/// rustc's `greater_than`.
pub(super) fn greater(cx: &Cx<'_, '_>, a: Vis, b: Vis) -> bool {
    a != b && at_least(cx, a, b)
}

/// The module a def is in, not counting blocks: rustc's `parent_module_from_def_id`.
pub(super) fn module_of(cx: &Cx<'_, '_>, def: Def) -> Option<hir::Module> {
    let db = cx.db;
    let module = match def {
        Def::Item(it) => it.module(db)?,
        Def::Field(it) => it.parent_def(db).module(db),
        Def::Impl(it) => it.module(db),
        Def::Use(it) => hir::Module::from(it.lookup(db).container),
    };
    Some(module.nearest_non_block_module(db))
}

pub(super) fn is_local(cx: &Cx<'_, '_>, def: Def) -> bool {
    module_of(cx, def).is_some_and(|it| it.krate(cx.db) == cx.krate)
}

/// What a def is visible in as it is written: an enum's variants and their fields as the enum,
/// a trait's items as the trait. An impl has none of its own, nor a `use` here (each of its
/// names has the `use`'s, read from the scope).
pub(super) fn nominal(cx: &Cx<'_, '_>, def: Def) -> Vis {
    let db = cx.db;
    match def {
        Def::Item(ModuleDef::EnumVariant(it)) => vis_of(cx, it.parent_enum(db).visibility(db)),
        Def::Item(it) => vis_of(cx, it.visibility(db)),
        Def::Field(it) => match it.parent_def(db) {
            hir::Variant::EnumVariant(variant) => vis_of(cx, variant.parent_enum(db).visibility(db)),
            _ => vis_of(cx, it.visibility(db)),
        },
        Def::Impl(_) => Vis::Public,
        Def::Use(_) => private(cx, def),
    }
}

/// The visibility of a def private to its module.
pub(super) fn private(cx: &Cx<'_, '_>, def: Def) -> Vis {
    match module_of(cx, def) {
        Some(module) => Vis::In(module),
        None => Vis::Public,
    }
}

/// `EffectiveVisibilities::update` of `current`: at each level up to `level`, raised to the
/// inherited visibility capped by `max`; whether any rose.
fn raise(cx: &Cx<'_, '_>, mut current: Ev, max: Option<Vis>, inherited: Ev, level: Level) -> (Ev, bool) {
    let mut changed = false;
    let mut inherited_at_prev = inherited.at(level);
    let mut calculated = inherited_at_prev;
    for l in Level::ALL {
        if level >= l {
            let inherited_at = inherited.at(l);
            if !(inherited_at_prev == inherited_at && level != l) {
                calculated = match max {
                    Some(max) if partial_cmp(cx, inherited_at, max) == Some(Ordering::Greater) => max,
                    _ => inherited_at,
                };
            }
            if partial_cmp(cx, calculated, current.at(l)) == Some(Ordering::Greater) {
                changed = true;
                *current.at_mut(l) = calculated;
            }
            inherited_at_prev = inherited_at;
        }
    }
    (current, changed)
}

struct Build<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    t: Effective,
    /// The definitions to check again, each once at a time (`queued`).
    queue: Vec<u32>,
    queued: Vec<bool>,
    /// rustc's `def_ids_to_impls`: the impls whose outermost self type or trait a type or trait is.
    impls_of: HashMap<u32, Vec<u32>>,
    /// The definitions a walk names, reused from walk to walk.
    names: Names<'db>,
}

impl<'db> Build<'_, '_, 'db> {
    fn id(&mut self, def: Def) -> u32 {
        if let Some(&id) = self.t.ids.get(&def) {
            return id;
        }
        let cx = self.cx;
        let id = self.t.defs.len() as u32;
        self.t.ids.insert(def, id);
        self.t.defs.push(def);
        self.t.ev.push(None);
        self.t.nominal.push(nominal(cx, def));
        self.t.private.push(private(cx, def));
        self.t.local.push(is_local(cx, def));
        self.queued.push(false);
        id
    }

    /// `effective_vis_or_private`: an entry is made private where there is none.
    fn ev_or_private(&mut self, id: u32) -> Ev {
        let private = self.t.private[id as usize];
        *self.t.ev[id as usize].get_or_insert(Ev::from_vis(private))
    }

    /// `EffectiveVisibilities::update`.
    fn update(&mut self, id: u32, max: Option<Vis>, private: Vis, inherited: Ev, level: Level) -> bool {
        let current = self.t.ev[id as usize].unwrap_or(Ev::from_vis(private));
        let (new, changed) = raise(self.cx, current, max, inherited, level);
        self.t.ev[id as usize] = Some(new);
        changed
    }

    /// `EffectiveVisibilities::update_eff_vis`: each level raised to `ev`'s.
    fn merge(&mut self, id: u32, ev: Ev) {
        let cx = self.cx;
        match &mut self.t.ev[id as usize] {
            Some(old) => {
                for level in Level::ALL {
                    if greater(cx, ev.at(level), old.at(level)) {
                        *old.at_mut(level) = ev.at(level);
                    }
                }
            }
            slot @ None => *slot = Some(ev),
        }
    }

    fn push(&mut self, id: u32) {
        if !self.queued[id as usize] {
            self.queued[id as usize] = true;
            self.queue.push(id);
        }
    }

    /// Resolve's pass: from the crate's root, the names of each module (an enum's variants, a
    /// trait's items) and the fields of each struct and variant, until nothing rises.
    fn resolve(&mut self, modules: &[hir::Module]) {
        let db = self.cx.db;
        let root = self.id(Def::Item(ModuleDef::Module(self.cx.krate.root_module(db))));
        self.t.ev[root as usize] = Some(Ev::from_vis(Vis::Public));
        for &module in modules {
            let id = self.id(Def::Item(ModuleDef::Module(module)));
            self.push(id);
            for def in module.declarations_iter(db) {
                if matches!(def, ModuleDef::Adt(_) | ModuleDef::Trait(_)) {
                    let id = self.id(Def::Item(def));
                    self.push(id);
                }
            }
        }
        while let Some(scope) = self.queue.pop() {
            self.queued[scope as usize] = false;
            self.bindings(scope);
        }
    }

    /// `set_bindings_effective_visibilities` of a module, an enum or a trait, and
    /// `update_field` of the fields of a struct, a union or an enum's variants.
    fn bindings(&mut self, scope: u32) {
        let cx = self.cx;
        let db = cx.db;
        let inherited = self.ev_or_private(scope);
        let scope_vis = self.t.nominal[scope as usize];
        match self.t.defs[scope as usize] {
            Def::Item(ModuleDef::Module(module)) => {
                let here = Vis::In(module);
                let id: hir_def::ModuleId = module.into();
                let items = id.def_map(db).module(db, id).scope(db);
                for (_, per_ns) in items.entries() {
                    if let Some(it) = per_ns.types {
                        self.binding(Some(it.def), it.vis, it.import.and_then(ImportOrExternCrate::use_), here, scope_vis, inherited);
                    }
                    if let Some(it) = per_ns.values {
                        let import = it.import.map(|import| match import {
                            ImportOrGlob::Import(it) => it.use_,
                            ImportOrGlob::Glob(it) => it.use_,
                        });
                        self.binding(Some(it.def), it.vis, import, here, scope_vis, inherited);
                    }
                    if let Some(it) = per_ns.macros {
                        let import = it.import.and_then(ImportOrExternCrate::use_);
                        self.binding(Some(ModuleDefId::MacroId(it.def)), it.vis, import, here, scope_vis, inherited);
                    }
                }
                for it in items.underscore_imports() {
                    self.binding(it.def, it.vis, Some(it.import.use_), here, scope_vis, inherited);
                }
            }
            Def::Item(ModuleDef::Adt(hir::Adt::Enum(it))) => {
                let here = self.t.private[scope as usize];
                let variants = it.variants(db);
                for &variant in &variants {
                    let id = self.id(Def::Item(ModuleDef::EnumVariant(variant)));
                    self.update_def(id, self.t.nominal[id as usize], scope_vis, here, inherited, Level::Direct);
                }
                for variant in variants {
                    let parent = self.id(Def::Item(ModuleDef::EnumVariant(variant)));
                    for field in variant.fields_iter(db) {
                        self.update_field(field, parent, here);
                    }
                }
            }
            Def::Item(ModuleDef::Adt(adt)) => {
                let here = self.t.private[scope as usize];
                let fields = match adt {
                    hir::Adt::Struct(it) => hir::Variant::from(it).fields_iter(db),
                    hir::Adt::Union(it) => hir::Variant::from(it).fields_iter(db),
                    hir::Adt::Enum(_) => return,
                };
                for field in fields {
                    self.update_field(field, scope, here);
                }
            }
            Def::Item(ModuleDef::Trait(it)) => {
                let here = self.t.private[scope as usize];
                for item in it.items_iter(db) {
                    let id = self.id(Def::Item(item.into()));
                    self.update_def(id, self.t.nominal[id as usize], scope_vis, here, inherited, Level::Direct);
                }
            }
            _ => {}
        }
    }

    /// One name of a module, `update_decl_chain`: of its own definition `Direct`; through a
    /// `use`, the `use`'s name `Direct` and its definition `Reexported` from it.
    fn binding(
        &mut self,
        def: Option<ModuleDefId>,
        vis: hir::Visibility,
        import: Option<UseId>,
        here: Vis,
        scope_vis: Vis,
        inherited: Ev,
    ) {
        let cx = self.cx;
        let vis = vis_of(cx, vis);
        let def = def.map(|it| self.id(Def::Item(ModuleDef::from(it))));
        let Some(use_) = import else {
            if let Some(id) = def
                && self.t.local[id as usize]
            {
                self.update_def(id, vis, scope_vis, here, inherited, Level::Direct);
            }
            return;
        };
        // `update_import`: the name the `use` gives, private to its module where it can't rise.
        let decl = match vis != here && scope_vis != here {
            true => raise(cx, Ev::from_vis(here), Some(vis), inherited, Level::Direct).0,
            false => Ev::from_vis(here),
        };
        let id = self.id(Def::Use(use_));
        self.merge(id, decl);
        if let Some(id) = def
            && self.t.local[id as usize]
        {
            let private = self.t.private[id as usize];
            self.update_def(id, self.t.nominal[id as usize], vis, private, decl, Level::Reexported);
        }
    }

    /// `update_def`: nothing rises where the name's visibility or its parent's is private.
    fn update_def(&mut self, id: u32, vis: Vis, parent_vis: Vis, private: Vis, inherited: Ev, level: Level) {
        if vis == private || parent_vis == private {
            return;
        }
        if self.update(id, Some(vis), private, inherited, level) {
            self.rise(id);
        }
    }

    /// `update_field` of a field of the struct or variant `parent`.
    fn update_field(&mut self, field: hir::Field, parent: u32, here: Vis) {
        let id = self.id(Def::Field(field));
        let inherited = self.ev_or_private(parent);
        let parent_vis = self.t.nominal[parent as usize];
        self.update_def(id, self.t.nominal[id as usize], parent_vis, here, inherited, Level::Direct);
    }

    /// What resolve's pass walks again when a definition rises: its names and fields.
    fn rise(&mut self, id: u32) {
        let db = self.cx.db;
        match self.t.defs[id as usize] {
            Def::Item(ModuleDef::Module(_) | ModuleDef::Adt(_) | ModuleDef::Trait(_)) => self.push(id),
            Def::Item(ModuleDef::EnumVariant(it)) => {
                let parent = self.id(Def::Item(ModuleDef::Adt(hir::Adt::Enum(it.parent_enum(db)))));
                self.push(parent);
            }
            _ => {}
        }
    }

    /// The embargo visitor's pass: the hidden types of `impl Trait`s, then each definition with
    /// a visibility and each impl, and what each rise reaches, until nothing rises.
    fn embargo(&mut self, scopes: &[hir::Module]) {
        let cx = self.cx;
        let db = cx.db;
        let mut impls = Vec::new(); // alloc: setup, once per crate
        for &scope in scopes {
            for imp in scope.impl_defs_iter(db) {
                let id = self.id(Def::Impl(imp));
                impls.push(id);
                for def in impl_defs(cx, imp, true) {
                    let def = self.id(def);
                    // alloc: setup, once per impl and what it is of
                    self.impls_of.entry(def).or_default().push(id);
                }
            }
        }
        self.through_impl_trait(scopes);
        for id in 0..self.t.defs.len() as u32 {
            if self.t.ev[id as usize].is_some() {
                self.enqueue(id);
            }
        }
        for id in impls {
            self.push(id);
        }
        while let Some(id) = self.queue.pop() {
            self.queued[id as usize] = false;
            self.check(id);
        }
    }

    /// The pass over every opaque type: what the hidden type of each `impl Trait` a function
    /// with a body returns names is public at `ReachableThroughImplTrait`.
    fn through_impl_trait(&mut self, scopes: &[hir::Module]) {
        let db = self.cx.db;
        for &scope in scopes {
            for def in scope.declarations_iter(db) {
                match def {
                    ModuleDef::Function(it) => self.hidden(it),
                    ModuleDef::Trait(it) => {
                        for item in it.items_iter(db) {
                            if let AssocItem::Function(it) = item
                                && it.has_body(db)
                            {
                                self.hidden(it);
                            }
                        }
                    }
                    _ => {}
                }
            }
            for imp in scope.impl_defs_iter(db) {
                for item in imp.items_iter(db) {
                    if let AssocItem::Function(it) = item {
                        self.hidden(it);
                    }
                }
            }
        }
    }

    fn hidden(&mut self, function: hir::Function) {
        let db = self.cx.db;
        let Ok(id) = FunctionId::try_from(function) else { return };
        if !function.ret_type(db).to_next_solver().has_opaque_types() {
            return;
        }
        let hidden = hir_ty::rpit_hidden_types(db, id);
        self.reach(Ev::from_vis(Vis::Public), Level::ReachableThroughImplTrait, |names| {
            for (_, ty) in hidden.iter() {
                names.ty(ty.get().skip_binder());
            }
        });
    }

    /// `enqueue_def_id` of a definition that rose: a type or a trait with the impls of it, an
    /// item, an associated item's trait or impl.
    fn enqueue(&mut self, id: u32) {
        let db = self.cx.db;
        let Def::Item(def) = self.t.defs[id as usize] else { return };
        if let Some(assoc) = def.as_assoc_item(db) {
            let container = match assoc.container(db) {
                AssocItemContainer::Trait(it) => Def::Item(ModuleDef::Trait(it)),
                AssocItemContainer::Impl(it) => Def::Impl(it),
            };
            let container = self.id(container);
            self.push(container);
            return;
        }
        match def {
            ModuleDef::Adt(_) | ModuleDef::Trait(_) | ModuleDef::TypeAlias(_) => {
                self.push(id);
                if let Some(impls) = self.impls_of.get(&id) {
                    for &imp in impls {
                        if !self.queued[imp as usize] {
                            self.queued[imp as usize] = true;
                            self.queue.push(imp);
                        }
                    }
                }
            }
            ModuleDef::Function(_) | ModuleDef::Const(_) | ModuleDef::Static(_) => self.push(id),
            _ => {}
        }
    }

    /// `update_eff_vis`: nothing rises whose most is private.
    fn update_eff_vis(&mut self, id: u32, inherited: Ev, max: Option<Vis>, level: Level) -> bool {
        let private = self.t.private[id as usize];
        if max == Some(private) {
            return false;
        }
        self.update(id, max, private, inherited, level)
    }

    /// `EmbargoVisitor::update`: at most its own visibility.
    fn update_nominal(&mut self, id: u32, inherited: Ev, level: Level) {
        let nominal = self.t.nominal[id as usize];
        self.update_eff_vis(id, inherited, Some(nominal), level);
    }

    /// `ReachEverythingInTheInterfaceVisitor`: what `collect` names rises to `ev` at `level`,
    /// at most its own visibility below `ReachableThroughImplTrait`, and is walked again.
    fn reach(&mut self, ev: Ev, level: Level, collect: impl FnOnce(&mut Names<'db>)) {
        collect(&mut self.names);
        let mut found = std::mem::take(&mut self.names.found);
        for &def in &found {
            let id = self.id(def);
            if !self.t.local[id as usize] {
                continue;
            }
            let max = (level != Level::ReachableThroughImplTrait).then(|| self.t.nominal[id as usize]);
            if self.update_eff_vis(id, ev, max, level) {
                self.enqueue(id);
            }
        }
        found.clear();
        self.names.found = found;
        self.names.seen.clear();
    }

    /// `check_def_id`.
    fn check(&mut self, id: u32) {
        let db = self.cx.db;
        let ev = self.t.ev[id as usize];
        match self.t.defs[id as usize] {
            Def::Item(def @ (ModuleDef::Function(_) | ModuleDef::Const(_) | ModuleDef::Static(_) | ModuleDef::TypeAlias(_))) => {
                let Some(ev) = ev else { return };
                if def.as_assoc_item(db).is_some() {
                    return;
                }
                self.reach(ev, Level::Reachable, |names| match def {
                    ModuleDef::Function(it) => {
                        names.generics(GenericDef::Function(it));
                        names.clauses(GenericDef::Function(it));
                        names.sig(it);
                    }
                    ModuleDef::Const(it) => {
                        names.generics(GenericDef::Const(it));
                        names.clauses(GenericDef::Const(it));
                        names.ty(it.ty(db).to_next_solver());
                    }
                    ModuleDef::Static(it) => {
                        names.generics(GenericDef::Static(it));
                        names.clauses(GenericDef::Static(it));
                        names.ty(it.ty(db).to_next_solver());
                    }
                    ModuleDef::TypeAlias(it) => {
                        names.generics(GenericDef::TypeAlias(it));
                        names.clauses(GenericDef::TypeAlias(it));
                        names.ty(it.ty(db).to_next_solver());
                    }
                    _ => {}
                });
            }
            Def::Item(ModuleDef::Trait(trait_)) => {
                let Some(ev) = ev else { return };
                self.reach(ev, Level::Reachable, |names| {
                    names.generics(GenericDef::Trait(trait_));
                    names.clauses(GenericDef::Trait(trait_));
                    for it in trait_.direct_supertraits(db) {
                        names.push(Def::Item(ModuleDef::Trait(it)));
                    }
                });
                for item in trait_.items_iter(db) {
                    let item_id = self.id(Def::Item(item.into()));
                    self.update_nominal(item_id, ev, Level::Reachable);
                    self.check_assoc(item, ev, None);
                }
            }
            Def::Impl(imp) => {
                // The least of what its outermost self type and trait are: an impl inference can
                // pick knowing only those (#57264).
                let ev = self.impl_ev(imp);
                self.update_eff_vis(id, ev, None, Level::Direct);
                let of_trait = imp.trait_(db).is_some();
                self.reach(ev, Level::Reachable, |names| {
                    names.generics(GenericDef::Impl(imp));
                    names.clauses(GenericDef::Impl(imp));
                    names.ty(imp.self_ty(db).to_next_solver());
                    if of_trait {
                        names.trait_ref(imp);
                    }
                });
                for item in imp.items_iter(db) {
                    let item_id = self.id(Def::Item(item.into()));
                    let max = (!of_trait).then(|| self.t.nominal[item_id as usize]);
                    self.update_eff_vis(item_id, ev, max, Level::Direct);
                    if let Some(item_ev) = self.t.ev[item_id as usize] {
                        self.check_assoc(item, item_ev, Some(imp));
                    }
                }
            }
            Def::Item(ModuleDef::Adt(adt @ hir::Adt::Enum(it))) => {
                if let Some(ev) = ev {
                    self.reach(ev, Level::Reachable, |names| {
                        names.generics(GenericDef::Adt(adt));
                        names.clauses(GenericDef::Adt(adt));
                    });
                }
                for variant in it.variants_iter(db) {
                    let variant_id = self.id(Def::Item(ModuleDef::EnumVariant(variant)));
                    if let Some(ev) = ev {
                        self.update_nominal(variant_id, ev, Level::Reachable);
                    }
                    let Some(variant_ev) = self.t.ev[variant_id as usize] else { continue };
                    for field in variant.fields_iter(db) {
                        let field_id = self.id(Def::Field(field));
                        self.update_nominal(field_id, variant_ev, Level::Reachable);
                        self.reach(variant_ev, Level::Reachable, |names| names.ty(field.ty(db).to_next_solver()));
                    }
                    // A variant reachable makes its enum so.
                    self.reach(variant_ev, Level::Reachable, |names| names.push(Def::Item(ModuleDef::Adt(adt))));
                }
            }
            Def::Item(ModuleDef::Adt(adt)) => {
                // A tuple or unit struct's constructor is the struct here: it has its ev.
                let Some(ev) = ev else { return };
                self.reach(ev, Level::Reachable, |names| {
                    names.generics(GenericDef::Adt(adt));
                    names.clauses(GenericDef::Adt(adt));
                });
                let fields = match adt {
                    hir::Adt::Struct(it) => hir::Variant::from(it).fields_iter(db),
                    hir::Adt::Union(it) => hir::Variant::from(it).fields_iter(db),
                    hir::Adt::Enum(_) => return,
                };
                for field in fields {
                    let field_id = self.id(Def::Field(field));
                    self.update_nominal(field_id, ev, Level::Reachable);
                    if let Some(field_ev) = self.t.ev[field_id as usize] {
                        self.reach(field_ev, Level::Reachable, |names| names.ty(field.ty(db).to_next_solver()));
                    }
                }
            }
            _ => {}
        }
    }

    /// `check_assoc_item` of an item of a trait (`impl_` none) or of an impl.
    fn check_assoc(&mut self, item: AssocItem, ev: Ev, impl_: Option<hir::Impl>) {
        let db = self.cx.db;
        self.reach(ev, Level::Reachable, |names| {
            let generic = match item {
                AssocItem::Function(it) => GenericDef::Function(it),
                AssocItem::Const(it) => GenericDef::Const(it),
                AssocItem::TypeAlias(it) => GenericDef::TypeAlias(it),
            };
            names.generics(generic);
            names.clauses(generic);
            match item {
                AssocItem::Function(it) => {
                    names.sig(it);
                    if let Some(imp) = impl_ {
                        names.ty(imp.self_ty(db).to_next_solver());
                    }
                }
                AssocItem::Const(it) => names.ty(it.ty(db).to_next_solver()),
                AssocItem::TypeAlias(it) => match impl_ {
                    Some(_) => names.ty(it.ty(db).to_next_solver()),
                    None => names.bounds(TypeAliasId::from(it)),
                },
            }
        });
    }

    /// `EffectiveVisibility::of_impl::<true>`, read from the table as it stands.
    fn impl_ev(&mut self, imp: hir::Impl) -> Ev {
        let cx = self.cx;
        let mut ev = Ev::from_vis(Vis::Public);
        for def in impl_defs(cx, imp, true) {
            let id = self.id(def);
            let at = self.t.ev[id as usize].unwrap_or(Ev::from_vis(self.t.private[id as usize]));
            ev = at.min(cx, ev);
        }
        ev
    }
}

/// The definitions a walk of an interface names, as `DefIdVisitorSkeleton` visits types.
struct Names<'db> {
    db: &'db Db,
    found: Vec<Def>,
    /// The aliases and opaque types walked, each once.
    seen: HashSet<Ty<'db>>,
    /// `skip_assoc_tys`: projections not looked into.
    skip_assoc: bool,
}

impl<'db> Names<'db> {
    fn new(db: &'db Db, skip_assoc: bool) -> Names<'db> {
        // alloc: setup, the buffers are reused for every walk
        Names { db, found: Vec::new(), seen: HashSet::default(), skip_assoc }
    }

    fn push(&mut self, def: Def) {
        self.found.push(def);
    }

    fn ty(&mut self, ty: Ty<'db>) {
        ty.visit_with(self);
    }

    /// A shallow walk's: the outermost definitions only.
    fn outermost(&mut self, ty: Ty<'db>) {
        match ty.kind() {
            TyKind::Adt(def, _) => self.push(Def::Item(ModuleDef::Adt(def.def_id().into()))),
            TyKind::Foreign(it) => self.push(Def::Item(ModuleDef::TypeAlias(it.0.into()))),
            TyKind::FnDef(def, _) => {
                if let CallableDefId::FunctionId(it) = def.0 {
                    self.push(Def::Item(ModuleDef::Function(it.into())));
                }
            }
            TyKind::Dynamic(..) => {
                if let Some(it) = ty.dyn_trait() {
                    self.push(Def::Item(ModuleDef::Trait(it.into())));
                }
            }
            TyKind::Alias(_, AliasTy { kind: AliasTyKind::Opaque { .. }, .. }) => {
                for clause in ty.impl_trait_bounds(self.db).into_iter().flatten() {
                    if let ClauseKind::Trait(it) = clause.kind().skip_binder() {
                        self.push(Def::Item(ModuleDef::Trait(it.def_id().0.into())));
                    }
                }
            }
            _ => {}
        }
    }

    /// `generics()`: the types of const parameters and the defaults of type parameters.
    fn generics(&mut self, def: GenericDef) {
        let db = self.db;
        for param in def.type_or_const_params(db) {
            match param.as_type_param(db) {
                Some(it) => {
                    if let Some(default) = it.default(db) {
                        self.ty(default.to_next_solver());
                    }
                }
                None => self.ty(param.ty(db).to_next_solver()),
            }
        }
    }

    /// `clauses()`: its own `where` clauses and parameters' bounds.
    fn clauses(&mut self, def: GenericDef) {
        for pred in def.where_predicates(self.db) {
            self.clause(pred.to_next_solver());
        }
    }

    /// `bounds()`: an associated type's bounds.
    fn bounds(&mut self, alias: TypeAliasId) {
        for &clause in self.db.type_alias_bounds(alias).skip_binder() {
            self.clause(clause);
        }
    }

    /// A clause's trait, then its types.
    fn clause(&mut self, clause: Clause<'db>) {
        if let ClauseKind::Trait(it) = clause.kind().skip_binder() {
            self.push(Def::Item(ModuleDef::Trait(it.def_id().0.into())));
        }
        clause.visit_with(self);
    }

    /// A function's type: its signature.
    fn sig(&mut self, function: hir::Function) {
        if let Some(sig) = function.signature(self.db) {
            sig.to_next_solver().visit_with(self);
        }
    }

    /// `trait_ref()`: an impl's trait and its arguments.
    fn trait_ref(&mut self, imp: hir::Impl) {
        let Some(trait_ref) = imp.trait_ref(self.db) else { return };
        self.push(Def::Item(ModuleDef::Trait(trait_ref.trait_())));
        trait_ref.to_next_solver().args.visit_with(self);
    }
}

impl<'db> TypeVisitor<DbInterner<'db>> for Names<'db> {
    type Result = ();

    fn visit_ty(&mut self, ty: Ty<'db>) {
        match ty.kind() {
            TyKind::Adt(..) | TyKind::Foreign(_) | TyKind::FnDef(..) | TyKind::Dynamic(..) => self.outermost(ty),
            TyKind::Alias(_, AliasTy { kind: AliasTyKind::Projection { def_id }, .. }) => {
                if self.skip_assoc || !self.seen.insert(ty) {
                    return;
                }
                if let ItemContainerId::TraitId(it) = def_id.0.lookup(self.db).container {
                    self.push(Def::Item(ModuleDef::Trait(it.into())));
                }
            }
            TyKind::Alias(_, AliasTy { kind: AliasTyKind::Opaque { .. }, .. }) => {
                if !self.seen.insert(ty) {
                    return;
                }
                for clause in ty.impl_trait_bounds(self.db).into_iter().flatten() {
                    self.clause(clause);
                }
            }
            _ => {}
        }
        ty.super_visit_with(self);
    }
}
