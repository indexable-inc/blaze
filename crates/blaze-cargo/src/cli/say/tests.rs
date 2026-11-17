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
    let mut out = String::new();
    finished_into(
        &mut out,
        "`dev` profile [unoptimized + debuginfo]",
        std::time::Duration::from_millis(1234),
    );
    assert_eq!(
        out,
        finished("`dev` profile [unoptimized + debuginfo]", std::time::Duration::from_millis(1234))
    );
}

#[cfg(not(windows))]
#[test]
fn a_command_with_cargos_variables_sorted() {
    let env =
        [("ZED".to_owned(), "a b".to_owned()), ("CARGO_PKG_NAME".to_owned(), "foo".to_owned())];
    assert_eq!(
        command_line_with(&env, "target/debug/foo", &["x".to_owned()]),
        "`CARGO_PKG_NAME=foo ZED='a b' target/debug/foo x`"
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
    assert_eq!(
        command_line("target/debug/foo", &["x".to_owned(), "y z".to_owned()]),
        "`target/debug/foo x 'y z'`"
    );
}

#[test]
fn how_a_program_ended() {
    #[cfg(not(windows))]
    assert_eq!(ended(Ended::Code(2)), "exit status: 2");
    #[cfg(unix)]
    assert_eq!(
        ended(Ended::Signal(libc::SIGSEGV)),
        format!("signal: {}, SIGSEGV: invalid memory reference", libc::SIGSEGV)
    );
    assert!(Ended::Code(0).success() && !Ended::Code(101).success());
}

#[test]
fn colors_come_off() {
    assert_eq!(strip_ansi("\x1b[1m\x1b[91merror\x1b[0m: x"), "error: x");
    assert_eq!(strip_ansi("a\x1b]8;;http://x\x1b\\link\x1b]8;;\x07b"), "alinkb");
    assert_eq!(strip_ansi("plain"), "plain");
}
