//! The parent's machine over a tape-search test's boards (docs/v2/plan/design/test.md, "End
//! state: property, differential and end-to-end tests", A, "The case board"; G, piece 3). Each
//! child is simulated by the real child side, blaze-sim's `search_on_board`, on a fresh board in
//! a buffer: the first child runs the given tapes (a few passing, then one failing), and every
//! candidate and the final replay run in a fresh child on a board planning that tape alone. The
//! planted body fails when choice 0 is at least 2 and choice 4 is 255, so the one least tape is
//! `2 0 0 0 255`, printed `2 0*3 255`.
//!
//! Oracle: the verdict is `Failed`, its line is `FAILED x: least tape 2 0*3 255`, the least tape
//! fails the body when replayed, `write_tape` prints what `blaze_sim::text::print` prints, and no
//! more children ran than the shrink budget and the replay. The planted bug
//! (`PropPlant::AcceptUnfailed`: a candidate taken as failing though its child passed) leaves a
//! passing tape as the least, and its replay files `CaseNotAFunction`: caught.
//!
//! Expected lines (`lanecheck.sh blaze-run test sim::property`):
//! `test sim::property::a_returned_failure_shrinks_to_the_least_tape ... ok` (it prints
//! `property: least tape 2 0*3 255; plant AcceptUnfailed caught` in its doc's words, not on
//! stdout), `test sim::property::accepting_an_unfailed_candidate_is_caught ... ok`,
//! `test sim::property::a_case_that_ends_the_child_shrinks_from_its_prefix ... ok`.

use std::panic::{AssertUnwindSafe, catch_unwind};

use blaze_own::{AVec, Alloc, Vec as OwnVec};
use blaze_sim::board::{Board, Budget, Bytes, Plan, Seeds, Shape, Spec, Writer, encode};
use blaze_sim::check::search_on_board;
use blaze_sim::{Broke, Config as SimConfig, Tape, Verdict, check, control};

use super::{harness, key, oracle};
use crate::TestId;
use crate::property::{ChildEnd, PropAct, PropPlant, PropVerdict, Property, PropertyConfig, write_tape};

/// The shrink runs each check declares; the parent's cap is above it.
const SHRINK: u32 = 500;
const LEAST: [u64; 5] = [2, 0, 0, 0, 255];
const LINE: &str = "FAILED x: least tape 2 0*3 255";

/// How the planted body fails.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fails {
    /// It returns `Broke::Oracle`: the board keeps the case's record.
    Returns,
    /// It panics: the child ends in the case (simulated by catching the unwind and reporting
    /// an abort, as a build without unwinding ends).
    Panics,
}

/// Five byte draws; true when the case fails.
fn breaks(tape: &Tape) -> bool {
    let first = tape.below(256, "a");
    for _ in 0..3 {
        tape.below(256, "middle");
    }
    let last = tape.below(256, "e");
    first >= 2 && last == 255
}

fn returning(tape: &Tape) -> Result<(), Broke> {
    match breaks(tape) {
        true => Err(Broke::Oracle("choice 0 >= 2 and choice 4 = 255".to_owned())),
        false => Ok(()),
    }
}

fn panicking(tape: &Tape) {
    assert!(!breaks(tape), "choice 0 >= 2 and choice 4 = 255");
}

/// One child from the zygote: a fresh board of `spec`, check 1 run on it, the page after and
/// how the child ended.
fn child(spec: &Spec<'_>, fails: Fails) -> Result<(Vec<u8>, ChildEnd), Broke> {
    let shape = Shape::of(spec).map_err(harness)?;
    let mut page = vec![0u8; shape.end()];
    encode(spec, &mut page).map_err(harness)?;
    let front = page[..shape.tape_at()].to_vec();
    let plan = Plan::open(&shape, &front).map_err(harness)?;
    let config = SimConfig::cases(16).shrink_runs(SHRINK);
    let mut writer = Writer::new(Bytes(&mut page), shape);
    let how = match fails {
        Fails::Returns => {
            match search_on_board(config, 1, &plan, &mut writer, returning).map_err(harness)? {
                Verdict::Passed(_) => ChildEnd::Exited(0),
                _ => ChildEnd::Exited(101),
            }
        }
        Fails::Panics => match catch_unwind(AssertUnwindSafe(|| search_on_board(config, 1, &plan, &mut writer, panicking))) {
            Ok(ran) => match ran.map_err(harness)? {
                Verdict::Passed(_) => ChildEnd::Exited(0),
                _ => ChildEnd::Exited(101),
            },
            // SIGABRT: a panic aborts where blaze builds without unwinding.
            Err(_) => ChildEnd::Signaled(6),
        },
    };
    drop(writer);
    Ok((page, how))
}

/// Runs the first child on `given`, then every run the machine asks for, to its verdict;
/// checks the oracle.
fn drive(given: &[&[u64]], fails: Fails, plant: PropPlant) -> Result<(), Broke> {
    let spec = Spec {
        target_check: 1,
        given,
        seeds: Seeds::Range { first: 0, count: 0 },
        budget: Budget::NONE,
        record_cap: 64,
        coverage_bytes: 0,
    };
    let mut property = Property::planted(TestId(key(1)), PropertyConfig::DEFAULT, Alloc::global(), plant);
    let mut out: AVec<'_, PropAct> = OwnVec::new_in(Alloc::global());
    let (page, how) = child(&spec, fails)?;
    let board = Board::open(&page).map_err(harness)?;
    property.ended(&board, how, &mut out).map_err(harness)?;
    let mut runs: u32 = 0;
    let verdict = loop {
        let [act] = out.as_slice() else {
            return Err(Broke::Harness(format!("the machine asked for {} acts at once, not one", out.len())));
        };
        let act = *act;
        out.clear();
        let rerun = match act {
            PropAct::Done(verdict) => break verdict,
            PropAct::RunCandidate(rerun) | PropAct::Replay(rerun) => rerun,
        };
        runs += 1;
        oracle(runs <= SHRINK + 1, || {
            format!("the machine ran {runs} children, past {SHRINK} shrink runs and the replay of the least tape")
        })?;
        let tapes = [property.tape()];
        let (page, how) = child(&rerun.spec(&tapes), fails)?;
        let board = Board::open(&page).map_err(harness)?;
        property.ran(rerun.id, &board, how, &mut out).map_err(harness)?;
    };
    let least = property.least();
    let mut line = String::new();
    property.write_verdict(&mut line, "x", &verdict).map_err(harness)?;
    oracle(matches!(verdict, PropVerdict::Failed { .. }) && line == LINE, || {
        format!("the verdict is {verdict:?}, printed {line:?}, not {LINE:?} (least tape {least:?})")
    })?;
    oracle(least == LEAST, || format!("the least tape is {least:?}, not {LEAST:?}"))?;
    oracle(breaks(&Tape::replay(least)), || format!("the least tape {least:?} does not fail the body"))?;
    let mut ours = String::new();
    write_tape(&mut ours, least).map_err(harness)?;
    let theirs = blaze_sim::text::print(least);
    oracle(ours == theirs.as_str(), || format!("write_tape printed the least tape {ours:?}; text.rs prints {theirs:?}"))
}

/// A first child with up to three passing given tapes, then one failing tape drawn from the case.
fn run(tape: &Tape, plant: PropPlant) -> Result<(), Broke> {
    let passing: Vec<[u64; 5]> = (0..tape.below(4, "passing tapes"))
        .map(|_| {
            [
                tape.below(256, "a"),
                tape.below(256, "m"),
                tape.below(256, "m"),
                tape.below(256, "m"),
                tape.below(255, "e below 255"),
            ]
        })
        .collect();
    let failing = [
        tape.range(2..256, "a at least 2"),
        tape.below(256, "m"),
        tape.below(256, "m"),
        tape.below(256, "m"),
        255,
    ];
    let mut given: Vec<&[u64]> = passing.iter().map(|values| &values[..]).collect();
    given.push(&failing);
    drive(&given, Fails::Returns, plant)
}

/// property: least tape 2 0*3 255.
#[test]
fn a_returned_failure_shrinks_to_the_least_tape() {
    check(SimConfig::cases(40), |tape| run(tape, PropPlant::None));
}

/// plant AcceptUnfailed caught.
#[test]
fn accepting_an_unfailed_candidate_is_caught() {
    control(SimConfig::cases(40), PropPlant::None, PropPlant::AcceptUnfailed, "least tape", run);
}

/// A body that panics: each failing child ends in its case (an abort), the board holds the
/// case's prefix, and the shrink reaches the same least tape. The panics print to stderr.
#[test]
fn a_case_that_ends_the_child_shrinks_from_its_prefix() {
    let failing: [u64; 5] = [9, 7, 5, 3, 255];
    if let Err(broke) = drive(&[&failing], Fails::Panics, PropPlant::None) {
        panic!("{broke}");
    }
}
