use std::alloc::Global;

use blaze_profile::stuck::{Edge, Stuck, Task, WaitsOn, Why};

use super::{Fate, Lost, breaks_at, fate, lost_bound};

/// The invariant: a parked job is failed at its wait, a child killed, and a thread lost only
/// where nothing it waits behind is failed this look. Make a thread behind a job lost and a
/// convoy behind one parked job would count every waiter lost, ending the daemon for a stall
/// the job's failure ends.
#[test]
fn each_task_meets_the_end_its_kind_allows() {
    assert_eq!(fate(Task::Job(7), Why::Still { bound_ns: 1 }), Fate::FailJob { observed: 7 });
    assert_eq!(fate(Task::Child(9), Why::Still { bound_ns: 1 }), Fate::Kill { pid: 9 });
    assert_eq!(fate(Task::Thread(3), Why::Still { bound_ns: 1 }), Fate::Lost { tid: 3 });
    assert_eq!(fate(Task::Thread(3), Why::Behind { end: Task::Job(7) }), Fate::Behind);
    assert_eq!(fate(Task::Thread(3), Why::Behind { end: Task::Child(9) }), Fate::Behind);
    assert_eq!(fate(Task::Thread(3), Why::Behind { end: Task::Thread(4) }), Fate::Lost { tid: 3 });
    assert_eq!(fate(Task::Thread(3), Why::BehindCycle), Fate::Lost { tid: 3 });
    assert_eq!(fate(Task::Loop, Why::Still { bound_ns: 1 }), Fate::Loop);
}

fn cycle(members: &[Task]) -> Stuck {
    let mut edges = Vec::new_in(Global);
    for (i, &task) in members.iter().enumerate() {
        let held_by = members[(i + 1) % members.len()];
        edges.push(Edge { task, on: WaitsOn::Join { task: held_by }, held_by });
    }
    Stuck {
        task: members[0],
        build: 1,
        span: 0,
        still_ns: 5_000_000_000,
        waits: WaitsOn::Join { task: members[1 % members.len()] },
        held_by: Some(members[1 % members.len()]),
        why: Why::Cycle,
        cycle: edges,
    }
}

/// A cycle is broken where a member can be failed (its least job, else a child), and lost
/// whole only when every member is a thread.
#[test]
fn a_cycle_breaks_at_its_least_job_and_a_cycle_of_threads_at_none() {
    let mixed = cycle(&[Task::Thread(1), Task::Job(9), Task::Job(4), Task::Child(2)]);
    assert_eq!(breaks_at(&mixed), Some(Task::Job(4)));
    let with_child = cycle(&[Task::Thread(1), Task::Child(2)]);
    assert_eq!(breaks_at(&with_child), Some(Task::Child(2)));
    let threads = cycle(&[Task::Thread(1), Task::Thread(2)]);
    assert_eq!(breaks_at(&threads), None);
}

/// Lost threads are counted once each, and the daemon ends only past a quarter of its workers,
/// at least two: one lost thread is survived.
#[test]
fn lost_workers_are_counted_once_and_bounded_by_a_quarter_of_the_pool() {
    assert_eq!(lost_bound(1), 2);
    assert_eq!(lost_bound(8), 2);
    assert_eq!(lost_bound(128), 32);
    let mut lost = Lost::new(8);
    assert!(!lost.lose(1));
    assert!(!lost.lose(1), "the same thread said again is not lost twice");
    assert!(!lost.lose(2));
    assert_eq!(lost.count(), 2);
    assert!(lost.lose(3), "the third of a pool of eight is past its bound of two");
}
