//! `single_char_add_str`: `s.push_str("a")` or `s.insert_str(i, "a")`, which `push('a')` and
//! `insert(i, 'a')` say.
//!
//! Follows clippy's `clippy_lints/src/methods/single_char_add_str.rs` and `clippy_utils`'s
//! `str_literal_to_char_literal` at rust-lang/rust 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0
//! (MIT or Apache-2.0).

use hir::AsAssocItem as _;
use syntax::view as ast;

use ast::{AstNode as _, HasArgList as _};

use super::utils;
use crate::{check::Cx, diag::Applicability};

const LINT: super::Implemented = super::Implemented::SingleCharAddStr;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, expr: ast::Expr<'t>) {
    if !cx.on(LINT) || !matches!(utils::method_name(expr), Some("push_str" | "insert_str")) {
        return;
    }
    let ast::Expr::MethodCallExpr(method) = expr else { return };
    let Some(call) = utils::method_call(cx, expr) else { return };
    let (short_name, arg, index) = match (call.name, call.args()) {
        ("push_str", &[arg]) => ("push", arg, None),
        ("insert_str", &[index, arg]) => ("insert", arg, Some(index)),
        _ => return,
    };
    // What clippy suggests of the argument, asked of its syntax before the method's type.
    let literal = char_literal(cx, arg);
    let to_string = match literal {
        Some(_) => None,
        None => match char_to_string(arg) {
            Some(it) => Some(it),
            None => return,
        },
    };
    if !is_string_method(cx, method) {
        return;
    }
    let (Some(range), Some(receiver)) = (cx.range(expr.syntax()), cx.range(call.receiver.syntax()))
    else {
        return;
    };
    let base = cx.snippet(receiver);
    let index = match index {
        Some(index) => match cx.range(index.syntax()) {
            Some(it) => format!("{}, ", cx.snippet(it)),
            None => return,
        },
        None => String::new(),
    };
    let (message, help, extension) = if let Some(literal) = literal {
        (
            format!("calling `{short_name}_str()` using a single-character string literal"),
            format!("consider using `{short_name}` with a character literal"),
            literal,
        )
    } else if let Some(char_expr) = to_string
        && let Some(ty) = cx.ty(char_expr)
        && let Some(at) = cx.range(char_expr.syntax())
    {
        let is_ref_char = ty.as_reference().is_some_and(|(it, _)| it.is_char());
        if !is_ref_char && !ty.is_char() {
            return;
        }
        let snippet = cx.snippet(at);
        (
            format!("calling `{short_name}_str()` using a single-character converted to string"),
            format!("consider using `{short_name}` without `to_string()`"),
            if is_ref_char { format!("*{snippet}") } else { snippet.to_owned() },
        )
    } else {
        return;
    };
    let span = cx.span(range);
    cx.lint(LINT, expr.syntax(), span.clone(), message).suggestion(
        span,
        help,
        format!("{base}.{short_name}({index}{extension})"),
        Applicability::MachineApplicable,
    );
}

/// `&c.to_string()`: what is turned into a string, `c`.
fn char_to_string(arg: ast::Expr<'_>) -> Option<ast::Expr<'_>> {
    let ast::Expr::RefExpr(borrow) = arg else { return None };
    if borrow.raw_token().is_some() {
        return None;
    }
    let Some(ast::Expr::MethodCallExpr(to_string)) = borrow.expr() else { return None };
    if to_string.name_ref().is_none_or(|it| it.text() != "to_string")
        || to_string.arg_list().is_none_or(|it| it.args().next().is_some())
    {
        return None;
    }
    to_string.receiver()
}

/// Whether the method `method` calls is `String`'s own.
fn is_string_method<'t>(cx: &Cx<'_, 't>, method: ast::MethodCallExpr<'t>) -> bool {
    let db = cx.db();
    let Some(function) = cx.method(method) else { return false };
    let implementing = function.as_assoc_item(db).and_then(|it| it.implementing_ty(db));
    let string = match cx.std_def(&["alloc", "string", "String"]) {
        Some(hir::ModuleDef::Adt(it)) => it,
        _ => return false,
    };
    implementing.and_then(|it| it.as_adt()) == Some(string)
}

/// clippy's `str_literal_to_char_literal`: the character literal a string literal of one
/// character is.
fn char_literal<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> Option<String> {
    let ast::Expr::Literal(literal) = expr else { return None };
    let ast::LiteralKind::String(string) = literal.kind() else { return None };
    if string.value().ok()?.chars().count() != 1 {
        return None;
    }
    let snippet = cx.snippet(cx.range(literal.syntax())?);
    let hashes = snippet.strip_prefix('r').map(|it| it.len() - it.trim_start_matches('#').len());
    let ch = match hashes {
        Some(hashes) => snippet.get(hashes + 2..snippet.len() - 1 - hashes)?,
        None => snippet.get(1..snippet.len() - 1)?,
    };
    Some(format!(
        "'{}'",
        match ch {
            "'" => "\\'",
            r"\" => "\\\\",
            "\\\"" => "\"",
            _ => ch,
        }
    ))
}
