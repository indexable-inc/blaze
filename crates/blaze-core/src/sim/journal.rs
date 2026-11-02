//! The simulation's disk as a journal of changes sees it (docs/v2/plan/lanes/frontend/journal.md):
//! each file and directory has an inode and change times that move as a real file system's
//! do, and a journal of every change survives [`super::Sim::crash`], as FSEvents' store and
//! NTFS's USN journal survive a reboot. [`Journal`] says which system it plays; the test
//! resets, truncates or loses parts of it to check that a watcher names each loss and looks
//! again only where it was.

use blaze_hash::HashMap;

use super::SimFile;
use crate::{Cursor, FileName, Journaled, Lost, S_IFDIR, S_IFLNK, S_IFREG, Stamp};

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
    Lost(FileName, Lost),
}

#[derive(Clone, Copy)]
struct Inode {
    ino: u64,
    mtime: u64,
    ctime: u64,
}

#[derive(Default)]
pub(super) struct Disk {
    pub(super) kind: Journal,
    /// Moves once per change: the simulation's mtimes and ctimes.
    clock: u64,
    next_ino: u64,
    /// Every file and directory there, by name.
    inodes: HashMap<FileName, Inode>,
    /// The journal's ID, the sequence number of its oldest entry kept, and its entries.
    id: u64,
    first: u64,
    log: Vec<Entry>,
    /// The name a rename just made: its touch moves its change time only, as a rename does.
    renamed_to: Option<FileName>,
}

fn parent(name: &FileName) -> Option<FileName> {
    name.as_str().rfind('/').and_then(|at| FileName::new(&name.as_str()[..at]))
}

impl Disk {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
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

    /// `file` changed, and is in `files` now or not: its inode and the directories above it
    /// move as on a real disk (a directory's mtime moves when a name in it is made or goes; a
    /// directory is there while a file is under it), and the journal records it. Returns the
    /// cursor after the change.
    pub(super) fn touched(&mut self, file: &FileName, files: &HashMap<FileName, SimFile>) -> Cursor {
        let there = files.contains_key(file);
        let now = self.tick();
        let renamed = self.renamed_to.as_ref() == Some(file);
        if renamed {
            self.renamed_to = None;
        }
        match (there, self.inodes.get_mut(file)) {
            (true, Some(inode)) if renamed => inode.ctime = now,
            (true, Some(inode)) => (inode.mtime, inode.ctime) = (now, now),
            (true, None) => {
                let inode = self.new_inode();
                // clone: the simulated disk's name of a new file.
                self.inodes.insert(file.clone(), inode);
                self.entered(file);
            }
            (false, Some(_)) => {
                self.inodes.remove(file);
                self.left(file, files);
            }
            (false, None) => self.left(file, files),
        }
        // clone: the simulated journal's entry, kept across crashes.
        self.log.push(Entry::Changed(file.clone()));
        self.cursor()
    }

    /// A name was made under the directories above `name`: the first that was there already
    /// has a name more; those that were not are made.
    fn entered(&mut self, name: &FileName) {
        // clone: a name of the simulated disk's, walked up.
        let mut at = name.clone();
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
                    self.inodes.insert(up.clone(), inode);
                    // clone: the simulated journal's entry.
                    self.log.push(Entry::Changed(up.clone()));
                }
            }
            at = up;
        }
    }

    /// A name under the directories above `name` went: those left with nothing go too, and
    /// the first left with something has a name fewer.
    fn left(&mut self, name: &FileName, files: &HashMap<FileName, SimFile>) {
        // clone: a name of the simulated disk's, walked up.
        let mut at = name.clone();
        while let Some(up) = parent(&at) {
            if !self.inodes.contains_key(&up) {
                return;
            }
            if files.keys().any(|it| it.is_under(&up)) {
                let now = self.tick();
                let inode = self.inodes.get_mut(&up).expect("looked up");
                (inode.mtime, inode.ctime) = (now, now);
                return;
            }
            self.inodes.remove(&up);
            // clone: the simulated journal's entry.
            self.log.push(Entry::Changed(up.clone()));
            at = up;
        }
    }

    /// `from` is renamed `to`, keeping its inode and mtime; called before the two are told as
    /// changed, `from` first.
    pub(super) fn renamed(&mut self, from: &FileName, to: &FileName) {
        if let Some(inode) = self.inodes.remove(from) {
            // clone: the simulated disk's new name of the file.
            self.inodes.insert(to.clone(), inode);
            self.entered(to);
            // clone: as above, until its touch.
            self.renamed_to = Some(to.clone());
        }
    }

    /// A chmod: the inode's change time moves, its mtime does not.
    pub(super) fn attributes(&mut self, file: &FileName) -> Cursor {
        let now = self.tick();
        if let Some(inode) = self.inodes.get_mut(file) {
            inode.ctime = now;
        }
        // clone: the simulated journal's entry.
        self.log.push(Entry::Changed(file.clone()));
        self.cursor()
    }

    /// A `utimensat` of `name`: its inode's mtime is `modified_ns` and its change time moves, as
    /// on a real disk. `false`, and nothing done, where `name` is neither a file nor a directory.
    pub(super) fn times(
        &mut self,
        name: &FileName,
        modified_ns: u64,
        files: &HashMap<FileName, SimFile>,
    ) -> bool {
        if self.stamp(name, files) == Stamp::GONE {
            return false;
        }
        let now = self.tick();
        let inode = self.inodes.get_mut(name).expect("stamped, so it has an inode");
        (inode.mtime, inode.ctime) = (modified_ns, now);
        // clone: the simulated journal's entry.
        self.log.push(Entry::Changed(name.clone()));
        true
    }

    /// The mtime of `name`'s inode, where it has one, without making one.
    pub(super) fn mtime(&self, name: &FileName) -> Option<u64> {
        self.inodes.get(name).map(|it| it.mtime)
    }

    /// What `name` is now.
    pub(super) fn stamp(&mut self, name: &FileName, files: &HashMap<FileName, SimFile>) -> Stamp {
        if let Some(file) = files.get(name) {
            if !self.inodes.contains_key(name) {
                let inode = self.new_inode();
                // clone: the simulated disk's name of a file first stamped.
                self.inodes.insert(name.clone(), inode);
            }
            let inode = self.inodes[name];
            return Stamp {
                ino: inode.ino,
                size: file.len(),
                mtime_ns: inode.mtime,
                ctime_ns: inode.ctime,
                mode: if file.link { S_IFLNK | 0o777 } else { S_IFREG | (file.mode & 0o7777) },
            };
        }
        if files.keys().any(|it| it.is_under(name)) {
            if !self.inodes.contains_key(name) {
                let inode = self.new_inode();
                // clone: the simulated disk's name of a directory first stamped.
                self.inodes.insert(name.clone(), inode);
            }
            let inode = self.inodes[name];
            return Stamp {
                ino: inode.ino,
                size: 0,
                mtime_ns: inode.mtime,
                ctime_ns: inode.ctime,
                mode: S_IFDIR | 0o755,
            };
        }
        Stamp::GONE
    }

    /// What is directly in `dir`, sorted; `None` when it is no directory.
    pub(super) fn entries(
        &mut self,
        dir: &FileName,
        files: &HashMap<FileName, SimFile>,
    ) -> Option<Vec<(FileName, Stamp)>> {
        if files.contains_key(dir) {
            return None;
        }
        let mut names: Vec<FileName> = files
            .keys()
            .filter(|it| *it != dir && it.is_under(dir))
            .filter_map(|it| {
                let rest = &it.as_str()[dir.as_str().len() + 1..];
                let first = rest.split('/').next().expect("a part");
                // alloc: the listing's payload, a name of it.
                FileName::new(&format!("{dir}/{first}"))
            })
            // alloc: the listing's payload.
            .collect();
        if names.is_empty() {
            return None;
        }
        names.sort_unstable();
        names.dedup();
        let stamped = names.into_iter().map(|name| {
            let stamp = self.stamp(&name, files);
            (name, stamp)
        });
        // alloc: the listing's payload.
        Some(stamped.collect())
    }

    /// The batches an [`crate::Effect::Journal`] of `dir` from `since` is told first: the
    /// history since then, split in `parts` (FSEvents') or one, the last caught up.
    pub(super) fn history(&self, dir: &FileName, since: &Cursor, parts: u64) -> Vec<Journaled> {
        let whole = |lost: Option<Lost>| Journaled {
            // alloc: none, an empty vector.
            files: Vec::new(),
            // alloc: the batch's payload. clone: its name of the directory.
            lost: lost.map(|why| vec![(dir.clone(), why)]).unwrap_or_default(),
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
            Cursor::Sim { journal, .. } if journal == self.id => return vec![whole(Some(Lost::Reset))],
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

    fn add(&self, dir: &FileName, entry: &Entry, batch: &mut Journaled) {
        match entry {
            // clone: the batch's payload, a name of it.
            Entry::Changed(file) if file.is_under(dir) => batch.files.push(file.clone()),
            // clone: as above.
            Entry::Lost(under, why) if under.is_under(dir) => batch.lost.push((under.clone(), *why)),
            // clone: as above.
            Entry::Lost(under, why) if dir.is_under(under) => batch.lost.push((dir.clone(), *why)),
            Entry::Changed(_) | Entry::Lost(..) => {}
        }
    }

    /// The live batch of the change just recorded, for a journal of `dir`; `None` when the
    /// change is not under it.
    pub(super) fn live(&self, dir: &FileName, dropped: bool) -> Option<Journaled> {
        let entry = self.log.last()?;
        let (cursor, caught_up) = (self.cursor(), true);
        // alloc: none, empty vectors, the batch's payload once pushed to.
        let mut batch = Journaled { files: Vec::new(), lost: Vec::new(), cursor, caught_up };
        self.add(dir, entry, &mut batch);
        if dropped {
            batch.files.clear();
            // alloc: the batch's payload. clone: its name of the directory.
            batch.lost = vec![(dir.clone(), Lost::Dropped)];
        }
        (!batch.files.is_empty() || !batch.lost.is_empty()).then_some(batch)
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
        self.log.push(Entry::Lost(dir.clone(), why));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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
        let mut effects = vec![Effect::Journal { dir: name("src"), since, tag: Tag(5) }];
        let (mut out, mut done) = (Vec::new(), Vec::new());
        loop {
            sim.submit(&mut effects);
            sim.poll(&mut done, Duration::ZERO);
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
        let (file, dir, top) = (stamp(&mut sim, "src/a/one.rs"), stamp(&mut sim, "src/a"), stamp(&mut sim, "src"));
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
        // A directory left empty goes, and its parent moves.
        sim.delete(&name("src/a/two.rs"));
        assert!(stamp(&mut sim, "src/a").is_gone());
        assert!(stamp(&mut sim, "src").mtime_ns > top.mtime_ns);
        let mut names = Names::default();
        for it in ["src/b.rs", "src/a", "src"] {
            assert!(names.push_str(it));
        }
        let mut effects = vec![Effect::Stamp { files: std::sync::Arc::new(names), tag: Tag(1) }];
        effects.push(Effect::Entries { dir: name("src"), tag: Tag(2) });
        let mut done = Vec::new();
        sim.submit(&mut effects);
        while sim.in_flight() > 0 {
            sim.poll(&mut done, Duration::ZERO);
        }
        done.sort_by_key(|it| it.tag);
        let b = stamp(&mut sim, "src/b.rs");
        let src = stamp(&mut sim, "src");
        assert_eq!(done[0].done, Done::Stamped(vec![b, Stamp::GONE, src]));
        assert_eq!(done[1].done, Done::Entries(vec![(name("src/b.rs"), b)]));
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
                let files: Vec<&str> = told.iter().flat_map(|it| &it.files).map(FileName::as_str).collect();
                for want in ["src/new", "src/new/a.rs", "src/kept.rs", "src/moved.rs"] {
                    assert!(files.contains(&want), "{want} in {files:?}");
                }
                assert!(!files.iter().any(|it| it.starts_with("other")), "{files:?}");
                assert!(told.iter().all(|it| it.lost.is_empty()));
                let last = told.last().unwrap().cursor;
                assert!(journal(&mut sim, last).iter().all(|it| it.files.is_empty()));
                sim.lose_changes(&name("src/new"), Lost::Rescan);
                let rescan = journal(&mut sim, last);
                assert_eq!(rescan.last().unwrap().lost, [(name("src/new"), Lost::Rescan)]);
                sim.truncate_journal();
                assert_eq!(journal(&mut sim, last)[0].lost, [(name("src"), Lost::Truncated)]);
                sim.reset_journal();
                assert_eq!(journal(&mut sim, last)[0].lost, [(name("src"), Lost::Reset)]);
            }
        }
    }

    #[test]
    fn with_no_journal_a_cursor_is_the_whole_directory_lost_and_changes_are_live() {
        let mut sim = Sim::new(3, Faults::default());
        let saved = Cursor::Sim { journal: 0, next: 4 };
        let mut effects = vec![Effect::Journal { dir: name("src"), since: saved, tag: Tag(5) }];
        let mut done = Vec::new();
        sim.submit(&mut effects);
        sim.poll(&mut done, Duration::ZERO);
        let Done::Journaled(first) = &done[0].done else { panic!("{done:?}") };
        assert_eq!(first.lost, [(name("src"), Lost::NoJournal)]);
        assert!(first.caught_up && first.cursor == Cursor::None);
        done.clear();
        sim.change(&name("src/x.rs"), b"x");
        sim.poll(&mut done, Duration::ZERO);
        let Done::Journaled(live) = &done[0].done else { panic!("{done:?}") };
        assert_eq!(live.files, [name("src/x.rs")]);
    }
}
