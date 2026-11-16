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
//! First cut, said where it differs: unsizing is the builtin `[T; N]` to `[T]` behind a
//! reference or pointer (the general form is `CoerceUnsized` obligations through the solver,
//! which blaze-ty's adapter will give); two unresolved variables unify instead of registering a
//! `Coerce` predicate; a closure does not coerce to a fn pointer yet.

use core::alloc::Allocator;

use crate::columns::AdjustKind;
use crate::error::Stop;
use crate::ids::{ExprId, Mutbl};
use crate::machine::Machine;
use crate::ty::{Ty, TyView, TypeCtx};
use crate::world::World;

impl<W: World, C: TypeCtx, A: Allocator + Clone> Machine<'_, '_, '_, W, C, A> {
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
        let got = self.commit_if(|m| m.try_coerce(a, b, two_phase))?;
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

    fn try_coerce(&mut self, a: Ty, b: Ty, two_phase: bool) -> Result<Option<()>, Stop> {
        self.adj_scratch.clear();
        if self.commit_if(|m| m.coerce_unsized(a, b))?.is_some() {
            return Ok(Some(()));
        }
        self.adj_scratch.clear();
        match self.view(b) {
            TyView::Ptr(mb, _) => return self.coerce_raw_ptr(a, b, mb),
            TyView::Ref(mb, _) => return self.coerce_borrowed(a, b, mb, two_phase),
            _ => {}
        }
        match (self.view(a), self.view(b)) {
            (TyView::FnDef(k, args), TyView::FnPtr(_)) => {
                let Some((inputs, output)) = self.fn_def_sig(k, args)? else {
                    return Ok(None);
                };
                let mut all = self.list(inputs);
                all.push(output);
                let list = self.mk_list(&all)?;
                let ptr = self.mk(TyView::FnPtr(list))?;
                if !self.unify(ptr, b)? {
                    return Ok(None);
                }
                self.adj_scratch.push((AdjustKind::ReifyFnPointer, b));
                Ok(Some(()))
            }
            _ => self.unify_some(a, b),
        }
    }

    /// `&[T; N]` (or `&mut`, or a raw pointer) to `&[T]`: deref, borrow, unsize.
    fn coerce_unsized(&mut self, a: Ty, b: Ty) -> Result<Option<()>, Stop> {
        let (ma, pointee_a, mb, pointee_b, raw) = match (self.view(a), self.view(b)) {
            (TyView::Ref(ma, x), TyView::Ref(mb, y)) => (ma, x, mb, y, false),
            (TyView::Ref(ma, x), TyView::Ptr(mb, y)) | (TyView::Ptr(ma, x), TyView::Ptr(mb, y)) => {
                (ma, x, mb, y, true)
            }
            _ => return Ok(None),
        };
        if ma == Mutbl::Not && mb == Mutbl::Mut {
            return Ok(None);
        }
        let (TyView::Array(elem_a, _), TyView::Slice(elem_b)) = (self.view(pointee_a), self.view(pointee_b)) else {
            return Ok(None);
        };
        if !self.unify(elem_a, elem_b)? {
            return Ok(None);
        }
        let borrowed = if raw { self.mk(TyView::Ptr(mb, pointee_a))? } else { self.mk(TyView::Ref(mb, pointee_a))? };
        self.adj_scratch.push((AdjustKind::Deref { overloaded: None }, pointee_a));
        self.adj_scratch.push((AdjustKind::Borrow { mutbl: mb, two_phase: false, raw }, borrowed));
        self.adj_scratch.push((AdjustKind::Unsize, b));
        Ok(Some(()))
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
