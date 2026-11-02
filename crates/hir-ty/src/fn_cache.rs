//! fn-cache's opt-in (docs/fn-cache.md): a crate takes part when it depends on the `blaze`
//! crate, whose attribute caches a function. Codegen looks for cached functions and the front
//! end checks `const fn` purity (`blaze::const_impure`) only in such a crate.

use base_db::{Crate, Db};

/// Whether `krate` depends on the `blaze` crate, fn-cache's opt-in.
#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub fn uses_fn_cache(db: &Db, krate: Crate) -> bool {
    krate.data(db).dependencies.iter().any(|dep| {
        dep.crate_id
            .extra_data(db)
            .display_name
            .as_ref()
            .is_some_and(|name| name.crate_name().symbol().as_str() == "blaze")
    })
}
