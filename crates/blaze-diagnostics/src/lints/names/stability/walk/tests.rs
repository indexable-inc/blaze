//! The walk's paths it can't resolve: each is checked, or is name resolution's own error.

use crate::{
    emit::Style,
    tests::{assert_reports, checked, error, warning},
};

/// Queue 01515: a module a block declares with `#[path]` is no crate def map's, and the walk
/// once skipped its paths because it could not resolve them.
#[test]
fn a_use_in_a_module_a_block_declares_is_checked() {
    let checked = checked(
        r#"
//- /main.rs crate:main
#[deprecated]
pub fn d() {}
fn main() {
    #[path = "m.rs"]
    mod m;
}
//- /m.rs
pub fn inner() {
    crate::d();
}
"#,
    );
    assert_reports(&checked.stderr_with(Style::default()), &[warning(2, &["deprecated"])]);
}

/// rustc's `Res::Err`: a path name resolution can't resolve is its own error, and no
/// stability check has anything to say of it.
#[test]
fn an_unresolved_path_is_only_its_own_error() {
    let checked = checked(
        r#"
//- /main.rs crate:main
fn main() {
    nope::f();
}
"#,
    );
    assert_reports(&checked.stderr_with(Style::default()), &[error(2, &["`nope`"])]);
}
