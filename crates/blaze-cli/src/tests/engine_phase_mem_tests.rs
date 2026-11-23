//! A phase's memory ends with the phase (engine_phase_mem.rs): each phase's arena is made before
//! its tasks and dropped once its records are committed, on a stop and a panic too; the profile
//! says, a line a stage, the arena's most, what its drop gave back and what the heap kept past
//! the phase; a 10 MB leak planted in one stage's phase is that stage's "kept past the phase" and
//! no other's (rule 23's control: unplanted, it is gone); and the answer is the same bytes with
//! the leak or without.
//!
//! The rule that nothing made over a phase's handle outlives the phase is a compile error, not a
//! test: `PhaseMem::shared` borrows the arena and `PhaseMem::end` moves it, so
//!
//! ```text
//! let mem = request.phase(Stage::Names)?;
//! let mut kept = blaze_own::Vec::new_in(mem.shared());
//! kept.push(1u8);
//! let row = mem.end();   // E0505: cannot move out of `mem` because it is borrowed
//! kept.len();            //        ...borrow later used here
//! ```
//!
//! is refused. The crate is a binary with `doctest = false`, so no `compile_fail` doctest holds it.
//!
//! The heap's growth is the process's (blaze-mem's counted global heap): another test running
//! at the same time adds to it. Run these alone: `-p blaze-cli --bin blaze -- engine_phase_mem
//! --test-threads=1`.

use super::*;


use blaze_own::FixedString;

use super::engine_status_tests::{checked, six_bodies, with_profile};
use crate::engine_phase_mem::{PhaseLedger, RequestMem, last_request, with_planted_leak};
use crate::engine_status::{Asking, Plant};
use blaze_engine::Stage;
use bstd::procs::By;

const MB: u64 = 1 << 20;

/// The planted leak's bytes: 32 MB (large enough to stay well above concurrent test heap noise).
const LEAK: usize = 32 << 20;

/// The number after `label` in `line`.
fn number_after(line: &str, label: &str) -> i64 {
    let Some((_, rest)) = line.split_once(label) else { panic!("`{label}` in `{line}`") };
    let digits: &str = rest.trim_start().split(|c: char| c != '-' && !c.is_ascii_digit()).next().unwrap_or("");
    digits.parse::<i64>().unwrap_or_else(|_| panic!("a number after `{label}` in `{line}`"))
}

/// (a) One arena: what it committed is its most and its drop gives it back; nothing live at its
/// end; dropped without `end` (a stop's return) or by an unwind (a panic), it is counted dropped
/// all the same.
#[test]
fn a_phase_arena_is_made_and_dropped() {
    let request = RequestMem::new().expect("the request's arena");
    let mem = request.phase(Stage::Names).expect("a phase's arena");
    let mut scratch = blaze_own::Vec::new_in(mem.shared());
    scratch.try_reserve_exact(1 << 20).expect("a megabyte of the arena");
    scratch.resize(1 << 20, 7u8);
    drop(scratch);
    let row = mem.end();
    assert_eq!(row.stage, Stage::Names);
    assert!(row.arena_most >= MB, "the arena committed its megabyte: {row:?}");
    assert_eq!(row.returned, row.arena_most, "its drop gave back all it committed: {row:?}");
    assert_eq!(row.live_at_end, 0, "nothing live at its end: {row:?}");
    let counts = request.counts();
    assert_eq!((counts.made, counts.dropped), (1, 1), "one made, one dropped");
    assert!(counts.returned >= MB, "the drop's bytes counted: {counts:?}");
    // A stop: the arena dropped by a return, never ended.
    {
        let _stopped = request.phase(Stage::Bodies).expect("a phase's arena");
    }
    assert_eq!((request.counts().made, request.counts().dropped), (2, 2), "dropped by the stop's return");
    // A panic in the phase: the unwind drops it (rule 20).
    let unwound = std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| {
        let mem = request.phase(Stage::Heads).expect("a phase's arena");
        let mut held = blaze_own::Vec::new_in(mem.shared());
        held.push(1u8);
        panic!("a planted panic in a phase");
    }));
    assert!(unwound.is_err(), "the planted panic unwound");
    assert_eq!((request.counts().made, request.counts().dropped), (3, 3), "dropped by the unwind");
    // The ledger sums a stage's phases: the most of any one, the sums of the rest.
    let mut ledger = PhaseLedger::new();
    let first = ledger.add(row);
    assert_eq!((first.phases, first.arena_most, first.returned), (1, row.arena_most, row.returned));
    let second = ledger.add(row);
    assert_eq!((second.phases, second.arena_most, second.returned), (2, row.arena_most, 2 * row.returned));
}

/// (b) A check: each phase run had its arena made and dropped (as many as the phases the engine
/// ran), the stages' phases sum to them, and the request's drop saw the same; the profile has
/// one memory line a stage that ran a phase, each with its three numbers.
#[test]
fn every_phase_of_a_check_makes_and_drops_its_arena_and_says_so() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    six_bodies(&io);
    let checked = checked(&io, Asking::NONE).unwrap_or_else(|error| panic!("the check answers: {error:?}"));
    let ran = u32::try_from(checked.phases.len()).expect("a count");
    let ledger = checked.phase_mem;
    assert!(ran >= 4, "the check ran its phases: {ran}");
    assert_eq!((ledger.counts.made, ledger.counts.dropped), (ran, ran), "one arena a phase, each dropped: {ledger:?}");
    let summed: u32 = ledger.stages.iter().map(|stage| stage.phases).sum();
    assert_eq!(summed, ran, "every phase counted under its stage: {ledger:?}");
    let last = last_request().expect("the request's drop said what its phases did");
    assert_eq!((last.made, last.dropped), (ran, ran), "the request's drop saw every arena dropped");
    for stage in Stage::ALL {
        assert_eq!(ledger.stages[stage.index()].live_at_end, 0, "{stage:?}: nothing live at a phase's end");
    }
    with_profile(&io, |text| {
        for stage in Stage::ALL {
            let at = ledger.stages[stage.index()];
            let mut head = FixedString::<48>::new();
            write!(head, "memory ({stage:?}): arena most ").expect("a short head");
            let line = text.lines().find(|line| line.starts_with(head.as_str()));
            if at.phases == 0 {
                assert!(line.is_none(), "{stage:?} ran no phase and has no line:\n{text}");
                continue;
            }
            let Some(line) = line else { panic!("a phase memory line for {stage:?}:\n{text}") };
            let bytes = line.split_once("in bytes:").expect("the bytes part").1;
            let most = number_after(bytes, "arena most");
            let returned = number_after(bytes, "returned");
            let kept = number_after(bytes, "kept past the phase on the heap");
            let ledger_says = (i64::try_from(at.arena_most).expect("bytes"), i64::try_from(at.returned).expect("bytes"), at.kept_on_heap);
            assert_eq!((most, returned, kept), ledger_says, "{stage:?}: the line says the ledger's three numbers: {line}");
            assert!(line.contains("(floor 0)"), "the floor is said: {line}");
        }
        let Some(arenas) = text.lines().find(|line| line.starts_with("phase arenas: ")) else { panic!("the profile counts the arenas:\n{text}") };
        assert_eq!(number_after(arenas, "phase arenas:"), i64::from(ran), "made: {arenas}");
        assert_eq!(number_after(arenas, "made,"), i64::from(ran), "dropped: {arenas}");
    });
}

/// (c) A stop: the stopped phase's arena is dropped by the stop's return, and every one before
/// it ended; the request's drop saw as many dropped as made.
#[test]
fn a_stop_drops_the_phase_arena() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    six_bodies(&io);
    let plant = Plant::StopAfter { stage: Stage::Bodies, done: 2, by: By::Terminate };
    match checked(&io, Asking { requests: None, every: None, plant }) {
        Err(CliError::EngineCheck(engine_check::EngineCheckError::Stopped { stage: Some(Stage::Bodies), .. })) => {}
        Err(other) => panic!("the planted stop is the typed stop in Bodies: {other:?}"),
        Ok(_) => panic!("the planted stop answered"),
    }
    let last = last_request().expect("the request's drop said what its phases did");
    assert!(last.made >= 1, "the Bodies phase's arena was made before the stop: {last:?}");
    assert_eq!(last.dropped, last.made, "the stopped phase's arena dropped too: {last:?}");
    with_profile(&io, |text| {
        assert!(text.contains("memory (Names): arena most "), "the phases before the stop said their memory:\n{text}");
    });
}

/// (d) Rule 23: 10 MB planted on the heap in each Names phase and kept to the command's end is
/// the Names stage's "kept past the phase" and no other stage's; CONTROL: unplanted, it is gone.
/// The answers are the same bytes either way (the records' digest, the status, the notes).
#[test]
fn a_planted_heap_leak_is_its_stages_kept_and_no_others() {
    let run = |planted: bool| {
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        six_bodies(&io);
        let checked = match planted {
            true => with_planted_leak(Stage::Names, LEAK, || checked(&io, Asking::NONE)),
            false => checked(&io, Asking::NONE),
        };
        checked.unwrap_or_else(|error| panic!("the check answers: {error:?}"))
    };
    let control = run(false);
    let planted = run(true);
    let names = Stage::Names.index();
    let phases = i64::from(planted.phase_mem.stages[names].phases);
    assert!(phases >= 1, "the Names stage ran a phase");
    let leak = i64::try_from(LEAK).expect("a size");
    let grew = planted.phase_mem.stages[names].kept_on_heap - control.phase_mem.stages[names].kept_on_heap;
    // The heap's counter folds each thread's count in steps (blaze-mem counted.rs `BATCH`): a
    // tenth of the leak is slack enough.
    assert!(grew >= phases * leak - leak / 10, "the leak is the Names stage's kept: {grew} bytes more than the control, {phases} phase(s) of {leak}");
    assert!(planted.phase_mem.stages[names].kept_on_heap >= phases * leak - leak / 10, "and over the floor by the leak: {:?}", planted.phase_mem.stages[names]);
    for stage in Stage::ALL {
        if stage == Stage::Names {
            continue;
        }
        let at = stage.index();
        let moved = (planted.phase_mem.stages[at].kept_on_heap - control.phase_mem.stages[at].kept_on_heap).abs();
        assert!(moved < leak / 2, "{stage:?}: the leak is not this stage's: it moved {moved} bytes (planted {:?}, control {:?})", planted.phase_mem.stages[at], control.phase_mem.stages[at]);
    }
    // The same answer, byte for byte: what the leak changes is memory, never a record.
    assert_eq!(planted.status.0, control.status.0, "the exit status");
    assert_eq!((planted.tasks, planted.entered, planted.puts), (control.tasks, control.entered, control.puts), "the tasks planned, entered and put");
    assert_eq!(planted.records.digest(), control.records.digest(), "every record put, by digest");
    assert_eq!(planted.bodies, control.bodies, "the body notes");
}
