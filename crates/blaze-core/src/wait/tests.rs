//! The deadline wait under a simulated kernel (`testing` skill, dst-not-review): one Hegel test
//! draws a sequence of waits (a deadline or none, and when the awaited event arrives: before,
//! at or after the deadline, or never), and a swarm of kernel faults (an EINTR at random, a
//! train of them at a fixed period as the profile's sampler sent, spurious wakeups, a timeout
//! returned early, and each call's latency), runs the real retry loop ([`retry::drive`])
//! against a kernel on a virtual clock, and checks at every wait:
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
//! must find a seed it breaks. Then [`Sim`] waits through the same loop under its own
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

use hegel::{TestCase, generators as gs};

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
    sim::{Faults, Rng, Sim},
};

/// Seeds of [`plan_of`] the control searches for one its plant breaks.
const CONTROL_SEEDS: u64 = 256;

/// The seed [`replay_one_seed`] runs, and its plant: set them to what a failure names.
const REPLAY: (u64, Plant) = (0, Plant::None);

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

#[derive(Debug)]
struct Plan {
    /// The kernel's seed: which call a fault hits, where, and each call's latency.
    seed: u64,
    faults: KernelFaults,
    waits: Vec<Wait>,
}

/// A source of draws: Hegel's (shrunk, replayed by Hegel) or a seed's ([`plan_of`]).
trait Draw {
    /// In `lo..=hi`.
    fn int(&mut self, lo: u64, hi: u64) -> u64;

    fn coin(&mut self) -> bool {
        self.int(0, 1) == 1
    }
}

struct Drawn<'a>(&'a TestCase);

impl Draw for Drawn<'_> {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        self.0.draw(gs::integers::<u64>().min_value(lo).max_value(hi))
    }
}

impl Draw for Rng {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        match (hi - lo).checked_add(1) {
            Some(n) => lo + self.below(n),
            None => self.next_u64(),
        }
    }
}

fn us(n: u64) -> Duration {
    Duration::from_micros(n)
}

/// A fault's rate: off, or once in 1 to 8 calls.
fn sometimes(d: &mut impl Draw) -> u64 {
    match d.coin() {
        false => 0,
        true => d.int(1, 8),
    }
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let eintr_one_in = sometimes(d);
    let spurious_one_in = sometimes(d);
    let early_one_in = sometimes(d);
    // The sampler's shape: a period of 1 to 30 ms against waits of up to 200 ms.
    let train = match d.coin() {
        false => None,
        true => {
            let period = us(d.int(1_000, 30_000));
            Some((period, us(d.int(0, period.as_micros() as u64))))
        }
    };
    let max_latency = match d.coin() {
        false => Duration::ZERO,
        true => us(d.int(0, 2_000)),
    };
    let faults = KernelFaults { eintr_one_in, train, spurious_one_in, early_one_in, max_latency };
    let n = d.int(1, MOST_WAITS);
    let waits = (0..n)
        .map(|_| {
            let deadline = match d.int(0, 5) {
                0 => None,
                _ => Some(us(d.int(0, 200_000))),
            };
            let event = match deadline {
                None => Some(us(d.int(0, 500_000))),
                Some(deadline) => match d.int(0, 3) {
                    0 => Some(us(d.int(0, deadline.as_micros() as u64))),
                    1 => Some(deadline),
                    2 => Some(deadline + us(d.int(1, 100_000))),
                    _ => None,
                },
            };
            Wait { deadline, event }
        })
        .collect(); // alloc: a test's plan
    Plan { seed, faults, waits }
}

/// The plan of `seed`, for the control and replays.
fn plan_of(seed: u64) -> Plan {
    plan(&mut Rng::new(seed))
}

// --- The simulated kernel ------------------------------------------------------------------

fn one_in(rng: &mut Rng, n: u64) -> bool {
    n > 0 && rng.below(n) == 0
}

/// A point drawn in `[from, to)`, or `None` where that is empty.
fn within(rng: &mut Rng, from: Duration, to: Duration) -> Option<Duration> {
    let span = to.saturating_sub(from).as_nanos() as u64;
    (span > 0).then(|| from + Duration::from_nanos(rng.below(span)))
}

/// One blocking call at the clock's now, given `left` (`None`: no limit), for an event at
/// `event`: it blocks until the event or its timeout, unless a fault ends it first, and
/// returns a drawn latency after.
fn call(
    now: &Cell<Duration>,
    rng: &mut Rng,
    faults: &KernelFaults,
    event: Option<Duration>,
    left: Option<Duration>,
) -> io::Result<Once<()>> {
    let t0 = now.get();
    let latency = Duration::from_nanos(1 + rng.below(faults.max_latency.as_nanos() as u64 + 1));
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
    if one_in(rng, faults.eintr_one_in)
        && let Some(at) = within(rng, t0, end.0)
    {
        end = (at, Once::Interrupted);
    }
    if one_in(rng, faults.spurious_one_in)
        && let Some(at) = within(rng, t0, end.0)
    {
        end = (at, Once::Spurious);
    }
    if one_in(rng, faults.early_one_in)
        && end.1 == Once::TimedOut
        && let Some(at) = within(rng, t0, end.0)
    {
        end = (at, Once::TimedOut);
    }
    now.set(end.0 + latency);
    Ok(end.1)
}

// --- A run and its oracle ------------------------------------------------------------------

/// `plan`'s waits one after another on one clock, with `plant`: the first broken rule.
fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let now = Cell::new(Duration::ZERO);
    let clock = Virtual(&now);
    let mut rng = Rng::new(plan.seed);
    let faults = plan.faults;
    let bound = faults.latency_bound();
    for (n, wait) in plan.waits.iter().enumerate() {
        let start = now.get();
        let deadline = wait.deadline.map(|it| Deadline::after(start, it));
        let event = wait.event.map(|it| start + it);
        let retry = Retry::new("a simulated event", deadline, start).planted(plant);
        let waited = retry::drive(retry, &clock, |left| call(&now, &mut rng, &faults, event, left));
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
#[hegel::test(test_cases = 400)]
fn every_wait_ends_by_its_deadline_under_interruption(tc: TestCase) {
    let plan = plan(&mut Drawn(&tc));
    if let Err(broke) = run(&plan, Plant::None) {
        panic!("{broke}\nplan {plan:?}");
    }
}

/// The plant the oracle must catch in one of [`CONTROL_SEEDS`] seeded plans (test-the-cause on
/// the test itself): a retry with the first relative timeout overruns a deadline under a train
/// of interruptions.
#[test]
fn the_oracle_catches_a_retry_with_the_first_timeout() {
    let plant = Plant::SameTimeout;
    for seed in 0..CONTROL_SEEDS {
        if let Err(why) = run(&plan_of(seed), plant) {
            eprintln!(
                "{plant:?} caught at seed {seed} (REPLAY = ({seed}, Plant::{plant:?})): {why}"
            );
            return;
        }
    }
    panic!(
        "the oracle caught Plant::{plant:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak"
    );
}

/// One seeded plan, [`REPLAY`]'s: set it to a seed a failure names to run that case alone.
#[test]
fn replay_one_seed() {
    let (seed, plant) = REPLAY;
    if let Err(broke) = run(&plan_of(seed), plant) {
        panic!("seed {seed}, Plant::{plant:?}: {broke}");
    }
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
#[hegel::test(test_cases = 100)]
fn a_sim_under_interruption_delivers_everything_by_each_deadline(tc: TestCase) {
    let d = &mut Drawn(&tc);
    let seed = d.int(0, u64::MAX);
    let faults = Faults {
        interrupt_one_in: d.int(1, 4) as u32,
        max_latency: us(d.int(0, 50_000)),
        ..Faults::default()
    };
    let mut sim = Sim::new(seed, faults);
    let timers = d.int(1, 16);
    let writes = d.int(0, 8);
    let mut effects = Vec::new(); // alloc: a test's effects
    for n in 0..timers {
        effects.push(Effect::Timer { after: us(d.int(0, 300_000)), tag: Tag(n) });
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
        let deadline = match d.coin() {
            true => Deadline::NEVER,
            false => Deadline::after(sim.now(), us(d.int(0, 100_000))),
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

fn futex_faults(d: &mut impl Draw) -> FutexFaults {
    FutexFaults {
        eintr_one_in: sometimes(d),
        spurious_one_in: sometimes(d),
        max_latency: Duration::from_nanos(d.int(0, 20_000)),
        max_step: Duration::from_nanos(d.int(1, 50_000)),
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

#[derive(Debug)]
struct ParkPlan {
    seed: u64,
    faults: FutexFaults,
    rounds: Vec<Round>,
}

fn park_plan(d: &mut impl Draw) -> ParkPlan {
    let seed = d.int(0, u64::MAX);
    let faults = futex_faults(d);
    let n = d.int(1, MOST_WAITS);
    let rounds = (0..n)
        .map(|_| {
            let deadline = match d.int(0, 4) {
                0 => None,
                _ => Some(us(d.int(0, 50_000))),
            };
            // Nanoseconds, dense where the park's steps are; a park with no deadline always
            // gets one, or it would wait forever, as it should.
            let unpark = match (deadline, d.int(0, 2)) {
                (Some(_), 0) => None,
                _ => Some(Duration::from_nanos(d.int(0, 300_000))),
            };
            Round { deadline, unpark, stray: d.int(0, 5) == 0 }
        })
        .collect(); // alloc: a test's plan
    ParkPlan { seed, faults, rounds }
}

/// A futex on the virtual clock, with the unparks of a plan to run at their times: at a
/// switch point of the park once their time has come, or while the park waits in it.
struct FutexSim<'a> {
    now: &'a Cell<Duration>,
    rng: RefCell<Rng>,
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

    fn fault_at(&self, rng: &mut Rng, one_in_n: u64, t0: Duration) -> Option<Duration> {
        one_in(rng, one_in_n).then(|| t0 + Duration::from_nanos(rng.below(100_000)))
    }

    /// One wait call on `word` for `expected`, given `left`.
    fn call(
        &self,
        word: &AtomicU32,
        expected: u32,
        left: Option<Duration>,
    ) -> io::Result<Once<()>> {
        let t0 = self.now.get();
        let (latency, eintr, spurious) = {
            let mut rng = self.rng.borrow_mut();
            let latency =
                Duration::from_nanos(1 + rng.below(self.faults.max_latency.as_nanos() as u64 + 1));
            let eintr = self.fault_at(&mut rng, self.faults.eintr_one_in, t0);
            let spurious = self.fault_at(&mut rng, self.faults.spurious_one_in, t0);
            (latency, eintr, spurious)
        };
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
        let step = {
            let mut rng = self.rng.borrow_mut();
            Duration::from_nanos(1 + rng.below(self.faults.max_step.as_nanos() as u64))
        };
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

/// `plan`'s parks one after another on one word and clock, with `plant`: the first broken rule.
fn run_parks(plan: &ParkPlan, plant: ParkPlant) -> Result<(), String> {
    run_parks_through(plan, plant, Through::Core)
}

/// [`run_parks`], the park reaching the simulated futex as `through` says.
fn run_parks_through(plan: &ParkPlan, plant: ParkPlant, through: Through) -> Result<(), String> {
    let now = Cell::new(Duration::ZERO);
    let clock = Virtual(&now);
    let word = AtomicU32::new(parking::EMPTY);
    let sim = FutexSim {
        now: &now,
        rng: RefCell::new(Rng::new(plan.seed)),
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
#[hegel::test(test_cases = 300)]
fn no_unpark_is_lost_under_interruption(tc: TestCase) {
    let plan = park_plan(&mut Drawn(&tc));
    if let Err(broke) = run_parks(&plan, ParkPlant::None) {
        panic!("{broke}\nplan {plan:?}");
    }
}

/// The lost wakeup the oracle must catch in one of [`CONTROL_SEEDS`] seeded plans: a park that
/// looks for a token and then stores over one that came between.
#[test]
fn the_oracle_catches_a_lost_wakeup() {
    let plant = ParkPlant::CheckThenStore;
    for seed in 0..CONTROL_SEEDS {
        if let Err(why) = run_parks(&park_plan(&mut Rng::new(seed)), plant) {
            eprintln!("{plant:?} caught at seed {seed}: {why}");
            return;
        }
    }
    panic!(
        "the oracle caught ParkPlant::{plant:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak"
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

#[derive(Debug)]
struct ReapPlan {
    seed: u64,
    faults: FutexFaults,
    kids: Vec<Kid>,
}

fn reap_plan(d: &mut impl Draw) -> ReapPlan {
    let seed = d.int(0, u64::MAX);
    let faults = futex_faults(d);
    let n = d.int(1, MOST_WAITS);
    let kids = (0..n)
        .map(|_| Kid {
            exits: match d.int(0, 3) {
                0 => Duration::ZERO,
                _ => us(d.int(0, 100_000)),
            },
            status: d.int(0, 255) as i32,
            deadline: match d.int(0, 2) {
                0 => None,
                _ => Some(us(d.int(0, 150_000))),
            },
        })
        .collect(); // alloc: a test's plan
    ReapPlan { seed, faults, kids }
}

/// The kernel's children on the virtual clock.
struct ChildSim<'a> {
    now: &'a Cell<Duration>,
    rng: RefCell<Rng>,
    faults: FutexFaults,
    kids: RefCell<BTreeMap<i32, SimChild>>,
}

impl ChildSim<'_> {
    fn latency(&self) -> Duration {
        let mut rng = self.rng.borrow_mut();
        Duration::from_nanos(1 + rng.below(self.faults.max_latency.as_nanos() as u64 + 1))
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
        let (eintr, spurious) = {
            let mut rng = self.rng.borrow_mut();
            let at = |rng: &mut Rng, n| {
                one_in(rng, n).then(|| t0 + Duration::from_nanos(rng.below(100_000)))
            };
            (at(&mut rng, self.faults.eintr_one_in), at(&mut rng, self.faults.spurious_one_in))
        };
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

/// `plan`'s children reaped one after another on one clock, with `plant`: the first broken rule.
fn run_reaps(plan: &ReapPlan, plant: ReapPlant) -> Result<(), String> {
    run_reaps_through(plan, plant, Through::Core)
}

/// [`run_reaps`], the reap reaching the simulated children as `through` says.
fn run_reaps_through(plan: &ReapPlan, plant: ReapPlant, through: Through) -> Result<(), String> {
    let now = Cell::new(Duration::ZERO);
    let clock = Virtual(&now);
    let sim = ChildSim {
        now: &now,
        rng: RefCell::new(Rng::new(plan.seed)),
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
#[hegel::test(test_cases = 300)]
fn every_reaped_child_is_reported_under_interruption(tc: TestCase) {
    let plan = reap_plan(&mut Drawn(&tc));
    if let Err(broke) = run_reaps(&plan, ReapPlant::None) {
        panic!("{broke}\nplan {plan:?}");
    }
}

/// The unreported child the oracle must catch in one of [`CONTROL_SEEDS`] seeded plans: a wait
/// that reaps, leaving the reap after it nothing to report.
#[test]
fn the_oracle_catches_a_reaped_child_never_reported() {
    let plant = ReapPlant::ReapInWait;
    for seed in 0..CONTROL_SEEDS {
        if let Err(why) = run_reaps(&reap_plan(&mut Rng::new(seed)), plant) {
            eprintln!("{plant:?} caught at seed {seed}: {why}");
            return;
        }
    }
    panic!(
        "the oracle caught ReapPlant::{plant:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak"
    );
}

// --- The direct driver on the same kernels -------------------------------------------------

/// The invariant: on the direct driver (a futex wait to an absolute time on the kernel's clock,
/// made anew for every call), whatever interrupts it and whenever its unpark comes, no wakeup
/// is lost and every park ends by its deadline.
#[cfg(target_os = "linux")]
#[hegel::test(test_cases = 300)]
fn no_unpark_is_lost_through_the_direct_driver(tc: TestCase) {
    let plan = park_plan(&mut Drawn(&tc));
    if let Err(broke) = run_parks_through(&plan, ParkPlant::None, Through::Direct) {
        panic!("{broke}\nplan {plan:?}");
    }
}

/// The lost wakeup, planted in the park the direct driver waits for: caught through it in one
/// of [`CONTROL_SEEDS`] seeded plans.
#[cfg(target_os = "linux")]
#[test]
fn the_oracle_catches_a_lost_wakeup_through_the_direct_driver() {
    let plant = ParkPlant::CheckThenStore;
    for seed in 0..CONTROL_SEEDS {
        let plan = park_plan(&mut Rng::new(seed));
        if let Err(why) = run_parks_through(&plan, plant, Through::Direct) {
            eprintln!("{plant:?} caught through the direct driver at seed {seed}: {why}");
            return;
        }
    }
    panic!(
        "the oracle caught ParkPlant::{plant:?} through the direct driver at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak"
    );
}

/// The invariant: on the direct driver (a look with `waitid`, a poll of the child's pidfd, the
/// look again, then the reap), whatever interrupts the wait and whenever the child exits, every
/// child is reaped once and its exit reported, by the reap's deadline or after it.
#[cfg(target_os = "linux")]
#[hegel::test(test_cases = 300)]
fn every_reaped_child_is_reported_through_the_direct_driver(tc: TestCase) {
    let plan = reap_plan(&mut Drawn(&tc));
    if let Err(broke) = run_reaps_through(&plan, ReapPlant::None, Through::Direct) {
        panic!("{broke}\nplan {plan:?}");
    }
}

/// The unreported child, planted in the reap the direct driver waits for (its `waitid` without
/// `WNOWAIT`): caught through it in one of [`CONTROL_SEEDS`] seeded plans.
#[cfg(target_os = "linux")]
#[test]
fn the_oracle_catches_a_reaped_child_never_reported_through_the_direct_driver() {
    let plant = ReapPlant::ReapInWait;
    for seed in 0..CONTROL_SEEDS {
        let plan = reap_plan(&mut Rng::new(seed));
        if let Err(why) = run_reaps_through(&plan, plant, Through::Direct) {
            eprintln!("{plant:?} caught through the direct driver at seed {seed}: {why}");
            return;
        }
    }
    panic!(
        "the oracle caught ReapPlant::{plant:?} through the direct driver at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak"
    );
}
