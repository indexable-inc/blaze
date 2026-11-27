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

/// One file's diff stat inside a git stash entry (parsed from `git stash show --numstat`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitStashFileStat {
    pub path: String,
    pub icon: &'static str,
    pub added: u32,
    pub removed: u32,
    pub is_binary: bool,
}

/// Rich git stash entry shown in the interactive stash popover (`branch`, `summary`, files, `+`/`-`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitStashEntry {
    pub index: u32,
    pub branch: String,
    pub summary: String,
    pub files: Vec<GitStashFileStat>,
    pub total_files: u32,
    pub total_added: u32,
    pub total_removed: u32,
}

/// Rich per-file metadata for uncommitted changes shown in the prompt hover popover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFileDetail {
    pub path: String,
    pub icon: &'static str,
    /// Short single-char status tag (`"+"`, `"M"`, `"D"`, `"?"`, `"!"`).
    pub tag: &'static str,
    /// Human-readable status word (`"staged"`, `"modified"`, `"deleted"`, `"untracked"`, `"conflict"`).
    pub status_label: &'static str,
    /// Semantic file type from `blaze-sniff` (e.g. `"Lockfile"`, `"Env"`, `"Rust"`, `"TOML"`).
    pub kind_label: &'static str,
    /// On-disk byte size when the file exists.
    pub size_bytes: Option<u64>,
    /// Diff line additions/deletions for tracked modified/staged files (`+N -M`), when available.
    pub added_lines: Option<u32>,
    pub deleted_lines: Option<u32>,
}

/// Compact commit entry with diff churn (`+added` / `-deleted`) for the visual branch commit graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchCommit {
    pub short_sha: String,
    pub time_secs: i64,
    pub subject: String,
    pub added_lines: u32,
    pub deleted_lines: u32,
    pub files_changed: u32,
}

/// Relation of the current HEAD to the repository's base branch (`main`/`master`/`origin/...`),
/// including ahead/behind commit counts, total branch diff churn, per-commit graph entries,
/// and common ancestor (`git merge-base`) commit summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchRelation {
    /// Short display name of the comparison ref (e.g. `"main"`, `"master"`, `"origin/main"`).
    pub base_ref: String,
    /// Commits reachable from `HEAD` but not from `base_ref`.
    pub ahead: u32,
    /// Commits reachable from `base_ref` but not from `HEAD`.
    pub behind: u32,
    /// Aggregate diff stats across `base_ref...HEAD` (`git diff --shortstat`).
    pub total_added: u32,
    pub total_deleted: u32,
    pub total_files: u32,
    /// Abbreviated SHA of the common ancestor (`git merge-base <base_ref> HEAD`).
    pub merge_base_short: String,
    /// Unix timestamp (seconds) of the common ancestor commit.
    pub merge_base_time_secs: i64,
    /// Single-line subject of the common ancestor commit.
    pub merge_base_subject: String,
    /// Up to 5 newest commits on `HEAD` ahead of `base_ref` (newest first).
    pub ahead_commits: Vec<BranchCommit>,
    /// Up to 3 newest commits on `base_ref` behind `HEAD` (newest first).
    pub behind_commits: Vec<BranchCommit>,
    /// Up to 3 newest commits at `HEAD` when `ahead == 0 && behind == 0` (up to date).
    pub recent_commits: Vec<BranchCommit>,
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
    /// Rich per-file metadata for uncommitted files (status, sniffed kind, size, diff stats), max 32.
    pub changed_details: Vec<ChangedFileDetail>,
    /// Recent stash descriptions parsed directly from `logs/refs/stash` (newest first, max 8).
    pub stashes: Vec<String>,
    /// Rich stash entries with per-file `+`/`-` diff stats (cached by stash reflog mtime).
    pub stash_entries: Vec<GitStashEntry>,
    /// Local git branches read directly from `refs/heads` + `packed-refs` (current first, max 16).
    pub branches: Vec<String>,
    /// Relation of current HEAD to the repo's default/base branch (`main`/`master`/`origin/...`)
    /// and their common ancestor commit, cached by `(head_oid, base_oid)`.
    pub branch_relation: Option<BranchRelation>,
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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PromptInfo {
    /// Machine hostname (local laptop or remote server over SSH/BSH1).
    pub host: String,
    /// True when this session is connected to a remote server rather than the local Mac.
    pub is_remote: bool,
    /// Optional custom host symbol/emoji from `~/.config/blaze-shell/config.toml` (e.g. `"🦑"`).
    pub host_symbol: Option<String>,
    /// Optional custom host label (`Some("")` hides hostname for symbol-only display).
    pub host_label_override: Option<String>,
    pub cwd: String,
    /// Full `~`-abbreviated path (used in tooltips / titlebar).
    pub cwd_display: String,
    /// Concise Starship directory label (repo-rooted inside git, e.g. `blaze-shell-1124`).
    pub dir_short: String,
    /// Optional custom project symbol/emoji from `~/.config/blaze-shell/config.toml` or live
    /// `TermServer` inode binding (e.g. `"👋🌎"` or a registered custom symbol name like `"rocket"`).
    pub project_symbol: Option<String>,
    /// Optional custom project display label (`Some("")` hides dir name for symbol-only display).
    pub project_label_override: Option<String>,
    /// Optional custom accent color from live `TermServer` inode binding.
    pub project_color: Option<[f32; 4]>,
    /// Optional short badge pill from live `TermServer` inode binding (e.g. `"prod"`).
    pub project_badge: Option<String>,
    /// Canonical entity id when matched from `TermServer` (e.g. `"file:16777234:9812345"`).
    pub project_entity_id: Option<String>,
    pub git_branch: Option<String>,
    pub git_detached: bool,
    pub git_status: Option<GitStatusSummary>,
    /// True when a `Cargo.toml` lives in cwd or repo root.
    pub in_rust_project: bool,
    /// Detected project toolchain/ecosystem IntelliJ SVG icons (e.g. `rust_dark`, `nix_dark`, `typeScript_dark`).
    pub project_icons: Vec<&'static str>,
    /// Detailed project markers `(icon_name, toolchain_name, marker_file)` for hover inspection.
    pub project_details: Vec<(&'static str, &'static str, &'static str)>,
    /// True when `[prompt] show_toolchains = false` in `~/.config/blaze-shell/config.toml`.
    pub hide_toolchains: bool,
    /// Live processes running in `cwd`, subfolders, and descendants (updated asynchronously).
    pub folder_procs: crate::sysmon_driver::FolderProcSnapshot,
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
        let host = crate::zoxide::detect_local_hostname();
        let mut info = Self {
            host,
            is_remote: false,
            host_symbol: None,
            host_label_override: None,
            cwd: cwd.to_owned(),
            cwd_display,
            dir_short,
            project_symbol: None,
            project_label_override: None,
            project_color: None,
            project_badge: None,
            project_entity_id: None,
            git_branch,
            git_detached,
            git_status: None,
            in_rust_project,
            project_icons,
            project_details,
            hide_toolchains: false,
            folder_procs: crate::sysmon_driver::FolderProcSnapshot::default(),
            last_exit,
            last_duration_ms,
        };
        let cfg = crate::config::load_cached(home);
        info.apply_user_config(&cfg, home);
        info
    }

    /// Apply host & project symbol/label overrides from `~/.config/blaze-shell/config.toml`.
    pub fn apply_user_config(&mut self, cfg: &crate::config::UserConfig<'_>, home: &str) {
        if let Some(host_rule) = cfg.resolve_host(&self.host, self.is_remote) {
            self.host_symbol = host_rule.symbol_str().map(str::to_owned);
            self.host_label_override = host_rule.label_str().map(str::to_owned);
        } else {
            self.host_symbol = None;
            self.host_label_override = None;
        }
        if let Some(proj) = cfg.resolve_project(&self.cwd, home) {
            self.project_symbol = proj.symbol_str().map(str::to_owned);
            self.project_label_override = proj.label_str().map(str::to_owned);
        } else {
            self.project_symbol = None;
            self.project_label_override = None;
        }
        self.hide_toolchains = !cfg.show_toolchains;
        self.project_color = None;
        self.project_badge = None;
        self.project_entity_id = None;

        // Live `TermServer` entity bindings (inode-keyed for folders so renames/moves preserve
        // metadata, and hostname-keyed for hosts) override static config.toml entries!
        if let Some(host_ent) = crate::term_server::lookup_global_host_entity(&self.host) {
            if let Some(sym) = host_ent.symbol {
                self.host_symbol = Some(sym);
            }
            if let Some(lbl) = host_ent.display_name {
                self.host_label_override = Some(lbl);
            }
        }
        if let Some(dir_ent) =
            crate::term_server::lookup_global_dir_entity_with_ancestors(Path::new(&self.cwd))
        {
            if let Some(sym) = dir_ent.symbol {
                self.project_symbol = Some(sym);
            }
            if let Some(lbl) = dir_ent.display_name {
                self.project_label_override = Some(lbl);
            }
            if let Some([r, g, b, a]) = dir_ent.color {
                self.project_color = Some([
                    f32::from(r) / 255.0,
                    f32::from(g) / 255.0,
                    f32::from(b) / 255.0,
                    f32::from(a) / 255.0,
                ]);
            }
            self.project_badge = dir_ent.badge;
            self.project_entity_id = Some(format!("{}:{}", dir_ent.kind.as_str(), dir_ent.id));
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
    if abbr.starts_with("~/") {
        // `parts[0]` is `"~"`, so `parts.len() <= 3` (e.g. `~/Downloads/foo`) has ZERO omitted
        // segments — never emit a spurious `~/…/` ellipsis!
        if parts.len() <= 3 {
            abbr
        } else {
            format!("~/…/{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
        }
    } else if parts.len() <= 2 {
        abbr
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
                const MAX_CHANGED_DETAILS: usize = 32;
                let mut changed_icons: Vec<(&'static str, u32)> = Vec::new();
                let mut changed_files: Vec<(String, &'static str, &'static str)> = Vec::new();
                let mut changed_details: Vec<ChangedFileDetail> = Vec::new();
                let mut tracked_diff_paths: Vec<String> = Vec::new();
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
                    if changed_details.len() < MAX_CHANGED_DETAILS {
                        let abs = head.work_tree.join(p);
                        let size_bytes = fs::metadata(&abs)
                            .ok()
                            .filter(|m| m.is_file())
                            .map(|m| m.len());
                        let kind_label = classify_changed_file_kind(&abs, leaf);
                        if tag != "?" {
                            tracked_diff_paths.push(p.to_owned());
                        }
                        changed_details.push(ChangedFileDetail {
                            path: p.to_owned(),
                            icon,
                            tag,
                            status_label: status_label_for_tag(tag),
                            kind_label,
                            size_bytes,
                            added_lines: None,
                            deleted_lines: None,
                        });
                    }
                }
                changed_icons.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

                if !tracked_diff_paths.is_empty() {
                    let diff_stats = read_git_diff_numstat(&head.work_tree, &tracked_diff_paths);
                    for detail in &mut changed_details {
                        if detail.tag != "?"
                            && let Some(&(added, deleted)) = diff_stats.get(detail.path.as_str())
                        {
                            detail.added_lines = Some(added);
                            detail.deleted_lines = Some(deleted);
                        }
                    }
                }

                let stashes = read_git_stashes(&head.common_dir, 8);
                let stash_entries = if stashes.is_empty() {
                    Vec::new()
                } else {
                    read_git_stash_entries(&head.common_dir, &head.work_tree, &stashes, 6)
                };
                let branches = read_local_branches(&head.common_dir, &branch, 16);
                let branch_relation = read_branch_relation(&head, &branch, detached);

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
                    changed_details,
                    stashes,
                    stash_entries,
                    branches,
                    branch_relation,
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

/// Map a short status tag (`"+"`, `"M"`, `"D"`, `"?"`, `"!"`) to its human-readable status word.
fn status_label_for_tag(tag: &'static str) -> &'static str {
    match tag {
        "+" => "staged",
        "M" => "modified",
        "D" => "deleted",
        "?" => "untracked",
        "!" => "conflict",
        other => other,
    }
}

/// Classify a changed working-tree entry using `blaze-sniff` (reading up to 512 header bytes
/// when the file exists on disk so magic headers & shebangs are detected).
fn classify_changed_file_kind(abs: &Path, leaf: &str) -> &'static str {
    let sym_meta = fs::symlink_metadata(abs).ok();
    if let Some(ref m) = sym_meta {
        if m.file_type().is_symlink() {
            return blaze_sniff::sniff_symlink().label;
        }
        if m.is_dir() {
            return blaze_sniff::sniff_dir(leaf, blaze_sniff::DirMarkers::default()).label;
        }
    }
    let mut head_buf = [0u8; blaze_sniff::SNIFF_HEAD_BYTES];
    let n = if sym_meta.as_ref().is_some_and(|m| m.is_file()) {
        File::open(abs)
            .and_then(|mut f| f.read(&mut head_buf))
            .unwrap_or(0)
    } else {
        0
    };
    let res = blaze_sniff::sniff_file(leaf, &head_buf[..n]);
    if matches!(res.label, "Plain text" | "Empty") {
        if let Some((_, ext)) = leaf.rsplit_once('.')
            && ext.eq_ignore_ascii_case("env")
        {
            return "Env";
        }
        if sym_meta.is_none() && res.label == "Empty" {
            return "File";
        }
    }
    res.label
}

/// Parse `git diff --numstat` lines (`<added>\t<deleted>\t<path>`) into `path -> (added, deleted)`
/// via the pure `#![no_std]` parser in `blaze-git-meta`.
fn parse_git_diff_numstat(stdout: &str) -> std::collections::HashMap<String, (u32, u32)> {
    blaze_git_meta::parse_git_diff_numstat_in(stdout, Alloc::global())
        .into_iter()
        .map(|(path, added, deleted)| (path.as_str().to_owned(), (added, deleted)))
        .collect()
}

/// Run one fast bounded `git -C <work_tree> diff --numstat HEAD` for up to 32 tracked paths.
fn read_git_diff_numstat(
    work_tree: &Path,
    tracked_paths: &[String],
) -> std::collections::HashMap<String, (u32, u32)> {
    use std::process::{Command, Stdio};

    if tracked_paths.is_empty() {
        return std::collections::HashMap::new();
    }
    let mut cmd = Command::new("git");
    cmd.arg("--literal-pathspecs")
        .arg("-C")
        .arg(work_tree)
        .args(["-c", "core.quotepath=false", "diff", "--numstat", "--no-renames", "HEAD", "--"]);
    for p in tracked_paths.iter().take(32) {
        cmd.arg(p);
    }
    let Ok(out) = cmd
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return std::collections::HashMap::new();
    };
    if !out.status.success() {
        return std::collections::HashMap::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_git_diff_numstat(&text)
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

/// Split `"WIP on main: c54fc93e6 Merge..."` or `"On main: pre-pull-backup"` into `(branch, summary)`.
fn parse_stash_header(raw: &str) -> (String, String) {
    let (br, msg) = blaze_git_meta::parse_stash_header(raw);
    (br.to_owned(), msg.to_owned())
}

/// Read rich per-stash file diff stats (`+added` / `-removed`), cached by `(common_dir, reflog mtime, len)`.
fn read_git_stash_entries(
    common_dir: &Path,
    work_tree: &Path,
    stashes: &[String],
    limit: usize,
) -> Vec<GitStashEntry> {
    use std::collections::HashMap;
    use std::process::{Command, Stdio};
    use std::sync::{Mutex, OnceLock};
    use std::time::UNIX_EPOCH;

    struct CachedStashes {
        mtime_ns: u128,
        len: u64,
        entries: Vec<GitStashEntry>,
    }
    static STASH_CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedStashes>>> = OnceLock::new();

    let reflog = common_dir.join("logs/refs/stash");
    let (mtime_ns, len) = fs::metadata(&reflog)
        .ok()
        .map(|m| {
            let mt = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            (mt, m.len())
        })
        .unwrap_or((0, 0));

    let map = STASH_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = map.lock()
        && let Some(hit) = guard.get(common_dir)
        && hit.mtime_ns == mtime_ns
        && hit.len == len
    {
        return hit.entries.clone();
    }

    let mut entries = Vec::new();
    for (idx, raw_msg) in stashes.iter().take(limit).enumerate() {
        let (branch, summary) = parse_stash_header(raw_msg);
        let stash_ref = format!("stash@{{{idx}}}");
        let mut files: Vec<GitStashFileStat> = Vec::new();
        let mut total_files = 0u32;
        let mut total_added = 0u32;
        let mut total_removed = 0u32;

        if let Ok(out) = Command::new("git")
            .args(["stash", "show", "--numstat", "--include-untracked", &stash_ref])
            .current_dir(work_tree)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let mut parts = line.splitn(3, '\t');
                let (Some(a_s), Some(r_s), Some(path)) = (parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                if path.trim().is_empty() {
                    continue;
                }
                total_files += 1;
                let is_binary = a_s == "-" || r_s == "-";
                let added = a_s.parse::<u32>().unwrap_or(0);
                let removed = r_s.parse::<u32>().unwrap_or(0);
                total_added = total_added.saturating_add(added);
                total_removed = total_removed.saturating_add(removed);
                if files.len() < 8 {
                    let leaf = path.rsplit('/').next().unwrap_or(path);
                    let icon = crate::svg_icons::icon_name_for_file(leaf, Some("file"));
                    files.push(GitStashFileStat {
                        path: path.to_owned(),
                        icon,
                        added,
                        removed,
                        is_binary,
                    });
                }
            }
            // Sort by largest churn first so the most meaningful files show at the top.
            files.sort_by(|a, b| (b.added + b.removed).cmp(&(a.added + a.removed)));
        }

        entries.push(GitStashEntry {
            index: idx as u32,
            branch,
            summary,
            files,
            total_files,
            total_added,
            total_removed,
        });
    }

    if let Ok(mut guard) = map.lock() {
        if guard.len() > 16 {
            guard.clear();
        }
        guard.insert(
            common_dir.to_path_buf(),
            CachedStashes {
                mtime_ns,
                len,
                entries: entries.clone(),
            },
        );
    }
    entries
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

/// Resolve a git ref (such as `"HEAD"`, `"refs/heads/main"`, `"refs/remotes/origin/main"`)
/// directly from loose ref files and `packed-refs` with zero subprocesses.
fn resolve_git_ref_oid(git_dir: &Path, common_dir: &Path, refname: &str) -> Option<String> {
    let mut current = refname.to_owned();
    for _ in 0..5 {
        let loose = if current == "HEAD" {
            git_dir.join("HEAD")
        } else {
            common_dir.join(&current)
        };
        if let Ok(raw) = fs::read_to_string(&loose) {
            let line = raw.lines().next().unwrap_or("").trim();
            if let Some(target) = line.strip_prefix("ref:") {
                let next = target.trim();
                if next.is_empty() {
                    return None;
                }
                current = next.to_owned();
                continue;
            }
            if is_git_oid_hex(line) {
                return Some(line.to_owned());
            }
            return None;
        }
        // Loose ref absent: fall back to `packed-refs` in `common_dir`.
        let packed = fs::read_to_string(common_dir.join("packed-refs")).ok()?;
        for line in packed.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            if let Some((oid, name)) = line.split_once(' ')
                && name == current
                && is_git_oid_hex(oid)
            {
                return Some(oid.to_owned());
            }
        }
        return None;
    }
    None
}

#[inline]
fn is_git_oid_hex(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Read the remote default branch name from `refs/remotes/origin/HEAD` (e.g. `"main"` or `"master"`).
fn read_origin_default_branch(common_dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(common_dir.join("refs/remotes/origin/HEAD")).ok()?;
    let line = raw.lines().next()?.trim();
    let target = line.strip_prefix("ref:")?.trim();
    let name = target.strip_prefix("refs/remotes/origin/")?;
    if name.is_empty() { None } else { Some(name.to_owned()) }
}

/// Pick the best comparison base ref for `current_branch`, returning `(display_name, full_ref, oid)`.
///
/// - On a feature branch or detached HEAD: prefers the repo's default branch (`origin/HEAD` target,
///   then `main`, `origin/main`, `master`, `origin/master`).
/// - On the default branch itself (`main`/`master`): compares against `origin/<current>` when present.
fn pick_base_branch_ref(
    common_dir: &Path,
    current_branch: &str,
    detached: bool,
) -> Option<(String, String, String)> {
    let origin_def = read_origin_default_branch(common_dir);
    let on_default = !detached
        && (current_branch == "main"
            || current_branch == "master"
            || origin_def.as_deref() == Some(current_branch));

    let mut candidates: Vec<(String, String)> = Vec::with_capacity(8);
    if on_default {
        candidates.push((
            format!("origin/{current_branch}"),
            format!("refs/remotes/origin/{current_branch}"),
        ));
    } else {
        if let Some(def) = &origin_def {
            candidates.push((def.clone(), format!("refs/heads/{def}")));
            candidates.push((format!("origin/{def}"), format!("refs/remotes/origin/{def}")));
        }
        candidates.push(("main".to_owned(), "refs/heads/main".to_owned()));
        candidates.push(("origin/main".to_owned(), "refs/remotes/origin/main".to_owned()));
        candidates.push(("master".to_owned(), "refs/heads/master".to_owned()));
        candidates.push(("origin/master".to_owned(), "refs/remotes/origin/master".to_owned()));
    }

    let mut seen: Vec<String> = Vec::with_capacity(candidates.len());
    for (display, full_ref) in candidates {
        if (!detached && display == current_branch) || seen.contains(&full_ref) {
            continue;
        }
        seen.push(full_ref.clone());
        if let Some(oid) = resolve_git_ref_oid(common_dir, common_dir, &full_ref) {
            return Some((display, full_ref, oid));
        }
    }
    None
}

/// Compute ahead/behind divergence and common ancestor (`git merge-base`) against the repository's
/// base branch (`main`/`master`/`origin/...`), cached in-process by `(work_tree, head_oid, base_oid)`.
fn read_branch_relation(
    head: &GitHead,
    current_branch: &str,
    detached: bool,
) -> Option<BranchRelation> {
    use std::collections::HashMap;
    use std::process::{Command, Stdio};
    use std::sync::{Mutex, OnceLock};

    struct CachedBranchRel {
        head_oid: String,
        base_ref: String,
        base_oid: String,
        relation: Option<BranchRelation>,
    }
    static BRANCH_REL_CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedBranchRel>>> = OnceLock::new();

    let head_oid = resolve_git_ref_oid(&head.git_dir, &head.common_dir, "HEAD")?;
    let (base_ref, full_ref, base_oid) =
        pick_base_branch_ref(&head.common_dir, current_branch, detached)?;

    let cache = BRANCH_REL_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some(hit) = guard.get(&head.work_tree)
        && hit.head_oid == head_oid
        && hit.base_ref == base_ref
        && hit.base_oid == base_oid
    {
        return hit.relation.clone();
    }

    let (ahead, behind, merge_base_oid) = if head_oid == base_oid {
        (0u32, 0u32, Some(head_oid.clone()))
    } else {
        let range = format!("{full_ref}...HEAD");
        let count_out = Command::new("git")
            .arg("-C")
            .arg(&head.work_tree)
            .args(["rev-list", "--left-right", "--count", &range])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !count_out.status.success() {
            return None;
        }
        let count_str = String::from_utf8_lossy(&count_out.stdout);
        let mut parts = count_str.split_whitespace();
        let behind = parts.next()?.parse::<u32>().ok()?;
        let ahead = parts.next()?.parse::<u32>().ok()?;

        let mb_oid = Command::new("git")
            .arg("-C")
            .arg(&head.work_tree)
            .args(["merge-base", &full_ref, "HEAD"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|s| !s.is_empty());

        (ahead, behind, mb_oid)
    };

    let (merge_base_short, merge_base_time_secs, merge_base_subject) =
        if let Some(mb) = merge_base_oid {
            Command::new("git")
                .arg("-C")
                .arg(&head.work_tree)
                .args(["show", "-s", "--format=%h\x1f%ct\x1f%s", &mb])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| {
                    let s = String::from_utf8_lossy(&o.stdout);
                    let mut it = s.trim_end_matches(['\r', '\n']).splitn(3, '\x1f');
                    let short = it.next()?.trim().to_owned();
                    let ts = it.next()?.trim().parse::<i64>().unwrap_or(0);
                    let subj = it.next().unwrap_or("").trim().to_owned();
                    Some((short, ts, subj))
                })
                .unwrap_or_else(|| (mb.chars().take(7).collect(), 0, String::new()))
        } else {
            (String::new(), 0, String::new())
        };

    let (total_files, total_added, total_deleted) = if ahead > 0 {
        let range = format!("{full_ref}...HEAD");
        Command::new("git")
            .arg("-C")
            .arg(&head.work_tree)
            .args(["diff", "--shortstat", &range])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| parse_git_shortstat(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or((0, 0, 0))
    } else {
        (0, 0, 0)
    };

    let ahead_commits = if ahead > 0 {
        read_git_log_commits(&head.work_tree, &format!("{full_ref}..HEAD"), 5)
    } else {
        Vec::new()
    };
    let behind_commits = if behind > 0 {
        read_git_log_commits(&head.work_tree, &format!("HEAD..{full_ref}"), 3)
    } else {
        Vec::new()
    };
    let recent_commits = if ahead == 0 && behind == 0 {
        read_git_log_commits(&head.work_tree, "HEAD", 3)
    } else {
        Vec::new()
    };

    let rel = Some(BranchRelation {
        base_ref: base_ref.clone(),
        ahead,
        behind,
        total_added,
        total_deleted,
        total_files,
        merge_base_short,
        merge_base_time_secs,
        merge_base_subject,
        ahead_commits,
        behind_commits,
        recent_commits,
    });

    if let Ok(mut guard) = cache.lock() {
        if guard.len() > 16 {
            guard.clear();
        }
        guard.insert(
            head.work_tree.clone(),
            CachedBranchRel {
                head_oid,
                base_ref,
                base_oid,
                relation: rel.clone(),
            },
        );
    }
    rel
}

/// Parse one `git diff --shortstat` / `git log --shortstat` summary line into
/// `(files_changed, added_lines, deleted_lines)` via `blaze-git-meta`.
fn parse_git_shortstat(line: &str) -> (u32, u32, u32) {
    let stat = blaze_git_meta::parse_git_shortstat(line);
    (stat.files_changed, stat.added_lines, stat.deleted_lines)
}

/// Fetch up to `limit` commits for `rev_range` with compact `--shortstat` `+`/`-` churn.
fn read_git_log_commits(work_tree: &Path, rev_range: &str, limit: usize) -> Vec<BranchCommit> {
    use std::process::{Command, Stdio};

    let max_flag = format!("-n{limit}");
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(work_tree)
        .args([
            "log",
            &max_flag,
            "--shortstat",
            "--format=\x1e%h\x1f%ct%x1f%s",
            rev_range,
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    blaze_git_meta::parse_git_log_commits_in(&text, limit, Alloc::global())
        .into_iter()
        .map(|c| BranchCommit {
            short_sha: c.short_sha.as_str().to_owned(),
            time_secs: c.time_secs,
            subject: c.subject.as_str().to_owned(),
            added_lines: c.stat.added_lines,
            deleted_lines: c.stat.deleted_lines,
            files_changed: c.stat.files_changed,
        })
        .collect()
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
    let cmd = parts.first().copied()?;
    if cmd != "git" && cmd != "bgit" {
        return None;
    }
    let sub = parts.get(1).copied()?;
    // For `bgit`, intercept rich structured table commands (`status`, `log`, `worktree list`),
    // letting all other `bgit` subcommands run in-process via `host.rs::try_run_in_process_bgit`.
    if cmd == "bgit"
        && !matches!(sub, "status" | "st" | "log" | "worktree" | "wt")
    {
        return None;
    }
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
                let status_label = status_label_for_tag(*tag);
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
        "blame" if !rest.is_empty() => {
            // Let explicit porcelain / raw / help flags pass straight through to external `git`.
            if rest.iter().any(|a| {
                matches!(
                    *a,
                    "--porcelain" | "-p" | "--line-porcelain" | "--incremental" | "-h" | "--help"
                )
            }) {
                return None;
            }
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(cwd)
                .arg("blame")
                .arg("--porcelain")
                .args(rest)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let raw = String::from_utf8_lossy(&out.stdout);
            let lines = blaze_git_meta::parse_blame_porcelain_in(&raw, 2000, alloc);
            if lines.is_empty() {
                return None;
            }
            let mut tbl = blaze_sh::Table::from_col_names(
                &["line", "commit", "author", "modified", "code", "summary", "email", "sha"],
                alloc,
            );
            tbl.set_col_display("commit", blaze_sh::ColDisplay::GitCommit);
            tbl.set_col_display("author", blaze_sh::ColDisplay::GitAuthor);
            tbl.set_col_display("modified", blaze_sh::ColDisplay::HumanTime);
            tbl.hide_col_by_name("summary");
            tbl.hide_col_by_name("email");
            tbl.hide_col_by_name("sha");
            tbl.default_grid = false;

            let gh_repo = crate::git_meta_driver::github_repo_for_dir(Path::new(cwd));
            let mut warmed: std::collections::HashSet<String> = std::collections::HashSet::new();

            for bl in &lines {
                let email = bl.author_email.as_str();
                if !email.is_empty() && warmed.len() < 16 && warmed.insert(email.to_owned()) {
                    let _ = crate::git_meta_driver::request_avatar(
                        email,
                        Some(bl.full_sha.as_str()),
                        gh_repo.as_ref(),
                    );
                }
                let dt_val = if bl.author_time_secs > 0 {
                    blaze_sh::Value::Date(blaze_sh::DateTime {
                        utc_ns: bl.author_time_secs.saturating_mul(1_000_000_000),
                        offset_secs: 0,
                    })
                } else {
                    blaze_sh::Value::Nothing
                };
                tbl.push_row_values(&[
                    blaze_sh::Value::Int(i64::from(bl.line_no)),
                    blaze_sh::Value::string(bl.short_sha.as_str(), alloc),
                    blaze_sh::Value::string(bl.author_name.as_str(), alloc),
                    dt_val,
                    blaze_sh::Value::string(bl.code.as_str(), alloc),
                    blaze_sh::Value::string(bl.summary.as_str(), alloc),
                    blaze_sh::Value::string(email, alloc),
                    blaze_sh::Value::string(bl.full_sha.as_str(), alloc),
                ]);
            }
            Some(blaze_sh::Value::Table(tbl))
        }
        "log" => {
            // Intercept plain `git log` / `bgit log` (and optional `-n N` / `-N`) when backed by
            // an existing `.bgit` store -> rich interactive commit table with avatars!
            let mut limit = 30usize;
            match rest {
                [] => {}
                ["-n", n] => limit = n.parse().ok()?,
                [tok] => {
                    if let Some(d) = tok.strip_prefix("-n").or_else(|| tok.strip_prefix('-'))
                        && !d.is_empty()
                        && d.bytes().all(|b| b.is_ascii_digit())
                    {
                        limit = d.parse().ok()?;
                    } else {
                        return None;
                    }
                }
                _ => return None,
            }
            let entries = bgit::query_log_entries(Path::new(cwd), "HEAD", limit, alloc).ok()??;
            if entries.is_empty() {
                return None;
            }
            let mut tbl = blaze_sh::Table::from_col_names(
                &["commit", "author", "modified", "summary", "email", "sha"],
                alloc,
            );
            tbl.set_col_display("commit", blaze_sh::ColDisplay::GitCommit);
            tbl.set_col_display("author", blaze_sh::ColDisplay::GitAuthor);
            tbl.set_col_display("modified", blaze_sh::ColDisplay::HumanTime);
            tbl.hide_col_by_name("email");
            tbl.hide_col_by_name("sha");
            tbl.default_grid = false;

            let gh_repo = crate::git_meta_driver::github_repo_for_dir(Path::new(cwd));
            let mut warmed: std::collections::HashSet<String> = std::collections::HashSet::new();
            for e in &entries {
                if !e.author_email.is_empty()
                    && warmed.len() < 16
                    && warmed.insert(e.author_email.clone())
                {
                    let _ = crate::git_meta_driver::request_avatar(
                        &e.author_email,
                        Some(&e.oid),
                        gh_repo.as_ref(),
                    );
                }
                let dt_val = if e.timestamp_secs > 0 {
                    blaze_sh::Value::Date(blaze_sh::DateTime {
                        utc_ns: e.timestamp_secs.saturating_mul(1_000_000_000),
                        offset_secs: 0,
                    })
                } else {
                    blaze_sh::Value::Nothing
                };
                tbl.push_row_values(&[
                    blaze_sh::Value::string(&e.short_oid, alloc),
                    blaze_sh::Value::string(&e.author_name, alloc),
                    dt_val,
                    blaze_sh::Value::string(&e.subject, alloc),
                    blaze_sh::Value::string(&e.author_email, alloc),
                    blaze_sh::Value::string(&e.oid, alloc),
                ]);
            }
            Some(blaze_sh::Value::Table(tbl))
        }
        "worktree" | "wt" if matches!(rest, ["list"] | ["ls"]) => {
            let wts = bgit::query_worktrees(Path::new(cwd), alloc).ok()??;
            let mut tbl = blaze_sh::Table::from_col_names(
                &["path", "branch", "tracks", "commit", "mode", "policy"],
                alloc,
            );
            tbl.set_col_display("path", blaze_sh::ColDisplay::FileEntry);
            tbl.set_col_display("commit", blaze_sh::ColDisplay::GitCommit);
            tbl.default_grid = false;
            for wt in &wts {
                tbl.push_row_values(&[
                    blaze_sh::Value::string(&wt.path, alloc),
                    blaze_sh::Value::string(&wt.branch, alloc),
                    blaze_sh::Value::string(&wt.track_branch, alloc),
                    blaze_sh::Value::string(&wt.head_short, alloc),
                    blaze_sh::Value::string(if wt.is_vfs { "vfs" } else { "disk" }, alloc),
                    blaze_sh::Value::string(
                        if wt.auto_delete_on_merge { "auto-delete" } else { "keep" },
                        alloc,
                    ),
                ]);
            }
            Some(blaze_sh::Value::Table(tbl))
        }
        "branch" | "br"
            if matches!(
                rest,
                [] | ["-v"]
                    | ["-vv"]
                    | ["--verbose"]
                    | ["-a"]
                    | ["--all"]
                    | ["-r"]
                    | ["--remotes"]
                    | ["-av"]
                    | ["-va"]
                    | ["-rv"]
                    | ["-vr"]
            ) =>
        {
            let include_all = rest.iter().any(|&a| matches!(a, "-a" | "--all" | "-av" | "-va"));
            let remotes_only =
                rest.iter().any(|&a| matches!(a, "-r" | "--remotes" | "-rv" | "-vr"));
            let branches =
                bgit::query_branches(Path::new(cwd), include_all, remotes_only, alloc).ok()??;
            if branches.is_empty() {
                return None;
            }
            let mut tbl = blaze_sh::Table::from_col_names(
                &["active", "branch", "commit", "summary", "sha"],
                alloc,
            );
            tbl.set_col_display("commit", blaze_sh::ColDisplay::GitCommit);
            tbl.hide_col_by_name("sha");
            tbl.default_grid = false;
            for br in &branches {
                tbl.push_row_values(&[
                    blaze_sh::Value::string(if br.is_current { "*" } else { "" }, alloc),
                    blaze_sh::Value::string(&br.name, alloc),
                    blaze_sh::Value::string(&br.short_oid, alloc),
                    blaze_sh::Value::string(&br.subject, alloc),
                    blaze_sh::Value::string(&br.oid, alloc),
                ]);
            }
            Some(blaze_sh::Value::Table(tbl))
        }
        "diff" | "df"
            if matches!(
                rest,
                ["--stat"]
                    | ["--staged", "--stat"]
                    | ["--cached", "--stat"]
                    | ["--stat", "--staged"]
                    | ["--stat", "--cached"]
            ) =>
        {
            let cwd_path = Path::new(cwd);
            let _ = bgit::ensure_fsmonitor_watching(cwd_path);
            let staged = rest.iter().any(|&a| matches!(a, "--staged" | "--cached"));
            let rows = bgit::query_diff_stat(cwd_path, staged, alloc).ok()??;
            if rows.is_empty() {
                return None;
            }
            let mut tbl = blaze_sh::Table::from_col_names(
                &["status", "path", "added", "removed", "changes"],
                alloc,
            );
            tbl.set_col_display("path", blaze_sh::ColDisplay::FileEntry);
            tbl.default_grid = false;
            for r in &rows {
                let st_buf = [r.status as u8];
                let st_str = core::str::from_utf8(&st_buf).unwrap_or("M");
                let total = r.insertions + r.deletions;
                let scale = if total > 24 { 24.0 / total as f32 } else { 1.0 };
                let plus_n = ((r.insertions as f32 * scale).round() as usize).min(24);
                let minus_n = ((r.deletions as f32 * scale).round() as usize).min(24 - plus_n);
                let bar = format!("{}{}", "+".repeat(plus_n), "-".repeat(minus_n));
                tbl.push_row_values(&[
                    blaze_sh::Value::string(st_str, alloc),
                    blaze_sh::Value::string(&r.path, alloc),
                    blaze_sh::Value::Int(r.insertions as i64),
                    blaze_sh::Value::Int(r.deletions as i64),
                    blaze_sh::Value::string(&bar, alloc),
                ]);
            }
            Some(blaze_sh::Value::Table(tbl))
        }
        _ => None,
    }
}

/// Per-entry Git tracking / dirty status for `ls` grids and tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitFileStatus {
    Tracked,
    Modified,
    Staged,
    Untracked,
    Deleted,
    Conflicted,
    Ignored,
}

impl GitFileStatus {
    /// Short single-char status tag (`None` for clean tracked/ignored entries).
    pub const fn tag(self) -> Option<&'static str> {
        match self {
            Self::Conflicted => Some("!"),
            Self::Modified => Some("M"),
            Self::Staged => Some("+"),
            Self::Deleted => Some("D"),
            Self::Untracked => Some("?"),
            Self::Tracked | Self::Ignored => None,
        }
    }

    const fn severity(self) -> u8 {
        match self {
            Self::Conflicted => 5,
            Self::Modified => 4,
            Self::Staged => 3,
            Self::Deleted => 2,
            Self::Untracked => 1,
            Self::Tracked | Self::Ignored => 0,
        }
    }

    fn from_tag(tag: &str) -> Self {
        match tag {
            "!" => Self::Conflicted,
            "M" => Self::Modified,
            "+" => Self::Staged,
            "D" => Self::Deleted,
            "?" => Self::Untracked,
            _ => Self::Tracked,
        }
    }
}

struct CachedIndexPaths {
    mtime_ns: u128,
    len: u64,
    /// Lexicographically sorted worktree-relative tracked file paths from `.git/index`.
    paths: Arc<Vec<String>>,
}

fn load_cached_index_paths(git_dir: &Path) -> Option<Arc<Vec<String>>> {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    use std::time::UNIX_EPOCH;

    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedIndexPaths>>> = OnceLock::new();

    let index_path = git_dir.join("index");
    let meta = fs::metadata(&index_path).ok()?;
    let len = meta.len();
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());

    let map = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = map.lock()
        && let Some(hit) = guard.get(git_dir)
        && hit.mtime_ns == mtime_ns
        && hit.len == len
    {
        return Some(Arc::clone(&hit.paths));
    }

    let bytes = fs::read(&index_path).ok()?;
    let parsed = blaze_git::index::parse_index(&bytes, blaze_git::Limits::SHIPPED, Alloc::global(), NoPlant).ok()?;
    let mut paths = Vec::with_capacity(parsed.entries.len());
    for entry in parsed.entries.iter() {
        if let Some(p) = parsed.path_str(entry) {
            paths.push(p.to_owned());
        }
    }
    let arc = Arc::new(paths);
    if let Ok(mut guard) = map.lock() {
        if guard.len() > 16 {
            guard.clear();
        }
        guard.insert(git_dir.to_path_buf(), CachedIndexPaths { mtime_ns, len, paths: Arc::clone(&arc) });
    }
    Some(arc)
}

fn repo_relative_dir(work_tree: &Path, dir: &Path) -> Option<String> {
    let rel = dir.strip_prefix(work_tree).ok().or_else(|| {
        let cw = fs::canonicalize(work_tree).ok()?;
        let cd = fs::canonicalize(dir).ok()?;
        cd.strip_prefix(&cw).ok().map(Path::to_path_buf).and_then(|_| None)
    });
    if let Some(r) = rel {
        let s = r.to_string_lossy().replace('\\', "/");
        return Some(s.trim_matches('/').to_owned());
    }
    let cw = fs::canonicalize(work_tree).ok()?;
    let cd = fs::canonicalize(dir).ok()?;
    let r = cd.strip_prefix(&cw).ok()?;
    let s = r.to_string_lossy().replace('\\', "/");
    Some(s.trim_matches('/').to_owned())
}

fn paths_dir_range(paths: &[String], dir: &str) -> std::ops::Range<usize> {
    if dir.is_empty() {
        return 0..paths.len();
    }
    let db = dir.as_bytes();
    let start = paths.partition_point(|p| {
        let pb = p.as_bytes();
        let n = pb.len().min(db.len());
        match pb[..n].cmp(&db[..n]) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Equal if pb.len() <= db.len() => true,
            std::cmp::Ordering::Equal => pb[db.len()] < b'/',
        }
    });
    let end = start
        + paths[start..].partition_point(|p| {
            let pb = p.as_bytes();
            pb.len() > db.len() && pb.starts_with(db) && pb[db.len()] == b'/'
        });
    start..end
}

/// Return git-tracked subfolder paths strictly beneath `search_dir` (e.g. `src/main.rs`) for
/// lower-priority recursive fuzzy file completion inside git repositories (zero subprocesses!).
pub fn git_tracked_subpaths(search_dir: &Path, dirs_only: bool, max_results: usize) -> Option<Vec<String>> {
    let dir_str = search_dir.to_str()?;
    let head = read_git_head(dir_str)?;
    let paths = load_cached_index_paths(&head.git_dir)?;
    let rel_dir = repo_relative_dir(&head.work_tree, search_dir)?;
    let range = paths_dir_range(&paths, &rel_dir);
    let strip_len = if rel_dir.is_empty() { 0 } else { rel_dir.len() + 1 };

    let mut out = Vec::new();
    if dirs_only {
        let mut seen = std::collections::HashSet::new();
        for p in &paths[range] {
            let rel = &p[strip_len..];
            // Collect nested directories (`a/b/`, `a/b/c/`), skipping top-level dirs already from read_dir.
            let mut search = 0usize;
            while let Some(slash) = rel[search..].find('/') {
                let end = search + slash;
                let sub = &rel[..end];
                if sub.contains('/') && seen.insert(sub.to_owned()) {
                    out.push(format!("{sub}/"));
                    if out.len() >= max_results {
                        return Some(out);
                    }
                }
                search = end + 1;
            }
        }
    } else {
        for p in &paths[range] {
            let rel = &p[strip_len..];
            if rel.contains('/') {
                out.push(rel.to_owned());
                if out.len() >= max_results {
                    break;
                }
            }
        }
    }
    Some(out)
}

/// Classify every `ls` entry `(name, is_dir)` inside `dir` against the enclosing git repository's
/// `.git/index`, `.gitignore` stack, and working-tree status (zero subprocesses, cached).
/// Returns `None` when `dir` is not inside a git repository.
pub fn git_ls_entry_statuses(
    dir: &Path,
    entries: &[(String, bool)],
    hint: Option<&GitStatusSummary>,
) -> Option<Vec<GitFileStatus>> {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    let dir_str = dir.to_str()?;
    let head = read_git_head(dir_str)?;
    let rel_dir = repo_relative_dir(&head.work_tree, dir)?;
    let index_paths = load_cached_index_paths(&head.git_dir);

    // Use the block's captured GitStatusSummary when available; otherwise cache a quick query
    // for 750ms per worktree so 60-120fps layout frames never re-scan disk.
    let owned_summary: Option<GitStatusSummary> = if hint.is_some() {
        None
    } else {
        static STATUS_CACHE: OnceLock<Mutex<HashMap<PathBuf, (Instant, Option<GitStatusSummary>)>>> =
            OnceLock::new();
        let cache = STATUS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        let cached = cache.lock().ok().and_then(|g| {
            g.get(&head.work_tree)
                .filter(|(t, _)| t.elapsed() < Duration::from_millis(750))
                .map(|(_, s)| s.clone())
        });
        if let Some(hit) = cached {
            hit
        } else {
            let fresh = query_blaze_git_status(dir_str);
            if let Ok(mut g) = cache.lock() {
                if g.len() > 16 {
                    g.clear();
                }
                g.insert(head.work_tree.clone(), (Instant::now(), fresh.clone()));
            }
            fresh
        }
    };
    let status_ref = hint.or(owned_summary.as_ref());

    // Build IgnoreStack from global ignore, `.git/info/exclude`, and `.gitignore` chain down to `dir`.
    let alloc = Alloc::global();
    let mut ignore = blaze_git::ignore::IgnoreStack::new(blaze_git::Limits::SHIPPED, alloc);
    if let Ok(home) = std::env::var("HOME")
        && let Ok(bytes) = fs::read(Path::new(&home).join(".config/git/ignore"))
    {
        ignore.set_global_exclude(&bytes, NoPlant);
    }
    if let Ok(bytes) = fs::read(head.common_dir.join("info/exclude")) {
        ignore.set_info_exclude(&bytes, NoPlant);
    }
    if let Ok(bytes) = fs::read(head.work_tree.join(".gitignore")) {
        ignore.add_folder_gitignore("", &bytes, NoPlant);
    }
    if !rel_dir.is_empty() {
        let mut accum = String::new();
        for part in rel_dir.split('/') {
            if !accum.is_empty() {
                accum.push('/');
            }
            accum.push_str(part);
            if let Ok(bytes) = fs::read(head.work_tree.join(&accum).join(".gitignore")) {
                ignore.add_folder_gitignore(&accum, &bytes, NoPlant);
            }
        }
    }

    let changed = status_ref.map(|s| s.changed_files.as_slice()).unwrap_or(&[]);
    let mut out = Vec::with_capacity(entries.len());

    for (raw_name, is_dir) in entries {
        let name = raw_name.trim_end_matches('/');
        if name == ".git" {
            out.push(GitFileStatus::Ignored);
            continue;
        }
        let repo_rel = if rel_dir.is_empty() {
            name.to_owned()
        } else {
            format!("{rel_dir}/{name}")
        };

        // 1. Check dirty/staged/untracked working-tree changes (bubbling up onto parent dirs).
        let mut best_dirty: Option<GitFileStatus> = None;
        if *is_dir {
            let prefix = format!("{repo_rel}/");
            for (p, _, tag) in changed {
                if p == &repo_rel || p.starts_with(&prefix) {
                    let st = GitFileStatus::from_tag(tag);
                    if best_dirty.is_none_or(|cur| st.severity() > cur.severity()) {
                        best_dirty = Some(st);
                    }
                }
            }
        } else {
            for (p, _, tag) in changed {
                if p == &repo_rel {
                    let st = GitFileStatus::from_tag(tag);
                    if best_dirty.is_none_or(|cur| st.severity() > cur.severity()) {
                        best_dirty = Some(st);
                    }
                }
            }
        }
        if let Some(st) = best_dirty {
            out.push(st);
            continue;
        }

        // 2. Check `.git/index` tracked membership (`O(log N)` binary search).
        let tracked = if let Some(ref paths) = index_paths {
            if *is_dir {
                !paths_dir_range(paths, &repo_rel).is_empty()
            } else {
                paths.binary_search(&repo_rel).is_ok()
            }
        } else {
            false
        };
        if tracked {
            out.push(GitFileStatus::Tracked);
            continue;
        }

        // 3. Not tracked in `.git/index`: distinguish `.gitignore`d artifacts (`target/`) vs untracked.
        if ignore.is_ignored(&repo_rel, *is_dir) {
            out.push(GitFileStatus::Ignored);
        } else {
            out.push(GitFileStatus::Untracked);
        }
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_repo(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "blaze-shell-prompt-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn git");
        assert!(st.success(), "git {args:?} failed");
    }

    #[test]
    fn parse_numstat_handles_text_and_binary() {
        let raw = "12\t3\tsrc/prompt.rs\n-\t-\tassets/logo.png\n0\t5\told.md\r\n\n";
        let map = parse_git_diff_numstat(raw);
        assert_eq!(map.get("src/prompt.rs"), Some(&(12, 3)));
        assert_eq!(map.get("old.md"), Some(&(0, 5)));
        assert!(!map.contains_key("assets/logo.png"));
        assert_eq!(status_label_for_tag("+"), "staged");
        assert_eq!(status_label_for_tag("M"), "modified");
        assert_eq!(status_label_for_tag("D"), "deleted");
        assert_eq!(status_label_for_tag("?"), "untracked");
        assert_eq!(status_label_for_tag("!"), "conflict");
    }

    #[test]
    fn changed_details_populates_status_kind_size_and_diff_stats() {
        let dir = make_temp_repo("changed-details");
        run_git(&dir, &["init", "-q", "-b", "main"]);
        run_git(&dir, &["config", "user.email", "test@blaze.dev"]);
        run_git(&dir, &["config", "user.name", "Blaze Test"]);

        fs::create_dir_all(dir.join("src")).expect("mkdir src");
        fs::write(dir.join("src/lib.rs"), "pub fn a() {}\npub fn b() {}\n").expect("write lib.rs");
        fs::write(dir.join("config.toml"), "mode = \"slow\"\n").expect("write toml");
        fs::write(dir.join("old.md"), "# old doc\n").expect("write old.md");
        run_git(&dir, &["add", "."]);
        run_git(&dir, &["commit", "-q", "-m", "init"]);

        // 1. Untracked-only snapshot first (verifies 0ms no-git-diff path too).
        fs::write(dir.join("k3-meta.install.lock"), "v1\n").expect("write lock");
        fs::write(dir.join("local-bf16.env"), "MODEL=bf16\n").expect("write env");
        let untracked_only = query_blaze_git_status(dir.to_str().unwrap()).expect("git status");
        assert_eq!(untracked_only.changed_details.len(), 2);
        let lock = untracked_only
            .changed_details
            .iter()
            .find(|d| d.path == "k3-meta.install.lock")
            .expect("lock detail");
        assert_eq!(lock.tag, "?");
        assert_eq!(lock.status_label, "untracked");
        assert_eq!(lock.kind_label, "Lockfile");
        assert_eq!(lock.size_bytes, Some(3));
        assert_eq!(lock.added_lines, None);
        assert_eq!(lock.deleted_lines, None);

        let env = untracked_only
            .changed_details
            .iter()
            .find(|d| d.path == "local-bf16.env")
            .expect("env detail");
        assert_eq!(env.tag, "?");
        assert_eq!(env.status_label, "untracked");
        assert_eq!(env.kind_label, "Env");
        assert_eq!(env.size_bytes, Some(11));
        assert_eq!(env.added_lines, None);
        assert_eq!(env.deleted_lines, None);

        // 2. Mix tracked modified, deleted, and untracked files.
        fs::write(
            dir.join("src/lib.rs"),
            "pub fn a() {}\npub fn c() {}\npub fn d() {}\n",
        )
        .expect("modify lib.rs");
        fs::write(dir.join("config.toml"), "mode = \"fast\"\nworkers = 8\n").expect("modify toml");
        fs::remove_file(dir.join("old.md")).expect("remove old.md");

        let mixed = query_blaze_git_status(dir.to_str().unwrap()).expect("mixed git status");
        assert_eq!(mixed.changed_details.len(), mixed.changed_files.len());

        let modified = mixed
            .changed_details
            .iter()
            .find(|d| d.path == "src/lib.rs")
            .expect("modified lib.rs");
        assert_eq!(modified.tag, "M");
        assert_eq!(modified.status_label, "modified");
        assert_eq!(modified.kind_label, "Rust");
        assert!(modified.size_bytes.is_some_and(|n| n > 0));
        assert_eq!(modified.added_lines, Some(2));
        assert_eq!(modified.deleted_lines, Some(1));

        let toml = mixed
            .changed_details
            .iter()
            .find(|d| d.path == "config.toml")
            .expect("modified config.toml");
        assert_eq!(toml.tag, "M");
        assert_eq!(toml.status_label, "modified");
        assert_eq!(toml.kind_label, "TOML");
        assert!(toml.size_bytes.is_some_and(|n| n > 0));
        assert_eq!(toml.added_lines, Some(2));
        assert_eq!(toml.deleted_lines, Some(1));

        assert_eq!(mixed.deleted, 1);
        assert_eq!(
            classify_changed_file_kind(&dir.join("old.md"), "old.md"),
            "Markdown"
        );
        assert_eq!(
            classify_changed_file_kind(&dir.join("deleted_noext"), "deleted_noext"),
            "File"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn branch_relation_reports_divergence_and_common_ancestor() {
        use std::process::Command;

        let dir = std::env::temp_dir().join(format!(
            "blaze_shell_branch_rel_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp repo");

        let run_git = |args: &[&str]| {
            let status = Command::new("git")
                .current_dir(&dir)
                .env("GIT_AUTHOR_NAME", "Blaze")
                .env("GIT_AUTHOR_EMAIL", "blaze@example.com")
                .env("GIT_COMMITTER_NAME", "Blaze")
                .env("GIT_COMMITTER_EMAIL", "blaze@example.com")
                .args(args)
                .status()
                .expect("spawn git");
            assert!(status.success(), "git {args:?} failed");
        };

        run_git(&["init", "-q", "-b", "main"]);
        fs::write(dir.join("a.txt"), "base\n").expect("write base");
        run_git(&["add", "."]);
        run_git(&["commit", "-q", "-m", "common base commit"]);

        // Create feature branch with 2 commits ahead of common base.
        run_git(&["switch", "-q", "-c", "feat/demo"]);
        fs::write(dir.join("feat1.txt"), "1\n2\n3\n").expect("write feat1");
        run_git(&["add", "."]);
        run_git(&["commit", "-q", "-m", "feat commit one"]);
        fs::write(dir.join("a.txt"), "updated\nmore\n").expect("modify a");
        run_git(&["add", "."]);
        run_git(&["commit", "-q", "-m", "feat commit two"]);

        // Advance `main` by 1 commit so `feat/demo` is 2 ahead, 1 behind `main`.
        run_git(&["switch", "-q", "main"]);
        fs::write(dir.join("main2.txt"), "m\n").expect("write main2");
        run_git(&["add", "."]);
        run_git(&["commit", "-q", "-m", "main moved ahead"]);

        // Switch back to `feat/demo` and verify `query_blaze_git_status` populates `branch_relation`.
        run_git(&["switch", "-q", "feat/demo"]);
        let st = query_blaze_git_status(dir.to_str().unwrap()).expect("git status");
        let rel = st.branch_relation.expect("expected branch relation vs main");
        assert_eq!(rel.base_ref, "main");
        assert_eq!(rel.ahead, 2);
        assert_eq!(rel.behind, 1);
        assert_eq!(rel.total_added, 5);
        assert_eq!(rel.total_deleted, 1);
        assert_eq!(rel.total_files, 2);
        assert!(!rel.merge_base_short.is_empty());
        assert!(rel.merge_base_time_secs > 0);
        assert_eq!(rel.merge_base_subject, "common base commit");
        assert_eq!(rel.ahead_commits.len(), 2);
        assert_eq!(rel.ahead_commits[0].subject, "feat commit two");
        assert_eq!(rel.ahead_commits[0].added_lines, 2);
        assert_eq!(rel.ahead_commits[0].deleted_lines, 1);
        assert_eq!(rel.ahead_commits[1].subject, "feat commit one");
        assert_eq!(rel.ahead_commits[1].added_lines, 3);
        assert_eq!(rel.ahead_commits[1].deleted_lines, 0);
        assert_eq!(rel.behind_commits.len(), 1);
        assert_eq!(rel.behind_commits[0].subject, "main moved ahead");
        assert_eq!(rel.behind_commits[0].added_lines, 1);
        assert_eq!(rel.behind_commits[0].deleted_lines, 0);

        // Second call hits the in-process OID cache and returns the exact same relation.
        let st2 = query_blaze_git_status(dir.to_str().unwrap()).expect("cached git status");
        assert_eq!(st2.branch_relation, Some(rel));

        let _ = fs::remove_dir_all(&dir);
    }
}
