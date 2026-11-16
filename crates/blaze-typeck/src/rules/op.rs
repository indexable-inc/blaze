//! Operators, assignment, borrows and derefs.
//!
//! Follows rustc_hir_typeck/src/op.rs (`check_binop`, `check_binop_assign`,
//! `check_overloaded_binop`, `is_builtin_binop`, `enforce_builtin_binop_types`,
//! `check_user_unop`) and expr.rs (`check_expr_unop`, `check_expr_addr_of`,
//! `check_expr_assign`, `is_syntactic_place_expr`); Relax maps them in typeck/ops.rs and
//! typeck.rs (`check_assign`, `check_ref`).
//!
//! A binary operator, in rustc's order: `&&`/`||` check both sides coercible to `bool`, the right
//! side's divergence not counted. Every other operator checks the left side with no expectation,
//! makes a fresh variable for the right side, registers `lhs: Op<rhs>` and its `Output` (when it
//! may hold; when it cannot and the operation is not builtin, E0369), then checks the right side
//! coercible to the variable. When both sides are builtin-compatible scalars (integer and float
//! variables included), the builtin rule decides the type (`enforce_builtin_binop_types`) and
//! the trait's output is made equal to it, so `1 + 2` is `{integer}` without selection.
//!
//! Omitted for now: the autoref adjustments of overloaded comparison operands, and the first
//! cut does not coerce the left side to a fresh variable (rustc does, which only adds a
//! reborrow to `&mut` left sides).

use blaze_own::Alloc;

use crate::body::{BinOp, Expr, UnOp};
use crate::diag::{At, DiagCode};
use crate::error::Stop;
use crate::ids::{ExprId, LangItem, Mutbl, Name};
use crate::machine::{Expectation, Flow, Frame, Machine, Then};
use crate::ty::{Cause, Goal, Ty, TyView, TypeCtx, Var, VarKind};
use crate::world::{Def, World};

/// rustc's `BinOpCategory`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Category {
    Shortcircuit,
    Shift,
    Math,
    Bitwise,
    Comparison,
}

fn category(op: BinOp) -> Category {
    match op {
        BinOp::And | BinOp::Or => Category::Shortcircuit,
        BinOp::Shl | BinOp::Shr => Category::Shift,
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => Category::Math,
        BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => Category::Bitwise,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => Category::Comparison,
    }
}

fn op_lang(op: BinOp, assign: bool) -> Option<LangItem> {
    Some(match (op, assign) {
        (BinOp::Add, false) => LangItem::Add,
        (BinOp::Sub, false) => LangItem::Sub,
        (BinOp::Mul, false) => LangItem::Mul,
        (BinOp::Div, false) => LangItem::Div,
        (BinOp::Rem, false) => LangItem::Rem,
        (BinOp::BitAnd, false) => LangItem::BitAnd,
        (BinOp::BitOr, false) => LangItem::BitOr,
        (BinOp::BitXor, false) => LangItem::BitXor,
        (BinOp::Shl, false) => LangItem::Shl,
        (BinOp::Shr, false) => LangItem::Shr,
        (BinOp::Add, true) => LangItem::AddAssign,
        (BinOp::Sub, true) => LangItem::SubAssign,
        (BinOp::Mul, true) => LangItem::MulAssign,
        (BinOp::Div, true) => LangItem::DivAssign,
        (BinOp::Rem, true) => LangItem::RemAssign,
        (BinOp::BitAnd, true) => LangItem::BitAndAssign,
        (BinOp::BitOr, true) => LangItem::BitOrAssign,
        (BinOp::BitXor, true) => LangItem::BitXorAssign,
        (BinOp::Shl, true) => LangItem::ShlAssign,
        (BinOp::Shr, true) => LangItem::ShrAssign,
        (BinOp::Eq | BinOp::Ne, _) => LangItem::PartialEq,
        (BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge, _) => LangItem::PartialOrd,
        (BinOp::And | BinOp::Or, _) => return None,
    })
}

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    fn is_integral(&mut self, t: Ty) -> bool {
        matches!(self.view(t), TyView::Int(_) | TyView::Uint(_) | TyView::Infer(Var { kind: VarKind::Int, .. }))
    }

    fn is_float(&mut self, t: Ty) -> bool {
        matches!(self.view(t), TyView::Float(_) | TyView::Infer(Var { kind: VarKind::Float, .. }))
    }

    fn is_bool(&mut self, t: Ty) -> bool {
        matches!(self.view(t), TyView::Bool)
    }

    fn is_scalar(&mut self, t: Ty) -> bool {
        self.is_integral(t)
            || self.is_float(t)
            || matches!(self.view(t), TyView::Bool | TyView::Char | TyView::Ptr(..) | TyView::FnPtr(..))
    }

    /// `is_builtin_binop`.
    fn is_builtin_binop(&mut self, lhs: Ty, rhs: Ty, op: BinOp) -> bool {
        if self.is_err(lhs) || self.is_err(rhs) {
            return true;
        }
        match category(op) {
            Category::Shortcircuit => true,
            Category::Shift => self.is_integral(lhs) && self.is_integral(rhs),
            Category::Math => {
                (self.is_integral(lhs) && self.is_integral(rhs)) || (self.is_float(lhs) && self.is_float(rhs))
            }
            Category::Bitwise => {
                (self.is_integral(lhs) && self.is_integral(rhs))
                    || (self.is_float(lhs) && self.is_float(rhs))
                    || (self.is_bool(lhs) && self.is_bool(rhs))
            }
            Category::Comparison => self.is_scalar(lhs) && self.is_scalar(rhs),
        }
    }

    /// `enforce_builtin_binop_types`: the builtin result.
    fn enforce_builtin(&mut self, e: ExprId, lhs: Ty, rhs: Ty, op: BinOp, assign: bool) -> Result<Ty, Stop> {
        match category(op) {
            Category::Shortcircuit => Ok(self.common.bool_),
            Category::Shift => Ok(if assign { self.common.unit } else { lhs }),
            Category::Math | Category::Bitwise => {
                self.demand_eq(At::Expr(e), lhs, rhs)?;
                Ok(if assign { self.common.unit } else { lhs })
            }
            Category::Comparison => {
                self.demand_eq(At::Expr(e), lhs, rhs)?;
                Ok(self.common.bool_)
            }
        }
    }

    pub(crate) fn expr_binary(&mut self, f: Frame, e: ExprId, op: BinOp, lhs: ExprId, rhs: ExprId) -> Result<Flow, Stop> {
        if category(op) == Category::Shortcircuit {
            return self.expr_lazy(f, lhs, rhs);
        }
        self.binop(f, e, op, lhs, rhs, false)
    }

    pub(crate) fn expr_assign_op(&mut self, f: Frame, e: ExprId, op: BinOp, lhs: ExprId, rhs: ExprId) -> Result<Flow, Stop> {
        if f.state == 0 {
            self.check_place_code(lhs, DiagCode::InvalidCompoundLhs)?;
        }
        self.binop(f, e, op, lhs, rhs, true)
    }

    /// `&&` and `||`: the right side may not run, so its divergence is dropped.
    fn expr_lazy(&mut self, mut f: Frame, lhs: ExprId, rhs: ExprId) -> Result<Flow, Stop> {
        let b = self.common.bool_;
        match f.state {
            0 => {
                self.resume(f, 1)?;
                self.push_coercible(lhs, b)?;
                Ok(Flow::Wait)
            }
            1 => {
                f.b = u32::from(self.diverges);
                self.resume(f, 2)?;
                self.push_coercible(rhs, b)?;
                Ok(Flow::Wait)
            }
            _ => {
                self.diverges = f.b != 0;
                Ok(Flow::Done(b))
            }
        }
    }

    /// `check_overloaded_binop` and the builtin fast path. `f.t` holds the left type, `f.u` the
    /// right variable, `f.a` the trait's output (a `Ty` index) and `f.b` whether the trait
    /// obligation was registered.
    fn binop(&mut self, mut f: Frame, e: ExprId, op: BinOp, lhs: ExprId, rhs: ExprId, assign: bool) -> Result<Flow, Stop> {
        match f.state {
            0 => {
                self.resume(f, 1)?;
                self.push_expr(lhs, Expectation::None, Then::Nothing)?;
                Ok(Flow::Wait)
            }
            1 => {
                let lhs_ty = self.try_structurally_resolve(self.ret)?;
                let rhs_var = self.fresh()?;
                f.t = lhs_ty;
                f.u = rhs_var;
                f.b = 0;
                f.a = self.common.err.0;
                if let Some(item) = op_lang(op, assign) {
                    // `f.b == 2`: the world has no `item`; reported at the end unless builtin.
                    let tr = self.lang(item)?;
                    if tr.is_none() {
                        f.b = 2;
                    }
                    if let Some(tr) = tr {
                        let args = self.mk_list(&[lhs_ty, rhs_var])?;
                        let goal = Goal::Trait { trait_: tr, args };
                        // `lookup_method_in_trait`: only `predicate_may_hold`. A left side that
                        // is still a variable (`|a, b| a + b`) may hold: the obligation is
                        // registered ambiguous and `Output` projected, both settled by a later use.
                        if self.ty.may_hold(goal, &mut self.reads)? {
                            self.register_trait(tr, args, Cause::Expr(e))?;
                            let out = match category(op) {
                                Category::Comparison => self.common.bool_,
                                _ if assign => self.common.unit,
                                _ => self.project(tr, args, Name::of("Output"), Cause::Expr(e))?,
                            };
                            f.a = out.0;
                            f.b = 1;
                        }
                    }
                }
                self.resume(f, 2)?;
                self.push_expr(rhs, Expectation::HasType(rhs_var), Then::Coerce { to: rhs_var, two_phase: false })?;
                Ok(Flow::Wait)
            }
            _ => {
                let lhs_ty = self.ty.shallow_resolve(f.t);
                let rhs_ty = self.ty.shallow_resolve(f.u);
                let out = Ty(f.a);
                if !self.is_ty_var(lhs_ty) && !self.is_ty_var(rhs_ty) && self.is_builtin_binop(lhs_ty, rhs_ty, op) {
                    let builtin = self.enforce_builtin(e, lhs_ty, rhs_ty, op, assign)?;
                    if f.b == 1 {
                        self.unify(builtin, out)?;
                    }
                    return Ok(Flow::Done(builtin));
                }
                if f.b == 1 {
                    return Ok(Flow::Done(out));
                }
                if f.b == 2
                    && let Some(item) = op_lang(op, assign)
                {
                    self.missing_lang(item, At::Expr(e));
                    return Ok(Flow::Done(self.common.err));
                }
                if !self.is_err(lhs_ty) {
                    self.diag(DiagCode::BinOp, At::Expr(e), None, Some(lhs_ty), None);
                }
                Ok(Flow::Done(self.common.err))
            }
        }
    }

    pub(crate) fn expr_unary(&mut self, f: Frame, e: ExprId, op: UnOp, operand: ExprId) -> Result<Flow, Stop> {
        if f.state == 0 {
            let expect = if op == UnOp::Deref { Expectation::None } else { f.expect };
            self.resume(f, 1)?;
            self.push_expr(operand, expect, Then::Nothing)?;
            return Ok(Flow::Wait);
        }
        let t = self.ret;
        if self.is_err(t) {
            return Ok(Flow::Done(t));
        }
        let t = self.structurally_resolve(t, At::Expr(operand))?;
        match op {
            UnOp::Deref => {
                match self.view(t) {
                    TyView::Ref(_, inner) | TyView::Ptr(_, inner) => return Ok(Flow::Done(inner)),
                    TyView::Error => return Ok(Flow::Done(t)),
                    _ => {}
                }
                if let Some(target) = self.overloaded_deref(t)? {
                    let kind = crate::columns::AdjustKind::Deref { overloaded: Some(Mutbl::Not) };
                    self.adjust.push((operand, kind, target));
                    return Ok(Flow::Done(target));
                }
                if self.need_lang(LangItem::Deref, At::Expr(e))?.is_none() {
                    return Ok(Flow::Done(self.common.err));
                }
                self.diag(DiagCode::CantDeref, At::Expr(e), None, Some(t), None);
                Ok(Flow::Done(self.common.err))
            }
            UnOp::Not | UnOp::Neg => {
                let builtin = if op == UnOp::Not {
                    self.is_integral(t) || self.is_bool(t)
                } else {
                    self.is_integral(t) || self.is_float(t)
                };
                if builtin {
                    return Ok(Flow::Done(t));
                }
                let item = if op == UnOp::Not { LangItem::Not } else { LangItem::Neg };
                let Some(tr) = self.need_lang(item, At::Expr(e))? else {
                    return Ok(Flow::Done(self.common.err));
                };
                let args = self.mk_list(&[t])?;
                if self.ty.may_hold(Goal::Trait { trait_: tr, args }, &mut self.reads)? {
                    self.register_trait(tr, args, Cause::Expr(e))?;
                    let out = self.project(tr, args, Name::of("Output"), Cause::Expr(e))?;
                    return Ok(Flow::Done(out));
                }
                self.diag(DiagCode::UnOp, At::Expr(e), None, Some(t), None);
                Ok(Flow::Done(self.common.err))
            }
        }
    }

    /// `check_expr_addr_of`: the expectation of the operand is the expected pointee, as an
    /// rvalue hint when it is unsized.
    pub(crate) fn expr_ref(&mut self, f: Frame, e: ExprId, mutbl: Mutbl, operand: ExprId) -> Result<Flow, Stop> {
        self.expr_borrow(f, e, mutbl, operand, false)
    }

    /// `&` (`raw` false: a reference) and `&raw` (`raw` true: a raw pointer).
    fn expr_borrow(&mut self, f: Frame, _e: ExprId, mutbl: Mutbl, operand: ExprId, raw: bool) -> Result<Flow, Stop> {
        if f.state == 0 {
            let hint = match f.expect.to_option() {
                Some(t) => match self.view(t) {
                    TyView::Ref(_, inner) | TyView::Ptr(_, inner) => {
                        let inner = self.ty.shallow_resolve(inner);
                        match self.view(inner) {
                            TyView::Str | TyView::Slice(_) => Expectation::RvalueLikeUnsized(inner),
                            _ => Expectation::HasType(inner),
                        }
                    }
                    _ => Expectation::None,
                },
                None => Expectation::None,
            };
            self.resume(f, 1)?;
            self.push_expr(operand, hint, Then::Nothing)?;
            return Ok(Flow::Wait);
        }
        let inner = self.ret;
        if self.is_err(inner) {
            return Ok(Flow::Done(inner));
        }
        let made = if raw { TyView::Ptr(mutbl, inner) } else { TyView::Ref(mutbl, inner) };
        Ok(Flow::Done(self.mk(made)?))
    }

    /// `&raw const place` is `*const T` and `&raw mut place` is `*mut T` for a place of type
    /// `T`; the operand must be a place (E0745), and an expected `*_ U` is its hint, as `&`'s.
    pub(crate) fn expr_raw_ref(&mut self, f: Frame, e: ExprId, mutbl: Mutbl, operand: ExprId) -> Result<Flow, Stop> {
        if f.state == 0 {
            self.check_place_code(operand, DiagCode::RawBorrowOfTemporary)?;
        }
        self.expr_borrow(f, e, mutbl, operand, true)
    }

    /// `_` outside the left side of an assignment, which `expr_assign` types without this rule.
    pub(crate) fn expr_underscore(&mut self, e: ExprId) -> Result<Flow, Stop> {
        self.diag(DiagCode::UnderscoreExpr, At::Expr(e), None, None, None);
        Ok(Flow::Done(self.common.err))
    }

    /// `check_expr_assign`: the left side first, then the right coerced to it; `()`.
    ///
    /// `_ = rhs` is `let _ = rhs;` (rustc's destructuring-assignment lowering): the right side
    /// is checked with no expectation, its value dropped, and `_` takes its type.
    pub(crate) fn expr_assign(&mut self, mut f: Frame, _e: ExprId, lhs: ExprId, rhs: ExprId) -> Result<Flow, Stop> {
        if self.body.expr(lhs)? == Expr::Underscore {
            return match f.state {
                0 => {
                    self.resume(f, 1)?;
                    self.push_expr(rhs, Expectation::None, Then::Nothing)?;
                    Ok(Flow::Wait)
                }
                _ => {
                    self.expr_ty[lhs.ix()] = self.ret;
                    Ok(Flow::Done(self.common.unit))
                }
            };
        }
        match f.state {
            0 => {
                self.check_place(lhs)?;
                self.resume(f, 1)?;
                self.push_expr(lhs, Expectation::None, Then::Nothing)?;
                Ok(Flow::Wait)
            }
            1 => {
                f.t = self.ret;
                self.resume(f, 2)?;
                let to = f.t;
                self.push_expr(rhs, Expectation::HasType(to), Then::Coerce { to, two_phase: false })?;
                Ok(Flow::Wait)
            }
            _ => Ok(Flow::Done(self.common.unit)),
        }
    }

    /// `is_syntactic_place_expr`, else E0070. Bounded by the body's expression count.
    fn check_place(&mut self, lhs: ExprId) -> Result<(), Stop> {
        self.check_place_code(lhs, DiagCode::InvalidLhs)
    }

    /// `is_syntactic_place_expr`, else `code` at `lhs`.
    fn check_place_code(&mut self, lhs: ExprId, code: DiagCode) -> Result<(), Stop> {
        let mut cur = lhs;
        for _ in 0..=self.body.exprs.len() {
            match self.body.expr(cur)? {
                Expr::Paren(inner) => cur = inner,
                Expr::Local(_) | Expr::Field { .. } | Expr::TupleField { .. } | Expr::Index { .. } | Expr::Missing => {
                    return Ok(());
                }
                Expr::Unary { op: UnOp::Deref, .. } => return Ok(()),
                Expr::Path(p) => {
                    // A static is a place; anything else a path names is not.
                    let bp = self.body.path(p)?;
                    // `<T as Tr>::x` names an associated fn or const: never a place.
                    if bp.anchor.is_some() {
                        self.diag(code, At::Expr(lhs), None, None, None);
                        return Ok(());
                    }
                    let segs = self.body.segment_list(bp.segments)?;
                    let mut names: blaze_own::Vec<Name, Alloc<'a>> = blaze_own::Vec::with_capacity_in(segs.len(), self.alloc);
                    for s in segs {
                        names.push(s.name);
                    }
                    let res = self.path_res(bp, p, &names, At::Expr(lhs))?;
                    if !matches!(res.map(|r| r.def), Ok(Def::Static(..)) | Err(_)) {
                        self.diag(code, At::Expr(lhs), None, None, None);
                    }
                    return Ok(());
                }
                _ => {
                    self.diag(code, At::Expr(lhs), None, None, None);
                    return Ok(());
                }
            }
        }
        Err(crate::error::CheckError::Malformed(crate::error::Malformed::Expr(lhs)).into())
    }
}
