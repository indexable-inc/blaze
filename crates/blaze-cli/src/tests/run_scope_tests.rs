//! `blaze run` checks only what its program reaches (run.md "End state (D113)", stage 4): the
//! crate roots of the program package's member closure, which units.rs lists (each member's
//! units name the members it depends on) and blaze-drive run_scope.rs follows. A child of
//! main.rs's tests: its helpers are theirs.
//!
//! The workspace: `app` (the one package with a binary, so the program with no flag:
//! run_package.rs) depends on `lib_a`, which depends on `lib_b`; `lib_c` is a member nobody
//! depends on. One of the libraries holds a body with a type error (the Rust Reference, "Call
//! expressions": the argument `1u8` is not coerced to the parameter's `u16`).

use super::*;

/// The library with the type error: a callee and its bad caller.
const BAD: &str = "fn f(_x: u16) {}\npub fn bad() {\n    f(1u8);\n}\n";
const GOOD: &str = "pub fn good() {}\n";

fn package(name: &str, deps: &[&str]) -> std::string::String {
    let mut text = std::format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    if !deps.is_empty() {
        text.push_str("\n[dependencies]\n");
        for dep in deps {
            text.push_str(&std::format!("{dep} = {{ path = \"../{dep}\" }}\n"));
        }
    }
    text
}

/// The four members, the type error in `bad`'s library.
fn four_members(seed: u64, bad: &str) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"app\", \"lib_a\", \"lib_b\", \"lib_c\"]\nresolver = \"2\"\n");
    write(&io, "app/Cargo.toml", &package("app", &["lib_a"]));
    write(&io, "app/src/main.rs", "fn main() {}\n");
    write(&io, "lib_a/Cargo.toml", &package("lib_a", &["lib_b"]));
    write(&io, "lib_b/Cargo.toml", &package("lib_b", &[]));
    write(&io, "lib_c/Cargo.toml", &package("lib_c", &[]));
    for lib in ["lib_a", "lib_b", "lib_c"] {
        write(&io, &std::format!("{lib}/src/lib.rs"), if lib == bad { BAD } else { GOOD });
    }
    io
}

/// What a run came to, as one line a failing assertion quotes, with both streams (`run` draws
/// its findings beside the program's output).
fn said(ran: &Result<Checked, CliError>, io: &Io) -> std::string::String {
    let (out, err) = io.script(|host| (host.out.clone(), host.err.clone())).expect("a simulated host");
    let err = std::format!("{}{}", std::string::String::from_utf8(out).expect("UTF-8"), std::string::String::from_utf8(err).expect("UTF-8"));
    match ran {
        Ok(ran) => std::format!("errors {} status {:?} {:?} {:?}\n{err}", ran.errors, ran.status, ran.ids, ran.words),
        Err(error) => std::format!("error: {error}\n{err}"),
    }
}

/// The type error in `lib_c`, which the program does not reach: `check` finds it (the
/// control), `run` does not check `lib_c` and its error does not stop the start. In `lib_b`,
/// which `app` reaches through `lib_a`: `run` finds it, and the program does not start.
#[test]
fn run_checks_the_program_packages_closure_alone() {
    let checked = check(&four_members(31, "lib_c"), SMALL);
    assert_eq!(checked.errors, 1, "the control: `check` checks every member: {:?} {:?}", checked.ids, checked.words);

    let io = four_members(32, "lib_c");
    let ran = ask(&io, (Command::Run, "run"), SMALL);
    let seen = said(&ran, &io);
    assert!(!seen.contains("lib_c/src/lib.rs"), "`run` checked `lib_c`, which the program does not reach: {seen}");
    match &ran {
        Ok(ran) => assert_eq!(ran.errors, 0, "{seen}"),
        // The start was tried (nothing found stopped it); what stops it there is the start's own.
        Err(error) => assert!(matches!(error, CliError::Ran { .. } | CliError::OwnProcess(_) | CliError::Program(_) | CliError::RunNames(_)), "{seen}"),
    }

    let io = four_members(33, "lib_b");
    let ran = ask(&io, (Command::Run, "run"), SMALL);
    let seen = said(&ran, &io);
    let Ok(ran) = ran else { panic!("the run answers with its finding: {seen}") };
    assert_eq!((ran.errors, ran.status), (1, Some(program::NOT_RUN)), "`run` checks `lib_b`, which `app` reaches through `lib_a`: {seen}");
    assert!(seen.contains("lib_b/src/lib.rs"), "{seen}");
}
