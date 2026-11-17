//! A closure's body as its own flow, and an `async fn`'s body with its suspensions (the move and
//! borrow check's two nested forms: wiring/blaze-ladder/borrows-closures-async-1.md).
//!
//! The Reference, "Closure expressions": "A closure expression ... captures variables from the
//! environment"; "Capture modes": "The compiler prefers to capture a closed-over variable by
//! immutable borrow, followed by unique immutable borrow, by mutable borrow, and finally by
//! move"; "Closure types": the closure is "roughly" a struct of its captures, and "Call traits
//! and coercions": an `Fn` closure's body reads its environment through `&self`, an `FnMut`'s
//! through `&mut self`, an `FnOnce`'s owns it (`self`). The owner's side (each capture a borrow
//! or a move at the closure expression, held in the closure value's own positions, so live as
//! long as the value) is the lowering's `closure_value`; this module lowers the BODY, the type
//! checker's capture rows (`Checked::captures`) and the closure's settled kind
//! (`Checked::closures`) deciding each capture's argument:
//!
//! - by reference (`ByRef`, `ByMutRef`): an argument `&T` / `&mut T`, and the captured variable
//!   is the place `*arg` (the same form an async block's body takes, coroutine.rs `run_async`);
//! - by value in an `FnOnce` closure: an argument `T`, the variable itself, moved and dropped as
//!   the body's own;
//! - by value in an `Fn` or `FnMut` closure: an argument `&T` or `&mut T` to the environment's
//!   field, the variable the place `*arg`: a move out of it is a move out of a place behind a
//!   reference (E0507, "cannot move out of a captured variable in an `Fn(Mut)` closure"), and it
//!   is not dropped by a call.
//!
//! The closure's parameters follow the captures as arguments; its return place is the body's
//! value. Every position is a placeholder. Known among them: each argument's implied bounds
//! (`&'a T` is well formed only when `T: 'a`, the Reference, "Lifetime bounds"), and that each
//! capture's positions outlive every position of the return place, except the environment's own
//! reference (`&'env self` of an `Fn`/`FnMut`: its by-value captures and, in an `FnMut`, its
//! `&mut` captures can be returned only for `'env`, which the return type does not name). A
//! relation the body needs beyond these is the owner's to prove (its closure requirements, not
//! built): the caller files such an outlives result as a refusal, never as a finding.
//!
//! An `async fn` (the Reference, "Async functions": "an async fn ... returns a future"; its body
//! runs when the future is polled, its parameters moved into it): the fn's body lowered as a fn
//! body, `.await` allowed in it (coroutine.rs `await_done`: each a suspension whose loans stay
//! live through it), the return place the written output.
//!
//! No recursion: the walk is the lowering's task stack.

use blaze_flow::{FlowBuilder, Local, Proj};
use blaze_own::Vec;
use blaze_typeck::{BodyView, CaptureKind, Checked, ClosureKind, Expr, ExprId, Mutbl, Pat, PatId};
use core::alloc::Allocator;

use crate::{At, Facts, Form, Lower, Lowered, MOST_POSITIONS, Refusal, Task, TyD, Val, range_of};

/// The body of closure `closure` (an `Expr::Closure` of `body`, not `async`) as its own flow:
/// its captures, then its parameters, as the arguments (the module doc).
pub fn lower_closure<A: Allocator + Clone, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    closure: ExprId,
    facts: &F,
    alloc: A,
) -> Result<Lowered<A>, Refusal> {
    let range = range_of(body, closure);
    let (params, inner) = match body.exprs.get(closure.ix()) {
        Some(Expr::Closure { is_async: true, .. }) => return Err(Refusal { form: Form::AsyncClosure, range }),
        Some(Expr::Closure { params, body: inner, .. }) => (*params, *inner),
        _ => return Err(Refusal { form: Form::Malformed, range }),
    };
    let params: &[PatId] = match body.pat_lists.get(params.range()) {
        Some(p) => p,
        None => return Err(Refusal { form: Form::Malformed, range }),
    };
    let row = ck.closures.column::<0>().iter().position(|&c| c == closure);
    let kind = match row.and_then(|i| ck.closures.column::<1>().get(i)) {
        Some(&k) => k,
        None => return Err(Refusal { form: Form::Malformed, range }),
    };
    let mut l = Lower::new(body, ck, facts, alloc)?;
    l.run_closure(closure, inner, params, kind)?;
    let stats = l.stats;
    let flow = l.b.finish().map_err(|_| Refusal { form: Form::Builder, range })?;
    Ok(Lowered { flow, stats, params: l.param_origs })
}

/// The body of an `async fn` (`BodyView::coroutine` is `Some`) as its own flow, its `.await`s
/// suspensions (the module doc). A body that is not an `async fn`'s is refused as malformed:
/// [`crate::lower`] takes it.
pub fn lower_async_fn<A: Allocator + Clone, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    alloc: A,
) -> Result<Lowered<A>, Refusal> {
    let range = range_of(body, body.root);
    if body.coroutine.is_none() {
        return Err(Refusal { form: Form::Malformed, range });
    }
    let mut l = Lower::new(body, ck, facts, alloc)?;
    l.async_root = Some(body.root);
    l.run()?;
    let suspends = l.points.len();
    let stats = l.stats;
    let flow = l.b.finish().map_err(|_| Refusal { form: Form::Builder, range })?;
    let counted = flow.stmts.iter().filter(|s| matches!(s.kind, blaze_flow::StmtKind::Suspend { .. })).count();
    if counted != suspends {
        return Err(Refusal { form: Form::Malformed, range });
    }
    Ok(Lowered { flow, stats, params: l.param_origs })
}

impl<F: Facts, A: Allocator + Clone> Lower<'_, '_, F, A> {
    /// The walk of closure `closure`'s body `inner`: as `run`, with the captures then the
    /// parameters as the arguments and the body's value as the return place.
    fn run_closure(&mut self, closure: ExprId, inner: ExprId, params: &[PatId], kind: ClosureKind) -> Result<(), Refusal> {
        let ck = self.ck;
        // The closure's captures, in row order: argument `1 + k` is row `k`.
        let mut rows: Vec<(PatId, CaptureKind), A> = Vec::new_in(self.alloc.clone());
        {
            let firsts = ck.captures.column::<0>();
            let pats = ck.captures.column::<1>();
            let kinds = ck.captures.column::<2>();
            for i in 0..firsts.len() {
                if firsts[i] == closure {
                    rows.push((pats[i], kinds[i]));
                }
            }
        }
        let nu = rows.len() as u32;
        let np = params.len() as u32;
        self.b = FlowBuilder::new_in(1 + nu + np, nu + np, self.alloc.clone());
        self.nargs = nu + np;
        let entry = self.b.block();
        self.ret_block = self.b.block();
        self.b.start(entry);
        self.scope_marks.push(0);
        // The environment's own references (by origin index in `origs`): not known to outlive
        // the return place.
        let mut env_refs: Vec<u32, A> = Vec::new_in(self.alloc.clone());
        for k in 0..rows.len() {
            let (p, cap) = rows[k];
            let t = match ck.local_ty.get(p.ix()) {
                Some(&t) => t,
                None => return self.refuse_pat(Form::Malformed, p),
            };
            let l = Local(1 + k as u32);
            // How the body reaches the variable: owned (`None`), or through a reference of
            // this mutability, which is the environment's own when `env` is set.
            let (through, env) = match (cap, kind) {
                (CaptureKind::ByValue, ClosureKind::FnOnce) => (None, false),
                (CaptureKind::ByValue, ClosureKind::FnMut) => (Some(Mutbl::Mut), true),
                (CaptureKind::ByValue, ClosureKind::Fn) => (Some(Mutbl::Not), true),
                (CaptureKind::ByRef, _) => (Some(Mutbl::Not), false),
                // An `FnMut` reaches its `&mut` capture through `&'env mut self`: a reborrow of
                // it lives only for `'env` (the Reference, "Closure types", unique borrows).
                (CaptureKind::ByMutRef, ClosureKind::FnOnce) => (Some(Mutbl::Mut), false),
                (CaptureKind::ByMutRef, _) => (Some(Mutbl::Mut), true),
            };
            match through {
                None => {
                    self.new_local_with(TyD::of(t), true, Some(l), closure)?;
                    match self.pat_local.get_mut(p.ix()) {
                        Some(slot) => *slot = l.0,
                        None => return self.refuse_pat(Form::Malformed, p),
                    }
                }
                Some(m) => {
                    let rt = TyD { wrap: Some(m), ty: t };
                    let start = self.origs.len() as u32;
                    self.new_local_with(rt, true, Some(l), closure)?;
                    if env {
                        env_refs.push(start);
                    }
                    let at = self.place_of(l);
                    let place = self.project(at, rt, Proj::Deref, TyD::of(t), false, closure)?;
                    self.upvar_places.push((p, place, TyD::of(t)));
                }
            }
            self.vars.push(l);
        }
        let upvar_origins = self.origs.len();
        // The parameters: their recorded types, every position a placeholder.
        for (i, &p) in params.iter().enumerate() {
            let t = match ck.pat_ty.get(p.ix()) {
                Some(&t) => TyD::of(t),
                None => return self.refuse_pat(Form::Malformed, p),
            };
            let l = Local(1 + nu + i as u32);
            self.new_local_with(t, true, Some(l), closure)?;
            self.vars.push(l);
        }
        // The return place: the body's value, as a fn body's is.
        let rt = TyD::of(self.adjusted_ty(inner)?);
        let ret_from = self.origs.len();
        self.new_local_with(rt, true, Some(Local(0)), closure)?;
        let all = self.origs.len();
        if (all - ret_from) as u64 * upvar_origins as u64 > u64::from(MOST_POSITIONS) {
            return self.refuse(Form::Bound, closure);
        }
        for u in 0..upvar_origins {
            if env_refs.contains(&(u as u32)) {
                continue;
            }
            for o in ret_from..all {
                self.b.known(self.origs[u], self.origs[o]);
            }
        }
        // Each argument's and the return place's implied bounds.
        let mut ranges: Vec<(u32, u32), A> = Vec::new_in(self.alloc.clone());
        for l in 0..=self.nargs {
            let (start, _) = self.local_origs[l as usize];
            let t = self.local_ty[l as usize];
            self.ref_ranges(t, closure, &mut ranges)?;
            for j in 0..ranges.len() {
                let (at, inner_n) = ranges[j];
                let outer = self.origs[(start + at) as usize];
                for k in 1..=inner_n {
                    let o = self.origs[(start + at + k) as usize];
                    if o != outer {
                        self.b.known(o, outer);
                    }
                }
            }
        }
        // The parameters' patterns, bound as a fn body's are.
        for (i, &p) in params.iter().enumerate() {
            let l = Local(1 + nu + i as u32);
            let ty = self.local_ty[l.0 as usize];
            let pl = self.place_of(l);
            match self.body.pats.get(p.ix()) {
                Some(Pat::Bind { ann, sub: None, .. }) if ann.by_ref.is_none() => {
                    self.pat_local[p.ix()] = l.0;
                }
                _ => self.bind_pattern(p, Val { at: At::Place(pl), ty })?,
            }
        }
        self.tasks.push(Task::RootEnd(inner));
        self.tasks.push(Task::Eval(inner, false));
        let body = self.body;
        let fuel = 32 * (body.exprs.len() + body.pats.len() + body.stmts.len()) as u64 + 256;
        let mut steps = 0u64;
        while let Some(t) = self.tasks.pop() {
            steps += 1;
            if steps > fuel {
                return self.refuse(Form::Bound, closure);
            }
            self.step(t)?;
        }
        Ok(())
    }
}
