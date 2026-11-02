//! `NoopMethodCall`: `noop_method_call`, a call of `clone`, `deref` or `borrow` that resolves to
//! the impl for `&T` and returns the reference it was called on, and `suspicious_double_ref_op`,
//! one on `&&T` that returns the inner `&T`.
//!
//! Follows rustc's `compiler/rustc_lint/src/noop_method_call.rs` and
//! `compiler/rustc_lint/src/lints.rs` (`NoopMethodCallDiag`, `SuspiciousDoubleRefDerefDiag`,
//! `SuspiciousDoubleRefCloneDiag`) (MIT or Apache-2.0).

use base_db::Db;
use either::Either;
use hir::InFile;
use hir_def::{AdtId, AttrDefId, HasModule as _, ItemContainerId, Lookup as _, hir::ExprId, src::HasSource as _};
use hir_ty::{
    Adjust, ParamEnvAndCrate,
    db::HirDatabase,
    next_solver::{DbInterner, Region, RegionExt as _, Ty, TyKind},
};
use rustc_type_ir::inherent::IntoKind as _;
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::def_range_view,
    lints::bodies::{
        BodyCx, ExprKinds, ExprKind, LatePass, LintSet, lint_set,
        types::util::{self, diagnostic_item, fn_item, from_expansion},
    },
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

#[derive(Default)]
pub(crate) struct NoopMethodCall;

/// The method a call resolves to in the impl for `&T`, by its diagnostic item.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Noop {
    Borrow,
    Clone,
    Deref,
}

impl LatePass for NoopMethodCall {
    const LINTS: LintSet = lint_set(["noop_method_call", "suspicious_double_ref_op"]);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::MethodCall]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        if let Some(found) = noop_method_call(b, expr) {
            out.push(found);
        }
    }
}

fn noop_method_call(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> Option<Found> {
    let hir_def::hir::Expr::MethodCall { receiver, method_name, .. } = b.store().expr(expr) else { return None };
    let db = b.db();
    let (method, args) = b.infer.method_resolution(expr)?;
    let ItemContainerId::TraitId(trait_id) = method.lookup(db).container else { return None };
    let trait_name = diagnostic_item(db, AttrDefId::TraitId(trait_id))?;
    if !matches!(trait_name.as_str(), "Borrow" | "Clone" | "Deref") {
        return None;
    }
    let env = ParamEnvAndCrate {
        param_env: db.trait_environment(b.owner.generic_def(db)),
        krate: b.owner.krate(db),
    };
    let (Either::Left(instance), _) = db.lookup_impl_method(env, method, args) else { return None };
    let noop = match fn_item(db, instance)?.as_str() {
        "noop_method_borrow" => Noop::Borrow,
        "noop_method_clone" => Noop::Clone,
        "noop_method_deref" => Noop::Deref,
        _ => return None,
    };
    // Last of the tests: it reads the source.
    let node = util::syntax(b, expr)?;
    // clone: a node handle, a reference count, once the call is of a noop method
    let call = ast::MethodCallExpr::cast(node.value)?;
    let name = call.name_ref()?;
    let call_span = b.cx.span(node.with_value(TextRange::new(
        name.syntax().text_range().start(),
        node.value.text_range().end(),
    )));
    if from_expansion(&call_span) {
        return None;
    }

    let interner = DbInterner::new_with(db, b.owner.krate(db));
    let receiver_ty = erased(interner, b.infer.expr_ty(receiver));
    let expr_ty = erased(interner, util::ty_adjusted(b, expr));
    let adjustments = b.infer.expr_adjustment(receiver).unwrap_or_default();
    if adjustments.iter().any(|it| matches!(it.kind, Adjust::Deref(Some(_)))) {
        return None;
    }

    let expr_span = util::span(b, expr)?;
    let receiver_span = util::span(b, receiver)?;
    let receiver_span = util::ancestor_inside(&receiver_span, &expr_span)?;
    let span = expr_span.with_lo(receiver_span.hi);
    let at = b.expr_node(expr)?;
    let method_name = method_name.as_str();

    let mut diag = if receiver_ty == expr_ty {
        let orig_ty = expr_ty.strip_references();
        let printed = b.ty(orig_ty);
        let mut diag = Diag::lint(
            "noop_method_call",
            // alloc: the lint's diagnostic
            format!("call to `.{method_name}()` on a reference in this situation does nothing"),
            // clone: a node handle or span, for the lint's diagnostic
            span.clone(),
        );
        // alloc: the lint's diagnostic
        diag.note(format!(
            "the type `{printed}` does not implement `{trait_name}`, so calling `{method_name}` on `&{printed}` copies the reference, which does not do anything and can be removed"
        ));
        diag.suggestion(span, "remove this redundant call", "", Applicability::MachineApplicable);
        if let TyKind::Adt(def, _) = orig_ty.kind() {
            diag.suggestion(
                adt_def_span(b, def.def_id()).shrink_to_lo(),
                // alloc: the lint's diagnostic
                format!("if you meant to clone `{printed}`, implement `Clone` for it"),
                "#[derive(Clone)]\n",
                Applicability::MaybeIncorrect,
            );
        }
        facts(b, &mut diag, receiver, method_name, trait_id, trait_name.as_str(), orig_ty);
        diag
    } else {
        let ty = b.ty(erased(interner, b.infer.expr_ty(expr)));
        let message = match noop {
            Noop::Borrow => return None,
            Noop::Clone => {
                // alloc: the lint's diagnostic
                format!("using `.clone()` on a double reference, which returns `{ty}` instead of cloning the inner type")
            }
            // alloc: the lint's diagnostic
            Noop::Deref => format!(
                "using `.deref()` on a double reference, which returns `{ty}` instead of dereferencing the inner type"
            ),
        };
        let mut diag = Diag::lint("suspicious_double_ref_op", message, span);
        facts(b, &mut diag, receiver, method_name, trait_id, trait_name.as_str(), expr_ty.strip_references());
        diag
    };
    Some(b.lint(diag, Phase::Late, at))
}

/// `ty` with its free regions erased, as rustc's type checking results have them.
fn erased<'db>(interner: DbInterner<'db>, ty: Ty<'db>) -> Ty<'db> {
    rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_erased(interner))
}

/// rustc's `def_span` of an ADT: its header, from its visibility to its generics.
fn adt_def_span(b: &BodyCx<'_, '_, '_>, adt: AdtId) -> Span {
    let db: &Db = b.db();
    let node: InFile<ast::Node<'_>> = match adt {
        AdtId::StructId(it) => it.lookup(db).source_view(db).map(|it| it.syntax()),
        AdtId::EnumId(it) => it.lookup(db).source_view(db).map(|it| it.syntax()),
        AdtId::UnionId(it) => it.lookup(db).source_view(db).map(|it| it.syntax()),
    };
    b.cx.span(node.map(def_range_view))
}

/// What the lint read: which method the call is on the receiver's type, and whether the type
/// under the references implements the trait.
fn facts<'db>(
    b: &BodyCx<'_, '_, 'db>,
    diag: &mut Diag,
    receiver: ExprId,
    method_name: &str,
    trait_id: hir_def::TraitId,
    trait_name: &str,
    inner: Ty<'db>,
) {
    let Some(written) = util::syntax(b, receiver).and_then(|it| ast::Expr::cast(it.value)) else { return };
    let Some(receiver_ty) = b.cx.vsema.type_of_expr(written) else { return };
    let receiver_ty = receiver_ty.original;
    let printed = b.cx.ty(&receiver_ty);
    diag.fact_opt(
        b.cx.method_fact(&receiver_ty, method_name),
        // alloc: the lint's diagnostic
        format!("what `{method_name}` is on `{printed}`"),
    );
    let inner = receiver_ty.derived_from_next_solver(inner);
    // alloc: the lint's diagnostic
    let what = format!("whether `{}` implements `{trait_name}`", b.cx.ty(&inner));
    diag.fact_opt(b.cx.implements_fact(hir::Trait::from(trait_id), &inner), what);
}
