use blaze_diagnostics::{Level, Span};

use super::*;

fn at(lo: u32) -> Span {
    Span { file: "src/lib.rs".to_owned(), lo, hi: lo + 1, expansion: None }
}

#[test]
fn a_check_fails_its_unit_on_an_error_cargo_shows_and_nothing_else() {
    let error = Diag::error(Some("E0308"), "mismatched types", at(0));
    let warning = Diag::lint("dead_code", "function `f` is never used", at(1));
    let mut denied =
        Diag::lint("non_snake_case", "function `F` should have a snake case name", at(2));
    denied.level = Level::Error;
    let closing = Diag::new(Level::Error, None, "aborting due to 1 previous error");

    assert!(!failed(&[]));
    assert!(!failed(std::slice::from_ref(&warning)));
    assert!(
        !failed(std::slice::from_ref(&closing)),
        "a closing line cargo hides is no error of the unit"
    );
    assert!(failed(&[warning.clone(), error]));
    assert!(failed(&[warning, denied]), "a denied lint fails the unit as rustc's error does");
}

/// A crate with a type error in one body and none in another.
const ONE_BODY_BROKEN: &str = r#"
//- /lib.rs crate:main
pub fn clean() -> u8 { 1 }
pub fn broken() { let _: u8 = (); }
"#;

/// A unit's body check gives way (a cycle of waits, memory) and its task is posted again
/// whole, with the `Checking` its names phase made: the run after gives what the first gave,
/// once more and again after that, where a plan run twice panics ("a plan of a check runs
/// once"). Each run is the whole check (`blaze_diagnostics::check_planned`), so this holds a
/// run that gave way after its plan ended; codegen's `reached` between two runs is taken.
#[test]
fn a_check_posted_again_runs_a_plan_of_its_own() {
    let fixture = hir_expand::fixture::db(ONE_BODY_BROKEN);
    let file = fixture.file_at("/lib.rs").file_id(fixture.db());
    let db = fixture.into_db();
    let names = [(file, "src/lib.rs".to_owned())].into_iter().collect();
    let files = Files::new(&names, "");
    let options = Options::default();
    let krate = blaze_query::db::attach(&db, || hir::check::workspace_crates(&db)[0]);
    let checking = blaze_query::db::attach(&db, || Checking::new(&db, krate));

    let first = checking.check(&db, files, &options);
    assert!(failed(&first.diags), "the broken body fails the check: {:?}", first.diags);
    assert!(checking.stopped());
    for _ in 0..2 {
        checking.reached(&[]);
        let again = checking.check(&db, files, &options);
        assert_eq!(again.diags, first.diags);
        assert_eq!(again.items_broken, first.items_broken);
    }
}
