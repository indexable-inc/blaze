//! `bgit` — fast, Git-wire-compatible VCS library and CLI over `blaze-git`.
//!
//! Can be driven either from the standalone `bgit` binary (`src/main.rs`) or embedded directly
//! in-process inside `blaze-shell` (zero `fork`/`exec`, thread-safe captured stdout/stderr,
//! explicit `cwd`, and Git CLI compatibility via [`try_run_git_via_bgit`]).

#![feature(default_field_values)]
#![forbid(unsafe_code)]

mod cli;
mod disk;
mod fsmonitor;
mod index_io;
mod net_driver;
mod porcelain;
mod remote_transport;
mod status_driver;
mod vfs_driver;
mod worktree_driver;

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use blaze_git::{
    Detail, PromptStyle, ShortOid, StatusOpts, UntrackedMode, format_long_status,
    format_porcelain_v1, format_short_status, format_status,
    merge::{MergeLimits, MergeOpts, MergeStatus, find_merge_bases, merge_commits},
    slab::SlabCompression,
    storage::{ObjectStore, RefStore, RefTarget, RefUpdate, encode_commit},
};
use blaze_git_fetch::{Kind as ObjectKind, store::Commit};
use blaze_own::{Alloc, String as OwnString, Vec as OwnVec};
use bstd::cli::{Matches, ParseOutcome, format_help_with_path_in, parse_in};

use crate::{
    disk::{BgitDiskStore, disinfect_repo_on_disk, infect_repo_on_disk, open_or_infect},
    net_driver::{
        FetchParams, remote_clone, remote_ls_remote, remote_push, serve_http, serve_stdio,
    },
    remote_transport::{default_clone_dir_name, looks_like_remote_url},
    status_driver::run_status,
    vfs_driver::{cmd_internal_vfs_serve, cmd_mount, cmd_unmount, spawn_background_mount},
    worktree_driver::{
        canonical_branch_ref, cmd_worktree, on_branch_merged, query_worktrees_in_repo,
        refresh_bgit_if_git_refs_newer,
    },
};

/// Exposes `bgit`'s declarative `bstd::cli` command tree so embedders like `blaze-shell`
/// derive completions, live inline signatures, and help previews from one source of truth.
pub use bstd::cli::CommandSpec as CliCommandSpec;

/// Returns the root declarative CLI specification for `bgit`.
pub const fn cli_spec() -> &'static CliCommandSpec<'static> {
    &cli::BGIT_CLI
}

/// Captured result of running `bgit` (or a `git` command routed to `libbgit`) in-process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Structured commit metadata returned by [`query_log_entries`] for rich shell tables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub oid: String,
    pub short_oid: String,
    pub author_name: String,
    pub author_email: String,
    pub timestamp_secs: i64,
    pub subject: String,
}

/// Structured worktree metadata returned by [`query_worktrees`] for rich shell tables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: String,
    pub track_branch: String,
    pub head_short: String,
    pub is_vfs: bool,
    pub auto_delete_on_merge: bool,
}

/// Structured branch metadata returned by [`query_branches`] for rich shell tables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    pub short_oid: String,
    pub oid: String,
    pub subject: String,
    pub is_current: bool,
    pub is_remote: bool,
}

/// Structured per-file diffstat row returned by [`query_diff_stat`] for rich shell tables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffStatEntry {
    pub path: String,
    pub status: char,
    pub insertions: usize,
    pub deletions: usize,
}

/// Starts or refreshes an in-process FSEvents / kernel journal dirty-path monitor for the repo
/// containing `cwd`. Safe to call repeatedly (O(1) once started); accelerates subsequent in-process
/// `bgit status` and `bgit diff` calls to O(dirty).
pub fn ensure_fsmonitor_watching(cwd: &Path) -> bool {
    let Some(root) = find_repo_root(cwd) else {
        return false;
    };
    fsmonitor::ensure_watching(&root)
}

/// Explicit output writers so `libbgit` never hardcodes process-global stdout/stderr.
pub(crate) struct IoStreams<'w> {
    pub(crate) out: &'w mut dyn Write,
    pub(crate) err: &'w mut dyn Write,
}

pub(crate) fn resolve_path(cwd: &Path, raw: &str) -> PathBuf {
    if raw.is_empty() || raw == "." {
        return cwd.to_path_buf();
    }
    let p = Path::new(raw);
    if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
}

/// Resolves the `bgit` executable path cleanly even when `libbgit` runs in-process inside
/// `blaze-shell` (where `std::env::current_exe()` is `blaze-shell`, not `bgit`).
pub(crate) fn resolve_bgit_binary() -> PathBuf {
    if let Ok(cur) = std::env::current_exe()
        && cur.file_stem().and_then(|s| s.to_str()) == Some("bgit")
    {
        return cur;
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let cand = dir.join("bgit");
            if cand.is_file() {
                return cand;
            }
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let cargo_bin = PathBuf::from(home).join(".cargo/bin/bgit");
        if cargo_bin.is_file() {
            return cargo_bin;
        }
    }
    PathBuf::from("bgit")
}

/// Walks upward from `start` to find the nearest repository root containing `.bgit/VERSION` or `.git`.
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut cur = fs::canonicalize(start).ok().unwrap_or_else(|| start.to_path_buf());
    for _ in 0..64 {
        if cur.join(".bgit/VERSION").is_file() || cur.join(".git").exists() {
            return Some(cur);
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

/// Returns `true` for long-running/blocking `bgit` subcommands (`serve`, `worktree watch`,
/// `mount --foreground`, `__vfs-serve`) that an interactive shell should spawn as a live process
/// rather than buffering to completion in-process.
pub fn is_streaming_bgit_command(args: &[&str]) -> bool {
    let Some(&sub) = args.first() else {
        return false;
    };
    match sub {
        "serve" | "__vfs-serve" => true,
        "worktree" | "wt" => matches!(args.get(1).copied(), Some("watch")),
        "mount" => args.iter().any(|&a| a == "--foreground" || a == "-f"),
        _ => false,
    }
}

/// Executes any `bgit` CLI invocation in-process against explicit `cwd`, capturing stdout/stderr.
pub fn run_bgit_in_process<'a>(
    args: &[&str],
    cwd: &Path,
    use_color: bool,
    alloc: Alloc<'a>,
) -> ExecOutput {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit_code = run_bgit_to_writers(args, cwd, use_color, &mut stdout, &mut stderr, alloc);
    ExecOutput { exit_code, stdout, stderr }
}

/// Executes any `bgit` CLI invocation writing directly to `out` and `err`.
pub fn run_bgit_to_writers<'a>(
    args: &[&str],
    cwd: &Path,
    use_color: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
    alloc: Alloc<'a>,
) -> i32 {
    let mut io = IoStreams { out, err };
    let outcome = match parse_in(&cli::BGIT_CLI, args, alloc) {
        Ok(o) => o,
        Err(parse_err) => {
            let _ = writeln!(io.err, "bgit error: {parse_err}");
            return 1;
        }
    };

    match outcome {
        ParseOutcome::Help { command, path } => {
            print_help(command, path.as_slice(), use_color, alloc, io.out);
            0
        }
        ParseOutcome::Version { name, version } => {
            let _ = writeln!(io.out, "{name} {version}");
            0
        }
        ParseOutcome::Matched(matches) => {
            if matches.command_path().len() == 1 {
                print_help(matches.active_command(), matches.command_path(), use_color, alloc, io.out);
                return 0;
            }
            match dispatch(&matches, cwd, use_color, alloc, &mut io) {
                Ok(()) => 0,
                Err(e) => {
                    let _ = writeln!(io.err, "bgit error: {e}");
                    1
                }
            }
        }
    }
}

/// Routes supported `git` CLI invocations directly to in-process `libbgit` (zero `fork`/`exec`!).
///
/// Returns `Some(ExecOutput)` for commands natively backed by `libbgit`, or `None` when the
/// invocation uses a `git`-only subcommand or exotic flag that should be handled by external `git`.
/// Note: never silently mutates an un-infected `.git` repository (only uses `.bgit` slabs when
/// `.bgit/VERSION` already exists, refreshing from `.git` automatically when `.git` refs are newer).
pub fn try_run_git_via_bgit<'a>(
    args: &[&str],
    cwd: &Path,
    use_color: bool,
    alloc: Alloc<'a>,
) -> Option<ExecOutput> {
    // Parse leading global git flags (`-C <dir>`, `--no-pager`). Any other global flag defers to `git`.
    let mut work_dir = cwd.to_path_buf();
    let mut idx = 0usize;
    while let Some(&tok) = args.get(idx) {
        if tok == "-C" {
            let next = *args.get(idx + 1)?;
            work_dir = resolve_path(&work_dir, next);
            idx += 2;
        } else if let Some(rest) = tok.strip_prefix("-C")
            && !rest.is_empty()
        {
            work_dir = resolve_path(&work_dir, rest);
            idx += 1;
        } else if tok == "--no-pager" || tok == "-P" {
            idx += 1;
        } else if tok.starts_with('-') {
            return None;
        } else {
            break;
        }
    }

    let sub = *args.get(idx)?;
    let rest = &args[idx + 1..];

    match sub {
        "status" | "st" => try_git_status_in_process(rest, &work_dir, use_color, alloc),
        "rev-parse" => try_git_rev_parse_in_process(rest, &work_dir, alloc),
        "log" => try_git_log_in_process(rest, &work_dir, use_color, alloc),
        "merge-base" => try_git_merge_base_in_process(rest, &work_dir, alloc),
        "show-ref" => try_git_show_ref_in_process(rest, &work_dir, alloc),
        "ls-remote" => try_git_ls_remote_in_process(rest, &work_dir, alloc),
        "worktree" => try_git_worktree_in_process(rest, &work_dir, use_color, alloc),
        "branch" | "br" | "tag" | "diff" | "show" | "remote" | "add" | "commit" | "checkout"
        | "switch" | "reset" | "restore" | "stash" | "rm" | "mv" | "cherry-pick" | "revert"
        | "clean" | "config" => try_git_bgit_passthrough(sub, rest, &work_dir, use_color, alloc),
        _ => None,
    }
}

/// Forwards Git porcelain commands to in-process `libbgit` ONLY when the enclosing repo already
/// has `.bgit/VERSION` AND every argument is accepted by `bgit`'s `bstd::cli` spec (never
/// implicitly infecting un-infected `.git` repos and never intercepting interactive/exotic flags
/// such as `-p`, `-i`, or `git commit` without `-m`/`--amend`).
fn try_git_bgit_passthrough<'a>(
    sub: &str,
    rest: &[&str],
    work_dir: &Path,
    use_color: bool,
    alloc: Alloc<'a>,
) -> Option<ExecOutput> {
    let root = find_repo_root(work_dir)?;
    if !root.join(".bgit/VERSION").is_file() {
        return None;
    }
    // Interactive `$EDITOR` commits (`git commit` with neither `-m` nor `--amend`) must go to real git.
    if sub == "commit"
        && !rest.iter().any(|&a| {
            a == "-m"
                || a.starts_with("-m")
                || a == "--message"
                || a.starts_with("--message=")
                || a == "--amend"
        })
    {
        return None;
    }
    let mut bgit_args: Vec<&str> = Vec::with_capacity(rest.len() + 1);
    bgit_args.push(sub);
    bgit_args.extend_from_slice(rest);
    // Validate strictly against `BGIT_CLI` first so any exotic git-only flag cleanly falls through.
    match parse_in(&cli::BGIT_CLI, &bgit_args, alloc) {
        Ok(ParseOutcome::Matched(_)) => Some(run_bgit_in_process(&bgit_args, work_dir, use_color, alloc)),
        _ => None,
    }
}

/// Reads structured branch metadata directly from `.bgit` mmap slabs (`BGRF` v2 O(log R + K)).
/// Returns `Ok(None)` if the repo is not `.bgit`-infected (never creates `.bgit` implicitly).
pub fn query_branches<'a>(
    cwd: &Path,
    include_all: bool,
    remotes_only: bool,
    alloc: Alloc<'a>,
) -> io::Result<Option<Vec<BranchInfo>>> {
    let Some(root) = find_repo_root(cwd) else {
        return Ok(None);
    };
    let Some(store) = open_existing_bgit_fresh(&root, alloc)? else {
        return Ok(None);
    };
    let rows = porcelain::query_branches_in_repo(&store, include_all, remotes_only, alloc)?;
    Ok(Some(rows))
}

/// Reads structured diffstat rows directly from `.bgit` mmap slabs + in-process FSEvents dirty set.
/// Returns `Ok(None)` if the repo is not `.bgit`-infected (never creates `.bgit` implicitly).
pub fn query_diff_stat<'a>(
    cwd: &Path,
    staged: bool,
    alloc: Alloc<'a>,
) -> io::Result<Option<Vec<DiffStatEntry>>> {
    let Some(root) = find_repo_root(cwd) else {
        return Ok(None);
    };
    let Some(store) = open_existing_bgit_fresh(&root, alloc)? else {
        return Ok(None);
    };
    let rows = porcelain::query_diff_stat_in_repo(&root, &store, staged, alloc)?;
    Ok(Some(rows))
}

/// Reads structured commit log entries directly from `.bgit` mmap slabs (if `.bgit` exists for
/// `cwd`, auto-refreshing if `.git` refs are newer). Returns `Ok(None)` if the repo is not
/// `.bgit`-infected (never creates `.bgit` implicitly).
pub fn query_log_entries<'a>(
    cwd: &Path,
    rev: &str,
    limit: usize,
    alloc: Alloc<'a>,
) -> io::Result<Option<Vec<LogEntry>>> {
    let Some(root) = find_repo_root(cwd) else {
        return Ok(None);
    };
    let Some(store) = open_existing_bgit_fresh(&root, alloc)? else {
        return Ok(None);
    };
    let entries = collect_log_entries(&store, rev, limit, alloc)?;
    Ok(Some(entries))
}

/// Lists registered `bgit` worktrees for the repo containing `cwd`, if `.bgit/worktrees.bgwt` exists.
pub fn query_worktrees<'a>(cwd: &Path, alloc: Alloc<'a>) -> io::Result<Option<Vec<WorktreeInfo>>> {
    let Some(root) = find_repo_root(cwd) else {
        return Ok(None);
    };
    query_worktrees_in_repo(&root, alloc)
}

fn open_existing_bgit_fresh<'a>(
    repo_root: &Path,
    alloc: Alloc<'a>,
) -> io::Result<Option<BgitDiskStore<'a>>> {
    if !repo_root.join(".bgit/VERSION").is_file() {
        return Ok(None);
    }
    refresh_bgit_if_git_refs_newer(repo_root, false, alloc)?;
    Ok(Some(BgitDiskStore::open(repo_root, alloc)?))
}

fn try_git_status_in_process<'a>(
    rest: &[&str],
    work_dir: &Path,
    use_color: bool,
    alloc: Alloc<'a>,
) -> Option<ExecOutput> {
    // Only intercept options that `bgit status` implements with exact Git parity.
    for &arg in rest {
        let supported = matches!(
            arg,
            "-s" | "--short"
                | "-b"
                | "--branch"
                | "-sb"
                | "-bs"
                | "--porcelain"
                | "--porcelain=v1"
                | "--porcelain=1"
                | "-u"
                | "-uno"
                | "-unormal"
                | "-uall"
                | "-u=no"
                | "-u=normal"
                | "-u=all"
                | "--untracked-files"
                | "--untracked-files=no"
                | "--untracked-files=normal"
                | "--untracked-files=all"
                | "--color"
                | "--color=always"
                | "--color=auto"
                | "--color=never"
                | "--no-color"
        );
        if !supported {
            return None;
        }
    }
    let mut bgit_args: Vec<&str> = Vec::with_capacity(rest.len() + 1);
    bgit_args.push("status");
    bgit_args.extend_from_slice(rest);
    Some(run_bgit_in_process(&bgit_args, work_dir, use_color, alloc))
}

fn try_git_rev_parse_in_process<'a>(
    rest: &[&str],
    work_dir: &Path,
    alloc: Alloc<'a>,
) -> Option<ExecOutput> {
    let root = find_repo_root(work_dir)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    match rest {
        ["--show-toplevel"] => {
            let _ = writeln!(stdout, "{}", root.display());
            Some(ExecOutput { exit_code: 0, stdout, stderr })
        }
        ["--is-inside-work-tree"] => {
            let _ = writeln!(stdout, "true");
            Some(ExecOutput { exit_code: 0, stdout, stderr })
        }
        ["--git-dir"] => {
            let gd = if root.join(".git").exists() { root.join(".git") } else { root.join(".bgit") };
            let rel = gd.strip_prefix(work_dir).unwrap_or(&gd);
            let _ = writeln!(stdout, "{}", rel.display());
            Some(ExecOutput { exit_code: 0, stdout, stderr })
        }
        ["--abbrev-ref", "HEAD"] => {
            if let Ok(Some(store)) = open_existing_bgit_fresh(&root, alloc) {
                match store.read_head(alloc) {
                    Ok(Some(RefTarget::Symbolic(sym))) => {
                        let name = sym.as_str().strip_prefix("refs/heads/").unwrap_or(sym.as_str());
                        let _ = writeln!(stdout, "{name}");
                        return Some(ExecOutput { exit_code: 0, stdout, stderr });
                    }
                    Ok(Some(RefTarget::Direct(_))) => {
                        let _ = writeln!(stdout, "HEAD");
                        return Some(ExecOutput { exit_code: 0, stdout, stderr });
                    }
                    _ => {}
                }
            }
            // Also handle un-infected `.git/HEAD` directly in-process with zero disk writes!
            let head_path = resolve_git_head_path(&root)?;
            let raw = fs::read_to_string(head_path).ok()?;
            let line = raw.trim();
            if let Some(sym) = line.strip_prefix("ref:") {
                let sym = sym.trim();
                let name = sym.strip_prefix("refs/heads/").unwrap_or(sym);
                let _ = writeln!(stdout, "{name}");
            } else {
                let _ = writeln!(stdout, "HEAD");
            }
            Some(ExecOutput { exit_code: 0, stdout, stderr })
        }
        ["--short", rev] if !rev.starts_with('-') => {
            let store = open_existing_bgit_fresh(&root, alloc).ok()??;
            match store.resolve_rev(rev) {
                Ok(Some(oid)) => {
                    let _ = writeln!(stdout, "{}", ShortOid::from_oid(oid, 7));
                    Some(ExecOutput { exit_code: 0, stdout, stderr })
                }
                Ok(None) => {
                    let _ = writeln!(stderr, "fatal: ambiguous argument '{rev}': unknown revision");
                    Some(ExecOutput { exit_code: 128, stdout, stderr })
                }
                Err(e) => {
                    let _ = writeln!(stderr, "fatal: {e}");
                    Some(ExecOutput { exit_code: 128, stdout, stderr })
                }
            }
        }
        [rev] if !rev.starts_with('-') => {
            let store = open_existing_bgit_fresh(&root, alloc).ok()??;
            match store.resolve_rev(rev) {
                Ok(Some(oid)) => {
                    let _ = writeln!(stdout, "{oid}");
                    Some(ExecOutput { exit_code: 0, stdout, stderr })
                }
                Ok(None) => {
                    let _ = writeln!(stderr, "fatal: ambiguous argument '{rev}': unknown revision");
                    Some(ExecOutput { exit_code: 128, stdout, stderr })
                }
                Err(e) => {
                    let _ = writeln!(stderr, "fatal: {e}");
                    Some(ExecOutput { exit_code: 128, stdout, stderr })
                }
            }
        }
        _ => None,
    }
}

fn resolve_git_head_path(repo_root: &Path) -> Option<PathBuf> {
    let dot_git = repo_root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git.join("HEAD"));
    }
    if dot_git.is_file() {
        let raw = fs::read(&dot_git).ok()?;
        let target = blaze_git::refs::parse_gitdir_file(&raw).ok()?;
        let p = Path::new(target);
        let gd = if p.is_absolute() { p.to_path_buf() } else { repo_root.join(p) };
        return Some(gd.join("HEAD"));
    }
    None
}

fn try_git_log_in_process<'a>(
    rest: &[&str],
    work_dir: &Path,
    use_color: bool,
    alloc: Alloc<'a>,
) -> Option<ExecOutput> {
    let root = find_repo_root(work_dir)?;
    let store = open_existing_bgit_fresh(&root, alloc).ok()??;

    let mut limit = 20usize;
    let mut oneline = false;
    let mut rev: Option<&str> = None;
    let mut i = 0usize;
    while let Some(&tok) = rest.get(i) {
        if tok == "--oneline" {
            oneline = true;
            i += 1;
        } else if tok == "-n" || tok == "--max-count" {
            let val = *rest.get(i + 1)?;
            limit = val.parse().ok()?;
            i += 2;
        } else if let Some(val) = tok.strip_prefix("--max-count=").or_else(|| tok.strip_prefix("-n"))
            && !val.is_empty()
        {
            limit = val.parse().ok()?;
            i += 1;
        } else if let Some(digits) = tok.strip_prefix('-')
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            limit = digits.parse().ok()?;
            i += 1;
        } else if tok.starts_with('-') {
            return None;
        } else if rev.is_none() {
            rev = Some(tok);
            i += 1;
        } else {
            // Multiple positionals or pathspecs -> defer to git.
            return None;
        }
    }

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let target_rev = rev.unwrap_or("HEAD");
    match collect_log_entries(&store, target_rev, limit, alloc) {
        Ok(entries) => {
            for e in entries {
                if oneline {
                    if use_color {
                        let _ = writeln!(stdout, "\x1b[33m{}\x1b[0m {}", e.short_oid, e.subject);
                    } else {
                        let _ = writeln!(stdout, "{} {}", e.short_oid, e.subject);
                    }
                } else {
                    if use_color {
                        let _ = writeln!(stdout, "\x1b[33mcommit {}\x1b[0m", e.oid);
                    } else {
                        let _ = writeln!(stdout, "commit {}", e.oid);
                    }
                    let _ = writeln!(stdout, "Author: {} <{}>", e.author_name, e.author_email);
                    let _ = writeln!(stdout, "Date:   {}\n", e.timestamp_secs);
                    let _ = writeln!(stdout, "    {}\n", e.subject);
                }
            }
            Some(ExecOutput { exit_code: 0, stdout, stderr })
        }
        Err(e) => {
            let _ = writeln!(stderr, "fatal: {e}");
            Some(ExecOutput { exit_code: 128, stdout, stderr })
        }
    }
}

fn try_git_merge_base_in_process<'a>(
    rest: &[&str],
    work_dir: &Path,
    alloc: Alloc<'a>,
) -> Option<ExecOutput> {
    let [rev_a, rev_b] = rest else {
        return None;
    };
    if rev_a.starts_with('-') || rev_b.starts_with('-') {
        return None;
    }
    let root = find_repo_root(work_dir)?;
    let store = open_existing_bgit_fresh(&root, alloc).ok()??;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let Some(a) = store.resolve_rev(rev_a).ok().flatten() else {
        let _ = writeln!(stderr, "fatal: Not a valid object name {rev_a}");
        return Some(ExecOutput { exit_code: 128, stdout, stderr });
    };
    let Some(b) = store.resolve_rev(rev_b).ok().flatten() else {
        let _ = writeln!(stderr, "fatal: Not a valid object name {rev_b}");
        return Some(ExecOutput { exit_code: 128, stdout, stderr });
    };
    let cg = store.commit_graph();
    match find_merge_bases(&store, cg.as_ref(), a, b, MergeLimits::SHIPPED, alloc) {
        Ok(bases) => {
            if bases.is_empty() {
                return Some(ExecOutput { exit_code: 1, stdout, stderr });
            }
            for base in bases {
                let _ = writeln!(stdout, "{base}");
            }
            Some(ExecOutput { exit_code: 0, stdout, stderr })
        }
        Err(e) => {
            let _ = writeln!(stderr, "fatal: {e}");
            Some(ExecOutput { exit_code: 128, stdout, stderr })
        }
    }
}

fn try_git_show_ref_in_process<'a>(
    rest: &[&str],
    work_dir: &Path,
    alloc: Alloc<'a>,
) -> Option<ExecOutput> {
    let prefix = match rest {
        [] => "",
        ["--heads"] => "refs/heads/",
        ["--tags"] => "refs/tags/",
        _ => return None,
    };
    let root = find_repo_root(work_dir)?;
    let store = open_existing_bgit_fresh(&root, alloc).ok()??;
    let mut refs = OwnVec::new_in(alloc);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Err(e) = store.list_refs(prefix, alloc, &mut refs) {
        let _ = writeln!(stderr, "fatal: {e}");
        return Some(ExecOutput { exit_code: 128, stdout, stderr });
    }
    let mut rows: Vec<(String, String)> = Vec::with_capacity(refs.len());
    for r in &refs {
        let oid = match &r.target {
            RefTarget::Direct(o) => Some(*o),
            RefTarget::Symbolic(_) => store.resolve_ref(r.name.as_str()).ok().flatten(),
        };
        if let Some(o) = oid {
            rows.push((r.name.as_str().to_string(), format!("{o}")));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    if rows.is_empty() {
        return Some(ExecOutput { exit_code: 1, stdout, stderr });
    }
    for (name, oid) in rows {
        let _ = writeln!(stdout, "{oid} {name}");
    }
    Some(ExecOutput { exit_code: 0, stdout, stderr })
}

fn try_git_ls_remote_in_process<'a>(
    rest: &[&str],
    work_dir: &Path,
    alloc: Alloc<'a>,
) -> Option<ExecOutput> {
    let [url] = rest else {
        return None;
    };
    if !looks_like_remote_url(url) {
        return None;
    }
    Some(run_bgit_in_process(&["ls-remote", url], work_dir, false, alloc))
}

fn try_git_worktree_in_process<'a>(
    rest: &[&str],
    work_dir: &Path,
    use_color: bool,
    alloc: Alloc<'a>,
) -> Option<ExecOutput> {
    let root = find_repo_root(work_dir)?;
    let has_bgit_wt = root.join(".bgit/worktrees.bgwt").is_file();
    let is_bgit_specific = matches!(rest.first().copied(), Some("sync"))
        || rest.iter().any(|&a| matches!(a, "--track" | "--vfs" | "--no-auto-delete"));
    if is_bgit_specific || (has_bgit_wt && matches!(rest.first().copied(), Some("list" | "ls"))) {
        let mut bgit_args: Vec<&str> = Vec::with_capacity(rest.len() + 1);
        bgit_args.push("worktree");
        bgit_args.extend_from_slice(rest);
        return Some(run_bgit_in_process(&bgit_args, &root, use_color, alloc));
    }
    None
}

fn collect_log_entries<'a>(
    store: &BgitDiskStore<'a>,
    rev: &str,
    limit: usize,
    alloc: Alloc<'a>,
) -> io::Result<Vec<LogEntry>> {
    let mut cur = store
        .resolve_rev(rev)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    if cur.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("unknown revision `{rev}`"),
        ));
    }
    let mut buf = OwnVec::new_in(alloc);
    let mut entries = Vec::with_capacity(limit.min(256));
    while let Some(oid) = cur {
        if entries.len() >= limit {
            break;
        }
        let kind = store
            .read_object(&oid, &mut buf)
            .map_err(|e| io::Error::other(format!("{e}")))?;
        if kind != Some(ObjectKind::Commit) {
            break;
        }
        let parsed = Commit::parse(&buf).map_err(|(_, why)| io::Error::other(format!("{why:?}")))?;
        let text = core::str::from_utf8(&buf).unwrap_or("");
        let (author_name, author_email, timestamp_secs) = parse_commit_author_line(text);
        let subject = text
            .split_once("\n\n")
            .and_then(|(_, msg)| msg.lines().next())
            .unwrap_or("")
            .to_string();
        entries.push(LogEntry {
            oid: format!("{oid}"),
            short_oid: format!("{}", ShortOid::from_oid(oid, 7)),
            author_name,
            author_email,
            timestamp_secs,
            subject,
        });
        cur = parsed.parents().next();
    }
    Ok(entries)
}

fn parse_commit_author_line(commit_text: &str) -> (String, String, i64) {
    for line in commit_text.lines() {
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix("author ")
            && let Some((name_part, after_lt)) = rest.split_once('<')
            && let Some((email_part, after_gt)) = after_lt.split_once('>')
        {
            let ts = after_gt
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            return (name_part.trim().to_string(), email_part.trim().to_string(), ts);
        }
    }
    (String::new(), String::new(), 0)
}

pub(crate) fn print_help<'a>(
    spec: &bstd::cli::CommandSpec<'_>,
    path: &[&str],
    color: bool,
    alloc: Alloc<'a>,
    out: &mut dyn Write,
) {
    if let Ok(text) = format_help_with_path_in(spec, path, color, alloc) {
        let _ = write!(out, "{}", text.as_str());
    }
}

fn dispatch<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    use_color: bool,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    match matches.command_path().get(1).copied().unwrap_or("") {
        "status" => cmd_status(matches, cwd, use_color, alloc, io),
        "add" => porcelain::cmd_add(matches, cwd, alloc, io),
        "commit" => porcelain::cmd_commit(matches, cwd, alloc, io),
        "branch" => porcelain::cmd_branch(matches, cwd, use_color, alloc, io),
        "checkout" => porcelain::cmd_checkout(matches, cwd, alloc, io),
        "switch" => porcelain::cmd_switch(matches, cwd, alloc, io),
        "diff" => {
            let color = use_color && !matches.flag("no-color");
            porcelain::cmd_diff(matches, cwd, color, alloc, io)
        }
        "show" => {
            let color = use_color && !matches.flag("no-color");
            porcelain::cmd_show(matches, cwd, color, alloc, io)
        }
        "reset" => porcelain::cmd_reset(matches, cwd, alloc, io),
        "restore" => porcelain::cmd_restore(matches, cwd, alloc, io),
        "rm" => porcelain::cmd_rm(matches, cwd, alloc, io),
        "mv" => porcelain::cmd_mv(matches, cwd, alloc, io),
        "tag" => porcelain::cmd_tag(matches, cwd, alloc, io),
        "rev-parse" => porcelain::cmd_rev_parse(matches, cwd, alloc, io),
        "show-ref" => porcelain::cmd_show_ref(matches, cwd, alloc, io),
        "remote" => porcelain::cmd_remote(matches, cwd, alloc, io),
        "config" => porcelain::cmd_config(matches, cwd, alloc, io),
        "stash" => porcelain::cmd_stash(matches, cwd, use_color, alloc, io),
        "cherry-pick" => porcelain::cmd_cherry_pick(matches, cwd, alloc, io),
        "revert" => porcelain::cmd_revert(matches, cwd, alloc, io),
        "clean" => porcelain::cmd_clean(matches, cwd, alloc, io),
        "fetch" => porcelain::cmd_fetch(matches, cwd, alloc, io),
        "pull" => porcelain::cmd_pull(matches, cwd, alloc, io),
        "worktree" => cmd_worktree(matches, cwd, use_color, alloc, io),
        "mount" => cmd_mount(matches, cwd, alloc, io),
        "unmount" => cmd_unmount(matches, cwd, io),
        "__vfs-serve" => cmd_internal_vfs_serve(matches, cwd, alloc),
        "init" => {
            let dir = resolve_path(cwd, matches.positional(0).unwrap_or("."));
            let store = BgitDiskStore::init(&dir, alloc)?;
            writeln!(io.out, "Initialized empty .bgit repository in {}", store.bgit_dir.display())?;
            Ok(())
        }
        "infect" => {
            let dir = resolve_path(cwd, matches.positional(0).unwrap_or("."));
            let compression = if matches.flag("raw") {
                SlabCompression::RawOnly
            } else {
                SlabCompression::AutoZstd
            };
            let t0 = Instant::now();
            let stats = infect_repo_on_disk(&dir, compression, alloc)?;
            let elapsed = t0.elapsed();
            let ratio = if stats.stored_slab_bytes > 0 {
                stats.raw_object_bytes as f64 / stats.stored_slab_bytes as f64
            } else {
                1.0
            };
            writeln!(
                io.out,
                "Infected {} -> {}/.bgit in {:.2} ms\n  \
                 objects: {} unique (from {} loose + {} in {} packfiles)\n  \
                 commits in CGPH graph: {}\n  \
                 refs in BGRF table: {}\n  \
                 bytes: {} raw -> {} stored ({:.2}x)",
                dir.display(),
                dir.display(),
                elapsed.as_secs_f64() * 1000.0,
                stats.unique_objects_out,
                stats.loose_objects_in,
                stats.packed_objects_in,
                stats.packfiles_in,
                stats.commits_in_graph,
                stats.refs_out,
                stats.raw_object_bytes,
                stats.stored_slab_bytes,
                ratio,
            )?;
            Ok(())
        }
        "disinfect" => {
            let dir = resolve_path(cwd, matches.positional(0).expect("required DIR"));
            let out = resolve_path(cwd, matches.positional(1).expect("required OUT_GIT_DIR"));
            let t0 = Instant::now();
            disinfect_repo_on_disk(&dir, &out, alloc)?;
            writeln!(
                io.out,
                "Disinfected {} -> {} in {:.2} ms",
                dir.display(),
                out.display(),
                t0.elapsed().as_secs_f64() * 1000.0
            )?;
            Ok(())
        }
        "verify" => {
            let dir = resolve_path(cwd, matches.positional(0).unwrap_or("."));
            let t0 = Instant::now();
            let store = BgitDiskStore::open(&dir, alloc)?;
            let checked = store.verify_full()?;
            writeln!(
                io.out,
                "Verified {} objects (BGIX CRC-32 + per-record CRC-32 + SHA-1) in {:.2} ms",
                checked,
                t0.elapsed().as_secs_f64() * 1000.0
            )?;
            Ok(())
        }
        "log" => cmd_log(matches, cwd, use_color, alloc, io),
        "merge-base" => cmd_merge_base(matches, cwd, alloc, io),
        "merge" => cmd_merge(matches, cwd, alloc, io),
        "serve" => {
            let repo = resolve_path(cwd, matches.positional(0).expect("required REPO"));
            match (matches.opt("http"), matches.opt("stdio")) {
                (Some(addr), None) => serve_http(addr, &repo, alloc),
                (None, Some(service)) => serve_stdio(service, &repo, alloc),
                (Some(_), Some(_)) => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "specify only one of --http or --stdio",
                )),
                (None, None) => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "specify --http <addr> or --stdio <upload-pack|receive-pack>",
                )),
            }
        }
        "ls-remote" => {
            let raw = matches.positional(0).unwrap_or("origin");
            let url = if looks_like_remote_url(raw) {
                raw.to_owned()
            } else {
                let root = find_repo_root(cwd).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "not a git or bgit repository")
                })?;
                let cfg = index_io::GitConfig::load_merged(&root);
                cfg.get(&format!("remote.{raw}.url"))
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, format!("no such remote '{raw}'"))
                    })?
                    .to_owned()
            };
            remote_ls_remote(&url, alloc, io.out)
        }
        "clone" => {
            let url = matches.positional(0).expect("required URL");
            let dest_raw = matches
                .positional(1)
                .map(str::to_owned)
                .unwrap_or_else(|| default_clone_dir_name(url));
            let dest = resolve_path(cwd, &dest_raw);
            let depth = matches
                .opt_parse::<u32>("depth")
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?;
            let filter = matches.opt("filter");
            let params = FetchParams { depth, filter };
            if matches.flag("vfs") {
                let mut store_name = dest
                    .file_name()
                    .map(|n| n.to_os_string())
                    .unwrap_or_else(|| std::ffi::OsString::from("repo"));
                store_name.push(".bgit-store");
                let store_dir = dest.with_file_name(store_name);
                remote_clone(url, &store_dir, false, params, alloc, io.out)?;
                spawn_background_mount(&store_dir, &dest, "HEAD", false, io.out)
            } else {
                remote_clone(url, &dest, true, params, alloc, io.out)
            }
        }
        "push" => cmd_push(matches, cwd, alloc, io),
        "bench" => {
            let dir = resolve_path(cwd, matches.positional(0).unwrap_or("."));
            cmd_bench(&dir, alloc, io)
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown command `{other}` (run `bgit --help`)"),
        )),
    }
}

fn cmd_merge_base<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let (dir, rev_a, rev_b) = if let Some(rb) = matches.positional(2) {
        (
            resolve_path(cwd, matches.positional(0).expect("required DIR")),
            matches.positional(1).expect("required REV_A"),
            rb,
        )
    } else {
        let root = find_repo_root(cwd).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "not a git or bgit repository")
        })?;
        (
            root,
            matches.positional(0).expect("required REV_A"),
            matches.positional(1).expect("required REV_B"),
        )
    };
    let store = open_or_infect(&dir, alloc)?;
    let a = store
        .resolve_rev(rev_a)
        .map_err(|e| io::Error::other(format!("{e}")))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("rev not found: {rev_a}")))?;
    let b = store
        .resolve_rev(rev_b)
        .map_err(|e| io::Error::other(format!("{e}")))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("rev not found: {rev_b}")))?;
    let cg = store.commit_graph();
    let bases = find_merge_bases(&store, cg.as_ref(), a, b, MergeLimits::SHIPPED, alloc)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    for base in bases {
        writeln!(io.out, "{base}")?;
    }
    Ok(())
}

fn cmd_push<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    // Backwards-compatible 3-arg form: `bgit push <url> <dir> <ref>`
    if let Some(ref_name) = matches.positional(2) {
        let url = matches.positional(0).expect("required URL");
        let dir = resolve_path(cwd, matches.positional(1).expect("required DIR"));
        return remote_push(url, &dir, ref_name, alloc, io.out);
    }
    // Ergonomic Git form: `bgit push [<remote>] [<branch>]`
    let root = find_repo_root(cwd).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "not a git or bgit repository")
    })?;
    let remote = matches.positional(0).unwrap_or("origin");
    let url = if looks_like_remote_url(remote) {
        remote.to_owned()
    } else {
        let cfg = index_io::GitConfig::load_merged(&root);
        cfg.get(&format!("remote.{remote}.url"))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such remote '{remote}' (configure with `bgit remote add {remote} <url>`)"),
                )
            })?
            .to_owned()
    };
    let ref_name = if let Some(r) = matches.positional(1) {
        canonical_branch_ref(r)
    } else {
        let store = open_or_infect(&root, alloc)?;
        match store.read_head(alloc).map_err(|e| io::Error::other(format!("{e}")))? {
            Some(RefTarget::Symbolic(sym)) => sym.as_str().to_owned(),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "detached HEAD; specify a branch or ref to push",
                ));
            }
        }
    };
    remote_push(&url, &root, &ref_name, alloc, io.out)
}

fn cmd_log<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    use_color: bool,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let oneline = matches.flag("oneline");
    let color = use_color && !matches.flag("no-color");
    let limit = matches
        .opt_parse::<usize>("max-count")
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?
        .unwrap_or(20);

    let (dir, rev) = match (matches.positional(0), matches.positional(1)) {
        (Some(d), Some(r)) => (resolve_path(cwd, d), r),
        (Some(arg), None) => {
            let cand = resolve_path(cwd, arg);
            if cand.join(".bgit/VERSION").is_file() || cand.join(".git").exists() {
                (cand, "HEAD")
            } else {
                let root = find_repo_root(cwd).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "not a git or bgit repository")
                })?;
                (root, arg)
            }
        }
        (None, _) => {
            let root = find_repo_root(cwd).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "not a git or bgit repository")
            })?;
            (root, "HEAD")
        }
    };

    if dir.join(".bgit/VERSION").is_file() {
        let _ = refresh_bgit_if_git_refs_newer(&dir, false, alloc);
    }
    let store = open_or_infect(&dir, alloc)?;
    for entry in collect_log_entries(&store, rev, limit, alloc)? {
        if oneline {
            if color {
                writeln!(io.out, "\x1b[33m{}\x1b[0m {}", entry.short_oid, entry.subject)?;
            } else {
                writeln!(io.out, "{} {}", entry.short_oid, entry.subject)?;
            }
        } else {
            writeln!(io.out, "{}  {}", entry.oid, entry.subject)?;
        }
    }
    Ok(())
}

fn cmd_merge<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    // Support both ergonomic 1-arg (`bgit merge <branch>`) and 3-arg (`bgit merge <dir> <ours> <theirs>`).
    let (dir, ours_rev, theirs_rev, ergonomic_porcelain) = match (
        matches.positional(0),
        matches.positional(1),
        matches.positional(2),
    ) {
        (Some(d), Some(ours), Some(theirs)) => (resolve_path(cwd, d), ours, theirs, false),
        (Some(branch), None, _) => {
            let root = find_repo_root(cwd).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "not a git or bgit repository")
            })?;
            (root, "HEAD", branch, true)
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: bgit merge <branch> OR bgit merge <dir> <ours> <theirs>",
            ));
        }
    };
    let write_commit = ergonomic_porcelain || matches.flag("write-commit");
    let keep_worktree = matches.flag("keep-worktree");
    let no_sync = matches.flag("no-sync");

    let mut store = open_or_infect(&dir, alloc)?;
    let ours = store
        .resolve_rev(ours_rev)
        .map_err(|e| io::Error::other(format!("{e}")))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("rev not found: {ours_rev}")))?;
    let theirs = store
        .resolve_rev(theirs_rev)
        .map_err(|e| io::Error::other(format!("{e}")))?
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("rev not found: {theirs_rev}"))
        })?;

    let t0 = Instant::now();
    let result = {
        let cg_map = store.graph_mapping();
        let cg = cg_map
            .as_ref()
            .filter(|m| !m.bytes().is_empty())
            .and_then(|m| blaze_git::commit_graph::CommitGraph::from_layers(&[m.bytes()], alloc).ok());
        merge_commits(&mut store, cg.as_ref(), ours, theirs, MergeOpts::DEFAULT, alloc)
            .map_err(|e| io::Error::other(format!("{e}")))?
    };
    let elapsed = t0.elapsed();

    writeln!(
        io.out,
        "status={:?} tree={} subtrees_skipped={} subtrees_merged={} blobs_merged={} renames={} conflicts={} ({:.3} ms)",
        result.status,
        result.tree,
        result.stats.subtrees_skipped,
        result.stats.subtrees_merged,
        result.stats.blobs_merged,
        result.stats.renames_detected,
        result.conflicts.len(),
        elapsed.as_secs_f64() * 1000.0,
    )?;
    for c in &result.conflicts {
        writeln!(io.out, "  CONFLICT ({:?}): {}", c.kind, c.path.as_str())?;
    }

    if write_commit
        && (result.status == MergeStatus::MergedClean || result.status == MergeStatus::FastForward)
    {
        let commit_oid = if result.status == MergeStatus::FastForward {
            theirs
        } else {
            let (name, email, ts) = index_io::resolve_identity(&dir);
            let sig = index_io::make_signature(&name, &email, ts);
            let mut commit_buf = OwnVec::new_in(alloc);
            let msg = format!("Merge {theirs_rev} into {ours_rev}\n");
            encode_commit(result.tree, &[ours, theirs], sig, sig, &msg, &mut commit_buf);
            store
                .write_object(ObjectKind::Commit, &commit_buf)
                .map_err(|e| io::Error::other(format!("{e}")))?
        };

        let target_ref: Option<String> = if ours_rev != "HEAD"
            && store
                .resolve_ref(&canonical_branch_ref(ours_rev))
                .ok()
                .flatten()
                .is_some()
        {
            Some(canonical_branch_ref(ours_rev))
        } else if let Some(RefTarget::Symbolic(sym)) = store
            .read_head(alloc)
            .map_err(|e| io::Error::other(format!("{e}")))?
        {
            Some(sym.as_str().to_string())
        } else {
            None
        };

        if ergonomic_porcelain {
            let old_tree = index_io::read_commit_tree(&store, ours, alloc)?;
            let old_flat = index_io::flatten_tree(&store, old_tree, alloc)?;
            let new_flat = index_io::flatten_tree(&store, result.tree, alloc)?;
            index_io::checkout_tree_to_worktree(&dir, &store, &old_flat, &new_flat, false, alloc)?;
            let new_idx = index_io::index_from_flat_tree(&dir, &new_flat);
            index_io::write_index(&dir, &new_idx)?;
        }

        if let Some(ref sym) = target_ref {
            let _ = store
                .apply_ref_updates(
                    &[RefUpdate {
                        name: OwnString::from_str_in(sym.as_str(), alloc),
                        old: None,
                        new: commit_oid,
                    }],
                    false,
                    alloc,
                )
                .map_err(|e| io::Error::other(format!("{e}")))?;
        }
        disk::mirror_overlay_objects_to_git(&store, &dir, alloc);
        store.flush()?;
        if let Some(ref sym) = target_ref {
            disk::mirror_ref_to_git(&dir, sym, commit_oid);
        }
        writeln!(io.out, "commit={commit_oid}")?;

        drop(store);
        on_branch_merged(
            &dir,
            target_ref.as_deref(),
            theirs_rev,
            keep_worktree,
            no_sync,
            alloc,
            io,
        )?;
    } else if result.status == MergeStatus::MergedClean || result.status == MergeStatus::Conflicted
    {
        store.flush()?;
    }
    Ok(())
}

fn cmd_status<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    default_color: bool,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Long,
        Short,
        Porcelain,
        Prompt,
    }

    let dir = resolve_path(cwd, matches.positional(0).unwrap_or("."));
    let show_branch = matches.flag("branch");
    let show_timing = matches.flag("timing");

    let mode = if let Some(ver) = matches.opt("porcelain") {
        match ver {
            "v1" | "1" => Mode::Porcelain,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unsupported --porcelain version `{other}` (supported: v1)"),
                ));
            }
        }
    } else if matches.flag("prompt") {
        Mode::Prompt
    } else if matches.flag("short") {
        Mode::Short
    } else {
        Mode::Long
    };

    let untracked = match matches.opt("untracked-files").map(|v| v.strip_prefix('=').unwrap_or(v)) {
        None | Some("normal") => UntrackedMode::Normal,
        Some("no") => UntrackedMode::Off,
        Some("all") => UntrackedMode::All,
        Some(other) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid --untracked-files mode `{other}` (use no, normal, or all)"),
            ));
        }
    };

    let color_override = if matches.flag("no-color") {
        Some(false)
    } else {
        match matches.opt("color") {
            None | Some("auto") => None,
            Some("always") => Some(true),
            Some("never") => Some(false),
            Some(other) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid --color value `{other}` (use always, auto, or never)"),
                ));
            }
        }
    };

    let detail = if mode == Mode::Prompt { Detail::Counts } else { Detail::Full };
    let opts = StatusOpts { untracked, detail, ..StatusOpts::FULL };
    let (status, timing) = run_status(&dir, opts, alloc)?;

    let use_color = match mode {
        Mode::Porcelain | Mode::Prompt => false,
        Mode::Long | Mode::Short => color_override.unwrap_or(default_color),
    };

    let mut out = String::new();
    match mode {
        Mode::Long => {
            let _ = format_long_status(&status, use_color, &mut out);
        }
        Mode::Short => {
            let _ = format_short_status(&status, show_branch, use_color, &mut out);
        }
        Mode::Porcelain => {
            let _ = format_porcelain_v1(&status, show_branch, &mut out);
        }
        Mode::Prompt => {
            let _ = format_status(&status, &PromptStyle::STARSHIP, &mut out);
            out.push('\n');
        }
    }
    write!(io.out, "{out}")?;

    if show_timing {
        writeln!(
            io.err,
            "timing: total={:.2}ms machine={:.2}ms list_dirs={:.2}ms(n={}) stat_paths={:.2}ms(n={}) read_files={:.2}ms(n={}) read_with_stat={:.2}ms links={:.2}ms ranges={:.2}ms rounds={}",
            timing.total_us as f64 / 1000.0,
            timing.machine_us as f64 / 1000.0,
            timing.list_dirs_us as f64 / 1000.0,
            timing.list_dirs_count,
            timing.stat_paths_us as f64 / 1000.0,
            timing.stat_paths_count,
            timing.read_files_us as f64 / 1000.0,
            timing.read_files_count,
            timing.read_with_stat_us as f64 / 1000.0,
            timing.read_links_us as f64 / 1000.0,
            timing.read_ranges_us as f64 / 1000.0,
            timing.rounds,
        )?;
    }

    Ok(())
}

fn cmd_bench<'a>(repo_dir: &Path, alloc: Alloc<'a>, io: &mut IoStreams<'_>) -> io::Result<()> {
    let store = open_or_infect(repo_dir, alloc)?;
    let mut oids = OwnVec::new_in(alloc);
    store.list_oids(&mut oids).map_err(|e| io::Error::other(format!("{e}")))?;
    let mut refs = OwnVec::new_in(alloc);
    store.list_refs("", alloc, &mut refs).map_err(|e| io::Error::other(format!("{e}")))?;

    writeln!(
        io.out,
        "=== bgit macOS floor benchmark on {} (objects={}, refs={}) ===",
        repo_dir.display(),
        oids.len(),
        refs.len()
    )?;

    let mut bgit_refs_us = [0u64; 5];
    for slot in &mut bgit_refs_us {
        let t0 = Instant::now();
        let mut r = OwnVec::new_in(alloc);
        store.list_refs("", alloc, &mut r).map_err(|e| io::Error::other(format!("{e}")))?;
        *slot = t0.elapsed().as_micros() as u64;
    }
    bgit_refs_us.sort_unstable();

    let mut git_refs_us = [0u64; 5];
    for slot in &mut git_refs_us {
        let t0 = Instant::now();
        let _ = Command::new("git").arg("-C").arg(repo_dir).arg("show-ref").output()?;
        *slot = t0.elapsed().as_micros() as u64;
    }
    git_refs_us.sort_unstable();
    writeln!(
        io.out,
        "ls-refs / show-ref:     bgit = {:>6} us  vs  git = {:>6} us  ({:.1}x faster)",
        bgit_refs_us[2],
        git_refs_us[2],
        git_refs_us[2] as f64 / bgit_refs_us[2].max(1) as f64
    )?;

    let sample_n = oids.len().min(100);
    if sample_n > 0 {
        let mut buf = OwnVec::new_in(alloc);
        let t0 = Instant::now();
        for oid in &oids[..sample_n] {
            let _ = store.read_object(oid, &mut buf).map_err(|e| io::Error::other(format!("{e}")))?;
        }
        let bgit_obj_us = t0.elapsed().as_micros() as u64;

        let mut batch_in = String::new();
        for oid in &oids[..sample_n] {
            batch_in.push_str(&format!("{oid}\n"));
        }
        let t1 = Instant::now();
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repo_dir)
            .args(["cat-file", "--batch"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(batch_in.as_bytes());
        }
        let _ = child.wait()?;
        let git_obj_us = t1.elapsed().as_micros() as u64;
        writeln!(
            io.out,
            "read {sample_n} objects:       bgit = {:>6} us  vs  git = {:>6} us  ({:.1}x faster)",
            bgit_obj_us,
            git_obj_us,
            git_obj_us as f64 / bgit_obj_us.max(1) as f64
        )?;
    }

    if !oids.is_empty() {
        let mut pack_out = OwnVec::new_in(alloc);
        let t0 = Instant::now();
        let _ = store
            .write_pack_parallel(&oids, blaze_git::pack_write::PackWriteOpts::FAST, &mut pack_out)
            .map_err(|e| io::Error::other(format!("{e}")))?;
        let bgit_pack_us = t0.elapsed().as_micros() as u64;

        let t1 = Instant::now();
        let _ = Command::new("git")
            .arg("-C")
            .arg(repo_dir)
            .args(["pack-objects", "--all", "--stdout", "-q"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()?;
        let git_pack_us = t1.elapsed().as_micros() as u64;
        writeln!(
            io.out,
            "pack all ({:>4} objs):   bgit = {:>6} us  vs  git = {:>6} us  ({:.1}x faster, {} B)",
            oids.len(),
            bgit_pack_us,
            git_pack_us,
            git_pack_us as f64 / bgit_pack_us.max(1) as f64,
            pack_out.len(),
        )?;
    }

    if let Some(head_oid) =
        store.resolve_ref("HEAD").map_err(|e| io::Error::other(format!("{e}")))?
    {
        let mut buf = OwnVec::new_in(alloc);
        if store.read_object(&head_oid, &mut buf).ok().flatten() == Some(ObjectKind::Commit)
            && let Ok(c) = Commit::parse(&buf)
            && let Some(parent_oid) = c.parents().next()
        {
            let cg = store.commit_graph();
            let t0 = Instant::now();
            let _ = find_merge_bases(
                &store,
                cg.as_ref(),
                head_oid,
                parent_oid,
                MergeLimits::SHIPPED,
                alloc,
            )
            .map_err(|e| io::Error::other(format!("{e}")))?;
            let bgit_mb_us = t0.elapsed().as_micros() as u64;

            let t1 = Instant::now();
            let _ = Command::new("git")
                .arg("-C")
                .arg(repo_dir)
                .args(["merge-base", &format!("{head_oid}"), &format!("{parent_oid}")])
                .output()?;
            let git_mb_us = t1.elapsed().as_micros() as u64;
            writeln!(
                io.out,
                "merge-base (CGPH):      bgit = {:>6} us  vs  git = {:>6} us  ({:.1}x faster)",
                bgit_mb_us,
                git_mb_us,
                git_mb_us as f64 / bgit_mb_us.max(1) as f64
            )?;
        }
    }
    Ok(())
}
