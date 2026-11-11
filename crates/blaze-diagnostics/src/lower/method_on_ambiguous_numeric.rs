//! E0689: a method called on a number whose type is not decided where the method is looked up
//! (rustc's `{integer}` and `{float}`), as in `let x = 2; x.pow(2)`. A method call does not
//! wait for a later use of the number to decide its type, so the call is an error whatever
//! the rest of the body makes of the number, and the message names the type as it was at the
//! call, never the one the number became afterwards. When no numeric type and no trait has a
//! method of the name, it is E0599 of the same `{integer}`.
//!
//! When it is reported and where its fix goes are rustc's
//! (`compiler/rustc_hir_typeck/src/method/suggest.rs`,
//! `report_failed_method_call_on_numerical_infer_var`; MIT or Apache-2.0). The wording is
//! blaze's, and the type the fix writes is the one the rest of the body decided for the
//! number, where rustc always writes `i32` or `f32`.

use either::Either;
use hir::{InFile, PathResolution};
use hir_ty::NumericVar;
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasName as _},
};

use crate::{
    cx::Cx,
    lower::unresolved_method_call::{NoMatch, all_traits},
    model::{Applicability, Diag},
};

/// The error of `call`, in `file`, whose method `no_match` found none of on a receiver that
/// was the numeric variable `numeric` there. `d.receiver` is what the variable became.
pub(crate) fn lower<'db>(
    cx: &Cx<'_, 'db>,
    d: &hir::UnresolvedMethodCall<'db>,
    numeric: NumericVar,
    call: ast::MethodCallExpr<'db>,
    file: hir::HirFileId,
    no_match: &NoMatch<'db>,
) -> Diag {
    let name = no_match.name();
    // The variable as rustc writes it, what it is, and the type rustc's fix names.
    let (var, what, rustc_names) = match numeric {
        NumericVar::Integer => ("{integer}", "an integer", "i32"),
        NumericVar::Float => ("{float}", "a float", "f32"),
    };
    // clone: on the error path, for an error hir found
    let at = no_match.name_span.clone();
    if !a_numeric_type_or_a_trait_has(cx, &d.receiver, &no_match.name) {
        // alloc: the error's text, for an error hir found
        let message =
            format!("no method named `{name}` found for type `{var}` in the current scope");
        // clone: on the error path, for an error hir found
        let mut diag = Diag::error(Some("E0599"), message, at.clone());
        // alloc: the error's text, for an error hir found
        diag.span_label(at, format!("method not found in `{var}`"));
        return diag;
    }
    // alloc: the error's text, for an error hir found
    let message =
        format!("method `{name}` is called on `{var}`, {what} whose type is not decided yet");
    // clone: on the error path, for an error hir found
    let mut diag = Diag::error(Some("E0689"), message, at.clone());
    diag.span_label(
        at,
        "a method is looked up on the type known here; a later use does not decide it",
    );
    let ty = match d.receiver.is_int_or_uint() || d.receiver.is_float() {
        true => cx.ty(&d.receiver),
        false => rustc_names.to_owned(), // alloc: the error's text, for an error hir found
    };
    let _ = suggest_a_type(cx, &mut diag, call, file, &ty);
    diag
}

/// Whether a method named `name` is on some numeric primitive type (looked up from the crate
/// `receiver` is of), or some trait has a function or constant of the name: rustc's
/// `found_candidate`, without which the call is an ordinary E0599.
fn a_numeric_type_or_a_trait_has<'db>(
    cx: &Cx<'_, 'db>,
    receiver: &hir::Type<'db>,
    name: &hir::Name,
) -> bool {
    let named = |item: hir::AssocItem| item.name(cx.db).as_ref() == Some(name);
    let on_a_numeric_type = receiver.numeric_primitives(cx.db).iter().any(|ty| {
        let found = ty.iterate_assoc_items(cx.db, |item| {
            let method = matches!(item, hir::AssocItem::Function(f) if f.has_self_param(cx.db));
            (method && named(item)).then_some(())
        });
        found.is_some()
    });
    on_a_numeric_type
        || all_traits(cx).into_iter().any(|it| {
            it.items(cx.db).into_iter().any(|item| {
                matches!(item, hir::AssocItem::Function(_) | hir::AssocItem::Const(_))
                    && named(item)
            })
        })
}

/// The fix: a suffix `ty` on a literal receiver (`2_i32`), or the type `ty` on the `let` that
/// declares the receiver (`let x: i32 = 2`, also in place of `: _`). A binding declared
/// another way (a pattern's part, a `for`'s, one a macro wrote) is labelled.
fn suggest_a_type<'db>(
    cx: &Cx<'_, 'db>,
    diag: &mut Diag,
    call: ast::MethodCallExpr<'db>,
    file: hir::HirFileId,
    ty: &str,
) -> Option<()> {
    let mut receiver = call.receiver()?;
    while let ast::Expr::ParenExpr(it) = receiver {
        receiver = it.expr()?;
    }
    match receiver {
        ast::Expr::Literal(literal) => {
            let text = literal.syntax().text();
            // `2.` and a suffix are `2._f32`, a field access: the suffix goes on `2`.
            let text = text.strip_suffix('.').unwrap_or(text);
            diag.suggestion_verbose(
                cx.span(InFile::new(file, literal.syntax().text_range())),
                "give the literal its type with a suffix",
                format!("{text}_{ty}"), // alloc: the error's text, for an error hir found
                Applicability::MaybeIncorrect,
            );
        }
        ast::Expr::PathExpr(path) => {
            let Some(PathResolution::Local(local)) = cx.vsema.resolve_path(path.path()?) else {
                return None;
            };
            let source = local.primary_source_view(cx.db);
            let declared_in = source.source.file_id;
            let Either::Left(pat) = source.source.value else { return None };
            let name = pat.name()?;
            let name = name.text();
            let pat_range = pat.syntax().text_range();
            let stmt = pat.syntax().parent().and_then(ast::LetStmt::cast);
            match stmt {
                Some(stmt) if !declared_in.is_macro() => {
                    // After the pattern, over a `: _` when the `let` has one.
                    let end = stmt.ty().map_or(pat_range.end(), |it| it.syntax().text_range().end());
                    diag.suggestion_verbose(
                        cx.span(InFile::new(declared_in, TextRange::new(pat_range.end(), end))),
                        // alloc: the error's text, for an error hir found
                        format!("give `{name}` its type where it is declared"),
                        format!(": {ty}"), // alloc: the error's text, for an error hir found
                        Applicability::MaybeIncorrect,
                    );
                }
                _ => {
                    diag.span_label(
                        cx.span(InFile::new(declared_in, pat_range)),
                        // alloc: the error's text, for an error hir found
                        format!("give `{name}` a type, such as `{ty}`"),
                    );
                }
            }
        }
        _ => {}
    }
    Some(())
}
