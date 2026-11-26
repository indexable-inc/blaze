//! Sans-IO worktree manager, `BGWT` v1 manifest codec, automatic tracked-branch merge evaluator,
//! and filesystem-aware 3-way worktree update planner.
//!
//! Pure `#![no_std]`, `#![blaze::sans_io]`, `#![forbid(unsafe_code)]`, explicit [`Alloc`].
//!
//! - **Tracked-branch auto-merge**: [`evaluate_worktree_sync`] checks each worktree's branch
//!   against its `track_branch` (using O(log N) `CGPH` commit-graph ancestry), fast-forwarding
//!   uncommitted branches or running an ORT 3-way [`merge_commits`] for diverged branches.
//! - **Filesystem-aware 3-way projection**: [`plan_worktree_fs_update`] walks ONLY subtrees that
//!   changed between `base_tree` and `target_tree` (skipping unchanged subtrees in O(1)), reads
//!   the live worktree file ONLY for changed paths, preserves clean/dirty untouched files, and
//!   runs a 3-way line-level [`merge_blobs`] when both the local filesystem and the tracked branch
//!   edited the same file.
//! - **Auto-delete on merge**: when a worktree branch's commits have been merged into another
//!   branch (either detected via `is_ancestor` with `branch_oid != base_commit`, or explicitly
//!   after `bgit merge <ours> <theirs>`), emits [`WorktreeSyncDecision::MergedIntoTracked`] so
//!   the driver can safely remove the worktree folder by default (unless dirty or opted out).

#![blaze::sans_io]

use core::cmp::Ordering;

use blaze_git_fetch::{Kind as ObjectKind, Oid, store::Commit};
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::{
    ErrorKind, GitError, GitFile,
    commit_graph::CommitGraph,
    merge::{
        BlobMerge, MergeConflict, MergeLimits, MergeOpts, MergeStatus, find_merge_bases,
        merge_blobs, merge_commits,
    },
    objects::{TreeEntry, TreeIter},
    storage::{ObjectStore, RefStore, git_tree_name_cmp},
};

const BGWT_MAGIC: &[u8; 4] = b"BGWT";
const BGWT_VERSION: u32 = 1;

const FLAG_IS_VFS: u8 = 1 << 0;
const FLAG_AUTO_DELETE_ON_MERGE: u8 = 1 << 1;

/// One registered worktree stored in `.bgit/worktrees.bgwt`.
#[derive(PartialEq, Eq, Debug)]
pub struct WorktreeRecord<'a> {
    /// Short identifier (usually directory basename).
    pub name: AString<'a>,
    /// Canonical filesystem path of the worktree folder.
    pub path: AString<'a>,
    /// Full ref name checked out in this worktree (e.g. `refs/heads/feat/foo`).
    pub branch: AString<'a>,
    /// Full ref name tracked for automatic updates & merge detection (e.g. `refs/heads/main`).
    pub track_branch: AString<'a>,
    /// Commit OID from which this branch started (advanced on fast-forward when untouched).
    /// Distinguishes "brand-new branch with 0 commits behind main" from "branch with commits
    /// that were merged into main".
    pub base_commit: Oid,
    /// Last commit OID synced/projected onto this worktree folder.
    pub head_commit: Oid,
    /// Last tree OID synced/projected onto this worktree folder (base for FS 3-way merge!).
    pub head_tree: Oid,
    /// Whether this worktree is mounted lazily via `bgit mount` (NFSv3 VFS).
    pub is_vfs: bool,
    /// Automatically delete this worktree folder when its branch is merged into another branch.
    pub auto_delete_on_merge: bool,
}

/// Encode `records` into `BGWT` v1 binary format protected by a trailing CRC-32 checksum.
pub fn encode_worktrees(records: &[WorktreeRecord<'_>], out: &mut AVec<'_, u8>) {
    out.clear();
    out.extend_from_slice(BGWT_MAGIC);
    out.extend_from_slice(&BGWT_VERSION.to_le_bytes());
    out.extend_from_slice(&(records.len() as u32).to_le_bytes());

    for r in records {
        put_str(out, r.name.as_str());
        put_str(out, r.path.as_str());
        put_str(out, r.branch.as_str());
        put_str(out, r.track_branch.as_str());
        out.extend_from_slice(&r.base_commit.0);
        out.extend_from_slice(&r.head_commit.0);
        out.extend_from_slice(&r.head_tree.0);
        let mut flags = 0u8;
        if r.is_vfs {
            flags |= FLAG_IS_VFS;
        }
        if r.auto_delete_on_merge {
            flags |= FLAG_AUTO_DELETE_ON_MERGE;
        }
        out.push(flags);
    }

    let crc = blaze_deflate::crc32(out);
    out.extend_from_slice(&crc.to_le_bytes());
}

/// Decode and CRC-32 verify a `BGWT` v1 worktree registry manifest (`verify-once`).
pub fn decode_worktrees<'a>(
    bytes: &[u8],
    most_records: usize,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, WorktreeRecord<'a>>, GitError> {
    let mal = |at: usize| GitError {
        file: GitFile::Worktree,
        at: at as u64,
        kind: ErrorKind::Malformed,
    };
    if bytes.len() < 16 {
        return Err(mal(0));
    }
    if &bytes[0..4] != BGWT_MAGIC {
        return Err(mal(0));
    }
    let ver = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if ver != BGWT_VERSION {
        return Err(GitError {
            file: GitFile::Worktree,
            at: 4,
            kind: ErrorKind::UnsupportedVersion(ver),
        });
    }
    let body_end = bytes.len() - 4;
    let stored_crc = u32::from_le_bytes(bytes[body_end..].try_into().unwrap());
    if blaze_deflate::crc32(&bytes[..body_end]) != stored_crc {
        return Err(GitError {
            file: GitFile::Worktree,
            at: body_end as u64,
            kind: ErrorKind::ChecksumMismatch,
        });
    }

    let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    if count > most_records {
        return Err(GitError {
            file: GitFile::Worktree,
            at: 8,
            kind: ErrorKind::TooManyEntries { count: count as u32, most: most_records as u32 },
        });
    }

    let mut at = 12usize;
    let mut out = Vec::with_capacity_in(count, alloc);
    for _ in 0..count {
        let name = read_str(bytes, &mut at, body_end, alloc)?;
        let path = read_str(bytes, &mut at, body_end, alloc)?;
        let branch = read_str(bytes, &mut at, body_end, alloc)?;
        let track_branch = read_str(bytes, &mut at, body_end, alloc)?;
        if at + 61 > body_end {
            return Err(mal(at));
        }
        let mut base_raw = [0u8; 20];
        let mut head_raw = [0u8; 20];
        let mut tree_raw = [0u8; 20];
        base_raw.copy_from_slice(&bytes[at..at + 20]);
        head_raw.copy_from_slice(&bytes[at + 20..at + 40]);
        tree_raw.copy_from_slice(&bytes[at + 40..at + 60]);
        let flags = bytes[at + 60];
        at += 61;

        out.push(WorktreeRecord {
            name,
            path,
            branch,
            track_branch,
            base_commit: Oid(base_raw),
            head_commit: Oid(head_raw),
            head_tree: Oid(tree_raw),
            is_vfs: (flags & FLAG_IS_VFS) != 0,
            auto_delete_on_merge: (flags & FLAG_AUTO_DELETE_ON_MERGE) != 0,
        });
    }
    Ok(out)
}

fn put_str(out: &mut AVec<'_, u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn read_str<'a>(
    bytes: &[u8],
    at: &mut usize,
    end: usize,
    alloc: Alloc<'a>,
) -> Result<AString<'a>, GitError> {
    if *at + 4 > end {
        return Err(GitError {
            file: GitFile::Worktree,
            at: *at as u64,
            kind: ErrorKind::Malformed,
        });
    }
    let len = u32::from_le_bytes(bytes[*at..*at + 4].try_into().unwrap()) as usize;
    *at += 4;
    if *at + len > end {
        return Err(GitError {
            file: GitFile::Worktree,
            at: *at as u64,
            kind: ErrorKind::Malformed,
        });
    }
    let s = core::str::from_utf8(&bytes[*at..*at + len]).map_err(|_| GitError {
        file: GitFile::Worktree,
        at: *at as u64,
        kind: ErrorKind::Malformed,
    })?;
    *at += len;
    Ok(String::from_str_in(s, alloc))
}

// ---------------------------------------------------------------------------
// Ancestry & Tracked-Branch Auto-Merge / Auto-Delete Decision Engine
// ---------------------------------------------------------------------------

/// Returns `true` iff `ancestor` is reachable from `descendant` in the commit DAG.
pub fn is_ancestor<'a>(
    store: &impl ObjectStore<'a>,
    graph: Option<&CommitGraph<'_, 'a>>,
    ancestor: Oid,
    descendant: Oid,
    limits: MergeLimits,
    alloc: Alloc<'a>,
) -> Result<bool, GitError> {
    if ancestor == descendant {
        return Ok(true);
    }
    let bases = find_merge_bases(store, graph, ancestor, descendant, limits, alloc)?;
    Ok(bases.len() == 1 && bases[0] == ancestor)
}

/// Decision produced by [`evaluate_worktree_sync`] for one worktree.
#[derive(Debug)]
pub enum WorktreeSyncDecision<'a> {
    /// Worktree branch already contains the latest tracked branch commit.
    UpToDate,
    /// Worktree's own branch ref no longer exists.
    MissingBranch,
    /// Tracked upstream branch ref does not exist.
    MissingTrackedBranch,
    /// Worktree's branch had commits that are now merged into `track_branch`: delete the folder!
    MergedIntoTracked {
        branch_oid: Oid,
        track_oid: Oid,
    },
    /// Worktree's branch can cleanly fast-forward to `track_oid` (no local commits yet, or ff).
    FastForward {
        old_commit: Oid,
        new_commit: Oid,
        old_tree: Oid,
        new_tree: Oid,
    },
    /// Worktree's branch and `track_branch` diverged: ORT 3-way merge result ready to project!
    AutoMerge {
        old_commit: Oid,
        track_oid: Oid,
        old_tree: Oid,
        merged_tree: Oid,
        status: MergeStatus,
        conflicts: AVec<'a, MergeConflict<'a>>,
    },
}

/// Read the root tree OID of commit `commit_oid`.
pub fn commit_tree_oid<'a>(
    store: &impl ObjectStore<'a>,
    commit_oid: Oid,
    alloc: Alloc<'a>,
) -> Result<Oid, GitError> {
    let mut raw = Vec::new_in(alloc);
    let Some(kind) = store.read_object(&commit_oid, &mut raw)? else {
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
    let commit = Commit::parse(&raw).map_err(|(at, why)| GitError {
        file: GitFile::CommitObject(commit_oid),
        at,
        kind: ErrorKind::CommitParse(why),
    })?;
    Ok(commit.tree)
}

/// Evaluate whether `record` should be fast-forwarded, 3-way auto-merged from `track_branch`,
/// or auto-deleted because its commits were merged into `track_branch`.
pub fn evaluate_worktree_sync<'a, S>(
    record: &WorktreeRecord<'_>,
    store: &mut S,
    graph: Option<&CommitGraph<'_, 'a>>,
    opts: MergeOpts<'_>,
    alloc: Alloc<'a>,
) -> Result<WorktreeSyncDecision<'a>, GitError>
where
    S: ObjectStore<'a> + RefStore<'a>,
{
    let Some(branch_oid) = store.resolve_ref(record.branch.as_str())? else {
        return Ok(WorktreeSyncDecision::MissingBranch);
    };
    if record.track_branch.is_empty() {
        return Ok(WorktreeSyncDecision::UpToDate);
    }
    let Some(track_oid) = store.resolve_ref(record.track_branch.as_str())? else {
        return Ok(WorktreeSyncDecision::MissingTrackedBranch);
    };

    // Same commit: either untouched (`branch_oid == base_commit`) or fast-forward merged into
    // `track_branch` (`branch_oid != base_commit`, meaning this branch made commits that `track_branch`
    // has now fast-forwarded onto!).
    if branch_oid == track_oid {
        if record.auto_delete_on_merge
            && record.branch.as_str() != record.track_branch.as_str()
            && branch_oid != record.base_commit
        {
            return Ok(WorktreeSyncDecision::MergedIntoTracked { branch_oid, track_oid });
        }
        if record.head_commit != branch_oid {
            let new_tree = commit_tree_oid(store, branch_oid, alloc)?;
            return Ok(WorktreeSyncDecision::FastForward {
                old_commit: record.head_commit,
                new_commit: branch_oid,
                old_tree: record.head_tree,
                new_tree,
            });
        }
        return Ok(WorktreeSyncDecision::UpToDate);
    }

    // Check if `branch_oid` is an ancestor of `track_oid`.
    if is_ancestor(store, graph, branch_oid, track_oid, opts.limits, alloc)? {
        // Distinguish:
        // - `branch_oid != record.base_commit`: this branch MADE commits and those commits are now
        //   merged into `track_branch` -> auto-delete worktree (when `auto_delete_on_merge`)!
        // - `branch_oid == record.base_commit`: this branch has no local commits yet, and
        //   `track_branch` simply moved forward -> fast-forward it!
        if record.auto_delete_on_merge
            && record.branch.as_str() != record.track_branch.as_str()
            && branch_oid != record.base_commit
        {
            return Ok(WorktreeSyncDecision::MergedIntoTracked { branch_oid, track_oid });
        }
        let new_tree = commit_tree_oid(store, track_oid, alloc)?;
        return Ok(WorktreeSyncDecision::FastForward {
            old_commit: record.head_commit,
            new_commit: track_oid,
            old_tree: record.head_tree,
            new_tree,
        });
    }

    // Check if `track_oid` is already an ancestor of `branch_oid` (branch is strictly ahead).
    if is_ancestor(store, graph, track_oid, branch_oid, opts.limits, alloc)? {
        if record.head_commit != branch_oid {
            let new_tree = commit_tree_oid(store, branch_oid, alloc)?;
            return Ok(WorktreeSyncDecision::FastForward {
                old_commit: record.head_commit,
                new_commit: branch_oid,
                old_tree: record.head_tree,
                new_tree,
            });
        }
        return Ok(WorktreeSyncDecision::UpToDate);
    }

    // Diverged! Run sans-IO ORT 3-way merge of `branch_oid` (ours) with `track_oid` (theirs).
    let merge_res = merge_commits(store, graph, branch_oid, track_oid, opts, alloc)?;
    Ok(WorktreeSyncDecision::AutoMerge {
        old_commit: branch_oid,
        track_oid,
        old_tree: record.head_tree,
        merged_tree: merge_res.tree,
        status: merge_res.status,
        conflicts: merge_res.conflicts,
    })
}

// ---------------------------------------------------------------------------
// Filesystem-Aware 3-Way Worktree Projection (`plan_worktree_fs_update`)
// ---------------------------------------------------------------------------

/// One filesystem mutation emitted by [`plan_worktree_fs_update`].
#[derive(PartialEq, Eq, Debug)]
pub enum FsAction<'a> {
    WriteFile { path: AString<'a>, mode: u32, bytes: AVec<'a, u8> },
    WriteSymlink { path: AString<'a>, target: AVec<'a, u8> },
    DeleteFile { path: AString<'a> },
}

/// Reason a filesystem-aware 3-way update could not apply cleanly without clobbering local work.
#[derive(PartialEq, Eq, Debug)]
pub enum FsConflictKind {
    /// Upstream added a file where an untracked local file with different content already exists.
    UntrackedOverwrite,
    /// Upstream deleted a file that was modified locally on disk.
    ModifyDelete,
    /// Upstream modified a file that was deleted locally on disk.
    DeleteModify,
    /// Both the local filesystem and upstream modified overlapping lines (or binary content).
    ContentConflict,
}

/// One filesystem-level merge conflict reported by [`plan_worktree_fs_update`].
#[derive(PartialEq, Eq, Debug)]
pub struct FsConflict<'a> {
    pub path: AString<'a>,
    pub kind: FsConflictKind,
    /// For `ContentConflict`, the 3-way merged bytes with Git conflict markers.
    pub merged_with_markers: Option<AVec<'a, u8>>,
}

/// Live file snapshot returned by the driver's `read_disk` callback for one changed path.
#[derive(PartialEq, Eq, Debug)]
pub struct DiskFile<'a> {
    pub mode: u32,
    pub bytes: AVec<'a, u8>,
}

/// Result of [`plan_worktree_fs_update`].
#[derive(Debug)]
pub struct WorktreeFsPlan<'a> {
    pub actions: AVec<'a, FsAction<'a>>,
    pub conflicts: AVec<'a, FsConflict<'a>>,
    /// Number of dirty files on disk cleanly 3-way line-merged with upstream changes.
    pub auto_merged_files: u32,
    /// Number of unchanged subtrees skipped in O(1) without reading a single tree or disk file!
    pub subtrees_skipped: u32,
}

impl WorktreeFsPlan<'_> {
    #[inline]
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// Plan the minimal filesystem updates to move a live worktree from `base_tree` to `target_tree`,
/// preserving untouched dirty files and 3-way line-merging files edited both locally and upstream!
///
/// Only subtrees where `base_tree != target_tree` are descended; `read_disk(path)` is invoked
/// ONLY for files that actually changed between `base_tree` and `target_tree`.
pub fn plan_worktree_fs_update<'a, F>(
    store: &impl ObjectStore<'a>,
    base_tree: Oid,
    target_tree: Oid,
    mut read_disk: F,
    opts: MergeOpts<'_>,
    alloc: Alloc<'a>,
) -> Result<WorktreeFsPlan<'a>, GitError>
where
    F: FnMut(&str) -> Option<DiskFile<'a>>,
{
    let mut plan = WorktreeFsPlan {
        actions: Vec::new_in(alloc),
        conflicts: Vec::new_in(alloc),
        auto_merged_files: 0,
        subtrees_skipped: 0,
    };
    if base_tree == target_tree {
        plan.subtrees_skipped = 1;
        return Ok(plan);
    }
    diff_trees_for_fs(
        store,
        Some(base_tree),
        Some(target_tree),
        "",
        0,
        &mut read_disk,
        opts,
        alloc,
        &mut plan,
    )?;
    Ok(plan)
}

#[allow(clippy::too_many_arguments)]
fn diff_trees_for_fs<'a, F>(
    store: &impl ObjectStore<'a>,
    base_tree: Option<Oid>,
    target_tree: Option<Oid>,
    prefix: &str,
    depth: u16,
    read_disk: &mut F,
    opts: MergeOpts<'_>,
    alloc: Alloc<'a>,
    plan: &mut WorktreeFsPlan<'a>,
) -> Result<(), GitError>
where
    F: FnMut(&str) -> Option<DiskFile<'a>>,
{
    if base_tree == target_tree {
        plan.subtrees_skipped = plan.subtrees_skipped.saturating_add(1);
        return Ok(());
    }
    if depth > opts.limits.most_tree_depth {
        return Err(GitError {
            file: GitFile::Worktree,
            at: 0,
            kind: ErrorKind::DepthExceeded { most: opts.limits.most_tree_depth },
        });
    }

    let mut base_raw = Vec::new_in(alloc);
    let mut target_raw = Vec::new_in(alloc);
    if let Some(bt) = base_tree {
        read_tree_raw(store, bt, &mut base_raw)?;
    }
    if let Some(tt) = target_tree {
        read_tree_raw(store, tt, &mut target_raw)?;
    }

    let mut bi = base_tree.map(|oid| TreeIter::new(oid, &base_raw));
    let mut ti = target_tree.map(|oid| TreeIter::new(oid, &target_raw));
    let mut next_b = match &mut bi {
        Some(it) => it.next_entry()?,
        None => None,
    };
    let mut next_t = match &mut ti {
        Some(it) => it.next_entry()?,
        None => None,
    };

    loop {
        match (next_b, next_t) {
            (None, None) => break,
            (Some(b), None) => {
                handle_base_only(store, b, prefix, depth, read_disk, opts, alloc, plan)?;
                next_b = bi.as_mut().unwrap().next_entry()?;
            }
            (None, Some(t)) => {
                handle_target_only(store, t, prefix, depth, read_disk, opts, alloc, plan)?;
                next_t = ti.as_mut().unwrap().next_entry()?;
            }
            (Some(b), Some(t)) => {
                match git_tree_name_cmp(b.name, b.is_dir(), t.name, t.is_dir()) {
                    Ordering::Less => {
                        handle_base_only(store, b, prefix, depth, read_disk, opts, alloc, plan)?;
                        next_b = bi.as_mut().unwrap().next_entry()?;
                    }
                    Ordering::Greater => {
                        handle_target_only(store, t, prefix, depth, read_disk, opts, alloc, plan)?;
                        next_t = ti.as_mut().unwrap().next_entry()?;
                    }
                    Ordering::Equal => {
                        if b.oid == t.oid && b.mode == t.mode {
                            if b.is_dir() {
                                plan.subtrees_skipped = plan.subtrees_skipped.saturating_add(1);
                            }
                        } else if b.is_dir() && t.is_dir() {
                            let sub = join_rel(prefix, b.name, alloc)?;
                            diff_trees_for_fs(
                                store,
                                Some(b.oid),
                                Some(t.oid),
                                sub.as_str(),
                                depth + 1,
                                read_disk,
                                opts,
                                alloc,
                                plan,
                            )?;
                        } else if b.is_dir() && !t.is_dir() {
                            handle_base_only(store, b, prefix, depth, read_disk, opts, alloc, plan)?;
                            handle_target_only(store, t, prefix, depth, read_disk, opts, alloc, plan)?;
                        } else if !b.is_dir() && t.is_dir() {
                            handle_base_only(store, b, prefix, depth, read_disk, opts, alloc, plan)?;
                            handle_target_only(store, t, prefix, depth, read_disk, opts, alloc, plan)?;
                        } else {
                            let path = join_rel(prefix, b.name, alloc)?;
                            reconcile_modified_file(
                                store, path, b, t, read_disk, opts, alloc, plan,
                            )?;
                        }
                        next_b = bi.as_mut().unwrap().next_entry()?;
                        next_t = ti.as_mut().unwrap().next_entry()?;
                    }
                }
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_base_only<'a, F>(
    store: &impl ObjectStore<'a>,
    b: TreeEntry<'_>,
    prefix: &str,
    depth: u16,
    read_disk: &mut F,
    opts: MergeOpts<'_>,
    alloc: Alloc<'a>,
    plan: &mut WorktreeFsPlan<'a>,
) -> Result<(), GitError>
where
    F: FnMut(&str) -> Option<DiskFile<'a>>,
{
    let path = join_rel(prefix, b.name, alloc)?;
    if b.is_dir() {
        return diff_trees_for_fs(
            store,
            Some(b.oid),
            None,
            path.as_str(),
            depth + 1,
            read_disk,
            opts,
            alloc,
            plan,
        );
    }
    let Some(disk) = read_disk(path.as_str()) else {
        // Already deleted locally.
        return Ok(());
    };
    if Oid::of(ObjectKind::Blob, &disk.bytes) == b.oid {
        plan.actions.push(FsAction::DeleteFile { path });
    } else {
        plan.conflicts.push(FsConflict {
            path,
            kind: FsConflictKind::ModifyDelete,
            merged_with_markers: None,
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_target_only<'a, F>(
    store: &impl ObjectStore<'a>,
    t: TreeEntry<'_>,
    prefix: &str,
    depth: u16,
    read_disk: &mut F,
    opts: MergeOpts<'_>,
    alloc: Alloc<'a>,
    plan: &mut WorktreeFsPlan<'a>,
) -> Result<(), GitError>
where
    F: FnMut(&str) -> Option<DiskFile<'a>>,
{
    let path = join_rel(prefix, t.name, alloc)?;
    if t.is_dir() {
        return diff_trees_for_fs(
            store,
            None,
            Some(t.oid),
            path.as_str(),
            depth + 1,
            read_disk,
            opts,
            alloc,
            plan,
        );
    }
    let mut t_bytes = Vec::new_in(alloc);
    read_blob_raw(store, t.oid, &mut t_bytes)?;
    if let Some(disk) = read_disk(path.as_str()) {
        if disk.bytes.as_slice() == t_bytes.as_slice() {
            return Ok(());
        }
        plan.conflicts.push(FsConflict {
            path,
            kind: FsConflictKind::UntrackedOverwrite,
            merged_with_markers: None,
        });
        return Ok(());
    }
    if (t.mode & 0o170000) == 0o120000 {
        plan.actions.push(FsAction::WriteSymlink { path, target: t_bytes });
    } else {
        plan.actions.push(FsAction::WriteFile { path, mode: t.mode, bytes: t_bytes });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn reconcile_modified_file<'a, F>(
    store: &impl ObjectStore<'a>,
    path: AString<'a>,
    b: TreeEntry<'_>,
    t: TreeEntry<'_>,
    read_disk: &mut F,
    opts: MergeOpts<'_>,
    alloc: Alloc<'a>,
    plan: &mut WorktreeFsPlan<'a>,
) -> Result<(), GitError>
where
    F: FnMut(&str) -> Option<DiskFile<'a>>,
{
    let mut t_bytes = Vec::new_in(alloc);
    read_blob_raw(store, t.oid, &mut t_bytes)?;

    let Some(disk) = read_disk(path.as_str()) else {
        plan.conflicts.push(FsConflict {
            path,
            kind: FsConflictKind::DeleteModify,
            merged_with_markers: None,
        });
        return Ok(());
    };

    let disk_oid = Oid::of(ObjectKind::Blob, &disk.bytes);
    // Already matches target bytes & mode -> no-op.
    if disk_oid == t.oid && normalize_mode(disk.mode) == normalize_mode(t.mode) {
        return Ok(());
    }
    // Untouched on disk relative to `base_tree` -> fast-path direct write!
    if disk_oid == b.oid {
        if (t.mode & 0o170000) == 0o120000 {
            plan.actions.push(FsAction::WriteSymlink { path, target: t_bytes });
        } else {
            plan.actions.push(FsAction::WriteFile { path, mode: t.mode, bytes: t_bytes });
        }
        return Ok(());
    }

    // Both local disk and upstream modified this file! Run 3-way line-level merge!
    let mut b_bytes = Vec::new_in(alloc);
    read_blob_raw(store, b.oid, &mut b_bytes)?;
    match merge_blobs(&b_bytes, &disk.bytes, &t_bytes, opts, alloc) {
        BlobMerge::Clean(merged) => {
            plan.auto_merged_files = plan.auto_merged_files.saturating_add(1);
            let mode = if normalize_mode(disk.mode) != normalize_mode(b.mode) {
                disk.mode
            } else {
                t.mode
            };
            plan.actions.push(FsAction::WriteFile { path, mode, bytes: merged });
        }
        BlobMerge::Conflicted(marked) => {
            plan.conflicts.push(FsConflict {
                path,
                kind: FsConflictKind::ContentConflict,
                merged_with_markers: Some(marked),
            });
        }
        BlobMerge::BinaryConflict => {
            plan.conflicts.push(FsConflict {
                path,
                kind: FsConflictKind::ContentConflict,
                merged_with_markers: None,
            });
        }
    }
    Ok(())
}

fn normalize_mode(mode: u32) -> u32 {
    match mode & 0o170000 {
        0o120000 => 0o120000,
        _ if mode & 0o111 != 0 => 0o100755,
        _ => 0o100644,
    }
}

fn read_tree_raw<'a>(
    store: &impl ObjectStore<'a>,
    oid: Oid,
    out: &mut AVec<'a, u8>,
) -> Result<(), GitError> {
    let Some(kind) = store.read_object(&oid, out)? else {
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
    Ok(())
}

fn read_blob_raw<'a>(
    store: &impl ObjectStore<'a>,
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

fn join_rel<'a>(prefix: &str, name: &[u8], alloc: Alloc<'a>) -> Result<AString<'a>, GitError> {
    let name_str = core::str::from_utf8(name).map_err(|_| GitError {
        file: GitFile::Worktree,
        at: 0,
        kind: ErrorKind::Malformed,
    })?;
    if prefix.is_empty() {
        return Ok(String::from_str_in(name_str, alloc));
    }
    let mut s = String::with_capacity_in(prefix.len() + 1 + name_str.len(), alloc);
    s.push_str(prefix);
    s.push('/');
    s.push_str(name_str);
    Ok(s)
}
