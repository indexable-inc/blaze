//! Sans-IO lazy copy-on-write Virtual Filesystem ([`LazyVfs`]) over any [`ObjectStore`].
//!
//! Mounts a Git commit tree in **O(1)** time (zero files written to disk, zero subtrees parsed up
//! front). Directories are hydrated lazily on first `lookup` / `readdir`; file sizes are served
//! via [`ObjectStore::object_size`] (O(log N) with zero Zstd decompression on `.bgit` `BGIX`);
//! file payloads are read lazily on demand; and edits/creates/deletes/renames are held in a
//! copy-on-write overlay that computes exact `status` and writes new Git trees in **O(dirty)**
//! time regardless of repository size.
//!
//! Also provides the `BGOV` v1 binary overlay-status manifest codec so `bgit status <mount>` can
//! read exact dirty status in microseconds without walking the mounted tree.

#![blaze::sans_io]

use blaze_git_fetch::{Kind as ObjectKind, Oid};
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::{
    ErrorKind, FileStatus, GitError, GitFile, StatusCounts, StatusEntry, UntrackedMode,
    objects::TreeIter,
    storage::{ObjectStore, TreeBuildEntry, encode_tree},
};

/// Root directory inode number (POSIX / NFSv3 convention: never 0).
pub const ROOT_INO: u64 = 1;

/// Bounds for [`LazyVfs`] (rule `bounded-progress`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VfsLimits {
    pub most_inodes: usize,
    pub most_file_bytes: usize,
    pub most_dir_depth: u16,
}

impl VfsLimits {
    pub const SHIPPED: VfsLimits = VfsLimits {
        most_inodes: 4 << 20,
        most_file_bytes: 256 << 20,
        most_dir_depth: 128,
    };
}

/// Configuration for [`LazyVfs::new`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VfsOpts {
    pub read_only: bool,
    pub uid: u32,
    pub gid: u32,
    pub base_mtime_sec: u32,
    pub limits: VfsLimits,
}

impl VfsOpts {
    pub const DEFAULT: VfsOpts = VfsOpts {
        read_only: false,
        uid: 501,
        gid: 20,
        base_mtime_sec: 1_700_000_000,
        limits: VfsLimits::SHIPPED,
    };
}

/// POSIX / NFSv3 node kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VfsKind {
    File,
    Dir,
    Symlink,
}

/// POSIX / NFSv3 attributes for one inode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VfsAttr {
    pub ino: u64,
    pub kind: VfsKind,
    /// Full POSIX mode bits (e.g. `0o100644`, `0o100755`, `0o040755`, `0o120777`).
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub mtime_sec: u32,
    pub ctime_sec: u32,
}

/// One entry returned by [`LazyVfs::readdir`].
#[derive(PartialEq, Eq, Debug)]
pub struct VfsDirEntry<'a> {
    pub name: AString<'a>,
    pub ino: u64,
    pub cookie: u64,
    pub attr: VfsAttr,
}

/// Typed VFS error mapped directly to NFSv3 `nfsstat3` status codes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VfsError {
    NoEnt,
    Exist,
    NotDir,
    IsDir,
    NotEmpty,
    ReadOnly,
    Inval,
    NameTooLong,
    NoSpc,
    Io(GitError),
}

impl From<GitError> for VfsError {
    fn from(e: GitError) -> VfsError {
        VfsError::Io(e)
    }
}

/// Live hydration and overlay telemetry counters.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct VfsStats {
    pub trees_hydrated: u64,
    pub blobs_stat_cached: u64,
    pub blobs_read_from_store: u64,
    pub cow_promotions: u64,
    pub total_inodes: u64,
    pub overlay_generation: u64,
}

/// Base Git tree entry recorded when a directory is first hydrated (for O(dirty) status diffing).
#[derive(Debug)]
struct BaseChild<'a> {
    name: AString<'a>,
    oid: Oid,
    mode: u32,
}

#[derive(Debug)]
struct DirState<'a> {
    base_tree: Option<Oid>,
    loaded: bool,
    /// Base entries from `base_tree` (populated when `loaded` becomes true).
    base_children: AVec<'a, BaseChild<'a>>,
    /// Current live entries sorted by `name` bytes: `(name, child_ino)`.
    children: AVec<'a, (AString<'a>, u64)>,
    /// True if this directory or any descendant has overlay edits/creates/deletes.
    subtree_dirty: bool,
    /// True if this directory itself had children added, removed, or renamed.
    dir_dirty: bool,
}

#[derive(Debug)]
struct FileState<'a> {
    base_oid: Option<Oid>,
    size_hint: Option<u64>,
    overlay_bytes: Option<AVec<'a, u8>>,
    mode: u32,
    dirty: bool,
}

#[derive(Debug)]
struct SymlinkState<'a> {
    base_oid: Option<Oid>,
    overlay_target: Option<AVec<'a, u8>>,
}

#[derive(Debug)]
enum NodeBody<'a> {
    Dir(DirState<'a>),
    File(FileState<'a>),
    Symlink(SymlinkState<'a>),
}

#[derive(Debug)]
struct VfsNode<'a> {
    ino: u64,
    parent: u64,
    mtime_sec: u32,
    ctime_sec: u32,
    body: NodeBody<'a>,
}

/// Lazy copy-on-write virtual filesystem over a Git commit tree stored in any [`ObjectStore`].
pub struct LazyVfs<'a> {
    opts: VfsOpts,
    nodes: AVec<'a, VfsNode<'a>>,
    stats: VfsStats,
    alloc: Alloc<'a>,
}

impl<'a> LazyVfs<'a> {
    /// Mount `root_tree` lazily in **O(1)** (0 objects read, 0 files materialized).
    pub fn new(root_tree: Option<Oid>, opts: VfsOpts, alloc: Alloc<'a>) -> LazyVfs<'a> {
        let mut nodes = Vec::with_capacity_in(64, alloc);
        nodes.push(VfsNode {
            ino: ROOT_INO,
            parent: ROOT_INO,
            mtime_sec: opts.base_mtime_sec,
            ctime_sec: opts.base_mtime_sec,
            body: NodeBody::Dir(DirState {
                base_tree: root_tree,
                loaded: root_tree.is_none(),
                base_children: Vec::new_in(alloc),
                children: Vec::new_in(alloc),
                subtree_dirty: false,
                dir_dirty: false,
            }),
        });
        let stats = VfsStats { total_inodes: 1, ..VfsStats::default() };
        LazyVfs { opts, nodes, stats, alloc }
    }

    #[inline]
    pub fn opts(&self) -> VfsOpts {
        self.opts
    }

    #[inline]
    pub fn stats(&self) -> VfsStats {
        self.stats
    }

    #[inline]
    pub fn overlay_generation(&self) -> u64 {
        self.stats.overlay_generation
    }

    #[inline]
    fn slot(&self, ino: u64) -> Result<usize, VfsError> {
        if ino == 0 || (ino as usize) > self.nodes.len() {
            return Err(VfsError::NoEnt);
        }
        Ok((ino - 1) as usize)
    }

    fn alloc_node(&mut self, parent: u64, now_sec: u32, body: NodeBody<'a>) -> Result<u64, VfsError> {
        if self.nodes.len() >= self.opts.limits.most_inodes {
            return Err(VfsError::NoSpc);
        }
        let ino = (self.nodes.len() as u64) + 1;
        self.nodes.push(VfsNode { ino, parent, mtime_sec: now_sec, ctime_sec: now_sec, body });
        self.stats.total_inodes = self.nodes.len() as u64;
        Ok(ino)
    }

    fn mark_ancestors_dirty(&mut self, mut ino: u64) {
        self.stats.overlay_generation = self.stats.overlay_generation.saturating_add(1);
        for _ in 0..=self.opts.limits.most_dir_depth {
            let Ok(idx) = self.slot(ino) else {
                break;
            };
            let parent = self.nodes[idx].parent;
            if let NodeBody::Dir(ref mut d) = self.nodes[idx].body {
                if d.subtree_dirty {
                    break;
                }
                d.subtree_dirty = true;
            }
            if parent == ino {
                break;
            }
            ino = parent;
        }
    }

    /// Lazily hydrate directory `dir_ino` from `store` on first access.
    fn ensure_dir_loaded(
        &mut self,
        dir_ino: u64,
        store: &impl ObjectStore<'a>,
    ) -> Result<(), VfsError> {
        let idx = self.slot(dir_ino)?;
        let tree_oid = match &self.nodes[idx].body {
            NodeBody::Dir(d) => {
                if d.loaded {
                    return Ok(());
                }
                d.base_tree
            }
            _ => return Err(VfsError::NotDir),
        };

        let Some(oid) = tree_oid else {
            if let NodeBody::Dir(ref mut d) = self.nodes[idx].body {
                d.loaded = true;
            }
            return Ok(());
        };

        let mut raw = Vec::new_in(self.alloc);
        let Some(kind) = store.read_object(&oid, &mut raw)? else {
            return Err(VfsError::Io(GitError {
                file: GitFile::TreeObject(oid),
                at: 0,
                kind: ErrorKind::ObjectMissing(oid),
            }));
        };
        if kind != ObjectKind::Tree {
            return Err(VfsError::Io(GitError {
                file: GitFile::TreeObject(oid),
                at: 0,
                kind: ErrorKind::WrongObjectKind { want: ObjectKind::Tree, got: kind },
            }));
        }

        let mut base_children: AVec<'a, BaseChild<'a>> = Vec::new_in(self.alloc);
        let mut live_children: AVec<'a, (AString<'a>, u64)> = Vec::new_in(self.alloc);
        let base_mtime = self.opts.base_mtime_sec;

        let mut iter = TreeIter::new(oid, &raw);
        while let Some(te) = iter.next_entry()? {
            let Ok(name_str) = core::str::from_utf8(te.name) else {
                continue;
            };
            if name_str.is_empty() || name_str == "." || name_str == ".." || name_str.contains('/') {
                continue;
            }
            let body = if te.is_dir() {
                NodeBody::Dir(DirState {
                    base_tree: Some(te.oid),
                    loaded: false,
                    base_children: Vec::new_in(self.alloc),
                    children: Vec::new_in(self.alloc),
                    subtree_dirty: false,
                    dir_dirty: false,
                })
            } else if (te.mode & 0o170000) == 0o120000 {
                NodeBody::Symlink(SymlinkState {
                    base_oid: Some(te.oid),
                    overlay_target: None,
                })
            } else if (te.mode & 0o170000) == 0o160000 {
                // Gitlink / submodule: expose as empty directory placeholder (matches git checkout).
                NodeBody::Dir(DirState {
                    base_tree: None,
                    loaded: true,
                    base_children: Vec::new_in(self.alloc),
                    children: Vec::new_in(self.alloc),
                    subtree_dirty: false,
                    dir_dirty: false,
                })
            } else {
                let mode = if te.mode & 0o111 != 0 { 0o100755 } else { 0o100644 };
                NodeBody::File(FileState {
                    base_oid: Some(te.oid),
                    size_hint: None,
                    overlay_bytes: None,
                    mode,
                    dirty: false,
                })
            };
            let child_ino = self.alloc_node(dir_ino, base_mtime, body)?;
            base_children.push(BaseChild {
                name: String::from_str_in(name_str, self.alloc),
                oid: te.oid,
                mode: te.mode,
            });
            live_children.push((String::from_str_in(name_str, self.alloc), child_ino));
        }

        base_children.sort_unstable_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        live_children.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

        if let NodeBody::Dir(ref mut d) = self.nodes[idx].body {
            d.base_children = base_children;
            d.children = live_children;
            d.loaded = true;
        }
        self.stats.trees_hydrated = self.stats.trees_hydrated.saturating_add(1);
        Ok(())
    }

    /// Return POSIX attributes for `ino`, using O(log N) zero-decompression `object_size` for
    /// unhydrated base blobs!
    pub fn getattr(
        &mut self,
        ino: u64,
        store: &impl ObjectStore<'a>,
    ) -> Result<VfsAttr, VfsError> {
        let idx = self.slot(ino)?;
        // Resolve lazy blob/symlink size if not cached yet.
        let need_size_oid = match &self.nodes[idx].body {
            NodeBody::File(f) if f.overlay_bytes.is_none() && f.size_hint.is_none() => f.base_oid,
            NodeBody::Symlink(s) if s.overlay_target.is_none() => s.base_oid,
            _ => None,
        };
        let resolved_size = if let Some(oid) = need_size_oid {
            let sz = store.object_size(&oid, self.alloc)?.unwrap_or(0);
            self.stats.blobs_stat_cached = self.stats.blobs_stat_cached.saturating_add(1);
            Some(sz)
        } else {
            None
        };
        if let (Some(sz), NodeBody::File(f)) = (resolved_size, &mut self.nodes[idx].body) {
            f.size_hint = Some(sz);
        }

        let node = &self.nodes[idx];
        let (kind, mode, nlink, size) = match &node.body {
            NodeBody::Dir(d) => {
                let n = if d.loaded { (d.children.len() as u32).saturating_add(2) } else { 2 };
                (VfsKind::Dir, 0o040755, n, 4096)
            }
            NodeBody::File(f) => {
                let sz = if let Some(ref b) = f.overlay_bytes {
                    b.len() as u64
                } else {
                    f.size_hint.unwrap_or(0)
                };
                (VfsKind::File, f.mode, 1, sz)
            }
            NodeBody::Symlink(s) => {
                let sz = if let Some(ref t) = s.overlay_target {
                    t.len() as u64
                } else {
                    resolved_size.unwrap_or(0)
                };
                (VfsKind::Symlink, 0o120777, 1, sz)
            }
        };
        Ok(VfsAttr {
            ino: node.ino,
            kind,
            mode,
            nlink,
            uid: self.opts.uid,
            gid: self.opts.gid,
            size,
            mtime_sec: node.mtime_sec,
            ctime_sec: node.ctime_sec,
        })
    }

    /// Look up single component `name` inside `dir_ino` (handles `.` and `..`).
    pub fn lookup(
        &mut self,
        dir_ino: u64,
        name: &str,
        store: &impl ObjectStore<'a>,
    ) -> Result<(u64, VfsAttr), VfsError> {
        if name == "." {
            let attr = self.getattr(dir_ino, store)?;
            return Ok((dir_ino, attr));
        }
        if name == ".." {
            let idx = self.slot(dir_ino)?;
            let parent = self.nodes[idx].parent;
            let attr = self.getattr(parent, store)?;
            return Ok((parent, attr));
        }
        validate_component(name)?;
        self.ensure_dir_loaded(dir_ino, store)?;
        let idx = self.slot(dir_ino)?;
        let child_ino = match &self.nodes[idx].body {
            NodeBody::Dir(d) => d
                .children
                .binary_search_by(|(n, _)| n.as_bytes().cmp(name.as_bytes()))
                .ok()
                .map(|pos| d.children[pos].1),
            _ => return Err(VfsError::NotDir),
        };
        let Some(ino) = child_ino else {
            return Err(VfsError::NoEnt);
        };
        let attr = self.getattr(ino, store)?;
        Ok((ino, attr))
    }

    /// Read directory entries starting strictly after `after_cookie` (`0` starts at `.`).
    /// Returns `(entries, eof)`.
    pub fn readdir(
        &mut self,
        dir_ino: u64,
        after_cookie: u64,
        max_entries: usize,
        store: &impl ObjectStore<'a>,
    ) -> Result<(AVec<'a, VfsDirEntry<'a>>, bool), VfsError> {
        self.ensure_dir_loaded(dir_ino, store)?;
        let idx = self.slot(dir_ino)?;
        let parent_ino = self.nodes[idx].parent;

        // Snapshot child names & inodes so we can call `self.getattr` cleanly.
        let child_list: AVec<'a, (AString<'a>, u64)> = match &self.nodes[idx].body {
            NodeBody::Dir(d) => {
                let mut tmp = Vec::with_capacity_in(d.children.len(), self.alloc);
                for (n, c_ino) in d.children.iter() {
                    tmp.push((String::from_str_in(n.as_str(), self.alloc), *c_ino));
                }
                tmp
            }
            _ => return Err(VfsError::NotDir),
        };

        let total = 2 + child_list.len();
        let start = (after_cookie as usize).min(total);
        let limit = max_entries.max(1);
        let end = (start + limit).min(total);

        let mut out = Vec::with_capacity_in(end.saturating_sub(start), self.alloc);
        for pos in start..end {
            let cookie = (pos + 1) as u64;
            if pos == 0 {
                let attr = self.getattr(dir_ino, store)?;
                out.push(VfsDirEntry {
                    name: String::from_str_in(".", self.alloc),
                    ino: dir_ino,
                    cookie,
                    attr,
                });
            } else if pos == 1 {
                let attr = self.getattr(parent_ino, store)?;
                out.push(VfsDirEntry {
                    name: String::from_str_in("..", self.alloc),
                    ino: parent_ino,
                    cookie,
                    attr,
                });
            } else {
                let (ref name, c_ino) = child_list[pos - 2];
                let attr = self.getattr(c_ino, store)?;
                out.push(VfsDirEntry {
                    name: String::from_str_in(name.as_str(), self.alloc),
                    ino: c_ino,
                    cookie,
                    attr,
                });
            }
        }
        Ok((out, end >= total))
    }

    /// Read up to `count` bytes starting at `offset` from regular file `ino`. Returns `eof`.
    pub fn read(
        &mut self,
        ino: u64,
        offset: u64,
        count: usize,
        store: &impl ObjectStore<'a>,
        out: &mut AVec<'a, u8>,
    ) -> Result<bool, VfsError> {
        out.clear();
        let idx = self.slot(ino)?;
        let base_oid = match &self.nodes[idx].body {
            NodeBody::File(f) => {
                if let Some(ref bytes) = f.overlay_bytes {
                    let start = (offset as usize).min(bytes.len());
                    let end = start.saturating_add(count).min(bytes.len());
                    out.extend_from_slice(&bytes[start..end]);
                    return Ok(end >= bytes.len());
                }
                f.base_oid
            }
            NodeBody::Dir(_) => return Err(VfsError::IsDir),
            NodeBody::Symlink(_) => return Err(VfsError::Inval),
        };

        let Some(oid) = base_oid else {
            return Ok(true);
        };
        let mut whole = Vec::new_in(self.alloc);
        let Some(kind) = store.read_object(&oid, &mut whole)? else {
            return Err(VfsError::NoEnt);
        };
        if kind != ObjectKind::Blob {
            return Err(VfsError::Inval);
        }
        self.stats.blobs_read_from_store = self.stats.blobs_read_from_store.saturating_add(1);
        if let NodeBody::File(ref mut f) = self.nodes[idx].body {
            f.size_hint = Some(whole.len() as u64);
        }
        let start = (offset as usize).min(whole.len());
        let end = start.saturating_add(count).min(whole.len());
        out.extend_from_slice(&whole[start..end]);
        Ok(end >= whole.len())
    }

    /// Read symlink target bytes of `ino`.
    pub fn readlink(
        &mut self,
        ino: u64,
        store: &impl ObjectStore<'a>,
        out: &mut AVec<'a, u8>,
    ) -> Result<(), VfsError> {
        out.clear();
        let idx = self.slot(ino)?;
        let base_oid = match &self.nodes[idx].body {
            NodeBody::Symlink(s) => {
                if let Some(ref target) = s.overlay_target {
                    out.extend_from_slice(target);
                    return Ok(());
                }
                s.base_oid
            }
            _ => return Err(VfsError::Inval),
        };
        let Some(oid) = base_oid else {
            return Ok(());
        };
        let mut whole = Vec::new_in(self.alloc);
        let Some(kind) = store.read_object(&oid, &mut whole)? else {
            return Err(VfsError::NoEnt);
        };
        if kind != ObjectKind::Blob {
            return Err(VfsError::Inval);
        }
        self.stats.blobs_read_from_store = self.stats.blobs_read_from_store.saturating_add(1);
        out.extend_from_slice(&whole);
        Ok(())
    }

    fn ensure_file_cow(&mut self, idx: usize, skip_read: bool, store: &impl ObjectStore<'a>) -> Result<(), VfsError> {
        let base_oid = match &self.nodes[idx].body {
            NodeBody::File(f) => {
                if f.overlay_bytes.is_some() {
                    return Ok(());
                }
                f.base_oid
            }
            NodeBody::Dir(_) => return Err(VfsError::IsDir),
            NodeBody::Symlink(_) => return Err(VfsError::Inval),
        };
        let mut buf = Vec::new_in(self.alloc);
        if !skip_read && let Some(oid) = base_oid {
            let _ = store.read_object(&oid, &mut buf)?;
            self.stats.blobs_read_from_store = self.stats.blobs_read_from_store.saturating_add(1);
        }
        if let NodeBody::File(ref mut f) = self.nodes[idx].body {
            f.size_hint = Some(buf.len() as u64);
            f.overlay_bytes = Some(buf);
        }
        self.stats.cow_promotions = self.stats.cow_promotions.saturating_add(1);
        Ok(())
    }

    /// Copy-on-write write to regular file `ino` at `offset`.
    pub fn write(
        &mut self,
        ino: u64,
        offset: u64,
        data: &[u8],
        now_sec: u32,
        store: &impl ObjectStore<'a>,
    ) -> Result<VfsAttr, VfsError> {
        if self.opts.read_only {
            return Err(VfsError::ReadOnly);
        }
        let idx = self.slot(ino)?;
        let end = (offset as usize).checked_add(data.len()).ok_or(VfsError::NoSpc)?;
        if end > self.opts.limits.most_file_bytes {
            return Err(VfsError::NoSpc);
        }
        self.ensure_file_cow(idx, false, store)?;
        if let NodeBody::File(ref mut f) = self.nodes[idx].body {
            let buf = f.overlay_bytes.as_mut().expect("cow promoted");
            if end > buf.len() {
                buf.resize(end, 0);
            }
            buf[offset as usize..end].copy_from_slice(data);
            f.size_hint = Some(buf.len() as u64);
            f.dirty = true;
        }
        self.nodes[idx].mtime_sec = now_sec;
        self.nodes[idx].ctime_sec = now_sec;
        let parent = self.nodes[idx].parent;
        self.mark_ancestors_dirty(parent);
        self.getattr(ino, store)
    }

    /// Update mode, size (truncate), or mtime on `ino`.
    pub fn setattr(
        &mut self,
        ino: u64,
        mode: Option<u32>,
        size: Option<u64>,
        mtime_sec: Option<u32>,
        now_sec: u32,
        store: &impl ObjectStore<'a>,
    ) -> Result<VfsAttr, VfsError> {
        if self.opts.read_only {
            return Err(VfsError::ReadOnly);
        }
        let idx = self.slot(ino)?;
        let mut changed_content = false;

        if let Some(new_sz) = size {
            let new_len = new_sz as usize;
            if new_len > self.opts.limits.most_file_bytes {
                return Err(VfsError::NoSpc);
            }
            // Instant O_TRUNC when truncating to 0: no need to inflate the base blob!
            self.ensure_file_cow(idx, new_len == 0, store)?;
            if let NodeBody::File(ref mut f) = self.nodes[idx].body {
                let buf = f.overlay_bytes.as_mut().expect("cow promoted");
                buf.resize(new_len, 0);
                f.size_hint = Some(new_len as u64);
                f.dirty = true;
                changed_content = true;
            }
        }

        if let Some(m) = mode
            && let NodeBody::File(ref mut f) = self.nodes[idx].body
        {
            let norm = if m & 0o111 != 0 { 0o100755 } else { 0o100644 };
            if f.mode != norm {
                f.mode = norm;
                f.dirty = true;
                changed_content = true;
            }
        }

        if let Some(mt) = mtime_sec {
            self.nodes[idx].mtime_sec = mt;
        } else if changed_content {
            self.nodes[idx].mtime_sec = now_sec;
        }
        self.nodes[idx].ctime_sec = now_sec;
        if changed_content {
            let parent = self.nodes[idx].parent;
            self.mark_ancestors_dirty(parent);
        }
        self.getattr(ino, store)
    }

    /// Create (or optionally replace) a regular file inside `dir_ino`.
    pub fn create(
        &mut self,
        dir_ino: u64,
        name: &str,
        mode: u32,
        exclusive: bool,
        now_sec: u32,
        store: &impl ObjectStore<'a>,
    ) -> Result<(u64, VfsAttr), VfsError> {
        if self.opts.read_only {
            return Err(VfsError::ReadOnly);
        }
        validate_component(name)?;
        self.ensure_dir_loaded(dir_ino, store)?;
        let dir_idx = self.slot(dir_ino)?;

        let existing = match &self.nodes[dir_idx].body {
            NodeBody::Dir(d) => d
                .children
                .binary_search_by(|(n, _)| n.as_bytes().cmp(name.as_bytes()))
                .ok()
                .map(|pos| d.children[pos].1),
            _ => return Err(VfsError::NotDir),
        };

        if let Some(ex_ino) = existing {
            if exclusive {
                return Err(VfsError::Exist);
            }
            // UNCHECKED create on existing file truncates it to 0 in place (POSIX/NFSv3 semantics).
            let attr = self.setattr(ex_ino, Some(mode), Some(0), None, now_sec, store)?;
            return Ok((ex_ino, attr));
        }

        let norm = if mode & 0o111 != 0 { 0o100755 } else { 0o100644 };
        let child_ino = self.alloc_node(
            dir_ino,
            now_sec,
            NodeBody::File(FileState {
                base_oid: None,
                size_hint: Some(0),
                overlay_bytes: Some(Vec::new_in(self.alloc)),
                mode: norm,
                dirty: true,
            }),
        )?;

        if let NodeBody::Dir(ref mut d) = self.nodes[dir_idx].body {
            let pos = d
                .children
                .binary_search_by(|(n, _)| n.as_bytes().cmp(name.as_bytes()))
                .unwrap_err();
            d.children.insert_shift(pos, (String::from_str_in(name, self.alloc), child_ino));
            d.dir_dirty = true;
        }
        self.nodes[dir_idx].mtime_sec = now_sec;
        self.nodes[dir_idx].ctime_sec = now_sec;
        self.mark_ancestors_dirty(dir_ino);
        let attr = self.getattr(child_ino, store)?;
        Ok((child_ino, attr))
    }

    /// Create a new subdirectory inside `dir_ino`.
    pub fn mkdir(
        &mut self,
        dir_ino: u64,
        name: &str,
        now_sec: u32,
        store: &impl ObjectStore<'a>,
    ) -> Result<(u64, VfsAttr), VfsError> {
        if self.opts.read_only {
            return Err(VfsError::ReadOnly);
        }
        validate_component(name)?;
        self.ensure_dir_loaded(dir_ino, store)?;
        let dir_idx = self.slot(dir_ino)?;

        let insert_pos = match &self.nodes[dir_idx].body {
            NodeBody::Dir(d) => match d
                .children
                .binary_search_by(|(n, _)| n.as_bytes().cmp(name.as_bytes()))
            {
                Ok(_) => return Err(VfsError::Exist),
                Err(pos) => pos,
            },
            _ => return Err(VfsError::NotDir),
        };

        let child_ino = self.alloc_node(
            dir_ino,
            now_sec,
            NodeBody::Dir(DirState {
                base_tree: None,
                loaded: true,
                base_children: Vec::new_in(self.alloc),
                children: Vec::new_in(self.alloc),
                subtree_dirty: true,
                dir_dirty: true,
            }),
        )?;

        if let NodeBody::Dir(ref mut d) = self.nodes[dir_idx].body {
            d.children.insert_shift(insert_pos, (String::from_str_in(name, self.alloc), child_ino));
            d.dir_dirty = true;
        }
        self.nodes[dir_idx].mtime_sec = now_sec;
        self.nodes[dir_idx].ctime_sec = now_sec;
        self.mark_ancestors_dirty(dir_ino);
        let attr = self.getattr(child_ino, store)?;
        Ok((child_ino, attr))
    }

    /// Create a symbolic link inside `dir_ino`.
    pub fn symlink(
        &mut self,
        dir_ino: u64,
        name: &str,
        target: &[u8],
        now_sec: u32,
        store: &impl ObjectStore<'a>,
    ) -> Result<(u64, VfsAttr), VfsError> {
        if self.opts.read_only {
            return Err(VfsError::ReadOnly);
        }
        validate_component(name)?;
        self.ensure_dir_loaded(dir_ino, store)?;
        let dir_idx = self.slot(dir_ino)?;

        let insert_pos = match &self.nodes[dir_idx].body {
            NodeBody::Dir(d) => match d
                .children
                .binary_search_by(|(n, _)| n.as_bytes().cmp(name.as_bytes()))
            {
                Ok(_) => return Err(VfsError::Exist),
                Err(pos) => pos,
            },
            _ => return Err(VfsError::NotDir),
        };

        let mut tgt = Vec::with_capacity_in(target.len(), self.alloc);
        tgt.extend_from_slice(target);
        let child_ino = self.alloc_node(
            dir_ino,
            now_sec,
            NodeBody::Symlink(SymlinkState {
                base_oid: None,
                overlay_target: Some(tgt),
            }),
        )?;

        if let NodeBody::Dir(ref mut d) = self.nodes[dir_idx].body {
            d.children.insert_shift(insert_pos, (String::from_str_in(name, self.alloc), child_ino));
            d.dir_dirty = true;
        }
        self.nodes[dir_idx].mtime_sec = now_sec;
        self.nodes[dir_idx].ctime_sec = now_sec;
        self.mark_ancestors_dirty(dir_ino);
        let attr = self.getattr(child_ino, store)?;
        Ok((child_ino, attr))
    }

    /// Remove a file or symlink from `dir_ino`.
    pub fn remove(
        &mut self,
        dir_ino: u64,
        name: &str,
        now_sec: u32,
        store: &impl ObjectStore<'a>,
    ) -> Result<(), VfsError> {
        if self.opts.read_only {
            return Err(VfsError::ReadOnly);
        }
        validate_component(name)?;
        self.ensure_dir_loaded(dir_ino, store)?;
        let dir_idx = self.slot(dir_ino)?;

        let (pos, child_ino) = match &self.nodes[dir_idx].body {
            NodeBody::Dir(d) => match d
                .children
                .binary_search_by(|(n, _)| n.as_bytes().cmp(name.as_bytes()))
            {
                Ok(pos) => (pos, d.children[pos].1),
                Err(_) => return Err(VfsError::NoEnt),
            },
            _ => return Err(VfsError::NotDir),
        };

        let child_idx = self.slot(child_ino)?;
        if matches!(self.nodes[child_idx].body, NodeBody::Dir(_)) {
            return Err(VfsError::IsDir);
        }

        if let NodeBody::Dir(ref mut d) = self.nodes[dir_idx].body {
            d.children.remove_shift(pos);
            d.dir_dirty = true;
        }
        self.nodes[dir_idx].mtime_sec = now_sec;
        self.nodes[dir_idx].ctime_sec = now_sec;
        self.mark_ancestors_dirty(dir_ino);
        Ok(())
    }

    /// Remove an empty directory from `dir_ino`.
    pub fn rmdir(
        &mut self,
        dir_ino: u64,
        name: &str,
        now_sec: u32,
        store: &impl ObjectStore<'a>,
    ) -> Result<(), VfsError> {
        if self.opts.read_only {
            return Err(VfsError::ReadOnly);
        }
        validate_component(name)?;
        self.ensure_dir_loaded(dir_ino, store)?;
        let dir_idx = self.slot(dir_ino)?;

        let (pos, child_ino) = match &self.nodes[dir_idx].body {
            NodeBody::Dir(d) => match d
                .children
                .binary_search_by(|(n, _)| n.as_bytes().cmp(name.as_bytes()))
            {
                Ok(pos) => (pos, d.children[pos].1),
                Err(_) => return Err(VfsError::NoEnt),
            },
            _ => return Err(VfsError::NotDir),
        };

        self.ensure_dir_loaded(child_ino, store)?;
        let child_idx = self.slot(child_ino)?;
        match &self.nodes[child_idx].body {
            NodeBody::Dir(d) => {
                if !d.children.is_empty() {
                    return Err(VfsError::NotEmpty);
                }
            }
            _ => return Err(VfsError::NotDir),
        }

        if let NodeBody::Dir(ref mut d) = self.nodes[dir_idx].body {
            d.children.remove_shift(pos);
            d.dir_dirty = true;
        }
        self.nodes[dir_idx].mtime_sec = now_sec;
        self.nodes[dir_idx].ctime_sec = now_sec;
        self.mark_ancestors_dirty(dir_ino);
        Ok(())
    }

    /// Rename `from_dir/from_name` to `to_dir/to_name` (supports atomic overwrite of target file).
    pub fn rename(
        &mut self,
        from_dir: u64,
        from_name: &str,
        to_dir: u64,
        to_name: &str,
        now_sec: u32,
        store: &impl ObjectStore<'a>,
    ) -> Result<(), VfsError> {
        if self.opts.read_only {
            return Err(VfsError::ReadOnly);
        }
        validate_component(from_name)?;
        validate_component(to_name)?;
        if from_dir == to_dir && from_name == to_name {
            return Ok(());
        }
        self.ensure_dir_loaded(from_dir, store)?;
        self.ensure_dir_loaded(to_dir, store)?;

        let from_idx = self.slot(from_dir)?;
        let (src_pos, src_ino) = match &self.nodes[from_idx].body {
            NodeBody::Dir(d) => match d
                .children
                .binary_search_by(|(n, _)| n.as_bytes().cmp(from_name.as_bytes()))
            {
                Ok(pos) => (pos, d.children[pos].1),
                Err(_) => return Err(VfsError::NoEnt),
            },
            _ => return Err(VfsError::NotDir),
        };

        let src_is_dir = matches!(self.nodes[self.slot(src_ino)?].body, NodeBody::Dir(_));

        // Check destination overwrite rules.
        let to_idx = self.slot(to_dir)?;
        let dst_existing = match &self.nodes[to_idx].body {
            NodeBody::Dir(d) => d
                .children
                .binary_search_by(|(n, _)| n.as_bytes().cmp(to_name.as_bytes()))
                .ok()
                .map(|pos| (pos, d.children[pos].1)),
            _ => return Err(VfsError::NotDir),
        };

        if let Some((_, dst_ino)) = dst_existing {
            let dst_is_dir = matches!(self.nodes[self.slot(dst_ino)?].body, NodeBody::Dir(_));
            if src_is_dir && !dst_is_dir {
                return Err(VfsError::NotDir);
            }
            if !src_is_dir && dst_is_dir {
                return Err(VfsError::IsDir);
            }
            if dst_is_dir {
                self.ensure_dir_loaded(dst_ino, store)?;
                if let NodeBody::Dir(d) = &self.nodes[self.slot(dst_ino)?].body
                    && !d.children.is_empty()
                {
                    return Err(VfsError::NotEmpty);
                }
            }
        }

        // Remove from source directory.
        if let NodeBody::Dir(ref mut d) = self.nodes[from_idx].body {
            d.children.remove_shift(src_pos);
            d.dir_dirty = true;
        }

        // Insert or overwrite in destination directory.
        if let NodeBody::Dir(ref mut d) = self.nodes[to_idx].body {
            match d
                .children
                .binary_search_by(|(n, _)| n.as_bytes().cmp(to_name.as_bytes()))
            {
                Ok(pos) => d.children[pos].1 = src_ino,
                Err(pos) => {
                    d.children.insert_shift(pos, (String::from_str_in(to_name, self.alloc), src_ino));
                }
            }
            d.dir_dirty = true;
        }

        let src_node_idx = self.slot(src_ino)?;
        self.nodes[src_node_idx].parent = to_dir;
        self.nodes[src_node_idx].ctime_sec = now_sec;
        // Mark moved directory dirty so its sub-entries are accounted for in O(dirty) status.
        if let NodeBody::Dir(ref mut d) = self.nodes[src_node_idx].body {
            d.subtree_dirty = true;
        }

        self.nodes[from_idx].mtime_sec = now_sec;
        self.nodes[to_idx].mtime_sec = now_sec;
        self.mark_ancestors_dirty(from_dir);
        self.mark_ancestors_dirty(to_dir);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // O(dirty) Status Computation (never touches clean subtrees!)
    // -----------------------------------------------------------------------

    /// Compute exact working-tree status in **O(dirty)** time — never descends into clean or
    /// unhydrated subtrees!
    pub fn dirty_status(
        &self,
        untracked: UntrackedMode,
        store: &impl ObjectStore<'a>,
        alloc: Alloc<'a>,
    ) -> Result<(StatusCounts, AVec<'a, StatusEntry<'a>>), VfsError> {
        let mut counts = StatusCounts::default();
        let mut entries = Vec::new_in(alloc);
        self.collect_dir_status(ROOT_INO, "", 0, untracked, store, alloc, &mut counts, &mut entries)?;
        entries.sort_unstable_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
        Ok((counts, entries))
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_dir_status(
        &self,
        dir_ino: u64,
        prefix: &str,
        depth: u16,
        untracked: UntrackedMode,
        store: &impl ObjectStore<'a>,
        alloc: Alloc<'a>,
        counts: &mut StatusCounts,
        entries: &mut AVec<'a, StatusEntry<'a>>,
    ) -> Result<(), VfsError> {
        if depth > self.opts.limits.most_dir_depth {
            return Ok(());
        }
        let idx = self.slot(dir_ino)?;
        let NodeBody::Dir(d) = &self.nodes[idx].body else {
            return Ok(());
        };
        if !d.loaded || !d.subtree_dirty {
            return Ok(());
        }

        // Two-pointer merge walk over sorted `base_children` and sorted `children`!
        let mut bi = 0usize;
        let mut li = 0usize;
        while bi < d.base_children.len() || li < d.children.len() {
            let cmp = match (d.base_children.get(bi), d.children.get(li)) {
                (Some(b), Some((ln, _))) => b.name.as_bytes().cmp(ln.as_bytes()),
                (Some(_), None) => core::cmp::Ordering::Less,
                (None, Some(_)) => core::cmp::Ordering::Greater,
                (None, None) => break,
            };
            match cmp {
                core::cmp::Ordering::Less => {
                    // Present in base tree, deleted in overlay!
                    let b = &d.base_children[bi];
                    let path = join_path(prefix, b.name.as_str(), alloc);
                    if b.mode == 0o040000 {
                        self.collect_deleted_base_tree(
                            b.oid,
                            path.as_str(),
                            depth + 1,
                            store,
                            alloc,
                            counts,
                            entries,
                        )?;
                    } else {
                        counts.deleted = counts.deleted.saturating_add(1);
                        entries.push(StatusEntry {
                            path,
                            index: FileStatus::Unmodified,
                            worktree: FileStatus::Deleted,
                            unmerged: false,
                        });
                    }
                    bi += 1;
                }
                core::cmp::Ordering::Greater => {
                    // Newly added in overlay (untracked)!
                    let (ref lname, child_ino) = d.children[li];
                    if untracked != UntrackedMode::Off {
                        let path = join_path(prefix, lname.as_str(), alloc);
                        let c_idx = self.slot(child_ino)?;
                        match &self.nodes[c_idx].body {
                            NodeBody::Dir(_) => {
                                if untracked == UntrackedMode::Normal {
                                    if self.dir_has_any_file(child_ino, depth + 1, store)? {
                                        let mut dir_path =
                                            String::with_capacity_in(path.len() + 1, alloc);
                                        dir_path.push_str(path.as_str());
                                        dir_path.push('/');
                                        counts.untracked = counts.untracked.saturating_add(1);
                                        entries.push(StatusEntry {
                                            path: dir_path,
                                            index: FileStatus::Untracked,
                                            worktree: FileStatus::Untracked,
                                            unmerged: false,
                                        });
                                    }
                                } else {
                                    self.collect_all_untracked(
                                        child_ino,
                                        path.as_str(),
                                        depth + 1,
                                        store,
                                        alloc,
                                        counts,
                                        entries,
                                    )?;
                                }
                            }
                            _ => {
                                counts.untracked = counts.untracked.saturating_add(1);
                                entries.push(StatusEntry {
                                    path,
                                    index: FileStatus::Untracked,
                                    worktree: FileStatus::Untracked,
                                    unmerged: false,
                                });
                            }
                        }
                    }
                    li += 1;
                }
                core::cmp::Ordering::Equal => {
                    let b = &d.base_children[bi];
                    let child_ino = d.children[li].1;
                    let path = join_path(prefix, b.name.as_str(), alloc);
                    let c_idx = self.slot(child_ino)?;
                    let base_is_dir = b.mode == 0o040000;
                    let base_is_symlink = (b.mode & 0o170000) == 0o120000;
                    match &self.nodes[c_idx].body {
                        NodeBody::Dir(cd) if base_is_dir => {
                            if cd.subtree_dirty {
                                self.collect_dir_status(
                                    child_ino,
                                    path.as_str(),
                                    depth + 1,
                                    untracked,
                                    store,
                                    alloc,
                                    counts,
                                    entries,
                                )?;
                            }
                        }
                        NodeBody::File(f) if !base_is_dir && !base_is_symlink => {
                            let base_norm = if b.mode & 0o111 != 0 { 0o100755 } else { 0o100644 };
                            let modified = if f.mode != base_norm {
                                true
                            } else if let Some(ref bytes) = f.overlay_bytes {
                                Oid::of(ObjectKind::Blob, bytes) != b.oid
                            } else {
                                f.base_oid != Some(b.oid)
                            };
                            if modified {
                                counts.modified = counts.modified.saturating_add(1);
                                entries.push(StatusEntry {
                                    path,
                                    index: FileStatus::Unmodified,
                                    worktree: FileStatus::Modified,
                                    unmerged: false,
                                });
                            }
                        }
                        NodeBody::Symlink(s) if base_is_symlink => {
                            let modified = if let Some(ref tgt) = s.overlay_target {
                                Oid::of(ObjectKind::Blob, tgt) != b.oid
                            } else {
                                s.base_oid != Some(b.oid)
                            };
                            if modified {
                                counts.modified = counts.modified.saturating_add(1);
                                entries.push(StatusEntry {
                                    path,
                                    index: FileStatus::Unmodified,
                                    worktree: FileStatus::Modified,
                                    unmerged: false,
                                });
                            }
                        }
                        _ => {
                            // Kind changed (e.g. file <-> symlink or file <-> dir).
                            counts.typechanged = counts.typechanged.saturating_add(1);
                            entries.push(StatusEntry {
                                path,
                                index: FileStatus::Unmodified,
                                worktree: FileStatus::TypeChanged,
                                unmerged: false,
                            });
                        }
                    }
                    bi += 1;
                    li += 1;
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_deleted_base_tree(
        &self,
        tree_oid: Oid,
        prefix: &str,
        depth: u16,
        store: &impl ObjectStore<'a>,
        alloc: Alloc<'a>,
        counts: &mut StatusCounts,
        entries: &mut AVec<'a, StatusEntry<'a>>,
    ) -> Result<(), VfsError> {
        if depth > self.opts.limits.most_dir_depth {
            return Ok(());
        }
        let mut raw = Vec::new_in(alloc);
        let Some(ObjectKind::Tree) = store.read_object(&tree_oid, &mut raw)? else {
            return Ok(());
        };
        let mut iter = TreeIter::new(tree_oid, &raw);
        while let Some(te) = iter.next_entry()? {
            let Ok(name) = core::str::from_utf8(te.name) else {
                continue;
            };
            let path = join_path(prefix, name, alloc);
            if te.is_dir() {
                self.collect_deleted_base_tree(
                    te.oid,
                    path.as_str(),
                    depth + 1,
                    store,
                    alloc,
                    counts,
                    entries,
                )?;
            } else {
                counts.deleted = counts.deleted.saturating_add(1);
                entries.push(StatusEntry {
                    path,
                    index: FileStatus::Unmodified,
                    worktree: FileStatus::Deleted,
                    unmerged: false,
                });
            }
        }
        Ok(())
    }

    fn dir_has_any_file(
        &self,
        dir_ino: u64,
        depth: u16,
        store: &impl ObjectStore<'a>,
    ) -> Result<bool, VfsError> {
        if depth > self.opts.limits.most_dir_depth {
            return Ok(false);
        }
        let idx = self.slot(dir_ino)?;
        let NodeBody::Dir(d) = &self.nodes[idx].body else {
            return Ok(true);
        };
        if !d.loaded {
            // An unhydrated base tree (e.g. a renamed base directory) is non-empty in Git.
            return Ok(d.base_tree.is_some());
        }
        for (_, child_ino) in d.children.iter() {
            let c_idx = self.slot(*child_ino)?;
            match &self.nodes[c_idx].body {
                NodeBody::Dir(_) => {
                    if self.dir_has_any_file(*child_ino, depth + 1, store)? {
                        return Ok(true);
                    }
                }
                _ => return Ok(true),
            }
        }
        Ok(false)
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_all_untracked(
        &self,
        dir_ino: u64,
        prefix: &str,
        depth: u16,
        store: &impl ObjectStore<'a>,
        alloc: Alloc<'a>,
        counts: &mut StatusCounts,
        entries: &mut AVec<'a, StatusEntry<'a>>,
    ) -> Result<(), VfsError> {
        if depth > self.opts.limits.most_dir_depth {
            return Ok(());
        }
        let idx = self.slot(dir_ino)?;
        let NodeBody::Dir(d) = &self.nodes[idx].body else {
            return Ok(());
        };
        if !d.loaded {
            if let Some(tree_oid) = d.base_tree {
                self.collect_base_tree_as_untracked(
                    tree_oid,
                    prefix,
                    depth,
                    store,
                    alloc,
                    counts,
                    entries,
                )?;
            }
            return Ok(());
        }
        for (name, child_ino) in d.children.iter() {
            let path = join_path(prefix, name.as_str(), alloc);
            let c_idx = self.slot(*child_ino)?;
            match &self.nodes[c_idx].body {
                NodeBody::Dir(_) => {
                    self.collect_all_untracked(
                        *child_ino,
                        path.as_str(),
                        depth + 1,
                        store,
                        alloc,
                        counts,
                        entries,
                    )?;
                }
                _ => {
                    counts.untracked = counts.untracked.saturating_add(1);
                    entries.push(StatusEntry {
                        path,
                        index: FileStatus::Untracked,
                        worktree: FileStatus::Untracked,
                        unmerged: false,
                    });
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_base_tree_as_untracked(
        &self,
        tree_oid: Oid,
        prefix: &str,
        depth: u16,
        store: &impl ObjectStore<'a>,
        alloc: Alloc<'a>,
        counts: &mut StatusCounts,
        entries: &mut AVec<'a, StatusEntry<'a>>,
    ) -> Result<(), VfsError> {
        if depth > self.opts.limits.most_dir_depth {
            return Ok(());
        }
        let mut raw = Vec::new_in(alloc);
        let Some(ObjectKind::Tree) = store.read_object(&tree_oid, &mut raw)? else {
            return Ok(());
        };
        let mut iter = TreeIter::new(tree_oid, &raw);
        while let Some(te) = iter.next_entry()? {
            let Ok(name) = core::str::from_utf8(te.name) else {
                continue;
            };
            let path = join_path(prefix, name, alloc);
            if te.is_dir() {
                self.collect_base_tree_as_untracked(
                    te.oid,
                    path.as_str(),
                    depth + 1,
                    store,
                    alloc,
                    counts,
                    entries,
                )?;
            } else {
                counts.untracked = counts.untracked.saturating_add(1);
                entries.push(StatusEntry {
                    path,
                    index: FileStatus::Untracked,
                    worktree: FileStatus::Untracked,
                    unmerged: false,
                });
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // O(dirty) Git Tree Materialization (writes ONLY changed blobs & trees!)
    // -----------------------------------------------------------------------

    /// Materialize the current VFS state into a Git tree object in `store` in **O(dirty)** time
    /// (clean/unhydrated subtrees reuse their existing `base_tree` OID with 0 reads/writes!).
    pub fn write_tree(&self, store: &mut impl ObjectStore<'a>) -> Result<Oid, VfsError> {
        self.write_dir_tree(ROOT_INO, 0, store)
    }

    fn write_dir_tree(
        &self,
        dir_ino: u64,
        depth: u16,
        store: &mut impl ObjectStore<'a>,
    ) -> Result<Oid, VfsError> {
        if depth > self.opts.limits.most_dir_depth {
            return Err(VfsError::NameTooLong);
        }
        let idx = self.slot(dir_ino)?;
        let NodeBody::Dir(d) = &self.nodes[idx].body else {
            return Err(VfsError::NotDir);
        };
        if !d.subtree_dirty
            && let Some(oid) = d.base_tree
        {
            return Ok(oid);
        }
        let mut tree_entries: AVec<'a, (AString<'a>, u32, Oid)> =
            Vec::with_capacity_in(d.children.len(), self.alloc);
        for (name, child_ino) in d.children.iter() {
            let c_idx = self.slot(*child_ino)?;
            match &self.nodes[c_idx].body {
                NodeBody::Dir(cd) => {
                    // Git omits empty directories from tree objects.
                    if cd.loaded && cd.children.is_empty() {
                        continue;
                    }
                    let sub_oid = self.write_dir_tree(*child_ino, depth + 1, store)?;
                    // Skip empty subtrees created after child deletion.
                    if sub_oid == Oid::of(ObjectKind::Tree, &[]) {
                        continue;
                    }
                    tree_entries.push((
                        String::from_str_in(name.as_str(), self.alloc),
                        0o040000,
                        sub_oid,
                    ));
                }
                NodeBody::File(f) => {
                    let oid = if let Some(ref bytes) = f.overlay_bytes {
                        store.write_object(ObjectKind::Blob, bytes)?
                    } else if let Some(oid) = f.base_oid {
                        oid
                    } else {
                        store.write_object(ObjectKind::Blob, &[])?
                    };
                    tree_entries.push((
                        String::from_str_in(name.as_str(), self.alloc),
                        f.mode,
                        oid,
                    ));
                }
                NodeBody::Symlink(s) => {
                    let oid = if let Some(ref tgt) = s.overlay_target {
                        store.write_object(ObjectKind::Blob, tgt)?
                    } else if let Some(oid) = s.base_oid {
                        oid
                    } else {
                        store.write_object(ObjectKind::Blob, &[])?
                    };
                    tree_entries.push((
                        String::from_str_in(name.as_str(), self.alloc),
                        0o120000,
                        oid,
                    ));
                }
            }
        }
        let mut inputs: AVec<'a, TreeBuildEntry<'_>> =
            Vec::with_capacity_in(tree_entries.len(), self.alloc);
        for (name, mode, oid) in tree_entries.iter() {
            inputs.push(TreeBuildEntry { mode: *mode, name: name.as_bytes(), oid: *oid });
        }
        let mut body = Vec::new_in(self.alloc);
        encode_tree(&inputs, self.alloc, &mut body)?;
        Ok(store.write_object(ObjectKind::Tree, &body)?)
    }
}

fn validate_component(name: &str) -> Result<(), VfsError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(VfsError::Inval);
    }
    if name.len() > 255 {
        return Err(VfsError::NameTooLong);
    }
    Ok(())
}

fn join_path<'a>(prefix: &str, name: &str, alloc: Alloc<'a>) -> AString<'a> {
    if prefix.is_empty() {
        return String::from_str_in(name, alloc);
    }
    let mut s = String::with_capacity_in(prefix.len() + 1 + name.len(), alloc);
    s.push_str(prefix);
    s.push('/');
    s.push_str(name);
    s
}

// ---------------------------------------------------------------------------
// `BGOV` v1 Overlay Status Manifest (O(dirty) status file shared with `bgit status`)
// ---------------------------------------------------------------------------

const BGOV_MAGIC: &[u8; 4] = b"BGOV";
const BGOV_VERSION: u32 = 1;

/// Decoded `BGOV` v1 VFS overlay status manifest.
#[derive(PartialEq, Eq, Debug)]
pub struct OverlayStatusManifest<'a> {
    pub generation: u64,
    pub branch: Option<AString<'a>>,
    pub head_oid: Option<Oid>,
    pub counts: StatusCounts,
    pub entries: AVec<'a, StatusEntry<'a>>,
}

/// Encode a `BGOV` v1 overlay status manifest protected by a trailing CRC-32 checksum.
pub fn encode_overlay_status(
    generation: u64,
    branch: Option<&str>,
    head_oid: Option<Oid>,
    counts: StatusCounts,
    entries: &[StatusEntry<'_>],
    out: &mut AVec<'_, u8>,
) {
    out.clear();
    out.extend_from_slice(BGOV_MAGIC);
    out.extend_from_slice(&BGOV_VERSION.to_le_bytes());
    out.extend_from_slice(&generation.to_le_bytes());

    match head_oid {
        Some(oid) => {
            out.push(1);
            out.extend_from_slice(&oid.0);
        }
        None => {
            out.push(0);
            out.extend_from_slice(&[0u8; 20]);
        }
    }

    match branch {
        Some(b) => {
            out.push(1);
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b.as_bytes());
        }
        None => {
            out.push(0);
            out.extend_from_slice(&0u32.to_le_bytes());
        }
    }

    out.extend_from_slice(&counts.modified.to_le_bytes());
    out.extend_from_slice(&counts.deleted.to_le_bytes());
    out.extend_from_slice(&counts.typechanged.to_le_bytes());
    out.extend_from_slice(&counts.untracked.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());

    for e in entries {
        out.push(encode_file_status(e.index));
        out.push(encode_file_status(e.worktree));
        out.push(u8::from(e.unmerged));
        out.extend_from_slice(&(e.path.len() as u32).to_le_bytes());
        out.extend_from_slice(e.path.as_bytes());
    }

    let crc = blaze_deflate::crc32(out);
    out.extend_from_slice(&crc.to_le_bytes());
}

/// Decode and CRC-verify a `BGOV` v1 overlay status manifest.
pub fn decode_overlay_status<'a>(
    bytes: &[u8],
    alloc: Alloc<'a>,
) -> Result<OverlayStatusManifest<'a>, GitError> {
    let mal = |at: usize| GitError { file: GitFile::Slab, at: at as u64, kind: ErrorKind::Malformed };
    if bytes.len() < 4 + 4 + 8 + 21 + 5 + 20 + 4 {
        return Err(mal(0));
    }
    if &bytes[0..4] != BGOV_MAGIC {
        return Err(mal(0));
    }
    let ver = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if ver != BGOV_VERSION {
        return Err(GitError {
            file: GitFile::Slab,
            at: 4,
            kind: ErrorKind::UnsupportedVersion(ver),
        });
    }
    let body_end = bytes.len() - 4;
    let stored_crc = u32::from_le_bytes(bytes[body_end..].try_into().unwrap());
    if blaze_deflate::crc32(&bytes[..body_end]) != stored_crc {
        return Err(GitError {
            file: GitFile::Slab,
            at: body_end as u64,
            kind: ErrorKind::ChecksumMismatch,
        });
    }

    let mut at = 8usize;
    let generation = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    at += 8;

    let has_oid = bytes[at] != 0;
    at += 1;
    let mut oid_raw = [0u8; 20];
    oid_raw.copy_from_slice(&bytes[at..at + 20]);
    at += 20;
    let head_oid = has_oid.then_some(Oid(oid_raw));

    let has_branch = bytes[at] != 0;
    at += 1;
    let branch_len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
    at += 4;
    if at + branch_len > body_end {
        return Err(mal(at));
    }
    let branch = if has_branch {
        let s = core::str::from_utf8(&bytes[at..at + branch_len]).map_err(|_| mal(at))?;
        Some(String::from_str_in(s, alloc))
    } else {
        None
    };
    at += branch_len;

    if at + 20 > body_end {
        return Err(mal(at));
    }
    let mut counts = StatusCounts::default();
    counts.modified = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    counts.deleted = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap());
    counts.typechanged = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap());
    counts.untracked = u32::from_le_bytes(bytes[at + 12..at + 16].try_into().unwrap());
    let entry_count = u32::from_le_bytes(bytes[at + 16..at + 20].try_into().unwrap()) as usize;
    at += 20;

    let mut entries = Vec::with_capacity_in(entry_count.min(1 << 20), alloc);
    for _ in 0..entry_count {
        if at + 7 > body_end {
            return Err(mal(at));
        }
        let index = decode_file_status(bytes[at]).ok_or_else(|| mal(at))?;
        let worktree = decode_file_status(bytes[at + 1]).ok_or_else(|| mal(at + 1))?;
        let unmerged = bytes[at + 2] != 0;
        let plen = u32::from_le_bytes(bytes[at + 3..at + 7].try_into().unwrap()) as usize;
        at += 7;
        if at + plen > body_end {
            return Err(mal(at));
        }
        let p = core::str::from_utf8(&bytes[at..at + plen]).map_err(|_| mal(at))?;
        at += plen;
        entries.push(StatusEntry { path: String::from_str_in(p, alloc), index, worktree, unmerged });
    }

    Ok(OverlayStatusManifest { generation, branch, head_oid, counts, entries })
}

fn encode_file_status(s: FileStatus) -> u8 {
    match s {
        FileStatus::Unmodified => 0,
        FileStatus::Added => 1,
        FileStatus::Modified => 2,
        FileStatus::Deleted => 3,
        FileStatus::TypeChanged => 4,
        FileStatus::Untracked => 5,
        FileStatus::Unmerged => 6,
    }
}

fn decode_file_status(v: u8) -> Option<FileStatus> {
    match v {
        0 => Some(FileStatus::Unmodified),
        1 => Some(FileStatus::Added),
        2 => Some(FileStatus::Modified),
        3 => Some(FileStatus::Deleted),
        4 => Some(FileStatus::TypeChanged),
        5 => Some(FileStatus::Untracked),
        6 => Some(FileStatus::Unmerged),
        _ => None,
    }
}
