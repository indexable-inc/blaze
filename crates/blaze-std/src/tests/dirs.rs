//! Directories made and removed ([`Dir::make_dir`], [`Dir::remove_dir`]) on the simulation,
//! through a crash that tears unsynced writes (`Io::crash`: each file keeps what was flushed
//! and a prefix of what was not; blaze-core `sim`).
//!
//! 1. `make_dir(a, Durable::Yes)` waits for a sync of the names of the directory `a` is in
//!    before it returns (the disk's ledger, `DiskCounts::name_syncs`), and after a crash `a`
//!    is there, a directory. Plant: [`Plant::DurableSkipsNameSync`] (the `_synced` form skips
//!    the name sync), caught by the crash: the simulation journals a made directory as a change
//!    of its parent's names (blaze-core `sim/host/dir_names.rs`), so on some seed the crash
//!    loses `a` ([`Finding::MadeLost`]); on a seed whose crash happened to keep it, the ledger,
//!    checked after the crash, catches it ([`Finding::NoNameSync`]). A second control proves
//!    the crash is real: a file published unsynced under `a` before the crash is lost or torn
//!    on at least one seed.
//! 2. `remove_dir` of a directory that holds a file fails naming it (`FsOp::RemoveDir`, the
//!    directory's name) and removes nothing under it; once emptied it is removed and stays
//!    removed through a crash; a make over a name that is there is `FsKind::Exists`, a remove
//!    of one not there `FsKind::NotFound`. Plant: [`DirsPlant::RefusalIgnored`] (the driver's
//!    refusal taken as done), caught.
//!
//! Findings are typed ([`Finding`]), each naming its seed.

use blaze_core::{DirName, FileName};

use super::{path, root, world, write_synced};
use crate::{
    Plant,
    fs::{Content, Dir, DirsPlant, Durable, FsError, FsKind, FsOp, ReadWrite, Unsynced},
    io::Io,
};

/// The seeds each test runs.
const SEEDS: u64 = 64;

/// What the file published unsynced under the made directory holds.
const TORN: &[u8] = b"published unsynced under the made directory, torn or lost by a crash";

/// What a run found wrong: a broken invariant, or the harness's own step failing.
#[derive(Debug)]
enum Finding {
    /// A step of the harness, not of the invariant, failed: `step` on seed `seed`.
    Harness { seed: u64, step: &'static str, error: FsError },
    /// The world has no disk ledger or does not crash: it is not simulated.
    NotSimulated,
    /// `make_dir(a, Durable::Yes)` returned having synced no directory's names.
    NoNameSync { seed: u64, before: u64, after: u64 },
    /// `make_dir(a)` gave a directory of another name.
    WrongDir { seed: u64, made: DirName },
    /// After a crash `a` is a file.
    MadeIsFile { seed: u64 },
    /// `make_dir(a, Durable::Yes)` returned, and a crash lost `a`.
    MadeLost { seed: u64 },
    /// A make over the `a` made did not fail as `Exists` naming `a`.
    ExistsNotSaid { seed: u64, got: Option<FsError> },
    /// `remove_dir(a)` of an `a` holding a file said done.
    FullRemoved { seed: u64 },
    /// `remove_dir(a)` of a full `a` failed, but not as the driver's refusal naming `a`.
    FullRefusedAs { seed: u64, error: FsError },
    /// The file under `a` changed or went under a refused `remove_dir`.
    FileTouched { seed: u64, error: Option<FsError> },
    /// `remove_dir(a)` of an `a` already removed did not fail as `NotFound` naming `a`.
    GoneNotSaid { seed: u64, got: Option<FsError> },
    /// `remove_dir(a, Durable::Yes)` returned, and a crash brought `a` back.
    CameBack { seed: u64 },
}

/// The world's root directory; a world that withholds files is not the simulation's default.
fn top(io: &Io) -> Result<Dir<ReadWrite>, Finding> {
    io.need_fs().map(|fs| fs.root()).map_err(|_| Finding::NotSimulated)
}

fn name_syncs(io: &Io) -> Result<u64, Finding> {
    io.disk().map(|it| it.name_syncs).ok_or(Finding::NotSimulated)
}

/// Whether `error` names `want` (a name under the driver's root).
fn names(error: &FsError, want: &str) -> bool {
    error.path.name().map(FileName::as_str) == Some(want)
}

/// `step` on seed `seed` failed: the harness's own.
fn harness(seed: u64, step: &'static str) -> impl FnOnce(FsError) -> Finding {
    move |error| Finding::Harness { seed, step, error }
}

/// One seed of (1): `Ok(torn)` when the invariant held, `torn` saying whether the crash lost or
/// cut the unsynced file.
fn made_then_crashed(seed: u64, plant: Plant) -> Result<bool, Finding> {
    let io = world(seed, plant);
    let cx = root(&io);
    let top = top(&io)?;
    let before = name_syncs(&io)?;
    let made = top.make_dir(&cx, &path("a"), Durable::Yes).map_err(harness(seed, "make_dir(a, Durable::Yes)"))?;
    let after = name_syncs(&io)?;
    if made.name().name().map(FileName::as_str) != Some("a") {
        // clone: a finding's name, a refcount.
        return Err(Finding::WrongDir { seed, made: made.name().clone() });
    }
    made.publish_unsynced(&cx, &path("torn"), Content::bytes(TORN), Unsynced::No).map_err(harness(seed, "publish a/torn"))?;
    if !io.crash() {
        return Err(Finding::NotSimulated);
    }
    match top.stat(&cx, &path("a")).map_err(harness(seed, "stat a"))? {
        Some(meta) if meta.dir => {}
        Some(_) => return Err(Finding::MadeIsFile { seed }),
        None => return Err(Finding::MadeLost { seed }),
    }
    // The ledger last: a skipped name sync that this seed's crash happened to keep is caught
    // here.
    if after <= before {
        return Err(Finding::NoNameSync { seed, before, after });
    }
    let mut into = cx.bytes();
    match made.read_all(&cx, &path("torn"), &mut into) {
        Ok(_) => Ok(into.as_slice() != TORN),
        Err(error) if error.kind == FsKind::NotFound => Ok(true),
        Err(error) => Err(Finding::Harness { seed, step: "read a/torn", error }),
    }
}

/// The invariant (1): a directory made with `Durable::Yes` waited for its parent's name sync
/// and is there after a crash, on every seed of [`SEEDS`]; and the crash was real (some seed
/// lost or tore what was not synced).
///
/// Prints `test tests::dirs::a_made_dir_survives_a_crash_when_synced ... ok`.
#[test]
fn a_made_dir_survives_a_crash_when_synced() {
    let mut torn = 0u32;
    for seed in 0..SEEDS {
        match made_then_crashed(seed, Plant::None) {
            Ok(lost) => torn += u32::from(lost),
            Err(finding) => panic!("{finding:?}"),
        }
    }
    assert!(torn > 0, "no seed of {SEEDS} lost or tore the unsynced file under a: the crash tore nothing, so it proves nothing");
}

/// Control of (1): a `_synced` make that skips its name sync is caught on every seed of
/// [`SEEDS`], and on at least one by the crash itself losing `a` ([`Finding::MadeLost`]): a
/// change of a name not synced is undone one time in three (blaze-core `Sim::crash_cut`), so
/// all 64 seeds keep `a` with odds (2/3)^64, and the seeds are fixed, so the run is the same
/// every time. The search is bounded by [`SEEDS`].
///
/// Prints `test tests::dirs::a_made_dir_test_catches_a_skipped_name_sync ... ok`.
#[test]
fn a_made_dir_test_catches_a_skipped_name_sync() {
    let (mut lost, mut first) = (0u32, None);
    for seed in 0..SEEDS {
        match made_then_crashed(seed, Plant::DurableSkipsNameSync) {
            Err(Finding::MadeLost { seed }) => {
                lost += 1;
                first = first.or(Some(seed));
            }
            Err(Finding::NoNameSync { .. }) => {}
            found => panic!("seed {seed}: the planted skipped name sync was not caught: {found:?}"),
        }
    }
    std::println!("skipped name sync: a lost on {lost} of {SEEDS} seeds, the first seed {first:?}");
    assert!(lost > 0, "no seed of {SEEDS} lost the directory made without its parent's sync: the crash cannot lose a directory, so it proves nothing");
}

/// One seed of (2).
fn full_then_emptied(seed: u64, plant: DirsPlant) -> Result<(), Finding> {
    let io = world(seed, Plant::None);
    let cx = root(&io);
    let top = top(&io)?;
    let a = top.make_dir(&cx, &path("a"), Durable::Yes).map_err(harness(seed, "make_dir(a)"))?;
    match top.make_dir(&cx, &path("a"), Durable::No) {
        Err(error) if error.kind == FsKind::Exists && error.op == FsOp::MakeDir && names(&error, "a") => {}
        Err(error) => return Err(Finding::ExistsNotSaid { seed, got: Some(error) }),
        Ok(_) => return Err(Finding::ExistsNotSaid { seed, got: None }),
    }
    write_synced(&cx, &a, &path("f"), b"kept").map_err(harness(seed, "publish a/f"))?;
    match top.remove_dir_in(&cx, &path("a"), Durable::Yes, plant) {
        Err(error) if error.op == FsOp::RemoveDir && names(&error, "a") && matches!(error.kind, FsKind::Driver(_)) => {}
        Err(error) => return Err(Finding::FullRefusedAs { seed, error }),
        Ok(()) => return Err(Finding::FullRemoved { seed }),
    }
    let mut into = cx.bytes();
    match a.read_all(&cx, &path("f"), &mut into) {
        Ok(_) if into.as_slice() == b"kept" => {}
        Ok(_) => return Err(Finding::FileTouched { seed, error: None }),
        Err(error) => return Err(Finding::FileTouched { seed, error: Some(error) }),
    }
    a.remove_synced(&cx, &path("f")).map_err(harness(seed, "remove a/f"))?;
    top.remove_dir_in(&cx, &path("a"), Durable::Yes, plant).map_err(harness(seed, "remove_dir(a) of an empty a"))?;
    match top.remove_dir(&cx, &path("a"), Durable::No) {
        Err(error) if error.kind == FsKind::NotFound && names(&error, "a") => {}
        Err(error) => return Err(Finding::GoneNotSaid { seed, got: Some(error) }),
        Ok(()) => return Err(Finding::GoneNotSaid { seed, got: None }),
    }
    if !io.crash() {
        return Err(Finding::NotSimulated);
    }
    match top.stat(&cx, &path("a")).map_err(harness(seed, "stat a"))? {
        None => Ok(()),
        Some(_) => Err(Finding::CameBack { seed }),
    }
}

/// The invariant (2), on every seed of [`SEEDS`].
///
/// Prints `test tests::dirs::remove_dir_of_a_full_dir_fails_naming_it ... ok`.
#[test]
fn remove_dir_of_a_full_dir_fails_naming_it() {
    for seed in 0..SEEDS {
        if let Err(finding) = full_then_emptied(seed, DirsPlant::None) {
            panic!("{finding:?}");
        }
    }
}

/// Control of (2): a removal that takes the driver's refusal as done is caught.
///
/// Prints `test tests::dirs::remove_dir_test_catches_an_ignored_refusal ... ok`.
#[test]
fn remove_dir_test_catches_an_ignored_refusal() {
    for seed in 0..SEEDS {
        let found = full_then_emptied(seed, DirsPlant::RefusalIgnored);
        assert!(matches!(found, Err(Finding::FullRemoved { .. })), "seed {seed}: the planted ignored refusal was not caught: {found:?}");
    }
}
