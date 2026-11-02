//! Model-level ports of Miri's concurrency tests (`src/tools/miri/`, commit ad3d0bc141a0):
//!
//! - the unit tests of `src/concurrency/vector_clock.rs`, `src/data_structures/dedup_range_map.rs`
//!   and `src/data_structures/range_object_map.rs`;
//! - `tests/fail/data_race/*.rs`, `tests/fail/weak_memory/weak_uninit.rs` and
//!   `tests/pass/0weak_memory/*.rs`, each as the same thread steps and accesses run by [`Sim`],
//!   with the same verdict and, for races, Miri's message.
//!
//! A test program's threads are op lists; [`Sim`] runs them through [`Races::schedule`] the way
//! the evaluator will, one op per step. Miri's `-Zmiri-deterministic-concurrency` (fixed schedule,
//! no weak memory) is [`deterministic`], `-Zmiri-fixed-schedule` is [`fixed`]; a `sleep` that
//! orders threads in a Miri test is a yield here, which gives the same order under the fixed
//! round-robin schedule.

use std::{cmp::Ordering, collections::BTreeSet, fmt::Debug};

use blaze_ids::rng::SplitMix64;

use super::{
    Config, Deadlock, RaceError, RaceErrorKind, Races, Slot,
    range_map::{Overlap, RangeMap, RangeObjectMap},
    vclock::{VClock, VectorIdx},
    weak::STORE_BUFFER_LIMIT,
};
use crate::ids::{
    AccessKind, AllocId, AtomicOrd,
    AtomicOrd::{AcqRel, Acquire, Relaxed, Release, SeqCst},
    SiteId, Span, ThreadId,
};

use self::Op::*;

const MAIN: ThreadId = ThreadId::MAIN;
const S: SiteId = SiteId(7);

fn id(n: u32) -> AllocId {
    AllocId::new(n).unwrap()
}

// --- vector_clock.rs ---

#[test]
fn test_equal() {
    let mut c1 = VClock::default();
    let mut c2 = VClock::default();
    assert_eq!(c1, c2);
    c1.increment_index(VectorIdx(5), SiteId::START);
    assert_ne!(c1, c2);
    c2.increment_index(VectorIdx(53), SiteId::START);
    assert_ne!(c1, c2);
    c1.increment_index(VectorIdx(53), SiteId::START);
    assert_ne!(c1, c2);
    c2.increment_index(VectorIdx(5), SiteId::START);
    assert_eq!(c1, c2);
}

#[test]
fn test_partial_order() {
    // Small test
    assert_order(&[1], &[1], Some(Ordering::Equal));
    assert_order(&[1], &[2], Some(Ordering::Less));
    assert_order(&[2], &[1], Some(Ordering::Greater));
    assert_order(&[1], &[1, 2], Some(Ordering::Less));
    assert_order(&[2], &[1, 2], None);

    // Misc tests
    assert_order(&[400], &[0, 1], None);

    // Large test
    assert_order(
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0, 0, 0],
        Some(Ordering::Equal),
    );
    assert_order(
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0, 1, 0],
        Some(Ordering::Less),
    );
    assert_order(
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 11],
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0, 0, 0],
        Some(Ordering::Greater),
    );
    assert_order(
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 11],
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0, 1, 0],
        None,
    );
    assert_order(
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 9],
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0, 0, 0],
        Some(Ordering::Less),
    );
    assert_order(
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 9],
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0, 1, 0],
        Some(Ordering::Less),
    );
}

fn assert_order(l: &[u32], r: &[u32], o: Option<Ordering>) {
    let l = VClock::from_times(l);
    let r = VClock::from_times(r);

    // Test partial_cmp
    let compare = l.partial_cmp(&r);
    assert_eq!(compare, o, "Invalid comparison\n l: {l:?}\n r: {r:?}");
    let alt_compare = r.partial_cmp(&l);
    assert_eq!(alt_compare, o.map(Ordering::reverse), "Invalid alt comparison\n l: {l:?}\n r: {r:?}");

    // Test operators with faster implementations
    assert_eq!(matches!(compare, Some(Ordering::Less)), l < r, "Invalid (<):\n l: {l:?}\n r: {r:?}");
    assert_eq!(
        matches!(compare, Some(Ordering::Less) | Some(Ordering::Equal)),
        l <= r,
        "Invalid (<=):\n l: {l:?}\n r: {r:?}"
    );
    assert_eq!(matches!(compare, Some(Ordering::Greater)), l > r, "Invalid (>):\n l: {l:?}\n r: {r:?}");
    assert_eq!(
        matches!(compare, Some(Ordering::Greater) | Some(Ordering::Equal)),
        l >= r,
        "Invalid (>=):\n l: {l:?}\n r: {r:?}"
    );
    assert_eq!(matches!(alt_compare, Some(Ordering::Less)), r < l, "Invalid alt (<):\n l: {l:?}\n r: {r:?}");
    assert_eq!(
        matches!(alt_compare, Some(Ordering::Less) | Some(Ordering::Equal)),
        r <= l,
        "Invalid alt (<=):\n l: {l:?}\n r: {r:?}"
    );
    assert_eq!(matches!(alt_compare, Some(Ordering::Greater)), r > l, "Invalid alt (>):\n l: {l:?}\n r: {r:?}");
    assert_eq!(
        matches!(alt_compare, Some(Ordering::Greater) | Some(Ordering::Equal)),
        r >= l,
        "Invalid alt (>=):\n l: {l:?}\n r: {r:?}"
    );
}

#[test]
fn set_index_to_0() {
    let mut clock1 = VClock::from_times(&[0, 1, 2, 3]);
    let clock2 = VClock::from_times(&[0, 2, 3, 4, 0, 5]);
    // Naively, this would extend clock1 with a new index and set it to 0, making
    // the last index 0. Make sure that does not happen.
    clock1.set_at_index(&clock2, VectorIdx(4));
    // This must not have made the last element 0.
    assert_ne!(clock1.last_time(), Some(0));
}

#[test]
fn find_gt_index() {
    let l = VClock::from_times(&[1, 2, 3]);
    assert_eq!(l.find_gt_index(&VClock::from_times(&[1, 2, 3])), None);
    assert_eq!(l.find_gt_index(&VClock::from_times(&[1, 1, 3])), Some(VectorIdx(1)));
    assert_eq!(l.find_gt_index(&VClock::from_times(&[1, 2])), Some(VectorIdx(2)));
    assert_eq!(VClock::from_times(&[0, 0, 0, 4]).find_gt_index(&VClock::from_times(&[1])), Some(VectorIdx(3)));
}

// --- dedup_range_map.rs ---

/// Query the map at every offset in the range and collect the results.
fn to_vec(map: &RangeMap<i32>, offset: u64, len: u64) -> Vec<i32> {
    (offset..offset + len).map(|i| *map.iter(Span::new(i, 1)).next().unwrap().1).collect()
}

#[test]
fn basic_insert() {
    let mut map = RangeMap::<i32>::new(20, -1);
    // Insert.
    for (_, x) in map.iter_mut(Span::new(10, 1)) {
        *x = 42;
    }
    // Check.
    assert_eq!(to_vec(&map, 10, 1), vec![42]);
    assert_eq!(map.runs(), 3);

    // Insert with size 0.
    for (_, x) in map.iter_mut(Span::new(10, 0)) {
        *x = 19;
    }
    for (_, x) in map.iter_mut(Span::new(11, 0)) {
        *x = 19;
    }
    assert_eq!(to_vec(&map, 10, 2), vec![42, -1]);
    assert_eq!(map.runs(), 3);
}

#[test]
fn gaps() {
    let mut map = RangeMap::<i32>::new(20, -1);
    for (_, x) in map.iter_mut(Span::new(11, 1)) {
        *x = 42;
    }
    for (_, x) in map.iter_mut(Span::new(15, 1)) {
        *x = 43;
    }
    assert_eq!(map.runs(), 5);
    assert_eq!(to_vec(&map, 10, 10), vec![-1, 42, -1, -1, -1, 43, -1, -1, -1, -1]);

    for (_, x) in map.iter_mut(Span::new(10, 10)) {
        if *x < 42 {
            *x = 23;
        }
    }
    assert_eq!(map.runs(), 6);
    assert_eq!(to_vec(&map, 10, 10), vec![23, 42, 23, 23, 23, 43, 23, 23, 23, 23]);
    assert_eq!(to_vec(&map, 13, 5), vec![23, 23, 43, 23, 23]);

    for (_, x) in map.iter_mut(Span::new(15, 5)) {
        *x = 19;
    }
    assert_eq!(map.runs(), 6);
    assert_eq!(to_vec(&map, 10, 10), vec![23, 42, 23, 23, 23, 19, 19, 19, 19, 19]);
    // Should be seeing two blocks with 19.
    assert_eq!(map.iter(Span::new(15, 2)).map(|(_, &t)| t).collect::<Vec<_>>(), vec![19, 19]);

    // A NOP `iter_mut` should trigger merging.
    for _ in map.iter_mut(Span::new(15, 5)) {}
    assert_eq!(map.runs(), 5);
    assert_eq!(to_vec(&map, 10, 10), vec![23, 42, 23, 23, 23, 19, 19, 19, 19, 19]);
}

#[test]
#[should_panic]
fn out_of_range_iter_mut() {
    let mut map = RangeMap::<i32>::new(20, -1);
    let _ = map.iter_mut(Span::new(11, 11));
}

#[test]
#[should_panic]
fn out_of_range_iter() {
    let map = RangeMap::<i32>::new(20, -1);
    let _ = map.iter(Span::new(11, 11));
}

#[test]
fn empty_map_iter() {
    let map = RangeMap::<i32>::new(0, -1);
    let _ = map.iter(Span::new(0, 0));
}

#[test]
fn empty_map_iter_mut() {
    let mut map = RangeMap::<i32>::new(0, -1);
    let _ = map.iter_mut(Span::new(0, 0));
}

// --- range_object_map.rs ---

#[test]
fn empty_map() {
    let map = RangeObjectMap::<()>::default();
    // Correctly tells the access type along with the supposed position
    assert_eq!(map.overlap(Span::new(0, 4)), Overlap::Empty(0));
}

#[test]
#[should_panic]
fn no_overlapping_inserts() {
    let mut map = RangeObjectMap::<&str>::default();
    // |_|_|_|_|#|#|#|#|_|_|_|_|...
    //  0 1 2 3 4 5 6 7 8 9 a b c d
    map.insert_at_pos(0, Span::new(4, 4), "#");
    // |_|_|_|_|#|#|#|#|_|_|_|_|...
    //  0 ^ ^ ^ ^ 5 6 7 8 9 a b c d
    map.insert_at_pos(0, Span::new(1, 4), "@");
}

#[test]
fn boundaries() {
    let mut map = RangeObjectMap::<&str>::default();
    // |#|#|#|#|_|_|...
    //  0 1 2 3 4 5
    map.insert_at_pos(0, Span::new(0, 4), "#");
    // |#|#|#|#|_|_|_|_|_|...
    //  0 1 2 3 ^ ^ ^ ^ 8
    assert_eq!(map.overlap(Span::new(4, 4)), Overlap::Empty(1));
    // |#|#|#|#|_|_|_|_|@|@|@|@|_|_|...
    //  0 1 2 3 4 5 6 7 8 9 a b c d
    map.insert_at_pos(1, Span::new(8, 4), "@");
    // |#|#|#|#|_|_|_|_|@|@|@|@|_|_|...
    //  0 1 2 3 ^ ^ ^ ^ 8 9 a b c d
    assert_eq!(map.overlap(Span::new(4, 4)), Overlap::Empty(1));
    assert_eq!(map.len(), 2);
}

#[test]
fn perfectly_overlapping() {
    let mut map = RangeObjectMap::<&str>::default();
    // |#|#|#|#|_|_|...
    //  ^ ^ ^ ^ 4 5
    map.insert_at_pos(0, Span::new(0, 4), "#");
    assert_eq!(map.overlap(Span::new(0, 4)), Overlap::Perfect(0));
    // |#|#|#|#|@|@|@|@|_|...
    //  0 1 2 3 ^ ^ ^ ^ 8
    map.insert_at_pos(1, Span::new(4, 4), "@");
    assert_eq!(map.overlap(Span::new(4, 4)), Overlap::Perfect(1));
}

#[test]
fn straddling() {
    let mut map = RangeObjectMap::<&str>::default();
    // |_|_|_|_|#|#|#|#|_|_|_|_|...
    //  0 1 2 3 4 5 6 7 8 9 a b c d
    map.insert_at_pos(0, Span::new(4, 4), "#");
    // |_|_|_|_|#|#|#|#|_|_|_|_|...
    //  0 1 ^ ^ ^ ^ 6 7 8 9 a b c d
    assert_eq!(map.overlap(Span::new(2, 4)), Overlap::Imperfect(0..1));
    // |_|_|_|_|#|#|#|#|_|_|_|_|...
    //  0 1 2 3 4 5 ^ ^ ^ ^ a b c d
    assert_eq!(map.overlap(Span::new(6, 4)), Overlap::Imperfect(0..1));
    // |_|_|_|_|#|#|#|#|_|_|_|_|...
    //  0 1 ^ ^ ^ ^ ^ ^ ^ ^ a b c d
    assert_eq!(map.overlap(Span::new(2, 8)), Overlap::Imperfect(0..1));
    // |_|_|_|_|#|#|#|#|_|_|@|@|_|_|...
    //  0 1 2 3 4 5 6 7 8 9 a b c d
    map.insert_at_pos(1, Span::new(10, 2), "@");
    // |_|_|_|_|#|#|#|#|_|_|@|@|_|_|...
    //  0 1 2 3 4 5 ^ ^ ^ ^ ^ ^ ^ ^
    assert_eq!(map.overlap(Span::new(6, 8)), Overlap::Imperfect(0..2));
}

// --- the generator ---

#[test]
fn rng_is_seeded() {
    let draw = |seed| {
        let mut rng = SplitMix64::new(seed);
        (0..32).map(|_| rng.next_u64()).collect::<Vec<_>>()
    };
    assert_eq!(draw(3), draw(3));
    assert_ne!(draw(3), draw(4));

    let mut rng = SplitMix64::new(9);
    assert!((0..1000).all(|_| !rng.chance(0.0) && rng.chance(1.0)));
    assert!((0..1000).all(|_| rng.index(7) < 7));
    let hits = (0..10_000).filter(|_| rng.chance(0.5)).count();
    assert!((4_500..5_500).contains(&hits), "{hits}");
}

// --- the scheduler ---

#[test]
fn round_robin_and_yield() {
    let mut r = Races::new(&Config { preemption_rate: 0.0, ..Config::default() });
    let t1 = r.spawn();
    let t2 = r.spawn();
    let t3 = r.spawn();
    let all = [MAIN, t1, t2, t3];
    assert_eq!(r.schedule(MAIN, &all), Ok(MAIN));
    r.yield_active();
    assert_eq!(r.schedule(MAIN, &all), Ok(t1));
    assert_eq!(r.active(), t1);
    r.yield_active();
    assert_eq!(r.schedule(t1, &all), Ok(t2));
    // `t2` blocks: the next one after it runs.
    assert_eq!(r.schedule(t2, &[MAIN, t1, t3]), Ok(t3));
    r.yield_active();
    // Round robin wraps around.
    assert_eq!(r.schedule(t3, &[MAIN, t1, t3]), Ok(MAIN));
    // A yield with nobody else to run goes on.
    r.yield_active();
    assert_eq!(r.schedule(MAIN, &[MAIN]), Ok(MAIN));
    // The yield was used up.
    assert_eq!(r.schedule(MAIN, &all), Ok(MAIN));
}

#[test]
fn deadlock() {
    let mut r = Races::new(&Config::default());
    let t1 = r.spawn();
    let err: Deadlock = r.schedule(MAIN, &[]).unwrap_err();
    assert_eq!(err.threads, vec![MAIN, t1]);
    assert_eq!(err.to_string(), "the evaluated program deadlocked");

    // Two threads joining each other.
    let main: &[Op] = &[Spawn(1), Join(1)];
    let t1: &[Op] = &[Join(0)];
    match Sim::new(&deterministic(), &[main, t1]).run() {
        Err(Stop::Deadlock(err)) => assert_eq!(err.threads, vec![MAIN, ThreadId(1)]),
        other => panic!("expected a deadlock, got {other:?}"),
    }
}

fn decisions(seed: u64, preemption_rate: f64, steps: usize) -> Vec<ThreadId> {
    let mut r = Races::new(&Config { seed, preemption_rate, weak_memory: true });
    let t1 = r.spawn();
    let all = [MAIN, t1];
    let mut running = MAIN;
    (0..steps)
        .map(|_| {
            running = r.schedule(running, &all).unwrap();
            running
        })
        .collect()
}

#[test]
fn preemption_is_seeded() {
    assert_eq!(decisions(7, 0.3, 256), decisions(7, 0.3, 256));
    assert_ne!(decisions(7, 0.3, 256), decisions(8, 0.3, 256));
    assert!(decisions(1, 0.0, 256).iter().all(|&t| t == MAIN));
    assert!(decisions(1, 1.0, 256).windows(2).all(|w| w[0] != w[1]));
    // Miri's default rate switches about once in a hundred terminators.
    let switches = decisions(5, Config::default().preemption_rate, 20_000).windows(2).filter(|w| w[0] != w[1]).count();
    assert!((100..300).contains(&switches), "{switches}");
}

#[test]
fn spurious_failure_rate() {
    let mut r = Races::new(&Config { seed: 11, ..Config::default() });
    let failures = (0..10_000).filter(|_| r.spurious_failure()).count();
    assert!((7_600..8_400).contains(&failures), "{failures}");
}

// --- threads ---

#[test]
fn join_and_detach_errors() {
    let mut r = Races::new(&deterministic());
    let t1 = r.spawn();
    assert_eq!(r.join(MAIN).unwrap_err().to_string(), "trying to join itself");
    assert_eq!(r.join(t1).unwrap_err().kind, RaceErrorKind::JoinRunning(t1));
    assert_eq!(r.schedule(MAIN, &[t1]), Ok(t1));
    r.exit();
    assert_eq!(r.schedule(t1, &[MAIN]), Ok(MAIN));
    r.join(t1).unwrap();
    assert_eq!(r.join(t1).unwrap_err().to_string(), "trying to join an already joined thread");
    assert_eq!(r.detach(t1).unwrap_err().to_string(), "trying to detach thread that was already detached or joined");

    let t2 = r.spawn();
    r.detach(t2).unwrap();
    assert_eq!(r.detach(t2).unwrap_err().to_string(), "trying to detach thread that was already detached or joined");
    assert_eq!(r.join(t2).unwrap_err().to_string(), "trying to join a detached thread");
}

#[test]
fn names_and_termination() {
    let mut r = Races::new(&deterministic());
    let t1 = r.spawn();
    let t2 = r.spawn();
    r.set_thread_name(t2, b"worker");
    assert_eq!(r.thread_name(MAIN), Some("main"));
    assert_eq!(r.thread_name(t1), None);
    assert_eq!(r.thread_display_name(t1), "unnamed-1");
    assert_eq!(r.thread_display_name(t2), "worker");

    run_to_exit(&mut r, t1);
    // `main` returns while `t2` still runs: Miri's thread leak.
    r.exit();
    assert!(!r.all_terminated());
    assert_eq!(r.schedule(MAIN, &[t2]), Ok(t2));
    r.exit();
    assert!(r.all_terminated());
}

/// Runs `thread` to its exit and goes back to the main thread.
fn run_to_exit(r: &mut Races, thread: ThreadId) {
    assert_eq!(r.schedule(r.active(), &[thread]), Ok(thread));
    r.exit();
    assert_eq!(r.schedule(thread, &[MAIN]), Ok(MAIN));
}

#[test]
fn single_thread_fast_path() {
    let mut r = Races::new(&Config::default());
    let a = id(1);
    let span = Span::new(0, 8);
    r.new_alloc(a, 8, Some(MAIN));
    assert!(r.is_lazy(a));
    r.write(a, span, None, S).unwrap();
    r.write(a, span, Some(SeqCst), S).unwrap();
    r.store_value(a, span, SeqCst, &[1; 8], Some(&[0; 8]));
    r.read(a, span, Some(Acquire), S).unwrap();
    assert_eq!(r.load_value(a, span), Ok(None));
    // Nothing could race: no clocks and no store buffer were made.
    assert!(r.is_lazy(a));
    assert_eq!(r.buffer_count(a), 0);

    let t1 = r.spawn();
    assert!(r.is_multi_threaded());
    r.read(a, span, None, S).unwrap();
    assert!(!r.is_lazy(a));

    // Miri turns detection off at a join only when every thread's time is within the joined
    // thread's final clock (`thread_joined`), which never has the joiner's later steps.
    run_to_exit(&mut r, t1);
    r.join(t1).unwrap();
    assert!(r.is_multi_threaded());
}

#[test]
fn single_thread_again_when_the_joinee_saw_everything() {
    let mut r = Races::new(&deterministic());
    let t1 = r.spawn();
    run_to_exit(&mut r, t1);
    // The joined thread's final clock covering every thread's time is the condition.
    let main_clock = r.global.vector_clocks[0].clock.clone();
    r.global.vector_clocks[1].clock.join(&main_clock);
    r.join(t1).unwrap();
    assert!(!r.is_multi_threaded());
    // Nothing races now, and a new allocation needs no clocks.
    r.new_alloc(id(1), 4, Some(MAIN));
    assert!(r.is_lazy(id(1)));
}

#[test]
fn vector_index_reuse() {
    let mut r = Races::new(&deterministic());
    let t1 = r.spawn();
    assert_eq!(r.global.threads.vector_index[t1.index()], Some(VectorIdx(1)));
    run_to_exit(&mut r, t1);
    r.join(t1).unwrap();
    // The join acquires the clock from before the exit's own increment, as in Miri, so no
    // clock has caught up with `t1`'s last time: its index stays taken.
    let t2 = r.spawn();
    assert_eq!(r.global.threads.vector_index[t2.index()], Some(VectorIdx(2)));
    run_to_exit(&mut r, t2);
    // Once every clock that isn't an exited thread's has, the index is reused.
    let last = r.global.vector_clocks[2].clock.clone();
    r.global.vector_clocks[0].clock.join(&last);
    let t3 = r.spawn();
    assert_eq!(r.global.threads.vector_index[t3.index()], Some(VectorIdx(2)));
    assert_eq!(r.global.threads.vector_index[t2.index()], None);
    assert_eq!(r.global.vector_info[2], t3);
}

#[test]
fn sync_objects() {
    for acquire in [true, false] {
        let mut r = Races::new(&deterministic());
        let a = id(1);
        let span = Span::new(0, 4);
        r.new_alloc(a, 4, Some(MAIN));
        let t1 = r.spawn();
        assert_eq!(r.schedule(MAIN, &[t1]), Ok(t1));
        r.write(a, span, None, SiteId(1)).unwrap();
        // An unlock.
        r.release(0x1000);
        assert_eq!(r.schedule(t1, &[MAIN]), Ok(MAIN));
        // A lock, of this mutex or another one.
        r.acquire(if acquire { 0x1000 } else { 0x2000 });
        let res = r.write(a, span, None, SiteId(2));
        if acquire {
            res.unwrap();
        } else {
            let err = res.unwrap_err();
            assert_eq!(
                err.to_string(),
                "Data race detected between (1) non-atomic write on thread `unnamed-1` and (2) non-atomic write on thread `main` at alloc1"
            );
            assert_eq!(err.sites(), Some((SiteId(1), SiteId(2))));
        }
    }
}

#[test]
fn named_threads_and_retags() {
    let mut r = Races::new(&deterministic());
    let a = id(1);
    let span = Span::new(0, 4);
    r.new_alloc(a, 4, Some(MAIN));
    let t1 = r.spawn();
    r.set_thread_name(t1, b"worker");
    assert_eq!(r.schedule(MAIN, &[t1]), Ok(t1));
    r.write(a, span, None, S).unwrap();
    assert_eq!(r.schedule(t1, &[MAIN]), Ok(MAIN));
    let err = r.retag(a, span, AccessKind::Read, S).unwrap_err();
    assert_eq!(
        err.to_string(),
        "Data race detected between (1) non-atomic write on thread `worker` and (2) retag read on thread `main` at alloc1"
    );
    assert_eq!(err.help()[0], "retags occur on all (re)borrows and as well as when references are copied or moved");
}

#[test]
fn statics_exist_before_every_thread() {
    let mut r = Races::new(&deterministic());
    let t1 = r.spawn();
    // Made after the spawn, but by nobody: its "allocation" happens before everything.
    r.new_alloc(id(1), 4, None);
    // On a thread's stack, it is a write by that thread.
    r.new_alloc(id(2), 4, Some(MAIN));
    assert_eq!(r.schedule(MAIN, &[t1]), Ok(t1));
    r.write(id(1), Span::new(0, 4), None, S).unwrap();
    assert_eq!(
        r.write(id(2), Span::new(0, 4), None, S).unwrap_err().to_string(),
        "Data race detected between (1) creating a new allocation on thread `main` and (2) non-atomic write on thread `unnamed-1` at alloc2"
    );
}

#[test]
fn freed_memory_has_no_clocks() {
    let mut r = Races::new(&deterministic());
    let a = id(1);
    r.new_alloc(a, 8, Some(MAIN));
    let t1 = r.spawn();
    r.free(a, 8, S).unwrap();
    assert!(matches!(r.allocs[a.index()], Slot::Absent));
    // Memory reports the use after free; the race detector has nothing to say.
    assert_eq!(r.schedule(MAIN, &[t1]), Ok(t1));
    r.read(a, Span::new(0, 8), None, S).unwrap();
    r.write(a, Span::new(0, 8), Some(Relaxed), S).unwrap();
}

// --- a tiny evaluator: threads as op lists ---

/// A location: `size` bytes at `offset` in allocation `alloc` (counted from 1).
#[derive(Clone, Copy, Debug)]
struct Loc {
    alloc: u32,
    offset: u64,
    size: u64,
}

/// A 4-byte location at the start of `alloc`.
const fn at(alloc: u32) -> Loc {
    Loc { alloc, offset: 0, size: 4 }
}

const fn at_sized(alloc: u32, offset: u64, size: u64) -> Loc {
    Loc { alloc, offset, size }
}

impl Loc {
    fn id(self) -> AllocId {
        id(self.alloc)
    }

    fn span(self) -> Span {
        Span::new(self.offset, self.size)
    }
}

type Reg = usize;
const REGS: usize = 4;

#[derive(Clone, Copy, Debug)]
enum Op {
    /// A static: `size` bytes holding `value`, there before every thread.
    Static(u32, u64, u64),
    /// Stack or heap memory of the running thread, uninitialized.
    Alloc(u32, u64),
    Free(u32, u64),
    Write(Loc, u64),
    Read(Loc, Reg),
    Store(Loc, u64, AtomicOrd),
    Load(Loc, AtomicOrd, Reg),
    Swap(Loc, u64, AtomicOrd, Reg),
    FetchAdd(Loc, u64, AtomicOrd, Reg),
    /// `compare_exchange(expected, new, success, failure)`; the register gets the old value.
    Cxchg(Loc, u64, u64, AtomicOrd, AtomicOrd, Reg),
    Fence(AtomicOrd),
    /// `while loc.load(ord) != value { spin_loop() }`: each miss yields (Miri's `spin_loop`).
    SpinUntil(Loc, AtomicOrd, u64),
    /// `thread::yield_now`, or a `sleep` that lets the others run first.
    Yield,
    /// A thread running program `n`; threads are numbered in spawn order from 1.
    Spawn(usize),
    /// Blocks until the thread exits, then joins it.
    Join(u32),
    Detach(u32),
    /// Skips the next `n` ops unless the register holds the value.
    If(Reg, u64, usize),
}

/// Why a run ended before `main` returned.
#[derive(Debug)]
enum Stop {
    Race(RaceError),
    Deadlock(Deadlock),
    /// Miri: "memory access failed: ALLOC has been freed, so this pointer is dangling".
    Freed(u32),
    /// A read of uninitialized bytes.
    Uninit(u32),
}

struct Thread {
    prog: usize,
    pc: usize,
    regs: [u64; REGS],
    done: bool,
}

/// Runs thread programs against [`Races`] and a byte memory, one op per scheduling step.
struct Sim<'p> {
    races: Races,
    progs: &'p [&'p [Op]],
    threads: Vec<Thread>,
    /// By allocation index; `None` once freed, a `None` byte uninitialized.
    mem: Vec<Option<Vec<Option<u8>>>>,
}

/// The site of op `pc` of `thread`.
fn site(thread: ThreadId, pc: usize) -> SiteId {
    SiteId(1 + thread.0 * 1000 + pc as u32)
}

const STEP_LIMIT: usize = 1_000_000;

fn encode(value: u64, size: u64) -> Vec<u8> {
    value.to_le_bytes()[..size as usize].to_vec()
}

fn decode(bytes: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    b[..bytes.len()].copy_from_slice(bytes);
    u64::from_le_bytes(b)
}

impl<'p> Sim<'p> {
    /// `progs[0]` is `main`.
    fn new(config: &Config, progs: &'p [&'p [Op]]) -> Sim<'p> {
        Sim {
            races: Races::new(config),
            progs,
            threads: vec![Thread { prog: 0, pc: 0, regs: [0; REGS], done: false }],
            mem: Vec::new(),
        }
    }

    /// Runs until `main` returns: every thread's registers.
    fn run(&mut self) -> Result<Vec<[u64; REGS]>, Stop> {
        let mut running = MAIN;
        let mut runnable = Vec::new();
        for _ in 0..STEP_LIMIT {
            if self.threads[0].done {
                return Ok(self.threads.iter().map(|t| t.regs).collect());
            }
            runnable.clear();
            for (i, t) in self.threads.iter().enumerate() {
                if !t.done && !self.blocked(t) {
                    runnable.push(ThreadId(i as u32));
                }
            }
            running = self.races.schedule(running, &runnable).map_err(Stop::Deadlock)?;
            self.step(running)?;
        }
        panic!("no end after {STEP_LIMIT} steps");
    }

    fn blocked(&self, t: &Thread) -> bool {
        match self.progs[t.prog].get(t.pc) {
            Some(&Join(j)) => !self.threads[j as usize].done,
            _ => false,
        }
    }

    fn step(&mut self, t: ThreadId) -> Result<(), Stop> {
        let (prog, pc) = (self.threads[t.index()].prog, self.threads[t.index()].pc);
        let Some(&op) = self.progs[prog].get(pc) else {
            self.races.exit();
            self.threads[t.index()].done = true;
            return Ok(());
        };
        let site = site(t, pc);
        let mut advance = 1;
        match op {
            Static(a, size, value) => {
                self.races.new_alloc(id(a), size, None);
                self.alloc_mem(a, size);
                self.poke(at_sized(a, 0, size), value);
            }
            Alloc(a, size) => {
                self.races.new_alloc(id(a), size, Some(t));
                self.alloc_mem(a, size);
            }
            Free(a, size) => {
                self.live(a)?;
                self.races.free(id(a), size, site).map_err(Stop::Race)?;
                self.mem[a as usize - 1] = None;
            }
            Write(l, value) => {
                self.live(l.alloc)?;
                self.races.write(l.id(), l.span(), None, site).map_err(Stop::Race)?;
                self.poke(l, value);
            }
            Read(l, r) => {
                self.live(l.alloc)?;
                self.races.read(l.id(), l.span(), None, site).map_err(Stop::Race)?;
                let value = self.peek(l).ok_or(Stop::Uninit(l.alloc))?;
                self.set(t, r, value);
            }
            Store(l, value, ord) => self.store(l, value, ord, site)?,
            Load(l, ord, r) => {
                let value = self.load(l, ord, site)?;
                self.set(t, r, value);
            }
            Swap(l, value, ord, r) => {
                let old = self.rmw(l, ord, site, |_| value)?;
                self.set(t, r, old);
            }
            FetchAdd(l, value, ord, r) => {
                let old = self.rmw(l, ord, site, |old| old.wrapping_add(value))?;
                self.set(t, r, old);
            }
            Cxchg(l, expected, new, success, failure, r) => {
                self.live(l.alloc)?;
                // Miri reads the latest value without a race check, then checks the RMW or
                // the failed load (`atomic_compare_exchange_scalar`).
                let current = self.peek(l).ok_or(Stop::Uninit(l.alloc))?;
                if current == expected {
                    self.rmw(l, success, site, |_| new)?;
                } else {
                    self.races.failed_cxchg(l.id(), l.span(), failure, site).map_err(Stop::Race)?;
                }
                self.set(t, r, current);
            }
            Fence(ord) => self.races.fence(ord),
            SpinUntil(l, ord, value) => {
                if self.load(l, ord, site)? != value {
                    self.races.yield_active();
                    advance = 0;
                }
            }
            Yield => self.races.yield_active(),
            Spawn(p) => {
                let child = self.races.spawn();
                assert_eq!(child.index(), self.threads.len());
                self.threads.push(Thread { prog: p, pc: 0, regs: [0; REGS], done: false });
            }
            Join(j) => self.races.join(ThreadId(j)).map_err(Stop::Race)?,
            Detach(j) => self.races.detach(ThreadId(j)).map_err(Stop::Race)?,
            If(r, value, n) => {
                if self.threads[t.index()].regs[r] != value {
                    advance = 1 + n;
                }
            }
        }
        self.threads[t.index()].pc += advance;
        Ok(())
    }

    fn store(&mut self, l: Loc, value: u64, ord: AtomicOrd, site: SiteId) -> Result<(), Stop> {
        self.live(l.alloc)?;
        self.races.write(l.id(), l.span(), Some(ord), site).map_err(Stop::Race)?;
        let old = self.bytes(l);
        self.races.store_value(l.id(), l.span(), ord, &encode(value, l.size), old.as_deref());
        self.poke(l, value);
        Ok(())
    }

    fn load(&mut self, l: Loc, ord: AtomicOrd, site: SiteId) -> Result<u64, Stop> {
        self.live(l.alloc)?;
        self.races.read(l.id(), l.span(), Some(ord), site).map_err(Stop::Race)?;
        match self.races.load_value(l.id(), l.span()).map_err(Stop::Race)? {
            Some(bytes) => Ok(decode(bytes)),
            None => self.peek(l).ok_or(Stop::Uninit(l.alloc)),
        }
    }

    fn rmw(&mut self, l: Loc, ord: AtomicOrd, site: SiteId, f: impl FnOnce(u64) -> u64) -> Result<u64, Stop> {
        self.live(l.alloc)?;
        self.races.rmw(l.id(), l.span(), ord, site).map_err(Stop::Race)?;
        let old_bytes = self.bytes(l).ok_or(Stop::Uninit(l.alloc))?;
        let old = decode(&old_bytes);
        let new = f(old);
        self.races.store_value(l.id(), l.span(), ord, &encode(new, l.size), Some(&old_bytes));
        self.poke(l, new);
        Ok(old)
    }

    fn alloc_mem(&mut self, a: u32, size: u64) {
        let i = a as usize - 1;
        if self.mem.len() <= i {
            self.mem.resize(i + 1, None);
        }
        self.mem[i] = Some(vec![None; size as usize]);
    }

    fn live(&self, a: u32) -> Result<(), Stop> {
        match self.mem.get(a as usize - 1) {
            Some(Some(_)) => Ok(()),
            _ => Err(Stop::Freed(a)),
        }
    }

    /// The bytes at `l`; `None` unless all are initialized.
    fn bytes(&self, l: Loc) -> Option<Vec<u8>> {
        let mem = self.mem[l.alloc as usize - 1].as_ref().unwrap();
        mem[l.offset as usize..(l.offset + l.size) as usize].iter().copied().collect()
    }

    fn peek(&self, l: Loc) -> Option<u64> {
        self.bytes(l).map(|bytes| decode(&bytes))
    }

    fn poke(&mut self, l: Loc, value: u64) {
        let mem = self.mem[l.alloc as usize - 1].as_mut().unwrap();
        for (i, byte) in encode(value, l.size).into_iter().enumerate() {
            mem[l.offset as usize + i] = Some(byte);
        }
    }

    fn set(&mut self, t: ThreadId, r: Reg, value: u64) {
        self.threads[t.index()].regs[r] = value;
    }
}

/// Miri's `-Zmiri-deterministic-concurrency`: a fixed schedule and no weak memory.
fn deterministic() -> Config {
    Config { seed: 0, preemption_rate: 0.0, weak_memory: false }
}

/// Miri's `-Zmiri-fixed-schedule`: no preemption, weak memory on.
fn fixed(seed: u64) -> Config {
    Config { seed, preemption_rate: 0.0, weak_memory: true }
}

/// Miri's defaults with a seed.
fn seeded(seed: u64) -> Config {
    Config { seed, ..Config::default() }
}

/// Runs to the end; every thread's registers.
fn regs(config: &Config, progs: &[&[Op]]) -> Vec<[u64; REGS]> {
    Sim::new(config, progs).run().unwrap_or_else(|stop| panic!("the run stopped: {stop:?}"))
}

/// Runs under [`deterministic`] and expects a race.
fn race(progs: &[&[Op]]) -> RaceError {
    match Sim::new(&deterministic(), progs).run() {
        Err(Stop::Race(err)) => err,
        other => panic!("expected a race, got {other:?}"),
    }
}

/// Miri's `check_all_outcomes` (`tests/utils/mod.rs`): every run gives one of `expected`, and
/// each shows up within the runs Miri allows (fifty per outcome), one seed each.
fn check_all_outcomes<T: Ord + Debug + Copy>(expected: &[T], mut run: impl FnMut(u64) -> T) {
    let expected: BTreeSet<T> = expected.iter().copied().collect();
    let mut seen = BTreeSet::new();
    for seed in 0..(expected.len() * 50) as u64 {
        let value = run(seed);
        assert!(expected.contains(&value), "got an unexpected value: {value:?} (seed {seed})");
        seen.insert(value);
    }
    assert_eq!(seen, expected, "not every outcome was seen");
}

#[test]
fn sim_reports_sites() {
    let main: &[Op] = &[Alloc(1, 4), Write(at(1), 0), Spawn(1), Spawn(2), Join(1), Join(2)];
    let t1: &[Op] = &[Read(at(1), 0)];
    let t2: &[Op] = &[Write(at(1), 64)];
    let err = race(&[main, t1, t2]);
    assert_eq!(err.sites(), Some((site(ThreadId(1), 0), site(ThreadId(2), 0))));

    // An atomic access names itself too.
    let t1: &[Op] = &[Load(at(1), SeqCst, 0)];
    let err = race(&[main, t1, t2]);
    assert_eq!(err.sites(), Some((site(ThreadId(1), 0), site(ThreadId(2), 0))));
}

// --- tests/fail/data_race ---

/// The two threads every `atomic_*_race*.rs` test spawns, on an 8-byte `AtomicUsize`.
fn atomic_vs_na(t1: &[Op], t2: &[Op]) -> String {
    let main: &[Op] = &[Alloc(1, 8), Write(at_sized(1, 0, 8), 0), Spawn(1), Spawn(2), Join(1), Join(2)];
    race(&[main, t1, t2]).to_string()
}

const A8: Loc = at_sized(1, 0, 8);

#[test]
fn atomic_read_na_write_race1() {
    assert_eq!(
        atomic_vs_na(&[Write(A8, 32)], &[Load(A8, SeqCst, 0)]),
        "Data race detected between (1) non-atomic write on thread `unnamed-1` and (2) atomic load on thread `unnamed-2` at alloc1"
    );
}

#[test]
fn atomic_read_na_write_race2() {
    assert_eq!(
        atomic_vs_na(&[Load(A8, SeqCst, 0)], &[Write(A8, 32)]),
        "Data race detected between (1) atomic load on thread `unnamed-1` and (2) non-atomic write on thread `unnamed-2` at alloc1"
    );
}

#[test]
fn atomic_write_na_read_race1() {
    assert_eq!(
        atomic_vs_na(&[Store(A8, 32, SeqCst)], &[Read(A8, 0)]),
        "Data race detected between (1) atomic store on thread `unnamed-1` and (2) non-atomic read on thread `unnamed-2` at alloc1"
    );
}

#[test]
fn atomic_write_na_read_race2() {
    assert_eq!(
        atomic_vs_na(&[Read(A8, 0)], &[Store(A8, 32, SeqCst)]),
        "Data race detected between (1) non-atomic read on thread `unnamed-1` and (2) atomic store on thread `unnamed-2` at alloc1"
    );
}

#[test]
fn atomic_write_na_write_race1() {
    assert_eq!(
        atomic_vs_na(&[Write(A8, 32)], &[Store(A8, 64, SeqCst)]),
        "Data race detected between (1) non-atomic write on thread `unnamed-1` and (2) atomic store on thread `unnamed-2` at alloc1"
    );
}

#[test]
fn atomic_write_na_write_race2() {
    assert_eq!(
        atomic_vs_na(&[Store(A8, 64, SeqCst)], &[Write(A8, 32)]),
        "Data race detected between (1) atomic store on thread `unnamed-1` and (2) non-atomic write on thread `unnamed-2` at alloc1"
    );
}

#[test]
fn read_write_race() {
    let main: &[Op] = &[Alloc(1, 4), Write(at(1), 0), Spawn(1), Spawn(2), Join(1), Join(2)];
    assert_eq!(
        race(&[main, &[Read(at(1), 0)], &[Write(at(1), 64)]]).to_string(),
        "Data race detected between (1) non-atomic read on thread `unnamed-1` and (2) non-atomic write on thread `unnamed-2` at alloc1"
    );
}

#[test]
fn write_write_race() {
    let main: &[Op] = &[Alloc(1, 4), Write(at(1), 0), Spawn(1), Spawn(2), Join(1), Join(2)];
    assert_eq!(
        race(&[main, &[Write(at(1), 32)], &[Write(at(1), 64)]]).to_string(),
        "Data race detected between (1) non-atomic write on thread `unnamed-1` and (2) non-atomic write on thread `unnamed-2` at alloc1"
    );
}

/// `alloc_*_race.rs`: thread 1 allocates and publishes the pointer with a relaxed store,
/// thread 2 loads it relaxed and uses it.
fn alloc_race(use_it: Op) -> String {
    let main: &[Op] = &[Alloc(1, 8), Write(A8, 0), Spawn(1), Spawn(2), Join(1), Join(2)];
    let t1: &[Op] = &[Alloc(2, 8), Store(A8, 2, Relaxed)];
    let t2: &[Op] = &[Load(A8, Relaxed, 0), If(0, 2, 1), use_it];
    race(&[main, t1, t2]).to_string()
}

#[test]
fn alloc_read_race() {
    assert_eq!(
        alloc_race(Read(at_sized(2, 0, 8), 1)),
        "Data race detected between (1) creating a new allocation on thread `unnamed-1` and (2) non-atomic read on thread `unnamed-2` at alloc2"
    );
}

#[test]
fn alloc_write_race() {
    assert_eq!(
        alloc_race(Write(at_sized(2, 0, 8), 2)),
        "Data race detected between (1) creating a new allocation on thread `unnamed-1` and (2) non-atomic write on thread `unnamed-2` at alloc2"
    );
}

/// `dealloc_*_race{1,2}.rs`: a `Box<usize>` made by `main`, used and freed by two threads.
fn dealloc_race(t1: &[Op], t2: &[Op]) -> Result<Vec<[u64; REGS]>, Stop> {
    let main: &[Op] = &[Alloc(1, 8), Write(A8, 0), Spawn(1), Spawn(2), Join(1), Join(2)];
    Sim::new(&deterministic(), &[main, t1, t2]).run()
}

#[test]
fn dealloc_read_race1() {
    match dealloc_race(&[Read(A8, 0)], &[Free(1, 8)]) {
        Err(Stop::Race(err)) => assert_eq!(
            err.to_string(),
            "Data race detected between (1) non-atomic read on thread `unnamed-1` and (2) deallocation on thread `unnamed-2` at alloc1"
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn dealloc_read_race2() {
    // Miri: "has been freed", from memory, not the race detector.
    assert!(matches!(dealloc_race(&[Free(1, 8)], &[Read(A8, 0)]), Err(Stop::Freed(1))));
}

#[test]
fn dealloc_write_race1() {
    match dealloc_race(&[Write(A8, 2)], &[Free(1, 8)]) {
        Err(Stop::Race(err)) => assert_eq!(
            err.to_string(),
            "Data race detected between (1) non-atomic write on thread `unnamed-1` and (2) deallocation on thread `unnamed-2` at alloc1"
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn dealloc_write_race2() {
    assert!(matches!(dealloc_race(&[Free(1, 8)], &[Write(A8, 2)]), Err(Stop::Freed(1))));
}

/// The `*_race_stack.rs` tests: thread 1 publishes a stack variable with a release store,
/// sleeps, then does `last`; thread 2 acquires the pointer and does `use_it`.
fn stack_race(last: Op, use_it: Op) -> String {
    let main: &[Op] = &[Alloc(1, 8), Write(A8, 0), Spawn(1), Spawn(2), Join(1), Join(2)];
    let s = at_sized(2, 0, 8);
    let t1: &[Op] = &[Alloc(2, 8), Write(s, 0), Store(A8, 2, Release), Yield, last];
    let t2: &[Op] = &[Load(A8, Acquire, 0), If(0, 2, 1), use_it];
    race(&[main, t1, t2]).to_string()
}

#[test]
fn dealloc_read_race_stack() {
    assert_eq!(
        stack_race(Free(2, 8), Read(at_sized(2, 0, 8), 1)),
        "Data race detected between (1) non-atomic read on thread `unnamed-2` and (2) deallocation on thread `unnamed-1` at alloc2"
    );
}

#[test]
fn dealloc_write_race_stack() {
    assert_eq!(
        stack_race(Free(2, 8), Write(at_sized(2, 0, 8), 3)),
        "Data race detected between (1) non-atomic write on thread `unnamed-2` and (2) deallocation on thread `unnamed-1` at alloc2"
    );
}

#[test]
fn read_write_race_stack() {
    assert_eq!(
        stack_race(Read(at_sized(2, 0, 8), 0), Write(at_sized(2, 0, 8), 3)),
        "Data race detected between (1) non-atomic write on thread `unnamed-2` and (2) non-atomic read on thread `unnamed-1` at alloc2"
    );
}

#[test]
fn write_write_race_stack() {
    assert_eq!(
        stack_race(Write(at_sized(2, 0, 8), 1), Write(at_sized(2, 0, 8), 3)),
        "Data race detected between (1) non-atomic write on thread `unnamed-2` and (2) non-atomic write on thread `unnamed-1` at alloc2"
    );
}

#[test]
fn dangling_thread_race() {
    // Thread 1 is detached and runs to its end while `main` sleeps; spawning and joining
    // another thread must not turn race detection off.
    let main: &[Op] =
        &[Alloc(1, 4), Write(at(1), 0), Spawn(1), Detach(1), Yield, Spawn(2), Join(2), Write(at(1), 64)];
    assert_eq!(
        race(&[main, &[Write(at(1), 32)], &[]]).to_string(),
        "Data race detected between (1) non-atomic write on thread `unnamed-1` and (2) non-atomic write on thread `main` at alloc1"
    );
}

#[test]
fn dangling_thread_async_race() {
    let main: &[Op] =
        &[Alloc(1, 4), Write(at(1), 0), Spawn(1), Detach(1), Yield, Spawn(2), Join(2), Spawn(3), Join(3)];
    assert_eq!(
        race(&[main, &[Write(at(1), 32)], &[], &[Write(at(1), 64)]]).to_string(),
        "Data race detected between (1) non-atomic write on thread `unnamed-1` and (2) non-atomic write on thread `unnamed-3` at alloc1"
    );
}

#[test]
fn enable_after_join_to_main() {
    // Four threads joined turn detection off; the next spawn turns it back on, with reused
    // vector indices.
    let main: &[Op] = &[
        Spawn(1),
        Spawn(1),
        Spawn(1),
        Spawn(1),
        Join(1),
        Join(2),
        Join(3),
        Join(4),
        Alloc(1, 4),
        Write(at(1), 0),
        Spawn(2),
        Spawn(3),
        Join(5),
        Join(6),
    ];
    assert_eq!(
        race(&[main, &[], &[Write(at(1), 32)], &[Write(at(1), 64)]]).to_string(),
        "Data race detected between (1) non-atomic write on thread `unnamed-5` and (2) non-atomic write on thread `unnamed-6` at alloc1"
    );
}

#[test]
fn fence_after_load() {
    // `V` is a static; `a` is an `Arc<AtomicUsize>`. An SC fence and a relaxed load of the
    // value an SC store wrote don't synchronize.
    let v = at(1);
    let a = at_sized(2, 0, 8);
    let main: &[Op] =
        &[Static(1, 4, 0), Alloc(2, 8), Write(a, 0), Spawn(1), Yield, Fence(SeqCst), Load(a, Relaxed, 0), Write(v, 2)];
    let t1: &[Op] = &[Write(v, 1), Store(a, 1, SeqCst)];
    assert_eq!(
        race(&[main, t1]).to_string(),
        "Data race detected between (1) non-atomic write on thread `unnamed-1` and (2) non-atomic write on thread `main` at alloc1"
    );
}

/// The thread of the `local_variable_*_race.rs` tests: spins until `P` (a static) is set, then
/// writes through it.
const LOCAL_SPINNER: &[Op] =
    &[SpinUntil(at_sized(1, 0, 8), Relaxed, 2), Load(at_sized(1, 0, 8), Relaxed, 0), Write(at_sized(2, 0, 1), 127)];

#[test]
fn local_variable_alloc_race() {
    let p = at_sized(1, 0, 8);
    let main: &[Op] = &[Static(1, 8, 0), Spawn(1), Alloc(2, 1), Store(p, 2, Relaxed), Join(1)];
    assert_eq!(
        race(&[main, LOCAL_SPINNER]).to_string(),
        "Data race detected between (1) creating a new allocation on thread `main` and (2) non-atomic write on thread `unnamed-1` at alloc2"
    );
}

#[test]
fn local_variable_read_race() {
    let p = at_sized(1, 0, 8);
    let val = at_sized(2, 0, 1);
    let main: &[Op] =
        &[Static(1, 8, 0), Alloc(2, 1), Write(val, 0), Spawn(1), Read(val, 0), Store(p, 2, Relaxed), Join(1)];
    assert_eq!(
        race(&[main, LOCAL_SPINNER]).to_string(),
        "Data race detected between (1) non-atomic read on thread `main` and (2) non-atomic write on thread `unnamed-1` at alloc2"
    );
}

#[test]
fn local_variable_write_race() {
    let p = at_sized(1, 0, 8);
    let main: &[Op] =
        &[Static(1, 8, 0), Spawn(1), Alloc(2, 1), Write(at_sized(2, 0, 1), 0), Store(p, 2, Relaxed), Join(1)];
    assert_eq!(
        race(&[main, LOCAL_SPINNER]).to_string(),
        "Data race detected between (1) non-atomic write on thread `main` and (2) non-atomic write on thread `unnamed-1` at alloc2"
    );
}

#[test]
fn stack_pop_race() {
    // The argument's memory is freed when `race` returns, after the other thread read it.
    let main: &[Op] = &[Alloc(1, 4), Write(at(1), 0), Spawn(1), Yield, Free(1, 4)];
    assert_eq!(
        race(&[main, &[Read(at(1), 0)]]).to_string(),
        "Data race detected between (1) non-atomic read on thread `unnamed-1` and (2) deallocation on thread `main` at alloc1"
    );
}

/// The `SYNC` static and the variable `a` of the release-sequence tests.
const SYNC: Loc = at_sized(1, 0, 8);
const VAR: Loc = at(2);
const SYNC_MAIN: &[Op] =
    &[Static(1, 8, 0), Alloc(2, 4), Write(VAR, 0), Spawn(1), Spawn(2), Spawn(3), Join(1), Join(2), Join(3)];

#[test]
fn relax_acquire_race() {
    let t1: &[Op] = &[Write(VAR, 1), Store(SYNC, 1, Release)];
    let t2: &[Op] = &[Load(SYNC, Acquire, 0), If(0, 1, 1), Store(SYNC, 2, Relaxed)];
    let t3: &[Op] = &[Load(SYNC, Acquire, 0), If(0, 2, 1), Read(VAR, 1)];
    assert_eq!(
        race(&[SYNC_MAIN, t1, t2, t3]).to_string(),
        "Data race detected between (1) non-atomic write on thread `unnamed-1` and (2) non-atomic read on thread `unnamed-3` at alloc2"
    );
}

#[test]
fn release_seq_race() {
    let t1: &[Op] = &[Write(VAR, 1), Store(SYNC, 1, Release), Yield, Store(SYNC, 3, Relaxed)];
    let t2: &[Op] = &[Store(SYNC, 2, Relaxed)];
    let t3: &[Op] = &[Yield, Load(SYNC, Acquire, 0), If(0, 3, 1), Read(VAR, 1)];
    assert_eq!(
        race(&[SYNC_MAIN, t1, t2, t3]).to_string(),
        "Data race detected between (1) non-atomic write on thread `unnamed-1` and (2) non-atomic read on thread `unnamed-3` at alloc2"
    );
}

#[test]
fn release_seq_race_same_thread() {
    let main: &[Op] = &[Static(1, 8, 0), Alloc(2, 4), Write(VAR, 0), Spawn(1), Spawn(2), Join(1), Join(2)];
    // Since C++20 the relaxed store ends the release sequence, though on the same thread.
    let t1: &[Op] = &[Write(VAR, 1), Store(SYNC, 1, Release), Store(SYNC, 2, Relaxed)];
    let t2: &[Op] = &[Load(SYNC, Acquire, 0), If(0, 2, 1), Read(VAR, 1)];
    assert_eq!(
        race(&[main, t1, t2]).to_string(),
        "Data race detected between (1) non-atomic write on thread `unnamed-1` and (2) non-atomic read on thread `unnamed-2` at alloc2"
    );
}

#[test]
fn rmw_race() {
    let t1: &[Op] = &[Write(VAR, 1), Store(SYNC, 1, Release)];
    let t2: &[Op] = &[Swap(SYNC, 2, Relaxed, 0), If(0, 1, 1), Store(SYNC, 3, Relaxed)];
    let t3: &[Op] = &[Load(SYNC, Acquire, 0), If(0, 3, 1), Read(VAR, 1)];
    assert_eq!(
        race(&[SYNC_MAIN, t1, t2, t3]).to_string(),
        "Data race detected between (1) non-atomic write on thread `unnamed-1` and (2) non-atomic read on thread `unnamed-3` at alloc2"
    );
}

/// The `mixed_size_*.rs` tests: an `AtomicU16` also used as two `AtomicU8`, inside
/// `thread::scope`.
const A16: Loc = at_sized(1, 0, 2);

fn mixed_size(threads: &[&[Op]]) -> RaceError {
    let mut main = vec![Alloc(1, 2), Write(A16, 0)];
    for n in 1..=threads.len() {
        main.push(Spawn(n));
    }
    for n in 1..=threads.len() {
        main.push(Join(n as u32));
    }
    let mut progs = vec![main.as_slice()];
    progs.extend_from_slice(threads);
    let err = race(&progs);
    assert_eq!(err.help()[0], "overlapping unsynchronized atomic accesses must use the same access size");
    err
}

#[test]
fn mixed_size_write_write() {
    for (idx, at_offset) in [(0, ""), (1, "+0x1")] {
        let err = mixed_size(&[&[Store(A16, 1, SeqCst)], &[Store(at_sized(1, idx, 1), 1, SeqCst)]]);
        assert_eq!(
            err.to_string(),
            format!(
                "Race condition detected between (1) 2-byte atomic store on thread `unnamed-1` and (2) 1-byte atomic store on thread `unnamed-2` at alloc1{at_offset}"
            )
        );
    }
}

#[test]
fn mixed_size_read_write() {
    let a8 = at_sized(1, 0, 1);
    // read_write
    let err = mixed_size(&[&[Yield, Store(A16, 1, SeqCst)], &[Load(a8, SeqCst, 0)]]);
    assert_eq!(
        err.to_string(),
        "Race condition detected between (1) 1-byte atomic load on thread `unnamed-2` and (2) 2-byte atomic store on thread `unnamed-1` at alloc1"
    );
    // write_read
    let err = mixed_size(&[&[Store(A16, 1, SeqCst)], &[Yield, Load(a8, SeqCst, 0)]]);
    assert_eq!(
        err.to_string(),
        "Race condition detected between (1) 2-byte atomic store on thread `unnamed-1` and (2) 1-byte atomic load on thread `unnamed-2` at alloc1"
    );
}

#[test]
fn mixed_size_read_read_write() {
    let a8 = at_sized(1, 0, 1);
    // match_first_load
    let err = mixed_size(&[&[Load(A16, SeqCst, 0)], &[Load(a8, SeqCst, 0)], &[Yield, Store(A16, 0, SeqCst)]]);
    assert_eq!(
        err.to_string(),
        "Race condition detected between (1) multiple differently-sized atomic loads, including one load on thread `unnamed-1` and (2) 2-byte atomic store on thread `unnamed-3` at alloc1"
    );
    // match_second_load
    let err = mixed_size(&[&[Load(A16, SeqCst, 0)], &[Load(a8, SeqCst, 0)], &[Yield, Store(a8, 0, SeqCst)]]);
    assert_eq!(
        err.to_string(),
        "Race condition detected between (1) multiple differently-sized atomic loads, including one load on thread `unnamed-1` and (2) 1-byte atomic store on thread `unnamed-3` at alloc1"
    );
}

#[test]
fn mixed_size_read_write_read() {
    // A non-atomic read of all four bytes, a 1-byte compare-exchange of the first, then a
    // 4-byte atomic load on another thread.
    let main: &[Op] = &[Alloc(1, 4), Write(at(1), 0), Spawn(1), Spawn(2), Join(1), Join(2)];
    let t1: &[Op] = &[Read(at(1), 0), Cxchg(at_sized(1, 0, 1), 0, 1, Relaxed, Relaxed, 1), Yield];
    let t2: &[Op] = &[Load(at(1), Relaxed, 0)];
    assert_eq!(
        race(&[main, t1, t2]).to_string(),
        "Race condition detected between (1) 1-byte atomic store on thread `unnamed-1` and (2) 4-byte atomic load on thread `unnamed-2` at alloc1"
    );
}

// --- tests/fail/weak_memory ---

#[test]
fn weak_uninit() {
    let x = at_sized(1, 0, 8);
    let main: &[Op] = &[Alloc(1, 8), Spawn(1), Spawn(2), Join(1), Join(2)];
    let t1: &[Op] = &[Store(x, 1, Relaxed)];
    let t2: &[Op] = &[Load(x, Relaxed, 0)];
    // If we try often enough, we should hit UB.
    let mut ub = 0;
    for seed in 0..100 {
        match Sim::new(&fixed(seed), &[main, t1, t2]).run() {
            Ok(regs) => assert_eq!(regs[2][0], 1),
            Err(Stop::Race(err)) => {
                assert_eq!(err.kind, RaceErrorKind::UninitLoad { alloc: id(1), offset: 0 });
                assert_eq!(err.to_string(), "using uninitialized data, but this operation requires initialized memory");
                ub += 1;
            }
            Err(other) => panic!("{other:?}"),
        }
    }
    assert!(ub > 0);
}

// --- tests/pass/0weak_memory/weak.rs (`-Zmiri-fixed-schedule`) ---

/// `static_atomic(value)` for each value: a leaked box made by `main`, `n`th from 1.
fn static_atomics(values: &[u64]) -> Vec<Op> {
    let mut ops = Vec::new();
    for (i, &value) in values.iter().enumerate() {
        let n = i as u32 + 1;
        ops.push(Alloc(n, 4));
        ops.push(Write(at(n), value));
    }
    ops
}

/// `main` making `atomics`, spawning `threads` threads and joining them in order.
fn spawn_join(atomics: &[u64], threads: usize) -> Vec<Op> {
    let mut main = static_atomics(atomics);
    main.extend((1..=threads).map(Spawn));
    main.extend((1..=threads as u32).map(Join));
    main
}

const X: Loc = at(1);
const Y: Loc = at(2);
const Z: Loc = at(3);
const K: Loc = at(4);

/// What thread `t` loaded into register 1, if register 0 holds `value`.
fn if_then(regs: &[[u64; REGS]], t: usize, value: u64) -> Option<u64> {
    (regs[t][0] == value).then_some(regs[t][1])
}

#[test]
fn weak_relaxed() {
    let main = spawn_join(&[0], 2);
    let t1: &[Op] = &[Store(X, 1, Relaxed), Store(X, 2, Relaxed)];
    let t2: &[Op] = &[Load(X, Relaxed, 0)];
    check_all_outcomes(&[0, 1, 2], |seed| regs(&fixed(seed), &[&main, t1, t2])[2][0]);
}

#[test]
fn weak_seq_cst() {
    // POPL 2017, Figure 8: even though t3 runs last it can see 1, and never 2.
    let main = spawn_join(&[0], 3);
    let t1: &[Op] = &[Store(X, 1, Relaxed)];
    let t2: &[Op] = &[Store(X, 2, SeqCst), Store(X, 3, SeqCst)];
    let t3: &[Op] = &[Load(X, SeqCst, 0)];
    check_all_outcomes(&[1, 3], |seed| regs(&fixed(seed), &[&main, t1, t2, t3])[3][0]);
}

#[test]
fn weak_initialization_write() {
    for add_fence in [false, true] {
        let mut main = static_atomics(&[11]);
        if add_fence {
            // Make the location atomic and non-atomic again.
            main.push(Store(X, 99, Relaxed));
            main.push(Write(X, 11));
        }
        main.extend([Alloc(2, 4), Write(Y, 0), Spawn(1), Spawn(2), Join(1), Join(2)]);
        // Reading `x` non-atomically must not delete its store buffer.
        let t1: &[Op] = &[Store(X, 22, Relaxed), Read(X, 1), Store(Y, 1, Relaxed)];
        let t2: Vec<Op> = if add_fence {
            vec![SpinUntil(Y, Relaxed, 1), Fence(AcqRel), Load(X, Relaxed, 0)]
        } else {
            vec![SpinUntil(Y, Relaxed, 1), Load(X, Relaxed, 0)]
        };
        check_all_outcomes(&[11, 22], |seed| regs(&fixed(seed), &[&main, t1, &t2])[2][0]);
    }
}

/// `rdmw` of `faa_replaced_by_load` and `test_sync_through_rmw_and_fences`; `fetch_add(0)`
/// or a load as the middle access.
fn rdmw(storing: Loc, sync: Loc, loading: Loc, faa: bool) -> [Op; 5] {
    [
        Store(storing, 1, Relaxed),
        Fence(Release),
        if faa { FetchAdd(sync, 0, Relaxed, 1) } else { Load(sync, Relaxed, 1) },
        Fence(Acquire),
        Load(loading, Relaxed, 0),
    ]
}

#[test]
fn weak_faa_replaced_by_load() {
    let main = spawn_join(&[0, 0, 0], 2);
    let t1 = rdmw(Y, X, Z, false);
    let t2 = rdmw(Z, X, Y, false);
    check_all_outcomes(&[true, false], |seed| {
        let regs = regs(&fixed(seed), &[&main, &t1, &t2]);
        (regs[1][0], regs[2][0]) == (0, 0)
    });
}

#[test]
fn weak_weaker_release_sequences() {
    // P0982R0: in C++20 the relaxed store of 3 is not in the release sequence of the store
    // of 1, so `Some(0)` can be seen.
    let main = spawn_join(&[0, 0], 3);
    let t1: &[Op] = &[Store(X, 2, Relaxed)];
    let t2: &[Op] = &[Store(Y, 1, Relaxed), Store(X, 1, Release), Store(X, 3, Relaxed)];
    let t3: &[Op] = &[Load(X, Acquire, 0), If(0, 3, 1), Load(Y, Relaxed, 1)];
    check_all_outcomes(&[None, Some(0), Some(1)], |seed| if_then(&regs(&fixed(seed), &[&main, t1, t2, t3]), 3, 3));
}

#[test]
fn weak_release_sequence() {
    // An RMW still continues a release sequence.
    let main = spawn_join(&[0, 0], 2);
    let t1: &[Op] = &[Store(Y, 1, Relaxed), Store(X, 1, Release), Swap(X, 3, Relaxed, 2)];
    let t2: &[Op] = &[Load(X, Acquire, 0), If(0, 3, 1), Load(Y, Relaxed, 1)];
    check_all_outcomes(&[None, Some(1)], |seed| if_then(&regs(&fixed(seed), &[&main, t1, t2]), 2, 3));
}

#[test]
fn weak_old_release_store() {
    // Reading an outdated release store acquires its clock.
    let main = spawn_join(&[0, 0], 2);
    let t1: &[Op] = &[Store(Y, 1, Relaxed), Store(X, 1, Release), Store(X, 3, Relaxed)];
    let t2: &[Op] = &[Load(X, Acquire, 0), If(0, 1, 1), Load(Y, Relaxed, 1)];
    check_all_outcomes(&[None, Some(1)], |seed| if_then(&regs(&fixed(seed), &[&main, t1, t2]), 2, 1));
}

// --- tests/pass/0weak_memory/consistency.rs (Miri's default schedule) ---

/// Fifty runs, as Miri's `main` does.
fn fifty(mut test: impl FnMut(&Config)) {
    for seed in 0..50 {
        test(&seeded(seed));
    }
}

#[test]
fn consistency_corr() {
    fifty(|config| {
        let main = spawn_join(&[0, 0], 3);
        let t1: &[Op] = &[Store(X, 1, Relaxed), Store(X, 2, Relaxed)];
        let t2: &[Op] = &[Load(X, Relaxed, 0), Store(Y, 1, Release)];
        let t3: &[Op] = &[SpinUntil(Y, Acquire, 1), Load(X, Relaxed, 0)];
        let regs = regs(config, &[&main, t1, t2, t3]);
        if regs[2][0] == 2 {
            assert_eq!(regs[3][0], 2);
        }
    });
}

#[test]
fn consistency_wrc() {
    fifty(|config| {
        let main = spawn_join(&[0, 0], 3);
        let t1: &[Op] = &[Store(X, 1, Release)];
        let t2: &[Op] = &[SpinUntil(X, Acquire, 1), Store(Y, 1, Release)];
        let t3: &[Op] = &[SpinUntil(Y, Acquire, 1), Load(X, Relaxed, 0)];
        assert_eq!(regs(config, &[&main, t1, t2, t3])[3][0], 1);
    });
}

#[test]
fn consistency_message_passing() {
    fifty(|config| {
        // `var` is a plain `u32` on main's stack, `y` an atomic.
        let main: &[Op] = &[Alloc(1, 4), Write(X, 0), Alloc(2, 4), Write(Y, 0), Spawn(1), Spawn(2), Join(1), Join(2)];
        let t1: &[Op] = &[Write(X, 1), Store(Y, 1, Release)];
        let t2: &[Op] = &[SpinUntil(Y, Acquire, 1), Read(X, 0)];
        assert_eq!(regs(config, &[main, t1, t2])[2][0], 1);
    });
}

#[test]
fn consistency_load_buffering_acq_rel() {
    fifty(|config| {
        let main = spawn_join(&[0, 0], 2);
        let t1: &[Op] = &[Load(X, Acquire, 0), Store(Y, 1, Release)];
        let t2: &[Op] = &[Load(Y, Acquire, 0), Store(X, 1, Release)];
        let regs = regs(config, &[&main, t1, t2]);
        assert_ne!((regs[1][0], regs[2][0]), (1, 1));
    });
}

#[test]
fn consistency_mixed_access() {
    fifty(|config| {
        let mut main = static_atomics(&[0]);
        main.extend([Spawn(1), Join(1), Store(X, 2, Relaxed), Spawn(2), Join(2)]);
        let t1: &[Op] = &[Store(X, 1, Relaxed)];
        let t2: &[Op] = &[Load(X, Relaxed, 0)];
        assert_eq!(regs(config, &[&main, t1, t2])[2][0], 2);
    });
}

#[test]
fn consistency_single_thread() {
    fifty(|config| {
        let main: &[Op] =
            &[Alloc(1, 4), Write(X, 42), Load(X, Relaxed, 0), Store(X, 43, Relaxed), Load(X, Relaxed, 1)];
        assert_eq!(regs(config, &[main])[0][..2], [42, 43]);
    });
}

#[test]
fn consistency_sync_through_rmw_and_fences() {
    fifty(|config| {
        // x, y, z, and `go` as the fourth.
        let mut main = static_atomics(&[0, 0, 0, 0]);
        main.extend([Spawn(1), Spawn(2), Store(K, 1, Relaxed), Join(1), Join(2)]);
        let mut t1 = vec![SpinUntil(K, Relaxed, 1)];
        t1.extend(rdmw(Y, X, Z, true));
        let mut t2 = vec![SpinUntil(K, Relaxed, 1)];
        t2.extend(rdmw(Z, X, Y, true));
        let regs = regs(config, &[&main, &t1, &t2]);
        assert_ne!((regs[1][0], regs[2][0]), (0, 0));
    });
}

// --- tests/pass/0weak_memory/consistency_sc.rs ---

#[test]
fn sc_store_buffering() {
    fifty(|config| {
        let main = spawn_join(&[0, 0], 2);
        let t1: &[Op] = &[Store(X, 1, SeqCst), Load(Y, SeqCst, 0)];
        let t2: &[Op] = &[Store(Y, 1, SeqCst), Load(X, SeqCst, 0)];
        let regs = regs(config, &[&main, t1, t2]);
        assert_ne!((regs[1][0], regs[2][0]), (0, 0));
    });
}

#[test]
fn sc_iriw_sc_rlx() {
    fifty(|config| {
        let main = spawn_join(&[0, 0], 4);
        let a: &[Op] = &[Store(X, 1, Relaxed)];
        let b: &[Op] = &[Store(Y, 1, Relaxed)];
        let c: &[Op] = &[SpinUntil(X, SeqCst, 1), Load(Y, SeqCst, 0)];
        let d: &[Op] = &[SpinUntil(Y, SeqCst, 1), Load(X, SeqCst, 0)];
        let regs = regs(config, &[&main, a, b, c, d]);
        assert!(regs[3][0] == 1 || regs[4][0] == 1);
    });
}

#[test]
fn sc_cpp20_sc_fence_fix() {
    fifty(|config| {
        let main = spawn_join(&[0, 0], 4);
        let t1: &[Op] = &[Load(X, Relaxed, 0), Fence(SeqCst), Load(Y, Relaxed, 1)];
        let t2: &[Op] = &[Store(X, 1, Relaxed)];
        let t3: &[Op] = &[Store(Y, 1, Relaxed)];
        let t4: &[Op] = &[Load(Y, Relaxed, 0), Fence(SeqCst), Load(X, Relaxed, 1)];
        let regs = regs(config, &[&main, t1, t2, t3, t4]);
        assert_ne!((regs[1][0], regs[1][1], regs[4][0], regs[4][1]), (1, 0, 1, 0));
    });
}

#[test]
fn sc_cpp20_rwc_syncs() {
    fifty(|config| {
        let main = spawn_join(&[0, 0], 3);
        let t1: &[Op] = &[Store(X, 1, Relaxed)];
        let t2: &[Op] = &[SpinUntil(X, Relaxed, 1), Fence(SeqCst), Load(Y, Relaxed, 0)];
        let t3: &[Op] = &[Store(Y, 1, Relaxed), Fence(SeqCst), Load(X, Relaxed, 0)];
        let regs = regs(config, &[&main, t1, t2, t3]);
        assert_ne!((regs[2][0], regs[3][0]), (0, 0));
    });
}

#[test]
fn sc_fence_release() {
    fifty(|config| {
        let main = spawn_join(&[0, 0, 0, 0], 4);
        let t1: &[Op] = &[Store(X, 1, Relaxed), Fence(SeqCst), Store(K, 1, Relaxed)];
        let t2: &[Op] = &[Store(Y, 1, Relaxed), Fence(SeqCst), Store(Z, 1, Relaxed)];
        let t3: &[Op] = &[Load(K, Acquire, 0), Load(Y, Relaxed, 1)];
        let t4: &[Op] = &[Load(Z, Acquire, 0), Load(X, Relaxed, 1)];
        let regs = regs(config, &[&main, t1, t2, t3, t4]);
        assert_ne!((regs[3][0], regs[3][1], regs[4][0], regs[4][1]), (1, 0, 1, 0));
    });
}

#[test]
fn sc_fence_access() {
    fifty(|config| {
        let main = spawn_join(&[0, 0], 2);
        let t1: &[Op] = &[Store(X, 1, SeqCst), Load(Y, SeqCst, 0)];
        let t2: &[Op] = &[Store(Y, 1, Relaxed), Fence(SeqCst), Load(X, Relaxed, 0)];
        let regs = regs(config, &[&main, t1, t2]);
        assert_ne!((regs[1][0], regs[2][0]), (0, 0));
    });
}

#[test]
fn sc_fence_access_relacq() {
    fifty(|config| {
        let main = spawn_join(&[0, 0, 0], 3);
        let t1: &[Op] = &[Store(X, 1, SeqCst), Load(Y, SeqCst, 0)];
        let t2: &[Op] = &[Store(Y, 1, Relaxed), Store(Z, 1, Release)];
        let t3: &[Op] = &[Load(Z, Acquire, 0), Fence(SeqCst), Load(X, Relaxed, 1)];
        let regs = regs(config, &[&main, t1, t2, t3]);
        assert!(!(regs[1][0] == 0 && regs[3][..2] == [1, 0]));
    });
}

#[test]
fn sc_multi_fence() {
    fifty(|config| {
        let main = spawn_join(&[0, 0, 0], 4);
        let t1: &[Op] = &[Store(X, 1, SeqCst), Load(Y, SeqCst, 0)];
        let t2: &[Op] = &[Store(Y, 1, Relaxed), Fence(SeqCst), Load(Z, Relaxed, 0)];
        let t3: &[Op] = &[Store(Z, 1, Relaxed)];
        let t4: &[Op] = &[Load(Z, Relaxed, 0), Fence(SeqCst), Load(X, Relaxed, 1)];
        let regs = regs(config, &[&main, t1, t2, t3, t4]);
        assert!(!(regs[1][0] == 0 && regs[2][0] == 0 && regs[4][..2] == [1, 0]));
    });
}

#[test]
fn sc_relaxed() {
    fifty(|config| {
        let main = spawn_join(&[0, 0, 0], 2);
        let t1: &[Op] =
            &[Store(Y, 1, Relaxed), Fence(SeqCst), Load(X, Relaxed, 1), Fence(Acquire), Load(Z, Relaxed, 0)];
        let t2: &[Op] = &[Store(Z, 1, Relaxed), Store(X, 1, SeqCst), Load(Y, SeqCst, 0)];
        let regs = regs(config, &[&main, t1, t2]);
        assert_ne!((regs[1][0], regs[2][0]), (0, 0));
    });
}

#[test]
fn sc_cob_due_to_mo() {
    fifty(|config| {
        let mut main = static_atomics(&[0, 0]);
        main.extend([Store(X, 0, Relaxed), Store(Y, 0, Relaxed), Spawn(1), Spawn(2), Spawn(3), Join(2), Join(3), Join(1)]);
        let t1: &[Op] = &[Store(X, 1, Release)];
        let t2: &[Op] = &[Load(X, Relaxed, 0), Store(X, 2, SeqCst), Load(Y, SeqCst, 1)];
        let t3: &[Op] = &[Store(Y, 1, SeqCst), Load(X, SeqCst, 0)];
        let regs = regs(config, &[&main, t1, t2, t3]);
        assert!(!(regs[2][..2] == [1, 0] && regs[3][0] == 1));
    });
}

// --- tests/pass/0weak_memory/extra_cpp.rs ---

#[test]
fn extra_cpp_get_mut_write() {
    let mut main = static_atomics(&[0]);
    main.extend([Store(X, 1, Relaxed), Write(X, 2), Spawn(1), Join(1)]);
    let t1: &[Op] = &[Load(X, Relaxed, 0)];
    for seed in 0..50 {
        assert_eq!(regs(&seeded(seed), &[&main, t1])[1][0], 2);
    }
}

#[test]
fn extra_cpp_from_mut_split() {
    // A 4-byte atomic store, then 2-byte atomic loads of each half.
    let main: &[Op] = &[
        Alloc(1, 4),
        Write(X, 0),
        Store(X, 0xabba_fafa, Relaxed),
        Load(at_sized(1, 0, 2), Relaxed, 0),
        Load(at_sized(1, 2, 2), Relaxed, 1),
    ];
    assert_eq!(regs(&seeded(0), &[main])[0][..2], [0xfafa, 0xabba]);
}

#[test]
fn extra_cpp_assign_to_mut() {
    let main: &[Op] = &[Alloc(1, 4), Write(X, 0), Store(X, 1, Relaxed), Write(X, 2), Load(X, Relaxed, 0)];
    assert_eq!(regs(&seeded(0), &[main])[0][0], 2);
}

// --- store buffers ---

#[test]
fn non_atomic_write_drops_the_store_buffer() {
    // Once threads exist, a non-atomic write that doesn't race ends the location's weak
    // behavior (Miri: `non_atomic_write`): a later load on another thread sees only it.
    let mut main = static_atomics(&[0]);
    main.extend([Spawn(1), Join(1), Write(X, 5), Spawn(2), Join(2)]);
    let t1: &[Op] = &[Store(X, 1, Relaxed), Store(X, 2, Relaxed)];
    let t2: &[Op] = &[Load(X, Relaxed, 0)];
    let progs: [&[Op]; 3] = [&main, t1, t2];
    for seed in 0..50 {
        let mut sim = Sim::new(&fixed(seed), &progs);
        let regs = sim.run().unwrap_or_else(|stop| panic!("{stop:?}"));
        assert_eq!(regs[2][0], 5);
        assert_eq!(sim.races.buffer_count(id(1)), 0);
    }
}

#[test]
fn failed_cxchg_reads_the_latest_store() {
    // After a failed compare-exchange, which reads the latest store, a later load of the same
    // thread can't read an older one (CoRR).
    let main = spawn_join(&[0], 2);
    let t1: &[Op] = &[Store(X, 1, Relaxed)];
    let t2: &[Op] = &[Cxchg(X, 5, 6, SeqCst, Relaxed, 0), Load(X, Relaxed, 1)];
    for seed in 0..50 {
        let regs = regs(&fixed(seed), &[&main, t1, t2]);
        assert_eq!(regs[2][..2], [1, 1]);
    }
}

#[test]
fn store_buffer_limit() {
    let mut r = Races::new(&fixed(0));
    let x = id(1);
    let span = Span::new(0, 4);
    r.new_alloc(x, 4, Some(MAIN));
    let _t1 = r.spawn();
    let mut old = 0u32;
    for value in 1..=300u32 {
        r.write(x, span, Some(Relaxed), S).unwrap();
        r.store_value(x, span, Relaxed, &value.to_le_bytes(), Some(&old.to_le_bytes()));
        old = value;
    }
    let Slot::Live(clocks) = &mut r.allocs[x.index()] else { panic!("no clocks") };
    let pos = clocks.buffers.find(span).unwrap();
    assert_eq!(clocks.buffers.get_mut(pos).len(), STORE_BUFFER_LIMIT);
}
