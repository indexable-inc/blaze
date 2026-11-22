//! Which units check each body (engine_body_plan.rs, lane RP1, D191): a body is checked by each
//! unit whose records hold it, each row decided from its own unit's records alone; what no unit
//! holds is no unit's, never a refusal: a file no unit holds says so once, an entry no unit holds
//! is counted.
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

use super::engine_check_tests::{boxed, engine_opts, entries, error_lines, opts, profile, said};
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
        claims.push(FileClaim { file: file(0), unit: *unit, is: *is, reads: refusal_cells(units, *unit) });
    }
    let bodies: std::vec::Vec<BodyClaim> = bodies.iter().enumerate().map(|(took, (unit, is))| BodyClaim { name: body(1), unit: *unit, is: *is, took: u32::try_from(took).expect("a few claims") }).collect();
    Holders::new(list, claims, bodies.into_iter())
}

/// The `FileItems` name of test file `at`: claims are made, and decisions asked, by it.
fn file(at: u32) -> Name {
    Name::new(Kind::FileItems, ContentId::of("rp1 file", &at.to_le_bytes()))
}

/// Unit `unit`'s row of body 1 of file `at`: its decision, the units whose not-held cell it
/// reads, and its refusal's cells.
fn decide(holders: &Holders<'_>, at: u32, unit: u32) -> (Decision, std::vec::Vec<u32>, std::vec::Vec<CellId>) {
    let mut passed: Vec<u32, Global> = Vec::new_in(Global);
    let mut reads: Vec<CellId, Global> = Vec::new_in(Global);
    let decided = holders.decide(body(1), file(at), unit, &mut passed, &mut reads);
    (decided, passed.iter().copied().collect(), reads.iter().copied().collect())
}

/// THE RULE (D191), case by case, each unit's row of one body: a unit whose records hold the
/// body checks it (both, where both hold it); a unit that does not hold the file or the entry has
/// no body there, reading its own not-held cell alone; a unit's refusal is that row's alone and
/// never decides another unit's row; nothing held is no unit's (the plan's note), never refused.
/// CONTROL: each case's decision differs from its neighbour's by the one claim changed, and the
/// test unit's row and the lib's row of one body differ where the two units' claims differ.
#[test]
fn engine_unit_files_decide_by_what_each_unit_holds() {
    use crate::engine_body_plan::OutsideIs;
    let units = [unit(1, false), unit(2, true)];
    // (943) `#[cfg(test)] mod tests;`: the lib does not hold the file, the test unit does. The
    // lib's row says so on its own cell; the test unit's checks it.
    let h = holders(&units, &[(LIB, FileIs::NotHeld), (TEST, FileIs::Held)], &[(TEST, BodyIs::Held)]);
    assert_eq!(decide(&h, 0, LIB), (Decision::NotHeld, std::vec![LIB], std::vec![]), "the lib's row reads that the lib does not hold it");
    assert_eq!(decide(&h, 0, TEST), (Decision::Taken { took: 0, unit: TEST }, std::vec![], std::vec![]));
    assert_eq!(h.outside(body(1), file(0), 2), None, "a unit holds it: no unit's note");
    // (179) An inline `#[cfg(test)]` item: both hold the file, the lib's records not the entry.
    let h = holders(&units, &[(LIB, FileIs::Held), (TEST, FileIs::Held)], &[(LIB, BodyIs::Absent), (TEST, BodyIs::Held)]);
    assert_eq!(decide(&h, 0, LIB), (Decision::Absent { took: Some(0) }, std::vec![LIB], std::vec![]));
    assert_eq!(decide(&h, 0, TEST).0, Decision::Taken { took: 1, unit: TEST });
    // Both hold it: EACH checks it (D191), each reading nothing of the other.
    let h = holders(&units, &[(LIB, FileIs::Held), (TEST, FileIs::Held)], &[(LIB, BodyIs::Held), (TEST, BodyIs::Held)]);
    assert_eq!(decide(&h, 0, LIB), (Decision::Taken { took: 0, unit: LIB }, std::vec![], std::vec![]));
    assert_eq!(decide(&h, 0, TEST), (Decision::Taken { took: 1, unit: TEST }, std::vec![], std::vec![]));
    // List order is the unit's index, not its rank: the test unit listed first is still decided
    // by its own claim (rank names the rows, engine_body_plan.rs `Reach`; a decision is per unit).
    let swapped = [unit(2, true), unit(1, false)];
    let h = holders(&swapped, &[(0, FileIs::Held), (1, FileIs::Held)], &[(0, BodyIs::Held), (1, BodyIs::Held)]);
    assert_eq!(decide(&h, 0, 1).0, Decision::Taken { took: 1, unit: 1 });
    assert_eq!(decide(&h, 0, 0).0, Decision::Taken { took: 0, unit: 0 });
    // `#[cfg(feature = "x")] mod gated;` with `x` off: no unit holds the file: no unit's.
    let h = holders(&units, &[(LIB, FileIs::NotHeld), (TEST, FileIs::NotHeld)], &[]);
    assert_eq!(decide(&h, 0, LIB), (Decision::NotHeld, std::vec![LIB], std::vec![]));
    assert_eq!(decide(&h, 0, TEST), (Decision::NotHeld, std::vec![TEST], std::vec![]));
    assert_eq!(h.outside(body(1), file(0), 2), Some(OutsideIs::FileNotHeld));
    // CONTROL of the note: one of the two reachers made no claim (not visited): not known.
    assert_eq!(h.outside(body(1), file(0), 3), None, "a reacher with no claim may hold it");
    // `#[cfg(feature = "x")] fn off()`: both hold the file, neither the entry.
    let h = holders(&units, &[(LIB, FileIs::Held), (TEST, FileIs::Held)], &[(LIB, BodyIs::Absent), (TEST, BodyIs::Absent)]);
    assert_eq!(decide(&h, 0, LIB).0, Decision::Absent { took: Some(0) });
    assert_eq!(decide(&h, 0, TEST).0, Decision::Absent { took: Some(1) });
    assert_eq!(h.outside(body(1), file(0), 2), Some(OutsideIs::EntryAbsent));
    // A refusing lib and a holding test unit: each row is its own unit's; the lib's refusal reads
    // its cells, the test unit's check reads none of the lib's.
    let h = holders(&units, &[(LIB, FileIs::Refused("lib's names failed")), (TEST, FileIs::Held)], &[(TEST, BodyIs::Held)]);
    assert_eq!(decide(&h, 0, LIB), (Decision::Refused { why: "lib's names failed", took: None }, std::vec![], refusal_cells(&units, LIB).to_vec()));
    assert_eq!(decide(&h, 0, TEST), (Decision::Taken { took: 0, unit: TEST }, std::vec![], std::vec![]));
    assert_eq!(h.outside(body(1), file(0), 2), None, "a refusal is not known to be no unit's");
    // Held file whose records list no entry at the name at all: named, never skipped (rule 3).
    let h = holders(&units, &[(LIB, FileIs::Held)], &[]);
    assert_eq!(decide(&h, 0, LIB).0, Decision::Refused { why: crate::engine_body_plan::NO_ENTRY, took: None });
    // A unit the reach gives the file that made no claim while another did: named, reads nothing.
    assert_eq!(decide(&h, 0, TEST), (Decision::Refused { why: crate::engine_body_plan::NO_CLAIM, took: None }, std::vec![], std::vec![]));
    // A file no unit's module tree reaches.
    assert_eq!(decide(&h, 7, LIB).0, Decision::Unreached);
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

/// The words of the plan's note of a file no unit holds (engine_body_plan.rs `FileWords`).
const FILE_NOTE: &str = "no unit checks this file";

/// The words the two refusal classes and an unnamed body were said with.
const REFUSED: [&str; 4] = ["lists no file at its path", "no body of its crate's records is at this entry", "the body of `` ", "body ``"];

fn file_notes(notes: &[std::string::String]) -> std::vec::Vec<&std::string::String> {
    notes.iter().filter(|note| note.contains(FILE_NOTE)).collect()
}

/// NO UNIT'S BODY IS REFUSED, EACH ROW ONCE: on the fixture, no body is refused (neither class,
/// no body named ``), `src/gated.rs` is said once as a file no unit holds, the profile counts it
/// and the view rows checked, every Bodies task planned is entered once (a row a body and unit
/// that reaches it, D191); with nothing changed the request is CERTIFIED and says the note again
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
    assert!(gated[0].contains("by the `mod` in `src/lib.rs`"), "it names the file whose `mod` is cut: {}", gated[0]);
    assert_eq!(first.iter().filter(|line| line.contains(FILE_NOTE)).count(), 1, "printed once:\n{}", first.join("\n"));
    assert!(error_lines(&first.join("\n")).is_empty(), "no error: orphan.rs is no module's:\n{}", first.join("\n"));
    // No row with findings lacks its block: the head task of `gated.rs`'s `g` was one (a refused,
    // failed row no printer writes) before it became no unit's (engine_heads.rs `run_items`).
    assert!(!first.iter().any(|line| line.contains("have no block in the lines record")), "every row with findings is printed:\n{}", first.join("\n"));
    for (phase, planned, entered) in checked.phases.iter() {
        assert_eq!(planned, entered, "{phase:?}: each task planned is entered once (one task a body and unit that reaches it)");
    }
    let bodies: u32 = checked.phases.iter().filter(|(phase, _, _)| phase.stage == Stage::Bodies).map(|(_, planned, _)| *planned).sum();
    assert_eq!(checked.stages[Stage::Bodies.index()].entered, bodies, "cold: each row entered once");
    let line = profile(&io).lines().find(|line| line.contains("file(s) no unit holds (cfg or macro)")).map(str::to_owned);
    let line = line.expect("the plan's line is in the profile");
    assert!(line.contains("; 1 file(s) no unit holds"), "the profile counts the file: {line}");
    // `top` (lib.rs) is held by the lib and the test unit: checked by each, the test unit's row
    // at its view, counted (D191).
    assert!(!line.contains("view rows 0 checked"), "the rows the test unit checks at its view are counted: {line}");

    // Nothing changed: CERTIFIED (the note is no row's finding), the same note from the lines record.
    let (_, again, certificate) = said(&io, false);
    assert!(certificate.certified && certificate.entered == 0, "nothing changed: certified, nothing entered; {}", entries(&certificate));
    assert_eq!(again, first, "the note is said again from the stored lines");
    assert_eq!(again.iter().filter(|line| line.contains(FILE_NOTE)).count(), 1, "once");

    // An edit elsewhere (a comment in other/place.rs): the gated file's rows stand, its note is
    // kept from the last lines record, as a fresh run says it.
    write(&io, "src/other/place.rs", "// moved\npub fn here() -> u8 {\n    1\n}\n");
    let (_, kept, _) = said(&io, false);
    let (_, fresh, _) = said(&io, true);
    let notes_of = |lines: &[std::string::String]| lines.iter().filter(|line| line.contains(FILE_NOTE)).cloned().collect::<std::vec::Vec<_>>();
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

// ---- Every unit that reaches a body checks it (D191, tests (a), (b), (d), (e)) ----

/// One package `wide`: `src/lib.rs` (the lib and its test build reach it) with `shared`, whose
/// body holds `stmt`, and `tests/it.rs` (its integration test alone reaches it) with `alone`,
/// whose body returns `alone`.
fn shared_fixture(io: &Io, stmt: &str, alone: u8) {
    write(io, "Cargo.toml", MANIFEST);
    write(io, "src/lib.rs", &format!("pub fn shared() -> u8 {{\n{stmt}    1\n}}\n"));
    write(io, "tests/it.rs", &format!("fn alone() -> u8 {{\n    {alone}\n}}\n\n#[test]\nfn it() {{\n    let _ = alone();\n}}\n"));
}

/// The Bodies tasks a request entered.
fn bodies_entered(checked: &engine_check::Checked) -> u32 {
    checked.stages[Stage::Bodies.index()].entered
}

/// (a) A TEST-ONLY ERROR IS REPORTED, NAMING THE TEST BUILD: a `#[cfg(test)]` statement of a
/// lib body that does not type-check is an error of the test build's row alone, printed once
/// with "(in the test build of `wide`)". CONTROL: the lib's own row (the one row "the first
/// holder checks" checked) holds no error; `engine_lines_replay_tests.rs`'s
/// `a_finding_only_some_rows_hold_names_their_view` controls the words, and the view row's
/// plan `engine_unit_files_first_only_plans_no_view_row`.
#[test]
fn engine_unit_files_a_test_only_error_names_the_test_build() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    shared_fixture(&io, "    #[cfg(test)]\n    let _only: u8 = true;\n", 1);
    let (_, said_lines, checked) = said(&io, true);
    let text = said_lines.join("\n");
    println!("printed:\n{text}\n{}", entries(&checked));
    assert_eq!(error_lines(&text).len(), 1, "the test build's error, once:\n{text}");
    assert!(text.contains("(in the test build of `wide`)"), "named the test build:\n{text}");
}

/// "The first holder checks" (the design D191 deleted), as a view plan: every holder's row is
/// the base's (test only, the control of (a)).
struct FirstOnly;

impl blaze_drive::view::ViewPlan for FirstOnly {
    const NAME: &'static str = "first only";

    fn view_of(_holders: &[blaze_drive::view::ViewKey], _at: usize) -> blaze_drive::view::UnitView {
        blaze_drive::view::UnitView::Base
    }
}

/// The names `P` gives a body's rows over `views` (engine_bodies.rs `plan_views`'s inner loop),
/// sorted and each once, as the plan keeps them.
fn rows_named<P: blaze_drive::view::ViewPlan>(body: Name, views: &[blaze_drive::view::ViewKey]) -> std::vec::Vec<Name> {
    let mut names: std::vec::Vec<Name> = (0..views.len()).map(|at| blaze_drive::view::at_view(body, P::view_of(views, at))).collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// (a)'s CONTROL, THE VIEW ROW'S PLAN: over the lib's and its test build's views, the production
/// plan names two rows of a body (the base, and the test build's at its view, which checks the
/// `#[cfg(test)]` statement); "the first holder checks" names the base alone, so the test
/// build's error has no row to be found in, and nothing is reported.
#[test]
fn engine_unit_files_first_only_plans_no_view_row() {
    let body = Name::new(Kind::BodyCheck, ContentId([7; 32]));
    let views = [blaze_drive::view::ViewKey(ContentId([1; 32])), blaze_drive::view::ViewKey(ContentId([2; 32]))];
    let full = rows_named::<blaze_drive::view::Production>(body, &views);
    assert_eq!(full.len(), 2, "a row a unit: {full:?}");
    assert!(full.contains(&body), "the base row keeps the body's name");
    assert_eq!(rows_named::<FirstOnly>(body, &views), [body], "CONTROL: the first holder's row alone");
}

/// (b) AN ERROR BOTH VIEWS HOLD PRINTS ONE LINE, naming no view. CONTROL: the fold off at its
/// one seam prints it twice (`engine_lines_replay_tests.rs`
/// `a_finding_every_row_holds_prints_once`, over the blocks of the same shape).
#[test]
fn engine_unit_files_an_error_both_views_hold_prints_once() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    shared_fixture(&io, "    let _both: u8 = true;\n", 1);
    let (_, said_lines, checked) = said(&io, true);
    let text = said_lines.join("\n");
    println!("printed:\n{text}\n{}", entries(&checked));
    assert_eq!(error_lines(&text).len(), 1, "one line for the two rows' one error:\n{text}");
    assert!(!text.contains("(in ") && !text.contains("(not in "), "every row holds it: no view named:\n{text}");
}

/// (d) THE COST RULE: an edit of a body two units reach enters its two rows; of a body one unit
/// reaches, its one row; a rerun with nothing changed enters none and prints the same.
#[test]
fn engine_unit_files_an_edit_enters_a_row_a_unit() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    shared_fixture(&io, "", 1);
    let _ = said(&io, true);
    shared_fixture(&io, "    let _x = 0u8;\n", 1);
    let (_, _, shared) = said(&io, false);
    println!("shared edit: {}", entries(&shared));
    assert_eq!(bodies_entered(&shared), 2, "the lib's row and the test build's: {}", entries(&shared));
    shared_fixture(&io, "    let _x = 0u8;\n", 2);
    let (_, edited, alone) = said(&io, false);
    println!("an unshared edit: {}", entries(&alone));
    assert_eq!(bodies_entered(&alone), 1, "the integration test's row alone: {}", entries(&alone));
    let (_, again, nothing) = said(&io, false);
    assert!(nothing.certified && nothing.entered == 0, "nothing changed: none entered: {}", entries(&nothing));
    assert_eq!(again, edited, "and the same lines");
}

/// (e) A LIB AND A BIN THAT `mod` ONE FILE: the file's body has a row a unit, the bin's too, so
/// its edit enters more rows than an edit of a body only the lib and its test build reach.
#[test]
fn engine_unit_files_a_file_a_lib_and_a_bin_mod_has_the_bins_row() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let files = |io: &Io, common: u8, own: u8| {
        write(io, "Cargo.toml", MANIFEST);
        write(io, "src/lib.rs", &format!("pub mod common;\n\npub fn own() -> u8 {{\n    {own}\n}}\n"));
        write(io, "src/main.rs", "mod common;\n\nfn main() {\n    let _ = common::both();\n}\n");
        write(io, "src/common.rs", &format!("pub fn both() -> u8 {{\n    {common}\n}}\n"));
    };
    files(&io, 1, 1);
    let (_, first, checked) = said(&io, true);
    println!("first:\n{}\n{}", first.join("\n"), entries(&checked));
    files(&io, 2, 1);
    let (_, _, common) = said(&io, false);
    files(&io, 2, 2);
    let (_, _, lib_only) = said(&io, false);
    println!("common.rs: {}\nlib.rs: {}", entries(&common), entries(&lib_only));
    assert_eq!(bodies_entered(&lib_only), 2, "lib.rs: the lib's row and its test build's: {}", entries(&lib_only));
    assert!(bodies_entered(&common) > bodies_entered(&lib_only), "common.rs: the bin's row too: {} vs {}", entries(&common), entries(&lib_only));
}

/// (e) A `#[cfg(test)]` ITEM IS THE TEST BUILD'S ROW ALONE: `only_in_tests` (a statement-level
/// item of `top`, its own body) with a type error is checked by the test build's row only (the
/// lib's row is `Decision::Absent`, reading its own claim's cell:
/// `engine_unit_files_decide_by_what_each_unit_holds`, case 179), so its error prints once,
/// naming the test build. CONTROL: without its `#[cfg(test)]` both rows hold and check it: one
/// line, no view named.
#[test]
fn engine_unit_files_a_test_only_item_is_the_test_builds_row() {
    let lib = |gate: &str| std::format!("pub fn top() -> u8 {{\n    {gate}fn only_in_tests() -> u8 {{\n        true\n    }}\n    1\n}}\n");
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", &lib("#[cfg(test)]\n    "));
    let (_, gated, checked) = said(&io, true);
    let text = gated.join("\n");
    println!("gated:\n{text}\n{}", entries(&checked));
    assert_eq!(error_lines(&text).len(), 1, "the test build's row alone checks it:\n{text}");
    assert!(text.contains("(in the test build of `wide`)"), "named the test build:\n{text}");
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", &lib(""));
    let (_, both, _) = said(&io, true);
    let text = both.join("\n");
    assert_eq!(error_lines(&text).len(), 1, "CONTROL: both rows hold it, one line:\n{text}");
    assert!(!text.contains("(in ") && !text.contains("(not in "), "CONTROL: no view named:\n{text}");
}

// ---- The plan's reach on a request whose Units stage runs (D191, engine_bodies.rs `plan_reach`) ----

/// What the check under `opts` printed (as [`said`] cuts it) and its counts.
fn said_under(io: &Io, opts: engine_check::Opts<'static>) -> (std::vec::Vec<std::string::String>, engine_check::Checked) {
    let before = stdout(io).len();
    let (_, checked) = engine_opts(io, opts);
    let text = stdout(io).split_off(before);
    let mut lines: std::vec::Vec<std::string::String> = text.lines().map(str::to_owned).collect();
    lines.pop();
    (lines, checked)
}

/// The check under `opts`, its error kept (not a panic): the control's stop.
fn check_result(io: &Io, opts: engine_check::Opts<'static>) -> Result<engine_check::Checked, CliError> {
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let out = io.need_out().expect("out");
    let err = io.need_err().expect("err");
    let mut phases = boxed(|| Phases::new(io.need_clock().expect("a clock")));
    let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, View::Person, "/ws", cx.frame());
    let fs = io.need_fs().expect("files");
    let host = io.need_host().expect("host");
    let mut rng = io.need_rng().expect("rng");
    let args = io.args();
    let net = io.net();
    engine_check::check(&cx, &fs, &host, &mut rng, args.as_ref(), net.as_ref(), &root, opts, &mut shown, &mut phases, &out, &err)
}

/// The profile's count of the rows checked at a unit's view ("view rows N checked").
fn view_rows(io: &Io) -> u32 {
    let text = profile(io);
    let line = text.lines().find(|line| line.contains("; view rows ")).expect("the plan's line is in the profile");
    let after = line.split("; view rows ").nth(1).expect("its view rows");
    after.split(' ').next().and_then(|count| count.parse().ok()).unwrap_or_else(|| panic!("a count: {line}"))
}

/// The command's options with another blaze's tools: every record's key moves, so no stored
/// row stands (the first command after a new blaze reads the stored answer and enters every row).
fn new_blaze(io: &Io) -> engine_check::Opts<'static> {
    let mut opts = opts(io, 2, false);
    let producers = blaze_ids::producer::Producers::for_tests(b"a new blaze");
    opts.tools = engine_store::tools(&home::tools(&producers, lints::Lints::NONE.rules));
    opts
}

/// THE PLAN'S REACH NEEDS NO RECORD IN THE STORE: a first check in a fresh home of a lib and its
/// tests passes, checks rows at the test build's view and prints the test build's one error;
/// then the first check after a new blaze (every record's key moved, so the Units stage runs
/// again) passes with the same answer and the same view rows. CONTROL: the reach read from the
/// graph's record in the store, as the plan read it before (engine_units.rs
/// `PlanPlant::ReachFromStore`), stops the fresh check: the store holds no record of the
/// `UnitGraph` at the plan (the engine files it when it merges the Units stage).
#[test]
fn engine_unit_files_reach_on_a_first_check_and_after_a_new_blaze() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    units_fixture(&io, true, false);
    let (_, first, checked) = said(&io, true);
    println!("first:\n{}\n{}", first.join("\n"), entries(&checked));
    let rows = view_rows(&io);
    assert!(rows > 0, "the test build's rows of lib.rs's bodies are checked at its view");
    assert_eq!(error_lines(&first.join("\n")).len(), 1, "the one error in tests.rs:\n{}", first.join("\n"));

    let (renewed, checked) = said_under(&io, new_blaze(&io));
    println!("a new blaze:\n{}\n{}", renewed.join("\n"), entries(&checked));
    // (T) The checkout's answer is one whichever tools wrote it (engine_answer.rs "THE TOOLS":
    // the root is named by the checkout alone, each row valid by its own key): a new blaze
    // starts from the stored answer, not the certificate, and every row's key moved. Was
    // `!checked.from_last` ("a new blaze reads no stored answer").
    assert!(checked.from_last && !checked.certified, "THE TOOLS: a new blaze starts from the stored answer and enters every row by its moved key, never the certificate: {}", entries(&checked));
    assert_eq!(renewed, first, "a new blaze answers as the first check did");
    assert_eq!(view_rows(&io), rows, "the same rows at the test build's view");

    let control = Io::simulated(SimWorld::new(11, Limits::USUAL));
    units_fixture(&control, true, false);
    crate::engine_units::PLAN_PLANT.with(|cell| cell.set(crate::engine_units::PlanPlant::ReachFromStore));
    let stopped = check_result(&control, opts(&control, 2, true));
    crate::engine_units::PLAN_PLANT.with(|cell| cell.set(crate::engine_units::PlanPlant::Built));
    match stopped {
        Err(CliError::Input { name, why: work::InputWhy::Absent }) if name == Name::unit_graph() => {}
        Err(other) => panic!("the control stops on the graph's missing record, not {other:?}"),
        Ok(checked) => panic!("the control stops: the graph's record is not in the store at the plan; it answered {}", entries(&checked)),
    }
}

/// Two members: `a` with `#[cfg(test)] mod tests;` (its test build's file, one type error) and
/// `b`; `edge`: `b` depends on `a` (another unit graph).
fn two_members(io: &Io, edge: bool) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
    write(io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "a/src/lib.rs", "#[cfg(test)]\nmod tests;\n\npub fn f() -> u8 {\n    1\n}\n");
    write(io, "a/src/tests.rs", "#[test]\nfn one() {\n    let _wrong: u8 = true;\n}\n");
    let deps = if edge { "\n[dependencies]\na = { path = \"../a\" }\n" } else { "" };
    write(io, "b/Cargo.toml", &format!("[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{deps}"));
    write(io, "b/src/lib.rs", "pub fn h() -> u8 {\n    2\n}\n");
}

/// A MANIFEST EDIT PLANS ITS REACH TOO: from the stored answer, a new dependency edge (`b` on
/// `a`: the unit graph's digest moves, so the Units stage discovers it again and its record is
/// filed only at the merge) checks, and the answer is a fresh run's. CONTROL: the first check's
/// rows at the test build's view (the reach was planned), and the stop of
/// `engine_unit_files_reach_on_a_first_check_and_after_a_new_blaze`'s control.
#[test]
fn engine_unit_files_reach_after_a_manifest_edit() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    two_members(&io, false);
    let (_, first, checked) = said(&io, true);
    println!("first:\n{}\n{}", first.join("\n"), entries(&checked));
    assert!(view_rows(&io) > 0, "a's lib.rs is checked at its test build's view too");
    two_members(&io, true);
    let (moved_ids, moved, moved_checked) = said(&io, false);
    println!("an edge:\n{}\n{}", moved.join("\n"), entries(&moved_checked));
    assert!(moved_checked.from_last, "the edit starts from the stored answer: {}", entries(&moved_checked));
    let (fresh_ids, fresh, _) = said(&io, true);
    assert_eq!(error_lines(&moved.join("\n")).len(), 1, "the one error in a's tests.rs:\n{}", moved.join("\n"));
    assert_eq!((moved_ids, moved), (fresh_ids, fresh), "the answer is a fresh run's");
}

/// The number written right after `marker` in `line` ("view rows " then "3 checked").
fn count_after(line: &str, marker: &str) -> u32 {
    let after = line.split(marker).nth(1).unwrap_or_else(|| panic!("{marker:?} is in the line: {line}"));
    let digits: std::string::String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap_or_else(|_| panic!("a count after {marker:?}: {line}"))
}

/// The plan's counts of the last request: (rows entered, rows that checked their body, rows
/// that checked nothing, undecided at plan time, distinct bodies held).
fn plan_counts(io: &Io) -> (u32, u32, u32, u32, u32) {
    let text = profile(io);
    let line = text.lines().find(|line| line.contains(" rows entered; ")).expect("the plan's line is in the profile");
    let entered = line.split(" rows entered; ").next().and_then(|head| head.rsplit(' ').next()).and_then(|count| count.parse().ok()).unwrap_or_else(|| panic!("a count of rows entered: {line}"));
    let checked = count_after(line, "; base rows ") + count_after(line, "; view rows ");
    let nothing = count_after(line, " checked, ") + count_after(line.split("; view rows ").nth(1).unwrap_or(""), " checked, ");
    let undecided = count_after(line, "undecided at plan time: ");
    let distinct = text.lines().find(|line| line.contains("bodies (distinct): ")).expect("the distinct bodies' line is in the profile");
    (entered, checked, nothing, undecided, count_after(distinct, "bodies (distinct): "))
}

/// CFG-AWARE HOLDERS (D191: every unit that holds a body checks it, and a unit plan time knows
/// does not hold it has no row of it; engine_plan_cfg.rs): a crate with no build script whose
/// lib holds `both` and the `#[cfg(not(test))]` `only_lib`, and whose test build holds `both`
/// and the `#[cfg(test)] mod tests`'s `t`: 4 rows (`both` at each unit, `only_lib` at the lib,
/// `t` at the test build), each of which checks its body (3 bodies held); none checks nothing,
/// none is undecided. CONTROL (the cause removed: an atom plan time cannot know): a
/// `#[cfg(feature = "x")]` fn with `x` off keeps both units' rows (never a wrong `No`), each of
/// which checks nothing, counted "undecided at plan time: 2".
#[test]
fn engine_unit_files_a_unit_has_no_row_of_a_body_its_test_cuts() {
    const CUT_LIB: &str = "pub fn both() -> u8 {\n    1\n}\n\n#[cfg(not(test))]\npub fn only_lib() -> u8 {\n    2\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        let _ = super::both();\n    }\n}\n";
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    write(&io, "Cargo.toml", "[package]\nname = \"cuts\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\n");
    write(&io, "src/lib.rs", CUT_LIB);
    let (_, lines, checked) = said(&io, true);
    println!("cut:\n{}\n{}\n{}", lines.join("\n"), entries(&checked), profile(&io).lines().filter(|line| line.contains("bodies")).collect::<std::vec::Vec<_>>().join("\n"));
    assert!(error_lines(&lines.join("\n")).is_empty(), "the crate checks clean:\n{}", lines.join("\n"));
    let (entered, held_rows, nothing, undecided, held) = plan_counts(&io);
    assert_eq!(checked.stages[Stage::Bodies.index()].entered, entered, "the plan's line counts the Bodies rows entered");
    assert_eq!((entered, held_rows, held), (4, 4, 3), "D191 cfg-aware holders: every row entered checks its body (`both` twice, `only_lib` at the lib, `t` at the test build)");
    assert_eq!((nothing, undecided), (0, 0), "D191 cfg-aware holders: no row checks nothing, and plan time decided every row");

    let control = Io::simulated(SimWorld::new(11, Limits::USUAL));
    write(&control, "Cargo.toml", "[package]\nname = \"cuts\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\n");
    write(&control, "src/lib.rs", &format!("{CUT_LIB}\n#[cfg(feature = \"x\")]\npub fn gated() -> u8 {{\n    3\n}}\n"));
    let (_, control_lines, _) = said(&control, true);
    assert!(error_lines(&control_lines.join("\n")).is_empty(), "the control checks clean:\n{}", control_lines.join("\n"));
    let (entered, held_rows, nothing, undecided, held) = plan_counts(&control);
    assert_eq!((entered, held_rows, held), (6, 4, 3), "the feature-gated fn's two rows are entered and check nothing (its units' records do not hold it)");
    assert_eq!((nothing, undecided), (2, 2), "an atom plan time cannot know keeps every reacher's row, counted as undecided");
}

// ---- One file two crates name by two names (D191; turso c1814) ----

/// `a`'s `mod config;`: planned as the slot `a/src/config.rs`, read at `a/src/config/mod.rs`.
const A_SLOT: &str = "pub mod config;\n";
/// `a`'s `#[path]` to the same file: planned as `a/src/config/mod.rs` itself.
const A_PATH: &str = "#[path = \"config/mod.rs\"]\npub mod config;\n";
/// `p`'s (turso's postgres/cli/main.rs `#[path = "../../cli/config/mod.rs"] mod config;`).
const P_PATH: &str = "#[path = \"../../a/src/config/mod.rs\"]\npub mod config;\n";
/// A `mod` line under a feature that is off.
const GATED: &str = "#[cfg(feature = \"x\")]\n";

/// Members `a` and `p` (each with a feature `x`, off), `a/src/lib.rs` and `p/src/lib.rs` the
/// given texts, and the one file `a/src/config/mod.rs` whose one body is a type error.
fn named_twice(io: &Io, a_lib: &str, p_lib: &str) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"p\"]\nresolver = \"2\"\n");
    write(io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\n");
    write(io, "p/Cargo.toml", "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nx = []\n");
    write(io, "a/src/lib.rs", a_lib);
    write(io, "p/src/lib.rs", p_lib);
    write(io, "a/src/config/mod.rs", "pub fn shared() -> u8 {\n    let _wrong: u8 = true;\n    1\n}\n");
}

/// A FILE TWO CRATES NAME BY TWO NAMES IS CHECKED BY EVERY UNIT THAT HOLDS IT (D191): `a`'s
/// `mod config;` and `p`'s `#[path]` give the one file two outlines; each of the four units
/// (each crate's lib and test build) checks the body of the outline its tree reaches: 4 rows,
/// 4 checked, none checking nothing, 2 distinct bodies (one an outline), no file note, the
/// error printed in `a/src/config/mod.rs`. Before the fix (engine_bodies.rs matched an items
/// record's file row to the FIRST outline at its path) the crate whose outline was not first
/// held "nothing" there: 2 rows checked nothing and the file was noted "not part of this build"
/// (turso: `cli/config/mod.rs`, twice). TWIN: both crates name it by `#[path]`, one outline:
/// 4 rows, 4 checked, 1 distinct body, no note (this shape checked before the fix too, so the
/// two names are the trigger). CONTROL: both `mod`s cut by a feature that is off: one note (one
/// file, not one an outline) naming both `mod`s, nothing checked, no error; and with `p`'s alone
/// cut, `a` checks the file and nothing is noted (the file is not outside the build).
#[test]
fn engine_unit_files_a_file_named_twice_is_checked_by_each_holder() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    named_twice(&io, A_SLOT, P_PATH);
    let (_, lines, checked) = said(&io, true);
    let text = lines.join("\n");
    println!("named twice:\n{text}\n{}", entries(&checked));
    assert!(file_notes(&checked.bodies.0).is_empty() && !text.contains(FILE_NOTE), "the file is held by both crates: no note:\n{text}");
    let errors = error_lines(&text);
    assert!(!errors.is_empty() && errors.iter().all(|line| line.contains("a/src/config/mod.rs")), "the shared body's error, in its file:\n{text}");
    let (entered, held_rows, nothing, undecided, held) = plan_counts(&io);
    assert_eq!((entered, held_rows, nothing, undecided, held), (4, 4, 0, 0, 2), "each unit checks the outline its tree reaches");
    assert!(profile(&io).contains("; 0 file(s) no unit holds"), "the profile counts no file");

    let twin = Io::simulated(SimWorld::new(11, Limits::USUAL));
    named_twice(&twin, A_PATH, P_PATH);
    let (_, twin_lines, _) = said(&twin, true);
    assert!(!twin_lines.join("\n").contains(FILE_NOTE), "TWIN: no note:\n{}", twin_lines.join("\n"));
    assert!(!error_lines(&twin_lines.join("\n")).is_empty(), "TWIN: the error is printed");
    assert_eq!(plan_counts(&twin), (4, 4, 0, 0, 1), "TWIN: one outline, its body's 4 rows each checked");

    let cut = Io::simulated(SimWorld::new(11, Limits::USUAL));
    named_twice(&cut, &format!("{GATED}{A_SLOT}"), &format!("{GATED}{P_PATH}"));
    let (_, cut_lines, cut_checked) = said(&cut, true);
    let cut_text = cut_lines.join("\n");
    println!("both cut:\n{cut_text}");
    let notes = file_notes(&cut_checked.bodies.0);
    assert_eq!(notes.len(), 1, "CONTROL: one note for the one file: {notes:#?}");
    assert!(notes[0].contains("`a/src/config/mod.rs`") && notes[0].contains("the `mod` in `a/src/lib.rs`") && notes[0].contains("the `mod` in `p/src/lib.rs`"), "CONTROL: it names the file and both `mod`s: {}", notes[0]);
    assert_eq!(cut_lines.iter().filter(|line| line.contains(FILE_NOTE)).count(), 1, "CONTROL: printed once:\n{cut_text}");
    assert!(error_lines(&cut_text).is_empty(), "CONTROL: nothing checked, no error:\n{cut_text}");
    assert!(profile(&cut).contains("; 1 file(s) no unit holds"), "CONTROL: the profile counts the one file");

    let half = Io::simulated(SimWorld::new(11, Limits::USUAL));
    named_twice(&half, A_SLOT, &format!("{GATED}{P_PATH}"));
    let (_, half_lines, _) = said(&half, true);
    let half_text = half_lines.join("\n");
    assert!(!half_text.contains(FILE_NOTE), "CONTROL: `a` holds the file: no note:\n{half_text}");
    assert!(!error_lines(&half_text).is_empty(), "CONTROL: `a` checks it:\n{half_text}");
    assert!(profile(&half).contains("; 0 file(s) no unit holds"), "CONTROL: the profile counts no file");
}

/// THE CAUSE, ALONE (engine_body_plan.rs `outline_at_path`): of one file's two outlines (the
/// slot `a/src/config.rs` and the `#[path]` name `a/src/config/mod.rs`, both read at
/// `a/src/config/mod.rs`), each unit holds the one its tree reaches; a unit that reaches
/// neither holds the first; no outline at a path is `None`. CONTROL: the rule it replaced, the
/// first outline at the path, gives both units ONE outline, so the unit that reaches the other
/// held nothing it reached.
#[test]
fn engine_unit_files_a_unit_holds_the_outline_it_reaches() {
    use crate::engine_body_plan::outline_at_path;
    let alloc = blaze_own::testing::heap().shared();
    let outline = |name: &[u8]| {
        let file = Name::file(name);
        let mut path = blaze_engine::SVec::new_in(alloc);
        path.extend_from_slice(b"a/src/config/mod.rs");
        crate::engine_tasks::Outlined {
            name: file,
            path,
            source: ContentId::of("named twice file", name),
            outline_source: ContentId::of("named twice outline", name),
            edition: 3,
            made: Ok(crate::work::Record::new_in(file, alloc)),
        }
    };
    let mut outlined = std::vec![outline(&b"a/src/config.rs"[..]), outline(&b"a/src/config/mod.rs"[..])];
    outlined.sort_unstable_by_key(|one| one.name);
    let (first, second) = (outlined[0].name, outlined[1].name);
    let path: &[u8] = b"a/src/config/mod.rs";
    assert_eq!(outline_at_path(&outlined, path, &[first]), Some(0), "the unit reaching the first holds it");
    assert_eq!(outline_at_path(&outlined, path, &[second]), Some(1), "the unit reaching the second holds the second");
    assert_eq!(outline_at_path(&outlined, path, &[]), Some(0), "a unit reaching neither holds the first");
    assert_eq!(outline_at_path(&outlined, b"a/src/other.rs", &[first]), None, "no outline at the path");
    // The old rule read no reach: one answer for every unit.
    let old = outlined.iter().position(|one| one.path.as_slice() == path);
    assert_eq!(old, Some(0), "CONTROL: the old rule gives both units the first outline");
    assert_ne!(outline_at_path(&outlined, path, &[second]), old, "CONTROL: the unit reaching the second held it under the old rule nowhere it reached");
}
