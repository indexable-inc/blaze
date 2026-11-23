//! A call through a value of a closure type (`PNode::Closure`): `c()`, `c(x)`, `(&c)(x)`.
//!
//! The use of the closure value itself is made by `call_done` (a shared borrow for `Fn`, a
//! mutable one for `FnMut`, a move for `FnOnce`: the Reference, "Closure types", "Call traits
//! and coercions"), so calling an `FnMut` closure while a capture's loan is held conflicts, and
//! calling an `FnOnce` twice is a use after move (E0382).
//!
//! What the RESULT may hold: the closure's body is checked in its own flow (closure.rs), where
//! its parameters and its return place are placeholders bound per call (`External::Sig`) and
//! its captures are the value's positions (`External::Capture`). That flow proves nothing
//! between two `Sig` placeholders and nothing from a capture into the return place but what
//! the owner is told to prove (`Facts::closure_requirements`): a body that returns a borrow of
//! a parameter, or stores one argument into another, is a `NestedOutlives` refusal of the
//! item, never accepted. So what a call's result can hold that the owner must track is what
//! the closure value holds: every position of the value is related to every position of the
//! result, both ways (a result `&mut Vec<&'c u8>` may be written through, so its inner region
//! is invariant with the capture it came from). The arguments are not related to the result.
//!
//! Floor: one subset pair per (value position, result position); a closure value and a result
//! hold a handful of positions each, so the relation is small next to the call's own operands.
//!
//! The control `Plant::ClosureCallUnrelated` relates nothing: `let r = c(); drop(s); r` with
//! `c = || &s` is then accepted (callee_value_tests.rs).

use blaze_flow::Place;
use blaze_typeck::{ExprId, PNode};
use core::alloc::Allocator;

use crate::{At, Facts, Form, Lower, Plant, Refusal, Val};

impl<F: Facts, A: Allocator + Copy, R: crate::one_call::CallRule> Lower<'_, '_, F, A, R> {
    /// When the callee `vs[0]` is a closure value (or a reference to one, as `call_done` leaves
    /// an `Fn`/`FnMut` callee): relates the value's positions with `dest`'s (the module doc)
    /// and answers `true`; any other callee answers `false`, untouched.
    pub(crate) fn call_of_closure_value(&mut self, e: ExprId, vs: &[Val], dest: Place) -> Result<bool, Refusal> {
        let Some(&c) = vs.first() else { return self.refuse(Form::Malformed, e) };
        // How many leading positions of the callee's place are references to the closure (the
        // borrow `call_done` made, or a `&c` written), not the closure's own.
        let mut skip = u32::from(c.ty.wrap.is_some());
        let mut t = c.ty.ty;
        if c.ty.wrap.is_none()
            && let PNode::Ref(_, inner) = self.node(t, e)?
        {
            skip = 1;
            t = inner;
        }
        if !matches!(self.node(t, e)?, PNode::Closure { .. }) {
            return Ok(false);
        }
        let At::Place(cp) = c.at else { return self.refuse(Form::Malformed, e) };
        if self.plant == Plant::ClosureCallUnrelated {
            return Ok(true);
        }
        let (ci, di) = (self.info(cp), self.info(dest));
        if ci.len < skip {
            return self.refuse(Form::Malformed, e);
        }
        // THE ONE RULE (D190 item 2(b)): the result's type is the closure signature's output,
        // whose regions are the signature's own: a binder variable (fresh per call) or a free
        // region of the signature, which IS a slot of the closure's type (grain.rs: a closure's
        // slots are its captures' then its signature's). What the body moves from a capture
        // into its return place reaches the result only through those, and the body's flow
        // proves nothing from a capture into its return place but what it requires of the
        // owner (`Facts::closure_requirements`, applied at the closure expression among the
        // value's slots: lib.rs `closure_value`). So the call relates the arguments and the
        // result with the signature ([`Self::closure_call_sig`]) and nothing else: no
        // capture slot is tied to the result (rustc's closure call is `Fn::call` with the
        // closure type's signature, its captures never named). Only where the bodies are
        // checked (their requirements given); else the old relation below stands.
        let facts: &F = self.facts;
        if <R::Grain as crate::grain::Grain>::SLOTS && facts.closure_bodies_checked() {
            self.closure_call_sig(t, cp, skip, &vs[1..], dest, e)?;
            return Ok(true);
        }
        // Two or more value positions with any result position, or two result positions with
        // any of the value's, are made equal to each other: stricter than Rust's (the result's
        // regions are those the closure's signature names, each one of them): noted.
        let held = ci.len - skip;
        if held > 0 && di.len > 0 && held + di.len > 2 {
            self.coarse_step(Form::Unified { why: crate::Unified::ClosureValueCall }, e);
        }
        for k in ci.start + skip..ci.start + ci.len {
            for d in di.start..di.start + di.len {
                let (a, b) = (self.origs[k as usize], self.origs[d as usize]);
                self.b.subset(a, b);
                self.b.subset(b, a);
            }
        }
        // Each argument and the result with the signature positions they bind (D148:
        // closure.rs `closure_call_sig`).
        self.closure_call_sig(t, cp, skip, &vs[1..], dest, e)?;
        Ok(true)
    }
}
