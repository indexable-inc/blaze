//! `blaze check` must really check (the owner's rule): every body of the program is checked,
//! whoever wrote it, and its findings are printed; never a pass for a body that was not checked.
//!
//! The Reference: "Statements", "Item declarations" ("An item declaration statement has a
//! syntactic form identical to an item declaration within a module": a fn declared in a body
//! is a body of the program); "Attribute macros" (the item is replaced by the macro's output);
//! "Literal expressions" (a string literal is `&'static str`, never `u8`: `let m: u8 = "main";`
//! is E0308, mismatched types). D104: a blaze-edition root `fn main` is `std::main`'s input and
//! names its grants (`fn main(cx: &Cx, out: Out)`); bstd's `main!` (blaze-std entry.rs) writes
//! the launcher `fn main() -> Exit { fn main(..) <the written body> .. }`, so the body the user
//! wrote is a LOCAL fn of a body a macro wrote.
//!
//! THE PROGRAM: `#[std::main] fn main(cx: &Cx, out: Out)` holding a planted E0308, and `deep`,
//! a fn declared in `mid`, declared in `outer` (two blocks down), holding another.
//!
//! What makes both print (blaze-cli body_types/covered.rs): the entry `main` stands for the
//! body the attribute wrote in its place, and every entry's check covers each fn declared in a
//! body it reaches (front2 every_body.rs `each_from`).
//!
//! CONTROL (rule 8, run once by the integrator with the fix out): with body_types.rs's old
//! `check` (the root body alone, by chain), neither E0308 prints: `main`'s entry is the note
//! "front2 lists no body at this item", and `outer`'s check never reaches `deep`; this test
//! then fails with "2 planted E0308, 0 printed".
//!
//! Expected line: `WRITTEN BODIES CLI: 2 E0308 printed (the written main, outer::mid::deep)`.
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

const MANIFEST: &str = "[package]\nname = \"written-bodies\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n";

const MAIN: &str = "#[std::main]
fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    let m: u8 = \"main\";
    outer();
    out.line(cx, \"written\")?;
    Ok(())
}

fn outer() {
    fn mid() {
        fn deep() {
            let d: u8 = \"deep\";
        }
        deep();
    }
    mid();
}
";

/// The type error's code as the agent view prints it. D70: "`agent` is one or two plain lines
/// per finding with nothing decorative: the id, the code, `file:line:col`, the message", and
/// the agent view is the one a file is given; the checker's message ends in the code and its
/// class: "found `&str`, expected `u8` (E0308 Mismatch)". The person view's frame label ("type
/// error {0}", blaze-report frame/words.rs Cli rule 17) is drawn on a terminal alone.
const E0308: &str = "(E0308 Mismatch)";

/// `blaze check` in `dir` with the home `home`: standard output and error together, through a
/// file (no pipe can fill and stall the child), waited for at most [`DEADLINE`].
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
    // Waiting for: `blaze check` of the one-file package, at most DEADLINE.
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
fn every_written_body_is_checked_by_the_command() {
    let base = fresh_home("check_written_bodies", true);
    let dir = base.join("written-bodies");
    std::fs::create_dir_all(dir.join("src")).expect("the package's folders");
    std::fs::write(dir.join("Cargo.toml"), MANIFEST).expect("the manifest");
    // A lock to match, so the command has a unit graph and checks types (body_types.rs `NO_GRAPH`).
    std::fs::write(dir.join("Cargo.lock"), "version = 4\n\n[[package]]\nname = \"written-bodies\"\nversion = \"0.1.0\"\n").expect("the lock");
    std::fs::write(dir.join("src/main.rs"), MAIN).expect("src/main.rs");
    let said = check(&dir, &base.join("home"));
    let _ = std::fs::remove_dir_all(&base);

    let printed = said.matches(E0308).count();
    assert_eq!(printed, 2, "2 planted E0308, {printed} printed:\n{said}");
    // Each finding says which body it is in (covered.rs `Shown`): the body the user wrote, the
    // local `main` of the launcher `main!` wrote in its place (D104), and `deep` two blocks
    // down. The launcher is a body a macro wrote at the entry (covered.rs `written_by`), named
    // by its macro, so its local is `<the launcher>::main`; the entry's REPLACED_MAIN name
    // (engine_bodies.rs) stands only where no written body is found at the entry. Once, under
    // one name (rule 77: the block's module of the launcher lists it no second time,
    // covered.rs `in_blocks`).
    assert!(said.contains("in `{the expansion of `std::main!`}::main::main`"), "the written main's E0308 names its body:\n{said}");
    assert!(said.contains("in `outer::mid::deep`"), "deep's E0308 names its body:\n{said}");
    // Never a pass for a body that was not checked: no "lists no body" note for `main`.
    assert!(!said.contains("front2 lists no body at this item"), "an entry left unchecked:\n{said}");
    println!("WRITTEN BODIES CLI: {printed} E0308 printed (the written main, outer::mid::deep)");
}
