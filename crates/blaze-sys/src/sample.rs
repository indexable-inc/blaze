//! The sampler's memory and its frame-pointer walk (D178): what the per-OS drivers
//! (`macos_sample`, `linux::sample`) write between a thread's suspend and resume or in a signal
//! handler, kept here so that every `unsafe` of the sampler is in blaze-sys and bstd's core and
//! drivers have none. Pure: no system call in this file; every function is `noalloc` and
//! `nopanic` (indexing is bounded by the checks before it).
//!
//! A [`Slot`] is one worker's: the marks its hooks write (parked, top-level span), what its
//! driver recorded (its thread, its stack's top, its Linux timer), and its [`Ring`]. A ring has
//! one writer at a time by construction: [`Slot::write`] takes the slot's `writing` flag with a
//! swap (a second writer finds it taken and skips the tick: it never waits), and refuses once
//! [`Slot::close`] has begun; the close sets `closed` and then sees `writing` clear (both
//! sequentially consistent, so one of the two sees the other), after which [`Slot::ring`] reads
//! the ring with no writer left.

use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering},
};

/// The return addresses a sample keeps past its `pc`: 16 frames, 128 bytes.
pub const FRAMES: usize = 16;

/// The samples a ring keeps.
pub const RING: usize = 1024;

/// The bits of an address: arm64 macOS signs return addresses in the bits above 47 (pointer
/// authentication); Linux user space is below 2^48 on aarch64 and x86-64.
#[cfg(target_vendor = "apple")]
pub const ADDRESS: u64 = (1 << 47) - 1;
#[cfg(not(target_vendor = "apple"))]
pub const ADDRESS: u64 = (1 << 48) - 1;

/// A frame record's alignment: 16 on aarch64 (x29 is set from a 16-aligned sp) and on x86-64
/// (`push rbp` at a call boundary leaves rsp 16-aligned before `mov rbp, rsp`).
pub const ALIGN: u64 = 16;

/// The readable stack of a stopped thread: from its stack pointer (nothing below it is the
/// thread's) up to the top recorded when it started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stack {
    pub low: u64,
    pub high: u64,
}

impl Stack {
    /// Whether the 16-byte record at `at` is wholly inside.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn holds_record(&self, at: u64) -> bool {
        match at.checked_add(16) {
            Some(past) => self.low <= at && past <= self.high,
            None => false,
        }
    }
}

/// Why a walk stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stop {
    /// A zero frame pointer or return address: the chain's own end.
    Ended,
    /// [`FRAMES`] taken: the sample is truncated, its deepest callers dropped.
    Depth,
    /// The next record is not wholly inside the stack.
    Bounds,
    /// The next record is not [`ALIGN`]-aligned.
    Unaligned,
    /// The next record is not above this one: a cycle or a corrupt record.
    NotAbove,
}

impl Stop {
    pub const ALL: [Stop; 5] = [Stop::Ended, Stop::Depth, Stop::Bounds, Stop::Unaligned, Stop::NotAbove];

    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn code(self) -> usize {
        match self {
            Stop::Ended => 0,
            Stop::Depth => 1,
            Stop::Bounds => 2,
            Stop::Unaligned => 3,
            Stop::NotAbove => 4,
        }
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn says(self) -> &'static str {
        match self {
            Stop::Ended => "at the chain's end",
            Stop::Depth => "truncated at the depth cap",
            Stop::Bounds => "at the stack's bounds",
            Stop::Unaligned => "at an unaligned frame pointer",
            Stop::NotAbove => "at a record not above the last (a cycle or corruption)",
        }
    }
}

/// A walk's return addresses (`frames[..depth]`, nearest caller first) and why it stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Walked {
    pub frames: [u64; FRAMES],
    pub depth: u8,
    pub stop: Stop,
}

/// The chain from `fp` inside `stack`: a pure function. `read(at)` is asked only for `at` and
/// `at + 8` of a record with [`Stack::holds_record`]: the drivers' reads rely on it. A record
/// (AAPCS64, System V x86-64) is the caller's frame pointer, then the return address.
/// `plant` is a test's planted bug ([`SamplePlant`]); the drivers pass [`SamplePlant::None`].
#[inline]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn walk(fp: u64, stack: Stack, mut read: impl FnMut(u64) -> u64, plant: SamplePlant) -> Walked {
    let mut frames = [0u64; FRAMES];
    let mut depth = 0usize;
    let mut at = fp;
    let stop = loop {
        if at == 0 {
            break Stop::Ended;
        }
        if depth >= FRAMES {
            break Stop::Depth;
        }
        // Bounds before alignment: a pointer outside the stack is never read, whatever its
        // alignment, and it is reported as out of bounds.
        if !stack.holds_record(at) && plant != SamplePlant::WalkUnbounded {
            break Stop::Bounds;
        }
        if at % ALIGN != 0 {
            break Stop::Unaligned;
        }
        let next = read(at);
        let ret = read(at.wrapping_add(8)) & ADDRESS;
        if ret == 0 {
            break Stop::Ended;
        }
        frames[depth] = ret;
        depth += 1;
        if next != 0 && next <= at {
            break Stop::NotAbove;
        }
        at = next;
    };
    Walked { frames, depth: depth as u8, stop }
}

/// The bugs a control plants in [`take`] and [`walk`] (bstd's `SamplerPlant` maps onto it).
/// No driver passes anything but `None`: the drivers' reads rely on the bounds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SamplePlant {
    None,
    /// A worker marked parked is sampled as on-CPU.
    ParkedSampled,
    /// The walk reads a record outside the stack's bounds.
    WalkUnbounded,
}

/// The registers a sample starts from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Regs {
    pub pc: u64,
    pub fp: u64,
    pub sp: u64,
}

/// One sample of one worker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sample {
    pub pc: u64,
    /// Return addresses, nearest caller first: `frames[..depth]`.
    pub frames: [u64; FRAMES],
    pub depth: u8,
    pub stop: Stop,
    /// The worker's top-level span id (bstd's), 0 for none.
    pub span: u64,
    /// The driver's tick: the pass on macOS, the worker's own CPU-time tick on Linux.
    pub tick: u64,
}

impl Sample {
    pub const ZERO: Sample = Sample { pc: 0, frames: [0; FRAMES], depth: 0, stop: Stop::Ended, span: 0, tick: 0 };

    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn frames(&self) -> &[u64] {
        let depth = (self.depth as usize).min(FRAMES);
        &self.frames[..depth]
    }
}

/// A worker's samples: at most [`RING`] (160 bytes each), kept uniform over the whole run. A
/// full ring halves: every second sample is kept and from then on one tick in `stride` is, so
/// each kept sample stands for `stride` ticks (kept sample `k` was offered at `k * stride`).
/// Ticks the worker was parked are counted apart, never walked.
#[derive(Debug)]
pub struct Ring {
    samples: [Sample; RING],
    len: usize,
    seen: u64,
    stride: u64,
    parked: u64,
    stops: [u64; 5],
}

impl Ring {
    pub const EMPTY: Ring = Ring { samples: [Sample::ZERO; RING], len: 0, seen: 0, stride: 1, parked: 0, stops: [0; 5] };

    /// An on-CPU sample offered: kept when its tick falls on the stride.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn offer(&mut self, sample: Sample) {
        let index = self.seen;
        self.seen += 1;
        self.stops[sample.stop.code()] += 1;
        if index % self.stride != 0 {
            return;
        }
        if self.len >= RING {
            self.halve();
            if index % self.stride != 0 {
                return;
            }
        }
        self.samples[self.len] = sample;
        self.len += 1;
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn halve(&mut self) {
        let mut kept = 0;
        let mut at = 0;
        while at < self.len {
            self.samples[kept] = self.samples[at];
            kept += 1;
            at += 2;
        }
        self.len = kept;
        self.stride *= 2;
    }

    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn parked_tick(&mut self) {
        self.parked += 1;
    }

    pub fn samples(&self) -> &[Sample] {
        &self.samples[..self.len]
    }

    /// The ticks each kept sample stands for.
    pub fn stride(&self) -> u64 {
        self.stride
    }

    /// On-CPU ticks offered.
    pub fn seen(&self) -> u64 {
        self.seen
    }

    pub fn parked(&self) -> u64 {
        self.parked
    }

    /// Walks by how they stopped, in [`Stop::ALL`] order.
    pub fn stops(&self) -> [u64; 5] {
        self.stops
    }
}

/// One tick of one worker, as both drivers and bstd's simulation take it: a parked worker's
/// tick is counted apart; an on-CPU one is walked from `regs` inside `[regs.sp, top)` and
/// offered.
#[inline]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn take(
    ring: &mut Ring,
    parked: bool,
    regs: Regs,
    top: u64,
    span: u64,
    tick: u64,
    read: impl FnMut(u64) -> u64,
    plant: SamplePlant,
) {
    if parked && plant != SamplePlant::ParkedSampled {
        ring.parked_tick();
        return;
    }
    let walked = walk(regs.fp, Stack { low: regs.sp, high: top }, read, plant);
    ring.offer(Sample { pc: regs.pc & ADDRESS, frames: walked.frames, depth: walked.depth, stop: walked.stop, span, tick });
}

/// The marks a writer reads with the ring.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Marks {
    pub parked: bool,
    pub span: u64,
    pub top: u64,
    /// macOS: the Mach port; Linux: the tid.
    pub thread: u32,
}

/// One worker's slot (the module doc gives its writer rule).
pub struct Slot {
    claimed: AtomicBool,
    entered: AtomicBool,
    parked: AtomicBool,
    span: AtomicU64,
    /// The stack's range, as the driver's `enter` recorded it: the walk's bounds.
    low: AtomicU64,
    top: AtomicU64,
    thread: AtomicU32,
    pub(crate) timer: AtomicI32,
    writing: AtomicBool,
    closed: AtomicBool,
    drained: AtomicBool,
    ring: UnsafeCell<Ring>,
}

// SAFETY: every field but the ring is an atomic; the ring is written only inside
// `Slot::write`, which holds the `writing` flag (one writer, taken by a swap), and read through
// `Slot::ring` only once `close` saw no writer after setting `closed` (writers check `closed`
// after taking the flag, and both sides are SeqCst), or through `&mut self`.
unsafe impl Sync for Slot {}

/// A close that gave up: the slot's writer did not leave within the close's loads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StillWriting;

impl Default for Slot {
    fn default() -> Slot {
        Slot::new()
    }
}

impl Slot {
    pub const fn new() -> Slot {
        Slot {
            claimed: AtomicBool::new(false),
            entered: AtomicBool::new(false),
            parked: AtomicBool::new(false),
            span: AtomicU64::new(0),
            low: AtomicU64::new(0),
            top: AtomicU64::new(0),
            thread: AtomicU32::new(0),
            timer: AtomicI32::new(-1),
            writing: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            drained: AtomicBool::new(false),
            ring: UnsafeCell::new(Ring::EMPTY),
        }
    }

    /// The worker parks (just before the park's kernel wait). One store.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn park(&self) {
        self.parked.store(true, Ordering::Release);
    }

    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn unpark(&self) {
        self.parked.store(false, Ordering::Release);
    }

    /// The worker opened top-level span `id`: the span it replaces.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn set_span(&self, id: u64) -> u64 {
        self.span.swap(id, Ordering::Relaxed)
    }

    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn restore_span(&self, previous: u64) {
        self.span.store(previous, Ordering::Relaxed);
    }

    /// The thread and its stack's range, recorded once by a driver's `enter` (a second record
    /// is refused: `false`). Crate-private: the range is what the drivers' reads trust.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub(crate) fn record_stack(&self, thread: u32, low: u64, top: u64) -> bool {
        if self.claimed.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.thread.store(thread, Ordering::Relaxed);
        self.low.store(low, Ordering::Relaxed);
        self.top.store(top, Ordering::Relaxed);
        // Release: a driver that sees `entered` sees the range.
        self.entered.store(true, Ordering::Release);
        true
    }

    /// The stack's recorded range `(low, top)`.
    pub fn stack(&self) -> (u64, u64) {
        (self.low.load(Ordering::Relaxed), self.top.load(Ordering::Relaxed))
    }

    pub fn entered(&self) -> bool {
        self.entered.load(Ordering::Acquire)
    }

    pub fn marks(&self) -> Marks {
        Marks {
            parked: self.parked.load(Ordering::Acquire),
            span: self.span.load(Ordering::Relaxed),
            top: self.top.load(Ordering::Relaxed),
            thread: self.thread.load(Ordering::Relaxed),
        }
    }

    /// `f` given the ring and the marks as they are now, when no other writer holds the ring
    /// and the slot is not closed; otherwise `f` is not run and the answer is `false`. Never
    /// waits.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn write(&self, f: impl FnOnce(&mut Ring, Marks)) -> bool {
        if self.writing.swap(true, Ordering::SeqCst) {
            return false;
        }
        let wrote = if self.closed.load(Ordering::SeqCst) {
            false
        } else {
            let marks = self.marks();
            // SAFETY: this call holds `writing` (taken by the swap above) and the slot was not
            // closed after taking it: no other writer, and no reader (`ring` reads only after
            // `close` saw `writing` clear following `closed`).
            f(unsafe { &mut *self.ring.get() }, marks);
            true
        };
        self.writing.store(false, Ordering::SeqCst);
        wrote
    }

    /// No writer from now on: sets `closed`, then waits at most `loads` loads for a writer in
    /// flight to leave.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn close(&self, loads: u32) -> Result<(), StillWriting> {
        self.closed.store(true, Ordering::SeqCst);
        let mut n = 0u32;
        while self.writing.load(Ordering::SeqCst) {
            n += 1;
            if n >= loads {
                return Err(StillWriting);
            }
            core::hint::spin_loop();
        }
        self.drained.store(true, Ordering::Release);
        Ok(())
    }

    /// The ring, once [`Slot::close`] succeeded.
    pub fn ring(&self) -> Option<&Ring> {
        if !self.drained.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: `drained` is set only after `close` saw no writer following `closed`, and
        // no writer touches the ring after it sees `closed`: shared reads only from here.
        Some(unsafe { &*self.ring.get() })
    }

    /// The ring, by exclusive borrow (a simulation, a test).
    pub fn ring_mut(&mut self) -> &mut Ring {
        self.ring.get_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::{Ring, SamplePlant, Slot, Stack, Stop, walk};

    /// A fake stack of `words` at `base`; reads outside it are counted, and answer 0.
    struct Fake<'a> {
        base: u64,
        words: &'a [u64],
        outside: core::cell::Cell<u32>,
    }

    impl Fake<'_> {
        fn read(&self, at: u64) -> u64 {
            match at.checked_sub(self.base).map(|off| off / 8).and_then(|i| self.words.get(i as usize)) {
                Some(word) if at % 8 == 0 => *word,
                _ => {
                    self.outside.set(self.outside.get() + 1);
                    0
                }
            }
        }

        fn stack(&self) -> Stack {
            Stack { low: self.base, high: self.base + 8 * self.words.len() as u64 }
        }
    }

    const BASE: u64 = 0x1000;

    #[test]
    fn a_chain_ends_at_zero() {
        // Records at 0x1000 -> 0x1010 -> 0x1020 -> 0.
        let words = [0x1010, 0xa1, 0x1020, 0xa2, 0, 0xa3];
        let fake = Fake { base: BASE, words: &words, outside: core::cell::Cell::new(0) };
        let walked = walk(BASE, fake.stack(), |at| fake.read(at), SamplePlant::None);
        assert_eq!((walked.depth, walked.stop), (3, Stop::Ended));
        assert_eq!(&walked.frames[..3], &[0xa1, 0xa2, 0xa3]);
        assert_eq!(fake.outside.get(), 0);
    }

    #[test]
    fn a_cycle_and_a_corrupt_fp_end_the_walk() {
        // Four words, so the unaligned pointer below (`BASE + 8`) is inside the stack: it is
        // refused for its alignment, not its bounds (bounds are checked first).
        let cycle = [BASE, 0xa1, 0, 0];
        let fake = Fake { base: BASE, words: &cycle, outside: core::cell::Cell::new(0) };
        let walked = walk(BASE, fake.stack(), |at| fake.read(at), SamplePlant::None);
        assert_eq!((walked.depth, walked.stop), (1, Stop::NotAbove));
        let unaligned = walk(BASE + 8, fake.stack(), |at| fake.read(at), SamplePlant::None);
        assert_eq!((unaligned.depth, unaligned.stop), (0, Stop::Unaligned));
        // A record pointing below itself.
        let below = [0x1010, 0xa1, 0x0ff0, 0xa2];
        let fake = Fake { base: BASE, words: &below, outside: core::cell::Cell::new(0) };
        let walked = walk(BASE, fake.stack(), |at| fake.read(at), SamplePlant::None);
        assert_eq!((walked.depth, walked.stop), (2, Stop::NotAbove));
        assert_eq!(fake.outside.get(), 0);
    }

    #[test]
    fn a_read_past_the_stack_is_refused() {
        // The second record would straddle the stack's end: refused before any read of it.
        let words = [0x1010, 0xa1, 0x1020];
        let fake = Fake { base: BASE, words: &words, outside: core::cell::Cell::new(0) };
        let walked = walk(BASE, fake.stack(), |at| fake.read(at), SamplePlant::None);
        assert_eq!((walked.depth, walked.stop), (1, Stop::Bounds));
        assert_eq!(fake.outside.get(), 0, "the walk read outside the stack");
        // A frame pointer far outside.
        let walked = walk(0x9_0000, fake.stack(), |at| fake.read(at), SamplePlant::None);
        assert_eq!((walked.depth, walked.stop), (0, Stop::Bounds));
        assert_eq!(fake.outside.get(), 0);
    }

    #[test]
    fn a_ring_halves_and_stays_uniform() {
        let mut slot = Slot::new();
        let ring: &mut Ring = slot.ring_mut();
        let n = 5 * super::RING as u64;
        for tick in 0..n {
            super::take(ring, false, super::Regs { pc: tick, fp: 0, sp: 0 }, 0, 0, tick, |_| 0, SamplePlant::None);
        }
        assert_eq!(ring.seen(), n);
        let stride = ring.stride();
        assert!(stride.is_power_of_two() && ring.samples().len() <= super::RING);
        for (k, sample) in ring.samples().iter().enumerate() {
            assert_eq!(sample.tick, k as u64 * stride, "kept sample {k} is not on the stride");
        }
    }

    #[test]
    fn a_closed_slot_takes_no_writer() {
        let slot = Slot::new();
        assert!(slot.write(|ring, _| ring.parked_tick()));
        assert!(slot.ring().is_none());
        assert_eq!(slot.close(16), Ok(()));
        assert!(!slot.write(|ring, _| ring.parked_tick()));
        assert_eq!(slot.ring().map(Ring::parked), Some(1));
    }
}
