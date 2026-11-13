//! How the database reads a view's files: a [`Tree`] answers for any root, from any thread.

use std::{collections::BTreeMap, sync::Arc};

use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use blaze_vfs::{Bytes, Corrupt, Mode};
use std::alloc::Global;

use crate::table::{FxMap, ShortLock};

/// The files of every root, read from many worker threads at once. A root's files never
/// change (a root is a content ID), so every answer for a root is the same forever.
pub trait Tree: Send + Sync {
    /// The file at `path` under `root`: its content ID, mode and bytes (a symlink's are its
    /// target); `None` for none, a directory, or a root this tree does not have. A file whose
    /// bytes are a range of the mapped image that fails its check is `Err`, naming the
    /// image's section and group: never `None`, which would be a file that is not there.
    fn file(
        &self,
        root: ContentId,
        path: &str,
    ) -> Result<Option<(ContentId, Mode, Bytes)>, Corrupt>;
    /// Lends `read` the names in the directory at `path` (`""` for the root), sorted by name,
    /// each with whether it is a directory; `false`, with `read` not called, for none or a
    /// file. The database hashes a listing as it is lent, copying nothing, and stops naming
    /// the root and directory at a name out of order.
    fn read_dir(&self, root: ContentId, path: &str, read: &mut dyn FnMut(Names<'_, '_>)) -> bool; // dyn: one call a directory per view, and `Tree` is a trait object
    /// The content ID of the directory at `path` under `root` (`""`: `root` itself, when this
    /// tree has it); `None` for none, a file, or a root this tree does not have. It stands
    /// for every file below the directory, by path under it, mode and content: two
    /// directories of one tree with the same ID hold the same files, and the same files give
    /// the same ID wherever the directory is. No file's bytes are read and nothing is listed
    /// to the caller, so a key made of it costs the path's depth, not the directory's size
    /// (restore.md, "Restart reads records (d)", A). IDs of two kinds of tree are not
    /// compared: each hashes by its own domain.
    fn dir(&self, root: ContentId, path: &str) -> Option<ContentId>;

    /// [`Tree::read_dir`]'s names, owned: for a caller that keeps them.
    fn list(&self, root: ContentId, path: &str) -> Option<Vec<(String, bool)>> {
        let mut owned = None;
        self.read_dir(root, path, &mut |names| {
            // alloc: the caller asked for the names owned.
            owned = Some(names.map(|(name, dir)| (name.to_owned(), dir)).collect());
        });
        owned
    }
}

/// A directory's names as a [`Tree`] lends them (for `'n`), each with whether it is a
/// directory.
pub type Names<'a, 'n> = &'a mut (dyn Iterator<Item = (&'n str, bool)> + 'a); // dyn: see `Tree::read_dir`

/// The VFS's files: [`blaze_vfs::Vfs::reader`], a lock-free handle on its immutable nodes by
/// content ID. Here rather than in blaze-vfs because blaze-query depends on blaze-vfs.
///
/// A file's [`Bytes`] are the tree's own (its `Arc`, or its range of the mapped image) with
/// one count taken, once per file per view, as the database keeps the input.
impl Tree for blaze_vfs::Reader {
    fn file(
        &self,
        root: ContentId,
        path: &str,
    ) -> Result<Option<(ContentId, Mode, Bytes)>, Corrupt> {
        blaze_vfs::Reader::file(self, root, path)
    }

    fn read_dir(&self, root: ContentId, path: &str, read: &mut dyn FnMut(Names<'_, '_>)) -> bool {
        // dyn: see `Tree::read_dir`
        let pinned = self.pin();
        let Some(mut names) = pinned.list(root, path) else { return false };
        read(&mut names);
        true
    }

    fn dir(&self, root: ContentId, path: &str) -> Option<ContentId> {
        // The node's own Merkle ID: the tree made it when the directory was built.
        self.dir_id(root, path)
    }
}

/// A tree in memory, for tests and for what-ifs built without the VFS. Its roots are content
/// IDs of the sorted list of paths, modes and file IDs (not the VFS's Merkle IDs).
pub struct MemTree {
    roots: ShortLock<FxMap<ContentId, Arc<MemRoot>>>,
}

impl Default for MemTree {
    fn default() -> MemTree {
        MemTree { roots: ShortLock::new("query.tree.mem_roots", FxMap::default()) }
    }
}

// BTreeMap: hashed in path order into the root's ID.
type Files = BTreeMap<String, (ContentId, Mode, Bytes)>;

/// A root's files and each of its directories' names, sorted, made with the root: a listing is
/// lent as the VFS lends one, nothing made per read.
struct MemRoot {
    files: Files,
    dirs: HashMap<String, Vec<(String, bool)>, Global>, // alloc: `MemTree` is the tree of tests and what-ifs, off every hot path
}

impl MemRoot {
    fn new(files: Files) -> MemRoot {
        // alloc: `MemTree` is the tree of tests and what-ifs, off every hot path
        let mut dirs: HashMap<String, Vec<(String, bool)>, Global> = HashMap::new_in(Global);
        dirs.insert(String::new(), Vec::new()); // alloc: see above
        for path in files.keys() {
            let (mut parent, mut from) = (0, 0);
            loop {
                let listing = dirs.entry(path[..parent].to_owned()).or_default(); // alloc: see above
                match path[from..].find('/') {
                    Some(at) => {
                        listing.push((path[from..from + at].to_owned(), true)); // alloc: see above
                        parent = from + at;
                        from = parent + 1;
                    }
                    None => {
                        listing.push((path[from..].to_owned(), false)); // alloc: see above
                        break;
                    }
                }
            }
        }
        for names in dirs.values_mut() {
            // A name that is a directory under one path and a file at another is a directory.
            names.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
            names.dedup_by(|later, first| later.0 == first.0);
        }
        MemRoot { files, dirs }
    }

    /// The names in the directory at `path`, sorted; `None` for none or a file.
    fn names(&self, path: &str) -> Option<&[(String, bool)]> {
        if self.files.contains_key(path) {
            return None;
        }
        self.dirs.get(path).map(Vec::as_slice)
    }

    /// [`Tree::dir`] of a directory below the root: the hash of each file under `path`, by
    /// its path under `path`, its mode and its content ID, in path order; `None` where
    /// `path` is no directory. The files are already keyed by path, so none is read.
    fn dir(&self, path: &str) -> Option<ContentId> {
        self.names(path)?;
        let mut hasher = Hasher::new(MEM_DIR_DOMAIN);
        let below = format!("{path}/"); // alloc: `MemTree` is the tree of tests and what-ifs, off every hot path
        for (file, (id, mode, _)) in self.files.range::<str, _>((std::ops::Bound::Included(below.as_str()), std::ops::Bound::Unbounded)) {
            let Some(under) = file.strip_prefix(below.as_str()) else { break };
            hasher.part(under.as_bytes()).update(&mode.git().to_le_bytes()).id(id);
        }
        Some(hasher.finish())
    }
}

pub const MEM_ROOT_DOMAIN: &str = "blaze v2 db memory root 2";
/// A [`MemTree`] directory's ID ([`Tree::dir`]): the files below it by path under it.
pub const MEM_DIR_DOMAIN: &str = "blaze v2 db memory directory 1";

impl MemTree {
    /// The root holding exactly `files`, as `(path, bytes)`, each a regular file.
    pub fn root(&self, files: &[(&str, &[u8])]) -> ContentId {
        let files: Vec<(&str, Mode, &[u8])> =
            files.iter().map(|&(path, bytes)| (path, Mode::Regular, bytes)).collect(); // alloc: `MemTree` is the tree of tests and what-ifs, off every hot path
        self.root_with_modes(&files)
    }

    /// The root holding exactly `files`, as `(path, mode, bytes)`.
    pub fn root_with_modes(&self, files: &[(&str, Mode, &[u8])]) -> ContentId {
        let files: Files = files
            .iter()
            .map(|&(path, mode, bytes)| {
                let id = ContentId::of(blaze_vfs::FILE_DOMAIN, bytes);
                (path.to_owned(), (id, mode, Bytes::from(Arc::<[u8]>::from(bytes)))) // alloc: `MemTree` is the tree of tests and what-ifs, off every hot path
            })
            .collect(); // alloc: `MemTree` is the tree of tests and what-ifs, off every hot path
        let mut hasher = Hasher::new(MEM_ROOT_DOMAIN);
        for (path, (id, mode, _)) in &files {
            hasher.part(path.as_bytes()).update(&mode.git().to_le_bytes()).id(id);
        }
        let root = hasher.finish();
        self.roots.lock().insert(root, Arc::new(MemRoot::new(files))); // alloc: `MemTree` is the tree of tests and what-ifs, off every hot path
        root
    }

    fn files(&self, root: ContentId) -> Option<Arc<MemRoot>> {
        self.roots.lock().get(&root).cloned()
    }

    /// The names in the directory at `path` under `root`, sorted, each with whether it is a
    /// directory; `None` for none or a file.
    #[cfg(test)]
    fn names(&self, root: ContentId, path: &str) -> Option<Vec<(String, bool)>> {
        self.files(root)?.names(path).map(<[_]>::to_vec)
    }
}

impl Tree for MemTree {
    fn file(
        &self,
        root: ContentId,
        path: &str,
    ) -> Result<Option<(ContentId, Mode, Bytes)>, Corrupt> {
        Ok(self.files(root).and_then(|it| it.files.get(path).cloned()))
    }

    fn read_dir(&self, root: ContentId, path: &str, read: &mut dyn FnMut(Names<'_, '_>)) -> bool {
        // dyn: see `Tree::read_dir`
        let Some(files) = self.files(root) else { return false };
        let Some(names) = files.names(path) else { return false };
        read(&mut names.iter().map(|(name, dir)| (name.as_str(), *dir)));
        true
    }

    fn dir(&self, root: ContentId, path: &str) -> Option<ContentId> {
        let files = self.files(root)?;
        if path.is_empty() {
            return Some(root);
        }
        files.dir(path)
    }
}

#[cfg(test)]
mod tests {
    use super::{MemTree, Tree as _};

    /// The invariant: a directory's ID is equal exactly when the files below it are (path
    /// under it, mode, content), wherever the directory is and whatever is beside it.
    #[test]
    fn a_directorys_id_is_its_files_and_nothing_beside_it() {
        let tree = MemTree::default();
        let one = tree.root(&[("a/x.rs", b"1"), ("a/sub/y.rs", b"2"), ("a.rs", b"9"), ("b/x.rs", b"1")]);
        let moved = tree.root(&[("deep/er/x.rs", b"1"), ("deep/er/sub/y.rs", b"2"), ("other", b"3")]);
        let edited = tree.root(&[("a/x.rs", b"1"), ("a/sub/y.rs", b"two"), ("a.rs", b"9")]);
        let renamed = tree.root(&[("a/x.rs", b"1"), ("a/sub/z.rs", b"2")]);
        let a = tree.dir(one, "a").expect("a directory");
        assert_eq!(tree.dir(one, ""), Some(one), "the root is its own ID");
        assert_eq!(tree.dir(moved, "deep/er"), Some(a), "the same files elsewhere");
        assert_ne!(tree.dir(edited, "a"), Some(a), "a file's content below it");
        assert_ne!(tree.dir(renamed, "a"), Some(a), "a file's name below it");
        assert_ne!(tree.dir(one, "b"), Some(a), "`a.rs` and `a/sub` are not in `b`");
        assert_eq!(tree.dir(one, "a/sub"), tree.dir(moved, "deep/er/sub"));
        assert_eq!(tree.dir(one, "a/x.rs"), None, "a file");
        assert_eq!(tree.dir(one, "a/"), None, "a path is spelled without its last `/`");
        assert_eq!(tree.dir(one, "nowhere"), None);
        assert_eq!(tree.dir(a, ""), None, "a directory's ID is not a root this tree has");
    }

    #[test]
    fn a_listing_is_sorted_by_name_and_sees_only_its_directory() {
        let tree = MemTree::default();
        let root = tree.root(&[
            ("a/b", b"1"),
            ("a-x", b"2"),
            ("a/c/d", b"3"),
            ("a.rs", b"4"),
            ("ab", b"5"),
            ("a/c.rs", b"6"),
            ("b", b"7"),
        ]);
        let names = |path| tree.names(root, path);
        let owned = |it: &[(&str, bool)]| {
            Some(it.iter().map(|&(n, d)| (n.to_owned(), d)).collect::<Vec<_>>())
        };
        assert_eq!(
            names(""),
            owned(&[("a", true), ("a-x", false), ("a.rs", false), ("ab", false), ("b", false)])
        );
        assert_eq!(names("a"), owned(&[("b", false), ("c", true), ("c.rs", false)]));
        assert_eq!(names("a/c"), owned(&[("d", false)]));
        assert_eq!(names("a/b"), None, "a file");
        assert_eq!(names("ab/x"), None, "nothing there");
        assert_eq!(names("a-"), None, "a prefix of a name is no directory");
    }
}
