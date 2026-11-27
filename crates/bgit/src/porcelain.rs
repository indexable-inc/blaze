//! Native Git porcelain & plumbing commands for `bgit` — 100% pure Rust over `blaze-git`
//! (`BgitDiskStore`, `index_io`, ORT 3-way merge, Protocol v2 transport), keeping `.bgit` and
//! sibling `.git` stores dual-synced with zero external `git` subprocess calls.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use blaze_git::{
    ShortOid,
    merge::{MergeLimits, MergeOpts, MergeStatus, find_merge_bases, merge_trees},
    refs::ref_name_safe,
    storage::{ObjectStore, RefStore, RefTarget, RefUpdate, ZERO_OID, encode_commit},
};
use blaze_git_fetch::{Kind as ObjectKind, Oid, store::Commit};
use blaze_own::{Alloc, String as OwnString, Vec as OwnVec};
use bstd::cli::Matches;

use crate::{
    IoStreams,
    disk::{
        BgitDiskStore, delete_ref_in_git, locate_git_dir, mirror_head_to_git,
        mirror_overlay_objects_to_git, mirror_ref_to_git, open_or_infect, write_file_atomic,
    },
    find_repo_root,
    index_io::{
        GitConfig, WritableIndexEntry, build_tree_from_index, build_tree_from_map,
        checkout_tree_to_worktree, diff_line_stats, diff_trees_changed, flatten_tree,
        format_unified_diff, index_entry_stat_clean, index_from_flat_tree, load_index,
        make_signature, path_matches_filters, prune_empty_parents, read_commit_tree,
        read_worktree_blob, resolve_identity, stat_to_index_entry, to_repo_relative, union_paths,
        walk_working_tree, write_blob_to_worktree, write_index,
    },
    net_driver::{FetchParams, remote_fetch},
    remote_transport::looks_like_remote_url,
    worktree_driver::{on_branch_merged, refresh_bgit_if_git_refs_newer},
};

/// Discovers the repo root enclosing `cwd`, refreshes `.bgit` if `.git` refs are newer, and opens it.
pub(crate) fn open_repo_from_cwd<'a>(
    cwd: &Path,
    alloc: Alloc<'a>,
) -> io::Result<(PathBuf, BgitDiskStore<'a>)> {
    let root = find_repo_root(cwd).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("not a git or bgit repository (or any parent up to /): {}", cwd.display()),
        )
    })?;
    if root.join(".bgit/VERSION").is_file() {
        let _ = refresh_bgit_if_git_refs_newer(&root, false, alloc);
    }
    let store = open_or_infect(&root, alloc)?;
    Ok((root, store))
}

fn current_head_branch<'a>(store: &BgitDiskStore<'a>, alloc: Alloc<'a>) -> Option<String> {
    match store.read_head(alloc).ok().flatten() {
        Some(RefTarget::Symbolic(sym)) => {
            Some(sym.as_str().strip_prefix("refs/heads/").unwrap_or(sym.as_str()).to_owned())
        }
        _ => None,
    }
}

fn current_head_ref<'a>(store: &BgitDiskStore<'a>, alloc: Alloc<'a>) -> Option<String> {
    match store.read_head(alloc).ok().flatten() {
        Some(RefTarget::Symbolic(sym)) => Some(sym.as_str().to_owned()),
        _ => None,
    }
}

fn read_commit_subject<'a>(store: &BgitDiskStore<'a>, oid: Oid, alloc: Alloc<'a>) -> String {
    let mut buf = OwnVec::new_in(alloc);
    if store.read_object(&oid, &mut buf).ok().flatten() != Some(ObjectKind::Commit) {
        return String::new();
    }
    let text = core::str::from_utf8(&buf).unwrap_or("");
    text.split_once("\n\n")
        .and_then(|(_, msg)| msg.lines().next())
        .unwrap_or("")
        .to_owned()
}

fn read_blob_bytes<'a>(
    store: &BgitDiskStore<'a>,
    oid: Oid,
    alloc: Alloc<'a>,
) -> io::Result<Vec<u8>> {
    let mut buf = OwnVec::new_in(alloc);
    let kind = store
        .read_object(&oid, &mut buf)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    if kind != Some(ObjectKind::Blob) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("object {oid} is not a blob"),
        ));
    }
    Ok(buf.as_slice().to_vec())
}

fn head_flat_tree<'a>(
    store: &BgitDiskStore<'a>,
    alloc: Alloc<'a>,
) -> io::Result<BTreeMap<String, (u32, Oid)>> {
    let Some(head_oid) = store.resolve_commit("HEAD").map_err(|e| io::Error::other(format!("{e}")))?
    else {
        return Ok(BTreeMap::new());
    };
    let tree_oid = read_commit_tree(store, head_oid, alloc)?;
    flatten_tree(store, tree_oid, alloc)
}

fn apply_single_ref_update<'a>(
    store: &mut BgitDiskStore<'a>,
    repo_root: &Path,
    full_ref: &str,
    new_oid: Oid,
    alloc: Alloc<'a>,
) -> io::Result<()> {
    let upd = [RefUpdate {
        name: OwnString::from_str_in(full_ref, alloc),
        old: None,
        new: new_oid,
    }];
    let res = store
        .apply_ref_updates(&upd, true, alloc)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    if res.first().copied() != Some(blaze_git::storage::RefUpdateStatus::Ok) {
        return Err(io::Error::other(format!("failed to update ref `{full_ref}`")));
    }
    mirror_overlay_objects_to_git(store, repo_root, alloc);
    store.flush()?;
    if new_oid == ZERO_OID {
        delete_ref_in_git(repo_root, full_ref);
    } else {
        mirror_ref_to_git(repo_root, full_ref, new_oid);
    }
    Ok(())
}

fn update_head_target<'a>(
    store: &mut BgitDiskStore<'a>,
    repo_root: &Path,
    target: &RefTarget<'a>,
    alloc: Alloc<'a>,
) -> io::Result<()> {
    store.write_head(target).map_err(|e| io::Error::other(format!("{e}")))?;
    mirror_overlay_objects_to_git(store, repo_root, alloc);
    store.flush()?;
    mirror_head_to_git(repo_root, target);
    Ok(())
}

// ============================================================================
// 1. `bgit branch`
// ============================================================================

pub(crate) fn cmd_branch<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    use_color: bool,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let (repo_root, mut store) = open_repo_from_cwd(cwd, alloc)?;
    let show_current = matches.flag("show-current");
    let delete = matches.flag("delete");
    let delete_force = matches.flag("force-delete");
    let rename = matches.flag("move");
    let rename_force = matches.flag("force-move");
    let force = matches.flag("force");
    let all = matches.flag("all");
    let remotes_only = matches.flag("remotes");
    let verbose = matches.flag_count("verbose");
    let list_flag = matches.flag("list");
    let pos = matches.positionals();

    if show_current {
        if let Some(cur) = current_head_branch(&store, alloc) {
            writeln!(io.out, "{cur}")?;
        }
        return Ok(());
    }

    if delete || delete_force {
        if pos.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "branch name required (usage: bgit branch -d <name>...)",
            ));
        }
        let active = current_head_branch(&store, alloc);
        let head_oid = store.resolve_commit("HEAD").map_err(|e| io::Error::other(format!("{e}")))?;
        for &name in pos {
            let short = name.strip_prefix("refs/heads/").unwrap_or(name);
            if active.as_deref() == Some(short) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("cannot delete branch '{short}' checked out at '{}'", repo_root.display()),
                ));
            }
            let full = format!("refs/heads/{short}");
            let Some(tip) = store
                .resolve_ref(&full)
                .map_err(|e| io::Error::other(format!("{e}")))?
            else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("branch '{short}' not found"),
                ));
            };
            if !delete_force && !force {
                let is_merged = if let Some(h) = head_oid {
                    let cg = store.commit_graph();
                    find_merge_bases(&store, cg.as_ref(), h, tip, MergeLimits::SHIPPED, alloc)
                        .map(|bases| bases.iter().any(|b| *b == tip))
                        .unwrap_or(false)
                } else {
                    false
                };
                if !is_merged {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "the branch '{short}' is not fully merged. \
                             If you are sure you want to delete it, run 'bgit branch -D {short}'"
                        ),
                    ));
                }
            }
            apply_single_ref_update(&mut store, &repo_root, &full, ZERO_OID, alloc)?;
            writeln!(io.out, "Deleted branch {short} (was {}).", ShortOid::from_oid(tip, 7))?;
        }
        return Ok(());
    }

    if rename || rename_force {
        let (old_short, new_short) = match pos {
            [new_name] => {
                let cur = current_head_branch(&store, alloc).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "cannot rename detached HEAD; specify <old> <new>",
                    )
                })?;
                (cur, (*new_name).to_owned())
            }
            [old_name, new_name] => ((*old_name).to_owned(), (*new_name).to_owned()),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: bgit branch -m [<old>] <new>",
                ));
            }
        };
        let old_ref = format!("refs/heads/{old_short}");
        let new_ref = format!("refs/heads/{new_short}");
        if !ref_name_safe(&new_ref) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("'{new_short}' is not a valid branch name"),
            ));
        }
        let Some(oid) = store
            .resolve_ref(&old_ref)
            .map_err(|e| io::Error::other(format!("{e}")))?
        else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("branch '{old_short}' not found"),
            ));
        };
        if !rename_force
            && !force
            && old_ref != new_ref
            && store
                .resolve_ref(&new_ref)
                .map_err(|e| io::Error::other(format!("{e}")))?
                .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("a branch named '{new_short}' already exists (pass -M to overwrite)"),
            ));
        }
        let active = current_head_branch(&store, alloc);
        apply_single_ref_update(&mut store, &repo_root, &new_ref, oid, alloc)?;
        if active.as_deref() == Some(old_short.as_str()) {
            let sym = RefTarget::Symbolic(OwnString::from_str_in(&new_ref, alloc));
            update_head_target(&mut store, &repo_root, &sym, alloc)?;
        }
        if old_ref != new_ref {
            apply_single_ref_update(&mut store, &repo_root, &old_ref, ZERO_OID, alloc)?;
        }
        return Ok(());
    }

    // Create branch when positional(s) are given without `--list`.
    if !list_flag && !pos.is_empty() {
        let name = pos[0].strip_prefix("refs/heads/").unwrap_or(pos[0]);
        let start = pos.get(1).copied().unwrap_or("HEAD");
        let full = format!("refs/heads/{name}");
        if !ref_name_safe(&full) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("'{name}' is not a valid branch name"),
            ));
        }
        if !force
            && store
                .resolve_ref(&full)
                .map_err(|e| io::Error::other(format!("{e}")))?
                .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("a branch named '{name}' already exists (pass -f to move it)"),
            ));
        }
        let target_oid = store
            .resolve_commit(start)
            .map_err(|e| io::Error::other(format!("{e}")))?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("not a valid commit: '{start}'"))
            })?;
        apply_single_ref_update(&mut store, &repo_root, &full, target_oid, alloc)?;
        return Ok(());
    }

    // List branches (narrow prefix for O(log R + K) lookup over mmap `BGRF` v2!).
    let active = current_head_branch(&store, alloc);
    let ref_prefix = if all {
        "refs/"
    } else if remotes_only {
        "refs/remotes/"
    } else {
        "refs/heads/"
    };
    let mut refs = OwnVec::new_in(alloc);
    store
        .list_refs(ref_prefix, alloc, &mut refs)
        .map_err(|e| io::Error::other(format!("{e}")))?;

    let mut local: Vec<(String, Oid)> = Vec::new();
    let mut remote: Vec<(String, Oid)> = Vec::new();
    for r in &refs {
        let oid = match &r.target {
            RefTarget::Direct(o) => Some(*o),
            RefTarget::Symbolic(_) => store.resolve_ref(r.name.as_str()).ok().flatten(),
        };
        let Some(oid) = oid else {
            continue;
        };
        if let Some(short) = r.name.as_str().strip_prefix("refs/heads/") {
            local.push((short.to_owned(), oid));
        } else if let Some(short) = r.name.as_str().strip_prefix("refs/remotes/") {
            remote.push((short.to_owned(), oid));
        }
    }
    local.sort_by(|a, b| a.0.cmp(&b.0));
    remote.sort_by(|a, b| a.0.cmp(&b.0));

    let (green, red, dim, reset) = if use_color {
        ("\x1b[32m", "\x1b[31m", "\x1b[2m", "\x1b[0m")
    } else {
        ("", "", "", "")
    };

    if !remotes_only {
        if active.is_none()
            && let Some(detached) =
                store.resolve_commit("HEAD").map_err(|e| io::Error::other(format!("{e}")))?
        {
            if verbose > 0 {
                let subj = read_commit_subject(&store, detached, alloc);
                writeln!(
                    io.out,
                    "* {green}(HEAD detached at {}){reset} {} {subj}",
                    ShortOid::from_oid(detached, 7),
                    ShortOid::from_oid(detached, 7)
                )?;
            } else {
                writeln!(
                    io.out,
                    "* {green}(HEAD detached at {}){reset}",
                    ShortOid::from_oid(detached, 7)
                )?;
            }
        }
        let max_w = local.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
        for (name, oid) in &local {
            let is_cur = active.as_deref() == Some(name.as_str());
            if verbose > 0 {
                let subj = read_commit_subject(&store, *oid, alloc);
                if is_cur {
                    writeln!(
                        io.out,
                        "* {green}{name:<max_w$}{reset} {dim}{}{reset} {subj}",
                        ShortOid::from_oid(*oid, 7)
                    )?;
                } else {
                    writeln!(
                        io.out,
                        "  {name:<max_w$} {dim}{}{reset} {subj}",
                        ShortOid::from_oid(*oid, 7)
                    )?;
                }
            } else if is_cur {
                writeln!(io.out, "* {green}{name}{reset}")?;
            } else {
                writeln!(io.out, "  {name}")?;
            }
        }
    }

    if all || remotes_only {
        let max_w = remote.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
        for (name, oid) in &remote {
            let label = if all { format!("remotes/{name}") } else { name.clone() };
            if verbose > 0 {
                let subj = read_commit_subject(&store, *oid, alloc);
                writeln!(
                    io.out,
                    "  {red}{label:<w$}{reset} {dim}{}{reset} {subj}",
                    ShortOid::from_oid(*oid, 7),
                    w = max_w + if all { 8 } else { 0 }
                )?;
            } else {
                writeln!(io.out, "  {red}{label}{reset}")?;
            }
        }
    }
    Ok(())
}

// ============================================================================
// 2. `bgit checkout` & `bgit switch`
// ============================================================================

pub(crate) fn cmd_checkout<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let create = matches.opt("create");
    let force_create = matches.opt("force-create");
    let force = matches.flag("force");
    let detach = matches.flag("detach");
    let pos = matches.positionals();

    let (repo_root, mut store) = open_repo_from_cwd(cwd, alloc)?;

    // Path checkout mode (`bgit checkout -- <path>...` or `bgit checkout <rev> -- <path>...`
    // or `bgit checkout <existing_file>` when no branch/create flag is active).
    if create.is_none() && force_create.is_none() && !detach && !pos.is_empty() {
        let first = pos[0];
        let rev_exists = store.resolve_commit(first).ok().flatten().is_some();
        if !rev_exists {
            // Treat all positionals as working-tree paths to restore from index!
            return restore_paths_inner(&repo_root, &store, cwd, None, false, true, pos, alloc, io);
        }
        if pos.len() > 1 {
            // `bgit checkout <rev> <path>...` -> restore paths from `<rev>` into both index & worktree.
            return restore_paths_inner(
                &repo_root,
                &store,
                cwd,
                Some(first),
                true,
                true,
                &pos[1..],
                alloc,
                io,
            );
        }
    }

    switch_or_checkout_branch(
        &repo_root,
        &mut store,
        create,
        force_create,
        detach,
        force,
        pos.first().copied(),
        alloc,
        io,
    )
}

pub(crate) fn cmd_switch<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let create = matches.opt("create");
    let force_create = matches.opt("force-create");
    let detach = matches.flag("detach");
    let force = matches.flag("force");
    let target = matches.positional(0);
    let (repo_root, mut store) = open_repo_from_cwd(cwd, alloc)?;
    switch_or_checkout_branch(
        &repo_root,
        &mut store,
        create,
        force_create,
        detach,
        force,
        target,
        alloc,
        io,
    )
}

#[allow(clippy::too_many_arguments)]
fn switch_or_checkout_branch<'a>(
    repo_root: &Path,
    store: &mut BgitDiskStore<'a>,
    create: Option<&str>,
    force_create: Option<&str>,
    detach: bool,
    force: bool,
    target_arg: Option<&str>,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let old_flat = head_flat_tree(store, alloc)?;

    if let Some((new_name, is_force_create)) =
        create.map(|n| (n, false)).or_else(|| force_create.map(|n| (n, true)))
    {
        let short = new_name.strip_prefix("refs/heads/").unwrap_or(new_name);
        let full = format!("refs/heads/{short}");
        if !ref_name_safe(&full) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("'{short}' is not a valid branch name"),
            ));
        }
        if !is_force_create
            && store
                .resolve_ref(&full)
                .map_err(|e| io::Error::other(format!("{e}")))?
                .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("a branch named '{short}' already exists"),
            ));
        }
        let start_rev = target_arg.unwrap_or("HEAD");
        let start_oid = store
            .resolve_commit(start_rev)
            .map_err(|e| io::Error::other(format!("{e}")))?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("not a valid commit: '{start_rev}'"),
                )
            })?;
        let new_tree = read_commit_tree(store, start_oid, alloc)?;
        let new_flat = flatten_tree(store, new_tree, alloc)?;
        checkout_tree_to_worktree(repo_root, store, &old_flat, &new_flat, force, alloc)?;
        let new_idx = index_from_flat_tree(repo_root, &new_flat);
        write_index(repo_root, &new_idx)?;
        apply_single_ref_update(store, repo_root, &full, start_oid, alloc)?;
        let sym = RefTarget::Symbolic(OwnString::from_str_in(&full, alloc));
        update_head_target(store, repo_root, &sym, alloc)?;
        writeln!(io.err, "Switched to a new branch '{short}'")?;
        return Ok(());
    }

    let Some(target) = target_arg else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing branch or revision to switch to",
        ));
    };

    let branch_ref = format!("refs/heads/{target}");
    let is_local_branch = !detach
        && store
            .resolve_ref(&branch_ref)
            .map_err(|e| io::Error::other(format!("{e}")))?
            .is_some();

    let target_oid = if is_local_branch {
        store
            .resolve_ref(&branch_ref)
            .map_err(|e| io::Error::other(format!("{e}")))?
            .expect("verified above")
    } else {
        store
            .resolve_commit(target)
            .map_err(|e| io::Error::other(format!("{e}")))?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("pathspec or branch '{target}' did not match any known branch or revision"),
                )
            })?
    };

    let new_tree = read_commit_tree(store, target_oid, alloc)?;
    let new_flat = flatten_tree(store, new_tree, alloc)?;
    checkout_tree_to_worktree(repo_root, store, &old_flat, &new_flat, force, alloc)?;
    let new_idx = index_from_flat_tree(repo_root, &new_flat);
    write_index(repo_root, &new_idx)?;

    if is_local_branch {
        let sym = RefTarget::Symbolic(OwnString::from_str_in(&branch_ref, alloc));
        update_head_target(store, repo_root, &sym, alloc)?;
        writeln!(io.err, "Switched to branch '{target}'")?;
    } else {
        let direct = RefTarget::Direct(target_oid);
        update_head_target(store, repo_root, &direct, alloc)?;
        writeln!(
            io.err,
            "HEAD is now at {} {}",
            ShortOid::from_oid(target_oid, 7),
            read_commit_subject(store, target_oid, alloc)
        )?;
    }
    Ok(())
}

// ============================================================================
// 3. `bgit add`
// ============================================================================

pub(crate) fn cmd_add<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    _io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let all = matches.flag("all");
    let update = matches.flag("update");
    let force = matches.flag("force");
    let pos = matches.positionals();

    if !all && !update && pos.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "nothing specified, nothing added (try `bgit add .` or `bgit add -A`)",
        ));
    }

    let (repo_root, mut store) = open_repo_from_cwd(cwd, alloc)?;
    let mut index = load_index(&repo_root, alloc)?;

    if update && pos.is_empty() {
        stage_tracked_updates(&repo_root, &mut store, &mut index, &[])?;
    } else if all && pos.is_empty() {
        let wt_files = walk_working_tree(&repo_root, force, alloc)?;
        let wt_set: BTreeSet<&str> = wt_files.iter().map(String::as_str).collect();
        index.retain(|p, _| wt_set.contains(p.as_str()));
        for rel in &wt_files {
            stage_single_file(&repo_root, &mut store, &mut index, rel)?;
        }
    } else {
        let mut filters = Vec::with_capacity(pos.len());
        for &raw in pos {
            filters.push(to_repo_relative(&repo_root, cwd, raw)?);
        }
        if update {
            stage_tracked_updates(&repo_root, &mut store, &mut index, &filters)?;
        } else {
            let wt_files = walk_working_tree(&repo_root, force, alloc)?;
            let wt_set: BTreeSet<&str> = wt_files.iter().map(String::as_str).collect();
            // Stage matching files present in working tree (and explicit file args even if ignored when `-f`).
            let mut matched_any = false;
            for rel in &wt_files {
                if path_matches_filters(rel, &filters) {
                    stage_single_file(&repo_root, &mut store, &mut index, rel)?;
                    matched_any = true;
                }
            }
            // Remove tracked files matching filters that were deleted on disk.
            let to_remove: Vec<String> = index
                .keys()
                .filter(|p| path_matches_filters(p, &filters) && !wt_set.contains(p.as_str()))
                .cloned()
                .collect();
            for p in to_remove {
                index.remove(&p);
                matched_any = true;
            }
            if !matched_any {
                for f in &filters {
                    let full = repo_root.join(f);
                    if !full.exists() && !full.is_symlink() {
                        return Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("pathspec '{f}' did not match any files"),
                        ));
                    }
                }
            }
        }
    }

    mirror_overlay_objects_to_git(&store, &repo_root, alloc);
    store.flush()?;
    write_index(&repo_root, &index)?;
    Ok(())
}

fn stage_tracked_updates<'a>(
    repo_root: &Path,
    store: &mut BgitDiskStore<'a>,
    index: &mut BTreeMap<String, WritableIndexEntry>,
    filters: &[String],
) -> io::Result<()> {
    let keys: Vec<String> = index
        .keys()
        .filter(|p| path_matches_filters(p, filters))
        .cloned()
        .collect();
    for rel in keys {
        let full = repo_root.join(&rel);
        if full.exists() || full.is_symlink() {
            stage_single_file(repo_root, store, index, &rel)?;
        } else {
            index.remove(&rel);
        }
    }
    Ok(())
}

fn stage_single_file<'a>(
    repo_root: &Path,
    store: &mut BgitDiskStore<'a>,
    index: &mut BTreeMap<String, WritableIndexEntry>,
    rel: &str,
) -> io::Result<()> {
    let full = repo_root.join(rel);
    let (mode, bytes) = read_worktree_blob(&full)?;
    let oid = store
        .write_object(ObjectKind::Blob, &bytes)
        .map_err(|e| io::Error::other(format!("write blob `{rel}`: {e}")))?;
    let mut entry = stat_to_index_entry(repo_root, rel, oid)?;
    entry.mode = mode;
    index.insert(rel.to_owned(), entry);
    Ok(())
}

// ============================================================================
// 4. `bgit commit`
// ============================================================================

pub(crate) fn cmd_commit<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let all = matches.flag("all");
    let amend = matches.flag("amend");
    let allow_empty = matches.flag("allow-empty");
    let messages = matches
        .opt_all("message", alloc)
        .map_err(|e| io::Error::other(format!("{e:?}")))?;

    let (repo_root, mut store) = open_repo_from_cwd(cwd, alloc)?;
    let mut index = load_index(&repo_root, alloc)?;

    if all {
        stage_tracked_updates(&repo_root, &mut store, &mut index, &[])?;
        write_index(&repo_root, &index)?;
    }

    let head_oid = store.resolve_commit("HEAD").map_err(|e| io::Error::other(format!("{e}")))?;

    // Resolve commit message (`-m` required unless `--amend` reuses prior message).
    let mut msg = if !messages.is_empty() {
        messages.join("\n\n")
    } else if amend {
        let Some(prev) = head_oid else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "nothing to amend (no HEAD commit yet)",
            ));
        };
        let mut buf = OwnVec::new_in(alloc);
        store
            .read_object(&prev, &mut buf)
            .map_err(|e| io::Error::other(format!("{e}")))?;
        core::str::from_utf8(&buf)
            .ok()
            .and_then(|t| t.split_once("\n\n").map(|(_, body)| body.to_owned()))
            .unwrap_or_default()
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "commit message required (pass -m <MSG>)",
        ));
    };
    if !msg.ends_with('\n') {
        msg.push('\n');
    }

    let tree_oid = build_tree_from_index(&mut store, &index, alloc)?;

    let (parents, prev_tree_oid): (Vec<Oid>, Option<Oid>) = if amend {
        let Some(prev) = head_oid else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot --amend when there is no existing commit",
            ));
        };
        let mut buf = OwnVec::new_in(alloc);
        store
            .read_object(&prev, &mut buf)
            .map_err(|e| io::Error::other(format!("{e}")))?;
        let parsed = Commit::parse(&buf)
            .map_err(|(_, why)| io::Error::new(io::ErrorKind::InvalidData, format!("{why:?}")))?;
        let ps: Vec<Oid> = parsed.parents().collect();
        let pt = ps.first().and_then(|&p| read_commit_tree(&store, p, alloc).ok());
        (ps, pt)
    } else if let Some(prev) = head_oid {
        let pt = read_commit_tree(&store, prev, alloc)?;
        if pt == tree_oid && !allow_empty {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "nothing to commit (stage changes with `bgit add` or pass `--allow-empty`)",
            ));
        }
        (vec![prev], Some(pt))
    } else {
        if index.is_empty() && !allow_empty {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "nothing to commit (stage files with `bgit add` or pass `--allow-empty`)",
            ));
        }
        (Vec::new(), None)
    };

    let (name, email, ts) = resolve_identity(&repo_root);
    let sig = make_signature(&name, &email, ts);
    let mut commit_buf = OwnVec::new_in(alloc);
    encode_commit(tree_oid, &parents, sig, sig, &msg, &mut commit_buf);
    let commit_oid = store
        .write_object(ObjectKind::Commit, &commit_buf)
        .map_err(|e| io::Error::other(format!("write commit: {e}")))?;

    // Update HEAD's branch ref (or detached HEAD).
    let branch_label = if let Some(full_ref) = current_head_ref(&store, alloc) {
        apply_single_ref_update(&mut store, &repo_root, &full_ref, commit_oid, alloc)?;
        full_ref.strip_prefix("refs/heads/").unwrap_or(&full_ref).to_owned()
    } else {
        let direct = RefTarget::Direct(commit_oid);
        update_head_target(&mut store, &repo_root, &direct, alloc)?;
        "detached HEAD".to_owned()
    };

    // Compute O(changed) diffstat via subtree-skipping tree diff!
    let changed = diff_trees_changed(&store, prev_tree_oid, Some(tree_oid), alloc)?;
    let (files_changed, ins, del) = summarize_changed_entries(&store, &changed, alloc)?;

    let subject = msg.lines().next().unwrap_or("");
    let root_tag = if parents.is_empty() { " (root-commit)" } else { "" };
    writeln!(
        io.out,
        "[{branch_label}{root_tag} {}] {subject}",
        ShortOid::from_oid(commit_oid, 7)
    )?;
    if files_changed > 0 {
        writeln!(
            io.out,
            " {files_changed} file(s) changed, {ins} insertion(s)(+), {del} deletion(s)(-)"
        )?;
    }

    // Notify auto-merging downstream worktrees if this repo uses `bgit worktree`.
    if repo_root.join(".bgit/worktrees.bgwt").is_file()
        && let Some(full_ref) = current_head_ref(&store, alloc)
    {
        drop(store);
        let _ = on_branch_merged(&repo_root, Some(full_ref.as_str()), "", true, false, alloc, io);
    }

    Ok(())
}

fn summarize_changed_entries<'a>(
    store: &BgitDiskStore<'a>,
    changed: &[(String, Option<(u32, Oid)>, Option<(u32, Oid)>)],
    alloc: Alloc<'a>,
) -> io::Result<(usize, usize, usize)> {
    let mut total_ins = 0usize;
    let mut total_del = 0usize;
    for (_, a, b) in changed {
        let old_b = a.and_then(|(_, oid)| read_blob_bytes(store, oid, alloc).ok());
        let new_b = b.and_then(|(_, oid)| read_blob_bytes(store, oid, alloc).ok());
        let (ins, del) = diff_line_stats(
            old_b.as_deref().unwrap_or(b""),
            new_b.as_deref().unwrap_or(b""),
        );
        total_ins += ins;
        total_del += del;
    }
    Ok((changed.len(), total_ins, total_del))
}

// ============================================================================
// 5. `bgit diff` & 6. `bgit show`
// ============================================================================

/// Scans `index` entries against the working tree IN PARALLEL across CPU cores using the index
/// stat cache (`mtime`, `size`, `ino`, `mode`), skipping stat-clean files with zero file reads!
fn collect_dirty_worktree_entries<'a>(
    repo_root: &Path,
    index: &'a BTreeMap<String, WritableIndexEntry>,
    filters: &[String],
) -> Vec<(&'a WritableIndexEntry, Option<Vec<u8>>)> {
    let candidates: Vec<&WritableIndexEntry> = index
        .values()
        .filter(|e| path_matches_filters(&e.path, filters))
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get()).min(16);
    if candidates.len() < 128 || workers <= 1 {
        let mut dirty = Vec::new();
        for e in candidates {
            if let Some(d) = check_worktree_entry_dirty(repo_root, e) {
                dirty.push((e, d));
            }
        }
        return dirty;
    }
    let chunk_sz = candidates.len().div_ceil(workers);
    let chunks: Vec<&[&WritableIndexEntry]> = candidates.chunks(chunk_sz).collect();
    let mut out = Vec::new();
    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            handles.push(s.spawn(move || {
                let mut local = Vec::new();
                for &e in chunk {
                    if let Some(d) = check_worktree_entry_dirty(repo_root, e) {
                        local.push((e, d));
                    }
                }
                local
            }));
        }
        for h in handles {
            out.extend(h.join().expect("diff stat worker"));
        }
    });
    out
}

/// Returns `None` if working-tree file is identical to index entry `e`, or `Some(Option<bytes>)`
/// (`Some(None)` = deleted, `Some(Some(bytes))` = modified).
fn check_worktree_entry_dirty(
    repo_root: &Path,
    e: &WritableIndexEntry,
) -> Option<Option<Vec<u8>>> {
    if index_entry_stat_clean(repo_root, e) {
        return None;
    }
    let full = repo_root.join(&e.path);
    if !full.exists() && !full.is_symlink() {
        return Some(None);
    }
    let Ok((_, bytes)) = read_worktree_blob(&full) else {
        return Some(None);
    };
    if Oid::of(ObjectKind::Blob, &bytes) == e.oid {
        return None;
    }
    Some(Some(bytes))
}

#[allow(clippy::too_many_arguments)]
fn emit_diff_entry(
    path: &str,
    old_bytes: Option<&[u8]>,
    new_bytes: Option<&[u8]>,
    name_only: bool,
    name_status: bool,
    stat_only: bool,
    use_color: bool,
    stat_rows: &mut Vec<(String, usize, usize)>,
    out: &mut dyn Write,
) -> io::Result<()> {
    if old_bytes == new_bytes {
        return Ok(());
    }
    if name_only {
        writeln!(out, "{path}")?;
    } else if name_status {
        let status = match (old_bytes, new_bytes) {
            (None, Some(_)) => "A",
            (Some(_), None) => "D",
            _ => "M",
        };
        writeln!(out, "{status}\t{path}")?;
    } else if stat_only {
        let (ins, del) = diff_line_stats(old_bytes.unwrap_or(b""), new_bytes.unwrap_or(b""));
        stat_rows.push((path.to_owned(), ins, del));
    } else {
        let patch = format_unified_diff(path, old_bytes, new_bytes, use_color);
        write!(out, "{patch}")?;
    }
    Ok(())
}

pub(crate) fn cmd_diff<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    use_color: bool,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let staged = matches.flag("staged") || matches.flag("cached");
    let stat_only = matches.flag("stat");
    let name_only = matches.flag("name-only");
    let name_status = matches.flag("name-status");
    let pos = matches.positionals();

    let (repo_root, store) = open_repo_from_cwd(cwd, alloc)?;

    // Parse optional revisions vs path filters.
    let mut revs: Vec<String> = Vec::new();
    let mut filters: Vec<String> = Vec::new();
    for &arg in pos {
        if revs.len() < 2
            && filters.is_empty()
            && (arg.contains("..") || store.resolve_commit(arg).ok().flatten().is_some())
        {
            revs.push(arg.to_owned());
        } else {
            filters.push(to_repo_relative(&repo_root, cwd, arg)?);
        }
    }

    let mut stat_rows: Vec<(String, usize, usize)> = Vec::new();

    // Fast path 1: Working tree vs Index (`bgit diff` with no revs and not `--staged`).
    // When an in-process FSEvents monitor (`blaze-shell`) is active, checks ONLY the K dirty paths
    // (0 index parsing when clean!); otherwise runs parallel stat-cache check across cores.
    if revs.is_empty() && !staged {
        let dirty: Vec<(WritableIndexEntry, Option<Vec<u8>>)> =
            match crate::fsmonitor::try_diff_candidates(&repo_root, alloc) {
                Some(crate::fsmonitor::DiffFastPath::Clean) => Vec::new(),
                Some(crate::fsmonitor::DiffFastPath::Candidates(cands)) => {
                    let mut all_dirty_paths = BTreeSet::new();
                    let mut out = Vec::new();
                    for e in cands {
                        if let Some(d) = check_worktree_entry_dirty(&repo_root, &e) {
                            all_dirty_paths.insert(e.path.clone());
                            if path_matches_filters(&e.path, &filters) {
                                out.push((e, d));
                            }
                        }
                    }
                    crate::fsmonitor::update_diff_known_dirty(&repo_root, all_dirty_paths);
                    out
                }
                None => {
                    let scan_gen = crate::fsmonitor::begin_full_scan(&repo_root);
                    let idx = load_index(&repo_root, alloc)?;
                    let all_dirty = if filters.is_empty() || crate::fsmonitor::is_watching(&repo_root) {
                        let d = collect_dirty_worktree_entries(&repo_root, &idx, &[]);
                        let dirty_names: BTreeSet<String> =
                            d.iter().map(|(e, _)| e.path.clone()).collect();
                        crate::fsmonitor::record_diff_baseline(&repo_root, dirty_names, scan_gen);
                        d.into_iter()
                            .filter(|(e, _)| path_matches_filters(&e.path, &filters))
                            .map(|(e, b)| (e.clone(), b))
                            .collect()
                    } else {
                        collect_dirty_worktree_entries(&repo_root, &idx, &filters)
                            .into_iter()
                            .map(|(e, b)| (e.clone(), b))
                            .collect()
                    };
                    all_dirty
                }
            };
        for (entry, new_opt) in dirty {
            let old_opt = if name_only || name_status {
                Some(Vec::new())
            } else {
                read_blob_bytes(&store, entry.oid, alloc).ok()
            };
            let new_ref = if (name_only || name_status) && new_opt.is_some() {
                Some(b"x".as_slice())
            } else {
                new_opt.as_deref()
            };
            emit_diff_entry(
                &entry.path,
                old_opt.as_deref(),
                new_ref,
                name_only,
                name_status,
                stat_only,
                use_color,
                &mut stat_rows,
                io.out,
            )?;
        }
        if stat_only && !stat_rows.is_empty() {
            write_diffstat(&stat_rows, use_color, io.out)?;
        }
        return Ok(());
    }

    // Fast path 2: Tree vs Tree (`A..B`, `A...B`, `A B`) via O(changed) subtree-skipping walk!
    let tree_pair: Option<(Oid, Oid)> = if let Some(first) = revs.first()
        && first.contains("...")
    {
        let (a_s, b_s) = first.split_once("...").expect("checked");
        let a_oid = store
            .resolve_commit(if a_s.is_empty() { "HEAD" } else { a_s })
            .map_err(|e| io::Error::other(format!("{e}")))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("unknown rev `{a_s}`")))?;
        let b_oid = store
            .resolve_commit(if b_s.is_empty() { "HEAD" } else { b_s })
            .map_err(|e| io::Error::other(format!("{e}")))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("unknown rev `{b_s}`")))?;
        let cg = store.commit_graph();
        let bases = find_merge_bases(&store, cg.as_ref(), a_oid, b_oid, MergeLimits::SHIPPED, alloc)
            .map_err(|e| io::Error::other(format!("{e}")))?;
        let base_oid = bases.first().copied().unwrap_or(a_oid);
        Some((read_commit_tree(&store, base_oid, alloc)?, read_commit_tree(&store, b_oid, alloc)?))
    } else if let Some(first) = revs.first()
        && first.contains("..")
    {
        let (a_s, b_s) = first.split_once("..").expect("checked");
        let a_oid = store
            .resolve_commit(if a_s.is_empty() { "HEAD" } else { a_s })
            .map_err(|e| io::Error::other(format!("{e}")))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("unknown rev `{a_s}`")))?;
        let b_oid = store
            .resolve_commit(if b_s.is_empty() { "HEAD" } else { b_s })
            .map_err(|e| io::Error::other(format!("{e}")))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("unknown rev `{b_s}`")))?;
        Some((read_commit_tree(&store, a_oid, alloc)?, read_commit_tree(&store, b_oid, alloc)?))
    } else if revs.len() == 2 {
        let a_oid = store
            .resolve_commit(&revs[0])
            .map_err(|e| io::Error::other(format!("{e}")))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("unknown rev `{}`", revs[0])))?;
        let b_oid = store
            .resolve_commit(&revs[1])
            .map_err(|e| io::Error::other(format!("{e}")))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("unknown rev `{}`", revs[1])))?;
        Some((read_commit_tree(&store, a_oid, alloc)?, read_commit_tree(&store, b_oid, alloc)?))
    } else {
        None
    };

    if let Some((lt, rt)) = tree_pair {
        for (path, a, b) in diff_trees_changed(&store, Some(lt), Some(rt), alloc)? {
            if !path_matches_filters(&path, &filters) {
                continue;
            }
            let old_b = if name_only || name_status {
                a.map(|_| Vec::new())
            } else {
                a.and_then(|(_, o)| read_blob_bytes(&store, o, alloc).ok())
            };
            let new_b = if name_only || name_status {
                b.map(|_| vec![1u8])
            } else {
                b.and_then(|(_, o)| read_blob_bytes(&store, o, alloc).ok())
            };
            emit_diff_entry(
                &path,
                old_b.as_deref(),
                new_b.as_deref(),
                name_only,
                name_status,
                stat_only,
                use_color,
                &mut stat_rows,
                io.out,
            )?;
        }
        if stat_only && !stat_rows.is_empty() {
            write_diffstat(&stat_rows, use_color, io.out)?;
        }
        return Ok(());
    }

    // Remaining modes: Tree vs Index (`--staged` or `<rev> --staged`) or Tree vs Worktree (`<rev>`).
    let left_tree = if let Some(rev) = revs.first() {
        let oid = store
            .resolve_commit(rev)
            .map_err(|e| io::Error::other(format!("{e}")))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("unknown rev `{rev}`")))?;
        flatten_tree(&store, read_commit_tree(&store, oid, alloc)?, alloc)?
    } else {
        head_flat_tree(&store, alloc)?
    };
    let idx = load_index(&repo_root, alloc)?;

    let mut paths: BTreeSet<&str> = BTreeSet::new();
    for k in left_tree.keys() {
        paths.insert(k.as_str());
    }
    for k in idx.keys() {
        paths.insert(k.as_str());
    }

    for path in paths {
        if !path_matches_filters(path, &filters) {
            continue;
        }
        let left_oid = left_tree.get(path).map(|&(_, o)| o);
        let idx_entry = idx.get(path);

        if staged {
            let right_oid = idx_entry.map(|e| e.oid);
            // OID short-circuit: skip identical blobs with ZERO decompression!
            if left_oid == right_oid {
                continue;
            }
            let old_b = if name_only || name_status {
                left_oid.map(|_| Vec::new())
            } else {
                left_oid.and_then(|o| read_blob_bytes(&store, o, alloc).ok())
            };
            let new_b = if name_only || name_status {
                right_oid.map(|_| vec![1u8])
            } else {
                right_oid.and_then(|o| read_blob_bytes(&store, o, alloc).ok())
            };
            emit_diff_entry(
                path,
                old_b.as_deref(),
                new_b.as_deref(),
                name_only,
                name_status,
                stat_only,
                use_color,
                &mut stat_rows,
                io.out,
            )?;
        } else {
            // Tree vs Worktree: if index matches left_tree AND disk is stat-clean, skip with ZERO reads!
            if let Some(e) = idx_entry
                && Some(e.oid) == left_oid
                && index_entry_stat_clean(&repo_root, e)
            {
                continue;
            }
            let full = repo_root.join(path);
            let new_bytes = if full.exists() || full.is_symlink() {
                read_worktree_blob(&full).ok().map(|(_, b)| b)
            } else {
                None
            };
            if let (Some(lo), Some(nb)) = (left_oid, &new_bytes)
                && Oid::of(ObjectKind::Blob, nb) == lo
            {
                continue;
            }
            let old_b = left_oid.and_then(|o| read_blob_bytes(&store, o, alloc).ok());
            emit_diff_entry(
                path,
                old_b.as_deref(),
                new_bytes.as_deref(),
                name_only,
                name_status,
                stat_only,
                use_color,
                &mut stat_rows,
                io.out,
            )?;
        }
    }

    if stat_only && !stat_rows.is_empty() {
        write_diffstat(&stat_rows, use_color, io.out)?;
    }
    Ok(())
}

fn write_diffstat(
    rows: &[(String, usize, usize)],
    use_color: bool,
    out: &mut dyn Write,
) -> io::Result<()> {
    let max_path = rows.iter().map(|(p, _, _)| p.len()).max().unwrap_or(0).min(60);
    let (green, red, reset) = if use_color {
        ("\x1b[32m", "\x1b[31m", "\x1b[0m")
    } else {
        ("", "", "")
    };
    let mut tot_ins = 0usize;
    let mut tot_del = 0usize;
    for (p, ins, del) in rows {
        tot_ins += *ins;
        tot_del += *del;
        let tot = ins + del;
        let scale = if tot > 40 { 40.0 / tot as f32 } else { 1.0 };
        let plus_n = ((*ins as f32 * scale).round() as usize).min(40);
        let minus_n = ((*del as f32 * scale).round() as usize).min(40 - plus_n);
        writeln!(
            out,
            " {p:<max_path$} | {tot:>4} {green}{}{reset}{red}{}{reset}",
            "+".repeat(plus_n),
            "-".repeat(minus_n)
        )?;
    }
    writeln!(
        out,
        " {} file(s) changed, {tot_ins} insertion(s)(+), {tot_del} deletion(s)(-)",
        rows.len()
    )
}

pub(crate) fn cmd_show<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    use_color: bool,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let stat_only = matches.flag("stat");
    let name_only = matches.flag("name-only");
    let rev = matches.positional(0).unwrap_or("HEAD");
    let (_repo_root, store) = open_repo_from_cwd(cwd, alloc)?;

    // Support `<rev>:<path>` blob inspection!
    if let Some((rev_part, file_path)) = rev.split_once(':') {
        let base_rev = if rev_part.is_empty() { "HEAD" } else { rev_part };
        let commit_oid = store
            .resolve_commit(base_rev)
            .map_err(|e| io::Error::other(format!("{e}")))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("unknown revision `{base_rev}`")))?;
        let tree_oid = read_commit_tree(&store, commit_oid, alloc)?;
        let flat = flatten_tree(&store, tree_oid, alloc)?;
        let norm = file_path.trim_start_matches('/');
        let &(_, blob_oid) = flat.get(norm).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("path '{norm}' does not exist in '{base_rev}'"),
            )
        })?;
        let bytes = read_blob_bytes(&store, blob_oid, alloc)?;
        io.out.write_all(&bytes)?;
        return Ok(());
    }

    let oid = store
        .resolve_rev(rev)
        .map_err(|e| io::Error::other(format!("{e}")))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("unknown revision `{rev}`")))?;
    let oid = blaze_git::storage::peel_tag(&store, oid, 8, alloc)
        .map_err(|e| io::Error::other(format!("{e}")))?
        .unwrap_or(oid);

    let mut buf = OwnVec::new_in(alloc);
    let kind = store
        .read_object(&oid, &mut buf)
        .map_err(|e| io::Error::other(format!("{e}")))?;

    match kind {
        Some(ObjectKind::Blob) => {
            io.out.write_all(&buf)?;
            Ok(())
        }
        Some(ObjectKind::Tree) => {
            let flat = flatten_tree(&store, oid, alloc)?;
            for path in flat.keys() {
                writeln!(io.out, "{path}")?;
            }
            Ok(())
        }
        Some(ObjectKind::Commit) => {
            let parsed = Commit::parse(&buf)
                .map_err(|(_, why)| io::Error::new(io::ErrorKind::InvalidData, format!("{why:?}")))?;
            let text = core::str::from_utf8(&buf).unwrap_or("");
            let (yellow, reset) = if use_color { ("\x1b[33m", "\x1b[0m") } else { ("", "") };
            writeln!(io.out, "{yellow}commit {oid}{reset}")?;
            for line in text.lines() {
                if line.is_empty() {
                    break;
                }
                if let Some(rest) = line.strip_prefix("author ")
                    && let Some((who, tail)) = rest.rsplit_once('>')
                {
                    writeln!(io.out, "Author: {who}>")?;
                    writeln!(io.out, "Date:   {}", tail.trim())?;
                }
            }
            if let Some((_, body)) = text.split_once("\n\n") {
                writeln!(io.out)?;
                for line in body.lines() {
                    writeln!(io.out, "    {line}")?;
                }
                writeln!(io.out)?;
            }

            let parent_tree = match parsed.parents().next() {
                Some(p) => Some(read_commit_tree(&store, p, alloc)?),
                None => None,
            };
            let changed = diff_trees_changed(&store, parent_tree, Some(parsed.tree), alloc)?;

            let mut stat_rows = Vec::new();
            for (path, a, b) in changed {
                if name_only {
                    writeln!(io.out, "{path}")?;
                    continue;
                }
                let old_b = a.and_then(|(_, o)| read_blob_bytes(&store, o, alloc).ok());
                let new_b = b.and_then(|(_, o)| read_blob_bytes(&store, o, alloc).ok());
                if stat_only {
                    let (ins, del) = diff_line_stats(
                        old_b.as_deref().unwrap_or(b""),
                        new_b.as_deref().unwrap_or(b""),
                    );
                    stat_rows.push((path, ins, del));
                } else {
                    write!(
                        io.out,
                        "{}",
                        format_unified_diff(&path, old_b.as_deref(), new_b.as_deref(), use_color)
                    )?;
                }
            }
            if stat_only && !stat_rows.is_empty() {
                write_diffstat(&stat_rows, use_color, io.out)?;
            }
            Ok(())
        }
        _ => Err(io::Error::new(io::ErrorKind::NotFound, format!("object `{rev}` not found"))),
    }
}

// ============================================================================
// 7. `bgit reset` & 8. `bgit restore`
// ============================================================================

pub(crate) fn cmd_reset<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let soft = matches.flag("soft");
    let hard = matches.flag("hard");
    let mixed = matches.flag("mixed") || (!soft && !hard);
    let pos = matches.positionals();

    let (repo_root, mut store) = open_repo_from_cwd(cwd, alloc)?;

    // Determine if we are unstaging specific paths (`bgit reset <path>...` or `bgit reset <rev> <path>...`).
    let mut target_rev = "HEAD";
    let mut path_args: &[&str] = &[];
    if !pos.is_empty() {
        if !soft && !hard && store.resolve_commit(pos[0]).ok().flatten().is_none() {
            path_args = pos;
        } else {
            target_rev = pos[0];
            if pos.len() > 1 {
                path_args = &pos[1..];
            }
        }
    }

    if !path_args.is_empty() {
        return restore_paths_inner(
            &repo_root,
            &store,
            cwd,
            Some(target_rev),
            true,
            false,
            path_args,
            alloc,
            io,
        );
    }

    let target_oid = store
        .resolve_commit(target_rev)
        .map_err(|e| io::Error::other(format!("{e}")))?
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("unknown revision `{target_rev}`"))
        })?;
    let target_tree = read_commit_tree(&store, target_oid, alloc)?;
    let target_flat = flatten_tree(&store, target_tree, alloc)?;

    if hard {
        let old_flat = head_flat_tree(&store, alloc)?;
        checkout_tree_to_worktree(&repo_root, &store, &old_flat, &target_flat, true, alloc)?;
    }

    if mixed || hard {
        let new_idx = index_from_flat_tree(&repo_root, &target_flat);
        write_index(&repo_root, &new_idx)?;
    }

    // Move active branch ref (or detached HEAD).
    if let Some(full_ref) = current_head_ref(&store, alloc) {
        apply_single_ref_update(&mut store, &repo_root, &full_ref, target_oid, alloc)?;
    } else {
        let direct = RefTarget::Direct(target_oid);
        update_head_target(&mut store, &repo_root, &direct, alloc)?;
    }

    if hard {
        writeln!(
            io.out,
            "HEAD is now at {} {}",
            ShortOid::from_oid(target_oid, 7),
            read_commit_subject(&store, target_oid, alloc)
        )?;
    }
    Ok(())
}

pub(crate) fn cmd_restore<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let staged = matches.flag("staged");
    let worktree = matches.flag("worktree") || !staged;
    let source = matches.opt("source");
    let pos = matches.positionals();
    if pos.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "specify one or more paths to restore",
        ));
    }
    let (repo_root, store) = open_repo_from_cwd(cwd, alloc)?;
    restore_paths_inner(&repo_root, &store, cwd, source, staged, worktree, pos, alloc, io)
}

#[allow(clippy::too_many_arguments)]
fn restore_paths_inner<'a>(
    repo_root: &Path,
    store: &BgitDiskStore<'a>,
    cwd: &Path,
    source_rev: Option<&str>,
    restore_staged: bool,
    restore_worktree: bool,
    paths: &[&str],
    alloc: Alloc<'a>,
    _io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let mut filters = Vec::with_capacity(paths.len());
    for &p in paths {
        filters.push(to_repo_relative(repo_root, cwd, p)?);
    }

    let mut index = load_index(repo_root, alloc)?;
    let source_tree_map: Option<BTreeMap<String, (u32, Oid)>> =
        if let Some(rev) = source_rev {
            let oid = store
                .resolve_commit(rev)
                .map_err(|e| io::Error::other(format!("{e}")))?
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, format!("unknown revision `{rev}`"))
                })?;
            Some(flatten_tree(store, read_commit_tree(store, oid, alloc)?, alloc)?)
        } else if restore_staged {
            Some(head_flat_tree(store, alloc)?)
        } else {
            None
        };

    if restore_staged {
        let src = source_tree_map.as_ref().expect("source tree computed");
        let existing_keys: Vec<String> = index
            .keys()
            .filter(|p| path_matches_filters(p, &filters))
            .cloned()
            .collect();
        for k in existing_keys {
            if !src.contains_key(&k) {
                index.remove(&k);
            }
        }
        for (p, &(mode, oid)) in src {
            if path_matches_filters(p, &filters) {
                let mut e = stat_to_index_entry(repo_root, p, oid).unwrap_or(WritableIndexEntry {
                    ctime_sec: 0,
                    ctime_nsec: 0,
                    mtime_sec: 0,
                    mtime_nsec: 0,
                    dev: 0,
                    ino: 0,
                    mode,
                    uid: 0,
                    gid: 0,
                    file_size: 0,
                    oid,
                    path: p.clone(),
                });
                e.mode = mode;
                index.insert(p.clone(), e);
            }
        }
        write_index(repo_root, &index)?;
    }

    if restore_worktree {
        let mut buf = OwnVec::new_in(alloc);
        let mut restored_any = false;
        if let Some(src) = &source_tree_map {
            for (p, &(mode, oid)) in src {
                if path_matches_filters(p, &filters) {
                    write_blob_to_worktree(store, &repo_root.join(p), mode, oid, &mut buf)?;
                    restored_any = true;
                }
            }
        } else {
            for (p, entry) in &index {
                if path_matches_filters(p, &filters) {
                    write_blob_to_worktree(
                        store,
                        &repo_root.join(p),
                        entry.mode,
                        entry.oid,
                        &mut buf,
                    )?;
                    restored_any = true;
                }
            }
        }
        if !restored_any {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("pathspec '{}' did not match any tracked file(s)", paths.join(", ")),
            ));
        }
    }
    Ok(())
}

// ============================================================================
// 9. `bgit rm` & 10. `bgit mv`
// ============================================================================

pub(crate) fn cmd_rm<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let cached = matches.flag("cached");
    let recursive = matches.flag("recursive");
    let pos = matches.positionals();
    if pos.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "specify path(s) to remove"));
    }
    let (repo_root, _store) = open_repo_from_cwd(cwd, alloc)?;
    let mut index = load_index(&repo_root, alloc)?;
    let mut filters = Vec::with_capacity(pos.len());
    for &p in pos {
        let rel = to_repo_relative(&repo_root, cwd, p)?;
        if !recursive && repo_root.join(&rel).is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not removing '{p}' recursively without -r"),
            ));
        }
        filters.push(rel);
    }

    let targets: Vec<String> = index
        .keys()
        .filter(|p| path_matches_filters(p, &filters))
        .cloned()
        .collect();
    if targets.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("pathspec '{}' did not match any tracked files", pos.join(", ")),
        ));
    }
    for rel in targets {
        index.remove(&rel);
        if !cached {
            let full = repo_root.join(&rel);
            let _ = fs::remove_file(&full);
            prune_empty_parents(&repo_root, full.parent());
        }
        writeln!(io.out, "rm '{rel}'")?;
    }
    write_index(&repo_root, &index)?;
    Ok(())
}

pub(crate) fn cmd_mv<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    _io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let force = matches.flag("force");
    let src_raw = matches.positional(0).expect("required SRC");
    let dst_raw = matches.positional(1).expect("required DST");
    let (repo_root, _store) = open_repo_from_cwd(cwd, alloc)?;
    let mut index = load_index(&repo_root, alloc)?;

    let src_rel = to_repo_relative(&repo_root, cwd, src_raw)?;
    let mut dst_rel = to_repo_relative(&repo_root, cwd, dst_raw)?;
    let dst_abs_check = repo_root.join(&dst_rel);
    if dst_abs_check.is_dir()
        && let Some(fname) = Path::new(&src_rel).file_name()
    {
        dst_rel = if dst_rel.is_empty() {
            fname.to_string_lossy().into_owned()
        } else {
            format!("{dst_rel}/{}", fname.to_string_lossy())
        };
    }

    let src_abs = repo_root.join(&src_rel);
    let dst_abs = repo_root.join(&dst_rel);
    if !src_abs.exists() && !src_abs.is_symlink() {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("bad source '{src_raw}'")));
    }
    if (dst_abs.exists() || dst_abs.is_symlink()) && !force {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("destination '{dst_rel}' already exists (pass -f to overwrite)"),
        ));
    }
    if let Some(parent) = dst_abs.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&src_abs, &dst_abs)?;

    if let Some(mut entry) = index.remove(&src_rel) {
        entry.path = dst_rel.clone();
        if let Ok(updated) = stat_to_index_entry(&repo_root, &dst_rel, entry.oid) {
            entry = updated;
        }
        index.insert(dst_rel, entry);
        write_index(&repo_root, &index)?;
    }
    Ok(())
}

// ============================================================================
// 11. `bgit tag`
// ============================================================================

pub(crate) fn cmd_tag<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let delete = matches.flag("delete");
    let annotated = matches.flag("annotate");
    let force = matches.flag("force");
    let list_flag = matches.flag("list");
    let msg_opt = matches.opt("message");
    let pos = matches.positionals();

    let (repo_root, mut store) = open_repo_from_cwd(cwd, alloc)?;

    if delete {
        if pos.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "tag name required for -d"));
        }
        for &name in pos {
            let full = format!("refs/tags/{name}");
            let Some(oid) = store
                .resolve_ref(&full)
                .map_err(|e| io::Error::other(format!("{e}")))?
            else {
                return Err(io::Error::new(io::ErrorKind::NotFound, format!("tag '{name}' not found")));
            };
            apply_single_ref_update(&mut store, &repo_root, &full, ZERO_OID, alloc)?;
            writeln!(io.out, "Deleted tag '{name}' (was {})", ShortOid::from_oid(oid, 7))?;
        }
        return Ok(());
    }

    if list_flag || pos.is_empty() {
        let mut refs = OwnVec::new_in(alloc);
        store
            .list_refs("refs/tags/", alloc, &mut refs)
            .map_err(|e| io::Error::other(format!("{e}")))?;
        let mut names: Vec<String> = refs
            .iter()
            .filter_map(|r| r.name.as_str().strip_prefix("refs/tags/").map(str::to_owned))
            .collect();
        names.sort();
        for name in names {
            writeln!(io.out, "{name}")?;
        }
        return Ok(());
    }

    let name = pos[0];
    let target_rev = pos.get(1).copied().unwrap_or("HEAD");
    let full = format!("refs/tags/{name}");
    if !ref_name_safe(&full) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("'{name}' is not a valid tag name"),
        ));
    }
    if !force
        && store
            .resolve_ref(&full)
            .map_err(|e| io::Error::other(format!("{e}")))?
            .is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("tag '{name}' already exists (pass -f to replace)"),
        ));
    }

    let target_oid = store
        .resolve_rev(target_rev)
        .map_err(|e| io::Error::other(format!("{e}")))?
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("unknown revision `{target_rev}`"))
        })?;

    let final_oid = if annotated || msg_opt.is_some() {
        let msg = msg_opt.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "annotated tag requires -m <MSG>")
        })?;
        let (who, email, ts) = resolve_identity(&repo_root);
        let mut body = format!(
            "object {target_oid}\ntype commit\ntag {name}\ntagger {who} <{email}> {ts} +0000\n\n{msg}"
        );
        if !body.ends_with('\n') {
            body.push('\n');
        }
        store
            .write_object(ObjectKind::Tag, body.as_bytes())
            .map_err(|e| io::Error::other(format!("write tag object: {e}")))?
    } else {
        target_oid
    };

    apply_single_ref_update(&mut store, &repo_root, &full, final_oid, alloc)?;
    Ok(())
}

// ============================================================================
// 12. `bgit rev-parse` & 13. `bgit show-ref`
// ============================================================================

pub(crate) fn cmd_rev_parse<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let show_toplevel = matches.flag("show-toplevel");
    let git_dir_flag = matches.flag("git-dir");
    let inside_wt = matches.flag("is-inside-work-tree");
    let abbrev_ref = matches.flag("abbrev-ref");
    let short_opt = matches.opt("short");
    let pos = matches.positionals();

    let (repo_root, store) = open_repo_from_cwd(cwd, alloc)?;

    if show_toplevel {
        writeln!(io.out, "{}", repo_root.display())?;
    }
    if inside_wt {
        writeln!(io.out, "true")?;
    }
    if git_dir_flag {
        let gd = locate_git_dir(&repo_root).unwrap_or_else(|_| repo_root.join(".bgit"));
        let rel = gd.strip_prefix(cwd).unwrap_or(&gd);
        writeln!(io.out, "{}", rel.display())?;
    }

    for &rev in pos {
        if abbrev_ref && (rev == "HEAD" || rev == "@") {
            match store.read_head(alloc).map_err(|e| io::Error::other(format!("{e}")))? {
                Some(RefTarget::Symbolic(sym)) => {
                    let s = sym.as_str().strip_prefix("refs/heads/").unwrap_or(sym.as_str());
                    writeln!(io.out, "{s}")?;
                }
                _ => writeln!(io.out, "HEAD")?,
            }
            continue;
        }
        let oid = store
            .resolve_rev(rev)
            .map_err(|e| io::Error::other(format!("{e}")))?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("ambiguous argument '{rev}': unknown revision"),
                )
            })?;
        if let Some(len_str) = short_opt {
            let n = len_str.parse::<usize>().unwrap_or(7).clamp(4, 40);
            writeln!(io.out, "{}", ShortOid::from_oid(oid, n as u8))?;
        } else {
            writeln!(io.out, "{oid}")?;
        }
    }
    Ok(())
}

pub(crate) fn cmd_show_ref<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let heads = matches.flag("heads");
    let tags = matches.flag("tags");
    let hash_only = matches.flag("hash");
    let (_repo_root, store) = open_repo_from_cwd(cwd, alloc)?;

    let mut refs = OwnVec::new_in(alloc);
    store
        .list_refs("refs/", alloc, &mut refs)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    let mut rows: Vec<(String, Oid)> = Vec::new();
    for r in &refs {
        let name = r.name.as_str();
        if heads && !name.starts_with("refs/heads/") {
            continue;
        }
        if tags && !name.starts_with("refs/tags/") {
            continue;
        }
        let oid = match &r.target {
            RefTarget::Direct(o) => Some(*o),
            RefTarget::Symbolic(_) => store.resolve_ref(name).ok().flatten(),
        };
        if let Some(o) = oid {
            rows.push((name.to_owned(), o));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, oid) in rows {
        if hash_only {
            writeln!(io.out, "{oid}")?;
        } else {
            writeln!(io.out, "{oid} {name}")?;
        }
    }
    Ok(())
}

// ============================================================================
// 14. `bgit remote` & 15. `bgit config`
// ============================================================================

pub(crate) fn cmd_remote<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    _alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let repo_root = find_repo_root(cwd).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "not a git or bgit repository")
    })?;
    let cfg_path = GitConfig::config_path_for(&repo_root);
    let mut cfg = GitConfig::load_file(&cfg_path);

    let sub = matches.command_path().get(2).copied();
    match sub {
        None => {
            let verbose = matches.flag("verbose");
            for (name, url) in cfg.remotes() {
                if verbose {
                    writeln!(io.out, "{name}\t{url} (fetch)")?;
                    writeln!(io.out, "{name}\t{url} (push)")?;
                } else {
                    writeln!(io.out, "{name}")?;
                }
            }
            Ok(())
        }
        Some("add") => {
            let name = matches.positional(0).expect("required NAME");
            let url = matches.positional(1).expect("required URL");
            let key = format!("remote.{name}.url");
            if cfg.get(&key).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("remote {name} already exists"),
                ));
            }
            cfg.set(&key, url)?;
            cfg.set(
                &format!("remote.{name}.fetch"),
                &format!("+refs/heads/*:refs/remotes/{name}/*"),
            )?;
            cfg.save_file(&cfg_path)
        }
        Some("remove") => {
            let name = matches.positional(0).expect("required NAME");
            if !cfg.remove_section("remote", Some(name)) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such remote: '{name}'"),
                ));
            }
            cfg.save_file(&cfg_path)
        }
        Some("rename") => {
            let old = matches.positional(0).expect("required OLD");
            let new = matches.positional(1).expect("required NEW");
            if !cfg.rename_subsection("remote", old, new) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such remote: '{old}'"),
                ));
            }
            cfg.save_file(&cfg_path)
        }
        Some("get-url") => {
            let name = matches.positional(0).expect("required NAME");
            let key = format!("remote.{name}.url");
            let url = cfg.get(&key).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("no such remote: '{name}'"))
            })?;
            writeln!(io.out, "{url}")
        }
        Some("set-url") => {
            let name = matches.positional(0).expect("required NAME");
            let url = matches.positional(1).expect("required URL");
            let key = format!("remote.{name}.url");
            if cfg.get(&key).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such remote: '{name}'"),
                ));
            }
            cfg.set(&key, url)?;
            cfg.save_file(&cfg_path)
        }
        Some(other) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown remote subcommand `{other}`"),
        )),
    }
}

pub(crate) fn cmd_config<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    _alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let list = matches.flag("list");
    let unset = matches.flag("unset");
    let global = matches.flag("global");
    let local = matches.flag("local");
    let pos = matches.positionals();

    let target_path = if global {
        GitConfig::global_config_path().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "HOME is not set for --global config")
        })?
    } else {
        let root = find_repo_root(cwd).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "not a git or bgit repository")
        })?;
        GitConfig::config_path_for(&root)
    };

    if list {
        let cfg = if global || local {
            GitConfig::load_file(&target_path)
        } else if let Some(root) = find_repo_root(cwd) {
            GitConfig::load_merged(&root)
        } else {
            GitConfig::load_file(&target_path)
        };
        for (k, v) in cfg.format_list() {
            writeln!(io.out, "{k}={v}")?;
        }
        return Ok(());
    }

    let key = pos.first().copied().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "usage: bgit config <key> [<value>]")
    })?;

    if unset {
        let mut cfg = GitConfig::load_file(&target_path);
        if !cfg.unset(key)? {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("key '{key}' not found")));
        }
        return cfg.save_file(&target_path);
    }

    if let Some(&val) = pos.get(1) {
        let mut cfg = GitConfig::load_file(&target_path);
        cfg.set(key, val)?;
        cfg.save_file(&target_path)
    } else {
        let cfg = if global || local {
            GitConfig::load_file(&target_path)
        } else if let Some(root) = find_repo_root(cwd) {
            GitConfig::load_merged(&root)
        } else {
            GitConfig::load_file(&target_path)
        };
        let val = cfg.get(key).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("config key '{key}' not set"))
        })?;
        writeln!(io.out, "{val}")
    }
}

// ============================================================================
// 16. `bgit stash`
// ============================================================================

fn stash_log_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".bgit/stash.log")
}

fn read_stash_entries(repo_root: &Path) -> Vec<(Oid, String)> {
    let Ok(raw) = fs::read_to_string(stash_log_path(repo_root)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in raw.lines() {
        let Some((hex, msg)) = line.split_once(' ') else {
            continue;
        };
        if let Some(oid) = Oid::parse_hex(hex.as_bytes()) {
            out.push((oid, msg.to_owned()));
        }
    }
    out
}

fn write_stash_entries(repo_root: &Path, entries: &[(Oid, String)]) -> io::Result<()> {
    let mut buf = String::new();
    for (oid, msg) in entries {
        buf.push_str(&format!("{oid} {msg}\n"));
    }
    write_file_atomic(&stash_log_path(repo_root), buf.as_bytes())
}

pub(crate) fn cmd_stash<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    use_color: bool,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let sub = matches.command_path().get(2).copied().unwrap_or("push");
    let (repo_root, mut store) = open_repo_from_cwd(cwd, alloc)?;

    match sub {
        "push" => {
            let include_untracked = matches.flag("include-untracked");
            let custom_msg = matches.opt("message");
            let head_oid = store
                .resolve_commit("HEAD")
                .map_err(|e| io::Error::other(format!("{e}")))?
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "cannot stash without a HEAD commit")
                })?;
            let head_flat = head_flat_tree(&store, alloc)?;
            let index = load_index(&repo_root, alloc)?;

            // Build working-tree snapshot map.
            let mut wt_flat: BTreeMap<String, (u32, Oid)> = BTreeMap::new();
            let wt_files = if include_untracked {
                walk_working_tree(&repo_root, false, alloc)?
            } else {
                index.keys().cloned().collect()
            };
            for rel in wt_files {
                let full = repo_root.join(&rel);
                if full.exists() || full.is_symlink() {
                    let (mode, bytes) = read_worktree_blob(&full)?;
                    let oid = store
                        .write_object(ObjectKind::Blob, &bytes)
                        .map_err(|e| io::Error::other(format!("{e}")))?;
                    wt_flat.insert(rel, (mode, oid));
                }
            }
            if wt_flat == head_flat {
                writeln!(io.out, "No local changes to save")?;
                return Ok(());
            }

            let wt_tree = build_tree_from_map(&mut store, &wt_flat, alloc)?;
            let branch = current_head_branch(&store, alloc).unwrap_or_else(|| "HEAD".to_owned());
            let subj = read_commit_subject(&store, head_oid, alloc);
            let label = match custom_msg {
                Some(m) => format!("On {branch}: {m}"),
                None => format!("WIP on {branch}: {} {subj}", ShortOid::from_oid(head_oid, 7)),
            };
            let (name, email, ts) = resolve_identity(&repo_root);
            let sig = make_signature(&name, &email, ts);
            let mut cbuf = OwnVec::new_in(alloc);
            encode_commit(wt_tree, &[head_oid], sig, sig, &format!("{label}\n"), &mut cbuf);
            let stash_oid = store
                .write_object(ObjectKind::Commit, &cbuf)
                .map_err(|e| io::Error::other(format!("{e}")))?;

            apply_single_ref_update(&mut store, &repo_root, "refs/stash", stash_oid, alloc)?;
            let mut entries = read_stash_entries(&repo_root);
            entries.insert(0, (stash_oid, label.clone()));
            write_stash_entries(&repo_root, &entries)?;

            // Restore working tree & index cleanly back to HEAD.
            checkout_tree_to_worktree(&repo_root, &store, &wt_flat, &head_flat, true, alloc)?;
            let clean_idx = index_from_flat_tree(&repo_root, &head_flat);
            write_index(&repo_root, &clean_idx)?;
            writeln!(io.out, "Saved working directory and index state {label}")?;
            Ok(())
        }
        "list" => {
            let entries = read_stash_entries(&repo_root);
            for (idx, (_, msg)) in entries.iter().enumerate() {
                writeln!(io.out, "stash@{{{idx}}}: {msg}")?;
            }
            Ok(())
        }
        "apply" | "pop" => {
            let idx = parse_stash_index(matches.positional(0))?;
            let mut entries = read_stash_entries(&repo_root);
            if idx >= entries.len() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("stash@{{{idx}}} does not exist"),
                ));
            }
            let (stash_oid, msg) = entries[idx].clone();
            let stash_tree = read_commit_tree(&store, stash_oid, alloc)?;
            let stash_flat = flatten_tree(&store, stash_tree, alloc)?;
            let head_flat = head_flat_tree(&store, alloc)?;
            checkout_tree_to_worktree(&repo_root, &store, &head_flat, &stash_flat, true, alloc)?;
            if sub == "pop" {
                entries.remove(idx);
                write_stash_entries(&repo_root, &entries)?;
                let top = entries.first().map(|(o, _)| *o).unwrap_or(ZERO_OID);
                apply_single_ref_update(&mut store, &repo_root, "refs/stash", top, alloc)?;
                writeln!(io.out, "Dropped stash@{{{idx}}} ({}) — {msg}", ShortOid::from_oid(stash_oid, 7))?;
            }
            Ok(())
        }
        "drop" => {
            let idx = parse_stash_index(matches.positional(0))?;
            let mut entries = read_stash_entries(&repo_root);
            if idx >= entries.len() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("stash@{{{idx}}} does not exist"),
                ));
            }
            let (oid, _) = entries.remove(idx);
            write_stash_entries(&repo_root, &entries)?;
            let top = entries.first().map(|(o, _)| *o).unwrap_or(ZERO_OID);
            apply_single_ref_update(&mut store, &repo_root, "refs/stash", top, alloc)?;
            writeln!(io.out, "Dropped stash@{{{idx}}} ({oid})")?;
            Ok(())
        }
        "show" => {
            let idx = parse_stash_index(matches.positional(0))?;
            let entries = read_stash_entries(&repo_root);
            let (stash_oid, _) = entries.get(idx).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("stash@{{{idx}}} does not exist"))
            })?;
            let mut buf = OwnVec::new_in(alloc);
            store
                .read_object(stash_oid, &mut buf)
                .map_err(|e| io::Error::other(format!("{e}")))?;
            let parsed = Commit::parse(&buf)
                .map_err(|(_, why)| io::Error::new(io::ErrorKind::InvalidData, format!("{why:?}")))?;
            let base_flat = match parsed.parents().next() {
                Some(p) => flatten_tree(&store, read_commit_tree(&store, p, alloc)?, alloc)?,
                None => BTreeMap::new(),
            };
            let cur_flat = flatten_tree(&store, parsed.tree, alloc)?;
            for path in union_paths(&base_flat, &cur_flat) {
                let a = base_flat.get(path);
                let b = cur_flat.get(path);
                if a == b {
                    continue;
                }
                let old_b = a.and_then(|&(_, o)| read_blob_bytes(&store, o, alloc).ok());
                let new_b = b.and_then(|&(_, o)| read_blob_bytes(&store, o, alloc).ok());
                write!(
                    io.out,
                    "{}",
                    format_unified_diff(path, old_b.as_deref(), new_b.as_deref(), use_color)
                )?;
            }
            Ok(())
        }
        "clear" => {
            write_stash_entries(&repo_root, &[])?;
            apply_single_ref_update(&mut store, &repo_root, "refs/stash", ZERO_OID, alloc)?;
            Ok(())
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown stash subcommand `{other}`"),
        )),
    }
}

fn parse_stash_index(raw: Option<&str>) -> io::Result<usize> {
    let Some(s) = raw else {
        return Ok(0);
    };
    let inner = s
        .strip_prefix("stash@{")
        .and_then(|r| r.strip_suffix('}'))
        .unwrap_or(s);
    inner.parse::<usize>().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("invalid stash reference `{s}`"))
    })
}

// ============================================================================
// 17. `bgit cherry-pick` & 18. `bgit revert`
// ============================================================================

pub(crate) fn cmd_cherry_pick<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    apply_commit_delta(matches, cwd, false, alloc, io)
}

pub(crate) fn cmd_revert<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    apply_commit_delta(matches, cwd, true, alloc, io)
}

fn apply_commit_delta<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    is_revert: bool,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let no_commit = matches.flag("no-commit");
    let rev = matches.positional(0).expect("required REV");
    let (repo_root, mut store) = open_repo_from_cwd(cwd, alloc)?;

    let head_oid = store
        .resolve_commit("HEAD")
        .map_err(|e| io::Error::other(format!("{e}")))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no HEAD commit"))?;
    let target_oid = store
        .resolve_commit(rev)
        .map_err(|e| io::Error::other(format!("{e}")))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("unknown revision `{rev}`")))?;

    let mut cbuf = OwnVec::new_in(alloc);
    store
        .read_object(&target_oid, &mut cbuf)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    let parsed = Commit::parse(&cbuf)
        .map_err(|(_, why)| io::Error::new(io::ErrorKind::InvalidData, format!("{why:?}")))?;
    let orig_msg = core::str::from_utf8(&cbuf)
        .ok()
        .and_then(|t| t.split_once("\n\n").map(|(_, m)| m.to_owned()))
        .unwrap_or_default();
    let parent_tree = match parsed.parents().next() {
        Some(p) => Some(read_commit_tree(&store, p, alloc)?),
        None => None,
    };
    let head_tree = read_commit_tree(&store, head_oid, alloc)?;

    let (base_t, theirs_t) = if is_revert {
        (Some(parsed.tree), parent_tree)
    } else {
        (parent_tree, Some(parsed.tree))
    };

    let res = merge_trees(
        &mut store,
        base_t,
        Some(head_tree),
        theirs_t,
        MergeOpts::DEFAULT,
        alloc,
    )
    .map_err(|e| io::Error::other(format!("ORT merge: {e}")))?;

    if res.status == MergeStatus::Conflicted {
        let mut msg = format!("conflicts while {} {rev}:\n", if is_revert { "reverting" } else { "cherry-picking" });
        for c in &res.conflicts {
            msg.push_str(&format!("  {} ({:?})\n", c.path.as_str(), c.kind));
        }
        return Err(io::Error::new(io::ErrorKind::Other, msg));
    }

    let old_flat = flatten_tree(&store, head_tree, alloc)?;
    let new_flat = flatten_tree(&store, res.tree, alloc)?;
    checkout_tree_to_worktree(&repo_root, &store, &old_flat, &new_flat, false, alloc)?;
    let new_idx = index_from_flat_tree(&repo_root, &new_flat);
    write_index(&repo_root, &new_idx)?;

    if no_commit {
        mirror_overlay_objects_to_git(&store, &repo_root, alloc);
        store.flush()?;
        return Ok(());
    }

    let commit_msg = if is_revert {
        let subj = orig_msg.lines().next().unwrap_or("");
        format!("Revert \"{subj}\"\n\nThis reverts commit {target_oid}.\n")
    } else if orig_msg.ends_with('\n') {
        orig_msg
    } else {
        format!("{orig_msg}\n")
    };
    let (name, email, ts) = resolve_identity(&repo_root);
    let sig = make_signature(&name, &email, ts);
    let mut out_commit = OwnVec::new_in(alloc);
    encode_commit(res.tree, &[head_oid], sig, sig, &commit_msg, &mut out_commit);
    let new_oid = store
        .write_object(ObjectKind::Commit, &out_commit)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    if let Some(full_ref) = current_head_ref(&store, alloc) {
        apply_single_ref_update(&mut store, &repo_root, &full_ref, new_oid, alloc)?;
    } else {
        let direct = RefTarget::Direct(new_oid);
        update_head_target(&mut store, &repo_root, &direct, alloc)?;
    }
    writeln!(
        io.out,
        "[{} {}] {}",
        current_head_branch(&store, alloc).unwrap_or_else(|| "HEAD".to_owned()),
        ShortOid::from_oid(new_oid, 7),
        commit_msg.lines().next().unwrap_or("")
    )?;
    Ok(())
}

// ============================================================================
// 19. `bgit clean`
// ============================================================================

pub(crate) fn cmd_clean<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let dry_run = matches.flag("dry-run");
    let force = matches.flag("force");
    let include_ignored = matches.flag("ignored");
    if !dry_run && !force {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to clean without -f/--force or -n/--dry-run",
        ));
    }
    let (repo_root, _store) = open_repo_from_cwd(cwd, alloc)?;
    let index = load_index(&repo_root, alloc)?;
    let wt_files = walk_working_tree(&repo_root, include_ignored, alloc)?;
    for rel in wt_files {
        if index.contains_key(&rel) {
            continue;
        }
        if dry_run {
            writeln!(io.out, "Would remove {rel}")?;
        } else {
            let full = repo_root.join(&rel);
            let _ = fs::remove_file(&full);
            prune_empty_parents(&repo_root, full.parent());
            writeln!(io.out, "Removing {rel}")?;
        }
    }
    Ok(())
}

// ============================================================================
// 20. `bgit fetch` & `bgit pull`
// ============================================================================

pub(crate) fn cmd_fetch<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let remote_name = matches.positional(0).unwrap_or("origin");
    let depth = matches
        .opt_parse::<u32>("depth")
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?;
    let filter = matches.opt("filter");
    let params = FetchParams { depth, filter };
    let repo_root = find_repo_root(cwd).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "not a git or bgit repository")
    })?;
    let cfg = GitConfig::load_merged(&repo_root);
    let url = if looks_like_remote_url(remote_name) {
        remote_name.to_owned()
    } else {
        cfg.get(&format!("remote.{remote_name}.url"))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("remote '{remote_name}' has no configured URL (add with `bgit remote add {remote_name} <url>`)"),
                )
            })?
            .to_owned()
    };
    let _ = remote_fetch(&url, remote_name, &repo_root, params, alloc, io.out)?;
    Ok(())
}

pub(crate) fn cmd_pull<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let remote_name = matches.positional(0).unwrap_or("origin");
    let explicit_branch = matches.positional(1);
    let repo_root = find_repo_root(cwd).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "not a git or bgit repository")
    })?;
    let cfg = GitConfig::load_merged(&repo_root);
    let url = cfg
        .get(&format!("remote.{remote_name}.url"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("remote '{remote_name}' has no configured URL"),
            )
        })?
        .to_owned();
    let _ = remote_fetch(&url, remote_name, &repo_root, FetchParams::default(), alloc, io.out)?;

    let mut store = open_or_infect(&repo_root, alloc)?;
    let branch = explicit_branch
        .map(str::to_owned)
        .or_else(|| current_head_branch(&store, alloc))
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "detached HEAD; specify remote branch to pull")
        })?;
    let remote_ref = format!("refs/remotes/{remote_name}/{branch}");
    let theirs_oid = store
        .resolve_ref(&remote_ref)
        .map_err(|e| io::Error::other(format!("{e}")))?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("remote ref `{remote_ref}` not found after fetch"),
            )
        })?;
    let ours_oid = store
        .resolve_commit("HEAD")
        .map_err(|e| io::Error::other(format!("{e}")))?;

    let full_branch_ref = format!("refs/heads/{branch}");
    let Some(ours) = ours_oid else {
        let new_flat = flatten_tree(&store, read_commit_tree(&store, theirs_oid, alloc)?, alloc)?;
        checkout_tree_to_worktree(&repo_root, &store, &BTreeMap::new(), &new_flat, false, alloc)?;
        write_index(&repo_root, &index_from_flat_tree(&repo_root, &new_flat))?;
        apply_single_ref_update(&mut store, &repo_root, &full_branch_ref, theirs_oid, alloc)?;
        return Ok(());
    };

    if ours == theirs_oid {
        writeln!(io.out, "Already up to date.")?;
        return Ok(());
    }

    let cg = store.commit_graph();
    let bases = find_merge_bases(&store, cg.as_ref(), ours, theirs_oid, MergeLimits::SHIPPED, alloc)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    if bases.iter().any(|b| *b == theirs_oid) {
        writeln!(io.out, "Already up to date.")?;
        return Ok(());
    }

    let old_flat = flatten_tree(&store, read_commit_tree(&store, ours, alloc)?, alloc)?;
    let final_oid = if bases.iter().any(|b| *b == ours) {
        // Fast-forward!
        let new_flat = flatten_tree(&store, read_commit_tree(&store, theirs_oid, alloc)?, alloc)?;
        checkout_tree_to_worktree(&repo_root, &store, &old_flat, &new_flat, false, alloc)?;
        write_index(&repo_root, &index_from_flat_tree(&repo_root, &new_flat))?;
        writeln!(
            io.out,
            "Fast-forward {}..{}",
            ShortOid::from_oid(ours, 7),
            ShortOid::from_oid(theirs_oid, 7)
        )?;
        theirs_oid
    } else {
        let res = blaze_git::merge::merge_commits(
            &mut store,
            None,
            ours,
            theirs_oid,
            MergeOpts::DEFAULT,
            alloc,
        )
        .map_err(|e| io::Error::other(format!("merge: {e}")))?;
        if res.status == MergeStatus::Conflicted {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("pull merge resulted in {} conflict(s)", res.conflicts.len()),
            ));
        }
        let new_flat = flatten_tree(&store, res.tree, alloc)?;
        checkout_tree_to_worktree(&repo_root, &store, &old_flat, &new_flat, false, alloc)?;
        write_index(&repo_root, &index_from_flat_tree(&repo_root, &new_flat))?;
        let (name, email, ts) = resolve_identity(&repo_root);
        let sig = make_signature(&name, &email, ts);
        let mut cbuf = OwnVec::new_in(alloc);
        encode_commit(
            res.tree,
            &[ours, theirs_oid],
            sig,
            sig,
            &format!("Merge remote-tracking branch '{remote_name}/{branch}'\n"),
            &mut cbuf,
        );
        store
            .write_object(ObjectKind::Commit, &cbuf)
            .map_err(|e| io::Error::other(format!("{e}")))?
    };

    apply_single_ref_update(&mut store, &repo_root, &full_branch_ref, final_oid, alloc)?;
    if repo_root.join(".bgit/worktrees.bgwt").is_file() {
        drop(store);
        let _ = on_branch_merged(
            &repo_root,
            Some(full_branch_ref.as_str()),
            &remote_ref,
            true,
            false,
            alloc,
            io,
        );
    }
    Ok(())
}

// ============================================================================
// Structured query helpers for `blaze-shell` rich tables (`branch`, `diff --stat`)
// ============================================================================

/// Returns structured branch rows (sorted, current branch marked, commit subject populated)
/// directly from `.bgit` mmap slabs (`BGRF` v2 O(log R + K)).
pub(crate) fn query_branches_in_repo<'a>(
    store: &BgitDiskStore<'a>,
    include_all: bool,
    remotes_only: bool,
    alloc: Alloc<'a>,
) -> io::Result<Vec<crate::BranchInfo>> {
    let active = current_head_branch(store, alloc);
    let ref_prefix = if include_all {
        "refs/"
    } else if remotes_only {
        "refs/remotes/"
    } else {
        "refs/heads/"
    };
    let mut refs = OwnVec::new_in(alloc);
    store
        .list_refs(ref_prefix, alloc, &mut refs)
        .map_err(|e| io::Error::other(format!("{e}")))?;

    let mut local: Vec<(String, Oid)> = Vec::new();
    let mut remote: Vec<(String, Oid)> = Vec::new();
    for r in &refs {
        let oid = match &r.target {
            RefTarget::Direct(o) => Some(*o),
            RefTarget::Symbolic(_) => store.resolve_ref(r.name.as_str()).ok().flatten(),
        };
        let Some(oid) = oid else {
            continue;
        };
        if let Some(short) = r.name.as_str().strip_prefix("refs/heads/") {
            local.push((short.to_owned(), oid));
        } else if let Some(short) = r.name.as_str().strip_prefix("refs/remotes/") {
            remote.push((short.to_owned(), oid));
        }
    }
    local.sort_by(|a, b| a.0.cmp(&b.0));
    remote.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = Vec::with_capacity(local.len() + remote.len());
    if !remotes_only {
        if active.is_none()
            && let Some(detached) =
                store.resolve_commit("HEAD").map_err(|e| io::Error::other(format!("{e}")))?
        {
            let short = ShortOid::from_oid(detached, 7).as_str().to_owned();
            let subject = read_commit_subject(store, detached, alloc);
            out.push(crate::BranchInfo {
                name: format!("(HEAD detached at {short})"),
                short_oid: short,
                oid: detached.to_string(),
                subject,
                is_current: true,
                is_remote: false,
            });
        }
        for (name, oid) in local {
            let is_current = active.as_deref() == Some(name.as_str());
            let subject = read_commit_subject(store, oid, alloc);
            out.push(crate::BranchInfo {
                name,
                short_oid: ShortOid::from_oid(oid, 7).as_str().to_owned(),
                oid: oid.to_string(),
                subject,
                is_current,
                is_remote: false,
            });
        }
    }
    if include_all || remotes_only {
        for (name, oid) in remote {
            let subject = read_commit_subject(store, oid, alloc);
            out.push(crate::BranchInfo {
                name: format!("remotes/{name}"),
                short_oid: ShortOid::from_oid(oid, 7).as_str().to_owned(),
                oid: oid.to_string(),
                subject,
                is_current: false,
                is_remote: true,
            });
        }
    }
    Ok(out)
}

/// Returns structured diffstat rows for working-tree vs index (`staged == false`) or `HEAD` vs
/// index (`staged == true`), using the in-process FSEvents O(dirty) fast path when active.
pub(crate) fn query_diff_stat_in_repo<'a>(
    repo_root: &Path,
    store: &BgitDiskStore<'a>,
    staged: bool,
    alloc: Alloc<'a>,
) -> io::Result<Vec<crate::DiffStatEntry>> {
    let mut rows = Vec::new();
    if !staged {
        let dirty: Vec<(WritableIndexEntry, Option<Vec<u8>>)> =
            match crate::fsmonitor::try_diff_candidates(repo_root, alloc) {
                Some(crate::fsmonitor::DiffFastPath::Clean) => Vec::new(),
                Some(crate::fsmonitor::DiffFastPath::Candidates(cands)) => {
                    let mut all_dirty_paths = BTreeSet::new();
                    let mut out = Vec::new();
                    for e in cands {
                        if let Some(d) = check_worktree_entry_dirty(repo_root, &e) {
                            all_dirty_paths.insert(e.path.clone());
                            out.push((e, d));
                        }
                    }
                    crate::fsmonitor::update_diff_known_dirty(repo_root, all_dirty_paths);
                    out
                }
                None => {
                    let scan_gen = crate::fsmonitor::begin_full_scan(repo_root);
                    let idx = load_index(repo_root, alloc)?;
                    let d = collect_dirty_worktree_entries(repo_root, &idx, &[]);
                    let dirty_names: BTreeSet<String> =
                        d.iter().map(|(e, _)| e.path.clone()).collect();
                    crate::fsmonitor::record_diff_baseline(repo_root, dirty_names, scan_gen);
                    d.into_iter().map(|(e, b)| (e.clone(), b)).collect()
                }
            };
        for (entry, new_opt) in dirty {
            let old_opt = read_blob_bytes(store, entry.oid, alloc).ok();
            let status = if new_opt.is_none() { 'D' } else { 'M' };
            let (ins, del) =
                diff_line_stats(old_opt.as_deref().unwrap_or(b""), new_opt.as_deref().unwrap_or(b""));
            rows.push(crate::DiffStatEntry {
                path: entry.path,
                status,
                insertions: ins,
                deletions: del,
            });
        }
        return Ok(rows);
    }

    let left_tree = head_flat_tree(store, alloc)?;
    let idx = load_index(repo_root, alloc)?;
    let mut paths: BTreeSet<&str> = BTreeSet::new();
    for k in left_tree.keys() {
        paths.insert(k.as_str());
    }
    for k in idx.keys() {
        paths.insert(k.as_str());
    }
    for path in paths {
        let left_oid = left_tree.get(path).map(|&(_, o)| o);
        let right_oid = idx.get(path).map(|e| e.oid);
        if left_oid == right_oid {
            continue;
        }
        let status = match (left_oid, right_oid) {
            (None, Some(_)) => 'A',
            (Some(_), None) => 'D',
            _ => 'M',
        };
        let old_b = left_oid.and_then(|o| read_blob_bytes(store, o, alloc).ok());
        let new_b = right_oid.and_then(|o| read_blob_bytes(store, o, alloc).ok());
        let (ins, del) =
            diff_line_stats(old_b.as_deref().unwrap_or(b""), new_b.as_deref().unwrap_or(b""));
        rows.push(crate::DiffStatEntry {
            path: path.to_owned(),
            status,
            insertions: ins,
            deletions: del,
        });
    }
    Ok(rows)
}
