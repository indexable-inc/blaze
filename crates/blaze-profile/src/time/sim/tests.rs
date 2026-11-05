//! The replay's hard logic on a build small enough to work out by hand.

use super::*;
use crate::time::{Event, Input, SwitchIn};

const P: u32 = 3;

/// Two workers from 0 to 16. Job 1 runs on worker 1 from 0 to 10 and gives job 2 to the pool
/// at 4; job 2 waits 2 for a core, though worker 2 sleeps (it slept since before the build) to
/// 6, then runs on it from 6 to 16. Worker 1 sleeps from 10.
fn model() -> Model {
    let ran = |thread: u32, t: i64, job: u64, since: i64| SwitchIn {
        since_ns: Some(since),
        ..SwitchIn::of(P, thread, t, Event::Ran, job, thread, thread)
    };
    let switches = vec![
        ran(1, 10, 1, 0),
        SwitchIn::of(P, 1, 10, Event::Idle, 0, 0, 1),
        SwitchIn::of(P, 2, 6, Event::Woke, 0, 0, 2),
        SwitchIn { by: Some(1), queued_ns: Some(2), ..ran(2, 16, 2, 6) },
    ];
    Model::build(Input {
        build: 1,
        build_row: Some((0, 16)),
        switches,
        spans: vec![],
        events: vec![],
    })
    .unwrap()
}

#[test]
fn a_worker_asleep_beside_a_queued_job_is_idle_with_work() {
    let model = model();
    assert_eq!(
        model.workers.iter().map(|it| it.idle.clone()).collect::<Vec<_>>(),
        [vec![(10, 16)], vec![(0, 6)]]
    );
    let idle = model.idle_with_work();
    assert_eq!(
        (idle.workers, idle.idle_ns, idle.with_work_ns, idle.submits_said),
        (2, 12, 2, true)
    );
}

#[test]
fn a_spawned_job_starts_at_its_spawners_progress_when_it_was_given() {
    let transcript = Transcript::of(&model());
    assert_eq!(
        (transcript.spawned, transcript.roots, transcript.runs, transcript.cost_ns),
        (1, 1, 2, 20)
    );
    assert_eq!(
        transcript.jobs[0].triggers,
        [Trigger { progress_ns: 4, job: 1, sets: Sets::Start }]
    );
    // No time of the workers' is neither idle nor running: no cost per run.
    assert_eq!(transcript.overhead_ns(), 0);
    let two = transcript.replay(2, Policy::Fifo, 0);
    assert_eq!(
        (two.wall_ns, two.busy_ns, two.ended, two.stuck, two.out_of_fuel),
        (14, 20, 2, 0, false)
    );
    let one = transcript.replay(1, Policy::Fifo, 0);
    assert_eq!(one.wall_ns, 20);
    let unbounded = transcript.replay(u32::MAX, Policy::Fifo, 0);
    assert_eq!((unbounded.wall_ns, unbounded.peak), (14, 2));
    // Each run pays the overhead before it.
    assert_eq!(transcript.replay(2, Policy::Fifo, 1).wall_ns, 16);
}

/// Job 2 parks on job 1's end and job 1 runs on after its wait: the wait's end is job 1's end,
/// whatever the cores.
#[test]
fn a_wait_on_a_jobs_end_ends_at_that_jobs_last_run() {
    let started = |thread: u32, t: i64, job: u64| SwitchIn {
        since_ns: Some(t),
        ..SwitchIn::of(P, thread, t, Event::Started, job, thread, thread)
    };
    let switches = vec![
        started(1, 0, 1),
        SwitchIn::of(P, 1, 8, Event::Ended, 1, 1, 1),
        started(2, 0, 2),
        SwitchIn::of(P, 2, 2, Event::Parked { on: WaitedOn::Job, value: 1 }, 2, 2, 2),
        SwitchIn::of(P, 2, 9, Event::Resumed, 2, 2, 2),
        SwitchIn::of(P, 2, 12, Event::Ended, 2, 2, 2),
    ];
    let model = Model::build(Input {
        build: 1,
        build_row: Some((0, 12)),
        switches,
        spans: vec![],
        events: vec![],
    })
    .unwrap();
    let transcript = Transcript::of(&model);
    assert_eq!(transcript.woken, 1);
    assert_eq!(
        transcript.jobs[1].segments,
        [
            Segment::Run { ns: 2 },
            Segment::Wait { wait: 0, ends: Ends::Woken },
            Segment::Run { ns: 3 }
        ]
    );
    // On one core: job 1 to 8, then job 2's 2 and 3.
    assert_eq!(transcript.replay(1, Policy::Fifo, 0).wall_ns, 13);
    assert_eq!(transcript.replay(2, Policy::Fifo, 0).wall_ns, 11);
}
