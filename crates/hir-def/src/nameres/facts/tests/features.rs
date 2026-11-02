//! `UnstableFeatures::of_root`: the crate's `#![feature]`s read without its def map, as a proc
//! macro the crate calls reads them while the map is being built.

use base_db::Db;
use intern::Symbol;
use hir_expand::fixture;

use crate::{
    nameres::crate_def_map,
    test_db::TestDBExt,
    unstable_features::UnstableFeatures,
};

const FIXTURE: &str = r#"
//- /lib.rs crate:foo crate-attr:feature(never_patterns) crate-attr:no_std
#![feature(lang_items, forced_keywords)]
#![cfg_attr(all(), feature(mut_ref))]
#![cfg_attr(any(), feature(deref_patterns))]
#![doc(hidden)]
pub mod a;
//- /a.rs
pub struct A;
"#;

fn names(features: &UnstableFeatures) -> Vec<String> {
    let mut names: Vec<_> = features.iter().map(|it| it.as_str().to_owned()).collect();
    names.sort_unstable();
    names
}

fn db() -> Db {
    fixture::db(FIXTURE).into_db()
}

#[test]
fn the_roots_features_are_the_def_maps() {
    let db = db();
    let krate = db.fetch_test_crate();
    let of_root = UnstableFeatures::of_root(&db, krate);
    assert_eq!(of_root, crate_def_map(&db, krate).features());
    assert_eq!(of_root, UnstableFeatures::query(&db, krate));
    assert_eq!(names(of_root), ["forced_keywords", "lang_items", "mut_ref", "never_patterns"]);
    assert!(of_root.forced_keywords && of_root.lang_items && of_root.mut_ref);
    assert!(!of_root.deref_patterns);
    assert!(of_root.is_enabled(&Symbol::intern("never_patterns")));
}

/// Read inside a batch, after an edit of the root and before any maintainer ran: what the read
/// executes is what the features need, as when the maintainer's run of a proc macro reads them.
#[test]
fn reading_the_roots_features_builds_no_def_map() {
    let fx = fixture::db(FIXTURE);
    let root = fx.file(0).file_id(fx.db());
    let mut db = fx.into_db();
    let krate = db.fetch_test_crate();
    let text = format!("#![feature(deref_patterns)]\n{}", db.file_text(root).text(&db));
    let (forced, deref, executed) = db.batch(|db| {
        db.set_file_text(root, &text);
        let view: &Db = db;
        let mut read = (false, false);
        let executed = view.executed_in_read_order(|| {
            let features = UnstableFeatures::of_root(view, krate);
            read = (features.forced_keywords, features.deref_patterns);
        });
        (read.0, read.1, executed)
    });
    assert!(forced && deref, "the edit's feature is read");
    assert!(executed.iter().any(|it| it.contains("crate_data")), "{executed:#?}");
    let resolver = |it: &&String| it.contains("def_map") || it.contains("module_decls");
    assert_eq!(executed.iter().filter(resolver).count(), 0, "{executed:#?}");
}

#[test]
fn a_crate_without_features_has_none() {
    // `no_std` keeps the fixture from gating a `no_core` with `feature(no_core)`.
    let fx = fixture::db("//- /lib.rs crate:foo crate-attr:no_std\npub struct A;\n");
    let _ = fx.only_file();
    let db = fx.into_db();
    let krate = db.fetch_test_crate();
    let of_root = UnstableFeatures::of_root(&db, krate);
    assert!(names(of_root).is_empty());
    assert!(!of_root.forced_keywords);
    assert_eq!(of_root, crate_def_map(&db, krate).features());
}
