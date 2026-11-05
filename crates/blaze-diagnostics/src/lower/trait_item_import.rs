//! An import of a trait's associated items: rustc's E0658 "`use` associated items of traits is
//! unstable" without `import_trait_associated_functions`, once per glob and once per namespace
//! of a single import (hir-def gives one diagnostic per report). Also blaze's own error for a
//! trait some of whose items a macro or an attribute macro writes, whose items blaze can't
//! import: the import binds nothing, so the build stops there.
//!
//! Follows rustc's `compiler/rustc_resolve/src/imports.rs` (`resolve_glob_import`,
//! `finalize_import`: `feature_err` at `import.span`) (MIT or Apache-2.0).

use hir::InFile;
use hir_def::nameres::diagnostics::TraitItemImport;
use syntax::view as ast;

use crate::{
    checks::features::lowered_feature_err,
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::TraitItemImport) -> Vec<Found> {
    let at = d.decl.map(|it| it.syntax_node_ptr());
    let span = cx.span_of(at);
    let mut diag = match &d.why {
        TraitItemImport::Unstable => lowered_feature_err(
            cx,
            "import_trait_associated_functions",
            span,
            "`use` associated items of traits is unstable",
        ),
        TraitItemImport::ItemsFromMacros => {
            let tree = cx.node_view(d.decl);
            Diag::error(
                None,
                // alloc: the error's text, for an error hir found
                format!(
                    "blaze cannot import the items of trait `{}`: an item of it comes from a macro or an attribute macro",
                    trait_name(tree)
                ),
                span,
            )
        }
    };
    code_fact(cx, &mut diag, at);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Resolve)]
}

/// The trait a use tree imports from, as written: a glob's path, or the qualifier of a single
/// import's.
fn trait_name<'t>(tree: InFile<ast::UseTree<'t>>) -> &'t str {
    let path = tree.value.path();
    let named = match tree.value.star_token() {
        Some(_) => path,
        None => path.and_then(|it| it.qualifier()),
    };
    named.and_then(|it| it.segment()).and_then(|it| it.name_ref()).map_or("_", |it| it.text())
}
