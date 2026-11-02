//! rustc's UI tests of the errors of items, names, attributes, impls, macros and syntax.

mod ast_validation;
mod attrs;
mod crate_attr;
mod defs;
mod features;
mod generics;
mod impls;
mod krate;
mod lifetimes;
mod macros;
mod names;
mod parse;
mod refused;
mod resolve;

use crate::{
    Options,
    emit::Style,
    tests::{Mode, compiletest},
};

/// [`crate::tests::ui`] of a UI test that needs core's items: `flags` are minicore's
/// (`crates/test-utils/src/minicore.rs`), and core is in the prelude as std is for rustc. The
/// file's lines are the source's, as the `.stderr` counts them.
pub(crate) fn ui_core(name: &str, edition: &str, flags: &str, source: &str) -> String {
    ui_fixture(&format!("//- minicore: {flags}\n//- /{name} crate:other:main edition:{edition}\n{source}"))
}

/// [`crate::tests::ui`] of a UI test rustc builds as a binary (its default), named `krate` as
/// rustc names it from the file (`missing-main.rs` is `missing_main`).
pub(crate) fn ui_bin(name: &str, krate: &str, edition: &str, source: &str) -> String {
    ui_fixture(&format!("//- /{name} crate:bin:{krate} edition:{edition}\n{source}"))
}

/// The `.stderr` of a fixture's crates, as [`crate::tests::ui`] writes it: with compiletest's
/// flags, in `-Z ui-testing` style, normalized.
fn ui_fixture(fixture: &str) -> String {
    let options = compiletest(Mode::Other, &Options::default());
    let checked = crate::tests::checked_with(fixture, "$DIR/", &options);
    crate::tests::normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() }))
}
