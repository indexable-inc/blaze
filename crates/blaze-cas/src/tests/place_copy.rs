//! A store on a file system that cannot share blocks (ext4, tmpfs): how it places is decided
//! once as it opens ([`PlaceBy`]), and every place after is made that one way, a restart's
//! and a patched one's too. Before 2026-11-12 a first place was a write and every later one a
//! clone, which such a file system refused: a home on tmpfs built once and then failed every
//! build after a restart.

use super::*;

fn no_clone() -> Faults {
    Faults { no_clone: true, ..Faults::default() }
}

/// The values the cases place: one kept as a file of its own, one output long enough to be
/// chunks (and, where blocks are shared, cloned from a whole file), one short record.
fn values() -> Vec<(u64, Kind, Vec<u8>)> {
    vec![
        (1, Kind::Query, noise(EXTERNAL_FROM, 100)),
        (2, Kind::Output, noise(CHUNK_FROM + 10, 200)),
        (3, Kind::Code, text(3, 900)),
    ]
}

/// Places every value at `dir/<n>` and gives each answer.
fn place_all(sim: &mut Sim, store: &mut Store, dir: &str) -> Vec<Placed> {
    let mut effects = Vec::new();
    for (n, _, _) in values() {
        store.place(&key(n), name(&format!("{dir}/{n}")), &mut effects).unwrap();
    }
    drive(sim, store, &mut effects);
    let mut placed = store.placed();
    placed.sort_by(|a, b| a.to.as_str().cmp(b.to.as_str()));
    placed
}

/// The oracle of a place: each path holds its value's bytes, each answer is a copy of that
/// many bytes and a clone of none, and the store keeps no whole file to clone from.
fn copied_whole(sim: &Sim, placed: &[Placed], dir: &str) -> Result<(), String> {
    for ((n, _, bytes), placed) in values().iter().zip(placed) {
        let to = name(&format!("{dir}/{n}"));
        if placed.to != to || placed.result != Ok(()) {
            return Err(format!("{to}: {placed:?}"));
        }
        if sim.contents(&to) != *bytes {
            return Err(format!("{to} does not hold the value's bytes"));
        }
        let want = PlacedBytes { cloned: 0, written: 0, copied: bytes.len() as u64 };
        if placed.bytes != want {
            return Err(format!("{to}: placed as {:?}, not a copy of its bytes", placed.bytes));
        }
        let file = name(&format!("store/placed/{:x}", content_of(bytes)));
        if sim.exists(&file) {
            return Err(format!("{file}: a whole file kept to clone from, where nothing clones"));
        }
    }
    Ok(())
}

/// Where blocks cannot be shared the store says so as it opens, and every place is a copy
/// of the value's bytes: the first, the second of the same value (which a store that shares
/// blocks makes a clone), and the first after a restart.
#[test]
fn where_blocks_cannot_be_shared_every_place_is_a_copy_before_and_after_a_restart() {
    let mut sim = Sim::new(21, no_clone());
    let mut store = open(&mut sim, "store");
    assert_eq!(store.place_by(), Some(PlaceBy::Copy));
    assert!(!sim.exists(&name("store/.shares-blocks")), "the try's file is removed");
    assert!(!sim.exists(&name("store/.shares-blocks.clone")));
    for (n, kind, bytes) in values() {
        store.put(key(n), kind, &bytes).unwrap();
    }
    flush(&mut sim, &mut store);
    for dir in ["first", "second"] {
        let placed = place_all(&mut sim, &mut store, dir);
        copied_whole(&sim, &placed, dir).unwrap();
    }
    drop(store);
    let mut store = open(&mut sim, "store");
    assert_eq!(store.place_by(), Some(PlaceBy::Copy));
    let placed = place_all(&mut sim, &mut store, "restarted");
    copied_whole(&sim, &placed, "restarted").unwrap();
    assert_eq!(store.verify(None), Vec::new());
}

/// Where blocks are shared the store places by cloning, as before, and says so.
#[test]
fn where_blocks_are_shared_the_store_places_by_cloning() {
    let mut sim = Sim::new(22, quiet());
    let mut store = open(&mut sim, "store");
    assert_eq!(store.place_by(), Some(PlaceBy::Clone));
    assert!(!sim.exists(&name("store/.shares-blocks")), "the try's file is removed");
    let out = noise(EXTERNAL_FROM, 100);
    store.put(key(1), Kind::Query, &out).unwrap();
    flush(&mut sim, &mut store);
    let mut effects = Vec::new();
    store.place(&key(1), name("out/q"), &mut effects).unwrap();
    drive(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    assert_eq!(placed[0].result, Ok(()));
    assert_eq!(placed[0].bytes, PlacedBytes { cloned: out.len() as u64, written: 0, copied: 0 });
    assert_eq!(sim.contents(&name("out/q")), out);
}

/// A patched place where blocks cannot be shared: the path's file with the runs written over
/// it and cut to the new length, and no object of the store changed. `Err` says what the
/// oracle found. `plant`: the planted bug of a path that is another name of the store's file,
/// which the patch is then written through.
fn patched(seed: u64, plant: bool) -> Result<(), String> {
    let mut sim = Sim::new(seed, no_clone());
    let mut store = open(&mut sim, "store");
    if plant {
        store.plant = PlacePlant::PatchThroughLink;
    }
    let value = noise(EXTERNAL_FROM, seed);
    store.put(key(1), Kind::Query, &value).unwrap();
    flush(&mut sim, &mut store);
    let own = name(&format!("store/{}", disk::external_file(&content_of(&value))));
    if sim.contents(&own) != value {
        return Err(format!("{own} is not the value's file of its own"));
    }
    let to = name("target/app");
    let mut effects = Vec::new();
    store.place(&key(1), to.clone(), &mut effects).ok_or("the store has no value to place")?;
    drive(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    if placed.len() != 1 || placed[0].result != Ok(()) {
        return Err(format!("the place: {placed:?}"));
    }
    // What a link wrote: two runs, and a file a little shorter.
    let len = value.len() as u64 - 100;
    let runs = vec![(16, noise(64, seed + 1)), (4096, noise(300, seed + 2))];
    let mut want = value.clone();
    want.truncate(len as usize);
    for (at, run) in &runs {
        want[*at as usize..*at as usize + run.len()].copy_from_slice(run);
    }
    store.place_patched(to.clone(), Sparse { len, runs }, false, &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    if placed.len() != 1 || placed[0].result != Ok(()) {
        return Err(format!("the patched place: {placed:?}"));
    }
    if sim.contents(&to) != want {
        return Err(format!("{to} is not its file with what changed written over it"));
    }
    if placed[0].bytes != (PlacedBytes { cloned: 0, written: 0, copied: len }) {
        return Err(format!("the patched place was {:?}, not a copy of the file", placed[0].bytes));
    }
    // The oracle: a place never changes an object of the store.
    if sim.contents(&own) != value {
        return Err(format!("{own}, the store's own file of the value, was changed by a place"));
    }
    match store.get(&key(1)) {
        Ok(Some(entry)) if entry.bytes == value => Ok(()),
        Ok(_) => Err("the store reads other bytes under the value's key".into()),
        Err(corrupt) => Err(format!("the store's value: {corrupt}")),
    }
}

/// The invariant: where blocks cannot be shared a patched place leaves the path holding its
/// file with what changed written, and changes no object of the store.
#[test]
fn a_patched_place_that_copies_changes_no_object_of_the_store() {
    for seed in 0..8 {
        assert_eq!(patched(seed, false), Ok(()), "seed {seed}");
    }
}

/// The planted-bug control: a path made another name of the store's own file (a hard link)
/// and patched through it changes the store's object, and the oracle says so.
#[test]
fn the_oracle_catches_a_patch_written_through_a_link() {
    assert_eq!(patched(3, false), Ok(()), "the store as it is must pass");
    let why = patched(3, true).expect_err("the oracle missed the patch written through a link");
    assert!(
        why.contains("the store's own file of the value, was changed by a place")
            || why.contains("the store's value"),
        "{why}"
    );
}

/// A patched place of a path that holds no file fails, naming the path, where blocks cannot
/// be shared as where they can.
#[test]
fn a_patched_place_of_a_path_with_no_file_fails_naming_it() {
    let mut sim = Sim::new(23, no_clone());
    let mut store = open(&mut sim, "store");
    let mut effects = Vec::new();
    let runs = vec![(0, noise(32, 1))];
    store.place_patched(name("target/gone"), Sparse { len: 64, runs }, false, &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    assert!(
        matches!(&placed[0].result, Err(why) if why.contains("target/gone is not there")),
        "{placed:?}"
    );
}
