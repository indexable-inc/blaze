//! Implied bounds (`#![feature(blaze_implied_bounds)]`, docs/v2/plan/design/implied-bounds.md):
//! the bounds declared on the definition of each type and trait an item's header names, as
//! where clauses of the item, with the definition's parameters replaced by the written
//! arguments. `struct C<A: X>` gives `impl<A> C<A> {}` the where clause `A: X`.
//!
//! The header is an impl's self type and trait, a function's parameter and return types (a
//! method's impl adds its own through the parent's predicates), and a trait's supertraits. A
//! type is walked to every depth: each struct, enum and union in it, each `dyn` principal and
//! each projection's trait. Only declared bounds count: a definition's own implied bounds are
//! never read again ([`declared_trait_predicates`]), so the rule does not chase itself.
//!
//! A clause is implied only when its subject is a type parameter or a projection rooted at one
//! (`T: X`, `T::A: X`, `T: 'a`), or it is a lifetime or const condition over parameters. Any
//! other declared bound (`C<u8>` with `u8: X`) stays what it is without the feature, an
//! obligation of the header's well-formedness: implying it would add a where clause on a
//! concrete type, which the solver prefers to the impls that prove it. A clause under a
//! late-bound lifetime of a function is passed over: a where clause would make that lifetime
//! early-bound, which changes the function's type.

use base_db::Db;
use blaze_mem::Frame;
use hir_def::{
    CallableDefId, GenericDefId, GenericParamId, HasModule, TraitId, TypeOrConstParamId,
    TypeParamId,
    expr_store::path::{GenericArg as WrittenArg, GenericArgs as WrittenArgs, Path},
    hir::generics::{BinderParams, GenericParams, TypeParamProvenance, WherePredicate},
    resolver::TypeNs,
    signatures::{FunctionSignature, ImplSignature},
    type_ref::{
        PathId, TraitBoundModifier, TraitRef as WrittenTraitRef, TypeBound, TypeRef, TypeRefId,
    },
    unstable_features::UnstableFeatures,
};
use hir_expand::name::Name;
use rustc_type_ir::{
    AliasTyKind, ClausePolarity, OutlivesClause, TypeVisitableExt,
    inherent::{BoundExistentialPredicates as _, GenericArg as _, IntoKind as _, Term as _},
};

use super::{GenericPredicates, PredicateSource, TyLoweringContext};
use crate::{
    Span,
    db::HirDatabase,
    next_solver::{
        AliasTy, Clause, ClauseKind, DbInterner, EarlyBinder, GenericArg, GenericArgs, TraitRef,
        Ty, TyKind,
    },
};

/// Whether `def` gets the bounds its header implies: a function, impl or trait of a crate with
/// `blaze_implied_bounds` (or `blaze_anon_params`, which needs it). The gate is the crate's
/// that owns `def`, never the definition's: a dependency's types give their bounds to a gated
/// crate's items, and a gated crate's types give none to an ungated crate's.
pub(super) fn applies(db: &Db, def: GenericDefId) -> bool {
    matches!(def, GenericDefId::FunctionId(_) | GenericDefId::ImplId(_) | GenericDefId::TraitId(_))
        && UnstableFeatures::of_root(db, def.module(db).krate(db)).implied_bounds()
}

/// The predicates a trait declares: [`GenericPredicates::query`] without the bounds its own
/// supertraits imply, which [`implied_clauses`] of an item naming the trait must not read
/// again.
pub(super) fn declared_trait_predicates(db: &Db, trait_: TraitId) -> &GenericPredicates {
    if applies(db, trait_.into()) {
        super::declared_trait_predicates(db, trait_)
    } else {
        GenericPredicates::query(db, trait_.into())
    }
}

/// The where clauses `def`'s header implies, each with where it was declared, in the order the
/// header names them, each clause once. Its callers ask only where [`applies`] holds.
pub(super) fn implied_clauses<'db, 'f>(
    db: &'db Db,
    def: GenericDefId,
    frame: &'f Frame,
) -> Vec<(Clause<'db>, PredicateSource), &'f Frame> {
    let mut collector =
        Collector { db, interner: DbInterner::new_no_crate(db), out: Vec::new_in(frame) };
    match def {
        GenericDefId::FunctionId(function) => {
            let sig = db.callable_item_signature(CallableDefId::FunctionId(function));
            for &ty in sig.skip_binder().skip_binder().inputs_and_output.as_slice() {
                collector.ty(ty);
            }
        }
        GenericDefId::ImplId(impl_) => {
            collector.ty(db.impl_self_ty(impl_).skip_binder());
            if let Some(trait_ref) = db.impl_trait(impl_) {
                collector.trait_ref(trait_ref.skip_binder());
            }
        }
        GenericDefId::TraitId(trait_) => {
            let supers = super::explicit_super_clauses(db, trait_).explicit_predicates();
            for clause in supers.skip_binder() {
                match clause.kind().skip_binder() {
                    ClauseKind::Trait(it) => collector.trait_ref(it.trait_ref),
                    // `Self: Tr<A = C<X>>` comes with its `Self: Tr`; the type it binds is named.
                    ClauseKind::Projection(it) => {
                        if let Some(ty) = it.term.as_type() {
                            collector.ty(ty);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    collector.out
}

struct Collector<'db, 'f> {
    db: &'db Db,
    interner: DbInterner<'db>,
    out: Vec<(Clause<'db>, PredicateSource), &'f Frame>,
}

impl<'db> Collector<'db, '_> {
    /// Every definition named in `ty`, at every depth.
    fn ty(&mut self, ty: Ty<'db>) {
        for part in ty.walk().filter_map(|it| it.as_type()) {
            match part.kind() {
                TyKind::Adt(adt, args) => {
                    let from = GenericDefId::AdtId(adt.def_id());
                    let declared = GenericPredicates::query(self.db, from).explicit_predicates();
                    self.instantiate(from, declared, args);
                }
                // `dyn Tr<X>`: the bounds `Tr` declares on its parameters. Those on `Self` are
                // about the `dyn` type, which no clause here is.
                TyKind::Dynamic(bounds, _) => {
                    let principal =
                        bounds.principal().map(|it| it.with_self_ty(self.interner, part));
                    if let Some(trait_ref) = principal.and_then(|it| it.no_bound_vars()) {
                        self.declared_trait(trait_ref);
                    }
                }
                TyKind::Alias(_, alias @ AliasTy { kind: AliasTyKind::Projection { .. }, .. }) => {
                    self.declared_trait(alias.trait_ref(self.interner));
                }
                _ => {}
            }
        }
    }

    /// The trait `trait_ref` names and every definition in its arguments.
    fn trait_ref(&mut self, trait_ref: TraitRef<'db>) {
        self.declared_trait(trait_ref);
        for arg in trait_ref.args.as_slice().iter().skip(1) {
            if let Some(ty) = arg.as_type() {
                self.ty(ty);
            }
        }
    }

    fn declared_trait(&mut self, trait_ref: TraitRef<'db>) {
        let trait_ = trait_ref.def_id.0;
        let declared = declared_trait_predicates(self.db, trait_).explicit_predicates();
        self.instantiate(GenericDefId::TraitId(trait_), declared, trait_ref.args);
    }

    fn instantiate(
        &mut self,
        from: GenericDefId,
        declared: EarlyBinder<'db, impl Iterator<Item = Clause<'db>>>,
        args: GenericArgs<'db>,
    ) {
        let clauses = declared.iter_instantiated(self.interner, args.as_slice());
        for (index, clause) in clauses.enumerate() {
            let clause = clause.skip_norm_wip();
            // alloc: none, a scan of the few clauses one header implies
            if implies(clause) && !self.out.iter().any(|&(it, _)| it == clause) {
                self.out.push((clause, PredicateSource::Implied { from, index: index as u32 }));
            }
        }
    }
}

/// Whether a declared clause, with the header's arguments in, is one the header implies: see
/// the module.
fn implies(clause: Clause<'_>) -> bool {
    if clause.has_escaping_bound_vars() || clause.references_error() {
        return false;
    }
    match clause.kind().skip_binder() {
        ClauseKind::Trait(it) => rooted_at_param(it.self_ty()),
        ClauseKind::Projection(it) => rooted_at_param(it.self_ty()),
        ClauseKind::TypeOutlives(OutlivesClause(ty, _)) => rooted_at_param(ty),
        ClauseKind::RegionOutlives(_) | ClauseKind::ConstEvaluatable(_) => clause.has_param(),
        ClauseKind::ConstArgHasType(..)
        | ClauseKind::WellFormed(_)
        | ClauseKind::HostEffect(_)
        | ClauseKind::UnstableFeature(_) => false,
    }
}

/// `T`, `T::A`, `<T as Tr>::A::B`: a type parameter or a projection rooted at one.
fn rooted_at_param(ty: Ty<'_>) -> bool {
    match ty.kind() {
        TyKind::Param(_) => true,
        TyKind::Alias(_, alias @ AliasTy { kind: AliasTyKind::Projection { .. }, .. }) => {
            rooted_at_param(alias.self_ty())
        }
        _ => false,
    }
}

/// The trait bounds `owner`'s header implies on `param` whose traits may define `assoc_name`,
/// each as the trait ref `param` (`param_ty`) satisfies: what the shorthand `param::assoc_name`
/// reads besides the written bounds ([`super::resolve_type_param_assoc_type_shorthand`]).
/// With no name, every such trait bound, as the shorthand of any name would read it: what
/// [`super::written_implied_predicates`] asks before it says a written bound can go.
///
/// The shorthand is resolved while the header itself is lowered (`-> I::Item`), so this reads
/// the header as written, never [`implied_clauses`]'s lowered one: each struct, enum, union or
/// trait the header's types name with `param` as an argument ([`Header`]), then that
/// definition's declared trait bounds on the parameter it is the argument of. A bound naming
/// the definition's other parameters (`struct C<A: Tr<B>, B>`) needs the header's arguments
/// for them, and only then is the header's type lowered: if one of those arguments is written
/// with this same shorthand, that is a cycle, which the query reports as E0391.
///
/// What [`implied_clauses`] walks and this does not: a shorthand projection's own trait
/// (`I::Out` in a header gives `Tr`'s declared bounds to `Tr`'s other arguments), since which
/// trait that is is a shorthand resolution itself. Such a bound is implied for everything but
/// another shorthand, which says E0220 "not found" as it would with no gate.
pub(super) fn shorthand_bounds<'db, 'f>(
    ctx: &mut TyLoweringContext<'db, '_>,
    owner: GenericDefId,
    param: TypeParamId,
    param_ty: Ty<'db>,
    assoc_name: Option<Name>,
    frame: &'f Frame,
) -> Vec<(TraitId, TraitRef<'db>), &'f Frame> {
    let db = ctx.db;
    let interner = ctx.interner;
    let mut named = Vec::new_in(frame);
    Header { ctx, param, named: &mut named }.of(owner);
    let mut out = Vec::new_in(frame);
    for site in &named {
        let declared = match site.def {
            GenericDefId::TraitId(trait_) => declared_trait_predicates(db, trait_),
            def => GenericPredicates::query(db, def),
        };
        for clause in declared.explicit_predicates().skip_binder() {
            let ClauseKind::Trait(bound) = clause.kind().skip_binder() else { continue };
            let TyKind::Param(subject) = bound.self_ty().kind() else { continue };
            let trait_ = bound.trait_ref.def_id.0;
            if bound.polarity != ClausePolarity::Positive
                || subject.id != site.as_param
                || assoc_name
                    .is_some_and(|it| !super::trait_may_define_assoc_type(db, trait_, it))
            {
                continue;
            }
            // The definition's arguments: `param` where it is written, an error for each other
            // parameter, which the header's own arguments replace only when the bound names one.
            let partial = GenericArgs::for_item(interner, site.def.into(), |_, id, _, _| {
                if id == GenericParamId::TypeParamId(site.as_param) {
                    param_ty.into()
                } else {
                    GenericArg::error_from_id(interner, id)
                }
            });
            let declared_ref = || EarlyBinder::bind(interner, bound.trait_ref);
            let mut trait_ref = declared_ref().instantiate(interner, partial).skip_norm_wip();
            if trait_ref.references_error() {
                let Some(args) = header_args(ctx, site) else { continue };
                trait_ref = declared_ref().instantiate(interner, args).skip_norm_wip();
            }
            // alloc: none, a scan of the few bounds one parameter's position implies
            if !out.iter().any(|&(_, it)| it == trait_ref) {
                out.push((trait_, trait_ref));
            }
        }
    }
    out
}

/// A struct, enum, union or trait a header names with the parameter as a written argument.
struct Named {
    def: GenericDefId,
    /// The definition's parameter that the argument is written for.
    as_param: TypeParamId,
    /// Where the header names the definition, to lower its other arguments from.
    at: Written,
}

#[derive(Clone, Copy)]
enum Written {
    /// A type that names the definition: a struct, enum or union's path, a `dyn` type whose
    /// principal it is, or a qualified path `<X as Tr<..>>::A` of it.
    Type(TypeRefId),
    /// An impl's trait, with the impl's self type.
    ImplTrait(PathId, TypeRefId),
    /// A supertrait of `TraitId`, whose `Self` is the self type.
    Supertrait(PathId, TraitId),
}

/// The header's arguments of `site`'s definition, lowered: the type naming it, or the trait
/// ref. `None` when the lowered type is not the definition (an error the header's own
/// lowering reports).
fn header_args<'db>(
    ctx: &mut TyLoweringContext<'db, '_>,
    site: &Named,
) -> Option<GenericArgs<'db>> {
    let interner = ctx.interner;
    let (path, self_ty, trait_) = match site.at {
        Written::Type(ty) => {
            let lowered = ctx.lower_ty(ty);
            let (def, args) = match lowered.kind() {
                TyKind::Adt(adt, args) => (GenericDefId::AdtId(adt.def_id()), args),
                TyKind::Dynamic(bounds, _) => {
                    let principal =
                        bounds.principal()?.with_self_ty(interner, lowered).no_bound_vars()?;
                    (GenericDefId::TraitId(principal.def_id.0), principal.args)
                }
                TyKind::Alias(_, alias @ AliasTy { kind: AliasTyKind::Projection { .. }, .. }) => {
                    let trait_ref = alias.trait_ref(interner);
                    (GenericDefId::TraitId(trait_ref.def_id.0), trait_ref.args)
                }
                _ => return None,
            };
            return (def == site.def).then_some(args);
        }
        Written::ImplTrait(path, self_ty) => (path, ctx.lower_ty(self_ty), site.def),
        Written::Supertrait(path, owner) => {
            let self_param = TypeParamId::trait_self(owner);
            let index = ctx.generics.type_or_const_param_idx(self_param.into());
            (path, Ty::new_param(interner, self_param, index), site.def)
        }
    };
    let GenericDefId::TraitId(trait_) = trait_ else { return None };
    let mut path = ctx.at_path(path);
    let Some(TypeNs::TraitId(resolved)) = path.resolve_path_in_type_ns_fully() else {
        return None;
    };
    (resolved == trait_).then(|| {
        path.lower_trait_ref_from_resolved_path(trait_, self_ty, false, Span::Dummy).args
    })
}

/// The pre-pass over a header's written types: [`Named`] for each place `param` is written as
/// an argument of a definition, found by name resolution alone, with nothing lowered.
struct Header<'c, 'db, 'a, 'f> {
    ctx: &'c TyLoweringContext<'db, 'a>,
    param: TypeParamId,
    named: &'c mut Vec<Named, &'f Frame>,
}

impl<'a> Header<'_, '_, 'a, '_> {
    /// The header of `owner`, the owner the context is set to: a function's parameter and
    /// return types, an impl's self type and trait, a trait's supertraits.
    fn of(&mut self, owner: GenericDefId) {
        let db = self.ctx.db;
        match owner {
            GenericDefId::FunctionId(function) => {
                let sig = FunctionSignature::of(db, function);
                for &ty in sig.params.iter().chain(&sig.ret_type) {
                    self.ty(ty);
                }
            }
            GenericDefId::ImplId(impl_) => {
                let sig = ImplSignature::of(db, impl_);
                self.ty(sig.self_ty);
                if let Some(WrittenTraitRef { path }) = sig.target_trait {
                    self.trait_path(path, Written::ImplTrait(path, sig.self_ty));
                }
            }
            GenericDefId::TraitId(trait_) => {
                let store = self.ctx.store;
                for pred in GenericParams::of(db, owner).where_predicates() {
                    let WherePredicate::TypeBound {
                        lifetimes: _,
                        binder: BinderParams { len: 0, .. },
                        target,
                        bound,
                    } = pred
                    else {
                        continue;
                    };
                    let (TypeBound::Path(
                        path,
                        TraitBoundModifier::None
                        | TraitBoundModifier::Const
                        | TraitBoundModifier::MaybeConst,
                    )
                    | TypeBound::ForLifetime(_, path)) = *bound
                    else {
                        continue;
                    };
                    let on_self = match store.type_ref(*target) {
                        TypeRef::TypeParam(it) => {
                            it.local_id() == GenericParams::SELF_PARAM_ID_IN_SELF
                        }
                        TypeRef::Path(it) => it.is_self_type(),
                        _ => false,
                    };
                    if on_self {
                        self.trait_path(path, Written::Supertrait(path, trait_));
                    }
                }
            }
            _ => {}
        }
    }

    /// Every definition `ty` names, at every depth, as [`Collector::ty`] walks the lowered
    /// type. An `impl Trait` is passed over as it is there: an argument's is a parameter, a
    /// return type's an opaque type, neither a type whose definition gives bounds.
    fn ty(&mut self, ty: TypeRefId) {
        let store = self.ctx.store;
        match store.type_ref(ty) {
            TypeRef::Path(path) => self.path(ty, path),
            TypeRef::Tuple(tys) | TypeRef::Union(tys) | TypeRef::Record { types: tys, .. } => {
                for &it in tys {
                    self.ty(it);
                }
            }
            TypeRef::RawPtr(it, _)
            | TypeRef::Slice(it)
            | TypeRef::PatternType(it, _)
            | TypeRef::UnsafeBinder { ty: it, .. } => self.ty(it),
            TypeRef::Reference(it) => self.ty(it.ty),
            TypeRef::Array(it) => self.ty(it.ty),
            TypeRef::Fn(it) => {
                for &(_, it) in it.params.iter() {
                    self.ty(it);
                }
            }
            TypeRef::DynTrait(bounds) => {
                for bound in bounds {
                    if let TypeBound::Path(path, _) | TypeBound::ForLifetime(_, path) = *bound {
                        self.trait_path(path, Written::Type(ty));
                    }
                }
            }
            TypeRef::ImplTrait(_)
            | TypeRef::Never
            | TypeRef::Placeholder
            | TypeRef::TypeParam(_)
            | TypeRef::Error => {}
        }
    }

    /// A type's path: the types in its arguments, then the struct, enum or union it names, or
    /// the trait of a qualified `<X as Tr<..>>::A` (`Tr<Self = X>::A`). A shorthand `P::A` is
    /// passed over: see [`shorthand_bounds`].
    fn path(&mut self, ty: TypeRefId, path: Path<'a>) {
        self.nested(path);
        let ctx = self.ctx;
        let hygiene = ctx.store.type_path_hygiene(ty);
        let Some((resolution, remaining, ..)) =
            ctx.resolver.resolve_path_in_type_ns_hygienic(ctx.db, path, hygiene)
        else {
            return;
        };
        let segments = path.segments();
        match (resolution, remaining) {
            (TypeNs::AdtId(adt), None) => {
                if let Some(args) = segments.last().and_then(|it| it.args_and_bindings) {
                    self.named(GenericDefId::AdtId(adt), args, Written::Type(ty));
                }
            }
            (TypeNs::TraitId(trait_), Some(at)) if at > 0 && at + 1 == segments.len() => {
                if let Some(args) = segments.get(at - 1).and_then(|it| it.args_and_bindings) {
                    self.named(GenericDefId::TraitId(trait_), args, Written::Type(ty));
                }
            }
            _ => {}
        }
    }

    /// A trait path of a bound: the types in its arguments, then the trait.
    fn trait_path(&mut self, path: PathId, at: Written) {
        let ctx = self.ctx;
        let written = ctx.store.path(path);
        self.nested(written);
        let hygiene = ctx.store.type_path_hygiene(path.type_ref());
        let Some((TypeNs::TraitId(trait_), None, ..)) =
            ctx.resolver.resolve_path_in_type_ns_hygienic(ctx.db, written, hygiene)
        else {
            return;
        };
        if let Some(args) = written.segments().last().and_then(|it| it.args_and_bindings) {
            self.named(GenericDefId::TraitId(trait_), args, at);
        }
    }

    /// The types written in `path`'s arguments and associated type bindings, and its anchor.
    fn nested(&mut self, path: Path<'a>) {
        if let Some(anchor) = path.type_anchor() {
            self.ty(anchor);
        }
        for segment in path.segments().iter() {
            let Some(args) = segment.args_and_bindings else { continue };
            for arg in args.args {
                if let WrittenArg::Type(it) = *arg {
                    self.ty(it);
                }
            }
            for binding in args.bindings.iter() {
                if let Some(it) = binding.type_ref {
                    self.ty(it);
                }
            }
        }
    }

    /// `def` named with `args`: a [`Named`] for each argument that is the parameter, by the
    /// position it is written at (lifetimes apart, and a qualified path's `Self` first).
    fn named(&mut self, def: GenericDefId, args: WrittenArgs<'_>, at: Written) {
        let written = args
            .args
            .iter()
            .skip(usize::from(args.has_self_type))
            .filter(|it| !matches!(it, WrittenArg::Lifetime(_)));
        for (position, arg) in written.enumerate() {
            let WrittenArg::Type(arg) = *arg else { continue };
            if self.ctx.lower_ty_only_param(arg) != Some(self.param.into()) {
                continue;
            }
            if let Some(as_param) = param_at(self.ctx.db, def, position) {
                self.named.push(Named { def, as_param, at });
            }
        }
    }
}

/// The type parameter of `def` that the `position`th written type or const argument is for,
/// a trait's `Self` not counted; `None` for a const parameter or past the last.
fn param_at(db: &Db, def: GenericDefId, position: usize) -> Option<TypeParamId> {
    let (local_id, data) = GenericParams::of(db, def)
        .iter_type_or_consts()
        .filter(|(_, it)| {
            !it.type_param().is_some_and(|it| it.provenance == TypeParamProvenance::TraitSelf)
        })
        .nth(position)?;
    data.type_param()?;
    Some(TypeParamId::from_unchecked(TypeOrConstParamId { parent: def, local_id }))
}
