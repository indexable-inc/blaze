//! Step 1 of an `.await` whose future is not its operand, `fut = into_future(op)`, related
//! through the head the type check recorded at the `.await` (piece await-intofuture).
//!
//! The type check selects the operand's `IntoFuture` impl with the body's solver and records
//! that impl's `into_future` at the impl's arguments as the `.await`'s callee
//! (blaze-typeck rules/await_impl.rs); a parameter's bound keeps the trait's `into_future`. So
//! the call is one of a KNOWN head, and is related as `call_done` relates any known head
//! (lib.rs), in the same order and by the same pieces: a declared `T: 'a` it owes is refused by
//! name (`Form::CallOutlivesUnread`); the head's lifetime part when it lines up
//! (`call_by_signature`), or instantiated with the generic arguments
//! (`call_by_generic_signature`, generic_call.rs); a head that writes lifetimes neither could
//! line up is refused by name (`Form::CallWritesLifetimes`: it may demand `'static`, which one
//! origin for all cannot say); else every position through one origin (`unify_call`), the
//! superset of any relation a signature that writes no lifetime can state. No head at all (a
//! record without the row) is refused by name (`Form::AwaitOperand`): never related as an
//! unknown call by default.
//!
//! The Reference, "Await expressions": "Create a future by calling `IntoFuture::into_future` on
//! the future operand"; the head's own signature (`fn into_future(self) -> Self::IntoFuture`)
//! is what its call may relate.

use blaze_flow::Place;
use core::alloc::Allocator;

use blaze_typeck::ExprId;

use crate::generic_call::CallBounds;
use crate::{Facts, Form, Lower, Refusal, TyD, Val};

impl<'b, 'c, F: Facts, A: Allocator + Clone> Lower<'b, 'c, F, A> {
    /// Relates `dest = into_future(v)` (the call statement just added) through the callee the
    /// record names at the `.await` `e` (the module doc).
    pub(crate) fn relate_into_future(&mut self, v: Val, dest: Place, dest_ty: TyD, e: ExprId) -> Result<(), Refusal> {
        let Some((item, g)) = self.callee_of(e) else {
            return self.refuse(Form::AwaitOperand, e);
        };
        let vs = [v];
        let facts: &'b F = self.facts;
        let sig = facts.callee(item);
        let mut generic_origins = false;
        for &a in self.list(g, e)? {
            generic_origins |= self.npos(a, e)? > 0;
        }
        let owes = if facts.sig(item).is_some() {
            match facts.call_bounds(item) {
                Some(b) => self.owes_type_outlives(b, g, e)?,
                None => self.owes_type_outlives(CallBounds { outlives: &[], unread_below: u32::MAX, trait_bounded: &[] }, g, e)?,
            }
        } else {
            None
        };
        if let Some(param) = owes {
            return self.refuse(Form::CallOutlivesUnread { callee: item, param }, e);
        }
        if !generic_origins
            && let Some(life) = facts.sig(item)
            && self.call_by_signature(life, Some((item, g)), &vs, dest, dest_ty, e)?
        {
            self.stats.calls_by_signature += 1;
            return Ok(());
        }
        if sig.is_some_and(|s| generic_origins || s.writes_lifetimes)
            && let Some(life) = facts.sig(item)
            && let Some(shape) = facts.shape(item)
            && let Some(bounds) = facts.call_bounds(item)
            && self.call_by_generic_signature(item, life, shape, bounds, g, &vs, dest, dest_ty, e)?
        {
            self.stats.calls_by_signature += 1;
            return Ok(());
        }
        if facts.sig(item).is_some_and(|l| l.holds_hidden()) {
            self.stats.calls_hidden += 1;
        }
        if sig.is_some_and(|s| s.writes_lifetimes) {
            return self.refuse(Form::CallWritesLifetimes { callee: item, lacks: self.call_lacks(item) }, e);
        }
        self.unify_call(&vs, dest, e)?;
        self.stats.calls_unified += 1;
        Ok(())
    }
}
