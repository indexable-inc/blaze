//! `unused_parens`, `unused_braces` and `unused_import_braces`, against rustc's UI tests.

mod braces;
mod patterns;
mod types;
mod values;

use crate::{
    Options,
    emit::Style,
    tests::{Mode, compiletest, normalized},
};

/// [`crate::tests::ui`] of a UI test that uses items of core: core is minicore with `flags`
/// (`crates/test-utils/src/minicore.rs`). The test's file is the UI test's source byte for
/// byte, so the lines and columns are its `.stderr`'s.
fn ui_core(name: &str, edition: &str, flags: &str, source: &str) -> String {
    let fixture = format!("//- minicore: {flags}\n//- /{name} crate:other:main edition:{edition}\n{source}");
    let options = compiletest(Mode::Other, &Options::default());
    let stderr =
        crate::tests::checked_with(&fixture, "$DIR/", &options).stderr_with(Style { ui_testing: true, ..Style::default() });
    normalized(&stderr)
}
