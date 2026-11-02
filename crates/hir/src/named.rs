//! The dependencies a crate's code names, as rustc loads them: [`hir_def::nameres::named`].

use base_db::Db;

use crate::Crate;

/// The dependencies of `krate` that its code names, as rustc loads them, with those of the
/// standard library's that it is given: every one, for a crate of the standard library
/// ([`hir_def::nameres::named::named_dependencies`]).
pub fn named_dependencies(db: &Db, krate: Crate) -> Vec<Crate> {
    let named = hir_def::nameres::named::named_dependencies(db, krate.id);
    // alloc: once per crate asked, its dependencies
    named.iter().map(|&id| Crate { id }).collect()
}
