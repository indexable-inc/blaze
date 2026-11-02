//! Runs judged as cargo's `build_work` judges them (`tests/testsuite/build_script.rs` of
//! rust-lang/cargo at 5f94df478: `custom_build_script_failed`, `build_script_with_error`,
//! `output_shows_on_vv`), from the bytes a run printed and how it ended. The run itself is the
//! sandbox's.

#![cfg(unix)]

use std::{os::unix::process::ExitStatusExt, path::PathBuf};

use blaze_ids::rng::SplitMix64;

use super::*;
use crate::unit::{Package, Profile};

/// A package in a fresh directory, its script never made.
fn package(test: &str) -> Unit {
    let dir =
        std::env::temp_dir().join(format!("blaze-build-script-{test}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let manifest_dir = dir.join("foo");
    fs::create_dir_all(&manifest_dir).unwrap();
    Unit {
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
        script: dir.join("target/debug/build/foo-1/build-script-build"),
        out_dir: dir.join("target/debug/build/foo-2/out"),
        ..Unit::default()
    }
}

fn exited(code: i32) -> ExitStatus {
    ExitStatus::from_raw(code << 8)
}

fn judged(unit: &Unit, status: ExitStatus, stdout: &[u8], stderr: &[u8]) -> Result<Output, Failed> {
    judge(unit, &Context::of(unit, &[]), status, stdout, stderr)
}

/// What cargo does before a run, and the files it keeps of one, in its first layout.
#[test]
fn a_run_begins_and_its_files() {
    let unit = package("where");
    let began = begin(&unit).unwrap();
    assert!(unit.out_dir.is_dir());
    let dir = run_dir(&unit);
    let (timestamp, text) = invoked(&unit);
    assert_eq!(timestamp, dir.join("invoked.timestamp"));
    assert_eq!(fs::read(&timestamp).unwrap(), text);
    assert_eq!(fs::metadata(&timestamp).unwrap().modified().unwrap(), began);

    let files = files(&unit, b"cargo:rustc-cfg=foo\n", b"said on stderr\n").unwrap();
    assert_eq!(printed(&unit), dir.join("output"));
    assert_eq!(files.printed, (dir.join("output"), &b"cargo:rustc-cfg=foo\n"[..]));
    assert_eq!(files.stderr, (dir.join("stderr"), &b"said on stderr\n"[..]));
    assert_eq!(files.root_output, (dir.join("root-output"), path_bytes(&unit.out_dir).unwrap()));
    let output = judged(&unit, exited(0), files.printed.1, files.stderr.1).unwrap();
    assert_eq!(output.cfgs, ["foo"]);
}

/// An elided script is judged as a run that printed what elision says and succeeded.
#[test]
fn an_elided_run() {
    let unit = package("elided");
    let files = std::collections::BTreeMap::from([(
        PathBuf::from("build.rs"),
        b"fn main() { println!(\"cargo:rustc-cfg=foo\"); }".to_vec(),
    )]);
    let stdout = crate::elide::elide(std::path::Path::new("build.rs"), &files).unwrap();
    let output = judged(&unit, exited(0), &stdout, b"").unwrap();
    assert_eq!(output.cfgs, ["foo"]);

    let files = std::collections::BTreeMap::from([(
        PathBuf::from("build.rs"),
        b"fn main() { println!(\"cargo::error=no\"); }".to_vec(),
    )]);
    let stdout = crate::elide::elide(std::path::Path::new("build.rs"), &files).unwrap();
    let failed = judged(&unit, exited(0), &stdout, b"");
    assert_eq!(failed.unwrap_err().error.0, "build script logged errors", "judged as a run is");
}

#[test]
fn a_script_that_fails() {
    let unit = package("fails");
    let stdout = b"cargo:warning=careful\ncargo::error=oh no\nout\n";
    let failed = judged(&unit, exited(101), stdout, b"err\n").unwrap_err();
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
}

#[test]
fn a_script_that_logs_an_error() {
    let unit = package("logs");
    let failed = judged(&unit, exited(0), b"cargo::warning=foo\ncargo::error=bar\n", b"").unwrap_err();
    assert_eq!(failed.error.0, "build script logged errors");
    assert_eq!(
        failed.log_messages,
        [(Severity::Warning, "foo".to_owned()), (Severity::Error, "bar".to_owned())]
    );

    // cargo watches for errors on lines as printed, and reads the output with each line trimmed:
    // an error indented is shown but fails nothing.
    let output = judged(&unit, exited(0), b"  cargo::error=bar\n", b"").unwrap();
    assert_eq!(output.log_messages, [(Severity::Error, "bar".to_owned())]);
}

#[test]
fn output_cargo_refuses() {
    let unit = package("refused");
    let failed = judged(&unit, exited(0), b"cargo::rustc-what=1\n", b"").unwrap_err();
    assert!(failed.error.0.starts_with("invalid output in build script of `foo v0.5.0`"));
}

#[test]
fn a_run_in_cargos_second_layout() {
    let mut unit = package("second");
    let target = unit.script.ancestors().nth(4).unwrap().to_owned();
    unit.out_dir = target.join("debug/build/foo/0123456789abcdef/out");
    begin(&unit).unwrap();
    let dir = target.join("debug/build/foo/0123456789abcdef/run");
    assert_eq!((run_dir(&unit), printed(&unit)), (dir.clone(), dir.join("stdout")));
    assert!(dir.join("invoked.timestamp").exists());
    let files = files(&unit, b"cargo:rustc-cfg=foo\n", b"said on stderr\n").unwrap();
    assert_eq!(files.printed.0, dir.join("stdout"));
    assert_eq!(files.stderr.0, dir.join("stderr"));
    assert_eq!(files.root_output, (dir.join("root-output"), path_bytes(&unit.out_dir).unwrap()));
    assert!(!is_second_layout(&target.join("debug/build/foo-1/out")));
}

/// What is logged, read in the bytes, against `str::lines` of the output read lossily (what
/// it was before), over seeded outputs of the pieces that matter: the prefixes, line breaks
/// with and without a carriage return, a carriage return at the end, bytes not UTF-8.
#[test]
fn what_is_logged_is_read_as_the_lossy_lines_have_it() {
    let pieces: [&[u8]; 10] = [
        b"cargo::error=", b"cargo:warning=", b"cargo::warning=", b"cargo:rustc-cfg=x", b"\n",
        b"\r\n", b"\r", b"a b", b"\xff\xfe", "é".as_bytes(),
    ];
    let model = |stdout: &[u8]| -> Vec<(Severity, String)> {
        String::from_utf8_lossy(stdout)
            .lines()
            .filter_map(|line| {
                if let Some(error) = line.strip_prefix("cargo::error=") {
                    return Some((Severity::Error, error.to_owned()));
                }
                let warning = line
                    .strip_prefix("cargo:warning=")
                    .or_else(|| line.strip_prefix("cargo::warning="))?;
                Some((Severity::Warning, warning.to_owned()))
            })
            .collect()
    };
    let mut rng = SplitMix64::new(0x9fb2_1c65_1e98_df25);
    for _ in 0..20_000 {
        let mut stdout = Vec::new();
        let state = rng.next_u64();
        for at in 0..(state % 9) {
            stdout.extend_from_slice(pieces[(state.rotate_left(at as u32 * 7) % 10) as usize]);
        }
        assert_eq!(logged(&stdout), model(&stdout), "{stdout:?}");
    }
}

/// [`judge`] over runs that succeed, fail, log an error, print output cargo refuses, in both
/// layouts: a run that fails or logs an error is refused before what it printed is read, with
/// what it logged; any other is what [`output::parse`] makes of it.
#[test]
fn judged_from_the_bytes() {
    let mut unit = package("judged");
    let first = unit.out_dir.clone();
    let target = unit.script.ancestors().nth(4).unwrap().to_owned();
    let second = target.join("debug/build/foo/0123456789abcdef/out");
    let outputs: [&[u8]; 5] = [
        b"cargo:rustc-cfg=foo\ncargo:warning=w\r\n",
        b"noise\ncargo::error=bad\n",
        b"cargo::rustc-what=1\n",
        b"",
        b"cargo:warning=a\ncargo:error=not one\n  cargo::error=indented",
    ];
    for out_dir in [first, second] {
        unit.out_dir = out_dir;
        for status in [0, 101 << 8, 9] {
            for stdout in outputs {
                let status = ExitStatus::from_raw(status);
                let cx = Context::of(&unit, &[]);
                let judged = judge(&unit, &cx, status, stdout, b"err\n");
                let logs_error = stdout.starts_with(b"cargo::error") || stdout.windows(14).any(|it| it == b"\ncargo::error=");
                if !status.success() || logs_error {
                    let failed = judged.unwrap_err();
                    assert_eq!(failed.log_messages, logged(stdout), "{status} {stdout:?}");
                } else {
                    let parsed = output::parse(stdout, &cx).map_err(|error| Failed { error, log_messages: Vec::new() });
                    assert_eq!(judged, parsed, "{status} {stdout:?}");
                }
            }
        }
    }
}

/// Whether cargo refuses a run that succeeded is read from its bytes without allocating: the
/// lines it logs are found and borrowed, and messages are made only for a refusal.
#[test]
fn a_run_cargo_accepts_is_judged_without_allocating() {
    let stdout: &[u8] =
        b"cargo:rustc-cfg=x\ncargo:warning=one\r\nnoise\ncargo::warning=two\ncargo:error=no";
    let unit = Unit::default();
    let (logged, accepted) = blaze_alloc_count::assert_at_most(0, "the check for an error", || {
        let logged = log_lines(stdout).count();
        (logged, refused(&unit, ExitStatus::from_raw(0), stdout, b"err\n").is_ok())
    });
    assert_eq!((logged, accepted), (2, true));
}
