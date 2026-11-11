//! Placement under crashes: outputs put whole and as derived values ([`Kind::Derived`]), placed
//! at a few paths, replaced, with a full `placed/`, clean reopens and crashes at any completion,
//! on the simulation ([`Sim`]), which tears what was not synced. Nothing a place asks for is
//! synced, so every crash leaves the whole files of `placed/` old, new, torn or gone and may
//! undo a rename onto an output's path; the oracle is what a build may rely on all the same.
//! The same harness as the race of two stores (`place_race`): the store's own state machine on
//! the simulated disk, Hegel drawing the run.
//!
//! The planted bugs ([`PlacePlant`]) each take one safeguard out; the oracle must catch each.

use hegel::TestCase;
use hegel::generators as gs;

use blaze_core::sim::SECTOR;

use super::*;

/// How long a derived value's file is, against its base's.
#[derive(Clone, Copy, Debug)]
enum Len {
    Same,
    Shorter,
    Longer,
}

#[derive(Clone, Debug)]
enum Op {
    /// A new output of `len` bytes put whole: past [`CLONE_FROM`] it is placed by cloning a
    /// whole file, under it written where it goes.
    Whole { seed: u64, len: u32 },
    /// A new derived value: `ranges` ranges written over the value `base` (of those so far
    /// that the store keeps a whole file of).
    Derived { base: u8, seed: u64, ranges: u8, len: Len },
    /// The value `value` placed at path `path`, as a build's job does: with `replace`, the
    /// whole file of what the path held is let go of first ([`Store::unplace`]). Then `steps`
    /// completions are delivered (none: all of them), so what follows (another place, a
    /// crash) comes at any step of it.
    Place { value: u8, path: u8, replace: bool, steps: Option<u8> },
    /// Everything in flight run to its end.
    Settle,
    /// `placed/` has room for `room` more bytes, until a crash: writes there past it fail.
    Fill { room: u32 },
    /// The power goes; the store is opened again.
    Crash,
    /// Everything in flight ended, the store dropped and opened again.
    Reopen,
}

#[derive(Clone, Debug)]
struct Case {
    seed: u64,
    latency_us: u64,
    ops: Vec<Op>,
}

const PATHS: [&str; 3] = ["one/build/a", "two/build/a", "one/build/b"];

/// A value put: its key, the bytes a place of it must give, and whether the store keeps a
/// whole file of it (so it can be a base).
struct Value {
    key: Key,
    bytes: Vec<u8>,
    whole: bool,
}

/// A place asked and not yet answered.
struct Asked {
    tag: Tag,
    path: usize,
    value: usize,
}

struct World {
    sim: Sim,
    /// The store; none only while it is closed to be opened again.
    store: Option<Store>,
    plant: PlacePlant,
    effects: Vec<Effect>,
    values: Vec<Value>,
    /// What each path holds: none, or exactly these bytes. Changed only by a place that
    /// ended well, and read off the disk after a crash (once it is found to be allowed).
    holds: [Option<Vec<u8>>; PATHS.len()],
    /// The value each path was last placed with, for the next place's [`Store::unplace`].
    last: [Option<usize>; PATHS.len()],
    /// Every value a place was ever asked of at each path.
    ever: [Vec<usize>; PATHS.len()],
    asked: Vec<Asked>,
}

fn path(n: usize) -> FileName {
    name(PATHS[n])
}

impl World {
    fn new(case: &Case, plant: PlacePlant) -> World {
        let latency = Duration::from_micros(case.latency_us);
        let mut sim = Sim::new(case.seed, Faults { max_latency: latency, ..Faults::default() });
        let mut store = open(&mut sim, "store");
        store.plant = plant;
        World {
            sim,
            store: Some(store),
            plant,
            effects: Vec::new(),
            values: Vec::new(),
            holds: [None, None, None],
            last: [None; PATHS.len()],
            ever: [Vec::new(), Vec::new(), Vec::new()],
            asked: Vec::new(),
        }
    }

    /// Delivers up to `steps` completions (none: until nothing is in flight), then takes what
    /// the store says of its places: one that ended well must have left its path holding its
    /// value's bytes, whole.
    fn pump(&mut self, steps: Option<u8>) -> Result<(), String> {
        let store = self.store.as_mut().expect("the store is open");
        let mut left = steps.map(u32::from);
        let mut done = Vec::new();
        // The bound, on the simulation's clock: a completion is at most the latency away.
        let end = blaze_core::wait::Deadline::after(self.sim.now(), Duration::from_secs(600));
        while left != Some(0) {
            self.sim.submit(&mut self.effects);
            if self.sim.in_flight() == 0 {
                break;
            }
            self.sim.poll(&mut done, end);
            if done.is_empty() {
                return Err(format!(
                    "{} effects in flight and none completed by {end:?}",
                    self.sim.in_flight()
                ));
            }
            for completion in done.drain(..) {
                if store.owns(completion.tag) {
                    store.complete(completion, &mut self.effects);
                }
                left = left.map(|it| it.saturating_sub(1));
            }
        }
        // What a completion asked for is asked, so a crash next finds it in flight.
        self.sim.submit(&mut self.effects);
        for placed in store.placed() {
            let Some(at) = self.asked.iter().position(|it| it.tag == placed.tag) else {
                return Err(format!("the store answered a place no one asked: {placed:?}"));
            };
            let Asked { path: n, value, .. } = self.asked.swap_remove(at);
            let want = &self.values[value].bytes;
            match placed.result {
                Ok(()) => {
                    let found = self.sim.contents(&path(n));
                    if !self.sim.exists(&path(n)) || found != *want {
                        return Err(format!(
                            "{} was placed with value {value} and holds {} bytes that are not \
                             its {}: first apart at {:?}",
                            PATHS[n],
                            found.len(),
                            want.len(),
                            found.iter().zip(want).position(|(a, b)| a != b)
                        ));
                    }
                    if placed.bytes.cloned + placed.bytes.copied != want.len() as u64 {
                        return Err(format!(
                            "{} was placed with {} bytes and reported as {:?}",
                            PATHS[n],
                            want.len(),
                            placed.bytes
                        ));
                    }
                    // clone: the model's copy of what the path holds.
                    self.holds[n] = Some(want.clone());
                    self.last[n] = Some(value);
                }
                // A place may fail (a full `placed/`): then its path is as it was.
                Err(_) => {}
            }
        }
        self.look()
    }

    /// The oracle between crashes: a path with no place in flight holds exactly what the last
    /// place that ended well left (or nothing); one with a place in flight holds that or the
    /// new value whole.
    fn look(&self) -> Result<(), String> {
        for n in 0..PATHS.len() {
            let there = self.sim.exists(&path(n)).then(|| self.sim.contents(&path(n)));
            if there == self.holds[n] {
                continue;
            }
            let new = self
                .asked
                .iter()
                .any(|it| it.path == n && Some(&self.values[it.value].bytes) == there.as_ref());
            if !new {
                return Err(format!(
                    "{} holds {:?} bytes: neither what it held ({:?} bytes) nor the whole of a \
                     value being placed there",
                    PATHS[n],
                    there.map(|it| it.len()),
                    self.holds[n].as_ref().map(Vec::len),
                ));
            }
        }
        Ok(())
    }

    /// The oracle after a crash: each path holds nothing, or the whole of a value once placed
    /// there, or, of a value short enough to be written where it goes and not cloned, any of
    /// its sectors (its write is not synced, as no part of a place is). What it holds is what
    /// it is then known to hold.
    fn look_after_crash(&mut self) -> Result<(), String> {
        for n in 0..PATHS.len() {
            let there = self.sim.exists(&path(n)).then(|| self.sim.contents(&path(n)));
            let allowed = match &there {
                None => true,
                Some(found) => self.ever[n].iter().any(|value| {
                    let Value { bytes, whole, .. } = &self.values[*value];
                    found == bytes
                        || (!whole
                            && found.len() <= bytes.len()
                            && found.chunks(SECTOR).enumerate().all(|(at, sector)| {
                                sector == &bytes[at * SECTOR..at * SECTOR + sector.len()]
                                    || sector.iter().all(|it| *it == 0)
                            }))
                }),
            };
            if !allowed {
                return Err(format!(
                    "after a crash {} holds {:?} bytes that are no value ever placed there",
                    PATHS[n],
                    there.map(|it| it.len())
                ));
            }
            self.last[n] = self.ever[n]
                .iter()
                .copied()
                .find(|value| Some(&self.values[*value].bytes) == there.as_ref());
            self.holds[n] = there;
        }
        Ok(())
    }

    fn put(&mut self, kind: Kind, bytes: &[u8], gives: Vec<u8>) -> Result<(), String> {
        let key = key(1000 + self.values.len() as u64);
        let store = self.store.as_mut().expect("the store is open");
        store.put(key, kind, bytes).map_err(|it| format!("a put was refused: {it:?}"))?;
        // Made durable before anything else: what a crash may lose here is placement's.
        store.flush(&mut self.effects);
        self.pump(None)?;
        let whole = kind == Kind::Derived || gives.len() as u64 >= CLONE_FROM;
        self.values.push(Value { key, bytes: gives, whole });
        Ok(())
    }

    /// The store dropped, as its process ends, and opened again.
    fn open(&mut self) {
        self.store = None;
        let mut store = open(&mut self.sim, "store");
        store.plant = self.plant;
        self.store = Some(store);
    }

    fn step(&mut self, op: &Op) -> Result<(), String> {
        match *op {
            Op::Whole { seed, len } => {
                let bytes = noise(len as usize, seed);
                // clone: the model's copy of the value.
                self.put(Kind::Output, &bytes, bytes.clone())?;
            }
            Op::Derived { base, seed, ranges, len } => {
                let bases: Vec<usize> =
                    (0..self.values.len()).filter(|it| self.values[*it].whole).collect();
                if bases.is_empty() {
                    return Ok(());
                }
                let base = &self.values[bases[usize::from(base) % bases.len()]];
                let (len, writes) = recipe_of(base.bytes.len() as u64, seed, ranges, len);
                let recipe = derived::encode(&base.key, len, &writes)
                    .map_err(|why| format!("the test drew a bad recipe: {why}"))?;
                let gives = derived::applied(&base.bytes, len, writes.iter().map(|(at, it)| (*at, it)));
                self.put(Kind::Derived, &recipe, gives)?;
            }
            Op::Place { value, path: n, replace, steps } => {
                if self.values.is_empty() {
                    return Ok(());
                }
                let (value, n) = (usize::from(value) % self.values.len(), usize::from(n) % PATHS.len());
                // One place at a path at a time, so one that ended well is judged alone.
                if self.asked.iter().any(|it| it.path == n) {
                    self.pump(None)?;
                }
                let key = self.values[value].key;
                let store = self.store.as_mut().expect("the store is open");
                if let (true, Some(before)) = (replace, self.last[n])
                    && before != value
                {
                    let before = self.values[before].key;
                    store.unplace(&before, &key, &mut self.effects);
                }
                let tag = store
                    .place(&key, path(n), &mut self.effects)
                    .ok_or_else(|| format!("the store has no value {value} to place"))?;
                self.asked.push(Asked { tag, path: n, value });
                self.ever[n].push(value);
                self.pump(steps)?;
            }
            Op::Settle => self.pump(None)?,
            Op::Fill { room } => self.sim.fill(name("store/placed"), u64::from(room)),
            Op::Crash => {
                self.sim.crash();
                self.effects.clear();
                self.asked.clear();
                self.open();
                self.look_after_crash()?;
            }
            Op::Reopen => {
                self.pump(None)?;
                self.open();
                self.look()?;
            }
        }
        match self.store.as_ref().and_then(Store::broken) {
            Some(broken) => Err(format!("the store stopped: {broken}")),
            None => Ok(()),
        }
    }
}

/// A recipe over a base of `base` bytes: its length and its ranges, in order and apart, each
/// of noise.
fn recipe_of(base: u64, seed: u64, ranges: u8, len: Len) -> (u64, Vec<(u64, Vec<u8>)>) {
    let mut rng = Rng::new(seed);
    let len = match len {
        Len::Same => base,
        Len::Shorter => base - rng.below(base / 2 + 1),
        Len::Longer => base + 1 + rng.below(3 * SECTOR as u64),
    }
    .max(4 * SECTOR as u64);
    let count = u64::from(ranges % 4);
    let room = len / count.max(1);
    let writes = (0..count)
        .map(|n| {
            let at = n * room + rng.below(room / 2);
            let bytes = 1 + rng.below((room / 2).min(3 * SECTOR as u64));
            (at, noise(bytes as usize, seed ^ (n + 1)))
        })
        .collect();
    (len, writes)
}

/// Runs `case` with `plant` planted; `Err` says where the oracle failed.
fn run(case: &Case, plant: PlacePlant) -> Result<(), String> {
    let mut world = World::new(case, plant);
    for (n, op) in case.ops.iter().enumerate() {
        world.step(op).map_err(|why| format!("at op {n} ({op:?}): {why}"))?;
    }
    world.pump(None).map_err(|why| format!("at the end: {why}"))
}

#[hegel::composite]
fn op(tc: &TestCase) -> Op {
    let byte = |tc: &TestCase| tc.draw(gs::integers::<u8>());
    match tc.draw(gs::integers::<u8>().max_value(15)) {
        0 | 1 => Op::Whole {
            seed: tc.draw(gs::integers()),
            // Mostly long enough to be cloned from a whole file; sometimes written in place.
            len: match tc.draw(gs::integers::<u8>().max_value(4)) {
                0 => tc.draw(gs::integers::<u32>().min_value(1).max_value(3000)),
                _ => tc.draw(gs::integers::<u32>().min_value(CLONE_FROM as u32).max_value(40 << 10)),
            },
        },
        2..5 => Op::Derived {
            base: byte(tc),
            seed: tc.draw(gs::integers()),
            ranges: byte(tc),
            len: [Len::Same, Len::Same, Len::Shorter, Len::Longer]
                [usize::from(tc.draw(gs::integers::<u8>().max_value(3)))],
        },
        5..11 => Op::Place {
            value: byte(tc),
            path: byte(tc),
            replace: tc.draw(gs::booleans()),
            steps: match tc.draw(gs::booleans()) {
                true => Some(tc.draw(gs::integers::<u8>().max_value(12))),
                false => None,
            },
        },
        11 => Op::Settle,
        12 => Op::Fill { room: tc.draw(gs::integers::<u32>().max_value(60 << 10)) },
        13 | 14 => Op::Crash,
        _ => Op::Reopen,
    }
}

#[hegel::composite]
fn case(tc: &TestCase) -> Case {
    Case {
        seed: tc.draw(gs::integers()),
        latency_us: tc.draw(gs::integers().min_value(1).max_value(2000)),
        ops: tc.draw(gs::vecs(op()).min_size(1).max_size(24)),
    }
}

/// The invariant: whatever is put, placed, replaced and derived, with `placed/` full or not,
/// and wherever the power goes, (1) a place that ends well leaves its path holding exactly
/// its value's bytes, so a whole file of `placed/` is cloned from only when its bytes are its
/// value's: one written or derived by this run, or one an earlier run left that hashes to its
/// name (a derived value's is never kept: it is made again from its base and its ranges); (2)
/// until a place ends well its path holds what it held or the new value whole, never a part;
/// (3) after a crash a path holds nothing, or the whole of a value once placed there, or, of
/// a value under [`CLONE_FROM`] bytes (written where it goes, not cloned, and not synced), any
/// of its sectors; and the store opens. `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn a_place_gives_its_values_bytes_whole_through_any_crash() {
    super::model_check::run_cases(96, |tc| {
        let case = tc.draw(case());
        if let Err(why) = run(&case, PlacePlant::None) {
            panic!("{case:?}: {why}");
        }
    });
}

/// A whole output, long enough to be cloned from a whole file.
fn whole(seed: u64) -> Op {
    Op::Whole { seed, len: (CLONE_FROM as u32) + 9000 }
}

fn place(value: u8, path: u8) -> Op {
    Op::Place { value, path, replace: true, steps: None }
}

/// `ops` on each of `seeds`, as it is (which must pass) and with `plant` planted: the first
/// seed on which the oracle fails the planted store, and how.
fn caught(ops: &[Op], seeds: std::ops::Range<u64>, plant: PlacePlant) -> Option<(u64, String)> {
    seeds.into_iter().find_map(|seed| {
        // clone: a test's ops, once a seed.
        let case = Case { seed, latency_us: 300, ops: ops.to_vec() };
        assert_eq!(run(&case, PlacePlant::None), Ok(()), "seed {seed}: the store as it is must pass");
        run(&case, plant).err().map(|why| (seed, why))
    })
}

/// The planted-bug control: a whole file found after a crash and cloned from without its
/// bytes being hashed against its name. The file was written with no sync, so on some seed
/// the crash tears it at its full length, and the place after gives the torn bytes.
#[test]
fn the_oracle_catches_a_placed_file_used_after_a_crash_unchecked() {
    let ops = [whole(1), place(0, 0), Op::Crash, place(0, 1)];
    let (seed, why) = caught(&ops, 0..64, PlacePlant::UncheckedFile)
        .expect("the oracle missed a torn whole file cloned from unchecked");
    assert!(why.contains("was placed with value 0 and holds"), "seed {seed}: {why}");
}

/// The planted-bug control: a derived value's whole file renamed to its name, and cloned to
/// the output's path, before its ranges are written. On some seed the clone comes first and
/// the path holds the base's bytes where a range goes.
#[test]
fn the_oracle_catches_a_rename_before_the_clones_bytes_are_complete() {
    let derived = Op::Derived { base: 0, seed: 7, ranges: 3, len: Len::Same };
    let ops = [whole(2), place(0, 0), derived, place(1, 0)];
    let (seed, why) = caught(&ops, 0..64, PlacePlant::RenameBeforeRanges)
        .expect("the oracle missed a derived file cloned before its ranges were written");
    assert!(why.contains("was placed with value 1 and holds"), "seed {seed}: {why}");
}

/// The planted-bug control: a derive that leaves out a range. The path holds the base's
/// bytes there.
#[test]
fn the_oracle_catches_a_derive_that_skipped_a_range() {
    let derived = Op::Derived { base: 0, seed: 9, ranges: 2, len: Len::Longer };
    let ops = [whole(3), place(0, 0), derived, place(1, 0)];
    let (seed, why) = caught(&ops, 0..4, PlacePlant::SkipsARange)
        .expect("the oracle missed a derive that skipped a range");
    assert!(why.contains("was placed with value 1 and holds"), "seed {seed}: {why}");
}

/// The planted-bug control: a derived value's whole file an earlier run left, kept as the
/// store opens and cloned from as it is found. Its ranges were written with no sync, so on
/// some seed the crash leaves the base's clone with a part of them under the derived value's
/// name, at its length, and the place after gives that. (As it is, the store never takes one:
/// a derived value's file has no hash to be checked against, so it is made again.)
#[test]
fn the_oracle_catches_a_derived_file_accepted_as_a_crash_left_it() {
    let derived = Op::Derived { base: 0, seed: 11, ranges: 3, len: Len::Same };
    let ops = [whole(4), place(0, 0), derived, place(1, 1), Op::Crash, place(1, 2)];
    let (seed, why) = caught(&ops, 0..96, PlacePlant::KeepsDerived)
        .expect("the oracle missed a torn derived file cloned from as a crash left it");
    assert!(why.contains("was placed with value 1 and holds"), "seed {seed}: {why}");
}

/// One whole output of `len` bytes under key 1, placed at `one/build/a`, and under key 2 a
/// derived value of it: `ranges` ranges of `range` bytes written over it, at `len`. The
/// store, the base's bytes and the derived file's.
fn derived_once(sim: &mut Sim, len: usize, ranges: u64, range: usize) -> (Store, Vec<u8>, Vec<u8>) {
    let mut store = open(sim, "store");
    let base = noise(len, 77);
    store.put(key(1), Kind::Output, &base).unwrap();
    flush(sim, &mut store);
    let mut effects = Vec::new();
    store.place(&key(1), name("one/build/a"), &mut effects).unwrap();
    drive(sim, &mut store, &mut effects);
    assert_eq!(store.placed()[0].result, Ok(()));
    let writes: Vec<(u64, Vec<u8>)> = (0..ranges)
        .map(|n| ((1 + 2 * n) * (len as u64 / (2 * ranges + 1)) / 4096 * 4096, noise(range, 78 + n)))
        .collect();
    let recipe = derived::encode(&key(1), len as u64, &writes).unwrap();
    store.put(key(2), Kind::Derived, &recipe).unwrap();
    flush(sim, &mut store);
    let gives = derived::applied(&base, len as u64, writes.iter().map(|(at, it)| (*at, it)));
    (store, base, gives)
}

/// The one place asked, driven to its end.
fn placed_one(sim: &mut Sim, store: &mut Store, effects: &mut Vec<Effect>) -> Placed {
    drive(sim, store, effects);
    let placed = store.placed();
    assert_eq!(placed.len(), 1, "{placed:?}");
    placed.into_iter().next().unwrap()
}

/// A derive writes its ranges and nothing else: an image of 10 MiB that differs from the last
/// by one block of 4 KiB is placed by writing 4 KiB, with none of the base read (or hashed:
/// the store never has its bytes in hand). The simulation counts every byte an effect wrote
/// to a file and read from one.
#[test]
fn a_derive_writes_its_ranges_and_reads_none_of_its_base() {
    const LEN: usize = 10 << 20;
    let mut sim = Sim::new(31, quiet());
    let (mut store, base, gives) = derived_once(&mut sim, LEN, 1, 4096);
    let (wrote, read) = (sim.bytes_written(), sim.bytes_read());
    // As a build's job replaces the image at its path: the old one's whole file is let go
    // of, and the new one placed.
    let mut effects = Vec::new();
    store.unplace(&key(1), &key(2), &mut effects);
    store.place(&key(2), name("one/build/a"), &mut effects).unwrap();
    let placed = placed_one(&mut sim, &mut store, &mut effects);
    assert_eq!(placed.result, Ok(()));
    assert_eq!(
        placed.bytes,
        PlacedBytes { cloned: LEN as u64, written: 4096, copied: 0 },
        "what the place reports is what was written"
    );
    assert_eq!(sim.contents(&name("one/build/a")), gives);
    let (wrote, read) = (sim.bytes_written() - wrote, sim.bytes_read() - read);
    assert!(
        (4096..=2 * 4096).contains(&wrote),
        "a derive of one 4 KiB range wrote {wrote} bytes: its range, and at most a constant"
    );
    assert!(read < 64 << 10, "a derive read {read} bytes: none of its 10 MiB base is read");
    // The base's whole file went once the derive had cloned it: one whole file is kept.
    assert!(!sim.exists(&name(&format!("store/placed/{:x}", content_of(&base)))));
    assert!(sim.exists(&name(&format!("store/placed/{:x}{DERIVED_SUFFIX}", key(2).0))));
    assert!(store.problems().is_empty(), "{:?}", store.problems());
}

/// A derived value across a restart: its recipe is durable as every value is, and the whole
/// file the store kept of it is not taken from the earlier run. The first place after the
/// open makes it again (the base's whole file, kept, is hashed against its name and cloned;
/// only the ranges are written) and gives byte for byte what a whole put of the same file
/// gives (persisted-state-round-trips). After a crash in place of the clean stop, on every
/// seed, the same bytes: what the crash tore is checked or made again.
#[test]
fn a_derived_value_places_the_same_bytes_after_a_restart_as_a_whole_put_of_them() {
    const LEN: usize = 96 << 10;
    for seed in 0..48 {
        let mut sim = Sim::new(seed, quiet());
        let (mut store, base, gives) = derived_once(&mut sim, LEN, 3, 700);
        let mut effects = Vec::new();
        store.place(&key(2), name("two/build/a"), &mut effects).unwrap();
        assert_eq!(placed_one(&mut sim, &mut store, &mut effects).result, Ok(()));
        assert_eq!(sim.contents(&name("two/build/a")), gives, "seed {seed}");
        let crashed = seed % 2 == 1;
        drop(store);
        if crashed {
            sim.crash();
        }
        let mut store = open(&mut sim, "store");
        let found = store.placed_on_open();
        if !crashed {
            assert_eq!(
                (found.kept, found.kept_bytes, found.swept),
                (1, LEN as u64, 1),
                "the base's whole file is kept to be checked; the derived value's goes"
            );
        }
        assert_eq!(store.reader().derivable(&enter(), &key(2)), Derivable::Yes);
        let mut effects = Vec::new();
        store.place(&key(2), name("three/build/a"), &mut effects).unwrap();
        let placed = placed_one(&mut sim, &mut store, &mut effects);
        assert_eq!(placed.result, Ok(()), "seed {seed}");
        if !crashed {
            assert_eq!(
                placed.bytes,
                PlacedBytes { cloned: LEN as u64, written: 3 * 700, copied: 0 },
                "the base's file is checked, not written again; the ranges are"
            );
        }
        // The same file put whole in another store, placed: the bytes a derive must give.
        let mut other = open_with(
            &mut sim,
            Config {
                dir: name("other"),
                layers: Vec::new(),
                tags: 3 << 40,
                load_tags: (3 << 40) + (1 << 32),
                loader: crate::Loader::Pumped,
            },
        );
        other.put(key(3), Kind::Output, &gives).unwrap();
        flush(&mut sim, &mut other);
        let mut effects = Vec::new();
        other.place(&key(3), name("four/build/a"), &mut effects).unwrap();
        assert_eq!(placed_one(&mut sim, &mut other, &mut effects).result, Ok(()));
        let (derived, put_whole) =
            (sim.contents(&name("three/build/a")), sim.contents(&name("four/build/a")));
        assert!(derived == put_whole && derived == gives, "seed {seed}: the two files differ");
        assert_ne!(derived, base);
    }
}

/// What can be a base and what cannot, said in a type: a value not there, one too short to be
/// kept as a whole file, and the last of a chain at its bound are no base, and the caller
/// puts its whole file; a place of a recipe whose base is gone fails naming both.
#[test]
fn a_value_that_is_no_base_is_said_so_and_a_recipe_of_one_is_refused_by_name() {
    let mut sim = Sim::new(5, quiet());
    let mut store = open(&mut sim, "store");
    let long = noise(CLONE_FROM as usize + 100, 1);
    store.put(key(1), Kind::Output, &long).unwrap();
    store.put(key(2), Kind::Output, b"short").unwrap();
    // A chain: each derived from the one before, with nothing written.
    let most = u64::from(derived::CHAIN_MOST / 2);
    for n in 0..=most {
        let base = if n == 0 { key(1) } else { key(99 + n) };
        let recipe = derived::encode(&base, long.len() as u64, &[]).unwrap();
        store.put(key(100 + n), Kind::Derived, &recipe).unwrap();
    }
    let orphan = derived::encode(&key(9), 5000, &[(0, vec![1; 8])]).unwrap();
    store.put(key(8), Kind::Derived, &orphan).unwrap();
    flush(&mut sim, &mut store);
    let (reader, guard) = (store.reader(), enter());
    assert_eq!(reader.derivable(&guard, &key(1)), Derivable::Yes);
    assert_eq!(reader.derivable(&guard, &key(100)), Derivable::Yes);
    assert_eq!(reader.derivable(&guard, &key(9)), Derivable::No(NoBase::Absent));
    assert_eq!(
        reader.derivable(&guard, &key(2)),
        Derivable::No(NoBase::NotWhole { kind: Kind::Output, len: 5 })
    );
    assert_eq!(reader.derivable(&guard, &key(99 + most)), Derivable::Yes);
    assert_eq!(
        reader.derivable(&guard, &key(100 + most)),
        Derivable::No(NoBase::Chain(most as u32))
    );
    drop(guard);
    // The longest chain a caller that asks can make places, each file made from the last.
    let mut effects = Vec::new();
    store.place(&key(100 + most), name("one/build/a"), &mut effects).unwrap();
    assert_eq!(placed_one(&mut sim, &mut store, &mut effects).result, Ok(()));
    assert_eq!(sim.contents(&name("one/build/a")), long);
    // A recipe whose base the store has none of: refused, naming the value and its base.
    let mut effects = Vec::new();
    store.place(&key(8), name("one/build/b"), &mut effects).unwrap();
    let refused = placed_one(&mut sim, &mut store, &mut effects).result.unwrap_err();
    assert!(
        refused.contains(&format!("{:?}", key(8))) && refused.contains(&format!("{:?}", key(9))),
        "{refused}"
    );
    assert!(!sim.exists(&name("one/build/b")));
}
