//! Time as a number, the base every crate can name: [`Nanos`] (nanoseconds on one monotonic
//! clock's origin), [`Deadline`] (an absolute time by which a wait ends) and [`Clock`] (where a
//! `now` comes from, always handed in: rule 75, nothing ambient). Here because this crate depends
//! on nothing and every crate may depend on it, so a sans-IO core takes `&impl Clock` without
//! reaching for blaze-core or bstd. No clock is read here: a driver implements [`Clock`] over
//! `clock_gettime` (blaze-sys), a test over a counter or a simulation's virtual time.
//!
//! # The clocks this one replaces (none moved yet; each becomes this one by these steps)
//!
//! Each is a one-method trait over its own number. The move, per trait: its impls implement
//! `blaze_int::Clock` (`now` gives `Nanos`), its users take `&impl blaze_int::Clock`, and the
//! old trait is deleted with a ban on its name (rule 30). A trait that also sleeps or can fail
//! keeps that part as its own trait over this one.
//!
//! | trait | its `now` | becomes | its impls and users |
//! |---|---|---|---|
//! | blaze-core `wait::retry::Clock` (wait/retry.rs:17), with its `Deadline(Duration)` (:24) | `Duration` | `Nanos` (`Nanos::from_duration`); its `Deadline` is this [`Deadline`], the same methods (`PASSED`, `NEVER`, `at`, `after`, `get`, `left`, `passed`, `finite`) | impls: wait.rs:156 `Monotonic`, sim.rs:2950 and wait/tests.rs:101 `Virtual`, wait/parking.rs:194 `Still` and :292 `Shown`, blaze-std procs/linked.rs:82 `DriverClock`, blaze-cli engine_scripts.rs:252 `WaitClock`, blaze-ladder's two `Mono`, blaze-real examples/check.rs:218; takers: every `clock: &impl Clock` of wait.rs (:174, :208, :232, :267, :319, :336), busy.rs, host.rs, fanotify.rs, lock/raw.rs, world/clock.rs, iocp/sys.rs, blaze-spin, blaze-std io/fast.rs and task/help.rs. blaze-core must first take blaze-int as a dependency (it has none today) |
//! | blaze-stage `cx::Clock: Sync` (cx.rs:27) | `u64` ns | `Nanos`; the `Sync` bound stays on the takers (`&(impl Clock + Sync)`) | impl: sim_tests.rs:234 `SimClock`; the driver's clock in blaze-cli |
//! | blaze-typeck `asks::Clock` (asks.rs:19) | `now_ns() -> u64` | `now() -> Nanos` | impls: tests.rs:357, suite/op_fast_path.rs:35, suite/op_literal_vars.rs:59 (`Ticks`), examples/cost.rs:43 `Wall` |
//! | blaze-trace `capture::Clock: Sync` (capture.rs:23) | `ticks() -> u64` | `Nanos` read as ticks (a count, not a time: `Counter` keeps counting) | impl: capture.rs:43 `Counter` |
//! | blaze-launch-rt `zygote::Clock` (zygote.rs:58) | `Result<Nanos, i32>` | `Nanos`: `clock_gettime(CLOCK_MONOTONIC)` does not fail on a supported host, so the driver stops once at start if it does | impls: zygote.rs:65 `Monotonic`, stub_wait_model.rs:46 and stub_wait_tests.rs:112 `Steps` |
//! | blaze-launch-rt `sample::clock::Clock` (sample/clock.rs:7) | `now_ns() -> Result<u64, _>`, and `sleep_until_ns` | `now` is this; the sleep stays its own trait `Sleep: Clock` | impl: sample/clock.rs:17 `Monotonic` |
//!
//! Not a trait and not replaced: blaze-std's `clock::Clock` (clock.rs:97) is a capability
//! STRUCT over its `Io` whose `now()` gives an `Instant`; it implements this trait (its `now`
//! as nanoseconds since its origin), and bstd's `Instant` keeps its own type.
//!
//! The `Nanos` newtypes this one replaces: blaze-watch lib.rs:71, blaze-core claim_watch.rs:38,
//! blaze-run lib.rs:131, blaze-sim flight.rs:20 and blaze-launch-rt's (each `Nanos(pub u64)`,
//! the same layout, so a move is the import alone). blaze-text show.rs:228 is a display of a
//! count, not a time, and stays.

use core::time::Duration;

/// Nanoseconds on one monotonic clock's origin: a time when read from a [`Clock`], a length
/// when two are subtracted. 584 years fit; every operation that could pass that saturates.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Nanos(pub u64);

impl Nanos {
    pub const ZERO: Nanos = Nanos(0);
    pub const MAX: Nanos = Nanos(u64::MAX);

    #[blaze::pure]
    pub const fn from_secs(secs: u64) -> Nanos {
        Nanos(secs.saturating_mul(1_000_000_000))
    }

    #[blaze::pure]
    pub const fn from_millis(millis: u64) -> Nanos {
        Nanos(millis.saturating_mul(1_000_000))
    }

    #[blaze::pure]
    pub const fn from_micros(micros: u64) -> Nanos {
        Nanos(micros.saturating_mul(1_000))
    }

    /// `duration` in nanoseconds, [`Nanos::MAX`] where it is longer.
    #[blaze::pure]
    pub const fn from_duration(duration: Duration) -> Nanos {
        Nanos(crate::Cast::<u128, u64>::saturate(duration.as_nanos()))
    }

    #[blaze::pure]
    pub const fn to_duration(self) -> Duration {
        Duration::from_nanos(self.0)
    }

    #[blaze::pure]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[blaze::pure]
    pub const fn saturating_add(self, other: Nanos) -> Nanos {
        Nanos(self.0.saturating_add(other.0))
    }

    #[blaze::pure]
    pub const fn saturating_sub(self, other: Nanos) -> Nanos {
        Nanos(self.0.saturating_sub(other.0))
    }

    #[blaze::pure]
    pub const fn checked_add(self, other: Nanos) -> Option<Nanos> {
        match self.0.checked_add(other.0) {
            Some(sum) => Some(Nanos(sum)),
            None => None,
        }
    }

    /// How long after `earlier` this is: zero when `earlier` is not before it.
    #[blaze::pure]
    pub const fn since(self, earlier: Nanos) -> Nanos {
        self.saturating_sub(earlier)
    }
}

/// An absolute time on a [`Clock`] by which a wait ends. Never a length: a wait that is
/// interrupted recomputes what is left of it, so no retry restarts it. Every deadline is on the
/// clock that made it; comparing two clocks' deadlines means nothing.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Deadline(Nanos);

impl Deadline {
    /// Passed on every clock: a wait on it looks once and does not block.
    pub const PASSED: Deadline = Deadline(Nanos::ZERO);
    /// Never reached: a wait on it ends only when what it waits for is ready.
    pub const NEVER: Deadline = Deadline(Nanos::MAX);

    /// The deadline at `at` on its clock.
    #[blaze::pure]
    pub const fn at(at: Nanos) -> Deadline {
        Deadline(at)
    }

    /// `wait` after `now`, or [`Deadline::NEVER`] where that is past the clock's end.
    #[blaze::pure]
    pub const fn after(now: Nanos, wait: Nanos) -> Deadline {
        Deadline(now.saturating_add(wait))
    }

    /// Its time on its clock.
    #[blaze::pure]
    pub const fn get(self) -> Nanos {
        self.0
    }

    /// What is left of it at `now`: zero once it has passed.
    #[blaze::pure]
    pub const fn left(self, now: Nanos) -> Nanos {
        self.0.saturating_sub(now)
    }

    /// Whether it has passed at `now`.
    #[blaze::pure]
    pub const fn passed(self, now: Nanos) -> bool {
        now.0 >= (self.0).0
    }

    /// `None` for [`Deadline::NEVER`]: the form a wait takes (`None` waits with no limit).
    #[blaze::pure]
    pub const fn finite(self) -> Option<Deadline> {
        if (self.0).0 == u64::MAX { None } else { Some(self) }
    }

    /// The earlier of the two: a wait bounded by both.
    #[blaze::pure]
    pub const fn min(self, other: Deadline) -> Deadline {
        if (other.0).0 < (self.0).0 { other } else { self }
    }
}

/// Where a `now` comes from: monotonic, never going back, on an origin of its own. Always a
/// value handed in (rule 75): a driver's clock over the system's, a test's counter, a
/// simulation's virtual time. Reading it is the only thing a core may do with time.
pub trait Clock {
    fn now(&self) -> Nanos;
}

impl<C: Clock + ?Sized> Clock for &C {
    #[inline]
    fn now(&self) -> Nanos {
        (**self).now()
    }
}
