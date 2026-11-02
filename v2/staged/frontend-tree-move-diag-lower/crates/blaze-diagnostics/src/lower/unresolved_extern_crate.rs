//! E0463, an `extern crate` of a crate that is not found, as rustc's `CannotFindCrate` gives
//! it when the crate is not a dependency: "can't find crate for `x`" with its label, and the
//! help or note rustc gives for the crates it knows of.
//!
//! Follows rustc's `compiler/rustc_metadata/src/diagnostics.rs` (`CannotFindCrate`) and
//! `locator.rs` (`CrateError::report`) (MIT or Apache-2.0).

use hir::InFile;
use syntax::view::AstNode as _;

use crate::{
    cx::{Cx, item_range_view as item_range},
    lower::{Found, Phase},
    model::Diag,
};

/// rustc's `-Z instrument-coverage` runtime, which it names when it is not found.
const PROFILER_RUNTIME: &str = "profiler_builtins";

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnresolvedExternCrate) -> Vec<Found> {
    let node = cx.node_view(d.decl);
    // alloc: empty, allocates nothing
    let Some(name) = node.value.name_ref() else { return Vec::new() };
    let name = name.text();
    let span = cx.span(InFile::new(node.file_id, item_range(node.value.syntax())));
    // alloc: the error's text, for an error hir found
    let mut diag = Diag::error(Some("E0463"), format!("can't find crate for `{name}`"), span.clone());
    if name == PROFILER_RUNTIME {
        diag.note("the compiler may have been built without the profiler runtime");
    } else if name.starts_with("rustc_") {
        diag.help(
            "maybe you need to install the missing components with: `rustup component add rust-src rustc-dev llvm-tools-preview`",
        );
    }
    diag.span_label(span, "can't find crate");
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Expand)]
}
