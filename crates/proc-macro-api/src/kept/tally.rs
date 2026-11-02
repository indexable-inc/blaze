//! Statistics of expansions, kept where they are counted (docs/v2/plan/design/concurrency.md,
//! "Bookkeeping on a read stays on the worker"): each thread counts into a block of its own,
//! padded to 128 bytes and written by that thread alone, and [`Tally::get`] sums the blocks.
//! A count is a load and a store of the thread's own line: no line another core writes.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use crossbeam_queue::SegQueue;
use parking_lot::Mutex;

/// `N` counts, each how many and the nanoseconds they took, of one thread.
#[repr(align(128))]
pub struct Block<const N: usize> {
    counts: [[AtomicU64; 2]; N],
}

impl<const N: usize> Block<N> {
    const fn new() -> Block<N> {
        Block { counts: [const { [AtomicU64::new(0), AtomicU64::new(0)] }; N] }
    }

    /// Counts one of `which`, which took `took`. Only the thread whose block it is calls it,
    /// so a load and a store are the whole add.
    fn add(&self, which: usize, took: Duration) {
        let [calls, nanos] = &self.counts[which];
        calls.store(calls.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        let took = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        nanos.store(nanos.load(Ordering::Relaxed).saturating_add(took), Ordering::Relaxed);
    }
}

const _: () = assert!(std::mem::align_of::<Block<1>>() == 128);
const _: () = assert!(std::mem::size_of::<Block<9>>() % 128 == 0);

/// Every thread's block of `N` counts. The lock is taken only when a thread first counts, and
/// by [`Tally::get`] for a report; never by a count, so never across a call.
pub struct Tally<const N: usize> {
    blocks: Mutex<Vec<&'static Block<N>>>,
    /// The blocks of threads that ended, for the next thread to count into: their counts stay
    /// in the sums.
    free: SegQueue<&'static Block<N>>,
}

impl<const N: usize> Tally<N> {
    pub const fn new() -> Tally<N> {
        Tally { blocks: Mutex::new(Vec::new()), free: SegQueue::new() } // alloc: empty, allocates nothing
    }

    fn block(&'static self) -> &'static Block<N> {
        if let Some(block) = self.free.pop() {
            return block;
        }
        let block: &'static Block<N> = Box::leak(Box::new(Block::new())); // alloc: setup: once per thread, kept for the process
        self.blocks.lock().push(block);
        block
    }

    /// How many of `which` every thread counted, and the seconds they took.
    pub fn get(&self, which: usize) -> (u64, f64) {
        let blocks = self.blocks.lock();
        let (mut calls, mut nanos) = (0u64, 0u64);
        for block in blocks.iter() {
            let [c, n] = &block.counts[which];
            calls += c.load(Ordering::Relaxed);
            nanos = nanos.saturating_add(n.load(Ordering::Relaxed));
        }
        (calls, nanos as f64 / 1e9)
    }
}

impl<const N: usize> Default for Tally<N> {
    fn default() -> Tally<N> {
        Tally::new()
    }
}

/// A thread's block of a [`Tally`], given back to it when the thread ends: what a
/// `thread_local!` of the counting crate holds.
pub struct Mine<const N: usize> {
    tally: &'static Tally<N>,
    block: &'static Block<N>,
}

impl<const N: usize> Mine<N> {
    pub fn of(tally: &'static Tally<N>) -> Mine<N> {
        Mine { tally, block: tally.block() }
    }

    pub fn add(&self, which: usize, took: Duration) {
        self.block.add(which, took);
    }
}

impl<const N: usize> Drop for Mine<N> {
    fn drop(&mut self) {
        self.tally.free.push(self.block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_threads_counts_are_summed_and_outlive_it() {
        static TALLY: Tally<3> = Tally::new();
        std::thread::scope(|scope| {
            for n in 0..8u64 {
                scope.spawn(move || {
                    let mine = Mine::of(&TALLY);
                    for _ in 0..1000 {
                        mine.add(1, Duration::from_nanos(n));
                    }
                    mine.add(2, Duration::from_secs(1));
                });
            }
        });
        assert_eq!(TALLY.get(0), (0, 0.0));
        let (calls, took) = TALLY.get(1);
        assert_eq!(calls, 8000);
        assert!((took - 28_000.0 / 1e9).abs() < 1e-12);
        assert_eq!(TALLY.get(2), (8, 8.0));
        // A thread that comes after counts into a block one that ended gave back.
        let before = TALLY.blocks.lock().len();
        std::thread::spawn(|| Mine::of(&TALLY).add(0, Duration::ZERO)).join().unwrap();
        assert_eq!(TALLY.blocks.lock().len(), before);
        assert_eq!(TALLY.get(0).0, 1);
    }

    /// A count is a load and a store of the thread's own block, and a sum a walk of the
    /// blocks: neither allocates once the thread has its block.
    #[test]
    fn a_count_and_a_sum_allocate_nothing() {
        static TALLY: Tally<2> = Tally::new();
        let mine = Mine::of(&TALLY);
        let sum = blaze_alloc_count::assert_at_most(0, "expansions counted and summed", || {
            for _ in 0..1000 {
                mine.add(1, Duration::from_nanos(3));
            }
            TALLY.get(1)
        });
        assert_eq!(sum.0, 1000);
        assert!((sum.1 - 3_000.0 / 1e9).abs() < 1e-12);
    }
}
