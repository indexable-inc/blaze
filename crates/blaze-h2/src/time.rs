//! The time the caller gives: nanoseconds since an epoch the driver picks (its own monotonic
//! clock's start, or a simulation's zero). The machine never reads a clock (rule 75): every
//! entry point that can start or pass a deadline takes `now`.

#![blaze::sans_io]

use core::time::Duration;

/// A point in the driver's time.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Instant {
    nanos: u64,
}

impl Instant {
    /// The driver's epoch.
    pub const ZERO: Instant = Instant { nanos: 0 };

    /// `nanos` nanoseconds after the driver's epoch.
    pub const fn from_nanos(nanos: u64) -> Instant {
        Instant { nanos }
    }

    /// Nanoseconds since the driver's epoch.
    pub const fn nanos(self) -> u64 {
        self.nanos
    }

    /// `self` moved `by` later, saturating at the end of time (a deadline that never comes).
    pub fn after(self, by: Duration) -> Instant {
        let by = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        Instant { nanos: self.nanos.saturating_add(by) }
    }

    /// How long after `earlier` this is, zero if it is not after it.
    pub fn since(self, earlier: Instant) -> Duration {
        Duration::from_nanos(self.nanos.saturating_sub(earlier.nanos))
    }
}
