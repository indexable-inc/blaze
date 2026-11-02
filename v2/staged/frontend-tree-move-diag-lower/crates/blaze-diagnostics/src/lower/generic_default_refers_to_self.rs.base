//! E0735 "generic parameters cannot use `Self` in their defaults": `struct S<T = Self>`.
//!
//! Follows rustc's `compiler/rustc_resolve/src/diagnostics/impls.rs`
//! (`ResolutionError::ForwardDeclaredSelf`) and `compiler/rustc_resolve/src/diagnostics/mod.rs`
//! (`SelfInGenericParamDefault`) (MIT or Apache-2.0).

use hir::InFile;
use syntax::AstNode;

use crate::{
    cx::Cx,
    lower::{Found, Phase, incorrect_generics_len::site_fact},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::GenericDefaultRefersToSelf) -> Vec<Found> {
    let segment = cx.node(d.segment);
    let range = match segment.value.name_ref() {
        Some(name) => name.syntax().text_range(),
        None => segment.value.syntax().text_range(),
    };
    let span = cx.span(InFile::new(segment.file_id, range));
    let mut diag =
        Diag::error(Some("E0735"), "generic parameters cannot use `Self` in their defaults", span);
    site_fact(cx, &mut diag, InFile::new(segment.file_id, segment.value.syntax()));
    vec![Found::new(diag, Phase::Resolve)]
}
