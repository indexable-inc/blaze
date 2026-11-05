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

use std::{cell::Cell, convert::Infallible, fmt::Write as _, io, time::Duration};

use hegel::{TestCase, generators as gs};

use super::{
    Clock, Deadline, Once, Plant, WaitError, Waited,
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
