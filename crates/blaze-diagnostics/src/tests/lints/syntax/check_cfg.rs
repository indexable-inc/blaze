//! `--check-cfg`, `unexpected_cfgs` and `explicit_builtin_cfgs_in_flags`, against rustc's UI
//! tests (`tests/ui/check-cfg/*`, `tests/ui/cfg/disallowed-cli-cfgs.rs`).

mod add;
mod builtin;
mod unexpected;

use crate::{
    Options,
    emit::Style,
    tests::{Mode, checked_with, compiletest, normalized},
};

/// The options of a UI test's `compile-flags`: its `--check-cfg`s and `--cfg`s, and whether
/// cargo runs rustc (`rustc-env:CARGO_CRATE_NAME=..`).
fn flags(check_cfg: &[&str], cfg: &[&str], cargo: bool) -> Options {
    let mut options = Options::default();
    for spec in check_cfg {
        options.lints.check_cfg.add(spec).expect("the test's `--check-cfg` is rustc's");
    }
    options.lints.cfg.extend(cfg.iter().map(|&it| it.to_owned()));
    options.lints.check_cfg.cargo = cargo;
    options
}

/// compiletest's `--check-cfg` of a test without `no-auto-check-cfg` and with no revisions.
const AUTO: &str = "cfg(test,FALSE)";

/// [`crate::tests::ui_with`] of a fixture whose crate `main` is the UI test, with the files
/// `before` it, and the cfgs `cfg` set (`cfg:` of a fixture): what the test's `--cfg`s make
/// true. The output is as compiletest compares it: `\\` as `\`
/// (`normalize_platform_differences`), which a blessed `.stderr` already is.
fn ui_cfg(before: &str, name: &str, header: &str, source: &str, options: &Options) -> String {
    let fixture = format!("{before}//- /{name} crate:other:main {header}\n{source}");
    let stderr = checked_with(&fixture, "$DIR/", &compiletest(Mode::Other, options))
        .stderr_with(Style { ui_testing: true, ..Style::default() });
    platform(&normalized(&stderr))
}

fn platform(text: &str) -> String {
    text.replace("\\\\", "\\")
}
