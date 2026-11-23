//! The simulation's disk as a journal of changes sees it:
//! each file and directory has an inode and change times that move as a real file system's
//! do, and a journal of every change survives [`super::Sim::crash`], as FSEvents' store and
//! NTFS's USN journal survive a reboot. [`Journal`] says which system it plays; the test
//! resets, truncates or loses parts of it to check that a watcher names each loss and looks
//! again only where it was.

use blaze_hash::{FixedState, Map};
use blaze_own::Share;
use std::alloc::Global;

use super::SimFile;
use crate::{Cursor, DirName, FileName, Journaled, Lost, S_IFDIR, S_IFLNK, S_IFREG, Stamp};

/// Which system's journal the simulation plays.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Journal {
    /// Linux: none survives a restart, so every [`crate::Effect::Journal`] from a cursor says
    /// [`Lost::NoJournal`] for its whole directory, and its batches have no cursor.
    #[default]
    None,
    /// FSEvents: history comes in several batches, `caught_up` once it is all told.
    FsEvents,
    /// NTFS's USN journal: history in one batch.
    Usn,
}

enum Entry {
    Changed(FileName),
    /// A name that arrived by a rename or a link (its destination): told as a change, kept
    /// apart only so [`RootPlant::RenameIntoRootAtSourceOnly`] can drop it.
    Moved(FileName),
    Lost(FileName, Lost),
}

/// A bug planted in the journal's telling of the driver's root, for the root journal's
/// simulation test (its oracle must catch each); [`RootPlant::None`] everywhere else.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) enum RootPlant {
    #[default]
    None,
    /// A name directly in the root not told (as a journal tiled by the root's folders would
    /// miss it: a create, a write or a removal at the root's own level).
    #[cfg_attr(not(test), expect(dead_code, reason = "a test control's plant"))]
    RootNamesDropped,
    /// A rename or link INTO the root told only at its source.
    #[cfg_attr(not(test), expect(dead_code, reason = "a test control's plant"))]
    RenameIntoRootAtSourceOnly,
}

/// The device every simulated file is on: not 0, which a real driver reports where it knows
/// none.
pub(super) const SIM_DEV: u64 = 0x5155;

#[derive(Clone, Copy)]
struct Inode {
    ino: u64,
    mtime: u64,
    ctime: u64,
}

#[derive(Default)]
pub(super) struct Disk {
    pub(super) kind: Journal,
    /// The last change time given: the simulation's mtimes and ctimes, in nanoseconds since
    /// the epoch.
    clock: u64,
    /// The wall clock now ([`super::Sim::wall_ns`]), set as the virtual clock moves ([`Disk::at`]).
    wall_ns: u64,
    next_ino: u64,
    /// Every file and directory there, by name.
    inodes: Map<FileName, Inode, FixedState, Global>,
    /// The journal's ID, the sequence number of its oldest entry kept, and its entries.
    id: u64,
    first: u64,
    log: Vec<Entry>,
    /// The name a rename just made: its touch moves its change time only, as a rename does.
    renamed_to: Option<FileName>,
    /// The planted bug of the root journal's test; `None` but there.
    pub(super) plant: RootPlant,
}

/// The simulated disk's tree as the journal's disk reads it: the files, and every directory
/// there but the root (`Host::made`), so a stamp, a listing and a stat agree on what a
/// directory is.
#[derive(Clone, Copy)]
pub(super) struct Tree<'a> {
    pub(super) files: &'a Map<FileName, SimFile, FixedState, Global>,
    /// Sorted.
    pub(super) dirs: &'a [FileName],
}

impl Tree<'_> {
    fn is_dir(&self, name: &FileName) -> bool {
        self.dirs.binary_search(name).is_ok()
    }
}

fn parent(name: &FileName) -> Option<FileName> {
    name.as_str().rfind('/').and_then(|at| FileName::new(&name.as_str()[..at]))
}

impl Disk {
    /// The wall clock is `wall_ns` from now on: each change after this is stamped with it.
    pub(super) fn at(&mut self, wall_ns: u64) {
        self.wall_ns = wall_ns;
    }

    /// A change's time: the wall clock now, as a real file system stamps one, so a look within
    /// the racy window of it sees a racy stamp. Two changes at one virtual instant are a
    /// nanosecond apart: change times never repeat here, so the racy rule is exercised by
    /// looks inside the window, not by a second write under an equal stamp.
    fn tick(&mut self) -> u64 {
        self.clock = self.wall_ns.max(self.clock + 1);
        self.clock
    }

    fn new_inode(&mut self) -> Inode {
        self.next_ino += 1;
        let now = self.tick();
        Inode { ino: self.next_ino, mtime: now, ctime: now }
    }

    /// The sequence number the next entry gets.
    fn end(&self) -> u64 {
        self.first + self.log.len() as u64
    }

    fn cursor(&self) -> Cursor {
        match self.kind {
            Journal::None => Cursor::None,
            Journal::FsEvents | Journal::Usn => Cursor::Sim { journal: self.id, next: self.end() },
        }
    }

    /// `file` changed, and is in the tree's files now or not: its inode and the directory it
    /// is in move as on a real disk (a directory's mtime moves when a name in it is made or
    /// goes; a directory is there until removed, [`Disk::dir_gone`]), and the journal records
    /// it. Returns the cursor after the change.
    pub(super) fn touched(&mut self, file: &FileName, tree: Tree<'_>) -> Cursor {
        let there = tree.files.contains_key(file);
        let now = self.tick();
        let renamed = self.renamed_to.as_ref() == Some(file);
        if renamed {
            self.renamed_to = None;
        }
        match (there, self.inodes.get_mut(file)) {
            (true, Some(inode)) if renamed => {
                inode.ctime = now;
                self.share(file);
            }
            (true, Some(inode)) => {
                (inode.mtime, inode.ctime) = (now, now);
                self.share(file);
            }
            (true, None) => {
                let inode = self.new_inode();
                // clone: the simulated disk's name of a new file.
                self.inodes.insert(file.share(), inode);
                self.entered(file);
            }
            (false, Some(_)) => {
                self.inodes.remove(file);
                self.left(file);
            }
            (false, None) => self.left(file),
        }
        // clone: the simulated journal's entry, kept across crashes.
        let named = file.share();
        self.log.push(if renamed { Entry::Moved(named) } else { Entry::Changed(named) });
        self.cursor()
    }

    /// A name was made under the directories above `name`: the first that was there already
    /// has a name more; those that were not are made.
    fn entered(&mut self, name: &FileName) {
        // clone: a name of the simulated disk's, walked up.
        let mut at = name.share();
        while let Some(up) = parent(&at) {
            let now = self.tick();
            match self.inodes.get_mut(&up) {
                Some(inode) => {
                    (inode.mtime, inode.ctime) = (now, now);
                    return;
                }
                None => {
                    let inode = self.new_inode();
                    // clone: the simulated disk's name of a new directory.
                    self.inodes.insert(up.share(), inode);
                    // clone: the simulated journal's entry.
                    self.log.push(Entry::Changed(up.share()));
                }
            }
            at = up;
        }
    }

    /// The name `name` went from the directory it is in: that directory has a name fewer and
    /// stays, empty or not, as a real one does until it is removed ([`Disk::dir_gone`]).
    fn left(&mut self, name: &FileName) {
        let Some(up) = parent(name) else { return };
        let now = self.tick();
        if let Some(inode) = self.inodes.get_mut(&up) {
            (inode.mtime, inode.ctime) = (now, now);
        }
    }

    /// The directory `dir` was removed ([`crate::Effect::RemoveDir`]), with every inode at or
    /// under it (a removal of one not empty is only a planted bug's): a directory made again
    /// under the name is a new inode, and the one it was in has a name fewer.
    pub(super) fn dir_gone(&mut self, dir: &FileName) {
        self.inodes.retain(|name, _| !name.is_under(dir));
        self.left(dir);
        // clone: the simulated journal's entry.
        self.log.push(Entry::Changed(dir.share()));
    }

    /// `from` is renamed `to`, keeping its inode and mtime; called before the two are told as
    /// changed, `from` first.
    pub(super) fn renamed(&mut self, from: &FileName, to: &FileName) {
        if let Some(inode) = self.inodes.remove(from) {
            // clone: the simulated disk's new name of the file.
            self.inodes.insert(to.share(), inode);
            self.entered(to);
            // clone: as above, until its touch.
            self.renamed_to = Some(to.share());
        }
    }

    /// `to` made another name of `from`'s inode (a hard link): one inode, so the same `ino`,
    /// mtime and change time under both, and the link moves that change time, as a link
    /// count's change does; `to`'s touch, which comes next, moves its change time only, as a
    /// rename's. Every later change of either name is the other's ([`Disk::share`]). A name
    /// of the pair removed leaves the other its inode.
    pub(super) fn linked(&mut self, from: &FileName, to: &FileName, tree: Tree<'_>) {
        // The source's inode, made if it was never looked at.
        self.stamp(from, tree);
        let mut inode = self.inodes[from];
        inode.ctime = self.tick();
        // clone: the simulated disk's second name of the file.
        self.inodes.insert(to.share(), inode);
        self.entered(to);
        self.share(to);
        // clone: as a rename's, until its touch.
        self.renamed_to = Some(to.share());
    }

    /// Every other name of `name`'s inode (a hard link, [`Disk::linked`]) takes its times:
    /// one inode under two names. A few names a simulation, so a walk of them all.
    fn share(&mut self, name: &FileName) {
        let Some(inode) = self.inodes.get(name).copied() else { return };
        for (other, it) in self.inodes.iter_mut() {
            if it.ino == inode.ino && other != name {
                *it = inode;
            }
        }
    }

    /// A chmod: the inode's change time moves, its mtime does not.
    pub(super) fn attributes(&mut self, file: &FileName) -> Cursor {
        let now = self.tick();
        if let Some(inode) = self.inodes.get_mut(file) {
            inode.ctime = now;
            self.share(file);
        }
        // clone: the simulated journal's entry.
        self.log.push(Entry::Changed(file.share()));
        self.cursor()
    }

    /// A `utimensat` of `name`: its inode's mtime is `modified_ns` and its change time moves, as
    /// on a real disk. `false`, and nothing done, where `name` is neither a file nor a directory.
    pub(super) fn times(&mut self, name: &FileName, modified_ns: u64, tree: Tree<'_>) -> bool {
        if self.stamp(name, tree) == Stamp::GONE {
            return false;
        }
        let now = self.tick();
        let inode = self.inodes.get_mut(name).expect("stamped, so it has an inode");
        (inode.mtime, inode.ctime) = (modified_ns, now);
        self.share(name);
        // clone: the simulated journal's entry.
        self.log.push(Entry::Changed(name.share()));
        true
    }

    /// A stat or a listing's [`crate::Meta`] of `name`, there with the length `len`, a directory
    /// if `dir`: its inode (one is given to a name first seen) and both times, which every
    /// change moves, on the one simulated device, [`SIM_DEV`].
    pub(super) fn meta(&mut self, name: &FileName, len: u64, dir: bool) -> crate::Meta {
        if !self.inodes.contains_key(name) {
            let inode = self.new_inode();
            // clone: the simulated disk's name of a file or directory first seen.
            self.inodes.insert(name.share(), inode);
        }
        let inode = self.inodes[name];
        crate::Meta {
            len,
            dir,
            modified_ns: inode.mtime,
            changed_ns: inode.ctime,
            ino: inode.ino,
            dev: SIM_DEV,
        }
    }

    /// What `name` is now.
    pub(super) fn stamp(&mut self, name: &FileName, tree: Tree<'_>) -> Stamp {
        if let Some(file) = tree.files.get(name) {
            if !self.inodes.contains_key(name) {
                let inode = self.new_inode();
                // clone: the simulated disk's name of a file first stamped.
                self.inodes.insert(name.share(), inode);
            }
            let inode = self.inodes[name];
            return Stamp {
                ino: inode.ino,
                size: file.len(),
                mtime_ns: inode.mtime,
                ctime_ns: inode.ctime,
                dev: SIM_DEV,
                mode: if file.link { S_IFLNK | 0o777 } else { S_IFREG | (file.mode & 0o7777) },
            };
        }
        if tree.is_dir(name) {
            if !self.inodes.contains_key(name) {
                let inode = self.new_inode();
                // clone: the simulated disk's name of a directory first stamped.
                self.inodes.insert(name.share(), inode);
            }
            let inode = self.inodes[name];
            return Stamp {
                ino: inode.ino,
                size: 0,
                mtime_ns: inode.mtime,
                ctime_ns: inode.ctime,
                dev: SIM_DEV,
                mode: S_IFDIR | 0o755,
            };
        }
        Stamp::GONE
    }

    /// What is directly in `dir`, sorted; `None` when it is no directory.
    pub(super) fn entries(&mut self, dir: &crate::DirName, tree: Tree<'_>) -> Option<crate::Listing> {
        // The root is always there; any other is a directory made and not removed.
        if dir.name().is_some_and(|name| !tree.is_dir(name)) {
            return None;
        }
        // Each entry directly in `dir`, once: a file's or a directory's own name, or the first
        // part of a deeper one (a directory).
        let mut firsts: Vec<&str, Global> = Vec::new_in(Global);
        let under = tree.files.keys().chain(tree.dirs.iter());
        firsts.extend(under.filter(|it| Some(*it) != dir.name() && dir.holds(it)).map(|it| {
            let rest = &it.as_str()[dir.start()..];
            rest.split('/').next().expect("a part")
        }));
        firsts.sort_unstable();
        firsts.dedup();
        // clone: the listing's directory, a refcount.
        let mut listing = crate::ListingBuilder::new(dir.share());
        for first in firsts {
            let name = dir.child(first).expect("a directory's name and one part are a name");
            let stamp = self.stamp(&name, tree);
            listing.push(first.as_bytes(), stamp).expect("the simulation's names are names, under the caps");
        }
        Some(listing.finish().expect("the simulation's names, sorted, each once"))
    }

    /// The batches an [`crate::Effect::Journal`] of `dir` from `since` is told first: the
    /// history since then, split in `parts` (FSEvents') or one, the last caught up.
    pub(super) fn history(&self, dir: &DirName, since: &Cursor, parts: u64) -> Vec<Journaled> {
        let whole = |lost: Option<Lost>| Journaled {
            // alloc: none, an empty vector.
            files: Vec::new(),
            // alloc: none, an empty vector.
            lost: Vec::new(),
            whole: lost,
            itself: false,
            cursor: self.cursor(),
            caught_up: true,
        };
        if self.kind == Journal::None {
            // alloc: the batches' payload.
            return vec![whole((*since != Cursor::None).then_some(Lost::NoJournal))];
        }
        let next = match *since {
            // alloc: the batches' payload.
            Cursor::None => return vec![whole(None)],
            Cursor::Sim { journal, next } if journal == self.id && next <= self.end() => next,
            // alloc: the batches' payload.
            Cursor::Sim { journal, .. } if journal == self.id => {
                return vec![whole(Some(Lost::Reset))];
            }
            // alloc: the batches' payload.
            _ => return vec![whole(Some(Lost::Reset))],
        };
        if next < self.first {
            // alloc: the batches' payload.
            return vec![whole(Some(Lost::Truncated))];
        }
        let entries = &self.log[(next - self.first) as usize..];
        let parts = match self.kind {
            Journal::FsEvents => parts.clamp(1, entries.len().max(1) as u64) as usize,
            _ => 1,
        };
        let each = entries.len().div_ceil(parts).max(1);
        // alloc: the batches' payload.
        let mut out = Vec::new();
        let mut at = next;
        for chunk in entries.chunks(each) {
            at += chunk.len() as u64;
            let mut batch = Journaled {
                // alloc: the batch's payload.
                files: Vec::new(),
                // alloc: as above.
                lost: Vec::new(),
                whole: None,
                itself: false,
                cursor: Cursor::Sim { journal: self.id, next: at },
                caught_up: false,
            };
            for entry in chunk {
                self.add(dir, entry, &mut batch);
            }
            batch.files.sort_unstable();
            batch.files.dedup();
            out.push(batch);
        }
        match out.last_mut() {
            Some(last) => last.caught_up = true,
            None => out.push(whole(None)),
        }
        out
    }

    /// `entry` as a journal of `dir` (a named directory or the root) tells it: a name strictly
    /// beneath `dir` in `files`, `dir`'s own in `itself`; a loss beneath it in `lost`, one of
    /// `dir` or above it in `whole`. A rename is two entries, its source and its destination,
    /// each told where it is, so one into the root is told at the root.
    fn add(&self, dir: &DirName, entry: &Entry, batch: &mut Journaled) {
        let (file, moved) = match entry {
            Entry::Changed(file) => (file, false),
            Entry::Moved(file) => (file, true),
            Entry::Lost(under, why) => {
                if dir.name().is_some_and(|dir| dir.is_under(under)) {
                    batch.whole = batch.whole.or(Some(*why));
                } else if dir.holds(under) {
                    // clone: the batch's payload, a name of it.
                    batch.lost.push((under.share(), *why));
                }
                return;
            }
        };
        if dir.name() == Some(file) {
            batch.itself = true;
            return;
        }
        if !dir.holds(file) {
            return;
        }
        let at_the_root = dir.name().is_none() && !file.as_str().contains('/');
        match self.plant {
            RootPlant::RootNamesDropped if at_the_root => return,
            RootPlant::RenameIntoRootAtSourceOnly if at_the_root && moved => return,
            RootPlant::None | RootPlant::RootNamesDropped | RootPlant::RenameIntoRootAtSourceOnly => {}
        }
        // clone: the batch's payload, a name of it.
        batch.files.push(file.share());
    }

    /// The live batch of the change just recorded, for a journal of `dir`; `None` when the
    /// change is not under it.
    pub(super) fn live(&self, dir: &DirName, dropped: bool) -> Option<Journaled> {
        let entry = self.log.last()?;
        let (cursor, caught_up) = (self.cursor(), true);
        // alloc: none, empty vectors, the batch's payload once pushed to.
        let mut batch = Journaled { files: Vec::new(), lost: Vec::new(), whole: None, itself: false, cursor, caught_up };
        self.add(dir, entry, &mut batch);
        if dropped {
            batch.files.clear();
            batch.lost.clear();
            batch.itself = false;
            batch.whole = Some(Lost::Dropped);
        }
        let told = !batch.files.is_empty() || !batch.lost.is_empty() || batch.whole.is_some() || batch.itself;
        told.then_some(batch)
    }

    /// The journal made again with a new ID and nothing in it (FSEvents' store reset, the USN
    /// journal deleted and made again).
    pub(super) fn reset(&mut self) {
        self.id += 1;
        self.first = 0;
        self.log.clear();
    }

    /// Every entry before now dropped: the journal no longer reaches back past now.
    pub(super) fn truncate(&mut self) {
        self.first = self.end();
        self.log.clear();
    }

    /// The system loses the changes under `dir` (FSEvents' `MustScanSubDirs`).
    pub(super) fn lose(&mut self, dir: &FileName, why: Lost) {
        // clone: the simulated journal's entry.
        self.log.push(Entry::Lost(dir.share(), why));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Completion, Done, Driver, Effect, Names, Tag,
        sim::{Faults, Sim},
    };

    fn name(it: &str) -> FileName {
        FileName::new(it).unwrap()
    }

    fn stamp(sim: &mut Sim, it: &str) -> Stamp {
        sim.stamp(&name(it))
    }

    /// Every batch a journal of `src` from `since` is told until it is caught up, then unwatched.
    fn journal(sim: &mut Sim, since: Cursor) -> Vec<Journaled> {
        journal_of(sim, DirName::Named(name("src")), since)
    }

    /// Every batch a journal of `dir` from `since` is told until it is caught up, then unwatched.
    fn journal_of(sim: &mut Sim, dir: DirName, since: Cursor) -> Vec<Journaled> {
        let mut effects = vec![Effect::Journal { dir, since, tag: Tag(5) }];
        let (mut out, mut done) = (Vec::new(), Vec::new());
        loop {
            sim.submit(&mut effects);
            sim.poll(&mut done, crate::wait::Deadline::NEVER);
            for Completion { tag, done } in done.drain(..) {
                assert_eq!(tag, Tag(5));
                match done {
                    Done::Journaled(batch) => {
                        let caught_up = batch.caught_up;
                        out.push(*batch);
                        if caught_up {
                            effects.push(Effect::Unwatch { tag: Tag(5) });
                        }
                    }
                    Done::Unwatched => return out,
                    done => panic!("{done:?}"),
                }
            }
        }
    }

    #[test]
    fn stamps_move_as_a_disk_s_do() {
        let mut sim = Sim::new(1, Faults::default());
        sim.change(&name("src/a/one.rs"), b"1");
        sim.change(&name("src/b.rs"), b"2");
        let (file, dir, top) =
            (stamp(&mut sim, "src/a/one.rs"), stamp(&mut sim, "src/a"), stamp(&mut sim, "src"));
        assert!(file.is_file() && dir.is_dir() && top.is_dir());
        assert_eq!(stamp(&mut sim, "src/none"), Stamp::GONE);
        // A write moves the file's times, not its directory's.
        sim.change(&name("src/a/one.rs"), b"one");
        let written = stamp(&mut sim, "src/a/one.rs");
        assert_eq!((written.ino, written.size), (file.ino, 3));
        assert!(written.mtime_ns > file.mtime_ns);
        assert_eq!(stamp(&mut sim, "src/a"), dir);
        // A chmod moves the change time only.
        sim.chmod(&name("src/a/one.rs"), 0o755);
        let moded = stamp(&mut sim, "src/a/one.rs");
        assert_eq!(moded.mtime_ns, written.mtime_ns);
        assert!(moded.ctime_ns > written.ctime_ns && moded.executable());
        // A new name moves its directory; a rename keeps the inode and mtime, not the ctime.
        sim.rename(&name("src/a/one.rs"), &name("src/a/two.rs"));
        let renamed = stamp(&mut sim, "src/a/two.rs");
        assert_eq!((renamed.ino, renamed.size, renamed.mtime_ns), (file.ino, 3, moded.mtime_ns));
        assert!(renamed.ctime_ns > moded.ctime_ns);
        assert!(stamp(&mut sim, "src/a").mtime_ns > dir.mtime_ns);
        // A directory left empty stays, as a disk's does (the conformance suite's
        // `a_folder_outlives_its_last_file`), and moves; its parent does not.
        let emptied = stamp(&mut sim, "src/a");
        sim.delete(&name("src/a/two.rs"));
        let empty = stamp(&mut sim, "src/a");
        assert!(empty.is_dir() && empty.ino == dir.ino && empty.mtime_ns > emptied.mtime_ns);
        let mut names = Names::default();
        for it in ["src/b.rs", "src/none", "src"] {
            assert!(names.push_str(it));
        }
        let mut effects = vec![Effect::Stamp { files: std::sync::Arc::new(names), tag: Tag(1) }];
        effects.push(Effect::Entries { dir: name("src").into(), tag: Tag(2) });
        let mut done = Vec::new();
        sim.submit(&mut effects);
        while sim.in_flight() > 0 {
            sim.poll(&mut done, crate::wait::Deadline::NEVER);
        }
        done.sort_by_key(|it| it.tag);
        let b = stamp(&mut sim, "src/b.rs");
        let src = stamp(&mut sim, "src");
        assert_eq!(done[0].done, Done::Stamped(vec![b, Stamp::GONE, src]));
        let mut listed = crate::ListingBuilder::new(name("src"));
        listed.push(b"a", empty).unwrap();
        listed.push(b"b.rs", b).unwrap();
        assert_eq!(done[1].done, Done::Entries(Box::new(listed.finish().unwrap())));
    }

    #[test]
    fn a_journal_from_a_cursor_names_what_changed_while_down_and_each_loss() {
        for seed in 0..50 {
            for kind in [Journal::FsEvents, Journal::Usn] {
                let mut sim = Sim::new(seed, Faults::default());
                sim.set_journal(kind);
                sim.change(&name("src/kept.rs"), b"k");
                let first = journal(&mut sim, Cursor::None);
                assert_eq!(first.len(), 1);
                let cursor = first[0].cursor;
                assert!(matches!(cursor, Cursor::Sim { .. }) && first[0].caught_up);
                sim.change(&name("src/new/a.rs"), b"a");
                sim.change(&name("other/b.rs"), b"b");
                sim.rename(&name("src/kept.rs"), &name("src/moved.rs"));
                sim.crash();
                let told = journal(&mut sim, cursor);
                assert!(told.iter().rev().skip(1).all(|it| !it.caught_up), "{told:?}");
                let files: Vec<&str> =
                    told.iter().flat_map(|it| &it.files).map(FileName::as_str).collect();
                for want in ["src/new", "src/new/a.rs", "src/kept.rs", "src/moved.rs"] {
                    assert!(files.contains(&want), "{want} in {files:?}");
                }
                assert!(!files.iter().any(|it| it.starts_with("other")), "{files:?}");
                assert!(told.iter().all(|it| it.lost.is_empty() && it.whole.is_none()));
                let last = told.last().unwrap().cursor;
                assert!(journal(&mut sim, last).iter().all(|it| it.files.is_empty()));
                sim.lose_changes(&name("src/new"), Lost::Rescan);
                let rescan = journal(&mut sim, last);
                assert_eq!(rescan.last().unwrap().lost, [(name("src/new"), Lost::Rescan)]);
                sim.truncate_journal();
                assert_eq!(journal(&mut sim, last)[0].whole, Some(Lost::Truncated));
                sim.reset_journal();
                assert_eq!(journal(&mut sim, last)[0].whole, Some(Lost::Reset));
            }
        }
    }

    #[test]
    fn with_no_journal_a_cursor_is_the_whole_directory_lost_and_changes_are_live() {
        let mut sim = Sim::new(3, Faults::default());
        let saved = Cursor::Sim { journal: 0, next: 4 };
        let mut effects = vec![Effect::Journal { dir: DirName::Named(name("src")), since: saved, tag: Tag(5) }];
        let mut done = Vec::new();
        sim.submit(&mut effects);
        sim.poll(&mut done, crate::wait::Deadline::NEVER);
        let Done::Journaled(first) = &done[0].done else { panic!("{done:?}") };
        assert_eq!((first.lost.as_slice(), first.whole), (&[][..], Some(Lost::NoJournal)));
        assert!(first.caught_up && first.cursor == Cursor::None);
        done.clear();
        sim.change(&name("src/x.rs"), b"x");
        sim.poll(&mut done, crate::wait::Deadline::NEVER);
        let Done::Journaled(live) = &done[0].done else { panic!("{done:?}") };
        assert_eq!(live.files, [name("src/x.rs")]);
    }

    /// The files of the root journal's cases: two directly in the root, the rest beneath it.
    /// None is a folder of another, so a write, a removal or a rename of any is always one.
    const ROOT_FILES: [&str; 6] = ["a.toml", "b.rs", "src/c.rs", "src/d/e.rs", "other/f.rs", ".blaze/g"];

    /// One drawn case of a journal of the driver's ROOT (a checkout run from its own folder):
    /// a start state, a place taken, then a drawn history of writes, removals and renames
    /// (into, out of and within the root's own level), and a journal from the place. THE
    /// ORACLE, from the case's own model of the files (not the simulation's): every file whose
    /// bytes or existence differ between the place and now is a name told, or under a subtree
    /// lost, or the root lost whole. More is allowed; a miss is not.
    fn drawn_root_history(tape: &blaze_sim::Tape, plant: RootPlant) -> Result<(), blaze_sim::Broke> {
        let mut sim = Sim::new(tape.below(1 << 16, "the simulation's seed"), Faults::default());
        sim.set_journal(if tape.coin("NTFS's journal, else FSEvents'") { Journal::Usn } else { Journal::FsEvents });
        sim.disk.plant = plant;
        let mut model: std::collections::BTreeMap<&str, u8> = std::collections::BTreeMap::new();
        for file in ROOT_FILES {
            if tape.coin("a file there at the start") {
                sim.change(&name(file), &[0]);
                model.insert(file, 0);
            }
        }
        let place = journal_of(&mut sim, DirName::Root, Cursor::None).last().expect("a first batch").cursor;
        let before = model.clone();
        let pick = |why: &'static str| ROOT_FILES[tape.below(ROOT_FILES.len() as u64, why) as usize];
        for step in 1..=tape.below(9, "the history's length") as u8 {
            let file = pick("the file");
            match tape.below(3, "write, remove or rename") {
                0 => {
                    sim.change(&name(file), &[step]);
                    model.insert(file, step);
                }
                1 if model.contains_key(file) => {
                    sim.delete(&name(file));
                    model.remove(file);
                }
                _ => {
                    let to = pick("the rename's destination");
                    if let Some(bytes) = model.get(file).copied()
                        && to != file
                        && !model.contains_key(to)
                    {
                        sim.rename(&name(file), &name(to));
                        model.remove(file);
                        model.insert(to, bytes);
                    }
                }
            }
        }
        let told = journal_of(&mut sim, DirName::Root, place);
        if told.iter().any(|it| it.whole.is_some()) {
            return Ok(());
        }
        let covered = |file: &FileName| {
            told.iter().any(|it| it.files.contains(file) || it.lost.iter().any(|(under, _)| file.is_under(under)))
        };
        for file in ROOT_FILES {
            if before.get(file) != model.get(file) && !covered(&name(file)) {
                return Err(blaze_sim::Broke::Oracle(format!(
                    "not told: {file} differs since the place ({:?} then, {:?} now), told {told:?}",
                    before.get(file),
                    model.get(file)
                )));
            }
        }
        Ok(())
    }

    /// The invariant: a journal of the root names every change since its place, at the
    /// root's own level as beneath it.
    #[test]
    fn a_journal_of_the_root_names_every_change_since_its_place() {
        blaze_sim::check(blaze_sim::Config::cases(3000), |tape| drawn_root_history(tape, RootPlant::None));
    }

    /// Controls: the oracle catches a root-level name not told, and a rename into the root
    /// told only at its source.
    #[test]
    fn the_oracle_catches_a_root_journal_that_misses_the_roots_own_level() {
        blaze_sim::control(blaze_sim::Config::cases(4000), RootPlant::None, RootPlant::RootNamesDropped, "not told", drawn_root_history);
        blaze_sim::control(blaze_sim::Config::cases(4000), RootPlant::None, RootPlant::RenameIntoRootAtSourceOnly, "not told", drawn_root_history);
    }
}
