//! `could_be_linear`: a type with a method that finishes a value by taking it by value (`close`,
//! `commit`, `finish`) and a `Drop::drop` that does the same work implicitly: it calls a
//! function of the type's crate that the finishing method calls too, or it can panic. A value
//! dropped instead of finished then does its last step where no caller sees it, with nowhere
//! to report its failure (and a panic in `drop` while unwinding aborts the process).
//!
//! `#[blaze::linear]` on the type and `#[blaze::consumes]` on the finishing method make the
//! choice explicit: every path hands each value to a consuming method, checked on built MIR
//! (docs/v2/plan/design/linear.md), and the `Drop` is left only for an error exit's abandon.
//!
//! Found in the body of `Drop::drop` of a struct or enum of the crate linted that is not already
//! `#[blaze::linear]`, against the inherent methods in the type's own module that take `self` by
//! value and are not `#[blaze::consumes]` already (a method in another module is not looked
//! at). Not found: test code; a `drop` that only frees (calls nothing of its crate and cannot
//! panic). The calls compared are what each body writes (`alloc::written_callee`), not what
//! their callees call.
//!
//! Class: cheap (per body: one walk of `drop` and of each finishing method, whose callees are
//! read from their own bodies' inference). No fix: which method finishes and what an error exit
//! does are choices.

use hir::AsAssocItem as _;
use hir_def::{AttrDefId, DefWithBodyId, FunctionId, attrs::AttrFlags, expr_store::Body, hir::Expr};
use hir_ty::InferenceResult;
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::super::{Lint, alloc, general_facts, std_mistakes, test_code};
use crate::check::Cx;

const LINT: Lint = Lint::CouldBeLinear;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !cx.on_blaze(LINT) || test_code::is_test_code(cx, f.syntax()) {
        return;
    }
    let Some(analysis) = cx.analysis() else { return };
    let Ok(DefWithBodyId::FunctionId(id)) = DefWithBodyId::try_from(analysis.def()) else { return };
    let db = cx.db();
    let Some(drop) = cx.lang_trait(hir::LangItem::Drop) else { return };
    let func = hir::Function::from(id);
    let Some(hir::AssocItemContainer::Impl(imp)) = func.as_assoc_item(db).map(|it| it.container(db)) else {
        return;
    };
    if imp.trait_(db) != Some(drop) || func.name(db).as_str() != "drop" {
        return;
    }
    let Some(adt) = imp.self_ty(db).as_adt() else { return };
    if adt.module(db).krate(db) != cx.krate() {
        return;
    }
    let adt_id = hir_def::AdtId::from(adt);
    if AttrFlags::query(db, AttrDefId::AdtId(adt_id)).contains(AttrFlags::LINEAR) {
        return;
    }
    let krate = cx.krate();
    let dropped = callees(db, id, krate);
    let panics = !general_facts::panic_sites(db, analysis).is_empty()
        && !general_facts::calls_std(db, analysis, "thread", "panicking");
    if dropped.is_empty() && !panics {
        return;
    }
    // The finishing methods: inherent, in the type's module, `self` by value.
    let mut finishing: Option<(hir::Function, Option<FunctionId>)> = None;
    'impls: for other in adt.module(db).impl_defs(db) {
        if other.trait_(db).is_some() || other.self_ty(db).as_adt() != Some(adt) {
            continue;
        }
        for item in other.items(db) {
            let hir::AssocItem::Function(method) = item else { continue };
            let owned = method.self_param(db).is_some_and(|it| matches!(it.access(db), hir::Access::Owned));
            let Ok(method_id) = FunctionId::try_from(method) else { continue };
            if !owned
                || !method.has_body(db)
                || AttrFlags::query(db, AttrDefId::FunctionId(method_id)).contains(AttrFlags::CONSUMES)
            {
                continue;
            }
            let shared = callees(db, method_id, krate).into_iter().find(|it| dropped.contains(it));
            if shared.is_some() {
                finishing = Some((method, shared));
                break 'impls;
            }
            if panics && finishing.is_none() {
                finishing = Some((method, None));
            }
        }
    }
    let Some((method, shared)) = finishing else { return };
    let ty = adt.name(db).as_str().to_owned();
    let method = method.name(db).as_str().to_owned();
    let what = match shared {
        Some(it) => format!("calls `{}`, as `{ty}::{method}` does", hir::Function::from(it).name(db).as_str()),
        None => "can panic".to_owned(),
    };
    let message = format!(
        "`Drop::drop` of `{ty}` {what}: a `{ty}` dropped instead of finished by `{method}` does that work where \
         no caller sees it fail; make `{ty}` `#[blaze::linear]` and `{method}` `#[blaze::consumes]`"
    );
    let note = "a `#[blaze::linear]` value cannot be dropped implicitly: every path hands it to a `#[blaze::consumes]` \
                method, checked on built MIR, and `Drop` is left for an error exit's abandon";
    let Some(range) = f.name().and_then(|it| cx.range(it.syntax())) else { return };
    std_mistakes::report(cx, LINT, f.syntax(), range, message, note, None);
}

/// The functions of `krate` that `f`'s body calls, as written, each once, in body order.
fn callees(db: &base_db::Db, f: FunctionId, krate: hir::Crate) -> Vec<FunctionId, std::alloc::Global> {
    let def = DefWithBodyId::FunctionId(f);
    let body = Body::of(db, def);
    let infer = InferenceResult::of(db, def);
    let mut out = Vec::new_in(std::alloc::Global);
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        let Some((callee, _)) = alloc::written_callee(infer, body, id) else { continue };
        if hir::Function::from(callee).module(db).krate(db) == krate && callee != f && !out.contains(&callee) {
            out.push(callee);
        }
    }
    out
}
