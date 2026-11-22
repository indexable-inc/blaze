//! The clock the macOS control thread paces its passes by, passed in (rule 75): the driver asks
//! it the time and sleeps through it, never through an ambient call. Tests give a model clock.

use blaze_sys::posix::io::{ClockId, clock_gettime, nanosleep};
use blaze_sys::{Errno, Timespec};

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
        let now = clock_gettime(ClockId::Monotonic)
            .map_err(|error| SampleError::Kernel { call: "clock_gettime", code: i32::from(error.raw()), thread: u32::MAX })?;
        // A monotonic clock is never negative.
        let secs = u64::try_from(now.sec).unwrap_or(0);
        let nanos = u64::try_from(now.nsec).unwrap_or(0);
        Ok(secs.saturating_mul(1_000_000_000).saturating_add(nanos))
    }

    fn sleep_until_ns(&self, until_ns: u64) -> Result<(), SampleError> {
        let now = self.now_ns()?;
        let Some(wait) = until_ns.checked_sub(now) else { return Ok(()) };
        let span = Timespec::from_duration(core::time::Duration::from_nanos(wait));
        match nanosleep(span) {
            Ok(()) | Err(Errno::Intr) => Ok(()),
            Err(error) => Err(SampleError::Kernel { call: "nanosleep", code: i32::from(error.raw()), thread: u32::MAX }),
        }
    }
}

