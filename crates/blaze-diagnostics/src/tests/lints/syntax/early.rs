//! The rest of the early lint pass and of the lints the lexer, the parser, expansion and
//! resolution buffer, against rustc's UI tests.

mod builtin;
mod labels;
mod lexer;
mod macros;
mod validation;

use crate::{
    Options,
    emit::Style,
    tests::{Mode, checked_with, compiletest, normalized},
};

/// What rustc writes to stderr for `fixture`, each file named `$DIR/<path>`, as compiletest runs
/// a UI test that is not `//@ run-pass`: a UI test's `.stderr`.
fn stderr(fixture: &str) -> String {
    let stderr = checked_with(fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()))
        .stderr_with(Style { ui_testing: true, ..Style::default() });
    normalized(&stderr)
}

/// [`crate::tests::ui`] of a UI test that needs core's items: core is minicore with `flags`
/// (`crates/test-utils/src/minicore.rs`).
pub(crate) fn ui_core(name: &str, edition: &str, flags: &str, source: &str) -> String {
    stderr(&format!("//- minicore: {flags}\n//- /{name} crate:other:main edition:{edition}\n{source}"))
}

/// [`ui_core`] of a UI test that uses std: `std` is a crate of `std_items`, written as std
/// writes them, with a prelude of core's and of `prelude`, what std's prelude adds that the
/// test uses.
pub(crate) fn ui_std(name: &str, edition: &str, flags: &str, std: (&str, &str), source: &str) -> String {
    let (std_items, prelude) = std;
    let preludes: String = ["rust_2015", "rust_2018", "rust_2021", "rust_2024"]
        .iter()
        .map(|it| format!("    pub mod {it} {{ pub use core::prelude::{it}::*; {prelude} }}\n"))
        .collect();
    stderr(&format!(
        "//- minicore: {flags}\n//- /std.rs crate:std\npub mod prelude {{\n{preludes}}}\n{std_items}\n\
         //- /{name} crate:other:main edition:{edition} deps:std\n{source}"
    ))
}
