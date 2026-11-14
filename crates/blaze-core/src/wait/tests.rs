//! The deadline wait under a simulated kernel (`testing` skill, dst-not-review): one test draws,
//! from its case's tape, a sequence of waits (a deadline or none, and when the awaited event arrives: before,
//! at or after the deadline, or never), and a swarm of kernel faults (an EINTR at random, a
//! train of them at a fixed period as the profile's sampler sent, spurious wakeups, a timeout
//! returned early, and each call's latency), runs the real retry loop ([`retry::drive`])
//! against a kernel on a virtual clock, which draws which call a fault hits, where, and each
//! call's latency from the same tape, and checks at every wait:
//!
//! - it ends by its deadline: the clock at its return is at most the deadline plus one call's
//!   latency;
//! - it never says [`Waited::Deadline`] before the deadline;
//! - it says [`Waited::Ready`] whenever the event arrived first: by the deadline less one
//!   call's latency (a call interrupted just before the deadline may return past it with the
//!   event arriving meanwhile, which is a deadline, not a lost event), and always for a wait
//!   with no deadline.
//!
//! A wait with no deadline is drawn only with an event that arrives: one that never came would
//! wait forever, as it should. The fuel that ends a wait whose clock does not move has its own
//! test, [`a_clock_that_does_not_move_ends_the_wait`].
//!
//! The planted bug ([`Plant::SameTimeout`]) gives every call the time left at the first, as
//! `Ring::enter` did on 0.20261105.0; [`the_oracle_catches_a_retry_with_the_first_timeout`]
//! must find a case it breaks. Then [`Sim`] waits through the same loop under its own
//! interruptions ([`a_sim_under_interruption_delivers_everything_by_each_deadline`]).
//!
//! The park ([`parking`], a pool worker's idle wait, a futex on its thread's ring on Linux):
//! a simulated futex on the virtual clock runs a drawn unpark between any two of the park's
//! atomics ([`Futex::switch`]) or while it waits, with interruptions, spurious returns and
//! latency; the oracle is that no unpark is lost (a park an unpark came to ends `Ready`,
//! promptly; one with a token waiting takes it at once; `Deadline` only with no unpark) and
//! every park ends by its deadline. The plant ([`ParkPlant::CheckThenStore`]) looks for a token
//! and then stores `PARKED` over it, the lost wakeup [`the_oracle_catches_a_lost_wakeup`] must
//! find.
//!
//! The reap ([`reaping`], `IORING_OP_WAITID` then `wait4(WNOHANG)` on Linux): simulated children
//! exit at drawn times, before or after their reap begins and its deadline, under the same
//! faults; the oracle is that every child reaped is reported with its status, never before it
//! exited and promptly after, a reap that ends at its deadline leaves its child unreaped, and no
//! child is left a zombie. The plant ([`ReapPlant::ReapInWait`]) waits without `WNOWAIT`, so the
//! wait reaps the child and its exit is never reported, which
//! [`the_oracle_catches_a_reaped_child_never_reported`] must find.
//!
//! The direct driver (`wait/direct.rs`, the waits of a process where io_uring is refused) runs
//! the park's and the reap's plans too ([`Through::Direct`], on Linux): the same simulated
//! kernels answer its calls ([`FutexCalls`], [`ChildCalls`]), their `CLOCK_MONOTONIC` on another
//! origin than the wait's clock ([`KERNEL_AHEAD`]) so each call's absolute time is made from
//! the kernel's, and each fault comes back as the error the kernel returns (`EINTR`,
//! `ETIMEDOUT`). The same oracles hold, and the same plants are caught through it.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    convert::Infallible,
    fmt::Write as _,
    io,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use blaze_sim::{Broke, Config, Tape};

#[cfg(target_os = "linux")]
use super::direct::{ChildCalls, Direct, FutexCalls};
use super::{
    Clock, Deadline, Once, Plant, WaitError, Waited,
    parking::{self, Futex, ParkPlant},
    reaping::{self, Children, ReapPlant},
    retry::{self, FUEL, Retry},
    until,
};
use crate::{
    Completion, Done, Driver, Effect, FileName, Tag,
    sim::{Faults, Sim},
};

/// Cases a control searches for one its plant breaks.
const CONTROL_SEEDS: u32 = 256;

/// Most waits a plan draws.
const MOST_WAITS: u64 = 24;

/// How a park or a reap reaches its simulated kernel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Through {
    /// The core on the kernel's own wait, each call retried by [`until`]: the ring's shape.
    Core,
    /// The core on the direct driver, which makes the kernel's calls itself.
    #[cfg(target_os = "linux")]
    Direct,
}

/// How far the simulated kernel's `CLOCK_MONOTONIC` is ahead of the wait's clock: the direct
/// driver gives a futex wait an absolute time on the kernel's, never the wait's.
#[cfg(target_os = "linux")]
const KERNEL_AHEAD: Duration = Duration::from_secs(1_000);

/// A clock the test moves.
struct Virtual<'a>(&'a Cell<Duration>);

impl Clock for Virtual<'_> {
    fn now(&self) -> Duration {
        self.0.get()
    }
}

// --- The plan ------------------------------------------------------------------------------

/// The kernel's faults a case turns on: a swarm, so a rare class is not drowned by a common one.
#[derive(Debug, Clone, Copy)]
struct KernelFaults {
    /// A call is interrupted at a random point of its wait once in this many (0: never).
    eintr_one_in: u64,
    /// An EINTR at every multiple of `period` past `phase`, on the absolute clock: the
    /// sampler's signal train.
    train: Option<(Duration, Duration)>,
    /// A call wakes with nothing once in this many (0: never).
    spurious_one_in: u64,
    /// A timeout ends early once in this many (0: never).
    early_one_in: u64,
    /// Each call returns up to this long after it ended, and at least a nanosecond.
    max_latency: Duration,
}

impl KernelFaults {
    /// The longest one call's return can take.
    fn latency_bound(&self) -> Duration {
        self.max_latency + Duration::from_nanos(1)
    }
}

/// One wait: its deadline and when its event arrives, both after the wait begins.
#[derive(Debug, Clone, Copy)]
struct Wait {
    deadline: Option<Duration>,
    event: Option<Duration>,
}

/// The kernel's choices (which call a fault hits, where, and each call's latency) are drawn
/// from the case's tape as the run goes.
#[derive(Debug)]
struct Plan {
    faults: KernelFaults,
    waits: Vec<Wait>,
}

fn us(n: u64) -> Duration {
    Duration::from_micros(n)
}

/// A fault's rate: off, or once in 1 to 8 calls.
fn sometimes(tape: &Tape, label: &'static str) -> u64 {
    match tape.coin(label) {
        false => 0,
        true => tape.int_in(1..=8u64, label),
    }
}

fn plan(tape: &Tape) -> Plan {
    let eintr_one_in = sometimes(tape, "eintr one in");
    let spurious_one_in = sometimes(tape, "spurious one in");
    let early_one_in = sometimes(tape, "early one in");
    // The sampler's shape: a period of 1 to 30 ms against waits of up to 200 ms.
    let train = match tape.coin("a train of eintr") {
        false => None,
        true => {
            let period = us(tape.int_in(1_000..=30_000u64, "train period us"));
            Some((period, us(tape.int_in(0..=period.as_micros() as u64, "train phase us"))))
        }
    };
    let max_latency = match tape.coin("latency") {
        false => Duration::ZERO,
        true => us(tape.int_in(0..=2_000u64, "max latency us")),
    };
    let faults = KernelFaults { eintr_one_in, train, spurious_one_in, early_one_in, max_latency };
    let n = tape.int_in(1..=MOST_WAITS, "waits");
    let waits = (0..n)
        .map(|_| {
            let deadline = match tape.int_in(0..=5u64, "deadline kind") {
                0 => None,
                _ => Some(us(tape.int_in(0..=200_000u64, "deadline us"))),
            };
            let event = match deadline {
                None => Some(us(tape.int_in(0..=500_000u64, "event us"))),
                Some(deadline) => match tape.int_in(0..=3u64, "event kind") {
                    0 => Some(us(tape.int_in(0..=deadline.as_micros() as u64, "event us"))),
                    1 => Some(deadline),
                    2 => Some(deadline + us(tape.int_in(1..=100_000u64, "event after us"))),
                    _ => None,
                },
            };
            Wait { deadline, event }
        })
        .collect(); // alloc: a test's plan
    Plan { faults, waits }
}

// --- The simulated kernel ------------------------------------------------------------------

/// A point drawn from `tape` in `[from, to)`, or `None` (and no draw) where that is empty.
fn within(tape: &Tape, from: Duration, to: Duration, label: &'static str) -> Option<Duration> {
    let span = to.saturating_sub(from).as_nanos() as u64;
    (span > 0).then(|| from + Duration::from_nanos(tape.below(span, label)))
}

/// One blocking call at the clock's now, given `left` (`None`: no limit), for an event at
/// `event`: it blocks until the event or its timeout, unless a fault drawn from `tape` ends it
/// first, and returns a drawn latency after.
fn call(
    now: &Cell<Duration>,
    tape: &Tape,
    faults: &KernelFaults,
    event: Option<Duration>,
    left: Option<Duration>,
) -> io::Result<Once<()>> {
    let t0 = now.get();
    let latency = Duration::from_nanos(1 + tape.below(faults.max_latency.as_nanos() as u64 + 1, "latency ns"));
    if event.is_some_and(|it| it <= t0) {
        now.set(t0 + latency);
        return Ok(Once::Ready(()));
    }
    let timeout = left.map(|it| t0.saturating_add(it));
    let wake = match (event, timeout) {
        (Some(event), Some(timeout)) => event.min(timeout),
        (event, timeout) => event.or(timeout).ok_or_else(|| {
            io::Error::other("a wait with no deadline and no event: the plan drew one")
        })?,
    };
    let mut end = match event.is_some_and(|it| it <= wake) {
        true => (wake, Once::Ready(())),
        false => (wake, Once::TimedOut),
    };
    if let Some((period, phase)) = faults.train {
        let (period, phase) = (period.as_nanos(), phase.as_nanos());
        let t = t0.as_nanos();
        let tick = match t < phase {
            true => phase,
            false => phase + period * ((t - phase) / period + 1),
        };
        let tick = Duration::from_nanos(tick as u64);
        if tick < end.0 {
            end = (tick, Once::Interrupted);
        }
    }
    if tape.one_in(faults.eintr_one_in, "eintr")
        && let Some(at) = within(tape, t0, end.0, "eintr at")
    {
        end = (at, Once::Interrupted);
    }
    if tape.one_in(faults.spurious_one_in, "spurious")
        && let Some(at) = within(tape, t0, end.0, "spurious at")
    {
        end = (at, Once::Spurious);
    }
    if tape.one_in(faults.early_one_in, "early timeout")
        && end.1 == Once::TimedOut
        && let Some(at) = within(tape, t0, end.0, "early timeout at")
    {
        end = (at, Once::TimedOut);
    }
    now.set(end.0 + latency);
    Ok(end.1)
}

// --- A run and its oracle ------------------------------------------------------------------

/// `plan`'s waits one after another on one clock, with `plant`, the kernel drawing from
/// `tape`: the first broken rule.
fn run(tape: &Tape, plan: &Plan, plant: Plant) -> Result<(), String> {
    let now = Cell::new(Duration::ZERO);
    let clock = Virtual(&now);
    let faults = plan.faults;
    let bound = faults.latency_bound();
    for (n, wait) in plan.waits.iter().enumerate() {
        let start = now.get();
        let deadline = wait.deadline.map(|it| Deadline::after(start, it));
        let event = wait.event.map(|it| start + it);
        let retry = Retry::new("a simulated event", deadline, start).planted(plant);
        let waited = retry::drive(retry, &clock, |left| call(&now, tape, &faults, event, left));
        let end = now.get();
        let waited = waited.map_err(|error| format!("wait {n}: {error}"))?;
        let said = |what: &str| {
            format!(
                "wait {n} (began {start:?}, deadline {deadline:?}, event {event:?}) said \
                 {waited:?} at {end:?}: {what}"
            )
        };
        let Some(deadline) = deadline else {
            if waited != Waited::Ready(()) {
                return Err(said("a wait with no deadline ends only when its event comes"));
            }
            continue;
        };
        if end > deadline.get() + bound {
            return Err(said(&format!("past its deadline by more than one call's {bound:?}")));
        }
        if waited == Waited::Deadline && end < deadline.get() {
            return Err(said("its deadline had not passed"));
        }
        if waited == Waited::Deadline && event.is_some_and(|it| it + bound <= deadline.get()) {
            return Err(said("the event came before the deadline"));
        }
    }
    Ok(())
}

// --- The tests -----------------------------------------------------------------------------

/// The invariant: whatever interrupts it, every wait ends by its deadline, never says it
/// passed before it did, and is ready whenever its event came first.
#[test]
fn every_wait_ends_by_its_deadline_under_interruption() {
    blaze_sim::check(Config::cases(400), |tape| drawn_run(tape, Plant::None));
}

/// The case of `tape`: its plan drawn, noted for the report, and run with `plant`.
fn drawn_run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let plan = plan(tape);
    tape.note(format_args!("plan {plan:?}"));
    run(tape, &plan, plant).map_err(Broke::Oracle)
}

/// The plant the oracle must catch in one of [`CONTROL_SEEDS`] cases (test-the-cause on the
/// test itself): a retry with the first relative timeout overruns a deadline under a train of
/// interruptions.
#[test]
fn the_oracle_catches_a_retry_with_the_first_timeout() {
    blaze_sim::control(Config::cases(CONTROL_SEEDS), Plant::None, Plant::SameTimeout, "past its deadline", drawn_run);
}

/// A wait whose clock never moves ends with an error naming it once its fuel is spent, not a
/// spin (bounded-progress).
#[test]
fn a_clock_that_does_not_move_ends_the_wait() {
    let now = Cell::new(Duration::from_secs(1));
    let mut calls = 0u32;
    let deadline = Some(Deadline::after(now.get(), Duration::from_secs(1)));
    let waited = until("a frozen clock", &Virtual(&now), deadline, |_| -> io::Result<Once<()>> {
        calls += 1;
        Ok(Once::Interrupted)
    });
    let error: WaitError = waited.expect_err("a frozen clock waited forever");
    assert_eq!(calls, FUEL);
    assert_eq!(error.what(), "a frozen clock");
    let said = error.to_string();
    assert!(said.contains("a frozen clock") && said.contains("bounded-progress"), "{said}");
}

/// A wait on a deadline that has passed makes exactly one call, given no time to wait, and
/// ends at the deadline whatever that call returns: `readable` with [`Deadline::PASSED`] is
/// one `poll` with a timeout of 0.
#[test]
fn a_passed_deadline_makes_exactly_one_call() {
    let now = Cell::new(Duration::from_secs(3));
    for case in 0..3 {
        let mut given = Vec::new(); // alloc: a test's record
        let waited = until("a look", &Virtual(&now), Some(Deadline::PASSED), |left| {
            given.push(left);
            Ok::<Once<Infallible>, io::Error>(match case {
                0 => Once::Interrupted,
                1 => Once::Spurious,
                _ => Once::TimedOut,
            })
        });
        assert!(matches!(waited, Ok(Waited::Deadline)));
        assert_eq!(given, [Some(Duration::ZERO)]);
    }
}

/// A [`Sim`] whose waits are interrupted delivers every completion, and no poll moves its
/// clock past the deadline it was given.
#[test]
fn a_sim_under_interruption_delivers_everything_by_each_deadline() {
    blaze_sim::check(Config::cases(100), a_sim_under_interruption);
}

fn a_sim_under_interruption(tape: &Tape) {
    let seed = tape.next_u64("sim seed");
    let faults = Faults {
        interrupt_one_in: tape.int_in(1..=4u32, "interrupt one in"),
        max_latency: us(tape.int_in(0..=50_000u64, "max latency us")),
        ..Faults::default()
    };
    let mut sim = Sim::new(seed, faults);
    let timers = tape.int_in(1..=16u64, "timers");
    let writes = tape.int_in(0..=8u64, "writes");
    let mut effects = Vec::new(); // alloc: a test's effects
    for n in 0..timers {
        effects.push(Effect::Timer { after: us(tape.int_in(0..=300_000u64, "timer after us")), tag: Tag(n) });
    }
    for n in 0..writes {
        let file = FileName::new(&format!("f{n}")).unwrap();
        effects.push(Effect::Write { file, bytes: b"x".to_vec(), tag: Tag(100 + n) });
    }
    let wanted = (timers + writes) as usize;
    sim.submit(&mut effects);
    let mut seen: Vec<Completion> = Vec::new(); // alloc: a test's record
    let mut log = String::new();
    for _ in 0..10_000 {
        if seen.len() == wanted {
            break;
        }
        let deadline = match tape.coin("poll with no deadline") {
            true => Deadline::NEVER,
            false => Deadline::after(sim.now(), us(tape.int_in(0..=100_000u64, "poll deadline us"))),
        };
        let before = seen.len();
        let got = sim.poll(&mut seen, deadline);
        _ = writeln!(log, "poll until {deadline:?}: {got} at {:?}", sim.now());
        assert_eq!(got, seen.len() - before);
        assert!(sim.now() <= deadline.get(), "a poll passed its deadline:\n{log}");
        if got == 0 && deadline != Deadline::NEVER {
            assert_eq!(sim.now(), deadline.get(), "a poll that came back empty early:\n{log}");
        }
    }
    assert_eq!(seen.len(), wanted, "not every completion came:\n{log}");
    for it in &seen {
        assert!(matches!(it.done, Done::Fired | Done::Written), "{it:?}");
    }
}

// --- The park: a futex wait and the unparks that come to it -----------------------------

/// The faults of the park's and the reap's simulated kernel.
#[derive(Debug, Clone, Copy)]
struct FutexFaults {
    /// A call is interrupted within 100 µs of its start once in this many (0: never).
    eintr_one_in: u64,
    /// A call returns with nothing within 100 µs of its start once in this many (0: never).
    spurious_one_in: u64,
    /// Each call returns up to this long after it ended, and at least a nanosecond.
    max_latency: Duration,
    /// Each step between two atomics takes up to this long, and at least a nanosecond.
    max_step: Duration,
}

impl FutexFaults {
    /// How late after its cause a park or a reap may end: a few calls and steps.
    fn bound(&self) -> Duration {
        (self.max_latency + self.max_step + Duration::from_nanos(1)) * 16
    }
}

fn futex_faults(tape: &Tape) -> FutexFaults {
    FutexFaults {
        eintr_one_in: sometimes(tape, "eintr one in"),
        spurious_one_in: sometimes(tape, "spurious one in"),
        max_latency: Duration::from_nanos(tape.int_in(0..=20_000u64, "max latency ns")),
        max_step: Duration::from_nanos(tape.int_in(1..=50_000u64, "max step ns")),
    }
}

/// One park: its deadline, when an unpark comes after it begins, and whether a stray unpark
/// comes after it ends (a token the next park must take at once).
#[derive(Debug, Clone, Copy)]
struct Round {
    deadline: Option<Duration>,
    unpark: Option<Duration>,
    stray: bool,
}

/// The futex's choices (which call a fault hits, where, each latency and step) are drawn from
/// the case's tape as the run goes.
#[derive(Debug)]
struct ParkPlan {
    faults: FutexFaults,
    rounds: Vec<Round>,
}

fn park_plan(tape: &Tape) -> ParkPlan {
    let faults = futex_faults(tape);
    let n = tape.int_in(1..=MOST_WAITS, "parks");
    let rounds = (0..n)
        .map(|_| {
            let deadline = match tape.int_in(0..=4u64, "deadline kind") {
                0 => None,
                _ => Some(us(tape.int_in(0..=50_000u64, "deadline us"))),
            };
            // Nanoseconds, dense where the park's steps are; a park with no deadline always
            // gets one, or it would wait forever, as it should.
            let unpark = match (deadline, tape.int_in(0..=2u64, "unpark kind")) {
                (Some(_), 0) => None,
                _ => Some(Duration::from_nanos(tape.int_in(0..=300_000u64, "unpark ns"))),
            };
            // A stray one in six, never at the calm 0.
            Round { deadline, unpark, stray: tape.int_in(0..=5u64, "stray") == 5 }
        })
        .collect(); // alloc: a test's plan
    ParkPlan { faults, rounds }
}

/// A futex on the virtual clock, with the unparks of a plan to run at their times: at a
/// switch point of the park once their time has come, or while the park waits in it.
struct FutexSim<'a> {
    now: &'a Cell<Duration>,
    tape: &'a Tape,
    faults: FutexFaults,
    word: &'a AtomicU32,
    unparks: RefCell<VecDeque<Duration>>,
    /// When each unpark ran, since the last clear.
    fired: RefCell<Vec<Duration>>,
    in_unpark: Cell<bool>,
    waiting: Cell<bool>,
    woken: Cell<bool>,
}

impl FutexSim<'_> {
    /// The real unpark, at `at`.
    fn fire(&self, at: Duration) {
        self.in_unpark.set(true);
        parking::unpark(self.word, self);
        self.in_unpark.set(false);
        self.fired.borrow_mut().push(at);
    }

    fn next_unpark(&self) -> Option<Duration> {
        self.unparks.borrow().front().copied()
    }

    fn fault_at(&self, one_in_n: u64, t0: Duration, label: &'static str) -> Option<Duration> {
        self.tape.one_in(one_in_n, label).then(|| t0 + Duration::from_nanos(self.tape.below(100_000, label)))
    }

    /// One wait call on `word` for `expected`, given `left`.
    fn call(
        &self,
        word: &AtomicU32,
        expected: u32,
        left: Option<Duration>,
    ) -> io::Result<Once<()>> {
        let t0 = self.now.get();
        let latency = Duration::from_nanos(
            1 + self.tape.below(self.faults.max_latency.as_nanos() as u64 + 1, "latency ns"),
        );
        let eintr = self.fault_at(self.faults.eintr_one_in, t0, "eintr");
        let spurious = self.fault_at(self.faults.spurious_one_in, t0, "spurious");
        // The kernel's compare: a word that changed is `EAGAIN`, at once.
        if word.load(Ordering::SeqCst) != expected {
            self.now.set(t0 + latency);
            return Ok(Once::Ready(()));
        }
        let mut end: Option<(Duration, Once<()>)> =
            left.map(|it| (t0.saturating_add(it), Once::TimedOut));
        for (at, why) in [(eintr, Once::Interrupted), (spurious, Once::Spurious)] {
            if let Some(at) = at
                && end.as_ref().is_none_or(|(e, _)| at < *e)
            {
                end = Some((at, why));
            }
        }
        self.waiting.set(true);
        self.woken.set(false);
        while let Some(at) = self.next_unpark() {
            if end.as_ref().is_some_and(|(e, _)| at > *e) {
                break;
            }
            self.unparks.borrow_mut().pop_front();
            let at = at.max(t0);
            self.now.set(at);
            self.fire(at);
            if self.woken.get() {
                end = Some((at, Once::Ready(())));
                break;
            }
        }
        self.waiting.set(false);
        let Some((at, outcome)) = end else {
            return Err(io::Error::other(
                "a futex wait with no deadline that nothing will wake: a lost wakeup",
            ));
        };
        self.now.set(at.max(t0) + latency);
        Ok(outcome)
    }
}

impl Futex for FutexSim<'_> {
    fn wait<C: Clock>(
        &self,
        what: &'static str,
        word: &AtomicU32,
        expected: u32,
        deadline: Option<Deadline>,
        clock: &C,
    ) -> Result<Waited<()>, WaitError> {
        until(what, clock, deadline, |left| self.call(word, expected, left))
    }

    fn wake(&self, _: &AtomicU32) {
        if self.waiting.get() {
            self.woken.set(true);
        }
    }

    fn switch(&self) {
        if self.in_unpark.get() {
            return;
        }
        let step = Duration::from_nanos(1 + self.tape.below(self.faults.max_step.as_nanos() as u64, "step ns"));
        self.now.set(self.now.get() + step);
        while let Some(at) = self.next_unpark() {
            if at > self.now.get() {
                break;
            }
            self.unparks.borrow_mut().pop_front();
            self.fire(at);
        }
    }
}

#[cfg(target_os = "linux")]
impl FutexCalls for FutexSim<'_> {
    fn monotonic(&self) -> io::Result<Duration> {
        Ok(self.now.get() + KERNEL_AHEAD)
    }

    fn futex_wait(&self, word: &AtomicU32, expected: u32, at: Option<Duration>) -> io::Result<()> {
        let left = at.map(|at| at.saturating_sub(self.now.get() + KERNEL_AHEAD));
        match self.call(word, expected, left)? {
            // Woken, a word that changed (the kernel's `EAGAIN`, which the driver reads the
            // same), or a return with nothing.
            Once::Ready(()) | Once::Spurious => Ok(()),
            Once::Interrupted => Err(io::Error::from_raw_os_error(libc::EINTR)),
            Once::TimedOut => Err(io::Error::from_raw_os_error(libc::ETIMEDOUT)),
        }
    }

    fn futex_wake(&self, word: &AtomicU32) {
        Futex::wake(self, word);
    }

    fn between(&self) {
        Futex::switch(self);
    }
}

/// A drawn [`ParkPlan`] run with `plant`, the park reaching the simulated futex as `through`
/// says: the first broken rule, the oracle's.
fn drawn_parks(tape: &Tape, plant: ParkPlant, through: Through) -> Result<(), Broke> {
    let plan = park_plan(tape);
    tape.note(format_args!("plan {plan:?}"));
    run_parks_through(tape, &plan, plant, through).map_err(Broke::Oracle)
}

/// `plan`'s parks one after another on one word and clock, with `plant`, the futex drawing
/// from `tape`, the park reaching it as `through` says: the first broken rule.
fn run_parks_through(tape: &Tape, plan: &ParkPlan, plant: ParkPlant, through: Through) -> Result<(), String> {
    let now = Cell::new(Duration::ZERO);
    let clock = Virtual(&now);
    let word = AtomicU32::new(parking::EMPTY);
    let sim = FutexSim {
        now: &now,
        tape,
        faults: plan.faults,
        word: &word,
        unparks: RefCell::new(VecDeque::new()),
        fired: RefCell::new(Vec::new()), // alloc: a test's record
        in_unpark: Cell::new(false),
        waiting: Cell::new(false),
        woken: Cell::new(false),
    };
    let bound = plan.faults.bound();
    let mut token = false;
    for (n, round) in plan.rounds.iter().enumerate() {
        let start = now.get();
        let unpark = round.unpark.map(|it| start + it);
        sim.unparks.borrow_mut().extend(unpark);
        sim.fired.borrow_mut().clear();
        let deadline = round.deadline.map(|it| Deadline::after(start, it));
        let what = "a simulated park";
        let parked = match through {
            Through::Core => parking::park(&word, &sim, what, deadline, &clock, plant),
            #[cfg(target_os = "linux")]
            Through::Direct => parking::park(&word, &Direct(&sim), what, deadline, &clock, plant),
        };
        let end = now.get();
        let fired = sim.fired.borrow().first().copied();
        // The token as the park began: `token` itself is set again below, for the next park,
        // while `said` is still used.
        let began_with = token;
        let said = |what: &str| {
            format!(
                "park {n} (began {start:?}, deadline {deadline:?}, unpark {unpark:?}, a token \
                 waiting {began_with}) said {parked:?} at {end:?}: {what}"
            )
        };
        match &parked {
            Err(error) => return Err(said(&format!("it failed: {error}"))),
            Ok(Waited::Ready(())) if token => {
                if end > start + bound {
                    return Err(said("the token waiting for it was taken late"));
                }
            }
            Ok(Waited::Ready(())) => {
                let Some(fired) = fired else { return Err(said("it woke with no unpark")) };
                if end > fired + bound {
                    return Err(said(&format!("woken more than {bound:?} after its unpark")));
                }
            }
            Ok(Waited::Deadline) => {
                if token {
                    return Err(said("the token waiting for it was lost"));
                }
                if let Some(fired) = fired {
                    return Err(said(&format!("the unpark at {fired:?} was lost: a lost wakeup")));
                }
                let Some(deadline) = deadline else {
                    return Err(said("a park with no deadline said its deadline passed"));
                };
                if end < deadline.get() {
                    return Err(said("its deadline had not passed"));
                }
            }
        }
        if let Some(deadline) = deadline
            && end > deadline.get() + bound
        {
            return Err(said(&format!("past its deadline by more than {bound:?}")));
        }
        // The unparks still to come land now, after the park, and a stray one: a token for
        // the next park.
        let late: Vec<Duration> = sim.unparks.borrow_mut().drain(..).collect(); // alloc: a test's record
        token = !late.is_empty() || round.stray;
        for _ in late {
            sim.fire(now.get());
        }
        if round.stray {
            sim.fire(now.get());
        }
        let left = word.load(Ordering::SeqCst);
        if (left == parking::NOTIFIED) != token {
            return Err(said(&format!(
                "the word is {left} after it, with a token waiting {token}"
            )));
        }
    }
    Ok(())
}

/// The invariant: whatever interrupts it and whenever its unpark comes, no wakeup is lost and
/// every park ends by its deadline.
#[test]
fn no_unpark_is_lost_under_interruption() {
    blaze_sim::check(Config::cases(300), |tape| drawn_parks(tape, ParkPlant::None, Through::Core));
}

/// What the park's oracle says of an unpark or a token lost, whichever way the loss shows: a
/// park that ends at its deadline after one, a token gone, or a futex wait nothing will wake.
const LOST: &str = "lost";

/// The lost wakeup the oracle must catch in one of [`CONTROL_SEEDS`] cases: a park that looks
/// for a token and then stores over one that came between.
#[test]
fn the_oracle_catches_a_lost_wakeup() {
    blaze_sim::control(
        Config::cases(CONTROL_SEEDS),
        ParkPlant::None,
        ParkPlant::CheckThenStore,
        LOST,
        |tape, plant| drawn_parks(tape, plant, Through::Core),
    );
}

// --- The reap: a child's exit waited for, then reaped ------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Life {
    Running,
    Zombie,
    Reaped,
}

struct SimChild {
    exits: Duration,
    status: i32,
    life: Life,
}

/// One child: when it exits after its reap begins (zero: it had exited), its status, and the
/// reap's deadline.
#[derive(Debug, Clone, Copy)]
struct Kid {
    exits: Duration,
    status: i32,
    deadline: Option<Duration>,
}

/// The kernel's choices (which call a fault hits, where, each latency) are drawn from the
/// case's tape as the run goes.
#[derive(Debug)]
struct ReapPlan {
    faults: FutexFaults,
    kids: Vec<Kid>,
}

fn reap_plan(tape: &Tape) -> ReapPlan {
    let faults = futex_faults(tape);
    let n = tape.int_in(1..=MOST_WAITS, "children");
    let kids = (0..n)
        .map(|_| Kid {
            exits: match tape.int_in(0..=3u64, "exit kind") {
                0 => Duration::ZERO,
                _ => us(tape.int_in(0..=100_000u64, "exits us")),
            },
            status: tape.int_in(0..=255i32, "status"),
            deadline: match tape.int_in(0..=2u64, "deadline kind") {
                0 => None,
                _ => Some(us(tape.int_in(0..=150_000u64, "deadline us"))),
            },
        })
        .collect(); // alloc: a test's plan
    ReapPlan { faults, kids }
}

/// The kernel's children on the virtual clock.
struct ChildSim<'a> {
    now: &'a Cell<Duration>,
    tape: &'a Tape,
    faults: FutexFaults,
    kids: RefCell<BTreeMap<i32, SimChild>>,
}

impl ChildSim<'_> {
    fn latency(&self) -> Duration {
        Duration::from_nanos(1 + self.tape.below(self.faults.max_latency.as_nanos() as u64 + 1, "latency ns"))
    }

    /// Children whose exit has come are zombies.
    fn settle(&self) {
        let now = self.now.get();
        for kid in self.kids.borrow_mut().values_mut() {
            if kid.life == Life::Running && kid.exits <= now {
                kid.life = Life::Zombie;
            }
        }
    }

    fn life(&self, pid: i32) -> Option<Life> {
        self.kids.borrow().get(&pid).map(|it| it.life)
    }

    fn no_child(pid: i32) -> io::Error {
        // alloc: a test's error
        io::Error::other(format!("ECHILD: process {pid} is no child, or was reaped already"))
    }

    /// One `waitid` call for `pid`, given `left`.
    fn call(&self, pid: i32, keep: bool, left: Option<Duration>) -> io::Result<Once<()>> {
        let t0 = self.now.get();
        let latency = self.latency();
        let at = |n: u64, label: &'static str| {
            self.tape.one_in(n, label).then(|| t0 + Duration::from_nanos(self.tape.below(100_000, label)))
        };
        let (eintr, spurious) = (at(self.faults.eintr_one_in, "eintr"), at(self.faults.spurious_one_in, "spurious"));
        self.settle();
        let exits = match self.kids.borrow().get(&pid) {
            None => return Err(ChildSim::no_child(pid)),
            Some(kid) => match kid.life {
                Life::Reaped => return Err(ChildSim::no_child(pid)),
                Life::Zombie => t0,
                Life::Running => kid.exits,
            },
        };
        let mut end = (exits, Once::Ready(()));
        let timeout = left.map(|it| (t0.saturating_add(it), Once::TimedOut));
        for (at, why) in [
            timeout,
            eintr.map(|it| (it, Once::Interrupted)),
            spurious.map(|it| (it, Once::Spurious)),
        ]
        .into_iter()
        .flatten()
        {
            if at < end.0 {
                end = (at, why);
            }
        }
        self.now.set(end.0.max(t0) + latency);
        if end.1 == Once::Ready(()) {
            self.settle();
            if !keep && let Some(kid) = self.kids.borrow_mut().get_mut(&pid) {
                // Reaped by the wait itself: its status goes to no one.
                kid.life = Life::Reaped;
            }
        }
        Ok(end.1)
    }
}

impl Children for ChildSim<'_> {
    type Reaped = i32;

    fn reap_now(&self, pid: i32) -> io::Result<Option<i32>> {
        self.now.set(self.now.get() + self.latency());
        self.settle();
        let mut kids = self.kids.borrow_mut();
        let Some(kid) = kids.get_mut(&pid) else { return Err(ChildSim::no_child(pid)) };
        match kid.life {
            Life::Running => Ok(None),
            Life::Zombie => {
                kid.life = Life::Reaped;
                Ok(Some(kid.status))
            }
            Life::Reaped => Err(ChildSim::no_child(pid)),
        }
    }

    fn exited<C: Clock>(
        &self,
        what: &'static str,
        pid: i32,
        keep: bool,
        deadline: Option<Deadline>,
        clock: &C,
    ) -> Result<Waited<()>, WaitError> {
        until(what, clock, deadline, |left| self.call(pid, keep, left))
    }
}

#[cfg(target_os = "linux")]
impl ChildCalls for ChildSim<'_> {
    type Reaped = i32;
    /// The child's pid: the simulated kernel has no descriptors.
    type Pidfd = i32;

    fn reap_now(&self, pid: i32) -> io::Result<Option<i32>> {
        Children::reap_now(self, pid)
    }

    fn exited_now(&self, pid: i32, keep: bool) -> io::Result<bool> {
        self.now.set(self.now.get() + self.latency());
        self.settle();
        let mut kids = self.kids.borrow_mut();
        let Some(kid) = kids.get_mut(&pid) else { return Err(ChildSim::no_child(pid)) };
        match kid.life {
            Life::Running => Ok(false),
            Life::Zombie => {
                if !keep {
                    // Reaped by the look itself: its status goes to no one.
                    kid.life = Life::Reaped;
                }
                Ok(true)
            }
            Life::Reaped => Err(ChildSim::no_child(pid)),
        }
    }

    fn pidfd(&self, pid: i32) -> io::Result<i32> {
        match self.life(pid) {
            Some(Life::Running | Life::Zombie) => Ok(pid),
            Some(Life::Reaped) | None => Err(ChildSim::no_child(pid)),
        }
    }

    fn poll_exit(&self, fd: &i32, left: Option<Duration>) -> io::Result<Once<()>> {
        // A poll reaps nothing.
        self.call(*fd, true, left)
    }
}

/// A drawn [`ReapPlan`] run with `plant`, the reap reaching the simulated children as `through`
/// says: the first broken rule, the oracle's.
fn drawn_reaps(tape: &Tape, plant: ReapPlant, through: Through) -> Result<(), Broke> {
    let plan = reap_plan(tape);
    tape.note(format_args!("plan {plan:?}"));
    run_reaps_through(tape, &plan, plant, through).map_err(Broke::Oracle)
}

/// `plan`'s children reaped one after another on one clock, with `plant`, the kernel drawing
/// from `tape`, the reap reaching it as `through` says: the first broken rule.
fn run_reaps_through(tape: &Tape, plan: &ReapPlan, plant: ReapPlant, through: Through) -> Result<(), String> {
    let now = Cell::new(Duration::ZERO);
    let clock = Virtual(&now);
    let sim = ChildSim {
        now: &now,
        tape,
        faults: plan.faults,
        kids: RefCell::new(BTreeMap::new()),
    };
    let bound = plan.faults.bound();
    let what = "a simulated child's exit";
    for (n, kid) in plan.kids.iter().enumerate() {
        let pid = 100 + n as i32;
        let start = now.get();
        let exits = start + kid.exits;
        sim.kids
            .borrow_mut()
            .insert(pid, SimChild { exits, status: kid.status, life: Life::Running });
        let deadline = kid.deadline.map(|it| Deadline::after(start, it));
        let reap = |deadline| match through {
            Through::Core => reaping::reap(what, &sim, pid, deadline, &clock, plant),
            #[cfg(target_os = "linux")]
            Through::Direct => reaping::reap(what, &Direct(&sim), pid, deadline, &clock, plant),
        };
        let reaped = reap(deadline);
        let end = now.get();
        let life = sim.life(pid);
        let said = |what: &str| {
            format!(
                "child {pid} (exits {exits:?}, status {}, reap began {start:?}, deadline \
                 {deadline:?}) said {reaped:?} at {end:?}, its life now {life:?}: {what}",
                kid.status
            )
        };
        match &reaped {
            Err(error) if life == Some(Life::Reaped) => {
                return Err(said(&format!("it was reaped and its exit never reported ({error})")));
            }
            Err(error) => return Err(said(&format!("the reap failed: {error}"))),
            Ok(Waited::Ready(status)) => {
                if *status != kid.status {
                    return Err(said("reported with another status"));
                }
                if life != Some(Life::Reaped) {
                    return Err(said("reported and not reaped"));
                }
                if end < exits {
                    return Err(said("reported before it exited"));
                }
                if end > exits.max(start) + bound {
                    return Err(said(&format!("reported more than {bound:?} after it exited")));
                }
            }
            Ok(Waited::Deadline) => {
                if life == Some(Life::Reaped) {
                    return Err(said("it was reaped and its exit never reported"));
                }
                let Some(deadline) = deadline else {
                    return Err(said("a reap with no deadline said its deadline passed"));
                };
                if end < deadline.get() {
                    return Err(said("its deadline had not passed"));
                }
                if exits + bound <= deadline.get() {
                    return Err(said("it had exited well before the deadline"));
                }
                // No zombie is left: a reap with no deadline reports it.
                match reap(None) {
                    Ok(Waited::Ready(status))
                        if status == kid.status && sim.life(pid) == Some(Life::Reaped) => {}
                    other => {
                        return Err(said(&format!(
                            "reaped again with no deadline it said {other:?}, its life then {:?}: \
                             a child whose exit is never reported",
                            sim.life(pid)
                        )));
                    }
                }
            }
        }
        if let Some(deadline) = deadline
            && end > deadline.get() + bound
        {
            return Err(said(&format!("past its deadline by more than {bound:?}")));
        }
    }
    Ok(())
}

/// The invariant: whatever interrupts the wait and whenever the child exits, every child is
/// reaped once and its exit reported, by the reap's deadline or after it.
#[test]
fn every_reaped_child_is_reported_under_interruption() {
    blaze_sim::check(Config::cases(300), |tape| drawn_reaps(tape, ReapPlant::None, Through::Core));
}

/// What the reap's oracle says of a child the wait reaped, whose exit then goes to no one.
const NEVER_REPORTED: &str = "never reported";

/// The unreported child the oracle must catch in one of [`CONTROL_SEEDS`] cases: a wait that
/// reaps, leaving the reap after it nothing to report.
#[test]
fn the_oracle_catches_a_reaped_child_never_reported() {
    blaze_sim::control(
        Config::cases(CONTROL_SEEDS),
        ReapPlant::None,
        ReapPlant::ReapInWait,
        NEVER_REPORTED,
        |tape, plant| drawn_reaps(tape, plant, Through::Core),
    );
}

// --- The direct driver on the same kernels -------------------------------------------------

/// The invariant: on the direct driver (a futex wait to an absolute time on the kernel's clock,
/// made anew for every call), whatever interrupts it and whenever its unpark comes, no wakeup
/// is lost and every park ends by its deadline.
#[cfg(target_os = "linux")]
#[test]
fn no_unpark_is_lost_through_the_direct_driver() {
    blaze_sim::check(Config::cases(300), |tape| drawn_parks(tape, ParkPlant::None, Through::Direct));
}

/// The lost wakeup, planted in the park the direct driver waits for: caught through it in one
/// of [`CONTROL_SEEDS`] cases.
#[cfg(target_os = "linux")]
#[test]
fn the_oracle_catches_a_lost_wakeup_through_the_direct_driver() {
    blaze_sim::control(
        Config::cases(CONTROL_SEEDS),
        ParkPlant::None,
        ParkPlant::CheckThenStore,
        LOST,
        |tape, plant| drawn_parks(tape, plant, Through::Direct),
    );
}

/// The invariant: on the direct driver (a look with `waitid`, a poll of the child's pidfd, the
/// look again, then the reap), whatever interrupts the wait and whenever the child exits, every
/// child is reaped once and its exit reported, by the reap's deadline or after it.
#[cfg(target_os = "linux")]
#[test]
fn every_reaped_child_is_reported_through_the_direct_driver() {
    blaze_sim::check(Config::cases(300), |tape| drawn_reaps(tape, ReapPlant::None, Through::Direct));
}

/// The unreported child, planted in the reap the direct driver waits for (its `waitid` without
/// `WNOWAIT`): caught through it in one of [`CONTROL_SEEDS`] cases.
#[cfg(target_os = "linux")]
#[test]
fn the_oracle_catches_a_reaped_child_never_reported_through_the_direct_driver() {
    blaze_sim::control(
        Config::cases(CONTROL_SEEDS),
        ReapPlant::None,
        ReapPlant::ReapInWait,
        NEVER_REPORTED,
        |tape, plant| drawn_reaps(tape, plant, Through::Direct),
    );
}
