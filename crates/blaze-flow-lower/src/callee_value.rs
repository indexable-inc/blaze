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

impl<F: Facts, A: Allocator + Clone> Lower<'_, '_, F, A> {
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
        for k in ci.start + skip..ci.start + ci.len {
            for d in di.start..di.start + di.len {
                let (a, b) = (self.origs[k as usize], self.origs[d as usize]);
                self.b.subset(a, b);
                self.b.subset(b, a);
            }
        }
        Ok(true)
    }
}
