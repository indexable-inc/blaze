//! An unsizing refused only because a future's SUPERSET interior misses an auto trait is not
//! refused: it is the interior gap (rules/async_fn.rs, 2), never a mismatch.
//!
//! The rule. The Reference, "Type coercions", "Unsized Coercions": "`T` to `dyn U`, when `T`
//! implements `U + Sized`, and `U` is dyn compatible", so `Pin<Box<F>>` to `Pin<Box<dyn
//! Future<Output = ()> + Send>>` asks `F: Send`; "Auto traits": a future is `Send` when what its
//! state holds is; "Impl trait", "Auto trait leakage": an `async fn`'s opaque is `Send` when its
//! hidden future is. The pinned library: core/src/fmt/mod.rs `impl !Send for Arguments<'_> {}`.
//!
//! The class (rung s9-app-03, std census 3, class C). A future's interior is fixed as a
//! SUPERSET of what it holds across an `.await` (rules/coroutine.rs: every type the body
//! names): `async fn worker(..) { for i in 0..n { sleep(p).await; log(format!("{i}")); } }`
//! holds the `fmt::Arguments` temporary of a statement AFTER the `.await` in that superset,
//! though its storage is dead at every `.await`. A caller's `Box::pin(worker(..))` coerced to
//! `Pin<Box<dyn Future<Output = ()> + Send>>` then asked the solver `Pin<Box<O>>:
//! CoerceUnsized<..>` with `O`'s hidden future's superset interior known (the defining body's
//! record), got a definite `No` from `O: Send`, and the site fell to unification: E0308
//! "found Pin<Box<Opaque>>, expected Pin<Box<Dynamic>>" for a program Rust accepts. An async
//! block of the asking body is not affected at the site: its interior is a variable until the
//! end sequence fixes it, so its auto traits are ambiguous there and the goal is registered.
//!
//! The decision here. A `No` to the site's question `a: Unsize<b>` / `a: CoerceUnsized<b>` is
//! taken as the answer unless BOTH: `a` holds a future whose interior was fixed elsewhere (an
//! async block's `Future`, another body's `CoroutineOf`, or an opaque, whose hidden type may be
//! one), and the same question with every auto trait dropped from the target's objects MAY
//! hold. Then the only thing refused is an auto trait the source must meet, of a type holding a
//! superset interior: the coercion is taken, the goal registered as it is, and its failure at
//! the end of the check is the typed gap [`crate::INTERIOR_GAP`] ([`Machine::unsize_interior_gap`]),
//! never E0277 and never E0308. A wrong `Output` (`dyn Future<Output = u16>` from a `u8`
//! future) still fails without the auto traits: the mismatch stands.
//!
//! Flow lowering supplies a local block's saved types. The obligation recorded for a local
//! replay includes the target and every object predicate. A foreign coroutine record can
//! still contain a provisional witness when its defining body's flow lowering was refused.

use blaze_own::{Alloc, Vec};

use super::async_fn::Witness;
use crate::diag::{At, Because, DiagCode, PendingDiag, PendingGoal, UnmetGoal};
use crate::error::{CheckError, Limit, Stop};
use crate::ids::{ItemKey, LangItem};
use crate::infer::InferCtx;
use crate::instance::{ClosureSource, import, import_list};
use crate::machine::Machine;
use crate::portable::PTys;
use crate::ty::{FulfillError, Goal, Ty, TyList, TyView, TypeCtx};
use crate::world::World;

/// The most type nodes one walk here visits (rule 18). Interned types are acyclic, so only a
/// shared subtree met many times reaches it; past it the walk refuses by the limit, never
/// guesses.
const MOST_NODES: u32 = 1 << 16;

/// One step of [`Machine::without_auto_traits`]'s post-order rebuild.
#[derive(Clone, Copy)]
enum Step {
    /// Visit `t`: push its rebuilt form, or its children then its `Exit`.
    Enter(Ty),
    /// Rebuild `t` from its children's rebuilt forms, the last ones on the results stack.
    Exit(Ty),
}

impl crate::stalled::Stalled {
    /// Import the same predicate that failed before flow lowering. Writeback already
    /// exports its complete argument list with the finding. A where-const reason on
    /// a unary auto-trait obligation carries the reason instead of an unmet predicate.
    pub(crate) fn import_witness_goal<S: ClosureSource>(
        &self,
        cx: &mut InferCtx<'_>,
        src: &mut S,
        types: PTys<'_>,
    ) -> Result<Goal, S::Error> {
        let args = match self.diag.because {
            Some(Because::Unmet { goal: UnmetGoal::Trait { trait_, args }, .. })
                if trait_ == self.trait_ && types.list(args)?.first() == Some(&self.ty) =>
            {
                import_list(cx, src, types, args, &[], 0)?
            }
            Some(Because::False(_) | Because::Error(_) | Because::Generic(_)) => {
                // Only unary auto goals keep a where-const reason in the stalled row.
                let ty = import(cx, src, types, self.ty, &[], 0)?;
                cx.mk_list(&[ty])?
            }
            _ => return Err(CheckError::Malformed(crate::error::Malformed::StalledGoal {
                trait_: self.trait_, at: self.diag.at,
            }).into()),
        };
        Ok(Goal::Trait { trait_: self.trait_, args })
    }
}

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// Whether a `No` to `args[0]: tr<args[1]>` (`tr` the `Unsize` or `CoerceUnsized` lang
    /// trait) may rest only on an auto trait of a superset interior (the module doc).
    pub(crate) fn unsize_no_may_rest_on_interior(&mut self, tr: ItemKey, args: TyList) -> Result<bool, Stop> {
        if self.ty.list_len(args) != 2 {
            return Ok(false);
        }
        let (a, b) = (self.ty.list_get(args, 0), self.ty.list_get(args, 1));
        if !self.holds_fixed_interior(a)? {
            return Ok(false);
        }
        let Some(b_free) = self.without_auto_traits(b)? else { return Ok(false) };
        let free = self.mk_list(&[a, b_free])?;
        Ok(self.ty.may_hold(Goal::Trait { trait_: tr, args: free }, &mut self.reads)?)
    }

    /// For a goal that FAILED at the end of the check: the interior gap when it is an
    /// `Unsize` / `CoerceUnsized` goal whose failure may rest only on a superset interior
    /// (the module doc).
    pub(crate) fn unsize_interior_gap(&mut self, goal: Goal) -> Result<Witness, Stop> {
        let Goal::Trait { trait_, args } = goal else { return Ok(Witness::Fixed) };
        let unsize = self.lang(LangItem::Unsize)?;
        let coerce = self.lang(LangItem::CoerceUnsized)?;
        if unsize != Some(trait_) && coerce != Some(trait_) {
            return Ok(Witness::Fixed);
        }
        if self.unsize_no_may_rest_on_interior(trait_, args)? {
            // A missing bound on an arbitrary source capture does not prove that the
            // corresponding object in the target requires that bound of that capture.
            // Keep the solver's No when the full goal already used fixed witnesses.
            return self.witness_source(self.ty.list_get(args, 0));
        }
        Ok(Witness::Fixed)
    }

    /// Keep the entire failed coercion at its site. Replaying just the auto trait would
    /// lose the pointer impl's other bounds and the object's associated-type equalities.
    /// Only the current body's futures get a new witness when its flow is lowered.
    pub(crate) fn defer_local_unsizing(&mut self, err: FulfillError, at: At) -> bool {
        if self.interior != crate::stalled::Interior::Defer || err.because.is_some() {
            return false;
        }
        let Goal::Trait { trait_, args } = err.obligation.goal else { return false };
        let source = self.ty.list_get(args, 0);
        let diag = PendingDiag::unmet(DiagCode::Unsatisfied, at, Some(source), PendingGoal::Trait { trait_, args }, None);
        self.stalled.push(crate::stalled::PendingStalled { trait_, ty: source, diag });
        true
    }

    /// Whether `t` is, or holds anywhere inside, a future whose interior may be a superset: an
    /// async block's `Future`, another body's `CoroutineOf`, or an opaque type (its hidden type
    /// is read by the solver, not here, and may be either).
    fn holds_fixed_interior(&mut self, t: Ty) -> Result<bool, Stop> {
        let mut work: Vec<Ty, Alloc<'a>> = Vec::new_in(self.alloc);
        work.push(t);
        let mut met = 0u32;
        while let Some(x) = work.pop() {
            met += 1;
            if met > MOST_NODES {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            let x = self.ty.shallow_resolve(x);
            let v = self.view(x);
            if matches!(v, TyView::Future(..) | TyView::CoroutineOf { .. } | TyView::Opaque(..)) {
                return Ok(true);
            }
            // A closure holds its captures, its environment (the Reference, "Closure types":
            // "a closure is `Send` if all of its captures are `Send`"), which `ty_children`
            // does not reach (it walks the signature): turso sync/engine
            // database_sync_lazy_storage.rs:589, `Box::new(move || generator.resume_with(..))`
            // to `Box<dyn FnMut() -> bool + Send>`, the `Gen` captured holding an async block's
            // `Pin<Box<F>>`, was E0283 on the block's undecided interior, never deferred to
            // its exact one. The same walk as `stored_witness_source` (rules/async_fn.rs).
            if let Some((_, Some(env))) = self.closure_witness(v) {
                work.push(env);
            }
            self.ty_children(v, &mut work);
        }
        Ok(false)
    }

    /// `t` with every `ExAuto` row dropped from each trait object reached through ADT
    /// arguments, references and raw pointers (the paths rules/coerce_nested.rs walks to find
    /// the object); `None` when no object there names an auto trait. A post-order rebuild on
    /// an explicit stack, no recursion.
    fn without_auto_traits(&mut self, t: Ty) -> Result<Option<Ty>, Stop> {
        let mut work: Vec<Step, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut done: Vec<Ty, Alloc<'a>> = Vec::new_in(self.alloc);
        work.push(Step::Enter(t));
        let mut dropped = false;
        let mut met = 0u32;
        while let Some(step) = work.pop() {
            met += 1;
            if met > MOST_NODES {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            match step {
                Step::Enter(x) => {
                    let x = self.ty.shallow_resolve(x);
                    match self.view(x) {
                        TyView::Dynamic(preds) => {
                            let rows = self.list(preds);
                            let mut kept: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(rows.len(), self.alloc);
                            for &row in rows.iter() {
                                if !matches!(self.view(row), TyView::ExAuto(_)) {
                                    kept.push(row);
                                }
                            }
                            if kept.len() == rows.len() {
                                done.push(x);
                            } else {
                                dropped = true;
                                let l = self.mk_list(&kept)?;
                                let y = self.mk(TyView::Dynamic(l))?;
                                done.push(y);
                            }
                        }
                        TyView::Adt(_, args) => {
                            work.push(Step::Exit(x));
                            let args = self.list(args);
                            // Reversed, so the first argument is rebuilt first and the results
                            // stack holds them in order.
                            for &arg in args.iter().rev() {
                                work.push(Step::Enter(arg));
                            }
                        }
                        TyView::Ref(_, pointee) | TyView::Ptr(_, pointee) => {
                            work.push(Step::Exit(x));
                            work.push(Step::Enter(pointee));
                        }
                        _ => done.push(x),
                    }
                }
                Step::Exit(x) => {
                    let y = match self.view(x) {
                        TyView::Adt(k, args) => {
                            let old = self.list(args);
                            let Some(start) = done.len().checked_sub(old.len()) else {
                                return Err(CheckError::Limit(Limit::Steps).into());
                            };
                            let changed = done[start..].iter().zip(old.iter()).any(|(n, o)| n != o);
                            let y = if changed {
                                let l = self.mk_list(&done[start..])?;
                                self.mk(TyView::Adt(k, l))?
                            } else {
                                x
                            };
                            done.truncate(start);
                            y
                        }
                        TyView::Ref(m, pointee) | TyView::Ptr(m, pointee) => {
                            let Some(new) = done.pop() else { return Err(CheckError::Limit(Limit::Steps).into()) };
                            if new == pointee {
                                x
                            } else if matches!(self.view(x), TyView::Ref(..)) {
                                self.mk(TyView::Ref(m, new))?
                            } else {
                                self.mk(TyView::Ptr(m, new))?
                            }
                        }
                        // Only the three shapes above push an `Exit`.
                        _ => x,
                    };
                    done.push(y);
                }
            }
        }
        if !dropped {
            return Ok(None);
        }
        Ok(done.pop())
    }
}
