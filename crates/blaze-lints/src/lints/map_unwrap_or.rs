//! `map_unwrap_or`: `.map(f).unwrap_or(a)` and `.map(f).unwrap_or_default()` on an `Option`
//! or a `Result`, which `map_or(a, f)` and `map_or_default(f)` say in one call.
//!
//! Follows clippy's `clippy_lints/src/methods/map_unwrap_or.rs` and
//! `map_unwrap_or_default.rs`, and their dispatch in `methods/mod.rs`, at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0).

use syntax::{TextRange, view as ast};

use ast::AstNode as _;

use super::utils::{self, MethodCall};
use crate::{check::Cx, diag::Applicability, msrvs};

const LINT: super::Implemented = super::Implemented::MapUnwrapOr;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, expr: ast::Expr<'t>) {
    if !cx.on(LINT)
        || cx.expansion().is_some()
        || !matches!(utils::method_name(expr), Some("unwrap_or" | "unwrap_or_default"))
    {
        return;
    }
    let Some(outer) = utils::method_call(cx, expr) else { return };
    let recv = outer.receiver;
    let Some(map) = utils::method_call(cx, recv) else { return };
    let [map_arg] = *map.args() else { return };
    if map.name != "map" {
        return;
    }
    match (outer.name, outer.args()) {
        ("unwrap_or", &[unwrap_arg]) => unwrap_or(cx, expr, recv, map, map_arg, unwrap_arg),
        ("unwrap_or_default", []) => unwrap_or_default(cx, expr, recv, map),
        _ => {}
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Suggested {
    AndThen,
    IsVariantAnd,
    Other,
}

fn unwrap_or<'t>(
    cx: &mut Cx<'_, 't>,
    expr: ast::Expr<'t>,
    unwrap_recv: ast::Expr<'t>,
    map: MethodCall<'t>,
    map_arg: ast::Expr<'t>,
    unwrap_arg: ast::Expr<'t>,
) {
    let Some(recv_ty) = cx.ty(map.receiver) else { return };
    let kind = match utils::option_or_result(cx, &recv_ty) {
        Some("Option") => "Option",
        Some("Result") if cx.conf().meets(msrvs::RESULT_MAP_OR) => "Result",
        _ => return,
    };
    let Some(unwrap_ty) = cx.ty(unwrap_arg) else { return };
    if !unwrap_ty.is_copy(cx.db()) && moved_is_used_before(cx, expr, unwrap_arg) {
        return;
    }
    let (Some(expr_range), Some(unwrap_range), Some(map_range), Some(map_arg_range)) = (
        cx.range(expr.syntax()),
        cx.range(unwrap_arg.syntax()),
        cx.range(map.name_ref.syntax()),
        cx.range(map_arg.syntax()),
    ) else {
        return;
    };
    let Some(recv_range) = cx.range(unwrap_recv.syntax()) else { return };
    let suggested = match () {
        _ if kind == "Option" && utils::is_none(cx, unwrap_arg) => Suggested::AndThen,
        _ if utils::is_bool_literal(unwrap_arg, false)
            && cx.conf().meets(msrvs::OPTION_RESULT_IS_VARIANT_AND) =>
        {
            Suggested::IsVariantAnd
        }
        _ => Suggested::Other,
    };
    let arg = match suggested {
        Suggested::AndThen => "None",
        Suggested::IsVariantAnd => "false",
        Suggested::Other => "<a>",
    };
    let suggest = match (suggested, kind) {
        (Suggested::AndThen, _) => "and_then(<f>)",
        (Suggested::IsVariantAnd, "Result") => "is_ok_and(<f>)",
        (Suggested::IsVariantAnd, _) => "is_some_and(<f>)",
        _ => "map_or(<a>, <f>)",
    };
    let article = if kind == "Option" { "an" } else { "a" };
    let message = format!("called `map(<f>).unwrap_or({arg})` on {article} `{kind}` value");
    let method = match (suggested, kind) {
        (Suggested::AndThen, _) => "and_then",
        (Suggested::IsVariantAnd, "Result") => "is_ok_and",
        (Suggested::IsVariantAnd, _) => "is_some_and",
        // An argument that needs an adjustment can have `map_or` infer the type unadjusted
        // and turn the closure down: clippy lints it with no suggestion.
        (Suggested::Other, _) if adjusted(cx, unwrap_arg) => "",
        _ => "map_or",
    };
    let unwrap_snippet = cx.snippet(unwrap_range);
    let span = cx.span(expr_range);
    let mut parts = vec![
        (cx.span(map_range), method.to_owned()),
        (cx.span(TextRange::new(recv_range.end(), expr_range.end())), String::new()),
    ];
    if suggested == Suggested::Other {
        let at = TextRange::empty(map_arg_range.start());
        parts.push((cx.span(at), format!("{unwrap_snippet}, ")));
    }
    let diagnostic = cx.lint(LINT, expr.syntax(), span, message);
    if !method.is_empty() {
        diagnostic.multipart_suggestion(
            format!("use `{suggest}` instead"),
            parts,
            Applicability::MachineApplicable,
        );
    }
}

/// Whether `arg`'s type before adjustments is not its type after them.
fn adjusted<'t>(cx: &Cx<'_, 't>, arg: ast::Expr<'t>) -> bool {
    let Some(original) = cx.ty(arg) else { return false };
    cx.adjusted_ty(arg).is_some_and(|it| it != original)
}

/// Whether a local that `unwrap_arg` moves is used ahead of it in its body: `map_or` would
/// move it first.
fn moved_is_used_before<'t>(
    cx: &Cx<'_, 't>,
    expr: ast::Expr<'t>,
    unwrap_arg: ast::Expr<'t>,
) -> bool {
    let mut locals = Vec::new();
    utils::locals_in(cx, unwrap_arg.syntax(), &mut locals);
    if locals.is_empty() {
        return false;
    }
    let Some(body) = utils::enclosing_body(expr.syntax()) else { return false };
    let Some(unwrap_at) = cx.range(unwrap_arg.syntax()) else { return true };
    body.descendants().filter_map(ast::PathExpr::cast).any(|path| {
        let before = cx
            .range(path.syntax())
            .is_some_and(|it| (it.start(), it.end()) < (unwrap_at.start(), unwrap_at.end()));
        before && utils::local_of(cx, path).is_some_and(|it| locals.contains(&it))
    })
}

fn unwrap_or_default<'t>(
    cx: &mut Cx<'_, 't>,
    expr: ast::Expr<'t>,
    recv: ast::Expr<'t>,
    map: MethodCall<'t>,
) {
    if !cx.conf().meets(msrvs::MAP_OR_DEFAULT) {
        return;
    }
    let Some(recv_ty) = cx.ty(map.receiver) else { return };
    let Some(kind) = utils::option_or_result(cx, &recv_ty) else { return };
    // `manual_is_variant_and` says more of an `Option<bool>` or `Result<bool, _>`.
    if cx.ty(expr).is_some_and(|it| it.is_bool()) {
        return;
    }
    let (Some(expr_range), Some(recv_range), Some(map_range)) =
        (cx.range(expr.syntax()), cx.range(recv.syntax()), cx.range(map.name_ref.syntax()))
    else {
        return;
    };
    let ty = if kind == "Option" { "an `Option`" } else { "a `Result`" };
    let parts = vec![
        (cx.span(TextRange::new(recv_range.end(), expr_range.end())), String::new()),
        (cx.span(map_range), "map_or_default".to_owned()),
    ];
    let span = cx.span(expr_range);
    cx.lint(
        LINT,
        expr.syntax(),
        span,
        format!("called `map(_).unwrap_or_default()` on {ty} value"),
    )
    .multipart_suggestion("use instead", parts, Applicability::MachineApplicable);
}
