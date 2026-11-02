//! `useless_format`: `format!` of a string literal alone, or of one string argument alone,
//! which `.to_string()` says.
//!
//! Follows clippy's `clippy_lints/src/format.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0).

use syntax::view::{self as ast, AstNode as _};

use super::{
    format::{self, Arg, Trait},
    utils,
};
use crate::{check::Cx, diag::Applicability, sugg::Sugg};

const LINT: super::Implemented = super::Implemented::UselessFormat;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, call: ast::MacroCall<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(macro_expr) = call.syntax().parent().filter(|it| ast::MacroExpr::can_cast(it.kind()))
    else {
        return;
    };
    let Some(format) = format::of(cx, call) else { return };
    let Some(call_site) = cx.range(call.syntax()) else { return };
    if !format.std || format.name != "format" {
        return;
    }
    // `&format!(..)`, which the argument or the literal itself would serve.
    let borrowed = past_parens(macro_expr)
        .parent()
        .and_then(ast::RefExpr::cast)
        .filter(|it| it.raw_token().is_none() && it.mut_token().is_none())
        .and_then(|it| cx.range(it.syntax()));
    let (span, expl, sugg) = match (format.placeholder_count, format.arg_count) {
        (0, 0) if format.template.is_empty() => {
            (call_site, "`String::new()`", "String::new()".to_owned())
        }
        (0, 0) => {
            let snippet = cx.snippet(format.template_range);
            let expanded = snippet.replace("{{", "{").replace("}}", "}");
            match borrowed {
                Some(parent) => (parent, "the string literal directly", expanded),
                None => (call_site, "`.to_string()`", format!("{expanded}.to_string()")),
            }
        }
        (1, args) if args <= 1 => {
            let Some(placeholder) = format.placeholders().next() else { return };
            if placeholder.format_trait != Trait::Display
                || placeholder.width.is_some()
                || placeholder.precision.is_some()
                || !only_placeholder(&format.template)
            {
                return;
            }
            let value = match (placeholder.arg, args) {
                (Arg::Index(0), 1) => match format.arg(0) {
                    Some(value) => Value::Expr(value),
                    None => return,
                },
                (Arg::Captured(name), 0) => Value::Captured(name),
                _ => return,
            };
            let Some(ty) = value.ty(cx, call.syntax()) else { return };
            let peeled = ty.strip_references();
            let string = match cx.std_def(&["alloc", "string", "String"]) {
                Some(hir::ModuleDef::Adt(it)) => Some(it),
                _ => None,
            };
            if !(peeled.is_str() || (string.is_some() && peeled.as_adt() == string)) {
                return;
            }
            let is_new_string = match value {
                Value::Expr(ast::Expr::BinExpr(_)) => true,
                Value::Expr(ast::Expr::MethodCallExpr(it)) => {
                    it.name_ref().is_some_and(|it| it.text() == "to_string")
                }
                _ => false,
            };
            let text = |cx: &Cx<'_, 't>| match value {
                Value::Expr(expr) => utils::sugg(cx, expr),
                Value::Captured(name) => Sugg::NonParen(name.to_owned()),
            };
            match borrowed {
                Some(parent) if !is_new_string && peeled.is_str() => {
                    (parent, "the argument directly", text(cx).to_string())
                }
                _ if is_new_string => {
                    let Value::Expr(expr) = value else { return };
                    (call_site, "`.to_string()`", cx.source_of(expr.syntax()).into_owned())
                }
                _ => {
                    (call_site, "`.to_string()`", format!("{}.to_string()", text(cx).maybe_paren()))
                }
            }
        }
        _ => return,
    };
    let span = cx.span(span);
    cx.lint(LINT, macro_expr, span.clone(), "useless use of `format!`").suggestion(
        span,
        format!("consider using {expl}"),
        sugg,
        Applicability::MachineApplicable,
    );
}

/// What the one placeholder of a `format!` formats.
#[derive(Clone, Copy)]
enum Value<'t, 'a> {
    Expr(ast::Expr<'t>),
    /// A name the template captures from the scope.
    Captured(&'a str),
}

impl<'t> Value<'t, '_> {
    fn ty<'q>(self, cx: &Cx<'q, 't>, call: ast::Node<'t>) -> Option<hir::Type<'q>> {
        match self {
            Value::Expr(expr) => cx.ty(expr),
            Value::Captured(name) => match cx.resolve_name(call, name)? {
                hir::PathResolution::Local(local) => Some(local.ty(cx.db())),
                _ => None,
            },
        }
    }
}

/// Whether `template` is one placeholder and nothing else, with no format options.
fn only_placeholder(template: &str) -> bool {
    let Some(inner) = template.strip_prefix('{').and_then(|it| it.strip_suffix('}')) else {
        return false;
    };
    if inner.contains(['{', '}']) {
        return false;
    }
    inner.split_once(':').is_none_or(|(_, spec)| spec.is_empty())
}

fn past_parens(mut node: ast::Node<'_>) -> ast::Node<'_> {
    while let Some(parent) = node.parent()
        && ast::ParenExpr::can_cast(parent.kind())
    {
        node = parent;
    }
    node
}
