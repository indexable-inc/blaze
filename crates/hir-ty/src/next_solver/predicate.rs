//! Things related to predicates.

use std::{cmp::Ordering, marker::PhantomData, num::NonZeroU32};

use macros::{GenericTypeVisitable, TypeFoldable, TypeVisitable};
use rustc_type_ir::{
    self as ty, ClausePolarity, CollectAndApply, EarlyBinder, FlagComputation, Flags,
    GenericTypeVisitable, PredicateProxy, TypeFoldable, TypeSuperFoldable, TypeSuperVisitable,
    TypeVisitable, Upcast, UpcastFrom,
    elaborate::Elaboratable,
    error::{ExpectedFound, TypeError},
    inherent::{IntoKind, SliceLike},
};
use smallvec::SmallVec;

use crate::next_solver::{
    GenericArg, TraitIdWrapper, default_types, impl_foldable_for_interned_slice,
    impl_foldable_for_stored_type, impl_stored_interned, impl_stored_interned_slice,
    interned_slice, interner::table,
    persist,
};

use super::{Binder, BoundVarKinds, DbInterner, Region, Ty};

pub type BoundExistentialPredicate<'db> = Binder<'db, ExistentialPredicate<'db>>;

pub type TraitRef<'db> = ty::TraitRef<DbInterner<'db>>;
pub type AliasTerm<'db> = ty::AliasTerm<DbInterner<'db>>;
pub type ProjectionClause<'db> = ty::ProjectionClause<DbInterner<'db>>;
pub type ExistentialPredicate<'db> = ty::ExistentialPredicate<DbInterner<'db>>;
pub type ExistentialTraitRef<'db> = ty::ExistentialTraitRef<DbInterner<'db>>;
pub type ExistentialProjection<'db> = ty::ExistentialProjection<DbInterner<'db>>;
pub type TraitClause<'db> = ty::TraitClause<DbInterner<'db>>;
pub type HostEffectClause<'db> = ty::HostEffectClause<DbInterner<'db>>;
pub type ClauseKind<'db> = ty::ClauseKind<DbInterner<'db>>;
pub type PredicateKind<'db> = ty::PredicateKind<DbInterner<'db>>;
pub type NormalizesTo<'db> = ty::NormalizesTo<DbInterner<'db>>;
pub type CoercePredicate<'db> = ty::CoercePredicate<DbInterner<'db>>;
pub type SubtypePredicate<'db> = ty::SubtypePredicate<DbInterner<'db>>;
pub type OutlivesClause<'db, T> = ty::OutlivesClause<DbInterner<'db>, T>;
pub type RegionOutlivesClause<'db> = OutlivesClause<'db, Region<'db>>;
pub type TypeOutlivesClause<'db> = OutlivesClause<'db, Ty<'db>>;
pub type PolyTraitClause<'db> = Binder<'db, TraitClause<'db>>;
pub type PolyRegionOutlivesClause<'db> = Binder<'db, RegionOutlivesClause<'db>>;
pub type PolyTypeOutlivesClause<'db> = Binder<'db, TypeOutlivesClause<'db>>;
pub type PolySubtypePredicate<'db> = Binder<'db, SubtypePredicate<'db>>;
pub type PolyCoercePredicate<'db> = Binder<'db, CoercePredicate<'db>>;
pub type PolyProjectionClause<'db> = Binder<'db, ProjectionClause<'db>>;
pub type PolyTraitRef<'db> = Binder<'db, TraitRef<'db>>;
pub type PolyExistentialTraitRef<'db> = Binder<'db, ExistentialTraitRef<'db>>;
pub type PolyExistentialProjection<'db> = Binder<'db, ExistentialProjection<'db>>;
pub type ArgOutlivesClause<'db> = OutlivesClause<'db, GenericArg<'db>>;

/// Compares via an ordering that will not change if modules are reordered or other changes are
/// made to the tree. In particular, this ordering is preserved across incremental compilations.
fn stable_cmp_existential_predicate<'db>(
    a: &ExistentialPredicate<'db>,
    b: &ExistentialPredicate<'db>,
) -> Ordering {
    // FIXME: this is actual unstable - see impl in predicate.rs in `rustc_middle`
    match (a, b) {
        (ExistentialPredicate::Trait(_), ExistentialPredicate::Trait(_)) => Ordering::Equal,
        (ExistentialPredicate::Projection(_a), ExistentialPredicate::Projection(_b)) => {
            // Should sort by def path hash
            Ordering::Equal
        }
        (ExistentialPredicate::AutoTrait(_a), ExistentialPredicate::AutoTrait(_b)) => {
            // Should sort by def path hash
            Ordering::Equal
        }
        (ExistentialPredicate::Trait(_), _) => Ordering::Less,
        (ExistentialPredicate::Projection(_), ExistentialPredicate::Trait(_)) => Ordering::Greater,
        (ExistentialPredicate::Projection(_), _) => Ordering::Less,
        (ExistentialPredicate::AutoTrait(_), _) => Ordering::Greater,
    }
}
interned_slice!(
    BoundExistentialPredicatesStorage,
    BoundExistentialPredicates,
    StoredBoundExistentialPredicates,
    bound_existential_predicates,
    BoundExistentialPredicate<'db>,
    BoundExistentialPredicate<'static>,
);
impl_foldable_for_interned_slice!(BoundExistentialPredicates);

impl<'db> rustc_type_ir::inherent::BoundExistentialPredicates<DbInterner<'db>>
    for BoundExistentialPredicates<'db>
{
    fn principal_def_id(self) -> Option<TraitIdWrapper> {
        self.principal().map(|trait_ref| trait_ref.skip_binder().def_id)
    }

    fn principal(
        self,
    ) -> Option<
        rustc_type_ir::Binder<DbInterner<'db>, rustc_type_ir::ExistentialTraitRef<DbInterner<'db>>>,
    > {
        self[0]
            .map_bound(|this| match this {
                ExistentialPredicate::Trait(tr) => Some(tr),
                _ => None,
            })
            .transpose()
    }

    fn auto_traits(self) -> impl IntoIterator<Item = TraitIdWrapper> {
        self.iter().filter_map(|predicate| match predicate.skip_binder() {
            ExistentialPredicate::AutoTrait(did) => Some(did),
            _ => None,
        })
    }

    fn projection_bounds(
        self,
    ) -> impl IntoIterator<
        Item = rustc_type_ir::Binder<
            DbInterner<'db>,
            rustc_type_ir::ExistentialProjection<DbInterner<'db>>,
        >,
    > {
        self.iter().filter_map(|predicate| {
            predicate
                .map_bound(|pred| match pred {
                    ExistentialPredicate::Projection(projection) => Some(projection),
                    _ => None,
                })
                .transpose()
        })
    }
}

impl<'db> rustc_type_ir::relate::Relate<DbInterner<'db>> for BoundExistentialPredicates<'db> {
    fn relate<R: rustc_type_ir::relate::TypeRelation<DbInterner<'db>>>(
        relation: &mut R,
        a: Self,
        b: Self,
    ) -> rustc_type_ir::relate::RelateResult<DbInterner<'db>, Self> {
        // Not deduplicated, as rustc no longer does since #136458: `dyn Trait<A, B>` with
        // `trait Trait<A, B>: Sup<A, Assoc = A> + Sup<B, Assoc = B>` keeps both projections,
        // which are one twice once `A` and `B` are inferred to be the same, and one is still
        // related to each of the other's.
        let mut a_v: SmallVec<[_; 8]> = SmallVec::from_iter(a);
        let mut b_v: SmallVec<[_; 8]> = SmallVec::from_iter(b);
        // `skip_binder` here is okay because `stable_cmp` doesn't look at binders
        a_v.sort_by(|a, b| {
            stable_cmp_existential_predicate(a.as_ref().skip_binder(), b.as_ref().skip_binder())
        });
        b_v.sort_by(|a, b| {
            stable_cmp_existential_predicate(a.as_ref().skip_binder(), b.as_ref().skip_binder())
        });
        if a_v.len() != b_v.len() {
            return Err(TypeError::ExistentialMismatch(ExpectedFound::new(a, b)));
        }

        let v = std::iter::zip(a_v, b_v).map(
            |(ep_a, ep_b): (
                Binder<'_, ty::ExistentialPredicate<_>>,
                Binder<'_, ty::ExistentialPredicate<_>>,
            )| {
                match (ep_a.skip_binder(), ep_b.skip_binder()) {
                    (ty::ExistentialPredicate::Trait(a), ty::ExistentialPredicate::Trait(b)) => {
                        Ok(ep_a.rebind(ty::ExistentialPredicate::Trait(
                            relation.relate(ep_a.rebind(a), ep_b.rebind(b))?.skip_binder(),
                        )))
                    }
                    (
                        ty::ExistentialPredicate::Projection(a),
                        ty::ExistentialPredicate::Projection(b),
                    ) => Ok(ep_a.rebind(ty::ExistentialPredicate::Projection(
                        relation.relate(ep_a.rebind(a), ep_b.rebind(b))?.skip_binder(),
                    ))),
                    (
                        ty::ExistentialPredicate::AutoTrait(a),
                        ty::ExistentialPredicate::AutoTrait(b),
                    ) if a == b => Ok(ep_a.rebind(ty::ExistentialPredicate::AutoTrait(a))),
                    _ => Err(TypeError::ExistentialMismatch(ExpectedFound::new(a, b))),
                }
            },
        );

        BoundExistentialPredicates::new_from_iter(v)
    }
}

/// What a [`Predicate`] is.
type PredicateRow<'db> = Binder<'db, PredicateKind<'db>>;

/// Every interned [`Predicate`].
static PREDICATES: table::Rows<PredicateRow<'static>> =
    table::Rows::new("predicates", table::LAST_ROW);
table::erase!(PredicateRow, Clause);

/// A predicate: its row in [`PREDICATES`], which is never freed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Predicate<'db> {
    index: NonZeroU32,
    _db: PhantomData<&'db ()>,
}

const _: () = assert!(size_of::<Option<Predicate<'static>>>() == 4);
// A row costs `table::ROW_BYTES` hot bytes and its kind, at most 48 cold ones.
const _: () = assert!(size_of::<Binder<'static, PredicateKind<'static>>>() <= 48);

impl<'db> Predicate<'db> {
    pub fn new(_interner: DbInterner<'db>, kind: Binder<'db, PredicateKind<'db>>) -> Self {
        Self::intern(kind)
    }

    #[inline]
    pub(super) fn intern(kind: Binder<'db, PredicateKind<'db>>) -> Self {
        Self::at(PREDICATES.intern(kind, || {
            let computed = FlagComputation::for_predicate(kind);
            table::RowInfo {
                tag: persist::tag(&kind),
                flags: computed.flags,
                binder: computed.outer_exclusive_binder,
            }
        }))
    }

    #[inline]
    pub(super) fn at(index: NonZeroU32) -> Self {
        Predicate { index, _db: PhantomData }
    }

    #[inline]
    pub(super) fn row(self) -> NonZeroU32 {
        self.index
    }

    /// Flips the polarity of a Predicate.
    ///
    /// Given `T: Trait` predicate it returns `T: !Trait` and given `T: !Trait` returns `T: Trait`.
    pub fn flip_polarity(self, interner: DbInterner<'db>) -> Option<Predicate<'db>> {
        let kind = self
            .kind()
            .map_bound(|kind| match kind {
                PredicateKind::Clause(ClauseKind::Trait(TraitClause { trait_ref, polarity })) => {
                    Some(PredicateKind::Clause(ClauseKind::Trait(TraitClause {
                        trait_ref,
                        polarity: polarity.flip(),
                    })))
                }

                _ => None,
            })
            .transpose()?;

        Some(Predicate::new(interner, kind))
    }

    /// `f` of the projection clause this is, interned; `None` for any other predicate.
    fn map_projection_clause(
        self,
        f: impl FnOnce(PolyProjectionClause<'db>) -> PolyProjectionClause<'db>,
    ) -> Option<Self> {
        let projection = self.as_projection_clause()?;
        Some(Predicate::intern(
            f(projection).map_bound(|it| PredicateKind::Clause(ClauseKind::Projection(it))),
        ))
    }
}

impl<'db> std::fmt::Debug for Predicate<'db> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.kind().fmt(f)
    }
}

/// Where every interned [`Clauses`] is: a list table with flags, for `Clauses` is `Flags`.
pub struct ClausesStorage;

impl ClausesStorage {
    #[inline]
    fn table() -> &'static table::Lists<Clause<'static>> {
        static TABLE: table::Lists<Clause<'static>> = table::Lists::with_flags("Clauses");
        &TABLE
    }
}

impl_stored_interned_slice!(Clauses, StoredClauses);
impl_foldable_for_stored_type!(StoredClauses);

impl_stored_interned!(Predicate, StoredPredicate);
impl_foldable_for_stored_type!(StoredPredicate);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Clauses<'db> {
    index: NonZeroU32,
    _db: PhantomData<&'db ()>,
}

const _: () = assert!(size_of::<Option<Clauses<'static>>>() == 4);

impl<'db> std::fmt::Debug for Clauses<'db> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_slice().fmt(fmt)
    }
}

impl<'db> Clauses<'db> {
    #[inline]
    pub fn empty() -> Self {
        default_types().empty.clauses
    }

    #[inline]
    pub fn new_from_slice(slice: &[Clause<'db>]) -> Self {
        if slice.is_empty() {
            // Common case: avoid looking up the empty slice.
            Self::empty()
        } else {
            Self::new_from_slice_no_empty_check(slice)
        }
    }

    /// Same as [`Self::new_from_slice()`] but won't use the global empty slice `slice.is_empty()`, because someone
    /// needs to intern the global slice as well.
    #[inline]
    pub(crate) fn new_from_slice_no_empty_check(slice: &[Clause<'db>]) -> Self {
        Self::at(ClausesStorage::table().intern_with(slice, || {
            let computed = FlagComputation::<DbInterner<'db>>::for_clauses(slice);
            (computed.flags, computed.outer_exclusive_binder)
        }))
    }

    /// The list's place in its table: dense from 1, the same for equal lists, for the life of
    /// the process.
    #[inline]
    pub fn index(self) -> u32 {
        self.index.get()
    }

    /// The list at `index`, which [`Self::index`] gave. Panics on any other number.
    pub fn from_index(_interner: DbInterner<'db>, index: u32) -> Self {
        match NonZeroU32::new(index) {
            Some(index) if ClausesStorage::table().published(index) => Self::at(index),
            _ => panic!("{index} is not the index of an interned Clauses"),
        }
    }

    #[inline]
    pub(crate) fn at(index: NonZeroU32) -> Self {
        Clauses { index, _db: PhantomData }
    }

    #[inline]
    pub(crate) fn row(self) -> NonZeroU32 {
        self.index
    }

    #[inline]
    pub fn new_from_iter<I, T>(args: I) -> T::Output
    where
        I: IntoIterator<Item = T>,
        T: CollectAndApply<Clause<'db>, Self>,
    {
        CollectAndApply::collect_and_apply(args.into_iter(), Self::new_from_slice)
    }

    #[inline]
    pub fn as_slice(self) -> &'db [Clause<'db>] {
        ClausesStorage::table().get(self.index)
    }

    #[inline]
    pub fn iter(self) -> ::std::iter::Copied<::std::slice::Iter<'db, Clause<'db>>> {
        self.as_slice().iter().copied()
    }

    #[inline]
    pub fn len(self) -> usize {
        self.as_slice().len()
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.as_slice().is_empty()
    }
}

impl Default for Clauses<'_> {
    #[inline]
    fn default() -> Self {
        Self::empty()
    }
}

impl<'db> IntoIterator for Clauses<'db> {
    type IntoIter = ::std::iter::Copied<::std::slice::Iter<'db, Clause<'db>>>;
    type Item = Clause<'db>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'db> std::ops::Deref for Clauses<'db> {
    type Target = [Clause<'db>];

    #[inline]
    fn deref(&self) -> &Self::Target {
        (*self).as_slice()
    }
}

impl<'db> rustc_type_ir::inherent::SliceLike for Clauses<'db> {
    type Item = Clause<'db>;

    type IntoIter = ::std::iter::Copied<::std::slice::Iter<'db, Clause<'db>>>;

    #[inline]
    fn iter(self) -> Self::IntoIter {
        self.iter()
    }

    #[inline]
    fn as_slice(&self) -> &[Self::Item] {
        (*self).as_slice()
    }
}

impl<'db> rustc_type_ir::inherent::Clauses<DbInterner<'db>> for Clauses<'db> {}

impl<'db> rustc_type_ir::TypeSuperFoldable<DbInterner<'db>> for Clauses<'db> {
    fn try_super_fold_with<F: rustc_type_ir::FallibleTypeFolder<DbInterner<'db>>>(
        self,
        folder: &mut F,
    ) -> Result<Self, F::Error> {
        let clauses = self.as_slice();
        let mut iter = clauses.iter().copied().enumerate();
        let first_change = iter.by_ref().find_map(|(i, clause)| match clause.try_fold_with(folder) {
            Ok(new) if new == clause => None,
            new => Some((i, new)),
        });
        match first_change {
            None => Ok(self),
            Some((_, Err(error))) => Err(error),
            Some((i, Ok(new))) => {
                let mut folded = SmallVec::<[Clause<'db>; 8]>::with_capacity(clauses.len());
                folded.extend_from_slice(&clauses[..i]);
                folded.push(new);
                for (_, clause) in iter {
                    folded.push(clause.try_fold_with(folder)?);
                }
                Ok(Clauses::new_from_slice(&folded))
            }
        }
    }

    fn super_fold_with<F: rustc_type_ir::TypeFolder<DbInterner<'db>>>(
        self,
        folder: &mut F,
    ) -> Self {
        let clauses = self.as_slice();
        let mut iter = clauses.iter().copied().enumerate();
        let first_change = iter.by_ref().find_map(|(i, clause)| {
            let new = clause.fold_with(folder);
            (new != clause).then_some((i, new))
        });
        match first_change {
            None => self,
            Some((i, new)) => {
                let mut folded = SmallVec::<[Clause<'db>; 8]>::with_capacity(clauses.len());
                folded.extend_from_slice(&clauses[..i]);
                folded.push(new);
                folded.extend(iter.map(|(_, clause)| clause.fold_with(folder)));
                Clauses::new_from_slice(&folded)
            }
        }
    }
}

impl<'db> rustc_type_ir::TypeFoldable<DbInterner<'db>> for Clauses<'db> {
    fn try_fold_with<F: rustc_type_ir::FallibleTypeFolder<DbInterner<'db>>>(
        self,
        folder: &mut F,
    ) -> Result<Self, F::Error> {
        folder.try_fold_clauses(self)
    }
    fn fold_with<F: rustc_type_ir::TypeFolder<DbInterner<'db>>>(self, folder: &mut F) -> Self {
        folder.fold_clauses(self)
    }
}

impl<'db> rustc_type_ir::TypeVisitable<DbInterner<'db>> for Clauses<'db> {
    fn visit_with<V: rustc_type_ir::TypeVisitor<DbInterner<'db>>>(
        &self,
        visitor: &mut V,
    ) -> V::Result {
        visitor.visit_clauses(*self)
    }
}

// SAFETY: visits every field: the list's one field is its row, and every clause in it.
unsafe impl<'db, V: super::WorldExposer> rustc_type_ir::GenericTypeVisitable<V> for Clauses<'db> {
    fn generic_visit_with(&self, visitor: &mut V) {
        if visitor.on_interned(ClausesStorage::table().name(), self.index.get()).is_continue() {
            self.as_slice().iter().for_each(|it| it.generic_visit_with(visitor));
        }
    }
}

impl<'db> rustc_type_ir::Flags for Clauses<'db> {
    #[inline]
    fn flags(&self) -> rustc_type_ir::TypeFlags {
        ClausesStorage::table().flags(self.index)
    }

    #[inline]
    fn outer_exclusive_binder(&self) -> rustc_type_ir::DebruijnIndex {
        ClausesStorage::table().binder(self.index)
    }
}

impl<'db> rustc_type_ir::TypeSuperVisitable<DbInterner<'db>> for Clauses<'db> {
    fn super_visit_with<V: rustc_type_ir::TypeVisitor<DbInterner<'db>>>(
        &self,
        visitor: &mut V,
    ) -> V::Result {
        use rustc_ast_ir::visit::VisitorResult;
        rustc_ast_ir::walk_visitable_list!(visitor, (*self).as_slice());
        V::Result::output()
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, GenericTypeVisitable)] // TODO implement Debug by hand
pub struct Clause<'db>(pub(crate) Predicate<'db>);

// We could cram the reveal into the clauses like rustc does, probably
#[derive(
    Copy, Clone, Debug, Hash, PartialEq, Eq, TypeVisitable, TypeFoldable, GenericTypeVisitable,
)]
pub struct ParamEnv<'db> {
    pub(crate) clauses: Clauses<'db>,
}

impl<'db> ParamEnv<'db> {
    #[inline]
    pub fn empty() -> Self {
        ParamEnv { clauses: Clauses::empty() }
    }

    pub fn new(clauses: Clauses<'db>) -> Self {
        ParamEnv { clauses }
    }

    pub fn clauses(self) -> Clauses<'db> {
        self.clauses
    }
}

impl<'db> rustc_type_ir::inherent::ParamEnv<DbInterner<'db>> for ParamEnv<'db> {
    fn caller_bounds(self) -> impl Iterator<Item = Clause<'db>> {
        self.clauses.iter()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ParamEnvAnd<'db, T> {
    pub param_env: ParamEnv<'db>,
    pub value: T,
}

impl<'db, T> ParamEnvAnd<'db, T> {
    pub fn into_parts(self) -> (ParamEnv<'db>, T) {
        (self.param_env, self.value)
    }
}

impl<'db> TypeVisitable<DbInterner<'db>> for Predicate<'db> {
    fn visit_with<V: rustc_type_ir::TypeVisitor<DbInterner<'db>>>(
        &self,
        visitor: &mut V,
    ) -> V::Result {
        visitor.visit_predicate(*self)
    }
}

// SAFETY: visits every field: the predicate's one field is its row, and its kind.
unsafe impl<'db, V: super::WorldExposer> GenericTypeVisitable<V> for Predicate<'db> {
    fn generic_visit_with(&self, visitor: &mut V) {
        if visitor.on_interned(PREDICATES.name(), self.index.get()).is_continue() {
            self.kind().generic_visit_with(visitor);
        }
    }
}

impl<'db> TypeSuperVisitable<DbInterner<'db>> for Predicate<'db> {
    fn super_visit_with<V: rustc_type_ir::TypeVisitor<DbInterner<'db>>>(
        &self,
        visitor: &mut V,
    ) -> V::Result {
        (*self).kind().visit_with(visitor)
    }
}

impl<'db> TypeFoldable<DbInterner<'db>> for Predicate<'db> {
    fn try_fold_with<F: rustc_type_ir::FallibleTypeFolder<DbInterner<'db>>>(
        self,
        folder: &mut F,
    ) -> Result<Self, F::Error> {
        folder.try_fold_predicate(self)
    }
    fn fold_with<F: rustc_type_ir::TypeFolder<DbInterner<'db>>>(self, folder: &mut F) -> Self {
        folder.fold_predicate(self)
    }
}

impl<'db> TypeSuperFoldable<DbInterner<'db>> for Predicate<'db> {
    fn try_super_fold_with<F: rustc_type_ir::FallibleTypeFolder<DbInterner<'db>>>(
        self,
        folder: &mut F,
    ) -> Result<Self, F::Error> {
        let new = self.kind().try_fold_with(folder)?;
        Ok(Predicate::new(folder.cx(), new))
    }
    fn super_fold_with<F: rustc_type_ir::TypeFolder<DbInterner<'db>>>(
        self,
        folder: &mut F,
    ) -> Self {
        let new = self.kind().fold_with(folder);
        Predicate::new(folder.cx(), new)
    }
}

impl<'db> Elaboratable<DbInterner<'db>> for Predicate<'db> {
    fn predicate(&self) -> <DbInterner<'db> as rustc_type_ir::Interner>::Predicate {
        *self
    }

    fn child(&self, clause: <DbInterner<'db> as rustc_type_ir::Interner>::Clause) -> Self {
        clause.as_predicate()
    }

    fn child_with_derived_cause(
        &self,
        clause: <DbInterner<'db> as rustc_type_ir::Interner>::Clause,
        _span: <DbInterner<'db> as rustc_type_ir::Interner>::Span,
        _parent_trait_pred: PolyTraitClause<'db>,
        _index: usize,
    ) -> Self {
        clause.as_predicate()
    }
}

impl<'db> PredicateProxy<DbInterner<'db>> for Predicate<'db> {
    #[inline]
    fn allow_normalization(&self) -> bool {
        rustc_type_ir::inherent::Predicate::allow_normalization(*self)
    }

    #[inline]
    fn clause_kind_unchecked(&self) -> Option<Binder<'db, ClauseKind<'db>>> {
        self.as_clause().map(IntoKind::kind)
    }

    fn map_projection(
        self,
        _cx: DbInterner<'db>,
        f: impl FnOnce(PolyProjectionClause<'db>) -> PolyProjectionClause<'db>,
    ) -> Option<Self> {
        self.map_projection_clause(f)
    }
}

impl<'db> Flags for Predicate<'db> {
    #[inline]
    fn flags(&self) -> rustc_type_ir::TypeFlags {
        PREDICATES.flags(self.index)
    }

    #[inline]
    fn outer_exclusive_binder(&self) -> rustc_type_ir::DebruijnIndex {
        PREDICATES.binder(self.index)
    }
}

impl<'db> IntoKind for Predicate<'db> {
    type Kind = Binder<'db, PredicateKind<'db>>;

    #[inline]
    fn kind(self) -> Self::Kind {
        PREDICATES.kind(self.index)
    }
}

impl<'db> UpcastFrom<DbInterner<'db>, ty::PredicateKind<DbInterner<'db>>> for Predicate<'db> {
    fn upcast_from(from: ty::PredicateKind<DbInterner<'db>>, interner: DbInterner<'db>) -> Self {
        Binder::dummy(from).upcast(interner)
    }
}
impl<'db>
    UpcastFrom<DbInterner<'db>, ty::Binder<DbInterner<'db>, ty::PredicateKind<DbInterner<'db>>>>
    for Predicate<'db>
{
    fn upcast_from(
        from: ty::Binder<DbInterner<'db>, ty::PredicateKind<DbInterner<'db>>>,
        interner: DbInterner<'db>,
    ) -> Self {
        Predicate::new(interner, from)
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, ty::ClauseKind<DbInterner<'db>>> for Predicate<'db> {
    fn upcast_from(from: ty::ClauseKind<DbInterner<'db>>, interner: DbInterner<'db>) -> Self {
        Binder::dummy(PredicateKind::Clause(from)).upcast(interner)
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, ty::Binder<DbInterner<'db>, ty::ClauseKind<DbInterner<'db>>>>
    for Predicate<'db>
{
    fn upcast_from(
        from: ty::Binder<DbInterner<'db>, ty::ClauseKind<DbInterner<'db>>>,
        interner: DbInterner<'db>,
    ) -> Self {
        from.map_bound(PredicateKind::Clause).upcast(interner)
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, Clause<'db>> for Predicate<'db> {
    fn upcast_from(from: Clause<'db>, _interner: DbInterner<'db>) -> Self {
        from.0
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, ty::NormalizesTo<DbInterner<'db>>> for Predicate<'db> {
    fn upcast_from(from: ty::NormalizesTo<DbInterner<'db>>, interner: DbInterner<'db>) -> Self {
        PredicateKind::NormalizesTo(from).upcast(interner)
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, ty::TraitRef<DbInterner<'db>>> for Predicate<'db> {
    fn upcast_from(from: ty::TraitRef<DbInterner<'db>>, interner: DbInterner<'db>) -> Self {
        Binder::dummy(from).upcast(interner)
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, ty::Binder<DbInterner<'db>, ty::TraitRef<DbInterner<'db>>>>
    for Predicate<'db>
{
    fn upcast_from(
        from: ty::Binder<DbInterner<'db>, ty::TraitRef<DbInterner<'db>>>,
        interner: DbInterner<'db>,
    ) -> Self {
        from.map_bound_no_validate_bound_vars(|trait_ref| TraitClause {
            trait_ref,
            polarity: ClausePolarity::Positive,
        })
        .upcast(interner)
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, PolyTraitClause<'db>> for Predicate<'db> {
    fn upcast_from(from: PolyTraitClause<'db>, interner: DbInterner<'db>) -> Self {
        from.map_bound_no_validate_bound_vars(|it| PredicateKind::Clause(ClauseKind::Trait(it)))
            .upcast(interner)
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, PolyProjectionClause<'db>> for Predicate<'db> {
    fn upcast_from(from: PolyProjectionClause<'db>, interner: DbInterner<'db>) -> Self {
        from.map_bound_no_validate_bound_vars(|it| {
            PredicateKind::Clause(ClauseKind::Projection(it))
        })
        .upcast(interner)
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, ProjectionClause<'db>> for Predicate<'db> {
    fn upcast_from(from: ProjectionClause<'db>, interner: DbInterner<'db>) -> Self {
        PredicateKind::Clause(ClauseKind::Projection(from)).upcast(interner)
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, TraitClause<'db>> for Predicate<'db> {
    fn upcast_from(from: TraitClause<'db>, interner: DbInterner<'db>) -> Self {
        PredicateKind::Clause(ClauseKind::Trait(from)).upcast(interner)
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, TypeOutlivesClause<'db>> for Predicate<'db> {
    fn upcast_from(from: TypeOutlivesClause<'db>, interner: DbInterner<'db>) -> Self {
        PredicateKind::Clause(ClauseKind::TypeOutlives(from)).upcast(interner)
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, RegionOutlivesClause<'db>> for Predicate<'db> {
    fn upcast_from(from: RegionOutlivesClause<'db>, interner: DbInterner<'db>) -> Self {
        PredicateKind::Clause(ClauseKind::RegionOutlives(from)).upcast(interner)
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, TypeOutlivesClause<'db>> for Clause<'db> {
    fn upcast_from(from: TypeOutlivesClause<'db>, interner: DbInterner<'db>) -> Self {
        Clause(from.upcast(interner))
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, RegionOutlivesClause<'db>> for Clause<'db> {
    fn upcast_from(from: RegionOutlivesClause<'db>, interner: DbInterner<'db>) -> Self {
        Clause(from.upcast(interner))
    }
}

impl<'db> UpcastFrom<DbInterner<'db>, PolyRegionOutlivesClause<'db>> for Predicate<'db> {
    fn upcast_from(from: PolyRegionOutlivesClause<'db>, tcx: DbInterner<'db>) -> Self {
        from.map_bound(|p| PredicateKind::Clause(ClauseKind::RegionOutlives(p))).upcast(tcx)
    }
}

impl<'db> rustc_type_ir::inherent::Predicate<DbInterner<'db>> for Predicate<'db> {
    fn as_clause(self) -> Option<<DbInterner<'db> as rustc_type_ir::Interner>::Clause> {
        match self.kind().skip_binder() {
            PredicateKind::Clause(..) => Some(self.expect_clause()),
            _ => None,
        }
    }
}

impl<'db> Predicate<'db> {
    pub fn as_trait_clause(self) -> Option<PolyTraitClause<'db>> {
        let predicate = self.kind();
        match predicate.skip_binder() {
            PredicateKind::Clause(ClauseKind::Trait(t)) => Some(predicate.rebind(t)),
            _ => None,
        }
    }

    pub fn as_projection_clause(self) -> Option<PolyProjectionClause<'db>> {
        let predicate = self.kind();
        match predicate.skip_binder() {
            PredicateKind::Clause(ClauseKind::Projection(t)) => Some(predicate.rebind(t)),
            _ => None,
        }
    }

    /// Matches a `PredicateKind::Clause` and turns it into a `Clause`, otherwise returns `None`.
    pub fn as_clause(self) -> Option<Clause<'db>> {
        match self.kind().skip_binder() {
            PredicateKind::Clause(..) => Some(self.expect_clause()),
            _ => None,
        }
    }

    /// Assert that the predicate is a clause.
    pub fn expect_clause(self) -> Clause<'db> {
        match self.kind().skip_binder() {
            PredicateKind::Clause(..) => Clause(self),
            _ => panic!("{self:?} is not a clause"),
        }
    }
}

impl<'db> TypeVisitable<DbInterner<'db>> for Clause<'db> {
    fn visit_with<V: rustc_type_ir::TypeVisitor<DbInterner<'db>>>(
        &self,
        visitor: &mut V,
    ) -> V::Result {
        visitor.visit_predicate(*self)
    }
}

impl<'db> TypeSuperVisitable<DbInterner<'db>> for Clause<'db> {
    fn super_visit_with<V: rustc_type_ir::TypeVisitor<DbInterner<'db>>>(
        &self,
        visitor: &mut V,
    ) -> V::Result {
        self.0.super_visit_with(visitor)
    }
}

impl<'db> TypeFoldable<DbInterner<'db>> for Clause<'db> {
    fn try_fold_with<F: rustc_type_ir::FallibleTypeFolder<DbInterner<'db>>>(
        self,
        folder: &mut F,
    ) -> Result<Self, F::Error> {
        folder.try_fold_predicate(self)
    }
    fn fold_with<F: rustc_type_ir::TypeFolder<DbInterner<'db>>>(self, folder: &mut F) -> Self {
        folder.fold_predicate(self)
    }
}

impl<'db> TypeSuperFoldable<DbInterner<'db>> for Clause<'db> {
    fn try_super_fold_with<F: rustc_type_ir::FallibleTypeFolder<DbInterner<'db>>>(
        self,
        folder: &mut F,
    ) -> Result<Self, F::Error> {
        let kind = self.kind().try_fold_with(folder)?;
        Ok(Clause(Predicate::intern(kind.map_bound_no_validate_bound_vars(PredicateKind::Clause))))
    }
    fn super_fold_with<F: rustc_type_ir::TypeFolder<DbInterner<'db>>>(
        self,
        folder: &mut F,
    ) -> Self {
        let kind = self.kind().fold_with(folder);
        Clause(Predicate::intern(kind.map_bound_no_validate_bound_vars(PredicateKind::Clause)))
    }
}

impl<'db> Flags for Clause<'db> {
    #[inline]
    fn flags(&self) -> rustc_type_ir::TypeFlags {
        self.0.flags()
    }

    #[inline]
    fn outer_exclusive_binder(&self) -> rustc_type_ir::DebruijnIndex {
        self.0.outer_exclusive_binder()
    }
}

impl<'db> PredicateProxy<DbInterner<'db>> for Clause<'db> {
    #[inline]
    fn allow_normalization(&self) -> bool {
        rustc_type_ir::inherent::Predicate::allow_normalization(self.0)
    }

    #[inline]
    fn clause_kind_unchecked(&self) -> Option<Binder<'db, ClauseKind<'db>>> {
        Some(self.kind())
    }

    fn map_projection(
        self,
        _cx: DbInterner<'db>,
        f: impl FnOnce(PolyProjectionClause<'db>) -> PolyProjectionClause<'db>,
    ) -> Option<Self> {
        self.0.map_projection_clause(f).map(Clause)
    }
}

impl<'db> IntoKind for Clause<'db> {
    type Kind = Binder<'db, ClauseKind<'db>>;

    #[inline]
    fn kind(self) -> Self::Kind {
        self.0.kind().map_bound_no_validate_bound_vars(|pk| match pk {
            PredicateKind::Clause(kind) => kind,
            _ => unreachable!("a `Clause` is made of a `PredicateKind::Clause` only"),
        })
    }
}

impl<'db> Clause<'db> {
    pub fn as_predicate(self) -> Predicate<'db> {
        self.0
    }
}

impl<'db> Elaboratable<DbInterner<'db>> for Clause<'db> {
    fn predicate(&self) -> <DbInterner<'db> as rustc_type_ir::Interner>::Predicate {
        self.0
    }

    fn child(&self, clause: <DbInterner<'db> as rustc_type_ir::Interner>::Clause) -> Self {
        clause
    }

    fn child_with_derived_cause(
        &self,
        clause: <DbInterner<'db> as rustc_type_ir::Interner>::Clause,
        _span: <DbInterner<'db> as rustc_type_ir::Interner>::Span,
        _parent_trait_pred: PolyTraitClause<'db>,
        _index: usize,
    ) -> Self {
        clause
    }
}

impl<'db> UpcastFrom<DbInterner<'db>, Binder<'db, ClauseKind<'db>>> for Clause<'db> {
    fn upcast_from(from: Binder<'db, ClauseKind<'db>>, interner: DbInterner<'db>) -> Self {
        Clause(from.map_bound_no_validate_bound_vars(PredicateKind::Clause).upcast(interner))
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, TraitRef<'db>> for Clause<'db> {
    fn upcast_from(from: TraitRef<'db>, interner: DbInterner<'db>) -> Self {
        Clause(from.upcast(interner))
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, PolyTraitRef<'db>> for Clause<'db> {
    fn upcast_from(from: PolyTraitRef<'db>, interner: DbInterner<'db>) -> Self {
        Clause(from.upcast(interner))
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, TraitClause<'db>> for Clause<'db> {
    fn upcast_from(from: TraitClause<'db>, interner: DbInterner<'db>) -> Self {
        Clause(from.upcast(interner))
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, PolyTraitClause<'db>> for Clause<'db> {
    fn upcast_from(from: PolyTraitClause<'db>, interner: DbInterner<'db>) -> Self {
        Clause(from.upcast(interner))
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, ProjectionClause<'db>> for Clause<'db> {
    fn upcast_from(from: ProjectionClause<'db>, interner: DbInterner<'db>) -> Self {
        Clause(from.upcast(interner))
    }
}
impl<'db> UpcastFrom<DbInterner<'db>, PolyProjectionClause<'db>> for Clause<'db> {
    fn upcast_from(from: PolyProjectionClause<'db>, interner: DbInterner<'db>) -> Self {
        Clause(from.upcast(interner))
    }
}

impl<'db> rustc_type_ir::inherent::Clause<DbInterner<'db>> for Clause<'db> {
    fn as_predicate(self) -> <DbInterner<'db> as rustc_type_ir::Interner>::Predicate {
        self.0
    }

    fn instantiate_supertrait(
        self,
        cx: DbInterner<'db>,
        trait_ref: rustc_type_ir::Binder<DbInterner<'db>, rustc_type_ir::TraitRef<DbInterner<'db>>>,
    ) -> Self {
        tracing::debug!(?self, ?trait_ref);
        // See the rustc impl for a long comment
        let bound_pred = self.kind();
        let pred_bound_vars = bound_pred.bound_vars();
        let trait_bound_vars = trait_ref.bound_vars();
        // 1) Self: Bar1<'a, '^0.0> -> Self: Bar1<'a, '^0.1>
        let shifted_pred =
            cx.shift_bound_var_indices(trait_bound_vars.len(), bound_pred.skip_binder());
        // 2) Self: Bar1<'a, '^0.1> -> T: Bar1<'^0.0, '^0.1>
        let new = EarlyBinder::bind(cx, shifted_pred)
            .instantiate(cx, trait_ref.skip_binder().args)
            .skip_norm_wip();
        // 3) ['x] + ['b] -> ['x, 'b]
        let bound_vars =
            BoundVarKinds::new_from_iter(trait_bound_vars.iter().chain(pred_bound_vars.iter()));

        let predicate: Predicate<'db> =
            ty::Binder::bind_with_vars(PredicateKind::Clause(new), bound_vars).upcast(cx);
        predicate.expect_clause()
    }
}

#[cfg(test)]
mod tests {
    use rustc_type_ir::inherent::Clause as _;
    use blaze_query::db::FromId;

    use super::*;
    use crate::next_solver::{AliasTermKind, GenericArgs, TraitAssocTyId};

    fn id<T: FromId>(index: u32) -> T {
        T::from_id(blaze_query::db::Id::from_u32(index))
    }

    fn clause<'db>(kind: ClauseKind<'db>) -> Clause<'db> {
        Clause(Predicate::intern(Binder::dummy(PredicateKind::Clause(kind))))
    }

    struct Samples<'db> {
        trait_: Clause<'db>,
        outlives: Clause<'db>,
        well_formed: Clause<'db>,
        projection: Clause<'db>,
    }

    fn samples<'db>() -> Samples<'db> {
        let types = &default_types().types;
        let statik = default_types().regions.statik;
        let args = GenericArgs::new_from_slice(&[types.u8.into()]);
        let trait_ref = TraitRef::from_checked_parts(TraitIdWrapper(id(4)), args);
        Samples {
            trait_: clause(ClauseKind::Trait(TraitClause {
                trait_ref,
                polarity: ClausePolarity::Positive,
            })),
            outlives: clause(ClauseKind::TypeOutlives(ty::OutlivesClause(types.u8, statik))),
            well_formed: clause(ClauseKind::WellFormed(types.u8.into())),
            projection: clause(ClauseKind::Projection(ProjectionClause {
                projection_term: AliasTerm::from_checked_parts(
                    AliasTermKind::ProjectionTy { def_id: TraitAssocTyId(id(5)) },
                    args,
                ),
                term: types.u8.into(),
            })),
        }
    }

    #[test]
    fn a_clause_and_its_predicate_are_the_same_proxy() {
        let c = samples();
        for clause in [c.trait_, c.outlives, c.well_formed, c.projection] {
            let predicate = clause.as_predicate();
            assert_eq!(PredicateProxy::clause_kind_unchecked(&clause), Some(clause.kind()));
            assert_eq!(PredicateProxy::clause_kind_unchecked(&predicate), Some(clause.kind()));
            assert_eq!(
                PredicateProxy::allow_normalization(&clause),
                PredicateProxy::allow_normalization(&predicate),
            );
            assert_eq!(clause.flags(), predicate.flags());
            assert_eq!(clause.outer_exclusive_binder(), predicate.outer_exclusive_binder());
        }
        assert!(!PredicateProxy::allow_normalization(&c.well_formed));
        assert!(PredicateProxy::allow_normalization(&c.trait_));
        let ambiguous = Predicate::intern(Binder::dummy(PredicateKind::Ambiguous));
        assert_eq!(PredicateProxy::clause_kind_unchecked(&ambiguous), None);
    }

    #[test]
    fn map_projection_maps_projection_clauses_only() {
        let c = samples();
        let types = &default_types().types;
        let mapped = c
            .projection
            .0
            .map_projection_clause(|it| {
                it.map_bound(|it| ProjectionClause { term: types.bool.into(), ..it })
            })
            .expect("a projection clause");
        let before = c.projection.0.as_projection_clause().unwrap().skip_binder();
        let after = mapped.as_projection_clause().unwrap().skip_binder();
        assert_eq!(after.term, types.bool.into());
        assert_eq!(after.projection_term, before.projection_term);
        assert_eq!(c.projection.0.map_projection_clause(|it| it), Some(c.projection.0));
        for other in [c.trait_, c.outlives, c.well_formed] {
            assert_eq!(other.0.map_projection_clause(|it| it), None);
        }
    }

    #[test]
    fn as_type_outlives_clause_finds_type_outlives_only() {
        let c = samples();
        let types = &default_types().types;
        assert_eq!(
            c.outlives.as_type_outlives_clause().map(Binder::skip_binder),
            Some(ty::OutlivesClause(types.u8, default_types().regions.statik)),
        );
        for other in [c.trait_, c.well_formed, c.projection] {
            assert!(other.as_type_outlives_clause().is_none());
        }
    }
}
