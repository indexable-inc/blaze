//! A BODY NEVER FREEZES A MEMBER'S HEAD (lanes RU1, RU2; engine_seeds.rs `StretchHeads`): a
//! member crate's head is its head task's run for the reader's unit, or the body is refused,
//! typed. A child of main.rs's tests, over the fixtures and helpers of its siblings.
//!
//! THE INVARIANT (rule 53), stated once: on a FRESH request of a workspace that checks clean,
//! - no member crate's head is frozen at a seed (the count "frozen now and not put: N a member
//!   crate's" is 0): a macro-written item's head is its entry's head task's too (lane RH4,
//!   engine_heads.rs "WRITTEN HEADS"; before it the fixtures `module-facts` and `unnamed-calls`
//!   counted 1 and 2 here), and
//! - no body is refused at a seed (no member head refused: none of another unit, none with no
//!   cell, none its head task failed),
//!
//! on 1 and 8 workers alike (rule 25). And incrementally (lane RU2's find A): a `#[cfg(test)]`
//! taken off a `mod`, then put back, answers as a fresh run and refuses no body at a seed, the
//! head task of the moved file reading which units hold it (engine_body_plan.rs
//! `not_held_cell`, engine_heads.rs `run_items`).

use super::engine_check_tests::{engine_opts, entries, opts, three_crates};
use super::engine_seeds_tests::seeds_line;
use super::*;

/// The ids a request listed, by the helpers' own form (engine_check_tests.rs `engine_opts`).
type Ids = std::vec::Vec<(std::string::String, std::string::String)>;

/// The words of a body refused for a member head (engine_seeds.rs `other_units`, `no_cell`,
/// `could_not_freeze`): none is printed on a workspace that checks clean.
const REFUSED: [&str; 2] = ["and not under this body's unit", "a body never freezes a member's head"];

/// One request on `workers`: its ids, what it printed and its counts.
fn run(io: &Io, workers: u32, fresh: bool) -> (Ids, std::string::String, engine_check::Checked) {
    let before = stdout(io).len();
    let (ids, checked) = engine_opts(io, opts(io, workers, fresh));
    (ids, stdout(io).split_off(before), checked)
}

/// One fixture of the invariant: its name and how it is written.
struct Fixture {
    name: &'static str,
    write: fn(&Io),
}

/// The words of a head task refused whole for two heads under one cell (engine_heads.rs
/// `heads_of`): never printed (lane RU3: before signature.rs `signature_name` held an unnamed
/// entry's head, `meters`' two impls of `src/a.rs` were one entry and printed it).
const UNDER_ONE_CELL: &str = "fall under one cell";

/// The head tasks' closing lines (engine_head_notes.rs: "heads: N head(s) failed: <why>: first ..").
const HEADS_FAILED: &str = "head(s) failed: ";

/// THE HEAD FAILURES ARE THE DECLARED ONES (lane RU3): every "heads: N head(s) failed" line of
/// `text` carries one of the reasons `declared` (a fixture that plants a failing head declares
/// its reason; a clean one declares none), each declared reason is printed, and no head task
/// failed for two heads under one cell.
fn failures_declared(what: &str, text: &str, declared: &[&str]) {
    for line in text.lines().filter(|line| line.contains(HEADS_FAILED)) {
        assert!(!line.contains(UNDER_ONE_CELL), "{what}: no head task holds two heads under one cell (signature.rs `signature_name`): {line}\n{text}");
        assert!(declared.iter().any(|reason| line.contains(reason)), "{what}: a head failed that the fixture does not declare (declared: {declared:?}): {line}\n{text}");
    }
    for reason in declared.iter() {
        assert!(text.lines().any(|line| line.contains(HEADS_FAILED) && line.contains(reason)), "{what}: the declared head failure {reason:?} is printed:\n{text}");
    }
}

/// `edit_rows`' three-member workspace (engine_edit_rows_tests.rs `world`) with R11's file: two
/// impls of `Measure` in `b/src/wrap.rs` (two unnamed entries of one file, as `meters`' two
/// impls), written into `io`.
fn edit_rows(io: &Io) {
    use super::engine_edit_rows_tests as rows;
    write(io, "Cargo.toml", rows::WORKSPACE);
    write(io, "a/Cargo.toml", rows::A_MANIFEST);
    write(io, "a/src/lib.rs", rows::A_LIB);
    write(io, "a/src/shapes.rs", rows::A_SHAPES);
    write(io, "b/Cargo.toml", rows::B_MANIFEST);
    write(io, "b/src/lib.rs", rows::B_LIB);
    write(io, "b/src/wrap.rs", rows::B_WRAP_IMPL);
    write(io, "c/Cargo.toml", rows::C_MANIFEST);
    write(io, "c/src/lib.rs", rows::C_LIB);
    write(io, "c/src/use_b.rs", rows::C_MODULE);
}

fn meters(io: &Io) {
    super::engine_scope_reads_tests::meters(io, super::engine_scope_reads_tests::D);
}

fn unit_files(io: &Io) {
    super::engine_unit_files_tests::units_fixture(io, false, false);
}

fn unnamed_calls(io: &Io) {
    write(io, "Cargo.toml", MANIFEST);
    write(io, "src/lib.rs", &super::engine_unnamed_calls_tests::two_calls(false, false));
}

const FIXTURES: [Fixture; 7] = [
    Fixture { name: "three_crates", write: three_crates },
    // `#[macro_use]` on `a`, but no macro: every head is an outline entry's.
    Fixture { name: "scope-reads (meters)", write: meters },
    Fixture { name: "unit-files", write: unit_files },
    // `mac`, written by `mk!()` in `app`: its head is the head task's of the `mk!()` entry.
    Fixture { name: "module-facts", write: super::engine_module_facts_tests::three_members },
    // `p` and `q`, written by `write_p!()` and `write_q!()`: each call's entry's.
    Fixture { name: "unnamed-calls", write: unnamed_calls },
    // Two impls of one trait in one file (lane RU3).
    Fixture { name: "edit-rows (R11's two impls)", write: edit_rows },
    // A toolchain head with a constant, frozen at the body's seed: not a member's.
    Fixture { name: "refusal-causes (toolchain constant)", write: super::engine_refusal_causes_tests::toolchain_constant },
];

/// The invariant on one answer (module docs).
fn holds(what: &str, text: &str, checked: &engine_check::Checked) {
    let seeds = &checked.seeds;
    println!("{what}: member counter {}; {}\n{}", seeds.not_put.member, entries(checked), seeds_line(checked));
    // Before any assertion, on any sign of a refusal: EVERY body's note (a refused body's
    // names the head, its kind, the reader's unit and the reason: `NotTheirs`' words, or the
    // head's own freeze error) and everything the request printed (each head task's notes,
    // one a head it could not freeze, with its reason: engine_head_notes.rs).
    if seeds.bodies.refused > 0 || seeds.not_put.member != 0 || checked.status.0 == Status::FAILED.0 || REFUSED.iter().any(|words| text.contains(words)) {
        println!("{what}: {} body note(s), {} clean:", checked.bodies.0.len(), checked.bodies.1);
        for (at, note) in checked.bodies.0.iter().enumerate() {
            println!("{what}: note {at}: {note}");
        }
        println!("{what}: printed:\n{text}");
    }
    assert_ne!(checked.status.0, Status::FAILED.0, "{what}: the fixture checks clean, so the request does not fail:\n{text}");
    // A clean fixture declares no failing head (lane RU3).
    failures_declared(what, text, &[]);
    for words in REFUSED.iter() {
        assert!(!text.contains(words), "{what}: no body refused for a member head ({words:?}):\n{text}");
    }
    assert_eq!(seeds.bodies.refused, 0, "{what}: no body refused at a seed:\n{}", seeds_line(checked));
    assert_eq!(seeds.not_put.member, 0, "{what}: no member crate's head frozen at a seed, a macro-written one's neither:\n{}", seeds_line(checked));
}

/// THE INVARIANT on every fixture, fresh, on 1 and 8 workers; the heads line and the member
/// counter printed for each. CONTROL (rule 23): on RU1's tree (before RU2) `meters` failed cold
/// with no finding line (engine_scope_reads_tests.rs `checks_clean`, seven cases), and the
/// typed crates' body whose member head failed in its head task was FAILED, not unchecked.
#[test]
fn engine_member_heads_counted_on_fresh_requests() {
    for fixture in FIXTURES.iter() {
        for workers in [1u32, 8] {
            let io = Io::simulated(SimWorld::new(23, Limits::USUAL));
            (fixture.write)(&io);
            let (_, text, checked) = run(&io, workers, true);
            holds(&format!("{}, {workers} worker(s)", fixture.name), &text, &checked);
        }
    }
}

/// A PLANTED HEAD FAILURE IS PRINTED AS DECLARED, AND NO OTHER (lane RU3): `again`'s `bad` holds
/// a constant with no value (engine_refusal_causes_tests.rs `asked_twice`), so its head task
/// fails with the constant's reason (engine_head_notes.rs `Reason`); no other head fails, and
/// none for two heads under one cell. On 1 and 8 workers. CONTROL: the clean fixtures above
/// declare none, so a head failing there (as `meters`' did before signature.rs held an unnamed
/// entry's head) fails them.
#[test]
fn engine_member_heads_failures_are_the_declared_ones() {
    for (workers, what) in [(1u32, "asked-twice, 1 worker(s)"), (8, "asked-twice, 8 worker(s)")] {
        let io = Io::simulated(SimWorld::new(59, Limits::USUAL));
        super::engine_refusal_causes_tests::asked_twice(&io);
        let (_, text, checked) = run(&io, workers, true);
        println!("{what}: {}\n{text}", entries(&checked));
        failures_declared(what, &text, &["the value of a constant its type holds"]);
    }
}

/// FIND A, BOTH DIRECTIONS: from the stored answer, `#[cfg(test)]` taken off `mod tests;` makes
/// the lib hold `src/tests.rs`, and the head task of `helper` must freeze it under the lib too
/// (its row reads the lib's not-held cell, which moved); put back, the lib stops holding it.
/// Each incremental answer is a fresh run's, no body is refused at a seed, and the head task of
/// the moved file is entered again. On 1 and 8 workers. CONTROL: on RU1's tree the first
/// direction refused `helper` with "frozen by its head task under unit(s) .. and not under this
/// body's unit .." (engine_unit_files_tests.rs `engine_unit_files_follow_a_cfg_taken_off`).
#[test]
fn engine_member_heads_follow_a_cfg_off_and_back() {
    for workers in [1u32, 8] {
        for (from, to, what) in [(false, true, "cfg taken off"), (true, false, "cfg put back")] {
            let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
            super::engine_unit_files_tests::units_fixture(&io, false, from);
            let (_, cold_text, cold) = run(&io, workers, true);
            holds(&format!("{what}, cold, {workers} worker(s)"), &cold_text, &cold);
            super::engine_unit_files_tests::units_fixture(&io, false, to);
            let (moved_ids, moved_text, moved) = run(&io, workers, false);
            let (fresh_ids, fresh_text, fresh) = run(&io, workers, true);
            let what = format!("{what}, {workers} worker(s)");
            holds(&format!("{what}, incremental"), &moved_text, &moved);
            holds(&format!("{what}, fresh"), &fresh_text, &fresh);
            assert!(moved.stages[blaze_engine::Stage::Heads.index()].entered > 0, "{what}: the moved file's head tasks are entered again: {}", entries(&moved));
            assert_eq!((&moved_ids, moved.status.0), (&fresh_ids, fresh.status.0), "{what}: the incremental answer is a fresh run's: {}", entries(&moved));
            // Without the closing line, as engine_check_tests.rs `said` compares two answers.
            let body = |text: &str| -> std::vec::Vec<std::string::String> {
                let mut lines: std::vec::Vec<std::string::String> = text.lines().map(str::to_owned).collect();
                lines.pop();
                lines
            };
            assert_eq!(body(&moved_text), body(&fresh_text), "{what}: it prints what a fresh run prints");
        }
    }
}
