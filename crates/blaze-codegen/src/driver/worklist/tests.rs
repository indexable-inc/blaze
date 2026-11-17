use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use blaze_graph::{pool::Pool, topology::Topology};
use blaze_ids::rng::SplitMix64 as Rng;

use super::*;

/// A graph of `count` nodes, each with a few edges to any others, from `seed`: wide in places
/// and a chain in others, with cycles and nodes nothing reaches.
fn graph(count: usize, seed: u64) -> Vec<Vec<usize>> {
    let mut rng = Rng::new(seed);
    (0..count)
        .map(|at| {
            // A third of the nodes go on to the next alone: stretches of one item at a time,
            // where the jobs drain away, before the graph widens again.
            if rng.below(3) == 0 {
                return vec![(at + 1) % count];
            }
            (0..rng.below(6)).map(|_| rng.below(count as u64) as usize).collect()
        })
        .collect()
}

/// The reference model: what `roots` reach in `edges`, one node at a time.
fn reached(edges: &[Vec<usize>], roots: &[usize]) -> Vec<bool> {
    let mut seen = vec![false; edges.len()];
    let mut queue: Vec<usize> = roots.to_vec();
    while let Some(at) = queue.pop() {
        if !std::mem::replace(&mut seen[at], true) {
            queue.extend(&edges[at]);
        }
    }
    seen
}

/// Walks `edges` from `roots` on a worklist of a pool of `workers`, each node claimed once
/// by whoever reaches it first, unless `claims` is off (the planted bug: every reference is
/// pushed). How often each node was visited, and how many jobs the worklist started.
fn walk(
    edges: &[Vec<usize>],
    roots: &[usize],
    workers: usize,
    claims: bool,
) -> (Vec<usize>, usize) {
    let pool = Pool::new(&Topology::flat(workers), false).unwrap();
    pool.install(|| {
        let claimed: Vec<AtomicBool> = edges.iter().map(|_| AtomicBool::new(false)).collect();
        let visits: Vec<AtomicUsize> = edges.iter().map(|_| AtomicUsize::new(0)).collect();
        let work = Worklist::new();
        let claim = |at: usize| !claimed[at].swap(true, Ordering::AcqRel) || !claims;
        for &root in roots {
            if claim(root) {
                work.push(root);
            }
        }
        work.run(&|&at: &usize| {
            visits[at].fetch_add(1, Ordering::Relaxed);
            for &to in &edges[at] {
                if claim(to) {
                    work.push(to);
                }
            }
        });
        let (visited, started) = work.counts();
        let visits: Vec<usize> = visits.iter().map(|it| it.load(Ordering::Relaxed)).collect();
        assert_eq!(visited, visits.iter().sum::<usize>(), "the worklist's count of its visits");
        (visits, started)
    })
}

/// The invariant: a worklist visits every item pushed, by the caller or by a visit, exactly
/// once, and returns only when none is left; so a walk that claims each node once visits
/// exactly what the roots reach, whatever the pool and the schedule.
#[test]
fn a_walk_visits_what_its_roots_reach_each_once() {
    for seed in 0..24u64 {
        let edges = graph(2000, seed);
        let roots = [0, 7, 7, 1999];
        let want: Vec<usize> = reached(&edges, &roots).iter().map(|&it| usize::from(it)).collect();
        for workers in [1, 2, 8] {
            let (visits, started) = walk(&edges, &roots, workers, true);
            assert_eq!(visits, want, "seed {seed}, {workers} workers");
            if workers == 1 {
                assert_eq!(started, 0, "seed {seed}: one worker drains alone");
            }
        }
    }
}

/// The planted bug: a walk that pushes every reference with no claim. A node two nodes refer
/// to is then visited twice, which the oracle of [`a_walk_visits_what_its_roots_reach_each_once`]
/// (each reached node's visits are 1) reports.
#[test]
fn the_oracle_catches_a_node_pushed_by_each_that_reaches_it() {
    // A diamond: 0 refers to 1 and 2, both to 3.
    let edges = vec![vec![1, 2], vec![3], vec![3], vec![]];
    let (visits, _) = walk(&edges, &[0], 2, false);
    assert_eq!(visits, [1, 1, 1, 2], "3 is pushed by 1 and by 2");
    let want: Vec<usize> = reached(&edges, &[0]).iter().map(|&it| usize::from(it)).collect();
    assert_ne!(visits, want);
}

/// A wide program has more than one job: the worklist starts jobs while items wait, and
/// every job ends once the queue is empty.
#[test]
fn a_wide_walk_starts_jobs_and_a_chain_starts_none() {
    // One root that refers to 4000 leaves: once it is visited, 3999 wait past the one its
    // job takes next, and three workers are free.
    let mut wide: Vec<Vec<usize>> = vec![(1..=4000).collect()];
    wide.extend((0..4000).map(|_| Vec::new()));
    let pool = Pool::new(&Topology::flat(4), false).unwrap();
    let started = pool.install(|| {
        let claimed: Vec<AtomicBool> = wide.iter().map(|_| AtomicBool::new(false)).collect();
        let work = Worklist::new();
        work.push(0);
        work.run(&|&at: &usize| {
            for &to in &wide[at] {
                if !claimed[to].swap(true, Ordering::AcqRel) {
                    work.push(to);
                }
            }
        });
        assert!(claimed[1..].iter().all(|it| it.load(Ordering::Acquire)));
        work.counts().1
    });
    assert!(started >= 3, "4000 leaves waited behind one job on a pool of 4: {started} started");

    // A chain: one item at a time, so nothing waits past the one the job takes next.
    let chain: Vec<Vec<usize>> = (0..3000).map(|at| vec![(at + 1).min(2999)]).collect();
    let (visits, started) = walk(&chain, &[0], 4, true);
    assert!(visits.iter().all(|&it| it == 1));
    assert_eq!(started, 0, "a chain is one job's");
}

/// A visit's panic stops the taking of items and is raised on the caller once the jobs
/// ended, with its own payload (rule 20: it fails the caller's job, and nothing else).
#[test]
fn a_visits_panic_is_raised_on_the_caller() {
    for workers in [1, 4] {
        let pool = Pool::new(&Topology::flat(workers), false).unwrap();
        let (payload, visited) = pool.install(|| {
            let work = Worklist::new();
            for at in 0..10_000usize {
                work.push(at);
            }
            let ran = catch_unwind(AssertUnwindSafe(|| {
                work.run(&|&at: &usize| {
                    if at == 17 {
                        resume_unwind(Box::new("item 17 failed"));
                    }
                });
            }));
            let payload = ran.expect_err("the panic of item 17");
            assert!(work.halted());
            (*payload.downcast::<&str>().expect("the visit's own payload"), work.counts().0)
        });
        assert_eq!(payload, "item 17 failed", "{workers} workers");
        if workers == 1 {
            assert_eq!(visited, 18, "one worker takes no item once a visit panicked");
        }
    }
}

/// A halt (a build that stopped) ends the run with the visits under way: on one worker, with
/// the visit that halted.
#[test]
fn a_halt_ends_the_run_with_the_visits_under_way() {
    let pool = Pool::new(&Topology::flat(1), false).unwrap();
    let visited = pool.install(|| {
        let work = Worklist::new();
        for at in 0..10_000usize {
            work.push(at);
        }
        let seen = AtomicUsize::new(0);
        work.run(&|_: &usize| {
            if seen.fetch_add(1, Ordering::Relaxed) == 100 {
                work.halt();
            }
        });
        seen.into_inner()
    });
    assert_eq!(visited, 101, "no visit is begun past the one that halted");
}

/// A visit that gave way, on whichever thread, is not the worklist's panic: its item is
/// visited again by the caller once the queue is empty, and what that visit pushes is visited
/// too.
#[test]
fn a_visit_that_gave_way_runs_again() {
    const ITEMS: usize = 512;
    for workers in [1, 4] {
        let pool = Pool::new(&Topology::flat(workers), false).unwrap();
        let runs = pool.install(|| {
            let work = Worklist::new();
            for at in 0..ITEMS {
                work.push(at);
            }
            let runs: Vec<AtomicUsize> = (0..2 * ITEMS).map(|_| AtomicUsize::new(0)).collect();
            let gave: Vec<AtomicBool> = (0..ITEMS).map(|_| AtomicBool::new(false)).collect();
            work.run(&|&at: &usize| {
                runs[at].fetch_add(1, Ordering::Relaxed);
                if at >= ITEMS {
                    return;
                }
                // Every fifth item gives way the first time it is visited.
                if at % 5 == 0 && !gave[at].swap(true, Ordering::AcqRel) {
                    _ = pump::give_way();
                    resume_unwind(Box::new("gave way"));
                }
                // Each item pushes one more, a rerun's included.
                work.push(ITEMS + at);
            });
            runs.iter().map(|it| it.load(Ordering::Relaxed)).collect::<Vec<usize>>()
        });
        for at in 0..ITEMS {
            assert_eq!(runs[at], if at % 5 == 0 { 2 } else { 1 }, "{workers} workers: item {at}");
            assert_eq!(runs[ITEMS + at], 1, "{workers} workers: what item {at} pushed");
        }
    }
}
