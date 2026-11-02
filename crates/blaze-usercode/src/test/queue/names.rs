//! Names interned to `u32` IDs, in the order first seen: their bytes in one buffer and an
//! open-addressed index of their hash, so a name seen again costs one hash and one compare
//! and allocates nothing. A record's paths and variables and a hello's test names go through
//! one of these on the way in; from then on a line is a `u32`.
//!
//! Cost: 4 bytes of index (at most half full) plus 12 of columns per name, and its bytes once.

use std::hash::BuildHasher;

use blaze_hash::FixedState;

/// The most names one table holds: an ID fits in 29 bits, beside a 3-bit tag.
pub(super) const MOST: u32 = 1 << 29;

#[derive(Default, Debug)]
pub(super) struct Names {
    text: String,
    /// Each name's end in `text`; it starts where the one before ends.
    ends: Vec<u32>,
    /// Each name's hash, so the index grows without hashing again.
    hashes: Vec<u64>,
    /// A power of two of slots, at most half full: a name's ID plus one, zero for none.
    index: Vec<u32>,
}

impl Names {
    pub(super) fn new() -> Names {
        Names::default()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.ends.len()
    }

    /// The name of `id`, which this table gave.
    pub(super) fn get(&self, id: u32) -> &str {
        let end = self.ends[id as usize] as usize;
        let start = match id {
            0 => 0,
            _ => self.ends[id as usize - 1] as usize,
        };
        &self.text[start..end]
    }

    /// The ID of `name`, if it was interned.
    pub(super) fn find(&self, name: &str) -> Option<u32> {
        if self.index.is_empty() {
            return None;
        }
        self.slot(name, FixedState.hash_one(name)).1
    }

    /// The ID of `name`, interned now if it was not; `None` once the table is full ([`MOST`]
    /// names, or 4 GiB of them).
    pub(super) fn intern(&mut self, name: &str) -> Option<u32> {
        let hash = FixedState.hash_one(name);
        if !self.index.is_empty()
            && let (_, Some(id)) = self.slot(name, hash)
        {
            return Some(id);
        }
        let id = self.ends.len() as u32;
        let end = self.text.len() + name.len();
        if id >= MOST || end > u32::MAX as usize {
            return None;
        }
        if 2 * (self.ends.len() + 1) > self.index.len() {
            self.grow();
        }
        self.text.push_str(name);
        self.ends.push(end as u32);
        self.hashes.push(hash);
        let (at, _) = self.slot(name, hash);
        self.index[at] = id + 1;
        Some(id)
    }

    /// Where `name` is in the index, or the empty slot it would go in.
    fn slot(&self, name: &str, hash: u64) -> (usize, Option<u32>) {
        let mask = self.index.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            match self.index[at] {
                0 => return (at, None),
                held => {
                    let id = held - 1;
                    if self.hashes[id as usize] == hash && self.get(id) == name {
                        return (at, Some(id));
                    }
                }
            }
            at = (at + 1) & mask;
        }
    }

    fn grow(&mut self) {
        let len = (self.index.len() * 2).max(16);
        self.index.clear();
        self.index.resize(len, 0);
        let mask = len - 1;
        for (id, hash) in self.hashes.iter().enumerate() {
            let mut at = *hash as usize & mask;
            while self.index[at] != 0 {
                at = (at + 1) & mask;
            }
            self.index[at] = id as u32 + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_hash::HashMap;
    use blaze_ids::rng::SplitMix64;

    use super::*;

    #[test]
    fn a_name_interned_again_allocates_nothing() {
        let mut names = Names::new();
        let all: Vec<String> = (0..1_000).map(|n| format!("/ws/{n}")).collect();
        for name in &all {
            names.intern(name).unwrap();
        }
        blaze_alloc_count::assert_at_most(0, "an interner hit", || {
            for name in &all {
                assert!(names.intern(name).is_some() && names.find(name).is_some());
            }
        });
    }

    /// Against a map, over seeded names that repeat: the same ID for the same name, in the
    /// order first seen, every name read back as it was given.
    #[test]
    fn names_intern_as_a_map_would() {
        let mut rng = SplitMix64::new(0x853c_49e6_748f_ea9b);
        let mut names = Names::new();
        let mut map: HashMap<String, u32> = HashMap::default();
        for _ in 0..20_000 {
            let name = format!("/ws/{}", rng.below(3_000));
            let next = map.len() as u32;
            let expected = *map.entry(name.clone()).or_insert(next);
            assert_eq!(names.intern(&name), Some(expected));
            assert_eq!(names.find(&name), Some(expected));
        }
        assert_eq!(names.len(), map.len());
        for (name, id) in &map {
            assert_eq!(names.get(*id), name);
        }
        assert_eq!(names.find("/not/there"), None);
        assert_eq!(Names::new().find(""), None);
    }

    /// A name seen again grows nothing: the steady state of a server's record allocates
    /// nothing for its names.
    #[test]
    fn a_name_seen_again_grows_nothing() {
        let mut names = Names::new();
        for n in 0..100 {
            names.intern(&format!("/lib/{n}"));
        }
        let (text, index) = (names.text.capacity(), names.index.capacity());
        for n in 0..100 {
            names.intern(&format!("/lib/{n}"));
        }
        assert_eq!((names.text.capacity(), names.index.capacity(), names.len()), (text, index, 100));
        assert_eq!(names.intern(""), Some(100));
        assert_eq!(names.get(100), "");
    }
}
