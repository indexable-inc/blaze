//! The driver's machine (typeck.md, "The walk: a work stack in rustc's order").
//!
//! A stack of [`Frame`]s over the body's flat arrays. A rule never calls the checker of a child:
//! it pushes itself back with its next state, then the child, and reads the child's type from
//! `ret` when it resumes. No native recursion follows the expression tree, the type tree or
//! the pattern tree, so depth costs frames in a column sized from the body, not native stack.
//! Every column and queue is plain data, so a run can stop anywhere at `Need`.

#![blaze::sans_io]

use blaze_own::{Alloc, Soa, Vec};
use core::mem::DropGuard;

use crate::body::BodyView;
use crate::columns::{AdjustKind, CaptureKind};
use crate::diag::{At, DiagCode, PendingDiag};
use crate::error::{CheckError, Limit, Malformed, Stop};
use crate::ids::{
    ExprId, FloatTy, IntTy, ItemKey, LangItem, Mutbl, Name, PatId, StmtId, UintTy,
};
use crate::portable::{PNode, PTy, PTys};
use crate::ty::{Cause, Goal, Obligation, Ty, TyList, TyView, TypeCtx, Var, VarKind};
use crate::world::{PClause, Reads, Shape, World};

/// rustc's default `recursion_limit`: the autoderef bound (E0055 past it).
pub(crate) const RECURSION_LIMIT: u32 = 128;

/// A planted bug for the read-list property (`tests::read_list_property`): outside tests there is none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    None,
    /// A field access reads its ADT through the unrecorded backdoor.
    #[cfg(test)]
    PeekFields,
}

/// What the context expects of an expression (rustc's `Expectation`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Expectation {
    None,
    HasType(Ty),
    Castable(Ty),
    RvalueLikeUnsized(Ty),
}

impl Expectation {
    /// `ExpectHasType`'s type only (`only_has_type`).
    pub(crate) fn only_has_type(self) -> Option<Ty> {
        match self {
            Expectation::HasType(t) => Some(t),
            _ => None,
        }
    }

    /// Any expectation's type (`to_option`).
    pub(crate) fn to_option(self) -> Option<Ty> {
        match self {
            Expectation::None => None,
            Expectation::HasType(t) | Expectation::Castable(t) | Expectation::RvalueLikeUnsized(t) => {
                Some(t)
            }
        }
    }
}

/// Where a finished expression's result goes, after it is written to its column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Then {
    /// Nowhere: the parent reads it from `ret`.
    Nothing,
    /// Coerced to `to` (`check_expr_coercible_to_type`); `ret` is `to`.
    Coerce { to: Ty, two_phase: bool },
    /// Must have type `to`, `!` allowed (`check_expr_has_type_or_error`); `ret` is `to`.
    Demand(Ty),
    /// Pushed into a coercion site (`CoerceMany::coerce`).
    CoerceMany(u32),
}

/// What a frame checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Node {
    Expr(ExprId),
    Pat(PatId),
    Stmt(StmtId),
}

/// One frame of the walk: the node, the step of its rule, its expectation, where its result
/// goes, the divergence flag rustc saves around each expression, and a rule's own scratch.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Frame {
    pub node: Node,
    pub state: u8,
    pub saved: bool,
    pub expect: Expectation,
    pub then: Then,
    pub a: u32,
    pub b: u32,
    pub c: u32,
    pub t: Ty,
    pub u: Ty,
    pub l: TyList,
}

/// What a rule's step did.
pub(crate) enum Flow {
    /// It pushed its continuation and a child.
    Wait,
    /// The node's type.
    Done(Ty),
}

/// A coercion site with several sources (`CoerceMany`): `if`/`else`, `match` arms, a block's
/// tail and its breaks, a loop's breaks, a body's returns.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CoerceMany {
    pub expected: Ty,
    pub final_ty: Option<Ty>,
    pub pushed: u32,
}

/// An enclosing loop or block that a `break` may leave (`BreakableCtxt`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct BreakCtx {
    pub target: ExprId,
    pub coerce: Option<u32>,
    pub may_break: bool,
}

/// A cast to check after fallback (`deferred_cast_checks`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct CastCheck {
    pub expr: ExprId,
    pub operand: ExprId,
    pub from: Ty,
    pub to: Ty,
}

/// A closure whose captures are analysed after selection (`closure_analyze`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingClosure {
    pub expr: ExprId,
}

/// What `Machine::snapshot` saved: the type context's snapshot and each column's length.
pub(crate) struct MachineSnapshot<S> {
    ty: S,
    diags: usize,
    errors: u32,
    adjust: usize,
    adj_scratch: usize,
    callees: usize,
    fields: usize,
    captures: usize,
    casts: usize,
    numeric: usize,
    diverging: usize,
    closures: usize,
}

/// An autoderef step: the type, and whether it was reached by `Deref::deref`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Step {
    pub ty: Ty,
    pub overloaded: bool,
}

/// How a portable type's generic parameters and holes are lowered.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Lower {
    /// A world's answer: parameter `i` is `args[i]`; a hole is malformed.
    Args(TyList),
    /// A type written in the body: parameters are the owner's own; a hole is a fresh variable.
    Body,
}

/// The types every body uses.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Common {
    pub unit: Ty,
    pub bool_: Ty,
    pub char_: Ty,
    pub str_: Ty,
    pub static_str: Ty,
    pub never: Ty,
    pub err: Ty,
    pub usize_: Ty,
    pub u8_: Ty,
    pub i32_: Ty,
    pub f64_: Ty,
    pub empty: TyList,
}

/// The machine of one run.
pub(crate) struct Machine<'b, 'w, 'c, 'a, W: World, C: TypeCtx> {
    pub body: BodyView<'b>,
    pub reads: Reads<'w, 'a, W>,
    pub ty: &'c mut C,
    pub alloc: Alloc<'a>,
    pub plant: Plant,
    pub common: Common,
    /// The type a finished child left for its parent.
    pub ret: Ty,
    /// Whether the code checked so far always diverges (rustc's `Diverges::Always`).
    pub diverges: bool,
    pub frames: Vec<Frame, A>,
    pub max_frames: usize,
    // Results, by node.
    pub expr_ty: Vec<Ty, A>,
    pub pat_ty: Vec<Ty, A>,
    pub local_ty: Vec<Ty, A>,
    // Sparse results, rows read whole.
    /// Adjustments in the order made; scanned by expression (a receiver's autoref).
    pub adjust: Soa<(ExprId, AdjustKind, Ty), A>,
    pub callees: Vec<(ExprId, ItemKey, TyList), A>,
    pub fields: Vec<(ExprId, u32, u32), A>,
    pub captures: Vec<(ExprId, PatId, CaptureKind), A>,
    pub diags: Vec<PendingDiag, A>,
    /// The number of errors reported: the body is "tainted by errors" when it is not zero.
    pub errors: u32,
    // Queues, processed where rustc processes them.
    pub casts: Vec<CastCheck, A>,
    pub numeric: Vec<Ty, A>,
    pub diverging: Vec<Ty, A>,
    pub closures: Vec<PendingClosure, A>,
    pub coerce_many: Vec<CoerceMany, A>,
    pub breaks: Vec<BreakCtx, A>,
    /// The innermost body's return site (the fn's, or a closure's).
    pub ret_sites: Vec<u32, A>,
    /// The adjustments of the coercion being tried, moved to `adjust` once it commits.
    pub adj_scratch: Vec<(AdjustKind, Ty), A>,
}

impl<'b, 'w, 'c, 'a, W: World, C: TypeCtx> Machine<'b, 'w, 'c, 'a, W, C> {
    pub(crate) fn new_in(
        body: BodyView<'b>,
        reads: Reads<'w, 'a, W>,
        ty: &'c mut C,
        plant: Plant,
        alloc: Alloc<'a>,
    ) -> Result<Machine<'b, 'w, 'c, 'a, W, C>, CheckError> {
        let empty = ty.mk_list(&[])?;
        let unit = ty.mk(TyView::Tuple(empty))?;
        let str_ = ty.mk(TyView::Str)?;
        let common = Common {
            unit,
            bool_: ty.mk(TyView::Bool)?,
            char_: ty.mk(TyView::Char)?,
            str_,
            static_str: ty.mk(TyView::Ref(Mutbl::Not, str_))?,
            never: ty.mk(TyView::Never)?,
            err: ty.mk(TyView::Error)?,
            usize_: ty.mk(TyView::Uint(UintTy::Usize))?,
            u8_: ty.mk(TyView::Uint(UintTy::U8))?,
            i32_: ty.mk(TyView::Int(IntTy::I32))?,
            f64_: ty.mk(TyView::Float(FloatTy::F64))?,
            empty,
        };
        let err = common.err;
        let mut expr_ty = Vec::with_capacity_in(body.exprs.len(), alloc.clone());
        expr_ty.resize(body.exprs.len(), err);
        let mut pat_ty = Vec::with_capacity_in(body.pats.len(), alloc.clone());
        pat_ty.resize(body.pats.len(), err);
        let mut local_ty = Vec::with_capacity_in(body.pats.len(), alloc.clone());
        local_ty.resize(body.pats.len(), err);
        Ok(Machine {
            body,
            reads,
            ty,
            plant,
            common,
            ret: err,
            diverges: false,
            frames: Vec::new_in(alloc.clone()),
            max_frames: 4 * body.nodes() + 64,
            expr_ty,
            pat_ty,
            local_ty,
            adjust: Soa::new_in(alloc.clone()),
            callees: Vec::new_in(alloc.clone()),
            fields: Vec::new_in(alloc.clone()),
            captures: Vec::new_in(alloc.clone()),
            diags: Vec::new_in(alloc.clone()),
            errors: 0,
            casts: Vec::new_in(alloc.clone()),
            numeric: Vec::new_in(alloc.clone()),
            diverging: Vec::new_in(alloc.clone()),
            closures: Vec::new_in(alloc.clone()),
            coerce_many: Vec::new_in(alloc.clone()),
            breaks: Vec::new_in(alloc.clone()),
            ret_sites: Vec::new_in(alloc.clone()),
            adj_scratch: Vec::new_in(alloc.clone()),
            alloc,
        })
    }

    // The stack.

    /// Runs frames until the stack is empty.
    pub(crate) fn run(&mut self) -> Result<(), Stop> {
        let bound = 16 * (self.body.nodes() as u64 + 1) + 1024;
        let mut steps = 0u64;
        while let Some(f) = self.frames.pop() {
            steps += 1;
            if steps > bound {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            match f.node {
                Node::Expr(e) => self.step_expr(f, e)?,
                Node::Pat(p) => self.step_pat(f, p)?,
                Node::Stmt(s) => self.step_stmt(f, s)?,
            }
        }
        Ok(())
    }

    pub(crate) fn push(&mut self, f: Frame) -> Result<(), Stop> {
        if self.frames.len() >= self.max_frames {
            return Err(CheckError::Limit(Limit::Frames).into());
        }
        self.frames.push(f);
        Ok(())
    }

    fn frame(&self, node: Node, expect: Expectation, then: Then) -> Frame {
        let err = self.common.err;
        Frame {
            node,
            state: 0,
            saved: false,
            expect,
            then,
            a: 0,
            b: 0,
            c: 0,
            t: err,
            u: err,
            l: self.common.empty,
        }
    }

    /// Pushes the check of `e`.
    pub(crate) fn push_expr(&mut self, e: ExprId, expect: Expectation, then: Then) -> Result<(), Stop> {
        let f = self.frame(Node::Expr(e), expect, then);
        self.push(f)
    }

    /// Pushes the check of `e` coerced to `to` (`check_expr_coercible_to_type`).
    pub(crate) fn push_coercible(&mut self, e: ExprId, to: Ty) -> Result<(), Stop> {
        self.push_expr(e, Expectation::HasType(to), Then::Coerce { to, two_phase: false })
    }

    /// Pushes the check of `e` against `to`, `!` allowed (`check_expr_has_type_or_error`).
    pub(crate) fn push_has_type(&mut self, e: ExprId, to: Ty) -> Result<(), Stop> {
        self.push_expr(e, Expectation::HasType(to), Then::Demand(to))
    }

    /// Pushes the check of pattern `p` against `expected` with default binding mode `mode`
    /// (0 move, 1 `ref`, 2 `ref mut`).
    pub(crate) fn push_pat(&mut self, p: PatId, expected: Ty, mode: u32) -> Result<(), Stop> {
        let mut f = self.frame(Node::Pat(p), Expectation::HasType(expected), Then::Nothing);
        f.a = mode;
        self.push(f)
    }

    pub(crate) fn push_stmt(&mut self, s: StmtId) -> Result<(), Stop> {
        let f = self.frame(Node::Stmt(s), Expectation::None, Then::Nothing);
        self.push(f)
    }

    /// Pushes `f` back to resume at `state`.
    pub(crate) fn resume(&mut self, mut f: Frame, state: u8) -> Result<(), Stop> {
        f.state = state;
        self.push(f)
    }

    fn step_expr(&mut self, mut f: Frame, e: ExprId) -> Result<(), Stop> {
        if f.state == 0 {
            // `check_expr_with_expectation_and_args`: the divergence of what came before is
            // saved and the expression starts at "maybe".
            f.saved = self.diverges;
            self.diverges = false;
        }
        let expr = self.body.expr(e)?;
        if let Flow::Done(ty) = self.expr_rule(f, e, expr)? {
            self.finish_expr(f, e, ty)?;
        }
        Ok(())
    }

    /// Writes `e`'s type, folds divergence, and sends the result where `f.then` says.
    fn finish_expr(&mut self, f: Frame, e: ExprId, ty: Ty) -> Result<(), Stop> {
        self.expr_ty[e.ix()] = ty;
        if self.is_never(ty) {
            self.diverges = true;
        }
        self.diverges |= f.saved;
        self.ret = match f.then {
            Then::Nothing => ty,
            Then::Coerce { to, two_phase } => {
                if !self.coerce(Some(e), ty, to, two_phase)? {
                    self.diag(DiagCode::Mismatch, At::Expr(e), Some(to), Some(ty), None);
                }
                to
            }
            Then::Demand(to) => {
                self.demand_has_type(e, ty, to)?;
                to
            }
            Then::CoerceMany(site) => {
                self.coerce_many_push(site, Some(e), ty)?;
                ty
            }
        };
        Ok(())
    }

    /// `check_expr_has_type_or_error`'s demand: `!` becomes any type by `NeverToAny`; otherwise
    /// the types must be equal.
    pub(crate) fn demand_has_type(&mut self, e: ExprId, ty: Ty, to: Ty) -> Result<(), Stop> {
        if self.is_never(ty) && !self.is_never(to) {
            self.adjust.push((e, AdjustKind::NeverToAny, to));
            if self.is_ty_var(to) {
                self.diverging.push(to);
            }
            return Ok(());
        }
        if !self.unify(to, ty)? {
            self.diag(DiagCode::Mismatch, At::Expr(e), Some(to), Some(ty), None);
        }
        Ok(())
    }

    // Coercion sites.

    pub(crate) fn new_coerce_many(&mut self, expected: Ty) -> Result<u32, Stop> {
        let at = u32::try_from(self.coerce_many.len())
            .map_err(|_| CheckError::Limit(Limit::Table))?;
        self.coerce_many.push(CoerceMany { expected, final_ty: None, pushed: 0 });
        Ok(at)
    }

    /// `CoerceMany::coerce` (and `coerce_forced_unit` when `e` is `None`).
    pub(crate) fn coerce_many_push(&mut self, site: u32, e: Option<ExprId>, ty: Ty) -> Result<(), Stop> {
        let cm = self.coerce_many[site as usize];
        let ty = self.try_structurally_resolve(ty)?;
        let merged = cm.final_ty.unwrap_or(cm.expected);
        if self.is_err(ty) || self.is_err(merged) {
            self.coerce_many[site as usize].final_ty = Some(self.common.err);
            return Ok(());
        }
        let ok = match e {
            None => {
                // A block with no tail, or a `break`/`return` with no value: `()` is unified,
                // not coerced.
                if self.unify(cm.expected, ty)? { Some(cm.expected) } else { None }
            }
            Some(e) if cm.pushed == 0 => {
                // The first source is coerced to the expected type.
                if self.coerce(Some(e), ty, cm.expected, false)? { Some(cm.expected) } else { None }
            }
            Some(e) => self.coercion_lub(e, merged, ty)?,
        };
        let at = match e {
            Some(e) => At::Expr(e),
            None => At::Body,
        };
        match ok {
            Some(t) => {
                let entry = &mut self.coerce_many[site as usize];
                entry.final_ty = Some(t);
                entry.pushed += 1;
            }
            None => {
                let code = if cm.pushed == 0 { DiagCode::Mismatch } else { DiagCode::IncompatibleArms };
                self.diag(code, at, Some(merged), Some(ty), None);
                self.coerce_many[site as usize].final_ty = Some(self.common.err);
            }
        }
        Ok(())
    }

    /// `try_find_coercion_lub`, first cut: the new source coerced to the merged type, else the
    /// two unified as their least upper bound (equal, regions being erased).
    fn coercion_lub(&mut self, e: ExprId, merged: Ty, ty: Ty) -> Result<Option<Ty>, Stop> {
        if self.coerce(Some(e), ty, merged, false)? {
            return Ok(Some(merged));
        }
        if self.unify(merged, ty)? {
            return Ok(Some(merged));
        }
        Ok(None)
    }

    /// `CoerceMany::complete`: the merged type, or `!` when nothing was pushed.
    pub(crate) fn coerce_many_complete(&mut self, site: u32) -> Ty {
        match self.coerce_many[site as usize].final_ty {
            Some(t) => t,
            None => self.common.never,
        }
    }

    // Types.

    pub(crate) fn mk(&mut self, v: TyView) -> Result<Ty, Stop> {
        Ok(self.ty.mk(v)?)
    }

    pub(crate) fn mk_list(&mut self, tys: &[Ty]) -> Result<TyList, Stop> {
        Ok(self.ty.mk_list(tys)?)
    }

    /// A fresh type variable.
    pub(crate) fn fresh(&mut self) -> Result<Ty, Stop> {
        Ok(self.ty.fresh(VarKind::Ty)?)
    }

    /// A fresh integer variable, queued for fallback.
    pub(crate) fn fresh_int(&mut self) -> Result<Ty, Stop> {
        let v = self.ty.fresh(VarKind::Int)?;
        self.numeric.push(v);
        Ok(v)
    }

    /// A fresh float variable, queued for fallback.
    pub(crate) fn fresh_float(&mut self) -> Result<Ty, Stop> {
        let v = self.ty.fresh(VarKind::Float)?;
        self.numeric.push(v);
        Ok(v)
    }

    /// `n` fresh type variables.
    pub(crate) fn fresh_args(&mut self, n: u32) -> Result<TyList, Stop> {
        let mut v: Vec<Ty, A> = Vec::with_capacity_in(n as usize, self.alloc.clone());
        for _ in 0..n {
            v.push(self.fresh()?);
        }
        self.mk_list(&v)
    }

    /// The list's types, copied out.
    pub(crate) fn list(&self, l: TyList) -> Vec<Ty, A> {
        let n = self.ty.list_len(l);
        let mut v = Vec::with_capacity_in(n as usize, self.alloc.clone());
        for i in 0..n {
            v.push(self.ty.list_get(l, i));
        }
        v
    }

    /// One level of `t`, resolved at its top.
    pub(crate) fn view(&mut self, t: Ty) -> TyView {
        let t = self.ty.shallow_resolve(t);
        self.ty.view(t)
    }

    pub(crate) fn is_err(&mut self, t: Ty) -> bool {
        matches!(self.view(t), TyView::Error)
    }

    pub(crate) fn is_never(&mut self, t: Ty) -> bool {
        matches!(self.view(t), TyView::Never)
    }

    /// An unresolved general type variable (not an integer or float one).
    pub(crate) fn is_ty_var(&mut self, t: Ty) -> bool {
        matches!(self.view(t), TyView::Infer(Var { kind: VarKind::Ty, .. }))
    }

    pub(crate) fn unify(&mut self, a: Ty, b: Ty) -> Result<bool, Stop> {
        Ok(self.ty.unify(a, b)?)
    }

    /// Unifies, reporting a mismatch at `at`.
    pub(crate) fn demand_eq(&mut self, at: At, expected: Ty, found: Ty) -> Result<bool, Stop> {
        let ok = self.unify(expected, found)?;
        if !ok {
            self.diag(DiagCode::Mismatch, at, Some(expected), Some(found), None);
        }
        Ok(ok)
    }

    /// Records a diagnostic. The body is tainted by errors from now on.
    pub(crate) fn diag(
        &mut self,
        code: DiagCode,
        at: At,
        expected: Option<Ty>,
        found: Option<Ty>,
        name: Option<Name>,
    ) {
        self.errors += 1;
        self.diags.push(PendingDiag { code, at, expected, found, name });
    }

    // Selection.

    /// `select_obligations_where_possible`, reporting what failed (E0277).
    pub(crate) fn select(&mut self) -> Result<(), Stop> {
        self.ty.select_where_possible(&mut self.reads)?;
        self.report_fulfill_errors(false);
        Ok(())
    }

    /// `select_all_obligations_or_error`: ambiguity is "type annotations needed", reported
    /// only when the body has no other error (rustc's `tainted_by_errors`).
    pub(crate) fn select_all(&mut self) -> Result<(), Stop> {
        self.ty.select_all(&mut self.reads)?;
        self.report_fulfill_errors(true);
        Ok(())
    }

    fn report_fulfill_errors(&mut self, ambiguity: bool) {
        // Bounded by the context's error count; each `next_error` removes one.
        while let Some(err) = self.ty.next_error() {
            let at = match err.obligation.cause {
                Cause::Expr(e) => At::Expr(e),
                Cause::Pat(p) => At::Pat(p),
                Cause::Body => At::Body,
            };
            match err.kind {
                crate::ty::FulfillErrorKind::Ambiguous => {
                    if ambiguity && self.errors == 0 {
                        self.diag(DiagCode::AnnotationsNeeded, at, None, None, None);
                    }
                }
                crate::ty::FulfillErrorKind::Unsatisfied | crate::ty::FulfillErrorKind::Mismatch => {
                    self.diag(DiagCode::Unsatisfied, at, None, None, None);
                }
            }
        }
    }

    /// `try_structurally_resolve_type`: resolved at the top, selecting pending obligations
    /// first when it is still a variable.
    pub(crate) fn try_structurally_resolve(&mut self, t: Ty) -> Result<Ty, Stop> {
        let t = self.ty.shallow_resolve(t);
        if matches!(self.ty.view(t), TyView::Infer(_)) {
            self.select()?;
            return Ok(self.ty.shallow_resolve(t));
        }
        Ok(t)
    }

    /// `structurally_resolve_type`: as `try_structurally_resolve`, and a type variable left is
    /// E0282 (unless the body already has errors) and becomes the error type.
    pub(crate) fn structurally_resolve(&mut self, t: Ty, at: At) -> Result<Ty, Stop> {
        let t = self.try_structurally_resolve(t)?;
        if self.is_ty_var(t) {
            if self.errors == 0 {
                self.diag(DiagCode::AnnotationsNeeded, at, None, None, None);
            } else {
                self.errors += 1;
            }
            let err = self.common.err;
            self.unify(t, err)?;
            return Ok(err);
        }
        Ok(t)
    }

    // Snapshots: `DropGuard`s, so any early return rolls back.

    /// A snapshot of the whole run's undoable state (rustc's `InferCtxt` snapshot also covers
    /// the fulfilment context and, through `TypeckResults` being written only on success, the
    /// diagnostics a probe may not leave behind): the type context's (variables and pending
    /// obligations), and the length of every append-only column a rule writes. The read list
    /// is kept on rollback: a probe's failure was decided by what it read.
    pub(crate) fn snapshot(&mut self) -> MachineSnapshot<C::Snapshot> {
        MachineSnapshot {
            ty: self.ty.snapshot(),
            diags: self.diags.len(),
            errors: self.errors,
            adjust: self.adjust.len(),
            adj_scratch: self.adj_scratch.len(),
            callees: self.callees.len(),
            fields: self.fields.len(),
            captures: self.captures.len(),
            casts: self.casts.len(),
            numeric: self.numeric.len(),
            diverging: self.diverging.len(),
            closures: self.closures.len(),
        }
    }

    /// Undoes everything since `s`.
    pub(crate) fn rollback_to(&mut self, s: MachineSnapshot<C::Snapshot>) {
        self.ty.rollback_to(s.ty);
        self.diags.truncate(s.diags);
        self.errors = s.errors;
        self.adjust.truncate(s.adjust);
        self.adj_scratch.truncate(s.adj_scratch);
        self.callees.truncate(s.callees);
        self.fields.truncate(s.fields);
        self.captures.truncate(s.captures);
        self.casts.truncate(s.casts);
        self.numeric.truncate(s.numeric);
        self.diverging.truncate(s.diverging);
        self.closures.truncate(s.closures);
    }

    /// Runs `f` in a snapshot that is always rolled back (`probe`).
    pub(crate) fn probe<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let snap = self.snapshot();
        let mut g = DropGuard::new((self, snap), |(m, s)| m.rollback_to(s));
        f(&mut *g.0)
    }

    /// Runs `f` in a snapshot kept only when it returns `Ok(Some(_))` (`commit_if_ok`).
    pub(crate) fn commit_if<T, E>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<Option<T>, E>,
    ) -> Result<Option<T>, E> {
        let snap = self.snapshot();
        let mut g = DropGuard::new((self, snap), |(m, s)| m.rollback_to(s));
        match f(&mut *g.0) {
            Ok(Some(v)) => {
                let (m, s) = DropGuard::dismiss(g);
                m.ty.commit(s.ty);
                Ok(Some(v))
            }
            other => other,
        }
    }

    // Lowering portable types.

    /// Lowers `root` of a portable table, children first, with an explicit stack. An alias is
    /// normalized by a fresh variable and a projection obligation, as rustc normalizes a
    /// signature when it instantiates it.
    pub(crate) fn lower(&mut self, types: PTys<'_>, root: PTy, mode: Lower) -> Result<Ty, Stop> {
        let n = root.0 as usize + 1;
        if n > types.nodes.len() {
            return Err(CheckError::Malformed(Malformed::TypeOrder { node: root.0, child: root.0 }).into());
        }
        let mut memo: Vec<Option<Ty>, A> = Vec::with_capacity_in(n, self.alloc.clone());
        memo.resize(n, None);
        let mut stack: Vec<(u32, bool), A> = Vec::new_in(self.alloc.clone());
        let mut kids: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        stack.push((root.0, false));
        // Each node is expanded once and finished once; each list entry is pushed at most once
        // per expansion of its parent.
        let bound = 2 * (n + types.lists.len()) + 2;
        let mut steps = 0usize;
        while let Some((at, expanded)) = stack.pop() {
            steps += 1;
            if steps > bound {
                return Err(CheckError::Limit(Limit::Export).into());
            }
            if memo[at as usize].is_some() {
                continue;
            }
            let node = types.node(PTy(at))?;
            if !expanded {
                stack.push((at, true));
                kids.clear();
                children(node, types, &mut kids)?;
                for &c in kids.iter() {
                    if c.0 >= at {
                        return Err(CheckError::Malformed(Malformed::TypeOrder { node: at, child: c.0 }).into());
                    }
                    if memo[c.0 as usize].is_none() {
                        stack.push((c.0, false));
                    }
                }
                continue;
            }
            let ty = self.lower_node(node, types, &memo, mode)?;
            memo[at as usize] = Some(ty);
        }
        match memo[root.0 as usize] {
            Some(t) => Ok(t),
            None => Err(CheckError::Malformed(Malformed::TypeOrder { node: root.0, child: root.0 }).into()),
        }
    }

    fn lowered(memo: &[Option<Ty>], t: PTy) -> Result<Ty, Stop> {
        match memo.get(t.0 as usize).copied().flatten() {
            Some(t) => Ok(t),
            None => Err(CheckError::Malformed(Malformed::TypeOrder { node: t.0, child: t.0 }).into()),
        }
    }

    fn lowered_list(&mut self, memo: &[Option<Ty>], types: PTys<'_>, l: crate::ids::List) -> Result<TyList, Stop> {
        let src = types.list(l)?;
        let mut v: Vec<Ty, A> = Vec::with_capacity_in(src.len(), self.alloc.clone());
        for &t in src {
            v.push(Self::lowered(memo, t)?);
        }
        self.mk_list(&v)
    }

    fn lower_node(&mut self, node: PNode, types: PTys<'_>, memo: &[Option<Ty>], mode: Lower) -> Result<Ty, Stop> {
        let view = match node {
            PNode::Bool => TyView::Bool,
            PNode::Char => TyView::Char,
            PNode::Int(i) => TyView::Int(i),
            PNode::Uint(u) => TyView::Uint(u),
            PNode::Float(f) => TyView::Float(f),
            PNode::Str => TyView::Str,
            PNode::Never => TyView::Never,
            PNode::Tuple(l) => TyView::Tuple(self.lowered_list(memo, types, l)?),
            PNode::Ref(m, t) => TyView::Ref(m, Self::lowered(memo, t)?),
            PNode::Ptr(m, t) => TyView::Ptr(m, Self::lowered(memo, t)?),
            PNode::Array(t, n) => TyView::Array(Self::lowered(memo, t)?, n),
            PNode::Slice(t) => TyView::Slice(Self::lowered(memo, t)?),
            PNode::Adt(k, l) => TyView::Adt(k, self.lowered_list(memo, types, l)?),
            PNode::FnDef(k, l) => TyView::FnDef(k, self.lowered_list(memo, types, l)?),
            PNode::FnPtr(l) => TyView::FnPtr(self.lowered_list(memo, types, l)?),
            PNode::Closure(e, sig) => TyView::Closure(e, Self::lowered(memo, sig)?),
            PNode::Param(i) => match mode {
                Lower::Args(args) => {
                    let len = self.ty.list_len(args);
                    if i >= len {
                        return Err(CheckError::Malformed(Malformed::Param { index: i, args: len }).into());
                    }
                    return Ok(self.ty.list_get(args, i));
                }
                Lower::Body => TyView::Param(i),
            },
            PNode::Alias { trait_, name, args } => {
                let args = self.lowered_list(memo, types, args)?;
                let term = self.fresh()?;
                self.ty.register(Obligation {
                    goal: Goal::Projection { trait_, args, name, term },
                    cause: Cause::Body,
                })?;
                return Ok(term);
            }
            PNode::Opaque(k, l) => TyView::Opaque(k, self.lowered_list(memo, types, l)?),
            PNode::Infer => match mode {
                Lower::Body => return self.fresh(),
                Lower::Args(_) => return Err(CheckError::Malformed(Malformed::HoleInAnswer).into()),
            },
            PNode::Error => TyView::Error,
        };
        self.mk(view)
    }

    /// A written type of the body.
    pub(crate) fn lower_body_ty(&mut self, t: PTy) -> Result<Ty, Stop> {
        let types = self.body.types;
        self.lower(types, t, Lower::Body)
    }

    /// A list of a world's answer, lowered with `args`.
    pub(crate) fn lower_list(&mut self, types: PTys<'_>, l: crate::ids::List, args: TyList) -> Result<TyList, Stop> {
        let src = types.list(l)?;
        let mut v: Vec<Ty, A> = Vec::with_capacity_in(src.len(), self.alloc.clone());
        for &t in src {
            v.push(self.lower(types, t, Lower::Args(args))?);
        }
        self.mk_list(&v)
    }

    /// Registers `item`'s where-clauses, instantiated with `args`
    /// (`add_required_obligations_for_hir`).
    pub(crate) fn register_predicates(&mut self, item: ItemKey, args: TyList, cause: Cause) -> Result<(), Stop> {
        let preds = self.reads.predicates(item)?;
        for clause in preds.clauses {
            let goal = match *clause {
                PClause::Trait { trait_, args: l } => {
                    Goal::Trait { trait_, args: self.lower_list(preds.types, l, args)? }
                }
                PClause::Projection { trait_, args: l, name, term } => Goal::Projection {
                    trait_,
                    args: self.lower_list(preds.types, l, args)?,
                    name,
                    term: self.lower(preds.types, term, Lower::Args(args))?,
                },
            };
            self.ty.register(Obligation { goal, cause })?;
        }
        Ok(())
    }

    /// Registers `args[0]: trait_<args[1..]>`.
    pub(crate) fn register_trait(&mut self, trait_: ItemKey, args: TyList, cause: Cause) -> Result<(), Stop> {
        self.ty.register(Obligation { goal: Goal::Trait { trait_, args }, cause })?;
        Ok(())
    }

    /// `<args[0] as trait_<args[1..]>>::name` as a fresh variable and a projection obligation.
    pub(crate) fn project(&mut self, trait_: ItemKey, args: TyList, name: Name, cause: Cause) -> Result<Ty, Stop> {
        let term = self.fresh()?;
        self.ty.register(Obligation { goal: Goal::Projection { trait_, args, name, term }, cause })?;
        Ok(term)
    }

    pub(crate) fn lang(&mut self, item: LangItem) -> Result<Option<ItemKey>, Stop> {
        self.reads.lang_item(item)
    }

    /// The impl-lookup shape of `t`, or `None` for a variable or the error type.
    pub(crate) fn shape(&mut self, t: Ty) -> Option<Shape> {
        Some(match self.view(t) {
            TyView::Bool => Shape::Bool,
            TyView::Char => Shape::Char,
            TyView::Int(i) => Shape::Int(i),
            TyView::Uint(u) => Shape::Uint(u),
            TyView::Float(f) => Shape::Float(f),
            TyView::Str => Shape::Str,
            TyView::Never => Shape::Never,
            TyView::Tuple(l) => Shape::Tuple(self.ty.list_len(l)),
            TyView::Ref(m, _) => Shape::Ref(m),
            TyView::Ptr(m, _) => Shape::Ptr(m),
            TyView::Array(..) => Shape::Array,
            TyView::Slice(_) => Shape::Slice,
            TyView::Adt(k, _) => Shape::Adt(k),
            TyView::FnDef(k, _) => Shape::FnDef(k),
            TyView::FnPtr(l) => Shape::FnPtr(self.ty.list_len(l).saturating_sub(1)),
            TyView::Closure(..) => Shape::Closure,
            TyView::Param(_) => Shape::Param,
            TyView::Opaque(k, _) => Shape::Opaque(k),
            TyView::Alias { .. } | TyView::Infer(_) | TyView::Error => return None,
        })
    }

    /// The autoderef steps of `base` (rustc's `Autoderef`): `base` first, then each builtin or
    /// overloaded deref, until a type that does not deref, a variable, or the recursion limit
    /// (the second value: E0055 when true).
    pub(crate) fn autoderef(&mut self, base: Ty, raw_ptrs: bool) -> Result<(Vec<Step, A>, bool), Stop> {
        let mut steps: Vec<Step, A> = Vec::new_in(self.alloc.clone());
        let mut cur = self.try_structurally_resolve(base)?;
        steps.push(Step { ty: cur, overloaded: false });
        for _ in 0..RECURSION_LIMIT {
            let (next, overloaded) = match self.view(cur) {
                TyView::Ref(_, t) => (t, false),
                TyView::Ptr(_, t) if raw_ptrs => (t, false),
                TyView::Adt(..) | TyView::Param(_) | TyView::Opaque(..) => {
                    match self.overloaded_deref(cur)? {
                        Some(t) => (t, true),
                        None => return Ok((steps, false)),
                    }
                }
                _ => return Ok((steps, false)),
            };
            cur = self.try_structurally_resolve(next)?;
            steps.push(Step { ty: cur, overloaded });
        }
        Ok((steps, true))
    }

    /// `<t as Deref>::Target`, if `t: Deref` may hold and selects now.
    pub(crate) fn overloaded_deref(&mut self, t: Ty) -> Result<Option<Ty>, Stop> {
        let Some(deref) = self.lang(LangItem::Deref)? else {
            return Ok(None);
        };
        let args = self.mk_list(&[t])?;
        if !self.ty.may_hold(Goal::Trait { trait_: deref, args }, &mut self.reads)? {
            return Ok(None);
        }
        self.ty.normalize(deref, args, Name::of("Target"), &mut self.reads)
    }

    /// Records the adjustments of an autoderef to step `n` of `steps` on `e`.
    pub(crate) fn adjust_autoderef(&mut self, e: ExprId, steps: &[Step], n: usize) {
        for s in steps.iter().take(n + 1).skip(1) {
            let kind = AdjustKind::Deref { overloaded: if s.overloaded { Some(Mutbl::Not) } else { None } };
            self.adjust.push((e, kind, s.ty));
        }
    }

    /// The range of an `At`.
    pub(crate) fn range_of(&self, at: At) -> crate::ids::Range {
        match at {
            At::Expr(e) => self.body.expr_range(e),
            At::Pat(p) => self.body.pat_range(p),
            At::Body => crate::ids::Range::default(),
        }
    }
}

/// The children of a node, in order.
fn children(node: PNode, types: PTys<'_>, out: &mut Vec<PTy, Alloc<'_>>) -> Result<(), CheckError> {
    match node {
        PNode::Tuple(l)
        | PNode::Adt(_, l)
        | PNode::FnDef(_, l)
        | PNode::FnPtr(l)
        | PNode::Opaque(_, l)
        | PNode::Alias { args: l, .. } => out.extend_from_slice(types.list(l)?),
        PNode::Ref(_, t) | PNode::Ptr(_, t) | PNode::Array(t, _) | PNode::Slice(t) | PNode::Closure(_, t) => {
            out.push(t);
        }
        PNode::Bool
        | PNode::Char
        | PNode::Int(_)
        | PNode::Uint(_)
        | PNode::Float(_)
        | PNode::Str
        | PNode::Never
        | PNode::Param(_)
        | PNode::Infer
        | PNode::Error => {}
    }
    Ok(())
}
