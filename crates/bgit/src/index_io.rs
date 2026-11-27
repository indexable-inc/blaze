//! Native Git index (`DIRC` v2) I/O, tree construction/flattening, safe working-tree checkout,
//! `.gitignore`-aware directory walk, INI `.git/config` + `.bgit/config` handling, and Myers/LCS
//! unified diff engine for `bgit`'s porcelain commands.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use blaze_git::{
    Limits,
    ignore::IgnoreStack,
    index::parse_index,
    merge::is_binary,
    objects::TreeIter,
    storage::{ObjectStore, Signature, TreeBuildEntry, encode_tree, git_tree_name_cmp},
};
use blaze_git_fetch::{Kind as ObjectKind, Oid, sha1::sha1, store::Commit};
use blaze_own::{AVec, Alloc, Vec as OwnVec, planted::NoPlant};

use crate::disk::{BgitDiskStore, locate_git_dir, write_file_atomic};

/// One entry in a Git v2 `DIRC` index file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WritableIndexEntry {
    pub ctime_sec: u32,
    pub ctime_nsec: u32,
    pub mtime_sec: u32,
    pub mtime_nsec: u32,
    pub dev: u32,
    pub ino: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub file_size: u32,
    pub oid: Oid,
    pub path: String,
}

/// Resolves the canonical index file path (`.git/index` when `.git` exists, else `.bgit/index`).
pub(crate) fn index_path_for(repo_root: &Path) -> PathBuf {
    if let Ok(git_dir) = locate_git_dir(repo_root) {
        git_dir.join("index")
    } else {
        repo_root.join(".bgit/index")
    }
}

/// Loads the repository index into a path-sorted map (`BTreeMap<repo_rel_path, entry>`).
/// Returns an empty map if no index file exists yet (e.g. freshly initialized repo).
pub(crate) fn load_index<'a>(
    repo_root: &Path,
    alloc: Alloc<'a>,
) -> io::Result<BTreeMap<String, WritableIndexEntry>> {
    let idx_path = index_path_for(repo_root);
    if !idx_path.is_file() {
        return Ok(BTreeMap::new());
    }
    let raw = fs::read(&idx_path)?;
    if raw.is_empty() {
        return Ok(BTreeMap::new());
    }
    let parsed = parse_index(&raw, Limits::SHIPPED, alloc, NoPlant)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("index parse: {e}")))?;
    let mut map = BTreeMap::new();
    for entry in parsed.entries.iter() {
        let Some(path) = parsed.path_str(entry) else {
            continue;
        };
        map.insert(
            path.to_owned(),
            WritableIndexEntry {
                ctime_sec: entry.ctime_sec,
                ctime_nsec: entry.ctime_nsec,
                mtime_sec: entry.mtime_sec,
                mtime_nsec: entry.mtime_nsec,
                dev: entry.dev,
                ino: entry.ino,
                mode: entry.mode,
                uid: entry.uid,
                gid: entry.gid,
                file_size: entry.size,
                oid: entry.oid,
                path: path.to_owned(),
            },
        );
    }
    Ok(map)
}

/// Encodes `entries` into a valid, SHA-1-checksummed Git `DIRC` v2 index and writes it atomically.
pub(crate) fn write_index(
    repo_root: &Path,
    entries: &BTreeMap<String, WritableIndexEntry>,
) -> io::Result<()> {
    let mut sorted: Vec<&WritableIndexEntry> = entries.values().collect();
    sorted.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));

    let mut out = Vec::with_capacity(12 + sorted.len() * 96 + 20);
    out.extend_from_slice(b"DIRC");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(sorted.len() as u32).to_be_bytes());

    for e in sorted {
        let entry_start = out.len();
        out.extend_from_slice(&e.ctime_sec.to_be_bytes());
        out.extend_from_slice(&e.ctime_nsec.to_be_bytes());
        out.extend_from_slice(&e.mtime_sec.to_be_bytes());
        out.extend_from_slice(&e.mtime_nsec.to_be_bytes());
        out.extend_from_slice(&e.dev.to_be_bytes());
        out.extend_from_slice(&e.ino.to_be_bytes());
        out.extend_from_slice(&e.mode.to_be_bytes());
        out.extend_from_slice(&e.uid.to_be_bytes());
        out.extend_from_slice(&e.gid.to_be_bytes());
        out.extend_from_slice(&e.file_size.to_be_bytes());
        out.extend_from_slice(&e.oid.0);
        let name_bytes = e.path.as_bytes();
        let flags = (name_bytes.len().min(0x0fff)) as u16;
        out.extend_from_slice(&flags.to_be_bytes());
        out.extend_from_slice(name_bytes);
        out.push(0);
        // Git DIRC v2 pads each entry to a multiple of 8 bytes from `entry_start` (at least 1 NUL).
        while !(out.len() - entry_start).is_multiple_of(8) {
            out.push(0);
        }
    }

    let digest = sha1(&out);
    out.extend_from_slice(&digest);

    let idx_path = index_path_for(repo_root);
    write_file_atomic(&idx_path, &out)
}

/// Builds a `WritableIndexEntry` from on-disk `symlink_metadata` + blob `oid`.
pub(crate) fn stat_to_index_entry(
    repo_root: &Path,
    rel_path: &str,
    oid: Oid,
) -> io::Result<WritableIndexEntry> {
    let full = repo_root.join(rel_path);
    let meta = fs::symlink_metadata(&full)?;
    let mode = if meta.file_type().is_symlink() {
        0o120000
    } else if meta.permissions().mode() & 0o111 != 0 {
        0o100755
    } else {
        0o100644
    };
    Ok(WritableIndexEntry {
        ctime_sec: meta.ctime() as u32,
        ctime_nsec: meta.ctime_nsec() as u32,
        mtime_sec: meta.mtime() as u32,
        mtime_nsec: meta.mtime_nsec() as u32,
        dev: meta.dev() as u32,
        ino: meta.ino() as u32,
        mode,
        uid: meta.uid(),
        gid: meta.gid(),
        file_size: meta.len() as u32,
        oid,
        path: rel_path.to_owned(),
    })
}

/// Reads the blob bytes for a working-tree file or symlink (symlinks store their target path).
pub(crate) fn read_worktree_blob(path: &Path) -> io::Result<(u32, Vec<u8>)> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        let bytes = target.as_os_str().as_encoded_bytes().to_vec();
        Ok((0o120000, bytes))
    } else {
        let bytes = fs::read(path)?;
        let mode = if meta.permissions().mode() & 0o111 != 0 { 0o100755 } else { 0o100644 };
        Ok((mode, bytes))
    }
}

/// Reads a commit's root tree OID from `store`.
pub(crate) fn read_commit_tree<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    commit_oid: Oid,
    alloc: Alloc<'a>,
) -> io::Result<Oid> {
    let mut buf = OwnVec::new_in(alloc);
    let kind = store
        .read_object(&commit_oid, &mut buf)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    if kind != Some(ObjectKind::Commit) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("object {commit_oid} is not a commit"),
        ));
    }
    let commit = Commit::parse(&buf)
        .map_err(|(_, why)| io::Error::new(io::ErrorKind::InvalidData, format!("{why:?}")))?;
    Ok(commit.tree)
}

/// Recursively flattens a Git tree into `BTreeMap<repo_rel_path, (mode, blob_oid)>`.
pub(crate) fn flatten_tree<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    tree_oid: Oid,
    alloc: Alloc<'a>,
) -> io::Result<BTreeMap<String, (u32, Oid)>> {
    let mut out = BTreeMap::new();
    flatten_tree_rec(store, tree_oid, "", alloc, &mut out)?;
    Ok(out)
}

fn flatten_tree_rec<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    tree_oid: Oid,
    prefix: &str,
    alloc: Alloc<'a>,
    out: &mut BTreeMap<String, (u32, Oid)>,
) -> io::Result<()> {
    let mut buf = OwnVec::new_in(alloc);
    let kind = store
        .read_object(&tree_oid, &mut buf)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    if kind != Some(ObjectKind::Tree) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("object {tree_oid} is not a tree"),
        ));
    }
    let bytes = buf.as_slice().to_vec();
    let mut iter = TreeIter::new(tree_oid, &bytes);
    while let Some(entry) = iter
        .next_entry()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?
    {
        let Ok(name) = core::str::from_utf8(entry.name) else {
            continue;
        };
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode == 0o040000 {
            flatten_tree_rec(store, entry.oid, &path, alloc, out)?;
        } else {
            out.insert(path, (entry.mode, entry.oid));
        }
    }
    Ok(())
}

/// Builds a Git tree hierarchy in `store` from flat `(path -> (mode, oid))` entries.
pub(crate) fn build_tree_from_map<'a>(
    store: &mut BgitDiskStore<'a>,
    files: &BTreeMap<String, (u32, Oid)>,
    alloc: Alloc<'a>,
) -> io::Result<Oid> {
    let items: Vec<(&str, u32, Oid)> = files
        .iter()
        .map(|(p, &(mode, oid))| (p.as_str(), mode, oid))
        .collect();
    build_tree_rec(store, &items, alloc)
}

/// Builds a Git tree hierarchy in `store` directly from the current index.
pub(crate) fn build_tree_from_index<'a>(
    store: &mut BgitDiskStore<'a>,
    entries: &BTreeMap<String, WritableIndexEntry>,
    alloc: Alloc<'a>,
) -> io::Result<Oid> {
    let items: Vec<(&str, u32, Oid)> = entries
        .values()
        .map(|e| (e.path.as_str(), e.mode, e.oid))
        .collect();
    build_tree_rec(store, &items, alloc)
}

fn build_tree_rec<'a>(
    store: &mut BgitDiskStore<'a>,
    items: &[(&str, u32, Oid)],
    alloc: Alloc<'a>,
) -> io::Result<Oid> {
    let mut direct: Vec<(String, u32, Oid)> = Vec::new();
    let mut subdirs: BTreeMap<String, Vec<(&str, u32, Oid)>> = BTreeMap::new();

    for &(rel, mode, oid) in items {
        if let Some((dir, rest)) = rel.split_once('/') {
            subdirs.entry(dir.to_owned()).or_default().push((rest, mode, oid));
        } else {
            direct.push((rel.to_owned(), mode, oid));
        }
    }

    for (dir_name, children) in subdirs {
        let sub_oid = build_tree_rec(store, &children, alloc)?;
        direct.push((dir_name, 0o040000, sub_oid));
    }

    direct.sort_by(|a, b| {
        git_tree_name_cmp(a.0.as_bytes(), a.1 == 0o040000, b.0.as_bytes(), b.1 == 0o040000)
    });

    let build_entries: Vec<TreeBuildEntry<'_>> = direct
        .iter()
        .map(|(name, mode, oid)| TreeBuildEntry { mode: *mode, name: name.as_bytes(), oid: *oid })
        .collect();
    let mut encoded = OwnVec::new_in(alloc);
    encode_tree(&build_entries, alloc, &mut encoded)
        .map_err(|e| io::Error::other(format!("encode_tree: {e}")))?;
    store
        .write_object(ObjectKind::Tree, &encoded)
        .map_err(|e| io::Error::other(format!("write tree: {e}")))
}

/// Populates an index map from a flattened tree, attaching live stat metadata where files exist.
pub(crate) fn index_from_flat_tree(
    repo_root: &Path,
    flat: &BTreeMap<String, (u32, Oid)>,
) -> BTreeMap<String, WritableIndexEntry> {
    let mut map = BTreeMap::new();
    for (path, &(mode, oid)) in flat {
        let entry = stat_to_index_entry(repo_root, path, oid).unwrap_or(WritableIndexEntry {
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
            path: path.clone(),
        });
        map.insert(path.clone(), entry);
    }
    map
}

/// Safely transitions the working tree from `old_flat` to `new_flat`:
/// - Unless `force` is true, refuses if any file being modified/removed differs from `old_flat`
///   on disk or if an untracked file would be overwritten.
/// - Writes added/changed blobs with executable/symlink modes and deletes removed files + empty dirs.
pub(crate) fn checkout_tree_to_worktree<'a, S: ObjectStore<'a> + ?Sized>(
    repo_root: &Path,
    store: &S,
    old_flat: &BTreeMap<String, (u32, Oid)>,
    new_flat: &BTreeMap<String, (u32, Oid)>,
    force: bool,
    alloc: Alloc<'a>,
) -> io::Result<()> {
    if !force {
        let mut dirty = Vec::new();
        let mut untracked = Vec::new();
        for (path, &(old_mode, old_oid)) in old_flat {
            let new_entry = new_flat.get(path);
            if new_entry == Some(&(old_mode, old_oid)) {
                continue;
            }
            let full = repo_root.join(path);
            if full.exists() || full.is_symlink() {
                if let Ok((_, disk_bytes)) = read_worktree_blob(&full) {
                    let disk_oid = Oid::of(ObjectKind::Blob, &disk_bytes);
                    if disk_oid != old_oid {
                        dirty.push(path.clone());
                    }
                }
            }
        }
        for (path, &(_, new_oid)) in new_flat {
            if old_flat.contains_key(path) {
                continue;
            }
            let full = repo_root.join(path);
            if full.exists() || full.is_symlink() {
                let matches = read_worktree_blob(&full)
                    .map(|(_, b)| Oid::of(ObjectKind::Blob, &b) == new_oid)
                    .unwrap_or(false);
                if !matches {
                    untracked.push(path.clone());
                }
            }
        }
        if !dirty.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "local changes would be overwritten by checkout:\n  {}\n\
                     Commit or stash your changes (or pass -f/--force).",
                    dirty.join("\n  ")
                ),
            ));
        }
        if !untracked.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "untracked working tree files would be overwritten by checkout:\n  {}\n\
                     Move or remove them (or pass -f/--force).",
                    untracked.join("\n  ")
                ),
            ));
        }
    }

    // 1. Remove files present in `old_flat` that are absent in `new_flat`.
    for path in old_flat.keys() {
        if !new_flat.contains_key(path) {
            let full = repo_root.join(path);
            let _ = fs::remove_file(&full);
            prune_empty_parents(repo_root, full.parent());
        }
    }

    // 2. Write files added or changed in `new_flat` (or missing on disk when `force`).
    let mut buf = OwnVec::new_in(alloc);
    for (path, &(mode, oid)) in new_flat {
        let full = repo_root.join(path);
        let unchanged = !force && old_flat.get(path) == Some(&(mode, oid)) && full.exists();
        if unchanged {
            continue;
        }
        write_blob_to_worktree(store, &full, mode, oid, &mut buf)?;
    }

    Ok(())
}

/// Writes one blob (regular file, executable, or symlink) from `store` to `dst`.
pub(crate) fn write_blob_to_worktree<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    dst: &Path,
    mode: u32,
    oid: Oid,
    buf: &mut AVec<'a, u8>,
) -> io::Result<()> {
    let kind = store
        .read_object(&oid, buf)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    if kind != Some(ObjectKind::Blob) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("object {oid} is not a blob"),
        ));
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    if dst.is_symlink() || dst.is_file() {
        let _ = fs::remove_file(dst);
    }
    if mode == 0o120000 {
        let target = core::str::from_utf8(buf.as_slice())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 symlink"))?;
        symlink(target, dst)?;
    } else {
        fs::write(dst, buf.as_slice())?;
        if mode == 0o100755 {
            fs::set_permissions(dst, fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}

/// Removes empty directories walking upward from `dir` until `repo_root` (exclusive).
pub(crate) fn prune_empty_parents(repo_root: &Path, mut dir: Option<&Path>) {
    while let Some(cur) = dir {
        if cur == repo_root || !cur.starts_with(repo_root) {
            break;
        }
        if fs::remove_dir(cur).is_err() {
            break;
        }
        dir = cur.parent();
    }
}

/// Walks the working tree returning repo-relative paths of all non-ignored files (skipping `.git`/`.bgit`).
pub(crate) fn walk_working_tree<'a>(
    repo_root: &Path,
    include_ignored: bool,
    alloc: Alloc<'a>,
) -> io::Result<Vec<String>> {
    let mut stack = IgnoreStack::new(Limits::SHIPPED, alloc);
    if !include_ignored {
        if let Ok(git_dir) = locate_git_dir(repo_root) {
            let exclude = git_dir.join("info/exclude");
            if let Ok(bytes) = fs::read(&exclude) {
                stack.set_info_exclude(&bytes, NoPlant);
            }
        }
        let root_ignore = repo_root.join(".gitignore");
        if let Ok(bytes) = fs::read(&root_ignore) {
            stack.add_folder_gitignore("", &bytes, NoPlant);
        }
    }
    let mut files = Vec::new();
    walk_dir_rec(repo_root, "", include_ignored, &mut stack, &mut files)?;
    files.sort();
    Ok(files)
}

fn walk_dir_rec(
    repo_root: &Path,
    rel_dir: &str,
    include_ignored: bool,
    stack: &mut IgnoreStack<'_>,
    out: &mut Vec<String>,
) -> io::Result<()> {
    let abs_dir = if rel_dir.is_empty() {
        repo_root.to_path_buf()
    } else {
        repo_root.join(rel_dir)
    };
    if !include_ignored && !rel_dir.is_empty() {
        let gi = abs_dir.join(".gitignore");
        if let Ok(bytes) = fs::read(&gi) {
            stack.add_folder_gitignore(rel_dir, &bytes, NoPlant);
        }
    }

    let mut entries: Vec<(String, PathBuf, fs::FileType)> = Vec::new();
    for ent in fs::read_dir(&abs_dir)? {
        let ent = ent?;
        let name = ent.file_name().to_string_lossy().into_owned();
        if name == ".git" || name == ".bgit" {
            continue;
        }
        let ft = ent.file_type()?;
        entries.push((name, ent.path(), ft));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, _path, ft) in entries {
        let rel = if rel_dir.is_empty() {
            name
        } else {
            format!("{rel_dir}/{name}")
        };
        let is_dir = ft.is_dir();
        if !include_ignored && stack.is_ignored(&rel, is_dir) {
            continue;
        }
        if is_dir {
            walk_dir_rec(repo_root, &rel, include_ignored, stack, out)?;
        } else if ft.is_file() || ft.is_symlink() {
            out.push(rel);
        }
    }
    Ok(())
}

/// Converts a user-supplied path (relative to `cwd` or absolute) into a repo-relative POSIX path.
pub(crate) fn to_repo_relative(repo_root: &Path, cwd: &Path, raw: &str) -> io::Result<String> {
    let abs = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        cwd.join(raw)
    };
    let canon_root = fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let norm = normalize_lexical(&abs);
    let rel = norm
        .strip_prefix(&canon_root)
        .map(Path::to_path_buf)
        .or_else(|_| norm.strip_prefix(repo_root).map(Path::to_path_buf))
        .or_else(|_| {
            // Try canonicalizing the existing parent (e.g. `/tmp` -> `/private/tmp` on macOS).
            if let Some(parent) = norm.parent()
                && let Ok(cp) = fs::canonicalize(parent)
                && let Some(file) = norm.file_name()
            {
                let full = cp.join(file);
                if let Ok(r) = full.strip_prefix(&canon_root) {
                    return Ok(r.to_path_buf());
                }
            }
            if let Ok(c) = fs::canonicalize(&abs)
                && let Ok(r) = c.strip_prefix(&canon_root)
            {
                return Ok(r.to_path_buf());
            }
            Err(())
        })
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("path `{raw}` is outside repository `{}`", repo_root.display()),
            )
        })?;
    let s = rel.to_string_lossy().replace('\\', "/");
    Ok(s.trim_start_matches("./").trim_end_matches('/').to_owned())
}

fn normalize_lexical(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Minimal, lossless-preserving INI reader/writer for `.git/config`, `.bgit/config`, `~/.gitconfig`.
#[derive(Clone, Debug, Default)]
pub(crate) struct GitConfig {
    /// Ordered entries `(section, subsection, key, value)` with lowercase section & key.
    pub entries: Vec<(String, Option<String>, String, String)>,
}

impl GitConfig {
    pub(crate) fn parse(text: &str) -> Self {
        let mut cfg = GitConfig::default();
        let mut cur_sec = String::new();
        let mut cur_sub: Option<String> = None;
        for raw_line in text.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                let inner = line[1..line.len() - 1].trim();
                if let Some((sec, rest)) = inner.split_once(char::is_whitespace) {
                    cur_sec = sec.trim().to_ascii_lowercase();
                    let sub = rest.trim().trim_matches('"').to_owned();
                    cur_sub = if sub.is_empty() { None } else { Some(sub) };
                } else {
                    cur_sec = inner.to_ascii_lowercase();
                    cur_sub = None;
                }
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let key = k.trim().to_ascii_lowercase();
                let val = strip_inline_comment(v.trim());
                cfg.entries.push((cur_sec.clone(), cur_sub.clone(), key, val));
            }
        }
        cfg
    }

    pub(crate) fn load_file(path: &Path) -> Self {
        fs::read_to_string(path).map(|s| Self::parse(&s)).unwrap_or_default()
    }

    pub(crate) fn config_path_for(repo_root: &Path) -> PathBuf {
        if let Ok(git_dir) = locate_git_dir(repo_root) {
            git_dir.join("config")
        } else {
            repo_root.join(".bgit/config")
        }
    }

    pub(crate) fn global_config_path() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gitconfig"))
    }

    /// Loads merged config (global `~/.gitconfig` overridden by repo-local config).
    pub(crate) fn load_merged(repo_root: &Path) -> Self {
        let mut merged = Self::global_config_path()
            .map(|p| Self::load_file(&p))
            .unwrap_or_default();
        let local = Self::load_file(&Self::config_path_for(repo_root));
        merged.entries.extend(local.entries);
        merged
    }

    /// Looks up a dotted key like `user.name` or `remote.origin.url` (last match wins).
    pub(crate) fn get(&self, dotted: &str) -> Option<&str> {
        let (sec, sub, key) = split_config_key(dotted)?;
        self.entries
            .iter()
            .rev()
            .find(|(s, sb, k, _)| {
                s == &sec && sb.as_deref() == sub.as_deref() && k == &key
            })
            .map(|(_, _, _, v)| v.as_str())
    }

    /// Sets (or replaces) a dotted key and persists to `path`.
    pub(crate) fn set(&mut self, dotted: &str, value: &str) -> io::Result<()> {
        let (sec, sub, key) = split_config_key(dotted).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid config key `{dotted}` (expected section.key or section.sub.key)"),
            )
        })?;
        if let Some(entry) = self
            .entries
            .iter_mut()
            .rev()
            .find(|(s, sb, k, _)| s == &sec && sb.as_deref() == sub.as_deref() && k == &key)
        {
            entry.3 = value.to_owned();
        } else {
            self.entries.push((sec, sub, key, value.to_owned()));
        }
        Ok(())
    }

    /// Removes all entries matching a dotted key. Returns true if any entry was removed.
    pub(crate) fn unset(&mut self, dotted: &str) -> io::Result<bool> {
        let (sec, sub, key) = split_config_key(dotted).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("invalid config key `{dotted}`"))
        })?;
        let before = self.entries.len();
        self.entries
            .retain(|(s, sb, k, _)| !(s == &sec && sb.as_deref() == sub.as_deref() && k == &key));
        Ok(self.entries.len() != before)
    }

    /// Removes an entire `[section "subsection"]` block (e.g. `remote "origin"`).
    pub(crate) fn remove_section(&mut self, sec: &str, sub: Option<&str>) -> bool {
        let sec_l = sec.to_ascii_lowercase();
        let before = self.entries.len();
        self.entries.retain(|(s, sb, _, _)| !(s == &sec_l && sb.as_deref() == sub));
        self.entries.len() != before
    }

    /// Renames a subsection (e.g. `remote "old"` -> `remote "new"`).
    pub(crate) fn rename_subsection(&mut self, sec: &str, old: &str, new: &str) -> bool {
        let sec_l = sec.to_ascii_lowercase();
        let mut changed = false;
        for (s, sb, _, _) in &mut self.entries {
            if s == &sec_l && sb.as_deref() == Some(old) {
                *sb = Some(new.to_owned());
                changed = true;
            }
        }
        changed
    }

    /// Lists all named remotes `(name, url)`.
    pub(crate) fn remotes(&self) -> Vec<(String, String)> {
        let mut by_name: BTreeMap<String, String> = BTreeMap::new();
        for (s, sb, k, v) in &self.entries {
            if s == "remote"
                && k == "url"
                && let Some(name) = sb
            {
                by_name.insert(name.clone(), v.clone());
            }
        }
        by_name.into_iter().collect()
    }

    /// Serializes cleanly back to Git INI format grouped by section header.
    pub(crate) fn save_file(&self, path: &Path) -> io::Result<()> {
        let mut out = String::new();
        let mut grouped: Vec<((String, Option<String>), Vec<(&str, &str)>)> = Vec::new();
        for (s, sb, k, v) in &self.entries {
            let header = (s.clone(), sb.clone());
            if let Some((_, kvs)) = grouped.iter_mut().find(|(h, _)| *h == header) {
                kvs.push((k.as_str(), v.as_str()));
            } else {
                grouped.push((header, vec![(k.as_str(), v.as_str())]));
            }
        }
        for ((s, sb), kvs) in grouped {
            if let Some(sub) = sb {
                out.push_str(&format!("[{s} \"{sub}\"]\n"));
            } else {
                out.push_str(&format!("[{s}]\n"));
            }
            for (k, v) in kvs {
                out.push_str(&format!("\t{k} = {v}\n"));
            }
        }
        write_file_atomic(path, out.as_bytes())
    }

    /// Formats all key=value pairs in `git config --list` format.
    pub(crate) fn format_list(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .map(|(s, sb, k, v)| {
                let full = match sb {
                    Some(sub) => format!("{s}.{sub}.{k}"),
                    None => format!("{s}.{k}"),
                };
                (full, v.clone())
            })
            .collect()
    }
}

fn split_config_key(dotted: &str) -> Option<(String, Option<String>, String)> {
    let first = dotted.find('.')?;
    let last = dotted.rfind('.')?;
    let sec = dotted[..first].to_ascii_lowercase();
    let key = dotted[last + 1..].to_ascii_lowercase();
    if sec.is_empty() || key.is_empty() {
        return None;
    }
    let sub = if first == last {
        None
    } else {
        Some(dotted[first + 1..last].to_owned())
    };
    Some((sec, sub, key))
}

fn strip_inline_comment(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut in_quotes = false;
    let mut chars = v.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            in_quotes = !in_quotes;
            continue;
        }
        if !in_quotes && (c == '#' || c == ';') {
            break;
        }
        out.push(c);
    }
    out.trim_end().to_owned()
}

/// Resolves author/committer name, email, and current Unix timestamp from env / config.
pub(crate) fn resolve_identity(repo_root: &Path) -> (String, String, i64) {
    let cfg = GitConfig::load_merged(repo_root);
    let name = std::env::var("GIT_AUTHOR_NAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.get("user.name").map(str::to_owned))
        .or_else(|| std::env::var("USER").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "bgit".to_owned());
    let email = std::env::var("GIT_AUTHOR_EMAIL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.get("user.email").map(str::to_owned))
        .unwrap_or_else(|| format!("{name}@localhost"));
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (name, email, ts)
}

/// Helper building a borrowed `Signature` from resolved identity fields.
pub(crate) fn make_signature<'b>(name: &'b str, email: &'b str, timestamp: i64) -> Signature<'b> {
    Signature { name, email, timestamp, tz: "+0000" }
}

/// Computes `(insertions, deletions)` between two byte slices using line-level LCS diff.
pub(crate) fn diff_line_stats(old: &[u8], new: &[u8]) -> (usize, usize) {
    if is_binary(old) || is_binary(new) {
        return (0, 0);
    }
    let a = split_lines(old);
    let b = split_lines(new);
    let ops = compute_line_diff(&a, &b);
    let mut ins = 0usize;
    let mut del = 0usize;
    for op in ops {
        match op {
            DiffOp::Insert(_) => ins += 1,
            DiffOp::Delete(_) => del += 1,
            DiffOp::Equal(_) => {}
        }
    }
    (ins, del)
}

/// Formats a Git-compatible unified diff for one file pair (with optional ANSI color).
pub(crate) fn format_unified_diff(
    path: &str,
    old: Option<&[u8]>,
    new: Option<&[u8]>,
    use_color: bool,
) -> String {
    let old_b = old.unwrap_or(b"");
    let new_b = new.unwrap_or(b"");
    if old == new {
        return String::new();
    }

    let (bold, red, green, cyan, reset) = if use_color {
        ("\x1b[1m", "\x1b[31m", "\x1b[32m", "\x1b[36m", "\x1b[0m")
    } else {
        ("", "", "", "", "")
    };

    let mut out = String::new();
    out.push_str(&format!("{bold}diff --git a/{path} b/{path}{reset}\n"));
    if old.is_none() {
        out.push_str(&format!("{bold}new file mode 100644{reset}\n"));
    } else if new.is_none() {
        out.push_str(&format!("{bold}deleted file mode 100644{reset}\n"));
    }

    if is_binary(old_b) || is_binary(new_b) {
        out.push_str(&format!("Binary files a/{path} and b/{path} differ\n"));
        return out;
    }

    let old_hdr = if old.is_some() { format!("a/{path}") } else { "/dev/null".to_owned() };
    let new_hdr = if new.is_some() { format!("b/{path}") } else { "/dev/null".to_owned() };
    out.push_str(&format!("{bold}--- {old_hdr}{reset}\n"));
    out.push_str(&format!("{bold}+++ {new_hdr}{reset}\n"));

    let a = split_lines(old_b);
    let b = split_lines(new_b);
    let ops = compute_line_diff(&a, &b);

    // Group changes into unified hunks with 3 lines of context.
    const CONTEXT: usize = 3;
    let mut change_indices = Vec::new();
    for (idx, op) in ops.iter().enumerate() {
        if !matches!(op, DiffOp::Equal(_)) {
            change_indices.push(idx);
        }
    }
    if change_indices.is_empty() {
        return String::new();
    }

    let mut hunks: Vec<(usize, usize)> = Vec::new();
    for idx in change_indices {
        let start = idx.saturating_sub(CONTEXT);
        let end = (idx + CONTEXT + 1).min(ops.len());
        if let Some(last) = hunks.last_mut()
            && start <= last.1
        {
            last.1 = end;
            continue;
        }
        hunks.push((start, end));
    }

    // Track 1-based line numbers in `a` and `b` at each op index.
    let mut a_pos = Vec::with_capacity(ops.len() + 1);
    let mut b_pos = Vec::with_capacity(ops.len() + 1);
    let (mut la, mut lb) = (1usize, 1usize);
    for op in &ops {
        a_pos.push(la);
        b_pos.push(lb);
        match op {
            DiffOp::Equal(_) => {
                la += 1;
                lb += 1;
            }
            DiffOp::Delete(_) => la += 1,
            DiffOp::Insert(_) => lb += 1,
        }
    }
    a_pos.push(la);
    b_pos.push(lb);

    for (h_start, h_end) in hunks {
        let old_count = a_pos[h_end] - a_pos[h_start];
        let new_count = b_pos[h_end] - b_pos[h_start];
        let old_start = if old_count == 0 { a_pos[h_start].saturating_sub(1) } else { a_pos[h_start] };
        let new_start = if new_count == 0 { b_pos[h_start].saturating_sub(1) } else { b_pos[h_start] };
        out.push_str(&format!(
            "{cyan}@@ -{old_start},{old_count} +{new_start},{new_count} @@{reset}\n"
        ));
        for op in &ops[h_start..h_end] {
            match *op {
                DiffOp::Equal(line) => {
                    out.push(' ');
                    out.push_str(&String::from_utf8_lossy(line));
                    out.push('\n');
                }
                DiffOp::Delete(line) => {
                    out.push_str(red);
                    out.push('-');
                    out.push_str(&String::from_utf8_lossy(line));
                    out.push_str(reset);
                    out.push('\n');
                }
                DiffOp::Insert(line) => {
                    out.push_str(green);
                    out.push('+');
                    out.push_str(&String::from_utf8_lossy(line));
                    out.push_str(reset);
                    out.push('\n');
                }
            }
        }
    }

    out
}

#[derive(Clone, Copy)]
enum DiffOp<'a> {
    Equal(&'a [u8]),
    Delete(&'a [u8]),
    Insert(&'a [u8]),
}

fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    if lines.last() == Some(&b"".as_slice()) {
        lines.pop();
    }
    lines
}

/// Myers O((N+M)D) line diff (with common prefix/suffix trimming and bounded fallback for huge files).
fn compute_line_diff<'a>(a: &[&'a [u8]], b: &[&'a [u8]]) -> Vec<DiffOp<'a>> {
    let mut prefix = 0usize;
    while prefix < a.len() && prefix < b.len() && a[prefix] == b[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix + prefix < a.len()
        && suffix + prefix < b.len()
        && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let mut ops = Vec::with_capacity(a.len() + b.len());
    for &line in &a[..prefix] {
        ops.push(DiffOp::Equal(line));
    }

    let a_mid = &a[prefix..a.len() - suffix];
    let b_mid = &b[prefix..b.len() - suffix];
    let n = a_mid.len();
    let m = b_mid.len();

    if n == 0 {
        for &line in b_mid {
            ops.push(DiffOp::Insert(line));
        }
    } else if m == 0 {
        for &line in a_mid {
            ops.push(DiffOp::Delete(line));
        }
    } else {
        let max_d = (n + m).min(4096);
        let offset = max_d as isize;
        let mut v = vec![0isize; 2 * max_d + 1];
        let mut trace: Vec<Vec<isize>> = Vec::new();
        let mut solved_d: Option<usize> = None;

        'outer: for d in 0..=max_d {
            trace.push(v.clone());
            let d_i = d as isize;
            let mut k = -d_i;
            while k <= d_i {
                let idx = (k + offset) as usize;
                let mut x = if k == -d_i
                    || (k != d_i && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize])
                {
                    v[(k + 1 + offset) as usize]
                } else {
                    v[(k - 1 + offset) as usize] + 1
                };
                let mut y = x - k;
                while (x as usize) < n
                    && (y as usize) < m
                    && a_mid[x as usize] == b_mid[y as usize]
                {
                    x += 1;
                    y += 1;
                }
                v[idx] = x;
                if (x as usize) >= n && (y as usize) >= m {
                    solved_d = Some(d);
                    break 'outer;
                }
                k += 2;
            }
        }

        if let Some(d_end) = solved_d {
            let mut mid_ops = Vec::new();
            let mut x = n as isize;
            let mut y = m as isize;
            for d in (1..=d_end).rev() {
                let v_prev = &trace[d];
                let d_i = d as isize;
                let k = x - y;
                let prev_k = if k == -d_i
                    || (k != d_i
                        && v_prev[(k - 1 + offset) as usize] < v_prev[(k + 1 + offset) as usize])
                {
                    k + 1
                } else {
                    k - 1
                };
                let prev_x = v_prev[(prev_k + offset) as usize];
                let prev_y = prev_x - prev_k;
                while x > prev_x && y > prev_y {
                    x -= 1;
                    y -= 1;
                    mid_ops.push(DiffOp::Equal(a_mid[x as usize]));
                }
                if x == prev_x {
                    y -= 1;
                    mid_ops.push(DiffOp::Insert(b_mid[y as usize]));
                } else {
                    x -= 1;
                    mid_ops.push(DiffOp::Delete(a_mid[x as usize]));
                }
            }
            while x > 0 && y > 0 {
                x -= 1;
                y -= 1;
                mid_ops.push(DiffOp::Equal(a_mid[x as usize]));
            }
            mid_ops.reverse();
            ops.extend(mid_ops);
        } else {
            // Bounded hunk replacement when edit distance exceeds 4096 lines.
            for &line in a_mid {
                ops.push(DiffOp::Delete(line));
            }
            for &line in b_mid {
                ops.push(DiffOp::Insert(line));
            }
        }
    }

    for &line in &a[a.len() - suffix..] {
        ops.push(DiffOp::Equal(line));
    }
    ops
}

/// Checks whether a candidate repo-relative path matches any user pathspec filter.
pub(crate) fn path_matches_filters(rel: &str, filters: &[String]) -> bool {
    if filters.is_empty() {
        return true;
    }
    filters.iter().any(|f| {
        f.is_empty()
            || f == "."
            || rel == f
            || rel.starts_with(&format!("{f}/"))
    })
}

/// Collects all unique paths across two sorted path maps.
pub(crate) fn union_paths<'a>(
    a: &'a BTreeMap<String, (u32, Oid)>,
    b: &'a BTreeMap<String, (u32, Oid)>,
) -> BTreeSet<&'a str> {
    let mut set = BTreeSet::new();
    for k in a.keys() {
        set.insert(k.as_str());
    }
    for k in b.keys() {
        set.insert(k.as_str());
    }
    set
}

/// Checks whether a working-tree file matches its Git index stat-cache entry without reading disk bytes.
pub(crate) fn index_entry_stat_clean(repo_root: &Path, e: &WritableIndexEntry) -> bool {
    let Ok(meta) = fs::symlink_metadata(repo_root.join(&e.path)) else {
        return false;
    };
    let is_link = meta.file_type().is_symlink();
    let mode_ok = if is_link {
        e.mode == 0o120000
    } else if meta.is_file() {
        let exec = meta.permissions().mode() & 0o111 != 0;
        (exec && e.mode == 0o100755) || (!exec && e.mode == 0o100644)
    } else {
        false
    };
    if !mode_ok {
        return false;
    }
    (meta.mtime() as u32) == e.mtime_sec
        && (meta.mtime_nsec() as u32) == e.mtime_nsec
        && (meta.len() as u32) == e.file_size
        && (e.ino == 0 || (meta.ino() as u32) == e.ino)
}

/// O(changed) recursive tree diff: skips any subtree whose OID is identical on both sides!
/// Returns sorted `(repo_rel_path, old_entry, new_entry)` only for files that actually changed.
pub(crate) fn diff_trees_changed<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    old_tree: Option<Oid>,
    new_tree: Option<Oid>,
    alloc: Alloc<'a>,
) -> io::Result<Vec<(String, Option<(u32, Oid)>, Option<(u32, Oid)>)>> {
    let mut out = Vec::new();
    diff_trees_rec(store, old_tree, new_tree, "", alloc, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn read_tree_children<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    tree_oid: Option<Oid>,
    alloc: Alloc<'a>,
) -> io::Result<BTreeMap<String, (u32, Oid)>> {
    let Some(oid) = tree_oid else {
        return Ok(BTreeMap::new());
    };
    let mut buf = OwnVec::new_in(alloc);
    let kind = store
        .read_object(&oid, &mut buf)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    if kind != Some(ObjectKind::Tree) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("object {oid} is not a tree"),
        ));
    }
    let bytes = buf.as_slice().to_vec();
    let mut iter = TreeIter::new(oid, &bytes);
    let mut map = BTreeMap::new();
    while let Some(entry) = iter
        .next_entry()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?
    {
        if let Ok(name) = core::str::from_utf8(entry.name) {
            map.insert(name.to_owned(), (entry.mode, entry.oid));
        }
    }
    Ok(map)
}

fn diff_trees_rec<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    old_tree: Option<Oid>,
    new_tree: Option<Oid>,
    prefix: &str,
    alloc: Alloc<'a>,
    out: &mut Vec<(String, Option<(u32, Oid)>, Option<(u32, Oid)>)>,
) -> io::Result<()> {
    if old_tree == new_tree {
        return Ok(());
    }
    let old_map = read_tree_children(store, old_tree, alloc)?;
    let new_map = read_tree_children(store, new_tree, alloc)?;

    for name in union_paths(&old_map, &new_map) {
        let a = old_map.get(name).copied();
        let b = new_map.get(name).copied();
        if a == b {
            continue;
        }
        let path = if prefix.is_empty() { name.to_owned() } else { format!("{prefix}/{name}") };
        let a_dir = a.is_some_and(|(m, _)| m == 0o040000);
        let b_dir = b.is_some_and(|(m, _)| m == 0o040000);
        match (a_dir, b_dir) {
            (true, true) => {
                diff_trees_rec(store, a.map(|(_, o)| o), b.map(|(_, o)| o), &path, alloc, out)?;
            }
            (true, false) => {
                diff_trees_rec(store, a.map(|(_, o)| o), None, &path, alloc, out)?;
                if b.is_some() {
                    out.push((path, None, b));
                }
            }
            (false, true) => {
                if a.is_some() {
                    out.push((path.clone(), a, None));
                }
                diff_trees_rec(store, None, b.map(|(_, o)| o), &path, alloc, out)?;
            }
            (false, false) => {
                out.push((path, a, b));
            }
        }
    }
    Ok(())
}
