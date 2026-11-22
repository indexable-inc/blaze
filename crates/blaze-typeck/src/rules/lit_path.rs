//! Literals and value paths.
//!
//! Follows rustc_hir_typeck/src/expr.rs (`check_lit`), fn_ctxt/_impl.rs
//! (`instantiate_value_path`, `resolve_ty_and_res_fully_qualified_call`) and method/mod.rs
//! (`resolve_fully_qualified_call`, the probe in `Mode::Path`); Relax maps them in typeck.rs
//! (`check_expr_inner`'s literal arm, `resolve_value_path_inner`, `value_def_ty_with_self`).
//!
//! A path is resolved through the world up to the segment it consumes; one more segment is
//! type-relative (`S::new`, `T::default`) and is resolved here: inherent items of the self
//! type first, then the traits in scope, as rustc's probe does without autoderef.

use blaze_own::{Alloc, Vec};

use crate::body::{BodyPath, IntSuffix, Lit, Segment};
use crate::diag::{About, At, DiagCode};
use crate::error::{CheckError, Malformed, Stop};
use crate::ids::{ExprId, ItemKey, Mutbl, Name, PathIdx, ScopeId};
use crate::machine::{Expectation, Flow, Frame, Lower, Machine, Then};
use crate::portable::{PNode, PTy, PTys};
use crate::ty::{Cause, Goal, Ty, TyList, TyView, TypeCtx, VarKind};
use crate::path_miss::{MissWhy, PathMiss, PathPrefix};
use crate::world::{AssocKind, CtorKind, Def, FnSig, PathRes, Shape, World};

/// The segment `Self` (`Name::of`, as the world spells every name).
const SELF_NAME: Name = Name::of("Self");

/// Why a body path has no definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unresolved {
    /// Not found by its name: E0425 naming the segment the world's answer says failed.
    ByName(PathMiss),
    /// A desugaring's lang path the world cannot answer: already reported as
    /// `DiagCode::MissingLangItem` naming the lang item.
    Reported,
}

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// `check_lit`.
    pub(crate) fn expr_lit(&mut self, f: Frame, e: ExprId, lit: Lit) -> Result<Flow, Stop> {
        Ok(Flow::Done(self.lit_ty(lit, f.expect, At::Expr(e))?))
    }

    /// A literal's type under an expectation; `at` is the literal, named when a lang item it
    /// needs is missing.
    pub(crate) fn lit_ty(&mut self, lit: Lit, expect: Expectation, at: At) -> Result<Ty, Stop> {
        let c = self.common;
        Ok(match lit {
            Lit::Bool(_) => c.bool_,
            Lit::Char => c.char_,
            Lit::Str => c.static_str,
            Lit::CStr => match self.need_lang(crate::ids::LangItem::CStr, at)? {
                Some(k) => {
                    let none = self.mk_list(&[])?;
                    let cstr = self.mk(TyView::Adt(k, none))?;
                    self.mk(TyView::Ref(Mutbl::Not, cstr))?
                }
                None => c.err,
            },
            Lit::ByteStr(n) => {
                let n = self.len_value(n)?;
                let arr = self.mk(TyView::Array(c.u8_, n))?;
                self.mk(TyView::Ref(Mutbl::Not, arr))?
            }
            Lit::Int(Some(IntSuffix::Int(i))) => self.mk(TyView::Int(i))?,
            Lit::Int(Some(IntSuffix::Uint(u))) => self.mk(TyView::Uint(u))?,
            Lit::Int(None) => {
                // An unsuffixed integer takes an expected integer type directly; an expected
                // `char` gives `u8`, a pointer or fn `usize` (for the cast that follows).
                let hint = match expect.to_option() {
                    Some(t) => match self.view(t) {
                        TyView::Int(_) | TyView::Uint(_) => Some(self.ty.shallow_resolve(t)),
                        TyView::Char => Some(c.u8_),
                        TyView::Ptr(..) | TyView::FnDef(..) | TyView::FnPtr(..) => Some(c.usize_),
                        _ => None,
                    },
                    None => None,
                };
                match hint {
                    Some(t) => t,
                    None => self.fresh_int()?,
                }
            }
            Lit::Float(Some(fl)) => self.mk(TyView::Float(fl))?,
            Lit::Float(None) => {
                let hint = match expect.to_option() {
                    Some(t) => match self.view(t) {
                        TyView::Float(_) => Some(self.ty.shallow_resolve(t)),
                        _ => None,
                    },
                    None => None,
                };
                match hint {
                    Some(t) => t,
                    None => self.fresh_float()?,
                }
            }
        })
    }

    /// Parentheses pass the expectation through; the parent's coercion applies to the
    /// parenthesized expression.
    pub(crate) fn expr_paren(&mut self, f: Frame, inner: ExprId) -> Result<Flow, Stop> {
        match f.state {
            0 => {
                self.resume(f, 1)?;
                self.push_expr(inner, f.expect, Then::Nothing)?;
                Ok(Flow::Wait)
            }
            _ => Ok(Flow::Done(self.ret)),
        }
    }

    pub(crate) fn expr_path(&mut self, expect: Expectation, e: ExprId, path: PathIdx) -> Result<Flow, Stop> {
        let before = self.diags.len();
        let ty = self.value_path(At::Expr(e), Cause::Expr(e), path)?;
        // An E0425 at a one-segment name: a hole `rules::suggest` ranks a fix for. Only on this
        // error path: a clean body pays one length compare per path.
        if self.diags.len() > before {
            let bp = self.body.path(path)?;
            let named = self.diags[before..].iter().any(|d| d.code == DiagCode::UnresolvedPath && d.at == At::Expr(e));
            if named && bp.anchor.is_none() && bp.lang.is_none() && bp.segments.len == 1 {
                self.named_holes.push((e, expect));
            }
        }
        Ok(Flow::Done(ty))
    }

    /// Explicit generic arguments of a segment (`::<..>`) for parameters `from..from + count`
    /// of `item`'s generics, or fresh ones. A wrong count is E0107, and the arguments are then
    /// fresh. Type and const parameters are counted together, in declaration order (the
    /// Reference, "Generic arguments"). A const argument is checked against its parameter's
    /// declared type (the Reference, "Const generics": "the type of the const argument must
    /// match the type of the const parameter"): E0308 on a mismatch; a constant given for a
    /// type parameter, or a type for a const parameter, is E0747. Either error leaves the
    /// argument fresh.
    pub(crate) fn explicit_args(
        &mut self,
        at: At,
        seg: Option<Segment>,
        item: ItemKey,
        from: u32,
        count: u32,
    ) -> Result<TyList, Stop> {
        self.explicit_args_declared(at, seg, item, from, count, count)
    }

    /// A fn's own generic arguments: [`Self::explicit_args`] for `sig.own_count` parameters of
    /// which only the first `sig.declared_count` may be written; the anonymous ones of its
    /// `impl Trait` arguments are always fresh (the Reference, "Impl trait").
    pub(crate) fn explicit_fn_args(&mut self, at: At, seg: Option<Segment>, item: ItemKey, sig: &FnSig<'_>) -> Result<TyList, Stop> {
        if sig.declared_count > sig.own_count {
            let site = crate::error::ParamSite::ExplicitArgs { item };
            return Err(CheckError::Malformed(Malformed::Param { index: sig.declared_count, args: sig.own_count, site }).into());
        }
        self.explicit_args_declared(at, seg, item, sig.parent_count, sig.declared_count, sig.own_count)
    }

    /// Parameters `from..from + count` of `item`, the first `declared` of them written in `seg`
    /// (all or none), the rest fresh.
    fn explicit_args_declared(
        &mut self,
        at: At,
        seg: Option<Segment>,
        item: ItemKey,
        from: u32,
        declared: u32,
        count: u32,
    ) -> Result<TyList, Stop> {
        let Some(seg) = seg.filter(|s| s.has_args) else {
            return self.fresh_generic_args(item, from, count);
        };
        let written = self.body.type_list(seg.generic_args)?;
        if written.len() != declared as usize {
            self.diag(DiagCode::GenericArgCount, at, About::name(seg.name));
            let got = u32::try_from(written.len()).unwrap_or(u32::MAX);
            if let Some(d) = self.diags.last_mut() {
                d.because = Some(crate::diag::Because::ArgCount { item, expected: declared, got });
            }
            return self.fresh_generic_args(item, from, count);
        }
        let consts = if count == 0 { None } else { Some(self.reads.const_params(item)?) };
        let mut v: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(count as usize, self.alloc);
        for (i, &t) in written.iter().enumerate() {
            let declared = consts.and_then(|c| c.type_of(from + i as u32).map(|d| (d, c.types)));
            v.push(self.generic_arg(at, t, declared)?);
        }
        // The anonymous parameters: type parameters, inferred from the arguments.
        for _ in declared..count {
            v.push(self.fresh()?);
        }
        self.mk_list(&v)
    }

    /// One written generic argument `t` for a parameter whose declared type is `declared`
    /// (a const parameter) or `None` (a type parameter). An unsuffixed integer constant takes
    /// the parameter's type, as an integer literal takes its expected type.
    fn generic_arg(&mut self, at: At, t: PTy, declared: Option<(PTy, PTys<'_>)>) -> Result<Ty, Stop> {
        let node = self.body.types.node(t)?;
        let Some((d, dtypes)) = declared else {
            if let PNode::ConstValue { ty, .. } = node {
                let const_ty = self.lower_body_ty(ty, at)?;
                self.diag(DiagCode::GenericArgKind, at, About::found(const_ty));
                return self.fresh();
            }
            return self.lower_body_ty(t, at);
        };
        // A hole for a const parameter is a const variable, as an array length's hole is
        // (`lower_len`), never a type variable (which is E0747 below): `_` as written (the
        // Reference, "Const generics": a const argument may be an "inferred const"), and a
        // const argument the front end could not value (blaze-lower `ConstGap`: the gap is
        // the front end's to report; here it takes the one value that makes the body check).
        if let PNode::Infer = node {
            return self.ty.fresh(VarKind::Const).map_err(Stop::from);
        }
        let empty = self.mk_list(&[])?;
        let d = self.lower(dtypes, d, Lower::Args(empty), at.cause())?;
        if let PNode::ConstValue { ty, bits } = node
            && let PNode::Infer = self.body.types.node(ty)?
        {
            if !matches!(self.view(d), TyView::Int(_) | TyView::Uint(_)) {
                self.diag(DiagCode::Mismatch, at, About::expected(d));
                return self.ty.fresh(VarKind::Const).map_err(Stop::from);
            }
            return self.mk(TyView::ConstValue { ty: d, bits });
        }
        let arg = self.lower_body_ty(t, at)?;
        let found = match self.view(arg) {
            TyView::ConstValue { ty, .. } => ty,
            TyView::ConstParam(j) => {
                let owner = self.body.owner;
                let own = self.reads.const_params(owner)?;
                let Some(pt) = own.type_of(j) else {
                    let site = crate::error::ParamSite::ConstParamType { owner };
                    return Err(CheckError::Malformed(Malformed::Param { index: j, args: 0, site }).into());
                };
                let empty = self.mk_list(&[])?;
                self.lower(own.types, pt, Lower::Args(empty), at.cause())?
            }
            TyView::ConstInfer(_) => return Ok(arg),
            _ => {
                self.diag(DiagCode::GenericArgKind, at, About::found(arg));
                return self.ty.fresh(VarKind::Const).map_err(Stop::from);
            }
        };
        if !self.demand_eq(at, d, found)? {
            return self.ty.fresh(VarKind::Const).map_err(Stop::from);
        }
        Ok(arg)
    }

    /// `parent` followed by `own`.
    pub(crate) fn concat(&mut self, parent: TyList, own: TyList) -> Result<TyList, Stop> {
        let mut v = self.list(parent);
        let own = self.list(own);
        v.extend_from_slice(&own);
        self.mk_list(&v)
    }

    /// A body path's resolution: by name, or, for a desugaring's path, the lang item's
    /// definition with nothing consumed (it has no segments).
    /// A lang path the world cannot answer is the finding naming the lang item, at `at`, and
    /// `Unresolved::Reported`: never an E0425 without a name.
    pub(crate) fn path_res(
        &mut self,
        bp: BodyPath,
        path: PathIdx,
        names: &[Name],
        at: At,
    ) -> Result<Result<PathRes, Unresolved>, Stop> {
        match bp.lang {
            Some(l) => match self.reads.lang_path(l)? {
                Some(def) => Ok(Ok(PathRes { def, consumed: 0 })),
                None => {
                    self.missing_lang(l, at);
                    Ok(Err(Unresolved::Reported))
                }
            },
            None => Ok(self.reads.resolve_path(bp.scope, path, names)?.map_err(Unresolved::ByName)),
        }
    }

    /// E0425 for a path written as `names` and not found by name, naming the segment that
    /// failed; nothing for one already reported.
    pub(crate) fn unresolved(&mut self, why: Unresolved, at: At, names: &[Name]) {
        if let Unresolved::ByName(miss) = why {
            self.path_miss(at, names, miss);
        }
    }

    /// E0425 at `at` for `miss` of the path written as `names` (rule 19): `name` is the
    /// failing segment's (the last one's only when the world does not say which,
    /// [`PathMiss::Whole`]), `because` is `miss` itself.
    pub(crate) fn path_miss(&mut self, at: At, names: &[Name], miss: PathMiss) {
        let name = match miss.segment() {
            Some(i) => names.get(i as usize).copied(),
            None => names.last().copied(),
        };
        self.diag(DiagCode::UnresolvedPath, at, About::path(name));
        if let Some(d) = self.diags.last_mut() {
            d.because = Some(crate::diag::Because::Path(miss));
        }
    }

    /// The type of a value path (`instantiate_value_path`).
    pub(crate) fn value_path(&mut self, at: At, cause: Cause, path: PathIdx) -> Result<Ty, Stop> {
        let bp = self.body.path(path)?;
        let segs = self.body.segment_list(bp.segments)?;
        let mut names: Vec<Name, Alloc<'a>> = Vec::with_capacity_in(segs.len(), self.alloc);
        for s in segs {
            names.push(s.name);
        }
        let last = segs.last().copied();
        let last_name = last.map(|s| s.name);
        if let Some(anchor) = bp.anchor {
            return self.anchored_value(at, cause, anchor, segs, bp.scope);
        }
        let res = match self.path_res(bp, path, &names, at)? {
            Ok(res) => res,
            Err(why) => {
                self.unresolved(why, at, &names);
                return Ok(self.common.err);
            }
        };
        let consumed = res.consumed as usize;
        if consumed == segs.len() {
            // The world answers `Self` in an impl of an ADT with its type (`Def::Adt`, one
            // segment): as a value it is the struct's constructor ([`Self::self_ctor_value`]).
            if let (Def::Adt(adt), [seg]) = (res.def, segs) {
                if seg.name == SELF_NAME {
                    return self.self_ctor_value(at, cause, adt, *seg);
                }
            }
            return self.value_def(at, cause, res.def, segs);
        }
        if consumed == 0 {
            self.diag(DiagCode::UnresolvedPath, at, About::path(last_name));
            return Ok(self.common.err);
        }
        // Type-relative: the consumed prefix names a type. One segment after it is an
        // associated item of that type; two are an associated type of the prefix, then an
        // item of THAT type (`T::Assoc::f`, `Self::Error::new`: rules/self_path.rs, where the
        // rules are quoted). A longer path would go through two associated types.
        let after = segs.len() - consumed;
        if after > 2 {
            return Err(CheckError::Unported("a value path through two or more associated types (`T::A::B::f`)").into());
        }
        if after == 2 {
            use crate::rules::self_path::AssocPrefix;
            let prefix = match res.def {
                Def::TyParam(i) => AssocPrefix::Param(i),
                Def::SelfTy => AssocPrefix::ImplSelf,
                Def::Adt(_) if consumed == 1 && segs[0].name == SELF_NAME => AssocPrefix::ImplSelf,
                _ => AssocPrefix::Other,
            };
            let Some(projected) = self.assoc_ty_of_prefix(at, cause, prefix, segs[consumed])? else {
                return Ok(self.common.err);
            };
            return self.assoc_value(at, cause, projected, segs[consumed + 1], bp.scope);
        }
        let self_ty = match res.def {
            // `Self` in an impl of a type that is no ADT (the Reference, "Paths", "`Self`": "In
            // an implementation, it refers to the type being implemented."): the impl's self
            // type; its items are found as any type's are (a parameter's through its bounds).
            Def::SelfTy => match self.impl_self_ty(cause)? {
                Some(t) => t,
                None => {
                    // `Self` outside an impl: the first segment names nothing here.
                    self.path_miss(at, &names, PathMiss::At { segment: 0, after: None, why: MissWhy::NotFound });
                    return Ok(self.common.err);
                }
            },
            Def::Adt(k) => {
                let Some(adt) = self.reads.adt(k)? else {
                    self.diag(DiagCode::UnresolvedPath, at, About::path(last_name));
                    return Ok(self.common.err);
                };
                // `Self::X` in an impl: `Self` is the impl's self type at the impl's own
                // arguments (rules/self_path.rs `impl_self_adt_args`), so `Self::HIGH` in
                // `impl<const SLOTS: usize> Ctrl<SLOTS>` leaves no variable; a written name
                // (`Ctrl::HIGH`) takes fresh arguments, inferred or E0282.
                let by_self = consumed == 1 && segs[0].name == SELF_NAME;
                let args = if by_self {
                    if segs[0].has_args {
                        // The Reference, "Paths", "`Self`": "The Self path cannot include
                        // generic arguments (as in Self::<i32>)." rustc's E0109, for which the
                        // checker has no code: refused by name.
                        return Err(CheckError::Unported("generic arguments on `Self` in a value path (E0109)").into());
                    }
                    if adt.generics == 0 {
                        // An ADT with no generic parameters has one argument list, the empty
                        // one, so `Self` needs nothing from an impl: the body may be in none.
                        // front2's anonymous constant `[u8; Self::BYTES]` of an `impl L` (anon.rs
                        // `Ask::of_self`, filed with `Self` = `L` and no container) is checked
                        // as `const _: usize = Self::BYTES;` and has no assoc parent.
                        self.common.empty
                    } else {
                        match self.impl_self_adt_args(at, cause, k)? {
                            Some(args) => args,
                            None => return Ok(self.common.err),
                        }
                    }
                } else {
                    self.explicit_args(at, Some(segs[consumed - 1]), k, 0, adt.generics)?
                };
                // `Self::A`, `Alias::A`: a variant of the enum the prefix names, in an
                // expression or a pattern, before any associated item of that name (rustc's
                // `associated_path_to_ty` checks the variants first; its deny-by-default lint
                // `ambiguous_associated_items` refuses a variant and an associated item of one
                // name through `Self` or an alias, so either order types such a program alike).
                // Without arguments on its segment the variant is found by
                // [`Self::assoc_value`], as through any type-relative path.
                let seg = segs[consumed];
                if adt.kind == crate::world::AdtKind::Enum && seg.has_args {
                    if let Some((i, v)) = adt.variants.iter().enumerate().find(|(_, v)| v.name == seg.name) {
                        let (vkey, ctor) = (v.key, v.ctor);
                        let def = Def::Ctor { adt: k, variant: i as u32, kind: ctor };
                        // `E::A::<T>`: the arguments on the variant's segment.
                        let args = self.explicit_args(at, Some(seg), k, 0, adt.generics)?;
                        let t = self.ctor_ty(at, k, Some(vkey), ctor, args, Some(seg.name))?;
                        if !self.is_err(t) {
                            self.defs.push((at, def));
                        }
                        return Ok(t);
                    }
                }
                self.mk(TyView::Adt(k, args))?
            }
            // `A::new()`, `A::Variant` through `type A<..> = X;` (rules/alias_path.rs): `X` at
            // the alias's segment's arguments, then a variant of the enum it is, then an
            // associated item of it, as for the type written.
            // The variant is found by [`Self::assoc_value`], as for any type-relative path.
            Def::Alias(k) => {
                let Some(t) = self.alias_path_ty(at, cause, k, segs[consumed - 1])? else {
                    return Ok(self.common.err);
                };
                t
            }
            Def::TyParam(i) => self.mk(TyView::Param(i))?,
            // `Trait::NAME` that the world did not answer as the trait's fn (a constant, pyo3's
            // `FunctionArgumentHolder::INIT`): `<_ as Trait>::NAME`.
            // A name the trait does not declare fails at that segment, after the trait (rustc's
            // E0425 "cannot find value `x` in trait `Tr`"; suite/path_miss.rs).
            Def::Trait(tr) => {
                if self.reads.assoc_item(tr, segs[consumed].name)?.is_none() {
                    let miss = PathMiss::At { segment: res.consumed, after: Some(PathPrefix::Def(res.def)), why: MissWhy::NotFound };
                    self.path_miss(at, &names, miss);
                    return Ok(self.common.err);
                }
                return self.trait_relative_value(at, cause, tr, segs[consumed - 1], segs[consumed]);
            }
            // The Reference, "Paths": a type-relative path may begin at a primitive type; its
            // items are its inherent impls' (core's `impl u32 { .. }`), then traits in scope.
            Def::Prim(p) => self.mk(match p {
                crate::world::Prim::Bool => TyView::Bool,
                crate::world::Prim::Char => TyView::Char,
                crate::world::Prim::Str => TyView::Str,
                crate::world::Prim::Int(i) => TyView::Int(i),
                crate::world::Prim::Uint(u) => TyView::Uint(u),
                crate::world::Prim::Float(f) => TyView::Float(f),
            })?,
            // A prefix with no associated values the checker looks in (a trait whose fn of
            // that name the world did not find, a fn, a const, a constructor): the segment
            // after it names nothing, on it.
            _ => {
                let miss = PathMiss::At { segment: res.consumed, after: Some(PathPrefix::Def(res.def)), why: MissWhy::NotFound };
                self.path_miss(at, &names, miss);
                return Ok(self.common.err);
            }
        };
        let seg = segs[consumed];
        self.assoc_value(at, cause, self_ty, seg, bp.scope)
    }

    /// A qualified value path (the Reference, "Qualified paths"). `<T>::x` is `x` looked up on
    /// `T` as `T::x` is (inherent items, then traits in scope). `<T as Tr>::x` is `Tr`'s own
    /// associated item `x` (no inherent item, no other trait, no supertrait) with `Self = T` and
    /// `Tr`'s arguments as written on its path (fresh when none are), and registers `T: Tr`.
    /// `<T as Tr>::Assoc::x` is `x` looked up on the projection `<T as Tr>::Assoc` as on any
    /// type. The lowering admits one segment after `<T>` and at most two after `<T as Tr>`.
    fn anchored_value(
        &mut self,
        at: At,
        cause: Cause,
        anchor: crate::body::Anchor,
        segs: &[Segment],
        scope: ScopeId,
    ) -> Result<Ty, Stop> {
        let malformed = || -> Stop {
            crate::error::CheckError::Malformed(crate::error::Malformed::List {
                column: "segments",
                start: 0,
                len: segs.len() as u32,
            })
            .into()
        };
        let (assoc, seg) = match segs {
            [seg] => (None, seg),
            [assoc, seg] if anchor.trait_.is_some() => (Some(*assoc), seg),
            _ => return Err(malformed()),
        };
        let self_ty = self.lower_body_ty(anchor.ty, at)?;
        let Some(tpath) = anchor.trait_ else {
            return self.assoc_value(at, cause, self_ty, *seg, scope);
        };
        let tbp = self.body.path(tpath)?;
        let tsegs = self.body.segment_list(tbp.segments)?;
        let mut names: Vec<Name, Alloc<'a>> = Vec::with_capacity_in(tsegs.len(), self.alloc);
        for s in tsegs {
            names.push(s.name);
        }
        let tlast = tsegs.last().copied();
        let tr = match self.path_res(tbp, tpath, &names, at)? {
            Ok(PathRes { def: Def::Trait(tr), consumed }) if consumed as usize == tsegs.len() => tr,
            Ok(_) => {
                self.diag(DiagCode::NotATrait, at, About::path(tlast.map(|s| s.name)));
                return Ok(self.common.err);
            }
            Err(why) => {
                self.unresolved(why, at, &names);
                return Ok(self.common.err);
            }
        };
        let Some(def) = self.reads.trait_def(tr)? else {
            self.diag(DiagCode::UnresolvedPath, at, About::path(tlast.map(|s| s.name)));
            return Ok(self.common.err);
        };
        let trait_args = match tlast.filter(|s| s.has_args) {
            // Written: exactly as written (`explicit_args`), `Self` first.
            Some(_) => {
                let rest = self.explicit_args(at, tlast, tr, 1, def.generics.saturating_sub(1))?;
                let self_list = self.mk_list(&[self_ty])?;
                self.concat(self_list, rest)?
            }
            // None written: the trait's defaults, as any type path's (rules/qpath_defaults.rs).
            None => self.qpath_trait_args(tr, &def, self_ty, cause)?,
        };
        if let Some(assoc) = assoc {
            let Some(decl) = def.assoc_types.iter().find(|a| a.name == assoc.name) else {
                self.diag(DiagCode::NoAssocItem, at, About::found_named(self_ty, assoc.name));
                return Ok(self.common.err);
            };
            self.register_trait(tr, trait_args, self.written_cause(anchor.ty_range, cause))?;
            // The GAT's own parameters follow Self and the trait parameters. Check arity
            // before lowering any argument against its corresponding declaration.
            let own = assoc.generic_args.len;
            let written = def.generics.saturating_add(own);
            if !self.alias_arity(tr, assoc.name, written, cause)? {
                return Ok(self.common.err);
            }
            let projection_args = if own == 0 {
                trait_args
            } else {
                let args = self.explicit_args(at, Some(assoc), decl.item, def.generics, own)?;
                self.concat(trait_args, args)?
            };
            let projected = self.project(tr, projection_args, assoc.name, cause)?;
            let projected = self.try_structurally_resolve(projected)?;
            return self.assoc_value(at, cause, projected, *seg, scope);
        }
        let Some(item) = self.reads.assoc_item(tr, seg.name)? else {
            self.diag(DiagCode::NoAssocItem, at, About::found_named(self_ty, seg.name));
            return Ok(self.common.err);
        };
        self.register_trait(tr, trait_args, self.written_cause(anchor.ty_range, cause))?;
        self.assoc_item_ty(at, cause, item.item, item.kind, trait_args, Some(*seg))
    }

    /// `Trait::NAME` written without a qualified self type: the trait's own associated item
    /// `NAME` with `Self` inferred, the path `<_ as Trait>::NAME` (the Reference, "Paths",
    /// "Paths in expressions": a path to an associated item through its trait; "Qualified
    /// paths": `<_ as Trait>` leaves the self type to inference; rustc_resolve's
    /// `TraitItem` resolution of a value path whose prefix is a trait). pyo3 0.29's
    /// `#[pymethods]` writes `let mut holder = ::pyo3::impl_::extract_argument::
    /// FunctionArgumentHolder::INIT;` and the later `&mut holder` argument fixes `Self`. The
    /// trait's arguments are the ones written on its segment (`Tr::<A>::C`), `Self` fresh, or
    /// all fresh where none are written, as a trait's fn reached this way takes them
    /// ([`Self::parent_args`]); `Self: Trait` is registered. A name the trait does not declare
    /// is E0576's class, `NoAssocItem`, never "not found in this scope".
    fn trait_relative_value(&mut self, at: At, cause: Cause, tr: ItemKey, tseg: Segment, seg: Segment) -> Result<Ty, Stop> {
        let Some(def) = self.reads.trait_def(tr)? else {
            self.diag(DiagCode::UnresolvedPath, at, About::path(Some(tseg.name)));
            return Ok(self.common.err);
        };
        let trait_args = if tseg.has_args {
            let self_ty = self.fresh_generic_args(tr, 0, 1)?;
            let rest = self.explicit_args(at, Some(tseg), tr, 1, def.generics.saturating_sub(1))?;
            self.concat(self_ty, rest)?
        } else {
            self.fresh_generic_args(tr, 0, def.generics)?
        };
        let Some(item) = self.reads.assoc_item(tr, seg.name)? else {
            let self_ty = if self.ty.list_len(trait_args) == 0 { self.common.err } else { self.ty.list_get(trait_args, 0) };
            self.diag(DiagCode::NoAssocItem, at, About::found_named(self_ty, seg.name));
            return Ok(self.common.err);
        };
        self.register_trait(tr, trait_args, cause)?;
        self.assoc_item_ty(at, cause, item.item, item.kind, trait_args, Some(seg))
    }

    /// `Self` used as a value in an impl (the Reference, "Paths", "`Self`": "In an
    /// implementation, it refers to the type being implemented. When implementing a tuple or
    /// unit struct, it also refers to the constructor in the value namespace."): the struct's
    /// ctor through the one home for ctors ([`Self::ctor_ty`]), at the arguments of the impl's
    /// own self type as the body sees them (`Self(t)` in `impl<T> W<T>` builds `W<T>`, not a
    /// `W` at fresh arguments). A braced struct, an enum or a union has no ctor in the value
    /// namespace: E0423, as its name gives. Only the segment `Self` takes this rule: a `type`
    /// alias does not (the Reference, "Type aliases": "A type alias to a tuple-struct or
    /// unit-struct cannot be used to qualify that type's constructor"), so `type A = S; A(1)`
    /// stays E0423 through [`Self::value_def`].
    fn self_ctor_value(&mut self, at: At, cause: Cause, adt: ItemKey, seg: Segment) -> Result<Ty, Stop> {
        let name = Some(seg.name);
        let header = match self.reads.assoc_parent(self.body.owner)? {
            Some(parent) => self.reads.impl_header(parent.container)?,
            None => None,
        };
        // `Self` outside an impl (a trait's default body answers `Def::TyParam`, not here).
        let Some(h) = header else {
            self.diag(DiagCode::NotAValue, at, About::path(name));
            return Ok(self.common.err);
        };
        let self_ty = self.lower(h.types, h.self_ty, Lower::Body, cause)?;
        let self_ty = self.try_structurally_resolve(self_ty)?;
        let args = match self.view(self_ty) {
            TyView::Adt(k, args) if k == adt => args,
            // The world's `Self` and the impl's header name different types: no value to give.
            _ => {
                self.diag(DiagCode::UnresolvedPath, at, About::found_maybe_named(self_ty, name));
                return Ok(self.common.err);
            }
        };
        let Some(info) = self.reads.adt(adt)? else {
            self.diag(DiagCode::UnresolvedPath, at, About::path(name));
            return Ok(self.common.err);
        };
        let first = info.variants.first().map(|v| (v.key, v.ctor));
        let (vkey, ctor) = match (info.kind, first) {
            (crate::world::AdtKind::Struct, Some(v)) => v,
            _ => {
                self.diag(DiagCode::NotAValue, at, About::found_maybe_named(self_ty, name));
                return Ok(self.common.err);
            }
        };
        let t = self.ctor_ty(at, adt, Some(vkey), ctor, args, name)?;
        if !self.is_err(t) {
            self.defs.push((at, Def::Ctor { adt, variant: 0, kind: ctor }));
        }
        Ok(t)
    }

    /// A fully resolved value definition, recorded at its node (`Checked::expr_defs`,
    /// `pat_defs`) once it typed without an error.
    fn value_def(&mut self, at: At, cause: Cause, def: Def, segs: &[Segment]) -> Result<Ty, Stop> {
        let t = self.value_def_ty(at, cause, def, segs)?;
        if !self.is_err(t) {
            self.defs.push((at, def));
        }
        Ok(t)
    }

    /// Records the generic arguments a constant's value path is read at (`Checked::const_args`),
    /// so the builder evaluates the constant at the instance's arguments (the Reference,
    /// "Associated Items", "Associated Constants": "An associated constant definition defines a
    /// constant associated with a type": `<T as Tr>::C` at `T = u8` is `u8`'s impl's value, or the
    /// trait's default where that impl writes none). A constant with no generics (a free `const`
    /// item) has no row. A pattern's constant has its row too (`Checked::pat_const_args`):
    /// an inherent impl's constant (`U256::ZERO` of `impl<const BITS: usize, const LIMBS:
    /// usize> Uint<BITS, LIMBS>`) has a value only at the impl's arguments, which a constant
    /// pattern never takes from a generic parameter (E0158), so they are known once checked.
    fn record_const_args(&mut self, at: At, args: TyList) {
        if matches!(at, At::Expr(_) | At::Pat(_)) && self.ty.list_len(args) != 0 {
            self.const_args.push((at, args));
        }
    }

    /// The parent's arguments of `item` (its first `count` generics) reached by the whole path
    /// `segs`: for an item of a trait named through the trait (`Tr::<A>::f`, `Tr::<A>::C`), the
    /// arguments written on the trait's segment for the trait's parameters after `Self`, with
    /// `Self` fresh (the Reference, "Paths", "Paths in expressions": generic arguments are
    /// written on the segment of the item they belong to; "Traits": a trait's `Self` is its
    /// implicit first parameter, inferred). alloy-sol-types 1.x: `impl EventTopic for Address
    /// { fn encode_topic(rust: &Self::RustType) -> WordToken {
    /// crate::private::SolTypeValue::<Self>::stv_to_tokens(rust) } }`, where `T = Self` is
    /// what picks the impl: dropped, `T` was a fresh variable nothing fixes and the call
    /// E0283 `Address: SolTypeValue<?>`. Every other parent (none written, an impl's item):
    /// fresh, inferred.
    fn parent_args(&mut self, at: At, item: ItemKey, segs: &[Segment], count: u32) -> Result<TyList, Stop> {
        let written = match segs {
            [.., tseg, _] if tseg.has_args && count > 0 => Some(*tseg),
            _ => None,
        };
        let Some(tseg) = written else { return self.fresh_generic_args(item, 0, count) };
        let Some(parent) = self.reads.assoc_parent(item)? else { return self.fresh_generic_args(item, 0, count) };
        let Some(def) = self.reads.trait_def(parent.container)? else { return self.fresh_generic_args(item, 0, count) };
        if def.generics != count {
            let site = crate::error::ParamSite::ExplicitArgs { item };
            return Err(CheckError::Malformed(Malformed::Param { index: def.generics, args: count, site }).into());
        }
        let self_ty = self.fresh_generic_args(item, 0, 1)?;
        let rest = self.explicit_args(at, Some(tseg), parent.container, 1, count - 1)?;
        self.concat(self_ty, rest)
    }

    fn value_def_ty(&mut self, at: At, cause: Cause, def: Def, segs: &[Segment]) -> Result<Ty, Stop> {
        let last = segs.last().copied();
        let name = last.map(|s| s.name);
        match def {
            Def::Fn(k) => {
                let Some(sig) = self.reads.fn_sig(k)? else {
                    self.diag(DiagCode::UnresolvedPath, at, About::path(name));
                    return Ok(self.common.err);
                };
                let parent = self.parent_args(at, k, segs, sig.parent_count)?;
                let own = self.explicit_fn_args(at, last, k, &sig)?;
                let args = self.concat(parent, own)?;
                self.register_predicates(k, args, cause)?;
                self.mk(TyView::FnDef(k, args))
            }
            Def::Const(k) | Def::Static(k, _) => {
                let Some(sig) = self.reads.fn_sig(k)? else {
                    self.diag(DiagCode::UnresolvedPath, at, About::path(name));
                    return Ok(self.common.err);
                };
                let parent = self.parent_args(at, k, segs, sig.parent_count)?;
                let own = self.fresh_generic_args(k, sig.parent_count, sig.own_count)?;
                let args = self.concat(parent, own)?;
                self.register_predicates(k, args, cause)?;
                if let Def::Const(_) = def {
                    self.record_const_args(at, args);
                }
                self.lower(sig.types, sig.output, Lower::Args(args), cause)
            }
            Def::Ctor { adt, variant, kind } => {
                let Some(info) = self.reads.adt(adt)? else {
                    self.diag(DiagCode::UnresolvedPath, at, About::path(name));
                    return Ok(self.common.err);
                };
                // The type's arguments may be written on any segment (`S::<T>`, `E::<T>::V`,
                // `E::V::<T>`): the last segment that has them.
                let written = segs.iter().rev().find(|s| s.has_args).copied();
                let args = self.explicit_args(at, written, adt, 0, info.generics)?;
                let vkey = info.variants.get(variant as usize).map(|v| v.key);
                self.ctor_ty(at, adt, vkey, kind, args, name)
            }
            // A const parameter is a value of its declared type (the Reference, "Const
            // generics": "const parameters may be used ... as a standalone expression").
            Def::TyParam(i) => {
                let owner = self.body.owner;
                let own = self.reads.const_params(owner)?;
                let Some(pt) = own.type_of(i) else {
                    self.diag(DiagCode::NotAValue, at, About::path(name));
                    return Ok(self.common.err);
                };
                let empty = self.mk_list(&[])?;
                self.lower(own.types, pt, Lower::Args(empty), cause)
            }
            Def::Adt(_) | Def::Trait(_) | Def::Prim(_) | Def::SelfTy | Def::Alias(_) => {
                self.diag(DiagCode::NotAValue, at, About::path(name));
                Ok(self.common.err)
            }
        }
    }

    /// The value of a struct's or variant's ctor at the ADT's `args`: the ADT for a unit ctor,
    /// the ctor's fn item for a tuple one; a braces-only ctor is not a value (E0533's class).
    fn ctor_ty(
        &mut self,
        at: At,
        adt: ItemKey,
        variant: Option<ItemKey>,
        kind: CtorKind,
        args: TyList,
        name: Option<Name>,
    ) -> Result<Ty, Stop> {
        match kind {
            CtorKind::Unit => self.mk(TyView::Adt(adt, args)),
            CtorKind::Tuple => {
                let Some(v) = variant else {
                    self.diag(DiagCode::UnresolvedPath, at, About::path(name));
                    return Ok(self.common.err);
                };
                self.mk(TyView::FnDef(v, args))
            }
            CtorKind::Braces => {
                self.diag(DiagCode::NotAValue, at, About::path(name));
                Ok(self.common.err)
            }
        }
    }

    /// `T::name` for a value: ONE home for an associated value item named `name` of `T`.
    /// Inherent items of `T` first; an applicable inherent item ends the search (the Reference,
    /// "Method call expressions": "1. `T`'s inherent methods (methods implemented directly on
    /// `T`). 2. Any of the methods provided by a visible trait implemented by `T`", the order
    /// rustc's path probe also keeps). Then the traits in scope, the traits of `T`'s bounds when
    /// `T` is a parameter, and the trait of the impl the body is a member of (rustc_resolve
    /// counts the current impl's trait in scope: `Self::g()` in `impl Tr for E` needs no
    /// `use`). An item reached twice is one candidate: candidates are counted by item key.
    ///
    /// Before any of them: a variant of the enum `T` is (rustc_hir_typeck method/mod.rs
    /// `resolve_fully_qualified_call`, "Check if we have an enum variant", ahead of the probe;
    /// RFC 2338 `type_alias_enum_variants`, stable since 1.37: "the variants of an enum may be
    /// named through any path that resolves to the enum's type": `Alias::V`, `Self::V`,
    /// `<E>::V`, and through an associated type `Self::Error::Overflow` where `type Error =
    /// FromUintError<u8>`). An associated item of the same name is shadowed by the variant
    /// (rustc's deny-by-default `ambiguous_associated_items` refuses such a program anyway).
    fn assoc_value(&mut self, at: At, cause: Cause, self_ty: Ty, seg: Segment, scope: ScopeId) -> Result<Ty, Stop> {
        let self_ty = self.try_structurally_resolve(self_ty)?;
        if let Some(t) = self.type_relative_variant(at, self_ty, seg)? {
            return Ok(t);
        }
        // A type parameter has no inherent items (the Reference, "Implementations", "Inherent
        // implementations": the implementing type is a nominal type), so the world is not asked:
        // its answer for "any type" would list every inherent impl whose self type it could
        // not read, and one refused header there made every `T::NAME` a gap.
        let nominal = self.shape(self_ty).filter(|s| !matches!(s, Shape::Param));
        // rustc's `pick_core` for a path too (probe.rs, `Mode::Path` takes the same `pick`): an
        // inherent item unstable under a feature this crate does not turn on is left out first,
        // and taken only when neither an inherent item nor a trait's is picked without it.
        // turso's `DynVec::try_with_capacity_in(n, alloc)` (`DynVec<T> = Vec<T>`): std's
        // `#[unstable(feature = "allocator_ext", implied_by = "allocator_api")]` inherent fn is
        // left out, and the in-scope `TursoVecInExt::try_with_capacity_in` is the pick.
        let mut left_out = false;
        if let Some(shape) = nominal
            && let Some(t) = self.inherent_value(at, cause, self_ty, seg, shape, true, &mut left_out)?
        {
            return Ok(t);
        }
        // Trait candidates, in two tiers: the traits of a parameter's bounds come before the
        // rest (the Reference, "Method call expressions": "If `T` is a type parameter, methods
        // provided by trait bounds on `T` are looked up first. Then all remaining methods in
        // scope are looked up."; for a path the Reference is silent and this is rustc's order:
        // `assemble_inherent_candidates_from_param` gives inherent candidates, which `pick`
        // tries before the extension candidates). A tier with one applicable candidate ends
        // the search; with two it is E0034.
        let mut bounds: Vec<ItemKey, Alloc<'a>> = Vec::new_in(self.alloc);
        if let TyView::Param(i) = self.view(self_ty) {
            self.param_bound_traits(i, &mut bounds)?;
        }
        let mut traits: Vec<ItemKey, Alloc<'a>> = Vec::new_in(self.alloc);
        traits.extend_from_slice(self.reads.traits_in_scope(scope)?);
        if let Some(tr) = self.current_impl_trait()? {
            traits.push(tr);
        }
        for tier in [&mut bounds, &mut traits] {
            // A trait reached twice (in scope and as the impl's own, say) is one candidate.
            tier.sort_unstable();
            tier.dedup();
            let mut picked: Option<(ItemKey, ItemKey, AssocKind, TyList)> = None;
            let mut count = 0u32;
            for &tr in tier.iter() {
                let Some(item) = self.reads.assoc_item(tr, seg.name)? else { continue };
                let trait_args = self.trait_args_for(item.item, self_ty)?;
                let goal = Goal::Trait { trait_: tr, args: trait_args };
                // A candidate the world cannot decide stops here as the body's gap (`?`).
                if self.ty.may_hold(goal, &mut self.reads)? {
                    count += 1;
                    if picked.is_none() {
                        picked = Some((tr, item.item, item.kind, trait_args));
                    }
                }
            }
            if count > 1 {
                self.diag(DiagCode::AmbiguousMethod, at, About::found_named(self_ty, seg.name));
                return Ok(self.common.err);
            }
            if let Some((tr, item, kind, trait_args)) = picked {
                self.register_trait(tr, trait_args, cause)?;
                return self.assoc_item_ty(at, cause, item, kind, trait_args, Some(seg));
            }
        }
        // The second pass ("consider unstable candidates if not found"): only the inherent
        // items change, and only when the first left one out.
        if left_out
            && let Some(shape) = nominal
            && let Some(t) = self.inherent_value(at, cause, self_ty, seg, shape, false, &mut left_out)?
        {
            return Ok(t);
        }
        self.diag(DiagCode::NoMethod, at, About::found_named(self_ty, seg.name));
        Ok(self.common.err)
    }

    /// `T::name`'s inherent items (of the nominal `shape`): the one that applies, its type;
    /// two is E0034 (the error, then `Some`); none, `None`. With `stable_only`, an applicable
    /// item unstable under a feature this crate does not turn on ([`World::member_unstable`])
    /// is not counted, and `left_out` is set (rustc's first pass, `consider_candidates`).
    #[allow(clippy::too_many_arguments)]
    fn inherent_value(
        &mut self,
        at: At,
        cause: Cause,
        self_ty: Ty,
        seg: Segment,
        shape: Shape,
        stable_only: bool,
        left_out: &mut bool,
    ) -> Result<Option<Ty>, Stop> {
        let cands = self.reads.inherent(shape, seg.name)?;
        let mut found: Option<(ItemKey, ItemKey, AssocKind)> = None;
        let mut count = 0u32;
        // An item listed twice counts once (a handful of candidates: the scan is short).
        for (i, cand) in cands.iter().enumerate() {
            if cands[..i].iter().any(|c| c.item == cand.item) {
                continue;
            }
            if self.impl_applies(cand.container, self_ty)? {
                if stable_only && self.reads.member_unstable(cand.container, seg.name)? {
                    *left_out = true;
                    continue;
                }
                count += 1;
                if found.is_none() {
                    found = Some((cand.container, cand.item, cand.kind));
                }
            }
        }
        if count > 1 {
            self.diag(DiagCode::AmbiguousMethod, at, About::found_named(self_ty, seg.name));
            return Ok(Some(self.common.err));
        }
        let Some((imp, item, kind)) = found else { return Ok(None) };
        let Some(impl_args) = self.impl_args(imp, self_ty, at.cause())? else {
            self.diag(DiagCode::NoMethod, at, About::found_named(self_ty, seg.name));
            return Ok(Some(self.common.err));
        };
        self.assoc_item_ty(at, cause, item, kind, impl_args, Some(seg)).map(Some)
    }

    /// The variant `seg` of the enum `self_ty` is, as a value at the enum's own arguments
    /// (its ctor: the enum for a unit variant, the variant's fn item for a tuple one, E0533's
    /// class for a braced one), recorded at its node; `None` when `self_ty` is no enum or has
    /// no variant of that name. Generic arguments on the variant's segment after a type that
    /// already gave the enum's are rustc's E0109 ("type arguments are not allowed on this
    /// type"), for which the checker has no code: refused by name.
    fn type_relative_variant(&mut self, at: At, self_ty: Ty, seg: Segment) -> Result<Option<Ty>, Stop> {
        let TyView::Adt(k, args) = self.view(self_ty) else { return Ok(None) };
        let Some(adt) = self.reads.adt(k)? else { return Ok(None) };
        if adt.kind != crate::world::AdtKind::Enum {
            return Ok(None);
        }
        let Some((i, v)) = adt.variants.iter().enumerate().find(|(_, v)| v.name == seg.name) else { return Ok(None) };
        if seg.has_args {
            return Err(CheckError::Unported("generic arguments on a variant named through a type-relative path (E0109)").into());
        }
        let (vkey, ctor) = (v.key, v.ctor);
        let def = Def::Ctor { adt: k, variant: i as u32, kind: ctor };
        let t = self.ctor_ty(at, k, Some(vkey), ctor, args, Some(seg.name))?;
        if !self.is_err(t) {
            self.defs.push((at, def));
        }
        Ok(Some(t))
    }

    /// The traits of the body's where-clauses whose self type is parameter `i`, elaborated
    /// through their supertraits (rustc's `assemble_inherent_candidates_from_param` reads the
    /// elaborated param env: `T: Sub` with `trait Sub: Base` gives `Base`'s methods too).
    pub(crate) fn param_bound_traits(&mut self, i: u32, out: &mut Vec<ItemKey, Alloc<'a>>) -> Result<(), Stop> {
        let owner = self.body.owner;
        let start = out.len();
        let preds = self.reads.predicates(owner)?;
        for clause in preds.clauses {
            if let crate::world::PClause::Trait { trait_, args } = *clause {
                let list = preds.types.list(args)?;
                if let Some(&first) = list.first() {
                    if preds.types.node(first)? == crate::portable::PNode::Param(i) && !out.contains(&trait_) {
                        out.push(trait_);
                    }
                }
            }
        }
        self.elaborate_bound_traits(out, start)
    }

    /// `out[start..]`'s traits, each followed by its supertraits not yet in `out`, transitively
    /// (a parameter's bounds, an object's principal).
    pub(crate) fn elaborate_bound_traits(&mut self, out: &mut Vec<ItemKey, Alloc<'a>>, start: usize) -> Result<(), Stop> {
        // The worklist is `out` itself from `start`: each distinct trait is pushed once, so it
        // ends after the world's traits at most.
        let mut at = start;
        while at < out.len() {
            let t = out[at];
            at += 1;
            let Some(def) = self.reads.trait_def(t)? else { continue };
            for clause in def.supertraits {
                if let crate::world::PClause::Trait { trait_, args } = *clause {
                    let list = def.types.list(args)?;
                    if let Some(&first) = list.first() {
                        if def.types.node(first)? == crate::portable::PNode::Param(0) && !out.contains(&trait_) {
                            out.push(trait_);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// `[self_ty, fresh..]`: a trait's arguments for an item of it, `Self` first.
    pub(crate) fn trait_args_for(&mut self, item: ItemKey, self_ty: Ty) -> Result<TyList, Stop> {
        let parent = match self.reads.fn_sig(item)? {
            Some(sig) => sig.parent_count,
            None => 1,
        };
        let rest = self.fresh_generic_args(item, 1, parent.saturating_sub(1))?;
        let self_list = self.mk_list(&[self_ty])?;
        self.concat(self_list, rest)
    }

    /// Whether `imp`'s self type can be `self_ty`, in a probe.
    pub(crate) fn impl_applies(&mut self, imp: ItemKey, self_ty: Ty) -> Result<bool, Stop> {
        // A probe: rolled back, so its obligations' cause reaches no diagnostic.
        Ok(self.probe(|m| m.impl_args(imp, self_ty, Cause::Body))?.is_some())
    }

    /// The value type of an associated item given its parent's arguments.
    fn assoc_item_ty(
        &mut self,
        at: At,
        cause: Cause,
        item: ItemKey,
        kind: AssocKind,
        parent_args: TyList,
        seg: Option<Segment>,
    ) -> Result<Ty, Stop> {
        let Some(sig) = self.reads.fn_sig(item)? else {
            self.diag(DiagCode::UnresolvedPath, at, About::path(seg.map(|s| s.name)));
            return Ok(self.common.err);
        };
        match kind {
            AssocKind::Fn { .. } => {
                let own = self.explicit_fn_args(at, seg, item, &sig)?;
                let args = self.concat(parent_args, own)?;
                self.register_predicates(item, args, cause)?;
                self.defs.push((at, Def::Fn(item)));
                self.mk(TyView::FnDef(item, args))
            }
            AssocKind::Const => {
                self.register_predicates(item, parent_args, cause)?;
                self.defs.push((at, Def::Const(item)));
                self.record_const_args(at, parent_args);
                self.lower(sig.types, sig.output, Lower::Args(parent_args), cause)
            }
            AssocKind::Type => {
                self.diag(DiagCode::NotAValue, at, About::path(seg.map(|s| s.name)));
                Ok(self.common.err)
            }
        }
    }
}
