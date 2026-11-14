//! `test_misses_its_subject`: a `#[test]` that never touches the code it is in the crate to
//! test. No call edge leads from it to a function, method or constructor of the crate's own
//! non-test code, and no value it makes has a type of that code; helpers of the test's own
//! module are followed one level deep. For an integration test (a `tests/` target) the subject
//! is every workspace crate it depends on. Such a test checks std, a copy of the logic pasted
//! into the test, or a mock: it stays green whatever the crate does, which is how a refactor
//! that deleted the real path ships with every test passing.
//!
//! Read from the body's HIR with every call resolved, never by name: a helper named like the
//! subject is no edge. No fix.

use base_db::Db;
use hir::HasAttrs as _;
use hir_def::hir::Expr;
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{
    super::{Lint, std_mistakes::report, test_code::names_test},
    test_asserts_nothing::is_test_fn,
};
use crate::check::Cx;

const LINT: Lint = Lint::TestMissesItsSubject;

pub(super) fn check_body(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(f) = ast::Fn::cast(cx.root()) else { return };
    if !is_test_fn(f) || f.body().is_none() {
        return;
    }
    let Some(analysis) = cx.analysis() else { return };
    if !has_subject(cx) || touches(cx, analysis, 1) {
        return;
    }
    let Some(name) = f.name() else { return };
    let Some(range) = cx.range(name.syntax()) else { return };
    let message = format!(
        "the test `{}` reaches none of the crate's own code: no call, constructor or value of a \
         type of it, in the test or the helpers it calls, so it passes whatever the crate does",
        name.text()
    );
    let note = "call the function the test is about (not a copy of its logic or a stand-in), or \
                delete the test";
    report(cx, LINT, f.syntax(), range, message, note, None);
}

/// Whether the crate linted has code a test is there to test: its own non-test items, or, for a
/// test target, a workspace crate it depends on.
fn has_subject(cx: &Cx<'_, '_>) -> bool {
    let db = cx.db();
    let krate = cx.krate();
    if matches!(krate.target_kind(db), base_db::TargetKind::Test) {
        return krate.dependencies_iter(db).any(|it| it.krate.origin(db).is_local());
    }
    true
}

/// Whether `krate` is code a test of the crate linted tests: the crate itself outside its test
/// modules, or a workspace crate a test target depends on.
fn is_subject(cx: &Cx<'_, '_>, krate: hir::Crate, module: hir::Module, attrs_test: bool) -> bool {
    let db = cx.db();
    if krate == cx.krate() {
        return !attrs_test && !in_test_module(db, module);
    }
    matches!(cx.krate().target_kind(db), base_db::TargetKind::Test) && krate.origin(db).is_local()
}

/// Whether the body `analysis` touches the subject, looking `depth` levels into the test code
/// of the same crate it calls.
fn touches(cx: &Cx<'_, '_>, analysis: &hir::BodyAnalysis<'_>, depth: u32) -> bool {
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    for (id, expr) in body.exprs() {
        let ty = analysis.ty(infer.expr_ty(id)).strip_references();
        if let Some(adt) = ty.as_adt() {
            let module = adt.module(db);
            if is_subject(cx, module.krate(db), module, test_item(db, adt)) {
                return true;
            }
        }
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        let Some((callee, _)) = analysis.callee(id) else { continue };
        let callee = hir::Function::from(callee);
        let module = callee.module(db);
        let krate = module.krate(db);
        if is_subject(cx, krate, module, test_item(db, callee)) {
            return true;
        }
        if depth > 0 && krate == cx.krate() && callee.has_body(db) {
            let inner = hir::BodyAnalysis::new(db, hir::DefWithBody::from(callee));
            if touches(cx, &inner, depth - 1) {
                return true;
            }
        }
    }
    false
}

/// Whether `item` is itself `#[cfg(test)]` or a `#[test]`.
fn test_item(db: &Db, item: impl hir::HasAttrs + Copy) -> bool {
    let attrs = item.attrs(db);
    attrs.cfgs(db).is_some_and(names_test) || attrs.is_test()
}

/// Whether `module` or a module it is in is `#[cfg(test)]`.
fn in_test_module(db: &Db, module: hir::Module) -> bool {
    let mut at = Some(module);
    while let Some(it) = at.take() {
        if it.attrs(db).cfgs(db).is_some_and(names_test) {
            return true;
        }
        at = it.parent(db);
    }
    false
}

