//! E0527 and E0528: a slice pattern of more or fewer elements than the array it matches has,
//! E0528 when the pattern has a `..` and still needs more than the array has.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/pat.rs` (`check_array_pat_len`,
//! `error_scrutinee_inconsistent_length`, `error_scrutinee_with_rest_inconsistent_length`)
//! (MIT or Apache-2.0).

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MismatchedArrayPatLen) -> Vec<Found> {
    let span = cx.span_of_ptr(d.pat);
    let (min, size) = (d.found, d.expected);
    let mut diag = match d.has_rest {
        false => {
            // alloc: the error's text, for an error hir found
            let message = format!("pattern requires {min} element{} but array has {size}", s(min));
            // clone: on the error path, for an error hir found
            let mut diag = Diag::error(Some("E0527"), message, span.clone());
            // alloc: the error's text, for an error hir found
            diag.span_label(span, format!("expected {size} element{}", s(size)));
            diag
        }
        true => {
            // alloc: the error's text, for an error hir found
            let message = format!("pattern requires at least {min} element{} but array has {size}", s(min));
            // clone: on the error path, for an error hir found
            let mut diag = Diag::error(Some("E0528"), message, span.clone());
            // alloc: the error's text, for an error hir found
            diag.span_label(span, format!("pattern cannot match array of {size} element{}", s(size)));
            diag
        }
    };
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// rustc's `pluralize!` of a count.
fn s(count: u64) -> &'static str {
    if count == 1 { "" } else { "s" }
}
