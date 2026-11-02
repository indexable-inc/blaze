//! rustc's evaluation of each free `const` and `static` in a check (`ConstEvalFailed`): which
//! items it evaluates, and that a failure names the item and the reason.

use hir_expand::fixture;

use super::db::TestDB;
use crate::{AnyDiagnostic, ConstEvalFailure};

/// Each `ConstEvalFailed` of the fixture's crate: the item, whether it is a `static`, and
/// the failure's reason (`cycle` for a value that needs itself).
fn failures(db: &TestDB) -> Vec<(String, bool, String)> {
    crate::attach_db(db, || {
        let krate = crate::check::workspace_crates(db)[0];
        crate::check::check_crate(db, krate, false, &crate::check::every_body)
            .iter()
            .filter_map(|it| match it {
                AnyDiagnostic::ConstEvalFailed(d) => {
                    let reason = match &d.failure {
                        ConstEvalFailure::Failed(reason) => reason.clone(),
                        ConstEvalFailure::Cycle => "cycle".to_owned(),
                        ConstEvalFailure::Unsupported(reason) => format!("unsupported: {reason}"),
                    };
                    Some((d.item.clone(), d.is_static, reason))
                }
                _ => None,
            })
            .collect()
    })
}

const RECURSES: &str = r#"
const fn f(x: u32) {
    f(x);
}
"#;

/// tests/ui/consts/recursive.rs: nothing uses `X`, and its evaluation still fails the check.
#[test]
fn an_unused_const_that_recurses_forever_is_an_error() {
    let db = fixture::db(&format!("{RECURSES}const X: () = f(1);")).into_db();
    assert_eq!(failures(&db), [(
        "X".to_owned(),
        false,
        "reached the configured maximum number of stack frames".to_owned()
    )]);
}

#[test]
fn a_static_is_evaluated_as_a_const_is() {
    let db = fixture::db(&format!("{RECURSES}static S: () = f(1);")).into_db();
    assert_eq!(failures(&db), [(
        "S".to_owned(),
        true,
        "reached the configured maximum number of stack frames".to_owned()
    )]);
}

/// rustc evaluates no associated `const` until something uses it (`const_eval_poly` runs on
/// free ones only), and no `const` inside a function body is left out.
#[test]
fn an_associated_const_is_not_evaluated_and_a_const_in_a_body_is() {
    let db = fixture::db(&format!(
        "{RECURSES}struct T;\nimpl T {{ const C: () = f(1); }}\nfn g() {{ const IN: () = f(2); }}"
    ))
    .into_db();
    let found = failures(&db);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, "IN");
}

/// A value whose body has an error of its own is not evaluated: the error is reported there.
#[test]
fn a_const_with_a_type_error_gets_no_evaluation_error() {
    let db = fixture::db("const T: u32 = \"a\";").into_db();
    let found = failures(&db);
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn values_that_evaluate_get_no_error() {
    let db = fixture::db("const A: u32 = 1 + 2;\nstatic B: u32 = A * 2;\nconst _: () = ();").into_db();
    let found = failures(&db);
    assert!(found.is_empty(), "{found:?}");
}
