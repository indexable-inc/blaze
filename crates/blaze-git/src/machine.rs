//! Batched sans-IO state machine ([`StatusMachine`]) for Starship-class Git status.
//!
//! The machine never touches the filesystem, clock, or environment. Instead, [`StatusMachine::poll`]
//! returns either [`Step::Need`] with a batched [`Want`] request or [`Step::Done`] with the final
//! [`PromptStatus`]. The driver executes each batch (e.g. `bstd::fs::Dir::read_all_many`,
//! `Dir::map_read`, `Dir::entries` via `getattrlistbulk`, `Dir::facts_of`, `Dir::read_link`) and
//! passes the reply into [`StatusMachine::feed`].

#![blaze::sans_io]

use blaze_git_fetch::{
    Kind as ObjectKind, Oid,
    store::{self, Base, Commit, Idx},
};
use blaze_hash::{BuildKeyHasher, Map, MapOpts};
use blaze_memchr::memchr;
use blaze_own::{AString, AVec, Alloc, String, Vec, planted::Planted};

use crate::{
    AheadBehindMode, ChangeKind, ChangedSample, Detail, Divergence, ErrorKind, FileKind, FileStat,
    GitError, GitFile, GitStatusPlant, Head, PromptStatus, RepoState, ShortOid, StatusCounts,
    StatusOpts, TagMode, Truncated, UntrackedMode, Upstream, WorktreeKind,
    commit_graph::{CommitGraph, parse_graph_chain},
    ignore::IgnoreStack,
    index::{IndexEntry, IndexView, OwnedIndex},
    objects::{AheadBehindWalker, CommitInfo, StagedDiff, WalkStep},
    refs::{
        AutoCrlf, HeadFile, RepoConfig, StateFiles, UpstreamRef, count_stashes, detect_repo_state,
        find_packed_tag_for_oid, lookup_packed_ref, parse_config, parse_head, parse_loose_ref,
    },
};

const MAX_CHANGED_SAMPLES: usize = 24;

/// Which capability directory a requested relative path belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathScope {
    /// Relative to the working tree root (e.g. `src/lib.rs`, `.gitignore`, `""` for root).
    Worktree,
    /// Relative to the per-worktree git directory (e.g. `HEAD`, `index`, `MERGE_HEAD`).
    GitDir,
    /// Relative to the common git directory (e.g. `config`, `packed-refs`, `objects/...`).
    CommonDir,
}

/// A relative path scoped to one of the repository's three roots.
#[derive(PartialEq, Eq, Debug)]
pub struct ScopedPath<'a> {
    pub scope: PathScope,
    pub path: AString<'a>,
}

impl<'a> ScopedPath<'a> {
    pub fn new(scope: PathScope, path: &str, alloc: Alloc<'a>) -> ScopedPath<'a> {
        ScopedPath { scope, path: String::from_str_in(path, alloc) }
    }
}

/// One byte-range read request (used for packed git objects inside `.pack` files).
#[derive(PartialEq, Eq, Debug)]
pub struct RangeRead<'a> {
    pub scope: PathScope,
    pub path: AString<'a>,
    pub offset: u64,
    pub len: u64,
}

/// A batched IO request emitted by [`StatusMachine::poll`].
#[derive(Debug)]
pub enum Want<'a> {
    /// Read small files whole (each missing file answered as `None`).
    ReadFiles(AVec<'a, ScopedPath<'a>>),
    /// Read/map binary files along with their `FileStat` (`.git/index`, `commit-graph`, pack `.idx`).
    ReadWithStat(AVec<'a, ScopedPath<'a>>),
    /// List directories (each returning sorted entries with full `FileStat`, e.g. `getattrlistbulk`).
    ListDirs(AVec<'a, ScopedPath<'a>>),
    /// Batch `lstat` paths without following symlinks.
    StatPaths(AVec<'a, ScopedPath<'a>>),
    /// Batch `readlink` targets without following symlinks.
    ReadLinks(AVec<'a, ScopedPath<'a>>),
    /// Batch byte-range reads from files.
    ReadRanges(AVec<'a, RangeRead<'a>>),
}

/// One entry span inside a flat packed [`DirListing`].
#[derive(Clone, Copy, Debug)]
pub struct DirEntry {
    pub name_start: u32,
    pub name_len: u32,
    pub stat: FileStat,
}

/// Flat packed directory listing (one byte buffer for all names + one array of [`DirEntry`] spans,
/// sorted by entry name bytes): zero per-entry heap allocations.
#[derive(Debug)]
pub struct DirListing<'a> {
    pub names: AVec<'a, u8>,
    pub entries: AVec<'a, DirEntry>,
}

impl<'a> DirListing<'a> {
    pub fn with_capacity(entry_cap: usize, name_bytes_cap: usize, alloc: Alloc<'a>) -> DirListing<'a> {
        DirListing {
            names: Vec::with_capacity_in(name_bytes_cap, alloc),
            entries: Vec::with_capacity_in(entry_cap, alloc),
        }
    }

    pub fn push(&mut self, name: &str, stat: FileStat) {
        let name_start = self.names.len() as u32;
        self.names.extend_from_slice(name.as_bytes());
        let name_len = name.len() as u32;
        self.entries.push(DirEntry { name_start, name_len, stat });
    }

    #[inline]
    pub fn name_bytes(&self, entry: &DirEntry) -> &[u8] {
        let s = entry.name_start as usize;
        &self.names[s..s + entry.name_len as usize]
    }

    #[inline]
    pub fn name_str(&self, entry: &DirEntry) -> &str {
        core::str::from_utf8(self.name_bytes(entry)).unwrap_or("")
    }

    #[inline]
    pub fn has_name(&self, needle: &[u8]) -> bool {
        self.entries.binary_search_by(|e| self.name_bytes(e).cmp(needle)).is_ok()
    }
}

/// One file's bytes (if present) and optional stat.
#[derive(Debug)]
pub struct FileData<'a> {
    pub bytes: Option<AVec<'a, u8>>,
    pub stat: Option<FileStat>,
}

/// One stat reply.
#[derive(Clone, Copy, Debug)]
pub struct StatEntry {
    pub stat: FileStat,
}

/// One byte-range reply.
#[derive(Debug)]
pub struct RangeData<'a> {
    pub bytes: Option<AVec<'a, u8>>,
}

/// Driver reply matching the last [`Want`] emitted by [`StatusMachine::poll`].
#[derive(Debug)]
pub enum Got<'a> {
    Files(AVec<'a, Option<AVec<'a, u8>>>),
    FilesWithStat(AVec<'a, FileData<'a>>),
    Dirs(AVec<'a, Option<DirListing<'a>>>),
    Stats(AVec<'a, StatEntry>),
    Links(AVec<'a, Option<AVec<'a, u8>>>),
    Ranges(AVec<'a, RangeData<'a>>),
}

/// Result of one [`StatusMachine::poll`] call.
pub enum Step<'a> {
    Need(Want<'a>),
    Done(PromptStatus<'a>),
}

// ---------------------------------------------------------------------------
// Incremental snapshot helper (for FS journal drivers like `Dir::changes_since`)
// ---------------------------------------------------------------------------

/// Decision returned by [`IncrementalPlan::decide`] when given a list of changed worktree paths.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IncrementalPlan {
    /// No relevant path changed; the previous [`PromptStatus`] is still exact.
    Reuse,
    /// Only ordinary working-tree files changed (no `.git`, no `.gitignore`, no `.gitattributes`).
    WorktreeOnly,
    /// Repository metadata or ignore rules changed; run a fresh [`StatusMachine`].
    Full(FullReason),
}

/// Why an incremental check requires a full status run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FullReason {
    GitMetadataChanged,
    IgnoreOrAttributesChanged,
    RootInvalidated,
    BareOrLinkedOutside,
}

/// Lightweight fingerprint of a completed status run for incremental journal checks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Snapshot {
    pub head_oid: Option<Oid>,
    pub worktree: WorktreeKind,
    pub counts: StatusCounts,
    pub state: RepoState,
}

impl IncrementalPlan {
    /// Decide whether a journal's `changed_paths` (worktree-relative, `/`-separated) invalidate
    /// `prev` or allow reusing it directly.
    pub fn decide(prev: &Snapshot, changed_paths: &[&str]) -> IncrementalPlan {
        if prev.worktree != WorktreeKind::Normal {
            return IncrementalPlan::Full(FullReason::BareOrLinkedOutside);
        }
        if changed_paths.is_empty() {
            return IncrementalPlan::Reuse;
        }
        for &rel in changed_paths {
            if rel.is_empty() {
                return IncrementalPlan::Full(FullReason::RootInvalidated);
            }
            if rel == ".git" || rel.starts_with(".git/") || rel.split('/').any(|p| p == ".git") {
                return IncrementalPlan::Full(FullReason::GitMetadataChanged);
            }
            let base = rel.rsplit('/').next().unwrap_or(rel);
            if base == ".gitignore" || base == ".gitattributes" {
                return IncrementalPlan::Full(FullReason::IgnoreOrAttributesChanged);
            }
        }
        IncrementalPlan::WorktreeOnly
    }
}

// ---------------------------------------------------------------------------
// Internal state machine
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Boot,
    ReadBinaryMeta,
    ResolveRefs,
    ScanTags,
    WorktreeStart,
    WalkDirs,
    WalkReadIgnores,
    StatRemaining,
    HashFiles,
    HashLinks,
    ObjectsStart,
    ObjectsReadLoose,
    ObjectsListPacks,
    ObjectsReadPackIdx,
    ObjectsReadPackRanges,
    Finish,
    Done,
}

/// A directory queued in the untracked wavefront walk, carrying its exact sorted slice
/// `[index_start..index_end)` in `.git/index` so entries are merged in O(1) with zero binary search.
struct WalkDir<'a> {
    path: AString<'a>,
    depth: u16,
    index_start: usize,
    index_end: usize,
    /// When `Some(group_id)`, this directory is being probed only to discover if an untracked
    /// collapsed directory contains at least one non-ignored file.
    collapse_group: Option<u32>,
}

/// One packed object read currently in flight (may require resolving a delta chain).
struct PendingPackObj<'a> {
    want_oid: Oid,
    pack_idx: usize,
    offset: u64,
    /// Accumulated delta frames to apply once a whole base is decoded: innermost base last.
    deltas: AVec<'a, AVec<'a, u8>>,
    depth: usize,
}

/// Batched sans-IO Git prompt and status state machine.
pub struct StatusMachine<'a, P: Planted<GitStatusPlant>> {
    opts: StatusOpts,
    worktree_kind: WorktreeKind,
    alloc: Alloc<'a>,
    plant: P,
    phase: Phase,

    // Parsed control data from Phase Boot.
    head_raw: Option<AVec<'a, u8>>,
    config_raw: Option<AVec<'a, u8>>,
    packed_refs_raw: Option<AVec<'a, u8>>,
    stash_log_raw: Option<AVec<'a, u8>>,
    info_exclude_raw: Option<AVec<'a, u8>>,
    graph_chain_raw: Option<AVec<'a, u8>>,
    repo_state: RepoState,

    // Resolved repo metadata.
    branch_name: Option<AString<'a>>,
    head_sym_ref: Option<AString<'a>>,
    head_oid: Option<Oid>,
    ref_follow_depth: u8,
    config: RepoConfig<'a>,
    upstream_ref: Option<UpstreamRef<'a>>,
    upstream_oid: Option<Oid>,
    tag: Option<AString<'a>>,

    // Binary metadata kept in memory for the duration of the run (index parsed ONCE!).
    index: Option<OwnedIndex<'a>>,
    index_mtime: (i64, u32),
    graph_layers_raw: AVec<'a, AVec<'a, u8>>,
    graph_layer_oids: AVec<'a, Oid>,

    // Working-tree status tracking.
    counts: StatusCounts,
    changed_samples: AVec<'a, ChangedSample<'a>>,
    truncated: Truncated,
    ignores: IgnoreStack<'a>,

    // Unified single-pass directory walk state.
    /// Per stage-0 index entry: whether its worktree stat was already verified via `ListDirs`.
    verified_entries: AVec<'a, bool>,
    /// Current wave of directories to list.
    walk_queue: AVec<'a, WalkDir<'a>>,
    /// Listings buffered while waiting for newly discovered `.gitignore` files in those directories.
    buffered_dirs: AVec<'a, (WalkDir<'a>, DirListing<'a>)>,
    /// Directories whose `.gitignore` was just requested.
    pending_ignore_dirs: AVec<'a, AString<'a>>,
    /// Next collapse group id and whether each collapse group has already found an untracked file.
    collapse_found: AVec<'a, bool>,
    worktree_entries_seen: u32,
    /// Reusable scratch path buffer for ignore checks (zero per-entry allocations).
    child_scratch: AString<'a>,
    /// Indices into `ParsedIndex::entries` waiting for `StatPaths`.
    stat_pending_indices: AVec<'a, usize>,
    /// Indices into `ParsedIndex::entries` waiting for content hashing (`ReadFiles` / `ReadLinks`).
    hash_file_indices: AVec<'a, usize>,
    hash_link_indices: AVec<'a, usize>,

    // Object store / staged diff / ahead-behind state.
    head_tree_oid: Option<Oid>,
    staged_diff: Option<StagedDiff<'a>>,
    ahead_behind: Option<AheadBehindWalker<'a>>,
    divergence: Divergence,
    /// Decoded objects cached for this single run: `oid -> (kind, body)`.
    objects: Map<Oid, (ObjectKind, AVec<'a, u8>), BuildKeyHasher, Alloc<'a>>,
    /// OIDs currently being fetched from loose/pack store.
    fetching_oids: AVec<'a, Oid>,
    /// Discovered packfile OIDs in `objects/pack`.
    packs_listed: bool,
    pack_oids: AVec<'a, Oid>,
    pack_idx_bytes: AVec<'a, AVec<'a, u8>>,
    pack_sizes: AVec<'a, u64>,
    pending_pack_objs: AVec<'a, PendingPackObj<'a>>,
    /// Pending pack range reads corresponding 1-to-1 with `pending_pack_objs`.
    pack_range_map: AVec<'a, usize>,
}

const CONTROL_FILES: [(PathScope, &str); 18] = [
    (PathScope::GitDir, "HEAD"),
    (PathScope::CommonDir, "config"),
    (PathScope::CommonDir, "packed-refs"),
    (PathScope::CommonDir, "logs/refs/stash"),
    (PathScope::CommonDir, "info/exclude"),
    (PathScope::CommonDir, "objects/info/commit-graphs/commit-graph-chain"),
    (PathScope::GitDir, "rebase-merge/msgnum"),
    (PathScope::GitDir, "rebase-merge/end"),
    (PathScope::GitDir, "rebase-merge/interactive"),
    (PathScope::GitDir, "rebase-apply/rebasing"),
    (PathScope::GitDir, "rebase-apply/applying"),
    (PathScope::GitDir, "rebase-apply/next"),
    (PathScope::GitDir, "rebase-apply/last"),
    (PathScope::GitDir, "MERGE_HEAD"),
    (PathScope::GitDir, "CHERRY_PICK_HEAD"),
    (PathScope::GitDir, "REVERT_HEAD"),
    (PathScope::GitDir, "BISECT_LOG"),
    (PathScope::Worktree, ".gitignore"),
];

impl<'a, P: Planted<GitStatusPlant>> StatusMachine<'a, P> {
    /// Start a new sans-IO status computation with explicit options, worktree kind, allocator,
    /// and planted-bug controller (`NoPlant` in production).
    pub fn new(
        opts: StatusOpts,
        worktree_kind: WorktreeKind,
        alloc: Alloc<'a>,
        plant: P,
    ) -> StatusMachine<'a, P> {
        StatusMachine {
            opts,
            worktree_kind,
            alloc,
            plant,
            phase: Phase::Boot,
            head_raw: None,
            config_raw: None,
            packed_refs_raw: None,
            stash_log_raw: None,
            info_exclude_raw: None,
            graph_chain_raw: None,
            repo_state: RepoState::Clean,
            branch_name: None,
            head_sym_ref: None,
            head_oid: None,
            ref_follow_depth: 0,
            config: RepoConfig::default_config(),
            upstream_ref: None,
            upstream_oid: None,
            tag: None,
            index: None,
            index_mtime: (0, 0),
            graph_layers_raw: Vec::new_in(alloc),
            graph_layer_oids: Vec::new_in(alloc),
            counts: StatusCounts::default(),
            changed_samples: Vec::new_in(alloc),
            truncated: Truncated::default(),
            ignores: IgnoreStack::new(opts.limits, alloc),
            verified_entries: Vec::new_in(alloc),
            walk_queue: Vec::new_in(alloc),
            buffered_dirs: Vec::new_in(alloc),
            pending_ignore_dirs: Vec::new_in(alloc),
            collapse_found: Vec::new_in(alloc),
            worktree_entries_seen: 0,
            child_scratch: String::with_capacity_in(256, alloc),
            stat_pending_indices: Vec::new_in(alloc),
            hash_file_indices: Vec::new_in(alloc),
            hash_link_indices: Vec::new_in(alloc),
            head_tree_oid: None,
            staged_diff: None,
            ahead_behind: None,
            divergence: Divergence::Skipped,
            objects: Map::new(MapOpts {
                hasher: BuildKeyHasher::default(),
                alloc,
                capacity: Some(32),
            }),
            fetching_oids: Vec::new_in(alloc),
            packs_listed: false,
            pack_oids: Vec::new_in(alloc),
            pack_idx_bytes: Vec::new_in(alloc),
            pack_sizes: Vec::new_in(alloc),
            pending_pack_objs: Vec::new_in(alloc),
            pack_range_map: Vec::new_in(alloc),
        }
    }

    /// Supply optional global ignore file bytes (e.g. `~/.config/git/ignore`), keeping the core
    /// strictly sans-IO.
    pub fn set_global_exclude(&mut self, bytes: &[u8]) {
        self.ignores.set_global_exclude(bytes, self.plant);
    }

    /// Advance the state machine until it needs an IO batch or finishes.
    pub fn poll(&mut self) -> Result<Step<'a>, GitError> {
        loop {
            match self.phase {
                Phase::Boot => {
                    let count = if self.worktree_kind == WorktreeKind::Bare { 17 } else { 18 };
                    let mut paths = Vec::with_capacity_in(count, self.alloc);
                    for &(scope, rel) in &CONTROL_FILES[..count] {
                        paths.push(ScopedPath::new(scope, rel, self.alloc));
                    }
                    return Ok(Step::Need(Want::ReadFiles(paths)));
                }
                Phase::ReadBinaryMeta => {
                    let mut paths = Vec::new_in(self.alloc);
                    if self.worktree_kind != WorktreeKind::Bare {
                        paths.push(ScopedPath::new(PathScope::GitDir, "index", self.alloc));
                    }
                    if !self.graph_layer_oids.is_empty() {
                        for oid in self.graph_layer_oids.iter() {
                            let hex = oid.hex();
                            let mut p =
                                String::from_str_in("objects/info/commit-graphs/graph-", self.alloc);
                            p.push_str(core::str::from_utf8(&hex).expect("hex"));
                            p.push_str(".graph");
                            paths.push(ScopedPath { scope: PathScope::CommonDir, path: p });
                        }
                    } else {
                        paths.push(ScopedPath::new(
                            PathScope::CommonDir,
                            "objects/info/commit-graph",
                            self.alloc,
                        ));
                    }
                    return Ok(Step::Need(Want::ReadWithStat(paths)));
                }
                Phase::ResolveRefs => {
                    let mut paths = Vec::new_in(self.alloc);
                    if let Some(sym) = &self.head_sym_ref {
                        paths.push(ScopedPath::new(PathScope::CommonDir, sym.as_str(), self.alloc));
                    }
                    if self.upstream_oid.is_none()
                        && let Some(up) = &self.upstream_ref
                    {
                        paths.push(ScopedPath::new(
                            PathScope::CommonDir,
                            up.ref_path.as_str(),
                            self.alloc,
                        ));
                    }
                    if paths.is_empty() {
                        self.phase = Phase::ScanTags;
                        continue;
                    }
                    return Ok(Step::Need(Want::ReadFiles(paths)));
                }
                Phase::ScanTags => {
                    let want_tag = match self.opts.tag {
                        TagMode::Always => true,
                        TagMode::DetachedOnly => self.branch_name.is_none(),
                        TagMode::Skipped => false,
                    };
                    if want_tag
                        && let Some(oid) = self.head_oid
                        && let Some(packed) = &self.packed_refs_raw
                        && let Some(found) = find_packed_tag_for_oid(packed, oid)
                    {
                        self.tag = Some(String::from_str_in(found, self.alloc));
                    }
                    self.phase = Phase::WorktreeStart;
                }
                Phase::WorktreeStart => {
                    if self.worktree_kind == WorktreeKind::Bare {
                        self.phase = Phase::ObjectsStart;
                        continue;
                    }
                    let Some(idx) = self.index.take() else {
                        // Fresh `git init` has no `.git/index` yet! Walk for untracked files only.
                        if self.opts.untracked != UntrackedMode::Off {
                            self.walk_queue.push(WalkDir {
                                path: String::new_in(self.alloc),
                                depth: 0,
                                index_start: 0,
                                index_end: 0,
                                collapse_group: None,
                            });
                            self.phase = Phase::WalkDirs;
                        } else {
                            self.phase = Phase::ObjectsStart;
                        }
                        continue;
                    };
                    self.counts.conflicted = idx.conflicted_count;
                    let n = idx.entries.len();
                    self.verified_entries = Vec::with_capacity_in(n, self.alloc);
                    for _ in 0..n {
                        self.verified_entries.push(false);
                    }

                    if self.opts.untracked == UntrackedMode::Off {
                        self.queue_unverified_stats(idx.view());
                        self.phase = Phase::StatRemaining;
                    } else {
                        self.walk_queue.push(WalkDir {
                            path: String::new_in(self.alloc),
                            depth: 0,
                            index_start: 0,
                            index_end: n,
                            collapse_group: None,
                        });
                        self.phase = Phase::WalkDirs;
                    }
                    self.index = Some(idx);
                }
                Phase::WalkDirs => {
                    if self.walk_queue.is_empty() {
                        // Wavefront complete! Sweep any remaining unverified index entries.
                        self.prepare_remaining_stats()?;
                        self.phase = Phase::StatRemaining;
                        continue;
                    }
                    let mut dirs = Vec::with_capacity_in(self.walk_queue.len(), self.alloc);
                    for wd in self.walk_queue.iter() {
                        dirs.push(ScopedPath::new(PathScope::Worktree, wd.path.as_str(), self.alloc));
                    }
                    return Ok(Step::Need(Want::ListDirs(dirs)));
                }
                Phase::WalkReadIgnores => {
                    let mut files = Vec::with_capacity_in(self.pending_ignore_dirs.len(), self.alloc);
                    for dir in self.pending_ignore_dirs.iter() {
                        let mut p = String::from_str_in(dir.as_str(), self.alloc);
                        if !p.is_empty() {
                            p.push('/');
                        }
                        p.push_str(".gitignore");
                        files.push(ScopedPath { scope: PathScope::Worktree, path: p });
                    }
                    return Ok(Step::Need(Want::ReadFiles(files)));
                }
                Phase::StatRemaining => {
                    if self.stat_pending_indices.is_empty() {
                        self.phase = Phase::HashFiles;
                        continue;
                    }
                    let idx_view = self.index.as_ref().expect("index present").view();
                    let mut paths = Vec::with_capacity_in(self.stat_pending_indices.len(), self.alloc);
                    for &idx in self.stat_pending_indices.iter() {
                        let p = idx_view.path_str(&idx_view.entries[idx]).unwrap_or("");
                        paths.push(ScopedPath::new(PathScope::Worktree, p, self.alloc));
                    }
                    return Ok(Step::Need(Want::StatPaths(paths)));
                }
                Phase::HashFiles => {
                    if self.hash_file_indices.is_empty()
                        || (self.opts.detail == Detail::Presence && self.counts.modified > 0)
                    {
                        self.hash_file_indices.clear();
                        self.phase = Phase::HashLinks;
                        continue;
                    }
                    let idx_view = self.index.as_ref().expect("index present").view();
                    let mut paths = Vec::with_capacity_in(self.hash_file_indices.len(), self.alloc);
                    for &idx in self.hash_file_indices.iter() {
                        let p = idx_view.path_str(&idx_view.entries[idx]).unwrap_or("");
                        paths.push(ScopedPath::new(PathScope::Worktree, p, self.alloc));
                    }
                    return Ok(Step::Need(Want::ReadFiles(paths)));
                }
                Phase::HashLinks => {
                    if self.hash_link_indices.is_empty()
                        || (self.opts.detail == Detail::Presence && self.counts.modified > 0)
                    {
                        self.hash_link_indices.clear();
                        self.phase = Phase::ObjectsStart;
                        continue;
                    }
                    let idx_view = self.index.as_ref().expect("index present").view();
                    let mut paths = Vec::with_capacity_in(self.hash_link_indices.len(), self.alloc);
                    for &idx in self.hash_link_indices.iter() {
                        let p = idx_view.path_str(&idx_view.entries[idx]).unwrap_or("");
                        paths.push(ScopedPath::new(PathScope::Worktree, p, self.alloc));
                    }
                    return Ok(Step::Need(Want::ReadLinks(paths)));
                }
                Phase::ObjectsStart => {
                    if let Some(want) = self.drive_objects_phase()? {
                        return Ok(Step::Need(want));
                    }
                    self.phase = Phase::Finish;
                }
                Phase::ObjectsReadLoose => {
                    let mut paths = Vec::with_capacity_in(self.fetching_oids.len(), self.alloc);
                    for oid in self.fetching_oids.iter() {
                        let hex = oid.hex();
                        let hex_s = core::str::from_utf8(&hex).expect("hex");
                        let mut p = String::from_str_in("objects/", self.alloc);
                        p.push_str(&hex_s[..2]);
                        p.push('/');
                        p.push_str(&hex_s[2..]);
                        paths.push(ScopedPath { scope: PathScope::CommonDir, path: p });
                    }
                    return Ok(Step::Need(Want::ReadFiles(paths)));
                }
                Phase::ObjectsListPacks => {
                    let mut dirs = Vec::with_capacity_in(1, self.alloc);
                    dirs.push(ScopedPath::new(PathScope::CommonDir, "objects/pack", self.alloc));
                    return Ok(Step::Need(Want::ListDirs(dirs)));
                }
                Phase::ObjectsReadPackIdx => {
                    let mut paths = Vec::with_capacity_in(self.pack_oids.len() * 2, self.alloc);
                    for oid in self.pack_oids.iter() {
                        let hex = oid.hex();
                        let hex_s = core::str::from_utf8(&hex).expect("hex");
                        let mut idx_p = String::from_str_in("objects/pack/pack-", self.alloc);
                        idx_p.push_str(hex_s);
                        idx_p.push_str(".idx");
                        paths.push(ScopedPath { scope: PathScope::CommonDir, path: idx_p });
                    }
                    return Ok(Step::Need(Want::ReadWithStat(paths)));
                }
                Phase::ObjectsReadPackRanges => {
                    let mut reads = Vec::new_in(self.alloc);
                    self.pack_range_map.clear();
                    for (slot, pending) in self.pending_pack_objs.iter().enumerate() {
                        let pack_oid = self.pack_oids[pending.pack_idx];
                        let pack_end = self.pack_sizes[pending.pack_idx].saturating_sub(20);
                        let idx = Idx::parse(&self.pack_idx_bytes[pending.pack_idx], u32::MAX)
                            .map_err(|(at, why)| GitError {
                                file: GitFile::PackIndex(pack_oid),
                                at,
                                kind: ErrorKind::ObjectStore(why),
                            })?;
                        let end = idx.entry_end(pending.offset, pack_end).map_err(|(at, why)| GitError {
                            file: GitFile::PackIndex(pack_oid),
                            at,
                            kind: ErrorKind::ObjectStore(why),
                        })?;
                        let len = end.saturating_sub(pending.offset);
                        let hex = pack_oid.hex();
                        let mut p = String::from_str_in("objects/pack/pack-", self.alloc);
                        p.push_str(core::str::from_utf8(&hex).expect("hex"));
                        p.push_str(".pack");
                        reads.push(RangeRead {
                            scope: PathScope::CommonDir,
                            path: p,
                            offset: pending.offset,
                            len,
                        });
                        self.pack_range_map.push(slot);
                    }
                    if reads.is_empty() {
                        self.phase = Phase::ObjectsStart;
                        continue;
                    }
                    return Ok(Step::Need(Want::ReadRanges(reads)));
                }
                Phase::Finish => {
                    self.phase = Phase::Done;
                    return Ok(Step::Done(self.build_result()));
                }
                Phase::Done => {
                    return Err(GitError {
                        file: GitFile::Worktree,
                        at: 0,
                        kind: ErrorKind::UnexpectedReply,
                    });
                }
            }
        }
    }

    /// Feed the driver's reply matching the last [`Want`] returned by [`StatusMachine::poll`].
    pub fn feed(&mut self, got: Got<'a>) -> Result<(), GitError> {
        match (self.phase, got) {
            (Phase::Boot, Got::Files(mut files)) => {
                let want_len = if self.worktree_kind == WorktreeKind::Bare { 17 } else { 18 };
                if files.len() != want_len {
                    return Err(GitError {
                        file: GitFile::Head,
                        at: 0,
                        kind: ErrorKind::UnexpectedReply,
                    });
                }
                let take = |v: &mut AVec<'a, Option<AVec<'a, u8>>>, i: usize| v[i].take();
                self.head_raw = take(&mut files, 0);
                self.config_raw = take(&mut files, 1);
                self.packed_refs_raw = take(&mut files, 2);
                self.stash_log_raw = take(&mut files, 3);
                self.info_exclude_raw = take(&mut files, 4);
                self.graph_chain_raw = take(&mut files, 5);

                let state_files = StateFiles {
                    rebase_merge_msgnum: files[6].as_deref(),
                    rebase_merge_end: files[7].as_deref(),
                    rebase_merge_interactive: files[8].is_some(),
                    rebase_apply_rebasing: files[9].is_some(),
                    rebase_apply_applying: files[10].is_some(),
                    rebase_apply_next: files[11].as_deref(),
                    rebase_apply_last: files[12].as_deref(),
                    merge_head: files[13].is_some(),
                    cherry_pick_head: files[14].is_some(),
                    revert_head: files[15].is_some(),
                    bisect_log: files[16].is_some(),
                };
                self.repo_state = detect_repo_state(&state_files);

                let root_gitignore = if want_len == 18 { take(&mut files, 17) } else { None };
                if let Some(excl) = &self.info_exclude_raw {
                    self.ignores.set_info_exclude(excl, self.plant);
                }
                if let Some(root_ig) = &root_gitignore {
                    self.ignores.add_folder_gitignore("", root_ig, self.plant);
                }
                if let Some(stash) = &self.stash_log_raw {
                    self.counts.stashed = count_stashes(stash);
                }

                // Parse HEAD.
                let head_bytes = self.head_raw.as_deref().ok_or(GitError {
                    file: GitFile::Head,
                    at: 0,
                    kind: ErrorKind::Missing,
                })?;
                match parse_head(head_bytes)? {
                    HeadFile::Symbolic(ref_name) => {
                        let branch = ref_name.strip_prefix("refs/heads/").unwrap_or(ref_name);
                        self.branch_name = Some(String::from_str_in(branch, self.alloc));
                        self.head_sym_ref = Some(String::from_str_in(ref_name, self.alloc));
                    }
                    HeadFile::Detached(oid) => {
                        self.head_oid = Some(oid);
                    }
                }

                // Parse `.git/config`.
                if let Some(cfg_bytes) = &self.config_raw {
                    self.config = parse_config(cfg_bytes, self.branch_name.as_deref(), self.alloc);
                    if self.config.bare {
                        self.worktree_kind = WorktreeKind::Bare;
                    }
                }
                self.upstream_ref = self.config.upstream_ref(self.alloc);

                // Parse optional commit-graph-chain.
                if let Some(chain_bytes) = &self.graph_chain_raw
                    && let Ok(oids) = parse_graph_chain(chain_bytes, self.alloc)
                {
                    self.graph_layer_oids = oids;
                }

                self.phase = Phase::ReadBinaryMeta;
                Ok(())
            }

            (Phase::ReadBinaryMeta, Got::FilesWithStat(mut items)) => {
                let mut cursor = 0usize;
                if self.worktree_kind != WorktreeKind::Bare {
                    if items.is_empty() {
                        return Err(GitError {
                            file: GitFile::Index,
                            at: 0,
                            kind: ErrorKind::UnexpectedReply,
                        });
                    }
                    let idx_data = &mut items[0];
                    if let Some(raw) = idx_data.bytes.take() {
                        self.index =
                            Some(OwnedIndex::parse(raw, self.opts.limits, self.alloc, self.plant)?);
                    }
                    if let Some(st) = idx_data.stat {
                        self.index_mtime = (st.mtime_sec, st.mtime_nsec);
                    }
                    cursor = 1;
                }
                for item in items.iter_mut().skip(cursor) {
                    if let Some(bytes) = item.bytes.take() {
                        // Validate layer; ignore malformed optional commit-graph rather than failing whole status?
                        // Rule `no-fallback`: if a commit-graph file exists on disk and is corrupt, refuse loudly!
                        let _ = crate::commit_graph::CommitGraphLayer::parse(&bytes, 0)?;
                        self.graph_layers_raw.push(bytes);
                    }
                }
                self.phase = Phase::ResolveRefs;
                Ok(())
            }

            (Phase::ResolveRefs, Got::Files(files)) => {
                let mut idx = 0usize;
                let mut follow_again = false;

                if let Some(sym) = self.head_sym_ref.take() {
                    let loose = files.get(idx).and_then(|o| o.as_deref());
                    idx += 1;
                    if let Some(bytes) = loose {
                        match parse_loose_ref(bytes)? {
                            HeadFile::Detached(oid) => self.head_oid = Some(oid),
                            HeadFile::Symbolic(next) => {
                                self.ref_follow_depth += 1;
                                if self.ref_follow_depth > 8 {
                                    return Err(GitError {
                                        file: GitFile::Ref,
                                        at: 0,
                                        kind: ErrorKind::RefCycle,
                                    });
                                }
                                self.head_sym_ref = Some(String::from_str_in(next, self.alloc));
                                follow_again = true;
                            }
                        }
                    } else if let Some(packed) = &self.packed_refs_raw {
                        self.head_oid = lookup_packed_ref(packed, sym.as_str());
                    }
                }

                if self.upstream_oid.is_none()
                    && let Some(up) = &self.upstream_ref
                    && idx < files.len()
                {
                    let loose = files[idx].as_deref();
                    if let Some(bytes) = loose {
                        if let HeadFile::Detached(oid) = parse_loose_ref(bytes)? {
                            self.upstream_oid = Some(oid);
                        }
                    } else if let Some(packed) = &self.packed_refs_raw {
                        self.upstream_oid = lookup_packed_ref(packed, up.ref_path.as_str());
                    }
                }

                if !follow_again {
                    self.phase = Phase::ScanTags;
                }
                Ok(())
            }

            (Phase::WalkDirs, Got::Dirs(mut listings)) => {
                let dirs = core::mem::replace(&mut self.walk_queue, Vec::new_in(self.alloc));
                if listings.len() != dirs.len() {
                    return Err(GitError {
                        file: GitFile::Worktree,
                        at: 0,
                        kind: ErrorKind::UnexpectedReply,
                    });
                }
                self.buffered_dirs.clear();
                self.pending_ignore_dirs.clear();

                for (wd, listing) in dirs.into_iter().zip(listings.iter_mut()) {
                    let Some(entries) = listing.take() else {
                        continue;
                    };
                    // Check if a non-root directory has a `.gitignore` file to load before processing!
                    if !wd.path.is_empty() && entries.has_name(b".gitignore") {
                        self.pending_ignore_dirs
                            .push(String::from_str_in(wd.path.as_str(), self.alloc));
                    }
                    self.buffered_dirs.push((wd, entries));
                }

                if !self.pending_ignore_dirs.is_empty() {
                    self.phase = Phase::WalkReadIgnores;
                } else {
                    self.process_buffered_dirs()?;
                    self.phase = Phase::WalkDirs;
                }
                Ok(())
            }

            (Phase::WalkReadIgnores, Got::Files(files)) => {
                if files.len() != self.pending_ignore_dirs.len() {
                    return Err(GitError {
                        file: GitFile::Worktree,
                        at: 0,
                        kind: ErrorKind::UnexpectedReply,
                    });
                }
                for (dir, maybe_bytes) in self.pending_ignore_dirs.iter().zip(files.iter()) {
                    if let Some(bytes) = maybe_bytes {
                        self.ignores.add_folder_gitignore(dir.as_str(), bytes, self.plant);
                    }
                }
                self.truncated.ignore_rules_capped |= self.ignores.rules_capped;
                self.pending_ignore_dirs.clear();
                self.process_buffered_dirs()?;
                self.phase = Phase::WalkDirs;
                Ok(())
            }

            (Phase::StatRemaining, Got::Stats(stats)) => {
                if stats.len() != self.stat_pending_indices.len() {
                    return Err(GitError {
                        file: GitFile::Worktree,
                        at: 0,
                        kind: ErrorKind::UnexpectedReply,
                    });
                }
                let owned_idx = self.index.take().expect("index present");
                let pending =
                    core::mem::replace(&mut self.stat_pending_indices, Vec::new_in(self.alloc));
                for (idx, st) in pending.into_iter().zip(stats.iter()) {
                    let path_bytes = owned_idx.view().path_bytes(&owned_idx.entries[idx]);
                    self.check_tracked_stat(idx, &owned_idx.entries[idx], path_bytes, st.stat);
                }
                self.index = Some(owned_idx);
                self.phase = Phase::HashFiles;
                Ok(())
            }

            (Phase::HashFiles, Got::Files(files)) => {
                if files.len() != self.hash_file_indices.len() {
                    return Err(GitError {
                        file: GitFile::Worktree,
                        at: 0,
                        kind: ErrorKind::UnexpectedReply,
                    });
                }
                let owned_idx = self.index.take().expect("index present");
                let indices =
                    core::mem::replace(&mut self.hash_file_indices, Vec::new_in(self.alloc));
                let autocrlf = self.config.autocrlf;
                for (idx, maybe_bytes) in indices.into_iter().zip(files.iter()) {
                    if self.opts.detail == Detail::Presence && self.counts.modified > 0 {
                        break;
                    }
                    let entry = &owned_idx.entries[idx];
                    let path_bytes = owned_idx.view().path_bytes(entry);
                    let Some(bytes) = maybe_bytes.as_deref() else {
                        self.counts.deleted = self.counts.deleted.saturating_add(1);
                        self.record_sample_bytes(path_bytes, ChangeKind::Deleted);
                        continue;
                    };
                    let blob_oid = hash_worktree_blob(bytes, autocrlf, self.alloc);
                    if blob_oid != entry.oid {
                        self.counts.modified = self.counts.modified.saturating_add(1);
                        self.record_sample_bytes(path_bytes, ChangeKind::Modified);
                    }
                }
                self.index = Some(owned_idx);
                self.phase = Phase::HashLinks;
                Ok(())
            }

            (Phase::HashLinks, Got::Links(links)) => {
                if links.len() != self.hash_link_indices.len() {
                    return Err(GitError {
                        file: GitFile::Worktree,
                        at: 0,
                        kind: ErrorKind::UnexpectedReply,
                    });
                }
                let owned_idx = self.index.take().expect("index present");
                let indices =
                    core::mem::replace(&mut self.hash_link_indices, Vec::new_in(self.alloc));
                for (idx, maybe_target) in indices.into_iter().zip(links.iter()) {
                    if self.opts.detail == Detail::Presence && self.counts.modified > 0 {
                        break;
                    }
                    let entry = &owned_idx.entries[idx];
                    let path_bytes = owned_idx.view().path_bytes(entry);
                    let Some(target) = maybe_target.as_deref() else {
                        self.counts.typechanged = self.counts.typechanged.saturating_add(1);
                        self.record_sample_bytes(path_bytes, ChangeKind::Modified);
                        continue;
                    };
                    if Oid::of(ObjectKind::Blob, target) != entry.oid {
                        self.counts.modified = self.counts.modified.saturating_add(1);
                        self.record_sample_bytes(path_bytes, ChangeKind::Modified);
                    }
                }
                self.index = Some(owned_idx);
                self.phase = Phase::ObjectsStart;
                Ok(())
            }

            (Phase::ObjectsReadLoose, Got::Files(files)) => {
                if files.len() != self.fetching_oids.len() {
                    return Err(GitError {
                        file: GitFile::Worktree,
                        at: 0,
                        kind: ErrorKind::UnexpectedReply,
                    });
                }
                let mut still_missing = Vec::new_in(self.alloc);
                let oids = core::mem::replace(&mut self.fetching_oids, Vec::new_in(self.alloc));
                let mut whole = Vec::new_in(self.alloc);
                for (oid, maybe_bytes) in oids.into_iter().zip(files.iter()) {
                    if let Some(compressed) = maybe_bytes.as_deref() {
                        whole.clear();
                        let (kind, body_start) = store::loose(
                            compressed,
                            &oid,
                            self.opts.limits.most_object_bytes,
                            self.alloc,
                            &mut whole,
                        )
                        .map_err(|(at, why)| GitError {
                            file: GitFile::LooseObject(oid),
                            at,
                            kind: ErrorKind::ObjectStore(why),
                        })?;
                        let mut body = Vec::with_capacity_in(whole.len() - body_start, self.alloc);
                        body.extend_from_slice(&whole[body_start..]);
                        self.objects.insert(oid, (kind, body));
                    } else {
                        still_missing.push(oid);
                    }
                }
                self.fetching_oids = still_missing;
                if self.fetching_oids.is_empty() {
                    self.phase = Phase::ObjectsStart;
                } else if !self.packs_listed {
                    self.phase = Phase::ObjectsListPacks;
                } else {
                    self.locate_in_packs()?;
                }
                Ok(())
            }

            (Phase::ObjectsListPacks, Got::Dirs(dirs)) => {
                self.packs_listed = true;
                let mut pack_sizes_map: Map<Oid, u64, BuildKeyHasher, Alloc<'a>> = Map::new(MapOpts {
                    hasher: BuildKeyHasher::default(),
                    alloc: self.alloc,
                    capacity: Some(16),
                });
                if let Some(Some(listing)) = dirs.first() {
                    for entry in listing.entries.iter() {
                        let name = listing.name_str(entry);
                        if let Some(hex) =
                            name.strip_prefix("pack-").and_then(|s| s.strip_suffix(".pack"))
                            && let Some(oid) = Oid::parse_hex(hex.as_bytes())
                        {
                            pack_sizes_map.insert(oid, entry.stat.size);
                        }
                    }
                    for entry in listing.entries.iter() {
                        let name = listing.name_str(entry);
                        if let Some(hex) =
                            name.strip_prefix("pack-").and_then(|s| s.strip_suffix(".idx"))
                            && let Some(oid) = Oid::parse_hex(hex.as_bytes())
                            && let Some(&pack_size) = pack_sizes_map.get(&oid)
                        {
                            if self.pack_oids.len() >= self.opts.limits.most_packs {
                                break;
                            }
                            self.pack_oids.push(oid);
                            self.pack_sizes.push(pack_size);
                        }
                    }
                }
                if self.pack_oids.is_empty() {
                    let missing = self.fetching_oids[0];
                    return Err(GitError {
                        file: GitFile::LooseObject(missing),
                        at: 0,
                        kind: ErrorKind::ObjectMissing(missing),
                    });
                }
                self.phase = Phase::ObjectsReadPackIdx;
                Ok(())
            }

            (Phase::ObjectsReadPackIdx, Got::FilesWithStat(mut items)) => {
                if items.len() != self.pack_oids.len() {
                    return Err(GitError {
                        file: GitFile::Worktree,
                        at: 0,
                        kind: ErrorKind::UnexpectedReply,
                    });
                }
                self.pack_idx_bytes.clear();
                for (i, item) in items.iter_mut().enumerate() {
                    let pack_oid = self.pack_oids[i];
                    let bytes = item.bytes.take().ok_or(GitError {
                        file: GitFile::PackIndex(pack_oid),
                        at: 0,
                        kind: ErrorKind::Missing,
                    })?;
                    let _ = Idx::parse(&bytes, u32::MAX).map_err(|(at, why)| GitError {
                        file: GitFile::PackIndex(pack_oid),
                        at,
                        kind: ErrorKind::ObjectStore(why),
                    })?;
                    self.pack_idx_bytes.push(bytes);
                }
                self.locate_in_packs()?;
                Ok(())
            }

            (Phase::ObjectsReadPackRanges, Got::Ranges(ranges)) => {
                if ranges.len() != self.pack_range_map.len() {
                    return Err(GitError {
                        file: GitFile::Worktree,
                        at: 0,
                        kind: ErrorKind::UnexpectedReply,
                    });
                }
                self.process_pack_ranges(ranges)?;
                Ok(())
            }

            _ => Err(GitError {
                file: GitFile::Worktree,
                at: 0,
                kind: ErrorKind::UnexpectedReply,
            }),
        }
    }

    // -----------------------------------------------------------------------
    // Worktree & tracked file helpers
    // -----------------------------------------------------------------------

    fn queue_unverified_stats(&mut self, parsed: IndexView<'_>) {
        self.stat_pending_indices.clear();
        for (i, entry) in parsed.entries.iter().enumerate() {
            if entry.assume_valid || entry.skip_worktree || entry.is_sparse_dir() || entry.is_gitlink() {
                self.verified_entries[i] = true;
                continue;
            }
            if !self.verified_entries[i] {
                self.stat_pending_indices.push(i);
            }
        }
    }

    fn prepare_remaining_stats(&mut self) -> Result<(), GitError> {
        let Some(owned) = self.index.take() else {
            return Ok(());
        };
        let parsed = owned.view();
        self.stat_pending_indices.clear();
        for (i, entry) in parsed.entries.iter().enumerate() {
            if self.verified_entries[i] {
                continue;
            }
            if entry.assume_valid || entry.skip_worktree || entry.is_sparse_dir() || entry.is_gitlink() {
                self.verified_entries[i] = true;
                continue;
            }
            // Any direct file inside a listed directory was already verified or marked deleted in
            // `process_buffered_dirs`; remaining entries live inside ignored/unwalked subtrees.
            self.stat_pending_indices.push(i);
        }
        self.index = Some(owned);
        Ok(())
    }

    fn process_buffered_dirs(&mut self) -> Result<(), GitError> {
        let buffered = core::mem::replace(&mut self.buffered_dirs, Vec::new_in(self.alloc));
        let owned_opt = self.index.take();
        let parsed = owned_opt.as_ref().map(OwnedIndex::view);

        for (wd, listing) in buffered {
            if let Some(group_id) = wd.collapse_group
                && self.collapse_found[group_id as usize]
            {
                continue;
            }

            let prefix_len = if wd.path.is_empty() { 0 } else { wd.path.len() + 1 };
            let idx_slice = match parsed {
                Some(idx) => &idx.entries[wd.index_start..wd.index_end],
                None => &[],
            };

            for entry in listing.entries.iter() {
                self.worktree_entries_seen = self.worktree_entries_seen.saturating_add(1);
                if self.worktree_entries_seen > self.opts.limits.most_worktree_entries {
                    self.truncated.untracked_capped = true;
                    self.index = owned_opt;
                    return Ok(());
                }

                let name_bytes = listing.name_bytes(entry);
                if wd.path.is_empty() && name_bytes == b".git" {
                    continue;
                }

                // Scoped binary search ONLY inside this directory's `[index_start..index_end)`,
                // comparing local suffix `&path[prefix_len..]` with zero allocations!
                if let Some(idx) = parsed
                    && !idx_slice.is_empty()
                    && let Ok(rel_pos) = idx_slice
                        .binary_search_by(|ie| idx.path_bytes(ie)[prefix_len..].cmp(name_bytes))
                {
                    let idx_pos = wd.index_start + rel_pos;
                    self.verified_entries[idx_pos] = true;
                    let idx_entry = &idx.entries[idx_pos];
                    if !idx_entry.assume_valid
                        && !idx_entry.skip_worktree
                        && !idx_entry.is_sparse_dir()
                        && !idx_entry.is_gitlink()
                    {
                        let path_bytes = idx.path_bytes(idx_entry);
                        self.check_tracked_stat(idx_pos, idx_entry, path_bytes, entry.stat);
                    }
                    continue;
                }

                let name = listing.name_str(entry);
                let is_dir = entry.stat.kind == FileKind::Dir;
                let presence_untracked_done =
                    self.opts.detail == Detail::Presence && self.counts.untracked > 0;

                // Build child path in reusable scratch buffer (ZERO allocation unless queued!).
                self.child_scratch.clear();
                if !wd.path.is_empty() {
                    self.child_scratch.push_str(wd.path.as_str());
                    self.child_scratch.push('/');
                }
                self.child_scratch.push_str(name);

                if is_dir {
                    if self.ignores.is_ignored(self.child_scratch.as_str(), true) {
                        continue;
                    }
                    // Find exact sub-range of index entries under `name/` inside `idx_slice`!
                    let (sub_start, sub_end) = if let Some(idx) = parsed
                        && !idx_slice.is_empty()
                    {
                        let lo = idx_slice.partition_point(|ie| {
                            let rel = &idx.path_bytes(ie)[prefix_len..];
                            let n = rel.len().min(name_bytes.len());
                            match rel[..n].cmp(&name_bytes[..n]) {
                                core::cmp::Ordering::Less => true,
                                core::cmp::Ordering::Greater => false,
                                core::cmp::Ordering::Equal if rel.len() <= name_bytes.len() => true,
                                core::cmp::Ordering::Equal => rel[name_bytes.len()] < b'/',
                            }
                        });
                        let hi = lo
                            + idx_slice[lo..].partition_point(|ie| {
                                let rel = &idx.path_bytes(ie)[prefix_len..];
                                rel.len() > name_bytes.len()
                                    && rel.starts_with(name_bytes)
                                    && rel[name_bytes.len()] == b'/'
                            });
                        (wd.index_start + lo, wd.index_start + hi)
                    } else {
                        (0, 0)
                    };

                    if sub_start < sub_end {
                        if wd.depth + 1 <= self.opts.limits.most_dir_depth {
                            self.walk_queue.push(WalkDir {
                                path: String::from_str_in(self.child_scratch.as_str(), self.alloc),
                                depth: wd.depth + 1,
                                index_start: sub_start,
                                index_end: sub_end,
                                collapse_group: None,
                            });
                        }
                        continue;
                    }
                    if presence_untracked_done {
                        continue;
                    }
                    if wd.depth + 1 > self.opts.limits.most_dir_depth {
                        self.truncated.untracked_capped = true;
                        continue;
                    }
                    if self.opts.untracked == UntrackedMode::Normal {
                        let group = match wd.collapse_group {
                            Some(g) => g,
                            None => {
                                let g = self.collapse_found.len() as u32;
                                self.collapse_found.push(false);
                                g
                            }
                        };
                        self.walk_queue.push(WalkDir {
                            path: String::from_str_in(self.child_scratch.as_str(), self.alloc),
                            depth: wd.depth + 1,
                            index_start: 0,
                            index_end: 0,
                            collapse_group: Some(group),
                        });
                    } else {
                        self.walk_queue.push(WalkDir {
                            path: String::from_str_in(self.child_scratch.as_str(), self.alloc),
                            depth: wd.depth + 1,
                            index_start: 0,
                            index_end: 0,
                            collapse_group: None,
                        });
                    }
                } else {
                    if presence_untracked_done {
                        continue;
                    }
                    if self.ignores.is_ignored(self.child_scratch.as_str(), false) {
                        continue;
                    }
                    if let Some(group_id) = wd.collapse_group {
                        if !self.collapse_found[group_id as usize] {
                            self.collapse_found[group_id as usize] = true;
                            self.counts.untracked = self.counts.untracked.saturating_add(1);
                            self.record_child_scratch(ChangeKind::Untracked);
                        }
                        break;
                    } else {
                        self.counts.untracked = self.counts.untracked.saturating_add(1);
                        self.record_child_scratch(ChangeKind::Untracked);
                    }
                }
            }

            // Detect direct tracked files deleted from this listed directory without any `lstat`!
            if wd.collapse_group.is_none()
                && let Some(idx) = parsed
            {
                let mut i = wd.index_start;
                while i < wd.index_end {
                    let entry = &idx.entries[i];
                    let rel = &idx.path_bytes(entry)[prefix_len..];
                    if let Some(slash) = memchr(b'/', rel) {
                        // Skip entire subdirectory block in O(log block) / linear scan; if that
                        // subdirectory was missing on disk, its entries stay unverified and get
                        // caught cleanly (or deleted if subdir wasn't listed).
                        let sub = &rel[..slash];
                        // Check if this subdir existed in `listing`: if NOT, every entry under it
                        // is deleted!
                        if !listing.has_name(sub) {
                            while i < wd.index_end {
                                let e2 = &idx.entries[i];
                                let r2 = &idx.path_bytes(e2)[prefix_len..];
                                if r2.len() > slash && r2[..slash] == *sub && r2[slash] == b'/' {
                                    if !self.verified_entries[i]
                                        && !e2.assume_valid
                                        && !e2.skip_worktree
                                        && !e2.is_sparse_dir()
                                        && !e2.is_gitlink()
                                    {
                                        self.counts.deleted = self.counts.deleted.saturating_add(1);
                                        self.verified_entries[i] = true;
                                    }
                                    i += 1;
                                } else {
                                    break;
                                }
                            }
                        } else {
                            // Subdir exists: jump past it in `idx_slice`.
                            i += 1;
                            while i < wd.index_end {
                                let r2 = &idx.path_bytes(&idx.entries[i])[prefix_len..];
                                if r2.len() > slash && r2[..slash] == *sub && r2[slash] == b'/' {
                                    i += 1;
                                } else {
                                    break;
                                }
                            }
                        }
                    } else {
                        if !self.verified_entries[i]
                            && !entry.assume_valid
                            && !entry.skip_worktree
                            && !entry.is_sparse_dir()
                            && !entry.is_gitlink()
                        {
                            self.counts.deleted = self.counts.deleted.saturating_add(1);
                            self.verified_entries[i] = true;
                        }
                        i += 1;
                    }
                }
            }
        }
        self.index = owned_opt;
        Ok(())
    }

    fn record_child_scratch(&mut self, kind: ChangeKind) {
        if self.changed_samples.len() >= MAX_CHANGED_SAMPLES || self.child_scratch.is_empty() {
            return;
        }
        self.changed_samples.push(ChangedSample {
            path: String::from_str_in(self.child_scratch.as_str(), self.alloc),
            kind,
        });
    }

    fn record_sample_bytes(&mut self, path: &[u8], kind: ChangeKind) {
        if self.changed_samples.len() >= MAX_CHANGED_SAMPLES || path.is_empty() {
            return;
        }
        if let Ok(s) = core::str::from_utf8(path) {
            self.changed_samples.push(ChangedSample {
                path: String::from_str_in(s, self.alloc),
                kind,
            });
        }
    }

    fn check_tracked_stat(
        &mut self,
        idx_pos: usize,
        entry: &IndexEntry,
        path_bytes: &[u8],
        stat: FileStat,
    ) {
        if stat.kind == FileKind::Gone {
            self.counts.deleted = self.counts.deleted.saturating_add(1);
            self.record_sample_bytes(path_bytes, ChangeKind::Deleted);
            return;
        }
        if entry.intent_to_add {
            self.counts.modified = self.counts.modified.saturating_add(1);
            self.record_sample_bytes(path_bytes, ChangeKind::Modified);
            return;
        }
        let want_kind = if entry.is_symlink() {
            FileKind::Symlink
        } else if entry.is_regular() {
            FileKind::File
        } else {
            return;
        };
        if stat.kind != want_kind {
            self.counts.typechanged = self.counts.typechanged.saturating_add(1);
            self.record_sample_bytes(path_bytes, ChangeKind::Modified);
            return;
        }

        let mode_differs = self.config.filemode
            && want_kind == FileKind::File
            && stat.executable() != entry.is_executable();
        if mode_differs {
            self.counts.modified = self.counts.modified.saturating_add(1);
            self.record_sample_bytes(path_bytes, ChangeKind::Modified);
            return;
        }

        // Hardware-floor short-circuit: when no line-ending conversion applies (`autocrlf == False`
        // or symlink), any regular file < 4 GiB whose byte length differs from its index entry
        // cannot hash to the same Git blob OID -> guaranteed modified with 0 reads & 0 SHA-1!
        let size_differs_definitively = stat.size < (1u64 << 32)
            && u64::from(entry.size) != stat.size
            && (want_kind == FileKind::Symlink || self.config.autocrlf == AutoCrlf::False);
        if size_differs_definitively {
            self.counts.modified = self.counts.modified.saturating_add(1);
            self.record_sample_bytes(path_bytes, ChangeKind::Modified);
            return;
        }

        // Fast stat match (Git's `ce_match_stat_basic` + racy-git rule).
        let size_match = u64::from(entry.size) == (stat.size & 0xffff_ffff);
        let mtime_match = entry.mtime_sec == (stat.mtime_sec as u32)
            && (entry.mtime_nsec == 0 || entry.mtime_nsec == stat.mtime_nsec);
        let ctime_match = entry.ctime_sec == (stat.ctime_sec as u32)
            && (entry.ctime_nsec == 0 || entry.ctime_nsec == stat.ctime_nsec);
        let ino_match = entry.ino == 0 || entry.ino == (stat.ino as u32);

        let racy = !self.plant.has(GitStatusPlant::SkipRacyHashCheck)
            && self.index_mtime.0 > 0
            && i64::from(entry.mtime_sec) >= self.index_mtime.0;

        if size_match && mtime_match && ctime_match && ino_match && !racy {
            return;
        }

        if want_kind == FileKind::Symlink {
            self.hash_link_indices.push(idx_pos);
        } else {
            self.hash_file_indices.push(idx_pos);
        }
    }

    // -----------------------------------------------------------------------
    // Object store / staged diff / ahead-behind orchestration
    // -----------------------------------------------------------------------

    fn build_commit_graph<'s>(&'s self) -> Result<Option<CommitGraph<'s, 'a>>, GitError> {
        if self.graph_layers_raw.is_empty() {
            return Ok(None);
        }
        let mut slices: AVec<'a, &'s [u8]> =
            Vec::with_capacity_in(self.graph_layers_raw.len(), self.alloc);
        for raw in self.graph_layers_raw.iter() {
            slices.push(raw.as_slice());
        }
        Ok(Some(CommitGraph::from_layers(&slices, self.alloc)?))
    }

    fn drive_objects_phase(&mut self) -> Result<Option<Want<'a>>, GitError> {
        let mut needed: AVec<'a, Oid> = Vec::new_in(self.alloc);

        // 1. Resolve HEAD's root tree OID (for staged diff).
        if self.worktree_kind != WorktreeKind::Bare && self.staged_diff.is_none() {
            if let Some(head_oid) = self.head_oid {
                if self.head_tree_oid.is_none() {
                    // Try commit-graph first (O(log N), 0 object reads!).
                    if let Some(cg) = self.build_commit_graph()?
                        && let Some(gc) = cg.lookup(&head_oid)
                    {
                        self.head_tree_oid = Some(gc.tree);
                    }
                    // Else check already-fetched commit object.
                    if self.head_tree_oid.is_none()
                        && let Some((kind, body)) = self.objects.get(&head_oid)
                    {
                        if *kind != ObjectKind::Commit {
                            return Err(GitError {
                                file: GitFile::CommitObject(head_oid),
                                at: 0,
                                kind: ErrorKind::WrongObjectKind {
                                    want: ObjectKind::Commit,
                                    got: *kind,
                                },
                            });
                        }
                        let commit = Commit::parse(body).map_err(|(at, why)| GitError {
                            file: GitFile::CommitObject(head_oid),
                            at,
                            kind: ErrorKind::CommitParse(why),
                        })?;
                        self.head_tree_oid = Some(commit.tree);
                    }
                    if self.head_tree_oid.is_none() {
                        needed.push(head_oid);
                    }
                }
                if let Some(head_tree) = self.head_tree_oid {
                    if let Some(owned) = &self.index {
                        self.staged_diff = Some(StagedDiff::start(
                            Some(head_tree),
                            owned.view(),
                            self.opts.detail,
                            self.opts.limits,
                            self.alloc,
                            self.plant,
                        ));
                    }
                }
            } else if let Some(owned) = &self.index {
                // Unborn HEAD: staged_added = stage-0 entries.
                self.staged_diff = Some(StagedDiff::start(
                    None,
                    owned.view(),
                    self.opts.detail,
                    self.opts.limits,
                    self.alloc,
                    self.plant,
                ));
            }
        }

        // 2. Step staged diff (cache-tree pruned!).
        if self.staged_diff.is_some() {
            let mut tree_oids = Vec::new_in(self.alloc);
            loop {
                let is_done = self.staged_diff.as_ref().unwrap().is_done();
                if is_done {
                    let sc = self.staged_diff.as_ref().unwrap().counts;
                    self.counts.staged_added = sc.added;
                    self.counts.staged_modified = sc.modified;
                    self.counts.staged_deleted = sc.deleted;
                    self.counts.staged_typechanged = sc.typechanged;
                    break;
                }
                self.staged_diff.as_ref().unwrap().needed_trees(&mut tree_oids);
                let mut any_missing = false;
                for &oid in tree_oids.iter() {
                    if !self.objects.contains_key(&oid) {
                        if needed.scan_for(&oid).is_none() {
                            needed.push(oid);
                        }
                        any_missing = true;
                    }
                }
                if any_missing {
                    break;
                }
                let owned = self.index.as_ref().expect("index present");
                let objects = &self.objects;
                let mut diff = self.staged_diff.take().unwrap();
                let res = diff.step(owned.view(), |oid| {
                    objects.get(&oid).and_then(|(k, b)| (*k == ObjectKind::Tree).then_some(b.as_slice()))
                });
                self.staged_diff = Some(diff);
                res?;
            }
        }

        // 3. Step ahead/behind divergence walker!
        if self.opts.ahead_behind == AheadBehindMode::Enabled && self.upstream_ref.is_some() {
            match (self.head_oid, self.upstream_oid) {
                (None, _) => self.divergence = Divergence::Unborn,
                (Some(_), None) => self.divergence = Divergence::UpstreamMissing,
                (Some(head), Some(upstream)) => {
                    if self.ahead_behind.is_none() {
                        self.ahead_behind = Some(AheadBehindWalker::new(
                            head,
                            upstream,
                            self.opts.limits,
                            self.alloc,
                        ));
                    }
                    // Supply any newly fetched commit objects.
                    let mut walker = self.ahead_behind.take().unwrap();
                    let missing_prev: AVec<'a, Oid> = {
                        let mut tmp = Vec::with_capacity_in(walker.missing_commits().len(), self.alloc);
                        tmp.extend_from_slice(walker.missing_commits());
                        tmp
                    };
                    for oid in missing_prev.iter() {
                        if let Some((kind, body)) = self.objects.get(oid) {
                            if *kind != ObjectKind::Commit {
                                self.ahead_behind = Some(walker);
                                return Err(GitError {
                                    file: GitFile::CommitObject(*oid),
                                    at: 0,
                                    kind: ErrorKind::WrongObjectKind {
                                        want: ObjectKind::Commit,
                                        got: *kind,
                                    },
                                });
                            }
                            let commit = Commit::parse(body).map_err(|(at, why)| GitError {
                                file: GitFile::CommitObject(*oid),
                                at,
                                kind: ErrorKind::CommitParse(why),
                            })?;
                            walker.supply_commit(*oid, CommitInfo::from_raw_commit(&commit));
                        }
                    }

                    let cg = self.build_commit_graph()?;
                    let step = walker.advance(cg.as_ref(), self.plant);
                    match step {
                        WalkStep::Done(div) => {
                            if matches!(div, Divergence::Capped { .. }) {
                                self.truncated.commits_capped = true;
                            }
                            self.divergence = div;
                        }
                        WalkStep::NeedCommits => {
                            for &oid in walker.missing_commits() {
                                if needed.scan_for(&oid).is_none() {
                                    needed.push(oid);
                                }
                            }
                        }
                    }
                    self.ahead_behind = Some(walker);
                }
            }
        }

        if needed.is_empty() {
            Ok(None)
        } else {
            self.fetching_oids = needed;
            self.phase = Phase::ObjectsReadLoose;
            let mut paths = Vec::with_capacity_in(self.fetching_oids.len(), self.alloc);
            for oid in self.fetching_oids.iter() {
                let hex = oid.hex();
                let hex_s = core::str::from_utf8(&hex).expect("hex");
                let mut p = String::from_str_in("objects/", self.alloc);
                p.push_str(&hex_s[..2]);
                p.push('/');
                p.push_str(&hex_s[2..]);
                paths.push(ScopedPath { scope: PathScope::CommonDir, path: p });
            }
            Ok(Some(Want::ReadFiles(paths)))
        }
    }

    fn locate_in_packs(&mut self) -> Result<(), GitError> {
        let missing = core::mem::replace(&mut self.fetching_oids, Vec::new_in(self.alloc));
        self.pending_pack_objs.clear();

        for oid in missing {
            let mut found = None;
            for (p_idx, idx_raw) in self.pack_idx_bytes.iter().enumerate() {
                let pack_oid = self.pack_oids[p_idx];
                let idx = Idx::parse(idx_raw, u32::MAX).map_err(|(at, why)| GitError {
                    file: GitFile::PackIndex(pack_oid),
                    at,
                    kind: ErrorKind::ObjectStore(why),
                })?;
                if let Ok(entry_pos) = idx.search(&oid) {
                    let pack_end = self.pack_sizes[p_idx].saturating_sub(20);
                    let offset = idx.offset(entry_pos, pack_end).map_err(|(at, why)| GitError {
                        file: GitFile::PackIndex(pack_oid),
                        at,
                        kind: ErrorKind::ObjectStore(why),
                    })?;
                    found = Some((p_idx, offset));
                    break;
                }
            }
            let Some((pack_idx, offset)) = found else {
                return Err(GitError {
                    file: GitFile::LooseObject(oid),
                    at: 0,
                    kind: ErrorKind::ObjectMissing(oid),
                });
            };
            self.pending_pack_objs.push(PendingPackObj {
                want_oid: oid,
                pack_idx,
                offset,
                deltas: Vec::new_in(self.alloc),
                depth: 0,
            });
        }
        self.phase = Phase::ObjectsReadPackRanges;
        Ok(())
    }

    fn process_pack_ranges(&mut self, ranges: AVec<'a, RangeData<'a>>) -> Result<(), GitError> {
        let prev = core::mem::replace(&mut self.pending_pack_objs, Vec::new_in(self.alloc));
        let mut next_pending = Vec::new_in(self.alloc);
        let mut buf = Vec::new_in(self.alloc);

        for (mut pending, range) in prev.into_iter().zip(ranges.into_iter()) {
            let pack_oid = self.pack_oids[pending.pack_idx];
            let window = range.bytes.as_deref().ok_or(GitError {
                file: GitFile::PackData(pack_oid),
                at: pending.offset,
                kind: ErrorKind::Missing,
            })?;
            let head = store::entry_head(window).map_err(|why| GitError {
                file: GitFile::PackData(pack_oid),
                at: pending.offset,
                kind: ErrorKind::ObjectStore(why),
            })?;
            buf.clear();
            store::inflate_entry(window, &head, self.opts.limits.most_object_bytes, &mut buf)
                .map_err(|why| GitError {
                    file: GitFile::PackData(pack_oid),
                    at: pending.offset,
                    kind: ErrorKind::ObjectStore(why),
                })?;

            match head.base {
                Base::Whole(kind) => {
                    let mut current = Vec::with_capacity_in(buf.len(), self.alloc);
                    current.extend_from_slice(&buf);
                    let mut scratch = Vec::new_in(self.alloc);
                    // Apply accumulated deltas in reverse (deepest base first).
                    for delta in pending.deltas.iter().rev() {
                        scratch.clear();
                        store::apply_delta(
                            &current,
                            delta,
                            self.opts.limits.most_object_bytes,
                            &mut scratch,
                        )
                        .map_err(|why| GitError {
                            file: GitFile::PackData(pack_oid),
                            at: pending.offset,
                            kind: ErrorKind::ObjectStore(why),
                        })?;
                        core::mem::swap(&mut current, &mut scratch);
                    }
                    if Oid::of(kind, &current) != pending.want_oid {
                        return Err(GitError {
                            file: GitFile::PackData(pack_oid),
                            at: pending.offset,
                            kind: ErrorKind::ObjectStore(store::StoreWhy::Id {
                                want: pending.want_oid,
                            }),
                        });
                    }
                    self.objects.insert(pending.want_oid, (kind, current));
                }
                Base::Ofs { back } => {
                    pending.depth += 1;
                    if pending.depth > self.opts.limits.most_delta_depth {
                        return Err(GitError {
                            file: GitFile::PackData(pack_oid),
                            at: pending.offset,
                            kind: ErrorKind::DepthExceeded {
                                most: self.opts.limits.most_delta_depth as u16,
                            },
                        });
                    }
                    let base_offset = pending.offset.checked_sub(back).ok_or(GitError {
                        file: GitFile::PackData(pack_oid),
                        at: pending.offset,
                        kind: ErrorKind::Malformed,
                    })?;
                    let mut delta_copy = Vec::with_capacity_in(buf.len(), self.alloc);
                    delta_copy.extend_from_slice(&buf);
                    pending.deltas.push(delta_copy);
                    pending.offset = base_offset;
                    next_pending.push(pending);
                }
                Base::Ref { base } => {
                    pending.depth += 1;
                    if pending.depth > self.opts.limits.most_delta_depth {
                        return Err(GitError {
                            file: GitFile::PackData(pack_oid),
                            at: pending.offset,
                            kind: ErrorKind::DepthExceeded {
                                most: self.opts.limits.most_delta_depth as u16,
                            },
                        });
                    }
                    // If base is already in `self.objects`, finish immediately!
                    if let Some((base_kind, base_body)) = self.objects.get(&base) {
                        let kind = *base_kind;
                        let mut current = Vec::with_capacity_in(base_body.len(), self.alloc);
                        current.extend_from_slice(base_body);
                        let mut scratch = Vec::new_in(self.alloc);
                        let mut delta_copy = Vec::with_capacity_in(buf.len(), self.alloc);
                        delta_copy.extend_from_slice(&buf);
                        pending.deltas.push(delta_copy);
                        for delta in pending.deltas.iter().rev() {
                            scratch.clear();
                            store::apply_delta(
                                &current,
                                delta,
                                self.opts.limits.most_object_bytes,
                                &mut scratch,
                            )
                            .map_err(|why| GitError {
                                file: GitFile::PackData(pack_oid),
                                at: pending.offset,
                                kind: ErrorKind::ObjectStore(why),
                            })?;
                            core::mem::swap(&mut current, &mut scratch);
                        }
                        self.objects.insert(pending.want_oid, (kind, current));
                    } else {
                        // Locate base in pack indexes.
                        let mut found = None;
                        for (p_idx, idx_raw) in self.pack_idx_bytes.iter().enumerate() {
                            let p_oid = self.pack_oids[p_idx];
                            let idx = Idx::parse(idx_raw, u32::MAX).map_err(|(at, why)| GitError {
                                file: GitFile::PackIndex(p_oid),
                                at,
                                kind: ErrorKind::ObjectStore(why),
                            })?;
                            if let Ok(pos) = idx.search(&base) {
                                let pack_end = self.pack_sizes[p_idx].saturating_sub(20);
                                let off = idx.offset(pos, pack_end).map_err(|(at, why)| GitError {
                                    file: GitFile::PackIndex(p_oid),
                                    at,
                                    kind: ErrorKind::ObjectStore(why),
                                })?;
                                found = Some((p_idx, off));
                                break;
                            }
                        }
                        let Some((base_pack, base_off)) = found else {
                            return Err(GitError {
                                file: GitFile::PackData(pack_oid),
                                at: pending.offset,
                                kind: ErrorKind::ObjectMissing(base),
                            });
                        };
                        let mut delta_copy = Vec::with_capacity_in(buf.len(), self.alloc);
                        delta_copy.extend_from_slice(&buf);
                        pending.deltas.push(delta_copy);
                        pending.pack_idx = base_pack;
                        pending.offset = base_off;
                        next_pending.push(pending);
                    }
                }
            }
        }

        self.pending_pack_objs = next_pending;
        if self.pending_pack_objs.is_empty() {
            self.phase = Phase::ObjectsStart;
        } else {
            self.phase = Phase::ObjectsReadPackRanges;
        }
        Ok(())
    }

    fn build_result(&mut self) -> PromptStatus<'a> {
        let head = match self.branch_name.take() {
            Some(name) => Head::Branch {
                name,
                oid: self.head_oid,
                short: self.head_oid.map(|o| ShortOid::from_oid(o, self.opts.short_oid_len)),
            },
            None => {
                let oid = self.head_oid.unwrap_or(Oid([0u8; 20]));
                Head::Detached { oid, short: ShortOid::from_oid(oid, self.opts.short_oid_len) }
            }
        };

        let upstream = self.upstream_ref.take().map(|up| Upstream {
            remote: up.remote,
            branch: up.branch,
            oid: self.upstream_oid,
            divergence: self.divergence,
        });

        PromptStatus {
            head,
            tag: self.tag.take(),
            state: self.repo_state,
            upstream,
            counts: self.counts,
            worktree: self.worktree_kind,
            truncated: self.truncated,
            changed_samples: core::mem::replace(&mut self.changed_samples, Vec::new_in(self.alloc)),
        }
    }
}

/// Hash a working-tree regular file as a Git blob, applying `core.autocrlf` CRLF-to-LF
/// conversion for non-binary files when configured.
pub fn hash_worktree_blob(bytes: &[u8], autocrlf: AutoCrlf, alloc: Alloc<'_>) -> Oid {
    if autocrlf == AutoCrlf::False || memchr(b'\r', bytes).is_none() || is_binary(bytes) {
        return Oid::of(ObjectKind::Blob, bytes);
    }
    let mut cleaned = Vec::with_capacity_in(bytes.len(), alloc);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            i += 1;
            continue;
        }
        cleaned.push(bytes[i]);
        i += 1;
    }
    Oid::of(ObjectKind::Blob, &cleaned)
}

/// Git's standard binary sniff (`buffer_is_binary`): NUL byte in the first 8000 bytes.
fn is_binary(bytes: &[u8]) -> bool {
    let probe = &bytes[..bytes.len().min(8000)];
    memchr(0, probe).is_some()
}
