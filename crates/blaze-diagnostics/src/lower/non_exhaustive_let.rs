//! E0005: a refutable pattern where one must be irrefutable: a `let`, a parameter, a `for`
//! loop or a destructuring assignment.
//!
//! Follows rustc's `compiler/rustc_mir_build/src/thir/pattern/check_match.rs`
//! (`check_binding_is_irrefutable`, `is_const_pat_that_looks_like_binding`,
//! `report_adt_defined_here`), `compiler/rustc_mir_build/src/errors.rs` (`PatternNotCovered`,
//! `Inform`, `InterpretedAsConst`, `InterpretedAsConstSugg`, `SuggestLet`,
//! `MiscPatternSuggestion`), `compiler/rustc_pattern_analysis/src/errors.rs` (`Uncovered`) and,
//! for what a destructuring assignment binds, `compiler/rustc_ast_lowering/src/expr.rs`
//! (`destructure_assign`) (MIT or Apache-2.0).

use either::Either;
use hir::{Adt, BindingOrigin, ModuleDef, PathResolution, StructKind};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasArgList as _},
};

use super::missing_match_arms::{DefinedHere, adt_fact, all_chars, const_like_binding, joined, plural};
use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::NonExhaustiveLet<'db>) -> Vec<Found> {
    let pat = cx.node_view(d.pat);
    let span = cx.span_of_ptr(d.pat);
    let origin = match d.origin {
        BindingOrigin::Let | BindingOrigin::Assignment => "local binding",
        BindingOrigin::FnParam => "function argument",
        BindingOrigin::ClosureParam => "closure argument",
        BindingOrigin::ForLoop => "`for` loop binding",
    };
    // rustc has the statement's span (`sp`) of a `let`, and of the one an assignment lowers to.
    let in_statement = matches!(d.origin, BindingOrigin::Let | BindingOrigin::Assignment);

    // alloc: clone: the error's parts, for an error hir found
    let mut diag = Diag::error(Some("E0005"), format!("refutable pattern in {origin}"), span.clone());
    let count = d.witnesses.len();
    // alloc: clone: the error's parts, for an error hir found
    diag.span_label(span.clone(), format!("pattern{} {} not covered", plural(count), joined(&d.witnesses)));
    if in_statement {
        diag.note("`let` bindings require an \"irrefutable pattern\", like a `struct` or an `enum` with only one variant");
        diag.note("for more information, visit https://doc.rust-lang.org/book/ch19-02-refutability.html");
    }

    let constant = d.interpreted_as_const.and_then(|konst| const_like_binding(cx, pat.value, konst));
    if let Some((name, def)) = &constant {
        diag.span_label(
            // clone: the error's parts, for an error hir found
            def.clone(),
            // alloc: the error's text, for an error hir found
            format!(
                "missing patterns are not covered because `{name}` is interpreted as a constant pattern, not a new variable"
            ),
        );
        diag.suggestion_verbose(
            // clone: the error's parts, for an error hir found
            span.clone(),
            "introduce a variable instead",
            // alloc: the error's text, for an error hir found
            format!("{name}_var"),
            Applicability::MaybeIncorrect,
        );
    }
    if let Some(here) = DefinedHere::of(cx, &d.pattern_ty, &d.variants, false) {
        diag.span_note(here.span(false), here.message());
    }
    if d.privately_uninhabited
        && let Some(first) = d.witnesses.first()
    {
        // alloc: the error's text, for an error hir found
        diag.note(format!(
            "pattern `{first}` is currently uninhabited, but this variant contains private fields which may become inhabited in the future"
        ));
    }
    // alloc: the error's text, for an error hir found
    diag.note(format!("the matched value is of type `{}`", cx.ty(&d.pattern_ty)));

    if in_statement
        && d.has_initializer
        && constant.is_none()
        && let Some((whole, binds)) = statement(cx, pat.value)
    {
        let whole = cx.span(pat.with_value(whole));
        let (start, end) = (whole.shrink_to_lo(), whole.shrink_to_hi());
        let variants = match count {
            1 => "variant that isn't",
            _ => "variants that aren't",
        };
        match binds {
            true => {
                diag.suggestion(
                    end,
                    // alloc: the error's text, for an error hir found
                    format!("you might want to use `let...else` to handle the {variants} matched"),
                    " else { todo!() }",
                    Applicability::HasPlaceholders,
                );
            }
            false => {
                diag.multipart_suggestion(
                    // alloc: the error's text, for an error hir found
                    format!("you might want to use `if let` to ignore the {variants} matched"),
                    // alloc: the error's parts, for an error hir found
                    vec![(start, "if ".to_owned()), (end, " { todo!() }".to_owned())],
                    Applicability::HasPlaceholders,
                );
            }
        }
    }
    if constant.is_none()
        && let Either::Right(ast::Pat::LiteralPat(literal)) = pat.value
        && all_chars(literal.syntax(), |c| c.is_ascii_digit())
    {
        diag.suggestion(
            span.shrink_to_lo(),
            "alternatively, you could prepend the pattern with an underscore to define a new named variable; identifiers cannot begin with digits",
            "_",
            Applicability::MaybeIncorrect,
        );
    }

    adt_fact(cx, &mut diag, &d.pattern_ty);
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Mir)]
}

/// rustc's span of the statement the pattern is of, and whether the pattern binds anything
/// (`each_binding`). A `let`'s runs from `let` (or `super`) to the end of its initializer; an
/// assignment's is the whole assignment, which rustc lowers to a `let`.
fn statement<'db>(cx: &Cx<'_, 'db>, pat: Either<ast::Expr<'db>, ast::Pat<'db>>) -> Option<(TextRange, bool)> {
    match pat {
        Either::Right(pat) => {
            let stmt = pat.syntax().parent().and_then(ast::LetStmt::cast)?;
            let init = stmt.initializer()?;
            let keyword = stmt.super_token().or_else(|| stmt.let_token());
            let lo = keyword.map_or(stmt.syntax().text_range().start(), |it| it.text_range().start());
            let hi = init.syntax().text_range().end();
            let binds = pat
                .syntax()
                .descendants()
                .filter_map(ast::IdentPat::cast)
                .any(|it| cx.vsema.resolve_bind_pat_to_const(it).is_none());
            Some((TextRange::new(lo, hi.max(lo)), binds))
        }
        Either::Left(lhs) => {
            let assignment = lhs.syntax().parent().and_then(ast::BinExpr::cast)?;
            Some((assignment.syntax().text_range(), assignee_binds(cx, lhs)))
        }
    }
}

/// Whether the assignee `expr` lowers to a pattern with a binding: every part of it that
/// `destructure_assign` does not make a pattern of is a place, bound and then assigned.
fn assignee_binds<'db>(cx: &Cx<'_, 'db>, expr: ast::Expr<'db>) -> bool {
    match expr {
        ast::Expr::UnderscoreExpr(_) | ast::Expr::RangeExpr(_) => false,
        ast::Expr::ParenExpr(it) => it.expr().is_some_and(|it| assignee_binds(cx, it)),
        ast::Expr::TupleExpr(it) => it.fields().any(|it| assignee_binds(cx, it)),
        ast::Expr::ArrayExpr(it) => it.exprs().any(|it| assignee_binds(cx, it)),
        ast::Expr::CallExpr(it) => it.arg_list().is_some_and(|args| args.args().any(|it| assignee_binds(cx, it))),
        ast::Expr::RecordExpr(it) => it.record_expr_field_list().is_some_and(|list| {
            list.fields().any(|field| field.expr().is_none_or(|it| assignee_binds(cx, it)))
        }),
        ast::Expr::PathExpr(it) => !it.path().is_some_and(|path| is_unit_ctor(cx, path)),
        _ => true,
    }
}

/// `extract_unit_struct_path`: the path names a unit struct or a unit variant.
fn is_unit_ctor<'db>(cx: &Cx<'_, 'db>, path: ast::Path<'db>) -> bool {
    match cx.vsema.resolve_path(path) {
        Some(PathResolution::Def(ModuleDef::EnumVariant(it))) => it.kind(cx.db) == StructKind::Unit,
        Some(PathResolution::Def(ModuleDef::Adt(Adt::Struct(it)))) => it.kind(cx.db) == StructKind::Unit,
        _ => false,
    }
}
