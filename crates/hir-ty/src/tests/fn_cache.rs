//! fn-cache's opt-in: only a crate that depends on `blaze` takes part.

use base_db::all_crates;
use hir_expand::fixture;

use crate::fn_cache::uses_fn_cache;

const FIXTURE: &str = r#"
//- /blaze.rs crate:blaze
//- /opted.rs crate:opted deps:blaze
//- /plain.rs crate:plain
//- /blaze_like.rs crate:blaze_like deps:plain
"#;

#[test]
fn only_a_crate_that_depends_on_blaze_uses_the_cache() {
    let db = fixture::db(FIXTURE).into_db();
    let mut opted: Vec<String> = all_crates(&db)
        .iter()
        .filter(|&&krate| uses_fn_cache(&db, krate))
        .filter_map(|krate| Some(krate.extra_data(&db).display_name.as_ref()?.to_string()))
        .collect();
    opted.sort();
    assert_eq!(opted, ["opted"]);
}
