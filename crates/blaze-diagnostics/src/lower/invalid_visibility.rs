//! A `pub(in path)` or `pub(super)` whose path names no module above the item: E0433 at the
//! segment that names nothing, E0577 "expected module, found ..", E0742 "visibilities can only
//! be restricted to ancestor modules", or the error for a path that starts with a name in Rust
//! 2018 or later, with its `crate::` suggestion.
//!
//! Follows rustc's `compiler/rustc_resolve/src/build_reduced_graph.rs`
//! (`try_resolve_visibility`), `diagnostics.rs` (`report_vis_error`) and `errors.rs`
//! (`Relative2018`, `AncestorOnly`, `ExpectedModuleFound`) (MIT or Apache-2.0).

use hir::InFile;
use hir_def::{AdtId, ModuleDefId, visibility::VisibilityError};
use syntax::view::AstNode as _;

use super::unresolved_import::{names_to_string, visibility_path_error, visibility_segments};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::InvalidVisibility) -> Vec<Found> {
    let vis = cx.node_view(d.node);
    let file = vis.file_id;
    let path = vis
        .value
        .visibility_inner()
        .and_then(|it| it.path())
        .expect("hir reports only a visibility with a path: `pub(in path)` or `pub(super)`");
    let at = cx.span(InFile::new(file, path.syntax().text_range()));
    let diag = match d.error {
        VisibilityError::Unresolved => visibility_path_error(cx, file, path),
        VisibilityError::NotAModule { found } => {
            let names = names_to_string(visibility_segments(cx, path).iter().map(|it| it.name.as_str()));
            // alloc: the error's text, for an error hir found
            let message = format!("expected module, found {} `{names}`", descr(found));
            // clone: on the error path, for an error hir found
            let mut diag = Diag::error(Some("E0577"), message, at.clone());
            diag.span_label(at, "not a module");
            diag
        }
        VisibilityError::NotAncestor { .. } => {
            Diag::error(Some("E0742"), "visibilities can only be restricted to ancestor modules", at)
        }
        VisibilityError::Relative2018 => {
            let message = "relative paths are not supported in visibilities in 2018 edition or later";
            // clone: on the error path, for an error hir found
            let mut diag = Diag::error(None, message, at.clone());
            // alloc: the error's text, for an error hir found
            diag.suggestion(at, "try", format!("crate::{}", path.syntax().text()), Applicability::MaybeIncorrect);
            diag
        }
    };
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Resolve)]
}

/// rustc's `Res::descr` of what a visibility's path names in the type namespace.
fn descr(found: ModuleDefId) -> &'static str {
    match found {
        ModuleDefId::AdtId(AdtId::StructId(_)) => "struct",
        ModuleDefId::AdtId(AdtId::UnionId(_)) => "union",
        ModuleDefId::AdtId(AdtId::EnumId(_)) => "enum",
        ModuleDefId::TraitId(_) => "trait",
        ModuleDefId::TypeAliasId(_) => "type alias",
        ModuleDefId::EnumVariantId(_) => "variant",
        ModuleDefId::BuiltinType(_) => "builtin type",
        ModuleDefId::FunctionId(_) => "function",
        ModuleDefId::ConstId(_) => "constant",
        ModuleDefId::StaticId(_) => "static",
        ModuleDefId::MacroId(_) => "macro",
        ModuleDefId::ModuleId(_) => "module",
    }
}
