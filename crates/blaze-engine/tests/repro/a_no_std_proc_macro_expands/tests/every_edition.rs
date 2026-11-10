#[test]
fn every_no_std_macro_expands() {
    assert_eq!(m2015::answer!(), 2015);
    assert_eq!(m2018::answer!(), 2018);
    assert_eq!(m2021::answer!(), 2021);
    assert_eq!(repro::f(), 2024);
    assert_eq!(own_std::answer!(), 7);
}
