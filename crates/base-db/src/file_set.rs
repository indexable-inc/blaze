//! The files of a source root ([`FileSet`]), and the path prefixes that say which source root a
//! path is in ([`FileSetConfig`]). A path under no prefix is in the last set.
use std::fmt;

use blaze_hash::{FixedState, HashMap};
use blaze_mem::scratch;
use indexmap::IndexMap;

use crate::{AnchoredPath, FileId, VfsPath};

/// A set of [`VfsPath`]s identified by [`FileId`]s.
#[derive(Default, Clone, Eq, PartialEq)]
pub struct FileSet {
    files: HashMap<VfsPath, FileId>,
    paths: IndexMap<FileId, VfsPath, FixedState>,
    /// Files that have another file's path, each with that file ([`FileSet::alias`]): found by
    /// file alone, never by path, and not among the set's files.
    aliases: HashMap<FileId, FileId>,
}

impl FileSet {
    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The file `path` names from its anchor's directory; `None` if the anchor or that file is
    /// not in the set.
    pub fn resolve_path(&self, path: AnchoredPath<'_>) -> Option<FileId> {
        let mut base = self.path_for_file(&path.anchor)?.clone(); // clone: popped and joined into the key looked up
        base.pop();
        let path = base.join(path.path)?;
        self.files.get(&path).copied()
    }

    /// The file at `path`: never an alias, whose path is another file's.
    pub fn file_for_path(&self, path: &VfsPath) -> Option<&FileId> {
        self.files.get(path)
    }

    /// The path of `file`, or of the file `file` is an alias of.
    pub fn path_for_file(&self, file: &FileId) -> Option<&VfsPath> {
        match self.paths.get(file) {
            Some(path) => Some(path),
            None => self.paths.get(self.aliases.get(file)?),
        }
    }

    /// Makes `alias` a file with `of`'s path, found by file and never by path: `path_for_file`
    /// and `resolve_path` from it answer as for `of`, `file_for_path`, `iter` and `len` know only
    /// `of`. Whether `of` is in the set.
    pub(crate) fn alias(&mut self, alias: FileId, of: FileId) -> bool {
        if !self.paths.contains_key(&of) || self.paths.contains_key(&alias) {
            return false;
        }
        self.aliases.insert(alias, of);
        true
    }

    /// Each alias with the file it is an alias of, in no order.
    pub fn aliases(&self) -> impl Iterator<Item = (FileId, FileId)> + '_ {
        self.aliases.iter().map(|(&alias, &of)| (alias, of))
    }

    /// Puts `file_id` at `path`. A path names the last file put at it; a file has the last path
    /// it was put at.
    pub fn insert(&mut self, file_id: FileId, path: VfsPath) {
        self.aliases.remove(&file_id);
        self.files.insert(path.clone(), file_id); // clone: the set keys both ways
        self.paths.insert(file_id, path);
    }

    /// Takes `file_id` out of the set and returns its path, the other files left in their
    /// order: `O(n)` moves of the entries after it, and no allocation. A path that names
    /// another file keeps naming it. The aliases of `file_id` go with it; an alias taken out
    /// leaves its file, and has no path of its own to return.
    pub fn remove(&mut self, file_id: FileId) -> Option<VfsPath> {
        if self.aliases.remove(&file_id).is_some() {
            return None;
        }
        let path = self.paths.shift_remove(&file_id)?;
        if self.files.get(&path) == Some(&file_id) {
            self.files.remove(&path);
        }
        if !self.aliases.is_empty() {
            self.aliases.retain(|_, of| *of != file_id);
        }
        Some(path)
    }

    /// The set's files, in the order they were put in.
    pub fn iter(&self) -> impl Iterator<Item = FileId> + '_ {
        self.paths.keys().copied()
    }
}

impl fmt::Debug for FileSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileSet").field("n_files", &self.files.len()).finish()
    }
}

/// Path prefixes that partition files into [`FileSet`]s: a file is in the set of the longest
/// prefix of its directory's key ([`VfsPath::encode`], so a prefix matches by bytes:
/// `/foo` takes `/foo-things/lib.rs` unless `/foo-things` is a prefix too), or in the last set.
///
/// Classifying a path is one hash probe per distinct prefix length, longest first: no prefix
/// tree is walked and nothing is allocated past the key's buffer.
#[derive(Debug)]
pub struct FileSetConfig {
    /// The sets `partition` makes: one per prefix list, then the set of everything else.
    n_file_sets: usize,
    /// Each prefix's key, with the first set it was given to.
    prefixes: HashMap<Box<[u8]>, u32>,
    /// The distinct lengths of `prefixes`' keys, longest first.
    lengths: Box<[usize]>,
}

impl Default for FileSetConfig {
    fn default() -> Self {
        FileSetConfig::builder().build()
    }
}

impl FileSetConfig {
    pub fn builder() -> FileSetConfigBuilder {
        FileSetConfigBuilder::default()
    }

    /// `files` partitioned: set `i` for each prefix list `i`, then the set of the rest.
    pub fn partition<'p>(&self, files: impl IntoIterator<Item = (FileId, &'p VfsPath)>) -> Vec<FileSet> {
        let mut res = vec![FileSet::default(); self.n_file_sets]; // alloc: the sets are the output
        scratch::with_bytes(|key| {
            for (file_id, path) in files {
                let set = self.classify(path, key);
                res[set].insert(file_id, path.clone()); // clone: the set owns its paths
            }
        });
        res
    }

    /// The set `path` is in, or `None` for the set of the rest.
    pub fn classify_path(&self, path: &VfsPath) -> Option<usize> {
        let set = scratch::with_bytes(|key| self.classify(path, key));
        (set != self.n_file_sets - 1).then_some(set)
    }

    /// The set `path` is in, with `key` as the buffer for its directory's key.
    fn classify(&self, path: &VfsPath, key: &mut Vec<u8>) -> usize {
        // A file's set is its directory's: `/foo/bar_baz.rs` is not under the prefix `/foo/bar`.
        key.clear();
        // alloc: the parent, only for a path no source root has named yet.
        match path.parent() {
            Some(dir) => dir.encode(key),
            None => path.encode(key),
        }
        for &len in &self.lengths {
            if len <= key.len()
                && let Some(&set) = self.prefixes.get(&key[..len])
            {
                return set as usize;
            }
        }
        self.n_file_sets - 1
    }
}

#[derive(Default)]
pub struct FileSetConfigBuilder {
    roots: Vec<Vec<VfsPath>>,
}

impl FileSetConfigBuilder {
    /// The prefix lists added so far.
    pub fn len(&self) -> usize {
        self.roots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Adds the next set's prefixes.
    pub fn add_file_set(&mut self, roots: Vec<VfsPath>) {
        self.roots.push(roots);
    }

    /// A prefix in more than one list belongs to the first.
    pub fn build(self) -> FileSetConfig {
        let n_file_sets = self.roots.len() + 1;
        let mut prefixes: HashMap<Box<[u8]>, u32> = HashMap::default();
        scratch::with_bytes(|key| {
            for (set, paths) in self.roots.iter().enumerate() {
                let set = u32::try_from(set).expect("fewer than 2^32 source roots");
                for path in paths {
                    key.clear();
                    path.encode(key);
                    // alloc: once per prefix when the workspace's roots are made.
                    prefixes.entry(Box::from(&key[..])).or_insert(set);
                }
            }
        });
        // alloc: once when the workspace's roots are made, a length per distinct one.
        let mut lengths: Vec<usize> = prefixes.keys().map(|it| it.len()).collect();
        lengths.sort_unstable_by(|a, b| b.cmp(a));
        lengths.dedup();
        FileSetConfig { n_file_sets, prefixes, lengths: lengths.into_boxed_slice() }
    }
}

#[cfg(test)]
mod tests;
