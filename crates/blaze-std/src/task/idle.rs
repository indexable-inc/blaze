//! The pool's idle protocol (std.md, "`Threads`", point 4): which workers are searching for
//! work, which are parked, and whom a spawn wakes. The protocol is these counters and each
//! worker's [`Mark`]; the park and the unpark themselves are the caller's (the pool parks on
//! `blaze_core::wait::Parker`, the model on blaze-check's `thread::park`).
//!
//! A worker's mark moves so (`W` the worker itself, `O` another thread):
//!
//! ```text
//! Busy -W-> Searching -W-> Busy          a searcher found work
//! Busy | Searching -W-> Parked           nothing found: going to sleep
//! Parked -W-> Busy                       woke (deadline, or work seen at the re-check)
//! Parked -O-> Searching                  a spawn's wake: the waker counts it as a searcher
//! Parked | Searching -O-> Reserved       `must_overlap` handed it a task (its mailbox)
//! Reserved -W-> Busy                     it took its mailbox
//! ```
//!
//! Every move out of `Searching` or `Parked` is a compare-and-swap, since another thread may
//! move it to `Reserved` at the same moment; a move out of `Busy` or `Reserved` is the worker's
//! alone, a store.
//!
//! **No lost wake.** A worker going to sleep moves its mark to `Parked`, counts itself parked
//! and leaves the search (all `SeqCst`), then a `SeqCst` fence, then looks at every queue once
//! more ([`Idle::to_park`] is the first half; the look is the caller's). A spawner publishes
//! its task (the deque's `Release` store or the injection queue's lock), then a `SeqCst`
//! fence ([`Idle::wake_one`]), then reads the searching count, the parked count and the marks.
//! In the single order of `SeqCst` fences one of the two is first: if the sleeper's, the
//! spawner sees it parked (and wakes it, or sees a searcher that will find the task); if the
//! spawner's, the sleeper's look sees the task. The unpark is a token, so a wake between the
//! look and the park is kept.
//!
//! **No thundering herd.** A spawn wakes at most one worker, and none while a worker is
//! searching (that searcher will find the task); a searcher that finds work and was the last
//! one wakes one more ([`Idle::leave_search`] says so), so a burst of spawns fans out one
//! worker at a time. At most half the workers search at once ([`Idle::begin_search`]).

#[cfg(blaze_check)]
use blaze_check::sync::atomic::{AtomicU32, Ordering, fence};
#[cfg(not(blaze_check))]
use core::sync::atomic::{AtomicU32, Ordering, fence};

use super::threads::Padded;

const BUSY: u32 = 0;
const SEARCHING: u32 = 1;
const PARKED: u32 = 2;
const RESERVED: u32 = 3;

/// A planted bug for the model's control (rule 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum IdlePlant {
    None,
    /// [`Idle::wake_one`] reads the counts without its fence: a sleeper's re-check and the
    /// spawner's look can each miss the other.
    WakeSkipsFence,
}

/// One worker's place in the protocol.
pub(crate) struct Mark {
    state: AtomicU32,
}

/// How a parked worker came back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Woke {
    /// By itself: it is busy and not searching.
    Itself,
    /// By a spawn's wake: it is a searcher, already counted.
    Searching,
    /// By `must_overlap`: its mailbox holds a task.
    Reserved,
}

impl Mark {
    pub(crate) const fn new() -> Mark {
        Mark { state: AtomicU32::new(BUSY) }
    }

    /// Whether `must_overlap` handed it a task. `Acquire`: the mailbox was written before the
    /// reservation's compare-and-swap.
    pub(crate) fn reserved(&self) -> bool {
        self.state.load(Ordering::Acquire) == RESERVED
    }

    /// The worker took its mailbox: busy again. Its own move, out of `Reserved`.
    pub(crate) fn took_mailbox(&self) {
        self.state.store(BUSY, Ordering::Relaxed);
    }
}

/// The pool's counts.
pub(crate) struct Idle {
    searching: Padded<AtomicU32>,
    parked: Padded<AtomicU32>,
    most_searching: u32,
    plant: IdlePlant,
}

impl Idle {
    pub(crate) fn new(workers: u32, plant: IdlePlant) -> Idle {
        Idle {
            searching: Padded(AtomicU32::new(0)),
            parked: Padded(AtomicU32::new(0)),
            most_searching: workers.div_ceil(2).max(1),
            plant,
        }
    }

    /// The worker of `mark`, busy, asks to search: granted while fewer than half the workers
    /// search. Its mark moves `Busy -> Searching`, a store: nobody else moves a busy mark.
    pub(crate) fn begin_search(&self, mark: &Mark) -> bool {
        if self.searching.0.load(Ordering::SeqCst) >= self.most_searching {
            return false;
        }
        self.searching.0.fetch_add(1, Ordering::SeqCst);
        mark.state.store(SEARCHING, Ordering::SeqCst);
        true
    }

    /// The searcher of `mark` found a task: `false` when it was reserved meanwhile (it runs its
    /// mailbox first). Either way it is no longer searching; the second value says it was the
    /// last searcher, so it must wake one more ([`Idle::wake_one`]).
    pub(crate) fn found(&self, mark: &Mark) -> (bool, bool) {
        let busy = mark.state.compare_exchange(SEARCHING, BUSY, Ordering::SeqCst, Ordering::Acquire).is_ok();
        (busy, self.leave_search())
    }

    /// A searcher stops searching (it found work, or was reserved): whether it was the last.
    pub(crate) fn leave_search(&self) -> bool {
        self.searching.0.fetch_sub(1, Ordering::SeqCst) == 1
    }

    /// The first half of going to sleep: the mark moves to `Parked` (from `Busy`, or
    /// `Searching` when `searching`), the worker is counted parked and leaves the search, and a
    /// `SeqCst` fence follows. `false` when the mark was reserved meanwhile: the worker does
    /// not sleep (and has left the search). The caller then looks at every queue once more, and
    /// parks only if it finds nothing.
    pub(crate) fn to_park(&self, mark: &Mark, searching: bool) -> bool {
        let from = if searching { SEARCHING } else { BUSY };
        let parked = mark.state.compare_exchange(from, PARKED, Ordering::SeqCst, Ordering::Acquire).is_ok();
        if searching {
            self.searching.0.fetch_sub(1, Ordering::SeqCst);
        }
        if !parked {
            return false;
        }
        self.parked.0.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        true
    }

    /// The worker of `mark`, parked (or about to), comes back: how.
    pub(crate) fn back(&self, mark: &Mark) -> Woke {
        match mark.state.compare_exchange(PARKED, BUSY, Ordering::SeqCst, Ordering::Acquire) {
            Ok(_) => {
                self.parked.0.fetch_sub(1, Ordering::SeqCst);
                Woke::Itself
            }
            Err(SEARCHING) => Woke::Searching,
            Err(RESERVED) => Woke::Reserved,
            Err(other) => unreachable!("bstd::task::idle: a parked worker's mark was moved to {other}"),
        }
    }

    /// The second half of a spawn, after its task is published: the fence, then the index of
    /// the one worker to unpark (its mark moved `Parked -> Searching` and counted a searcher),
    /// or none while a worker searches or none is parked. `marks` is every worker, in the order
    /// the spawner prefers (its own group first).
    pub(crate) fn wake_one<'m>(&self, marks: impl Iterator<Item = (usize, &'m Mark)>) -> Option<usize> {
        if self.plant != IdlePlant::WakeSkipsFence {
            fence(Ordering::SeqCst);
        }
        if self.searching.0.load(Ordering::SeqCst) != 0 || self.parked.0.load(Ordering::SeqCst) == 0 {
            return None;
        }
        for (at, mark) in marks {
            if mark.state.compare_exchange(PARKED, SEARCHING, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
                self.parked.0.fetch_sub(1, Ordering::SeqCst);
                self.searching.0.fetch_add(1, Ordering::SeqCst);
                return Some(at);
            }
        }
        None
    }

    /// `must_overlap`'s reservation: a worker that is searching or parked, its mark moved to
    /// `Reserved` (after the caller wrote its mailbox, which this compare-and-swap publishes),
    /// or none. A parked one is uncounted here; a searching one leaves the search itself when
    /// it sees the mark.
    pub(crate) fn reserve(&self, mark: &Mark) -> bool {
        if mark.state.compare_exchange(PARKED, RESERVED, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
            self.parked.0.fetch_sub(1, Ordering::SeqCst);
            return true;
        }
        mark.state.compare_exchange(SEARCHING, RESERVED, Ordering::SeqCst, Ordering::Relaxed).is_ok()
    }

    /// Whether the mark is free to reserve now: searching or parked (one look).
    pub(crate) fn idle(&self, mark: &Mark) -> bool {
        matches!(mark.state.load(Ordering::Relaxed), SEARCHING | PARKED)
    }

    /// The counts, for a test: (searching, parked).
    #[cfg(test)]
    pub(crate) fn counts(&self) -> (u32, u32) {
        (self.searching.0.load(Ordering::SeqCst), self.parked.0.load(Ordering::SeqCst))
    }
}

/// Under the model checker (`RUSTFLAGS="--cfg blaze_check" cargo test -p blaze-std --release
/// model_idle`): a worker that finds nothing goes to sleep while a spawner publishes a task and
/// wakes; the worker never sleeps through the task (the model reports a deadlock if it does).
/// The control drops the spawner's fence.
#[cfg(all(test, blaze_check))]
mod model {
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::Arc,
    };

    use blaze_check::{
        sync::atomic::{AtomicBool, Ordering},
        thread,
    };

    use super::{Idle, IdlePlant, Mark, Woke};

    struct World {
        idle: Idle,
        mark: Mark,
        task: AtomicBool,
    }

    fn sleep_and_spawn(plant: IdlePlant) {
        let world = Arc::new(World { idle: Idle::new(1, plant), mark: Mark::new(), task: AtomicBool::new(false) });
        let worker = {
            let world = Arc::clone(&world);
            thread::spawn(move || {
                if !world.idle.to_park(&world.mark, false) {
                    return;
                }
                // The re-check after the fence: the task was published before it, or the
                // spawner will see this worker parked.
                while !world.task.load(Ordering::Relaxed) {
                    thread::park();
                    match world.idle.back(&world.mark) {
                        // Woken by the spawner: look again (it published first).
                        Woke::Searching => {}
                        Woke::Itself => {
                            if !world.idle.to_park(&world.mark, false) {
                                return;
                            }
                        }
                        Woke::Reserved => return,
                    }
                }
            })
        };
        world.task.store(true, Ordering::Relaxed);
        if world.idle.wake_one([(0, &world.mark)].into_iter()).is_some() {
            worker.thread().unpark();
        }
        worker.join().expect("the worker ends");
    }

    #[test]
    fn model_idle_no_wake_is_lost() {
        blaze_check::model(|| sleep_and_spawn(IdlePlant::None));
    }

    #[test]
    fn model_idle_catches_a_wake_without_its_fence() {
        let found = catch_unwind(AssertUnwindSafe(|| blaze_check::model(|| sleep_and_spawn(IdlePlant::WakeSkipsFence))));
        assert!(found.is_err(), "the model must find the worker asleep with a task published");
    }
}
