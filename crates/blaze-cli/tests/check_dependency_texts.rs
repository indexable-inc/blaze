//! A body job reads its dependencies' texts (hello world, check-r11-1.out: "the records did not
//! answer while its head was made; the items record of crate `bstd` (crate 77): Gone { krate:
//! CrateItemKey(fd3d00fee9e3), file: Some(9), call: None }"). The body job opened no pack
//! (body_types.rs `prepare` handed `&[]`), so no file of a tree past the checkout was served:
//! every head of bstd or core outside its crate's root file was `Gone`. Now the crate list's tree
//! table (blaze-world crate_list.rs, version 2) names each tree's pack and `prepare` opens them
//! once a command (body_types.rs `Kept::trees`).
//!
//! The workspace: `app`, whose `main` calls `util::twice`, whose head is in `util/src/m.rs`
//! (file 1 of util's items record, not its root), and `out.line`, whose head is in a non-root
//! file of bstd (a pack of the toolchain's shelf: tree 1 and up).
//!
//! 1. Cold: `check` passes with no `Gone`, no "is not kept" and no not-checked line.
//! 2. An unchanged repeat says the same and passes (every task found, none run: the repeat's
//!    certificate; the count is the in-process tests' `runs == 0`, main.rs, not seen here).
//! 3. `twice`'s head edited to return `u64`: `main`'s body, which read it, is checked again and
//!    its `let n: u32 = ..` is a type error naming `main`; the edit undone, `check` passes again.
//!    That `util`'s other bodies are not run again is the in-process tests' count, not seen here.
//!
//! A clean check exits 3, never 0, while the borrows stage is NOT checked (`BORROWS_ARE_ERRORS`
//! false, work.rs `CHECK_STAGES`): the old expectation of 0 was the old checker's too, and met
//! by neither; with the switch on, 0. Since `blaze check` is the engine (check_command.rs).
//!
//! Expected line: `DEPENDENCY TEXTS: cold 3, repeat 3, edited <not 0>, undone 3` (0 for 3 once
//! `BORROWS_ARE_ERRORS` is true).
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain` (linked into a fresh home).

mod support;
#[path = "support/switch.rs"]
mod switch;

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// How long `blaze check` of the two-package workspace may take; a hang is a bug (rule 18).
const DEADLINE: Duration = Duration::from_secs(300);

/// How long the child killed at [`DEADLINE`] may take to be reaped.
const REAP: Duration = Duration::from_secs(10);

const MAIN: &str = "fn main(cx: &Cx, out: Out) -> Result<(), Error> {\n    let n: u32 = util::twice(21);\n    if n == 42 {\n        out.line(cx, \"x\")?;\n    }\n    Ok(())\n}\n";
const LIB: &str = "mod m;\npub use m::twice;\n";
const TWICE: &str = "pub fn twice(x: u32) -> u32 {\n    x * 2\n}\n";
const TWICE_U64: &str = "pub fn twice(x: u32) -> u64 {\n    u64::from(x) * 2\n}\n";

/// The words of the failure this test guards, and of any body not checked.
const GONE: &str = "Gone {";
const NOT_KEPT: &str = "is not kept";
const NOT_CHECKED: &str = "not checked because";

fn write(path: &Path, text: &str) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap_or_else(|error| panic!("making {}: {error}", dir.display()));
    }
    std::fs::write(path, text).unwrap_or_else(|error| panic!("writing {}: {error}", path.display()));
}

/// The workspace under `base/ws`: `app` (a binary on `util`) and `util` (a library whose
/// `twice` is in its second file).
fn workspace(base: &Path) -> std::path::PathBuf {
    let ws = base.join("ws");
    write(&ws.join("Cargo.toml"), "[workspace]\nmembers = [\"app\", \"util\"]\nresolver = \"2\"\n");
    write(
        &ws.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"util\"]\n\n[[package]]\nname = \"util\"\nversion = \"0.1.0\"\n",
    );
    write(
        &ws.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n\n[dependencies]\nutil = { path = \"../util\" }\n",
    );
    write(&ws.join("app/src/main.rs"), MAIN);
    write(&ws.join("util/Cargo.toml"), "[package]\nname = \"util\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n");
    write(&ws.join("util/src/lib.rs"), LIB);
    write(&ws.join("util/src/m.rs"), TWICE);
    ws
}

/// `blaze check` in `ws`: its status and everything it said, the output kept as `<base>/<what>.said`.
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
    // Waiting for: `blaze check` of the workspace (`what`), at most DEADLINE.
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

/// A clean run: status 0, nothing `Gone`, no text not kept, no body not checked.
fn assert_clean(what: &str, (status, said): &(Option<i32>, String)) {
    assert!(!said.contains(GONE), "{what}: a file text was `Gone`:\n{said}");
    assert!(!said.contains(NOT_KEPT), "{what}: a file text was not kept:\n{said}");
    assert!(!said.contains(NOT_CHECKED), "{what}: a body was not checked:\n{said}");
    // The engine's words for a body not checked (engine_bodies.rs `note`: "was not checked",
    // "not checked whole") and for a stage (print.rs `stage_notes`): only the borrows stage,
    // unbuilt while `BORROWS_ARE_ERRORS` is false, may be named.
    let unchecked: Vec<&str> = said.lines().filter(|line| (line.contains("not checked") || line.contains("NOT checked")) && !line.contains("NOT checked: borrows")).collect();
    assert!(unchecked.is_empty(), "{what}: a body or a stage was not checked: {unchecked:?}\n{said}");
    assert!(said.lines().any(|line| line == "0 errors: blaze check"), "{what}: the closing line:\n{said}");
    // A clean check exits 3, never 0, while the borrows stage is not built (work.rs
    // `CHECK_STAGES`, `BORROWS_ARE_ERRORS`): the old expectation of 0 was met by no checker.
    let clean = if switch::borrows_are_errors() { 0 } else { 3 };
    assert_eq!(*status, Some(clean), "{what}: status {status:?}:\n{said}");
}

/// Should print `DEPENDENCY TEXTS: cold 0, repeat 0, edited <not 0>, undone 0`; on failure,
/// the assertion and all `check` said.
#[test]
fn a_body_reads_its_dependencies_texts_from_their_packs() {
    let base = fresh_home("check_dependency_texts", true);
    let ws = workspace(&base);
    let cold = check(&base, &ws, "cold");
    let repeat = check(&base, &ws, "repeat");
    write(&ws.join("util/src/m.rs"), TWICE_U64);
    let edited = check(&base, &ws, "edited");
    write(&ws.join("util/src/m.rs"), TWICE);
    let undone = check(&base, &ws, "undone");
    let _ = std::fs::remove_dir_all(&base);

    assert_clean("cold", &cold);
    assert_clean("repeat", &repeat);
    assert_eq!(repeat.1, cold.1, "an unchanged repeat says what the cold check said");
    let (status, said) = &edited;
    assert!(!said.contains(GONE) && !said.contains(NOT_KEPT), "edited: a file text was not served:\n{said}");
    assert!(status.is_some() && *status != Some(0), "edited: `main` reads a `u64` into a `u32`, status {status:?}:\n{said}");
    assert!(said.contains("main"), "edited: the type error names `main`, the body that read `twice`'s head:\n{said}");
    assert_clean("undone", &undone);

    println!(
        "DEPENDENCY TEXTS: cold {}, repeat {}, edited {}, undone {}",
        cold.0.unwrap_or(-1),
        repeat.0.unwrap_or(-1),
        edited.0.unwrap_or(-1),
        undone.0.unwrap_or(-1)
    );
}
