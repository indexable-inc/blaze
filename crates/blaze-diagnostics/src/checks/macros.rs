//! The errors of defining each `macro_rules!` of the crate, as rustc's expansion gives them
//! when it comes to the definition: mbe accepts definitions rustc refuses (no rules, an
//! unknown fragment specifier, a binding twice), so hir's `MacroDefError` does not have them
//! all. Each definition `cfg` keeps, in a file or in an expansion, in the order the walk comes
//! to it.
//!
//! Follows rustc's `compiler/rustc_expand/src/mbe/macro_rules.rs` (`compile_declarative_macro`,
//! called as `rustc_resolve`'s `define_macro` meets each definition) (MIT or Apache-2.0).

use hir::InFile;
use syntax::view::{self as ast, AstNode as _};

use crate::{cx::Cx, lints::syntax::Walk, lower::Found, lower::macro_def_error};

pub(crate) fn check<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>) -> Vec<Found> {
    // alloc: empty, allocates only when a definition is in error
    let mut found = Vec::new();
    for tree in &walk.trees {
        for node in tree.of_kind(|kind| ast::Macro::can_cast(kind)) {
            let Some(mac) = ast::Macro::cast(node) else { continue };
            found.extend(macro_def_error::check(cx, InFile::new(tree.file, mac)));
        }
    }
    found
}
