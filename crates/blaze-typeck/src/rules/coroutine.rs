//! Async blocks, and the interior of every future a body makes (typeck.md, "Async: decided by
//! lane 2"). The rules are the Reference's ("Async blocks", "Await expressions", "Closure
//! expressions"); rustc's `check_expr_closure` for a coroutine and its upvar and interior
//! analyses only say what the rules are.
//!
//! - `async { body }` has an anonymous type, one per block: `TyView::Future(block, output,
//!   interior)`, implementing `Future<Output = output>` where `output` is the body's (its tail's,
//!   or what its `return`s and `?`s give). The body is its own return site and async site: a
//!   `return` or `?` in it leaves the future, not the enclosing fn, and `.await` is allowed in it.
//!   The block itself does not diverge when made (its body runs when polled).
//! - Its auto traits (`Send`, `Sync`) are its interior's: its captures and every value live
//!   across an `.await`. The interior is a variable until `analyze_futures` fixes it after the
//!   walk, so an auto-trait goal on the future waits for it. It is never `Unpin` (the solver's
//!   builtin: an async coroutine is immovable).
//!
//! An async block with no suspension stores only its captures. A direct forwarding
//! await can also prove its complete state from the checked operand. For other
//! suspensions flow's saved-local analysis decides what is live (blaze-flow-lower
//! `interior_parts`). Until then the type check uses a provisional superset:
//! - the type of every expression and every pattern in the body, nested closures and blocks
//!   included, which covers every local, every temporary, and every value moved in;
//! - the future each `.await` holds while suspended (`<T as IntoFuture>::IntoFuture`);
//! - for a block that is not `move`, `&T` for every local it names but does not bind: a capture
//!   by reference. When the capture mode is unknown, `T` is kept too: a capture is held as `T`,
//!   `&T` or `&mut T`, and `T` and `&T` together require at least what each of them does.
//!
//! A failed auto-trait obligation on that superset can be decided only on the saved locals
//! and captures, including the pending `IntoFuture` at the suspend point.

use blaze_own::{Alloc, Vec};
use core::ops::Range;

use crate::body::{Capture, Expr, Pat, Stmt};
use crate::columns::CaptureKind;
use crate::diag::At;
use crate::error::{CheckError, Limit, Malformed, Stop};
use crate::ids::{ExprId, ItemKey, LangItem, Mutbl, Name, PatId};
use crate::machine::{Expectation, Flow, Frame, Machine, PendingFuture, Then};
use crate::ty::{Goal, Ty, TyView, TypeCtx};
use crate::world::World;

impl crate::columns::Checked<'_> {
    /// Returns the output and stored types named by the local coroutine at `e`.
    /// An async closure's expression is the closure. Its call signature returns
    /// the future, while a block's expression already has the future type.
    pub fn coroutine_at(
        &self, body: &crate::body::BodyView<'_>, e: ExprId,
    ) -> Result<Option<(u32, crate::portable::PTy, crate::portable::PTy)>, CheckError> {
        use crate::portable::PNode;
        let Some(ordinal) = crate::coroutine_id::coroutine_ordinal(body, e) else { return Ok(None) };
        let refused = CheckError::Malformed(Malformed::CoroutineFactsMissing {
            reader: body.owner, at: body.expr_range(e), owner: body.owner, ordinal,
        });
        let types = self.types.view();
        let ty = if ordinal < crate::coroutine_id::CLOSURE_BASE {
            *self.expr_ty.get(e.0 as usize).ok_or(refused)?
        } else {
            let row = (ordinal - crate::coroutine_id::CLOSURE_BASE) as usize;
            let sig = *self.closures.column::<2>().get(row).ok_or(refused)?;
            let PNode::FnPtr(io, _) = types.node(sig)? else { return Err(refused) };
            *types.list(io)?.last().ok_or(refused)?
        };
        match types.node(ty)? {
            PNode::Future(block, output, interior) if block == e => Ok(Some((ordinal, output, interior))),
            _ => Err(refused),
        }
    }
}

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// `async { body }`: `f.t` is the future, `f.u` its output, `f.a` the body's return site,
    /// `f.b` the divergence before it.
    pub(crate) fn expr_async_block(&mut self, mut f: Frame, e: ExprId, body: ExprId, capture: Capture) -> Result<Flow, Stop> {
        if f.state == 0 {
            // The root block of an `async fn` returns the fn's written type (the Reference,
            // "Async functions"; rules/opaque_def.rs `async_fn_output`).
            let written = self.async_fn_output(e)?;
            let (output, body_expect) = match written {
                Some(t) => (t, Expectation::HasType(t)),
                None => {
                    let hint = self.expected_future_output(f.expect)?;
                    // Keep the body's output inference independent. The destination's
                    // associated-type equality is checked by the enclosing coercion.
                    // This is a non-reporting hint even when the output is sized, so a mismatch
                    // is diagnosed by the future's `Output` projection at the coercion site.
                    let expect = hint.map_or(Expectation::None, Expectation::RvalueLikeUnsized);
                    (self.fresh()?, expect)
                }
            };
            #[cfg(test)]
            let fut_out = match self.plant {
                crate::machine::Plant::AsyncOutputFromFirst => self.fresh()?,
                _ => output,
            };
            #[cfg(not(test))]
            let fut_out = output;
            f.t = self.open_future(e, body, fut_out, capture)?;
            f.u = output;
            f.a = self.new_coerce_many(output)?;
            f.b = u32::from(self.diverges);
            self.ret_sites.push(f.a);
            self.resume(f, 1)?;
            self.push_expr(body, body_expect, Then::CoerceMany(f.a))?;
            return Ok(Flow::Wait);
        }
        self.ret_sites.pop();
        self.async_sites.pop();
        let actual = self.coerce_many_complete(f.a);
        #[cfg(test)]
        if self.plant == crate::machine::Plant::AsyncOutputFromFirst
            && let TyView::Future(_, planted, _) = self.view(f.t)
            && let Some(first) = self.first_stmt_ty(body)?
        {
            self.unify(planted, first)?;
        }
        if !self.is_never(actual) && !self.is_err(actual) {
            self.demand_eq(At::Expr(body), f.u, actual)?;
        }
        self.diverges = f.b != 0;
        Ok(Flow::Done(f.t))
    }

    /// A pointer constructor can supply its unsized pointee as the argument's hint.
    /// For an async block, the object's equality on Future::Output types its return
    /// site. The block itself keeps its concrete future type for the pointer coercion.
    fn expected_future_output(&mut self, expect: Expectation) -> Result<Option<Ty>, Stop> {
        let Some(target) = expect.to_option() else { return Ok(None) };
        let TyView::Dynamic(bounds) = self.view(target) else { return Ok(None) };
        let Some(future) = self.lang(LangItem::Future)? else { return Ok(None) };
        for i in 0..self.ty.list_len(bounds) {
            let predicate = self.ty.list_get(bounds, i);
            if let TyView::ExProj { trait_, name, args_term } = self.view(predicate)
                && trait_ == future && name == Name::of("Output")
            {
                if self.ty.list_len(args_term) != 1 {
                    return Err(CheckError::Malformed(crate::error::Malformed::ObjectPreds { node: predicate.0 }).into());
                }
                return Ok(Some(self.ty.list_get(args_term, 0)));
            }
        }
        Ok(None)
    }

    /// Makes the future of the async block or async closure `e` with `body` and `output`, opens
    /// its async site (the caller pops it), and queues its interior for `analyze_futures`.
    pub(crate) fn open_future(&mut self, e: ExprId, body: ExprId, output: Ty, capture: Capture) -> Result<Ty, Stop> {
        let interior = self.fresh()?;
        let fut = self.mk(TyView::Future(e, output, interior))?;
        self.async_sites.push(true);
        self.futures.push(PendingFuture {
            expr: e, body, interior, capture, initial: None, witness: crate::CoroutineWitness::NeedsCapturePlaces,
        });
        Ok(fut)
    }

    /// The planted bug's output: the type of the block's first statement's expression.
    #[cfg(test)]
    fn first_stmt_ty(&mut self, body: ExprId) -> Result<Option<Ty>, Stop> {
        let Expr::Block { stmts, .. } = self.body.expr(body)? else { return Ok(None) };
        let Some(&s) = self.body.stmt_list(stmts)?.first() else { return Ok(None) };
        Ok(match self.body.stmt(s)? {
            Stmt::Expr { expr, .. } => Some(self.expr_ty[expr.ix()]),
            Stmt::Let { init: Some(i), .. } => Some(self.expr_ty[i.ix()]),
            _ => None,
        })
    }

    /// Fixes each future's interior after the walk and selection,
    /// so the auto-trait goals on the futures can be decided by `select_all`.
    pub(crate) fn analyze_futures(&mut self) -> Result<(), Stop> {
        if self.futures.is_empty() {
            return Ok(());
        }
        let mut seen_expr: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(self.body.exprs.len(), self.alloc);
        let mut bound: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(self.body.pats.len(), self.alloc);
        let mut exprs: Vec<ExprId, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut pats: Vec<PatId, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut locals: Vec<(PatId, Ty), Alloc<'a>> = Vec::new_in(self.alloc);
        let mut held: Vec<Ty, Alloc<'a>> = Vec::new_in(self.alloc);
        let bound_steps = self.body.exprs.len() + self.body.pats.len() + self.body.stmts.len() + 1;
        for fi in 0..self.futures.len() {
            let future = self.futures[fi];
            let PendingFuture { expr, body, interior, capture, .. } = future;
            if future.witness == crate::CoroutineWitness::Complete {
                continue;
            }
            held.clear();
            if let Some(initial) = future.initial {
                let TyView::Tuple(fields) = self.view(initial) else {
                    return Err(CheckError::Malformed(Malformed::Expr(expr)).into());
                };
                for i in 0..self.ty.list_len(fields) {
                    held.push(self.ty.list_get(fields, i));
                }
            }
            if future.witness == crate::CoroutineWitness::NeedsSavedLocals
                && let Some(pending) = self.forwarded_await(expr, body)?
            {
                held.push(pending);
                let state = self.witness_tuple(&mut held)?;
                self.demand_eq(At::Expr(expr), interior, state)?;
                self.futures[fi].witness = crate::CoroutineWitness::Complete;
                continue;
            }
            seen_expr.clear();
            seen_expr.resize(self.body.exprs.len(), false);
            bound.clear();
            bound.resize(self.body.pats.len(), false);
            locals.clear();
            exprs.clear();
            exprs.push(body);
            let mut steps = 0usize;
            while let Some(x) = exprs.pop() {
                steps += 1;
                if steps > bound_steps {
                    return Err(CheckError::Limit(Limit::Steps).into());
                }
                if let Some(s) = seen_expr.get_mut(x.ix()) {
                    *s = true;
                }
                held.push(self.expr_ty[x.ix()]);
                if let Expr::Local(p) = self.body.expr(x)? {
                    locals.push((p, self.expr_ty[x.ix()]));
                }
                pats.clear();
                self.expr_children(x, &mut exprs, &mut pats)?;
                // Every pattern under this expression: its type, and each binding's.
                while let Some(p) = pats.pop() {
                    steps += 1;
                    if steps > bound_steps {
                        return Err(CheckError::Limit(Limit::Steps).into());
                    }
                    if let Some(b) = bound.get_mut(p.ix()) {
                        *b = true;
                    }
                    held.push(self.pat_ty[p.ix()]);
                    held.push(self.local_ty[p.ix()]);
                    self.pat_children(p, &mut pats)?;
                }
            }
            // A capture by reference holds `&T` (module doc: `T` is held too).
            if capture == Capture::Ref {
                for &(p, t) in locals.iter() {
                    if !bound.get(p.ix()).copied().unwrap_or(true) {
                        let r = self.mk(TyView::Ref(Mutbl::Not, t))?;
                        held.push(r);
                    }
                }
            }
            for &(aw, t) in self.awaited.iter() {
                if seen_expr.get(aw.ix()).copied().unwrap_or(false) {
                    held.push(t);
                }
            }
            let witness = self.witness_tuple(&mut held)?;
            self.demand_eq(At::Expr(expr), interior, witness)?;
        }
        Ok(())
    }

    /// A block that forwards directly to a single await has two states holding
    /// user values: its captures before polling and the IntoFuture result while
    /// pending. Only accept operands whose evaluation cannot introduce another
    /// temporary borrowed across that await. All other bodies need flow's
    /// saved-local analysis.
    fn forwarded_await(&mut self, owner: ExprId, body: ExprId) -> Result<Option<Ty>, Stop> {
        if matches!(self.body.expr(owner)?, Expr::Closure { .. }) {
            // Destructuring a call parameter can leave an independently owned
            // field to drop after the await.
            return Ok(None);
        }
        let mut at = body;
        for _ in 0..=self.body.exprs.len() {
            match self.body.expr(at)? {
                Expr::Paren(inner) => at = inner,
                Expr::Block { stmts, tail: Some(tail), .. } => {
                    if stmts.len != 0 {
                        // The lowering of an async fn starts with `let x = x`
                        // for each simple parameter. Both bindings have the
                        // type already present in the initial, by-value state.
                        if owner != self.body.root || self.body.coroutine.is_none() || at != body {
                            return Ok(None);
                        }
                        for &s in self.body.stmt_list(stmts)? {
                            let Stmt::Let { pat, init: Some(init), ty: None, else_: None } = self.body.stmt(s)? else { return Ok(None) };
                            let Expr::Local(arg) = self.body.expr(init)? else { return Ok(None) };
                            if !self.body.params.contains(&arg)
                                || !matches!(self.body.pat(pat)?, Pat::Bind { ann, sub: None, .. } if ann.by_ref.is_none())
                                || self.adjust.column::<0>().contains(&init)
                            {
                                return Ok(None);
                            }
                        }
                    }
                    at = tail;
                }
                Expr::Await(operand) => {
                    let args = match self.body.expr(operand)? {
                        Expr::Local(_) | Expr::AsyncBlock { .. } => &[][..],
                        Expr::Call { callee, args } => {
                            let callee_ty = self.ty.shallow_resolve(self.expr_ty[callee.ix()]);
                            if !matches!(self.body.expr(callee)?, Expr::Path(_)) || !matches!(self.view(callee_ty), TyView::FnDef(..)) {
                                return Ok(None);
                            }
                            self.body.expr_list(args)?
                        }
                        Expr::MethodCall { receiver, args, .. } if matches!(self.body.expr(receiver)?, Expr::Local(_)) => {
                            // Borrowing a local receiver needs no backing temporary.
                            // An arbitrary receiver adjustment can create one.
                            let mut adjustments = self.adjust.column::<0>().iter().zip(self.adjust.column::<1>())
                                .filter_map(|(&e, &kind)| (e == receiver).then_some(kind));
                            if !matches!(adjustments.next(), None | Some(crate::columns::AdjustKind::Borrow { raw: false, .. }))
                                || adjustments.next().is_some()
                            {
                                return Ok(None);
                            }
                            self.body.expr_list(args)?
                        }
                        _ => return Ok(None),
                    };
                    for &arg in args {
                        if self.adjust.column::<0>().contains(&arg) {
                            return Ok(None);
                        }
                        match self.body.expr(arg)? {
                            Expr::Local(_) | Expr::Lit(_) | Expr::Path(_) => {}
                            Expr::Ref { operand, .. }
                                if matches!(self.body.expr(operand)?, Expr::Local(_)) && !self.adjust.column::<0>().contains(&operand) => {}
                            _ => return Ok(None),
                        }
                    }
                    return Ok(self.awaited.iter().find_map(|&(e, ty)| (e == at).then_some(ty)));
                }
                _ => return Ok(None),
            }
        }
        Err(CheckError::Limit(Limit::Steps).into())
    }

    pub(super) fn capture_rows(&self, owner: ExprId) -> Range<usize> {
        let start = self.captures.partition_point(|&(c, _, _)| c < owner);
        let end = self.captures.partition_point(|&(c, _, _)| c <= owner);
        start..end
    }

    pub(super) fn stored_capture(&mut self, row: usize) -> Result<Ty, Stop> {
        let (_, p, kind) = self.captures[row];
        let t = self.local_ty[p.ix()];
        match kind {
            CaptureKind::ByValue => Ok(t),
            CaptureKind::ByRef => self.mk(TyView::Ref(Mutbl::Not, t)),
            CaptureKind::ByMutRef => self.mk(TyView::Ref(Mutbl::Mut, t)),
        }
    }

    /// Whether this body can suspend, if the rows prove its captured places.
    /// The rows name bindings. A direct use of a whole binding in the recorded mode
    /// proves the capture; a field use alone cannot establish that the whole value is stored.
    pub(super) fn whole_capture_witness(
        &mut self,
        body: ExprId,
        capture: Capture,
        rows: &Range<usize>,
        inherited: &[(PatId, CaptureKind)],
        exprs: &mut Vec<ExprId, Alloc<'a>>,
        pats: &mut Vec<PatId, Alloc<'a>>,
        whole: &mut Vec<bool, Alloc<'a>>,
    ) -> Result<Option<bool>, Stop> {
        exprs.clear();
        pats.clear();
        if !rows.is_empty() {
            whole.clear();
            whole.resize(self.body.pats.len(), false);
            for &(p, mode) in inherited {
                if let Ok(i) = self.captures[rows.clone()].binary_search_by_key(&p, |&(_, pat, _)| pat)
                    && self.captures[rows.start + i].2 == mode
                {
                    whole[p.ix()] = true;
                }
            }
            self.mark_whole_capture(capture, rows, body, None, whole)?;
        }
        let needs_places = self.captures[rows.clone()].iter().any(|&(_, p, _)| !whole[p.ix()]);
        exprs.push(body);
        let mut steps = 0;
        let mut suspends = false;
        while let Some(e) = exprs.pop() {
            steps += 1;
            if steps > self.body.exprs.len() {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            let node = self.body.expr(e)?;
            match node {
                Expr::Await(_) => suspends = true,
                Expr::Missing => return Ok(None),
                // Creating a nested coroutine or closure does not poll or call it.
                Expr::AsyncBlock { .. } | Expr::Closure { .. } | Expr::ConstBlock { .. } => continue,
                _ => {}
            }
            if needs_places {
                if let Expr::Block { stmts, .. } = node {
                    for &s in self.body.stmt_list(stmts)? {
                        match self.body.stmt(s)? {
                            Stmt::Expr { expr, .. } => self.mark_whole_capture(capture, rows, expr, None, whole)?,
                            // An async fn moves each argument into its root block with a
                            // plain binding. A wildcard or a destructure need not move it.
                            Stmt::Let { init: Some(init), pat, .. }
                                if matches!(self.body.pat(pat)?, Pat::Bind { ann, sub: None, .. } if ann.by_ref.is_none()) =>
                            {
                                self.mark_whole_capture(capture, rows, init, None, whole)?;
                            }
                            _ => {}
                        }
                    }
                }
                let values = match node {
                    Expr::Tuple { elems } | Expr::Array { elems } => self.body.expr_list(elems)?,
                    Expr::Call { callee, args } => {
                        if let Some(usage) = self.callee_capture_use(e, callee) {
                            let borrow = match usage {
                                0 => Some(Mutbl::Not),
                                1 => Some(Mutbl::Mut),
                                _ => None,
                            };
                            self.mark_whole_capture(capture, rows, callee, borrow, whole)?;
                        }
                        self.body.expr_list(args)?
                    }
                    Expr::Binary { op, lhs, rhs } => {
                        use crate::body::BinOp;
                        let borrow = matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
                            .then_some(Mutbl::Not);
                        self.mark_whole_capture(capture, rows, lhs, borrow, whole)?;
                        self.mark_whole_capture(capture, rows, rhs, borrow, whole)?;
                        &[]
                    }
                    Expr::Assign { lhs, rhs }
                        if matches!(self.body.expr(lhs)?, Expr::Underscore) && self.discarded_local(rhs)? => &[],
                    Expr::Assign { lhs, rhs } | Expr::AssignOp { lhs, rhs, .. } => {
                        self.mark_whole_capture(capture, rows, lhs, Some(Mutbl::Mut), whole)?;
                        self.mark_whole_capture(capture, rows, rhs, None, whole)?;
                        &[]
                    }
                    Expr::MethodCall { receiver, args, .. } => {
                        self.mark_whole_capture(capture, rows, receiver, None, whole)?;
                        self.body.expr_list(args)?
                    }
                    Expr::Ref { mutbl, operand } => {
                        self.mark_whole_capture(capture, rows, operand, Some(mutbl), whole)?;
                        &[]
                    }
                    Expr::FormatArgs { args, .. } => {
                        for &arg in self.body.expr_list(args)? {
                            // The implicit formatting borrow is an adjustment on the argument.
                            self.mark_whole_capture(capture, rows, arg, None, whole)?;
                        }
                        &[]
                    }
                    Expr::Struct { fields, .. } => {
                        for field in self.body.field_init_list(fields)? {
                            self.mark_whole_capture(capture, rows, field.expr, None, whole)?;
                        }
                        &[]
                    }
                    Expr::If { ref cond, .. } => core::slice::from_ref(cond),
                    Expr::Unary { op, ref operand } if op != crate::body::UnOp::Deref => core::slice::from_ref(operand),
                    Expr::Block { tail: Some(ref value), .. } | Expr::Return { value: Some(ref value) } | Expr::Await(ref value) => core::slice::from_ref(value),
                    _ => &[],
                };
                for &value in values {
                    self.mark_whole_capture(capture, rows, value, None, whole)?;
                }
            }
            pats.clear();
            self.expr_children(e, exprs, pats)?;
        }
        for i in rows.clone() {
            let p = self.captures[i].1;
            if !whole[p.ix()] {
                return Ok(None);
            }
        }
        Ok(Some(suspends))
    }

    fn mark_whole_capture(
        &mut self,
        capture: Capture,
        rows: &Range<usize>,
        value: ExprId,
        mut borrow: Option<Mutbl>,
        whole: &mut [bool],
    ) -> Result<(), Stop> {
        let mut place = value;
        if capture == Capture::Move {
            // The use may be written through parentheses or explicit dereferences.
            // A field reached without dereferencing its base can be disjoint.
            for _ in 0..=self.body.exprs.len() {
                place = match self.body.expr(place)? {
                    Expr::Paren(inner) | Expr::Unary { op: crate::body::UnOp::Deref, operand: inner } => inner,
                    Expr::Field { base, .. } | Expr::TupleField { base, .. }
                        if self.adjust.column::<0>().iter().zip(self.adjust.column::<1>()).any(|(&at, kind)| {
                            at == base && matches!(kind, crate::columns::AdjustKind::Deref { .. })
                        }) => base,
                    _ => break,
                };
            }
        }
        let Expr::Local(p) = self.body.expr(place)? else { return Ok(()) };
        let Ok(i) = self.captures[rows.clone()].binary_search_by_key(&p, |&(_, pat, _)| pat) else { return Ok(()) };
        // A move capture ends before the first dereference. The receiver of
        // `move || shared.lock()` is the owned smart pointer, even though the
        // method expects a reference to its target. The same holds for a
        // by-value argument that undergoes a coercion.
        if capture == Capture::Move {
            if self.captures[rows.start + i].2 == CaptureKind::ByValue {
                whole[p.ix()] = true;
            }
            return Ok(());
        }
        // Calling Deref on a smart pointer borrows the pointer itself. Box
        // has a special place projection in the Reference and needs its own
        // precision proof. Builtin reborrows through references do too.
        let mut steps = self.adjust.column::<0>().iter().zip(self.adjust.column::<1>())
            .filter_map(|(&at, &kind)| (at == value).then_some(kind));
        if matches!(steps.next(), Some(crate::columns::AdjustKind::Deref { overloaded: Some(Mutbl::Not) }))
            && matches!(steps.last(), Some(crate::columns::AdjustKind::Borrow { raw: false, mutbl: Mutbl::Not, .. }))
            && borrow.is_none()
        {
            let ty = self.ty.shallow_resolve(self.local_ty[p.ix()]);
            if let TyView::Adt(adt, _) = self.view(ty)
                && self.lang(LangItem::OwnedBox)? != Some(adt)
                && self.captures[rows.start + i].2 == CaptureKind::ByRef
            {
                whole[p.ix()] = true;
            }
            return Ok(());
        }
        for (&at, &adjustment) in self.adjust.column::<0>().iter().zip(self.adjust.column::<1>()) {
            if at != value {
                continue;
            }
            // Borrowing the unprojected receiver still uses the entire binding. An
            // autoderef, unsizing, or other coercion needs the adjusted place instead.
            match adjustment {
                crate::columns::AdjustKind::Borrow { mutbl, raw: false, .. }
                    if borrow.is_none() || borrow == Some(mutbl) => borrow = Some(mutbl),
                _ => return Ok(()),
            }
        }
        let mode = match (capture, borrow) {
            (Capture::Move, _) => CaptureKind::ByValue,
            (Capture::Ref, Some(Mutbl::Mut)) => CaptureKind::ByMutRef,
            (Capture::Ref, Some(Mutbl::Not)) => CaptureKind::ByRef,
            (Capture::Ref, None) if self.block_moved_out(value)? => CaptureKind::ByValue,
            (Capture::Ref, None) => CaptureKind::ByRef,
        };
        if self.captures[rows.start + i].2 == mode {
            *whole.get_mut(p.ix()).ok_or(CheckError::Malformed(Malformed::Pat(p)))? = true;
        }
        Ok(())
    }

    /// Sending or sharing the unpolled future already requires its initial state to
    /// satisfy the trait. Losing a local at a later suspension cannot change this.
    pub(super) fn initial_state_fails(&mut self, block: ExprId, trait_: ItemKey) -> Result<bool, Stop> {
        let Some(initial) = self.futures.iter().find(|f| f.expr == block).and_then(|f| f.initial) else { return Ok(false) };
        if self.witness_source(initial)? != super::async_fn::Witness::Fixed {
            return Ok(false);
        }
        let args = self.mk_list(&[initial])?;
        Ok(!self.ty.may_hold(Goal::Trait { trait_, args }, &mut self.reads)?)
    }

    pub(super) fn witness_tuple(&mut self, held: &mut Vec<Ty, Alloc<'a>>) -> Result<Ty, Stop> {
        for t in held.iter_mut() {
            *t = self.ty.resolve(*t)?;
        }
        let err = self.common.err;
        held.retain(|&t| t != err);
        held.sort_unstable_by_key(|t| t.0);
        held.dedup();
        let list = self.mk_list(held)?;
        self.mk(TyView::Tuple(list))
    }

    /// The direct child expressions of `x` into `exprs`, and the patterns it binds into `pats`.
    pub(crate) fn expr_children(&self, x: ExprId, exprs: &mut Vec<ExprId, Alloc<'a>>, pats: &mut Vec<PatId, Alloc<'a>>) -> Result<(), Stop> {
        let b = self.body;
        match b.expr(x)? {
            Expr::Missing
            | Expr::Lit(_)
            | Expr::Local(_)
            | Expr::Path(_)
            | Expr::Continue { .. }
            | Expr::Underscore
            | Expr::OffsetOf { .. } => {}
            Expr::Paren(i) | Expr::Loop { body: i } | Expr::Await(i) => exprs.push(i),
            Expr::Block { stmts, tail, .. } | Expr::ConstBlock { stmts, tail, .. } => {
                for &s in b.stmt_list(stmts)? {
                    match b.stmt(s)? {
                        Stmt::Let { pat, init, else_, .. } => {
                            pats.push(pat);
                            exprs.extend(init);
                            exprs.extend(else_);
                        }
                        Stmt::Expr { expr, .. } => exprs.push(expr),
                        Stmt::Item => {}
                    }
                }
                exprs.extend(tail);
            }
            Expr::If { cond, then, else_ } => {
                exprs.push(cond);
                exprs.push(then);
                exprs.extend(else_);
            }
            Expr::Let { pat, init } => {
                pats.push(pat);
                exprs.push(init);
            }
            Expr::Match { scrutinee, arms } => {
                exprs.push(scrutinee);
                for arm in b.arm_list(arms)? {
                    pats.push(arm.pat);
                    exprs.extend(arm.guard);
                    exprs.push(arm.body);
                }
            }
            Expr::Break { value, .. } | Expr::Return { value } => exprs.extend(value),
            Expr::Call { callee, args } => {
                exprs.push(callee);
                exprs.extend_from_slice(b.expr_list(args)?);
            }
            Expr::MethodCall { receiver, args, .. } => {
                exprs.push(receiver);
                exprs.extend_from_slice(b.expr_list(args)?);
            }
            Expr::Field { base, .. } | Expr::TupleField { base, .. } => exprs.push(base),
            Expr::Unary { operand, .. }
            | Expr::Ref { operand, .. }
            | Expr::RawRef { operand, .. }
            | Expr::Cast { operand, .. } => exprs.push(operand),
            Expr::Binary { lhs, rhs, .. } | Expr::Assign { lhs, rhs } | Expr::AssignOp { lhs, rhs, .. } => {
                exprs.push(lhs);
                exprs.push(rhs);
            }
            Expr::Tuple { elems } | Expr::Array { elems } => exprs.extend_from_slice(b.expr_list(elems)?),
            Expr::FormatArgs { args, .. } => exprs.extend_from_slice(b.expr_list(args)?),
            Expr::Asm { operands, .. } => {
                for o in b.asm_operand_list(operands)? {
                    exprs.push(o.expr);
                    if let Some(x) = o.out_expr {
                        exprs.push(x);
                    }
                }
            }
            Expr::Repeat { elem, .. } => exprs.push(elem),
            Expr::Struct { fields, base, .. } | Expr::Record { fields, base } => {
                for fi in b.field_init_list(fields)? {
                    exprs.push(fi.expr);
                }
                exprs.extend(base.expr());
            }
            Expr::Index { base, index } => {
                exprs.push(base);
                exprs.push(index);
            }
            Expr::Closure { params, body, .. } => {
                pats.extend_from_slice(b.pat_list(params)?);
                exprs.push(body);
            }
            Expr::AsyncBlock { body, .. } => exprs.push(body),
        }
        Ok(())
    }

    /// The direct sub-patterns of `p` into `pats`.
    fn pat_children(&self, p: PatId, pats: &mut Vec<PatId, Alloc<'a>>) -> Result<(), Stop> {
        let b = self.body;
        match b.pat(p)? {
            Pat::Missing | Pat::Wild | Pat::Bind { sub: None, .. } | Pat::Lit(_) | Pat::NegLit(_) | Pat::Path(_) => {}
            Pat::Bind { sub: Some(s), .. } | Pat::Ref { inner: s, .. } => pats.push(s),
            Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Or { alts: elems } | Pat::Slice { elems, .. } => {
                pats.extend_from_slice(b.pat_list(elems)?);
            }
            Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                for fp in b.field_pat_list(fields)? {
                    pats.push(fp.pat);
                }
            }
            Pat::Range { lo, hi, .. } => {
                pats.extend(lo);
                pats.extend(hi);
            }
        }
        Ok(())
    }
}
