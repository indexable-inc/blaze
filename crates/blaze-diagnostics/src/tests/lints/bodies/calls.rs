//! rustc's UI tests of the late lint passes of calls, statements, `let`s and patterns in bodies.

mod desugar;
mod items;
mod methods;
mod statements;

use crate::{
    Options,
    emit::Style,
    lints::CommandLine,
    tests::{Mode, compiletest},
};

/// A UI test: its file's name and edition, what its crate is built with, and its source byte
/// for byte, so that the lines and columns are its `.stderr`'s.
pub(crate) struct Ui<'a> {
    pub(crate) name: &'a str,
    pub(crate) edition: &'a str,
    /// `Mode::RunPass` for a `//@ run-pass` test, which compiletest gives no `-A unused`.
    pub(crate) mode: Mode,
    /// minicore's flags (`crates/test-utils/src/minicore.rs`) when the test needs core.
    pub(crate) core: &'a str,
    /// What std's prelude has beyond core's that the test uses, and the items of std it uses,
    /// written as std writes them (their `#[rustc_diagnostic_item]`s and `#[lang]`s too), when
    /// it uses std; std needs `core`.
    pub(crate) std: Option<(&'a str, &'a str)>,
    /// More of the main crate's fixture meta: `cfg:deny_level` for a revision.
    pub(crate) meta: &'a str,
    /// The test's `//@ compile-flags` of lints: `-D path-statements`.
    pub(crate) flags: &'a [&'a str],
    pub(crate) source: &'a str,
}

impl Default for Ui<'_> {
    fn default() -> Self {
        Ui { name: "", edition: "2015", mode: Mode::Other, core: "", std: None, meta: "", flags: &[], source: "" }
    }
}

/// `-D path-statements` as the two arguments rustc reads it as.
fn split_flag(flag: &str) -> Vec<&str> {
    match flag.split_once(' ') {
        Some((level, lint)) => vec![level, lint],
        None => vec![flag],
    }
}

/// compiletest's options for `ui`, its lint flags before the test's own.
fn options(ui: &Ui<'_>) -> Options {
    let args: Vec<String> = ui.flags.iter().flat_map(|flag| split_flag(flag)).map(str::to_owned).collect();
    let own = Options { lints: CommandLine::parse(&args).unwrap(), ..Options::default() };
    compiletest(ui.mode, &own)
}

/// What rustc writes to stderr for `ui`: its `.stderr`.
pub(crate) fn ui(ui: Ui<'_>) -> String {
    let mut fixture = String::new();
    if !ui.core.is_empty() {
        fixture.push_str(&format!("//- minicore: {}\n", ui.core));
    }
    let deps = match ui.std {
        Some((prelude, items)) => {
            assert!(!ui.core.is_empty(), "std's prelude is core's: give minicore's flags");
            // Without the feature, hir-def collects none of the crate's `#[lang]` items.
            fixture.push_str("//- /std.rs crate:std\n#![feature(lang_items, rustc_attrs)]\npub mod prelude {\n");
            for edition in ["rust_2015", "rust_2018", "rust_2021", "rust_2024"] {
                fixture.push_str(&format!(
                    "    pub mod {edition} {{ pub use core::prelude::{edition}::*; pub use crate::__prelude::*; }}\n"
                ));
            }
            fixture.push_str(&format!("}}\n#[doc(hidden)]\npub mod __prelude {{\n{prelude}\n}}\n{items}\n"));
            " deps:std"
        }
        None => "",
    };
    let meta = if ui.meta.is_empty() { String::new() } else { format!(" {}", ui.meta) };
    fixture.push_str(&format!("//- /{} crate:other:main edition:{}{deps}{meta}\n{}", ui.name, ui.edition, ui.source));
    let stderr = crate::tests::checked_with(&fixture, "$DIR/", &options(&ui))
        .stderr_with(Style { ui_testing: true, ..Style::default() });
    crate::tests::normalized(&stderr)
}
