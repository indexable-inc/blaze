//! The kernel's waits and clocks: what blaze-core's one deadline wait (`blaze_core::wait`)
//! makes each of its calls with. Nothing here loops or recomputes a time: `wait::until` does,
//! from the wait's clock, before each call (rules deadline-waits, bounded-progress).

use core::{mem::MaybeUninit, sync::atomic::AtomicU32};

use super::{nr, raw};
use crate::{Errno, Fd, Timespec};

/// `FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG`.
const FUTEX_WAIT_BITSET_PRIVATE: usize = 9 | 128;
/// `FUTEX_WAKE | FUTEX_PRIVATE_FLAG`.
const FUTEX_WAKE_PRIVATE: usize = 1 | 128;

/// `FUTEX_BITSET_MATCH_ANY`: a wake of any bit ends the wait.
pub const FUTEX_ANY: u32 = u32::MAX;

/// One `futex(FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG)` on `word` while it holds `expected`,
/// until `at` on `CLOCK_MONOTONIC` (an absolute time; no limit for `None`), woken by a wake
/// of any bit of `bitset`. `Ok` when woken or returned with nothing; [`Errno::Again`] where
/// the word held something else; [`Errno::Intr`]; [`Errno::TimedOut`]. The park of the
/// direct wait driver (`wait/direct.rs`).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn futex_wait_bitset(
    word: &AtomicU32,
    expected: u32,
    at: Option<Timespec>,
    bitset: u32,
) -> Result<(), Errno> {
    let at = at.as_ref().map_or(0, |it| core::ptr::from_ref(it) as usize);
    // SAFETY: `word` is a live `u32` of this process the kernel only reads; the timespec
    // (null for no limit) is a live local it reads; it keeps no pointer after the call.
    raw::check(unsafe {
        raw::syscall6(
            nr::FUTEX,
            word.as_ptr() as usize,
            FUTEX_WAIT_BITSET_PRIVATE,
            expected as usize,
            at,
            0,
            bitset as usize,
        )
    })
    .map(|_| ())
}

/// One `futex(FUTEX_WAKE | FUTEX_PRIVATE_FLAG)` of up to `most` waiters on `word`: how many
/// were woken. The unpark of the direct wait driver.
// wakes: the threads parked on `word` by `futex_wait_bitset`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn futex_wake(word: &AtomicU32, most: u32) -> Result<u32, Errno> {
    // SAFETY: a wake names a word of this process by its address; the kernel reads no memory.
    raw::check(unsafe {
        raw::syscall3(nr::FUTEX, word.as_ptr() as usize, FUTEX_WAKE_PRIVATE, most as usize)
    })
    .map(|woken| woken as u32)
}

flag_set! {
    /// What a poll waits for and what it found (`POLL*`).
    PollEvents: i16 {
        /// `POLLIN`.
        IN = 0x01;
        /// `POLLPRI`.
        PRI = 0x02;
        /// `POLLOUT`.
        OUT = 0x04;
        /// `POLLERR`.
        ERR = 0x08;
        /// `POLLHUP`.
        HUP = 0x10;
        /// `POLLNVAL`.
        NVAL = 0x20;
    }
}

/// `struct pollfd`.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

impl PollFd {
    /// A poll of `fd` for `events`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn new(fd: Fd, events: PollEvents) -> PollFd {
        PollFd { fd: fd.raw(), events: events.bits(), revents: 0 }
    }

    /// What the last poll found.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn found(&self) -> PollEvents {
        PollEvents::from_bits(self.revents)
    }
}

/// The kernel's own signal set: 64 signals, 8 bytes, on every architecture blaze runs on
/// (glibc's `sigset_t` is 128 bytes, which the kernel refuses). Bit `n - 1` is signal `n`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct SigSet(pub u64);

impl SigSet {
    /// No signal.
    pub const EMPTY: SigSet = SigSet(0);

    /// The set with `signal` (1 to 64) added; unchanged for a number that is no signal.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn with(self, signal: u32) -> SigSet {
        match signal {
            // Wrapping forms: the arm's range keeps both in bounds, and no overflow check
            // (a panic path) is compiled in.
            1..=64 => SigSet(self.0 | 1u64.wrapping_shl(signal.wrapping_sub(1))),
            _ => self,
        }
    }
}

/// One `ppoll` of `fds` for up to `left` (a length of time; no limit for `None`) with `mask`
/// as the thread's signal mask for the call's length (the thread's own for `None`): how many
/// descriptors are ready, 0 when the time ran out. [`Errno::Intr`] when a signal ended it.
/// Used by the direct driver's `pollable` and its wait for a pidfd, and the accept and watch
/// errands.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn ppoll(fds: &mut [PollFd], left: Option<Timespec>, mask: Option<SigSet>) -> Result<usize, Errno> {
    // The kernel writes what was left into the timespec: a copy, so the caller's is its own.
    let mut left = left;
    let ts = left.as_mut().map_or(0, |it| core::ptr::from_mut(it) as usize);
    let (set, size) = match mask.as_ref() {
        Some(mask) => (core::ptr::from_ref(mask) as usize, size_of::<SigSet>()),
        None => (0, 0),
    };
    // SAFETY: `fds` is `fds.len()` live `pollfd`s the kernel writes `revents` of; the
    // timespec and the mask (null for none) are live locals, the mask of the kernel's own
    // size; it keeps no pointer after the call.
    raw::check(unsafe {
        raw::syscall5(nr::PPOLL, fds.as_mut_ptr() as usize, fds.len(), ts, set, size)
    })
}

/// A kernel clock.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClockId {
    /// `CLOCK_REALTIME`: wall time, which can jump.
    Realtime,
    /// `CLOCK_MONOTONIC`: never goes back, stops while the machine is suspended.
    Monotonic,
    /// `CLOCK_BOOTTIME`: as monotonic, and counts suspension.
    Boottime,
}

impl ClockId {
    const fn raw(self) -> usize {
        match self {
            ClockId::Realtime => 0,
            ClockId::Monotonic => 1,
            ClockId::Boottime => 7,
        }
    }
}

/// `clock_gettime`: `clock` now. Slice 1 makes the system call and does not read the vDSO: a
/// known cost on a hot path (sys.md, "The clock"), measured before blaze-core's clock moves
/// here. Used by the direct driver's absolute times.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn clock_gettime(clock: ClockId) -> Result<Timespec, Errno> {
    let mut now = MaybeUninit::<Timespec>::zeroed();
    // SAFETY: `now` is a live `__kernel_timespec` the kernel writes.
    raw::check(unsafe { raw::syscall2(nr::CLOCK_GETTIME, clock.raw(), now.as_mut_ptr() as usize) })?;
    // SAFETY: zeroed bytes are a valid `Timespec`, and the kernel filled it.
    Ok(unsafe { now.assume_init() })
}

/// One `clock_nanosleep(TIMER_ABSTIME)`: sleeps until `at` on `clock`. `Ok` once the time
/// came; [`Errno::Intr`] when a signal ended it early (the caller recomputes and sleeps
/// again). The direct driver's `sleep_until`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn clock_nanosleep_until(clock: ClockId, at: Timespec) -> Result<(), Errno> {
    /// `TIMER_ABSTIME`.
    const ABSOLUTE: usize = 1;
    // SAFETY: `at` is a live timespec the kernel reads; no remainder is asked for, as the
    // time is absolute.
    raw::check(unsafe {
        raw::syscall4(nr::CLOCK_NANOSLEEP, clock.raw(), ABSOLUTE, core::ptr::from_ref(&at) as usize, 0)
    })
    .map(|_| ())
}
