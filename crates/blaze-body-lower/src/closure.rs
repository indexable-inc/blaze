//! Closures: the value a closure expression makes, and a closure's own body with its captures
//! as places.
//!
//! The Reference, "Closure expressions" and "Closure types": a closure is a value holding its
//! captures, each by shared reference, by unique (mutable) reference, or by value
//! (`Checked::captures`, in the record's order). The closure value is one aggregate of them in
//! that order. In the closure's body, argument `_1` is the environment (`&C` for `Fn`, `&mut C`
//! for `FnMut`, `C` for `FnOnce`; `Checked::closures`), then the closure's parameters; a use of
//! captured variable `x` at index `i` is the place `env.i` (by value) or `*env.i` (by
//! reference), where `env` is `*_1` or `_1`.

use core::alloc::Allocator;

use blaze_body::{AggregateKind, Body, BorrowKind, LocalDecl, LocalKind, Operand, Place, Proj, Role, Value};
use blaze_own::Vec;
use blaze_typeck::{BodyView, CaptureKind, Checked, ClosureKind, Expr, ExprId, List, Mutbl, PNode, PTy, PatId};

use crate::scope::Entry;
use crate::{At, BodyFacts, Construct, Lower, LowerError};

/// Lowers the body of closure expression `closure` of `body` (checked as `ck`) to a [`Body`].
pub fn lower_closure<A: Allocator + Copy, F: BodyFacts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    closure: ExprId,
    alloc: A,
) -> Result<Body<A>, LowerError> {
    let mut l = Lower::new(body, ck, facts, alloc);
    let Expr::Closure { params, body: cb, is_async, .. } = l.expr(closure)? else {
        return l.refuse(Construct::Malformed, closure);
    };
    if is_async {
        return l.refuse(Construct::AsyncBlock, closure);
    }
    l.closure_frame(closure, params, cb)?;
    l.finish(cb)
}

impl<F: BodyFacts, A: Allocator + Copy> Lower<'_, '_, '_, F, A> {
    /// The captures of closure `c`, in its environment's field order (blaze-typeck
    /// env_fields.rs, the active strategy): each captured binding and how. A field holding a
    /// place below its binding is refused by name (`Construct::CapturedPlace`): its field is the
    /// place's value, which this lowering does not build yet.
    fn captures_of(&self, c: ExprId) -> Result<Vec<(PatId, CaptureKind), A>, LowerError> {
        let mut out = Vec::new_in(self.alloc);
        for f in self.ck.env_fields(c) {
            if !f.is_whole() {
                return self.refuse(Construct::CapturedPlace, c);
            }
            out.push((f.base, f.how));
        }
        Ok(out)
    }

    /// The type of capture of `x` (its variable's type `t`) as a field of the closure.
    fn capture_ty(&mut self, kind: CaptureKind, t: PTy, at: At) -> Result<PTy, LowerError> {
        match kind {
            CaptureKind::ByValue => Ok(t),
            CaptureKind::ByRef => self.find_ty(PNode::Ref(Mutbl::Not, t), at),
            CaptureKind::ByMutRef => self.find_ty(PNode::Ref(Mutbl::Mut, t), at),
        }
    }

    /// The closure value of closure expression `e` into `dest`: each capture borrowed or moved
    /// in, in the record's order, as one aggregate.
    pub(crate) fn closure_into(&mut self, dest: Place, e: ExprId) -> Result<(), LowerError> {
        let span = self.span(e);
        let at = At::Expr(e);
        let ty = self.expr_ty(e)?;
        let caps = self.captures_of(e)?;
        let mut ops: Vec<Operand, A> = Vec::new_in(self.alloc);
        for &(p, kind) in caps.iter() {
            let place = self.bound(p, e)?;
            let vt = self.local_ty(p)?;
            let op = match kind {
                CaptureKind::ByValue => {
                    if self.is_copy(vt, at)? {
                        Operand::Copy(place)
                    } else {
                        Operand::Move(place)
                    }
                }
                CaptureKind::ByRef | CaptureKind::ByMutRef => {
                    let rt = self.capture_ty(kind, vt, at)?;
                    let t = self.temp(e, rt, Role::Borrow)?;
                    let o = self.b.origin(false);
                    if kind == CaptureKind::ByRef {
                        self.b.borrow(t, BorrowKind::Shared, place, o, span);
                        Operand::Copy(t)
                    } else {
                        self.b.borrow(t, BorrowKind::Mut, place, o, span);
                        Operand::Move(t)
                    }
                }
            };
            ops.push(op);
        }
        let operands = self.b.operands(&ops);
        self.b.assign(dest, Value::Aggregate { kind: AggregateKind::Closure, ty, operands }, span);
        Ok(())
    }

    /// The frame of closure `closure`'s body `cb`: `_0`, the environment `_1`, the parameters,
    /// and each capture's place bound to the variable it captures.
    pub(crate) fn closure_frame(&mut self, closure: ExprId, params: List, cb: ExprId) -> Result<(), LowerError> {
        let at = At::Expr(closure);
        let ret_ty = self.adjusted_ty(cb)?;
        self.return_place(ret_ty);
        let cty = self.expr_ty(closure)?;
        let cols = self.ck.closures.column::<0>();
        let Some(row) = cols.iter().position(|x| *x == closure) else { return self.refuse(Construct::Malformed, closure) };
        let Some(&kind) = self.ck.closures.column::<1>().get(row) else {
            return self.refuse(Construct::Malformed, closure);
        };
        let env_ty = match kind {
            ClosureKind::Fn => self.find_ty(PNode::Ref(Mutbl::Not, cty), at)?,
            ClosureKind::FnMut => self.find_ty(PNode::Ref(Mutbl::Mut, cty), at)?,
            ClosureKind::FnOnce => cty,
        };
        let env = self.b.new_local(LocalDecl { ty: env_ty, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
        let env_place = self.b.local_place(env);
        let pats = self.exprs_param_pats(params, closure)?;
        let mut args: Vec<(PatId, blaze_body::Local, Place, PTy), A> = Vec::new_in(self.alloc);
        for (i, &p) in pats.iter().enumerate() {
            let ty = self.pat_ty(p)?;
            let local = self.b.new_local(LocalDecl { ty, kind: LocalKind::Arg(1 + i as u32), mutbl: Mutbl::Not });
            let place = self.b.local_place(local);
            args.push((p, local, place, ty));
        }
        let base = match kind {
            ClosureKind::FnOnce => env_place,
            _ => self.b.project(env_place, Proj::Deref),
        };
        let caps = self.captures_of(closure)?;
        for (index, &(p, ck)) in caps.iter().enumerate() {
            let vt = self.local_ty(p)?;
            let fty = self.capture_ty(ck, vt, at)?;
            let f = self.b.project(base, Proj::Field { index: index as u32, ty: fty });
            let place = match ck {
                CaptureKind::ByValue => f,
                _ => self.b.project(f, Proj::Deref),
            };
            self.bind(p, place);
        }
        let env_drops = kind == ClosureKind::FnOnce && self.needs_drop(cty, at)?;
        self.sched.push(Entry { local: env, place: env_place, drops: env_drops, storage: false, cleanup: None });
        for &(p, local, place, ty) in args.iter() {
            let drops = self.needs_drop(ty, At::Pat(p))?;
            self.sched.push(Entry { local, place, drops, storage: false, cleanup: None });
            if self.simple_bind(p)? {
                self.bind(p, place);
                continue;
            }
            let mut out = Vec::new_in(self.alloc);
            self.bind_irrefutable(p, place, ty, &mut out)?;
            self.schedule(&out);
        }
        Ok(())
    }

    /// A closure's parameter patterns (its `params` list, of `BodyView::pat_lists`).
    fn exprs_param_pats(&self, params: List, closure: ExprId) -> Result<Vec<PatId, A>, LowerError> {
        let mut out = Vec::new_in(self.alloc);
        match self.body.pat_lists.get(params.range()) {
            Some(s) => out.extend_from_slice(s),
            None => return self.refuse(Construct::Malformed, closure),
        }
        Ok(out)
    }
}
