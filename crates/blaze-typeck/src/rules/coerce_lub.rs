//! The least upper bound of two fn items (or a fn item and a closure that captures nothing) at
//! a coercion site with several sources: rustc_hir_typeck/src/coercion.rs,
//! `try_find_coercion_lub`'s first case ("Function items or non-capturing closures of differing
//! IDs or GenericArgs").
//!
//! The Reference, "Type coercions", "Least upper bound coercions", and "Function item types":
//! each fn item has its own zero-sized type, and where the arms of an `if` or a `match` are two
//! different fn items of one signature, both coerce to the fn pointer of that signature. std's
//! own source rests on it: library/std/src/sys/sync/rwlock/queue.rs:362 (`RwLock::
//! lock_contended`), `let update_fn = if write { write_lock } else { read_lock };`, where
//! `write_lock` and `read_lock` are both `fn(State) -> Option<State>` (queue.rs:145, :151).
//! Before this case the second arm was E0308, "found FnDef(read_lock), expected
//! FnDef(write_lock)": the owner's hello world stopped there.
//!
//! The order: the two types unified first (one fn item at equal arguments, or one closure,
//! stays itself: nothing is reified); else each side's fn pointer (a fn item's with its own
//! header; a closure's with the Rust ABI and, beside a fn item, that item's safety, else safe:
//! rustc's `closure_sig_as_fn_ptr_ty` with `b_sig.safety()` / `Safety::Safe`), the two pointers
//! unified as their least upper bound (regions are erased, so unification is the lub); every
//! earlier source of the site and the new one record the pointer as their adjustment
//! (`ReifyFnPointer` or `ClosureFnPointer`). Two pointers that do not unify (another signature,
//! ABI or safety) are the site's mismatch: nothing else is tried after this case, as in rustc.
//! A closure that captures anything is not this case (rustc's `is_capturing_closure`).

use crate::columns::AdjustKind;
use crate::error::Stop;
use crate::ids::{ExprId, FnPtrHeader, Safety};
use crate::machine::Machine;
use crate::ty::{Cause, Ty, TyView, TypeCtx};
use crate::world::World;

/// What [`Machine::fn_lub`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FnLub {
    /// One side is neither a fn item nor a closure that captures nothing: the site goes on to
    /// its plain coercions.
    NotFns,
    /// The site's type is now this (the merged type itself, or the fn pointer both reify to).
    Merged(Ty),
    /// Both are fns, and their pointers do not unify: the site's mismatch.
    Mismatch,
}

/// One side of the lub, before it is reified.
#[derive(Clone, Copy, Debug)]
enum Side {
    /// A fn item and its arguments.
    Item(crate::ids::ItemKey, crate::ty::TyList),
    /// A closure that captures nothing, and its signature's list (inputs then output).
    Closure(crate::ty::TyList),
}

impl<W: World, C: TypeCtx> Machine<'_, '_, '_, '_, W, C> {
    /// The fn-item case of the lub of `merged` (the site's type so far) and `ty` (source `e`'s
    /// type) at site `site`; the module doc says the order.
    pub(crate) fn fn_lub(&mut self, site: u32, e: ExprId, merged: Ty, ty: Ty) -> Result<FnLub, Stop> {
        let Some(a) = self.fn_side(merged)? else { return Ok(FnLub::NotFns) };
        let Some(b) = self.fn_side(ty)? else { return Ok(FnLub::NotFns) };
        if self.commit_if(|m| Ok::<_, Stop>(if m.unify(merged, ty)? { Some(()) } else { None }))?.is_some() {
            return Ok(FnLub::Merged(merged));
        }
        let cause = Cause::Expr(e);
        let Some(pa) = self.side_ptr(a, b, cause)? else { return Ok(FnLub::Mismatch) };
        let Some(pb) = self.side_ptr(b, a, cause)? else { return Ok(FnLub::Mismatch) };
        if self.commit_if(|m| Ok::<_, Stop>(if m.unify(pa, pb)? { Some(()) } else { None }))?.is_none() {
            return Ok(FnLub::Mismatch);
        }
        let ka = Self::reify_kind(a);
        let kb = Self::reify_kind(b);
        // Every earlier source of this site had the merged type with no adjustment of its own
        // (a `!` source keeps its `NeverToAny` and is not listed: rustc's `apply_adjustments`
        // keeps a `NeverToAny`); each now reifies to the pointer.
        for i in 0..self.coerce_sources.len() {
            let (s, src) = self.coerce_sources[i];
            if s == site {
                self.adjust.push((src, ka, pa));
            }
        }
        self.adjust.push((e, kb, pa));
        Ok(FnLub::Merged(pa))
    }

    /// `t` as a side of the lub: a fn item, or a closure that captures nothing; else `None`.
    fn fn_side(&mut self, t: Ty) -> Result<Option<Side>, Stop> {
        match self.view(t) {
            TyView::FnDef(k, args) => Ok(Some(Side::Item(k, args))),
            TyView::Closure(c, sig) => {
                let TyView::FnPtr(l, _) = self.view(sig) else { return Ok(None) };
                if !self.captures_nothing(c)? {
                    return Ok(None);
                }
                Ok(Some(Side::Closure(l)))
            }
            _ => Ok(None),
        }
    }

    /// The fn pointer `side` reifies to beside `other`; `None` when a fn item's signature is
    /// not in the world (no pointer, so no lub).
    fn side_ptr(&mut self, side: Side, other: Side, cause: Cause) -> Result<Option<Ty>, Stop> {
        match side {
            Side::Item(k, args) => {
                let Some(sig) = self.reads.fn_sig(k)? else { return Ok(None) };
                let header = sig.header;
                let Some((inputs, output)) = self.fn_def_sig(k, args, cause)? else {
                    return Ok(None);
                };
                let mut all = self.list(inputs);
                all.push(output);
                let list = self.mk_list(&all)?;
                Ok(Some(self.mk(TyView::FnPtr(list, header))?))
            }
            Side::Closure(l) => {
                let safety = match other {
                    Side::Item(k, _) => match self.reads.fn_sig(k)? {
                        Some(sig) => sig.header.safety,
                        None => return Ok(None),
                    },
                    Side::Closure(_) => Safety::Safe,
                };
                Ok(Some(self.mk(TyView::FnPtr(l, FnPtrHeader { safety, ..FnPtrHeader::RUST }))?))
            }
        }
    }

    fn reify_kind(side: Side) -> AdjustKind {
        match side {
            Side::Item(..) => AdjustKind::ReifyFnPointer,
            Side::Closure(_) => AdjustKind::ClosureFnPointer,
        }
    }
}
