//! The pool's schedules (engine_pool.rs): each plan covers its tasks once, a skewed phase keeps
//! every worker busy under [`Guided`] where [`Static`] leaves them idle (the control), and the
//! results come back in task order whichever claimer ran which chunk, under both, with a planted
//! control for the reorder.
//!
//! The simulation runs the pool's own sans-IO core: [`Schedule::plan`], [`Claims`] and
//! [`in_chunk_order`], with the workers' time a model (a worker free at the earliest time claims
//! next) in place of bstd's threads.

use std::alloc::Global;

use blaze_own::Vec;
use blaze_sim::{Broke, Config, Tape, check, control};

use super::{Claims, Guided, Piece, PoolPlant, Schedule, Static, in_chunk_order};

/// A plan's chunks: ascending from 0 to `len`, none empty.
fn plan_of<S: Schedule>(len: usize, workers: usize) -> Vec<usize, Global> {
    let mut starts = Vec::new_in(Global);
    S::plan(len, workers, workers * 4, &mut starts);
    starts
}

fn covers<S: Schedule>(len: usize, workers: usize) {
    let starts = plan_of::<S>(len, workers);
    assert_eq!(starts.first(), Some(&0), "{} {len} {workers}", S::NAME);
    assert_eq!(starts.last(), Some(&len), "{} {len} {workers}", S::NAME);
    assert!(starts.windows(2).all(|it| it[0] < it[1]), "{}: an empty or descending chunk, {len} tasks", S::NAME);
    let chunks = starts.len() - 1;
    assert!(S::claimers(workers, workers * 4, chunks) <= workers * 4);
}

#[test]
fn every_plan_covers_its_tasks_once() {
    for len in [0, 1, 2, 7, 31, 32, 33, 1000, 100_003] {
        for workers in [1, 2, 8, 128] {
            covers::<Static>(len, workers);
            covers::<Guided>(len, workers);
            assert!(plan_of::<Static>(len, workers).len() - 1 <= workers * 4, "static's chunks fit its buffers");
        }
    }
    // Guided shrinks and ends in single tasks: a chunk is `ceil(left / (4 * workers))`, so each
    // chunk that starts with `4 * workers` tasks or fewer left is one task (how many such chunks
    // there are depends on where the remainder lands: 31 or 32 here), and no chunk is larger
    // than the one before it.
    for len in [1000, 1024, 1025, 100_003] {
        for workers in [1, 2, 8, 128] {
            let parts = 4 * workers;
            let starts = plan_of::<Guided>(len, workers);
            assert!(starts.windows(2).filter(|it| len - it[0] <= parts).all(|it| it[1] - it[0] == 1), "guided's tail, {len} tasks on {workers}");
            assert!(starts.windows(3).all(|it| it[2] - it[1] <= it[1] - it[0]), "guided's chunks shrink, {len} tasks on {workers}");
        }
    }
    let starts = plan_of::<Guided>(1024, 8);
    assert!(starts.windows(2).rev().take(31).all(|it| it[1] - it[0] == 1));
    assert_eq!(starts[1], 32, "the first chunk is a static chunk's size");
}

/// The phase's wall and its idle worker time when `workers` workers claim `S`'s chunks of tasks
/// costing `costs`, each worker claiming the next chunk as it comes free.
fn simulate<S: Schedule>(costs: &[u64], workers: usize) -> (u64, u64) {
    let starts = plan_of::<S>(costs.len(), workers);
    let claims = Claims::new(&starts);
    let mut free = std::vec![0u64; workers];
    loop {
        let (worker, at) = free.iter().copied().enumerate().min_by_key(|&(_, at)| at).expect("a worker");
        let Some((_, range)) = claims.claim() else { break };
        free[worker] = at + costs[range].iter().sum::<u64>();
    }
    let wall = free.iter().copied().max().unwrap_or(0);
    (wall, workers as u64 * wall - costs.iter().sum::<u64>())
}

/// One task of 100 units first among 1,023 of one unit, on eight workers: the floor is the
/// busy time over the workers, 140.4 units. Guided ends at 141, with 5 units of idle worker
/// time; the control, static, holds the long task's chunk-mates behind it and ends at 160 with
/// 157 idle. Uniform costs leave neither idle.
#[test]
fn a_skewed_phase_keeps_every_worker_busy_under_guided() {
    let mut costs = std::vec![1u64; 1024];
    costs[0] = 100;
    let (guided_wall, guided_idle) = simulate::<Guided>(&costs, 8);
    let (static_wall, static_idle) = simulate::<Static>(&costs, 8);
    assert!(guided_wall <= 142, "guided ended at {guided_wall}, the floor is 140.4");
    assert!(guided_idle <= 8, "guided left {guided_idle} units idle");
    assert!(static_wall >= 160, "the control: static ended at {static_wall}");
    assert!(static_idle >= 150, "the control: static left {static_idle} units idle");
    // In the middle, too: guided's wall is the long task's claim plus itself and a small chunk.
    costs[0] = 1;
    costs[512] = 100;
    let (guided_wall, guided_idle) = simulate::<Guided>(&costs, 8);
    let (static_wall, static_idle) = simulate::<Static>(&costs, 8);
    assert!(guided_wall < static_wall && guided_idle < static_idle, "guided {guided_wall}/{guided_idle}, static {static_wall}/{static_idle}");
    let uniform = std::vec![1u64; 1024];
    assert_eq!(simulate::<Guided>(&uniform, 8), (128, 0));
    assert_eq!(simulate::<Static>(&uniform, 8), (128, 0));
}

/// The items task `task` puts on its buffer: none, one or two, so pieces differ in length.
fn items(task: usize) -> impl Iterator<Item = (usize, usize)> {
    (0..task % 3).map(move |at| (task, at))
}

/// One case: `S`'s chunks of `len` tasks claimed in a drawn interleaving of its claimers, each
/// putting its tasks' items on its own list, then put in chunk order; the list must be every
/// task's items in task order, as one claimer running every task in turn makes it.
fn in_task_order<S: Schedule>(tape: &Tape, plant: PoolPlant) -> Result<(), Broke> {
    let len = tape.int_in(0..=200usize, "tasks");
    let workers = tape.int_in(1..=8usize, "workers");
    let starts = plan_of::<S>(len, workers);
    let claims = Claims::new(&starts);
    let claimers = S::claimers(workers, workers * 4, claims.chunks());
    let mut lists: std::vec::Vec<Vec<(usize, usize), Global>> = (0..claimers).map(|_| Vec::new_in(Global)).collect();
    let mut pieces = std::vec![Piece::UNCLAIMED; claims.chunks()];
    let mut active: std::vec::Vec<usize> = (0..claimers).collect();
    while !active.is_empty() {
        let at = tape.index(active.len(), "which claimer claims next");
        let claimer = active[at];
        let Some((chunk, range)) = claims.claim() else {
            active.swap_remove(at);
            continue;
        };
        let before = lists[claimer].len();
        for task in range.clone() {
            lists[claimer].extend(items(task));
        }
        pieces[chunk] = Piece { claimer, ran: range.len(), done: lists[claimer].len() - before, made: 0, owed: 0 };
    }
    if claimers > 1 && pieces.windows(2).any(|it| it[1].claimer < it[0].claimer) {
        tape.reached("a later chunk was claimed by an earlier claimer");
    }
    let mut parts: std::vec::Vec<&mut Vec<(usize, usize), Global>> = lists.iter_mut().collect();
    in_chunk_order(&mut parts, &pieces, |piece| piece.done, Global, plant);
    let mut got = std::vec::Vec::new();
    for list in &lists {
        got.extend(list.iter().copied());
    }
    let want: std::vec::Vec<(usize, usize)> = (0..len).flat_map(items).collect();
    match got == want {
        true => Ok(()),
        false => Err(Broke::Oracle(format!("{}: the results are not in task order: {len} tasks on {workers} workers", S::NAME))),
    }
}

/// The invariant: whatever claimer ran whatever chunk, the buffers read in order hold every
/// task's results in task order, under both schedules: so the merge reads one list, the one a
/// static split made (rule 25).
#[test]
fn the_results_come_back_in_task_order_under_both_schedules() {
    let config = Config { must_reach: &["a later chunk was claimed by an earlier claimer"], ..Config::cases(2_000) };
    check(config, |tape| in_task_order::<Guided>(tape, PoolPlant::None));
    let config = Config { must_reach: &["a later chunk was claimed by an earlier claimer"], ..Config::cases(2_000) };
    check(config, |tape| in_task_order::<Static>(tape, PoolPlant::None));
}

/// The claimers' lists left in claimer order are caught.
#[test]
fn the_oracle_catches_results_left_in_claimer_order() {
    control(Config::cases(2_000), PoolPlant::None, PoolPlant::Concatenated, "not in task order", in_task_order::<Guided>);
}
