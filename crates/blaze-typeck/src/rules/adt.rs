//! Tuples, arrays, repeat expressions and struct expressions.
//!
//! Follows rustc_hir_typeck/src/expr.rs (`check_expr_tuple`, `check_expr_array`,
//! `check_expr_repeat`, `check_expr_struct`, `check_expr_struct_fields`, `check_struct_path`);
//! Relax maps them in typeck.rs (`check_tuple`, `check_array`, `check_struct_lit`).
//!
//! A tuple element is coerced to the expected tuple's element when the expectation is a tuple
//! of that position, else checked with no expectation. Array elements are one coercion site
//! aimed at the expected element type (of an expected array or slice) or a fresh variable, each
//! checked with that type as a hint; `[]` is an array of a fresh variable. A repeat's element is
//! coerced to an expected element type, else must have a fresh variable's type. A struct
//! expression resolves its path to a variant, takes the expected type as a hint when they unify,
//! coerces each field to its declared type (E0560 for a name the variant lacks), checks the base
//! of `..base` as the struct's type, and without a base reports missing fields (E0063).
//!
//! Omitted for now: the `Copy` requirement of `[x; N]` with `N > 1` (rustc defers it and allows
//! constants), the `Sized` obligation of the struct, union single-field rules, and
//! `#[non_exhaustive]` construction from other crates.

use blaze_own::Alloc;

use crate::diag::{At, DiagCode};
use crate::error::Stop;
use crate::ids::{ExprId, ItemKey, List, PathIdx};
use crate::machine::{Expectation, Flow, Frame, Lower, Machine, Then};
use crate::portable::PTy;
use crate::ty::{Ty, TyList, TyView, TypeCtx};
use crate::world::{AdtKind, Def, World};

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// The expected tuple's elements, if the expectation is a tuple. `f.l` holds them, `f.c`
    /// whether there are any, `f.a` the next element.
    pub(crate) fn expr_tuple(&mut self, mut f: Frame, _e: ExprId, elems: List) -> Result<Flow, Stop> {
        let ids = self.body.expr_list(elems)?;
        if f.state == 0 {
            f.c = 0;
            if let Some(t) = f.expect.only_has_type() {
                let t = self.try_structurally_resolve(t)?;
                if let TyView::Tuple(l) = self.view(t) {
                    f.l = l;
                    f.c = 1;
                }
            }
            f.a = 0;
        }
        let n = if f.c == 1 { self.ty.list_len(f.l) as usize } else { 0 };
        if let Some(&el) = ids.get(f.a as usize) {
            let i = f.a as usize;
            f.a += 1;
            self.resume(f, 1)?;
            if i < n {
                let to = self.ty.list_get(f.l, i as u32);
                self.push_coercible(el, to)?;
            } else {
                self.push_expr(el, Expectation::None, Then::Nothing)?;
            }
            return Ok(Flow::Wait);
        }
        let mut tys: blaze_own::Vec<Ty, Alloc<'a>> = blaze_own::Vec::with_capacity_in(ids.len(), self.alloc);
        for (i, &el) in ids.iter().enumerate() {
            tys.push(if i < n { self.ty.list_get(f.l, i as u32) } else { self.expr_ty[el.ix()] });
        }
        let l = self.mk_list(&tys)?;
        Ok(Flow::Done(self.mk(TyView::Tuple(l))?))
    }

    /// The element type an expectation of an array or slice gives.
    fn expected_elem(&mut self, ex: Expectation) -> Option<Ty> {
        let t = ex.to_option()?;
        match self.view(t) {
            TyView::Array(e, _) | TyView::Slice(e) => Some(e),
            _ => None,
        }
    }

    /// `f.a` is the next element, `f.b` the site, `f.t` the target.
    pub(crate) fn expr_array(&mut self, mut f: Frame, _e: ExprId, elems: List) -> Result<Flow, Stop> {
        let ids = self.body.expr_list(elems)?;
        if f.state == 0 {
            if ids.is_empty() {
                let v = self.fresh()?;
                let n = self.len_value(0)?;
                return Ok(Flow::Done(self.mk(TyView::Array(v, n))?));
            }
            f.t = match self.expected_elem(f.expect) {
                Some(t) => t,
                None => self.fresh()?,
            };
            f.b = self.new_coerce_many(f.t)?;
            f.a = 0;
        }
        if let Some(&el) = ids.get(f.a as usize) {
            f.a += 1;
            self.resume(f, 1)?;
            self.push_expr(el, Expectation::HasType(f.t), Then::CoerceMany(f.b))?;
            return Ok(Flow::Wait);
        }
        let elem = self.coerce_many_complete(f.b);
        let n = self.len_value(ids.len() as u64)?;
        Ok(Flow::Done(self.mk(TyView::Array(elem, n))?))
    }

    /// `[elem; len]` (the Reference, "Array expressions": `len` a constant expression of type
    /// `usize`, here already a const term of the body's types).
    /// `[elem; len]`: state 0 checks `elem`, state 1 the length's own expression `count` as a
    /// `usize` (the Reference, "Array expressions"), state 2 makes `[T; len]`.
    pub(crate) fn expr_repeat(&mut self, mut f: Frame, e: ExprId, elem: ExprId, len: PTy, count: Option<ExprId>) -> Result<Flow, Stop> {
        if f.state == 0 {
            match f.expect.only_has_type().and_then(|_| self.expected_elem(f.expect)) {
                Some(t) => {
                    f.t = t;
                    self.resume(f, 1)?;
                    self.push_coercible(elem, t)?;
                }
                None => {
                    let t = self.fresh()?;
                    f.t = t;
                    self.resume(f, 1)?;
                    self.push_has_type(elem, t)?;
                }
            }
            return Ok(Flow::Wait);
        }
        if f.state == 1
            && let Some(c) = count
        {
            let usize_ = self.common.usize_;
            self.resume(f, 2)?;
            self.push_has_type(c, usize_)?;
            return Ok(Flow::Wait);
        }
        let len = self.lower_len(len, At::Expr(e))?;
        Ok(Flow::Done(self.mk(TyView::Array(f.t, len))?))
    }

    /// `check_struct_path`: the ADT, the variant and its arguments, or `None` after a
    /// diagnostic.
    pub(crate) fn struct_path(&mut self, at: At, path: PathIdx) -> Result<Option<(ItemKey, u32, TyList)>, Stop> {
        let bp = self.body.path(path)?;
        let segs = self.body.segment_list(bp.segments)?;
        let mut names: blaze_own::Vec<crate::ids::Name, Alloc<'a>> = blaze_own::Vec::with_capacity_in(segs.len(), self.alloc);
        for s in segs {
            names.push(s.name);
        }
        let last = segs.last().map(|s| s.name);
        let res = self.path_res(bp, path, &names, at)?;
        let (k, variant, def) = match res {
            Ok(r) if r.consumed as usize == segs.len() => match r.def {
                Def::Adt(k) => (k, None, r.def),
                Def::Ctor { adt, variant, .. } => (adt, Some(variant), r.def),
                _ => {
                    self.diag(DiagCode::NotAStruct, at, None, None, last);
                    return Ok(None);
                }
            },
            Ok(_) => {
                self.diag(DiagCode::NotAStruct, at, None, None, last);
                return Ok(None);
            }
            Err(why) => {
                self.unresolved(why, at, last);
                return Ok(None);
            }
        };
        let Some(adt) = self.reads.adt(k)? else {
            self.diag(DiagCode::NotAStruct, at, None, None, last);
            return Ok(None);
        };
        let variant = match (variant, adt.kind) {
            (Some(v), _) => v,
            (None, AdtKind::Struct | AdtKind::Union) => 0,
            (None, AdtKind::Enum) => {
                self.diag(DiagCode::NotAStruct, at, None, None, last);
                return Ok(None);
            }
        };
        let written = segs.iter().rev().find(|s| s.has_args).copied();
        let args = self.explicit_args(at, written, k, 0, adt.generics)?;
        self.defs.push((at, def));
        Ok(Some((k, variant, args)))
    }

    /// `check_expr_struct`. `f.t` is the struct's type, `f.l` its arguments, `f.c` the variant,
    /// `f.a` the next field, `f.b` 1 once the path failed.
    pub(crate) fn expr_struct(&mut self, mut f: Frame, e: ExprId, path: PathIdx, fields: List, base: Option<ExprId>) -> Result<Flow, Stop> {
        let inits = self.body.field_init_list(fields)?;
        if f.state == 0 {
            f.a = 0;
            match self.struct_path(At::Expr(e), path)? {
                Some((k, variant, args)) => {
                    let adt_ty = self.mk(TyView::Adt(k, args))?;
                    if let Some(hint) = f.expect.only_has_type() {
                        self.commit_if(|m| -> Result<Option<()>, Stop> {
                            Ok(if m.unify(adt_ty, hint)? { Some(()) } else { None })
                        })?;
                    }
                    f.t = adt_ty;
                    f.l = args;
                    f.c = variant;
                    f.b = 0;
                }
                None => {
                    f.t = self.common.err;
                    f.b = 1;
                }
            }
            f.state = 1;
        }
        if f.state == 1 {
            if let Some(init) = inits.get(f.a as usize).copied() {
                f.a += 1;
                let to = if f.b == 1 { None } else { self.struct_field_ty(f.t, f.c, f.l, init.name, crate::ty::Cause::Expr(init.expr))? };
                self.resume(f, 1)?;
                match to {
                    Some(t) => self.push_coercible(init.expr, t)?,
                    None => {
                        if f.b == 0 {
                            self.diag(DiagCode::NoSuchFieldInit, At::Expr(init.expr), None, Some(f.t), Some(init.name));
                        }
                        self.push_expr(init.expr, Expectation::None, Then::Nothing)?;
                    }
                }
                return Ok(Flow::Wait);
            }
            match base {
                Some(b) => {
                    self.resume(f, 2)?;
                    self.push_has_type(b, f.t)?;
                    return Ok(Flow::Wait);
                }
                None if f.b == 0 => self.check_missing_fields(e, f.t, f.c, inits)?,
                None => {}
            }
        }
        Ok(Flow::Done(f.t))
    }

    /// A field's declared type in variant `variant` of `adt_ty`, instantiated with `args`.
    fn struct_field_ty(
        &mut self,
        adt_ty: Ty,
        variant: u32,
        args: TyList,
        name: crate::ids::Name,
        cause: crate::ty::Cause,
    ) -> Result<Option<Ty>, Stop> {
        let TyView::Adt(k, _) = self.view(adt_ty) else { return Ok(None) };
        let Some(adt) = self.reads.adt(k)? else { return Ok(None) };
        let Some(v) = adt.variants.get(variant as usize) else { return Ok(None) };
        let declared = adt.fields.get(v.fields.range()).unwrap_or(&[]);
        match declared.iter().find(|d| d.name == name) {
            Some(d) => Ok(Some(self.lower(adt.types, d.ty, Lower::Args(args), cause)?)),
            None => Ok(None),
        }
    }

    fn check_missing_fields(&mut self, e: ExprId, adt_ty: Ty, variant: u32, inits: &[crate::body::FieldInit]) -> Result<(), Stop> {
        let TyView::Adt(k, _) = self.view(adt_ty) else { return Ok(()) };
        let Some(adt) = self.reads.adt(k)? else { return Ok(()) };
        let Some(v) = adt.variants.get(variant as usize) else { return Ok(()) };
        let declared = adt.fields.get(v.fields.range()).unwrap_or(&[]);
        let missing = declared.iter().find(|d| !inits.iter().any(|i| i.name == d.name));
        if adt.kind != AdtKind::Union {
            if let Some(d) = missing {
                self.diag(DiagCode::MissingFields, At::Expr(e), None, Some(adt_ty), Some(d.name));
            }
        }
        Ok(())
    }
}
