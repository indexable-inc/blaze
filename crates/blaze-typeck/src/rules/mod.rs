//! The rules: one file per family of nodes, each naming at its top the rustc file it follows
//! (rustc's order of inference is the specification) and the Relax file that maps it
//! (`/Volumes/Projects/ref-relax/relax/libs/relax/types/src`). This file dispatches a frame to
//! its rule and holds what several rules share: argument checking and the `rvalue_hint`.

use blaze_own::{Alloc, Vec};

use crate::body::Expr;
use crate::error::Stop;
use crate::ids::ExprId;
use crate::machine::{Expectation, Flow, Frame, Machine, Then};
use crate::ty::{Ty, TyList, TyView, TypeCtx};
use crate::world::World;

pub(crate) mod adt;
pub(crate) mod alias_path;
pub(crate) mod args_inline;
pub(crate) mod assoc_args;
pub(crate) mod async_fmt;
pub(crate) mod await_impl;
pub(crate) mod async_fn;
pub(crate) mod block;
pub(crate) mod builtin_ops;
pub(crate) mod c_variadic;
#[cfg(test)]
mod c_variadic_tests;
pub(crate) mod call;
pub(crate) mod cast;
pub(crate) mod closure;
pub(crate) mod coerce;
pub(crate) mod coerce_lub;
pub(crate) mod coerce_nested;
pub(crate) mod coerce_interior;
pub(crate) mod control;
pub(crate) mod coroutine;
pub(crate) mod expect_inputs;
pub(crate) mod fallback;
pub(crate) mod field;
pub(crate) mod legacy_const;
pub(crate) mod lit_path;
pub(crate) mod method;
pub(crate) mod obligation_site;
pub(crate) mod mut_places;
pub(crate) mod op;
pub(crate) mod opaque_def;
pub(crate) mod pat;
pub(crate) mod promote;
pub(crate) mod qpath_defaults;
pub(crate) mod reify;
pub(crate) mod rpitit_use;
pub(crate) mod self_path;
pub(crate) mod target_feature;
pub(crate) mod unsafety;
pub mod suggest;
pub(crate) mod writeback;

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// One step of `e`'s rule (`check_expr_kind`).
    pub(crate) fn expr_rule(&mut self, f: Frame, e: ExprId, expr: Expr) -> Result<Flow, Stop> {
        match expr {
            Expr::Missing => Ok(Flow::Done(self.common.err)),
            Expr::Lit(lit) => self.expr_lit(f, e, lit),
            Expr::Local(p) => Ok(Flow::Done(self.local_ty[p.ix()])),
            Expr::Path(path) => self.expr_path(f.expect, e, path),
            Expr::Paren(inner) => self.expr_paren(f, inner),
            Expr::Block { stmts, tail, scope: _ } => self.expr_block(f, e, stmts, tail),
            Expr::If { cond, then, else_ } => self.expr_if(f, e, cond, then, else_),
            Expr::Let { pat, init } => self.expr_let(f, pat, init),
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
            Expr::Repeat { elem, len, count } => self.expr_repeat(f, e, elem, len, count),
            Expr::Struct { path, fields, base } => self.expr_struct(f, e, path, fields, base),
            Expr::Record { fields, base } => self.expr_record(f, e, fields, base),
            Expr::Index { base, index } => self.expr_index(f, e, base, index),
            Expr::Closure { params, param_tys, ret, body, is_async, .. } => {
                self.expr_closure(f, e, params, param_tys, ret, body, is_async)
            }
            Expr::FormatArgs { args, uses } => self.expr_format_args(f, e, args, uses),
            Expr::Await(operand) => self.expr_await(f, e, operand),
            Expr::AsyncBlock { body, capture } => self.expr_async_block(f, e, body, capture),
            Expr::Underscore => self.expr_underscore(e),
            Expr::OffsetOf { ty, fields } => self.expr_offset_of(e, ty, fields),
            Expr::RawRef { mutbl, operand } => self.expr_raw_ref(f, e, mutbl, operand),
            Expr::ConstBlock { stmts, tail, scope: _ } => self.expr_block(f, e, stmts, tail),
            Expr::Asm { operands, options, arch, .. } => self.expr_asm(f, operands, arch, options),
        }
    }

    /// `Expectation::rvalue_hint`: an unsized expected type is only a hint. rustc
    /// (expectation.rs `rvalue_hint`) matches the type's STRUCT TAIL (`struct_tail_raw`):
    /// `ty::Slice | ty::Str | ty::Dynamic` give `ExpectRvalueLikeUnsized`. So a struct whose last
    /// field is unsized is a hint too. turso core/btree_dump.rs:51 is `Ok(Arc::new(RwLock::new(
    /// cursor)))` returning `Arc<RwLock<dyn InternalVirtualTableCursor>>`. `Arc::new`'s input,
    /// from the expected output, is `lock_api::RwLock<RawRwLock, dyn ..>`, whose tail (`data:
    /// UnsafeCell<T>`, `value: T`) is the object. Taken as `HasType`, it bound `T` to it: E0308
    /// at `RwLock::new(cursor)` and E0277 "`dyn ..: Sized`". As the hint, `T` is
    /// `RwLock<RawRwLock, BtreeDumpCursor>`, and the `Arc` coerces after (`CoerceUnsized`).
    pub(crate) fn rvalue_hint(&mut self, t: Ty) -> Result<Expectation, Stop> {
        let tail = self.struct_tail(t)?;
        Ok(match self.view(tail) {
            TyView::Str | TyView::Slice(_) | TyView::Dynamic(_) => Expectation::RvalueLikeUnsized(t),
            _ => Expectation::HasType(t),
        })
    }

    /// The type's struct tail (rustc's `struct_tail_raw`): a struct's last field at its
    /// arguments, a tuple's last element, again until neither. An enum, a union, a struct with
    /// no fields and every other type is its own tail. Bounded: a tail deeper than
    /// [`Self::TAIL_DEPTH`] structs is refused by name (rustc's recursion limit, E0275's class).
    pub(crate) fn struct_tail(&mut self, t: Ty) -> Result<Ty, Stop> {
        let mut t = t;
        for _ in 0..Self::TAIL_DEPTH {
            let next = match self.view(t) {
                TyView::Tuple(l) | TyView::Record(_, l) => match self.ty.list_len(l) {
                    0 => return Ok(t),
                    n => self.ty.list_get(l, n - 1),
                },
                TyView::Adt(k, args) => {
                    let Some(adt) = self.reads.adt(k)? else { return Ok(t) };
                    if adt.kind != crate::world::AdtKind::Struct {
                        return Ok(t);
                    }
                    let Some(v) = adt.variants.first() else { return Ok(t) };
                    let Some(last) = adt.fields.get(v.fields.range()).and_then(|f| f.last()).copied() else { return Ok(t) };
                    self.lower(adt.types, last.ty, crate::machine::Lower::Args(args), crate::ty::Cause::Body)?
                }
                _ => return Ok(t),
            };
            t = self.ty.shallow_resolve(next);
        }
        Err(crate::error::CheckError::Unported("a struct tail deeper than 64 structs (rustc's recursion limit)").into())
    }

    /// How many structs [`Self::struct_tail`] goes through.
    const TAIL_DEPTH: u32 = 64;

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
                // An unsized input the call's expected type gave is the argument's hint; it is
                // still coerced to the formal (rustc `check_argument_types`: `coerced_ty =
                // expectation.only_has_type().unwrap_or(formal_input_ty)`).
                let expect = match self.take_unsized_arg_hint(arg) {
                    Some(t) => Expectation::RvalueLikeUnsized(t),
                    None => self.rvalue_hint(formal)?,
                };
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
            let count = crate::diag::About::Count { expected: self.ty.list_len(inputs), got: args.len() as u32, found: None };
            self.diag(crate::diag::DiagCode::ArgCount, crate::diag::At::Expr(e), count);
        }
        f.l = inputs;
        f.u = output;
        f.a = 0;
        f.b = 0;
        if self.next_arg(f, args, state)? { Ok(Flow::Wait) } else { Ok(Flow::Done(output)) }
    }

    /// The types of a list's elements, all the error type: the inputs of a callee that failed.
    pub(crate) fn err_list(&mut self, n: usize) -> Result<TyList, Stop> {
        let mut v: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(n, self.alloc);
        v.resize(n, self.common.err);
        self.mk_list(&v)
    }
}
