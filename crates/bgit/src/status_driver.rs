//! Hardware-floor native status driver for `bgit status`.
//!
//! - For standard `.git` repos (normal, linked worktree, bare, and `.bgit`-infected `.git` repos):
//!   drives `blaze_git::StatusMachine` directly over `blaze_core::Native` (`kqueue` + multi-core
//!   `getattrlistbulk` on macOS; `io_uring` + `statx` on Linux) with read-only `Mapping::map_file`
//!   for `.git/index` and `commit-graph`, transparently using `.bgit/graph.bggp` when `.git` has
//!   no commit-graph yet.
//! - For pure `.bgit`-only checkouts (created by `bgit clone` or `bgit init` without `.git/`):
//!   walks the working tree via multi-core `blaze_core::Native` (`Effect::Entries`) and diffs
//!   against `HEAD`'s tree from `BgitDiskStore` (mmap slabs) with full `.gitignore` support.

use std::{
    collections::HashMap,
    fs::{self, File},
    io,
    os::unix::fs::{FileExt, MetadataExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use blaze_core::{
    DirName, Done, Driver as _, Effect, FileName, Kind as CoreKind, Listing, Mapping, Names,
    STAMP_MOST, Stamp, Tag,
};
use blaze_git::{
    Detail, FileKind, FileStat, FileStatus, Head, Limits, PromptStatus, RepoState, ShortOid,
    StatusCounts, StatusEntry, StatusMachine, StatusOpts, Step, Truncated, UntrackedMode,
    WorktreeKind,
    ignore::IgnoreStack,
    machine::{DirListing, FileData, Got, PathScope, RangeData, ScopedPath, StatEntry, Want},
    objects::TreeIter,
    storage::{ObjectStore, RefStore, RefTarget},
};
use blaze_git_fetch::{Kind as ObjectKind, Oid, store::Commit};
use blaze_own::{AVec, Alloc, String as OwnString, Vec as OwnVec, planted::NoPlant};

use crate::disk::{BgitDiskStore, write_file_atomic};

/// Stateless sparse-directory entry-name cache (`.bgit/dircache.bgdc`).
///
/// Caches ONLY `(dir_path -> (mtime_ns, ctime_ns, ino, sorted_child_names))` for sparse
/// directories (`<= MAX_SPARSE_CHILDREN` entries). On every `bgit status`, each cached directory
/// and its children are stamped in ONE batched `Effect::Stamp` (`fstatat` across all cores, zero
/// `open()`/`getattrlistbulk()`/`close()`). Any directory whose `(mtime_ns, ctime_ns, ino)`
/// changed falls through immediately to `Effect::Entries` (`getattrlistbulk`) and refreshes the
/// record atomically. Completely independent of `.git/index`, `HEAD`, and `.gitignore`.
const DIRCACHE_MAGIC: &[u8; 4] = b"BGDC";
const DIRCACHE_VERSION: u32 = 1;
const MAX_SPARSE_CHILDREN: usize = 4;
const RACY_DIR_WINDOW_NS: u64 = 50_000_000;

struct SparseDirEntry {
    mtime_ns: u64,
    ctime_ns: u64,
    ino: u64,
    children: Vec<String>,
}

struct DirCache {
    path: PathBuf,
    entries: HashMap<String, SparseDirEntry>,
    dirty: bool,
}

fn dircache_crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

impl DirCache {
    fn load(bgit_dir: &Path) -> Self {
        let path = bgit_dir.join("dircache.bgdc");
        let mut entries = HashMap::new();
        if let Ok(raw) = fs::read(&path)
            && raw.len() >= 16
            && &raw[0..4] == DIRCACHE_MAGIC
            && u32::from_le_bytes(raw[4..8].try_into().expect("4b")) == DIRCACHE_VERSION
        {
            let body_end = raw.len() - 4;
            let want_crc = u32::from_le_bytes(raw[body_end..].try_into().expect("4b"));
            if dircache_crc32(&raw[..body_end]) == want_crc {
                let count = u32::from_le_bytes(raw[8..12].try_into().expect("4b")) as usize;
                entries.reserve(count);
                let mut pos = 12usize;
                let mut ok = true;
                for _ in 0..count {
                    if pos + 2 > body_end {
                        ok = false;
                        break;
                    }
                    let dlen = u16::from_le_bytes([raw[pos], raw[pos + 1]]) as usize;
                    pos += 2;
                    if pos + dlen + 25 > body_end {
                        ok = false;
                        break;
                    }
                    let Ok(dir) = core::str::from_utf8(&raw[pos..pos + dlen]) else {
                        ok = false;
                        break;
                    };
                    pos += dlen;
                    let mtime_ns = u64::from_le_bytes(raw[pos..pos + 8].try_into().expect("8b"));
                    let ctime_ns =
                        u64::from_le_bytes(raw[pos + 8..pos + 16].try_into().expect("8b"));
                    let ino = u64::from_le_bytes(raw[pos + 16..pos + 24].try_into().expect("8b"));
                    let n_children = raw[pos + 24] as usize;
                    pos += 25;
                    if n_children > MAX_SPARSE_CHILDREN {
                        ok = false;
                        break;
                    }
                    let mut children = Vec::with_capacity(n_children);
                    for _ in 0..n_children {
                        if pos + 2 > body_end {
                            ok = false;
                            break;
                        }
                        let clen = u16::from_le_bytes([raw[pos], raw[pos + 1]]) as usize;
                        pos += 2;
                        if pos + clen > body_end {
                            ok = false;
                            break;
                        }
                        let Ok(cname) = core::str::from_utf8(&raw[pos..pos + clen]) else {
                            ok = false;
                            break;
                        };
                        pos += clen;
                        children.push(cname.to_owned());
                    }
                    if !ok {
                        break;
                    }
                    entries.insert(
                        dir.to_owned(),
                        SparseDirEntry { mtime_ns, ctime_ns, ino, children },
                    );
                }
                if !ok {
                    entries.clear();
                }
            }
        }
        Self { path, entries, dirty: false }
    }

    fn flush_if_dirty(&mut self) {
        if !self.dirty {
            return;
        }
        let mut out = Vec::with_capacity(16 + self.entries.len() * 64);
        out.extend_from_slice(DIRCACHE_MAGIC);
        out.extend_from_slice(&DIRCACHE_VERSION.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        let mut written = 0u32;
        for (dir, e) in &self.entries {
            let Ok(dlen) = u16::try_from(dir.len()) else {
                continue;
            };
            if e.children.len() > MAX_SPARSE_CHILDREN {
                continue;
            }
            if e.children.iter().any(|c| u16::try_from(c.len()).is_err()) {
                continue;
            }
            out.extend_from_slice(&dlen.to_le_bytes());
            out.extend_from_slice(dir.as_bytes());
            out.extend_from_slice(&e.mtime_ns.to_le_bytes());
            out.extend_from_slice(&e.ctime_ns.to_le_bytes());
            out.extend_from_slice(&e.ino.to_le_bytes());
            out.push(e.children.len() as u8);
            for c in &e.children {
                out.extend_from_slice(&(c.len() as u16).to_le_bytes());
                out.extend_from_slice(c.as_bytes());
            }
            written += 1;
        }
        out[8..12].copy_from_slice(&written.to_le_bytes());
        let crc = dircache_crc32(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        let _ = write_file_atomic(&self.path, &out);
        self.dirty = false;
    }
}

/// Timing breakdown printed when `bgit status --timing` is requested.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct StatusTiming {
    pub total_us: u64,
    pub machine_us: u64,
    pub read_files_us: u64,
    pub read_files_count: u32,
    pub read_with_stat_us: u64,
    pub list_dirs_us: u64,
    pub list_dirs_count: u32,
    pub stat_paths_us: u64,
    pub stat_paths_count: u32,
    pub read_links_us: u64,
    pub read_ranges_us: u64,
    pub rounds: u32,
}

enum DiscoveredRepo {
    Git {
        worktree: Option<PathBuf>,
        git_dir: PathBuf,
        common_dir: PathBuf,
        bgit_dir: Option<PathBuf>,
        kind: WorktreeKind,
    },
    BgitOnly {
        worktree: PathBuf,
    },
}

/// Walk upward from `start` to discover the enclosing `.git`, linked worktree, bare repo, or `.bgit`.
fn discover_repo(start: &Path) -> io::Result<DiscoveredRepo> {
    let mut cur = fs::canonicalize(start)?;
    loop {
        let dot_git = cur.join(".git");
        if let Ok(meta) = fs::symlink_metadata(&dot_git) {
            let bgit_dir = {
                let b = cur.join(".bgit");
                b.join("refs.bgref").is_file().then_some(b)
            };
            if meta.is_dir() {
                return Ok(DiscoveredRepo::Git {
                    worktree: Some(cur),
                    git_dir: dot_git.clone(),
                    common_dir: dot_git,
                    bgit_dir,
                    kind: WorktreeKind::Normal,
                });
            }
            if meta.is_file() {
                let raw = fs::read(&dot_git)?;
                let target = blaze_git::refs::parse_gitdir_file(&raw).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("bad .git file: {e}"))
                })?;
                let gd = {
                    let p = Path::new(target);
                    if p.is_absolute() { p.to_path_buf() } else { cur.join(p) }
                };
                let common = {
                    let commondir_file = gd.join("commondir");
                    if commondir_file.is_file() {
                        let c = fs::read_to_string(&commondir_file)?;
                        let c = c.trim();
                        let p = Path::new(c);
                        if p.is_absolute() { p.to_path_buf() } else { gd.join(p) }
                    } else {
                        gd.clone()
                    }
                };
                return Ok(DiscoveredRepo::Git {
                    worktree: Some(cur),
                    git_dir: gd,
                    common_dir: common,
                    bgit_dir,
                    kind: WorktreeKind::LinkedWorktree,
                });
            }
        }

        // Pure `.bgit` repository (created by `bgit init` or `bgit clone`).
        if cur.join(".bgit").join("refs.bgref").is_file() {
            return Ok(DiscoveredRepo::BgitOnly { worktree: cur });
        }

        // Bare git repository (`HEAD` + `objects/`).
        if cur.join("HEAD").is_file() && cur.join("objects").is_dir() {
            let bgit_dir = {
                let b = cur.join(".bgit");
                b.join("refs.bgref").is_file().then_some(b)
            };
            return Ok(DiscoveredRepo::Git {
                worktree: None,
                git_dir: cur.clone(),
                common_dir: cur,
                bgit_dir,
                kind: WorktreeKind::Bare,
            });
        }

        if !cur.pop() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("not a git or bgit repository: {}", start.display()),
            ));
        }
    }
}

/// Compute repository status at `start` (walking upward to find the repo root).
pub(crate) fn run_status<'a>(
    start: &Path,
    opts: StatusOpts,
    alloc: Alloc<'a>,
) -> io::Result<(PromptStatus<'a>, StatusTiming)> {
    let t_total = Instant::now();
    // O(dirty) fast-path for active lazy VFS mounts (< 1 ms, 0 directory walks!).
    if let Some(vfs_status) = crate::vfs_driver::try_vfs_mount_status(start, opts, alloc)? {
        let mut timing = StatusTiming::default();
        timing.total_us = t_total.elapsed().as_micros() as u64;
        return Ok((vfs_status, timing));
    }
    let repo = discover_repo(start)?;
    let (status, mut timing) = match repo {
        DiscoveredRepo::Git { worktree, git_dir, common_dir, bgit_dir, kind } => {
            run_git_status(worktree, git_dir, common_dir, bgit_dir, kind, opts, alloc)?
        }
        DiscoveredRepo::BgitOnly { worktree } => {
            run_bgit_only_status(&worktree, opts, alloc)?
        }
    };
    timing.total_us = t_total.elapsed().as_micros() as u64;
    Ok((status, timing))
}

fn load_global_exclude() -> Option<Vec<u8>> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        let p = PathBuf::from(xdg).join("git/ignore");
        if let Ok(bytes) = fs::read(p) {
            return Some(bytes);
        }
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        let p = PathBuf::from(home).join(".config/git/ignore");
        if let Ok(bytes) = fs::read(p) {
            return Some(bytes);
        }
    }
    None
}

fn default_io_threads() -> usize {
    std::thread::available_parallelism().map_or(8, |n| n.get().clamp(4, 16))
}

fn stamp_to_file_stat(stamp: Stamp, kind: CoreKind) -> FileStat {
    let fk = match kind {
        CoreKind::File => FileKind::File,
        CoreKind::Dir => FileKind::Dir,
        CoreKind::Link => FileKind::Symlink,
        CoreKind::Gone => FileKind::Gone,
        CoreKind::Other | CoreKind::Unknown => FileKind::Other,
    };
    FileStat::from_ns(fk, stamp.size, stamp.mtime_ns, stamp.ctime_ns, stamp.ino, stamp.dev, stamp.mode)
}

fn meta_to_file_stat(meta: &fs::Metadata) -> FileStat {
    let ft = meta.file_type();
    let kind = if ft.is_file() {
        FileKind::File
    } else if ft.is_dir() {
        FileKind::Dir
    } else if ft.is_symlink() {
        FileKind::Symlink
    } else {
        FileKind::Other
    };
    FileStat {
        kind,
        size: meta.size(),
        mtime_sec: meta.mtime(),
        mtime_nsec: meta.mtime_nsec() as u32,
        ctime_sec: meta.ctime(),
        ctime_nsec: meta.ctime_nsec() as u32,
        ino: meta.ino(),
        dev: meta.dev(),
        mode: meta.mode(),
    }
}

struct NativeGitDriver {
    worktree: Option<PathBuf>,
    worktree_io: Option<blaze_core::Native>,
    git_dir: PathBuf,
    common_dir: PathBuf,
    bgit_dir: Option<PathBuf>,
    hide_root_bgit: bool,
    dircache: Option<DirCache>,
}

impl NativeGitDriver {
    fn resolve_path(&self, scope: PathScope, rel: &str) -> Option<PathBuf> {
        let base = match scope {
            PathScope::Worktree => self.worktree.as_ref()?,
            PathScope::GitDir => &self.git_dir,
            PathScope::CommonDir => &self.common_dir,
        };
        if rel.is_empty() { Some(base.clone()) } else { Some(base.join(rel)) }
    }

    fn exec_read_files<'a>(
        &mut self,
        paths: &[ScopedPath<'a>],
        alloc: Alloc<'a>,
    ) -> io::Result<Got<'a>> {
        let mut out: AVec<'a, Option<AVec<'a, u8>>> = OwnVec::with_capacity_in(paths.len(), alloc);
        for _ in 0..paths.len() {
            out.push(None);
        }

        // Batch Worktree reads through `blaze_core::Native` (parallel across cores).
        if let Some(drv) = &mut self.worktree_io {
            let mut effects = Vec::new();
            for (i, sp) in paths.iter().enumerate() {
                if sp.scope == PathScope::Worktree
                    && let Some(fname) = FileName::new(sp.path.as_str())
                {
                    effects.push(Effect::ReadAll { file: fname, tag: Tag(i as u64) });
                }
            }
            if !effects.is_empty() {
                blaze_core::run_until_idle(drv, &mut effects, |comp, _| {
                    let slot = comp.tag.0 as usize;
                    if let Done::Read(bytes) = comp.done {
                        let mut v = OwnVec::with_capacity_in(bytes.len(), alloc);
                        v.extend_from_slice(&bytes);
                        out[slot] = Some(v);
                    }
                })
                .map_err(io::Error::from)?;
            }
        }

        // GitDir / CommonDir reads (~15 small metadata files or loose objects).
        for (i, sp) in paths.iter().enumerate() {
            if sp.scope == PathScope::Worktree {
                continue;
            }
            let Some(full) = self.resolve_path(sp.scope, sp.path.as_str()) else {
                continue;
            };
            if let Ok(bytes) = fs::read(&full) {
                let mut v = OwnVec::with_capacity_in(bytes.len(), alloc);
                v.extend_from_slice(&bytes);
                out[i] = Some(v);
            }
        }

        Ok(Got::Files(out))
    }

    fn exec_read_with_stat<'a>(
        &mut self,
        paths: &[ScopedPath<'a>],
        alloc: Alloc<'a>,
    ) -> io::Result<Got<'a>> {
        let mut out: AVec<'a, FileData<'a>> = OwnVec::with_capacity_in(paths.len(), alloc);
        for sp in paths.iter() {
            let mut full = self.resolve_path(sp.scope, sp.path.as_str());
            // Transparently serve `.bgit/graph.bggp` when `.git/objects/info/commit-graph` does not exist!
            if sp.scope == PathScope::CommonDir
                && sp.path.as_str() == "objects/info/commit-graph"
                && !full.as_ref().is_some_and(|p| p.is_file())
                && let Some(bgit) = &self.bgit_dir
            {
                let bg = bgit.join("graph.bggp");
                if bg.is_file() {
                    full = Some(bg);
                }
            }
            let Some(path) = full else {
                out.push(FileData { bytes: None, stat: None });
                continue;
            };
            let Ok(meta) = fs::symlink_metadata(&path) else {
                out.push(FileData { bytes: None, stat: None });
                continue;
            };
            if !meta.is_file() {
                out.push(FileData { bytes: None, stat: Some(meta_to_file_stat(&meta)) });
                continue;
            }
            let stat = meta_to_file_stat(&meta);
            let bytes = if meta.len() == 0 {
                Some(OwnVec::new_in(alloc))
            } else if let Ok(mapped) = Mapping::map_file(&path) {
                let mut v = OwnVec::with_capacity_in(mapped.bytes().len(), alloc);
                v.extend_from_slice(mapped.bytes());
                Some(v)
            } else {
                None
            };
            out.push(FileData { bytes, stat: Some(stat) });
        }
        Ok(Got::FilesWithStat(out))
    }

    fn exec_list_dirs<'a>(
        &mut self,
        dirs: &[ScopedPath<'a>],
        alloc: Alloc<'a>,
    ) -> io::Result<Got<'a>> {
        let mut out: AVec<'a, Option<DirListing<'a>>> = OwnVec::with_capacity_in(dirs.len(), alloc);
        for _ in 0..dirs.len() {
            out.push(None);
        }

        // Partition worktree dirs into:
        // 1. Cached sparse dirs (<= 4 children in `.bgit/dircache.bgdc`): verify `(dir + children)`
        //    in ONE batched `Effect::Stamp` (`fstatat` across all cores, zero `open`/`getattrlistbulk`/`close`!).
        // 2. Dense or uncached dirs: run `Effect::Entries` (`getattrlistbulk`) in parallel alongside
        //    the `Effect::Stamp` batch!
        if let Some(drv) = &mut self.worktree_io {
            let hide_root_bgit = self.hide_root_bgit;
            let wall_ns = drv.wall_ns();

            struct CachedCheck {
                slot: usize,
                dir_stamp_idx: usize,
                child_start: usize,
                child_count: usize,
            }
            let mut cached_checks: Vec<CachedCheck> = Vec::new();
            let mut stamp_names = Names::with_capacity(0, 0);
            let mut effects = Vec::new();

            for (i, sp) in dirs.iter().enumerate() {
                if sp.scope != PathScope::Worktree {
                    continue;
                }
                let dir_str = sp.path.as_str();
                if !dir_str.is_empty()
                    && let Some(dc) = self.dircache.as_ref()
                    && let Some(entry) = dc.entries.get(dir_str)
                    && stamp_names.len() + 1 + entry.children.len() <= STAMP_MOST
                {
                    let dir_stamp_idx = stamp_names.len();
                    if stamp_names.push_str(dir_str) {
                        let child_start = stamp_names.len();
                        let mut all_pushed = true;
                        let mut scratch = String::with_capacity(dir_str.len() + 32);
                        for cname in &entry.children {
                            scratch.clear();
                            scratch.push_str(dir_str);
                            scratch.push('/');
                            scratch.push_str(cname);
                            if !stamp_names.push_str(&scratch) {
                                all_pushed = false;
                                break;
                            }
                        }
                        if all_pushed {
                            cached_checks.push(CachedCheck {
                                slot: i,
                                dir_stamp_idx,
                                child_start,
                                child_count: entry.children.len(),
                            });
                            continue;
                        }
                    }
                }
                let dir_name = if dir_str.is_empty() {
                    DirName::Root
                } else if let Some(fname) = FileName::new(dir_str) {
                    DirName::from(fname)
                } else {
                    continue;
                };
                effects.push(Effect::Entries { dir: dir_name, tag: Tag(i as u64) });
            }

            const STAMP_TAG: u64 = u64::MAX;
            if !cached_checks.is_empty() {
                effects.push(Effect::Stamp { files: Arc::new(stamp_names), tag: Tag(STAMP_TAG) });
            }

            let mut stamps_out: Option<Vec<Stamp>> = None;
            let mut new_sparse_candidates: Vec<(usize, Vec<String>)> = Vec::new();
            let record_dircache = self.dircache.is_some();

            if !effects.is_empty() {
                blaze_core::run_until_idle(drv, &mut effects, |comp, _| {
                    if comp.tag.0 == STAMP_TAG {
                        if let Done::Stamped(stamps) = comp.done {
                            stamps_out = Some(stamps);
                        }
                        return;
                    }
                    let slot = comp.tag.0 as usize;
                    if let Done::Entries(listing) = comp.done {
                        let is_root = listing.dir().name().is_none();
                        if record_dircache
                            && !is_root
                            && listing.len() <= MAX_SPARSE_CHILDREN
                        {
                            let mut names = Vec::with_capacity(listing.len());
                            for k in 0..listing.len() {
                                names.push(listing.leaf(k).as_str().to_owned());
                            }
                            new_sparse_candidates.push((slot, names));
                        }
                        out[slot] = Some(listing_to_dir_listing(
                            &listing,
                            is_root && hide_root_bgit,
                            alloc,
                        ));
                    }
                })
                .map_err(io::Error::from)?;
            }

            // Verify every cached sparse dir's `(mtime_ns, ctime_ns, ino)`!
            let mut stale_slots: Vec<usize> = Vec::new();
            if let Some(stamps) = stamps_out {
                for chk in cached_checks {
                    let dir_str = dirs[chk.slot].path.as_str();
                    let dir_st = stamps[chk.dir_stamp_idx];
                    let entry = self
                        .dircache
                        .as_ref()
                        .and_then(|dc| dc.entries.get(dir_str));
                    let valid = entry.is_some_and(|e| {
                        CoreKind::of(&dir_st) == CoreKind::Dir
                            && dir_st.mtime_ns > 0
                            && dir_st.mtime_ns == e.mtime_ns
                            && dir_st.ctime_ns == e.ctime_ns
                            && dir_st.ino == e.ino
                    });
                    if valid {
                        let e = entry.expect("checked");
                        let mut dl =
                            DirListing::with_capacity(chk.child_count, chk.child_count * 16, alloc);
                        for (k, cname) in e.children.iter().enumerate() {
                            let cst = stamps[chk.child_start + k];
                            let kind = CoreKind::of(&cst);
                            if kind != CoreKind::Gone {
                                dl.push(cname, stamp_to_file_stat(cst, kind));
                            }
                        }
                        out[chk.slot] = Some(dl);
                    } else {
                        stale_slots.push(chk.slot);
                    }
                }
            } else {
                for chk in cached_checks {
                    stale_slots.push(chk.slot);
                }
            }

            // Re-list any stale directories with `Effect::Entries` (`getattrlistbulk`) and update cache.
            if !stale_slots.is_empty() {
                let mut retry_effects = Vec::with_capacity(stale_slots.len());
                for &slot in &stale_slots {
                    let dir_str = dirs[slot].path.as_str();
                    if let Some(dc) = self.dircache.as_mut()
                        && dc.entries.remove(dir_str).is_some()
                    {
                        dc.dirty = true;
                    }
                    if let Some(fname) = FileName::new(dir_str) {
                        retry_effects
                            .push(Effect::Entries { dir: DirName::from(fname), tag: Tag(slot as u64) });
                    }
                }
                if !retry_effects.is_empty() {
                    blaze_core::run_until_idle(drv, &mut retry_effects, |comp, _| {
                        let slot = comp.tag.0 as usize;
                        if let Done::Entries(listing) = comp.done {
                            if record_dircache && listing.len() <= MAX_SPARSE_CHILDREN {
                                let mut names = Vec::with_capacity(listing.len());
                                for k in 0..listing.len() {
                                    names.push(listing.leaf(k).as_str().to_owned());
                                }
                                new_sparse_candidates.push((slot, names));
                            }
                            out[slot] = Some(listing_to_dir_listing(&listing, false, alloc));
                        }
                    })
                    .map_err(io::Error::from)?;
                }
            }

            // Stamp any newly discovered sparse dirs ONCE so future runs skip `getattrlistbulk`!
            if !new_sparse_candidates.is_empty() && self.dircache.is_some() {
                let mut cand_names = Names::with_capacity(
                    new_sparse_candidates.len(),
                    new_sparse_candidates.len() * 24,
                );
                let mut accepted: Vec<(usize, Vec<String>)> =
                    Vec::with_capacity(new_sparse_candidates.len());
                for (slot, names) in new_sparse_candidates {
                    if cand_names.push_str(dirs[slot].path.as_str()) {
                        accepted.push((slot, names));
                    }
                }
                if !accepted.is_empty() {
                    let mut st_eff =
                        vec![Effect::Stamp { files: Arc::new(cand_names), tag: Tag(0) }];
                    let mut cand_stamps: Option<Vec<Stamp>> = None;
                    let _ = blaze_core::run_until_idle(drv, &mut st_eff, |comp, _| {
                        if let Done::Stamped(s) = comp.done {
                            cand_stamps = Some(s);
                        }
                    });
                    if let (Some(stamps), Some(dc)) = (cand_stamps, self.dircache.as_mut()) {
                        for ((slot, names), st) in accepted.into_iter().zip(stamps) {
                            if CoreKind::of(&st) == CoreKind::Dir
                                && st.mtime_ns > 0
                                && st.mtime_ns.saturating_add(RACY_DIR_WINDOW_NS) < wall_ns
                            {
                                dc.entries.insert(
                                    dirs[slot].path.as_str().to_owned(),
                                    SparseDirEntry {
                                        mtime_ns: st.mtime_ns,
                                        ctime_ns: st.ctime_ns,
                                        ino: st.ino,
                                        children: names,
                                    },
                                );
                                dc.dirty = true;
                            }
                        }
                    }
                }
            }
        }

        // GitDir / CommonDir directory listings (e.g. `objects/pack`, `refs/tags`).
        for (i, sp) in dirs.iter().enumerate() {
            if sp.scope == PathScope::Worktree {
                continue;
            }
            let Some(full) = self.resolve_path(sp.scope, sp.path.as_str()) else {
                continue;
            };
            let Ok(rd) = fs::read_dir(&full) else {
                continue;
            };
            let mut entries: Vec<(String, FileStat)> = Vec::new();
            for entry in rd.flatten() {
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                    continue;
                };
                entries.push((name, meta_to_file_stat(&meta)));
            }
            entries.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            let mut dl = DirListing::with_capacity(entries.len(), entries.len() * 16, alloc);
            for (name, st) in entries {
                dl.push(&name, st);
            }
            out[i] = Some(dl);
        }

        Ok(Got::Dirs(out))
    }

    fn exec_stat_paths<'a>(
        &mut self,
        paths: &[ScopedPath<'a>],
        alloc: Alloc<'a>,
    ) -> io::Result<Got<'a>> {
        let mut out: AVec<'a, StatEntry> = OwnVec::with_capacity_in(paths.len(), alloc);
        for _ in 0..paths.len() {
            out.push(StatEntry { stat: FileStat::GONE });
        }

        // For Worktree paths, group dense directories (>= 4 paths) into parallel `Effect::Entries`
        // (`getattrlistbulk`) and send sparse paths in parallel chunked `Effect::Stamp` batches!
        if let Some(drv) = &mut self.worktree_io {
            let mut by_dir: HashMap<&str, Vec<(usize, &str)>> = HashMap::new();
            for (i, sp) in paths.iter().enumerate() {
                if sp.scope != PathScope::Worktree {
                    continue;
                }
                let s = sp.path.as_str();
                let (dir, leaf) = match s.rfind('/') {
                    Some(pos) => (&s[..pos], &s[pos + 1..]),
                    None => ("", s),
                };
                by_dir.entry(dir).or_default().push((i, leaf));
            }

            let mut bulk_dirs: Vec<(&str, Vec<(usize, &str)>)> = Vec::new();
            let mut sparse_slots: Vec<usize> = Vec::new();
            for (dir, items) in by_dir {
                if items.len() >= 4 {
                    bulk_dirs.push((dir, items));
                } else {
                    for (slot, _) in items {
                        sparse_slots.push(slot);
                    }
                }
            }

            let mut effects = Vec::new();
            for (k, (dir, _)) in bulk_dirs.iter().enumerate() {
                let dir_name = if dir.is_empty() {
                    DirName::Root
                } else if let Some(fname) = FileName::new(dir) {
                    DirName::from(fname)
                } else {
                    continue;
                };
                effects.push(Effect::Entries { dir: dir_name, tag: Tag(k as u64) });
            }

            // Encode sparse `Effect::Stamp` batches with tag >= bulk_dirs.len().
            // `blaze_core::fs::stamps` already fans out across all CPU cores internally!
            let stamp_base = bulk_dirs.len();
            let mut stamp_batches: Vec<Vec<usize>> = Vec::new();
            let chunk_size = STAMP_MOST;
            for chunk in sparse_slots.chunks(chunk_size) {
                let mut names = Names::with_capacity(chunk.len(), chunk.len() * 32);
                let mut batch_slots = Vec::with_capacity(chunk.len());
                for &slot in chunk {
                    if names.push_str(paths[slot].path.as_str()) {
                        batch_slots.push(slot);
                    }
                }
                if !batch_slots.is_empty() {
                    let tag_idx = stamp_base + stamp_batches.len();
                    stamp_batches.push(batch_slots);
                    effects.push(Effect::Stamp {
                        files: Arc::new(names),
                        tag: Tag(tag_idx as u64),
                    });
                }
            }

            if !effects.is_empty() {
                blaze_core::run_until_idle(drv, &mut effects, |comp, _| {
                    let idx = comp.tag.0 as usize;
                    match comp.done {
                        Done::Entries(listing) if idx < stamp_base => {
                            for &(slot, leaf) in &bulk_dirs[idx].1 {
                                if let Some(pos) = listing.find(leaf) {
                                    out[slot] = StatEntry {
                                        stat: stamp_to_file_stat(
                                            listing.stamp(pos),
                                            listing.kind(pos),
                                        ),
                                    };
                                }
                            }
                        }
                        Done::Stamped(stamps) if idx >= stamp_base => {
                            let slots = &stamp_batches[idx - stamp_base];
                            for (&slot, &stamp) in slots.iter().zip(stamps.iter()) {
                                let kind = CoreKind::of(&stamp);
                                out[slot] = StatEntry { stat: stamp_to_file_stat(stamp, kind) };
                            }
                        }
                        _ => {}
                    }
                })
                .map_err(io::Error::from)?;
            }
        }

        // Non-worktree paths (rare).
        for (i, sp) in paths.iter().enumerate() {
            if sp.scope == PathScope::Worktree {
                continue;
            }
            if let Some(full) = self.resolve_path(sp.scope, sp.path.as_str())
                && let Ok(meta) = fs::symlink_metadata(&full)
            {
                out[i] = StatEntry { stat: meta_to_file_stat(&meta) };
            }
        }

        Ok(Got::Stats(out))
    }

    fn exec_read_links<'a>(
        &mut self,
        paths: &[ScopedPath<'a>],
        alloc: Alloc<'a>,
    ) -> io::Result<Got<'a>> {
        let mut out: AVec<'a, Option<AVec<'a, u8>>> = OwnVec::with_capacity_in(paths.len(), alloc);
        for sp in paths.iter() {
            let Some(full) = self.resolve_path(sp.scope, sp.path.as_str()) else {
                out.push(None);
                continue;
            };
            if let Ok(target) = fs::read_link(&full) {
                let bytes = target.as_os_str().as_encoded_bytes();
                let mut v = OwnVec::with_capacity_in(bytes.len(), alloc);
                v.extend_from_slice(bytes);
                out.push(Some(v));
            } else {
                out.push(None);
            }
        }
        Ok(Got::Links(out))
    }

    fn exec_read_ranges<'a>(
        &mut self,
        reads: &[blaze_git::RangeRead<'a>],
        alloc: Alloc<'a>,
    ) -> io::Result<Got<'a>> {
        let mut out: AVec<'a, blaze_git::RangeData<'a>> = OwnVec::with_capacity_in(reads.len(), alloc);
        let mut open_files: HashMap<PathBuf, File> = HashMap::new();
        for rr in reads.iter() {
            let Some(full) = self.resolve_path(rr.scope, rr.path.as_str()) else {
                out.push(RangeData { bytes: None });
                continue;
            };
            let file = match open_files.entry(full.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => match File::open(&full) {
                    Ok(f) => v.insert(f),
                    Err(_) => {
                        out.push(RangeData { bytes: None });
                        continue;
                    }
                },
            };
            let mut buf = vec![0u8; rr.len as usize];
            match file.read_at(&mut buf, rr.offset) {
                Ok(n) => {
                    let mut v = OwnVec::with_capacity_in(n, alloc);
                    v.extend_from_slice(&buf[..n]);
                    out.push(RangeData { bytes: Some(v) });
                }
                Err(_) => out.push(RangeData { bytes: None }),
            }
        }
        Ok(Got::Ranges(out))
    }
}

fn listing_to_dir_listing<'a>(
    listing: &Listing,
    skip_bgit: bool,
    alloc: Alloc<'a>,
) -> DirListing<'a> {
    let mut dl = DirListing::with_capacity(listing.len(), listing.len() * 16, alloc);
    for i in 0..listing.len() {
        let name = listing.leaf(i).as_str();
        if skip_bgit && name == ".bgit" {
            continue;
        }
        dl.push(name, stamp_to_file_stat(listing.stamp(i), listing.kind(i)));
    }
    dl
}

fn run_git_status<'a>(
    worktree: Option<PathBuf>,
    git_dir: PathBuf,
    common_dir: PathBuf,
    bgit_dir: Option<PathBuf>,
    kind: WorktreeKind,
    opts: StatusOpts,
    alloc: Alloc<'a>,
) -> io::Result<(PromptStatus<'a>, StatusTiming)> {
    // O(dirty) fast-path when an in-process FSEvents monitor (`blaze-shell` host) is active!
    if let Some(wt) = worktree.as_deref()
        && let Some(cached) = crate::fsmonitor::try_cached_status(
            wt,
            &git_dir,
            &common_dir,
            bgit_dir.as_deref(),
            opts,
            alloc,
        )
    {
        return Ok((cached, StatusTiming::default()));
    }
    let scan_start_gen = worktree.as_deref().and_then(crate::fsmonitor::begin_full_scan);

    let worktree_io = match &worktree {
        Some(wt) => Some(blaze_core::native(wt, default_io_threads())?),
        None => None,
    };
    let hide_root_bgit = bgit_dir.is_some();
    let dircache = bgit_dir.as_deref().map(DirCache::load);
    let mut driver = NativeGitDriver {
        worktree,
        worktree_io,
        git_dir,
        common_dir,
        bgit_dir,
        hide_root_bgit,
        dircache,
    };

    let mut machine = StatusMachine::new(opts, kind, alloc, NoPlant);
    if let Some(global_ignore) = load_global_exclude() {
        machine.set_global_exclude(&global_ignore);
    }

    let mut timing = StatusTiming::default();
    loop {
        let m0 = Instant::now();
        let step = machine.poll().map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        timing.machine_us += m0.elapsed().as_micros() as u64;
        match step {
            Step::Done(status) => {
                if let Some(dc) = driver.dircache.as_mut() {
                    dc.flush_if_dirty();
                }
                if let Some(wt) = driver.worktree.as_deref() {
                    crate::fsmonitor::record_status_baseline(
                        wt,
                        &driver.git_dir,
                        &driver.common_dir,
                        driver.bgit_dir.as_deref(),
                        opts,
                        &status,
                        scan_start_gen,
                    );
                }
                return Ok((status, timing));
            }
            Step::Need(want) => {
                timing.rounds += 1;
                let io0 = Instant::now();
                let got = match want {
                    Want::ReadFiles(paths) => {
                        timing.read_files_count += paths.len() as u32;
                        let r = driver.exec_read_files(&paths, alloc)?;
                        timing.read_files_us += io0.elapsed().as_micros() as u64;
                        r
                    }
                    Want::ReadWithStat(paths) => {
                        let r = driver.exec_read_with_stat(&paths, alloc)?;
                        timing.read_with_stat_us += io0.elapsed().as_micros() as u64;
                        r
                    }
                    Want::ListDirs(dirs) => {
                        timing.list_dirs_count += dirs.len() as u32;
                        let r = driver.exec_list_dirs(&dirs, alloc)?;
                        timing.list_dirs_us += io0.elapsed().as_micros() as u64;
                        r
                    }
                    Want::StatPaths(paths) => {
                        timing.stat_paths_count += paths.len() as u32;
                        let r = driver.exec_stat_paths(&paths, alloc)?;
                        timing.stat_paths_us += io0.elapsed().as_micros() as u64;
                        r
                    }
                    Want::ReadLinks(paths) => {
                        let r = driver.exec_read_links(&paths, alloc)?;
                        timing.read_links_us += io0.elapsed().as_micros() as u64;
                        r
                    }
                    Want::ReadRanges(reads) => {
                        let r = driver.exec_read_ranges(&reads, alloc)?;
                        timing.read_ranges_us += io0.elapsed().as_micros() as u64;
                        r
                    }
                };
                let f0 = Instant::now();
                machine
                    .feed(got)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                timing.machine_us += f0.elapsed().as_micros() as u64;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pure `.bgit`-only repository status (no `.git/index` present)
// ---------------------------------------------------------------------------

struct TrackedTreeBlob {
    path: String,
    mode: u32,
    oid: Oid,
}

fn flatten_bgit_tree<'a>(
    store: &BgitDiskStore<'a>,
    tree_oid: Oid,
    prefix: &str,
    depth: u16,
    alloc: Alloc<'a>,
    out: &mut Vec<TrackedTreeBlob>,
) -> io::Result<()> {
    if depth > Limits::SHIPPED.most_dir_depth {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "tree depth exceeded"));
    }
    let mut raw = OwnVec::new_in(alloc);
    let kind = store
        .read_object(&tree_oid, &mut raw)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    if kind != Some(ObjectKind::Tree) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected tree object"));
    }
    let mut iter = TreeIter::new(tree_oid, &raw);
    while let Some(te) = iter
        .next_entry()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
    {
        let Ok(name) = std::str::from_utf8(te.name) else {
            continue;
        };
        let full = if prefix.is_empty() { name.to_string() } else { format!("{prefix}/{name}") };
        if te.is_dir() {
            flatten_bgit_tree(store, te.oid, &full, depth + 1, alloc, out)?;
        } else {
            out.push(TrackedTreeBlob { path: full, mode: te.mode, oid: te.oid });
        }
    }
    Ok(())
}

fn run_bgit_only_status<'a>(
    worktree: &Path,
    opts: StatusOpts,
    alloc: Alloc<'a>,
) -> io::Result<(PromptStatus<'a>, StatusTiming)> {
    let store = BgitDiskStore::open(worktree, alloc)?;
    let (head, head_oid) = match store
        .read_head(alloc)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
    {
        Some(RefTarget::Symbolic(sym)) => {
            let branch = sym.as_str().strip_prefix("refs/heads/").unwrap_or(sym.as_str());
            let oid = store
                .resolve_ref("HEAD")
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            let short = oid.map(|o| ShortOid::from_oid(o, opts.short_oid_len));
            (Head::Branch { name: OwnString::from_str_in(branch, alloc), oid, short }, oid)
        }
        Some(RefTarget::Direct(oid)) => {
            (Head::Detached { oid, short: ShortOid::from_oid(oid, opts.short_oid_len) }, Some(oid))
        }
        None => (
            Head::Branch {
                name: OwnString::from_str_in("main", alloc),
                oid: None,
                short: None,
            },
            None,
        ),
    };

    // Flatten HEAD tree from `.bgit` mmap slabs.
    let mut tracked = Vec::new();
    if let Some(oid) = head_oid {
        let mut raw = OwnVec::new_in(alloc);
        let kind = store
            .read_object(&oid, &mut raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        if kind == Some(ObjectKind::Commit) {
            let commit = Commit::parse(&raw)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?;
            flatten_bgit_tree(&store, commit.tree, "", 0, alloc, &mut tracked)?;
        }
    }
    tracked.sort_unstable_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));

    let mut ignores = IgnoreStack::new(opts.limits, alloc);
    if let Some(global_ignore) = load_global_exclude() {
        ignores.set_global_exclude(&global_ignore, NoPlant);
    }

    let mut drv = blaze_core::native(worktree, default_io_threads())?;
    let mut timing = StatusTiming::default();

    // BFS walk worktree via parallel `Effect::Entries` (`getattrlistbulk`), loading `.gitignore`s.
    let mut queue: Vec<String> = vec![String::new()];
    let mut disk_files: HashMap<String, (CoreKind, u32)> = HashMap::new();
    let mut untracked_entries: Vec<String> = Vec::new();
    let mut collapse_roots: Vec<String> = Vec::new();

    while !queue.is_empty() {
        let wave = std::mem::take(&mut queue);
        let mut effects = Vec::with_capacity(wave.len());
        for (k, dir) in wave.iter().enumerate() {
            let dir_name = if dir.is_empty() {
                DirName::Root
            } else if let Some(fname) = FileName::new(dir) {
                DirName::from(fname)
            } else {
                continue;
            };
            effects.push(Effect::Entries { dir: dir_name, tag: Tag(k as u64) });
        }
        let mut listings: Vec<Option<Box<Listing>>> = (0..wave.len()).map(|_| None).collect();
        let io0 = Instant::now();
        blaze_core::run_until_idle(&mut drv, &mut effects, |comp, _| {
            if let Done::Entries(listing) = comp.done {
                listings[comp.tag.0 as usize] = Some(listing);
            }
        })
        .map_err(io::Error::from)?;
        timing.list_dirs_count += wave.len() as u32;
        timing.list_dirs_us += io0.elapsed().as_micros() as u64;
        timing.rounds += 1;

        for (dir, maybe_listing) in wave.into_iter().zip(listings.into_iter()) {
            let Some(listing) = maybe_listing else {
                continue;
            };
            if listing.find(".gitignore").is_some() {
                let gi_path =
                    if dir.is_empty() { worktree.join(".gitignore") } else { worktree.join(&dir).join(".gitignore") };
                if let Ok(bytes) = fs::read(gi_path) {
                    ignores.add_folder_gitignore(&dir, &bytes, NoPlant);
                }
            }
            for i in 0..listing.len() {
                let leaf = listing.leaf(i).as_str();
                if dir.is_empty() && (leaf == ".bgit" || leaf == ".git") {
                    continue;
                }
                let child = if dir.is_empty() { leaf.to_string() } else { format!("{dir}/{leaf}") };
                let kind = listing.kind(i);
                if kind == CoreKind::Dir {
                    if ignores.is_ignored(&child, true) {
                        continue;
                    }
                    let has_tracked = tracked_has_prefix(&tracked, &child);
                    if has_tracked || opts.untracked == UntrackedMode::All {
                        queue.push(child);
                    } else if opts.untracked == UntrackedMode::Normal {
                        collapse_roots.push(child);
                    }
                } else {
                    let mode = listing.stamp(i).mode;
                    if tracked.binary_search_by(|t| t.path.as_bytes().cmp(child.as_bytes())).is_ok() {
                        disk_files.insert(child, (kind, mode));
                    } else if opts.untracked != UntrackedMode::Off && !ignores.is_ignored(&child, false) {
                        untracked_entries.push(child);
                    }
                }
            }
        }
    }

    // Check collapsed untracked directories to see if they contain at least one non-ignored file.
    for root in collapse_roots {
        if dir_has_non_ignored_file(worktree, &root, &ignores) {
            untracked_entries.push(format!("{root}/"));
        }
    }

    // Compare tracked files in parallel across cores!
    let mut counts = StatusCounts::default();
    let mut entries: AVec<'a, StatusEntry<'a>> = OwnVec::new_in(alloc);

    let mut to_hash: Vec<(usize, bool)> = Vec::new();
    for (idx, t) in tracked.iter().enumerate() {
        let Some(&(kind, mode)) = disk_files.get(&t.path) else {
            counts.deleted += 1;
            if opts.detail == Detail::Full {
                entries.push(StatusEntry {
                    path: OwnString::from_str_in(&t.path, alloc),
                    index: FileStatus::Unmodified,
                    worktree: FileStatus::Deleted,
                    unmerged: false,
                });
            }
            continue;
        };
        let is_symlink = (t.mode & 0o170000) == 0o120000;
        let want_kind = if is_symlink { CoreKind::Link } else { CoreKind::File };
        if kind != want_kind {
            counts.typechanged += 1;
            if opts.detail == Detail::Full {
                entries.push(StatusEntry {
                    path: OwnString::from_str_in(&t.path, alloc),
                    index: FileStatus::Unmodified,
                    worktree: FileStatus::TypeChanged,
                    unmerged: false,
                });
            }
            continue;
        }
        if !is_symlink && ((t.mode & 0o111 != 0) != (mode & 0o111 != 0)) {
            counts.modified += 1;
            if opts.detail == Detail::Full {
                entries.push(StatusEntry {
                    path: OwnString::from_str_in(&t.path, alloc),
                    index: FileStatus::Unmodified,
                    worktree: FileStatus::Modified,
                    unmerged: false,
                });
            }
            continue;
        }
        to_hash.push((idx, is_symlink));
    }

    let modified_indices: Vec<usize> = if to_hash.len() >= 8 {
        let n_threads = default_io_threads().min(to_hash.len());
        let chunk_sz = to_hash.len().div_ceil(n_threads);
        std::thread::scope(|s| {
            let mut handles = Vec::new();
            for chunk in to_hash.chunks(chunk_sz) {
                let tracked_ref = &tracked;
                handles.push(s.spawn(move || {
                    let mut diffs = Vec::new();
                    for &(idx, is_symlink) in chunk {
                        let t = &tracked_ref[idx];
                        let full = worktree.join(&t.path);
                        let oid = if is_symlink {
                            fs::read_link(&full)
                                .ok()
                                .map(|p| Oid::of(ObjectKind::Blob, p.as_os_str().as_encoded_bytes()))
                        } else {
                            fs::read(&full).ok().map(|b| Oid::of(ObjectKind::Blob, &b))
                        };
                        if oid != Some(t.oid) {
                            diffs.push(idx);
                        }
                    }
                    diffs
                }));
            }
            let mut all = Vec::new();
            for h in handles {
                all.extend(h.join().unwrap_or_default());
            }
            all
        })
    } else {
        let mut diffs = Vec::new();
        for &(idx, is_symlink) in &to_hash {
            let t = &tracked[idx];
            let full = worktree.join(&t.path);
            let oid = if is_symlink {
                fs::read_link(&full)
                    .ok()
                    .map(|p| Oid::of(ObjectKind::Blob, p.as_os_str().as_encoded_bytes()))
            } else {
                fs::read(&full).ok().map(|b| Oid::of(ObjectKind::Blob, &b))
            };
            if oid != Some(t.oid) {
                diffs.push(idx);
            }
        }
        diffs
    };

    for idx in modified_indices {
        counts.modified += 1;
        if opts.detail == Detail::Full {
            entries.push(StatusEntry {
                path: OwnString::from_str_in(&tracked[idx].path, alloc),
                index: FileStatus::Unmodified,
                worktree: FileStatus::Modified,
                unmerged: false,
            });
        }
    }

    for path in untracked_entries {
        counts.untracked += 1;
        if opts.detail == Detail::Full {
            entries.push(StatusEntry {
                path: OwnString::from_str_in(&path, alloc),
                index: FileStatus::Untracked,
                worktree: FileStatus::Untracked,
                unmerged: false,
            });
        }
    }

    entries.sort_unstable_by(|a, b| a.path.as_str().cmp(b.path.as_str()));

    Ok((
        PromptStatus {
            head,
            tag: None,
            state: RepoState::Clean,
            upstream: None,
            counts,
            worktree: WorktreeKind::Normal,
            truncated: Truncated::default(),
            changed_samples: OwnVec::new_in(alloc),
            entries,
        },
        timing,
    ))
}

fn tracked_has_prefix(tracked: &[TrackedTreeBlob], dir: &str) -> bool {
    let dir_bytes = dir.as_bytes();
    let start = tracked.partition_point(|t| {
        let p = t.path.as_bytes();
        let n = p.len().min(dir_bytes.len());
        match p[..n].cmp(&dir_bytes[..n]) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Equal if p.len() <= dir_bytes.len() => true,
            std::cmp::Ordering::Equal => p[dir_bytes.len()] < b'/',
        }
    });
    tracked.get(start).is_some_and(|t| {
        let p = t.path.as_bytes();
        p.len() > dir_bytes.len() && p.starts_with(dir_bytes) && p[dir_bytes.len()] == b'/'
    })
}

fn dir_has_non_ignored_file(worktree: &Path, rel: &str, ignores: &IgnoreStack<'_>) -> bool {
    let mut stack = vec![rel.to_string()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(worktree.join(&dir)) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let child = format!("{dir}/{name}");
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                if !ignores.is_ignored(&child, true) {
                    stack.push(child);
                }
            } else if !ignores.is_ignored(&child, false) {
                return true;
            }
        }
    }
    false
}
