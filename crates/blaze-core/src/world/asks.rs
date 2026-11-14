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

use crate::world::disk::{Barrier, Change, Disk, File, Kept, Made, Undo, sectors, starts_with};

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

/// One effect asked of a file and answered while a sync of it is in flight, in the run of
/// [`a_synced_append_survives_a_crash`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Between {
    /// More bytes appended, told done, or told failed after this many of them landed.
    Append { bytes: Vec<u8>, failed_after: Option<usize> },
    /// Another sync of the file, told it covered this many of its changes.
    Sync { covers: usize },
}

/// Whether `after`, a file read after a crash, still begins with `before`.
fn survives(before: &[u8], after: &Option<Vec<u8>>) -> bool {
    match after {
        Some(bytes) => starts_with(bytes, before),
        None => false,
    }
}

/// The run of [`a_synced_append_survives_a_crash`] on `disk` with nothing in flight: `bytes`
/// appended to `file` and told done; a sync of `file` asked; `between` asked and answered one
/// by one; the sync told it covered `covers`, where there is one (`None`: it is still in
/// flight at the crash); then a crash that lands `landings` and keeps `cut`. `None` where the
/// run is not one of the world's (an answer or a crash it does not allow, a disk out of the
/// bounds, a file some change not synced of which is not an append or that a crash may undo);
/// else whether the file then begins with everything it read as when the sync was asked.
fn synced_append(
    disk: Disk,
    file: u64,
    bytes: Vec<u8>,
    between: &[Between],
    covers: Option<usize>,
    landings: &[Landing],
    cut: &[Kept],
) -> Option<bool> {
    if !disk.wf() || !disk.small() {
        return None;
    }
    let mut world = World { disk, asked: Vec::new_in(Global) };
    if !world.ask(0, Ask::Append { file, bytes }) || !world.tell(0, &Told::Done) {
        return None;
    }
    let Some(at) = world.disk.find(file) else { return None };
    if !world.disk.files[at].only_appended() || world.disk.files[at].undo.is_some() {
        return None;
    }
    let before = world.disk.files[at].contents();
    if !world.ask(1, Ask::Sync { file }) {
        return None;
    }
    let mut i = 0;
    while i < between.len() {
        if !world.disk.small() {
            return None;
        }
        match &between[i] {
            Between::Append { bytes, failed_after } => {
                if !world.ask(2, Ask::Append { file, bytes: copy_bytes(bytes) }) {
                    return None;
                }
                let told = match failed_after {
                    Some(landed) => Told::FailedAfter(*landed),
                    None => Told::Done,
                };
                if !world.tell(2, &told) {
                    return None;
                }
            }
            Between::Sync { covers } => {
                if !world.ask(2, Ask::Sync { file }) {
                    return None;
                }
                if !world.tell(2, &Told::Covered(*covers)) {
                    return None;
                }
            }
        }
        i += 1;
    }
    if !world.disk.small() {
        return None;
    }
    if let Some(covers) = covers {
        if !world.tell(1, &Told::Covered(covers)) {
            return None;
        }
    }
    if !world.allows_crash(landings, cut) {
        return None;
    }
    world.crash(landings, cut);
    Some(survives(&before, &world.disk.contents(file)))
}

/// The invariant: what was
/// appended before a sync was asked survives every crash after the sync is told. From every
/// well-formed disk within the bounds with nothing in flight: bytes are appended to a file
/// and told done, a sync of the file is asked, and while it is in flight more is appended to
/// the file (done, or failed part way) and other syncs of it are asked and told, in any
/// order; whatever the kernel says each covered, within what the world allows
/// ([`World::allows`]: a sync covers at least what it owes, and what it owes is lowered only
/// by what another made durable). Once the first sync is told, after every crash the world
/// allows ([`World::allows_crash`]) the file is there and begins with everything it read as
/// when that sync was asked ([`survives`]): the bytes appended before it and all before
/// them. So a core that waits for its sync's answer before it says "written" never says it of
/// bytes a crash can take, whatever else it has in flight to that file. Its control is
/// `the_oracle_catches_an_append_lost_before_its_sync_was_told`.
///
/// Run on drawn disks, effects and cuts today; not yet proved.
// when its proof (crates/blaze-core/lean/BlazeCore/World/Asks.lean) lands: `#[blaze::theorem]`
// here, and "(proved: that file)" in the first line above. Until then it is no theorem of
// `blaze prove`'s, which exits 1 on one that is stated and not proved.
#[blaze::theorem]
pub fn a_synced_append_survives_a_crash(
    disk: Disk,
    file: u64,
    bytes: Vec<u8>,
    between: Vec<Between>,
    covers: usize,
    landings: Vec<Landing>,
    cut: Vec<Kept>,
) -> bool {
    match synced_append(disk, file, bytes, &between, Some(covers), &landings, &cut) {
        Some(survived) => survived,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use blaze_sim::{Broke, Config, Tape, check};

    use super::*;
    use crate::world::disk::{Torn, tests as disks};

    /// What is asked and answered while the sync is in flight, drawn: appends that end done
    /// or part way, and syncs told a count near what the file has not synced.
    fn between(tape: &Tape) -> Vec<Between> {
        let mut between = Vec::new_in(Global);
        for _ in 0..tape.below(5, "effects between") {
            between.push(match tape.coin("another sync") {
                true => Between::Sync { covers: tape.index(6, "the other sync covers") },
                false => {
                    let bytes = tape.bytes(0..=4, "more bytes");
                    let failed_after =
                        tape.option("failed part way", |tape| tape.index(bytes.len() + 2, "bytes landed"));
                    Between::Append { bytes, failed_after }
                }
            });
        }
        between
    }

    /// The theorem's function holds where it is run: on drawn disks, drawn effects between
    /// and drawn cuts, reached with runs the world allows to the end, among them one whose
    /// crash loses an append made after the sync was asked and one where another sync
    /// lowered what the first owed.
    #[test]
    fn a_synced_append_survives_a_crash_where_it_is_run() {
        let config = Config {
            must_reach: &[
                "a run the world allows to its end",
                "a crash that loses a later append",
                "another sync told first",
                "a run the world refuses",
            ],
            ..Config::cases(4000)
        };
        check(config, |tape| {
            let disk = disks::disk(tape);
            let file = tape.below(3, "the theorem's file");
            let bytes = tape.bytes(1..=5, "the append");
            let between = between(tape);
            // What the file has not synced when the first sync is told, to draw an answer
            // near it: the same run with the sync left in flight says nothing of that, so
            // the count is drawn wide and the world refuses what it does not allow.
            let covers = tape.index(12, "the sync covers");
            // The cut is drawn for the disk as it stands at the crash: run to there first.
            let mut world = World { disk: copy_disk(&disk), asked: Vec::new_in(Global) };
            let reached = world.ask(0, Ask::Append { file, bytes: bytes.clone() })
                && world.tell(0, &Told::Done)
                && world.ask(1, Ask::Sync { file })
                && between.iter().all(|it| match it {
                    Between::Append { bytes, failed_after } => {
                        world.ask(2, Ask::Append { file, bytes: bytes.clone() })
                            && world.tell(
                                2,
                                &failed_after.map_or(Told::Done, Told::FailedAfter),
                            )
                    }
                    Between::Sync { covers } => {
                        world.ask(2, Ask::Sync { file }) && world.tell(2, &Told::Covered(*covers))
                    }
                })
                && world.tell(1, &Told::Covered(covers));
            let cut = disks::cut(tape, &world.disk);
            let landings: Vec<Landing> = Vec::new_in(Global);
            let ran = synced_append(copy_disk(&disk), file, bytes.clone(), &between, Some(covers), &landings, &cut);
            match ran {
                None => tape.reached("a run the world refuses"),
                Some(_) => {
                    tape.reached("a run the world allows to its end");
                    if between.iter().any(|it| matches!(it, Between::Sync { covers } if *covers > 0)) {
                        tape.reached("another sync told first");
                    }
                    let lost = world.disk.find(file).is_some_and(|at| {
                        let file = &world.disk.files[at];
                        !file.changes.is_empty() && cut[at].whole < file.changes.len() && cut[at].torn == Torn::Nothing
                    });
                    if reached && lost {
                        tape.reached("a crash that loses a later append");
                    }
                }
            }
            match a_synced_append_survives_a_crash(disk.clone(), file, bytes, between.clone(), covers, landings, cut.clone()) {
                true => Ok(()),
                false => Err(Broke::Oracle(format!(
                    "an append told done before a sync that was told did not survive: {disk:?} file {file} between {between:?} covers {covers} cut {cut:?}"
                ))),
            }
        });
    }

    /// The cause removed (test-the-cause): the same run with the sync still in flight at the
    /// crash. The world then allows a cut that keeps none of the file's changes, the append
    /// is gone, and [`survives`] says so; told, the sync covers the append and the same cut
    /// is one the world refuses.
    #[test]
    fn the_oracle_catches_an_append_lost_before_its_sync_was_told() {
        let run = |covers| {
            let mut bytes = Vec::new_in(Global);
            bytes.extend_from_slice(b"abc");
            let mut cut = Vec::new_in(Global);
            cut.push(Kept { whole: 0, torn: Torn::Nothing, undone: false });
            let landings: Vec<Landing> = Vec::new_in(Global);
            synced_append(Disk::new(), 7, bytes, &[], covers, &landings, &cut)
        };
        // Not told: the crash keeps nothing of the append, and the file is empty.
        assert_eq!(run(None), Some(false));
        // Told it covered the one change: nothing is left for a cut to drop, so the cut
        // that keeps 0 of 0 changes is allowed and the file reads "abc".
        assert_eq!(run(Some(1)), Some(true));
        // Told it covered nothing is not an answer the world allows a sync that owes one.
        assert_eq!(run(Some(0)), None);
    }
}
