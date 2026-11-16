//! Closures: the signature from the expectation, the body checked in place, captures after.
//!
//! Follows rustc_hir_typeck/src/closure.rs (`check_expr_closure`, `deduce_closure_signature`,
//! `deduce_sig_from_projection`, `sig_of_closure_with_expectation`,
//! `sig_of_closure_no_expectation`) and upvar.rs (`closure_analyze`, the capture kinds); Relax
//! maps them in typeck/closure.rs.
//!
//! The expected signature comes from an expected fn pointer, or, when the expected type is a
//! variable, from the pending `Fn*` obligation on it and its `FnOnce::Output` projection (the
//! reason a call checks its closure arguments last, after a selection). A parameter's type is
//! the written one (unified with the expected one when the arity matches), else the expected
//! one, else a fresh variable; likewise the return type. The body is checked at once, in the
//! same inference context, as its own return site; its divergence does not leak out. After the
//! selection that follows the walk (`closure_analyze` in rustc's end sequence), each closure's
//! captures are found: every local it uses that it does not bind, by mutable reference when
//! assigned or mutably borrowed, by shared reference otherwise.
//!
//! Omitted for now: `move` closures and by-value captures of moved non-`Copy` values (the body
//! lowering does not yet say `move`), precise place paths (rustc captures `a.b`, not `a`),
//! the closure kind (`Fn`/`FnMut`/`FnOnce`, which follows from the captures), and coroutines.

use blaze_own::{Alloc, Vec};

use crate::body::{Expr, Pat, Stmt, UnOp};
use crate::columns::CaptureKind;
use crate::diag::At;
use crate::error::{CheckError, Limit, Stop};
use crate::ids::{ExprId, LangItem, List, Mutbl, Name, PatId};
use crate::machine::{Expectation, Flow, Frame, Machine, PendingClosure, Then};
use crate::portable::PTy;
use crate::ty::{Ty, TyList, TyView, TypeCtx, Var, VarKind};
use crate::world::World;

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// The expected signature: inputs, and the output if known.
    fn deduce_sig(&mut self, ex: Expectation) -> Result<Option<(TyList, Option<Ty>)>, Stop> {
        let Some(t) = ex.to_option() else { return Ok(None) };
        let t = self.ty.shallow_resolve(t);
        match self.view(t) {
            TyView::FnPtr(l) => {
                let (inputs, output) = self.fn_ptr_sig(l)?;
                Ok(Some((inputs, Some(output))))
            }
            TyView::Infer(Var { kind: VarKind::Ty, .. }) => {
                let (Some(fn_), Some(fn_mut), Some(fn_once)) =
                    (self.lang(LangItem::Fn)?, self.lang(LangItem::FnMut)?, self.lang(LangItem::FnOnce)?)
                else {
                    return Ok(None);
                };
                Ok(self.ty.closure_hint(t, [fn_, fn_mut, fn_once], Name::of("Output")))
            }
            _ => Ok(None),
        }
    }

    /// `f.t` is the closure's type, `f.u` its output, `f.a` its return site, `f.b` the
    /// divergence before it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn expr_closure(
        &mut self,
        mut f: Frame,
        e: ExprId,
        params: List,
        param_tys: List,
        ret: Option<PTy>,
        body: ExprId,
    ) -> Result<Flow, Stop> {
        if f.state == 0 {
            let pats = self.body.pat_list(params)?;
            let written = self.body.opt_type_list(param_tys)?;
            let expected = self.deduce_sig(f.expect)?;
            let arity_ok = expected.is_some_and(|(l, _)| self.ty.list_len(l) as usize == pats.len());
            let mut inputs: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(pats.len() + 1, self.alloc);
            for i in 0..pats.len() {
                let exp = match expected {
                    Some((l, _)) if arity_ok => Some(self.ty.list_get(l, i as u32)),
                    _ => None,
                };
                let t = match (written.get(i).copied().flatten(), exp) {
                    (Some(w), Some(x)) => {
                        let w = self.lower_body_ty(w)?;
                        self.demand_eq(At::Pat(pats[i]), x, w)?;
                        w
                    }
                    (Some(w), None) => self.lower_body_ty(w)?,
                    (None, Some(x)) => x,
                    (None, None) => self.fresh()?,
                };
                inputs.push(t);
            }
            let output = match (ret, expected.and_then(|(_, o)| o)) {
                (Some(r), _) => self.lower_body_ty(r)?,
                (None, Some(o)) => o,
                (None, None) => self.fresh()?,
            };
            inputs.push(output);
            let sig_list = self.mk_list(&inputs)?;
            let sig = self.mk(TyView::FnPtr(sig_list))?;
            f.t = self.mk(TyView::Closure(e, sig))?;
            f.u = output;
            f.a = self.new_coerce_many(output)?;
            f.b = u32::from(self.diverges);
            self.ret_sites.push(f.a);
            self.closures.push(PendingClosure { expr: e });
            self.resume(f, 1)?;
            self.push_expr(body, Expectation::HasType(output), Then::CoerceMany(f.a))?;
            for (i, &p) in pats.iter().enumerate().rev() {
                self.push_pat(p, inputs[i], 0)?;
            }
            return Ok(Flow::Wait);
        }
        self.ret_sites.pop();
        let actual = self.coerce_many_complete(f.a);
        if !self.is_never(actual) && !self.is_err(actual) {
            self.demand_eq(At::Expr(body), f.u, actual)?;
        }
        self.diverges = f.b != 0;
        Ok(Flow::Done(f.t))
    }

    /// `closure_analyze`: the captures of every closure, innermost first as the walk met them.
    pub(crate) fn analyze_closures(&mut self) -> Result<(), Stop> {
        let mut bound: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(self.body.pats.len(), self.alloc);
        let mut exprs: Vec<(ExprId, u8), Alloc<'a>> = Vec::new_in(self.alloc);
        let mut pats: Vec<PatId, Alloc<'a>> = Vec::new_in(self.alloc);
        for ci in 0..self.closures.len() {
            let closure = self.closures[ci].expr;
            let Expr::Closure { params, body, .. } = self.body.expr(closure)? else { continue };
            bound.clear();
            bound.resize(self.body.pats.len(), false);
            pats.clear();
            pats.extend_from_slice(self.body.pat_list(params)?);
            self.mark_bound(&mut pats, &mut bound)?;
            exprs.clear();
            // The u8 is how the expression is used: 0 read, 1 assigned or mutably borrowed.
            exprs.push((body, 0));
            let mut steps = 0usize;
            let bound_steps = self.body.exprs.len() + self.body.stmts.len() + 1;
            while let Some((x, usage)) = exprs.pop() {
                steps += 1;
                if steps > bound_steps {
                    return Err(CheckError::Limit(Limit::Steps).into());
                }
                self.visit_for_captures(closure, x, usage, &mut exprs, &mut pats, &mut bound)?;
            }
        }
        // One row per (closure, local), the strongest use kept (rule 25: sorted).
        self.captures.sort_unstable_by_key(|&(c, p, k)| (c, p, core::cmp::Reverse(k as u8)));
        self.captures.dedup_by_key(|&mut (c, p, _)| (c, p));
        Ok(())
    }

    fn visit_for_captures(
        &mut self,
        closure: ExprId,
        x: ExprId,
        usage: u8,
        exprs: &mut Vec<(ExprId, u8), Alloc<'a>>,
        pats: &mut Vec<PatId, Alloc<'a>>,
        bound: &mut Vec<bool, Alloc<'a>>,
    ) -> Result<(), Stop> {
        let bv = self.body;
        let el = |l: List| bv.expr_list(l);
        match self.body.expr(x)? {
            Expr::Local(p) => {
                if !bound.get(p.ix()).copied().unwrap_or(true) {
                    let kind = if usage == 1 { CaptureKind::ByMutRef } else { CaptureKind::ByRef };
                    self.captures.push((closure, p, kind));
                }
            }
            Expr::Missing | Expr::Lit(_) | Expr::Path(_) | Expr::Continue { .. } => {}
            Expr::Paren(i) => exprs.push((i, usage)),
            Expr::Block { stmts, tail, .. } => {
                if let Some(t) = tail {
                    exprs.push((t, 0));
                }
                for &s in self.body.stmt_list(stmts)? {
                    match self.body.stmt(s)? {
                        Stmt::Let { pat, init, else_, .. } => {
                            pats.clear();
                            pats.push(pat);
                            self.mark_bound(pats, bound)?;
                            if let Some(i) = init {
                                exprs.push((i, 0));
                            }
                            if let Some(e) = else_ {
                                exprs.push((e, 0));
                            }
                        }
                        Stmt::Expr { expr, .. } => exprs.push((expr, 0)),
                        Stmt::Item => {}
                    }
                }
            }
            Expr::If { cond, then, else_ } => {
                exprs.push((cond, 0));
                exprs.push((then, 0));
                if let Some(e) = else_ {
                    exprs.push((e, 0));
                }
            }
            Expr::Match { scrutinee, arms } => {
                exprs.push((scrutinee, 0));
                for arm in self.body.arm_list(arms)? {
                    pats.clear();
                    pats.push(arm.pat);
                    self.mark_bound(pats, bound)?;
                    if let Some(g) = arm.guard {
                        exprs.push((g, 0));
                    }
                    exprs.push((arm.body, 0));
                }
            }
            Expr::Loop { body } => exprs.push((body, 0)),
            Expr::Break { value, .. } | Expr::Return { value } => {
                if let Some(v) = value {
                    exprs.push((v, 0));
                }
            }
            Expr::Call { callee, args } => {
                exprs.push((callee, 0));
                for &a in el(args)? {
                    exprs.push((a, 0));
                }
            }
            Expr::MethodCall { receiver, args, .. } => {
                exprs.push((receiver, self.receiver_usage(x)));
                for &a in el(args)? {
                    exprs.push((a, 0));
                }
            }
            Expr::Field { base, .. } | Expr::TupleField { base, .. } => exprs.push((base, usage)),
            Expr::Unary { op, operand } => exprs.push((operand, if op == UnOp::Deref { usage } else { 0 })),
            Expr::Binary { lhs, rhs, .. } => {
                exprs.push((lhs, 0));
                exprs.push((rhs, 0));
            }
            Expr::Assign { lhs, rhs } | Expr::AssignOp { lhs, rhs, .. } => {
                exprs.push((lhs, 1));
                exprs.push((rhs, 0));
            }
            Expr::Ref { mutbl, operand } => exprs.push((operand, u8::from(mutbl == Mutbl::Mut))),
            Expr::Cast { operand, .. } => exprs.push((operand, 0)),
            Expr::Tuple { elems } | Expr::Array { elems } => {
                for &a in el(elems)? {
                    exprs.push((a, 0));
                }
            }
            Expr::Repeat { elem, .. } => exprs.push((elem, 0)),
            Expr::Struct { fields, base, .. } => {
                for fi in self.body.field_init_list(fields)? {
                    exprs.push((fi.expr, 0));
                }
                if let Some(b) = base {
                    exprs.push((b, 0));
                }
            }
            Expr::Index { base, index } => {
                exprs.push((base, usage));
                exprs.push((index, 0));
            }
            Expr::Closure { params, body, .. } => {
                // A nested closure's own bindings are bound; what it captures from outside is
                // captured by this one too.
                pats.clear();
                pats.extend_from_slice(self.body.pat_list(params)?);
                self.mark_bound(pats, bound)?;
                exprs.push((body, 0));
            }
        }
        Ok(())
    }

    /// A method receiver borrowed `&mut` by its autoref is a mutable use.
    fn receiver_usage(&self, call: ExprId) -> u8 {
        let Expr::MethodCall { receiver, .. } = self.body.expr(call).unwrap_or(Expr::Missing) else { return 0 };
        let exprs = self.adjust.column::<0>();
        let kinds = self.adjust.column::<1>();
        let mutable = exprs.iter().zip(kinds).any(|(&e, k)| {
            e == receiver && matches!(k, crate::columns::AdjustKind::Borrow { mutbl: Mutbl::Mut, .. })
        });
        u8::from(mutable)
    }

    /// Marks every binding under the patterns in `stack` as bound (the stack is consumed).
    fn mark_bound(&self, stack: &mut Vec<PatId, Alloc<'a>>, bound: &mut Vec<bool, Alloc<'a>>) -> Result<(), Stop> {
        let mut steps = 0usize;
        while let Some(p) = stack.pop() {
            steps += 1;
            if steps > self.body.pats.len() + 1 {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            if let Some(b) = bound.get_mut(p.ix()) {
                *b = true;
            }
            match self.body.pat(p)? {
                Pat::Bind { sub: Some(s), .. } | Pat::Ref { inner: s, .. } => stack.push(s),
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Or { alts: elems } | Pat::Slice { elems } => {
                    stack.extend_from_slice(self.body.pat_list(elems)?);
                }
                Pat::Struct { fields, .. } => {
                    for fp in self.body.field_pat_list(fields)? {
                        stack.push(fp.pat);
                    }
                }
                Pat::Missing | Pat::Wild | Pat::Bind { sub: None, .. } | Pat::Lit(_) | Pat::NegLit(_) | Pat::Path(_) => {}
            }
        }
        Ok(())
    }
}
