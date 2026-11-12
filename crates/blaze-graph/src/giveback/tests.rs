//! The give-back rule's DST (docs/v2/plan/design/memory.md, "Giving memory back"; `testing`
//! skill, dst-not-review), and a few cases of [`Planner`] alone, the first of them the
//! incident the module's doc names.
//!
//! A real [`Planner`] is stepped once a tick against a simulated daemon under a drawn schedule
//! of limit changes, tasks that ask for memory and builds. The daemon's footprint is what its
//! tasks hold, what the allocator keeps freed (returned by a purge, which lands a drawn lag of
//! ticks after it is asked) and a part that never goes back, the store's pool, a cache, and
//! each checkout's front end: its values, given in grains, and its core, given whole. Whatever
//! is given but the pool's pages is freed into what the allocator keeps, as in the daemon, so
//! the footprint falls only at the purge after. The planner is told the last reading, never
//! the truth: a reading comes every few ticks, after a purge, and after a build.
//!
//! After every step the oracle checks:
//! - cheapest first: a value or a front end is given only when a purge alone would not have
//!   covered what is short, the pool is empty and the cache would not have covered it, or
//!   each of those was asked within the last quarter second (and so is in a reading, or was
//!   found to give nothing); a front end only when no values are left;
//! - in proportion: a give of values or a front end for a task's ask is at most [`FACTOR`]
//!   times the target, and a rung's gives stop once they cover the target;
//! - the floor (the checkout built last) is never given whole;
//! - no thrash: a front end made again is not given again within its hold;
//! - measured: no rung gives while the last rung's gives are unmeasured and within
//!   [`MEASURE_WITHIN_NS`].
//!
//! Five plants ([`Plant`]) prove the oracle catches what it claims to, each in a control; the
//! first is the rule before this module (every front end, whole, for any ask). Replay: a
//! control or [`replay_one_seed`] names a seed of [`plan_of`]; the Hegel test prints its plan.

use std::fmt;

use blaze_rng::SplitMix64;
use hegel::{TestCase, generators as gs};

use super::*;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// The simulated time of one tick: 50 ms, so a cheap source asked is asked again after 5.
const TICK_NS: u64 = 50_000_000;

/// Most acts a plan draws.
const MOST_ACTS: u64 = 160;

/// The ticks a task waits for memory before it leaves (it starts alone, in the daemon).
const WAIT_TICKS: u64 = 30;

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
    /// The limit becomes this many hundredths of what the daemon holds now: a neighbour took
    /// memory, or gave it back.
    Limit(u64),
    /// A task of this many bytes asks to start, and runs this many ticks once it does.
    Task(u64, u64),
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
    base: u64,
    /// What the allocator's kept estimate shows that no purge returns.
    stuck: u64,
    pool: u64,
    caches: u64,
    /// What a build frees into the allocator.
    churn: u64,
    fronts: Vec<FrontPlan>,
    /// Ticks from a purge's ask to its landing, and between readings.
    purge_lag: u64,
    read_every: u64,
    /// Each act, and the ticks that follow it.
    acts: Vec<(Act, u64)>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let count = d.int(1, 3) as usize;
    // alloc: a plan, once a case
    let fronts: Vec<FrontPlan> = (0..count)
        .map(|_| FrontPlan {
            core: d.int(2, 40) * GIB,
            values: d.int(0, 20) * GIB,
            remake_ns: d.int(20, 200) * 1_000_000_000,
        })
        .collect();
    // Half the plans are squeezed: every limit is under what is held, so front ends are
    // given, built again and asked again.
    let squeezed = d.one_in(2);
    let acts = (0..d.int(1, MOST_ACTS))
        .map(|_| {
            let act = match d.int(0, 9) {
                0..=2 if squeezed => Act::Limit(d.int(50, 95)),
                0..=2 => Act::Limit(d.int(60, 200)),
                3..=5 => Act::Task(d.int(1, 512) * 16 * MIB, d.int(1, 12)),
                6..=8 => Act::Build(d.int(0, count as u64 - 1) as usize),
                _ => Act::Idle,
            };
            (act, d.int(1, 16))
        })
        .collect();
    Plan {
        base: d.int(1, 8) * GIB,
        stuck: d.int(0, 4) * 256 * MIB,
        pool: d.int(0, 16) * 256 * MIB,
        caches: d.int(0, 8) * 256 * MIB,
        churn: d.int(0, 64) * 64 * MIB,
        fronts,
        purge_lag: d.int(1, 10),
        read_every: d.int(1, 6),
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
    reading: u64,
    purged: u64,
    at_ns: u64,
}

struct World<'a> {
    plan: &'a Plan,
    limit: u64,
    base: u64,
    /// What the allocator keeps freed that a purge returns.
    kept: u64,
    pool: u64,
    caches: u64,
    fronts: Vec<Front>,
    /// The checkout built last: the floor.
    last_built: usize,
    waiting: Option<(u64, u64, u64)>,
    running: Vec<(u64, u64)>,
    tick: u64,
    now_ns: u64,
    reading: u64,
    purged: u64,
    footprint_read: u64,
    purge_lands: Option<u64>,
    // What the oracle remembers.
    gave: Option<Gave>,
    /// When the allocator, the pool and the cache were last asked, by [`Rung::index`].
    cheap_asked: [Option<u64>; 3],
    hold_until: Vec<u64>,
    sources: Vec<Source>,
    out: Vec<Give>,
}

enum Broke {
    /// The oracle's: a rule of the module's list was broken.
    Oracle(String),
    /// The simulation's own: the planner asked what the daemon could not do.
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
        let mut world = World {
            plan,
            limit: u64::MAX,
            base: plan.base,
            kept: 0,
            pool: plan.pool,
            caches: plan.caches,
            // alloc: a case's checkouts
            fronts: plan.fronts.iter().map(|it| Front { core: it.core, values: it.values }).collect(),
            last_built: 0,
            waiting: None,
            running: Vec::new(), // alloc: a case's tasks
            tick: 0,
            now_ns: 0,
            reading: 0,
            purged: 0,
            footprint_read: 0,
            purge_lands: None,
            gave: None,
            cheap_asked: [None; 3],
            hold_until: vec![0; plan.fronts.len()], // alloc: a case's checkouts
            sources: Vec::new(),                    // alloc: a case's scratch
            out: Vec::new(),                        // alloc: a case's scratch
        };
        world.read(false);
        world
    }

    fn footprint(&self) -> u64 {
        self.base
            + self.kept
            + self.plan.stuck
            + self.pool
            + self.caches
            + self.fronts.iter().map(|it| it.core + it.values).sum::<u64>()
    }

    fn read(&mut self, purged: bool) {
        self.footprint_read = self.footprint();
        self.reading += 1;
        if purged {
            self.purged += 1;
        }
    }

    fn ask(&self) -> u64 {
        match self.waiting {
            Some((bytes, ..)) => (self.footprint_read.saturating_add(bytes))
                .saturating_sub(self.limit)
                .min(self.footprint_read),
            None => 0,
        }
    }

    fn act(&mut self, planner: &mut Planner, act: Act) {
        match act {
            Act::Limit(hundredths) => self.limit = self.footprint() / 100 * hundredths,
            Act::Task(bytes, ticks) => {
                if self.waiting.is_none() {
                    self.waiting = Some((bytes, self.tick, ticks));
                }
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
                self.pool = (self.pool + self.plan.pool / 4).min(self.plan.pool);
                // A purge under way lands within any build.
                let purged = self.purge_lands.take().is_some();
                if purged {
                    self.kept = 0;
                }
                self.read(purged);
            }
            Act::Idle => {}
        }
    }

    fn list_sources(&mut self) {
        self.sources.clear();
        let kept = self.kept + self.plan.stuck;
        let one = |rung, gives, grain| Source { rung, owner: 0, gives, grain, remake_ns: 0, floor: false };
        self.sources.push(one(Rung::Allocator, kept, kept));
        self.sources.push(one(Rung::Pool, self.pool, 1));
        self.sources.push(one(Rung::Caches, self.caches, self.caches));
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

    /// One tick: the tasks move, a purge lands or a reading comes, the planner steps, the
    /// oracle looks, and the gives are done.
    fn tick(&mut self, planner: &mut Planner) -> Result<(), Broke> {
        self.tick += 1;
        self.now_ns += TICK_NS;
        let tick = self.tick;
        let mut freed = 0;
        self.running.retain(|&(bytes, ends)| {
            if ends <= tick {
                freed += bytes;
            }
            ends > tick
        });
        self.base -= freed;
        self.kept += freed;
        if let Some((bytes, since, ticks)) = self.waiting {
            if self.footprint_read.saturating_add(bytes) <= self.limit {
                self.base += bytes;
                self.running.push((bytes, tick + ticks));
                self.waiting = None;
            } else if tick >= since + WAIT_TICKS {
                self.waiting = None;
            }
        }
        if self.purge_lands.is_some_and(|at| tick >= at) {
            self.purge_lands = None;
            self.kept = 0;
            self.read(true);
        } else if tick % self.plan.read_every == 0 {
            self.read(false);
        }
        self.list_sources();
        let now = Now {
            at_ns: self.now_ns,
            limit: self.limit,
            footprint: self.footprint_read,
            ask: self.ask(),
            reading: self.reading,
            purged: self.purged,
        };
        let mut out = std::mem::take(&mut self.out);
        out.clear();
        let outcome = planner.step(&now, &self.sources, &mut out);
        let looked = self.look(&now, outcome, &out).map_err(Broke::Oracle);
        let done = looked.and_then(|()| self.give(&out));
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
        let need = now.ask.max(now.footprint.saturating_sub(now.limit));
        let target = target(need);
        let sum: u64 = out.iter().map(|it| it.bytes).sum();
        // Measured: the last rung's gives are in a reading, or the wait for one ran out.
        if let Some(last) = self.gave {
            let measured = match last.rung {
                Rung::Allocator => now.purged > last.purged,
                _ => now.reading > last.reading,
            };
            if !measured && now.at_ns < last.at_ns + MEASURE_WITHIN_NS {
                return Err(format!(
                    "{rung:?} gave {sum} bytes while what {:?} gave is not measured",
                    last.rung
                ));
            }
        }
        self.gave = Some(Gave { rung, reading: now.reading, purged: now.purged, at_ns: now.at_ns });
        // In proportion.
        if rung != Rung::Allocator && sum - out[out.len() - 1].bytes >= target {
            return Err(format!(
                "{rung:?} went on giving past the target of {target} bytes: {out:?}"
            ));
        }
        if rung.valued() && matches!(out[0].why, Why::Ask(_)) && sum > target.saturating_mul(FACTOR) {
            return Err(format!(
                "{rung:?} gave {sum} bytes for an ask of {need}: more than {FACTOR} times the target of {target}"
            ));
        }
        if !rung.valued() {
            return Ok(());
        }
        // Cheapest first: each cheap rung that could have covered what is short was asked
        // within the last quarter second.
        let lately = |rung: Rung| {
            self.cheap_asked[rung.index()].is_some_and(|at| now.at_ns < at + PURGE_EVERY_NS)
        };
        if self.kept >= need && !lately(Rung::Allocator) {
            return Err(format!(
                "{rung:?} gave {sum} bytes for {need} short while a purge would return {}",
                self.kept
            ));
        }
        if (self.pool != 0 && !lately(Rung::Pool)) || (self.caches >= need && !lately(Rung::Caches)) {
            return Err(format!(
                "{rung:?} gave {sum} bytes for {need} short while the pool holds {} and the cache {}",
                self.pool, self.caches
            ));
        }
        if rung != Rung::FrontEnd {
            return Ok(());
        }
        let values: u64 = self.fronts.iter().map(|it| it.values).sum();
        if values != 0 {
            return Err(format!(
                "a front end was given for {need} short while {values} bytes of values are left"
            ));
        }
        for give in out {
            let at = give.owner as usize;
            if at == self.last_built {
                return Err(format!("the floor, checkout {at}, was given whole for {need} short"));
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

    fn give(&mut self, out: &[Give]) -> Result<(), Broke> {
        for give in out {
            let at = give.owner as usize;
            let has = match give.rung {
                Rung::Allocator => self.kept + self.plan.stuck,
                Rung::Pool => self.pool,
                Rung::Caches => self.caches,
                Rung::Values => self.fronts[at].values,
                Rung::FrontEnd => self.fronts[at].core + self.fronts[at].values,
                // The simulation lists no running build.
                Rung::Running => 0,
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
                Rung::Pool => self.pool -= give.bytes,
                Rung::Caches => {
                    self.kept += self.caches;
                    self.caches = 0;
                }
                Rung::Values => {
                    self.fronts[at].values -= give.bytes;
                    self.kept += give.bytes;
                }
                Rung::FrontEnd => {
                    self.kept += has;
                    self.fronts[at] = Front { core: 0, values: 0 };
                }
                Rung::Running => {}
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

/// The invariant: under any schedule of limit changes, asks and builds, nothing of value is
/// given while a cheaper give-back would cover what is short; what is given for an ask is
/// within [`FACTOR`] times its target; the floor is never given whole; a front end made again
/// is not given again within its hold; and each rung's gives are measured before the next.
#[hegel::test(test_cases = 300)]
fn nothing_of_value_is_given_while_a_cheaper_give_back_covers_the_wait(tc: TestCase) {
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
                eprintln!(
                    "{plant:?} caught at seed {seed} (REPLAY = ({seed}, Plant::{plant:?})): {why}"
                );
                return;
            }
            Err(broke @ Broke::Harness(_)) => panic!("seed {seed}, {plant:?}: {broke}"),
            Ok(()) => {}
        }
    }
    panic!(
        "the oracle caught Plant::{plant:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak"
    );
}

/// The control: the rule before this module.
#[test]
fn the_oracle_catches_every_front_end_dropped_whole_for_any_ask() {
    control(Plant::WholeFrontEnd);
}

#[test]
fn the_oracle_catches_a_rung_asked_before_the_last_is_measured() {
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

/// The daemon of 2026-11-12, 15:15:02: 55 GiB held of a limit of 67.08, the allocator holding
/// freed pages, reth's front end the only one kept, and a task that waits for 0.09 GiB.
fn incident() -> (Now, Vec<Source>) {
    let now = Now {
        at_ns: 1_000_000_000,
        limit: 67 * GIB + 82 * MIB,
        footprint: 55 * GIB,
        ask: 92 * MIB,
        reading: 7,
        purged: 2,
    };
    let sources = vec![
        Source { remake_ns: 195_400_000_000, floor: true, ..source(Rung::FrontEnd, 0, 40 * GIB) },
        Source { grain: GRAIN, remake_ns: 12_000_000_000, ..source(Rung::Values, 0, 6 * GIB) },
        source(Rung::Allocator, 0, 20 * GIB),
    ];
    (now, sources)
}

/// The invariant: a wait smaller than what the allocator holds freed is answered by a purge
/// alone; nothing else is given before a reading after that purge, and with the wait gone
/// nothing is given at all. The front end is kept.
#[test]
fn a_small_wait_is_met_by_a_purge_and_the_front_end_is_kept() {
    let (now, sources) = incident();
    let mut planner = Planner::new();
    let mut out = Vec::new(); // alloc: a test's gives
    assert_eq!(planner.step(&now, &sources, &mut out), Outcome::Gave { rung: Rung::Allocator });
    assert_eq!(out.len(), 1);
    assert_eq!((out[0].rung, out[0].bytes, out[0].why), (Rung::Allocator, 20 * GIB, Why::Ask(92 * MIB)));
    // A reading that is not after the purge measures nothing.
    out.clear();
    let later = Now { at_ns: now.at_ns + TICK_NS, reading: 8, ..now };
    assert_eq!(planner.step(&later, &sources, &mut out), Outcome::Measuring { rung: Rung::Allocator });
    assert!(out.is_empty());
    // The purge landed: the footprint fell, the task fits, nothing more is given.
    let after = Now { at_ns: now.at_ns + 6 * TICK_NS, footprint: 35 * GIB, ask: 0, reading: 9, purged: 3, ..now };
    assert_eq!(planner.step(&after, &sources[..2], &mut out), Outcome::Enough);
    assert!(out.is_empty());
    assert_eq!(planner.counts().gives, [1, 0, 0, 0, 0, 0]);
}

/// The control of the test above (test-the-cause): the rule before this module, on the same
/// reading, gives the whole front end for the 0.09 GiB.
#[test]
fn the_rule_before_dropped_the_whole_front_end_for_the_same_wait() {
    let (now, sources) = incident();
    let mut planner = Planner::new();
    planner.plant = Plant::WholeFrontEnd;
    let mut out = Vec::new();
    assert_eq!(planner.step(&now, &sources, &mut out), Outcome::Gave { rung: Rung::FrontEnd });
    assert_eq!((out[0].rung, out[0].bytes), (Rung::FrontEnd, 40 * GIB));
}

/// The invariant: when the purge returned nothing the next rung gives what is short and a
/// margin, in its grain, never more; and with only front ends left a small wait gets none:
/// the task waits.
#[test]
fn a_small_wait_gets_values_in_proportion_and_never_a_front_end() {
    let (now, mut sources) = incident();
    // A second checkout's front end, not the floor: still out of proportion to 0.09 GiB.
    sources.push(Source { remake_ns: 60_000_000_000, ..source(Rung::FrontEnd, 1, 9 * GIB) });
    let mut planner = Planner::new();
    let mut out = Vec::new();
    assert_eq!(planner.step(&now, &sources, &mut out), Outcome::Gave { rung: Rung::Allocator });
    // The purge landed and returned nothing: the allocator's estimate stands.
    out.clear();
    let after = Now { at_ns: now.at_ns + TICK_NS, reading: 8, purged: 3, ..now };
    assert_eq!(planner.step(&after, &sources, &mut out), Outcome::Gave { rung: Rung::Values });
    // 92 MiB and the least margin, 64 MiB, in grains of 16 MiB.
    assert_eq!((out.len(), out[0].bytes), (1, 160 * MIB));
    assert_eq!(out[0].remake_ns, 12_000_000_000 * 160 / (6 * 1024));
    // No values left, the purge tried: a front end of 9 GiB is not given for 0.09 GiB.
    out.clear();
    sources[1].gives = 0;
    let last = Now { at_ns: after.at_ns + TICK_NS, reading: 9, ..after };
    assert_eq!(planner.step(&last, &sources, &mut out), Outcome::Waits { need: 92 * MIB });
    assert!(out.is_empty());
    assert_eq!(planner.counts().out_of_proportion, 1);
}

/// The invariant: within the quarter second after a purge, freed pages that grew by what is
/// short are purged again before any value goes: a purge that alone covers a wait always
/// comes first.
#[test]
fn freed_pages_that_grew_by_the_wait_are_purged_before_a_value_goes() {
    let (now, mut sources) = incident();
    let mut planner = Planner::new();
    let mut out = Vec::new(); // alloc: a test's gives
    assert_eq!(planner.step(&now, &sources, &mut out), Outcome::Gave { rung: Rung::Allocator });
    // The purge left 1 GiB the estimate still shows; a task then ended and freed 0.5 GiB.
    out.clear();
    sources[2].gives = GIB;
    let after = Now { at_ns: now.at_ns + TICK_NS, footprint: 36 * GIB, ask: 0, reading: 8, purged: 3, ..now };
    assert_eq!(planner.step(&after, &sources, &mut out), Outcome::Enough);
    sources[2].gives = GIB + 512 * MIB;
    let short = Now { at_ns: after.at_ns + TICK_NS, ask: 92 * MIB, reading: 9, ..after };
    assert_eq!(planner.step(&short, &sources, &mut out), Outcome::Gave { rung: Rung::Allocator });
    assert_eq!(out.len(), 1);
}

/// The invariant: past the limit, a front end that is not the floor goes, one, the least
/// remake time a byte first; with the floor alone left the planner says so and gives nothing.
#[test]
fn past_the_limit_one_front_end_goes_and_the_floor_is_said_not_given() {
    let mut sources = vec![
        Source { remake_ns: 195_000_000_000, floor: true, ..source(Rung::FrontEnd, 0, 30 * GIB) },
        Source { remake_ns: 100_000_000_000, ..source(Rung::FrontEnd, 1, 10 * GIB) },
        Source { remake_ns: 50_000_000_000, ..source(Rung::FrontEnd, 2, 10 * GIB) },
    ];
    let now = Now { at_ns: 1, limit: 45 * GIB, footprint: 52 * GIB, ask: 0, reading: 1, purged: 0 };
    let mut planner = Planner::new();
    let mut out = Vec::new();
    assert_eq!(planner.step(&now, &sources, &mut out), Outcome::Gave { rung: Rung::FrontEnd });
    assert_eq!(out.len(), 1, "7 GiB over and a margin: one front end of 10 covers it");
    assert_eq!((out[0].owner, out[0].why, out[0].again), (2, Why::Over(7 * GIB), false));
    // The limit falls again, under the floor itself.
    out.clear();
    sources[2].gives = 0;
    sources[1].gives = 0;
    let low = Now { at_ns: 2, limit: 20 * GIB, footprint: 32 * GIB, reading: 2, ..now };
    assert_eq!(
        planner.step(&low, &sources, &mut out),
        Outcome::FloorOver { over: 12 * GIB, limit: 20 * GIB, footprint: 32 * GIB, owner: 0, kept: 30 * GIB }
    );
    assert!(out.is_empty());
    assert_eq!(planner.counts().floor_over, 1);
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
    let now = Now { at_ns: 1, limit: 35 * GIB, footprint: 42 * GIB, ask: 0, reading: 1, purged: 0 };
    let mut planner = Planner::new();
    let mut out = Vec::new();
    assert_eq!(planner.step(&now, &sources, &mut out), Outcome::Gave { rung: Rung::FrontEnd });
    assert_eq!(out[0].owner, 1);
    // Its checkout builds: 100 s to lower anew, so it is held 1,600 s.
    let remade = 200_000_000_000;
    planner.remade(Rung::FrontEnd, 1, remade, 100_000_000_000);
    assert!(planner.held(Rung::FrontEnd, 1, remade + 1_599_000_000_000));
    assert!(!planner.held(Rung::FrontEnd, 1, remade + 1_600_000_000_000));
    out.clear();
    let again = Now { at_ns: remade + 1_000_000_000, reading: 2, ..now };
    assert!(matches!(planner.step(&again, &sources, &mut out), Outcome::FloorOver { .. }));
    assert!(out.is_empty());
    assert_eq!((planner.counts().held, planner.counts().thrash), (1, 0));
    // Past the hold it may go again, and that is said.
    let past = Now { at_ns: remade + 1_600_000_000_000, reading: 3, ..now };
    assert_eq!(planner.step(&past, &sources, &mut out), Outcome::Gave { rung: Rung::FrontEnd });
    assert!(out[0].again);
    assert_eq!(planner.counts().thrash, 1);
}

/// The invariant: a wait for a reading ends at [`MEASURE_WITHIN_NS`] (bounded-progress): the
/// planner goes on with the reading it has, and counts it.
#[test]
fn a_reading_that_never_comes_holds_the_next_rung_two_seconds_at_most() {
    let sources = [source(Rung::Allocator, 0, GIB), source(Rung::Caches, 0, 2 * GIB)];
    let now = Now { at_ns: 0, limit: u64::MAX, footprint: 10 * GIB, ask: GIB + 1, reading: 1, purged: 0 };
    let mut planner = Planner::new();
    let mut out = Vec::new();
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
        why: Why::Over(9_000_000_000),
        limit: 40_000_000_000,
        footprint: 49_000_000_000,
        again: false,
        by: Planned(()),
    };
    assert_eq!(
        dropped_line("reth", &give),
        "analysis of reth was dropped to free 12.0 GB (limit 40.0 GB, held 49.0 GB); the next build redoes it"
    );
    assert_eq!(
        floor_line("reth", 9_000_000_000, 40_000_000_000, 49_000_000_000, 30_000_000_000),
        "memory: blaze holds 49.0 GB, 9.0 GB past its limit of 40.0 GB, and what is left to give is the analysis of reth (30.0 GB), which the next edit needs: it is kept; free memory or raise the limit"
    );
}
