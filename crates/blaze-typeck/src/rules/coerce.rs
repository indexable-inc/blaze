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
//! to a `dyn` pointee, and between two instances of one struct whose tail unsizes, by the
//! solver's `Unsize`; one ADT's two instances (`Box`, `Pin<Box<_>>`)
//! by `CoerceUnsized`; each goal asked in a probe, then registered. Said where it differs: two
//! unresolved variables unify instead of registering a `Coerce` predicate. A closure that
//! captures nothing coerces to a fn pointer (`ClosureFnPointer`).

use crate::columns::AdjustKind;
use crate::diag::{At, DiagCode, PendingDiag, PendingGoal};
use crate::error::{CheckError, Malformed, Stop};
use crate::ids::{ExprId, FnPtrHeader, ItemKey, LangItem, Mutbl, RecordSchema, Safety};
use crate::machine::Machine;
use crate::ty::{Cause, Goal, Ty, TyList, TyView, TypeCtx};
use crate::world::World;

#[derive(Clone, Copy)]
enum Unsizing {
    Adjust,
    Diagnosed,
    ProjectionMismatch(PendingDiag),
}

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
            if self.is_ty_var(b) && self.active_inferred_err_var() != Some(b) {
                self.diverging.push(b);
            }
            return Ok(true);
        }
        if self.active_inferred_err_var() == Some(b) {
            self.inferred_err_coercions.push((e, a));
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

    /// `Coerce::coerce` past its first cases: the adjustments are left in `adj_scratch` for the
    /// caller to record (on one source here; on every earlier source of a site in
    /// rules/coerce_lub.rs `lub_earlier_to_new`). Run inside `commit_if`.
    pub(crate) fn try_coerce(&mut self, a: Ty, b: Ty, two_phase: bool, cause: Cause) -> Result<Option<()>, Stop> {
        self.adj_scratch.clear();
        let mismatch = match self.commit_if(|m| m.coerce_unsized(a, b, cause))? {
            Some(Unsizing::Adjust | Unsizing::Diagnosed) => return Ok(Some(())),
            Some(Unsizing::ProjectionMismatch(diag)) => diag,
            None => return self.coerce_without_unsizing(a, b, two_phase, cause),
        };
        // An ordinary deref coercion can reach the target even when unsizing the
        // original pointee fails one of its associated-type equalities.
        if self.commit_if(|m| m.coerce_without_unsizing(a, b, two_phase, cause))?.is_none() {
            self.errors += 1;
            self.diags.push(mismatch);
        }
        Ok(Some(()))
    }

    fn coerce_without_unsizing(&mut self, a: Ty, b: Ty, two_phase: bool, cause: Cause) -> Result<Option<()>, Stop> {
        self.adj_scratch.clear();
        match self.view(b) {
            TyView::Ptr(mb, _) => return self.coerce_raw_ptr(a, b, mb),
            TyView::Ref(mb, _) => return self.coerce_borrowed(a, b, mb, two_phase),
            TyView::Union(lb) => return self.coerce_to_union(a, b, lb),
            _ => {}
        }
        match (self.view(a), self.view(b)) {
            // `coerce_from_fn_item`: the fn's own header (`extern "C" fn` reifies to
            // `extern "C" fn(..)`), then, to an `unsafe` target, `UnsafeFnPointer` after it.
            (TyView::FnDef(k, args), TyView::FnPtr(_, hb)) => {
                // The one rule for reifying a fn item (rules/reify.rs): an intrinsic is refused
                // by name there, and the coercion taken, so no second error follows as a
                // mismatch (as a missing lang item in `coerce_unsized`).
                let (reified, list, ha) = match self.reify_fn_item(a, k, args, cause)? {
                    crate::rules::reify::Reify::Ptr { ty, list, header, gated } => {
                        // A safe `#[target_feature]` fn to a SAFE pointer outside its features
                        // (rules/target_feature.rs): Rust's refusal by name, and the coercion
                        // taken, so no second error follows as a mismatch.
                        if gated && hb.safety == Safety::Safe {
                            self.diag(
                                crate::diag::DiagCode::TargetFeatureToSafeFnPointer,
                                crate::diag::At::of(cause),
                                crate::diag::About::types(b, a),
                            );
                            return Ok(Some(()));
                        }
                        (ty, list, header)
                    }
                    crate::rules::reify::Reify::NoSig => return Ok(None),
                    crate::rules::reify::Reify::Refused => return Ok(Some(())),
                };
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
            // Width-subtyping projection of structural records: `{ a, b, .. } -> { a }`.
            (TyView::Record(src_schema, src_tys), TyView::Record(dst_schema, dst_tys))
                if src_schema != dst_schema && self.ty.list_len(src_tys) > self.ty.list_len(dst_tys) =>
            {
                self.coerce_record_project(b, src_schema, src_tys, dst_schema, dst_tys)
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
    /// asked in a probe, then registered; never on a variable source pointee (one to a `dyn`
    /// target is first resolved by selecting the pending obligations: a projection's variable).
    fn coerce_unsized(&mut self, a: Ty, b: Ty, cause: Cause) -> Result<Option<Unsizing>, Stop> {
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
        let unsizing = match (self.view(pointee_a), self.view(pointee_b)) {
            (TyView::Array(elem_a, _), TyView::Slice(elem_b)) => {
                if !self.unify(elem_a, elem_b)? {
                    return Ok(None);
                }
                Some(Unsizing::Adjust)
            }
            // A type that names an associated type (a field `digest: T::Digest` read through
            // `&self.digest`, a call's `-> &T::Digest`) is lowered as a variable bound by its
            // `Projection` obligation (`Machine::lower`), and is the alias (`<T as
            // Digestible>::Digest`, rigid in a generic body) only once that obligation is
            // selected. Selected here, as autoderef does at each step: else the variable
            // refused the unsizing, the reborrow arm then resolved it to the alias, and `&Alias`
            // to `&dyn Debug` was E0308. The Reference, "Unsized Coercions": "`T` to `dyn U`,
            // when `T` implements `U + Sized`": for a projection that is the associated type's
            // declared bounds (`type Digest: Debug;`, its implied `Sized`) or a where clause in
            // scope, which the solver's `Unsize` asks of the alias.
            (TyView::Infer(_), TyView::Dynamic(_)) if self.is_ty_var(pointee_a) => {
                let resolved = self.try_structurally_resolve(pointee_a)?;
                if self.is_ty_var(resolved) {
                    return Ok(None);
                }
                self.unsize_to_dyn(resolved, pointee_b, cause)?
            }
            (TyView::Infer(_), _) | (_, TyView::Infer(_)) => return Ok(None),
            (_, TyView::Dynamic(_)) => self.unsize_to_dyn(pointee_a, pointee_b, cause)?,
            // A struct whose tail field unsizes (the Reference, "Type coercions", "Unsized
            // Coercions": "`Foo<.., T, ..>` to `Foo<.., U, ..>`, when `Foo` is a struct, `T`
            // implements `Unsize<U>`, the last field of `Foo` has a type involving `T` ..";
            // core's `array::IntoIter::unsize_mut`, `&mut PolymorphicIter<[MaybeUninit<T>; N]>`
            // to `&mut PolymorphicIter<[MaybeUninit<T>]>`): the solver's structural
            // `Unsize` (blaze-solve `step_struct_unsize`) decides which parameters may change.
            // Asked only when the target's arguments reach a slice or `dyn`
            // (rules/coerce_nested.rs): `&S<?0>` to `&S<u8>` would register an ambiguous goal.
            (TyView::Adt(ka, _), TyView::Adt(kb, lb)) if ka == kb && self.target_reaches_unsized(lb) => {
                self.unsize_pointee(pointee_a, pointee_b, true, cause)?
            }
            _ => return Ok(None),
        };
        if !matches!(unsizing, Some(Unsizing::Adjust)) {
            return Ok(unsizing);
        }
        let borrowed = if raw { self.mk(TyView::Ptr(mb, pointee_a))? } else { self.mk(TyView::Ref(mb, pointee_a))? };
        self.adj_scratch.push((AdjustKind::Deref { overloaded: None }, pointee_a));
        self.adj_scratch.push((AdjustKind::Borrow { mutbl: mb, two_phase: false, raw }, borrowed));
        self.adj_scratch.push((AdjustKind::Unsize, b));
        Ok(unsizing)
    }

    /// The pointee `a` unsizes to the object `b` (`a: Unsize<b>`), as [`Self::unsize_pointee`].
    fn unsize_to_dyn(&mut self, a: Ty, b: Ty, cause: Cause) -> Result<Option<Unsizing>, Stop> {
        // An opaque type is never the object type (a caller knows only its bounds: the
        // Reference, "Abstract return types"), and must not be asked: unification with an alias
        // is lazy (the pair is deferred and "succeeds"), which made `&mut impl Tr` to `&mut dyn
        // Tr` a plain reborrow whose deferred `impl Tr == dyn Tr` then failed.
        let opaque_source = matches!(self.view(a), TyView::Opaque(..));
        self.unsize_pointee(a, b, !opaque_source, cause)
    }

    /// Equal pointees take the reborrow arms. Otherwise the solver decides the unsizing.
    fn unsize_pointee(&mut self, a: Ty, b: Ty, equal_is_no_unsize: bool, cause: Cause) -> Result<Option<Unsizing>, Stop> {
        if equal_is_no_unsize && self.probe(|m| m.unify_some(a, b))?.is_some() {
            return Ok(None);
        }
        self.unsize_goal(LangItem::Unsize, a, b, cause)
    }

    /// `coerce_unsized` for one ADT's two instances (`Box<T>` to `Box<dyn Tr>`, and nested:
    /// `Pin<Box<T>>` to `Pin<Box<dyn Tr>>`, a user `Wrap<Rc<T>>`): only when the target's
    /// arguments `lb` reach a `dyn` or a slice through ADT arguments and pointers
    /// (rules/coerce_nested.rs) and the two do not unify as they are; then the one general
    /// question `a: CoerceUnsized<b>`, asked, registered, and one `Unsize` adjustment. The
    /// nested pointer's own `CoerceUnsized` is the impl's where-clause, the solver's goal.
    fn coerce_unsized_adt(&mut self, a: Ty, b: Ty, lb: TyList, cause: Cause) -> Result<Option<Unsizing>, Stop> {
        if !self.target_reaches_unsized(lb) || self.probe(|m| m.unify_some(a, b))?.is_some() {
            return Ok(None);
        }
        let unsizing = self.unsize_goal(LangItem::CoerceUnsized, a, b, cause)?;
        if matches!(unsizing, Some(Unsizing::Adjust)) {
            self.adj_scratch.push((AdjustKind::Unsize, b));
        }
        Ok(unsizing)
    }

    /// A failed object equality diagnoses the coercion at its source without installing an
    /// adjustment. Other inapplicable unsizings leave the site free to take a different coercion.
    fn unsize_goal(&mut self, item: LangItem, a: Ty, b: Ty, cause: Cause) -> Result<Option<Unsizing>, Stop> {
        let Some(tr) = self.need_lang(item, At::of(cause))? else { return Ok(Some(Unsizing::Diagnosed)) };
        let args = self.mk_list(&[a, b])?;
        #[cfg(test)]
        if self.plant == crate::machine::Plant::UnsizeUnasked {
            return Ok(Some(Unsizing::Adjust));
        }
        // A `No` that may rest only on a future's superset interior is not the answer: the
        // coercion is taken and the goal registered; its failure is the interior gap
        // (rules/coerce_interior.rs).
        if !self.ty.may_hold(Goal::Trait { trait_: tr, args }, &mut self.reads)?
            && !self.unsize_no_may_rest_on_interior(tr, args)?
        {
            return self.diagnose_unsize_projection(tr, args, cause);
        }
        self.register_trait(tr, args, cause)?;
        Ok(Some(Unsizing::Adjust))
    }

    fn diagnose_unsize_projection(&mut self, tr: ItemKey, args: TyList, cause: Cause) -> Result<Option<Unsizing>, Stop> {
        let Some(unsize) = self.lang(LangItem::Unsize)? else { return Ok(None) };
        if self.ty.list_len(args) != 2 {
            return Ok(None);
        }
        let (a, b) = (self.ty.list_get(args, 0), self.ty.list_get(args, 1));
        // The pointee pair under the failed `CoerceUnsized` goal (`Pin<Box<A>>` to `Pin<Box<dyn
        // F>>` reaches `A: Unsize<dyn F>` through two impls' where-clauses), read off the two
        // types (`unsize_pair`). Not off the solver's why-chain: its steps keep no goal naming a
        // variable (infer/mod.rs `why_rows_into`), and an async block's interior is one until
        // the body's check ends (rules/coroutine.rs `open_future`), so the `Unsize` step of
        // `Pin<Box<{async}>>` was never found and E0271 became E0308 (suite/pin_unsize.rs (c)).
        let pair = if tr == unsize { Some((a, b)) } else { self.unsize_pair(a, b)? };
        let Some((source, target)) = pair else { return Ok(None) };
        let TyView::Dynamic(rows) = self.view(target) else { return Ok(None) };
        // Object upcasting compares the two objects' existing bounds. Only concrete-to-object
        // unsizing introduces a projection obligation on the source (blaze-solve step_unsize_to_dyn).
        if matches!(self.view(source), TyView::Dynamic(_)) {
            return Ok(None);
        }
        for i in 0..self.ty.list_len(rows) {
            let row = self.ty.list_get(rows, i);
            let TyView::ExProj { trait_, name, args_term } = self.view(row) else { continue };
            // The object's existential equality becomes a projection obligation with the
            // concrete pointee as Self, just as in the solver's step_unsize_to_dyn.
            let mut args = self.list(args_term);
            let Some(term) = args.pop() else {
                return Err(CheckError::Malformed(Malformed::ObjectPreds { node: row.0 }).into());
            };
            args.insert(0, source);
            let args = self.mk_list(&args)?;
            let goal = Goal::Projection { trait_, args, name, term };
            if self.ty.may_hold(goal, &mut self.reads)? {
                continue;
            }
            // An absent implementation can also fail a projection. E0271 requires a
            // known associated type which disagrees with the requested term.
            if let Some(found) = self.projection_found(trait_, args, name)? {
                let at = match self.view(source) {
                    TyView::Future(block, ..) => At::Expr(block),
                    _ => At::of(cause),
                };
                let goal = PendingGoal::Projection { trait_, args, name, term };
                let mut diag = PendingDiag::unmet(DiagCode::ProjectionMismatch, at, Some(found), goal, None);
                diag.expected = Some(term);
                diag.name = Some(name);
                return Ok(Some(Unsizing::ProjectionMismatch(diag)));
            }
        }
        Ok(None)
    }

    /// The pair `(source, dyn ..)` a failed `a: CoerceUnsized<b>` unsizes at, for its diagnosis
    /// only (the solver has answered No): the first position, through one ADT's arguments
    /// (the Reference, "Unsized Coercions": "`Foo<T>` can implement `CoerceUnsized<Foo<U>>`
    /// when `T` implements `Unsize<U>` or `CoerceUnsized<Foo<U>>`") and `&`/`&mut`/raw
    /// pointers, where the target is an object and the source is a known non-object type
    /// (an object's upcast compares bounds, no projection of the source). A work list bounded
    /// as `target_reaches_unsized` is (rule 18); `None` past it.
    fn unsize_pair(&mut self, a: Ty, b: Ty) -> Result<Option<(Ty, Ty)>, Stop> {
        const MAX_PAIRS: u32 = 256;
        let mut work = blaze_own::Vec::new_in(self.alloc);
        work.push((a, b));
        let mut seen = 0u32;
        while let Some((x, y)) = work.pop() {
            seen += 1;
            if seen > MAX_PAIRS {
                return Ok(None);
            }
            let (x, y) = (self.ty.shallow_resolve(x), self.ty.shallow_resolve(y));
            match (self.view(x), self.view(y)) {
                (TyView::Dynamic(_) | TyView::Infer(_), _) => {}
                (_, TyView::Dynamic(_)) => return Ok(Some((x, y))),
                (TyView::Adt(kx, lx), TyView::Adt(ky, ly)) if kx == ky => {
                    let n = self.ty.list_len(lx).min(self.ty.list_len(ly));
                    // Last pushed is first popped: the first argument is looked at first.
                    for i in (0..n).rev() {
                        let (p, q) = (self.ty.list_get(lx, i), self.ty.list_get(ly, i));
                        work.push((p, q));
                    }
                }
                (TyView::Ref(_, px) | TyView::Ptr(_, px), TyView::Ref(_, py) | TyView::Ptr(_, py)) => work.push((px, py)),
                _ => {}
            }
        }
        Ok(None)
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

    /// Coercion into a structural union `b = Union(lb)`:
    /// 1. If `a` is `Union(la)`: exact unification when arities match; otherwise sub-union
    ///    widening (`A | B -> A | B | C`) when every member of `la` unifies with a unique member
    ///    of `lb` (`AdjustKind::UnionWiden`).
    /// 2. If `a` is not a union: member injection (`A -> A | B`) when `a` unifies with a unique
    ///    member `v` of `lb` (`AdjustKind::UnionInject { variant: v }`).
    fn coerce_to_union(&mut self, a: Ty, b: Ty, lb: TyList) -> Result<Option<()>, Stop> {
        let nb = self.ty.list_len(lb);
        if let TyView::Union(la) = self.view(a) {
            let na = self.ty.list_len(la);
            if na == nb && self.commit_if(|m| m.unify_some(a, b))?.is_some() {
                return Ok(Some(()));
            }
            let widened = self.commit_if(|m| -> Result<Option<()>, Stop> {
                for i in 0..na {
                    let ma = m.ty.list_get(la, i);
                    let mut matched: Option<u32> = None;
                    let mut count = 0u32;
                    for j in 0..nb {
                        let mb = m.ty.list_get(lb, j);
                        if m.probe(|m2| m2.unify(ma, mb))? {
                            matched = Some(j);
                            count += 1;
                            if count > 1 {
                                return Ok(None);
                            }
                        }
                    }
                    let Some(j) = matched else { return Ok(None) };
                    let mb = m.ty.list_get(lb, j);
                    if !m.unify(ma, mb)? {
                        return Ok(None);
                    }
                }
                Ok(Some(()))
            })?;
            if widened.is_some() {
                self.adj_scratch.push((AdjustKind::UnionWiden, b));
                return Ok(Some(()));
            }
            return Ok(None);
        }
        let mut matched: Option<u32> = None;
        let mut count = 0u32;
        for j in 0..nb {
            let mb = self.ty.list_get(lb, j);
            if self.probe(|m| m.unify(a, mb))? {
                matched = Some(j);
                count += 1;
                if count > 1 {
                    return Ok(None);
                }
            }
        }
        if let Some(variant) = matched {
            let mb = self.ty.list_get(lb, variant);
            if self.unify(a, mb)? {
                self.adj_scratch.push((AdjustKind::UnionInject { variant }, b));
                return Ok(Some(()));
            }
        }
        Ok(None)
    }

    /// Structural record width-subtyping projection: `{ a, b, .. } -> { a }` when every field
    /// name of `dst_schema` exists in `src_schema` and its field types unify.
    fn coerce_record_project(
        &mut self,
        b: Ty,
        src_schema: RecordSchema,
        src_tys: TyList,
        dst_schema: RecordSchema,
        dst_tys: TyList,
    ) -> Result<Option<()>, Stop> {
        let src_len = self.ty.list_len(src_tys);
        let dst_len = self.ty.list_len(dst_tys);
        if src_len <= dst_len {
            return Ok(None);
        }
        let mut pairs: blaze_own::Vec<(Ty, Ty), _> =
            blaze_own::Vec::with_capacity_in(usize::try_from(dst_len).unwrap_or(0), self.alloc);
        {
            let Some(src_names) = self.record_names(src_schema) else { return Ok(None) };
            let Some(dst_names) = self.record_names(dst_schema) else { return Ok(None) };
            if src_names.len() != usize::try_from(src_len).unwrap_or(usize::MAX)
                || dst_names.len() != usize::try_from(dst_len).unwrap_or(usize::MAX)
            {
                return Ok(None);
            }
            for (di, &nm) in dst_names.iter().enumerate() {
                let Some(si) = src_names.iter().position(|&sn| sn == nm) else {
                    return Ok(None);
                };
                let Ok(si_u32) = u32::try_from(si) else { return Ok(None) };
                let Ok(di_u32) = u32::try_from(di) else { return Ok(None) };
                pairs.push((self.ty.list_get(src_tys, si_u32), self.ty.list_get(dst_tys, di_u32)));
            }
        }
        let projected = self.commit_if(|m| -> Result<Option<()>, Stop> {
            for &(st, dt) in pairs.iter() {
                if !m.unify(st, dt)? {
                    return Ok(None);
                }
            }
            Ok(Some(()))
        })?;
        if projected.is_some() {
            self.adj_scratch.push((AdjustKind::RecordProject, b));
            return Ok(Some(()));
        }
        Ok(None)
    }
}
