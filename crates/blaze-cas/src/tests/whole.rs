//! The simulation of [`crate::whole::Names`]: places and crashes at every step, over a model
//! of `placed/` whose unsynced writes a crash loses. The store's own routes driving these
//! names, with crashes at any completion, are `tests::place_crash`'s (its oracle `bounded`).

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::whole::Names;

/// `placed/` and its listing as a crash leaves them.
#[derive(Default)]
struct Disk {
    /// Whole files whose bytes are durable.
    files: BTreeSet<ContentId>,
    /// Whole files written and not yet synced: a crash loses them.
    unsynced: BTreeSet<ContentId>,
    /// The listing of names, written whole with no sync: the last one written, or what a
    /// crash left of it ([`World::step`]).
    listing: Vec<u8>,
}

impl Disk {
    fn all(&self) -> Vec<ContentId> {
        self.files.union(&self.unsynced).copied().collect()
    }
}

/// The store's side and the oracle's.
struct World {
    names: Names,
    disk: Disk,
    /// What the oracle knows independently of [`Names`]: the file each answered place named.
    live: BTreeMap<String, ContentId>,
    /// The file the op in flight makes.
    making: Option<ContentId>,
    rng: Rng,
    /// One step in this many is preceded by a crash.
    crash_one_in: u64,
    crashes: u64,
    removed: u64,
}

/// A crash came before a step.
struct Crashed;

impl World {
    fn new(seed: u64, plant: bool) -> World {
        let mut names = Names::default();
        names.plant_one_name = plant;
        World {
            names,
            disk: Disk::default(),
            live: BTreeMap::new(),
            making: None,
            rng: Rng::new(seed),
            crash_one_in: 7,
            crashes: 0,
            removed: 0,
        }
    }

    /// Before each step: maybe a crash, which loses what is in memory and what was not synced.
    fn step(&mut self) -> Result<(), Crashed> {
        if self.rng.next_u64() % self.crash_one_in == 0 {
            self.crashes += 1;
            self.disk.unsynced.clear();
            self.making = None;
            // The listing is written with no sync: the crash may cut it. Cut, it names nothing
            // (its check fails, or nothing is left), and the oracle then knows no live place:
            // each path holds a clone of its own, so a whole file is a cache the paths' bytes
            // do not depend on, and the open's sweep may take every one.
            if !self.disk.listing.is_empty() && self.rng.next_u64() % 3 == 0 {
                let cut = self.rng.next_u64() % self.disk.listing.len() as u64;
                self.disk.listing.truncate(cut as usize);
                self.live.clear();
            }
            return Err(Crashed);
        }
        Ok(())
    }

    /// The oracle, at a removal: nothing a live place or the op in flight names goes.
    fn remove(&mut self, file: ContentId) -> Result<(), String> {
        if let Some((path, _)) = self.live.iter().find(|(_, it)| **it == file) {
            return Err(format!("{file:x} was removed while the place at {path} names it"));
        }
        if self.making == Some(file) {
            return Err(format!("{file:x} was removed while a place in flight makes it"));
        }
        self.disk.files.remove(&file);
        self.disk.unsynced.remove(&file);
        self.removed += 1;
        Ok(())
    }

    /// The oracle, once removals are done: no file outside the names is on disk.
    fn bounded(&self, when: &str) -> Result<(), String> {
        for file in self.disk.all() {
            if !self.live.values().any(|it| *it == file) {
                return Err(format!("{when}: {file:x} is on disk with nothing naming it"));
            }
        }
        Ok(())
    }

    /// Removes what nothing names, a step each; a crash may come between.
    fn sweep(&mut self) -> Result<Result<(), Crashed>, String> {
        for file in self.names.doomed(self.disk.all()) {
            if self.step().is_err() {
                return Ok(Err(Crashed));
            }
            self.remove(file)?;
        }
        Ok(Ok(()))
    }

    /// The store opens after a crash: the names are the listing's, and what it does not name goes.
    fn reopen(&mut self) -> Result<(), String> {
        loop {
            let plant = self.names.plant_one_name;
            self.names = Names::decode(&self.disk.listing)
                .map_err(|why| format!("the listing: {why}"))?
                .unwrap_or_default();
            self.names.plant_one_name = plant;
            if self.names.paths_listed() != self.live {
                return Err(format!("the listing names {:?}, the answered places {:?}", self.names.paths_listed(), self.live));
            }
            match self.sweep()? {
                Ok(()) => return self.bounded("after an open"),
                Err(Crashed) => continue,
            }
        }
    }

    /// The listing written in one step: once it is, the op is answered.
    fn commit(&mut self) -> Result<(), Crashed> {
        self.step()?;
        self.disk.listing = self.names.listing();
        Ok(())
    }

    fn place(&mut self, path: &str, file: ContentId) -> Result<Result<(), Crashed>, String> {
        let crashed = Ok(Err(Crashed));
        if self.step().is_err() {
            return crashed;
        }
        self.names.making(file);
        self.making = Some(file);
        if !self.disk.files.contains(&file) {
            if self.step().is_err() {
                return crashed;
            }
            self.disk.unsynced.insert(file);
            if self.step().is_err() {
                return crashed;
            }
            self.disk.unsynced.remove(&file);
            self.disk.files.insert(file);
        }
        if self.step().is_err() {
            return crashed;
        }
        // The path is renamed onto the file's clone, then the names change.
        self.names.placed(path, file, true).expect("the place was begun");
        if self.commit().is_err() {
            return crashed;
        }
        self.live.insert(path.to_owned(), file);
        self.making = None;
        if let Err(Crashed) = self.sweep()? {
            return crashed;
        }
        self.bounded(&format!("after the place of {path}")).map(Ok)
    }
}

fn file(n: u64) -> ContentId {
    ContentId::of("blaze v2 store test whole file 1", &n.to_le_bytes())
}

const PATHS: [&str; 3] = ["debug/hello", "debug/build/hello/out/hello", "release/hello"];

/// One run of `ops` random places under `seed`, crashing before about one step in seven;
/// the first broken invariant.
fn run(seed: u64, ops: u32, plant: bool) -> Result<World, String> {
    let mut world = World::new(seed, plant);
    for _ in 0..ops {
        let path = PATHS[(world.rng.next_u64() % PATHS.len() as u64) as usize];
        let bytes = file(world.rng.next_u64() % 4);
        let done = world.place(path, bytes)?;
        if let Err(Crashed) = done {
            world.reopen()?;
        }
    }
    Ok(world)
}

/// The invariant: no whole file a live place (one answered) names is ever removed, a
/// crash coming before any step; and once a place is answered and its removals are done, and
/// after every open, no file nothing names is on disk: so `placed/` holds a file for each
/// path's current output and each pin, never one for every build. The listing an open reads
/// names exactly the places answered, or, cut by the crash, nothing.
#[test]
fn no_named_whole_file_is_removed_and_no_unnamed_one_outlives_the_next_place() {
    let (mut crashes, mut removed) = (0, 0);
    for seed in 0..400 {
        let world = run(seed, 60, false).unwrap_or_else(|why| panic!("seed {seed}: {why}"));
        crashes += world.crashes;
        removed += world.removed;
    }
    assert!(crashes > 1000 && removed > 1000, "the runs crashed {crashes} times and removed {removed} files");
}

/// The control: names kept as a set (the collector's placed keys before this) remove a file
/// while a second place still names it, and the oracle says so: here by construction, and
/// under some seed of the simulation.
#[test]
fn the_oracle_catches_a_file_removed_while_a_second_place_names_it() {
    let mut world = World::new(0, true);
    world.crash_one_in = u64::MAX;
    let program = file(1);
    assert!(matches!(world.place(PATHS[0], program), Ok(Ok(()))));
    assert!(matches!(world.place(PATHS[1], program), Ok(Ok(()))));
    let caught = world.place(PATHS[0], file(2));
    assert!(
        matches!(&caught, Err(why) if why.contains(PATHS[1])),
        "the oracle missed it: {:?}",
        caught.err()
    );
    assert!((0..400).any(|seed| run(seed, 60, true).is_err()), "no seed caught the planted bug");
}

/// The invariant: a listing decodes to the names and keys it was made of, the same bytes for
/// the same names; written with no sync, any part of one a crash leaves (every prefix but the
/// empty one, which is none written yet; one with a byte changed; bytes after it) fails its
/// check and names nothing, never names
/// something else; and a listing of layout 1 (paths and files, no keys, no check) still
/// reads, its paths holding no key. The control: a listing that passes its check and is not
/// one is refused naming the offset.
#[test]
fn a_listing_round_trips_and_a_torn_one_names_nothing() {
    let mut names = Names::default();
    for (at, path) in PATHS.iter().enumerate() {
        names.making(file(at as u64));
        names.placed(path, file(at as u64), true).unwrap();
        // Every second path holds a key; a path with a key and no file is listed too.
        if at % 2 == 0 {
            assert_eq!(names.hold(path, Some(crate::Key(file(100 + at as u64)))), None);
        }
    }
    assert_eq!(names.hold("elsewhere/small", Some(crate::Key(file(200)))), None);
    let listing = names.listing();
    let decoded = Names::decode(&listing).unwrap().expect("it passes its check");
    assert_eq!(decoded.listing(), listing);
    assert_eq!(decoded.paths_listed(), names.paths_listed());
    let keys = |it: &Names| it.held_keys().copied().collect::<Vec<_>>();
    assert_eq!(keys(&decoded), keys(&names));
    assert!(decoded.holds(&crate::Key(file(200))));
    assert_eq!(Names::decode(b"").unwrap().unwrap().listing(), Names::default().listing());
    for cut in 1..listing.len() {
        assert!(Names::decode(&listing[..cut]).unwrap().is_none(), "cut at {cut}");
    }
    let mut changed = listing.clone();
    changed[20] ^= 1;
    assert!(Names::decode(&changed).unwrap().is_none());
    let mut long = listing.clone();
    long.push(0);
    assert!(Names::decode(&long).unwrap().is_none());
    // The control: bytes after the last path, under a check that holds, are a bug, not a tear.
    let mut wrong = listing[..listing.len() - 4].to_vec();
    wrong.push(0);
    let check = blaze_ids::crc32c::crc32c(&wrong);
    wrong.extend_from_slice(&check.to_le_bytes());
    assert!(Names::decode(&wrong).unwrap_err().contains("follow the last path"));
    // Layout 1: the magic, the count, then each path's length, the path and its file.
    let mut one = b"BZPLACE1".to_vec();
    one.extend_from_slice(&2u32.to_le_bytes());
    for (at, path) in ["a/app", "b/app"].iter().enumerate() {
        one.extend_from_slice(&(path.len() as u32).to_le_bytes());
        one.extend_from_slice(path.as_bytes());
        one.extend_from_slice(&file(at as u64).0);
    }
    let old = Names::decode(&one).unwrap().expect("layout 1 has no check to fail");
    let paths: Vec<(String, ContentId)> = old.paths_listed().into_iter().collect();
    assert_eq!(paths, [("a/app".to_owned(), file(0)), ("b/app".to_owned(), file(1))]);
    assert_eq!(old.held_keys().count(), 0, "layout 1 lists no keys");
}
