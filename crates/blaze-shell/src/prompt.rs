//! In-process Rust prompt: cwd abbreviation, direct `.git/HEAD` reading (supporting
//! both regular repos and git worktrees where `.git` is a file), and a single non-blocking
//! background worker that checks git index dirtiness without ever stalling a keystroke.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::UNIX_EPOCH;

/// Parsed git HEAD reference for the enclosing repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHead {
    /// Branch name (e.g. `main`), tag, or short detached SHA (`a1b2c3d`).
    pub label: String,
    /// True when HEAD is detached at a commit hash rather than a symbolic ref.
    pub detached: bool,
    /// Resolved `.git` directory (for worktrees, the `gitdir:` target).
    pub git_dir: PathBuf,
    /// Repository working-tree root containing `.git`.
    pub work_tree: PathBuf,
}

/// Dirtiness summary computed asynchronously on the background status thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GitDirty {
    pub dirty: bool,
    pub modified: usize,
    pub untracked: usize,
}

/// Snapshot of prompt metadata captured at block submission and displayed live in the input bar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptInfo {
    pub cwd: String,
    pub cwd_display: String,
    pub git_branch: Option<String>,
    pub git_detached: bool,
    pub git_dirty: Option<GitDirty>,
    pub last_exit: i32,
    pub last_duration_ms: u64,
}

impl PromptInfo {
    /// Build a prompt snapshot synchronously (zero subprocesses: only direct stat/HEAD read).
    pub fn from_cwd(cwd: &str, home: &str, last_exit: i32, last_duration_ms: u64) -> Self {
        let head = read_git_head(cwd);
        let (git_branch, git_detached) = match head {
            Some(h) => (Some(h.label), h.detached),
            None => (None, false),
        };
        Self {
            cwd: cwd.to_owned(),
            cwd_display: abbreviate_home(cwd, home),
            git_branch,
            git_detached,
            git_dirty: None,
            last_exit,
            last_duration_ms,
        }
    }
}

/// Replace a leading `home` directory prefix with `~` on a path boundary.
pub fn abbreviate_home(cwd: &str, home: &str) -> String {
    if home.is_empty() || home == "/" {
        return cwd.to_owned();
    }
    let home_trimmed = home.trim_end_matches('/');
    if cwd == home_trimmed {
        return "~".to_owned();
    }
    if let Some(rest) = cwd.strip_prefix(home_trimmed)
        && rest.starts_with('/')
    {
        return format!("~{rest}");
    }
    cwd.to_owned()
}

/// Walk upward from `cwd` looking for `.git` (directory or worktree file) and parse `HEAD`
/// directly in-process with zero subprocesses.
pub fn read_git_head(cwd: &str) -> Option<GitHead> {
    let mut dir = PathBuf::from(cwd);
    for _ in 0..64 {
        let dot_git = dir.join(".git");
        if let Ok(meta) = fs::symlink_metadata(&dot_git) {
            let git_dir = if meta.is_dir() {
                dot_git
            } else if meta.is_file() {
                parse_gitdir_file(&dot_git, &dir)?
            } else {
                return None;
            };
            let head_path = git_dir.join("HEAD");
            let head_text = fs::read_to_string(&head_path).ok()?;
            let (label, detached) = parse_head_contents(&head_text)?;
            return Some(GitHead { label, detached, git_dir, work_tree: dir });
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

fn parse_gitdir_file(dot_git_file: &Path, work_tree: &Path) -> Option<PathBuf> {
    let content = fs::read_to_string(dot_git_file).ok()?;
    let line = content.lines().next()?.trim();
    let raw = line.strip_prefix("gitdir:")?.trim();
    if raw.is_empty() {
        return None;
    }
    let p = PathBuf::from(raw);
    if p.is_absolute() { Some(p) } else { Some(work_tree.join(p)) }
}

fn parse_head_contents(contents: &str) -> Option<(String, bool)> {
    let line = contents.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    if let Some(rest) = line.strip_prefix("ref:") {
        let r = rest.trim();
        let label = r
            .strip_prefix("refs/heads/")
            .or_else(|| r.strip_prefix("refs/tags/"))
            .or_else(|| r.strip_prefix("refs/remotes/"))
            .unwrap_or(r);
        if label.is_empty() {
            return None;
        }
        return Some((label.to_owned(), false));
    }
    // Detached HEAD commit hash: take short 7-char prefix if hex.
    let short: String = line.chars().take(7).collect();
    if short.is_empty() { None } else { Some((short, true)) }
}

/// Check git working-tree dirtiness in-process by reading `<git_dir>/index` (v2/v3) and
/// comparing each tracked entry's `(mtime_sec, size)` against `symlink_metadata`, bounded to
/// keep latency tiny even in huge monorepos.
pub fn check_git_dirty(head: &GitHead) -> GitDirty {
    // Active merge / rebase / cherry-pick immediately marks repo dirty.
    if head.git_dir.join("MERGE_HEAD").exists()
        || head.git_dir.join("CHERRY_PICK_HEAD").exists()
        || head.git_dir.join("rebase-merge").exists()
        || head.git_dir.join("rebase-apply").exists()
    {
        return GitDirty { dirty: true, modified: 1, untracked: 0 };
    }

    let index_path = head.git_dir.join("index");
    let Ok(bytes) = fs::read(&index_path) else {
        return GitDirty::default();
    };
    parse_index_and_stat(&bytes, &head.work_tree)
}

const MAX_INDEX_ENTRIES: usize = 25_000;

fn parse_index_and_stat(bytes: &[u8], work_tree: &Path) -> GitDirty {
    if bytes.len() < 12 || &bytes[0..4] != b"DIRC" {
        return GitDirty::default();
    }
    let version = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if version != 2 && version != 3 {
        // v4 uses prefix-compressed paths; conservatively report clean rather than misreading.
        return GitDirty::default();
    }
    let entry_count = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    let limit = entry_count.min(MAX_INDEX_ENTRIES);

    let mut pos = 12usize;
    let mut modified = 0usize;

    for _ in 0..limit {
        // Fixed 62-byte entry header before optional 2-byte extended flags + NUL-terminated path.
        if pos + 62 > bytes.len() {
            break;
        }
        let entry_start = pos;
        let mtime_sec = u32::from_be_bytes([
            bytes[pos + 8],
            bytes[pos + 9],
            bytes[pos + 10],
            bytes[pos + 11],
        ]);
        let mode = u32::from_be_bytes([
            bytes[pos + 24],
            bytes[pos + 25],
            bytes[pos + 26],
            bytes[pos + 27],
        ]);
        let file_size = u32::from_be_bytes([
            bytes[pos + 36],
            bytes[pos + 37],
            bytes[pos + 38],
            bytes[pos + 39],
        ]);
        let flags = u16::from_be_bytes([bytes[pos + 60], bytes[pos + 61]]);
        let extended = version >= 3 && (flags & 0x4000) != 0;
        let name_start = if extended { pos + 64 } else { pos + 62 };
        if name_start >= bytes.len() {
            break;
        }
        let Some(nul_rel) = bytes[name_start..].iter().position(|&b| b == 0) else {
            break;
        };
        let name_bytes = &bytes[name_start..name_start + nul_rel];
        // Advance to next 8-byte aligned entry boundary (with at least 1 NUL byte).
        let raw_len = (name_start - entry_start) + nul_rel + 1;
        let padded_len = (raw_len + 7) & !7;
        pos = entry_start.saturating_add(padded_len);

        // Skip submodules (gitlink mode 0o160000).
        if (mode & 0o170000) == 0o160000 {
            continue;
        }
        let Ok(rel_path) = core::str::from_utf8(name_bytes) else {
            continue;
        };
        let full_path = work_tree.join(rel_path);
        match fs::symlink_metadata(&full_path) {
            Ok(meta) => {
                let sz = meta.len() as u32;
                let mt = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as u32)
                    .unwrap_or(0);
                // Fast stat check matching git's racy-index heuristic: size mismatch or newer mtime.
                if (file_size != 0 && sz != file_size) || (mtime_sec != 0 && mt != mtime_sec) {
                    modified += 1;
                    // Early exit once we know the tree is dirty and have a small count for display.
                    if modified >= 64 {
                        break;
                    }
                }
            }
            Err(_) => {
                // Tracked file deleted on disk.
                modified += 1;
                if modified >= 64 {
                    break;
                }
            }
        }
    }

    GitDirty { dirty: modified > 0, modified, untracked: 0 }
}

struct WorkerSlot {
    pending_cwd: Option<String>,
    shutdown: bool,
}

/// Single background thread for asynchronous git status checks (coalesced bounded mailbox,
/// atomic dirty notification, zero UI-thread blocking).
pub struct GitStatusWorker {
    req: Arc<(Mutex<WorkerSlot>, Condvar)>,
    result: Arc<Mutex<Option<(String, GitDirty)>>>,
    has_update: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl GitStatusWorker {
    pub fn spawn() -> Self {
        let req = Arc::new((
            Mutex::new(WorkerSlot { pending_cwd: None, shutdown: false }),
            Condvar::new(),
        ));
        let result: Arc<Mutex<Option<(String, GitDirty)>>> = Arc::new(Mutex::new(None));
        let has_update = Arc::new(AtomicBool::new(false));

        let req_bg = Arc::clone(&req);
        let res_bg = Arc::clone(&result);
        let flag_bg = Arc::clone(&has_update);

        let handle = thread::Builder::new()
            .name("blaze-git-status".to_owned())
            .spawn(move || {
                loop {
                    let cwd = {
                        let (lock, cvar) = &*req_bg;
                        let Ok(mut slot) = lock.lock() else { return };
                        while slot.pending_cwd.is_none() && !slot.shutdown {
                            match cvar.wait(slot) {
                                Ok(g) => slot = g,
                                Err(_) => return,
                            }
                        }
                        if slot.shutdown {
                            return;
                        }
                        slot.pending_cwd.take()
                    };
                    if let Some(cwd) = cwd {
                        let dirty = read_git_head(&cwd)
                            .map(|h| check_git_dirty(&h))
                            .unwrap_or_default();
                        if let Ok(mut out) = res_bg.lock() {
                            *out = Some((cwd, dirty));
                            flag_bg.store(true, Ordering::Release);
                        }
                    }
                }
            })
            .ok();

        Self { req, result, has_update, handle }
    }

    /// Schedule a background git dirtiness refresh for `cwd`. Coalesces any unstarted previous
    /// request so the mailbox stays strictly bounded to 1 slot.
    pub fn request(&self, cwd: &str) {
        let (lock, cvar) = &*self.req;
        if let Ok(mut slot) = lock.lock() {
            slot.pending_cwd = Some(cwd.to_owned());
            cvar.notify_one();
        }
    }

    /// Non-blocking poll for a completed status result (single atomic load when idle).
    pub fn poll(&self) -> Option<(String, GitDirty)> {
        if !self.has_update.swap(false, Ordering::AcqRel) {
            return None;
        }
        self.result.lock().ok()?.take()
    }
}

impl Drop for GitStatusWorker {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.req;
        if let Ok(mut slot) = lock.lock() {
            slot.shutdown = true;
            cvar.notify_all();
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
