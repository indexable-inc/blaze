//! [`Reader`]: the tree's nodes by content ID, read from any thread while the [`Vfs`] goes on,
//! without a lock and without writing memory another core writes.
//!
//! A node is immutable and named by its content ID, so what a root holds never changes; the
//! only change is which nodes exist. A reader reads the tree's own storage ([`crate::store`]):
//! a node is readable as soon as the [`Vfs`] interned it, before any call that gives its ID
//! returns, and after [`Vfs::collect`] a dropped root reads as none. A read is in a task
//! ([`blaze_reclaim::enter`]: thread-local counts inside the task a plane worker already runs,
//! else a store to the thread's own slot and a fence), then walks the path with acquire loads:
//! one index probe for the root, one binary search of a name column per component. A file's
//! bytes are the tree's own (its [`Arc`], or its range of the mapped image), never copied;
//! [`Pinned`] reads them in place, with no reference count touched, and many reads under one
//! pin. A file of the image is read only once the page groups it is in match their CRC32C:
//! one that does not is [`Corrupt`], naming the section and group, and its bytes are never
//! lent.
//!
//! [`Vfs`]: crate::Vfs
//! [`Vfs::collect`]: crate::Vfs::collect
//! [`Arc`]: std::sync::Arc

use std::sync::Arc;

use blaze_ids::ContentId;
use blaze_image::Corrupt;

use crate::{
    File, Mode,
    image::Bytes,
    store::{Shared, Snapshot, mode_of},
};

/// A handle on every node the [`Vfs`](crate::Vfs) has, by content ID: cheap to clone, `Send`
/// and `Sync`. A root is readable while the tree has it (a head reaches it, a view of it is
/// open, or a collection kept it); after that it reads as none.
#[derive(Clone)]
pub struct Reader {
    shared: Arc<Shared>,
}

impl Reader {
    pub(crate) fn new(shared: Arc<Shared>) -> Reader {
        Reader { shared }
    }

    /// The file at `path` under `root`: its content ID, mode and bytes (a symlink's are its
    /// target, not followed); `None` for none, a directory, or a root the tree does not have.
    /// The bytes are the tree's own, with one count taken so they outlive the pin;
    /// [`Pinned::file`] lends them with none.
    pub fn file(
        &self,
        root: ContentId,
        path: &str,
    ) -> Result<Option<(ContentId, Mode, Bytes)>, Corrupt> {
        self.pin().held(root, path)
    }

    /// The names in the directory at `path` (`""` for the root), sorted, each with whether it
    /// is a directory; `None` for none, a file, or a root the tree does not have. Owned, as
    /// they outlive the pin: one allocation for the list and one per name, for a caller that
    /// keeps them. Every hot reader (the database's `Tree`) lists through [`Pinned::list`],
    /// which lends them and allocates nothing.
    pub fn list(&self, root: ContentId, path: &str) -> Option<std::vec::IntoIter<(String, bool)>> {
        let pinned = self.pin();
        let names: Vec<(String, bool)> =
            // alloc: owned names that outlive the pin; `Pinned::list` lends them instead.
            pinned.list(root, path)?.map(|(name, dir)| (name.to_owned(), dir)).collect();
        Some(names.into_iter())
    }

    /// Pins this thread to read the tree as it is now, in place, as many times as wanted.
    /// Hold it only while reading: a collection's old nodes are freed once the task it is in
    /// ends.
    pub fn pin(&self) -> Pinned<'_> {
        Pinned { snapshot: self.shared.read() }
    }
}

/// The tree as it was when [`Reader::pin`] was called, read in place. Not `Send`: the task is
/// the thread's.
pub struct Pinned<'r> {
    snapshot: Snapshot<'r>,
}

impl Pinned<'_> {
    /// The file at `path` under `root`, its bytes read in place; `None` for none, a
    /// directory, or a root the tree does not have.
    pub fn file(&self, root: ContentId, path: &str) -> Result<Option<File<'_>>, Corrupt> {
        let Some((node, mode)) = self.walk(root, path) else { return Ok(None) };
        let table = self.snapshot.table();
        let shape = table.shape(node);
        if shape.is_dir() {
            return Ok(None);
        }
        Ok(Some(File { id: *table.id(node), mode, bytes: table.blob(shape)? }))
    }

    /// [`Pinned::file`], its bytes held past the pin: the tree's own, with one count taken. A
    /// walk of many directories reads each file this way under one pin, listing with
    /// [`Pinned::list`].
    pub fn held(&self, root: ContentId, path: &str) -> Result<Option<(ContentId, Mode, Bytes)>, Corrupt> {
        let Some((node, mode)) = self.walk(root, path) else { return Ok(None) };
        let table = self.snapshot.table();
        let shape = table.shape(node);
        if shape.is_dir() {
            return Ok(None);
        }
        Ok(Some((*table.id(node), mode, table.bytes(shape)?)))
    }

    /// The names in the directory at `path` (`""` for the root), sorted, each with whether it
    /// is a directory; `None` for none, a file, or a root the tree does not have.
    pub fn list(
        &self,
        root: ContentId,
        path: &str,
    ) -> Option<impl Iterator<Item = (&str, bool)> + '_> {
        let (node, _) = self.walk(root, path)?;
        let table = self.snapshot.table();
        let shape = table.shape(node);
        if !shape.is_dir() {
            return None;
        }
        let entries = table.entries(shape);
        let names = self.snapshot.names();
        Some((0..entries.len()).map(move |at| (names.str(entries.names[at]), entries.is_dir(at))))
    }

    /// The node `path` names under the directory `root`, and its mode, as the tree's `entry`.
    fn walk(&self, root: ContentId, path: &str) -> Option<(u32, Mode)> {
        let table = self.snapshot.table();
        let root = table.find(&root)?;
        let mut shape = table.shape(root);
        if !shape.is_dir() {
            return None;
        }
        if path.is_empty() {
            return Some((root, Mode::Regular));
        }
        let (mut node, mut mode) = (root, Mode::Regular);
        for name in path.split('/') {
            if !shape.is_dir() {
                return None;
            }
            let entries = table.entries(shape);
            let at = entries.find(self.snapshot.names(), name.as_bytes())?;
            (node, mode) = (entries.nodes[at], mode_of(entries.modes[at]));
            shape = table.shape(node);
        }
        Some((node, mode))
    }
}
