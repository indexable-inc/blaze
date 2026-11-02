//! `invalid_reference_casting` (deny): a `&T` cast to a `*mut T` that is then borrowed
//! mutably (`&mut *(r as *const T as *mut T)`) or assigned through (`*p = ..`, `+=`,
//! `ptr::write`) where `T` is not covered by `UnsafeCell`s, and a reference cast to a pointer
//! to a type bigger than the allocation it comes from, following the `let`s the pointer was
//! bound by.
//!
//! Follows rustc's `compiler/rustc_lint/src/reference_casting.rs`, `compiler/rustc_lint/src/
//! utils.rs` (`peel_casts`) and `compiler/rustc_lint/src/lints.rs`
//! (`InvalidReferenceCastingDiag`) (MIT or Apache-2.0).

use blaze_hash::HashMap;
use hir_def::{
    AdtId, HasModule, VariantId,
    hir::{BinaryOp, BindingId, Expr, ExprId, Pat, UnaryOp},
};
use hir_ty::{
    db::HirDatabase as _,
    next_solver::{DbInterner, Ty, TyKind},
};
use rustc_type_ir::{
    Interner as _, Mutability,
    inherent::{AdtDef as _, IntoKind as _},
};

use super::{Inits, LayoutFacts, callee_item, expr_or_init, layout, peel_casts, span, type_info};
use crate::{
    lints::bodies::{BodyCx, ExprKind, ExprKinds, LatePass, LintSet, lint_set, types::util},
    lower::{Found, Phase},
    model::{Diag, Span},
};

const LINT: &str = "invalid_reference_casting";
const NOTE: &str = "for more information, visit <https://doc.rust-lang.org/book/ch15-05-interior-mutability.html>";

#[derive(Default)]
pub(crate) struct InvalidReferenceCasting {
    inits: Inits,
}

/// How the pointer is used: borrowed through, or assigned through.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PatternKind {
    Borrow { mutable: bool },
    Assign,
}

impl LatePass for InvalidReferenceCasting {
    const LINTS: LintSet = lint_set([LINT]);
    const EXPRS: ExprKinds =
        ExprKinds::of(&[ExprKind::Ref, ExprKind::Assignment, ExprKind::BinaryOp, ExprKind::Call]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let Some((e, pat)) = borrow_or_assign(b, expr) else { return };
        let inits = self.inits.of(b);
        let init = expr_or_init(b, inits, e);
        let init_span = span(b, init);
        let orig_cast = if init_span != span(b, e) { init_span } else { None };
        let mut peeled = None;
        let mut peel = || *peeled.get_or_insert_with(|| peel_casts(b, inits, init));
        let (Some(at), Some(expr_span)) = (b.expr_node(expr), span(b, expr)) else { return };

        if matches!(pat, PatternKind::Borrow { mutable: true } | PatternKind::Assign)
            && is_invalid_cast_from_ref_to_mut_ptr(b, init, &mut peel)
        {
            let message = match pat {
                PatternKind::Assign => "assigning to `&T` is undefined behavior, consider using an `UnsafeCell`",
                PatternKind::Borrow { .. } => {
                    "casting `&T` to `&mut T` is undefined behavior, even if the reference is unused, consider instead using an `UnsafeCell`"
                }
            };
            // clone: a lint's span, for a lint found
            let mut diag = Diag::lint(LINT, message, expr_span.clone());
            if let Some(cast) = &orig_cast {
                // clone: a lint's span, for a lint found
                diag.span_label(cast.clone(), "casting happened here");
            }
            diag.note(NOTE);
            out.push(b.lint(diag, Phase::Late, at));
        }

        if let Some(bigger) = is_cast_to_bigger_memory_layout(b, inits, init, &mut peel) {
            let mut diag = Diag::lint(
                LINT,
                "casting references to a bigger memory layout than the backing allocation is undefined behavior, even if the reference is unused",
                expr_span,
            );
            if let Some(cast) = orig_cast {
                diag.span_label(cast, "casting happened here");
            }
            diag.span_label(bigger.alloc, "backing allocation comes from here");
            // alloc: the lint's text, for a lint found
            diag.note(format!(
                "casting from `{}` ({} bytes) to `{}` ({} bytes)",
                bigger.from_ty, bigger.from.size, bigger.to_ty, bigger.to.size
            ));
            out.push(b.lint(diag, Phase::Late, at));
        }
    }
}

/// rustc's `borrow_or_assign`: the pointer `expr` borrows (`&(mut) *p`) or assigns through
/// (`*p = ..`, `*p += ..`, `ptr::write(p, ..)` and its volatile and unaligned forms).
fn borrow_or_assign(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> Option<(ExprId, PatternKind)> {
    let store = b.store();
    let deref_of = |place: ExprId| match store.expr(place) {
        Expr::UnaryOp { expr: pointer, op: UnaryOp::Deref } => Some(pointer),
        _ => None,
    };
    let place = match store.expr(expr) {
        Expr::Ref { expr: place, mutability, .. } => {
            Some((place, PatternKind::Borrow { mutable: mutability == hir_def::hir::type_ref::Mutability::Mut }))
        }
        Expr::Assignment { target, .. } => match store.pat(target) {
            Pat::Expr(place) => Some((place, PatternKind::Assign)),
            _ => None,
        },
        Expr::BinaryOp { lhs, op: Some(BinaryOp::Assignment { op: Some(_) }), .. } => Some((lhs, PatternKind::Assign)),
        _ => None,
    };
    if let Some((place, pat)) = place {
        return deref_of(place).map(|pointer| (pointer, pat));
    }
    let Expr::Call { callee, args } = store.expr(expr) else { return None };
    let [pointer, _] = args else { return None };
    let items = util::items();
    let name = callee_item(b, callee)?;
    [items.ptr_write, items.ptr_write_volatile, items.ptr_write_unaligned]
        .contains(&name)
        .then_some((*pointer, PatternKind::Assign))
}

fn is_invalid_cast_from_ref_to_mut_ptr(b: &BodyCx<'_, '_, '_>, orig: ExprId, peel: &mut impl FnMut() -> ExprId) -> bool {
    if !matches!(util::ty(b, orig).kind(), TyKind::RawPtr(_, Mutability::Mut)) {
        return false;
    }
    let e = peel();
    match util::ty(b, e).kind() {
        TyKind::Ref(_, inner, Mutability::Not) => !(has_concrete_skeleton(inner) && is_ty_fully_unsafe_celled(b, inner)),
        _ => false,
    }
}

/// rustc's `Ty::has_concrete_skeleton`.
fn has_concrete_skeleton(ty: Ty<'_>) -> bool {
    !matches!(ty.kind(), TyKind::Param(_) | TyKind::Infer(_) | TyKind::Error(_))
}

struct Bigger {
    from: LayoutFacts,
    to: LayoutFacts,
    from_ty: String,
    to_ty: String,
    alloc: Span,
}

fn is_cast_to_bigger_memory_layout(
    b: &BodyCx<'_, '_, '_>,
    inits: &HashMap<BindingId, ExprId>,
    orig: ExprId,
    peel: &mut impl FnMut() -> ExprId,
) -> Option<Bigger> {
    let TyKind::RawPtr(inner_end, _) = util::ty(b, orig).kind() else { return None };
    let e = peel();
    let TyKind::Ref(_, inner_start, _) = util::ty(b, e).kind() else { return None };

    let alloc = expr_or_init(b, inits, e);
    let alloc = match b.store().expr(alloc) {
        Expr::Ref { expr, .. } => expr,
        _ => alloc,
    };
    if matches!(
        b.store().expr(alloc),
        Expr::Index { .. } | Expr::Field { .. } | Expr::UnaryOp { op: UnaryOp::Deref, .. }
    ) {
        return None;
    }
    let alloc_ty = util::ty(b, alloc);
    if matches!(alloc_ty.kind(), TyKind::Ref(..) | TyKind::RawPtr(..) | TyKind::FnPtr(..)) {
        return None;
    }
    let from = layout(b, inner_start)?;
    if from.is_unsized {
        return None;
    }
    let alloc_layout = layout(b, alloc_ty)?;
    let to = layout(b, inner_end)?;
    if !(to.size > from.size && to.size > alloc_layout.size) {
        return None;
    }
    let from_ty = b.cx.ty(&type_info(b, e)?.original.as_reference()?.0);
    let to_ty = b.cx.ty(&type_info(b, orig)?.original.as_raw_ptr()?.0);
    Some(Bigger { from, to, from_ty, to_ty, alloc: span(b, alloc)? })
}

/// rustc's `is_ty_fully_unsafe_celled`: every byte of `ty` that is not of a zero-sized part is
/// in an `UnsafeCell`, as far as its fields show it (an enum, a type parameter or an alias is
/// not).
fn is_ty_fully_unsafe_celled<'db>(b: &BodyCx<'_, '_, 'db>, ty: Ty<'db>) -> bool {
    let interner = DbInterner::new_with(b.db(), b.owner.krate(b.db()));
    fully_unsafe_celled(b, interner, ty, 0)
}

fn fully_unsafe_celled<'db>(b: &BodyCx<'_, '_, 'db>, interner: DbInterner<'db>, ty: Ty<'db>, depth: usize) -> bool {
    if depth >= interner.recursion_limit() {
        return false;
    }
    if let TyKind::Adt(def, _) = ty.kind() {
        let unsafe_cell = interner.lang_items().UnsafeCell.map(AdtId::StructId);
        if Some(def.def_id()) == unsafe_cell || def.is_phantom_data() {
            return true;
        }
    }
    if layout(b, ty).is_some_and(|it| it.is_zst) {
        return true;
    }
    let inner = |ty: Ty<'db>| fully_unsafe_celled(b, interner, ty, depth + 1);
    match ty.kind() {
        TyKind::Adt(def, args) => {
            let variant: VariantId = match def.def_id() {
                AdtId::EnumId(_) => return false,
                AdtId::StructId(it) => it.into(),
                AdtId::UnionId(it) => it.into(),
            };
            b.db().field_types(variant).iter().all(|(_, field)| inner(field.ty().instantiate(interner, args).skip_norm_wip()))
        }
        TyKind::Tuple(fields) => fields.iter().all(inner),
        TyKind::Array(element, _) | TyKind::Slice(element) | TyKind::Pat(element, _) => inner(element),
        _ => false,
    }
}
