use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use blaze_units::stdlib::target;

use super::*;

fn args(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// The checkout's `library/`, which is the release's.
fn checkout() -> anyhow::Result<probe::Library> {
    probe::Library::at(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library"))
}

/// What rustc says to `asked` with `stdin`, checked against the checkout's library.
fn said(asked: &[&str], stdin: &str) -> Outcome {
    answer(&args(asked), stdin.as_bytes(), checkout).unwrap()
}

/// A directory of its own for a test's outputs, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("blaze-as-rustc-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn probes_read() {
    let anyhow = [
        "--cfg=anyhow_build_probe",
        "--edition=2018",
        "--crate-name=anyhow",
        "--crate-type=lib",
        "--cap-lints=allow",
        "--emit=dep-info,metadata",
        "--out-dir",
        "/o/probe",
        "src/nightly.rs",
        "--target",
        "x86_64-unknown-linux-gnu",
        "-Clinker-features=-lld",
    ];
    let probe = Probe::of(&anyhow).unwrap();
    assert_eq!(probe.input, Some("src/nightly.rs"));
    assert_eq!(probe.emit, [("dep-info", None), ("metadata", None)]);
    assert_eq!(probe.target, Some("x86_64-unknown-linux-gnu"));
    assert_eq!(probe.cfg_flags, ["--cfg", "anyhow_build_probe", "-C", "linker-features=-lld"]);
    let autocfg = "--crate-name autocfg_0 --crate-type=lib --out-dir /o --emit=llvm-ir -C extra-filename=-x -";
    let autocfg: Vec<&str> = autocfg.split(' ').collect();
    let probe = Probe::of(&autocfg).unwrap();
    assert_eq!((probe.input, probe.name().unwrap().as_str()), (Some("-"), "autocfg_0"));
    assert_eq!((probe.extra_filename, &probe.cfg_flags[..]), ("-x", &[][..]));
    let rustix = ["--crate-type=rlib", "--emit=metadata", "-o", "/o/f", "-"];
    assert_eq!(Probe::of(&rustix).unwrap().name().unwrap(), "rust_out");
    assert_eq!(Probe::of(&["--crate-type=lib", "--emit=metadata", "my-probe.rs"]).unwrap().name().unwrap(), "my_probe");
    // The lint flags are the check's, read from the whole command line.
    assert!(
        Probe::of(&["-Dwarnings", "-A", "dead_code", "--check-cfg", "cfg(x)", "--crate-type=lib", "--emit=metadata", "-"])
            .is_ok()
    );
    let not_a_library = |asked: &[&str]| Probe::of(asked).unwrap_err().downcast::<Refused>().unwrap();
    assert_eq!(
        not_a_library(&["--crate-type=bin", "--emit=metadata", "-"]),
        Refused::NotALibrary { crate_type: Some("bin".to_owned()) }
    );
    // rustc's crate is a binary unless told otherwise: blaze does not take it for a library.
    assert_eq!(not_a_library(&["--emit=metadata", "-"]), Refused::NotALibrary { crate_type: None });
    assert!(Probe::of(&["--emit=link", "-"]).is_err());
    assert!(Probe::of(&["--crate-type=lib", "-"]).is_err());
    assert!(Probe::of(&["--extern", "a=b", "--emit=metadata", "-"]).is_err());
    assert!(Probe::of(&["a.rs", "b.rs", "--emit=metadata"]).is_err());
    assert!(Probe::of(&["--emit=metadata,dep-info", "-o", "/o/f", "-"]).is_err());
    let none = Probe::of(&["--emit=metadata"]).unwrap_err();
    assert_eq!(none.to_string(), "no input filename given");
}

/// Questions are answered without the library, as blaze's release's rustc answers them.
#[test]
fn questions_answered() {
    let no_library = || -> anyhow::Result<probe::Library> { panic!("a question reads no library") };
    let version = answer(&args(&["--version"]), &[], no_library).unwrap();
    assert_eq!(version.code, 0);
    let version = String::from_utf8(version.stdout).unwrap();
    assert_eq!(version, format!("rustc {}\n", target::rustc_version()));
    // rustversion's build script: `$RUSTC --version`, read as `rustc 1.N.P-nightly (hash date)`.
    let rest = version.strip_prefix("rustc 1.").unwrap();
    let (minor, rest) = rest.split_once('.').unwrap();
    assert!(minor.parse::<u32>().is_ok(), "{version}");
    let (patch, rest) = rest.split_once('-').unwrap();
    assert!(patch.parse::<u32>().is_ok(), "{version}");
    assert!(rest.starts_with("nightly ("), "{version}");
    // autocfg's `probe_rustc_version`: `$RUSTC --version --verbose`, read for its `release:`.
    let verbose = answer(&args(&["--version", "--verbose"]), &[], no_library).unwrap();
    let verbose = String::from_utf8(verbose.stdout).unwrap();
    assert!(verbose.starts_with(&version), "{verbose}");
    let release = verbose.lines().find_map(|it| it.strip_prefix("release: ")).unwrap();
    assert_eq!(release, format!("1.{minor}.{patch}-nightly"));
}

#[test]
fn autocfg_probes() {
    let scratch = Scratch::new("autocfg");
    let asked = |name: &str| {
        let out = scratch.path();
        ["--crate-name", name, "--crate-type=lib", "--out-dir", out, "--emit=llvm-ir", "-"].map(str::to_owned)
    };
    let i128 = asked("autocfg_0123_1");
    let i128: Vec<&str> = i128.iter().map(String::as_str).collect();
    let outcome = said(&i128, "#![no_std]\npub type Probe = i128;\n");
    assert_eq!(outcome.code, 0, "{}", String::from_utf8_lossy(&outcome.stderr));
    assert!(scratch.0.join("autocfg_0123_1.ll").exists());
    let std = asked("autocfg_0123_2");
    let std: Vec<&str> = std.iter().map(String::as_str).collect();
    let outcome = said(&std, "extern crate std as probe;\npub type Probe = std::string::String;\n");
    assert_eq!(outcome.code, 0, "{}", String::from_utf8_lossy(&outcome.stderr));
    let i256 = asked("autocfg_0123_3");
    let i256: Vec<&str> = i256.iter().map(String::as_str).collect();
    let outcome = said(&i256, "#![no_std]\npub type Probe = i256;\n");
    assert_eq!(outcome.code, 1);
    let stderr = String::from_utf8(outcome.stderr).unwrap();
    assert!(stderr.contains("error[E0425]: cannot find type `i256` in this scope"), "{stderr}");
    assert!(!scratch.0.join("autocfg_0123_3.ll").exists());
}

#[test]
fn type_error_fails_as_rustc() {
    let scratch = Scratch::new("type-error");
    let asked = ["--crate-name", "probe", "--crate-type=lib", "--emit=metadata", "--out-dir", scratch.path(), "-"];
    let outcome = said(&asked, "pub fn f() -> u32 { \"x\" }\n");
    assert_eq!(outcome.code, 1);
    let stderr = String::from_utf8(outcome.stderr).unwrap();
    assert!(stderr.contains("error[E0308]: mismatched types"), "{stderr}");
    assert!(stderr.contains("--> <anon>:1:21"), "{stderr}");
    assert!(stderr.contains("error: aborting due to 1 previous error"), "{stderr}");
    assert!(!scratch.0.join("libprobe.rmeta").exists());
}

/// anyhow's `build/probe.rs`, which its build script checks to tell whether the nightly has
/// the error generic member access it backs backtraces with.
const ANYHOW_PROBE: &str = r#"
#![cfg_attr(anyhow_build_probe, feature(error_generic_member_access))]

use core::error::{self, Error};
use std::backtrace::Backtrace;
use std::fmt::{self, Debug, Display};

struct MyError(Thing);
struct Thing;

impl Debug for MyError {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("MyError")
    }
}

impl Display for MyError {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("MyError")
    }
}

impl Error for MyError {
    fn provide<'a>(&'a self, request: &mut error::Request<'a>) {
        request.provide_ref(&self.0);
    }
}

const _: fn(&dyn Error) -> Option<&Backtrace> = |err| error::request_ref::<Backtrace>(err);

const _: Option<&str> = option_env!("RUSTC_BOOTSTRAP");
"#;

#[test]
fn anyhow_backtrace_probe() {
    let scratch = Scratch::new("anyhow");
    let file = scratch.0.join("probe.rs");
    std::fs::write(&file, ANYHOW_PROBE).unwrap();
    let out = scratch.0.join("out");
    std::fs::create_dir(&out).unwrap();
    let file = file.to_str().unwrap();
    let asked = [
        "--cfg=anyhow_build_probe",
        "--edition=2018",
        "--crate-name=anyhow",
        "--crate-type=lib",
        "--cap-lints=allow",
        "--emit=dep-info,metadata",
        "--out-dir",
        out.to_str().unwrap(),
        file,
    ];
    let outcome = said(&asked, "");
    assert_eq!(outcome.code, 0, "{}", String::from_utf8_lossy(&outcome.stderr));
    assert!(out.join("libanyhow.rmeta").exists());
    let dep_info = std::fs::read_to_string(out.join("anyhow.d")).unwrap();
    assert_eq!(dep_info, format!("{}: {file}\n\n{file}:\n", out.join("anyhow.d").display()));
    // Without the cfg the feature is not turned on, and the API is unstable.
    let unstable = said(&asked[1..], "");
    assert_eq!(unstable.code, 1);
    let stderr = String::from_utf8(unstable.stderr).unwrap();
    assert!(stderr.contains("error[E0658]"), "{stderr}");
}

#[test]
fn rustix_writes_to_standard_output() {
    let asked = ["--crate-type=rlib", "--emit=metadata", "-o", "-", "-"];
    let outcome = said(&asked, "#![no_std]\npub fn f() {}\n");
    assert_eq!((outcome.code, &outcome.stdout[..]), (0, &b""[..]));
}

#[test]
fn unknown_feature_fails() {
    let asked = ["--crate-type=lib", "--emit=metadata", "-o", "-", "-"];
    let outcome = said(&asked, "#![feature(no_such_feature_at_all)]\n");
    assert_eq!(outcome.code, 1);
    let stderr = String::from_utf8(outcome.stderr).unwrap();
    assert!(stderr.contains("error[E0635]: unknown feature `no_such_feature_at_all`"), "{stderr}");
}

#[test]
fn unanswerable_is_an_error() {
    let refused = |asked: &[&str]| answer(&args(asked), b"", checkout).unwrap_err().to_string();
    assert_eq!(refused(&["--emit=metadata"]), "no input filename given");
    assert!(refused(&["--emit=link", "-"]).contains("--emit=link"));
    assert!(refused(&["-Z", "no-such-option", "--crate-type=lib", "--emit=metadata", "-"]).contains("no-such-option"));
}
