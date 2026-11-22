//! A `#![no_std]` dependency's `#[macro_use] extern crate std;` gives its bodies std's `vec!`, a
//! re-export at std's root (`pub use alloc_crate::vec;`, std lib.rs), on the run path where its
//! body is lowered by need (blaze-scope names.rs `lexical`, the `#[macro_use]` layer).
//!
//! tempo, sweep of 2026-11-21: every proptest test was NOT RUN at proptest's
//! `FileFailurePersistence::load_persisted_failures2` (`|| Ok(vec![])`): "a path in module
//! 18b14d..: segment 0: not found". The Reference, "The `macro_use` attribute": on an `extern
//! crate`, it imports the crate's exported macros into the `macro_use` prelude; rustc imports
//! every macro of the crate's root module, a `pub use` re-export included, which is how std's
//! `vec!` reaches a `#![no_std]` crate at all.
//!
//! The workspace: `dep` (`#![no_std]`, `#[macro_use] extern crate std;`, `pub fn empty() ->
//! usize` building `vec![]` in a closure, proptest's shape; `pub fn three()` from `vec![1, 2,
//! 3]`), `app` on `dep`.
//!
//! 1. `check`, then `test`: `app`'s test asserts both; it runs and passes.
//! 2. TWIN: `app`'s own `vec!` (std's prelude, the layer that always worked) in the same test.
//! 3. CONTROL: `dep` without `#[macro_use]` on its `extern crate std;`: `vec!` is in no layer, so
//!    the test that calls `dep`'s fns is NOT RUN naming `vec` (a dependency's bodies are not
//!    checked, so the run is the first to lower them); never a pass.
//!
//! macOS only: Linux has no test sandbox. Needs the installed toolchain under
//! `$HOME/.blaze/v2/toolchain` (linked into a fresh home).
//!
//! Run: `cargo test -p blaze-cli --test run_dependency_macro_use`.

mod support;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// One command's bound: a check of a two-package workspace and a few first calls built.
const DEADLINE: Duration = Duration::from_secs(300);

const WORKSPACE: &str = "[workspace]\nmembers = [\"app\", \"dep\"]\nresolver = \"2\"\n";
const APP: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n";
const DEP: &str = "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
const DEP_LIB: &str = "#![no_std]\n\n#[macro_use]\nextern crate std;\n\nuse std::vec::Vec;\n\npub fn empty() -> usize {\n    let none: Option<&u8> = None;\n    let v: Vec<u8> = none.map_or_else(|| vec![], |x| vec![*x]);\n    v.len()\n}\n\npub fn three() -> usize {\n    vec![1u8, 2, 3].len()\n}\n";
/// [`DEP_LIB`] with `extern crate std;` not `#[macro_use]`: no layer has `vec`.
const DEP_LIB_NO_MACRO_USE: &str = "#![no_std]\n\nextern crate std;\n\nuse std::vec::Vec;\n\npub fn empty() -> usize {\n    let none: Option<&u8> = None;\n    let v: Vec<u8> = none.map_or_else(|| vec![], |x| vec![*x]);\n    v.len()\n}\n\npub fn three() -> usize {\n    vec![1u8, 2, 3].len()\n}\n";
const APP_LIB: &str = "#[test]\nfn the_dependencys_vec_runs() {\n    assert!(dep::empty() == 0);\n    assert!(dep::three() == 3);\n    let own: Vec<u8> = vec![7, 8];\n    assert!(own.len() == 2);\n}\n";

struct Said {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

impl Said {
    fn evidence(&self, why: &str) -> String {
        format!("--- status {:?}; {why}\n--- stderr\n{}\n--- stdout\n{}", self.code, self.stderr, self.stdout)
    }
}

/// A scratch workspace of `files` and its home; `check` is given both, then they are removed.
fn fixture<T>(name: &str, files: &[(&str, &str)], check: impl FnOnce(&Path, &Path) -> T) -> T {
    let base = fresh_home(name, true);
    let base = base.canonicalize().expect("the scratch folder's canonical path");
    let home = base.join("home");
    let dir = base.join("project");
    for &(name, source) in files {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().expect("a file in the fixture")).unwrap_or_else(|error| panic!("making the folder for {name}: {error}"));
        std::fs::write(&path, source).unwrap_or_else(|error| panic!("writing {name}: {error}"));
    }
    let result = check(&dir, &home);
    std::fs::remove_dir_all(&base).expect("remove the scratch command fixture");
    result
}

/// `blaze <command> <args>` in `dir` with `home`, under [`DEADLINE`].
fn blaze(dir: &Path, home: &Path, command: &str, args: &[&str]) -> Said {
    let stdout = home.join("command.stdout");
    let stderr = home.join("command.stderr");
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .arg(command)
        .args(args)
        .current_dir(dir)
        .env("PWD", dir)
        .env("HOME", home)
        .env("CARGO_HOME", home.join(".cargo"))
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout).expect("capture blaze's standard output"))
        .stderr(std::fs::File::create(&stderr).expect("capture blaze's standard error"))
        .spawn()
        .expect("blaze starts");
    let started = Instant::now();
    // Waiting for: `blaze <command>` in the fixture, at most DEADLINE.
    let status = loop {
        if let Some(status) = child.try_wait().expect("a wait on blaze") {
            break status;
        }
        if started.elapsed() > DEADLINE {
            child.kill().expect("stop the timed-out command");
            let _ = child.wait();
            panic!("`blaze {command}` past {DEADLINE:?} in {}: the command hung (rule 18)", dir.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Said {
        stdout: std::fs::read_to_string(stdout).expect("read blaze's standard output"),
        stderr: String::from_utf8_lossy(&std::fs::read(stderr).expect("read blaze's standard error")).into_owned(),
        code: status.code(),
    }
}

/// 1 and 2: `check`, then `test`: the dependency's `vec!` and the root's own both run.
#[test]
#[cfg(target_os = "macos")]
fn a_no_std_dependencys_macro_use_vec_runs_after_a_check() {
    let files = [("Cargo.toml", WORKSPACE), ("app/Cargo.toml", APP), ("app/src/lib.rs", APP_LIB), ("dep/Cargo.toml", DEP), ("dep/src/lib.rs", DEP_LIB)];
    fixture("run_dependency_macro_use", &files, |dir, home| {
        let checked = blaze(dir, home, "check", &[]);
        assert!(checked.stdout.contains("0 errors") || checked.stderr.contains("0 errors"), "{}", checked.evidence("the check finds no error"));
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert!(!said.stdout.contains("segment 0: not found"), "{}", said.evidence("`vec` is found in the dependency's bodies"));
        assert!(!said.stdout.contains("NOT RUN") && !said.stdout.contains("FAILED"), "{}", said.evidence("the test runs"));
        assert!(said.stdout.contains("ran 1: 1 passed, 0 failed"), "{}", said.evidence("the test passes"));
        assert_eq!(said.code, Some(0), "{}", said.evidence("a passing suite exits 0"));
    });
}

/// 3. CONTROL: no `#[macro_use]`, no `vec!`: never a pass.
#[test]
#[cfg(target_os = "macos")]
fn control_without_macro_use_the_dependencys_vec_is_refused_by_name() {
    let files = [("Cargo.toml", WORKSPACE), ("app/Cargo.toml", APP), ("app/src/lib.rs", APP_LIB), ("dep/Cargo.toml", DEP), ("dep/src/lib.rs", DEP_LIB_NO_MACRO_USE)];
    fixture("run_dependency_macro_use_control", &files, |dir, home| {
        let _ = blaze(dir, home, "check", &[]);
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert!(said.stdout.contains("NOT RUN") && said.stdout.contains("vec"), "{}", said.evidence("the test is NOT RUN naming `vec`"));
        assert_ne!(said.code, Some(0), "{}", said.evidence("never a pass"));
    });
}
