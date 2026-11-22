//! The pool's class rule (threads.rs, "Search order": "a background task never starts on a
//! foreground worker while foreground work is queued where that worker looks") and a thief's
//! group order (its own group's deques before another's), on what the pool HANDS OUT, checked
//! on [`Pool::find`] itself with the pool's threads never started: no thread, no timing, no
//! scheduler. Both replace timing forms in task/tests.rs: the class rule's asserted the
//! order the tasks' bodies ran in, which the machine's scheduler decides (a worker descheduled
//! between taking a foreground task and running it let a later background body run first);
//! the group order's asserted that real thieves stole near at least once, which the scheduler
//! also decides and which a thief trying the other group first passes too.

use core::ptr::NonNull;

use blaze_core::wait::{Deadline, Monotonic};
use blaze_mem::{Frame, Tag};

use super::{Class, FindPlant, Header, Here, JobPtr, Plants, Pool, Queued, Ran, Start, Tally, Topology, job};
use crate::{cx::Cx, io::Io, mem::TASK_FRAME};

/// Runs `job` on worker 0 as its loop would: its name.
fn ran(pool: &Pool, job: JobPtr) -> &'static str {
    let header: NonNull<Header> = job.0;
    // SAFETY: the queue's reference keeps the job alive until `run_job` releases it.
    let name = unsafe { header.as_ref() }.name;
    pool.run_job(0, job);
    name
}

/// A starter that starts nothing: the workers stay unstarted, and the test is their thread.
fn no_threads(_pool: &Pool, _at: u32) {}

/// What worker 0 of a one-group, two-worker foreground pool is handed, in order, after three
/// foreground tasks and one background task were spawned from outside (the background one
/// first), with the pool's `plants`, in `looks` looks of [`Pool::find`]; the rest taken from
/// the queues: the tasks' names.
fn handed(plants: Plants, looks: usize) -> [&'static str; 4] {
    let pool = Pool::new(&Topology::flat(2), Monotonic::new(), plants, no_threads);
    let io = Io::sim(13);
    let frame = Frame::with_reserve(TASK_FRAME, Tag::Frames).expect("a root frame");
    let cx = Cx::root_on(Deadline::after(io.now(), core::time::Duration::from_secs(60)), frame, io.clone(), pool.clone());
    let tally = Tally::new(cx.canceller());
    let mut handles: [Option<Queued<()>>; 4] = [None, None, None, None];
    let spawns = [("background", Class::Background), ("f0", Class::Foreground), ("f1", Class::Foreground), ("f2", Class::Foreground)];
    for (at, (name, class)) in spawns.into_iter().enumerate() {
        // SAFETY: the body borrows nothing; every job is run below before its handle drops.
        let (queued, queue) = unsafe { job::<_, ()>(name, &tally, Start::Open, |_here: Here| Ran::Made(())) };
        assert!(pool.spawn(Here::Outside, class, queue).is_ok(), "an injection queue of 1024 refused task {name}");
        handles[at] = Some(queued);
    }
    // Three looks while three tasks of each worker's class are queued in its own group: each
    // is answered from the group's queues before any search (so the idle protocol's search
    // grant, which a spawn's wake moves, plays no part). The fourth name is what is left in
    // the background queue, taken directly.
    // `looks` looks while tasks of the worker's class are queued in its own group: each is
    // answered from the group's queues before any search (so the idle protocol's search grant,
    // which a spawn's wake moves, plays no part). The rest are taken out of the group's queues
    // directly, background first, and run, so every handle finds its task ended.
    let mut searching = false;
    let mut names = [""; 4];
    for at in 0..looks {
        let Some(found) = pool.find(0, &mut searching) else {
            panic!("worker 0 was handed nothing with tasks queued (handed so far: {names:?})");
        };
        names[at] = ran(&pool, found);
    }
    assert!(!searching, "worker 0 searched with tasks queued in its own group");
    let shared = pool.shared();
    for name in names.iter_mut().skip(looks) {
        let left = super::take(&shared.groups[0], Class::Background).or_else(|| super::take(&shared.groups[0], Class::Foreground));
        let Some(left) = left else {
            panic!("a spawned task was not in its group's queues (handed: {names:?})");
        };
        *name = ran(&pool, left);
    }
    for handle in handles.iter().flatten() {
        assert!(handle.done(), "task {} was handed out but did not end", handle.name());
    }
    names
}

/// The rule: while f0, f1 and f2 are queued, worker 0 is handed each of them before the
/// background task, though the background task was spawned first. Prints
/// `test task::threads::find_tests::a_foreground_worker_is_handed_no_background_task_while_foreground_is_queued ... ok`.
#[test]
fn a_foreground_worker_is_handed_no_background_task_while_foreground_is_queued() {
    assert_eq!(handed(Plants::NONE, 3), ["f0", "f1", "f2", "background"], "the class rule");
}

/// The control (rule 53): a `find` that looks at the background queue first hands the
/// background task out while foreground work is queued, and the rule's check sees it. Prints
/// `test task::threads::find_tests::the_class_rule_catches_a_find_that_takes_the_background_queue_first ... ok`.
#[test]
fn the_class_rule_catches_a_find_that_takes_the_background_queue_first() {
    let planted = Plants { find: FindPlant::BackgroundFirst, ..Plants::NONE };
    // One look: the planted find answers it from the background queue, while f0..f2 wait.
    let names = handed(planted, 1);
    assert_eq!(names[0], "background", "the planted find was not caught: it handed {names:?}");
}

/// What worker 0 of two foreground groups of two steals first, with the pool's `plants`, when
/// its group-mate (worker 1) and a worker of the other group (worker 2) each hold one task on
/// its deque: the stolen task's name, and worker 0's (near, far) steal counts after that one
/// steal. Both tasks are run before the handles drop.
fn stolen_first(plants: Plants) -> (&'static str, (u64, u64)) {
    let topology = Topology::groups(&[(Class::Foreground, 2), (Class::Foreground, 2)]);
    let pool = Pool::new(&topology, Monotonic::new(), plants, no_threads);
    let io = Io::sim(19);
    let frame = Frame::with_reserve(TASK_FRAME, Tag::Frames).expect("a root frame");
    let cx = Cx::root_on(Deadline::after(io.now(), core::time::Duration::from_secs(60)), frame, io.clone(), pool.clone());
    let tally = Tally::new(cx.canceller());
    let shared = pool.shared();
    let mut handles = Vec::new();
    for (name, victim) in [("near", 1usize), ("far", 2usize)] {
        // SAFETY: the body borrows nothing; every job is run below before its handle drops.
        let (queued, queue) = unsafe { job::<_, ()>(name, &tally, Start::Open, |_here: Here| Ran::Made(())) };
        assert!(shared.workers[victim].0.deque.push(queue.0).is_ok(), "worker {victim}'s empty deque refused {name}");
        handles.push(queued);
    }
    // Already searching: the idle protocol's grant plays no part, only the search order.
    let mut searching = true;
    let Some(found) = pool.find(0, &mut searching) else { panic!("worker 0 stole nothing with two deques holding a task") };
    let first = ran(&pool, found);
    let counts = pool.steals(0);
    let Some(rest) = pool.find(0, &mut searching) else { panic!("worker 0 did not find the second task after {first}") };
    ran(&pool, rest);
    for handle in &handles {
        assert!(handle.done(), "task {} was handed out but did not end", handle.name());
    }
    (first, counts)
}

/// The rule (threads.rs, "Search order"): a thief tries its own group's deques before another
/// group's, and counts that steal near. Prints
/// `test task::threads::find_tests::a_thief_steals_from_its_own_group_before_another ... ok`.
#[test]
fn a_thief_steals_from_its_own_group_before_another() {
    assert_eq!(stolen_first(Plants::NONE), ("near", (1, 0)), "worker 0 stole first from its group-mate, counted near");
}

/// The control (rule 53): a `find` that tries the other group first steals its task, and the
/// rule's check sees it. Prints
/// `test task::threads::find_tests::the_group_rule_catches_a_thief_that_tries_another_group_first ... ok`.
#[test]
fn the_group_rule_catches_a_thief_that_tries_another_group_first() {
    let planted = Plants { find: FindPlant::FarFirst, ..Plants::NONE };
    let first = stolen_first(planted);
    assert_ne!(first, ("near", (1, 0)), "the planted find was not caught");
    assert_eq!(first, ("far", (0, 1)), "the planted find stole from the other group, counted far");
}
