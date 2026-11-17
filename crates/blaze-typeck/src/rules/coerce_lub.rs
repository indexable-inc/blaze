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
//! A fn item is reified by the one rule (rules/reify.rs): an intrinsic on either side is
//! refused by name at its own source, and the site becomes the error type.
//!
//! The other direction ([`Machine::lub_earlier_to_new`]; the Reference, "Least upper bound
//! coercions"; rustc's `try_find_coercion_lub`, "Then try to coerce the previous expressions to
//! the type of the new one"): when the new source does not coerce to the site's type, every
//! earlier source coerces to the new source's type instead, each recording the adjustments
//! (`if c { f } else { p }` with `p: fn(u8)`: `f` reifies, the `if` is `fn(u8)`). Only when no
//! earlier source carries an adjustment of its own, a noop reborrow aside; else the plain lub
//! (equal types). A noop reborrow (rustc's `try_find_coercion_lub`: "other than noop
//! reborrows") is a builtin deref and a borrow of the mutability of the source's own reference
//! type (`&mut *b` where `b: &mut T`, recorded when `b` met an earlier `&mut T`): it is
//! REPLACED by the new adjustments, which begin with that same deref (rustc's
//! `apply_adjustments`: "A reborrow has no effect before a dereference"): its two rows are
//! overwritten in place and the rest pushed. In place is sound: this runs at the walk's
//! coercion site, never inside a probe (a probe is a closure over the machine, and the walk's
//! frames are not inside one), so no snapshot taken before those rows is rolled back over them;
//! rules/mut_places.rs writes adjustment rows in place the same way.
//!
//! A safe `#[target_feature]` fn's pointer is `unsafe` outside its features
//! (rules/target_feature.rs): beside a safe fn item, where only safety sets the two pointers
//! apart, the site is Rust's refusal by name at the gated item
//! ([`DiagCode::TargetFeatureToSafeFnPointer`]); two gated items meet at `unsafe fn(..)`; a
//! closure beside a gated item takes `unsafe` (rustc's `closure_sig_as_fn_ptr_ty` with the
//! item's signature's safety).

use crate::columns::AdjustKind;
use crate::diag::{About, At, DiagCode};
use crate::error::Stop;
use crate::ids::{ExprId, FnPtrHeader, Safety};
use crate::machine::Machine;
use crate::rules::reify::Reify;
use crate::rules::target_feature::Gate;
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

/// An earlier source's adjustments, as the lub's second direction sees them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Earlier {
    /// None.
    Plain,
    /// A noop reborrow (the module doc): the rows of its deref and its borrow.
    Reborrow(usize, usize),
    /// Any other: the plain lub only.
    Other,
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
        // The site's side is named at its first source, the new side at `e`.
        let cause_a = Cause::Expr(self.first_source(site).unwrap_or(e));
        let (pa, ga) = match self.side_ptr(merged, a, b, cause_a)? {
            Reify::Ptr { ty, gated, .. } => (ty, gated),
            Reify::NoSig => return Ok(FnLub::Mismatch),
            Reify::Refused => return Ok(FnLub::Merged(self.common.err)),
        };
        let (pb, gb) = match self.side_ptr(ty, b, a, cause)? {
            Reify::Ptr { ty, gated, .. } => (ty, gated),
            Reify::NoSig => return Ok(FnLub::Mismatch),
            Reify::Refused => return Ok(FnLub::Merged(self.common.err)),
        };
        if self.commit_if(|m| Ok::<_, Stop>(if m.unify(pa, pb)? { Some(()) } else { None }))?.is_none() {
            // One side a gated `#[target_feature]` fn, the other safe, and nothing else apart:
            // Rust's refusal by name at the gated side's source (the module doc); the site is
            // the error type, so no second finding follows.
            if ga != gb {
                let (gated_ptr, other_ptr, at, found) = if ga { (pa, pb, cause_a, merged) } else { (pb, pa, cause, ty) };
                if self.only_safety_apart(gated_ptr, other_ptr)? {
                    self.diag(DiagCode::TargetFeatureToSafeFnPointer, At::of(at), About::types(other_ptr, found));
                    return Ok(FnLub::Merged(self.common.err));
                }
            }
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

    /// The fn pointer `side` (of type `t`) reifies to beside `other`: a fn item by the one rule
    /// (rules/reify.rs, which refuses an intrinsic by name at `cause`); [`Reify::NoSig`] when a
    /// fn item's signature is not in the world (no pointer, so no lub).
    fn side_ptr(&mut self, t: Ty, side: Side, other: Side, cause: Cause) -> Result<Reify, Stop> {
        match side {
            Side::Item(k, args) => self.reify_fn_item(t, k, args, cause),
            Side::Closure(l) => {
                let safety = match other {
                    Side::Item(k, _) => {
                        let own = match self.reads.fn_sig(k)? {
                            Some(sig) => sig.header.safety,
                            None => return Ok(Reify::NoSig),
                        };
                        let attrs = self.reads.attrs(k)?;
                        match self.feature_gate(attrs, own)? {
                            Gate::Open => own,
                            // The item's pointer is `unsafe` here; a body with features of its
                            // own is refused at the item's own side (rules/reify.rs).
                            Gate::Unsafe | Gate::FeatureBody => Safety::Unsafe,
                        }
                    }
                    Side::Closure(_) => Safety::Safe,
                };
                let header = FnPtrHeader { safety, ..FnPtrHeader::RUST };
                let ty = self.mk(TyView::FnPtr(l, header))?;
                Ok(Reify::Ptr { ty, list: l, header, gated: false })
            }
        }
    }

    /// The first source of site `site` that is listed (not `!`), if any.
    fn first_source(&self, site: u32) -> Option<ExprId> {
        for i in 0..self.coerce_sources.len() {
            let (s, src) = self.coerce_sources[i];
            if s == site {
                return Some(src);
            }
        }
        None
    }

    /// Whether the `unsafe` fn pointer `gated` would unify with `other` were it safe (asked in a
    /// probe: nothing bound).
    fn only_safety_apart(&mut self, gated: Ty, other: Ty) -> Result<bool, Stop> {
        let TyView::FnPtr(l, h) = self.view(gated) else { return Ok(false) };
        let safe = self.mk(TyView::FnPtr(l, FnPtrHeader { safety: Safety::Safe, ..h }))?;
        self.probe(|m| m.unify(safe, other))
    }

    /// Earlier source `src`'s adjustments so far (the module doc): none, a noop reborrow (its
    /// two rows), or any other. One scan of the rows, off the common path.
    fn earlier_adjust(&mut self, src: ExprId) -> Result<Earlier, Stop> {
        let mut rows = [0usize; 2];
        let mut n = 0usize;
        let es = self.adjust.column::<0>();
        for i in 0..es.len() {
            if es[i] != src {
                continue;
            }
            if n == 2 {
                return Ok(Earlier::Other);
            }
            rows[n] = i;
            n += 1;
        }
        match n {
            0 => return Ok(Earlier::Plain),
            2 => {}
            _ => return Ok(Earlier::Other),
        }
        let ks = self.adjust.column::<1>();
        let (k0, k1) = (ks[rows[0]], ks[rows[1]]);
        let AdjustKind::Deref { overloaded: None } = k0 else { return Ok(Earlier::Other) };
        let AdjustKind::Borrow { mutbl, raw: false, .. } = k1 else { return Ok(Earlier::Other) };
        let own = self.ty.shallow_resolve(self.expr_ty[src.ix()]);
        Ok(match self.view(own) {
            TyView::Ref(m, _) if m == mutbl => Earlier::Reborrow(rows[0], rows[1]),
            _ => Earlier::Other,
        })
    }

    /// The lub's other direction (the module doc): every earlier source of site `site`, whose
    /// type is the site's `merged`, coerced to the new source's type `ty`, each recording the
    /// same adjustments (a noop reborrow's two rows overwritten by the first two); `Some(ty)`
    /// when taken. `None`: not taken and nothing bound or written (an earlier source carries an
    /// adjustment other than a noop reborrow, there is none, a side is unresolved, they do not
    /// coerce, or a reborrow is there and the new adjustments do not begin with a builtin deref
    /// and a second row). Runs only after the new source failed to coerce to `merged`, so the
    /// scans of the adjustments per earlier source are off the common path.
    pub(crate) fn lub_earlier_to_new(&mut self, site: u32, merged: Ty, ty: Ty) -> Result<Option<Ty>, Stop> {
        #[cfg(test)]
        let reborrow_on = self.plant != crate::machine::Plant::LubReborrowOff;
        #[cfg(not(test))]
        let reborrow_on = true;
        let mut first = None;
        let mut reborrows = false;
        for i in 0..self.coerce_sources.len() {
            let (s, src) = self.coerce_sources[i];
            if s != site {
                continue;
            }
            match self.earlier_adjust(src)? {
                Earlier::Plain => {}
                Earlier::Reborrow(..) if reborrow_on => reborrows = true,
                Earlier::Reborrow(..) | Earlier::Other => return Ok(None),
            }
            if first.is_none() {
                first = Some(src);
            }
        }
        let Some(first) = first else { return Ok(None) };
        if self.is_err(merged) || self.is_never(merged) || self.is_ty_var(merged) || self.is_ty_var(ty) {
            return Ok(None);
        }
        let cause = Cause::Expr(first);
        let got = self.commit_if(|m| {
            if m.try_coerce(merged, ty, false, cause)?.is_none() {
                return Ok(None);
            }
            // A reborrow is replaced only by adjustments that begin with its own deref.
            if reborrows
                && (m.adj_scratch.len() < 2
                    || !matches!(m.adj_scratch[0].0, AdjustKind::Deref { overloaded: None }))
            {
                return Ok(None);
            }
            Ok::<_, Stop>(Some(()))
        })?;
        if got.is_none() {
            self.adj_scratch.clear();
            return Ok(None);
        }
        let n = self.adj_scratch.len();
        for i in 0..self.coerce_sources.len() {
            let (s, src) = self.coerce_sources[i];
            if s != site {
                continue;
            }
            let mut from = 0;
            if let Earlier::Reborrow(d, b) = self.earlier_adjust(src)? {
                // The two rows are overwritten in place (the module doc says why that is sound).
                let (k0, t0) = self.adj_scratch[0];
                let (k1, t1) = self.adj_scratch[1];
                self.adjust.column_mut::<1>()[d] = k0;
                self.adjust.column_mut::<2>()[d] = t0;
                self.adjust.column_mut::<1>()[b] = k1;
                self.adjust.column_mut::<2>()[b] = t1;
                from = 2;
            }
            for j in from..n {
                let (kind, target) = self.adj_scratch[j];
                self.adjust.push((src, kind, target));
            }
        }
        self.adj_scratch.clear();
        Ok(Some(ty))
    }

    fn reify_kind(side: Side) -> AdjustKind {
        match side {
            Side::Item(..) => AdjustKind::ReifyFnPointer,
            Side::Closure(_) => AdjustKind::ClosureFnPointer,
        }
    }
}
