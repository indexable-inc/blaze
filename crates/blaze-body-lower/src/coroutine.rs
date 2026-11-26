//! Coroutines (`async fn`, `async { .. }`, `async move { .. }`, `.await`):
//!
//! - An `async` block expression evaluates to a coroutine aggregate (`AggregateKind::Coroutine`)
//!   holding its upvars in `Checked::env_fields` order, identical to a closure's environment.
//! - An `async fn`'s outer body is a normal function whose root is that `Expr::AsyncBlock`,
//!   moving every parameter into the coroutine value.
//! - [`lower_coroutine`] lowers an `Expr::AsyncBlock`'s inner body as its own [`Body`]:
//!   `_0` is the future's `Output`, arguments `1..=n` are the upvars (by value `T`, by ref `&T`,
//!   or by mut ref `&mut T`), and each `.await` desugars into `IntoFuture::into_future`, `Pin`,
//!   `Future::poll`, a `Poll::Ready` / `Poll::Pending` branch (with `Poll<!>` directly
//!   suspending), `StmtKind::Suspend { resume, cancel }`, and a `cancel` path dropping every live
//!   local (innermost first, upvars last) before `StmtKind::Cancelled`.
//! - Any body-local `PNode::Future(e, ..)` is canonicalized into `PNode::Coroutine { owner,
//!   ordinal, args }` in the body's type overlay so downstream passes (`blaze-body-drops`,
//!   `blaze-body-build`) never need the source AST to identify a coroutine.

use core::alloc::Allocator;

use blaze_body::{
    AggregateKind, BlockKind, Body, BorrowKind, Callee, LocalDecl, LocalKind, Operand, Place, Proj, Role, StmtKind,
    Value,
};
use blaze_own::Vec;
use blaze_typeck::coroutine_id::async_ordinal;
use blaze_typeck::{BodyView, CaptureKind, Checked, Expr, ExprId, List, Mutbl, PNode, PTy, PTys, Pat};

use crate::scope::Entry;
use crate::{At, BodyFacts, Construct, DiscrInt, Lower, LowerError, MOST_POSITIONS};

/// Library items needed to lower `.await` inside a coroutine body (`IntoFuture::into_future`
/// comes from `Checked::callees`; these supply `Pin`, `Future::poll`, and `Poll`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AsyncBodyItems {
    /// `core::future::Future::poll`.
    pub future_poll: blaze_typeck::ItemKey,
    /// `core::task::Poll`.
    pub poll: blaze_typeck::ItemKey,
    /// Variant index of `Poll::Ready` (0 in core).
    pub ready: u32,
    /// Variant index of `Poll::Pending` (1 in core).
    pub pending: u32,
    /// `core::pin::Pin`.
    pub pin: blaze_typeck::ItemKey,
    /// Discriminant of `Poll::Ready` (its integer type and bits).
    pub ready_discr: crate::Discr,
}

/// Lowers the body of async block expression `block` of `body` (checked as `ck`) to a [`Body`].
pub fn lower_coroutine<A: Allocator + Copy, F: BodyFacts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    block: ExprId,
    alloc: A,
) -> Result<Body<A>, LowerError> {
    let mut l = Lower::new(body, ck, facts, alloc);
    let Expr::AsyncBlock { body: inner, .. } = l.expr(block)? else {
        return l.refuse(Construct::Malformed, block);
    };
    l.in_coroutine = true;
    l.coroutine_frame(block)?;
    l.finish(inner)
}

/// Highest `Param(i)` / `ConstParam(i)` count referenced anywhere in `tys` (0 if none).
fn inferred_generics(tys: PTys<'_>) -> u32 {
    let mut count = 0u32;
    for i in 0..tys.len() {
        if let Ok(idx) = u32::try_from(i)
            && let Ok(PNode::Param(p) | PNode::ConstParam(p)) = tys.node(PTy(idx))
        {
            count = count.max(p.saturating_add(1));
        }
    }
    count
}

impl<F: BodyFacts, A: Allocator + Copy> Lower<'_, '_, '_, F, A> {
    /// The owner item's generic parameter list `[Param(0), .., Param(n - 1)]`, cached once.
    fn owner_param_list(&mut self, at: At) -> Result<List, LowerError> {
        if let Some(list) = self.owner_params {
            return Ok(list);
        }
        let n = self.facts.owner_generics().unwrap_or_else(|| inferred_generics(self.tys));
        let mut params: Vec<PTy, A> = Vec::with_capacity_in(usize::try_from(n).unwrap_or(0), self.alloc);
        for i in 0..n {
            params.push(self.find_ty(PNode::Param(i), at)?);
        }
        let list = self.b.intern_list(&params).map_err(|_| self.error_at(Construct::Malformed, at))?;
        self.owner_params = Some(list);
        Ok(list)
    }

    /// Rewrites any body-local `PNode::Future(fe, ..)` reachable from `ty` into canonical
    /// `PNode::Coroutine { owner, ordinal, args }` in the body's type overlay.
    pub(crate) fn canonical_ty(&mut self, ty: PTy, at: At) -> Result<PTy, LowerError> {
        if !self.has_async_blocks {
            return Ok(ty);
        }
        self.canonical_ty_iter(ty, at)
    }

    /// Rewrites every element of type list `list` via [`Self::canonical_ty`].
    pub(crate) fn canonical_list(&mut self, list: List, at: At) -> Result<List, LowerError> {
        if !self.has_async_blocks || list.len == 0 {
            return Ok(list);
        }
        let orig: Vec<PTy, A> = {
            let entries = self.list(list, at)?;
            let mut v = Vec::with_capacity_in(entries.len(), self.alloc);
            v.extend_from_slice(entries);
            v
        };
        let mut out: Vec<PTy, A> = Vec::with_capacity_in(orig.len(), self.alloc);
        let mut changed = false;
        for &t in orig.iter() {
            let ct = self.canonical_ty_iter(t, at)?;
            changed |= ct != t;
            out.push(ct);
        }
        if !changed {
            return Ok(list);
        }
        self.b.intern_list(&out).map_err(|_| self.error_at(Construct::Malformed, at))
    }

    fn canonical_ty_iter(&mut self, root: PTy, at: At) -> Result<PTy, LowerError> {
        enum Step {
            Visit(PTy),
            Rebuild { orig: PTy, node: PNode, list_len: usize },
        }
        let mut work: Vec<Step, A> = Vec::new_in(self.alloc);
        let mut vals: Vec<PTy, A> = Vec::new_in(self.alloc);
        work.push(Step::Visit(root));
        let mut steps = 0u32;
        while let Some(step) = work.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse_at(Construct::Bound, at);
            }
            match step {
                Step::Visit(t) => {
                    let node = self.node(t, at)?;
                    match node {
                        PNode::Future(fe, _, _) => {
                            let Some(ordinal) = async_ordinal(self.body, fe) else {
                                return self.refuse_at(Construct::Malformed, at);
                            };
                            let args = self.owner_param_list(at)?;
                            let ct = self.find_ty(PNode::Coroutine { owner: self.body.owner, ordinal, args }, at)?;
                            vals.push(ct);
                        }
                        PNode::Ref(_, inner) | PNode::Ptr(_, inner) | PNode::Slice(inner) | PNode::ConstValue { ty: inner, .. } => {
                            work.push(Step::Rebuild { orig: t, node, list_len: 0 });
                            work.push(Step::Visit(inner));
                        }
                        PNode::Array(elem, len) => {
                            work.push(Step::Rebuild { orig: t, node, list_len: 0 });
                            work.push(Step::Visit(len));
                            work.push(Step::Visit(elem));
                        }
                        PNode::Tuple(l)
                        | PNode::Record(_, l)
                        | PNode::Union(l)
                        | PNode::Adt(_, l)
                        | PNode::FnDef(_, l)
                        | PNode::FnPtr(l, _)
                        | PNode::Closure { args: l, .. }
                        | PNode::Coroutine { args: l, .. }
                        | PNode::Dynamic(l)
                        | PNode::ExTrait(_, l)
                        | PNode::Alias { args: l, .. }
                        | PNode::Opaque(_, l) => {
                            let entries: Vec<PTy, A> = {
                                let s = self.list(l, at)?;
                                let mut v = Vec::with_capacity_in(s.len(), self.alloc);
                                v.extend_from_slice(s);
                                v
                            };
                            if entries.is_empty() {
                                vals.push(t);
                            } else {
                                work.push(Step::Rebuild { orig: t, node, list_len: entries.len() });
                                for &child in entries.iter().rev() {
                                    work.push(Step::Visit(child));
                                }
                            }
                        }
                        PNode::Pat(base, l) => {
                            let entries: Vec<PTy, A> = {
                                let s = self.list(l, at)?;
                                let mut v = Vec::with_capacity_in(s.len(), self.alloc);
                                v.extend_from_slice(s);
                                v
                            };
                            work.push(Step::Rebuild { orig: t, node, list_len: entries.len() });
                            for &child in entries.iter().rev() {
                                work.push(Step::Visit(child));
                            }
                            work.push(Step::Visit(base));
                        }
                        PNode::ExProj { args: l, term, .. } => {
                            let entries: Vec<PTy, A> = {
                                let s = self.list(l, at)?;
                                let mut v = Vec::with_capacity_in(s.len(), self.alloc);
                                v.extend_from_slice(s);
                                v
                            };
                            work.push(Step::Rebuild { orig: t, node, list_len: entries.len() });
                            work.push(Step::Visit(term));
                            for &child in entries.iter().rev() {
                                work.push(Step::Visit(child));
                            }
                        }
                        PNode::Bool
                        | PNode::Char
                        | PNode::Int(_)
                        | PNode::Uint(_)
                        | PNode::Float(_)
                        | PNode::Str
                        | PNode::Never
                        | PNode::Param(_)
                        | PNode::ConstParam(_)
                        | PNode::ExAuto(_)
                        | PNode::Foreign(_)
                        | PNode::Infer
                        | PNode::Error => vals.push(t),
                    }
                }
                Step::Rebuild { orig, node, list_len } => {
                    let rebuilt = match node {
                        PNode::Ref(m, old) => {
                            let inner = vals.pop().ok_or_else(|| self.error_at(Construct::Malformed, at))?;
                            if inner == old { orig } else { self.find_ty(PNode::Ref(m, inner), at)? }
                        }
                        PNode::Ptr(m, old) => {
                            let inner = vals.pop().ok_or_else(|| self.error_at(Construct::Malformed, at))?;
                            if inner == old { orig } else { self.find_ty(PNode::Ptr(m, inner), at)? }
                        }
                        PNode::Slice(old) => {
                            let inner = vals.pop().ok_or_else(|| self.error_at(Construct::Malformed, at))?;
                            if inner == old { orig } else { self.find_ty(PNode::Slice(inner), at)? }
                        }
                        PNode::ConstValue { ty: old, bits } => {
                            let inner = vals.pop().ok_or_else(|| self.error_at(Construct::Malformed, at))?;
                            if inner == old { orig } else { self.find_ty(PNode::ConstValue { ty: inner, bits }, at)? }
                        }
                        PNode::Array(old_e, old_n) => {
                            let n = vals.pop().ok_or_else(|| self.error_at(Construct::Malformed, at))?;
                            let e = vals.pop().ok_or_else(|| self.error_at(Construct::Malformed, at))?;
                            if e == old_e && n == old_n { orig } else { self.find_ty(PNode::Array(e, n), at)? }
                        }
                        PNode::Pat(old_base, old_l) => {
                            let start = vals.len().checked_sub(list_len).ok_or_else(|| self.error_at(Construct::Malformed, at))?;
                            let new_slice = vals.get(start..).ok_or_else(|| self.error_at(Construct::Malformed, at))?;
                            let orig_slice = self.list(old_l, at)?;
                            let list_changed = new_slice != orig_slice;
                            let nl = if list_changed {
                                self.b.intern_list(new_slice).map_err(|_| self.error_at(Construct::Malformed, at))?
                            } else {
                                old_l
                            };
                            vals.truncate(start);
                            let base = vals.pop().ok_or_else(|| self.error_at(Construct::Malformed, at))?;
                            if base == old_base && !list_changed { orig } else { self.find_ty(PNode::Pat(base, nl), at)? }
                        }
                        PNode::ExProj { trait_, name, args: old_l, term: old_term } => {
                            let term = vals.pop().ok_or_else(|| self.error_at(Construct::Malformed, at))?;
                            let start = vals.len().checked_sub(list_len).ok_or_else(|| self.error_at(Construct::Malformed, at))?;
                            let new_slice = vals.get(start..).ok_or_else(|| self.error_at(Construct::Malformed, at))?;
                            let orig_slice = self.list(old_l, at)?;
                            let list_changed = new_slice != orig_slice;
                            let nl = if list_changed {
                                self.b.intern_list(new_slice).map_err(|_| self.error_at(Construct::Malformed, at))?
                            } else {
                                old_l
                            };
                            vals.truncate(start);
                            if term == old_term && !list_changed {
                                orig
                            } else {
                                self.find_ty(PNode::ExProj { trait_, name, args: nl, term }, at)?
                            }
                        }
                        other => {
                            let old_l = match other {
                                PNode::Tuple(l)
                                | PNode::Record(_, l)
                                | PNode::Union(l)
                                | PNode::Adt(_, l)
                                | PNode::FnDef(_, l)
                                | PNode::FnPtr(l, _)
                                | PNode::Closure { args: l, .. }
                                | PNode::Coroutine { args: l, .. }
                                | PNode::Dynamic(l)
                                | PNode::ExTrait(_, l)
                                | PNode::Alias { args: l, .. }
                                | PNode::Opaque(_, l) => l,
                                _ => return self.refuse_at(Construct::Malformed, at),
                            };
                            let start = vals.len().checked_sub(list_len).ok_or_else(|| self.error_at(Construct::Malformed, at))?;
                            let new_slice = vals.get(start..).ok_or_else(|| self.error_at(Construct::Malformed, at))?;
                            let orig_slice = self.list(old_l, at)?;
                            let list_changed = new_slice != orig_slice;
                            let nl = if list_changed {
                                self.b.intern_list(new_slice).map_err(|_| self.error_at(Construct::Malformed, at))?
                            } else {
                                old_l
                            };
                            vals.truncate(start);
                            if !list_changed {
                                orig
                            } else {
                                let new_node = match other {
                                    PNode::Tuple(_) => PNode::Tuple(nl),
                                    PNode::Record(s, _) => PNode::Record(s, nl),
                                    PNode::Union(_) => PNode::Union(nl),
                                    PNode::Adt(k, _) => PNode::Adt(k, nl),
                                    PNode::FnDef(k, _) => PNode::FnDef(k, nl),
                                    PNode::FnPtr(_, h) => PNode::FnPtr(nl, h),
                                    PNode::Closure { owner, ordinal, .. } => PNode::Closure { owner, ordinal, args: nl },
                                    PNode::Coroutine { owner, ordinal, .. } => PNode::Coroutine { owner, ordinal, args: nl },
                                    PNode::Dynamic(_) => PNode::Dynamic(nl),
                                    PNode::ExTrait(k, _) => PNode::ExTrait(k, nl),
                                    PNode::Alias { trait_, name, .. } => PNode::Alias { trait_, name, args: nl },
                                    PNode::Opaque(k, _) => PNode::Opaque(k, nl),
                                    _ => return self.refuse_at(Construct::Malformed, at),
                                };
                                self.find_ty(new_node, at)?
                            }
                        }
                    };
                    vals.push(rebuilt);
                }
            }
        }
        vals.pop().ok_or_else(|| self.error_at(Construct::Malformed, at))
    }

    /// Evaluates `Expr::AsyncBlock` `e` into `dest`: borrows or moves each captured binding into
    /// a `Value::Aggregate { kind: AggregateKind::Coroutine, ty, operands }` in capture order.
    pub(crate) fn coroutine_into(&mut self, dest: Place, e: ExprId) -> Result<(), LowerError> {
        let span = self.span(e);
        let at = At::Expr(e);
        let raw_ty = self.expr_ty(e)?;
        let ty = self.canonical_ty(raw_ty, at)?;
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
        self.b.assign(dest, Value::Aggregate { kind: AggregateKind::Coroutine, ty, operands }, span);
        Ok(())
    }

    /// Sets up the coroutine body frame for async block `block`: `_0` is the block's `Output`
    /// type, and arguments `1..=n` are the block's captures (by value `T`, or `&T` / `&mut T`).
    fn coroutine_frame(&mut self, block: ExprId) -> Result<(), LowerError> {
        let at = At::Expr(block);
        let raw_future = self.expr_ty(block)?;
        let PNode::Future(fe, output, _) = self.node(raw_future, at)? else {
            return self.refuse(Construct::Malformed, block);
        };
        if fe != block {
            return self.refuse(Construct::Malformed, block);
        }
        let out_ty = self.canonical_ty(output, at)?;
        self.return_place(out_ty);
        let caps = self.captures_of(block)?;
        for (index, &(p, kind)) in caps.iter().enumerate() {
            let vt_raw = self.local_ty(p)?;
            let vt = self.canonical_ty(vt_raw, At::Pat(p))?;
            let arg_ty = self.capture_ty(kind, vt, at)?;
            let mutbl = match kind {
                CaptureKind::ByValue => match self.pat(p)? {
                    Pat::Bind { ann, .. } if ann.mutable => Mutbl::Mut,
                    _ => Mutbl::Not,
                },
                CaptureKind::ByRef | CaptureKind::ByMutRef => Mutbl::Not,
            };
            let idx = u32::try_from(index).map_err(|_| self.error_at(Construct::Malformed, at))?;
            let local = self.b.new_local(LocalDecl { ty: arg_ty, kind: LocalKind::Arg(idx), mutbl });
            let arg_place = self.b.local_place(local);
            match kind {
                CaptureKind::ByValue => {
                    let drops = self.needs_drop(vt_raw, At::Pat(p))?;
                    self.sched.push(Entry { local, place: arg_place, drops, storage: false, cleanup: None });
                    self.bind(p, arg_place);
                }
                CaptureKind::ByRef | CaptureKind::ByMutRef => {
                    self.sched.push(Entry { local, place: arg_place, drops: false, storage: false, cleanup: None });
                    let deref = self.b.project(arg_place, Proj::Deref);
                    self.bind(p, deref);
                }
            }
        }
        Ok(())
    }

    /// Lowers `operand.await` at `e` into `dest` (The Reference, "Await expressions", steps 1–6).
    pub(crate) fn await_into(&mut self, dest: Place, e: ExprId, operand: ExprId) -> Result<(), LowerError> {
        if !self.in_coroutine {
            return self.refuse(Construct::Await, e);
        }
        let Some(items) = self.facts.async_items() else {
            return self.refuse(Construct::Await, e);
        };
        let span = self.span(e);
        let at = At::Expr(e);
        let Some((into_key, into_args_raw)) = self.callee_of(e) else {
            return self.refuse(Construct::Malformed, e);
        };
        let Some(held_raw) = self.ck.awaited_of(e) else {
            return self.refuse(Construct::Malformed, e);
        };
        let held = self.canonical_ty(held_raw, at)?;
        let out_raw = self.expr_ty(e)?;
        let out_ty = self.canonical_ty(out_raw, at)?;
        let into_args = self.canonical_list(into_args_raw, at)?;

        // Step 1: evaluate `operand` and call `IntoFuture::into_future(operand)` into `fut`.
        let op = self.operand(operand)?;
        let fut_local = self.b.new_local(LocalDecl { ty: held, kind: LocalKind::Temp(e, Role::Awaitee), mutbl: Mutbl::Mut });
        let fut_place = self.b.local_place(fut_local);
        let fut_drops = self.needs_drop(held_raw, at)?;
        self.b.push(StmtKind::StorageLive(fut_local), span);
        let into_callee = self.item_callee(into_key, into_args, e)?;
        let into_id = self.b.callee(into_callee);
        let into_ops = self.b.operands(&[op]);
        let head = self.b.new_block(BlockKind::Normal);
        let unwind_before_fut = self.unwind_here();
        self.b.terminate(
            StmtKind::Call { callee: into_id, args: into_ops, dest: Some(fut_place), target: Some(head), unwind: unwind_before_fut },
            span,
        );

        // `fut` is initialized upon reaching `head`: schedule it as the innermost live temporary
        // so both `poll`'s unwind path and `cancel` drop `fut` first.
        let fut_mark = self.sched.len();
        self.sched.push(Entry { local: fut_local, place: fut_place, drops: fut_drops, storage: true, cleanup: None });

        // Steps 2 & 3: pin `fut` (`&mut fut` wrapped in `Pin`) and call `Future::poll`.
        self.b.start(head);
        let pin_ref_ty = self.find_ty(PNode::Ref(Mutbl::Mut, held), at)?;
        let pin_ref_local = self.b.new_local(LocalDecl { ty: pin_ref_ty, kind: LocalKind::Temp(e, Role::Borrow), mutbl: Mutbl::Mut });
        let pin_ref_place = self.b.local_place(pin_ref_local);
        self.b.push(StmtKind::StorageLive(pin_ref_local), span);
        let pin_origin = self.b.origin(false);
        self.b.borrow(pin_ref_place, BorrowKind::Mut, fut_place, pin_origin, span);

        let pin_args = self.b.intern_list(&[pin_ref_ty]).map_err(|_| self.error_at(Construct::Malformed, at))?;
        let pin_ty = self.find_ty(PNode::Adt(items.pin, pin_args), at)?;
        let pin_local = self.b.new_local(LocalDecl { ty: pin_ty, kind: LocalKind::Temp(e, Role::Pin), mutbl: Mutbl::Mut });
        let pin_place = self.b.local_place(pin_local);
        self.b.push(StmtKind::StorageLive(pin_local), span);
        let pin_fields = self.b.operands(&[Operand::Move(pin_ref_place)]);
        self.b.assign(pin_place, Value::Aggregate { kind: AggregateKind::Adt(0), ty: pin_ty, operands: pin_fields }, span);
        self.b.push(StmtKind::StorageDead(pin_ref_local), span);

        let poll_args = self.b.intern_list(&[out_ty]).map_err(|_| self.error_at(Construct::Malformed, at))?;
        let poll_ty = self.find_ty(PNode::Adt(items.poll, poll_args), at)?;
        let poll_local = self.b.new_local(LocalDecl { ty: poll_ty, kind: LocalKind::Temp(e, Role::Result), mutbl: Mutbl::Mut });
        let poll_place = self.b.local_place(poll_local);
        self.b.push(StmtKind::StorageLive(poll_local), span);

        let poll_method_args = self.b.intern_list(&[held]).map_err(|_| self.error_at(Construct::Malformed, at))?;
        let poll_callee = self.b.callee(Callee::Method { key: items.future_poll, args: poll_method_args });
        let poll_call_args = self.b.operands(&[Operand::Move(pin_place)]);
        let check = self.b.new_block(BlockKind::Normal);
        let unwind_with_fut = self.unwind_here();
        self.b.terminate(
            StmtKind::Call {
                callee: poll_callee,
                args: poll_call_args,
                dest: Some(poll_place),
                target: Some(check),
                unwind: unwind_with_fut,
            },
            span,
        );

        // Step 4 & 5: inspect `Poll<Out>` (for `Poll<!>`, `Ready` is uninhabited so suspend
        // unconditionally).
        self.b.start(check);
        self.b.push(StmtKind::StorageDead(pin_local), span);
        let pending = self.b.new_block(BlockKind::Normal);
        let cancel = self.b.new_block(BlockKind::Normal);

        let is_never = matches!(self.node(out_raw, at)?, PNode::Never);
        if is_never {
            self.b.terminate(StmtKind::Goto(pending), span);

            self.b.start(pending);
            self.b.push(StmtKind::StorageDead(poll_local), span);
            self.b.terminate(StmtKind::Suspend { resume: head, cancel }, span);

            self.b.start(cancel);
            self.emit_exit(0, span)?;
            self.b.terminate(StmtKind::Cancelled, span);

            self.sched.truncate(fut_mark);
            self.dead_block();
            return Ok(());
        }

        let ready = self.b.new_block(BlockKind::Normal);
        let discr_node = match items.ready_discr.int {
            DiscrInt::Int(i) => PNode::Int(i),
            DiscrInt::Uint(u) => PNode::Uint(u),
        };
        let discr_ty = self.find_ty(discr_node, at)?;
        let discr_local = self.b.new_local(LocalDecl { ty: discr_ty, kind: LocalKind::Temp(e, Role::Value), mutbl: Mutbl::Mut });
        let discr_place = self.b.local_place(discr_local);
        self.b.push(StmtKind::StorageLive(discr_local), span);
        self.b.assign(discr_place, Value::Discriminant(poll_place), span);
        let cases = self.b.cases(&[(items.ready_discr.bits, ready)]);
        self.b.terminate(StmtKind::Switch { on: Operand::Copy(discr_place), cases, otherwise: pending }, span);

        // Pending: end transient poll locals and suspend; re-polling re-enters `head`.
        self.b.start(pending);
        self.b.push(StmtKind::StorageDead(discr_local), span);
        self.b.push(StmtKind::StorageDead(poll_local), span);
        self.b.terminate(StmtKind::Suspend { resume: head, cancel }, span);

        // Cancel: dropped while suspended at this `.await` — drop `fut` first, then all enclosing
        // scopes and by-value upvars, and terminate with `Cancelled`.
        self.b.start(cancel);
        self.emit_exit(0, span)?;
        self.b.terminate(StmtKind::Cancelled, span);

        // Step 6: Ready — move/copy payload out of `Poll::Ready`, drop `fut`, and continue.
        self.b.start(ready);
        self.b.push(StmtKind::StorageDead(discr_local), span);
        let downcast = self.b.project(poll_place, Proj::Downcast(items.ready));
        let payload = self.b.project(downcast, Proj::Field { index: 0, ty: out_ty });
        let value_op = if self.is_copy(out_raw, at)? { Operand::Copy(payload) } else { Operand::Move(payload) };
        self.b.assign(dest, Value::Use(value_op), span);
        self.b.push(StmtKind::StorageDead(poll_local), span);

        // Pop `fut` from `self.sched`, then drop `fut` (unwinding into everything below `fut`) and
        // end its storage so later awaits can reuse its state bytes.
        self.sched.truncate(fut_mark);
        if fut_drops {
            let after_drop = self.b.new_block(BlockKind::Normal);
            let unwind_after_fut = self.unwind_here();
            self.b.terminate(StmtKind::Drop { place: fut_place, target: after_drop, unwind: unwind_after_fut }, span);
            self.b.start(after_drop);
        }
        self.b.push(StmtKind::StorageDead(fut_local), span);
        Ok(())
    }
}
