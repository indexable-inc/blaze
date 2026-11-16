//! Control flow and divergence: the body itself, `if`, `match`, `loop`, `break`, `return`.
//!
//! Follows rustc_hir_typeck/src/check.rs (`check_fn`: parameters, then the body as a return
//! site), _match.rs (`check_match`), expr.rs (`check_then_else`, `if_fallback_coercion`,
//! `check_expr_loop`, `check_expr_break`, `check_expr_return`, `check_return_or_body_tail`) and
//! diverges.rs; Relax maps them in typeck.rs (`check_body`, `check_if`, `check_match`,
//! `check_loop`, `check_break`, `check_return`).
//!
//! The orders are rustc's. `if`: the condition must be `bool`; the then-branch is checked with
//! the expectation adjusted for branches, the coercion target is computed after it, then the
//! else-branch; without `else`, a forced `()` (E0317). The `if` diverges when the condition
//! does, or both branches do. `match`: the scrutinee with no expectation, every arm's pattern,
//! then each arm's guard (`bool`) and body, coerced into one site aimed at the expected type
//! unless that is `()` (a fresh variable then). It diverges when the scrutinee or every arm
//! does; no arms is `!`. `loop`: its body is `()`; its type is what its breaks coerce to, `!`
//! when none; it diverges unless something breaks it. `break` and `return` check their value
//! with the site's expected type as a hint, push it (or a forced `()`) into the site, and are
//! `!`.

use blaze_own::Alloc;

use crate::diag::{At, DiagCode};
use crate::body::CoroutineKind;
use crate::error::{CheckError, Malformed, Stop};
use crate::ids::{ExprId, LangItem, List, PatId};
use crate::machine::{BreakCtx, Expectation, Flow, Frame, Lower, Machine, Source, Then};
use crate::ty::{Cause, Goal, Ty, TypeCtx};
use crate::world::{PClause, World};

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// `check_fn`'s start: the owner's where-clauses become assumptions, the return site is
    /// opened, and the body and then the parameter patterns are pushed (so the patterns run
    /// first). Returns the declared output.
    pub(crate) fn start_body(&mut self) -> Result<Ty, Stop> {
        let owner = self.body.owner;
        let preds = self.reads.predicates(owner)?;
        for clause in preds.clauses {
            let goal = match *clause {
                PClause::Trait { trait_, args } => {
                    let args = self.lower_body_list(preds.types, args)?;
                    Goal::Trait { trait_, args }
                }
                PClause::Projection { trait_, args, name, term } => {
                    let args = self.lower_body_list(preds.types, args)?;
                    let term = self.lower(preds.types, term, Lower::Assumed, Cause::Body)?;
                    Goal::Projection { trait_, args, name, term }
                }
                // The body's own `where const` condition is assumed, as its other clauses are.
                PClause::Const { cond, args } => Goal::ConstHolds { cond, args: self.lower_body_list(preds.types, args)? },
            };
            #[cfg(test)]
            if self.plant == crate::machine::Plant::IgnoreWhereClauses {
                continue;
            }
            self.ty.assume(goal)?;
        }
        let (inputs, output, rust_call) = match self.reads.fn_sig(owner)? {
            Some(sig) => {
                let mut inputs: blaze_own::Vec<Ty, Alloc<'a>> = blaze_own::Vec::with_capacity_in(sig.inputs.len(), self.alloc);
                for &t in sig.inputs {
                    inputs.push(self.lower_sig_ty(sig.types, t)?);
                }
                let output = self.lower_sig_ty(sig.types, sig.output)?;
                (inputs, output, sig.header.abi == crate::ids::Abi::RustCall)
            }
            None => (blaze_own::Vec::new_in(self.alloc), self.fresh()?, false),
        };
        // `extern "rust-call"` (the `unboxed_closures` feature; core's `Fn*::call*` are the
        // form: `extern "rust-call" fn call(&self, args: Args)` with `Args: Tuple`): the
        // arguments come as one tuple, so the last parameter's type must be `Tuple` (E0277 at
        // that parameter, the bound core's `Tuple` reports as "functions with the \"rust-call\"
        // ABI must take a single non-self tuple argument"); a type parameter passes by its bound.
        // OWED (typeck.md): a "rust-call" fn with no parameter has no place in the body to
        // report at; its finding belongs to the fn head's reader.
        if rust_call && let (Some(&p), Some(&last)) = (self.body.params.last(), inputs.last()) {
            if let Some(tuple) = self.need_lang(LangItem::Tuple, At::Pat(p))? {
                let args = self.mk_list(&[last])?;
                self.register_trait(tuple, args, Cause::Pat(p))?;
            }
        }
        // An `async fn`'s body returns the written type; its signature's output is the future.
        let output = match self.body.coroutine {
            Some(CoroutineKind::Async { output }) => self.lower_body_ty(output, At::Body)?,
            None => output,
        };
        let site = self.new_coerce_many(output)?;
        self.ret_sites.push(site);
        self.async_sites.push(self.body.coroutine.is_some());
        let root = self.body.root;
        self.push_expr(root, Expectation::HasType(output), Then::CoerceMany(site))?;
        let params = self.body.params;
        for (i, &p) in params.iter().enumerate().rev() {
            let t = match inputs.get(i) {
                Some(&t) => t,
                None => self.common.err,
            };
            self.push_pat(p, t, 0)?;
        }
        Ok(output)
    }

    /// `check_fn`'s end: the return site's type must be the declared output, `!` excepted.
    pub(crate) fn finish_body(&mut self, output: Ty) -> Result<(), Stop> {
        self.async_sites.pop();
        let Some(site) = self.ret_sites.pop() else { return Ok(()) };
        let actual = self.coerce_many_complete(site);
        if !self.is_never(actual) && !self.is_err(actual) {
            // The body's value against the declared output: reported at the body's block.
            let root = self.body.root;
            self.demand_eq(At::Expr(root), output, actual)?;
        }
        Ok(())
    }

    fn lower_body_list(&mut self, types: crate::portable::PTys<'_>, l: List) -> Result<crate::ty::TyList, Stop> {
        let src = types.list(l)?;
        let mut v: blaze_own::Vec<Ty, Alloc<'a>> = blaze_own::Vec::with_capacity_in(src.len(), self.alloc);
        for &t in src {
            v.push(self.lower(types, t, Lower::Assumed, Cause::Body)?);
        }
        self.mk_list(&v)
    }

    /// `check_then_else`. `f.a` is the site, `f.b` bit 0 the condition's divergence and bit 1
    /// the then-branch's.
    /// `let pat = init` as a condition (`check_expr_let`, through `check_decl`): the binding
    /// type a fresh variable, `init` coercible to it, `pat` checked against it as a `let`
    /// statement's is; the expression is `bool`.
    pub(crate) fn expr_let(&mut self, mut f: Frame, pat: PatId, init: ExprId) -> Result<Flow, Stop> {
        match f.state {
            0 => {
                f.t = self.fresh()?;
                self.resume(f, 1)?;
                self.push_coercible(init, f.t)?;
                Ok(Flow::Wait)
            }
            1 => {
                self.resume(f, 2)?;
                self.push_pat(pat, f.t, 0)?;
                Ok(Flow::Wait)
            }
            _ => Ok(Flow::Done(self.common.bool_)),
        }
    }

    pub(crate) fn expr_if(&mut self, mut f: Frame, e: ExprId, cond: ExprId, then: ExprId, else_: Option<ExprId>) -> Result<Flow, Stop> {
        match f.state {
            0 => {
                self.resume(f, 1)?;
                self.push_has_type(cond, self.common.bool_)?;
                Ok(Flow::Wait)
            }
            1 => {
                f.b = u32::from(self.diverges);
                self.diverges = false;
                f.expect = self.adjust_for_branches(f.expect);
                self.resume(f, 2)?;
                self.push_expr(then, f.expect, Then::Nothing)?;
                Ok(Flow::Wait)
            }
            2 => {
                let then_ty = self.ret;
                f.b |= u32::from(self.diverges) << 1;
                self.diverges = false;
                let target = self.coercion_target(f.expect)?;
                f.a = self.new_coerce_many(target)?;
                self.coerce_many_push(f.a, Source::Expr(then), then_ty)?;
                match else_ {
                    Some(els) => {
                        self.resume(f, 3)?;
                        self.push_expr(els, f.expect, Then::CoerceMany(f.a))?;
                        Ok(Flow::Wait)
                    }
                    None => {
                        self.if_fallback(e, f.a)?;
                        self.diverges = f.b & 1 != 0;
                        Ok(Flow::Done(self.coerce_many_complete(f.a)))
                    }
                }
            }
            _ => {
                let else_div = self.diverges;
                self.diverges = (f.b & 1 != 0) || ((f.b & 2 != 0) && else_div);
                Ok(Flow::Done(self.coerce_many_complete(f.a)))
            }
        }
    }

    /// `if_fallback_coercion`: an `if` without `else` is `()`; otherwise E0317.
    fn if_fallback(&mut self, e: ExprId, site: u32) -> Result<(), Stop> {
        let cm = self.coerce_many[site as usize];
        let merged = cm.final_ty.unwrap_or(cm.expected);
        if self.is_err(merged) {
            return Ok(());
        }
        let unit = self.common.unit;
        if self.unify(merged, unit)? {
            let entry = &mut self.coerce_many[site as usize];
            entry.final_ty = Some(unit);
            entry.pushed += 1;
        } else {
            self.diag(DiagCode::IfWithoutElse, At::Expr(e), Some(merged), Some(unit), None);
            self.coerce_many[site as usize].final_ty = Some(self.common.err);
        }
        Ok(())
    }

    /// `check_match`. `f.t` is the scrutinee's type, `f.a` the arm, `f.b` the site, `f.c` bit 0
    /// the scrutinee's divergence and bit 1 "every arm so far diverges".
    pub(crate) fn expr_match(&mut self, mut f: Frame, _e: ExprId, scrutinee: ExprId, arms: List) -> Result<Flow, Stop> {
        let arm_list = self.body.arm_list(arms)?;
        match f.state {
            0 => {
                self.resume(f, 1)?;
                self.push_expr(scrutinee, Expectation::None, Then::Nothing)?;
                Ok(Flow::Wait)
            }
            1 => {
                f.t = self.ret;
                if arm_list.is_empty() {
                    return Ok(Flow::Done(self.common.never));
                }
                f.c = u32::from(self.diverges) | 2;
                self.diverges = false;
                f.a = 0;
                self.match_pats(f, arm_list)
            }
            2 => self.match_pats(f, arm_list),
            3 => {
                // The arm's guard is done (or there was none): its body.
                let arm = arm_list[f.a as usize];
                self.diverges = false;
                self.resume(f, 4)?;
                self.push_expr(arm.body, f.expect, Then::CoerceMany(f.b))?;
                Ok(Flow::Wait)
            }
            _ => {
                if !self.diverges {
                    f.c &= !2;
                }
                f.a += 1;
                self.match_arm(f, arm_list)
            }
        }
    }

    /// The patterns of every arm, in order, then the first arm.
    fn match_pats(&mut self, mut f: Frame, arms: &[crate::body::Arm]) -> Result<Flow, Stop> {
        if let Some(arm) = arms.get(f.a as usize) {
            f.a += 1;
            self.resume(f, 2)?;
            self.push_pat(arm.pat, f.t, 0)?;
            return Ok(Flow::Wait);
        }
        f.expect = self.adjust_for_branches(f.expect);
        let first = match f.expect {
            Expectation::HasType(t) if t != self.common.unit => t,
            _ => self.fresh()?,
        };
        f.b = self.new_coerce_many(first)?;
        f.a = 0;
        self.match_arm(f, arms)
    }

    /// Arm `f.a`'s guard, or the end of the match.
    fn match_arm(&mut self, f: Frame, arms: &[crate::body::Arm]) -> Result<Flow, Stop> {
        let Some(arm) = arms.get(f.a as usize) else {
            self.diverges = f.c != 0;
            return Ok(Flow::Done(self.coerce_many_complete(f.b)));
        };
        self.resume(f, 3)?;
        if let Some(g) = arm.guard {
            self.diverges = false;
            self.push_has_type(g, self.common.bool_)?;
        }
        Ok(Flow::Wait)
    }

    /// `check_expr_loop` for `loop`. `f.a` is the site.
    pub(crate) fn expr_loop(&mut self, mut f: Frame, e: ExprId, body: ExprId) -> Result<Flow, Stop> {
        if f.state == 0 {
            let target = self.coercion_target(f.expect)?;
            f.a = self.new_coerce_many(target)?;
            self.breaks.push(BreakCtx { target: e, coerce: Some(f.a), may_break: false });
            self.resume(f, 1)?;
            self.push_has_type(body, self.common.unit)?;
            return Ok(Flow::Wait);
        }
        let ctx = self.breaks.pop();
        self.diverges = !ctx.is_some_and(|c| c.may_break);
        Ok(Flow::Done(self.coerce_many_complete(f.a)))
    }

    fn find_break(&self, target: ExprId) -> Result<usize, Stop> {
        match self.breaks.iter().rposition(|b| b.target == target) {
            Some(i) => Ok(i),
            None => Err(CheckError::Malformed(Malformed::BreakTarget(target)).into()),
        }
    }

    /// `check_expr_break`. `f.a` is the break context's index.
    pub(crate) fn expr_break(&mut self, mut f: Frame, e: ExprId, value: Option<ExprId>, target: ExprId) -> Result<Flow, Stop> {
        if f.state == 0 {
            let i = self.find_break(target)?;
            f.a = i as u32;
            let Some(site) = self.breaks[i].coerce else {
                return Err(CheckError::Malformed(Malformed::BreakTarget(target)).into());
            };
            match value {
                Some(v) => {
                    let hint = self.coerce_many[site as usize].expected;
                    self.resume(f, 1)?;
                    self.push_expr(v, Expectation::HasType(hint), Then::CoerceMany(site))?;
                    return Ok(Flow::Wait);
                }
                None => self.coerce_many_push(site, Source::Unit(e), self.common.unit)?,
            }
        }
        if !self.diverges {
            self.breaks[f.a as usize].may_break = true;
        }
        Ok(Flow::Done(self.common.never))
    }

    /// `check_expr_return`.
    pub(crate) fn expr_return(&mut self, f: Frame, e: ExprId, value: Option<ExprId>) -> Result<Flow, Stop> {
        if f.state == 0 {
            let Some(&site) = self.ret_sites.last() else {
                return Err(CheckError::Malformed(Malformed::BreakTarget(e)).into());
            };
            match value {
                Some(v) => {
                    let hint = self.coerce_many[site as usize].expected;
                    self.resume(f, 1)?;
                    self.push_expr(v, Expectation::HasType(hint), Then::CoerceMany(site))?;
                    return Ok(Flow::Wait);
                }
                None => self.coerce_many_push(site, Source::Unit(e), self.common.unit)?,
            }
        }
        Ok(Flow::Done(self.common.never))
    }
}
