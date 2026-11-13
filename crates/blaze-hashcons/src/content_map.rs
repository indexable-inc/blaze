//! [`ContentMap`]: values stored once each, found by the BLAKE3 of their content. The map of
//! one owner; [`HashCons`](crate::HashCons) is the one many threads share.
//!
//! `insert(value)` gives the value's [`ContentId`] ([`content_of`]: the type's domain, then
//! what [`Consed::hash_content`] feeds), and stores the value unless an equal one is held;
//! `get(&id)` lends it. The table is a [`ByHash`]: the id is a hash already, so a lookup
//! hashes nothing, and BLAKE3 runs once per insert, over the value. It is in the allocator
//! its type names, as every single-owner table is.
//!
//! Why this is not `HashCons`: that one is shared between threads (shards, a lock per insert,
//! lock-free reads), hands out a dense [`Idx`](blaze_ids::Idx) and a handle that compares by
//! address, never frees a value, and lives on the global heap for the process. This one has
//! one owner, no index, removes, and dies with its allocator. They are the same pair as
//! `SharedMap` and `HashMap`, and they agree on what a value's id is ([`content_of`]), so a
//! value moved from one to the other keeps its id.
//!
//! The trust rule: an id is trusted only where this map computed it. A value that comes with
//! an id from outside (a file, a peer, a cache) goes in through [`ContentMap::insert_claimed`],
//! which hashes the value and refuses the pair when the id is not its own; nothing is stored
//! under an id nobody checked.

use std::{alloc::Allocator, any::type_name, fmt};

use blaze_hash::{ByHash, hash_map::Entry};
use blaze_ids::ContentId;

use crate::{Consed, content_of};

/// Values of `V`, each stored once, by content ID, in the allocator `A`.
pub struct ContentMap<V, A: Allocator> {
    held: ByHash<ContentId, V, A>,
}

/// A value whose content is not what the id it came with says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mismatch {
    /// The id the value came with.
    pub claimed: ContentId,
    /// The id of the value's content.
    pub actual: ContentId,
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a value said to have the content id {:?} has {:?}: it is not stored",
            self.claimed, self.actual
        )
    }
}

impl std::error::Error for Mismatch {}

impl<V, A: Allocator> ContentMap<V, A> {
    /// An empty map in `alloc`: no allocation until the first insert.
    pub fn new_in(alloc: A) -> Self {
        ContentMap { held: ByHash::new_in(alloc) }
    }

    /// An empty map in `alloc` with room for `capacity` values: one allocation, and none
    /// after until it holds more.
    pub fn with_capacity_in(capacity: usize, alloc: A) -> Self {
        ContentMap { held: ByHash::with_capacity_in(capacity, alloc) }
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// The value whose content `id` is of, if it is held.
    pub fn get(&self, id: &ContentId) -> Option<&V> {
        self.held.get(id)
    }

    pub fn contains(&self, id: &ContentId) -> bool {
        self.held.contains_key(id)
    }

    /// Takes the value of `id` out.
    pub fn remove(&mut self, id: &ContentId) -> Option<V> {
        self.held.remove(id)
    }

    /// Removes every value and keeps the memory.
    pub fn clear(&mut self) {
        self.held.clear();
    }

    /// Every value with its id, in no order: one that reaches an output is sorted by id first
    /// (a [`ContentId`] is `Ord`).
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&ContentId, &V)> {
        self.held.iter()
    }
}

impl<V: Consed, A: Allocator> ContentMap<V, A> {
    /// The content ID of `value`, which is stored unless an equal value is held already (then
    /// `value` is dropped). BLAKE3 over the value once; an equal value held is compared once.
    ///
    /// Two unequal values with one id stop the process naming the type, as
    /// [`HashCons::intern`](crate::HashCons::intern) does: `hash_content` leaves out something
    /// `Eq` compares, and every table keyed by the id would alias them.
    pub fn insert(&mut self, value: V) -> ContentId {
        let id = content_of(&value);
        self.put(id, value);
        id
    }

    /// [`ContentMap::insert`] of a value that came with an id from outside: stored only when
    /// `claimed` is the id of its content.
    pub fn insert_claimed(&mut self, claimed: ContentId, value: V) -> Result<ContentId, Mismatch> {
        let actual = content_of(&value);
        if actual != claimed {
            return Err(Mismatch { claimed, actual });
        }
        self.put(actual, value);
        Ok(actual)
    }

    fn put(&mut self, id: ContentId, value: V) {
        match self.held.entry(id) {
            Entry::Occupied(held) => assert!(
                *held.get() == value,
                "two unequal values of {} have the content id {id:?}: its `hash_content` does \
                 not feed everything its `Eq` compares",
                type_name::<V>()
            ),
            Entry::Vacant(free) => {
                free.insert(value);
            }
        }
    }
}

impl<V: fmt::Debug, A: Allocator> fmt::Debug for ContentMap<V, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Sorted by id, so that what is printed does not depend on the table's layout.
        let mut held: Vec<(&ContentId, &V)> = self.held.iter().collect(); // alloc: a debug print's order
        held.sort_by_key(|it| it.0);
        f.debug_map().entries(held).finish()
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_ids::{ContentId, Domain, Hasher, rng::SplitMix64 as Rng};

    use super::{ContentMap, Mismatch};
    use crate::{Consed, content_of};

    #[derive(Clone, PartialEq, Eq, Debug)]
    struct Blob(Vec<u8>);

    impl Consed for Blob {
        fn domain() -> &'static Domain {
            static DOMAIN: Domain = Domain::new("blaze v2 content map test blob 1");
            &DOMAIN
        }

        fn hash_content(&self, hasher: &mut Hasher) {
            hasher.part(&self.0);
        }
    }

    /// A blob whose content leaves out its second half: the planted bug of a `hash_content`
    /// that does not cover `Eq`.
    #[derive(PartialEq, Eq, Debug)]
    struct Half(u8, u8);

    impl Consed for Half {
        fn domain() -> &'static Domain {
            static DOMAIN: Domain = Domain::new("blaze v2 content map test half 1");
            &DOMAIN
        }

        fn hash_content(&self, hasher: &mut Hasher) {
            hasher.update(&[self.0]);
        }
    }

    fn blob(rng: &mut Rng) -> Blob {
        // Few distinct blobs, so that most inserts meet one that is held.
        let n = rng.below(12);
        Blob((0..n).map(|it| (it * 7 + n) as u8).collect()) // alloc: a test's value
    }

    /// The invariant: after any inserts and removes the map holds each distinct value once,
    /// under the id of its content, as a `Vec` of `(id, value)` with no id twice does.
    #[test]
    fn a_content_map_is_the_distinct_values_by_their_ids() {
        for seed in 0..64 {
            let mut rng = Rng::new(seed);
            let mut map: ContentMap<Blob, Global> = ContentMap::new_in(Global); // alloc: a test's map
            let mut model: Vec<(ContentId, Blob)> = Vec::new(); // alloc: a test's model
            for _ in 0..rng.below(120) {
                let value = blob(&mut rng);
                let id = content_of(&value);
                let at = model.iter().position(|it| it.0 == id);
                if rng.below(4) == 0 {
                    assert_eq!(map.remove(&id), at.map(|at| model.swap_remove(at).1), "seed {seed}");
                } else {
                    assert_eq!(map.insert(value.clone()), id, "seed {seed}");
                    if at.is_none() {
                        model.push((id, value));
                    }
                }
                assert_eq!(map.len(), model.len(), "seed {seed}");
                for (id, value) in &model {
                    assert_eq!(map.get(id), Some(value), "seed {seed}");
                }
                let mut held: Vec<ContentId> = map.iter().map(|it| *it.0).collect(); // alloc: a test's copy
                let mut expected: Vec<ContentId> = model.iter().map(|it| it.0).collect(); // alloc: a test's copy
                held.sort();
                expected.sort();
                assert_eq!(held, expected, "seed {seed}");
            }
        }
    }

    /// The trust rule: a value under an id that is not its content's is refused and nothing is
    /// stored; under its own id it is stored as `insert` stores it.
    #[test]
    fn an_id_from_outside_is_checked_against_the_value() {
        let mut map: ContentMap<Blob, Global> = ContentMap::new_in(Global); // alloc: a test's map
        let (good, evil) = (Blob(b"good".to_vec()), Blob(b"evil".to_vec())); // alloc: a test's values
        let id = content_of(&good);
        let refused = map.insert_claimed(id, evil.clone());
        assert_eq!(refused, Err(Mismatch { claimed: id, actual: content_of(&evil) }));
        assert!(map.is_empty() && map.get(&id).is_none(), "nothing is stored under an unchecked id");
        assert_eq!(map.insert_claimed(id, good.clone()), Ok(id));
        assert_eq!((map.get(&id), map.len()), (Some(&good), 1));
        assert_eq!(map.insert(good), id, "the same value again is the same id, stored once");
        assert_eq!(map.len(), 1);
    }

    /// The oracle of `insert`: a `hash_content` that leaves out what `Eq` compares is found
    /// when the twin goes in.
    #[test]
    #[should_panic(expected = "two unequal values")]
    fn two_unequal_values_with_one_id_stop() {
        let mut map: ContentMap<Half, Global> = ContentMap::new_in(Global); // alloc: a test's map
        map.insert(Half(1, 1));
        map.insert(Half(1, 2));
    }

    /// A map made with room allocates once, when it is made; inserts of values that own no
    /// memory, lookups and removes within the room allocate nothing.
    #[cfg(not(blaze_check))]
    #[test]
    fn a_content_map_with_room_allocates_once() {
        assert!(blaze_alloc_count::installed(), "the counting allocator is this test binary's");
        let (mut map, made) = blaze_alloc_count::measure(|| {
            ContentMap::<Half, Global>::with_capacity_in(256, Global) // alloc: measured
        });
        assert_eq!(made.allocations, 1);
        // The process's first hash sets up the BLAKE3 backend once (CPU detection, the
        // kernels' self-test, the domain's key): that is the hasher's, not the map's, so it
        // happens before the count.
        content_of(&Half(0, 0));
        let ((), used) = blaze_alloc_count::measure(|| {
            for it in 0..=255u8 {
                let id = map.insert(Half(it, 0));
                assert!(map.contains(&id));
            }
            for it in 0..64u8 {
                map.remove(&content_of(&Half(it, 0)));
            }
        });
        assert_eq!((used.allocations, map.len()), (0, 192));
    }
}
