//! The kernel's waits and clocks: what blaze-core's one deadline wait (`blaze_core::wait`)
//! makes each of its calls with. Nothing here loops or recomputes a time: `wait::until` does,
//! from the wait's clock, before each call (rules deadline-waits, bounded-progress).

use core::sync::atomic::AtomicU32;

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
/// replaces libc::syscall(libc::SYS_futex) (FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG; as blaze_sys::linux::futex_wait_bitset(word: &AtomicU32, expected, at: absolute CLOCK_MONOTONIC Timespec or None, bitset, FUTEX_ANY for MATCH_ANY); returns Result<(), Errno>, the site's -1/errno check becomes the Err; a shared wait (no PRIVATE flag) or a plain FUTEX_WAIT with a relative time is not this call)
/// replaces libc::SYS_futex
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
/// replaces libc::syscall(libc::SYS_futex) (FUTEX_WAKE | FUTEX_PRIVATE_FLAG; as blaze_sys::linux::futex_wake(word: &AtomicU32, most); i32::MAX becomes u32::MAX or i32::MAX as u32; returns Result<u32 woken, Errno>; a shared wake (no PRIVATE flag) is not this call)
/// replaces libc::SYS_futex
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

/// `FUTEX_WAIT` without `FUTEX_PRIVATE_FLAG`: a shared wait.
const FUTEX_WAIT_SHARED: usize = 0;
/// `FUTEX_WAKE` without `FUTEX_PRIVATE_FLAG`: a shared wake.
const FUTEX_WAKE_SHARED: usize = 1;

/// One shared `futex(FUTEX_WAIT)` (no `FUTEX_PRIVATE_FLAG`) on `word` while it holds
/// `expected`, for up to `left` (a length of time on `CLOCK_MONOTONIC`; no limit for `None`).
/// Shared: the kernel keys the wait by the word's page, so a word of a shared mapping is the
/// same futex in every process that maps it (a private wait is keyed by this process's
/// address and never meets another's wake). `Ok` when woken or returned with nothing;
/// [`Errno::Again`] where the word held something else; [`Errno::Intr`];
/// [`Errno::TimedOut`]. The caller's deadline loop recomputes `left` before each call.
/// replaces libc::syscall(libc::SYS_futex) (FUTEX_WAIT without FUTEX_PRIVATE_FLAG, a relative timeout; as blaze_sys::linux::futex_wait_shared(word: &AtomicU32, expected, left: Option<Timespec>); returns Result<(), Errno>, the site's -1/errno check becomes the Err)
/// replaces libc::FUTEX_WAIT (the shared form, relative time: blaze_sys::linux::futex_wait_shared; Linux only)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn futex_wait_shared(word: &AtomicU32, expected: u32, left: Option<Timespec>) -> Result<(), Errno> {
    let left = left.as_ref().map_or(0, |it| core::ptr::from_ref(it) as usize);
    // SAFETY: `word` is a live, aligned `u32` the kernel only reads; the timespec (null for no
    // limit) is a live local it reads; it keeps no pointer after the call.
    raw::check(unsafe {
        raw::syscall4(nr::FUTEX, word.as_ptr() as usize, FUTEX_WAIT_SHARED, expected as usize, left)
    })
    .map(|_| ())
}

/// `FUTEX_WAIT_BITSET` without `FUTEX_PRIVATE_FLAG`: a shared wait to an absolute time.
const FUTEX_WAIT_BITSET_SHARED: usize = 9;

/// One shared `futex(FUTEX_WAIT_BITSET)` (no `FUTEX_PRIVATE_FLAG`) on `word` while it holds
/// `expected`, until `at` on `CLOCK_MONOTONIC` (an absolute time; no limit for `None`), woken
/// by a wake of any bit of `bitset` from any process that maps the word's page
/// ([`futex_wake_shared`] wakes every bit). The absolute time makes a retry after
/// [`Errno::Intr`] wait only for what is left. Answers as [`futex_wait_bitset`].
/// replaces libc::syscall(libc::SYS_futex) (FUTEX_WAIT_BITSET without FUTEX_PRIVATE_FLAG, an absolute CLOCK_MONOTONIC time; as blaze_sys::linux::futex_wait_bitset_shared(word: &AtomicU32, expected, at: Option<Timespec>, bitset, FUTEX_ANY for MATCH_ANY); returns Result<(), Errno>)
/// replaces libc::FUTEX_WAIT_BITSET (the shared form: blaze_sys::linux::futex_wait_bitset_shared; Linux only)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn futex_wait_bitset_shared(
    word: &AtomicU32,
    expected: u32,
    at: Option<Timespec>,
    bitset: u32,
) -> Result<(), Errno> {
    let at = at.as_ref().map_or(0, |it| core::ptr::from_ref(it) as usize);
    // SAFETY: `word` is a live, aligned `u32` the kernel only reads; the timespec (null for no
    // limit) is a live local it reads; it keeps no pointer after the call.
    raw::check(unsafe {
        raw::syscall6(nr::FUTEX, word.as_ptr() as usize, FUTEX_WAIT_BITSET_SHARED, expected as usize, at, 0, bitset as usize)
    })
    .map(|_| ())
}

/// One shared `futex(FUTEX_WAKE)` (no `FUTEX_PRIVATE_FLAG`) of up to `most` waiters on
/// `word`, in any process that maps its page: how many were woken.
/// replaces libc::syscall(libc::SYS_futex) (FUTEX_WAKE without FUTEX_PRIVATE_FLAG; as blaze_sys::linux::futex_wake_shared(word: &AtomicU32, most); i32::MAX becomes i32::MAX as u32; returns Result<u32 woken, Errno>)
/// replaces libc::FUTEX_WAKE (the shared form: blaze_sys::linux::futex_wake_shared; Linux only)
// wakes: the threads, in any process, parked on `word` by `futex_wait_shared`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn futex_wake_shared(word: &AtomicU32, most: u32) -> Result<u32, Errno> {
    // SAFETY: a wake names a word by its address (resolved to its page); the kernel reads no
    // memory of it.
    raw::check(unsafe {
        raw::syscall3(nr::FUTEX, word.as_ptr() as usize, FUTEX_WAKE_SHARED, most as usize)
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
            // Bit `signal - 1`. The arm's range keeps both in bounds; written with forms that
            // have no check in them (a check is a panic path): `<<` checks its amount and
            // `wrapping_shl` reaches `unchecked_shl`'s precondition check, where a rotation
            // of 1 is the same shift and checks nothing.
            1..=64 => SigSet(self.0 | 1u64.rotate_left(signal.wrapping_sub(1))),
            _ => self,
        }
    }
}

/// One `ppoll` of `fds` for up to `left` (a length of time; no limit for `None`) with `mask`
/// as the thread's signal mask for the call's length (the thread's own for `None`): how many
/// descriptors are ready, 0 when the time ran out. [`Errno::Intr`] when a signal ended it.
/// Used by the direct driver's `pollable` and its wait for a pidfd, and the accept and watch
/// errands.
/// replaces libc::syscall(libc::SYS_ppoll) (as blaze_sys::linux::ppoll(&mut [PollFd], left: relative Timespec or None, mask: Option<SigSet>); the kernel's sigset size is passed for you; returns Result<usize ready, Errno>)
/// replaces libc::SYS_ppoll
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
    /// `CLOCK_MONOTONIC_RAW`: monotonic, not slewed by NTP.
    MonotonicRaw,
    /// `CLOCK_PROCESS_CPUTIME_ID`: CPU time of the calling process.
    ProcessCpu,
    /// `CLOCK_THREAD_CPUTIME_ID`: CPU time of the calling thread.
    ThreadCpu,
    /// The CPU-time clock of process `pid`, the kernel's `(!pid << 3) | 2` (`CPUCLOCK_SCHED`
    /// of a process); what libc's `clock_getcpuclockid` returns.
    /// replaces libc::clock_getcpuclockid (blaze_sys::linux::ClockId::ProcessCpuOf(pid); Linux)
    ProcessCpuOf(i32),
    /// The CPU-time clock of thread `tid` (any thread of this process, read from another),
    /// the kernel's `MAKE_THREAD_CPUCLOCK(tid, CPUCLOCK_SCHED)`: `(!tid << 3) | 6`
    /// (include/linux/posix-timers.h). What glibc's `pthread_getcpuclockid` returns for that
    /// thread's handle.
    /// replaces libc::pthread_getcpuclockid (Linux, by tid: blaze_sys::linux::ClockId::ThreadCpuOf(tid))
    /// replaces blaze_profile::sampler::linux's hand-made `(!tid << 3) | 6` clock id
    ThreadCpuOf(i32),
}

impl ClockId {
    const fn raw(self) -> usize {
        match self {
            ClockId::Realtime => 0,
            ClockId::Monotonic => 1,
            ClockId::ProcessCpu => 2,
            ClockId::ThreadCpu => 3,
            ClockId::MonotonicRaw => 4,
            ClockId::Boottime => 7,
            // The kernel's MAKE_PROCESS_CPUCLOCK(pid, CPUCLOCK_SCHED): a negative clockid_t,
            // passed sign-extended as the register value.
            ClockId::ProcessCpuOf(pid) => (((!pid) << 3) | 2) as isize as usize,
            // MAKE_THREAD_CPUCLOCK(tid, CPUCLOCK_SCHED): CPUCLOCK_PERTHREAD_MASK (4) | SCHED (2).
            ClockId::ThreadCpuOf(tid) => (((!tid) << 3) | 6) as isize as usize,
        }
    }

    /// The clock's `clockid_t`: for a caller that writes it into a kernel struct itself, as
    /// `perf_event_attr`'s `clockid` with `use_clockid` (`PerfEventAttr::clockid`).
    /// replaces libc::CLOCK_MONOTONIC (as a raw clockid_t in a struct the caller fills: blaze_sys::linux::ClockId::Monotonic.id(), an i32; the same for the other clocks)
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn id(self) -> i32 {
        // Every number is a sign-extended `clockid_t` (an `int`): the cut gives it back.
        self.raw() as i32
    }
}

/// `clock_gettime`: `clock` now, by the system call. The read for a caller that holds no
/// [`super::vdso::Vdso`] (the direct driver's absolute times today), and the path
/// [`clock_gettime_vdso`] takes on a kernel whose vDSO has no `clock_gettime`. A hot path
/// reads through [`clock_gettime_vdso`] instead (sys.md, "The clock").
/// replaces libc::clock_gettime (the system call form)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn clock_gettime(clock: ClockId) -> Result<Timespec, Errno> {
    let mut now = Timespec::ZERO;
    // SAFETY: `now` is a live `__kernel_timespec` (two 64-bit integers, any bytes valid) the
    // kernel writes and keeps no pointer to.
    raw::check(unsafe {
        raw::syscall2(nr::CLOCK_GETTIME, clock.raw(), core::ptr::from_mut(&mut now) as usize)
    })?;
    Ok(now)
}

/// `clock_gettime` through the vDSO: `clock` now, read from the kernel's clock page without
/// entering the kernel, where `vdso` has the kernel's `clock_gettime`
/// ([`super::vdso::Vdso::find`], made once by the driver and passed down).
///
/// Where `vdso` has none (a kernel built without a vDSO, or [`super::vdso::Vdso::NONE`]), the
/// stated path is the system call, [`clock_gettime`]: the absence is a fact of that kernel,
/// the same on every call, not a fallback. A nonzero answer of the vDSO's function is the
/// errno it negates, returned as it is ([`Errno::Inval`] for a clock the kernel lacks).
/// replaces libc::clock_gettime (glibc's vDSO path)
#[blaze::noalloc(trusted = "the vDSO's clock_gettime is the kernel's code: it reads the clock page and, for a clock it cannot read there, makes the system call; it calls no code of this process, so no allocator runs")]
#[blaze::nopanic]
pub fn clock_gettime_vdso(vdso: &super::vdso::Vdso, clock: ClockId) -> Result<Timespec, Errno> {
    let Some(read) = vdso.clock_gettime_fn() else { return clock_gettime(clock) };
    let mut now = Timespec::ZERO;
    // SAFETY: `read` is the kernel's `clock_gettime` in this process's vDSO (only
    // `Vdso::find` makes the pointer, from this process's own `AT_SYSINFO_EHDR`), mapped
    // executable for the process's life, with the C ABI vdso(7) documents. `now` is a live
    // `struct timespec` (two `i64`s on a 64-bit Linux, any bytes valid) it writes and keeps no
    // pointer to; the clock id is one of the kernel's (0, 1, 7).
    let said = unsafe { read(clock.raw() as core::ffi::c_int, core::ptr::from_mut(&mut now)) };
    // 0, or `-errno`: sign-extended, the raw call's result form `raw::check` reads.
    raw::check(said as isize as usize)?;
    Ok(now)
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

#[cfg(test)]
mod shared_futex {
    use core::sync::atomic::AtomicU32;

    use super::{FUTEX_WAIT_SHARED, FUTEX_WAKE_SHARED, futex_wait_shared, futex_wake_shared};
    use crate::{Errno, Timespec};

    /// The shared operations are libc's `FUTEX_WAIT` and `FUTEX_WAKE`; a wait on a word that
    /// changed is `EAGAIN`, one that holds ends at its time, and a wake with no waiter wakes 0.
    #[test]
    fn a_shared_futex_waits_and_wakes() {
        assert_eq!(FUTEX_WAIT_SHARED, libc::FUTEX_WAIT as usize);
        assert_eq!(FUTEX_WAKE_SHARED, libc::FUTEX_WAKE as usize);
        let word = AtomicU32::new(5);
        assert_eq!(futex_wait_shared(&word, 4, None), Err(Errno::Again));
        let left = Timespec::from_duration(std::time::Duration::from_millis(2));
        assert_eq!(futex_wait_shared(&word, 5, Some(left)), Err(Errno::TimedOut));
        assert_eq!(futex_wake_shared(&word, i32::MAX as u32), Ok(0));
    }

    /// The shared bitset wait is libc's `FUTEX_WAIT_BITSET` without the private flag, and a
    /// time already past ends it.
    #[test]
    fn a_shared_bitset_wait_ends_at_its_absolute_time() {
        use super::{ClockId, FUTEX_ANY, FUTEX_WAIT_BITSET_SHARED, clock_gettime, futex_wait_bitset_shared};
        assert_eq!(FUTEX_WAIT_BITSET_SHARED, libc::FUTEX_WAIT_BITSET as usize);
        let word = AtomicU32::new(5);
        assert_eq!(futex_wait_bitset_shared(&word, 4, None, FUTEX_ANY), Err(Errno::Again));
        let past = clock_gettime(ClockId::Monotonic).unwrap();
        assert_eq!(futex_wait_bitset_shared(&word, 5, Some(past), FUTEX_ANY), Err(Errno::TimedOut));
    }
}

#[cfg(test)]
mod clock_ids {
    use super::ClockId;

    /// Each clock's number is libc's, and a process's CPU clock is the one
    /// `clock_getcpuclockid` hands back.
    #[test]
    fn clock_numbers_are_libcs() {
        for (ours, theirs) in [
            (ClockId::Realtime, libc::CLOCK_REALTIME),
            (ClockId::Monotonic, libc::CLOCK_MONOTONIC),
            (ClockId::ProcessCpu, libc::CLOCK_PROCESS_CPUTIME_ID),
            (ClockId::ThreadCpu, libc::CLOCK_THREAD_CPUTIME_ID),
            (ClockId::MonotonicRaw, libc::CLOCK_MONOTONIC_RAW),
            (ClockId::Boottime, libc::CLOCK_BOOTTIME),
        ] {
            assert_eq!(ours.raw(), theirs as isize as usize, "{ours:?}");
            assert_eq!(ours.id(), theirs, "{ours:?}");
        }
        // SAFETY: getpid takes nothing and cannot fail.
        let pid = unsafe { libc::getpid() };
        let mut theirs: libc::clockid_t = 0;
        // SAFETY: `theirs` is a live clockid_t libc writes.
        assert_eq!(unsafe { libc::clock_getcpuclockid(pid, &mut theirs) }, 0);
        assert_eq!(ClockId::ProcessCpuOf(pid).raw(), theirs as isize as usize);
        assert!(super::clock_gettime(ClockId::ProcessCpuOf(pid)).is_ok());
    }

    /// A thread's CPU clock by tid is the one glibc's `pthread_getcpuclockid` gives for the
    /// calling thread's handle, and it reads.
    #[test]
    fn a_threads_cpu_clock_by_tid_is_glibcs() {
        let tid = crate::linux::sample::gettid();
        let mut theirs: libc::clockid_t = 0;
        // SAFETY: the calling thread's own handle, and a live clockid_t libc writes.
        assert_eq!(unsafe { libc::pthread_getcpuclockid(libc::pthread_self(), &mut theirs) }, 0);
        assert_eq!(ClockId::ThreadCpuOf(tid).id(), theirs);
        assert!(super::clock_gettime(ClockId::ThreadCpuOf(tid)).is_ok());
    }
}
