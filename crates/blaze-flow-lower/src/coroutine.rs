//! An async block's body as its own flow, and its exact interior (port27/tempo-run/ASYNC.md
//! D-A2, prompt P2).
//!
//! The Reference, "Async blocks": "An async block is a variant of a block expression which
//! evaluates to a future. The final expression of the block, if present, determines the result
//! value of the future." "Capture modes": "Async blocks capture variables from their environment
//! using the same capture modes as closures." "The future type that rustc generates is roughly
//! equivalent to an enum with one variant per await point, where each variant stores the data
//! needed to resume from its corresponding point." So the body is checked as a body of its own:
//! its upvars are its arguments (the block's rows of `Checked::captures`, in row order, as
//! arguments `1..=n`: by value the captured type, by reference a reference argument, every
//! position a placeholder origin, as a closure body's captures would be), and `return` in it is
//! its own return.
//!
//! `.await` (the Reference, "Await expressions", steps 1 to 6, quoted in
//! docs/v2/plan/design/coroutines.md lines 16-27: "1. Create a future by calling
//! `IntoFuture::into_future` on the future operand. 2. Evaluate the future to a future `tmp`;
//! 3. Pin `tmp` using `Pin::new_unchecked`; 4. This pinned future is then polled by calling the
//! `Future::poll` method and passing it the current task context; 5. If the call to `poll`
//! returns `Poll::Pending`, then the future returns `Poll::Pending`, suspending its state so
//! that, when the surrounding async context is re-polled, execution returns to step 3; 6.
//! Otherwise the call to `poll` must have returned `Poll::Ready`, in which case the value
//! contained in the `Poll::Ready` variant is used as the result of the await expression itself")
//! is lowered as:
//!
//! ```text
//!     fut = Call into_future(op)              // the checker's callee row of the await
//!     goto H
//! H:  StorageLive(pin); pin = &mut fut        // BorrowKind::Mut
//!     StorageLive(r); r = Call poll(move pin); StorageDead(pin)
//!     Switch r -> [R, P]
//! P:  StorageDead(r); Suspend { resume: H, cancel: C }
//! C:  the drops a `return` here makes (fut first: the innermost temporary); _0 = const; Return
//! R:  Drop(fut) if it needs one; StorageDead(fut); r is the await's value (a temporary)
//! ```
//!
//! Three choices, each stated:
//! - `fut`'s type is the record's: the type checker decided `<T as IntoFuture>::IntoFuture` when
//!   it checked the `.await` (`Checked::awaited_of`); the lowering reads it and never solves it.
//!   No row (a future not wholly known): refused by name ([`Form::AwaitOperand`]). When it is
//!   the operand's own type, `into_future` is the identity: the library's blanket impl
//!   (core/src/future/into_future.rs:138: `impl<F: Future> IntoFuture for F { type Output =
//!   F::Output; type IntoFuture = F; fn into_future(self) -> Self::IntoFuture { self } }`) is the
//!   only impl that can name `Self` there (`type IntoFuture: Future<..>` makes `Self: Future`,
//!   and the blanket impl would then overlap any other), so `fut` is related to the operand
//!   position by position. Otherwise `into_future` is the selected impl's (the type check
//!   records it as the `.await`'s callee: blaze-typeck rules/await_impl.rs), related by that
//!   known head as any call is (await_call.rs); no callee row is refused by name.
//! - `r`, the `Poll<T>` the call returns, is given the await's output type `T`: `Poll<T>` has
//!   exactly the origin positions of `T` (core/src/task/poll.rs: `pub enum Poll<T> { Ready(T),
//!   Pending }`, no lifetime parameter of its own), and the Ready payload is all of `r` there is.
//!   `Future::poll` is `fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>`
//!   (core/src/future/future.rs:113): its result does not borrow the pin, so the pin is not related
//!   to `r`; `Self::Output` may hold the future's own lifetimes, so `fut` and `r` are made equal
//!   (a superset of what any impl can state). The task context `cx` is the resume argument, not a
//!   local of the body, and holds no loan of it: it is not an operand.
//! - On Ready the completed future is dropped as well as its storage ended: the desugaring in
//!   the Reference's step 2 makes `tmp` a value of the await, and "When an initialized variable or
//!   temporary goes out of scope, its destructor is run" ("Destructors").
//!
//! The cancel path returns no value; the move check reads the return place at every `Return`,
//! so `_0` is set to a constant (no loan, no move) before it.

use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_flow::{BorrowKind, Flow, Local, Operand, Place, Proj, StmtKind};
use blaze_typeck::{BodyView, CaptureKind, Checked, Expr, ExprId, Mutbl, PNode, PTy, PatId, PortableTys, Range};

use crate::identity::{Identity, Role};
use crate::{At, Facts, Form, Lower, MOST_POSITIONS, Refusal, Task, TyD, range_of};

/// An async block's body lowered as its own flow.
pub struct LoweredAsync<A: Allocator + Clone> {
    pub flow: Flow<A>,
    /// Per local of `flow`, its type in `ck.types` (`PTy`).
    pub local_ty: Vec<PTy, A>,
    /// Per suspension, in statement order, the `Expr::Await` it came from.
    pub point_of: Vec<ExprId, A>,
    /// Per local of `flow`: `Some(m)` when the local is a reference `&m T` to its `local_ty`
    /// that the record does not name (a capture's reference argument, an `.await`'s pin, a
    /// guard's fake borrow, an overloaded deref's result); `None` when `local_ty` is its type.
    pub local_wrap: Vec<Option<Mutbl>, A>,
    /// Per local of `flow`: what it stands for (identity.rs), the ONE table of identities the
    /// builder looks the locals it keeps across a suspension up in (ASYNC.md P8, async-P8-4).
    pub identity: Vec<crate::identity::Identity, A>,
    /// The block's item-relative range (what a refusal of [`interior`] names).
    pub range: Range,
}

/// The body of async block `block` (an `Expr::AsyncBlock` of `body`) as its own flow.
pub fn lower_async<A: Allocator + Clone, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    block: ExprId,
    facts: &F,
    alloc: A,
) -> Result<LoweredAsync<A>, Refusal> {
    let range = range_of(body, block);
    let inner = match body.exprs.get(block.ix()) {
        Some(Expr::AsyncBlock { body: inner, .. }) => *inner,
        _ => return Err(Refusal { form: Form::Malformed, range }),
    };
    refuse_async_closures(body, block)?;
    let mut l = Lower::new(body, ck, facts, alloc.clone())?;
    l.async_root = Some(block);
    l.run_async(block, inner)?;
    let flow = l.b.finish().map_err(|_| Refusal { form: Form::Builder, range })?;
    if l.local_ty.len() != flow.locals as usize {
        return Err(Refusal { form: Form::Malformed, range });
    }
    let mut local_ty = Vec::new_in(alloc.clone());
    let mut local_wrap = Vec::new_in(alloc);
    for t in l.local_ty.iter() {
        local_ty.push(t.ty);
        local_wrap.push(t.wrap);
    }
    let suspends = flow.stmts.iter().filter(|s| matches!(s.kind, StmtKind::Suspend { .. })).count();
    if suspends != l.points.len() {
        return Err(Refusal { form: Form::Malformed, range });
    }
    if l.identity.len() != flow.locals as usize {
        return Err(Refusal { form: Form::Malformed, range });
    }
    Ok(LoweredAsync { flow, local_ty, point_of: l.points, local_wrap, identity: l.identity, range })
}

/// A planted bug for the tests' controls.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InteriorPlant {
    None,
    /// Every local but the return place and the arguments is taken as saved: the moved-local
    /// rule (a local moved out before every suspension is not saved) is left out.
    EveryLocal,
}

/// The exact interior: the sorted, deduplicated types of the saved locals
/// (`blaze_flow::suspend::saved`) and of the captures by kind (by value `T`, by reference
/// `&T`/`&mut T`, made in `out`).
///
/// `out` holds `ck.types` as its prefix (a `fork_in` of it, or the table `ck.types` itself
/// extends): every `PTy` of the record is the same type of `out`. A type of the record that
/// `out` does not hold as the record does is refused by name ([`Form::InteriorTable`]). The
/// Reference's rule (typeck's rules/coroutine.rs doc lines 11-12): "Its auto traits (`Send`,
/// `Sync`) are its interior's: its captures and every value live across an `.await`."
pub fn interior<A: Allocator + Clone>(
    lowered: &LoweredAsync<A>,
    ck: &Checked<'_>,
    block: ExprId,
    out: &mut PortableTys<'_>,
    alloc: A,
) -> Result<PTy, Refusal> {
    interior_with(lowered, ck, block, out, false, alloc)
}

#[cfg(test)]
pub(crate) fn interior_planted<A: Allocator + Clone>(
    lowered: &LoweredAsync<A>,
    ck: &Checked<'_>,
    block: ExprId,
    out: &mut PortableTys<'_>,
    plant: InteriorPlant,
    alloc: A,
) -> Result<PTy, Refusal> {
    interior_with(lowered, ck, block, out, plant == InteriorPlant::EveryLocal, alloc)
}

/// One element of an exact interior ([`interior_parts`]): a type of the block's record
/// (`Checked::types`), behind a reference `&m` when `wrap` is `Some(m)` (a capture by reference,
/// or a local the record does not type as itself: [`LoweredAsync::local_wrap`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InteriorPart {
    pub ty: PTy,
    pub wrap: Option<Mutbl>,
}

/// The exact interior's elements, sorted by (record type, wrap) and deduplicated: the saved
/// locals (`blaze_flow::suspend::saved`, the one rule of what a state stores) and the captures
/// by kind. [`interior`] interns the same elements into a table extending the record's; a caller
/// with a table of its own (front2 opaque_auto.rs) copies each `ty` and makes the wraps there.
/// A local or capture whose type the record does not hold is refused by name
/// ([`Form::Malformed`]).
pub fn interior_parts<A: Allocator + Clone>(lowered: &LoweredAsync<A>, ck: &Checked<'_>, block: ExprId, alloc: A) -> Result<Vec<InteriorPart, A>, Refusal> {
    parts_with(lowered, ck, block, false, alloc)
}

fn parts_with<A: Allocator + Clone>(
    lowered: &LoweredAsync<A>,
    ck: &Checked<'_>,
    block: ExprId,
    every_local: bool,
    alloc: A,
) -> Result<Vec<InteriorPart, A>, Refusal> {
    let range = lowered.range;
    let refuse = |form: Form| Refusal { form, range };
    let flow = &lowered.flow;
    if lowered.local_ty.len() != flow.locals as usize || lowered.local_wrap.len() != flow.locals as usize {
        return Err(refuse(Form::Malformed));
    }
    let saved = blaze_flow::suspend::saved(flow, alloc.clone()).map_err(|_| refuse(Form::Bound))?;
    let record = ck.types.view();
    let mut parts: Vec<InteriorPart, A> = Vec::new_in(alloc.clone());
    // The saved locals; the arguments are the captures, taken below by their kind.
    for l in flow.args + 1..flow.locals {
        if !every_local && !saved.is_saved(Local(l)) {
            continue;
        }
        let ty = lowered.local_ty[l as usize];
        record.get(ty).ok_or(refuse(Form::Malformed))?;
        parts.push(InteriorPart { ty, wrap: lowered.local_wrap[l as usize] });
    }
    // The captures, in row order, by kind.
    let firsts = ck.captures.column::<0>();
    let pats = ck.captures.column::<1>();
    let kinds = ck.captures.column::<2>();
    for i in 0..firsts.len() {
        if firsts[i] != block {
            continue;
        }
        let ty = match ck.local_ty.get(pats[i].ix()) {
            Some(&t) => t,
            None => return Err(refuse(Form::Malformed)),
        };
        record.get(ty).ok_or(refuse(Form::Malformed))?;
        let wrap = match kinds[i] {
            CaptureKind::ByValue => None,
            CaptureKind::ByRef => Some(Mutbl::Not),
            CaptureKind::ByMutRef => Some(Mutbl::Mut),
        };
        parts.push(InteriorPart { ty, wrap });
    }
    parts.sort_unstable();
    let mut unique: Vec<InteriorPart, A> = Vec::new_in(alloc);
    for &p in parts.iter() {
        if unique.last() != Some(&p) {
            unique.push(p);
        }
    }
    Ok(unique)
}

fn interior_with<A: Allocator + Clone>(
    lowered: &LoweredAsync<A>,
    ck: &Checked<'_>,
    block: ExprId,
    out: &mut PortableTys<'_>,
    every_local: bool,
    alloc: A,
) -> Result<PTy, Refusal> {
    let range = lowered.range;
    let refuse = |form: Form| Refusal { form, range };
    let parts = parts_with(lowered, ck, block, every_local, alloc.clone())?;
    let record = ck.types.view();
    if out.view().len() < record.len() {
        return Err(refuse(Form::InteriorTable));
    }
    let mut tys: Vec<PTy, A> = Vec::new_in(alloc.clone());
    for p in parts.iter() {
        same_in(record.get(p.ty), out.view().get(p.ty)).ok_or(refuse(Form::InteriorTable))?;
        let t = match p.wrap {
            Some(m) => out.intern(PNode::Ref(m, p.ty)).map_err(|_| refuse(Form::Bound))?,
            None => p.ty,
        };
        tys.push(t);
    }
    tys.sort_unstable();
    let mut unique: Vec<PTy, A> = Vec::new_in(alloc);
    for &t in tys.iter() {
        if unique.last() != Some(&t) {
            unique.push(t);
        }
    }
    let list = out.intern_list(&unique).map_err(|_| refuse(Form::Bound))?;
    out.intern(PNode::Tuple(list)).map_err(|_| refuse(Form::Bound))
}

/// `Some(())` when the record's node and `out`'s at one index are the same node.
fn same_in(record: Option<&PNode>, out: Option<&PNode>) -> Option<()> {
    match (record, out) {
        (Some(a), Some(b)) if a == b => Some(()),
        _ => None,
    }
}

/// An `async` closure, or an `.await` inside a closure, within `block`'s range: refused by name
/// ([`Form::AsyncClosure`]). A closure's body is not lowered with the block's (the lowering
/// makes a closure expression its captures), so an `.await` there would be no suspension of
/// this block's; the Reference, "Await expressions": "await expressions are only allowed within
/// an async context". Found by item-relative range (a closure with an empty range holds none),
/// one pass over the closures and one over the awaits: no walk of the tree.
fn refuse_async_closures(body: &BodyView<'_>, block: ExprId) -> Result<(), Refusal> {
    let outer = range_of(body, block);
    let within = |inner: Range, around: Range| around.start <= inner.start && inner.end <= around.end;
    for (ci, c) in body.exprs.iter().enumerate() {
        let Expr::Closure { is_async, .. } = *c else { continue };
        let cr = range_of(body, ExprId(ci as u32));
        if cr.start >= cr.end || !within(cr, outer) {
            continue;
        }
        if is_async {
            return Err(Refusal { form: Form::AsyncClosure, range: cr });
        }
        for (ai, a) in body.exprs.iter().enumerate() {
            if !matches!(*a, Expr::Await(_)) {
                continue;
            }
            let ar = range_of(body, ExprId(ai as u32));
            if within(ar, cr) {
                return Err(Refusal { form: Form::AsyncClosure, range: ar });
            }
        }
    }
    Ok(())
}

impl<F: Facts, A: Allocator + Clone> Lower<'_, '_, F, A> {
    /// The walk of an async block's body `inner`: as `run`, with the block's captures as the
    /// arguments and its output as the return place.
    fn run_async(&mut self, block: ExprId, inner: ExprId) -> Result<(), Refusal> {
        let ck = self.ck;
        // The block's captures, in row order: argument `1 + k` is row `k`.
        let mut rows: Vec<(PatId, CaptureKind), A> = Vec::new_in(self.alloc.clone());
        {
            let firsts = ck.captures.column::<0>();
            let pats = ck.captures.column::<1>();
            let kinds = ck.captures.column::<2>();
            for i in 0..firsts.len() {
                if firsts[i] == block {
                    rows.push((pats[i], kinds[i]));
                }
            }
        }
        let nu = rows.len() as u32;
        self.b = blaze_flow::FlowBuilder::new_in(1 + nu, nu, self.alloc.clone());
        self.nargs = nu;
        let entry = self.b.block();
        self.ret_block = self.b.block();
        self.b.start(entry);
        self.scope_marks.push(0);
        for k in 0..rows.len() {
            let (p, kind) = rows[k];
            let t = match ck.local_ty.get(p.ix()) {
                Some(&t) => t,
                None => return self.refuse_pat(Form::Malformed, p),
            };
            let l = Local(1 + k as u32);
            match kind {
                CaptureKind::ByValue => {
                    self.new_local_with(TyD::of(t), true, Some(l), block)?;
                    if let Some(slot) = self.pat_local.get_mut(p.ix()) {
                        *slot = l.0;
                    } else {
                        return self.refuse_pat(Form::Malformed, p);
                    }
                }
                CaptureKind::ByRef | CaptureKind::ByMutRef => {
                    let m = if kind == CaptureKind::ByRef { Mutbl::Not } else { Mutbl::Mut };
                    let rt = TyD { wrap: Some(m), ty: t };
                    self.new_local_with(rt, true, Some(l), block)?;
                    let at = self.place_of(l);
                    let place = self.project(at, rt, Proj::Deref, TyD::of(t), false, block)?;
                    self.upvar_places.push((p, place, TyD::of(t)));
                }
            }
            self.vars.push(l);
        }
        // The return place: the block's output, its positions placeholders; every capture's
        // placeholder is known to outlive every one of them (the output may hold what a capture
        // by reference points to; a loan of the block's own locals or by-value captures is not
        // known to outlive it and is refused, as a returned reference to a local is).
        let out = match self.node(self.expr_ty(block)?, block)? {
            PNode::Future(fe, out, _) if fe == block => out,
            _ => return self.refuse(Form::TypeNode, block),
        };
        let upvar_origins = self.origs.len();
        self.new_local_with(TyD::of(out), true, Some(Local(0)), block)?;
        let all = self.origs.len();
        if (all - upvar_origins) as u64 * upvar_origins as u64 > u64::from(MOST_POSITIONS) {
            return self.refuse(Form::Bound, block);
        }
        for u in 0..upvar_origins {
            for o in upvar_origins..all {
                self.b.known(self.origs[u], self.origs[o]);
            }
        }
        self.tasks.push(Task::RootEnd(inner));
        self.tasks.push(Task::Eval(inner, false));
        let body = self.body;
        let fuel = 32 * (body.exprs.len() + body.pats.len() + body.stmts.len()) as u64 + 256;
        let mut steps = 0u64;
        while let Some(t) = self.tasks.pop() {
            steps += 1;
            if steps > fuel {
                return self.refuse(Form::Bound, block);
            }
            self.step(t)?;
        }
        Ok(())
    }

    /// The place and type an `Expr::Local` of captured pattern `p` reads in an async block's
    /// body when the block captures it by reference: `*arg`.
    pub(crate) fn upvar_place(&self, p: PatId) -> Option<(Place, TyD)> {
        self.upvar_places.iter().find(|u| u.0 == p).map(|u| (u.1, u.2))
    }

    /// `.await` at `e`, its operand's value on the stack (the module doc's lowering).
    pub(crate) fn await_done(&mut self, e: ExprId) -> Result<(), Refusal> {
        let v = self.pop(e)?;
        if self.async_root.is_none() {
            return self.refuse(Form::Await, e);
        }
        if self.callee_of(e).is_none() {
            return self.refuse(Form::Malformed, e);
        }
        if v.ty.wrap.is_some() {
            return self.refuse(Form::AwaitOperand, e);
        }
        // The future `tmp` holds across the suspension, `<T as IntoFuture>::IntoFuture`, as the
        // type checker decided it at this `.await` (module doc): read, never solved here.
        let Some(held) = self.ck.awaited_of(e) else {
            return self.refuse(Form::AwaitOperand, e);
        };
        let span = self.span(e);
        let ft = TyD::of(held);
        // Step 1: `fut = into_future(op)`.
        let fut = self.temp(ft, e)?;
        self.name_local(fut, Identity::Temp(e, Role::Awaitee));
        let fp = self.place_of(fut);
        let a = self.operand(v, e)?;
        self.b.call(&[a], fp, span);
        if held == v.ty.ty {
            // The blanket impl's identity (module doc): position by position.
            if let At::Place(p) = v.at {
                self.relate(p, fp, ft, e)?;
            }
        } else {
            // The selected impl's `into_future(self) -> Self::IntoFuture`, by its head.
            self.relate_into_future(v, fp, ft, e)?;
        }
        let (head, ready, pending, cancel) = (self.b.block(), self.b.block(), self.b.block(), self.b.block());
        self.b.goto(head, span);
        // Steps 3 and 4: pin, poll.
        self.b.start(head);
        let pin = self.new_local_with(TyD { wrap: Some(Mutbl::Mut), ty: held }, false, None, e)?;
        self.name_local(pin, Identity::Temp(e, Role::Pin));
        self.b.stmt(StmtKind::StorageLive(pin), span);
        self.borrow(fp, ft, BorrowKind::Mut, pin, e)?;
        let out = TyD::of(self.expr_ty(e)?);
        let r = self.new_local_with(out, false, None, e)?;
        self.name_local(r, Identity::Temp(e, Role::Result));
        self.b.stmt(StmtKind::StorageLive(r), span);
        let rp = self.place_of(r);
        let pp = self.place_of(pin);
        self.b.call(&[Operand::Move(pp)], rp, span);
        self.unify(&[fp, rp]);
        self.b.stmt(StmtKind::StorageDead(pin), span);
        self.b.switch(rp, &[ready, pending], span);
        // Step 5: Pending suspends; polled again, back to the head.
        self.b.start(pending);
        self.b.stmt(StmtKind::StorageDead(r), span);
        self.b.suspend(head, cancel);
        self.points.push(e);
        // Dropped while parked here: what a `return` here drops, `fut` first.
        self.b.start(cancel);
        self.exit_drops(0, 0, e)?;
        let ret = self.place_of(Local(0));
        self.b.assign(ret, Operand::Const, span);
        self.b.ret(span);
        // Step 6: Ready; the future is done with, `r` is the await's value.
        self.b.start(ready);
        if self.temps.last() != Some(&fut) {
            return self.refuse(Form::Malformed, e);
        }
        self.temps.pop();
        self.drop_local(fut, e)?;
        self.temps.push(r);
        self.push_place(rp, out);
        Ok(())
    }
}
