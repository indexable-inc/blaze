//! An attribute macro call of the project's own code that the solve REFUSED (here: its proc
//! macro crate has no compiled module in the store) is printed by `blaze check` at the call,
//! with the refusal's own words (rule 19). Before (2026-11-17), only a call whose expansion ran
//! and failed was printed (names/failed_call.rs read only `ExpandNote::Failed` notes): a refused
//! call said nothing, and what it would have written was only "no `XApiServer` in .." further
//! on (tempo: 28 such lines behind jsonrpsee's `#[rpc]`, no line naming the call).
//!
//! The shape: pm-small (body_proc_macro_call.rs) with `#[proc_macro_attribute] pub fn rpc` and
//! `app` writing `#[mac::rpc] pub trait XApi {}`; the home's module store is EMPTY, so the call
//! is refused before any macro runs.
//!
//! CONTROL: the same workspace without the attribute prints no macro-call-failed line.
//!
//! Expected (`--test check_refused_attribute_call`): 1 passed, printing
//! "REFUSED ATTRIBUTE CALL: <the line>".
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain` (linked into a fresh home).

mod support;

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// How long one `blaze check` of the two small members may take; a hang is a bug (rule 18).
const DEADLINE: Duration = Duration::from_secs(300);
/// How long the child killed at [`DEADLINE`] may take to be reaped (the kill is SIGKILL).
const REAP: Duration = Duration::from_secs(10);
/// The finding's rule name (blaze-report words, Cli 22).
const RULE: &str = "macro-call-failed";
/// The call's words in the finding (names/failed_call.rs).
const CALL: &str = "the attribute call of `mac::rpc` failed:";

/// pm-small with an attribute macro at `dir`; `app`'s lib is `lib`.
fn workspace(dir: &Path, lib: &str) {
    let files: [(&str, &str); 6] = [
        ("Cargo.toml", "[workspace]\nmembers = [\"mac\", \"app\"]\nresolver = \"2\"\n"),
        ("Cargo.lock", "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"mac\",\n]\n\n[[package]]\nname = \"mac\"\nversion = \"0.1.0\"\n"),
        ("mac/Cargo.toml", "[package]\nname = \"mac\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n"),
        ("mac/src/lib.rs", "use proc_macro::TokenStream;\n\n#[proc_macro_attribute]\npub fn rpc(_args: TokenStream, item: TokenStream) -> TokenStream {\n    item\n}\n"),
        ("app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nmac = { path = \"../mac\" }\n"),
        ("app/src/lib.rs", lib),
    ];
    for (path, text) in files {
        let at = dir.join(path);
        std::fs::create_dir_all(at.parent().expect("a file's folder")).unwrap_or_else(|error| panic!("making the folder of {}: {error}", at.display()));
        std::fs::write(&at, text).unwrap_or_else(|error| panic!("writing {}: {error}", at.display()));
    }
}

/// `blaze check` in `dir` with the home `home`: its standard output and error, together, through
/// a file; waited for in a loop bounded by [`DEADLINE`].
fn check(dir: &Path, home: &Path) -> String {
    let said = dir.with_extension("said");
    let file = File::create(&said).unwrap_or_else(|error| panic!("making {}: {error}", said.display()));
    let file_err = file.try_clone().expect("the output file, twice");
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .arg("check")
        .current_dir(dir)
        .env("HOME", home)
        .env("CARGO_HOME", home.join(".cargo"))
        .env("PWD", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(file_err))
        .spawn()
        .expect("blaze starts");
    let started = Instant::now();
    // Waiting for: `blaze check` of the two members, at most DEADLINE.
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
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
    }
    std::fs::read_to_string(&said).unwrap_or_else(|error| panic!("reading {}: {error}", said.display()))
}

#[test]
fn a_refused_attribute_call_is_printed_at_the_call() {
    let home = fresh_home("check_refused_attribute_call", true);
    let at = home.join("rpc");
    workspace(&at, "#[mac::rpc]\npub trait XApi {}\n");
    let said = check(&at, &home.join("home"));
    let lines: Vec<&str> = said.lines().filter(|line| line.contains(RULE) && line.contains(CALL)).collect();
    assert_eq!(lines.len(), 1, "one line naming the refused call and why:\n{said}");
    assert!(lines[0].contains("src/lib.rs:1:"), "the line is at the call:\n{said}");

    let control = home.join("plain");
    workspace(&control, "pub trait XApi {}\n");
    let plain = check(&control, &home.join("home"));
    assert!(!plain.contains(RULE), "control: a macro-call-failed line without a call:\n{plain}");
    println!("REFUSED ATTRIBUTE CALL: {}", lines[0]);
}
