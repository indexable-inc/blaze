//! An output placed from its maker's bytes ([`Store::place_made`]): the edit path's place, on
//! the simulation ([`Sim`]), with the store's own whole files in every state beside it (a cold
//! place writes one, seals it and begins its write-back), write-backs that never complete
//! ([`Sim::hold_syncs`]), clean reopens and crashes at any completion. Hegel draws the run.
//!
//! What the simulation shows of a clone of a file with data not written back: the bytes the
//! clone forces out of its source ([`Sim::bytes_forced`]), which on a real disk the clone
//! waits for (measured on xfs, 162 MB: 1.4 s, on the daemon's loop thread, 2026-11-10); and
//! of an answer that waits for a write-back: a held write-back never completes, so a place
//! that waits for one is never answered.
//!
//! The planted bugs ([`PlacePlant::MadeClonesUnflushed`],
//! [`PlacePlant::MadeWaitsForWriteBack`]) put the route back that an edit's output took
//! until then: a file of the store's written, then cloned to the path.

use hegel::TestCase;
use hegel::generators as gs;

use blaze_core::sim::SECTOR;

use super::*;

const PATHS: [&str; 3] = ["one/build/deps/app", "one/build/app", "two/build/app"];

#[derive(Clone, Debug)]
enum Op {
    /// An image made: `sectors` sectors and `tail` bytes long, `runs` runs of it holding
    /// bytes, the rest zeros (its patch room).
    Make { seed: u64, sectors: u8, tail: u8, runs: u8 },
    /// A whole output put into the store and placed at path `path` by [`Store::place`]: the
    /// store writes the whole file of it under `placed/`, seals it and begins its write-back,
    /// so the store's own files are being made while made outputs are placed.
    Cold { seed: u64, path: u8, steps: Option<u8> },
    /// The image `image` placed at path `path` from its maker's bytes, as a build's job does:
    /// the whole file of what the path held is let go of first ([`Store::unplace_made`]).
    /// Then `steps` completions are delivered (none: all of them).
    Place { image: u8, path: u8, steps: Option<u8> },
    /// Everything in flight run to its end.
    Settle,
    /// The power goes; the store is opened again.
    Crash,
    /// Everything in flight ended, the store dropped and opened again.
    Reopen,
}

#[derive(Clone, Debug)]
struct Case {
    seed: u64,
    latency_us: u64,
    /// Whether every write-back of a file under `placed/` is taken and never completes.
    held: bool,
    ops: Vec<Op>,
}

/// What a place puts at a path: its bytes, and for a value of the store its key.
struct Value {
    bytes: Vec<u8>,
    key: Option<Key>,
}

/// A place asked and not yet answered; `made`, by [`Store::place_made`].
struct Asked {
    tag: Tag,
    path: usize,
    value: usize,
    made: bool,
}

struct World {
    sim: Sim,
    /// The store; none only while it is closed to be opened again.
    store: Option<Store>,
    plant: PlacePlant,
    held: bool,
    effects: Vec<Effect>,
    /// The images made, each with the place of it in `values`.
    images: Vec<(Sparse, usize)>,
    values: Vec<Value>,
    /// What each path holds: none, or exactly these bytes.
    holds: [Option<Vec<u8>>; PATHS.len()],
    /// The value each path was last placed with.
    last: [Option<usize>; PATHS.len()],
    /// Every value a place was ever asked of at each path.
    ever: [Vec<usize>; PATHS.len()],
    asked: Vec<Asked>,
    /// Whether a cold place was asked since the store opened: until one is, the store has
    /// no file of its own to make, so every effect it asks for is a made place's.
    cold: bool,
}

fn path(n: usize) -> FileName {
    name(PATHS[n])
}

/// An image of `sectors` sectors and `tail` bytes with up to three runs of noise, in order
/// and apart, at offsets and lengths that are no multiple of a sector: the runs, and the
/// file they give.
fn image_of(seed: u64, sectors: u8, tail: u8, runs: u8) -> (Sparse, Vec<u8>) {
    let mut rng = Rng::new(seed);
    let len = (u64::from(sectors) + 4) * SECTOR as u64 + u64::from(tail);
    let count = u64::from(runs % 4);
    let room = len / count.max(1);
    let runs: Vec<(u64, Vec<u8>)> = (0..count)
        .map(|n| {
            let at = n * room + rng.below(room / 2);
            let bytes = 1 + rng.below((room / 2).min(3 * SECTOR as u64));
            (at, noise(bytes as usize, seed ^ (n + 1)))
        })
        .collect();
    let mut file = vec![0; len as usize];
    for (at, bytes) in &runs {
        file[*at as usize..*at as usize + bytes.len()].copy_from_slice(bytes);
    }
    (Sparse { len, runs }, file)
}

impl World {
    fn new(case: &Case, plant: PlacePlant) -> World {
        let latency = Duration::from_micros(case.latency_us);
        let mut sim = Sim::new(case.seed, Faults { max_latency: latency, ..Faults::default() });
        let mut store = open(&mut sim, "store");
        store.plant = plant;
        let mut world = World {
            sim,
            store: Some(store),
            plant,
            held: case.held,
            effects: Vec::new(),
            images: Vec::new(),
            values: Vec::new(),
            holds: [None, None, None],
            last: [None; PATHS.len()],
            ever: [Vec::new(), Vec::new(), Vec::new()],
            asked: Vec::new(),
            cold: false,
        };
        world.hold();
        world
    }

    /// A disk whose write-backs under `placed/` never complete, where the case says so (a
    /// crash lets the hold go, so it is taken again after each).
    fn hold(&mut self) {
        if self.held {
            self.sim.hold_syncs(name("store/placed"));
        }
    }

    /// What the store asks the driver for now, held to the rule while only made places were
    /// asked since it opened: no clone, no write-back, and no write or rename that waits for
    /// a sync.
    fn asks(&self) -> Result<(), String> {
        if self.cold {
            return Ok(());
        }
        for effect in &self.effects {
            let what = match effect {
                Effect::Clone { .. } => "a clone",
                Effect::WriteBack { .. } => "a write-back",
                Effect::Sync { .. } => "a sync",
                Effect::Write { .. } => "a write that waits for a sync",
                Effect::Rename { .. } => "a rename that waits for a sync",
                _ => continue,
            };
            return Err(format!(
                "with only outputs placed from their makers' bytes, the store asked for {what}: {effect:?}"
            ));
        }
        Ok(())
    }

    /// Delivers up to `steps` completions (none: until nothing is in flight but write-backs
    /// held), then takes what the store says of its places.
    fn pump(&mut self, steps: Option<u8>) -> Result<(), String> {
        let mut left = steps.map(u32::from);
        let mut done = Vec::new();
        // The bound, on the simulation's clock: a completion is at most the latency away.
        let end = blaze_core::wait::Deadline::after(self.sim.now(), Duration::from_secs(600));
        let mut idle = false;
        while left != Some(0) {
            self.asks()?;
            self.sim.submit(&mut self.effects);
            if self.sim.in_flight() == 0 {
                idle = true;
                break;
            }
            self.sim.poll(&mut done, end);
            if done.is_empty() {
                return Err(format!(
                    "{} effects in flight and none completed by {end:?}",
                    self.sim.in_flight()
                ));
            }
            let store = self.store.as_mut().expect("the store is open");
            for completion in done.drain(..) {
                if store.owns(completion.tag) {
                    store.complete(completion, &mut self.effects);
                }
                left = left.map(|it| it.saturating_sub(1));
            }
        }
        // What a completion asked for is asked, so a crash next finds it in flight.
        self.asks()?;
        self.sim.submit(&mut self.effects);
        idle |= self.sim.in_flight() == 0;
        let store = self.store.as_mut().expect("the store is open");
        for placed in store.placed() {
            let Some(at) = self.asked.iter().position(|it| it.tag == placed.tag) else {
                return Err(format!("the store answered a place no one asked: {placed:?}"));
            };
            let Asked { path: n, value, made, .. } = self.asked.swap_remove(at);
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
                    let runs = self.images.iter().find(|it| it.1 == value).map(|it| it.0.bytes());
                    if made && Some(placed.bytes.copied) != runs
                        || made && placed.bytes.cloned + placed.bytes.written != 0
                    {
                        return Err(format!(
                            "{} was placed from its maker's runs of {runs:?} bytes and reported \
                             as {:?}",
                            PATHS[n], placed.bytes
                        ));
                    }
                    // clone: the model's copy of what the path holds.
                    self.holds[n] = Some(want.clone());
                    self.last[n] = Some(value);
                }
                // Nothing here fills the disk or fails an effect: a made place has no
                // reason to fail.
                Err(why) if made => {
                    return Err(format!("the place of {} from its maker's bytes failed: {why}", PATHS[n]));
                }
                // A cold place is `place_crash`'s to judge; one that fails leaves its path.
                Err(_) => {}
            }
        }
        // Nothing is in flight but what the disk holds for ever: a place from its maker's
        // bytes that is not answered now waits for one of those, a write-back.
        if idle && let Some(waits) = self.asked.iter().find(|it| it.made) {
            return Err(format!(
                "the place of {} from its maker's bytes is not answered with nothing in flight \
                 but {} write-backs the disk holds: its answer waits for a write-back",
                PATHS[waits.path],
                self.sim.held_syncs()
            ));
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
    /// there, or what the power left of a place of one, since nothing of a place is synced:
    /// no longer than the value, each byte the value's or a zero (a run not written is a
    /// hole, and a sector not written reads as zeros). Never bytes of anything else.
    fn look_after_crash(&mut self) -> Result<(), String> {
        for n in 0..PATHS.len() {
            let there = self.sim.exists(&path(n)).then(|| self.sim.contents(&path(n)));
            let allowed = match &there {
                None => true,
                Some(found) => self.ever[n].iter().any(|value| {
                    let bytes = &self.values[*value].bytes;
                    found.len() <= bytes.len()
                        && found.iter().zip(bytes).all(|(it, want)| it == want || *it == 0)
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

    /// The store dropped, as its process ends, and opened again.
    fn open(&mut self) {
        self.store = None;
        let mut store = open(&mut self.sim, "store");
        store.plant = self.plant;
        self.store = Some(store);
        self.cold = false;
        self.hold();
    }

    /// One place at a path at a time, so one that ended well is judged alone.
    fn clear(&mut self, n: usize) -> Result<(), String> {
        if self.asked.iter().any(|it| it.path == n) {
            self.pump(None)?;
        }
        // One still not answered waits for a held write-back (a cold place's): it stays,
        // and this path is not placed at again until a crash or a reopen.
        Ok(())
    }

    fn step(&mut self, op: &Op) -> Result<(), String> {
        match *op {
            Op::Make { seed, sectors, tail, runs } => {
                let (file, bytes) = image_of(seed, sectors, tail, runs);
                self.images.push((file, self.values.len()));
                self.values.push(Value { bytes, key: None });
            }
            Op::Cold { seed, path: n, steps } => {
                let n = usize::from(n) % PATHS.len();
                self.clear(n)?;
                if self.asked.iter().any(|it| it.path == n) {
                    return Ok(());
                }
                let bytes = noise(CLONE_FROM as usize + 9000, seed);
                let key = key(5000 + self.values.len() as u64);
                self.cold = true;
                let store = self.store.as_mut().expect("the store is open");
                store.put(key, Kind::Output, &bytes).map_err(|it| format!("a put was refused: {it:?}"))?;
                store.flush(&mut self.effects);
                self.pump(None)?;
                let store = self.store.as_mut().expect("the store is open");
                if let Some(before) = self.last[n].and_then(|it| self.values[it].key) {
                    store.unplace(&before, &key, &mut self.effects);
                }
                let tag = store
                    .place(&key, path(n), &mut self.effects)
                    .ok_or_else(|| "the store has no value to place".to_owned())?;
                let value = self.values.len();
                self.values.push(Value { bytes, key: Some(key) });
                self.asked.push(Asked { tag, path: n, value, made: false });
                self.ever[n].push(value);
                self.pump(steps)?;
            }
            Op::Place { image, path: n, steps } => {
                if self.images.is_empty() {
                    return Ok(());
                }
                let n = usize::from(n) % PATHS.len();
                self.clear(n)?;
                if self.asked.iter().any(|it| it.path == n) {
                    return Ok(());
                }
                // clone: the runs the place's writes own, as a link's are handed over.
                let (file, value) = self.images[usize::from(image) % self.images.len()].clone();
                let store = self.store.as_mut().expect("the store is open");
                if let Some(before) = self.last[n].and_then(|it| self.values[it].key) {
                    store.unplace_made(&before, &mut self.effects);
                }
                let tag = store.place_made(path(n), file, &mut self.effects);
                self.asked.push(Asked { tag, path: n, value, made: true });
                self.ever[n].push(value);
                self.pump(steps)?;
            }
            Op::Settle => self.pump(None)?,
            Op::Crash => {
                self.sim.crash();
                self.effects.clear();
                self.asked.clear();
                self.open();
                self.look_after_crash()?;
            }
            Op::Reopen => {
                self.pump(None)?;
                // A cold place that waits for a held write-back is not answered by a reopen.
                self.asked.clear();
                self.effects.clear();
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

/// Runs `case` with `plant` planted: the bytes its clones forced out of their sources
/// ([`Sim::bytes_forced`]), or where the oracle failed.
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
    let steps = |tc: &TestCase| match tc.draw(gs::booleans()) {
        true => Some(tc.draw(gs::integers::<u8>().max_value(12))),
        false => None,
    };
    match tc.draw(gs::integers::<u8>().max_value(15)) {
        0..3 => Op::Make {
            seed: tc.draw(gs::integers()),
            sectors: tc.draw(gs::integers::<u8>().max_value(60)),
            tail: byte(tc),
            runs: byte(tc),
        },
        3 | 4 => Op::Cold { seed: tc.draw(gs::integers()), path: byte(tc), steps: steps(tc) },
        5..12 => Op::Place { image: byte(tc), path: byte(tc), steps: steps(tc) },
        12 => Op::Settle,
        13 | 14 => Op::Crash,
        _ => Op::Reopen,
    }
}

#[hegel::composite]
fn case(tc: &TestCase) -> Case {
    Case {
        seed: tc.draw(gs::integers()),
        latency_us: tc.draw(gs::integers().min_value(1).max_value(2000)),
        held: tc.draw(gs::booleans()),
        ops: tc.draw(gs::vecs(op()).min_size(1).max_size(24)),
    }
}

/// The invariant: an output placed from its maker's bytes ([`Store::place_made`]), whatever
/// the store's own whole files are doing beside it (written, sealed, their write-backs in
/// flight or held for ever) and wherever the power goes, (1) asks the driver for no clone, no
/// write-back and nothing that waits for a sync, so no clone of a file with data not written
/// back is made for it and none of its bytes are forced out by one; (2) is answered once its
/// own effects have completed, with write-backs the disk never completes still held: its
/// answer waits for no write-back; (3) ends with its path holding exactly its file, the runs
/// over zeros at its length, and reports the runs' bytes as written there; (4) until it ends
/// its path holds what it held or the new file whole, never a part; (5) after a crash its
/// path holds nothing, a value once placed there whole, or what the power left of one (each
/// byte the value's or a zero), never bytes of anything else; and the store opens.
/// `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn a_made_output_is_placed_with_no_clone_and_no_wait_for_a_write_back() {
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

fn make(seed: u64) -> Op {
    Op::Make { seed, sectors: 40, tail: 7, runs: 3 }
}

fn place(image: u8, path: u8) -> Op {
    Op::Place { image, path, steps: None }
}

/// The planted-bug control: the output's file written under `placed/` and cloned to its path
/// at once, the route an edit's 162 MB image took until 2026-11-10 (`ioctl(FICLONE)` of a
/// file just written, 1.4 s on the loop thread). The bytes placed are right, so only the
/// rule on what a made place asks for, and the bytes the clone forced out, say it.
#[test]
fn the_oracle_catches_a_made_output_cloned_from_a_file_just_written() {
    let ops = [make(1), place(0, 0), make(2), place(1, 0), place(1, 1)];
    for seed in 0..8 {
        let case = Case { seed, latency_us: 300, held: false, ops: ops.to_vec() }; // clone: a test's ops.
        assert_eq!(forced(&case, PlacePlant::None), Ok(0), "seed {seed}: the store as it is must pass");
        let why = forced(&case, PlacePlant::MadeClonesUnflushed)
            .expect_err("the oracle missed a made output cloned from a file just written");
        assert!(why.contains("the store asked for a clone"), "seed {seed}: {why}");
    }
    // With a cold place before it the rule on effects is off (the store's own file is being
    // made), and the count of forced bytes alone says it: the runs' bytes, every one.
    let ops = [Op::Cold { seed: 3, path: 2, steps: None }, make(4), place(0, 0)];
    let case = Case { seed: 1, latency_us: 300, held: false, ops: ops.to_vec() }; // clone: a test's ops.
    assert_eq!(forced(&case, PlacePlant::None), Ok(0));
    let planted = forced(&case, PlacePlant::MadeClonesUnflushed)
        .expect("the bytes placed are right: only the forced write-back says it");
    let (file, _) = image_of(4, 40, 7, 3);
    assert_eq!(planted, file.bytes(), "the clone forced out every byte just written");
}

/// The planted-bug control: the output's file written under `placed/`, written back, and
/// cloned to its path only once its write-back completed. No clone is of a dirty file, so
/// nothing is forced; with the disk holding write-backs the place is never answered, which
/// is the 3.2 s an edit's answer was held (`the job's answer waits for what it still does`).
#[test]
fn the_oracle_catches_a_made_output_whose_answer_waits_for_a_write_back() {
    // A cold place first, so the rule on effects is off and the wait alone is what is caught.
    let ops = [Op::Cold { seed: 5, path: 2, steps: None }, make(6), place(0, 0)];
    for seed in 0..8 {
        let case = Case { seed, latency_us: 300, held: true, ops: ops.to_vec() }; // clone: a test's ops.
        assert_eq!(forced(&case, PlacePlant::None), Ok(0), "seed {seed}: the store as it is must pass");
        let why = forced(&case, PlacePlant::MadeWaitsForWriteBack)
            .expect_err("the oracle missed a made output's answer waiting for a write-back");
        assert!(why.contains("its answer waits for a write-back"), "seed {seed}: {why}");
    }
}

/// What a body edit's place writes: the runs that hold bytes, once a path, and nothing of
/// the patch room. An image of 16 MiB of which 1 MiB holds bytes, placed at two paths (the
/// unit's own and the copy beside it): 2 MiB written, none read, none forced. Before, the
/// same edit wrote the whole length three times (the store's file and each path) or cloned
/// a file just written.
#[test]
fn a_made_outputs_place_writes_its_runs_and_no_patch_room() {
    const LEN: u64 = 16 << 20;
    const RUN: usize = 256 << 10;
    let mut sim = Sim::new(41, quiet());
    let mut store = open(&mut sim, "store");
    let runs: Vec<(u64, Vec<u8>)> =
        (0..4u64).map(|n| (n * (LEN / 4), noise(RUN, 300 + n))).collect();
    let file = Sparse { len: LEN, runs };
    let mut want = vec![0; LEN as usize];
    for (at, bytes) in &file.runs {
        want[*at as usize..*at as usize + bytes.len()].copy_from_slice(bytes);
    }
    let (wrote, read) = (sim.bytes_written(), sim.bytes_read());
    let mut effects = Vec::new();
    // clone: the second path's writes own their runs, as the first's do.
    store.place_made(name("one/build/deps/app"), file.clone(), &mut effects);
    store.place_made(name("one/build/app"), file, &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    assert_eq!(placed.len(), 2, "{placed:?}");
    for it in &placed {
        assert_eq!(it.result, Ok(()));
        let bytes = PlacedBytes { cloned: 0, written: 0, copied: 4 * RUN as u64 };
        assert_eq!(it.bytes, bytes);
    }
    assert_eq!(sim.contents(&name("one/build/deps/app")), want);
    assert_eq!(sim.contents(&name("one/build/app")), want);
    assert_eq!(sim.bytes_written() - wrote, 2 * 4 * RUN as u64, "the runs, once a path");
    assert_eq!(sim.bytes_read(), read, "nothing of the store is read");
    assert_eq!(sim.bytes_forced(), 0);
}

/// Runs that are no file are refused by name before anything is written: one past the
/// length, and two that overlap.
#[test]
fn runs_that_are_no_file_are_refused_naming_the_run() {
    let mut sim = Sim::new(43, quiet());
    let mut store = open(&mut sim, "store");
    let mut effects = Vec::new();
    let past = Sparse { len: 100, runs: vec![(90, vec![1; 20])] };
    let over = Sparse { len: 100, runs: vec![(0, vec![1; 20]), (10, vec![2; 5])] };
    store.place_made(name("one/build/app"), past, &mut effects);
    store.place_made(name("one/build/app"), over, &mut effects);
    assert!(effects.is_empty(), "nothing is asked of the driver: {effects:?}");
    let placed = store.placed();
    let why = |n: usize| placed[n].result.clone().unwrap_err();
    assert!(why(0).contains("ends at 0x6e, past the file's length 0x64"), "{}", why(0));
    assert!(why(1).contains("the run at 0xa begins before 0x14"), "{}", why(1));
    assert!(!sim.exists(&name("one/build/app")));
}
