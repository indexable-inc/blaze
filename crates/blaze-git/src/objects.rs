//! Git tree parsing, cache-tree-pruned `HEAD`-vs-index staged diff, and symmetric ahead/behind
//! commit-graph divergence solver.
//!
//! All algorithms are strictly bounded by [`Limits`] and allocate only in the caller's [`Alloc`].

#![blaze::sans_io]

use core::cmp::Ordering;

use blaze_git_fetch::Oid;
use blaze_hash::{BuildKeyHasher, Map, MapOpts};
use blaze_memchr::memchr;
use blaze_own::{AString, AVec, Alloc, Heap4, String, Vec, planted::Planted};

use crate::{
    Detail, Divergence, ErrorKind, GitError, GitFile, GitStatusPlant, Limits,
    commit_graph::CommitGraph,
    index::{IndexEntry, IndexView},
};

/// One entry in a raw Git tree object (`<octal-mode> <name>\0<20-byte-oid>`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TreeEntry<'b> {
    pub mode: u32,
    pub name: &'b [u8],
    pub oid: Oid,
}

impl TreeEntry<'_> {
    #[inline]
    pub const fn is_dir(self) -> bool {
        self.mode == 0o040000
    }
}

/// Zero-copy streaming parser over a Git tree object's body bytes.
pub struct TreeIter<'b> {
    bytes: &'b [u8],
    pos: usize,
    tree_oid: Oid,
}

impl<'b> TreeIter<'b> {
    pub fn new(tree_oid: Oid, bytes: &'b [u8]) -> TreeIter<'b> {
        TreeIter { bytes, pos: 0, tree_oid }
    }

    pub fn next_entry(&mut self) -> Result<Option<TreeEntry<'b>>, GitError> {
        if self.pos >= self.bytes.len() {
            return Ok(None);
        }
        let start = self.pos;
        let rest = &self.bytes[start..];
        let sp = memchr(b' ', rest).ok_or(GitError {
            file: GitFile::TreeObject(self.tree_oid),
            at: start as u64,
            kind: ErrorKind::Malformed,
        })?;
        let mut mode: u32 = 0;
        if sp == 0 || sp > 7 {
            return Err(GitError {
                file: GitFile::TreeObject(self.tree_oid),
                at: start as u64,
                kind: ErrorKind::Malformed,
            });
        }
        for &b in &rest[..sp] {
            if !(b'0'..=b'7').contains(&b) {
                return Err(GitError {
                    file: GitFile::TreeObject(self.tree_oid),
                    at: start as u64,
                    kind: ErrorKind::Malformed,
                });
            }
            mode = (mode << 3) | u32::from(b - b'0');
        }
        let name_start = start + sp + 1;
        let nul = memchr(0, &self.bytes[name_start..]).ok_or(GitError {
            file: GitFile::TreeObject(self.tree_oid),
            at: name_start as u64,
            kind: ErrorKind::Malformed,
        })?;
        let name = &self.bytes[name_start..name_start + nul];
        let oid_start = name_start + nul + 1;
        if oid_start + 20 > self.bytes.len() {
            return Err(GitError {
                file: GitFile::TreeObject(self.tree_oid),
                at: oid_start as u64,
                kind: ErrorKind::Malformed,
            });
        }
        let oid = Oid(self.bytes[oid_start..oid_start + 20].try_into().expect("20 bytes"));
        self.pos = oid_start + 20;
        Ok(Some(TreeEntry { mode, name, oid }))
    }
}

/// Normalize mode for comparing tree entries against index entries.
#[inline]
const fn normalize_mode(mode: u32) -> u32 {
    match mode & 0o170000 {
        0o100000 => {
            if mode & 0o111 != 0 {
                0o100755
            } else {
                0o100644
            }
        }
        other => other,
    }
}

/// Compare two names in Git's tree ordering: directories sort as if suffixed by `'/'`.
fn git_name_cmp(a: &[u8], a_is_dir: bool, b: &[u8], b_is_dir: bool) -> Ordering {
    let n = a.len().min(b.len());
    match a[..n].cmp(&b[..n]) {
        Ordering::Equal => {
            let ca = a.get(n).copied().or_else(|| a_is_dir.then_some(b'/'));
            let cb = b.get(n).copied().or_else(|| b_is_dir.then_some(b'/'));
            ca.cmp(&cb)
        }
        ord => ord,
    }
}

/// Staged diff counts computed between `HEAD` tree and `.git/index`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StagedCounts {
    pub added: u32,
    pub modified: u32,
    pub deleted: u32,
    pub typechanged: u32,
}

impl StagedCounts {
    #[inline]
    pub const fn total(self) -> u32 {
        self.added + self.modified + self.deleted + self.typechanged
    }
}

/// One pending directory comparison between a `HEAD` tree object and a slice of `.git/index`.
pub struct DiffDir<'a> {
    pub prefix: AString<'a>,
    pub tree_oid: Oid,
    pub cache_node: Option<u32>,
    pub index_start: usize,
    pub index_end: usize,
    pub depth: u16,
}

/// Incremental sans-IO staged-diff engine with `TREE` cache-tree subtree pruning.
pub struct StagedDiff<'a> {
    queue: AVec<'a, DiffDir<'a>>,
    pub counts: StagedCounts,
    detail: Detail,
    limits: Limits,
    alloc: Alloc<'a>,
}

impl<'a> StagedDiff<'a> {
    /// Start diffing `head_tree` against `index`.
    ///
    /// Returns immediately with an empty queue (0 tree reads!) when `index`'s root cache-tree OID
    /// equals `head_tree`.
    pub fn start(
        head_tree: Option<Oid>,
        index: IndexView<'_>,
        detail: Detail,
        limits: Limits,
        alloc: Alloc<'a>,
        plant: impl Planted<GitStatusPlant>,
    ) -> StagedDiff<'a> {
        let mut queue = Vec::new_in(alloc);
        let mut counts = StagedCounts::default();

        if plant.has(GitStatusPlant::SkipStagedDiff) {
            return StagedDiff { queue, counts, detail, limits, alloc };
        }

        let Some(root_tree) = head_tree else {
            // Unborn HEAD: every non-intent-to-add stage-0 entry is staged_added.
            for entry in index.entries.iter() {
                if !entry.intent_to_add {
                    counts.added = counts.added.saturating_add(1);
                    if detail == Detail::Presence {
                        break;
                    }
                }
            }
            return StagedDiff { queue, counts, detail, limits, alloc };
        };

        // O(1) cache-tree root check!
        if index.cache_tree_root_oid() == Some(root_tree) {
            return StagedDiff { queue, counts, detail, limits, alloc };
        }

        let root_node = index.cache_tree.and_then(|nodes| (!nodes.is_empty()).then_some(0u32));
        queue.push(DiffDir {
            prefix: String::new_in(alloc),
            tree_oid: root_tree,
            cache_node: root_node,
            index_start: 0,
            index_end: index.entries.len(),
            depth: 0,
        });
        StagedDiff { queue, counts, detail, limits, alloc }
    }

    pub fn is_done(&self) -> bool {
        self.queue.is_empty() || (self.detail == Detail::Presence && self.counts.total() > 0)
    }

    /// Collect all tree OIDs needed by currently queued directories.
    pub fn needed_trees(&self, out: &mut AVec<'_, Oid>) {
        out.clear();
        if self.is_done() {
            return;
        }
        for item in self.queue.iter() {
            if out.scan_for(&item.tree_oid).is_none() {
                out.push(item.tree_oid);
            }
        }
    }

    /// Advance all currently queued directories using `resolve_tree` (which returns the raw tree
    /// object body for each OID requested in [`Self::needed_trees`]).
    pub fn step<'t>(
        &mut self,
        index: IndexView<'_>,
        mut resolve_tree: impl FnMut(Oid) -> Option<&'t [u8]>,
    ) -> Result<(), GitError> {
        if self.is_done() {
            self.queue.clear();
            return Ok(());
        }
        let batch = core::mem::replace(&mut self.queue, Vec::new_in(self.alloc));
        for dir in batch {
            if self.detail == Detail::Presence && self.counts.total() > 0 {
                break;
            }
            let Some(tree_bytes) = resolve_tree(dir.tree_oid) else {
                return Err(GitError {
                    file: GitFile::TreeObject(dir.tree_oid),
                    at: 0,
                    kind: ErrorKind::ObjectMissing(dir.tree_oid),
                });
            };
            self.diff_one_dir(index, &dir, tree_bytes)?;
        }
        Ok(())
    }

    fn diff_one_dir(
        &mut self,
        index: IndexView<'_>,
        dir: &DiffDir<'a>,
        tree_bytes: &[u8],
    ) -> Result<(), GitError> {
        let prefix_len = if dir.prefix.is_empty() { 0 } else { dir.prefix.len() + 1 };
        let mut tree_iter = TreeIter::new(dir.tree_oid, tree_bytes);
        let mut next_tree = tree_iter.next_entry()?;
        let mut idx_pos = dir.index_start;

        loop {
            if self.detail == Detail::Presence && self.counts.total() > 0 {
                return Ok(());
            }

            // Determine next index group (either a direct file entry or a subdirectory span).
            let idx_group = if idx_pos < dir.index_end {
                let first_path = index.path_bytes(&index.entries[idx_pos]);
                let rel = &first_path[prefix_len..];
                if let Some(slash) = memchr(b'/', rel) {
                    let sub_name = &rel[..slash];
                    let sub_full_len = prefix_len + slash;
                    let mut group_end = idx_pos + 1;
                    while group_end < dir.index_end {
                        let p = index.path_bytes(&index.entries[group_end]);
                        if p.len() > sub_full_len
                            && p[prefix_len..sub_full_len] == *sub_name
                            && p[sub_full_len] == b'/'
                        {
                            group_end += 1;
                        } else {
                            break;
                        }
                    }
                    Some((sub_name, true, idx_pos, group_end))
                } else {
                    Some((rel, false, idx_pos, idx_pos + 1))
                }
            } else {
                None
            };

            match (next_tree, idx_group) {
                (None, None) => break,
                (Some(te), None) => {
                    self.record_head_only(te, dir)?;
                    next_tree = tree_iter.next_entry()?;
                }
                (None, Some((_, _, g_start, g_end))) => {
                    self.record_index_only(&index.entries[g_start..g_end]);
                    idx_pos = g_end;
                }
                (Some(te), Some((idx_name, idx_is_dir, g_start, g_end))) => {
                    match git_name_cmp(te.name, te.is_dir(), idx_name, idx_is_dir) {
                        Ordering::Less => {
                            self.record_head_only(te, dir)?;
                            next_tree = tree_iter.next_entry()?;
                        }
                        Ordering::Greater => {
                            self.record_index_only(&index.entries[g_start..g_end]);
                            idx_pos = g_end;
                        }
                        Ordering::Equal => {
                            if te.is_dir() && idx_is_dir {
                                // Check cache-tree subtree pruning!
                                let child_cache = dir
                                    .cache_node
                                    .and_then(|parent_node| index.cache_tree_find_child(parent_node, te.name));
                                let pruned = child_cache.is_some_and(|c_idx| {
                                    index
                                        .cache_tree
                                        .and_then(|nodes| nodes[c_idx as usize].oid)
                                        == Some(te.oid)
                                });
                                if !pruned {
                                    self.enqueue_subdir(
                                        dir,
                                        te.name,
                                        te.oid,
                                        child_cache,
                                        g_start,
                                        g_end,
                                    )?;
                                }
                            } else if !te.is_dir() && !idx_is_dir {
                                let entry = &index.entries[g_start];
                                if !entry.intent_to_add {
                                    let head_type = te.mode & 0o170000;
                                    let idx_type = entry.mode & 0o170000;
                                    if head_type != idx_type {
                                        self.counts.typechanged =
                                            self.counts.typechanged.saturating_add(1);
                                    } else if te.oid != entry.oid
                                        || normalize_mode(te.mode) != normalize_mode(entry.mode)
                                    {
                                        self.counts.modified =
                                            self.counts.modified.saturating_add(1);
                                    }
                                }
                            } else if te.is_dir() && !idx_is_dir {
                                // Directory replaced by file in index.
                                self.record_head_only(te, dir)?;
                                self.record_index_only(&index.entries[g_start..g_end]);
                            } else {
                                // File replaced by directory in index.
                                self.counts.deleted = self.counts.deleted.saturating_add(1);
                                self.record_index_only(&index.entries[g_start..g_end]);
                            }
                            next_tree = tree_iter.next_entry()?;
                            idx_pos = g_end;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn record_head_only(&mut self, te: TreeEntry<'_>, dir: &DiffDir<'a>) -> Result<(), GitError> {
        if te.is_dir() {
            if self.detail == Detail::Presence {
                // Any non-empty tree in Git has at least 1 blob leaf.
                self.counts.deleted = self.counts.deleted.saturating_add(1);
                return Ok(());
            }
            self.enqueue_subdir(dir, te.name, te.oid, None, 0, 0)
        } else {
            self.counts.deleted = self.counts.deleted.saturating_add(1);
            Ok(())
        }
    }

    fn record_index_only(&mut self, entries: &[IndexEntry]) {
        for entry in entries {
            if !entry.intent_to_add {
                self.counts.added = self.counts.added.saturating_add(1);
                if self.detail == Detail::Presence {
                    return;
                }
            }
        }
    }

    fn enqueue_subdir(
        &mut self,
        parent: &DiffDir<'a>,
        name: &[u8],
        tree_oid: Oid,
        cache_node: Option<u32>,
        index_start: usize,
        index_end: usize,
    ) -> Result<(), GitError> {
        let next_depth = parent.depth.saturating_add(1);
        if next_depth > self.limits.most_dir_depth {
            return Err(GitError {
                file: GitFile::TreeObject(tree_oid),
                at: 0,
                kind: ErrorKind::DepthExceeded { most: self.limits.most_dir_depth },
            });
        }
        let mut sub_prefix = String::from_str_in(parent.prefix.as_str(), self.alloc);
        if !sub_prefix.is_empty() {
            sub_prefix.push('/');
        }
        if let Ok(s) = core::str::from_utf8(name) {
            sub_prefix.push_str(s);
        }
        self.queue.push(DiffDir {
            prefix: sub_prefix,
            tree_oid,
            cache_node,
            index_start,
            index_end,
            depth: next_depth,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Symmetric Ahead/Behind Commit-Graph Divergence Solver
// ---------------------------------------------------------------------------

const COLOR_HEAD: u8 = 1;
const COLOR_UPSTREAM: u8 = 2;
const COLOR_COMMON: u8 = 3;
const COLOR_MASK: u8 = 3;
/// Bit set on a commit in `colors` while it is queued in `heap` as a non-common commit.
/// Cleared either when popped or when upgraded to `COLOR_COMMON`, guaranteeing `non_common_in_heap`
/// stays exact even when a queued `HEAD`/`UPSTREAM` commit becomes `COMMON` before it is popped.
const IN_HEAP_NON_COMMON: u8 = 4;

/// Priority-queue key for commit traversal: highest `(generation, commit_time)` pops first
/// (`Heap4` is a min-heap, so we invert the order via `Reverse`-like wrapper).
#[derive(Clone, Copy, PartialEq, Eq)]
struct QueueItem {
    generation: u32,
    commit_time: u64,
    oid: Oid,
}

impl Ord for QueueItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // Min-heap pops smallest: invert so largest (generation, commit_time) pops first!
        other
            .generation
            .cmp(&self.generation)
            .then_with(|| other.commit_time.cmp(&self.commit_time))
            .then_with(|| self.oid.cmp(&other.oid))
    }
}

impl PartialOrd for QueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Parent links and ordering hints for one commit, from either `CommitGraph` or a raw commit object.
#[derive(Clone, Copy)]
pub struct CommitInfo {
    pub generation: u32,
    pub commit_time: u64,
    pub parent_count: u8,
    pub parents: [Oid; 8],
}

impl CommitInfo {
    pub fn from_raw_commit(commit: &blaze_git_fetch::store::Commit<'_>) -> CommitInfo {
        let mut parents = [Oid([0u8; 20]); 8];
        let mut count = 0u8;
        for p in commit.parents().take(8) {
            parents[usize::from(count)] = p;
            count += 1;
        }
        CommitInfo {
            generation: 0,
            commit_time: commit.committed.max(0) as u64,
            parent_count: count,
            parents,
        }
    }

    pub fn from_graph(graph: &CommitGraph<'_, '_>, oid: &Oid, scratch: &mut AVec<'_, u32>) -> Option<CommitInfo> {
        let gc = graph.lookup(oid)?;
        let (layer_idx, _) = graph.get(gc.graph_pos)?;
        scratch.clear();
        graph.parents_into(layer_idx, &gc, scratch);
        let mut parents = [Oid([0u8; 20]); 8];
        let mut count = 0u8;
        for &p_pos in scratch.iter().take(8) {
            if let Some(p_oid) = graph.oid_of(p_pos) {
                parents[usize::from(count)] = p_oid;
                count += 1;
            }
        }
        Some(CommitInfo {
            generation: gc.generation,
            commit_time: gc.commit_time,
            parent_count: count,
            parents,
        })
    }
}

/// Outcome of one step of [`AheadBehindWalker`].
pub enum WalkStep {
    /// Need parent metadata for these commits (not found in `CommitGraph`).
    NeedCommits,
    /// Finished computing divergence.
    Done(Divergence),
}

/// Incremental sans-IO symmetric ahead/behind walker (`paint_down_to_common` + count).
///
/// Uses `CommitGraph` for O(1) zero-IO lookup whenever available, and only requests raw commit
/// objects when a commit is absent from the commit-graph.
pub struct AheadBehindWalker<'a> {
    head: Oid,
    upstream: Oid,
    colors: Map<Oid, u8, BuildKeyHasher, Alloc<'a>>,
    info_cache: Map<Oid, CommitInfo, BuildKeyHasher, Alloc<'a>>,
    heap: Heap4<QueueItem, Alloc<'a>>,
    /// Commits we need raw commit objects for before continuing.
    pending_lookup: AVec<'a, Oid>,
    /// Commits queued to be pushed once their `CommitInfo` arrives.
    pending_push: AVec<'a, (Oid, u8)>,
    non_common_in_heap: u32,
    explored: u32,
    most_commits: u32,
    done: Option<Divergence>,
    scratch_parents: AVec<'a, u32>,
}

impl<'a> AheadBehindWalker<'a> {
    pub fn new(head: Oid, upstream: Oid, limits: Limits, alloc: Alloc<'a>) -> AheadBehindWalker<'a> {
        let mut colors =
            Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: Some(64) });
        let info_cache =
            Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: Some(64) });
        let heap = Heap4::new_in(alloc);
        let pending_lookup = Vec::new_in(alloc);
        let mut pending_push = Vec::new_in(alloc);
        let scratch_parents = Vec::new_in(alloc);

        let done = if head == upstream {
            Some(Divergence::Equal)
        } else {
            colors.insert(head, COLOR_HEAD);
            colors.insert(upstream, COLOR_UPSTREAM);
            pending_push.push((head, COLOR_HEAD));
            pending_push.push((upstream, COLOR_UPSTREAM));
            None
        };

        AheadBehindWalker {
            head,
            upstream,
            colors,
            info_cache,
            heap,
            pending_lookup,
            pending_push,
            non_common_in_heap: 0,
            explored: 0,
            most_commits: limits.most_commits,
            done,
            scratch_parents,
        }
    }

    pub fn missing_commits(&self) -> &[Oid] {
        &self.pending_lookup
    }

    /// Supply `CommitInfo` parsed from loose/pack commit objects for previously requested OIDs.
    pub fn supply_commit(&mut self, oid: Oid, info: CommitInfo) {
        self.info_cache.insert(oid, info);
    }

    /// Run as far as possible using `graph` (if present) and cached commit metadata.
    pub fn advance(
        &mut self,
        graph: Option<&CommitGraph<'_, '_>>,
        plant: impl Planted<GitStatusPlant>,
    ) -> WalkStep {
        if let Some(div) = self.done {
            return WalkStep::Done(div);
        }

        // First flush any `pending_push` whose CommitInfo is now available.
        self.pending_lookup.clear();
        let mut still_waiting = Vec::new_in(*self.colors.allocator());
        let mut i = 0;
        while i < self.pending_push.len() {
            let (oid, color) = self.pending_push[i];
            i += 1;
            if let Some(info) = self.lookup_info(oid, graph) {
                self.heap.push(QueueItem {
                    generation: info.generation,
                    commit_time: info.commit_time,
                    oid,
                });
                let cur = self.colors.get(&oid).copied().unwrap_or(0);
                if (cur & COLOR_MASK) != COLOR_COMMON && (cur & IN_HEAP_NON_COMMON) == 0 {
                    self.colors.insert(oid, cur | IN_HEAP_NON_COMMON);
                    self.non_common_in_heap += 1;
                }
            } else {
                still_waiting.push((oid, color));
                if self.pending_lookup.scan_for(&oid).is_none() {
                    self.pending_lookup.push(oid);
                }
            }
        }
        self.pending_push = still_waiting;
        if !self.pending_lookup.is_empty() {
            return WalkStep::NeedCommits;
        }

        // Main paint-down-to-common loop.
        while self.non_common_in_heap > 0 {
            if self.explored >= self.most_commits {
                let (ahead, behind) = self.tally();
                let div = Divergence::Capped { ahead_at_least: ahead, behind_at_least: behind };
                self.done = Some(div);
                return WalkStep::Done(div);
            }
            let Some(item) = self.heap.pop() else {
                break;
            };
            let raw_flags = self.colors.get(&item.oid).copied().unwrap_or(0);
            let my_color = raw_flags & COLOR_MASK;
            if raw_flags & IN_HEAP_NON_COMMON != 0 {
                self.colors.insert(item.oid, my_color);
                self.non_common_in_heap -= 1;
            }
            self.explored += 1;

            let Some(info) = self.lookup_info(item.oid, graph) else {
                // Re-queue to fetch info.
                self.pending_push.push((item.oid, my_color));
                self.pending_lookup.push(item.oid);
                return WalkStep::NeedCommits;
            };

            let parent_limit = if plant.has(GitStatusPlant::SkipMergeSecondParent) {
                usize::from(info.parent_count.min(1))
            } else {
                usize::from(info.parent_count)
            };

            let mut need_fetch = false;
            for p_idx in 0..parent_limit {
                let parent = info.parents[p_idx];
                let prev_flags = self.colors.get(&parent).copied().unwrap_or(0);
                let prev_color = prev_flags & COLOR_MASK;
                let next_color = prev_color | my_color;
                if next_color == prev_color {
                    continue;
                }
                let mut next_flags = next_color;
                if next_color == COLOR_COMMON && (prev_flags & IN_HEAP_NON_COMMON) != 0 {
                    // This parent was already sitting in the heap as non-common: cancel its count!
                    self.non_common_in_heap -= 1;
                } else if prev_flags & IN_HEAP_NON_COMMON != 0 {
                    next_flags |= IN_HEAP_NON_COMMON;
                }
                self.colors.insert(parent, next_flags);
                if let Some(p_info) = self.lookup_info(parent, graph) {
                    self.heap.push(QueueItem {
                        generation: p_info.generation,
                        commit_time: p_info.commit_time,
                        oid: parent,
                    });
                    if next_color != COLOR_COMMON && (next_flags & IN_HEAP_NON_COMMON) == 0 {
                        self.colors.insert(parent, next_flags | IN_HEAP_NON_COMMON);
                        self.non_common_in_heap += 1;
                    }
                } else {
                    self.pending_push.push((parent, next_color));
                    if self.pending_lookup.scan_for(&parent).is_none() {
                        self.pending_lookup.push(parent);
                    }
                    need_fetch = true;
                }
            }
            if need_fetch {
                return WalkStep::NeedCommits;
            }
        }

        let (ahead, behind) = self.tally();
        let div = if ahead == 0 && behind == 0 && self.head == self.upstream {
            Divergence::Equal
        } else {
            Divergence::Counted { ahead, behind }
        };
        self.done = Some(div);
        WalkStep::Done(div)
    }

    fn lookup_info(&mut self, oid: Oid, graph: Option<&CommitGraph<'_, '_>>) -> Option<CommitInfo> {
        if let Some(&info) = self.info_cache.get(&oid) {
            return Some(info);
        }
        if let Some(g) = graph
            && let Some(info) = CommitInfo::from_graph(g, &oid, &mut self.scratch_parents)
        {
            self.info_cache.insert(oid, info);
            return Some(info);
        }
        None
    }

    fn tally(&self) -> (u32, u32) {
        let mut ahead = 0u32;
        let mut behind = 0u32;
        for (_, &flags) in self.colors.iter() {
            let color = flags & COLOR_MASK;
            if color == COLOR_HEAD {
                ahead = ahead.saturating_add(1);
            } else if color == COLOR_UPSTREAM {
                behind = behind.saturating_add(1);
            }
        }
        (ahead, behind)
    }
}
