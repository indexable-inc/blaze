//! The retry loop of every blocking wait and its deadline's arithmetic, a sans-IO core: it
//! reads time and makes its calls only through the [`Kernel`] it is given. Its proof is in
//! Lean (`crates/blaze-core/lean/BlazeCore/Wait/Retry.lean`): one step from every state
//! ([`a_retry_waits_what_is_left`], [`a_stopped_clock_ends_the_wait`]), and the whole loop
//! ([`drive`]) against every run of answers the blocking call's world allows
//! ([`a_wait_ends_by_its_deadline`]).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{fmt, io, time::Duration};

use crate::world::blocking::{Answer, Blocking, Ended};

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

/// What a wait runs against, as the one thing its loop is given: the kernel its calls go to
/// and the clock it reads after each. A clock and a closure are one ([`super::until`]); so is
/// a world that answers from a list (a theorem's, [`Answers`]).
pub(crate) trait Kernel<T> {
    /// One system call with the time it may wait (`None`: no limit), and how it went.
    fn once(&mut self, left: Option<Duration>) -> io::Result<Once<T>>;

    /// The clock, read after a call.
    fn now(&self) -> Duration;
}

impl<T, C: Clock, F: FnMut(Option<Duration>) -> io::Result<Once<T>>> Kernel<T> for (&C, F) {
    fn once(&mut self, left: Option<Duration>) -> io::Result<Once<T>> {
        (self.1)(left)
    }

    fn now(&self) -> Duration {
        self.0.now()
    }
}

/// The loop of [`super::until`]: one call of `kernel`'s with the time left, until it is ready,
/// the deadline has passed on `kernel`'s clock, or it fails.
pub(crate) fn drive<T, K: Kernel<T>>(
    mut retry: Retry,
    kernel: &mut K,
) -> Result<Waited<T>, WaitError> {
    loop {
        match kernel.once(retry.left()) {
            Ok(Once::Ready(it)) => return Ok(Waited::Ready(it)),
            Ok(Once::Interrupted | Once::Spurious | Once::TimedOut) => {}
            Err(error) => return Err(WaitError::io(retry.what, error)),
        }
        match retry.after(kernel.now())? {
            Next::Again => {}
            Next::Deadline => return Ok(Waited::Deadline),
        }
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

/// The invariant (proved: crates/blaze-core/lean/BlazeCore/Wait/Retry.lean): a retry waits
/// what is left. From every state of a wait (any deadline or
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

/// The invariant (proved: crates/blaze-core/lean/BlazeCore/Wait/Retry.lean): a stopped clock
/// ends the wait (rule bounded-progress). From every state of
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

/// The kernel of [`a_wait_ends_by_its_deadline`]: the blocking call's world giving `answers`
/// in order, one to a call. A call past the last answer, or an answer the world does not
/// allow there, sets `stopped` and ends the wait with a ready call: a run that stopped is not
/// one of the world's, and the theorem says nothing of it.
struct Answers {
    world: Blocking,
    answers: Vec<Answer>,
    next: usize,
    stopped: bool,
}

impl Kernel<()> for Answers {
    fn once(&mut self, left: Option<Duration>) -> io::Result<Once<()>> {
        if self.next >= self.answers.len() {
            self.stopped = true;
            return Ok(Once::Ready(()));
        }
        let answer = self.answers[self.next];
        self.next += 1;
        if !self.world.allows(left, &answer) {
            self.stopped = true;
            return Ok(Once::Ready(()));
        }
        self.world.answer(&answer);
        Ok(match answer.ended {
            Ended::Ready => Once::Ready(()),
            Ended::Interrupted => Once::Interrupted,
            Ended::Spurious => Once::Spurious,
            Ended::TimedOut => Once::TimedOut,
        })
    }

    fn now(&self) -> Duration {
        self.world.now
    }
}

/// Whether the wait `retry`, for `deadline`, begun at `start`, ends as it must when its calls
/// are answered by `answers` in order, in a world whose event comes at `event` and whose
/// calls are read at most `late` late. A run the world does not allow, or one `answers` ends
/// before the wait does, is none of the theorem's (true). Of every other: the wait does not
/// fail; it ends at most `late` past its deadline; it says the deadline passed only once it
/// has, and never where the event came `late` or more before it; and with no deadline it ends
/// only ready.
fn waits(
    retry: Retry,
    start: Duration,
    deadline: Option<Deadline>,
    event: Option<Duration>,
    late: Duration,
    answers: Vec<Answer>,
) -> bool {
    let mut kernel = Answers { world: Blocking { now: start, event, late }, answers, next: 0, stopped: false };
    let waited = drive(retry, &mut kernel);
    if kernel.stopped {
        return true;
    }
    let end = kernel.world.now;
    let waited = match waited {
        Ok(waited) => waited,
        Err(_) => return false,
    };
    let Some(deadline) = deadline else {
        return match waited {
            Waited::Ready(()) => true,
            Waited::Deadline => false,
        };
    };
    if end > deadline.get().saturating_add(late) {
        return false;
    }
    match waited {
        Waited::Ready(()) => true,
        Waited::Deadline => {
            // The event came first where it came a call's lateness before the deadline: a
            // call that ends later than that may be read past the deadline.
            let first = match event {
                Some(event) => match event.checked_add(late) {
                    Some(by) => by <= deadline.get(),
                    None => false,
                },
                None => false,
            };
            deadline.passed(end) && !first
        }
    }
}

/// The invariant (proved: crates/blaze-core/lean/BlazeCore/Wait/Retry.lean): whatever
/// interrupts it, every wait ends by its deadline, never says it passed before it did, and is
/// ready whenever its event came first. For every start, deadline or none, event or none,
/// lateness and list of answers: where each answer is one the blocking call's world allows
/// the call it answers ([`Blocking::allows`]: interrupted, woken for nothing or timed out
/// early, at any moment before the event, and read up to `late` after), the real loop
/// ([`drive`]) does not fail and does not panic, ends at most `late` past its deadline, says
/// [`Waited::Deadline`] only at a reading that has passed it and never where the event came
/// `late` or more before it, and with no deadline ends only [`Waited::Ready`] ([`waits`]).
/// Proved of every run, where the simulation test this replaced followed drawn ones. Its
/// control is `the_oracle_catches_a_wait_given_its_first_timeout_again`.
#[cfg_attr(not(test), expect(dead_code, reason = "a theorem function: `blaze prove` reads it"))]
#[blaze::theorem]
pub(crate) fn a_wait_ends_by_its_deadline(
    start: Duration,
    wait: Option<Duration>,
    event: Option<Duration>,
    late: Duration,
    answers: Vec<Answer>,
) -> bool {
    let deadline = match wait {
        Some(wait) => Some(Deadline::after(start, wait)),
        None => None,
    };
    waits(Retry::new("a wait", deadline, start), start, deadline, event, late, answers)
}

#[cfg(test)]
mod theorems {
    use std::alloc::Global;

    use blaze_sim::{Broke, Config, Tape, check};

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

    /// A wait and a run of its kernel's answers, drawn: the answers follow the world from the
    /// time left a wait without a plant gives each call, so most runs are the world's to the
    /// end; one in a few has an answer drawn with no regard to it.
    fn drawn(tape: &Tape) -> (Duration, Option<Duration>, Option<Duration>, Duration, Vec<Answer>) {
        let start = ns(tape.int_in(0..=1_000u64, "start"));
        let wait = tape.option("a deadline", |tape| ns(tape.int_in(0..=1_000u64, "wait")));
        let event = tape.option("an event", |tape| start + ns(tape.int_in(0..=1_500u64, "event")));
        let late = ns(tape.int_in(1..=20u64, "late"));
        let deadline = wait.map(|wait| start + wait);
        let mut now = start;
        let mut answers = Vec::new_in(Global);
        for _ in 0..tape.int_in(0..=24u64, "answers") {
            if tape.one_in(12, "an answer from nowhere") {
                answers.push(Answer {
                    ended: *tape.one_of(&[Ended::Ready, Ended::Interrupted, Ended::Spurious, Ended::TimedOut], "ended"),
                    at: ns(tape.int_in(0..=3_000u64, "at")),
                    after: ns(tape.int_in(0..=30u64, "after")),
                });
                continue;
            }
            // The call's limit on the clock, as a wait gives it: its deadline, or now once
            // that has passed; with none, the call ends within 200 ns here.
            let most = match deadline {
                Some(deadline) => deadline.max(now),
                None => now + ns(200),
            };
            let at = ns(tape.int_in(now.as_nanos() as u64..=most.as_nanos() as u64, "at"));
            let after = ns(tape.int_in(1..=late.as_nanos() as u64, "after"));
            let ended = match event.is_some_and(|event| event <= at) {
                true => Ended::Ready,
                false => match tape.int_in(0..=2u64, "ended") {
                    0 => Ended::Interrupted,
                    1 if deadline.is_some() => Ended::TimedOut,
                    _ => Ended::Spurious,
                },
            };
            answers.push(Answer { ended, at, after });
            now = at + after;
        }
        (start, wait, event, late, answers)
    }

    /// The theorem's function holds where it is run, on drawn waits and answers, and the
    /// draws reach each way a wait ends in a run that is the world's to its end.
    #[test]
    fn a_wait_ends_by_its_deadline_where_it_is_run() {
        let config = Config {
            must_reach: &[
                "a wait that ended ready",
                "a wait that ended at its deadline",
                "a wait interrupted and gone on",
                "a run the world does not allow",
            ],
            ..Config::cases(2_000)
        };
        check(config, |tape| {
            let (start, wait, event, late, answers) = drawn(tape);
            tape.note(format_args!("start {start:?} wait {wait:?} event {event:?} late {late:?} answers {answers:?}"));
            // The same run, to see how it ended.
            let deadline = wait.map(|wait| Deadline::after(start, wait));
            let mut kernel =
                Answers { world: Blocking { now: start, event, late }, answers: answers.clone(), next: 0, stopped: false };
            let waited = drive(Retry::new("a wait", deadline, start), &mut kernel);
            match (kernel.stopped, &waited) {
                (true, _) if kernel.next < answers.len() => tape.reached("a run the world does not allow"),
                (true, _) | (false, Err(_)) => {}
                (false, Ok(Waited::Ready(()))) => tape.reached("a wait that ended ready"),
                (false, Ok(Waited::Deadline)) => tape.reached("a wait that ended at its deadline"),
            }
            if !kernel.stopped && kernel.next > 1 {
                tape.reached("a wait interrupted and gone on");
            }
            match a_wait_ends_by_its_deadline(start, wait, event, late, answers) {
                true => Ok(()),
                false => Err(Broke::Oracle(format!("the wait ended {waited:?} at {:?}", kernel.world.now))),
            }
        });
    }

    /// The planted bug of 0.20261105.0, against the whole loop: every call is given the time
    /// left at the first. A wait of 1,000 ns interrupted at 400 gives its second call 1,000
    /// again, so the world lets that call time out at 1,400, and the wait ends 400 ns past
    /// its deadline, which [`waits`] refuses. Without the plant the second call has 599 ns,
    /// the world does not allow that answer, and the run is none of the theorem's.
    #[test]
    fn the_oracle_catches_a_wait_given_its_first_timeout_again() {
        let (start, late) = (ns(0), ns(1));
        let deadline = Some(Deadline::after(start, ns(1_000)));
        let answers = || {
            let mut answers = Vec::new_in(Global);
            answers.push(Answer { ended: Ended::Interrupted, at: ns(400), after: ns(1) });
            answers.push(Answer { ended: Ended::TimedOut, at: ns(1_400), after: ns(1) });
            answers
        };
        let retry = |plant| Retry::new("a wait", deadline, start).planted(plant);
        assert!(waits(retry(Plant::None), start, deadline, None, late, answers()));
        assert!(!waits(retry(Plant::SameTimeout), start, deadline, None, late, answers()));
    }
}
