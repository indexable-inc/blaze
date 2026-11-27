//! Native worktree driver over `blaze-git`'s sans-IO `worktree` module.
//!
//! - `bgit worktree add [repo] <path> <branch> [--from <rev>] [--track <branch>] [--vfs] [--no-auto-delete]`
//! - `bgit worktree list [repo]`
//! - `bgit worktree sync [repo] [--dry-run] [--force]`
//! - `bgit worktree watch [repo] [--interval-ms N]`
//! - `bgit worktree remove [repo] <path-or-branch> [--force]`
//! - Post-merge hook (`on_branch_merged`) called automatically by `bgit merge --write-commit`:
//!   auto-deletes the merged branch's worktree folder by default (if clean) and auto-merges
//!   downstream worktrees tracking the target branch!

use std::{
    fs,
    io::{self, Write},
    os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink},
    path::Path,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use blaze_git::{
    ShortOid,
    commit_graph::CommitGraph,
    merge::{MergeOpts, MergeStatus},
    objects::TreeIter,
    slab::SlabCompression,
    storage::{ObjectStore, RefStore, RefTarget, RefUpdate, Signature, encode_commit},
    worktree::{
        DiskFile, FsAction, WorktreeRecord, WorktreeSyncDecision, commit_tree_oid,
        decode_worktrees, encode_worktrees, evaluate_worktree_sync, plan_worktree_fs_update,
    },
};
use blaze_git_fetch::{Kind as ObjectKind, Oid};
use blaze_own::{AVec, Alloc, String as OwnString, Vec as OwnVec};
use bstd::cli::Matches;

use crate::{
    IoStreams, WorktreeInfo,
    disk::{
        BgitDiskStore, infect_repo_on_disk, mirror_overlay_objects_to_git, mirror_ref_to_git,
        open_or_infect, resolve_common_git_dir,
    },
    print_help, resolve_bgit_binary, resolve_path,
    vfs_driver::{cmd_unmount_path, spawn_background_mount},
};

const WORKTREES_FILE: &str = "worktrees.bgwt";
const MOST_WORKTREES: usize = 4_096;

pub(crate) fn canonical_branch_ref(name: &str) -> String {
    if name.starts_with("refs/") { name.to_string() } else { format!("refs/heads/{name}") }
}

fn short_branch_name(ref_name: &str) -> &str {
    ref_name.strip_prefix("refs/heads/").unwrap_or(ref_name)
}

fn load_worktrees<'a>(
    bgit_dir: &Path,
    alloc: Alloc<'a>,
) -> io::Result<AVec<'a, WorktreeRecord<'a>>> {
    let path = bgit_dir.join(WORKTREES_FILE);
    if !path.is_file() {
        return Ok(OwnVec::new_in(alloc));
    }
    let bytes = fs::read(&path)?;
    decode_worktrees(&bytes, MOST_WORKTREES, alloc)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad worktrees.bgwt: {e}")))
}

fn save_worktrees<'a>(
    bgit_dir: &Path,
    records: &[WorktreeRecord<'a>],
    alloc: Alloc<'a>,
) -> io::Result<()> {
    let mut buf = OwnVec::new_in(alloc);
    encode_worktrees(records, &mut buf);
    let path = bgit_dir.join(WORKTREES_FILE);
    let tmp = bgit_dir.join("worktrees.bgwt.tmp");
    fs::write(&tmp, buf.as_slice())?;
    fs::rename(tmp, path)
}

/// Entry point for `bgit worktree` (alias `bgit wt`).
pub(crate) fn cmd_worktree<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    use_color: bool,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    match matches.command_path().get(2).copied() {
        None => {
            print_help(matches.active_command(), matches.command_path(), use_color, alloc, io.out);
            Ok(())
        }
        Some("add") => cmd_wt_add(matches, cwd, alloc, io),
        Some("list") => cmd_wt_list(matches, cwd, alloc, io),
        Some("sync") => cmd_wt_sync(matches, cwd, alloc, io),
        Some("watch") => cmd_wt_watch(matches, cwd, alloc, io),
        Some("remove") => cmd_wt_remove(matches, cwd, alloc, io),
        Some(other) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown worktree subcommand `{other}` (run `bgit worktree --help`)"),
        )),
    }
}

/// Returns registered bgit worktrees for `repo_root` if `.bgit/worktrees.bgwt` exists.
pub(crate) fn query_worktrees_in_repo<'a>(
    repo_root: &Path,
    alloc: Alloc<'a>,
) -> io::Result<Option<Vec<WorktreeInfo>>> {
    let bgit_dir = repo_root.join(".bgit");
    if !bgit_dir.join(WORKTREES_FILE).is_file() {
        return Ok(None);
    }
    let records = load_worktrees(&bgit_dir, alloc)?;
    let mut out = Vec::with_capacity(records.len());
    for r in records.iter() {
        out.push(WorktreeInfo {
            path: r.path.as_str().to_string(),
            branch: short_branch_name(r.branch.as_str()).to_string(),
            track_branch: short_branch_name(r.track_branch.as_str()).to_string(),
            head_short: format!("{}", ShortOid::from_oid(r.head_commit, 7)),
            is_vfs: r.is_vfs,
            auto_delete_on_merge: r.auto_delete_on_merge,
        });
    }
    Ok(Some(out))
}

fn cmd_wt_add<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let from_rev = matches.opt("from");
    let track_rev = matches.opt("track");
    let use_vfs = matches.flag("vfs");
    let auto_delete = !matches.flag("no-auto-delete");

    let pos = matches.positionals();
    let (repo_dir, wt_path_raw, branch_arg) = match pos.len() {
        2 => (cwd.to_path_buf(), resolve_path(cwd, pos[0]), pos[1]),
        3 => (resolve_path(cwd, pos[0]), resolve_path(cwd, pos[1]), pos[2]),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: bgit worktree add [repo] <path> <branch> [--from <rev>] [--track <branch>] [--vfs]",
            ));
        }
    };

    let canon_repo = fs::canonicalize(&repo_dir)?;
    let mut store = open_or_infect(&canon_repo, alloc)?;

    // Determine default base & tracked branch (defaults to current symbolic HEAD branch).
    let head_branch = match store
        .read_head(alloc)
        .map_err(|e| io::Error::other(format!("{e}")))?
    {
        Some(RefTarget::Symbolic(sym)) => Some(sym.as_str().to_string()),
        _ => None,
    };

    let branch_ref = canonical_branch_ref(branch_arg);
    let track_ref = match track_rev {
        Some(t) => canonical_branch_ref(t),
        None => match from_rev {
            Some(f) if f != "HEAD" => canonical_branch_ref(f),
            _ => head_branch.clone().unwrap_or_else(|| "refs/heads/main".to_string()),
        },
    };

    // Resolve or create branch commit OID.
    let (branch_oid, base_commit) = if let Some(existing) = store
        .resolve_ref(&branch_ref)
        .map_err(|e| io::Error::other(format!("{e}")))?
    {
        let base = store
            .resolve_ref(&track_ref)
            .ok()
            .flatten()
            .unwrap_or(existing);
        (existing, base)
    } else {
        let base_rev = from_rev.unwrap_or("HEAD");
        let base_oid = store
            .resolve_rev(base_rev)
            .map_err(|e| io::Error::other(format!("{e}")))?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("base rev not found: {base_rev}"))
            })?;
        let _ = store
            .apply_ref_updates(
                &[RefUpdate {
                    name: OwnString::from_str_in(&branch_ref, alloc),
                    old: None,
                    new: base_oid,
                }],
                false,
                alloc,
            )
            .map_err(|e| io::Error::other(format!("{e}")))?;
        store.flush()?;
        mirror_ref_to_git(&canon_repo, &branch_ref, base_oid);
        (base_oid, base_oid)
    };

    let head_tree = commit_tree_oid(&store, branch_oid, alloc)
        .map_err(|e| io::Error::other(format!("{e}")))?;

    fs::create_dir_all(&wt_path_raw)?;
    let canon_wt = fs::canonicalize(&wt_path_raw)?;

    // Check for duplicate path or duplicate checked-out branch.
    let mut records = load_worktrees(&store.bgit_dir, alloc)?;
    for r in records.iter() {
        if Path::new(r.path.as_str()) == canon_wt {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("worktree already registered at {}", canon_wt.display()),
            ));
        }
        if r.branch.as_str() == branch_ref {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "branch {} is already checked out in worktree {}",
                    short_branch_name(&branch_ref),
                    r.path.display_str_compat()
                ),
            ));
        }
    }

    if use_vfs {
        spawn_background_mount(
            &canon_repo,
            &canon_wt,
            short_branch_name(&branch_ref),
            false,
            io.out,
        )?;
    } else {
        let count = checkout_tree_to_dir(&store, head_tree, &canon_wt, alloc)?;
        writeln!(
            io.out,
            "Created worktree {} on {} (tracks {}, {} files, auto-delete-on-merge={})",
            canon_wt.display(),
            short_branch_name(&branch_ref),
            short_branch_name(&track_ref),
            count,
            auto_delete
        )?;
    }

    let name = canon_wt
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("worktree")
        .to_string();
    records.push(WorktreeRecord {
        name: OwnString::from_str_in(&name, alloc),
        path: OwnString::from_str_in(canon_wt.to_string_lossy().as_ref(), alloc),
        branch: OwnString::from_str_in(&branch_ref, alloc),
        track_branch: OwnString::from_str_in(&track_ref, alloc),
        base_commit,
        head_commit: branch_oid,
        head_tree,
        is_vfs: use_vfs,
        auto_delete_on_merge: auto_delete,
    });
    save_worktrees(&store.bgit_dir, &records, alloc)?;
    // Install lightweight `.git/hooks/reference-transaction` hook so plain `git merge` / `git pull`
    // / `git commit` automatically updates tracking worktrees and deletes merged worktrees with
    // zero daemon and zero manual `sync`!
    let _ = install_git_worktree_hooks(&canon_repo);
    Ok(())
}

trait StrDisplayCompat {
    fn display_str_compat(&self) -> &str;
}
impl StrDisplayCompat for blaze_own::AString<'_> {
    fn display_str_compat(&self) -> &str {
        self.as_str()
    }
}

fn cmd_wt_list<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let repo_dir = resolve_path(cwd, matches.positional(0).unwrap_or("."));
    let store = open_or_infect(&repo_dir, alloc)?;
    let records = load_worktrees(&store.bgit_dir, alloc)?;
    if records.is_empty() {
        writeln!(io.out, "No registered bgit worktrees in {}", repo_dir.display())?;
        return Ok(());
    }
    for r in records.iter() {
        let mode = if r.is_vfs { "vfs" } else { "disk" };
        let del = if r.auto_delete_on_merge { "auto-delete" } else { "keep" };
        writeln!(
            io.out,
            "{}\t{}\t-> tracks {}\t({})\t[{mode}, {del}]",
            r.path.as_str(),
            short_branch_name(r.branch.as_str()),
            short_branch_name(r.track_branch.as_str()),
            ShortOid::from_oid(r.head_commit, 7),
        )?;
    }
    Ok(())
}

fn cmd_wt_sync<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let repo_dir = resolve_path(cwd, matches.positional(0).unwrap_or("."));
    let dry_run = matches.flag("dry-run");
    let force = matches.flag("force");
    let from_hook = matches.flag("from-hook");
    sync_all_worktrees(&repo_dir, None, dry_run, force, from_hook, alloc, io)?;
    Ok(())
}

fn cmd_wt_watch<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let repo_dir = resolve_path(cwd, matches.positional(0).unwrap_or("."));
    let interval_ms = matches
        .opt_parse::<u64>("interval-ms")
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?
        .unwrap_or(250)
        .clamp(50, 10_000);

    let canon_repo = fs::canonicalize(&repo_dir)?;
    writeln!(
        io.out,
        "Watching {} for tracked-branch updates (poll={}ms, auto-merge + auto-delete-on-merge active)...",
        canon_repo.display(),
        interval_ms
    )?;
    let _ = io.out.flush();

    let mut last_fp = ref_tree_fingerprint(&canon_repo);
    // Run an initial sync immediately on startup.
    let _ = sync_all_worktrees(&canon_repo, None, false, false, false, alloc, io);
    loop {
        thread::sleep(Duration::from_millis(interval_ms));
        let fp = ref_tree_fingerprint(&canon_repo);
        if fp != last_fp {
            if let Err(e) = sync_all_worktrees(&canon_repo, None, false, false, true, alloc, io) {
                let _ = writeln!(io.err, "worktree watch sync error: {e}");
            }
            let _ = io.out.flush();
            last_fp = ref_tree_fingerprint(&canon_repo);
        }
    }
}

fn cmd_wt_remove<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let force = matches.flag("force");
    let pos = matches.positionals();
    let (repo_dir, target) = match pos.len() {
        1 => (cwd.to_path_buf(), pos[0]),
        2 => (resolve_path(cwd, pos[0]), pos[1]),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: bgit worktree remove [repo] <path-or-branch> [--force]",
            ));
        }
    };

    let store = open_or_infect(&repo_dir, alloc)?;
    let records = load_worktrees(&store.bgit_dir, alloc)?;
    let target_canon = fs::canonicalize(resolve_path(cwd, target)).ok();
    let target_ref = canonical_branch_ref(target);

    let mut kept: AVec<'a, WorktreeRecord<'a>> = OwnVec::with_capacity_in(records.len(), alloc);
    let mut removed_any = false;
    for r in records {
        let matches_path = target_canon
            .as_ref()
            .is_some_and(|c| Path::new(r.path.as_str()) == c.as_path())
            || r.path.as_str() == target
            || r.name.as_str() == target;
        let matches_branch = r.branch.as_str() == target_ref || r.branch.as_str() == target;
        if matches_path || matches_branch {
            remove_worktree_folder(&store, &r, force, alloc, io.out)?;
            writeln!(
                io.out,
                "Removed worktree {} ({})",
                r.path.as_str(),
                short_branch_name(r.branch.as_str())
            )?;
            removed_any = true;
        } else {
            kept.push(r);
        }
    }
    if !removed_any {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no registered worktree matching `{target}`"),
        ));
    }
    save_worktrees(&store.bgit_dir, &kept, alloc)
}

/// Post-merge hook called automatically by `bgit merge --write-commit`:
/// 1. Auto-deletes any registered worktree for `merged_branch_ref` (unless `keep_worktree` or dirty).
/// 2. Auto-merges/syncs any other registered worktree tracking `into_branch_ref` (unless `no_sync`).
pub(crate) fn on_branch_merged<'a>(
    repo_dir: &Path,
    into_branch_ref: Option<&str>,
    merged_branch_ref: &str,
    keep_worktree: bool,
    no_sync: bool,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let bgit_dir = repo_dir.join(".bgit");
    if !bgit_dir.join(WORKTREES_FILE).is_file() {
        return Ok(());
    }

    if !keep_worktree {
        let store = open_or_infect(repo_dir, alloc)?;
        let records = load_worktrees(&store.bgit_dir, alloc)?;
        let merged_canon = canonical_branch_ref(merged_branch_ref);
        let mut kept: AVec<'a, WorktreeRecord<'a>> = OwnVec::with_capacity_in(records.len(), alloc);
        let mut changed = false;

        for r in records {
            if r.auto_delete_on_merge && r.branch.as_str() == merged_canon {
                match remove_worktree_folder(&store, &r, false, alloc, io.out) {
                    Ok(()) => {
                        writeln!(
                            io.out,
                            "Auto-deleted merged worktree {} ({} merged into {})",
                            r.path.as_str(),
                            short_branch_name(r.branch.as_str()),
                            into_branch_ref.map_or("target", short_branch_name),
                        )?;
                        changed = true;
                    }
                    Err(e) => {
                        let _ = writeln!(
                            io.err,
                            "warning: kept merged worktree {} ({e})",
                            r.path.as_str()
                        );
                        kept.push(r);
                    }
                }
            } else {
                kept.push(r);
            }
        }
        if changed {
            save_worktrees(&store.bgit_dir, &kept, alloc)?;
        }
    }

    if !no_sync && let Some(target_ref) = into_branch_ref {
        sync_all_worktrees(repo_dir, Some(target_ref), false, false, false, alloc, io)?;
    }
    Ok(())
}

/// Sync all registered worktrees (or only those tracking `only_tracking_ref`).
pub(crate) fn sync_all_worktrees<'a>(
    repo_dir: &Path,
    only_tracking_ref: Option<&str>,
    dry_run: bool,
    force: bool,
    force_git_refresh: bool,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    // Fast-path: if no worktrees are registered, do zero work (< 0.5 ms in Git hooks!).
    let wt_file = repo_dir.join(".bgit").join(WORKTREES_FILE);
    if !wt_file.is_file() {
        return Ok(());
    }

    // Refresh `.bgit` from `.git` when triggered by a Git hook or when `.git` refs are newer!
    refresh_bgit_if_git_refs_newer(repo_dir, force_git_refresh, alloc)?;

    let mut store = open_or_infect(repo_dir, alloc)?;
    let records = load_worktrees(&store.bgit_dir, alloc)?;
    if records.is_empty() {
        return Ok(());
    }

    let mut updated_records: AVec<'a, WorktreeRecord<'a>> =
        OwnVec::with_capacity_in(records.len(), alloc);
    let mut store_dirty = false;
    let mut manifest_dirty = false;

    for mut rec in records {
        if let Some(filter_ref) = only_tracking_ref
            && rec.track_branch.as_str() != filter_ref
        {
            updated_records.push(rec);
            continue;
        }

        let decision = {
            let cg_map = store.graph_mapping();
            let cg = cg_map
                .as_ref()
                .filter(|m| !m.bytes().is_empty())
                .and_then(|m| CommitGraph::from_layers(&[m.bytes()], alloc).ok());
            evaluate_worktree_sync(&rec, &mut store, cg.as_ref(), MergeOpts::DEFAULT, alloc)
                .map_err(|e| io::Error::other(format!("{e}")))?
        };

        match decision {
            WorktreeSyncDecision::UpToDate => {
                updated_records.push(rec);
            }
            WorktreeSyncDecision::MissingBranch => {
                let _ = writeln!(
                    io.err,
                    "worktree {}: branch {} no longer exists",
                    rec.path.as_str(),
                    short_branch_name(rec.branch.as_str())
                );
                updated_records.push(rec);
            }
            WorktreeSyncDecision::MissingTrackedBranch => {
                let _ = writeln!(
                    io.err,
                    "worktree {}: tracked branch {} does not exist",
                    rec.path.as_str(),
                    short_branch_name(rec.track_branch.as_str())
                );
                updated_records.push(rec);
            }
            WorktreeSyncDecision::MergedIntoTracked { .. } => {
                if dry_run {
                    writeln!(
                        io.out,
                        "[dry-run] would auto-delete merged worktree {} ({} merged into {})",
                        rec.path.as_str(),
                        short_branch_name(rec.branch.as_str()),
                        short_branch_name(rec.track_branch.as_str()),
                    )?;
                    updated_records.push(rec);
                } else {
                    match remove_worktree_folder(&store, &rec, force, alloc, io.out) {
                        Ok(()) => {
                            writeln!(
                                io.out,
                                "Auto-deleted merged worktree {} ({} merged into {})",
                                rec.path.as_str(),
                                short_branch_name(rec.branch.as_str()),
                                short_branch_name(rec.track_branch.as_str()),
                            )?;
                            manifest_dirty = true;
                        }
                        Err(e) => {
                            let _ = writeln!(
                                io.err,
                                "warning: kept merged worktree {} ({e})",
                                rec.path.as_str()
                            );
                            updated_records.push(rec);
                        }
                    }
                }
            }
            WorktreeSyncDecision::FastForward { old_commit, new_commit, old_tree, new_tree } => {
                let wt_dir = Path::new(rec.path.as_str());
                let plan = plan_worktree_fs_update(
                    &store,
                    old_tree,
                    new_tree,
                    |rel| read_disk_file(wt_dir, rel, alloc),
                    MergeOpts::DEFAULT,
                    alloc,
                )
                .map_err(|e| io::Error::other(format!("{e}")))?;

                if !plan.is_clean() {
                    let _ = writeln!(
                        io.err,
                        "worktree {} ({}): skipped fast-forward due to {} filesystem conflict(s):",
                        rec.path.as_str(),
                        short_branch_name(rec.branch.as_str()),
                        plan.conflicts.len()
                    );
                    for c in &plan.conflicts {
                        let _ = writeln!(io.err, "  FS CONFLICT ({:?}): {}", c.kind, c.path.as_str());
                    }
                    updated_records.push(rec);
                    continue;
                }

                if dry_run {
                    writeln!(
                        io.out,
                        "[dry-run] fast-forward {} ({} -> {}, {} fs ops, {} auto-merged dirty files)",
                        rec.path.as_str(),
                        ShortOid::from_oid(old_commit, 7),
                        ShortOid::from_oid(new_commit, 7),
                        plan.actions.len(),
                        plan.auto_merged_files,
                    )?;
                    updated_records.push(rec);
                    continue;
                }

                apply_fs_actions(wt_dir, &plan.actions)?;
                let _ = store
                    .apply_ref_updates(
                        &[RefUpdate {
                            name: OwnString::from_str_in(rec.branch.as_str(), alloc),
                            old: None,
                            new: new_commit,
                        }],
                        false,
                        alloc,
                    )
                    .map_err(|e| io::Error::other(format!("{e}")))?;
                mirror_ref_to_git(repo_dir, rec.branch.as_str(), new_commit);
                store_dirty = true;
                if rec.head_commit == rec.base_commit {
                    rec.base_commit = new_commit;
                }
                rec.head_commit = new_commit;
                rec.head_tree = new_tree;
                manifest_dirty = true;
                writeln!(
                    io.out,
                    "Synced worktree {} ({} fast-forwarded to {}, {} fs ops, {} auto-merged dirty files)",
                    rec.path.as_str(),
                    short_branch_name(rec.branch.as_str()),
                    ShortOid::from_oid(new_commit, 7),
                    plan.actions.len(),
                    plan.auto_merged_files,
                )?;
                updated_records.push(rec);
            }
            WorktreeSyncDecision::AutoMerge {
                old_commit,
                track_oid,
                old_tree,
                merged_tree,
                status,
                conflicts,
            } => {
                if status != MergeStatus::MergedClean {
                    let _ = writeln!(
                        io.err,
                        "worktree {} ({}): auto-merge from {} had {} tree conflict(s), leaving untouched:",
                        rec.path.as_str(),
                        short_branch_name(rec.branch.as_str()),
                        short_branch_name(rec.track_branch.as_str()),
                        conflicts.len()
                    );
                    for c in &conflicts {
                        let _ = writeln!(io.err, "  TREE CONFLICT ({:?}): {}", c.kind, c.path.as_str());
                    }
                    updated_records.push(rec);
                    continue;
                }

                let wt_dir = Path::new(rec.path.as_str());
                let plan = plan_worktree_fs_update(
                    &store,
                    old_tree,
                    merged_tree,
                    |rel| read_disk_file(wt_dir, rel, alloc),
                    MergeOpts::DEFAULT,
                    alloc,
                )
                .map_err(|e| io::Error::other(format!("{e}")))?;

                if !plan.is_clean() {
                    let _ = writeln!(
                        io.err,
                        "worktree {} ({}): auto-merge had {} local filesystem conflict(s), leaving untouched:",
                        rec.path.as_str(),
                        short_branch_name(rec.branch.as_str()),
                        plan.conflicts.len()
                    );
                    for c in &plan.conflicts {
                        let _ = writeln!(io.err, "  FS CONFLICT ({:?}): {}", c.kind, c.path.as_str());
                    }
                    updated_records.push(rec);
                    continue;
                }

                if dry_run {
                    writeln!(
                        io.out,
                        "[dry-run] auto-merge {} into {} ({} fs ops, {} auto-merged dirty files)",
                        short_branch_name(rec.track_branch.as_str()),
                        rec.path.as_str(),
                        plan.actions.len(),
                        plan.auto_merged_files,
                    )?;
                    updated_records.push(rec);
                    continue;
                }

                // Write merge commit on the worktree's branch and project actions onto disk!
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64);
                let sig =
                    Signature { name: "bgit", email: "bgit@ix.dev", timestamp: now, tz: "+0000" };
                let msg = format!(
                    "Auto-merge {} into {}\n",
                    short_branch_name(rec.track_branch.as_str()),
                    short_branch_name(rec.branch.as_str())
                );
                let mut commit_buf = OwnVec::new_in(alloc);
                encode_commit(
                    merged_tree,
                    &[old_commit, track_oid],
                    sig,
                    sig,
                    &msg,
                    &mut commit_buf,
                );
                let merge_oid = store
                    .write_object(ObjectKind::Commit, &commit_buf)
                    .map_err(|e| io::Error::other(format!("{e}")))?;
                let _ = store
                    .apply_ref_updates(
                        &[RefUpdate {
                            name: OwnString::from_str_in(rec.branch.as_str(), alloc),
                            old: None,
                            new: merge_oid,
                        }],
                        false,
                        alloc,
                    )
                    .map_err(|e| io::Error::other(format!("{e}")))?;
                apply_fs_actions(wt_dir, &plan.actions)?;
                mirror_overlay_objects_to_git(&store, repo_dir, alloc);
                mirror_ref_to_git(repo_dir, rec.branch.as_str(), merge_oid);
                store_dirty = true;
                rec.head_commit = merge_oid;
                rec.head_tree = merged_tree;
                manifest_dirty = true;
                writeln!(
                    io.out,
                    "Auto-merged {} into worktree {} ({} -> {}, {} fs ops, {} auto-merged dirty files)",
                    short_branch_name(rec.track_branch.as_str()),
                    rec.path.as_str(),
                    short_branch_name(rec.branch.as_str()),
                    ShortOid::from_oid(merge_oid, 7),
                    plan.actions.len(),
                    plan.auto_merged_files,
                )?;
                updated_records.push(rec);
            }
        }
    }

    if store_dirty {
        store.flush()?;
    }
    if manifest_dirty {
        save_worktrees(&store.bgit_dir, &updated_records, alloc)?;
    }
    Ok(())
}

fn remove_worktree_folder<'a>(
    store: &BgitDiskStore<'a>,
    rec: &WorktreeRecord<'a>,
    force: bool,
    alloc: Alloc<'a>,
    out: &mut dyn Write,
) -> io::Result<()> {
    let wt_path = Path::new(rec.path.as_str());
    if !wt_path.exists() {
        return Ok(());
    }
    if !force && !rec.is_vfs {
        let dirty = count_dirty_against_tree(store, rec.head_tree, wt_path, alloc)?;
        if dirty > 0 {
            return Err(io::Error::other(format!(
                "{dirty} uncommitted change(s) on disk; commit or pass --force"
            )));
        }
    }
    if rec.is_vfs {
        let _ = cmd_unmount_path(wt_path, force, out);
    }
    if wt_path.exists() {
        fs::remove_dir_all(wt_path)?;
    }
    Ok(())
}

fn read_disk_file<'a>(wt_dir: &Path, rel: &str, alloc: Alloc<'a>) -> Option<DiskFile<'a>> {
    let full = wt_dir.join(rel);
    let meta = fs::symlink_metadata(&full).ok()?;
    let mode = meta.mode();
    let bytes = if meta.file_type().is_symlink() {
        let target = fs::read_link(&full).ok()?;
        let raw = target.as_os_str().as_encoded_bytes();
        let mut v = OwnVec::with_capacity_in(raw.len(), alloc);
        v.extend_from_slice(raw);
        v
    } else if meta.is_file() {
        let raw = fs::read(&full).ok()?;
        let mut v = OwnVec::with_capacity_in(raw.len(), alloc);
        v.extend_from_slice(&raw);
        v
    } else {
        return None;
    };
    Some(DiskFile { mode, bytes })
}

fn apply_fs_actions(wt_dir: &Path, actions: &[FsAction<'_>]) -> io::Result<()> {
    for action in actions {
        match action {
            FsAction::WriteFile { path, mode, bytes } => {
                let full = wt_dir.join(path.as_str());
                if let Some(parent) = full.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&full, bytes.as_slice())?;
                let perms = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
                let _ = fs::set_permissions(&full, fs::Permissions::from_mode(perms));
            }
            FsAction::WriteSymlink { path, target } => {
                let full = wt_dir.join(path.as_str());
                if let Some(parent) = full.parent() {
                    fs::create_dir_all(parent)?;
                }
                let _ = fs::remove_file(&full);
                if let Ok(t) = core::str::from_utf8(target.as_slice()) {
                    symlink(t, &full)?;
                }
            }
            FsAction::DeleteFile { path } => {
                let full = wt_dir.join(path.as_str());
                if full.exists() || fs::symlink_metadata(&full).is_ok() {
                    let _ = fs::remove_file(&full);
                }
                // Prune empty parent directories inside `wt_dir`.
                let mut cur = full.parent();
                while let Some(dir) = cur {
                    if dir == wt_dir {
                        break;
                    }
                    if fs::remove_dir(dir).is_err() {
                        break;
                    }
                    cur = dir.parent();
                }
            }
        }
    }
    Ok(())
}

fn checkout_tree_to_dir<'a>(
    store: &BgitDiskStore<'a>,
    tree_oid: Oid,
    dir: &Path,
    alloc: Alloc<'a>,
) -> io::Result<u32> {
    let mut raw = OwnVec::new_in(alloc);
    if store
        .read_object(&tree_oid, &mut raw)
        .map_err(|e| io::Error::other(format!("{e}")))?
        != Some(ObjectKind::Tree)
    {
        return Ok(0);
    }
    fs::create_dir_all(dir)?;
    let mut count = 0u32;
    let mut iter = TreeIter::new(tree_oid, &raw);
    while let Some(te) = iter.next_entry().map_err(|e| io::Error::other(format!("{e}")))? {
        let Ok(name) = core::str::from_utf8(te.name) else {
            continue;
        };
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            continue;
        }
        let target = dir.join(name);
        if te.is_dir() {
            count += checkout_tree_to_dir(store, te.oid, &target, alloc)?;
        } else {
            let mut blob = OwnVec::new_in(alloc);
            if store
                .read_object(&te.oid, &mut blob)
                .map_err(|e| io::Error::other(format!("{e}")))?
                == Some(ObjectKind::Blob)
            {
                if (te.mode & 0o170000) == 0o120000 {
                    let _ = fs::remove_file(&target);
                    if let Ok(s) = core::str::from_utf8(blob.as_slice()) {
                        let _ = symlink(s, &target);
                    }
                } else {
                    fs::write(&target, blob.as_slice())?;
                    let perms = if te.mode & 0o111 != 0 { 0o755 } else { 0o644 };
                    let _ = fs::set_permissions(&target, fs::Permissions::from_mode(perms));
                }
                count += 1;
            }
        }
    }
    Ok(count)
}

/// Counts modified, deleted, or untracked files in `wt_dir` relative to `tree_oid`.
fn count_dirty_against_tree<'a>(
    store: &BgitDiskStore<'a>,
    tree_oid: Oid,
    wt_dir: &Path,
    alloc: Alloc<'a>,
) -> io::Result<u32> {
    let mut expected: std::collections::HashMap<String, (u32, Oid)> =
        std::collections::HashMap::new();
    collect_tree_blobs(store, tree_oid, "", alloc, &mut expected)?;

    let mut dirty = 0u32;
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![String::new()];
    while let Some(rel_dir) = stack.pop() {
        let full_dir = if rel_dir.is_empty() { wt_dir.to_path_buf() } else { wt_dir.join(&rel_dir) };
        let Ok(rd) = fs::read_dir(&full_dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if rel_dir.is_empty() && (name == ".git" || name == ".bgit") {
                continue;
            }
            let rel = if rel_dir.is_empty() { name } else { format!("{rel_dir}/{name}") };
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(rel);
                continue;
            }
            let Some(&(exp_mode, exp_oid)) = expected.get(&rel) else {
                dirty += 1;
                continue;
            };
            seen.insert(rel.clone());
            let bytes = if meta.file_type().is_symlink() {
                fs::read_link(entry.path())
                    .map(|p| p.as_os_str().as_encoded_bytes().to_vec())
                    .unwrap_or_default()
            } else {
                fs::read(entry.path()).unwrap_or_default()
            };
            let is_link = (exp_mode & 0o170000) == 0o120000;
            let mode_ok = is_link == meta.file_type().is_symlink()
                && (is_link || ((exp_mode & 0o111 != 0) == (meta.mode() & 0o111 != 0)));
            if !mode_ok || Oid::of(ObjectKind::Blob, &bytes) != exp_oid {
                dirty += 1;
            }
        }
    }
    // Any expected file not seen on disk was deleted.
    dirty += expected.len().saturating_sub(seen.len()) as u32;
    Ok(dirty)
}

fn collect_tree_blobs<'a>(
    store: &BgitDiskStore<'a>,
    tree_oid: Oid,
    prefix: &str,
    alloc: Alloc<'a>,
    out: &mut std::collections::HashMap<String, (u32, Oid)>,
) -> io::Result<()> {
    let mut raw = OwnVec::new_in(alloc);
    if store
        .read_object(&tree_oid, &mut raw)
        .map_err(|e| io::Error::other(format!("{e}")))?
        != Some(ObjectKind::Tree)
    {
        return Ok(());
    }
    let mut iter = TreeIter::new(tree_oid, &raw);
    while let Some(te) = iter.next_entry().map_err(|e| io::Error::other(format!("{e}")))? {
        let Ok(name) = core::str::from_utf8(te.name) else {
            continue;
        };
        let full = if prefix.is_empty() { name.to_string() } else { format!("{prefix}/{name}") };
        if te.is_dir() {
            collect_tree_blobs(store, te.oid, &full, alloc, out)?;
        } else {
            out.insert(full, (te.mode, te.oid));
        }
    }
    Ok(())
}

/// Install idempotent, non-destructive `.git/hooks/reference-transaction` so plain `git merge`,
/// `git pull`, `git commit`, `git fetch`, and `git reset` automatically update tracking worktrees
/// and auto-delete merged worktree folders with zero daemon and zero manual `sync`.
pub(crate) fn install_git_worktree_hooks(repo_root: &Path) -> io::Result<()> {
    let git_dir = resolve_common_git_dir(repo_root);
    let Some(git_dir) = git_dir else {
        return Ok(());
    };
    let hooks_dir = git_dir.join("hooks");
    fs::create_dir_all(&hooks_dir)?;
    let hook_path = hooks_dir.join("reference-transaction");
    const MARKER: &str = "# bgit-worktree-hook";

    let existing = fs::read_to_string(&hook_path).unwrap_or_default();
    if existing.contains(MARKER) {
        return Ok(());
    }

    let exe_str = resolve_bgit_binary().to_string_lossy().into_owned();
    let escaped_exe = exe_str.replace('\'', "'\\''");
    let canon_root = fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let escaped_root = canon_root.to_string_lossy().replace('\'', "'\\''");
    let snippet = format!(
        "{MARKER}\n\
         if [ \"${{1:-}}\" = \"committed\" ] && [ -z \"${{BGIT_HOOK_ACTIVE:-}}\" ]; then\n\
           BGIT_REPO=\"$(cd \"$(dirname \"$0\")/../..\" 2>/dev/null && pwd)\"\n\
           if [ ! -d \"$BGIT_REPO/.bgit\" ]; then\n\
             BGIT_REPO='{escaped_root}'\n\
           fi\n\
           if [ -f \"$BGIT_REPO/.bgit/worktrees.bgwt\" ]; then\n\
             if [ -z \"${{BGIT_BIN:-}}\" ]; then\n\
               BGIT_BIN='{escaped_exe}'\n\
             fi\n\
             if [ ! -x \"$BGIT_BIN\" ]; then\n\
               BGIT_BIN=\"$(command -v bgit 2>/dev/null || true)\"\n\
             fi\n\
             if [ -n \"$BGIT_BIN\" ]; then\n\
               BGIT_HOOK_ACTIVE=1 \"$BGIT_BIN\" worktree sync \"$BGIT_REPO\" --from-hook || true\n\
             fi\n\
           fi\n\
         fi\n"
    );

    let mut content = if existing.is_empty() {
        String::from("#!/bin/sh\n")
    } else {
        let mut s = existing;
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s
    };
    content.push_str(&snippet);

    let tmp = hooks_dir.join("reference-transaction.bgit.tmp");
    fs::write(&tmp, content)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    fs::rename(tmp, &hook_path)
}

/// If a sibling `.git` directory has refs modified more recently than `.bgit/refs.bgref`
/// (or when invoked from a Git `reference-transaction` hook), refresh `.bgit` automatically.
pub(crate) fn refresh_bgit_if_git_refs_newer<'a>(
    repo_dir: &Path,
    force_refresh: bool,
    alloc: Alloc<'a>,
) -> io::Result<()> {
    let Some(common_git) = resolve_common_git_dir(repo_dir) else {
        return Ok(());
    };
    let should_refresh = if force_refresh {
        true
    } else {
        let bgref = repo_dir.join(".bgit/refs.bgref");
        let bg_mtime = fs::metadata(&bgref).and_then(|m| m.modified()).ok();
        let wt_git = crate::disk::locate_git_dir(repo_dir).ok();
        let git_latest = newest_git_ref_mtime(wt_git.as_deref().unwrap_or(&common_git), &common_git);
        matches!((bg_mtime, git_latest), (Some(bm), Some(gm)) if gm > bm)
    };
    if should_refresh {
        if !force_refresh && repo_dir.join(".bgit/objects/index.bgix").is_file() {
            crate::disk::refresh_bgit_incremental_on_disk(repo_dir, alloc)?;
        } else {
            let _ = infect_repo_on_disk(repo_dir, SlabCompression::AutoZstd, alloc)?;
        }
    }
    Ok(())
}

/// O(1) freshness check: stats `HEAD`, `logs/HEAD`, `packed-refs`, the active HEAD branch ref,
/// and top-level `refs/*` directories (Git atomic lock+rename always bumps parent dir mtime,
/// avoiding O(N_refs) recursive stat walks on 5,000+-branch monorepos!).
fn newest_git_ref_mtime(wt_git_dir: &Path, common_git_dir: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let mut bump = |p: &Path| {
        if let Ok(t) = fs::metadata(p).and_then(|m| m.modified()) {
            newest = Some(newest.map_or(t, |cur| cur.max(t)));
        }
    };
    let head_path = wt_git_dir.join("HEAD");
    bump(&head_path);
    bump(&wt_git_dir.join("logs/HEAD"));
    bump(&common_git_dir.join("packed-refs"));
    bump(&common_git_dir.join("refs"));
    bump(&common_git_dir.join("refs/heads"));
    bump(&common_git_dir.join("refs/tags"));
    bump(&common_git_dir.join("refs/remotes"));
    // Also check active symbolic HEAD target ref file (e.g. `refs/heads/feat/foo` inside nested dir).
    if let Ok(raw) = fs::read_to_string(&head_path)
        && let Some(sym) = raw.trim().strip_prefix("ref:")
    {
        let sym = sym.trim();
        if !sym.contains("..") {
            let ref_p = common_git_dir.join(sym);
            bump(&ref_p);
            if let Some(parent) = ref_p.parent() {
                bump(parent);
            }
        }
    }
    // Check immediate remote subdirs (`refs/remotes/origin`, `refs/remotes/upstream`, …).
    if let Ok(rd) = fs::read_dir(common_git_dir.join("refs/remotes")) {
        for e in rd.flatten().take(32) {
            bump(&e.path());
        }
    }
    newest
}

fn ref_tree_fingerprint(repo_dir: &Path) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u64| {
        h ^= v;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    let mut hash_file = |p: &Path| {
        if let Ok(m) = fs::metadata(p) {
            mix(m.len());
            mix(m.mtime() as u64);
            mix(m.mtime_nsec() as u64);
        }
    };
    hash_file(&repo_dir.join(".bgit/refs.bgref"));
    hash_file(&repo_dir.join(".bgit/worktrees.bgwt"));
    let dot_git = repo_dir.join(".git");
    if dot_git.is_dir() {
        hash_file(&dot_git.join("HEAD"));
        hash_file(&dot_git.join("packed-refs"));
        let mut stack = vec![dot_git.join("refs")];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    hash_file(&p);
                }
            }
        }
    }
    h
}
