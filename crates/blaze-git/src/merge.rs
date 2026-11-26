//! Sans-IO, ORT-style (Ostensibly Recursive's Twin) 3-way Git merge engine.
//!
//! Operates directly over any [`ObjectStore`] with zero working-tree IO, zero `std`, and zero
//! third-party dependencies:
//! - **Merge-base / LCA solver** ([`find_merge_bases`]): finds minimal lowest common ancestors
//!   using optional [`CommitGraph`] generation numbers (O(d log N), zero commit decompression) or
//!   commit objects in `ObjectStore`, and handles criss-cross merges via virtual merge-base trees.
//! - **O(changed-subtrees) 3-way tree merge** ([`merge_trees`]): skips any subtree where
//!   `ours == theirs`, `base == ours`, or `base == theirs` in O(1) without reading a single child
//!   tree or blob, and records `MergeStats::subtrees_skipped` for floor verification.
//! - **Content-level 3-way line merge** ([`merge_blobs`]): binary detection (`NUL` in first 8 KiB),
//!   bounded Myers O(ND) shortest-edit-script line diff, and 3-way hunk reconciliation emitting
//!   standard Git conflict markers (`merge` or `diff3`).

#![blaze::sans_io]

use core::cmp::Ordering;

use blaze_git_fetch::{Kind as ObjectKind, Oid, store::Commit};
use blaze_hash::{BuildKeyHasher, FixedState, Map, MapOpts};
use blaze_memchr::memchr;
use blaze_own::{
    AString, AVec, Alloc, String, Vec,
    planted::{NoPlant, Planted},
};

use crate::{
    ErrorKind, GitError, GitFile,
    commit_graph::CommitGraph,
    objects::TreeIter,
    plant::GitEnginePlant,
    storage::{ObjectStore, TreeBuildEntry, encode_tree, git_tree_name_cmp},
};

/// Bounds on every merge walk and diff step (rule `bounded-progress`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MergeLimits {
    /// Maximum commits explored when computing merge bases.
    pub most_commits: u32,
    /// Maximum tree nesting depth.
    pub most_tree_depth: u16,
    /// Maximum recursive virtual merge-base depth for criss-cross histories.
    pub most_virtual_depth: u8,
    /// Maximum Myers diff edit distance `d` per file before falling back to whole-file conflict.
    pub most_diff_d: usize,
    /// Maximum lines per blob for line-level diff.
    pub most_lines: usize,
}

impl MergeLimits {
    pub const SHIPPED: MergeLimits = MergeLimits {
        most_commits: 65_536,
        most_tree_depth: 128,
        most_virtual_depth: 8,
        most_diff_d: 4_096,
        most_lines: 250_000,
    };
}

/// Conflict marker rendering style.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConflictStyle {
    /// Standard two-way markers (`<<<<<<<`, `=======`, `>>>>>>>`).
    Merge,
    /// Three-way markers including the base section (`|||||||`).
    Diff3,
}

/// Options for [`merge_commits`] and [`merge_trees`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MergeOpts<'b> {
    pub ours_label: &'b str,
    pub base_label: &'b str,
    pub theirs_label: &'b str,
    pub conflict_style: ConflictStyle,
    pub detect_exact_renames: bool,
    pub limits: MergeLimits,
}

impl<'b> MergeOpts<'b> {
    pub const DEFAULT: MergeOpts<'static> = MergeOpts {
        ours_label: "ours",
        base_label: "base",
        theirs_label: "theirs",
        conflict_style: ConflictStyle::Merge,
        detect_exact_renames: true,
        limits: MergeLimits::SHIPPED,
    };
}

/// Kind of conflict detected during a 3-way tree merge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConflictKind {
    /// Both sides modified the same text file with overlapping differing edits.
    Content,
    /// Both sides modified the same binary file differently.
    Binary,
    /// Both sides added different content at the same path with no base entry.
    AddAdd,
    /// One side modified a file while the other side deleted it.
    ModifyDelete,
    /// One side has a directory and the other side has a file/symlink at the same path.
    DirectoryFile,
    /// Both sides changed the entry mode incompatibly (e.g. file vs symlink or 644 vs 755).
    Mode,
}

/// One tree entry reference `(mode, oid)` stored for conflict stage 1 (base), 2 (ours), 3 (theirs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StageEntry {
    pub mode: u32,
    pub oid: Oid,
}

/// One structured conflict emitted by [`merge_trees`] or [`merge_commits`].
#[derive(PartialEq, Eq, Debug)]
pub struct MergeConflict<'a> {
    pub path: AString<'a>,
    pub kind: ConflictKind,
    pub base: Option<StageEntry>,
    pub ours: Option<StageEntry>,
    pub theirs: Option<StageEntry>,
}

/// High-level outcome of [`merge_commits`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MergeStatus {
    /// `theirs` is already an ancestor of `ours` (or equal): no work needed.
    AlreadyUpToDate,
    /// `ours` is a strict ancestor of `theirs`: fast-forward to `theirs`.
    FastForward,
    /// Clean 3-way merge with zero conflicts.
    MergedClean,
    /// 3-way merge completed with one or more conflicts recorded in `conflicts`.
    Conflicted,
}

/// Cost counters proving O(changed) subtree pruning.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MergeStats {
    pub subtrees_skipped: u32,
    pub subtrees_merged: u32,
    pub blobs_merged: u32,
    pub renames_detected: u32,
}

/// Complete output of [`merge_commits`] or [`merge_trees`].
#[derive(PartialEq, Eq, Debug)]
pub struct MergeResult<'a> {
    pub status: MergeStatus,
    pub tree: Oid,
    pub bases: AVec<'a, Oid>,
    pub conflicts: AVec<'a, MergeConflict<'a>>,
    pub stats: MergeStats,
}

// ---------------------------------------------------------------------------
// Merge-base / LCA solver
// ---------------------------------------------------------------------------

const FLAG_OURS: u8 = 1 << 0;
const FLAG_THEIRS: u8 = 1 << 1;
const FLAG_BOTH: u8 = FLAG_OURS | FLAG_THEIRS;
const FLAG_STALE: u8 = 1 << 2;

struct AncNode<'a> {
    flags: u8,
    generation: u32,
    commit_time: u64,
    parents: AVec<'a, Oid>,
}

fn load_commit_meta<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    graph: Option<&CommitGraph<'_, 'a>>,
    oid: Oid,
    alloc: Alloc<'a>,
    scratch: &mut AVec<'a, u8>,
) -> Result<(u32, u64, AVec<'a, Oid>), GitError> {
    if let Some(g) = graph
        && let Some(c) = g.lookup(&oid)
        && let Some((layer_idx, _)) = g.get(c.graph_pos)
    {
        let mut parents = Vec::new_in(alloc);
        let mut p_positions = Vec::new_in(alloc);
        g.parents_into(layer_idx, &c, &mut p_positions);
        for p in p_positions {
            if let Some(p_oid) = g.oid_of(p) {
                parents.push(p_oid);
            }
        }
        return Ok((c.generation, c.commit_time, parents));
    }
    let Some(kind) = store.read_object(&oid, scratch)? else {
        return Err(GitError {
            file: GitFile::CommitObject(oid),
            at: 0,
            kind: ErrorKind::ObjectMissing(oid),
        });
    };
    if kind != ObjectKind::Commit {
        return Err(GitError {
            file: GitFile::CommitObject(oid),
            at: 0,
            kind: ErrorKind::WrongObjectKind { want: ObjectKind::Commit, got: kind },
        });
    }
    let parsed = Commit::parse(scratch).map_err(|(at, why)| GitError {
        file: GitFile::CommitObject(oid),
        at,
        kind: ErrorKind::CommitParse(why),
    })?;
    let mut parents = Vec::new_in(alloc);
    for p in parsed.parents() {
        parents.push(p);
    }
    Ok((0, u64::try_from(parsed.committed).unwrap_or(0), parents))
}

/// Finds all minimal Lowest Common Ancestors (merge bases) of `ours` and `theirs`, ordered
/// deterministically (newest commit time / OID first).
pub fn find_merge_bases<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    graph: Option<&CommitGraph<'_, 'a>>,
    ours: Oid,
    theirs: Oid,
    limits: MergeLimits,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, Oid>, GitError> {
    let mut out = Vec::new_in(alloc);
    if ours == theirs {
        out.push(ours);
        return Ok(out);
    }

    let mut nodes: Map<Oid, AncNode<'a>, BuildKeyHasher, Alloc<'a>> =
        Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: Some(64) });
    let mut scratch = Vec::new_in(alloc);

    // Queue of commits to propagate flags from; always pop highest (generation, commit_time) first
    // so common ancestors are discovered before exploring arbitrarily deep history.
    let mut queue: AVec<'a, (u32, u64, Oid)> = Vec::new_in(alloc);

    for (seed, flag) in [(ours, FLAG_OURS), (theirs, FLAG_THEIRS)] {
        if let Some(existing) = nodes.get_mut(&seed) {
            existing.flags |= flag;
        } else {
            let (generation, time, parents) =
                load_commit_meta(store, graph, seed, alloc, &mut scratch)?;
            nodes.insert(seed, AncNode { flags: flag, generation, commit_time: time, parents });
            queue.push((generation, time, seed));
        }
    }

    let mut visited: u32 = 0;
    let mut common: AVec<'a, Oid> = Vec::new_in(alloc);

    while !queue.is_empty() {
        // Check if every remaining queued node is already STALE (we can stop early!).
        let has_non_stale = queue.iter().any(|(_, _, id)| {
            nodes.get(id).is_some_and(|n| (n.flags & FLAG_STALE) == 0)
        });
        if !has_non_stale {
            break;
        }

        // Pop highest (generation, commit_time, oid) deterministically.
        let mut best_i = 0usize;
        for i in 1..queue.len() {
            if queue[i] > queue[best_i] {
                best_i = i;
            }
        }
        let (_, _, cur_oid) = queue.remove_swap(best_i);
        visited += 1;
        if visited > limits.most_commits {
            return Err(GitError {
                file: GitFile::Merge,
                at: u64::from(visited),
                kind: ErrorKind::BudgetExceeded,
            });
        }

        let (cur_flags, parents_copy) = {
            let node = nodes.get_mut(&cur_oid).expect("in map");
            let mut flags = node.flags;
            if (flags & FLAG_BOTH) == FLAG_BOTH && (flags & FLAG_STALE) == 0 {
                common.push(cur_oid);
                // Mark this node's ancestors as STALE so ancestors of a merge base aren't also bases.
                flags |= FLAG_STALE;
                node.flags = flags;
            }
            let mut ps = Vec::with_capacity_in(node.parents.len(), alloc);
            ps.extend_from_slice(&node.parents);
            (flags, ps)
        };

        let prop_flags = cur_flags;
        for p_oid in parents_copy {
            let need_push = if let Some(p_node) = nodes.get_mut(&p_oid) {
                let before = p_node.flags;
                p_node.flags |= prop_flags;
                p_node.flags != before
            } else {
                let (generation, time, parents) =
                    load_commit_meta(store, graph, p_oid, alloc, &mut scratch)?;
                nodes.insert(
                    p_oid,
                    AncNode { flags: prop_flags, generation, commit_time: time, parents },
                );
                true
            };
            if need_push {
                let p_node = nodes.get(&p_oid).expect("inserted");
                queue.push((p_node.generation, p_node.commit_time, p_oid));
            }
        }
    }

    // Keep only common ancestors that were NOT marked STALE by a descendant common ancestor.
    // Wait: a common ancestor pushed into `common` had `FLAG_STALE` set on itself to propagate
    // STALE to its parents; to distinguish "was already stale before being discovered" from
    // "marked stale by itself", run a clean reachability filter over `common` (typically 1-3 items!).
    if common.len() <= 1 {
        return Ok(common);
    }

    // Deduplicate `common`.
    common.sort_unstable();
    let mut uniq = Vec::with_capacity_in(common.len(), alloc);
    for id in common {
        if uniq.last() != Some(&id) {
            uniq.push(id);
        }
    }

    // Filter out any candidate in `uniq` that is reachable from another candidate in `uniq`.
    let mut redundant: AVec<'a, bool> = Vec::with_capacity_in(uniq.len(), alloc);
    for _ in 0..uniq.len() {
        redundant.push(false);
    }
    let mut stack = Vec::new_in(alloc);
    let mut seen: Map<Oid, (), BuildKeyHasher, Alloc<'a>> =
        Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: Some(32) });

    for i in 0..uniq.len() {
        if redundant[i] {
            continue;
        }
        seen.clear();
        stack.clear();
        if let Some(n) = nodes.get(&uniq[i]) {
            for &p in &n.parents {
                stack.push(p);
            }
        }
        let mut steps = 0u32;
        while let Some(cur) = stack.pop() {
            if seen.contains_key(&cur) {
                continue;
            }
            seen.insert(cur, ());
            steps += 1;
            if steps > limits.most_commits {
                break;
            }
            for j in 0..uniq.len() {
                if j != i && uniq[j] == cur {
                    redundant[j] = true;
                }
            }
            if let Some(n) = nodes.get(&cur) {
                for &p in &n.parents {
                    stack.push(p);
                }
            }
        }
    }

    for i in 0..uniq.len() {
        if !redundant[i] {
            out.push(uniq[i]);
        }
    }
    // Sort deterministically by (commit_time desc, oid asc).
    out.sort_by(|a, b| {
        let ta = nodes.get(a).map_or(0, |n| n.commit_time);
        let tb = nodes.get(b).map_or(0, |n| n.commit_time);
        tb.cmp(&ta).then_with(|| a.cmp(b))
    });
    Ok(out)
}

// ---------------------------------------------------------------------------
// Content-level 3-way line merge (Myers O(ND) diff + 3-way hunk reconciliation)
// ---------------------------------------------------------------------------

/// Result of 3-way blob merge.
#[derive(PartialEq, Eq, Debug)]
pub enum BlobMerge<'a> {
    Clean(AVec<'a, u8>),
    Conflicted(AVec<'a, u8>),
    BinaryConflict,
}

/// Returns `true` if `bytes` looks binary (contains a NUL byte in the first 8,000 bytes, matching
/// Git's `buffer_is_binary` heuristic).
pub fn is_binary(bytes: &[u8]) -> bool {
    let probe = &bytes[..bytes.len().min(8_000)];
    memchr(0, probe).is_some()
}

fn split_lines<'b, 'a>(bytes: &'b [u8], alloc: Alloc<'a>) -> AVec<'a, &'b [u8]> {
    let mut lines = Vec::new_in(alloc);
    let mut pos = 0usize;
    while pos < bytes.len() {
        match memchr(b'\n', &bytes[pos..]) {
            Some(off) => {
                let end = pos + off + 1;
                lines.push(&bytes[pos..end]);
                pos = end;
            }
            None => {
                lines.push(&bytes[pos..]);
                break;
            }
        }
    }
    lines
}

/// One changed hunk between `base[base_start..base_end]` and `target[target_start..target_end]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct DiffHunk {
    base_start: usize,
    base_end: usize,
    target_start: usize,
    target_end: usize,
}

/// Bounded Myers O(ND) shortest-edit-script diff producing minimal [`DiffHunk`]s from `a` to `b`.
/// Returns `None` if edit distance exceeds `max_d` (caller degrades gracefully to whole-file hunk).
fn myers_diff_hunks<'a>(
    a: &[&[u8]],
    b: &[&[u8]],
    max_d: usize,
    alloc: Alloc<'a>,
) -> Option<AVec<'a, DiffHunk>> {
    // Trim common prefix and common suffix in O(N) first!
    let mut prefix = 0usize;
    let min_len = a.len().min(b.len());
    while prefix < min_len && a[prefix] == b[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while prefix + suffix < min_len
        && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let a_mid = &a[prefix..a.len() - suffix];
    let b_mid = &b[prefix..b.len() - suffix];
    let n = a_mid.len();
    let m = b_mid.len();

    let mut hunks = Vec::new_in(alloc);
    if n == 0 && m == 0 {
        return Some(hunks);
    }
    if n == 0 || m == 0 {
        hunks.push(DiffHunk {
            base_start: prefix,
            base_end: prefix + n,
            target_start: prefix,
            target_end: prefix + m,
        });
        return Some(hunks);
    }

    let limit = (n + m).min(max_d);
    let offset = limit as isize;
    let width = 2 * limit + 1;
    let mut v: AVec<'a, isize> = Vec::with_capacity_in(width, alloc);
    for _ in 0..width {
        v.push(0);
    }
    // Store snapshots of V for backtracking: only the active window `[-d..=d]` per step d!
    let mut trace_data: AVec<'a, isize> = Vec::new_in(alloc);
    let mut trace_starts: AVec<'a, usize> = Vec::with_capacity_in(limit + 1, alloc);

    let mut found_d: Option<usize> = None;
    'outer: for d in 0..=limit {
        trace_starts.push(trace_data.len());
        let d_i = d as isize;
        let mut k = -d_i;
        while k <= d_i {
            let idx = (k + offset) as usize;
            let mut x = if k == -d_i || (k != d_i && v[idx - 1] < v[idx + 1]) {
                v[idx + 1]
            } else {
                v[idx - 1] + 1
            };
            let mut y = x - k;
            while (x as usize) < n && (y as usize) < m && a_mid[x as usize] == b_mid[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if (x as usize) >= n && (y as usize) >= m {
                // Save final d snapshot and stop.
                let lo = (offset - d_i) as usize;
                let hi = (offset + d_i) as usize;
                trace_data.extend_from_slice(&v[lo..=hi]);
                found_d = Some(d);
                break 'outer;
            }
            k += 2;
        }
        let lo = (offset - d_i) as usize;
        let hi = (offset + d_i) as usize;
        trace_data.extend_from_slice(&v[lo..=hi]);
    }

    let final_d = found_d?;

    // Backtrack matching pairs `(ax, by)` from `(n, m)` to `(0, 0)`.
    let mut matches: AVec<'a, (usize, usize)> = Vec::new_in(alloc);
    let mut x = n as isize;
    let mut y = m as isize;
    for d in (1..=final_d).rev() {
        let prev_d = (d - 1) as isize;
        let prev_start = trace_starts[d - 1];
        let prev_v = |diag: isize| -> isize { trace_data[prev_start + (diag + prev_d) as usize] };

        let k = x - y;
        let prev_k = if k == -(d as isize) || (k != d as isize && prev_v(k - 1) < prev_v(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = prev_v(prev_k);
        let prev_y = prev_x - prev_k;

        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            matches.push((x as usize, y as usize));
        }
        x = prev_x;
        y = prev_y;
    }
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        matches.push((x as usize, y as usize));
    }
    matches.reverse();

    // Convert matching diagonal runs into non-matching diff hunks.
    let mut cur_a = 0usize;
    let mut cur_b = 0usize;
    for (ma, mb) in matches {
        if cur_a < ma || cur_b < mb {
            hunks.push(DiffHunk {
                base_start: prefix + cur_a,
                base_end: prefix + ma,
                target_start: prefix + cur_b,
                target_end: prefix + mb,
            });
        }
        cur_a = ma + 1;
        cur_b = mb + 1;
    }
    if cur_a < n || cur_b < m {
        hunks.push(DiffHunk {
            base_start: prefix + cur_a,
            base_end: prefix + n,
            target_start: prefix + cur_b,
            target_end: prefix + m,
        });
    }
    Some(hunks)
}

/// 3-way content merge over raw blob bytes (production checks).
pub fn merge_blobs<'a>(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    opts: MergeOpts<'_>,
    alloc: Alloc<'a>,
) -> BlobMerge<'a> {
    merge_blobs_planted(base, ours, theirs, opts, alloc, NoPlant)
}

/// 3-way content merge with optional planted-bug seam for DST oracle controls.
pub fn merge_blobs_planted<'a, P: Planted<GitEnginePlant>>(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    opts: MergeOpts<'_>,
    alloc: Alloc<'a>,
    plant: P,
) -> BlobMerge<'a> {
    if ours == theirs {
        let mut out = Vec::with_capacity_in(ours.len(), alloc);
        out.extend_from_slice(ours);
        return BlobMerge::Clean(out);
    }
    if base == ours {
        let mut out = Vec::with_capacity_in(theirs.len(), alloc);
        out.extend_from_slice(theirs);
        return BlobMerge::Clean(out);
    }
    if base == theirs {
        let mut out = Vec::with_capacity_in(ours.len(), alloc);
        out.extend_from_slice(ours);
        return BlobMerge::Clean(out);
    }
    if is_binary(base) || is_binary(ours) || is_binary(theirs) {
        return BlobMerge::BinaryConflict;
    }

    let base_lines = split_lines(base, alloc);
    let ours_lines = split_lines(ours, alloc);
    let theirs_lines = split_lines(theirs, alloc);

    if base_lines.len() > opts.limits.most_lines
        || ours_lines.len() > opts.limits.most_lines
        || theirs_lines.len() > opts.limits.most_lines
    {
        return emit_whole_conflict(base, ours, theirs, opts, alloc);
    }

    let Some(h_ours) = myers_diff_hunks(&base_lines, &ours_lines, opts.limits.most_diff_d, alloc)
    else {
        return emit_whole_conflict(base, ours, theirs, opts, alloc);
    };
    let Some(h_theirs) =
        myers_diff_hunks(&base_lines, &theirs_lines, opts.limits.most_diff_d, alloc)
    else {
        return emit_whole_conflict(base, ours, theirs, opts, alloc);
    };

    let mut out = Vec::with_capacity_in(ours.len().max(theirs.len()) + 64, alloc);
    let mut conflicted = false;
    let mut oi = 0usize;
    let mut ti = 0usize;
    let mut base_pos = 0usize;

    while oi < h_ours.len() || ti < h_theirs.len() {
        let next_o = h_ours.get(oi).map_or(usize::MAX, |h| h.base_start);
        let next_t = h_theirs.get(ti).map_or(usize::MAX, |h| h.base_start);
        let region_start = next_o.min(next_t);

        // Emit unchanged base lines before this region.
        while base_pos < region_start {
            out.extend_from_slice(base_lines[base_pos]);
            base_pos += 1;
        }

        // Check whether only one side acts cleanly before the other starts.
        let o_before_t = oi < h_ours.len()
            && (ti >= h_theirs.len()
                || (h_ours[oi].base_end < h_theirs[ti].base_start)
                || (h_ours[oi].base_end == h_theirs[ti].base_start
                    && h_ours[oi].base_start < h_ours[oi].base_end
                    && h_theirs[ti].base_start < h_theirs[ti].base_end));
        if o_before_t {
            let h = h_ours[oi];
            for line in &ours_lines[h.target_start..h.target_end] {
                out.extend_from_slice(line);
            }
            base_pos = h.base_end;
            oi += 1;
            continue;
        }

        let t_before_o = ti < h_theirs.len()
            && (oi >= h_ours.len()
                || (h_theirs[ti].base_end < h_ours[oi].base_start)
                || (h_theirs[ti].base_end == h_ours[oi].base_start
                    && h_theirs[ti].base_start < h_theirs[ti].base_end
                    && h_ours[oi].base_start < h_ours[oi].base_end));
        if t_before_o {
            let h = h_theirs[ti];
            for line in &theirs_lines[h.target_start..h.target_end] {
                out.extend_from_slice(line);
            }
            base_pos = h.base_end;
            ti += 1;
            continue;
        }

        // Overlapping / touching edits on both sides: coalesce maximal overlapping region!
        let mut region_end = region_start;
        let oi_start = oi;
        let ti_start = ti;
        loop {
            let mut grew = false;
            while oi < h_ours.len() && h_ours[oi].base_start <= region_end {
                if h_ours[oi].base_end > region_end {
                    region_end = h_ours[oi].base_end;
                }
                oi += 1;
                grew = true;
            }
            while ti < h_theirs.len() && h_theirs[ti].base_start <= region_end {
                if h_theirs[ti].base_end > region_end {
                    region_end = h_theirs[ti].base_end;
                }
                ti += 1;
                grew = true;
            }
            if !grew {
                break;
            }
        }

        // Materialize ours and theirs slices over [region_start..region_end].
        let mut ours_region = Vec::new_in(alloc);
        let mut cur = region_start;
        for h in &h_ours[oi_start..oi] {
            while cur < h.base_start {
                ours_region.extend_from_slice(base_lines[cur]);
                cur += 1;
            }
            for line in &ours_lines[h.target_start..h.target_end] {
                ours_region.extend_from_slice(line);
            }
            cur = h.base_end;
        }
        while cur < region_end {
            ours_region.extend_from_slice(base_lines[cur]);
            cur += 1;
        }

        let mut theirs_region = Vec::new_in(alloc);
        cur = region_start;
        for h in &h_theirs[ti_start..ti] {
            while cur < h.base_start {
                theirs_region.extend_from_slice(base_lines[cur]);
                cur += 1;
            }
            for line in &theirs_lines[h.target_start..h.target_end] {
                theirs_region.extend_from_slice(line);
            }
            cur = h.base_end;
        }
        while cur < region_end {
            theirs_region.extend_from_slice(base_lines[cur]);
            cur += 1;
        }

        if ours_region.as_slice() == theirs_region.as_slice()
            || plant.has(GitEnginePlant::SkipThreeWayConflict)
        {
            // Both sides made the exact same change!
            out.extend_from_slice(&ours_region);
        } else {
            conflicted = true;
            append_conflict_marker(b"<<<<<<< ", opts.ours_label, &mut out);
            out.extend_from_slice(&ours_region);
            ensure_newline(&mut out);
            if opts.conflict_style == ConflictStyle::Diff3 {
                append_conflict_marker(b"||||||| ", opts.base_label, &mut out);
                for line in &base_lines[region_start..region_end] {
                    out.extend_from_slice(line);
                }
                ensure_newline(&mut out);
            }
            out.extend_from_slice(b"=======\n");
            out.extend_from_slice(&theirs_region);
            ensure_newline(&mut out);
            append_conflict_marker(b">>>>>>> ", opts.theirs_label, &mut out);
        }
        base_pos = region_end;
    }

    while base_pos < base_lines.len() {
        out.extend_from_slice(base_lines[base_pos]);
        base_pos += 1;
    }

    if conflicted { BlobMerge::Conflicted(out) } else { BlobMerge::Clean(out) }
}

fn append_conflict_marker<'a>(prefix: &[u8], label: &str, out: &mut AVec<'a, u8>) {
    out.extend_from_slice(prefix);
    out.extend_from_slice(label.as_bytes());
    out.push(b'\n');
}

fn ensure_newline<'a>(out: &mut AVec<'a, u8>) {
    if !out.is_empty() && *out.last().expect("non-empty") != b'\n' {
        out.push(b'\n');
    }
}

fn emit_whole_conflict<'a>(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    opts: MergeOpts<'_>,
    alloc: Alloc<'a>,
) -> BlobMerge<'a> {
    let mut out = Vec::with_capacity_in(ours.len() + theirs.len() + 64, alloc);
    append_conflict_marker(b"<<<<<<< ", opts.ours_label, &mut out);
    out.extend_from_slice(ours);
    ensure_newline(&mut out);
    if opts.conflict_style == ConflictStyle::Diff3 {
        append_conflict_marker(b"||||||| ", opts.base_label, &mut out);
        out.extend_from_slice(base);
        ensure_newline(&mut out);
    }
    out.extend_from_slice(b"=======\n");
    out.extend_from_slice(theirs);
    ensure_newline(&mut out);
    append_conflict_marker(b">>>>>>> ", opts.theirs_label, &mut out);
    BlobMerge::Conflicted(out)
}

// ---------------------------------------------------------------------------
// O(changed-subtrees) ORT-style 3-way tree merge
// ---------------------------------------------------------------------------

struct OwnedTreeEntry<'a> {
    mode: u32,
    name: AVec<'a, u8>,
    oid: Oid,
}

impl OwnedTreeEntry<'_> {
    #[inline]
    fn is_dir(&self) -> bool {
        self.mode == 0o040000
    }

    #[inline]
    fn as_stage(&self) -> StageEntry {
        StageEntry { mode: self.mode, oid: self.oid }
    }
}

fn read_tree_entries<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    tree_oid: Option<Oid>,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, OwnedTreeEntry<'a>>, GitError> {
    let mut out = Vec::new_in(alloc);
    let Some(oid) = tree_oid else {
        return Ok(out);
    };
    let mut buf = Vec::new_in(alloc);
    let Some(kind) = store.read_object(&oid, &mut buf)? else {
        return Err(GitError {
            file: GitFile::TreeObject(oid),
            at: 0,
            kind: ErrorKind::ObjectMissing(oid),
        });
    };
    if kind != ObjectKind::Tree {
        return Err(GitError {
            file: GitFile::TreeObject(oid),
            at: 0,
            kind: ErrorKind::WrongObjectKind { want: ObjectKind::Tree, got: kind },
        });
    }
    let mut iter = TreeIter::new(oid, &buf);
    while let Some(e) = iter.next_entry()? {
        let mut name = Vec::with_capacity_in(e.name.len(), alloc);
        name.extend_from_slice(e.name);
        out.push(OwnedTreeEntry { mode: e.mode, name, oid: e.oid });
    }
    Ok(out)
}

fn empty_tree_oid<'a, S: ObjectStore<'a> + ?Sized>(store: &mut S) -> Result<Oid, GitError> {
    store.write_object(ObjectKind::Tree, b"")
}

fn join_path<'a>(prefix: &str, name: &[u8], alloc: Alloc<'a>) -> AString<'a> {
    let name_str = core::str::from_utf8(name).unwrap_or("?");
    let mut s = String::with_capacity_in(prefix.len() + name_str.len() + 1, alloc);
    if !prefix.is_empty() {
        s.push_str(prefix);
        s.push('/');
    }
    s.push_str(name_str);
    s
}

/// Performs an ORT-style 3-way merge of three tree OIDs (`base`, `ours`, `theirs`), writing all
/// newly created blobs and trees into `store`.
///
/// Any subtree where `ours == theirs`, `base == ours`, or `base == theirs` is skipped in O(1)
/// without reading its children.
pub fn merge_trees<'a, S: ObjectStore<'a> + ?Sized>(
    store: &mut S,
    base: Option<Oid>,
    ours: Option<Oid>,
    theirs: Option<Oid>,
    opts: MergeOpts<'_>,
    alloc: Alloc<'a>,
) -> Result<MergeResult<'a>, GitError> {
    merge_trees_planted(store, base, ours, theirs, opts, alloc, NoPlant)
}

/// Same as [`merge_trees`] with an optional planted-bug control seam.
pub fn merge_trees_planted<'a, S: ObjectStore<'a> + ?Sized, P: Planted<GitEnginePlant>>(
    store: &mut S,
    base: Option<Oid>,
    ours: Option<Oid>,
    theirs: Option<Oid>,
    opts: MergeOpts<'_>,
    alloc: Alloc<'a>,
    plant: P,
) -> Result<MergeResult<'a>, GitError> {
    let mut conflicts = Vec::new_in(alloc);
    let mut stats = MergeStats::default();

    let merged = merge_tree_rec(
        store,
        base,
        ours,
        theirs,
        "",
        0,
        opts,
        alloc,
        &mut conflicts,
        &mut stats,
        plant,
    )?;
    let tree = match merged {
        Some(oid) => oid,
        None => empty_tree_oid(store)?,
    };
    let status = if conflicts.is_empty() { MergeStatus::MergedClean } else { MergeStatus::Conflicted };
    Ok(MergeResult { status, tree, bases: Vec::new_in(alloc), conflicts, stats })
}

#[allow(clippy::too_many_arguments)]
fn merge_tree_rec<'a, S: ObjectStore<'a> + ?Sized, P: Planted<GitEnginePlant>>(
    store: &mut S,
    base: Option<Oid>,
    ours: Option<Oid>,
    theirs: Option<Oid>,
    prefix: &str,
    depth: u16,
    opts: MergeOpts<'_>,
    alloc: Alloc<'a>,
    conflicts: &mut AVec<'a, MergeConflict<'a>>,
    stats: &mut MergeStats,
    plant: P,
) -> Result<Option<Oid>, GitError> {
    // O(1) ORT subtree pruning: zero child object reads!
    if ours == theirs {
        stats.subtrees_skipped += 1;
        return Ok(ours);
    }
    if base == ours {
        stats.subtrees_skipped += 1;
        return Ok(theirs);
    }
    if base == theirs {
        stats.subtrees_skipped += 1;
        return Ok(ours);
    }
    if depth >= opts.limits.most_tree_depth {
        return Err(GitError {
            file: GitFile::Merge,
            at: u64::from(depth),
            kind: ErrorKind::DepthExceeded { most: opts.limits.most_tree_depth },
        });
    }

    stats.subtrees_merged += 1;
    let b_entries = read_tree_entries(store, base, alloc)?;
    let o_entries = read_tree_entries(store, ours, alloc)?;
    let t_entries = read_tree_entries(store, theirs, alloc)?;

    // Optional exact-OID rename map within this directory:
    // If ours deleted `(oid -> name)` and added `(oid -> new_name)` while theirs modified `name`,
    // we detect the exact move and resolve cleanly!
    let mut ours_renames: Map<Oid, usize, BuildKeyHasher, Alloc<'a>> =
        Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: None });
    let mut theirs_renames: Map<Oid, usize, BuildKeyHasher, Alloc<'a>> =
        Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: None });
    if opts.detect_exact_renames {
        index_exact_additions(&b_entries, &o_entries, &mut ours_renames, alloc);
        index_exact_additions(&b_entries, &t_entries, &mut theirs_renames, alloc);
    }
    let mut skip_ours_idx: Map<usize, (), FixedState, Alloc<'a>> =
        Map::new(MapOpts { hasher: FixedState, alloc, capacity: None });
    let mut skip_theirs_idx: Map<usize, (), FixedState, Alloc<'a>> =
        Map::new(MapOpts { hasher: FixedState, alloc, capacity: None });

    let mut bi = 0usize;
    let mut oi = 0usize;
    let mut ti = 0usize;
    let mut result_entries: AVec<'a, OwnedTreeEntry<'a>> = Vec::new_in(alloc);

    while bi < b_entries.len() || oi < o_entries.len() || ti < t_entries.len() {
        // Pick the smallest (name, is_dir) across the three sorted cursors.
        let mut min_name: Option<(&[u8], bool)> = None;
        for cand in [
            b_entries.get(bi).map(|e| (e.name.as_slice(), e.is_dir())),
            o_entries.get(oi).map(|e| (e.name.as_slice(), e.is_dir())),
            t_entries.get(ti).map(|e| (e.name.as_slice(), e.is_dir())),
        ]
        .into_iter()
        .flatten()
        {
            min_name = Some(match min_name {
                None => cand,
                Some(cur) => {
                    if git_tree_name_cmp(cand.0, cand.1, cur.0, cur.1) == Ordering::Less {
                        cand
                    } else {
                        cur
                    }
                }
            });
        }
        let (name_slice, _) = min_name.expect("at least one cursor active");

        // Match ALL entries whose bare name equals `name_slice` (handles D/F where one side has
        // dir `foo` and another has file `foo` — notice `git_tree_name_cmp` orders file `foo`
        // before dir `foo/` only if another name sits between them; check bare name equality!).
        let b_e = b_entries.get(bi).filter(|e| e.name.as_slice() == name_slice);
        let o_e = o_entries.get(oi).filter(|e| e.name.as_slice() == name_slice);
        let t_e = t_entries.get(ti).filter(|e| e.name.as_slice() == name_slice);
        let o_cur_idx = oi;
        let t_cur_idx = ti;
        if b_e.is_some() {
            bi += 1;
        }
        if o_e.is_some() {
            oi += 1;
        }
        if t_e.is_some() {
            ti += 1;
        }

        if o_e.is_some() && skip_ours_idx.contains_key(&o_cur_idx) && b_e.is_none() && t_e.is_none() {
            continue;
        }
        if t_e.is_some() && skip_theirs_idx.contains_key(&t_cur_idx) && b_e.is_none() && o_e.is_none()
        {
            continue;
        }

        // Fast equality checks on `(mode, oid)`.
        let b_s = b_e.map(OwnedTreeEntry::as_stage);
        let o_s = o_e.map(OwnedTreeEntry::as_stage);
        let t_s = t_e.map(OwnedTreeEntry::as_stage);

        if o_s == t_s {
            if let Some(o) = o_e {
                if o.is_dir() {
                    stats.subtrees_skipped += 1;
                }
                result_entries.push(clone_entry(o, alloc));
            }
            continue;
        }
        if b_s == o_s {
            if let Some(t) = t_e {
                if t.is_dir() {
                    stats.subtrees_skipped += 1;
                }
                result_entries.push(clone_entry(t, alloc));
            }
            continue;
        }
        if b_s == t_s {
            if let Some(o) = o_e {
                if o.is_dir() {
                    stats.subtrees_skipped += 1;
                }
                result_entries.push(clone_entry(o, alloc));
            }
            continue;
        }

        // Both sides diverged from base for `name_slice`!
        let path = join_path(prefix, name_slice, alloc);

        // Case 1: Both present and both are directories -> recurse!
        let any_dir = b_e.is_some_and(OwnedTreeEntry::is_dir)
            || o_e.is_some_and(OwnedTreeEntry::is_dir)
            || t_e.is_some_and(OwnedTreeEntry::is_dir);
        let any_non_dir = b_e.is_some_and(|e| !e.is_dir())
            || o_e.is_some_and(|e| !e.is_dir())
            || t_e.is_some_and(|e| !e.is_dir());

        if any_dir && !any_non_dir {
            let sub = merge_tree_rec(
                store,
                b_e.map(|e| e.oid),
                o_e.map(|e| e.oid),
                t_e.map(|e| e.oid),
                path.as_str(),
                depth + 1,
                opts,
                alloc,
                conflicts,
                stats,
                plant,
            )?;
            if let Some(sub_oid) = sub {
                let mut name_copy = Vec::with_capacity_in(name_slice.len(), alloc);
                name_copy.extend_from_slice(name_slice);
                result_entries.push(OwnedTreeEntry { mode: 0o040000, name: name_copy, oid: sub_oid });
            }
            continue;
        }

        // Case 2: Directory/File (D/F) conflict!
        if any_dir && any_non_dir {
            conflicts.push(MergeConflict {
                path,
                kind: ConflictKind::DirectoryFile,
                base: b_s,
                ours: o_s,
                theirs: t_s,
            });
            if let Some(keep) = o_e.or(t_e) {
                result_entries.push(clone_entry(keep, alloc));
            }
            continue;
        }

        // Case 3: Modify/Delete (including exact-OID rename resolution!).
        match (b_e, o_e, t_e) {
            (Some(b), None, Some(t)) => {
                // Ours deleted `b`, theirs modified `b -> t`. Did ours rename `b.oid` to a new name?
                if let Some(&dst_idx) = ours_renames.get(&b.oid) {
                    let dst = &o_entries[dst_idx];
                    skip_ours_idx.insert(dst_idx, ());
                    stats.renames_detected += 1;
                    let mut name_copy = Vec::with_capacity_in(dst.name.len(), alloc);
                    name_copy.extend_from_slice(&dst.name);
                    result_entries.push(OwnedTreeEntry { mode: t.mode, name: name_copy, oid: t.oid });
                } else {
                    conflicts.push(MergeConflict {
                        path,
                        kind: ConflictKind::ModifyDelete,
                        base: b_s,
                        ours: None,
                        theirs: t_s,
                    });
                    result_entries.push(clone_entry(t, alloc));
                }
            }
            (Some(b), Some(o), None) => {
                // Theirs deleted `b`, ours modified `b -> o`. Did theirs rename `b.oid`?
                if let Some(&dst_idx) = theirs_renames.get(&b.oid) {
                    let dst = &t_entries[dst_idx];
                    skip_theirs_idx.insert(dst_idx, ());
                    stats.renames_detected += 1;
                    let mut name_copy = Vec::with_capacity_in(dst.name.len(), alloc);
                    name_copy.extend_from_slice(&dst.name);
                    result_entries.push(OwnedTreeEntry { mode: o.mode, name: name_copy, oid: o.oid });
                } else {
                    conflicts.push(MergeConflict {
                        path,
                        kind: ConflictKind::ModifyDelete,
                        base: b_s,
                        ours: o_s,
                        theirs: None,
                    });
                    result_entries.push(clone_entry(o, alloc));
                }
            }
            (base_opt, Some(o), Some(t)) => {
                // Both sides present (either modify/modify or add/add).
                let mode = if o.mode == t.mode {
                    o.mode
                } else if base_opt.is_some_and(|b| b.mode == o.mode) {
                    t.mode
                } else if base_opt.is_some_and(|b| b.mode == t.mode) {
                    o.mode
                } else {
                    conflicts.push(MergeConflict {
                        path,
                        kind: ConflictKind::Mode,
                        base: b_s,
                        ours: o_s,
                        theirs: t_s,
                    });
                    result_entries.push(clone_entry(o, alloc));
                    continue;
                };

                // Only regular files (`0o100644`/`0o100755`) get line-level 3-way merge.
                if (mode & 0o170000) != 0o100000 {
                    conflicts.push(MergeConflict {
                        path,
                        kind: if base_opt.is_none() {
                            ConflictKind::AddAdd
                        } else {
                            ConflictKind::Content
                        },
                        base: b_s,
                        ours: o_s,
                        theirs: t_s,
                    });
                    result_entries.push(clone_entry(o, alloc));
                    continue;
                }

                let mut base_bytes = Vec::new_in(alloc);
                if let Some(b) = base_opt {
                    read_blob(store, b.oid, &mut base_bytes)?;
                }
                let mut ours_bytes = Vec::new_in(alloc);
                read_blob(store, o.oid, &mut ours_bytes)?;
                let mut theirs_bytes = Vec::new_in(alloc);
                read_blob(store, t.oid, &mut theirs_bytes)?;

                stats.blobs_merged += 1;
                match merge_blobs_planted(&base_bytes, &ours_bytes, &theirs_bytes, opts, alloc, plant)
                {
                    BlobMerge::Clean(merged_bytes) => {
                        let new_oid = store.write_object(ObjectKind::Blob, &merged_bytes)?;
                        let mut name_copy = Vec::with_capacity_in(name_slice.len(), alloc);
                        name_copy.extend_from_slice(name_slice);
                        result_entries.push(OwnedTreeEntry { mode, name: name_copy, oid: new_oid });
                    }
                    BlobMerge::Conflicted(marked_bytes) => {
                        let new_oid = store.write_object(ObjectKind::Blob, &marked_bytes)?;
                        conflicts.push(MergeConflict {
                            path,
                            kind: if base_opt.is_none() {
                                ConflictKind::AddAdd
                            } else {
                                ConflictKind::Content
                            },
                            base: b_s,
                            ours: o_s,
                            theirs: t_s,
                        });
                        let mut name_copy = Vec::with_capacity_in(name_slice.len(), alloc);
                        name_copy.extend_from_slice(name_slice);
                        result_entries.push(OwnedTreeEntry { mode, name: name_copy, oid: new_oid });
                    }
                    BlobMerge::BinaryConflict => {
                        conflicts.push(MergeConflict {
                            path,
                            kind: ConflictKind::Binary,
                            base: b_s,
                            ours: o_s,
                            theirs: t_s,
                        });
                        result_entries.push(clone_entry(o, alloc));
                    }
                }
            }
            _ => {}
        }
    }

    if result_entries.is_empty() && depth > 0 {
        return Ok(None);
    }

    let mut build: AVec<'a, TreeBuildEntry<'_>> = Vec::with_capacity_in(result_entries.len(), alloc);
    for e in &result_entries {
        build.push(TreeBuildEntry { mode: e.mode, name: e.name.as_slice(), oid: e.oid });
    }
    let mut tree_bytes = Vec::new_in(alloc);
    encode_tree(&build, alloc, &mut tree_bytes)?;
    let tree_oid = store.write_object(ObjectKind::Tree, &tree_bytes)?;
    Ok(Some(tree_oid))
}

fn clone_entry<'a>(e: &OwnedTreeEntry<'a>, alloc: Alloc<'a>) -> OwnedTreeEntry<'a> {
    let mut name = Vec::with_capacity_in(e.name.len(), alloc);
    name.extend_from_slice(&e.name);
    OwnedTreeEntry { mode: e.mode, name, oid: e.oid }
}

fn index_exact_additions<'a>(
    base: &[OwnedTreeEntry<'a>],
    side: &[OwnedTreeEntry<'a>],
    out: &mut Map<Oid, usize, BuildKeyHasher, Alloc<'a>>,
    alloc: Alloc<'a>,
) {
    let mut base_names: Map<&[u8], (), FixedState, Alloc<'a>> =
        Map::new(MapOpts { hasher: FixedState, alloc, capacity: Some(base.len()) });
    for b in base {
        base_names.insert(b.name.as_slice(), ());
    }
    for (idx, s) in side.iter().enumerate() {
        if !s.is_dir() && !base_names.contains_key(s.name.as_slice()) {
            out.insert(s.oid, idx);
        }
    }
}

fn read_blob<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    oid: Oid,
    out: &mut AVec<'a, u8>,
) -> Result<(), GitError> {
    let Some(kind) = store.read_object(&oid, out)? else {
        return Err(GitError {
            file: GitFile::LooseObject(oid),
            at: 0,
            kind: ErrorKind::ObjectMissing(oid),
        });
    };
    if kind != ObjectKind::Blob {
        return Err(GitError {
            file: GitFile::LooseObject(oid),
            at: 0,
            kind: ErrorKind::WrongObjectKind { want: ObjectKind::Blob, got: kind },
        });
    }
    Ok(())
}

fn commit_tree<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    graph: Option<&CommitGraph<'_, 'a>>,
    commit_oid: Oid,
    alloc: Alloc<'a>,
) -> Result<Oid, GitError> {
    if let Some(g) = graph
        && let Some(c) = g.lookup(&commit_oid)
    {
        return Ok(c.tree);
    }
    let mut buf = Vec::new_in(alloc);
    let Some(kind) = store.read_object(&commit_oid, &mut buf)? else {
        return Err(GitError {
            file: GitFile::CommitObject(commit_oid),
            at: 0,
            kind: ErrorKind::ObjectMissing(commit_oid),
        });
    };
    if kind != ObjectKind::Commit {
        return Err(GitError {
            file: GitFile::CommitObject(commit_oid),
            at: 0,
            kind: ErrorKind::WrongObjectKind { want: ObjectKind::Commit, got: kind },
        });
    }
    let parsed = Commit::parse(&buf).map_err(|(at, why)| GitError {
        file: GitFile::CommitObject(commit_oid),
        at,
        kind: ErrorKind::CommitParse(why),
    })?;
    Ok(parsed.tree)
}

/// Full ORT-style 3-way commit merge (`ours` + `theirs`), handling fast-forward, already-up-to-date,
/// disjoint histories, single LCA, and criss-cross multi-LCA virtual merge bases!
pub fn merge_commits<'a, S: ObjectStore<'a> + ?Sized>(
    store: &mut S,
    graph: Option<&CommitGraph<'_, 'a>>,
    ours: Oid,
    theirs: Oid,
    opts: MergeOpts<'_>,
    alloc: Alloc<'a>,
) -> Result<MergeResult<'a>, GitError> {
    merge_commits_planted(store, graph, ours, theirs, opts, alloc, NoPlant)
}

pub fn merge_commits_planted<'a, S: ObjectStore<'a> + ?Sized, P: Planted<GitEnginePlant>>(
    store: &mut S,
    graph: Option<&CommitGraph<'_, 'a>>,
    ours: Oid,
    theirs: Oid,
    opts: MergeOpts<'_>,
    alloc: Alloc<'a>,
    plant: P,
) -> Result<MergeResult<'a>, GitError> {
    let bases = find_merge_bases(store, graph, ours, theirs, opts.limits, alloc)?;
    let ours_tree = commit_tree(store, graph, ours, alloc)?;
    let theirs_tree = commit_tree(store, graph, theirs, alloc)?;

    if ours == theirs || bases.as_slice() == [theirs] {
        return Ok(MergeResult {
            status: MergeStatus::AlreadyUpToDate,
            tree: ours_tree,
            bases,
            conflicts: Vec::new_in(alloc),
            stats: MergeStats::default(),
        });
    }
    if bases.as_slice() == [ours] {
        return Ok(MergeResult {
            status: MergeStatus::FastForward,
            tree: theirs_tree,
            bases,
            conflicts: Vec::new_in(alloc),
            stats: MergeStats::default(),
        });
    }

    let base_tree = if bases.is_empty() {
        None
    } else if bases.len() == 1 {
        Some(commit_tree(store, graph, bases[0], alloc)?)
    } else {
        // Criss-cross merge: fold multiple LCAs into a virtual merge base tree.
        let mut virtual_tree = commit_tree(store, graph, bases[0], alloc)?;
        let cap = bases.len().min(usize::from(opts.limits.most_virtual_depth) + 1);
        for &next_base in &bases[1..cap] {
            let next_tree = commit_tree(store, graph, next_base, alloc)?;
            let sub = merge_trees_planted(
                store,
                None,
                Some(virtual_tree),
                Some(next_tree),
                opts,
                alloc,
                plant,
            )?;
            virtual_tree = sub.tree;
        }
        Some(virtual_tree)
    };

    let mut res = merge_trees_planted(
        store,
        base_tree,
        Some(ours_tree),
        Some(theirs_tree),
        opts,
        alloc,
        plant,
    )?;
    res.bases = bases;
    Ok(res)
}
