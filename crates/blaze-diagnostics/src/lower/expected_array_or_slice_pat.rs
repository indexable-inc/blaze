//! E0529: a slice pattern matched against what is neither an array nor a slice, with rustc's
//! suggestions to slice the scrutinee (`v[..]`) or to call `as_deref` on it when it is an
//! `Option` or a `Result` of what derefs to one.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/pat.rs` (`check_pat_slice`,
//! `error_expected_array_or_slice`, `is_slice_or_array_or_vector`, and `TopInfo` as
//! `check_decl` and `check_match` give it) and `compiler/rustc_hir_typeck/src/diagnostics.rs`
//! (`ExpectedArrayOrSlice`, `AsDerefSuggestion`, `SlicingSuggestion`) (MIT or Apache-2.0).
//! The scrutinee rustc's `TopInfo` holds is found again from the syntax: the expression of the
//! `match`, of a `let` with no type, of an `if let`, or the right of a destructuring
//! assignment. A `for` loop's pattern has the call of `next` rustc desugars it to as its
//! scrutinee, which is in no source: rustc's suggestion there is not given.
//!
//! The autoderef, the one step that asks the trait solver, runs last and only when a
//! suggestion is still possible.

use base_db::LangCrateOrigin;
use hir::InFile;
use intern::sym;
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _, BinaryOp},
};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::ExpectedArrayOrSlicePat<'db>) -> Vec<Found> {
    let span = cx.span_of_ptr(d.pat);
    let ty = cx.ty(&d.found);
    // alloc: the error's message
    let message = format!("expected an array or slice, found `{ty}`");
    let mut diag = Diag::error(Some("E0529"), message, span.clone()); // clone: the error's span
    // alloc: the error's label
    diag.span_label(span, format!("pattern cannot match with input type `{ty}`"));
    suggest(cx, d, &mut diag);
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Bodies)] // alloc: the error found
}

/// The scrutinee of the pattern a slice pattern is in, and whether the slice pattern is that
/// pattern itself (rustc's `current_depth <= 1`).
struct Origin<'db> {
    expr: InFile<ast::Expr<'db>>,
    top_level: bool,
}

fn suggest<'db>(cx: &Cx<'_, 'db>, d: &hir::ExpectedArrayOrSlicePat<'db>, diag: &mut Diag) {
    let found = &d.found;
    if let Some((inner, _)) = found.as_reference()
        && (inner.is_array() || inner.is_slice())
    {
        return;
    }
    let Some(origin) = origin(cx, cx.syntax_view(d.pat.map(|it| it.syntax_node_ptr()))) else { return };
    let Some(root) = cx.vsema.type_of_expr(origin.expr.value).map(|it| it.original) else {
        return;
    };
    let (sequence, root) = slice_or_array_or_vector(cx, root);
    let as_deref = matches!(root.as_adt(), Some(hir::Adt::Enum(it)) if is_option_or_result(cx, it));
    let slicing = sequence && origin.top_level;
    if !(as_deref || slicing) || !found.autoderef(cx.db).any(|it| it.is_slice() || it.is_array()) {
        return;
    }
    let at = cx.span(origin.expr.map(|it| it.syntax().text_range())).shrink_to_hi();
    // At most one holds: `as_deref` wants an enum at the root, slicing a sequence.
    let (message, code) = match as_deref {
        true => ("consider using `as_deref` here", ".as_deref()"),
        false => ("consider slicing here", "[..]"),
    };
    diag.suggestion_verbose(at, message, code, Applicability::MaybeIncorrect);
}

/// rustc's `is_slice_or_array_or_vector`: whether `ty` behind its references is a `Vec`, an
/// array or a slice, and `ty` without them.
fn slice_or_array_or_vector<'db>(cx: &Cx<'_, 'db>, mut ty: hir::Type<'db>) -> (bool, hir::Type<'db>) {
    while let Some((inner, _)) = ty.as_reference() {
        ty = inner;
    }
    let vec = match ty.as_adt() {
        Some(hir::Adt::Struct(it)) => {
            it.name(cx.db) == sym::Vec && in_lang_crate(cx, it.module(cx.db), LangCrateOrigin::Alloc)
        }
        _ => false,
    };
    (vec || ty.is_slice() || ty.is_array(), ty)
}

/// core's `Option` or `Result`: rustc's diagnostic items of them.
fn is_option_or_result(cx: &Cx<'_, '_>, it: hir::Enum) -> bool {
    let name = it.name(cx.db);
    (name == sym::Option || name == sym::Result)
        && in_lang_crate(cx, it.module(cx.db), LangCrateOrigin::Core)
}

fn in_lang_crate(cx: &Cx<'_, '_>, module: hir::Module, krate: LangCrateOrigin) -> bool {
    module.krate(cx.db).lang_origin(cx.db) == Some(krate)
}

/// rustc's `TopInfo` of the pattern `node` is in: the expression it matches, and whether
/// `node` is the whole pattern, found by going up through the patterns (or, of a
/// destructuring assignment, the expressions) around it. Each step matches on the node's kind;
/// only the node the walk ends at is cast.
fn origin<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> Option<Origin<'db>> {
    let is_pat = ast::Pat::can_cast(node.value.kind());
    let mut depth = 0u32;
    let mut prev = node.value.text_range();
    for ancestor in cx.vsema.ancestors_with_macros_file(node).skip(1) {
        let it = ancestor.value;
        let top_level = depth == 0;
        let found =
            |expr: Option<ast::Expr<'db>>| expr.map(|expr| Origin { expr: ancestor.with_value(expr), top_level });
        let kind = it.kind();
        if is_pat {
            match kind {
                SyntaxKind::MATCH_ARM => {
                    return found(it.parent()?.parent().and_then(ast::MatchExpr::cast)?.expr());
                }
                SyntaxKind::LET_STMT => {
                    let stmt = ast::LetStmt::cast(it)?;
                    return if stmt.ty().is_some() { None } else { found(stmt.initializer()) };
                }
                SyntaxKind::LET_EXPR => return found(ast::LetExpr::cast(it)?.expr()),
                SyntaxKind::PAREN_PAT
                | SyntaxKind::MACRO_PAT
                | SyntaxKind::RECORD_PAT_FIELD
                | SyntaxKind::RECORD_PAT_FIELD_LIST
                | SyntaxKind::MACRO_CALL
                | SyntaxKind::TOKEN_TREE => {}
                _ if ast::Pat::can_cast(kind) => depth += 1,
                _ => return None,
            }
        } else {
            match kind {
                SyntaxKind::BIN_EXPR => {
                    let bin = ast::BinExpr::cast(it)?;
                    if bin.op_kind() != Some(BinaryOp::Assignment { op: None })
                        || bin.lhs().is_none_or(|lhs| lhs.syntax().text_range() != prev)
                    {
                        return None;
                    }
                    return found(bin.rhs());
                }
                SyntaxKind::PAREN_EXPR
                | SyntaxKind::ARG_LIST
                | SyntaxKind::RECORD_EXPR_FIELD
                | SyntaxKind::RECORD_EXPR_FIELD_LIST => {}
                SyntaxKind::TUPLE_EXPR
                | SyntaxKind::ARRAY_EXPR
                | SyntaxKind::CALL_EXPR
                | SyntaxKind::RECORD_EXPR => depth += 1,
                _ => return None,
            }
        }
        prev = it.text_range();
    }
    None
}
