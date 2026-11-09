//! E0320 "overflow while adding drop-check rules for `{ty}`", noting "overflowed on
//! `{overflow_ty}`", at the local whose drop check went past the crate's recursion limit. Both
//! types are abbreviated as rustc's `short_string` abbreviates them. The message also names the
//! body and the limit, which rustc's leaves out (rule 19), and a help to raise the limit, as
//! E0055's.
//!
//! Follows rustc's `compiler/rustc_trait_selection/src/traits/query/dropck_outlives.rs`
//! (`compute_dropck_outlives_with_errors`), `compiler/rustc_borrowck/src/type_check/liveness/
//! trace.rs` (`dropck_local`) and `compiler/rustc_middle/src/error.rs` (`DropCheckOverflow`)
//! (MIT or Apache-2.0).

use crate::{
    cx::Cx,
    lints::{body_path, code_fact},
    lower::{Found, const_check_error::phase},
    model::Diag,
    print,
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::DropCheckOverflow<'db>) -> Vec<Found> {
    let span = cx.span_of(d.node);
    let ty = print::short_ty(cx, &d.ty);
    let owner = body_path(cx, d.node).map(|(_, path)| path);
    let owner = owner.as_deref().unwrap_or("this body");
    let limit = d.limit;
    // alloc: the error's text, for an error hir found
    let message = format!(
        "overflow while adding drop-check rules for `{ty}` in `{owner}`, past the recursion limit of {limit}"
    );
    let mut diag = Diag::error(Some("E0320"), message, span);
    // alloc: the error's text, for an error hir found
    diag.note(format!("overflowed on `{}`", print::short_ty(cx, &d.overflow_ty)));
    let krate = cx.krate.display_name(cx.db).expect("a crate blaze checks has a name");
    // alloc: the error's text, for an error hir found
    diag.help(format!(
        "consider increasing the recursion limit by adding a `#![recursion_limit = \"{}\"]` attribute to your crate (`{krate}`)",
        limit.saturating_mul(2).max(2),
    ));
    code_fact(cx, &mut diag, d.node);
    // alloc: `lower`'s one `Vec` per diagnostic (lower/mod.rs's interface, the edge).
    vec![Found::new(diag, phase(cx, d.node))]
}
