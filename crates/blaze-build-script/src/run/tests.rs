//! Runs of small shell scripts standing in for compiled build scripts, judged as cargo's
//! `build_work` judges them (`tests/testsuite/build_script.rs` of rust-lang/cargo at 5f94df478:
//! `custom_build_script_failed`, `build_script_with_error`, `output_shows_on_vv`).

#![cfg(unix)]

use std::{ffi::OsString, os::unix::fs::PermissionsExt, path::PathBuf};

use super::*;
use crate::unit::{Package, Profile};

/// A package in a fresh directory whose script is `body`, run with cargo's environment.
fn package(test: &str, body: &str) -> (Unit, Env) {
    let dir =
        std::env::temp_dir().join(format!("blaze-build-script-{test}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let manifest_dir = dir.join("foo");
    fs::create_dir_all(&manifest_dir).unwrap();
    let script = dir.join("target/debug/build/foo-1/build-script-build");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let unit = Unit {
        package: Package {
            name: "foo".into(),
            version: "0.5.0".into(),
            manifest_path: manifest_dir.join("Cargo.toml"),
            manifest_dir,
            ..Package::default()
        },
        profile: Profile { name: "dev".into(), opt_level: "0".into(), ..Profile::default() },
        target: "x86_64-unknown-linux-gnu".into(),
        host: "x86_64-unknown-linux-gnu".into(),
        jobs: 1,
        script,
        out_dir: dir.join("target/debug/build/foo-2/out"),
        ..Unit::default()
    };
    let inherited = [(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))];
    let env = crate::env(&unit, &inherited).unwrap();
    (unit, env)
}

fn ran(unit: &Unit, env: Env) -> Ran {
    run(unit, Invocation::of(unit, env).command()).unwrap()
}

#[test]
fn a_run_where_cargo_runs_it_and_its_files() {
    let (unit, env) = package(
        "where",
        r#"read line && echo "cargo:warning=read $line"
echo "cargo:rustc-env=CWD=$(pwd)"
echo "cargo:rustc-cfg=foo"
echo "cargo:rustc-link-search=native=$OUT_DIR"
test -d "$OUT_DIR" && echo "cargo:rustc-env=MADE=1"
echo "said on stderr" >&2"#,
    );
    let ran = ran(&unit, env);
    assert!(ran.status.success());
    let output = ran.finish(&unit, &Context::of(&unit, &[])).unwrap();
    assert_eq!(
        output.env,
        [
            ("CWD".to_owned(), unit.package.manifest_dir.display().to_string()),
            ("MADE".to_owned(), "1".to_owned()),
        ]
    );
    assert_eq!(output.cfgs, ["foo"]);
    assert_eq!(
        output.library_paths,
        [crate::output::LibraryPath::CargoArtifact(PathBuf::from(format!(
            "native={}",
            unit.out_dir.display()
        )))]
    );
    assert!(output.log_messages.is_empty(), "stdin is closed, so nothing was read");

    let dir = run_dir(&unit);
    assert_eq!(printed(&unit), dir.join("output"));
    assert_eq!(fs::read(dir.join("output")).unwrap(), ran.stdout);
    assert_eq!(fs::read_to_string(dir.join("stderr")).unwrap(), "said on stderr\n");
    assert_eq!(fs::read(dir.join("root-output")).unwrap(), path_bytes(&unit.out_dir).unwrap());
    assert_eq!(
        fs::read_to_string(dir.join("invoked.timestamp")).unwrap(),
        "This file has an mtime of when this was started."
    );
    let modified = fs::metadata(dir.join("output")).unwrap().modified().unwrap();
    assert_eq!(modified, ran.began, "output is dated when the run began");
}

#[test]
fn a_script_that_fails() {
    let (unit, env) = package(
        "fails",
        "echo cargo:warning=careful\necho cargo::error=oh no\necho out\necho err >&2\nexit 101",
    );
    let ran = ran(&unit, env);
    let failed = ran.finish(&unit, &Context::of(&unit, &[])).unwrap_err();
    assert_eq!(
        failed.error.0,
        format!(
            "failed to run custom build command for `foo v0.5.0`\n\nCaused by:\n  process didn't \
             exit successfully: `{}` (exit status: 101)\n  --- stdout\n  cargo:warning=careful\n  \
             cargo::error=oh no\n  out\n  \n  --- stderr\n  err",
            unit.script.display()
        )
    );
    assert_eq!(
        failed.log_messages,
        [(Severity::Warning, "careful".to_owned()), (Severity::Error, "oh no".to_owned())]
    );
    assert!(!run_dir(&unit).join("output").exists(), "a failed run leaves no output");
}

#[test]
fn a_script_that_logs_an_error() {
    let (unit, env) = package("logs", "echo cargo::warning=foo\necho cargo::error=bar");
    let failed = ran(&unit, env).finish(&unit, &Context::of(&unit, &[])).unwrap_err();
    assert_eq!(failed.error.0, "build script logged errors");
    assert_eq!(
        failed.log_messages,
        [(Severity::Warning, "foo".to_owned()), (Severity::Error, "bar".to_owned())]
    );

    // cargo watches for errors on lines as printed, and reads the output with each line trimmed:
    // an error indented is shown but fails nothing.
    let (unit, env) = package("indented", "echo '  cargo::error=bar'");
    let output = ran(&unit, env).finish(&unit, &Context::of(&unit, &[])).unwrap();
    assert_eq!(output.log_messages, [(Severity::Error, "bar".to_owned())]);
}

#[test]
fn output_cargo_refuses() {
    let (unit, env) = package("refused", "echo cargo::rustc-what=1");
    let failed = ran(&unit, env).finish(&unit, &Context::of(&unit, &[])).unwrap_err();
    assert!(failed.error.0.starts_with("invalid output in build script of `foo v0.5.0`"));
    assert!(run_dir(&unit).join("output").exists(), "cargo keeps what it could not read");
}

#[test]
fn a_run_in_cargos_second_layout() {
    let (mut unit, env) = package("second", "echo cargo:rustc-cfg=foo\necho said on stderr >&2");
    let target = unit.script.ancestors().nth(4).unwrap().to_owned();
    unit.out_dir = target.join("debug/build/foo/0123456789abcdef/out");
    let ran = ran(&unit, env);
    ran.finish(&unit, &Context::of(&unit, &[])).unwrap();
    let dir = target.join("debug/build/foo/0123456789abcdef/run");
    assert_eq!((run_dir(&unit), printed(&unit)), (dir.clone(), dir.join("stdout")));
    assert_eq!(fs::read(dir.join("stdout")).unwrap(), ran.stdout);
    assert_eq!(fs::read_to_string(dir.join("stderr")).unwrap(), "said on stderr\n");
    assert_eq!(fs::read(dir.join("root-output")).unwrap(), path_bytes(&unit.out_dir).unwrap());
    assert!(dir.join("invoked.timestamp").exists() && !dir.join("output").exists());
    assert!(!is_second_layout(&target.join("debug/build/foo-1/out")));
}
