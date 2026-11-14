//! The named locks under simulation (`testing` skill, dst-not-review;
//! docs/v2/plan/design/named-locks.md, section 8).
//!
//! The convoy DST: workers take a hot lock, run a critical section on the virtual clock that
//! [`Sim::stall_holders`] stalls once in a while, and let it go; a turn loop must read the
//! state they keep once a turn. One test draws, from its case's tape, the worker count, the
//! turns, the stalls, the schedule (which of the actors due at one virtual time steps first)
//! and the workload (each section's work, each think and gap), and every actor
//! is a state machine one OS thread steps in virtual-time order, its guard or its wait
//! ([`Lock::enqueue`], [`Queued::take`]) kept in its state between steps, so no step ever
//! blocks. Its oracle:
//!
//! - every turn ends within [`TURN_BOUND`] of its start on the virtual clock;
//! - every turn reads the state every critical section let go so far left;
//! - every wait any actor made (the turn loop's above all) is recorded as exactly one
//!   [`LockWait`] naming the lock, the waiter's span and thread, the holder it waited behind
//!   last (the stalled worker's span and thread) and a `wait_ns` equal to the virtual wait; and
//!   every wait span the lock opened is closed.
//!
//! The planted bug ([`Plant::HotLock`]) has the turn loop read through the hot lock, as the
//! daemon's turn loop read `blaze_profile::flight::intern` on 2026-11-05;
//! [`the_oracle_catches_a_hot_lock_on_the_turn_loop`] must find a case it breaks. The fixed
//! design ([`Plant::None`]) publishes the state through an atomic the turn loop loads, never
//! touching the lock, and must pass every case.
//!
//! Then the wait-for graph ([`graph`], [`stall::analyze`]) under the same probe: a cycle of two
//! holders is one deadlock, a stalled holder with waiters behind it one convoy, and a healthy
//! run nothing. The registry is the process's, so each test's locks have names of their own
//! and each test reads only the rows of its own locks. Last, two real threads contend one lock
//! and record one wait behind the holder.

use std::{
    any::type_name,
    fmt::Write as _,
    mem,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use blaze_sim::{Broke, Config, Tape};

use super::{
    Awaits, Lock, LockGuard, LockWait, Mode, Probe, Queued, graph, set_thread_mark,
    stall::{self, Blocked, Snapshot, Stall, WaitRow},
};
use crate::{
    Driver,
    sim::{Faults, Sim},
};

/// Cases the control searches for one its plant breaks.
const CONTROL_SEEDS: u32 = 256;

/// What the oracle says of a turn that did not end within [`TURN_BOUND`]: what the planted hot
/// lock does. Any other finding is a wrong read or record, which no design may do.
const LATE: &str = "past TURN_BOUND";

/// How long a turn of the turn loop may take, from its start to its end, on the virtual clock.
const TURN_BOUND: Duration = Duration::from_millis(2);

/// Most workers a plan draws.
const MOST_WORKERS: u64 = 6;

/// Most turns a plan draws.
const MOST_TURNS: u64 = 40;

/// The steps a run may take before it says it ran out (`bounded-progress`).
const MOST_STEPS: usize = 100_000;

/// The turn loop's actor id (its thread mark and probe thread). Workers are 1 to
/// [`MOST_WORKERS`].
const TURN_LOOP: u64 = 100;

/// A worker thinks up to this long, in microseconds, between its critical sections.
const THINK_US: u64 = 500;

/// The turn loop waits 100 to 100 plus this many microseconds between turns.
const GAP_US: u64 = 900;

/// A turn's own work: 20 to 20 plus this many microseconds.
const TURN_WORK_US: u64 = 80;

fn us(n: u64) -> Duration {
    Duration::from_micros(n)
}

fn ns(at: Duration) -> u64 {
    u64::try_from(at.as_nanos()).expect("a virtual time past 584 years")
}

/// An actor's span, as the probe gives it.
fn span_of(actor: u64) -> u64 {
    0x5_0000 + actor
}

// --- The probe -----------------------------------------------------------------------------

/// A simulation's probe: the clock, span and thread are what the test sets before each actor
/// step, and the waits it records are kept. Leaked, as a lock holds its probe as `&'static`;
/// each test has its own, so the process's ([`super::install`]) is never touched.
struct Probed {
    now_ns: AtomicU64,
    span: AtomicU64,
    thread: AtomicU64,
    /// Wait spans opened and closed.
    began: AtomicU64,
    ended: AtomicU64,
    /// Told of each wait span opened, for a test on real threads to know a waiter waits.
    on_begin: Option<crossbeam_channel::Sender<()>>,
    waits: Lock<Vec<LockWait>>,
}

impl Probed {
    fn leaked() -> &'static Probed {
        Probed::leaked_telling(None)
    }

    fn leaked_telling(on_begin: Option<crossbeam_channel::Sender<()>>) -> &'static Probed {
        // alloc: a test's probe, leaked so its locks can hold it as `&'static`.
        Box::leak(Box::new(Probed {
            now_ns: AtomicU64::new(0),
            span: AtomicU64::new(0),
            thread: AtomicU64::new(0),
            began: AtomicU64::new(0),
            ended: AtomicU64::new(0),
            on_begin,
            waits: Lock::unrecorded("dst.lock.probe.waits", Vec::new()),
        }))
    }

    /// The next step is `actor`'s, at `now`: the probe's span, thread and clock and the
    /// thread's mark in the wait-for graph are its.
    fn act(&self, actor: u64, now: Duration) {
        self.now_ns.store(ns(now), Ordering::Relaxed);
        self.span.store(span_of(actor), Ordering::Relaxed);
        self.thread.store(actor, Ordering::Relaxed);
        set_thread_mark(actor);
    }

    fn recorded(&self) -> Vec<LockWait> {
        self.waits.lock().clone() // alloc: a test's read of its record
    }
}

impl Probe for Probed {
    fn now_ns(&self) -> u64 {
        self.now_ns.load(Ordering::Relaxed)
    }

    fn span(&self) -> u64 {
        self.span.load(Ordering::Relaxed)
    }

    fn thread(&self) -> u64 {
        self.thread.load(Ordering::Relaxed)
    }

    fn wait_begin(&self, _: Awaits) -> u64 {
        if let Some(tell) = &self.on_begin {
            _ = tell.try_send(());
        }
        self.began.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn wait_end(&self, _: u64) {
        self.ended.fetch_add(1, Ordering::Relaxed);
    }

    fn record(&self, wait: &LockWait) {
        self.waits.lock().push(*wait); // alloc: a test's record
    }
}

// --- The plan ------------------------------------------------------------------------------

/// The design under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plant {
    /// Workers publish the state through an atomic after their critical section; the turn
    /// loop loads it and never touches the lock.
    None,
    /// The turn loop reads the state through the hot lock, waiting behind whoever holds it.
    HotLock,
}

#[derive(Debug)]
struct Plan {
    /// The `Sim`'s seed: which sections stall, for how long. The workload (each section's
    /// work, each think and gap) is drawn from the tape as the run goes.
    seed: u64,
    workers: u64,
    turns: u64,
    /// The stall fault: once in `.0` sections, up to `.1`; `None` for none.
    stall: Option<(u32, Duration)>,
    /// A section's work is 1 to this many microseconds.
    most_work_us: u64,
    /// Which of the actors due at one virtual time steps first, in turn (cycled; empty: the
    /// first).
    schedule: Vec<u64>,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let workers = tape.int_in(1..=MOST_WORKERS, "workers");
    let turns = tape.int_in(1..=MOST_TURNS, "turns");
    let stall = match tape.coin("holders stall") {
        false => None,
        true => Some((tape.int_in(1..=4u32, "stall one in"), us(tape.int_in(1..=20_000u64, "most stall us")))),
    };
    let most_work_us = tape.int_in(1..=200u64, "most work us");
    let schedule = tape.vec(0..=64, "schedule", |tape| tape.int_in(0..=MOST_WORKERS, "first due"));
    Plan { seed, workers, turns, stall, most_work_us, schedule }
}

// --- A run and its oracle ------------------------------------------------------------------

/// What an actor does next.
enum Doing<'a> {
    /// A worker takes the lock; the turn loop begins a turn.
    Next,
    /// Waits for the lock since `since`, woken when its holder lets go.
    Queued { queued: Queued<'a, u64>, since: Duration },
    /// A worker in its critical section, letting go at its ready time.
    Holding(LockGuard<'a, u64>),
    /// The turn loop's turn, which ends at its ready time.
    Reading,
}

struct Actor<'a> {
    id: u64,
    /// When it steps next; `None` while it waits for the lock and no release has woken it.
    ready: Option<Duration>,
    doing: Doing<'a>,
}

/// How an acquire that waited must be recorded.
#[derive(Debug, PartialEq, Eq)]
struct Expect {
    span: u64,
    thread: u64,
    start_ns: u64,
    wait_ns: u64,
    /// The holder's span and thread.
    holder: Option<(u64, u64)>,
}

impl Expect {
    /// `id`'s wait from `since` to `now`, behind the last to let go.
    fn of(id: u64, since: Duration, now: Duration, last: Option<u64>) -> Expect {
        Expect {
            span: span_of(id),
            thread: id,
            start_ns: ns(since),
            wait_ns: ns(now - since),
            holder: last.map(|it| (span_of(it), it)),
        }
    }

    fn got(wait: &LockWait) -> Expect {
        Expect {
            span: wait.span,
            thread: wait.thread,
            start_ns: wait.start_ns,
            wait_ns: wait.wait_ns,
            holder: wait.holder.map(|it| (it.span, it.thread)),
        }
    }
}

/// A worker's critical section, begun at `now` with `guard`: its work, drawn from `tape`, a
/// stall maybe, and the virtual time it lets go at.
fn hold<'a>(
    mut guard: LockGuard<'a, u64>,
    sim: &mut Sim,
    tape: &Tape,
    most_work_us: u64,
    now: Duration,
) -> (Doing<'a>, Duration) {
    *guard += 1;
    let took = sim.hold_for(us(1 + tape.below(most_work_us, "work us")));
    (Doing::Holding(guard), now + took)
}

/// The last lines of `log`, for a failure.
fn tail(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect(); // alloc: a failure's message
    lines[lines.len().saturating_sub(40)..].join("\n")
}

/// `plan` run with `plant` on a lock named `name`, its workload drawn from `tape`: the first
/// broken rule. A turn past [`TURN_BOUND`] is the oracle's finding saying [`LATE`].
fn run(tape: &Tape, plan: &Plan, plant: Plant, name: &'static str) -> Result<(), Broke> {
    let probe = Probed::leaked();
    let lock = Lock::probed(name, 0u64, probe);
    let published = AtomicU64::new(0);
    let mut sim = Sim::new(plan.seed, Faults::default());
    if let Some((one_in, most)) = plan.stall {
        sim.stall_holders(one_in, most);
    }
    // alloc: a test's actors.
    let mut actors: Vec<Actor<'_>> = (1..=plan.workers)
        .map(|id| Actor { id, ready: Some(us(tape.below(THINK_US, "think us"))), doing: Doing::Next })
        .collect();
    actors.push(Actor { id: TURN_LOOP, ready: Some(us(tape.below(GAP_US, "gap us"))), doing: Doing::Next });
    let mut expected: Vec<Expect> = Vec::new(); // alloc: the oracle's model
    let mut log = String::new(); // alloc: a trace for a failure
    let mut enqueued = 0u64;
    let mut last: Option<u64> = None;
    let mut committed = 0u64;
    let mut turn_start = Duration::ZERO;
    let mut turns = 0u64;
    let mut picks = plan.schedule.iter().copied().cycle();
    let ran: Result<(), Broke> = 'run: {
        for _ in 0..MOST_STEPS {
            let Some(at) = actors.iter().filter_map(|it| it.ready).min() else {
                break 'run Err(Broke::Harness(format!(
                    "no actor can step at {:?}\n{}",
                    sim.now(),
                    tail(&log)
                )));
            };
            let in_turn = actors.last().is_some_and(|it| !matches!(it.doing, Doing::Next));
            if in_turn && at.saturating_sub(turn_start) > TURN_BOUND {
                break 'run Err(Broke::Oracle(format!(
                    "turn {turns} began at {turn_start:?} and had not ended by {at:?}: past \
                     TURN_BOUND {TURN_BOUND:?}\n{}",
                    tail(&log)
                )));
            }
            let due = actors.iter().filter(|it| it.ready == Some(at)).count();
            let pick = picks.next().unwrap_or(0) as usize % due;
            let (i, _) = actors
                .iter()
                .enumerate()
                .filter(|(_, it)| it.ready == Some(at))
                .nth(pick)
                .expect("counted as due");
            sim.advance(at.saturating_sub(sim.now()));
            let now = sim.now();
            let actor = &mut actors[i];
            let id = actor.id;
            probe.act(id, now);
            let mut released = false;
            match (id == TURN_LOOP, mem::replace(&mut actor.doing, Doing::Next)) {
                (false, Doing::Next) => match lock.try_lock() {
                    Some(guard) => {
                        let (doing, until) = hold(guard, &mut sim, tape, plan.most_work_us, now);
                        _ = writeln!(log, "{now:?} worker {id} takes it, until {until:?}");
                        (actor.doing, actor.ready) = (doing, Some(until));
                    }
                    None => {
                        _ = writeln!(log, "{now:?} worker {id} waits");
                        enqueued += 1;
                        actor.doing = Doing::Queued { queued: lock.enqueue(), since: now };
                        actor.ready = None;
                    }
                },
                (false, Doing::Queued { queued, since }) => match queued.take() {
                    Ok(guard) => {
                        expected.push(Expect::of(id, since, now, last));
                        let (doing, until) = hold(guard, &mut sim, tape, plan.most_work_us, now);
                        _ = writeln!(log, "{now:?} worker {id} takes it after waiting, until {until:?}");
                        (actor.doing, actor.ready) = (doing, Some(until));
                    }
                    Err(queued) => {
                        actor.doing = Doing::Queued { queued, since };
                        actor.ready = None;
                    }
                },
                (false, Doing::Holding(guard)) => {
                    committed += 1;
                    if plant == Plant::None {
                        published.store(*guard, Ordering::Release);
                    }
                    drop(guard);
                    _ = writeln!(log, "{now:?} worker {id} lets go");
                    (released, last) = (true, Some(id));
                    actor.ready = Some(now + us(tape.below(THINK_US, "think us")));
                }
                (true, Doing::Next) => {
                    turn_start = now;
                    _ = writeln!(log, "{now:?} turn {turns} begins");
                    let seen = match plant {
                        Plant::None => Some(published.load(Ordering::Acquire)),
                        Plant::HotLock => match lock.try_lock() {
                            Some(guard) => {
                                let seen = *guard;
                                drop(guard);
                                (released, last) = (true, Some(id));
                                Some(seen)
                            }
                            None => {
                                _ = writeln!(log, "{now:?} the turn loop waits");
                                enqueued += 1;
                                actor.doing = Doing::Queued { queued: lock.enqueue(), since: now };
                                actor.ready = None;
                                None
                            }
                        },
                    };
                    if let Some(seen) = seen {
                        if seen != committed {
                            break 'run Err(Broke::Oracle(format!(
                                "turn {turns} read {seen}, not the {committed} sections let go"
                            )));
                        }
                        actor.doing = Doing::Reading;
                        actor.ready = Some(now + us(20 + tape.below(TURN_WORK_US, "turn work us")));
                    }
                }
                (true, Doing::Queued { queued, since }) => match queued.take() {
                    Ok(guard) => {
                        expected.push(Expect::of(id, since, now, last));
                        let seen = *guard;
                        drop(guard);
                        (released, last) = (true, Some(id));
                        _ = writeln!(log, "{now:?} the turn loop reads after waiting");
                        if seen != committed {
                            break 'run Err(Broke::Oracle(format!(
                                "turn {turns} read {seen}, not the {committed} sections let go"
                            )));
                        }
                        actor.doing = Doing::Reading;
                        actor.ready = Some(now + us(20 + tape.below(TURN_WORK_US, "turn work us")));
                    }
                    Err(queued) => {
                        actor.doing = Doing::Queued { queued, since };
                        actor.ready = None;
                    }
                },
                (true, Doing::Reading) => {
                    let took = now - turn_start;
                    if took > TURN_BOUND {
                        break 'run Err(Broke::Oracle(format!(
                            "turn {turns} took {took:?} from {turn_start:?}: past TURN_BOUND \
                             {TURN_BOUND:?}\n{}",
                            tail(&log)
                        )));
                    }
                    turns += 1;
                    if turns == plan.turns {
                        break 'run Ok(());
                    }
                    actor.ready = Some(now + us(100 + tape.below(GAP_US, "gap us")));
                }
                (false, Doing::Reading) | (true, Doing::Holding(_)) => {
                    break 'run Err(Broke::Harness(format!("actor {id} in a state not its own")));
                }
            }
            if released {
                for it in &mut actors {
                    if matches!(it.doing, Doing::Queued { .. }) {
                        it.ready = Some(now);
                    }
                }
            }
        }
        Err(Broke::Harness(format!("the run took more than {MOST_STEPS} steps")))
    };
    // Every wait and guard ends before the record is read.
    drop(actors);
    ran?;
    let (began, ended) =
        (probe.began.load(Ordering::Relaxed), probe.ended.load(Ordering::Relaxed));
    if began != enqueued || ended != began {
        return Err(Broke::Oracle(format!(
            "{enqueued} waits opened {began} wait spans and closed {ended}"
        )));
    }
    let recorded = probe.recorded();
    if recorded.len() != expected.len() {
        return Err(Broke::Oracle(format!(
            "{} waits ended with the lock, {} recorded:\nwanted {expected:?}\ngot {recorded:?}",
            expected.len(),
            recorded.len()
        )));
    }
    for (n, (got, want)) in recorded.iter().zip(&expected).enumerate() {
        let said = |what: &str| Broke::Oracle(format!("wait {n} {what}: got {got:?}, wanted {want:?}"));
        if got.lock != name || got.mode != Mode::Lock || got.value_type != type_name::<u64>() {
            return Err(said("names another lock"));
        }
        if got.site.file() != file!() || got.holder.is_some_and(|it| it.site.file() != file!()) {
            return Err(said("names a site outside this test"));
        }
        if Expect::got(got) != *want {
            return Err(said("is recorded wrong"));
        }
    }
    Ok(())
}

// --- The convoy tests ----------------------------------------------------------------------

/// The invariant: with the state published through an atomic, a stalled holder never holds up
/// a turn, every turn reads the latest state, and every wait among the workers is recorded
/// once, behind the holder it waited for.
#[test]
fn the_turn_loop_never_waits_behind_a_stalled_holder() {
    blaze_sim::check(Config::cases(300), |tape| {
        let plan = plan(tape);
        tape.note(format_args!("plan {plan:?}"));
        run(tape, &plan, Plant::None, "dst.turn.state.fixed")
    });
}

/// The plant the oracle must catch in one of [`CONTROL_SEEDS`] cases (test-the-cause on the
/// test itself): a turn loop that reads through the hot lock waits behind a stalled holder past
/// its bound. Any other broken rule on the way is a bug in the locks, and ends the control.
#[test]
fn the_oracle_catches_a_hot_lock_on_the_turn_loop() {
    blaze_sim::control(Config::cases(CONTROL_SEEDS), Plant::None, Plant::HotLock, LATE, |tape, plant| {
        let plan = plan(tape);
        tape.note(format_args!("plan {plan:?}"));
        let name = match plant {
            Plant::None => "dst.turn.state.fixed",
            Plant::HotLock => "dst.turn.state.planted",
        };
        match run(tape, &plan, plant, name) {
            Err(Broke::Oracle(why)) if !why.contains(LATE) => {
                Err(Broke::Harness(format!("a bug in the locks, not the plant: {why}")))
            }
            ran => ran,
        }
    });
}

// --- The wait-for graph --------------------------------------------------------------------

/// The waits in progress on the locks named `names`, and only those: the registry is the
/// process's, and other tests' waits are in it.
fn rows_of(names: &[&str]) -> Snapshot {
    let mut snapshot = graph::snapshot();
    snapshot
        .waits
        .retain(|row| matches!(row.on, Blocked::Lock { name, .. } if names.contains(&name)));
    snapshot
}

const CYCLE_A: &str = "dst.graph.cycle.a";
const CYCLE_B: &str = "dst.graph.cycle.b";

/// Two holders each waiting on the other's lock are one deadlock, naming both threads and both
/// locks, once the threshold passes and not before; bystanders queued behind either holder
/// join no convoy and no second report.
#[test]
fn a_cycle_of_two_holders_is_one_deadlock() {
    blaze_sim::check(Config::cases(100), a_cycle_of_two_holders);
}

fn a_cycle_of_two_holders(tape: &Tape) {
    let mut sim = Sim::new(tape.next_u64("sim seed"), Faults::default());
    let threshold_us = tape.int_in(1..=100_000u64, "threshold us");
    let threshold = ns(us(threshold_us));
    let gap = us(tape.int_in(0..=threshold_us - 1, "gap us"));
    let past = us(tape.int_in(0..=50_000u64, "past us"));
    let one_first = tape.coin("1 waits first");
    let probe = Probed::leaked();
    let a = Lock::probed(CYCLE_A, 0u64, probe);
    let b = Lock::probed(CYCLE_B, 0u64, probe);
    probe.act(1, sim.now());
    let held_a = a.try_lock().expect("a is free");
    probe.act(2, sim.now());
    let held_b = b.try_lock().expect("b is free");
    // Actor 1 waits on b, actor 2 on a, in the drawn order, `gap` apart.
    let enqueue = |sim: &mut Sim, actor: u64| {
        probe.act(actor, sim.now());
        match actor {
            1 => b.enqueue(),
            _ => a.enqueue(),
        }
    };
    let (q1, q2) = match one_first {
        true => {
            let q1 = enqueue(&mut sim, 1);
            sim.advance(gap);
            (q1, enqueue(&mut sim, 2))
        }
        false => {
            let q2 = enqueue(&mut sim, 2);
            sim.advance(gap);
            (enqueue(&mut sim, 1), q2)
        }
    };
    let bystanders: Vec<_> = (0..tape.int_in(0..=4u64, "bystanders"))
        .map(|n| {
            probe.act(10 + n, sim.now());
            match tape.coin("bystander waits on a") {
                true => a.enqueue(),
                false => b.enqueue(),
            }
        })
        .collect(); // alloc: a test's waits
    let names = [CYCLE_A, CYCLE_B];
    let early = stall::analyze(&rows_of(&names), ns(sim.now()), threshold);
    assert!(early.is_empty(), "a stall before the threshold passed: {early:?}");
    sim.advance(us(threshold_us) + past);
    let now = ns(sim.now());
    let stalls = stall::analyze(&rows_of(&names), now, threshold);
    let [Stall::Deadlock { cycle }] = stalls.as_slice() else {
        panic!("not one deadlock: {stalls:?}");
    };
    let mut threads: Vec<u64> = cycle.iter().map(|it| it.thread).collect(); // alloc: a test's check
    threads.sort_unstable();
    assert_eq!(threads, [1, 2], "{cycle:?}");
    for row in cycle {
        let (want_on, want_holder) = match row.thread {
            1 => (CYCLE_B, 2),
            _ => (CYCLE_A, 1),
        };
        assert!(
            matches!(row.on, Blocked::Lock { name, mode: Mode::Lock, .. } if name == want_on),
            "{row:?}"
        );
        assert_eq!(row.holder, Some(want_holder), "{row:?}");
        assert_eq!(row.span, span_of(row.thread), "{row:?}");
    }
    let line = stalls[0].line(now);
    assert!(line.starts_with("deadlock:") && line.contains(CYCLE_A) && line.contains(CYCLE_B), "{line}");
    drop(bystanders);
    drop((q1, q2));
    drop((held_a, held_b));
    assert!(rows_of(&names).waits.is_empty(), "a wait outlived its end");
}

const CONVOY: &str = "dst.graph.convoy";

/// The convoy's head, by thread mark.
const HEAD: u64 = 100;

/// A holder stalled in its critical section with `n` waiters behind it is one convoy headed by
/// it, counting all `n`, once the threshold passes and not before; handed on, each waiter
/// records one wait behind the one before it.
#[test]
fn a_stalled_holder_is_one_convoy_of_every_waiter_behind_it() {
    blaze_sim::check(Config::cases(100), a_stalled_holder_with_waiters);
}

fn a_stalled_holder_with_waiters(tape: &Tape) {
    let mut sim = Sim::new(tape.next_u64("sim seed"), Faults::default());
    let n = tape.int_in(1..=8u64, "waiters");
    let threshold_us = tape.int_in(1..=50_000u64, "threshold us");
    let threshold = ns(us(threshold_us));
    let most = us(tape.int_in(0..=50_000u64, "most stall us"));
    // Every waiter is in line before the threshold passes for the first.
    let gap_most = (threshold_us - 1) / n;
    sim.stall_holders(1, most);
    let probe = Probed::leaked();
    let lock = Lock::probed(CONVOY, 0u64, probe);
    probe.act(HEAD, sim.now());
    let held = lock.try_lock().expect("free");
    let mut queued = Vec::new(); // alloc: a test's waits
    for id in 1..=n {
        sim.advance(us(tape.int_in(0..=gap_most, "gap us")));
        probe.act(id, sim.now());
        queued.push((id, lock.enqueue()));
    }
    let names = [CONVOY];
    let early = stall::analyze(&rows_of(&names), ns(sim.now()), threshold);
    assert!(early.is_empty(), "a stall before the threshold passed: {early:?}");
    // The head's critical section: at least the threshold of work, and a stall.
    let took = sim.critical_section(us(threshold_us));
    assert!(took >= us(threshold_us) && took <= us(threshold_us) + most, "took {took:?}");
    let now = sim.now();
    let stalls = stall::analyze(&rows_of(&names), ns(now), threshold);
    let [Stall::Convoy { head, head_wait, holds, waiters, oldest }] = stalls.as_slice() else {
        panic!("not one convoy: {stalls:?}");
    };
    assert_eq!((*head, *head_wait, *waiters), (HEAD, None, n as usize), "{stalls:?}");
    assert!(matches!(holds, Blocked::Lock { name: CONVOY, mode: Mode::Lock, .. }), "{holds:?}");
    assert_eq!(oldest.thread, 1, "{oldest:?}");
    // The head lets go; each waiter takes the lock and lets go in turn, all at `now`.
    probe.act(HEAD, now);
    drop(held);
    for (id, wait) in queued {
        probe.act(id, now);
        match wait.take() {
            Ok(guard) => drop(guard),
            Err(_) => panic!("waiter {id}: the lock is held after its holder let go"),
        }
    }
    assert!(rows_of(&names).waits.is_empty(), "a wait outlived its end");
    let recorded = probe.recorded();
    assert_eq!(recorded.len(), n as usize, "{recorded:?}");
    let first_since = recorded[0].start_ns;
    for (k, wait) in recorded.iter().enumerate() {
        let id = k as u64 + 1;
        let behind = match k {
            0 => HEAD,
            _ => id - 1,
        };
        let holder = wait.holder.unwrap_or_else(|| panic!("waiter {id} has no holder: {wait:?}"));
        assert_eq!((wait.lock, wait.thread, wait.span), (CONVOY, id, span_of(id)), "{wait:?}");
        assert_eq!(wait.wait_ns, ns(now) - wait.start_ns, "{wait:?}");
        assert_eq!((holder.thread, holder.span), (behind, span_of(behind)), "{wait:?}");
        // The head took the lock without waiting: its hold counts from the first waiter's
        // start. Each waiter after took it at `now` and let it go at once.
        let (hold_ns, whole) = match k {
            0 => (ns(now) - first_since, false),
            _ => (0, true),
        };
        assert_eq!((holder.hold_ns, holder.whole), (hold_ns, whole), "{wait:?}");
    }
    assert_eq!(probe.began.load(Ordering::Relaxed), n);
    assert_eq!(probe.ended.load(Ordering::Relaxed), n);
}

const ORDER: &str = "dst.stall.order";

/// One snapshot's rows, built by hand and in no registry: the head ([`HEAD`], itself in IO)
/// holds [`ORDER`] with three waiters behind it, 2 and 3 queued at the same instant and before
/// 1; and threads 20 and 21 each wait on a lock the other holds.
fn order_rows() -> Vec<WaitRow> {
    let lock = |name: &'static str, at: usize| Blocked::Lock { name, mode: Mode::Lock, at };
    let row = |thread: u64, since_ns: u64, on: Blocked, holder: Option<u64>| WaitRow {
        thread,
        span: span_of(thread),
        since_ns,
        deadline_ns: None,
        on,
        holder,
        site: None,
    };
    vec![
        row(HEAD, 1, Blocked::Wait("dst.stall.order.io"), None),
        row(1, 9, lock(ORDER, 0x1000), Some(HEAD)),
        row(2, 5, lock(ORDER, 0x1000), Some(HEAD)),
        row(3, 5, lock(ORDER, 0x1000), Some(HEAD)),
        row(20, 7, lock("dst.stall.order.b", 0x3000), Some(21)),
        row(21, 8, lock("dst.stall.order.a", 0x2000), Some(20)),
    ] // alloc: a test's rows
}

/// The invariant: analyze's answer is the same for every order of the snapshot's rows.
///
/// The rows in a drawn order give what they give sorted by thread, and that answer is the
/// deadlock from its least thread, then the convoy whose oldest is thread 2, the lesser of the
/// two queued at the same instant.
#[test]
fn analyze_is_the_same_for_every_order_of_the_rows() {
    blaze_sim::check(Config::cases(100), |tape| {
        let (now, threshold) = (1_000, 100);
        let mut sorted = order_rows();
        sorted.sort_unstable_by_key(|row| row.thread);
        let mut drawn = sorted.clone(); // alloc: a test's rows
        tape.shuffle(&mut drawn, "row order");
        let want = stall::analyze(&Snapshot { waits: sorted }, now, threshold);
        let got = stall::analyze(&Snapshot { waits: drawn.clone() }, now, threshold);
        assert_eq!(got, want, "rows in the order {drawn:?}");
        let [Stall::Deadlock { cycle }, convoy] = want.as_slice() else {
            panic!("not one deadlock, then one stall: {want:?}");
        };
        let Stall::Convoy { head, head_wait, holds, waiters, oldest } = convoy else {
            panic!("not a convoy after the deadlock: {want:?}");
        };
        // alloc: a test's check
        let threads: Vec<u64> = cycle.iter().map(|it| it.thread).collect();
        assert_eq!(threads, [20, 21], "{cycle:?}");
        assert_eq!((*head, *waiters, oldest.thread), (HEAD, 3, 2), "{want:?}");
        assert_eq!(head_wait.map(|it| it.thread), Some(HEAD), "{want:?}");
        assert!(matches!(holds, Blocked::Lock { name: ORDER, .. }), "{holds:?}");
    });
}

/// The old rule, kept for the control: over the rows as given, a row replaces the oldest only
/// when its `since_ns` is strictly less, so a tie goes to whichever row comes first.
fn oldest_in_row_order(rows: &[WaitRow]) -> u64 {
    let mut oldest = rows[0];
    for row in &rows[1..] {
        if row.since_ns < oldest.since_ns {
            oldest = *row;
        }
    }
    oldest.thread
}

/// The control for [`analyze_is_the_same_for_every_order_of_the_rows`]: the old rule names a
/// different oldest for two orders of the same waiters, so that test's comparison catches it.
#[test]
fn the_oracle_catches_an_oldest_that_follows_the_row_order() {
    // alloc: a test's rows
    let waiters: Vec<WaitRow> =
        order_rows().into_iter().filter(|row| row.holder == Some(HEAD)).collect();
    let mut reversed = waiters.clone(); // alloc: a test's rows
    reversed.reverse();
    let (forward, backward) = (oldest_in_row_order(&waiters), oldest_in_row_order(&reversed));
    assert_ne!(forward, backward, "the old rule named thread {forward} for both orders");
    assert_eq!((forward, backward), (2, 3));
}

const HEALTHY: &str = "dst.graph.healthy";

/// A run whose waits are all shorter than the threshold reports nothing, nor does a lock taken
/// and let go with no one waiting, which leaves no row at all.
#[test]
fn a_healthy_run_reports_no_stall() {
    blaze_sim::check(Config::cases(100), a_healthy_run);
}

fn a_healthy_run(tape: &Tape) {
    let mut sim = Sim::new(tape.next_u64("sim seed"), Faults::default());
    let n = tape.int_in(0..=8u64, "waiters");
    let threshold_us = tape.int_in(2..=50_000u64, "threshold us");
    let threshold = ns(us(threshold_us));
    // The waiters' gaps and the holder's work together stay under the threshold.
    let gap_most = (threshold_us / 2) / (n + 1);
    let work = us(tape.int_in(0..=threshold_us / 2 - 1, "work us"));
    let probe = Probed::leaked();
    let lock = Lock::probed(HEALTHY, 0u64, probe);
    let names = [HEALTHY];
    for round in 0..tape.int_in(0..=8u64, "uncontended rounds") {
        probe.act(1 + round % 3, sim.now());
        let guard = lock.try_lock().expect("no one holds it");
        assert!(rows_of(&names).waits.is_empty(), "an uncontended lock left a row");
        sim.critical_section(us(tape.int_in(0..=100u64, "section us")));
        drop(guard);
    }
    probe.act(HEAD, sim.now());
    let held = lock.try_lock().expect("free");
    let mut queued = Vec::new(); // alloc: a test's waits
    for id in 1..=n {
        sim.advance(us(tape.int_in(0..=gap_most, "gap us")));
        probe.act(id, sim.now());
        queued.push((id, lock.enqueue()));
    }
    let took = sim.critical_section(work);
    assert_eq!(took, work, "a section stalled with stalls off");
    let stalls = stall::analyze(&rows_of(&names), ns(sim.now()), threshold);
    assert!(stalls.is_empty(), "a healthy run reported {stalls:?}");
    probe.act(HEAD, sim.now());
    drop(held);
    for (id, wait) in queued {
        probe.act(id, sim.now());
        match wait.take() {
            Ok(guard) => drop(guard),
            Err(_) => panic!("waiter {id}: the lock is held after its holder let go"),
        }
    }
    assert!(rows_of(&names).waits.is_empty(), "a wait outlived its end");
    assert_eq!(probe.recorded().len(), n as usize);
}

// --- Real threads --------------------------------------------------------------------------

/// A lock contended by two real threads records exactly one wait, the waiter's, behind the
/// holder and naming where the holder took it.
#[test]
fn a_contended_lock_on_two_threads_records_one_wait_behind_its_holder() {
    let (tell, told) = crossbeam_channel::bounded::<()>(1);
    let probe = Probed::leaked_telling(Some(tell));
    // alloc: a test's lock, leaked so the waiter's thread can borrow it.
    let lock: &'static Lock<u64> = Box::leak(Box::new(Lock::probed("dst.threads.state", 0, probe)));
    let line = line!() + 1;
    let held = lock.lock();
    let waiter = std::thread::Builder::new()
        .name("dst.lock.waiter".into())
        .spawn(move || *lock.lock() += 1)
        .expect("spawn the waiter");
    // The waiter counts as waiting from its wait span's start, before it parks: the holder's
    // release sees it from then.
    told.recv_timeout(Duration::from_secs(60)).expect("the waiter never began to wait");
    drop(held);
    waiter.join().expect("the waiter panicked");
    let recorded = probe.recorded();
    let [wait] = recorded.as_slice() else {
        panic!("not one wait: {recorded:?}");
    };
    let holder = wait.holder.unwrap_or_else(|| panic!("no holder: {wait:?}"));
    assert_eq!((wait.lock, wait.mode), ("dst.threads.state", Mode::Lock), "{wait:?}");
    assert_eq!((holder.site.file(), holder.site.line()), (file!(), line), "{wait:?}");
    assert_eq!(holder.mode, Mode::Lock, "{wait:?}");
    assert_eq!(wait.site.file(), file!(), "{wait:?}");
    assert_eq!(*lock.lock(), 1);
    assert_eq!(probe.began.load(Ordering::Relaxed), 1);
    assert_eq!(probe.ended.load(Ordering::Relaxed), 1);
}
