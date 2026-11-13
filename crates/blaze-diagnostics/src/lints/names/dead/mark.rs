//! What is live: from the seeds, each live definition's signature and body walked, every path,
//! method call, field read and pattern in them making what it names live, with rustc's rules: a
//! variant in a pattern is not constructed, nor one a match arm's pattern needs within that arm;
//! a field written is not read, nor one of a struct expression (but of a union's); the fields of
//! a `repr(C)` or `repr(transparent)` type, and the public fields of a public type, are read; a
//! trait's associated types are live with it, and an inherent impl's items make its self type
//! live; the items of a trait impl are live once their trait's item and self type are, and the
//! impls of `#[derive(Clone)]` and `#[derive(Debug)]` read nothing. On the way, an assignment of
//! a variable or a field to itself is linted.
//!
//! What a definition's walk does is recorded once per expression store it owns (its
//! signature, its fields, its body, an enum's discriminants) in the front end's ids
//! ([`body::store_marks`], kept by the database), and replayed here in the crate's numbering:
//! a body edit walks that one body again, and the fold reads the same records for everything
//! else. A `use` is walked from what its module's scope binds through it. No syntax is read
//! but a self-assignment's, to lint it, and a closure's level, when a definition is in it.
//! Either way a trait impl's item named through a trait is mapped back to the trait's item as
//! typeck has it, each path is resolved once, and what is live and what was walked are bitsets
//! over the definitions.
//!
//! Follows `compiler/rustc_passes/src/dead.rs` (`MarkSymbolVisitor`, `mark_live_symbols`,
//! `check_impl_or_impl_item_live`, `live_symbols_and_ignored_derived_traits`,
//! `check_for_self_assign`), `compiler/rustc_hir/src/pat_util.rs` (`necessary_variants`) and
//! `compiler/rustc_builtin_macros/src/deriving/` (what each derive's method reads)
//! (MIT or Apache-2.0).

mod body;
#[cfg(test)]
mod reference;

pub(super) use body::{StoreMark, store_marks};

use base_db::Db;
use blaze_hash::{HashMap, HashSet};
use hir::{InFile, ModuleDef};
use hir_def::{
    AdtId, AttrDefId, DefWithBodyId, ExpressionStoreOwnerId, GenericDefId, Lookup as _,
    ModuleDefId, TypeParamId, UseId,
    attrs::AttrFlags,
    builtin_derive::BuiltinDeriveImplTrait,
    expr_store::ExpressionStore,
    hir::{Expr, ExprId, ExprOrPatId},
    item_scope::{ImportOrExternCrate, ImportOrGlob},
};
use hir_ty::next_solver::Ty;
use smallvec::SmallVec;
use std::alloc::Global;
use syntax::SyntaxNodePtr;

use super::{
    defs::{Anon, DefIdx, DefSet, Defs, Id, Kind, Parent, stores},
    res::Res,
    seed::{Crate, Seeds, Unsolved, WorkItem},
};
use crate::{
    lints::names::uses::Named,
    lower::{Found, Phase},
    model::Diag,
};

/// What is live at one point of the pass.
#[derive(Debug, Default)]
pub(super) struct Live {
    live: DefSet,
    /// The closures and anonymous constants walked, which items in them have as their parent.
    anon: HashSet<Anon, Global>,
    /// rustc's `ignored_derived_traits`: of each ADT, the traits of the derives whose impls
    /// were not walked, in the order they were met.
    pub(super) ignored: HashMap<DefIdx, SmallVec<[&'static str; 2]>, Global>,
}

impl Live {
    /// A copy, for rustc's `pre_deferred_seeding`: taken once per crate.
    fn snapshot(&self) -> Live {
        Live {
            live: self.live.clone(), // clone: once per crate, rustc's pre-deferred summary
            anon: self.anon.clone(), // clone: once per crate, rustc's pre-deferred summary
            ignored: self.ignored.clone(), // clone: once per crate, rustc's pre-deferred summary
        }
    }

    pub(super) fn contains(&self, def: DefIdx) -> bool {
        self.live.contains(def)
    }

    pub(super) fn parent_live(&self, parent: Parent) -> bool {
        match parent {
            Parent::Module => true,
            Parent::Def(it) => self.live.contains(it),
            Parent::Anon(it) => self.anon.contains(&it),
        }
    }
}

/// rustc's `DeadCodeLivenessSummary`: what is live before what is public and what an `allow` or
/// `expect` covers are walked (for `dead_code_pub_in_binary`), and at the end.
pub(super) struct Liveness {
    pub(super) pre_deferred: Live,
    pub(super) last: Live,
}

/// rustc's `live_symbols_and_ignored_derived_traits`, with the self-assignments it lints: the
/// fold of the recorded marks of every store a live definition owns.
pub(super) fn live(k: &Crate<'_, '_, '_>, seeds: Seeds) -> (Liveness, Vec<Found>) {
    Mark::new(k, seeds.worklist, None).fold(seeds.unsolved, seeds.deferred)
}

/// How a test has the pass walk a definition.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum How {
    /// As [`live`] does: each store's recorded marks replayed.
    Records,
    /// The reference: the walk over syntax and the body walk that marks as it goes
    /// ([`reference`]).
    Reference,
    /// The planted bug: the records, every self-assignment dropped at the replay.
    RecordsDroppingSelfAssigns,
}

/// [`live`], each definition walked as `how` says.
#[cfg(test)]
pub(super) fn live_as(k: &Crate<'_, '_, '_>, seeds: Seeds, how: How) -> (Liveness, Vec<Found>) {
    let mut m = Mark::new(k, seeds.worklist, None);
    m.how = how;
    m.fold(seeds.unsolved, seeds.deferred)
}

/// blaze: what the definitions of `from` (the code a configuration does not use) refer to, as
/// (referrer, referred) pairs, each referrer the definition whose walk names the other: the
/// same walk as [`live`], every name it resolves recorded, a pattern's too, before rustc's
/// rules decide what is live. A `#[cfg]` that leaves an item out of a configuration is sound
/// only when nothing that stays there names it (`crate::reach`). The self-assignments it
/// finds are [`live`]'s to report, and dropped here.
pub(super) fn references(k: &Crate<'_, '_, '_>, from: Vec<WorkItem>) -> Vec<(DefIdx, DefIdx)> {
    // alloc: setup, the pairs of one configuration's unused code, once per crate observed
    let mut m = Mark::new(k, from, Some(Vec::new()));
    m.mark_live_symbols();
    let mut edges = m.edges.take().unwrap_or_default();
    edges.sort_unstable();
    edges.dedup();
    edges
}

struct Mark<'k, 'c, 'a, 'db> {
    k: &'k Crate<'c, 'a, 'db>,
    worklist: Vec<WorkItem>,
    /// rustc's `scanned` of `(id, comes_from_allow)`: one bitset for each `comes_from_allow`.
    scanned: [DefSet; 2],
    live: Live,
    propagated: bool,
    /// rustc's `in_pat`, for the reference walk; a record carries its own.
    #[cfg(test)]
    in_pat: bool,
    /// rustc's `ignore_variant_stack`, for the reference walk; a record carries its own.
    #[cfg(test)]
    ignore_variants: Vec<DefIdx>,
    #[cfg(test)]
    how: How,
    repr_all_live: bool,
    repr_simd: bool,
    /// The self-assignments linted, by where they are written: one written in a macro's
    /// input can be lowered more than once.
    self_assigns: HashSet<InFile<SyntaxNodePtr>, Global>,
    found: Vec<Found>,
    /// The definition being walked, for [`Mark::edges`].
    current: DefIdx,
    /// blaze: every name the walk resolves, by the definition it is walked in ([`references`]);
    /// none for rustc's pass ([`live`]).
    edges: Option<Vec<(DefIdx, DefIdx)>>,
}

impl<'k, 'c, 'a, 'db> Mark<'k, 'c, 'a, 'db> {
    fn new(k: &'k Crate<'c, 'a, 'db>, worklist: Vec<WorkItem>, edges: Option<Vec<(DefIdx, DefIdx)>>) -> Self {
        let len = k.defs.len();
        Mark {
            k,
            worklist,
            scanned: [DefSet::new(len), DefSet::new(len)],
            live: Live { live: DefSet::new(len), ..Live::default() },
            propagated: false,
            #[cfg(test)]
            in_pat: false,
            #[cfg(test)]
            ignore_variants: Vec::new(), // alloc: test setup, one stack for the reference walk
            #[cfg(test)]
            how: How::Records,
            repr_all_live: false,
            repr_simd: false,
            self_assigns: HashSet::new_in(Global),
            found: Vec::new(), // alloc: the pass's output, once per crate
            current: 0,
            edges,
        }
    }

    /// rustc's two rounds: the seeds, then what is public and what an `allow` or `expect`
    /// covers, each until no trait impl is left whose trait and self type are live.
    fn fold(mut self, mut unsolved: Vec<Unsolved>, deferred: Vec<WorkItem>) -> (Liveness, Vec<Found>) {
        self.mark_with_unsolved(&mut unsolved);
        let pre_deferred = self.live.snapshot();
        self.worklist.extend(deferred);
        self.mark_with_unsolved(&mut unsolved);
        (Liveness { pre_deferred, last: self.live }, self.found)
    }
}

impl<'db> Mark<'_, '_, '_, 'db> {
    /// blaze: `def` is named in the definition being walked ([`references`]).
    fn record(&mut self, def: DefIdx) {
        let current = self.current;
        if let Some(edges) = &mut self.edges
            && def != current
        {
            edges.push((current, def));
        }
    }

    /// rustc's `own_comes_from_allow_expect`.
    fn own(&self, def: DefIdx) -> bool {
        self.propagated && self.k.allow_or_lang(def) == Some(true)
    }

    fn check_def_id(&mut self, def: DefIdx) {
        self.record(def);
        let own = self.own(def);
        if self.k.defs[def].kind != Kind::Field {
            self.worklist.push(WorkItem { id: def, propagated: self.propagated, own });
        }
        if !own {
            self.live.live.insert(def);
        }
    }

    fn insert_def_id(&mut self, def: DefIdx) {
        self.record(def);
        if !self.own(def) {
            self.live.live.insert(def);
        }
    }

    /// rustc's `handle_res` as the walk was where it met `res`: in a pattern (`in_pat`), only
    /// a constant or a type alias it names is used; a variant a match arm being walked needs
    /// (`ignored`) is not constructed.
    fn handle_res_as(&mut self, res: Res, in_pat: bool, ignored: bool) {
        // A name a pattern resolves to is no use of it to rustc, and still a name the code
        // can't compile without.
        match res {
            Res::Def(def) => self.record(def),
            Res::Variant(variant) => {
                self.record(variant);
                if let Parent::Def(adt) = self.k.defs[variant].parent {
                    self.record(adt);
                }
            }
            Res::SelfCtor | Res::None => {}
        }
        match res {
            Res::Def(def) => {
                let in_pat_used =
                    matches!(self.k.defs[def].kind, Kind::Const | Kind::AssocConst | Kind::TyAlias);
                if !in_pat || in_pat_used {
                    self.check_def_id(def);
                }
            }
            Res::Variant(variant) => {
                if in_pat {
                    return;
                }
                if let Parent::Def(adt) = self.k.defs[variant].parent {
                    self.check_def_id(adt);
                }
                if !ignored {
                    self.check_def_id(variant);
                }
            }
            Res::SelfCtor | Res::None => {}
        }
    }

    /// rustc's `handle_res` where the reference walk is.
    #[cfg(test)]
    fn handle_res(&mut self, res: Res) {
        let ignored = matches!(res, Res::Variant(it) if self.ignore_variants.contains(&it));
        self.handle_res_as(res, self.in_pat, ignored);
    }

    /// `mark_live_symbols_and_ignored_derived_traits`: the worklist, then the trait impls and
    /// their items whose trait and self type are live, until none is left that is.
    fn mark_with_unsolved(&mut self, unsolved: &mut Vec<Unsolved>) {
        self.mark_live_symbols();
        let mut to_check = Vec::new(); // alloc: setup, one buffer per fixpoint, reused each round
        self.extract_live(unsolved, &mut to_check);
        while !to_check.is_empty() {
            self.worklist.extend(to_check.drain(..).map(|id| WorkItem::new(id, false)));
            self.mark_live_symbols();
            self.extract_live(unsolved, &mut to_check);
        }
    }

    /// rustc's `check_impl_or_impl_item_live` of each unsolved item, the live ones moved to
    /// `out`: two bit tests each.
    fn extract_live(&self, unsolved: &mut Vec<Unsolved>, out: &mut Vec<DefIdx>) {
        let live = &self.live.live;
        unsolved.retain(|it| {
            let is_live = it.trait_def.is_none_or(|it| live.contains(it))
                && it.self_adt.is_none_or(|it| live.contains(it));
            if is_live {
                out.push(it.def);
            }
            !is_live
        });
    }

    fn mark_live_symbols(&mut self) {
        while let Some(work) = self.worklist.pop() {
            self.propagated = work.propagated;
            if !work.own {
                self.live.live.insert(work.id);
            }
            if !self.scanned[work.propagated as usize].insert(work.id) {
                continue;
            }
            self.current = work.id;
            self.visit(work.id);
        }
    }

    /// rustc's `visit_node` of a definition taken from the worklist.
    fn visit(&mut self, def: DefIdx) {
        let k = self.k;
        let db = k.cx.db;
        let defs = k.defs;
        let d = &defs[def];
        match d.id {
            Id::DeriveMethod(derive) => {
                let loc = derive.loc(db);
                let Some(adt) = defs.get(Id::Adt(loc.adt)) else { return };
                self.derived(adt, loc.trait_);
                return;
            }
            Id::Derive(_) => {
                if let Some(adt) = defs.impl_of(def).and_then(|it| it.self_adt) {
                    self.check_def_id(adt);
                }
                return;
            }
            Id::Variant(_) | Id::Field(_) => return,
            _ => {}
        }
        if let Parent::Def(impl_) = d.parent
            && let Some(&name) = defs.trivial_derives.get(&impl_)
        {
            if let Some(adt) = defs.impl_of(impl_).and_then(|it| it.self_adt) {
                self.ignore(adt, name);
            }
            return;
        }
        let saved = (self.repr_all_live, self.repr_simd);
        self.repr_all_live = false;
        self.repr_simd = false;
        match (d.kind, d.id) {
            (Kind::Struct | Kind::Union, Id::Adt(adt)) => {
                if let Some(repr) = AttrFlags::repr(db, adt) {
                    self.repr_all_live = repr.c() || repr.transparent();
                    self.repr_simd = repr.simd();
                }
                self.variant_data(def, def);
            }
            (Kind::Enum, _) => {
                for &variant in defs.children(def) {
                    self.variant_data(def, variant);
                }
            }
            (Kind::Trait, _) => {
                for &item in defs.children(def) {
                    if defs[item].kind == Kind::AssocTy {
                        self.check_def_id(item);
                    }
                }
            }
            _ => {}
        }
        if let Parent::Def(parent) = d.parent {
            match defs[parent].kind {
                Kind::Trait => self.check_def_id(parent),
                Kind::Impl { of_trait: false } => self.inherent_self_ty(parent),
                _ => {}
            }
        }
        #[cfg(test)]
        if self.how == How::Reference {
            self.reference_walk(def);
            (self.repr_all_live, self.repr_simd) = saved;
            return;
        }
        let id = d.id;
        stores(db, id, |owner| self.replay(owner));
        if let Id::Use(it) = id {
            self.use_(it);
        }
        (self.repr_all_live, self.repr_simd) = saved;
    }

    /// What the walk of the store of `owner` recorded, done in the crate's numbering.
    fn replay(&mut self, owner: ExpressionStoreOwnerId) {
        let k = self.k;
        for &mark in store_marks(k.cx.db, owner, ()) {
            match mark {
                StoreMark::Named { named, in_pat, ignored } => {
                    self.handle_res_as(res(k.defs, named), in_pat, ignored)
                }
                StoreMark::Adt(adt) => {
                    if let Some(def) = k.defs.get(Id::Adt(adt)) {
                        self.check_def_id(def);
                    }
                }
                StoreMark::Field(field) => {
                    if let Some(def) = k.defs.get(Id::Field(field)) {
                        self.insert_def_id(def);
                    }
                }
                StoreMark::Anon { owner, expr, closure } => self.anon(owner, expr, closure),
                StoreMark::SelfAssign { owner, assign, lhs } => self.self_assign(owner, assign, lhs),
                #[cfg(test)]
                StoreMark::Planted(_) => {}
            }
        }
    }

    /// A closure (visited unless an `allow` it has came with what reached it) or an anonymous
    /// constant (visited). Which are visited matters only when a definition is in one.
    fn anon(&mut self, owner: ExpressionStoreOwnerId, expr: ExprId, closure: bool) {
        let k = self.k;
        if !k.defs.in_anon {
            return;
        }
        // Its level is read at its node only when an `allow` there can make it its own.
        let own = closure
            && self.propagated
            && written(k.cx.db, owner, expr).is_some_and(|ptr| {
                k.levels.at(k.cx, k.cx.syntax_view(ptr)).is_allow_or_expect()
            });
        if !own {
            self.live.anon.insert((owner, expr));
        }
    }

    /// rustc's `check_for_self_assign` of what the walk found: linted where it is written,
    /// unless that is in a macro's expansion, with the type inference gave the place.
    fn self_assign(&mut self, owner: ExpressionStoreOwnerId, assign: ExprId, lhs: ExprId) {
        #[cfg(test)]
        if self.how == How::RecordsDroppingSelfAssigns {
            return;
        }
        let cx = self.k.cx;
        let db = cx.db;
        let Some(at) = written(db, owner, assign) else { return };
        if crate::lints::syntax::from_expansion(cx, cx.syntax_view(at)) {
            return;
        }
        if !self.self_assigns.insert(at) {
            return;
        }
        let Some(infer) = body::inference_at(db, owner, lhs) else { return };
        let Some(ty) = infer.type_of_expr_or_pat(ExprOrPatId::ExprId(lhs)) else { return };
        let what = match ExpressionStore::of(db, owner).expr(lhs) {
            Expr::Field { .. } => "field",
            _ => "variable",
        };
        // alloc: the edge, a lint that fires
        let message = blaze_report::say!(
            "useless assignment of ",
            blaze_report::say::Words(what),
            " of type ",
            blaze_report::say::Code(&self.print(owner, ty)),
            " to itself"
        )
        .flat();
        let mut diag = Diag::lint("dead_code", message, cx.span_of(at));
        crate::lints::code_fact(cx, &mut diag, at);
        self.found.push(Found::lint(diag, Phase::Late, at));
    }

    /// A type of the store of `owner` as rustc prints it, through a type of its owner's.
    fn print(&self, owner: ExpressionStoreOwnerId, ty: Ty<'db>) -> String {
        let db = self.k.cx.db;
        let function = |it| hir::Function::from(it).ret_type(db);
        let const_ = |it| hir::Const::from(it).ty(db);
        let static_ = |it| hir::Static::from(it).ty(db);
        let of = match owner {
            ExpressionStoreOwnerId::Body(body) => match body {
                DefWithBodyId::FunctionId(it) => function(it),
                DefWithBodyId::ConstId(it) => const_(it),
                DefWithBodyId::StaticId(it) => static_(it),
                DefWithBodyId::VariantId(it) => hir::EnumVariant::from(it).parent_enum(db).ty(db),
            },
            ExpressionStoreOwnerId::Signature(def) => match def {
                GenericDefId::FunctionId(it) => function(it),
                GenericDefId::ConstId(it) => const_(it),
                GenericDefId::StaticId(it) => static_(it),
                GenericDefId::AdtId(it) => hir::Adt::from(it).ty(db),
                GenericDefId::TypeAliasId(it) => hir::TypeAlias::from(it).ty(db),
                GenericDefId::ImplId(it) => hir::Impl::from(it).self_ty(db),
                GenericDefId::TraitId(it) => {
                    hir::TypeParam::from(TypeParamId::trait_self(it)).ty(db)
                }
            },
            ExpressionStoreOwnerId::VariantFields(it) => hir::Adt::from(it.adt_id(db)).ty(db),
        };
        self.k.cx.ty(&of.derived_from_next_solver(ty))
    }

    /// rustc's walk of a `use`: what each of its paths resolves to is live, as its module's
    /// scope binds it (each namespace's, an underscore import's, one shadowed by another
    /// binding of the name), and a glob's enum (what its path names; a module is nothing).
    fn use_(&mut self, id: UseId) {
        let db = self.k.cx.db;
        let module = id.lookup(db).container;
        let scope = module.def_map(db).module(db, module).scope(db);
        for (_, item) in scope.types() {
            match item.import {
                Some(ImportOrExternCrate::Import(it)) if it.use_ == id => self.named_by_use(item.def),
                Some(ImportOrExternCrate::Glob(it)) if it.use_ == id => self.glob(item.def),
                _ => {}
            }
        }
        for (_, item) in scope.values() {
            match item.import {
                Some(ImportOrGlob::Import(it)) if it.use_ == id => self.named_by_use(item.def),
                Some(ImportOrGlob::Glob(it)) if it.use_ == id => self.glob(item.def),
                _ => {}
            }
        }
        for it in scope.underscore_imports() {
            if it.import.use_ == id
                && let Some(def) = it.def
            {
                self.named_by_use(def);
            }
        }
        for it in scope.shadowed_imports() {
            let Some(def) = it.item.as_module_def_id() else { continue };
            match it.import {
                ImportOrExternCrate::Import(import) if import.use_ == id => self.named_by_use(def),
                ImportOrExternCrate::Glob(glob) if glob.use_ == id => self.glob(def),
                _ => {}
            }
        }
    }

    fn named_by_use(&mut self, def: ModuleDefId) {
        self.handle_res_as(Res::def(self.k.defs, ModuleDef::from(def)), false, false);
    }

    /// A glob imported `def`: the enum it is a variant of is what the glob's path names.
    fn glob(&mut self, def: ModuleDefId) {
        if let ModuleDefId::EnumVariantId(it) = def {
            let enum_ = it.lookup(self.k.cx.db).parent;
            self.named_by_use(ModuleDefId::AdtId(AdtId::EnumId(enum_)));
        }
    }

    /// An inherent impl's item makes its self type live: an ADT, a foreign type, the principal
    /// trait of a `dyn`.
    fn inherent_self_ty(&mut self, impl_: DefIdx) {
        let named = self.k.defs.impl_of(impl_).and_then(|it| it.self_adt.or(it.self_dyn));
        if let Some(it) = named {
            self.check_def_id(it);
        }
    }

    /// rustc's `visit_variant_data`: the fields of `variant` (of the ADT `adt`) live whatever
    /// reads them: all of a `repr(C)` or `repr(transparent)` type's, the positional ones of a
    /// `repr(simd)` type's, the public ones of a public type's.
    fn variant_data(&mut self, adt: DefIdx, variant: DefIdx) {
        let defs = self.k.defs;
        for &field in defs.children(variant) {
            let live = self.repr_all_live
                || (self.repr_simd && defs[field].is_positional())
                || (self.k.reach.is_reachable(adt) && self.k.reach.is_reachable(field));
            if live {
                self.live.live.insert(field);
            }
        }
    }

    /// What the method of a builtin derive's impl reads, as the code the derive writes does:
    /// `Clone` and `Debug` are `rustc_trivial_field_reads` and not walked; `PartialEq`,
    /// `PartialOrd`, `Ord` and `Hash` read every field; `Default` constructs the struct, or the
    /// enum's `#[default]` variant.
    fn derived(&mut self, adt: DefIdx, trait_: BuiltinDeriveImplTrait) {
        let defs = self.k.defs;
        match trait_ {
            BuiltinDeriveImplTrait::Clone | BuiltinDeriveImplTrait::Debug => {
                let name = match trait_ {
                    BuiltinDeriveImplTrait::Clone => "Clone",
                    _ => "Debug",
                };
                self.ignore(adt, name);
            }
            BuiltinDeriveImplTrait::PartialEq
            | BuiltinDeriveImplTrait::PartialOrd
            | BuiltinDeriveImplTrait::Ord
            | BuiltinDeriveImplTrait::Hash => match defs[adt].kind {
                Kind::Enum => {
                    for &variant in defs.children(adt) {
                        for &field in defs.children(variant) {
                            self.insert_def_id(field);
                        }
                    }
                }
                _ => {
                    for &field in defs.children(adt) {
                        self.insert_def_id(field);
                    }
                }
            },
            BuiltinDeriveImplTrait::Default => {
                let db = self.k.cx.db;
                let default =
                    defs.children(adt).iter().copied().find(|&variant| match defs[variant].id {
                        Id::Variant(id) => {
                            AttrFlags::has_word(db, AttrDefId::EnumVariantId(id), "default")
                        }
                        _ => false,
                    });
                match default {
                    Some(variant) => self.handle_res_as(Res::Variant(variant), false, false),
                    None => self.check_def_id(adt),
                }
            }
            BuiltinDeriveImplTrait::Copy
            | BuiltinDeriveImplTrait::Eq
            | BuiltinDeriveImplTrait::CoerceUnsized
            | BuiltinDeriveImplTrait::DispatchFromDyn
            | BuiltinDeriveImplTrait::Reborrow => {}
        }
    }

    /// `should_ignore_item` of a method of a derive of a `rustc_trivial_field_reads` trait: not
    /// walked, the trait kept for `adt`'s note.
    fn ignore(&mut self, adt: DefIdx, name: &'static str) {
        let ignored = self.live.ignored.entry(adt).or_default();
        if !ignored.contains(&name) {
            ignored.push(name);
        }
    }
}

/// The node `expr` of the store of `owner` is written as, if it is written (not made by
/// lowering).
fn written(db: &Db, owner: ExpressionStoreOwnerId, expr: ExprId) -> Option<InFile<SyntaxNodePtr>> {
    let (_, source_map) = ExpressionStore::with_source_map(db, owner);
    let source = source_map.expr_syntax(expr).ok()?;
    Some(source.map(|it| it.syntax_node_ptr()))
}

/// What a recorded name names, in the crate's numbering: `Self` as a constructor is already
/// [`Named::SelfCtor`], so nothing here is mapped as one.
fn res(defs: &Defs, named: Named) -> Res {
    match named {
        Named::Value(it) => Res::value(defs, it, false),
        Named::Type(it) => Res::ty(defs, it, false),
        Named::Candidate(it) => Res::candidate(defs, it),
        Named::Variant(it) => Res::variant(defs, it),
        Named::SelfCtor => Res::SelfCtor,
        Named::None => Res::None,
    }
}
