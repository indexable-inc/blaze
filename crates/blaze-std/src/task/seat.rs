//! Two small pieces of the pool as one batch (std.md, "End state: efficiency", piece 15).
//!
//! **Seats** ([`Seats`]): a joiner outside the pool adopts a thief's identity for its join's
//! length, so it steals from the workers' deques as a worker does, not only from the injection
//! queues (task/help.rs `help_one`). The pool has [`SEATS`] of them; a join takes a free one with
//! one `fetch_or` a try and gives it back at its end; with none free it helps as before (the
//! injection queues only). A seat bounds how many threads outside the pool steal at once, and
//! names the thief in a count. A task a seated joiner steals runs on it as `Here::Outside`: a
//! deque holds only open tasks (a pinned visit is on a pinned queue, a reserved task in a
//! mailbox), each correct to run on any thread, and a `PerWorker` slot asked for there is refused
//! by name (`NotAWorker::Outside`), never shared.
//!
//! **The running task's name** ([`Current`]): a worker's `current` was a lock taken at each
//! task's start and end. It is now a sequence lock over an `AtomicPtr` to the name's bytes and
//! its length: the worker alone writes (no read-modify-write, no lock), and the stop's report
//! reads a consistent pair or says it was changing.

use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering, fence};

/// How many joiners outside the pool may steal at once.
pub(crate) const SEATS: u32 = 8;

/// How many times the stop's report reads a name that is being written before it says so: a
/// bounded read (rule 18), never a wait.
const NAME_TRIES: u32 = 4;

/// The pool's seats for joiners outside it: bit `k` set while seat `k` is taken.
pub(crate) struct Seats(AtomicU32);

impl Seats {
    pub(crate) const fn new() -> Seats {
        Seats(AtomicU32::new(0))
    }

    /// A free seat, taken: `None` when all [`SEATS`] are. One `fetch_or` a seat tried, at most
    /// [`SEATS`] tries.
    pub(crate) fn take(&self) -> Option<u32> {
        for seat in 0..SEATS {
            let bit = 1u32 << seat;
            if self.0.load(Ordering::Relaxed) & bit != 0 {
                continue;
            }
            // `Acquire`: pairs with the last holder's `Release` in `give`.
            if self.0.fetch_or(bit, Ordering::Acquire) & bit == 0 {
                return Some(seat);
            }
        }
        None
    }

    /// Gives seat `seat` back.
    pub(crate) fn give(&self, seat: u32) {
        let bit = 1u32 << seat;
        let was = self.0.fetch_and(!bit, Ordering::Release);
        assert!(was & bit != 0, "bstd::task::seat: seat {seat} given back that was not taken");
    }

    /// The seats taken now: for a test.
    #[cfg(test)]
    pub(crate) fn taken(&self) -> u32 {
        self.0.load(Ordering::Relaxed).count_ones()
    }
}

/// A worker's running task's name, written by that worker alone, read by the stop's report.
pub(crate) struct Current {
    /// Even when the pair below is whole, odd while it is written.
    version: AtomicU32,
    bytes: AtomicPtr<u8>,
    len: AtomicUsize,
}

/// What the report read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Named {
    Task(&'static str),
    /// Being written at each of [`NAME_TRIES`] reads: the worker is moving between tasks.
    Changing,
}

impl Current {
    pub(crate) fn new() -> Current {
        Current { version: AtomicU32::new(0), bytes: AtomicPtr::new("".as_ptr().cast_mut()), len: AtomicUsize::new(0) }
    }

    /// The name now, read by its own worker (the only writer, so no other write races).
    fn own(&self) -> &'static str {
        let bytes = self.bytes.load(Ordering::Relaxed);
        let len = self.len.load(Ordering::Relaxed);
        // SAFETY: the pair was stored by this thread from one `&'static str` (`set`), and no
        // other thread writes it.
        unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(bytes, len)) }
    }

    /// Sets the name, by its own worker only: what it was, to be put back after.
    pub(crate) fn set(&self, name: &'static str) -> &'static str {
        let outer = self.own();
        let version = self.version.load(Ordering::Relaxed);
        self.version.store(version.wrapping_add(1), Ordering::Relaxed);
        // The odd version is seen before either half changes (a sequence lock's writer).
        fence(Ordering::Release);
        self.bytes.store(name.as_ptr().cast_mut(), Ordering::Relaxed);
        self.len.store(name.len(), Ordering::Relaxed);
        self.version.store(version.wrapping_add(2), Ordering::Release);
        outer
    }

    /// The name, read from another thread: a whole pair, or [`Named::Changing`].
    pub(crate) fn read(&self) -> Named {
        for _ in 0..NAME_TRIES {
            let before = self.version.load(Ordering::Acquire);
            if before % 2 == 1 {
                core::hint::spin_loop();
                continue;
            }
            let bytes = self.bytes.load(Ordering::Relaxed);
            let len = self.len.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if self.version.load(Ordering::Relaxed) != before || bytes.is_null() {
                core::hint::spin_loop();
                continue;
            }
            // SAFETY: the version was even and unchanged across both loads, so the pair is one
            // `set` wrote whole, from one `&'static str`: its bytes live forever and are UTF-8.
            return Named::Task(unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(bytes, len)) });
        }
        Named::Changing
    }
}

impl Default for Current {
    fn default() -> Current {
        Current::new()
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests {
    use super::{Current, Named, SEATS, Seats};

    /// Prints `test task::seat::tests::seats_are_taken_up_to_their_count_and_given_back ... ok`.
    #[test]
    fn seats_are_taken_up_to_their_count_and_given_back() {
        let seats = Seats::new();
        let mut held = [0u32; SEATS as usize];
        for slot in held.iter_mut() {
            *slot = seats.take().expect("a free seat");
        }
        assert_eq!(seats.take(), None, "a seat past the count was taken");
        assert_eq!(seats.taken(), SEATS);
        seats.give(held[3]);
        assert_eq!(seats.take(), Some(held[3]), "a given-back seat was not taken again");
    }

    /// Prints `test task::seat::tests::a_name_set_is_read_whole_and_the_outer_one_comes_back ... ok`.
    #[test]
    fn a_name_set_is_read_whole_and_the_outer_one_comes_back() {
        let current = Current::new();
        assert_eq!(current.read(), Named::Task(""));
        let outer = current.set("bstd.task.example");
        assert_eq!(outer, "");
        assert_eq!(current.read(), Named::Task("bstd.task.example"));
        assert_eq!(current.set(outer), "bstd.task.example");
        assert_eq!(current.read(), Named::Task(""));
    }
}
