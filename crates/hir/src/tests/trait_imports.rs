//! hir reports hir-def's `TraitItemImport`: without `import_trait_associated_functions`, rustc's
//! E0658 once for a glob of a trait and once per namespace of a single import of its item.

use hir_def::nameres::diagnostics::TraitItemImport;
use hir_expand::fixture;

use crate::{AnyDiagnostic, Crate};

/// The `why` of each `TraitItemImport` hir reports for the crate `main`, in order.
fn reported(fixture: &str) -> Vec<TraitItemImport> {
    let db = fixture::db(fixture).into_db();
    crate::attach_db(&db, || {
        let main = Crate::all(&db)
            .into_iter()
            .find(|it| it.display_name(&db).is_some_and(|it| it.to_string() == "main"))
            .expect("the fixture has `main`");
        let mut acc = Vec::new();
        main.root_module(&db).diagnostics(&db, &mut acc, false);
        acc.iter()
            .filter_map(|it| match it {
                AnyDiagnostic::TraitItemImport(it) => Some(it.why),
                _ => None,
            })
            .collect()
    })
}

#[test]
fn a_trait_item_import_is_e0658_without_the_feature() {
    let found = reported(
        r#"
//- minicore: default
//- /main.rs crate:main
use core::default::Default::*;
use core::default::Default::default as make;
"#,
    );
    assert_eq!(found, [TraitItemImport::Unstable, TraitItemImport::Unstable]);
}

#[test]
fn a_trait_item_import_is_not_reported_with_the_feature() {
    let found = reported(
        r#"
//- minicore: default
//- /main.rs crate:main
#![feature(import_trait_associated_functions)]
use core::default::Default::*;
use core::default::Default::default as make;
"#,
    );
    assert!(found.is_empty(), "{found:?}");
}
