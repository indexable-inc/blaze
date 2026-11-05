//! A path to a type with lifetime parameters that leaves them out: E0726 "implicit elided
//! lifetime not allowed here" where that is an error (an `impl` header, an `async fn`), else
//! the lint `elided_lifetimes_in_paths`, "hidden lifetime parameters in types are deprecated";
//! both suggest writing `'_`.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`resolve_elided_lifetimes_in_path`),
//! `compiler/rustc_resolve/src/diagnostics/mod.rs` (`ImplicitElidedLifetimeNotAllowedHere`,
//! `ElidedLifetimesInPaths`) and `compiler/rustc_errors/src/lib.rs`
//! (`elided_lifetime_in_path_suggestion`) (MIT or Apache-2.0).

use hir::InFile;
use syntax::view::{self as ast, AstNode as _};

use crate::{
    cx::Cx,
    lints::registry::LintId,
    lower::{
        Found, Phase,
        incorrect_generics_len::{Segment, generics_fact, s, site_fact},
    },
    model::{Applicability, Diag},
};

/// The lint is allowed by default and hir reports every elided path: where it is allowed
/// everywhere, the lowering is one bit test.
pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ElidedLifetimesInPath) -> Vec<Found> {
    const LINT: LintId = LintId::named("elided_lifetimes_in_paths");
    if !d.hard_error && !cx.lint_may_fire(LINT) {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let n = d.expected as usize;
    let node = cx.node_view(d.generics_or_segment);
    let node_span = cx.span(node.map(|it| it.syntax().text_range()));
    let seg = Segment::of(cx, d.generics_or_segment);
    let angle = seg.as_ref().and_then(|it| it.list?.l_angle_token());
    // rustc's `elided_lifetime_span`: the `<` of the arguments, or the name without them.
    let elided = match (&seg, angle) {
        (Some(seg), Some(angle)) => seg.span(cx, angle.text_range()),
        (Some(seg), None) => seg.name_span(cx),
        (None, _) => node_span.clone(), // clone: the diagnostic's span, past the lint test
    };
    let path_span = match seg.as_ref().and_then(|it| it.path()) {
        Some(path) => cx.span(InFile::new(node.file_id, path.syntax().text_range())),
        None => node_span.clone(), // clone: the diagnostic's span, past the lint test
    };
    // alloc: the suggestion's text, past the lint test
    let anon_lts = vec!["'_"; n].join(", ");
    let suggestion = match angle {
        Some(_) => format!("{anon_lts}, "), // alloc: the suggestion's text
        None => format!("<{anon_lts}>"),    // alloc: the suggestion's text
    };

    let mut diag = match d.hard_error {
        true => Diag::error(
            Some("E0726"),
            "implicit elided lifetime not allowed here",
            path_span.clone(), // clone: the diagnostic's span
        ),
        false => Diag::lint(
            "elided_lifetimes_in_paths",
            "hidden lifetime parameters in types are deprecated",
            elided.clone(), // clone: the diagnostic's span
        ),
    };
    // alloc: the diagnostic's label
    diag.span_label(path_span, format!("expected lifetime parameter{}", s(n)));
    diag.suggestion_verbose(
        elided.shrink_to_hi(),
        // alloc: the suggestion's message
        format!("indicate the anonymous lifetime{}", s(n)),
        suggestion,
        Applicability::Unspecified,
    );
    generics_fact(cx, &mut diag, d.def);
    let at = match &seg {
        Some(seg) => InFile::new(seg.file, seg.node),
        None => node.map(|it| it.syntax()),
    };
    site_fact(cx, &mut diag, at);
    match d.hard_error {
        // alloc: the diagnostic found
        true => vec![Found::new(diag, Phase::Resolve)],
        // alloc: the diagnostic found
        false => vec![Found::lint(diag, Phase::Resolve, at.map(ast::ptr))],
    }
}
