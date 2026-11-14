//! What a sans-IO core has asked of the disk and not been answered yet, and which answers the
//! kernel may give to each: the layer over [`Disk`] that says what a completion may be, where
//! the disk says only what one does.
//!
//! What it says:
//!
//! - An effect is in flight from when it is asked ([`World::ask`]) until it is told
//!   ([`World::tell`]) or a crash ends it ([`World::crash`]). Each is known by its caller's tag.
//! - A sync or a barrier owes what was not synced of its file when it was asked and is still
//!   not durable ([`Asked::owed`]): a sync that makes changes durable before it is told lowers
//!   what the others of that name owe, and a name whose file was put there or taken away while
//!   the ask was in flight is owed nothing.
//! - Which answers there are ([`World::allows`]): a sync or a barrier covers at least what it
//!   owes and at most what is not synced now; a write, a rename and a removal end done, failed
//!   with nothing changed, or for want of the file; a failed append lands a prefix of its
//!   bytes; a failed write from memory lands any of its sectors.
//! - A crash ends everything in flight with no answer, after any sectors of the writes from
//!   memory in flight land, each at most once, in any order and interleaving ([`Landing`],
//!   [`World::allows_crash`]): two writes in flight to one place may land either way round.
//!
//! Written in the subset the MIR-to-Lean translator takes, as [`crate::world::disk`] is.

#![blaze::sans_io]

use std::alloc::Global;

use crate::world::disk::{Barrier, Change, Disk, File, Kept, Made, Undo, sectors};

/// An effect on the disk, as it was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// Bytes added at the end of a file, made where there is none.
    Append { file: u64, bytes: Vec<u8> },
    /// Bytes written at an offset of a file that is there.
    WriteAt { file: u64, at: usize, bytes: Vec<u8> },
    /// Bytes written from memory at an offset of a file that is there, a sector at a time.
    WriteFrom { file: u64, at: usize, bytes: Vec<u8> },
    /// A file cut or grown to a length, made where there is none.
    Truncate { file: u64, len: usize },
    /// A file's changes made durable, as many as it has now, and the drive flushed.
    Sync { file: u64 },
    /// A file's changes ordered before every later change, as many as it has now.
    Barrier { file: u64 },
    /// The directory synced: the drive flushed.
    SyncDir,
    /// A whole-file write that waits for the drive.
    Write { file: u64, bytes: Vec<u8> },
    /// A whole-file write that waits for no sync.
    WriteUnsynced { file: u64, bytes: Vec<u8> },
    /// A rename that waits for the drive.
    Rename { from: u64, to: u64 },
    /// A rename that waits for no sync.
    RenameUnsynced { from: u64, to: u64 },
    /// A file's name taken away.
    Remove { file: u64 },
}

/// An effect in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    /// The caller's name for it, one per effect in flight.
    pub tag: u64,
    pub ask: Ask,
    /// For a sync and a barrier, what the kernel must cover at least: how many of its file's
    /// changes were not synced when it was asked and are still not durable. 0 for any other.
    pub owed: usize,
}

/// How an effect ended, as the kernel told it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Told {
    /// It did what was asked.
    Done,
    /// A sync or a barrier did what was asked, covering this many of its file's changes.
    Covered(usize),
    /// A write or a rename found no file under the name: nothing changed.
    Missing,
    /// It failed, and nothing changed.
    Failed,
    /// An append failed after the first this many of its bytes landed.
    FailedAfter(usize),
    /// A write from memory failed after each of its sectors this says landed.
    FailedLanding(Vec<bool>),
}

/// One sector of a write from memory in flight that a crash landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landing {
    /// Where the write is in [`World::asked`].
    pub ask: usize,
    /// Which of its [`SECTOR`](crate::world::disk::SECTOR)s, from the first.
    pub sector: usize,
}

/// The disk and what has been asked of it and not answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct World {
    pub disk: Disk,
    /// The effects in flight, in the order they were asked.
    pub asked: Vec<Asked>,
}

/// How many changes not synced the file named `name` has: 0 when there is none.
pub fn unsynced(disk: &Disk, name: u64) -> usize {
    match disk.find(name) {
        Some(at) => disk.files[at].changes.len(),
        None => 0,
    }
}

/// Whether a file is named `name`.
fn there(disk: &Disk, name: u64) -> bool {
    disk.find(name).is_some()
}

/// The name a sync or a barrier is of; none for any other ask.
fn synced_name(ask: &Ask) -> Option<u64> {
    match ask {
        Ask::Sync { file } | Ask::Barrier { file } => Some(*file),
        Ask::Append { .. }
        | Ask::WriteAt { .. }
        | Ask::WriteFrom { .. }
        | Ask::Truncate { .. }
        | Ask::SyncDir
        | Ask::Write { .. }
        | Ask::WriteUnsynced { .. }
        | Ask::Rename { .. }
        | Ask::RenameUnsynced { .. }
        | Ask::Remove { .. } => None,
    }
}

/// Whether `ask`, done, puts another file under `name` or takes its file away: a whole-file
/// write or a removal of it, a rename from it or to it.
fn renames(ask: &Ask, name: u64) -> bool {
    match ask {
        Ask::Write { file, .. } | Ask::WriteUnsynced { file, .. } | Ask::Remove { file } => {
            *file == name
        }
        Ask::Rename { from, to } | Ask::RenameUnsynced { from, to } => {
            *from == name || *to == name
        }
        Ask::Append { .. }
        | Ask::WriteAt { .. }
        | Ask::WriteFrom { .. }
        | Ask::Truncate { .. }
        | Ask::Sync { .. }
        | Ask::Barrier { .. }
        | Ask::SyncDir => false,
    }
}

/// How many sectors `landed` says landed.
fn count(landed: &[bool]) -> usize {
    let mut landings = 0;
    let mut i = 0;
    while i < landed.len() {
        if landed[i] {
            landings += 1;
        }
        i += 1;
    }
    landings
}

/// How many changes `ask`, told `told`, added to the file named `name`, which `had` says was
/// there before: an append, a write and a cut add one where they change it, a failed append
/// one (the prefix that landed, empty or not), a failed write from memory one per sector that
/// landed.
fn added(ask: &Ask, told: &Told, name: u64, had: bool) -> usize {
    match ask {
        Ask::Append { file, .. } | Ask::Truncate { file, .. } => match told {
            Told::Done | Told::FailedAfter(_) => {
                if *file == name {
                    1
                } else {
                    0
                }
            }
            Told::Covered(_) | Told::Missing | Told::Failed | Told::FailedLanding(_) => 0,
        },
        Ask::WriteAt { file, .. } | Ask::WriteFrom { file, .. } => match told {
            Told::Done => {
                if *file == name && had {
                    1
                } else {
                    0
                }
            }
            Told::FailedLanding(landed) => {
                if *file == name && had {
                    count(landed)
                } else {
                    0
                }
            }
            Told::Covered(_) | Told::Missing | Told::Failed | Told::FailedAfter(_) => 0,
        },
        Ask::Sync { .. }
        | Ask::Barrier { .. }
        | Ask::SyncDir
        | Ask::Write { .. }
        | Ask::WriteUnsynced { .. }
        | Ask::Rename { .. }
        | Ask::RenameUnsynced { .. }
        | Ask::Remove { .. } => 0,
    }
}

/// Whether `told` is `Done`.
fn done(told: &Told) -> bool {
    match told {
        Told::Done => true,
        Told::Covered(_)
        | Told::Missing
        | Told::Failed
        | Told::FailedAfter(_)
        | Told::FailedLanding(_) => false,
    }
}

/// Whether `told` is `Done` or `Failed`.
fn done_or_failed(told: &Told) -> bool {
    match told {
        Told::Done | Told::Failed => true,
        Told::Covered(_) | Told::Missing | Told::FailedAfter(_) | Told::FailedLanding(_) => false,
    }
}

/// The first `count` of `bytes`: all of them when it has fewer.
fn prefix(bytes: &[u8], count: usize) -> Vec<u8> {
    let mut out = Vec::new_in(Global);
    let mut i = 0;
    while i < count && i < bytes.len() {
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// A copy of `bytes`.
fn copy_bytes(bytes: &[u8]) -> Vec<u8> {
    prefix(bytes, bytes.len())
}

/// A copy of `change`.
fn copy_change(change: &Change) -> Change {
    match change {
        Change::Append(more) => Change::Append(copy_bytes(more)),
        Change::At(at, more) => Change::At(*at, copy_bytes(more)),
        Change::Sectors(at, more) => Change::Sectors(*at, copy_bytes(more)),
        Change::Cut(len) => Change::Cut(*len),
    }
}

/// A copy of `file`.
fn copy_file(file: &File) -> File {
    let mut changes = Vec::new_in(Global);
    let mut i = 0;
    while i < file.changes.len() {
        let made = &file.changes[i];
        changes.push(Made { change: copy_change(&made.change), epoch: made.epoch });
        i += 1;
    }
    let mut barriers = Vec::new_in(Global);
    i = 0;
    while i < file.barriers.len() {
        barriers.push(Barrier { covers: file.barriers[i].covers, epoch: file.barriers[i].epoch });
        i += 1;
    }
    let undo = match &file.undo {
        Some(undo) => {
            let was = match &undo.was {
                Some(bytes) => Some(copy_bytes(bytes)),
                None => None,
            };
            Some(Undo { was, from: undo.from })
        }
        None => None,
    };
    File { name: file.name, durable: copy_bytes(&file.durable), changes, barriers, undo }
}

/// A copy of `disk`.
fn copy_disk(disk: &Disk) -> Disk {
    let mut files = Vec::new_in(Global);
    let mut i = 0;
    while i < disk.files.len() {
        files.push(copy_file(&disk.files[i]));
        i += 1;
    }
    Disk { files, epoch: disk.epoch }
}

impl World {
    /// A world with no file and nothing asked.
    pub fn new() -> World {
        World { disk: Disk::new(), asked: Vec::new_in(Global) }
    }

    /// Where the effect `tag` is in [`World::asked`]; none when it is not in flight.
    pub fn find(&self, tag: u64) -> Option<usize> {
        let mut i = 0;
        while i < self.asked.len() {
            if self.asked[i].tag == tag {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// `ask` is in flight under `tag`. A sync or a barrier owes every change not synced of
    /// the file under its name now. `false`, and nothing changed, where `tag` is in flight
    /// already.
    pub fn ask(&mut self, tag: u64, ask: Ask) -> bool {
        if self.find(tag).is_some() {
            return false;
        }
        let owed = match synced_name(&ask) {
            Some(name) => unsynced(&self.disk, name),
            None => 0,
        };
        self.asked.push(Asked { tag, ask, owed });
        true
    }

    /// Whether `tag` is in flight and `told` is an answer the kernel may give it now.
    pub fn allows(&self, tag: u64, told: &Told) -> bool {
        let Some(at) = self.find(tag) else { return false };
        let asked = &self.asked[at];
        match &asked.ask {
            Ask::Append { bytes, .. } => match told {
                Told::Done => true,
                Told::FailedAfter(landed) => *landed <= bytes.len(),
                Told::Covered(_) | Told::Missing | Told::Failed | Told::FailedLanding(_) => false,
            },
            Ask::WriteAt { file, .. } => match told {
                Told::Done => there(&self.disk, *file),
                Told::Missing => !there(&self.disk, *file),
                Told::Failed => true,
                Told::Covered(_) | Told::FailedAfter(_) | Told::FailedLanding(_) => false,
            },
            Ask::WriteFrom { file, bytes, .. } => match told {
                Told::Done => there(&self.disk, *file),
                Told::Missing => !there(&self.disk, *file),
                Told::Failed => true,
                Told::FailedLanding(landed) => {
                    there(&self.disk, *file) && landed.len() == sectors(bytes.len())
                }
                Told::Covered(_) | Told::FailedAfter(_) => false,
            },
            Ask::Truncate { .. } | Ask::Write { .. } | Ask::WriteUnsynced { .. } | Ask::SyncDir => {
                done_or_failed(told)
            }
            Ask::Sync { file } | Ask::Barrier { file } => match told {
                Told::Covered(covers) => {
                    asked.owed <= *covers && *covers <= unsynced(&self.disk, *file)
                }
                Told::Failed => true,
                Told::Done | Told::Missing | Told::FailedAfter(_) | Told::FailedLanding(_) => {
                    false
                }
            },
            Ask::Rename { from, .. } | Ask::RenameUnsynced { from, .. } => match told {
                Told::Done => there(&self.disk, *from),
                Told::Missing => !there(&self.disk, *from),
                Told::Failed => true,
                Told::Covered(_) | Told::FailedAfter(_) | Told::FailedLanding(_) => false,
            },
            Ask::Remove { .. } => done_or_failed(told),
        }
    }

    /// The effect `tag` told `told`: it leaves [`World::asked`], the disk is as that
    /// completion leaves it, and what each sync and barrier still in flight owes is lowered
    /// by what was made durable of its name, or is nothing where its name's file changed.
    /// `false`, and nothing changed, where the world does not allow the answer.
    pub fn tell(&mut self, tag: u64, told: &Told) -> bool {
        if !self.allows(tag, told) {
            return false;
        }
        let Some(at) = self.find(tag) else { return false };
        let asked = self.asked.remove(at);
        // For each still in flight, what its name would have not synced after this answer
        // were nothing made durable: what it had, and what the answer added to it. A name
        // whose file the answer put there or took away is owed nothing from now on.
        let mut kept: Vec<usize> = Vec::new_in(Global);
        let mut i = 0;
        while i < self.asked.len() {
            match synced_name(&self.asked[i].ask) {
                Some(name) => {
                    if done(told) && renames(&asked.ask, name) {
                        self.asked[i].owed = 0;
                    }
                    let had = there(&self.disk, name);
                    kept.push(unsynced(&self.disk, name) + added(&asked.ask, told, name, had));
                }
                None => kept.push(0),
            }
            i += 1;
        }
        self.complete(asked.ask, told);
        // What was made durable of each name, its first changes, is no longer owed.
        i = 0;
        while i < self.asked.len() {
            if let Some(name) = synced_name(&self.asked[i].ask) {
                let after = unsynced(&self.disk, name);
                let durable = if kept[i] > after { kept[i] - after } else { 0 };
                let owed = self.asked[i].owed;
                self.asked[i].owed = if owed > durable { owed - durable } else { 0 };
            }
            i += 1;
        }
        true
    }

    /// The disk as `ask`, told `told`, leaves it; the world allows the answer.
    fn complete(&mut self, ask: Ask, told: &Told) {
        match ask {
            Ask::Append { file, bytes } => match told {
                Told::Done => self.disk.append(file, bytes),
                Told::FailedAfter(landed) => self.disk.append(file, prefix(&bytes, *landed)),
                Told::Covered(_) | Told::Missing | Told::Failed | Told::FailedLanding(_) => {}
            },
            Ask::WriteAt { file, at, bytes } => {
                if done(told) {
                    self.disk.write_at(file, at, bytes);
                }
            }
            Ask::WriteFrom { file, at, bytes } => match told {
                Told::Done => {
                    self.disk.write_from(file, at, bytes);
                }
                Told::FailedLanding(landed) => {
                    self.disk.land(file, at, &bytes, landed);
                }
                Told::Covered(_) | Told::Missing | Told::Failed | Told::FailedAfter(_) => {}
            },
            Ask::Truncate { file, len } => {
                if done(told) {
                    self.disk.truncate(file, len);
                }
            }
            Ask::Sync { file } => {
                if let Told::Covered(covers) = told {
                    self.disk.sync(file, *covers);
                }
            }
            Ask::Barrier { file } => {
                if let Told::Covered(covers) = told {
                    self.disk.barrier(file, *covers);
                }
            }
            Ask::SyncDir => {
                if done(told) {
                    self.disk.flush();
                }
            }
            Ask::Write { file, bytes } => {
                if done(told) {
                    self.disk.write(file, bytes);
                }
            }
            Ask::WriteUnsynced { file, bytes } => {
                if done(told) {
                    self.disk.write_unsynced(file, bytes);
                }
            }
            Ask::Rename { from, to } => {
                if done(told) {
                    self.disk.rename(from, to);
                }
            }
            Ask::RenameUnsynced { from, to } => {
                if done(told) {
                    self.disk.rename_unsynced(from, to);
                }
            }
            Ask::Remove { file } => {
                if done(told) {
                    self.disk.remove(file);
                }
            }
        }
    }

    /// Whether `landings` is a crash's landing of sectors of writes from memory in flight:
    /// each names a write from memory in [`World::asked`] and a sector of its bytes, and no
    /// sector lands twice.
    fn lands(&self, landings: &[Landing]) -> bool {
        let mut i = 0;
        while i < landings.len() {
            let landing = &landings[i];
            if landing.ask >= self.asked.len() {
                return false;
            }
            match &self.asked[landing.ask].ask {
                Ask::WriteFrom { bytes, .. } => {
                    if landing.sector >= sectors(bytes.len()) {
                        return false;
                    }
                }
                Ask::Append { .. }
                | Ask::WriteAt { .. }
                | Ask::Truncate { .. }
                | Ask::Sync { .. }
                | Ask::Barrier { .. }
                | Ask::SyncDir
                | Ask::Write { .. }
                | Ask::WriteUnsynced { .. }
                | Ask::Rename { .. }
                | Ask::RenameUnsynced { .. }
                | Ask::Remove { .. } => return false,
            }
            let mut j = 0;
            while j < i {
                if landings[j].ask == landing.ask && landings[j].sector == landing.sector {
                    return false;
                }
                j += 1;
            }
            i += 1;
        }
        true
    }

    /// The disk after a crash landed `landings`, in their order: each sector written as a
    /// change of its own where a file is under its write's name, nothing where none is. None
    /// where `landings` is not a landing of sectors of writes in flight ([`World::lands`]).
    pub fn landed(&self, landings: &[Landing]) -> Option<Disk> {
        if !self.lands(landings) {
            return None;
        }
        let mut disk = copy_disk(&self.disk);
        let mut i = 0;
        while i < landings.len() {
            if let Ask::WriteFrom { file, at, bytes } = &self.asked[landings[i].ask].ask {
                // `Disk::land` with only this sector landed: the one change it writes is the
                // sector's, at its offset, the last one shorter where the bytes end in it. A
                // file not there lands nothing.
                let mut mask: Vec<bool> = Vec::new_in(Global);
                let mut sector = 0;
                while sector < sectors(bytes.len()) {
                    mask.push(sector == landings[i].sector);
                    sector += 1;
                }
                disk.land(*file, *at, bytes, &mask);
            }
            i += 1;
        }
        Some(disk)
    }

    /// Whether a crash may land `landings`, sectors of the writes from memory in flight in
    /// the order they land, and then keep `cut` of the disk they leave ([`Disk::allows`]).
    pub fn allows_crash(&self, landings: &[Landing], cut: &[Kept]) -> bool {
        match self.landed(landings) {
            Some(disk) => disk.allows(cut),
            None => false,
        }
    }

    /// A crash the world allows ([`World::allows_crash`]): the sectors `landings` names land
    /// in its order, the disk keeps `cut` ([`Disk::crash`]), and nothing is in flight, none
    /// of it answered. Where the world does not allow it, nothing changes.
    pub fn crash(&mut self, landings: &[Landing], cut: &[Kept]) {
        let Some(mut disk) = self.landed(landings) else { return };
        if !disk.allows(cut) {
            return;
        }
        disk.crash(cut);
        self.disk = disk;
        self.asked.clear();
    }

    /// Whether the world is well formed, which every function here keeps: the disk is
    /// ([`Disk::wf`]), no two effects in flight have one tag, and no sync or barrier owes
    /// more than its file under its name has not synced.
    pub fn wf(&self) -> bool {
        if !self.disk.wf() {
            return false;
        }
        let mut i = 0;
        while i < self.asked.len() {
            let mut j = i + 1;
            while j < self.asked.len() {
                if self.asked[i].tag == self.asked[j].tag {
                    return false;
                }
                j += 1;
            }
            let owes = match synced_name(&self.asked[i].ask) {
                Some(name) => unsynced(&self.disk, name),
                None => 0,
            };
            if self.asked[i].owed > owes {
                return false;
            }
            i += 1;
        }
        true
    }
}

impl Default for World {
    fn default() -> World {
        World::new()
    }
}
