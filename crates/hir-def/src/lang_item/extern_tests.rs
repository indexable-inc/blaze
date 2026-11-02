//! What crates a crate's lang items come from.

use base_db::{Crate, Db, all_crates};
use hir_expand::fixture;

use super::lang_items;

/// `rt` defines `start`; `lib` depends on it but has it out of its extern prelude, as a
/// `#![no_std]` crate has `std`. `{lib}` is the text of `lib`'s root.
fn start_of(lib: &str) -> bool {
    let text = format!(
        "//- /rt.rs crate:rt\n\
         #![feature(lang_items)]\n\
         #[lang = \"start\"]\n\
         fn lang_start() {{}}\n\
         //- /lib.rs crate:lib deps:rt extern-prelude:\n\
         {lib}\n"
    );
    let db = fixture::db(&text).into_db();
    lang_items(&db, crate_named(&db, "lib")).Start.is_some()
}

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// A test harness of a `#![no_std]` crate reaches `std`, and its `start`, only through the
/// `extern crate test;` in the harness's module: an `extern crate` outside the root loads its
/// crate's lang items, as rustc's crate loader does.
#[test]
fn start_is_found_through_an_extern_crate_outside_the_root() {
    assert!(start_of("pub mod harness { extern crate rt; }"));
    // The cause removed: with no `extern crate`, `rt` is not loaded and `start` is not found.
    assert!(!start_of("pub mod harness {}"));
}

/// A proc-macro crate is loaded for its macros alone (`Crate::loads_only_macros`): the lang
/// items of its dependencies (its own `std`, built for the host or for `wasm32-wasip1`) are
/// never its user's.
#[test]
fn a_proc_macros_lang_items_are_not_its_users() {
    let start_through = |kind: &str| {
        let text = format!(
            "//- /rt.rs crate:rt\n\
             #![feature(lang_items)]\n\
             #[lang = \"start\"]\n\
             fn lang_start() {{}}\n\
             //- /pm.rs crate:{kind}:pm deps:rt\n\
             //- /lib.rs crate:lib deps:pm\n"
        );
        let db = fixture::db(&text).into_db();
        lang_items(&db, crate_named(&db, "lib")).Start.is_some()
    };
    assert!(!start_through("proc-macro"));
    // The cause removed: through a library, `rt`'s `start` is the user's.
    assert!(start_through("lib"));
}
