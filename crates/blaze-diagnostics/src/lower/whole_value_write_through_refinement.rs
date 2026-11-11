//! blaze: a whole-value write through a unique reference to a refinement by enum variants
//! (`#![blaze::pattern_types]`, docs/v2/plan/design/edition.md, section 8): through
//! `&mut (T is PAT)` the fields of the value may be written, but not the whole value, which
//! could leave the place holding a variant outside the set the reference was made for (rule
//! pattern-types-unique-ref).

use hir::InFile;

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
    print,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::WholeValueWriteThroughRefinement<'_>) -> Vec<Found> {
    let ptr = InFile::new(d.place.file_id, d.place.value.syntax_node_ptr());
    let at = cx.span_of(ptr);
    let ty = print::ty(cx, &d.ty);
    let place = cx.snippet(&at).unwrap_or("the place");
    let mut diag = Diag::error(
        None,
        // alloc: the error's text, for an error hir found
        format!(
            "`{place}` is a whole value of `{ty}` behind a unique reference, which may not be \
             assigned, swapped or replaced (rule pattern-types-unique-ref)"
        ),
        // clone: a handle, on the error path
        at.clone(),
    );
    diag.help("write its fields one by one, or match on a `&mut` of the base type");
    code_fact(cx, &mut diag, ptr);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}
