//! The collector's deterministic simulation (docs/gc.md, "The store's budget and collector"):
//! the store and its [`Collector`] on the simulation, with builds, reads, pins, periods' ends
//! whose scans come back late, crashes inside commits and failed effects drawn by Hegel, and a
//! plain model beside the run.

use std::collections::{BTreeMap, BTreeSet};

use hegel::TestCase;
use hegel::generators as gs;

use super::model_check::run_cases;
use super::*;
use crate::collect::{
    CollectBatch, Collector, Facts, Pace, PeriodScan, Pin, Plant, Standing, Step,
};

/// Keys are numbered below this.
const KEYS: u64 = 160;
/// The budget the simulated store is held to: far under what the runs put, so the collector
/// always has work.
const BUDGET: u64 = 96 << 10;

fn pace() -> Pace {
    Pace {
        gap_idle: Duration::from_millis(50),
        gap_full: Duration::from_millis(1),
        copy_most: 8 << 10,
        flush_most: 16 << 10,
        fill_most: 32 << 10,
        lap_gap: Duration::from_secs(5),
        grace: Duration::from_secs(10),
    }
}

/// The value of key `n`, always the same: an output long enough to be chunks for one key in
/// eight, else text of a few hundred to a few thousand bytes.
fn value(n: u64) -> (Kind, Vec<u8>) {
    if n % 8 == 7 {
        return (Kind::Output, noise(300 << 10, n));
    }
    let kind = [Kind::Query, Kind::Code, Kind::Debug, Kind::Run][n as usize % 4];
    (kind, text(n, 200 + (n as usize * 37) % 3000))
}

#[derive(Clone, Debug)]
enum Op {
    /// `count` keys from `from` on put, as a build puts what it made.
    Put(u64, u8),
    /// A key looked up and, if it is there, read: by the build under way, or by no build.
    Use(u64),
    /// A key's value pinned by a reader that keeps it (a kept front end).
    Hold(u64),
    /// Every reader's pin let go.
    Release,
    /// Pin `slot` names these keys from now on (none: the name is let go).
    Pin(u8, Vec<u64>),
    Flush,
    BuildBegin,
    BuildEnd,
    /// A period's scan handed out, and (a later op) run and handed back: what happens between
    /// is what a worker's scan races.
    ScanTake,
    ScanRun,
    /// Steps of the collector; a batch it hands out waits for `Work`.
    Collect(u8),
    /// The batch out is worked and handed back.
    Work,
    /// Steps of the cold tier's moves, as the daemon makes them between requests.
    Moves(u8),
    /// The collector runs until it rests with nothing out.
    CollectToRest,
    /// The clocks move on by this many seconds.
    Tick(u32),
    /// The disk stops answering syncs, the collector and a flush go on, then the power goes
    /// out and the store is opened again.
    CrashInCommit(u8),
    Crash,
    Reopen,
}

#[hegel::composite]
fn op(tc: &TestCase) -> Op {
    let key = || tc.draw(gs::integers::<u64>().max_value(KEYS - 1));
    match tc.draw(gs::integers::<u8>().max_value(27)) {
        0..5 => Op::Put(key(), tc.draw(gs::integers::<u8>().min_value(1).max_value(60))),
        5..8 => Op::Use(key()),
        8 => Op::Hold(key()),
        9 => Op::Release,
        10 | 11 => Op::Pin(
            tc.draw(gs::integers::<u8>().max_value(2)),
            tc.draw(gs::vecs(gs::integers::<u64>().max_value(KEYS - 1)).max_size(6)),
        ),
        12 | 13 => Op::Flush,
        14 => Op::BuildBegin,
        15 => Op::BuildEnd,
        16 | 17 => Op::ScanTake,
        18 | 19 => Op::ScanRun,
        20..23 => Op::Collect(tc.draw(gs::integers::<u8>().min_value(1).max_value(40))),
        23 | 24 => Op::Work,
        25 => Op::Tick(tc.draw(gs::integers::<u32>().min_value(1).max_value(30))),
        26 => match tc.draw(gs::integers::<u8>().max_value(3)) {
            0 => Op::Crash,
            1 => Op::Reopen,
            _ => Op::CrashInCommit(tc.draw(gs::integers::<u8>().max_value(40))),
        },
        _ => Op::Moves(tc.draw(gs::integers::<u8>().min_value(1).max_value(8))),
    }
}

/// What the model knows of a key the store was given.
struct Rec {
    bytes: Vec<u8>,
    ticket: Ticket,
}

/// The planted bugs of a run.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Plants {
    collect: Plant,
    head_before_syncs: bool,
    retire_pinned: bool,
    /// The driver tells the collector no build is under way and none ever began, whatever
    /// is: a pack is sealed under a build.
    seal_in_build: bool,
}

struct World {
    sim: Sim,
    store: Store,
    collector: Collector,
    plants: Plants,
    model: BTreeMap<u64, Rec>,
    /// Each pin's keys, and the ticket of its record.
    pins: BTreeMap<u8, (Vec<u64>, Ticket)>,
    /// The keys the build under way found.
    build: Option<BTreeSet<u64>>,
    builds: u64,
    /// The keys used (put, found or read) since the last period's end, and in the period
    /// before.
    used: BTreeSet<u64>,
    used_before: BTreeSet<u64>,
    scan: Option<PeriodScan>,
    batch: Option<CollectBatch>,
    /// What readers hold pinned: the key, the pin and the bytes it held when taken.
    held: Vec<(u64, Pinned, Vec<u8>)>,
    seconds: u64,
    /// The longest record put, as stored: the most one step may copy past its bound.
    longest: u64,
}

impl World {
    fn new(seed: u64, faults: Faults, plants: Plants) -> World {
        let mut sim = Sim::new(seed, faults);
        let mut store = open(&mut sim, "store");
        Self::plant(&mut store, plants);
        World {
            sim,
            store,
            collector: Collector::new(pace()),
            plants,
            model: BTreeMap::new(),
            pins: BTreeMap::new(),
            build: None,
            builds: 0,
            used: BTreeSet::new(),
            used_before: BTreeSet::new(),
            scan: None,
            batch: None,
            held: Vec::new(),
            seconds: 1_000_000,
            longest: 0,
        }
    }

    fn plant(store: &mut Store, plants: Plants) {
        store.collecting.plant = plants.collect;
        store.plant_head_before_syncs = plants.head_before_syncs;
        crate::pool::RETIRE_PINNED.with(|it| it.set(plants.retire_pinned));
    }

    /// What the store holds under key `n`, read as a check reads it (no flag of use set).
    fn peek(&self, n: u64) -> Result<Option<Vec<u8>>, String> {
        let look = self.store.look();
        match look.find(&key(n)) {
            None => Ok(None),
            Some(Err(corrupt)) => Err(format!("key {n}: {corrupt}")),
            Some(Ok(found)) => match look.value(found, &key(n), Use::Check) {
                Ok((_, bytes)) => Ok(Some(bytes.into_owned())),
                Err(corrupt) => Err(format!("key {n}: {corrupt}")),
            },
        }
    }

    /// The keys a live root names: the pins', the keys used in this period and the one
    /// before, and what the build under way found.
    fn rooted(&self) -> BTreeSet<u64> {
        let mut out: BTreeSet<u64> = self.pins.values().flat_map(|(keys, _)| keys).copied().collect();
        out.extend(&self.used);
        out.extend(&self.used_before);
        out.extend(self.build.iter().flatten());
        out
    }

    /// The oracle, after every step: a key the model holds reads its bytes or, when no root
    /// names it, nothing (and the model lets it go); what a reader pinned still reads as it
    /// did.
    fn check(&mut self) -> Result<(), String> {
        if let Some(broken) = self.store.broken() {
            return Err(format!("the store stopped: {broken}"));
        }
        let rooted = self.rooted();
        let mut gone = Vec::new();
        for (&n, rec) in &self.model {
            match self.peek(n)? {
                Some(bytes) if bytes == rec.bytes => {}
                Some(bytes) => {
                    return Err(format!("key {n}: read {} other bytes", bytes.len()));
                }
                None if rooted.contains(&n) => {
                    let by = if self.build.as_ref().is_some_and(|it| it.contains(&n)) {
                        "the build under way found it"
                    } else if self.pins.values().any(|(keys, _)| keys.contains(&n)) {
                        "a pin names it"
                    } else {
                        "it was used within two periods"
                    };
                    return Err(format!("key {n}: forgotten, though {by}"));
                }
                None => gone.push(n),
            }
        }
        for n in gone {
            self.model.remove(&n);
        }
        for (n, pin, bytes) in &self.held {
            if **pin != bytes[..] {
                return Err(format!("key {n}: the bytes a reader pinned changed under it"));
            }
        }
        Ok(())
    }

    fn drive(&mut self, effects: &mut Vec<Effect>) {
        drive(&mut self.sim, &mut self.store, effects);
    }

    fn put(&mut self, n: u64) -> Result<(), String> {
        let (kind, bytes) = value(n);
        let ticket = self
            .store
            .put(key(n), kind, &bytes)
            .map_err(|mismatch| format!("key {n}: its put was refused: {mismatch:?}"))?;
        // A put of a value the store has writes nothing, and is a use of it as a read is.
        let ticket = self.model.get(&n).map_or(ticket, |rec| rec.ticket);
        self.longest = self.longest.max(bytes.len() as u64 + 4096);
        self.model.insert(n, Rec { bytes, ticket });
        self.used.insert(n);
        Ok(())
    }

    fn facts(&self) -> Facts {
        Facts {
            held: self.store.usage().bytes,
            budget: BUDGET,
            idle: self.build.is_none() || self.plants.seal_in_build,
            builds: if self.plants.seal_in_build { 0 } else { self.builds },
            seconds: self.seconds,
        }
    }

    /// One step of the collector, with the checks of its bounds: no commit begun while a
    /// build is under way, and no step that copies more than its bound or one record.
    fn collect_step(&mut self) -> Result<bool, String> {
        // As the daemon's turn does before it: an index swap's flags are carried over.
        if let Some(mut carry) = self.store.carry_take() {
            carry.run();
            self.store.carry_put(carry);
        }
        self.collector.advance_clock(self.sim.now());
        self.collector.observe(self.facts());
        let commits = self.store.usage().counts.commits;
        let mut effects = Vec::new();
        let step = self.collector.step(&mut self.store, &mut effects);
        self.drive(&mut effects);
        if self.build.is_some() && !self.plants.seal_in_build && self.store.usage().counts.commits != commits {
            return Err("the collector began a commit while a build was under way".into());
        }
        let (_, most) = self.collector.steps();
        if most > pace().copy_most.max(self.longest) {
            return Err(format!("a step of the collector copied {most} bytes, past its bound"));
        }
        Ok(match step {
            Step::Rest => false,
            Step::Stepped(_) => true,
            Step::Work(batch) => {
                assert!(self.batch.is_none(), "two batches out at once");
                self.batch = Some(batch);
                true
            }
        })
    }

    fn work(&mut self) {
        if let Some(mut batch) = self.batch.take() {
            batch.work();
            self.collector.back(batch);
        }
    }

    fn scan_run(&mut self) {
        if let Some(mut scan) = self.scan.take() {
            scan.run();
            // The flags rotated as the scan ended: this period's uses are the one before's.
            self.used_before = std::mem::take(&mut self.used);
            self.store.plan_put(scan);
        }
    }

    /// The store dropped and opened again, after a crash or not: the collector's batch and
    /// scan are gone with the process, and so is everything the store kept in memory of use.
    fn reopen(&mut self, crashed: bool) -> Result<(), String> {
        // What the model may require after a crash: what was durable before it.
        let durable: BTreeSet<u64> = self
            .model
            .iter()
            .filter(|(_, rec)| self.store.durable(rec.ticket))
            .map(|(&n, _)| n)
            .collect();
        let pins: Vec<u8> = self
            .pins
            .iter()
            .filter(|(_, (_, ticket))| self.store.durable(*ticket))
            .map(|(&slot, _)| slot)
            .collect();
        self.held.clear();
        self.batch = None;
        self.scan = None;
        self.build = None;
        self.used.clear();
        self.used_before.clear();
        if crashed {
            self.sim.crash();
            self.model.retain(|n, _| durable.contains(n));
            self.pins.retain(|slot, _| pins.contains(slot));
        } else {
            let mut effects = Vec::new();
            self.store.flush(&mut effects);
            self.drive(&mut effects);
        }
        let mut effects = Vec::new();
        let store = Store::open(config("store"), &mut effects);
        let old = std::mem::replace(&mut self.store, store);
        drop(old);
        self.drive(&mut effects);
        if let Some(broken) = self.store.broken() {
            return Err(format!("the store did not open: {broken}"));
        }
        if !self.store.ready() {
            return Err("the store did not open, and says nothing of why".into());
        }
        Self::plant(&mut self.store, self.plants);
        self.collector = Collector::new(pace());
        // Every key's ticket is of the store that is gone: durable from here on.
        for rec in self.model.values_mut() {
            rec.ticket = Ticket::DURABLE;
        }
        for (_, ticket) in self.pins.values_mut() {
            *ticket = Ticket::DURABLE;
        }
        // A pin whose keys were not all durable names keys the crash took: the model keeps
        // the pin for the keys it still holds.
        let model = &self.model;
        for (keys, _) in self.pins.values_mut() {
            keys.retain(|n| model.contains_key(n));
        }
        Ok(())
    }

    fn step(&mut self, op: &Op) -> Result<(), String> {
        match op {
            Op::Put(from, count) => {
                for n in *from..(*from + u64::from(*count)).min(KEYS) {
                    self.put(n)?;
                }
            }
            Op::Use(n) => {
                let guard = enter();
                let reader = self.store.reader();
                if reader.has(&guard, &key(*n)) {
                    let read = reader
                        .get(&guard, &key(*n))
                        .map_err(|corrupt| format!("key {n}: {corrupt}"))?
                        .map(|it| it.bytes.to_vec());
                    if read.is_none() {
                        return Err(format!("key {n}: found, then read as nothing"));
                    }
                    if self.model.contains_key(n) {
                        self.used.insert(*n);
                        if let Some(build) = &mut self.build {
                            build.insert(*n);
                        }
                    }
                }
            }
            Op::Hold(n) => {
                let pinned = self.store.pinned(&key(*n)).map_err(|it| format!("key {n}: {it}"))?;
                if let Some(pin) = pinned {
                    let bytes = pin.to_vec();
                    if self.model.contains_key(n) {
                        self.used.insert(*n);
                    }
                    self.held.push((*n, pin, bytes));
                }
            }
            Op::Release => self.held.clear(),
            Op::Pin(slot, keys) => {
                let keys: Vec<u64> =
                    keys.iter().copied().filter(|n| self.model.contains_key(n)).collect();
                let named: Vec<Key> = keys.iter().map(|&n| key(n)).collect();
                self.seconds += 1;
                let ticket = self
                    .store
                    .pin(&format!("pin {slot}"), "the simulation's", self.seconds, &named)
                    .map_err(|mismatch| format!("pin {slot}: refused: {mismatch:?}"))?;
                match keys.is_empty() {
                    true => _ = self.pins.remove(slot),
                    false => _ = self.pins.insert(*slot, (keys, ticket)),
                }
            }
            Op::Flush => {
                let mut effects = Vec::new();
                self.store.flush(&mut effects);
                self.drive(&mut effects);
            }
            Op::BuildBegin => {
                if self.build.is_none() {
                    self.build = Some(BTreeSet::new());
                    self.builds += 1;
                }
            }
            Op::BuildEnd => self.build = None,
            Op::ScanTake => {
                // An index swap's flags of use are carried over first, as the daemon's worker
                // does.
                if let Some(mut carry) = self.store.carry_take() {
                    carry.run();
                    self.store.carry_put(carry);
                }
                // A few asks: an index swap's fold ends at a task boundary or two.
                for _ in 0..8 {
                    if self.scan.is_none() {
                        self.scan = self.store.plan_take();
                    }
                }
            }
            Op::Moves(steps) => {
                for _ in 0..*steps {
                    if let Some(mut batch) = self.store.settle_take(64 << 10) {
                        batch.compress();
                        self.store.settle_put(batch);
                    } else {
                        let mut effects = Vec::new();
                        self.store.settle_step(64 << 10, &mut effects);
                        self.drive(&mut effects);
                    }
                }
            }
            Op::CollectToRest => self.collect_to_rest()?,
            Op::ScanRun => self.scan_run(),
            Op::Collect(steps) => {
                for _ in 0..*steps {
                    if !self.collect_step()? {
                        break;
                    }
                }
            }
            Op::Work => self.work(),
            Op::Tick(seconds) => {
                self.seconds += u64::from(*seconds);
                self.sim.advance(Duration::from_secs(u64::from(*seconds)));
                self.store.clock(self.seconds);
            }
            Op::CrashInCommit(steps) => {
                self.sim.hold_syncs(name("store"));
                for _ in 0..*steps {
                    self.work();
                    if !self.collect_step()? {
                        break;
                    }
                }
                let mut effects = Vec::new();
                self.store.flush(&mut effects);
                self.drive(&mut effects);
                self.reopen(true)?;
            }
            Op::Crash => self.reopen(true)?,
            Op::Reopen => self.reopen(false)?,
        }
        Ok(())
    }

    /// Lets the collector run until it rests with nothing out: every batch worked at once,
    /// the clocks moved past each gap.
    fn collect_to_rest(&mut self) -> Result<(), String> {
        // Turns in a row the collector rested on the store (an index swap's fold ends at a
        // task boundary or two): the daemon's loop turns again, and so does this.
        let mut on_store = 0;
        for round in 0..20_000 {
            // The oracle reads every value: every so many steps, and at the rest.
            if round % 64 == 63 {
                self.check()?;
            }
            self.work();
            if self.collect_step()? {
                on_store = 0;
            } else {
                if self.batch.is_some() {
                    continue;
                }
                let now = self.sim.now();
                if !self.collector.wants() {
                    return self.check();
                }
                if self.collector.waits() == Some(crate::collect::Waits::Store) && on_store < 64 {
                    on_store += 1;
                    self.sim.advance(Duration::from_millis(1));
                    continue;
                }
                match self.collector.next_deadline() {
                    // The gap to its next pack.
                    Some(at) if at <= now + Duration::from_secs(1) => {
                        self.sim.advance(at.saturating_sub(now).max(Duration::from_millis(1)));
                    }
                    // The lap ended, or it rests on something that is not the time.
                    _ => return self.check(),
                }
            }
        }
        Err("the collector did not come to rest in 20000 steps".into())
    }

    /// The end of a run: the build ends, periods end until the flags of use say what is
    /// unused, the packs age past the grace, and the collector runs to rest. Then the store
    /// is under its budget or the collector says it is over; and the store verifies clean.
    fn settle_down(&mut self) -> Result<(), String> {
        self.build = None;
        self.held.clear();
        self.scan_run();
        for _ in 0..3 {
            self.step(&Op::Flush)?;
            self.step(&Op::Tick(20))?;
            self.step(&Op::ScanTake)?;
            self.step(&Op::ScanRun)?;
            self.check()?;
            self.collect_to_rest()?;
        }
        self.step(&Op::Flush)?;
        self.check()?;
        self.collector.observe(self.facts());
        let (bytes, standing) = (self.store.usage().bytes, self.collector.standing());
        if bytes >= BUDGET && !matches!(standing, Standing::OverBudget { .. }) {
            return Err(format!(
                "the store holds {bytes} bytes, at or over its budget of {BUDGET}, and its collector says {standing:?}, waiting for {:?}; lap {:?}, last {:?}; {:?}",
                self.collector.waits(),
                self.collector.lap(),
                self.collector.last_lap(),
                self.store.usage(),
            ));
        }
        match self.store.verify(None) {
            corrupt if corrupt.is_empty() => Ok(()),
            corrupt => Err(format!("the store does not verify clean: {corrupt:?}")),
        }
    }
}

/// Runs `ops` with the oracle after each; `Err` says what it found and after which op.
fn runs(ops: &[Op], seed: u64, faults: Faults, plants: Plants) -> Result<(), String> {
    let mut world = World::new(seed, faults, plants);
    let result = (|| {
        for (at, op) in ops.iter().enumerate() {
            world.step(op).and_then(|()| world.check()).map_err(|why| format!("op {at} ({op:?}): {why}"))?;
        }
        world.settle_down().map_err(|why| format!("at the end: {why}"))
    })();
    crate::pool::RETIRE_PINNED.with(|it| it.set(false));
    result
}

/// A store given failures it did not cause may stop, saying so: not the collector's doing.
fn injected(why: &str) -> bool {
    why.contains("injected by the simulation")
}

/// The invariant: no record a live root names is ever forgotten (a pin's keys, what was used
/// within two periods, what the build under way found: a build never finds a key and then
/// misses it); after any crash the store opens and reads every record that was durable and
/// pinned; what a reader pinned reads as it did; the collector begins no commit while a build
/// is under way and no step of it copies more than its bound; and a store left to itself
/// ends under its budget or says it is over. `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn the_collector_forgets_nothing_a_root_names() {
    run_cases(64, |tc| {
        let ops = tc.draw(gs::vecs(op()).min_size(1).max_size(60));
        let seed = tc.draw(gs::integers());
        let faults = Faults {
            fail_one_in: [0, 0, 0, 23][tc.draw(gs::integers::<usize>().max_value(3))],
            max_latency: Duration::from_micros(tc.draw(gs::integers::<u64>().max_value(5000))),
            ..Faults::default()
        };
        if let Err(why) = runs(&ops, seed, faults.clone(), Plants::default())
            && !injected(&why)
        {
            panic!("seed {seed}, {faults:?}, {ops:?}: {why}");
        }
    });
}

/// The directed run every control shares: a first build puts a hundred records and a second
/// forty more (so the first's are in the index table), a pin names two of the first's, a
/// reader pins another, the first's go unused for two periods and their packs age past the
/// grace, and the collector runs with the store far over its budget.
fn directed() -> Vec<Op> {
    vec![
        Op::BuildBegin,
        Op::Put(0, 100),
        Op::Flush,
        Op::BuildEnd,
        Op::Pin(0, vec![3, 7, 60]),
        Op::Flush,
        Op::ScanTake,
        Op::ScanRun,
        Op::BuildBegin,
        Op::Put(100, 40),
        Op::Flush,
        Op::BuildEnd,
        Op::Tick(20),
        Op::ScanTake,
        Op::ScanRun,
        Op::Hold(9),
        Op::Tick(20),
        Op::ScanTake,
        Op::ScanRun,
        Op::Tick(20),
        Op::ScanTake,
        Op::ScanRun,
        Op::BuildBegin,
        Op::Use(50),
        Op::BuildEnd,
        Op::Tick(20),
        Op::CollectToRest,
        Op::Flush,
    ]
}

/// The directed run collects: it passes as the store is, and it forgets records and gives
/// bytes back, so the controls below test a collector that does something.
#[test]
fn the_directed_run_forgets_what_no_root_names_and_keeps_the_rest() {
    let mut world = World::new(3, quiet(), Plants::default());
    for (at, op) in directed().iter().enumerate() {
        world.step(op).and_then(|()| world.check()).unwrap_or_else(|why| panic!("op {at} ({op:?}): {why}"));
    }
    world.settle_down().unwrap();
    let usage = world.store.usage();
    assert!(usage.counts.packs_retired > 0, "{usage:?}");
    assert!(usage.counts.records_forgotten > 0, "{usage:?}");
    assert!(usage.counts.forgotten_bytes > BUDGET, "{usage:?}");
    for n in [3, 7, 60] {
        assert_eq!(world.peek(n).unwrap(), Some(value(n).1), "pinned key {n}");
    }
    assert!(world.model.len() < 140, "the model let go of what was forgotten");
    // What was forgotten is a miss and can be put again.
    let forgotten = (0..100).find(|n| !world.model.contains_key(n)).expect("a key was forgotten");
    assert_eq!(world.peek(forgotten).unwrap(), None);
    world.put(forgotten).unwrap();
    assert_eq!(world.peek(forgotten).unwrap(), Some(value(forgotten).1));
    world.step(&Op::Reopen).unwrap();
    world.check().unwrap();
    assert_eq!(world.peek(forgotten).unwrap(), Some(value(forgotten).1), "put again, kept across a reopen");
    assert_eq!(world.peek(3).unwrap(), Some(value(3).1), "the pin holds across a reopen");
}

fn control(plants: Plants, ops: Vec<Op>, seeds: std::ops::Range<u64>) -> (u64, String) {
    let mut caught = None;
    for seed in seeds {
        assert_eq!(
            runs(&ops, seed, quiet(), Plants::default()),
            Ok(()),
            "seed {seed}: the collector as it is must pass"
        );
        if caught.is_none() {
            // A planted bug may also trip a check inside the store (the pool's own, of a
            // pin given back twice): that is the bug caught too, said as its panic.
            let ops = ops.clone();
            let ran = std::panic::catch_unwind(move || runs(&ops, seed, quiet(), plants));
            crate::pool::RETIRE_PINNED.with(|it| it.set(false));
            match ran {
                Ok(Ok(())) => {}
                Ok(Err(why)) => caught = Some((seed, why)),
                Err(panic) => {
                    let why = panic
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| panic.downcast_ref::<&str>().map(|it| (*it).to_owned()))
                        .unwrap_or_else(|| "a panic".to_owned());
                    caught = Some((seed, format!("panicked: {why}")));
                }
            }
        }
    }
    caught.unwrap_or_else(|| panic!("the oracle missed the planted bug {plants:?}"))
}

/// The planted-bug control: a scan that keeps nothing for a pin's sake forgets a pinned key.
#[test]
fn the_oracle_catches_a_pin_ignored() {
    let plants = Plants { collect: Plant::PinIgnored, ..Plants::default() };
    let (seed, why) = control(plants, directed(), 0..4);
    assert!(why.contains("forgotten, though a pin names it"), "seed {seed}: {why}");
}

/// The planted-bug control: a scan that misses the flags of use forgets what a build found
/// in the period before.
#[test]
fn the_oracle_catches_a_root_missed() {
    let plants = Plants { collect: Plant::RootMissed, ..Plants::default() };
    let (seed, why) = control(plants, directed(), 0..4);
    assert!(why.contains("forgotten, though it was used within two periods"), "seed {seed}: {why}");
}

/// A build that begins after a pack was scanned, and finds a key the scan judged unused: the
/// collector must not seal the pack as scanned. `Err` says what the oracle found.
fn seal_under_a_build(seed: u64, plants: Plants) -> Result<(), String> {
    let mut world = World::new(seed, quiet(), plants);
    let ops = directed();
    let at = ops.iter().position(|op| matches!(op, Op::CollectToRest)).unwrap();
    for op in &ops[..at] {
        world.step(op).and_then(|()| world.check())?;
    }
    // Until a batch is out, scanned, and forgets a key the model holds.
    let mut found = None;
    for _ in 0..5000 {
        let Some(mut batch) = world.batch.take() else {
            if !world.collect_step()? {
                world.sim.advance(Duration::from_millis(60));
            }
            continue;
        };
        batch.work();
        found = match batch.passed() {
            Some(_) => None,
            None => world
                .model
                .keys()
                .copied()
                .find(|n| batch.forgotten_keys.contains(&key(*n))),
        };
        world.collector.back(batch);
        if found.is_some() {
            break;
        }
    }
    let found = found.ok_or("no pack scanned forgets a key the model holds")?;
    world.step(&Op::BuildBegin)?;
    world.step(&Op::Use(found))?;
    world.check()?;
    let result = world.collect_to_rest().and_then(|()| world.check());
    crate::pool::RETIRE_PINNED.with(|it| it.set(false));
    result
}

/// The planted-bug control: a driver that tells the collector no build is under way while one
/// is forgets a key between the build's finding it and its reading it.
#[test]
fn the_oracle_catches_a_seal_under_a_build() {
    let mut caught = None;
    for seed in 0..4 {
        assert_eq!(seal_under_a_build(seed, Plants::default()), Ok(()), "seed {seed}");
        let plants = Plants { seal_in_build: true, ..Plants::default() };
        if caught.is_none()
            && let Err(why) = seal_under_a_build(seed, plants)
        {
            caught = Some((seed, why));
        }
    }
    let (seed, why) = caught.expect("the oracle missed the planted seal under a build");
    assert!(why.contains("forgotten, though the build under way found it"), "seed {seed}: {why}");
}

/// The planted-bug control: a retired pack's pages given back while a reader pins them
/// change the bytes under the reader.
#[test]
fn the_oracle_catches_a_pack_retired_under_a_readers_pin() {
    let plants = Plants { retire_pinned: true, ..Plants::default() };
    let (seed, why) = control(plants, directed(), 0..4);
    assert!(
        why.contains("the bytes a reader pinned changed under it") || why.contains("given back"),
        "seed {seed}: {why}"
    );
}

/// The planted-bug control: with the head written before the copies it names are synced, a
/// crash inside the commit that retires a pack leaves a head naming the pack removed and the
/// pinned records' copies not on the disk.
#[test]
fn the_oracle_catches_a_head_written_before_the_copies_are_synced() {
    let mut ops = directed();
    let at = ops.iter().position(|op| matches!(op, Op::CollectToRest)).unwrap();
    ops.truncate(at);
    ops.push(Op::CrashInCommit(200));
    let plants = Plants { head_before_syncs: true, ..Plants::default() };
    let (seed, why) = control(plants, ops, 0..16);
    // The pinned keys are 3 and 7, and 7 is a chunked output: its list or a chunk is gone,
    // or the store does not open on a head naming bytes that are not there.
    assert!(
        why.contains("forgotten, though a pin names it")
            || why.contains("did not open")
            || why.contains("key 7")
            || why.contains("key 3"),
        "seed {seed}: {why}"
    );
}

/// The planted-bug control: a seal before the records it keeps were copied on forgets them.
#[test]
fn the_oracle_catches_a_seal_before_the_copies() {
    let plants = Plants { collect: Plant::SealBeforeCopies, ..Plants::default() };
    let (seed, why) = control(plants, directed(), 0..4);
    assert!(why.contains("forgotten, though"), "seed {seed}: {why}");
}

/// A pin's bytes round-trip, and bytes of another layout are refused by name.
#[test]
fn a_pin_round_trips_and_another_layout_is_refused_naming_it() {
    let pin = Pin {
        name: "toolchain".into(),
        why: "the standard library's records, copied in".into(),
        when: 1_762_900_000_000_000_000,
        keys: vec![key(1), key(2), key(99)],
    };
    let bytes = pin.encode();
    assert_eq!(Pin::decode(&bytes).unwrap(), pin);
    assert_eq!(Pin::decode(&Pin { keys: Vec::new(), ..pin.clone() }.encode()).unwrap().keys, Vec::new());
    let mut other = bytes.clone();
    other[7] = 2;
    assert!(Pin::decode(&other).unwrap_err().contains("a pin of layout 2, where this blaze reads layout 1"));
    assert!(Pin::decode(&bytes[..bytes.len() - 1]).unwrap_err().contains("whose lengths say"));
    assert!(Pin::decode(b"not a pin").is_err());
    // Layout 1, written out: what a pin of this layout is on any blaze.
    let small = Pin { name: "a".into(), why: "b".into(), when: 5, keys: vec![Key(ContentId([9; 32]))] };
    let mut want = b"BLZPIN\x00\x01".to_vec();
    want.extend(5u64.to_le_bytes());
    want.extend(1u32.to_le_bytes());
    want.extend(1u32.to_le_bytes());
    want.extend(1u64.to_le_bytes());
    want.extend(b"ab");
    want.extend([9; 32]);
    assert_eq!(small.encode(), want);
}

/// The newest pin of a name is the name's, across a reopen: pins are records, found by the
/// first scan after the open.
#[test]
fn the_newest_pin_of_a_name_is_kept_across_a_reopen() {
    let mut sim = Sim::new(4, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..6 {
        store.put(key(n), Kind::Code, &text(n, 500)).unwrap();
    }
    store.pin("toolchain", "first", 10, &[key(1)]).unwrap();
    store.pin("toolchain", "second", 20, &[key(2), key(3)]).unwrap();
    store.pin("old", "let go", 10, &[key(4)]).unwrap();
    store.pin("old", "let go", 11, &[]).unwrap();
    flush(&mut sim, &mut store);
    let names = |store: &Store| -> Vec<(String, String, usize)> {
        store.pins().map(|pin| (pin.name.clone(), pin.why.clone(), pin.keys.len())).collect()
    };
    assert_eq!(names(&store), [("toolchain".to_owned(), "second".to_owned(), 2)]);
    drop(store);
    let mut store = open(&mut sim, "store");
    assert_eq!(names(&store), [], "no scan yet");
    assert!(!store.usage().marked);
    assert!(store.end_period());
    assert_eq!(names(&store), [("toolchain".to_owned(), "second".to_owned(), 2)]);
    assert!(store.usage().marked);
    assert!(store.stored_bytes(&[key(2), key(3)]) >= 1000);
}

/// The one wait a request can meet: a flush asked for while a commit the collector began is
/// in flight waits for the rest of it, is counted, and the commit it waits for holds no
/// more than the collector's bound of copied bytes and writes no index table.
#[test]
fn a_flush_that_meets_the_collectors_commit_is_counted_and_the_commit_is_small() {
    let mut world = World::new(5, quiet(), Plants::default());
    let ops = directed();
    let at = ops.iter().position(|op| matches!(op, Op::CollectToRest)).unwrap();
    for op in &ops[..at] {
        world.step(op).unwrap();
    }
    // Steps until the collector begins a commit, its effects held back.
    let mut effects = Vec::new();
    let mut begun = false;
    for _ in 0..10_000 {
        world.work();
        world.collector.advance_clock(world.sim.now());
        world.collector.observe(world.facts());
        let before = world.store.usage().counts.commits;
        match world.collector.step(&mut world.store, &mut effects) {
            Step::Work(batch) => world.batch = Some(batch),
            Step::Rest => world.sim.advance(Duration::from_millis(60)),
            Step::Stepped(_) => {}
        }
        if world.store.usage().counts.commits != before {
            begun = true;
            break;
        }
        world.drive(&mut effects);
    }
    assert!(begun, "the collector began no commit");
    assert!(world.store.collect_committing());
    assert!(
        !effects.iter().any(|it| matches!(it, Effect::Write { file, .. } if file.as_str().contains("index-"))),
        "the collector's commit writes no index table"
    );
    let commit = world.store.commit.as_ref().expect("a commit in flight");
    let appended: u64 = commit
        .packs
        .iter()
        .map(|&(at, to)| to - world.store.tiers[0].packs[at].committed)
        .sum();
    let most = pace().flush_most + pace().copy_most + world.longest + (256 << 10);
    assert!(appended <= most, "the collector's commit appends {appended} bytes, past {most}");
    // A build's put and flush arrive now.
    world.build = Some(BTreeSet::new());
    world.builds += 1;
    world.put(150).unwrap();
    let ticket = world.model[&150].ticket;
    world.store.flush(&mut effects);
    assert_eq!(world.store.usage().counts.flush_waits, 1, "the flush met the collector's commit");
    world.drive(&mut effects);
    assert!(world.store.durable(ticket), "the build's put is durable after both commits");
    assert!(!world.store.collect_committing());
    world.check().unwrap();
}
