#[test]
fn the_macro_expands() {
    assert_eq!(repro::answer!(), 42);
}
