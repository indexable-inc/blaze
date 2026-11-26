//! Constant promotion (ASYNC.md D-A10, P10): which shared borrows of a fn body are borrows of a
//! `'static` slot rather than of a frame temporary.
//!
//! The Reference, "Destructors", "Constant promotion": "Promotion of a value expression to a
//! `'static` slot occurs when the expression could be written in a constant and borrowed, and
//! that borrow could be dereferenced where the expression was originally written, without
//! changing the runtime behavior. That is, the promoted expression can be evaluated at
//! compile-time and the resulting value does not contain interior mutability or destructors
//! (these properties are determined based on the value where possible, e.g. `&None` always has
//! the type `&'static Option<_>`, as it contains nothing disallowed)."
//!
//! The library leans on it: core/src/task/wake.rs marks `RawWakerVTable::new`
//! `#[rustc_promotable]`, and tokio's `fn waker_vtable<W: Wake>() -> &'static RawWakerVTable {
//! &RawWakerVTable::new(clone_arc_raw::<W>, ..) }` returns such a borrow. Without promotion the
//! borrow points into the returning fn's frame: a dangling vtable, silently wrong code.
//!
//! The rule as decided here, after the walk (the expression tree and the solver are the
//! checker's): an `Expr::Ref { mutbl: Not, operand }` is listed in `Checked::promoted` when
//! - its operand is promotable: a literal; a path to a `const`, a fn item or a unit or tuple
//!   ctor; a tuple, array or struct literal (no `..base`) of promotable operands; a call of a
//!   fn item whose attributes carry `#[rustc_promotable]` ([`TypeAttrs::PROMOTABLE`]), or of a
//!   tuple ctor, with promotable arguments; a direct field or tuple-field projection of a
//!   promotable non-union `Freeze`-and-no-drop base (no `Deref` adjustment, e.g. hashbrown's
//!   `&ALIGNED_BYTES.bytes`); parentheses around one of these;
//! - and the operand's type is `Freeze` with no drop glue, decided structurally where the type
//!   says so (scalars, `&T`, raw and fn pointers, fn items, tuples and arrays of such) and by
//!   the lang `freeze` trait (`may_hold`, an auto trait: core/src/marker.rs `pub unsafe auto
//!   trait Freeze {}`, `impl<T: PointeeSized> !Freeze for UnsafeCell<T> {}`) and the ADT's
//!   fields for an ADT.
//!
//! "Where possible" is narrowed, never widened: a type this rule cannot decide (a parameter, a
//! projection, an opaque, a closure, a future, a trait object, a variable) is not listed, and
//! nor is an ADT when the world has no `freeze` lang item. A borrow not listed is a frame
//! temporary, as before this rule. A borrow inside an inline `const { .. }` is never listed:
//! the block is consteval's and its borrows are part of its value.

use blaze_own::{Alloc, Vec};

use crate::body::Expr;
use crate::columns::AdjustKind;
use crate::diag::At;
use crate::error::{CheckError, Limit, Stop};
use crate::ids::{ExprId, ItemKey, LangItem, Mutbl, PatId};
use crate::machine::{Lower, Machine};
use crate::portable::PNode;
use crate::ty::{Cause, Goal, Ty, TyView, TypeCtx};
use crate::world::{AdtKind, CtorKind, Def, ImplPolarity, Shape, TypeAttrs, World};

/// What a promotable call expression calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromotableCallee {
    /// A tuple struct or enum tuple-variant constructor (`Def::Ctor { kind: CtorKind::Tuple }`).
    TupleCtor(ItemKey),
    /// A fn item carrying `#[rustc_promotable]` (`TypeAttrs::PROMOTABLE`).
    PromotableFn,
    /// Not a promotable callee.
    No,
}

/// The most types one `Freeze`-and-no-drop decision visits.
const MOST_TYPE_STEPS: usize = 1 << 14;

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// Lists every promoted borrow of the body in `self.promoted`, sorted by expression (the
    /// module doc). Called in the end sequence after `analyze_futures`, when every type the
    /// rule reads is as resolved as the walk makes it.
    pub(crate) fn analyze_promotions(&mut self) -> Result<(), Stop> {
        self.promoted.clear();
        let bound = self.body.exprs.len() + self.body.pats.len() + self.body.stmts.len() + 1;
        // Each expression's first adjustment (O(exprs + adjusts)): a leading shared non-raw
        // autoref (`AdjustKind::Borrow { mutbl: Mutbl::Not, raw: false, .. }` with no leading
        // derefs, e.g. `EthereumHardfork::Frontier.boxed()` or `[1, 2, 3].as_slice()`) borrows
        // the expression's own rvalue, and is promoted by the same rule as `&operand`.
        let mut first_adjust: Vec<Option<AdjustKind>, Alloc<'a>> = Vec::with_capacity_in(self.body.exprs.len(), self.alloc);
        first_adjust.resize(self.body.exprs.len(), None);
        let mut has_deref: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(self.body.exprs.len(), self.alloc);
        has_deref.resize(self.body.exprs.len(), false);
        for (&e, &k) in self.adjust.column::<0>().iter().zip(self.adjust.column::<1>().iter()) {
            if let Some(slot) = first_adjust.get_mut(e.ix())
                && slot.is_none()
            {
                *slot = Some(k);
            }
            if matches!(k, AdjustKind::Deref { .. })
                && let Some(slot) = has_deref.get_mut(e.ix())
            {
                *slot = true;
            }
        }
        // The whole tree from the root, each expression with whether it is under a const block.
        let mut work: Vec<(ExprId, bool), Alloc<'a>> = Vec::new_in(self.alloc);
        let mut kids: Vec<ExprId, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut pats: Vec<PatId, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut found: Vec<ExprId, Alloc<'a>> = Vec::new_in(self.alloc);
        work.push((self.body.root, false));
        let mut steps = 0usize;
        while let Some((x, in_const)) = work.pop() {
            steps += 1;
            if steps > bound {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            let node = self.body.expr(x)?;
            let under = in_const || matches!(node, Expr::ConstBlock { .. });
            if !in_const {
                match node {
                    Expr::Ref { mutbl: Mutbl::Not, operand } => {
                        if self.promotable_value(operand, &has_deref)? {
                            found.push(x);
                        }
                    }
                    Expr::Ref { mutbl: Mutbl::Mut, .. } => {}
                    _ => {
                        if matches!(
                            first_adjust.get(x.ix()).copied().flatten(),
                            Some(AdjustKind::Borrow { mutbl: Mutbl::Not, raw: false, .. })
                        ) && self.expr_ty.get(x.ix()).is_some()
                            && self.promotable_value(x, &has_deref)?
                        {
                            found.push(x);
                        }
                    }
                }
            }
            kids.clear();
            pats.clear();
            self.expr_children(x, &mut kids, &mut pats)?;
            for &k in kids.iter() {
                work.push((k, under));
            }
        }
        found.sort_unstable_by_key(|e| e.0);
        found.dedup();
        self.promoted.extend_from_slice(&found);
        Ok(())
    }

    /// What value path or ctor `x` resolved to, if the walk recorded it.
    fn promote_def(&self, x: ExprId) -> Option<Def> {
        self.defs.iter().rev().find(|(at, _)| matches!(at, At::Expr(y) if *y == x)).map(|&(_, d)| d)
    }

    /// Whether `op` is a promotable operand whose constructed value holds no interior mutability
    /// and needs no drop (the Reference, "Constant promotion": value-based where possible, e.g.
    /// `&[CrateType::Bin]` or `&None::<String>` when another variant has `Drop` or `!Freeze`,
    /// falling back to type-based [`Self::freeze_without_drop`] for opaque subexpressions such as
    /// `const` paths, `#[rustc_promotable]` calls, field projections, and unions).
    fn promotable_value(&mut self, op: ExprId, has_deref: &[bool]) -> Result<bool, Stop> {
        let freeze = self.lang(LangItem::Freeze)?;
        let manually_drop = self.lang(LangItem::ManuallyDrop)?;
        let drop_trait = self.lang(LangItem::Drop)?;
        let bound = self.body.exprs.len() + 1;
        let mut work: Vec<ExprId, Alloc<'a>> = Vec::new_in(self.alloc);
        work.push(op);
        let mut steps = 0usize;
        while let Some(x) = work.pop() {
            steps += 1;
            if steps > bound {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            match self.body.expr(x)? {
                Expr::Lit(_) => {
                    let Some(&ty) = self.expr_ty.get(x.ix()) else {
                        return Ok(false);
                    };
                    let t = self.ty.resolve(ty)?;
                    if !matches!(
                        self.view(t),
                        TyView::Bool
                            | TyView::Char
                            | TyView::Int(_)
                            | TyView::Uint(_)
                            | TyView::Float(_)
                            | TyView::Str
                            | TyView::Ref(Mutbl::Not, _)
                    ) {
                        return Ok(false);
                    }
                }
                Expr::Paren(i) => work.push(i),
                Expr::Path(_) => match self.promote_def(x) {
                    Some(Def::Fn(_)) | Some(Def::Ctor { kind: CtorKind::Tuple, .. }) => {}
                    Some(Def::Const(_)) => {
                        let Some(&ty) = self.expr_ty.get(x.ix()) else {
                            return Ok(false);
                        };
                        if !self.freeze_without_drop(ty)? {
                            return Ok(false);
                        }
                    }
                    Some(Def::Ctor { adt: k, kind: CtorKind::Unit, .. }) => {
                        let Some(freeze_tr) = freeze else {
                            return Ok(false);
                        };
                        let Some(&ty) = self.expr_ty.get(x.ix()) else {
                            return Ok(false);
                        };
                        let t = self.ty.resolve(ty)?;
                        if !matches!(self.view(t), TyView::Adt(ak, _) if ak == k) {
                            return Ok(false);
                        }
                        if self.adt_has_explicit_drop(drop_trait, manually_drop, k)?
                            || self.adt_has_negative_freeze(freeze_tr, k)?
                        {
                            return Ok(false);
                        }
                    }
                    _ => return Ok(false),
                },
                Expr::Field { base, .. } | Expr::TupleField { base, .. } => {
                    if has_deref.get(base.ix()).copied().unwrap_or(false) {
                        return Ok(false);
                    }
                    let Some(&bt) = self.expr_ty.get(base.ix()) else {
                        return Ok(false);
                    };
                    let bt_res = self.ty.resolve(bt)?;
                    if let TyView::Adt(k, _) = self.view(bt_res)
                        && self.reads.adt(k)?.is_none_or(|adt| adt.kind == AdtKind::Union)
                    {
                        return Ok(false);
                    }
                    if !self.freeze_without_drop(bt_res)? {
                        return Ok(false);
                    }
                    work.push(base);
                }
                Expr::Tuple { elems } | Expr::Array { elems } => work.extend_from_slice(self.body.expr_list(elems)?),
                Expr::Repeat { elem, .. } => work.push(elem),
                Expr::Struct { fields, base: crate::body::StructBase::None, .. } => {
                    let Some(freeze_tr) = freeze else {
                        return Ok(false);
                    };
                    let Some(&ty) = self.expr_ty.get(x.ix()) else {
                        return Ok(false);
                    };
                    let t = self.ty.resolve(ty)?;
                    let TyView::Adt(k, _) = self.view(t) else {
                        return Ok(false);
                    };
                    let Some(adt) = self.reads.adt(k)? else {
                        return Ok(false);
                    };
                    let inits = self.body.field_init_list(fields)?;
                    if adt.kind == AdtKind::Union {
                        if !self.freeze_without_drop(t)? {
                            return Ok(false);
                        }
                    } else {
                        if self.adt_has_explicit_drop(drop_trait, manually_drop, k)?
                            || self.adt_has_negative_freeze(freeze_tr, k)?
                        {
                            return Ok(false);
                        }
                        let variant = match adt.kind {
                            AdtKind::Struct => 0u32,
                            AdtKind::Enum => match self.promote_def(x) {
                                Some(Def::Ctor { adt: ak, variant, .. }) if ak == k => variant,
                                _ => return Ok(false),
                            },
                            AdtKind::Union => unreachable!(),
                        };
                        let Some(v) = adt.variants.get(variant as usize) else {
                            return Ok(false);
                        };
                        if inits.len() < v.fields.len as usize && !self.freeze_without_drop(t)? {
                            return Ok(false);
                        }
                    }
                    for f in inits {
                        work.push(f.expr);
                    }
                }
                Expr::Call { callee, args } => {
                    match self.promotable_callee(callee)? {
                        PromotableCallee::TupleCtor(k) => {
                            let Some(freeze_tr) = freeze else {
                                return Ok(false);
                            };
                            let Some(&ty) = self.expr_ty.get(x.ix()) else {
                                return Ok(false);
                            };
                            let t = self.ty.resolve(ty)?;
                            if !matches!(self.view(t), TyView::Adt(ak, _) if ak == k) {
                                return Ok(false);
                            }
                            if self.adt_has_explicit_drop(drop_trait, manually_drop, k)?
                                || self.adt_has_negative_freeze(freeze_tr, k)?
                            {
                                return Ok(false);
                            }
                        }
                        PromotableCallee::PromotableFn => {
                            let Some(&ty) = self.expr_ty.get(x.ix()) else {
                                return Ok(false);
                            };
                            if !self.freeze_without_drop(ty)? {
                                return Ok(false);
                            }
                        }
                        PromotableCallee::No => return Ok(false),
                    }
                    work.extend_from_slice(self.body.expr_list(args)?);
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Whether `callee` is a path to a tuple ctor or to a fn item carrying
    /// `#[rustc_promotable]`.
    fn promotable_callee(&mut self, callee: ExprId) -> Result<PromotableCallee, Stop> {
        let mut c = callee;
        while let Expr::Paren(i) = self.body.expr(c)? {
            c = i;
        }
        if !matches!(self.body.expr(c)?, Expr::Path(_)) {
            return Ok(PromotableCallee::No);
        }
        if let Some(Def::Ctor { adt, kind: CtorKind::Tuple, .. }) = self.promote_def(c) {
            return Ok(PromotableCallee::TupleCtor(adt));
        }
        let t = self.ty.resolve(self.expr_ty[c.ix()])?;
        let TyView::FnDef(item, _) = self.view(t) else { return Ok(PromotableCallee::No) };
        let attrs = self.reads.attrs(item)?;
        Ok(if attrs.0 & TypeAttrs::PROMOTABLE != 0 {
            PromotableCallee::PromotableFn
        } else {
            PromotableCallee::No
        })
    }

    /// Whether ADT `k` itself has an explicit `impl Drop` (E0366/E0367: a `Drop` impl applies to
    /// every instance of `k`, independent of variant or generic arguments). Conservatively returns
    /// `true` if a candidate impl header is unreadable.
    fn adt_has_explicit_drop(
        &mut self,
        drop_trait: Option<ItemKey>,
        manually_drop: Option<ItemKey>,
        k: ItemKey,
    ) -> Result<bool, Stop> {
        if manually_drop == Some(k) {
            return Ok(false);
        }
        let Some(d) = drop_trait else {
            return Ok(false);
        };
        let impls = self.reads.impls_of(d, Shape::Adt(k))?;
        for &imp in impls {
            let Some(h) = self.reads.impl_header(imp)? else {
                return Ok(true);
            };
            if h.trait_ != Some(d) || h.polarity != ImplPolarity::Positive {
                continue;
            }
            match h.types.node(h.self_ty)? {
                PNode::Adt(hk, _) if hk == k => return Ok(true),
                PNode::Param(_) => return Ok(true),
                _ => {}
            }
        }
        Ok(false)
    }

    /// Whether ADT `k` itself has a negative (or non-positive) `impl !Freeze` (such as
    /// `UnsafeCell<T>`). Conservatively returns `true` if a candidate impl header is unreadable.
    fn adt_has_negative_freeze(&mut self, freeze: ItemKey, k: ItemKey) -> Result<bool, Stop> {
        let impls = self.reads.impls_of(freeze, Shape::Adt(k))?;
        for &imp in impls {
            let Some(h) = self.reads.impl_header(imp)? else {
                return Ok(true);
            };
            if h.trait_ != Some(freeze) || h.polarity == ImplPolarity::Positive {
                continue;
            }
            match h.types.node(h.self_ty)? {
                PNode::Adt(hk, _) if hk == k => return Ok(true),
                PNode::Param(_) => return Ok(true),
                _ => {}
            }
        }
        Ok(false)
    }

    /// Whether a value of `t` holds no interior mutability and needs no drop (the module
    /// doc). `false` for whatever this rule cannot decide.
    fn freeze_without_drop(&mut self, t: Ty) -> Result<bool, Stop> {
        let freeze = self.lang(LangItem::Freeze)?;
        let manually_drop = self.lang(LangItem::ManuallyDrop)?;
        let drop_trait = self.lang(LangItem::Drop)?;
        let mut work: Vec<Ty, Alloc<'a>> = Vec::new_in(self.alloc);
        work.push(t);
        let mut steps = 0usize;
        while let Some(t) = work.pop() {
            steps += 1;
            if steps > MOST_TYPE_STEPS {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            let t = self.ty.resolve(t)?;
            match self.view(t) {
                TyView::Bool
                | TyView::Char
                | TyView::Int(_)
                | TyView::Uint(_)
                | TyView::Float(_)
                | TyView::Str
                | TyView::Never
                | TyView::FnDef(..)
                | TyView::FnPtr(..)
                | TyView::Ptr(..)
                | TyView::Ref(Mutbl::Not, _) => {}
                TyView::Tuple(l) | TyView::Record(_, l) | TyView::Union(l) => {
                    for i in 0..self.ty.list_len(l) {
                        work.push(self.ty.list_get(l, i));
                    }
                }
                TyView::Array(elem, _) | TyView::Slice(elem) | TyView::Pat(elem, _) => work.push(elem),
                TyView::Adt(k, args) => {
                    let Some(freeze) = freeze else { return Ok(false) };
                    let one = self.mk_list(&[t])?;
                    if !self.ty.may_hold(Goal::Trait { trait_: freeze, args: one }, &mut self.reads)? {
                        return Ok(false);
                    }
                    // core/src/mem/manually_drop.rs: "`ManuallyDrop<T>` ... inhibit[s] the
                    // compiler from automatically calling `T`'s destructor".
                    if manually_drop == Some(k) {
                        continue;
                    }
                    let Some(adt) = self.reads.adt(k)? else { return Ok(false) };
                    if let Some(d) = drop_trait
                        && self.ty.may_hold(Goal::Trait { trait_: d, args: one }, &mut self.reads)?
                    {
                        return Ok(false);
                    }
                    // The Reference, "Unions": "Unions have no notion of an active field ... no
                    // drop glue"; a union's fields are `Copy` or `ManuallyDrop` (an explicit
                    // `impl Drop for MyUnion` was already rejected above).
                    if adt.kind == AdtKind::Union {
                        continue;
                    }
                    for f in adt.fields.iter() {
                        let ft = self.lower(adt.types, f.ty, Lower::Args(args), Cause::Body)?;
                        work.push(ft);
                    }
                }
                // `&mut T`, parameters, projections, opaques, closures, futures, trait
                // objects, variables and the error type: not decided here, so not promoted.
                _ => return Ok(false),
            }
        }
        Ok(true)
    }
}
