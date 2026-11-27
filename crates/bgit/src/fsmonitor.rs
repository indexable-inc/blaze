//! In-process FSEvents / kernel journal dirty-path monitor for O(dirty) `bgit status` and `bgit diff`.
//!
//! Dogfoods [`blaze_core::Native`] directly (`Effect::Journal` + `Effect::Flush` cookie barrier)
//! with `#![forbid(unsafe_code)]`:
//! - One-shot CLI invocations never start a watch (zero startup overhead).
//! - Long-lived hosts (such as `blaze-shell`) call [`ensure_watching`] once per repository; every
//!   subsequent `bgit status` and `bgit diff` flushes the kernel journal barrier (`Effect::Flush`)
//!   and touches **only** the `K` dirty paths instead of scanning the entire working tree.
//! - Linked worktrees are handled cleanly: working-tree events come from `Effect::Journal` on the
//!   worktree root, while `.git/index`, `HEAD`, active branch ref, `packed-refs`, `MERGE_HEAD`,
//!   and `.bgit/refs.bgref` are validated via lightweight metadata stamps (~10 µs).

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use blaze_core::{
    Completion, Cursor, DirName, Done, Driver, Effect, Native, Tag, wait::Deadline,
};
use blaze_git::{
    ChangeKind, ChangedSample, Detail, FileStatus, Head, PromptStatus, RepoState, ShortOid,
    StatusCounts, StatusEntry, StatusOpts, Truncated, UntrackedMode, Upstream, WorktreeKind,
};
use blaze_git_fetch::{Kind as ObjectKind, Oid};
use blaze_own::{Alloc, String as OwnString, Vec as OwnVec};

use crate::index_io::{WritableIndexEntry, index_path_for, load_index};

const MAX_WATCHED_REPOS: usize = 8;
const WATCH_TAG: Tag = Tag(1);
const START_TIMEOUT: Duration = Duration::from_secs(2);
const FLUSH_TIMEOUT: Duration = Duration::from_millis(400);
const MAX_INCREMENTAL_PATHS: usize = 64;

static REGISTRY: OnceLock<Mutex<WatchRegistry>> = OnceLock::new();

#[derive(Default)]
struct WatchRegistry {
    watches: HashMap<PathBuf, Arc<Mutex<RepoMonitor>>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct FileSig {
    mtime_sec: i64,
    mtime_nsec: i64,
    ctime_sec: i64,
    ctime_nsec: i64,
    size: u64,
    ino: u64,
}

fn file_sig(path: &Path) -> Option<FileSig> {
    let m = fs::symlink_metadata(path).ok()?;
    Some(FileSig {
        mtime_sec: m.mtime(),
        mtime_nsec: m.mtime_nsec(),
        ctime_sec: m.ctime(),
        ctime_nsec: m.ctime_nsec(),
        size: m.size(),
        ino: m.ino(),
    })
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct RepoMetaStamp {
    pub(crate) index_sig: Option<FileSig>,
    head_sig: Option<FileSig>,
    head_ref_sig: Option<FileSig>,
    packed_refs_sig: Option<FileSig>,
    logs_head_sig: Option<FileSig>,
    bgit_refs_sig: Option<FileSig>,
    merge_head_sig: Option<FileSig>,
    rebase_merge_sig: Option<FileSig>,
    rebase_apply_sig: Option<FileSig>,
    cherry_pick_sig: Option<FileSig>,
    revert_sig: Option<FileSig>,
    stash_log_sig: Option<FileSig>,
}

pub(crate) fn compute_meta_stamp(
    git_dir: &Path,
    common_dir: &Path,
    bgit_dir: Option<&Path>,
) -> RepoMetaStamp {
    let head_path = git_dir.join("HEAD");
    let head_sig = file_sig(&head_path);
    let head_ref_sig = fs::read_to_string(&head_path)
        .ok()
        .and_then(|s| s.trim().strip_prefix("ref:").map(|r| r.trim().to_owned()))
        .and_then(|rel| file_sig(&common_dir.join(&rel)).or_else(|| file_sig(&git_dir.join(&rel))));
    RepoMetaStamp {
        index_sig: file_sig(&git_dir.join("index")),
        head_sig,
        head_ref_sig,
        packed_refs_sig: file_sig(&common_dir.join("packed-refs")),
        logs_head_sig: file_sig(&git_dir.join("logs/HEAD")),
        bgit_refs_sig: bgit_dir.and_then(|b| file_sig(&b.join("refs.bgref"))),
        merge_head_sig: file_sig(&git_dir.join("MERGE_HEAD")),
        rebase_merge_sig: file_sig(&git_dir.join("rebase-merge")),
        rebase_apply_sig: file_sig(&git_dir.join("rebase-apply")),
        cherry_pick_sig: file_sig(&git_dir.join("CHERRY_PICK_HEAD")),
        revert_sig: file_sig(&git_dir.join("REVERT_HEAD")),
        stash_log_sig: file_sig(&common_dir.join("logs/refs/stash")),
    }
}

/// Owned snapshot of `PromptStatus<'a>` stored inside `RepoMonitor` across arena lifetimes.
#[derive(Clone)]
struct CachedPromptStatus {
    head: CachedHead,
    tag: Option<String>,
    state: RepoState,
    upstream: Option<CachedUpstream>,
    counts: StatusCounts,
    worktree: WorktreeKind,
    truncated: Truncated,
    changed_samples: Vec<(String, ChangeKind)>,
    entries: Vec<CachedStatusEntry>,
}

#[derive(Clone)]
enum CachedHead {
    Branch { name: String, oid: Option<Oid>, short: Option<ShortOid> },
    Detached { oid: Oid, short: ShortOid },
}

#[derive(Clone)]
struct CachedUpstream {
    remote: String,
    branch: String,
    oid: Option<Oid>,
    divergence: blaze_git::Divergence,
}

#[derive(Clone)]
struct CachedStatusEntry {
    path: String,
    index: FileStatus,
    worktree: FileStatus,
    unmerged: bool,
}

impl CachedPromptStatus {
    fn from_status(s: &PromptStatus<'_>) -> Self {
        let head = match &s.head {
            Head::Branch { name, oid, short } => {
                CachedHead::Branch { name: name.as_str().to_owned(), oid: *oid, short: *short }
            }
            Head::Detached { oid, short } => CachedHead::Detached { oid: *oid, short: *short },
        };
        let upstream = s.upstream.as_ref().map(|u| CachedUpstream {
            remote: u.remote.as_str().to_owned(),
            branch: u.branch.as_str().to_owned(),
            oid: u.oid,
            divergence: u.divergence,
        });
        let changed_samples = s
            .changed_samples
            .iter()
            .map(|c| (c.path.as_str().to_owned(), c.kind))
            .collect();
        let entries = s
            .entries
            .iter()
            .map(|e| CachedStatusEntry {
                path: e.path.as_str().to_owned(),
                index: e.index,
                worktree: e.worktree,
                unmerged: e.unmerged,
            })
            .collect();
        Self {
            head,
            tag: s.tag.as_ref().map(|t| t.as_str().to_owned()),
            state: s.state,
            upstream,
            counts: s.counts,
            worktree: s.worktree,
            truncated: s.truncated,
            changed_samples,
            entries,
        }
    }

    fn materialize<'a>(&self, want_entries: bool, alloc: Alloc<'a>) -> PromptStatus<'a> {
        let head = match &self.head {
            CachedHead::Branch { name, oid, short } => Head::Branch {
                name: OwnString::from_str_in(name, alloc),
                oid: *oid,
                short: *short,
            },
            CachedHead::Detached { oid, short } => Head::Detached { oid: *oid, short: *short },
        };
        let upstream = self.upstream.as_ref().map(|u| Upstream {
            remote: OwnString::from_str_in(&u.remote, alloc),
            branch: OwnString::from_str_in(&u.branch, alloc),
            oid: u.oid,
            divergence: u.divergence,
        });
        let mut changed_samples = OwnVec::with_capacity_in(self.changed_samples.len(), alloc);
        for (p, k) in &self.changed_samples {
            changed_samples
                .push(ChangedSample { path: OwnString::from_str_in(p, alloc), kind: *k });
        }
        let mut entries = if want_entries {
            OwnVec::with_capacity_in(self.entries.len(), alloc)
        } else {
            OwnVec::new_in(alloc)
        };
        if want_entries {
            for e in &self.entries {
                entries.push(StatusEntry {
                    path: OwnString::from_str_in(&e.path, alloc),
                    index: e.index,
                    worktree: e.worktree,
                    unmerged: e.unmerged,
                });
            }
        }
        PromptStatus {
            head,
            tag: self.tag.as_deref().map(|t| OwnString::from_str_in(t, alloc)),
            state: self.state,
            upstream,
            counts: self.counts,
            worktree: self.worktree,
            truncated: self.truncated,
            changed_samples,
            entries,
        }
    }

    /// Recomputes `counts` and `changed_samples` from `entries` after an O(K) entry update.
    fn recompute_summaries(&mut self) {
        let stashed = self.counts.stashed;
        let mut c = StatusCounts { stashed, ..StatusCounts::default() };
        self.changed_samples.clear();
        for e in &self.entries {
            if e.unmerged {
                c.conflicted += 1;
                if self.changed_samples.len() < 24 {
                    self.changed_samples.push((e.path.clone(), ChangeKind::Conflicted));
                }
                continue;
            }
            if e.index == FileStatus::Untracked && e.worktree == FileStatus::Untracked {
                c.untracked += 1;
                if self.changed_samples.len() < 24 {
                    self.changed_samples.push((e.path.clone(), ChangeKind::Untracked));
                }
                continue;
            }
            match e.index {
                FileStatus::Added => c.staged_added += 1,
                FileStatus::Modified => c.staged_modified += 1,
                FileStatus::Deleted => c.staged_deleted += 1,
                FileStatus::TypeChanged => c.staged_typechanged += 1,
                _ => {}
            }
            match e.worktree {
                FileStatus::Modified => c.modified += 1,
                FileStatus::Deleted => c.deleted += 1,
                FileStatus::TypeChanged => c.typechanged += 1,
                _ => {}
            }
            if self.changed_samples.len() < 24 {
                let kind = match e.worktree {
                    FileStatus::Deleted => Some(ChangeKind::Deleted),
                    FileStatus::Modified | FileStatus::TypeChanged => Some(ChangeKind::Modified),
                    _ if e.index != FileStatus::Unmodified => Some(ChangeKind::Staged),
                    _ => None,
                };
                if let Some(k) = kind {
                    self.changed_samples.push((e.path.clone(), k));
                }
            }
        }
        self.counts = c;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct StatusOptsKey {
    untracked: UntrackedMode,
    detail: Detail,
    ahead_behind: blaze_git::AheadBehindMode,
    tag: blaze_git::TagMode,
    short_oid_len: u8,
}

impl From<StatusOpts> for StatusOptsKey {
    fn from(o: StatusOpts) -> Self {
        Self {
            untracked: o.untracked,
            detail: o.detail,
            ahead_behind: o.ahead_behind,
            tag: o.tag,
            short_oid_len: o.short_oid_len,
        }
    }
}

struct DiffBaseline {
    index_sig: FileSig,
    known_dirty: BTreeSet<String>,
    pending_paths: BTreeSet<String>,
    needs_full: bool,
}

struct StatusBaseline {
    meta_stamp: RepoMetaStamp,
    opts: StatusOptsKey,
    snapshot: CachedPromptStatus,
    pending_paths: BTreeSet<String>,
    needs_full: bool,
}

struct CachedIndex {
    sig: FileSig,
    entries: BTreeMap<String, WritableIndexEntry>,
}

struct RepoMonitor {
    root: PathBuf,
    driver: Native,
    next_tag: u64,
    alive: bool,
    last_used: Instant,
    generation: u64,
    cached_index: Option<CachedIndex>,
    diff_baseline: Option<DiffBaseline>,
    status_baseline: Option<StatusBaseline>,
}

impl Drop for RepoMonitor {
    fn drop(&mut self) {
        if self.alive {
            let mut effects = vec![Effect::Unwatch { tag: WATCH_TAG }];
            self.driver.submit(&mut effects);
            let mut out = Vec::new();
            let until = Deadline::at(self.driver.now() + Duration::from_millis(50));
            let _ = self.driver.poll(&mut out, until);
            self.alive = false;
        }
    }
}

impl RepoMonitor {
    fn start(root: PathBuf) -> io::Result<Self> {
        let mut driver = blaze_core::native(&root, 1)?;
        let mut effects =
            vec![Effect::Journal { dir: DirName::Root, since: Cursor::None, tag: WATCH_TAG }];
        driver.submit(&mut effects);

        let deadline = Deadline::at(driver.now() + START_TIMEOUT);
        let mut out: Vec<Completion> = Vec::new();
        let mut caught_up = false;
        while !caught_up {
            out.clear();
            let n = driver.poll(&mut out, deadline);
            if n == 0 {
                break;
            }
            for comp in out.drain(..) {
                match comp.done {
                    Done::Journaled(batch) if comp.tag == WATCH_TAG => {
                        if batch.caught_up || batch.cursor != Cursor::None {
                            caught_up = true;
                        }
                    }
                    Done::Failed(e) if comp.tag == WATCH_TAG => {
                        return Err(io::Error::other(format!("journal watch failed: {e}")));
                    }
                    _ => {}
                }
            }
        }
        if !caught_up {
            let mut stop = vec![Effect::Unwatch { tag: WATCH_TAG }];
            driver.submit(&mut stop);
            return Err(io::Error::new(io::ErrorKind::TimedOut, "journal start timed out"));
        }

        Ok(Self {
            root,
            driver,
            next_tag: 2,
            alive: true,
            last_used: Instant::now(),
            generation: 1,
            cached_index: None,
            diff_baseline: None,
            status_baseline: None,
        })
    }

    /// Flushes the kernel journal via [`Effect::Flush`] cookie barrier so every filesystem write
    /// issued before this call is guaranteed to have been ingested into `diff_baseline` and
    /// `status_baseline`.
    fn flush(&mut self) -> io::Result<()> {
        if !self.alive {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "fsmonitor stopped"));
        }
        self.last_used = Instant::now();
        let flush_tag = Tag(self.next_tag);
        self.next_tag = self.next_tag.wrapping_add(1).max(2);

        let mut effects = vec![Effect::Flush { watch: WATCH_TAG, tag: flush_tag }];
        self.driver.submit(&mut effects);

        let deadline = Deadline::at(self.driver.now() + FLUSH_TIMEOUT);
        let mut out: Vec<Completion> = Vec::new();
        let mut flushed = false;
        while !flushed {
            out.clear();
            let n = self.driver.poll(&mut out, deadline);
            if n == 0 {
                break;
            }
            for comp in out.drain(..) {
                match comp.done {
                    Done::Flushed if comp.tag == flush_tag => {
                        flushed = true;
                    }
                    Done::Journaled(batch) if comp.tag == WATCH_TAG => {
                        self.ingest_batch(*batch);
                    }
                    Done::Failed(e) if comp.tag == WATCH_TAG => {
                        self.alive = false;
                        return Err(io::Error::other(format!("journal failed: {e}")));
                    }
                    Done::Failed(e) if comp.tag == flush_tag => {
                        self.alive = false;
                        return Err(io::Error::other(format!("journal flush failed: {e}")));
                    }
                    _ => {}
                }
            }
        }

        if !flushed {
            self.invalidate_all();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "journal flush timed out"));
        }
        Ok(())
    }

    /// Non-blocking drain of any completions already queued in the kernel driver (0 µs, no cookie).
    fn drain_ready(&mut self) {
        if !self.alive {
            return;
        }
        let mut out: Vec<Completion> = Vec::new();
        while self.driver.poll(&mut out, Deadline::PASSED) > 0 {
            for comp in out.drain(..) {
                match comp.done {
                    Done::Journaled(batch) if comp.tag == WATCH_TAG => {
                        self.ingest_batch(*batch);
                    }
                    Done::Failed(_) if comp.tag == WATCH_TAG => {
                        self.alive = false;
                        self.invalidate_all();
                        return;
                    }
                    _ => {}
                }
            }
        }
    }

    fn invalidate_all(&mut self) {
        if let Some(d) = self.diff_baseline.as_mut() {
            d.needs_full = true;
        }
        if let Some(s) = self.status_baseline.as_mut() {
            s.needs_full = true;
        }
    }

    fn ingest_batch(&mut self, batch: blaze_core::Journaled) {
        let lost = batch.whole.is_some() || !batch.lost.is_empty();
        if lost {
            self.generation = self.generation.wrapping_add(1);
            self.invalidate_all();
            return;
        }
        if batch.files.is_empty() {
            return;
        }
        self.generation = self.generation.wrapping_add(1);

        for f in &batch.files {
            let rel = f.as_str();
            // Ignore `.bgit/` internal slabs/refs/cookies so our own reads/writes never thrash.
            if rel == ".bgit" || rel.starts_with(".bgit/") {
                continue;
            }
            if rel == ".git" || rel.starts_with(".git/") {
                if let Some(s) = self.status_baseline.as_mut() {
                    s.needs_full = true;
                }
                continue;
            }
            let is_ignore_or_submod = rel == ".gitignore"
                || rel.ends_with("/.gitignore")
                || rel == ".gitmodules"
                || rel == ".gitattributes";
            if let Some(d) = self.diff_baseline.as_mut()
                && !d.needs_full
            {
                d.pending_paths.insert(rel.to_owned());
            }
            if let Some(s) = self.status_baseline.as_mut()
                && !s.needs_full
            {
                if is_ignore_or_submod {
                    s.needs_full = true;
                } else {
                    s.pending_paths.insert(rel.to_owned());
                }
            }
        }
    }

    fn index_for_sig<'a>(
        &mut self,
        sig: FileSig,
        alloc: Alloc<'a>,
    ) -> io::Result<&BTreeMap<String, WritableIndexEntry>> {
        let fresh = self.cached_index.as_ref().is_some_and(|ci| ci.sig == sig);
        if !fresh {
            let entries = load_index(&self.root, alloc)?;
            self.cached_index = Some(CachedIndex { sig, entries });
        }
        Ok(&self.cached_index.as_ref().expect("populated").entries)
    }
}

fn canonical_root(repo_root: &Path) -> PathBuf {
    fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf())
}

fn get_watch(repo_root: &Path) -> Option<Arc<Mutex<RepoMonitor>>> {
    let reg = REGISTRY.get()?;
    let canon = canonical_root(repo_root);
    let guard = reg.lock().ok()?;
    guard.watches.get(&canon).cloned()
}

/// Ensures an in-process FSEvents / kernel journal monitor is running for `repo_root`.
///
/// Safe to call repeatedly (O(1) hash lookup once started). Returns `true` if the monitor is
/// active for this repository.
pub(crate) fn ensure_watching(repo_root: &Path) -> bool {
    let canon = canonical_root(repo_root);
    let reg = REGISTRY.get_or_init(|| Mutex::new(WatchRegistry::default()));
    {
        let Ok(guard) = reg.lock() else {
            return false;
        };
        if let Some(existing) = guard.watches.get(&canon) {
            if let Ok(mut mon) = existing.lock()
                && mon.alive
            {
                mon.last_used = Instant::now();
                return true;
            }
        }
    }

    let Ok(monitor) = RepoMonitor::start(canon.clone()) else {
        return false;
    };
    let arc = Arc::new(Mutex::new(monitor));

    let Ok(mut guard) = reg.lock() else {
        return false;
    };
    if guard.watches.len() >= MAX_WATCHED_REPOS && !guard.watches.contains_key(&canon) {
        // Evict least-recently-used watch to keep OS handles strictly bounded.
        let oldest = guard
            .watches
            .iter()
            .filter_map(|(k, v)| v.try_lock().ok().map(|m| (k.clone(), m.last_used)))
            .min_by_key(|(_, ts)| *ts)
            .map(|(k, _)| k);
        if let Some(evict) = oldest {
            guard.watches.remove(&evict);
        }
    }
    guard.watches.insert(canon, arc);
    true
}

/// Returns whether an in-process FSEvents monitor is currently active for `repo_root`.
pub(crate) fn is_watching(repo_root: &Path) -> bool {
    get_watch(repo_root)
        .and_then(|w| w.lock().ok().map(|m| m.alive))
        .unwrap_or(false)
}

/// Flushes and snapshots the monitor's generation before starting a full scan so any writes that
/// happen *during* the full scan are preserved in `pending_paths` instead of being lost.
pub(crate) fn begin_full_scan(repo_root: &Path) -> Option<u64> {
    let watch = get_watch(repo_root)?;
    let mut mon = watch.lock().ok()?;
    mon.flush().ok()?;
    Some(mon.generation)
}

// ============================================================================
// O(dirty) `bgit diff` (Working tree vs Index)
// ============================================================================

/// Result of querying the in-process FSEvents monitor for `bgit diff` (Worktree vs Index).
pub(crate) enum DiffFastPath {
    /// Clean working tree — 0 files dirty, 0 index parsing needed!
    Clean,
    /// Only these specific tracked entries could possibly be dirty (O(K) instead of O(N)).
    Candidates(Vec<WritableIndexEntry>),
}

/// Attempts an O(dirty) `bgit diff` candidate lookup using the active FSEvents watch.
///
/// Returns `None` when no watch is active, on the first scan before baseline priming, after an
/// FSEvents overflow, or when the index file changed since the last scan.
pub(crate) fn try_diff_candidates<'a>(
    repo_root: &Path,
    alloc: Alloc<'a>,
) -> Option<DiffFastPath> {
    let watch = get_watch(repo_root)?;
    let mut mon = watch.lock().ok()?;
    mon.flush().ok()?;

    let cur_sig = file_sig(&index_path_for(repo_root))?;
    let candidates: Vec<String> = {
        let base = mon.diff_baseline.as_mut()?;
        if base.needs_full || base.index_sig != cur_sig {
            return None;
        }
        if base.known_dirty.is_empty() && base.pending_paths.is_empty() {
            return Some(DiffFastPath::Clean);
        }
        let pending = std::mem::take(&mut base.pending_paths);
        base.known_dirty.union(&pending).cloned().collect()
    };

    let idx = mon.index_for_sig(cur_sig, alloc).ok()?;
    let mut entries = Vec::with_capacity(candidates.len());
    for p in &candidates {
        if let Some(e) = idx.get(p) {
            entries.push(e.clone());
        }
    }
    if entries.is_empty() {
        // Every FSEvents path was untracked (not in index) — working tree vs index is clean!
        return Some(DiffFastPath::Clean);
    }
    Some(DiffFastPath::Candidates(entries))
}

/// Updates `known_dirty` after checking the candidate paths returned by [`try_diff_candidates`],
/// preserving any new FSEvents that arrived concurrently (zero extra cookie flush!).
pub(crate) fn update_diff_known_dirty(repo_root: &Path, dirty_paths: BTreeSet<String>) {
    let Some(watch) = get_watch(repo_root) else {
        return;
    };
    let Ok(mut mon) = watch.lock() else {
        return;
    };
    mon.drain_ready();
    if let Some(base) = mon.diff_baseline.as_mut()
        && !base.needs_full
    {
        base.known_dirty = dirty_paths;
    }
}

/// Records the exact set of dirty tracked worktree paths after a full `bgit diff` scan.
pub(crate) fn record_diff_baseline(
    repo_root: &Path,
    dirty_paths: BTreeSet<String>,
    start_gen: Option<u64>,
) {
    let Some(watch) = get_watch(repo_root) else {
        return;
    };
    let Some(sig) = file_sig(&index_path_for(repo_root)) else {
        return;
    };
    let Ok(mut mon) = watch.lock() else {
        return;
    };
    mon.drain_ready();
    // If new FSEvents arrived while the scan was in progress, preserve them as pending.
    let concurrent_pending = match (&mon.diff_baseline, start_gen) {
        (Some(prev), Some(g)) if mon.generation != g => prev.pending_paths.clone(),
        _ => BTreeSet::new(),
    };
    mon.diff_baseline = Some(DiffBaseline {
        index_sig: sig,
        known_dirty: dirty_paths,
        pending_paths: concurrent_pending,
        needs_full: false,
    });
}

// ============================================================================
// O(dirty) `bgit status`
// ============================================================================

fn index_entry_stat_clean(repo_root: &Path, e: &WritableIndexEntry) -> bool {
    if e.mtime_sec == 0 && e.file_size == 0 {
        return false;
    }
    let Ok(meta) = fs::symlink_metadata(repo_root.join(&e.path)) else {
        return false;
    };
    if meta.is_dir() {
        return false;
    }
    meta.mtime() == i64::from(e.mtime_sec)
        && (meta.mtime_nsec() as u32) == e.mtime_nsec
        && (meta.size() as u32) == e.file_size
        && (meta.ino() as u32) == e.ino
}

/// Evaluates the worktree-vs-index `FileStatus` for one tracked index entry.
fn check_tracked_worktree_status(repo_root: &Path, e: &WritableIndexEntry) -> FileStatus {
    let full = repo_root.join(&e.path);
    let Ok(meta) = fs::symlink_metadata(&full) else {
        return FileStatus::Deleted;
    };
    let is_link = meta.file_type().is_symlink();
    let expected_link = (e.mode & 0o170000) == 0o120000;
    if meta.is_dir() || is_link != expected_link {
        return FileStatus::TypeChanged;
    }
    if index_entry_stat_clean(repo_root, e) {
        return FileStatus::Unmodified;
    }
    let bytes = if is_link {
        match fs::read_link(&full) {
            Ok(t) => t.as_os_str().as_encoded_bytes().to_vec(),
            Err(_) => return FileStatus::Deleted,
        }
    } else {
        match fs::read(&full) {
            Ok(b) => b,
            Err(_) => return FileStatus::Deleted,
        }
    };
    if Oid::of(ObjectKind::Blob, &bytes) == e.oid {
        FileStatus::Unmodified
    } else {
        FileStatus::Modified
    }
}

/// Attempts an O(dirty) `bgit status` lookup using the active FSEvents watch.
///
/// - Case A (0 worktree events, unchanged `.git` metadata): returns cached snapshot in ~0.5 ms!
/// - Case B (1..=64 touched paths, all tracked files in `.git/index`, `Detail::Full` baseline):
///   stats/hashes ONLY those `K` files, patches the cached snapshot in O(K), and returns in ~1 ms!
/// - Otherwise returns `None` so the caller runs the full multi-core `StatusMachine` scan.
pub(crate) fn try_cached_status<'a>(
    repo_root: &Path,
    git_dir: &Path,
    common_dir: &Path,
    bgit_dir: Option<&Path>,
    opts: StatusOpts,
    alloc: Alloc<'a>,
) -> Option<PromptStatus<'a>> {
    let watch = get_watch(repo_root)?;
    let mut mon = watch.lock().ok()?;
    mon.flush().ok()?;

    let cur_stamp = compute_meta_stamp(git_dir, common_dir, bgit_dir);
    let want_key = StatusOptsKey::from(opts);

    // First check whether the baseline is valid and compatible with `opts`.
    let (pending, can_patch_incremental) = {
        let base = mon.status_baseline.as_ref()?;
        if base.needs_full || base.meta_stamp != cur_stamp {
            return None;
        }
        let detail_ok = base.opts.detail == want_key.detail
            || (base.opts.detail == Detail::Full && want_key.detail != Detail::Full);
        if !detail_ok
            || base.opts.untracked != want_key.untracked
            || base.opts.ahead_behind != want_key.ahead_behind
            || base.opts.tag != want_key.tag
            || base.opts.short_oid_len != want_key.short_oid_len
        {
            return None;
        }
        if base.pending_paths.is_empty() {
            return Some(base.snapshot.materialize(opts.detail == Detail::Full, alloc));
        }
        let can_inc = base.opts.detail == Detail::Full
            && base.pending_paths.len() <= MAX_INCREMENTAL_PATHS
            && !base.snapshot.truncated.untracked_capped;
        (base.pending_paths.iter().cloned().collect::<Vec<_>>(), can_inc)
    };

    if !can_patch_incremental {
        return None;
    }

    // Case B: verify every touched path is a known tracked file in `.git/index` (not a new
    // untracked file or directory). If any path is untracked or a directory, do a full scan so
    // `.gitignore` rules are evaluated with 100% fidelity.
    let index_sig = cur_stamp.index_sig?;
    let root = mon.root.clone();
    let updates: Vec<(String, FileStatus)> = {
        let idx = mon.index_for_sig(index_sig, alloc).ok()?;
        let mut ups = Vec::with_capacity(pending.len());
        for p in &pending {
            let Some(entry) = idx.get(p) else {
                // Newly created untracked file or directory -> fall through to full scan.
                return None;
            };
            // Reject newly created directories replacing a file (could contain untracked children).
            if let Ok(m) = fs::symlink_metadata(root.join(p))
                && m.is_dir()
            {
                return None;
            }
            let st = check_tracked_worktree_status(&root, entry);
            ups.push((p.clone(), st));
        }
        ups
    };

    let base = mon.status_baseline.as_mut()?;
    for (path, new_wt) in updates {
        if let Some(pos) = base.snapshot.entries.iter().position(|e| e.path == path) {
            let entry = &mut base.snapshot.entries[pos];
            if entry.unmerged {
                return None;
            }
            entry.worktree = new_wt;
            if entry.index == FileStatus::Unmodified && entry.worktree == FileStatus::Unmodified {
                base.snapshot.entries.remove(pos);
            }
        } else if new_wt != FileStatus::Unmodified {
            let ins_at = base
                .snapshot
                .entries
                .binary_search_by(|e| e.path.as_str().cmp(path.as_str()))
                .unwrap_or_else(|i| i);
            base.snapshot.entries.insert(
                ins_at,
                CachedStatusEntry {
                    path,
                    index: FileStatus::Unmodified,
                    worktree: new_wt,
                    unmerged: false,
                },
            );
        }
    }
    base.snapshot.recompute_summaries();
    base.pending_paths.clear();

    Some(base.snapshot.materialize(opts.detail == Detail::Full, alloc))
}

/// Records a freshly computed `PromptStatus` snapshot into the active FSEvents monitor.
pub(crate) fn record_status_baseline(
    repo_root: &Path,
    git_dir: &Path,
    common_dir: &Path,
    bgit_dir: Option<&Path>,
    opts: StatusOpts,
    status: &PromptStatus<'_>,
    start_gen: Option<u64>,
) {
    let Some(watch) = get_watch(repo_root) else {
        return;
    };
    let Ok(mut mon) = watch.lock() else {
        return;
    };
    mon.drain_ready();
    let meta_stamp = compute_meta_stamp(git_dir, common_dir, bgit_dir);
    let concurrent_pending = match (&mon.status_baseline, start_gen) {
        (Some(prev), Some(g)) if mon.generation != g => prev.pending_paths.clone(),
        _ => BTreeSet::new(),
    };
    mon.status_baseline = Some(StatusBaseline {
        meta_stamp,
        opts: StatusOptsKey::from(opts),
        snapshot: CachedPromptStatus::from_status(status),
        pending_paths: concurrent_pending,
        needs_full: false,
    });
}
