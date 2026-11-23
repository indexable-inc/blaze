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
use crate::ids::{ExprId, ItemKey, List, Name, PathIdx, RecordSchema};
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
        if let Ok(PathRes { def: Def::Alias(alias), consumed: c }) = res {
            return self.alias_struct_path(at, alias, segs, c as usize, last);
        }
        // `Self::Output { .. }`, `T::Out { .. }`: one segment after `Self` or a parameter that
        // is no variant names an associated type ([`Self::assoc_struct_path`]).
        if let Ok(PathRes { def, consumed: c }) = res {
            let c = c as usize;
            if c != 0 && c + 1 == segs.len() {
                use crate::rules::self_path::AssocPrefix;
                let by_self = c == 1 && segs[0].name == SELF_NAME;
                let prefix = match def {
                    Def::TyParam(i) => Some(AssocPrefix::Param(i)),
                    Def::SelfTy => Some(AssocPrefix::ImplSelf),
                    Def::Adt(k) if by_self && self.assoc_route_for_adt(k, segs[c].name)? => Some(AssocPrefix::ImplSelf),
                    _ => None,
                };
                if let Some(prefix) = prefix {
                    return self.assoc_struct_path(at, prefix, segs[c], last);
                }
            }
        }
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

    /// A struct path through a type alias the world answered as [`Def::Alias`] (rules/
    /// alias_path.rs): `A { .. }` is the struct or union the aliased type is, at the arguments
    /// the alias gives it; `A::V { .. }` a variant of the enum it is ([`Self::struct_variant`]).
    /// E0574 for an aliased type that is no ADT, an enum named whole, a struct or union with a
    /// trailing segment, or more than one trailing segment.
    fn alias_struct_path(&mut self, at: At, alias: ItemKey, segs: &[Segment], consumed: usize, last: Option<crate::ids::Name>) -> Result<Option<(ItemKey, u32, TyList)>, Stop> {
        let (Some(&seg), true) = (consumed.checked_sub(1).and_then(|i| segs.get(i)), consumed + 1 >= segs.len()) else {
            self.diag(DiagCode::NotAStruct, at, About::path(last));
            return Ok(None);
        };
        let Some(t) = self.alias_path_ty(at, at.cause(), alias, seg)? else { return Ok(None) };
        let TyView::Adt(k, args) = self.view(t) else {
            self.diag(DiagCode::NotAStruct, at, About::path(last));
            return Ok(None);
        };
        let Some(adt) = self.reads.adt(k)? else {
            self.diag(DiagCode::NotAStruct, at, About::path(last));
            return Ok(None);
        };
        let (variant, def) = match (segs.get(consumed), adt.kind) {
            (None, AdtKind::Struct | AdtKind::Union) => (0, Def::Adt(k)),
            (Some(&v), AdtKind::Enum) => match self.struct_variant(at, k, v)? {
                Some(found) => found,
                None => return Ok(None),
            },
            _ => {
                self.diag(DiagCode::NotAStruct, at, About::path(last));
                return Ok(None);
            }
        };
        self.defs.push((at, def));
        Ok(Some((k, variant, args)))
    }

    /// Whether `k` is an enum with a variant named `name` (what a type-relative struct path's
    /// last segment is looked up as first: rustc `associated_path_to_ty` tries the variants of
    /// an enum before its associated types).
    fn names_variant(&mut self, k: ItemKey, name: Name) -> Result<bool, Stop> {
        Ok(match self.reads.adt(k)? {
            Some(adt) => adt.kind == AdtKind::Enum && adt.variants.iter().any(|v| v.name == name),
            None => false,
        })
    }

    /// Whether `Self::name { .. }`, `Self` the ADT `k`, takes the associated-type route. rustc
    /// `lower_assoc_path` looks a name up as a variant of an enum first; failing that, as an
    /// associated item, and when that fails too an enum reports "no variant named `name`"
    /// (here E0574 naming it, [`Self::struct_variant`]). An ADT `Self` can only have an
    /// associated type through the impl's trait, so the route is taken for an enum only in a
    /// trait impl (an inherent impl of a struct still takes it: rustc's E0223 there).
    fn assoc_route_for_adt(&mut self, k: ItemKey, name: Name) -> Result<bool, Stop> {
        let enum_ = matches!(self.reads.adt(k)?, Some(adt) if adt.kind == AdtKind::Enum);
        if !enum_ {
            return Ok(true);
        }
        if self.names_variant(k, name)? {
            return Ok(false);
        }
        let header = match self.reads.assoc_parent(self.body.owner)? {
            Some(parent) => self.reads.impl_header(parent.container)?,
            None => None,
        };
        Ok(header.is_some_and(|h| h.trait_.is_some()))
    }

    /// A struct path `P::Assoc { .. }` whose prefix `P` is `Self` or a generic parameter and
    /// whose last segment names an associated type of it (ark-poly's `impl Add for &Sparse<F>
    /// { type Output = Sparse<F>; fn add(..) -> Self::Output { Self::Output { .. } } }`, an
    /// `impl IntoIterator` building `Self::IntoIter { .. }`). The Reference, "Struct
    /// expressions": the path names "a struct, enum variant, or union item"; rustc resolves a
    /// type-relative struct path as a type (`check_struct_path` through
    /// `lower_assoc_path`), so the associated type is projected and normalized, and the
    /// struct or union it IS is the path's ADT at the arguments the normalized type gives it.
    /// E0574 when it normalizes to anything else (an enum named whole, a primitive, a rigid
    /// projection); E0220/E0221/E0223 from [`Self::assoc_ty_of_prefix`] when no single trait
    /// of the prefix declares the name.
    fn assoc_struct_path(
        &mut self,
        at: At,
        prefix: crate::rules::self_path::AssocPrefix,
        seg: Segment,
        last: Option<Name>,
    ) -> Result<Option<(ItemKey, u32, TyList)>, Stop> {
        let Some(projected) = self.assoc_ty_of_prefix(at, at.cause(), prefix, seg)? else { return Ok(None) };
        let t = self.try_structurally_resolve(projected)?;
        let TyView::Adt(k, args) = self.view(t) else {
            self.diag(DiagCode::NotAStruct, at, About::path(last));
            return Ok(None);
        };
        match self.reads.adt(k)? {
            Some(adt) if matches!(adt.kind, AdtKind::Struct | AdtKind::Union) => {
                self.defs.push((at, Def::Adt(k)));
                Ok(Some((k, 0, args)))
            }
            _ => {
                self.diag(DiagCode::NotAStruct, at, About::path(last));
                Ok(None)
            }
        }
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
    pub(crate) fn expr_struct(&mut self, mut f: Frame, e: ExprId, path: PathIdx, fields: List, base: crate::body::StructBase) -> Result<Flow, Stop> {
        use crate::body::StructBase;
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
                StructBase::Expr(b) => {
                    self.resume(f, 2)?;
                    self.push_has_type(b, f.t)?;
                    return Ok(Flow::Wait);
                }
                StructBase::None if f.b == 0 => self.check_missing_fields(e, f.t, f.c, inits)?,
                StructBase::Defaults if f.b == 0 => self.record_default_fields(e, f.t, f.c, f.l, inits)?,
                StructBase::None | StructBase::Defaults => {}
            }
        }
        if f.state == 2 && f.b == 0 {
            self.record_fru_fields(e, f.t, f.l, inits)?;
        }
        Ok(Flow::Done(f.t))
    }

    /// Structural record literal `{ b: 1u8, a: 2i32 }` and record update `{ a: 9, ..base }`.
    /// States:
    /// - 0: dispatch `StructBase::Expr` (push base -> state 1) vs `StructBase::None`/`Defaults`
    ///   (expected record -> state 3, inferred anonymous record -> state 4).
    /// - 1: base finished; resolve base record type, fall through to state 2.
    /// - 2: check written fields against base record type, then record `fru_fields` and finish.
    /// - 3: check written fields against expected record type, check missing fields, finish.
    /// - 4: check written fields with no expectation, assemble canonical `TyView::Record`.
    pub(crate) fn expr_record(
        &mut self,
        mut f: Frame,
        e: ExprId,
        fields: List,
        base: crate::body::StructBase,
    ) -> Result<Flow, Stop> {
        use crate::body::StructBase;
        let inits = self.body.field_init_list(fields)?;
        if f.state == 0 {
            let exp_rec = match f.expect.only_has_type() {
                Some(t) => {
                    let r = self.try_structurally_resolve(t)?;
                    match self.view(r) {
                        TyView::Record(_, l) => Some((r, l)),
                        _ => None,
                    }
                }
                None => None,
            };
            match base {
                StructBase::Expr(base_expr) => {
                    let base_expect = match exp_rec {
                        Some((r, _)) => Expectation::HasType(r),
                        None => Expectation::None,
                    };
                    self.resume(f, 1)?;
                    self.push_expr(base_expr, base_expect, Then::Nothing)?;
                    return Ok(Flow::Wait);
                }
                StructBase::None | StructBase::Defaults => {
                    f.a = 0;
                    if let Some((rec_ty, field_tys)) = exp_rec {
                        let is_wider = base == StructBase::None
                            && if let TyView::Record(exp_schema, _) = self.view(rec_ty)
                                && let Some(exp_names) = self.record_names(exp_schema)
                            {
                                inits.len() > exp_names.len()
                                    && exp_names.iter().all(|&en| inits.iter().any(|w| w.name == en))
                            } else {
                                false
                            };
                        f.t = rec_ty;
                        f.l = field_tys;
                        if is_wider {
                            f.b = 3;
                            f.state = 4;
                        } else {
                            f.b = 0;
                            f.state = 3;
                        }
                    } else {
                        f.b = 2;
                        f.state = 4;
                    }
                }
            }
        }
        if f.state == 1 {
            let base_expr = base.expr().unwrap_or(e);
            let base_ty = self.structurally_resolve(self.ret, At::Expr(base_expr))?;
            match self.view(base_ty) {
                TyView::Record(_, field_tys) => {
                    if let Some(hint) = f.expect.only_has_type() {
                        self.commit_if(|m| -> Result<Option<()>, Stop> {
                            Ok(if m.unify(base_ty, hint)? { Some(()) } else { None })
                        })?;
                    }
                    f.t = base_ty;
                    f.l = field_tys;
                    f.b = 0;
                }
                TyView::Error => {
                    f.t = self.common.err;
                    f.b = 1;
                }
                _ => {
                    self.diag(DiagCode::Mismatch, At::Expr(base_expr), About::found(base_ty));
                    f.t = self.common.err;
                    f.b = 1;
                }
            }
            f.a = 0;
            f.state = 2;
        }
        if f.state == 2 || f.state == 3 {
            let cur_state = f.state;
            if let Some(init) = usize::try_from(f.a).ok().and_then(|i| inits.get(i)).copied() {
                f.a += 1;
                let to = if f.b == 0
                    && let TyView::Record(schema, field_tys) = self.view(f.t)
                    && let Some(idx) = self.record_field_index(schema, init.name)
                    && idx < self.ty.list_len(field_tys)
                {
                    Some(self.ty.list_get(field_tys, idx))
                } else {
                    None
                };
                self.resume(f, cur_state)?;
                match to {
                    Some(t) => self.push_coercible(init.expr, t)?,
                    None => {
                        if f.b == 0 {
                            self.diag(DiagCode::NoField, At::Expr(init.expr), About::found_named(f.t, init.name));
                        }
                        self.push_expr(init.expr, Expectation::None, Then::Nothing)?;
                    }
                }
                return Ok(Flow::Wait);
            }
            if f.b == 0
                && let TyView::Record(schema, field_tys) = self.view(f.t)
            {
                if cur_state == 2 {
                    let n = self.ty.list_len(field_tys);
                    let mut unwritten: blaze_own::Vec<u32, Alloc<'a>> = blaze_own::Vec::new_in(self.alloc);
                    if let Some(names) = self.record_names(schema) {
                        for (i, &nm) in names.iter().enumerate() {
                            if !inits.iter().any(|w| w.name == nm)
                                && let Ok(idx) = u32::try_from(i)
                            {
                                unwritten.push(idx);
                            }
                        }
                    }
                    for idx in unwritten {
                        if idx < n {
                            let t = self.ty.list_get(field_tys, idx);
                            self.fru_fields.push((e, idx, t));
                        }
                    }
                } else {
                    let has_omitted = self.record_names(schema).is_some_and(|names| {
                        names.iter().any(|&nm| !inits.iter().any(|w| w.name == nm))
                    });
                    if has_omitted {
                        let n = self.ty.list_len(field_tys);
                        let mut missing: Option<Name> = None;
                        let mut unwritten: blaze_own::Vec<(u32, ItemKey), Alloc<'a>> = blaze_own::Vec::new_in(self.alloc);
                        {
                            let defaults = self.reads.field_defaults(schema.defaults_key())?;
                            if let Some(names) = self.record_names(schema) {
                                for (i, &nm) in names.iter().enumerate() {
                                    if inits.iter().any(|w| w.name == nm) {
                                        continue;
                                    }
                                    match defaults.get(i).copied().flatten() {
                                        Some(key) => {
                                            if let Ok(idx) = u32::try_from(i) {
                                                unwritten.push((idx, key));
                                            }
                                        }
                                        None => {
                                            if missing.is_none() {
                                                missing = Some(nm);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(nm) = missing {
                            self.diag(DiagCode::MissingFields, At::Expr(e), About::found_named(f.t, nm));
                        } else {
                            for (idx, key) in unwritten {
                                if idx < n {
                                    let t = self.ty.list_get(field_tys, idx);
                                    self.fru_fields.push((e, idx, t));
                                    self.default_fields.push((e, idx, key));
                                }
                            }
                        }
                    }
                }
            }
            return Ok(Flow::Done(f.t));
        }
        // State 4: inferred anonymous record literal `{ b: 1u8, a: 2i32 }` (or wider record
        // literal coerced via width-subtyping when `f.b == 3`).
        if let Some(init) = usize::try_from(f.a).ok().and_then(|i| inits.get(i)).copied() {
            f.a += 1;
            let hint = if f.b == 3
                && let TyView::Record(exp_schema, field_tys) = self.view(f.t)
                && let Some(idx) = self.record_field_index(exp_schema, init.name)
                && idx < self.ty.list_len(field_tys)
            {
                Expectation::HasType(self.ty.list_get(field_tys, idx))
            } else {
                Expectation::None
            };
            self.resume(f, 4)?;
            self.push_expr(init.expr, hint, Then::Nothing)?;
            return Ok(Flow::Wait);
        }
        let schema = match self.body.record_expr_schema(e) {
            Some(s) => s,
            None => self.infer_record_schema(inits)?,
        };
        let field_count = self.record_names(schema).map_or(inits.len(), |ns| ns.len());
        let mut canonical: blaze_own::Vec<Ty, Alloc<'a>> = blaze_own::Vec::with_capacity_in(field_count, self.alloc);
        canonical.resize(field_count, self.common.err);
        for init in inits {
            if let Some(idx) = self.record_field_index(schema, init.name)
                && let Some(ix) = usize::try_from(idx).ok()
                && let Some(slot) = canonical.get_mut(ix)
                && let Some(&ety) = self.expr_ty.get(init.expr.ix())
            {
                *slot = ety;
            }
        }
        let list = self.mk_list(&canonical)?;
        Ok(Flow::Done(self.mk(TyView::Record(schema, list))?))
    }

    /// Fallback canonical schema lookup/creation for hand-built unit-test bodies without
    /// `BodyView::record_expr_schemas`: reuses any already-registered schema matching the exact
    /// set of field names, otherwise sorts by `Name` and registers a fresh schema.
    fn infer_record_schema(&mut self, inits: &[crate::body::FieldInit]) -> Result<RecordSchema, Stop> {
        if inits.is_empty() {
            self.register_record_schema(RecordSchema::EMPTY, &[])?;
            return Ok(RecordSchema::EMPTY);
        }
        for &(schema, list) in self.record_schemas.iter() {
            let Some(names) = self.record_names.get(list.range()) else { continue };
            if names.len() == inits.len() && names.iter().all(|&nm| inits.iter().any(|w| w.name == nm)) {
                return Ok(schema);
            }
        }
        let mut names: blaze_own::Vec<Name, Alloc<'a>> = blaze_own::Vec::with_capacity_in(inits.len(), self.alloc);
        for init in inits {
            if !names.contains(&init.name) {
                names.push(init.name);
            }
        }
        names.sort_unstable();
        let schema = RecordSchema::of_sorted_names(&names);
        self.register_record_schema(schema, &names)?;
        Ok(schema)
    }

    /// `fru_field_types`: after the base of `S { a: x, ..base }` unified with `S<args>`, one
    /// row per field the update takes from the base (every field not written), in declaration
    /// order, with its declared type at `args`. The flow lowering moves or copies each
    /// `base.f` by that type (blaze-flow-lower, `Expr::Struct`), so a place base is moved out
    /// of field by field, as `let x = base.f;` is. The Reference, "Struct expressions",
    /// "Functional update syntax": the base must be a struct type, so a union or an enum
    /// variant is refused by name (rustc's E0436, for which the checker has no code).
    fn record_fru_fields(&mut self, e: ExprId, adt_ty: Ty, args: TyList, inits: &[crate::body::FieldInit]) -> Result<(), Stop> {
        let TyView::Adt(k, _) = self.view(adt_ty) else { return Ok(()) };
        let Some(adt) = self.reads.adt(k)? else { return Ok(()) };
        if adt.kind != AdtKind::Struct {
            return Err(CheckError::Unported("functional record update of a union or an enum variant (E0436)").into());
        }
        let Some(v) = adt.variants.first() else { return Ok(()) };
        let declared = adt.fields.get(v.fields.range()).unwrap_or(&[]);
        for (i, d) in declared.iter().enumerate() {
            if inits.iter().any(|w| w.name == d.name) {
                continue;
            }
            let t = self.lower(adt.types, d.ty, Lower::Args(args), crate::ty::Cause::Expr(e))?;
            self.fru_fields.push((e, i as u32, t));
        }
        Ok(())
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

    /// `S { a: 1, .. }` with no base (RFC 3681, `default_field_values`): each field of the
    /// variant the expression does not write takes its default, the field's anonymous constant
    /// at the expression's type arguments. A field the expression leaves out that has no
    /// default is E0063 (`MissingFields`), one finding per such field, each naming it. Each
    /// defaulted field gets a `fru_fields` row (its index in the variant and its type at
    /// `args`, so the lowering sizes the aggregate as a struct update's) and a
    /// `default_fields` row (the default's key, the value the lowering puts there). A union
    /// has no defaults (the declaration is refused where its head is made), so every one of
    /// its unwritten fields is simply unwritten, as `U { a: 1 }` is.
    fn record_default_fields(&mut self, e: ExprId, adt_ty: Ty, variant: u32, args: TyList, inits: &[crate::body::FieldInit]) -> Result<(), Stop> {
        let TyView::Adt(k, _) = self.view(adt_ty) else { return Ok(()) };
        let Some(adt) = self.reads.adt(k)? else { return Ok(()) };
        let Some(v) = adt.variants.get(variant as usize) else { return Ok(()) };
        let defaults = self.reads.field_defaults(k)?;
        let range = v.fields.range();
        let first = range.start;
        let declared = adt.fields.get(range).unwrap_or(&[]);
        for (i, d) in declared.iter().enumerate() {
            if inits.iter().any(|w| w.name == d.name) {
                continue;
            }
            match defaults.get(first + i).copied().flatten() {
                Some(key) => {
                    let t = self.lower(adt.types, d.ty, Lower::Args(args), crate::ty::Cause::Expr(e))?;
                    self.fru_fields.push((e, i as u32, t));
                    self.default_fields.push((e, i as u32, key));
                }
                None if adt.kind == AdtKind::Union => {}
                None => self.diag(DiagCode::MissingFields, At::Expr(e), About::found_named(adt_ty, d.name)),
            }
        }
        Ok(())
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
