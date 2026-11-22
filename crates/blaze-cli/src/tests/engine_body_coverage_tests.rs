//! NO BODY ENDS SILENTLY (lane BD1, 2026-11-18; the owner's rule "check is a whole check"). A
//! body entry the records list no body at was `ENTRY_ABSENT` (engine_bodies.rs `groups`), then
//! `BodyIs::Absent`: no finding and no note, so a blaze-edition root `main` (D104: replaced by
//! the launcher `std::main` writes) was never checked. Now such an entry is one of: the
//! launcher's body (`root_main` at `At::Expansion`, as body_types/covered.rs `root_of` takes it),
//! checked; cfg-false in its unit, absent by the language; or a named `Class::Unchecked` note
//! (`absent_entry`). The blaze-edition cases need bstd's `main!` and run as the integration
//! tests (crates/blaze-cli/tests/check_written_bodies.rs, check_unchecked_items.rs); this file
//! holds the half the simulated home can build: the cfg walk of `absent_entry` takes nothing
//! from what the language leaves out, and refuses nothing it keeps.
//!
//! The rule (the Reference, "Conditional compilation"): an item whose `cfg` predicate is false
//! is not part of the crate; `unix` is true on the fixture shelf (main.rs `SHELF_CFG`), `windows`
//! false. And "Type coercions": `fn f(x: u8) -> u16 { x }` is one E0308.

use super::engine_check_tests::said;
use super::*;

const ON_AND_OFF: &str = "#[cfg(unix)]\npub fn on(x: u8) -> u16 { x }\n\n#[cfg(windows)]\npub fn off(x: u8) -> u16 { x }\n";
/// The control: the cause of `off`'s absence (its false predicate) removed.
const BOTH_ON: &str = "#[cfg(unix)]\npub fn on(x: u8) -> u16 { x }\n\n#[cfg(unix)]\npub fn off(x: u8) -> u16 { x }\n";

/// What one fresh check of `lib` prints: its error lines, and its lines that say a body was
/// not checked.
fn checked(seed: u64, lib: &str) -> (usize, std::vec::Vec<std::string::String>) {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", lib);
    let (_, lines, checked) = said(&io, true);
    let errors = lines.iter().filter(|line| line.contains("error[")).count();
    let mut unchecked: std::vec::Vec<std::string::String> = lines.into_iter().filter(|line| line.contains("not checked") || line.contains("NOT checked: types")).collect();
    unchecked.extend(checked.bodies.0.into_iter().filter(|note| note.contains("not checked")));
    (errors, unchecked)
}

/// A CFG-FALSE BODY IS ABSENT, A CFG-TRUE ONE CHECKED, NEITHER NOTED: `on` (cfg true) prints
/// its E0308; `off` (cfg false) prints nothing and no note names a body unchecked. CONTROL:
/// with `off`'s predicate made true, its E0308 prints too (2 errors), so the one error is the
/// cfg's doing, not a body left out. Prints `CFG BODIES: 1 error (cfg false absent), 2 with it true; 0 unchecked notes`.
#[test]
fn a_cfg_false_body_is_absent_and_a_cfg_true_one_checked() {
    let (errors, unchecked) = checked(51, ON_AND_OFF);
    assert_eq!(errors, 1, "only the cfg-true body's E0308 prints; unchecked lines: {unchecked:?}");
    assert!(unchecked.is_empty(), "a body noted unchecked: {unchecked:?}");
    let (control, control_unchecked) = checked(53, BOTH_ON);
    assert_eq!(control, 2, "control: with both predicates true both E0308 print; unchecked lines: {control_unchecked:?}");
    assert!(control_unchecked.is_empty(), "control: a body noted unchecked: {control_unchecked:?}");
    println!("CFG BODIES: {errors} error (cfg false absent), {control} with it true; {} unchecked notes", unchecked.len());
}
