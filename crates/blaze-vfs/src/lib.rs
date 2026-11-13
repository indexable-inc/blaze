//! blaze v2's files (docs/v2/plan/design/vfs.md section 2, docs/v2/plan.md section 3): a persistent, content-addressed Merkle
//! tree, ported from blaze-next's `blaze-vfs` (`git show origin/blaze-next:crates/blaze-vfs`).
//!
//! - A file's [`ContentId`] is the hash of its bytes (domain [`FILE_DOMAIN`]); a directory's
//!   is the hash of its entries sorted by name ([`DIR_DOMAIN`]). Equal subtrees are one node.
//! - A **root** is a directory's content ID: a map of every file to its hash.
//! - A **view** ([`View`]) is a root a request builds: a branch, a PR, a CI job, a what-if
//!   of an agent's. Opening one is constant time, and every result keyed by input hashes is
//!   shared by every view whose inputs are equal.
//! - **Heads** are named roots (a workspace, a branch) that transactions move: a transaction
//!   made against an older root is rebased onto the head file by file, or refused naming
//!   the path ([`CommitError::Conflict`]).
//! - The journal makes heads and the nodes they reach durable through
//!   [`blaze_core::Effect`]s, with group commit and recovery from a torn tail.
//! - Watching: what the system's journal says changed ([`blaze_core::Done::Journaled`]),
//!   from the cursor saved with the tracked paths' stamps, becomes ops on the workspace's head
//!   ([`Watcher`]); a file whose stamp did not move is never read.
//! - Outside every view: the directories user code reads that no view holds, watched so a
//!   check asks a lock-free table whether anything under a path changed ([`outside`]).
//!
//! Unsafe code is only in `store` and `lent` (docs/v2/plan/design/unsafe.md), whose APIs are
//! safe.

#![deny(unsafe_code)]
#![feature(allocator_api)]

mod disk;
mod image;
mod journal;
#[allow(unsafe_code)]
mod lent;
pub mod outside;
mod paths;
mod probes;
mod reader;
mod rebase;
mod section;
mod settle;
#[allow(unsafe_code)]
mod store;
mod sync;
mod text;
mod texts;
mod tree;
pub mod watch;

use std::{borrow::Cow, fmt};

use blaze_core::{Completion, Effect, FileName, Stamp, Tag};
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Idx};
use serde::{Deserialize, Serialize};
use std::alloc::Global;

use crate::{
    journal::{Journal, Loaded},
    tree::{NodeId, Tree},
};
pub use blaze_image::Corrupt;

pub use crate::{
    disk::{COARSE_NS, FINE_NS, MARK_MOST, racy, window_ns},
    image::{Bytes, DisksUnused, ImageError, ImageFile, ImageMark, ImageUse, tree_layout},
    lent::lent,
    outside::{Cover, Outside, cover, generation, unchanged_since, want},
    reader::{Pinned, Reader},
    section::Section,
    settle::{Settled, Unsettled},
    text::{Text, TextFile, text_of},
    texts::{FaultWhy, HeldTexts, StoreFault, TEXT_SCHEMA, Texts},
    tree::Jobs,
    watch::{Watched, Watcher},
};

pub const FILE_DOMAIN: &str = "blaze v2 vfs file 1";

/// The most bytes one file of the tree holds: the VFS journal's file record carries its
/// payload's length in 32 bits. A commit of a longer file is refused by name
/// ([`OpError::TooLarge`]), and the watcher refuses one before it reads it
/// ([`watch::Watched::TooLarge`]), so no record is ever framed over it. Lifted once a file is
/// recorded by its content ID and its bytes stream to the store
/// (docs/v2/plan/design/lazy-file-ids.md).
pub const MOST_FILE_BYTES: u64 = u32::MAX as u64;
pub const DIR_DOMAIN: &str = "blaze v2 vfs directory 2";

/// What kind of file an entry is, as git's tree says: hashed into the directory's entry with
/// git's code, so a change of mode is a change of the tree while equal bytes stay one file. A
/// symlink's bytes are its target.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Mode {
    Regular,
    Executable,
    Symlink,
}

impl Mode {
    /// git's mode for it: `0o100644`, `0o100755`, `0o120000`.
    pub const fn git(self) -> u32 {
        match self {
            Mode::Regular => 0o100644,
            Mode::Executable => 0o100755,
            Mode::Symlink => 0o120000,
        }
    }

    /// The mode of a git tree entry that is a file; `None` for a directory, a submodule or
    /// anything else. `0o100664`, which old gits wrote, is a regular file, as git reads it.
    pub const fn from_git(mode: u32) -> Option<Mode> {
        match mode {
            0o100644 | 0o100664 => Some(Mode::Regular),
            0o100755 => Some(Mode::Executable),
            0o120000 => Some(Mode::Symlink),
            _ => None,
        }
    }
}

/// A root a request builds. Its identity is its content ID; [`ViewId`] is the daemon's dense
/// name for an open one.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct View {
    pub root: ContentId,
}

/// An open view, in the daemon's arena of them. Never in a key or a hash: results are keyed by
/// input hashes, never by view.
pub type ViewId = Idx<View>;

/// A file of a view, read in place. A symlink is read as itself: its bytes are its target
/// ([`Vfs::follow`] reads what it points to).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct File<'a> {
    /// The hash of `bytes` alone: equal bytes of two modes are one ID.
    pub id: ContentId,
    pub mode: Mode,
    pub bytes: &'a [u8],
}

/// A change to a list of files, applied all together or not at all. Ops act on a symlink
/// itself, never on what it points to.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)] // clone: in blaze-api's requests, the edge
pub enum Op {
    // Each variant's fields in CBOR key order (shorter name first, then bytewise), so the
    // encoder writes the map as it comes.
    /// The whole file with its mode, created with its directories if missing.
    Write {
        mode: Mode,
        path: String,
        #[serde(with = "blaze_protocol::bytes")]
        bytes: Vec<u8>,
    },
    /// `len` bytes at `at` replaced by `bytes`, as an editor's change is; the mode is kept.
    Edit {
        at: u64,
        len: u64,
        path: String,
        #[serde(with = "blaze_protocol::bytes")]
        bytes: Vec<u8>,
    },
    /// A file, or a directory with everything under it.
    Delete { path: String },
    /// A file or a directory moved; `to` must not exist.
    Rename { to: String, from: String },
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub enum OpError {
    /// Empty, absolute, or with an empty, `.` or `..` part.
    BadPath(String),
    NotFound(String),
    IsADirectory(String),
    NotADirectory(String),
    Exists(String),
    OutOfRange {
        path: String,
        at: u64,
        len: u64,
        size: u64,
    },
    /// The symlink at `path` points outside the tree (an absolute target, or `..` past the
    /// root) or into a loop of symlinks.
    BadLink {
        path: String,
        target: String,
    },
    /// The base root the ops were given is not one this tree has.
    UnknownRoot(ContentId),
    /// A file of the image the tree was restored from failed its page group's CRC32C: its
    /// bytes are not read until the same bytes are written again ([`Vfs::corrupt`]).
    Corrupt {
        section: u32,
        group: u32,
    },
    /// The file at `path` would be `size` bytes, more than `most` ([`MOST_FILE_BYTES`]).
    TooLarge {
        path: String,
        size: u64,
        most: u64,
    },
}

impl fmt::Display for OpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpError::BadPath(path) => write!(f, "{path:?} is not a path in the tree"),
            OpError::NotFound(path) => write!(f, "{path} is not there"),
            OpError::IsADirectory(path) => write!(f, "{path} is a directory"),
            OpError::NotADirectory(path) => write!(f, "a part of {path} is a file"),
            OpError::Exists(path) => write!(f, "{path} is already there"),
            OpError::OutOfRange { path, at, len, size } => {
                write!(f, "{path} has {size} bytes, so {len} at {at} are not in it")
            }
            OpError::BadLink { path, target } => {
                write!(f, "{path} is a symlink to {target:?}, which leaves the tree or loops")
            }
            OpError::UnknownRoot(root) => write!(f, "no root {root} is known"),
            OpError::Corrupt { section, group } => {
                write!(f, "page group {group} of image section {section:#x} is corrupt")
            }
            OpError::TooLarge { path, size, most } => write!(
                f,
                "{path} is {size} bytes, over the {most} one file of the tree may hold"
            ),
        }
    }
}

/// A file that differs between two roots. Ordered by path, then kind.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)] // clone: in blaze-api's responses, the edge
pub struct Change {
    // In CBOR key order (shorter name first), so the encoder writes the map as it comes.
    pub kind: ChangeKind,
    pub path: String,
}

impl Ord for Change {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.path.cmp(&other.path).then(self.kind.cmp(&other.kind))
    }
}

impl PartialOrd for Change {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
}

/// What a transaction made.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Commit {
    pub root: ContentId,
    /// The head had moved past the transaction's base and the ops were moved onto it.
    pub rebased: bool,
    /// Its place in line for durability: [`Vfs::complete`] says when it is durable.
    pub ticket: Ticket,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub enum CommitError {
    /// The base root is not one this tree has.
    UnknownBase(ContentId),
    /// The head moved and changed `path` in a way the ops can't be moved past.
    Conflict {
        path: String,
    },
    Op(OpError),
    /// The journal could not record the commit: the head did not move.
    Record(RecordTooLarge),
}

/// The most bytes of one journal record's payload: the record carries its length in 32 bits.
pub const MOST_RECORD_BYTES: u64 = u32::MAX as u64;

/// A journal record refused for a payload of `len` bytes, over [`MOST_RECORD_BYTES`]: nothing of
/// it is written. `what` names what it records (a directory by its content ID and its count of
/// entries, a head by its name).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct RecordTooLarge {
    pub what: String,
    pub len: u64,
}

impl fmt::Display for RecordTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the journal's record of {} is {} bytes, over the {MOST_RECORD_BYTES} a record's 32-bit length holds (blaze-vfs MOST_RECORD_BYTES)",
            self.what, self.len
        )
    }
}

impl std::error::Error for RecordTooLarge {}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommitError::UnknownBase(root) => write!(f, "no root {root} is known"),
            CommitError::Conflict { path } => write!(f, "{path} changed since the base"),
            CommitError::Op(error) => fmt::Display::fmt(error, f),
            CommitError::Record(error) => fmt::Display::fmt(error, f),
        }
    }
}

impl From<OpError> for CommitError {
    fn from(error: OpError) -> CommitError {
        CommitError::Op(error)
    }
}

/// A commit's place in line for durability.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Ticket(pub u64);

/// The tree, its heads, its open views and its journal: a state machine that does its IO
/// through effects.
pub struct Vfs {
    tree: Tree,
    heads: HashMap<Box<str>, NodeId, Global>,
    /// By view: its root while it is open. A closed view's number is not given out again.
    views: Vec<Option<NodeId>>,
    journal: Journal,
    /// What each head's watcher last saw of the disk: saved in the image with the tree, and
    /// what changed since in the journal (`SEEN` records).
    disks: disk::Disks,
    /// The machine's local facts by name ([`Vfs::mark`]): in the journal (`MARK` records).
    marks: disk::Marks,
}

/// One head's column as its watcher last saw it ([`Vfs::column`]): the watched directory
/// `dir` (a name under the driver's root, as every [`FileName`] is), each name under it with
/// its [`Stamp`], and `root`, the head's root, when the column vouches for it (`None`: it
/// vouches for no root). The invariant: a column never pairs a root with a stamp of bytes other
/// than the ones that root holds at that path. A name taken inside its racy window ([`racy`]),
/// one whose head chose other bytes (a conflict, a commit by anyone but the watcher) and one
/// never read are [`Stamp::UNKNOWN`]: the column vouches for none of them.
pub struct Column<'a> {
    pub dir: &'a str,
    pub root: Option<ContentId>,
    disk: &'a disk::Disk,
}

impl Column<'_> {
    /// Calls `f` with every name at or under `dir` (the names are under the driver's root,
    /// `dir` itself first) and its stamp, each once, in name order: the image's column merged
    /// with what changed since, a name gone left out, a stamp that vouches for nothing
    /// [`Stamp::UNKNOWN`].
    pub fn each(&self, mut f: impl FnMut(&str, blaze_core::Stamp)) {
        self.disk.under_saved(self.dir, |name, stamp| match disk::vouches(&stamp) {
            true => f(name, stamp),
            false => f(name, blaze_core::Stamp::UNKNOWN),
        });
    }

    /// At most how many names [`Column::each`] hands: what a copy of it reserves.
    pub fn len(&self) -> usize {
        self.disk.size().0
    }

    /// Whether the column holds no name.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A mark [`Vfs::mark`] refused: its bytes, or its name, more than [`MARK_MOST`] bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MarkTooLarge {
    pub name: String,
    pub len: usize,
}

impl fmt::Display for MarkTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = blaze_report::say!("the mark ", blaze_report::say::Code(&self.name)).flat();
        write!(
            f,
            "{name} (its bytes or its name) is {} bytes, over the {MARK_MOST} a mark holds (blaze-vfs MARK_MOST)",
            self.len
        )
    }
}

impl std::error::Error for MarkTooLarge {}

impl Vfs {
    /// A VFS recovered from its journal's bytes (empty for a new one), and what it found.
    /// Tags it asks with are `tags..tags + 2^32`.
    pub fn recover(journal: FileName, tags: u64, bytes: &[u8]) -> (Vfs, Recovered) {
        let (mut disks, mut marks) = (disk::Disks::default(), disk::Marks::default());
        let Loaded { tree, heads, journal, found } =
            Journal::recover(journal, tags, bytes, &mut disks, &mut marks);
        // alloc: empty, allocates nothing; once per process.
        (Vfs { tree, heads, views: Vec::new(), journal, disks, marks }, found)
    }

    /// Sets `name`'s mark to `bytes` (empty: removes it), a local fact the next start finds
    /// ([`Vfs::marked`]); durable once the ticket is in what [`Vfs::complete`] returns. A mark
    /// already `bytes` records nothing, its ticket given with the next batch. Refused, naming
    /// it, over [`MARK_MOST`] bytes.
    pub fn mark(&mut self, name: &str, bytes: &[u8]) -> Result<Ticket, MarkTooLarge> {
        // Its name too, so a mark's record is always under a journal record's bound.
        let len = bytes.len().max(name.len());
        if len > MARK_MOST {
            // alloc: once per refusal, the name it is about.
            return Err(MarkTooLarge { name: name.to_owned(), len });
        }
        if self.marks.get(name).unwrap_or_default() == bytes {
            return Ok(self.journal.ticket());
        }
        self.marks.set(name, bytes);
        Ok(self.journal.record_mark(name, bytes))
    }

    /// Removes `name`'s mark; `None` (and nothing recorded) for a name with none.
    pub fn unmark(&mut self, name: &str) -> Option<Ticket> {
        self.marks.get(name)?;
        self.marks.set(name, &[]);
        Some(self.journal.record_mark(name, &[]))
    }

    /// `name`'s mark, as last set.
    pub fn marked(&self, name: &str) -> Option<&[u8]> {
        self.marks.get(name)
    }

    /// Every mark whose name starts with `prefix`, in name order.
    pub fn marks<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = (&'a str, &'a [u8])> {
        self.marks.under(prefix)
    }

    /// A handle that reads any root this tree has, from any thread, without the `Vfs` and
    /// without a lock: what the query database's `blaze_query::Tree` is over. A root is in it
    /// before any call that gives its ID returns. Memory a collection frees is reclaimed by
    /// the `Vfs` itself once no reader can be in it; no caller has to say when.
    pub fn reader(&self) -> Reader {
        self.tree.reader()
    }

    /// What `head`'s watcher last saw of the disk ([`Column`]); `None` for a head no watcher
    /// saved a column of. What a request's slot copies (blaze-certify's `slot::Column`): the
    /// column's one home stays here.
    pub fn column(&self, head: &str) -> Option<Column<'_>> {
        let disk = self.disks.get(head)?;
        // The root the column was kept against (`Disk::root`), while it is the head's: a commit
        // by anyone but the watcher moves both at once (`Vfs::rechain`), so they differ only
        // where a journal's tail was cut between a head record and its `SEEN`.
        let root = match () {
            #[cfg(test)]
            () if planted(Plant::ColumnOfTheHead) => self.head(head),
            () => disk.root.filter(|it| Some(*it) == self.head(head)),
        };
        Some(Column { dir: disk.dir.as_str(), root, disk })
    }

    /// The root `head` is at, if it was ever written.
    pub fn head(&self, head: &str) -> Option<ContentId> {
        self.heads.get(head).map(|it| self.tree.id(*it))
    }

    /// Every head and its root, by name.
    pub fn heads(&self) -> Vec<(&str, ContentId)> {
        let mut heads: Vec<(&str, ContentId)> =
            // alloc: the listing is the output.
            self.heads.iter().map(|(name, root)| (&**name, self.tree.id(*root))).collect();
        heads.sort_unstable();
        heads
    }

    /// The root with no files: the base of a head's first transaction.
    pub fn empty(&self) -> ContentId {
        self.tree.id(self.tree.empty())
    }

    /// Whether `root` is a root this tree has.
    pub fn has(&self, root: ContentId) -> bool {
        self.root(root).is_some()
    }

    /// The journal's sequence number of `head`'s last move: later moves have larger ones.
    pub fn sequence(&self, head: &str) -> Option<u64> {
        self.journal.sequence(head)
    }

    fn root(&self, root: ContentId) -> Option<NodeId> {
        self.tree.node(&root).filter(|it| self.tree.is_dir(*it))
    }

    /// Applies `ops`, made against `base`, to `head`, rebasing them if it moved.
    pub fn commit(
        &mut self,
        head: &str,
        base: ContentId,
        ops: &[Op],
    ) -> Result<Commit, CommitError> {
        let base = self.root(base).ok_or(CommitError::UnknownBase(base))?;
        let at = self.heads.get(head).copied().unwrap_or(self.tree.empty());
        if base == at {
            let commit = self.move_head(head, at, false, ops.iter().map(Cow::Borrowed))?;
            self.rechain(head, at);
            return Ok(commit);
        }
        let moved = rebase::rebase(&self.tree, base, at, ops)?;
        let ops =
            ops.iter().zip(moved).map(|(op, moved)| moved.map_or(Cow::Borrowed(op), Cow::Owned));
        let commit = self.move_head(head, at, true, ops)?;
        self.rechain(head, at);
        Ok(commit)
    }

    /// [`Vfs::commit`] of ops it may keep: each write's bytes become the file's own with no
    /// copy, where `commit` copies them once.
    pub fn commit_owned(
        &mut self,
        head: &str,
        base: ContentId,
        ops: Vec<Op>,
    ) -> Result<Commit, CommitError> {
        let base = self.root(base).ok_or(CommitError::UnknownBase(base))?;
        let at = self.heads.get(head).copied().unwrap_or(self.tree.empty());
        if base == at {
            let commit = self.move_head(head, at, false, ops.into_iter().map(Cow::Owned))?;
            self.rechain(head, at);
            return Ok(commit);
        }
        let moved = rebase::rebase(&self.tree, base, at, &ops)?;
        let ops = ops.into_iter().zip(moved).map(|(op, moved)| Cow::Owned(moved.unwrap_or(op)));
        let commit = self.move_head(head, at, true, ops)?;
        self.rechain(head, at);
        Ok(commit)
    }

    /// The watcher's commit of what it saw on the disk to `head`, as it is now: the column's
    /// stamps for it are the watcher's to save ([`Watcher`]), so the column is not moved here as
    /// [`Vfs::commit`] moves it.
    pub(crate) fn commit_seen(&mut self, head: &str, ops: Vec<Op>) -> Result<Commit, CommitError> {
        let at = self.heads.get(head).copied().unwrap_or(self.tree.empty());
        self.move_head(head, at, false, ops.into_iter().map(Cow::Owned))
    }

    /// After a commit to `head` by anyone but its watcher, from the root `from`: the column
    /// moves with the head. What the commit changed is marked in it, so its invariant holds of
    /// the head's new root (`Disk::root`): a saved stamp of a path the commit changed is one
    /// the head chose against ([`disk::chose`]), and a path the commit made that the column has
    /// no name for is saved [`Stamp::UNKNOWN`], so the column holds every file of the root. A
    /// `SEEN` record after the commit's head record says so. A column kept against another root
    /// than `from` (a journal cut between a head record and its `SEEN`) vouches for none after.
    fn rechain(&mut self, head: &str, from: NodeId) {
        #[cfg(test)]
        if planted(Plant::ColumnOfTheHead) {
            return;
        }
        let Some(disk) = self.disks.by_head.get_mut(head) else { return };
        let Some(synced) = disk.root else { return };
        let to = self.heads.get(head).copied().unwrap_or(self.tree.empty());
        // alloc: per commit to a watched head by another writer, its `SEEN` record's entries.
        let mut entries = Vec::new();
        let mut count = 0u32;
        if synced == self.tree.id(from) {
            // alloc: per such commit, the one buffer each name is built in.
            let mut name = String::new();
            let changed = self.tree.diff(from, to);
            let relisted = self.tree.relisted(from, to);
            let paths = changed
                .iter()
                .map(|it| (it.path.as_str(), it.kind != ChangeKind::Removed))
                .chain(relisted.iter().map(|it| (it.as_str(), true)));
            for (path, made) in paths {
                name.clear();
                name.push_str(disk.dir.as_str());
                if !path.is_empty() {
                    name.push('/');
                    name.push_str(path);
                }
                let stamp = match disk.saved(&name) {
                    Some(saved) if disk::vouches(&saved) => disk::chose(saved),
                    Some(_) => continue,
                    None if made => Stamp::UNKNOWN,
                    None => continue,
                };
                disk.set(&name, stamp);
                disk::put_entry(&mut entries, &name, stamp);
                count += 1;
            }
            disk.root = Some(self.tree.id(to));
        } else {
            disk.root = None;
        }
        self.journal.record_seen(head, disk, count, &entries);
    }

    fn move_head<'o>(
        &mut self,
        head: &str,
        at: NodeId,
        rebased: bool,
        ops: impl IntoIterator<Item = Cow<'o, Op>>,
    ) -> Result<Commit, CommitError> {
        let root = self.tree.apply_all(at, ops)?;
        self.tree.reclaim();
        // Recorded first: a head the journal refuses does not move.
        let ticket = self.journal.record_head(&self.tree, head, root).map_err(CommitError::Record)?;
        match self.heads.get_mut(head) {
            Some(node) => *node = root,
            None => {
                self.heads.insert(head.into(), root);
            }
        }
        Ok(Commit { root: self.tree.id(root), rebased, ticket })
    }

    /// Moves `head` to `root` whatever it was at, as a checkout, a fork or a push does.
    pub fn set_head(&mut self, head: &str, root: ContentId) -> Result<Ticket, CommitError> {
        let node = self.root(root).ok_or(CommitError::UnknownBase(root))?;
        let from = self.heads.get(head).copied().unwrap_or(self.tree.empty());
        let ticket = self.journal.record_head(&self.tree, head, node).map_err(CommitError::Record)?;
        self.heads.insert(head.into(), node);
        self.rechain(head, from);
        Ok(ticket)
    }

    /// Removes `head`, as a deleted branch or a closed PR does; `None` when there is none.
    pub fn remove_head(&mut self, head: &str) -> Option<Ticket> {
        self.heads.remove(head)?;
        Some(self.journal.record_removal(head))
    }

    /// The root made of `base` and `ops`, kept as a view's root and moving no head: a what-if.
    pub fn apply(&mut self, base: ContentId, ops: &[Op]) -> Result<ContentId, OpError> {
        let base = self.root(base).ok_or(OpError::UnknownRoot(base))?;
        let root = self.tree.apply(base, ops)?;
        self.tree.reclaim();
        Ok(self.tree.id(root))
    }

    /// [`Vfs::apply`] of ops it may keep, as [`Vfs::commit_owned`] is: a checkout's files become
    /// the tree's with no copy.
    pub fn apply_owned(&mut self, base: ContentId, ops: Vec<Op>) -> Result<ContentId, OpError> {
        let base = self.root(base).ok_or(OpError::UnknownRoot(base))?;
        let root = self.tree.apply_all(base, ops.into_iter().map(Cow::Owned))?;
        self.tree.reclaim();
        Ok(self.tree.id(root))
    }

    /// The root `files` written over `base` make, each a whole file with its mode, as
    /// [`Vfs::apply_owned`] of `Op::Write`s: for bytes already held where they live (a fetched
    /// source's, [`blaze_mem::Kept`]), which become the tree's with no copy.
    pub fn apply_files(
        &mut self,
        base: ContentId,
        files: impl IntoIterator<Item = (String, Bytes, Mode)>,
    ) -> Result<ContentId, OpError> {
        let base = self.root(base).ok_or(OpError::UnknownRoot(base))?;
        let root = self.tree.apply_files(base, files)?;
        self.tree.reclaim();
        Ok(self.tree.id(root))
    }

    /// Hashes a commit's new files on `jobs` once they are many bytes (a checkout, a large
    /// write): the daemon's pool, so a checkout hashes on every core. The IDs are the same
    /// either way.
    pub fn hash_on(&mut self, jobs: Jobs) {
        self.tree.jobs = Some(jobs);
    }

    /// Opens `view`, in constant time. Its root and what it reaches are kept until it closes.
    pub fn open(&mut self, view: View) -> Result<ViewId, CommitError> {
        let root = self.root(view.root).ok_or(CommitError::UnknownBase(view.root))?;
        let id = ViewId::from_raw(u32::try_from(self.views.len()).expect("fewer than 2^32 views"));
        self.views.push(Some(root));
        Ok(id)
    }

    pub fn close(&mut self, view: ViewId) {
        let slot = self.views.get_mut(view.raw() as usize).and_then(Option::take);
        assert!(slot.is_some(), "view {view:?} is not open");
    }

    fn open_root(&self, view: ViewId) -> NodeId {
        self.views
            .get(view.raw() as usize)
            .copied()
            .flatten()
            .unwrap_or_else(|| panic!("view {view:?} is not open"))
    }

    pub fn view(&self, view: ViewId) -> View {
        View { root: self.tree.id(self.open_root(view)) }
    }

    /// The file at `path` in `view`; `Ok(None)` for none, or a directory. A file of the image
    /// whose page group fails its check is `Err`.
    pub fn read(&self, view: ViewId, path: &str) -> Result<Option<File<'_>>, Corrupt> {
        let Some((node, mode)) = self.tree.entry(self.open_root(view), path) else {
            return Ok(None);
        };
        Ok(self.tree.bytes(node)?.map(|bytes| File { id: self.tree.id(node), mode, bytes }))
    }

    /// The file at `path` in `view` with every symlink on the way followed, as the system opens
    /// a path (a relative target is taken from the link's directory); `Ok(None)` for none, or a
    /// directory. A link out of the tree or into a loop is [`OpError::BadLink`].
    pub fn follow(&self, view: ViewId, path: &str) -> Result<Option<File<'_>>, OpError> {
        let Some((node, mode)) = self.tree.resolve(self.open_root(view), path)? else {
            return Ok(None);
        };
        Ok(self.tree.bytes(node)?.map(|bytes| File { id: self.tree.id(node), mode, bytes }))
    }

    /// The content ID of what is at `path` in `view`, a file or a directory.
    pub fn lookup(&self, view: ViewId, path: &str) -> Option<ContentId> {
        self.tree.lookup(self.open_root(view), path).map(|it| self.tree.id(it))
    }

    /// The names in the directory at `path`, sorted, each with whether it is a directory.
    pub fn list(&self, view: ViewId, path: &str) -> Option<Vec<(String, bool)>> {
        let node = self.tree.lookup(self.open_root(view), path)?;
        let entries = self.tree.entries(node)?;
        Some(
            (0..entries.len())
                // alloc: the listing is the output, its names owned (the API's type).
                .map(|at| (self.tree.name_str(entries.get(at).0).to_owned(), entries.is_dir(at)))
                // alloc: the listing is the output, sized by the iterator.
                .collect(),
        )
    }

    /// The files that differ between two roots, in path order, descending only where hashes
    /// differ. Both must be roots this tree has ([`Vfs::has`]): an open view's, a head's.
    pub fn diff(&self, from: ContentId, to: ContentId) -> Vec<Change> {
        let node = |root: ContentId| {
            self.root(root).unwrap_or_else(|| panic!("a diff of {root:x}, which is no root here"))
        };
        self.tree.diff(node(from), node(to))
    }

    /// The directories whose listing differs between two roots (relative to the root, `""` the
    /// root itself), sorted bytewise: a listing is a directory's names and whether each is a
    /// directory, so one differs where a name is added or removed or turns between a file and a
    /// directory, and where the directory itself is added or removed (with every directory
    /// under it). A file whose bytes or mode changed changes no listing. What [`Vfs::diff`]'s
    /// files alone do not say: which directories above an added or removed file it made or
    /// emptied. Both must be roots this tree has, as [`Vfs::diff`]'s.
    pub fn relisted(&self, from: ContentId, to: ContentId) -> Vec<String> {
        let node = |root: ContentId| {
            self.root(root).unwrap_or_else(|| panic!("the listings of {root:x}, which is no root here"))
        };
        self.tree.relisted(node(from), node(to))
    }

    /// Asks for what commits made since the last flush need to be durable.
    pub fn flush(&mut self, effects: &mut Vec<Effect>) {
        self.tree.reclaim();
        self.journal.flush(&self.tree, &self.heads, &self.disks, &self.marks, effects);
    }

    /// Whether a completion is of an effect this asked for.
    pub fn owns(&self, tag: Tag) -> bool {
        self.journal.owns(tag)
    }

    /// Takes a completion of its own, and says which commits are now durable.
    pub fn complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        self.journal.complete(&self.tree, &self.heads, &self.disks, &self.marks, completion, effects)
    }

    /// Drops every node no head, open view or root in `keep` reaches. Readers move to the
    /// nodes kept at once; the old ones are freed once no reader pinned before is still
    /// reading them. Once as much was appended to the journal since its last rewrite as that
    /// rewrite held, the next flush also rewrites it as just what the heads reach.
    pub fn collect(&mut self, keep: &[ContentId]) {
        // alloc: once per collection, the heads' names.
        let names: Vec<Box<str>> = self.heads.keys().cloned().collect();
        let views: Vec<usize> =
            // alloc: once per collection, the open views.
            (0..self.views.len()).filter(|&at| self.views[at].is_some()).collect();
        // alloc: once per collection, the live roots.
        let mut live: Vec<NodeId> = names.iter().map(|name| self.heads[name]).collect();
        live.extend(views.iter().map(|&at| self.views[at].expect("an open view")));
        live.extend(keep.iter().filter_map(|root| self.tree.node(root)));
        let renumbered = self.tree.collect(&mut live);
        let mut live = live.into_iter();
        for name in names {
            self.heads.insert(name, live.next().expect("a head's root"));
        }
        for at in views {
            self.views[at] = live.next();
        }
        self.journal.collected(&renumbered, self.tree.len());
    }

    /// [`Vfs::collect`] once it is paid for: once the tree made at least as many nodes since
    /// the last collection as that one kept. A caller that collects after every job (the
    /// daemon) then walks and copies what is kept a constant number of times per node made,
    /// where `collect` does it every call. Whether it collected.
    pub fn collect_if_due(&mut self, keep: &[ContentId]) -> bool {
        let due = self.tree.collection_due();
        if due {
            self.collect(keep);
        }
        due
    }
}

/// What recovering a journal found.
#[derive(Default, PartialEq, Eq, Debug)]
pub struct Recovered {
    pub records: usize,
    /// Bytes from the first record that was not whole, failed its checksum or was malformed, to
    /// the end: none of them is loaded, and they are cut before the next append.
    pub skipped: usize,
    /// Head records whose root was not loaded, dropped; the journal is then rewritten without
    /// them.
    pub heads_dropped: usize,
    /// What [`Vfs::restore`] did with the image it was given; `None` from [`Vfs::recover`].
    pub image: ImageUse,
    /// `SEEN` records replayed: watcher batches' stamps and rewrites' overlays.
    pub seen: u64,
    /// `MARK` records replayed.
    pub marks: u64,
}

/// A bug planted for a test's control, each reverting one case of the column's invariant
/// ([`Column`]): the planted column pairs its root with a stamp of other bytes.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// A conflict saves the disk's stamp as one that vouches, as before 2026-11-12.
    ConflictVouched,
    /// A file moved from a node the tree no longer has is saved under its stamp, and the record
    /// that it is gone comes apart from the commit's, as before 2026-11-12.
    AgainApart,
    /// A commit by another writer leaves the column as it was, and the column is paired with
    /// the head's root whatever it was kept against, as before 2026-11-12.
    ColumnOfTheHead,
    /// A file renamed from a name the head chose other bytes for (or the column vouches no
    /// root) is taken as a move of the head's bytes, not read, as before 2026-11-12.
    MoveFromChosen,
    /// Any `CACHEDIR.TAG` prunes its directory, its signature unchecked.
    TagUnchecked,
    /// No `CACHEDIR.TAG` prunes its directory: what is under a cache directory is read, as
    /// before 2026-11-13.
    TagIgnored,
    /// A file over [`MOST_FILE_BYTES`] is read, as before 2026-11-13.
    TooLargeRead,
}

#[cfg(test)]
thread_local! {
    pub(crate) static PLANT: std::cell::Cell<Plant> = const { std::cell::Cell::new(Plant::None) };
}

#[cfg(test)]
pub(crate) fn planted(plant: Plant) -> bool {
    PLANT.get() == plant
}

/// Counts each test thread's allocations, for the allocation-budget tests
/// (docs/v2/plan.md).
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

#[cfg(test)]
mod tests;
