//! `unnecessary_map_or`: `.map_or(false, f)` and `.map_or(true, f)` on an `Option` or a
//! `Result`, which `is_some_and`, `is_ok_and` or `is_none_or` say plainer.
//!
//! Follows clippy's `clippy_lints/src/methods/unnecessary_map_or.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0).
//!
//! Where clippy would suggest a comparison (`.map_or(false, |x| x == y)`) or `is_ok`/`is_err`
//! for a `Result` whose drop order could change, this port says nothing: those need clippy's
//! eager-evaluation and drop-order analyses.

use syntax::{TextRange, view as ast};

use ast::{AstNode as _, HasArgList as _, HasName as _};

use super::utils;
use crate::{check::Cx, diag::Applicability, msrvs};

const LINT: super::Implemented = super::Implemented::UnnecessaryMapOr;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, expr: ast::Expr<'t>) {
    if !cx.on(LINT) || cx.expansion().is_some() || utils::method_name(expr) != Some("map_or") {
        return;
    }
    let Some(call) = utils::method_call(cx, expr) else { return };
    let [def, map] = *call.args() else { return };
    let def_bool = match def {
        ast::Expr::Literal(literal) => match literal.kind() {
            ast::LiteralKind::Bool(it) => Some(it),
            _ => None,
        },
        _ => None,
    };
    // Only a `bool` literal default, or two different `bool` constants, can be linted.
    if def_bool.is_none()
        && bool_constant(def).zip(closure_bool_constant(map)).is_none_or(|(a, b)| a == b)
    {
        return;
    }
    let (name_ref, recv) = (call.name_ref, call.receiver);
    let Some(recv_ty) = cx.adjusted_ty(recv) else { return };
    let kind = utils::option_or_result(cx, &recv_ty);
    if kind == Some("Result") && result_variant_query(cx, expr, name_ref, def, map, &recv_ty) {
        return;
    }
    let Some(def_bool) = def_bool else { return };
    let Some(kind) = kind else { return };
    if is_comparison(cx, recv, map, def_bool) {
        return;
    }
    let (Some(whole), Some(method), Some(def_range), Some(map_range)) = (
        cx.range(expr.syntax()),
        cx.range(name_ref.syntax()),
        cx.range(def.syntax()),
        cx.range(map.syntax()),
    ) else {
        return;
    };
    let suggested =
        match (def_bool, kind) {
            (false, _) if cx.conf().meets(msrvs::OPTION_RESULT_IS_VARIANT_AND) => {
                if kind == "Option" { "is_some_and" } else { "is_ok_and" }
            }
            (true, "Option") if cx.conf().meets(msrvs::IS_NONE_OR) => "is_none_or",
            _ => return,
        };
    let parts = vec![
        (cx.span(method), suggested.to_owned()),
        (cx.span(TextRange::new(def_range.start(), map_range.start())), String::new()),
    ];
    let span = cx.span(whole);
    cx.lint(LINT, expr.syntax(), span, "this `map_or` can be simplified").multipart_suggestion(
        format!("use `{suggested}` instead"),
        parts,
        Applicability::MachineApplicable,
    );
}

/// clippy's `check_result_variant_query`: `.map_or(a, |_| b)` with `a` and `b` different bool
/// constants, which is `is_ok()` or `is_err()`. Says whether clippy would take it as one.
fn result_variant_query<'t>(
    cx: &mut Cx<'_, 't>,
    expr: ast::Expr<'t>,
    name_ref: ast::NameRef<'t>,
    def: ast::Expr<'t>,
    map: ast::Expr<'t>,
    recv_ty: &hir::Type<'_>,
) -> bool {
    let Some(def_bool) = bool_constant(def) else { return false };
    let Some(map_bool) = closure_bool_constant(map) else { return false };
    if def_bool == map_bool {
        return false;
    }
    // Dropping the value earlier only changes nothing when it has no drop.
    if !recv_ty.is_copy(cx.db()) {
        return true;
    }
    let ast::Expr::MethodCallExpr(call) = expr else { return false };
    let (Some(method), Some(end)) =
        (cx.range(name_ref.syntax()), call.arg_list().and_then(|it| cx.range(it.syntax())))
    else {
        return false;
    };
    let suggested = if map_bool { "is_ok" } else { "is_err" };
    let call_span = cx.span(TextRange::new(method.start(), end.end()));
    let span = cx.span(method);
    cx.lint(LINT, expr.syntax(), span, format!("this `{}` can be simplified", name_ref.text()))
        .suggestion(
            call_span,
            format!("use `{suggested}` instead"),
            format!("{suggested}()"),
            Applicability::MachineApplicable,
        );
    true
}

fn bool_constant(expr: ast::Expr<'_>) -> Option<bool> {
    match expr {
        ast::Expr::Literal(literal) => match literal.kind() {
            ast::LiteralKind::Bool(it) => Some(it),
            _ => None,
        },
        ast::Expr::ParenExpr(paren) => bool_constant(paren.expr()?),
        ast::Expr::BlockExpr(block)
            if block.statements().next().is_none() && block.modifier().is_none() =>
        {
            bool_constant(block.tail_expr()?)
        }
        _ => None,
    }
}

fn closure_bool_constant(expr: ast::Expr<'_>) -> Option<bool> {
    let ast::Expr::ClosureExpr(closure) = expr else { return None };
    if closure.param_list()?.params().count() != 1 {
        return None;
    }
    bool_constant(closure.body()?)
}

/// Whether clippy would suggest a comparison instead: `|x| x == y` with `false`, `|x| x != y`
/// with `true`, one side the closure's parameter.
fn is_comparison<'t>(
    cx: &Cx<'_, 't>,
    recv: ast::Expr<'t>,
    map: ast::Expr<'t>,
    def_bool: bool,
) -> bool {
    let ast::Expr::ClosureExpr(closure) = map else { return false };
    let Some(params) = closure.param_list() else { return false };
    let mut params = params.params();
    let (Some(param), None) = (params.next(), params.next()) else { return false };
    let Some(ast::Pat::IdentPat(binding)) = param.pat() else { return false };
    let Some(name) = binding.name() else { return false };
    let name = name.text();
    let Some(mut body) = closure.body() else { return false };
    while let ast::Expr::BlockExpr(block) = body
        && block.statements().next().is_none()
        && block.modifier().is_none()
        && let Some(tail) = block.tail_expr()
    {
        body = tail;
    }
    let ast::Expr::BinExpr(bin) = body else { return false };
    let op = bin.op_kind();
    let eq = matches!(op, Some(ast::BinaryOp::CmpOp(ast::CmpOp::Eq { negated: false })));
    let ne = matches!(op, Some(ast::BinaryOp::CmpOp(ast::CmpOp::Eq { negated: true })));
    if !((eq && !def_bool) || (ne && def_bool)) {
        return false;
    }
    let (Some(lhs), Some(rhs)) = (bin.lhs(), bin.rhs()) else { return false };
    let is_binding = |side: ast::Expr<'_>| matches!(side, ast::Expr::PathExpr(path) if path.path().is_some_and(|it| it.as_single_name_ref().is_some_and(|it| it.text() == name)));
    if is_binding(lhs) == is_binding(rhs) {
        return false;
    }
    let other = if is_binding(lhs) { rhs } else { lhs };
    let uses_binding =
        other.syntax().descendants().filter_map(ast::NameRef::cast).any(|it| it.text() == name);
    if !is_eager(other) || uses_binding {
        return false;
    }
    if cx.ty(recv) != cx.adjusted_ty(recv) {
        return false;
    }
    let db = cx.db();
    let (Some(l_ty), Some(r_ty), Some(recv_ty)) = (cx.ty(lhs), cx.ty(rhs), cx.adjusted_ty(recv))
    else {
        return false;
    };
    let Some(partial_eq) = cx.lang_trait(hir::LangItem::PartialEq) else { return false };
    l_ty == r_ty
        && recv_ty.impls_trait(db, partial_eq, std::slice::from_ref(&recv_ty))
        && l_ty.is_copy(db)
}

/// An approximation of clippy's `switch_to_eager_eval`: what is as cheap evaluated early as
/// late, and has no effects.
fn is_eager(expr: ast::Expr<'_>) -> bool {
    match expr {
        ast::Expr::Literal(_) | ast::Expr::PathExpr(_) => true,
        ast::Expr::FieldExpr(field) => field.expr().is_some_and(is_eager),
        ast::Expr::RefExpr(it) => it.expr().is_some_and(is_eager),
        ast::Expr::ParenExpr(it) => it.expr().is_some_and(is_eager),
        ast::Expr::PrefixExpr(it) => {
            it.op_kind() == Some(ast::UnaryOp::Neg) && it.expr().is_some_and(is_eager)
        }
        _ => false,
    }
}
