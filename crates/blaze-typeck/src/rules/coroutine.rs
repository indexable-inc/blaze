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
//! The interior built now is a superset of the Reference's, so it can only refuse more, never
//! accept what the rule refuses. Liveness across an `.await` is not computed. The superset:
//! - the type of every expression and every pattern in the body, nested closures and blocks
//!   included, which covers every local, every temporary, and every value moved in;
//! - the future each `.await` holds while suspended (`<T as IntoFuture>::IntoFuture`);
//! - for a block that is not `move`, `&T` for every local it names but does not bind: a capture
//!   by reference. Capture kinds are not computed, so `T` is kept too: a capture is held as `T`,
//!   `&T` or `&mut T`, and `T` and `&T` together require at least what each of them does.
//!
//! Owed: the exact interior. It is the types of the locals and temporaries whose storage is live
//! at some `.await` (a local from its binding to its scope's end, less what was moved out; a
//! temporary to its enclosing statement's end, longer when extended by a `let`), plus the
//! captures by their computed kind.

use blaze_own::{Alloc, Vec};

use crate::body::{Capture, Expr, Pat, Stmt};
use crate::diag::At;
use crate::error::{CheckError, Limit, Stop};
use crate::ids::{ExprId, Mutbl, PatId};
use crate::machine::{Expectation, Flow, Frame, Machine, PendingFuture, Then};
use crate::ty::{Ty, TyView, TypeCtx};
use crate::world::World;

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// `async { body }`: `f.t` is the future, `f.u` its output, `f.a` the body's return site,
    /// `f.b` the divergence before it.
    pub(crate) fn expr_async_block(&mut self, mut f: Frame, e: ExprId, body: ExprId, capture: Capture) -> Result<Flow, Stop> {
        if f.state == 0 {
            // The root block of an `async fn` returns the fn's written type (the Reference,
            // "Async functions"; rules/opaque_def.rs `async_fn_output`).
            let output = match self.async_fn_output(e)? {
                Some(written) => written,
                None => self.fresh()?,
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
            self.push_expr(body, Expectation::HasType(output), Then::CoerceMany(f.a))?;
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

    /// Makes the future of the async block or async closure `e` with `body` and `output`, opens
    /// its async site (the caller pops it), and queues its interior for `analyze_futures`.
    pub(crate) fn open_future(&mut self, e: ExprId, body: ExprId, output: Ty, capture: Capture) -> Result<Ty, Stop> {
        let interior = self.fresh()?;
        let fut = self.mk(TyView::Future(e, output, interior))?;
        self.async_sites.push(true);
        self.futures.push(PendingFuture { expr: e, body, interior, capture });
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

    /// Fixes each future's interior (the module doc's superset), after the walk and selection,
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
            let PendingFuture { expr: _, body, interior, capture } = self.futures[fi];
            seen_expr.clear();
            seen_expr.resize(self.body.exprs.len(), false);
            bound.clear();
            bound.resize(self.body.pats.len(), false);
            locals.clear();
            held.clear();
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
            // The error type stands for a type already reported; it adds nothing to decide.
            // Resolved first, so a variable already bound to `u32` and `u32` are one entry.
            for t in held.iter_mut() {
                *t = self.ty.resolve(*t)?;
            }
            let err = self.common.err;
            held.retain(|&t| t != err);
            held.sort_unstable_by_key(|t| t.0);
            held.dedup();
            let list = self.mk_list(&held)?;
            let tuple = self.mk(TyView::Tuple(list))?;
            self.unify(interior, tuple)?;
        }
        Ok(())
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
                exprs.extend(tail);
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
            Expr::Struct { fields, base, .. } => {
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
            Pat::Struct { fields, .. } => {
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
