//! The clock the macOS control thread paces its passes by, passed in (rule 75): the driver asks
//! it the time and sleeps through it, never through an ambient call. Tests give a model clock.

use super::SampleError;

/// A monotonic clock in nanoseconds, and a sleep until a time on it.
pub trait Clock {
    fn now_ns(&self) -> Result<u64, SampleError>;
    /// Returns at or after `until_ns`, or early with nothing lost (the caller asks the time
    /// again).
    fn sleep_until_ns(&self, until_ns: u64) -> Result<(), SampleError>;
}

/// The system's monotonic clock (`clock_gettime(CLOCK_MONOTONIC)`) and `nanosleep`.
pub struct Monotonic;

impl Clock for Monotonic {
    fn now_ns(&self) -> Result<u64, SampleError> {
        let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: fills one live local; the result is checked before it is read.
        let failed = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
        if failed != 0 {
            return Err(SampleError::Kernel { call: "clock_gettime", code: errno(), thread: u32::MAX });
        }
        // A monotonic clock is never negative.
        let secs = u64::try_from(now.tv_sec).unwrap_or(0);
        let nanos = u64::try_from(now.tv_nsec).unwrap_or(0);
        Ok(secs.saturating_mul(1_000_000_000).saturating_add(nanos))
    }

    fn sleep_until_ns(&self, until_ns: u64) -> Result<(), SampleError> {
        let now = self.now_ns()?;
        let Some(wait) = until_ns.checked_sub(now) else { return Ok(()) };
        let span = libc::timespec {
            tv_sec: (wait / 1_000_000_000) as libc::time_t,
            tv_nsec: (wait % 1_000_000_000) as libc::c_long,
        };
        // SAFETY: reads one live local; no remainder is asked for (an early return is allowed by
        // the trait: the caller asks the time again).
        let failed = unsafe { libc::nanosleep(&span, core::ptr::null_mut()) };
        if failed != 0 && errno() != libc::EINTR {
            return Err(SampleError::Kernel { call: "nanosleep", code: errno(), thread: u32::MAX });
        }
        Ok(())
    }
}

fn errno() -> i32 {
    #[cfg(target_os = "macos")]
    // SAFETY: the calling thread's own `errno` cell, always valid.
    let cell = unsafe { libc::__error() };
    #[cfg(target_os = "linux")]
    // SAFETY: the calling thread's own `errno` cell, always valid.
    let cell = unsafe { libc::__errno_location() };
    // SAFETY: `cell` points at the calling thread's `errno`, valid for the thread's life.
    unsafe { *cell }
}
