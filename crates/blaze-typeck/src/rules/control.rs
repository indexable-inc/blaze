//! Control flow and divergence: the body itself, `if`, `match`, `loop`, `break`, `return`.
//!
//! Follows rustc_hir_typeck/src/check.rs (`check_fn`: parameters, then the body as a return
//! site), _match.rs (`check_match`), expr.rs (`check_then_else`, `if_fallback_coercion`,
//! `check_expr_loop`, `check_expr_break`, `check_expr_return`, `check_return_or_body_tail`) and
//! diverges.rs; Relax maps them in typeck.rs (`check_body`, `check_if`, `check_match`,
//! `check_loop`, `check_break`, `check_return`).
//!
//! The orders are rustc's. `if`: the condition must be `bool`; the then-branch is checked with
//! the expectation adjusted for branches, the coercion target is computed after it, then the
//! else-branch; without `else`, a forced `()` (E0317). The `if` diverges when the condition
//! does, or both branches do. `match`: the scrutinee with no expectation, every arm's pattern,
//! then each arm's guard (`bool`) and body, coerced into one site aimed at the expected type
//! unless that is `()` (a fresh variable then). It diverges when the scrutinee or every arm
//! does; no arms is `!`. `loop`: its body is `()`; its type is what its breaks coerce to, `!`
//! when none; it diverges unless something breaks it. `break` and `return` check their value
//! with the site's expected type as a hint, push it (or a forced `()`) into the site, and are
//! `!`.

use blaze_own::Alloc;

use crate::body::{CoroutineKind, Pat};
use crate::columns::UnionPat;
use crate::diag::{About, At, DiagCode};
use crate::error::{CheckError, Malformed, Stop};
use crate::ids::{ExprId, LangItem, List, Mutbl, PatId};
use crate::machine::{BreakCtx, Expectation, Flow, Frame, Lower, Machine, MatchNarrow, Source, Then};
use crate::ty::{Cause, Goal, Ty, TyView, TypeCtx};
use crate::world::{PClause, World};

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// `check_fn`'s start: the owner's where-clauses become assumptions, the return site is
    /// opened, and the body and then the parameter patterns are pushed (so the patterns run
    /// first). Returns the declared output.
    pub(crate) fn start_body(&mut self) -> Result<Ty, Stop> {
        let owner = self.body.owner;
        let preds = self.reads.predicates(owner)?;
        for clause in preds.clauses {
            let goal = match *clause {
                PClause::Trait { trait_, args } => {
                    let args = self.lower_body_list(preds.types, args)?;
                    Goal::Trait { trait_, args }
                }
                PClause::Projection { trait_, args, name, term } => {
                    let args = self.lower_body_list(preds.types, args)?;
                    let term = self.lower(preds.types, term, Lower::Assumed, Cause::Body)?;
                    Goal::Projection { trait_, args, name, term }
                }
                // The body's own `where const` condition is assumed, as its other clauses are.
                PClause::Const { cond, args } => Goal::ConstHolds { cond, args: self.lower_body_list(preds.types, args)? },
                PClause::ForFields { container, trait_, args } => Goal::ForFields {
                    container: self.lower(preds.types, container, Lower::Assumed, Cause::Body)?,
                    trait_,
                    args: self.lower_body_list(preds.types, args)?,
                },
                // Outlives clauses are the borrow checker's (D148); the checker takes them as true.
                PClause::TypeOutlives { .. } | PClause::RegionOutlives { .. } => continue,
            };
            #[cfg(test)]
            if self.plant == crate::machine::Plant::IgnoreWhereClauses {
                continue;
            }
            self.ty.assume(goal)?;
        }
        let (inputs, output, rust_call) = match self.reads.fn_sig(owner)? {
            Some(sig) => {
                self.own_params = sig.parent_count + sig.own_count;
                // A projection's well-formedness at each signature type that mentions one: the
                // type AS WRITTEN (an alias kept, not normalized: `Lower::Assumed`) is a
                // `WellFormed` obligation of the body, its where-clauses assumed above. What it
                // proves: a generic associated type's own clauses at each use (D89 (b);
                // blaze-solve structural.rs, the `Projection` arm): `x: <u8 as Tr>::Out<NotClone>`
                // under `type Out<T: Clone>` is E0277. OPEN: rustc's wfcheck asks every signature
                // type (an ADT's where-clauses at `S<NotClone>`); the rebuilt layer has no
                // item-level check yet, and this asks only types with a projection.
                // Each obligation is filed at the type's place in the body (`sig_place`: the
                // parameter, or the body for the output), so a failure is a finding with a place.
                let n_inputs = sig.inputs.len();
                for (k, &t) in sig.inputs.iter().chain(core::iter::once(&sig.output)).enumerate() {
                    // Whether `t` mentions an alias: a work list over its nodes, bounded (rule 18).
                    let mut todo: blaze_own::Vec<u32, Alloc<'a>> = blaze_own::Vec::new_in(self.alloc);
                    todo.push(t.0);
                    let mut alias = false;
                    let mut steps = 0u32;
                    while let Some(i) = todo.pop() {
                        steps += 1;
                        if steps > 1 << 16 {
                            return Err(CheckError::Limit(crate::error::Limit::Table).into());
                        }
                        let Some(&n) = sig.types.get(crate::PTy(i)) else {
                            return Err(CheckError::Malformed(Malformed::TypeOrder { node: i, child: i }).into());
                        };
                        if matches!(n, crate::PNode::Alias { .. }) {
                            alias = true;
                            break;
                        }
                        crate::portable::node_children(sig.types, n, &mut todo)?;
                    }
                    if !alias {
                        continue;
                    }
                    let cause = self.sig_place(k, n_inputs);
                    #[cfg(test)]
                    let cause = if self.plant == crate::machine::Plant::ObligationsWithoutPlace { Cause::Body } else { cause };
                    let w = self.lower(sig.types, t, Lower::Assumed, cause)?;
                    self.ty.register(crate::ty::Obligation { goal: Goal::WellFormed(w), cause })?;
                }
                let mut inputs: blaze_own::Vec<Ty, Alloc<'a>> = blaze_own::Vec::with_capacity_in(sig.inputs.len(), self.alloc);
                // The signature's projections are rows at the body's root while it is lowered
                // (machine.rs `sig_rows`); an early return stops the whole check.
                self.sig_rows = true;
                for (k, &t) in sig.inputs.iter().enumerate() {
                    let cause = self.sig_place(k, n_inputs);
                    inputs.push(self.lower_sig_ty(sig.types, t, cause)?);
                }
                let has_output_infer = Self::ptys_has_infer(sig.types, sig.output, self.alloc)?;
                let is_result_err_infer = match sig.types.node(sig.output)? {
                    crate::PNode::Adt(k, args) => {
                        let list = sig.types.list(args)?;
                        list.len() == 2
                            && matches!(sig.types.node(list[1])?, crate::PNode::Infer)
                            && self.is_result_adt(k)?
                    }
                    crate::PNode::Union(args) => {
                        let list = sig.types.list(args)?;
                        list.len() == 2 && matches!(sig.types.node(list[1])?, crate::PNode::Infer)
                    }
                    _ => false,
                };
                let out_cause = self.sig_place(n_inputs, n_inputs);
                // Each opaque the output names is this body's to define (rules/opaque_def.rs).
                // An async fn whose root is its `async move` block (the Reference, "Async
                // functions": `fn f() -> impl Future<Output = T> { async move { .. } }`, what
                // blaze-lower's `async_fn` makes) defines its `impl Future` the same way: the
                // hidden type is the block's future. A body marked async whose root is NOT an
                // async block (the hand-built fixtures' older form) is checked against the
                // written type, below.
                let output = if self.body.coroutine.is_none() || self.async_root()? {
                    let t = self.lower_defining_output(sig.types, sig.output, out_cause)?;
                    // rustc's order (`replace_opaque_types_with_inference_vars`): the
                    // bounds are obligations from the start, so a returned closure is
                    // typed from its `Fn` bound.
                    self.close_opaques()?;
                    t
                } else {
                    self.lower_sig_ty(sig.types, sig.output, out_cause)?
                };
                if has_output_infer {
                    self.inferred_output_ty = Some(output);
                }
                if is_result_err_infer {
                    let err_ty = match self.view(output) {
                        TyView::Adt(_, args) | TyView::Union(args) if self.ty.list_len(args) == 2 => {
                            Some(self.ty.list_get(args, 1))
                        }
                        _ => None,
                    };
                    if let Some(ev) = err_ty
                        && self.is_ty_var(ev)
                    {
                        self.inferred_err_var = Some(ev);
                    }
                }
                self.sig_rows = false;
                (inputs, output, sig.header.abi == crate::ids::Abi::RustCall)
            }
            None => (blaze_own::Vec::new_in(self.alloc), self.fresh()?, false),
        };
        // `extern "rust-call"` (the `unboxed_closures` feature; core's `Fn*::call*` are the
        // form: `extern "rust-call" fn call(&self, args: Args)` with `Args: Tuple`): the
        // arguments come as one tuple, so the last parameter's type must be `Tuple` (E0277 at
        // that parameter, the bound core's `Tuple` reports as "functions with the \"rust-call\"
        // ABI must take a single non-self tuple argument"); a type parameter passes by its bound.
        // OWED (typeck.md): a "rust-call" fn with no parameter has no place in the body to
        // report at; its finding belongs to the fn head's reader.
        if rust_call && let (Some(&p), Some(&last)) = (self.body.params.last(), inputs.last()) {
            if let Some(tuple) = self.need_lang(LangItem::Tuple, At::Pat(p))? {
                let args = self.mk_list(&[last])?;
                self.register_trait(tuple, args, Cause::Pat(p))?;
            }
        }
        // An `async fn`'s body returns the written type; its signature's output is the future.
        // With the `async move` block as the root, the fn returns the future (the signature's
        // output, above) and the BLOCK returns the written type (`async_fn_output`, read by
        // rules/coroutine.rs `expr_async_block`); the fn's own body is then not an async site.
        let async_root = self.async_root()?;
        let output = match self.body.coroutine {
            Some(CoroutineKind::Async { output }) if !async_root => self.lower_body_ty(output, At::Body)?,
            _ => output,
        };
        // The signature's projections are settled in the param env before the body runs
        // (rustc's `check_fn` takes the signature normalized): lowering `X::A` gave a variable
        // and an obligation of the signature, with no place in the body. Selected only later, a
        // body value would bind the variable first and the obligation would fail with no place
        // (`DiagWithoutPlace`); selected now, `X: Tr<A = u8>` makes it `u8` and a value that does
        // not fit is a mismatch at that value. What stays ambiguous (an opaque's bounds on its
        // hidden type) stays pending.
        #[cfg(test)]
        let settle = self.plant != crate::machine::Plant::SignatureUnsettled;
        #[cfg(not(test))]
        let settle = true;
        if settle {
            self.select()?;
        }
        let site = self.new_coerce_many(output)?;
        self.ret_sites.push(site);
        self.async_sites.push(self.body.coroutine.is_some() && !async_root);
        let root = self.body.root;
        self.push_expr(root, Expectation::HasType(output), Then::CoerceMany(site))?;
        let params = self.body.params;
        for (i, &p) in params.iter().enumerate().rev() {
            let t = match inputs.get(i) {
                Some(&t) => t,
                None => self.common.err,
            };
            self.push_pat(p, t, 0)?;
        }
        Ok(output)
    }

    /// `check_fn`'s end: the return site's type must be the declared output, `!` excepted.
    pub(crate) fn finish_body(&mut self, output: Ty) -> Result<(), Stop> {
        self.resolve_inferred_err()?;
        self.async_sites.pop();
        let Some(site) = self.ret_sites.pop() else { return Ok(()) };
        let actual = self.coerce_many_complete(site);
        if !self.is_never(actual) && !self.is_err(actual) {
            // The body's value against the declared output: reported at the body's block.
            let root = self.body.root;
            self.demand_eq(At::Expr(root), output, actual)?;
        }
        Ok(())
    }

    /// Whether any node reachable from `root` in `types` is `PNode::Infer`.
    fn ptys_has_infer(types: crate::portable::PTys<'_>, root: crate::PTy, alloc: Alloc<'a>) -> Result<bool, Stop> {
        let mut todo: blaze_own::Vec<u32, Alloc<'a>> = blaze_own::Vec::new_in(alloc);
        todo.push(root.0);
        let mut steps = 0u32;
        while let Some(i) = todo.pop() {
            steps += 1;
            if steps > 1 << 16 {
                return Err(CheckError::Limit(crate::error::Limit::Table).into());
            }
            let Some(&n) = types.get(crate::PTy(i)) else {
                return Err(CheckError::Malformed(Malformed::TypeOrder { node: i, child: i }).into());
            };
            if matches!(n, crate::PNode::Infer) {
                return Ok(true);
            }
            crate::portable::node_children(types, n, &mut todo)?;
        }
        Ok(false)
    }

    /// Whether `k` is a `Result`-shaped enum (two variants named `Ok` and `Err`).
    fn is_result_adt(&mut self, k: crate::ids::ItemKey) -> Result<bool, Stop> {
        let Some(adt) = self.reads.adt(k)? else { return Ok(false) };
        if adt.kind != crate::world::AdtKind::Enum || adt.variants.len() != 2 {
            return Ok(false);
        }
        let ok = crate::ids::Name::of("Ok");
        let err = crate::ids::Name::of("Err");
        let v0 = adt.variants[0].name;
        let v1 = adt.variants[1].name;
        Ok((v0 == ok && v1 == err) || (v0 == err && v1 == ok))
    }

    /// Finalizes `inferred_err_var` for `Result<T, _>` / `T ! _` functions from all collected
    /// `Err(...)` coercions and deferred `?` (`FromResidual`) obligations, then replays them so
    /// union injection/widening adjustments and `From`/`FromResidual` selections are recorded.
    fn resolve_inferred_err(&mut self) -> Result<(), Stop> {
        let Some(ev) = self.inferred_err_var.take() else { return Ok(()) };
        let coercions = core::mem::replace(&mut self.inferred_err_coercions, blaze_own::Vec::new_in(self.alloc));
        let try_obs = core::mem::replace(&mut self.deferred_try_obs, blaze_own::Vec::new_in(self.alloc));
        self.select()?;
        let ev_res = self.ty.shallow_resolve(ev);
        let final_err_ty = if self.is_ty_var(ev_res) {
            let mut members: blaze_own::Vec<Ty, Alloc<'a>> = blaze_own::Vec::new_in(self.alloc);
            for &(_, src_ty) in coercions.iter() {
                self.collect_inferred_err_members(src_ty, &mut members)?;
            }
            for &(_, fargs, _) in try_obs.iter() {
                if self.ty.list_len(fargs) >= 2 {
                    let self_ty = self.ty.resolve(self.ty.list_get(fargs, 0))?;
                    let matches_owner = match self.view(self_ty) {
                        TyView::Adt(_, a) | TyView::Union(a) if self.ty.list_len(a) == 2 => {
                            self.ty.shallow_resolve(self.ty.list_get(a, 1)) == ev_res
                        }
                        _ => false,
                    };
                    if matches_owner {
                        let res_ty = self.ty.resolve(self.ty.list_get(fargs, 1))?;
                        match self.view(res_ty) {
                            TyView::Adt(_, rargs) | TyView::Union(rargs) if self.ty.list_len(rargs) == 2 => {
                                let e_ty = self.ty.list_get(rargs, 1);
                                self.collect_inferred_err_members(e_ty, &mut members)?;
                            }
                            _ => {}
                        }
                    }
                }
            }
            let ty = match members.len() {
                0 => self.common.never,
                1 => members[0],
                _ => {
                    let list = self.mk_list(&members)?;
                    self.mk(TyView::Union(list))?
                }
            };
            self.unify(ev_res, ty)?;
            ty
        } else {
            ev_res
        };
        for (expr_opt, src_ty) in coercions {
            if !self.coerce(expr_opt, src_ty, final_err_ty, false)? {
                self.diag(DiagCode::Mismatch, expr_opt.map_or(At::Body, At::Expr), About::types(final_err_ty, src_ty));
            }
        }
        for (item, fargs, cause) in try_obs {
            self.register_predicates(item, fargs, cause)?;
        }
        Ok(())
    }

    fn collect_inferred_err_members(&mut self, root: Ty, out: &mut blaze_own::Vec<Ty, Alloc<'a>>) -> Result<(), Stop> {
        let mut stack: blaze_own::Vec<Ty, Alloc<'a>> = blaze_own::Vec::new_in(self.alloc);
        stack.push(root);
        let mut steps = 0u32;
        while let Some(cur) = stack.pop() {
            steps += 1;
            if steps > 1024 {
                return Err(CheckError::Limit(crate::error::Limit::Table).into());
            }
            let resolved = self.ty.resolve(cur)?;
            let resolved = match self.view(resolved) {
                TyView::Infer(crate::ty::Var { kind: crate::ty::VarKind::Int, .. }) => self.common.i32_,
                TyView::Infer(crate::ty::Var { kind: crate::ty::VarKind::Float, .. }) => self.common.f64_,
                _ => resolved,
            };
            match self.view(resolved) {
                TyView::Never | TyView::Error | TyView::Infer(_) | TyView::ConstInfer(_) => {}
                TyView::Union(list) => {
                    let n = self.ty.list_len(list);
                    for i in (0..n).rev() {
                        stack.push(self.ty.list_get(list, i));
                    }
                }
                _ => {
                    let mut dup = false;
                    for &seen in out.iter() {
                        if seen == resolved || self.probe(|m| m.unify(seen, resolved))? {
                            dup = true;
                            break;
                        }
                    }
                    if !dup {
                        out.push(resolved);
                    }
                }
            }
        }
        Ok(())
    }

    fn lower_body_list(&mut self, types: crate::portable::PTys<'_>, l: List) -> Result<crate::ty::TyList, Stop> {
        let src = types.list(l)?;
        let mut v: blaze_own::Vec<Ty, Alloc<'a>> = blaze_own::Vec::with_capacity_in(src.len(), self.alloc);
        for &t in src {
            v.push(self.lower(types, t, Lower::Assumed, Cause::Body)?);
        }
        self.mk_list(&v)
    }

    /// `check_then_else`. `f.a` is the site, `f.b` bit 0 the condition's divergence and bit 1
    /// the then-branch's.
    /// `let pat = init` as a condition (`check_expr_let`, through `check_decl`): the binding
    /// type a fresh variable, `init` coercible to it, `pat` checked against it as a `let`
    /// statement's is; the expression is `bool`.
    pub(crate) fn expr_let(&mut self, mut f: Frame, pat: PatId, init: ExprId) -> Result<Flow, Stop> {
        match f.state {
            0 => {
                f.t = self.fresh()?;
                self.resume(f, 1)?;
                self.push_coercible(init, f.t)?;
                Ok(Flow::Wait)
            }
            1 => {
                self.resume(f, 2)?;
                self.push_pat(pat, f.t, 0)?;
                Ok(Flow::Wait)
            }
            _ => Ok(Flow::Done(self.common.bool_)),
        }
    }

    pub(crate) fn expr_if(&mut self, mut f: Frame, e: ExprId, cond: ExprId, then: ExprId, else_: Option<ExprId>) -> Result<Flow, Stop> {
        match f.state {
            0 => {
                self.resume(f, 1)?;
                self.push_has_type(cond, self.common.bool_)?;
                Ok(Flow::Wait)
            }
            1 => {
                f.b = u32::from(self.diverges);
                self.diverges = false;
                f.expect = self.adjust_for_branches(f.expect);
                self.resume(f, 2)?;
                self.push_expr(then, f.expect, Then::Nothing)?;
                Ok(Flow::Wait)
            }
            2 => {
                let then_ty = self.ret;
                f.b |= u32::from(self.diverges) << 1;
                self.diverges = false;
                let target = self.coercion_target(f.expect)?;
                f.a = self.new_coerce_many(target)?;
                self.coerce_many_push(f.a, Source::Expr(then), then_ty)?;
                match else_ {
                    Some(els) => {
                        self.resume(f, 3)?;
                        self.push_expr(els, f.expect, Then::CoerceMany(f.a))?;
                        Ok(Flow::Wait)
                    }
                    None => {
                        self.if_fallback(e, f.a)?;
                        self.diverges = f.b & 1 != 0;
                        Ok(Flow::Done(self.coerce_many_complete(f.a)))
                    }
                }
            }
            _ => {
                let else_div = self.diverges;
                self.diverges = (f.b & 1 != 0) || ((f.b & 2 != 0) && else_div);
                Ok(Flow::Done(self.coerce_many_complete(f.a)))
            }
        }
    }

    /// `if_fallback_coercion`: an `if` without `else` is `()`; otherwise E0317.
    fn if_fallback(&mut self, e: ExprId, site: u32) -> Result<(), Stop> {
        let cm = self.coerce_many[site as usize];
        let merged = cm.final_ty.unwrap_or(cm.expected);
        if self.is_err(merged) {
            return Ok(());
        }
        let unit = self.common.unit;
        if self.unify(merged, unit)? {
            let entry = &mut self.coerce_many[site as usize];
            entry.final_ty = Some(unit);
            entry.pushed += 1;
        } else {
            self.diag(DiagCode::IfWithoutElse, At::Expr(e), About::types(merged, unit));
            self.coerce_many[site as usize].final_ty = Some(self.common.err);
        }
        Ok(())
    }

    /// `check_match`. `f.t` is the scrutinee's type, `f.a` the arm, `f.b` the site, `f.c` bit 0
    /// the scrutinee's divergence and bit 1 "every arm so far diverges".
    pub(crate) fn expr_match(&mut self, mut f: Frame, e: ExprId, scrutinee: ExprId, arms: List) -> Result<Flow, Stop> {
        let arm_list = self.body.arm_list(arms)?;
        match f.state {
            0 => {
                self.resume(f, 1)?;
                self.push_expr(scrutinee, Expectation::None, Then::Nothing)?;
                Ok(Flow::Wait)
            }
            1 => {
                f.t = self.ret;
                if arm_list.is_empty() {
                    if self.push_match_narrow(f.t)?
                        && let Some(narrow) = self.match_narrow.pop()
                        && let Some(&rem_v) = narrow.remaining.first()
                    {
                        let uncovered = self.ty.list_get(narrow.orig_members, rem_v);
                        self.diag(DiagCode::NonExhaustiveMatch, At::Expr(e), About::expected(uncovered));
                    }
                    return Ok(Flow::Done(self.common.never));
                }
                f.c = u32::from(self.diverges) | 2;
                self.diverges = false;
                f.a = 0;
                if self.push_match_narrow(f.t)? {
                    f.c |= 4;
                }
                self.match_pats(f, e, arm_list)
            }
            2 => {
                if f.c & 4 != 0
                    && let Some(prev_idx) = f.a.checked_sub(1)
                    && let Some(prev_arm) = usize::try_from(prev_idx).ok().and_then(|i| arm_list.get(i))
                {
                    let unguarded = prev_arm.guard.is_none();
                    let pat = self.body.pat(prev_arm.pat)?;
                    match pat {
                        Pat::Wild | Pat::Bind { ty: None, .. } => {
                            if unguarded && let Some(narrow) = self.match_narrow.last_mut() {
                                narrow.remaining.clear();
                            }
                        }
                        Pat::Or { alts } => {
                            let alt_pats = self.body.pat_list(alts)?;
                            let mut all_typed = !alt_pats.is_empty();
                            let mut has_catchall = false;
                            let mut matched_vs: blaze_own::Vec<u32, Alloc<'a>> =
                                blaze_own::Vec::with_capacity_in(alt_pats.len(), self.alloc);
                            for &ap in alt_pats {
                                let alt_kind = self.body.pat(ap)?;
                                if matches!(alt_kind, Pat::Wild | Pat::Bind { ty: None, .. }) {
                                    has_catchall = true;
                                    all_typed = false;
                                } else {
                                    let m = self
                                        .union_pats
                                        .iter()
                                        .rev()
                                        .find(|&&(p, _)| p == ap)
                                        .map(|&(_, kind)| kind);
                                    match m {
                                        Some(UnionPat::Variant(v)) => matched_vs.push(v),
                                        _ => all_typed = false,
                                    }
                                }
                            }
                            if all_typed {
                                let mut redundant_ty: Option<Ty> = None;
                                if let Some(narrow) = self.match_narrow.last_mut() {
                                    let orig_members = narrow.orig_members;
                                    for &v in matched_vs.iter() {
                                        let present = narrow.remaining.iter().any(|&idx| idx == v);
                                        if !present && redundant_ty.is_none() {
                                            redundant_ty = Some(self.ty.list_get(orig_members, v));
                                        }
                                        if unguarded {
                                            narrow.remaining.retain(|&idx| idx != v);
                                        }
                                    }
                                }
                                if let Some(ty) = redundant_ty {
                                    self.diag(DiagCode::RedundantPattern, At::Pat(prev_arm.pat), About::found(ty));
                                }
                            } else if has_catchall && unguarded && let Some(narrow) = self.match_narrow.last_mut() {
                                narrow.remaining.clear();
                            }
                        }
                        Pat::Bind { ty: Some(_), .. } => {
                            let matched = self
                                .union_pats
                                .iter()
                                .rev()
                                .find(|&&(p, _)| p == prev_arm.pat)
                                .map(|&(_, kind)| kind);
                            if let Some(UnionPat::Variant(v)) = matched {
                                let mut redundant_ty: Option<Ty> = None;
                                if let Some(narrow) = self.match_narrow.last_mut() {
                                    let present = narrow.remaining.iter().any(|&idx| idx == v);
                                    if !present {
                                        redundant_ty = Some(self.ty.list_get(narrow.orig_members, v));
                                    }
                                    if unguarded {
                                        narrow.remaining.retain(|&idx| idx != v);
                                    }
                                }
                                if let Some(ty) = redundant_ty {
                                    self.diag(DiagCode::RedundantPattern, At::Pat(prev_arm.pat), About::found(ty));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                self.match_pats(f, e, arm_list)
            }
            3 => {
                // The arm's guard is done (or there was none): its body.
                let arm = arm_list[f.a as usize];
                self.diverges = false;
                self.resume(f, 4)?;
                self.push_expr(arm.body, f.expect, Then::CoerceMany(f.b))?;
                Ok(Flow::Wait)
            }
            _ => {
                if !self.diverges {
                    f.c &= !2;
                }
                f.a += 1;
                self.match_arm(f, arm_list)
            }
        }
    }

    /// Pushes per-match union narrowing state if `scrutinee_ty` (after optional ref-peeling)
    /// resolves to a structural union.
    fn push_match_narrow(&mut self, scrutinee_ty: Ty) -> Result<bool, Stop> {
        let mut cur = scrutinee_ty;
        let mut peeled_mode = 0u32;
        for _ in 0..crate::machine::RECURSION_LIMIT {
            let r = self.try_structurally_resolve(cur)?;
            match self.view(r) {
                TyView::Ref(m, inner) => {
                    peeled_mode = match (m, peeled_mode) {
                        (Mutbl::Not, _) => 1,
                        (Mutbl::Mut, 0) => 2,
                        (Mutbl::Mut, other) => other,
                    };
                    cur = inner;
                }
                TyView::Union(orig_members) => {
                    let n = self.ty.list_len(orig_members);
                    let mut remaining = blaze_own::Vec::with_capacity_in(usize::try_from(n).unwrap_or(0), self.alloc);
                    for i in 0..n {
                        remaining.push(i);
                    }
                    self.match_narrow.push(MatchNarrow { orig_members, remaining, peeled_mode });
                    return Ok(true);
                }
                _ => return Ok(false),
            }
        }
        Ok(false)
    }

    /// The patterns of every arm, in order, then the first arm.
    fn match_pats(&mut self, mut f: Frame, e: ExprId, arms: &[crate::body::Arm]) -> Result<Flow, Stop> {
        if let Some(arm) = arms.get(f.a as usize) {
            f.a += 1;
            self.resume(f, 2)?;
            if f.c & 4 != 0
                && matches!(self.body.pat(arm.pat)?, Pat::Bind { ty: None, .. })
                && let Some(narrow) = self.match_narrow.last()
            {
                let orig_members = narrow.orig_members;
                let orig_len = usize::try_from(self.ty.list_len(orig_members)).unwrap_or(usize::MAX);
                let rem_len = narrow.remaining.len();
                let mode = narrow.peeled_mode;
                if rem_len < orig_len {
                    if rem_len == 0 {
                        self.push_pat(arm.pat, self.common.never, mode)?;
                        return Ok(Flow::Wait);
                    } else if rem_len == 1
                        && let Some(&rem_v) = narrow.remaining.first()
                    {
                        let member_ty = self.ty.list_get(orig_members, rem_v);
                        self.union_pats.push((arm.pat, UnionPat::Variant(rem_v)));
                        self.push_pat(arm.pat, member_ty, mode)?;
                        return Ok(Flow::Wait);
                    } else {
                        let mut rem_tys: blaze_own::Vec<Ty, Alloc<'a>> = blaze_own::Vec::with_capacity_in(rem_len, self.alloc);
                        for &idx in narrow.remaining.iter() {
                            rem_tys.push(self.ty.list_get(orig_members, idx));
                        }
                        let rem_list = self.mk_list(&rem_tys)?;
                        let sub_union = self.mk(TyView::Union(rem_list))?;
                        self.union_pats.push((arm.pat, UnionPat::Residual));
                        self.push_pat(arm.pat, sub_union, mode)?;
                        return Ok(Flow::Wait);
                    }
                }
            }
            self.push_pat(arm.pat, f.t, 0)?;
            return Ok(Flow::Wait);
        }
        if f.c & 4 != 0 {
            if let Some(narrow) = self.match_narrow.pop()
                && let Some(&rem_v) = narrow.remaining.first()
            {
                let uncovered = self.ty.list_get(narrow.orig_members, rem_v);
                self.diag(DiagCode::NonExhaustiveMatch, At::Expr(e), About::expected(uncovered));
            }
            f.c &= !4;
        }
        f.expect = self.adjust_for_branches(f.expect);
        let first = match f.expect {
            Expectation::HasType(t) if t != self.common.unit => t,
            _ => self.fresh()?,
        };
        f.b = self.new_coerce_many(first)?;
        f.a = 0;
        self.match_arm(f, arms)
    }

    /// Arm `f.a`'s guard, or the end of the match.
    fn match_arm(&mut self, f: Frame, arms: &[crate::body::Arm]) -> Result<Flow, Stop> {
        let Some(arm) = arms.get(f.a as usize) else {
            self.diverges = f.c != 0;
            return Ok(Flow::Done(self.coerce_many_complete(f.b)));
        };
        self.resume(f, 3)?;
        if let Some(g) = arm.guard {
            self.diverges = false;
            self.push_has_type(g, self.common.bool_)?;
        }
        Ok(Flow::Wait)
    }

    /// `check_expr_loop` for `loop`. `f.a` is the site.
    pub(crate) fn expr_loop(&mut self, mut f: Frame, e: ExprId, body: ExprId) -> Result<Flow, Stop> {
        if f.state == 0 {
            let target = self.coercion_target(f.expect)?;
            f.a = self.new_coerce_many(target)?;
            self.breaks.push(BreakCtx { target: e, coerce: Some(f.a), may_break: false });
            self.resume(f, 1)?;
            self.push_has_type(body, self.common.unit)?;
            return Ok(Flow::Wait);
        }
        let ctx = self.breaks.pop();
        self.diverges = !ctx.is_some_and(|c| c.may_break);
        Ok(Flow::Done(self.coerce_many_complete(f.a)))
    }

    fn find_break(&self, target: ExprId) -> Result<usize, Stop> {
        match self.breaks.iter().rposition(|b| b.target == target) {
            Some(i) => Ok(i),
            None => Err(CheckError::Malformed(Malformed::BreakTarget(target)).into()),
        }
    }

    /// `check_expr_break`. `f.a` is the break context's index.
    pub(crate) fn expr_break(&mut self, mut f: Frame, e: ExprId, value: Option<ExprId>, target: ExprId) -> Result<Flow, Stop> {
        if f.state == 0 {
            let i = self.find_break(target)?;
            f.a = i as u32;
            let Some(site) = self.breaks[i].coerce else {
                return Err(CheckError::Malformed(Malformed::BreakTarget(target)).into());
            };
            match value {
                Some(v) => {
                    let hint = self.coerce_many[site as usize].expected;
                    self.resume(f, 1)?;
                    self.push_expr(v, Expectation::HasType(hint), Then::CoerceMany(site))?;
                    return Ok(Flow::Wait);
                }
                None => self.coerce_many_push(site, Source::Unit(e), self.common.unit)?,
            }
        }
        if !self.diverges {
            self.breaks[f.a as usize].may_break = true;
        }
        Ok(Flow::Done(self.common.never))
    }

    /// `check_expr_return`.
    pub(crate) fn expr_return(&mut self, f: Frame, e: ExprId, value: Option<ExprId>) -> Result<Flow, Stop> {
        if f.state == 0 {
            let Some(&site) = self.ret_sites.last() else {
                return Err(CheckError::Malformed(Malformed::BreakTarget(e)).into());
            };
            match value {
                Some(v) => {
                    let hint = self.coerce_many[site as usize].expected;
                    self.resume(f, 1)?;
                    self.push_expr(v, Expectation::HasType(hint), Then::CoerceMany(site))?;
                    return Ok(Flow::Wait);
                }
                None => self.coerce_many_push(site, Source::Unit(e), self.common.unit)?,
            }
        }
        Ok(Flow::Done(self.common.never))
    }
}
