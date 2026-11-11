//! The one way a thread blocks in the kernel (rule `deadline-waits`,
//! docs/v2/plan/design/deadline-waits.md).
//!
//! A wait takes an absolute [`Deadline`] on an injected [`Clock`] (a driver's `now()`, or a
//! [`Monotonic`] where there is no driver), recomputes the time left from that clock before
//! every system call, retries a call that was interrupted (EINTR) or woke with nothing, and
//! ends in a typed outcome: [`Waited::Ready`] or [`Waited::Deadline`]. A clock that does not
//! move across many calls ends the wait with a [`WaitError`] naming it (`bounded-progress`).
//! A relative timeout is never passed on to a retry: on 0.20261105.0 `Ring::enter` entered
//! io_uring again with the same 50 ms after each EINTR while the profile's sampler signalled
//! the thread every 20 ms, so the daemon's first turn never ended.
//!
//! The retry loop and the deadline's arithmetic are a sans-IO core (`wait/retry.rs`); the
//! system calls are the drivers beside it, one per kernel: `readable` (`poll`), `sleep_until`,
//! and the IO backends' one-shot waits, `uring_enter` (Linux), `kevent` (macOS), and
//! `completions` and `signaled` (Windows). The raw calls of these waits appear nowhere else
//! in blaze; `.clippy.toml` bans them outside this module.
//!
//! On Linux every other blocking wait is one request on the thread's own wait ring
//! (`wait/ring.rs`, an io_uring per waiting thread, its deadline in a registered wait region):
//! a [`Parker`]'s futex wait, a child's exit ([`exited`], and the wait of a reap), a poll of one
//! descriptor ([`pollable`]) and a sleep ([`sleep_until`]). The park's protocol and the reap's
//! are sans-IO cores (`wait/parking.rs`, `wait/reaping.rs`), proven by the DST in `wait/tests.rs`.
//!
//! On macOS a [`Parker`] waits on its word's address too, through the same core
//! (`wait/macos.rs`: `os_sync_wait_on_address_with_deadline` to an absolute Mach time and
//! `os_sync_wake_by_address_any`; docs/macos.md).
//!
//! A process inside blaze's sandbox, whose seccomp filter refuses io_uring, makes the same
//! waits with direct system calls instead (`wait/direct.rs`: `futex`, `ppoll`, `pidfd_open`,
//! `waitid`, `clock_nanosleep`), over the same cores. Which of the two a process uses is chosen
//! once, in its `main`, from the mark the sandbox launcher leaves in its environment
//! ([`choose_from_env`], `wait/choice.rs`); the ring is never tried to find out.

use std::time::{Duration, Instant};

mod choice;
mod parking;
mod reaping;
mod retry;

#[cfg(test)]
pub(crate) use self::retry::Plant;
pub use self::{
    choice::{MarkRefused, SANDBOXED, Waits, choose, choose_from_env},
    retry::{Clock, Deadline, Once, WaitError, Waited},
};

#[cfg(target_os = "linux")]
#[expect(
    unsafe_code,
    reason = "the waits of a process where io_uring is refused: futex, ppoll, pidfd_open, waitid and clock_nanosleep, one call each"
)]
mod direct;
#[cfg(target_os = "linux")]
#[expect(
    unsafe_code,
    reason = "test support: the seccomp filter a case runs under, and io_uring_setup asked once to see it refused"
)]
pub mod filtered;
#[cfg(target_os = "linux")]
#[expect(unsafe_code, reason = "Linux's waits: io_uring_enter, one call")]
mod linux;
#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "macOS's waits: kevent, mach_wait_until and the wait on an address and its wake, one call each"
)]
mod macos;
#[cfg(not(any(unix, windows)))]
mod other;
#[cfg(target_os = "linux")]
#[expect(
    unsafe_code,
    reason = "a thread's wait ring: io_uring_setup, io_uring_enter, io_uring_register and its mappings"
)]
mod ring;
#[cfg(unix)]
#[expect(unsafe_code, reason = "poll of the caller's descriptors, one call")]
mod unix;
#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Windows' waits: GetQueuedCompletionStatusEx and WaitForSingleObject, one call each"
)]
mod windows;

#[cfg(target_os = "linux")]
pub use self::linux::sleep_until;
#[cfg(target_os = "linux")]
pub(crate) use self::linux::uring_enter;
#[cfg(target_os = "macos")]
pub(crate) use self::macos::kevent;
#[cfg(target_os = "macos")]
pub use self::macos::sleep_until;
#[cfg(not(any(unix, windows)))]
pub use self::other::sleep_until;
#[cfg(unix)]
pub use self::unix::readable;
#[cfg(windows)]
pub(crate) use self::windows::completions;
#[cfg(windows)]
pub use self::windows::{signaled, sleep_until};

#[cfg(test)]
mod tests;

/// The system's monotonic clock, counted from an origin: for code that has no driver to ask
/// for `now()`. A driver's own clock is one of these on the instant it began
/// ([`Monotonic::since`]), so its waits and its `now()` agree.
#[derive(Clone, Copy, Debug)]
pub struct Monotonic {
    origin: Instant,
}

impl Monotonic {
    /// A clock at zero now.
    #[expect(
        clippy::disallowed_methods,
        reason = "the one real clock of code with no driver: waits read time only through it"
    )]
    pub fn new() -> Monotonic {
        Monotonic { origin: Instant::now() }
    }

    /// A clock at zero at `origin`.
    pub fn since(origin: Instant) -> Monotonic {
        Monotonic { origin }
    }
}

impl Default for Monotonic {
    fn default() -> Monotonic {
        Monotonic::new()
    }
}

impl Clock for Monotonic {
    #[expect(
        clippy::disallowed_methods,
        reason = "the one real clock of code with no driver: waits read time only through it"
    )]
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// Waits for `once` to be ready by `deadline` on `clock` (forever for `None`), retrying it
/// with the time left recomputed from `clock` before every call. `once` makes one system call
/// with the time it is given (`None`: no limit) and says how it went: [`Once::Ready`] ends the
/// wait; [`Once::Interrupted`], [`Once::Spurious`] and [`Once::TimedOut`] are retried until the
/// deadline has passed on `clock`, so a call that returned early waits again for what is left.
/// An error of `once` ends the wait, named by `what`; so does a clock that does not move.
pub fn until<T>(
    what: &'static str,
    clock: &impl Clock,
    deadline: Option<Deadline>,
    once: impl FnMut(Option<Duration>) -> std::io::Result<Once<T>>,
) -> Result<Waited<T>, WaitError> {
    // The wait-for graph's row for this wait (docs/v2/plan/design/named-locks.md): what this
    // thread is blocked on, so a convoy whose head sits in a kernel wait names it.
    let probe = crate::lock::installed();
    let registered = probe.map(|probe| {
        crate::lock::graph::register(
            crate::lock::stall::Blocked::Wait(what),
            probe.span(),
            probe.now_ns(),
            None,
            None,
        )
    });
    let waited = retry::drive(retry::Retry::new(what, deadline, clock.now()), clock, once);
    drop(registered);
    waited
}

/// One thread's park, woken from any thread: a token, as std's `thread::park` is (an unpark
/// before the park makes it return at once), with an absolute deadline on an injected clock.
/// On Linux the park is `IORING_OP_FUTEX_WAIT` on the thread's wait ring and the unpark
/// `IORING_OP_FUTEX_WAKE` on the waker's (a plain `FUTEX_WAKE` from a thread with no ring), or,
/// in a process that chose the direct driver ([`choose`]), `FUTEX_WAIT_BITSET` to an absolute
/// time and `FUTEX_WAKE`. On macOS the park is `os_sync_wait_on_address_with_deadline` on the
/// word to an absolute Mach time and the unpark `os_sync_wake_by_address_any` (`wait/macos.rs`).
/// Elsewhere std's park, until that system's address wait lands. Only one thread parks on a
/// `Parker`; nothing signals it.
pub struct Parker {
    word: std::sync::atomic::AtomicU32,
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    thread: crate::lock::Lock<Option<std::thread::Thread>>,
}

impl Parker {
    pub fn new() -> Parker {
        Parker {
            word: std::sync::atomic::AtomicU32::new(parking::EMPTY),
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            thread: crate::lock::Lock::unrecorded("wait.parker.thread", None),
        }
    }

    /// Parks the calling thread for `what` until an unpark (`Ready`) or `deadline` on `clock`
    /// (`Deadline`; forever for `None`).
    pub fn park(
        &self,
        what: &'static str,
        deadline: Option<Deadline>,
        clock: &impl Clock,
    ) -> Result<Waited<()>, WaitError> {
        let plant = parking::ParkPlant::None;
        #[cfg(target_os = "linux")]
        if choice::chosen() == Waits::Direct {
            let kernel = direct::Direct(&direct::Kernel);
            return parking::park(&self.word, &kernel, what, deadline, clock, plant);
        }
        parking::park(&self.word, &self.kernel(), what, deadline, clock, plant)
    }

    /// Wakes its thread, or makes its next park return at once.
    pub fn unpark(&self) {
        #[cfg(target_os = "linux")]
        if choice::chosen() == Waits::Direct {
            return parking::unpark(&self.word, &direct::Direct(&direct::Kernel));
        }
        parking::unpark(&self.word, &self.kernel());
    }

    /// The ring's futex: the kernel of a process that did not choose the direct driver.
    #[cfg(target_os = "linux")]
    fn kernel(&self) -> ring::Futexes {
        ring::Futexes
    }

    /// macOS's wait on an address: its futex.
    #[cfg(target_os = "macos")]
    fn kernel(&self) -> macos::Addresses {
        macos::Addresses
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn kernel(&self) -> ThreadPark<'_> {
        ThreadPark(&self.thread)
    }
}

impl Default for Parker {
    fn default() -> Parker {
        Parker::new()
    }
}

/// The park of a system with no wait of this module on an address yet: std's thread park,
/// the thread recorded before it says it is parked.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
struct ThreadPark<'a>(&'a crate::lock::Lock<Option<std::thread::Thread>>);

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl parking::Futex for ThreadPark<'_> {
    fn prepare(&self, _: &std::sync::atomic::AtomicU32) {
        *self.0.lock() = Some(std::thread::current());
    }

    fn wait<C: Clock>(
        &self,
        what: &'static str,
        word: &std::sync::atomic::AtomicU32,
        expected: u32,
        deadline: Option<Deadline>,
        clock: &C,
    ) -> Result<Waited<()>, WaitError> {
        until(what, clock, deadline, |left| {
            if word.load(std::sync::atomic::Ordering::Acquire) != expected {
                return Ok(Once::Ready(()));
            }
            match left {
                Some(left) => std::thread::park_timeout(left),
                None => std::thread::park(),
            }
            Ok(Once::Ready(()))
        })
    }

    fn wake(&self, _: &std::sync::atomic::AtomicU32) {
        if let Some(thread) = self.0.lock().as_ref() {
            thread.unpark();
        }
    }
}

/// Waits until the child `pid` has exited, by `deadline` on `clock` (forever for `None`),
/// leaving it unreaped: `IORING_OP_WAITID` with `WEXITED | WNOWAIT` on this thread's wait ring,
/// or, on the direct driver, a `ppoll` of its pidfd between two `waitid(WNOHANG | WNOWAIT)`.
/// A pid that is no child of this process (or one reaped already) fails with `ECHILD`.
#[cfg(target_os = "linux")]
pub fn exited(
    what: &'static str,
    pid: libc::pid_t,
    deadline: Option<Deadline>,
    clock: &impl Clock,
) -> Result<Waited<()>, WaitError> {
    use reaping::Children as _;
    match choice::chosen() {
        Waits::Ring => ring::Kids.exited(what, pid, true, deadline, clock),
        Waits::Direct => direct::Direct(&direct::Kernel).exited(what, pid, true, deadline, clock),
    }
}

/// Reaps the child `pid` once it has exited, by `deadline` on `clock`: its status and what it
/// used. The wait is [`exited`]'s, then one `wait4(WNOHANG)`.
#[cfg(target_os = "linux")]
pub(crate) fn reap(
    what: &'static str,
    pid: libc::pid_t,
    deadline: Option<Deadline>,
    clock: &impl Clock,
) -> Result<Waited<(libc::c_int, libc::rusage)>, WaitError> {
    let plant = reaping::ReapPlant::None;
    match choice::chosen() {
        Waits::Ring => reaping::reap(what, &ring::Kids, pid, deadline, clock, plant),
        Waits::Direct => {
            reaping::reap(what, &direct::Direct(&direct::Kernel), pid, deadline, clock, plant)
        }
    }
}

/// Waits until `fd` is ready for `events` (`POLLIN`, `POLLPRI`, ...), by `deadline` on `clock`
/// (forever for `None`): its `revents`. One `IORING_OP_POLL_ADD` on this thread's wait ring, or
/// `ppoll` on the direct driver.
#[cfg(target_os = "linux")]
pub fn pollable(
    what: &'static str,
    fd: std::os::fd::RawFd,
    events: libc::c_short,
    deadline: Option<Deadline>,
    clock: &impl Clock,
) -> Result<Waited<libc::c_short>, WaitError> {
    match choice::chosen() {
        Waits::Ring => ring::poll(what, fd, events, deadline, clock),
        Waits::Direct => direct::poll(what, fd, events, deadline, clock),
    }
}
