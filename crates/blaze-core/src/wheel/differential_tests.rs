//! [`super::Wheel`] against the copy `blaze-usercode`'s test queue had (`test/queue/wheel.rs`,
//! a `Vec`-column wheel firing at the next tick and naming the next expiry rather than the
//! soonest deadline), kept here as the spec now that the queue runs on this wheel; and that
//! copy's tests, moved.

use blaze_ids::rng::SplitMix64;
use hegel::TestCase;
use hegel::generators as gs;

use super::{Handle, Wheel};

/// The queue's wheel as it was, unchanged but for its names.
mod queue {
    const LEVELS: usize = 11;
    const SLOTS: usize = 64;
    const NONE: u32 = u32::MAX;
    pub(super) const UNSET: u64 = u64::MAX;
    const NOWHERE: u16 = u16::MAX;

    pub(super) struct Wheel<T> {
        now: u64,
        heads: [[u32; SLOTS]; LEVELS],
        occupied: [u64; LEVELS],
        due: Vec<u64>,
        what: Vec<Option<T>>,
        next: Vec<u32>,
        prev: Vec<u32>,
        generation: Vec<u32>,
        place: Vec<u16>,
        free: u32,
        len: usize,
    }

    impl<T: Copy> Wheel<T> {
        pub(super) fn new(now: u64) -> Wheel<T> {
            Wheel {
                now,
                heads: [[NONE; SLOTS]; LEVELS],
                occupied: [0; LEVELS],
                due: Vec::new(),
                what: Vec::new(),
                next: Vec::new(),
                prev: Vec::new(),
                generation: Vec::new(),
                place: Vec::new(),
                free: NONE,
                len: 0,
            }
        }

        pub(super) fn len(&self) -> usize {
            self.len
        }

        pub(super) fn insert(&mut self, due: u64, what: T) -> u64 {
            let id = match self.free {
                NONE => {
                    self.due.push(0);
                    self.what.push(None);
                    self.next.push(NONE);
                    self.prev.push(NONE);
                    self.generation.push(0);
                    self.place.push(NOWHERE);
                    (self.due.len() - 1) as u32
                }
                free => {
                    self.free = self.next[free as usize];
                    free
                }
            };
            self.due[id as usize] = due.max(self.now + 1);
            self.what[id as usize] = Some(what);
            self.len += 1;
            self.link(id);
            (u64::from(self.generation[id as usize]) << 32) | u64::from(id)
        }

        pub(super) fn remove(&mut self, handle: u64) {
            let id = handle as u32;
            let at = id as usize;
            if handle == UNSET
                || at >= self.place.len()
                || self.what[at].is_none()
                || self.generation[at] != (handle >> 32) as u32
            {
                return;
            }
            if self.place[at] != NOWHERE {
                self.unlink(id);
            }
            self.release(id);
        }

        pub(super) fn next(&self) -> Option<u64> {
            for level in 0..LEVELS {
                let bits = self.occupied[level];
                if bits == 0 {
                    continue;
                }
                let slot = bits.trailing_zeros() as u64;
                let shift = 6 * level as u32;
                let above = shift + 6;
                let base = match above {
                    64.. => 0,
                    _ => (self.now >> above) << above,
                };
                return Some(base | (slot << shift));
            }
            None
        }

        pub(super) fn advance(&mut self, to: u64, fired: &mut Vec<T>) {
            while let Some(at) = self.next() {
                if at > to {
                    break;
                }
                let level = (0..LEVELS).find(|it| self.occupied[*it] != 0).expect("an occupied level");
                let slot = self.occupied[level].trailing_zeros() as usize;
                self.now = at;
                let mut id = self.heads[level][slot];
                self.heads[level][slot] = NONE;
                self.occupied[level] &= !(1 << slot);
                while id != NONE {
                    let next = self.next[id as usize];
                    self.place[id as usize] = NOWHERE;
                    match self.due[id as usize] <= self.now {
                        true => {
                            fired.push(self.what[id as usize].expect("a set timer"));
                            self.release(id);
                        }
                        false => self.link(id),
                    }
                    id = next;
                }
            }
            self.now = self.now.max(to);
        }

        fn link(&mut self, id: u32) {
            let due = self.due[id as usize];
            let differ = due ^ self.now;
            let level = ((63 - differ.leading_zeros()) / 6) as usize;
            let slot = ((due >> (6 * level as u32)) & 63) as usize;
            let head = self.heads[level][slot];
            self.next[id as usize] = head;
            self.prev[id as usize] = NONE;
            if head != NONE {
                self.prev[head as usize] = id;
            }
            self.heads[level][slot] = id;
            self.occupied[level] |= 1 << slot;
            self.place[id as usize] = (level * SLOTS + slot) as u16;
        }

        fn unlink(&mut self, id: u32) {
            let place = self.place[id as usize] as usize;
            let (level, slot) = (place / SLOTS, place % SLOTS);
            let (prev, next) = (self.prev[id as usize], self.next[id as usize]);
            match prev {
                NONE => self.heads[level][slot] = next,
                prev => self.next[prev as usize] = next,
            }
            if next != NONE {
                self.prev[next as usize] = prev;
            }
            if self.heads[level][slot] == NONE {
                self.occupied[level] &= !(1 << slot);
            }
            self.place[id as usize] = NOWHERE;
        }

        fn release(&mut self, id: u32) {
            self.what[id as usize] = None;
            self.generation[id as usize] = self.generation[id as usize].wrapping_add(1);
            self.next[id as usize] = self.free;
            self.free = id;
            self.len -= 1;
        }
    }
}

/// A wheel whose time is `now`, as the queue starts one.
fn starting_at<T>(now: u64) -> Wheel<T> {
    let mut wheel = Wheel::new();
    wheel.advance(now, |_, _| unreachable!("an empty wheel fires nothing"));
    wheel
}

#[derive(Clone, Debug)]
enum Op {
    /// A timer due this many ticks after the next one.
    Insert(u64),
    /// The timer at this place among those set (modulo their number), cancelled.
    Cancel(usize),
    /// Time moved on by this many ticks.
    Advance(u64),
}

/// Below one of `ends`, picked evenly, so every wheel level is reached.
fn below(tc: &TestCase, ends: [u64; 4]) -> u64 {
    let end = ends[tc.draw(gs::integers::<usize>().max_value(3))];
    tc.draw(gs::integers().max_value(end - 1))
}

#[hegel::composite]
fn op(tc: &TestCase) -> Op {
    match tc.draw(gs::integers::<u8>().max_value(8)) {
        0..4 => Op::Insert(below(tc, [64, 70_000, 4_000_000, 1 << 40])),
        4..6 => Op::Cancel(tc.draw(gs::integers())),
        _ => Op::Advance(below(tc, [3, 5_000, 3_600_000, 1 << 41])),
    }
}

/// From any start, over any inserts (due after the next tick, where the two agree on
/// when a timer is due), cancels and advances: both fire the same timers at each advance,
/// this wheel in deadline then insertion order; its soonest deadline is the earliest set,
/// at or past the queue wheel's next expiry.
#[hegel::test]
fn the_queue_wheel_and_this_one_fire_alike(tc: TestCase) {
    let start: u64 = tc.draw(gs::integers().max_value((1 << 42) - 1));
    let ops = tc.draw(gs::vecs(op()).min_size(1).max_size(299));
    let mut wheel: Wheel<u64> = starting_at(start);
    let mut old: queue::Wheel<u64> = queue::Wheel::new(start);
    let mut now = start;
    let mut set: Vec<(Handle, u64, u64, u64)> = Vec::new();
    for (step, op) in ops.into_iter().enumerate() {
        let step = step as u64;
        match op {
            Op::Insert(after) => {
                let due = now.saturating_add(1).saturating_add(after);
                let handle = wheel.insert(due, step);
                set.push((handle, old.insert(due, step), due, step));
            }
            Op::Cancel(at) if !set.is_empty() => {
                let (handle, old_handle, _, step) = set.swap_remove(at % set.len());
                assert_eq!(wheel.cancel(handle), Some(step));
                assert_eq!(wheel.cancel(handle), None);
                old.remove(old_handle);
                old.remove(old_handle);
            }
            Op::Cancel(_) => {}
            Op::Advance(by) => {
                now = now.saturating_add(by);
                let mut fired = Vec::new();
                wheel.advance(now, |deadline, step| fired.push((deadline, step)));
                let mut old_fired = Vec::new();
                old.advance(now, &mut old_fired);
                let mut due: Vec<(u64, u64)> = set.iter().filter(|it| it.2 <= now).map(|it| (it.2, it.3)).collect();
                due.sort_unstable();
                assert_eq!(&fired, &due);
                old_fired.sort_unstable();
                let mut due_steps: Vec<u64> = due.iter().map(|it| it.1).collect();
                due_steps.sort_unstable();
                assert_eq!(old_fired, due_steps, "the queue wheel fires the same timers, in any order");
                set.retain(|it| it.2 > now);
            }
        }
        assert_eq!(wheel.len(), set.len());
        assert_eq!(old.len(), set.len());
        let earliest = set.iter().map(|it| it.2).min();
        assert_eq!(wheel.next_deadline(), earliest);
        match (old.next(), earliest) {
            (Some(expiry), Some(due)) => assert!(expiry > now && expiry <= due, "{expiry} {now} {due}"),
            (None, None) => {}
            other => panic!("{other:?}"),
        }
    }
}

/// Timers set, cancelled and fired, round after round: once the slab has grown to what is set
/// at once, nothing is allocated (the queue's steady state: per attempt a deadline and a
/// grace, half of them cancelled).
#[test]
fn a_wheel_in_its_steady_state_allocates_nothing() {
    let mut wheel: Wheel<u32> = Wheel::new();
    let mut fired = Vec::with_capacity(8);
    let mut round = |wheel: &mut Wheel<u32>, fired: &mut Vec<u32>, now: u64| {
        let handles: [Handle; 8] = std::array::from_fn(|k| wheel.insert(now + 1 + 1_000 * k as u64, k as u32));
        for handle in &handles[..4] {
            wheel.cancel(*handle);
        }
        fired.clear();
        wheel.advance(now + 10_000, |_, value| fired.push(value));
        assert_eq!(fired.len(), 4);
    };
    round(&mut wheel, &mut fired, 0);
    blaze_alloc_count::assert_at_most(0, "timers set, cancelled and fired", || {
        for n in 1..200u64 {
            round(&mut wheel, &mut fired, 10_000 * n);
        }
    });
}

/// Against a plain list of timers, over seeded inserts (from now on), cancels and advances
/// by steps from a tick to an hour, from a start far from zero: the same timers fire at each
/// advance, each once, none early, and the soonest deadline is the earliest set.
#[test]
fn the_wheel_fires_what_a_list_would() {
    for seed in 1..=40u64 {
        let mut rng = SplitMix64::new(seed);
        let start = rng.below(1 << 40);
        let mut wheel: Wheel<u64> = starting_at(start);
        let mut now = start;
        let mut set: Vec<(Handle, u64, u64)> = Vec::new();
        let mut fired = Vec::new();
        for step in 0..3_000u64 {
            match rng.below(4) {
                0 | 1 => {
                    let after = match rng.below(3) {
                        0 => rng.below(64),
                        1 => rng.below(70_000),
                        _ => rng.below(4_000_000),
                    };
                    let handle = wheel.insert(now + after, step);
                    set.push((handle, now + after, step));
                }
                2 if !set.is_empty() => {
                    let at = rng.index(set.len());
                    wheel.cancel(set.swap_remove(at).0);
                }
                _ => {
                    let by = match rng.below(3) {
                        0 => rng.below(3),
                        1 => rng.below(5_000),
                        _ => rng.below(3_600_000),
                    };
                    now += by;
                    fired.clear();
                    wheel.advance(now, |deadline, step| {
                        assert!(deadline <= now, "seed {seed}: fired early");
                        fired.push(step);
                    });
                    fired.sort_unstable();
                    let mut expected: Vec<u64> = set.iter().filter(|it| it.1 <= now).map(|it| it.2).collect();
                    expected.sort_unstable();
                    assert_eq!(fired, expected, "seed {seed} step {step}");
                    set.retain(|it| it.1 > now);
                }
            }
            assert_eq!(wheel.len(), set.len(), "seed {seed}");
            assert_eq!(wheel.next_deadline(), set.iter().map(|it| it.1).min(), "seed {seed}");
        }
    }
}

/// Cancelling a timer twice, or one that fired, is nothing; and the slab is reused, so a
/// steady stream of timers makes no more than are set at once.
#[test]
fn timers_are_reused_and_a_stale_cancel_is_nothing() {
    let mut wheel: Wheel<u32> = Wheel::new();
    let mut fired = Vec::new();
    let mut now = 0;
    for n in 0..10_000u32 {
        let a = wheel.insert(now + 60_000, n);
        let b = wheel.insert(now + 5, n);
        assert_eq!(wheel.cancel(a), Some(n));
        assert_eq!(wheel.cancel(a), None);
        now += 10;
        fired.clear();
        wheel.advance(now, |_, value| fired.push(value));
        assert_eq!(fired, [n]);
        // `b` fired: its handle names nothing, though its slot is the next `a`'s.
        assert_eq!(wheel.cancel(b), None);
        let c = wheel.insert(now + 1, n);
        assert_eq!(wheel.cancel(b), None);
        assert_eq!(wheel.len(), 1);
        assert_eq!(wheel.cancel(c), Some(n));
    }
    assert_eq!(wheel.len(), 0);
    assert!(wheel.slots() <= 2, "{}", wheel.slots());
    assert_eq!(wheel.next_deadline(), None);
}
