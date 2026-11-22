//! macOS's waits: `kevent` for the kqueue driver, `mach_wait_until` for a sleep, and the
//! wait on an address (`os_sync_wait_on_address_with_deadline`) for a [`super::Parker`].

use blaze_sys::{Errno, Timespec, posix::thread};
use core::ffi::c_int;
use std::{
    convert::Infallible,
    ffi::c_void,
    io,
    os::fd::RawFd,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use crate::atomic::AtomicU32;

use super::{Clock, Deadline, Once, WaitError, Waited, parking::Futex, until};

/// `struct mach_timebase_info`: mach time units are `numer / denom` nanoseconds.
#[repr(C)]
#[derive(Default)]
struct Timebase {
    numer: u32,
    denom: u32,
}

// Mach's clock and its absolute wait (rule macos-private-apis). `mach_wait_until` blocks the
// thread until a time on `mach_absolute_time`'s clock, so the kernel holds the deadline itself
// and nothing restarts a relative wait; `nanosleep`, the public sleep, takes a length and hands
// back a remainder to restart from. Resolved at run time by the test below, so an OS that
// drops it fails CI naming it.
unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> c_int;
    fn mach_wait_until(deadline: u64) -> c_int;
}

/// `KERN_ABORTED`: the wait was interrupted.
const KERN_ABORTED: c_int = 14;

// The wait on an address and its wake (rule macos-private-apis; docs/macos.md, "Parking"):
// libplatform's `os_sync_wait_on_address_with_deadline`, `os_sync_wait_on_address` and
// `os_sync_wake_by_address_any` (<os/os_sync_wait_on_address.h>, macOS 14.4 and later), the
// futex of this system: the kernel compares the word with the value expected and blocks the
// thread on the word's address in one step, so a wake between the parker's look and its wait
// is not lost, and the wake is one system call on that address from any thread. They are the
// documented face of the private `__ulock_wait2` and `__ulock_wake` (the calls
// `os_unfair_lock` and libc++'s `atomic::wait` block in) and win over them: the deadline is
// an absolute `mach_absolute_time`, the kernel's own clock, where `__ulock_wait2` takes a
// length; and their signatures are a header's, where the `__ulock` calls' operation numbers
// are XNU's to change. They win over what was here, std's `thread::park` behind a lock that
// held the parker's `Thread`: std parks on a dispatch semaphore, a Mach semaphore trap for
// each wait and each wake, and the wake took that lock; this is one call each on the word
// the park's core already keeps, with nothing else shared.
//
// There is no other park on macOS (rule no-fallback). They are reached through blaze-sys's
// table of calls found at first use (`blaze_sys::macos::cold`), not bound at load: a program
// that never parks never pays for them. A blaze on a macOS before 14.4 stops at its first park,
// naming the symbol it could not find. The test below and blaze-sys's
// `every_cold_symbol_exists` resolve each at run time, so a system that drops one fails CI
// naming it. The signatures are the header's, in the table: `os_sync_wait_on_address_flags_t`,
// `os_sync_wake_by_address_flags_t` and `os_clockid_t` are `uint32_t` options and `size` is a
// `size_t`; each returns an `int`, -1 with `errno` set on an error.
use blaze_sys::macos::cold::{
    os_sync_wait_on_address, os_sync_wait_on_address_with_deadline, os_sync_wake_by_address_all,
    os_sync_wake_by_address_any,
};

/// `OS_CLOCK_MACH_ABSOLUTE_TIME` (<os/clock.h>): the one clock a deadline may be on.
const OS_CLOCK_MACH_ABSOLUTE_TIME: u32 = 32;
/// `OS_SYNC_WAIT_ON_ADDRESS_NONE` and `OS_SYNC_WAKE_BY_ADDRESS_NONE`: a word of this process
/// alone, as Linux's `FUTEX_PRIVATE_FLAG`.
const PRIVATE: u32 = 0;
/// The size of the word waited on: an `AtomicU32`.
const WORD: usize = size_of::<u32>();

/// Mach's timebase, read once: `numer << 32 | denom`, 0 until then. It never changes while
/// the system runs, so a race to store it stores the same value.
static TIMEBASE: AtomicU64 = AtomicU64::new(0);

/// Mach time units per nanosecond, as `(numer, denom)`: a unit is `numer / denom` ns.
fn timebase() -> io::Result<(u32, u32)> {
    let known = TIMEBASE.load(Ordering::Relaxed);
    if known != 0 {
        return Ok(((known >> 32) as u32, known as u32));
    }
    let mut base = Timebase::default();
    // SAFETY: a struct for the kernel to write.
    let got = unsafe { mach_timebase_info(&mut base) };
    if got != 0 || base.numer == 0 || base.denom == 0 {
        // alloc: an error.
        return Err(io::Error::other(format!(
            "mach_timebase_info returned {got} with {}/{}",
            base.numer, base.denom
        )));
    }
    TIMEBASE.store((u64::from(base.numer) << 32) | u64::from(base.denom), Ordering::Relaxed);
    Ok((base.numer, base.denom))
}

/// How long ago this process began, on `mach_absolute_time`'s clock, the one `Instant` and so
/// every driver's `now()` reads (it stops while the machine sleeps, as they do).
///
/// `proc_pid_rusage(RUSAGE_INFO_V4)` (<libproc.h>, public) gives `ri_proc_start_abstime`, the
/// exec's instant on that same clock, so the age is one subtraction with no clock between.
/// It beats `proc_pidinfo(PROC_PIDTBSDINFO)` and the `kern.proc.pid` sysctl, the other two
/// ways: they give the start as wall-clock microseconds, which a wall-clock step moves and
/// which would have to be set against a second clock read at another instant.
pub(crate) fn process_age() -> io::Result<Duration> {
    let mut info = std::mem::MaybeUninit::<thread::rusage_info_v4>::zeroed();
    // SAFETY: the flavour says the buffer is a `rusage_info_v4`, which `info` is, for the
    // kernel to fill; this process's own id needs no right.
    let got = unsafe {
        thread::proc_pid_rusage(thread::getpid(), thread::RUSAGE_INFO_V4, info.as_mut_ptr().cast())
    };
    if got != 0 {
        let error = io::Error::last_os_error();
        // alloc: an error.
        return Err(io::Error::new(error.kind(), format!("proc_pid_rusage(RUSAGE_INFO_V4): {error}")));
    }
    // sys/resource.h: ri_proc_start_abstime is word 8 following the UUID in rusage_info_v4.
    const START_ABSTIME: usize = 8;
    // SAFETY: filled by the call that returned 0.
    let began = unsafe { info.assume_init() }.words[START_ABSTIME];
    let (numer, denom) = timebase()?;
    // SAFETY: a read of Mach's clock.
    let now = unsafe { mach_absolute_time() };
    let ticks = now.checked_sub(began).ok_or_else(|| {
        // alloc: an error.
        io::Error::other(format!("proc_pid_rusage: the process began at {began}, after now ({now})"))
    })?;
    let ns = u128::from(ticks) * u128::from(numer) / u128::from(denom);
    Ok(Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX)))
}

/// The time on `mach_absolute_time`'s clock `left` from now, rounded up to a unit; the
/// clock's end where that is past it.
fn mach_after(left: Duration) -> io::Result<u64> {
    let (numer, denom) = timebase()?;
    let ticks = left.as_nanos().saturating_mul(u128::from(denom)).div_ceil(u128::from(numer));
    // SAFETY: a read of Mach's clock.
    let now = unsafe { mach_absolute_time() };
    Ok(now.saturating_add(u64::try_from(ticks).unwrap_or(u64::MAX)))
}

/// macOS's futex: the kernel a [`super::Parker`] waits in, a wait on its word's address.
pub(crate) struct Addresses;

impl Futex for Addresses {
    fn wait<C: Clock>(
        &self,
        what: &'static str,
        word: &AtomicU32,
        expected: u32,
        deadline: Option<Deadline>,
        clock: &C,
    ) -> Result<Waited<()>, WaitError> {
        until(what, clock, deadline, |left| address_once(word, expected, left))
    }

    fn wake(&self, word: &AtomicU32) {
        word_wake(word, false);
    }
}

/// The wait of a lock's word ([`super::word_wait`]): the same call as a park's, any number of
/// threads on one word, and no row of its own in the wait-for graph (the lock's is the row).
pub(crate) fn word_wait(
    what: &'static str,
    word: &AtomicU32,
    expected: u32,
    deadline: Option<Deadline>,
    clock: &impl Clock,
) -> Result<Waited<()>, WaitError> {
    super::unlisted(what, clock, deadline, |left| address_once(word, expected, left))
}

/// The wake of a word: one waiter (`os_sync_wake_by_address_any`) or every one
/// (`os_sync_wake_by_address_all`), of this process.
pub(crate) fn word_wake(word: &AtomicU32, all: bool) {
    let addr = word.as_ptr().cast::<c_void>();
    // `ENOENT`, no thread waits on the word, is not an error of a wake: the waiter looked at
    // the word again before it blocked. Nothing else can fail a wake of a live aligned word.
    // SAFETY: a wake of a word of this process; the kernel reads no memory of it.
    _ = unsafe {
        match all {
            true => os_sync_wake_by_address_all(addr, WORD, PRIVATE),
            false => os_sync_wake_by_address_any(addr, WORD, PRIVATE),
        }
    };
}

/// One wait on `word`'s address while it holds `expected`, for at most `left` (no limit for
/// `None`): the call of a park and of a lock's word alike.
fn address_once(word: &AtomicU32, expected: u32, left: Option<Duration>) -> io::Result<Once<()>> {
    let addr = word.as_ptr().cast::<c_void>();
    let value = u64::from(expected);
    let got = match left {
        // The absolute time of this call alone, from what is left now.
        Some(left) => {
            let at = mach_after(left)?;
            // SAFETY: `word` is alive and aligned for the call, which only reads it; the thread
            // blocks until a wake of the address, a signal or `at`.
            unsafe {
                os_sync_wait_on_address_with_deadline(
                    addr,
                    value,
                    WORD,
                    PRIVATE,
                    OS_CLOCK_MACH_ABSOLUTE_TIME,
                    at,
                )
            }
        }
        // wakes: the parker's `unpark` or a lock's release, an `os_sync_wake_by_address_*` of
        // this word
        // SAFETY: as above, with no limit.
        None => unsafe { os_sync_wait_on_address(addr, value, WORD, PRIVATE) },
    };
    // Woken, or the word no longer held `expected`: the waiter looks at it again.
    if got >= 0 {
        return Ok(Once::Ready(()));
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code == i32::from(Errno::Intr.raw()) => Ok(Once::Interrupted),
        Some(code) if code == i32::from(Errno::TimedOut.raw()) => Ok(Once::TimedOut),
        // The header's two transient returns under low memory (the kernel could not allocate,
        // or could not page the word in to read it; the word is a live reference, so `EFAULT`
        // is never a bad address): waited for again.
        Some(code) if code == i32::from(Errno::NoMem.raw()) || code == i32::from(Errno::Fault.raw()) => Ok(Once::Spurious),
        // alloc: an error.
        _ => Err(io::Error::new(
            error.kind(),
            format!("os_sync_wait_on_address_with_deadline: {error}"),
        )),
    }
}

/// `OS_SYNC_WAIT_ON_ADDRESS_SHARED` and `OS_SYNC_WAKE_BY_ADDRESS_SHARED`: a word other processes
/// map too. The kernel keys the wait by the shared mapping's object, not the address, so
/// processes that map one file meet on one word (the documented face of the private
/// `__ulock_wait2` with `UL_COMPARE_AND_WAIT_SHARED`; the header's flags win over XNU's
/// operation numbers, as for the park above).
const SHARED: u32 = 1;

/// The shared-address wait of [`super::wait_shared`]: one
/// `os_sync_wait_on_address_with_deadline` with [`SHARED`] per call, to the absolute Mach time
/// of what is left of `deadline` on `clock` now (never a length carried across a retry), on
/// the low half of `word`.
pub(crate) fn wait_shared(
    what: &'static str,
    word: &AtomicU64,
    expected: u32,
    deadline: Deadline,
    clock: &impl Clock,
) -> Result<Waited<()>, WaitError> {
    // The low half of a little-endian word is at its address (`super::wait_shared`'s contract).
    let addr = word.as_ptr().cast::<c_void>();
    until(what, clock, Some(deadline), |left| {
        let at = mach_after(left.unwrap_or_default())?;
        // SAFETY: `word` is alive and aligned for the call, which only reads its low four
        // bytes; the thread blocks until a wake of the word from any process that maps it, a
        // signal or `at`.
        let got = unsafe {
            os_sync_wait_on_address_with_deadline(
                addr,
                u64::from(expected),
                WORD,
                SHARED,
                OS_CLOCK_MACH_ABSOLUTE_TIME,
                at,
            )
        };
        // Woken, or the word no longer held `expected`: the caller looks again.
        if got >= 0 {
            return Ok(Once::Ready(()));
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(code) if code == i32::from(Errno::Intr.raw()) => Ok(Once::Interrupted),
            Some(code) if code == i32::from(Errno::TimedOut.raw()) => Ok(Once::TimedOut),
            // The header's transient returns under low memory: waited for again.
            Some(code) if code == i32::from(Errno::NoMem.raw()) || code == i32::from(Errno::Fault.raw()) => Ok(Once::Spurious),
            // alloc: an error.
            _ => Err(io::Error::new(
                error.kind(),
                format!("os_sync_wait_on_address_with_deadline, shared: {error}"),
            )),
        }
    })
}

/// The wake of [`super::wake_shared`]: every thread of every process waiting on `word`'s low
/// half.
pub(crate) fn wake_shared(word: &AtomicU64) {
    // `ENOENT`, nobody waits on the word, is not an error of a wake; nothing else can fail a
    // wake of a live aligned word.
    // SAFETY: a wake of a word of a shared mapping; the kernel reads no memory of it.
    _ = unsafe { os_sync_wake_by_address_all(word.as_ptr().cast(), WORD, SHARED) };
}

/// One `kevent` on `kq`: the `nchanges` changes at `changes` made, then up to `room` events
/// written at `events`, waiting for the first until `deadline` on `clock` (forever for
/// `None`). The changes are made by the first call only: the kernel makes them before it
/// waits, interrupted or not, so a retry passes none. `Ready` with how many events came;
/// with `room` 0 the call never waits and is `Ready(0)` once the changes are made.
///
/// # Safety
///
/// `changes` points at `nchanges` `kevent`s to read and `events` at room for `room` to write,
/// both valid until this returns.
pub(crate) unsafe fn kevent(
    what: &'static str,
    kq: RawFd,
    changes: *const libc::kevent,
    nchanges: usize,
    events: *mut libc::kevent,
    room: usize,
    deadline: Option<Deadline>,
    clock: &impl Clock,
) -> Result<Waited<usize>, WaitError> {
    let too_many =
        || WaitError::io(what, io::Error::new(io::ErrorKind::InvalidInput, "too many kevents"));
    let mut nchanges = c_int::try_from(nchanges).map_err(|_| too_many())?;
    let room = c_int::try_from(room).map_err(|_| too_many())?;
    until(what, clock, deadline, |left| {
        let timeout = left.map(Timespec::from_duration);
        let timeout = timeout.as_ref().map_or(std::ptr::null(), std::ptr::from_ref);
        // SAFETY: the caller's changes and room, and a timeout that outlives the call.
        let got = unsafe { kevent_once(kq, changes, nchanges, events, room, timeout) };
        nchanges = 0;
        if got > 0 || (got == 0 && room == 0) {
            return Ok(Once::Ready(got as usize));
        }
        if got == 0 {
            return Ok(Once::TimedOut);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(code) if code == i32::from(Errno::Intr.raw()) => Ok(Once::Interrupted),
            _ => Err(error),
        }
    })
}

/// One `kevent` call.
///
/// # Safety
///
/// As [`kevent`]'s, and `timeout` is null or a timespec valid for the call.
#[expect(
    clippy::disallowed_methods,
    reason = "the one `kevent` of `wait::kevent`, its timeout recomputed from the deadline"
)]
unsafe fn kevent_once(
    kq: RawFd,
    changes: *const libc::kevent,
    nchanges: c_int,
    events: *mut libc::kevent,
    room: c_int,
    timeout: *const Timespec,
) -> c_int {
    // SAFETY: the caller's pointers are valid for the counts given. Timespec has the
    // macOS timespec layout (two signed 64-bit fields in C order), including its alignment.
    unsafe { libc::kevent(kq, changes, nchanges, events, room, timeout.cast()) }
}

/// Sleeps until `deadline` on `clock`. Each call is a `mach_wait_until` to what is left on
/// `clock` from Mach's now, an absolute time the kernel holds, so an interrupted sleep sleeps
/// again for only what is left.
pub fn sleep_until(
    what: &'static str,
    deadline: Deadline,
    clock: &impl Clock,
) -> Result<(), WaitError> {
    let waited = until(what, clock, Some(deadline), |left| -> io::Result<Once<Infallible>> {
        let at = mach_after(left.unwrap_or_default())?;
        // SAFETY: blocks this thread until `at`; no memory is passed.
        match unsafe { mach_wait_until(at) } {
            0 => Ok(Once::TimedOut),
            KERN_ABORTED => Ok(Once::Interrupted),
            // alloc: an error.
            error => Err(io::Error::other(format!("mach_wait_until returned {error}"))),
        }
    })?;
    match waited {
        Waited::Ready(never) => match never {},
        Waited::Deadline => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use blaze_sys::{Errno, posix::thread::{RTLD_DEFAULT, symbol}};
    use std::{
        sync::{Arc, atomic::Ordering},
        time::Duration,
    };

    use crate::atomic::AtomicU32;

    use super::{
        super::{Clock as _, Deadline, Monotonic, Parker, Waited, parking},
        Addresses, Futex as _, PRIVATE, WORD, os_sync_wake_by_address_any, sleep_until,
    };

    /// The Mach calls the sleep is built on resolve at run time (rule macos-private-apis): an
    /// OS update that removes one fails here, naming it.
    #[test]
    fn the_mach_calls_of_a_sleep_resolve() {
        for name in [c"mach_wait_until", c"mach_absolute_time", c"mach_timebase_info"] {
            // SAFETY: RTLD_DEFAULT is the loader's handle for every loaded image.
            let found = unsafe { symbol(RTLD_DEFAULT, name) };
            assert!(
                found.is_some(),
                "{name:?} is gone from this macOS: wait::sleep_until needs it"
            );
        }
    }

    /// The process began before now, and not long before: the test binary started it within
    /// this run, and a second look is no younger than the first.
    #[test]
    fn the_process_began_a_moment_ago() {
        let first = super::process_age().expect("the process's start");
        let second = super::process_age().expect("the process's start");
        assert!(first > Duration::ZERO && first < Duration::from_secs(3600), "{first:?}");
        assert!(second >= first, "{second:?} then {first:?}");
    }

    /// The calls a park is built on resolve at run time (rule macos-private-apis): a macOS
    /// without one (before 14.4, or an update that removes it) fails here, naming it.
    #[test]
    fn the_address_waits_of_a_park_resolve() {
        for name in [
            c"os_sync_wait_on_address_with_deadline",
            c"os_sync_wait_on_address",
            c"os_sync_wake_by_address_any",
        ] {
            // SAFETY: RTLD_DEFAULT is the loader's handle for every loaded image.
            let found = unsafe { symbol(RTLD_DEFAULT, name) };
            assert!(
                found.is_some(),
                "{name:?} is gone from this macOS (it is there from 14.4): wait::Parker \
                 needs it, and there is no other park"
            );
        }
    }

    /// The calls the shared-address wait is built on resolve at run time (rule
    /// macos-private-apis): a macOS without one fails here, naming it.
    #[test]
    fn the_shared_address_waits_resolve() {
        for name in [c"os_sync_wait_on_address_with_deadline", c"os_sync_wake_by_address_all"] {
            // SAFETY: RTLD_DEFAULT is the loader's handle for every loaded image.
            let found = unsafe { symbol(RTLD_DEFAULT, name) };
            assert!(
                found.is_some(),
                "{name:?} is gone from this macOS (it is there from 14.4): wait::wait_shared \
                 needs it, and there is no other cross-process wait"
            );
        }
    }

    /// One wake of `word` by the raw call: whether a thread waited on it.
    fn wake(word: &AtomicU32) -> bool {
        // SAFETY: a wake of a live word; the kernel reads no memory of it.
        let got = unsafe { os_sync_wake_by_address_any(word.as_ptr().cast(), WORD, PRIVATE) };
        if got == 0 {
            return true;
        }
        let error = std::io::Error::last_os_error();
        assert_eq!(
            error.raw_os_error(),
            Some(i32::from(Errno::NoEnt.raw())),
            "a wake of a word nobody waits on fails with something other than ENOENT: {error}"
        );
        false
    }

    /// A wait on a word that holds what is expected, with no wake, ends as its deadline
    /// passes on the clock, and not before.
    #[test]
    fn a_wait_nobody_wakes_ends_at_its_deadline() {
        let clock = Monotonic::new();
        let word = AtomicU32::new(parking::PARKED);
        let wait = Duration::from_millis(30);
        let deadline = Deadline::after(clock.now(), wait);
        let waited = Addresses
            .wait("a test's wait nobody wakes", &word, parking::PARKED, Some(deadline), &clock)
            .unwrap();
        assert_eq!(waited, Waited::Deadline);
        let took = clock.now();
        assert!(took >= wait, "the wait ended after {took:?}, before its deadline of {wait:?}");
        assert!(took < Duration::from_secs(20), "the wait took {took:?}");
    }

    /// A wait on a word that no longer holds what is expected returns at once: the kernel's
    /// compare, which is what keeps a wake that came before the wait.
    #[test]
    fn a_wait_on_a_word_that_moved_returns_at_once() {
        let clock = Monotonic::new();
        let word = AtomicU32::new(parking::NOTIFIED);
        let deadline = Deadline::after(clock.now(), Duration::from_secs(60));
        let waited = Addresses
            .wait("a test's wait on a moved word", &word, parking::PARKED, Some(deadline), &clock)
            .unwrap();
        assert_eq!(waited, Waited::Ready(()));
        assert!(clock.now() < Duration::from_secs(20), "it waited for its deadline");
    }

    /// A wake of a word nobody waits on is nothing (`ENOENT`, which the park's wake drops),
    /// and leaves no token in the kernel: a wait after it still waits to its deadline.
    #[test]
    fn a_wake_with_no_waiter_is_nothing() {
        let clock = Monotonic::new();
        let word = AtomicU32::new(parking::PARKED);
        assert!(!wake(&word), "a wake found a waiter on a word nobody waits on");
        Addresses.wake(&word);
        let wait = Duration::from_millis(20);
        let deadline = Deadline::after(clock.now(), wait);
        let waited = Addresses
            .wait("a test's wait after a wake", &word, parking::PARKED, Some(deadline), &clock)
            .unwrap();
        assert_eq!(waited, Waited::Deadline, "a wake before the wait was kept by the kernel");
    }

    /// A thread blocked in the kernel on a word is woken by a wake of it, with and without a
    /// deadline: the wait ends `Ready` long before any deadline, the word unchanged.
    #[test]
    fn a_wait_is_woken_by_a_wake_of_its_word() {
        for limited in [true, false] {
            let word = Arc::new(AtomicU32::new(parking::PARKED));
            let waiter = std::thread::spawn({
                let word = word.clone();
                move || {
                    let clock = Monotonic::new();
                    let deadline =
                        limited.then(|| Deadline::after(clock.now(), Duration::from_secs(120)));
                    let waited = Addresses
                        .wait("a test's wait for a wake", &word, parking::PARKED, deadline, &clock)
                        .unwrap();
                    (waited, clock.now())
                }
            });
            // Wakes until one finds the thread waiting: bounded, a minute of them.
            let clock = Monotonic::new();
            let mut woke = false;
            for _ in 0..60_000 {
                if wake(&word) {
                    woke = true;
                    break;
                }
                let next = Deadline::after(clock.now(), Duration::from_millis(1));
                sleep_until("a test's pause between wakes", next, &clock).unwrap();
            }
            assert!(woke, "no wake found the thread waiting on the word (limited: {limited})");
            let (waited, took) = waiter.join().unwrap();
            assert_eq!(waited, Waited::Ready(()));
            assert!(took < Duration::from_secs(100), "woken only after {took:?}");
            assert_eq!(word.load(Ordering::Relaxed), parking::PARKED);
        }
    }

    /// The whole park on the real kernel: an unpark from another thread ends it `Ready`, a
    /// token left before a park is taken at once, and a park nobody unparks ends `Deadline`.
    #[test]
    fn a_parker_is_woken_keeps_a_token_and_ends_at_its_deadline() {
        let parker = Arc::new(Parker::new());
        let parked = std::thread::spawn({
            let parker = parker.clone();
            move || {
                let clock = Monotonic::new();
                let deadline = Deadline::after(clock.now(), Duration::from_secs(120));
                let waited = parker.park("a test's park", Some(deadline), &clock).unwrap();
                (waited, clock.now())
            }
        });
        let clock = Monotonic::new();
        // Long enough that the thread is most often blocked by now; right either way.
        let pause = Deadline::after(clock.now(), Duration::from_millis(50));
        sleep_until("a test's pause before its unpark", pause, &clock).unwrap();
        parker.unpark();
        let (waited, took) = parked.join().unwrap();
        assert_eq!(waited, Waited::Ready(()));
        assert!(took < Duration::from_secs(100), "unparked only after {took:?}");

        parker.unpark();
        let deadline = Deadline::after(clock.now(), Duration::from_secs(120));
        let began = clock.now();
        let waited = parker.park("a test's park on a token", Some(deadline), &clock).unwrap();
        assert_eq!(waited, Waited::Ready(()));
        assert!(clock.now() - began < Duration::from_secs(100), "the token was not taken at once");

        let wait = Duration::from_millis(20);
        let began = clock.now();
        let deadline = Deadline::after(began, wait);
        let waited = parker.park("a test's park nobody unparks", Some(deadline), &clock).unwrap();
        assert_eq!(waited, Waited::Deadline);
        assert!(clock.now() - began >= wait, "the park ended before its deadline");
    }
}
