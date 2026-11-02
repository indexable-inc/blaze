//! The pool's park and wake: which workers are parked, how a worker parks
//! without missing work published as it does, and how a publisher wakes
//! one. The place a wake can be lost, so the place a model checker runs
//! every schedule it can: `tests/shuttle_park.rs` compiles this same file
//! beside Shuttle's primitives (a `park_sync` module on each side, so no
//! copy drifts), where the library's `park_sync` is the standard library's.
//! Nothing here names the rest of the crate.
//!
//! The handshake is Dekker's: a worker sets its parked bit, then looks once
//! more for work, then parks; a publisher publishes, then looks at the
//! parked bits and wakes one. Each side stores, then loads, in SeqCst, so
//! in the one total order of SeqCst operations one of the two sees the
//! other: the worker sees the work and does not park, or the publisher
//! sees the bit and wakes it. That order is what the hardware gives SeqCst
//! on every target the fork builds for, and what Shuttle models (Loom
//! does not: data branch, 2026-10-23/model-checkers).

use super::park_sync::{AtomicU64, AtomicUsize, OnceLock, Ordering, thread};

/// Any value on a cache line of its own.
#[repr(align(128))]
pub(crate) struct Line<T>(pub(crate) T);

impl<T> std::ops::Deref for Line<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

pub(crate) struct Park {
    /// Workers parked: bit w of word w / 64 for worker w. A worker sets its
    /// own bit before it parks; a waker clears one (fetch_and), which makes
    /// it the one to unpark that worker. Each worker parks on its own
    /// thread's token (std's park), so no two share a lock or a futex word.
    parked: Box<[Line<AtomicU64>]>,
    /// Each worker's thread, set as it starts.
    handles: Box<[OnceLock<thread::Thread>]>,
    /// Wakes still owed: each woken worker pays up to `fanout` of them
    /// before it works.
    owed: AtomicUsize,
    /// The most wakes any one waker makes (FANOUT; a test may choose).
    fanout: usize,
}

/// The most wakes any one waker makes, publisher or woken worker: the rest
/// stay owed for the workers it woke, so a job's wakes spread as a tree,
/// FANOUT more a hop, and no thread makes more than FANOUT system calls
/// before its own work (BLAKE3 #119). Every wake direct put up to 47 calls
/// before the publisher's first piece on the EPYC 9255 (14 pool cells
/// 4-12% slower); one wake a hop made each worker wait for the one before
/// (the i7-12700K's 1-2 MiB one-shots 16-25% slower); a publisher of 8 with
/// workers paying the rest unbounded let one worker make every owed wake
/// in a quarter of the i7's 2 MiB jobs (data 2026-10-25/wake-counts).
pub(crate) const FANOUT: usize = 8;

/// A waker's share of `need` wakes: the ones it makes now, at most
/// `fanout`, and the ones it leaves owed.
pub(crate) fn split(need: usize, fanout: usize) -> (usize, usize) {
    let now = need.min(fanout);
    (now, need - now)
}

impl Park {
    pub(crate) fn new(workers: usize, fanout: usize) -> Park {
        assert!(fanout >= 1, "a waker makes at least one wake");
        Park {
            parked: (0..workers.div_ceil(64).max(1)).map(|_| Line(AtomicU64::new(0))).collect(),
            handles: (0..workers).map(|_| OnceLock::new()).collect(),
            owed: AtomicUsize::new(0),
            fanout,
        }
    }

    /// Worker `me` records its thread, once, as it starts.
    pub(crate) fn register(&self, me: usize) {
        assert!(self.handles[me].set(thread::current()).is_ok(), "each worker sets its thread once");
    }

    /// Worker `me`'s word and its bit in it.
    fn place(&self, me: usize) -> (&AtomicU64, u64) {
        (&self.parked[me / 64], 1u64.wrapping_shl(me as u32))
    }

    /// How many workers are parked.
    pub(crate) fn parked_count(&self) -> usize {
        self.parked.iter().map(|w| w.load(Ordering::SeqCst).count_ones() as usize).sum()
    }

    /// Wake one parked worker, if any: whether one was. With none parked,
    /// one load a word.
    pub(crate) fn wake_one(&self) -> bool {
        for (word, handles) in self.parked.iter().zip(self.handles.chunks(64)) {
            let mut bits = word.load(Ordering::SeqCst);
            while bits != 0 {
                let bit = 1u64 << bits.trailing_zeros();
                let before = word.fetch_and(!bit, Ordering::SeqCst);
                if before & bit != 0 {
                    handles[bit.trailing_zeros() as usize].get().expect("a parked worker has set its thread").unpark();
                    return true;
                }
                bits = before & !bit;
            }
        }
        false
    }

    /// Wake workers for `wanted` pieces of new work, given `workers`
    /// workers in all: up to `fanout` now, the rest owed, which the woken
    /// workers pay, `fanout` each, before they work. How many it woke. A
    /// publisher's read of `parked` follows its publication (a SeqCst store
    /// or a fence), as a parking worker's re-check follows its bit: one of
    /// the two sees the other.
    pub(crate) fn wake_for(&self, wanted: usize, workers: usize) -> usize {
        let parked = self.parked_count();
        // Nobody parked, or enough awake: nothing to wake.
        let (now, later) = split(wanted.saturating_sub(workers - parked).min(parked), self.fanout);
        if later > 0 {
            self.owed.fetch_add(later, Ordering::SeqCst);
        }
        (0..now).take_while(|_| self.wake_one()).count()
    }

    /// Worker `me` parks unless `visible()` finds work: the bit first,
    /// then one more look (a publisher stores, then reads `parked`; this
    /// sets the bit, then looks: one of the two sees the other), then the
    /// token, until a waker clears the bit. Returns having found work
    /// (false) or having been woken (true); a woken worker then pays the
    /// wakes owed ([`Park::pay_owed`]).
    pub(crate) fn park_unless(&self, me: usize, visible: impl Fn() -> bool) -> bool {
        let (word, bit) = self.place(me);
        word.fetch_or(bit, Ordering::SeqCst);
        if visible() {
            // Ours to clear, unless a waker cleared it first: then its wake
            // was spent on a worker that was leaving anyway (its unpark
            // leaves a token a later park returns from at once), and a
            // worker still parked may be the one it was for. Pass the wake
            // on, and return as woken so the caller pays what a woken
            // worker owes. Dropping it left a parked worker unwoken with
            // work published: Shuttle found both schedules (October 23).
            if word.fetch_and(!bit, Ordering::SeqCst) & bit == 0 {
                self.wake_one();
                return true;
            }
            return false;
        }
        while word.load(Ordering::SeqCst) & bit != 0 {
            thread::park();
        }
        true
    }

    /// A woken worker pays up to `fanout` of the wakes owed before it
    /// works, each a worker that pays in turn: how many it woke.
    pub(crate) fn pay_owed(&self) -> usize {
        let mut made = 0;
        while made < self.fanout {
            let owed = self.owed.load(Ordering::SeqCst);
            if owed == 0 {
                break;
            }
            if self.owed.compare_exchange(owed, owed - 1, Ordering::SeqCst, Ordering::SeqCst).is_err() {
                continue;
            }
            if !self.wake_one() {
                self.owed.store(0, Ordering::SeqCst);
                break;
            }
            made += 1;
        }
        made
    }
}

#[cfg(test)]
mod test {
    use super::split;

    /// A waker's share: at most its fan-out now, the rest owed, nothing
    /// lost or added.
    #[test]
    fn a_waker_wakes_at_most_its_fanout_and_owes_the_rest() {
        for need in 0..100 {
            for fanout in 1..20 {
                let (now, later) = split(need, fanout);
                assert!(now <= fanout, "need {need}, fan-out {fanout}");
                assert_eq!(now + later, need, "need {need}, fan-out {fanout}");
                assert_eq!(later > 0, need > fanout, "need {need}, fan-out {fanout}");
            }
        }
    }
}
