//! Blocks, statements and `let`.
//!
//! Follows rustc_hir_typeck/src/fn_ctxt/checks.rs (`check_block_with_expected`, `check_stmt`,
//! `check_decl_local`, `check_decl`, `check_decl_initializer`) and gather_locals.rs (a `let`'s
//! type is its annotation or a fresh variable); Relax maps them in typeck.rs (`check_block`,
//! `check_stmt`, `check_let`).
//!
//! A block is a coercion site and a break target: its tail (checked with the block's own
//! expectation) and every `break` to it are coerced into one `CoerceMany` aimed at the
//! expectation's type or a fresh variable. A block without a tail that does not diverge pushes a
//! forced `()` (E0308 when `()` is not the expected type); one that diverges is `!`. A statement
//! saves the divergence before it and folds it back after (`check_stmt`). A `let` checks its
//! initializer coercible to the declared type, then the pattern against it, then the `else`
//! block, which must be `!`; the divergence of the `else` is dropped.
//!
//! Omitted for now: a `let` whose pattern has a top-level `ref` binding checks the initializer
//! by equality rather than coercion in rustc (`check_decl_initializer`'s `ref_bindings`); here
//! it is always a coercion.

use crate::body::Stmt;
use crate::diag::{About, At, DiagCode};
use crate::error::Stop;
use crate::ids::{ExprId, List, StmtId};
use crate::machine::{BreakCtx, Expectation, Flow, Frame, Machine, Source, Then};
use crate::ty::TypeCtx;
use crate::world::World;

impl<W: World, C: TypeCtx> Machine<'_, '_, '_, '_, W, C> {
    /// `f.a` is the coercion site, `f.b` the next statement.
    pub(crate) fn expr_block(&mut self, mut f: Frame, e: ExprId, stmts: List, tail: Option<ExprId>) -> Result<Flow, Stop> {
        let list = self.body.stmt_list(stmts)?;
        if f.state == 0 {
            let target = self.coercion_target(f.expect)?;
            f.a = self.new_coerce_many(target)?;
            f.b = 0;
            self.breaks.push(BreakCtx { target: e, coerce: Some(f.a), may_break: false });
            f.state = 1;
        }
        if f.state == 1 {
            if let Some(&s) = list.get(f.b as usize) {
                f.b += 1;
                self.resume(f, 1)?;
                self.push_stmt(s)?;
                return Ok(Flow::Wait);
            }
            if let Some(tail) = tail {
                self.resume(f, 2)?;
                self.push_expr(tail, f.expect, Then::CoerceMany(f.a))?;
                return Ok(Flow::Wait);
            }
            if !self.diverges {
                self.coerce_many_push(f.a, Source::Unit(e), self.common.unit)?;
            }
        }
        let ctx = self.breaks.pop();
        if ctx.is_some_and(|c| c.may_break) {
            self.diverges = false;
        }
        Ok(Flow::Done(self.coerce_many_complete(f.a)))
    }

    /// `check_stmt`. `f.saved` is the divergence before the statement; for a `let`, `f.t` is
    /// the declared type and `f.b` the divergence before its `else`.
    pub(crate) fn step_stmt(&mut self, mut f: Frame, s: StmtId) -> Result<(), Stop> {
        if f.state == 0 {
            f.saved = self.diverges;
            self.diverges = false;
        }
        let done = match self.body.stmt(s)? {
            Stmt::Item => true,
            Stmt::Expr { expr, semi } => {
                if f.state == 0 {
                    self.resume(f, 1)?;
                    if semi {
                        self.push_expr(expr, Expectation::None, Then::Nothing)?;
                    } else {
                        self.push_has_type(expr, self.common.unit)?;
                    }
                    false
                } else {
                    true
                }
            }
            Stmt::Let { pat, ty, init, else_ } => self.step_let(&mut f, pat, ty, init, else_)?,
        };
        if done {
            self.diverges |= f.saved;
        }
        Ok(())
    }

    /// One step of a `let`; `true` when it is done.
    fn step_let(
        &mut self,
        f: &mut Frame,
        pat: crate::ids::PatId,
        ty: Option<crate::portable::PTy>,
        init: Option<ExprId>,
        else_: Option<ExprId>,
    ) -> Result<bool, Stop> {
        if f.state == 0 {
            f.t = match ty {
                Some(t) => self.lower_body_ty(t, At::Pat(pat))?,
                None => self.fresh()?,
            };
            if let Some(init) = init {
                self.resume(*f, 1)?;
                self.push_coercible(init, f.t)?;
                return Ok(false);
            }
            f.state = 1;
        }
        if f.state == 1 {
            self.resume(*f, 2)?;
            self.push_pat(pat, f.t, 0)?;
            return Ok(false);
        }
        if f.state == 2 {
            if let Some(els) = else_ {
                f.b = u32::from(self.diverges);
                self.resume(*f, 3)?;
                self.push_expr(els, Expectation::None, Then::Nothing)?;
                return Ok(false);
            }
            return Ok(true);
        }
        // After the `else` block: it must be `!`.
        let else_ty = self.ret;
        let never = self.common.never;
        if !self.is_never(else_ty) && !self.is_err(else_ty) && !self.unify(never, else_ty)? {
            let at = match else_ {
                Some(e) => At::Expr(e),
                None => At::Body,
            };
            self.diag(DiagCode::Mismatch, at, About::types(never, else_ty));
        }
        self.diverges = f.b != 0;
        Ok(true)
    }
}
