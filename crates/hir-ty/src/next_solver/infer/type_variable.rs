//! Storage for type variables for the infer context the next-trait-solver.

use std::cmp;
use std::ops::Range;

use hir_def::TypeParamId;
use rustc_type_ir::TyVid;
use rustc_type_ir::UniverseIndex;
use rustc_type_ir::inherent::Ty as _;
use tracing::debug;

use crate::Span;
use crate::next_solver::Ty;
use crate::next_solver::infer::iter_idx_range;
use crate::next_solver::infer::touched::Touched;
use crate::next_solver::infer::union_find::{NoError, UnifyValue, UnionFind, UnionFindMark};

#[derive(Debug, Default)]
pub(crate) struct TypeVariableStorage<'db> {
    /// Two variables are unified in `eq_relations` when we have a
    /// constraint `?X == ?Y`. This table also stores, for each key,
    /// the known value.
    eq_relations: UnionFind<TyVid, TypeVariableValue<'db>>,
    /// Only used by `-Znext-solver` and for diagnostics. Tracks whether
    /// type variables are related via subtyping at all, ignoring which of
    /// the two is the subtype.
    ///
    /// When reporting ambiguity errors, we sometimes want to
    /// treat all inference vars which are subtypes of each
    /// others as if they are equal. For this case we compute
    /// the transitive closure of our subtype obligations here.
    ///
    /// E.g. when encountering ambiguity errors, we want to suggest
    /// specifying some method argument or to add a type annotation
    /// to a local variable. Because subtyping cannot change the
    /// shape of a type, it's fine if the cause of the ambiguity error
    /// is only related to the suggested variable via subtyping.
    ///
    /// Even for something like `let x = returns_arg(); x.method();` the
    /// type of `x` is only a supertype of the argument of `returns_arg`. We
    /// still want to suggest specifying the type of the argument.
    sub_unification_table: UnionFind<TyVid, TypeVariableSubValue>,
    /// The type parameter each variable made for one stands for (rustc's
    /// `TypeVariableOrigin::param_def_id`), in variable order. Per variable, not per set, as in
    /// rustc; sparse, as most variables stand for no parameter.
    params: Vec<(TyVid, TypeParamId)>,
}

/// Where both tables of type variables stood when a snapshot started.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TypeVariableMark {
    eq: UnionFindMark,
    sub: UnionFindMark,
}

pub(crate) struct TypeVariableTable<'a, 'db> {
    storage: &'a mut TypeVariableStorage<'db>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum TypeVariableValue<'db> {
    Known { value: Ty<'db> },
    Unknown { universe: UniverseIndex },
}

impl<'db> TypeVariableValue<'db> {
    /// If this value is known, returns the type it is known to be.
    /// Otherwise, `None`.
    pub(crate) fn known(&self) -> Option<Ty<'db>> {
        match self {
            TypeVariableValue::Unknown { .. } => None,
            TypeVariableValue::Known { value, .. } => Some(*value),
        }
    }

    pub(crate) fn is_unknown(&self) -> bool {
        match *self {
            TypeVariableValue::Unknown { .. } => true,
            TypeVariableValue::Known { .. } => false,
        }
    }
}

impl Clone for TypeVariableStorage<'_> {
    fn clone(&self) -> Self {
        TypeVariableStorage {
            // clone: only `InferCtxt::fork` copies a table.
            eq_relations: self.eq_relations.clone(),
            // clone: only `InferCtxt::fork` copies a table.
            sub_unification_table: self.sub_unification_table.clone(),
            // clone: only `InferCtxt::fork` copies a table.
            params: self.params.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.eq_relations.clone_from(&source.eq_relations);
        self.sub_unification_table.clone_from(&source.sub_unification_table);
        self.params.clone_from(&source.params);
    }
}

impl<'db> TypeVariableStorage<'db> {
    /// Back to `default()`, its capacity kept.
    pub(crate) fn empty(&mut self) {
        self.eq_relations.empty();
        self.sub_unification_table.empty();
        self.params.clear();
    }

    /// Bytes it holds room for.
    pub(crate) fn held(&self) -> usize {
        self.eq_relations.held()
            + self.sub_unification_table.held()
            + self.params.capacity() * size_of::<(TyVid, TypeParamId)>()
    }

    #[inline]
    pub(crate) fn table(&mut self) -> TypeVariableTable<'_, 'db> {
        TypeVariableTable { storage: self }
    }

    /// The value of `vid` if it is the root of its set, read without changing the table.
    #[inline]
    pub(crate) fn try_probe_value(&self, vid: TyVid) -> Option<&TypeVariableValue<'db>> {
        self.eq_relations.try_probe_value(vid)
    }

    /// Whether `vid` is the root of its set in the sub-unification table, read without changing
    /// the table.
    #[inline]
    pub(crate) fn is_sub_unification_table_root_var(&self, vid: TyVid) -> bool {
        self.sub_unification_table.try_probe_value(vid).is_some()
    }

    /// The count of changes to both tables; see `UnionFind::changes`.
    #[inline]
    pub(crate) fn changes(&self) -> u64 {
        self.eq_relations.changes() + self.sub_unification_table.changes()
    }

    /// The rows written in the equality table; see `infer::touched`.
    #[inline]
    pub(crate) fn touched_eq(&self) -> &Touched {
        self.eq_relations.touched()
    }

    /// The rows written in the sub-unification table.
    #[inline]
    pub(crate) fn touched_sub(&self) -> &Touched {
        self.sub_unification_table.touched()
    }

    pub(crate) fn start_snapshot(&mut self) -> TypeVariableMark {
        TypeVariableMark {
            eq: self.eq_relations.start_snapshot(),
            sub: self.sub_unification_table.start_snapshot(),
        }
    }

    pub(crate) fn rollback_to(&mut self, mark: TypeVariableMark) {
        self.eq_relations.rollback_to(mark.eq);
        self.sub_unification_table.rollback_to(mark.sub);
        let len = self.eq_relations.len();
        while self.params.last().is_some_and(|&(vid, _)| vid.as_usize() >= len) {
            self.params.pop();
        }
    }

    /// The type parameter `vid` was made for, if any.
    pub(crate) fn param(&self, vid: TyVid) -> Option<TypeParamId> {
        let at = self.params.binary_search_by_key(&vid, |&(vid, _)| vid).ok()?;
        Some(self.params[at].1)
    }

    pub(crate) fn commit(&mut self, mark: TypeVariableMark) {
        self.eq_relations.commit(mark.eq);
        self.sub_unification_table.commit(mark.sub);
    }
}

impl<'db> TypeVariableTable<'_, 'db> {
    pub(crate) fn var_span(&mut self, vid: TyVid) -> Span {
        // We return the span from unification and not equation, since when equating we also unify,
        // and we want to prevent duplicate diagnostics from vars that were unified.
        self.storage.sub_unification_table.probe_value(vid).span
    }

    /// Records that `a == b`, depending on `dir`.
    ///
    /// Precondition: neither `a` nor `b` are known.
    pub(crate) fn equate(&mut self, a: TyVid, b: TyVid) {
        debug_assert!(self.probe(a).is_unknown());
        debug_assert!(self.probe(b).is_unknown());
        self.storage.eq_relations.union(a, b);
        self.storage.sub_unification_table.union(a, b);
    }

    /// Records that `a` and `b` are related via subtyping. We don't track
    /// which of the two is the subtype.
    ///
    /// Precondition: neither `a` nor `b` are known.
    pub(crate) fn sub_unify(&mut self, a: TyVid, b: TyVid) {
        debug_assert!(self.probe(a).is_unknown());
        debug_assert!(self.probe(b).is_unknown());
        self.storage.sub_unification_table.union(a, b);
    }

    /// Instantiates `vid` with the type `ty`.
    ///
    /// Precondition: `vid` must not have been previously instantiated.
    pub(crate) fn instantiate(&mut self, vid: TyVid, ty: Ty<'db>) {
        let vid = self.root_var(vid);
        debug_assert!(!ty.is_ty_var(), "instantiating ty var with var: {vid:?} {ty:?}");
        debug_assert!(self.probe(vid).is_unknown());
        debug_assert!(
            self.storage.eq_relations.probe_value(vid).is_unknown(),
            "instantiating type variable `{vid:?}` twice: new-value = {ty:?}, old-value={:?}",
            self.storage.eq_relations.probe_value(vid)
        );
        self.storage.eq_relations.union_value(vid, TypeVariableValue::Known { value: ty });
    }

    pub(crate) fn new_var(&mut self, universe: UniverseIndex, span: Span) -> TyVid {
        let eq_key = self.storage.eq_relations.new_key(TypeVariableValue::Unknown { universe });

        let sub_key = self.storage.sub_unification_table.new_key(TypeVariableSubValue { span });
        debug_assert_eq!(eq_key, sub_key);

        debug!("new_var(index={:?}, universe={:?}, span={:?})", eq_key, universe, span);

        eq_key
    }

    /// Like `new_var`, remembering the type parameter the variable stands for.
    pub(crate) fn new_var_for_param(
        &mut self,
        universe: UniverseIndex,
        span: Span,
        param: Option<TypeParamId>,
    ) -> TyVid {
        let vid = self.new_var(universe, span);
        if let Some(param) = param {
            self.storage.params.push((vid, param));
        }
        vid
    }

    /// Returns the number of type variables created thus far.
    pub(crate) fn num_vars(&self) -> usize {
        self.storage.eq_relations.len()
    }

    /// Returns the "root" variable of `vid` in the `eq_relations`
    /// equivalence table. All type variables that have been equated
    /// will yield the same root variable (per the union-find
    /// algorithm), so `root_var(a) == root_var(b)` implies that `a ==
    /// b` (transitively).
    pub(crate) fn root_var(&mut self, vid: TyVid) -> TyVid {
        self.storage.eq_relations.find(vid)
    }

    /// Returns the "root" variable of `vid` in the `sub_unification_table`
    /// equivalence table. All type variables that have been related via
    /// equality or subtyping will yield the same root variable (per the
    /// union-find algorithm), so `sub_unification_table_root_var(a)
    /// == sub_unification_table_root_var(b)` implies that `a` and `b` are
    /// transitively related via subtyping.
    pub(crate) fn sub_unification_table_root_var(&mut self, vid: TyVid) -> TyVid {
        self.storage.sub_unification_table.find(vid)
    }

    /// Retrieves the type to which `vid` has been instantiated, if
    /// any.
    pub(crate) fn probe(&mut self, vid: TyVid) -> TypeVariableValue<'db> {
        self.inlined_probe(vid)
    }

    /// An always-inlined variant of `probe`, for very hot call sites.
    #[inline(always)]
    pub(crate) fn inlined_probe(&mut self, vid: TyVid) -> TypeVariableValue<'db> {
        self.storage.eq_relations.inlined_probe_value(vid)
    }

    /// Retrieves the type to which `vid` has been instantiated, if
    /// any, along with the root `vid`.
    pub(crate) fn probe_with_root_vid(&mut self, vid: TyVid) -> (TyVid, TypeVariableValue<'db>) {
        self.inlined_probe_with_vid(vid)
    }

    /// An always-inlined variant of `probe_with_root_vid`, for very hot call sites.
    #[inline(always)]
    pub(crate) fn inlined_probe_with_vid(
        &mut self,
        vid: TyVid,
    ) -> (TyVid, TypeVariableValue<'db>) {
        self.storage.eq_relations.inlined_probe_key_value(vid)
    }

    /// Returns a range of the type variables created during the snapshot.
    pub(crate) fn vars_since_snapshot(
        &mut self,
        value_count: usize,
    ) -> (Range<TyVid>, Vec<(Span, Option<TypeParamId>)>) {
        let range = TyVid::from_usize(value_count)..TyVid::from_usize(self.num_vars());
        (
            range.start..range.end,
            iter_idx_range(range)
                .map(|vid| (self.var_span(vid), self.storage.param(vid)))
                // alloc: only when fudging, one origin per variable made in the snapshot.
                .collect(),
        )
    }

    /// Returns indices of all root variables that are not yet instantiated.
    pub(crate) fn unresolved_root_variables(&mut self) -> Vec<TyVid> {
        (0..self.num_vars())
            .map(TyVid::from_usize)
            .filter(|&vid| {
                let (root, value) = self.probe_with_root_vid(vid);
                root == vid && value.is_unknown()
            })
            // alloc: once per body, for fallback.
            .collect()
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TypeVariableSubValue {
    span: Span,
}

impl UnifyValue for TypeVariableSubValue {
    type Error = NoError;

    fn unify_values(value1: &Self, value2: &Self) -> Result<Self, NoError> {
        Ok(TypeVariableSubValue { span: Span::pick_best(value1.span, value2.span) })
    }
}

impl<'db> UnifyValue for TypeVariableValue<'db> {
    type Error = NoError;
    /// rustc's `order_roots` of `TyVidEqKey`; the sub-unification table keeps `ena`'s rule.
    const OLDER_ROOT: bool = true;

    fn unify_values(value1: &Self, value2: &Self) -> Result<Self, NoError> {
        match (value1, value2) {
            // We never equate two type variables, both of which
            // have known types. Instead, we recursively equate
            // those types.
            (&TypeVariableValue::Known { .. }, &TypeVariableValue::Known { .. }) => {
                panic!("equating two type variables, both of which have known types")
            }

            // If one side is known, prefer that one.
            (&TypeVariableValue::Known { value }, &TypeVariableValue::Unknown { .. })
            | (&TypeVariableValue::Unknown { .. }, &TypeVariableValue::Known { value }) => {
                Ok(TypeVariableValue::Known { value })
            }

            // If both sides are *unknown*, it hardly matters, does it?
            (
                &TypeVariableValue::Unknown { universe: universe1 },
                &TypeVariableValue::Unknown { universe: universe2 },
            ) => {
                // If we unify two unbound variables, ?T and ?U, then whatever
                // value they wind up taking (which must be the same value) must
                // be nameable by both universes. Therefore, the resulting
                // universe is the minimum of the two universes, because that is
                // the one which contains the fewest names in scope.
                let universe = cmp::min(universe1, universe2);
                Ok(TypeVariableValue::Unknown { universe })
            }
        }
    }
}
