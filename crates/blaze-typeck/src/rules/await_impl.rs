//! `.await`'s `into_future`: the impl the solver selects for the operand, recorded as the callee.
//!
//! The Reference, "Await expressions": `operand.await` first evaluates
//! `IntoFuture::into_future(operand)` to the future `tmp` it polls. `expr_await`
//! (rules/async_fmt.rs) proves `T: IntoFuture` and projects `<T as IntoFuture>::IntoFuture` and
//! `::Output` through the ordinary solver, which normalizes both from the impl it selects (the
//! library's blanket `impl<F: Future> IntoFuture for F`, core/src/future/into_future.rs, or a
//! type's own impl, as alloy's request builders write: `impl IntoFuture for EthCall<..> { type
//! IntoFuture = EthCallFut<..>; .. }`). It recorded the callee as the TRAIT's `into_future`
//! at `[T]`, so the flow lowering, holding no impl, related a non-blanket `into_future` as a
//! call whose signature is not known (blaze-flow-lower coroutine.rs `await_done`, wiring
//! await-operand-1 F2). Here, at the end sequence (after `select`, before `select_all`), the
//! callee becomes the selected impl's `into_future` at the impl's arguments, the same answer
//! `resolve_instance` gives a ground instance (instance.rs), so the lowering relates that call
//! by its known head and codegen's `method_callee` reads the same row.
//!
//! One path for blanket and user impls (rule 54): every impl of `IntoFuture` in the operand's
//! bucket and the blanket bucket is matched against the operand in a probe (a fresh
//! instantiation of its header unified with the operand) and its where-clauses are asked of the
//! body's own solver (`may_hold`: the body's param env included, so `impl<T: Send> IntoFuture
//! for Req<T>` holds at `Req<U>` under `U: Send`). Exactly one holding impl is the answer.
//!
//! What is not an impl is left as it was, by name, never guessed:
//! - an operand still holding an inference variable: the projection goal is stalled and
//!   `select_all` reports it (E0282/E0283); nothing is selected here;
//! - an operand with no impl bucket (a parameter, an alias, an opaque type): its `IntoFuture` is
//!   a bound's, whose `into_future` no impl defines; the trait's callee stays (the lowering's
//!   relation for an unknown body, a superset of any impl's);
//! - no impl holds: the trait goal itself fails in `select_all` (E0277);
//! - two impls hold: refused by name here, E0283 at the `.await` naming `into_future` (Rust has
//!   no `IntoFuture` specialization in the library; coherence (E0119) refuses two such impls in
//!   a crate, so this is a world that is already wrong, never a choice to make).

use crate::diag::{About, At, DiagCode};
use crate::error::Stop;
use crate::ids::{ItemKey, LangItem, Name};
use crate::machine::{Lower, Machine};
use crate::ty::{Cause, Obligation, TyList, TypeCtx};
use crate::world::{ImplPolarity, Shape, World};

/// The impls of `IntoFuture` that hold at one `.await`'s operand: the first, and how many.
#[derive(Clone, Copy)]
struct Holding {
    first: Option<ItemKey>,
    n: u32,
}

impl<W: World, C: TypeCtx> Machine<'_, '_, '_, '_, W, C> {
    /// Each `.await`'s callee row (`expr_await`'s `IntoFuture::into_future` at `[T]`) becomes the
    /// selected impl's `into_future` at the impl's arguments (the module doc). A work list over
    /// the rows, no recursion.
    pub(crate) fn await_impls(&mut self) -> Result<(), Stop> {
        if self.awaited.is_empty() {
            return Ok(());
        }
        let Some(tr) = self.lang(LangItem::IntoFuture)? else { return Ok(()) };
        for i in 0..self.awaited.len() {
            let e = self.awaited[i].0;
            let mut row = None;
            for j in 0..self.callees.len() {
                if self.callees[j].0 == e {
                    row = Some(j);
                    break;
                }
            }
            // `expr_await` pushes the callee with the row: none is a body already in error.
            let Some(j) = row else { continue };
            let args = self.callees[j].2;
            if self.ty.list_len(args) == 0 || self.ty.list_has_infer(args) {
                continue;
            }
            let t = self.ty.list_get(args, 0);
            let t = self.ty.resolve(t)?;
            let Some(shape) = self.shape(t) else { continue };
            let h = self.holding_into_future(tr, shape, args)?;
            match (h.n, h.first) {
                (1, Some(imp)) => {
                    let Some(iargs) = self.instantiate_impl_at(imp, tr, args)? else {
                        // It matched in the probe: the same unification cannot fail outside it.
                        return Err(crate::error::CheckError::Malformed(crate::error::Malformed::Expr(e)).into());
                    };
                    // The selected impl's where-clauses are owed here, as selection's nested
                    // goals are (the probe only asked them, then rolled back). A parameter
                    // neither the self type nor the trait names is decided by one of them
                    // alone: reth's `impl<L, Target, N, AddOns, B> IntoFuture for
                    // DebugNodeLauncherFuture<L, Target, N, B> where L: LaunchNode<Target,
                    // Node = NodeHandle<N, AddOns>>` binds `AddOns` only by that projection.
                    // Unregistered, the callee row kept `?AddOns` and writeback refused it
                    // (E0282, tempo crates/zones/node/tests/it/utils.rs:2658). Filed at the
                    // `.await` (`Cause::Expr`, not `Cause::Clause`: the row's item is
                    // `into_future`, not the impl whose clause it is).
                    let wc = self.reads.predicates(imp)?;
                    for c in wc.clauses {
                        if let Some(goal) = self.clause_goal(wc.types, *c, iargs, Cause::Body)? {
                            self.ty.register(Obligation { goal, cause: Cause::Expr(e) })?;
                        }
                    }
                    // An impl that does not define `into_future` runs the trait's provided one
                    // (`resolve_instance`'s rule): the trait's callee stays.
                    if let Some(a) = self.reads.assoc_item(imp, Name::of("into_future"))? {
                        self.callees[j] = (e, a.item, iargs);
                    }
                }
                (0, _) => {}
                _ => {
                    self.diag(DiagCode::CannotSatisfy, At::Expr(e), About::expected_named(t, Name::of("into_future")));
                }
            }
        }
        Ok(())
    }

    /// Every impl of `tr` in `shape`'s bucket and the blanket bucket that holds at `args`
    /// (self first), each tried in a probe rolled back.
    fn holding_into_future(&mut self, tr: ItemKey, shape: Shape, args: TyList) -> Result<Holding, Stop> {
        let mut h = Holding { first: None, n: 0 };
        let buckets = [shape, Shape::Param];
        for (bi, &b) in buckets.iter().enumerate() {
            if bi == 1 && b == shape {
                break;
            }
            let impls = self.reads.impls_of(tr, b)?;
            for &imp in impls {
                if bi == 1 && h.first == Some(imp) {
                    continue;
                }
                let s = self.snapshot();
                let holds = self.impl_holds_at(imp, tr, args)?;
                self.rollback_to(s);
                if holds {
                    if h.n == 0 {
                        h.first = Some(imp);
                    }
                    h.n += 1;
                }
            }
        }
        Ok(h)
    }

    /// Whether impl `imp`'s header matches `args[0]: tr<args[1..]>` and each of its
    /// where-clauses may hold in the body's solver. Binds variables: the caller rolls back.
    fn impl_holds_at(&mut self, imp: ItemKey, tr: ItemKey, args: TyList) -> Result<bool, Stop> {
        let Some(iargs) = self.instantiate_impl_at(imp, tr, args)? else { return Ok(false) };
        let wc = self.reads.predicates(imp)?;
        for c in wc.clauses {
            let Some(goal) = self.clause_goal(wc.types, *c, iargs, Cause::Body)? else { continue };
            if !self.ty.may_hold(goal, &mut self.reads)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `imp`'s generic arguments, fresh, with its header unified with `args[0]: tr<args[1..]>`;
    /// `None` when it is another trait's, not positive, or does not unify (nothing is rolled
    /// back here: a probe's caller does that).
    fn instantiate_impl_at(&mut self, imp: ItemKey, tr: ItemKey, args: TyList) -> Result<Option<TyList>, Stop> {
        let Some(h) = self.reads.impl_header(imp)? else { return Ok(None) };
        if h.trait_ != Some(tr) || h.polarity != ImplPolarity::Positive || h.trait_args.list().len + 1 != self.ty.list_len(args) {
            return Ok(None);
        }
        let iargs = self.fresh_generic_args(imp, 0, h.generics)?;
        let impl_self = self.lower(h.types, h.self_ty, Lower::Args(iargs), Cause::Body)?;
        let operand = self.ty.list_get(args, 0);
        if !self.ty.unify(impl_self, operand)? {
            return Ok(None);
        }
        let rest = h.types.list(h.trait_args.list())?;
        for (i, &p) in rest.iter().enumerate() {
            let t = self.lower(h.types, p, Lower::Args(iargs), Cause::Body)?;
            let want = self.ty.list_get(args, i as u32 + 1);
            if !self.ty.unify(t, want)? {
                return Ok(None);
            }
        }
        Ok(Some(iargs))
    }
}
