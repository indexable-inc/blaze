//! What each file call asks of the device, effect by effect, on the simulated disk: the
//! contract of each durability (docs/v2/plan/design/std.md, "Files: whether bytes reached the
//! disk is in the type"), as the effects a driver is handed, not as what a crash keeps (that
//! is [`super::durable`]'s).
//!
//! 1. A call told `Unsynced::No` (`Durable::No`) hands the driver no effect that syncs or
//!    writes back anything: no `Sync`, `Barrier`, `WriteBack`, `SyncDir`, whole-file `Write`
//!    or `Rename` (a real driver's rename syncs both directories: `F_FULLFSYNC` on macOS). A
//!    rename of its goes as `RenameUnsynced`.
//! 2. A call told `Unsynced::Started` adds only what it begins and does not wait for: a
//!    `WriteBack` of the file it changed, and a `SyncDir` of the directory whose names it
//!    changed. It waits for the device no more than `No` does.
//! 3. A `_synced` call hands over exactly its contract's syncs, in order. A synced `Rename`
//!    syncs the directories it changes, on every driver and in the simulation alike (the
//!    strict name rule, 2026-11-16), so no `SyncDir` follows one: a publish flushes the new
//!    file, then renames it over the path; a rename renames, then flushes the file at its new
//!    name; a remove removes, then syncs its directory.
//!
//! Paths both directly in the directory under test and in one beneath it, so the name sync
//! asked is of the right directory.
//!
//! Plants: [`Plant::UnsyncedRenameSyncs`] (an unsynced call that renames through the syncing
//! rename) and [`Plant::DurableSkipsNameSync`] (a `_synced` call that skips its directory's
//! name sync).

use std::time::Duration;

use blaze_core::{DirName, Effect, FileName, sim::Faults};
use blaze_sim::{Config, Tape};

use super::{check, control, path, root};
use crate::{
    Plant,
    fs::{Content, Durable, FsError, FsKind},
    io::{Io, Limits, SimWorld},
};

/// The directory under test, beneath the driver's root.
const DIR: &str = "data";

/// One directly in [`DIR`], one in a directory beneath it.
const PATHS: [&str; 2] = ["top", "d/deep"];

const DURABLE: [Durable; 3] = [Durable::No, Durable::Started, Durable::Yes];

#[derive(Clone, Copy, Debug)]
enum Call {
    Publish { at: usize, durable: Durable },
    /// A file made unsynced at `from`'s name plus `.new`, then renamed over `at`'s path.
    Rename { at: usize, durable: Durable },
    Remove { at: usize, durable: Durable },
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    latency_us: u64,
    calls: Vec<Call>,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let latency_us = *tape.one_of(&[0, 50, 2_000], "latency us");
    let calls = (0..tape.int_in(1..=12u64, "calls"))
        .map(|_| {
            let at = tape.int_in(0..=1u64, "at") as usize;
            let durable = *tape.one_of(&DURABLE, "durable");
            match tape.int_in(0..=2u64, "call") {
                0 => Call::Publish { at, durable },
                1 => Call::Rename { at, durable },
                _ => Call::Remove { at, durable },
            }
        })
        .collect();
    Plan { seed, latency_us, calls }
}

/// An effect that syncs, writes back or orders something, as the oracle names it; `None`
/// for every other.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Syncs {
    Sync(FileName),
    Barrier(FileName),
    WriteBack(FileName),
    SyncDir(DirName),
    /// A whole-file write, durable when it completes.
    Write(FileName),
    /// The rename that syncs both directories on a real driver.
    Rename { from: FileName, to: FileName },
}

fn syncs(effect: &Effect) -> Option<Syncs> {
    // clone: the oracle's copy of a name, a refcount.
    Some(match effect {
        Effect::Sync { file, .. } => Syncs::Sync(file.clone()),
        Effect::Barrier { file, .. } => Syncs::Barrier(file.clone()),
        Effect::WriteBack { file, .. } => Syncs::WriteBack(file.clone()),
        Effect::SyncDir { dir, .. } => Syncs::SyncDir(dir.clone()),
        Effect::Write { file, .. } => Syncs::Write(file.clone()),
        Effect::Rename { from, to, .. } => Syncs::Rename { from: from.clone(), to: to.clone() },
        _ => return None,
    })
}

/// The directory `name` is in, as an effect names it.
fn dir_of(name: &FileName) -> DirName {
    match name.as_str().rsplit_once('/') {
        Some((dir, _)) => DirName::Named(FileName::new(dir).expect("a name's directory is a name")),
        None => DirName::Root,
    }
}

fn name(raw: &str) -> FileName {
    FileName::new(raw).expect("a test's name")
}

/// Whether `asked`, what one call told `durable` handed the driver, is its contract; the
/// call changed `to`, and a synced rename moved `from` there.
fn keeps_contract(call: Call, durable: Durable, asked: &[Effect], from: Option<&FileName>, to: &FileName) -> Result<(), String> {
    let got: Vec<Syncs> = asked.iter().filter_map(syncs).collect();
    let renames_unsynced = asked.iter().any(|it| matches!(it, Effect::RenameUnsynced { to: onto, .. } if onto == to));
    let renames = matches!(call, Call::Publish { .. } | Call::Rename { .. });
    let fail = |why: &str| Err(format!("{call:?}: {why}: it asked {got:?} of {} effects", asked.len()));
    match durable {
        Durable::No => {
            if !got.is_empty() {
                return fail("told No, it asked a sync");
            }
            if renames && !renames_unsynced {
                return fail("told No, its rename was not `RenameUnsynced`");
            }
        }
        Durable::Started => {
            let mut wanted = Vec::new();
            if renames {
                // clone: the oracle's names, refcounts.
                wanted.push(Syncs::WriteBack(to.clone()));
            }
            wanted.push(Syncs::SyncDir(dir_of(to)));
            if got != wanted {
                return fail(&format!("told Started, it did not begin exactly {wanted:?}"));
            }
            if renames && !renames_unsynced {
                return fail("told Started, its rename was not `RenameUnsynced`");
            }
        }
        Durable::Yes => {
            // clone: the oracle's names, refcounts.
            let wanted = match (call, from) {
                // The synced rename syncs its directories: no `SyncDir` after it.
                (Call::Publish { .. }, Some(temp)) => vec![
                    Syncs::Sync(temp.clone()),
                    Syncs::Rename { from: temp.clone(), to: to.clone() },
                ],
                (Call::Rename { .. }, Some(from)) => vec![
                    Syncs::Rename { from: from.clone(), to: to.clone() },
                    Syncs::Sync(to.clone()),
                ],
                (Call::Remove { .. }, None) => vec![Syncs::SyncDir(dir_of(to))],
                _ => return fail("harness: no rename's source"),
            };
            if got != wanted {
                return fail(&format!("told Yes, it did not ask exactly {wanted:?} in order"));
            }
        }
    }
    Ok(())
}

/// The file a synced publish flushed before its rename: the one its `Sync` names.
fn temp_of(asked: &[Effect]) -> Option<FileName> {
    // clone: the oracle's name, a refcount.
    asked.iter().find_map(|it| match it {
        Effect::Sync { file, .. } => Some(file.clone()),
        _ => None,
    })
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let mut world = SimWorld::new(plan.seed, Limits::USUAL);
    world.faults = Faults { max_latency: Duration::from_micros(plan.latency_us), ..Faults::default() };
    let io = Io::planted(world, plant);
    let cx = root(&io);
    let fs = io.need_fs().map_err(|e| format!("harness: {e}"))?;
    let dir = fs.open_dir(&cx, &path(DIR)).map_err(|e| format!("harness: {e}"))?;
    io.record();
    for (step, &call) in plan.calls.iter().enumerate() {
        let harness = |error: FsError| format!("harness: call {step} ({call:?}): {error}");
        let (at, durable) = match call {
            Call::Publish { at, durable } | Call::Rename { at, durable } | Call::Remove { at, durable } => (at, durable),
        };
        let to = name(&format!("{DIR}/{}", PATHS[at]));
        let mut from = None;
        let source = format!("{}.new", PATHS[at]);
        if let Call::Rename { .. } = call {
            // The source, made unsynced and not under test: what it asked is dropped below.
            dir.publish(&cx, &path(&source), Content::bytes(b"renamed"), Durable::No).map_err(harness)?;
            from = Some(name(&format!("{DIR}/{source}")));
        }
        io.asked();
        let waits = io.disk().ok_or("harness: the world is not simulated")?.device_waits;
        match call {
            Call::Publish { .. } => {
                dir.publish(&cx, &path(PATHS[at]), Content::bytes(b"published"), durable).map_err(harness)?;
            }
            Call::Rename { .. } => {
                dir.rename_replacing(&cx, &path(&source), &path(PATHS[at]), durable).map_err(harness)?;
            }
            Call::Remove { .. } => dir.remove(&cx, &path(PATHS[at]), durable).map_err(harness)?,
        }
        let asked = io.asked();
        if let Call::Publish { .. } = call
            && durable == Durable::Yes
        {
            from = temp_of(&asked);
        }
        keeps_contract(call, durable, &asked, from.as_ref(), &to).map_err(|why| format!("call {step}: {why}"))?;
        let waited = io.disk().ok_or("harness: the world is not simulated")?.device_waits - waits;
        if durable != Durable::Yes && waited != 0 {
            return Err(format!("call {step} ({call:?}): told {durable:?}, it waited for the device {waited} times"));
        }
    }
    Ok(())
}

/// The invariant: an unsynced call asks nothing of the device (`Started` only begins a
/// write-back and a name sync), and a synced one asks exactly its contract's syncs, in order.
#[test]
fn each_call_asks_the_device_for_exactly_what_its_durability_says() {
    blaze_sim::check(Config::cases(200), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_an_unsynced_rename_that_syncs() {
    control(Plant::UnsyncedRenameSyncs, plan, run);
}

/// `Dir::entries`: one level of a folder, sorted by the names' bytes, a folder among the files
/// as a folder; a folder not there is `None`; a file is not a folder.
#[test]
fn entries_list_one_level_sorted_by_bytes() {
    let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
    let cx = root(&io);
    let top = io.need_fs().expect("Io::sim offers files").root();
    for (at, it) in ["src/b.rs", "src/a.rs", "src/Z", "src/deep/x.rs"].iter().enumerate() {
        top.publish(&cx, &path(it), Content::bytes(&b"bytes"[..at]), Durable::No).expect("a publish");
    }
    let entries = top.entries(&cx, &path("src")).expect("a listing").expect("the folder is there");
    let seen: Vec<(&str, crate::fs::EntryKind)> = entries.iter().collect();
    use crate::fs::EntryKind::{Dir, File};
    assert_eq!(seen, [("Z", File), ("a.rs", File), ("b.rs", File), ("deep", Dir)]);
    assert_eq!(entries.len_of(entries.find("a.rs").unwrap()), 1);
    assert_eq!(entries.path(3).unwrap().as_str(), "src/deep");
    assert_eq!(top.entries(&cx, &path("missing")).expect("no folder is no error"), None);
    let refused = top.entries(&cx, &path("src/a.rs")).unwrap_err();
    assert_eq!(refused.kind, FsKind::NotADirectory);

    // A directory lists itself (`Dir::own_entries`): the root, and a named one, the same
    // columns as the listing of it from above, with no folder and each path its own name.
    let own = top.own_entries(&cx).expect("the root lists itself");
    assert_eq!((own.folder(), own.iter().collect::<Vec<_>>()), (None, vec![("src", Dir)]));
    assert_eq!(own.path(0).unwrap().as_str(), "src");
    let src = top.subdir(&path("src")).own_entries(&cx).expect("a named directory lists itself");
    assert_eq!(src.iter().collect::<Vec<_>>(), seen);
    assert_eq!(src.path(3).unwrap().as_str(), "deep");
    // The refusals: a directory not there, and a file in its place, each named.
    for (it, kind) in [("missing", "no such directory"), ("src/a.rs", "no such directory")] {
        let refused = top.subdir(&path(it)).own_entries(&cx).unwrap_err();
        let said = refused.to_string();
        assert!(said.contains(it) && said.contains(kind), "{said}");
    }
}

/// The driver's root as a `Dir` (`Fs::root`): it lists every file at any depth by its path
/// from the root, its name sync is the root's (`DirName::Root`), and a synced publish in it,
/// whose name sync is the root's, is there after a crash, on every seed. (An unsynced
/// rename's name is not kept by a later directory sync in blaze-core's disk model, which
/// undoes it until the file's bytes sync: not asserted here.)
#[test]
fn the_root_lists_and_syncs_as_any_directory() {
    for seed in 0..64 {
        let mut world = SimWorld::new(seed, Limits::USUAL);
        world.faults = Faults { max_latency: Duration::from_micros(seed * 7), ..Faults::default() };
        let io = Io::simulated(world);
        let cx = root(&io);
        let fs = io.need_fs().expect("Io::sim offers files");
        let top = fs.root();
        assert_eq!(top.name(), &DirName::Root);
        // An empty root lists as empty: it is there before anything is made in it.
        let frame = blaze_mem::Frame::with_reserve(1 << 20, blaze_mem::Tag::Other).expect("a test frame");
        // Every file beneath the root at any depth, by a walk entering every folder.
        let files_of = |frame: &blaze_mem::Frame| -> Vec<String> {
            let mut files = Vec::new();
            let limits = crate::fs::WalkLimits { folders: 64, entries: 1024, depth: 8 };
            top.walk(&cx, None, limits, frame, |entries, enter| {
                files.extend((0..entries.len()).filter(|&i| entries.kind(i) == crate::fs::EntryKind::File).map(|i| entries.path(i).expect("a path").as_str().to_owned()));
                enter.enter_all();
                Ok::<(), core::convert::Infallible>(())
            })
            .expect("the root's walk");
            files
        };
        assert!(files_of(&frame).is_empty(), "seed {seed}");
        top.publish(&cx, &path("kept"), Content::bytes(b"kept"), Durable::Yes).expect("a synced publish at the root");
        top.publish(&cx, &path("d/deep"), Content::bytes(b"deep"), Durable::No).expect("a publish beneath the root");
        top.publish(&cx, &path("a.new"), Content::bytes(b"a"), Durable::No).expect("the source");
        top.rename_replacing(&cx, &path("a.new"), &path("a"), Durable::No).expect("an unsynced rename at the root");
        // The walk's order (fs/walk.rs): a folder's own files before its sub-folders.
        assert_eq!(files_of(&frame), ["a", "kept", "d/deep"], "seed {seed}");
        io.record();
        io.asked();
        top.sync_names(&cx).expect("the root's names synced");
        let asked = io.asked();
        assert!(matches!(asked.as_slice(), [Effect::SyncDir { dir: DirName::Root, .. }]), "seed {seed}: {asked:?}");
        assert!(io.crash(), "the world is simulated");
        let names = files_of(&frame);
        assert!(names.iter().any(|it| it == "kept"), "seed {seed}: {names:?}");
        assert!(names.iter().all(|it| ["a", "a.new", "kept"].contains(&it.as_str()) || it.starts_with("d/")), "seed {seed}: {names:?}");
    }
}

#[test]
fn the_oracle_catches_a_synced_call_that_skips_the_directory_sync() {
    control(Plant::DurableSkipsNameSync, plan, run);
}
