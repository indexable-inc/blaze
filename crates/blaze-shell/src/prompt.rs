//! In-process Rust Starship-class prompt powered by `blaze-git`'s sans-IO hardware-floor
//! status engine (`crates/blaze-git`): zero subprocesses, full `.git/index` v2–v4 + `TREE`
//! cache-tree pruning, `.gitignore` stack, binary `commit-graph` ahead/behind, stash &
//! rebase/merge state, and a single coalesced background status worker.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
#[cfg(not(unix))]
use std::time::UNIX_EPOCH;

use blaze_git::machine::{
    DirListing, FileData, Got, PathScope, ScopedPath, StatEntry, StatusMachine, Step, Want,
};
use blaze_git::{
    ChangeKind, Divergence, FileKind, FileStat, Head, PromptStyle, RepoState, StatusOpts,
    WorktreeKind, format_status,
};
use blaze_own::planted::NoPlant;
use blaze_own::{AVec, Alloc, Vec as OwnVec};

/// Parsed git HEAD reference for the enclosing repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHead {
    /// Branch name (e.g. `main`), tag, or short detached SHA (`a1b2c3d`).
    pub label: String,
    /// True when HEAD is detached at a commit hash rather than a symbolic ref.
    pub detached: bool,
    /// Resolved per-worktree `.git` directory (for worktrees, the `gitdir:` target).
    pub git_dir: PathBuf,
    /// Shared common `.git` directory (`commondir` for linked worktrees, else same as `git_dir`).
    pub common_dir: PathBuf,
    /// Repository working-tree root containing `.git`.
    pub work_tree: PathBuf,
    /// Normal checkout vs linked worktree.
    pub kind: WorktreeKind,
}

/// Full Starship-class Git prompt summary produced by `blaze-git`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitStatusSummary {
    /// Branch name (or detached short OID).
    pub branch: String,
    pub detached: bool,
    /// Optional exact tag at HEAD when detached.
    pub tag: Option<String>,
    /// Active merge / rebase / cherry-pick / bisect label (e.g. `"REBASING 2/4"`).
    pub state_label: Option<String>,
    /// Exact Starship status bracket (e.g. `"[?]"`, `"[!2+1?]"`, `"[⇡1]"`, or `""` when clean).
    pub status_badge: String,
    /// Distinct IntelliJ SVG icon names for uncommitted files (`(icon_name, count)`), ordered by frequency.
    pub changed_icons: Vec<(&'static str, u32)>,
    /// Representative uncommitted files `(rel_path, icon_name, status_tag)` for hover inspection.
    pub changed_files: Vec<(String, &'static str, &'static str)>,
    /// Recent stash descriptions parsed directly from `logs/refs/stash` (newest first, max 8).
    pub stashes: Vec<String>,
    /// Local git branches read directly from `refs/heads` + `packed-refs` (current first, max 16).
    pub branches: Vec<String>,
    pub dirty: bool,
    pub conflicted: u32,
    pub staged: u32,
    pub modified: u32,
    pub deleted: u32,
    pub untracked: u32,
    pub stashed: u32,
    pub ahead: u32,
    pub behind: u32,
}

/// Snapshot of prompt metadata captured at block submission and displayed live in the composer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptInfo {
    /// Machine hostname (local laptop or remote server over SSH/BSH1).
    pub host: String,
    /// True when this session is connected to a remote server rather than the local Mac.
    pub is_remote: bool,
    pub cwd: String,
    /// Full `~`-abbreviated path (used in tooltips / titlebar).
    pub cwd_display: String,
    /// Concise Starship directory label (repo-rooted inside git, e.g. `blaze-shell-1124`).
    pub dir_short: String,
    pub git_branch: Option<String>,
    pub git_detached: bool,
    pub git_status: Option<GitStatusSummary>,
    /// True when a `Cargo.toml` lives in cwd or repo root.
    pub in_rust_project: bool,
    /// Detected project toolchain/ecosystem IntelliJ SVG icons (e.g. `rust_dark`, `nix_dark`, `typeScript_dark`).
    pub project_icons: Vec<&'static str>,
    /// Detailed project markers `(icon_name, toolchain_name, marker_file)` for hover inspection.
    pub project_details: Vec<(&'static str, &'static str, &'static str)>,
    pub last_exit: i32,
    pub last_duration_ms: u64,
}

impl PromptInfo {
    /// Build a prompt snapshot synchronously (zero subprocesses: only direct stat/HEAD read).
    pub fn from_cwd(cwd: &str, home: &str, last_exit: i32, last_duration_ms: u64) -> Self {
        let head = read_git_head(cwd);
        let cwd_display = abbreviate_home(cwd, home);
        let dir_short = short_dir_display(cwd, home, head.as_ref().map(|h| h.work_tree.as_path()));
        let root = head.as_ref().map(|h| h.work_tree.as_path());
        let has_marker = |name: &str| {
            Path::new(cwd).join(name).exists() || root.is_some_and(|r| r.join(name).exists())
        };
        let in_rust_project = has_marker("Cargo.toml");
        let mut project_icons = Vec::new();
        let mut project_details = Vec::new();
        if in_rust_project {
            project_icons.push("rust_dark");
            project_details.push(("rust_dark", "Rust", "Cargo.toml"));
        }
        if has_marker("tsconfig.json") {
            project_icons.push("typeScript_dark");
            project_details.push(("typeScript_dark", "TypeScript", "tsconfig.json"));
        } else if has_marker("package.json") {
            project_icons.push("javaScript_dark");
            project_details.push(("javaScript_dark", "JavaScript", "package.json"));
        }
        if has_marker("pyproject.toml") {
            project_icons.push("python_dark");
            project_details.push(("python_dark", "Python", "pyproject.toml"));
        } else if has_marker("requirements.txt") {
            project_icons.push("python_dark");
            project_details.push(("python_dark", "Python", "requirements.txt"));
        }
        if has_marker("go.mod") {
            project_icons.push("go_dark");
            project_details.push(("go_dark", "Go", "go.mod"));
        }
        if has_marker("flake.nix") {
            project_icons.push("nix_dark");
            project_details.push(("nix_dark", "Nix", "flake.nix"));
        }
        if has_marker("Dockerfile") {
            project_icons.push("docker_dark");
            project_details.push(("docker_dark", "Docker", "Dockerfile"));
        }
        let (git_branch, git_detached) = match head {
            Some(h) => (Some(h.label), h.detached),
            None => (None, false),
        };
        Self {
            host: crate::zoxide::detect_local_hostname(),
            is_remote: false,
            cwd: cwd.to_owned(),
            cwd_display,
            dir_short,
            git_branch,
            git_detached,
            git_status: None,
            in_rust_project,
            project_icons,
            project_details,
            last_exit,
            last_duration_ms,
        }
    }

    /// Synchronously compute full `blaze-git` Starship status (used in `--demo --screenshot`).
    pub fn refresh_git_sync(&mut self) {
        if let Some(summary) = query_blaze_git_status(&self.cwd) {
            self.git_branch = Some(summary.branch.clone());
            self.git_detached = summary.detached;
            self.git_status = Some(summary);
        }
    }
}

/// Starship-style concise directory label:
/// - Inside a git repo: `<repo_name>` at root, or `<repo_name>/<rel>` (last 3 segments if deep).
/// - Outside a git repo: `~` or last 2–3 path segments.
pub fn short_dir_display(cwd: &str, home: &str, work_tree: Option<&Path>) -> String {
    if let Some(root) = work_tree {
        let root_name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("/");
        if let Ok(rel) = Path::new(cwd).strip_prefix(root) {
            let rel_str = rel.to_string_lossy();
            if rel_str.is_empty() {
                return root_name.to_owned();
            }
            let comps: Vec<&str> = rel_str.split('/').filter(|s| !s.is_empty()).collect();
            if comps.len() <= 2 {
                return format!("{root_name}/{}", comps.join("/"));
            }
            return format!(
                "{root_name}/…/{}/{}",
                comps[comps.len() - 2],
                comps[comps.len() - 1]
            );
        }
    }
    let abbr = abbreviate_home(cwd, home);
    if abbr == "~" || abbr == "/" {
        return abbr;
    }
    let parts: Vec<&str> = abbr.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        abbr
    } else if abbr.starts_with("~/") {
        format!("~/…/{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
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
            let (git_dir, common_dir, kind) = if meta.is_dir() {
                (dot_git.clone(), dot_git, WorktreeKind::Normal)
            } else if meta.is_file() {
                let gd = parse_gitdir_file(&dot_git, &dir)?;
                let cd = resolve_common_dir(&gd);
                (gd, cd, WorktreeKind::LinkedWorktree)
            } else {
                return None;
            };
            let head_path = git_dir.join("HEAD");
            let head_text = fs::read_to_string(&head_path).ok()?;
            let (label, detached) = parse_head_contents(&head_text)?;
            return Some(GitHead {
                label,
                detached,
                git_dir,
                common_dir,
                work_tree: dir,
                kind,
            });
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

fn resolve_common_dir(git_dir: &Path) -> PathBuf {
    let cd_file = git_dir.join("commondir");
    if let Ok(raw) = fs::read_to_string(&cd_file) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let p = PathBuf::from(trimmed);
            return if p.is_absolute() { p } else { git_dir.join(p) };
        }
    }
    git_dir.to_path_buf()
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
    let short: String = line.chars().take(7).collect();
    if short.is_empty() { None } else { Some((short, true)) }
}

// =============================================================================
// Thin OS driver over `blaze-git`'s sans-IO `StatusMachine`
// =============================================================================

/// Run `blaze-git`'s full Starship-class sans-IO `StatusMachine` for `cwd`.
/// Zero subprocesses, supports normal repos & linked worktrees, `.gitignore`, index v2–v4,
/// `TREE` cache-tree pruning, binary `commit-graph` ahead/behind, and stash counts.
pub fn query_blaze_git_status(cwd: &str) -> Option<GitStatusSummary> {
    let head = read_git_head(cwd)?;
    let alloc = Alloc::global();
    let mut machine = StatusMachine::new(StatusOpts::STARSHIP, head.kind, alloc, NoPlant);

    // Supply user global gitignore (`~/.config/git/ignore`) if present.
    if let Ok(home) = std::env::var("HOME") {
        let global_ig = Path::new(&home).join(".config/git/ignore");
        if let Ok(bytes) = fs::read(&global_ig) {
            machine.set_global_exclude(&bytes);
        }
    }

    // Strictly bounded IO rounds (rule `bounded-progress`).
    const MAX_ROUNDS: usize = 256;
    for _ in 0..MAX_ROUNDS {
        match machine.poll() {
            Ok(Step::Done(status)) => {
                let (branch, detached) = match &status.head {
                    Head::Branch { name, .. } => (name.as_str().to_owned(), false),
                    Head::Detached { short, .. } => (short.as_str().to_owned(), true),
                };
                let tag = status.tag.as_ref().map(|t| t.as_str().to_owned());
                let state_label = status.state.label().map(|lbl| match status.state {
                    RepoState::Rebase { step: Some(s), total: Some(t), .. }
                    | RepoState::Am { step: Some(s), total: Some(t) }
                    | RepoState::AmOrRebase { step: Some(s), total: Some(t) } => {
                        format!("{lbl} {s}/{t}")
                    }
                    _ => lbl.to_owned(),
                });
                let c = status.counts;
                let staged = c.staged();
                let div = status.upstream.as_ref().map(|u| u.divergence);
                let ahead = div.map_or(0, Divergence::ahead);
                let behind = div.map_or(0, Divergence::behind);

                // Format exact Starship status bracket using `blaze-git`'s formatter.
                let mut full_fmt = String::new();
                let _ = format_status(&status, &PromptStyle::STARSHIP, &mut full_fmt);
                let status_badge = full_fmt
                    .rfind(" [")
                    .map(|idx| full_fmt[idx + 1..].to_owned())
                    .unwrap_or_default();

                // Collect distinct IntelliJ SVG icons and representative file paths for uncommitted files.
                let mut changed_icons: Vec<(&'static str, u32)> = Vec::new();
                let mut changed_files: Vec<(String, &'static str, &'static str)> = Vec::new();
                for sample in status.changed_samples.iter() {
                    let p = sample.path.as_str();
                    let leaf = p.rsplit('/').next().unwrap_or(p);
                    let icon = crate::svg_icons::icon_name_for_file(leaf, Some("file"));
                    let tag = match sample.kind {
                        ChangeKind::Modified => "M",
                        ChangeKind::Untracked => "?",
                        ChangeKind::Deleted => "D",
                        ChangeKind::Staged => "+",
                        ChangeKind::Conflicted => "!",
                    };
                    changed_files.push((p.to_owned(), icon, tag));
                    if let Some(slot) = changed_icons.iter_mut().find(|(n, _)| *n == icon) {
                        slot.1 += 1;
                    } else {
                        changed_icons.push((icon, 1));
                    }
                }
                changed_icons.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

                let stashes = read_git_stashes(&head.common_dir, 8);
                let branches = read_local_branches(&head.common_dir, &branch, 16);

                let dirty = c.conflicted > 0
                    || staged > 0
                    || c.modified > 0
                    || c.deleted > 0
                    || c.typechanged > 0
                    || c.untracked > 0;

                return Some(GitStatusSummary {
                    branch,
                    detached,
                    tag,
                    state_label,
                    status_badge,
                    changed_icons,
                    changed_files,
                    stashes,
                    branches,
                    dirty,
                    conflicted: c.conflicted,
                    staged,
                    modified: c.modified,
                    deleted: c.deleted,
                    untracked: c.untracked,
                    stashed: c.stashed,
                    ahead,
                    behind,
                });
            }
            Ok(Step::Need(want)) => {
                let got = execute_want(&head, want, alloc);
                if machine.feed(got).is_err() {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    None
}

/// Read local git branch names for `cwd` with zero subprocesses (for prompt popover & `git` tab completion).
pub fn read_local_branches_for_cwd(cwd: &str, limit: usize) -> Vec<String> {
    let Some(head) = read_git_head(cwd) else {
        return Vec::new();
    };
    read_local_branches(&head.common_dir, &head.label, limit)
}

fn read_git_stashes(common_dir: &Path, limit: usize) -> Vec<String> {
    let Ok(raw) = fs::read_to_string(common_dir.join("logs/refs/stash")) else {
        return Vec::new();
    };
    raw.lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(limit)
        .map(|line| {
            line.split_once('\t')
                .map(|(_, msg)| msg.trim().to_owned())
                .unwrap_or_else(|| line.trim().to_owned())
        })
        .collect()
}

fn read_local_branches(common_dir: &Path, current: &str, limit: usize) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    fn walk_heads(dir: &Path, prefix: &str, out: &mut Vec<String>, depth: usize) {
        if depth > 6 || out.len() >= 128 {
            return;
        }
        let Ok(rd) = fs::read_dir(dir) else { return };
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            let full = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            if let Ok(ft) = ent.file_type() {
                if ft.is_dir() {
                    walk_heads(&ent.path(), &full, out, depth + 1);
                } else {
                    out.push(full);
                }
            }
        }
    }
    walk_heads(&common_dir.join("refs/heads"), "", &mut names, 0);
    if let Ok(packed) = fs::read_to_string(common_dir.join("packed-refs")) {
        for line in packed.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            if let Some((_, refname)) = line.split_once(' ')
                && let Some(branch) = refname.strip_prefix("refs/heads/")
            {
                names.push(branch.to_owned());
            }
        }
    }
    names.sort();
    names.dedup();
    // Place current branch first, then `main`/`master`, then alphabetical.
    names.sort_by(|a, b| {
        let rank = |s: &str| {
            if s == current {
                0
            } else if s == "main" || s == "master" {
                1
            } else {
                2
            }
        };
        rank(a).cmp(&rank(b)).then_with(|| a.cmp(b))
    });
    names.truncate(limit);
    names
}

fn resolve_scoped(head: &GitHead, sp: &ScopedPath<'_>) -> PathBuf {
    let base = match sp.scope {
        PathScope::Worktree => &head.work_tree,
        PathScope::GitDir => &head.git_dir,
        PathScope::CommonDir => &head.common_dir,
    };
    let rel = sp.path.as_str();
    if rel.is_empty() {
        base.clone()
    } else {
        base.join(rel)
    }
}

fn meta_to_file_stat(meta: &fs::Metadata) -> FileStat {
    let ft = meta.file_type();
    let kind = if ft.is_symlink() {
        FileKind::Symlink
    } else if ft.is_dir() {
        FileKind::Dir
    } else if ft.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    };
    #[cfg(unix)]
    {
        FileStat {
            kind,
            size: meta.len(),
            mtime_sec: meta.mtime(),
            mtime_nsec: meta.mtime_nsec().clamp(0, 999_999_999) as u32,
            ctime_sec: meta.ctime(),
            ctime_nsec: meta.ctime_nsec().clamp(0, 999_999_999) as u32,
            ino: meta.ino(),
            dev: meta.dev(),
            mode: meta.mode(),
        }
    }
    #[cfg(not(unix))]
    {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .unwrap_or_default();
        FileStat {
            kind,
            size: meta.len(),
            mtime_sec: mtime.as_secs() as i64,
            mtime_nsec: mtime.subsec_nanos(),
            ctime_sec: mtime.as_secs() as i64,
            ctime_nsec: mtime.subsec_nanos(),
            ino: 0,
            dev: 0,
            mode: 0o100644,
        }
    }
}

fn stat_path(p: &Path) -> FileStat {
    match fs::symlink_metadata(p) {
        Ok(meta) => meta_to_file_stat(&meta),
        Err(_) => FileStat::GONE,
    }
}

fn read_to_avec<'a>(p: &Path, alloc: Alloc<'a>) -> Option<AVec<'a, u8>> {
    let data = fs::read(p).ok()?;
    let mut out = OwnVec::with_capacity_in(data.len(), alloc);
    out.extend_from_slice(&data);
    Some(out)
}

fn execute_want<'a>(head: &GitHead, want: Want<'a>, alloc: Alloc<'a>) -> Got<'a> {
    match want {
        Want::ReadFiles(paths) => {
            let mut out = OwnVec::with_capacity_in(paths.len(), alloc);
            for sp in paths.iter() {
                let full = resolve_scoped(head, sp);
                out.push(read_to_avec(&full, alloc));
            }
            Got::Files(out)
        }
        Want::ReadWithStat(paths) => {
            let mut out = OwnVec::with_capacity_in(paths.len(), alloc);
            for sp in paths.iter() {
                let full = resolve_scoped(head, sp);
                let stat = fs::symlink_metadata(&full).ok().map(|m| meta_to_file_stat(&m));
                let bytes = if stat.is_some() { read_to_avec(&full, alloc) } else { None };
                out.push(FileData { bytes, stat });
            }
            Got::FilesWithStat(out)
        }
        Want::ListDirs(paths) => {
            let mut out = OwnVec::with_capacity_in(paths.len(), alloc);
            for sp in paths.iter() {
                let full = resolve_scoped(head, sp);
                let listing = match fs::read_dir(&full) {
                    Ok(rd) => {
                        let mut entries: Vec<(String, FileStat)> = Vec::new();
                        for ent in rd.flatten() {
                            let name = ent.file_name().to_string_lossy().into_owned();
                            let ep = full.join(&name);
                            let st = stat_path(&ep);
                            if st.kind != FileKind::Gone {
                                entries.push((name, st));
                            }
                        }
                        // `blaze-git::DirListing` requires entries sorted by name bytes.
                        entries.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                        let name_bytes: usize = entries.iter().map(|(n, _)| n.len()).sum();
                        let mut dl = DirListing::with_capacity(entries.len(), name_bytes, alloc);
                        for (name, st) in entries {
                            dl.push(&name, st);
                        }
                        Some(dl)
                    }
                    Err(_) => None,
                };
                out.push(listing);
            }
            Got::Dirs(out)
        }
        Want::StatPaths(paths) => {
            let mut out = OwnVec::with_capacity_in(paths.len(), alloc);
            for sp in paths.iter() {
                let full = resolve_scoped(head, sp);
                out.push(StatEntry { stat: stat_path(&full) });
            }
            Got::Stats(out)
        }
        Want::ReadLinks(paths) => {
            let mut out = OwnVec::with_capacity_in(paths.len(), alloc);
            for sp in paths.iter() {
                let full = resolve_scoped(head, sp);
                let bytes = fs::read_link(&full).ok().map(|target| {
                    let s = target.to_string_lossy();
                    let mut v = OwnVec::with_capacity_in(s.len(), alloc);
                    v.extend_from_slice(s.as_bytes());
                    v
                });
                out.push(bytes);
            }
            Got::Links(out)
        }
        Want::ReadRanges(ranges) => {
            let mut out = OwnVec::with_capacity_in(ranges.len(), alloc);
            for rr in ranges.iter() {
                let sp = ScopedPath::new(rr.scope, rr.path.as_str(), alloc);
                let full = resolve_scoped(head, &sp);
                let bytes = (|| -> Option<AVec<'a, u8>> {
                    let mut f = File::open(&full).ok()?;
                    f.seek(SeekFrom::Start(rr.offset)).ok()?;
                    let want_len = (rr.len as usize).min(64 << 20);
                    let mut buf = vec![0u8; want_len];
                    let mut read_total = 0usize;
                    while read_total < want_len {
                        let n = f.read(&mut buf[read_total..]).ok()?;
                        if n == 0 {
                            break;
                        }
                        read_total += n;
                    }
                    let mut v = OwnVec::with_capacity_in(read_total, alloc);
                    v.extend_from_slice(&buf[..read_total]);
                    Some(v)
                })();
                out.push(blaze_git::machine::RangeData { bytes });
            }
            Got::Ranges(out)
        }
    }
}

// =============================================================================
// Single coalesced background status worker
// =============================================================================

struct WorkerSlot {
    pending_cwd: Option<String>,
    shutdown: bool,
}

/// Single background thread running `blaze-git`'s Starship status engine without ever
/// blocking a keystroke (bounded 1-slot coalescing mailbox + single atomic poll).
pub struct GitStatusWorker {
    req: Arc<(Mutex<WorkerSlot>, Condvar)>,
    result: Arc<Mutex<Option<(String, Option<GitStatusSummary>)>>>,
    has_update: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl GitStatusWorker {
    pub fn spawn() -> Self {
        let req = Arc::new((
            Mutex::new(WorkerSlot { pending_cwd: None, shutdown: false }),
            Condvar::new(),
        ));
        let result: Arc<Mutex<Option<(String, Option<GitStatusSummary>)>>> =
            Arc::new(Mutex::new(None));
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
                        let summary = query_blaze_git_status(&cwd);
                        if let Ok(mut out) = res_bg.lock() {
                            *out = Some((cwd, summary));
                            flag_bg.store(true, Ordering::Release);
                        }
                    }
                }
            })
            .ok();

        Self { req, result, has_update, handle }
    }

    /// Schedule a background `blaze-git` refresh for `cwd`. Coalesces pending requests.
    pub fn request(&self, cwd: &str) {
        let (lock, cvar) = &*self.req;
        if let Ok(mut slot) = lock.lock() {
            slot.pending_cwd = Some(cwd.to_owned());
            cvar.notify_one();
        }
    }

    /// Non-blocking poll for a completed status result (single atomic load when idle).
    pub fn poll(&self) -> Option<(String, Option<GitStatusSummary>)> {
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

/// Native `blaze-git` rich command overrides for `git status` / `git st`, `git branch`, and
/// `git stash list` (bypassed automatically if raw-text flags like `--porcelain` or `^git`
/// are passed). Returns structured typed `blaze_sh::Value` tables with IntelliJ SVG file icons,
/// proportional byte size bars, hover file previews, and clickable header sorting!
pub fn try_native_git_command(src: &str, cwd: &str) -> Option<blaze_sh::Value<'static>> {
    let trimmed = src.trim();
    // Respect `^git ...` explicit external escape hatch and pipelines/redirections handled by blaze-sh.
    if trimmed.starts_with('^') || trimmed.contains('|') || trimmed.contains(';') || trimmed.contains('>') {
        return None;
    }
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.first().copied() != Some("git") {
        return None;
    }
    let sub = parts.get(1).copied()?;
    let rest = &parts[2..];
    let alloc = Alloc::global();

    match sub {
        "status" | "st" => {
            // Only intercept plain `git status` / `git st` (let `--porcelain`, `-s`, `-u`, etc. hit git).
            if !rest.is_empty() {
                return None;
            }
            let head = read_git_head(cwd)?;
            let summary = query_blaze_git_status(cwd)?;
            if summary.changed_files.is_empty() && !summary.dirty {
                let mut rec = blaze_sh::Record::new(alloc);
                rec.insert_str("branch", blaze_sh::Value::string(&summary.branch, alloc));
                rec.insert_str("state", blaze_sh::Value::string("clean", alloc));
                rec.insert_str("ahead", blaze_sh::Value::Int(i64::from(summary.ahead)));
                rec.insert_str("behind", blaze_sh::Value::Int(i64::from(summary.behind)));
                rec.insert_str("stashes", blaze_sh::Value::Int(i64::from(summary.stashed)));
                return Some(blaze_sh::Value::Record(rec));
            }
            let mut tbl = blaze_sh::Table::from_col_names(&["status", "name", "size", "modified"], alloc);
            tbl.set_col_display("status", blaze_sh::ColDisplay::Status);
            tbl.set_col_display("name", blaze_sh::ColDisplay::FileEntry);
            tbl.set_col_display("size", blaze_sh::ColDisplay::FileSize);
            tbl.set_col_display("modified", blaze_sh::ColDisplay::HumanTime);
            tbl.default_grid = false;

            for (rel_path, _icon, tag) in &summary.changed_files {
                let status_label = match *tag {
                    "+" => "staged",
                    "M" => "modified",
                    "D" => "deleted",
                    "?" => "untracked",
                    "!" => "conflict",
                    other => other,
                };
                let abs = head.work_tree.join(rel_path);
                let (size_val, mod_val) = match fs::metadata(&abs) {
                    Ok(meta) => {
                        let sz = blaze_sh::Value::Size(blaze_sh::Bytes(meta.len() as i64));
                        let dt = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| {
                                blaze_sh::Value::Date(blaze_sh::DateTime {
                                    utc_ns: d.as_nanos() as i64,
                                    offset_secs: 0,
                                })
                            })
                            .unwrap_or(blaze_sh::Value::Nothing);
                        (sz, dt)
                    }
                    Err(_) => (blaze_sh::Value::Nothing, blaze_sh::Value::Nothing),
                };
                tbl.push_row_values(&[
                    blaze_sh::Value::string(status_label, alloc),
                    blaze_sh::Value::string(rel_path, alloc),
                    size_val,
                    mod_val,
                ]);
            }
            Some(blaze_sh::Value::Table(tbl))
        }
        "branch" if rest.is_empty() => {
            let head = read_git_head(cwd)?;
            let branches = read_local_branches(&head.common_dir, &head.label, 64);
            let mut tbl = blaze_sh::Table::from_col_names(&["branch", "status"], alloc);
            tbl.set_col_display("status", blaze_sh::ColDisplay::Status);
            for br in branches {
                let is_cur = br == head.label;
                tbl.push_row_values(&[
                    blaze_sh::Value::string(&br, alloc),
                    blaze_sh::Value::string(if is_cur { "current" } else { "local" }, alloc),
                ]);
            }
            Some(blaze_sh::Value::Table(tbl))
        }
        "stash" if rest == ["list"] => {
            let head = read_git_head(cwd)?;
            let stashes = read_git_stashes(&head.common_dir, 32);
            let mut tbl = blaze_sh::Table::from_col_names(&["stash", "message"], alloc);
            for (i, msg) in stashes.iter().enumerate() {
                tbl.push_row_values(&[
                    blaze_sh::Value::string(&format!("stash@{{{i}}}"), alloc),
                    blaze_sh::Value::string(msg, alloc),
                ]);
            }
            Some(blaze_sh::Value::Table(tbl))
        }
        _ => None,
    }
}
