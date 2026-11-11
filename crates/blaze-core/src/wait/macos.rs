//! macOS's waits: `kevent` for the kqueue driver, `mach_wait_until` for a sleep, and the
//! wait on an address (`os_sync_wait_on_address_with_deadline`) for a [`super::Parker`].

use std::{
    convert::Infallible,
    ffi::c_void,
    io,
    os::fd::RawFd,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
    time::Duration,
};

use super::{Clock, Deadline, Once, WaitError, Waited, parking::Futex, unix::timespec, until};

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
    fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
    fn mach_wait_until(deadline: u64) -> libc::c_int;
}

/// `KERN_ABORTED`: the wait was interrupted.
const KERN_ABORTED: libc::c_int = 14;

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
// There is no other park on macOS (rule no-fallback). A blaze started on a macOS before 14.4
// does not start: dyld stops it, naming the symbol it could not bind
// (`_os_sync_wait_on_address_with_deadline`). The test below resolves each at run time, so a
// system that drops one fails CI naming it.
//
// SAFETY: the signatures are the header's: `os_sync_wait_on_address_flags_t`,
// `os_sync_wake_by_address_flags_t` and `os_clockid_t` are `uint32_t` options and `size` is a
// `size_t`; each returns an `int`, -1 with `errno` set on an error.
unsafe extern "C" {
    fn os_sync_wait_on_address(addr: *mut c_void, value: u64, size: usize, flags: u32)
    -> libc::c_int;
    fn os_sync_wait_on_address_with_deadline(
        addr: *mut c_void,
        value: u64,
        size: usize,
        flags: u32,
        clockid: u32,
        deadline: u64,
    ) -> libc::c_int;
    fn os_sync_wake_by_address_any(addr: *mut c_void, size: usize, flags: u32) -> libc::c_int;
}

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
        until(what, clock, deadline, |left| {
            let addr = word.as_ptr().cast::<c_void>();
            let value = u64::from(expected);
            let got = match left {
                // The absolute time of this call alone, from what is left now.
                Some(left) => {
                    let at = mach_after(left)?;
                    // SAFETY: `word` is alive and aligned for the call, which only reads it;
                    // the thread blocks until a wake of the address, a signal or `at`.
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
                // wakes: the parker's `unpark`, an `os_sync_wake_by_address_any` of this word
                // SAFETY: as above, with no limit.
                None => unsafe { os_sync_wait_on_address(addr, value, WORD, PRIVATE) },
            };
            // Woken, or the word no longer held `expected`: the parker looks at it again.
            if got >= 0 {
                return Ok(Once::Ready(()));
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => Ok(Once::Interrupted),
                Some(libc::ETIMEDOUT) => Ok(Once::TimedOut),
                // The header's two transient returns under low memory (the kernel could not
                // allocate, or could not page the word in to read it; the word is a live
                // reference, so `EFAULT` is never a bad address): waited for again.
                Some(libc::ENOMEM | libc::EFAULT) => Ok(Once::Spurious),
                // alloc: an error.
                _ => Err(io::Error::new(
                    error.kind(),
                    format!("os_sync_wait_on_address_with_deadline: {error}"),
                )),
            }
        })
    }

    fn wake(&self, word: &AtomicU32) {
        // `ENOENT`, no thread waits on the word, is not an error of a wake: the parker took
        // its token before it blocked. Nothing else can fail a wake of a live aligned word.
        // SAFETY: a wake of a word of this process; the kernel reads no memory of it.
        _ = unsafe { os_sync_wake_by_address_any(word.as_ptr().cast(), WORD, PRIVATE) };
    }
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
    let mut nchanges = libc::c_int::try_from(nchanges).map_err(|_| too_many())?;
    let room = libc::c_int::try_from(room).map_err(|_| too_many())?;
    until(what, clock, deadline, |left| {
        let timeout = left.map(timespec);
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
            Some(libc::EINTR) => Ok(Once::Interrupted),
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
    nchanges: libc::c_int,
    events: *mut libc::kevent,
    room: libc::c_int,
    timeout: *const libc::timespec,
) -> libc::c_int {
    // SAFETY: the caller's pointers, valid for the counts given.
    unsafe { libc::kevent(kq, changes, nchanges, events, room, timeout) }
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
    use std::{
        sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
        },
        time::Duration,
    };

    use super::{
        super::{Clock as _, Deadline, Monotonic, Parker, Waited, parking},
        Addresses, Futex as _, PRIVATE, WORD, os_sync_wake_by_address_any, sleep_until,
    };

    /// The Mach calls the sleep is built on resolve at run time (rule macos-private-apis): an
    /// OS update that removes one fails here, naming it.
    #[test]
    fn the_mach_calls_of_a_sleep_resolve() {
        for name in [c"mach_wait_until", c"mach_absolute_time", c"mach_timebase_info"] {
            // SAFETY: a lookup of a NUL-terminated name in every image loaded.
            let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
            assert!(
                !found.is_null(),
                "{name:?} is gone from this macOS: wait::sleep_until needs it"
            );
        }
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
            // SAFETY: a lookup of a NUL-terminated name in every image loaded.
            let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
            assert!(
                !found.is_null(),
                "{name:?} is gone from this macOS (it is there from 14.4): wait::Parker \
                 needs it, and there is no other park"
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
            Some(libc::ENOENT),
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
