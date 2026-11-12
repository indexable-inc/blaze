//! The clock (docs/v2/plan/design/std.md, `Clock`): monotonic time on the driver's clock (the
//! simulation's is virtual), deadlines on it, and sleeps that end by the task's deadline. Wall
//! time needs an effect blaze-core does not have, so there is none yet.

use alloc::{format, string::String};
use core::time::Duration;

use blaze_core::{Done, Effect, wait::Deadline};

use crate::{Stopped, cx::Cx, io::Io};

/// A moment on the driver's monotonic clock, from when the driver began.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Instant(Duration);

impl Instant {
    /// Time since the driver began.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn since_start(self) -> Duration {
        self.0
    }

    /// Time from `earlier` to this; `None` when `earlier` is the later of the two (it was
    /// once zero then, unsaid).
    ///
    /// The contract: it answers exactly when `earlier` is not the later one.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn since(self, earlier: Instant) -> (r: Option<Duration>)
        ensures r.is_some() == (earlier.0 <= self.0)
    {
        self.0.checked_sub(earlier.0)
    }

    /// The deadline at this moment.
    #[blaze::pure]
    pub fn deadline(self) -> Deadline {
        Deadline::at(self.0)
    }
}

/// Reads the driver's clock: from [`Io::clock`].
#[derive(Clone)]
pub struct Clock {
    io: Io,
}

/// What a sleep's timer said other than that it fired.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum SleepError {
    Stopped(Stopped),
    /// The driver answered the timer with something else, or failed it.
    Driver { span: &'static str, said: String },
}

impl core::fmt::Display for SleepError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SleepError::Stopped(stopped) => write!(f, "bstd::clock sleep: {stopped}"),
            SleepError::Driver { span, said } => {
                write!(f, "bstd::clock sleep of {span}: the driver said {said}")
            }
        }
    }
}

impl core::error::Error for SleepError {}

impl From<Stopped> for SleepError {
    #[blaze::pure]
fn from(stopped: Stopped) -> SleepError {
        SleepError::Stopped(stopped)
    }
}

impl Clock {
    #[blaze::nopanic]
pub(crate) fn new(io: Io) -> Clock {
        Clock { io }
    }

    pub fn now(&self) -> Instant {
        Instant(self.io.now())
    }

    /// The deadline `wait` from now.
    pub fn after(&self, wait: Duration) -> Deadline {
        Deadline::after(self.io.now(), wait)
    }

    /// Waits until `at`, or until the task's deadline or cancellation, whichever comes first.
    pub fn sleep_until(&self, cx: &Cx, at: Deadline) -> Result<(), SleepError> {
        let after = at.left(self.io.now());
        match self.io.call(cx, |tag| Effect::Timer { after, tag })? {
            Done::Fired => Ok(()),
            done => Err(SleepError::Driver {
                span: cx.name(),
                // alloc: an error's message.
                said: format!("{done:?}"),
            }),
        }
    }

    /// Waits `wait`, or until the task's deadline or cancellation.
    pub fn sleep(&self, cx: &Cx, wait: Duration) -> Result<(), SleepError> {
        self.sleep_until(cx, self.after(wait))
    }
}
