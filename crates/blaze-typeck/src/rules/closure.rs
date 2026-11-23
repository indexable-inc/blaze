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
//! What is captured is the captured PLACES (capture_places.rs, RFC 2229): each owner's places
//! decide its whole-binding rows (`Checked::captures`, one per captured binding in its
//! strongest mode, what the layout still holds), its kind, and the environment tuple the
//! solver reads (`closure_upvars`). Where the places are a gap, the walk below
//! (`visit_for_captures`, `whole_capture_witness`) still files the rows and proves what it can
//! from whole-binding uses alone.
//!
//! An `async` closure returns a future of its body per call (`open_async_closure`; the future
//! itself is rules/coroutine.rs's).

use blaze_own::{Alloc, Vec};

use crate::body::{BinOp, Capture, Expr, Pat, Stmt, UnOp};
use crate::columns::{CaptureKind, SigFrom};
use crate::diag::At;
use crate::error::{CheckError, Limit, Stop};
use crate::ids::{ExprId, FnPtrHeader, ItemKey, LangItem, List, Mutbl, Name, PatId};
use crate::machine::{Expectation, Flow, Frame, Machine, PendingClosure, Then};
use crate::portable::PTy;
use crate::ty::{Cause, Ty, TyList, TyView, TypeCtx, Var, VarKind};
use crate::world::World;

/// A capture walk's usage: the expression's value is taken (moved, or copied when `Copy`).
pub(super) const USE_MOVE: u8 = 2;

/// Rows already computed for an inner owner. Only a proven whole-place
/// capture can establish a whole-place capture in its parent.
struct InnerCaptures {
    rows: core::ops::Range<usize>,
    whole: bool,
}

/// A callable trait's method and return projection. A synchronous call projects
/// `FnOnce::Output`. An async call returns the future for its receiver's capture mode.
pub(super) struct CallMethod {
    pub(super) trait_: LangItem,
    pub(super) method: &'static str,
    pub(super) borrow: Option<Mutbl>,
    pub(super) owner: LangItem,
    pub(super) result: &'static str,
}

pub(super) const CALL_METHODS: [CallMethod; 6] = {
    use LangItem::{AsyncFn, AsyncFnMut, AsyncFnOnce, Fn, FnMut, FnOnce};
    [
        CallMethod { trait_: Fn, method: "call", borrow: Some(Mutbl::Not), owner: FnOnce, result: "Output" },
        CallMethod { trait_: FnMut, method: "call_mut", borrow: Some(Mutbl::Mut), owner: FnOnce, result: "Output" },
        CallMethod { trait_: FnOnce, method: "call_once", borrow: None, owner: FnOnce, result: "Output" },
        CallMethod { trait_: AsyncFn, method: "async_call", borrow: Some(Mutbl::Not), owner: AsyncFnMut, result: "CallRefFuture" },
        CallMethod { trait_: AsyncFnMut, method: "async_call_mut", borrow: Some(Mutbl::Mut), owner: AsyncFnMut, result: "CallRefFuture" },
        CallMethod { trait_: AsyncFnOnce, method: "async_call_once", borrow: None, owner: AsyncFnOnce, result: "CallOnceFuture" },
    ]
};

/// Where a pending obligation's expectation came from (D148, [`SigFrom`]): a clause of a use
/// when a declared bound filed it (`Cause::Clause`), with the output's clause when the output's
/// projection was filed by the same use; any other cause is an obligation the body made itself
/// (a call of a value of unknown type), which no head's column describes: every region is the
/// maker's (`Inferred`).
fn sig_from_causes(inputs: Cause, output: Option<Cause>) -> SigFrom {
    match inputs {
        Cause::Clause { expr, nth } if nth != u32::MAX => SigFrom::Clause {
            expr,
            nth,
            output: match output {
                Some(Cause::Clause { expr: at, nth: k }) if at == expr && k != u32::MAX => Some(k),
                _ => None,
            },
        },
        _ => SigFrom::Inferred,
    }
}

/// An expected closure signature: the inputs, the output if known, and where it came from.
#[derive(Clone, Copy)]
struct Expected {
    inputs: TyList,
    output: Option<Ty>,
    from: SigFrom,
}

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// The expected signature: inputs, and the output if known, and where it came from.
    fn deduce_sig(&mut self, ex: Expectation) -> Result<Option<Expected>, Stop> {
        let Some(t) = ex.to_option() else { return Ok(None) };
        let t = self.ty.shallow_resolve(t);
        match self.view(t) {
            // rustc `deduce_closure_signature`: any fn pointer's inputs and output (its header
            // is not the closure's: a closure is a safe Rust fn).
            TyView::FnPtr(l, _) => {
                let (inputs, output) = self.fn_ptr_sig(l)?;
                Ok(Some(Expected { inputs, output: Some(output), from: SigFrom::FnPtr }))
            }
            TyView::Infer(Var { kind: VarKind::Ty, .. }) => {
                let (Some(fn_), Some(fn_mut), Some(fn_once)) =
                    (self.lang(LangItem::Fn)?, self.lang(LangItem::FnMut)?, self.lang(LangItem::FnOnce)?)
                else {
                    return Ok(None);
                };
                Ok(self.ty.closure_hint(t, [fn_, fn_mut, fn_once], Name::of("Output"))?.map(|((inputs, ic), out)| Expected {
                    inputs,
                    output: out.map(|(o, _)| o),
                    from: sig_from_causes(ic, out.map(|(_, oc)| oc)),
                }))
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
                Ok(self.object_closure_sig(rows, [fn_, fn_mut, fn_once], fn_once).map(|(inputs, output)| Expected { inputs, output, from: SigFrom::Object }))
            }
            _ => Ok(None),
        }
    }

    /// `AsyncFnOnce<Args>::Output` is the output of the *future* returned by
    /// the call. Its arguments still describe the closure's own parameters.
    fn async_callable_hint(&mut self, ex: Expectation) -> Result<Option<Expected>, Stop> {
        let Some(t) = ex.to_option() else { return Ok(None) };
        let t = self.ty.shallow_resolve(t);
        if !matches!(self.view(t), TyView::Infer(Var { kind: VarKind::Ty, .. })) {
            return Ok(None);
        }
        let Some(once) = self.lang(LangItem::AsyncFnOnce)? else { return Ok(None) };
        let shared = self.lang(LangItem::AsyncFn)?;
        let mutable = self.lang(LangItem::AsyncFnMut)?;
        // Absent lang items cannot have a matching obligation. Include the
        // available members even in a world declaring just AsyncFnOnce.
        Ok(self.ty.closure_hint(t, [shared.unwrap_or(once), mutable.unwrap_or(once), once], Name::of("Output"))?.map(|((inputs, ic), out)| Expected {
            inputs,
            output: out.map(|(o, _)| o),
            from: sig_from_causes(ic, out.map(|(_, oc)| oc)),
        }))
    }

    /// Where closure `e`'s signature came from ([`SigFrom`]): the expectation it took, else
    /// `Written` when every parameter's type and the return type are written, else `Inferred`.
    fn sig_from(&self, expected: Option<Expected>, param_tys: List, n: usize, ret: Option<PTy>) -> Result<SigFrom, Stop> {
        if let Some(x) = expected {
            return Ok(x.from);
        }
        let written = self.body.opt_type_list(param_tys)?;
        let all = ret.is_some() && written.len() == n && written.iter().all(|w| w.is_some());
        Ok(if all { SigFrom::Written } else { SigFrom::Inferred })
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
            let expected = match self.deduce_sig(f.expect)? {
                some @ Some(_) => some,
                // A regular closure may implement AsyncFnOnce by returning a future.
                // Its body produces the future, so only the input hint applies here.
                // Its output clause is the FUTURE's output, not the closure's return type: the
                // clause names its inputs only.
                None => self.async_callable_hint(f.expect)?.map(|x| Expected {
                    output: None,
                    from: match x.from {
                        SigFrom::Clause { expr, nth, .. } => SigFrom::Clause { expr, nth, output: None },
                        other => other,
                    },
                    ..x
                }),
            };
            let sig_from = self.sig_from(expected, param_tys, pats.len(), ret)?;
            let expected = expected.map(|x| (x.inputs, x.output));
            let mut inputs = self.closure_inputs(e, pats, param_tys, expected)?;
            // A sized rvalue hint (an async block's destination output: rules/coroutine.rs) is
            // non-reporting: the output stays free and is equated after the body. An expected
            // trait object (`Box::new(|_| ..)` expected as `Box<dyn Fn(&E) -> Box<dyn H>>`) is
            // rustc's `deduce_closure_signature` on `ty::Dynamic`, whose output IS the closure's
            // return type (`sig_of_closure_with_expectation` equates the supplied return type
            // with the expected one), so the body is a coercion site to it (the Reference,
            // "Coercion sites": a closure's body is a function body): `Box::new(Handler)`
            // there unsizes to `Box<dyn H>`. Free, it was `Box<Handler>`, then a false
            // "found `Box<Handler>`, expected `Box<dyn H>`" (tempo's crates/eyre lib.rs:42,
            // `eyre::set_hook(Box::new(|_| Box::new(ErrorHandler)))`).
            let hinted_output = match f.expect {
                Expectation::RvalueLikeUnsized(t) if ret.is_none() => {
                    let t = self.ty.shallow_resolve(t);
                    if matches!(self.view(t), TyView::Dynamic(_)) { None } else { expected.and_then(|(_, o)| o) }
                }
                _ => None,
            };
            let output = match (ret, expected.and_then(|(_, o)| o), hinted_output) {
                (Some(r), _, _) => self.lower_body_ty(r, At::Expr(e))?,
                (None, Some(o), None) => o,
                (None, _, Some(_)) | (None, None, None) => self.fresh_from(crate::diag::VarOrigin::ClosureReturn { closure: e })?,
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
            self.closures.push(PendingClosure { expr: e, sig_from });
            f.l = match hinted_output {
                Some(hint) => self.mk_list(&[hint])?,
                None => self.common.empty,
            };
            self.resume(f, 1)?;
            let body_expect = if hinted_output.is_some() { Expectation::None } else { Expectation::HasType(output) };
            self.push_expr(body, body_expect, Then::CoerceMany(f.a))?;
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
            if self.ty.list_len(f.l) != 0 {
                self.demand_eq(At::Expr(e), self.ty.list_get(f.l, 0), actual)?;
            }
        }
        self.diverges = f.b != 0;
        Ok(Flow::Done(f.t))
    }

    /// A closure's parameter types: the written one (unified with the expected one when the
    /// arity matches), else the expected one, else a fresh variable. Capacity for the output.
    fn closure_inputs(
        &mut self,
        closure: ExprId,
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
                (None, None) => self.fresh_from(crate::diag::VarOrigin::ClosureParam { closure, index: i as u32 })?,
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
        let async_expected = self.async_callable_hint(f.expect)?;
        let call_expected = match async_expected {
            None => self.deduce_sig(f.expect)?,
            Some(_) => None,
        };
        let sig_from = self.sig_from(async_expected.or(call_expected), param_tys, pats.len(), ret)?;
        let async_expected = async_expected.map(|x| (x.inputs, x.output));
        let call_expected = call_expected.map(|x| (x.inputs, x.output));
        let expected = async_expected.or(call_expected);
        let mut inputs = self.closure_inputs(e, pats, param_tys, expected)?;
        let body_out = match (ret, async_expected.and_then(|(_, out)| out)) {
            (Some(r), _) => self.lower_body_ty(r, At::Expr(e))?,
            (None, Some(out)) => out,
            (None, None) => self.fresh_from(crate::diag::VarOrigin::ClosureReturn { closure: e })?,
        };
        // Captures are not told apart by kind here: `Ref` keeps both `T` and `&T` in the interior.
        let fut = self.open_future(e, body, body_out, crate::body::Capture::Ref)?;
        if let Some((_, Some(o))) = call_expected {
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
        self.closures.push(PendingClosure { expr: e, sig_from });
        self.resume(f, 1)?;
        self.push_expr(body, Expectation::HasType(body_out), Then::CoerceMany(f.a))?;
        for (i, &p) in pats.iter().enumerate().rev() {
            self.push_pat(p, inputs[i], 0)?;
        }
        Ok(Flow::Wait)
    }

    /// Reverse preorder puts every nested owner before its enclosing owner.
    fn capture_order(&self) -> Result<Vec<ExprId, Alloc<'a>>, Stop> {
        let mut pending = blaze_hash::HashSet::new_in(self.alloc);
        pending.extend(self.closures.iter().map(|c| c.expr));
        pending.extend(self.futures.iter().map(|f| f.expr));
        let mut order = Vec::with_capacity_in(pending.len(), self.alloc);
        let mut places = Vec::new_in(self.alloc);
        let mut pats = Vec::new_in(self.alloc);
        places.push(self.body.root);
        let mut steps = 0;
        while let Some(e) = places.pop() {
            steps += 1;
            if steps > self.body.exprs.len() {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            if pending.remove(&e) {
                order.push(e);
                if pending.is_empty() {
                    break;
                }
            }
            pats.clear();
            self.expr_children(e, &mut places, &mut pats)?;
        }
        if let Some(&missing) = pending.iter().min() {
            return Err(CheckError::Malformed(crate::error::Malformed::Expr(missing)).into());
        }
        order.reverse();
        Ok(order)
    }

    /// Constructing a nested closure uses its environment. Its body runs later.
    pub(crate) fn analyze_closures(&mut self) -> Result<(), Stop> {
        if self.closures.is_empty() && self.futures.is_empty() {
            return Ok(());
        }
        let order = self.capture_order()?;
        let mut places = Vec::new_in(self.alloc);
        let mut pats = Vec::new_in(self.alloc);
        let mut bound: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(self.body.pats.len(), self.alloc);
        let mut exprs: Vec<(ExprId, u8), Alloc<'a>> = Vec::new_in(self.alloc);
        let mut children: blaze_hash::HashMap<ExprId, InnerCaptures, Alloc<'a>> = blaze_hash::HashMap::new_in(self.alloc);
        let mut inherited = Vec::new_in(self.alloc);
        let mut fields = Vec::new_in(self.alloc);
        let mut places_cx = self.capture_places_new()?;
        for owner in order {
            let (body, params) = match self.body.expr(owner)? {
                Expr::Closure { body, params, .. } => (body, Some(params)),
                Expr::AsyncBlock { body, .. } => (body, None),
                _ => return Err(CheckError::Malformed(crate::error::Malformed::Expr(owner)).into()),
            };
            let capture = if self.captures_by_move(owner)? { Capture::Move } else { Capture::Ref };
            bound.clear();
            bound.resize(self.body.pats.len(), false);
            if let Some(params) = params {
                pats.clear();
                pats.extend_from_slice(self.body.pat_list(params)?);
                self.mark_bound(&mut pats, &mut bound)?;
            }
            let first = self.captures.len();
            let first_use = self.capture_uses.len();
            inherited.clear();
            let mut unknown_child = false;
            if owner == self.body.root && self.body.coroutine.is_some() && params.is_none() && capture == Capture::Move {
                // An async fn moves every argument into its unpolled future,
                // including unused arguments. Its lowering gives even a
                // destructured or wildcard parameter a single entry binding.
                for &p in self.body.params {
                    if !matches!(self.body.pat(p)?, Pat::Bind { ann, sub: None, .. } if ann.by_ref.is_none()) {
                        return Err(CheckError::FutureWitness {
                            reason: "the async fn's initial state requires a binding for each moved argument",
                            at: self.body.pat_range(p),
                        }.into());
                    }
                    self.captures.push((owner, p, CaptureKind::ByValue));
                    inherited.push((p, CaptureKind::ByValue));
                }
            } else {
                exprs.push((body, USE_MOVE));
            }
            let mut steps = 0;
            while let Some((e, usage)) = exprs.pop() {
                steps += 1;
                if steps > self.body.exprs.len() {
                    return Err(CheckError::Limit(Limit::Steps).into());
                }
                if let Some(child) = children.get(&e).filter(|child| child.whole) {
                    for i in child.rows.clone() {
                        let (_, p, child_mode) = self.captures[i];
                        if bound[p.ix()] {
                            continue;
                        }
                        // Moving a Copy value into the child only reads it in
                        // the parent. The parent's own move clause changes
                        // storage independently of the way it can be called.
                        let needs_copy = capture == Capture::Ref || params.is_some();
                        let used = if needs_copy && child_mode == CaptureKind::ByValue && self.copied_into_child(p, e)? {
                            CaptureKind::ByRef
                        } else {
                            child_mode
                        };
                        let mode = if capture == Capture::Move { CaptureKind::ByValue } else { used };
                        let kind = match used {
                            CaptureKind::ByRef => crate::ty::ClosureKind::Fn,
                            CaptureKind::ByMutRef => crate::ty::ClosureKind::FnMut,
                            CaptureKind::ByValue => crate::ty::ClosureKind::FnOnce,
                        };
                        self.captures.push((owner, p, mode));
                        self.capture_uses.push((owner, kind));
                        inherited.push((p, mode));
                    }
                } else {
                    if let Some(child) = children.get(&e) {
                        let moves_into_child = params.is_none() && capture == Capture::Ref && self.captures_by_move(e)?;
                        for i in child.rows.clone() {
                            let (_, p, _) = self.captures[i];
                            if !bound[p.ix()] {
                                unknown_child = true;
                                // Keep the block's conservative move row when the
                                // child may move a projection. It cannot certify
                                // an exact witness for the parent.
                                if moves_into_child && !self.local_may_be_copy(p)? {
                                    self.captures.push((owner, p, CaptureKind::ByValue));
                                }
                            }
                        }
                    }
                    self.visit_for_captures(owner, e, usage, &mut exprs, &mut pats, &mut bound)?;
                }
            }
            self.captures[first..].sort_unstable_by_key(|&(_, p, k)| (p, core::cmp::Reverse(k)));
            self.captures.dedup_range_by(first.., |a, b| a.1 == b.1);
            // The captured places (capture_places.rs, RFC 2229) decide the rows, the kind and
            // the environment. Where they are a gap, the whole-binding proof below still
            // decides what it can prove, and the rest stays the solver's witness gap.
            let async_fn_root = owner == self.body.root && self.body.coroutine.is_some() && params.is_none() && capture == Capture::Move;
            let (rows, suspends, kind, upvars, whole, decided) = match self.capture_places(&mut places_cx, owner, async_fn_root)? {
                Ok(precise) => {
                    // A binding the analysis widened (capture_places.rs `widen`, a place the
                    // build does not lower) is filed as one whole-binding place.
                    debug_assert!(
                        places_cx
                            .widened()
                            .iter()
                            .filter(|w| w.0 == owner)
                            .all(|w| places_cx.places(precise).iter().any(|c| c.base == w.1 && c.len == 0)),
                        "owner {owner:?}: a widened binding filed with places below it"
                    );
                    let whole = self.rows_from_places(&places_cx, owner, first, precise);
                    let tuple = self.places_tuple(&places_cx, first, precise)?;
                    // The places themselves, one row per environment field (stage 2): beside
                    // the whole-binding rows until every reader takes them.
                    self.file_places(&places_cx, owner, precise);
                    (first..self.captures.len(), Some(precise.suspends), precise.kind, Some(tuple), whole, true)
                }
                Err(_gap) => {
                    let rows = first..self.captures.len();
                    let suspends = if unknown_child {
                        None
                    } else {
                        self.whole_capture_witness(body, capture, &rows, &inherited, &mut places, &mut pats, &mut bound)?
                    };
                    let kind = self.capture_uses[first_use..].iter().map(|r| r.1).max().unwrap_or(crate::ty::ClosureKind::Fn);
                    let upvars = if suspends.is_some() {
                        fields.clear();
                        for i in rows.clone() {
                            fields.push(self.stored_capture(i)?);
                        }
                        let list = self.mk_list(&fields)?;
                        Some(self.mk(TyView::Tuple(list))?)
                    } else {
                        None
                    };
                    (rows, suspends, kind, upvars, suspends.is_some(), false)
                }
            };
            children.insert(owner, InnerCaptures { rows: rows.clone(), whole });
            if params.is_some() {
                self.ty.settle_closure(owner, kind, upvars)?;
                self.closure_kinds.push((owner, kind, upvars));
            }
            self.capture_uses.truncate(first_use);
            if let Some(f) = self.futures.iter_mut().find(|f| f.expr == owner) {
                // An AsyncFnOnce-only closure consumes its environment. There
                // is no borrowing call variant with a different stored type.
                let owns_environment = params.is_none() || rows.is_empty() || kind == crate::ty::ClosureKind::FnOnce;
                // RFC 3668 ("Async closures"), rustc `CoroutineClosureSignature::
                // tupled_upvars_by_closure_kind`: an `Fn`/`FnMut` async closure has two call
                // futures. `async_call_once(self)`'s (the by-move body) stores the closure's own
                // captures; `async_call(&self)`/`async_call_mut(&mut self)`'s stores each place
                // the body uses by reference (`&P` or `&mut P` by its use) and, where the
                // closure holds the place BY VALUE, borrows it from the closure's environment.
                // The checker has ONE future term for both (blaze-solve async_fn.rs, "Deviation,
                // stated"). Where every decided place is held by reference (no `move`, no
                // by-value capture), the two store the same types up to regions, which no auto
                // trait reads: `&P` (or `&mut P`) per place in capture order, the places' tuple.
                // So that future is decided as the owning case is. A closure holding a place by
                // value (`async move || *r`) stores `P` in one and `&P` in the other (`Send`
                // differs: `Rc<u8>` versus `&Rc<u8>`, `MutexGuard` versus `&MutexGuard`): one
                // term cannot be both, so it stays `NeedsCapturePlaces` (opaque_auto.rs
                // `CapturesUnknown`, a named gap) until each call future has its own identity.
                let borrows_alike = decided
                    && params.is_some()
                    && self.captures[rows.clone()].iter().all(|&(_, _, mode)| mode != CaptureKind::ByValue);
                if (owns_environment || borrows_alike) && let Some(suspends) = suspends {
                    // The exact interior's readers take the captures from the PLACES
                    // (blaze-flow-lower coroutine.rs `parts_with`, blaze-ladder front2
                    // stalled_goals.rs `storage_tuple`: `env_fields_by::<Places>`), each typed
                    // by how its field holds it, and only the saved locals from the flow, which
                    // are the same under either layout. A decided owner's places are known, an
                    // undecided one's are its whole-binding rows: the flow's saved locals are
                    // all that is missing either way.
                    f.witness = if !suspends {
                        crate::CoroutineWitness::Complete
                    } else {
                        crate::CoroutineWitness::NeedsSavedLocals
                    };
                    // The places' environment: async_fn.rs `analyze_coroutine_captures` holds it
                    // in the unpolled state in place of the whole-binding rows.
                    if decided {
                        f.initial = upvars;
                    }
                }
            }
        }
        self.captures.sort_unstable_by_key(|&(c, p, _)| (c, p));
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
                            if let Some(i) = init
                                && !(matches!(self.body.pat(pat)?, Pat::Wild) && self.discarded_local(i)?)
                            {
                                exprs.push((i, USE_MOVE));
                            }
                            if let Some(e) = else_ {
                                exprs.push((e, 0));
                            }
                        }
                        // A value statement evaluates and discards its result.
                        // A captured non-Copy place in `x;` is moved out.
                        Stmt::Expr { expr, .. } => exprs.push((expr, USE_MOVE)),
                        Stmt::Item => {}
                    }
                }
            }
            // The stack pops last-pushed first: `cond` is pushed last so its `let` marks its
            // bindings bound before `then` (which uses them) is visited.
            // A branch's value is the `if`'s or the `match`'s: taken by value when it is (as a
            // block's tail).
            Expr::If { cond, then, else_ } => {
                let branch = if usage == USE_MOVE { USE_MOVE } else { 0 };
                if let Some(e) = else_ {
                    exprs.push((e, branch));
                }
                exprs.push((then, branch));
                exprs.push((cond, 0));
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
                    exprs.push((arm.body, if usage == USE_MOVE { USE_MOVE } else { 0 }));
                }
            }
            Expr::Loop { body } => exprs.push((body, 0)),
            Expr::Break { value, .. } | Expr::Return { value } => {
                if let Some(v) = value {
                    exprs.push((v, USE_MOVE));
                }
            }
            Expr::Call { callee, args } => {
                // An overloaded call has a checked receiver just like a method call.
                // FnOnce consumes it. Fn and FnMut leave a borrow adjustment.
                let usage = self.callee_capture_use(x, callee).unwrap_or(0);
                exprs.push((callee, usage));
                for &a in el(args)? {
                    exprs.push((a, USE_MOVE));
                }
            }
            Expr::MethodCall { receiver, args, .. } => {
                exprs.push((receiver, self.receiver_usage(receiver)));
                for &a in el(args)? {
                    exprs.push((a, USE_MOVE));
                }
            }
            // A field use is a place use of its base. A non-`Copy` field moved out of a base
            // held by value moves out of the base's place (the Reference, "Capture modes": a
            // moved non-`Copy` place is captured by value, and the closure is `FnOnce`): the
            // base is captured by value. rustc captures the field's place (`a.b`, "Capture
            // precision"); the whole binding is a superset, so a later use of the rest of the
            // base is refused by the borrow check, never miscompiled. A field through a
            // reference cannot be moved out (only reborrowed or copied): a read of its base.
            Expr::Field { base, .. } | Expr::TupleField { base, .. } => {
                let held = self.ty.resolve(self.expr_ty[base.ix()])?;
                let by_value = !matches!(self.view(held), TyView::Ref(..) | TyView::Ptr(..));
                let of_block = matches!(self.body.expr(closure)?, Expr::AsyncBlock { .. });
                let moves = usage == USE_MOVE && by_value && if of_block { self.block_moved_out(x)? } else { self.moved_out(x)? };
                exprs.push((base, if moves { USE_MOVE } else if usage == USE_MOVE { 0 } else { usage }))
            }
            Expr::Unary { op: UnOp::Deref, operand } => {
                // Copying the target of an owned pointer leaves the pointer
                // in the environment for the next call.
                let usage = if usage == USE_MOVE && !self.block_moved_out(x)? { 0 } else { usage };
                exprs.push((operand, usage));
            }
            Expr::Unary { operand, .. } => exprs.push((operand, USE_MOVE)),
            // `lhs` pushed last, visited first: a let chain's `let` binds before `rhs` uses it.
            Expr::Binary { op, lhs, rhs } => {
                let usage = match op {
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::And | BinOp::Or => 0,
                    _ => USE_MOVE,
                };
                exprs.push((rhs, usage));
                exprs.push((lhs, usage));
            }
            Expr::Assign { lhs, rhs } if matches!(bv.expr(lhs)?, Expr::Underscore) && self.discarded_local(rhs)? => {}
            Expr::Assign { lhs, rhs } | Expr::AssignOp { lhs, rhs, .. } => {
                exprs.push((lhs, 1));
                exprs.push((rhs, USE_MOVE));
            }
            Expr::Ref { mutbl, operand } | Expr::RawRef { mutbl, operand } => {
                exprs.push((operand, u8::from(mutbl == Mutbl::Mut)));
            }
            // An inline constant sees no local of the body: it captures nothing.
            Expr::ConstBlock { .. } => {}
            Expr::Cast { operand, .. } => exprs.push((operand, 0)),
            // The Reference, "Place expressions and value expressions": the operands of a tuple
            // or array expression and a struct expression's field initializers are value
            // expressions, so a place there is moved (copied when `Copy`) into the aggregate,
            // whatever the aggregate's own context (`&(s, 1)` moves `s` too). Walked as reads,
            // `move || (1u8, s)` was not `FnOnce` and its body moved out of `&mut self`
            // (tempo vsock-proxy main.rs:210, a false E0507).
            Expr::Tuple { elems } | Expr::Array { elems } => {
                for &a in el(elems)? {
                    exprs.push((a, USE_MOVE));
                }
            }
            Expr::Repeat { elem, .. } => exprs.push((elem, 0)),
            Expr::Struct { fields, base, .. } | Expr::Record { fields, base } => {
                for fi in self.body.field_init_list(fields)? {
                    exprs.push((fi.expr, USE_MOVE));
                }
                if let Some(b) = base.expr() {
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
            Expr::Await(operand) => exprs.push((operand, USE_MOVE)),
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
        // A by-value method may autoderef to a Copy receiver. The value taken
        // is that adjusted value, while the pointer used to reach it stays put.
        let t = exprs.iter().zip(self.adjust.column::<2>()).rev()
            .find_map(|(&e, &t)| (e == x).then_some(t))
            .unwrap_or(self.expr_ty[x.ix()]);
        // Without `Copy` nothing can be told copied: the finding names the lang item (the body
        // is tainted), and the use counts as a move, the Reference's case for a non-`Copy` place.
        let Some(copy) = self.need_lang(LangItem::Copy, At::Expr(x))? else { return Ok(true) };
        let t = self.ty.resolve(t)?;
        let args = self.mk_list(&[t])?;
        Ok(!self.ty.may_hold(crate::ty::Goal::Trait { trait_: copy, args }, &mut self.reads)?)
    }

    /// A wildcard does not read a local place. A call or a coercion in the
    /// discarded expression still evaluates its operands.
    pub(super) fn discarded_local(&self, e: ExprId) -> Result<bool, Stop> {
        Ok(matches!(self.body.expr(e)?, Expr::Local(_)) && !self.adjust.column::<0>().contains(&e))
    }

    /// Calls on a concrete closure use its inferred kind without an overloaded
    /// method row. For trait calls, the selected method supplied the adjustment.
    pub(crate) fn callee_capture_use(&mut self, call: ExprId, callee: ExprId) -> Option<u8> {
        if self.callees.iter().any(|&(at, _, _)| at == call) {
            return Some(self.receiver_usage(callee));
        }
        let ty = self.ty.shallow_resolve(self.expr_ty[callee.ix()]);
        let kind = match self.view(ty) {
            TyView::FnPtr(..) => return Some(0),
            TyView::Closure(e, _) => self.closure_kinds.iter().find(|row| row.0 == e)?.1,
            TyView::ClosureOf { kind, .. } => kind,
            _ => return None,
        };
        Some(match kind {
            crate::ty::ClosureKind::Fn => 0,
            crate::ty::ClosureKind::FnMut => 1,
            crate::ty::ClosureKind::FnOnce => USE_MOVE,
        })
    }

    /// An autoref uses its borrow mode. A by-value receiver consumes or copies the place.
    fn receiver_usage(&self, receiver: ExprId) -> u8 {
        self.adjust.column::<0>().iter().zip(self.adjust.column::<1>()).rev()
            .find_map(|(&e, k)| match k {
                crate::columns::AdjustKind::Borrow { mutbl, .. } if e == receiver => Some(u8::from(*mutbl == Mutbl::Mut)),
                _ => None,
            })
            .unwrap_or(USE_MOVE)
    }

    /// Marks every binding under the patterns in `stack` as bound (the stack is consumed).
    pub(crate) fn mark_bound(&self, stack: &mut Vec<PatId, Alloc<'a>>, bound: &mut Vec<bool, Alloc<'a>>) -> Result<(), Stop> {
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
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
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
