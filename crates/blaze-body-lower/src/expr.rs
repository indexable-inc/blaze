//! Values, places and operands; literals and paths; calls; operators with their checks; casts,
//! borrows and aggregates; assignment.
//!
//! An expression is lowered one of three ways: [`Lower::lower_into`] writes its value into a place,
//! [`Lower::place`] gives the place it names (a place expression's own, or a temporary holding
//! a value expression's value), [`Lower::operand`] gives it as an operand (a copy or move of
//! its place, or a constant). Each applies the record's adjustments (`Checked::adjusts`) in
//! their order after the expression's own value.
//!
//! Evaluation order is the Reference's, "Evaluation order of operands": operands left to
//! right; a call's callee, then its arguments; an assignment's value, then its place; a
//! compound assignment of primitives its value then its place, of other types its place then
//! its value ("Compound assignment expressions").
//!
//! Arithmetic checks ("Arithmetic and logical binary operators", "Overflow"): `/` and `%` by
//! zero, and `MIN / -1`, are checked in every build; `+ - * << >>` and unary `-` are written as
//! `checked` operations with an `Overflow` assert, which a build without overflow checks drops
//! (a build fact, as panic=abort is: the body does not change). A negated literal is folded and
//! not checked.

use core::alloc::Allocator;

use blaze_body::{
    AggregateKind, AssertKind, BinOp as BBin, BlockKind, BorrowKind, Callee, CastKind, ConstDecl, ConstKind, Local,
    LocalDecl, LocalKind, Operand, Place, Proj, Role, Span, StmtKind, UnOp as BUn, Unwind, Value,
};
use blaze_own::Vec;
use blaze_typeck::{
    AdjustKind, Arm, BinOp, Def, Expr, ExprId, FieldInit, FieldPat, IntTy, ItemKey, Lit, List, Mutbl, PNode, PTy, PatId,
    Stmt, StmtId, UintTy, UnOp,
};

use crate::scope::Pending;
use crate::{At, BodyFacts, Construct, Lower, LowerError, MOST_DEPTH};

/// A lowered expression before it is used: a place and its type, or an operand and its type.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Val {
    Place(Place, PTy),
    Op(Operand, PTy),
}

const EMPTY: List = List { start: 0, len: 0 };

fn mask(bits: u32) -> u128 {
    if bits >= 128 { u128::MAX } else { (1u128 << bits) - 1 }
}

fn comparison(op: BinOp) -> bool {
    matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
}

impl<'b, 'v, 'c, F: BodyFacts, A: Allocator + Copy> Lower<'b, 'v, 'c, F, A> {
    // The body's side tables, read with the body's lifetime.

    pub(crate) fn exprs_in(&self, l: List, at: ExprId) -> Result<&'v [ExprId], LowerError> {
        let body: &'b blaze_typeck::BodyView<'v> = self.body;
        match body.expr_lists.get(l.range()) {
            Some(s) => Ok(s),
            None => self.refuse(Construct::Malformed, at),
        }
    }

    pub(crate) fn pats_in(&self, l: List, at: PatId) -> Result<&'v [PatId], LowerError> {
        let body: &'b blaze_typeck::BodyView<'v> = self.body;
        match body.pat_lists.get(l.range()) {
            Some(s) => Ok(s),
            None => self.refuse_pat(Construct::Malformed, at),
        }
    }

    pub(crate) fn stmts_in(&self, l: List, at: ExprId) -> Result<&'v [StmtId], LowerError> {
        let body: &'b blaze_typeck::BodyView<'v> = self.body;
        match body.stmt_lists.get(l.range()) {
            Some(s) => Ok(s),
            None => self.refuse(Construct::Malformed, at),
        }
    }

    pub(crate) fn arms_in(&self, l: List, at: ExprId) -> Result<&'v [Arm], LowerError> {
        let body: &'b blaze_typeck::BodyView<'v> = self.body;
        match body.arms.get(l.range()) {
            Some(s) => Ok(s),
            None => self.refuse(Construct::Malformed, at),
        }
    }

    pub(crate) fn field_inits_in(&self, l: List, at: ExprId) -> Result<&'v [FieldInit], LowerError> {
        let body: &'b blaze_typeck::BodyView<'v> = self.body;
        match body.field_inits.get(l.range()) {
            Some(s) => Ok(s),
            None => self.refuse(Construct::Malformed, at),
        }
    }

    pub(crate) fn field_pats_in(&self, l: List, at: PatId) -> Result<&'v [FieldPat], LowerError> {
        let body: &'b blaze_typeck::BodyView<'v> = self.body;
        match body.field_pats.get(l.range()) {
            Some(s) => Ok(s),
            None => self.refuse_pat(Construct::Malformed, at),
        }
    }

    pub(crate) fn stmt_at(&self, s: StmtId, at: ExprId) -> Result<Stmt, LowerError> {
        match self.body.stmts.get(s.0 as usize) {
            Some(x) => Ok(*x),
            None => self.refuse(Construct::Malformed, at),
        }
    }

    // Locals.

    /// A new temporary of `ty` made for `e`, its storage begun and scheduled for drop.
    pub(crate) fn temp_local(&mut self, e: ExprId, ty: PTy, role: Role) -> Result<(Local, Place), LowerError> {
        let local = self.b.new_local(LocalDecl { ty, kind: LocalKind::Temp(e, role), mutbl: Mutbl::Mut });
        let place = self.b.local_place(local);
        let drops = self.needs_drop(ty, At::Expr(e))?;
        let span = self.span(e);
        self.declare(local, place, drops, e, span);
        Ok((local, place))
    }

    pub(crate) fn temp(&mut self, e: ExprId, ty: PTy, role: Role) -> Result<Place, LowerError> {
        Ok(self.temp_local(e, ty, role)?.1)
    }

    /// A temporary read at once by the row after it (a condition, a length, a checked pair, a
    /// discriminant): no storage rows, not scheduled; its type never needs drop.
    pub(crate) fn scratch(&mut self, e: ExprId, ty: PTy) -> Place {
        let local = self.b.new_local(LocalDecl { ty, kind: LocalKind::Temp(e, Role::Value), mutbl: Mutbl::Mut });
        self.b.local_place(local)
    }

    pub(crate) fn bool_ty(&mut self, at: At) -> Result<PTy, LowerError> {
        self.find_ty(PNode::Bool, at)
    }

    pub(crate) fn scalar(&mut self, bits: u128, ty: PTy) -> Operand {
        Operand::Const(self.b.new_const(ConstDecl { kind: ConstKind::Scalar { bits }, ty }))
    }

    /// `dest = ()` (nothing when the expression's type is `!`).
    pub(crate) fn unit(&mut self, dest: Place, ty: PTy, at: At, span: Span) -> Result<(), LowerError> {
        if let PNode::Never = self.node(ty, at)? {
            return Ok(());
        }
        let operands = self.b.operands(&[]);
        self.b.assign(dest, Value::Aggregate { kind: AggregateKind::Tuple, ty, operands }, span);
        Ok(())
    }

    pub(crate) fn goto(&mut self, to: blaze_body::Block, span: Span) {
        self.b.terminate(StmtKind::Goto(to), span);
    }

    // The three ways.

    /// Writes `e`'s adjusted value into `dest`. Not named `into`: a call on a `Lower` taken by
    /// value would resolve to `core::convert::Into::into` before this inherent method.
    pub(crate) fn lower_into(&mut self, dest: Place, e: ExprId) -> Result<(), LowerError> {
        self.depth += 1;
        if self.depth > MOST_DEPTH {
            return self.refuse(Construct::Depth, e);
        }
        let r = self.into_inner(dest, e);
        self.depth -= 1;
        r
    }

    fn into_inner(&mut self, dest: Place, e: ExprId) -> Result<(), LowerError> {
        let (lo, hi) = self.adjust_rows(e);
        if hi > lo && matches!(self.adjust(lo), Some((AdjustKind::NeverToAny, _))) {
            // A diverging expression: nothing is written; the code after it is not reached.
            let ty = self.expr_ty(e)?;
            let t = self.scratch(e, ty);
            return self.value_into(t, e);
        }
        if hi > lo || self.is_place_expr(e)? {
            let v = self.val(e)?;
            let op = self.use_val(v, e)?;
            let span = self.span(e);
            self.b.assign(dest, Value::Use(op), span);
            return Ok(());
        }
        self.value_into(dest, e)
    }

    pub(crate) fn operand(&mut self, e: ExprId) -> Result<Operand, LowerError> {
        let v = self.val(e)?;
        self.use_val(v, e)
    }

    pub(crate) fn place(&mut self, e: ExprId) -> Result<Place, LowerError> {
        let v = self.val(e)?;
        self.to_place(v, e)
    }

    /// `e` lowered and adjusted.
    pub(crate) fn val(&mut self, e: ExprId) -> Result<Val, LowerError> {
        self.depth += 1;
        if self.depth > MOST_DEPTH {
            return self.refuse(Construct::Depth, e);
        }
        let r = self.val_inner(e);
        self.depth -= 1;
        r
    }

    fn val_inner(&mut self, e: ExprId) -> Result<Val, LowerError> {
        let (lo, hi) = self.adjust_rows(e);
        let ty = self.expr_ty(e)?;
        let mut v = if self.is_place_expr(e)? {
            Val::Place(self.place_unadjusted(e)?, ty)
        } else if let Some(c) = self.const_operand(e)? {
            Val::Op(c, ty)
        } else if hi > lo && matches!(self.adjust(lo), Some((AdjustKind::NeverToAny, _))) {
            let t = self.scratch(e, ty);
            self.value_into(t, e)?;
            let Some((_, to)) = self.adjust(hi - 1) else {
                return self.refuse(Construct::Malformed, e);
            };
            let u = self.scratch(e, to);
            return Ok(Val::Op(Operand::Move(u), to));
        } else {
            let t = self.temp(e, ty, Role::Value)?;
            self.value_into(t, e)?;
            Val::Place(t, ty)
        };
        for i in lo..hi {
            let Some((k, to)) = self.adjust(i) else {
                return self.refuse(Construct::Malformed, e);
            };
            v = self.apply(v, k, to, e)?;
        }
        Ok(v)
    }

    pub(crate) fn use_val(&mut self, v: Val, e: ExprId) -> Result<Operand, LowerError> {
        match v {
            Val::Place(p, ty) => {
                if self.is_copy(ty, At::Expr(e))? {
                    Ok(Operand::Copy(p))
                } else {
                    Ok(Operand::Move(p))
                }
            }
            Val::Op(o, _) => Ok(o),
        }
    }

    pub(crate) fn to_place(&mut self, v: Val, e: ExprId) -> Result<Place, LowerError> {
        match v {
            Val::Place(p, _) => Ok(p),
            Val::Op(o, ty) => {
                let t = self.temp(e, ty, Role::Value)?;
                let span = self.span(e);
                self.b.assign(t, Value::Use(o), span);
                Ok(t)
            }
        }
    }

    /// One adjustment applied to `v`, giving a value of type `to`.
    fn apply(&mut self, v: Val, k: AdjustKind, to: PTy, e: ExprId) -> Result<Val, LowerError> {
        let span = self.span(e);
        match k {
            AdjustKind::NeverToAny => {
                let u = self.scratch(e, to);
                Ok(Val::Op(Operand::Move(u), to))
            }
            AdjustKind::Deref { overloaded: None } => {
                let ty = match v {
                    Val::Place(_, t) | Val::Op(_, t) => t,
                };
                let p = self.to_place(v, e)?;
                let d = self.deref(p, ty, At::Expr(e))?;
                Ok(Val::Place(d, to))
            }
            AdjustKind::Deref { overloaded: Some(_) } => self.refuse(Construct::OverloadedDeref, e),
            AdjustKind::Borrow { mutbl, two_phase, raw } => {
                let p = self.to_place(v, e)?;
                let t = self.temp(e, to, Role::Borrow)?;
                if raw {
                    self.b.assign(t, Value::RawAddr { mutbl, place: p }, span);
                } else {
                    let kind = match (mutbl, two_phase) {
                        (Mutbl::Not, _) => BorrowKind::Shared,
                        (Mutbl::Mut, true) => BorrowKind::TwoPhase,
                        (Mutbl::Mut, false) => BorrowKind::Mut,
                    };
                    let o = self.b.origin(false);
                    let loan = self.b.borrow(t, kind, p, o, span);
                    if kind == BorrowKind::TwoPhase {
                        self.activate.push(loan);
                    }
                }
                Ok(Val::Place(t, to))
            }
            AdjustKind::ReifyFnPointer
            | AdjustKind::ClosureFnPointer
            | AdjustKind::UnsafeFnPointer
            | AdjustKind::MutToConstPointer
            | AdjustKind::Unsize => {
                let kind = match k {
                    AdjustKind::ReifyFnPointer => CastKind::ReifyFnPointer,
                    AdjustKind::ClosureFnPointer => CastKind::ClosureFnPointer,
                    AdjustKind::UnsafeFnPointer => CastKind::UnsafeFnPointer,
                    AdjustKind::MutToConstPointer => CastKind::MutToConstPointer,
                    _ => CastKind::Unsize,
                };
                let op = self.use_val(v, e)?;
                let t = self.temp(e, to, Role::Value)?;
                self.b.assign(t, Value::Cast { kind, operand: op, ty: to }, span);
                Ok(Val::Place(t, to))
            }
            AdjustKind::UnionInject { variant } => {
                let PNode::Union(ul) = self.node(to, At::Expr(e))? else {
                    return self.refuse(Construct::Malformed, e);
                };
                if variant >= ul.len {
                    return self.refuse(Construct::Malformed, e);
                }
                let op = self.use_val(v, e)?;
                let t = self.temp(e, to, Role::Value)?;
                let operands = self.b.operands(&[op]);
                self.b.assign(t, Value::Aggregate { kind: AggregateKind::Adt(variant), ty: to, operands }, span);
                Ok(Val::Place(t, to))
            }
            AdjustKind::UnionWiden => {
                let from = match v {
                    Val::Place(_, t) | Val::Op(_, t) => t,
                };
                if from == to {
                    return Ok(v);
                }
                let sp = self.to_place(v, e)?;
                let t = self.temp(e, to, Role::Value)?;
                self.remap_union(sp, from, to, t, e, span, At::Expr(e))?;
                Ok(Val::Place(t, to))
            }
            AdjustKind::RecordProject => {
                let from = match v {
                    Val::Place(_, t) | Val::Op(_, t) => t,
                };
                if from == to {
                    return Ok(v);
                }
                let at = At::Expr(e);
                let PNode::Record(src_schema, _) = self.node(from, at)? else {
                    return self.refuse(Construct::Malformed, e);
                };
                let PNode::Record(dst_schema, dst_list) = self.node(to, at)? else {
                    return self.refuse(Construct::Malformed, e);
                };
                let Some(src_names) = self.tys.record_names(src_schema) else {
                    return self.refuse(Construct::UnknownField, e);
                };
                let Some(dst_names) = self.tys.record_names(dst_schema) else {
                    return self.refuse(Construct::UnknownField, e);
                };
                let dst_tys = self.list(dst_list, at)?;
                let mut plan: Vec<(u32, PTy), A> = Vec::new_in(self.alloc);
                for (di, &nm) in dst_names.iter().enumerate() {
                    let Some(si_usize) = src_names.iter().position(|&sn| sn == nm) else {
                        return self.refuse(Construct::UnknownField, e);
                    };
                    let Ok(si) = u32::try_from(si_usize) else {
                        return self.refuse(Construct::Malformed, e);
                    };
                    let Some(&fty) = dst_tys.get(di) else {
                        return self.refuse(Construct::Malformed, e);
                    };
                    plan.push((si, fty));
                }
                let sp = self.to_place(v, e)?;
                let mut ops: Vec<Operand, A> = Vec::new_in(self.alloc);
                for &(si, fty) in plan.iter() {
                    let fp = self.b.project(sp, Proj::Field { index: si, ty: fty });
                    let op = if self.is_copy(fty, at)? { Operand::Copy(fp) } else { Operand::Move(fp) };
                    ops.push(op);
                }
                let operands = self.b.operands(&ops);
                let t = self.temp(e, to, Role::Value)?;
                self.b.assign(t, Value::Aggregate { kind: AggregateKind::Tuple, ty: to, operands }, span);
                Ok(Val::Place(t, to))
            }
        }
    }

    /// Remaps structural union value at `src_place` (of type `src_ty`) into target structural union
    /// `dst_ty` at `dest`, switching on the source discriminant across members present in both
    /// (shared by `AdjustKind::UnionWiden` and `UnionPat::Residual`).
    pub(crate) fn remap_union(
        &mut self,
        src_place: Place,
        src_ty: PTy,
        dst_ty: PTy,
        dest: Place,
        e: ExprId,
        span: Span,
        at: At,
    ) -> Result<(), LowerError> {
        if src_ty == dst_ty {
            let op = if self.is_copy(src_ty, at)? { Operand::Copy(src_place) } else { Operand::Move(src_place) };
            self.b.assign(dest, Value::Use(op), span);
            return Ok(());
        }
        let PNode::Union(src_l) = self.node(src_ty, at)? else {
            return self.refuse_at(Construct::Malformed, at);
        };
        let PNode::Union(dst_l) = self.node(dst_ty, at)? else {
            return self.refuse_at(Construct::Malformed, at);
        };
        let mut src_members: Vec<PTy, A> = Vec::new_in(self.alloc);
        src_members.extend_from_slice(self.list(src_l, at)?);
        let mut dst_members: Vec<PTy, A> = Vec::new_in(self.alloc);
        dst_members.extend_from_slice(self.list(dst_l, at)?);
        let mut arms: Vec<(u32, u32, PTy), A> = Vec::new_in(self.alloc);
        for (i_idx, &sm) in src_members.iter().enumerate() {
            let Some(j_idx) = dst_members.iter().position(|&dm| dm == sm) else { continue };
            let Ok(i) = u32::try_from(i_idx) else { return self.refuse_at(Construct::Malformed, at) };
            let Ok(j) = u32::try_from(j_idx) else { return self.refuse_at(Construct::Malformed, at) };
            arms.push((i, j, sm));
        }
        if src_members.len() == 1 {
            let Some(&(i, j, sm)) = arms.first() else {
                self.b.terminate(StmtKind::Unreachable, span);
                self.dead_block();
                return Ok(());
            };
            let dp = self.b.project(src_place, Proj::Downcast(i));
            let fp = self.b.project(dp, Proj::Field { index: 0, ty: sm });
            let op = if self.is_copy(sm, at)? { Operand::Copy(fp) } else { Operand::Move(fp) };
            let operands = self.b.operands(&[op]);
            self.b.assign(dest, Value::Aggregate { kind: AggregateKind::Adt(j), ty: dst_ty, operands }, span);
            return Ok(());
        }
        let dty = self.find_ty(PNode::Int(IntTy::Isize), at)?;
        let discr = self.scratch(e, dty);
        self.b.assign(discr, Value::Discriminant(src_place), span);
        let join = self.b.new_block(BlockKind::Normal);
        let none = self.b.new_block(BlockKind::Normal);
        let mut cases_vec: Vec<(u128, blaze_body::Block), A> = Vec::new_in(self.alloc);
        let mut blocks_vec: Vec<(blaze_body::Block, u32, u32, PTy), A> = Vec::new_in(self.alloc);
        for &(i, j, sm) in arms.iter() {
            let blk = self.b.new_block(BlockKind::Normal);
            cases_vec.push((u128::from(i), blk));
            blocks_vec.push((blk, i, j, sm));
        }
        let cases = self.b.cases(&cases_vec);
        self.b.terminate(StmtKind::Switch { on: Operand::Move(discr), cases, otherwise: none }, span);
        for &(blk, i, j, sm) in blocks_vec.iter() {
            self.b.start(blk);
            let dp = self.b.project(src_place, Proj::Downcast(i));
            let fp = self.b.project(dp, Proj::Field { index: 0, ty: sm });
            let op = if self.is_copy(sm, at)? { Operand::Copy(fp) } else { Operand::Move(fp) };
            let operands = self.b.operands(&[op]);
            self.b.assign(dest, Value::Aggregate { kind: AggregateKind::Adt(j), ty: dst_ty, operands }, span);
            self.goto(join, span);
        }
        self.b.start(none);
        self.b.terminate(StmtKind::Unreachable, span);
        self.b.start(join);
        Ok(())
    }

    /// `*p` for `p` of type `ty`: a reference's or raw pointer's deref, or a `Box`'s.
    pub(crate) fn deref(&mut self, p: Place, ty: PTy, at: At) -> Result<Place, LowerError> {
        match self.node(ty, at)? {
            PNode::Ref(..) | PNode::Ptr(..) => Ok(self.b.project(p, Proj::Deref)),
            PNode::Adt(k, _) if self.adt_facts(k, at)?.is_box => Ok(self.b.project(p, Proj::DerefBox)),
            _ => self.refuse_at(Construct::OverloadedDeref, at),
        }
    }

    /// The type a reference, raw pointer or `Box` of type `ty` points to.
    pub(crate) fn pointee(&self, ty: PTy, at: At) -> Result<PTy, LowerError> {
        match self.node(ty, at)? {
            PNode::Ref(_, t) | PNode::Ptr(_, t) => Ok(t),
            PNode::Adt(k, l) if self.adt_facts(k, at)?.is_box => match self.list(l, at)?.first() {
                Some(&t) => Ok(t),
                None => self.refuse_at(Construct::Malformed, at),
            },
            _ => self.refuse_at(Construct::Malformed, at),
        }
    }

    pub(crate) fn is_place_expr(&self, mut e: ExprId) -> Result<bool, LowerError> {
        loop {
            match self.expr(e)? {
                Expr::Local(_) | Expr::Field { .. } | Expr::TupleField { .. } | Expr::Index { .. } => return Ok(true),
                Expr::Unary { op: UnOp::Deref, .. } => return Ok(true),
                Expr::Paren(x) => e = x,
                Expr::Path(_) => return Ok(matches!(self.expr_def(e), Some(Def::Static(..)))),
                _ => return Ok(false),
            }
        }
    }

    /// The place place expression `e` names, before `e`'s own adjustments.
    fn place_unadjusted(&mut self, e: ExprId) -> Result<Place, LowerError> {
        match self.expr(e)? {
            Expr::Local(p) => self.bound(p, e),
            Expr::Paren(x) => self.place(x),
            Expr::Field { base, .. } => {
                let Some(index) = self.field_of(e) else { return self.refuse(Construct::UnknownField, e) };
                self.field_place(base, index, e)
            }
            Expr::TupleField { base, index } => {
                let index = self.field_of(e).unwrap_or(index);
                self.field_place(base, index, e)
            }
            Expr::Index { base, index } => self.index_place(e, base, index),
            Expr::Unary { op: UnOp::Deref, operand } => {
                if self.callee_of(e).is_some() {
                    return self.refuse(Construct::OverloadedDeref, e);
                }
                let ty = self.adjusted_ty(operand)?;
                let p = self.place(operand)?;
                self.deref(p, ty, At::Expr(e))
            }
            Expr::Path(_) => self.static_place(e),
            _ => self.refuse(Construct::Malformed, e),
        }
    }

    /// `base.index`, of `e`'s type; a field of a type with a `Drop` impl is marked.
    fn field_place(&mut self, base: ExprId, index: u32, e: ExprId) -> Result<Place, LowerError> {
        let bt = self.adjusted_ty(base)?;
        let ty = self.expr_ty(e)?;
        let marked = match self.node(bt, At::Expr(base))? {
            PNode::Adt(k, _) => {
                let f = self.adt_facts(k, At::Expr(base))?;
                if f.union {
                    return self.refuse(Construct::Union, e);
                }
                f.drop_impl
            }
            _ => false,
        };
        let bp = self.place(base)?;
        Ok(self.b.project_marked(bp, Proj::Field { index, ty }, marked))
    }

    /// `base[index]` of an array or slice: the index in its own local, then `assert(index <
    /// len)` (the Reference, "Array and slice indexing expressions": out of bounds panics).
    fn index_place(&mut self, e: ExprId, base: ExprId, index: ExprId) -> Result<Place, LowerError> {
        if self.callee_of(e).is_some() {
            return self.refuse(Construct::OverloadedIndex, e);
        }
        let span = self.span(e);
        let bp = self.place(base)?;
        let it = self.adjusted_ty(index)?;
        let iv = self.operand(index)?;
        let (il, ip) = self.temp_local(index, it, Role::Value)?;
        self.b.assign(ip, Value::Use(iv), span);
        let len = self.scratch(e, it);
        self.b.assign(len, Value::Len(bp), span);
        let bool_ty = self.bool_ty(At::Expr(e))?;
        let lt = self.scratch(e, bool_ty);
        self.b.assign(lt, Value::Binary { op: BBin::Lt, lhs: Operand::Copy(ip), rhs: Operand::Copy(len) }, span);
        self.assert(Operand::Move(lt), true, AssertKind::Bounds, &[Operand::Copy(len), Operand::Copy(ip)], span);
        Ok(self.b.project(bp, Proj::Index(il)))
    }

    /// A static's place: `*(const static)`, the constant its address (`&T`, or `*mut T` for a
    /// `static mut`).
    fn static_place(&mut self, e: ExprId) -> Result<Place, LowerError> {
        let Some(Def::Static(k, m)) = self.expr_def(e) else { return self.refuse(Construct::Malformed, e) };
        let ty = self.expr_ty(e)?;
        let pty = match m {
            Mutbl::Not => self.find_ty(PNode::Ref(Mutbl::Not, ty), At::Expr(e))?,
            Mutbl::Mut => self.find_ty(PNode::Ptr(Mutbl::Mut, ty), At::Expr(e))?,
        };
        let c = self.b.new_const(ConstDecl { kind: ConstKind::StaticAddr(k), ty: pty });
        let t = self.scratch(e, pty);
        let span = self.span(e);
        self.b.assign(t, Value::Use(Operand::Const(c)), span);
        Ok(self.b.project(t, Proj::Deref))
    }

    /// `e` as a constant operand, when it is one: a literal, a fn item, a constant.
    fn const_operand(&mut self, e: ExprId) -> Result<Option<Operand>, LowerError> {
        match self.expr(e)? {
            Expr::Lit(l) => Ok(Some(self.lit(e, l)?)),
            Expr::Path(_) => self.path_const(e),
            Expr::ConstBlock { .. } => {
                let ty = self.expr_ty(e)?;
                Ok(Some(Operand::Const(self.b.new_const(ConstDecl { kind: ConstKind::Anon(e), ty }))))
            }
            _ => Ok(None),
        }
    }

    /// A literal's value, as blaze-build's walk reads it (walk.rs `lit`): the facts' answer.
    fn lit(&mut self, e: ExprId, l: Lit) -> Result<Operand, LowerError> {
        let ty = self.expr_ty(e)?;
        let facts: &'b F = self.facts;
        let bits = match l {
            Lit::Bool(b) => b as u128,
            Lit::Int(_) => match self.body.call_arg_ints.iter().find(|(x, _)| *x == e) {
                Some(&(_, v)) => v,
                None => match facts.int_lit(e) {
                    Some(v) => v,
                    None => return self.refuse(Construct::NoLiteralValue, e),
                },
            },
            Lit::Float(_) => match facts.float_lit(e) {
                Some(v) => v,
                None => return self.refuse(Construct::NoLiteralValue, e),
            },
            Lit::Char => match facts.char_lit(e) {
                Some(v) => v as u128,
                None => return self.refuse(Construct::NoLiteralValue, e),
            },
            Lit::Str | Lit::ByteStr(_) => {
                let Some(bytes) = facts.str_lit(e) else { return self.refuse(Construct::NoLiteralValue, e) };
                return Ok(Operand::Const(self.b.new_bytes(bytes, ty)));
            }
            Lit::CStr => return self.refuse(Construct::CStr, e),
        };
        Ok(self.scalar(bits, ty))
    }

    /// A path to a fn item (its type's `FnDef`), a constant, or an associated constant the
    /// check resolved (`Checked::callees`).
    fn path_const(&mut self, e: ExprId) -> Result<Option<Operand>, LowerError> {
        let ty = self.expr_ty(e)?;
        if let PNode::FnDef(key, args) = self.node(ty, At::Expr(e))? {
            return Ok(Some(Operand::Const(self.b.new_const(ConstDecl { kind: ConstKind::FnItem { key, args }, ty }))));
        }
        match self.expr_def(e) {
            Some(Def::Const(key)) => {
                let args = self.ck.const_args_of(e).unwrap_or(EMPTY);
                Ok(Some(Operand::Const(self.b.new_const(ConstDecl { kind: ConstKind::Item { key, args }, ty }))))
            }
            Some(Def::Static(..)) | Some(Def::Ctor { .. }) | Some(Def::Fn(_)) => Ok(None),
            _ => match self.callee_of(e) {
                Some((key, args)) => {
                    Ok(Some(Operand::Const(self.b.new_const(ConstDecl { kind: ConstKind::Item { key, args }, ty }))))
                }
                None => Ok(None),
            },
        }
    }

    /// A value expression's value written into `dest`.
    fn value_into(&mut self, dest: Place, e: ExprId) -> Result<(), LowerError> {
        let span = self.span(e);
        if let Some(c) = self.const_operand(e)? {
            self.b.assign(dest, Value::Use(c), span);
            return Ok(());
        }
        match self.expr(e)? {
            Expr::Path(_) => match self.expr_def(e) {
                // A unit struct or unit variant.
                Some(Def::Ctor { variant, .. }) => {
                    let ty = self.expr_ty(e)?;
                    let operands = self.b.operands(&[]);
                    self.b.assign(dest, Value::Aggregate { kind: AggregateKind::Adt(variant), ty, operands }, span);
                    Ok(())
                }
                _ => self.refuse(Construct::PathValue, e),
            },
            Expr::Paren(x) => self.lower_into(dest, x),
            Expr::Block { stmts, tail, .. } => self.block_into(dest, e, stmts, tail),
            Expr::If { cond, then, else_ } => self.if_into(dest, e, cond, then, else_),
            Expr::Match { scrutinee, arms } => self.match_into(dest, e, scrutinee, arms),
            Expr::Loop { body } => self.loop_into(dest, e, body),
            Expr::Break { value, target } => self.break_(e, value, target),
            Expr::Continue { target } => self.continue_(e, target),
            Expr::Return { value } => self.return_(e, value),
            Expr::Let { .. } => self.refuse(Construct::LetChain, e),
            Expr::Call { callee, args } => self.call_into(dest, e, callee, args),
            Expr::MethodCall { receiver, args, .. } => self.method_into(dest, e, receiver, args),
            Expr::Unary { op, operand } => self.unary_into(dest, e, op, operand),
            Expr::Binary { op, lhs, rhs } => self.binary_into(dest, e, op, lhs, rhs),
            Expr::Assign { lhs, rhs } => {
                self.assign_expr(e, lhs, rhs)?;
                let ty = self.expr_ty(e)?;
                self.unit(dest, ty, At::Expr(e), span)
            }
            Expr::AssignOp { op, lhs, rhs } => {
                self.assign_op(e, op, lhs, rhs)?;
                let ty = self.expr_ty(e)?;
                self.unit(dest, ty, At::Expr(e), span)
            }
            Expr::Ref { mutbl, operand } => self.ref_into(dest, e, mutbl, operand),
            Expr::RawRef { mutbl, operand } => {
                let p = self.place(operand)?;
                self.b.assign(dest, Value::RawAddr { mutbl, place: p }, span);
                Ok(())
            }
            Expr::Cast { operand, ty } => self.cast_into(dest, e, operand, ty),
            Expr::Tuple { elems } => self.aggregate_into(dest, e, AggregateKind::Tuple, elems),
            Expr::Array { elems } => self.aggregate_into(dest, e, AggregateKind::Array, elems),
            Expr::Repeat { elem, .. } => self.repeat_into(dest, e, elem),
            Expr::Struct { fields, base, .. } => self.struct_into(dest, e, fields, base),
            Expr::Record { fields, base } => self.record_into(dest, e, fields, base),
            Expr::Closure { .. } => self.closure_into(dest, e),
            Expr::FormatArgs { .. } => self.refuse(Construct::FormatArgs, e),
            Expr::Await(_) => self.refuse(Construct::Await, e),
            Expr::AsyncBlock { .. } => self.refuse(Construct::AsyncBlock, e),
            Expr::OffsetOf { .. } => self.refuse(Construct::OffsetOf, e),
            Expr::Asm { .. } => self.refuse(Construct::InlineAsm, e),
            Expr::Underscore => self.refuse(Construct::Underscore, e),
            _ => self.refuse(Construct::Malformed, e),
        }
    }

    // Calls.

    /// The callee of a fn item: a trait's method is resolved per instance.
    fn item_callee(&self, key: ItemKey, args: List, e: ExprId) -> Result<Callee, LowerError> {
        match self.facts.trait_item(key) {
            Some(true) => Ok(Callee::Method { key, args }),
            Some(false) => Ok(Callee::FnItem { key, args }),
            None => self.refuse(Construct::CalleeKind, e),
        }
    }

    fn operands_of(&mut self, ids: &[ExprId]) -> Result<Vec<Operand, A>, LowerError> {
        let mut ops = Vec::new_in(self.alloc);
        for &x in ids {
            ops.push(self.operand(x)?);
        }
        Ok(ops)
    }

    /// `callee(args..)`: a constructor is an aggregate; a fn item is called by name (its path
    /// not evaluated); a fn pointer is called through its value, evaluated first.
    fn call_into(&mut self, dest: Place, e: ExprId, callee: ExprId, args: List) -> Result<(), LowerError> {
        let ids = self.exprs_in(args, e)?;
        let span = self.span(e);
        if let Some(Def::Ctor { variant, .. }) = self.expr_def(callee) {
            let ty = self.expr_ty(e)?;
            let ops = self.operands_of(ids)?;
            let operands = self.b.operands(&ops);
            self.b.assign(dest, Value::Aggregate { kind: AggregateKind::Adt(variant), ty, operands }, span);
            return Ok(());
        }
        let cty = self.adjusted_ty(callee)?;
        let act = self.activate.len();
        let c = match self.node(cty, At::Expr(callee))? {
            PNode::FnDef(k, l) => {
                if !matches!(self.expr(callee)?, Expr::Path(_)) {
                    let _ = self.operand(callee)?;
                }
                self.item_callee(k, l, e)?
            }
            PNode::FnPtr(..) => Callee::Value(self.operand(callee)?),
            _ => return self.refuse(Construct::CallThroughFnTrait, e),
        };
        let ops = self.operands_of(ids)?;
        self.emit_call(c, &ops, dest, e, act)
    }

    /// `receiver.name(args..)`: the method the check chose, the adjusted receiver first.
    fn method_into(&mut self, dest: Place, e: ExprId, receiver: ExprId, args: List) -> Result<(), LowerError> {
        let Some((k, l)) = self.callee_of(e) else { return self.refuse(Construct::Malformed, e) };
        let c = self.item_callee(k, l, e)?;
        let act = self.activate.len();
        let ids = self.exprs_in(args, e)?;
        let mut ops = Vec::new_in(self.alloc);
        ops.push(self.operand(receiver)?);
        for &a in ids {
            ops.push(self.operand(a)?);
        }
        self.emit_call(c, &ops, dest, e, act)
    }

    /// The call row: the two-phase loans made since `act` activated first; its unwind edge the
    /// chain of what is live; a call of type `!` has no return block.
    pub(crate) fn emit_call(&mut self, c: Callee, ops: &[Operand], dest: Place, e: ExprId, act: usize) -> Result<(), LowerError> {
        let span = self.span(e);
        let mut i = act;
        while let Some(&loan) = self.activate.get(i) {
            self.b.push(StmtKind::Activate(loan), span);
            i += 1;
        }
        self.activate.truncate(act);
        let callee = self.b.callee(c);
        let args = self.b.operands(ops);
        let ty = self.expr_ty(e)?;
        let diverges = matches!(self.node(ty, At::Expr(e))?, PNode::Never);
        let unwind = self.unwind_here();
        if diverges {
            self.b.terminate(StmtKind::Call { callee, args, dest: None, target: None, unwind }, span);
            self.dead_block();
        } else {
            let target = self.b.new_block(BlockKind::Normal);
            self.b.terminate(StmtKind::Call { callee, args, dest: Some(dest), target: Some(target), unwind }, span);
            self.b.start(target);
        }
        Ok(())
    }

    /// `assert(cond == expected, kind(msg..))`, unwinding to the chain of what is live.
    pub(crate) fn assert(&mut self, cond: Operand, expected: bool, kind: AssertKind, msg: &[Operand], span: Span) {
        let msg = self.b.operands(msg);
        let target = self.b.new_block(BlockKind::Normal);
        let unwind = self.unwind_here();
        self.b.terminate(StmtKind::Assert { cond, expected, kind, msg, target, unwind }, span);
        self.b.start(target);
    }

    /// `&x` (or `&mut x`) of place expression `x`, in a new temporary: an overloaded operator's
    /// operand passed by reference.
    fn borrowed(&mut self, x: ExprId, m: Mutbl) -> Result<Operand, LowerError> {
        let ty = self.adjusted_ty(x)?;
        let p = self.place(x)?;
        let rt = self.find_ty(PNode::Ref(m, ty), At::Expr(x))?;
        let t = self.temp(x, rt, Role::Borrow)?;
        let kind = match m {
            Mutbl::Not => BorrowKind::Shared,
            Mutbl::Mut => BorrowKind::Mut,
        };
        let o = self.b.origin(false);
        let span = self.span(x);
        self.b.borrow(t, kind, p, o, span);
        Ok(match m {
            Mutbl::Not => Operand::Copy(t),
            Mutbl::Mut => Operand::Move(t),
        })
    }

    // Operators.

    fn int_bits(&self, n: PNode, at: At) -> Result<Option<(bool, u32)>, LowerError> {
        let ptr = || self.facts.pointer_bits();
        Ok(match n {
            PNode::Int(i) => Some((
                true,
                match i {
                    IntTy::I8 => 8,
                    IntTy::I16 => 16,
                    IntTy::I32 => 32,
                    IntTy::I64 => 64,
                    IntTy::I128 => 128,
                    IntTy::Isize => match ptr() {
                        Some(b) => b,
                        None => return self.refuse_at(Construct::PointerWidth, at),
                    },
                },
            )),
            PNode::Uint(u) => Some((
                false,
                match u {
                    UintTy::U8 => 8,
                    UintTy::U16 => 16,
                    UintTy::U32 => 32,
                    UintTy::U64 => 64,
                    UintTy::U128 => 128,
                    UintTy::Usize => match ptr() {
                        Some(b) => b,
                        None => return self.refuse_at(Construct::PointerWidth, at),
                    },
                },
            )),
            _ => None,
        })
    }

    fn bin(&self, op: BinOp, e: ExprId) -> Result<BBin, LowerError> {
        Ok(match op {
            BinOp::Add => BBin::Add,
            BinOp::Sub => BBin::Sub,
            BinOp::Mul => BBin::Mul,
            BinOp::Div => BBin::Div,
            BinOp::Rem => BBin::Rem,
            BinOp::BitAnd => BBin::BitAnd,
            BinOp::BitOr => BBin::BitOr,
            BinOp::BitXor => BBin::BitXor,
            BinOp::Shl => BBin::Shl,
            BinOp::Shr => BBin::Shr,
            BinOp::Eq => BBin::Eq,
            BinOp::Ne => BBin::Ne,
            BinOp::Lt => BBin::Lt,
            BinOp::Le => BBin::Le,
            BinOp::Gt => BBin::Gt,
            BinOp::Ge => BBin::Ge,
            BinOp::And | BinOp::Or => return self.refuse(Construct::Malformed, e),
        })
    }

    /// `lhs op rhs`: `&&` and `||` short-circuit; an overloaded operator is a call of the trait
    /// method the check chose (`Checked::callees` on `e`), a comparison taking its operands by
    /// shared reference in place; a builtin one is a row with its checks.
    fn binary_into(&mut self, dest: Place, e: ExprId, op: BinOp, lhs: ExprId, rhs: ExprId) -> Result<(), LowerError> {
        if matches!(op, BinOp::And | BinOp::Or) {
            return self.lazy_bool(dest, e, op, lhs, rhs);
        }
        if let Some((k, l)) = self.callee_of(e) {
            let c = self.item_callee(k, l, e)?;
            let act = self.activate.len();
            let ops = if comparison(op) {
                [self.borrowed(lhs, Mutbl::Not)?, self.borrowed(rhs, Mutbl::Not)?]
            } else {
                [self.operand(lhs)?, self.operand(rhs)?]
            };
            return self.emit_call(c, &ops, dest, e, act);
        }
        let lt = self.adjusted_ty(lhs)?;
        let rt = self.expr_ty(e)?;
        let l = self.operand(lhs)?;
        let r = self.operand(rhs)?;
        self.arith(dest, e, op, l, r, lt, rt)
    }

    /// `dest = l op r` of builtin operands of type `lt`, the result of type `rt`, with the
    /// checks the module's rule names.
    fn arith(&mut self, dest: Place, e: ExprId, op: BinOp, l: Operand, r: Operand, lt: PTy, rt: PTy) -> Result<(), LowerError> {
        let span = self.span(e);
        let at = At::Expr(e);
        let bop = self.bin(op, e)?;
        let n = self.node(lt, at)?;
        let int = self.int_bits(n, at)?;
        match (op, int) {
            (BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Shl | BinOp::Shr, Some(_)) => {
                let bool_ty = self.bool_ty(at)?;
                let pair = self.find_pair(rt, bool_ty, at)?;
                let t = self.scratch(e, pair);
                self.b.assign(t, Value::CheckedBinary { op: bop, lhs: l, rhs: r }, span);
                let flag = self.b.project(t, Proj::Field { index: 1, ty: bool_ty });
                self.assert(Operand::Copy(flag), false, AssertKind::Overflow(bop), &[l, r], span);
                let v = self.b.project(t, Proj::Field { index: 0, ty: rt });
                self.b.assign(dest, Value::Use(Operand::Copy(v)), span);
            }
            (BinOp::Div | BinOp::Rem, Some((signed, w))) => {
                let bool_ty = self.bool_ty(at)?;
                let zero = self.scalar(0, lt);
                let z = self.scratch(e, bool_ty);
                self.b.assign(z, Value::Binary { op: BBin::Eq, lhs: r, rhs: zero }, span);
                let kind = if op == BinOp::Div { AssertKind::DivisionByZero } else { AssertKind::RemainderByZero };
                self.assert(Operand::Move(z), false, kind, &[l], span);
                if signed {
                    let minus_one = self.scalar(mask(w), lt);
                    let min = self.scalar(1u128 << (w - 1), lt);
                    let a = self.scratch(e, bool_ty);
                    self.b.assign(a, Value::Binary { op: BBin::Eq, lhs: r, rhs: minus_one }, span);
                    let c = self.scratch(e, bool_ty);
                    self.b.assign(c, Value::Binary { op: BBin::Eq, lhs: l, rhs: min }, span);
                    let both = self.scratch(e, bool_ty);
                    self.b.assign(both, Value::Binary { op: BBin::BitAnd, lhs: Operand::Move(a), rhs: Operand::Move(c) }, span);
                    self.assert(Operand::Move(both), false, AssertKind::Overflow(bop), &[l, r], span);
                }
                self.b.assign(dest, Value::Binary { op: bop, lhs: l, rhs: r }, span);
            }
            _ => {
                self.b.assign(dest, Value::Binary { op: bop, lhs: l, rhs: r }, span);
            }
        }
        Ok(())
    }

    /// `lhs && rhs` and `lhs || rhs`: `rhs` runs only when `lhs` does not decide.
    fn lazy_bool(&mut self, dest: Place, e: ExprId, op: BinOp, lhs: ExprId, rhs: ExprId) -> Result<(), LowerError> {
        let span = self.span(e);
        let ty = self.expr_ty(e)?;
        let l = self.operand(lhs)?;
        let right = self.b.new_block(BlockKind::Normal);
        let short = self.b.new_block(BlockKind::Normal);
        let join = self.b.new_block(BlockKind::Normal);
        let and = op == BinOp::And;
        let cases = if and { self.b.cases(&[(0, short)]) } else { self.b.cases(&[(0, right)]) };
        let otherwise = if and { right } else { short };
        self.b.terminate(StmtKind::Switch { on: l, cases, otherwise }, span);
        self.b.start(short);
        let v = self.scalar(if and { 0 } else { 1 }, ty);
        self.b.assign(dest, Value::Use(v), span);
        self.goto(join, span);
        self.b.start(right);
        self.lower_into(dest, rhs)?;
        self.goto(join, span);
        self.b.start(join);
        Ok(())
    }

    /// `!x` and `-x`; an overloaded one is a call. A signed `-x` asserts `x != MIN`
    /// (`OverflowNeg`), except of a literal, which is folded.
    fn unary_into(&mut self, dest: Place, e: ExprId, op: UnOp, operand: ExprId) -> Result<(), LowerError> {
        let span = self.span(e);
        if op == UnOp::Deref {
            return self.refuse(Construct::Malformed, e);
        }
        if let Some((k, l)) = self.callee_of(e) {
            let c = self.item_callee(k, l, e)?;
            let act = self.activate.len();
            let o = self.operand(operand)?;
            return self.emit_call(c, &[o], dest, e, act);
        }
        if op == UnOp::Not {
            let o = self.operand(operand)?;
            self.b.assign(dest, Value::Unary { op: BUn::Not, operand: o }, span);
            return Ok(());
        }
        let ty = self.adjusted_ty(operand)?;
        let n = self.node(ty, At::Expr(e))?;
        let Some((true, w)) = self.int_bits(n, At::Expr(e))? else {
            let o = self.operand(operand)?;
            self.b.assign(dest, Value::Unary { op: BUn::Neg, operand: o }, span);
            return Ok(());
        };
        let mut inner = operand;
        while let Expr::Paren(x) = self.expr(inner)? {
            inner = x;
        }
        if let Expr::Lit(Lit::Int(_)) = self.expr(inner)? {
            let bits = self.lit_bits(inner)?;
            let v = self.scalar(bits.wrapping_neg() & mask(w), ty);
            self.b.assign(dest, Value::Use(v), span);
            return Ok(());
        }
        let o = self.operand(operand)?;
        let bool_ty = self.bool_ty(At::Expr(e))?;
        let min = self.scalar(1u128 << (w - 1), ty);
        let t = self.scratch(e, bool_ty);
        self.b.assign(t, Value::Binary { op: BBin::Eq, lhs: o, rhs: min }, span);
        self.assert(Operand::Move(t), false, AssertKind::OverflowNeg, &[o], span);
        self.b.assign(dest, Value::Unary { op: BUn::Neg, operand: o }, span);
        Ok(())
    }

    /// The value of integer literal `e`.
    fn lit_bits(&self, e: ExprId) -> Result<u128, LowerError> {
        match self.body.call_arg_ints.iter().find(|(x, _)| *x == e) {
            Some(&(_, v)) => Ok(v),
            None => match self.facts.int_lit(e) {
                Some(v) => Ok(v),
                None => self.refuse(Construct::NoLiteralValue, e),
            },
        }
    }

    // Assignment.

    /// `lhs = rhs`: the value first, then the place (the Reference, "Assignment expressions");
    /// the old value dropped (un-elaborated) before the write, and on that drop's unwind edge
    /// the new value written so the place still owns it. `_ = rhs` evaluates `rhs` as a place
    /// and reads it, moving nothing.
    fn assign_expr(&mut self, e: ExprId, lhs: ExprId, rhs: ExprId) -> Result<(), LowerError> {
        let span = self.span(e);
        if let Expr::Underscore = self.expr(lhs)? {
            let p = self.place(rhs)?;
            self.b.push(StmtKind::FakeRead(p), span);
            return Ok(());
        }
        if !self.is_place_expr(lhs)? {
            return self.refuse(Construct::DestructuringAssign, lhs);
        }
        let v = self.operand(rhs)?;
        let ty = self.adjusted_ty(lhs)?;
        let p = self.place(lhs)?;
        if self.needs_drop(ty, At::Expr(lhs))? {
            let chain = self.unwind_here();
            let after = self.unwind_block(chain);
            let r = self.b.new_block(BlockKind::Cleanup);
            self.pending.push(Pending::Assign { block: r, dest: p, value: v, next: after });
            let next = self.b.new_block(BlockKind::Normal);
            self.b.terminate(StmtKind::Drop { place: p, target: next, unwind: Unwind::Cleanup(r) }, span);
            self.b.start(next);
        }
        self.b.assign(p, Value::Use(v), span);
        Ok(())
    }

    /// `lhs op= rhs`: of primitives, the value then the place, and the operation's checks; an
    /// overloaded one, `&mut lhs` then the value, then the trait method's call.
    fn assign_op(&mut self, e: ExprId, op: BinOp, lhs: ExprId, rhs: ExprId) -> Result<(), LowerError> {
        if let Some((k, l)) = self.callee_of(e) {
            let c = self.item_callee(k, l, e)?;
            let act = self.activate.len();
            let a = self.borrowed(lhs, Mutbl::Mut)?;
            let v = self.operand(rhs)?;
            let ty = self.expr_ty(e)?;
            let out = self.scratch(e, ty);
            return self.emit_call(c, &[a, v], out, e, act);
        }
        let r = self.operand(rhs)?;
        let lt = self.adjusted_ty(lhs)?;
        let p = self.place(lhs)?;
        self.arith(p, e, op, Operand::Copy(p), r, lt, lt)
    }

    // Borrows, casts, aggregates.

    /// `&x` / `&mut x`: a loan of `x`'s place; a borrow the check promoted
    /// (`Checked::promoted`) is the promoted constant, and its operand's code does not run here.
    fn ref_into(&mut self, dest: Place, e: ExprId, mutbl: Mutbl, operand: ExprId) -> Result<(), LowerError> {
        let span = self.span(e);
        if mutbl == Mutbl::Not
            && let Ok(i) = self.ck.promoted.binary_search_by_key(&e.0, |x| x.0)
        {
            let ty = self.expr_ty(e)?;
            let c = self.b.new_const(ConstDecl { kind: ConstKind::Promoted(i as u32), ty });
            self.b.assign(dest, Value::Use(Operand::Const(c)), span);
            return Ok(());
        }
        let p = self.place(operand)?;
        let kind = match mutbl {
            Mutbl::Not => BorrowKind::Shared,
            Mutbl::Mut => BorrowKind::Mut,
        };
        let o = self.b.origin(false);
        self.b.borrow(dest, kind, p, o, span);
        Ok(())
    }

    /// `x as T`, by the kinds of the two types (the Reference, "Type cast expressions"). A
    /// reference cast to a raw pointer is the raw address of its pointee.
    fn cast_into(&mut self, dest: Place, e: ExprId, operand: ExprId, _written: PTy) -> Result<(), LowerError> {
        let span = self.span(e);
        let at = At::Expr(e);
        let from = self.adjusted_ty(operand)?;
        // The checked type, not the written one (which may hold `_`).
        let to = self.expr_ty(e)?;
        let fnode = self.node(from, at)?;
        let tnode = self.node(to, at)?;
        if let (PNode::Ref(_, _), PNode::Ptr(m, _)) = (fnode, tnode) {
            let p = self.place(operand)?;
            let d = self.b.project(p, Proj::Deref);
            self.b.assign(dest, Value::RawAddr { mutbl: m, place: d }, span);
            return Ok(());
        }
        let intish = |n: PNode| matches!(n, PNode::Int(_) | PNode::Uint(_));
        let kind = match (fnode, tnode) {
            (PNode::Int(_) | PNode::Uint(_) | PNode::Bool | PNode::Char, PNode::Int(_) | PNode::Uint(_)) => CastKind::IntToInt,
            (PNode::Uint(UintTy::U8), PNode::Char) => CastKind::IntToInt,
            (f, PNode::Float(_)) if intish(f) => CastKind::IntToFloat,
            (PNode::Float(_), t) if intish(t) => CastKind::FloatToInt,
            (PNode::Float(_), PNode::Float(_)) => CastKind::FloatToFloat,
            (PNode::Ptr(..), PNode::Ptr(..)) => CastKind::PtrToPtr,
            (PNode::Ptr(..), t) if intish(t) => CastKind::ExposeAddr,
            (f, PNode::Ptr(..)) if intish(f) => CastKind::FromExposedAddr,
            (PNode::FnPtr(..), PNode::Ptr(..)) => CastKind::FnPtrToPtr,
            (PNode::FnDef(..), PNode::FnPtr(..)) => CastKind::ReifyFnPointer,
            _ => return self.refuse(Construct::Cast, e),
        };
        let o = self.operand(operand)?;
        self.b.assign(dest, Value::Cast { kind, operand: o, ty: to }, span);
        Ok(())
    }

    fn aggregate_into(&mut self, dest: Place, e: ExprId, kind: AggregateKind, elems: List) -> Result<(), LowerError> {
        let span = self.span(e);
        let ty = self.expr_ty(e)?;
        let ids = self.exprs_in(elems, e)?;
        let ops = self.operands_of(ids)?;
        let operands = self.b.operands(&ops);
        self.b.assign(dest, Value::Aggregate { kind, ty, operands }, span);
        Ok(())
    }

    /// `[x; N]`: the count a constant (its value, or the const parameter it names), read from
    /// the checked array type of `e`; the written `len` is a handle of the view's own type
    /// table, not of `Checked::types`.
    fn repeat_into(&mut self, dest: Place, e: ExprId, elem: ExprId) -> Result<(), LowerError> {
        let span = self.span(e);
        let at = At::Expr(e);
        let usize_ty = self.find_ty(PNode::Uint(UintTy::Usize), at)?;
        let ty = self.expr_ty(e)?;
        let len = match self.node(ty, at)? {
            PNode::Array(_, n) => n,
            _ => return self.refuse(Construct::Malformed, e),
        };
        let kind = match self.tys.value_u64(len) {
            Some(n) => ConstKind::Scalar { bits: n as u128 },
            None => match self.node(len, at)? {
                PNode::ConstParam(i) => ConstKind::Param(i),
                _ => return self.refuse(Construct::Malformed, e),
            },
        };
        let count = self.b.new_const(ConstDecl { kind, ty: usize_ty });
        let o = self.operand(elem)?;
        self.b.assign(dest, Value::Repeat { operand: o, count }, span);
        Ok(())
    }

    /// `S { f: x, .. }`: the fields' values in written order, then the base's remaining fields
    /// (`Checked::fru_fields`) moved or copied out of its place, as one aggregate.
    /// `S { f: x, .. }` with no base (RFC 3681): each unwritten field is its default
    /// (`Checked::default_fields`), the constant item at the struct type's arguments (the
    /// default's generics are the ADT's), as `S { f: x, g: DEFAULT_G }` would be.
    fn struct_into(&mut self, dest: Place, e: ExprId, fields: List, base: blaze_typeck::StructBase) -> Result<(), LowerError> {
        let span = self.span(e);
        let at = At::Expr(e);
        let ty = self.expr_ty(e)?;
        let (adt, variant) = match self.expr_def(e) {
            Some(Def::Ctor { adt, variant, .. }) => (adt, variant),
            Some(Def::Adt(k)) => (k, 0),
            _ => match self.node(ty, at)? {
                PNode::Adt(k, _) => (k, 0),
                _ => return self.refuse(Construct::Malformed, e),
            },
        };
        if self.adt_facts(adt, at)?.union {
            return self.refuse(Construct::Union, e);
        }
        let Some(n) = self.facts.field_count(adt, variant) else { return self.refuse(Construct::UnknownField, e) };
        let mut slots: Vec<Option<Operand>, A> = Vec::new_in(self.alloc);
        for _ in 0..n {
            slots.push(None);
        }
        for fi in self.field_inits_in(fields, e)? {
            let Some(i) = self.facts.field(adt, variant, fi.name) else { return self.refuse(Construct::UnknownField, fi.expr) };
            let o = self.operand(fi.expr)?;
            match slots.get_mut(i as usize) {
                Some(s) => *s = Some(o),
                None => return self.refuse(Construct::UnknownField, fi.expr),
            }
        }
        if base == blaze_typeck::StructBase::Defaults {
            let PNode::Adt(_, args) = self.node(ty, at)? else { return self.refuse(Construct::Malformed, e) };
            let ck: &'b blaze_typeck::Checked<'c> = self.ck;
            let (idx, keys) = ck.default_fields_of(e);
            let (_, tys) = ck.fru_fields_of(e);
            for (k, (&i, &key)) in idx.iter().zip(keys.iter()).enumerate() {
                let Some(&fty) = tys.get(k) else { return self.refuse(Construct::Malformed, e) };
                let o = Operand::Const(self.b.new_const(ConstDecl { kind: ConstKind::Item { key, args }, ty: fty }));
                match slots.get_mut(i as usize) {
                    Some(s) => *s = Some(o),
                    None => return self.refuse(Construct::Malformed, e),
                }
            }
        }
        if let Some(b) = base.expr() {
            let bp = self.place(b)?;
            let ck: &'b blaze_typeck::Checked<'c> = self.ck;
            let (idx, tys) = ck.fru_fields_of(e);
            for (k, &i) in idx.iter().enumerate() {
                let Some(&fty) = tys.get(k) else { return self.refuse(Construct::Malformed, e) };
                let fp = self.b.project(bp, Proj::Field { index: i, ty: fty });
                let o = if self.is_copy(fty, at)? { Operand::Copy(fp) } else { Operand::Move(fp) };
                match slots.get_mut(i as usize) {
                    Some(s) => *s = Some(o),
                    None => return self.refuse(Construct::Malformed, e),
                }
            }
        }
        let mut ops: Vec<Operand, A> = Vec::new_in(self.alloc);
        for s in slots.iter() {
            match s {
                Some(o) => ops.push(*o),
                None => return self.refuse(Construct::UnknownField, e),
            }
        }
        let operands = self.b.operands(&ops);
        self.b.assign(dest, Value::Aggregate { kind: AggregateKind::Adt(variant), ty, operands }, span);
        Ok(())
    }

    /// `{ f: x, .. }`: structural record literal / functional update / defaults, lowered as
    /// `AggregateKind::Tuple` with operands in canonical sorted field order.
    fn record_into(&mut self, dest: Place, e: ExprId, fields: List, base: blaze_typeck::StructBase) -> Result<(), LowerError> {
        let span = self.span(e);
        let at = At::Expr(e);
        let ty = self.expr_ty(e)?;
        let PNode::Record(schema, flist) = self.node(ty, at)? else {
            return self.refuse(Construct::Malformed, e);
        };
        let mut slots: Vec<Option<Operand>, A> = Vec::new_in(self.alloc);
        for _ in 0..flist.len {
            slots.push(None);
        }
        for fi in self.field_inits_in(fields, e)? {
            let Some(i) = self.tys.record_field_index(schema, fi.name) else {
                return self.refuse(Construct::UnknownField, fi.expr);
            };
            let o = self.operand(fi.expr)?;
            let Ok(ix) = usize::try_from(i) else { return self.refuse(Construct::UnknownField, fi.expr) };
            match slots.get_mut(ix) {
                Some(s) => *s = Some(o),
                None => return self.refuse(Construct::UnknownField, fi.expr),
            }
        }
        if matches!(base, blaze_typeck::StructBase::Defaults | blaze_typeck::StructBase::None) {
            let ck: &'b blaze_typeck::Checked<'c> = self.ck;
            let (idx, keys) = ck.default_fields_of(e);
            let (_, tys) = ck.fru_fields_of(e);
            for (k, (&i, &key)) in idx.iter().zip(keys.iter()).enumerate() {
                let Some(&fty) = tys.get(k) else { return self.refuse(Construct::Malformed, e) };
                let o = Operand::Const(self.b.new_const(ConstDecl { kind: ConstKind::Item { key, args: EMPTY }, ty: fty }));
                let Ok(ix) = usize::try_from(i) else { return self.refuse(Construct::Malformed, e) };
                match slots.get_mut(ix) {
                    Some(s) => *s = Some(o),
                    None => return self.refuse(Construct::Malformed, e),
                }
            }
        }
        if let Some(b) = base.expr() {
            let bp = self.place(b)?;
            let ck: &'b blaze_typeck::Checked<'c> = self.ck;
            let (idx, tys) = ck.fru_fields_of(e);
            for (k, &i) in idx.iter().enumerate() {
                let Some(&fty) = tys.get(k) else { return self.refuse(Construct::Malformed, e) };
                let fp = self.b.project(bp, Proj::Field { index: i, ty: fty });
                let o = if self.is_copy(fty, at)? { Operand::Copy(fp) } else { Operand::Move(fp) };
                let Ok(ix) = usize::try_from(i) else { return self.refuse(Construct::Malformed, e) };
                match slots.get_mut(ix) {
                    Some(s) => *s = Some(o),
                    None => return self.refuse(Construct::Malformed, e),
                }
            }
        }
        let mut ops: Vec<Operand, A> = Vec::new_in(self.alloc);
        for s in slots.iter() {
            match s {
                Some(o) => ops.push(*o),
                None => return self.refuse(Construct::UnknownField, e),
            }
        }
        let operands = self.b.operands(&ops);
        self.b.assign(dest, Value::Aggregate { kind: AggregateKind::Tuple, ty, operands }, span);
        Ok(())
    }
}
