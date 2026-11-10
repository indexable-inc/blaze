//! The give-back handshake's DST (`testing` skill, dst-not-review).
//!
//! Actors share one [`GiveBacks`]: up to four pool workers, which join, look at the generation
//! asked (a give-back begins), give back (it ends and acks what it looked at) and leave, in
//! any order; a purge, which asks and then checks whether it may read the footprint; and
//! another asker (a release's give-back) that asks between. Each operation is one atomic
//! read-modify-write of the word, so interleaving them one per step is every interleaving
//! there is. Hegel draws the steps; a seed of [`plan_of`] replays one.
//!
//! The oracle, at every check of the purge:
//! - `Settled { generation }` is the generation asked now, at or after the purge's own;
//! - every thread counted then has given back, beginning after `generation` was asked: what a
//!   purge reads after it holds none of the pages a counted thread kept before the ask.
//!
//! And at the end, once every counted thread gives back after the last ask, the purge settles
//! (bounded-progress: the acks of a whole round always reach the count).
//!
//! Two plants ([`Plant`]) prove the oracle catches what it claims to, each in a control.

use blaze_rng::SplitMix64;
use hegel::{TestCase, generators as gs};

use super::{GiveBacks, Plant, Settle};

/// Most workers a plan has, and most steps.
const MOST_WORKERS: u64 = 4;
const MOST_STEPS: u64 = 48;

/// Seeds of [`plan_of`] a control searches for one its plant breaks.
const CONTROL_SEEDS: u64 = 512;

#[derive(Clone, Copy, Debug)]
enum Op {
    Join(usize),
    Look(usize),
    Give(usize),
    Leave(usize),
    /// The purge asks.
    Ask,
    /// Another give-back asks (a release, the pressure watch).
    AskOther,
    /// The purge checks whether it may read.
    Check,
}

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

#[derive(Debug)]
struct Plan {
    workers: usize,
    ops: Vec<Op>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let workers = d.int(1, MOST_WORKERS) as usize;
    let steps = d.int(1, MOST_STEPS);
    // alloc: a plan, once a case
    let ops = (0..steps)
        .map(|_| {
            let w = d.int(0, workers as u64 - 1) as usize;
            match d.int(0, 9) {
                0 => Op::Join(w),
                1 | 2 => Op::Look(w),
                3 | 4 => Op::Give(w),
                5 => Op::Leave(w),
                6 => Op::Ask,
                7 => Op::AskOther,
                _ => Op::Check,
            }
        })
        .collect();
    Plan { workers, ops }
}

fn plan_of(seed: u64) -> Plan {
    plan(&mut SplitMix64::new(seed))
}

/// A worker as the model sees it.
#[derive(Clone, Copy, Debug, Default)]
struct Worker {
    joined: bool,
    /// The generation it looked at as its give-back began, while one is under way.
    looking: Option<u64>,
    /// The generation its last counted give-back acked: what it leaves with.
    acked: Option<u64>,
    /// The generation it looked at as its last finished give-back began, since it joined.
    collected: Option<u64>,
}

struct Sim {
    give_backs: GiveBacks,
    workers: Vec<Worker>,
    /// The generation the purge asked.
    asked: Option<u64>,
}

impl Sim {
    fn new(workers: usize, plant: Plant) -> Sim {
        Sim {
            give_backs: GiveBacks::planted(plant),
            workers: vec![Worker::default(); workers], // alloc: a case's workers
            asked: None,
        }
    }

    fn apply(&mut self, op: Op) -> Result<(), String> {
        match op {
            Op::Join(w) if !self.workers[w].joined => {
                self.give_backs.join();
                // A thread that joins anew: its own pages, nothing collected yet.
                self.workers[w] = Worker { joined: true, ..Worker::default() };
            }
            Op::Look(w) if self.workers[w].joined && self.workers[w].looking.is_none() => {
                self.workers[w].looking = Some(self.give_backs.asked());
            }
            Op::Give(w) if self.workers[w].joined => {
                if let Some(looked) = self.workers[w].looking.take() {
                    self.workers[w].collected = Some(looked);
                    // As `heap_gave_back` does: a thread acks a generation once.
                    if self.workers[w].acked != Some(looked) && self.give_backs.gave(looked) {
                        self.workers[w].acked = Some(looked);
                    }
                }
            }
            Op::Leave(w) if self.workers[w].joined => {
                self.give_backs.leave(self.workers[w].acked);
                self.workers[w] = Worker::default();
            }
            Op::Ask => self.asked = Some(self.give_backs.ask()),
            Op::AskOther => _ = self.give_backs.ask(),
            Op::Check => return self.check(),
            _ => {}
        }
        Ok(())
    }

    /// The oracle at a check of the purge (the module's doc).
    fn check(&self) -> Result<(), String> {
        let Some(asked) = self.asked else { return Ok(()) };
        let Settle::Settled { generation } = self.give_backs.settled() else { return Ok(()) };
        if generation < asked || generation != self.give_backs.asked() {
            return Err(format!(
                "settled for generation {generation}, the purge asked {asked} and {} is asked now",
                self.give_backs.asked()
            ));
        }
        for (at, worker) in self.workers.iter().enumerate() {
            if worker.joined && worker.collected.is_none_or(|it| it < generation) {
                return Err(format!(
                    "settled for generation {generation} while counted worker {at} gave back last \
                     for {:?}: the purge would read the footprint with its kept pages in it \
                     ({:?})",
                    worker.collected, self.workers
                ));
            }
        }
        Ok(())
    }

    /// Every counted worker finishes what it began, then gives back after the last ask: the
    /// purge must settle.
    fn settle_all(&mut self) -> Result<(), String> {
        for w in 0..self.workers.len() {
            self.apply(Op::Give(w))?;
            self.apply(Op::Look(w))?;
            self.apply(Op::Give(w))?;
        }
        self.check()?;
        match self.give_backs.settled() {
            Settle::Settled { .. } => Ok(()),
            Settle::Pending { gave, of } => Err(format!(
                "every counted worker gave back after the last ask, yet {gave} of {of} acked: {:?}",
                self.workers
            )),
        }
    }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let mut sim = Sim::new(plan.workers, plant);
    for (at, &op) in plan.ops.iter().enumerate() {
        sim.apply(op).map_err(|why| format!("at op {at} ({op:?}): {why}"))?;
    }
    sim.settle_all()
}

/// The invariant: when a purge may read, the generation settled is the one asked now, at or
/// after the purge's own, and every thread counted then has given back beginning after it was
/// asked; and once every counted thread gives back after the last ask, the purge settles.
#[hegel::test(test_cases = 400)]
fn a_purge_reads_only_once_every_counted_thread_gave_back_after_its_ask(tc: TestCase) {
    let plan = plan(&mut Drawn(&tc));
    if let Err(why) = run(&plan, Plant::None) {
        panic!("{why}\nplan {plan:?}");
    }
}

/// A plant the oracle must catch in one of [`CONTROL_SEEDS`] seeded plans.
fn control(plant: Plant) {
    for seed in 0..CONTROL_SEEDS {
        if let Err(why) = run(&plan_of(seed), plant) {
            eprintln!("{plant:?} caught at seed {seed}: {why}");
            return;
        }
    }
    panic!(
        "the oracle caught Plant::{plant:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak"
    );
}

#[test]
fn the_oracle_catches_an_ack_for_a_generation_not_collected_for() {
    control(Plant::AckAnyGeneration);
}

#[test]
fn the_oracle_catches_a_leave_that_keeps_its_ack() {
    control(Plant::LeaveKeepsAck);
}

/// The two plants' smallest cases, written out.
#[test]
fn the_plants_smallest_cases() {
    let early =
        Plan { workers: 1, ops: vec![Op::Join(0), Op::Look(0), Op::Ask, Op::Give(0), Op::Check] };
    assert!(run(&early, Plant::None).is_ok());
    assert!(run(&early, Plant::AckAnyGeneration).is_err());
    let left = Plan {
        workers: 2,
        ops: vec![
            Op::Join(0),
            Op::Join(1),
            Op::Ask,
            Op::Look(0),
            Op::Give(0),
            Op::Leave(0),
            Op::Check,
        ],
    };
    assert!(run(&left, Plant::None).is_ok());
    assert!(run(&left, Plant::LeaveKeepsAck).is_err());
}
