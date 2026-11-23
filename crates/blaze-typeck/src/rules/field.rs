//! Field access, tuple indexing and indexing.
//!
//! Follows rustc_hir_typeck/src/expr.rs (`check_expr_field`, `check_field`, `ban_nonexisting_field`)
//! and place_op.rs (`lookup_indexing`, `try_index_step`, `try_overloaded_place_op`); Relax maps
//! them in typeck.rs (`check_field_access`, `check_index`) and typeck/place.rs.
//!
//! A field: the base is checked with no expectation and resolved structurally; autoderef finds
//! the first step that is a struct or union with that field (a tuple with that index), and the
//! derefs to it are the base's adjustments. A primitive at step 0 is E0610, else E0609.
//!
//! An index: base, then index, each with no expectation. At each autoderef step of the base
//! (then the array's slice), the builtin case first (an array or slice indexed by `usize` or an
//! integer variable, which becomes `usize`), then `Index` through the solver, its `Output` the
//! type. None at all is E0608. In a mutable place, the base's overloaded derefs become
//! `DerefMut` and an overloaded index itself `IndexMut` after the walk (rules/mut_places.rs,
//! rustc's `convert_place_derefs_to_mutable`; `Checked::index_mut`). Omitted: privacy.

use crate::diag::{About, At, DiagCode};
use crate::error::{CheckError, Malformed, ParamSite, Stop};
use crate::ids::{ExprId, ItemKey, LangItem, Name};
use crate::machine::{Expectation, Flow, Frame, Lower, Machine, Then};
use crate::ty::{Cause, Goal, Ty, TyView, TypeCtx, Var, VarKind};
use crate::world::{Adt, AdtKind, World};

impl<'b, 'w, 'c, W: World, C: TypeCtx> Machine<'b, 'w, 'c, '_, W, C> {
    /// The ADT a field access reads. Under the planted bug it bypasses the recorder.
    fn field_adt(&mut self, k: ItemKey) -> Result<Option<Adt<'w>>, Stop> {
        #[cfg(test)]
        if self.plant == crate::machine::Plant::PeekFields {
            return self
                .reads
                .peek_unrecorded_adt(k)
                .map_err(|_| Stop::Need(crate::world::Question::Adt(k)));
        }
        self.reads.adt(k)
    }

    pub(crate) fn expr_field(&mut self, f: Frame, e: ExprId, base: ExprId, name: Name) -> Result<Flow, Stop> {
        self.field_like(f, e, base, Some(name), 0)
    }

    pub(crate) fn expr_tuple_field(&mut self, f: Frame, e: ExprId, base: ExprId, index: u32) -> Result<Flow, Stop> {
        self.field_like(f, e, base, None, index)
    }

    /// A named field when `name` is given, else tuple field `index` (which a tuple struct's
    /// fields also answer, their names being their indices in the lowering).
    fn field_like(&mut self, f: Frame, e: ExprId, base: ExprId, name: Option<Name>, index: u32) -> Result<Flow, Stop> {
        if f.state == 0 {
            self.resume(f, 1)?;
            self.push_expr(base, Expectation::None, Then::Nothing)?;
            return Ok(Flow::Wait);
        }
        let base_ty = self.structurally_resolve(self.ret, At::Expr(base))?;
        if self.is_err(base_ty) {
            return Ok(Flow::Done(self.common.err));
        }
        let (steps, limit) = self.autoderef(base_ty, false)?;
        for (i, step) in steps.iter().enumerate() {
            let found = match self.view(step.ty) {
                TyView::Tuple(l) if name.is_none() => {
                    if index < self.ty.list_len(l) { Some((self.ty.list_get(l, index), 0, index, false)) } else { None }
                }
                TyView::Record(schema, l) if name.is_some() => {
                    if let Some(want) = name
                        && let Some(at) = self.record_field_index(schema, want)
                        && at < self.ty.list_len(l)
                    {
                        Some((self.ty.list_get(l, at), 0, at, false))
                    } else {
                        None
                    }
                }
                TyView::Adt(k, args) => {
                    let Some(adt) = self.field_adt(k)? else { continue };
                    if !matches!(adt.kind, AdtKind::Struct | AdtKind::Union) {
                        None
                    } else {
                        let want = name.unwrap_or(Name::of_index(index));
                        let Some(v) = adt.variants.first() else { continue };
                        let fields = adt.fields.get(v.fields.range()).unwrap_or(&[]);
                        match fields.iter().position(|fl| fl.name == want) {
                            Some(at) => {
                                let field = fields[at];
                                let ty = self.lower(adt.types, field.ty, Lower::Args(args), Cause::Expr(e)).map_err(|err| match err {
                                    Stop::Error(CheckError::Malformed(Malformed::Param { index, args, site: ParamSite::Lower { at: Some(at) } })) => {
                                        let site = ParamSite::Field { owner: self.body.owner, expr: e, adt: k, field: field.name, at };
                                        Stop::Error(CheckError::Malformed(Malformed::Param { index, args, site }))
                                    }
                                    other => other,
                                })?;
                                Some((ty, 0, at as u32, adt.kind == AdtKind::Union))
                            }
                            None => None,
                        }
                    }
                }
                _ => None,
            };
            if let Some((ty, variant, at, union_)) = found {
                self.adjust_autoderef(base, &steps, i);
                self.fields.push((e, variant, at));
                if union_ {
                    // Read by the unsafety pass (rules/unsafety.rs): a union's field read is
                    // `unsafe`, its write is not. Pushed in walk order, sorted there.
                    self.union_fields.push(e);
                }
                return Ok(Flow::Done(ty));
            }
        }
        let primitive = !matches!(self.view(base_ty), TyView::Adt(..) | TyView::Record(..) | TyView::Ref(..) | TyView::Param(_));
        let code = if limit {
            DiagCode::RecursionLimit
        } else if primitive && name.is_some() {
            DiagCode::PrimitiveField
        } else {
            DiagCode::NoField
        };
        self.diag(code, At::Expr(e), About::found_maybe_named(base_ty, name));
        Ok(Flow::Done(self.common.err))
    }

    /// `offset_of!(T, a.b.0)` (the pinned library's `core::mem::offset_of!`: "Expands to the
    /// offset in bytes of a field from the beginning of the given type. The type may be a
    /// `struct`, `enum`, `union`, or tuple. The field may be a nested field (`field1.field2`),
    /// but not an array index ... The offset is returned as a `usize`"; `offset_of_enum`:
    /// "allows `enum` variants to be traversed as if they were fields"): `usize`. Each step is
    /// looked up on the type the step before reached, with no autoderef (a reference's
    /// pointee is not inside it); a step that type lacks is E0609 naming the type and the
    /// step, as is a path that ends on a variant. Omitted: privacy (the world gives no field's
    /// visibility yet). The value is the layout's, not this crate's.
    pub(crate) fn expr_offset_of(&mut self, e: ExprId, ty: crate::PTy, fields: crate::List) -> Result<Flow, Stop> {
        let at = At::Expr(e);
        let usize_ = self.common.usize_;
        let mut cur = self.lower_body_ty(ty, at)?;
        let steps = self.body.segment_list(fields)?;
        // The variant the step before named, when it named one of an enum's.
        let mut variant: Option<usize> = None;
        for s in steps {
            let want = s.name;
            let here = self.structurally_resolve(cur, at)?;
            if self.is_err(here) {
                return Ok(Flow::Done(usize_));
            }
            let next = match self.view(here) {
                TyView::Tuple(l) if variant.is_none() => {
                    let n = self.ty.list_len(l);
                    (0..n).find(|&i| Name::of_index(i) == want).map(|i| self.ty.list_get(l, i))
                }
                TyView::Record(schema, l) if variant.is_none() => {
                    self.record_field_index(schema, want)
                        .filter(|&i| i < self.ty.list_len(l))
                        .map(|i| self.ty.list_get(l, i))
                }
                TyView::Adt(k, args) => match self.field_adt(k)? {
                    Some(adt) if adt.kind == AdtKind::Enum && variant.is_none() => {
                        if let Some(i) = adt.variants.iter().position(|v| v.name == want) {
                            variant = Some(i);
                            continue;
                        }
                        None
                    }
                    Some(adt) => {
                        let fs = match adt.variants.get(variant.unwrap_or(0)) {
                            Some(v) => adt.fields.get(v.fields.range()).unwrap_or(&[]),
                            None => &[],
                        };
                        match fs.iter().position(|fl| fl.name == want) {
                            Some(i) => {
                                variant = None;
                                Some(self.lower(adt.types, fs[i].ty, Lower::Args(args), Cause::Expr(e))?)
                            }
                            None => None,
                        }
                    }
                    None => None,
                },
                _ => None,
            };
            match next {
                Some(t) => cur = t,
                None => {
                    self.diag(DiagCode::NoField, at, About::found_named(here, want));
                    return Ok(Flow::Done(usize_));
                }
            }
        }
        if variant.is_some() {
            // `offset_of!(E, V)`: a variant is no field.
            let last = steps.last().map(|s| s.name);
            self.diag(DiagCode::NoField, at, About::found_maybe_named(cur, last));
        }
        Ok(Flow::Done(usize_))
    }

    pub(crate) fn expr_index(&mut self, mut f: Frame, e: ExprId, base: ExprId, index: ExprId) -> Result<Flow, Stop> {
        match f.state {
            0 => {
                self.resume(f, 1)?;
                self.push_expr(base, Expectation::None, Then::Nothing)?;
                Ok(Flow::Wait)
            }
            1 => {
                f.t = self.ret;
                self.resume(f, 2)?;
                self.push_expr(index, Expectation::None, Then::Nothing)?;
                Ok(Flow::Wait)
            }
            _ => {
                let idx = self.ret;
                let base_ty = self.structurally_resolve(f.t, At::Expr(base))?;
                if self.is_err(base_ty) {
                    return Ok(Flow::Done(self.common.err));
                }
                let (mut steps, _limit) = self.autoderef(base_ty, false)?;
                if let Some(last) = steps.last().copied() {
                    if let TyView::Array(elem, _) = self.view(last.ty) {
                        let slice = self.mk(TyView::Slice(elem))?;
                        steps.push(crate::machine::Step { ty: slice, overloaded: false });
                    }
                }
                let index_trait = self.lang(LangItem::Index)?;
                for i in 0..steps.len() {
                    let step = steps[i];
                    if let Some(elem) = self.builtin_index(step.ty, idx)? {
                        // An array is indexed at its own step, so the slice step is never
                        // reached by the builtin case.
                        self.adjust_autoderef(base, &steps, i);
                        return Ok(Flow::Done(elem));
                    }
                    if let Some(tr) = index_trait {
                        let args = self.mk_list(&[step.ty, idx])?;
                        if self.ty.may_hold(Goal::Trait { trait_: tr, args }, &mut self.reads)? {
                            self.adjust_autoderef(base, &steps, i);
                            self.register_trait(tr, args, Cause::Expr(e))?;
                            let out = self.project(tr, args, Name::of("Output"), Cause::Expr(e))?;
                            return Ok(Flow::Done(out));
                        }
                    }
                }
                if index_trait.is_none() {
                    self.missing_lang(LangItem::Index, At::Expr(e));
                    return Ok(Flow::Done(self.common.err));
                }
                self.diag(DiagCode::CantIndex, At::Expr(e), About::found(base_ty));
                Ok(Flow::Done(self.common.err))
            }
        }
    }

    /// `builtin_index`: an array or slice by `usize` or an integer variable.
    fn builtin_index(&mut self, base: Ty, idx: Ty) -> Result<Option<Ty>, Stop> {
        let elem = match self.view(base) {
            TyView::Array(t, _) | TyView::Slice(t) => t,
            _ => return Ok(None),
        };
        let usize_ = self.common.usize_;
        let ok = match self.view(idx) {
            TyView::Uint(crate::ids::UintTy::Usize) => true,
            TyView::Infer(Var { kind: VarKind::Int, .. }) => self.unify(idx, usize_)?,
            _ => false,
        };
        Ok(if ok { Some(elem) } else { None })
    }
}
