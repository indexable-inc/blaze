//! Directories made and removed ([`Dir::make_dir`], [`Dir::remove_dir`]), each in the three
//! forms every name change has (the module's doc above: `_unsynced`, `_synced`, and the
//! [`Durable`] chosen at run time), over blaze-core's `Effect::MakeDir` and
//! `Effect::RemoveDir`.
//!
//! - A directory made is there, empty, at once: a file need not be made under it first. Its
//!   parent must be there (the driver's root always is); something already at its name is
//!   [`FsKind::Exists`], so "make it unless it is there" is a match on that kind, never a stat
//!   before.
//! - A directory removed must be empty: a full one is refused by the driver, naming it, and
//!   nothing under it is removed (there is no recursive remove here: a caller that wants one
//!   walks the folder, [`Dir::walk`], and removes what it found, each removal its own).
//! - Durability is a name change's: the `_synced` form syncs the names of the directory the
//!   new or removed one is in ([`Effect::SyncDir`], the root's for a name directly under it)
//!   before it returns; the `_unsynced` form syncs nothing, or starts that sync with
//!   [`Unsynced::Started`].
//!
//! What a crash keeps, on the simulated disk today: a directory made or removed stays made or
//! removed whichever form made it (blaze-core `sim`, `Sim::names`: "a directory's own name is
//! durable as it is made, which is looser than a disk"). So the simulation cannot yet tell a
//! synced make from an unsynced one by a crash; the tests (`tests/dirs.rs`) check the sync by
//! the disk's ledger ([`crate::disk::DiskCounts::name_syncs`]) instead, until the simulation
//! journals a made directory's name as it journals a file's.
//!
//! Locking a file (`Effect::Lock`) is not here: its release is a linear value (an `Unlock`
//! that must be ended), which wants its own short spec first.

use core::fmt::Write as _;

use blaze_core::{Done, Effect, FileName, IoError};

use super::{Dir, Durable, FsError, FsKind, FsOp, ReadWrite, Unsynced, call, names_started, stat, sync_names, unexpected};
use crate::{cx::Cx, path::Path};

/// The planted bug of the removal's test (`tests/dirs.rs`): the driver's refusal to remove a
/// directory (one that is full, missing or a file) is taken as done, the "quietly ignored"
/// class rule 3 forbids. Only the test that must catch it plants it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DirsPlant {
    None,
    RefusalIgnored,
}

impl Dir<ReadWrite> {
    /// The directory `path` beneath this one made, empty, as durably as `durable` says:
    /// [`Dir::make_dir_unsynced`] or [`Dir::make_dir_synced`], chosen at run time. Gives the
    /// directory made, with this one's rights and quota.
    pub fn make_dir(&self, cx: &Cx, path: &Path, durable: Durable) -> Result<Dir<ReadWrite>, FsError> {
        match durable.unsynced() {
            Some(unsynced) => self.make_dir_unsynced(cx, path, unsynced),
            None => self.make_dir_synced(cx, path),
        }
    }

    /// The directory `path` made, empty, with nothing synced: a crash may undo it on a real
    /// disk. Something already at `path` is [`FsKind::Exists`]; a missing parent fails naming
    /// it.
    pub fn make_dir_unsynced(&self, cx: &Cx, path: &Path, unsynced: Unsynced) -> Result<Dir<ReadWrite>, FsError> {
        let name = self.beneath(path);
        self.made(cx, &name)?;
        names_started(&self.io, &name, unsynced);
        Ok(self.subdir(path))
    }

    /// The directory `path` made, empty, and the names of the directory it is in synced: a
    /// crash keeps it. Something already at `path` is [`FsKind::Exists`].
    pub fn make_dir_synced(&self, cx: &Cx, path: &Path) -> Result<Dir<ReadWrite>, FsError> {
        let name = self.beneath(path);
        self.made(cx, &name)?;
        sync_names(&self.io, cx, FsOp::MakeDir, &name, None)?;
        Ok(self.subdir(path))
    }

    /// The empty directory `path` removed, as durably as `durable` says:
    /// [`Dir::remove_dir_unsynced`] or [`Dir::remove_dir_synced`], chosen at run time.
    pub fn remove_dir(&self, cx: &Cx, path: &Path, durable: Durable) -> Result<(), FsError> {
        self.remove_dir_in(cx, path, durable, DirsPlant::None)
    }

    /// The empty directory `path` removed, with nothing synced: a crash may bring it back on a
    /// real disk. One that is full fails naming it, and nothing under it is removed; one not
    /// there is [`FsKind::NotFound`]; a file there is [`FsKind::NotADirectory`].
    pub fn remove_dir_unsynced(&self, cx: &Cx, path: &Path, unsynced: Unsynced) -> Result<(), FsError> {
        self.remove_dir_in(cx, path, Durable::from(unsynced), DirsPlant::None)
    }

    /// The empty directory `path` removed, and the names of the directory it was in synced: a
    /// crash keeps it gone. Refused as [`Dir::remove_dir_unsynced`] is.
    pub fn remove_dir_synced(&self, cx: &Cx, path: &Path) -> Result<(), FsError> {
        self.remove_dir_in(cx, path, Durable::Yes, DirsPlant::None)
    }

    /// [`Dir::remove_dir`], with the test's planted bug when `plant` says so.
    pub(crate) fn remove_dir_in(&self, cx: &Cx, path: &Path, durable: Durable, plant: DirsPlant) -> Result<(), FsError> {
        let name = self.beneath(path);
        // clone: the effect's name, a refcount.
        let dir = name.clone();
        match call(&self.io, cx, FsOp::RemoveDir, &name, |tag| Effect::RemoveDir { dir, tag })? {
            Done::RemovedDir => {}
            Done::Failed(_) if plant == DirsPlant::RefusalIgnored => {}
            Done::Failed(error) => return Err(self.refused(cx, FsOp::RemoveDir, &name, error)),
            done => return Err(unexpected(FsOp::RemoveDir, &name, done)),
        }
        match durable.unsynced() {
            Some(unsynced) => {
                names_started(&self.io, &name, unsynced);
                Ok(())
            }
            None => sync_names(&self.io, cx, FsOp::RemoveDir, &name, None),
        }
    }

    /// `name` made empty by the driver; nothing synced.
    fn made(&self, cx: &Cx, name: &FileName) -> Result<(), FsError> {
        // clone: the effect's name, a refcount.
        let dir = name.clone();
        match call(&self.io, cx, FsOp::MakeDir, name, |tag| Effect::MakeDir { dir, tag })? {
            Done::MadeDir => Ok(()),
            Done::Failed(error) => Err(self.refused(cx, FsOp::MakeDir, name, error)),
            done => Err(unexpected(FsOp::MakeDir, name, done)),
        }
    }

    /// The driver's refusal of `op` on `name`, which said `why`, as the kind a caller matches
    /// on: what is at `name` now is looked at (one stat, on the failure path only) to say
    /// [`FsKind::Exists`] for a make over something, [`FsKind::NotFound`] or
    /// [`FsKind::NotADirectory`] for a remove of nothing or of a file; anything else, a full
    /// directory or a missing parent, is the driver's own words ([`FsKind::Driver`]). A look
    /// that itself fails gives the driver's words too, with the look's failure after them.
    fn refused(&self, cx: &Cx, op: FsOp, name: &FileName, why: IoError) -> FsError {
        let IoError(mut why) = why;
        let kind = match (op, stat(&self.io, cx, op, name)) {
            (FsOp::MakeDir, Ok(Some(_))) => FsKind::Exists,
            (FsOp::RemoveDir, Ok(None)) => FsKind::NotFound,
            (FsOp::RemoveDir, Ok(Some(meta))) if !meta.dir => FsKind::NotADirectory,
            (_, Ok(_)) => FsKind::Driver(why),
            (_, Err(looked)) => {
                // alloc: an error's message, grown. Writing into a growable string cannot fail
                // but by running out of memory, which aborts.
                write!(why, "; and the look at what is there failed: {looked}").expect("text written into a string");
                FsKind::Driver(why)
            }
        };
        FsError::new(op, name, kind)
    }
}
