//! The index table built off the store's thread ([`crate::rebuild`]), on the simulation: puts,
//! flushes, cold moves and periods' ends while a build is out, builds handed back late or
//! lost, crashes inside the commit that writes the table and at each of its steps, failed
//! effects, and a plain map beside the run.

use std::collections::{BTreeMap, BTreeSet};

use hegel::TestCase;
use hegel::generators as gs;

use super::model_check::run_cases;
use super::*;
use crate::rebuild::IndexBuild;

/// Keys are numbered below this: several index tables' worth.
const KEYS: u64 = 400;

fn value(n: u64) -> Vec<u8> {
    text(n, 40 + (n as usize * 37) % 900)
}

#[derive(Clone, Debug)]
enum Op {
    /// `count` keys from `from` on put.
    Put(u64, u8),
    Flush,
    /// A build taken, if one is due; it waits for `Run` and `Back`.
    Take,
    Run,
    /// The build out handed back (run first if it was not).
    Back,
    /// The build out lost with its worker.
    Lose,
    /// A period of reading ends, and steps of its cold moves are made.
    Moves(u8),
    /// The disk stops answering syncs, the build out is handed back and a flush asked, then
    /// the power goes out and the store is opened again.
    CrashInCommit,
    Crash,
    Reopen,
}

#[hegel::composite]
fn op(tc: &TestCase) -> Op {
    let key = || tc.draw(gs::integers::<u64>().max_value(KEYS - 1));
    match tc.draw(gs::integers::<u8>().max_value(19)) {
        0..6 => Op::Put(key(), tc.draw(gs::integers::<u8>().min_value(1).max_value(70))),
        6..9 => Op::Flush,
        9..12 => Op::Take,
        12 | 13 => Op::Run,
        14 | 15 => Op::Back,
        16 => Op::Lose,
        17 => Op::Moves(tc.draw(gs::integers::<u8>().min_value(1).max_value(6))),
        18 => match tc.draw(gs::integers::<u8>().max_value(2)) {
            0 => Op::Crash,
            1 => Op::Reopen,
            _ => Op::CrashInCommit,
        },
        _ => Op::Take,
    }
}

struct World {
    sim: Sim,
    store: Store,
    /// Each key put, with its put's ticket.
    model: BTreeMap<u64, Ticket>,
    build: Option<IndexBuild>,
    plant: bool,
    /// Whether tables are built off the store's thread (the subject) or in its commits.
    off_thread: bool,
}

impl World {
    fn new(seed: u64, faults: Faults, plant: bool, off_thread: bool) -> World {
        let mut sim = Sim::new(seed, faults);
        let store = open(&mut sim, "store");
        let mut world =
            World { sim, store, model: BTreeMap::new(), build: None, plant, off_thread };
        world.set();
        world
    }

    fn set(&mut self) {
        if self.off_thread {
            self.store.index_off_thread();
        }
        self.store.rebuild.plant_indexed_at_write = self.plant;
    }

    fn drive(&mut self, effects: &mut Vec<Effect>) {
        drive(&mut self.sim, &mut self.store, effects);
    }

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

    /// The oracle: the store is whole, and every key the model holds reads its value.
    fn check(&self) -> Result<(), String> {
        if let Some(broken) = self.store.broken() {
            return Err(format!("the store stopped: {broken}"));
        }
        for &n in self.model.keys() {
            match self.peek(n)? {
                Some(bytes) if bytes == value(n) => {}
                Some(_) => return Err(format!("key {n}: reads other bytes than were put")),
                None => return Err(format!("key {n}: put, durable if a crash came, and gone")),
            }
        }
        Ok(())
    }

    fn back(&mut self) {
        let Some(mut build) = self.build.take() else { return };
        build.run();
        let mut effects = Vec::new();
        self.store.index_put(build, &mut effects);
        self.drive(&mut effects);
    }

    fn reopen(&mut self, crashed: bool) -> Result<(), String> {
        let durable: BTreeSet<u64> = self
            .model
            .iter()
            .filter(|(_, ticket)| self.store.durable(**ticket))
            .map(|(&n, _)| n)
            .collect();
        self.build = None;
        if crashed {
            self.sim.crash();
            self.model.retain(|n, _| durable.contains(n));
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
        self.set();
        for ticket in self.model.values_mut() {
            *ticket = Ticket::DURABLE;
        }
        Ok(())
    }

    fn step(&mut self, op: &Op) -> Result<(), String> {
        match op {
            Op::Put(from, count) => {
                for n in (*from..KEYS).take(*count as usize) {
                    let ticket = self
                        .store
                        .put(key(n), Kind::Query, &value(n))
                        .map_err(|mismatch| format!("key {n}: its put was refused: {mismatch:?}"))?;
                    self.model.entry(n).or_insert(ticket);
                }
            }
            Op::Flush => {
                let mut effects = Vec::new();
                self.store.flush(&mut effects);
                self.drive(&mut effects);
            }
            Op::Take => {
                if self.build.is_none() {
                    self.build = self.store.index_take();
                }
            }
            Op::Run => {
                if let Some(build) = &mut self.build {
                    build.run();
                }
            }
            Op::Back => self.back(),
            Op::Lose => {
                if self.build.take().is_some() {
                    self.store.index_lost();
                }
            }
            Op::Moves(steps) => {
                if let Some(mut carry) = self.store.carry_take() {
                    carry.run();
                    self.store.carry_put(carry);
                }
                _ = self.store.end_period();
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
            Op::CrashInCommit => {
                self.sim.hold_syncs(name("store"));
                self.back();
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
}

fn runs(ops: &[Op], seed: u64, faults: Faults, plant: bool, off_thread: bool) -> Result<World, String> {
    let mut world = World::new(seed, faults, plant, off_thread);
    for (at, op) in ops.iter().enumerate() {
        world
            .step(op)
            .and_then(|()| world.check())
            .map_err(|why| format!("op {at} ({op:?}): {why}"))?;
    }
    // At the end: the build out comes back, and a clean reopen reads everything.
    world.back();
    world.reopen(false).and_then(|()| world.check()).map_err(|why| format!("at the end: {why}"))?;
    Ok(world)
}

fn quiet() -> Faults {
    Faults { fail_one_in: 0, max_latency: Duration::from_micros(300), ..Faults::default() }
}

/// The invariant: with its index tables built off its thread, the store reads every record
/// it was given, and after any crash every record that was durable, whatever was put, moved
/// or committed while a build was out, whenever the build came back or was lost, and wherever
/// in the commit that writes the table the power went. `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn an_index_built_off_the_thread_loses_no_record() {
    run_cases(96, |tc| {
        let ops = tc.draw(gs::vecs(op()).min_size(1).max_size(50));
        let seed = tc.draw(gs::integers());
        let faults = Faults {
            fail_one_in: [0, 0, 0, 23][tc.draw(gs::integers::<usize>().max_value(3))],
            max_latency: Duration::from_micros(tc.draw(gs::integers::<u64>().max_value(5000))),
            ..Faults::default()
        };
        if let Err(why) = runs(&ops, seed, faults.clone(), false, true)
            && !why.contains("injected by the simulation")
        {
            panic!("seed {seed}, {faults:?}, {ops:?}: {why}");
        }
    });
}

/// A build taken over sixty committed records, forty more committed while it is out, the
/// build handed back, and a clean reopen.
fn late_back() -> Vec<Op> {
    vec![
        Op::Put(0, 60),
        Op::Flush,
        Op::Take,
        Op::Run,
        Op::Put(60, 40),
        Op::Flush,
        Op::Back,
        Op::Reopen,
    ]
}

/// The planted-bug control: the head that names a table built off the thread names each
/// pack's end at the time of the write as indexed (as a table built in its commit rightly
/// does). The records committed while the build was out are then in no table and under the
/// end an open scans from: gone after a reopen. The run passes with nothing planted.
#[test]
fn a_head_naming_the_ends_at_the_write_loses_what_was_committed_meanwhile() {
    for seed in 0..8 {
        let world = runs(&late_back(), seed, quiet(), false, true)
            .unwrap_or_else(|why| panic!("seed {seed}: the store as it is must pass: {why}"));
        let counts = world.store.index_counts();
        assert_eq!(counts.lost + counts.dropped, 0, "seed {seed}: {counts:?}");
        let why = runs(&late_back(), seed, quiet(), true, true)
            .err()
            .unwrap_or_else(|| panic!("seed {seed}: the planted bug was not caught"));
        assert!(why.contains("and gone") || why.contains("did not open"), "seed {seed}: {why}");
    }
}

/// A table built off the thread is written and swapped in, the store's thread having walked
/// no row to build it; the same run on a store that builds its tables in its commits walks
/// them there (the control: the count is of the walk the worker took over).
#[test]
fn the_stores_thread_walks_no_row_for_a_table_built_off_it() {
    // No reopen: its counts are the opened store's.
    let ops: Vec<Op> = late_back().into_iter().filter(|it| !matches!(it, Op::Reopen)).collect();
    let mut world = World::new(1, quiet(), false, true);
    for op in &ops {
        world.step(op).and_then(|()| world.check()).unwrap();
    }
    let counts = world.store.index_counts();
    assert_eq!((counts.taken, counts.written, counts.loop_rows), (1, 1, 0), "{counts:?}");
    assert!(world.store.tiers[0].head.index.is_some(), "the head names the table");
    assert!(!world.store.index_building());

    let mut world = World::new(1, quiet(), false, false);
    for op in &ops {
        world.step(op).and_then(|()| world.check()).unwrap();
    }
    let counts = world.store.index_counts();
    assert_eq!(counts.taken, 0, "{counts:?}");
    assert!(counts.loop_rows >= 60, "a table built in a commit walks the rows there: {counts:?}");
}

/// The power goes after each completion of the commit that writes a built table, in turn
/// (the table's write, the packs' writes and syncs, the head): the store opens and reads
/// every record that was durable before the commit began, with the table or without it.
#[test]
fn a_crash_at_each_step_of_the_commit_that_writes_the_table_loses_nothing() {
    let before = [Op::Put(0, 60), Op::Flush, Op::Take, Op::Run, Op::Put(60, 40), Op::Flush, Op::Put(100, 30)];
    // The steps a whole commit takes, counted on a run that is left to finish.
    let mut steps = 0u64;
    for seed in 0..4 {
        for cut in 0.. {
            let mut world = World::new(seed, quiet(), false, true);
            for op in &before {
                world.step(op).and_then(|()| world.check()).unwrap();
            }
            let mut build = world.build.take().expect("a build was taken: sixty records were committed");
            build.run();
            let mut effects = Vec::new();
            world.store.index_put(build, &mut effects);
            let mut done = 0u64;
            let World { sim, store, .. } = &mut world;
            run_until_idle(sim, &mut effects, |completion, effects| {
                if store.owns(completion.tag) && done < cut {
                    done += 1;
                    store.complete(completion, effects);
                }
            })
            .expect("the effects end");
            steps = steps.max(done);
            world
                .reopen(true)
                .and_then(|()| world.check())
                .unwrap_or_else(|why| panic!("seed {seed}, cut after {cut} completions: {why}"));
            assert!(world.model.len() >= 100, "what two flushes made durable is still held");
            if done < cut {
                break;
            }
        }
    }
    assert!(steps >= 3, "the commit wrote a table, a pack and a head at least: {steps} steps");
}

/// A build lost with its worker is taken again; three lost in a row and the store asks for
/// no more and says so, still reading everything through its delta.
#[test]
fn builds_lost_in_a_row_end_in_a_problem_said_once() {
    let mut world = World::new(3, quiet(), false, true);
    for op in [Op::Put(0, 60), Op::Flush] {
        world.step(&op).unwrap();
    }
    for _ in 0..3 {
        world.step(&Op::Take).unwrap();
        assert!(world.build.is_some(), "a build is due and is handed out again after a loss");
        world.step(&Op::Lose).unwrap();
    }
    world.step(&Op::Take).unwrap();
    assert!(world.build.is_none(), "no build is handed out after three lost in a row");
    let said = world.store.problems().iter().filter(|it| it.contains("is not built any more")).count();
    assert_eq!(said, 1, "{:?}", world.store.problems());
    world.check().unwrap();
}
