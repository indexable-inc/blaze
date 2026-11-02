//! rustc's late lint passes of casts and borrows of statics and places in bodies.
//!
//! - `static_mut_refs`: `StaticMutRefs` (`static_mut_refs`).
//! - `function_casts`: `FunctionCastsAsInteger` (`function_casts_as_integer`).
//! - `provenance`: `ImplicitProvenanceCasts` (`implicit_provenance_casts`).
//! - `raw_borrows`: `RawBorrowsViaReferences` (`raw_borrows_via_references`).
//!
//! What they share: spans as rustc's HIR has them (an expression's with the parentheses around
//! it, a cast's type), rustc's `Span::find_ancestor_in_same_ctxt` and
//! `can_be_used_for_suggestions`, and types printed as rustc prints them.
//!
//! Follows rustc's `compiler/rustc_ast_lowering/src/expr.rs` (`ExprKind::Paren`: the span of
//! what parentheses hold is theirs), `compiler/rustc_ast_lowering/src/lib.rs` (`TyKind::Paren`
//! lowers to what it holds) and `compiler/rustc_span/src/lib.rs` (MIT or Apache-2.0).

pub(crate) mod function_casts;
pub(crate) mod provenance;
pub(crate) mod raw_borrows;
pub(crate) mod static_mut_refs;

use hir::InFile;
use hir_def::hir::ExprId;
use syntax::view::{self as ast, AstNode};

use super::util::{from_expansion, span as expr_span};
use crate::{
    lints::bodies::BodyCx,
    model::{MacroKind, Span},
};

/// rustc's span of the type of the cast `cast` (`ExprKind::Cast`'s `hir::Ty`), its parentheses
/// left out as rustc's lowering leaves them.
fn cast_ty_span(b: &BodyCx<'_, '_, '_>, cast: ExprId) -> Option<Span> {
    let node = b.expr_node(cast)?;
    let InFile { file_id, value } = b.cx.syntax_view(node);
    let mut ty = ast::CastExpr::cast(value)?.ty()?;
    while let ast::Type::ParenType(it) = ty {
        ty = it.ty()?;
    }
    Some(b.cx.span(InFile::new(file_id, ty.syntax().text_range())))
}

/// rustc's `Span::can_be_used_for_suggestions`: not in a macro's expansion, or in a derive's
/// where it is not the derive's own call.
fn can_be_used_for_suggestions(span: &Span) -> bool {
    match &span.expansion {
        None => true,
        Some(it) => it.kind == MacroKind::Derive && (it.call_site.lo, it.call_site.hi) != (span.lo, span.hi),
    }
}

/// rustc's `Span::find_ancestor_in_same_ctxt`: `span`, or the call of the macro it came from,
/// and of the macro that call came from, .., the first in the expansion `other` is in.
fn ancestor_in_same_ctxt(span: &Span, other: &Span) -> Option<Span> {
    // clone: a lint's span, asked only for a lint's suggestion
    let mut at = span.clone();
    while at.expansion != other.expansion {
        at = at.expansion?.call_site;
    }
    Some(at)
}

/// rustc's printing of the type type checking gave `expr`, before its adjustments
/// (`typeck_results().expr_ty`).
fn ty_text(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> Option<String> {
    let node = b.expr_node(expr)?;
    let syntax = b.cx.syntax_view(node);
    let expr = ast::Expr::cast(syntax.value)?;
    let info = b.cx.vsema.type_of_expr(expr)?;
    Some(b.cx.ty(&info.original))
}
