//! blaze v2's files (docs/v2/plan/lanes/vfs.md section 2, docs/v2/plan/lanes/core.md section 3): a persistent, content-addressed Merkle
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
//! Unsafe code is only in `store` (docs/v2/plan/design/unsafe.md), whose API is safe.

#![deny(unsafe_code)]

mod disk;
mod image;
mod journal;
pub mod outside;
mod paths;
mod reader;
mod rebase;
mod section;
#[allow(unsafe_code)]
mod store;
mod sync;
mod text;
mod tree;
pub mod watch;

use std::{borrow::Cow, fmt};

use blaze_core::{Completion, Effect, FileName, Tag};
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Idx};
use serde::{Deserialize, Serialize};

use crate::{
    journal::{Journal, Loaded},
    tree::{NodeId, Tree},
};
pub use blaze_image::Corrupt;

pub use crate::{
    image::{Bytes, DisksUnused, ImageError, ImageMark, ImageUse},
    outside::{Cover, Outside, cover, generation, unchanged_since, want},
    reader::{Pinned, Reader},
    section::Section,
    text::{Text, TextFile, text_of},
    tree::Jobs,
    watch::{Watched, Watcher},
};

pub const FILE_DOMAIN: &str = "blaze v2 vfs file 1";
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
    Corrupt { section: u32, group: u32 },
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
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommitError::UnknownBase(root) => write!(f, "no root {root} is known"),
            CommitError::Conflict { path } => write!(f, "{path} changed since the base"),
            CommitError::Op(error) => fmt::Display::fmt(error, f),
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
    heads: HashMap<Box<str>, NodeId>,
    /// By view: its root while it is open. A closed view's number is not given out again.
    views: Vec<Option<NodeId>>,
    journal: Journal,
    /// What each head's watcher last saw of the disk: saved in the image with the tree.
    disks: disk::Disks,
}

impl Vfs {
    /// A VFS recovered from its journal's bytes (empty for a new one), and what it found.
    /// Tags it asks with are `tags..tags + 2^32`.
    pub fn recover(journal: FileName, tags: u64, bytes: &[u8]) -> (Vfs, Recovered) {
        let Loaded { tree, heads, journal, found } = Journal::recover(journal, tags, bytes);
        // alloc: empty, allocates nothing; once per process.
        (Vfs { tree, heads, views: Vec::new(), journal, disks: disk::Disks::default() }, found)
    }

    /// A handle that reads any root this tree has, from any thread, without the `Vfs` and
    /// without a lock: what the query database's `blaze_query::Tree` is over. A root is in it
    /// before any call that gives its ID returns. Memory a collection frees is reclaimed by
    /// the `Vfs` itself once no reader can be in it; no caller has to say when.
    pub fn reader(&self) -> Reader {
        self.tree.reader()
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
            return self.move_head(head, at, false, ops.iter().map(Cow::Borrowed));
        }
        let moved = rebase::rebase(&self.tree, base, at, ops)?;
        let ops =
            ops.iter().zip(moved).map(|(op, moved)| moved.map_or(Cow::Borrowed(op), Cow::Owned));
        self.move_head(head, at, true, ops)
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
            return self.move_head(head, at, false, ops.into_iter().map(Cow::Owned));
        }
        let moved = rebase::rebase(&self.tree, base, at, &ops)?;
        let ops = ops.into_iter().zip(moved).map(|(op, moved)| Cow::Owned(moved.unwrap_or(op)));
        self.move_head(head, at, true, ops)
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
        match self.heads.get_mut(head) {
            Some(node) => *node = root,
            None => {
                self.heads.insert(head.into(), root);
            }
        }
        let ticket = self.journal.record_head(&self.tree, head, root);
        Ok(Commit { root: self.tree.id(root), rebased, ticket })
    }

    /// Moves `head` to `root` whatever it was at, as a checkout, a fork or a push does.
    pub fn set_head(&mut self, head: &str, root: ContentId) -> Result<Ticket, CommitError> {
        let node = self.root(root).ok_or(CommitError::UnknownBase(root))?;
        self.heads.insert(head.into(), node);
        Ok(self.journal.record_head(&self.tree, head, node))
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

    /// Asks for what commits made since the last flush need to be durable.
    pub fn flush(&mut self, effects: &mut Vec<Effect>) {
        self.tree.reclaim();
        self.journal.flush(&self.tree, &self.heads, effects);
    }

    /// Whether a completion is of an effect this asked for.
    pub fn owns(&self, tag: Tag) -> bool {
        self.journal.owns(tag)
    }

    /// Takes a completion of its own, and says which commits are now durable.
    pub fn complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        self.journal.complete(&self.tree, &self.heads, completion, effects)
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
}

/// Counts each test thread's allocations, for the allocation-budget tests
/// (docs/v2/plan/goals/principles.md).
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

#[cfg(test)]
mod tests;
