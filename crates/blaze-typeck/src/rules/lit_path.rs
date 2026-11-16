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

use blaze_own::Vec;
use core::alloc::Allocator;

use crate::body::{IntSuffix, Lit, Segment};
use crate::diag::{At, DiagCode};
use crate::error::Stop;
use crate::ids::{ExprId, ItemKey, Mutbl, Name, PathIdx, ScopeId};
use crate::machine::{Expectation, Flow, Frame, Lower, Machine, Then};
use crate::ty::{Cause, Goal, Ty, TyList, TyView, TypeCtx};
use crate::world::{AssocKind, CtorKind, Def, World};

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// `check_lit`.
    pub(crate) fn expr_lit(&mut self, f: Frame, lit: Lit) -> Result<Flow, Stop> {
        Ok(Flow::Done(self.lit_ty(lit, f.expect)?))
    }

    /// A literal's type under an expectation.
    pub(crate) fn lit_ty(&mut self, lit: Lit, expect: Expectation) -> Result<Ty, Stop> {
        let c = self.common;
        Ok(match lit {
            Lit::Bool(_) => c.bool_,
            Lit::Char => c.char_,
            Lit::Str => c.static_str,
            Lit::ByteStr(n) => {
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
                        TyView::Ptr(..) | TyView::FnDef(..) | TyView::FnPtr(_) => Some(c.usize_),
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

    pub(crate) fn expr_path(&mut self, e: ExprId, path: PathIdx) -> Result<Flow, Stop> {
        let ty = self.value_path(At::Expr(e), Cause::Expr(e), path)?;
        Ok(Flow::Done(ty))
    }

    /// Explicit generic arguments of a segment (`::<..>`), or `count` fresh variables. A wrong
    /// count is E0107, and the arguments are then fresh.
    pub(crate) fn explicit_args(&mut self, at: At, seg: Option<Segment>, count: u32) -> Result<TyList, Stop> {
        let Some(seg) = seg.filter(|s| s.has_args) else {
            return self.fresh_args(count);
        };
        let written = self.body.type_list(seg.generic_args)?;
        if written.len() != count as usize {
            self.diag(DiagCode::GenericArgCount, at, None, None, Some(seg.name));
            return self.fresh_args(count);
        }
        let mut v: Vec<Ty, A> = Vec::with_capacity_in(written.len(), self.alloc.clone());
        for &t in written {
            v.push(self.lower_body_ty(t)?);
        }
        self.mk_list(&v)
    }

    /// `parent` followed by `own`.
    pub(crate) fn concat(&mut self, parent: TyList, own: TyList) -> Result<TyList, Stop> {
        let mut v = self.list(parent);
        let own = self.list(own);
        v.extend_from_slice(&own);
        self.mk_list(&v)
    }

    /// The type of a value path (`instantiate_value_path`).
    pub(crate) fn value_path(&mut self, at: At, cause: Cause, path: PathIdx) -> Result<Ty, Stop> {
        let bp = self.body.path(path)?;
        let segs = self.body.segment_list(bp.segments)?;
        let mut names: Vec<Name, A> = Vec::with_capacity_in(segs.len(), self.alloc.clone());
        for s in segs {
            names.push(s.name);
        }
        let last = segs.last().copied();
        let last_name = last.map(|s| s.name);
        let Some(res) = self.reads.resolve_path(bp.scope, path, &names)? else {
            self.diag(DiagCode::UnresolvedPath, at, None, None, last_name);
            return Ok(self.common.err);
        };
        let consumed = res.consumed as usize;
        if consumed == segs.len() {
            return self.value_def(at, cause, res.def, segs);
        }
        if consumed + 1 != segs.len() || consumed == 0 {
            self.diag(DiagCode::UnresolvedPath, at, None, None, last_name);
            return Ok(self.common.err);
        }
        // Type-relative: the consumed prefix names a type.
        let self_ty = match res.def {
            Def::Adt(k) => {
                let Some(adt) = self.reads.adt(k)? else {
                    self.diag(DiagCode::UnresolvedPath, at, None, None, last_name);
                    return Ok(self.common.err);
                };
                let args = self.explicit_args(at, Some(segs[consumed - 1]), adt.generics)?;
                self.mk(TyView::Adt(k, args))?
            }
            Def::TyParam(i) => self.mk(TyView::Param(i))?,
            _ => {
                self.diag(DiagCode::UnresolvedPath, at, None, None, last_name);
                return Ok(self.common.err);
            }
        };
        let seg = segs[consumed];
        self.assoc_value(at, cause, self_ty, seg, bp.scope)
    }

    /// A fully resolved value definition.
    fn value_def(&mut self, at: At, cause: Cause, def: Def, segs: &[Segment]) -> Result<Ty, Stop> {
        let last = segs.last().copied();
        let name = last.map(|s| s.name);
        match def {
            Def::Fn(k) => {
                let Some(sig) = self.reads.fn_sig(k)? else {
                    self.diag(DiagCode::UnresolvedPath, at, None, None, name);
                    return Ok(self.common.err);
                };
                let parent = self.fresh_args(sig.parent_count)?;
                let own = self.explicit_args(at, last, sig.own_count)?;
                let args = self.concat(parent, own)?;
                self.register_predicates(k, args, cause)?;
                self.mk(TyView::FnDef(k, args))
            }
            Def::Const(k) | Def::Static(k, _) => {
                let Some(sig) = self.reads.fn_sig(k)? else {
                    self.diag(DiagCode::UnresolvedPath, at, None, None, name);
                    return Ok(self.common.err);
                };
                let args = self.fresh_args(sig.parent_count + sig.own_count)?;
                self.register_predicates(k, args, cause)?;
                self.lower(sig.types, sig.output, Lower::Args(args))
            }
            Def::Ctor { adt, variant, kind } => {
                let Some(info) = self.reads.adt(adt)? else {
                    self.diag(DiagCode::UnresolvedPath, at, None, None, name);
                    return Ok(self.common.err);
                };
                // The type's arguments may be written on any segment (`S::<T>`, `E::<T>::V`,
                // `E::V::<T>`): the last segment that has them.
                let written = segs.iter().rev().find(|s| s.has_args).copied();
                let args = self.explicit_args(at, written, info.generics)?;
                match kind {
                    CtorKind::Unit => self.mk(TyView::Adt(adt, args)),
                    CtorKind::Tuple => {
                        let Some(v) = info.variants.get(variant as usize) else {
                            self.diag(DiagCode::UnresolvedPath, at, None, None, name);
                            return Ok(self.common.err);
                        };
                        self.mk(TyView::FnDef(v.key, args))
                    }
                    CtorKind::Braces => {
                        self.diag(DiagCode::NotAValue, at, None, None, name);
                        Ok(self.common.err)
                    }
                }
            }
            Def::Adt(_) | Def::Trait(_) | Def::TyParam(_) => {
                self.diag(DiagCode::NotAValue, at, None, None, name);
                Ok(self.common.err)
            }
        }
    }

    /// `T::name` for a value: inherent items of `T` first, then the traits in scope (and the
    /// traits of `T`'s bounds when `T` is a parameter).
    fn assoc_value(&mut self, at: At, cause: Cause, self_ty: Ty, seg: Segment, scope: ScopeId) -> Result<Ty, Stop> {
        if let Some(shape) = self.shape(self_ty) {
            let cands = self.reads.inherent(shape, seg.name)?;
            let mut found: Option<(ItemKey, ItemKey, AssocKind)> = None;
            let mut count = 0u32;
            for cand in cands {
                if self.impl_applies(cand.container, self_ty)? {
                    count += 1;
                    if found.is_none() {
                        found = Some((cand.container, cand.item, cand.kind));
                    }
                }
            }
            if count > 1 {
                self.diag(DiagCode::AmbiguousMethod, at, None, Some(self_ty), Some(seg.name));
                return Ok(self.common.err);
            }
            if let Some((imp, item, kind)) = found {
                let Some(impl_args) = self.impl_args(imp, self_ty)? else {
                    self.diag(DiagCode::NoMethod, at, None, Some(self_ty), Some(seg.name));
                    return Ok(self.common.err);
                };
                return self.assoc_item_ty(at, cause, item, kind, impl_args, Some(seg));
            }
        }
        // Trait candidates.
        let mut traits: Vec<ItemKey, A> = Vec::new_in(self.alloc.clone());
        traits.extend_from_slice(self.reads.traits_in_scope(scope)?);
        if let TyView::Param(i) = self.view(self_ty) {
            self.param_bound_traits(i, &mut traits)?;
        }
        let mut picked: Option<(ItemKey, ItemKey, AssocKind, TyList)> = None;
        let mut count = 0u32;
        for &tr in traits.iter() {
            let Some(item) = self.reads.assoc_item(tr, seg.name)? else { continue };
            let trait_args = self.trait_args_for(item.item, self_ty)?;
            let goal = Goal::Trait { trait_: tr, args: trait_args };
            if self.ty.may_hold(goal, &mut self.reads)? {
                count += 1;
                if picked.is_none() {
                    picked = Some((tr, item.item, item.kind, trait_args));
                }
            }
        }
        if count > 1 {
            self.diag(DiagCode::AmbiguousMethod, at, None, Some(self_ty), Some(seg.name));
            return Ok(self.common.err);
        }
        match picked {
            Some((tr, item, kind, trait_args)) => {
                self.register_trait(tr, trait_args, cause)?;
                self.assoc_item_ty(at, cause, item, kind, trait_args, Some(seg))
            }
            None => {
                self.diag(DiagCode::NoMethod, at, None, Some(self_ty), Some(seg.name));
                Ok(self.common.err)
            }
        }
    }

    /// The traits of the body's where-clauses whose self type is parameter `i`.
    pub(crate) fn param_bound_traits(&mut self, i: u32, out: &mut Vec<ItemKey, A>) -> Result<(), Stop> {
        let owner = self.body.owner;
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
        Ok(())
    }

    /// `[self_ty, fresh..]`: a trait's arguments for an item of it, `Self` first.
    pub(crate) fn trait_args_for(&mut self, item: ItemKey, self_ty: Ty) -> Result<TyList, Stop> {
        let parent = match self.reads.fn_sig(item)? {
            Some(sig) => sig.parent_count,
            None => 1,
        };
        let mut v: Vec<Ty, A> = Vec::with_capacity_in(parent.max(1) as usize, self.alloc.clone());
        v.push(self_ty);
        for _ in 1..parent {
            v.push(self.fresh()?);
        }
        self.mk_list(&v)
    }

    /// Whether `imp`'s self type can be `self_ty`, in a probe.
    pub(crate) fn impl_applies(&mut self, imp: ItemKey, self_ty: Ty) -> Result<bool, Stop> {
        Ok(self.probe(|m| m.impl_args(imp, self_ty))?.is_some())
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
            self.diag(DiagCode::UnresolvedPath, at, None, None, seg.map(|s| s.name));
            return Ok(self.common.err);
        };
        match kind {
            AssocKind::Fn { .. } => {
                let own = self.explicit_args(at, seg, sig.own_count)?;
                let args = self.concat(parent_args, own)?;
                self.register_predicates(item, args, cause)?;
                self.mk(TyView::FnDef(item, args))
            }
            AssocKind::Const => {
                self.register_predicates(item, parent_args, cause)?;
                self.lower(sig.types, sig.output, Lower::Args(parent_args))
            }
            AssocKind::Type => {
                self.diag(DiagCode::NotAValue, at, None, None, seg.map(|s| s.name));
                Ok(self.common.err)
            }
        }
    }
}
