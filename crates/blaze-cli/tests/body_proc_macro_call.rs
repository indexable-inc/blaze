//! A body-position call of a function-like proc macro goes through the ONE proc-macro path
//! (piece pm-call-expand, 2026-11-16; rules 3, 9, 19, 21, 30).
//!
//! The shape is pm-small's (port27/tempo-deps NOTES.md "### pm-small"): `mac` with
//! `[lib] proc-macro = true` and `#[proc_macro] pub fn same`, and `app` whose `main` body writes
//! `mac::same!(1 + 2)`. Rust's rule: the Reference, "Procedural Macros": "Function-like
//! procedural macros are procedural macros that are invoked using the macro invocation operator
//! (`!`)", and "Procedural macros must be defined in the root of a crate with the crate type of
//! `proc-macro`": the call names a proc macro, never a `macro_rules!` source.
//!
//! What was wrong (pm-6.out): the macro crate's items record held no runner key (the names
//! job's texts answered `SourceTexts::proc_macro_key` with `None`), so blaze-scope's
//! `definition_of` (resolve.rs, the `MacroBody::Proc` arm) gave
//! "its source makes no definition: NoDefinition { shape: FnLike, name: \"same\" }" and the body
//! was not type checked. Now the names job gives the crate its module-identity key
//! (crates/blaze-cli/src/proc_runner.rs) and the body's host runs the call through
//! blaze-proc-macro's hook, `ProcMacros::run`, keyed by the module's wasm digest, the export, the
//! edition and the input tokens, in the Wasmtime sandbox.
//!
//! This test's home has an EMPTY module store, so the runner has the crate registered without a
//! module: the body's call goes to the hook and is refused there (its finding names the path
//! `mac::same`), and the `MacroModule` task's failure names the crate and the bootstrap command
//! (rule 3/19); never `NoDefinition`.
//! The positive run (the module present, the body checked) is pm-small with the interim
//! builder's module, key 360b848c.. (leg 2's reproduction): expected "0 errors: blaze check" with
//! no `types-not-checked` note on app/src/main.rs.
//!
//! Expected (`--test body_proc_macro_call`): "test blaze-cli: exit 0; 1 passed, 0 failed", the
//! test printing "named: mac 0.1.0: no compiled proc-macro module in".
//! Control: with job_texts.rs's `proc_macro_key` answering `None` again (the trait's default), it
//! fails with "the body call was looked up as a source definition (NoDefinition)".

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
/// The words of the finding this piece removes (blaze-scope resolve.rs `ExpandNote::NoDefinition`).
const NO_DEFINITION: &str = "NoDefinition";
/// The `MacroModule` task's line for a crate whose module the store lacks (macros.rs).
const NAMED: &str = "mac 0.1.0: no compiled proc-macro module in";
/// The body's call, named by its path in the body's finding (blaze-lower by_name.rs).
const CALL: &str = "mac::same";

/// pm-small at `dir`.
fn workspace(dir: &Path) {
    let files: [(&str, &str); 6] = [
        ("Cargo.toml", "[workspace]\nmembers = [\"mac\", \"app\"]\nresolver = \"2\"\n"),
        ("Cargo.lock", "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"mac\",\n]\n\n[[package]]\nname = \"mac\"\nversion = \"0.1.0\"\n"),
        ("mac/Cargo.toml", "[package]\nname = \"mac\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n"),
        ("mac/src/lib.rs", "use proc_macro::TokenStream;\n\n#[proc_macro]\npub fn same(input: TokenStream) -> TokenStream {\n    input\n}\n"),
        ("app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nmac = { path = \"../mac\" }\n"),
        ("app/src/main.rs", "fn main() {\n    let x: u32 = mac::same!(1 + 2);\n    let _ = x;\n}\n"),
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
    // Waiting for: `blaze check` of pm-small, at most DEADLINE.
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
fn a_body_call_of_a_fn_like_proc_macro_goes_to_the_runner() {
    let home = fresh_home("body_proc_macro_call", true);
    let at = home.join("pm");
    workspace(&at);
    let said = check(&at, &home.join("home"));
    assert!(!said.contains(NO_DEFINITION), "the body call was looked up as a source definition (NoDefinition):\n{said}");
    assert!(said.contains(NAMED), "the crate with no module is named with its bootstrap command:\n{said}");
    assert!(said.contains(CALL), "the body's unexpanded call is named by its path:\n{said}");
    println!("named: {NAMED}");
}
