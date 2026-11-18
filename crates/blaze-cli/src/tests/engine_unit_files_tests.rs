//! Which unit checks each body (engine_body_plan.rs, lane RP1): a body is checked by a unit
//! exactly when that unit's records hold it, by the first in rank, once; what no unit holds is no
//! unit's, never a refusal: a file no unit holds says so once, an entry no unit holds is counted.
//! The real runs this answers: 943 bodies refused "its crate's items record lists no file at its
//! path" (`#[cfg(test)] mod tests;` files, `#[cfg(feature = ..)]` modules) and 179 "no body of
//! its crate's records is at this entry" (inline `#[cfg(test)]` items), each with an empty name.
//! A child of main.rs's tests: its helpers are theirs, and engine_check_tests.rs's.

use super::*;

use blaze_drive::{Kind, Name};
use blaze_engine::{CellId, Stage};
use blaze_ids::ContentId;
use blaze_own::Vec;
use std::alloc::Global;

use super::engine_check_tests::{entries, error_lines, profile, said};
use crate::engine_body_plan::{BodyClaim, BodyIs, Decision, FileClaim, FileIs, Holders, Unit};

// ---- The decision alone (pure) ----

const LIB: u32 = 0;
const TEST: u32 = 1;

fn unit(at: u8, test: bool) -> Unit {
    Unit { name: Name::new(Kind::CrateScope, ContentId::of("rp1 unit", &[at])), prefix: [at; 8], test }
}

fn body(at: u8) -> Name {
    Name::new(Kind::BodyCheck, ContentId::of("rp1 body", &[at]))
}

/// One read a refusal stands on: its unit's whole names cell.
fn refusal_cells(units: &[Unit], at: u32) -> &'static [CellId] {
    std::boxed::Box::leak(std::boxed::Box::new([CellId { of: units[at as usize].name, cell: crate::engine_tasks::WHOLE }]))
}

/// Holders over `units` (crate-list order) from (file 0) claims and (body) claims.
fn holders(units: &[Unit], files: &[(u32, FileIs)], bodies: &[(u32, BodyIs)]) -> Holders<'static> {
    let mut list: Vec<Unit, Global> = Vec::new_in(Global);
    list.extend(units.iter().copied());
    let mut claims: Vec<FileClaim<'static>, Global> = Vec::new_in(Global);
    for (unit, is) in files.iter() {
        claims.push(FileClaim { at: 0, unit: *unit, is: *is, reads: refusal_cells(units, *unit) });
    }
    let bodies: std::vec::Vec<BodyClaim> = bodies.iter().enumerate().map(|(took, (unit, is))| BodyClaim { name: body(1), unit: *unit, is: *is, took: u32::try_from(took).expect("a few claims") }).collect();
    Holders::new(list, claims, bodies.into_iter())
}

fn decide(holders: &Holders<'_>, at: u32) -> (Decision, std::vec::Vec<u32>, std::vec::Vec<CellId>) {
    let mut passed: Vec<u32, Global> = Vec::new_in(Global);
    let mut reads: Vec<CellId, Global> = Vec::new_in(Global);
    let decided = holders.decide(body(1), at, &mut passed, &mut reads);
    (decided, passed.iter().copied().collect(), reads.iter().copied().collect())
}

/// THE RULE, case by case: a file only the test unit holds is the test unit's (the 943); an
/// entry only the test unit holds is the test unit's (the 179); the lib ranks first though listed
/// second; nothing held is no unit's, never refused; a refusal is a refusal only where no unit
/// holds the body, and its cells are read either way. CONTROL: each case's decision differs from
/// its neighbour's by the one claim changed.
#[test]
fn engine_unit_files_decide_by_what_each_unit_holds() {
    let units = [unit(1, false), unit(2, true)];
    // (943) `#[cfg(test)] mod tests;`: the lib does not hold the file, the test unit does.
    let h = holders(&units, &[(LIB, FileIs::NotHeld), (TEST, FileIs::Held)], &[(TEST, BodyIs::Held)]);
    assert_eq!(decide(&h, 0).0, Decision::Taken { took: 0, unit: TEST });
    assert_eq!(decide(&h, 0).1, std::vec![LIB], "who checks it reads that the lib does not hold it");
    // (179) An inline `#[cfg(test)]` item: both hold the file, the lib's records not the entry.
    let h = holders(&units, &[(LIB, FileIs::Held), (TEST, FileIs::Held)], &[(LIB, BodyIs::Absent), (TEST, BodyIs::Held)]);
    assert_eq!(decide(&h, 0).0, Decision::Taken { took: 1, unit: TEST });
    // Both hold it: the lib checks it, once, reading nothing of the test unit.
    let h = holders(&units, &[(LIB, FileIs::Held), (TEST, FileIs::Held)], &[(LIB, BodyIs::Held), (TEST, BodyIs::Held)]);
    assert_eq!(decide(&h, 0), (Decision::Taken { took: 0, unit: LIB }, std::vec![], std::vec![]));
    // Rank, not list order: the test unit listed first, the lib still checks it.
    let swapped = [unit(2, true), unit(1, false)];
    let h = holders(&swapped, &[(0, FileIs::Held), (1, FileIs::Held)], &[(0, BodyIs::Held), (1, BodyIs::Held)]);
    assert_eq!(decide(&h, 0).0, Decision::Taken { took: 1, unit: 1 }, "a non-test unit ranks first");
    // `#[cfg(feature = "x")] mod gated;` with `x` off: no unit holds the file.
    let h = holders(&units, &[(LIB, FileIs::NotHeld), (TEST, FileIs::NotHeld)], &[]);
    assert_eq!(decide(&h, 0), (Decision::NotHeld, std::vec![LIB, TEST], std::vec![]));
    // `#[cfg(feature = "x")] fn off()`: both hold the file, neither the entry.
    let h = holders(&units, &[(LIB, FileIs::Held), (TEST, FileIs::Held)], &[(LIB, BodyIs::Absent), (TEST, BodyIs::Absent)]);
    assert_eq!(decide(&h, 0).0, Decision::Absent { took: Some(0) });
    // A refusing unit before a holding one: the holder checks it, on the refusal's cells too.
    let h = holders(&units, &[(LIB, FileIs::Refused("lib's names failed")), (TEST, FileIs::Held)], &[(TEST, BodyIs::Held)]);
    let (decided, _, reads) = decide(&h, 0);
    assert_eq!(decided, Decision::Taken { took: 0, unit: TEST });
    assert_eq!(reads, refusal_cells(&units, LIB).to_vec(), "the refusal's cells are read: the lib's fix may make it the lib's");
    // A refusal and no holder: refused, by the refusal's words.
    let h = holders(&units, &[(LIB, FileIs::Refused("lib's names failed")), (TEST, FileIs::NotHeld)], &[]);
    assert_eq!(decide(&h, 0).0, Decision::Refused { why: "lib's names failed", took: None });
    // Held file whose records list no entry at the name at all: named, never skipped (rule 3).
    let h = holders(&units, &[(LIB, FileIs::Held)], &[]);
    assert_eq!(decide(&h, 0).0, Decision::Refused { why: crate::engine_body_plan::NO_ENTRY, took: None });
    // A file no unit's module tree reaches.
    assert_eq!(decide(&h, 7).0, Decision::Unreached);
}

// ---- The command ----

/// The crate's lib and test units over: `#[cfg(test)] mod tests;` (a file the lib does not
/// hold), `#[cfg(feature = "x")] mod gated;` with `x` off (a file no unit holds), a `#[path]`
/// module, `src/orphan.rs` no `mod` names, a statement-level `#[cfg(test)]` item, a gated `fn`,
/// and an inline `#[cfg(test)]` module. `two` is the body of `src/tests.rs`'s second test, a
/// type error when `wrong`. `cut`: `mod tests;` without its `#[cfg(test)]`.
pub(super) fn units_fixture(io: &Io, wrong: bool, cut: bool) {
    write(io, "Cargo.toml", &format!("{MANIFEST}\n[features]\nx = []\n"));
    let tests_mod = if cut { "mod tests;\n" } else { "#[cfg(test)]\nmod tests;\n" };
    write(
        io,
        "src/lib.rs",
        &format!(
            "{tests_mod}#[cfg(feature = \"x\")]\nmod gated;\n#[path = \"other/place.rs\"]\nmod placed;\n\npub fn top() -> u8 {{\n    #[cfg(test)]\n    fn only_in_tests() -> u8 {{\n        2\n    }}\n    placed::here()\n}}\n\n#[cfg(feature = \"x\")]\npub fn off() -> u8 {{\n    5\n}}\n\n#[cfg(test)]\nmod inline_tests {{\n    #[test]\n    fn inline() {{}}\n}}\n"
        ),
    );
    let two = if wrong { "    let _wrong: u8 = true;\n" } else { "    let _right: u8 = helper();\n" };
    write(io, "src/tests.rs", &format!("use super::top;\n\nfn helper() -> u8 {{\n    top() + 1\n}}\n\n#[test]\nfn one() {{\n    let _ = helper();\n}}\n\n#[test]\nfn two() {{\n{two}}}\n"));
    write(io, "src/gated.rs", "pub fn g() -> u8 {\n    3\n}\n");
    write(io, "src/other/place.rs", "pub fn here() -> u8 {\n    1\n}\n");
    write(io, "src/orphan.rs", "pub fn lost() -> u8 {\n    true\n}\n");
}

/// The words the two refusal classes and an unnamed body were said with.
const REFUSED: [&str; 4] = ["lists no file at its path", "no body of its crate's records is at this entry", "the body of `` ", "body ``"];

fn file_notes(notes: &[std::string::String]) -> std::vec::Vec<&std::string::String> {
    notes.iter().filter(|note| note.contains("no unit of its crate holds this file")).collect()
}

/// NO UNIT'S BODY IS REFUSED, EACH BODY ONCE: on the fixture, no body is refused (neither class,
/// no body named ``), `src/gated.rs` is said once as a file no unit holds, the profile counts it
/// and the bodies two units hold, every Bodies task planned is entered once (lib and test units
/// share one task a body); with nothing changed the request is CERTIFIED and says the note again
/// from the lines record; after an edit elsewhere the kept note is a fresh run's.
/// CONTROL: before RP1 the same fixture gave the lib's "lists no file" refusal for each body of
/// `src/tests.rs` and `src/gated.rs`, and "no body ... at this entry" for `inline`.
#[test]
fn engine_unit_files_refuse_nothing_and_check_each_body_once() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    units_fixture(&io, false, false);
    let (_, first, checked) = said(&io, true);
    let notes = &checked.bodies.0;
    println!("printed:\n{}\nnotes: {notes:#?}\n{}", first.join("\n"), entries(&checked));
    for words in REFUSED.iter() {
        assert!(!notes.iter().any(|note| note.contains(words)), "no body refused by {words:?}: {notes:#?}");
        assert!(!first.iter().any(|line| line.contains(words)), "none printed by {words:?}");
    }
    let gated = file_notes(notes);
    assert_eq!(gated.len(), 1, "one note for the one file no unit holds: {notes:#?}");
    assert!(gated[0].contains("src/gated.rs"), "it names the file: {}", gated[0]);
    assert_eq!(first.iter().filter(|line| line.contains("no unit of its crate holds this file")).count(), 1, "printed once:\n{}", first.join("\n"));
    assert!(error_lines(&first.join("\n")).is_empty(), "no error: orphan.rs is no module's:\n{}", first.join("\n"));
    // No row with findings lacks its block: the head task of `gated.rs`'s `g` was one (a refused,
    // failed row no printer writes) before it became no unit's (engine_heads.rs `run_items`).
    assert!(!first.iter().any(|line| line.contains("have no block in the lines record")), "every row with findings is printed:\n{}", first.join("\n"));
    for (phase, planned, entered) in checked.phases.iter() {
        assert_eq!(planned, entered, "{phase:?}: each task planned is entered once (one task a body, whichever units hold it)");
    }
    let bodies: u32 = checked.phases.iter().filter(|(phase, _, _)| phase.stage == Stage::Bodies).map(|(_, planned, _)| *planned).sum();
    assert_eq!(checked.stages[Stage::Bodies.index()].entered, bodies, "cold: each body entered once");
    let line = profile(&io).lines().find(|line| line.contains("file(s) no unit holds under its cfg")).map(str::to_owned);
    let line = line.expect("the plan's line is in the profile");
    assert!(line.contains("; 1 file(s) no unit holds"), "the profile counts the file: {line}");
    // `top` (lib.rs) is held by the lib and the test unit: checked under the lib, counted.
    assert!(!line.contains("; 0 bodies held by 2 units"), "the bodies both units hold are counted: {line}");

    // Nothing changed: CERTIFIED (the note is no row's finding), the same note from the lines record.
    let (_, again, certificate) = said(&io, false);
    assert!(certificate.certified && certificate.entered == 0, "nothing changed: certified, nothing entered; {}", entries(&certificate));
    assert_eq!(again, first, "the note is said again from the stored lines");
    assert_eq!(again.iter().filter(|line| line.contains("no unit of its crate holds this file")).count(), 1, "once");

    // An edit elsewhere (a comment in other/place.rs): the gated file's rows stand, its note is
    // kept from the last lines record, as a fresh run says it.
    write(&io, "src/other/place.rs", "// moved\npub fn here() -> u8 {\n    1\n}\n");
    let (_, kept, _) = said(&io, false);
    let (_, fresh, _) = said(&io, true);
    let notes_of = |lines: &[std::string::String]| lines.iter().filter(|line| line.contains("no unit of its crate holds this file")).cloned().collect::<std::vec::Vec<_>>();
    assert_eq!(notes_of(&kept), notes_of(&fresh), "an incremental request says the note a fresh one says");
    assert_eq!(notes_of(&kept).len(), 1, "once");
}

/// A BODY ONLY THE TEST UNIT HOLDS IS CHECKED: a type error in `src/tests.rs` (held by the test
/// unit alone) is printed; `src/orphan.rs`'s is not (no `mod` names it). CONTROL: before RP1 the
/// lib's refusal won and no `error[` line was printed for `src/tests.rs`.
#[test]
fn engine_unit_files_check_what_the_test_unit_holds() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    units_fixture(&io, true, false);
    let (_, lines, checked) = said(&io, true);
    let text = lines.join("\n");
    println!("printed:\n{text}\n{}", entries(&checked));
    assert_eq!(error_lines(&text).len(), 1, "the one error in tests.rs is printed:\n{text}");
    assert!(text.contains("tests.rs"), "placed in tests.rs:\n{text}");
    assert!(!text.contains("orphan.rs"), "orphan.rs is no module's: never checked, never said:\n{text}");
}

/// THE DECISION IS INCREMENTAL: from the stored answer, taking `#[cfg(test)]` off `mod tests;`
/// makes the lib hold `src/tests.rs`: its bodies move to the lib (their reads held the lib's
/// items cell), so they are entered again, and the answer is a fresh run's. CONTROL: with
/// nothing changed, none was entered (`engine_unit_files_refuse_nothing_and_check_each_body_once`).
#[test]
fn engine_unit_files_follow_a_cfg_taken_off() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    units_fixture(&io, false, false);
    let _ = said(&io, true);
    units_fixture(&io, false, true);
    let (moved_ids, moved, moved_checked) = said(&io, false);
    let (fresh_ids, fresh, _) = said(&io, true);
    println!("cut:\n{}\n{}", moved.join("\n"), entries(&moved_checked));
    // helper, one and two (lib.rs's own bodies moved with its text too).
    assert!(moved_checked.stages[Stage::Bodies.index()].entered >= 3, "tests.rs's bodies entered again: {}", entries(&moved_checked));
    assert_eq!((moved_ids, moved), (fresh_ids, fresh), "the answer is a fresh run's");
}
