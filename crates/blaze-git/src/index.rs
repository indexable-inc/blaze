//! Zero-copy `.git/index` (`DIRC` versions 2, 3, and 4) and recursive `TREE` cache-tree parser.
//!
//! For index v2 and v3, every entry path is stored as a `(start, len)` span into the mapped
//! index buffer: zero per-entry string allocations, zero path copies. For v4 (prefix-compressed
//! paths), all reconstructed paths are packed into one contiguous byte arena in the caller's
//! allocator.
//!
//! The `TREE` extension is parsed into a flat node table so staged-change diffing against `HEAD`
//! can prune any unchanged directory subtree in O(1) without reading its tree object.

#![blaze::sans_io]

use blaze_git_fetch::Oid;
use blaze_memchr::memchr;
use blaze_own::{AVec, Alloc, Vec, planted::Planted};

use crate::{ErrorKind, GitError, GitFile, GitStatusPlant, Limits};

/// One entry in `.git/index`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IndexEntry {
    pub path_start: u32,
    pub path_len: u32,
    pub ctime_sec: u32,
    pub ctime_nsec: u32,
    pub mtime_sec: u32,
    pub mtime_nsec: u32,
    pub dev: u32,
    pub ino: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
    pub oid: Oid,
    /// Stage 0 = normal, 1 = base, 2 = ours, 3 = theirs.
    pub stage: u8,
    pub assume_valid: bool,
    pub skip_worktree: bool,
    pub intent_to_add: bool,
}

impl IndexEntry {
    /// File type bits (`mode & 0o170000`).
    #[inline]
    pub const fn mode_type(self) -> u32 {
        self.mode & 0o170000
    }

    #[inline]
    pub const fn is_regular(self) -> bool {
        self.mode_type() == 0o100000
    }

    #[inline]
    pub const fn is_symlink(self) -> bool {
        self.mode_type() == 0o120000
    }

    #[inline]
    pub const fn is_gitlink(self) -> bool {
        self.mode_type() == 0o160000
    }

    #[inline]
    pub const fn is_sparse_dir(self) -> bool {
        self.mode_type() == 0o040000
    }

    #[inline]
    pub const fn is_executable(self) -> bool {
        self.mode & 0o111 != 0
    }
}

/// One node in the `.git/index` `TREE` cache-tree extension.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CacheTreeNode {
    /// Byte span of this node's path component in the original index bytes (empty for root).
    pub name_start: u32,
    pub name_len: u32,
    /// Number of index entries covered by this subtree, or `-1` if invalidated.
    pub entry_count: i32,
    /// Tree OID when `entry_count >= 0`.
    pub oid: Option<Oid>,
    /// First child node index in [`CacheTree::nodes`], or `u32::MAX` if none.
    pub first_child: u32,
    /// Next sibling node index in [`CacheTree::nodes`], or `u32::MAX` if none.
    pub next_sibling: u32,
}

/// Parsed recursive `TREE` cache-tree extension.
pub struct CacheTree<'a> {
    pub nodes: AVec<'a, CacheTreeNode>,
}

impl<'a> CacheTree<'a> {
    /// Root tree OID when the root cache-tree entry is valid.
    pub fn root_oid(&self) -> Option<Oid> {
        self.nodes.first().and_then(|root| root.oid)
    }

    /// Look up a direct child of `parent_idx` by directory component name `name`.
    pub fn find_child<'b>(&self, index_bytes: &'b [u8], parent_idx: u32, name: &[u8]) -> Option<u32> {
        let mut child = self.nodes.get(parent_idx as usize)?.first_child;
        while child != u32::MAX {
            let node = self.nodes[child as usize];
            let node_name = &index_bytes[node.name_start as usize..(node.name_start + node.name_len) as usize];
            match node_name.cmp(name) {
                core::cmp::Ordering::Equal => return Some(child),
                core::cmp::Ordering::Greater => return None,
                core::cmp::Ordering::Less => child = node.next_sibling,
            }
        }
        None
    }

    /// Name slice of `node_idx` in `index_bytes`.
    #[inline]
    pub fn name<'b>(&self, index_bytes: &'b [u8], node_idx: u32) -> &'b [u8] {
        let node = self.nodes[node_idx as usize];
        &index_bytes[node.name_start as usize..(node.name_start + node.name_len) as usize]
    }
}

/// Parsed `.git/index` borrowing v2/v3 paths from the input buffer.
pub struct ParsedIndex<'i, 'a> {
    /// Original index bytes (used to slice v2/v3 paths and cache-tree names).
    raw: &'i [u8],
    /// Flat path buffer populated only for v4 prefix-compressed indexes.
    v4_paths: AVec<'a, u8>,
    is_v4: bool,
    /// Normal (stage 0) entries in sorted byte order.
    pub entries: AVec<'a, IndexEntry>,
    /// Number of distinct paths that have at least one unmerged (stage 1..=3) entry.
    pub conflicted_count: u32,
    /// Optional recursive `TREE` cache-tree extension.
    pub cache_tree: Option<CacheTree<'a>>,
    pub version: u32,
}

impl<'i, 'a> ParsedIndex<'i, 'a> {
    /// Path bytes of `entry`.
    #[inline]
    pub fn path_bytes<'s>(&'s self, entry: &IndexEntry) -> &'s [u8] {
        let start = entry.path_start as usize;
        let end = start + entry.path_len as usize;
        if self.is_v4 { &self.v4_paths[start..end] } else { &self.raw[start..end] }
    }

    /// Path of `entry` as UTF-8 (`None` if non-UTF-8 raw bytes).
    #[inline]
    pub fn path_str<'s>(&'s self, entry: &IndexEntry) -> Option<&'s str> {
        core::str::from_utf8(self.path_bytes(entry)).ok()
    }

    /// Binary search for exact stage-0 entry `path`.
    pub fn find_entry(&self, path: &[u8]) -> Option<usize> {
        self.entries.binary_search_by(|entry| self.path_bytes(entry).cmp(path)).ok()
    }

    /// Range of stage-0 entries strictly inside directory `dir` (`""` = all entries).
    pub fn entries_in_dir_range(&self, dir: &[u8]) -> core::ops::Range<usize> {
        if dir.is_empty() {
            return 0..self.entries.len();
        }
        let start = self.entries.partition_point(|entry| {
            let p = self.path_bytes(entry);
            let n = p.len().min(dir.len());
            match p[..n].cmp(&dir[..n]) {
                core::cmp::Ordering::Less => true,
                core::cmp::Ordering::Greater => false,
                core::cmp::Ordering::Equal if p.len() <= dir.len() => true,
                core::cmp::Ordering::Equal => p[dir.len()] < b'/',
            }
        });
        let end = start
            + self.entries[start..].partition_point(|entry| {
                let p = self.path_bytes(entry);
                p.len() > dir.len() && p.starts_with(dir) && p[dir.len()] == b'/'
            });
        start..end
    }

    /// Whether any stage-0 entry lies strictly under `dir`.
    pub fn has_tracked_below(&self, dir: &[u8]) -> bool {
        !self.entries_in_dir_range(dir).is_empty()
    }

    /// Original raw bytes (for `CacheTree::find_child`).
    #[inline]
    pub fn raw_bytes(&self) -> &'i [u8] {
        self.raw
    }

    /// Borrow a lightweight O(1) view for lookup/diffing without re-parsing.
    #[inline]
    pub fn view(&self) -> IndexView<'_> {
        IndexView {
            raw: self.raw,
            v4_paths: &self.v4_paths,
            is_v4: self.is_v4,
            entries: &self.entries,
            cache_tree: self.cache_tree.as_ref().map(|ct| ct.nodes.as_slice()),
        }
    }
}

/// Zero-cost borrowed view over an already-parsed index (4 slices + 1 bool, zero allocations).
#[derive(Clone, Copy)]
pub struct IndexView<'a> {
    pub raw: &'a [u8],
    pub v4_paths: &'a [u8],
    pub is_v4: bool,
    pub entries: &'a [IndexEntry],
    pub cache_tree: Option<&'a [CacheTreeNode]>,
}

impl<'a> IndexView<'a> {
    #[inline]
    pub fn path_bytes(&self, entry: &IndexEntry) -> &'a [u8] {
        let start = entry.path_start as usize;
        let end = start + entry.path_len as usize;
        if self.is_v4 { &self.v4_paths[start..end] } else { &self.raw[start..end] }
    }

    #[inline]
    pub fn path_str(&self, entry: &IndexEntry) -> Option<&'a str> {
        core::str::from_utf8(self.path_bytes(entry)).ok()
    }

    pub fn find_entry(&self, path: &[u8]) -> Option<usize> {
        self.entries.binary_search_by(|entry| self.path_bytes(entry).cmp(path)).ok()
    }

    pub fn has_tracked_below(&self, dir: &[u8]) -> bool {
        if dir.is_empty() {
            return !self.entries.is_empty();
        }
        let start = self.entries.partition_point(|entry| {
            let p = self.path_bytes(entry);
            let n = p.len().min(dir.len());
            match p[..n].cmp(&dir[..n]) {
                core::cmp::Ordering::Less => true,
                core::cmp::Ordering::Greater => false,
                core::cmp::Ordering::Equal if p.len() <= dir.len() => true,
                core::cmp::Ordering::Equal => p[dir.len()] < b'/',
            }
        });
        self.entries.get(start).is_some_and(|entry| {
            let p = self.path_bytes(entry);
            p.len() > dir.len() && p.starts_with(dir) && p[dir.len()] == b'/'
        })
    }

    pub fn cache_tree_root_oid(&self) -> Option<Oid> {
        self.cache_tree?.first()?.oid
    }

    pub fn cache_tree_find_child(&self, parent_idx: u32, name: &[u8]) -> Option<u32> {
        let nodes = self.cache_tree?;
        let mut child = nodes.get(parent_idx as usize)?.first_child;
        while child != u32::MAX {
            let node = nodes[child as usize];
            let node_name = &self.raw[node.name_start as usize..(node.name_start + node.name_len) as usize];
            match node_name.cmp(name) {
                core::cmp::Ordering::Equal => return Some(child),
                core::cmp::Ordering::Greater => return None,
                core::cmp::Ordering::Less => child = node.next_sibling,
            }
        }
        None
    }
}

/// Owned parsed index parts (keeps raw bytes + parsed tables together so `.git/index` is parsed
/// once per status run).
pub struct OwnedIndex<'a> {
    pub raw: AVec<'a, u8>,
    pub v4_paths: AVec<'a, u8>,
    pub is_v4: bool,
    pub entries: AVec<'a, IndexEntry>,
    pub conflicted_count: u32,
    pub cache_tree: Option<CacheTree<'a>>,
    pub version: u32,
}

impl<'a> OwnedIndex<'a> {
    pub fn parse(
        raw: AVec<'a, u8>,
        limits: Limits,
        alloc: Alloc<'a>,
        plant: impl Planted<GitStatusPlant>,
    ) -> Result<OwnedIndex<'a>, GitError> {
        let parsed = parse_index(&raw, limits, alloc, plant)?;
        let ParsedIndex { v4_paths, is_v4, entries, conflicted_count, cache_tree, version, .. } =
            parsed;
        Ok(OwnedIndex { raw, v4_paths, is_v4, entries, conflicted_count, cache_tree, version })
    }

    #[inline]
    pub fn view(&self) -> IndexView<'_> {
        IndexView {
            raw: &self.raw,
            v4_paths: &self.v4_paths,
            is_v4: self.is_v4,
            entries: &self.entries,
            cache_tree: self.cache_tree.as_ref().map(|ct| ct.nodes.as_slice()),
        }
    }
}

#[inline]
fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

#[inline]
fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    let slice = bytes.get(at..at + 2)?;
    Some(u16::from_be_bytes([slice[0], slice[1]]))
}

#[inline]
fn err(at: usize, kind: ErrorKind) -> GitError {
    GitError { file: GitFile::Index, at: at as u64, kind }
}

/// Validate a path as safe relative worktree path (no `.`, `..`, empty component, or `.git`).
fn path_safe(path: &[u8]) -> bool {
    if path.is_empty() || path[0] == b'/' {
        return false;
    }
    // Sparse directory entries may have a single trailing '/'.
    let trimmed = path.strip_suffix(b"/").unwrap_or(path);
    if trimmed.is_empty() {
        return false;
    }
    for part in trimmed.split(|&b| b == b'/') {
        if part.is_empty() || part == b"." || part == b".." || part.eq_ignore_ascii_case(b".git") {
            return false;
        }
    }
    true
}

/// Parse `.git/index` (`DIRC` v2, v3, or v4) in place into `alloc`.
pub fn parse_index<'i, 'a>(
    bytes: &'i [u8],
    limits: Limits,
    alloc: Alloc<'a>,
    plant: impl Planted<GitStatusPlant>,
) -> Result<ParsedIndex<'i, 'a>, GitError> {
    if bytes.len() < 12 + 20 || &bytes[..4] != b"DIRC" {
        return Err(err(0, ErrorKind::Malformed));
    }
    let version = be32(bytes, 4).ok_or_else(|| err(4, ErrorKind::Malformed))?;
    if !(2..=4).contains(&version) {
        return Err(err(4, ErrorKind::UnsupportedVersion(version)));
    }
    let count = be32(bytes, 8).ok_or_else(|| err(8, ErrorKind::Malformed))?;
    if count > limits.most_index_entries {
        return Err(err(8, ErrorKind::TooManyEntries { count, most: limits.most_index_entries }));
    }

    let end = bytes.len() - 20;
    let mut entries = Vec::with_capacity_in(count as usize, alloc);
    let is_v4 = version == 4;
    let mut v4_paths: AVec<'a, u8> = Vec::new_in(alloc);
    let mut prev_v4: AVec<'a, u8> = Vec::new_in(alloc);

    let mut conflicted_count: u32 = 0;
    let mut last_conflict_start: usize = 0;
    let mut last_conflict_len: usize = 0;
    let mut has_last_conflict = false;

    let mut at: usize = 12;
    for _ in 0..count {
        let start = at;
        if start + 62 > end {
            return Err(err(start, ErrorKind::Malformed));
        }
        let ctime_sec = be32(bytes, start).unwrap();
        let ctime_nsec = be32(bytes, start + 4).unwrap();
        let mtime_sec = be32(bytes, start + 8).unwrap();
        let mtime_nsec = be32(bytes, start + 12).unwrap();
        let dev = be32(bytes, start + 16).unwrap();
        let ino = be32(bytes, start + 20).unwrap();
        let mode = be32(bytes, start + 24).unwrap();
        let uid = be32(bytes, start + 28).unwrap();
        let gid = be32(bytes, start + 32).unwrap();
        let size = be32(bytes, start + 36).unwrap();
        let oid = Oid(bytes[start + 40..start + 60].try_into().expect("20 bytes"));
        let flags = be16(bytes, start + 60).unwrap();

        let assume_valid = flags & 0x8000 != 0;
        let has_extended = flags & 0x4000 != 0;
        let stage = ((flags >> 12) & 0x03) as u8;
        let name_len_hint = usize::from(flags & 0x0fff);

        at = start + 62;
        let mut skip_worktree = false;
        let mut intent_to_add = false;
        if has_extended {
            if version < 3 || at + 2 > end {
                return Err(err(start, ErrorKind::Malformed));
            }
            let ext = be16(bytes, at).unwrap();
            skip_worktree = ext & 0x4000 != 0;
            intent_to_add = ext & 0x2000 != 0;
            at += 2;
        }

        let (path_start, path_len) = if is_v4 {
            let mut pos = at;
            let mut byte = *bytes.get(pos).filter(|_| pos < end).ok_or_else(|| err(pos, ErrorKind::Malformed))?;
            pos += 1;
            let mut drop = u64::from(byte & 0x7f);
            while byte & 0x80 != 0 {
                byte = *bytes.get(pos).filter(|_| pos < end).ok_or_else(|| err(pos, ErrorKind::Malformed))?;
                pos += 1;
                drop = drop
                    .checked_add(1)
                    .and_then(|d| d.checked_mul(128))
                    .ok_or_else(|| err(pos, ErrorKind::Malformed))?
                    | u64::from(byte & 0x7f);
            }
            if drop > prev_v4.len() as u64 {
                return Err(err(at, ErrorKind::Malformed));
            }
            prev_v4.truncate(prev_v4.len() - drop as usize);
            let nul = memchr(0, &bytes[pos..end]).ok_or_else(|| err(pos, ErrorKind::Malformed))?;
            prev_v4.extend_from_slice(&bytes[pos..pos + nul]);
            at = pos + nul + 1;

            let p_start = v4_paths.len();
            v4_paths.extend_from_slice(&prev_v4);
            (p_start, prev_v4.len())
        } else {
            // Fast path: in v2/v3, `name_len_hint < 0x0fff` gives exact path length!
            let nul_offset = if name_len_hint < 0x0fff && at + name_len_hint < end && bytes[at + name_len_hint] == 0 {
                name_len_hint
            } else {
                memchr(0, &bytes[at..end]).ok_or_else(|| err(at, ErrorKind::Malformed))?
            };
            let p_start = at;
            let p_len = nul_offset;
            let entry_len = (at + nul_offset) - start;
            // Git pads v2/v3 entries with 1..=8 NUL bytes to a multiple of 8 bytes.
            at = start + ((entry_len + 8) & !7);
            if at > end {
                return Err(err(start, ErrorKind::Malformed));
            }
            (p_start, p_len)
        };

        let path_slice: &[u8] = if is_v4 {
            &v4_paths[path_start..path_start + path_len]
        } else {
            &bytes[path_start..path_start + path_len]
        };
        if !path_safe(path_slice) {
            return Err(err(start, ErrorKind::Malformed));
        }

        if stage > 0 {
            if !plant.has(GitStatusPlant::SkipConflictCount) {
                let same_as_last = has_last_conflict && {
                    let prev: &[u8] = if is_v4 {
                        &v4_paths[last_conflict_start..last_conflict_start + last_conflict_len]
                    } else {
                        &bytes[last_conflict_start..last_conflict_start + last_conflict_len]
                    };
                    prev == path_slice
                };
                if !same_as_last {
                    conflicted_count = conflicted_count.saturating_add(1);
                    last_conflict_start = path_start;
                    last_conflict_len = path_len;
                    has_last_conflict = true;
                }
            }
            continue;
        }

        entries.push(IndexEntry {
            path_start: path_start as u32,
            path_len: path_len as u32,
            ctime_sec,
            ctime_nsec,
            mtime_sec,
            mtime_nsec,
            dev,
            ino,
            mode,
            uid,
            gid,
            size,
            oid,
            stage,
            assume_valid,
            skip_worktree,
            intent_to_add,
        });
    }

    // Parse extensions (TREE cache-tree; refuse split index `link`).
    let mut cache_tree = None;
    while at < end {
        if at + 8 > end {
            return Err(err(at, ErrorKind::Malformed));
        }
        let sig = &bytes[at..at + 4];
        let ext_len = be32(bytes, at + 4).unwrap() as usize;
        let data_start = at + 8;
        let Some(data_end) = data_start.checked_add(ext_len) else {
            return Err(err(at, ErrorKind::Malformed));
        };
        if data_end > end {
            return Err(err(at, ErrorKind::Malformed));
        }
        match sig {
            b"link" => return Err(err(at, ErrorKind::SplitIndexUnsupported)),
            b"TREE" => {
                cache_tree = Some(parse_cache_tree(bytes, data_start, data_end, limits, alloc)?);
            }
            _ => {}
        }
        at = data_end;
    }

    Ok(ParsedIndex {
        raw: bytes,
        v4_paths,
        is_v4,
        entries,
        conflicted_count,
        cache_tree,
        version,
    })
}

/// Parse the recursive `TREE` extension iteratively using an explicit stack (bounded depth).
fn parse_cache_tree<'a>(
    bytes: &[u8],
    start: usize,
    end: usize,
    limits: Limits,
    alloc: Alloc<'a>,
) -> Result<CacheTree<'a>, GitError> {
    let mut nodes: AVec<'a, CacheTreeNode> = Vec::new_in(alloc);
    // Stack frames: (node_idx, remaining_children, last_child_idx).
    let mut stack: AVec<'a, (u32, u32, u32)> = Vec::new_in(alloc);
    let mut pos = start;

    while pos < end {
        if nodes.len() as u32 >= limits.most_cache_tree_nodes {
            return Err(err(
                pos,
                ErrorKind::TooManyEntries {
                    count: nodes.len() as u32 + 1,
                    most: limits.most_cache_tree_nodes,
                },
            ));
        }
        let nul = memchr(0, &bytes[pos..end]).ok_or_else(|| err(pos, ErrorKind::Malformed))?;
        let name_start = pos as u32;
        let name_len = nul as u32;
        pos += nul + 1;

        let nl = memchr(b'\n', &bytes[pos..end]).ok_or_else(|| err(pos, ErrorKind::Malformed))?;
        let counts_str =
            core::str::from_utf8(&bytes[pos..pos + nl]).map_err(|_| err(pos, ErrorKind::Malformed))?;
        let (ec_str, sc_str) =
            counts_str.split_once(' ').ok_or_else(|| err(pos, ErrorKind::Malformed))?;
        let entry_count: i32 = ec_str.parse().map_err(|_| err(pos, ErrorKind::Malformed))?;
        let subtree_count: u32 = sc_str.parse().map_err(|_| err(pos, ErrorKind::Malformed))?;
        pos += nl + 1;

        let oid = if entry_count >= 0 {
            if pos + 20 > end {
                return Err(err(pos, ErrorKind::Malformed));
            }
            let o = Oid(bytes[pos..pos + 20].try_into().expect("20 bytes"));
            pos += 20;
            Some(o)
        } else {
            None
        };

        let my_idx = nodes.len() as u32;
        nodes.push(CacheTreeNode {
            name_start,
            name_len,
            entry_count,
            oid,
            first_child: u32::MAX,
            next_sibling: u32::MAX,
        });

        // Attach to parent if stack is non-empty.
        if let Some((parent_idx, rem, last_child)) = stack.last_mut() {
            if *last_child == u32::MAX {
                nodes[*parent_idx as usize].first_child = my_idx;
            } else {
                nodes[*last_child as usize].next_sibling = my_idx;
            }
            *last_child = my_idx;
            *rem -= 1;
        }

        if subtree_count > 0 {
            if stack.len() >= usize::from(limits.most_dir_depth) {
                return Err(err(pos, ErrorKind::DepthExceeded { most: limits.most_dir_depth }));
            }
            stack.push((my_idx, subtree_count, u32::MAX));
        } else {
            while let Some(&(_, rem, _)) = stack.last() {
                if rem == 0 {
                    stack.pop();
                } else {
                    break;
                }
            }
        }
    }

    if !stack.is_empty() {
        return Err(err(end, ErrorKind::Malformed));
    }

    Ok(CacheTree { nodes })
}
