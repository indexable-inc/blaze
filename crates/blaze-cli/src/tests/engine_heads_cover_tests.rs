//! A DEPENDENCY'S HEAD IS READ AS ITS COVER (engine_heads_once.rs `Cover`, `coarse`; lane H8).
//! A child of main.rs's tests, over engine_check_tests.rs's helpers.
//!
//! THE CAUSE IT REMOVES (tempo, 2026-11-18): the Heads stage ended holding 46.6 GB of heap. Each
//! head task kept every names cell of `core` and `std` that its heads, and the heads they read,
//! were frozen from: about ten thousand cells a task over 20,183 tasks, each list kept three
//! times until its row. A dependency's names read is now its record's `WHOLE`, and a read of a
//! dependency's head the `WHOLE` cells of the crates it was made from, once a request.
//!
//! THE TESTS: (1) the grain, by value, with its control (a member's cell is kept as it is);
//! (2) THE RACE (rule 43, `blaze_sim`): two workers that both miss a cover and make it,
//! interleaved every way the tape draws: one cover kept, the first's, each worker handed it;
//! CONTROL: the planted "second write wins" (`OncePlant::SecondWins`) is caught; (3) THE SEAM:
//! two members reading one dependency head (the fixture core's `FixtureCell`) under their two units: ONE entry
//! of it, the answer a fresh check's, the table's counts adding up.

use super::engine_check_tests::{engine_opts, entries, opts};
use super::*;

use crate::engine_heads_once::{Cover, HeadsOnce, OncePlant, Sources, coarse};
use crate::engine_tasks::WHOLE;
use blaze_drive::Kind;
use blaze_drive::cells::Cell;
use blaze_engine::CellId;
use blaze_ladder::front2::heads::Want;
use blaze_sim::{Broke, Config, Tape, check as sim_check, control};

/// The request's heap, by the helpers' own form (engine_check_tests.rs `opts`).
fn heap() -> &'static blaze_own::Heap {
    let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
    opts(&io, 2, false).heap
}

fn scope(name: &str) -> blaze_drive::Name {
    blaze_drive::Name::new(Kind::CrateScope, blaze_ids::ContentId::of("blaze-cli heads cover test", name.as_bytes()))
}

/// Expected: passes; a dependency's names cell becomes its record's `WHOLE`, every other is kept.
#[test]
fn a_dependency_names_read_is_kept_at_its_record_whole() {
    let mut members = [scope("member a"), scope("member b")];
    members.sort_unstable();
    let fine = Cell([7; 16]);
    let dependency = CellId { of: scope("core"), cell: fine };
    assert_eq!(coarse(&members, dependency), CellId { of: scope("core"), cell: WHOLE }, "ORACLE: a dependency's names cell was kept at its own grain");
    // CONTROL: a member's cell, a `WHOLE` and a cell of another kind are kept as they are.
    let member = CellId { of: scope("member a"), cell: fine };
    assert_eq!(coarse(&members, member), member, "CONTROL: a member's names cell was coarsened");
    let whole = CellId { of: scope("core"), cell: WHOLE };
    assert_eq!(coarse(&members, whole), whole);
    let other = CellId { of: blaze_drive::Name::new(Kind::Signature, blaze_ids::ContentId::of("blaze-cli heads cover test", b"a head")), cell: fine };
    assert_eq!(coarse(&members, other), other, "a head's cell is no names read: kept");
    // Two cells of one dependency are one read.
    let second = CellId { of: scope("core"), cell: Cell([9; 16]) };
    assert_eq!(coarse(&members, dependency), coarse(&members, second), "ORACLE: two cells of one dependency kept apart");
}

// THE RACE.

fn cell() -> Want {
    Want::Head(blaze_id::ItemKey(blaze_ids::ContentId::of("blaze-cli heads cover race", b"the cell")))
}

/// `names`' `WHOLE` cells lent in `heap` until the test run ends.
fn cover_of(heap: &'static blaze_own::Heap, names: &[&str]) -> Cover {
    let mut cells: blaze_own::Vec<CellId, blaze_own::Alloc<'static>> = blaze_own::Vec::with_capacity_in(names.len(), heap.alloc());
    cells.extend(names.iter().map(|n| CellId { of: scope(n), cell: WHOLE }));
    cells.sort_unstable();
    let members: blaze_own::Vec<Want, blaze_own::Alloc<'static>> = blaze_own::Vec::new_in(heap.alloc());
    Cover { cells: cells.leak_static(), members: members.leak_static() }
}

/// Two workers, each a look then (on a miss) a put of its cover, interleaved as the tape draws;
/// the second's cells equal or other. The oracle over the table after both, and over what each
/// worker was handed.
fn race(tape: &Tape, plant: OncePlant, heap: &'static blaze_own::Heap) -> Result<(), Broke> {
    let table = HeadsOnce::new(heap, plant, Sources::Job);
    let other = tape.bool("the second cover holds other cells");
    let made = [cover_of(heap, &["core", "alloc"]), if other { cover_of(heap, &["core"]) } else { cover_of(heap, &["core", "alloc"]) }];
    let mut step = [0u8; 2];
    let mut handed: [Option<Cover>; 2] = [None, None];
    let mut first: Option<usize> = None;
    while step != [2, 2] {
        let w = match (step[0] < 2, step[1] < 2) {
            (true, true) => usize::from(tape.bool("the second worker steps now")),
            (true, false) => 0,
            _ => 1,
        };
        match step[w] {
            0 => {
                handed[w] = table.cover_of(cell());
                step[w] = if handed[w].is_some() { 2 } else { 1 };
            }
            _ => {
                let put = table.put_cover(cell(), made[w]).map_err(|e| Broke::Harness(std::format!("the put was refused: {e}")))?;
                first.get_or_insert(w);
                handed[w] = Some(put);
                step[w] = 2;
            }
        }
    }
    let oracle = |what: std::string::String| Err(Broke::Oracle(what));
    let Some(first) = first else {
        return Err(Broke::Harness(std::string::String::from("no worker put")));
    };
    let counts = table.counts();
    if counts.covers != 1 {
        return oracle(std::format!("the table holds {} covers of one cell", counts.covers));
    }
    if table.cover_of(cell()) != Some(made[first]) {
        return oracle(std::string::String::from("the table holds another cover than the first's"));
    }
    if handed.iter().any(|one| *one != Some(made[first])) {
        return oracle(std::string::String::from("a worker was handed another cover than the table's first"));
    }
    Ok(())
}

/// Expected: passes over every interleaving the tape draws.
#[test]
fn two_covers_of_one_dependency_head_leave_the_first() {
    let heap = heap();
    sim_check(Config::cases(500), |tape| race(tape, OncePlant::Rule, heap));
}

/// CONTROL (rule 23): a second cover that wins is caught by the same oracle.
#[test]
fn the_cover_race_oracle_catches_a_second_write_that_wins() {
    let heap = heap();
    control(Config::cases(2000), OncePlant::Rule, OncePlant::SecondWins, "another cover than the first's", |tape, plant| race(tape, plant, heap));
}

// THE SEAM.

const WORKSPACE: &str = "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n";
const A_MANIFEST: &str = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const B_MANIFEST: &str = "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n";
// The dependency head both members read is the fixture core's `FixtureCell` (blaze-world
// fixture.rs `CORE`). Lane H9 found why this seam froze no dependency head before: that core
// declares no `Option` and its prelude is empty, so `Option<u32>` resolved nowhere, every
// member's head failed in the job's source and was frozen alone (`alone 4`), and no dependency
// head reached the table. The members list was right: `core` is no member.
const A_LIB: &str = "pub struct Point {\n    pub x: ::core::FixtureCell,\n}\n\npub fn get(p: ::core::FixtureCell) -> u8 {\n    p.0\n}\n";
const B_LIB: &str = "pub struct Holder {\n    pub p: a::Point,\n    pub q: ::core::FixtureCell,\n}\n\npub fn first(h: ::core::FixtureCell) -> ::core::FixtureCell {\n    h\n}\n";

/// Expected line: `heads cover seam: cells C (member M, dependency D), frozen F, twice T, found
/// N, covers K, bytes held B; reads a head task: mean R, most X; ...`.
#[test]
fn a_dependency_head_read_under_two_units_is_one_entry() {
    let io = Io::simulated(SimWorld::new(29, Limits::USUAL));
    write(&io, "Cargo.toml", WORKSPACE);
    write(&io, "a/Cargo.toml", A_MANIFEST);
    write(&io, "a/src/lib.rs", A_LIB);
    write(&io, "b/Cargo.toml", B_MANIFEST);
    write(&io, "b/src/lib.rs", B_LIB);
    let (ids, checked) = engine_opts(&io, opts(&io, 2, false));
    let (fresh_ids, fresh) = engine_opts(&io, opts(&io, 2, true));
    assert_eq!(ids, fresh_ids, "the answer is not a fresh check's");
    assert_eq!(checked.status.0, fresh.status.0, "the status is not a fresh check's");
    let o = checked.seeds.once;
    println!(
        "heads cover seam: cells {} (member {}, dependency {}), frozen {}, twice {}, found {}, covers {}, bytes held {}; reads a head task: mean {}, most {}; {}",
        o.cells,
        o.members,
        o.dependencies,
        o.frozen,
        o.twice,
        o.found,
        o.covers,
        o.bytes_held,
        o.read_mean(),
        o.read_most,
        entries(&checked)
    );
    assert!(o.dependencies > 0, "PRECONDITION: no dependency head was frozen (the fixture core's `FixtureCell`): {o:?}");
    assert_eq!(o.alone, 0, "PRECONDITION: a member's head did not freeze in its job's source: {o:?}");
    assert_eq!(o.members + o.dependencies, o.cells, "ORACLE: the table's entries are not each a member's or a dependency's: {o:?}");
    // ONE entry a cell, whichever unit read it first: a second freeze only by a race, counted.
    assert_eq!(o.frozen, o.cells + o.twice, "ORACLE: a cell was frozen more than once outside a race (a dependency head kept once a reading unit?): {o:?}");
    assert_eq!(o.covers_unequal, 0, "ORACLE: two covers of one dependency head held other cells: {o:?}");
    // `core` is CLOSED (lane H9, engine_seeds.rs `CrateAt::closed`): with the dependency finder
    // set, its head is read as `core`'s `WHOLE` alone and found or frozen by key, no cover made;
    // without it, by its cover.
    assert!(o.covers + o.deps_found + o.deps_frozen > 0, "ORACLE: a head read `FixtureCell` and neither a cover nor its key was made: {o:?}");
}
