//! A call of a compile-time-only function (`#[rustc_comptime]`: `TypeId::info` and the rest of
//! `core::mem::type_info`, the reflection intrinsics) where the call would run: rustc's
//! "comptime functions can only be called at compile time", found by `hir-ty-mir`'s
//! `const_check::comptime`. The message and the `const { .. }` suggestion are rustc's; the
//! notes name the function and say the other way out, which rustc does not: making the caller
//! compile-time-only too.

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ComptimeCall) -> Vec<Found> {
    let span = cx.span_of(d.node);
    let mut diag = Diag::error(None, hir::ComptimeCall::MESSAGE, span.clone());
    let name = d.callee.name(cx.db);
    // alloc: the note's text, once per diagnostic (the edge).
    diag.note(format!(
        "`{}` is `#[rustc_comptime]`: no code is compiled for it, so only a constant's value, \
         a `const {{ .. }}` block or another `#[rustc_comptime]` function may call it",
        name.display(cx.db, cx.edition)
    ));
    diag.multipart_suggestion(
        "wrap the call in a const block to force compile time evaluation",
        // alloc: the suggestion's parts
        vec![(span.shrink_to_lo(), "const { ".to_owned()), (span.shrink_to_hi(), " }".to_owned())],
        // The call's arguments may be values only known when the caller runs.
        Applicability::MaybeIncorrect,
    );
    diag.help(
        "or, when the caller itself only runs at compile time, mark it `#[rustc_comptime]` \
         (and not `const`)",
    );
    code_fact(cx, &mut diag, d.node);
    // alloc: `lower`'s one `Vec` per diagnostic (lower/mod.rs's interface, the edge).
    vec![Found::new(diag, Phase::Mir)]
}
