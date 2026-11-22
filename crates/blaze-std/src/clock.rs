//! The clock (docs/v2/plan/design/std.md, `Clock`): monotonic time on the driver's clock (the
//! simulation's is virtual), deadlines on it, and sleeps that end by the task's deadline; the
//! wall clock on the file system's time base ([`Clock::wall`]), and time since the process
//! began ([`Clock::since_process_start`]).

use alloc::{format, string::String, sync::Arc};
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
    #[core::contracts::ensures(move |r: &Option<Duration>| r.is_some() == (earlier.0 <= self.0))]
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn since(self, earlier: Instant) -> Option<Duration> {
        self.0.checked_sub(earlier.0)
    }

    /// The deadline at this moment.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn deadline(self) -> Deadline {
        Deadline::at(self.0)
    }
}

/// A reading of the wall clock ([`Clock::wall`]), in ns since the Unix epoch.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Wall {
    ns: u64,
}

impl Wall {
    /// Nanoseconds since the Unix epoch: what [`crate::fs::Stamp`]'s times count.
    pub fn as_ns(self) -> u64 {
        self.ns
    }

    pub fn secs(self) -> u64 {
        self.ns / 1_000_000_000
    }

    pub fn subsec_nanos(self) -> u32 {
        (self.ns % 1_000_000_000) as u32
    }
}

/// When the process began against the driver's clock, read once as the world is made
/// ([`crate::host::HostFacts::born`]).
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a test's description of a world
pub enum Born {
    /// The driver's clock began this long after the process did: 0 in the simulation, whose
    /// process begins with its clock.
    Before(Duration),
    /// The system would not say when the process began: what it said.
    Unknown(Arc<str>),
}

/// [`Clock::since_process_start`] on a system that would not say when the process began.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct ProcessStartUnknown {
    pub said: Arc<str>,
}

impl core::fmt::Display for ProcessStartUnknown {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "bstd::clock: when the process began is not known: {}", self.said)
    }
}

impl core::error::Error for ProcessStartUnknown {}

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
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn from(stopped: Stopped) -> SleepError {
        SleepError::Stopped(stopped)
    }
}

impl Clock {
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn new(io: Io) -> Clock {
        Clock { io }
    }

    pub fn now(&self) -> Instant {
        Instant(self.io.now())
    }

    /// The wall clock now, on the time base a [`crate::fs::Stamp`]'s times are on (the file
    /// system's: ns since the Unix epoch), so a stamp can be set against it: a change within
    /// the file system's clock step of a sweep's start is racy. A native driver reads the
    /// system's; the simulation's is its seed's start plus the virtual clock, the clock its
    /// disk stamps writes by, so a file written this instant has a stamp at this wall time.
    /// It may step back (a wall clock is set): nothing is ordered by it but stamps.
    pub fn wall(&self) -> Wall {
        Wall { ns: self.io.wall_ns() }
    }

    /// Time from the process's start (its exec) to `at`: what a program's first stamp says of
    /// the time before `main`, the loader's included. On macOS exact, on Linux in the kernel's
    /// 10 ms ticks; in the simulation it is [`Instant::since_start`].
    pub fn since_process_start(&self, at: Instant) -> Result<Duration, ProcessStartUnknown> {
        // clone: the error's reason, a refcount.
        match self.io.host_facts(|it| it.born.clone()) {
            Born::Before(before) => Ok(before.saturating_add(at.0)),
            Born::Unknown(said) => Err(ProcessStartUnknown { said }),
        }
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
