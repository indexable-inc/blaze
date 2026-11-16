//! The goal side: the trait a solver (blaze-solve) implements, and [`Ctx`], the inference
//! context with a solver behind it, which hands every goal relating emits to the solver.
//!
//! This is the half of rustc's split between `InferCtxt` (here [`TyCx`]: variables, relating,
//! folding, canonicalization) and the solver's `SolverDelegate` / `FulfillmentCtxt`
//! (rustc_next_trait_solver/src/delegate.rs, rustc_trait_selection/src/solve/fulfill.rs)
//! that the type checker sees: register obligations, select, ask, normalize.

use blaze_own::{Alloc, Vec};

use crate::cx::{Snapshot, TyCx};
use crate::error::TypeError;
use crate::fold::VarianceOf;
use crate::ids::{Arg, Pred, Ty, Variance};

/// A goal to prove and why it was made (an opaque cause the caller maps to a span).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Obligation {
    pub pred: Pred,
    pub cause: u32,
}

/// A trait solver over one task's [`TyCx`].
pub trait Solver<'a> {
    /// Why selecting failed: an obligation that cannot hold, or one past the solver's bounds.
    type Error;
    /// The solver's own undo point, taken with the context's.
    type Snapshot: Copy;

    fn snapshot(&mut self) -> Self::Snapshot;
    fn rollback_to(&mut self, s: Self::Snapshot);
    fn commit(&mut self, s: Self::Snapshot);

    /// Adds a goal to prove; nothing is proved yet.
    fn register(&mut self, cx: &mut TyCx<'a>, o: Obligation);
    /// Proves what it can now, binding variables; leaves ambiguous goals pending.
    fn select_where_possible(&mut self, cx: &mut TyCx<'a>) -> Result<(), Self::Error>;
    /// Proves everything pending; an ambiguous goal is an error now.
    fn select_all(&mut self, cx: &mut TyCx<'a>) -> Result<(), Self::Error>;
    /// Whether `pred` may hold, leaving no trace (rustc's `predicate_may_hold`).
    fn may_hold(&mut self, cx: &mut TyCx<'a>, pred: Pred) -> bool;
    /// `t` with its outermost alias normalized, or a variable standing for it with a goal
    /// registered (rustc's `structurally_normalize_ty`).
    fn normalize(&mut self, cx: &mut TyCx<'a>, t: Ty) -> Result<Ty, Self::Error>;
}

/// An inference context with its solver.
pub struct Ctx<'a, S> {
    pub cx: TyCx<'a>,
    pub solver: S,
    cause: u32,
    goals: Vec<Pred, Alloc<'a>>,
}

impl<'a, S: Solver<'a>> Ctx<'a, S> {
    pub fn new(cx: TyCx<'a>, solver: S, alloc: Alloc<'a>) -> Ctx<'a, S> {
        Ctx { cx, solver, cause: 0, goals: Vec::new_in(alloc) }
    }

    /// The cause later goals carry.
    pub fn set_cause(&mut self, cause: u32) {
        self.cause = cause;
    }

    fn hand_over(&mut self) {
        self.goals.clear();
        self.cx.take_goals(&mut self.goals);
        for &pred in self.goals.iter() {
            self.solver.register(&mut self.cx, Obligation { pred, cause: self.cause });
        }
    }

    /// `a == b`, its goals registered with the solver.
    pub fn eq(&mut self, a: Arg, b: Arg, vo: &mut dyn VarianceOf) -> Result<bool, TypeError> {
        let r = self.cx.eq(a, b, vo);
        self.hand_over();
        r
    }

    /// `a <: b`, its goals registered with the solver.
    pub fn sub(&mut self, a: Ty, b: Ty, vo: &mut dyn VarianceOf) -> Result<bool, TypeError> {
        let r = self.cx.relate(a.arg(), b.arg(), Variance::Covariant, vo);
        self.hand_over();
        r
    }

    /// See [`TyCx::unify_structurally`]: no goal is made.
    pub fn unify_structurally(&mut self, a: Arg, b: Arg, vo: &mut dyn VarianceOf) -> Result<bool, TypeError> {
        self.cx.unify_structurally(a, b, vo)
    }

    pub fn register(&mut self, pred: Pred) {
        let o = Obligation { pred, cause: self.cause };
        self.solver.register(&mut self.cx, o);
    }

    pub fn select_where_possible(&mut self) -> Result<(), S::Error> {
        self.solver.select_where_possible(&mut self.cx)
    }

    pub fn select_all(&mut self) -> Result<(), S::Error> {
        self.solver.select_all(&mut self.cx)
    }

    pub fn may_hold(&mut self, pred: Pred) -> bool {
        self.solver.may_hold(&mut self.cx, pred)
    }

    pub fn normalize(&mut self, t: Ty) -> Result<Ty, S::Error> {
        self.solver.normalize(&mut self.cx, t)
    }

    pub fn snapshot(&mut self) -> (Snapshot, S::Snapshot) {
        (self.cx.snapshot(), self.solver.snapshot())
    }

    pub fn rollback_to(&mut self, s: (Snapshot, S::Snapshot)) {
        self.solver.rollback_to(s.1);
        self.cx.rollback_to(s.0);
    }

    pub fn commit(&mut self, s: (Snapshot, S::Snapshot)) {
        self.solver.commit(s.1);
        self.cx.commit(s.0);
    }
}
