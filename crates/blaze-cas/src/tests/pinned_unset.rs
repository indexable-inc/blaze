//! Readers under no pool budget, on the simulation (issue #2178): nothing sets `Tag::Pool`'s
//! budget, so a running daemon's readers pin with no bound ([`crate::pool::counts::pinned_bound`]),
//! and `Store::tidy` lets the store's table of pins go once it holds
//! [`crate::pool::counts::pins_most`]. The hang this is for: a restarted daemon's codegen task
//! read a whole unit under one guard, so no table of pins retired while it ran could be freed,
//! and once the process's pins reached the 1 GiB an unset budget then meant, every further pin
//! waited at the bound and its read failed.
//!
//! Each reader is a task of its own on the test's thread, parked between its steps as a fiber's
//! job is ([`blaze_reclaim::park`]): it holds one guard from its first read to its end, so the
//! schedule (which reader reads what, when one ends, when the store tidies) is drawn from the
//! case's tape and nothing runs on another thread. The unset budget's numbers are divided by
//! `1 << SHIFT` ([`Unset`]) so a store of a few megabytes reaches them.
//!
//! The oracle: no read waits at a bound or is refused, and each reads its value; `tidy` lets
//! the table go exactly when it holds more than `pins_most`; no table is freed while a reader
//! that could have read from it runs; and once every such reader has ended, it is freed. The
//! planted bug ([`Unset::old_bound`]) is the mapping this replaced, an unset budget as a bound
//! of 1 GiB (here 1 MiB).

use std::{collections::BTreeSet, num::NonZeroUsize};

use blaze_sim::{Config as SimConfig, Tape};

use super::*;
use crate::pool::counts::{self, PINNED_UNSET_PLANTED, UNSET, Unset};

/// The unset budget's numbers are divided by `1 << SHIFT`: `pins_most` 256 KiB (four pages),
/// the planted bound 1 MiB.
const SHIFT: u32 = 10;
/// Keys are numbered below this; each value is 40,000 bytes, about a page of the pool.
const KEYS: u64 = 48;
/// Readers running at once, at most.
const READERS: u8 = 4;

fn value(n: u64) -> Vec<u8> {
    noise(40_000, n)
}

#[derive(Clone, Copy, Debug)]
enum Op {
    /// A reader begins its task in `slot`, if the slot is free.
    Begin(u8),
    /// The reader in `slot` reads `count` keys from `from` on, under its one guard.
    Read(u8, u64, u8),
    /// The reader in `slot` ends its task.
    End(u8),
    /// The store's `tidy`, as every `&mut` entry point runs it.
    Tidy,
}

fn op(tape: &Tape) -> Op {
    let slot = || tape.int_in(0..=READERS - 1, "slot");
    match tape.int_in(0..=9u8, "op") {
        0 | 1 => Op::Begin(slot()),
        2..=6 => Op::Read(slot(), tape.int_in(0..=KEYS - 1, "from"), tape.int_in(1..=8u8, "count")),
        7 => Op::End(slot()),
        _ => Op::Tidy,
    }
}

/// A reader's task, parked off the thread between its steps.
struct Task {
    id: u64,
    guard: Guard,
    parked: NonZeroUsize,
}

/// A table of pins the store let go, and the readers that ran when it did: it is freed once
/// they have all ended, and not before.
#[derive(Debug)]
struct Retired {
    pinned: u64,
    readers: BTreeSet<u64>,
}

/// One case at a time in the process: a reader's parked task pins the process's epoch, so two
/// cases at once would hold each other's tables of pins.
static ONE_CASE: blaze_core::lock::Lock<()> =
    blaze_core::lock::Lock::new("test.pinned_unset.case", ());

struct World {
    _sim: Sim,
    store: Store,
    reader: Reader,
    tasks: [Option<Task>; READERS as usize],
    began: u64,
    retired: Vec<Retired>,
    /// The most bytes the store's tables (the one in use and those let go, not yet freed) pinned
    /// at once.
    peak: u64,
    /// Tables freed.
    freed: usize,
    /// Held to the end, the store dropped before it.
    _case: blaze_core::lock::LockGuard<'static, ()>,
}

impl World {
    fn new(seed: u64, planted: bool) -> World {
        let case = ONE_CASE.lock();
        assert_eq!(counts::budget(), u64::MAX, "no budget is set in a test process");
        assert!(!blaze_reclaim::in_task(), "the test's thread is in no task");
        UNSET.with(|it| it.set(Unset { shift: SHIFT, old_bound: planted }));
        let mut sim = Sim::new(seed, quiet());
        let mut store = open(&mut sim, "store");
        for n in 0..KEYS {
            store.put(key(n), Kind::Code, &value(n)).unwrap();
        }
        flush(&mut sim, &mut store);
        let reader = store.reader();
        let mut world = World {
            _sim: sim,
            store,
            reader,
            tasks: [const { None }; READERS as usize],
            began: 0,
            retired: Vec::new(),
            peak: 0,
            freed: 0,
            _case: case,
        };
        world.settle().expect("the store's limbo empties with no reader running");
        world
    }

    fn pinned_now(&self) -> u64 {
        self.store.shared.owned(Held::writer(&*self.store.shared)).pinned()
    }

    fn running(&self) -> BTreeSet<u64> {
        self.tasks.iter().flatten().map(|it| it.id).collect()
    }

    /// Tables let go whose readers still run: what the store must not have freed.
    fn held(&self) -> usize {
        let running = self.running();
        self.retired.iter().filter(|it| !it.readers.is_disjoint(&running)).count()
    }

    /// Tidies until the store's limbo holds only the tables a running reader holds. The grace
    /// is the process's (another test's thread in a task delays it), so it is asked again,
    /// for at most about ten seconds.
    fn settle(&mut self) -> Result<(), String> {
        let held = self.held();
        for attempt in 0..20_000 {
            if self.store.limbo.len() == held {
                let running = self.running();
                let before = self.retired.len();
                self.retired.retain(|it| !it.readers.is_disjoint(&running));
                self.freed += before - self.retired.len();
                return Ok(());
            }
            match attempt < 10_000 {
                true => std::thread::yield_now(),
                false => std::thread::sleep(Duration::from_millis(1)),
            }
            self.store.tidy();
        }
        Err(format!(
            "{} tables of pins are let go and not freed, {held} of them held by running readers {:?}: \
             the pins of readers that ended were not given back ({:?})",
            self.store.limbo.len(),
            self.running(),
            self.retired
        ))
    }

    fn step(&mut self, op: Op) -> Result<(), String> {
        match op {
            Op::Begin(slot) => {
                if self.tasks[slot as usize].is_none() {
                    let guard = enter();
                    let parked = blaze_reclaim::park().expect("a task was just begun");
                    self.began += 1;
                    self.tasks[slot as usize] = Some(Task { id: self.began, guard, parked });
                }
            }
            Op::Read(slot, from, count) => {
                let Some(task) = &mut self.tasks[slot as usize] else { return Ok(()) };
                blaze_reclaim::unpark(task.parked);
                let mut failed = None;
                for n in (from..from + u64::from(count)).map(|it| it % KEYS) {
                    let read = match self.reader.get(&task.guard, &key(n)) {
                        Ok(Some(entry)) if *entry.bytes == *value(n) => Ok(()),
                        Ok(Some(_)) => Err("other bytes".to_owned()),
                        Ok(None) => Err("no value".to_owned()),
                        Err(corrupt) => Err(corrupt.to_string()),
                    };
                    if let Err(why) = read {
                        failed = Some(format!("reader {} read key {n}: {why}", task.id));
                        break;
                    }
                }
                task.parked = blaze_reclaim::park().expect("the reader's task was put back");
                if let Some(why) = failed {
                    return Err(why);
                }
                let live = self.pinned_now() + self.retired.iter().map(|it| it.pinned).sum::<u64>();
                self.peak = self.peak.max(live);
            }
            Op::End(slot) => {
                if let Some(task) = self.tasks[slot as usize].take() {
                    blaze_reclaim::unpark(task.parked);
                    drop(task.guard);
                }
            }
            Op::Tidy => {
                let owned = self.store.shared.owned(Held::writer(&*self.store.shared));
                let (pinned, bytes) = (owned.pinned(), owned.bytes());
                let most = counts::pins_most(counts::budget());
                self.store.tidy();
                let after = self.pinned_now();
                if pinned > most || bytes > OWNED_MOST {
                    if after != 0 {
                        return Err(format!(
                            "tidy kept a table pinning {pinned}, over pins_most {most}"
                        ));
                    }
                    self.retired.push(Retired { pinned, readers: self.running() });
                } else if after != pinned {
                    return Err(format!(
                        "tidy let a table pinning {pinned} go, under pins_most {most}"
                    ));
                }
                if self.store.limbo.len() < self.held() {
                    return Err(format!(
                        "the store freed a table of pins while a reader that could read from it runs \
                         ({} in limbo, {} held by {:?})",
                        self.store.limbo.len(),
                        self.held(),
                        self.running()
                    ));
                }
                self.settle()?;
            }
        }
        Ok(())
    }

    /// Every reader ends and the store tidies: every table let go is freed.
    fn end(&mut self) -> Result<(), String> {
        for slot in 0..READERS {
            self.step(Op::End(slot))?;
        }
        self.step(Op::Tidy)?;
        match self.retired.is_empty() {
            true => Ok(()),
            false => Err(format!("tables let go and never freed: {:?}", self.retired)),
        }
    }
}

impl Drop for World {
    /// A failed case's readers end too, so no parked task pins the process's epoch.
    fn drop(&mut self) {
        for task in self.tasks.iter_mut().filter_map(Option::take) {
            blaze_reclaim::unpark(task.parked);
            drop(task.guard);
        }
        UNSET.with(|it| it.set(Unset::default()));
    }
}

/// Runs `ops` with the oracle after each; `Err` says what it found and after which op.
fn runs(ops: &[Op], seed: u64, planted: bool) -> Result<World, String> {
    let mut world = World::new(seed, planted);
    for (at, op) in ops.iter().enumerate() {
        world.step(*op).map_err(|why| format!("op {at} ({op:?}): {why}"))?;
    }
    world.end().map_err(|why| format!("at the end: {why}"))?;
    Ok(world)
}

/// The invariant, under no pool budget: no reader waits at a bound or is refused whatever
/// the readers running at once pin between them, `tidy` lets the table of pins go once it
/// holds more than `pins_most`, and a table let go is freed once every reader that ran then
/// has ended, and not before. `BLAZE_SIM_SEEDS=<n>` runs it longer.
#[test]
fn readers_under_no_budget_never_wait_and_ended_readers_pins_go_at_tidy() {
    blaze_sim::check(SimConfig::cases(48), |tape| {
        let ops = tape.vec(1..=60, "ops", op);
        let seed = tape.next_u64("sim seed");
        if let Err(why) = runs(&ops, seed, false) {
            panic!("seed {seed}, {ops:?}: {why}");
        }
    });
}

/// The hang's shape: one reader reads the whole unit under one guard while others begin, read
/// and end around it, and the store tidies between, so every table let go while it runs is
/// held until it ends.
fn directed() -> Vec<Op> {
    let mut ops = vec![Op::Begin(0), Op::Begin(1), Op::Read(1, 0, 6), Op::Read(0, 0, 8), Op::Tidy];
    ops.extend([
        Op::Read(0, 8, 8),
        Op::Begin(2),
        Op::Read(2, 30, 6),
        Op::Tidy,
        Op::End(1),
        Op::Tidy,
    ]);
    for from in [16, 24, 32, 40] {
        ops.extend([Op::Read(0, from, 8), Op::Tidy]);
    }
    ops.extend([
        Op::End(2),
        Op::Begin(3),
        Op::Read(3, 0, 8),
        Op::End(3),
        Op::Tidy,
        Op::End(0),
        Op::Tidy,
    ]);
    ops
}

/// The directed run passes, its readers pin more at once than the planted bound (the 1 GiB an
/// unset budget was, scaled), and tables are let go and freed: so the control below tests
/// readers that would have met that bound.
#[test]
fn the_directed_run_pins_past_the_old_bound_and_frees_what_ended_readers_held() {
    let world = runs(&directed(), 5, false).unwrap_or_else(|why| panic!("{why}"));
    let old = PINNED_UNSET_PLANTED >> SHIFT;
    assert!(
        world.peak > old,
        "the readers pinned {} at most, under the old bound {old}",
        world.peak
    );
    assert!(world.freed > 1, "{} tables freed", world.freed);
}

/// The planted-bug control: an unset budget as the 1 GiB bound it was (scaled to 1 MiB). The
/// reader holding its guard for the whole unit reaches it, and its read waits and is refused.
#[test]
fn the_oracle_catches_an_unset_budget_as_a_bound() {
    for seed in 0..2 {
        let why = runs(&directed(), seed, true)
            .err()
            .unwrap_or_else(|| panic!("seed {seed}: the oracle missed the old bound"));
        assert!(why.contains("its bound is 1048576"), "seed {seed}: {why}");
    }
}
