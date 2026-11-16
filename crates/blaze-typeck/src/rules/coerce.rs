//! Coercions at a single site.
//!
//! Follows rustc_hir_typeck/src/coercion.rs, `Coerce::coerce` and its order: error types pass;
//! `!` becomes any type (`NeverToAny`, the target recorded as diverging when it is a variable);
//! from an unresolved variable, unify; then `coerce_unsized` in its own snapshot; then by the
//! target (`coerce_raw_ptr`, `coerce_borrowed_pointer`); then by the source (`coerce_from_fn_item`;
//! a fn pointer or closure source unifies here); else unify. Unsizing first was checked against
//! Relax's port of coercion.rs @8d1a764: types/src/typeck.rs `coerce` ("coercion.rs coerce:
//! unsizing is tried first": never, then `coerce_unsized`, then the reference arms, then fn
//! items) and types/src/typeck/coerce_unsized.rs (`coerce_unsized`).
//!
//! Unsizing (`coerce_unsized`): `[T; N]` to `[T]` behind a reference or pointer by unification;
//! to a `dyn` pointee by the solver's `Unsize`; one ADT's two instances (`Box`) by
//! `CoerceUnsized`; each goal asked in a probe, then registered. Said where it differs: two
//! unresolved variables unify instead of registering a `Coerce` predicate. A closure that
//! captures nothing coerces to a fn pointer (`ClosureFnPointer`).

use crate::columns::AdjustKind;
use crate::error::Stop;
use crate::ids::{ExprId, FnPtrHeader, ItemKey, LangItem, Mutbl, Safety};
use crate::machine::Machine;
use crate::ty::{Cause, Goal, Ty, TyList, TyView, TypeCtx};
use crate::world::World;

impl<W: World, C: TypeCtx> Machine<'_, '_, '_, '_, W, C> {
    /// Coerces `a` to `b`, recording the adjustments on `e`. `false`: they do not coerce, and
    /// nothing was bound.
    pub(crate) fn coerce(&mut self, e: Option<ExprId>, a: Ty, b: Ty, two_phase: bool) -> Result<bool, Stop> {
        let a = self.ty.shallow_resolve(a);
        let b = self.ty.shallow_resolve(b);
        if self.is_err(a) || self.is_err(b) {
            self.unify(a, b)?;
            return Ok(true);
        }
        if self.is_never(a) {
            if let Some(e) = e {
                self.adjust.push((e, AdjustKind::NeverToAny, b));
            }
            if self.is_ty_var(b) {
                self.diverging.push(b);
            }
            return Ok(true);
        }
        if self.is_ty_var(a) {
            return self.unify(a, b);
        }
        let cause = e.map_or(Cause::Body, Cause::Expr);
        let got = self.commit_if(|m| m.try_coerce(a, b, two_phase, cause))?;
        if got.is_none() {
            return Ok(false);
        }
        if let Some(e) = e {
            for &(kind, target) in self.adj_scratch.iter() {
                self.adjust.push((e, kind, target));
            }
        }
        self.adj_scratch.clear();
        Ok(true)
    }

    fn unify_some(&mut self, a: Ty, b: Ty) -> Result<Option<()>, Stop> {
        Ok(if self.unify(a, b)? { Some(()) } else { None })
    }

    fn try_coerce(&mut self, a: Ty, b: Ty, two_phase: bool, cause: Cause) -> Result<Option<()>, Stop> {
        self.adj_scratch.clear();
        if self.commit_if(|m| m.coerce_unsized(a, b, cause))?.is_some() {
            return Ok(Some(()));
        }
        self.adj_scratch.clear();
        match self.view(b) {
            TyView::Ptr(mb, _) => return self.coerce_raw_ptr(a, b, mb),
            TyView::Ref(mb, _) => return self.coerce_borrowed(a, b, mb, two_phase),
            _ => {}
        }
        match (self.view(a), self.view(b)) {
            // `coerce_from_fn_item`: the fn's own header (`extern "C" fn` reifies to
            // `extern "C" fn(..)`), then, to an `unsafe` target, `UnsafeFnPointer` after it.
            (TyView::FnDef(k, args), TyView::FnPtr(_, hb)) => {
                let Some(sig) = self.reads.fn_sig(k)? else { return Ok(None) };
                let ha = sig.header;
                let Some((inputs, output)) = self.fn_def_sig(k, args, cause)? else {
                    return Ok(None);
                };
                let mut all = self.list(inputs);
                all.push(output);
                let list = self.mk_list(&all)?;
                let reified = self.mk(TyView::FnPtr(list, ha))?;
                if ha.safety == Safety::Safe && hb.safety == Safety::Unsafe {
                    let unsafe_ = self.mk(TyView::FnPtr(list, FnPtrHeader { safety: Safety::Unsafe, ..ha }))?;
                    if !self.unify(unsafe_, b)? {
                        return Ok(None);
                    }
                    self.adj_scratch.push((AdjustKind::ReifyFnPointer, reified));
                    self.adj_scratch.push((AdjustKind::UnsafeFnPointer, b));
                    return Ok(Some(()));
                }
                if !self.unify(reified, b)? {
                    return Ok(None);
                }
                self.adj_scratch.push((AdjustKind::ReifyFnPointer, b));
                Ok(Some(()))
            }
            // `coerce_closure_to_fn`: the Reference, "Type coercions", "Coercion types": "Non
            // capturing closures to `fn` pointers". The pointer is the closure's signature with
            // the Rust ABI and the target's safety (rustc's `ClosureFnPointer(safety)`).
            (TyView::Closure(c, sig), TyView::FnPtr(_, hb)) => {
                let TyView::FnPtr(l, _) = self.view(sig) else { return Ok(None) };
                if !self.captures_nothing(c)? {
                    return Ok(None);
                }
                let ptr = self.mk(TyView::FnPtr(l, FnPtrHeader { safety: hb.safety, ..FnPtrHeader::RUST }))?;
                if !self.unify(ptr, b)? {
                    return Ok(None);
                }
                self.adj_scratch.push((AdjustKind::ClosureFnPointer, b));
                Ok(Some(()))
            }
            // `coerce_from_safe_fn`: a safe fn pointer to the `unsafe` one of the same ABI.
            (TyView::FnPtr(l, ha), TyView::FnPtr(_, hb)) if ha.safety == Safety::Safe && hb.safety == Safety::Unsafe => {
                let unsafe_ = self.mk(TyView::FnPtr(l, FnPtrHeader { safety: Safety::Unsafe, ..ha }))?;
                if !self.unify(unsafe_, b)? {
                    return Ok(None);
                }
                self.adj_scratch.push((AdjustKind::UnsafeFnPointer, b));
                Ok(Some(()))
            }
            _ => self.unify_some(a, b),
        }
    }

    /// `coerce_unsized` (the Reference, "Coercion types": "TyCtor(`T`) to TyCtor(`U`), where
    /// TyCtor(`T`) is one of `&T`, `&mut T`, `*const T`, `*mut T`, `Box<T>`, and where `U` can be
    /// obtained from `T` by unsized coercion"). A reference or raw pointer: `&[T; N]` to `&[T]`
    /// by unification, and to a `dyn` pointee by the solver's `Pointee: Unsize<dyn ..>` (to an
    /// object, between objects; an auto trait dropped, never added); then deref, borrow, unsize.
    /// Another pointer of one ADT whose target holds a `dyn` or slice argument (`Box<T>` to
    /// `Box<dyn Tr>`): `Source: CoerceUnsized<Target>` (core's and alloc's impls). Each goal is
    /// asked in a probe, then registered; never on a variable source pointee.
    fn coerce_unsized(&mut self, a: Ty, b: Ty, cause: Cause) -> Result<Option<()>, Stop> {
        let (ma, pointee_a, mb, pointee_b, raw) = match (self.view(a), self.view(b)) {
            (TyView::Ref(ma, x), TyView::Ref(mb, y)) => (ma, x, mb, y, false),
            (TyView::Ref(ma, x), TyView::Ptr(mb, y)) | (TyView::Ptr(ma, x), TyView::Ptr(mb, y)) => {
                (ma, x, mb, y, true)
            }
            (TyView::Adt(ka, _), TyView::Adt(kb, lb)) if ka == kb => return self.coerce_unsized_adt(a, b, lb, cause),
            _ => return Ok(None),
        };
        if ma == Mutbl::Not && mb == Mutbl::Mut {
            return Ok(None);
        }
        match (self.view(pointee_a), self.view(pointee_b)) {
            (TyView::Array(elem_a, _), TyView::Slice(elem_b)) => {
                if !self.unify(elem_a, elem_b)? {
                    return Ok(None);
                }
            }
            (TyView::Infer(_), _) | (_, TyView::Infer(_)) => return Ok(None),
            (_, TyView::Dynamic(_)) => {
                // Equal pointees are no unsizing: the reborrow arms take them. An opaque type is
                // never the object type (a caller knows only its bounds: the Reference, "Abstract
                // return types"), and must not be asked: unification with an alias is lazy (the
                // pair is deferred and "succeeds"), which made `&mut impl Tr` to `&mut dyn Tr`
                // a plain reborrow whose deferred `impl Tr == dyn Tr` then failed.
                let opaque_source = matches!(self.view(pointee_a), TyView::Opaque(..));
                if !opaque_source && self.probe(|m| m.unify_some(pointee_a, pointee_b))?.is_some() {
                    return Ok(None);
                }
                // No `Unsize` in the world: the coercion is undecidable, so the finding naming
                // it, and the coercion taken (no second error as a mismatch).
                if let Some(unsize) = self.need_lang(LangItem::Unsize, crate::diag::At::of(cause))? {
                    let args = self.mk_list(&[pointee_a, pointee_b])?;
                    if !self.unsize_goal(unsize, args, cause)? {
                        return Ok(None);
                    }
                }
            }
            _ => return Ok(None),
        }
        let borrowed = if raw { self.mk(TyView::Ptr(mb, pointee_a))? } else { self.mk(TyView::Ref(mb, pointee_a))? };
        self.adj_scratch.push((AdjustKind::Deref { overloaded: None }, pointee_a));
        self.adj_scratch.push((AdjustKind::Borrow { mutbl: mb, two_phase: false, raw }, borrowed));
        self.adj_scratch.push((AdjustKind::Unsize, b));
        Ok(Some(()))
    }

    /// `coerce_unsized` for one ADT's two instances (`Box<T>` to `Box<dyn Tr>`): only when an
    /// argument of the target `lb` is a `dyn` or a slice and the two do not unify as they are;
    /// then `a: CoerceUnsized<b>`, asked, registered, and one `Unsize` adjustment.
    fn coerce_unsized_adt(&mut self, a: Ty, b: Ty, lb: TyList, cause: Cause) -> Result<Option<()>, Stop> {
        let mut unsized_arg = false;
        for t in self.list(lb) {
            unsized_arg |= matches!(self.view(t), TyView::Dynamic(_) | TyView::Slice(_));
        }
        if !unsized_arg || self.probe(|m| m.unify_some(a, b))?.is_some() {
            return Ok(None);
        }
        // As `Unsize` above: no `CoerceUnsized` is the finding naming it, the coercion taken.
        if let Some(coerce) = self.need_lang(LangItem::CoerceUnsized, crate::diag::At::of(cause))? {
            let args = self.mk_list(&[a, b])?;
            if !self.unsize_goal(coerce, args, cause)? {
                return Ok(None);
            }
        }
        self.adj_scratch.push((AdjustKind::Unsize, b));
        Ok(Some(()))
    }

    /// `args[0]: tr<args[1]>` (`Unsize`, `CoerceUnsized`): `false` if it cannot hold (asked in
    /// a probe); else registered with `cause`.
    fn unsize_goal(&mut self, tr: ItemKey, args: TyList, cause: Cause) -> Result<bool, Stop> {
        #[cfg(test)]
        if self.plant == crate::machine::Plant::UnsizeUnasked {
            return Ok(true);
        }
        if !self.ty.may_hold(Goal::Trait { trait_: tr, args }, &mut self.reads)? {
            return Ok(false);
        }
        self.register_trait(tr, args, cause)?;
        Ok(true)
    }

    /// `coerce_borrowed_pointer`: autoderef the source, skipping its first step, until `&mb U`
    /// unifies with the target; then the derefs and a reborrow.
    fn coerce_borrowed(&mut self, a: Ty, b: Ty, mb: Mutbl, two_phase: bool) -> Result<Option<()>, Stop> {
        let TyView::Ref(ma, _) = self.view(a) else {
            return self.unify_some(a, b);
        };
        if ma == Mutbl::Not && mb == Mutbl::Mut {
            return Ok(None);
        }
        let (steps, _limit) = self.autoderef(a, false)?;
        let mut found = None;
        for (i, step) in steps.iter().enumerate().skip(1) {
            let cand = self.mk(TyView::Ref(mb, step.ty))?;
            if self.commit_if(|m| m.unify_some(cand, b))?.is_some() {
                found = Some(i);
                break;
            }
        }
        let Some(i) = found else { return Ok(None) };
        if i == 1 && ma == Mutbl::Not && mb == Mutbl::Not {
            // `&*x` of a `&T` to `&T` is no adjustment at all.
            return Ok(Some(()));
        }
        for s in steps.iter().take(i + 1).skip(1) {
            let kind = AdjustKind::Deref { overloaded: if s.overloaded { Some(Mutbl::Not) } else { None } };
            self.adj_scratch.push((kind, s.ty));
        }
        let two_phase = two_phase && mb == Mutbl::Mut;
        self.adj_scratch.push((AdjustKind::Borrow { mutbl: mb, two_phase, raw: false }, b));
        Ok(Some(()))
    }

    /// `coerce_raw_ptr`: `&T` or `*mut T` to `*const T` / `*mut T`.
    fn coerce_raw_ptr(&mut self, a: Ty, b: Ty, mb: Mutbl) -> Result<Option<()>, Stop> {
        let (is_ref, ma, pointee) = match self.view(a) {
            TyView::Ref(m, t) => (true, m, t),
            TyView::Ptr(m, t) => (false, m, t),
            _ => return self.unify_some(a, b),
        };
        if ma == Mutbl::Not && mb == Mutbl::Mut {
            return Ok(None);
        }
        let a_raw = self.mk(TyView::Ptr(mb, pointee))?;
        if !self.unify(a_raw, b)? {
            return Ok(None);
        }
        if is_ref {
            self.adj_scratch.push((AdjustKind::Deref { overloaded: None }, pointee));
            self.adj_scratch.push((AdjustKind::Borrow { mutbl: mb, two_phase: false, raw: true }, b));
        } else if ma != mb {
            self.adj_scratch.push((AdjustKind::MutToConstPointer, b));
        }
        Ok(Some(()))
    }
}
