//! A worker's queue of tasks that have not started (std.md, "`Threads`", point 2): a Chase-Lev
//! deque in a fixed ring of [`DEQUE`] slots. Its owner pushes and pops at the bottom; thieves
//! take from the top. The orderings are those of Lê, Pop, Cohen and Zappa Nardelli, "Correct
//! and Efficient Work-Stealing for Weak Memory Models" (PPoPP 2013), for a ring that never
//! grows:
//!
//! - **push** (owner): the slot at `bottom` is written (`Relaxed`), then `bottom + 1` stored
//!   with `Release`. A thief that loads that `bottom` with `Acquire` sees the slot's write.
//! - **pop** (owner): `bottom - 1` is stored (`Relaxed`), then a `SeqCst` fence, then `top` is
//!   loaded. The fence and the thief's own fence between its loads of `top` and `bottom` order
//!   the owner's claim on the last slot against every thief's (a Dekker pair): the owner sees a
//!   thief's advance of `top`, or the thief sees the lowered `bottom`. With more than one task
//!   left the owner takes the slot with no read-modify-write; with one, it races the thieves
//!   with the same compare-and-swap of `top` they use.
//! - **steal** (thief): `top` with `Acquire`, a `SeqCst` fence, `bottom` with `Acquire`, the
//!   slot (`Relaxed`, ordered after the `bottom` that published it), then a `SeqCst`
//!   compare-and-swap of `top` from what it read: it fails if another thief or the owner took
//!   that slot first, and the value read is then dropped unlooked-at.
//!
//! Slots are atomic pointers, so a thief that reads a slot the owner is rewriting (its `top` is
//! stale by a full ring) reads a whole old or new pointer, never a torn one, and its claim then
//! fails. The ring holds pointers it does not own: whoever pushes one gives the queue a
//! reference, and whoever takes it out owns that reference.
//!
//! Checked by blaze-check under `cfg(blaze_check)` (this module's `model` tests, each with
//! [`DequePlant::StealSkipsRecheck`], a steal that does not re-check `top`, that the model must
//! catch), and under real threads by the stress test with an exact count.

#[cfg(blaze_check)]
use blaze_check::sync::atomic::{AtomicIsize, AtomicPtr, Ordering, fence};
#[cfg(not(blaze_check))]
use core::sync::atomic::{AtomicIsize, AtomicPtr, Ordering, fence};
use core::ptr::{self, NonNull};

use super::threads::Padded;

/// The slots of a worker's deque: a power of two. A spawn into a full one runs its task inline
/// (std.md, "`Threads`", point 8): the bound is never passed and nothing waits for room.
pub(crate) const DEQUE: usize = 256;

const MASK: usize = DEQUE - 1;

const _: () = assert!(DEQUE.is_power_of_two(), "the deque's index is masked");

/// A planted bug for the model's control (rule 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DequePlant {
    None,
    /// A steal stores `top + 1` without comparing: it does not re-check that `top` is still
    /// what it read, so it and the owner (or two thieves) can both take one task.
    StealSkipsRecheck,
}

/// How a steal went.
#[derive(PartialEq, Eq, Debug)]
pub(crate) enum Steal<H> {
    /// Nothing to take.
    Empty,
    /// Another thief or the owner took the slot first: the thief moves on to another victim
    /// rather than spin on a contended one.
    Lost,
    Taken(NonNull<H>),
}

/// A worker's deque of pointers to `H`.
pub(crate) struct Deque<H> {
    /// The next slot a thief takes: written by thieves and by the owner's last-task race.
    top: Padded<AtomicIsize>,
    /// One past the owner's last task: written by the owner alone.
    bottom: Padded<AtomicIsize>,
    slots: [AtomicPtr<H>; DEQUE],
    plant: DequePlant,
}

impl<H> Deque<H> {
    pub(crate) fn new(plant: DequePlant) -> Deque<H> {
        Deque {
            top: Padded(AtomicIsize::new(0)),
            bottom: Padded(AtomicIsize::new(0)),
            slots: [const { AtomicPtr::new(ptr::null_mut()) }; DEQUE],
            plant,
        }
    }

    /// The owner's push: `Err` gives the task back when all [`DEQUE`] slots are taken.
    pub(crate) fn push(&self, job: NonNull<H>) -> Result<(), NonNull<H>> {
        let b = self.bottom.0.load(Ordering::Relaxed);
        // A stale `top` is lower than the true one, so the ring looks fuller, never emptier.
        let t = self.top.0.load(Ordering::Acquire);
        if b.wrapping_sub(t) >= DEQUE as isize {
            return Err(job);
        }
        self.slots[(b as usize) & MASK].store(job.as_ptr(), Ordering::Relaxed);
        self.bottom.0.store(b.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// The owner's pop of its newest task.
    pub(crate) fn pop(&self) -> Option<NonNull<H>> {
        let b = self.bottom.0.load(Ordering::Relaxed).wrapping_sub(1);
        self.bottom.0.store(b, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        let t = self.top.0.load(Ordering::Relaxed);
        if t > b {
            // Empty: put `bottom` back.
            self.bottom.0.store(b.wrapping_add(1), Ordering::Relaxed);
            return None;
        }
        let job = self.slots[(b as usize) & MASK].load(Ordering::Relaxed);
        if t < b {
            return NonNull::new(job);
        }
        // The last task: the thieves may be taking it too.
        let won = self.top.0.compare_exchange(t, t.wrapping_add(1), Ordering::SeqCst, Ordering::Relaxed).is_ok();
        self.bottom.0.store(b.wrapping_add(1), Ordering::Relaxed);
        if won { NonNull::new(job) } else { None }
    }

    /// A thief's take of the oldest task.
    pub(crate) fn steal(&self) -> Steal<H> {
        let t = self.top.0.load(Ordering::Acquire);
        fence(Ordering::SeqCst);
        let b = self.bottom.0.load(Ordering::Acquire);
        if t >= b {
            return Steal::Empty;
        }
        let job = self.slots[(t as usize) & MASK].load(Ordering::Relaxed);
        let claimed = match self.plant {
            DequePlant::StealSkipsRecheck => {
                self.top.0.store(t.wrapping_add(1), Ordering::SeqCst);
                true
            }
            DequePlant::None => {
                self.top.0.compare_exchange(t, t.wrapping_add(1), Ordering::SeqCst, Ordering::Relaxed).is_ok()
            }
        };
        match (claimed, NonNull::new(job)) {
            (true, Some(job)) => Steal::Taken(job),
            // A claimed slot always holds a pushed task: the push wrote it before the `bottom`
            // this thief read. A null here would be the deque's own bug.
            (true, None) => unreachable!("bstd::task::deque: a claimed slot held no task"),
            (false, _) => Steal::Lost,
        }
    }

    /// A thief's take of half the victim's tasks, at most what `into` (the thief's own deque)
    /// has room for: the first is returned to run now, the rest pushed onto `into`. Each task is
    /// one [`Deque::steal`], so each claim is the single steal's proven step; a lost race ends
    /// the batch.
    pub(crate) fn steal_half_into(&self, into: &Deque<H>) -> Steal<H> {
        let first = match self.steal() {
            Steal::Taken(job) => job,
            other => return other,
        };
        let want = self.len() / 2;
        // `into` is the caller's own deque: only it pushes there, so the room it sees now only
        // grows while this runs (thieves take, nobody else adds).
        let room = DEQUE - into.len();
        for _ in 0..want.min(room) {
            match self.steal() {
                Steal::Taken(job) => {
                    if let Err(job) = into.push(job) {
                        unreachable!("bstd::task::deque: a push into room counted before failed ({job:p})")
                    }
                }
                Steal::Empty | Steal::Lost => break,
            }
        }
        Steal::Taken(first)
    }

    /// How many tasks it holds, as one look sees it (thieves may be taking meanwhile).
    pub(crate) fn len(&self) -> usize {
        let b = self.bottom.0.load(Ordering::Acquire);
        let t = self.top.0.load(Ordering::Acquire);
        usize::try_from(b.wrapping_sub(t)).unwrap_or(0)
    }

    /// Whether one look finds it empty: the idle protocol's re-check after its fence
    /// (task/idle.rs), so the loads need no ordering of their own.
    pub(crate) fn looks_empty(&self) -> bool {
        self.bottom.0.load(Ordering::Relaxed) <= self.top.0.load(Ordering::Relaxed)
    }
}

/// Under the model checker (`RUSTFLAGS="--cfg blaze_check" cargo test -p blaze-std --release
/// model_deque`): the owner pops while a thief steals, and every pushed task is taken exactly
/// once; the planted steal that does not re-check `top` is caught.
#[cfg(all(test, blaze_check))]
mod model {
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        ptr::{NonNull, without_provenance_mut},
        sync::Arc,
        vec::Vec,
    };

    use blaze_check::thread;

    use super::{Deque, DequePlant, Steal};

    fn token(n: usize) -> NonNull<u8> {
        NonNull::new(without_provenance_mut(n)).expect("a token is not zero")
    }

    /// `pushed` tasks pushed, then the owner pops all it can while one thief steals once and
    /// another steals half into its own deque: every task out exactly once.
    fn race(plant: DequePlant, pushed: usize) {
        let deque = Arc::new(Deque::<u8>::new(plant));
        for n in 1..=pushed {
            deque.push(token(n)).expect("a fresh deque has room");
        }
        let thief = {
            let deque = Arc::clone(&deque);
            thread::spawn(move || match deque.steal() {
                Steal::Taken(job) => Some(job.as_ptr().addr()),
                Steal::Empty | Steal::Lost => None,
            })
        };
        let mut taken = Vec::new();
        while let Some(job) = deque.pop() {
            taken.push(job.as_ptr().addr());
        }
        if let Some(stolen) = thief.join().expect("the thief ends") {
            taken.push(stolen);
        }
        taken.sort_unstable();
        assert_eq!(taken, (1..=pushed).collect::<Vec<_>>(), "every task taken exactly once");
    }

    #[test]
    fn model_deque_pop_races_steal() {
        blaze_check::model(|| race(DequePlant::None, 1));
        blaze_check::model(|| race(DequePlant::None, 2));
    }

    #[test]
    fn model_deque_two_thieves_and_a_half_steal() {
        blaze_check::model(|| {
            let deque = Arc::new(Deque::<u8>::new(DequePlant::None));
            for n in 1..=3 {
                deque.push(token(n)).expect("a fresh deque has room");
            }
            let mine = Arc::new(Deque::<u8>::new(DequePlant::None));
            let half = {
                let (deque, mine) = (Arc::clone(&deque), Arc::clone(&mine));
                thread::spawn(move || {
                    let mut got = Vec::new();
                    if let Steal::Taken(job) = deque.steal_half_into(&mine) {
                        got.push(job.as_ptr().addr());
                    }
                    while let Some(job) = mine.pop() {
                        got.push(job.as_ptr().addr());
                    }
                    got
                })
            };
            let single = {
                let deque = Arc::clone(&deque);
                thread::spawn(move || match deque.steal() {
                    Steal::Taken(job) => Some(job.as_ptr().addr()),
                    Steal::Empty | Steal::Lost => None,
                })
            };
            let mut taken = Vec::new();
            while let Some(job) = deque.pop() {
                taken.push(job.as_ptr().addr());
            }
            taken.extend(half.join().expect("the half thief ends"));
            taken.extend(single.join().expect("the single thief ends"));
            taken.sort_unstable();
            assert_eq!(taken, [1, 2, 3], "every task taken exactly once");
        });
    }

    /// The control: a steal that does not re-check `top` takes the last task the owner takes.
    #[test]
    fn model_deque_catches_a_steal_that_skips_the_recheck() {
        let found = catch_unwind(AssertUnwindSafe(|| blaze_check::model(|| race(DequePlant::StealSkipsRecheck, 1))));
        assert!(found.is_err(), "the model must find the task taken twice");
    }
}

/// Under real threads: one owner pushes and pops while thieves steal singly and by halves; each
/// of the tasks is counted once by whoever took it, and the total is exact.
#[cfg(all(test, not(blaze_check)))]
mod stress {
    use std::{
        ptr::{NonNull, without_provenance_mut},
        sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        thread,
        vec::Vec,
    };

    use super::{DEQUE, Deque, DequePlant, Steal};

    const TASKS: usize = 200_000;
    const THIEVES: usize = 3;

    fn token(n: usize) -> NonNull<u8> {
        NonNull::new(without_provenance_mut(n + 1)).expect("a token is not zero")
    }

    fn take(seen: &[AtomicU8], ran: &AtomicUsize, job: NonNull<u8>) {
        let at = job.as_ptr().addr() - 1;
        let was = seen[at].fetch_add(1, Ordering::Relaxed);
        assert_eq!(was, 0, "task {at} taken twice");
        ran.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn every_task_is_taken_exactly_once_under_real_threads() {
        let deque = Deque::<u8>::new(DequePlant::None);
        let seen: Vec<AtomicU8> = (0..TASKS).map(|_| AtomicU8::new(0)).collect();
        let ran = AtomicUsize::new(0);
        let done = AtomicBool::new(false);
        thread::scope(|s| {
            for thief in 0..THIEVES {
                let (deque, seen, ran, done) = (&deque, &seen[..], &ran, &done);
                s.spawn(move || {
                    let mine = Deque::<u8>::new(DequePlant::None);
                    while !done.load(Ordering::Acquire) || !deque.looks_empty() {
                        let got = if thief == 0 { deque.steal_half_into(&mine) } else { deque.steal() };
                        if let Steal::Taken(job) = got {
                            take(seen, ran, job);
                        }
                        while let Some(job) = mine.pop() {
                            take(seen, ran, job);
                        }
                    }
                });
            }
            let mut next = 0;
            while next < TASKS {
                // Bursts of a third of the ring, then half of them popped back by the owner.
                let burst = (DEQUE / 3).min(TASKS - next);
                for _ in 0..burst {
                    match deque.push(token(next)) {
                        Ok(()) => next += 1,
                        // Full: run it inline, as a spawn does.
                        Err(job) => {
                            take(&seen, &ran, job);
                            next += 1;
                        }
                    }
                }
                for _ in 0..burst / 2 {
                    if let Some(job) = deque.pop() {
                        take(&seen, &ran, job);
                    }
                }
            }
            while let Some(job) = deque.pop() {
                take(&seen, &ran, job);
            }
            done.store(true, Ordering::Release);
        });
        assert_eq!(ran.load(Ordering::Relaxed), TASKS, "every task ran");
        assert!(seen.iter().all(|it| it.load(Ordering::Relaxed) == 1), "each exactly once");
    }
}
