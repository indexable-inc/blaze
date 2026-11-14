//! The retry loop of every blocking wait and its deadline's arithmetic, a sans-IO core: it
//! reads time only from the [`Clock`] it is given and makes no call of its own, only the
//! `once` its driver hands it. Its proof is the DST in `wait/tests.rs`, which drives it
//! against a simulated kernel on a virtual clock.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{fmt, io, time::Duration};

/// Monotonic time on some origin: a driver's `now()`, a [`super::Monotonic`], or a test's
/// virtual clock. Every [`Deadline`] is on the clock that made it.
pub trait Clock {
    fn now(&self) -> Duration;
}

/// An absolute time on a [`Clock`] by which a wait ends. Never a length: a wait that is
/// interrupted recomputes what is left of it, so no retry restarts it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Deadline(Duration);

impl Deadline {
    /// Passed on every clock: a wait on it looks once and does not block.
    pub const PASSED: Deadline = Deadline(Duration::ZERO);
    /// Never reached: a wait on it ends only when what it waits for is ready (the kernel is
    /// given no limit, see [`Deadline::finite`]).
    pub const NEVER: Deadline = Deadline(Duration::MAX);

    /// The deadline at `at` on its clock.
    pub fn at(at: Duration) -> Deadline {
        Deadline(at)
    }

    /// `wait` after `now`, or [`Deadline::NEVER`] where that is past the clock's end.
    pub fn after(now: Duration, wait: Duration) -> Deadline {
        Deadline(now.saturating_add(wait))
    }

    /// Its time on its clock.
    pub fn get(self) -> Duration {
        self.0
    }

    /// What is left of it at `now`: zero once it has passed.
    pub fn left(self, now: Duration) -> Duration {
        self.0.saturating_sub(now)
    }

    /// Whether it has passed at `now`.
    pub fn passed(self, now: Duration) -> bool {
        now >= self.0
    }

    /// `None` for [`Deadline::NEVER`]: the form a wait takes (`None` waits with no limit).
    pub fn finite(self) -> Option<Deadline> {
        (self != Deadline::NEVER).then_some(self)
    }
}

/// How a wait ended.
#[derive(PartialEq, Eq, Debug)]
pub enum Waited<T> {
    /// What it waited for was ready.
    Ready(T),
    /// Its deadline passed first.
    Deadline,
}

/// How one system call of a wait went.
#[derive(PartialEq, Eq, Debug)]
pub enum Once<T> {
    /// What the wait is for is ready.
    Ready(T),
    /// A signal ended the call (EINTR): retried with the time left.
    Interrupted,
    /// The call woke with nothing ready: retried with the time left.
    Spurious,
    /// The call's timeout ran out: the wait ends if its deadline has passed on its clock, and
    /// otherwise (the kernel's clock ran ahead of it, or the call ended early) waits again.
    TimedOut,
}

/// A wait that could not go on: the system call failed, or the clock stopped moving.
#[derive(Debug)]
pub struct WaitError {
    what: &'static str,
    why: Why,
}

#[derive(Debug)]
enum Why {
    Io(io::Error),
    /// This many calls in a row returned not ready with the clock still at `at`.
    Stuck {
        calls: u32,
        at: Duration,
    },
}

impl WaitError {
    /// The wait's system call failed with `error`.
    pub fn io(what: &'static str, error: io::Error) -> WaitError {
        WaitError { what, why: Why::Io(error) }
    }

    /// What the wait was for.
    pub fn what(&self) -> &'static str {
        self.what
    }

    /// The system call's error, where that is what ended the wait.
    pub fn io_error(&self) -> Option<&io::Error> {
        match &self.why {
            Why::Io(error) => Some(error),
            Why::Stuck { .. } => None,
        }
    }
}

impl fmt::Display for WaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.why {
            Why::Io(error) => write!(f, "waiting for {}: {error}", self.what),
            Why::Stuck { calls, at } => write!(
                f,
                "waiting for {}: {calls} calls in a row returned with nothing ready and the \
                 clock still at {} ns; a wait whose clock does not move is refused \
                 (rule bounded-progress)",
                self.what,
                at.as_nanos()
            ),
        }
    }
}

impl std::error::Error for WaitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.why {
            Why::Io(error) => Some(error),
            Why::Stuck { .. } => None,
        }
    }
}

/// The system call's own error where it failed, so its kind and OS code stay what they were;
/// a stuck clock as an error of its own kind with the wait's message.
impl From<WaitError> for io::Error {
    fn from(error: WaitError) -> io::Error {
        match error.why {
            Why::Io(error) => error,
            Why::Stuck { .. } => io::Error::other(error),
        }
    }
}

/// How many calls in a row may return not ready with the clock where it was before the wait
/// gives up: a real call takes longer than one tick of a monotonic clock, so only a clock that
/// stopped (or a `once` that never waits) reaches it.
pub(crate) const FUEL: u32 = 1 << 16;

/// A safeguard of the loop the DST removes, to see that it catches the removal.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// Every call is given the time left at the first: the bug of 0.20261105.0's
    /// `Ring::enter`, a relative timeout entered again whole after each interruption.
    SameTimeout,
    /// The calls that return with the clock where it was are not counted: a wait whose
    /// clock stopped goes on for ever.
    NeverStuck,
}

/// The state of one wait between its calls.
pub(crate) struct Retry {
    what: &'static str,
    deadline: Option<Deadline>,
    /// The clock when the last call returned (or the wait began).
    at: Duration,
    /// Calls in a row that returned with the clock not past `at`.
    still: u32,
    #[cfg(test)]
    plant: Plant,
    #[cfg(test)]
    first: Option<Option<Duration>>,
}

/// What the wait does after a call that was not ready.
pub(crate) enum Next {
    Again,
    Deadline,
}

impl Retry {
    /// A wait for `what` until `deadline`, beginning at `now`.
    pub(crate) fn new(what: &'static str, deadline: Option<Deadline>, now: Duration) -> Retry {
        Retry {
            what,
            deadline,
            at: now,
            still: 0,
            #[cfg(test)]
            plant: Plant::None,
            #[cfg(test)]
            first: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn planted(mut self, plant: Plant) -> Retry {
        self.plant = plant;
        self
    }

    /// The time the next call may wait: what is left of the deadline at the clock's last
    /// reading, `None` for no limit.
    pub(crate) fn left(&mut self) -> Option<Duration> {
        let left = self.deadline.map(|it| it.left(self.at));
        #[cfg(test)]
        let left = match self.plant {
            Plant::None | Plant::NeverStuck => left,
            Plant::SameTimeout => *self.first.get_or_insert(left),
        };
        left
    }

    /// After a call that returned with nothing ready at `now`: whether the deadline has
    /// passed, or the wait goes on; an error once the clock has not moved for [`FUEL`] calls.
    pub(crate) fn after(&mut self, now: Duration) -> Result<Next, WaitError> {
        if self.deadline.is_some_and(|it| it.passed(now)) {
            return Ok(Next::Deadline);
        }
        self.still = match now > self.at {
            true => 0,
            false => self.still + 1,
        };
        #[cfg(test)]
        if self.plant == Plant::NeverStuck {
            self.still = 0;
        }
        if self.still >= FUEL {
            return Err(WaitError {
                what: self.what,
                why: Why::Stuck { calls: self.still, at: now },
            });
        }
        self.at = now;
        Ok(Next::Again)
    }
}

/// Whether `left`, the time a wait's next call is given, is what is left of `deadline` at
/// `now`: no limit for no deadline, and never a time left at an earlier reading.
fn left_at(left: Option<Duration>, deadline: Option<Deadline>, now: Duration) -> bool {
    match (left, deadline) {
        (None, None) => true,
        (Some(left), Some(deadline)) => left == deadline.left(now),
        (None, Some(_)) | (Some(_), None) => false,
    }
}

/// What a wait `retry`, for `deadline`, whose last reading was `at` after `still` calls with
/// the clock not moved, does after a call that returned not ready at `now`: it ends exactly
/// when the deadline has passed at `now`; where it goes on, the next call is given what is
/// left at `now` ([`left_at`]) and fewer than [`FUEL`] calls stand counted; and it fails only
/// when the clock has not moved for the [`FUEL`]th call in a row.
fn steps(
    mut retry: Retry,
    deadline: Option<Deadline>,
    at: Duration,
    still: u32,
    now: Duration,
) -> bool {
    let passed = match deadline {
        Some(deadline) => deadline.passed(now),
        None => false,
    };
    match retry.after(now) {
        Ok(Next::Deadline) => passed,
        Ok(Next::Again) => !passed && retry.still < FUEL && left_at(retry.left(), deadline, now),
        Err(_) => !passed && now <= at && still + 1 >= FUEL,
    }
}

/// Whether [`FUEL`] calls in a row that return not ready at `now` end the wait `retry`: one
/// of them says the deadline passed or fails, and they do not all say "again".
fn ends(mut retry: Retry, now: Duration) -> bool {
    let mut calls = 0;
    while calls < FUEL {
        match retry.after(now) {
            Ok(Next::Again) => {}
            Ok(Next::Deadline) | Err(_) => return true,
        }
        calls += 1;
    }
    false
}

/// The invariant: a retry waits what is left. From every state of a wait (any deadline or
/// none, any last reading, any count below [`FUEL`]) and every reading of the clock after a
/// call that was not ready: the wait ends exactly when its deadline has passed at that
/// reading, and where it goes on the next call is given what is left of the deadline at that
/// reading, never the time left at an earlier one, so no interruption restarts a wait
/// ([`steps`]). Proved of one step for every state, where the simulation test in
/// `wait/tests.rs` follows drawn runs of the loop. Its control is
/// `the_oracle_catches_a_retry_given_its_first_timeout_again`.
#[cfg_attr(not(test), expect(dead_code, reason = "a theorem function: `blaze prove` reads it"))]
#[blaze::theorem]
pub(crate) fn a_retry_waits_what_is_left(
    deadline: Option<Deadline>,
    at: Duration,
    still: u32,
    now: Duration,
) -> bool {
    if still >= FUEL {
        return true;
    }
    let mut retry = Retry::new("a wait", deadline, at);
    retry.still = still;
    steps(retry, deadline, at, still, now)
}

/// The invariant: a stopped clock ends the wait (rule bounded-progress). From every state of
/// a wait, [`FUEL`] calls in a row that return not ready with the clock at or before the
/// wait's last reading do not all say "again": the deadline passes or the wait fails, naming
/// the clock ([`ends`]). Its control is `the_oracle_catches_a_wait_that_never_gives_up`.
#[cfg_attr(not(test), expect(dead_code, reason = "a theorem function: `blaze prove` reads it"))]
#[blaze::theorem]
pub(crate) fn a_stopped_clock_ends_the_wait(
    deadline: Option<Deadline>,
    at: Duration,
    still: u32,
    now: Duration,
) -> bool {
    if still >= FUEL || now > at {
        return true;
    }
    let mut retry = Retry::new("a wait", deadline, at);
    retry.still = still;
    ends(retry, now)
}

#[cfg(test)]
mod theorems {
    use super::*;

    fn ns(n: u64) -> Duration {
        Duration::from_nanos(n)
    }

    /// The theorem functions hold where they are run: on the edges of the deadline, the
    /// clock and the count. What Lean is to prove of their models for every state.
    #[test]
    fn the_theorems_hold_where_they_are_run() {
        let times = [0, 1, 2, 999, 1_000, 1_001, u64::MAX];
        let deadlines = [None, Some(0), Some(1), Some(1_000), Some(u64::MAX)];
        for deadline in deadlines {
            let deadline = deadline.map(|it| Deadline::at(ns(it)));
            for at in times {
                for now in times {
                    for still in [0, 1, FUEL - 2, FUEL - 1, FUEL, u32::MAX] {
                        assert!(
                            a_retry_waits_what_is_left(deadline, ns(at), still, ns(now)),
                            "{deadline:?} at {at} still {still} now {now}"
                        );
                    }
                    for still in [0, FUEL - 1, FUEL] {
                        assert!(
                            a_stopped_clock_ends_the_wait(deadline, ns(at), still, ns(now)),
                            "{deadline:?} at {at} still {still} now {now}"
                        );
                    }
                }
            }
        }
    }

    /// The planted bug of 0.20261105.0: every call is given the time left at the first.
    /// After a call that returned 400 ns into a wait of 1,000, the next is given 1,000 again,
    /// which [`steps`] refuses.
    #[test]
    fn the_oracle_catches_a_retry_given_its_first_timeout_again() {
        let deadline = Some(Deadline::at(ns(1_000)));
        let retry = |plant| {
            let mut retry = Retry::new("a wait", deadline, ns(0)).planted(plant);
            _ = retry.left();
            retry
        };
        assert!(steps(retry(Plant::None), deadline, ns(0), 0, ns(400)));
        assert!(!steps(retry(Plant::SameTimeout), deadline, ns(0), 0, ns(400)));
    }

    /// The planted bug: calls with the clock where it was are not counted. A wait with no
    /// deadline at a stopped clock then says "again" [`FUEL`] times, which [`ends`] refuses.
    #[test]
    fn the_oracle_catches_a_wait_that_never_gives_up() {
        let retry = |plant| Retry::new("a wait", None, ns(7)).planted(plant);
        assert!(ends(retry(Plant::None), ns(7)));
        assert!(!ends(retry(Plant::NeverStuck), ns(7)));
    }
}

/// The loop of [`super::until`]: `once` with the time left, until it is ready, the deadline
/// has passed on `clock`, or it fails.
pub(crate) fn drive<T>(
    mut retry: Retry,
    clock: &impl Clock,
    mut once: impl FnMut(Option<Duration>) -> io::Result<Once<T>>,
) -> Result<Waited<T>, WaitError> {
    loop {
        match once(retry.left()) {
            Ok(Once::Ready(it)) => return Ok(Waited::Ready(it)),
            Ok(Once::Interrupted | Once::Spurious | Once::TimedOut) => {}
            Err(error) => return Err(WaitError::io(retry.what, error)),
        }
        match retry.after(clock.now())? {
            Next::Again => {}
            Next::Deadline => return Ok(Waited::Deadline),
        }
    }
}
