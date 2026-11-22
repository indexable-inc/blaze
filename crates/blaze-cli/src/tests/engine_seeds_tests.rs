//! The bodies checked from the request's frozen heads (engine_seeds.rs; plan rows A1, A3;
//! runtime.md points 3 to 5): every body seeded from one table, a dependency head frozen once a
//! request and once a machine, the same bytes on any worker count, and the same answer as the
//! bodies lowering their own heads. A child of main.rs's tests: its helpers are theirs, and
//! engine_check_tests.rs's are named from there.

use super::*;

use super::engine_check_tests::{engine_from, engine_opts, entries, error_lines, opts, placed_ids, three_crates};
use crate::engine_seeds::HeadsFrom;

/// What `engine-check` listed and its counts, the bodies taking their heads `from` (rule 78's
/// seam, engine_seeds.rs `HeadsFrom`).
fn engine_heads_from(io: &Io, workers: u32, fresh: bool, from: HeadsFrom) -> (std::vec::Vec<(std::string::String, std::string::String)>, engine_check::Checked) {
    let mut opts = opts(io, workers, fresh);
    opts.heads_from = from;
    engine_opts(io, opts)
}

/// [`engine_heads_from`] with what it printed.
fn printed_heads_from(io: &Io, workers: u32, fresh: bool, from: HeadsFrom) -> (std::vec::Vec<(std::string::String, std::string::String)>, std::string::String, engine_check::Checked) {
    let before = stdout(io).len();
    let (ids, checked) = engine_heads_from(io, workers, fresh, from);
    (ids, stdout(io).split_off(before), checked)
}

/// The request's frozen heads as the profile says them (engine_seeds.rs `profile`).
pub(super) fn seeds_line(checked: &engine_check::Checked) -> std::string::String {
    let mut text = blaze_own::String::new_in(cx_frame_for_text());
    crate::engine_seeds::profile(&checked.seeds, &mut text);
    std::string::String::from(text.as_str())
}

/// A frame for a test's few lines of text (leaked, as `opts` leaks its request frame).
fn cx_frame_for_text() -> &'static bstd::mem::Frame {
    std::boxed::Box::leak(std::boxed::Box::new(bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("a text frame")))
}

/// [`three_crates`] whose bodies read the toolchain's heads too (blaze-world fixture.rs `CORE`:
/// `FixtureCell`, its constructor and its inherent `get` in `b`; `fixture_one` in `c`):
/// dependency heads, which no head task of the request freezes.
fn three_crates_reading_core(io: &Io) {
    three_crates(io);
    write(io, "b/src/lib.rs", "pub fn g() -> u8 {\n    ::core::FixtureCell(a::f() + a::m::k()).get()\n}\n");
    write(io, "c/src/lib.rs", C_READING_CORE);
}

/// `c`'s body in [`three_crates_reading_core`]: it reads one dependency head, `fixture_one`'s.
const C_READING_CORE: &str = "pub fn h() -> u8 {\n    b::g() + ::core::fixture_one()\n}\n";

/// [`C_READING_CORE`] edited in its body alone: the same heads read, its head's text another.
const C_EDITED: &str = "pub fn h() -> u8 {\n    b::g() + ::core::fixture_one() + 0\n}\n";

/// THE BODIES ARE CHECKED FROM FROZEN HEADS (plan rows A1, A3; runtime.md points 3 to 5): on
/// the three-crate chain every body entered is seeded from the request's frozen heads (the head
/// tasks' table, a dependency's head frozen on its first read), none refused at a seed, each in
/// one round; and the answer (the findings, each body's note, the error lines) is the one the
/// bodies give lowering their own heads (`HeadsFrom::Lowered`, rule 78). CONTROL: the lowered
/// run seeds no body, so the counters count the seam, not the run.
#[test]
fn engine_check_checks_bodies_from_frozen_heads() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    three_crates(&io);
    let (ids, text, frozen) = printed_heads_from(&io, 2, true, HeadsFrom::Frozen);
    println!("frozen: {}\n{}", entries(&frozen), seeds_line(&frozen));
    let bodies = frozen.stages[blaze_engine::Stage::Bodies.index()].entered;
    assert!(bodies > 0, "bodies were entered: {}", entries(&frozen));
    let seeds = frozen.seeds;
    assert_eq!(seeds.bodies.refused, 0, "no body refused at a seed: {:?}\n{}", frozen.bodies.0, seeds_line(&frozen));
    assert_eq!(seeds.bodies.refused_const, 0, "no body refused at a seed for a constant:\n{}", seeds_line(&frozen));
    assert_eq!(seeds.bodies.seeded, bodies, "every body entered was checked from frozen heads: {:?}\n{}", frozen.bodies.0, seeds_line(&frozen));
    assert_eq!(seeds.bodies.rounds, u64::from(seeds.bodies.seeded), "one round a seeded body:\n{}", seeds_line(&frozen));
    assert!(seeds.table > 0, "the head tasks' cells make the table:\n{}", seeds_line(&frozen));
    // Every member head a body reads is its head task's cell, whichever unit's names the body
    // reads it through (engine_seeds.rs `StretchHeads`): none frozen at a seed, and at least the
    // three a body calls across crates (`a::f`, `a::m::k`, `b::g`) taken from the table.
    assert_eq!(seeds.not_put.member, 0, "no member head frozen at a seed:\n{}", seeds_line(&frozen));
    assert!(seeds.taken >= 3, "the member heads the bodies call taken from the table:\n{}", seeds_line(&frozen));
    let (lowered_ids, lowered_text, lowered) = printed_heads_from(&io, 2, true, HeadsFrom::Lowered);
    println!("lowered: {}\n{}", entries(&lowered), seeds_line(&lowered));
    assert_eq!(lowered.seeds.bodies.seeded, 0, "CONTROL: the lowered run seeds no body");
    assert_eq!(lowered.stages[blaze_engine::Stage::Bodies.index()].entered, bodies, "both runs entered the same bodies");
    assert_eq!(placed_ids(&ids), placed_ids(&lowered_ids), "the findings, frozen against lowered heads");
    assert_eq!(error_lines(&text), error_lines(&lowered_text), "the error lines, frozen against lowered heads");
    assert_eq!(frozen.bodies, lowered.bodies, "each body's note and the clean count, frozen against lowered heads");
}

/// ONCE A MACHINE (plan row A3): the first request freezes each dependency head a body reads
/// and puts it under its key (the dependency's names record key, the cell, the tool); after
/// an edit of one body, the second request freezes none and finds them in the store. CONTROL:
/// the first request found none (the store held none) and froze at least one.
#[test]
fn a_dependency_head_is_frozen_once_a_machine() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    three_crates_reading_core(&io);
    let (_, first) = engine_from(&io, 2, false);
    println!("first: {}\n{}", entries(&first), seeds_line(&first));
    assert_eq!(first.seeds.bodies.refused, 0, "no body refused at a seed: {:?}", first.bodies.0);
    assert_eq!(first.seeds.found, 0, "CONTROL: an empty store holds no dependency head:\n{}", seeds_line(&first));
    assert_eq!(first.seeds.not_put.member, 0, "no member head frozen at a seed:\n{}", seeds_line(&first));
    assert!(first.seeds.read > 0, "the bodies read toolchain heads:\n{}", seeds_line(&first));
    assert!(first.seeds.frozen > 0 && first.seeds.put == first.seeds.frozen, "each dependency head was frozen and put:\n{}", seeds_line(&first));
    write(&io, "c/src/lib.rs", C_EDITED);
    let (_, second) = engine_from(&io, 2, false);
    println!("second: {}\n{}", entries(&second), seeds_line(&second));
    assert!(second.stages[blaze_engine::Stage::Bodies.index()].entered >= 1, "the edited body entered: {}", entries(&second));
    assert_eq!(second.seeds.bodies.refused, 0, "no body refused at a seed: {:?}", second.bodies.0);
    assert_eq!(second.seeds.frozen, 0, "no dependency head frozen again:\n{}", seeds_line(&second));
    assert!(second.seeds.found > 0, "the dependency heads were found in the store:\n{}", seeds_line(&second));
    assert_eq!(second.seeds.found, second.seeds.read, "every head read outside the table was found:\n{}", seeds_line(&second));
}

/// READ ON FIRST ASK (runtime.md end state 5): the request's table reads no head record when it
/// is made; a worker reads the dependency heads its body asks, in place through the store's
/// snapshot. After an edit of `c`'s body alone, the one body entered reads one dependency head
/// record (`fixture_one`'s), though the root lists every head the first request put.
/// CONTROL: the root lists more than one (the first request put `b`'s too), so a table that
/// read every listed record would read more than one.
#[test]
fn a_request_reads_only_the_dependency_heads_its_bodies_ask() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    three_crates_reading_core(&io);
    let (_, first) = engine_from(&io, 2, false);
    println!("first: {}\n{}", entries(&first), seeds_line(&first));
    assert_eq!(first.seeds.bodies.refused, 0, "no body refused at a seed: {:?}", first.bodies.0);
    assert!(first.seeds.put > 1, "CONTROL: the first request put more than one dependency head:\n{}", seeds_line(&first));
    write(&io, "c/src/lib.rs", C_EDITED);
    let (_, second) = engine_from(&io, 2, false);
    println!("second: {}\n{}", entries(&second), seeds_line(&second));
    // D191: `c::h` is held by `c`'s library and its test build, a row each; the head record
    // both rows ask is still read once (the request's table, rule 77).
    assert_eq!(second.stages[blaze_engine::Stage::Bodies.index()].entered, 2, "one body entered, `c::h`'s, in its 2 units' rows (D191): {}", entries(&second));
    assert_eq!(second.seeds.bodies.refused, 0, "no body refused at a seed: {:?}", second.bodies.0);
    assert_eq!(second.seeds.listed, first.seeds.put, "the root lists each head the first request put:\n{}", seeds_line(&second));
    assert_eq!(second.seeds.looked, 1, "one dependency head record read, the one `c::h` asks:\n{}", seeds_line(&second));
    assert_eq!((second.seeds.found, second.seeds.frozen), (1, 0), "found in the store, frozen again none:\n{}", seeds_line(&second));
}

/// EQUAL BYTES WHOEVER FREEZES (rule 25; plan point 5): with 1 and with 8 workers, on two
/// machines alike, the heads frozen now are the same cells with the same bytes (one digest over
/// each cell's bytes and its frozen bytes' digest), no freeze made other bytes (no body refused
/// at a seed: `SeedError::Unequal` would refuse it), and every dependency head read was either
/// found in the store or frozen now, once.
#[test]
fn frozen_heads_are_equal_bytes_on_any_worker_count() {
    let mut got = std::vec::Vec::new();
    for workers in [1u32, 8] {
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        three_crates_reading_core(&io);
        let (_, checked) = engine_from(&io, workers, true);
        let seeds = checked.seeds;
        println!("{workers} worker(s): {}\n{}", entries(&checked), seeds_line(&checked));
        assert_eq!(seeds.bodies.refused, 0, "{workers} worker(s): no body refused at a seed: {:?}", checked.bodies.0);
        assert!(seeds.frozen > 0, "{workers} worker(s): a dependency head was frozen now:\n{}", seeds_line(&checked));
        assert_eq!(seeds.frozen + seeds.found, seeds.read, "{workers} worker(s): every dependency head read was found or frozen, once:\n{}", seeds_line(&checked));
        got.push((seeds.frozen, seeds.fresh));
    }
    assert_eq!(got[0], got[1], "1 and 8 workers froze the same heads to the same bytes");
}
