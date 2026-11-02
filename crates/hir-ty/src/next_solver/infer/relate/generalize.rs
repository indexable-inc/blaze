//! Type generation code.

use std::hash::Hash;
use std::mem;

use arrayvec::ArrayVec;
use blaze_hash::HashMap;
use rustc_type_ir::error::TypeError;
use rustc_type_ir::inherent::{IntoKind, Term as _, Ty as _};
use rustc_type_ir::relate::VarianceDiagInfo;
use rustc_type_ir::{
    ConstVid, InferConst, InferTy, Interner, IsRigid, RegionKind, TermKind, TyVid, TypeFlags,
    TypeVisitableExt, UniverseIndex, Variance, max_universe, max_universe_of_placeholders,
};
use tracing::{debug, instrument, warn};

use super::{PredicateEmittingRelation, Relate, RelateResult, TypeRelation};
use crate::next_solver::infer::type_variable::TypeVariableValue;
use crate::next_solver::infer::unify_key::ConstVariableValue;
use crate::next_solver::infer::{InferCtxt, relate};
use crate::next_solver::{
    AliasTerm, Binder, Const, ConstKind, DbInterner, GenericArgs, Region, SolverDefId, Term, Ty,
    TyKind,
};
use crate::Span;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum TermVid {
    Ty(TyVid),
    Const(ConstVid),
}

impl From<TyVid> for TermVid {
    fn from(value: TyVid) -> Self {
        TermVid::Ty(value)
    }
}

impl From<ConstVid> for TermVid {
    fn from(value: ConstVid) -> Self {
        TermVid::Const(value)
    }
}

impl<'db> InferCtxt<'db> {
    /// The idea is that we should ensure that the type variable `target_vid`
    /// is equal to, a subtype of, or a supertype of `source_ty`.
    ///
    /// For this, we will instantiate `target_vid` with a *generalized* version
    /// of `source_ty`. Generalization introduces other inference variables wherever
    /// subtyping could occur. This also does the occurs checks, detecting whether
    /// instantiating `target_vid` would result in a cyclic type. We eagerly error
    /// in this case.
    ///
    /// This is *not* expected to be used anywhere except for an implementation of
    /// `TypeRelation`. Do not use this, and instead please use `At::eq`, for all
    /// other usecases (i.e. setting the value of a type var).
    #[instrument(level = "debug", skip(self, relation))]
    pub fn instantiate_ty_var<R: PredicateEmittingRelation<InferCtxt<'db>>>(
        &self,
        relation: &mut R,
        target_is_expected: bool,
        target_vid: TyVid,
        instantiation_variance: Variance,
        source_ty: Ty<'db>,
    ) -> RelateResult<'db, ()> {
        debug_assert!(self.inner.borrow_mut().type_variables().probe(target_vid).is_unknown());

        self.instantiate_var(
            relation,
            target_is_expected,
            target_vid.into(),
            instantiation_variance,
            source_ty.into(),
        )
    }

    /// Instantiates the const variable `target_vid` with the given constant.
    ///
    /// This also tests if the given const `ct` contains an inference variable which was previously
    /// unioned with `target_vid`. If this is the case, inferring `target_vid` to `ct`
    /// would result in an infinite type as we continuously replace an inference variable
    /// in `ct` with `ct` itself.
    ///
    /// This is especially important as alias consts use their parents generics.
    /// They therefore often contain unused args, making these errors far more likely.
    ///
    /// A good example of this is the following:
    ///
    /// ```compile_fail,E0308
    /// #![feature(generic_const_exprs)]
    ///
    /// fn bind<const N: usize>(value: [u8; N]) -> [u8; 3 + 4] {
    ///     loop {}
    /// }
    ///
    /// fn main() {
    ///     let mut arr = Default::default();
    ///     arr = bind(arr);
    /// }
    /// ```
    ///
    /// Here `3 + 4` ends up as `ConstKind::Alias` which uses the generics
    /// of `fn bind` (meaning that its args contain `N`).
    ///
    /// `bind(arr)` now infers that the type of `arr` must be `[u8; N]`.
    /// The assignment `arr = bind(arr)` now tries to equate `N` with `3 + 4`.
    ///
    /// As `3 + 4` contains `N` in its args, this must not succeed.
    ///
    /// See `tests/ui/const-generics/occurs-check/` for more examples where this is relevant.
    #[instrument(level = "debug", skip(self, relation))]
    pub(crate) fn instantiate_const_var<R: PredicateEmittingRelation<InferCtxt<'db>>>(
        &self,
        relation: &mut R,
        target_is_expected: bool,
        target_vid: ConstVid,
        source_ct: Const<'db>,
    ) -> RelateResult<'db, ()> {
        // FIXME(generic_const_exprs): Occurs check failures for alias consts
        // and generic expressions are not yet handled correctly.
        debug_assert!(
            self.inner
                .borrow_mut()
                .const_unification_table()
                .probe_value(target_vid)
                .known()
                .is_none()
        );

        self.instantiate_var(
            relation,
            target_is_expected,
            target_vid.into(),
            Variance::Invariant,
            source_ct.into(),
        )
    }

    fn instantiate_var<R: PredicateEmittingRelation<InferCtxt<'db>>>(
        &self,
        relation: &mut R,
        target_is_expected: bool,
        target_vid: TermVid,
        instantiation_variance: Variance,
        source_term: Term<'db>,
    ) -> RelateResult<'db, ()> {
        // Generalize `source_term` depending on the current variance. As an example, assume
        // `?target <: &'x ?1`, where `'x` is some free region and `?1` is an inference
        // variable.
        //
        // Then the `generalized_term` would be `&'?2 ?3`, where `'?2` and `?3` are fresh
        // region/type inference variables.
        //
        // We then relate `generalized_term <: source_term`, adding constraints like `'x: '?2` and
        // `?1 <: ?3`.
        let Generalization { value_may_be_infer: generalized_term } =
            self.generalize(relation.span(), target_vid, instantiation_variance, source_term)?;

        // Constrain `b_vid` to the generalized type `generalized_term`.
        self.union_var_term(target_vid, generalized_term);

        // Finally, relate `generalized_term` to `source_term`, as described in previous comment.
        //
        // FIXME(#16847): This code is non-ideal because all these subtype
        // relations wind up attributed to the same spans. We need
        // to associate causes/spans with each of the relations in
        // the stack to get this right.
        if generalized_term.is_infer() {
            // This happens for cases like `<?0 as Trait>::Assoc == ?0`, which the
            // relations (`super_combine_tys`, `super_combine_consts` and the solver's
            // own relating) resolve before they generalize.
            let Some(source_alias) = source_term.to_alias_term() else {
                panic!("generalized `{source_term:?}` to infer, not an alias");
            };
            panic!("nonrigid aliases should be handled in relations, not here: {source_alias:?}");
        }

        // NOTE: The `instantiation_variance` is not the same variance as
        // used by the relation. When instantiating `b`, `target_is_expected`
        // is flipped and the `instantiation_variance` is also flipped. To
        // constrain the `generalized_term` while using the original relation,
        // we therefore only have to flip the arguments.
        //
        // ```ignore (not code)
        // ?a rel B
        // instantiate_ty_var(?a, B) # expected and variance not flipped
        // B' rel B
        // ```
        // or
        // ```ignore (not code)
        // A rel ?b
        // instantiate_ty_var(?b, A) # expected and variance flipped
        // A rel A'
        // ```
        match generalized_term.kind() {
            TermKind::Ty(_) => {
                if target_is_expected {
                    relation.relate(generalized_term, source_term)?;
                } else {
                    debug!("flip relation");
                    relation.relate(source_term, generalized_term)?;
                }
            }
            TermKind::Const(_) => {
                // Override consts to always be invariant
                if target_is_expected {
                    relation.relate_with_variance(
                        Variance::Invariant,
                        VarianceDiagInfo::default(),
                        generalized_term,
                        source_term,
                    )?;
                } else {
                    relation.relate_with_variance(
                        Variance::Invariant,
                        VarianceDiagInfo::default(),
                        source_term,
                        generalized_term,
                    )?;
                }
            }
        }

        Ok(())
    }

    /// This is a thin wrapper around inserting into the var tables. You probably want
    /// [`Self::instantiate_var`] instead, which calls this method.
    fn union_var_term(&self, l: TermVid, r: Term<'db>) {
        match (l, r.kind()) {
            (TermVid::Ty(l), TermKind::Ty(r)) => {
                if let Some(r) = r.ty_vid() {
                    self.inner.borrow_mut().type_variables().equate(l, r)
                } else {
                    // Ideally, we put this assert into `type_variables().instantiate()`.
                    // But we can't pass the infcx into it as the infcx is already
                    // mutably borrowed.
                    debug_assert!(
                        self.try_resolve_ty_var(l).unwrap_err().can_name(max_universe(self, r))
                    );
                    self.inner.borrow_mut().type_variables().instantiate(l, r)
                }
            }
            (TermVid::Const(l), TermKind::Const(r)) => {
                if let Some(r) = r.ct_vid() {
                    self.inner.borrow_mut().const_unification_table().union(l, r)
                } else {
                    debug_assert!(
                        self.try_resolve_const_var(l).unwrap_err().can_name(max_universe(self, r))
                    );
                    self.inner
                        .borrow_mut()
                        .const_unification_table()
                        .union_value(l, ConstVariableValue::Known { value: r })
                }
            }
            _ => panic!("mismatched term kinds in generalize: {l:?}, {r:?}"),
        }
    }

    /// Attempts to generalize `source_term` for the type variable `target_vid`.
    /// This checks for cycles -- that is, whether `source_term` references `target_vid`.
    fn generalize(
        &self,
        span: Span,
        target_vid: TermVid,
        ambient_variance: Variance,
        source_term: Term<'db>,
    ) -> RelateResult<'db, Generalization<Term<'db>>> {
        assert!(!source_term.has_escaping_bound_vars());
        let (for_universe, root_vid) = match target_vid {
            TermVid::Ty(ty_vid) => {
                (self.try_resolve_ty_var(ty_vid).unwrap_err(), TermVid::Ty(self.root_var(ty_vid)))
            }
            TermVid::Const(ct_vid) => (
                self.try_resolve_const_var(ct_vid).unwrap_err(),
                TermVid::Const(self.inner.borrow_mut().const_unification_table().find(ct_vid)),
            ),
        };

        let mut generalizer = Generalizer {
            infcx: self,
            span,
            root_vid,
            for_universe,
            root_term: source_term,
            ambient_variance,
            in_alias: false,
            cache: Default::default(),
        };

        let value_may_be_infer = generalizer.relate(source_term, source_term)?;
        Ok(Generalization { value_may_be_infer })
    }
}

/// The "generalizer" is used when handling inference variables.
///
/// The basic strategy for handling a constraint like `?A <: B` is to
/// apply a "generalization strategy" to the term `B` -- this replaces
/// all the lifetimes in the term `B` with fresh inference variables.
/// (You can read more about the strategy in this [blog post].)
///
/// As an example, if we had `?A <: &'x u32`, we would generalize `&'x
/// u32` to `&'0 u32` where `'0` is a fresh variable. This becomes the
/// value of `A`. Finally, we relate `&'0 u32 <: &'x u32`, which
/// establishes `'0: 'x` as a constraint.
///
/// [blog post]: https://is.gd/0hKvIr
struct Generalizer<'me, 'db> {
    infcx: &'me InferCtxt<'db>,

    span: Span,

    /// The vid of the type variable that is in the process of being
    /// instantiated. If we find this within the value we are folding,
    /// that means we would have created a cyclic value.
    root_vid: TermVid,

    /// The universe of the type variable that is in the process of being
    /// instantiated. If we find anything that this universe cannot name,
    /// we reject the relation.
    for_universe: UniverseIndex,

    /// The root term (const or type) we're generalizing. Used for cycle errors.
    root_term: Term<'db>,

    /// After we generalize this type, we are going to relate it to
    /// some other type. What will be the variance at this point?
    ambient_variance: Variance,

    /// This is set once we're generalizing the arguments of an alias.
    ///
    /// This is necessary to correctly handle
    /// `<T as Bar<<?0 as Foo>::Assoc>::Assoc == ?0`. This equality can
    /// hold by either normalizing the outer or the inner associated type.
    in_alias: bool,

    cache: GeneralizeCache<(Ty<'db>, Variance, bool), Ty<'db>>,
}

const CACHE_INLINE: usize = 8;

/// Generalized types already computed in one generalization, keyed by the source type, the
/// ambient variance and `in_alias`. A generalization touches few distinct types, so the
/// first `CACHE_INLINE` entries live in place and nothing is allocated unless it spills.
pub(in crate::next_solver::infer) struct GeneralizeCache<K, V> {
    inline: ArrayVec<(K, V), CACHE_INLINE>,
    spill: HashMap<K, V>,
}

impl<K, V> Default for GeneralizeCache<K, V> {
    fn default() -> Self {
        GeneralizeCache { inline: ArrayVec::new(), spill: HashMap::default() }
    }
}

impl<K: Eq + Hash, V: Copy> GeneralizeCache<K, V> {
    #[inline]
    pub(in crate::next_solver::infer) fn get(&self, key: &K) -> Option<V> {
        if let Some((_, v)) = self.inline.iter().find(|(k, _)| k == key) {
            return Some(*v);
        }
        if self.spill.is_empty() { None } else { self.spill.get(key).copied() }
    }

    #[inline]
    pub(in crate::next_solver::infer) fn insert(&mut self, key: K, value: V) {
        if let Err(full) = self.inline.try_push((key, value)) {
            let (key, value) = full.element();
            self.spill.insert(key, value);
        }
    }
}

/// Do we need to generalize this type?
const NEEDS_GENERALIZATION: TypeFlags = TypeFlags::from_bits(
    TypeFlags::HAS_FREE_REGIONS.bits()
        | TypeFlags::HAS_INFER.bits()
        | TypeFlags::HAS_PLACEHOLDER.bits()
        | TypeFlags::HAS_NON_RIGID_ALIAS.bits(),
)
.unwrap();

impl<'db> Generalizer<'_, 'db> {
    /// Create an error that corresponds to the term kind in `root_term`
    fn cyclic_term_error(&self) -> TypeError<DbInterner<'db>> {
        match self.root_term.kind() {
            TermKind::Ty(ty) => TypeError::CyclicTy(ty),
            TermKind::Const(ct) => TypeError::CyclicConst(ct),
        }
    }

    /// Create a new type variable in the universe of the target when
    /// generalizing an alias.
    fn next_var_for_alias_of_kind(&self, alias: AliasTerm<'db>) -> Term<'db> {
        if alias.kind.is_type() {
            self.infcx.next_ty_var_in_universe(self.for_universe, self.span).into()
        } else {
            self.infcx.next_const_var_in_universe(self.for_universe, self.span).into()
        }
    }

    /// We only handle potentially normalizable aliases via this method. For rigid alias,
    /// we always generalize structurally.
    ///
    /// An occurs check failure inside of an alias does not mean
    /// that the types definitely don't unify. We may be able
    /// to normalize the alias after all.
    ///
    /// We handle this by lazily equating the normalizable alias
    /// and generalizing it to an inference variable. We always
    /// generalize to an infer var unless the alias contains escaping
    /// bound variables.
    ///
    /// Correctly handling aliases with escaping bound variables is
    /// difficult and currently incomplete in two opposite ways:
    /// - if we get an occurs check failure in the alias, replace it with a new infer var.
    ///   This causes us to later emit an alias-relate goal and is incomplete in case the
    ///   alias normalizes to type containing one of the bound variables.
    /// - if the alias contains an inference variable not nameable by `for_universe`, we
    ///   continue generalizing the alias. This ends up pulling down the universe of the
    ///   inference variable and is incomplete in case the alias would normalize to a type
    ///   which does not mention that inference variable.
    fn generalize_alias_term(
        &mut self,
        alias: AliasTerm<'db>,
    ) -> Result<Term<'db>, TypeError<DbInterner<'db>>> {
        // We do not eagerly replace aliases with inference variables if they have
        // escaping bound vars, see the method comment for details. However, when we
        // are inside of an alias with escaping bound vars replacing nested aliases
        // with inference variables can cause incorrect ambiguity.
        //
        // cc trait-system-refactor-initiative#110
        if !alias.has_escaping_bound_vars() && !self.in_alias {
            return Ok(self.next_var_for_alias_of_kind(alias));
        }

        let is_nested_alias = mem::replace(&mut self.in_alias, true);
        let result = match self.relate(alias, alias) {
            Ok(alias) => Ok(alias.to_term(self.cx(), IsRigid::No)),
            Err(e) => {
                if is_nested_alias {
                    return Err(e);
                } else {
                    let alias_max_universe = max_universe_of_placeholders(self.infcx, alias);
                    let infer_replacement_is_complete =
                        self.for_universe.can_name(alias_max_universe)
                            && !alias.has_escaping_bound_vars();
                    if !infer_replacement_is_complete {
                        warn!("may incompletely handle alias type: {alias:?}");
                    }

                    debug!("generalization failure in alias");
                    Ok(self.next_var_for_alias_of_kind(alias))
                }
            }
        };
        self.in_alias = is_nested_alias;
        result
    }
}

impl<'db> TypeRelation<DbInterner<'db>> for Generalizer<'_, 'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.infcx.interner
    }

    fn relate_ty_args(
        &mut self,
        a_ty: Ty<'db>,
        _: Ty<'db>,
        def_id: SolverDefId<'db>,
        a_args: GenericArgs<'db>,
        b_args: GenericArgs<'db>,
        mk: impl FnOnce(GenericArgs<'db>) -> Ty<'db>,
    ) -> RelateResult<'db, Ty<'db>> {
        let args = if self.ambient_variance == Variance::Invariant {
            // Avoid fetching the variance if we are in an invariant
            // context; no need, and it can induce dependency cycles
            // (e.g., #41849).
            relate::relate_args_invariantly(self, a_args, b_args)
        } else {
            let interner = self.cx();
            let variances = interner.variances_of(def_id);
            relate::relate_args_with_variances(self, variances, a_args, b_args)
        }?;
        if args == a_args { Ok(a_ty) } else { Ok(mk(args)) }
    }

    fn relate_with_variance<T: Relate<DbInterner<'db>>>(
        &mut self,
        variance: Variance,
        _info: VarianceDiagInfo<DbInterner<'db>>,
        a: T,
        b: T,
    ) -> RelateResult<'db, T> {
        let old_ambient_variance = self.ambient_variance;
        self.ambient_variance = self.ambient_variance.xform(variance);
        debug!(?self.ambient_variance, "new ambient variance");
        let r = self.relate(a, b);
        self.ambient_variance = old_ambient_variance;
        r
    }

    fn tys(&mut self, t: Ty<'db>, t2: Ty<'db>) -> RelateResult<'db, Ty<'db>> {
        assert_eq!(t, t2); // we are misusing TypeRelation here; both LHS and RHS ought to be ==

        if !t.has_type_flags(NEEDS_GENERALIZATION) {
            return Ok(t);
        }

        if let Some(result) = self.cache.get(&(t, self.ambient_variance, self.in_alias)) {
            return Ok(result);
        }

        // Check to see whether the type we are generalizing references
        // any other type variable related to `vid` via
        // subtyping. This is basically our "occurs check", preventing
        // us from creating infinitely sized types.
        let g = match t.kind() {
            TyKind::Infer(
                InferTy::FreshTy(_) | InferTy::FreshIntTy(_) | InferTy::FreshFloatTy(_),
            ) => {
                panic!("unexpected infer type: {t:?}")
            }

            TyKind::Infer(InferTy::TyVar(vid)) => {
                let mut inner = self.infcx.inner.borrow_mut();
                let vid = inner.type_variables().root_var(vid);
                if TermVid::Ty(vid) == self.root_vid {
                    // If sub-roots are equal, then `root_vid` and
                    // `vid` are related via subtyping.
                    Err(self.cyclic_term_error())
                } else {
                    let probe = inner.type_variables().probe(vid);
                    match probe {
                        TypeVariableValue::Known { value: u } => {
                            drop(inner);
                            self.relate(u, u)
                        }
                        TypeVariableValue::Unknown { universe } => {
                            match self.ambient_variance {
                                // Invariant: no need to make a fresh type variable
                                // if we can name the universe.
                                Variance::Invariant => {
                                    if self.for_universe.can_name(universe) {
                                        return Ok(t);
                                    }
                                }

                                // We do need a fresh type variable otherwise.
                                Variance::Bivariant
                                | Variance::Covariant
                                | Variance::Contravariant => (),
                            }

                            // rustc's `var_origin`: the new variable stands for the same
                            // parameter, which "cannot infer type of `T`" names.
                            let origin = inner.type_variables().var_span(vid);
                            let param = inner.type_variable_storage.param(vid);
                            let new_var_id = inner.type_variables().new_var_for_param(
                                self.for_universe,
                                origin,
                                param,
                            );
                            // Record that `vid` and `new_var_id` have to be subtypes
                            // of each other. This is currently only used for diagnostics.
                            // To see why, see the docs in the `type_variables` module.
                            inner.type_variables().sub_unify(vid, new_var_id);
                            // If we create a new inference variable inside of an alias we
                            // eagerly constrain that inference variable to prevent unexpected
                            // ambiguity errors.
                            //
                            // This is incomplete as it pulls down the universe of the
                            // original inference variable, even though the alias could
                            // normalize to a type which does not refer to that type at
                            // all. I don't expect this to cause unexpected errors in
                            // practice.
                            //
                            // We only need to do so for type and const variables, as
                            // region variables do not impact normalization, and will get
                            // correctly constrained by `AliasRelate` later on.
                            //
                            // cc trait-system-refactor-initiative#108
                            if !self.infcx.typing_mode_raw().is_coherence() && self.in_alias {
                                inner.type_variables().equate(vid, new_var_id);
                            }

                            debug!("replacing original vid={:?} with new={:?}", vid, new_var_id);
                            Ok(Ty::new_var(self.cx(), new_var_id))
                        }
                    }
                }
            }

            TyKind::Infer(InferTy::IntVar(_) | InferTy::FloatVar(_)) => {
                // No matter what mode we are in,
                // integer/floating-point types must be equal to be
                // relatable.
                Ok(t)
            }

            TyKind::Placeholder(placeholder) => {
                if self.for_universe.can_name(placeholder.universe) {
                    Ok(t)
                } else {
                    debug!(
                        "root universe {:?} cannot name placeholder in universe {:?}",
                        self.for_universe, placeholder.universe
                    );
                    Err(TypeError::Mismatch)
                }
            }

            // We only need to be careful with potentially normalizeable
            // aliases here. See `generalize_alias_term` for more information.
            TyKind::Alias(IsRigid::No, data) => {
                self.generalize_alias_term(data.into()).map(|v| v.expect_ty())
            }

            _ => relate::structurally_relate_tys(self, t, t),
        }?;

        self.cache.insert((t, self.ambient_variance, self.in_alias), g);
        Ok(g)
    }

    fn regions(&mut self, r: Region<'db>, r2: Region<'db>) -> RelateResult<'db, Region<'db>> {
        assert_eq!(r, r2); // we are misusing TypeRelation here; both LHS and RHS ought to be ==

        match r.kind() {
            // Never make variables for regions bound within the type itself,
            // nor for erased regions.
            RegionKind::ReBound(..) | RegionKind::ReErased => {
                return Ok(r);
            }

            // It doesn't really matter for correctness if we generalize ReError,
            // since we're already on a doomed compilation path.
            RegionKind::ReError(_) => {
                return Ok(r);
            }

            RegionKind::RePlaceholder(..)
            | RegionKind::ReVar(..)
            | RegionKind::ReStatic
            | RegionKind::ReEarlyParam(..)
            | RegionKind::ReLateParam(..) => {
                // see common code below
            }
        }

        // If we are in an invariant context, we can re-use the region
        // as is, unless it happens to be in some universe that we
        // can't name.
        if let Variance::Invariant = self.ambient_variance {
            let r_universe = self.infcx.universe_of_region(r);
            if self.for_universe.can_name(r_universe) {
                return Ok(r);
            }
        }

        Ok(self.infcx.next_region_var_in_universe(self.for_universe, self.span))
    }

    fn consts(&mut self, c: Const<'db>, c2: Const<'db>) -> RelateResult<'db, Const<'db>> {
        assert_eq!(c, c2); // we are misusing TypeRelation here; both LHS and RHS ought to be ==

        match c.kind() {
            ConstKind::Infer(InferConst::Var(vid)) => {
                // If root const vids are equal, then `root_vid` and
                // `vid` are related and we'd be inferring an infinitely
                // deep const.
                if TermVid::Const(
                    self.infcx.inner.borrow_mut().const_unification_table().find(vid),
                ) == self.root_vid
                {
                    return Err(self.cyclic_term_error());
                }

                let mut inner = self.infcx.inner.borrow_mut();
                let variable_table = inner.const_unification_table();
                match variable_table.probe_value(vid) {
                    ConstVariableValue::Known { value: u } => {
                        drop(inner);
                        self.relate(u, u)
                    }
                    ConstVariableValue::Unknown { span, universe } => {
                        if self.for_universe.can_name(universe) {
                            Ok(c)
                        } else {
                            let new_var_id = variable_table.new_key(ConstVariableValue::Unknown {
                                span,
                                universe: self.for_universe,
                            });

                            // See the comment for type inference variables
                            // for more details.
                            if !self.infcx.typing_mode_raw().is_coherence() && self.in_alias {
                                variable_table.union(vid, new_var_id);
                            }
                            // rustc's origin, copied: the parameter `vid` was made for.
                            let params = &inner.const_var_params;
                            if let Ok(at) = params.binary_search_by_key(&vid, |&(vid, _)| vid) {
                                let param = params[at].1;
                                inner.const_var_params.push((new_var_id, param));
                            }
                            Ok(Const::new_var(self.cx(), new_var_id))
                        }
                    }
                }
            }
            // FIXME: Alias consts are also not rigid, so the current
            // approach of always relating them structurally is incomplete.
            //
            // We only need to be careful with potentially normalizeable
            // aliases here. See `generalize_alias_term` for more information.
            ConstKind::Alias(IsRigid::No, alias_const) => {
                self.generalize_alias_term(alias_const.into()).map(|v| v.expect_const())
            }
            ConstKind::Placeholder(placeholder) => {
                if self.for_universe.can_name(placeholder.universe) {
                    Ok(c)
                } else {
                    debug!(
                        "root universe {:?} cannot name placeholder in universe {:?}",
                        self.for_universe, placeholder.universe
                    );
                    Err(TypeError::Mismatch)
                }
            }
            _ => relate::structurally_relate_consts(self, c, c),
        }
    }

    fn binders<T>(
        &mut self,
        a: Binder<'db, T>,
        _: Binder<'db, T>,
    ) -> RelateResult<'db, Binder<'db, T>>
    where
        T: Relate<DbInterner<'db>>,
    {
        let result = self.relate(a.skip_binder(), a.skip_binder())?;
        Ok(a.rebind(result))
    }
}

/// Result from a generalization operation.
#[derive(Debug)]
struct Generalization<T> {
    /// When generalizing `<?0 as Trait>::Assoc` or
    /// `<T as Bar<<?0 as Foo>::Assoc>>::Assoc`
    /// for `?0` generalization returns an inference
    /// variable.
    ///
    /// This has to be handled with care as it can
    /// otherwise very easily result in infinite
    /// recursion.
    pub value_may_be_infer: T,
}

#[cfg(test)]
mod tests {
    use super::{CACHE_INLINE, GeneralizeCache};

    #[test]
    fn cache_finds_inline_and_spilled_entries() {
        let mut cache = GeneralizeCache::<(u32, bool), u32>::default();
        let n = CACHE_INLINE as u32 * 3;
        for i in 0..n {
            assert_eq!(cache.get(&(i, true)), None);
            cache.insert((i, true), i + 100);
        }
        assert_eq!(cache.inline.len(), CACHE_INLINE);
        assert_eq!(cache.spill.len(), n as usize - CACHE_INLINE);
        for i in 0..n {
            assert_eq!(cache.get(&(i, true)), Some(i + 100));
            assert_eq!(cache.get(&(i, false)), None);
        }
    }

    #[test]
    fn small_cache_does_not_allocate() {
        let mut cache = GeneralizeCache::<u32, u32>::default();
        blaze_alloc_count::assert_at_most(0, "generalize cache below the spill", || {
            for i in 0..CACHE_INLINE as u32 {
                assert_eq!(cache.get(&i), None);
                cache.insert(i, i);
                assert_eq!(cache.get(&i), Some(i));
            }
        });
        assert_eq!(cache.spill.capacity(), 0);
    }

    #[test]
    fn cache_hits_after_spill_do_not_allocate() {
        let mut cache = GeneralizeCache::<u32, u32>::default();
        let n = CACHE_INLINE as u32 * 4;
        for i in 0..n {
            cache.insert(i, i);
        }
        blaze_alloc_count::assert_at_most(0, "generalize cache hits", || {
            for i in 0..n {
                assert_eq!(cache.get(&i), Some(i));
            }
        });
    }
}
