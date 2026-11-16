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
//! 3. A `_synced` call hands over exactly its contract's syncs, in order. On the simulated
//!    disk (whose rename syncs no names, so the name sync is its own effect): a publish
//!    flushes the new file, renames it over the path, then syncs the path's directory; a
//!    rename renames, flushes the file at its new name, then syncs that directory; a remove
//!    removes, then syncs its directory.
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
    fs::{Content, Durable, FsError},
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
                (Call::Publish { .. }, Some(temp)) => vec![
                    Syncs::Sync(temp.clone()),
                    Syncs::Rename { from: temp.clone(), to: to.clone() },
                    Syncs::SyncDir(dir_of(to)),
                ],
                (Call::Rename { .. }, Some(from)) => vec![
                    Syncs::Rename { from: from.clone(), to: to.clone() },
                    Syncs::Sync(to.clone()),
                    Syncs::SyncDir(dir_of(to)),
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

#[test]
fn the_oracle_catches_a_synced_call_that_skips_the_directory_sync() {
    control(Plant::DurableSkipsNameSync, plan, run);
}
