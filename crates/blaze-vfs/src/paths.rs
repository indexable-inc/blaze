//! The watcher's names, each interned once as a [`PathId`]: a slot, a look in flight, a delete,
//! a write and what its commits left are numbers into one buffer of text, which is read only
//! where a name leaves the watcher (an effect, an op, a [`crate::Watched`]). Cleared, its room
//! kept, whenever the watcher has nothing in flight, so a warm watch interns with no allocation.

use blaze_hash::{SipKey, adaptive};
use hashbrown::{HashTable, hash_table};

/// A name under the driver's root, interned in a [`Paths`]. Meaningful only until it clears.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct PathId(u32);

/// 4 bytes a name in flight, a lookup one hash of its text and one probe.
const _: () = assert!(size_of::<PathId>() == 4);

/// The names a cleared table keeps room for, at 64 bytes of text each.
const KEPT_NAMES: usize = 1 << 12;

pub(crate) struct Paths {
    /// Every name, one after another.
    text: String,
    /// Where each name ends in `text`; it starts where the one before ends.
    ends: Vec<u32>,
    /// Each name by its hash ([`Paths::hash`]), kept so growing never hashes again.
    index: HashTable<(u64, u32)>,
    /// The key `index` is placed by once it has seen a flood, `None` before: placed adaptively
    /// (`blaze_hash::adaptive`). The names are paths under a checkout, which whoever writes it
    /// picks, and [`fixed_hash`] is public, so they could pick many with one hash and make
    /// each intern walk them all. An intern that compares [`adaptive::FLOODED_COMPARES`] names
    /// that aren't its own switches the table ([`Paths::switch`]) to SipHash under a key drawn
    /// then, for the table's life: a clear keeps it, as whoever flooded it can again.
    key: Option<SipKey>,
    /// How many times the table switched: what the test that it switches once reads.
    #[cfg(test)]
    switches: u32,
    /// A prefix being interned, built here, reused.
    scratch: String,
}

impl Default for Paths {
    fn default() -> Paths {
        Paths {
            text: String::new(),
            // alloc: empty, allocates nothing; grown once and kept across clears.
            ends: Vec::new(),
            index: HashTable::new(),
            key: None,
            #[cfg(test)]
            switches: 0,
            scratch: String::new(),
        }
    }
}

/// The hash a [`Paths`] places names by until it sees a flood, which the VFS's tables of names
/// were all placed by until 2026-11-03: FxHash's multiply and rotate over little-endian words,
/// the high half folded down. A few cycles a word with its constant folded into the code, but
/// every step of it inverts, so a last word can be picked to land on any hash
/// (`crate::probes::one_old_hash`): what [`Paths::switch`] bounds.
#[inline]
pub(crate) fn fixed_hash(bytes: &[u8]) -> u64 {
    const K: u64 = 0xf135_7aea_2e62_a9c5;
    let mut hash = bytes.len() as u64;
    for chunk in bytes.chunks(8) {
        let mut word = [0; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        hash = (hash.rotate_left(26) ^ u64::from_le_bytes(word)).wrapping_mul(K);
    }
    hash ^ (hash >> 32)
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

    /// The hash `name` is placed by: [`fixed_hash`] until the table switched, SipHash under its
    /// key after.
    #[inline]
    fn hash(key: Option<SipKey>, name: &str) -> u64 {
        match key {
            None => fixed_hash(name.as_bytes()),
            Some(key) => key.hash(name.as_bytes()),
        }
    }

    /// The name's id if it was interned. It never switches the table (it can't, by `&self`):
    /// the runs it walks are the ones interning made, which an intern that met a flood ended.
    pub(crate) fn find(&self, name: &str) -> Option<PathId> {
        let hash = Paths::hash(self.key, name);
        let same = |&(at, id): &(u64, u32)| {
            crate::probes::count();
            at == hash && self.get(PathId(id)) == name
        };
        self.index.find(hash, same).map(|it| PathId(it.1))
    }

    pub(crate) fn intern(&mut self, name: &str) -> PathId {
        let hash = Paths::hash(self.key, name);
        let Paths { text, ends, index, .. } = self;
        let text_of = |id: u32| {
            let at = id as usize;
            let start = if at == 0 { 0 } else { ends[at - 1] as usize };
            &text[start..ends[at] as usize]
        };
        // Names compared that weren't this one: hashbrown calls `same` only on a slot whose
        // 7-bit tag is this hash's, so about 0.1 an intern by chance, and every slot of a run
        // of names picked to share a hash.
        let mut others = 0;
        let same = |&(at, id): &(u64, u32)| {
            crate::probes::count();
            let it = at == hash && text_of(id) == name;
            others += usize::from(!it);
            it
        };
        let id = match index.entry(hash, same, |&(at, _)| at) {
            hash_table::Entry::Occupied(it) => PathId(it.get().1),
            hash_table::Entry::Vacant(it) => {
                let id = u32::try_from(ends.len()).expect("fewer than 2^32 names in flight");
                text.push_str(name);
                ends.push(u32::try_from(text.len()).expect("names in flight of under 4 GiB"));
                it.insert((hash, id));
                PathId(id)
            }
        };
        if others >= adaptive::FLOODED_COMPARES && self.key.is_none() {
            self.switch();
        }
        id
    }

    /// Places every name again by SipHash under a key drawn now ([`adaptive::switch_key`]),
    /// for the table's life. Ids stay as they were: an id is the name's place in `ends`, never
    /// where it lands in `index`.
    #[cold]
    #[inline(never)]
    fn switch(&mut self) {
        let key = adaptive::switch_key();
        let mut index = HashTable::with_capacity(self.index.capacity()); // alloc: once, at a flood
        for id in 0..self.ends.len() as u32 {
            let hash = key.hash(self.get(PathId(id)).as_bytes());
            index.insert_unique(hash, (hash, id), |&(at, _)| at);
        }
        self.index = index;
        self.key = Some(key);
        #[cfg(test)]
        {
            self.switches += 1;
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
        assert_eq!(paths.key, None, "honest names switched the table to a key");
    }

    /// Once cleared, the same names intern again into the room they had: no allocation.
    #[test]
    fn a_warm_table_interns_without_allocating() {
        let mut paths = Paths::default();
        let names: Vec<String> =
            (0..512).map(|it| format!("ws/dir{}/file{it}.rs", it % 7)).collect();
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

/// Paths a checkout's author picked to share the hash this table was once placed by: each
/// intern and find looks at a bounded number of places, not at every path before it.
#[cfg(test)]
#[test]
fn paths_with_one_old_hash_take_bounded_probes() {
    let names = crate::probes::one_old_hash(4000);
    let mut paths = Paths::default();
    crate::probes::take();
    let start = std::time::Instant::now();
    let ids: Vec<PathId> = names.iter().map(|it| paths.intern(it)).collect(); // alloc: a test's ids
    let interned = crate::probes::take();
    eprintln!(
        "flood: Paths::intern of {} paths with one old hash: {:?}, {interned} names compared",
        names.len(),
        start.elapsed()
    );
    assert!(paths.key.is_some(), "the flood did not switch the table to a key");
    assert_eq!(paths.switches, 1, "the table switched more than once");
    for (name, id) in names.iter().zip(&ids) {
        assert_eq!(paths.find(name), Some(*id));
    }
    let found = crate::probes::take();
    let bound = 16 * names.len() as u64;
    assert!(
        interned <= bound,
        "interning {} paths with one old hash compared {interned} names, over {bound}",
        names.len()
    );
    assert!(
        found <= bound,
        "finding {} paths with one old hash compared {found} names, over {bound}",
        names.len()
    );
}

/// A flood switches the table once, at the intern that compares
/// [`adaptive::FLOODED_COMPARES`] names that aren't its own, and the table stays keyed: every
/// id reads back and is found, a clear keeps the key, and the same flood again neither switches
/// it again nor walks the names.
#[cfg(test)]
#[test]
fn a_flood_switches_paths_once_for_good() {
    let names = crate::probes::one_old_hash(1000);
    let mut paths = Paths::default();
    let first = &names[..adaptive::FLOODED_COMPARES];
    let before: Vec<PathId> = first.iter().map(|it| paths.intern(it)).collect(); // alloc: a test's ids
    assert_eq!(paths.key, None, "{} names with one hash switched the table", first.len());
    let ids: Vec<PathId> = names.iter().map(|it| paths.intern(it)).collect(); // alloc: a test's ids
    assert_eq!(paths.switches, 1);
    assert!(paths.key.is_some());
    assert_eq!(ids[..before.len()], before[..], "the switch moved an id");
    for (name, id) in names.iter().zip(&ids) {
        assert_eq!(paths.get(*id), name);
        assert_eq!(paths.find(name), Some(*id));
    }
    let key = paths.key;
    paths.clear();
    assert_eq!(paths.key, key, "a clear dropped the key");
    crate::probes::take();
    for name in &names {
        paths.intern(name);
    }
    let compared = crate::probes::take();
    assert_eq!(paths.switches, 1, "the table switched again");
    assert!(compared <= 2 * names.len() as u64, "the flood again compared {compared} names");
}
