#[test]
fn names_are_three() {
    assert_eq!(soak_core::names().len(), 3);
}

#[test]
fn level_counts_lines() {
    assert_eq!(soak_core::level(), soak_core::table::ALPHA + 3);
}
