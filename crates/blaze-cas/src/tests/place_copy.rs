//! A store on a file system that cannot share blocks (ext4, tmpfs): how it places is decided
//! once as it opens ([`PlaceBy`]), and every place after is made that one way, a restart's
//! and a patched one's too. Before 2026-11-12 a first place was a write and every later one a
//! clone, which such a file system refused: a home on tmpfs built once and then failed every
//! build after a restart.
//!
//! A patched place there is the clone route with one effect swapped (2026-11-12): the kernel
//! copies the path's file's data extents into a new inode beside it (`Effect::CopyData`), the
//! runs are written into that, and it is renamed onto the path. Until then the store read the
//! whole file (158 MB measured, twice a build) and wrote it back.
//!
//! Every place answers the stamp of its path as a look after it ended finds it, change time
//! included, on every route, and vouches for no file another writer renamed onto the path
//! between its rename and its look (`Store::vouch`, 2026-11-12).

use std::collections::BTreeSet;

use blaze_core::{Stamp, sim::SECTOR};

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
        let want = PlacedBytes { cloned: 0, written: 0, copied: bytes.len() as u64, kernel: 0 };
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
    assert_eq!(placed[0].bytes, PlacedBytes { cloned: out.len() as u64, written: 0, copied: 0, kernel: 0 });
    assert_eq!(sim.contents(&name("out/q")), out);
}

/// The sectors `runs` cover: the blocks a write of them allocates.
fn sectors(runs: &[(u64, Vec<u8>)]) -> BTreeSet<u64> {
    let sector = SECTOR as u64;
    runs.iter()
        .filter(|(_, bytes)| !bytes.is_empty())
        .flat_map(|(at, bytes)| at / sector..=(at + bytes.len() as u64 - 1) / sector)
        .collect()
}

/// The bytes of data the sectors hold in a file `len` long: what an extent copy copies.
fn data_bytes(sectors: &BTreeSet<u64>, len: u64) -> u64 {
    let sector = SECTOR as u64;
    sectors.iter().map(|it| len.saturating_sub(it * sector).min(sector)).sum()
}

/// `file` with `runs` written over it, cut to `len` first.
fn patched_bytes(file: &[u8], len: u64, runs: &[(u64, Vec<u8>)]) -> Vec<u8> {
    let mut want = file.to_vec();
    want.resize(len as usize, 0);
    for (at, run) in runs {
        want[*at as usize..*at as usize + run.len()].copy_from_slice(run);
    }
    want
}

/// Drives the effects to idle as [`drive`] does, keeping every effect asked, in order.
fn drive_logged(sim: &mut Sim, store: &mut Store, effects: &mut Vec<Effect>) -> Vec<Effect> {
    // clone: the log of what was asked, a test's
    let mut log: Vec<Effect> = effects.clone();
    run_until_idle(sim, effects, |completion, effects| {
        if store.owns(completion.tag) {
            let before = effects.len();
            store.complete(completion, effects);
            // clone: the log of what was asked, a test's
            log.extend(effects[before..].iter().cloned());
        }
    })
    .expect("the effects end");
    log
}

/// The answer under `tag` among `placed`.
fn answer_of(placed: &[Placed], tag: Tag) -> Result<&Placed, String> {
    placed.iter().find(|it| it.tag == tag).ok_or_else(|| format!("no answer under {tag:?}: {placed:?}"))
}

/// What a patched place had before it began: the path's bytes, its file's allocated sectors
/// and its stamp, and the simulation's counts.
struct Before {
    bytes: Vec<u8>,
    sectors: BTreeSet<u64>,
    stamp: Stamp,
    written: u64,
    copied: u64,
}

impl Before {
    /// `store`'s own bytes written are counted: the listing of its names apart ([`own`]).
    fn of(sim: &mut Sim, store: &Store, to: &FileName, sectors: BTreeSet<u64>) -> Before {
        Before {
            bytes: sim.contents(to),
            sectors,
            stamp: sim.stamp(to),
            written: own(sim.bytes_written(), store),
            copied: sim.bytes_copied(),
        }
    }
}

/// The oracle of a patched place where blocks cannot be shared. `Err` says what it found.
///
/// The invariant: the path's file is never written (no write, cut or append of `to` is
/// asked, so a holder of the old inode, a running program, reads it unchanged until the
/// rename); the bytes written are the runs' and the kernel's copy is the old file's data, its
/// holes not copied; the new file holds blocks where the old one did and where a run wrote,
/// and nowhere else (holes stay holes); the path holds the old bytes with the runs written,
/// cut to the new length; and the answer carries the stamp of the new inode now at the path,
/// with the wall time its look was asked.
fn patch_kept(
    sim: &mut Sim,
    store: &Store,
    to: &FileName,
    log: &[Effect],
    placed: &Placed,
    before: &Before,
    patch: &Sparse,
) -> Result<(), String> {
    for effect in log {
        let file = match effect {
            Effect::WriteAt { file, .. }
            | Effect::Write { file, .. }
            | Effect::WriteUnsynced { file, .. }
            | Effect::WriteFrom { file, .. }
            | Effect::Append { file, .. }
            | Effect::Truncate { file, .. } => file,
            _ => continue,
        };
        if file == to {
            return Err(format!("{to}, the path's own file, was written in place: {effect:?}"));
        }
    }
    if placed.result != Ok(()) {
        return Err(format!("the patched place: {placed:?}"));
    }
    if sim.contents(to) != patched_bytes(&before.bytes, patch.len, &patch.runs) {
        return Err(format!("{to} is not its file with what changed written over it"));
    }
    let runs: u64 = patch.runs.iter().map(|it| it.1.len() as u64).sum();
    let written = own(sim.bytes_written(), store) - before.written;
    if written != runs {
        return Err(format!("{written} bytes written for runs of {runs}"));
    }
    let copied = sim.bytes_copied() - before.copied;
    let data = data_bytes(&before.sectors, before.bytes.len() as u64);
    if copied != data || placed.bytes.kernel != copied {
        return Err(format!(
            "the kernel copied {copied} bytes (the answer says {}) of a file holding {data} bytes of data",
            placed.bytes.kernel
        ));
    }
    if placed.bytes != (PlacedBytes { cloned: 0, written: 0, copied: runs, kernel: data }) {
        return Err(format!("the patched place was {:?}", placed.bytes));
    }
    let kept = (patch.len as usize).div_ceil(SECTOR) as u64;
    let want: BTreeSet<u64> =
        before.sectors.union(&sectors(&patch.runs)).copied().filter(|it| *it < kept).collect();
    let allocated = sim.allocated(to);
    if allocated != want.len() as u64 * SECTOR as u64 {
        return Err(format!(
            "{to} holds {allocated} bytes of blocks, not the {} its old blocks and the runs' make: a hole was filled",
            want.len() * SECTOR
        ));
    }
    stamps_whole(sim, [placed])?;
    let now = sim.stamp(to);
    if now.ino == before.stamp.ino {
        return Err(format!("{to} is the inode it was ({}): no new file was renamed onto it", now.ino));
    }
    Ok(())
}

/// A file with holes placed from a maker's runs where blocks cannot be shared, then patched.
fn patched_with_holes(seed: u64) -> Result<(), String> {
    let mut sim = Sim::new(seed, no_clone());
    let mut store = open(&mut sim, "store");
    let to = name("target/app");
    // Data at the front and in the middle; holes between and to its end.
    let first = Sparse {
        len: 64 << 10,
        runs: vec![(0, noise(1000, seed)), (40_000, noise(2000, seed + 1))],
    };
    let mut effects = Vec::new();
    let tag = store.place_made(to.clone(), first.clone(), &mut effects); // clone: kept to compare
    drive(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    answer_of(&placed, tag)?.result.clone().map_err(|why| format!("the first place: {why}"))?;
    let before = Before::of(&mut sim, &store, &to, sectors(&first.runs));
    if sim.allocated(&to) != before.sectors.len() as u64 * SECTOR as u64 {
        return Err(format!("the first place filled holes of {to}: the case is not one with holes"));
    }
    // A run over data, one in a hole, and a file a little shorter.
    let patch = Sparse {
        len: (64 << 10) - 1000,
        runs: vec![(100, noise(64, seed + 2)), (50_000, noise(300, seed + 3))],
    };
    let tag = store.place_patched(to.clone(), patch.clone(), false, &mut effects); // clone: kept to compare
    let log = drive_logged(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    patch_kept(&mut sim, &store, &to, &log, answer_of(&placed, tag)?, &before, &patch)
}

/// The invariant ([`patch_kept`]): a patched place where blocks cannot be shared writes the
/// runs into a kernel copy of the file's data, holes kept holes, and renames it onto the path,
/// never writing the path's file; its answer carries the new file's stamp.
#[test]
fn a_patched_place_that_copies_writes_only_the_runs_and_keeps_holes() {
    for seed in 0..8 {
        assert_eq!(patched_with_holes(seed), Ok(()), "seed {seed}");
    }
}

/// A patched place where blocks cannot be shared of a value the store placed, which is also
/// held under the store's own file. `Err` says what the oracle found: [`patch_kept`], and
/// that no object of the store changed. `plant`: the planted bug of a patch written into the
/// path's file in place, the path another name of the store's file (a hard link), so the
/// write reaches both.
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
    // A copy writes the whole value: every sector of it holds a block.
    let whole: BTreeSet<u64> = (0..value.len().div_ceil(SECTOR) as u64).collect();
    let before = Before::of(&mut sim, &store, &to, whole);
    // What a link wrote: two runs, and a file a little shorter.
    let patch = Sparse {
        len: value.len() as u64 - 100,
        runs: vec![(16, noise(64, seed + 1)), (4096, noise(300, seed + 2))],
    };
    let tag = store.place_patched(to.clone(), patch.clone(), false, &mut effects); // clone: kept to compare
    let log = drive_logged(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    patch_kept(&mut sim, &store, &to, &log, answer_of(&placed, tag)?, &before, &patch)?;
    // A place never changes an object of the store.
    if sim.contents(&own) != value {
        return Err(format!("{own}, the store's own file of the value, was changed by a place"));
    }
    match store.get(&key(1)) {
        Ok(Some(entry)) if entry.bytes == value => Ok(()),
        Ok(_) => Err("the store reads other bytes under the value's key".into()),
        Err(corrupt) => Err(format!("the store's value: {corrupt}")),
    }
}

/// The invariant: where blocks cannot be shared a patched place of a value the store placed
/// keeps [`patch_kept`] and changes no object of the store.
#[test]
fn a_patched_place_that_copies_changes_no_object_of_the_store() {
    for seed in 0..8 {
        assert_eq!(patched(seed, false), Ok(()), "seed {seed}");
    }
}

/// The planted-bug control: the patch written into the path's file in place (the route the
/// store took before 2026-11-12 wrote the whole file there), through a path that is another
/// name of the store's own file, and the oracle says the path's file was written.
#[test]
fn the_oracle_catches_a_patch_written_in_place() {
    assert_eq!(patched(3, false), Ok(()), "the store as it is must pass");
    let why = patched(3, true).expect_err("the oracle missed the patch written in place");
    assert!(why.contains("was written in place"), "{why}");
}

/// The invariant: the second path of one program (`Store::place_also`) is a link of the
/// first's staged file, renamed beside it: one kernel copy (or one write of the runs) a
/// build, not one a path; both paths hold the file, and both answers carry the first's
/// stamp: the look at its own path after both renames, one inode's (the simulation keeps a
/// link as one inode under two names). Through a patched place and through a place from a
/// maker's runs.
#[test]
fn a_second_path_of_one_program_is_a_link_of_the_first() {
    for patch in [false, true] {
        let mut sim = Sim::new(31, no_clone());
        let mut store = open(&mut sim, "store");
        let (to, also) = (name("target/debug/deps/app-1"), name("target/debug/app"));
        let file = Sparse { len: 8192, runs: vec![(0, noise(3000, 7)), (6000, noise(100, 8))] };
        let mut effects = Vec::new();
        let (first, runs) = match patch {
            false => (store.place_made(to.clone(), file.clone(), &mut effects), file.clone()), // clone: kept to compare
            true => {
                store.place_made(to.clone(), file.clone(), &mut effects); // clone: kept to compare
                drive(&mut sim, &mut store, &mut effects);
                store.placed();
                let runs = Sparse { len: 8000, runs: vec![(10, noise(50, 9))] };
                (store.place_patched(to.clone(), runs.clone(), false, &mut effects), runs) // clone: kept to compare
            }
        };
        let before = sim.contents(&to);
        let (written, copied) = (own(sim.bytes_written(), &store), sim.bytes_copied());
        let second = store.place_also(first, also.clone()); // clone: kept to compare
        let log = drive_logged(&mut sim, &mut store, &mut effects);
        let placed = store.placed();
        let (a, b) = (answer_of(&placed, first).unwrap(), answer_of(&placed, second).unwrap());
        assert_eq!((&a.result, &b.result), (&Ok(()), &Ok(())), "{placed:?}");
        // One inode under two names, each answer the look at its own path after both renames.
        let (at_to, at_also) = (sim.stamp(&to), sim.stamp(&also));
        assert_eq!(at_to.ino, at_also.ino, "the second path is another name of the first's inode");
        assert_eq!(stamps_whole(&mut sim, [a, b]), Ok(()));
        assert_eq!(a.stamp.map(|it| it.stamp), b.stamp.map(|it| it.stamp), "{placed:?}");
        let want = match patch {
            false => patched_bytes(&[], runs.len, &runs.runs),
            true => patched_bytes(&before, runs.len, &runs.runs),
        };
        assert!(sim.contents(&to) == want && sim.contents(&also) == want, "both paths hold the file");
        let count = |what: fn(&Effect) -> bool| log.iter().filter(|it| what(it)).count();
        assert_eq!(count(|it| matches!(it, Effect::Link { .. })), 1, "{log:?}");
        assert_eq!(count(|it| matches!(it, Effect::CopyData { .. })), usize::from(patch), "{log:?}");
        let run_bytes: u64 = runs.runs.iter().map(|it| it.1.len() as u64).sum();
        assert_eq!(own(sim.bytes_written(), &store) - written, run_bytes, "the runs are written once, not once a path");
        assert_eq!(sim.bytes_copied() - copied, a.bytes.kernel, "one copy a build");
    }
}

/// A patched place of a path that holds no file fails, naming the path and the step.
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
        matches!(&placed[0].result, Err(why) if why.contains("copying the data of target/gone")
            && why.contains("no such file")),
        "{placed:?}"
    );
}

/// The oracle of the stamps answered: each answer placed its path, carries a stamp and a look
/// time, and the stamp is whole what a look at the path gives now (inode, size, modified
/// time, change time, mode). `Err` names the path and what differs.
fn stamps_whole<'a>(sim: &mut Sim, placed: impl IntoIterator<Item = &'a Placed>) -> Result<(), String> {
    let mut seen = 0;
    for it in placed {
        seen += 1;
        it.result.clone().map_err(|why| format!("{}: the place failed: {why}", it.to))?;
        let now = sim.stamp(&it.to);
        match it.stamp {
            Some(PlacedStamp { stamp, looked_ns }) if stamp == now && looked_ns > 0 => {}
            Some(PlacedStamp { stamp, .. }) => {
                let same_file = (stamp.ino, stamp.size, stamp.mtime_ns, stamp.mode)
                    == (now.ino, now.size, now.mtime_ns, now.mode);
                return Err(format!(
                    "{}: the answer's stamp {stamp:?} is not what a look finds now, {now:?}{}",
                    it.to,
                    if same_file { ": its change time is not the path's" } else { "" }
                ));
            }
            None => return Err(format!("{}: placed, and answered no stamp", it.to)),
        }
    }
    match seen {
        0 => Err("no place answered".into()),
        _ => Ok(()),
    }
}

/// Places by every route a store of each kind takes, driven to their ends, and the oracle of
/// their stamps ([`stamps_whole`]) over every answer. `plant`, a planted bug of the stamp.
fn every_route(seed: u64, plant: PlacePlant) -> Result<(), String> {
    let mut answers = Vec::new();
    for faults in [no_clone(), quiet()] {
        let mut sim = Sim::new(seed, faults);
        let mut store = open(&mut sim, "store");
        store.plant = plant;
        // A value kept as a file of its own (cloned where blocks are shared, else written), an
        // output kept whole to clone from (else written), a short record (written), and a
        // derived value of the output (derived from its placed file, else its bytes written).
        let base = noise(64 << 10, seed + 1);
        store.put(key(1), Kind::Image, &noise(EXTERNAL_FROM + 10, seed)).unwrap();
        store.put(key(2), Kind::Output, &base).unwrap();
        store.put(key(3), Kind::Code, &text(3, 900)).unwrap();
        flush(&mut sim, &mut store);
        let mut effects = Vec::new();
        // Each answer is checked against a look as its places end: a later place may replace
        // the path's file (the patch of `target/app` replaces the maker's), and the stamp it
        // answered is the file it placed, not the one after it.
        let mut placed = Vec::new();
        for n in [1, 2, 3] {
            store.place(&key(n), name(&format!("out/{n}")), &mut effects).ok_or("no value to place")?;
        }
        drive(&mut sim, &mut store, &mut effects);
        let now = store.placed();
        stamps_whole(&mut sim, &now)?;
        placed.extend(now);
        let writes = vec![(4096, noise(300, seed + 2)), (40_960, noise(100, seed + 3))];
        let recipe = derived::encode(&key(2), base.len() as u64, &writes).unwrap();
        store.put(key(4), Kind::Derived, &recipe).unwrap();
        flush(&mut sim, &mut store);
        store.place(&key(4), name("out/4"), &mut effects).ok_or("no derived value to place")?;
        drive(&mut sim, &mut store, &mut effects);
        let now = store.placed();
        stamps_whole(&mut sim, &now)?;
        placed.extend(now);
        // A maker's runs, then a patch of them (cloned, or the kernel's copy).
        let to = name("target/app");
        let made = Sparse { len: 32 << 10, runs: vec![(0, noise(2000, seed + 4))] };
        store.place_made(to.clone(), made, &mut effects); // clone: the path is looked at below
        drive(&mut sim, &mut store, &mut effects);
        let now = store.placed();
        stamps_whole(&mut sim, &now)?;
        placed.extend(now);
        let patch = Sparse { len: 30 << 10, runs: vec![(100, noise(64, seed + 5))] };
        store.place_patched(to, patch, false, &mut effects);
        drive(&mut sim, &mut store, &mut effects);
        let now = store.placed();
        stamps_whole(&mut sim, &now)?;
        placed.extend(now);
        if placed.len() != 6 {
            return Err(format!("{} answers of 6 places: {placed:?}", placed.len()));
        }
        answers.extend(placed.into_iter().map(|it| it.to));
    }
    match answers.len() {
        12 => Ok(()),
        n => Err(format!("{n} answers of 12")),
    }
}

/// The invariant: on every route (a value's bytes, a clone of the store's file, a derive, a
/// maker's runs, a patch by clone and by kernel copy), a reported stamp equals what a look at
/// the path gives after the place ended, change time included.
#[test]
fn every_place_answers_the_stamp_a_later_look_finds() {
    for seed in 0..4 {
        assert_eq!(every_route(seed, PlacePlant::None), Ok(()), "seed {seed}");
    }
}

/// The planted-bug control: the staged file's look answered as the path's, the change time
/// the one before the rename (the route of the first stamp, 2026-11-12), fails the oracle on
/// the change time.
#[test]
fn the_oracle_catches_the_staged_files_change_time_answered() {
    assert_eq!(every_route(5, PlacePlant::None), Ok(()), "the store as it is must pass");
    let why = every_route(5, PlacePlant::StampsStaged).expect_err("the oracle missed the staged look answered");
    assert!(why.contains("its change time is not the path's"), "{why}");
}

/// A place of a value's bytes where, between its rename onto the path and its look at the
/// path after it, another writer renames a file of its own onto the path. `Err` says what the
/// oracle found: the place succeeds (its bytes were placed) and answers no stamp. `plant`: the
/// store vouches for whatever its look finds.
fn swapped_before_the_look(seed: u64, clone: bool, plant: bool) -> Result<(), String> {
    let mut sim = Sim::new(seed, if clone { quiet() } else { no_clone() });
    let mut store = open(&mut sim, "store");
    if plant {
        store.plant = PlacePlant::VouchesAnyLook;
    }
    let value = noise(EXTERNAL_FROM + 10, seed);
    store.put(key(5), Kind::Image, &value).unwrap();
    flush(&mut sim, &mut store);
    let (to, theirs) = (name("target/out"), name("target/.out.theirs"));
    let mut effects = Vec::new();
    let tag = store.place(&key(5), to.clone(), &mut effects).ok_or("no value to place")?; // clone: looked at below
    let (mut swapped, mut done) = (false, Vec::new());
    // The bound of the loop, on the simulation's clock: a place ends in milliseconds.
    let end = blaze_core::wait::Deadline::after(sim.now(), Duration::from_secs(60));
    loop {
        // The clock read that begins the store's look at the path is asked only once the
        // rename onto it completed: the other writer's rename lands between the two.
        if !swapped && effects.iter().any(|it| matches!(it, Effect::Now { .. })) {
            swapped = true;
            sim.change(&theirs, b"another writer's file");
            sim.rename(&theirs, &to);
        }
        sim.submit(&mut effects);
        if sim.in_flight() == 0 {
            break;
        }
        sim.poll(&mut done, end);
        if done.is_empty() {
            return Err(format!("the place did not end by {end:?} on the simulation's clock"));
        }
        for completion in done.drain(..) {
            if store.owns(completion.tag) {
                _ = store.complete(completion, &mut effects);
            }
        }
    }
    if !swapped {
        return Err("no look at the path after its rename was asked".into());
    }
    let placed = store.placed();
    let it = answer_of(&placed, tag)?;
    it.result.clone().map_err(|why| format!("the place: {why}"))?;
    match it.stamp {
        None => Ok(()),
        Some(stamp) => Err(format!(
            "{to} vouched for as {stamp:?}: the file another writer renamed there, not the one placed"
        )),
    }
}

/// The invariant: a file another writer renamed onto the path between the place's rename and
/// its look after it is never vouched for: the place answers no stamp (and says why in its
/// span), on the write route and the clone route.
#[test]
fn a_file_swapped_onto_the_path_before_the_look_is_not_vouched_for() {
    for seed in 0..4 {
        for clone in [false, true] {
            assert_eq!(swapped_before_the_look(seed, clone, false), Ok(()), "seed {seed}, clone {clone}");
        }
    }
}

/// The planted-bug control: a store that vouches for any look after the rename vouches for
/// the stranger's file, and the oracle says so.
#[test]
fn the_oracle_catches_a_stranger_vouched_for() {
    assert_eq!(swapped_before_the_look(7, false, false), Ok(()), "the store as it is must pass");
    let why = swapped_before_the_look(7, false, true).expect_err("the oracle missed the stranger vouched for");
    assert!(why.contains("another writer renamed there"), "{why}");
}
