//! `blaze engine-check` checks the bodies a serde derive's shape makes: a macro call inside an
//! unnamed constant's block whose path's first segment is an `extern crate .. as` the block
//! itself declares (`const _: () = { extern crate serde as _serde;
//! _serde::__require_serde_not_serde_core!(); impl _serde::Serialize for T { .. } };`). Before the
//! fix the call's path was read at the module, where `_serde` is nothing: "lowering refused
//! (.. segment 0: not found)" for every derive's constant (port27/mini serde-root, g124650).
//!
//! The Reference, "Statements", "Item declarations": an item a block declares is in scope in
//! that block. The fixture, no serde: a workspace of `dep` (a `#[macro_export] macro_rules!`, a
//! trait, and `implement!`, which writes such a block at module level as a derive does) and
//! `app` (one block by hand, one by `implement!`). Without the planted error every body is
//! checked and none is refused; with `let _x: u32 = T;` in the hand-written block the type error
//! is reported (the control: the body holding the block-local call is really checked). NOT
//! covered here: the fns of an impl inside a block are bodies of the block's module, which
//! engine-check plans no task for yet (body_types/covered.rs `written_by`: the block module's
//! text keeps no origins, so no entry covers them); the lane report names it.
//!
//! Expected line: `BLOCK EXTERN CRATE: clean 0 errors, planted <n> errors, <m> findings note(s)`.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain` (linked into a fresh home).

mod support;

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// How long one `engine-check` of the two small packages may take; a hang is a bug (rule 18).
const DEADLINE: Duration = Duration::from_secs(200);
const REAP: Duration = Duration::from_secs(10);

/// `dep`: the macro the blocks call, the trait, and `implement!`, which writes the whole block
/// at module level as a derive does (an expansion entry: engine-check's task for it checks
/// every body the expansion wrote, the impl's fn among them, engine_bodies.rs `groups`).
const DEP: &str = "#![no_std]\n#[macro_export]\nmacro_rules! the_macro {\n    () => {};\n}\npub trait Trait {\n    fn f(&self) -> u32;\n}\n#[macro_export]\nmacro_rules! implement {\n    ($t:ident, $e:expr) => {\n        const _: () = {\n            extern crate dep as _dep;\n            _dep::the_macro!();\n            impl _dep::Trait for $t {\n                fn f(&self) -> u32 {\n                    $e\n                }\n            }\n        };\n    };\n}\n";

/// `app`'s lib.rs: one block written by hand, one written by `implement!` (each constant's
/// initializer holds the call through the block's `_dep`); `{RET}` initializes a `u32` in the
/// hand-written block, so the planted `T` is an error of the body that holds the call.
const APP: &str = "#![no_std]\npub struct T;\npub struct U;\nconst _: () = {\n    extern crate dep as _dep;\n    _dep::the_macro!();\n    let _x: u32 = {RET};\n    impl _dep::Trait for T {\n        fn f(&self) -> u32 {\n            7\n        }\n    }\n};\ndep::implement!(U, 9);\npub fn g() -> u32 {\n    1\n}\n";

fn write(path: &Path, text: &str) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap_or_else(|error| panic!("making {}: {error}", dir.display()));
    }
    std::fs::write(path, text).unwrap_or_else(|error| panic!("writing {}: {error}", path.display()));
}

fn workspace(ws: &Path, ret: &str) {
    write(&ws.join("Cargo.toml"), "[workspace]\nmembers = [\"app\", \"dep\"]\nresolver = \"2\"\n");
    write(
        &ws.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"dep\"]\n\n[[package]]\nname = \"dep\"\nversion = \"0.1.0\"\n",
    );
    write(&ws.join("app/Cargo.toml"), "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n");
    write(&ws.join("app/src/lib.rs"), &APP.replace("{RET}", ret));
    write(&ws.join("dep/Cargo.toml"), "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    write(&ws.join("dep/src/lib.rs"), DEP);
}

/// `blaze engine-check` in `ws` with the home `home`: everything it said (kept as `<ws>.<what>`).
fn engine_check(ws: &Path, home: &Path, what: &str) -> String {
    let said = ws.with_extension(what);
    let file = File::create(&said).unwrap_or_else(|error| panic!("making {}: {error}", said.display()));
    let file_err = file.try_clone().expect("the output file, twice");
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .arg("engine-check")
        .current_dir(ws)
        .env("HOME", home)
        .env("CARGO_HOME", home.join(".cargo"))
        .env("PWD", ws)
        .env_remove("SDKROOT")
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(file_err))
        .spawn()
        .expect("blaze starts");
    let started = Instant::now();
    // Waiting for: `blaze engine-check` of two small packages, at most DEADLINE.
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
                panic!("`blaze engine-check` in {} did not end within {DEADLINE:?} (rule 18)", ws.display());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("waiting for `blaze engine-check` in {}: {error}", ws.display());
            }
        }
    }
    std::fs::read_to_string(&said).unwrap_or_else(|error| panic!("reading {}: {error}", said.display()))
}

/// The words of a body's findings note (engine_bodies.rs `BodiesStage::note`).
const FINDINGS: &str = "finding(s) of types or borrows";

/// The error count of the closing line (`<n> errors: blaze check`, `1 error: ..`).
fn errors(said: &str) -> u32 {
    let line = said.lines().rev().find(|line| line.ends_with(": blaze check")).unwrap_or_else(|| panic!("no closing line:\n{said}"));
    let words: Vec<&str> = line.split_whitespace().collect();
    let at = words.iter().position(|w| w.starts_with("error")).unwrap_or_else(|| panic!("no error count in `{line}`:\n{said}"));
    at.checked_sub(1).and_then(|n| words[n].parse().ok()).unwrap_or_else(|| panic!("no error count in `{line}`:\n{said}"))
}

/// No body refused, none past a gap: the only "not checked" lines allowed are the borrows
/// note every check prints while `BORROWS_ARE_ERRORS` is off.
fn refusals(said: &str) -> Vec<&str> {
    said.lines().filter(|line| (line.contains("lowering refused") || line.contains("NOT checked") || line.contains("not checked")) && !line.contains("borrows")).collect()
}

#[test]
fn a_call_through_a_blocks_extern_crate_is_checked_on_the_engine() {
    let base = fresh_home("engine_block_extern_crate", true);
    let (home, ws) = (base.join("home"), base.join("ws"));
    workspace(&ws, "7");
    let clean = engine_check(&ws, &home, "clean");
    assert!(refusals(&clean).is_empty(), "a body was not checked:\n{clean}");
    let clean_errors = errors(&clean);
    assert_eq!(clean_errors, 0, "the clean fixture has errors:\n{clean}");
    assert!(!clean.contains(FINDINGS), "the clean fixture has findings:\n{clean}");
    // The control: `let _x: u32 = T;` beside the block's call. engine-check
    // carries a body's finding count, not yet its words (engine_bodies.rs `note`): the note
    // naming `implement!`'s entry, or an error line, is the report.
    workspace(&ws, "T");
    let planted = engine_check(&ws, &home, "planted");
    assert!(refusals(&planted).is_empty(), "a body was not checked:\n{planted}");
    let planted_errors = errors(&planted);
    let noted = planted.lines().filter(|line| line.contains(FINDINGS)).count();
    let profile = std::fs::read_to_string(ws.join(".blaze/v2/profile.txt")).unwrap_or_else(|error| format!("(no profile: {error})"));
    let phases: String = profile.lines().skip_while(|line| !line.starts_with("engine phases")).take_while(|line| !line.starts_with("entered because")).collect::<Vec<_>>().join("\n");
    assert!(planted_errors >= 1 || noted >= 1, "the planted type error beside the block's call is not reported:\n{planted}\nclean:\n{clean}\n{phases}");
    println!("BLOCK EXTERN CRATE: clean {clean_errors} errors, planted {planted_errors} errors, {noted} findings note(s)\n{planted}");
}
