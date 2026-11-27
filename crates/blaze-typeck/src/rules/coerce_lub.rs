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
        let mut cur = self.coerce_many[site as usize].first_source;
        while cur != u32::MAX {
            let (src, next) = self.coerce_sources[cur as usize];
            self.adjust.push((src, ka, pa));
            cur = next;
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
        let head = self.coerce_many[site as usize].first_source;
        (head != u32::MAX).then(|| self.coerce_sources[head as usize].0)
    }

    /// Whether the `unsafe` fn pointer `gated` would unify with `other` were it safe (asked in a
    /// probe: nothing bound).
    fn only_safety_apart(&mut self, gated: Ty, other: Ty) -> Result<bool, Stop> {
        let TyView::FnPtr(l, h) = self.view(gated) else { return Ok(false) };
        let safe = self.mk(TyView::FnPtr(l, FnPtrHeader { safety: Safety::Safe, ..h }))?;
        self.probe(|m| m.unify(safe, other))
    }

    /// Earlier source `src`'s adjustments so far (the module doc): none, a noop reborrow (its
    /// two rows), or any other. O(1) via `AdjustLog::rows_rev`.
    fn earlier_adjust(&mut self, src: ExprId) -> Result<Earlier, Stop> {
        let mut rev_rows = [0usize; 2];
        let mut n = 0usize;
        for i in self.adjust.rows_rev(src) {
            if n == 2 {
                return Ok(Earlier::Other);
            }
            rev_rows[n] = i;
            n += 1;
        }
        match n {
            0 => return Ok(Earlier::Plain),
            2 => {}
            _ => return Ok(Earlier::Other),
        }
        // `rows_rev` yields most recent first, so `rev_rows[1]` is the earlier row (Deref)
        // and `rev_rows[0]` is the later row (Borrow).
        let (r0, r1) = (rev_rows[1], rev_rows[0]);
        let ks = self.adjust.column::<1>();
        let (k0, k1) = (ks[r0], ks[r1]);
        let AdjustKind::Deref { overloaded: None } = k0 else { return Ok(Earlier::Other) };
        let AdjustKind::Borrow { mutbl, raw: false, .. } = k1 else { return Ok(Earlier::Other) };
        let own = self.ty.shallow_resolve(self.expr_ty[src.ix()]);
        Ok(match self.view(own) {
            TyView::Ref(m, _) if m == mutbl => Earlier::Reborrow(r0, r1),
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
        let mut cur = self.coerce_many[site as usize].first_source;
        while cur != u32::MAX {
            let (src, next) = self.coerce_sources[cur as usize];
            match self.earlier_adjust(src)? {
                Earlier::Plain => {}
                Earlier::Reborrow(..) if reborrow_on => reborrows = true,
                Earlier::Reborrow(..) | Earlier::Other => return Ok(None),
            }
            if first.is_none() {
                first = Some(src);
            }
            cur = next;
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
        let mut cur = self.coerce_many[site as usize].first_source;
        while cur != u32::MAX {
            let (src, next) = self.coerce_sources[cur as usize];
            let mut from = 0;
            if let Earlier::Reborrow(d, b) = self.earlier_adjust(src)? {
                // The two rows are overwritten in place (the module doc says why that is sound).
                let (k0, t0) = self.adj_scratch[0];
                let (k1, t1) = self.adj_scratch[1];
                self.adjust.set_kind_ty(d, k0, t0);
                self.adjust.set_kind_ty(b, k1, t1);
                from = 2;
            }
            for j in from..n {
                let (kind, target) = self.adj_scratch[j];
                self.adjust.push((src, kind, target));
            }
            cur = next;
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

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use blaze_own::Alloc;

    use crate::columns::AdjustKind;
    use crate::fixture::{Bb, Fx, show};
    use crate::ids::{ExprId, FnPtrHeader, ItemKey, Mutbl, UintTy};
    use crate::infer::InferCtx;
    use crate::portable::PNode;

    fn run_check(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId) -> crate::Checked<'static> {
        let alloc = Alloc::global();
        let mut cx = InferCtx::new_in(alloc);
        match crate::check_with(&b.view(owner, root), fx, &mut cx, &crate::world::Needs::new_in(alloc), crate::machine::Plant::None, alloc).expect("check") {
            crate::Outcome::Done(c) => c,
            crate::Outcome::Need(n) => panic!("unexpected need: {n:?}"),
        }
    }

    #[test]
    fn large_match_and_array_200_coercible_arms_are_deterministic() {
        const ARMS: usize = 200;
        let mut fx = Fx::new();
        let u8_ = fx.ty(PNode::Uint(UintTy::U8));
        let usize_ = fx.ty(PNode::Uint(UintTy::Usize));
        let unit = fx.unit();
        let fn_io = fx.list(&[u8_, u8_]);
        let fn_ptr = fx.ty(PNode::FnPtr(fn_io, FnPtrHeader::RUST));
        let four = fx.ty(PNode::ConstValue { ty: usize_, bits: 4 });
        let arr = fx.ty(PNode::Array(u8_, four));
        let slice = fx.ty(PNode::Slice(u8_));
        let r_arr = fx.r(Mutbl::Mut, arr);
        let r_slice = fx.r(Mutbl::Mut, slice);
        fx.func("f_a", 0, &[u8_], u8_, Vec::new());
        fx.func("f_b", 0, &[u8_], u8_, Vec::new());
        let main = fx.func("main", 0, &[fn_ptr, r_arr, r_slice], unit, Vec::new());
        fx.finish();

        let mut b = Bb::new();
        let p_fn = b.param("p_fn");
        let p_arr = b.param("p_arr");
        let p_slice = b.param("p_slice");

        // 1. 200-arm match: 199 arms of `f_a`, last arm `f_b` -> all 200 reify via `fn_lub`.
        let scr1 = b.bool();
        let mut match_arms = Vec::with_capacity(ARMS);
        let mut match_exprs = Vec::with_capacity(ARMS);
        for i in 0..ARMS {
            let pat = b.wild();
            let arm = b.path(&[if i + 1 < ARMS { "f_a" } else { "f_b" }]);
            match_arms.push((pat, arm));
            match_exprs.push(arm);
        }
        let m1 = b.match_(scr1, &match_arms);
        let f1 = b.bind("f1");
        let s1 = b.let_(f1, None, m1);

        // 2. 200-element array: 199 elements of `f_a`, last element `p_fn: fn(u8) -> u8`
        //    -> all 199 earlier elements reify via `lub_earlier_to_new`.
        let mut arr_elems = Vec::with_capacity(ARMS);
        for i in 0..ARMS {
            arr_elems.push(if i + 1 < ARMS { b.path(&["f_a"]) } else { b.local(p_fn) });
        }
        let arr_expr = b.array(&arr_elems);
        let f2 = b.bind("f2");
        let s2 = b.let_(f2, None, arr_expr);

        // 3. 200-arm match: 199 arms of `p_arr: &mut [u8; 4]` (accumulating noop reborrows
        //    on arms 1..199), last arm `p_slice: &mut [u8]` -> earlier reborrows overwritten
        //    in place and unsized via `lub_earlier_to_new` + `AdjustLog`.
        let scr2 = b.bool();
        let mut reborrow_arms = Vec::with_capacity(ARMS);
        let mut reborrow_exprs = Vec::with_capacity(ARMS);
        for i in 0..ARMS {
            let pat = b.wild();
            let arm = b.local(if i + 1 < ARMS { p_arr } else { p_slice });
            reborrow_arms.push((pat, arm));
            reborrow_exprs.push(arm);
        }
        let m2 = b.match_(scr2, &reborrow_arms);
        let f3 = b.bind("f3");
        let s3 = b.let_(f3, None, m2);

        let root = b.block(&[s1, s2, s3], None);
        let c1 = run_check(&fx, &b, main, root);
        let c2 = run_check(&fx, &b, main, root);
        assert!(c1.diags.is_empty(), "expected no diags, got {:?}", c1.diags);
        assert_eq!(show(&fx, c1.types.view(), c1.local_ty[f1.ix()]), "fn(u8) -> u8");
        assert_eq!(show(&fx, c1.types.view(), c1.local_ty[f3.ix()]), "&mut [u8]");

        // Every arm in `m1` has exactly one `ReifyFnPointer` adjustment.
        let mut m1_reified = 0usize;
        for i in 0..c1.adjusts.len() {
            let e = c1.adjusts.column::<0>()[i];
            let k = c1.adjusts.column::<1>()[i];
            if match_exprs.contains(&e) && k == AdjustKind::ReifyFnPointer {
                m1_reified += 1;
            }
        }
        assert_eq!(m1_reified, ARMS);

        // Every earlier arm in `m2` (0..199) has exactly 3 adjustments: Deref, Borrow(Mut), Unsize
        // (noop reborrows replaced in place, never stacked).
        for &arm in &reborrow_exprs[..ARMS - 1] {
            let kinds: Vec<AdjustKind> = (0..c1.adjusts.len())
                .filter(|&i| c1.adjusts.column::<0>()[i] == arm)
                .map(|i| c1.adjusts.column::<1>()[i])
                .collect();
            assert_eq!(
                kinds,
                std::vec![
                    AdjustKind::Deref { overloaded: None },
                    AdjustKind::Borrow { mutbl: Mutbl::Mut, two_phase: false, raw: false },
                    AdjustKind::Unsize,
                ],
            );
        }

        // Exact determinism across independent runs.
        assert_eq!(c1.adjusts.column::<0>(), c2.adjusts.column::<0>());
        assert_eq!(c1.adjusts.column::<1>(), c2.adjusts.column::<1>());
    }
}
