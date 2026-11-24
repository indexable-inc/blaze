//! The DEFINING body of a return-position `impl Trait`: the function whose written output names
//! the opaque. The Reference, "Impl trait", "Abstract return types": "These types stand in for
//! another concrete type where the caller may only use the methods declared by the specified
//! Trait" (suite/opaque.rs is the caller's
//! side). [type.impl-trait.return.constraint-body]: "Each possible return value from the
//! function must resolve to the same concrete type"; and "the function chooses the return
//! type, but only promises that it will implement Trait".
//!
//! The rule as built (the call site is rules/control.rs `start_body`):
//! - The owner's output is lowered with each opaque in it as ONE inference variable per opaque
//!   key (`Machine::defining`): the opaque's hidden type. Every returned expression and the tail
//!   are coerced into the body's one return site whose type holds those variables, so two
//!   returns of different types are the ordinary E0308 at the second (the Reference's "one
//!   type"). Only the output is lowered this way: inside the body (a recursive call) the
//!   opaque stays opaque.
//! - Before the walk (rustc's order, so a returned closure is typed from its `Fn` bound) each
//!   opaque's bounds (`predicates(k)`, written with the opaque as `Self`) are registered as
//!   obligations on the hidden type, at the body's root; they are decided after the walk with
//!   every other obligation: a hidden type that
//!   lacks a bound is the solver's E0277 naming that bound. `Sized` is among them (the head
//!   adds it unless `?Sized` is written).
//! - An `async fn` (the Reference, "Async functions": `async fn f(x: A) -> T { body }` "is
//!   roughly equivalent to" `fn f(x: A) -> impl Future<Output = T> { async move { body } }`):
//!   blaze-lower makes the root that `async move` block (desugar.rs `async_fn`), so the fn
//!   defines its `impl Future` as any other opaque: the hidden type is the block's future
//!   (`TyView::Future`), and the block's output is the written `T` (`async_fn_output`), so a
//!   wrong returned value is E0308 at the value. A body marked async whose root is not an
//!   async block (hand-built fixtures) is still checked against the written `T` directly.
//! - Each defined opaque's hidden type is written to the record (`Checked::hidden`, by
//!   writeback through `defined_hidden`).
//!
//! A CALLER's auto-trait goal on an opaque (`Send`) whose hidden type the world did not give
//! stays ambiguous in the solver (blaze-solve structural.rs: "ambiguous until it does") and is
//! the typed gap `CheckError::OpaqueHiddenUnknown { opaque, trait_ }` of
//! `opaque_ambiguity_gap`; blaze-ladder's front2 (opaque_auto.rs) then has the opaque's defining
//! body checked and checks the caller again with the hidden type given.

use crate::error::Stop;
use crate::ids::{ExprId, ItemKey};
use crate::machine::{Lower, Machine};
use crate::portable::{PTy, PTys};
use crate::ty::{Cause, Goal, Ty, TypeCtx};
use crate::world::{PClause, World};

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// The owner's output `t` of `types` with each opaque it defines as its hidden-type
    /// variable; a projection in it that fails is a finding at `cause`.
    pub(crate) fn lower_defining_output(&mut self, types: PTys<'_>, t: PTy, cause: Cause) -> Result<Ty, Stop> {
        #[cfg(test)]
        if self.plant == crate::machine::Plant::OpaqueNotOpened {
            return self.lower_sig_ty(types, t, cause);
        }
        self.opening = true;
        let got = self.lower_sig_ty(types, t, cause);
        self.opening = false;
        got
    }

    /// Whether this body is an `async fn`'s in the Reference's form ("Async functions":
    /// `async fn f(x: A) -> T { body }` "is roughly equivalent to" `fn f(x: A) -> impl
    /// Future<Output = T> { async move { body } }`): marked async, its root the async block.
    pub(crate) fn async_root(&mut self) -> Result<bool, Stop> {
        if self.body.coroutine.is_none() {
            return Ok(false);
        }
        Ok(matches!(self.body.expr(self.body.root)?, crate::body::Expr::AsyncBlock { .. }))
    }

    /// The output of the async block `e`, when `e` is the root block of an `async fn` (see
    /// [`Self::async_root`]): the fn's WRITTEN return type, so a returned value of another type
    /// is E0308 at that value, not a failed `Future<Output = T>` bound at the block. An opaque
    /// the written type names (`async fn f() -> impl Tr`) is this body's to define too: it is
    /// lowered as its hidden-type variable, the same variable the `Output = ..` bound names.
    pub(crate) fn async_fn_output(&mut self, e: ExprId) -> Result<Option<Ty>, Stop> {
        if e != self.body.root || !self.async_root()? {
            return Ok(None);
        }
        let Some(crate::body::CoroutineKind::Async { output }) = self.body.coroutine else { return Ok(None) };
        self.opening = true;
        let got = self.lower_body_ty(output, crate::diag::At::Expr(e));
        self.opening = false;
        Ok(Some(got?))
    }

    /// Each opaque this body defined with its hidden type, for the record (`Checked::hidden`),
    /// in the order the opaques were met in the signature. An opaque whose hidden type is still
    /// an inference variable (a body that never returns, or one with errors) has no row.
    pub(crate) fn defined_hidden(&mut self) -> Result<blaze_own::Vec<(ItemKey, Ty), blaze_own::Alloc<'a>>, crate::error::CheckError> {
        let mut out: blaze_own::Vec<(ItemKey, Ty), blaze_own::Alloc<'a>> = blaze_own::Vec::with_capacity_in(self.defining.len(), self.alloc);
        for i in 0..self.defining.len() {
            let (k, v) = self.defining[i];
            let t = self.ty.resolve(v)?;
            if !self.is_ty_var(t) && !self.is_err(t) {
                out.push((k, t));
            }
        }
        Ok(out)
    }

    /// The hidden-type variable of the opaque `k` this body defines, made on first use.
    pub(crate) fn defining_var(&mut self, k: ItemKey) -> Result<Ty, Stop> {
        if let Some(&(_, v)) = self.defining.iter().find(|(d, _)| *d == k) {
            return Ok(v);
        }
        let v = self.fresh()?;
        self.defining.push((k, v));
        Ok(v)
    }

    /// Registers each defined opaque's bounds on its hidden type, at the body's root.
    pub(crate) fn close_opaques(&mut self) -> Result<(), Stop> {
        let cause = Cause::Expr(self.body.root);
        self.opening = true;
        let mut i = 0;
        let mut got = Ok(());
        while i < self.defining.len() && got.is_ok() {
            got = self.close_one(self.defining[i].0, cause);
            i += 1;
        }
        self.opening = false;
        got
    }

    /// A goal left ambiguous on an opaque whose hidden type is not recorded (the world answers
    /// `opaque_hidden` with `None`): an auto trait of it (`Send`, which "leaks" from the hidden
    /// type) has no answer yet. A typed gap naming the opaque, never E0283.
    ///
    /// 1. The goal's SELF type is the opaque: only an AUTO trait is the gap (the Reference,
    ///    "Abstract return types": the caller "may only use the methods declared by the
    ///    specified Trait", auto traits excepted: they are read from the hidden type); any other
    ///    ambiguous trait goal on an opaque is decided from its bounds alone, so its ambiguity
    ///    is the ordinary E0283.
    /// 2. Otherwise the auto trait may be reached INSIDE the goal ("Auto trait leakage" holds
    ///    at any depth): `Option<O>: Send` structurally, or `Pin<Box<O>>: CoerceUnsized<Pin<Box<dyn
    ///    Future + Send>>>` through two impls and an unsizing (rung s9-app-03). The solver does
    ///    not say which leaf was ambiguous, so the first opaque met in the goal's types (left to
    ///    right, outermost first) that this body does not define and whose hidden type the world
    ///    does not give is the gap. A superset: if that opaque was not the cause, the next round
    ///    has its hidden type and reports the ordinary E0283 (this rule then passes it by).
    pub(crate) fn opaque_ambiguity_gap(&mut self, goal: Goal) -> Result<Option<crate::error::CheckError>, Stop> {
        // The most type nodes one goal's walk visits (rule 18); interned types are acyclic, so
        // only a shared subtree met many times can reach it.
        const MOST_GOAL_NODES: u32 = 1 << 16;
        let Goal::Trait { trait_, args } = goal else { return Ok(None) };
        let n = self.ty.list_len(args);
        if n == 0 {
            return Ok(None);
        }
        let this = self.ty.shallow_resolve(self.ty.list_get(args, 0));
        if let crate::ty::TyView::Opaque(opaque, _) = self.view(this) {
            let auto = match self.reads.trait_def(trait_)? {
                Some(def) => def.flags.0 & crate::world::TraitFlags::AUTO != 0,
                None => false,
            };
            return Ok(auto.then_some(crate::error::CheckError::OpaqueHiddenUnknown { opaque, trait_ }));
        }
        let mut work: blaze_own::Vec<Ty, blaze_own::Alloc<'a>> = blaze_own::Vec::new_in(self.alloc);
        let mut kids: blaze_own::Vec<Ty, blaze_own::Alloc<'a>> = blaze_own::Vec::new_in(self.alloc);
        for i in (0..n).rev() {
            work.push(self.ty.list_get(args, i));
        }
        let mut met = 0u32;
        while let Some(t) = work.pop() {
            met += 1;
            if met > MOST_GOAL_NODES {
                return Err(crate::error::CheckError::Limit(crate::error::Limit::Steps).into());
            }
            let v = self.view(t);
            if let crate::ty::TyView::Opaque(opaque, _) = v
                && !self.defining.iter().any(|&(d, _)| d == opaque)
                && self.reads.opaque_hidden(opaque)?.is_none()
            {
                return Ok(Some(crate::error::CheckError::OpaqueHiddenUnknown { opaque, trait_ }));
            }
            kids.clear();
            self.ty_children(v, &mut kids);
            while let Some(k) = kids.pop() {
                work.push(k);
            }
            if let Some((_, Some(env))) = self.closure_witness(v) {
                // The undecided requirement may belong to a captured opaque.
                // The signature alone never mentions a discarded capture.
                work.push(env);
            }
        }
        Ok(None)
    }

    fn close_one(&mut self, k: ItemKey, cause: Cause) -> Result<(), Stop> {
        let preds = self.reads.predicates(k)?;
        for clause in preds.clauses {
            let goal = match *clause {
                PClause::Trait { trait_, args } => Goal::Trait { trait_, args: self.lower_list_for(preds.types, args, cause)? },
                PClause::Projection { trait_, args, name, term } => {
                    let args = self.lower_list_for(preds.types, args, cause)?;
                    let term = self.lower(preds.types, term, Lower::Body, cause)?;
                    Goal::Projection { trait_, args, name, term }
                }
                PClause::Const { cond, args } => Goal::ConstHolds { cond, args: self.lower_list_for(preds.types, args, cause)? },
                PClause::ForFields { container, trait_, args } => Goal::ForFields {
                    container: self.lower(preds.types, container, Lower::Body, cause)?,
                    trait_,
                    args: self.lower_list_for(preds.types, args, cause)?,
                },
                // Outlives clauses are the borrow checker's (D148); the checker takes them as true.
                PClause::TypeOutlives { .. } | PClause::RegionOutlives { .. } => continue,
            };
            self.ty.register(crate::ty::Obligation { goal, cause })?;
        }
        Ok(())
    }

    fn lower_list_for(&mut self, types: PTys<'_>, l: crate::ids::List, cause: Cause) -> Result<crate::ty::TyList, Stop> {
        let src = types.list(l)?;
        let mut v: blaze_own::Vec<Ty, blaze_own::Alloc<'a>> = blaze_own::Vec::with_capacity_in(src.len(), self.alloc);
        for &t in src {
            v.push(self.lower(types, t, Lower::Body, cause)?);
        }
        self.mk_list(&v)
    }
}
