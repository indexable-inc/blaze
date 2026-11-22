//! An item `blaze check` could not check FAILS the command, and does not read like an error in
//! the user's code (main's ruling via the front-end coordinator, 2026-11-16; rule 3; the owner's
//! "`blaze check` must really check"): its own exit status (main.rs `UNCHECKED_ITEMS`, 4), its
//! own summary line ("the check is incomplete: blaze could not check N items"), each item
//! named under its cause (blaze-cli not_checked/items.rs); an error found keeps status 1 and
//! wins when both are there; never "0 errors" or "no errors in" beside items not checked.
//!
//! The item not checked: `#[cfg(blaze_unknown(form))] fn g() {}`, whose `cfg` predicate is no
//! form the Reference's "Conditional compilation" grammar has (a `ConfigurationPredicate` is an
//! option, `all`, `any` or `not`), so whether the crate holds `g` is not known (body_types.rs
//! `CFG_UNREAD`). (Until 2026-11-17 it was `const C: u8 = 1;`, whose initializer front2 did not
//! check then; it is checked now.) The user's error: the Reference,
//! "Literal expressions": a string literal is `&'static str`, so `let m: u8 = "x";` is E0308.
//!
//! Three packages: only the user's error (status 1, no incomplete line); only the item not
//! checked (status 4, the cause line and the summary, no error); both (status 1, both lines).
//!
//! CONTROL (rule 8, run once by the integrator with main.rs `close`'s old body): the
//! only-unchecked package exits 3 (INCOMPLETE, the same status a stage not built gives) and
//! names no item; this test then fails with "only unchecked: status Some(3)".
//! (The coordinator's brief expected the old form to exit 0; the tree's old `close` already
//! turns a `TYPES_NOT_CHECKED` note into "NOT checked: types" and status 3: it did not pass,
//! but named nothing and shared its status with "a stage is not built yet".)
//!
//! Expected line: `UNCHECKED ITEMS: errors-only 1, unchecked-only 4, both 1`.
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

const ERROR: &str = "fn main(cx: &Cx, out: Out) -> Result<(), Error> {\n    let m: u8 = \"x\";\n    out.line(cx, \"x\")?;\n    Ok(())\n}\n";
const CLEAN: &str = "fn main(cx: &Cx, out: Out) -> Result<(), Error> {\n    out.line(cx, \"x\")?;\n    Ok(())\n}\n";
const UNCHECKED: &str = "#[cfg(blaze_unknown(form))]\nfn g() {}\n";

const SUMMARY: &str = "the check is incomplete: blaze could not check";
const CAUSE: &str = "not checked because a `cfg` predicate on it or on an item it is written in does not read (blaze-scope `Cfg::eval`), so whether its crate holds it cannot be told; not checked: `g` (";
/// The type error's code as the agent view prints it. D70: "`agent` is one or two plain lines
/// per finding with nothing decorative: the id, the code, `file:line:col`, the message", the
/// view a file is given; the message ends "(E0308 Mismatch)". The person view's "type error
/// E0308" frame label is drawn on a terminal alone.
const E0308: &str = "(E0308 Mismatch)";

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

#[test]
fn an_item_not_checked_fails_the_check_with_its_own_status_and_line() {
    let base = fresh_home("check_unchecked_items", true);
    let errors_only = check(&base, "errors-only", ERROR);
    let unchecked_only = check(&base, "unchecked-only", &format!("{CLEAN}\n{UNCHECKED}"));
    let both = check(&base, "both", &format!("{ERROR}\n{UNCHECKED}"));
    let _ = std::fs::remove_dir_all(&base);

    let (status, said) = &errors_only;
    assert_eq!(*status, Some(1), "errors only: status {status:?}:\n{said}");
    assert!(said.contains(E0308), "errors only: the error is printed:\n{said}");
    assert!(!said.contains(SUMMARY), "errors only: no item was left unchecked:\n{said}");

    let (status, said) = &unchecked_only;
    assert_eq!(*status, Some(4), "only unchecked: status {status:?}:\n{said}");
    assert!(said.contains(SUMMARY), "only unchecked: its summary line:\n{said}");
    assert!(said.contains(CAUSE), "only unchecked: the item under its cause:\n{said}");
    assert!(!said.contains("0 errors") && !said.contains("no errors in") && !said.contains(E0308), "only unchecked: nothing reads like the code's verdict:\n{said}");

    let (status, said) = &both;
    assert_eq!(*status, Some(1), "both: the user's errors' status wins: {status:?}:\n{said}");
    assert!(said.contains(E0308) && said.contains(SUMMARY) && said.contains(CAUSE), "both: both lines and the item:\n{said}");

    println!("UNCHECKED ITEMS: errors-only {}, unchecked-only {}, both {}", errors_only.0.unwrap_or(-1), unchecked_only.0.unwrap_or(-1), both.0.unwrap_or(-1));
}
