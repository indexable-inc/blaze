//! E0029: a range pattern whose ends are not `char`s or numbers, as `"a"..="z"`, with a label
//! on each end of what it is.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/pat.rs` (`check_pat_range`,
//! `emit_err_pat_range`, `endpoint_has_type`) (MIT or Apache-2.0). The `-Z teach` note is
//! not written: blaze is not run with it.

use hir::InFile;
use syntax::view::{self as ast, AstNode as _, RangeItem as _};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Diag, Span},
};

/// An end of the range: where it is, its type, and whether it is what fails.
struct End<'db> {
    span: Span,
    ty: hir::Type<'db>,
    fail: bool,
}

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::InvalidRangePatType) -> Vec<Found> {
    let node = cx.node_view(d.pat);
    let Some(ast::Pat::RangePat(range)) = node.value.right() else { return Vec::new() };
    let file = node.file_id;
    let end = |pat: Option<ast::Pat<'_>>| {
        let pat = pat?;
        let ty = cx.vsema.type_of_pat(pat)?.original;
        let fail = !(ty.is_int_or_uint() || ty.is_float() || ty.is_char() || ty.contains_unknown());
        Some(End { span: cx.span(InFile::new(file, pat.syntax().text_range())), ty, fail })
    };
    let mut lhs = end(range.start());
    let mut rhs = end(range.end());
    // The ends were each fine, but not once unified: rustc's check after `demand_eqtype`,
    // which takes both to fail.
    if !lhs.as_ref().is_some_and(|it| it.fail) && !rhs.as_ref().is_some_and(|it| it.fail) {
        lhs.iter_mut().chain(rhs.iter_mut()).for_each(|it| it.fail = true);
    }
    if lhs.iter().chain(rhs.iter()).any(|it| it.ty.contains_unknown()) {
        // rustc's `downgrade_to_delayed_bug`: an error of an end was given already.
        return Vec::new();
    }
    // rustc's order: the failing end first, then the other.
    let (first, second) = match (lhs, rhs) {
        (Some(l), r) if l.fail => (l, r),
        (l, Some(r)) if r.fail => (r, l),
        _ => return Vec::new(),
    };
    let span = match &second {
        Some(it) if it.fail => cx.span_of_ptr(d.pat),
        _ => first.span.clone(),
    };
    let mut diag = Diag::error(
        Some("E0029"),
        "only `char` and numeric types are allowed in range patterns",
        span,
    );
    for end in std::iter::once(first).chain(second) {
        let label = match end.fail {
            true => format!("this is of type `{}` but it should be `char` or numeric", cx.ty(&end.ty)),
            false => format!("this is of type `{}`", cx.ty(&end.ty)),
        };
        diag.span_label(end.span, label);
    }
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Bodies)]
}
