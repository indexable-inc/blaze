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
//! - A crash keeps, of each file, its durable bytes and a prefix of its changes; of the first
//!   change it does not keep, a part may have landed ([`Torn`]); and a whole-file write or a
//!   rename that waited for no sync may be undone ([`Undo`]). [`Kept`] is one file's share of
//!   a crash, [`Disk::allows`] says which crashes there are, and [`Disk::crash`] is the disk
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
//! read cut short.

#![blaze::sans_io]

use std::alloc::Global;

/// What a write from memory lands in or is torn by: a drive's sector, written whole or not at
/// all when the power goes or the write fails.
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
    /// Bytes added at the end. A power cut leaves a prefix of them.
    Append(Vec<u8>),
    /// Bytes written at an offset, the file grown with zeros to hold them. A power cut leaves
    /// a prefix of them.
    At(usize, Vec<u8>),
    /// Bytes written at an offset from memory, the file grown with zeros to hold them. A
    /// power cut leaves any of their [`SECTOR`]s.
    Sectors(usize, Vec<u8>),
    /// The file cut to a length, or grown to it with zeros. A power cut leaves it done or not.
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

/// What a crash puts back when it undoes a whole-file write or a rename that waited for no
/// sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undo {
    /// The durable bytes of the file the name held before; none when it held no file.
    pub was: Option<Vec<u8>>,
    /// A rename's source: the name the file goes back under.
    pub from: Option<u64>,
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
    /// Set by a whole-file write or a rename that waited for no sync, until a sync covers
    /// every change of the file: a crash may undo it.
    pub undo: Option<Undo>,
}

/// Of the first change a crash does not keep whole, what landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Torn {
    Nothing,
    /// The first bytes, this many, of an append or of a write at an offset.
    Prefix(usize),
    /// Of a write from memory, each of its [`SECTOR`]s in order: landed or not.
    Sectors(Vec<bool>),
}

/// What a crash keeps of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    /// How many of its changes, from the first, are whole on the disk.
    pub whole: usize,
    /// What landed of the change after them.
    pub torn: Torn,
    /// Whether the write or rename that put the file under its name without a sync is undone.
    pub undone: bool,
}

/// The reference model: every file of a disk, and how many barriers have completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    /// The files in the order of their names, each name once.
    pub files: Vec<File>,
    /// How many barriers have completed.
    pub epoch: u64,
}

/// An undone write or rename a crash still has to put back.
struct Undone {
    name: u64,
    undo: Undo,
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

/// The first `count` of `more` added at the end of `bytes`: all of them when it has fewer.
fn extend(bytes: &mut Vec<u8>, more: &[u8], count: usize) {
    let mut i = 0;
    while i < count && i < more.len() {
        bytes.push(more[i]);
        i += 1;
    }
}

/// Each [`SECTOR`] of `more` that `landed` says, written over `bytes` where the whole of
/// `more` would go at `at`.
fn land(bytes: &mut Vec<u8>, at: usize, more: &[u8], landed: &[bool]) {
    let mut sector = 0;
    let mut from = 0;
    while sector < landed.len() && from < more.len() {
        let to = if more.len() - from < SECTOR { more.len() } else { from + SECTOR };
        if landed[sector] {
            write(bytes, at + from, more, from, to);
        }
        from = to;
        sector += 1;
    }
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

/// Whether `after`, what a file reads as after a crash, is a part of `read`, what it read as
/// before, that begins with all of `durable`: the file is there, nothing that was durable is
/// gone, and nothing is in it that was not written in that place.
pub fn cut_to_a_prefix(durable: &[u8], read: &[u8], after: &Option<Vec<u8>>) -> bool {
    match after {
        Some(bytes) => starts_with(bytes, durable) && starts_with(read, bytes),
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
            undo: None,
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

    /// Its first `covers` changes made durable: all of them when it has fewer. A barrier of
    /// what is durable now says nothing more, and one of more covers the rest. With no change
    /// left the file is on the disk under its name, and nothing is left to undo.
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
        if self.changes.is_empty() {
            self.undo = None;
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
    /// in part: a torn change is there in part.
    pub fn reach(&self, kept: &Kept) -> usize {
        let all = self.changes.len();
        let whole = if kept.whole < all { kept.whole } else { all };
        match kept.torn {
            Torn::Nothing => whole,
            Torn::Prefix(_) | Torn::Sectors(_) => {
                if whole < all {
                    whole + 1
                } else {
                    whole
                }
            }
        }
    }

    /// Whether a crash may keep `kept` of this file, looked at alone: no more changes than it
    /// has, a torn part that is a part of the next change as that kind of change tears, and
    /// an undo only of what can be undone.
    pub fn fits(&self, kept: &Kept) -> bool {
        if kept.whole > self.changes.len() {
            return false;
        }
        if kept.undone && self.undo.is_none() {
            return false;
        }
        match &kept.torn {
            Torn::Nothing => true,
            Torn::Prefix(count) => {
                if kept.whole == self.changes.len() {
                    return false;
                }
                match &self.changes[kept.whole].change {
                    Change::Append(more) | Change::At(_, more) => *count <= more.len(),
                    Change::Sectors(_, _) | Change::Cut(_) => false,
                }
            }
            Torn::Sectors(landed) => {
                if kept.whole == self.changes.len() {
                    return false;
                }
                match &self.changes[kept.whole].change {
                    Change::Sectors(_, more) => landed.len() == sectors(more.len()),
                    Change::Append(_) | Change::At(_, _) | Change::Cut(_) => false,
                }
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
    /// kept whole, and the part of the next that landed, all of it durable and nothing else
    /// left.
    fn cut(&mut self, kept: &Kept) {
        self.sync(kept.whole);
        if !self.changes.is_empty() {
            let next = self.changes.remove(0);
            match &kept.torn {
                Torn::Nothing => {}
                Torn::Prefix(count) => match &next.change {
                    Change::Append(more) => extend(&mut self.durable, more, *count),
                    Change::At(at, more) => {
                        let to = if *count < more.len() { *count } else { more.len() };
                        write(&mut self.durable, *at, more, 0, to);
                    }
                    Change::Sectors(_, _) | Change::Cut(_) => {}
                },
                Torn::Sectors(landed) => match &next.change {
                    Change::Sectors(at, more) => land(&mut self.durable, *at, more, landed),
                    Change::Append(_) | Change::At(_, _) | Change::Cut(_) => {}
                },
            }
        }
        self.changes.clear();
        self.barriers.clear();
        self.undo = None;
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
        Disk { files: Vec::new_in(Global), epoch: 0 }
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

    /// The file named `name`, made empty where there is none: where it is.
    fn open(&mut self, name: u64) -> usize {
        let at = self.slot(name);
        if at == self.files.len() || self.files[at].name != name {
            self.files.insert(at, File::whole(name, Vec::new_in(Global)));
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
    /// in place of whatever the name held, and the drive is flushed.
    pub fn write(&mut self, file: u64, bytes: Vec<u8>) {
        self.put(File::whole(file, bytes));
        self.flush();
    }

    /// A whole-file write that waited for no sync completed: `file` reads as `bytes`, none
    /// of them durable, and a crash may put back what the name held.
    pub fn write_unsynced(&mut self, file: u64, bytes: Vec<u8>) {
        let was = match self.take(file) {
            Some(old) => Some(old.durable),
            None => None,
        };
        let mut new = File::whole(file, Vec::new_in(Global));
        new.undo = Some(Undo { was, from: None });
        let epoch = self.epoch;
        new.changes.push(Made { change: Change::Sectors(0, bytes), epoch });
        self.put(new);
    }

    /// A rename that waited for the drive completed: the file is under `to`, in place of
    /// whatever that name held, the name is durable (its changes are as durable as they
    /// were), and the drive is flushed. `false`, and nothing changed, where there is no file
    /// under `from`.
    pub fn rename(&mut self, from: u64, to: u64) -> bool {
        match self.take(from) {
            Some(mut file) => {
                file.name = to;
                file.undo = None;
                self.put(file);
                self.flush();
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
                file.undo = Some(Undo { was, from: Some(from) });
                self.put(file);
                true
            }
            None => false,
        }
    }

    /// A removal completed: no file is under the name. `false` where none was.
    pub fn remove(&mut self, file: u64) -> bool {
        self.take(file).is_some()
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

    /// Whether a crash may keep `cut`: one [`Kept`] for each file in the disk's order, each
    /// fitting its file ([`File::fits`]), and no change kept, whole or in part, that was made
    /// after a barrier completed unless every change that barrier covers is kept whole.
    pub fn allows(&self, cut: &[Kept]) -> bool {
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

    /// The disk after a crash that kept `cut`: each file is the bytes kept of it, all of
    /// them durable; then each undone write or rename is put back, in the order of the names
    /// they were under: the file leaves the name, goes back under the name it was renamed
    /// from unless a file is there, and what the name held before is back as far as it was
    /// durable. The barriers completed stay counted.
    pub fn crash(&mut self, cut: &[Kept]) {
        let mut undone: Vec<Undone> = Vec::new_in(Global);
        let mut i = 0;
        while i < self.files.len() && i < cut.len() {
            let undo = self.files[i].undo.take();
            if cut[i].undone
                && let Some(undo) = undo
            {
                undone.push(Undone { name: self.files[i].name, undo });
            }
            self.files[i].cut(&cut[i]);
            i += 1;
        }
        while !undone.is_empty() {
            let Undone { name, undo } = undone.remove(0);
            let file = self.take(name);
            if let Some(from) = undo.from
                && let Some(mut file) = file
                && self.find(from).is_none()
            {
                file.name = from;
                self.put(file);
            }
            if let Some(was) = undo.was {
                self.put(File::whole(name, was));
            }
        }
    }

    /// Whether the disk is within the bounds the theorems below speak of: at most
    /// [`MOST_CHANGES`] files, each within [`MOST_BYTES`] and [`MOST_CHANGES`].
    pub fn small(&self) -> bool {
        if self.files.len() > MOST_CHANGES {
            return false;
        }
        let mut i = 0;
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
/// once a sync of `file` that covers all its changes has completed, whatever cut a crash is
/// allowed to make, `file` is there after it and reads as it did before ([`kept`]): no change
/// of it is lost or torn, no write or rename of another file that is undone takes its name,
/// and nothing here overflows or panics. Its control is
/// `the_oracle_catches_a_file_that_lost_what_was_not_synced`.
#[blaze::theorem]
pub fn a_crash_keeps_a_synced_file(disk: Disk, file: u64, cut: Vec<Kept>) -> bool {
    let mut disk = disk;
    if !disk.wf() || !disk.small() {
        return true;
    }
    let Some(at) = disk.find(file) else { return true };
    let read = disk.files[at].contents();
    let all = disk.files[at].changes.len();
    disk.sync(file, all);
    if !disk.allows(&cut) {
        return true;
    }
    disk.crash(&cut);
    kept(&read, &disk.contents(file))
}

/// The invariant (proved: crates/blaze-core/lean/BlazeCore/World/Disk.lean): a crash cuts an
/// appended file to a prefix. From every well-formed disk
/// within the bounds, for a file whose changes not synced are all appends and that no crash
/// may undo, whatever cut a crash is allowed to make, the file is there after it, begins with
/// all its durable bytes and is a prefix of what it read as before ([`cut_to_a_prefix`]). So
/// a log that is only appended to loses at most its end, and never holds a byte that was not
/// written in that place. Its control is `the_oracle_catches_a_file_cut_in_its_middle`.
#[blaze::theorem]
pub fn a_crash_cuts_an_appended_file_to_a_prefix(disk: Disk, file: u64, cut: Vec<Kept>) -> bool {
    let mut disk = disk;
    if !disk.wf() || !disk.small() {
        return true;
    }
    let Some(at) = disk.find(file) else { return true };
    if !disk.files[at].only_appended() || disk.files[at].undo.is_some() {
        return true;
    }
    let read = disk.files[at].contents();
    let durable = copy(&disk.files[at].durable);
    if !disk.allows(&cut) {
        return true;
    }
    disk.crash(&cut);
    cut_to_a_prefix(&durable, &read, &disk.contents(file))
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
    cut: Vec<Kept>,
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
    ordered_first(disk.files[appended].reach(&cut[appended]), last, cut[ordered].whole, covers)
}

/// Whether a cut that leaves `reach` changes of the later file on the disk in whole or in
/// part, the append after the barrier being its change `last`, and keeps `whole` changes of
/// the ordered file whole, keeps the barrier's order: none of the append, or all `covers`
/// changes the barrier covered.
pub const fn ordered_first(reach: usize, last: usize, whole: usize, covers: usize) -> bool {
    reach <= last || whole >= covers
}

#[cfg(test)]
mod tests {
    use blaze_sim::{Broke, Config, Tape, check};

    use super::*;

    /// A disk a run of drawn completions leaves: three names, small changes, syncs and
    /// barriers that cover all or a part, the writes and renames that wait for no sync.
    fn disk(tape: &Tape) -> Disk {
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
            match tape.below(10, "the completion") {
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

    /// A cut drawn for `disk`, each file's share fitting it: whether the disk allows it is
    /// the barriers' to say. Calm keeps every change whole and undoes nothing.
    fn cut(tape: &Tape, disk: &Disk) -> Vec<Kept> {
        disk.files
            .iter()
            .map(|file| {
                let all = file.changes.len();
                let whole = all - tape.index(all + 1, "changes not kept");
                let torn = match file.changes.get(whole).map(|it| &it.change) {
                    Some(Change::Append(more) | Change::At(_, more)) if tape.coin("torn") => {
                        Torn::Prefix(tape.index(more.len() + 1, "bytes that landed"))
                    }
                    Some(Change::Sectors(_, more)) if tape.coin("torn") => {
                        Torn::Sectors((0..sectors(more.len())).map(|_| tape.coin("a sector landed")).collect())
                    }
                    _ => Torn::Nothing,
                };
                Kept { whole, torn, undone: file.undo.is_some() && tape.one_in(3, "undone") }
            })
            .collect()
    }

    fn holds(what: &str, held: bool, disk: &Disk, cut: &[Kept]) -> Result<(), Broke> {
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
                "an appended file and an allowed cut that loses some",
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
            }
            // clone: as above
            holds("a_crash_keeps_a_synced_file", a_crash_keeps_a_synced_file(made.clone(), name, drawn.clone()), &made, &drawn)?;

            // An appended file: the cut is of the disk as it is.
            let drawn = cut(tape, &made);
            let file = &made.files[at];
            if file.only_appended() && file.undo.is_none() && made.allows(&drawn) && drawn[at].whole < file.changes.len() {
                tape.reached("an appended file and an allowed cut that loses some");
            }
            // clone: as above
            holds(
                "a_crash_cuts_an_appended_file_to_a_prefix",
                a_crash_cuts_an_appended_file_to_a_prefix(made.clone(), name, drawn.clone()),
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
                && ordered.files[appended].reach(&drawn[appended]) == ordered.files[appended].changes.len()
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
        let none = [Kept { whole: 0, torn: Torn::Nothing, undone: false }];
        assert!(disk.allows(&none));
        disk.crash(&none);
        assert_eq!(disk.contents(1).as_deref(), Some(&b"synced"[..]));
        assert!(!kept(&read, &disk.contents(1)));
        assert!(!kept(&read, &None));
        assert!(kept(b"synced", &disk.contents(1)));
    }

    /// A file with a hole where a middle change was lost, a file missing its durable bytes
    /// and no file at all are not [`cut_to_a_prefix`]: a write at an offset, which the
    /// theorem leaves out, is torn to what the oracle refuses.
    #[test]
    fn the_oracle_catches_a_file_cut_in_its_middle() {
        let mut disk = Disk::new();
        disk.write(1, b"durable".to_vec());
        disk.append(1, b"-more".to_vec());
        assert!(disk.write_at(1, 0, b"DUR".to_vec()));
        let read = disk.contents(1).expect("there");
        // The append kept whole and a byte of the write at the start.
        let torn = [Kept { whole: 1, torn: Torn::Prefix(1), undone: false }];
        assert!(disk.allows(&torn));
        disk.crash(&torn);
        assert_eq!(disk.contents(1).as_deref(), Some(&b"Durable-more"[..]));
        assert!(!cut_to_a_prefix(b"durable", &read, &disk.contents(1)));
        assert!(!cut_to_a_prefix(b"durable", b"durable-more", &Some(b"dur".to_vec())));
        assert!(!cut_to_a_prefix(b"durable", b"durable-more", &None));
        assert!(cut_to_a_prefix(b"durable", b"durable-more", &Some(b"durable-m".to_vec())));
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
        let cut = [
            Kept { whole: 0, torn: Torn::Nothing, undone: false },
            Kept { whole: 1, torn: Torn::Nothing, undone: false },
        ];
        assert!(!ordered_first(1, 0, 0, 1));
        assert!(!appended(true).allows(&cut));
        assert!(appended(false).allows(&cut));
        assert!(ordered_first(0, 0, 0, 1) && ordered_first(1, 0, 1, 1));
    }
}
