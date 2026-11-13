//! What of a watched directory is out of its view: the one home of that rule, which the
//! [`Watcher`](super::Watcher) and its reference both follow.
//!
//! - **Fixed names**: `target` at the root (what cargo or a `--target-dir` there writes) and
//!   every `.git`, with everything under them. `.git` has no tag; `target` has one once cargo
//!   wrote it, and is out before that too.
//! - **Cache directories, at any depth**: a directory holding a regular file `CACHEDIR.TAG`
//!   that starts with [`SIGNATURE`] (the Cache Directory Tagging spec,
//!   <https://bford.info/cachedir/>; cargo writes one in every target directory) is out of the
//!   view with everything under it. The directory's own name and its tag stay in the column
//!   (the tag saved [`chose`](crate::disk::chose): the disk was seen there, the head holds none
//!   of it), so a restart knows the directory without a read ([`Caches::from_column`]); nothing
//!   else under it is stamped, read, hashed or committed, and a journal event under it is
//!   dropped. A tag is read only for its first [`SIGNATURE`]`.len()` bytes ([`probe`]), and only
//!   when its directory is listed or its own stamp moved. A directory that gains a tag leaves
//!   the head; one that loses it (gone, rewritten without the signature, made a directory or a
//!   link) is listed and comes back whole. The watched directory itself is never pruned: a tag
//!   at its root is a file like any other.

use blaze_core::{Effect, FileName, Stamp, Tag};
use blaze_hash::HashSet;
use std::alloc::Global;

use crate::{
    disk::{Disk, vouches},
    tree::{NodeId, Tree, ancestors},
};

/// The tag's file name in its directory.
pub(crate) const TAG_NAME: &str = "CACHEDIR.TAG";

/// What a tag starts with: the 43 bytes the spec names. Anything after them is free text.
pub(crate) const SIGNATURE: &[u8; 43] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// Whether `rel` (relative to the watched directory) is out of the view by its name alone: the
/// root's `target` and every `.git`, with what is under them. A cache directory elsewhere is out
/// too, but only a look at its tag can say so ([`Caches::hides`]).
pub fn fixed_out(rel: &str) -> bool {
    rel.strip_prefix("target").is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        || (rel.contains(".git") && rel.split('/').any(|part| part == ".git"))
}

/// The directory `rel` would be the tag of (`a/b` for `a/b/CACHEDIR.TAG`); `None` for any other
/// name, and for a tag at the root (never pruned).
pub(crate) fn tag_of(rel: &str) -> Option<&str> {
    let dir = rel.strip_suffix(TAG_NAME)?.strip_suffix('/')?;
    (!dir.is_empty()).then_some(dir)
}

/// Whether a file with `stamp` can be a tag: a regular file (not a directory, a link or gone)
/// at least as long as the signature.
pub(crate) fn may_be_tag(stamp: &Stamp) -> bool {
    !stamp.is_gone()
        && !stamp.is_unknown()
        && !stamp.is_dir()
        && !stamp.is_link()
        && stamp.size >= SIGNATURE.len() as u64
}

/// The read of a tag's signature: its first bytes only, whatever the file's length.
pub(crate) fn probe(file: FileName, tag: Tag) -> Effect {
    Effect::Read { file, at: 0, len: SIGNATURE.len(), tag }
}

/// Whether a probe's bytes are a tag's.
pub(crate) fn is_tag(bytes: &[u8]) -> bool {
    #[cfg(test)]
    if crate::planted(crate::Plant::TagUnchecked) {
        return true;
    }
    #[cfg(test)]
    if crate::planted(crate::Plant::TagIgnored) {
        return false;
    }
    bytes == SIGNATURE
}

/// The cache directories of one watch, relative to its directory.
#[derive(Default)]
pub(crate) struct Caches {
    // alloc: a handful of names per checkout (its target directories), kept for the watch.
    dirs: HashSet<Box<str>, Global>,
}

impl Caches {
    /// The cache directories a column names: each `d/CACHEDIR.TAG` it saved as one the head
    /// chose against, where the head has nothing at `d`. What a pruned directory leaves in the
    /// column, so a start knows it before any look. `below` is the length of the watched
    /// directory's name and its `/`.
    pub(crate) fn from_column(
        disk: Option<&Disk>,
        tree: &Tree,
        head: NodeId,
        below: usize,
    ) -> Caches {
        let mut caches = Caches::default();
        let Some(disk) = disk else { return caches };
        disk.under(disk.dir.as_str(), |name, _| {
            let Some(rel) = name.get(below..) else { return };
            let Some(dir) = tag_of(rel) else { return };
            let chosen = disk.saved(name).is_some_and(|it| !vouches(&it));
            if chosen && tree.lookup(head, dir).is_none() {
                caches.dirs.insert(dir.into());
            }
        });
        caches
    }

    /// Whether `rel` is a cache directory.
    pub(crate) fn is_cache(&self, rel: &str) -> bool {
        !self.dirs.is_empty() && self.dirs.contains(rel)
    }

    /// Whether `rel` is out of the view: by [`fixed_out`], or under a cache directory and not
    /// that directory's tag (whose changes are how the directory leaves the set).
    pub(crate) fn hides(&self, rel: &str) -> bool {
        if fixed_out(rel) {
            return true;
        }
        if self.dirs.is_empty() {
            return false;
        }
        let tag = tag_of(rel);
        ancestors(rel).any(|up| self.dirs.contains(up) && tag != Some(up))
    }

    pub(crate) fn insert(&mut self, rel: &str) {
        if !self.dirs.contains(rel) {
            self.dirs.insert(rel.into());
        }
    }

    /// Takes `rel` out of the set; whether it was in it.
    pub(crate) fn remove(&mut self, rel: &str) -> bool {
        self.dirs.remove(rel)
    }
}
