//! A file system that remembers what a crash may keep.
//!
//! [`CrashFs`] runs the file effects a store asks for, one at a time and in the order the
//! explorer picks, against a disk that keeps three things apart: what the process sees (`live`),
//! what is on stable storage (`durable`), and what is in between (each file's unsynced data
//! changes, and the directory entries made or removed since their directory was last synced).
//! [`CrashFs::crash_states`] lists every disk a power cut at this moment may leave, under a
//! [`Persist`] model of the file system's names and a [`Clones`] model of its blocks, and
//! [`CrashFs::from_disk`] starts a fresh process on one.
//!
//! The rules follow `blaze_core::Effect`'s contract, which the native drivers keep:
//! - Append, Truncate and WriteAt are durable once a [`Effect::Sync`] of the file asked after
//!   them completes. A crash keeps any prefix of a file's unsynced changes, the last of them
//!   perhaps torn (cut at its first byte, its middle, or one short of its end): ALICE's
//!   "prefix with a partial last op" model (Pillai et al., OSDI 2014).
//! - A failed sync dooms what it covered: those changes stay in what the process reads but no
//!   later sync makes them durable (Linux after an `fsync` error, "fsyncgate"); a crash may keep
//!   or lose them. A failed barrier dooms what it covered the same way.
//! - Write is atomic and durable on completion: a temporary file, synced, renamed over, and its
//!   directory synced, so every entry of that directory made before it is durable too.
//! - WriteUnsynced is Write waiting for no sync: a new file under the name, whose entry and
//!   bytes are both pending. A crash leaves the name as it was (the old file, or none), or the
//!   new file holding nothing, all, or its bytes torn by block. A later sync of the file makes
//!   its bytes durable, and its entry is durable as any entry is.
//! - Rename is durable on completion (its directories are synced); RenameUnsynced is the same
//!   name change left pending. Either is one entry change: a crash leaves the file under its
//!   old name or its new one, never under neither. Remove is not synced.
//! - A file that Append, Truncate, Sync or Barrier makes is a new directory entry, durable only
//!   once its directory is synced ([`Effect::SyncDir`], or a Write or Rename in it), or, under
//!   [`Persist::Ordered`], once any sync commits the journal.
//! - Barrier orders and makes nothing durable: the changes of its file made before it was
//!   asked (and, under [`Persist::Ordered`], every entry change made before it completed) are
//!   on the device before anything asked after it completes, so a crash that keeps any of
//!   that, of any file, keeps all of these whole; alone, a crash may still lose them. They are
//!   durable once a sync that waits for the drive (Sync, SyncDir, Write, Rename) asked after
//!   the barrier completed has completed, or once anything asked after it is durable.
//! - Clone makes, where the file system shares blocks ([`Clones::Made`]), a new file holding
//!   its source's bytes as the process sees them, whole (the source's unwritten data is written
//!   out before its blocks are shared), under an entry that is pending as any new entry is: a
//!   crash leaves no clone or a whole one. Where it does not ([`Clones::Refused`]: ext4, tmpfs)
//!   every clone fails saying so and nothing is made. A clone onto a name that is there fails.
//! - WriteBack, Prewarm and Seal change nothing a crash can tell (data begun to be written, a
//!   hint to the page cache, a mode, which the model does not keep); each fails for a file that
//!   is not there.
//! - ReadInto fills its registered span with what Read would return of the file as the process
//!   sees it. WriteFrom is WriteAt of the span's bytes as they are when it completes, unsynced
//!   until a Sync. A crash tears a write from memory by block, not to a prefix: its first block
//!   alone, its later half without its first, or all but its last (a system writes a span's
//!   blocks in any order).
//! - An effect in flight at the crash may or may not have happened: its change is one more
//!   pending change (a Write in flight is its new contents, synced, behind a rename not yet
//!   durable), and its sync or barrier promises nothing.
//!
//! Every variant of [`Effect`] is named once, in [`Files::change`], with no arm for the rest:
//! an effect added to `blaze_core` does not compile here until it has a rule, or is said to have
//! none.

use std::collections::{BTreeMap, BTreeSet};

use blaze_core::{
    Completion, DIRECT_ALIGN, Done, Effect, FileName, IoError, Mapping, Meta, S_IFDIR, S_IFREG,
    Stamp, spans,
};
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

/// Whether a file system shares blocks between files: what [`Effect::Clone`] ends in, which a
/// store's open tries once to decide how it places outputs (`blaze_cas::PlaceBy`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Clones {
    /// It does (APFS, XFS and btrfs with reflinks, ReFS): a clone is made.
    Made,
    /// It does not (ext4, tmpfs): every clone fails, saying the file system cannot share
    /// blocks, and nothing copies instead.
    Refused,
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
    /// A write from memory ([`Effect::WriteFrom`]) or of a whole file that waits for no sync
    /// ([`Effect::WriteUnsynced`]): as `At`, torn by block.
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
                format!("write by block of {} bytes at {at}", more.len())
            }
        }
    }
}

#[derive(Clone, Debug)]
struct Unsynced {
    change: Change,
    /// A sync or a barrier that covered it failed: no later sync makes it durable.
    doomed: bool,
    /// The epoch its effect was asked in ([`CrashFs::epoch`]).
    asked: u64,
}

#[derive(Clone, Debug, Default)]
struct Inode {
    live: Vec<u8>,
    durable: Vec<u8>,
    unsynced: Vec<Unsynced>,
    /// How many changes its bytes took: its modification time, as a disk with no clock counts
    /// it (`blaze_core::Meta::modified_ns`).
    changes: u64,
}

#[derive(Clone, Debug)]
enum Entry {
    Link { name: String, ino: Ino },
    Unlink { name: String },
    /// A rename: one change of both names, so no crash keeps half of it.
    Move { from: String, to: String, ino: Ino },
}

impl Entry {
    /// Whether a sync of the directory `dir` makes it durable.
    fn within(&self, dir: &str) -> bool {
        match self {
            Entry::Link { name, .. } | Entry::Unlink { name } => dir_of(name) == dir,
            Entry::Move { from, to, .. } => dir_of(from) == dir || dir_of(to) == dir,
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
            Entry::Move { from, to, ino } => {
                names.remove(from);
                names.insert(to.clone(), *ino);
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            Entry::Link { name, .. } => format!("entry {name}"),
            Entry::Unlink { name } => format!("removal of {name}"),
            Entry::Move { from, to, .. } => format!("rename of {from} to {to}"),
        }
    }
}

/// An entry change not yet durable.
#[derive(Clone, Debug)]
struct Pending {
    entry: Entry,
    /// The epoch its effect was asked in ([`CrashFs::epoch`]).
    asked: u64,
}

/// A barrier that completed ([`Effect::Barrier`]) and that nothing has made durable yet: a
/// crash that keeps anything asked in a later epoch keeps all it covers, whole.
#[derive(Clone, Debug)]
struct Barrier {
    ino: Ino,
    /// How many of its file's unsynced changes, from the first, it covers.
    covers: usize,
    /// How many of the pending entry changes, from the first, it orders: under
    /// [`Persist::Ordered`] those made before it completed (the journal goes out in order, as
    /// a sync commits it); under [`Persist::Posix`] none.
    entries: usize,
    /// The epoch it completed in: what is asked from the next on is ordered after it.
    epoch: u64,
}

struct Flying {
    effect: Effect,
    /// Of a sync or a barrier: how many of its file's unsynced changes it covers.
    covers: usize,
    /// The epoch it was asked in.
    asked: u64,
}

/// What the process sees, and what of it is pending: the part of the file system an effect in
/// flight at a crash may have changed, so [`CrashFs::crash_states`] runs each on a copy.
#[derive(Clone)]
struct Files {
    clones: Clones,
    inodes: Vec<Inode>,
    live: BTreeMap<String, Ino>,
    /// Entry changes not yet durable, in the order made.
    pending: Vec<Pending>,
    /// How many entries were made, removed or renamed in each directory: its modification
    /// time, counted as a file's is.
    dirs: BTreeMap<String, u64>,
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
        Effect::Barrier { file, .. } => format!("barrier {file}"),
        Effect::WriteBack { file, .. } => format!("write back {file}"),
        Effect::SyncDir { dir, .. } => format!("sync the names of {dir}"),
        Effect::Truncate { file, len, .. } => format!("truncate {file} to {len}"),
        Effect::Write { file, bytes, .. } => format!("write {file} ({} bytes)", bytes.len()),
        Effect::WriteUnsynced { file, bytes, .. } => {
            format!("write {file} ({} bytes), unsynced", bytes.len())
        }
        Effect::WriteAt { file, at, bytes, .. } => {
            format!("write {} bytes of {file} at {at}", bytes.len())
        }
        Effect::ReadInto { file, at, into, .. } => {
            format!("read {} bytes of {file} at {at} into memory", into.len)
        }
        Effect::ReadMany { parts, .. } => {
            let parts: Vec<String> = parts
                .iter()
                .map(|it| format!("{} bytes of {} at {}", it.into.len, it.file, it.at))
                .collect();
            format!("read into memory, in one batch: {}", parts.join(", "))
        }
        Effect::WriteFrom { file, at, from, .. } => {
            format!("write {} bytes of {file} at {at} from memory", from.len)
        }
        Effect::Rename { from, to, .. } => format!("rename {from} to {to}"),
        Effect::RenameUnsynced { from, to, .. } => format!("rename {from} to {to}, unsynced"),
        Effect::Clone { from, to, .. } => format!("clone {from} to {to}"),
        Effect::CopyData { from, to, .. } => format!("copy the data of {from} to {to}"),
        Effect::Prewarm { file, .. } => format!("prewarm {file}"),
        Effect::Seal { file, .. } => format!("seal {file}"),
        Effect::Remove { file, .. } => format!("remove {file}"),
        Effect::Map { file, extent: blaze_core::Extent::Whole, .. } => format!("map {file}"),
        Effect::Map { file, extent: blaze_core::Extent::Range { at, len }, .. } => {
            format!("map {len} bytes of {file} at {at}")
        }
        Effect::Stat { file, .. } => format!("stat {file}"),
        Effect::Lstat { file, .. } => format!("lstat {file}"),
        Effect::List { dir, .. } => format!("list {dir}"),
        Effect::Entries { dir, .. } => format!("entries of {dir}"),
        other => format!("{other:?}"),
    }
}

/// One read into memory of `into.len` bytes at `at` of `file`, as the model reads it: what an
/// [`Effect::ReadInto`] ends with, and each part of an [`Effect::ReadMany`].
fn read_part(
    files: &Files,
    file: &FileName,
    at: u64,
    into: &blaze_core::Span,
    cache: blaze_core::Cache,
) -> Result<u32, IoError> {
    let bytes = files.contents(file);
    let from = (at as usize).min(bytes.len());
    spans::read_into(into, file, at, cache, &bytes[from..])
}

fn missing(file: &FileName) -> Done {
    Done::Failed(IoError(format!("{file}: no such file")))
}

impl Files {
    fn new(clones: Clones) -> Files {
        Files {
            clones,
            inodes: Vec::new(),
            live: BTreeMap::new(),
            pending: Vec::new(),
            dirs: BTreeMap::new(),
        }
    }

    fn ino(&self, file: &FileName) -> Option<Ino> {
        self.live.get(file.as_str()).copied()
    }

    fn contents(&self, file: &FileName) -> Vec<u8> {
        self.ino(file).map_or_else(Vec::new, |ino| self.inodes[ino].live.clone())
    }

    fn inode(&mut self, inode: Inode) -> Ino {
        self.inodes.push(inode);
        self.inodes.len() - 1
    }

    fn touched(&mut self, name: &str) {
        *self.dirs.entry(dir_of(name).to_owned()).or_insert(0) += 1;
    }

    fn link(&mut self, name: &str, ino: Ino, asked: u64) {
        self.live.insert(name.to_owned(), ino);
        self.touched(name);
        self.pending.push(Pending { entry: Entry::Link { name: name.to_owned(), ino }, asked });
    }

    fn unlink(&mut self, name: &str, asked: u64) {
        self.live.remove(name);
        self.touched(name);
        self.pending.push(Pending { entry: Entry::Unlink { name: name.to_owned() }, asked });
    }

    fn rename(&mut self, from: &str, to: &str, ino: Ino, asked: u64) {
        self.live.remove(from);
        self.live.insert(to.to_owned(), ino);
        self.touched(from);
        self.touched(to);
        let entry = Entry::Move { from: from.to_owned(), to: to.to_owned(), ino };
        self.pending.push(Pending { entry, asked });
    }

    /// The file's inode, made empty under a pending entry where the name has none.
    fn made(&mut self, file: &FileName, asked: u64) -> Ino {
        match self.ino(file) {
            Some(ino) => ino,
            None => {
                let ino = self.inode(Inode::default());
                self.link(file.as_str(), ino, asked);
                ino
            }
        }
    }

    /// One more unsynced change of the file, which is made if missing.
    fn edit(&mut self, file: &FileName, change: Change, asked: u64) {
        let ino = self.made(file, asked);
        let inode = &mut self.inodes[ino];
        change.apply(&mut inode.live);
        inode.changes += 1;
        inode.unsynced.push(Unsynced { change, doomed: false, asked });
    }

    /// Whether any file is under `dir`: the model keeps no directory of its own.
    fn is_dir(&self, dir: &str) -> bool {
        let under = format!("{dir}/");
        self.live.keys().any(|it| it.starts_with(&under))
    }

    fn dir_changes(&self, dir: &str) -> u64 {
        self.dirs.get(dir).copied().unwrap_or(0)
    }

    /// A file's [`Stamp`], as [`Effect::Entries`] and [`Effect::Stamp`] report it: the inode is
    /// the model's index plus one (never 0, which with the rest 0 is a name with nothing
    /// there), the times its count of changes, since a disk with no clock has no other.
    fn file_stamp(&self, ino: Ino) -> Stamp {
        let inode = &self.inodes[ino];
        Stamp {
            ino: ino as u64 + 1,
            size: inode.live.len() as u64,
            mtime_ns: inode.changes,
            ctime_ns: inode.changes,
            mode: S_IFREG | 0o644,
        }
    }

    fn stat(&self, file: &FileName) -> Option<Meta> {
        match self.ino(file) {
            Some(ino) => {
                let inode = &self.inodes[ino];
                Some(Meta { len: inode.live.len() as u64, dir: false, modified_ns: inode.changes })
            }
            None if self.is_dir(file.as_str()) => Some(Meta {
                len: 0,
                dir: true,
                modified_ns: self.dir_changes(file.as_str()),
            }),
            None => None,
        }
    }

    /// Every file and directory under `dir`, at any depth, sorted by name ([`Effect::List`]).
    fn list(&self, dir: &FileName) -> Done {
        if self.live.contains_key(dir.as_str()) {
            return Done::Failed(IoError(format!("{dir}: not a directory")));
        }
        let under = format!("{dir}/");
        let mut listed: BTreeMap<String, Meta> = BTreeMap::new();
        for (name, ino) in self.live.iter().filter(|(name, _)| name.starts_with(&under)) {
            for (at, _) in name[under.len()..].match_indices('/') {
                let inner = &name[..under.len() + at];
                let meta = Meta { len: 0, dir: true, modified_ns: self.dir_changes(inner) };
                listed.insert(inner.to_owned(), meta);
            }
            let inode = &self.inodes[*ino];
            let meta =
                Meta { len: inode.live.len() as u64, dir: false, modified_ns: inode.changes };
            listed.insert(name.clone(), meta);
        }
        if listed.is_empty() {
            return Done::Failed(IoError(format!("{dir}: no such directory")));
        }
        Done::Listed(
            listed
                .into_iter()
                .map(|(name, meta)| (FileName::new(&name).expect("a name of the disk"), meta))
                .collect(),
        )
    }

    /// What is directly in `dir`, each name with its stamp, sorted ([`Effect::Entries`]).
    fn entries(&self, dir: &FileName) -> Done {
        if self.live.contains_key(dir.as_str()) {
            return Done::Failed(IoError(format!("{dir}: not a directory")));
        }
        let under = format!("{dir}/");
        let mut found: BTreeMap<String, Stamp> = BTreeMap::new();
        for (name, ino) in self.live.iter().filter(|(name, _)| name.starts_with(&under)) {
            match name[under.len()..].split_once('/') {
                None => {
                    found.insert(name.clone(), self.file_stamp(*ino));
                }
                Some((first, _)) => {
                    let inner = format!("{under}{first}");
                    let changes = self.dir_changes(&inner);
                    let stamp = Stamp {
                        ino: u64::MAX,
                        size: 0,
                        mtime_ns: changes,
                        ctime_ns: changes,
                        mode: S_IFDIR | 0o755,
                    };
                    found.insert(inner, stamp);
                }
            }
        }
        if found.is_empty() {
            return Done::Failed(IoError(format!("{dir}: no such directory")));
        }
        Done::Entries(
            found
                .into_iter()
                .map(|(name, stamp)| (FileName::new(&name).expect("a name of the disk"), stamp))
                .collect(),
        )
    }

    /// Runs what `effect` does to the names and bytes the process sees, nothing of it durable
    /// yet, and says how it ended; `None` for an effect that changes neither (a read, a look, a
    /// hint), which [`CrashFs::answer`] ends. What a completion makes durable or orders on top
    /// is [`CrashFs::settle`]'s.
    ///
    /// Every variant of [`Effect`] is named here, and no arm takes the rest: one added to
    /// `blaze_core` fails to compile until it is given a rule or put with those that have none.
    fn change(&mut self, effect: &Effect, asked: u64) -> Option<Done> {
        Some(match effect {
            Effect::Append { file, bytes, .. } => {
                self.edit(file, Change::Append(bytes.clone()), asked);
                Done::Appended
            }
            Effect::Truncate { file, len, .. } => {
                self.edit(file, Change::Cut(*len), asked);
                Done::Truncated
            }
            Effect::WriteAt { file, at, bytes, .. } => match self.ino(file) {
                Some(_) => {
                    self.edit(file, Change::At(*at, bytes.clone()), asked);
                    Done::Written
                }
                None => missing(file),
            },
            Effect::WriteFrom { file, at, from, cache, .. } => {
                match (self.ino(file), spans::written_from(from, file, *at, *cache)) {
                    (Some(_), Ok(bytes)) => {
                        self.edit(file, Change::Blocks(*at, bytes), asked);
                        Done::Written
                    }
                    (Some(_), Err(refused)) => Done::Failed(refused),
                    (None, _) => missing(file),
                }
            }
            // A new file, whole and synced, behind an entry that its completion makes durable.
            Effect::Write { file, bytes, .. } => {
                let ino = self.inode(Inode {
                    live: bytes.clone(),
                    durable: bytes.clone(),
                    ..Inode::default()
                });
                self.link(file.as_str(), ino, asked);
                Done::Written
            }
            // A new file under the name, its bytes one change not synced and torn by block,
            // behind an entry that is pending: a crash that loses the entry leaves the name
            // what it was.
            Effect::WriteUnsynced { file, bytes, .. } => {
                let ino = self.inode(Inode::default());
                self.link(file.as_str(), ino, asked);
                if !bytes.is_empty() {
                    self.edit(file, Change::Blocks(0, bytes.clone()), asked);
                }
                Done::Written
            }
            Effect::Rename { from, to, .. } | Effect::RenameUnsynced { from, to, .. } => {
                match self.ino(from) {
                    Some(ino) => {
                        self.rename(from.as_str(), to.as_str(), ino, asked);
                        Done::Renamed
                    }
                    None => missing(from),
                }
            }
            Effect::Clone { from, to, .. } => match (self.ino(from), self.clones) {
                (None, _) => missing(from),
                // As the native drivers say it, naming both files: nothing copies instead.
                (Some(_), Clones::Refused) => Done::Failed(IoError(format!(
                    "the file system of {to} (the model's, which shares no blocks) cannot share blocks with {from}; blaze does not copy instead"
                ))),
                (Some(_), Clones::Made) if self.live.contains_key(to.as_str()) => {
                    Done::Failed(IoError(format!("{to}: already there")))
                }
                // Its source's bytes as the process sees them, whole: a file system writes a
                // source's unwritten data out before it shares the blocks, so a clone whose
                // entry a crash keeps is never torn.
                (Some(ino), Clones::Made) => {
                    let bytes = self.inodes[ino].live.clone();
                    let clone =
                        self.inode(Inode { live: bytes.clone(), durable: bytes, ..Inode::default() });
                    self.link(to.as_str(), clone, asked);
                    Done::Cloned
                }
            },
            // A copy of the source's data into a new inode: every file system makes one, so
            // there is no refusal for the volume. Like a clone, a crash that keeps the new
            // entry keeps it whole (the copy is synced before the store renames it).
            Effect::CopyData { from, to, .. } => match self.ino(from) {
                None => missing(from),
                Some(_) if self.live.contains_key(to.as_str()) => {
                    Done::Failed(IoError(format!("{to}: already there")))
                }
                Some(ino) => {
                    let bytes = self.inodes[ino].live.clone(); // clone: the new inode's own bytes
                    let len = bytes.len() as u64;
                    let copy =
                        self.inode(Inode { live: bytes.clone(), durable: bytes, ..Inode::default() }); // clone: live and durable start equal
                    self.link(to.as_str(), copy, asked);
                    Done::Copied { bytes: len }
                }
            },
            // Another name of the same inode: the model keys names to inodes, so the two share
            // bytes as a hard link does. The store asks it for the second path of a program,
            // which nothing writes in place.
            Effect::Link { from, to, .. } => match self.ino(from) {
                None => missing(from),
                Some(_) if self.live.contains_key(to.as_str()) => {
                    Done::Failed(IoError(format!("{to}: already there")))
                }
                Some(ino) => {
                    self.link(to.as_str(), ino, asked);
                    Done::Linked
                }
            },
            Effect::Remove { file, .. } => {
                if self.live.contains_key(file.as_str()) {
                    self.unlink(file.as_str(), asked);
                }
                Done::Removed
            }
            // The file is made where it is missing, as a sync makes it.
            Effect::Sync { file, .. } => {
                self.made(file, asked);
                Done::Synced
            }
            Effect::Barrier { file, .. } => {
                self.made(file, asked);
                Done::Ordered
            }
            // Neither a name nor a byte changes: `CrashFs::answer` ends them.
            Effect::Private { .. }
            | Effect::ReadAll { .. }
            | Effect::Read { .. }
            | Effect::ReadInto { .. }
            | Effect::ReadMany { .. }
            | Effect::Map { .. }
            | Effect::Stat { .. }
            | Effect::Lstat { .. }
            | Effect::List { .. }
            | Effect::Entries { .. }
            | Effect::Prewarm { .. }
            | Effect::Seal { .. }
            | Effect::WriteBack { .. }
            | Effect::Stamp { .. }
            | Effect::SyncDir { .. } => return None,
            // File effects neither the store nor the kernel asks for (2026-11-12): symbolic
            // links, modes, times, locks and directories of their own are not modelled.
            Effect::ReadLink { .. }
            | Effect::Mode { .. }
            | Effect::Times { .. }
            | Effect::Lock { .. }
            | Effect::Unlock { .. }
            | Effect::MakeDir { .. }
            | Effect::RemoveDir { .. } => panic!(
                "the crash model has no rule for {effect:?}: neither the store nor the kernel asked for it when the rules were written; add one to blaze-storemodel's fs.rs"
            ),
            // Not of a file system: watches, clocks, processes, sockets, the standard streams.
            Effect::Watch { .. }
            | Effect::Journal { .. }
            | Effect::Unwatch { .. }
            | Effect::Flush { .. }
            | Effect::Timer { .. }
            | Effect::Spawn { .. }
            | Effect::Kill { .. }
            | Effect::Listen { .. }
            | Effect::Unlisten { .. }
            | Effect::Connect { .. }
            | Effect::Recv { .. }
            | Effect::Send { .. }
            | Effect::Hangup { .. }
            | Effect::Pressure { .. }
            | Effect::Bind { .. }
            | Effect::SendTo { .. }
            | Effect::RecvFrom { .. }
            | Effect::Unbind { .. }
            | Effect::ReadStdin { .. }
            | Effect::WriteOut { .. }
            | Effect::Now { .. }
            | Effect::Random { .. } => panic!(
                "{effect:?} is no effect on files, and the crash model is a file system: a scenario that asks for it needs a driver for it beside blaze-storemodel's fs.rs"
            ),
        })
    }

    fn pending_text(&self) -> String {
        let mut parts: Vec<String> = self.pending.iter().map(|it| it.entry.describe()).collect();
        for (name, ino) in &self.live {
            for it in &self.inodes[*ino].unsynced {
                parts.push(format!("{name}: {}", it.change.describe()));
            }
        }
        parts.join(", ")
    }
}

/// The file system: see the module's docs.
pub struct CrashFs {
    persist: Persist,
    files: Files,
    durable: BTreeMap<String, Ino>,
    flying: Vec<Flying>,
    /// The barriers that completed and that nothing has made durable yet.
    barriers: Vec<Barrier>,
    /// How many barriers have completed: each effect, and each change it makes, is stamped
    /// with the epoch it was asked in, so what a barrier is ordered before is what was asked
    /// in a later epoch than the one it completed in.
    epoch: u64,
}

/// The later of two epochs, either of which may be of nothing.
fn later(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    a.max(b)
}

/// The earlier of two epochs, either of which may be of nothing.
fn earlier(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Whether a crash may keep what it kept: nothing asked after a barrier completed
/// (`newest`, the latest epoch of anything kept) without all that the barrier covers
/// (`unmet`, the earliest epoch of a barrier some of whose changes were lost or torn).
fn in_order(newest: Option<u64>, unmet: Option<u64>) -> bool {
    !matches!((newest, unmet), (Some(newest), Some(unmet)) if newest > unmet)
}

/// A crash state being put together: the files chosen so far.
struct Partial {
    disk: Disk,
    how: String,
    newest: Option<u64>,
    unmet: Option<u64>,
}

/// What a crash may keep of one file.
struct Choice {
    bytes: Vec<u8>,
    text: String,
    newest: Option<u64>,
    unmet: Option<u64>,
}

impl CrashFs {
    /// An empty disk.
    pub fn new(persist: Persist, clones: Clones) -> CrashFs {
        CrashFs {
            persist,
            files: Files::new(clones),
            durable: BTreeMap::new(),
            flying: Vec::new(),
            barriers: Vec::new(),
            epoch: 0,
        }
    }

    /// A fresh process on `disk`, everything on it durable.
    pub fn from_disk(persist: Persist, clones: Clones, disk: &Disk) -> CrashFs {
        let mut fs = CrashFs::new(persist, clones);
        for (name, bytes) in disk {
            let ino = fs.files.inode(Inode {
                live: bytes.clone(),
                durable: bytes.clone(),
                ..Inode::default()
            });
            fs.files.live.insert(name.clone(), ino);
            fs.durable.insert(name.clone(), ino);
        }
        fs
    }

    /// What a clean shutdown leaves: everything durable as it is now (no effect in flight).
    pub fn synced_disk(&self) -> Disk {
        self.durable
            .iter()
            .map(|(name, ino)| (name.clone(), self.files.inodes[*ino].durable.clone()))
            .collect()
    }

    /// The name of every file the process sees now.
    pub fn names(&self) -> BTreeSet<String> {
        self.files.live.keys().cloned().collect()
    }

    /// The effects asked for, in flight until [`CrashFs::complete`] or [`CrashFs::fail`] ends
    /// each.
    pub fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            let covers = match &effect {
                Effect::Sync { file, .. } | Effect::Barrier { file, .. } => {
                    self.files.ino(file).map_or(0, |ino| self.files.inodes[ino].unsynced.len())
                }
                _ => 0,
            };
            self.flying.push(Flying { effect, covers, asked: self.epoch });
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
        let Flying { effect, covers, asked } = self.flying.remove(at);
        let tag = effect.tag();
        let done = match self.files.change(&effect, asked) {
            Some(done) => done,
            None => self.answer(&effect),
        };
        if !matches!(done, Done::Failed(_)) {
            self.settle(&effect, covers, asked);
        }
        Completion { tag, done }
    }

    /// Ends an effect that changes no name and no byte ([`Files::change`] said `None`).
    fn answer(&self, effect: &Effect) -> Done {
        let files = &self.files;
        let there = |file: &FileName, done: Done| match files.ino(file) {
            Some(_) => done,
            None => missing(file),
        };
        match effect {
            Effect::Private { .. } => Done::Private(None),
            Effect::ReadAll { file, .. } => Done::Read(files.contents(file)),
            Effect::Read { file, at, len, .. } => {
                let bytes = files.contents(file);
                let at = (*at as usize).min(bytes.len());
                Done::Read(bytes[at..at.saturating_add(*len).min(bytes.len())].to_vec())
            }
            Effect::ReadInto { file, at, into, cache, .. } => {
                match read_part(files, file, *at, into, *cache) {
                    Ok(len) => Done::ReadInto { len },
                    Err(refused) => Done::Failed(refused),
                }
            }
            // Each part as its own read into memory, with its own result: one that fails
            // fails only itself, naming its file and offset.
            Effect::ReadMany { parts, cache, .. } => Done::ReadMany(
                parts
                    .iter()
                    .map(|part| {
                        let got = match read_part(files, &part.file, part.at, &part.into, *cache) {
                            Ok(len) => blaze_core::Got::Read(len),
                            Err(refused) => blaze_core::Got::Failed(refused),
                        };
                        // clone: the part handed back, a refcount and a span.
                        blaze_core::ReadPart { got, ..part.clone() }
                    })
                    // alloc: the parts, which the completion carries.
                    .collect(),
            ),
            Effect::Map { file, extent, .. } => {
                let bytes = files.contents(file);
                match *extent {
                    blaze_core::Extent::Whole => Done::Mapped(Mapping::copy(&bytes)),
                    blaze_core::Extent::Range { at, len } => {
                        match at.checked_add(len).filter(|end| *end <= bytes.len() as u64) {
                            Some(end) => Done::Mapped(Mapping::copy(&bytes[at as usize..end as usize])),
                            None => Done::Failed(IoError(format!(
                                "{file}: a mapping of {len} bytes at {at} runs past the file's end at {}",
                                bytes.len()
                            ))),
                        }
                    }
                }
            }
            // One look at each name, as the store asks before it renames a placed file.
            Effect::Stamp { files: names, .. } => Done::Stamped(
                names
                    .iter()
                    .map(|name| {
                        // alloc: a name of the batch as the model keys it.
                        let name = FileName::new(name).expect("a Names entry is a name");
                        files.ino(&name).map_or(Stamp::GONE, |ino| files.file_stamp(ino))
                    })
                    // alloc: the stamps, which the completion carries.
                    .collect(),
            ),
            // The model keeps no symbolic link: the two are one.
            Effect::Stat { file, .. } | Effect::Lstat { file, .. } => Done::Stat(files.stat(file)),
            Effect::List { dir, .. } => files.list(dir),
            Effect::Entries { dir, .. } => files.entries(dir),
            // A hint to the page cache, which the model has none of: nothing is read ahead.
            Effect::Prewarm { file, .. } => there(file, Done::Prewarmed { bytes: 0 }),
            // A mode, which the model does not keep.
            Effect::Seal { file, .. } => there(file, Done::Moded),
            // Begun to be written, with nothing of it durable or ordered: a crash tears what
            // it covered as before.
            Effect::WriteBack { file, .. } => there(file, Done::WrittenBack),
            Effect::SyncDir { dir, .. } => match files.is_dir(dir.as_str()) {
                true => Done::Synced,
                false => Done::Failed(IoError(format!("{dir}: no such directory"))),
            },
            other => unreachable!(
                "{other:?} changes files or has no rule: `Files::change` ends it, never `None`"
            ),
        }
    }

    /// What a completion makes durable or orders, on top of what [`Files::change`] did. An
    /// effect not named here makes nothing durable and orders nothing: the most a crash may
    /// undo.
    fn settle(&mut self, effect: &Effect, covers: usize, asked: u64) {
        match effect {
            Effect::Sync { file, .. } => {
                if let Some(ino) = self.files.ino(file) {
                    self.sync(ino, covers);
                }
                if self.persist == Persist::Ordered {
                    self.commit_entries(None);
                }
                self.flushed(asked);
            }
            Effect::Barrier { file, .. } => {
                if let Some(ino) = self.files.ino(file) {
                    let covers = covers.min(self.files.inodes[ino].unsynced.len());
                    let entries = match self.persist {
                        Persist::Ordered => self.files.pending.len(),
                        Persist::Posix => 0,
                    };
                    if covers > 0 || entries > 0 {
                        self.barriers.push(Barrier { ino, covers, entries, epoch: self.epoch });
                    }
                }
                self.epoch += 1;
            }
            Effect::SyncDir { dir, .. } => {
                self.commit_entries(Some(dir.as_str()));
                self.flushed(asked);
            }
            Effect::Write { file, .. } => {
                self.commit_entries(Some(dir_of(file.as_str())));
                self.flushed(asked);
            }
            Effect::Rename { from, to, .. } => {
                self.commit_entries(Some(dir_of(from.as_str())));
                self.commit_entries(Some(dir_of(to.as_str())));
                self.flushed(asked);
            }
            _ => {}
        }
    }

    /// Ends the effect in flight at `at` as failed. A failed append or write leaves its first
    /// half in the file, unsynced; a failed sync or barrier dooms what it covered; anything
    /// else changed nothing.
    pub fn fail(&mut self, at: usize) -> Completion {
        let Flying { effect, covers, asked } = self.flying.remove(at);
        let tag = effect.tag();
        match &effect {
            Effect::Append { file, bytes, .. } if !bytes.is_empty() => {
                self.files.edit(file, Change::Append(bytes[..bytes.len() / 2].to_vec()), asked);
            }
            Effect::WriteAt { file, at, bytes, .. } if self.files.ino(file).is_some() => {
                self.files.edit(file, Change::At(*at, bytes[..bytes.len() / 2].to_vec()), asked);
            }
            // A failed write from memory may have written any of its blocks: here, the whole
            // blocks of its first half, unsynced.
            Effect::WriteFrom { file, at, from, cache, .. } if self.files.ino(file).is_some() => {
                if let Ok(bytes) = spans::written_from(from, file, *at, *cache) {
                    let block = DIRECT_ALIGN as usize;
                    let half = bytes.len() / 2 / block * block;
                    if half > 0 {
                        self.files.edit(file, Change::Blocks(*at, bytes[..half].to_vec()), asked);
                    }
                }
            }
            Effect::Sync { file, .. } | Effect::Barrier { file, .. } => {
                if let Some(ino) = self.files.ino(file) {
                    for it in self.files.inodes[ino].unsynced.iter_mut().take(covers) {
                        it.doomed = true;
                    }
                }
            }
            _ => {}
        }
        Completion { tag, done: Done::Failed(IoError("injected by the model".into())) }
    }

    /// The first `covers` unsynced changes of the inode are durable now.
    fn sync(&mut self, ino: Ino, covers: usize) {
        let inode = &mut self.files.inodes[ino];
        let covers = covers.min(inode.unsynced.len());
        let covered: Vec<Unsynced> = inode.unsynced.drain(..covers).collect();
        for it in covered.iter().filter(|it| !it.doomed) {
            it.change.apply(&mut inode.durable);
        }
        for barrier in self.barriers.iter_mut().filter(|it| it.ino == ino) {
            barrier.covers = barrier.covers.saturating_sub(covers);
        }
        self.barriers.retain(|it| it.covers > 0 || it.entries > 0);
    }

    /// Something asked in epoch `asked` is durable now (a sync that waits for the drive
    /// completed, or an entry change was committed): so is all that each barrier that completed
    /// in an earlier epoch covers, which went to the device first.
    fn flushed(&mut self, asked: u64) {
        let mut due: BTreeMap<Ino, usize> = BTreeMap::new();
        for barrier in self.barriers.iter().filter(|it| it.epoch < asked) {
            let most = due.entry(barrier.ino).or_insert(0);
            *most = (*most).max(barrier.covers);
        }
        self.barriers.retain(|it| it.epoch >= asked);
        for (ino, covers) in due {
            self.sync(ino, covers);
        }
    }

    /// Makes durable the pending entry changes of `dir` (a directory sync), or every one
    /// (a journal commit).
    fn commit_entries(&mut self, dir: Option<&str>) {
        // A journaling file system commits every entry change before the sync, in any directory.
        let dir = if self.persist == Persist::Ordered { None } else { dir };
        let pending = std::mem::take(&mut self.files.pending);
        let mut newest: Option<u64> = None;
        for it in pending {
            match dir {
                Some(dir) if !it.entry.within(dir) => self.files.pending.push(it),
                _ => {
                    it.entry.apply(&mut self.durable);
                    newest = later(newest, Some(it.asked));
                }
            }
        }
        if self.persist == Persist::Ordered {
            // Every entry a barrier ordered is durable: it has none left to order.
            for barrier in &mut self.barriers {
                barrier.entries = 0;
            }
            self.barriers.retain(|it| it.covers > 0 || it.entries > 0);
        }
        if let Some(newest) = newest {
            self.flushed(newest);
        }
    }

    /// Every disk a crash now may leave, each once, in a fixed order; more than `most` is an
    /// error naming what was pending.
    pub fn crash_states(&self, most: usize) -> Result<Vec<CrashState>, TooManyStates> {
        // What may have happened: the pending entries and unsynced changes, then each effect
        // in flight as if it completed with nothing of it durable or ordered.
        let mut files = self.files.clone();
        for flying in &self.flying {
            drop(files.change(&flying.effect, flying.asked));
        }
        let entries = &files.pending;
        let too_many = || TooManyStates { most, pending: files.pending_text() };
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
                entries[n].entry.apply(&mut names);
            }
            let mut how = String::new();
            if !entries.is_empty() {
                let lost: Vec<String> = (0..entries.len())
                    .filter(|n| !kept.contains(n))
                    .map(|n| entries[n].entry.describe())
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
            // What the entries kept say of the barriers: an entry asked after one completed
            // is kept only with all it covers, and the entries one ordered are a prefix (a
            // barrier orders entries only where a crash keeps a prefix of them).
            let newest = kept.iter().map(|n| entries[*n].asked).max();
            let unmet =
                self.barriers.iter().filter(|it| it.entries > kept.len()).map(|it| it.epoch).min();
            if !in_order(newest, unmet) {
                continue;
            }
            // Each named file's choices: a prefix of its unsynced changes, the last perhaps torn.
            let mut disks: Vec<Partial> = vec![Partial { disk: Disk::new(), how, newest, unmet }];
            let named: BTreeSet<(String, Ino)> =
                names.iter().map(|(name, ino)| (name.clone(), *ino)).collect();
            for (name, ino) in named {
                let inode = &files.inodes[ino];
                // The latest epoch among the first `n` changes, all of them kept at least in part.
                let newest_of = |n: usize| inode.unsynced[..n].iter().map(|it| it.asked).max();
                // The earliest barrier of this file that covers more than the `whole` changes
                // kept whole.
                let unmet_by = |whole: usize| {
                    self.barriers
                        .iter()
                        .filter(|it| it.ino == ino && it.covers > whole)
                        .map(|it| it.epoch)
                        .min()
                };
                let mut choices: Vec<Choice> = Vec::new();
                for n in 0..=inode.unsynced.len() {
                    let mut bytes = inode.durable.clone();
                    for it in &inode.unsynced[..n] {
                        it.change.apply(&mut bytes);
                    }
                    let text = match (inode.unsynced.len(), n) {
                        (0, _) => String::new(),
                        (all, n) => format!("{name}: kept {n} of {all} unsynced changes"),
                    };
                    choices.push(Choice { bytes, text, newest: newest_of(n), unmet: unmet_by(n) });
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
                            let text = format!(
                                "{name}: kept {n} of {} unsynced changes, the last ({}) torn at {cut}",
                                inode.unsynced.len(),
                                last.describe()
                            );
                            choices.push(Choice {
                                bytes,
                                text,
                                newest: newest_of(n),
                                unmet: unmet_by(n - 1),
                            });
                        }
                    }
                }
                let mut next = Vec::with_capacity(disks.len() * choices.len());
                for partial in &disks {
                    for choice in &choices {
                        // Out of order already stays so: what is kept only grows.
                        let newest = later(partial.newest, choice.newest);
                        let unmet = earlier(partial.unmet, choice.unmet);
                        if !in_order(newest, unmet) {
                            continue;
                        }
                        let mut disk = partial.disk.clone();
                        disk.insert(name.clone(), choice.bytes.clone());
                        let how = match (partial.how.is_empty(), choice.text.is_empty()) {
                            (_, true) => partial.how.clone(),
                            (true, false) => choice.text.clone(),
                            (false, false) => format!("{}; {}", partial.how, choice.text),
                        };
                        next.push(Partial { disk, how, newest, unmet });
                    }
                }
                if next.len() > most {
                    return Err(too_many());
                }
                disks = next;
            }
            for Partial { disk, how, .. } in disks {
                states.entry(digest(&disk)).or_insert(CrashState { disk, how });
            }
            if states.len() > most {
                return Err(too_many());
            }
        }
        Ok(states.into_values().collect())
    }
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
