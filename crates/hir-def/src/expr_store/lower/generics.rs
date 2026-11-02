//! Many kinds of items or constructs can have generic parameters: functions,
//! structs, impls, traits, etc. This module provides a common HIR for these
//! generic parameters. See also the `Generics` type and the `generics_of` query
//! in rustc.

use either::Either;
use hir_expand::name::{AsName, Name};
use intern::sym;
use la_arena::Arena;
use syntax::{
    AstPtr, ast as tag,
    view::{self as ast, HasName, HasTypeBounds},
};

use crate::{
    GenericDefId, TypeOrConstParamId, TypeParamId,
    expr_store::{
        ListMark, TypePtr,
        lower::{ExprCollector, LifetimeBoundScope, NamedLifetimeStore},
        path::{Names, UseArgs},
    },
    hir::{
        columns::len,
        generics::{
            ConstParamData, GenericParams, LifetimeBoundType, LifetimeParamData,
            TypeOrConstParamData, TypeParamData, TypeParamProvenance, WherePredicate,
        },
    },
    type_ref::{
        LifetimeRef, LifetimeRefId, TraitBoundModifier, TypeBound, TypeRef, TypeRefId, UseArgRef,
    },
};

/// How an `impl Trait` type is lowered where it is written. Its bounds are on the builder's
/// bound stack above the mark [`ImplTraitLowering::lower`] is given, and lowering consumes
/// them: the stack is back at the mark when it returns.
pub enum ImplTraitLowering<'l> {
    /// An opaque type of its bounds (`TypeRef::ImplTrait`): a return type, an alias.
    Opaque,
    /// Not allowed here (a field, a `let`, a path's generic argument in a body): kept as
    /// written, a `TypeRef::ImplTrait`, for the type lowering to refuse with E0562.
    Error,
    /// An argument's: a new type parameter of the generics being collected, its bounds that
    /// parameter's where predicates.
    Argument {
        type_or_consts: &'l mut Arena<TypeOrConstParamData>,
        where_predicates: &'l mut Vec<WherePredicate>,
        parent: GenericDefId,
    },
}

impl ImplTraitLowering<'_> {
    pub(crate) fn lower(
        &mut self,
        ec: &mut ExprCollector<'_>,
        ptr: TypePtr,
        bounds: ListMark,
    ) -> TypeRefId {
        match self {
            ImplTraitLowering::Opaque | ImplTraitLowering::Error => {
                let id = ec.with_lists(bounds, |ec, lists| {
                    ec.store.alloc_type_ref(TypeRef::ImplTrait(lists.bounds_since(bounds)))
                });
                ec.map_type_ref(id, ptr, false)
            }
            ImplTraitLowering::Argument { type_or_consts, where_predicates, parent } => {
                let param = TypeParamData {
                    name: None,
                    default: None,
                    provenance: TypeParamProvenance::ArgumentImplTrait,
                };
                let param_id = TypeRef::TypeParam(TypeParamId::from_unchecked(TypeOrConstParamId {
                    parent: *parent,
                    local_id: type_or_consts.alloc(param.into()),
                }));
                let type_ref = ec.alloc_type_ref(param_id, ptr);
                where_predicates.extend(ec.store.drain_bounds(bounds).map(|bound| {
                    WherePredicate::TypeBound { lifetimes: None, target: type_ref, bound }
                }));
                type_ref
            }
        }
    }
}

pub(crate) struct GenericParamsCollector {
    type_or_consts: Arena<TypeOrConstParamData>,
    lifetimes: Arena<LifetimeParamData>,
    where_predicates: Vec<WherePredicate>,
    parent: GenericDefId,
}

impl GenericParamsCollector {
    pub(crate) fn new(parent: GenericDefId) -> Self {
        Self {
            type_or_consts: Default::default(),
            lifetimes: Default::default(),
            where_predicates: Default::default(),
            parent,
        }
    }
    pub(crate) fn with_self_param(
        ec: &mut ExprCollector<'_>,
        parent: GenericDefId,
        bounds: Option<ast::TypeBoundList<'_>>,
    ) -> Self {
        let mut this = Self::new(parent);
        this.fill_self_param(ec, bounds);
        this
    }

    pub(crate) fn lower(
        &mut self,
        ec: &mut ExprCollector<'_>,
        generic_param_list: Option<ast::GenericParamList<'_>>,
        where_clause: Option<ast::WhereClause<'_>>,
    ) {
        if let Some(params) = generic_param_list {
            self.lower_param_list(ec, params)
        }
        if let Some(where_clause) = where_clause {
            self.lower_where_predicates(ec, where_clause)
        }
    }

    /// `count` fresh early-bound lifetime parameters after those collected: the lifetimes an
    /// impl's header leaves out, as rustc's resolver `fresh_lifetime` makes them, named
    /// [`LifetimeParamData::fresh_name`].
    pub(crate) fn push_fresh_lifetimes(&mut self, count: u32) {
        for index in 0..count {
            self.lifetimes.alloc(LifetimeParamData {
                name: LifetimeParamData::fresh_name(index),
                bound_type: LifetimeBoundType::EarlyBound,
            });
        }
    }

    pub(crate) fn collect_impl_trait<R>(
        &mut self,
        ec: &mut ExprCollector<'_>,
        cb: impl FnOnce(&mut ExprCollector<'_>, &mut ImplTraitLowering<'_>) -> R,
    ) -> R {
        cb(ec, &mut self.argument_impl_trait())
    }

    pub(crate) fn finish(self) -> GenericParams {
        let Self { mut lifetimes, mut type_or_consts, where_predicates, parent: _ } = self;
        let early_bound_lifetimes_len =
            lifetimes.iter().filter(|(_, lt)| lt.is_early_bound()).count();

        lifetimes.shrink_to_fit();
        type_or_consts.shrink_to_fit();
        GenericParams {
            early_bound_lifetimes_len,
            type_or_consts,
            lifetimes,
            where_predicates: where_predicates.into_boxed_slice(),
        }
    }

    fn lower_param_list(&mut self, ec: &mut ExprCollector<'_>, params: ast::GenericParamList<'_>) {
        for generic_param in params.generic_params() {
            let enabled = ec.check_cfg(generic_param);
            if !enabled {
                continue;
            }

            match generic_param {
                ast::GenericParam::TypeParam(type_param) => {
                    let name = type_param.name().map_or_else(Name::missing, |it| it.as_name());
                    let default = type_param.default_type().map(|it| {
                        ec.lower_type_ref(it, &mut ImplTraitLowering::Error)
                    });
                    let param = TypeParamData {
                        name: Some(name),
                        default,
                        provenance: TypeParamProvenance::TypeParamList,
                    };
                    let idx = self.type_or_consts.alloc(param.into());
                    let type_ref =
                        TypeRef::TypeParam(TypeParamId::from_unchecked(TypeOrConstParamId {
                            parent: self.parent,
                            local_id: idx,
                        }));
                    let type_ref = ec.alloc_type_ref_desugared(type_ref);
                    ec.with_lifetime_bound_scope(LifetimeBoundScope::WhereClause, |ec| {
                        self.lower_bounds(ec, type_param.type_bound_list(), Either::Left(type_ref))
                    });
                }
                ast::GenericParam::ConstParam(const_param) => {
                    let name = const_param.name().map_or_else(Name::missing, |it| it.as_name());
                    let ty = ec.lower_type_ref_opt(
                        const_param.ty(),
                        &mut ImplTraitLowering::Error,
                    );
                    let default = const_param.default_val().map(|it| ec.lower_const_arg(it));
                    let param = ConstParamData { name, ty, default };
                    self.type_or_consts.alloc(param.into());
                }
                ast::GenericParam::LifetimeParam(lifetime_param) => {
                    let lifetime = ec.lower_lifetime_ref_opt(lifetime_param.lifetime());
                    if let LifetimeRef::Named(name) = &ec.store.lifetimes[lifetime] {
                        let param = LifetimeParamData {
                            name: *name,
                            bound_type: LifetimeBoundType::EarlyBound,
                        };
                        let _idx = self.lifetimes.alloc(param);
                        ec.with_lifetime_bound_scope(LifetimeBoundScope::WhereClause, |ec| {
                            self.lower_bounds(
                                ec,
                                lifetime_param.type_bound_list(),
                                Either::Right(lifetime),
                            )
                        });
                    }
                }
            }
        }
    }

    fn lower_where_predicates(
        &mut self,
        ec: &mut ExprCollector<'_>,
        where_clause: ast::WhereClause<'_>,
    ) {
        ec.with_lifetime_bound_scope(LifetimeBoundScope::WhereClause, |ec| {
            for pred in where_clause.predicates() {
                let target = if let Some(type_ref) = pred.ty() {
                    Either::Left(
                        ec.lower_type_ref(type_ref, &mut ImplTraitLowering::Error),
                    )
                } else if let Some(lifetime) = pred.lifetime() {
                    Either::Right(ec.lower_lifetime_ref(lifetime))
                } else {
                    continue;
                };

                // Higher-Ranked Trait Bounds: one run of names, shared by the predicate's bounds.
                let lifetimes =
                    pred.for_binder().and_then(|it| it.generic_param_list()).map(|param_list| {
                        ec.store.paths_mut().push_names(param_list.lifetime_params().map(
                            |lifetime_param| {
                                lifetime_param
                                    .lifetime()
                                    .map_or_else(Name::missing, |lt| Name::new_lifetime(lt.text()))
                            },
                        ))
                    });
                for bound in pred.type_bound_list().iter().flat_map(|l| l.bounds()) {
                    self.lower_type_bound_as_predicate(ec, bound, lifetimes, target);
                }
            }
        });
    }

    fn lower_bounds(
        &mut self,
        ec: &mut ExprCollector<'_>,
        type_bounds: Option<ast::TypeBoundList<'_>>,
        target: Either<TypeRefId, LifetimeRefId>,
    ) {
        for bound in type_bounds.iter().flat_map(|type_bound_list| type_bound_list.bounds()) {
            self.lower_type_bound_as_predicate(ec, bound, None, target);
        }
    }

    fn lower_type_bound_as_predicate(
        &mut self,
        ec: &mut ExprCollector<'_>,
        bound: ast::TypeBound<'_>,
        hrtb_lifetimes: Option<Names>,
        target: Either<TypeRefId, LifetimeRefId>,
    ) {
        let bound = ec.lower_type_bound(bound, &mut self.argument_impl_trait());
        let predicate = match (target, bound) {
            (_, TypeBound::Error | TypeBound::Use(_)) => return,
            (Either::Left(type_ref), bound) => {
                WherePredicate::TypeBound { lifetimes: hrtb_lifetimes, target: type_ref, bound }
            }
            (Either::Right(lifetime), TypeBound::Lifetime(bound)) => {
                WherePredicate::Lifetime { target: lifetime, bound }
            }
            (Either::Right(_), TypeBound::ForLifetime(..) | TypeBound::Path(..)) => return,
        };
        if let WherePredicate::Lifetime { target, .. } = predicate {
            ec.push_named_target_lifetime(target);
        }
        self.where_predicates.push(predicate);
    }

    fn argument_impl_trait(&mut self) -> ImplTraitLowering<'_> {
        ImplTraitLowering::Argument {
            type_or_consts: &mut self.type_or_consts,
            where_predicates: &mut self.where_predicates,
            parent: self.parent,
        }
    }

    fn fill_self_param(
        &mut self,
        ec: &mut ExprCollector<'_>,
        bounds: Option<ast::TypeBoundList<'_>>,
    ) {
        let self_ = Name::new_symbol_root(sym::Self_);
        let idx = self.type_or_consts.alloc(
            TypeParamData {
                name: Some(self_),
                default: None,
                provenance: TypeParamProvenance::TraitSelf,
            }
            .into(),
        );
        debug_assert_eq!(idx, GenericParams::SELF_PARAM_ID_IN_SELF);
        let type_ref = TypeRef::TypeParam(TypeParamId::from_unchecked(TypeOrConstParamId {
            parent: self.parent,
            local_id: idx,
        }));
        let self_ = ec.alloc_type_ref_desugared(type_ref);
        if let Some(bounds) = bounds {
            self.lower_bounds(ec, Some(bounds), Either::Left(self_));
        }
    }

    pub(crate) fn update_to_late_bound_lifetimes(
        &mut self,
        named_lifetime_store: &NamedLifetimeStore,
    ) {
        for (_param_id, lifetime) in self.lifetimes.iter_mut() {
            let lifetime_name = &lifetime.name;
            if named_lifetime_store.lifetimes_in_where_clause.contains(lifetime_name) {
                continue;
            }

            if !named_lifetime_store.lifetimes_constrained_by_input.contains(lifetime_name)
                && named_lifetime_store.lifetimes_in_output.contains(lifetime_name)
            {
                continue;
            }

            lifetime.bound_type = LifetimeBoundType::LateBound
        }
    }
}

impl ExprCollector<'_> {
    /// A bound; its `for<..>` binder and `use<..>` arguments are pushed as runs into the store's
    /// paths, so the bound itself is a plain record.
    pub(super) fn lower_type_bound(
        &mut self,
        node: ast::TypeBound<'_>,
        impl_trait_lower_fn: &mut ImplTraitLowering<'_>,
    ) -> TypeBound {
        let Some(kind) = node.kind() else { return TypeBound::Error };
        match kind {
            ast::TypeBoundKind::PathType(binder, path_type) => {
                let binder = self.lower_bound_binder(binder);
                let m = if node.question_mark_token().is_some() {
                    TraitBoundModifier::Maybe
                } else if node.const_token().is_none() {
                    TraitBoundModifier::None
                } else if node.tilde_token().is_some() || node.l_brack_token().is_some() {
                    TraitBoundModifier::MaybeConst
                } else {
                    TraitBoundModifier::Const
                };
                let Some(path) =
                    path_type.path().and_then(|it| self.lower_path_view(it, impl_trait_lower_fn))
                else {
                    return TypeBound::Error;
                };
                let path = self.alloc_path(path, AstPtr::<tag::PathType>::of(path_type).upcast());
                if binder.is_empty() {
                    TypeBound::Path(path, m)
                } else {
                    TypeBound::ForLifetime(binder, path)
                }
            }
            ast::TypeBoundKind::Use(gal) => {
                // Lowering a lifetime pushes no `use<..>` argument, so the pushes are one run.
                let start = len(self.store.paths().use_args.len());
                for arg in gal.use_bound_generic_args() {
                    let arg = match arg {
                        ast::UseBoundGenericArg::Lifetime(l) => {
                            UseArgRef::Lifetime(self.lower_lifetime_ref(l))
                        }
                        ast::UseBoundGenericArg::NameRef(n) => UseArgRef::Name(n.as_name()),
                    };
                    self.store.paths_mut().use_args.push(arg);
                }
                let end = len(self.store.paths().use_args.len());
                TypeBound::Use(UseArgs { start, len: end - start })
            }
            ast::TypeBoundKind::Lifetime(lifetime) => {
                TypeBound::Lifetime(self.lower_lifetime_ref(lifetime))
            }
        }
    }

    /// The lifetimes of a bound's `for<..>`, as a run of names (empty without a binder).
    fn lower_bound_binder(&mut self, binder: Option<ast::ForBinder<'_>>) -> Names {
        let Some(params) = binder.and_then(|it| it.generic_param_list()) else {
            return Names::EMPTY;
        };
        self.store.paths_mut().push_names(
            params
                .lifetime_params()
                .flat_map(|lp| lp.lifetime().map(|lt| Name::new_lifetime(lt.text()))),
        )
    }
}
