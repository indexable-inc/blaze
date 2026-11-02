//! A `ModuleId` kept across revisions that delete its module and make others: the query
//! database reads the fields the module last had, never UB (the contract of
//! `ModuleIdLt::to_static`, a safe `fn` over `blaze_query::db::FromId`). `blaze test --ub` runs this under
//! the UB checker.

use std::panic::{AssertUnwindSafe, catch_unwind};

use hir_expand::fixture;

use crate::{
    nameres::crate_def_map,
    test_db::TestDBExt,
};

#[test]
fn a_module_id_kept_past_its_deletion_panics_or_reads_a_module() {
    let fx = fixture::db(
        r#"
//- /main.rs
mod foo;
mod bar;
//- /foo.rs
pub struct S;
//- /bar.rs
pub struct T;
"#,
    );
    let files = fx.files().to_vec();
    let mut db = fx.into_db();
    let krate = db.fetch_test_crate();
    let main = files[0].file_id(&db);
    let foo = db.module_for_file(files[1].file_id(&db));
    let name_of = |name: Option<hir_expand::name::Name>| name.map(|it| it.as_str().to_owned());
    assert_eq!(name_of(foo.name(&db)), Some("foo".to_owned()));

    db.set_file_text(main, "mod bar;\n");
    assert!(crate_def_map(&db, krate).modules(&db).all(|(id, _)| id != foo), "`foo` is gone");

    // New modules, which must not be handed `foo`'s id.
    db.set_file_text(main, "mod bar;\nmod baz {}\nmod qux {}\n");
    let live: Vec<Option<String>> =
        crate_def_map(&db, krate).modules(&db).map(|(id, _)| name_of(id.name(&db))).collect();
    assert!(live.contains(&Some("qux".to_owned())));

    let read = catch_unwind(AssertUnwindSafe(|| (foo.krate(&db), name_of(foo.name(&db)))));
    if let Ok((stale_krate, name)) = read {
        assert_eq!(stale_krate, krate, "a slot of a module is a module of this crate");
        assert!(
            name == Some("foo".to_owned()) || live.contains(&name),
            "the stale id reads `foo` (not yet freed) or a live module, got {name:?}"
        );
    }

    // The database is still sound to use after the read, panicking or not.
    assert_eq!(crate_def_map(&db, krate).modules(&db).count(), live.len());
}
