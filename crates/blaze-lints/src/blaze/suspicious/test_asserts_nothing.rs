//! `test_asserts_nothing` and `assertion_cannot_fail`: a test that passes whatever the code it
//! runs does, and an assertion that holds whatever the values are. Both read as checks and check
//! nothing: the test list says "covered" and the next edit that breaks the code stays green.
//!
//! `test_asserts_nothing`: a `#[test]` function (the attribute's last segment is `test`, so
//! `#[tokio::test]` too) with no failing path. A failing path is a call of a function that
//! never returns (`panic!`, `assert!` and every macro built on them expand to one), an `unwrap`
//! or `expect` (and `unwrap_err`, `expect_err`) of `Option` or `Result`, a `return` (a `?`
//! lowers to one) or a tail that is a call in a test that returns `Result`, `#[should_panic]`,
//! or a call of a function of the same crate whose own body has one of these: helpers are
//! looked into one level deep. Each is read from the body's HIR with the calls resolved, so an
//! assertion behind a macro of any crate counts and a function merely named `check` does not.
//!
//! `assertion_cannot_fail`: in any body, an `assert!` of `true` or of a `const`, an
//! `assert_eq!` whose two sides are the same place (`x`, `self.len`) or both literals, an
//! `assert_ne!` of two literals, and an assertion of any kind after an unconditional exit (a
//! statement whose type is `!`: `return`, `panic!`, `loop {}`) in the same block. `assert!(false)`
//! is the panic idiom and is left alone. No fix for either: what the test should check is the
//! author's to say.

use hir_def::hir::Expr;
use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _},
};

use super::super::{Lint, facts, std_mistakes::report};
use crate::check::Cx;

/// The assertion macros of std and core, by name.
const ASSERTS: &[&str] =
    &["assert", "assert_eq", "assert_ne", "debug_assert", "debug_assert_eq", "debug_assert_ne"];

pub(super) fn check_body(cx: &mut Cx<'_, '_>) {
    if cx.on_blaze(Lint::TestAssertsNothing) {
        asserts_nothing(cx);
    }
    if cx.on_blaze(Lint::AssertionCannotFail) {
        cannot_fail(cx);
    }
}

/// Whether `f` is a test: an attribute whose path's last segment is `test`.
pub(super) fn is_test_fn(f: ast::Fn<'_>) -> bool {
    has_attr(f, "test")
}

/// Whether `f` has an attribute whose path's last segment is `name` (not inside `cfg_attr`).
pub(super) fn has_attr(f: ast::Fn<'_>, name: &str) -> bool {
    f.attrs().any(|attr| {
        attr.path()
            .and_then(|path| path.segment())
            .and_then(|segment| segment.name_ref())
            .is_some_and(|it| it.text() == name)
    })
}

fn asserts_nothing(cx: &mut Cx<'_, '_>) {
    let Some(f) = ast::Fn::cast(cx.root()) else { return };
    if !is_test_fn(f) || has_attr(f, "should_panic") || f.body().is_none() {
        return;
    }
    let Some(analysis) = cx.analysis() else { return };
    let hir::DefWithBody::Function(func) = analysis.def() else { return };
    if fails(cx, analysis, func, 1) {
        return;
    }
    let Some(name) = f.name() else { return };
    let Some(range) = cx.range(name.syntax()) else { return };
    let message = format!(
        "the test `{}` has no way to fail: no assertion, no panic, no `unwrap` and no error \
         returned, in it or in the helpers it calls, so it passes whatever the code it runs does",
        name.text()
    );
    let note = "assert what the code must do (`assert_eq!(got, want)`), return the `Result` it \
                gives, or say with `#[should_panic(expected = \"..\")]` that the panic is the check";
    report(cx, Lint::TestAssertsNothing, f.syntax(), range, message, note, None);
}

/// Whether the body `analysis` (of `func`) has a failing path, looking `depth` levels into the
/// same crate's functions it calls.
fn fails(cx: &Cx<'_, '_>, analysis: &hir::BodyAnalysis<'_>, func: hir::Function, depth: u32) -> bool {
    let db = cx.db();
    let famous = cx.famous();
    let option = famous.core_option_Option().map(hir::Adt::Enum);
    let result = famous.core_result_Result().map(hir::Adt::Enum);
    let returns_result = result.is_some() && func.ret_type(db).as_adt() == result;
    let body = analysis.body();
    if returns_result
        && let Expr::Block { tail: Some(tail), .. } = body.expr(body.root_expr())
        && analysis.callee(tail).is_some()
    {
        return true;
    }
    for (id, expr) in body.exprs() {
        if returns_result && matches!(expr, Expr::Return { .. }) {
            return true;
        }
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        let Some((id, _)) = analysis.callee(id) else { continue };
        let callee = hir::Function::from(id);
        if callee.ret_type(db).is_never() {
            return true;
        }
        let adt = facts::impl_adt(db, id);
        if matches!(callee.name(db).as_str(), "unwrap" | "expect" | "unwrap_err" | "expect_err")
            && adt.is_some()
            && (adt == option || adt == result)
        {
            return true;
        }
        if depth > 0
            && callee != func
            && callee.module(db).krate(db) == cx.krate()
            && callee.has_body(db)
        {
            let inner = hir::BodyAnalysis::new(db, hir::DefWithBody::from(callee));
            if fails(cx, &inner, callee, depth - 1) {
                return true;
            }
        }
    }
    false
}

fn cannot_fail<'t>(cx: &mut Cx<'_, 't>) {
    let root = cx.root();
    // alloc: the assertions of one body, a few
    let mut found: Vec<(ast::Node<'t>, TextRange, String)> = Vec::new();
    // alloc: one assertion's arguments, reused
    let mut args = Vec::new();
    for node in root.descendants() {
        if node != root && in_inner_body(root, node) {
            continue;
        }
        if let Some(list) = ast::StmtList::cast(node) {
            after_exit(cx, list, &mut found);
            continue;
        }
        let Some(call) = ast::MacroCall::cast(node) else { continue };
        let Some(name) = assert_name(cx, call) else { continue };
        cx.macro_args(call, &mut args);
        let Some(range) = cx.range(node) else { continue };
        let why = match name {
            "assert" | "debug_assert" => {
                let Some((_, cond)) = args.first() else { continue };
                if is_true(*cond) {
                    "its condition is `true`".to_owned()
                } else if names_const(cx, *cond) {
                    "its condition is a `const`, fixed when the crate is built".to_owned()
                } else {
                    continue;
                }
            }
            _ => {
                let [(_, left), (_, right), ..] = args.as_slice() else { continue };
                let (left, right) = (left.peel_parens(), right.peel_parens());
                let literals = matches!(left, ast::Expr::Literal(_)) && matches!(right, ast::Expr::Literal(_));
                if literals {
                    "both sides are literals".to_owned()
                } else if name.ends_with("_eq") && same_place(cx, left, right) {
                    format!("both sides are `{}`, the same place", cx.source_of(left.syntax()))
                } else {
                    continue;
                }
            }
        };
        found.push((node, range, format!("this `{name}!` cannot fail: {why}")));
    }
    for (node, range, message) in found {
        let note = "assert something the code computes against what it must be, or delete the \
                    assertion";
        report(cx, Lint::AssertionCannotFail, node, range, message, note, None);
    }
}

/// The assertions in a block after a statement that never finishes: each is never reached.
fn after_exit<'t>(
    cx: &Cx<'_, 't>,
    list: ast::StmtList<'t>,
    found: &mut Vec<(ast::Node<'t>, TextRange, String)>,
) {
    let mut exited = false;
    let tail = list.tail_expr().map(|it| it.syntax());
    for node in list.statements().map(|it| it.syntax()).chain(tail) {
        if exited {
            let Some(call) = node.descendants().filter_map(ast::MacroCall::cast).find(|it| assert_name(cx, *it).is_some())
            else {
                continue;
            };
            let Some(range) = cx.range(call.syntax()) else { continue };
            let name = assert_name(cx, call).map_or("assert", |it| it);
            found.push((
                call.syntax(),
                range,
                format!("this `{name}!` is never reached: a statement before it never finishes"),
            ));
            return;
        }
        let Some(stmt) = ast::ExprStmt::cast(node) else { continue };
        let Some(expr) = stmt.expr() else { continue };
        exited = cx.ty(expr).is_some_and(|it| it.is_never());
    }
}

/// Whether `node` is in an item inside `root` that is a body of its own (walked on its own).
fn in_inner_body(root: ast::Node<'_>, node: ast::Node<'_>) -> bool {
    node.ancestors().take_while(|it| *it != root).any(|it| {
        matches!(it.kind(), SyntaxKind::FN | SyntaxKind::CONST | SyntaxKind::STATIC)
    })
}

/// The name of the std or core assertion macro `call` calls.
fn assert_name<'t>(cx: &Cx<'_, 't>, call: ast::MacroCall<'t>) -> Option<&'static str> {
    let db = cx.db();
    let mac = cx.macro_def(call)?;
    let krate = cx.macro_crate(call)?;
    if !(facts::crate_is(db, krate, "std") || facts::crate_is(db, krate, "core")) {
        return None;
    }
    let name = mac.name(db);
    ASSERTS.iter().copied().find(|it| *it == name.as_str())
}

/// Whether `expr` is the literal `true`.
fn is_true(expr: ast::Expr<'_>) -> bool {
    matches!(expr.peel_parens(), ast::Expr::Literal(it) if it.kind() == ast::LiteralKind::Bool(true))
}

/// Whether `expr` is a path to a `const`.
fn names_const<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> bool {
    let ast::Expr::PathExpr(path) = expr.peel_parens() else { return false };
    let Some(path) = path.path() else { return false };
    matches!(cx.path(path), Some(hir::PathResolution::Def(hir::ModuleDef::Const(_))))
}

/// Whether `a` and `b` are the same place: the same local, `static` or `const`, or the same
/// fields of the same place.
fn same_place<'t>(cx: &Cx<'_, 't>, a: ast::Expr<'t>, b: ast::Expr<'t>) -> bool {
    match (a.peel_parens(), b.peel_parens()) {
        (ast::Expr::PathExpr(a), ast::Expr::PathExpr(b)) => {
            let (Some(a), Some(b)) = (a.path(), b.path()) else { return false };
            let (Some(a), Some(b)) = (cx.path(a), cx.path(b)) else { return false };
            a == b
        }
        (ast::Expr::FieldExpr(a), ast::Expr::FieldExpr(b)) => {
            let (Some(fa), Some(fb)) = (a.name_ref(), b.name_ref()) else { return false };
            let (Some(ra), Some(rb)) = (a.expr(), b.expr()) else { return false };
            fa.text() == fb.text() && same_place(cx, ra, rb)
        }
        (ast::Expr::RefExpr(a), ast::Expr::RefExpr(b)) => {
            let (Some(a), Some(b)) = (a.expr(), b.expr()) else { return false };
            same_place(cx, a, b)
        }
        _ => false,
    }
}
