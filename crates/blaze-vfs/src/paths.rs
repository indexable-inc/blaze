//! The watcher's names, each interned once as a [`PathId`]: a slot, a look in flight, a delete,
//! a write and what its commits left are numbers into one buffer of text, which is read only
//! where a name leaves the watcher (an effect, an op, a [`crate::Watched`]). Cleared, its room
//! kept, whenever the watcher has nothing in flight, so a warm watch interns with no allocation.

use hashbrown::{HashTable, hash_table};

use crate::store::name_hash;

/// A name under the driver's root, interned in a [`Paths`]. Meaningful only until it clears.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct PathId(u32);

/// 4 bytes a name in flight, a lookup one hash of its text and one probe.
const _: () = assert!(size_of::<PathId>() == 4);

/// The names a cleared table keeps room for, at 64 bytes of text each.
const KEPT_NAMES: usize = 1 << 12;

#[derive(Default)]
pub(crate) struct Paths {
    /// Every name, one after another.
    text: String,
    /// Where each name ends in `text`; it starts where the one before ends.
    ends: Vec<u32>,
    /// Each name by its [`name_hash`], kept so growing never hashes again.
    index: HashTable<(u64, u32)>,
    /// A prefix being interned, built here, reused.
    scratch: String,
}

impl Paths {
    pub(crate) fn get(&self, id: PathId) -> &str {
        let at = id.0 as usize;
        let start = if at == 0 { 0 } else { self.ends[at - 1] as usize };
        &self.text[start..self.ends[at] as usize]
    }

    /// The name `id` relative to the directory whose names start with `below` bytes (its own
    /// and a `/`): `""` for the directory itself.
    pub(crate) fn rel(&self, id: PathId, below: usize) -> &str {
        self.get(id).get(below..).unwrap_or_default()
    }

    pub(crate) fn find(&self, name: &str) -> Option<PathId> {
        let hash = name_hash(name.as_bytes());
        self.index.find(hash, |&(at, id)| at == hash && self.get(PathId(id)) == name).map(|it| PathId(it.1))
    }

    pub(crate) fn intern(&mut self, name: &str) -> PathId {
        let hash = name_hash(name.as_bytes());
        let Paths { text, ends, index, .. } = self;
        let text_of = |id: u32| {
            let at = id as usize;
            let start = if at == 0 { 0 } else { ends[at - 1] as usize };
            &text[start..ends[at] as usize]
        };
        match index.entry(hash, |&(at, id)| at == hash && text_of(id) == name, |&(at, _)| at) {
            hash_table::Entry::Occupied(it) => PathId(it.get().1),
            hash_table::Entry::Vacant(it) => {
                let id = u32::try_from(ends.len()).expect("fewer than 2^32 names in flight");
                text.push_str(name);
                ends.push(u32::try_from(text.len()).expect("names in flight of under 4 GiB"));
                it.insert((hash, id));
                PathId(id)
            }
        }
    }

    /// The name of `id`'s first `len` bytes (an ancestor's, `len` at a `/`), interned.
    pub(crate) fn intern_prefix(&mut self, id: PathId, len: usize) -> PathId {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        scratch.push_str(&self.get(id)[..len]);
        let prefix = self.intern(&scratch);
        self.scratch = scratch;
        prefix
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.ends.len()
    }

    /// Forgets every name, keeping the room for [`KEPT_NAMES`] of them: no [`PathId`] it gave
    /// may be used after. A rescan of a checkout's 100 000 paths gives the rest back.
    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.text.shrink_to(KEPT_NAMES * 64);
        self.ends.clear();
        self.ends.shrink_to(KEPT_NAMES);
        self.index.clear();
        self.index.shrink_to(KEPT_NAMES, |&(hash, _)| hash);
    }
}

#[cfg(test)]
mod tests {
    use super::Paths;

    #[test]
    fn a_name_is_one_id_and_reads_back() {
        let mut paths = Paths::default();
        let names = ["ws", "ws/a", "ws/a/b.rs", "ws/ab", "ws/a/b.rs", "ws/a"];
        let ids: Vec<_> = names.iter().map(|it| paths.intern(it)).collect();
        assert_eq!(paths.len(), 4);
        for (name, id) in names.iter().zip(&ids) {
            assert_eq!(paths.get(*id), *name);
            assert_eq!(paths.find(name), Some(*id));
        }
        assert_eq!(ids[1], ids[5]);
        assert_eq!(paths.rel(ids[0], 3), "");
        assert_eq!(paths.rel(ids[2], 3), "a/b.rs");
        assert_eq!(paths.intern_prefix(ids[2], 4), ids[1]);
        let up = paths.intern_prefix(ids[3], 3);
        assert_eq!(paths.get(up), "ws/");
        assert_eq!(paths.find("ws/zz"), None);
    }

    /// Once cleared, the same names intern again into the room they had: no allocation.
    #[test]
    fn a_warm_table_interns_without_allocating() {
        let mut paths = Paths::default();
        let names: Vec<String> = (0..512).map(|it| format!("ws/dir{}/file{it}.rs", it % 7)).collect();
        for name in &names {
            paths.intern(name);
        }
        paths.clear();
        blaze_alloc_count::assert_at_most(0, "names interned again once warm", || {
            for name in &names {
                let id = paths.intern(name);
                assert_eq!(paths.get(id), name);
            }
        });
        assert!(blaze_alloc_count::installed());
    }
}
