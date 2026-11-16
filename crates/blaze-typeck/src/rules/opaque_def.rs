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
//! - An `async fn` is not lowered here: its body has `CoroutineKind::Async { output }` and is
//!   checked against the written `T`; its hidden type is the body's own future, which the head's
//!   bounds (`Future<Output = T>`) describe by construction.
//!
//! Owed: the hidden types are not yet written to `Checked` (writeback), so the world still
//! answers `opaque_hidden` with `None` and an auto-trait goal on an opaque (`Send`) stays
//! ambiguous (blaze-solve structural.rs: "ambiguous until it does").

use crate::error::Stop;
use crate::ids::ItemKey;
use crate::machine::{Lower, Machine};
use crate::portable::{PTy, PTys};
use crate::ty::{Cause, Goal, Ty, TypeCtx};
use crate::world::{PClause, World};

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// The owner's output `t` of `types` with each opaque it defines as its hidden-type
    /// variable.
    pub(crate) fn lower_defining_output(&mut self, types: PTys<'_>, t: PTy) -> Result<Ty, Stop> {
        #[cfg(test)]
        if self.plant == crate::machine::Plant::OpaqueNotOpened {
            return self.lower_sig_ty(types, t);
        }
        self.opening = true;
        let got = self.lower_sig_ty(types, t);
        self.opening = false;
        got
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

    /// A goal left ambiguous whose self type is an opaque: its hidden type is not recorded (the
    /// world answers `opaque_hidden` with `None`), so an auto trait of it (`Send`, which "leaks"
    /// from the hidden type) has no answer yet. A typed gap naming the form, never E0283.
    pub(crate) fn opaque_ambiguity_gap(&mut self, goal: Goal) -> Option<crate::error::CheckError> {
        let Goal::Trait { args, .. } = goal else { return None };
        if self.ty.list_len(args) == 0 {
            return None;
        }
        let this = self.ty.shallow_resolve(self.ty.list_get(args, 0));
        match self.view(this) {
            crate::ty::TyView::Opaque(..) => {
                Some(crate::error::CheckError::Unported("an auto trait of an opaque type: its hidden type is not recorded yet"))
            }
            _ => None,
        }
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
