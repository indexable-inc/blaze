//! Placement under crashes: outputs put whole and as derived values ([`Kind::Derived`]), placed
//! at a few paths, replaced, with a full `placed/`, clean reopens and crashes at any completion,
//! on the simulation ([`Sim`]), which tears what was not synced. Nothing a place asks for is
//! synced ([`blaze_core::Durable::No`]: an output is derived data, placed again by the build
//! after a power failure), so every crash leaves the whole files of `placed/` old, new, torn
//! or gone, may undo a rename onto an output's path, and may tear an output written where it
//! goes; the oracle is what a build may rely on all the same.
//! The same harness as the race of two stores (`place_race`): the store's own state machine on
//! the simulated disk, Hegel drawing the run.
//!
//! The simulation also charges a clone of a file with data not written back the write-back
//! it forces ([`Sim::bytes_forced`]): on a real disk that is what the clone waits for, the
//! whole file's worth after a whole write. The store clones only a file whose write-back it
//! saw complete, so the count stays 0 in every run, and the counter tests below say what an
//! edit's placement writes.
//!
//! The planted bugs ([`PlacePlant`]) each take one safeguard out; the oracle must catch each.

use std::collections::BTreeSet;

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
    /// The value `value` placed at path `path`, as a build's job does: the whole file of what
    /// the path held is let go of as it is answered, unless another path names it
    /// ([`crate::whole`]). Then `steps` completions are delivered (none: all of them), so what
    /// follows (another place, a crash) comes at any step of it.
    Place { value: u8, path: u8, steps: Option<u8> },
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

/// A value put: its key, the bytes a place of it must give, whether the store keeps a whole
/// file of it (so it can be a base), and for a derived value the value it is derived from.
struct Value {
    key: Key,
    bytes: Vec<u8>,
    whole: bool,
    base: Option<usize>,
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
    /// The value each path was last placed with: what names its whole file ([`World::bounded`]).
    last: [Option<usize>; PATHS.len()],
    /// Whether each path was placed well since the last crash: until it is, the listing a
    /// crash left may still name a file of a value it held before.
    fresh: [bool; PATHS.len()],
    /// `placed/` was ever filled: a whole file's write may then have failed, so a path's file
    /// need not be there.
    filled: bool,
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
        store.names.plant_one_name = plant == PlacePlant::OneName;
        World {
            sim,
            store: Some(store),
            plant,
            effects: Vec::new(),
            values: Vec::new(),
            holds: [None, None, None],
            last: [None; PATHS.len()],
            fresh: [true; PATHS.len()],
            filled: false,
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
                    self.fresh[n] = true;
                }
                // A place may fail (a full `placed/`): then its path is as it was.
                Err(_) => {}
            }
        }
        self.look()?;
        match steps.is_none() && self.asked.is_empty() {
            true => self.bounded(),
            false => Ok(()),
        }
    }

    /// The name in `placed/` of the whole file value `value` is placed from, if it has one.
    fn whole_name(&self, value: usize) -> Option<String> {
        let Value { key, bytes, whole, base } = &self.values[value];
        match (whole, base) {
            (false, _) => None,
            (true, Some(_)) => Some(format!("store/placed/{:x}{}", key.0, crate::DERIVED_SUFFIX)),
            (true, None) => Some(format!("store/placed/{:x}", content_of(bytes))),
        }
    }

    /// The oracle of the store's bound on `placed/` ([`crate::whole`]), with nothing in
    /// flight: every whole file there is the file of a value a path holds, or (a path not
    /// placed since a crash, whose name the listing the crash left may hold) of one it was
    /// ever placed with; and, `placed/` never filled, every path placed since the last crash
    /// with a whole output names a file that is there, whatever was placed at the other paths.
    fn bounded(&mut self) -> Result<(), String> {
        let mut listed = None;
        let mut effects = vec![Effect::List { dir: name("store/placed"), tag: Tag(u64::MAX - 7) }];
        run_until_idle(&mut self.sim, &mut effects, |completion, _| {
            if completion.tag == Tag(u64::MAX - 7) {
                listed = Some(completion.done);
            }
        })
        .map_err(|why| format!("listing placed/: {why:?}"))?;
        let found: Vec<String> = match listed {
            Some(Done::Listed(found)) => found.into_iter().map(|(file, _)| file.as_str().to_owned()).collect(),
            // None yet: nothing placed by cloning.
            _ => Vec::new(),
        };
        let mut allowed = BTreeSet::new();
        // A home ([`crate::home`]) stays while its record does, whatever path names it.
        let store = self.store.as_ref().expect("the store is open");
        allowed.extend(
            store.tiers[0]
                .externals
                .iter()
                .filter(|(_, it)| it.placed && !it.dropped)
                .map(|(content, _)| format!("store/placed/{content:x}")),
        );
        for n in 0..PATHS.len() {
            let values: Vec<usize> = match self.fresh[n] {
                true => self.last[n].into_iter().collect(),
                false => self.ever[n].clone(),
            };
            allowed.extend(values.into_iter().filter_map(|value| self.whole_name(value)));
        }
        for file in &found {
            let leaf = file.rsplit('/').next().unwrap_or("");
            let whole = crate::content_named(leaf).is_some()
                || leaf.strip_suffix(crate::DERIVED_SUFFIX).is_some_and(|it| crate::content_named(it).is_some());
            if whole && !allowed.contains(file) {
                return Err(format!("{file} is in placed/ and no path's value is placed from it"));
            }
        }
        for n in 0..PATHS.len() {
            let Some(value) = self.last[n] else { continue };
            if self.filled || !self.fresh[n] || self.values[value].base.is_some() {
                continue;
            }
            if let Some(file) = self.whole_name(value)
                && !found.contains(&file)
            {
                return Err(format!("{file} was removed while {} names it", PATHS[n]));
            }
        }
        Ok(())
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
    /// there, or what a place of one that the power cut short may leave, since no part of a
    /// place is synced: of a value written where it goes, any of its sectors; of a derived
    /// value made of a clone of its base with its ranges written, the base's bytes with any
    /// of the value's written over them (each byte the value's, the base's, or a zero where
    /// the file grew), no longer than the longer of the two. Never bytes of anything else.
    /// What it holds is what it is then known to hold.
    fn look_after_crash(&mut self) -> Result<(), String> {
        for n in 0..PATHS.len() {
            let there = self.sim.exists(&path(n)).then(|| self.sim.contents(&path(n)));
            let allowed = match &there {
                None => true,
                Some(found) => self.ever[n].iter().any(|value| {
                    let Value { bytes, base, .. } = &self.values[*value];
                    let sectors = found.len() <= bytes.len()
                        && found.chunks(SECTOR).enumerate().all(|(at, sector)| {
                            sector == &bytes[at * SECTOR..at * SECTOR + sector.len()]
                                || sector.iter().all(|it| *it == 0)
                        });
                    let over_base = base.is_some_and(|base| {
                        let base = &self.values[base].bytes;
                        found.len() <= bytes.len().max(base.len())
                            && found.iter().enumerate().all(|(at, it)| {
                                bytes.get(at) == Some(it) || base.get(at) == Some(it) || *it == 0
                            })
                    });
                    found == bytes || sectors || over_base
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

    fn put(
        &mut self,
        kind: Kind,
        bytes: &[u8],
        gives: Vec<u8>,
        base: Option<usize>,
    ) -> Result<(), String> {
        let key = key(1000 + self.values.len() as u64);
        let store = self.store.as_mut().expect("the store is open");
        store.put(key, kind, bytes).map_err(|it| format!("a put was refused: {it:?}"))?;
        // Made durable before anything else: what a crash may lose here is placement's.
        store.flush(&mut self.effects);
        self.pump(None)?;
        let whole = kind == Kind::Derived || gives.len() as u64 >= CLONE_FROM;
        self.values.push(Value { key, bytes: gives, whole, base });
        Ok(())
    }

    /// The store dropped, as its process ends, and opened again; `Err` if it does not open.
    fn open(&mut self) -> Result<(), String> {
        self.store = None;
        let mut effects = Vec::new();
        let mut store = Store::open(config("store"), &mut effects);
        drive(&mut self.sim, &mut store, &mut effects);
        if !store.ready() {
            return Err(format!("the store does not open: {:?}", store.broken()));
        }
        store.plant = self.plant;
        store.names.plant_one_name = self.plant == PlacePlant::OneName;
        self.store = Some(store);
        Ok(())
    }

    /// The oracle of homes ([`crate::home`]), after an open: every output put reads its bytes,
    /// whole, from the pack's copy or from the placed file its record names; never neither,
    /// never a file a crash tore or took. (Puts are flushed before the next op, so none is
    /// lost.)
    fn homes(&self) -> Result<(), String> {
        let store = self.store.as_ref().expect("the store is open");
        for (n, value) in self.values.iter().enumerate() {
            if value.base.is_some() {
                continue;
            }
            match store.get(&value.key) {
                Ok(Some(entry)) if entry.bytes == &value.bytes[..] => {}
                Ok(Some(entry)) => {
                    return Err(format!(
                        "output {n} reads {} bytes that are not its {}: first apart at {:?}",
                        entry.bytes.len(),
                        value.bytes.len(),
                        entry.bytes.iter().zip(&value.bytes).position(|(a, b)| a != b)
                    ));
                }
                Ok(None) => return Err(format!("output {n} is gone: no record of it")),
                Err(why) => return Err(format!("output {n} does not read: {why:?}")),
            }
        }
        Ok(())
    }

    fn step(&mut self, op: &Op) -> Result<(), String> {
        match *op {
            Op::Whole { seed, len } => {
                let bytes = noise(len as usize, seed);
                // clone: the model's copy of the value.
                self.put(Kind::Output, &bytes, bytes.clone(), None)?;
            }
            Op::Derived { base, seed, ranges, len } => {
                let bases: Vec<usize> =
                    (0..self.values.len()).filter(|it| self.values[*it].whole).collect();
                if bases.is_empty() {
                    return Ok(());
                }
                let based = bases[usize::from(base) % bases.len()];
                let base = &self.values[based];
                let (len, writes) = recipe_of(base.bytes.len() as u64, seed, ranges, len);
                let recipe = derived::encode(&base.key, len, &writes)
                    .map_err(|why| format!("the test drew a bad recipe: {why}"))?;
                let gives = derived::applied(&base.bytes, len, writes.iter().map(|(at, it)| (*at, it)));
                self.put(Kind::Derived, &recipe, gives, Some(based))?;
            }
            Op::Place { value, path: n, steps } => {
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
                let tag = store
                    .place(&key, path(n), &mut self.effects)
                    .ok_or_else(|| format!("the store has no value {value} to place"))?;
                self.asked.push(Asked { tag, path: n, value });
                self.ever[n].push(value);
                self.pump(steps)?;
            }
            Op::Settle => self.pump(None)?,
            Op::Fill { room } => {
                self.filled = true;
                self.sim.fill(name("store/placed"), u64::from(room))
            }
            Op::Crash => {
                self.sim.crash();
                self.effects.clear();
                self.asked.clear();
                self.fresh = [false; PATHS.len()];
                self.open()?;
                self.homes()?;
                self.look_after_crash()?;
            }
            Op::Reopen => {
                self.pump(None)?;
                // A listing a full `placed/` refused leaves the open the one before it.
                if self.filled {
                    self.fresh = [false; PATHS.len()];
                }
                self.open()?;
                self.homes()?;
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
    forced(case, plant).map(|_| ())
}

/// Runs `case` with `plant` planted, as [`run`] does, and says how many bytes its clones
/// forced out of their sources ([`Sim::bytes_forced`]): what a real disk would have made
/// those clones wait for.
fn forced(case: &Case, plant: PlacePlant) -> Result<u64, String> {
    let mut world = World::new(case, plant);
    for (n, op) in case.ops.iter().enumerate() {
        world.step(op).map_err(|why| format!("at op {n} ({op:?}): {why}"))?;
    }
    world.pump(None).map_err(|why| format!("at the end: {why}"))?;
    Ok(world.sim.bytes_forced())
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
/// (3) after a crash a path holds nothing, or the whole of a value once placed there, or what
/// the power left of a place of one (nothing of a place is synced: any of the sectors of a
/// value written where it goes, or a derived value's base with any of its ranges written),
/// never bytes of anything else; (4) no clone the store asks for is of a file with data not written back,
/// so no place waits for a write-back a clone forced; and the store opens.
/// `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn a_place_gives_its_values_bytes_whole_through_any_crash() {
    super::model_check::run_cases(96, |tc| {
        let case = tc.draw(case());
        match forced(&case, PlacePlant::None) {
            Err(why) => panic!("{case:?}: {why}"),
            Ok(0) => {}
            Ok(forced) => panic!(
                "{case:?}: its clones forced {forced} bytes out of their sources: a file was \
                 cloned from before its write-back completed"
            ),
        }
    });
}

/// A whole output, long enough to be cloned from a whole file.
fn whole(seed: u64) -> Op {
    Op::Whole { seed, len: (CLONE_FROM as u32) + 9000 }
}

fn place(value: u8, path: u8) -> Op {
    Op::Place { value, path, steps: None }
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

/// The planted-bug control: the names of the whole files kept as a set, as the collector's
/// placed keys were: a path placed again from another value removes the file a second path
/// still names, and the bound's oracle says so.
#[test]
fn the_oracle_catches_a_whole_file_removed_while_a_second_path_names_it() {
    let ops = [whole(1), place(0, 0), place(0, 1), whole(2), place(1, 0)];
    let (seed, why) = caught(&ops, 0..8, PlacePlant::OneName)
        .expect("the oracle missed a whole file removed while a second path names it");
    assert!(why.contains("was removed while two/build/a names it"), "seed {seed}: {why}");
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
    // Placed after a reopen, so the output is a clone of the derived value's own file (its
    // base's is not yet known to be there): the file the planted store renames too early.
    let ops = [whole(2), place(0, 0), derived, Op::Reopen, place(1, 0)];
    let (seed, why) = caught(&ops, 0..512, PlacePlant::RenameBeforeRanges)
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

/// The planted-bug control: a whole file cloned from as soon as it is sealed, its write-back
/// begun and not waited for. The derived value's file is its base's clone with ranges just
/// written, so the clone of it to the output's path forces those ranges out and waits for
/// them: on a real disk, and after a whole write, the 1.3 s an edit's answer waited. The
/// bytes placed are right all the same, so only the count of forced bytes catches it.
#[test]
fn the_oracle_catches_a_clone_of_a_dirty_file_on_the_edit_path() {
    let derived = Op::Derived { base: 0, seed: 13, ranges: 3, len: Len::Same };
    let ops = [whole(5), place(0, 0), derived, Op::Reopen, place(1, 0)];
    // The planted store's clone races its own write-back, so on some seed it comes first.
    let caught = (0..96).find_map(|seed| {
        let case = Case { seed, latency_us: 300, ops: ops.to_vec() }; // clone: a test's ops.
        assert_eq!(
            forced(&case, PlacePlant::None),
            Ok(0),
            "seed {seed}: the store as it is forces nothing"
        );
        let planted = forced(&case, PlacePlant::ClonesUnflushed)
            .expect("the bytes placed are right: only the forced write-back says it");
        (planted > 0).then_some((seed, planted))
    });
    let (seed, planted) =
        caught.expect("the oracle missed a clone of a file not yet written back");
    assert!(planted <= 9 * SECTOR as u64, "seed {seed}: {planted} bytes, more than its ranges");
    // The first place too: a whole file written and cloned from at once forces all of it.
    let ops = [whole(6), place(0, 0), place(0, 1)];
    let case = Case { seed: 2, latency_us: 300, ops: ops.to_vec() }; // clone: a test's ops.
    assert_eq!(forced(&case, PlacePlant::None), Ok(0));
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

/// A derived value read back as a file is its base's bytes with every recipe's ranges
/// written, down a chain; [`Reader::get`] of it is the recipe. (A dev executable adopted from
/// a record after a restart was handed the recipe as its image: "Unknown file magic".)
#[test]
fn a_derived_values_file_is_its_chain_applied_and_never_its_recipe() {
    let mut sim = Sim::new(37, quiet());
    let (mut store, base, first) = derived_once(&mut sim, 64 << 10, 2, 300);
    let second = edit(&mut sim, &mut store, (2, &first), 3, 8192);
    let reader = store.reader();
    let guard = enter();
    assert_eq!(reader.file_of(&guard, &key(1)), Ok(Some(base)));
    assert_eq!(reader.file_of(&guard, &key(2)).unwrap().as_ref(), Some(&first));
    assert_eq!(reader.file_of(&guard, &key(3)).unwrap().as_ref(), Some(&second));
    assert_eq!(reader.file_of(&guard, &key(9)), Ok(None));
    // The control: what `get` gives of it is the recipe, a few hundred bytes, no file.
    let recipe = reader.get(&guard, &key(3)).unwrap().unwrap();
    assert_eq!(recipe.kind, Kind::Derived);
    assert_ne!(recipe.bytes, &second[..]);
}

/// The one place asked, driven to its end.
fn placed_one(sim: &mut Sim, store: &mut Store, effects: &mut Vec<Effect>) -> Placed {
    drive(sim, store, effects);
    let placed = store.placed();
    assert_eq!(placed.len(), 1, "{placed:?}");
    placed.into_iter().next().unwrap()
}

/// What a derived value of key `base`'s gives, put under `key`: one range of 4 KiB at `at`.
fn edit(sim: &mut Sim, store: &mut Store, base: (u64, &[u8]), new: u64, at: u64) -> Vec<u8> {
    let writes = vec![(at, noise(4096, 900 + new))];
    let recipe = derived::encode(&key(base.0), base.1.len() as u64, &writes).unwrap();
    store.put(key(new), Kind::Derived, &recipe).unwrap();
    flush(sim, store);
    derived::applied(base.1, base.1.len() as u64, writes.iter().map(|(at, it)| (*at, it)))
}

/// A warm edit's placement writes its ranges and nothing else, and waits for no write-back:
/// an image of 10 MiB that differs from the last by one block of 4 KiB is placed by cloning
/// the last image's whole file, which is written back, and writing 4 KiB into the clone,
/// once for the output and once for the store's own file of the new image; none of the base
/// is read (or hashed: the store never has its bytes in hand), and no clone is of a file with
/// data to write back. The edit after it derives from the file the first one made, written
/// back by then, at the same cost; and the same image at a second path (`debug/` beside
/// `deps/`) is a clone that writes nothing. The simulation counts every byte an effect wrote
/// to a file, read from one, and forced out of a clone's source.
#[test]
fn a_warm_edits_placement_writes_its_ranges_and_forces_no_write_back() {
    const LEN: usize = 10 << 20;
    let mut sim = Sim::new(31, quiet());
    let (mut store, base, gives) = derived_once(&mut sim, LEN, 1, 4096);
    assert_eq!(store.reader().derivable(&enter(), &key(1)), Derivable::Yes, "written back: a base");
    let (wrote, read) = (own(sim.bytes_written(), &store), sim.bytes_read());
    // As a build's job replaces the image at its path: the old one's whole file is let go
    // of, and the new one placed.
    let mut effects = Vec::new();
    store.place(&key(2), name("one/build/a"), &mut effects).unwrap();
    let placed = placed_one(&mut sim, &mut store, &mut effects);
    assert_eq!(placed.result, Ok(()));
    assert_eq!(
        placed.bytes,
        PlacedBytes { cloned: LEN as u64 - 4096, written: 0, copied: 4096, kernel: 0 },
        "what the place reports is what was written where it goes"
    );
    assert_eq!(sim.contents(&name("one/build/a")), gives);
    let (wrote, read) = (own(sim.bytes_written(), &store) - wrote, sim.bytes_read() - read);
    assert!(
        (2 * 4096..=3 * 4096).contains(&wrote),
        "an edit of one 4 KiB range wrote {wrote} bytes: its range into the output and into \
         the store's file, and at most a constant"
    );
    assert!(read < 64 << 10, "a derive read {read} bytes: none of its 10 MiB base is read");
    assert_eq!(sim.bytes_forced(), 0, "no clone was of a file with data to write back");
    // The base's whole file went once the derives had cloned it: one whole file is kept.
    assert!(!sim.exists(&name(&format!("store/placed/{:x}", content_of(&base)))));
    assert!(sim.exists(&name(&format!("store/placed/{:x}{DERIVED_SUFFIX}", key(2).0))));
    assert!(store.problems().is_empty(), "{:?}", store.problems());

    // The next edit: its base is the file the last one derived, written back by now.
    assert_eq!(store.reader().derivable(&enter(), &key(2)), Derivable::Yes);
    let next = edit(&mut sim, &mut store, (2, gives.as_slice()), 3, 8 << 20);
    let wrote = own(sim.bytes_written(), &store);
    store.place(&key(3), name("one/build/a"), &mut effects).unwrap();
    let placed = placed_one(&mut sim, &mut store, &mut effects);
    assert_eq!(placed.result, Ok(()));
    assert_eq!(placed.bytes, PlacedBytes { cloned: LEN as u64 - 4096, written: 0, copied: 4096, kernel: 0 });
    assert_eq!(sim.contents(&name("one/build/a")), next);
    let wrote = own(sim.bytes_written(), &store) - wrote;
    assert!((2 * 4096..=3 * 4096).contains(&wrote), "the second edit wrote {wrote} bytes");
    assert_eq!(sim.bytes_forced(), 0);
    assert!(!sim.exists(&name(&format!("store/placed/{:x}{DERIVED_SUFFIX}", key(2).0))));

    // One output, two paths: the second is a clone of the store's file of it, written back.
    let wrote = own(sim.bytes_written(), &store);
    store.place(&key(3), name("one/debug/a"), &mut effects).unwrap();
    let placed = placed_one(&mut sim, &mut store, &mut effects);
    assert_eq!(placed.result, Ok(()));
    assert_eq!(
        placed.bytes,
        PlacedBytes { cloned: LEN as u64, written: 2 * 4096, copied: 0, kernel: 0 },
        "cloned whole; it is the first to report what making the store's files wrote (its \
         own ranges and those of the file before it, which no place cloned)"
    );
    assert_eq!(sim.contents(&name("one/debug/a")), next);
    assert_eq!((own(sim.bytes_written(), &store) - wrote, sim.bytes_forced()), (0, 0));
    assert!(store.problems().is_empty(), "{:?}", store.problems());
}

/// One output, two paths, asked in the same turn as the edit (a unit whose image is at
/// `deps/` and at the profile's directory): each is a clone of the base with the ranges
/// written, so the edit writes its ranges three times (the two paths and the store's own
/// file) and the whole file nowhere, and neither path waits for the store's file.
#[test]
fn one_edit_at_two_paths_derives_each_and_writes_the_whole_file_nowhere() {
    const LEN: usize = 4 << 20;
    let mut sim = Sim::new(32, quiet());
    let (mut store, _, gives) = derived_once(&mut sim, LEN, 2, 4096);
    let wrote = own(sim.bytes_written(), &store);
    let mut effects = Vec::new();
    let a = store.place(&key(2), name("one/build/a"), &mut effects).unwrap();
    let b = store.place(&key(2), name("one/debug/a"), &mut effects).unwrap();
    drive(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    assert_eq!(placed.len(), 2, "{placed:?}");
    for it in &placed {
        assert!(it.tag == a || it.tag == b);
        assert_eq!(it.result, Ok(()));
        assert_eq!(it.bytes, PlacedBytes { cloned: LEN as u64 - 2 * 4096, written: 0, copied: 2 * 4096, kernel: 0 });
    }
    assert_eq!(sim.contents(&name("one/build/a")), gives);
    assert_eq!(sim.contents(&name("one/debug/a")), gives);
    let wrote = own(sim.bytes_written(), &store) - wrote;
    assert!(
        (6 * 4096..=7 * 4096).contains(&wrote),
        "two ranges at two paths and in the store's file wrote {wrote} bytes"
    );
    assert_eq!(sim.bytes_forced(), 0);
}

/// The dirty-base rule ([`Reader::derivable`]): while the whole file of the last image is
/// written and not yet written back (here its write-back never completes: the device stopped
/// answering), it is no base, so the edit's image is put whole; its place writes the whole
/// file once where it goes (and once as the store's own file of it), clones nothing and
/// forces nothing, and is answered while both write-backs are still held. When the
/// write-backs complete, the new image is a base again and the old one's file, let go of,
/// goes.
#[test]
fn an_edit_over_a_base_not_written_back_is_put_whole_written_once_and_forces_nothing() {
    const LEN: usize = 1 << 20;
    let mut sim = Sim::new(33, quiet());
    let mut store = open(&mut sim, "store");
    let base = noise(LEN, 5);
    store.put(key(1), Kind::Output, &base).unwrap();
    flush(&mut sim, &mut store);
    sim.hold_syncs(name("store/placed"));
    let mut effects = Vec::new();
    store.place(&key(1), name("one/build/a"), &mut effects).unwrap();
    let placed = placed_one(&mut sim, &mut store, &mut effects);
    assert_eq!(placed.result, Ok(()), "answered while its file's write-back is held");
    assert_eq!(placed.bytes, PlacedBytes { cloned: 0, written: LEN as u64, copied: LEN as u64, kernel: 0 });
    assert_eq!(sim.contents(&name("one/build/a")), base);
    assert_eq!(
        store.reader().derivable(&enter(), &key(1)),
        Derivable::No(NoBase::Unflushed),
        "written by this run and not written back: no base"
    );
    // So the edit is handed over whole, as the caller of `derivable` does.
    let mut edited = base.clone(); // clone: a test's bytes.
    edited[LEN / 2] ^= 1;
    store.put(key(2), Kind::Output, &edited).unwrap();
    flush(&mut sim, &mut store);
    let wrote = own(sim.bytes_written(), &store);
    store.place(&key(2), name("one/build/a"), &mut effects).unwrap();
    let placed = placed_one(&mut sim, &mut store, &mut effects);
    assert_eq!(placed.result, Ok(()));
    assert_eq!(placed.bytes, PlacedBytes { cloned: 0, written: LEN as u64, copied: LEN as u64, kernel: 0 });
    assert_eq!(sim.contents(&name("one/build/a")), edited);
    assert_eq!(
        own(sim.bytes_written(), &store) - wrote,
        2 * LEN as u64,
        "the whole file once where it goes, and once as the store's file of it"
    );
    assert_eq!(sim.bytes_forced(), 0, "nothing was cloned, so nothing was forced out");
    // The device answers again: both files are written back.
    sim.release_held(Duration::from_micros(50));
    drive(&mut sim, &mut store, &mut effects);
    assert_eq!(store.reader().derivable(&enter(), &key(2)), Derivable::Yes);
    assert!(!sim.exists(&name(&format!("store/placed/{:x}", content_of(&base)))), "let go of");
    assert!(sim.exists(&name(&format!("store/placed/{:x}", content_of(&edited)))));
    assert_eq!(sim.bytes_forced(), 0);
    assert!(store.problems().is_empty(), "{:?}", store.problems());
}

/// An image of `LEN` bytes that is two runs and, between and after them, room nothing wrote:
/// the runs, and the file.
fn roomy(len: usize) -> (Vec<(u64, Vec<u8>)>, Vec<u8>) {
    let runs = vec![(0, noise(24 << 10, 41)), (len as u64 / 2, noise(8 << 10, 42))];
    let file = derived::applied(&[], len as u64, runs.iter().map(|(at, it)| (*at, it)));
    (runs, file)
}

/// What a place of `key(2)` at `one/build/a` wrote into files in all, the store's own file of
/// it included, and the file it left there.
fn placed_writing(sim: &mut Sim, store: &mut Store) -> (u64, Vec<u8>) {
    let wrote = own(sim.bytes_written(), store);
    let mut effects = Vec::new();
    store.place(&key(2), name("one/build/a"), &mut effects).unwrap();
    let placed = placed_one(sim, store, &mut effects);
    assert_eq!(placed.result, Ok(()));
    (own(sim.bytes_written(), store) - wrote, sim.contents(&name("one/build/a")))
}

/// The invariant: an image put as its runs over the base of holes ([`derived::holes`]) is
/// placed as the file those runs give, and from its put to its place nothing writes its
/// room: the record is the runs, and the place writes the base once and the runs once (into
/// the store's own file of it, which the output is a clone of). The image is then a base as
/// any other, so the edit after it derives from it.
#[test]
fn an_image_put_as_its_runs_over_holes_is_placed_writing_its_runs_and_none_of_its_room() {
    const LEN: usize = 10 << 20;
    let mut sim = Sim::new(35, quiet());
    let mut store = open(&mut sim, "store");
    let (runs, file) = roomy(LEN);
    let held: usize = runs.iter().map(|it| it.1.len()).sum();
    store.put(key(1), Kind::Output, &derived::holes()).unwrap();
    let recipe = derived::encode(&key(1), LEN as u64, &runs).unwrap();
    assert!(recipe.len() < held + 256, "a record of {} bytes for {held} bytes of runs", recipe.len());
    store.put(key(2), Kind::Derived, &recipe).unwrap();
    flush(&mut sim, &mut store);
    let (wrote, placed) = placed_writing(&mut sim, &mut store);
    assert_eq!(placed, file);
    assert!(
        wrote <= (2 * (derived::HOLES + held) + (64 << 10)) as u64,
        "placing {held} bytes of runs in an image of {LEN} wrote {wrote} bytes"
    );
    assert_eq!(sim.bytes_forced(), 0, "no clone was of a file with data to write back");
    assert_eq!(store.reader().file_of(&enter(), &key(2)).unwrap().as_ref(), Some(&file));
    assert_eq!(store.reader().derivable(&enter(), &key(2)), Derivable::Yes, "a base for the next edit");
    let edited = edit(&mut sim, &mut store, (2, &file), 3, 8192);
    let mut effects = Vec::new();
    store.place(&key(3), name("one/build/a"), &mut effects).unwrap();
    assert_eq!(placed_one(&mut sim, &mut store, &mut effects).result, Ok(()));
    assert_eq!(sim.contents(&name("one/build/a")), edited);
    assert!(store.problems().is_empty(), "{:?}", store.problems());
}

/// The control of the invariant above (test-the-cause): the same image put whole, its room
/// in its bytes, is placed as the same file by writing all of it, room and all.
#[test]
fn the_oracle_catches_an_image_put_whole_with_its_room() {
    const LEN: usize = 10 << 20;
    let mut sim = Sim::new(35, quiet());
    let mut store = open(&mut sim, "store");
    let (_, file) = roomy(LEN);
    store.put(key(2), Kind::Output, &file).unwrap();
    flush(&mut sim, &mut store);
    let (wrote, placed) = placed_writing(&mut sim, &mut store);
    assert_eq!(placed, file);
    assert!(wrote >= LEN as u64, "a whole put of {LEN} bytes was placed writing {wrote}");
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
                PlacedBytes { cloned: LEN as u64, written: 3 * 700, copied: 0, kernel: 0 },
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

/// The invariant (`crate::home`): wherever the power goes while an output's placed file is
/// written, synced and made its record's home, the store opens and the output reads its bytes,
/// from the pack's copy or from the file, and a place of it after gives them. A crash after
/// each count of completions of the first place, and of everything after it, on a few seeds.
#[test]
fn a_crash_at_every_step_of_making_a_home_leaves_the_pack_copy_or_the_file() {
    for steps in 0..48u8 {
        for seed in 0..4 {
            let ops = [whole(1), Op::Place { value: 0, path: 0, steps: Some(steps) }, Op::Crash, place(0, 1)];
            // clone: a test's ops, once a seed.
            let case = Case { seed, latency_us: 300, ops: ops.to_vec() };
            assert_eq!(run(&case, PlacePlant::None), Ok(()), "steps {steps}, seed {seed}");
        }
    }
}

/// The planted-bug control: the records moved to the placed file before its sync and
/// `placed/`'s answered. The commit that names it is durable and the file is not, so on some
/// seed the crash tears it and the output reads torn bytes, or none.
#[test]
fn the_oracle_catches_a_home_named_before_its_file_is_synced() {
    let ops = [whole(1), place(0, 0), Op::Settle, Op::Crash];
    let (seed, why) = caught(&ops, 0..64, PlacePlant::HomesBeforeSync)
        .expect("the oracle missed a record moved to a file not yet synced");
    assert!(
        why.contains("output 0") || why.contains("does not open"),
        "seed {seed}: {why}"
    );
}
