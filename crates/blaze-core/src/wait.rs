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

use std::time::{Duration, Instant};

mod retry;

pub use self::retry::{Clock, Deadline, Once, WaitError, Waited};
#[cfg(test)]
pub(crate) use self::retry::Plant;

#[cfg(target_os = "linux")]
#[allow(unsafe_code, reason = "Linux's waits: io_uring_enter and clock_nanosleep, one call each")]
mod linux;
#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "macOS's waits: kevent and mach_wait_until, one call each")]
mod macos;
#[cfg(not(any(unix, windows)))]
mod other;
#[cfg(unix)]
#[allow(unsafe_code, reason = "poll of the caller's descriptors, one call")]
mod unix;
#[cfg(windows)]
#[allow(
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
    retry::drive(retry::Retry::new(what, deadline, clock.now()), clock, once)
}
