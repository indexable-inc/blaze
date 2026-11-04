//! The fence's real code: its rules one by one over plain words, and a stress test of threads
//! calling through a slot the daemon keeps moving while it poisons what the fence frees.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use blaze_ids::rng::SplitMix64;
use parking_lot::Mutex;

use super::*;

const PID: u32 = 7;

fn words() -> Vec<AtomicU64> {
    (0..CONTROL_WORDS).map(|_| AtomicU64::new(0)).collect()
}

#[test]
fn a_fresh_table_is_ready_at_epoch_one() {
    let fences = Fences::new(words());
    assert!(Thread::ready(fences.words()));
    assert_eq!(Thread::epoch(fences.words()), 1);
    assert!(!Thread::ready(&words()));
}

#[test]
fn with_no_threads_everything_has_passed() {
    let fences = Fences::new(words());
    assert_eq!(fences.oldest(fences.births(), &[(PID, &[])]), Some(u64::MAX));
    assert_eq!(fences.oldest(fences.births(), &[]), Some(u64::MAX));
}

#[test]
fn a_listed_thread_without_a_slot_holds_every_fence() {
    let fences = Fences::new(words());
    assert_eq!(fences.oldest(fences.births(), &[(PID, &[11])]), None);
    Thread::claim(fences.words(), PID, 11, 1).unwrap();
    assert_eq!(fences.oldest(fences.births(), &[(PID, &[11])]), Some(1));
}

#[test]
fn a_claim_while_looking_is_not_trusted() {
    let fences = Fences::new(words());
    let births = fences.births();
    Thread::claim(fences.words(), PID, 11, 1).unwrap();
    assert_eq!(fences.oldest(births, &[(PID, &[11])]), None);
    assert_eq!(fences.oldest(fences.births(), &[(PID, &[11])]), Some(1));
}

#[test]
fn a_half_claimed_slot_is_not_counted() {
    let fences = Fences::new(words());
    Thread::begin_claim(fences.words());
    assert_eq!(fences.oldest(fences.births(), &[(PID, &[11])]), None);
    Thread::finish_claim(fences.words(), PID, 11, 2).unwrap();
    assert_eq!(fences.oldest(fences.births(), &[(PID, &[11])]), Some(2));
}

#[test]
fn a_frame_in_a_posted_range_holds_its_grace() {
    let mut fences = Fences::new(words());
    let entry = fences.post(0x1000, 0x2000).unwrap();
    let grace = fences.start();
    assert_eq!(grace, Grace(1));
    let epoch = Thread::epoch(fences.words());
    assert_eq!(epoch, 2);
    let words = fences.words();
    assert_eq!(Thread::seen(words, epoch, Some(&[0x10, 0x3000])), epoch);
    assert_eq!(Thread::seen(words, epoch, Some(&[0x10, 0x1800])), 1);
    // A return address is one past its call: the range's end is in it, its start is not.
    assert_eq!(Thread::seen(words, epoch, Some(&[0x2000])), 1);
    assert_eq!(Thread::seen(words, epoch, Some(&[0x1000])), epoch);
    assert_eq!(Thread::seen(words, epoch, None), 1);
    assert!(!grace.passed_by(1));
    assert!(grace.passed_by(2));
    assert!(!Grace::NONE.passed_by(u64::MAX));
    fences.clear(entry);
    assert_eq!(Thread::seen(fences.words(), epoch, Some(&[0x1800])), epoch);
}

#[test]
fn a_range_posted_before_its_grace_starts_is_not_held_by_older_reports() {
    let mut fences = Fences::new(words());
    fences.post(0x1000, 0x2000).unwrap();
    // Read before `start`: the entry's grace is the epoch read, which the report can't pass.
    let epoch = Thread::epoch(fences.words());
    assert_eq!(Thread::seen(fences.words(), epoch, Some(&[0x1800])), epoch);
    let grace = fences.start();
    assert!(!grace.passed_by(epoch));
}

#[test]
fn the_oldest_report_decides() {
    let mut fences = Fences::new(words());
    fences.post(0x1000, 0x2000).unwrap();
    let grace = fences.start();
    let words = fences.words();
    let a = Thread::claim(words, PID, 1, 2).unwrap();
    let b = Thread::claim(words, PID, 2, 1).unwrap();
    assert_ne!(a, b);
    let oldest = fences.oldest(fences.births(), &[(PID, &[1, 2])]).unwrap();
    assert!(!grace.passed_by(oldest));
    Thread::report(fences.words(), b, 2);
    let oldest = fences.oldest(fences.births(), &[(PID, &[1, 2])]).unwrap();
    assert!(grace.passed_by(oldest));
}

#[test]
fn a_dead_threads_slot_is_reaped() {
    let fences = Fences::new(words());
    let index = Thread::claim(fences.words(), PID, 5, 1).unwrap();
    assert_eq!(fences.oldest(fences.births(), &[(PID, &[6])]), None);
    Thread::claim(fences.words(), PID, 6, 3).unwrap();
    assert_eq!(fences.oldest(fences.births(), &[(PID, &[6])]), Some(3));
    assert!(!Thread::holds(fences.words(), index, PID, 5));
}

#[test]
fn a_reused_thread_id_adopts_its_slot() {
    let fences = Fences::new(words());
    let first = Thread::claim(fences.words(), PID, 5, 1).unwrap();
    let again = Thread::claim(fences.words(), PID, 5, 2).unwrap();
    assert_eq!(first, again);
    assert_eq!(fences.oldest(fences.births(), &[(PID, &[5])]), Some(2));
    let other = Thread::claim(fences.words(), PID + 1, 5, 2).unwrap();
    assert_ne!(first, other);
}

#[test]
fn an_exited_process_holds_nothing() {
    let fences = Fences::new(words());
    let index = Thread::claim(fences.words(), PID, 5, 1).unwrap();
    fences.forget(PID);
    assert!(!Thread::holds(fences.words(), index, PID, 5));
    assert_eq!(fences.oldest(fences.births(), &[(PID, &[])]), Some(u64::MAX));
}

#[test]
fn a_released_slot_is_claimed_again() {
    let fences = Fences::new(words());
    let index = Thread::claim(fences.words(), PID, 5, 1).unwrap();
    Thread::release(fences.words(), index);
    assert_eq!(Thread::claim(fences.words(), PID, 6, 1), Some(index));
}

#[test]
fn every_slot_taken_leaves_a_thread_without_one() {
    let fences = Fences::new(words());
    for tid in 0..SLOTS as u64 {
        assert!(Thread::claim(fences.words(), PID, tid, 1).is_some());
    }
    assert_eq!(Thread::claim(fences.words(), PID, SLOTS as u64, 1), None);
    let tids: Vec<u64> = (0..=SLOTS as u64).collect();
    assert_eq!(fences.oldest(fences.births(), &[(PID, &tids)]), None);
}

#[test]
fn a_full_table_waits() {
    let mut fences = Fences::new(words());
    let entries: Vec<u32> = (0..ENTRIES as u64).map(|it| fences.post(it * 16, it * 16 + 16).unwrap()).collect();
    assert_eq!(fences.post(0, 16), None);
    fences.clear(entries[17]);
    assert_eq!(fences.post(0, 16), Some(17));
    for entry in entries {
        fences.clear(entry);
    }
    assert_eq!(fences.words().word(W_HIGH).load(Ordering::Relaxed), 0);
}

#[test]
fn an_announcement_is_one_word_apart_from_the_rest() {
    let mut fences = Fences::new(words());
    assert_eq!(Thread::announced(fences.words()), 0);
    fences.announce(9);
    assert_eq!(Thread::announced(fences.words()), 9);
    fences.post(0x1000, 0x2000).unwrap();
    fences.start();
    Thread::claim(fences.words(), PID, 1, 2).unwrap();
    assert_eq!(Thread::announced(fences.words()), 9);
    assert_eq!(Thread::epoch(fences.words()), 2);
}

#[test]
fn a_waiter_for_an_announcement_wakes_at_it_or_at_its_timeout() {
    let fences = Arc::new(Fences::new(words()));
    fences.announce(3);
    let started = Instant::now();
    assert_eq!(Thread::wait_announced(fences.words(), 2, Duration::from_secs(60)), 3, "not what was seen: at once");
    assert_eq!(Thread::wait_announced(fences.words(), 3, Duration::from_millis(20)), 3, "nothing announced");
    let announcer = Arc::clone(&fences);
    let announcing = std::thread::spawn(move || {
        blaze_spin::busy(Duration::from_millis(50));
        announcer.announce(4);
    });
    let mut now = 3;
    while now == 3 {
        now = Thread::wait_announced(fences.words(), 3, Duration::from_secs(60));
    }
    assert_eq!(now, 4);
    announcing.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(30), "woken by the announce, not the timeout");
}

#[test]
fn processes_are_counted_apart() {
    let fences = Fences::new(words());
    Thread::claim(fences.words(), PID, 1, 2).unwrap();
    Thread::claim(fences.words(), PID + 1, 1, 3).unwrap();
    let listed: &[(u32, &[u64])] = &[(PID, &[1]), (PID + 1, &[1])];
    assert_eq!(fences.oldest(fences.births(), listed), Some(2));
    assert_eq!(fences.oldest(fences.births(), &[(PID + 1, &[1, 2])]), None);
}

/// The control words, shared by the stress test's threads.
#[derive(Clone)]
struct Shared(Arc<[AtomicU64]>);

impl Words for Shared {
    fn words(&self) -> usize {
        self.0.len()
    }

    fn word(&self, index: usize) -> &AtomicU64 {
        &self.0[index]
    }
}

const RANGES: usize = 8;
const RANGE_BYTES: u64 = 0x100;
const RANGE_BASE: u64 = 0x10_0000;
const POISON: u64 = u64::MAX;

fn range_of(index: usize) -> (u64, u64) {
    let start = RANGE_BASE + index as u64 * RANGE_BYTES;
    (start, start + RANGE_BYTES)
}

/// What the stress test's daemon and threads share: the code ranges (a word each, poisoned
/// once freed), the slot word naming the range a call goes to, and the threads "the OS" lists.
struct World {
    control: Shared,
    code: Vec<AtomicU64>,
    slot: AtomicU64,
    listed: Mutex<Vec<u64>>,
    stop: AtomicBool,
    calls: AtomicU64,
}

/// A thread's side as `runtime::Attached::quiesce` does it, with the stack the test keeps.
struct Local {
    slot: Option<usize>,
    reported: u64,
}

fn quiesce(world: &World, local: &mut Local, tid: u64, frames: &[u64]) {
    let words = &world.control;
    let epoch = Thread::epoch(words);
    if let Some(index) = local.slot
        && local.reported == epoch
    {
        if Thread::holds(words, index, PID, tid) {
            return;
        }
        local.slot = None;
    }
    let seen = Thread::seen(words, epoch, Some(frames));
    match local.slot {
        Some(index) if Thread::holds(words, index, PID, tid) => Thread::report(words, index, seen),
        _ => local.slot = Thread::claim(words, PID, tid, seen),
    }
    local.reported = if seen == epoch { epoch } else { 0 };
}

/// One thread of the program: calls through the slot, reads the code it called into while
/// it is there (with quiescent points inside), and ends releasing its slot or, when `abrupt`,
/// dying with it held.
fn worker(world: &World, tid: u64, seed: u64, abrupt: bool) {
    let mut rng = SplitMix64::new(seed);
    let mut local = Local { slot: None, reported: 0 };
    let mut frames: Vec<u64> = Vec::with_capacity(4);
    frames.push(0x10);
    let mut left = 200 + rng.below(400);
    while left > 0 && !world.stop.load(Ordering::Relaxed) {
        left -= 1;
        let target = world.slot.load(Ordering::Acquire) as usize;
        let (start, _) = range_of(target);
        frames.push(start + 0x10);
        for _ in 0..1 + rng.below(3) {
            let value = world.code[target].load(Ordering::Acquire);
            assert_ne!(value, POISON, "thread {tid} ran range {target} after the fence freed it");
            if rng.coin() {
                quiesce(world, &mut local, tid, &frames);
            }
            if rng.one_in(8) {
                std::thread::yield_now();
            }
        }
        frames.pop();
        world.calls.fetch_add(1, Ordering::Relaxed);
        quiesce(world, &mut local, tid, &frames);
    }
    if !abrupt && let Some(index) = local.slot {
        let epoch = Thread::epoch(&world.control);
        if Thread::seen(&world.control, epoch, Some(&frames)) == epoch {
            Thread::release(&world.control, index);
        }
    }
}

#[test]
fn threads_never_run_freed_code() {
    const LANES: u64 = 4;
    const GENERATIONS: u64 = 6;
    const MOVES: usize = 3000;
    /// Calls that must land while ranges move: fewer than the lanes make in all.
    const OVERLAP: u64 = 1000;
    let world = Arc::new(World {
        control: Shared(Arc::from(words())),
        code: (0..RANGES).map(|_| AtomicU64::new(POISON)).collect(),
        slot: AtomicU64::new(0),
        listed: Mutex::new(Vec::new()),
        stop: AtomicBool::new(false),
        calls: AtomicU64::new(0),
    });
    world.code[0].store(1, Ordering::Release);
    let mut fences = Fences::new(world.control.clone());

    let lanes: Vec<_> = (0..LANES)
        .map(|lane| {
            let world = Arc::clone(&world);
            std::thread::spawn(move || {
                let tid = lane + 1;
                for generation in 0..GENERATIONS {
                    if world.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    // The OS lists a thread from before it runs to after it ended.
                    world.listed.lock().push(tid);
                    let inner = Arc::clone(&world);
                    let abrupt = generation % 3 == 2;
                    let seed = lane * 31 + generation + 1;
                    std::thread::spawn(move || worker(&inner, tid, seed, abrupt)).join().unwrap();
                    world.listed.lock().retain(|it| *it != tid);
                }
            })
        })
        .collect();

    let mut free: Vec<usize> = (1..RANGES).collect();
    let mut limbo: Vec<(Grace, u32, usize)> = Vec::new();
    let mut current = 0usize;
    let mut freed = 0usize;
    let mut generation = 1u64;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut listed: Vec<u64> = Vec::new();
    let collect = |fences: &mut Fences<Shared>, limbo: &mut Vec<(Grace, u32, usize)>, free: &mut Vec<usize>, listed: &mut Vec<u64>| {
        let births = fences.births();
        listed.clear();
        listed.extend_from_slice(&world.listed.lock());
        let Some(oldest) = fences.oldest(births, &[(PID, listed.as_slice())]) else { return 0 };
        let mut count = 0;
        limbo.retain(|&(grace, entry, range)| {
            if !grace.passed_by(oldest) {
                return true;
            }
            world.code[range].store(POISON, Ordering::Release);
            fences.clear(entry);
            free.push(range);
            count += 1;
            false
        });
        count
    };
    // Moves go on until the workers have called through them, however late they are scheduled;
    // all the lanes having ended (a worker panicked) stops them, to report that panic.
    let mut moves = 0;
    while (moves < MOVES || world.calls.load(Ordering::Relaxed) < OVERLAP) && !lanes.iter().all(|it| it.is_finished()) {
        assert!(
            Instant::now() < deadline,
            "{} calls landed in {moves} moves",
            world.calls.load(Ordering::Relaxed)
        );
        moves += 1;
        while free.is_empty() {
            assert!(Instant::now() < deadline, "the fence never passed: {} ranges held", limbo.len());
            freed += collect(&mut fences, &mut limbo, &mut free, &mut listed);
            std::thread::yield_now();
        }
        let next = free.pop().unwrap();
        generation += 1;
        world.code[next].store(generation, Ordering::Release);
        world.slot.store(next as u64, Ordering::Release);
        let (start, end) = range_of(current);
        let entry = fences.post(start, end).expect("the table has room for every range");
        limbo.push((fences.start(), entry, current));
        current = next;
        freed += collect(&mut fences, &mut limbo, &mut free, &mut listed);
    }
    world.stop.store(true, Ordering::Relaxed);
    for lane in lanes {
        lane.join().unwrap();
    }
    while !limbo.is_empty() {
        assert!(Instant::now() < deadline, "ranges held after every thread ended: {}", limbo.len());
        freed += collect(&mut fences, &mut limbo, &mut free, &mut listed);
    }
    assert!(freed >= moves.saturating_sub(RANGES), "only {freed} of {moves} ranges came back");
    let calls = world.calls.load(Ordering::Relaxed);
    assert!(calls >= OVERLAP, "only {calls} calls landed while ranges moved");
}
