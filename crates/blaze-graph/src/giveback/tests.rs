//! The core's give-back rule's DST (`testing` skill, dst-not-review), and a few cases of
//! [`Planner`] alone, the first of them the incident of 2026-11-12 on the core's count.
//!
//! A real [`Planner`] is stepped once a tick against a simulated core under a drawn schedule
//! of growth of what the builds under way hold there (their memos, which no rung gives) and of
//! builds. Core holds those, what the allocator keeps freed (returned by a purge, which lands
//! a drawn lag of ticks after it is asked), a part that never goes back, a cache, and each
//! checkout's front end: its values, given in grains on a worker (done a drawn lag of ticks
//! later), and its core, given whole. Whatever is given is freed into what the allocator
//! keeps, as in the daemon, so the count falls only at the purge after. The count the planner
//! is told is exact, as the gate's is.
//!
//! After every step the oracle checks:
//! - cheapest first: a value or a front end is given only when a purge alone would not have
//!   covered what is past the line and the cache would not have, or each of those was asked
//!   within [`RETRY_NS`]; a front end only when no values are left;
//! - in proportion: a rung's gives stop once they cover the target, and a give of values is
//!   what is still short rounded to its grain, never more;
//! - the floor (the checkout built last) is never given whole;
//! - no thrash: a front end made again is not given again within its hold;
//! - measured: no rung gives while the last rung's gives are not done (for the allocator's, a
//!   purge served after them) and within [`MEASURE_WITHIN_NS`].
//!
//! Five plants ([`Plant`]) prove the oracle catches what it claims to, each in a control; the
//! first is the rule before the ladder (every front end, whole, past the line). Replay: a
//! control or [`replay_one_seed`] names a seed of [`plan_of`]; the Hegel test prints its plan.

use std::fmt;

use blaze_rng::SplitMix64;
use hegel::{TestCase, generators as gs};

use super::*;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// The simulated time of one tick: 50 ms, so a cheap source asked is asked again after 20.
const TICK_NS: u64 = 50_000_000;

/// Most acts a plan draws.
const MOST_ACTS: u64 = 160;

/// The grain the simulated values are given in.
const GRAIN: u64 = 16 * MIB;

/// Seeds of [`plan_of`] a control searches for one its plant breaks.
const CONTROL_SEEDS: u64 = 512;

/// The seed [`replay_one_seed`] runs, and its plant: set them to what a failure names.
const REPLAY: (u64, Plant) = (0, Plant::None);

/// A source of draws: Hegel's (shrunk, replayed by Hegel) or a seed's ([`plan_of`]).
trait Draw {
    /// In `lo..=hi`.
    fn int(&mut self, lo: u64, hi: u64) -> u64;

    fn one_in(&mut self, n: u64) -> bool {
        self.int(0, n - 1) == 0
    }
}

struct Drawn<'a>(&'a TestCase);

impl Draw for Drawn<'_> {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        self.0.draw(gs::integers::<u64>().min_value(lo).max_value(hi))
    }
}

impl Draw for SplitMix64 {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        match (hi - lo).checked_add(1) {
            Some(n) => lo + self.below(n),
            None => self.next_u64(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Act {
    /// What the builds under way hold in core becomes this many hundredths of the core.
    Base(u64),
    /// A build of this checkout: it makes its front end again if it was given, and is the
    /// floor from then on.
    Build(usize),
    /// Nothing but ticks.
    Idle,
}

#[derive(Clone, Copy, Debug)]
struct FrontPlan {
    core: u64,
    values: u64,
    remake_ns: u64,
}

#[derive(Clone, Debug)]
struct Plan {
    /// The core's size.
    core: u64,
    /// What the allocator keeps that no purge returns.
    stuck: u64,
    caches: u64,
    /// What a build frees into the allocator.
    churn: u64,
    fronts: Vec<FrontPlan>,
    /// Ticks from a purge's ask to its landing, and from a give of values to its return.
    purge_lag: u64,
    values_lag: u64,
    /// Each act, and the ticks that follow it.
    acts: Vec<(Act, u64)>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let core = d.int(8, 64) * GIB;
    let count = d.int(1, 3) as usize;
    // alloc: a plan, once a case
    let fronts: Vec<FrontPlan> = (0..count)
        .map(|_| FrontPlan {
            core: d.int(1, core / GIB / 4) * GIB,
            values: d.int(0, core / GIB / 8) * GIB,
            remake_ns: d.int(20, 200) * 1_000_000_000,
        })
        .collect();
    // Half the plans are squeezed: the builds under way hold most of the core, so front ends
    // are given, built again and asked again.
    let squeezed = d.one_in(2);
    let acts = (0..d.int(1, MOST_ACTS))
        .map(|_| {
            let act = match d.int(0, 9) {
                0..=3 if squeezed => Act::Base(d.int(40, 95)),
                0..=3 => Act::Base(d.int(5, 80)),
                4..=7 => Act::Build(d.int(0, count as u64 - 1) as usize),
                _ => Act::Idle,
            };
            (act, d.int(1, 16))
        })
        .collect();
    Plan {
        core,
        stuck: d.int(0, 4) * 64 * MIB,
        caches: d.int(0, 8) * 64 * MIB,
        churn: d.int(0, 64) * 64 * MIB,
        fronts,
        purge_lag: d.int(1, 10),
        values_lag: d.int(1, 10),
        acts,
    }
}

fn plan_of(seed: u64) -> Plan {
    plan(&mut SplitMix64::new(seed))
}

#[derive(Clone, Copy, Debug)]
struct Front {
    core: u64,
    values: u64,
}

/// The last rung that gave, for the measured rule.
#[derive(Clone, Copy, Debug)]
struct Gave {
    rung: Rung,
    purged: u64,
    at_ns: u64,
}

struct World<'a> {
    plan: &'a Plan,
    base: u64,
    /// What the allocator keeps freed that a purge returns.
    kept: u64,
    caches: u64,
    fronts: Vec<Front>,
    /// The checkout built last: the floor.
    last_built: usize,
    tick: u64,
    now_ns: u64,
    purged: u64,
    purge_lands: Option<u64>,
    /// The tick a give of values out on a worker is back, and the bytes it frees then.
    values_back: Option<(u64, Vec<(usize, u64)>)>,
    // What the oracle remembers.
    gave: Option<Gave>,
    /// The step's gives are done (`Planner::done` told).
    done: bool,
    /// When the allocator and the cache were last asked, by [`Rung::index`].
    cheap_asked: [Option<u64>; 2],
    hold_until: Vec<u64>,
    sources: Vec<Source>,
    out: Vec<Give>,
}

enum Broke {
    /// The oracle's: a rule of the module's list was broken.
    Oracle(String),
    /// The simulation's own: the planner asked what the core could not do.
    Harness(String),
}

impl fmt::Display for Broke {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Broke::Oracle(why) => write!(f, "the oracle: {why}"),
            Broke::Harness(why) => write!(f, "the simulation: {why}"),
        }
    }
}

impl<'a> World<'a> {
    fn new(plan: &'a Plan) -> World<'a> {
        World {
            plan,
            base: 0,
            kept: 0,
            caches: plan.caches,
            // alloc: a case's checkouts
            fronts: plan.fronts.iter().map(|it| Front { core: it.core, values: it.values }).collect(),
            last_built: 0,
            tick: 0,
            now_ns: 0,
            purged: 0,
            purge_lands: None,
            values_back: None,
            gave: None,
            done: true,
            cheap_asked: [None; 2],
            hold_until: vec![0; plan.fronts.len()], // alloc: a case's checkouts
            sources: Vec::new(),                    // alloc: a case's scratch
            out: Vec::new(),                        // alloc: a case's scratch
        }
    }

    /// What core holds: exact, as the gate counts it.
    fn used(&self) -> u64 {
        self.base
            + self.kept
            + self.plan.stuck
            + self.caches
            + self.fronts.iter().map(|it| it.core + it.values).sum::<u64>()
    }

    fn act(&mut self, planner: &mut Planner, act: Act) {
        match act {
            Act::Base(hundredths) => {
                let base = self.plan.core / 100 * hundredths;
                // What the builds under way let go of is freed into the allocator.
                self.kept += self.base.saturating_sub(base);
                self.base = base;
            }
            Act::Build(at) => {
                let made = self.plan.fronts[at];
                if self.fronts[at].core == 0 {
                    self.now_ns += made.remake_ns;
                    self.fronts[at] = Front { core: made.core, values: made.values };
                    planner.remade(Rung::FrontEnd, at as u32, self.now_ns, made.remake_ns);
                    self.hold_until[at] = self.now_ns + hold_ns(made.remake_ns);
                } else {
                    let values = self.fronts[at].values + made.values / 4;
                    self.fronts[at].values = values.min(made.values);
                }
                self.last_built = at;
                self.kept += self.plan.churn;
                self.caches = self.plan.caches;
            }
            Act::Idle => {}
        }
    }

    fn list_sources(&mut self) {
        self.sources.clear();
        let kept = self.kept + self.plan.stuck;
        let one = |rung, owner, gives| Source { rung, owner, gives, grain: gives, remake_ns: 0, floor: false };
        self.sources.push(one(Rung::Allocator, 0, kept));
        self.sources.push(one(Rung::Caches, 0, self.caches));
        for (at, front) in self.fronts.iter().enumerate() {
            let made = self.plan.fronts[at];
            let whole = front.core + front.values;
            // A value costs a hundredth of its front end's remake a byte of the whole.
            let values_ns = if whole == 0 {
                0
            } else {
                (u128::from(made.remake_ns) * u128::from(front.values) / u128::from(whole) / 100) as u64
            };
            self.sources.push(Source {
                rung: Rung::Values,
                owner: at as u32,
                gives: front.values,
                grain: GRAIN,
                remake_ns: values_ns,
                floor: false,
            });
            self.sources.push(Source {
                rung: Rung::FrontEnd,
                owner: at as u32,
                gives: if front.core == 0 { 0 } else { whole },
                grain: whole,
                remake_ns: made.remake_ns,
                floor: at == self.last_built,
            });
        }
    }

    /// One tick: a purge lands or a give of values is back, the planner steps, the oracle
    /// looks, and the gives are done.
    fn tick(&mut self, planner: &mut Planner) -> Result<(), Broke> {
        self.tick += 1;
        self.now_ns += TICK_NS;
        let tick = self.tick;
        if self.purge_lands.is_some_and(|at| tick >= at) {
            self.purge_lands = None;
            self.kept = 0;
            self.purged += 1;
        }
        if self.values_back.as_ref().is_some_and(|(at, _)| tick >= *at) {
            let (_, gives) = self.values_back.take().expect("checked above");
            for (at, bytes) in gives {
                let went = bytes.min(self.fronts[at].values);
                self.fronts[at].values -= went;
                self.kept += went;
            }
            self.done = true;
            planner.done();
        }
        self.list_sources();
        let now = Now { at_ns: self.now_ns, core: self.plan.core, used: self.used(), purged: self.purged };
        let mut out = std::mem::take(&mut self.out);
        out.clear();
        let outcome = planner.step(&now, &self.sources, &mut out);
        let looked = self.look(&now, outcome, &out).map_err(Broke::Oracle);
        let done = looked.and_then(|()| self.give(planner, &out));
        self.out = out;
        done
    }

    /// The oracle (the module's doc).
    fn look(&mut self, now: &Now, outcome: Outcome, out: &[Give]) -> Result<(), String> {
        let rung = match outcome {
            Outcome::Gave { rung } => rung,
            _ if out.is_empty() => return Ok(()),
            other => return Err(format!("{other:?} came with gives: {out:?}")),
        };
        if out.is_empty() || out.iter().any(|it| it.rung != rung) {
            return Err(format!("a step that gave of {rung:?} listed {out:?}"));
        }
        let need = now.over();
        if need == 0 {
            return Err(format!("{rung:?} gave with core under its line: {out:?}"));
        }
        let target = target(need);
        let sum: u64 = out.iter().map(|it| it.bytes).sum();
        // Measured: the last rung's gives are done, or the wait for them ran out.
        if let Some(last) = self.gave {
            let measured = match last.rung {
                Rung::Allocator => now.purged > last.purged,
                _ => self.done,
            };
            if !measured && now.at_ns < last.at_ns + MEASURE_WITHIN_NS {
                return Err(format!(
                    "{rung:?} gave {sum} bytes while what {:?} gave is not done",
                    last.rung
                ));
            }
        }
        self.gave = Some(Gave { rung, purged: now.purged, at_ns: now.at_ns });
        // In proportion: no give past the target, and values in their grain.
        if rung != Rung::Allocator && sum - out[out.len() - 1].bytes >= target {
            return Err(format!("{rung:?} went on giving past the target of {target} bytes: {out:?}"));
        }
        if rung == Rung::Values {
            let mut before = 0u64;
            for give in out {
                let most = (target - before).div_ceil(GRAIN) * GRAIN;
                if give.bytes > most {
                    return Err(format!(
                        "values of checkout {} gave {} bytes where {} were still short of the target of {target}",
                        give.owner,
                        give.bytes,
                        target - before
                    ));
                }
                before += give.bytes;
            }
        }
        if !rung.valued() {
            return Ok(());
        }
        // Cheapest first: each cheap rung that could have covered what is past the line was
        // asked within its retry.
        let lately = |rung: Rung| self.cheap_asked[rung.index()].is_some_and(|at| now.at_ns < at + RETRY_NS);
        if self.kept >= need && !lately(Rung::Allocator) {
            return Err(format!(
                "{rung:?} gave {sum} bytes for {need} past the line while a purge would return {}",
                self.kept
            ));
        }
        if self.caches >= need && !lately(Rung::Caches) {
            return Err(format!(
                "{rung:?} gave {sum} bytes for {need} past the line while the cache holds {}",
                self.caches
            ));
        }
        if rung != Rung::FrontEnd {
            return Ok(());
        }
        let values: u64 = self.fronts.iter().map(|it| it.values).sum();
        if values != 0 {
            return Err(format!(
                "a front end was given for {need} past the line while {values} bytes of values are left"
            ));
        }
        for give in out {
            let at = give.owner as usize;
            if at == self.last_built {
                return Err(format!("the floor, checkout {at}, was given whole for {need} past the line"));
            }
            if now.at_ns < self.hold_until[at] {
                return Err(format!(
                    "checkout {at}'s front end was given again {} ns before its hold ends: a thrash",
                    self.hold_until[at] - now.at_ns
                ));
            }
        }
        Ok(())
    }

    fn give(&mut self, planner: &mut Planner, out: &[Give]) -> Result<(), Broke> {
        if out.is_empty() {
            return Ok(());
        }
        let mut on_worker = Vec::new(); // alloc: a step's gives of values
        for give in out {
            let at = give.owner as usize;
            let has = match give.rung {
                Rung::Allocator => self.kept + self.plan.stuck,
                Rung::Caches => self.caches,
                Rung::Values => self.fronts[at].values,
                Rung::FrontEnd => self.fronts[at].core + self.fronts[at].values,
            };
            if give.bytes > has || give.bytes == 0 {
                return Err(Broke::Harness(format!("{give:?} asks more than the {has} bytes there, or nothing")));
            }
            if !give.rung.valued() {
                self.cheap_asked[give.rung.index()] = Some(self.now_ns);
            }
            match give.rung {
                Rung::Allocator => {
                    if self.purge_lands.is_none() {
                        self.purge_lands = Some(self.tick + self.plan.purge_lag);
                    }
                }
                Rung::Caches => {
                    self.kept += self.caches;
                    self.caches = 0;
                }
                Rung::Values => on_worker.push((at, give.bytes)),
                Rung::FrontEnd => {
                    self.kept += has;
                    self.fronts[at] = Front { core: 0, values: 0 };
                }
            }
        }
        // As the daemon's driver does: a give on the loop is done as it returns, a give of
        // values once its task is back.
        match on_worker.is_empty() {
            true => {
                self.done = true;
                planner.done();
            }
            false => {
                self.done = false;
                self.values_back = Some((self.tick + self.plan.values_lag, on_worker));
            }
        }
        Ok(())
    }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), Broke> {
    let mut planner = Planner::new();
    planner.plant = plant;
    let mut world = World::new(plan);
    for &(act, ticks) in &plan.acts {
        world.act(&mut planner, act);
        for _ in 0..ticks {
            world.tick(&mut planner)?;
        }
    }
    Ok(())
}

// --- The tests ------------------------------------------------------------------------------

/// The invariant: under any schedule of growth of what builds hold in core and of builds,
/// nothing of value is given while a cheaper give-back would cover what is past the line; a
/// rung's gives stop at the target; the floor is never given whole; a front end made again is
/// not given again within its hold; and each rung's gives are done before the next.
#[hegel::test(test_cases = 300)]
fn nothing_of_value_is_given_while_a_cheaper_give_back_covers_the_core(tc: TestCase) {
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
        match run(&plan_of(seed), plant) {
            Err(Broke::Oracle(why)) => {
                eprintln!("{plant:?} caught at seed {seed} (REPLAY = ({seed}, Plant::{plant:?})): {why}");
                return;
            }
            Err(broke @ Broke::Harness(_)) => panic!("seed {seed}, {plant:?}: {broke}"),
            Ok(()) => {}
        }
    }
    panic!("the oracle caught Plant::{plant:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak");
}

/// The control: the rule before the ladder.
#[test]
fn the_oracle_catches_every_front_end_dropped_whole_past_the_line() {
    control(Plant::WholeFrontEnd);
}

#[test]
fn the_oracle_catches_a_rung_asked_before_the_last_is_done() {
    control(Plant::NoMeasure);
}

#[test]
fn the_oracle_catches_a_rung_that_gives_all_it_has() {
    control(Plant::TakeAll);
}

#[test]
fn the_oracle_catches_a_front_end_given_again_within_its_hold() {
    control(Plant::NoHold);
}

#[test]
fn the_oracle_catches_the_floor_given_whole() {
    control(Plant::NoFloor);
}

/// One seeded plan, [`REPLAY`]'s: set it to a seed a failure names to run that case alone.
#[test]
fn replay_one_seed() {
    let (seed, plant) = REPLAY;
    if let Err(broke) = run(&plan_of(seed), plant) {
        panic!("seed {seed}, Plant::{plant:?}: {broke}");
    }
}

// --- The planner alone ------------------------------------------------------------------------

fn source(rung: Rung, owner: u32, gives: u64) -> Source {
    Source { rung, owner, gives, grain: gives, remake_ns: 0, floor: false }
}

/// The core of 2026-11-12's incident, as the gate counts it: a core of 64 GiB (its line 56),
/// 56.09 GiB held of it, 20 GiB of that pages the allocator keeps freed, reth's front end the
/// only one kept.
fn incident() -> (Now, Vec<Source>) {
    let now = Now { at_ns: 1_000_000_000, core: 64 * GIB, used: 56 * GIB + 92 * MIB, purged: 2 };
    let sources = vec![
        Source { remake_ns: 195_400_000_000, floor: true, ..source(Rung::FrontEnd, 0, 40 * GIB) },
        Source { grain: GRAIN, remake_ns: 12_000_000_000, ..source(Rung::Values, 0, 6 * GIB) },
        source(Rung::Allocator, 0, 20 * GIB),
    ];
    (now, sources)
}

/// The invariant: a core just past its line, with more than that kept freed, is answered by a
/// purge alone; nothing else is given before the purge is served, and under the line nothing
/// is given at all. The front end is kept.
#[test]
fn a_core_just_past_its_line_costs_a_purge_and_the_front_end_is_kept() {
    let (now, sources) = incident();
    let mut planner = Planner::new();
    let mut out = Vec::new(); // alloc: a test's gives
    assert_eq!(planner.step(&now, &sources, &mut out), Outcome::Gave { rung: Rung::Allocator });
    assert_eq!(out.len(), 1);
    assert_eq!((out[0].rung, out[0].bytes, out[0].over), (Rung::Allocator, 20 * GIB, 92 * MIB));
    // No purge served yet: nothing more.
    out.clear();
    let later = Now { at_ns: now.at_ns + TICK_NS, ..now };
    assert_eq!(planner.step(&later, &sources, &mut out), Outcome::Measuring { rung: Rung::Allocator });
    assert!(out.is_empty());
    // The purge was served: core is under its line, nothing more is given.
    let after = Now { at_ns: now.at_ns + 6 * TICK_NS, used: 36 * GIB, purged: 3, ..now };
    assert_eq!(planner.step(&after, &sources[..2], &mut out), Outcome::Enough);
    assert!(out.is_empty());
    assert_eq!(planner.counts().gives, [1, 0, 0, 0]);
}

/// The control of the test above (test-the-cause): the rule before the ladder, on the same
/// count, gives the whole front end.
#[test]
fn the_rule_before_dropped_the_whole_front_end_for_the_same_core() {
    let (now, sources) = incident();
    let mut planner = Planner::new();
    planner.plant = Plant::WholeFrontEnd;
    let mut out = Vec::new(); // alloc: a test's gives
    assert_eq!(planner.step(&now, &sources, &mut out), Outcome::Gave { rung: Rung::FrontEnd });
    assert_eq!((out[0].rung, out[0].bytes), (Rung::FrontEnd, 40 * GIB));
}

/// The invariant: when the purge returned nothing the next rung gives what is past the line
/// and a margin, in its grain, never more; and its give is awaited until the driver says it
/// is done.
#[test]
fn values_go_in_proportion_and_are_awaited_until_done() {
    let (now, sources) = incident();
    let mut planner = Planner::new();
    let mut out = Vec::new(); // alloc: a test's gives
    assert_eq!(planner.step(&now, &sources, &mut out), Outcome::Gave { rung: Rung::Allocator });
    // The purge was served and returned nothing: the allocator's estimate stands.
    out.clear();
    let after = Now { at_ns: now.at_ns + TICK_NS, purged: 3, ..now };
    assert_eq!(planner.step(&after, &sources, &mut out), Outcome::Gave { rung: Rung::Values });
    // 92 MiB and the least margin, 64 MiB, in grains of 16 MiB.
    assert_eq!((out.len(), out[0].bytes), (1, 160 * MIB));
    assert_eq!(out[0].remake_ns, 12_000_000_000 * 160 / (6 * 1024));
    // Out on a worker: awaited until done, at most two seconds.
    out.clear();
    let waiting = Now { at_ns: after.at_ns + TICK_NS, ..after };
    assert_eq!(planner.step(&waiting, &sources, &mut out), Outcome::Measuring { rung: Rung::Values });
    planner.done();
    let back = Now { at_ns: waiting.at_ns + TICK_NS, used: 55 * GIB, ..waiting };
    assert_eq!(planner.step(&back, &sources, &mut out), Outcome::Enough);
    assert!(out.is_empty());
}

/// The invariant: past the line, a front end that is not the floor goes, one, the least
/// remake time a byte first; with the floor alone left the planner says so and gives nothing.
#[test]
fn past_the_line_one_front_end_goes_and_the_floor_is_said_not_given() {
    let mut sources = vec![
        Source { remake_ns: 195_000_000_000, floor: true, ..source(Rung::FrontEnd, 0, 30 * GIB) },
        Source { remake_ns: 100_000_000_000, ..source(Rung::FrontEnd, 1, 10 * GIB) },
        Source { remake_ns: 50_000_000_000, ..source(Rung::FrontEnd, 2, 10 * GIB) },
    ];
    // A core of 48 GiB: its line 42.
    let now = Now { at_ns: 1, core: 48 * GIB, used: 49 * GIB, purged: 0 };
    let mut planner = Planner::new();
    let mut out = Vec::new(); // alloc: a test's gives
    assert_eq!(planner.step(&now, &sources, &mut out), Outcome::Gave { rung: Rung::FrontEnd });
    assert_eq!(out.len(), 1, "7 GiB over and a margin: one front end of 10 covers it");
    assert_eq!((out[0].owner, out[0].over, out[0].again), (2, 7 * GIB, false));
    planner.done();
    // What builds hold grows again, under the floor itself.
    out.clear();
    sources[2].gives = 0;
    sources[1].gives = 0;
    let full = Now { at_ns: 2, used: 54 * GIB, ..now };
    assert_eq!(
        planner.step(&full, &sources, &mut out),
        Outcome::FloorOver { over: 12 * GIB, line: 42 * GIB, used: 54 * GIB, owner: 0, kept: 30 * GIB }
    );
    assert!(out.is_empty());
    assert_eq!(planner.counts().floor_over, 1);
    // With no front end kept at all, nothing is left: the engine's memos of the builds under
    // way hold it, which the gate's refusal names.
    assert_eq!(planner.step(&full, &[], &mut out), Outcome::Nothing { over: 12 * GIB });
}

/// The invariant: a front end given and made again is not given again until its hold ends
/// (sixteen times what the remake took); a give of it after that is counted a thrash and
/// marked so.
#[test]
fn a_front_end_made_again_is_held_and_a_later_give_of_it_is_a_thrash() {
    let sources = [
        Source { remake_ns: 100_000_000_000, floor: true, ..source(Rung::FrontEnd, 0, 30 * GIB) },
        Source { remake_ns: 100_000_000_000, ..source(Rung::FrontEnd, 1, 10 * GIB) },
    ];
    // A core of 40 GiB: its line 35.
    let now = Now { at_ns: 1, core: 40 * GIB, used: 42 * GIB, purged: 0 };
    let mut planner = Planner::new();
    let mut out = Vec::new(); // alloc: a test's gives
    assert_eq!(planner.step(&now, &sources, &mut out), Outcome::Gave { rung: Rung::FrontEnd });
    assert_eq!(out[0].owner, 1);
    planner.done();
    // Its checkout builds: 100 s to lower anew, so it is held 1,600 s.
    let remade = 200_000_000_000;
    planner.remade(Rung::FrontEnd, 1, remade, 100_000_000_000);
    assert!(planner.held(Rung::FrontEnd, 1, remade + 1_599_000_000_000));
    assert!(!planner.held(Rung::FrontEnd, 1, remade + 1_600_000_000_000));
    out.clear();
    let again = Now { at_ns: remade + 1_000_000_000, ..now };
    assert!(matches!(planner.step(&again, &sources, &mut out), Outcome::FloorOver { .. }));
    assert!(out.is_empty());
    assert_eq!((planner.counts().held, planner.counts().thrash), (1, 0));
    // Past the hold it may go again, and that is said.
    let past = Now { at_ns: remade + 1_600_000_000_000, ..now };
    assert_eq!(planner.step(&past, &sources, &mut out), Outcome::Gave { rung: Rung::FrontEnd });
    assert!(out[0].again);
    assert_eq!(planner.counts().thrash, 1);
}

/// The invariant: a wait for a purge ends at [`MEASURE_WITHIN_NS`] (bounded-progress): the
/// planner goes on with the count it has, and counts it.
#[test]
fn a_purge_that_is_never_served_holds_the_next_rung_two_seconds_at_most() {
    let sources = [source(Rung::Allocator, 0, GIB), source(Rung::Caches, 0, 2 * GIB)];
    // A core of 16 GiB: its line 14.
    let now = Now { at_ns: 0, core: 16 * GIB, used: 15 * GIB + 1, purged: 0 };
    let mut planner = Planner::new();
    let mut out = Vec::new(); // alloc: a test's gives
    assert_eq!(planner.step(&now, &sources, &mut out), Outcome::Gave { rung: Rung::Allocator });
    out.clear();
    let waiting = Now { at_ns: MEASURE_WITHIN_NS - 1, ..now };
    assert_eq!(planner.step(&waiting, &sources, &mut out), Outcome::Measuring { rung: Rung::Allocator });
    let late = Now { at_ns: MEASURE_WITHIN_NS, ..now };
    assert_eq!(planner.step(&late, &sources, &mut out), Outcome::Gave { rung: Rung::Caches });
    assert_eq!(planner.counts().unmeasured, 1);
}

/// What the user reads, to the letter.
#[test]
fn the_notes_name_the_project_and_the_numbers() {
    let give = Give {
        rung: Rung::FrontEnd,
        owner: 0,
        bytes: 12_000_000_000,
        remake_ns: 195_400_000_000,
        over: 9_000_000_000,
        line: 40_000_000_000,
        used: 49_000_000_000,
        again: false,
        by: Planned(()),
    };
    assert_eq!(
        dropped_line("reth", &give),
        "analysis of reth was dropped to free 12.0 GB (blaze's heap held 49.0 GB, past its line of 40.0 GB); the next build redoes it"
    );
    assert_eq!(
        floor_line("reth", 9_000_000_000, 40_000_000_000, 49_000_000_000, 30_000_000_000),
        "memory: blaze's heap holds 49.0 GB, 9.0 GB past its line of 40.0 GB, and what is left to give is the analysis of reth (30.0 GB), which the next edit needs: it is kept; a build that needs more fails, naming what it could not hold; raise the memory ceiling"
    );
}
