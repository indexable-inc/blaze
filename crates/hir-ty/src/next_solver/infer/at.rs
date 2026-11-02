//! A nice interface for working with the infcx. Subtyping goes through
//! `infcx.sub_raw(cause, param_env, a, b)` and `infcx.sup_raw(..)`; equality,
//! `relate` and `lub` also through `infcx.at(cause, param_env)`, which sets
//! the "cause" of the operation as well as the surrounding parameter
//! environment. The first value is always the "expected" output from the POV
//! of diagnostics.
//!
//! Examples:
//! ```ignore (fragment)
//!     infcx.sub_raw(cause, param_env, a, b)
//!     // requires that `a <: b`, with `a` considered the "expected" type
//!
//!     infcx.sup_raw(cause, param_env, a, b)
//!     // requires that `b <: a`, with `a` considered the "expected" type
//!
//!     infcx.at(cause, param_env).eq(a, b)
//!     // requires that `a == b`, with `a` considered the "expected" type
//! ```

use std::cell::{Cell, RefCell};

use rustc_type_ir::{
    FnSig, GenericArgKind, TypeFoldable, TypingMode, Variance,
    error::ExpectedFound,
    relate::{Relate, TypeRelation, solver_relating::RelateExt},
};

use crate::next_solver::{
    AliasTerm, AliasTy, Binder, Const, DbInterner, GenericArg, Goal, ParamEnv,
    PolyExistentialProjection, PolyExistentialTraitRef, PolyFnSig, Predicate, Region, Term,
    TraitRef, Ty,
    fulfill::NextSolverError,
    infer::relate::lattice::{LatticeOp, LatticeOpKind},
    recycle,
};

use super::{
    InferCtxt, InferOk, InferResult, InferTables, TypeTrace, ValuePairs,
    traits::{Obligation, ObligationCause},
};

#[derive(Clone, Copy)]
pub struct At<'a, 'db> {
    pub infcx: &'a InferCtxt<'db>,
    pub cause: &'a ObligationCause,
    pub param_env: ParamEnv<'db>,
}

impl<'db> InferCtxt<'db> {
    #[inline]
    pub fn at<'a>(&'a self, cause: &'a ObligationCause, param_env: ParamEnv<'db>) -> At<'a, 'db> {
        At { infcx: self, cause, param_env }
    }

    /// Forks the inference context, creating a new inference context with the same inference
    /// variables in the same state. This can be used to "branch off" many tests from the same
    /// common state.
    pub fn fork(&self) -> Self {
        let InferTables { inner, canonicalizer, placeholder_assumptions, next_solver_overflows } =
            self.fork_tables();
        Self {
            interner: self.interner,
            typing_mode: self.typing_mode,
            inner: RefCell::new(inner),
            tainted_by_errors: Cell::new(self.tainted_by_errors.get()),
            universe: Cell::new(self.universe.get()),
            placeholder_assumptions: RefCell::new(placeholder_assumptions),
            enable_next_solver_overflow_fcw: Cell::new(self.enable_next_solver_overflow_fcw.get()),
            next_solver_overflows: RefCell::new(next_solver_overflows),
            obligation_inspector: Cell::new(self.obligation_inspector.get()),
            canonicalizer_state: RefCell::new(canonicalizer),
        }
    }

    /// The worker's warm tables (`recycle::infer_tables`) holding a copy of this context's.
    fn fork_tables(&self) -> InferTables<'db> {
        let mut tables = recycle::infer_tables();
        // clone: a fork is a copy of the variable tables, into the worker's warm ones.
        tables.inner.clone_from(&self.inner.borrow());
        // clone: the fork sees the same universes, so it needs their assumptions.
        tables.placeholder_assumptions.clone_from(&self.placeholder_assumptions.borrow());
        // clone: empty unless a root goal overflowed, and then a handful.
        tables.next_solver_overflows.clone_from(&self.next_solver_overflows.borrow());
        tables
    }

    /// Forks the inference context, creating a new inference context with the same inference
    /// variables in the same state, except possibly changing the intercrate mode. This can be
    /// used to "branch off" many tests from the same common state. Used in negative coherence.
    pub fn fork_with_typing_mode(&self, typing_mode: TypingMode<DbInterner<'db>>) -> Self {
        // Unlike `fork`, this invalidates all cache entries as they may depend on the
        // typing mode.

        let InferTables { inner, canonicalizer, placeholder_assumptions, next_solver_overflows } =
            self.fork_tables();
        Self {
            interner: self.interner,
            typing_mode,
            inner: RefCell::new(inner),
            tainted_by_errors: Cell::new(self.tainted_by_errors.get()),
            universe: Cell::new(self.universe.get()),
            placeholder_assumptions: RefCell::new(placeholder_assumptions),
            enable_next_solver_overflow_fcw: Cell::new(self.enable_next_solver_overflow_fcw.get()),
            next_solver_overflows: RefCell::new(next_solver_overflows),
            obligation_inspector: Cell::new(self.obligation_inspector.get()),
            canonicalizer_state: RefCell::new(canonicalizer),
        }
    }

    /// Makes `actual <: expected`. For example, if type-checking a
    /// call like `foo(x)`, where `foo: fn(i32)`, you might have
    /// `sup_raw(cause, param_env, i32, x)`, since the "expected" type is the
    /// type that appears in the signature.
    pub fn sup_raw<T>(
        &self,
        cause: &ObligationCause,
        param_env: ParamEnv<'db>,
        expected: T,
        actual: T,
    ) -> InferResult<'db, ()>
    where
        T: ToTrace<'db>,
    {
        RelateExt::relate(self, param_env, expected, Variance::Contravariant, actual, cause.span())
            .map(|goals| self.goals_to_obligations(cause, goals))
    }

    /// Makes `expected <: actual`.
    pub fn sub_raw<T>(
        &self,
        cause: &ObligationCause,
        param_env: ParamEnv<'db>,
        expected: T,
        actual: T,
    ) -> InferResult<'db, ()>
    where
        T: ToTrace<'db>,
    {
        RelateExt::relate(self, param_env, expected, Variance::Covariant, actual, cause.span())
            .map(|goals| self.goals_to_obligations(cause, goals))
    }

    /// Makes `expected == actual`.
    pub fn eq_raw<T>(
        &self,
        cause: &ObligationCause,
        param_env: ParamEnv<'db>,
        expected: T,
        actual: T,
    ) -> InferResult<'db, ()>
    where
        T: Relate<DbInterner<'db>>,
    {
        RelateExt::relate(self, param_env, expected, Variance::Invariant, actual, cause.span())
            .map(|goals| self.goals_to_obligations(cause, goals))
    }

    pub fn relate_raw<T>(
        &self,
        cause: &ObligationCause,
        param_env: ParamEnv<'db>,
        expected: T,
        variance: Variance,
        actual: T,
    ) -> InferResult<'db, ()>
    where
        T: ToTrace<'db>,
    {
        self.at(cause, param_env).relate(expected, variance, actual)
    }

    /// Computes the least-upper-bound, or mutual supertype, of two
    /// values. The order of the arguments doesn't matter, but since
    /// this can result in an error (e.g., if asked to compute LUB of
    /// u32 and i32), it is meaningful to call one of them the
    /// "expected type".
    pub fn lub_raw<T>(
        &self,
        cause: &ObligationCause,
        param_env: ParamEnv<'db>,
        expected: T,
        actual: T,
    ) -> InferResult<'db, T>
    where
        T: ToTrace<'db>,
    {
        self.at(cause, param_env).lub(expected, actual)
    }

    pub fn goals_to_obligations(
        &self,
        cause: &ObligationCause,
        goals: Vec<Goal<'db, Predicate<'db>>>,
    ) -> InferOk<'db, ()> {
        InferOk {
            value: (),
            obligations: goals
                .into_iter()
                .map(|goal| Obligation::new(self.interner, *cause, goal.param_env, goal.predicate))
                // alloc: none when the relation made no goals; otherwise the caller owns them.
                .collect(),
        }
    }
}

pub trait ToTrace<'db>: Relate<DbInterner<'db>> {
    fn to_trace(cause: &ObligationCause, a: Self, b: Self) -> TypeTrace<'db>;
}

impl<'a, 'db> At<'a, 'db> {
    /// Makes `actual <: expected`. For example, if type-checking a
    /// call like `foo(x)`, where `foo: fn(i32)`, you might have
    /// `sup(i32, x)`, since the "expected" type is the type that
    /// appears in the signature.
    pub fn sup<T>(self, expected: T, actual: T) -> InferResult<'db, ()>
    where
        T: ToTrace<'db>,
    {
        self.infcx.sup_raw(self.cause, self.param_env, expected, actual)
    }

    /// Makes `expected <: actual`.
    pub fn sub<T>(self, expected: T, actual: T) -> InferResult<'db, ()>
    where
        T: ToTrace<'db>,
    {
        self.infcx.sub_raw(self.cause, self.param_env, expected, actual)
    }

    /// Makes `expected == actual`.
    pub fn eq<T>(self, expected: T, actual: T) -> InferResult<'db, ()>
    where
        T: Relate<DbInterner<'db>>,
    {
        self.infcx.eq_raw(self.cause, self.param_env, expected, actual)
    }

    pub fn relate<T>(self, expected: T, variance: Variance, actual: T) -> InferResult<'db, ()>
    where
        T: ToTrace<'db>,
    {
        match variance {
            Variance::Covariant => {
                self.infcx.sub_raw(self.cause, self.param_env, expected, actual)
            }
            Variance::Invariant => self.eq(expected, actual),
            Variance::Contravariant => {
                self.infcx.sup_raw(self.cause, self.param_env, expected, actual)
            }

            // We could make this make sense but it's not readily
            // exposed and I don't feel like dealing with it. Note
            // that bivariance in general does a bit more than just
            // *nothing*, it checks that the types are the same
            // "modulo variance" basically.
            Variance::Bivariant => panic!("Bivariant given to `relate()`"),
        }
    }

    /// Deeply normalizes `value`, replacing all aliases which can by normalized in
    /// the current environment. This errors in case normalization fails or is ambiguous.
    pub fn deeply_normalize<T>(self, value: T) -> Result<T, Vec<NextSolverError<'db>>>
    where
        T: TypeFoldable<DbInterner<'db>>,
    {
        crate::next_solver::normalize::deeply_normalize(self, value)
    }

    /// Computes the least-upper-bound, or mutual supertype, of two
    /// values. The order of the arguments doesn't matter, but since
    /// this can result in an error (e.g., if asked to compute LUB of
    /// u32 and i32), it is meaningful to call one of them the
    /// "expected type".
    pub fn lub<T>(self, expected: T, actual: T) -> InferResult<'db, T>
    where
        T: ToTrace<'db>,
    {
        let mut op = LatticeOp::new(
            self.infcx,
            ToTrace::to_trace(self.cause, expected, actual),
            self.param_env,
            LatticeOpKind::Lub,
        );
        let value = op.relate(expected, actual)?;
        Ok(InferOk { value, obligations: op.into_obligations() })
    }
}

impl<'db> ToTrace<'db> for Ty<'db> {
    fn to_trace(cause: &ObligationCause, a: Self, b: Self) -> TypeTrace<'db> {
        TypeTrace {
            cause: *cause,
            values: ValuePairs::Terms(ExpectedFound::new(a.into(), b.into())),
        }
    }
}

impl<'db> ToTrace<'db> for Region<'db> {
    fn to_trace(cause: &ObligationCause, a: Self, b: Self) -> TypeTrace<'db> {
        TypeTrace { cause: *cause, values: ValuePairs::Regions(ExpectedFound::new(a, b)) }
    }
}

impl<'db> ToTrace<'db> for Const<'db> {
    fn to_trace(cause: &ObligationCause, a: Self, b: Self) -> TypeTrace<'db> {
        TypeTrace {
            cause: *cause,
            values: ValuePairs::Terms(ExpectedFound::new(a.into(), b.into())),
        }
    }
}

impl<'db> ToTrace<'db> for GenericArg<'db> {
    fn to_trace(cause: &ObligationCause, a: Self, b: Self) -> TypeTrace<'db> {
        TypeTrace {
            cause: *cause,
            values: match (a.kind(), b.kind()) {
                (GenericArgKind::Lifetime(a), GenericArgKind::Lifetime(b)) => {
                    ValuePairs::Regions(ExpectedFound::new(a, b))
                }
                (GenericArgKind::Type(a), GenericArgKind::Type(b)) => {
                    ValuePairs::Terms(ExpectedFound::new(a.into(), b.into()))
                }
                (GenericArgKind::Const(a), GenericArgKind::Const(b)) => {
                    ValuePairs::Terms(ExpectedFound::new(a.into(), b.into()))
                }
                _ => panic!("relating different kinds: {a:?} {b:?}"),
            },
        }
    }
}

impl<'db> ToTrace<'db> for Term<'db> {
    fn to_trace(cause: &ObligationCause, a: Self, b: Self) -> TypeTrace<'db> {
        TypeTrace { cause: *cause, values: ValuePairs::Terms(ExpectedFound::new(a, b)) }
    }
}

impl<'db> ToTrace<'db> for TraitRef<'db> {
    fn to_trace(cause: &ObligationCause, a: Self, b: Self) -> TypeTrace<'db> {
        TypeTrace { cause: *cause, values: ValuePairs::TraitRefs(ExpectedFound::new(a, b)) }
    }
}

impl<'db> ToTrace<'db> for AliasTy<'db> {
    fn to_trace(cause: &ObligationCause, a: Self, b: Self) -> TypeTrace<'db> {
        TypeTrace {
            cause: *cause,
            values: ValuePairs::Aliases(ExpectedFound::new(a.into(), b.into())),
        }
    }
}

impl<'db> ToTrace<'db> for AliasTerm<'db> {
    fn to_trace(cause: &ObligationCause, a: Self, b: Self) -> TypeTrace<'db> {
        TypeTrace { cause: *cause, values: ValuePairs::Aliases(ExpectedFound::new(a, b)) }
    }
}

impl<'db> ToTrace<'db> for FnSig<DbInterner<'db>> {
    fn to_trace(cause: &ObligationCause, a: Self, b: Self) -> TypeTrace<'db> {
        TypeTrace {
            cause: *cause,
            values: ValuePairs::PolySigs(ExpectedFound::new(Binder::dummy(a), Binder::dummy(b))),
        }
    }
}

impl<'db> ToTrace<'db> for PolyFnSig<'db> {
    fn to_trace(cause: &ObligationCause, a: Self, b: Self) -> TypeTrace<'db> {
        TypeTrace { cause: *cause, values: ValuePairs::PolySigs(ExpectedFound::new(a, b)) }
    }
}

impl<'db> ToTrace<'db> for PolyExistentialTraitRef<'db> {
    fn to_trace(cause: &ObligationCause, a: Self, b: Self) -> TypeTrace<'db> {
        TypeTrace {
            cause: *cause,
            values: ValuePairs::ExistentialTraitRef(ExpectedFound::new(a, b)),
        }
    }
}

impl<'db> ToTrace<'db> for PolyExistentialProjection<'db> {
    fn to_trace(cause: &ObligationCause, a: Self, b: Self) -> TypeTrace<'db> {
        TypeTrace {
            cause: *cause,
            values: ValuePairs::ExistentialProjection(ExpectedFound::new(a, b)),
        }
    }
}
