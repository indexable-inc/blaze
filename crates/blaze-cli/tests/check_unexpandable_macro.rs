//! A body refused at lowering is named by `blaze check` with its item and the lowering's own
//! words, never the printer's invariant (hello world, 2026-11-17 00:34, check-r9-1.err: "error:
//! a types-not-checked finding without its item and cause (blaze-cli not_checked/items.rs
//! `file`)", status 1, nothing else printed). The one-argument findings of body_types/heads.rs
//! (a read list refused, a head not named, the `Signature` task's digest) were the findings
//! without the form; each is filed by not_checked/items.rs `file` now.
//!
//! The body: `fn helper` holding `nowhere::missing!()`, a macro path that resolves to nothing,
//! which front2's lowering refuses (`Lowered { findings: [Macro { path, why }, ..] }`, the same
//! refusal `blaze run` printed for the hello world's `main`). The findings' stream is a file, so
//! the view is the agent's (D70): each finding is a line with its words, the macro path in them.
//!
//! CONTROL: a package whose `main` holds no such body prints no not-checked line. (If `main`
//! itself is still refused at lowering, as run-r9-1.err shows for `crate::launch_body`, the
//! control fails naming `main`: a true finding, not this test's.)
//!
//! Expected line: `UNEXPANDABLE MACRO: refused-body <status, not 0>, control <status>`.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain` (linked into a fresh home).

mod support;

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// How long `blaze check` of a one-file package may take; a hang is a bug (rule 18), named.
const DEADLINE: Duration = Duration::from_secs(300);

/// How long the child killed at [`DEADLINE`] may take to be reaped.
const REAP: Duration = Duration::from_secs(10);

const CLEAN: &str = "fn main(cx: &Cx, out: Out) -> Result<(), Error> {\n    out.line(cx, \"x\")?;\n    Ok(())\n}\n";
const REFUSED_BODY: &str = "fn helper() {\n    nowhere::missing!();\n}\n";

const INVARIANT: &str = "a types-not-checked finding without its item and cause";
const NOT_CHECKED: &str = "not checked because";
/// The line's cause names what the lowering refused (rule 19; MX 2026-11-17: "lowering
/// refused" alone named nothing on tempo's clap derives): the class, then the macro's call.
///
/// Since `blaze check` is the engine (check_command.rs): the engine names a refused body in its
/// own note, `` `<file>`: body `<item>`: <cause>: <words> `` (engine_bodies.rs
/// `BodiesStage::note`), not under the old checker's grouped "not checked because <cause>:
/// `<item>` (" line, so the cause is matched after the body's name. The old words were the old
/// printer's layout (not_checked/items.rs), not what the test guards: one line naming the body,
/// the lowering's class and the macro's call.
const HELPER: &str = "body `helper`: lowering refused (Lowered: Macro ";
const HELPER_CALL: &str = ", the call of `nowhere::missing!`): ";

/// `blaze check` of a package whose `src/main.rs` is `main`: its status and everything it said.
fn check(base: &Path, name: &str, main: &str) -> (Option<i32>, String) {
    let dir = base.join(name);
    std::fs::create_dir_all(dir.join("src")).expect("the package's folders");
    std::fs::write(dir.join("Cargo.toml"), format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n")).expect("the manifest");
    std::fs::write(dir.join("Cargo.lock"), format!("version = 4\n\n[[package]]\nname = \"{name}\"\nversion = \"0.1.0\"\n")).expect("the lock");
    std::fs::write(dir.join("src/main.rs"), main).expect("src/main.rs");
    let home = base.join("home");
    let said = dir.with_extension("said");
    let file = File::create(&said).unwrap_or_else(|error| panic!("making {}: {error}", said.display()));
    let file_err = file.try_clone().expect("the output file, twice");
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .arg("check")
        .current_dir(&dir)
        .env("HOME", &home)
        .env("CARGO_HOME", home.join(".cargo"))
        .env("PWD", &dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(file_err))
        .spawn()
        .expect("blaze starts");
    let started = Instant::now();
    // Waiting for: `blaze check` of the one-file package, at most DEADLINE.
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
                panic!("`blaze check` in {} did not end within {DEADLINE:?} (rule 18)", dir.display());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("waiting for `blaze check` in {}: {error}", dir.display());
            }
        }
    };
    let text = std::fs::read_to_string(&said).unwrap_or_else(|error| panic!("reading {}: {error}", said.display()));
    (status.code(), text)
}

/// Should print `UNEXPANDABLE MACRO: refused-body 4, control 0` (the refused body's status is
/// any but 0; 4 is main.rs `UNCHECKED_ITEMS`); on failure, the assertion and all `check` said.
#[test]
fn a_body_refused_at_lowering_is_named_with_its_macro_path() {
    let base = fresh_home("check_unexpandable_macro", true);
    let refused = check(&base, "refused-body", &format!("{CLEAN}\n{REFUSED_BODY}"));
    let control = check(&base, "control", CLEAN);
    let _ = std::fs::remove_dir_all(&base);

    let (status, said) = &refused;
    assert!(!said.contains(INVARIANT), "refused body: the printer's invariant was hit:\n{said}");
    assert!(status.is_some() && *status != Some(0), "refused body: status {status:?}:\n{said}");
    assert_eq!(said.lines().filter(|line| line.contains(HELPER) && line.contains(HELPER_CALL)).count(), 1, "refused body: one line naming `helper` under the lowering's refusal and the macro it could not take:\n{said}");
    assert!(said.contains("nowhere") && said.contains("missing"), "refused body: the macro path in the lowering's words:\n{said}");

    let (status, said) = &control;
    assert!(!said.contains(INVARIANT), "control: the printer's invariant was hit:\n{said}");
    assert!(!said.contains(NOT_CHECKED), "control: a not-checked line without a refused body: status {status:?}:\n{said}");

    println!("UNEXPANDABLE MACRO: refused-body {}, control {}", refused.0.unwrap_or(-1), control.0.unwrap_or(-1));
}
