use std::time::Duration;

use blaze_api::{CrateType, UnitTarget};

use super::*;

/// A target of cargo's `kind` word (`lib`, `bin`, `custom-build`, ...).
pub(crate) fn target(kind: &str, name: &str) -> UnitTarget {
    UnitTarget {
        kind: TargetKind::named(kind).unwrap(),
        crate_types: vec![if kind == "lib" { CrateType::Lib } else { CrateType::Bin }],
        name: name.to_owned(),
        src_path: format!("/w/{name}/src/main.rs"),
        edition: "2021".to_owned(),
        doc: kind != "test",
        doctest: kind == "lib",
        test: true,
    }
}

#[test]
fn compiling_a_path_a_git_and_a_registry_package() {
    let path = package("foo", "0.1.0", Some("(/w/foo)"));
    assert_eq!(status("Compiling", &path, false), "   Compiling foo v0.1.0 (/w/foo)\n");
    let git = package("bar", "0.2.0", Some("(https://github.com/o/r#0123abcd)"));
    assert_eq!(
        status("Compiling", &git, false),
        "   Compiling bar v0.2.0 (https://github.com/o/r#0123abcd)\n"
    );
    let registry = package("serde", "1.0.219", None);
    assert_eq!(status("Compiling", &registry, false), "   Compiling serde v1.0.219\n");
    assert_eq!(
        status("Compiling", &path, true),
        "\x1b[1m\x1b[92m   Compiling\x1b[0m foo v0.1.0 (/w/foo)\n"
    );
    assert_eq!(status("Fresh", &registry, false), "       Fresh serde v1.0.219\n");
}

#[test]
fn errors_and_warnings_as_cargos_shell() {
    assert_eq!(error("what", false), "error: what\n");
    assert_eq!(error("what", true), "\x1b[1m\x1b[91merror\x1b[0m: what\n");
    assert_eq!(warning("what", false), "warning: what\n");
    assert_eq!(warning("what", true), "\x1b[1m\x1b[93mwarning\x1b[0m: what\n");
    let mut note = String::new();
    note_into(&mut note, "what", true);
    assert_eq!(note, "\x1b[1m\x1b[92mnote\x1b[0m: what\n");
}

#[test]
fn texts_into_a_buffer_as_their_own() {
    let bin = target("bin", "foo");
    let mut out = String::new();
    finished_into(&mut out, "`dev` profile [unoptimized + debuginfo]", std::time::Duration::from_millis(1234));
    assert_eq!(out, finished("`dev` profile [unoptimized + debuginfo]", std::time::Duration::from_millis(1234)));
    out.clear();
    could_not_compile_into(&mut out, "foo", &bin, false, 1, 0);
    assert_eq!(out, could_not_compile("foo", &bin, false, 1, 0));
    out.clear();
    assert!(!generated_into(&mut out, "foo", &bin, false, true, Warned::default()));
    assert_eq!(out, "", "nothing for a unit without warnings");
}

#[cfg(not(windows))]
#[test]
fn a_command_with_cargos_variables_sorted() {
    let env = [("ZED".to_owned(), "a b".to_owned()), ("CARGO_PKG_NAME".to_owned(), "foo".to_owned())];
    assert_eq!(command_line_with(&env, "target/debug/foo", &["x".to_owned()]), "`CARGO_PKG_NAME=foo ZED='a b' target/debug/foo x`");
}

#[test]
fn generated_warnings() {
    let lib = target("lib", "foo");
    let one = Warned { total: 1, duplicates: 0, fixable: Some(0) };
    assert_eq!(generated("foo", &lib, false, true, one).unwrap(), "`foo` (lib) generated 1 warning");
    let two = Warned { total: 2, duplicates: 0, fixable: Some(0) };
    assert_eq!(generated("foo", &lib, false, true, two).unwrap(), "`foo` (lib) generated 2 warnings");
    let bin = target("bin", "foo");
    let fixable = Warned { total: 2, duplicates: 1, fixable: Some(1) };
    assert_eq!(
        generated("foo", &bin, false, true, fixable).unwrap(),
        "`foo` (bin \"foo\") generated 2 warnings (1 duplicate) (run `cargo fix --bin \"foo\" -p foo` to apply 1 suggestion)"
    );
    let lib_fixable = Warned { total: 3, duplicates: 2, fixable: Some(2) };
    assert_eq!(
        generated("foo", &lib, false, true, lib_fixable).unwrap(),
        "`foo` (lib) generated 3 warnings (2 duplicates) (run `cargo fix --lib -p foo` to apply 2 suggestions)"
    );
    assert_eq!(
        generated("foo", &lib, true, true, one).unwrap(),
        "`foo` (lib test) generated 1 warning"
    );
    // A test or bench target is always compiled with `--test`, and named without " test".
    for kind in ["test", "bench"] {
        let tests = target(kind, "it");
        assert_eq!(generated("foo", &tests, true, true, one).unwrap(), format!("`foo` ({kind} \"it\") generated 1 warning"));
        assert_eq!(could_not_compile("foo", &tests, true, 1, 0), format!("could not compile `foo` ({kind} \"it\") due to 1 previous error"));
    }
    // Not local, or with an error: no hint.
    assert_eq!(generated("foo", &lib, false, false, lib_fixable).unwrap(), "`foo` (lib) generated 3 warnings (2 duplicates)");
    let errored = Warned { total: 1, duplicates: 0, fixable: None };
    assert_eq!(generated("foo", &lib, false, true, errored).unwrap(), "`foo` (lib) generated 1 warning");
    assert_eq!(generated("foo", &lib, false, true, Warned::default()), None);
    let script = target("custom-build", "build-script-build");
    let script_fixable = Warned { total: 1, duplicates: 0, fixable: Some(1) };
    assert_eq!(
        generated("foo", &script, false, true, script_fixable).unwrap(),
        "`foo` (build script) generated 1 warning"
    );
}

#[test]
fn could_not_compile_a_unit() {
    let lib = target("lib", "foo");
    assert_eq!(
        could_not_compile("foo", &lib, false, 1, 1),
        "could not compile `foo` (lib) due to 1 previous error; 1 warning emitted"
    );
    let bin = target("bin", "foo");
    assert_eq!(
        could_not_compile("foo", &bin, false, 2, 0),
        "could not compile `foo` (bin \"foo\") due to 2 previous errors"
    );
    assert_eq!(
        could_not_compile("foo", &lib, false, 3, 4),
        "could not compile `foo` (lib) due to 3 previous errors; 4 warnings emitted"
    );
    assert_eq!(
        error(&could_not_compile("foo", &bin, false, 1, 0), false),
        "error: could not compile `foo` (bin \"foo\") due to 1 previous error\n"
    );
}

#[test]
fn the_finished_line() {
    let profile = "`dev` profile [unoptimized + debuginfo]";
    let took = Duration::from_millis(123);
    assert_eq!(
        status("Finished", &finished(profile, took), false),
        "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.12s\n"
    );
    assert_eq!(elapsed(Duration::from_millis(5)), "0.00s");
    assert_eq!(elapsed(Duration::from_millis(59_999)), "59.99s");
    assert_eq!(elapsed(Duration::from_secs(65)), "1m 05s");
    assert_eq!(elapsed(Duration::from_secs(3600 + 7)), "60m 07s");
}

#[test]
fn package_ids() {
    assert_eq!(package_name("path+file:///w/foo#0.1.0"), "foo");
    assert_eq!(package_version("path+file:///w/foo#0.1.0"), "0.1.0");
    assert_eq!(package_name("path+file:///w/dir#bar@0.2.0"), "bar");
    assert_eq!(package_version("path+file:///w/dir#bar@0.2.0"), "0.2.0");
    let registry = "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.219";
    assert_eq!((package_name(registry), package_version(registry)), ("serde", "1.0.219"));
    let git = "git+https://github.com/o/r?branch=main#r@0.1.0";
    assert_eq!(package_name(git), "r");
}

#[test]
fn commands_as_cargo_quotes_them() {
    assert_eq!(escape_unix("target/debug/foo"), "target/debug/foo");
    assert_eq!(escape_unix("a b"), "'a b'");
    assert_eq!(escape_unix(""), "''");
    assert_eq!(escape_unix("it's"), "'it'\\''s'");
    assert_eq!(escape_windows("a b"), "\"a b\"");
    assert_eq!(escape_windows("plain"), "plain");
    assert_eq!(escape_windows("a\\\"b c"), "\"a\\\\\\\"b c\"");
    #[cfg(not(windows))]
    assert_eq!(command_line("target/debug/foo", &["x".to_owned(), "y z".to_owned()]), "`target/debug/foo x 'y z'`");
}

#[test]
fn how_a_program_ended() {
    #[cfg(not(windows))]
    assert_eq!(ended(Ended::Code(2)), "exit status: 2");
    #[cfg(unix)]
    assert_eq!(ended(Ended::Signal(libc::SIGSEGV)), format!("signal: {}, SIGSEGV: invalid memory reference", libc::SIGSEGV));
    assert!(Ended::Code(0).success() && !Ended::Code(101).success());
}

#[test]
fn colors_come_off() {
    assert_eq!(strip_ansi("\x1b[1m\x1b[91merror\x1b[0m: x"), "error: x");
    assert_eq!(strip_ansi("a\x1b]8;;http://x\x1b\\link\x1b]8;;\x07b"), "alinkb");
    assert_eq!(strip_ansi("plain"), "plain");
}
