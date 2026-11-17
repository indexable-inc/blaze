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
//! and the closure kind (`Fn`/`FnMut`/`FnOnce`, which follows from the captures).
//!
//! An `async` closure returns a future of its body per call (`open_async_closure`; the future
//! itself is rules/coroutine.rs's).

use blaze_own::{Alloc, Vec};

use crate::body::{Expr, Pat, Stmt, UnOp};
use crate::columns::CaptureKind;
use crate::diag::At;
use crate::error::{CheckError, Limit, Stop};
use crate::ids::{ExprId, FnPtrHeader, ItemKey, LangItem, List, Mutbl, Name, PatId};
use crate::machine::{Expectation, Flow, Frame, Machine, PendingClosure, Then};
use crate::portable::PTy;
use crate::ty::{Ty, TyList, TyView, TypeCtx, Var, VarKind};
use crate::world::World;

/// A capture walk's usage: the expression's value is taken (moved, or copied when `Copy`).
pub(super) const USE_MOVE: u8 = 2;

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// The expected signature: inputs, and the output if known.
    fn deduce_sig(&mut self, ex: Expectation) -> Result<Option<(TyList, Option<Ty>)>, Stop> {
        let Some(t) = ex.to_option() else { return Ok(None) };
        let t = self.ty.shallow_resolve(t);
        match self.view(t) {
            // rustc `deduce_closure_signature`: any fn pointer's inputs and output (its header
            // is not the closure's: a closure is a safe Rust fn).
            TyView::FnPtr(l, _) => {
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
            // rustc `deduce_closure_signature` on `ty::Dynamic`: a closure expected as
            // `dyn FnMut(&T) + Send` (through `&`, or as `Box::new`'s argument hint:
            // rules/expect_inputs.rs) takes the object's `Fn*` signature.
            TyView::Dynamic(rows) => {
                let (Some(fn_), Some(fn_mut), Some(fn_once)) =
                    (self.lang(LangItem::Fn)?, self.lang(LangItem::FnMut)?, self.lang(LangItem::FnOnce)?)
                else {
                    return Ok(None);
                };
                Ok(self.object_closure_sig(rows, [fn_, fn_mut, fn_once], fn_once))
            }
            _ => Ok(None),
        }
    }

    /// The signature a trait object (its predicate rows `rows`: an `ExTrait` principal, then
    /// `ExProj`s, then `ExAuto`s) gives a closure expected as it. rustc's
    /// `deduce_sig_from_projection` on the object's projection bounds: the `FnOnce<Args>::Output
    /// = R` row (library/core/src/ops/function.rs: `trait FnOnce<Args: Tuple> { type Output; }`)
    /// gives the inputs `Args` (a tuple) and the output `R`. With no such row, an `Fn*`
    /// principal's `Args` gives the inputs alone, as a variable's pending `Fn*` goal does
    /// (`closure_hint`). `None` when no row names an `Fn*` trait at a tuple.
    fn object_closure_sig(&mut self, rows: TyList, fn_traits: [ItemKey; 3], fn_once: ItemKey) -> Option<(TyList, Option<Ty>)> {
        let output_name = Name::of("Output");
        let mut from_principal = None;
        // Bounded by the object's rows.
        for i in 0..self.ty.list_len(rows) {
            let row = self.ty.list_get(rows, i);
            match self.view(row) {
                TyView::ExProj { trait_, name, args_term }
                    if trait_ == fn_once && name == output_name && self.ty.list_len(args_term) == 2 =>
                {
                    let args = self.ty.shallow_resolve(self.ty.list_get(args_term, 0));
                    if let TyView::Tuple(inputs) = self.view(args) {
                        return Some((inputs, Some(self.ty.list_get(args_term, 1))));
                    }
                }
                TyView::ExTrait(k, args) if fn_traits.contains(&k) && self.ty.list_len(args) == 1 => {
                    let a = self.ty.shallow_resolve(self.ty.list_get(args, 0));
                    if let TyView::Tuple(inputs) = self.view(a) {
                        from_principal = Some((inputs, None));
                    }
                }
                _ => {}
            }
        }
        from_principal
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
        is_async: bool,
    ) -> Result<Flow, Stop> {
        if f.state == 0 {
            if is_async {
                return self.open_async_closure(f, e, params, param_tys, ret, body);
            }
            let pats = self.body.pat_list(params)?;
            let expected = self.deduce_sig(f.expect)?;
            let mut inputs = self.closure_inputs(pats, param_tys, expected)?;
            let output = match (ret, expected.and_then(|(_, o)| o)) {
                (Some(r), _) => self.lower_body_ty(r, At::Expr(e))?,
                (None, Some(o)) => o,
                (None, None) => self.fresh()?,
            };
            inputs.push(output);
            let sig_list = self.mk_list(&inputs)?;
            let sig = self.mk(TyView::FnPtr(sig_list, FnPtrHeader::RUST))?;
            f.t = self.mk(TyView::Closure(e, sig))?;
            f.u = output;
            f.a = self.new_coerce_many(output)?;
            f.b = u32::from(self.diverges);
            self.ret_sites.push(f.a);
            self.async_sites.push(false);
            self.closures.push(PendingClosure { expr: e });
            self.resume(f, 1)?;
            self.push_expr(body, Expectation::HasType(output), Then::CoerceMany(f.a))?;
            for (i, &p) in pats.iter().enumerate().rev() {
                self.push_pat(p, inputs[i], 0)?;
            }
            return Ok(Flow::Wait);
        }
        self.ret_sites.pop();
        self.async_sites.pop();
        let actual = self.coerce_many_complete(f.a);
        if !self.is_never(actual) && !self.is_err(actual) {
            self.demand_eq(At::Expr(body), f.u, actual)?;
        }
        self.diverges = f.b != 0;
        Ok(Flow::Done(f.t))
    }

    /// A closure's parameter types: the written one (unified with the expected one when the
    /// arity matches), else the expected one, else a fresh variable. Capacity for the output.
    fn closure_inputs(
        &mut self,
        pats: &[PatId],
        param_tys: List,
        expected: Option<(TyList, Option<Ty>)>,
    ) -> Result<Vec<Ty, Alloc<'a>>, Stop> {
        let written = self.body.opt_type_list(param_tys)?;
        let arity_ok = expected.is_some_and(|(l, _)| self.ty.list_len(l) as usize == pats.len());
        let mut inputs: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(pats.len() + 1, self.alloc);
        for i in 0..pats.len() {
            let exp = match expected {
                Some((l, _)) if arity_ok => Some(self.ty.list_get(l, i as u32)),
                _ => None,
            };
            let t = match (written.get(i).copied().flatten(), exp) {
                (Some(w), Some(x)) => {
                    let w = self.lower_body_ty(w, At::Pat(pats[i]))?;
                    self.demand_eq(At::Pat(pats[i]), x, w)?;
                    w
                }
                (Some(w), None) => self.lower_body_ty(w, At::Pat(pats[i]))?,
                (None, Some(x)) => x,
                (None, None) => self.fresh()?,
            };
            inputs.push(t);
        }
        Ok(inputs)
    }

    /// `async |..| body` (the Reference, "Closure expressions": an async closure's call returns
    /// a future of its body): a closure whose output is `Future(e, body_out, interior)`, the
    /// body checked against `body_out` as its own return site and async site. A written `-> T`
    /// is the future's output. Its frame ends in `expr_closure`'s second state: `f.u` is the
    /// body's output there.
    fn open_async_closure(
        &mut self,
        mut f: Frame,
        e: ExprId,
        params: List,
        param_tys: List,
        ret: Option<PTy>,
        body: ExprId,
    ) -> Result<Flow, Stop> {
        let pats = self.body.pat_list(params)?;
        let expected = self.deduce_sig(f.expect)?;
        let mut inputs = self.closure_inputs(pats, param_tys, expected)?;
        let body_out = match ret {
            Some(r) => self.lower_body_ty(r, At::Expr(e))?,
            None => self.fresh()?,
        };
        // Captures are not told apart by kind here: `Ref` keeps both `T` and `&T` in the interior.
        let fut = self.open_future(e, body, body_out, crate::body::Capture::Ref)?;
        if let Some((_, Some(o))) = expected {
            self.demand_eq(At::Expr(e), o, fut)?;
        }
        inputs.push(fut);
        let sig_list = self.mk_list(&inputs)?;
        let sig = self.mk(TyView::FnPtr(sig_list, FnPtrHeader::RUST))?;
        f.t = self.mk(TyView::Closure(e, sig))?;
        f.u = body_out;
        f.a = self.new_coerce_many(body_out)?;
        f.b = u32::from(self.diverges);
        self.ret_sites.push(f.a);
        self.closures.push(PendingClosure { expr: e });
        self.resume(f, 1)?;
        self.push_expr(body, Expectation::HasType(body_out), Then::CoerceMany(f.a))?;
        for (i, &p) in pats.iter().enumerate().rev() {
            self.push_pat(p, inputs[i], 0)?;
        }
        Ok(Flow::Wait)
    }

    /// `closure_analyze`: the captures of every closure, innermost first as the walk met them.
    pub(crate) fn analyze_closures(&mut self) -> Result<(), Stop> {
        let mut bound: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(self.body.pats.len(), self.alloc);
        let mut exprs: Vec<(ExprId, u8), Alloc<'a>> = Vec::new_in(self.alloc);
        let mut pats: Vec<PatId, Alloc<'a>> = Vec::new_in(self.alloc);
        for ci in 0..self.closures.len() {
            let closure = self.closures[ci].expr;
            self.walk_captures(closure, &mut bound, &mut exprs, &mut pats)?;
        }
        // One row per (closure, local), the strongest use kept (rule 25: sorted).
        self.captures.sort_unstable_by_key(|&(c, p, k)| (c, p, core::cmp::Reverse(k as u8)));
        self.captures.dedup_by_key(|&mut (c, p, _)| (c, p));
        // Each closure's kind from its strongest USE of a capture (upvar.rs `closure_kind`): a
        // value moved out makes it `FnOnce`, a mutable use `FnMut`, else `Fn`, whatever the
        // capture modes (`move` changes the modes, not the kind).
        for ci in 0..self.closures.len() {
            let closure = self.closures[ci].expr;
            let mut kind = crate::ty::ClosureKind::Fn;
            for &(c, k) in self.capture_uses.iter() {
                if c == closure {
                    kind = kind.max(k);
                }
            }
            self.ty.settle_closure_kind(closure, kind)?;
            self.closure_kinds.push((closure, kind));
        }
        Ok(())
    }

    /// Pushes onto `captures` one row per use of a local of an enclosing body in `closure`'s
    /// body: a walk of its syntax alone (rustc's `upvars_mentioned`), so it can run before
    /// the closure's types are known.
    fn walk_captures(
        &mut self,
        closure: ExprId,
        bound: &mut Vec<bool, Alloc<'a>>,
        exprs: &mut Vec<(ExprId, u8), Alloc<'a>>,
        pats: &mut Vec<PatId, Alloc<'a>>,
    ) -> Result<(), Stop> {
        let Expr::Closure { params, body, .. } = self.body.expr(closure)? else { return Ok(()) };
        bound.clear();
        bound.resize(self.body.pats.len(), false);
        pats.clear();
        pats.extend_from_slice(self.body.pat_list(params)?);
        self.mark_bound(pats, bound)?;
        exprs.clear();
        // The u8 is how the expression is used: 0 read, 1 assigned or mutably borrowed,
        // `USE_MOVE` taken by value (the closure's result is returned: by value).
        exprs.push((body, USE_MOVE));
        let mut steps = 0usize;
        let bound_steps = self.body.exprs.len() + self.body.stmts.len() + 1;
        while let Some((x, usage)) = exprs.pop() {
            steps += 1;
            if steps > bound_steps {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            self.visit_for_captures(closure, x, usage, exprs, pats, bound)?;
        }
        Ok(())
    }

    /// Whether `closure` captures no variable (the Reference, "Type coercions", "Coercion
    /// types": "Non capturing closures to `fn` pointers"): its body names no local of an
    /// enclosing body. The rows the walk finds are dropped: `analyze_closures` records them.
    pub(crate) fn captures_nothing(&mut self, closure: ExprId) -> Result<bool, Stop> {
        let mut bound: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(self.body.pats.len(), self.alloc);
        let mut exprs: Vec<(ExprId, u8), Alloc<'a>> = Vec::new_in(self.alloc);
        let mut pats: Vec<PatId, Alloc<'a>> = Vec::new_in(self.alloc);
        let before = self.captures.len();
        let uses_before = self.capture_uses.len();
        self.walk_captures(closure, &mut bound, &mut exprs, &mut pats)?;
        let none = self.captures.len() == before;
        self.captures.truncate(before);
        self.capture_uses.truncate(uses_before);
        Ok(none)
    }

    pub(super) fn visit_for_captures(
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
                    // The Reference, "Closure types", "Capture modes": a `move` closure captures
                    // every place by value; otherwise by the least mode its uses need, a value
                    // moved out (not `Copy`) by value.
                    // The closure's kind follows the use, not the mode ("Closure types": a
                    // `move` closure that only reads its captures is still `Fn`).
                    // An async block's rows are found by this walk too (rules/async_fn.rs).
                    let is_move = self.captures_by_move(closure)?;
                    // A block's rows ask `Copy` only where the answer changes the mode, and
                    // never as a finding (`block_moved_out`): a body that was clean stays so.
                    let of_block = matches!(self.body.expr(closure)?, Expr::AsyncBlock { .. });
                    let moved = usage == USE_MOVE
                        && if of_block { !is_move && self.block_moved_out(x)? } else { self.moved_out(x)? };
                    let kind = if is_move || moved {
                        CaptureKind::ByValue
                    } else if usage == 1 {
                        CaptureKind::ByMutRef
                    } else {
                        CaptureKind::ByRef
                    };
                    let needs = if moved {
                        crate::ty::ClosureKind::FnOnce
                    } else if usage == 1 {
                        crate::ty::ClosureKind::FnMut
                    } else {
                        crate::ty::ClosureKind::Fn
                    };
                    self.captures.push((closure, p, kind));
                    self.capture_uses.push((closure, needs));
                }
            }
            Expr::Missing | Expr::Lit(_) | Expr::Path(_) | Expr::Continue { .. } => {}
            Expr::Paren(i) => exprs.push((i, usage)),
            Expr::Block { stmts, tail, .. } => {
                if let Some(t) = tail {
                    exprs.push((t, if usage == USE_MOVE { USE_MOVE } else { 0 }));
                }
                for &s in self.body.stmt_list(stmts)? {
                    match self.body.stmt(s)? {
                        Stmt::Let { pat, init, else_, .. } => {
                            pats.clear();
                            pats.push(pat);
                            self.mark_bound(pats, bound)?;
                            if let Some(i) = init {
                                exprs.push((i, USE_MOVE));
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
            Expr::Let { pat, init } => {
                pats.clear();
                pats.push(pat);
                self.mark_bound(pats, bound)?;
                exprs.push((init, 0));
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
                    exprs.push((v, USE_MOVE));
                }
            }
            Expr::Call { callee, args } => {
                exprs.push((callee, 0));
                for &a in el(args)? {
                    exprs.push((a, USE_MOVE));
                }
            }
            Expr::MethodCall { receiver, args, .. } => {
                exprs.push((receiver, self.receiver_usage(x)));
                for &a in el(args)? {
                    exprs.push((a, USE_MOVE));
                }
            }
            // A field read is a place use of its base (moving a field out is not yet told
            // apart: rustc captures the field's place by value, we capture the base by ref).
            Expr::Field { base, .. } | Expr::TupleField { base, .. } => {
                exprs.push((base, if usage == USE_MOVE { 0 } else { usage }))
            }
            Expr::Unary { op, operand } => exprs.push((operand, if op == UnOp::Deref { usage } else { 0 })),
            Expr::Binary { lhs, rhs, .. } => {
                exprs.push((lhs, 0));
                exprs.push((rhs, 0));
            }
            Expr::Assign { lhs, rhs } | Expr::AssignOp { lhs, rhs, .. } => {
                exprs.push((lhs, 1));
                exprs.push((rhs, 0));
            }
            Expr::Ref { mutbl, operand } | Expr::RawRef { mutbl, operand } => {
                exprs.push((operand, u8::from(mutbl == Mutbl::Mut)));
            }
            // An inline constant sees no local of the body: it captures nothing.
            Expr::ConstBlock { .. } => {}
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
                exprs.push((base, if usage == USE_MOVE { 0 } else { usage }));
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
            // Each argument is borrowed shared: a read.
            Expr::FormatArgs { args, .. } => {
                for &a in self.body.expr_list(args)? {
                    exprs.push((a, 0));
                }
            }
            // `.await` takes its operand by value.
            Expr::Await(operand) => exprs.push((operand, 0)),
            Expr::AsyncBlock { body, .. } => exprs.push((body, 0)),
            Expr::Underscore | Expr::OffsetOf { .. } => {}
            // An output writes its place (as an assignment's left side does); the rest read.
            Expr::Asm { operands, .. } => {
                for o in self.body.asm_operand_list(operands)? {
                    exprs.push((o.expr, u8::from(o.kind.is_output() && o.out_expr.is_none())));
                    if let Some(x) = o.out_expr {
                        exprs.push((x, 1));
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether a use of a local taken by value at `x` moves it: no autoref borrows it there
    /// and its type is not `Copy` (the Reference, "Capture modes": a moved non-`Copy` value is
    /// captured by value). An unknown type (`Copy` may hold) is not a move; with no `Copy`
    /// lang item in the world no type is known `Copy`-free, so nothing moves.
    pub(super) fn moved_out(&mut self, x: ExprId) -> Result<bool, Stop> {
        let exprs = self.adjust.column::<0>();
        let kinds = self.adjust.column::<1>();
        if exprs.iter().zip(kinds).any(|(&e, k)| e == x && matches!(k, crate::columns::AdjustKind::Borrow { .. })) {
            return Ok(false);
        }
        // Without `Copy` nothing can be told copied: the finding names the lang item (the body
        // is tainted), and the use counts as a move, the Reference's case for a non-`Copy` place.
        let Some(copy) = self.need_lang(LangItem::Copy, At::Expr(x))? else { return Ok(true) };
        let t = self.expr_ty[x.ix()];
        let t = self.ty.resolve(t)?;
        let args = self.mk_list(&[t])?;
        Ok(!self.ty.may_hold(crate::ty::Goal::Trait { trait_: copy, args }, &mut self.reads)?)
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
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Or { alts: elems } | Pat::Slice { elems, .. } => {
                    stack.extend_from_slice(self.body.pat_list(elems)?);
                }
                Pat::Struct { fields, .. } => {
                    for fp in self.body.field_pat_list(fields)? {
                        stack.push(fp.pat);
                    }
                }
                Pat::Missing
                | Pat::Wild
                | Pat::Bind { sub: None, .. }
                | Pat::Lit(_)
                | Pat::NegLit(_)
                | Pat::Path(_)
                | Pat::Range { .. } => {}
            }
        }
        Ok(())
    }
}
