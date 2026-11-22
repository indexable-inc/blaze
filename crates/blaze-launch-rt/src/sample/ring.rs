//! The sample ring: a fixed number of [`SampleEntry`] slots, allocated once before any thread is
//! sampled, one producer (the macOS control thread, or one Linux thread's signal handler) and one
//! consumer (attribution). A push into a full ring is counted as dropped, never waited for: the
//! producer may be running with a thread stopped or inside a signal handler, where it must not
//! wait, lock or allocate.
//!
//! Every slot field is an atomic, so the producer and the consumer never race on plain memory:
//! the producer fills a slot with relaxed stores and then publishes it with a release store of
//! `tail`; the consumer reads `tail` with acquire, the slot with relaxed loads, and gives the slot
//! back with a release store of `head`, which the producer reads with acquire before it reuses the
//! slot. The planted bug [`Plant::PublishBeforeWrite`] moves the publish before the fill; the
//! interleaving test (ring_wait.rs, under `--cfg blaze_check`) catches it.

#[cfg(blaze_check)]
use blaze_check::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(blaze_check))]
use core::sync::atomic::{AtomicU64, Ordering};
use core::{alloc::Allocator, num::NonZeroU64};

use blaze_own::Vec;
use blaze_tier::sample::SampleEntry;

use super::SampleError;

/// A planted bug for the ring's interleaving control (rule 53). `pub(crate)`: only the tests
/// choose.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The slot is published (`tail` moved) before its fields are written: the consumer can read
    /// a slot's old contents as a new entry.
    #[cfg_attr(not(all(test, blaze_check)), expect(dead_code, reason = "the ring's planted-bug control (ring_wait.rs)"))]
    PublishBeforeWrite,
}

/// One slot: an entry's fields as atomics (`word` in the low half of the last, `thread` in the
/// high half).
struct Slot {
    pc: AtomicU64,
    lr: AtomicU64,
    fp: AtomicU64,
    frame_ret: AtomicU64,
    word_thread: AtomicU64,
}

impl Slot {
    fn empty() -> Slot {
        Slot {
            pc: AtomicU64::new(0),
            lr: AtomicU64::new(0),
            fp: AtomicU64::new(0),
            frame_ret: AtomicU64::new(0),
            word_thread: AtomicU64::new(0),
        }
    }

    fn write(&self, entry: SampleEntry) {
        self.pc.store(entry.pc, Ordering::Relaxed);
        self.lr.store(entry.lr, Ordering::Relaxed);
        self.fp.store(entry.fp, Ordering::Relaxed);
        self.frame_ret.store(entry.frame_ret, Ordering::Relaxed);
        self.word_thread.store(u64::from(entry.word) | (u64::from(entry.thread) << 32), Ordering::Relaxed);
    }

    fn read(&self) -> SampleEntry {
        let word_thread = self.word_thread.load(Ordering::Relaxed);
        SampleEntry {
            pc: self.pc.load(Ordering::Relaxed),
            lr: self.lr.load(Ordering::Relaxed),
            fp: self.fp.load(Ordering::Relaxed),
            frame_ret: self.frame_ret.load(Ordering::Relaxed),
            // The low and the high 32 bits: each fits a `u32` exactly.
            word: (word_thread & 0xffff_ffff) as u32,
            thread: (word_thread >> 32) as u32,
        }
    }
}

/// A fixed single-producer single-consumer ring of sample entries with a dropped count.
pub struct Ring<A: Allocator> {
    slots: Vec<Slot, A>,
    capacity: NonZeroU64,
    /// Entries pushed (the producer's position); only the producer stores it.
    tail: AtomicU64,
    /// Entries taken (the consumer's position); only the consumer stores it.
    head: AtomicU64,
    /// Pushes refused because the ring was full.
    dropped: AtomicU64,
}

impl<A: Allocator> Ring<A> {
    /// A ring of `capacity` slots, all allocated now in `alloc`; refused by name when `capacity`
    /// is zero or the slots cannot be allocated.
    pub fn try_new_in(capacity: u32, alloc: A) -> Result<Ring<A>, SampleError> {
        let Some(count) = NonZeroU64::new(u64::from(capacity)) else {
            return Err(SampleError::Zero { what: "the sample ring's capacity" });
        };
        let mut slots = Vec::try_with_capacity_in(capacity as usize, alloc)
            .map_err(|_| SampleError::Alloc { what: "the sample ring", count: u64::from(capacity) })?;
        for _ in 0..capacity {
            // Within the capacity reserved just above: no growth.
            slots.push(Slot::empty());
        }
        Ok(Ring { slots, capacity: count, tail: AtomicU64::new(0), head: AtomicU64::new(0), dropped: AtomicU64::new(0) })
    }

    pub fn capacity(&self) -> u64 {
        self.capacity.get()
    }

    /// Pushes refused because the ring was full, so far.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// The producer's push: `true` when `entry` was stored, `false` (and counted) when the ring
    /// was full. Only one thread at a time may push (the ring's one producer). Takes no lock,
    /// allocates nothing, waits for nothing and cannot panic: it runs with a thread stopped or
    /// inside a signal handler.
    pub fn push(&self, entry: SampleEntry) -> bool {
        self.push_with(entry, Plant::None)
    }

    pub(crate) fn push_with(&self, entry: SampleEntry, plant: Plant) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) >= self.capacity.get() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // `% NonZeroU64` cannot divide by zero, and the result is below the slot count, which the
        // constructor made equal to the capacity: `get` always finds the slot.
        let Some(slot) = self.slots.get((tail % self.capacity) as usize) else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        if plant == Plant::PublishBeforeWrite {
            self.tail.store(tail.wrapping_add(1), Ordering::Release);
            slot.write(entry);
            return true;
        }
        slot.write(entry);
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// The consumer's take: the oldest entry not yet taken, or `None` when the ring is empty. Only
    /// one thread at a time may take (the ring's one consumer).
    pub fn pop(&self) -> Option<SampleEntry> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        let slot = self.slots.get((head % self.capacity) as usize)?;
        let entry = slot.read();
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(entry)
    }

    /// Takes every entry present now, oldest first, into `each`; returns how many.
    pub fn drain(&self, mut each: impl FnMut(SampleEntry)) -> u64 {
        let mut count = 0u64;
        while let Some(entry) = self.pop() {
            each(entry);
            count += 1;
        }
        count
    }
}
