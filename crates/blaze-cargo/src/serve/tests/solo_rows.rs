//! The daemon's take of the solo answers' kept rows (`<state>/solo-rows`,
//! `blaze_certify::kept`), on the simulation: at its start and in its idle looks, each whole
//! file is numbered as a build of its own and given to the history, and removed.

use blaze_certify::kept::{DIR, Kept, names};

use super::{
    super::{daemon::LEAVES_TAKEN_SOLO_ROWS, observe::SOLO_ROWS},
    *,
};

fn kept(pid: u32) -> Kept {
    Kept {
        pid,
        started_ns: 1_000,
        ended_ns: 9_000,
        certified: Some(3),
        slot: "ab12".to_owned(),
        workspace: "/w".to_owned(),
        command: "run".to_owned(),
        timed: vec![
            blaze_api::Timed { name: blaze_thin::talk::EXEC_TO_MAIN.to_owned(), start_ns: 1_000, end_ns: 2_000 },
            blaze_api::Timed { name: blaze_thin::talk::SOLO_CHECK.to_owned(), start_ns: 2_000, end_ns: 8_000 },
        ],
    }
}

fn file(leaf: &str) -> FileName {
    name(&format!("{STATE}/{DIR}/{leaf}"))
}

/// The daemon turned until the history was given `count` solo answers, or `TURNS` turns.
fn turn_to(daemon: &mut SimDaemon, count: usize) {
    for _ in 0..TURNS {
        if SOLO_ROWS.with_borrow(|it| it.len()) >= count {
            return;
        }
        daemon.turn();
    }
}

/// A stretch of idle looks: the clock moved past the daemon's one-second look a few times, each with
/// turns enough for a look to end.
fn idle_looks(daemon: &mut SimDaemon) {
    for _ in 0..3 {
        daemon.driver().advance(Duration::from_millis(1_500));
        for _ in 0..200 {
            daemon.turn();
        }
    }
}

/// What a start and its idle looks broke of the invariant, or `None`.
fn broken(seed: u64, leaves_taken: bool) -> Option<String> {
    SOLO_ROWS.with_borrow_mut(Vec::clear);
    LEAVES_TAKEN_SOLO_ROWS.set(leaves_taken);
    let mut sim = Sim::new(seed, Faults::default());
    for pid in [41, 42] {
        sim.change(&file(&names(pid).0), &kept(pid).write().unwrap());
    }
    // A writer still making its record whole: never taken, and left within its bound.
    sim.change(&file(&names(43).1), &kept(43).write().unwrap());
    sim.change(&file("47"), b"cut");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, seed);
    turn_to(&mut daemon, 2);
    // A solo answer kept while the daemon runs: the next idle look's.
    daemon.driver().change(&file(&names(45).0), &kept(45).write().unwrap());
    idle_looks(&mut daemon);
    LEAVES_TAKEN_SOLO_ROWS.set(false);
    let taken = SOLO_ROWS.with_borrow_mut(std::mem::take);
    let mut builds: Vec<u64> = taken.iter().map(|it| it.0).collect();
    builds.sort_unstable();
    builds.dedup();
    if builds.len() != taken.len() {
        return Some(format!("two solo answers were numbered one build: {builds:?}"));
    }
    for pid in [41, 42, 45] {
        let times = taken.iter().filter(|it| it.1.pid == pid).count();
        if times != 1 {
            return Some(format!("{pid} was taken {times} times, not once"));
        }
        if taken.iter().any(|it| it.1.pid == pid && it.1 != kept(pid)) {
            return Some(format!("{pid}'s record was not taken as it was kept"));
        }
        if daemon.driver().exists(&file(&names(pid).0)) {
            return Some(format!("{pid} was taken and left"));
        }
    }
    if taken.iter().any(|it| it.1.pid == 43 || it.1.pid == 47) {
        return Some("a `.part` or a file that is not a record was taken".to_owned());
    }
    if !daemon.driver().exists(&file(&names(43).1)) {
        return Some("the `.part` of a writer within its bound was removed".to_owned());
    }
    if daemon.driver().exists(&file("47")) {
        return Some("a file that is not a record was left".to_owned());
    }
    None
}

/// The invariant: every whole solo-rows file is taken exactly once into the history (at the
/// daemon's start, or in an idle look after it), each a build of its own number, and removed;
/// a `.part` is never taken; a file that is not a record is removed and never taken. Control
/// (the planted bug): a take that leaves the file is caught as a record taken twice.
#[test]
fn every_whole_solo_rows_file_is_taken_once_into_the_history_and_a_part_never() {
    for seed in 0..8 {
        assert_eq!(broken(seed, false), None, "seed {seed}");
    }
    let caught = broken(3, true).expect("the planted bug is caught");
    assert!(caught.contains("not once"), "{caught}");
}
