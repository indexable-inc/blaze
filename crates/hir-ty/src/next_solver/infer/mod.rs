//! Infer context the next-trait-solver.

use std::cell::{Cell, RefCell};
use std::mem;
use std::ops::Range;
use std::range::RangeInclusive;
use std::sync::Arc;

pub use BoundRegionConversionTime::*;
use hir_def::{ConstParamId, GenericParamId, TraitId, TypeParamId};
use opaque_types::{OpaqueHiddenType, OpaqueTypeStorage};
use region_constraints::{
    RegionConstraintCollector, RegionConstraintStorage, SolverRegionConstraint,
};
use rustc_next_trait_solver::solve::{GoalEvaluation, SolverDelegateEvalExt};
use rustc_type_ir::{
    AliasTermKind, CanonicalizerState, ClosureKind, ConstVid, FloatVarValue, FloatVid, InferConst,
    InferTy, IntVarValue, IntVid, OutlivesClause, RegionVid, TermKind, TyVid, TypeFoldable,
    TypeFolder, TypeSuperFoldable, TypeVisitableExt, UniverseIndex,
    error::{ExpectedFound, TypeError},
    inherent::{GenericArg as _, GenericArgs as _, IntoKind, SliceLike, Term as _, Ty as _},
    region_constraint::Assumptions,
};
use rustc_type_ir::{
    Upcast,
    solve::{NoSolution, TyOrConstInferVar, VisibleForLeakCheck, inspect},
};
use snapshot::undo_log::InferCtxtUndoLogs;
use tracing::{debug, instrument};
use traits::{ObligationCause, PredicateObligations};
use union_find::UnionFind;
use unify_key::ConstVariableValue;

pub use crate::next_solver::infer::traits::ObligationInspector;
use crate::{
    Span,
    next_solver::{
        ArgOutlivesClause, BoundConst, BoundRegion, BoundTy, BoundVariableKind, FxIndexMap, Goal,
        NextSolverOverflow, Predicate, SolverContext,
        fold::BoundVarReplacerDelegate,
        infer::{at::ToTrace, select::EvaluationResult, traits::PredicateObligation},
        obligation_ctxt::ObligationCtxt,
        recycle,
    },
};

use super::{
    AliasTerm, Binder, CanonicalQueryInput, CanonicalVarValues, Const, ConstKind, DbInterner,
    ErrorGuaranteed, GenericArg, GenericArgs, OpaqueTypeKey, ParamEnv, PolyCoercePredicate,
    PolyExistentialProjection, PolyExistentialTraitRef, PolyFnSig, PolyRegionOutlivesClause,
    PolySubtypePredicate, Region, RegionConstraint, SolverDefId, SubtypePredicate, Term,
    TraitRef, Ty, TyKind, TypingMode,
};

pub mod at;
pub mod canonical;
mod context;
pub mod errors;
pub mod opaque_types;
mod outlives;
pub mod region_constraints;
pub mod relate;
pub mod resolve;
pub mod select;
pub(crate) mod snapshot;
pub mod traits;
pub(crate) mod touched;
mod type_variable;
mod unify_key;
mod union_find;

/// `InferOk<'db, ()>` is used a lot. It may seem like a useless wrapper
/// around `PredicateObligations`, but it has one important property:
/// because `InferOk` is marked with `#[must_use]`, if you have a method
/// `InferCtxt::f` that returns `InferResult<()>` and you call it with
/// `infcx.f()?;` you'll get a warning about the obligations being discarded
/// without use, which is probably unintentional and has been a source of bugs
/// in the past.
#[must_use]
#[derive(Debug)]
pub struct InferOk<'db, T> {
    pub value: T,
    pub obligations: PredicateObligations<'db>,
}
pub type InferResult<'db, T> = Result<InferOk<'db, T>, TypeError<DbInterner<'db>>>;

fn iter_idx_range<T: From<u32> + Into<u32>>(range: Range<T>) -> impl Iterator<Item = T> {
    (range.start.into()..range.end.into()).map(Into::into)
}

/// This type contains all the things within `InferCtxt` that sit within a
/// `RefCell` and are involved with taking/rolling back snapshots. Snapshot
/// operations are hot enough that we want only one call to `borrow_mut` per
/// call to `start_snapshot` and `rollback_to`.
pub struct InferCtxtInner<'db> {
    pub(crate) undo_log: InferCtxtUndoLogs,

    /// The type variables: which are equal, which are related by subtyping, and the known
    /// type of each set.
    pub(crate) type_variable_storage: type_variable::TypeVariableStorage<'db>,

    /// Map from const parameter variable to the kind of const it represents.
    pub(crate) const_unification_storage: UnionFind<ConstVid, ConstVariableValue<'db>>,

    /// The const parameter each variable made for one stands for (rustc's
    /// `ConstVariableOrigin::param_def_id`), in variable order; sparse, as for type variables.
    pub(crate) const_var_params: Vec<(ConstVid, ConstParamId)>,

    /// Map from integral variable to the kind of integer it represents.
    pub(crate) int_unification_storage: UnionFind<IntVid, IntVarValue>,

    /// Map from floating variable to the kind of float it represents.
    pub(crate) float_unification_storage: UnionFind<FloatVid, FloatVarValue>,

    /// Tracks the set of region variables and the constraints between them.
    ///
    /// This is initially `Some(_)` but when
    /// `resolve_regions_and_report_errors` is invoked, this gets set to `None`
    /// -- further attempts to perform unification, etc., may fail if new
    /// region constraints would've been added.
    pub(crate) region_constraint_storage: Option<RegionConstraintStorage<'db>>,

    /// A set of constraints that regionck must validate.
    ///
    /// Each constraint has the form `T:'a`, meaning "some type `T` must
    /// outlive the lifetime 'a". These constraints derive from
    /// instantiated type parameters. So if you had a struct defined
    /// like the following:
    /// ```ignore (illustrative)
    /// struct Foo<T: 'static> { ... }
    /// ```
    /// In some expression `let x = Foo { ... }`, it will
    /// instantiate the type parameter `T` with a fresh type `$0`. At
    /// the same time, it will record a region obligation of
    /// `$0: 'static`. This will get checked later by regionck. (We
    /// can't generally check these things right away because we have
    /// to wait until types are resolved.)
    ///
    /// These are stored in a map keyed to the id of the innermost
    /// enclosing fn body / static initializer expression. This is
    /// because the location where the obligation was incurred can be
    /// relevant with respect to which sublifetime assumptions are in
    /// place. The reason that we store under the fn-id, and not
    /// something more fine-grained, is so that it is easier for
    /// regionck to be sure that it has found *all* the region
    /// obligations (otherwise, it's easy to fail to walk to a
    /// particular node-id).
    ///
    /// Before running `resolve_regions_and_report_errors`, the creator
    /// of the inference context is expected to invoke
    /// `InferCtxt::process_registered_region_obligations`
    /// for each body-id in this map, which will process the
    /// obligations within. This is expected to be done 'late enough'
    /// that all type inference variables have been bound and so forth.
    pub(crate) region_obligations: Vec<TypeOutlivesConstraint<'db>>,

    /// The outlives bounds that we assume must hold about placeholders that
    /// come from instantiating the binder of coroutine-witnesses. These bounds
    /// are deduced from the well-formedness of the witness's types, and are
    /// necessary because of the way we anonymize the regions in a coroutine,
    /// which may cause types to no longer be considered well-formed.
    region_assumptions: Vec<ArgOutlivesClause<'db>>,

    /// Caches for opaque type inference.
    pub(crate) opaque_type_storage: OpaqueTypeStorage<'db>,
}

impl Clone for InferCtxtInner<'_> {
    fn clone(&self) -> Self {
        let mut inner = InferCtxtInner::empty_unallocated();
        inner.clone_from(self);
        inner
    }

    /// Field by field into `self`'s tables, keeping their capacity: a fork into the worker's
    /// warm tables (`InferCtxt::fork`) allocates nothing.
    fn clone_from(&mut self, source: &Self) {
        let InferCtxtInner {
            undo_log,
            type_variable_storage,
            const_unification_storage,
            const_var_params,
            int_unification_storage,
            float_unification_storage,
            region_constraint_storage,
            region_obligations,
            region_assumptions,
            opaque_type_storage,
        } = self;
        *undo_log = source.undo_log;
        type_variable_storage.clone_from(&source.type_variable_storage);
        const_unification_storage.clone_from(&source.const_unification_storage);
        const_var_params.clone_from(&source.const_var_params);
        int_unification_storage.clone_from(&source.int_unification_storage);
        float_unification_storage.clone_from(&source.float_unification_storage);
        region_constraint_storage.clone_from(&source.region_constraint_storage);
        region_obligations.clone_from(&source.region_obligations);
        region_assumptions.clone_from(&source.region_assumptions);
        opaque_type_storage.clone_from(&source.opaque_type_storage);
    }
}

impl<'db> InferCtxtInner<'db> {
    /// Tables that hold nothing and have allocated nothing. A context takes warm ones from its
    /// worker instead (`next_solver::recycle`).
    pub(crate) fn empty_unallocated() -> InferCtxtInner<'db> {
        InferCtxtInner {
            undo_log: InferCtxtUndoLogs::default(),

            type_variable_storage: Default::default(),
            const_unification_storage: Default::default(),
            // alloc: an empty `Vec` does not allocate.
            const_var_params: Vec::new(),
            int_unification_storage: Default::default(),
            float_unification_storage: Default::default(),
            region_constraint_storage: Some(Default::default()),
            // alloc: an empty `Vec` does not allocate.
            region_obligations: vec![],
            region_assumptions: Default::default(),
            opaque_type_storage: Default::default(),
        }
    }

    /// Back to `empty_unallocated()` in every observable field, each table's capacity kept.
    pub(crate) fn empty(&mut self) {
        let InferCtxtInner {
            undo_log,
            type_variable_storage,
            const_unification_storage,
            const_var_params,
            int_unification_storage,
            float_unification_storage,
            region_constraint_storage,
            region_obligations,
            region_assumptions,
            opaque_type_storage,
        } = self;
        *undo_log = InferCtxtUndoLogs::default();
        type_variable_storage.empty();
        const_unification_storage.empty();
        const_var_params.clear();
        int_unification_storage.empty();
        float_unification_storage.empty();
        match region_constraint_storage {
            Some(storage) => storage.empty(),
            // Taken when regions were resolved.
            None => *region_constraint_storage = Some(Default::default()),
        }
        region_obligations.clear();
        region_assumptions.clear();
        opaque_type_storage.empty();
    }

    /// Bytes its tables hold room for.
    pub(crate) fn held(&self) -> usize {
        self.type_variable_storage.held()
            + self.const_unification_storage.held()
            + self.const_var_params.capacity() * size_of::<(ConstVid, ConstParamId)>()
            + self.int_unification_storage.held()
            + self.float_unification_storage.held()
            + self.region_constraint_storage.as_ref().map_or(0, RegionConstraintStorage::held)
            + self.region_obligations.capacity() * size_of::<TypeOutlivesConstraint<'db>>()
            + self.region_assumptions.capacity() * size_of::<ArgOutlivesClause<'db>>()
            + self.opaque_type_storage.held()
    }

    /// Grows whenever a type, const, int or float variable is instantiated or unified, an
    /// opaque type is registered, or a rollback undoes any of these, and never shrinks. Equal
    /// counts mean equal variable and opaque-type state, so a goal stalled at the first gives
    /// the same answer at the second. Region variables are not counted, as in rustc's
    /// `ty_or_const_infer_var_changed`: the solver's input canonicalization makes every region
    /// a fresh placeholder.
    #[inline]
    pub(crate) fn state_changes(&self) -> u64 {
        self.type_variable_storage.changes()
            + self.const_unification_storage.changes()
            + self.int_unification_storage.changes()
            + self.float_unification_storage.changes()
            + self.opaque_type_storage.changes()
    }

    #[inline]
    pub fn region_obligations(&self) -> &[TypeOutlivesConstraint<'db>] {
        &self.region_obligations
    }

    #[inline]
    fn try_type_variables_probe_ref(
        &self,
        vid: TyVid,
    ) -> Option<&type_variable::TypeVariableValue<'db>> {
        // Reads the table without path halving, so a shared borrow is enough.
        self.type_variable_storage.try_probe_value(vid)
    }

    #[inline]
    fn type_variables(&mut self) -> type_variable::TypeVariableTable<'_, 'db> {
        self.type_variable_storage.table()
    }

    #[inline]
    pub(crate) fn opaque_types(&mut self) -> opaque_types::OpaqueTypeTable<'_, 'db> {
        let in_snapshot = self.undo_log.in_snapshot();
        self.opaque_type_storage.table(in_snapshot)
    }

    #[inline]
    pub(crate) fn int_unification_table(&mut self) -> &mut UnionFind<IntVid, IntVarValue> {
        &mut self.int_unification_storage
    }

    #[inline]
    pub(crate) fn float_unification_table(&mut self) -> &mut UnionFind<FloatVid, FloatVarValue> {
        &mut self.float_unification_storage
    }

    #[inline]
    fn const_unification_table(&mut self) -> &mut UnionFind<ConstVid, ConstVariableValue<'db>> {
        &mut self.const_unification_storage
    }

    #[inline]
    pub fn unwrap_region_constraints(&mut self) -> RegionConstraintCollector<'db, '_> {
        let in_snapshot = self.undo_log.in_snapshot();
        self.region_constraint_storage
            .as_mut()
            .expect("region constraints already solved")
            .collector(in_snapshot)
    }
}

/// What an `InferCtxt` allocates: taken from the worker's shelf when one is built, and put
/// back emptied when it drops (`next_solver::recycle`), so a warm worker builds contexts
/// without allocating.
pub(crate) struct InferTables<'db> {
    pub(crate) inner: InferCtxtInner<'db>,
    pub(crate) canonicalizer: CanonicalizerState<DbInterner<'db>>,
    pub(crate) placeholder_assumptions: FxIndexMap<UniverseIndex, Assumptions<DbInterner<'db>>>,
    pub(crate) next_solver_overflows: Vec<NextSolverOverflow<'db>>,
}

impl Default for InferTables<'_> {
    fn default() -> Self {
        InferTables {
            inner: InferCtxtInner::empty_unallocated(),
            canonicalizer: Default::default(),
            placeholder_assumptions: Default::default(),
            // alloc: an empty `Vec` does not allocate.
            next_solver_overflows: Vec::new(),
        }
    }
}

impl<'db> InferTables<'db> {
    /// Drops every value, keeping each table's capacity.
    pub(crate) fn empty(&mut self) {
        let InferTables { inner, canonicalizer, placeholder_assumptions, next_solver_overflows } =
            self;
        inner.empty();
        canonicalizer.clear();
        placeholder_assumptions.clear();
        next_solver_overflows.clear();
    }

    /// Bytes it holds room for: what a shelf keeps the largest of.
    pub(crate) fn held(&self) -> usize {
        let canonicalizer = &self.canonicalizer;
        self.inner.held()
            + canonicalizer.variables.capacity() * size_of::<GenericArg<'db>>()
            + canonicalizer.var_kinds.capacity()
                * size_of::<rustc_type_ir::CanonicalVarKind<DbInterner<'db>>>()
            + canonicalizer.variable_lookup_table.capacity()
                * size_of::<(GenericArg<'db>, usize)>()
            + canonicalizer.sub_root_lookup_table.capacity() * size_of::<(TyVid, usize)>()
            + self.placeholder_assumptions.capacity()
                * (size_of::<(UniverseIndex, Assumptions<DbInterner<'db>>)>() + size_of::<usize>())
            + self.next_solver_overflows.capacity() * size_of::<NextSolverOverflow<'db>>()
    }
}

pub struct InferCtxt<'db> {
    pub interner: DbInterner<'db>,

    /// The mode of this inference context, see the struct documentation
    /// for more details.
    typing_mode: TypingMode<'db>,

    pub inner: RefCell<InferCtxtInner<'db>>,

    /// When an error occurs, we want to avoid reporting "derived"
    /// errors that are due to this original failure. We have this
    /// flag that one can set whenever one creates a type-error that
    /// is due to an error in a prior pass.
    ///
    /// Don't read this flag directly, call `is_tainted_by_errors()`
    /// and `set_tainted_by_errors()`.
    tainted_by_errors: Cell<Option<ErrorGuaranteed>>,

    /// What is the innermost universe we have created? Starts out as
    /// `UniverseIndex::root()` but grows from there as we enter
    /// universal quantifiers.
    ///
    /// N.B., at present, we exclude the universal quantifiers on the
    /// item we are type-checking, and just consider those names as
    /// part of the root universe. So this would only get incremented
    /// when we enter into a higher-ranked (`for<..>`) type or trait
    /// bound.
    universe: Cell<UniverseIndex>,

    /// The assumed well-formedness facts of each universe entered for a binder, from which the
    /// solver derives the implied bounds of a `for<...>` (rustc's
    /// `placeholder_assumptions_for_next_solver`). Like `universe`, not rolled back by snapshots.
    placeholder_assumptions: RefCell<FxIndexMap<UniverseIndex, Assumptions<DbInterner<'db>>>>,

    /// Whether a root goal that overflows is evaluated again with twice the recursion limit,
    /// reporting rustc's `recursion_depth_exceeding_limit` warning when that holds. Off while such
    /// a warning is being built, so it does not trigger itself.
    enable_next_solver_overflow_fcw: Cell<bool>,

    /// The goals that held only with the doubled recursion limit, for the caller to report.
    next_solver_overflows: RefCell<Vec<NextSolverOverflow<'db>>>,

    obligation_inspector: Cell<Option<ObligationInspector<'db>>>,

    /// Reused by each new canonicalizer, then cleared but not freed, so canonicalizing does not
    /// allocate its tables anew each time.
    pub(crate) canonicalizer_state: RefCell<CanonicalizerState<DbInterner<'db>>>,
}

impl<'db> Clone for InferCtxt<'db> {
    fn clone(&self) -> Self {
        self.fork()
    }
}

/// See the `error_reporting` module for more details.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValuePairs<'db> {
    Regions(ExpectedFound<Region<'db>>),
    Terms(ExpectedFound<Term<'db>>),
    Aliases(ExpectedFound<AliasTerm<'db>>),
    TraitRefs(ExpectedFound<TraitRef<'db>>),
    PolySigs(ExpectedFound<PolyFnSig<'db>>),
    ExistentialTraitRef(ExpectedFound<PolyExistentialTraitRef<'db>>),
    ExistentialProjection(ExpectedFound<PolyExistentialProjection<'db>>),
}

impl<'db> ValuePairs<'db> {
    pub fn ty(&self) -> Option<(Ty<'db>, Ty<'db>)> {
        if let ValuePairs::Terms(ExpectedFound { expected, found }) = self
            && let Some(expected) = expected.as_type()
            && let Some(found) = found.as_type()
        {
            return Some((expected, found));
        }
        None
    }
}

/// The trace designates the path through inference that we took to
/// encounter an error or subtyping constraint.
///
/// See the `error_reporting` module for more details.
#[derive(Clone, Debug)]
pub struct TypeTrace<'db> {
    pub cause: ObligationCause,
    pub values: ValuePairs<'db>,
}

/// Times when we replace bound regions with existentials:
#[derive(Clone, Copy, Debug)]
pub enum BoundRegionConversionTime<'db> {
    /// when a fn is called
    FnCall,

    /// when two higher-ranked types are compared
    HigherRankedType,

    /// when projecting an associated type
    AssocTypeProjection(SolverDefId<'db>),
}

/// See the `region_obligations` field for more information.
#[derive(Clone, Debug)]
pub struct TypeOutlivesConstraint<'db> {
    pub sub_region: Region<'db>,
    pub sup_type: Ty<'db>,
}

/// Used to configure inference contexts before their creation.
pub struct InferCtxtBuilder<'db> {
    interner: DbInterner<'db>,
    enable_next_solver_overflow_fcw: bool,
}

pub trait DbInternerInferExt<'db> {
    fn infer_ctxt(self) -> InferCtxtBuilder<'db>;
}

impl<'db> DbInternerInferExt<'db> for DbInterner<'db> {
    fn infer_ctxt(self) -> InferCtxtBuilder<'db> {
        InferCtxtBuilder { interner: self, enable_next_solver_overflow_fcw: true }
    }
}

impl<'db> InferCtxtBuilder<'db> {
    /// Coherence turns this off: a spurious answer there must not change with the limit.
    pub fn enable_next_solver_overflow_fcw(mut self, enable_next_solver_overflow_fcw: bool) -> Self {
        self.enable_next_solver_overflow_fcw = enable_next_solver_overflow_fcw;
        self
    }

    /// Given a canonical value `C` as a starting point, create an
    /// inference context that contains each of the bound values
    /// within instantiated as a fresh variable. The `f` closure is
    /// invoked with the new infcx, along with the instantiated value
    /// `V` and a instantiation `S`. This instantiation `S` maps from
    /// the bound values in `C` to their instantiated values in `V`
    /// (in other words, `S(C) = V`).
    pub fn build_with_canonical<T>(
        mut self,
        span: Span,
        input: &CanonicalQueryInput<'db, T>,
    ) -> (InferCtxt<'db>, T, CanonicalVarValues<'db>)
    where
        T: TypeFoldable<DbInterner<'db>>,
    {
        let infcx = self.build(input.typing_mode.0);
        let (value, args) = infcx.instantiate_canonical(span, &input.canonical);
        (infcx, value, args)
    }

    pub fn build(&mut self, typing_mode: TypingMode<'db>) -> InferCtxt<'db> {
        // Solving needs the crate's trait environment and solver cache.
        self.interner.expect_crate();
        let InferCtxtBuilder { interner, enable_next_solver_overflow_fcw } = *self;
        let InferTables { inner, canonicalizer, placeholder_assumptions, next_solver_overflows } =
            recycle::infer_tables();
        InferCtxt {
            interner,
            typing_mode,
            inner: RefCell::new(inner),
            tainted_by_errors: Cell::new(None),
            universe: Cell::new(UniverseIndex::ROOT),
            placeholder_assumptions: RefCell::new(placeholder_assumptions),
            enable_next_solver_overflow_fcw: Cell::new(enable_next_solver_overflow_fcw),
            next_solver_overflows: RefCell::new(next_solver_overflows),
            obligation_inspector: Cell::new(None),
            canonicalizer_state: RefCell::new(canonicalizer),
        }
    }
}

impl Drop for InferCtxt<'_> {
    fn drop(&mut self) {
        // A context dropped by an unwind may be mid-change; its tables are freed, not kept.
        if std::thread::panicking() {
            return;
        }
        recycle::give_infer_tables(InferTables {
            inner: mem::replace(self.inner.get_mut(), InferCtxtInner::empty_unallocated()),
            canonicalizer: mem::take(self.canonicalizer_state.get_mut()),
            placeholder_assumptions: mem::take(self.placeholder_assumptions.get_mut()),
            next_solver_overflows: mem::take(self.next_solver_overflows.get_mut()),
        });
    }
}

impl<'db> InferOk<'db, ()> {
    pub fn into_obligations(self) -> PredicateObligations<'db> {
        self.obligations
    }
}

impl<'db> InferCtxt<'db> {
    #[inline(always)]
    pub fn typing_mode_raw(&self) -> TypingMode<'db> {
        self.typing_mode
    }

    #[inline(always)]
    pub fn typing_mode_unchecked(&self) -> TypingMode<'db> {
        self.typing_mode
    }

    /// Evaluates whether the predicate can be satisfied (by any means)
    /// in the given `ParamEnv`.
    pub fn predicate_may_hold(&self, obligation: &PredicateObligation<'db>) -> bool {
        self.evaluate_obligation(obligation).may_apply()
    }

    /// See the comment on `GeneralAutoderef::overloaded_deref_ty`
    /// for more details.
    pub fn predicate_may_hold_opaque_types_jank(
        &self,
        obligation: &PredicateObligation<'db>,
    ) -> bool {
        <&SolverContext<'db>>::from(self).root_goal_may_hold_opaque_types_jank(Goal::new(
            self.interner,
            obligation.param_env,
            obligation.predicate,
        ))
    }

    pub(crate) fn insert_type_vars<T>(&self, ty: T) -> T
    where
        T: TypeFoldable<DbInterner<'db>>,
    {
        struct Folder<'a, 'db> {
            infcx: &'a InferCtxt<'db>,
        }
        impl<'db> TypeFolder<DbInterner<'db>> for Folder<'_, 'db> {
            fn cx(&self) -> DbInterner<'db> {
                self.infcx.interner
            }

            fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
                if !ty.references_error() {
                    return ty;
                }

                if ty.is_ty_error() {
                    self.infcx.next_ty_var(Span::Dummy)
                } else {
                    ty.super_fold_with(self)
                }
            }

            fn fold_const(&mut self, ct: Const<'db>) -> Const<'db> {
                if !ct.references_error() {
                    return ct;
                }

                if ct.is_ct_error() {
                    self.infcx.next_const_var(Span::Dummy)
                } else {
                    ct.super_fold_with(self)
                }
            }

            fn fold_region(&mut self, r: Region<'db>) -> Region<'db> {
                if r.is_error() { self.infcx.next_region_var(Span::Dummy) } else { r }
            }
        }

        ty.fold_with(&mut Folder { infcx: self })
    }

    /// Evaluates whether the predicate can be satisfied in the given
    /// `ParamEnv`, and returns `false` if not certain. However, this is
    /// not entirely accurate if inference variables are involved.
    ///
    /// This version may conservatively fail when outlives obligations
    /// are required. Therefore, this version should only be used for
    /// optimizations or diagnostics and be treated as if it can always
    /// return `false`.
    ///
    /// # Example
    ///
    /// ```
    /// # #![allow(dead_code)]
    /// trait Trait {}
    ///
    /// fn check<T: Trait>() {}
    ///
    /// fn foo<T: 'static>()
    /// where
    ///     &'static T: Trait,
    /// {
    ///     // Evaluating `&'?0 T: Trait` adds a `'?0: 'static` outlives obligation,
    ///     // which means that `predicate_must_hold_considering_regions` will return
    ///     // `false`.
    ///     check::<&'_ T>();
    /// }
    /// ```
    pub fn predicate_must_hold_considering_regions(
        &self,
        obligation: &PredicateObligation<'db>,
    ) -> bool {
        self.evaluate_obligation(obligation).must_apply_considering_regions()
    }

    /// Evaluates whether the predicate can be satisfied in the given
    /// `ParamEnv`, and returns `false` if not certain. However, this is
    /// not entirely accurate if inference variables are involved.
    ///
    /// This version ignores all outlives constraints.
    pub fn predicate_must_hold_modulo_regions(
        &self,
        obligation: &PredicateObligation<'db>,
    ) -> bool {
        self.evaluate_obligation(obligation).must_apply_modulo_regions()
    }

    /// Check whether a `ty` implements given trait(trait_def_id) without side-effects.
    ///
    /// The inputs are:
    ///
    /// - the def-id of the trait
    /// - the type parameters of the trait, including the self-type
    /// - the parameter environment
    ///
    /// Invokes `evaluate_obligation`, so in the event that evaluating
    /// `Ty: Trait` causes overflow, EvaluatedToAmbigStackDependent will be returned.
    ///
    /// `type_implements_trait` is a convenience function for simple cases like
    ///
    /// ```ignore (illustrative)
    /// let copy_trait = infcx.tcx.require_lang_item(LangItem::Copy, span);
    /// let implements_copy = infcx.type_implements_trait(copy_trait, [ty], param_env)
    /// .must_apply_modulo_regions();
    /// ```
    ///
    /// In most cases you should instead create an [Obligation] and check whether
    ///  it holds via [`evaluate_obligation`] or one of its helper functions like
    /// [`predicate_must_hold_modulo_regions`], because it properly handles higher ranked traits
    /// and it is more convenient and safer when your `params` are inside a [`Binder`].
    ///
    /// [Obligation]: traits::Obligation
    /// [`evaluate_obligation`]: InferCtxt::evaluate_obligation
    /// [`predicate_must_hold_modulo_regions`]: InferCtxt::predicate_must_hold_modulo_regions
    /// [`Binder`]: rustc_type_ir::Binder
    #[instrument(level = "debug", skip(self, params), ret)]
    pub fn type_implements_trait(
        &self,
        trait_def_id: TraitId,
        params: impl IntoIterator<Item: Into<GenericArg<'db>>>,
        param_env: ParamEnv<'db>,
    ) -> EvaluationResult {
        let trait_ref = TraitRef::new(self.interner, trait_def_id.into(), params);

        let obligation = traits::Obligation {
            cause: traits::ObligationCause::dummy(),
            param_env,
            recursion_depth: 0,
            predicate: trait_ref.upcast(self.interner),
        };
        self.evaluate_obligation(&obligation)
    }

    /// Evaluate a given predicate, capturing overflow and propagating it back.
    fn evaluate_obligation(&self, obligation: &PredicateObligation<'db>) -> EvaluationResult {
        self.probe(|snapshot| {
            let mut ocx = ObligationCtxt::new(self);
            ocx.register_obligation(*obligation);
            let mut result = EvaluationResult::EvaluatedToOk;
            ocx.evaluate_obligations_error_on_ambiguity_with(&mut |error| {
                result = result.max(if error.is_true_error() {
                    EvaluationResult::EvaluatedToErr
                } else {
                    EvaluationResult::EvaluatedToAmbig
                });
            });
            if result == EvaluationResult::EvaluatedToErr {
                return result;
            }
            if self.opaque_types_added_in_snapshot(snapshot) {
                result = result.max(EvaluationResult::EvaluatedToOkModuloOpaqueTypes);
            } else if self.region_constraints_added_in_snapshot(snapshot) {
                result = result.max(EvaluationResult::EvaluatedToOkModuloRegions);
            }
            result
        })
    }

    pub fn can_eq<T: ToTrace<'db>>(&self, param_env: ParamEnv<'db>, a: T, b: T) -> bool {
        self.probe(|_| {
            let mut ocx = ObligationCtxt::new(self);
            let Ok(()) = ocx.eq(&ObligationCause::dummy(), param_env, a, b) else {
                return false;
            };
            ocx.try_evaluate_obligations().is_empty()
        })
    }

    /// See the comment on `GeneralAutoderef::overloaded_deref_ty`
    /// for more details.
    pub fn goal_may_hold_opaque_types_jank(&self, goal: Goal<'db, Predicate<'db>>) -> bool {
        <&SolverContext<'db>>::from(self).root_goal_may_hold_opaque_types_jank(goal)
    }

    pub fn type_is_copy_modulo_regions(&self, param_env: ParamEnv<'db>, ty: Ty<'db>) -> bool {
        let ty = self.deeply_resolve_ignoring_regions(ty);

        let Some(copy_def_id) = self.interner.lang_items().Copy else {
            return false;
        };

        // This can get called from typeck (by euv), and `moves_by_default`
        // rightly refuses to work with inference variables, but
        // moves_by_default has a cache, which we want to use in other
        // cases.
        traits::type_known_to_meet_bound_modulo_regions(self, param_env, ty, copy_def_id)
    }

    pub fn type_is_sized_modulo_regions(&self, param_env: ParamEnv<'db>, ty: Ty<'db>) -> bool {
        let Some(sized_def_id) = self.interner.lang_items().Sized else {
            return true;
        };
        traits::type_known_to_meet_bound_modulo_regions(self, param_env, ty, sized_def_id)
    }

    pub fn type_is_use_cloned_modulo_regions(&self, param_env: ParamEnv<'db>, ty: Ty<'db>) -> bool {
        let ty = self.deeply_resolve_ignoring_regions(ty);

        let Some(use_cloned_def_id) = self.interner.lang_items().UseCloned else {
            return false;
        };

        traits::type_known_to_meet_bound_modulo_regions(self, param_env, ty, use_cloned_def_id)
    }

    /// The root of each set of type, integer and float variables that is not yet known.
    pub fn unresolved_root_variables(&self) -> (Vec<TyVid>, Vec<IntVid>, Vec<FloatVid>) {
        let mut inner = self.inner.borrow_mut();
        let ty = inner.type_variables().unresolved_root_variables();
        let int = inner.int_unification_storage.roots_where(IntVarValue::is_unknown);
        let float = inner.float_unification_storage.roots_where(FloatVarValue::is_unknown);
        (ty, int, float)
    }

    #[instrument(skip(self), level = "debug")]
    pub fn sub_regions(&self, a: Region<'db>, b: Region<'db>) {
        self.inner.borrow_mut().unwrap_region_constraints().make_subregion(
            a,
            b,
            VisibleForLeakCheck::Yes,
        );
    }

    /// Processes a `Coerce` predicate from the fulfillment context.
    /// This is NOT the preferred way to handle coercion, which is to
    /// invoke `FnCtxt::coerce` or a similar method (see `coercion.rs`).
    ///
    /// This method here is actually a fallback that winds up being
    /// invoked when `FnCtxt::coerce` encounters unresolved type variables
    /// and records a coercion predicate. Presently, this method is equivalent
    /// to `subtype_predicate` -- that is, "coercing" `a` to `b` winds up
    /// actually requiring `a <: b`. This is of course a valid coercion,
    /// but it's not as flexible as `FnCtxt::coerce` would be.
    ///
    /// (We may refactor this in the future, but there are a number of
    /// practical obstacles. Among other things, `FnCtxt::coerce` presently
    /// records adjustments that are required on the HIR in order to perform
    /// the coercion, and we don't currently have a way to manage that.)
    pub fn coerce_predicate(
        &self,
        cause: &ObligationCause,
        param_env: ParamEnv<'db>,
        predicate: PolyCoercePredicate<'db>,
    ) -> Result<InferResult<'db, ()>, (TyVid, TyVid)> {
        let subtype_predicate = predicate.map_bound(|p| SubtypePredicate {
            a_is_expected: false, // when coercing from `a` to `b`, `b` is expected
            a: p.a,
            b: p.b,
        });
        self.subtype_predicate(cause, param_env, subtype_predicate)
    }

    pub fn subtype_predicate(
        &self,
        cause: &ObligationCause,
        param_env: ParamEnv<'db>,
        predicate: PolySubtypePredicate<'db>,
    ) -> Result<InferResult<'db, ()>, (TyVid, TyVid)> {
        // Check for two unresolved inference variables, in which case we can
        // make no progress. This is partly a micro-optimization, but it's
        // also an opportunity to "sub-unify" the variables. This isn't
        // *necessary* to prevent cycles, because they would eventually be sub-unified
        // anyhow during generalization, but it helps with diagnostics (we can detect
        // earlier that they are sub-unified).
        //
        // Note that we can just skip the binders here because
        // type variables can't (at present, at
        // least) capture any of the things bound by this binder.
        //
        // Note that this sub here is not just for diagnostics - it has semantic
        // effects as well.
        let r_a = self.shallow_resolve(predicate.skip_binder().a);
        let r_b = self.shallow_resolve(predicate.skip_binder().b);
        match (r_a.kind(), r_b.kind()) {
            (TyKind::Infer(InferTy::TyVar(a_vid)), TyKind::Infer(InferTy::TyVar(b_vid))) => {
                return Err((a_vid, b_vid));
            }
            _ => {}
        }

        self.enter_forall(predicate, |SubtypePredicate { a_is_expected, a, b }| {
            if a_is_expected {
                Ok(self.sub_raw(cause, param_env, a, b))
            } else {
                Ok(self.sup_raw(cause, param_env, b, a))
            }
        })
    }

    pub fn region_outlives_predicate(
        &self,
        _cause: &traits::ObligationCause,
        predicate: PolyRegionOutlivesClause<'db>,
    ) {
        self.enter_forall(predicate, |OutlivesClause(r_a, r_b)| {
            self.sub_regions(r_b, r_a); // `b : a` ==> `a <= b`
        })
    }

    /// Number of type variables created so far.
    pub fn num_ty_vars(&self) -> usize {
        self.inner.borrow_mut().type_variables().num_vars()
    }

    pub fn next_ty_var(&self, span: Span) -> Ty<'db> {
        let vid = self.next_ty_vid(span);
        Ty::new_var(self.interner, vid)
    }

    pub fn next_ty_vid(&self, span: Span) -> TyVid {
        self.next_ty_var_id_in_universe(self.universe(), span)
    }

    pub fn next_ty_var_id_in_universe(&self, universe: UniverseIndex, span: Span) -> TyVid {
        self.inner.borrow_mut().type_variables().new_var(universe, span)
    }

    pub fn next_ty_var_in_universe(&self, universe: UniverseIndex, span: Span) -> Ty<'db> {
        let vid = self.next_ty_var_id_in_universe(universe, span);
        Ty::new_var(self.interner, vid)
    }

    pub(crate) fn next_ty_var_for_param(
        &self,
        span: Span,
        param: Option<TypeParamId>,
    ) -> Ty<'db> {
        let universe = self.universe();
        let vid =
            self.inner.borrow_mut().type_variables().new_var_for_param(universe, span, param);
        Ty::new_var(self.interner, vid)
    }

    pub(crate) fn next_const_var_for_param(
        &self,
        span: Span,
        param: Option<ConstParamId>,
    ) -> Const<'db> {
        let vid = self.next_const_vid(span);
        if let Some(param) = param {
            self.inner.borrow_mut().const_var_params.push((vid, param));
        }
        Const::new_var(self.interner, vid)
    }

    pub fn next_const_var(&self, span: Span) -> Const<'db> {
        let vid = self.next_const_vid(span);
        Const::new_var(self.interner, vid)
    }

    pub fn next_const_vid(&self, span: Span) -> ConstVid {
        self.next_const_vid_in_universe(self.universe(), span)
    }

    pub fn next_const_vid_in_universe(&self, universe: UniverseIndex, span: Span) -> ConstVid {
        self.inner
            .borrow_mut()
            .const_unification_table()
            .new_key(ConstVariableValue::Unknown { span, universe })
    }

    pub fn next_const_var_in_universe(&self, universe: UniverseIndex, span: Span) -> Const<'db> {
        let vid = self.next_const_vid_in_universe(universe, span);
        Const::new_var(self.interner, vid)
    }

    pub fn next_int_var(&self) -> Ty<'db> {
        let vid = self.next_int_vid();
        Ty::new_int_var(self.interner, vid)
    }

    pub fn next_int_vid(&self) -> IntVid {
        self.inner.borrow_mut().int_unification_table().new_key(IntVarValue::Unknown)
    }

    pub fn next_float_var(&self) -> Ty<'db> {
        Ty::new_float_var(self.interner, self.next_float_vid())
    }

    pub fn next_float_vid(&self) -> FloatVid {
        self.inner.borrow_mut().float_unification_table().new_key(FloatVarValue::Unknown)
    }

    /// Creates a fresh region variable with the next available index.
    /// The variable will be created in the maximum universe created
    /// thus far, allowing it to name any region created thus far.
    pub fn next_region_var(&self, span: Span) -> Region<'db> {
        self.next_region_var_in_universe(self.universe(), span)
    }

    pub fn next_region_vid(&self, span: Span) -> RegionVid {
        self.inner.borrow_mut().unwrap_region_constraints().new_region_var(self.universe(), span)
    }

    /// Creates a fresh region variable with the next available index
    /// in the given universe; typically, you can use
    /// `next_region_var` and just use the maximal universe.
    pub fn next_region_var_in_universe(&self, universe: UniverseIndex, span: Span) -> Region<'db> {
        let region_var =
            self.inner.borrow_mut().unwrap_region_constraints().new_region_var(universe, span);
        Region::new_var(self.interner, region_var)
    }

    pub fn next_term_var_of_alias_kind(&self, alias_term: AliasTerm<'db>, span: Span) -> Term<'db> {
        match alias_term.kind {
            AliasTermKind::ProjectionTy { .. }
            | AliasTermKind::InherentTy { .. }
            | AliasTermKind::OpaqueTy { .. }
            | AliasTermKind::FreeTy { .. } => self.next_ty_var(span).into(),
            AliasTermKind::FreeConst { .. }
            | AliasTermKind::InherentConstSelf { .. }
            | AliasTermKind::InherentConstImpl { .. }
            | AliasTermKind::AnonConst { .. }
            | AliasTermKind::ProjectionConst { .. } => self.next_const_var(span).into(),
        }
    }

    /// Return the universe that the region `r` was created in. For
    /// most regions (e.g., `'static`, named regions from the user,
    /// etc) this is the root universe U0. For inference variables or
    /// placeholders, however, it will return the universe which they
    /// are associated.
    pub fn universe_of_region(&self, r: Region<'db>) -> UniverseIndex {
        self.inner.borrow_mut().unwrap_region_constraints().universe(r)
    }

    /// Number of region variables created so far.
    pub fn num_region_vars(&self) -> usize {
        self.inner.borrow_mut().unwrap_region_constraints().num_region_vars()
    }

    pub fn var_for_def(&self, id: GenericParamId, span: Span) -> GenericArg<'db> {
        match id {
            GenericParamId::LifetimeParamId(_) => {
                // Create a region inference variable for the given
                // region parameter definition.
                self.next_region_var(span).into()
            }
            GenericParamId::TypeParamId(param) => {
                // Create a type inference variable for the given
                // type parameter definition. The generic parameters are
                // for actual parameters that may be referred to by
                // the default of this type parameter, if it exists.
                // e.g., `struct Foo<A, B, C = (A, B)>(...);` when
                // used in a path such as `Foo::<T, U>::new()` will
                // use an inference variable for `C` with `[T, U]`
                // as the generic parameters for the default, `(T, U)`.
                self.next_ty_var_for_param(span, Some(param)).into()
            }
            GenericParamId::ConstParamId(param) => {
                self.next_const_var_for_param(span, Some(param)).into()
            }
        }
    }

    /// Given a set of generics defined on a type or impl, returns the generic parameters mapping
    /// each type/region parameter to a fresh inference variable.
    pub fn fresh_args_for_item(&self, span: Span, def_id: SolverDefId<'db>) -> GenericArgs<'db> {
        GenericArgs::for_item(self.interner, def_id, |_index, kind, _, _| {
            self.var_for_def(kind, span)
        })
    }

    /// Like [`Self::fresh_args_for_item`], but first uses the args from `first`.
    pub fn fill_rest_fresh_args(
        &self,
        span: Span,
        def_id: SolverDefId<'db>,
        first: impl IntoIterator<Item = GenericArg<'db>>,
    ) -> GenericArgs<'db> {
        GenericArgs::fill_rest(self.interner, def_id, first, |_index, kind, _| {
            self.var_for_def(kind, span)
        })
    }

    /// Returns `true` if errors have been reported since this infcx was
    /// created. This is sometimes used as a heuristic to skip
    /// reporting errors that often occur as a result of earlier
    /// errors, but where it's hard to be 100% sure (e.g., unresolved
    /// inference variables, regionck errors).
    #[must_use = "this method does not have any side effects"]
    pub fn tainted_by_errors(&self) -> Option<ErrorGuaranteed> {
        self.tainted_by_errors.get()
    }

    /// Set the "tainted by errors" flag to true. We call this when we
    /// observe an error from a prior pass.
    pub fn set_tainted_by_errors(&self, e: ErrorGuaranteed) {
        debug!("set_tainted_by_errors(ErrorGuaranteed)");
        self.tainted_by_errors.set(Some(e));
    }

    #[instrument(level = "debug", skip(self))]
    pub fn take_opaque_types(
        &self,
    ) -> impl IntoIterator<Item = (OpaqueTypeKey<'db>, OpaqueHiddenType<'db>)> + use<'db> {
        self.inner.borrow_mut().opaque_type_storage.take_opaque_types()
    }

    #[instrument(level = "debug", skip(self), ret)]
    pub fn clone_opaque_types(&self) -> Vec<(OpaqueTypeKey<'db>, OpaqueHiddenType<'db>)> {
        // alloc: once per body, for the opaque-type pass; none without opaque types.
        self.inner.borrow_mut().opaque_type_storage.iter_opaque_types().collect()
    }

    pub fn has_opaques_with_sub_unified_hidden_type(&self, ty_vid: TyVid) -> bool {
        let ty_sub_vid = self.sub_unification_table_root_var(ty_vid);
        let inner = &mut *self.inner.borrow_mut();
        let mut type_variables = inner.type_variable_storage.table();
        inner.opaque_type_storage.iter_opaque_types().any(|(_, hidden_ty)| {
            if let TyKind::Infer(InferTy::TyVar(hidden_vid)) = hidden_ty.ty.kind() {
                let opaque_sub_vid = type_variables.sub_unification_table_root_var(hidden_vid);
                if opaque_sub_vid == ty_sub_vid {
                    return true;
                }
            }

            false
        })
    }

    #[inline(always)]
    pub fn can_define_opaque_ty(&self, id: impl Into<SolverDefId<'db>>) -> bool {
        match self.typing_mode_raw().assert_not_erased() {
            TypingMode::Typeck { defining_opaque_types_and_generators: defining_opaque_types }
            | TypingMode::PostTypeckThroughBorrowck { defining_opaque_types } => {
                defining_opaque_types.contains(&id.into())
            }
            TypingMode::Coherence
            | TypingMode::Reflection
            | TypingMode::PostBorrowck { .. }
            | TypingMode::PostAnalysis
            | TypingMode::Codegen => false,
        }
    }

    /// If `TyVar(vid)` resolves to a type, return that type. Else, return the
    /// universe index of `TyVar(vid)`.
    pub fn try_resolve_ty_var(&self, vid: TyVid) -> Result<Ty<'db>, UniverseIndex> {
        use self::type_variable::TypeVariableValue;

        match self.inner.borrow_mut().type_variables().probe(vid) {
            TypeVariableValue::Known { value, .. } => Ok(value),
            TypeVariableValue::Unknown { universe, .. } => Err(universe),
        }
    }

    /// If `vid` resolves to a type, return that type. Otherwise return the root variable id for `vid`.
    pub fn shallow_resolve_ty_var_or_get_root(&self, vid: TyVid) -> Result<Ty<'db>, TyVid> {
        let (root, value) = self.inner.borrow_mut().type_variables().probe_with_root_vid(vid);
        value.known().ok_or(root)
    }

    /// Resolves a type one level:
    ///
    /// - a rigid type is returned as is;
    /// - a type, int or float variable equated with a type returns that type;
    /// - a variable equated only with other variables returns the root of their set, so the
    ///   same set always gives the same variable (which caches rely on);
    /// - anything else is returned as is.
    ///
    /// Types inside the result may still contain variables; see
    /// [`Self::deeply_resolve_ignoring_regions`].
    pub fn shallow_resolve(&self, ty: Ty<'db>) -> Ty<'db> {
        if let TyKind::Infer(v) = ty.kind() {
            match v {
                InferTy::TyVar(v) => {
                    // Not entirely obvious: if `typ` is a type variable,
                    // it can be resolved to an int/float variable, which
                    // can then be recursively resolved, hence the
                    // recursion. Note though that we prevent type
                    // variables from unifying to other type variables
                    // directly (though they may be embedded
                    // structurally), and we prevent cycles in any case,
                    // so this recursion should always be of very limited
                    // depth.
                    //
                    // Note: if these two lines are combined into one we get
                    // dynamic borrow errors on `self.inner`.
                    let (root_vid, value) =
                        self.inner.borrow_mut().type_variables().probe_with_root_vid(v);
                    value.known().map_or_else(
                        || if root_vid == v { ty } else { Ty::new_var(self.interner, root_vid) },
                        |t| self.shallow_resolve(t),
                    )
                }

                InferTy::IntVar(v) => {
                    let (root, value) =
                        self.inner.borrow_mut().int_unification_table().inlined_probe_key_value(v);
                    match value {
                        IntVarValue::IntType(ty) => Ty::new_int(self.interner, ty),
                        IntVarValue::UintType(ty) => Ty::new_uint(self.interner, ty),
                        IntVarValue::Unknown => {
                            if root == v {
                                ty
                            } else {
                                Ty::new_int_var(self.interner, root)
                            }
                        }
                    }
                }

                InferTy::FloatVar(v) => {
                    let (root, value) = self
                        .inner
                        .borrow_mut()
                        .float_unification_table()
                        .inlined_probe_key_value(v);
                    match value {
                        FloatVarValue::Known(ty) => Ty::new_float(self.interner, ty),
                        FloatVarValue::Unknown => {
                            if root == v {
                                ty
                            } else {
                                Ty::new_float_var(self.interner, root)
                            }
                        }
                    }
                }

                InferTy::FreshTy(_) | InferTy::FreshIntTy(_) | InferTy::FreshFloatTy(_) => ty,
            }
        } else {
            ty
        }
    }

    /// [`Self::shallow_resolve`] for consts.
    pub fn shallow_resolve_const(&self, ct: Const<'db>) -> Const<'db> {
        match ct.kind() {
            ConstKind::Infer(infer_ct) => match infer_ct {
                InferConst::Var(vid) => {
                    let (root, value) =
                        self.inner.borrow_mut().const_unification_table().inlined_probe_key_value(vid);
                    value.known().unwrap_or_else(|| {
                        if root == vid { ct } else { Const::new_var(self.interner, root) }
                    })
                }
                InferConst::Fresh(_) => ct,
            },
            ConstKind::Param(_)
            | ConstKind::Bound(_, _)
            | ConstKind::Placeholder(_)
            | ConstKind::Alias(_, _)
            | ConstKind::Value(_)
            | ConstKind::Error(_)
            | ConstKind::Expr(_) => ct,
        }
    }

    pub fn shallow_resolve_term(&self, term: Term<'db>) -> Term<'db> {
        match term.kind() {
            TermKind::Ty(ty) => self.shallow_resolve(ty).into(),
            TermKind::Const(ct) => self.shallow_resolve_const(ct).into(),
        }
    }

    pub fn root_var(&self, var: TyVid) -> TyVid {
        self.inner.borrow_mut().type_variables().root_var(var)
    }

    pub fn root_const_var(&self, var: ConstVid) -> ConstVid {
        self.inner.borrow_mut().const_unification_table().find(var)
    }

    /// If `ty` is an unresolved type variable, returns its root vid.
    pub fn root_vid(&self, ty: Ty<'db>) -> Option<TyVid> {
        let TyKind::Infer(InferTy::TyVar(vid)) = ty.kind() else { return None };
        let (root, value) = self.inner.borrow_mut().type_variables().inlined_probe_with_vid(vid);
        value.is_unknown().then_some(root)
    }

    /// Resolves a const var to a rigid const, if it was constrained to one,
    /// or else the root const var in the unification table.
    pub fn shallow_resolve_const_var(&self, vid: ConstVid) -> Const<'db> {
        match self.try_resolve_const_var(vid) {
            Ok(ct) => ct,
            Err(_) => Const::new_var(self.interner, self.root_const_var(vid)),
        }
    }

    /// Resolves a type var to a rigid type, if it was constrained to one,
    /// or else the root type var in the unification table.
    pub fn shallow_resolve_ty_var(&self, vid: TyVid) -> Ty<'db> {
        match self.try_resolve_ty_var(vid) {
            Ok(ty) => ty,
            Err(_) => Ty::new_var(self.interner, self.root_var(vid)),
        }
    }

    /// Resolves an int var to a rigid int type, if it was constrained to one,
    /// or else the root int var in the unification table.
    pub fn shallow_resolve_int_var(&self, vid: IntVid) -> Ty<'db> {
        let mut inner = self.inner.borrow_mut();
        let value = inner.int_unification_table().probe_value(vid);
        match value {
            IntVarValue::IntType(ty) => Ty::new_int(self.interner, ty),
            IntVarValue::UintType(ty) => Ty::new_uint(self.interner, ty),
            IntVarValue::Unknown => {
                Ty::new_int_var(self.interner, inner.int_unification_table().find(vid))
            }
        }
    }

    pub fn resolve_int_var(&self, vid: IntVid) -> Option<Ty<'db>> {
        let mut inner = self.inner.borrow_mut();
        let value = inner.int_unification_table().probe_value(vid);
        match value {
            IntVarValue::IntType(ty) => Some(Ty::new_int(self.interner, ty)),
            IntVarValue::UintType(ty) => Some(Ty::new_uint(self.interner, ty)),
            IntVarValue::Unknown => None,
        }
    }

    /// Resolves a float var to a rigid float type, if it was constrained to one,
    /// or else the root float var in the unification table.
    pub fn shallow_resolve_float_var(&self, vid: FloatVid) -> Ty<'db> {
        let mut inner = self.inner.borrow_mut();
        let value = inner.float_unification_table().probe_value(vid);
        match value {
            FloatVarValue::Known(ty) => Ty::new_float(self.interner, ty),
            FloatVarValue::Unknown => {
                Ty::new_float_var(self.interner, inner.float_unification_table().find(vid))
            }
        }
    }

    pub fn resolve_float_var(&self, vid: FloatVid) -> Option<Ty<'db>> {
        let mut inner = self.inner.borrow_mut();
        let value = inner.float_unification_table().probe_value(vid);
        match value {
            FloatVarValue::Known(ty) => Some(Ty::new_float(self.interner, ty)),
            FloatVarValue::Unknown => None,
        }
    }

    /// Where possible, replaces type/const variables in `value` with their final value.
    /// If a type/const variable has not (yet) been unified, it is left as is.
    ///
    /// This is an idempotent operation that does not affect inference state in any way,
    /// which means it's safe to call this function at will.
    ///
    /// Region variables are unaffected.
    pub fn deeply_resolve_ignoring_regions<T>(&self, value: T) -> T
    where
        T: TypeFoldable<DbInterner<'db>>,
    {
        if let Err(guar) = value.error_reported() {
            self.set_tainted_by_errors(guar);
        }
        if !value.has_non_region_infer() {
            return value;
        }
        let mut r = resolve::DeepResolverIgnoringRegions::new(self);
        value.fold_with(&mut r)
    }

    /// Where possible, replaces type/const/region variables in `value` with their final value.
    /// If a type/const/region variable has not (yet) been unified, it is left as is.
    ///
    /// This is an idempotent operation that does not affect inference state in any way,
    /// which means it's safe to call this function at will.
    pub fn deeply_resolve_via_unification_table<T>(&self, value: T) -> T
    where
        T: TypeFoldable<DbInterner<'db>>,
    {
        rustc_type_ir::InferCtxtLike::deeply_resolve_via_unification_table(self, value)
    }

    pub fn try_resolve_const_var(&self, vid: ConstVid) -> Result<Const<'db>, UniverseIndex> {
        match self.inner.borrow_mut().const_unification_table().probe_value(vid) {
            ConstVariableValue::Known { value } => Ok(value),
            ConstVariableValue::Unknown { span: _, universe } => Err(universe),
        }
    }

    /// Returns the span of the type variable identified by `vid`.
    ///
    /// No attempt is made to resolve `vid` to its root variable.
    pub fn type_var_span(&self, vid: TyVid) -> Span {
        self.inner.borrow_mut().type_variables().var_span(vid)
    }

    /// The span of `vid` and the type parameter it was made for, if any (rustc's
    /// `type_var_origin`).
    pub fn type_var_origin(&self, vid: TyVid) -> (Span, Option<GenericParamId>) {
        let mut inner = self.inner.borrow_mut();
        let param = inner.type_variable_storage.param(vid);
        (inner.type_variables().var_span(vid), param.map(GenericParamId::TypeParamId))
    }

    /// The span of `vid` while it is unknown and the const parameter it was made for, if any
    /// (rustc's `const_var_origin`).
    pub fn const_var_origin(&self, vid: ConstVid) -> Option<(Span, Option<GenericParamId>)> {
        let span = self.const_var_span(vid)?;
        let inner = self.inner.borrow();
        let params = &inner.const_var_params;
        let param = params.binary_search_by_key(&vid, |&(vid, _)| vid).ok().map(|at| params[at].1);
        Some((span, param.map(GenericParamId::ConstParamId)))
    }

    /// Returns the span of the const variable identified by `vid`
    pub fn const_var_span(&self, vid: ConstVid) -> Option<Span> {
        match self.inner.borrow_mut().const_unification_table().probe_value(vid) {
            ConstVariableValue::Known { .. } => None,
            ConstVariableValue::Unknown { span, .. } => Some(span),
        }
    }

    // Instantiates the bound variables in a given binder with fresh inference
    // variables in the current universe.
    //
    // Use this method if you'd like to find some generic parameters of the binder's
    // variables (e.g. during a method call). If there isn't a [`BoundRegionConversionTime`]
    // that corresponds to your use case, consider whether or not you should
    // use [`InferCtxt::enter_forall`] instead.
    pub fn instantiate_binder_with_fresh_vars<T>(
        &self,
        span: Span,
        _lbrct: BoundRegionConversionTime<'db>,
        value: Binder<'db, T>,
    ) -> T
    where
        T: TypeFoldable<DbInterner<'db>>,
    {
        if value.as_ref().no_bound_vars().is_some() {
            return value.skip_binder();
        }

        let bound_vars = value.bound_vars();
        let mut args = smallvec::SmallVec::<[GenericArg<'db>; 8]>::with_capacity(bound_vars.len());

        for bound_var_kind in bound_vars {
            let arg: GenericArg<'db> = match bound_var_kind {
                BoundVariableKind::Ty(_) => self.next_ty_var(span).into(),
                BoundVariableKind::Region(_) => self.next_region_var(span).into(),
                BoundVariableKind::Const => self.next_const_var(span).into(),
            };
            args.push(arg);
        }

        struct ToFreshVars<'db> {
            args: smallvec::SmallVec<[GenericArg<'db>; 8]>,
        }

        impl<'db> BoundVarReplacerDelegate<DbInterner<'db>> for ToFreshVars<'db> {
            fn replace_region(&mut self, br: BoundRegion<'db>) -> Region<'db> {
                self.args[br.var.index()].expect_region()
            }
            fn replace_ty(&mut self, bt: BoundTy<'db>) -> Ty<'db> {
                self.args[bt.var.index()].expect_ty()
            }
            fn replace_const(&mut self, bv: BoundConst<'db>) -> Const<'db> {
                self.args[bv.var.index()].expect_const()
            }
        }
        let delegate = ToFreshVars { args };
        self.interner.replace_bound_vars_uncached(value, delegate)
    }

    /// Obtains the latest type of the given closure; this may be a
    /// closure in the current function, in which case its
    /// `ClosureKind` may not yet be known.
    pub fn closure_kind(&self, closure_ty: Ty<'db>) -> Option<ClosureKind> {
        let unresolved_kind_ty = match closure_ty.kind() {
            TyKind::Closure(_, args) => args.as_closure().kind_ty(),
            TyKind::CoroutineClosure(_, args) => args.as_coroutine_closure().kind_ty(),
            _ => panic!("unexpected type {closure_ty:?}"),
        };
        let closure_kind_ty = self.shallow_resolve(unresolved_kind_ty);
        closure_kind_ty.to_opt_closure_kind()
    }

    pub fn universe(&self) -> UniverseIndex {
        self.universe.get()
    }

    /// See `InferCtxtInner::state_changes`.
    #[inline]
    pub fn state_changes(&self) -> u64 {
        self.inner.borrow().state_changes()
    }

    /// Creates and return a fresh universe that extends all previous
    /// universes. Updates `self.universe` to that new universe.
    pub fn create_next_universe(&self) -> UniverseIndex {
        let u = self.universe.get().next_universe();
        debug!("create_next_universe {u:?}");
        self.universe.set(u);
        u
    }

    /// The returned function is used in a fast path. If it returns `true` the variable is
    /// unchanged, `false` indicates that the status is unknown.
    #[inline]
    pub fn is_ty_infer_var_definitely_unchanged<'a>(
        &'a self,
    ) -> impl Fn(TyOrConstInferVar) -> bool + use<'a, 'db> {
        // This hoists the borrow/release out of the loop body.
        let inner = self.inner.try_borrow();

        move |infer_var: TyOrConstInferVar| match (infer_var, &inner) {
            (TyOrConstInferVar::Ty(ty_var), Ok(inner)) => {
                use self::type_variable::TypeVariableValue;

                matches!(
                    inner.try_type_variables_probe_ref(ty_var),
                    Some(TypeVariableValue::Unknown { .. })
                )
            }
            _ => false,
        }
    }

    /// `ty_or_const_infer_var_changed` is equivalent to one of these two:
    ///   * `shallow_resolve(ty) != ty` (where `ty.kind = Infer(_)`)
    ///   * `shallow_resolve(ct) != ct` (where `ct.kind = ConstKind::Infer(_)`)
    ///
    /// However, `ty_or_const_infer_var_changed` is more efficient. It's always
    /// inlined, despite being large, because it has only two call sites that
    /// are extremely hot (both in `traits::fulfill`'s checking of `stalled_on`
    /// inference variables), and it handles both `Ty` and `Const` without
    /// having to resort to storing full `GenericArg`s in `stalled_on`.
    ///
    /// A variable that is no longer the root of its set counts as changed: `shallow_resolve`
    /// then returns the root, not the variable.
    #[inline(always)]
    pub fn ty_or_const_infer_var_changed(&self, var: TyOrConstInferVar) -> bool {
        use self::type_variable::TypeVariableValue;

        let inner = self.inner.borrow();
        match var {
            TyOrConstInferVar::Ty(vid) => !matches!(
                inner.try_type_variables_probe_ref(vid),
                Some(TypeVariableValue::Unknown { .. })
            ),
            TyOrConstInferVar::TyInt(vid) => !matches!(
                inner.int_unification_storage.try_probe_value(vid),
                Some(IntVarValue::Unknown)
            ),
            TyOrConstInferVar::TyFloat(vid) => !matches!(
                inner.float_unification_storage.try_probe_value(vid),
                Some(FloatVarValue::Unknown)
            ),
            TyOrConstInferVar::Const(vid) => !matches!(
                inner.const_unification_storage.try_probe_value(vid),
                Some(ConstVariableValue::Unknown { .. })
            ),
        }
    }

    pub(crate) fn sub_unification_table_root_var(
        &self,
        var: rustc_type_ir::TyVid,
    ) -> rustc_type_ir::TyVid {
        self.inner.borrow_mut().type_variables().sub_unification_table_root_var(var)
    }

    pub fn sub_unify_ty_vids_raw(&self, a: rustc_type_ir::TyVid, b: rustc_type_ir::TyVid) {
        self.inner.borrow_mut().type_variables().sub_unify(a, b);
    }

    pub fn insert_placeholder_assumptions(
        &self,
        u: UniverseIndex,
        assumptions: Assumptions<DbInterner<'db>>,
    ) {
        assert!(
            !assumptions.type_outlives.has_escaping_bound_vars(),
            "assumptions has escaping bound vars, which is indicative of a bug in how assumptions are handled: {:?}",
            assumptions.type_outlives
        );
        assert!(
            assumptions.region_outlives.base_edges().all(|r| !r.has_escaping_bound_vars()),
            "assumptions has escaping bound vars, which is indicative of a bug in how assumptions are handled: {:?}",
            assumptions.region_outlives
        );
        self.placeholder_assumptions.borrow_mut().insert(u, assumptions);
    }

    /// The assumptions of universe `u`. Panics when `u` has none: the solver asks only for
    /// universes it entered with assumptions.
    pub fn get_placeholder_assumptions(&self, u: UniverseIndex) -> Assumptions<DbInterner<'db>> {
        match self.placeholder_assumptions.borrow().get(&u) {
            // clone: `InferCtxtLike` returns it owned; reached only with assumptions on binders.
            Some(assumptions) => assumptions.clone(),
            None => panic!("no placeholder assumptions for universe {u:?}"),
        }
    }

    /// Whether every universe in `range` has assumptions.
    pub fn has_placeholder_assumptions(&self, range: RangeInclusive<UniverseIndex>) -> bool {
        let assumptions = self.placeholder_assumptions.borrow();
        (range.start.index()..=range.last.index())
            .all(|u| assumptions.contains_key(&UniverseIndex::from_usize(u)))
    }

    /// rustc's `make_query_region_constraints`, given to `f` one at a time: each region
    /// constraint `sub <= sup` as `sup: sub` with whether the leak check sees it, then each
    /// registered type-outlives obligation, which it always sees. A reflexive `'a: 'a` says
    /// nothing and is passed over. `f` runs with the tables borrowed, so it must not use `self`.
    pub(crate) fn for_each_query_region_constraint(
        &self,
        mut f: impl FnMut(RegionConstraint<'db>, VisibleForLeakCheck),
    ) {
        let inner = self.inner.borrow();
        let storage =
            inner.region_constraint_storage.as_ref().expect("region constraints already solved");
        for (constraint, visible) in storage.legacy_data.constraints.iter_with_visibility() {
            let (sub, sup) = constraint.regions(self.interner);
            if sub != sup {
                f(RegionConstraint::Outlives(OutlivesClause(sup.into(), sub)), visible);
            }
        }
        for it in inner.region_obligations() {
            f(
                RegionConstraint::Outlives(OutlivesClause(it.sup_type.into(), it.sub_region)),
                VisibleForLeakCheck::Yes,
            );
        }
    }

    /// The region constraint the solver has built so far, with the span of each leaf.
    pub fn get_solver_region_constraint(&self) -> SolverRegionConstraint<'db> {
        self.inner
            .borrow()
            .region_constraint_storage
            .as_ref()
            .expect("region constraints already solved")
            .solver_data
            .get_constraint()
    }

    pub fn overwrite_solver_region_constraint(&self, constraint: SolverRegionConstraint<'db>) {
        self.inner.borrow_mut().unwrap_region_constraints().overwrite_solver_constraint(constraint);
    }

    /// Adds `constraint` to the solver's region constraint with an `and`.
    pub fn register_solver_region_constraint(&self, constraint: SolverRegionConstraint<'db>) {
        self.inner.borrow_mut().unwrap_region_constraints().register_solver_constraint(constraint);
    }

    #[inline]
    pub fn enable_next_solver_overflow_fcw(&self) -> bool {
        self.enable_next_solver_overflow_fcw.get()
    }

    /// Runs `f` with the overflow warning off, so building one warning does not start another.
    pub fn with_disabled_next_solver_overflow_fcw<R>(&self, f: impl FnOnce() -> R) -> R {
        let prev = self.enable_next_solver_overflow_fcw.replace(false);
        let ret = f();
        self.enable_next_solver_overflow_fcw.set(prev);
        ret
    }

    pub fn push_next_solver_overflow(&self, overflow: NextSolverOverflow<'db>) {
        self.next_solver_overflows.borrow_mut().push(overflow);
    }

    /// The overflow warnings recorded so far, which the caller reports; leaves none behind.
    pub fn take_next_solver_overflows(&self) -> Vec<NextSolverOverflow<'db>> {
        std::mem::take(&mut *self.next_solver_overflows.borrow_mut())
    }

    /// How many overflow warnings are recorded and not yet taken; an evaluation that records
    /// one changes it.
    pub(crate) fn next_solver_overflow_count(&self) -> usize {
        self.next_solver_overflows.borrow().len()
    }

    pub(crate) fn has_obligation_inspector(&self) -> bool {
        self.obligation_inspector.get().is_some()
    }

    /// Attach a callback to be invoked on each root obligation evaluated in the new trait solver.
    pub fn attach_obligation_inspector(&self, inspector: ObligationInspector<'db>) {
        debug_assert!(
            self.obligation_inspector.get().is_none(),
            "shouldn't override a set obligation inspector"
        );
        self.obligation_inspector.set(Some(inspector));
    }

    pub fn inspect_evaluated_obligation(
        &self,
        obligation: &PredicateObligation<'db>,
        result: &Result<GoalEvaluation<DbInterner<'db>>, NoSolution>,
        get_proof_tree: impl FnOnce() -> Option<inspect::GoalEvaluation<DbInterner<'db>>>,
    ) {
        if let Some(inspector) = self.obligation_inspector.get() {
            let result = match result {
                Ok(GoalEvaluation { certainty, .. }) => Ok(*certainty),
                Err(_) => Err(NoSolution),
            };
            (inspector)(self, obligation, result, get_proof_tree());
        }
    }
}

impl<'db> TypeTrace<'db> {
    pub fn types(cause: &ObligationCause, a: Ty<'db>, b: Ty<'db>) -> TypeTrace<'db> {
        TypeTrace {
            cause: *cause,
            values: ValuePairs::Terms(ExpectedFound::new(a.into(), b.into())),
        }
    }

    pub fn trait_refs(
        cause: &ObligationCause,
        a: TraitRef<'db>,
        b: TraitRef<'db>,
    ) -> TypeTrace<'db> {
        TypeTrace { cause: *cause, values: ValuePairs::TraitRefs(ExpectedFound::new(a, b)) }
    }

    pub fn consts(cause: &ObligationCause, a: Const<'db>, b: Const<'db>) -> TypeTrace<'db> {
        TypeTrace {
            cause: *cause,
            values: ValuePairs::Terms(ExpectedFound::new(a.into(), b.into())),
        }
    }
}

/// Requires that `region` must be equal to one of the regions in `choice_regions`.
/// We often denote this using the syntax:
///
/// ```text
/// R0 member of [O1..On]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MemberConstraint<'db> {
    /// The `DefId` and args of the opaque type causing this constraint.
    /// Used for error reporting.
    pub key: OpaqueTypeKey<'db>,

    /// The hidden type in which `member_region` appears: used for error reporting.
    pub hidden_ty: Ty<'db>,

    /// The region `R0`.
    pub member_region: Region<'db>,

    /// The options `O1..On`.
    pub choice_regions: Arc<Vec<Region<'db>>>,
}

#[cfg(test)]
mod tests {
    use super::{DbInternerInferExt, InferCtxt, TypingMode, traits::ObligationCause};
    use crate::{
        Span,
        next_solver::{DbInterner, ParamEnv, budget_tests::fixture, recycle},
    };

    /// What a context's tables show from outside, for comparing one with a never-used one.
    #[derive(Debug, PartialEq, Eq)]
    struct Observed {
        open_snapshots: usize,
        ty_vars: usize,
        const_vars: usize,
        int_vars: usize,
        float_vars: usize,
        region_vars: usize,
        state_changes: u64,
        const_var_params: usize,
        region_obligations: usize,
        region_assumptions: usize,
        opaque_types: super::opaque_types::table::OpaqueTypeStorageEntries,
        region_constraints: (usize, usize, usize),
        placeholder_assumptions: usize,
        overflows: usize,
    }

    fn observed(infcx: &InferCtxt<'_>) -> Observed {
        let mut inner = infcx.inner.borrow_mut();
        let region_constraints = {
            let collector = inner.unwrap_region_constraints();
            let data = collector.region_constraint_data();
            (data.constraints.len(), data.member_constraints.len(), data.verifys.len())
        };
        Observed {
            open_snapshots: inner.undo_log.num_open_snapshots(),
            ty_vars: inner.type_variables().num_vars(),
            const_vars: inner.const_unification_storage.len(),
            int_vars: inner.int_unification_storage.len(),
            float_vars: inner.float_unification_storage.len(),
            region_vars: inner.unwrap_region_constraints().num_region_vars(),
            state_changes: inner.state_changes(),
            const_var_params: inner.const_var_params.len(),
            region_obligations: inner.region_obligations.len(),
            region_assumptions: inner.region_assumptions.len(),
            opaque_types: inner.opaque_type_storage.num_entries(),
            region_constraints,
            placeholder_assumptions: infcx.placeholder_assumptions.borrow().len(),
            overflows: infcx.next_solver_overflows.borrow().len(),
        }
    }

    /// New type, int and const variables, two unified, a snapshot opened and rolled back
    /// around more of them, and a region obligation.
    fn use_context(infcx: &InferCtxt<'_>) {
        let cause = ObligationCause::dummy();
        let (a, b) = (infcx.next_ty_var(Span::Dummy), infcx.next_ty_var(Span::Dummy));
        let _ = infcx.next_int_var();
        let _ = infcx.next_const_var(Span::Dummy);
        let ok = infcx.at(&cause, ParamEnv::empty()).eq(a, b).expect("two fresh variables unify");
        assert!(ok.obligations.is_empty());
        let snapshot = infcx.start_snapshot();
        let c = infcx.next_ty_var(Span::Dummy);
        let _ = infcx.at(&cause, ParamEnv::empty()).eq(a, c).expect("unify in the snapshot");
        let _ = infcx.next_region_var(Span::Dummy);
        infcx.rollback_to(snapshot);
        infcx.register_type_outlives_constraint(a, infcx.interner.default_types().regions.statik);
    }

    fn build<'db>(interner: DbInterner<'db>) -> InferCtxt<'db> {
        interner.infer_ctxt().build(TypingMode::non_body_analysis())
    }

    #[test]
    fn a_recycled_context_is_a_new_one() {
        let (db, krate) = fixture();
        let interner = DbInterner::new_with(&db, krate);
        // Each test runs on a thread of its own: this one's tables are new.
        let fresh = observed(&build(interner));
        let used = build(interner);
        use_context(&used);
        assert_ne!(observed(&used), fresh);
        drop(used);
        let recycled = build(interner);
        assert!(recycled.inner.borrow().held() > 0, "the used context's tables came back");
        assert_eq!(observed(&recycled), fresh);
        // And it behaves as a new one.
        use_context(&recycled);
    }

    #[test]
    fn a_warm_context_allocates_nothing() {
        let (db, krate) = fixture();
        let interner = DbInterner::new_with(&db, krate);
        let infcx = build(interner);
        use_context(&infcx);
        drop(infcx);
        blaze_alloc_count::assert_at_most(0, "a warm inference context", || {
            let infcx = build(interner);
            use_context(&infcx);
        });
    }

    #[test]
    fn a_warm_fork_allocates_nothing() {
        let (db, krate) = fixture();
        let interner = DbInterner::new_with(&db, krate);
        let infcx = build(interner);
        for _ in 0..100 {
            let _ = infcx.next_ty_var(Span::Dummy);
        }
        use_context(&infcx);
        drop(infcx.fork());
        blaze_alloc_count::assert_at_most(0, "a warm fork", || {
            let fork = infcx.fork();
            assert_eq!(fork.num_ty_vars(), infcx.num_ty_vars());
        });
    }

    #[test]
    fn a_huge_context_is_not_kept() {
        let (db, krate) = fixture();
        let interner = DbInterner::new_with(&db, krate);
        let infcx = build(interner);
        let rows = recycle::KEEP_AT_MOST / size_of::<super::TypeOutlivesConstraint<'_>>() + 1;
        infcx.inner.borrow_mut().region_obligations.reserve_exact(rows);
        assert!(infcx.inner.borrow().held() > recycle::KEEP_AT_MOST);
        drop(infcx);
        assert_eq!(recycle::infer_tables().held(), 0);
    }
}
