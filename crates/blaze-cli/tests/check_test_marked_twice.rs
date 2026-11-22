//! A function marked as a test twice is ONE test, and a function the tests record refuses is
//! that function's error, never its crate's failure (tempo, 2026-11-17 g105938: crates
//! `inline` and `deferred` of the e2e suite failed "its tests record could not be written:
//! .. is marked `test` or `bench` more than once": `#[tokio::test]` writes
//! `#[::core::prelude::v1::test]` and commonware's `#[test_traced]` writes `#[test]` on the
//! same function, crates/e2e/tests/suite/follow.rs:36-38).
//!
//! The Reference, "Testing attributes": "The `test` attribute marks a function to be executed
//! as a test"; rustc expands each marker and warns `duplicate_macro_attributes` ("duplicated
//! attribute") only where a later attribute is the bare name (rustc_builtin_macros
//! `warn_on_duplicate_attribute`).
//!
//! The package `lib` (a library, so `check` solves its test unit too): 1. `prelude_after_bare`
//! (`#[test] #[::core::prelude::v1::test]`, tempo's order) and `bare_after_prelude` (the other
//! order) and `once`: `check` passes, no line says the tests record could not be written, and
//! exactly one warning, for `bare_after_prelude`. 2. CONTROL: `both` added, marked `#[test]`
//! and `#[bench]`: one error naming `both`, and still no crate failure.
//!
//! Expected line: `TEST MARKED TWICE: twice <status> (<n> warning), both <not 0>`.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain` (linked into a fresh home).

mod support;

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// How long `blaze check` of the package may take; a hang is a bug (rule 18).
const DEADLINE: Duration = Duration::from_secs(300);

/// How long the child killed at [`DEADLINE`] may take to be reaped.
const REAP: Duration = Duration::from_secs(10);

const TWICE: &str = "pub fn plain() -> u32 {
    1
}

#[test]
#[::core::prelude::v1::test]
fn prelude_after_bare() {}

#[::core::prelude::v1::test]
#[test]
fn bare_after_prelude() {}

#[test]
fn once() {}
";

const BOTH: &str = "
#[test]
#[bench]
fn both() {}
";

/// The crate failed: the bug's words.
const UNWRITTEN: &str = "tests record could not be written";
const WARNED: &str = "is marked as a test more than once";
const REFUSED: &str = "is not a test";

fn write(path: &Path, text: &str) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap_or_else(|error| panic!("making {}: {error}", dir.display()));
    }
    std::fs::write(path, text).unwrap_or_else(|error| panic!("writing {}: {error}", path.display()));
}

/// The package under `base/ws`, its `src/lib.rs` being `lib`.
fn package(base: &Path, lib: &str) -> std::path::PathBuf {
    let ws = base.join("ws");
    write(&ws.join("Cargo.toml"), "[package]\nname = \"lib\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n");
    write(&ws.join("Cargo.lock"), "version = 4\n\n[[package]]\nname = \"lib\"\nversion = \"0.1.0\"\n");
    write(&ws.join("src/lib.rs"), lib);
    ws
}

/// `blaze check` in `ws`: its status and everything it said, kept as `<base>/<what>.said`.
fn check(base: &Path, ws: &Path, what: &str) -> (Option<i32>, String) {
    let home = base.join("home");
    let said = base.join(format!("{what}.said"));
    let file = File::create(&said).unwrap_or_else(|error| panic!("making {}: {error}", said.display()));
    let file_err = file.try_clone().expect("the output file, twice");
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .arg("check")
        .current_dir(ws)
        .env("HOME", &home)
        .env("CARGO_HOME", home.join(".cargo"))
        .env("PWD", ws)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(file_err))
        .spawn()
        .expect("blaze starts");
    let started = Instant::now();
    // Waiting for: `blaze check` of the package (`what`), at most DEADLINE.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > DEADLINE => {
                let _ = child.kill();
                let reaped = Instant::now();
                // Waiting for: the killed child's exit, at most REAP.
                while child.try_wait().ok().flatten().is_none() && reaped.elapsed() < REAP {
                    std::thread::sleep(Duration::from_millis(5));
                }
                panic!("`blaze check` ({what}) in {} did not end within {DEADLINE:?} (rule 18)", ws.display());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("waiting for `blaze check` ({what}) in {}: {error}", ws.display());
            }
        }
    };
    let text = std::fs::read_to_string(&said).unwrap_or_else(|error| panic!("reading {}: {error}", said.display()));
    (status.code(), text)
}

fn count(said: &str, words: &str) -> usize {
    said.lines().filter(|line| line.contains(words)).count()
}

/// Should print `TEST MARKED TWICE: twice <status> (<n> warning), both <not 0>`; on failure,
/// the assertion and all `check` said.
#[test]
fn a_function_marked_twice_is_one_test_and_a_refused_one_fails_only_itself() {
    let base = fresh_home("check_test_marked_twice", true);
    let ws = package(&base, TWICE);
    let twice = check(&base, &ws, "twice");
    write(&ws.join("src/lib.rs"), &format!("{TWICE}{BOTH}"));
    let both = check(&base, &ws, "both");
    let _ = std::fs::remove_dir_all(&base);

    let (status, said) = &twice;
    assert!(!said.contains(UNWRITTEN), "twice: the crate's tests record failed, status {status:?}:\n{said}");
    assert!(said.contains("0 errors, 1 warning: blaze check"), "twice: status {status:?}:\n{said}");
    assert_eq!(count(said, WARNED), 1, "twice: one warning, at `bare_after_prelude`:\n{said}");
    assert!(said.lines().any(|line| line.contains(WARNED) && line.contains("bare_after_prelude")), "twice: the warning names `bare_after_prelude`:\n{said}");
    assert_eq!(count(said, REFUSED), 0, "twice: nothing refused:\n{said}");

    let (status, said) = &both;
    assert!(status.is_some() && *status != Some(0), "both: `#[test] #[bench]` is an error, status {status:?}:\n{said}");
    assert!(!said.contains(UNWRITTEN), "both: one function failed its crate:\n{said}");
    assert_eq!(count(said, REFUSED), 1, "both: one error:\n{said}");
    assert!(said.lines().any(|line| line.contains(REFUSED) && line.contains("`both`")), "both: the error names `both`:\n{said}");

    println!("TEST MARKED TWICE: twice {} ({} warning), both {}", twice.0.unwrap_or(-1), count(&twice.1, WARNED), both.0.unwrap_or(-1));
}
