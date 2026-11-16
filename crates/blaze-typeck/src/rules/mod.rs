//! The rules: one file per family of nodes, each naming at its top the rustc file it follows
//! (rustc's order of inference is the specification) and the Relax file that maps it
//! (`/Volumes/Projects/ref-relax/relax/libs/relax/types/src`). This file dispatches a frame to
//! its rule and holds what several rules share: argument checking and the `rvalue_hint`.

use blaze_own::Vec;
use core::alloc::Allocator;

use crate::body::Expr;
use crate::error::Stop;
use crate::ids::ExprId;
use crate::machine::{Expectation, Flow, Frame, Machine, Then};
use crate::ty::{Ty, TyList, TyView, TypeCtx};
use crate::world::World;

pub(crate) mod adt;
pub(crate) mod block;
pub(crate) mod call;
pub(crate) mod cast;
pub(crate) mod closure;
pub(crate) mod coerce;
pub(crate) mod control;
pub(crate) mod fallback;
pub(crate) mod field;
pub(crate) mod lit_path;
pub(crate) mod method;
pub(crate) mod op;
pub(crate) mod pat;
pub(crate) mod writeback;

impl<W: World, C: TypeCtx, A: Allocator + Clone> Machine<'_, '_, '_, W, C, A> {
    /// One step of `e`'s rule (`check_expr_kind`).
    pub(crate) fn expr_rule(&mut self, f: Frame, e: ExprId, expr: Expr) -> Result<Flow, Stop> {
        match expr {
            Expr::Missing => Ok(Flow::Done(self.common.err)),
            Expr::Lit(lit) => self.expr_lit(f, lit),
            Expr::Local(p) => Ok(Flow::Done(self.local_ty[p.ix()])),
            Expr::Path(path) => self.expr_path(e, path),
            Expr::Paren(inner) => self.expr_paren(f, inner),
            Expr::Block { stmts, tail, scope: _ } => self.expr_block(f, e, stmts, tail),
            Expr::If { cond, then, else_ } => self.expr_if(f, e, cond, then, else_),
            Expr::Match { scrutinee, arms } => self.expr_match(f, e, scrutinee, arms),
            Expr::Loop { body } => self.expr_loop(f, e, body),
            Expr::Break { value, target } => self.expr_break(f, e, value, target),
            Expr::Continue { target: _ } => Ok(Flow::Done(self.common.never)),
            Expr::Return { value } => self.expr_return(f, e, value),
            Expr::Call { callee, args } => self.expr_call(f, e, callee, args),
            Expr::MethodCall { receiver, name, generic_args, args, scope } => {
                self.expr_method_call(f, e, receiver, name, generic_args, args, scope)
            }
            Expr::Field { base, name } => self.expr_field(f, e, base, name),
            Expr::TupleField { base, index } => self.expr_tuple_field(f, e, base, index),
            Expr::Unary { op, operand } => self.expr_unary(f, e, op, operand),
            Expr::Binary { op, lhs, rhs } => self.expr_binary(f, e, op, lhs, rhs),
            Expr::Assign { lhs, rhs } => self.expr_assign(f, e, lhs, rhs),
            Expr::AssignOp { op, lhs, rhs } => self.expr_assign_op(f, e, op, lhs, rhs),
            Expr::Ref { mutbl, operand } => self.expr_ref(f, e, mutbl, operand),
            Expr::Cast { operand, ty } => self.expr_cast(f, e, operand, ty),
            Expr::Tuple { elems } => self.expr_tuple(f, e, elems),
            Expr::Array { elems } => self.expr_array(f, e, elems),
            Expr::Repeat { elem, len } => self.expr_repeat(f, e, elem, len),
            Expr::Struct { path, fields, base } => self.expr_struct(f, e, path, fields, base),
            Expr::Index { base, index } => self.expr_index(f, e, base, index),
            Expr::Closure { params, param_tys, ret, body } => {
                self.expr_closure(f, e, params, param_tys, ret, body)
            }
        }
    }

    /// `Expectation::rvalue_hint`: an unsized expected type is only a hint.
    pub(crate) fn rvalue_hint(&mut self, t: Ty) -> Expectation {
        match self.view(t) {
            TyView::Str | TyView::Slice(_) => Expectation::RvalueLikeUnsized(t),
            _ => Expectation::HasType(t),
        }
    }

    /// `Expectation::adjust_for_branches`: an expected type that is still a variable is no
    /// expectation for the branches.
    pub(crate) fn adjust_for_branches(&mut self, ex: Expectation) -> Expectation {
        match ex {
            Expectation::HasType(t) => {
                let t = self.ty.shallow_resolve(t);
                if self.is_ty_var(t) { Expectation::None } else { Expectation::HasType(t) }
            }
            other => other,
        }
    }

    /// `Expectation::coercion_target_type`: the expected type, or a fresh variable.
    pub(crate) fn coercion_target(&mut self, ex: Expectation) -> Result<Ty, Stop> {
        match ex.only_has_type() {
            Some(t) => Ok(t),
            None => self.fresh(),
        }
    }

    /// The next argument of `check_argument_types`, in rustc's two passes: every argument that
    /// is not a closure, then (after a selection) the closures, each checked with the formal
    /// input as its hint and coerced to it with two-phase borrows allowed.
    ///
    /// `f.l` holds the formal inputs, `f.a` the next index, `f.b` the pass. Returns `true` when
    /// it pushed an argument (the frame resumes at `state`), `false` when all are checked.
    pub(crate) fn next_arg(&mut self, mut f: Frame, args: &[ExprId], state: u8) -> Result<bool, Stop> {
        let formals = self.ty.list_len(f.l);
        // Two passes over the arguments at most.
        for _ in 0..2 {
            while (f.a as usize) < args.len() {
                let i = f.a as usize;
                let arg = args[i];
                f.a += 1;
                let is_closure = matches!(self.body.expr(arg)?, Expr::Closure { .. });
                if is_closure != (f.b == 1) {
                    continue;
                }
                let formal = if (i as u32) < formals { self.ty.list_get(f.l, i as u32) } else { self.common.err };
                let expect = self.rvalue_hint(formal);
                self.resume(f, state)?;
                self.push_expr(arg, expect, Then::Coerce { to: formal, two_phase: true })?;
                return Ok(true);
            }
            if f.b == 1 {
                return Ok(false);
            }
            f.b = 1;
            f.a = 0;
            self.select()?;
        }
        Ok(false)
    }

    /// Starts argument checking for a callee with `inputs`: reports a wrong count (E0061),
    /// then checks the arguments. `Flow::Done(output)` when there are none to check.
    pub(crate) fn start_args(
        &mut self,
        mut f: Frame,
        e: ExprId,
        args: &[ExprId],
        inputs: TyList,
        output: Ty,
        state: u8,
        count_checked: bool,
    ) -> Result<Flow, Stop> {
        if count_checked && self.ty.list_len(inputs) as usize != args.len() {
            self.diag(crate::diag::DiagCode::ArgCount, crate::diag::At::Expr(e), None, None, None);
        }
        f.l = inputs;
        f.u = output;
        f.a = 0;
        f.b = 0;
        if self.next_arg(f, args, state)? { Ok(Flow::Wait) } else { Ok(Flow::Done(output)) }
    }

    /// The types of a list's elements, all the error type: the inputs of a callee that failed.
    pub(crate) fn err_list(&mut self, n: usize) -> Result<TyList, Stop> {
        let mut v: Vec<Ty, A> = Vec::with_capacity_in(n, self.alloc.clone());
        v.resize(n, self.common.err);
        self.mk_list(&v)
    }
}
