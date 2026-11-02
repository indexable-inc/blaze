//! hir reports each `pub(in path)` and `pub(super)` whose path names no module above the item,
//! as rustc's `try_resolve_visibility` does, on every kind of item that has a visibility.

use hir_def::visibility::VisibilityError;
use hir_expand::fixture;

use crate::{AnyDiagnostic, Crate};

/// The text of each visibility hir reports for the crate `main` with its error, sorted.
fn reported(fixture: &str) -> Vec<(String, &'static str)> {
    let db = fixture::db(fixture).into_db();
    crate::attach_db(&db, || {
        let main = Crate::all(&db)
            .into_iter()
            .find(|it| it.display_name(&db).is_some_and(|it| it.to_string() == "main"))
            .expect("the fixture has `main`");
        let mut acc = Vec::new();
        main.root_module(&db).diagnostics(&db, &mut acc, false);
        let mut found: Vec<_> = acc
            .iter()
            .filter_map(|it| match it {
                AnyDiagnostic::InvalidVisibility(it) => {
                    let root = it.node.file_id.parse_or_expand(&db);
                    let text = it.node.value.to_node(&root).to_string();
                    let error = match it.error {
                        VisibilityError::Unresolved => "E0433",
                        VisibilityError::NotAModule { .. } => "E0577",
                        VisibilityError::NotAncestor { .. } => "E0742",
                        VisibilityError::Relative2018 => "2018",
                    };
                    Some((text, error))
                }
                _ => None,
            })
            .collect();
        found.sort();
        found
    })
}

#[test]
fn each_kind_of_item_reports_its_visibility() {
    let found = reported(
        r#"
//- /main.rs crate:main
pub mod m {
    pub(in crate::m) fn ok() {}
    pub(in crate::nope) fn f() {}
    pub(in crate::S) struct T(pub(in crate::n) u8);
    pub(in crate::m) struct U { pub(in m) x: u8 }
    pub(in crate::n) use crate::S as V;
    pub(in crate::n) mod k {}
    impl super::S {
        pub(in crate::n) fn g() {}
    }
    fn h() {
        pub(in crate::n) struct W;
    }
}
pub mod n {}
pub struct S;
pub(super) const C: u8 = 0;
"#,
    );
    let want = [
        ("pub(in crate::S)", "E0577"),
        ("pub(in crate::n)", "E0742"),
        ("pub(in crate::n)", "E0742"),
        ("pub(in crate::n)", "E0742"),
        ("pub(in crate::n)", "E0742"),
        ("pub(in crate::n)", "E0742"),
        ("pub(in crate::nope)", "E0433"),
        ("pub(in m)", "2018"),
        ("pub(super)", "E0433"),
    ];
    let want: Vec<_> = want.iter().map(|&(text, error)| (text.to_owned(), error)).collect();
    assert_eq!(found, want);
}

#[test]
fn a_visibility_from_a_name_is_from_the_root_in_2015() {
    let found = reported(
        r#"
//- /main.rs crate:main edition:2015
pub mod m {
    pub mod k {
        pub(in m) fn ok() {}
        pub(in k) fn f() {}
    }
}
"#,
    );
    assert_eq!(found, [("pub(in k)".to_owned(), "E0433")]);
}
