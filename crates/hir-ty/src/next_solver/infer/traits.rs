//! Trait Resolution. See the [rustc-dev-guide] for more information on how this works.
//!
//! [rustc-dev-guide]: https://rustc-dev-guide.rust-lang.org/traits/resolution.html

use std::{
    cmp,
    hash::{Hash, Hasher},
};

use hir_def::{GenericDefId, TraitId};
use macros::{TypeFoldable, TypeVisitable};
use rustc_type_ir::elaborate::Elaboratable;
use rustc_type_ir::{
    Upcast,
    solve::{Certainty, NoSolution, inspect},
};
use tracing::debug;

use crate::{
    Span,
    next_solver::{
        Clause, DbInterner, Goal, ParamEnv, PolyTraitClause, Predicate, TraitClause, TraitRef, Ty,
    },
};

use super::InferCtxt;

/// The reason why we incurred this obligation; used for error reporting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, TypeVisitable, TypeFoldable)]
pub struct ObligationCause {
    #[type_visitable(ignore)]
    span: Span,
    #[type_visitable(ignore)]
    #[type_foldable(identity)]
    code: ObligationCauseCode,
}

/// rustc's `ObligationCauseCode`, the codes diagnostics read. Ids only, so a cause stays `Copy`
/// (rustc boxes it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObligationCauseCode {
    /// Not well classified or should be obvious from the span.
    Misc,
    /// Like rustc's `WhereClauseInExpr`: the `bound_index`th predicate of `item`, required by
    /// a use of it in an expression; `arg_index` is the argument the bound is about, if any.
    WhereClauseInExpr { item: GenericDefId, bound_index: u32, arg_index: Option<u32> },
    /// The type of a function argument must be `Sized`.
    SizedArgumentType,
    /// The declared return type of a body must be `Sized`.
    SizedReturnType,
    /// The tail expression of a block.
    BlockTailExpression,
    /// The value of a `return`, or a body's tail.
    ReturnValue,
    /// The arms of an `if` must agree.
    IfExpression,
    /// A `match` arm must agree with the `prior_arms` before it.
    MatchExpressionArm { prior_arms: u32 },
    /// The type of a `let` must be `Sized`.
    LetType,
    /// An `if` without an `else` is `()`, which must agree with what the `if` is expected to be.
    IfExpressionWithNoElse,
    /// A `return` without a value returns `()`, which must agree with the return type.
    ReturnNoExpression,
}

impl ObligationCause {
    #[inline]
    pub fn new<S: Into<Span>>(span: S) -> ObligationCause {
        ObligationCause::with_code(span, ObligationCauseCode::Misc)
    }

    #[inline]
    pub fn with_code<S: Into<Span>>(span: S, code: ObligationCauseCode) -> ObligationCause {
        ObligationCause { span: span.into(), code }
    }

    #[inline]
    pub fn dummy() -> ObligationCause {
        ObligationCause::new(Span::Dummy)
    }

    #[inline]
    pub(crate) fn span(&self) -> Span {
        self.span
    }

    #[inline]
    pub fn code(&self) -> ObligationCauseCode {
        self.code
    }
}

/// An `Obligation` represents some trait reference (e.g., `i32: Eq`) for
/// which the "impl_source" must be found. The process of finding an "impl_source" is
/// called "resolving" the `Obligation`. This process consists of
/// either identifying an `impl` (e.g., `impl Eq for i32`) that
/// satisfies the obligation, or else finding a bound that is in
/// scope. The eventual result is usually a `Selection` (defined below).
#[derive(Clone, Copy, Debug, TypeVisitable, TypeFoldable)]
pub struct Obligation<'db, T> {
    #[type_foldable(identity)]
    #[type_visitable(ignore)]
    /// The reason we have to prove this thing.
    pub cause: ObligationCause,

    /// The environment in which we should prove this thing.
    pub param_env: ParamEnv<'db>,

    /// The thing we are trying to prove.
    pub predicate: T,

    /// If we started proving this as a result of trying to prove
    /// something else, track the total depth to ensure termination.
    /// If this goes over a certain threshold, we abort compilation --
    /// in such cases, we can not say whether or not the predicate
    /// holds for certain. Stupid halting problem; such a drag.
    ///
    /// `u32`: fulfillment stops an obligation once this passes the crate's `recursion_limit`,
    /// itself a `u32`.
    #[type_foldable(identity)]
    #[type_visitable(ignore)]
    pub recursion_depth: u32,
}

// `PredicateObligation` is moved per nested goal. Make sure it doesn't unintentionally get bigger:
// 8 bytes of span, at most 24 of cause code (a `GenericDefId` and two `u32`s), 12 of the rest.
const _: () = assert!(size_of::<PredicateObligation<'static>>() <= 44);

/// A callback that can be provided to `inspect_typeck`. Invoked on evaluation
/// of root obligations.
pub type ObligationInspector<'db> = fn(
    &InferCtxt<'db>,
    &PredicateObligation<'db>,
    Result<Certainty, NoSolution>,
    Option<inspect::GoalEvaluation<DbInterner<'db>>>,
);

/// For [`Obligation`], a sub-obligation is combined with the current obligation's
/// param-env and cause code.
impl<'db> Elaboratable<DbInterner<'db>> for PredicateObligation<'db> {
    fn predicate(&self) -> Predicate<'db> {
        self.predicate
    }

    fn child(&self, clause: Clause<'db>) -> Self {
        Obligation {
            cause: self.cause,
            param_env: self.param_env,
            recursion_depth: 0,
            predicate: clause.as_predicate(),
        }
    }

    fn child_with_derived_cause(
        &self,
        clause: Clause<'db>,
        span: Span,
        _parent_trait_pred: PolyTraitClause<'db>,
        _index: usize,
    ) -> Self {
        let cause = ObligationCause::new(span);
        Obligation {
            cause,
            param_env: self.param_env,
            recursion_depth: 0,
            predicate: clause.as_predicate(),
        }
    }
}

impl<'db, T: Copy> Obligation<'db, T> {
    pub fn as_goal(&self) -> Goal<'db, T> {
        Goal { param_env: self.param_env, predicate: self.predicate }
    }
}

impl<'db, T: PartialEq> PartialEq<Obligation<'db, T>> for Obligation<'db, T> {
    #[inline]
    fn eq(&self, other: &Obligation<'db, T>) -> bool {
        // Ignore `cause` and `recursion_depth`. This is a small performance
        // win for a few crates, and a huge performance win for the crate in
        // https://github.com/rust-lang/rustc-perf/pull/1680, which greatly
        // stresses the trait system.
        self.param_env == other.param_env && self.predicate == other.predicate
    }
}

impl<'db, T: Eq> Eq for Obligation<'db, T> {}

impl<'db, T: Hash> Hash for Obligation<'db, T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // See the comment on `Obligation::eq`.
        self.param_env.hash(state);
        self.predicate.hash(state);
    }
}

impl<'db, P> From<Obligation<'db, P>> for Goal<'db, P> {
    fn from(value: Obligation<'db, P>) -> Self {
        Goal { param_env: value.param_env, predicate: value.predicate }
    }
}

pub(crate) type PredicateObligation<'db> = Obligation<'db, Predicate<'db>>;
pub(crate) type TraitObligation<'db> = Obligation<'db, TraitClause<'db>>;

pub(crate) type PredicateObligations<'db> = Vec<PredicateObligation<'db>>;

impl<'db> PredicateObligation<'db> {
    /// Flips the polarity of the inner predicate.
    ///
    /// Given `T: Trait` predicate it returns `T: !Trait` and given `T: !Trait` returns `T: Trait`.
    pub fn flip_polarity(&self, interner: DbInterner<'db>) -> Option<PredicateObligation<'db>> {
        Some(PredicateObligation {
            cause: self.cause,
            param_env: self.param_env,
            predicate: self.predicate.flip_polarity(interner)?,
            recursion_depth: self.recursion_depth,
        })
    }
}

impl<'db, O> Obligation<'db, O> {
    pub fn new(
        interner: DbInterner<'db>,
        cause: ObligationCause,
        param_env: ParamEnv<'db>,
        predicate: impl Upcast<DbInterner<'db>, O>,
    ) -> Obligation<'db, O> {
        Self::with_depth(interner, cause, 0, param_env, predicate)
    }

    /// We often create nested obligations without setting the correct depth.
    ///
    /// To deal with this evaluate and fulfill explicitly update the depth
    /// of nested obligations using this function.
    pub fn set_depth_from_parent(&mut self, parent_depth: u32) {
        self.recursion_depth = cmp::max(parent_depth + 1, self.recursion_depth);
    }

    pub fn with_depth(
        interner: DbInterner<'db>,
        cause: ObligationCause,
        recursion_depth: u32,
        param_env: ParamEnv<'db>,
        predicate: impl Upcast<DbInterner<'db>, O>,
    ) -> Obligation<'db, O> {
        let predicate = predicate.upcast(interner);
        Obligation { cause, param_env, recursion_depth, predicate }
    }

    pub fn with<P>(
        &self,
        tcx: DbInterner<'db>,
        value: impl Upcast<DbInterner<'db>, P>,
    ) -> Obligation<'db, P> {
        Obligation::with_depth(tcx, self.cause, self.recursion_depth, self.param_env, value)
    }
}

/// Determines whether the type `ty` is known to meet `bound` and
/// returns true if so. Returns false if `ty` either does not meet
/// `bound` or is not known to meet bound (note that this is
/// conservative towards *no impl*, which is the opposite of the
/// `evaluate` methods).
pub(crate) fn type_known_to_meet_bound_modulo_regions<'tcx>(
    infcx: &InferCtxt<'tcx>,
    param_env: ParamEnv<'tcx>,
    ty: Ty<'tcx>,
    def_id: TraitId,
) -> bool {
    let trait_ref = TraitRef::new(infcx.interner, def_id.into(), [ty]);
    pred_known_to_hold_modulo_regions(infcx, param_env, trait_ref)
}

/// FIXME(@lcnr): this function doesn't seem right and shouldn't exist?
///
/// Ping me on zulip if you want to use this method and need help with finding
/// an appropriate replacement.
fn pred_known_to_hold_modulo_regions<'db>(
    infcx: &InferCtxt<'db>,
    param_env: ParamEnv<'db>,
    pred: impl Upcast<DbInterner<'db>, Predicate<'db>>,
) -> bool {
    let obligation = Obligation::new(infcx.interner, ObligationCause::dummy(), param_env, pred);

    let result = infcx.evaluate_obligation(&obligation);
    debug!(?result);

    result.must_apply_modulo_regions()
}

#[cfg(test)]
mod tests {
    use std::hash::BuildHasher;

    use super::{Obligation, ObligationCause};
    use crate::next_solver::ParamEnv;

    fn obligation(predicate: u32, depth: u32) -> Obligation<'static, u32> {
        Obligation {
            cause: ObligationCause::dummy(),
            param_env: ParamEnv::empty(),
            predicate,
            recursion_depth: depth,
        }
    }

    #[test]
    fn eq_and_hash_ignore_cause_and_depth() {
        let hasher = blaze_hash::FixedState;
        let (a, b) = (obligation(7, 0), obligation(7, 5));
        assert_eq!(a, b);
        assert_eq!(hasher.hash_one(a), hasher.hash_one(b));
        assert_ne!(a, obligation(8, 0));
    }

    #[test]
    fn depth_from_parent_takes_the_deeper() {
        let mut child = obligation(1, 3);
        child.set_depth_from_parent(1);
        assert_eq!(child.recursion_depth, 3);
        child.set_depth_from_parent(9);
        assert_eq!(child.recursion_depth, 10);
    }

    /// Fulfillment copies, re-depths, hashes and turns obligations into goals once per nested
    /// goal; none of that may touch the heap.
    #[test]
    fn obligation_hot_path_allocates_nothing() {
        let hasher = blaze_hash::FixedState;
        // `ParamEnv::empty` initializes the default interned values once.
        let parent = obligation(1, 4);
        let mut sum = 0u64;
        blaze_alloc_count::assert_at_most(0, "copying an obligation into a goal", || {
            for predicate in 0..1024 {
                let mut child = parent;
                child.predicate = predicate;
                child.set_depth_from_parent(parent.recursion_depth);
                let goal = child.as_goal();
                sum = sum.wrapping_add(hasher.hash_one(child)) ^ u64::from(goal.predicate);
            }
        });
        assert_ne!(sum, 0);
    }
}
