//! E0618 "expected function, found `u32`": a call of something that is not callable, on the
//! callee, with "call expression requires function" on the call, the label on what the callee
//! names ("`x` has type `u32`", "`S` defined here"), the parentheses a unit struct or variant
//! does not take, the semicolon a call across lines was missing, the comma between array
//! elements, and the function a local shadows.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/callee.rs` (`report_invalid_callee`,
//! `maybe_suggest_bad_array_definition`) (MIT or Apache-2.0).

use hir::{InFile, PathResolution};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasArgList as _},
};

use super::binary_operator_cannot_be_applied::code_fact;
use crate::{
    cx::{Cx, def_range_view as def_range, signature_range_view as signature_range},
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::ExpectedFunction<'db>) -> Vec<Found> {
    let node = cx.node_view(d.call);
    let file = node.file_id;
    let at = |range: TextRange| cx.span(InFile::new(file, range));
    let call_span = cx.span_of_ptr(d.call);
    let Some(ast::Expr::CallExpr(call)) = node.value.left() else {
        let ty = cx.ty(&d.found);
        let message = format!("expected function, found `{ty}`"); // alloc: the error's message
        let mut diag = Diag::error(Some("E0618"), message, call_span.clone()); // clone: its span
        diag.span_label(call_span, "call expression requires function");
        return vec![Found::new(diag, Phase::Bodies)]; // alloc: the error found
    };
    let callee = call.expr();
    let callee_range = callee.map_or(call.syntax().text_range(), |it| it.syntax().text_range());
    let callee_span = at(callee_range);
    let args = call.arg_list().map(|it| it.args().count()).unwrap_or(0);
    let ty = cx.ty(&d.found);

    let callee_path = match callee {
        Some(ast::Expr::PathExpr(it)) => it.path(),
        _ => None,
    };
    let inner_path = match callee {
        Some(ast::Expr::CallExpr(inner)) => match inner.expr() {
            Some(ast::Expr::PathExpr(it)) => it.path(),
            _ => None,
        },
        _ => None,
    };
    let resolution = callee_path.or(inner_path).and_then(|it| cx.vsema.resolve_path(it));

    let unit = callee_path.zip(resolution.as_ref()).and_then(|(path, res)| {
        let kind = match res {
            PathResolution::Def(hir::ModuleDef::Adt(hir::Adt::Struct(it)))
                if it.kind(cx.db) == hir::StructKind::Unit =>
            {
                "struct"
            }
            PathResolution::Def(hir::ModuleDef::EnumVariant(it)) if it.kind(cx.db) == hir::StructKind::Unit => {
                "enum variant"
            }
            _ => return None,
        };
        (args == 0).then(|| (kind, path_text(path)))
    });

    let found = match &unit {
        Some((kind, path)) => format!("{kind} `{path}`"), // alloc: the error's message
        None => format!("`{ty}`"),                        // alloc: the error's message
    };
    let message = format!("expected function, found {found}"); // alloc: the error's message
    let mut diag = Diag::error(Some("E0618"), message, callee_span.clone()); // clone: its span

    if let Some((kind, path)) = &unit {
        diag.suggestion_verbose(
            at(TextRange::new(callee_range.end(), call.syntax().text_range().end())),
            // alloc: the suggestion's message
            format!("`{path}` is a unit {kind}, and does not take parentheses to be constructed"),
            "",
            Applicability::MachineApplicable,
        );
    }

    if let (Some(path), Some(PathResolution::Local(local))) = (callee_path, &resolution)
        && path.qualifier().is_none()
    {
        shadowed_functions(cx, local.name(cx.db), &mut diag);
    }

    if !bad_array(call, callee, &callee_span, &mut diag) {
        let rest = call.syntax().text();
        let start = usize::from(callee_range.end() - call.syntax().text_range().start());
        if rest.get(start..).is_some_and(|it| it.contains('\n')) {
            diag.suggestion(
                callee_span.shrink_to_hi(),
                "consider using a semicolon here to finish the statement",
                ";",
                Applicability::MaybeIncorrect,
            );
        }
        diag.span_label(call_span, "call expression requires function");
    }

    if let Some(res) = &resolution
        && let Some((span, label)) = defined_here(cx, res, &unit, inner_path, &ty)
    {
        diag.span_label(span, label);
    }
    code_fact(cx, d.call.map(|it| it.syntax_node_ptr()), &mut diag);
    vec![Found::new(diag, Phase::Bodies)] // alloc: the error found
}

/// rustc's `qpath_to_string` of a path as written: its segments, without whitespace.
fn path_text(path: ast::Path<'_>) -> String {
    path.syntax().text().split_whitespace().collect() // alloc: a path for the error
}

/// rustc's label on what the callee resolves to (`hir_res_span`).
fn defined_here<'db>(
    cx: &Cx<'_, 'db>,
    res: &PathResolution<'db>,
    unit: &Option<(&str, String)>,
    inner_path: Option<ast::Path<'_>>,
    ty: &str,
) -> Option<(crate::model::Span, String)> {
    let span = match res {
        PathResolution::Local(local) => {
            let source = local.primary_source_view(cx.db);
            cx.span(source.source.map(|it| it.syntax().text_range()))
        }
        PathResolution::Def(def) => {
            let node = match def {
                hir::ModuleDef::EnumVariant(it) => cx.vsema.source(*it)?.map(|it| def_range(it.syntax())),
                hir::ModuleDef::Adt(hir::Adt::Struct(it)) => cx.vsema.source(*it)?.map(|it| def_range(it.syntax())),
                hir::ModuleDef::Adt(hir::Adt::Union(it)) => cx.vsema.source(*it)?.map(|it| def_range(it.syntax())),
                hir::ModuleDef::Adt(hir::Adt::Enum(it)) => cx.vsema.source(*it)?.map(|it| def_range(it.syntax())),
                hir::ModuleDef::Function(it) => cx.vsema.source(*it)?.map(signature_range),
                hir::ModuleDef::Const(it) => cx.vsema.source(*it)?.map(|it| def_range(it.syntax())),
                hir::ModuleDef::Static(it) => cx.vsema.source(*it)?.map(|it| def_range(it.syntax())),
                _ => return None,
            };
            cx.span(node)
        }
        _ => return None,
    };
    let label = if let Some((kind, path)) = unit {
        format!("{kind} `{path}` defined here") // alloc: the error's label
    } else if let Some(inner) = inner_path {
        format!("`{}` defined here returns `{ty}`", path_text(inner)) // alloc: the error's label
    } else {
        match res {
            PathResolution::Local(local) => {
                // alloc: the error's label
                format!("`{}` has type `{ty}`", local.name(cx.db).display(cx.db, cx.edition))
            }
            PathResolution::Def(
                def @ (hir::ModuleDef::EnumVariant(_)
                | hir::ModuleDef::Adt(hir::Adt::Struct(_))
                | hir::ModuleDef::Function(_)
                | hir::ModuleDef::Const(_)
                | hir::ModuleDef::Static(_)),
            ) => format!("`{}` defined here", cx.def_path(*def)), // alloc: the error's label
            _ => format!("`{ty}` defined here"),                  // alloc: the error's label
        }
    };
    Some((span, label))
}

/// rustc's label on each free function with the name of the local the callee is: a walk of
/// every module's declarations, as rustc's of `hir_free_items`, comparing interned names.
fn shadowed_functions(cx: &Cx<'_, '_>, name: hir::Name, diag: &mut Diag) {
    for module in cx.krate.modules_iter(cx.db) {
        for decl in module.declarations_iter(cx.db) {
            let hir::ModuleDef::Function(function) = decl else { continue };
            if function.name(cx.db) != name {
                continue;
            }
            if let Some(source) = cx.vsema.source(function) {
                diag.span_label(
                    cx.span(source.map(signature_range)),
                    "this function of the same name is available here, but it's shadowed by the local binding",
                );
            }
        }
    }
}

/// rustc's `maybe_suggest_bad_array_definition`: `[(1, 2) (3, 4)]`, a tuple called with as many
/// arguments as it has elements, inside an array.
fn bad_array(
    call: ast::CallExpr<'_>,
    callee: Option<ast::Expr<'_>>,
    callee_span: &crate::model::Span,
    diag: &mut Diag,
) -> bool {
    let Some(ast::Expr::TupleExpr(tuple)) = callee else { return false };
    let in_array = call.syntax().parent().is_some_and(|it| ast::ArrayExpr::can_cast(it.kind()));
    let args = call.arg_list().map(|it| it.args().count()).unwrap_or(0);
    if !in_array || args != tuple.fields().count() {
        return false;
    }
    diag.suggestion(
        callee_span.shrink_to_hi(),
        "consider separating array elements with a comma",
        ",",
        Applicability::MaybeIncorrect,
    );
    true
}
