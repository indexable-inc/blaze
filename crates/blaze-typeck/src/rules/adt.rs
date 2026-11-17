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

use crate::body::Segment;
use crate::diag::{About, At, DiagCode};
use crate::error::{CheckError, Stop};
use crate::ids::{ExprId, ItemKey, List, Name, PathIdx};
use crate::machine::{Expectation, Flow, Frame, Lower, Machine, Then};
use crate::portable::PTy;
use crate::ty::{Ty, TyList, TyView, TypeCtx};
use crate::world::{AdtKind, Def, PathRes, World};

/// The segment `Self` (the Reference, "Paths", "`Self`").
const SELF_NAME: Name = Name::of("Self");

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
    ///
    /// The path names a struct, a union or an enum variant (the Reference, "Struct
    /// expressions": "A struct expression creates a struct, enum, or union value. It consists of
    /// a path to a struct, enum variant, or union item followed by the values for the fields of
    /// the item"; "Struct patterns": "Struct patterns match struct, enum, and union values"). A
    /// variant may be named through a type: `Self::A { .. }` or `Alias::A { .. }` (the
    /// Reference's own example there: `<Enum as Tr>::T::Tuple(); // OK`). The
    /// world answers such a prefix as `Def::Adt` with one segment left; that segment is a
    /// variant of the enum ([`Self::struct_variant`]). `Self`'s arguments are the impl's own
    /// self type's ([`Self::self_struct_args`]), never fresh: `Self` is that type (the
    /// Reference, "Paths", "`Self`": "In an implementation, it refers to the type being
    /// implemented"). Still E0574: a struct or union prefix with a trailing segment, a name
    /// that is no variant, a prefix that is no ADT, more than one trailing segment.
    pub(crate) fn struct_path(&mut self, at: At, path: PathIdx) -> Result<Option<(ItemKey, u32, TyList)>, Stop> {
        let bp = self.body.path(path)?;
        let segs = self.body.segment_list(bp.segments)?;
        let mut names: blaze_own::Vec<crate::ids::Name, Alloc<'a>> = blaze_own::Vec::with_capacity_in(segs.len(), self.alloc);
        for s in segs {
            names.push(s.name);
        }
        let last = segs.last().map(|s| s.name);
        let res = self.path_res(bp, path, &names, at)?;
        // The segments the world consumed: the last of them names the type.
        let consumed = match &res {
            Ok(r) => r.consumed as usize,
            Err(_) => 0,
        };
        let (k, variant, def) = match res {
            Ok(r) if r.consumed as usize == segs.len() => match r.def {
                Def::Adt(k) => (k, None, r.def),
                Def::Ctor { adt, variant, .. } => (adt, Some(variant), r.def),
                _ => {
                    self.diag(DiagCode::NotAStruct, at, About::path(last));
                    return Ok(None);
                }
            },
            // Type-relative: `Self::A`, `Alias::A`, the prefix an enum and `A` its variant.
            Ok(PathRes { def: Def::Adt(k), consumed: c }) if c != 0 && c as usize + 1 == segs.len() => {
                match self.struct_variant(at, k, segs[c as usize])? {
                    Some((variant, def)) => (k, Some(variant), def),
                    None => return Ok(None),
                }
            }
            Ok(_) => {
                self.diag(DiagCode::NotAStruct, at, About::path(last));
                return Ok(None);
            }
            Err(why) => {
                self.unresolved(why, at, &names);
                return Ok(None);
            }
        };
        let Some(adt) = self.reads.adt(k)? else {
            self.diag(DiagCode::NotAStruct, at, About::path(last));
            return Ok(None);
        };
        let variant = match (variant, adt.kind) {
            (Some(v), _) => v,
            (None, AdtKind::Struct | AdtKind::Union) => 0,
            (None, AdtKind::Enum) => {
                self.diag(DiagCode::NotAStruct, at, About::path(last));
                return Ok(None);
            }
        };
        // `Self { .. }` and `Self::A { .. }`: the world consumed exactly the segment `Self`.
        let by_self = consumed == 1 && segs.first().is_some_and(|s| s.name == SELF_NAME);
        let args = if by_self {
            if segs.first().is_some_and(|s| s.has_args) {
                // The Reference, "Paths", "`Self`": "The Self path cannot include generic
                // arguments (as in Self::<i32>)." Refused by name: rustc's E0109, for which the
                // checker has no code.
                return Err(CheckError::Unported("generic arguments on `Self` in a struct expression or pattern (E0109)").into());
            }
            match self.self_struct_args(at, k)? {
                Some(args) => args,
                None => return Ok(None),
            }
        } else {
            let written = segs.iter().rev().find(|s| s.has_args).copied();
            self.explicit_args(at, written, k, 0, adt.generics)?
        };
        self.defs.push((at, def));
        Ok(Some((k, variant, args)))
    }

    /// The variant `seg` of the enum `k` that a type-relative struct path names (`Self::A`,
    /// `Alias::A`): its index and its definition, or `None` after E0574 naming `seg` (`k` no
    /// enum, or no variant of that name). Generic arguments on the variant's own segment of a
    /// type-relative path are refused by name: rustc prohibits them there (E0109, "type
    /// arguments are not allowed on this type"), and the checker has no code for it.
    fn struct_variant(&mut self, at: At, k: ItemKey, seg: Segment) -> Result<Option<(u32, Def)>, Stop> {
        let Some(adt) = self.reads.adt(k)? else {
            self.diag(DiagCode::NotAStruct, at, About::name(seg.name));
            return Ok(None);
        };
        let found = match adt.kind {
            AdtKind::Enum => adt.variants.iter().enumerate().find(|(_, v)| v.name == seg.name).map(|(i, v)| (i as u32, v.ctor)),
            AdtKind::Struct | AdtKind::Union => None,
        };
        let Some((i, ctor)) = found else {
            self.diag(DiagCode::NotAStruct, at, About::name(seg.name));
            return Ok(None);
        };
        if seg.has_args {
            return Err(CheckError::Unported("generic arguments on a type-relative enum variant in a struct expression or pattern (E0109)").into());
        }
        Ok(Some((i, Def::Ctor { adt: k, variant: i, kind: ctor })))
    }

    /// `Self`'s generic arguments in a struct path: those of the impl's own self type as the
    /// body sees them (`Self::A { .. }` in `impl<T> E<T>` builds `E<T>`, not `E` at fresh
    /// arguments), as [`Self::self_ctor_value`] takes them for `Self` as a value. `None` after a
    /// diagnostic: E0574 naming `Self` where the body is in no impl, or where the impl's self
    /// type is not the ADT `adt` the world answered `Self` with.
    fn self_struct_args(&mut self, at: At, adt: ItemKey) -> Result<Option<TyList>, Stop> {
        let header = match self.reads.assoc_parent(self.body.owner)? {
            Some(parent) => self.reads.impl_header(parent.container)?,
            None => None,
        };
        let Some(h) = header else {
            self.diag(DiagCode::NotAStruct, at, About::name(SELF_NAME));
            return Ok(None);
        };
        let self_ty = self.lower(h.types, h.self_ty, Lower::Body, at.cause())?;
        let self_ty = self.try_structurally_resolve(self_ty)?;
        match self.view(self_ty) {
            TyView::Adt(k, args) if k == adt => Ok(Some(args)),
            _ => {
                self.diag(DiagCode::NotAStruct, at, About::found_named(self_ty, SELF_NAME));
                Ok(None)
            }
        }
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
                            self.diag(DiagCode::NoSuchFieldInit, At::Expr(init.expr), About::found_named(f.t, init.name));
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
                self.diag(DiagCode::MissingFields, At::Expr(e), About::found_named(adt_ty, d.name));
            }
        }
        Ok(())
    }
}
