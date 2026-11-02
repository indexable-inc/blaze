use std::{ffi::OsString, path::Path};

use blaze_api::{Artifact, ArtifactKind, CrateType, TargetKind, UnitProfile, UnitTarget};
use blaze_ids::ContentId;

use super::*;
use crate::cli::args::Flags;

/// An artifact of package `package` at `/w/<package>`, of a target `kind` named `name`; an
/// executable under `target/debug` when `exe`.
pub(crate) fn artifact(package: &str, kind: &str, name: &str, test: bool, exe: Option<&str>) -> Artifact {
    let src_path = match kind {
        "lib" => format!("/w/{package}/src/lib.rs"),
        "bin" if name == package => format!("/w/{package}/src/main.rs"),
        "bin" => format!("/w/{package}/src/bin/{name}.rs"),
        "test" => format!("/w/{package}/tests/{name}.rs"),
        "bench" => format!("/w/{package}/benches/{name}.rs"),
        "example" => format!("/w/{package}/examples/{name}.rs"),
        _ => format!("/w/{package}/src/lib.rs"),
    };
    let target = UnitTarget {
        kind: TargetKind::named(kind).unwrap(),
        crate_types: vec![if kind == "lib" { CrateType::Lib } else { CrateType::Bin }],
        name: name.to_owned(),
        src_path,
        edition: "2024".to_owned(),
        doc: kind == "lib" || kind == "bin",
        doctest: kind == "lib",
        test: true,
    };
    let path = exe.map_or_else(|| format!("/w/target/debug/lib{name}.rlib"), |it| format!("/w/target/debug/{it}"));
    Artifact {
        package: format!("path+file:///w/{package}#0.1.0"),
        kind: if test { ArtifactKind::Test } else { ArtifactKind::named(kind).unwrap() },
        path: path.clone(),
        content: ContentId([0; 32]),
        manifest_path: format!("/w/{package}/Cargo.toml"),
        target,
        profile: UnitProfile {
            opt_level: "0".to_owned(),
            debuginfo: 2,
            debug_assertions: true,
            overflow_checks: true,
            test,
        },
        features: Vec::new(),
        filenames: vec![path.clone()],
        executable: exe.map(|_| path),
        fresh: false,
        runner: Vec::new(),
        env: vec![("CARGO_PKG_NAME".to_owned(), package.to_owned())],
        harness: true,
    }
}

fn flags(bins: &[Option<&str>], examples: &[Option<&str>]) -> Flags {
    let names = |it: &[Option<&str>]| it.iter().map(|it| it.map(str::to_owned)).collect();
    Flags { bins: names(bins), examples: names(examples), ..Flags::default() }
}

#[test]
fn the_only_bin_is_run() {
    let built = [artifact("foo", "lib", "foo", false, None), artifact("foo", "bin", "foo", false, Some("foo"))];
    assert_eq!(pick(&built, &flags(&[], &[])).unwrap().target.name, "foo");
}

#[test]
fn a_bin_by_name() {
    let built = [artifact("foo", "bin", "foo", false, Some("foo")), artifact("foo", "bin", "tool", false, Some("tool"))];
    assert_eq!(pick(&built, &flags(&[Some("tool")], &[])).unwrap().target.name, "tool");
    let examples = [artifact("foo", "example", "demo", false, Some("examples/demo"))];
    assert_eq!(pick(&examples, &flags(&[], &[Some("demo")])).unwrap().target.name, "demo");
}

#[test]
fn default_run_is_the_one_the_daemon_built() {
    // `default-run = "tool"`: cargo's `commands/run.rs` builds that bin alone.
    let built = [artifact("foo", "lib", "foo", false, None), artifact("foo", "bin", "tool", false, Some("tool"))];
    assert_eq!(pick(&built, &flags(&[], &[])).unwrap().target.name, "tool");
}

#[test]
fn two_bins_are_cargos_error() {
    let built = [artifact("foo", "bin", "foo", false, Some("foo")), artifact("foo", "bin", "bar", false, Some("bar"))];
    assert_eq!(
        pick(&built, &flags(&[], &[])).unwrap_err(),
        "`cargo run` could not determine which binary to run. Use the `--bin` option to specify a \
         binary, or the `default-run` manifest key.\navailable binaries: bar, foo"
    );
    assert_eq!(
        pick(&built, &flags(&[Some("foo"), Some("bar")], &[])).unwrap_err(),
        "`cargo run` can run at most one executable, but multiple were specified\nhelp: available targets:\n    \
         bin `foo` in package `foo`\n    bin `bar` in package `foo`"
    );
}

#[test]
fn no_bin_and_a_library_example() {
    let lib = [artifact("foo", "lib", "foo", false, None)];
    assert_eq!(pick(&lib, &flags(&[], &[])).unwrap_err(), "a bin target must be available for `cargo run`");
    let example = [artifact("foo", "example", "plugin", false, None)];
    assert_eq!(
        pick(&example, &flags(&[], &[Some("plugin")])).unwrap_err(),
        "example target `plugin` is a library and cannot be executed"
    );
    // A test executable of a bin is no program to run.
    let test = [artifact("foo", "bin", "foo", true, Some("deps/foo-0123abcd"))];
    assert!(pick(&test, &flags(&[], &[])).is_err());
}

#[test]
fn the_program_as_cargo_shows_and_runs_it() {
    assert_eq!(shown_exe(Path::new("/w/target/debug/foo"), Path::new("/w")), Path::new("target/debug/foo"));
    assert_eq!(shown_exe(Path::new("/w/target/debug/foo"), Path::new("/w/target/debug")), Path::new("./foo"));
    assert_eq!(shown_exe(Path::new("/w/target/debug/foo"), Path::new("/elsewhere")), Path::new("/w/target/debug/foo"));

    let foo = artifact("foo", "bin", "foo", false, Some("foo"));
    let args = [OsString::from("a"), OsString::from("b c")];
    let exec = planned(&foo, &args, Path::new("/w/foo")).unwrap();
    assert_eq!(exec.program, Path::new("/w/target/debug/foo"));
    assert_eq!(exec.cwd, Path::new("/w/foo"));
    assert_eq!(exec.env, [("CARGO_PKG_NAME".to_owned(), "foo".to_owned())]);
    let exec = planned(&foo, &args, Path::new("/w")).unwrap();
    assert_eq!(exec.shown(), "`target/debug/foo a 'b c'`");

    let runner = Artifact { runner: vec!["qemu-aarch64".to_owned(), "-L".to_owned(), "/sysroot".to_owned()], ..foo };
    let exec = planned(&runner, &args, Path::new("/w")).unwrap();
    assert_eq!(exec.shown(), "`qemu-aarch64 -L /sysroot target/debug/foo a 'b c'`");
}

#[cfg(unix)]
#[test]
fn extra_verbose_shows_the_variables() {
    let foo = artifact("foo", "bin", "foo", false, Some("foo"));
    let exec = planned(&foo, &[OsString::from("a")], Path::new("/w")).unwrap();
    assert_eq!(exec.shown_at(1), "`target/debug/foo a`");
    assert_eq!(exec.shown_at(2), "`CARGO_PKG_NAME=foo target/debug/foo a`");
}

#[test]
fn a_cause_of_lines_indented_as_cargos_indented_lines() {
    assert_eq!(
        caused("test failed", &["first\n\nlast".to_owned()], false),
        "error: test failed\n\nCaused by:\n  first\n\n  last\n"
    );
}

#[test]
fn an_error_with_its_cause() {
    assert_eq!(
        caused("could not execute process `target/debug/foo` (never executed)", &["No such file or directory (os error 2)".to_owned()], false),
        "error: could not execute process `target/debug/foo` (never executed)\n\nCaused by:\n  No such file or directory (os error 2)\n"
    );
}
