//! A search run from a case board (check.rs `search_on_board`; test.md, "The case board";
//! piece 2): the board says which case runs, and on which tape, before the body runs, so a body
//! that panics leaves its case and tape there with nothing caught; a failure the body returns
//! ends the board with that case's record; a pass ends it passed with every case counted.
//!
//! `cargo test -p blaze-sim board_tests` runs the three; `search_on_board` prints nothing, so
//! each prints only `ok`, and the process's panic hook prints case 17's panic, `case 17 broke`.

#![expect(
    blaze::error_debug,
    reason = "a test's failure message shows the values it compared as they are"
)]

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use super::search_on_board;
use crate::board::{
    Board, Budget, Bytes, CaseOrigin, End, Plan, Plant, Progress, Seeds, Shape, Spec, TailWord,
    Writer, encode,
};
use crate::search::{Broke, Config, Failed, Origin, Passed, Verdict};
use crate::tape::Tape;

/// A board of `spec` on a page of its length, and a copy of its front through the plan: the
/// child reads its plan into its own memory, as `check` does.
fn board_of(spec: &Spec<'_>) -> (Vec<u8>, Shape, Vec<u8>) {
    let shape = Shape::of(spec).expect("a plan within the caps");
    let mut page = vec![0u8; shape.end()];
    encode(spec, &mut page).expect("a page of the board's length");
    let front = page[..shape.tape_at()].to_vec();
    (page, shape, front)
}

const FIRST: u64 = 1000;

/// Runs seeds `FIRST..FIRST + 40` from a board whose writer has `plant`, the body panicking in
/// case 17; then says whether the board holds case 17 and its tape: the case running, no
/// prefix, the 16 cases before it passed, a tape whose replay draws what case 17 drew, and the
/// seed `FIRST + 16`.
fn panics_at_17(plant: Plant) -> Result<(), String> {
    let spec = Spec {
        target_check: 1,
        given: &[],
        seeds: Seeds::Range { first: FIRST, count: 40 },
        budget: Budget::NONE,
        record_cap: 64,
        coverage_bytes: 0,
    };
    let (mut page, shape, front) = board_of(&spec);
    let plan = Plan::open(&shape, &front).expect("the plan opens");
    let cases = Cell::new(0u32);
    let drew = Cell::new(None);
    let mut writer = Writer::planted(Bytes(&mut page), shape, plant);
    let ran = catch_unwind(AssertUnwindSafe(|| {
        search_on_board(Config::cases(256), 1, &plan, &mut writer, |tape: &Tape| {
            cases.set(cases.get() + 1);
            let value = tape.next_u64("x");
            if cases.get() == 17 {
                drew.set(Some(value));
                panic!("case 17 broke");
            }
        })
    }));
    drop(writer);
    if ran.is_ok() {
        return Err("the search came back from a body that panicked".to_owned());
    }
    let Some(drawn) = drew.get() else { return Err("case 17 never ran".to_owned()) };
    let board = Board::open(&page).map_err(|error| format!("the board did not open: {error}"))?;
    let Progress::Running { counts, case, tape } = board.progress() else {
        return Err(format!("the board says {:?}, not a case running", board.progress()));
    };
    if case != 17 {
        return Err(format!("the board says case {case} was running"));
    }
    if counts.tally.passed != 16 {
        return Err(format!("the board says {} cases passed before case 17", counts.tally.passed));
    }
    if tape.prefix_len() != 0 {
        return Err(format!("case 17's tape has a prefix of {} values", tape.prefix_len()));
    }
    let TailWord::Seeded(seed) = tape.tail else {
        return Err(format!("case 17's tail is {:?}", tape.tail));
    };
    let again = Tape::seeded(seed).next_u64("x");
    if again != drawn {
        return Err(format!("the board's tape, seed {seed}, draws {again}; case 17 drew {drawn}"));
    }
    if seed != FIRST + 16 {
        return Err(format!("case 17's seed is {seed}, not {}", FIRST + 16));
    }
    Ok(())
}

/// The board names the case a body panicked in, and its tape, with nothing caught: a replay of
/// that tape draws what the case drew. The control: a writer that writes the seed one too high
/// (`Plant::SeedOffByOne`) is caught by the replay.
#[test]
fn a_panic_at_case_17_leaves_its_tape_on_the_board() {
    assert_eq!(panics_at_17(Plant::None), Ok(()));
    let planted = panics_at_17(Plant::SeedOffByOne);
    assert!(planted.is_err(), "a board one seed off was taken for case 17's tape");
}

/// Two given tapes, then seeds `0..10`; the body returns a failure in case 5 (seed 2): the board
/// ends failed at case 5 with the oracle's origin, the text, the values case 5 drew and its
/// seed; the verdict is that failure, numbered across the given tapes and the seeds.
#[test]
fn a_returned_failure_ends_the_board_with_its_record() {
    let given: [&[u64]; 2] = [&[3, 4], &[5]];
    let spec = Spec {
        target_check: 1,
        given: &given,
        seeds: Seeds::Range { first: 0, count: 10 },
        budget: Budget::NONE,
        record_cap: 8,
        coverage_bytes: 0,
    };
    let (mut page, shape, front) = board_of(&spec);
    let plan = Plan::open(&shape, &front).expect("the plan opens");
    let cases = Cell::new(0u32);
    let drew = Cell::new((0, 0));
    let mut writer = Writer::new(Bytes(&mut page), shape);
    let verdict =
        search_on_board(Config::cases(256), 1, &plan, &mut writer, |tape: &Tape| -> Result<(), Broke> {
            cases.set(cases.get() + 1);
            let (a, b) = (tape.below(10, "a"), tape.below(10, "b"));
            if cases.get() == 1 && (a, b) != (3, 4) {
                return Err(Broke::Harness(format!("the first given tape drew {a} {b}")));
            }
            if cases.get() == 5 {
                drew.set((a, b));
                return Err(Broke::Oracle("planted: case 5".to_owned()));
            }
            Ok(())
        })
        .expect("the board takes every write");
    drop(writer);
    let Verdict::Failed(Failed { seed, case, cases: of, failure, .. }) = &verdict else {
        panic!("{verdict:?}")
    };
    assert_eq!((*seed, *case, *of), (Some(2), 5, 12), "{verdict:?}");
    assert_eq!(failure.origin, Origin::Oracle, "{verdict:?}");
    let board = Board::open(&page).expect("an ended board opens");
    let Progress::Ended { counts, case, tape, end } = board.progress() else {
        panic!("{:?}", board.progress())
    };
    assert_eq!((case, counts.tally.passed, counts.given_run), (5, 4, 2));
    assert_eq!(tape.tail, TailWord::Seeded(2));
    let End::Failed { origin, record, text, .. } = end else { panic!("{end:?}") };
    assert_eq!((origin, text), (CaseOrigin::Oracle, "planted: case 5"));
    let (a, b) = drew.get();
    assert_eq!(record.values().collect::<Vec<u64>>(), vec![a, b]);
}

/// The target check runs the two given tapes and the ten seeds; check 2, not the board's
/// target, runs its own declared seven cases and no given tape. Each ends the board passed with
/// every case counted.
#[test]
fn a_pass_ends_the_board_passed_and_another_check_runs_its_declared_cases() {
    let given: [&[u64]; 2] = [&[3, 4], &[5]];
    let spec = Spec {
        target_check: 1,
        given: &given,
        seeds: Seeds::Range { first: 0, count: 10 },
        budget: Budget::NONE,
        record_cap: 8,
        coverage_bytes: 0,
    };
    for (check, cases, given_run) in [(1u32, 12u32, 2u32), (2, 7, 0)] {
        let (mut page, shape, front) = board_of(&spec);
        let plan = Plan::open(&shape, &front).expect("the plan opens");
        let mut writer = Writer::new(Bytes(&mut page), shape);
        let verdict = search_on_board(Config::cases(7), check, &plan, &mut writer, |tape: &Tape| {
            tape.below(10, "a");
        })
        .expect("the board takes every write");
        drop(writer);
        let Verdict::Passed(Passed { cases: ran, .. }) = verdict else {
            panic!("check {check}: {verdict:?}")
        };
        assert_eq!(ran, cases, "check {check}");
        let board = Board::open(&page).expect("an ended board opens");
        let Progress::Ended { counts, case, end: End::Passed, .. } = board.progress() else {
            panic!("check {check}: {:?}", board.progress())
        };
        assert_eq!(
            (counts.check, counts.given_run, counts.tally.passed, case),
            (check, given_run, cases, cases),
            "check {check}"
        );
    }
}
