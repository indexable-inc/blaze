//! rustc's late lint passes of what `for` loops, `.into_iter()` and `if let` desugar to.
//!
//! - `for_loops_over_fallibles`: `ForLoopsOverFallibles` (`for_loops_over_fallibles`).
//! - `shadowed_into_iter`: `ShadowedIntoIter` (`array_into_iter`, `boxed_slice_into_iter`).
//! - `if_let_rescope`: `IfLetRescope` (`if_let_rescope`), with `significant_drop`: the types
//!   of a value whose destructors are significant.
//!
//! Follows rustc's `compiler/rustc_lint/src/lib.rs` (`BuiltinCombinedLateLintModPass`),
//! `compiler/rustc_ast_lowering/src/expr.rs` (`lower_expr_for`: the `for` loop's `DropTemps(match
//! IntoIterator::into_iter(<head>) { iter => loop { match Iterator::next(&mut iter) { None =>
//! break, Some(<pat>) => <body> } } })`, which hir-def's `collect_for_loop` builds without the
//! `DropTemps`) and `compiler/rustc_span/src/lib.rs` (`Span::between`, `source_callsite`,
//! `can_be_used_for_suggestions`), `compiler/rustc_span/src/source_map.rs` (`end_point`)
//! (MIT or Apache-2.0).

pub(crate) mod for_loops_over_fallibles;
pub(crate) mod if_let_rescope;
pub(crate) mod shadowed_into_iter;
mod significant_drop;

use hir_def::hir::{Expr, ExprId, LoopSource, Pat, PatId};
use span::Edition;

use crate::{
    lints::bodies::BodyCx,
    model::{MacroKind, Span},
};

/// A `for <pat> in <head> { .. }` loop, as `expr`, the `match` it is desugared to.
struct ForLoop {
    /// The call `IntoIterator::into_iter(<head>)`.
    into_iter: ExprId,
    head: ExprId,
    pat: PatId,
}

/// The `for` loop `expr` is the desugaring of, if it is one: rustc's `extract_for_loop`, a
/// `match` of one arm over a call of one argument, whose arm is the loop (`LoopSource::ForLoop`)
/// of the `match` on `Iterator::next` whose second arm's pattern is `Some(<pat>)`.
fn for_loop(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> Option<ForLoop> {
    let store = b.store();
    let Expr::Match { expr: into_iter, arms: [arm] } = store.expr(expr) else { return None };
    let Expr::Call { args: [head], .. } = store.expr(into_iter) else { return None };
    let Expr::Loop { body, source: LoopSource::ForLoop, .. } = store.expr(arm.expr) else { return None };
    let Expr::Block { statements, tail: Some(next), .. } = store.expr(body) else { return None };
    if !statements.is_empty() {
        return None;
    }
    let Expr::Match { arms: [_, some], .. } = store.expr(next) else { return None };
    let Pat::TupleStruct { args: [pat], .. } = store.pat(some.pat) else { return None };
    Some(ForLoop { into_iter, head: *head, pat: *pat })
}

/// rustc's `Span::can_be_used_for_suggestions`: not in a macro's expansion, or in a derive's
/// where it is not the derive's own span.
fn can_be_used_for_suggestions(span: &Span) -> bool {
    match &span.expansion {
        None => true,
        Some(expansion) => {
            expansion.kind == MacroKind::Derive
                && (expansion.call_site.lo, expansion.call_site.hi) != (span.lo, span.hi)
        }
    }
}

/// rustc's `Span::source_callsite`: the call of the outermost macro `span` came from, or
/// `span`.
fn source_callsite(span: &Span) -> Span {
    // clone: a span, for a lint's suggestion
    let mut at = span.clone();
    while let Some(expansion) = at.expansion.take() {
        at = expansion.call_site;
    }
    at
}

/// rustc's `Span::between`: from the end of `from` to the start of `to`, in `to`'s expansion
/// when that is none, else in `from`'s.
fn between(from: &Span, to: &Span) -> Span {
    let expansion = match to.expansion {
        None => None,
        // clone: a span, for a lint's suggestion
        Some(_) => from.expansion.clone(),
    };
    // clone: a span, for a lint's suggestion
    Span { file: from.file.clone(), lo: from.hi.min(to.lo), hi: from.hi.max(to.lo), expansion }
}

/// rustc's `SourceMap::end_point`: the last character of `span` (of one byte when its text
/// can't be read).
fn end_point(b: &BodyCx<'_, '_, '_>, span: &Span) -> Span {
    let width = b.cx.snippet(span).and_then(|it| it.chars().next_back()).map_or(1, char::len_utf8);
    let lo = span.hi.checked_sub(width as u32).unwrap_or(span.hi).max(span.lo);
    span.with_lo(lo)
}

/// rustc's `Span::edition` of `expr`: the edition of the code that wrote it, the crate's or
/// that of the macro whose expansion wrote it.
fn edition(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> Edition {
    let Some(node) = b.expr_node(expr) else { return b.cx.edition };
    let Some(call) = node.file_id.macro_file() else { return b.cx.edition };
    let db = b.db();
    match node.map(|it| it.text_range()).original_node_file_range_opt(db) {
        Some((_, ctxt)) => ctxt.edition(db),
        None => call.loc(db).def.edition,
    }
}
