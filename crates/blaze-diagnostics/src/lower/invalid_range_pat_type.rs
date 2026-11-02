//! E0029: a range pattern whose ends are not `char`s or numbers, as `"a"..="z"`, with a label
//! on each end of what it is.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/pat.rs` (`check_pat_range`,
//! `emit_err_pat_range`, `endpoint_has_type`) (MIT or Apache-2.0). Each end's type and whether
//! it fails are inference's, as it had them when it checked that end (an undecided literal's
//! type is `{integer}`). The `-Z teach` note is not written: blaze is not run with it.

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

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::InvalidRangePatType<'db>) -> Vec<Found> {
    let node = cx.node_view(d.pat);
    // alloc: empty, allocates nothing
    let Some(ast::Pat::RangePat(range)) = node.value.right() else { return Vec::new() };
    let file = node.file_id;
    let end = |pat: Option<ast::Pat<'_>>, at: &Option<(hir::Type<'db>, bool)>| {
        let (pat, (ty, fail)) = (pat?, at.as_ref()?);
        // clone: a type handle, on the error path, for an error hir found
        Some(End { span: cx.span(InFile::new(file, pat.syntax().text_range())), ty: ty.clone(), fail: *fail })
    };
    let lhs = end(range.start(), &d.lhs);
    let rhs = end(range.end(), &d.rhs);
    // rustc's order: the failing end first, then the other.
    let (first, second) = match (lhs, rhs) {
        (Some(l), r) if l.fail => (l, r),
        (l, Some(r)) if r.fail => (r, l),
        // alloc: empty, allocates nothing
        _ => return Vec::new(),
    };
    let span = match &second {
        Some(it) if it.fail => cx.span_of_ptr(d.pat),
        // clone: on the error path, for an error hir found
        _ => first.span.clone(),
    };
    let mut diag = Diag::error(
        Some("E0029"),
        "only `char` and numeric types are allowed in range patterns",
        span,
    );
    for end in std::iter::once(first).chain(second) {
        let label = match end.fail {
            // alloc: the error's text, for an error hir found
            true => format!("this is of type `{}` but it should be `char` or numeric", cx.ty(&end.ty)),
            // alloc: the error's text, for an error hir found
            false => format!("this is of type `{}`", cx.ty(&end.ty)),
        };
        diag.span_label(end.span, label);
    }
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}
