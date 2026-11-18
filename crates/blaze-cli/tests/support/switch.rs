//! blaze-cli's `BORROWS_ARE_ERRORS`, read from its one line in src/body_types.rs (as
//! blaze-ladder's tests/flow_census_shared reads it): what a clean `blaze check` exits with
//! follows it (3 while the borrows stage is not built, 0 once it is). One home for the tests
//! that need it, each taking it with `#[path = "support/switch.rs"] mod switch;` (a module of
//! its own, so the tests that do not use it carry no unused function).

/// The line the switch is on, up to its value.
const SWITCH: &str = "pub const BORROWS_ARE_ERRORS: bool = ";

/// The switch's value; a missing or malformed line fails the test by name.
pub fn borrows_are_errors() -> bool {
    let source = include_str!("../../src/body_types.rs");
    let line = source.lines().find(|line| line.starts_with(SWITCH)).unwrap_or_else(|| panic!("no line `{SWITCH}<bool>;` in crates/blaze-cli/src/body_types.rs"));
    match line[SWITCH.len()..].trim_end() {
        "true;" => true,
        "false;" => false,
        other => panic!("`BORROWS_ARE_ERRORS` in crates/blaze-cli/src/body_types.rs is `{other}`, not `true;` or `false;`"),
    }
}
