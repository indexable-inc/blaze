//! Crash tests of a directory's name ([`super`]): a directory made or removed is a change of its
//! parent's names, undone by a crash until the parent is synced. Each scenario runs on every
//! seed of [`SEEDS`], one effect at a time (each completed before the next is asked, so the
//! journal's order is the program's), then one crash; each has its control, the planted loose
//! rule ([`Plant::DirNamesDurable`]: the journaling skipped), under which no seed loses what the
//! scenario says some seed loses: the loss comes from the journaling, removed and watched go.
//! The seeds that lose are printed (`--nocapture` shows them); a failing seed replays exactly.

use super::super::Plant;
use crate::{
    DirName, Done, Effect, FileName, Tag, run_until_idle,
    sim::{Faults, Sim},
};

/// The seeds each scenario runs: the bound of every search here. A change of a name not synced
/// is undone one time in three (`Sim::crash_cut`), so a loss reachable through one such change
/// is missed by all 64 seeds with odds (2/3)^64, about 5e-12, and by none of them for a fixed
/// seed set: the runs are deterministic, the same seeds give the same crashes.
const SEEDS: u64 = 64;

/// What the file synced into the made directory holds.
const KEPT: &[u8] = b"synced bytes, synced name, in a directory whose own name was not";

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

/// A simulation on `seed` with no faults injected, `plant` planted.
fn world(seed: u64, plant: Option<Plant>) -> Sim {
    let mut sim = Sim::new(seed, Faults::default());
    if let Some(plant) = plant {
        sim.plant(plant);
    }
    sim
}

/// `effect` run alone to its completion, which must not be a failure.
fn ok(sim: &mut Sim, effect: Effect) {
    let what = format!("{effect:?}");
    let mut done = None;
    run_until_idle(sim, &mut vec![effect], |it, _| done = Some(it.done))
        .unwrap_or_else(|stalled| panic!("{what}: stalled: {stalled:?}"));
    match done {
        Some(Done::Failed(error)) => panic!("{what}: failed: {error:?}"),
        Some(_) => {}
        None => panic!("{what}: never completed"),
    }
}

fn sync_root(sim: &mut Sim, tag: u64) {
    ok(sim, Effect::SyncDir { dir: DirName::Root, tag: Tag(tag) });
}

/// The seeds of [`SEEDS`] on which `lost` holds, printed under `what`.
fn seeds_where(what: &str, mut lost: impl FnMut(u64) -> bool) -> Vec<u64> {
    let seeds: Vec<u64> = (0..SEEDS).filter(|seed| lost(*seed)).collect();
    println!("{what}: {} of {SEEDS} seeds: {seeds:?}", seeds.len());
    seeds
}

/// (1) `d` made; the root (its parent) synced when `sync_parent`; a crash. Whether `d` is gone.
fn made_dir_lost(seed: u64, plant: Option<Plant>, sync_parent: bool) -> bool {
    let mut sim = world(seed, plant);
    ok(&mut sim, Effect::MakeDir { dir: name("d"), tag: Tag(1) });
    if sync_parent {
        sync_root(&mut sim, 2);
    }
    sim.crash();
    !sim.is_dir(&name("d"))
}

/// (1): an unsynced `MakeDir` is lost on some seed, and on none once its parent is synced.
///
/// Prints `test sim::host::dir_names::tests::an_unsynced_made_dir_is_lost_until_its_parent_is_synced ... ok`.
#[test]
fn an_unsynced_made_dir_is_lost_until_its_parent_is_synced() {
    let lost = seeds_where("an unsynced made d lost", |seed| made_dir_lost(seed, None, false));
    assert!(!lost.is_empty(), "no seed of {SEEDS} lost a directory made with its parent never synced");
    let lost = seeds_where("a made d lost after its parent's sync", |seed| made_dir_lost(seed, None, true));
    assert!(lost.is_empty(), "seeds {lost:?} lost a directory made after its parent was synced");
}

/// Control of (1): with the journaling skipped no seed loses the unsynced made directory.
///
/// Prints `test sim::host::dir_names::tests::control_an_unsynced_made_dir_is_kept_under_the_loose_rule ... ok`.
#[test]
fn control_an_unsynced_made_dir_is_kept_under_the_loose_rule() {
    let lost = seeds_where("loose rule: an unsynced made d lost", |seed| {
        made_dir_lost(seed, Some(Plant::DirNamesDurable), false)
    });
    assert!(lost.is_empty(), "seeds {lost:?} lost a made directory with the journaling skipped: the loss is not the journal's");
}

/// What a crash left of `d` and `d/f` in (2).
#[derive(Debug, PartialEq, Eq)]
enum Left {
    /// `d` there and `d/f` in it holding [`KEPT`].
    Both,
    /// `d` gone and nothing under it.
    Neither,
}

/// (2) `d` made; `d/f` appended, its bytes synced and its name synced into `d` (a `SyncDir` of
/// `d` itself); the root synced when `sync_parent`; a crash. What is left, or the broken
/// invariant: `d/f` without `d`, or `d/f` there with other bytes (both were synced).
fn synced_file_in_made_dir(seed: u64, plant: Option<Plant>, sync_parent: bool) -> Result<Left, String> {
    let mut sim = world(seed, plant);
    let (d, f) = (name("d"), name("d/f"));
    ok(&mut sim, Effect::MakeDir { dir: d.clone(), tag: Tag(1) });
    ok(&mut sim, Effect::Append { file: f.clone(), bytes: KEPT.to_vec(), tag: Tag(2) });
    ok(&mut sim, Effect::Sync { file: f.clone(), tag: Tag(3) });
    ok(&mut sim, Effect::SyncDir { dir: d.clone().into(), tag: Tag(4) });
    if sync_parent {
        sync_root(&mut sim, 5);
    }
    sim.crash();
    match (sim.is_dir(&d), sim.exists(&f)) {
        (true, true) if sim.contents(&f) == KEPT => Ok(Left::Both),
        (true, true) => Err(format!("seed {seed}: d/f holds {:?}, not its synced bytes", sim.contents(&f))),
        (false, false) => Ok(Left::Neither),
        (made, there) => Err(format!(
            "seed {seed}: d a directory {made}, d/f there {there}: a name outlived its directory or a synced name went alone"
        )),
    }
}

/// (2): a file whose bytes and name (in its directory) were synced, in a directory whose own
/// name was not, is lost with the directory on some seed, never alone, and on no seed once the
/// directory's parent is synced.
///
/// Prints `test sim::host::dir_names::tests::a_synced_file_goes_with_its_unsynced_directory ... ok`.
#[test]
fn a_synced_file_goes_with_its_unsynced_directory() {
    let left = |sync_parent: bool| {
        move |seed: u64| match synced_file_in_made_dir(seed, None, sync_parent) {
            Ok(left) => left == Left::Neither,
            Err(why) => panic!("{why}"),
        }
    };
    let lost = seeds_where("d and the synced d/f lost", left(false));
    assert!(!lost.is_empty(), "no seed of {SEEDS} lost the directory, and its synced file with it, whose parent was never synced");
    let lost = seeds_where("d and d/f lost after the root's sync", left(true));
    assert!(lost.is_empty(), "seeds {lost:?} lost a synced file in a directory whose parent was synced");
}

/// Control of (2): with the journaling skipped no seed loses the directory or its synced file.
///
/// Prints `test sim::host::dir_names::tests::control_a_synced_file_in_a_new_dir_is_kept_under_the_loose_rule ... ok`.
#[test]
fn control_a_synced_file_in_a_new_dir_is_kept_under_the_loose_rule() {
    let lost = seeds_where("loose rule: d and d/f lost", |seed| {
        match synced_file_in_made_dir(seed, Some(Plant::DirNamesDurable), false) {
            Ok(left) => left == Left::Neither,
            Err(why) => panic!("{why}"),
        }
    });
    assert!(lost.is_empty(), "seeds {lost:?} lost the synced file with the journaling skipped: the loss is not the journal's");
}

/// (3) `d` made and the root synced (`d` durable); `d` removed; the root synced again when
/// `sync_parent`; a crash. Whether `d` is back.
fn removed_dir_back(seed: u64, plant: Option<Plant>, sync_parent: bool) -> bool {
    let mut sim = world(seed, plant);
    ok(&mut sim, Effect::MakeDir { dir: name("d"), tag: Tag(1) });
    sync_root(&mut sim, 2);
    ok(&mut sim, Effect::RemoveDir { dir: name("d"), tag: Tag(3) });
    if sync_parent {
        sync_root(&mut sim, 4);
    }
    sim.crash();
    sim.is_dir(&name("d"))
}

/// (3): an unsynced `RemoveDir` comes back on some seed, and on none once its parent is synced.
///
/// Prints `test sim::host::dir_names::tests::an_unsynced_removed_dir_comes_back_until_its_parent_is_synced ... ok`.
#[test]
fn an_unsynced_removed_dir_comes_back_until_its_parent_is_synced() {
    let back = seeds_where("an unsynced removed d back", |seed| removed_dir_back(seed, None, false));
    assert!(!back.is_empty(), "no seed of {SEEDS} brought back a directory removed with its parent never synced");
    let back = seeds_where("a removed d back after its parent's sync", |seed| removed_dir_back(seed, None, true));
    assert!(back.is_empty(), "seeds {back:?} brought back a directory removed before its parent was synced");
}

/// Control of (3): with the journaling skipped no seed brings the removed directory back.
///
/// Prints `test sim::host::dir_names::tests::control_an_unsynced_removed_dir_stays_gone_under_the_loose_rule ... ok`.
#[test]
fn control_an_unsynced_removed_dir_stays_gone_under_the_loose_rule() {
    let back = seeds_where("loose rule: an unsynced removed d back", |seed| {
        removed_dir_back(seed, Some(Plant::DirNamesDurable), false)
    });
    assert!(back.is_empty(), "seeds {back:?} brought back a removed directory with the journaling skipped: the return is not the journal's");
}

/// What a crash left of a removed `d` that held `d/f` at its last sync, in (4).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
enum Back {
    /// `d` stayed removed, nothing under it.
    Gone,
    /// `d` back, empty.
    Empty,
    /// `d` back with `d/f` in it, its synced bytes.
    WithItsName,
}

/// (4) `d` made, the root synced; `d/f` appended, synced, its name synced into `d`; `d/f`
/// removed and `d` removed, neither synced; a crash. What is left, or the broken invariant.
fn removed_with_a_name(seed: u64) -> Result<Back, String> {
    let mut sim = world(seed, None);
    let (d, f) = (name("d"), name("d/f"));
    ok(&mut sim, Effect::MakeDir { dir: d.clone(), tag: Tag(1) });
    sync_root(&mut sim, 2);
    ok(&mut sim, Effect::Append { file: f.clone(), bytes: KEPT.to_vec(), tag: Tag(3) });
    ok(&mut sim, Effect::Sync { file: f.clone(), tag: Tag(4) });
    ok(&mut sim, Effect::SyncDir { dir: d.clone().into(), tag: Tag(5) });
    ok(&mut sim, Effect::Remove { file: f.clone(), tag: Tag(6) });
    ok(&mut sim, Effect::RemoveDir { dir: d.clone(), tag: Tag(7) });
    sim.crash();
    match (sim.is_dir(&d), sim.exists(&f)) {
        (false, false) => Ok(Back::Gone),
        (true, false) => Ok(Back::Empty),
        (true, true) if sim.contents(&f) == KEPT => Ok(Back::WithItsName),
        (made, there) => Err(format!(
            "seed {seed}: d made {made}, d/f there {there}, holding {:?}: a name outlived the kept removal of its directory, or came back with other bytes",
            sim.contents(&f)
        )),
    }
}

/// (4): an unsynced removal of a directory may be undone, the directory back empty or with the
/// name it held at its last sync; a name under it never outlives a removal of it that was kept.
/// Every one of the three outcomes is reached on some seed.
///
/// Prints `test sim::host::dir_names::tests::a_removed_dir_comes_back_empty_or_with_its_synced_names ... ok`.
#[test]
fn a_removed_dir_comes_back_empty_or_with_its_synced_names() {
    let mut first: [Option<u64>; 3] = [None; 3];
    for seed in 0..SEEDS {
        let back = removed_with_a_name(seed).unwrap_or_else(|why| panic!("{why}"));
        let at = back as usize;
        first[at] = first[at].or(Some(seed));
    }
    println!("first seed of each (gone, back empty, back with d/f): {first:?}");
    for (at, outcome) in [Back::Gone, Back::Empty, Back::WithItsName].into_iter().enumerate() {
        assert!(first[at].is_some(), "no seed of {SEEDS} reached {outcome:?}: {first:?}");
    }
}

/// (5) `a/b/f` written durably ([`Effect::Write`]) where neither `a` nor `a/b` is there, the
/// write making both; then `u/w/v` made by an unsynced append (making `u` and `u/w`), its
/// bytes synced and its name synced into `u/w`, the steps of bstd's `close_synced`; a crash.
/// Whether each file is gone. Both promise the file under its name after any crash, which
/// needs each folder made on the way synced into its parent by the effect that made it (the
/// folders rule, on [`Effect`]; `fs.rs`'s `make_parent`): the append's own sync and the sync
/// of `u/w` cover the bytes and the name in `u/w`, never `u/w`'s name in `u` or `u`'s in the
/// root.
fn written_into_new_folders(seed: u64, plant: Option<Plant>) -> (bool, bool) {
    let mut sim = world(seed, plant);
    let (f, v) = (name("a/b/f"), name("u/w/v"));
    ok(&mut sim, Effect::Write { file: f.clone(), bytes: KEPT.to_vec(), tag: Tag(1) });
    ok(&mut sim, Effect::Append { file: v.clone(), bytes: KEPT.to_vec(), tag: Tag(2) });
    ok(&mut sim, Effect::Sync { file: v.clone(), tag: Tag(3) });
    ok(&mut sim, Effect::SyncDir { dir: name("u/w").into(), tag: Tag(4) });
    sim.crash();
    let kept = |sim: &Sim, file: &FileName| sim.exists(file) && sim.contents(file) == KEPT;
    (!kept(&sim, &f), !kept(&sim, &v))
}

/// (5): a durable write into folders it made survives every crash, and so does a file synced
/// with its name into folders an unsynced append made.
///
/// Prints `test sim::host::dir_names::tests::a_durable_write_into_new_folders_survives_every_crash ... ok`.
#[test]
fn a_durable_write_into_new_folders_survives_every_crash() {
    let lost = seeds_where("a/b/f, written durably, lost", |seed| written_into_new_folders(seed, None).0);
    assert!(lost.is_empty(), "seeds {lost:?} lost a file `Effect::Write` called durable, with the folders it made");
    let lost = seeds_where("u/w/v, appended and synced, lost", |seed| written_into_new_folders(seed, None).1);
    assert!(lost.is_empty(), "seeds {lost:?} lost a file synced with its name into folders its append made");
}

/// Control of (5): with the made folders' parents not synced (the drivers before 2026-11-20),
/// some seed loses each file: the survival is the makings' syncs.
///
/// Prints `test sim::host::dir_names::tests::control_a_durable_write_is_lost_with_its_folders_left_unsynced ... ok`.
#[test]
fn control_a_durable_write_is_lost_with_its_folders_left_unsynced() {
    let planted = |seed| written_into_new_folders(seed, Some(Plant::MadeParentsNotSynced));
    let lost = seeds_where("parents not synced: a/b/f lost", |seed| planted(seed).0);
    assert!(!lost.is_empty(), "no seed of {SEEDS} lost the write with its folders' parents unsynced: the survival is not the syncs'");
    let lost = seeds_where("parents not synced: u/w/v lost", |seed| planted(seed).1);
    assert!(!lost.is_empty(), "no seed of {SEEDS} lost the appended file with its folders' parents unsynced: the survival is not the syncs'");
}
