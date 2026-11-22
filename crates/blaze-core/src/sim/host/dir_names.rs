//! A directory made or removed ([`crate::Effect::MakeDir`], [`crate::Effect::RemoveDir`]) is a
//! change of a name of its PARENT directory's, journaled in [`Sim::names`] beside a file's create,
//! rename and unlink, and durable only once the parent is synced ([`crate::Effect::SyncDir`]), as
//! every other name is.
//!
//! What a disk allows, and the source: the tree's rule for names is "a name (create, rename,
//! unlink) is durable only once its directory is synced. A file's sync covers its bytes, never
//! its name" (docs/v2/plan/design/std.md, the strict name rule, decision D55), and sim.rs's own
//! "A name is durable only once its directory is synced, as on a real disk". A `mkdir` is a
//! create of a name in its parent and an `rmdir` an unlink of one: the same `fsync` of the
//! parent's descriptor (Linux) or `F_FULLFSYNC` on it (macOS, std.md "The native calls the model
//! matches") is what makes either durable, and a sync of the directory itself makes the names IN
//! it durable, never its own name, which is its parent's. Before this the simulation kept a made
//! directory through every crash ("durable as it is made, which is looser than a disk"), so a
//! crash test over code that never syncs the parent of a directory it makes passed where a disk
//! loses the directory.
//!
//! The states a crash explores, and why those are the reachable ones:
//! - A directory made and not durable may be gone after a crash, WITH everything under it: every
//!   file and directory made under it since, a file whose bytes were synced and whose name was
//!   synced into it included. Its names were in an inode no durable name reaches; what a disk
//!   keeps of such an orphan is not reachable by a path (fsck's `lost+found` at best), so to a
//!   program it is gone. Whether it is undone is drawn as every change of a name is (one in
//!   three, `Sim::crash_cut`), and undoing it undoes every later change of a name at or under it
//!   first ([`Named::holds`]), so a file renamed INTO it is back under its old name rather than
//!   lost twice.
//! - A directory removed and not durable may be back. It was empty when removed, so what it
//!   holds when back is the names whose removal from it a crash also undoes: an earlier unsynced
//!   unlink under it is drawn on its own, and undoing it forces the removal of the directory
//!   undone too (the directory could not have been removed with the name still in it). So it
//!   comes back empty, or with any of the names it held at its last sync whose removals were
//!   not synced since; and a name under it never outlives a removal of it that was kept.
//! - A directory's change is made durable by a sync of its parent, by a later durable change of
//!   that same name (a removal of it, a make of it again: [`Named::kept_by`]), or by a later
//!   durable removal of a directory above it. A durable change of a name UNDER it never makes it
//!   durable: that is the case of a file synced into a new directory whose parent was not.
//!
//! A directory a file's making made (the drivers make a file's missing directories with it,
//! `fs.rs`'s `making_parent`; [`Sim::make_parents`]) is journaled as one [`crate::Effect::MakeDir`]
//! made: before 2026-11-20 such a directory was implicit, there while a file was under it, had
//! no name of its own to lose, and so survived every crash, where a disk loses a folder
//! `create_dir_all` made and nothing synced the parent of. Every effect that makes a file
//! syncs the parent of each directory it made on the way, before it completes, as the native
//! drivers do since the same day (`fs.rs`'s `make_parent`; the folders rule, on
//! [`crate::Effect`]): a sans-IO caller cannot know which folders its effects made, so only
//! the making can sync them, and a durable call under them (its file synced, its name synced
//! into the folder) is then durable from the root down. Only [`crate::Effect::MakeDir`], a
//! folder asked for by name, waits for its parent's sync. Scenario (5) below holds that, with
//! its control ([`Plant::MadeParentsNotSynced`]).
//!
//! The control is [`Plant::DirNamesDurable`]: the old loose rule, a directory's change kept
//! through every crash, under which the tests below find no seed that loses one.

use super::Plant;
use crate::{
    FileName,
    sim::{Named, Sim},
};

#[cfg(test)]
mod tests;

impl Named {
    /// The directory a directory's change is of; `None` for a file's.
    pub(in crate::sim) fn dir_changed(&self) -> Option<&FileName> {
        match self {
            Named::MadeDir { dir } | Named::RemovedDir { dir } => Some(dir),
            Named::Put { .. } | Named::Gone { .. } | Named::Moved { .. } => None,
        }
    }

    /// Whether a name it changes is `dir` or under it.
    fn within(&self, dir: &FileName) -> bool {
        match self {
            Named::Put { file, .. } | Named::Gone { file, .. } => file.is_under(dir),
            Named::Moved { from, to, .. } => from.is_under(dir) || to.is_under(dir),
            Named::MadeDir { dir: it } | Named::RemovedDir { dir: it } => it.is_under(dir),
        }
    }

    /// Whether it is a directory's change and `other` changes a name at or under that
    /// directory: a crash that undoes the earlier of the two undoes the later
    /// ([`Named::shares`]), as a directory not made holds nothing and a directory not removed
    /// was emptied first.
    pub(in crate::sim) fn holds(&self, other: &Named) -> bool {
        self.dir_changed().is_some_and(|dir| other.within(dir))
    }

    /// Whether `later`, made durable, makes this earlier change durable too (`Sim::sync_names`).
    /// A file's change: whenever the two share a name ([`Named::shares`]), as before. A
    /// directory's change: only when `later` changes that very name (in the same parent) or is
    /// the change of a directory above it; never a change under it, since a directory's sync
    /// makes the names in it durable, never its own name.
    pub(in crate::sim) fn kept_by(&self, later: &Named) -> bool {
        match self.dir_changed() {
            Some(dir) => later.touches(dir) || later.holds(self),
            None => self.shares(later),
        }
    }
}

impl Sim {
    /// A directory's change of a name, journaled as every change of a name is
    /// ([`Sim::names`]): durable once its parent is synced. The planted bug
    /// ([`Plant::DirNamesDurable`]) journals nothing: the loose rule.
    pub(super) fn dir_named(&mut self, named: Named) {
        if self.planted(Plant::DirNamesDurable) {
            return;
        }
        self.named(named);
    }

    /// A crash undid the making of `dir`: it goes, with every file and directory made at or
    /// under it (every later change under it was undone first, `Sim::crash_with` going newest
    /// first). The files' cuts were applied before: what they kept goes with the directory.
    pub(in crate::sim) fn undo_made_dir(&mut self, dir: &FileName) {
        self.files.retain(|name, _| !name.is_under(dir));
        self.host.made.retain(|it| !it.is_under(dir));
    }

    /// A crash undid the removal of `dir`: it is a directory made again, empty but for the
    /// names whose removal the crash undoes next.
    pub(in crate::sim) fn undo_removed_dir(&mut self, dir: FileName) {
        let at = self
            .host
            .made
            .binary_search(&dir)
            .expect_err("a directory whose removal is undone is not made: a later make of it was undone first");
        self.host.made.insert(at, dir);
    }
}
