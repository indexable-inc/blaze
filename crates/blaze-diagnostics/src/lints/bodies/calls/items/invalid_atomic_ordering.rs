//! rustc's `InvalidAtomicOrdering` (`invalid_atomic_ordering`, deny): an `Ordering` an atomic
//! operation panics on, named as a variant of `core::sync::atomic::Ordering`: a load that is
//! `Release` or `AcqRel`, a store that is `Acquire` or `AcqRel`, a fence that is `Relaxed`, and
//! the failure ordering of a `compare_exchange`, `compare_exchange_weak`, `fetch_update`,
//! `update` or `try_update` that is `Release` or `AcqRel`. Only methods of the inherent impls
//! of `Atomic` are checked, not those of traits.
//!
//! Follows rustc's `compiler/rustc_lint/src/types.rs` (`InvalidAtomicOrdering`) and its
//! diagnostics in `compiler/rustc_lint/src/lints.rs` (`AtomicOrderingLoad`,
//! `AtomicOrderingStore`, `AtomicOrderingFence`, `InvalidAtomicOrderingDiag`) (MIT or
//! Apache-2.0).

use hir_ty::db::HirDatabase;
use blaze_items::{Fact, ItemId};
use hir_def::{
    AdtId, AttrDefId, FunctionId, ItemContainerId, Lookup as _, ModuleDefId,
    hir::{Expr, ExprId},
    resolver::ValueNs,
};
use hir_ty::next_solver::TyKind;
use rustc_type_ir::inherent::IntoKind as _;

use super::FnItems;
use crate::{
    lints::bodies::{BodyCx, ExprKinds, ExprKind, LatePass, LintSet, lint_set, types::util},
    lower::{Found, Phase},
    model::Diag,
};

const LINT: &str = "invalid_atomic_ordering";

pub(crate) struct InvalidAtomicOrdering {
    fences: FnItems,
}

impl Default for InvalidAtomicOrdering {
    fn default() -> InvalidAtomicOrdering {
        InvalidAtomicOrdering { fences: FnItems::new(&["fence", "compiler_fence"]) }
    }
}

/// A variant of `core::sync::atomic::Ordering`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ordering {
    Relaxed,
    Release,
    Acquire,
    AcqRel,
    SeqCst,
}

impl LatePass for InvalidAtomicOrdering {
    const LINTS: LintSet = lint_set([LINT]);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::MethodCall, ExprKind::Call]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        match b.store().expr(expr) {
            Expr::MethodCall { method_name, args, .. } => {
                let method = method_name.as_str();
                check_load_store(b, expr, method, args, out);
                check_compare_exchange(b, expr, method, args, out);
            }
            Expr::Call { args, .. } => self.check_fence(b, expr, args, out),
            _ => {}
        }
    }
}

impl InvalidAtomicOrdering {
    /// rustc's `check_memory_fence`.
    fn check_fence(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, args: &[ExprId], out: &mut Vec<Found>) {
        let Some((def, _)) = util::called_fn(b, expr) else { return };
        if self.fences.of(b.db(), def).is_none() {
            return;
        }
        let Some(&arg) = args.first() else { return };
        if ordering(b, arg) != Some(Ordering::Relaxed) {
            return;
        }
        let Some(mut diag) = diag(b, arg, "memory fences cannot have `Relaxed` ordering") else { return };
        diag.help("consider using ordering modes `Acquire`, `Release`, `AcqRel` or `SeqCst`");
        emit(b, expr, def, diag, out);
    }
}

/// rustc's `check_atomic_load_store`.
fn check_load_store(b: &BodyCx<'_, '_, '_>, expr: ExprId, method: &str, args: &[ExprId], out: &mut Vec<Found>) {
    let (index, invalid, message, help) = match method {
        "load" => (
            0,
            Ordering::Release,
            "atomic loads cannot have `Release` or `AcqRel` ordering",
            "consider using ordering modes `Acquire`, `SeqCst` or `Relaxed`",
        ),
        "store" => (
            1,
            Ordering::Acquire,
            "atomic stores cannot have `Acquire` or `AcqRel` ordering",
            "consider using ordering modes `Release`, `SeqCst` or `Relaxed`",
        ),
        _ => return,
    };
    let Some(def) = inherent_atomic_method(b, expr) else { return };
    let Some(&arg) = args.get(index) else { return };
    let Some(ordering) = ordering(b, arg) else { return };
    if ordering != invalid && ordering != Ordering::AcqRel {
        return;
    }
    let Some(mut diag) = diag(b, arg, message) else { return };
    diag.help(help);
    emit(b, expr, def, diag, out);
}

/// rustc's `check_atomic_compare_exchange`.
fn check_compare_exchange(b: &BodyCx<'_, '_, '_>, expr: ExprId, method: &str, args: &[ExprId], out: &mut Vec<Found>) {
    let index = match method {
        "update" | "try_update" | "fetch_update" => 1,
        "compare_exchange" | "compare_exchange_weak" => 3,
        _ => return,
    };
    let Some(def) = inherent_atomic_method(b, expr) else { return };
    let Some(&arg) = args.get(index) else { return };
    if !matches!(ordering(b, arg), Some(Ordering::Release | Ordering::AcqRel)) {
        return;
    }
    // alloc: the lint's diagnostic
    let message = format!(
        "`{method}`'s failure ordering may not be `Release` or `AcqRel`, since a failed `{method}` does not result in a write"
    );
    let Some(mut diag) = diag(b, arg, message) else { return };
    if let Some(span) = diag.span.primary_span().cloned() {
        diag.span_label(span, "invalid failure ordering");
    }
    diag.help("consider using `Acquire` or `Relaxed` failure ordering instead");
    emit(b, expr, def, diag, out);
}

/// rustc's `inherent_atomic_method_call`: the method `expr` calls, when it is of an inherent
/// impl of `Atomic` (std's, not an extension trait's).
fn inherent_atomic_method(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> Option<FunctionId> {
    let db = b.db();
    let def = util::method_fn(b, expr)?;
    let ItemContainerId::ImplId(impl_) = def.lookup(db).container else { return None };
    if db.impl_trait(impl_).is_some() {
        return None;
    }
    let TyKind::Adt(adt, _) = db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip().kind() else {
        return None;
    };
    let atomic = util::diagnostic_item(db, AttrDefId::AdtId(adt.def_id()))?;
    (atomic.as_str() == "Atomic").then_some(def)
}

/// rustc's `match_ordering`: the variant of `Ordering` (std's diagnostic item `Ordering`) the
/// path `expr` names.
fn ordering(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> Option<Ordering> {
    let Some(ValueNs::EnumVariantId(variant)) = util::path_value(b, expr) else { return None };
    let db = b.db();
    let loc = variant.lookup(db);
    let ordering = match loc.name.as_str() {
        "Relaxed" => Ordering::Relaxed,
        "Release" => Ordering::Release,
        "Acquire" => Ordering::Acquire,
        "AcqRel" => Ordering::AcqRel,
        "SeqCst" => Ordering::SeqCst,
        _ => return None,
    };
    let item = util::diagnostic_item(db, AttrDefId::AdtId(AdtId::EnumId(loc.parent)))?;
    (item.as_str() == "Ordering").then_some(ordering)
}

/// The lint at the ordering argument `arg`.
fn diag(b: &BodyCx<'_, '_, '_>, arg: ExprId, message: impl Into<String>) -> Option<Diag> {
    Some(Diag::lint(LINT, message, util::span(b, arg)?))
}

/// The lint found at the call `expr` of `def`.
fn emit(b: &BodyCx<'_, '_, '_>, expr: ExprId, def: FunctionId, mut diag: Diag, out: &mut Vec<Found>) {
    let Some(at) = b.expr_node(expr) else { return };
    let called = b.cx.def_path(ModuleDefId::FunctionId(def).into());
    // alloc: the lint's diagnostic
    diag.fact(Fact::Signature(b.cx.key(ItemId::Function(def))), format!("the signature of `{called}`"));
    out.push(b.lint(diag, Phase::Late, at));
}
