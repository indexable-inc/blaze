//! `io_other_error`: `std::io::Error::new(ErrorKind::Other, e)`, which
//! `std::io::Error::other(e)` says shorter.
//!
//! Follows clippy's `clippy_lints/src/methods/io_other_error.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0).

use hir::{AsAssocItem as _, PathResolution};
use syntax::{TextRange, view as ast};

use ast::{AstNode as _, HasArgList as _};

use super::utils;
use crate::{check::Cx, diag::Applicability, msrvs};

const LINT: super::Implemented = super::Implemented::IoOtherError;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, expr: ast::Expr<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let ast::Expr::CallExpr(call) = expr else { return };
    let Some(ast::Expr::PathExpr(callee)) = call.expr() else { return };
    let Some(path) = callee.path() else { return };
    // `Type::new`, as rustc's type-relative path: an associated function is named as declared.
    let Some(segment) = path.segment().and_then(|it| it.name_ref()) else { return };
    if path.qualifier().is_none() || !matches!(segment.text(), "new" | "r#new") {
        return;
    }
    let Some(arg_list) = call.arg_list() else { return };
    let mut args = arg_list.args();
    let (Some(error_kind), Some(error), None) = (args.next(), args.next(), args.next()) else {
        return;
    };
    let (Some(whole), Some(kind_range), Some(error_range)) =
        (cx.range(call.syntax()), cx.range(error_kind.syntax()), cx.range(error.syntax()))
    else {
        return;
    };
    if !is_io_error_new(cx, path) || !is_other_kind(cx, utils::expr_or_init(cx, error_kind)) {
        return;
    }
    if !cx.conf().meets(msrvs::IO_ERROR_OTHER) {
        return;
    }
    let Some(segment) = cx.range(segment.syntax()) else { return };
    let parts = vec![
        (cx.span(segment), "other".to_owned()),
        (cx.span(TextRange::new(kind_range.start(), error_range.start())), String::new()),
    ];
    let span = cx.span(whole);
    cx.lint(LINT, call.syntax(), span, "this can be `std::io::Error::other(_)`")
        .multipart_suggestion(
            "use `std::io::Error::other`",
            parts,
            Applicability::MachineApplicable,
        );
}

fn is_io_error_new<'t>(cx: &Cx<'_, 't>, path: ast::Path<'t>) -> bool {
    let db = cx.db();
    let Some(PathResolution::Def(hir::ModuleDef::Function(function))) = cx.path(path) else {
        return false;
    };
    if function.name(db).as_str() != "new" {
        return false;
    }
    let Some(hir::ModuleDef::Adt(io_error)) = cx.std_def(&["std", "io", "Error"]) else {
        return false;
    };
    let implementing = function.as_assoc_item(db).and_then(|it| it.implementing_ty(db));
    implementing.and_then(|it| it.as_adt()) == Some(io_error)
}

/// `expr` is a path ending `ErrorKind::Other`, `std::io::ErrorKind` named in it.
fn is_other_kind<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> bool {
    let ast::Expr::PathExpr(path) = expr else { return false };
    let Some(path) = path.path() else { return false };
    let Some(qualifier) = path.qualifier() else { return false };
    if path.segment().and_then(|it| it.name_ref()).is_none_or(|it| it.text() != "Other") {
        return false;
    }
    let Some(hir::ModuleDef::Adt(kind)) = cx.std_def(&["std", "io", "ErrorKind"]) else {
        return false;
    };
    matches!(
        cx.path(qualifier),
        Some(PathResolution::Def(hir::ModuleDef::Adt(it))) if it == kind
    )
}
