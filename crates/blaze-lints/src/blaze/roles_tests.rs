//! `loop_turn_effects` and `sync_on_answer_path` over the effects tests' toolchain: per role, a
//! crate refused with the call chain named and a crate that passes; then `trusted` and a
//! malformed attribute. Each refusal is said once, at the marked function.

use super::effects_tests::{MINICORE, TOOLCHAIN};
use crate::tests::lint;

/// What the two role lints say of `main` (`src/lib.rs`).
fn roles_of(main: &str) -> String {
    let fixture = format!(
        "{MINICORE}{TOOLCHAIN}//- /src/lib.rs crate:main deps:alloc,std\n{}",
        main.trim_start_matches('\n')
    );
    lint(
        &fixture,
        "-A clippy::all --deny=blaze::loop_turn_effects --deny=blaze::sync_on_answer_path",
    )
}

/// The findings of the role `role` in `out`.
fn findings<'o>(out: &'o str, role: &str) -> Vec<&'o str> {
    let marker = format!("is `#[blaze::{role}]`");
    out.lines().filter(|line| line.contains(&marker)).collect()
}

// bounded_turn

/// A turn that takes a lock two calls down is refused at the turn, naming the chain to the
/// lock: `turn` → `drain` → `Mutex::lock`.
#[test]
fn bounded_turn_refuses_a_lock_through_a_call() {
    let out = roles_of(
        r#"
fn drain(m: &std::sync::Mutex<u32>) {
    let _guard = m.lock();
}
#[blaze::bounded_turn]
pub fn turn(m: &std::sync::Mutex<u32>) {
    drain(m)
}
"#,
    );
    let found = findings(&out, "bounded_turn");
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("`drain`"), "{out}");
    assert!(found[0].contains("std::sync::Mutex::lock"), "{out}");
}

/// A turn that blocks on a syscall that waits is refused by the symbol.
#[test]
fn bounded_turn_refuses_a_blocking_syscall() {
    let out = roles_of(
        r#"
unsafe extern "C" {
    fn read(fd: i32, buf: *mut u8, len: usize) -> isize;
}
#[blaze::bounded_turn]
pub fn turn(buf: &mut [u8; 8]) -> isize {
    unsafe { read(0, buf as *mut [u8; 8] as *mut u8, 8) }
}
"#,
    );
    let found = findings(&out, "bounded_turn");
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("`read`"), "{out}");
}

/// A turn of arithmetic through a clean call passes.
#[test]
fn bounded_turn_passes_a_clean_turn() {
    let out = roles_of(
        r#"
fn mix(a: u32, b: u32) -> u32 {
    a ^ b
}
#[blaze::bounded_turn]
pub fn turn(a: u32, b: u32) -> u32 {
    mix(a, b)
}
"#,
    );
    assert_eq!(findings(&out, "bounded_turn"), Vec::<&str>::new(), "{out}");
}

/// The allow-list is the accessor marked `trusted`: the turn passes, and the accessor itself
/// is not checked.
#[test]
fn bounded_turn_takes_a_trusted_accessor_at_its_word() {
    let out = roles_of(
        r#"
#[blaze::bounded_turn(trusted = "only the loop thread takes it")]
fn state(m: &std::sync::Mutex<u32>) {
    let _guard = m.lock();
}
#[blaze::bounded_turn]
pub fn turn(m: &std::sync::Mutex<u32>) {
    state(m)
}
"#,
    );
    assert_eq!(findings(&out, "bounded_turn"), Vec::<&str>::new(), "{out}");
}

/// An argument the attribute does not take is refused by its text.
#[test]
fn bounded_turn_refuses_a_malformed_attribute() {
    let out = roles_of(
        r#"
#[blaze::bounded_turn(often)]
pub fn turn() {}
"#,
    );
    assert!(out.contains("`#[blaze::bounded_turn(often)]` on `turn`"), "{out}");
}

// answers_request

/// An answer that syncs a file, through a call, is refused at the answer, naming `fsync`.
#[test]
fn answers_request_refuses_a_sync_through_a_call() {
    let out = roles_of(
        r#"
unsafe extern "C" {
    fn fsync(fd: i32) -> i32;
}
fn persist(fd: i32) -> i32 {
    unsafe { fsync(fd) }
}
#[blaze::answers_request]
pub fn answer(fd: i32) -> i32 {
    persist(fd)
}
"#,
    );
    let found = findings(&out, "answers_request");
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("`persist`"), "{out}");
    assert!(found[0].contains("`fsync`"), "{out}");
}

/// An answer may block or take a lock (that is `bounded_turn`'s), only not sync: it passes.
#[test]
fn answers_request_passes_a_lock() {
    let out = roles_of(
        r#"
#[blaze::answers_request]
pub fn answer(m: &std::sync::Mutex<u32>) {
    let _guard = m.lock();
}
"#,
    );
    assert_eq!(findings(&out, "answers_request"), Vec::<&str>::new(), "{out}");
}

/// A foreign function no table knows is refused by name, never taken as not syncing.
#[test]
fn answers_request_refuses_an_unknown_foreign_function() {
    let out = roles_of(
        r#"
unsafe extern "C" {
    fn mystery() -> i32;
}
#[blaze::answers_request]
pub fn answer() -> i32 {
    unsafe { mystery() }
}
"#,
    );
    let found = findings(&out, "answers_request");
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("`mystery`"), "{out}");
}
