//! Sans-IO, hardware-floor Git prompt and status engine for Starship-class tools.
//!
//! Everything a shell prompt or status bar needs from a Git checkout — branch, detached HEAD,
//! exact tag, in-progress operation state (rebase/am/merge/cherry-pick/revert/bisect with step
//! counts), upstream ahead/behind counts, conflicted/staged/modified/deleted/typechanged/untracked
//! /stashed counts — computed by a batched sans-IO state machine ([`StatusMachine`]) with zero
//! `std`, zero `alloc` defaults, zero third-party crates, and zero subprocesses.
//!
//! # Architecture
//!
//! - [`index`]: zero-copy `.git/index` parser (v2/v3/v4), stage 1..3 conflicts, full recursive
//!   `TREE` cache-tree extension for O(1) subtree pruning against `HEAD`.
//! - [`refs`]: `HEAD`, loose refs, `packed-refs` (with peeled tags), `.git/config` branch upstream
//!   and status settings, `logs/refs/stash` SIMD counter, and `git_state` detection.
//! - [`ignore`]: `.gitignore` / `info/exclude` matcher with negation, anchoring, `**` globs,
//!   directory-only rules, and per-directory rule caching in the caller's allocator.
//! - [`commit_graph`]: zero-copy binary `commit-graph` (`CGPH` v1) parser (`OIDF`/`OIDL`/`CDAT`/
//!   `EDGE` + split `commit-graph-chain`) for O(log N) parent and generation lookups.
//! - [`objects`]: tree entry parser, cache-tree-pruned `HEAD`-vs-index staged diff, and bounded
//!   symmetric ahead/behind graph distance solver (via `commit-graph` or `blaze-git-fetch::store`).
//! - [`machine`]: batched sans-IO state machine emitting [`Want`] batches (`ReadFiles`, `MapFiles`,
//!   `ListDirs`, `StatPaths`, `ReadLinks`, `ReadRanges`) so a driver like `bstd::git` can execute
//!   one `getattrlistbulk` per directory and verify both tracked and untracked files in one pass.
//! - [`format`]: zero-allocation Starship-style prompt formatter over `core::fmt::Write`.

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod commit_graph;
pub mod format;
pub mod ignore;
pub mod index;
pub mod machine;
pub mod objects;
pub mod plant;
pub mod refs;

#[cfg(test)]
mod tests;

use core::fmt;

pub use blaze_git_fetch::{Kind as ObjectKind, Oid};
use blaze_own::AString;

pub use crate::{
    format::{PromptStyle, format_status},
    machine::{
        DirEntry, DirListing, FileData, Got, IncrementalPlan, RangeData, RangeRead, Snapshot,
        StatEntry, StatusMachine, Step, Want,
    },
    plant::GitStatusPlant,
};

/// Bounds on every table, walk, and object read (rule `bounded-progress`).
///
/// Every field is explicit; [`Limits::SHIPPED`] holds the production limits for real repositories.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Maximum `.git/index` entries accepted.
    pub most_index_entries: u32,
    /// Maximum working-tree directory entries inspected during untracked walk.
    pub most_worktree_entries: u32,
    /// Maximum directory nesting depth during untracked walk and tree diff.
    pub most_dir_depth: u16,
    /// Maximum commits explored when computing ahead/behind divergence.
    pub most_commits: u32,
    /// Maximum inflated bytes of a single git object (commit/tree/blob).
    pub most_object_bytes: u64,
    /// Maximum pack delta chain depth (git's `pack.depth` ceiling is 4095).
    pub most_delta_depth: usize,
    /// Maximum packfiles indexed in `objects/pack`.
    pub most_packs: usize,
    /// Maximum ignore rules parsed across all `.gitignore` / `info/exclude` files.
    pub most_ignore_rules: u32,
    /// Maximum `TREE` cache-tree nodes in `.git/index`.
    pub most_cache_tree_nodes: u32,
}

impl Limits {
    /// Production limits for real repositories (Linux, reth, tempo, blaze).
    pub const SHIPPED: Limits = Limits {
        most_index_entries: 4 << 20,
        most_worktree_entries: 4 << 20,
        most_dir_depth: 128,
        most_commits: 4_096,
        most_object_bytes: 64 << 20,
        most_delta_depth: 4_095,
        most_packs: 1_024,
        most_ignore_rules: 65_536,
        most_cache_tree_nodes: 1 << 20,
    };
}

/// How untracked files in the working tree are scanned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UntrackedMode {
    /// Do not walk for untracked files (`-uno`). Fastest on huge trees with many ignored dirs.
    Off,
    /// Git's default (`-unormal`): collapse an untracked directory with no tracked files below it
    /// into one untracked count once a single non-ignored file is found inside it.
    Normal,
    /// Count every individual non-ignored untracked file (`-uall`).
    All,
}

/// How much detail the status machine computes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Detail {
    /// Compute exact counts for every category (Starship default with counts enabled).
    Counts,
    /// Stop hashing/walking a category as soon as its count reaches 1 (presence-only prompt).
    Presence,
}

/// Whether ahead/behind divergence against upstream is computed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AheadBehindMode {
    /// Compute ahead/behind counts against the branch's configured upstream.
    Enabled,
    /// Skip upstream graph traversal (still reports branch and upstream name).
    Skipped,
}

/// Whether an exact tag pointing at `HEAD` is resolved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TagMode {
    /// Resolve an exact tag pointing at `HEAD` only when `HEAD` is detached (Starship default:
    /// zero extra IO on attached branches).
    DetachedOnly,
    /// Always resolve an exact tag pointing at `HEAD` (peeling annotated tags when needed).
    Always,
    /// Do not scan tags.
    Skipped,
}

/// Options for [`StatusMachine::new`] (rule `options-struct`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StatusOpts {
    pub untracked: UntrackedMode,
    pub detail: Detail,
    pub ahead_behind: AheadBehindMode,
    pub tag: TagMode,
    pub short_oid_len: u8,
    pub limits: Limits,
}

impl StatusOpts {
    /// Starship's standard full prompt configuration.
    pub const STARSHIP: StatusOpts = StatusOpts {
        untracked: UntrackedMode::Normal,
        detail: Detail::Counts,
        ahead_behind: AheadBehindMode::Enabled,
        tag: TagMode::DetachedOnly,
        short_oid_len: 7,
        limits: Limits::SHIPPED,
    };

    /// Ultra-fast presence-only prompt configuration (stops each category at first finding).
    pub const FAST_PRESENCE: StatusOpts = StatusOpts {
        untracked: UntrackedMode::Normal,
        detail: Detail::Presence,
        ahead_behind: AheadBehindMode::Enabled,
        tag: TagMode::Skipped,
        short_oid_len: 7,
        limits: Limits::SHIPPED,
    };
}

/// Kind of filesystem entry observed by the driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
    Gone,
}

/// Filesystem metadata of one path, without following symlinks (`lstat` / `getattrlistbulk`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileStat {
    pub kind: FileKind,
    pub size: u64,
    /// Content modification time `(seconds, nanoseconds)` since Unix epoch.
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    /// Inode metadata change time `(seconds, nanoseconds)` since Unix epoch.
    pub ctime_sec: i64,
    pub ctime_nsec: u32,
    pub ino: u64,
    pub dev: u64,
    pub mode: u32,
}

impl FileStat {
    pub const GONE: FileStat = FileStat {
        kind: FileKind::Gone,
        size: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        ctime_sec: 0,
        ctime_nsec: 0,
        ino: 0,
        dev: 0,
        mode: 0,
    };

    /// Whether any execute permission bit is set on a regular file.
    #[inline]
    pub const fn executable(self) -> bool {
        self.mode & 0o111 != 0
    }

    /// Construct from nanosecond timestamps (as `blaze-core` / `bstd::fs::Stamp` provides).
    #[inline]
    pub const fn from_ns(
        kind: FileKind,
        size: u64,
        mtime_ns: u64,
        ctime_ns: u64,
        ino: u64,
        dev: u64,
        mode: u32,
    ) -> FileStat {
        FileStat {
            kind,
            size,
            mtime_sec: (mtime_ns / 1_000_000_000) as i64,
            mtime_nsec: (mtime_ns % 1_000_000_000) as u32,
            ctime_sec: (ctime_ns / 1_000_000_000) as i64,
            ctime_nsec: (ctime_ns % 1_000_000_000) as u32,
            ino,
            dev,
            mode,
        }
    }
}

/// What kind of git checkout was opened.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WorktreeKind {
    Normal,
    LinkedWorktree,
    Bare,
}

/// Abbreviated lower-case hex prefix of an [`Oid`], stored inline with zero heap allocation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ShortOid {
    buf: [u8; 40],
    len: u8,
}

impl ShortOid {
    pub fn from_oid(oid: Oid, len: u8) -> ShortOid {
        let clamped = len.clamp(4, 40);
        ShortOid { buf: oid.hex(), len: clamped }
    }

    pub fn as_str(&self) -> &str {
        // Hex digits from `Oid::hex` are guaranteed ASCII.
        core::str::from_utf8(&self.buf[..usize::from(self.len)]).expect("ascii hex")
    }
}

impl fmt::Display for ShortOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for ShortOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ShortOid({})", self.as_str())
    }
}

/// What `HEAD` currently points to.
#[derive(PartialEq, Eq, Debug)]
pub enum Head<'a> {
    /// Attached to a local branch (`refs/heads/<name>`). `oid` is `None` for an unborn branch.
    Branch { name: AString<'a>, oid: Option<Oid>, short: Option<ShortOid> },
    /// Detached at a concrete commit.
    Detached { oid: Oid, short: ShortOid },
}

impl Head<'_> {
    pub fn oid(&self) -> Option<Oid> {
        match self {
            Head::Branch { oid, .. } => *oid,
            Head::Detached { oid, .. } => Some(*oid),
        }
    }

    pub fn branch_name(&self) -> Option<&str> {
        match self {
            Head::Branch { name, .. } => Some(name.as_str()),
            Head::Detached { .. } => None,
        }
    }
}

/// In-progress multi-step Git operation (`git_state` module in Starship).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RepoState {
    Clean,
    Rebase { interactive: bool, step: Option<u32>, total: Option<u32> },
    Am { step: Option<u32>, total: Option<u32> },
    AmOrRebase { step: Option<u32>, total: Option<u32> },
    Merge,
    CherryPick,
    Revert,
    Bisect,
}

impl RepoState {
    pub const fn label(self) -> Option<&'static str> {
        match self {
            RepoState::Clean => None,
            RepoState::Rebase { interactive: true, .. } => Some("REBASING"),
            RepoState::Rebase { interactive: false, .. } => Some("REBASING"),
            RepoState::Am { .. } => Some("AM"),
            RepoState::AmOrRebase { .. } => Some("AM/REBASE"),
            RepoState::Merge => Some("MERGING"),
            RepoState::CherryPick => Some("CHERRY-PICKING"),
            RepoState::Revert => Some("REVERTING"),
            RepoState::Bisect => Some("BISECTING"),
        }
    }
}

/// Ahead/behind commit divergence between `HEAD` and its upstream tracking branch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Divergence {
    /// `HEAD` and upstream point to the same commit (`ahead = 0, behind = 0`).
    Equal,
    /// Exact symmetric commit-graph counts.
    Counted { ahead: u32, behind: u32 },
    /// Walk hit [`Limits::most_commits`]; lower bounds are reported truthfully.
    Capped { ahead_at_least: u32, behind_at_least: u32 },
    /// `HEAD` is unborn (no commits yet).
    Unborn,
    /// Upstream ref does not exist locally (e.g. not yet pushed/fetched).
    UpstreamMissing,
    /// Divergence check was skipped by [`AheadBehindMode::Skipped`].
    Skipped,
}

impl Divergence {
    pub const fn ahead(self) -> u32 {
        match self {
            Divergence::Counted { ahead, .. } => ahead,
            Divergence::Capped { ahead_at_least, .. } => ahead_at_least,
            _ => 0,
        }
    }

    pub const fn behind(self) -> u32 {
        match self {
            Divergence::Counted { behind, .. } => behind,
            Divergence::Capped { behind_at_least, .. } => behind_at_least,
            _ => 0,
        }
    }
}

/// Configured upstream tracking branch and its divergence from `HEAD`.
#[derive(PartialEq, Eq, Debug)]
pub struct Upstream<'a> {
    pub remote: AString<'a>,
    pub branch: AString<'a>,
    pub oid: Option<Oid>,
    pub divergence: Divergence,
}

/// Which bounded walks hit their [`Limits`] cap (never silently dropped).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Truncated {
    pub untracked_capped: bool,
    pub commits_capped: bool,
    pub ignore_rules_capped: bool,
}

/// Working-tree, index, and stash counts for Starship's `git_status` module.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StatusCounts {
    /// Distinct unmerged paths (index stages 1, 2, or 3).
    pub conflicted: u32,
    /// New files staged in index not present in `HEAD` tree (including intent-to-add excluded).
    pub staged_added: u32,
    /// Files modified in index relative to `HEAD` tree.
    pub staged_modified: u32,
    /// Files deleted in index relative to `HEAD` tree.
    pub staged_deleted: u32,
    /// Files whose mode/kind changed in index relative to `HEAD` tree (e.g. file <-> symlink).
    pub staged_typechanged: u32,
    /// Tracked files modified in the working tree relative to index.
    pub modified: u32,
    /// Tracked files missing from the working tree.
    pub deleted: u32,
    /// Tracked files whose kind changed in the working tree (file <-> symlink/dir).
    pub typechanged: u32,
    /// Non-ignored untracked files (or collapsed directories in [`UntrackedMode::Normal`]).
    pub untracked: u32,
    /// Stash entries in `logs/refs/stash`.
    pub stashed: u32,
}

impl StatusCounts {
    /// Total staged changes (`+`).
    #[inline]
    pub const fn staged(self) -> u32 {
        self.staged_added + self.staged_modified + self.staged_deleted + self.staged_typechanged
    }

    /// Whether the working tree and index have any dirty state (ignoring stashes).
    #[inline]
    pub const fn is_dirty(self) -> bool {
        self.conflicted > 0
            || self.staged() > 0
            || self.modified > 0
            || self.deleted > 0
            || self.typechanged > 0
            || self.untracked > 0
    }
}

/// Category of a changed file reported in [`PromptStatus::changed_samples`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChangeKind {
    Modified,
    Untracked,
    Deleted,
    Staged,
    Conflicted,
}

/// One bounded sample of an uncommitted file in the working tree or index.
#[derive(PartialEq, Eq, Debug)]
pub struct ChangedSample<'a> {
    pub path: AString<'a>,
    pub kind: ChangeKind,
}

/// Complete Starship-class summary of a Git repository snapshot.
#[derive(PartialEq, Eq, Debug)]
pub struct PromptStatus<'a> {
    pub head: Head<'a>,
    pub tag: Option<AString<'a>>,
    pub state: RepoState,
    pub upstream: Option<Upstream<'a>>,
    pub counts: StatusCounts,
    pub worktree: WorktreeKind,
    pub truncated: Truncated,
    /// Up to 24 representative uncommitted file paths (modified, untracked, deleted, conflicted).
    pub changed_samples: blaze_own::AVec<'a, ChangedSample<'a>>,
}

/// A typed refusal naming the git file, byte offset (when applicable), and rule violated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GitError {
    pub file: GitFile,
    pub at: u64,
    pub kind: ErrorKind,
}

/// Which git metadata or repository file an error refers to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GitFile {
    GitPointer,
    CommonDir,
    Head,
    Ref,
    PackedRefs,
    Config,
    Index,
    CommitGraph,
    LooseObject(Oid),
    PackIndex(Oid),
    PackData(Oid),
    TreeObject(Oid),
    CommitObject(Oid),
    Worktree,
}

/// Specific cause of a [`GitError`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ErrorKind {
    Missing,
    Malformed,
    UnsupportedVersion(u32),
    TooManyEntries { count: u32, most: u32 },
    SplitIndexUnsupported,
    DepthExceeded { most: u16 },
    ObjectStore(blaze_git_fetch::store::StoreWhy),
    CommitParse(blaze_git_fetch::store::CommitWhy),
    ObjectMissing(Oid),
    WrongObjectKind { want: ObjectKind, got: ObjectKind },
    RefCycle,
    UnexpectedReply,
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blaze-git {:?} at byte {}: {:?}", self.file, self.at, self.kind)
    }
}
