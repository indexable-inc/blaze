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
    AggregateKind, AsmArch as BodyAsmArch, AsmOpInput, AsmOpKind, AsmPieceInput, AsmRegKind,
    AssertKind, BinOp as BBin, Block, BlockKind, BorrowKind, Callee, CastKind, ConstDecl,
    ConstKind, Local, LocalDecl, LocalKind, Operand, Place, Proj, Role, Span, StmtKind,
    UnOp as BUn, Unwind, Value,
};
use blaze_own::Vec;
use blaze_typeck::{
    AdjustKind, Arm, AsmArch as TcAsmArch, AsmKind as TcAsmKind, AsmPiece as TcAsmPiece,
    AsmReg as TcAsmReg, BinOp, Def, Expr, ExprId, FieldInit, FieldPat, IntTy, ItemKey, List, Lit,
    Mutbl, PNode, PTy, PatId, Stmt, StmtId, UintTy, UnOp,
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
        let cty = self.canonical_ty(ty, At::Expr(e))?;
        let local = self.b.new_local(LocalDecl { ty: cty, kind: LocalKind::Temp(e, role), mutbl: Mutbl::Mut });
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
        let cty = self.canonical_ty(ty, At::Expr(e)).unwrap_or(ty);
        let local = self.b.new_local(LocalDecl { ty: cty, kind: LocalKind::Temp(e, Role::Value), mutbl: Mutbl::Mut });
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

    /// Whether the checker promoted borrow or autoref `e` (`Checked::promoted`, sorted).
    pub(crate) fn is_promoted(&self, e: ExprId) -> bool {
        !self.facts.constant_initializer() && self.ck.promoted.binary_search_by_key(&e.0, |x| x.0).is_ok()
    }

    /// Whether `e` is a promoted borrow inside a `const`/`static` initializer whose temporary
    /// gets `'static` lifetime (stays alive until root return interns it into `Origin::Data`).
    fn is_const_promoted(&self, e: ExprId) -> bool {
        self.facts.constant_initializer() && self.ck.promoted.binary_search_by_key(&e.0, |x| x.0).is_ok()
    }

    fn val_inner(&mut self, e: ExprId) -> Result<Val, LowerError> {
        let (lo, hi) = self.adjust_rows(e);
        // A shared autoref promoted to `'static` (`Checked::promoted`): the constant's address,
        // never a frame temporary; any coercions after it (e.g. `Unsize`) run from `lo + 1`.
        if hi > lo
            && !matches!(self.expr(e)?, Expr::Ref { .. })
            && let Some((AdjustKind::Borrow { mutbl: Mutbl::Not, raw: false, .. }, to)) = self.adjust(lo)
            && self.is_promoted(e)
        {
            let c = self.b.new_const(ConstDecl { kind: ConstKind::Promoted(e.0), ty: to });
            let mut v = Val::Op(Operand::Const(c), to);
            for i in (lo + 1)..hi {
                let Some((k, next_to)) = self.adjust(i) else {
                    return self.refuse(Construct::Malformed, e);
                };
                v = self.apply(v, k, next_to, e)?;
            }
            return Ok(v);
        }
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

    /// Lowers `e` applying only adjustments `lo..end` (used by array-to-slice overloaded index).
    fn val_prefix(&mut self, e: ExprId, lo: usize, end: usize) -> Result<Val, LowerError> {
        let ty = self.expr_ty(e)?;
        let mut v = if self.is_place_expr(e)? {
            Val::Place(self.place_unadjusted(e)?, ty)
        } else if let Some(c) = self.const_operand(e)? {
            Val::Op(c, ty)
        } else {
            let t = self.temp(e, ty, Role::Value)?;
            self.value_into(t, e)?;
            Val::Place(t, ty)
        };
        for i in lo..end {
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
            AdjustKind::Deref { overloaded: Some(mutbl) } => {
                let self_ty = match v {
                    Val::Place(_, t) | Val::Op(_, t) => t,
                };
                let at = At::Expr(e);
                let p = self.to_place(v, e)?;
                // `Box<T>` deref is builtin (`Proj::DerefBox`), never a recursive call to
                // `alloc`'s `Deref::deref` (`&**self`).
                if let PNode::Adt(k, _) = self.node(self_ty, at)?
                    && self.adt_facts(k, at)?.is_box
                {
                    let d = self.b.project(p, Proj::DerefBox);
                    return Ok(Val::Place(d, to));
                }
                let Some(key) = self.facts.deref_fn(mutbl) else {
                    return self.refuse(Construct::OverloadedDeref, e);
                };
                let arg_ty = self.find_ty(PNode::Ref(mutbl, self_ty), at)?;
                let arg_temp = self.temp(e, arg_ty, Role::Borrow)?;
                let bkind = match mutbl {
                    Mutbl::Not => BorrowKind::Shared,
                    Mutbl::Mut => BorrowKind::Mut,
                };
                let o = self.b.origin(false);
                self.b.borrow(arg_temp, bkind, p, o, span);
                let arg_op = match mutbl {
                    Mutbl::Not => Operand::Copy(arg_temp),
                    Mutbl::Mut => Operand::Move(arg_temp),
                };
                let Ok(args_list) = self.b.intern_list(&[self_ty]) else {
                    return self.refuse(Construct::Malformed, e);
                };
                let ret_ty = self.find_ty(PNode::Ref(mutbl, to), at)?;
                let ret_temp = self.temp(e, ret_ty, Role::Value)?;
                let act = self.activate.len();
                self.emit_call_ty(Callee::Method { key, args: args_list }, &[arg_op], ret_temp, ret_ty, e, act)?;
                let d = self.b.project(ret_temp, Proj::Deref);
                Ok(Val::Place(d, to))
            }
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

    /// Whether `operand` carries an overloaded dereference as its last adjustment (the checker
    /// records written `*x` on `x` itself for `Deref`/`DerefMut`/`Box`).
    fn deref_on_operand(&self, operand: ExprId) -> bool {
        let (lo, hi) = self.adjust_rows(operand);
        hi > lo && matches!(self.adjust(hi - 1), Some((AdjustKind::Deref { overloaded: Some(_) }, _)))
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
                if self.deref_on_operand(operand) {
                    return self.place(operand);
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
            PNode::Adt(k, _) => self.adt_facts(k, At::Expr(base))?.drop_impl,
            _ => false,
        };
        let bp = self.place(base)?;
        Ok(self.b.project_marked(bp, Proj::Field { index, ty }, marked))
    }

    /// `base[index]`: builtin array/slice element (`usize` index with bounds check), or
    /// overloaded `*Index::index(&base, index)` / `*IndexMut::index_mut(&mut base, index)`.
    fn index_place(&mut self, e: ExprId, base: ExprId, index: ExprId) -> Result<Place, LowerError> {
        let span = self.span(e);
        let at = At::Expr(e);
        let bt = self.adjusted_ty(base)?;
        let it = self.adjusted_ty(index)?;
        let builtin = matches!(self.node(bt, At::Expr(base))?, PNode::Array(..) | PNode::Slice(_))
            && matches!(self.node(it, At::Expr(index))?, PNode::Uint(UintTy::Usize));
        if builtin {
            let bp = self.place(base)?;
            let iv = self.operand(index)?;
            let (il, ip) = self.temp_local(index, it, Role::Value)?;
            self.b.assign(ip, Value::Use(iv), span);
            let len = self.scratch(e, it);
            self.b.assign(len, Value::Len(bp), span);
            let bool_ty = self.bool_ty(at)?;
            let lt = self.scratch(e, bool_ty);
            self.b.assign(lt, Value::Binary { op: BBin::Lt, lhs: Operand::Copy(ip), rhs: Operand::Copy(len) }, span);
            self.assert(Operand::Move(lt), true, AssertKind::Bounds, &[Operand::Copy(len), Operand::Copy(ip)], span);
            return Ok(self.b.project(bp, Proj::Index(il)));
        }
        let mutbl = if self.ck.index_is_mut(e) { Mutbl::Mut } else { Mutbl::Not };
        let Some(key) = self.facts.index_fn(mutbl) else {
            return self.refuse(Construct::OverloadedIndex, e);
        };
        let bkind = match mutbl {
            Mutbl::Not => BorrowKind::Shared,
            Mutbl::Mut => BorrowKind::Mut,
        };
        // Check for array-to-slice step on `base` (`a[1..3]`): checker records a builtin `Deref`
        // from `[T; N]` to `[T]`; lower it as borrowing `[T; N]` and unsizing to `&[T]`/`&mut [T]`.
        let (blo, bhi) = self.adjust_rows(base);
        let array_to_slice = if bhi > blo
            && matches!(self.adjust(bhi - 1), Some((AdjustKind::Deref { overloaded: None }, _)))
        {
            let before = if bhi - 1 == blo {
                self.expr_ty(base)?
            } else {
                let Some((_, prev)) = self.adjust(bhi - 2) else {
                    return self.refuse(Construct::Malformed, base);
                };
                prev
            };
            if matches!(self.node(before, At::Expr(base))?, PNode::Array(..)) { Some(before) } else { None }
        } else {
            None
        };
        let ref_ty = self.find_ty(PNode::Ref(mutbl, bt), at)?;
        let base_op = if let Some(arr_ty) = array_to_slice {
            let v = self.val_prefix(base, blo, bhi - 1)?;
            let ap = self.to_place(v, base)?;
            let thin_ty = self.find_ty(PNode::Ref(mutbl, arr_ty), at)?;
            let thin_t = self.temp(base, thin_ty, Role::Borrow)?;
            let o = self.b.origin(false);
            self.b.borrow(thin_t, bkind, ap, o, span);
            let thin_op = match mutbl {
                Mutbl::Not => Operand::Copy(thin_t),
                Mutbl::Mut => Operand::Move(thin_t),
            };
            let wide_t = self.temp(base, ref_ty, Role::Value)?;
            self.b.assign(wide_t, Value::Cast { kind: CastKind::Unsize, operand: thin_op, ty: ref_ty }, span);
            match mutbl {
                Mutbl::Not => Operand::Copy(wide_t),
                Mutbl::Mut => Operand::Move(wide_t),
            }
        } else {
            let bp = self.place(base)?;
            let rt = self.temp(base, ref_ty, Role::Borrow)?;
            let o = self.b.origin(false);
            self.b.borrow(rt, bkind, bp, o, span);
            match mutbl {
                Mutbl::Not => Operand::Copy(rt),
                Mutbl::Mut => Operand::Move(rt),
            }
        };
        let idx_op = self.operand(index)?;
        let Ok(args_list) = self.b.intern_list(&[bt, it]) else {
            return self.refuse(Construct::Malformed, e);
        };
        let out_ty = self.expr_ty(e)?;
        let ret_ty = self.find_ty(PNode::Ref(mutbl, out_ty), at)?;
        let ret_temp = self.temp(e, ret_ty, Role::Value)?;
        let act = self.activate.len();
        self.emit_call_ty(Callee::Method { key, args: args_list }, &[base_op, idx_op], ret_temp, ret_ty, e, act)?;
        Ok(self.b.project(ret_temp, Proj::Deref))
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

    /// `e` as a constant operand, when it is one: a literal, a fn item, a constant, or a promoted
    /// shared borrow.
    fn const_operand(&mut self, e: ExprId) -> Result<Option<Operand>, LowerError> {
        match self.expr(e)? {
            Expr::Lit(l) => Ok(Some(self.lit(e, l)?)),
            Expr::Path(_) => self.path_const(e),
            Expr::ConstBlock { .. } => {
                let ty = self.expr_ty(e)?;
                Ok(Some(Operand::Const(self.b.new_const(ConstDecl { kind: ConstKind::Anon(e), ty }))))
            }
            Expr::Ref { mutbl: Mutbl::Not, .. } if self.is_promoted(e) => {
                let ty = self.expr_ty(e)?;
                Ok(Some(Operand::Const(self.b.new_const(ConstDecl { kind: ConstKind::Promoted(e.0), ty }))))
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
            Lit::CStr => {
                let Some(bytes) = facts.str_lit(e) else { return self.refuse(Construct::NoLiteralValue, e) };
                let mut nul = Vec::with_capacity_in(bytes.len() + 1, self.alloc);
                nul.extend_from_slice(bytes);
                nul.push(0);
                return Ok(Operand::Const(self.b.new_bytes(&nul, ty)));
            }
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
            Some(Def::TyParam(idx)) => {
                Ok(Some(Operand::Const(self.b.new_const(ConstDecl { kind: ConstKind::Param(idx), ty }))))
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
            Expr::Become { value } => self.become_(e, value),
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
            Expr::Await(operand) => self.await_into(dest, e, operand),
            Expr::AsyncBlock { .. } => self.coroutine_into(dest, e),
            Expr::OffsetOf { .. } => self.refuse(Construct::OffsetOf, e),
            Expr::Asm { arch, pieces, operands, options, clobber_abis } => {
                self.asm_into(dest, e, arch, pieces, operands, options, clobber_abis)
            }
            Expr::Underscore => self.refuse(Construct::Underscore, e),
            _ => self.refuse(Construct::Malformed, e),
        }
    }

    // Calls.

    /// The callee of a fn item: a trait's method is resolved per instance.
    pub(crate) fn item_callee(&mut self, key: ItemKey, args: List, e: ExprId) -> Result<Callee, LowerError> {
        let args = self.canonical_list(args, At::Expr(e))?;
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

    /// Resolves the `ClosureKind` of closure `(owner, ordinal)`: from `Checked::closures` when
    /// defined in this body, else from `BodyFacts::closure_kind`.
    fn resolve_closure_kind(&self, owner: ItemKey, ordinal: u32) -> Option<blaze_typeck::ClosureKind> {
        if owner == self.body.owner {
            let mut seen = 0u32;
            for (idx, x) in self.body.exprs.iter().enumerate() {
                if matches!(x, Expr::Closure { .. }) {
                    if seen == ordinal {
                        let cid = ExprId(u32::try_from(idx).ok()?);
                        let row = self.ck.closures.column::<0>().iter().position(|&c| c == cid)?;
                        return self.ck.closures.column::<1>().get(row).copied();
                    }
                    seen += 1;
                }
            }
        }
        self.facts.closure_kind(owner, ordinal)
    }

    /// Packs call argument operands `arg_ops` into a tuple temporary of type `tuple_ty`.
    fn pack_args_tuple(&mut self, arg_ops: &[Operand], tuple_ty: PTy, e: ExprId) -> Result<Operand, LowerError> {
        let span = self.span(e);
        let t = self.temp(e, tuple_ty, Role::Value)?;
        let operands = self.b.operands(arg_ops);
        self.b.assign(t, Value::Aggregate { kind: AggregateKind::Tuple, ty: tuple_ty, operands }, span);
        if self.is_copy(tuple_ty, At::Expr(e))? {
            Ok(Operand::Copy(t))
        } else {
            Ok(Operand::Move(t))
        }
    }

    /// `callee(args..)`: a constructor is an aggregate; a fn item is called by name (its path
    /// not evaluated); a fn pointer is called through its value, evaluated first; closures and
    /// `Fn`/`FnMut`/`FnOnce`/`dyn Fn` values are called through their `Fn*` trait method with
    /// arguments packed into a tuple (spread by `Abi::RustCall`).
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
        match self.node(cty, At::Expr(callee))? {
            PNode::FnDef(k, l) => {
                if !matches!(self.expr(callee)?, Expr::Path(_)) {
                    let _ = self.operand(callee)?;
                }
                let c = self.item_callee(k, l, e)?;
                let ops = self.operands_of(ids)?;
                self.emit_call(c, &ops, dest, e, act)
            }
            PNode::FnPtr(..) => {
                let c = Callee::Value(self.operand(callee)?);
                let ops = self.operands_of(ids)?;
                self.emit_call(c, &ops, dest, e, act)
            }
            PNode::Closure { owner, ordinal, .. } if self.callee_of(e).is_none() => {
                let Some(kind) = self.resolve_closure_kind(owner, ordinal) else {
                    return self.refuse(Construct::CallThroughFnTrait, e);
                };
                let Some(key) = self.facts.fn_trait_method(kind) else {
                    return self.refuse(Construct::CallThroughFnTrait, e);
                };
                let recv_op = match kind {
                    blaze_typeck::ClosureKind::Fn => self.borrowed(callee, Mutbl::Not)?,
                    blaze_typeck::ClosureKind::FnMut => self.borrowed(callee, Mutbl::Mut)?,
                    blaze_typeck::ClosureKind::FnOnce => self.operand(callee)?,
                };
                let mut arg_tys: Vec<PTy, A> = Vec::with_capacity_in(ids.len(), self.alloc);
                for &a in ids {
                    arg_tys.push(self.adjusted_ty(a)?);
                }
                let arg_ops = self.operands_of(ids)?;
                let Ok(tuple_list) = self.b.intern_list(&arg_tys) else {
                    return self.refuse(Construct::Malformed, e);
                };
                let tuple_ty = self.find_ty(PNode::Tuple(tuple_list), At::Expr(e))?;
                let tuple_op = self.pack_args_tuple(&arg_ops, tuple_ty, e)?;
                let Ok(fn_args) = self.b.intern_list(&[cty, tuple_ty]) else {
                    return self.refuse(Construct::Malformed, e);
                };
                self.emit_call(Callee::Method { key, args: fn_args }, &[recv_op, tuple_op], dest, e, act)
            }
            _ => {
                let Some((key, fn_args)) = self.callee_of(e) else {
                    return self.refuse(Construct::CallThroughFnTrait, e);
                };
                let callee_op = self.operand(callee)?;
                let arg_ops = self.operands_of(ids)?;
                let tuple_ty = match self.list(fn_args, At::Expr(e))?.get(1) {
                    Some(&t) => t,
                    None => {
                        let mut arg_tys: Vec<PTy, A> = Vec::with_capacity_in(ids.len(), self.alloc);
                        for &a in ids {
                            arg_tys.push(self.adjusted_ty(a)?);
                        }
                        let Ok(tuple_list) = self.b.intern_list(&arg_tys) else {
                            return self.refuse(Construct::Malformed, e);
                        };
                        self.find_ty(PNode::Tuple(tuple_list), At::Expr(e))?
                    }
                };
                let tuple_op = self.pack_args_tuple(&arg_ops, tuple_ty, e)?;
                self.emit_call(Callee::Method { key, args: fn_args }, &[callee_op, tuple_op], dest, e, act)
            }
        }
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
        let ty = self.expr_ty(e)?;
        self.emit_call_ty(c, ops, dest, ty, e, act)
    }

    /// Emits a call whose return type is `ret_ty` (distinct from `self.expr_ty(e)` for helper
    /// trait calls like `Deref::deref` and `Index::index`).
    pub(crate) fn emit_call_ty(
        &mut self,
        c: Callee,
        ops: &[Operand],
        dest: Place,
        ret_ty: PTy,
        e: ExprId,
        act: usize,
    ) -> Result<(), LowerError> {
        let span = self.span(e);
        let mut i = act;
        while let Some(&loan) = self.activate.get(i) {
            self.b.push(StmtKind::Activate(loan), span);
            i += 1;
        }
        self.activate.truncate(act);
        let callee = self.b.callee(c);
        let args = self.b.operands(ops);
        let diverges = matches!(self.node(ret_ty, At::Expr(e))?, PNode::Never);
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
    /// and reads it, moving nothing. Destructuring assignment (`(a, b) = rhs`, `[a, b] = rhs`,
    /// `S { x, y } = rhs`, `S(a, b) = rhs`) evaluates `rhs` into a temporary first, then assigns
    /// each sub-place in left-to-right order.
    fn assign_expr(&mut self, e: ExprId, lhs: ExprId, rhs: ExprId) -> Result<(), LowerError> {
        let span = self.span(e);
        if let Expr::Underscore = self.expr(lhs)? {
            let p = self.place(rhs)?;
            self.b.push(StmtKind::FakeRead(p), span);
            return Ok(());
        }
        if !self.is_place_expr(lhs)? {
            let rty = self.adjusted_ty(rhs)?;
            let src = self.place(rhs)?;
            return self.destructure_assign(lhs, src, rty, span);
        }
        let v = self.operand(rhs)?;
        let ty = self.adjusted_ty(lhs)?;
        let p = self.place(lhs)?;
        self.write_place_dropping(p, ty, v, At::Expr(lhs), span)
    }

    /// Drops `p` (if `ty` needs drop, with unwind-safe write-back) and assigns `v` into `p`.
    fn write_place_dropping(&mut self, p: Place, ty: PTy, v: Operand, at: At, span: Span) -> Result<(), LowerError> {
        if self.needs_drop(ty, at)? {
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

    /// Destructuring assignment of value place `src` (of type `src_ty`) into assignee expression `lhs`.
    fn destructure_assign(&mut self, lhs: ExprId, src: Place, src_ty: PTy, span: Span) -> Result<(), LowerError> {
        self.depth += 1;
        if self.depth > MOST_DEPTH {
            return self.refuse(Construct::Depth, lhs);
        }
        let r = self.destructure_assign_inner(lhs, src, src_ty, span);
        self.depth -= 1;
        r
    }

    fn destructure_assign_inner(&mut self, lhs: ExprId, src: Place, src_ty: PTy, span: Span) -> Result<(), LowerError> {
        let at = At::Expr(lhs);
        match self.expr(lhs)? {
            Expr::Underscore => {
                self.b.push(StmtKind::FakeRead(src), span);
                Ok(())
            }
            Expr::Paren(inner) => self.destructure_assign(inner, src, src_ty, span),
            _ if self.is_place_expr(lhs)? => {
                let op = if self.is_copy(src_ty, at)? { Operand::Copy(src) } else { Operand::Move(src) };
                let ty = self.adjusted_ty(lhs)?;
                let p = self.place(lhs)?;
                self.write_place_dropping(p, ty, op, at, span)
            }
            Expr::Tuple { elems } => {
                let PNode::Tuple(flist) = self.node(src_ty, at)? else {
                    return self.refuse(Construct::DestructuringAssign, lhs);
                };
                let mut ftys: Vec<PTy, A> = Vec::new_in(self.alloc);
                ftys.extend_from_slice(self.list(flist, at)?);
                let ids = self.exprs_in(elems, lhs)?;
                for (i, &sub) in ids.iter().enumerate() {
                    let Some(&fty) = ftys.get(i) else { return self.refuse(Construct::Malformed, sub) };
                    let Ok(idx) = u32::try_from(i) else { return self.refuse(Construct::Malformed, sub) };
                    let fp = self.b.project(src, Proj::Field { index: idx, ty: fty });
                    self.destructure_assign(sub, fp, fty, span)?;
                }
                Ok(())
            }
            Expr::Array { elems } => {
                let PNode::Array(elem_ty, _) = self.node(src_ty, at)? else {
                    return self.refuse(Construct::DestructuringAssign, lhs);
                };
                let ids = self.exprs_in(elems, lhs)?;
                let Ok(len) = u32::try_from(ids.len()) else { return self.refuse(Construct::Malformed, lhs) };
                for (i, &sub) in ids.iter().enumerate() {
                    let Ok(offset) = u32::try_from(i) else { return self.refuse(Construct::Malformed, sub) };
                    let ep = self.b.project(src, Proj::ConstIndex { offset, min_length: len, from_end: false });
                    self.destructure_assign(sub, ep, elem_ty, span)?;
                }
                Ok(())
            }
            Expr::Call { callee, args } if matches!(self.expr_def(callee), Some(Def::Ctor { .. })) => {
                let Some(Def::Ctor { adt, variant, .. }) = self.expr_def(callee) else {
                    return self.refuse(Construct::DestructuringAssign, lhs);
                };
                let adt_f = self.adt_facts(adt, at)?;
                let base = if adt_f.is_enum { self.b.project(src, Proj::Downcast(variant)) } else { src };
                let ids = self.exprs_in(args, lhs)?;
                for (i, &sub) in ids.iter().enumerate() {
                    let fty = self.adjusted_ty(sub)?;
                    let Ok(index) = u32::try_from(i) else { return self.refuse(Construct::Malformed, sub) };
                    let fp = self.b.project_marked(base, Proj::Field { index, ty: fty }, adt_f.drop_impl);
                    self.destructure_assign(sub, fp, fty, span)?;
                }
                Ok(())
            }
            Expr::Struct { fields, .. } => {
                let (adt, variant) = match self.expr_def(lhs) {
                    Some(Def::Ctor { adt, variant, .. }) => (adt, variant),
                    Some(Def::Adt(k)) => (k, 0),
                    _ => match self.node(src_ty, at)? {
                        PNode::Adt(k, _) => (k, 0),
                        _ => return self.refuse(Construct::DestructuringAssign, lhs),
                    },
                };
                let adt_f = self.adt_facts(adt, at)?;
                let base = if adt_f.is_enum { self.b.project(src, Proj::Downcast(variant)) } else { src };
                for fi in self.field_inits_in(fields, lhs)? {
                    let Some(i) = self.facts.field(adt, variant, fi.name) else {
                        return self.refuse(Construct::UnknownField, fi.expr);
                    };
                    let fty = self.adjusted_ty(fi.expr)?;
                    let fp = self.b.project_marked(base, Proj::Field { index: i, ty: fty }, adt_f.drop_impl);
                    self.destructure_assign(fi.expr, fp, fty, span)?;
                }
                Ok(())
            }
            Expr::Record { fields, .. } => {
                let PNode::Record(schema, flist) = self.node(src_ty, at)? else {
                    return self.refuse(Construct::DestructuringAssign, lhs);
                };
                let mut ftys: Vec<PTy, A> = Vec::new_in(self.alloc);
                ftys.extend_from_slice(self.list(flist, at)?);
                for fi in self.field_inits_in(fields, lhs)? {
                    let Some(i) = self.tys.record_field_index(schema, fi.name) else {
                        return self.refuse(Construct::UnknownField, fi.expr);
                    };
                    let Ok(ix) = usize::try_from(i) else { return self.refuse(Construct::Malformed, fi.expr) };
                    let Some(&fty) = ftys.get(ix) else { return self.refuse(Construct::Malformed, fi.expr) };
                    let fp = self.b.project(src, Proj::Field { index: i, ty: fty });
                    self.destructure_assign(fi.expr, fp, fty, span)?;
                }
                Ok(())
            }
            _ => self.refuse(Construct::DestructuringAssign, lhs),
        }
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
        if mutbl == Mutbl::Not && self.is_const_promoted(e) {
            let oty = self.adjusted_ty(operand)?;
            let l = self.b.new_local(LocalDecl { ty: oty, kind: LocalKind::Temp(operand, Role::Value), mutbl: Mutbl::Mut });
            let p = self.b.local_place(l);
            self.lower_into(p, operand)?;
            let o = self.b.origin(false);
            self.b.borrow(dest, BorrowKind::Shared, p, o, span);
            return Ok(());
        }
        if mutbl == Mutbl::Not && self.is_promoted(e) {
            let ty = self.expr_ty(e)?;
            let c = self.b.new_const(ConstDecl { kind: ConstKind::Promoted(e.0), ty });
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
        if let PNode::Adt(k, _) = fnode
            && intish(tnode)
            && self.adt_facts(k, at)?.is_enum
        {
            let p = self.place(operand)?;
            self.b.assign(dest, Value::Discriminant(p), span);
            return Ok(());
        }
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
            let inits = self.field_inits_in(fields, e)?;
            let [fi] = inits else { return self.refuse(Construct::Malformed, e) };
            let Some(i) = self.facts.field(adt, 0, fi.name) else { return self.refuse(Construct::UnknownField, fi.expr) };
            let o = self.operand(fi.expr)?;
            let operands = self.b.operands(&[o]);
            self.b.assign(dest, Value::Aggregate { kind: AggregateKind::Adt(i), ty, operands }, span);
            return Ok(());
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

    /// Evaluates expression `x` into a fresh temporary local (scheduled above `arg_mark`) and
    /// returns an operand reading or moving it, so dropping earlier locals cannot invalidate it.
    fn materialize_tail_operand(&mut self, x: ExprId) -> Result<Operand, LowerError> {
        let ty = self.adjusted_ty(x)?;
        let tmp = self.temp(x, ty, Role::Value)?;
        self.lower_into(tmp, x)?;
        Ok(if self.is_copy(ty, At::Expr(x))? { Operand::Copy(tmp) } else { Operand::Move(tmp) })
    }

    /// `become value` (RFC 3407 explicit tail call): for direct fn, concrete method, and
    /// non-`"rust-call"` fn-pointer calls with no return adjustments, materializes callee/args
    /// into fresh temps, drops all local scopes `0..arg_mark`, and terminates with
    /// `StmtKind::TailCall`; otherwise falls back cleanly to `self.return_(e, Some(value))`.
    fn become_(&mut self, e: ExprId, value: ExprId) -> Result<(), LowerError> {
        let mut call_e = value;
        for _ in 0..16 {
            if let Expr::Paren(inner) = self.expr(call_e)? {
                call_e = inner;
            } else {
                break;
            }
        }
        let (vs, ve) = self.adjust_rows(value);
        let (cs, ce) = self.adjust_rows(call_e);
        if vs != ve || cs != ce {
            return self.return_(e, Some(value));
        }
        let span = self.span(e);
        match self.expr(call_e)? {
            Expr::Call { callee, args } => {
                if matches!(self.expr_def(callee), Some(Def::Ctor { .. })) {
                    return self.return_(e, Some(value));
                }
                let ids = self.exprs_in(args, call_e)?;
                let cty = self.adjusted_ty(callee)?;
                let act = self.activate.len();
                match self.node(cty, At::Expr(callee))? {
                    PNode::FnDef(k, l) => {
                        let arg_mark = self.sched.len();
                        if !matches!(self.expr(callee)?, Expr::Path(_)) {
                            let _ = self.operand(callee)?;
                        }
                        let c = self.item_callee(k, l, call_e)?;
                        let mut ops: Vec<Operand, A> = Vec::with_capacity_in(ids.len(), self.alloc);
                        for &a in ids {
                            ops.push(self.materialize_tail_operand(a)?);
                        }
                        self.finish_tail_call(c, &ops, arg_mark, act, span)
                    }
                    PNode::FnPtr(_, header) if header.abi != blaze_typeck::Abi::RustCall => {
                        let arg_mark = self.sched.len();
                        let callee_op = self.materialize_tail_operand(callee)?;
                        let mut ops: Vec<Operand, A> = Vec::with_capacity_in(ids.len(), self.alloc);
                        for &a in ids {
                            ops.push(self.materialize_tail_operand(a)?);
                        }
                        self.finish_tail_call(Callee::Value(callee_op), &ops, arg_mark, act, span)
                    }
                    _ => self.return_(e, Some(value)),
                }
            }
            Expr::MethodCall { receiver, args, .. } => {
                let Some((k, l)) = self.callee_of(call_e) else {
                    return self.return_(e, Some(value));
                };
                let c = self.item_callee(k, l, call_e)?;
                let act = self.activate.len();
                let ids = self.exprs_in(args, call_e)?;
                let arg_mark = self.sched.len();
                let mut ops: Vec<Operand, A> = Vec::with_capacity_in(ids.len() + 1, self.alloc);
                ops.push(self.materialize_tail_operand(receiver)?);
                for &a in ids {
                    ops.push(self.materialize_tail_operand(a)?);
                }
                self.finish_tail_call(c, &ops, arg_mark, act, span)
            }
            _ => self.return_(e, Some(value)),
        }
    }

    fn finish_tail_call(
        &mut self,
        c: Callee,
        ops: &[Operand],
        arg_mark: usize,
        act: usize,
        span: Span,
    ) -> Result<(), LowerError> {
        let mut i = act;
        while let Some(&loan) = self.activate.get(i) {
            self.b.push(StmtKind::Activate(loan), span);
            i += 1;
        }
        self.activate.truncate(act);
        self.emit_exit_below(arg_mark, span)?;
        let callee = self.b.callee(c);
        let args = self.b.operands(ops);
        self.b.terminate(StmtKind::TailCall { callee, args }, span);
        self.dead_block();
        Ok(())
    }

    /// Lowers `Expr::Asm` into `StmtKind::InlineAsm` with its self-contained `AsmRow` metadata
    /// table entry in `blaze-body`.
    fn asm_into(
        &mut self,
        dest: Place,
        e: ExprId,
        arch: TcAsmArch,
        pieces: List,
        operands: List,
        options: u16,
        clobber_abis: List,
    ) -> Result<(), LowerError> {
        const ASM_NORETURN: u16 = 1 << 4;
        let span = self.span(e);
        let body = self.body;
        let Some(ops) = body.asm_operands.get(operands.range()) else {
            return self.refuse(Construct::Malformed, e);
        };
        let Some(tpl) = body.asm_pieces.get(pieces.range()) else {
            return self.refuse(Construct::Malformed, e);
        };
        let Some(abis) = body.asm_clobbers.get(clobber_abis.range()) else {
            return self.refuse(Construct::Malformed, e);
        };
        let make_malformed = || LowerError { construct: Construct::Malformed, at: At::Expr(e), range: crate::range_of(body, e) };
        let asm_str = |first: u32, len: u32| -> Result<&'v str, LowerError> {
            let s = usize::try_from(first).map_err(|_| make_malformed())?;
            let l = usize::try_from(len).map_err(|_| make_malformed())?;
            let end = s.checked_add(l).ok_or_else(make_malformed)?;
            body.asm_text.get(s..end).ok_or_else(make_malformed)
        };

        let mut in_ops: Vec<Operand, A> = Vec::new_in(self.alloc);
        let mut asm_ops: Vec<AsmOpInput<'v>, A> = Vec::with_capacity_in(ops.len(), self.alloc);
        let mut labels: Vec<(Block, ExprId), A> = Vec::new_in(self.alloc);
        for op in ops {
            let (reg_kind, reg) = match op.reg {
                Some(TcAsmReg::Class { first, len }) => (AsmRegKind::Class, asm_str(first, len)?),
                Some(TcAsmReg::Explicit { first, len }) => (AsmRegKind::Explicit, asm_str(first, len)?),
                None => (AsmRegKind::None, ""),
            };
            let desc = match op.kind {
                TcAsmKind::In => {
                    let idx = u32::try_from(in_ops.len()).map_err(|_| make_malformed())?;
                    let v = self.operand(op.expr)?;
                    in_ops.push(v);
                    AsmOpInput {
                        kind: AsmOpKind::In,
                        reg_kind,
                        reg,
                        in_op: Some(idx),
                        out_place: None,
                        label: None,
                        discard_out: false,
                    }
                }
                TcAsmKind::Out | TcAsmKind::LateOut => {
                    let kind = if op.kind == TcAsmKind::Out { AsmOpKind::Out } else { AsmOpKind::LateOut };
                    if matches!(self.expr(op.expr)?, Expr::Underscore) {
                        AsmOpInput { kind, reg_kind, reg, in_op: None, out_place: None, label: None, discard_out: true }
                    } else {
                        let p = self.place(op.expr)?;
                        AsmOpInput { kind, reg_kind, reg, in_op: None, out_place: Some(p), label: None, discard_out: false }
                    }
                }
                TcAsmKind::InOut | TcAsmKind::InLateOut => {
                    let kind = if op.kind == TcAsmKind::InOut { AsmOpKind::InOut } else { AsmOpKind::InLateOut };
                    match op.out_expr {
                        None => {
                            let p = self.place(op.expr)?;
                            AsmOpInput { kind, reg_kind, reg, in_op: None, out_place: Some(p), label: None, discard_out: false }
                        }
                        Some(out) => {
                            let idx = u32::try_from(in_ops.len()).map_err(|_| make_malformed())?;
                            let v = self.operand(op.expr)?;
                            in_ops.push(v);
                            if matches!(self.expr(out)?, Expr::Underscore) {
                                AsmOpInput { kind, reg_kind, reg, in_op: Some(idx), out_place: None, label: None, discard_out: true }
                            } else {
                                let p = self.place(out)?;
                                AsmOpInput { kind, reg_kind, reg, in_op: Some(idx), out_place: Some(p), label: None, discard_out: false }
                            }
                        }
                    }
                }
                TcAsmKind::Sym => {
                    let idx = u32::try_from(in_ops.len()).map_err(|_| make_malformed())?;
                    let sym_ty = self.expr_ty(op.expr)?;
                    let c = if let PNode::FnDef(key, args) = self.node(sym_ty, At::Expr(op.expr))? {
                        self.b.new_const(ConstDecl { kind: ConstKind::FnItem { key, args }, ty: sym_ty })
                    } else if let Some(Def::Static(item, _)) = self.expr_def(op.expr) {
                        self.b.new_const(ConstDecl { kind: ConstKind::StaticAddr(item), ty: sym_ty })
                    } else {
                        return self.refuse(Construct::Malformed, op.expr);
                    };
                    in_ops.push(Operand::Const(c));
                    AsmOpInput {
                        kind: AsmOpKind::Sym,
                        reg_kind: AsmRegKind::None,
                        reg: "",
                        in_op: Some(idx),
                        out_place: None,
                        label: None,
                        discard_out: false,
                    }
                }
                TcAsmKind::Const => {
                    let idx = u32::try_from(in_ops.len()).map_err(|_| make_malformed())?;
                    let mut x = op.expr;
                    for _ in 0..16 {
                        if let Expr::Paren(inner) = self.expr(x)? {
                            x = inner;
                        } else {
                            break;
                        }
                    }
                    let c_op = if let Some(c) = self.const_operand(x)? {
                        c
                    } else if let Expr::Unary { op: UnOp::Neg, operand } = self.expr(x)?
                        && let Expr::Lit(Lit::Int(_)) = self.expr(operand)?
                    {
                        let ty = self.expr_ty(x)?;
                        let raw = match self.body.call_arg_ints.iter().find(|(id, _)| *id == operand) {
                            Some(&(_, v)) => v,
                            None => match self.facts.int_lit(operand) {
                                Some(v) => v,
                                None => return self.refuse(Construct::NoLiteralValue, operand),
                            },
                        };
                        self.scalar(raw.wrapping_neg(), ty)
                    } else {
                        return self.refuse(Construct::InlineAsm, x);
                    };
                    in_ops.push(c_op);
                    AsmOpInput {
                        kind: AsmOpKind::Const,
                        reg_kind: AsmRegKind::None,
                        reg: "",
                        in_op: Some(idx),
                        out_place: None,
                        label: None,
                        discard_out: false,
                    }
                }
                TcAsmKind::Label => {
                    let lbl = self.b.new_block(BlockKind::Normal);
                    labels.push((lbl, op.expr));
                    AsmOpInput {
                        kind: AsmOpKind::Label,
                        reg_kind: AsmRegKind::None,
                        reg: "",
                        in_op: None,
                        out_place: None,
                        label: Some(lbl),
                        discard_out: false,
                    }
                }
            };
            asm_ops.push(desc);
        }

        let mut pieces_in: Vec<AsmPieceInput<'v>, A> = Vec::with_capacity_in(tpl.len(), self.alloc);
        for piece in tpl {
            pieces_in.push(match *piece {
                TcAsmPiece::Text { first, len } => AsmPieceInput::Text(asm_str(first, len)?),
                TcAsmPiece::Line => AsmPieceInput::Line,
                TcAsmPiece::Operand { index, modifier } => AsmPieceInput::Operand { index, modifier },
            });
        }
        let mut clobbers_buf: Vec<u8, A> = Vec::new_in(self.alloc);
        for (i, &(first, len)) in abis.iter().enumerate() {
            if i > 0 {
                clobbers_buf.push(b',');
            }
            clobbers_buf.extend_from_slice(asm_str(first, len)?.as_bytes());
        }
        let clobbers_str = core::str::from_utf8(&clobbers_buf).map_err(|_| make_malformed())?;
        let body_arch = match arch {
            TcAsmArch::Aarch64 => BodyAsmArch::Aarch64,
            TcAsmArch::X86_64 => BodyAsmArch::X86_64,
        };
        let asm_id = self.b.new_asm(body_arch, options, clobbers_str, &pieces_in, &asm_ops);
        let ops_range = self.b.operands(&in_ops);
        let noreturn = (options & ASM_NORETURN) != 0;

        if labels.is_empty() {
            if noreturn {
                self.b.terminate(StmtKind::InlineAsm { asm: asm_id, operands: ops_range, target: None, unwind: Unwind::Continue }, span);
                self.dead_block();
                return Ok(());
            }
            let next = self.b.new_block(BlockKind::Normal);
            self.b.terminate(StmtKind::InlineAsm { asm: asm_id, operands: ops_range, target: Some(next), unwind: Unwind::Continue }, span);
            self.b.start(next);
            let ty = self.expr_ty(e)?;
            return self.unit(dest, ty, At::Expr(e), span);
        }

        let join = self.b.new_block(BlockKind::Normal);
        let target = if noreturn { None } else { Some(join) };
        self.b.terminate(StmtKind::InlineAsm { asm: asm_id, operands: ops_range, target, unwind: Unwind::Continue }, span);
        for &(lbl, lbl_expr) in labels.iter() {
            self.b.start(lbl);
            let _ = self.open();
            let lty = self.expr_ty(lbl_expr)?;
            let tmp = self.temp(lbl_expr, lty, Role::Value)?;
            self.lower_into(tmp, lbl_expr)?;
            self.close(span)?;
            self.goto(join, span);
        }
        self.b.start(join);
        let ty = self.expr_ty(e)?;
        if !matches!(self.node(ty, At::Expr(e))?, PNode::Never) {
            self.unit(dest, ty, At::Expr(e), span)?;
        }
        Ok(())
    }
}
