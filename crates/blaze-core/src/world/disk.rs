//! The file system a sans-IO core's effects go to, as a reference model: the bytes of each
//! file, which of them a power cut keeps for certain, and exactly which disks one may leave.
//!
//! What it says:
//!
//! - A file is its durable bytes and the changes made to it since the last sync that covered
//!   them, oldest first ([`File`]). A read sees every change ([`File::contents`]); a crash
//!   does not.
//! - A sync makes a file's changes durable, as many as had been made when it was asked
//!   ([`Disk::sync`]), and flushes the drive, so what every barrier covers goes with it
//!   ([`Disk::flush`]). A whole-file write and a rename that wait for the drive flush it too.
//! - A barrier orders and makes nothing durable ([`Disk::barrier`]): a crash that keeps any
//!   change made after the barrier completed, of any file, keeps every change it covers.
//! - A name is durable only once its directory is synced: a file made, written whole in place
//!   of another, renamed or removed is a change of a name ([`Named`]) that a crash may undo
//!   until a sync of the directory ([`Disk::sync_names`]). A file's sync makes its bytes
//!   durable, never its name. A whole-file write and a rename that wait for the drive sync the
//!   directory, as the native drivers do (`fs.rs`'s `write_atomically` and `rename`). The
//!   world's names are all in one directory.
//! - A crash keeps, of each file, its durable bytes and a prefix of its changes whole; of the
//!   changes after them, up to any one of them, each [`SECTOR`] of the file on its own may have
//!   landed or not, and the file's length is anywhere between what the changes kept whole and
//!   those leave ([`Torn`]): a later sector may be there while an earlier one is not, which
//!   reads as zeros past the old end. POSIX promises nothing of unsynced data after a crash,
//!   and real file systems drop and reorder unsynced block writes (ALICE, OSDI 2014) and leave
//!   zero-filled blocks under a grown size (ext4 `data=writeback`, XFS). And it undoes any of
//!   the changes of names not synced, so long as it undoes each later change of a name it
//!   undoes one of. [`Crash`] is a crash's witness ([`Kept`], one file's share, and the names
//!   undone), [`Disk::allows`] says which crashes there are, and [`Disk::crash`] is the disk
//!   after one.
//!
//! A crash is stated by its witness (how much of each file was kept), not by the disk it
//! left, so `allows` is a plain check and a theorem quantifies over witnesses: "for every
//! `cut` the disk allows, recovery from the disk after `crash(cut)` is right". A disk a real
//! power cut left conforms when some allowed cut gives it.
//!
//! A file's name is a number; whoever uses the world gives its names numbers in their order.
//! A function here is one completion: what is asked and not yet answered is its caller's to
//! keep, which is why a sync and a barrier are told how many changes they cover.
//!
//! Not modelled yet: modes, links, holes and clones (a file is its bytes); a full disk; a
//! read cut short; and a barrier's order before a later change of a name. `Effect::Barrier`
//! orders what it covers before every later write, an unsynced rename's new name included
//! ("the bytes ordered before the name"), and `Sim` keeps that (`sim.rs`'s `keep_ordered`:
//! a change of a name a crash does not undo counts as kept, by the epoch it was made in);
//! this world's [`Named`] carries no epoch, so [`Disk::allows`] still allows a crash that keeps
//! an unsynced rename made after a barrier and tears what the barrier covers. The simulation
//! draws fewer cuts than the world allows, never more, so the refinement (`sim/world.rs`)
//! holds; a proof here of a commit by "write, barrier, rename" waits for the epoch on
//! [`Named`] and the proofs under `lean/BlazeCore/World` redone over it.

#![blaze::sans_io]

use std::alloc::Global;

/// What a write lands in or is torn by: a drive's sector, written whole or not at all when the
/// power goes or the write fails. A crash tears every unsynced change by sectors, each on its
/// own ([`Torn::Sectors`]). The grain is the drive's 512-byte sector, not the 4096-byte page
/// writeback flushes: a page's write-out is itself torn by sectors on a drive with no atomic
/// 4096-byte write, and every outcome at the coarser grain is one at this grain too.
pub const SECTOR: usize = 512;

/// The most bytes a file's durable part, a change or an offset may be on a disk the theorems
/// below speak of ([`Disk::small`]), and the most changes a file may have not synced. Far
/// under what a `usize` holds (a file is at most `MOST_BYTES + MOST_CHANGES * 2 * MOST_BYTES`
/// long, under 2^58), so no length the world computes overflows.
pub const MOST_BYTES: usize = 1 << 40;
/// The most changes not synced a file may have, the most barriers no sync has covered, and
/// the most files a disk may have, where the theorems below speak of it: no list the world
/// adds to is ever full. The barriers' bound was missing at first; the proof of
/// `a_crash_keeps_what_a_barrier_ordered` found that the theorem was false without it (a
/// file with `usize::MAX` barriers, on which one more barrier panics), which no run could.
pub const MOST_CHANGES: usize = 1 << 16;

/// A change to a file's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Bytes added at the end. A power cut leaves any of their [`SECTOR`]s ([`Torn`]).
    Append(Vec<u8>),
    /// Bytes written at an offset, the file grown with zeros to hold them. A power cut leaves
    /// any of their [`SECTOR`]s.
    At(usize, Vec<u8>),
    /// Bytes written at an offset from memory, the file grown with zeros to hold them. A
    /// power cut leaves any of their [`SECTOR`]s; one that failed or was in flight too
    /// ([`Disk::land`]).
    Sectors(usize, Vec<u8>),
    /// The file cut to a length, or grown to it with zeros. A power cut leaves it done or not,
    /// or the length anywhere between.
    Cut(usize),
}

/// A change no sync has covered yet, and when it was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Made {
    pub change: Change,
    /// The epoch it was made in: how many barriers had completed.
    pub epoch: u64,
}

/// A barrier no sync has covered yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Barrier {
    /// How many of its file's changes, from the first, it covers.
    pub covers: usize,
    /// The epoch it completed in: a change of a later epoch was made after it.
    pub epoch: u64,
}

/// A change of a name no sync of its directory has covered yet: a crash may undo it, and
/// what it says is what the undo puts back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Named {
    /// A file put under `file`: made where none was (`was` none), or written whole in place
    /// of the file that was, whose durable bytes `was` holds. Undone, the file goes and `was`
    /// is back.
    Put { file: u64, was: Option<Vec<u8>> },
    /// The file under `file` removed; `was` is its durable bytes. Undone, they are back.
    Gone { file: u64, was: Vec<u8> },
    /// The file under `from` renamed to `to`, in place of the file that was, whose durable
    /// bytes `was` holds. Undone, the file is back under `from` and `was` under `to`.
    Moved { from: u64, to: u64, was: Option<Vec<u8>> },
}

/// One file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub name: u64,
    /// The bytes every crash keeps.
    pub durable: Vec<u8>,
    /// What changed it since the last sync that covered them, oldest first.
    pub changes: Vec<Made>,
    /// The barriers of it no sync has covered yet.
    pub barriers: Vec<Barrier>,
}

/// Of the changes a crash does not keep whole, what landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Torn {
    Nothing,
    /// Let `old` be the file with the changes kept whole made, and `new` with its first `upto`
    /// changes made (more than are kept whole, at most all). The file after the crash is `len`
    /// bytes long, between `old`'s length and `new`'s, and each of its [`SECTOR`]s, in order,
    /// is `new`'s where `landed` says so and `old`'s where not; past a source's end it reads
    /// as zeros, a hole. Synced bytes never change: where `old` and `new` agree, either is the
    /// same. A prefix is one of these (each sector under `len` landed).
    Sectors { upto: usize, len: usize, landed: Vec<bool> },
}

/// What a crash keeps of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    /// How many of its changes, from the first, are whole on the disk.
    pub whole: usize,
    /// What landed of the change after them.
    pub torn: Torn,
}

/// What a crash keeps: one [`Kept`] for each file in the disk's order, and for each change of
/// a name not synced ([`Disk::names`], in its order) whether it is undone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crash {
    pub kept: Vec<Kept>,
    pub undone: Vec<bool>,
}

/// The reference model: every file of a disk, the changes of names no sync of the directory
/// has covered, and how many barriers have completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    /// The files in the order of their names, each name once.
    pub files: Vec<File>,
    /// The changes of names not synced, oldest first.
    pub names: Vec<Named>,
    /// How many barriers have completed.
    pub epoch: u64,
}

impl Named {
    /// Whether it changes the name `name`.
    pub fn touches(&self, name: u64) -> bool {
        match self {
            Named::Put { file, .. } | Named::Gone { file, .. } => *file == name,
            Named::Moved { from, to, .. } => *from == name || *to == name,
        }
    }

    /// Whether it and `other` change a name in common: undoing the earlier of the two undoes
    /// the later.
    pub fn shares(&self, other: &Named) -> bool {
        match self {
            Named::Put { file, .. } | Named::Gone { file, .. } => other.touches(*file),
            Named::Moved { from, to, .. } => other.touches(*from) || other.touches(*to),
        }
    }

    /// Whether the bytes it would put back are at most [`MOST_BYTES`].
    fn small(&self) -> bool {
        match self {
            Named::Put { was, .. } | Named::Moved { was, .. } => match was {
                Some(bytes) => bytes.len() <= MOST_BYTES,
                None => true,
            },
            Named::Gone { was, .. } => was.len() <= MOST_BYTES,
        }
    }
}

/// How many [`SECTOR`]s `len` bytes take.
pub const fn sectors(len: usize) -> usize {
    len / SECTOR + if len % SECTOR == 0 { 0 } else { 1 }
}

/// `bytes` cut to `len`, or grown to it with zeros.
fn resize(bytes: &mut Vec<u8>, len: usize) {
    bytes.truncate(len);
    while bytes.len() < len {
        bytes.push(0);
    }
}

/// `more[from..to]` written over `bytes` at `at`, which grows with zeros to hold them.
fn write(bytes: &mut Vec<u8>, at: usize, more: &[u8], from: usize, to: usize) {
    let end = at + (to - from);
    while bytes.len() < end {
        bytes.push(0);
    }
    let mut i = from;
    while i < to {
        bytes[at + (i - from)] = more[i];
        i += 1;
    }
}

/// `source[from..to]` added at the end of `out`, zeros for what is past `source`'s end.
fn take(out: &mut Vec<u8>, source: &[u8], from: usize, to: usize) {
    let mut i = from;
    while i < to {
        if i < source.len() {
            out.push(source[i]);
        } else {
            out.push(0);
        }
        i += 1;
    }
}

/// The bytes a crash that tears by [`Torn::Sectors`] leaves: `len` of them, each [`SECTOR`]
/// `new`'s where `landed` says so and `old`'s where not, zeros past a source's end.
pub fn mixed(old: &[u8], new: &[u8], len: usize, landed: &[bool]) -> Vec<u8> {
    let mut out = Vec::new_in(Global);
    let mut sector = 0;
    let mut from = 0;
    while from < len {
        let to = if len - from < SECTOR { len } else { from + SECTOR };
        if sector < landed.len() && landed[sector] {
            take(&mut out, new, from, to);
        } else {
            take(&mut out, old, from, to);
        }
        from = to;
        sector += 1;
    }
    out
}


/// A copy of `bytes`.
fn copy(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new_in(Global);
    out.extend_from_slice(bytes);
    out
}

/// Whether `bytes` begins with `prefix`.
pub fn starts_with(bytes: &[u8], prefix: &[u8]) -> bool {
    if prefix.len() > bytes.len() {
        return false;
    }
    let mut i = 0;
    while i < prefix.len() {
        if bytes[i] != prefix[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Whether `after`, what a file reads as after a crash, is all of `read`, what it read as
/// before: the file is there, with those bytes and no others.
pub fn kept(read: &[u8], after: &Option<Vec<u8>>) -> bool {
    match after {
        Some(bytes) => bytes.len() == read.len() && starts_with(bytes, read),
        None => false,
    }
}

/// Whether `after`, what a file reads as after a crash, keeps what was synced of `read`, what
/// it read as before: the file is there, begins with all of `durable`, is no longer than
/// `read`, and each byte past `durable` is `read`'s in that place or a zero (a hole where a
/// sector did not land). Nothing durable is gone and nothing is in it that was not written in
/// that place; a middle may be missing while the end is there.
pub fn keeps_what_was_synced(durable: &[u8], read: &[u8], after: &Option<Vec<u8>>) -> bool {
    match after {
        Some(bytes) => {
            if !starts_with(bytes, durable) || bytes.len() > read.len() {
                return false;
            }
            let mut i = durable.len();
            while i < bytes.len() {
                if bytes[i] != read[i] && bytes[i] != 0 {
                    return false;
                }
                i += 1;
            }
            true
        }
        None => false,
    }
}

impl Change {
    /// `bytes` after the change.
    pub fn apply(&self, bytes: &mut Vec<u8>) {
        match self {
            Change::Append(more) => bytes.extend_from_slice(more),
            Change::At(at, more) | Change::Sectors(at, more) => {
                write(bytes, *at, more, 0, more.len());
            }
            Change::Cut(len) => resize(bytes, *len),
        }
    }
}

impl File {
    /// A file whose bytes are all on the disk.
    pub fn whole(name: u64, bytes: Vec<u8>) -> File {
        File {
            name,
            durable: bytes,
            changes: Vec::new_in(Global),
            barriers: Vec::new_in(Global),
        }
    }

    /// What a read of the whole file sees: its durable bytes with every change made.
    pub fn contents(&self) -> Vec<u8> {
        let mut bytes = Vec::new_in(Global);
        bytes.extend_from_slice(&self.durable);
        let mut i = 0;
        while i < self.changes.len() {
            self.changes[i].change.apply(&mut bytes);
            i += 1;
        }
        bytes
    }

    /// Its durable bytes with its first `count` changes made: all of them when it has fewer.
    pub fn after(&self, count: usize) -> Vec<u8> {
        let mut bytes = Vec::new_in(Global);
        bytes.extend_from_slice(&self.durable);
        let mut i = 0;
        while i < count && i < self.changes.len() {
            self.changes[i].change.apply(&mut bytes);
            i += 1;
        }
        bytes
    }

    /// Its first `covers` changes made durable: all of them when it has fewer. A barrier of
    /// what is durable now says nothing more, and one of more covers the rest. Its name is
    /// its directory's to make durable ([`Disk::sync_names`]), never this.
    pub fn sync(&mut self, covers: usize) {
        let covered = if covers < self.changes.len() { covers } else { self.changes.len() };
        let mut done = 0;
        while done < covered {
            let made = self.changes.remove(0);
            made.change.apply(&mut self.durable);
            done += 1;
        }
        let mut i = 0;
        while i < self.barriers.len() {
            if self.barriers[i].covers > covered {
                self.barriers[i].covers -= covered;
                i += 1;
            } else {
                self.barriers.remove(i);
            }
        }
    }

    /// How many of its changes its barriers cover: the most any one of them does.
    pub fn ordered(&self) -> usize {
        let mut most = 0;
        let mut i = 0;
        while i < self.barriers.len() {
            if self.barriers[i].covers > most {
                most = self.barriers[i].covers;
            }
            i += 1;
        }
        most
    }

    /// How many of its changes a crash that keeps `kept` of it leaves on the disk in whole or
    /// in part: a torn change is there in part, whether or not a sector of it landed.
    pub fn reach(&self, kept: &Kept) -> usize {
        let all = self.changes.len();
        let whole = if kept.whole < all { kept.whole } else { all };
        match &kept.torn {
            Torn::Nothing => whole,
            Torn::Sectors { upto, .. } => {
                if *upto > whole && *upto <= all {
                    *upto
                } else {
                    whole
                }
            }
        }
    }

    /// Whether a crash may keep `kept` of this file, looked at alone: no more changes than it
    /// has, and a tear of more changes than are kept whole, at most all, to a length between
    /// the two files they leave, with one landed-or-not for each [`SECTOR`] of that length.
    pub fn fits(&self, kept: &Kept) -> bool {
        if kept.whole > self.changes.len() {
            return false;
        }
        match &kept.torn {
            Torn::Nothing => true,
            Torn::Sectors { upto, len, landed } => {
                if *upto <= kept.whole || *upto > self.changes.len() {
                    return false;
                }
                let old = self.after(kept.whole).len();
                let new = self.after(*upto).len();
                let (low, high) = if old < new { (old, new) } else { (new, old) };
                *len >= low && *len <= high && landed.len() == sectors(*len)
            }
        }
    }

    /// The newest epoch of the changes a crash that keeps `kept` leaves in whole or in part,
    /// or `newest` where that is newer; `none` says no change has been seen yet.
    fn newest(&self, kept: &Kept, none: bool, newest: u64) -> (bool, u64) {
        let reach = self.reach(kept);
        let mut none = none;
        let mut newest = newest;
        let mut i = 0;
        while i < reach {
            let epoch = self.changes[i].epoch;
            if none || epoch > newest {
                newest = epoch;
                none = false;
            }
            i += 1;
        }
        (none, newest)
    }

    /// Whether a crash that keeps `whole` of its changes whole keeps what each of its
    /// barriers that completed before the epoch `newest` covers.
    fn keeps_ordered(&self, whole: usize, newest: u64) -> bool {
        let mut i = 0;
        while i < self.barriers.len() {
            if self.barriers[i].epoch < newest && self.barriers[i].covers > whole {
                return false;
            }
            i += 1;
        }
        true
    }

    /// The file as a crash that keeps `kept` of it leaves it: its durable bytes, the changes
    /// kept whole, and the sectors of the torn ones that landed ([`mixed`]), all of it durable
    /// and nothing else left.
    fn cut(&mut self, kept: &Kept) {
        match &kept.torn {
            Torn::Nothing => self.sync(kept.whole),
            Torn::Sectors { upto, len, landed } => {
                let new = self.after(*upto);
                self.sync(kept.whole);
                self.durable = mixed(&self.durable, &new, *len, landed);
            }
        }
        self.changes.clear();
        self.barriers.clear();
    }

    /// Whether its durable bytes, each change and each offset are at most [`MOST_BYTES`], and
    /// its changes not synced and its barriers at most [`MOST_CHANGES`] each.
    fn small(&self) -> bool {
        if self.durable.len() > MOST_BYTES
            || self.changes.len() > MOST_CHANGES
            || self.barriers.len() > MOST_CHANGES
        {
            return false;
        }
        let mut i = 0;
        while i < self.changes.len() {
            let fits = match &self.changes[i].change {
                Change::Append(more) => more.len() <= MOST_BYTES,
                Change::At(at, more) | Change::Sectors(at, more) => {
                    *at <= MOST_BYTES && more.len() <= MOST_BYTES
                }
                Change::Cut(len) => *len <= MOST_BYTES,
            };
            if !fits {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Whether every change of it not synced is an append.
    pub fn only_appended(&self) -> bool {
        let mut i = 0;
        while i < self.changes.len() {
            match &self.changes[i].change {
                Change::Append(_) => {}
                Change::At(_, _) | Change::Sectors(_, _) | Change::Cut(_) => return false,
            }
            i += 1;
        }
        true
    }

    /// Whether the file is well formed on a disk whose epoch is `epoch`: each barrier covers
    /// some and at most all of its changes, and no change or barrier is of a later epoch.
    fn wf(&self, epoch: u64) -> bool {
        let mut i = 0;
        while i < self.changes.len() {
            if self.changes[i].epoch > epoch {
                return false;
            }
            i += 1;
        }
        i = 0;
        while i < self.barriers.len() {
            let barrier = &self.barriers[i];
            if barrier.covers == 0 || barrier.covers > self.changes.len() || barrier.epoch > epoch {
                return false;
            }
            i += 1;
        }
        true
    }
}

impl Disk {
    /// A disk with no file.
    pub fn new() -> Disk {
        Disk { files: Vec::new_in(Global), names: Vec::new_in(Global), epoch: 0 }
    }

    /// Whether a change of `file`'s name is not synced: a crash may undo it.
    pub fn named(&self, file: u64) -> bool {
        let mut i = 0;
        while i < self.names.len() {
            if self.names[i].touches(file) {
                return true;
            }
            i += 1;
        }
        false
    }

    /// A sync of the directory completed: every change of a name is durable, and the drive
    /// is flushed. The native drivers' `Effect::SyncDir`: `fsync` of the directory,
    /// `F_FULLFSYNC` on Apple's systems.
    pub fn sync_names(&mut self) {
        self.names.clear();
        self.flush();
    }

    /// Where the file named `name` is in [`Disk::files`].
    pub fn find(&self, name: u64) -> Option<usize> {
        let mut i = 0;
        while i < self.files.len() {
            if self.files[i].name == name {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Where the file named `name` is or would go: the first file whose name is not below it.
    fn slot(&self, name: u64) -> usize {
        let mut i = 0;
        while i < self.files.len() && self.files[i].name < name {
            i += 1;
        }
        i
    }

    /// The file named `name`, made empty where there is none (a change of the name, not
    /// durable until the directory is synced): where it is.
    fn open(&mut self, name: u64) -> usize {
        let at = self.slot(name);
        if at == self.files.len() || self.files[at].name != name {
            self.files.insert(at, File::whole(name, Vec::new_in(Global)));
            self.names.push(Named::Put { file: name, was: None });
        }
        at
    }

    /// The file named `name`, taken off the disk.
    fn take(&mut self, name: u64) -> Option<File> {
        match self.find(name) {
            Some(at) => Some(self.files.remove(at)),
            None => None,
        }
    }

    /// `file` under its name, in place of the file that had it.
    fn put(&mut self, file: File) {
        let at = self.slot(file.name);
        if at < self.files.len() && self.files[at].name == file.name {
            self.files[at] = file;
        } else {
            self.files.insert(at, file);
        }
    }

    /// `change` made to the file at `at`, in the epoch that is now.
    fn change(&mut self, at: usize, change: Change) {
        let epoch = self.epoch;
        self.files[at].changes.push(Made { change, epoch });
    }

    /// What a read of the whole of `file` sees; none when there is no such file.
    pub fn contents(&self, file: u64) -> Option<Vec<u8>> {
        match self.find(file) {
            Some(at) => Some(self.files[at].contents()),
            None => None,
        }
    }

    /// An append completed: `bytes` are at the end of `file`, which is made where there was
    /// none. An append that failed is this with the bytes that landed, a prefix of them.
    pub fn append(&mut self, file: u64, bytes: Vec<u8>) {
        let at = self.open(file);
        self.change(at, Change::Append(bytes));
    }

    /// A write at an offset completed. `false`, and nothing changed, where there is no such
    /// file.
    pub fn write_at(&mut self, file: u64, at: usize, bytes: Vec<u8>) -> bool {
        match self.find(file) {
            Some(found) => {
                self.change(found, Change::At(at, bytes));
                true
            }
            None => false,
        }
    }

    /// A write from memory completed. `false`, and nothing changed, where there is no such
    /// file.
    pub fn write_from(&mut self, file: u64, at: usize, bytes: Vec<u8>) -> bool {
        match self.find(file) {
            Some(found) => {
                self.change(found, Change::Sectors(at, bytes));
                true
            }
            None => false,
        }
    }

    /// A write from memory failed, or was in flight when the power went: each [`SECTOR`] of
    /// `bytes` that `landed` says is written, as a write from memory of its own. `false`, and
    /// nothing changed, where there is no such file.
    pub fn land(&mut self, file: u64, at: usize, bytes: &[u8], landed: &[bool]) -> bool {
        let Some(found) = self.find(file) else { return false };
        let mut sector = 0;
        let mut from = 0;
        while sector < landed.len() && from < bytes.len() {
            let to = if bytes.len() - from < SECTOR { bytes.len() } else { from + SECTOR };
            if landed[sector] {
                let mut part = Vec::new_in(Global);
                let mut i = from;
                while i < to {
                    part.push(bytes[i]);
                    i += 1;
                }
                self.change(found, Change::Sectors(at + from, part));
            }
            from = to;
            sector += 1;
        }
        true
    }

    /// A truncation completed: `file`, made where there was none, is `len` long.
    pub fn truncate(&mut self, file: u64, len: usize) {
        let at = self.open(file);
        self.change(at, Change::Cut(len));
    }

    /// A sync of `file` completed: its first `covers` changes are durable, as many as had
    /// been made when the sync was asked and are still not durable, and the drive is flushed.
    /// The file is made, empty, where there was none.
    pub fn sync(&mut self, file: u64, covers: usize) {
        let at = self.open(file);
        self.files[at].sync(covers);
        self.flush();
    }

    /// A barrier of `file` completed: nothing is durable, and its first `covers` changes, as
    /// many as had been made when the barrier was asked, are ordered before every change
    /// made from now on, of any file. The file is made, empty, where there was none.
    pub fn barrier(&mut self, file: u64, covers: usize) {
        let at = self.open(file);
        let all = self.files[at].changes.len();
        let covered = if covers < all { covers } else { all };
        if covered > 0 {
            let epoch = self.epoch;
            self.files[at].barriers.push(Barrier { covers: covered, epoch });
        }
        // At the last epoch a counter can hold, a later change is no longer told from an
        // earlier one, and a crash is allowed more, never less: what is proved still holds.
        self.epoch = self.epoch.saturating_add(1);
    }

    /// The drive's cache flushed, as a sync, a sync of a directory, a whole-file write and a
    /// rename that wait for the drive all do: what each file's barriers cover is durable.
    pub fn flush(&mut self) {
        let mut i = 0;
        while i < self.files.len() {
            if !self.files[i].barriers.is_empty() {
                let covered = self.files[i].ordered();
                self.files[i].sync(covered);
            }
            i += 1;
        }
    }

    /// A whole-file write that waited for the drive completed: `file` is `bytes`, durable,
    /// in place of whatever the name held, and the directory synced, so every change of a
    /// name is durable and the drive is flushed.
    pub fn write(&mut self, file: u64, bytes: Vec<u8>) {
        self.put(File::whole(file, bytes));
        self.sync_names();
    }

    /// A whole-file write that waited for no sync completed: `file` reads as `bytes`, none
    /// of them durable, and a crash may put back what the name held.
    pub fn write_unsynced(&mut self, file: u64, bytes: Vec<u8>) {
        let was = match self.take(file) {
            Some(old) => Some(old.durable),
            None => None,
        };
        let mut new = File::whole(file, Vec::new_in(Global));
        let epoch = self.epoch;
        new.changes.push(Made { change: Change::Sectors(0, bytes), epoch });
        self.put(new);
        self.names.push(Named::Put { file, was });
    }

    /// A rename that waited for the drive completed: the file is under `to`, in place of
    /// whatever that name held, the directory is synced, so every change of a name is
    /// durable (the file's changes are as durable as they were), and the drive is flushed.
    /// `false`, and nothing changed, where there is no file under `from`.
    pub fn rename(&mut self, from: u64, to: u64) -> bool {
        match self.take(from) {
            Some(mut file) => {
                file.name = to;
                self.put(file);
                self.sync_names();
                true
            }
            None => false,
        }
    }

    /// A rename that waited for no sync completed: the file is under `to`, and a crash may
    /// put it back under `from` and what `to` held back under `to`. `false`, and nothing
    /// changed, where there is no file under `from`.
    pub fn rename_unsynced(&mut self, from: u64, to: u64) -> bool {
        match self.take(from) {
            Some(mut file) => {
                let was = match self.take(to) {
                    Some(old) => Some(old.durable),
                    None => None,
                };
                file.name = to;
                self.put(file);
                self.names.push(Named::Moved { from, to, was });
                true
            }
            None => false,
        }
    }

    /// A removal completed: no file is under the name, and a crash may put back its durable
    /// bytes. `false` where none was.
    pub fn remove(&mut self, file: u64) -> bool {
        match self.take(file) {
            Some(gone) => {
                self.names.push(Named::Gone { file, was: gone.durable });
                true
            }
            None => false,
        }
    }

    /// The change of a name `named` undone, every later one that shares a name with it
    /// undone already: the name holds again what it held before.
    fn undo(&mut self, named: Named) {
        match named {
            Named::Put { file, was } => {
                let _ = self.take(file);
                if let Some(was) = was {
                    self.put(File::whole(file, was));
                }
            }
            Named::Gone { file, was } => self.put(File::whole(file, was)),
            Named::Moved { from, to, was } => {
                if let Some(mut moved) = self.take(to) {
                    moved.name = from;
                    self.put(moved);
                }
                if let Some(was) = was {
                    self.put(File::whole(to, was));
                }
            }
        }
    }

    /// The newest epoch of the changes a crash that keeps `cut` leaves in whole or in part;
    /// none when it leaves no change.
    fn newest(&self, cut: &[Kept]) -> Option<u64> {
        let mut none = true;
        let mut newest = 0;
        let mut i = 0;
        while i < self.files.len() && i < cut.len() {
            let (still, epoch) = self.files[i].newest(&cut[i], none, newest);
            none = still;
            newest = epoch;
            i += 1;
        }
        if none { None } else { Some(newest) }
    }

    /// Whether a crash may make `crash`: it keeps of the files what [`Disk::keeps`] allows,
    /// and says of each change of a name not synced whether it is undone, undoing each later
    /// change that shares a name with one it undoes (a name's changes are kept in order).
    pub fn allows(&self, crash: &Crash) -> bool {
        if crash.undone.len() != self.names.len() {
            return false;
        }
        let mut i = 0;
        while i < self.names.len() {
            if crash.undone[i] {
                let mut j = i + 1;
                while j < self.names.len() {
                    if !crash.undone[j] && self.names[i].shares(&self.names[j]) {
                        return false;
                    }
                    j += 1;
                }
            }
            i += 1;
        }
        self.keeps(&crash.kept)
    }

    /// Whether a crash may keep `cut` of the files: one [`Kept`] for each file in the disk's
    /// order, each fitting its file ([`File::fits`]), and no change kept, whole or in part,
    /// that was made after a barrier completed unless every change that barrier covers is
    /// kept whole.
    pub fn keeps(&self, cut: &[Kept]) -> bool {
        if cut.len() != self.files.len() {
            return false;
        }
        let mut i = 0;
        while i < cut.len() {
            if !self.files[i].fits(&cut[i]) {
                return false;
            }
            i += 1;
        }
        let Some(newest) = self.newest(cut) else { return true };
        i = 0;
        while i < cut.len() {
            if !self.files[i].keeps_ordered(cut[i].whole, newest) {
                return false;
            }
            i += 1;
        }
        true
    }

    /// The disk after a crash `crash` ([`Disk::allows`]): each file is the bytes kept of it,
    /// all of them durable; then each change of a name undone is undone, the newest first
    /// ([`Named`] says what it puts back, as far as it was durable), and what is left of the
    /// names is durable. The barriers completed stay counted.
    pub fn crash(&mut self, crash: &Crash) {
        let mut i = 0;
        while i < self.files.len() && i < crash.kept.len() {
            self.files[i].cut(&crash.kept[i]);
            i += 1;
        }
        let mut names = core::mem::take(&mut self.names);
        let mut i = names.len();
        while i > 0 {
            i -= 1;
            let named = names.remove(i);
            if i < crash.undone.len() && crash.undone[i] {
                self.undo(named);
            }
        }
    }

    /// Whether the disk is within the bounds the theorems below speak of: at most
    /// [`MOST_CHANGES`] files and changes of names, each within [`MOST_BYTES`] and
    /// [`MOST_CHANGES`].
    pub fn small(&self) -> bool {
        if self.files.len() > MOST_CHANGES || self.names.len() > MOST_CHANGES {
            return false;
        }
        let mut i = 0;
        while i < self.names.len() {
            if !self.names[i].small() {
                return false;
            }
            i += 1;
        }
        i = 0;
        while i < self.files.len() {
            if !self.files[i].small() {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Whether the disk is well formed, which every function here keeps: the files are in
    /// the order of their names, each name once, and each is well formed at the disk's epoch
    /// (`File::wf`).
    pub fn wf(&self) -> bool {
        let mut i = 0;
        while i < self.files.len() {
            if !self.files[i].wf(self.epoch) {
                return false;
            }
            if i > 0 && self.files[i - 1].name >= self.files[i].name {
                return false;
            }
            i += 1;
        }
        true
    }
}

impl Default for Disk {
    fn default() -> Disk {
        Disk::new()
    }
}

/// The invariant (proved: crates/blaze-core/lean/BlazeCore/World/Disk.lean): a crash keeps a
/// synced file. From every well-formed disk within the bounds,
/// for a file whose name no change not synced touches (its directory was synced since it was
/// made, written whole or renamed there), once a sync of `file` that covers all its changes
/// has completed, whatever a crash is allowed to do, `file` is there after it and reads as
/// it did before ([`kept`]): no change of it is lost or torn, no change of another name that
/// is undone takes its name, and nothing here overflows or panics. A file's sync alone keeps
/// its bytes, not its name. Its controls are
/// `the_oracle_catches_a_file_that_lost_what_was_not_synced` and
/// `a_synced_file_whose_name_was_not_synced_may_be_gone`.
#[blaze::theorem]
pub fn a_crash_keeps_a_synced_file(disk: Disk, file: u64, cut: Crash) -> bool {
    let mut disk = disk;
    if !disk.wf() || !disk.small() {
        return true;
    }
    let Some(at) = disk.find(file) else { return true };
    if disk.named(file) {
        return true;
    }
    let read = disk.files[at].contents();
    let all = disk.files[at].changes.len();
    disk.sync(file, all);
    if !disk.allows(&cut) {
        return true;
    }
    disk.crash(&cut);
    kept(&read, &disk.contents(file))
}

/// The invariant (NOT YET PROVED: it replaces `a_crash_cuts_an_appended_file_to_a_prefix`,
/// false since a crash tears by sectors; D55): a crash keeps what was synced of an appended
/// file. From every well-formed disk within the bounds, for a file whose changes not synced
/// are all appends and whose name no change not synced touches, whatever a crash is allowed
/// to do, the file is there after it, begins with all its durable bytes, is no longer than
/// what it read as before, and each byte past the durable ones is the one read there or a
/// zero ([`keeps_what_was_synced`]). So a log that is only appended to keeps its synced bytes
/// and length; its unsynced end may be missing in the middle with later bytes there, which is
/// why a reader checks each record. Its control is
/// `the_oracle_catches_a_file_that_lost_what_was_synced`.
#[blaze::theorem]
pub fn a_crash_keeps_what_was_synced_of_an_appended_file(disk: Disk, file: u64, cut: Crash) -> bool {
    let mut disk = disk;
    if !disk.wf() || !disk.small() {
        return true;
    }
    let Some(at) = disk.find(file) else { return true };
    if !disk.files[at].only_appended() || disk.named(file) {
        return true;
    }
    let read = disk.files[at].contents();
    let durable = copy(&disk.files[at].durable);
    if !disk.allows(&cut) {
        return true;
    }
    disk.crash(&cut);
    keeps_what_was_synced(&durable, &read, &disk.contents(file))
}

/// The invariant (proved: crates/blaze-core/lean/BlazeCore/World/Disk.lean): a barrier
/// orders. From every well-formed disk within the bounds, once a
/// barrier of `file` that covers all its changes has completed and `bytes` were then appended
/// to `later` (any file, `file` itself too), a cut a crash is allowed to make that keeps any
/// part of that append keeps whole every change the barrier covered. So a commit record
/// appended after a barrier of the data it names is on the disk after a crash only if the
/// data is, without a sync between them. Its control is
/// `the_oracle_catches_a_later_append_kept_without_what_was_ordered`.
#[blaze::theorem]
pub fn a_crash_keeps_what_a_barrier_ordered(
    disk: Disk,
    file: u64,
    later: u64,
    bytes: Vec<u8>,
    cut: Crash,
) -> bool {
    let mut disk = disk;
    // At the last epoch a counter holds, a later change is not told from an earlier one.
    if !disk.wf() || !disk.small() || disk.epoch == u64::MAX {
        return true;
    }
    let Some(at) = disk.find(file) else { return true };
    let covers = disk.files[at].changes.len();
    disk.barrier(file, covers);
    disk.append(later, bytes);
    if !disk.allows(&cut) {
        return true;
    }
    let Some(ordered) = disk.find(file) else { return false };
    let Some(appended) = disk.find(later) else { return false };
    let last = disk.files[appended].changes.len() - 1;
    ordered_first(disk.files[appended].reach(&cut.kept[appended]), last, cut.kept[ordered].whole, covers)
}

/// Whether a cut that leaves `reach` changes of the later file on the disk in whole or in
/// part, the append after the barrier being its change `last`, and keeps `whole` changes of
/// the ordered file whole, keeps the barrier's order: none of the append, or all `covers`
/// changes the barrier covered.
pub const fn ordered_first(reach: usize, last: usize, whole: usize, covers: usize) -> bool {
    reach <= last || whole >= covers
}

#[cfg(test)]
pub(crate) mod tests {
    use blaze_sim::{Broke, Config, Tape, check};

    use super::*;

    /// A disk a run of drawn completions leaves: three names, small changes, syncs and
    /// barriers that cover all or a part, the writes and renames that wait for no sync.
    pub(crate) fn disk(tape: &Tape) -> Disk {
        let mut disk = Disk::new();
        let file = |tape: &Tape| tape.below(3, "the file");
        let bytes = |tape: &Tape| match tape.one_in(5, "bytes over sectors") {
            true => tape.bytes(SECTOR..=2 * SECTOR + 9, "the bytes"),
            false => tape.bytes(1..=5, "the bytes"),
        };
        for _ in 0..tape.below(14, "more completions") {
            let name = file(tape);
            let covers = |disk: &Disk, tape: &Tape| {
                let all = disk.find(name).map_or(0, |at| disk.files[at].changes.len());
                all - tape.index(all + 1, "changes the sync does not cover")
            };
            match tape.below(12, "the completion") {
                10 => _ = disk.remove(name),
                11 => disk.sync_names(),
                0 => disk.append(name, bytes(tape)),
                1 => {
                    let covers = covers(&disk, tape);
                    disk.sync(name, covers);
                }
                2 => {
                    let covers = covers(&disk, tape);
                    disk.barrier(name, covers);
                }
                3 => _ = disk.write_at(name, tape.index(2 * SECTOR, "the offset"), bytes(tape)),
                4 => _ = disk.write_from(name, tape.index(3, "the sector") * SECTOR, bytes(tape)),
                5 => disk.truncate(name, tape.index(SECTOR + 9, "the length")),
                6 => disk.write_unsynced(name, bytes(tape)),
                7 => _ = disk.rename_unsynced(name, file(tape)),
                8 => disk.write(name, bytes(tape)),
                _ => _ = disk.rename(name, file(tape)),
            }
            assert!(disk.wf(), "{disk:?}");
        }
        disk
    }

    /// A crash drawn for `disk`: each file's share fitting it, whether the disk allows it the
    /// barriers' to say, and changes of names undone, each later one that shares a name with
    /// one undone undone too. Calm keeps every change whole and undoes nothing.
    pub(crate) fn cut(tape: &Tape, disk: &Disk) -> Crash {
        let mut undone: Vec<bool> = Vec::new_in(Global);
        for (i, named) in disk.names.iter().enumerate() {
            let forced = (0..i).any(|earlier| undone[earlier] && disk.names[earlier].shares(named));
            undone.push(forced || tape.one_in(3, "undone"));
        }
        Crash { kept: kept_of(tape, disk), undone }
    }

    /// Each file's share of a crash drawn for `disk`, fitting it.
    pub(crate) fn kept_of(tape: &Tape, disk: &Disk) -> Vec<Kept> {
        disk.files
            .iter()
            .map(|file| {
                let all = file.changes.len();
                let whole = all - tape.index(all + 1, "changes not kept");
                let torn = match whole < all && tape.coin("torn") {
                    true => {
                        let upto = whole + 1 + tape.index(all - whole, "changes that may have landed");
                        let old = file.after(whole).len();
                        let new = file.after(upto).len();
                        let (low, high) = if old < new { (old, new) } else { (new, old) };
                        let len = low + tape.index(high - low + 1, "the length kept");
                        let landed = (0..sectors(len)).map(|_| tape.coin("a sector landed")).collect();
                        Torn::Sectors { upto, len, landed }
                    }
                    false => Torn::Nothing,
                };
                Kept { whole, torn }
            })
            .collect()
    }

    fn holds(what: &str, held: bool, disk: &Disk, cut: &Crash) -> Result<(), Broke> {
        match held {
            true => Ok(()),
            false => Err(Broke::Oracle(format!("{what} does not hold of {disk:?} cut to {cut:?}"))),
        }
    }

    /// The theorem functions hold where they are run: on drawn disks and drawn cuts, each
    /// reached with a cut its disk allows, a crash that tears and a crash that undoes among
    /// them. What Lean is to prove of their models for every disk and cut.
    #[test]
    fn the_theorems_hold_where_they_are_run() {
        let config = Config {
            must_reach: &[
                "a synced file and an allowed cut",
                "a synced file whose name was synced and a crash that undoes another name",
                "an appended file and an allowed cut that loses some",
                "an appended file whose end survived and whose middle did not",
                "a barrier and an allowed cut that keeps the later append",
                "a cut the barriers refuse",
            ],
            ..Config::cases(4000)
        };
        check(config, |tape| -> Result<(), Broke> {
            let made = disk(tape);
            let name = tape.below(3, "the theorem's file");
            let Some(at) = made.find(name) else { return Ok(()) };

            // A synced file: the cut is of the disk the sync leaves.
            // clone: the test's copy of the disk, as each theorem takes its own
            let mut synced = made.clone();
            synced.sync(name, made.files[at].changes.len());
            let drawn = cut(tape, &synced);
            // A sync flushes the drive, so no barrier is left to refuse a cut of this disk.
            if synced.allows(&drawn) {
                tape.reached("a synced file and an allowed cut");
                if !synced.named(name) && drawn.undone.iter().any(|it| *it) {
                    tape.reached("a synced file whose name was synced and a crash that undoes another name");
                }
            }
            // clone: as above
            holds("a_crash_keeps_a_synced_file", a_crash_keeps_a_synced_file(made.clone(), name, drawn.clone()), &made, &drawn)?;

            // An appended file: the cut is of the disk as it is.
            let drawn = cut(tape, &made);
            let file = &made.files[at];
            if file.only_appended() && !made.named(name) && made.allows(&drawn) && drawn.kept[at].whole < file.changes.len() {
                tape.reached("an appended file and an allowed cut that loses some");
            }
            // clone: as above
            if file.only_appended() && !made.named(name) && made.allows(&drawn) {
                // clone: the test's copy of the disk, to look at the crash's outcome
                let mut after = made.clone();
                after.crash(&drawn);
                let read = file.contents();
                let left = after.contents(name).unwrap_or_default();
                if left.len() == read.len() && left.len() > file.durable.len() && left != read {
                    tape.reached("an appended file whose end survived and whose middle did not");
                }
            }
            holds(
                "a_crash_keeps_what_was_synced_of_an_appended_file",
                a_crash_keeps_what_was_synced_of_an_appended_file(made.clone(), name, drawn.clone()),
                &made,
                &drawn,
            )?;

            // A barrier and a later append: the cut is of the disk they leave.
            let later = tape.below(3, "the file appended to after the barrier");
            let bytes = tape.bytes(1..=4, "the later append");
            // clone: as above
            let mut ordered = made.clone();
            ordered.barrier(name, made.files[at].changes.len());
            // clone: the test's copy of the bytes
            ordered.append(later, bytes.clone());
            let drawn = cut(tape, &ordered);
            let appended = ordered.find(later).expect("appended to");
            if !ordered.allows(&drawn) {
                tape.reached("a cut the barriers refuse");
            }
            if ordered.allows(&drawn)
                && ordered.files[appended].reach(&drawn.kept[appended]) == ordered.files[appended].changes.len()
                && !made.files[at].changes.is_empty()
            {
                tape.reached("a barrier and an allowed cut that keeps the later append");
            }
            // clone: as above
            holds(
                "a_crash_keeps_what_a_barrier_ordered",
                a_crash_keeps_what_a_barrier_ordered(made.clone(), name, later, bytes, drawn.clone()),
                &made,
                &drawn,
            )
        });
    }

    /// A file that lost its last bytes, or is gone, is not [`kept`]: without the sync, a
    /// crash that keeps none of an append leaves a file the oracle refuses.
    #[test]
    fn the_oracle_catches_a_file_that_lost_what_was_not_synced() {
        let mut disk = Disk::new();
        disk.write(1, b"synced".to_vec());
        disk.append(1, b"-not".to_vec());
        let read = disk.contents(1).expect("there");
        let none = crash(&[Kept { whole: 0, torn: Torn::Nothing }], &[]);
        assert!(disk.allows(&none));
        disk.crash(&none);
        assert_eq!(disk.contents(1).as_deref(), Some(&b"synced"[..]));
        assert!(!kept(&read, &disk.contents(1)));
        assert!(!kept(&read, &None));
        assert!(kept(b"synced", &disk.contents(1)));
    }

    /// A file whose durable bytes were written over, one missing its durable bytes, one
    /// longer than was written, one holding a byte not written in that place and no file at
    /// all do not [`keeps_what_was_synced`]: a write at an offset, which the theorem leaves
    /// out, is torn to what the oracle refuses. An append whose first sector was lost and
    /// whose second landed (a hole, then the end) is what the oracle and the world allow.
    #[test]
    fn the_oracle_catches_a_file_that_lost_what_was_synced() {
        let mut disk = Disk::new();
        disk.write(1, b"durable".to_vec());
        disk.append(1, b"-more".to_vec());
        assert!(disk.write_at(1, 0, b"DUR".to_vec()));
        let read = disk.contents(1).expect("there");
        // The append kept whole and the write at the start landed.
        let torn = crash(&[Kept { whole: 1, torn: Torn::Sectors { upto: 2, len: 12, landed: vec![true] } }], &[]);
        assert!(disk.allows(&torn));
        disk.crash(&torn);
        assert_eq!(disk.contents(1).as_deref(), Some(&b"DURable-more"[..]));
        assert!(!keeps_what_was_synced(b"durable", &read, &disk.contents(1)));
        assert!(!keeps_what_was_synced(b"durable", b"durable-more", &Some(b"dur".to_vec())));
        assert!(!keeps_what_was_synced(b"durable", b"durable-more", &Some(b"durable-more!".to_vec())));
        assert!(!keeps_what_was_synced(b"durable", b"durable-more", &Some(b"durable-mXre".to_vec())));
        assert!(!keeps_what_was_synced(b"durable", b"durable-more", &None));
        assert!(keeps_what_was_synced(b"durable", b"durable-more", &Some(b"durable-m".to_vec())));

        // A hole, then the end: the append's first sector lost, its second there.
        let mut disk = Disk::new();
        disk.write(1, Vec::new());
        disk.append(1, vec![7; 2 * SECTOR]);
        let read = disk.contents(1).expect("there");
        let hole = crash(&[Kept { whole: 0, torn: Torn::Sectors { upto: 1, len: 2 * SECTOR, landed: vec![false, true] } }], &[]);
        assert!(disk.allows(&hole));
        // A length past what was written, and a mask for another length, are no tear of it.
        for refused in [
            Torn::Sectors { upto: 1, len: 2 * SECTOR + 1, landed: vec![true, true, true] },
            Torn::Sectors { upto: 1, len: 2 * SECTOR, landed: vec![true] },
            Torn::Sectors { upto: 2, len: 2 * SECTOR, landed: vec![true, true] },
        ] {
            assert!(!disk.allows(&crash(&[Kept { whole: 0, torn: refused.clone() }], &[])), "{refused:?}");
        }
        disk.crash(&hole);
        let left = disk.contents(1).expect("there");
        assert!(left[..SECTOR].iter().all(|it| *it == 0) && left[SECTOR..].iter().all(|it| *it == 7));
        assert!(keeps_what_was_synced(b"", &read, &Some(left)));
    }

    /// A cut that keeps the append made after a barrier and not what the barrier covered is
    /// one [`ordered_first`] refuses, and one the disk does not allow; with a write-back's
    /// nothing where the barrier was, the disk allows it.
    #[test]
    fn the_oracle_catches_a_later_append_kept_without_what_was_ordered() {
        let appended = |barrier: bool| {
            let mut disk = Disk::new();
            disk.append(1, b"data".to_vec());
            if barrier {
                disk.barrier(1, 1);
            }
            disk.append(2, b"commit".to_vec());
            disk
        };
        // None of the data, all of the commit.
        // The commit's file is made by its append: a change of its name, kept.
        let cut = crash(&[Kept { whole: 0, torn: Torn::Nothing }, Kept { whole: 1, torn: Torn::Nothing }], &[false]);
        let made_both = crash(
            &[Kept { whole: 0, torn: Torn::Nothing }, Kept { whole: 1, torn: Torn::Nothing }],
            &[false, false],
        );
        assert!(!ordered_first(1, 0, 0, 1));
        // Both files were made by their appends: two changes of names, neither synced.
        assert!(!appended(true).allows(&made_both));
        assert!(appended(false).allows(&made_both));
        // A crash that says nothing of one of the changes of names is no crash of this disk.
        assert!(!appended(false).allows(&cut));
        assert!(ordered_first(0, 0, 0, 1) && ordered_first(1, 0, 1, 1));
    }

    /// A crash of `kept` that undoes the changes of names `undone` says to.
    fn crash(kept: &[Kept], undone: &[bool]) -> Crash {
        let mut crash = Crash { kept: Vec::new_in(Global), undone: Vec::new_in(Global) };
        crash.kept.extend_from_slice(kept);
        crash.undone.extend_from_slice(undone);
        crash
    }

    /// The strict rule's control: a file's sync keeps its bytes, never its name. A file made
    /// and synced whose directory was not synced may be gone after a crash, which is why
    /// [`a_crash_keeps_a_synced_file`] asks that no change of its name is left unsynced; the
    /// same file with its directory synced is kept by every crash.
    #[test]
    fn a_synced_file_whose_name_was_not_synced_may_be_gone() {
        let mut disk = Disk::new();
        disk.append(1, b"bytes".to_vec());
        disk.sync(1, 1);
        assert!(disk.named(1), "a file made is a change of a name until its directory is synced");
        let undone = crash(&[Kept { whole: 0, torn: Torn::Nothing }], &[true]);
        assert!(disk.allows(&undone));
        // clone: the test's copy, crashed
        let mut lost = disk.clone();
        lost.crash(&undone);
        assert!(!kept(b"bytes", &lost.contents(1)), "the crash took the name a file sync did not keep");
        // The theorem does not speak of it: its premise is the name's sync.
        assert!(a_crash_keeps_a_synced_file(disk.clone(), 1, undone.clone()));
        // With the directory synced the name is durable: no crash of this disk undoes it.
        disk.sync_names();
        assert!(!disk.named(1));
        assert!(!disk.allows(&undone));
        let calm = crash(&[Kept { whole: 0, torn: Torn::Nothing }], &[]);
        assert!(disk.allows(&calm));
        disk.crash(&calm);
        assert!(kept(b"bytes", &disk.contents(1)));
    }

    /// A rename's name is durable only once its directory is synced: a crash may put the file
    /// back under its old name and what the new name held back under it, and undoing the
    /// rename undoes every later change of either name with it.
    #[test]
    fn an_unsynced_rename_is_undone_with_what_came_after_it() {
        let mut disk = Disk::new();
        disk.write(1, b"new".to_vec());
        disk.write(2, b"old".to_vec());
        assert!(disk.names.is_empty(), "a whole-file write that waited syncs its directory");
        assert!(disk.rename_unsynced(1, 2));
        disk.append(2, b"+".to_vec());
        disk.sync(2, 1);
        assert!(disk.remove(2));
        // The rename undone without the remove after it is no crash.
        assert!(!disk.allows(&crash(&[], &[true, false])));
        let both = crash(&[], &[true, true]);
        assert!(disk.allows(&both));
        disk.crash(&both);
        // The file is back under its old name with what was synced of it, its append too.
        assert_eq!(disk.contents(1).as_deref(), Some(&b"new+"[..]));
        assert_eq!(disk.contents(2).as_deref(), Some(&b"old"[..]));
        assert!(disk.names.is_empty());
    }
}
