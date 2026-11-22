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
//! is reported (the control: the body holding the block-local call is really checked).
//!
//! THE BODIES OF A BLOCK'S MODULE (plan row A6): the fns of an impl inside a block are bodies of
//! the block's module, each checked by the task of the body whose block it is (body_types/
//! covered.rs `in_blocks`, walked by key from the block's text to its body). An error planted in
//! the hand-written block's impl fn, and one planted through `implement!`'s `$e` (a derive's
//! shape written by a `macro_rules!` call), are each the one `error[` line and "1 error". The
//! clean run checks each item body once: five (the two constants, their blocks' two `f`s, and
//! `g`) in three tasks (the hand-written constant, `implement!`'s entry, `g`), counted in the
//! profile's "item bodies" line. CONTROL: before `in_blocks`, the two `f`s were checked by no
//! task: three item bodies, and neither planted error was reported.
//!
//! Expected line: `BLOCK EXTERN CRATE: clean 0 errors, planted <n> errors, in the block's fn
//! <a>, through the macro <b>; <profile line>`.
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
/// hand-written block, so the planted `T` is an error of the body that holds the call; `{FN}`
/// is a statement in the block's impl fn, a body of the block's module; `{E}` is `implement!`'s
/// expression, the body of the fn of the block that call writes.
const APP: &str = "#![no_std]\npub struct T;\npub struct U;\nconst _: () = {\n    extern crate dep as _dep;\n    _dep::the_macro!();\n    let _x: u32 = {RET};\n    impl _dep::Trait for T {\n        fn f(&self) -> u32 {\n            {FN}7\n        }\n    }\n};\ndep::implement!(U, {E});\npub fn g() -> u32 {\n    1\n}\n";

/// What `workspace` writes into `APP`'s holes: clean, each is a value or nothing.
#[derive(Clone, Copy)]
struct Holes {
    ret: &'static str,
    fn_body: &'static str,
    e: &'static str,
}

const CLEAN: Holes = Holes { ret: "7", fn_body: "", e: "9" };

fn write(path: &Path, text: &str) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap_or_else(|error| panic!("making {}: {error}", dir.display()));
    }
    std::fs::write(path, text).unwrap_or_else(|error| panic!("writing {}: {error}", path.display()));
}

fn workspace(ws: &Path, holes: Holes) {
    write(&ws.join("Cargo.toml"), "[workspace]\nmembers = [\"app\", \"dep\"]\nresolver = \"2\"\n");
    write(
        &ws.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"dep\"]\n\n[[package]]\nname = \"dep\"\nversion = \"0.1.0\"\n",
    );
    write(&ws.join("app/Cargo.toml"), "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n");
    write(&ws.join("app/src/lib.rs"), &APP.replace("{RET}", holes.ret).replace("{FN}", holes.fn_body).replace("{E}", holes.e));
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

/// The profile's phase lines and its bodies lines, for a message.
fn profile_of(ws: &Path) -> String {
    let profile = std::fs::read_to_string(ws.join(".blaze/v2/profile.txt")).unwrap_or_else(|error| format!("(no profile: {error})"));
    let phases = profile.lines().skip_while(|line| !line.starts_with("engine phases")).take_while(|line| !line.starts_with("entered because"));
    let bodies = profile.lines().filter(|line| line.starts_with("bodies:"));
    phases.chain(bodies).collect::<Vec<_>>().join("\n")
}

/// The profile's item bodies line (engine_bodies.rs `BodiesStage::profile`): how many item bodies
/// were checked, in how many tasks, and how many bodies a macro wrote that no entry covers.
fn item_bodies(ws: &Path) -> (u32, u32, u32, String) {
    let profile = std::fs::read_to_string(ws.join(".blaze/v2/profile.txt")).unwrap_or_else(|error| panic!("no profile in {}: {error}", ws.display()));
    let line = profile.lines().find(|line| line.starts_with("bodies: ") && line.contains(" item bodies checked in ")).unwrap_or_else(|| panic!("no item bodies line in the profile:\n{profile}")).to_owned();
    let numbers: Vec<u32> = line.split(|c: char| !c.is_ascii_digit()).filter(|word| !word.is_empty()).filter_map(|word| word.parse().ok()).collect();
    match numbers[..] {
        [items, tasks, uncovered, ..] => (items, tasks, uncovered, line),
        _ => panic!("the item bodies line holds three counts: {line}"),
    }
}

/// One planted error: the run is not refused, prints exactly one `error[` line and says "1 error".
fn planted_once(ws: &Path, home: &Path, holes: Holes, what: &str) -> String {
    workspace(ws, holes);
    let said = engine_check(ws, home, what);
    assert!(refusals(&said).is_empty(), "{what}: a body was not checked:\n{said}");
    let lines: Vec<&str> = said.lines().filter(|line| line.contains("error[")).collect();
    assert_eq!(lines.len(), 1, "{what}: the planted type error is the one `error[` line:\n{said}\n{}", profile_of(ws));
    assert_eq!(errors(&said), 1, "{what}: the closing line says 1 error:\n{said}");
    assert!(said.lines().any(|line| line.ends_with(": blaze check") && line.contains("1 error")), "{what}: \"1 error\" closes the run:\n{said}");
    lines[0].to_owned()
}

#[test]
fn a_call_through_a_blocks_extern_crate_is_checked_on_the_engine() {
    let base = fresh_home("engine_block_extern_crate", true);
    let (home, ws) = (base.join("home"), base.join("ws"));
    workspace(&ws, CLEAN);
    let clean = engine_check(&ws, &home, "clean");
    assert!(refusals(&clean).is_empty(), "a body was not checked:\n{clean}");
    let clean_errors = errors(&clean);
    assert_eq!(clean_errors, 0, "the clean fixture has errors:\n{clean}");
    assert!(!clean.contains(FINDINGS), "the clean fixture has findings:\n{clean}");
    // Each item body checked once a view: the hand-written constant and its block's `f`;
    // `implement!`'s constant and its block's `f`; `g`. Three tasks: the constant, the call's
    // entry, `g`. `app` is a library, so its test build holds every body too, and D191: "Every
    // unit that holds a body checks it; the first holder in rank keeps the body's plain name
    // (the base), each other holder's row is the name at its view": five bodies in three tasks
    // at the base and at the test build's view (the profile's "base rows 3 checked, view rows 3
    // checked"). `implement!`'s block's `f` is the block's alone (covered.rs `block_stops`),
    // never also a body the call wrote: before that, 12 in 6.
    let (items, tasks, uncovered, line) = item_bodies(&ws);
    assert_eq!((items, tasks), (10, 6), "each item body checked once a view (D191: \"Every unit that holds a body checks it\"), the blocks' fns with their constants: {line}\n{}", profile_of(&ws));
    assert_eq!(uncovered, 0, "no body a macro wrote is left without a task: {line}");
    // The control: `let _x: u32 = T;` beside the block's call. engine-check
    // carries a body's finding count, not yet its words (engine_bodies.rs `note`): the note
    // naming `implement!`'s entry, or an error line, is the report.
    workspace(&ws, Holes { ret: "T", ..CLEAN });
    let planted = engine_check(&ws, &home, "planted");
    assert!(refusals(&planted).is_empty(), "a body was not checked:\n{planted}");
    let planted_errors = errors(&planted);
    let noted = planted.lines().filter(|line| line.contains(FINDINGS)).count();
    assert!(planted_errors >= 1 || noted >= 1, "the planted type error beside the block's call is not reported:\n{planted}\nclean:\n{clean}\n{}", profile_of(&ws));
    // A6: the error in the impl fn inside the hand-written block, and the one `implement!`'s
    // `$e` writes into its block's fn, each reported with its file and line.
    let in_fn = planted_once(&ws, &home, Holes { fn_body: "let _x: u32 = T;\n            ", ..CLEAN }, "in-block-fn");
    let in_macro = planted_once(&ws, &home, Holes { e: "{ let _y: u32 = T; 9 }", ..CLEAN }, "in-macro-block-fn");
    for (what, line) in [("in the block's fn", &in_fn), ("through the macro", &in_macro)] {
        assert!(line.contains("app/src/lib.rs:"), "the error {what} names its file and line: {line}");
    }
    println!("BLOCK EXTERN CRATE: clean {clean_errors} errors, planted {planted_errors} errors, in the block's fn {in_fn:?}, through the macro {in_macro:?}; {line}\n{planted}");
}
