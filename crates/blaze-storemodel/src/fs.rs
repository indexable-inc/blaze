//! A file system that remembers what a crash may keep.
//!
//! [`CrashFs`] runs the file effects a store asks for, one at a time and in the order the
//! explorer picks, against a disk that keeps three things apart: what the process sees (`live`),
//! what is on stable storage (`durable`), and what is in between (each file's unsynced data
//! changes, and the directory entries made or removed since their directory was last synced).
//! [`CrashFs::crash_states`] lists every disk a power cut at this moment may leave, under a
//! [`Persist`] model of the file system, and [`CrashFs::from_disk`] starts a fresh process on one.
//!
//! The rules follow `blaze_core::Effect`'s contract, which the native drivers keep:
//! - Append, Truncate and WriteAt are durable once a [`Effect::Sync`] of the file asked after
//!   them completes. A crash keeps any prefix of a file's unsynced changes, the last of them
//!   perhaps torn (cut at its first byte, its middle, or one short of its end): ALICE's
//!   "prefix with a partial last op" model (Pillai et al., OSDI 2014).
//! - A failed sync dooms what it covered: those changes stay in what the process reads but no
//!   later sync makes them durable (Linux after an `fsync` error, "fsyncgate"); a crash may keep
//!   or lose them.
//! - Write is atomic and durable on completion: a temporary file, synced, renamed over, and its
//!   directory synced, so every entry of that directory made before it is durable too.
//! - Rename is durable on completion (its directories are synced). Remove is not synced.
//! - A file that Append or Truncate makes is a new directory entry, durable only once its
//!   directory is synced, or, under [`Persist::Ordered`], once any sync commits the journal.
//! - ReadInto fills its registered span with what Read would return of the file as the process
//!   sees it. WriteFrom is WriteAt of the span's bytes as they are when it completes, unsynced
//!   until a Sync. A crash tears a write from memory by block, not to a prefix: its first block
//!   alone, its later half without its first, or all but its last (a system writes a span's
//!   blocks in any order).
//! - An effect in flight at the crash may or may not have happened: its change is one more
//!   unsynced change (a Write in flight is its new contents, synced, behind a rename not yet
//!   durable).

use std::collections::{BTreeMap, BTreeSet};

use blaze_core::{Completion, DIRECT_ALIGN, Done, Effect, FileName, IoError, Mapping, Meta, spans};
use blaze_ids::{ContentId, Hasher};

/// What a file system keeps of its directory entries across a crash.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Persist {
    /// Entries are journaled in order (ext4 `data=ordered`, XFS, APFS): a crash keeps a prefix
    /// of the entry changes not yet durable, and a sync of any file commits the journal, making
    /// every entry change before it durable.
    Ordered,
    /// The letter of POSIX: an entry is durable only once its directory is synced, and a crash
    /// keeps any subset of the entry changes not yet durable.
    Posix,
}

/// A disk as a crash left it: every file's bytes, by name.
pub type Disk = BTreeMap<String, Vec<u8>>;

/// One disk a crash may leave, and how.
#[derive(Clone, Debug)]
pub struct CrashState {
    pub disk: Disk,
    /// Which pending changes it kept, for the report.
    pub how: String,
}

/// More crash states than the bound: the explorer's run is incomplete, never silently cut.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TooManyStates {
    pub most: usize,
    /// The pending changes that made so many.
    pub pending: String,
}

impl std::fmt::Display for TooManyStates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "more than {} crash states of {}: raise STOREMODEL_CRASH_STATES or shrink the scenario",
            self.most, self.pending
        )
    }
}

type Ino = usize;

#[derive(Clone, Debug)]
enum Change {
    Append(Vec<u8>),
    Cut(u64),
    At(u64, Vec<u8>),
    /// A write from memory ([`Effect::WriteFrom`]): as `At`, torn by block.
    Blocks(u64, Vec<u8>),
}

impl Change {
    fn apply(&self, bytes: &mut Vec<u8>) {
        match self {
            Change::Append(more) => bytes.extend_from_slice(more),
            Change::Cut(len) => bytes.resize(*len as usize, 0),
            Change::At(at, more) | Change::Blocks(at, more) => {
                let at = *at as usize;
                if bytes.len() < at + more.len() {
                    bytes.resize(at + more.len(), 0);
                }
                bytes[at..at + more.len()].copy_from_slice(more);
            }
        }
    }

    /// The change cut short at `cut` of its bytes, as a crash in its middle leaves it.
    fn torn(&self, cut: usize) -> Option<Change> {
        match self {
            Change::Append(more) => Some(Change::Append(more[..cut].to_vec())),
            Change::At(at, more) => Some(Change::At(*at, more[..cut].to_vec())),
            // By block, and not only to a prefix. The three cuts a crash state tries (its
            // first byte, its middle, one short of its end) stand for: its first block alone,
            // its later half without what comes before, all but its last block.
            Change::Blocks(at, more) => {
                let block = DIRECT_ALIGN as usize;
                let blocks = more.len().div_ceil(block);
                if blocks < 2 {
                    return None;
                }
                let (from, to) = match cut {
                    1 => (0, block),
                    cut if cut + 1 == more.len() => (0, (blocks - 1) * block),
                    _ => ((blocks / 2) * block, more.len()),
                };
                Some(Change::At(*at + from as u64, more[from..to].to_vec()))
            }
            Change::Cut(_) => None,
        }
    }

    fn len(&self) -> usize {
        match self {
            Change::Append(more) | Change::At(_, more) | Change::Blocks(_, more) => more.len(),
            Change::Cut(_) => 0,
        }
    }

    fn describe(&self) -> String {
        match self {
            Change::Append(more) => format!("append of {} bytes", more.len()),
            Change::Cut(len) => format!("truncate to {len}"),
            Change::At(at, more) => format!("write of {} bytes at {at}", more.len()),
            Change::Blocks(at, more) => {
                format!("write from memory of {} bytes at {at}", more.len())
            }
        }
    }
}

#[derive(Clone, Debug)]
struct Unsynced {
    change: Change,
    /// A sync that covered it failed: no later sync makes it durable.
    doomed: bool,
}

#[derive(Clone, Debug, Default)]
struct Inode {
    live: Vec<u8>,
    durable: Vec<u8>,
    unsynced: Vec<Unsynced>,
}

#[derive(Clone, Debug)]
enum Entry {
    Link { name: String, ino: Ino },
    Unlink { name: String },
}

impl Entry {
    fn name(&self) -> &str {
        match self {
            Entry::Link { name, .. } | Entry::Unlink { name } => name,
        }
    }

    fn apply(&self, names: &mut BTreeMap<String, Ino>) {
        match self {
            Entry::Link { name, ino } => {
                names.insert(name.clone(), *ino);
            }
            Entry::Unlink { name } => {
                names.remove(name);
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            Entry::Link { name, .. } => format!("entry {name}"),
            Entry::Unlink { name } => format!("removal of {name}"),
        }
    }
}

struct Flying {
    effect: Effect,
    /// Of a sync: how many of its file's unsynced changes it covers.
    covers: usize,
}

/// The file system: see the module's docs.
pub struct CrashFs {
    persist: Persist,
    inodes: Vec<Inode>,
    live: BTreeMap<String, Ino>,
    durable: BTreeMap<String, Ino>,
    /// Entry changes not yet durable, in the order made.
    pending: Vec<Entry>,
    flying: Vec<Flying>,
}

/// The directory of `name`: everything before its last `/`.
fn dir_of(name: &str) -> &str {
    name.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// What an effect is, for a schedule's step and a report.
pub fn describe(effect: &Effect) -> String {
    match effect {
        Effect::Private { dir, .. } => format!("check {dir} is private"),
        Effect::ReadAll { file, .. } => format!("read {file}"),
        Effect::Read { file, at, len, .. } => format!("read {len} bytes of {file} at {at}"),
        Effect::Append { file, bytes, .. } => format!("append {} bytes to {file}", bytes.len()),
        Effect::Sync { file, .. } => format!("sync {file}"),
        Effect::Truncate { file, len, .. } => format!("truncate {file} to {len}"),
        Effect::Write { file, bytes, .. } => format!("write {file} ({} bytes)", bytes.len()),
        Effect::WriteAt { file, at, bytes, .. } => {
            format!("write {} bytes of {file} at {at}", bytes.len())
        }
        Effect::ReadInto { file, at, into, .. } => {
            format!("read {} bytes of {file} at {at} into memory", into.len)
        }
        Effect::WriteFrom { file, at, from, .. } => {
            format!("write {} bytes of {file} at {at} from memory", from.len)
        }
        Effect::Rename { from, to, .. } => format!("rename {from} to {to}"),
        Effect::Remove { file, .. } => format!("remove {file}"),
        Effect::Map { file, .. } => format!("map {file}"),
        Effect::Stat { file, .. } => format!("stat {file}"),
        other => format!("{other:?}"),
    }
}

impl CrashFs {
    /// An empty disk.
    pub fn new(persist: Persist) -> CrashFs {
        CrashFs {
            persist,
            inodes: Vec::new(),
            live: BTreeMap::new(),
            durable: BTreeMap::new(),
            pending: Vec::new(),
            flying: Vec::new(),
        }
    }

    /// A fresh process on `disk`, everything on it durable.
    pub fn from_disk(persist: Persist, disk: &Disk) -> CrashFs {
        let mut fs = CrashFs::new(persist);
        for (name, bytes) in disk {
            let ino = fs.inodes.len();
            fs.inodes.push(Inode {
                live: bytes.clone(),
                durable: bytes.clone(),
                unsynced: Vec::new(),
            });
            fs.live.insert(name.clone(), ino);
            fs.durable.insert(name.clone(), ino);
        }
        fs
    }

    /// What a clean shutdown leaves: everything durable as it is now (no effect in flight).
    pub fn synced_disk(&self) -> Disk {
        self.durable
            .iter()
            .map(|(name, ino)| (name.clone(), self.inodes[*ino].durable.clone()))
            .collect()
    }

    /// The effects asked for, in flight until [`CrashFs::complete`] or [`CrashFs::fail`] ends
    /// each.
    pub fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            let covers = match &effect {
                Effect::Sync { file, .. } => {
                    self.live.get(file.as_str()).map_or(0, |ino| self.inodes[*ino].unsynced.len())
                }
                _ => 0,
            };
            self.flying.push(Flying { effect, covers });
        }
    }

    /// The effects in flight, in the order asked, as [`describe`] names them.
    pub fn in_flight(&self) -> Vec<String> {
        self.flying.iter().map(|it| describe(&it.effect)).collect()
    }

    pub fn is_idle(&self) -> bool {
        self.flying.is_empty()
    }

    /// Ends the effect in flight at `at` as done, applying it.
    pub fn complete(&mut self, at: usize) -> Completion {
        let Flying { effect, covers } = self.flying.remove(at);
        let tag = effect.tag();
        let done = match effect {
            Effect::Private { .. } => Done::Private(None),
            Effect::ReadAll { file, .. } => Done::Read(self.contents(&file)),
            Effect::Read { file, at, len, .. } => {
                let bytes = self.contents(&file);
                let at = (at as usize).min(bytes.len());
                Done::Read(bytes[at..at.saturating_add(len).min(bytes.len())].to_vec())
            }
            Effect::ReadInto { file, at, into, cache, .. } => {
                let bytes = self.contents(&file);
                let from = (at as usize).min(bytes.len());
                match spans::read_into(&into, &file, at, cache, &bytes[from..]) {
                    Ok(len) => Done::ReadInto { len },
                    Err(refused) => Done::Failed(refused),
                }
            }
            Effect::WriteFrom { file, at, from, cache, .. } => {
                match (self.live.contains_key(file.as_str()), spans::written_from(&from, &file, at, cache)) {
                    (true, Ok(bytes)) => {
                        self.change(&file, Change::Blocks(at, bytes));
                        Done::Written
                    }
                    (true, Err(refused)) => Done::Failed(refused),
                    (false, _) => Done::Failed(IoError(format!("{file}: no such file"))),
                }
            }
            Effect::Append { file, bytes, .. } => {
                self.change(&file, Change::Append(bytes));
                Done::Appended
            }
            Effect::Truncate { file, len, .. } => {
                self.change(&file, Change::Cut(len));
                Done::Truncated
            }
            Effect::WriteAt { file, at, bytes, .. } => {
                match self.live.contains_key(file.as_str()) {
                    true => {
                        self.change(&file, Change::At(at, bytes));
                        Done::Written
                    }
                    false => Done::Failed(IoError(format!("{file}: no such file"))),
                }
            }
            Effect::Sync { file, .. } => {
                if let Some(&ino) = self.live.get(file.as_str()) {
                    let inode = &mut self.inodes[ino];
                    let covered: Vec<Unsynced> =
                        inode.unsynced.drain(..covers.min(inode.unsynced.len())).collect();
                    for it in covered.iter().filter(|it| !it.doomed) {
                        it.change.apply(&mut inode.durable);
                    }
                }
                if self.persist == Persist::Ordered {
                    self.commit_entries(None);
                }
                Done::Synced
            }
            Effect::Write { file, bytes, .. } => {
                let ino = self.inodes.len();
                self.inodes.push(Inode {
                    live: bytes.clone(),
                    durable: bytes,
                    unsynced: Vec::new(),
                });
                self.link(file.as_str(), ino);
                self.commit_entries(Some(dir_of(file.as_str())));
                Done::Written
            }
            Effect::Rename { from, to, .. } => match self.live.get(from.as_str()).copied() {
                Some(ino) => {
                    self.unlink(from.as_str());
                    self.link(to.as_str(), ino);
                    self.commit_entries(Some(dir_of(from.as_str())));
                    self.commit_entries(Some(dir_of(to.as_str())));
                    Done::Renamed
                }
                None => Done::Failed(IoError(format!("{from}: no such file"))),
            },
            Effect::Remove { file, .. } => {
                if self.live.contains_key(file.as_str()) {
                    self.unlink(file.as_str());
                }
                Done::Removed
            }
            Effect::Map { file, .. } => Done::Mapped(Mapping::copy(&self.contents(&file))),
            Effect::Stat { file, .. } => Done::Stat(match self.live.get(file.as_str()) {
                Some(ino) => Some(Meta {
                    len: self.inodes[*ino].live.len() as u64,
                    dir: false,
                    modified_ns: 0,
                }),
                None if self.live.keys().any(|it| it.starts_with(&format!("{file}/"))) => {
                    Some(Meta { len: 0, dir: true, modified_ns: 0 })
                }
                None => None,
            }),
            other => panic!(
                "the crash model has no rule for {other:?}: add one to blaze-storemodel's fs.rs"
            ),
        };
        Completion { tag, done }
    }

    /// Ends the effect in flight at `at` as failed. A failed append or write leaves its first
    /// half in the file, unsynced; a failed sync dooms what it covered; anything else changed
    /// nothing.
    pub fn fail(&mut self, at: usize) -> Completion {
        let Flying { effect, covers } = self.flying.remove(at);
        let tag = effect.tag();
        match effect {
            Effect::Append { file, bytes, .. } if !bytes.is_empty() => {
                self.change(&file, Change::Append(bytes[..bytes.len() / 2].to_vec()));
            }
            Effect::WriteAt { file, at, bytes, .. } if self.live.contains_key(file.as_str()) => {
                self.change(&file, Change::At(at, bytes[..bytes.len() / 2].to_vec()));
            }
            // A failed write from memory may have written any of its blocks: here, the whole
            // blocks of its first half, unsynced.
            Effect::WriteFrom { file, at, from, cache, .. }
                if self.live.contains_key(file.as_str()) =>
            {
                if let Ok(bytes) = spans::written_from(&from, &file, at, cache) {
                    let block = DIRECT_ALIGN as usize;
                    let half = bytes.len() / 2 / block * block;
                    if half > 0 {
                        self.change(&file, Change::Blocks(at, bytes[..half].to_vec()));
                    }
                }
            }
            Effect::Sync { file, .. } => {
                if let Some(&ino) = self.live.get(file.as_str()) {
                    for it in self.inodes[ino].unsynced.iter_mut().take(covers) {
                        it.doomed = true;
                    }
                }
            }
            _ => {}
        }
        Completion { tag, done: Done::Failed(IoError("injected by the model".into())) }
    }

    fn contents(&self, file: &FileName) -> Vec<u8> {
        self.live.get(file.as_str()).map_or_else(Vec::new, |ino| self.inodes[*ino].live.clone())
    }

    fn change(&mut self, file: &FileName, change: Change) {
        let ino = match self.live.get(file.as_str()) {
            Some(ino) => *ino,
            None => {
                let ino = self.inodes.len();
                self.inodes.push(Inode::default());
                self.link(file.as_str(), ino);
                ino
            }
        };
        let inode = &mut self.inodes[ino];
        change.apply(&mut inode.live);
        inode.unsynced.push(Unsynced { change, doomed: false });
    }

    fn link(&mut self, name: &str, ino: Ino) {
        self.live.insert(name.to_owned(), ino);
        self.pending.push(Entry::Link { name: name.to_owned(), ino });
    }

    fn unlink(&mut self, name: &str) {
        self.live.remove(name);
        self.pending.push(Entry::Unlink { name: name.to_owned() });
    }

    /// Makes durable the pending entry changes of `dir` (a directory sync), or every one
    /// (a journal commit).
    fn commit_entries(&mut self, dir: Option<&str>) {
        // A journaling file system commits every entry change before the sync, in any directory.
        let dir = if self.persist == Persist::Ordered { None } else { dir };
        let pending = std::mem::take(&mut self.pending);
        for entry in pending {
            match dir {
                Some(dir) if dir_of(entry.name()) != dir => self.pending.push(entry),
                _ => entry.apply(&mut self.durable),
            }
        }
    }

    /// Every disk a crash now may leave, each once, in a fixed order; more than `most` is an
    /// error naming what was pending.
    pub fn crash_states(&self, most: usize) -> Result<Vec<CrashState>, TooManyStates> {
        // What may have happened: the pending entries and unsynced changes, then each effect
        // in flight as if it completed with no sync.
        let mut inodes = self.inodes.clone();
        let mut live = self.live.clone();
        let mut entries = self.pending.clone();
        for flying in &self.flying {
            match &flying.effect {
                Effect::Append { file, bytes, .. } => may_change(
                    &mut inodes,
                    &mut live,
                    &mut entries,
                    file,
                    Change::Append(bytes.clone()),
                ),
                Effect::Truncate { file, len, .. } => {
                    may_change(&mut inodes, &mut live, &mut entries, file, Change::Cut(*len))
                }
                Effect::WriteAt { file, at, bytes, .. } if live.contains_key(file.as_str()) => {
                    may_change(
                        &mut inodes,
                        &mut live,
                        &mut entries,
                        file,
                        Change::At(*at, bytes.clone()),
                    )
                }
                // A span being written as the crash comes: the system may have written any of
                // its blocks, from the memory as it is now. One more unsynced change, torn by
                // block as `Change::Blocks` is.
                Effect::WriteFrom { file, at, from, cache, .. }
                    if live.contains_key(file.as_str()) =>
                {
                    if let Ok(bytes) = spans::written_from(from, file, *at, *cache) {
                        may_change(
                            &mut inodes,
                            &mut live,
                            &mut entries,
                            file,
                            Change::Blocks(*at, bytes),
                        );
                    }
                }
                Effect::Write { file, bytes, .. } => {
                    inodes.push(Inode {
                        live: bytes.clone(),
                        durable: bytes.clone(),
                        unsynced: Vec::new(),
                    });
                    let ino = inodes.len() - 1;
                    live.insert(file.as_str().to_owned(), ino);
                    entries.push(Entry::Link { name: file.as_str().to_owned(), ino });
                }
                Effect::Rename { from, to, .. } => {
                    if let Some(ino) = live.remove(from.as_str()) {
                        live.insert(to.as_str().to_owned(), ino);
                        entries.push(Entry::Unlink { name: from.as_str().to_owned() });
                        entries.push(Entry::Link { name: to.as_str().to_owned(), ino });
                    }
                }
                Effect::Remove { file, .. } => {
                    if live.remove(file.as_str()).is_some() {
                        entries.push(Entry::Unlink { name: file.as_str().to_owned() });
                    }
                }
                _ => {}
            }
        }
        let too_many = || TooManyStates { most, pending: self.pending_text(&inodes, &entries) };
        // Which entry changes a crash keeps.
        let kept_entries: Vec<Vec<usize>> = match self.persist {
            Persist::Ordered => (0..=entries.len()).map(|n| (0..n).collect()).collect(),
            Persist::Posix => {
                if entries.len() >= usize::BITS as usize - 1 || (1usize << entries.len()) > most {
                    return Err(too_many());
                }
                (0..1usize << entries.len())
                    .map(|set| (0..entries.len()).filter(|n| set & (1 << n) != 0).collect())
                    .collect()
            }
        };
        let mut states: BTreeMap<ContentId, CrashState> = BTreeMap::new();
        for kept in kept_entries {
            let mut names = self.durable.clone();
            for &n in &kept {
                entries[n].apply(&mut names);
            }
            let mut how = String::new();
            if !entries.is_empty() {
                let lost: Vec<String> = (0..entries.len())
                    .filter(|n| !kept.contains(n))
                    .map(|n| entries[n].describe())
                    .collect();
                how.push_str(&format!(
                    "kept {} of {} pending directory changes",
                    kept.len(),
                    entries.len()
                ));
                if !lost.is_empty() {
                    how.push_str(&format!(" (lost: {})", lost.join(", ")));
                }
            }
            // Each named file's choices: a prefix of its unsynced changes, the last perhaps torn.
            let mut disks: Vec<(Disk, String)> = vec![(Disk::new(), how)];
            let named: BTreeSet<(String, Ino)> =
                names.iter().map(|(name, ino)| (name.clone(), *ino)).collect();
            for (name, ino) in named {
                let inode = &inodes[ino];
                let mut choices: Vec<(Vec<u8>, String)> = Vec::new();
                for n in 0..=inode.unsynced.len() {
                    let mut bytes = inode.durable.clone();
                    for it in &inode.unsynced[..n] {
                        it.change.apply(&mut bytes);
                    }
                    let text = match (inode.unsynced.len(), n) {
                        (0, _) => String::new(),
                        (all, n) => format!("{name}: kept {n} of {all} unsynced changes"),
                    };
                    choices.push((bytes, text));
                    if let Some(last) = n.checked_sub(1).map(|it| &inode.unsynced[it].change) {
                        let len = last.len();
                        let cuts: BTreeSet<usize> = [1, len / 2, len.saturating_sub(1)]
                            .into_iter()
                            .filter(|it| *it > 0 && *it < len)
                            .collect();
                        for cut in cuts {
                            let Some(torn) = last.torn(cut) else { continue };
                            let mut bytes = inode.durable.clone();
                            for it in &inode.unsynced[..n - 1] {
                                it.change.apply(&mut bytes);
                            }
                            torn.apply(&mut bytes);
                            choices.push((bytes, format!("{name}: kept {n} of {} unsynced changes, the last ({}) torn at {cut}", inode.unsynced.len(), last.describe())));
                        }
                    }
                }
                let mut next = Vec::with_capacity(disks.len() * choices.len());
                for (disk, how) in &disks {
                    for (bytes, text) in &choices {
                        let mut disk = disk.clone();
                        disk.insert(name.clone(), bytes.clone());
                        let how = match (how.is_empty(), text.is_empty()) {
                            (_, true) => how.clone(),
                            (true, false) => text.clone(),
                            (false, false) => format!("{how}; {text}"),
                        };
                        next.push((disk, how));
                    }
                }
                if next.len() > most {
                    return Err(too_many());
                }
                disks = next;
            }
            for (disk, how) in disks {
                states.entry(digest(&disk)).or_insert(CrashState { disk, how });
            }
            if states.len() > most {
                return Err(too_many());
            }
        }
        Ok(states.into_values().collect())
    }

    fn pending_text(&self, inodes: &[Inode], entries: &[Entry]) -> String {
        let mut parts: Vec<String> = entries.iter().map(Entry::describe).collect();
        for (name, ino) in &self.live {
            for it in &inodes[*ino].unsynced {
                parts.push(format!("{name}: {}", it.change.describe()));
            }
        }
        parts.join(", ")
    }
}

/// One more change an effect in flight may have made, for [`CrashFs::crash_states`].
fn may_change(
    inodes: &mut Vec<Inode>,
    live: &mut BTreeMap<String, Ino>,
    entries: &mut Vec<Entry>,
    file: &FileName,
    change: Change,
) {
    let ino = match live.get(file.as_str()) {
        Some(ino) => *ino,
        None => {
            inodes.push(Inode::default());
            let ino = inodes.len() - 1;
            live.insert(file.as_str().to_owned(), ino);
            entries.push(Entry::Link { name: file.as_str().to_owned(), ino });
            ino
        }
    };
    change.apply(&mut inodes[ino].live);
    inodes[ino].unsynced.push(Unsynced { change, doomed: false });
}

/// The content ID of a disk: its names and bytes in order.
pub fn digest(disk: &Disk) -> ContentId {
    let mut hasher = Hasher::new("blaze storemodel disk 1");
    for (name, bytes) in disk {
        hasher.update(&(name.len() as u64).to_le_bytes()).update(name.as_bytes());
        hasher.update(&(bytes.len() as u64).to_le_bytes()).update(bytes);
    }
    hasher.finish()
}
