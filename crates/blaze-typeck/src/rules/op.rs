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
use crate::diag::{About, At, Because, DiagCode};
use crate::error::Stop;
use crate::ids::{ExprId, LangItem, Mutbl, Name};
use crate::machine::{Expectation, Flow, Frame, Machine, Then};
use crate::ty::{Cause, Goal, Ty, TyView, TypeCtx, Var, VarKind};
use crate::world::{Def, World};

use super::builtin_ops::{self, Decision, Yields};

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

/// The trait method an overloaded binary operator or compound assignment calls. The Reference,
/// "Operator expressions": "Arithmetic and Logical Binary Operators" (the table's "Trait"
/// column: `+` is `std::ops::Add`, .., `>>` is `std::ops::Shr`), "Comparison Operators"
/// ([expr.cmp.behavior]: `==` is `std::cmp::PartialEq::eq`, `!=` is `std::cmp::PartialEq::ne`,
/// `>` is `std::cmp::PartialOrd::gt`, `<` `lt`, `>=` `ge`, `<=` `le`) and "Compound assignment
/// expressions" (`+=` is `AddAssign::add_assign`, ..). `&&` and `||` are no trait's.
fn binop_method(op: BinOp, assign: bool) -> Option<&'static str> {
    Some(match (op, assign) {
        (BinOp::Add, false) => "add",
        (BinOp::Sub, false) => "sub",
        (BinOp::Mul, false) => "mul",
        (BinOp::Div, false) => "div",
        (BinOp::Rem, false) => "rem",
        (BinOp::BitAnd, false) => "bitand",
        (BinOp::BitOr, false) => "bitor",
        (BinOp::BitXor, false) => "bitxor",
        (BinOp::Shl, false) => "shl",
        (BinOp::Shr, false) => "shr",
        (BinOp::Add, true) => "add_assign",
        (BinOp::Sub, true) => "sub_assign",
        (BinOp::Mul, true) => "mul_assign",
        (BinOp::Div, true) => "div_assign",
        (BinOp::Rem, true) => "rem_assign",
        (BinOp::BitAnd, true) => "bitand_assign",
        (BinOp::BitOr, true) => "bitor_assign",
        (BinOp::BitXor, true) => "bitxor_assign",
        (BinOp::Shl, true) => "shl_assign",
        (BinOp::Shr, true) => "shr_assign",
        (BinOp::Eq, _) => "eq",
        (BinOp::Ne, _) => "ne",
        (BinOp::Lt, _) => "lt",
        (BinOp::Le, _) => "le",
        (BinOp::Gt, _) => "gt",
        (BinOp::Ge, _) => "ge",
        (BinOp::And | BinOp::Or, _) => return None,
    })
}

/// Why `-x` on an unsigned integer is refused (E0600): the Reference, "Negation operators",
/// [expr.negate.results], its table's note on `-` for integers.
const NEG_UNSIGNED: Because = Because::Reference { rule: "expr.negate.results", says: "Negation: Only for signed integer types." };

/// Why `_` is refused outside an assignment's left side: the Reference, "Underscore
/// expressions", [expr.placeholder.lhs-assignment-only].
const UNDERSCORE_LHS_ONLY: About = About::Rule {
    rule: "expr.placeholder.lhs-assignment-only",
    says: "They may only appear in the left-hand side of an assignment.",
};

/// Why a value expression is refused where a place is wanted (an assignment's left side, a
/// compound assignment's, `&raw`'s operand): the Reference, "Place expressions and value
/// expressions", [expr.place-value.place-memory-location].
const PLACE_EXPR: About = About::Rule {
    rule: "expr.place-value.place-memory-location",
    says: "A place expression is an expression that represents a memory location.",
};

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// Whether callee row `(e, _, args)` is an operator's whose FINAL operand types are that
    /// operator's builtin scalar types: then the operator is the builtin one and the row is
    /// dropped (rustc's writeback `fix_scalar_builtin_expr`: an operand that was a variable
    /// when the operator was checked, `let x = Default::default(); x + 1u32`, is known here).
    /// A row left in `Checked::callees` on an operator expression therefore says: this
    /// operator is a call of the trait's method at these arguments, and the absence of one
    /// says: builtin, on builtin scalar types. One home for the decision (rule 30): the build
    /// reads it and never decides from how a value is held.
    pub(crate) fn operator_settled_builtin(&mut self, e: ExprId, args: crate::ty::TyList) -> Result<bool, crate::error::CheckError> {
        let n = self.ty.list_len(args);
        match self.body.expr(e)? {
            Expr::Binary { op, .. } | Expr::AssignOp { op, .. } if n == 2 => {
                let lhs = self.ty.list_get(args, 0);
                let lhs = self.ty.resolve(lhs)?;
                let rhs = self.ty.list_get(args, 1);
                let rhs = self.ty.resolve(rhs)?;
                Ok(self.is_builtin_binop(lhs, rhs, op))
            }
            Expr::Unary { op: op @ (UnOp::Neg | UnOp::Not), .. } if n == 1 => {
                let t = self.ty.list_get(args, 0);
                let t = self.ty.resolve(t)?;
                Ok(self.is_err(t) || if op == UnOp::Not { self.is_integral(t) || self.is_bool(t) } else { self.is_neg_builtin(t) })
            }
            _ => Ok(false),
        }
    }

    fn is_integral(&mut self, t: Ty) -> bool {
        matches!(self.view(t), TyView::Int(_) | TyView::Uint(_) | TyView::Infer(Var { kind: VarKind::Int, .. }))
    }

    /// Whether unary `-` is builtin on `t`: a signed integer, an integer variable, or a float
    /// ([`NEG_UNSIGNED`]: "Only for signed integer types"). On an unsigned integer it is no
    /// builtin: the `Neg` trait is asked, as on any other type, and core has no impl there.
    /// MISSING: an integer variable that later resolves to an unsigned type is not refused.
    fn is_neg_builtin(&mut self, t: Ty) -> bool {
        matches!(self.view(t), TyView::Int(_) | TyView::Infer(Var { kind: VarKind::Int, .. })) || self.is_float(t)
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

    /// rustc op.rs `deref_ty_if_possible`: `T` for `&T` (a shared reference only), else `t`.
    fn peel_shared_ref(&mut self, t: Ty) -> Ty {
        match self.view(t) {
            TyView::Ref(Mutbl::Not, inner) => inner,
            _ => t,
        }
    }

    /// Records the operator `e` as the call of its trait's method at `[lhs, rhs]`. That is the
    /// row `operator_settled_builtin` drops when the final types are builtin scalars.
    fn push_binop_callee(&mut self, e: ExprId, op: BinOp, assign: bool, lhs: Ty, rhs: Ty) -> Result<(), Stop> {
        if let Some(item) = op_lang(op, assign)
            && let Some(tr) = self.lang(item)?
            && let Some(name) = binop_method(op, assign)
            && let Some(m) = self.reads.assoc_item(tr, Name::of(name))?
        {
            let args = self.mk_list(&[lhs, rhs])?;
            self.callees.push((e, m.item, args));
        }
        Ok(())
    }

    /// `is_builtin_binop` on types already peeled ([`Machine::peel_shared_ref`]).
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

    /// `lookup_op_method` for `f.t op f.u`: asks whether the operator's trait may hold at
    /// `[f.t, ask_rhs]` and, when it may, registers the obligation at `[f.t, f.u]` and projects
    /// `Output`. `ask_rhs` is `f.u` when the right operand is not checked yet (state 1), and a
    /// fresh variable when the question was deferred to state 2: the question is then the one
    /// state 1 would have asked (`lhs: Op<?R>`), so a deferral never changes its answer or the
    /// finding (`1 + 1.0` stays E0277, the registered obligation failing). Sets `f.a` (the
    /// trait's output) and `f.b` (1: registered; 2: the world has no such lang trait, reported
    /// at the end unless the operator is builtin).
    fn binop_lookup(&mut self, f: &mut Frame, e: ExprId, op: BinOp, assign: bool, ask_rhs: Ty) -> Result<(), Stop> {
        let Some(item) = op_lang(op, assign) else { return Ok(()) };
        let Some(tr) = self.lang(item)? else {
            f.b = 2;
            return Ok(());
        };
        let asked = self.mk_list(&[f.t, ask_rhs])?;
        let goal = Goal::Trait { trait_: tr, args: asked };
        let args = self.mk_list(&[f.t, f.u])?;
        // `lookup_method_in_trait`: only `predicate_may_hold`. A left side that is still a
        // variable (`|a, b| a + b`) may hold: the obligation is registered ambiguous and
        // `Output` projected, both settled by a later use.
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
        Ok(())
    }

    /// `check_overloaded_binop` and the builtin fast path. `f.t` holds the left type, `f.u` the
    /// right variable, `f.a` the trait's output (a `Ty` index) and `f.b` what was done about
    /// the operator's trait (0: asked, does not hold; 1: registered; 2: no such lang trait;
    /// 3: not asked yet, the left type being one some right operand makes the operator
    /// built in on: `builtin_ops::defers`, integer and float literal variables included).
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
                let lhs_shape = builtin_ops::shape(self.view(lhs_ty));
                if builtin_ops::defers(op, assign, lhs_shape) {
                    // `f.b == 3`: the left side is integral, floating, `bool` or `char` (a
                    // literal's variable included). The trait is not asked yet: if the right
                    // side comes back of the class `builtin_ops::classify` needs, it never is.
                    f.b = 3;
                } else {
                    self.binop_lookup(&mut f, e, op, assign, rhs_var)?;
                }
                self.resume(f, 2)?;
                self.push_expr(rhs, Expectation::HasType(rhs_var), Then::Coerce { to: rhs_var, two_phase: false })?;
                Ok(Flow::Wait)
            }
            _ => {
                // rustc's `resolve_vars_with_obligations` on both sides: an operand whose type
                // is a projection still pending (`count + t[1]`, `t[1]`'s type `<Table as
                // Index<usize>>::Output`) is normalized BEFORE builtin is decided, so the
                // builtin `+` on `i32` is found in a crate that declares no `add` lang trait.
                let mut lhs_ty = self.try_structurally_resolve(f.t)?;
                let mut rhs_ty = self.try_structurally_resolve(f.u)?;
                if f.b == 3 {
                    // Both operand types are of one built-in class for the operator
                    // (rules/builtin_ops.rs says the rule and why no answer can differ): the
                    // operator is the built-in one, with no callee row, no obligation and no
                    // question to the solver. Unifying `{integer}` with `u8` resolves the
                    // variable as the solver's answer would have.
                    let lhs_shape = builtin_ops::shape(self.view(lhs_ty));
                    let rhs_shape = builtin_ops::shape(self.view(rhs_ty));
                    if let Decision::Builtin { unify, yields } = builtin_ops::classify(op, assign, lhs_shape, rhs_shape) {
                        if unify {
                            self.demand_eq(At::Expr(e), lhs_ty, rhs_ty)?;
                        }
                        let value = match yields {
                            Yields::Left => lhs_ty,
                            Yields::Bool => self.common.bool_,
                            Yields::Unit => self.common.unit,
                        };
                        return Ok(Flow::Done(value));
                    }
                    // Anything else (a reference, a type parameter, a type still unknown, an
                    // ADT, an integer against a float): the trait is asked now with the
                    // question state 1 would have asked (`lhs: Op<?R>`, a fresh `?R`).
                    f.b = 0;
                    let ask_rhs = self.fresh()?;
                    self.binop_lookup(&mut f, e, op, assign, ask_rhs)?;
                    lhs_ty = self.try_structurally_resolve(f.t)?;
                    rhs_ty = self.try_structurally_resolve(f.u)?;
                }
                let out = Ty(f.a);
                // rustc's `is_builtin_binop` and `enforce_builtin_binop_types` peel one shared
                // reference from each side first ("Special-case a single layer of referencing,
                // so that things like `5.0 + &6.0f32` work", rust-lang/rust#57447). So
                // `header_byte >> 4` on `header_byte: &u8` is `u8` before `&u8: Shr<{integer}>`
                // is selected (turso core/json/jsonb.rs:1041). The trait's output is made equal
                // to it, and the callee row stays: writeback's `fix_scalar_builtin_expr` tests
                // `is_scalar` on the unpeeled types, as `operator_settled_builtin` does.
                // A peeled side is taken only once the trait is registered (`f.b == 1`). Where
                // `&T: Op<..>` cannot hold, the error below is still the trait's (E0369).
                let (lhs_b, rhs_b) = match (self.peel_shared_ref(lhs_ty), self.peel_shared_ref(rhs_ty)) {
                    (l, r) if f.b == 1 => (l, r),
                    _ => (lhs_ty, rhs_ty),
                };
                if !self.is_ty_var(lhs_ty) && !self.is_ty_var(rhs_ty) && self.is_builtin_binop(lhs_b, rhs_b, op) {
                    let builtin = self.enforce_builtin(e, lhs_b, rhs_b, op, assign)?;
                    if f.b == 1 {
                        self.unify(builtin, out)?;
                        // A peeled reference: the operator is still the trait's method on
                        // the reference (core's `forward_ref_binop` impls).
                        if lhs_b != lhs_ty || rhs_b != rhs_ty {
                            self.push_binop_callee(e, op, assign, f.t, f.u)?;
                        }
                    }
                    return Ok(Flow::Done(builtin));
                }
                if f.b == 1 {
                    self.push_binop_callee(e, op, assign, f.t, f.u)?;
                    return Ok(Flow::Done(out));
                }
                if f.b == 2
                    && let Some(item) = op_lang(op, assign)
                {
                    self.missing_lang(item, At::Expr(e));
                    return Ok(Flow::Done(self.common.err));
                }
                if !self.is_err(lhs_ty) {
                    self.diag(DiagCode::BinOp, At::Expr(e), About::found(lhs_ty));
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
                if let Some(target) = self.overloaded_deref_at(t, crate::machine::DerefAt::Operator)? {
                    let kind = crate::columns::AdjustKind::Deref { overloaded: Some(Mutbl::Not) };
                    self.adjust.push((operand, kind, target));
                    return Ok(Flow::Done(target));
                }
                if self.need_lang(LangItem::Deref, At::Expr(e))?.is_none() {
                    return Ok(Flow::Done(self.common.err));
                }
                self.diag(DiagCode::CantDeref, At::Expr(e), About::found(t));
                Ok(Flow::Done(self.common.err))
            }
            UnOp::Not | UnOp::Neg => {
                let builtin = if op == UnOp::Not { self.is_integral(t) || self.is_bool(t) } else { self.is_neg_builtin(t) };
                if builtin {
                    // rustc's `check_expr_unop`: `-x` on an integer variable still owes `Neg`,
                    // so a variable that later resolves to an unsigned type is refused when the
                    // obligation is solved ([`NEG_UNSIGNED`]); the builtin type is kept for
                    // inference. A concrete signed integer or float has core's impl: nothing owed.
                    if op == UnOp::Neg
                        && matches!(self.view(t), TyView::Infer(Var { kind: VarKind::Int, .. }))
                        && let Some(tr) = self.lang(LangItem::Neg)?
                    {
                        let args = self.mk_list(&[t])?;
                        self.register_trait(tr, args, Cause::Expr(e))?;
                    }
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
                    // The operator is the trait's method at `[operand]` (`binop`'s row).
                    let name = if op == UnOp::Not { "not" } else { "neg" };
                    if let Some(m) = self.reads.assoc_item(tr, Name::of(name))? {
                        self.callees.push((e, m.item, args));
                    }
                    return Ok(Flow::Done(out));
                }
                self.diag(DiagCode::UnOp, At::Expr(e), About::found(t));
                if op == UnOp::Neg
                    && matches!(self.view(t), TyView::Uint(_))
                    && let Some(d) = self.diags.last_mut()
                {
                    d.because = Some(NEG_UNSIGNED);
                }
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
        self.diag(DiagCode::UnderscoreExpr, At::Expr(e), UNDERSCORE_LHS_ONLY);
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
    pub(crate) fn check_place_code(&mut self, lhs: ExprId, code: DiagCode) -> Result<(), Stop> {
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
                        self.diag(code, At::Expr(lhs), PLACE_EXPR);
                        return Ok(());
                    }
                    let segs = self.body.segment_list(bp.segments)?;
                    let mut names: blaze_own::Vec<Name, Alloc<'a>> = blaze_own::Vec::with_capacity_in(segs.len(), self.alloc);
                    for s in segs {
                        names.push(s.name);
                    }
                    let res = self.path_res(bp, p, &names, At::Expr(lhs))?;
                    if !matches!(res.map(|r| r.def), Ok(Def::Static(..)) | Err(_)) {
                        self.diag(code, At::Expr(lhs), PLACE_EXPR);
                    }
                    return Ok(());
                }
                _ => {
                    self.diag(code, At::Expr(lhs), PLACE_EXPR);
                    return Ok(());
                }
            }
        }
        Err(crate::error::CheckError::Malformed(crate::error::Malformed::Expr(lhs)).into())
    }
}
