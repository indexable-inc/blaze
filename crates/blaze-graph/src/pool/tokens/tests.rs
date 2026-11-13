//! A pool runs no more jobs at once than it holds core tokens for.
//!
//! The deterministic test runs the real [`Gate`] under a model of the pool's loop around it
//! (`work` in `pool.rs`: a permit taken as a worker wakes, a take when every permit is taken
//! and the epoch moved, the permit given back before the worker marks itself asleep and looks
//! at the queue, the last look for a free permit of a worker that found none), one worker's
//! step at a time in an order the draws pick, against a fake ledger whose cores the draws move
//! (another process taking or giving back). Its oracle, after every step: the workers running
//! never pass the tokens the ledger counts for the pool, and the pool never gives back more
//! than it holds; once the draws end and every worker has run out its steps: no job is left
//! queued with every worker parked (no lost wake), the idle pool holds one token, and its end
//! gives every token back.
//!
//! Two plants ([`Plant`]) prove the oracle catches what it claims to, each in a control: a
//! pool that ignores its tokens (the pool before this module), and a worker with no permit that
//! parks without its last look (a lost wake). Replay: a control or [`replay_one_seed`] names a
//! seed of [`plan_of`]; the Hegel test prints its plan.
//!
//! The pool itself, with threads, runs on the same fake in the last tests: every job runs, the
//! jobs running at once never pass the tokens held, an idle pool keeps one and a dropped one
//! none; its control is the same pool with [`Gate::plant`] set.

use std::{fmt, sync::atomic::AtomicU64};

use blaze_rng::SplitMix64;
use hegel::{TestCase, generators as gs};

use super::{Gate, Ordering};
use crate::{
    ledger::{Cores, MachineError, Tokens},
    pool::Pool,
    pump::Lock,
    topology::Topology,
};

/// Seeds of [`plan_of`] a control searches for one its plant breaks.
const CONTROL_SEEDS: u64 = 512;

/// The seed [`replay_one_seed`] runs, and its plant: set them to what a failure names.
const REPLAY: (u64, Plant) = (0, Plant::None);

// --- The fake ledger ----------------------------------------------------------------------------

/// One process's cores on a machine whose other processes the test moves: it grants what fits
/// under `cap` (the floor past it), as [`crate::ledger::TakeCores`] decides for one slot, and
/// moves its epoch on every give and every take that gave some back, as the ledger does.
struct Fake(Lock<Ledger>);

#[derive(Default, Debug)]
struct Ledger {
    cap: u64,
    held: u64,
    epoch: u64,
    /// A give of more than was held: the most it passed by.
    over_given: u64,
}

impl Fake {
    // alloc: a test's ledger, kept for the process as a registered driver is
    fn leaked(cap: u64) -> &'static Fake {
        Box::leak(Box::new(Fake(Lock::new(
            "graph.pool.tokens.test_ledger",
            Ledger { cap, ..Ledger::default() },
        ))))
    }

    fn held(&self) -> u64 {
        self.0.lock().held
    }

    /// Other processes took or gave back: the cores that fit move to `cap`, and the epoch.
    fn set_cap(&self, cap: u64) {
        let mut ledger = self.0.lock();
        ledger.cap = cap;
        ledger.epoch += 1;
    }
}

impl Tokens for Fake {
    fn take(&self, want: u64, floor: u64) -> Result<Cores, MachineError> {
        let mut ledger = self.0.lock();
        let old = ledger.held;
        let fit = ledger.cap.saturating_sub(old).min(want);
        let granted = fit.max(floor.saturating_sub(old)).min(want);
        ledger.held += granted;
        if granted < want {
            ledger.epoch += 1;
        }
        Ok(Cores {
            granted,
            held: ledger.held,
            over: 0,
            counted: ledger.held,
            cores: ledger.cap,
            epoch: ledger.epoch,
        })
    }

    fn give(&self, cores: u64) -> Result<u64, MachineError> {
        let mut ledger = self.0.lock();
        ledger.over_given = ledger.over_given.max(cores.saturating_sub(ledger.held));
        ledger.held = ledger.held.saturating_sub(cores);
        ledger.epoch += 1;
        Ok(ledger.held)
    }

    fn epoch(&self) -> u64 {
        self.0.lock().epoch
    }
}

// --- The plan -----------------------------------------------------------------------------------

/// A source of draws: Hegel's (shrunk, replayed by Hegel) or a seed's ([`plan_of`]).
trait Draw {
    /// In `lo..=hi`.
    fn int(&mut self, lo: u64, hi: u64) -> u64;
}

struct Drawn<'a>(&'a TestCase);

impl Draw for Drawn<'_> {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        self.0.draw(gs::integers::<u64>().min_value(lo).max_value(hi))
    }
}

impl Draw for SplitMix64 {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
}

#[derive(Clone, Copy, Debug)]
enum Act {
    /// Jobs queued, and a wake as a submit's (`Shared::announce`): of the sleeping worker this
    /// many places on from the first, when none searches.
    Submit { jobs: u64, wakes: usize },
    /// The cores that fit for the pool move to this many.
    Cap(u64),
    /// This worker takes its next step.
    Step(usize),
}

#[derive(Clone, Debug)]
struct Plan {
    workers: usize,
    cap: u64,
    acts: Vec<Act>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let workers = d.int(1, 6) as usize;
    let cap = d.int(1, workers as u64 + 1);
    let count = d.int(1, 120);
    // alloc: a plan, once a case
    let acts = (0..count)
        .map(|_| match d.int(0, 9) {
            0..=1 => Act::Submit { jobs: d.int(1, 4), wakes: d.int(0, 5) as usize },
            2 => Act::Cap(d.int(1, workers as u64 + 1)),
            _ => Act::Step(d.int(0, workers as u64 - 1) as usize),
        })
        .collect();
    Plan { workers, cap, acts }
}

fn plan_of(seed: u64) -> Plan {
    plan(&mut SplitMix64::new(seed))
}

// --- The model of the pool's loop ---------------------------------------------------------------

/// What a test plants in the pool.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// [`Gate::enter`] ignores the tokens: every worker runs, as before this module.
    IgnoresTokens,
    /// A worker with no permit parks without its last look for a free one.
    NoLastLook,
}

/// Where a worker of the model is in `work`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum At {
    /// Parked: only a submit's wake moves it.
    Parked,
    /// At the loop's top with no permit; a searcher when a wake brought it.
    Awake { searcher: bool },
    /// Found every permit taken (and took none), still counted a searcher.
    Refused,
    /// Stopped counting, marked asleep: its next step is its last look for a free permit.
    Looking,
    /// On a permit: runs a queued job a step, or gives the permit back.
    Running,
    /// Gave its permit back (the last one gave back what was idle), not yet marked asleep.
    Leaving,
    /// Marked asleep: its next step looks at the queue.
    Checking,
}

impl At {
    fn sleeping(self) -> bool {
        matches!(self, At::Parked | At::Looking | At::Checking)
    }

    fn searcher(self) -> bool {
        matches!(self, At::Awake { searcher: true } | At::Refused)
    }
}

#[derive(Debug)]
enum Broke {
    /// The invariant does not hold.
    Oracle(String),
}

impl fmt::Display for Broke {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Broke::Oracle(why) => write!(f, "the oracle: {why}"),
        }
    }
}

struct World {
    fake: &'static Fake,
    gate: Gate,
    workers: Vec<At>,
    queued: u64,
    submitted: u64,
    ran: u64,
    plant: Plant,
}

impl World {
    fn new(plan: &Plan, plant: Plant) -> World {
        let fake = Fake::leaked(plan.cap);
        let gate = Gate::start(fake, plan.workers as u64);
        gate.plant.store(plant == Plant::IgnoresTokens, Ordering::Relaxed);
        World {
            fake,
            gate,
            workers: vec![At::Awake { searcher: false }; plan.workers],
            queued: 0,
            submitted: 0,
            ran: 0,
            plant,
        }
    }

    /// Wakes the sleeping worker `skip` places on from the first, as `Shared::wake` does.
    fn wake(&mut self, skip: usize) {
        let sleeping: Vec<usize> =
            (0..self.workers.len()).filter(|&at| self.workers[at].sleeping()).collect();
        if !sleeping.is_empty() {
            self.workers[sleeping[skip % sleeping.len()]] = At::Awake { searcher: true };
        }
    }

    fn act(&mut self, act: Act) -> Result<(), Broke> {
        match act {
            Act::Submit { jobs, wakes } => {
                self.queued += jobs;
                self.submitted += jobs;
                if !self.workers.iter().any(|it| it.searcher()) {
                    self.wake(wakes);
                }
            }
            Act::Cap(cap) => self.fake.set_cap(cap),
            Act::Step(at) => self.step(at),
        }
        self.check()
    }

    fn step(&mut self, at: usize) {
        let next = match self.workers[at] {
            At::Parked => At::Parked,
            At::Awake { searcher } => {
                if self.gate.enter() {
                    At::Running
                } else {
                    let queued = self.queued > 0;
                    let granted = self.gate.grow(|| queued);
                    for _ in 1..granted {
                        self.wake(0);
                    }
                    if granted > 0 && self.gate.enter() {
                        At::Running
                    } else if searcher {
                        At::Refused
                    } else {
                        At::Looking
                    }
                }
            }
            At::Refused => At::Looking,
            At::Looking if self.plant != Plant::NoLastLook && self.gate.free() => {
                At::Awake { searcher: false }
            }
            At::Looking => At::Parked,
            At::Running if self.queued > 0 => {
                self.queued -= 1;
                self.ran += 1;
                At::Running
            }
            At::Running => {
                if self.gate.leave() && self.queued == 0 {
                    self.gate.shrink();
                }
                At::Leaving
            }
            At::Leaving => At::Checking,
            At::Checking if self.queued > 0 => At::Awake { searcher: false },
            At::Checking => At::Parked,
        };
        self.workers[at] = next;
    }

    fn check(&self) -> Result<(), Broke> {
        let running = self.workers.iter().filter(|it| **it == At::Running).count() as u64;
        let held = self.fake.held();
        if running > held {
            return Err(Broke::Oracle(format!(
                "{running} workers run on {held} tokens held: {:?}",
                self.workers
            )));
        }
        if self.gate.held() as u64 > held {
            return Err(Broke::Oracle(format!(
                "the pool runs on {} tokens, the ledger counts {held} for it",
                self.gate.held()
            )));
        }
        let over = self.fake.0.lock().over_given;
        if over > 0 {
            return Err(Broke::Oracle(format!(
                "the pool gave back {over} more tokens than it held"
            )));
        }
        Ok(())
    }

    /// Every worker not parked steps until all are, within a bound: then nothing may be
    /// queued, one token is held, and the end gives every token back.
    fn drain(&mut self) -> Result<(), Broke> {
        let bound = 16 * (self.submitted + self.workers.len() as u64 + 4);
        for _ in 0..bound {
            if self.workers.iter().all(|it| *it == At::Parked) {
                break;
            }
            for at in 0..self.workers.len() {
                self.step(at);
                self.check()?;
            }
        }
        if !self.workers.iter().all(|it| *it == At::Parked) {
            return Err(Broke::Oracle(format!("the workers never all parked: {:?}", self.workers)));
        }
        if self.queued > 0 {
            return Err(Broke::Oracle(format!(
                "{} of {} jobs left queued with every worker parked: a lost wake",
                self.queued, self.submitted
            )));
        }
        assert_eq!(self.ran, self.submitted, "every job dequeued ran once");
        if self.fake.held() != 1 {
            return Err(Broke::Oracle(format!(
                "an idle pool holds {} tokens, not one",
                self.fake.held()
            )));
        }
        self.gate.give_all();
        if self.fake.held() != 0 {
            return Err(Broke::Oracle(format!(
                "an ended pool still holds {} tokens",
                self.fake.held()
            )));
        }
        self.check()
    }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), Broke> {
    let mut world = World::new(plan, plant);
    world.check()?;
    for &act in &plan.acts {
        world.act(act)?;
    }
    world.drain()
}

// --- The tests ----------------------------------------------------------------------------------

/// The invariant: whatever the order of the workers' steps, the submits and the ledger's
/// moves, the workers running on a pool's permits never pass the tokens the ledger counts for
/// it, and it never gives back more than it holds; every job queued runs (no wake is lost as
/// tokens shrink to one and grow back); an idle pool keeps one token, and an ended one none.
#[hegel::test(test_cases = 300)]
fn a_pool_runs_no_more_workers_than_its_tokens_and_loses_no_job(tc: TestCase) {
    let plan = plan(&mut Drawn(&tc));
    if let Err(broke) = run(&plan, Plant::None) {
        panic!("{broke}\nplan {plan:?}");
    }
}

/// The same over seeded plans, so the rule is run where Hegel's server is not.
#[test]
fn the_rule_holds_over_seeded_plans() {
    for seed in 0..CONTROL_SEEDS {
        if let Err(broke) = run(&plan_of(seed), Plant::None) {
            panic!("seed {seed} (REPLAY = ({seed}, Plant::None)): {broke}");
        }
    }
}

/// A plant the oracle must catch in one of [`CONTROL_SEEDS`] seeded plans (test-the-cause on
/// the test itself): a green DST with a blind oracle proves nothing.
fn control(plant: Plant) {
    for seed in 0..CONTROL_SEEDS {
        if let Err(Broke::Oracle(why)) = run(&plan_of(seed), plant) {
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

/// The control: the pool before this module, every worker running whatever it holds.
#[test]
fn the_oracle_catches_a_pool_that_ignores_its_tokens() {
    control(Plant::IgnoresTokens);
}

#[test]
fn the_oracle_catches_a_worker_with_no_permit_that_parks_without_its_last_look() {
    control(Plant::NoLastLook);
}

/// One seeded plan, [`REPLAY`]'s: set it to a seed a failure names to run that case alone.
#[test]
fn replay_one_seed() {
    let (seed, plant) = REPLAY;
    if let Err(broke) = run(&plan_of(seed), plant) {
        panic!("seed {seed}, Plant::{plant:?}: {broke}");
    }
}

// --- The pool itself ----------------------------------------------------------------------------

/// Runs `phases` (the cores that fit, then that many jobs) on a pool of `workers` threads on a
/// fake ledger: the most jobs that ran at once past the tokens held (0 when never), after
/// checking every job ran, the idle pool kept one token and the dropped one none.
fn on_a_pool(
    workers: usize,
    phases: &[(u64, u64)],
    plant: bool,
    job: impl Fn(&AtomicU64) + Sync,
) -> u64 {
    let fake = Fake::leaked(phases.first().map_or(1, |it| it.0));
    let pool = Pool::with_tokens(&Topology::flat(workers), fake).unwrap();
    if let Some(gate) = &pool.shared.gate {
        gate.plant.store(plant, Ordering::Relaxed);
    }
    let (running, over, ran) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    let mut submitted = 0;
    for &(cap, jobs) in phases {
        fake.set_cap(cap);
        pool.scope(|scope| {
            for _ in 0..jobs {
                scope.submit(
                    None,
                    || {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        over.fetch_max(now.saturating_sub(fake.held()), Ordering::SeqCst);
                        job(&running);
                        running.fetch_sub(1, Ordering::SeqCst);
                        ran.fetch_add(1, Ordering::SeqCst);
                    },
                    |it| it.unwrap(),
                );
            }
        });
        submitted += jobs;
        assert_eq!(ran.load(Ordering::SeqCst), submitted, "a scope returned before its jobs ran");
        crate::spin::SpinWait::within(
            "the pool to go idle",
            std::time::Duration::from_secs(30),
            || pool.idle(),
        );
        if !plant {
            assert_eq!(fake.held(), 1, "an idle pool keeps one token");
        }
    }
    drop(pool);
    assert_eq!(fake.held(), 0, "a dropped pool gives every token back");
    over.load(Ordering::SeqCst)
}

/// The invariant: on a pool with threads, whatever the ledger's cores do between bursts of
/// jobs (shrinking to one, growing back), the jobs running at once never pass the tokens it
/// holds, every job runs, an idle pool keeps one token and a dropped one none.
#[hegel::test(test_cases = 24)]
fn a_pool_with_threads_runs_no_more_jobs_at_once_than_it_holds_tokens(tc: TestCase) {
    let workers = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
    let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
    let phases: Vec<(u64, u64)> = (0..count)
        .map(|_| {
            (
                tc.draw(gs::integers::<u64>().min_value(1).max_value(workers as u64)),
                tc.draw(gs::integers::<u64>().min_value(1).max_value(48)),
            )
        })
        .collect();
    let over = on_a_pool(workers, &phases, false, |_| {
        crate::spin::busy(std::time::Duration::from_micros(50));
    });
    assert_eq!(
        over, 0,
        "{workers} workers, phases {phases:?}: jobs ran past the tokens held by {over}"
    );
}

/// The control: the same pool ignoring its tokens, one token and four workers, each job held
/// until another runs beside it (or a second), runs past its tokens.
#[test]
fn the_oracle_catches_a_pool_with_threads_that_ignores_its_tokens() {
    let over = on_a_pool(4, &[(1, 4)], true, |running| {
        crate::spin::SpinWait::within(
            "a second job beside this one, on a pool that ignores its tokens",
            std::time::Duration::from_secs(10),
            || running.load(Ordering::SeqCst) >= 2,
        );
    });
    assert!(over > 0, "a pool that ignores its one token never ran two jobs at once");
}
