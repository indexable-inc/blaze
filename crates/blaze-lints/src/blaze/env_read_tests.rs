//! `env_read` over a `std` of its own whose `env` has the four functions read and one that only
//! computes. The crate linted is the one at `/src/lib.rs`.

use std::path::PathBuf;

use super::{
    Lint,
    env_read::{is_allowed, message},
};
use crate::{
    conf::Conf,
    tests::{CLIPPY_OFF, lint, lint_both_builds, lint_with},
};

const STD: &str = r#"
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub mod env {
    pub struct Vars;
    pub fn var(_name: &str) -> Option<u32> {
        loop {}
    }
    pub fn var_os(_name: &str) -> Option<u32> {
        loop {}
    }
    pub fn vars() -> Vars {
        loop {}
    }
    pub fn vars_os() -> Vars {
        loop {}
    }
    pub fn split_paths(_paths: &str) -> u32 {
        loop {}
    }
}
"#;

/// Reads by a full path, through a `use` and an alias, twice in one function; the whole
/// environment; a function that only computes; a settings module; and a test. The first line
/// is line 1 of `src/lib.rs`.
const MAIN: &str = r#"use std::env::var_os;
use std::env as environment;
pub fn home() -> Option<u32> {
    let _a = std::env::var("HOME");
    let _b = environment::var("HOME");
    var_os("USER")
}
pub fn hand_on() -> std::env::Vars {
    std::env::vars_os()
}
pub fn paths() -> u32 {
    std::env::split_paths("a:b")
}
pub mod settings {
    pub fn read() -> Option<u32> {
        std::env::var("BLAZE_X")
    }
    pub mod below {
        pub fn read() -> Option<u32> {
            std::env::var("BLAZE_Y")
        }
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn reads() {
        let _ = std::env::var("BLAZE_TEST");
    }
}
"#;

fn fixture(main: &str) -> String {
    format!("//- minicore: sized, option\n{}//- /src/lib.rs crate:main deps:std\n{main}", STD.trim_start_matches('\n'))
}

const FLAGS: &str = "--deny=blaze::env_read";

fn flags() -> String {
    format!("{CLIPPY_OFF} -A blaze::general -A blaze::internal {FLAGS}")
}

/// `clippy.toml` allowing `main::settings`.
fn allowing() -> Conf {
    let text = "env-read-allowed = [\"main::settings\"]\n".to_owned();
    Conf::of_text(Some((PathBuf::from("clippy.toml"), text)), None).expect("a list of module paths")
}

/// The lines of `src/lib.rs` with a finding of `env_read`, in order.
fn lines(out: &str) -> Vec<u32> {
    let mut found = Vec::new();
    let mut ours = false;
    for line in out.lines() {
        if let Some(it) = line.strip_prefix("error: ") {
            ours = it.contains("which is not a settings module");
            continue;
        }
        let Some(at) = line.trim_start().strip_prefix("--> src/lib.rs:") else { continue };
        if std::mem::take(&mut ours) {
            found.push(at.split(':').next().and_then(|it| it.parse().ok()).expect("a line number"));
        }
    }
    found.sort_unstable();
    found
}

/// The finding: one per function read per body, at its first use, with its count there, by
/// what the path resolves to (`var` through a full path and an alias is one function; `var_os`
/// through a `use` another); the whole environment too; each a `deny` error naming the
/// module. With nothing allowed, the settings module and the one below it are found as well.
#[test]
fn a_read_outside_a_settings_module_is_refused() {
    let out = lint(&fixture(MAIN), &flags());
    assert_eq!(lines(&out), [4, 6, 9, 16, 20], "{out}");
    assert!(out.contains("error: `std::env::var`, read 2 times in `main`"), "{out}");
    assert!(out.contains("error: `std::env::var_os`, read once in `main`"), "{out}");
    assert!(out.contains("error: `std::env::vars_os`, read once in `main`"), "{out}");
    assert!(out.contains("in `main::settings::below`"), "{out}");
    assert!(out.contains("an input no key says"), "{out}");
    assert_eq!(out.matches(Lint::EnvRead.help()).count(), 5, "{out}");
    assert!(out.contains("lints.md#env_read"), "{out}");
}

/// The near misses: a module `env-read-allowed` names reads freely, but not a module below it;
/// a function of `std::env` that only computes and a test are no finding.
#[test]
fn a_settings_module_a_function_that_computes_and_a_test_are_not_findings() {
    let out = lint_with(&fixture(MAIN), &flags(), allowing());
    assert_eq!(lines(&out), [4, 6, 9, 20], "only `main::settings` itself is allowed:\n{out}");
    assert!(!out.contains("split_paths") && !out.contains("BLAZE_TEST"), "{out}");
    assert!(is_allowed("main::settings", &["main::settings".to_owned()]));
    assert!(!is_allowed("main::settings::below", &["main::settings".to_owned()]));
    assert!(!is_allowed("main", &[]));
}

/// A process that hands its environment on says so: the expectation is fulfilled by the
/// finding, which is not said, and no `unfulfilled_lint_expectations` is.
#[test]
fn an_expected_read_is_fulfilled() {
    const HANDS_ON: &str = r#"
#[expect(blaze::env_read, reason = "the client hands its whole environment to the daemon")]
pub fn hand_on() -> std::env::Vars {
    std::env::vars_os()
}
"#;
    let out = lint(&fixture(HANDS_ON), &flags());
    assert!(lines(&out).is_empty(), "{out}");
    assert!(!out.contains("unfulfilled"), "the expectation is met:\n{out}");
}

/// The invariant: a library's `--test` build judges its production code as its plain build
/// does, so the expectation on a function whose read is inside a closure is fulfilled in both
/// builds, and one on a function that reads nothing is unfulfilled in both. The `--test` build
/// once ran no `env_read` (it took the crate's target for a test target) and said the
/// expectation its plain build had met unfulfilled.
#[test]
fn an_expected_read_is_fulfilled_in_the_test_build_too() {
    const READS: &str = r#"
pub fn given() {
    let _read = |name: &str| std::env::var(name);
}
"#;
    const EXPECTED: &str = r#"
#[expect(
    blaze::env_read,
    reason = "the entry point reads its settings once"
)]
pub fn given() {
    let _read = |name: &str| std::env::var(name);
}
"#;
    const MISSED: &str = r#"
#[expect(
    blaze::env_read,
    reason = "the entry point reads its settings once"
)]
pub fn paths() -> u32 {
    std::env::split_paths("a:b")
}
"#;
    for out in lint_both_builds(&fixture(READS), &flags()) {
        assert_eq!(lines(&out).len(), 1, "{out}");
    }
    for out in lint_both_builds(&fixture(EXPECTED), &flags()) {
        assert!(lines(&out).is_empty(), "{out}");
        assert!(!out.contains("lint expectation is unfulfilled"), "the expectation is met:\n{out}");
    }
    for out in lint_both_builds(&fixture(MISSED), &flags()) {
        assert!(lines(&out).is_empty(), "{out}");
        assert_eq!(out.matches("lint expectation is unfulfilled").count(), 1, "{out}");
    }
}

/// A value of `env-read-allowed` that is not a list of paths is refused as the file loads.
#[test]
fn an_allowed_list_that_is_no_list_is_refused() {
    let text = "env-read-allowed = \"main::settings\"\n".to_owned();
    let err = Conf::of_text(Some((PathBuf::from("clippy.toml"), text)), None).expect_err("a string is no list");
    assert!(err.contains("expected a list of module paths for `env-read-allowed`"), "{err}");
}

#[test]
fn a_finding_names_the_function_its_count_and_the_module() {
    assert_eq!(
        message("std::env::var", 2, "blaze_cargo::cli"),
        "`std::env::var`, read 2 times in `blaze_cargo::cli`, which is not a settings module (`env-read-allowed` in clippy.toml)"
    );
}
