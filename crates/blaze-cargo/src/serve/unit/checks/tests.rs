use blaze_diagnostics::{Level, Span};

use super::*;

fn at(lo: u32) -> Span {
    Span { file: "src/lib.rs".to_owned(), lo, hi: lo + 1, expansion: None }
}

#[test]
fn a_check_fails_its_unit_on_an_error_cargo_shows_and_nothing_else() {
    let error = Diag::error(Some("E0308"), "mismatched types", at(0));
    let warning = Diag::lint("dead_code", "function `f` is never used", at(1));
    let mut denied = Diag::lint("non_snake_case", "function `F` should have a snake case name", at(2));
    denied.level = Level::Error;
    let closing = Diag::new(Level::Error, None, "aborting due to 1 previous error");

    assert!(!failed(&[]));
    assert!(!failed(std::slice::from_ref(&warning)));
    assert!(!failed(std::slice::from_ref(&closing)), "a closing line cargo hides is no error of the unit");
    assert!(failed(&[warning.clone(), error]));
    assert!(failed(&[warning, denied]), "a denied lint fails the unit as rustc's error does");
}
